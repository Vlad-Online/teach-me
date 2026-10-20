#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod harvest;
mod presence;
mod recorder;

use chrono::{DateTime, Duration, Local, Timelike, Utc};
use serde::Serialize;
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use recorder::Recorder;
use teachme_core::db::Stats;
use teachme_core::judge::{self, Verdict};
use teachme_core::llm::Llm;
use teachme_core::openai::OpenAi;
use tauri_plugin_autostart::ManagerExt;
use teachme_core::{Config, Db, Direction, Home, Outcome, StudyCard};

const QUIZ: &str = "quiz";
const PAIR: &str = "pair";
const SETTINGS: &str = "settings";
const TOAST_SIZE: (f64, f64) = (380.0, 128.0);
const FULL_SIZE: (f64, f64) = (460.0, 640.0);
pub(crate) const DIR: Direction = Direction::PtToRu;

struct Sched {
    next_popup: DateTime<Utc>,
    paused_until: Option<DateTime<Utc>>,
}

struct Session {
    card: StudyCard,
    started: bool,
    /// Opened by the user (hotkey/tray): skip the toast.
    autostart: bool,
}

pub(crate) struct AppState {
    pub(crate) home: Home,
    cfg: Config,
    pub(crate) db: Mutex<Db>,
    /// Speech-to-text.
    api: Option<OpenAi>,
    /// Answer checking.
    llm: Option<Llm>,
    sched: Mutex<Sched>,
    session: Mutex<Option<Session>>,
    recorder: Recorder,
    recording: Mutex<bool>,
    ptt: Option<Shortcut>,
    card_now: Option<Shortcut>,
}

#[derive(Serialize, Clone)]
struct CardDto {
    #[serde(flatten)]
    card: StudyCard,
    clip_path: String,
    started: bool,
    autostart: bool,
    has_api: bool,
    hotkey: String,
}

#[derive(Serialize, Clone)]
struct RecEvent {
    state: &'static str,
    text: String,
}

pub(crate) type CmdResult<T> = Result<T, String>;

pub(crate) fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

impl AppState {
    fn dto(&self, s: &Session) -> CardDto {
        CardDto {
            card: s.card.clone(),
            clip_path: self.home.clips_dir().join(&s.card.clip_file).to_string_lossy().into_owned(),
            started: s.started,
            autostart: s.autostart,
            has_api: self.api.is_some(),
            hotkey: self.cfg.trainer.hotkey.clone(),
        }
    }

    fn set_next_popup(&self, at: DateTime<Utc>) {
        self.sched.lock().unwrap().next_popup = at;
    }

    /// Regular interval, or sooner if a failed phrase is due to come back.
    fn plan_next_popup(&self) {
        let now = Utc::now();
        let t = &self.cfg.trainer;
        let regular = now + Duration::minutes(t.interval_minutes as i64);
        let earliest = self.db.lock().unwrap().earliest_due(DIR).ok().flatten();
        let next = match earliest {
            Some(d) if d < regular => d.max(now + Duration::minutes(t.relearn_minutes as i64)),
            _ => regular,
        };
        self.set_next_popup(next);
    }
}

// ---------- window ----------

fn place(win: &tauri::WebviewWindow, (w, h): (f64, f64)) {
    let _ = win.set_size(LogicalSize::new(w, h));
    let monitor = win.primary_monitor().ok().flatten().or_else(|| win.current_monitor().ok().flatten());
    if let Some(m) = monitor {
        let scale = m.scale_factor();
        let area = m.work_area();
        let margin = (12.0 * scale) as i32;
        let x = area.position.x + area.size.width as i32 - (w * scale) as i32 - margin;
        let y = area.position.y + area.size.height as i32 - (h * scale) as i32 - margin;
        let _ = win.set_position(PhysicalPosition::new(x, y));
    }
}

/// Opens the popup with the next due card. Returns false if nothing to study.
fn open_quiz(app: &AppHandle, start_now: bool) -> bool {
    let st = app.state::<AppState>();
    if let Some(w) = app.get_webview_window(QUIZ) {
        let _ = w.show();
        let _ = w.set_focus();
        return true;
    }
    let card = match st.db.lock().unwrap().next_card(DIR, Utc::now(), st.cfg.trainer.new_per_day) {
        Ok(Some(c)) => c,
        Ok(None) => return false,
        Err(e) => {
            eprintln!("next_card: {e}");
            return false;
        }
    };
    *st.session.lock().unwrap() = Some(Session { card, started: false, autostart: start_now });
    let built = WebviewWindowBuilder::new(app, QUIZ, WebviewUrl::App("index.html".into()))
        .title("teach-me")
        .inner_size(TOAST_SIZE.0, TOAST_SIZE.1)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(start_now)
        .visible(false)
        .additional_browser_args(
            "--autoplay-policy=no-user-gesture-required --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection",
        )
        .build();
    match built {
        Ok(win) => {
            place(&win, TOAST_SIZE);
            let _ = win.show();
            true
        }
        Err(e) => {
            eprintln!("window: {e}");
            *st.session.lock().unwrap() = None;
            false
        }
    }
}

fn spawn_open_quiz(app: &AppHandle, start_now: bool) {
    let app = app.clone();
    std::thread::spawn(move || {
        if !open_quiz(&app, start_now) {
            eprintln!("нет карточек для показа");
        }
    });
}

fn on_quiz_closed(app: &AppHandle) {
    let st = app.state::<AppState>();
    if let Some(ptt) = st.ptt {
        let _ = app.global_shortcut().unregister(ptt);
    }
    let mut rec = st.recording.lock().unwrap();
    if *rec {
        let _ = st.recorder.stop();
        *rec = false;
    }
    *st.session.lock().unwrap() = None;
}

// ---------- recording ----------

fn start_recording(app: &AppHandle) {
    let st = app.state::<AppState>();
    let mut rec = st.recording.lock().unwrap();
    if *rec {
        return;
    }
    match st.recorder.start() {
        Ok(()) => {
            *rec = true;
            let _ = app.emit_to(QUIZ, "rec", RecEvent { state: "recording", text: String::new() });
        }
        Err(e) => {
            let _ = app.emit_to(QUIZ, "rec", RecEvent { state: "error", text: format!("Микрофон: {e}") });
        }
    }
}

fn stop_recording(app: &AppHandle) {
    let st = app.state::<AppState>();
    {
        let mut rec = st.recording.lock().unwrap();
        if !*rec {
            return;
        }
        *rec = false;
    }
    let (wav, ms) = match st.recorder.stop() {
        Ok(r) => r,
        Err(e) => {
            let _ = app.emit_to(QUIZ, "rec", RecEvent { state: "error", text: e.to_string() });
            return;
        }
    };
    if ms < 400 {
        let _ = app.emit_to(QUIZ, "rec", RecEvent { state: "idle", text: String::new() });
        return;
    }
    let _ = app.emit_to(QUIZ, "rec", RecEvent { state: "transcribing", text: String::new() });
    let app = app.clone();
    std::thread::spawn(move || {
        let st = app.state::<AppState>();
        let ev = match &st.api {
            None => RecEvent { state: "error", text: "Нет ключа OpenAI в config.toml — введите ответ текстом".into() },
            Some(api) => match api.transcribe(wav, "answer.wav", "ru", "Перевод короткой фразы на русский язык.") {
                Ok(text) => RecEvent { state: "done", text },
                Err(e) => RecEvent { state: "error", text: format!("Распознавание: {e}") },
            },
        };
        let _ = app.emit_to(QUIZ, "rec", ev);
    });
}

// ---------- commands ----------

#[tauri::command]
fn get_card(st: State<AppState>) -> Option<CardDto> {
    st.session.lock().unwrap().as_ref().map(|s| st.dto(s))
}

#[tauri::command]
fn begin(app: AppHandle, st: State<AppState>) -> CmdResult<()> {
    let clip_id = {
        let mut s = st.session.lock().unwrap();
        let Some(s) = s.as_mut() else { return Err("нет карточки".into()) };
        if s.started {
            return Ok(());
        }
        s.started = true;
        s.card.clip_id
    };
    st.db.lock().unwrap().mark_clip_shown(clip_id).map_err(err)?;
    if let Some(win) = app.get_webview_window(QUIZ) {
        place(&win, FULL_SIZE);
        let _ = win.set_focus();
    }
    if let Some(ptt) = st.ptt {
        if let Err(e) = app.global_shortcut().register(ptt) {
            eprintln!("hotkey {}: {e}", st.cfg.trainer.hotkey);
        }
    }
    Ok(())
}

#[tauri::command]
async fn check_answer(app: AppHandle, answer: String, attempt: i64, latency_ms: i64) -> CmdResult<Verdict> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let card = st.session.lock().unwrap().as_ref().map(|s| s.card.clone()).ok_or("нет карточки")?;
        let v = judge::check(st.llm.as_ref(), &card, DIR, &answer).map_err(err)?;
        let _ = st
            .db
            .lock()
            .unwrap()
            .log_attempt(card.phrase_id, DIR, attempt, &answer, &v.verdict, &v.feedback, latency_ms);
        Ok(v)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn finish(st: State<AppState>, outcome: Outcome, answer: String) -> CmdResult<String> {
    let phrase_id = st.session.lock().unwrap().as_ref().map(|s| s.card.phrase_id).ok_or("нет карточки")?;
    let due = st.db.lock().unwrap().record_outcome(phrase_id, DIR, outcome, &answer, Utc::now()).map_err(err)?;
    st.plan_next_popup();
    Ok(human_due(due))
}

#[tauri::command]
fn next_card(st: State<AppState>) -> CmdResult<Option<CardDto>> {
    let card = st
        .db
        .lock()
        .unwrap()
        .next_card(DIR, Utc::now(), st.cfg.trainer.new_per_day)
        .map_err(err)?;
    let Some(card) = card else { return Ok(None) };
    st.db.lock().unwrap().mark_clip_shown(card.clip_id).map_err(err)?;
    let s = Session { card, started: true, autostart: false };
    let dto = st.dto(&s);
    *st.session.lock().unwrap() = Some(s);
    Ok(Some(dto))
}

#[tauri::command]
fn snooze(app: AppHandle, st: State<AppState>, minutes: i64) {
    st.set_next_popup(Utc::now() + Duration::minutes(minutes));
    if let Some(w) = app.get_webview_window(QUIZ) {
        let _ = w.close();
    }
}

#[tauri::command]
fn close_quiz(app: AppHandle) {
    if let Some(w) = app.get_webview_window(QUIZ) {
        let _ = w.close();
    }
}

#[tauri::command]
fn toggle_record(app: AppHandle) {
    let recording = *app.state::<AppState>().recording.lock().unwrap();
    if recording {
        stop_recording(&app);
    } else {
        start_recording(&app);
    }
}

/// Diagnostics from the page (video errors etc.) go to stderr.
#[tauri::command]
fn ui_log(msg: String) {
    eprintln!("[ui] {msg}");
}

#[derive(Serialize)]
struct PairingDto {
    svg: String,
    host: String,
    port: u16,
    enabled: bool,
}

/// QR code with everything the phone needs: address, pairing key, API keys.
#[tauri::command]
fn get_pairing(st: State<AppState>) -> CmdResult<PairingDto> {
    let key = teachme_core::sync::pairing_key(&st.db.lock().unwrap()).map_err(err)?;
    let host = teachme_core::sync::local_ip().map(|ip| ip.to_string()).unwrap_or_default();
    let nonempty = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    let pairing = teachme_core::sync::Pairing {
        host: host.clone(),
        port: st.cfg.sync.http_port,
        key,
        anthropic_key: nonempty(&st.cfg.anthropic.api_key),
        openai_key: nonempty(&st.cfg.openai.api_key),
    };
    let payload = serde_json::to_string(&pairing).map_err(err)?;
    let svg = qrcode::QrCode::new(payload.as_bytes())
        .map_err(err)?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(320, 320)
        .quiet_zone(true)
        .build();
    Ok(PairingDto { svg, host, port: st.cfg.sync.http_port, enabled: st.cfg.sync.enabled })
}

fn open_pairing(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(PAIR) {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, PAIR, WebviewUrl::App("pair.html".into()))
        .title("teach-me — подключить телефон")
        .inner_size(420.0, 600.0)
        .resizable(false)
        .center()
        .build();
}

// ---------- settings ----------

pub(crate) fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(SETTINGS) {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("settings.html".into()))
        .title("teach-me — настройки")
        .inner_size(620.0, 780.0)
        .min_inner_size(480.0, 500.0)
        .center()
        .build();
}

#[derive(Serialize)]
struct SettingsDto {
    config: Config,
    autostart: bool,
    config_path: String,
    macos: bool,
}

#[tauri::command]
fn get_settings(app: AppHandle, st: State<AppState>) -> CmdResult<SettingsDto> {
    Ok(SettingsDto {
        // From the file: it may have been edited by hand since start.
        config: st.home.load_config().map_err(err)?,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        config_path: st.home.config_path().to_string_lossy().into_owned(),
        macos: cfg!(target_os = "macos"),
    })
}

/// Validates, writes config.toml and restarts the app to apply everything
/// (hotkeys, API clients, sync server).
#[tauri::command]
fn save_settings(app: AppHandle, st: State<AppState>, config: Config, autostart: bool) -> CmdResult<()> {
    let t = &config.trainer;
    for (what, key) in [("для голоса", &t.hotkey), ("«карточка сейчас»", &t.card_now_hotkey)] {
        key.parse::<Shortcut>()
            .map_err(|_| format!("Горячая клавиша {what} «{key}» не распознана"))?;
    }
    if t.hotkey == t.card_now_hotkey {
        return Err("Горячие клавиши для голоса и для карточки совпадают".into());
    }
    if t.active_from_hour >= t.active_to_hour || t.active_to_hour > 24 {
        return Err("Активные часы: «с» должно быть меньше «до» (0–24)".into());
    }
    if t.interval_minutes == 0 || t.new_per_day == 0 {
        return Err("Интервал и число новых фраз должны быть больше нуля".into());
    }
    teachme_core::config::save_config(&st.home.config_path(), &config).map_err(err)?;
    let al = app.autolaunch();
    let _ = if autostart { al.enable() } else { al.disable() };
    app.restart();
}

#[tauri::command]
async fn list_models(st: State<'_, AppState>, provider: String, key: String) -> CmdResult<Vec<String>> {
    let base_url = st.cfg.openai.base_url.clone();
    tauri::async_runtime::spawn_blocking(move || match provider.as_str() {
        "anthropic" => teachme_core::anthropic::Claude::list_models(&key),
        _ => OpenAi::list_models(&base_url, &key),
    })
    .await
    .map_err(err)?
    .map_err(err)
}

/// Opens a web page (only https) or one of the app's files in the system app.
#[tauri::command]
fn open_external(st: State<AppState>, target: String) -> CmdResult<()> {
    let path = match target.as_str() {
        "sources" => st.home.sources_path().to_string_lossy().into_owned(),
        "folder" => st.home.root.to_string_lossy().into_owned(),
        url if url.starts_with("https://") => url.to_string(),
        _ => return Err("unsupported target".into()),
    };
    let mut cmd = if cfg!(windows) {
        if target == "sources" {
            std::process::Command::new("notepad")
        } else {
            let mut c = std::process::Command::new("rundll32");
            c.arg("url.dll,FileProtocolHandler");
            c
        }
    } else if target == "sources" {
        let mut c = std::process::Command::new("open");
        c.arg("-t");
        c
    } else {
        std::process::Command::new("open")
    };
    cmd.arg(path).spawn().map_err(err)?;
    Ok(())
}

#[tauri::command]
fn show_pairing(app: AppHandle) {
    std::thread::spawn(move || open_pairing(&app));
}

#[tauri::command]
fn stats(st: State<AppState>) -> CmdResult<Stats> {
    st.db.lock().unwrap().stats(DIR).map_err(err)
}

fn human_due(due: DateTime<Utc>) -> String {
    let mins = (due - Utc::now()).num_minutes();
    if mins < 60 {
        format!("через {} мин", mins.max(1))
    } else if mins < 24 * 60 {
        format!("через {} ч", (mins + 30) / 60)
    } else {
        format!("через {} дн", (mins + 12 * 60) / (24 * 60))
    }
}

// ---------- background ----------

fn scheduler(app: AppHandle) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(20));
        let st = app.state::<AppState>();
        update_tray_tooltip(&app);
        if app.get_webview_window(QUIZ).is_some() {
            continue;
        }
        let now = Utc::now();
        {
            let s = st.sched.lock().unwrap();
            if s.paused_until.is_some_and(|p| p > now) || now < s.next_popup {
                continue;
            }
        }
        let t = &st.cfg.trainer;
        let hour = Local::now().hour();
        if hour < t.active_from_hour || hour >= t.active_to_hour {
            continue;
        }
        if presence::idle_secs() > t.idle_minutes as u64 * 60 || presence::busy() {
            continue;
        }
        st.set_next_popup(now + Duration::minutes(t.interval_minutes as i64));
        open_quiz(&app, false);
    }
}

pub(crate) fn update_tray_tooltip(app: &AppHandle) {
    let st = app.state::<AppState>();
    let Ok(s) = st.db.lock().unwrap().stats(DIR) else { return };
    let paused = st.sched.lock().unwrap().paused_until.is_some_and(|p| p > Utc::now());
    let last_sync = st
        .db
        .lock()
        .unwrap()
        .meta("sync_last_client")
        .ok()
        .flatten()
        .and_then(|v| v.split_once(' ').and_then(|(_, ts)| ts.parse::<DateTime<Utc>>().ok()))
        .map(|t| format!("\nТелефон: синхр. {}", t.with_timezone(&Local).format("%d.%m %H:%M")))
        .unwrap_or_default();
    let harvesting = app
        .state::<harvest::Harvest>()
        .tray_line()
        .map(|l| format!("\n{l}"))
        .unwrap_or_default();
    let text = format!(
        "teach-me{}\nФраз: {} · новых: {}\nК повтору: {} · трудных: {}\nСегодня: {}{}{}",
        if paused { " (пауза)" } else { "" },
        s.phrases,
        s.unseen,
        s.due_now,
        s.difficult,
        s.reviewed_today,
        last_sync,
        harvesting
    );
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(text));
    }
}

fn main() {
    let home = Home::locate().expect("config.toml not found");
    let cfg = home.load_config().expect("bad config.toml");
    let db = Db::open(&home.db_path()).expect("cannot open database");
    let api = OpenAi::new(&cfg.openai).ok();
    let llm = match Llm::from_config(&cfg) {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("LLM недоступна, проверка будет приблизительной: {e}");
            None
        }
    };
    let ptt: Option<Shortcut> = cfg.trainer.hotkey.parse().ok();
    let card_now: Option<Shortcut> = cfg.trainer.card_now_hotkey.parse().ok();

    let state = AppState {
        home,
        db: Mutex::new(db),
        api,
        llm,
        sched: Mutex::new(Sched {
            next_popup: Utc::now() + Duration::minutes(2),
            paused_until: None,
        }),
        session: Mutex::new(None),
        recorder: Recorder::spawn(),
        recording: Mutex::new(false),
        ptt,
        card_now,
        cfg,
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    let st = app.state::<AppState>();
                    if Some(*shortcut) == st.card_now {
                        if event.state() == ShortcutState::Pressed {
                            spawn_open_quiz(app, true);
                        }
                    } else if Some(*shortcut) == st.ptt {
                        let started = st.session.lock().unwrap().as_ref().is_some_and(|s| s.started);
                        match (event.state(), started) {
                            (ShortcutState::Pressed, false) => {
                                let _ = app.emit_to(QUIZ, "ui-start", ());
                            }
                            (ShortcutState::Pressed, true) => start_recording(app),
                            (ShortcutState::Released, true) => stop_recording(app),
                            _ => {}
                        }
                    }
                })
                .build(),
        )
        .manage(state)
        .manage(harvest::Harvest::default())
        .invoke_handler(tauri::generate_handler![
            get_card,
            begin,
            check_answer,
            finish,
            next_card,
            snooze,
            close_quiz,
            toggle_record,
            stats,
            ui_log,
            get_pairing,
            get_settings,
            save_settings,
            list_models,
            open_external,
            show_pairing,
            harvest::harvest_overview,
            harvest::harvest_start,
            harvest::harvest_stop,
            harvest::tools_install,
            harvest::sources_save,
            harvest::source_probe,
            harvest::show_settings
        ])
        .setup(|app| {
            // Menu bar only, no Dock icon.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let handle = app.handle().clone();
            let st = app.state::<AppState>();
            if let Some(s) = st.card_now {
                if let Err(e) = app.global_shortcut().register(s) {
                    eprintln!("hotkey {}: {e}", st.cfg.trainer.card_now_hotkey);
                }
            }

            let now_i = MenuItem::with_id(app, "now", "Карточка сейчас", true, None::<&str>)?;
            let pause_i = MenuItem::with_id(app, "pause", "Пауза на час", true, None::<&str>)?;
            let resume_i = MenuItem::with_id(app, "resume", "Продолжить", true, None::<&str>)?;
            let pair_i = MenuItem::with_id(app, "pair", "Подключить телефон…", true, None::<&str>)?;
            let settings_i = MenuItem::with_id(app, "settings", "Настройки…", true, None::<&str>)?;
            let harvest_i = MenuItem::with_id(app, "harvest", "База фраз…", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let sep2 = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&now_i, &pause_i, &resume_i, &sep, &harvest_i, &settings_i, &pair_i, &sep2, &quit_i])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("teach-me")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| {
                    let st = app.state::<AppState>();
                    match e.id().as_ref() {
                        "now" => spawn_open_quiz(app, true),
                        "pause" => st.sched.lock().unwrap().paused_until = Some(Utc::now() + Duration::hours(1)),
                        "resume" => {
                            let mut s = st.sched.lock().unwrap();
                            s.paused_until = None;
                            s.next_popup = Utc::now();
                        }
                        "pair" => {
                            let app = app.clone();
                            std::thread::spawn(move || open_pairing(&app));
                        }
                        "harvest" => {
                            let app = app.clone();
                            std::thread::spawn(move || harvest::open_window(&app));
                        }
                        "settings" => {
                            let app = app.clone();
                            std::thread::spawn(move || open_settings(&app));
                        }
                        "quit" => app.exit(0),
                        _ => {}
                    }
                    update_tray_tooltip(app);
                })
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        spawn_open_quiz(tray.app_handle(), true);
                    }
                })
                .build(app)?;
            if st.cfg.sync.enabled {
                let server = teachme_core::sync::ServerConfig {
                    db_path: st.home.db_path(),
                    clips_dir: st.home.clips_dir(),
                    udp_port: st.cfg.sync.udp_port,
                    http_port: st.cfg.sync.http_port,
                    settings: teachme_core::sync::SharedSettings::from_config(&st.cfg),
                };
                if let Err(e) = teachme_core::sync::start_server(server) {
                    eprintln!("синхронизация недоступна: {e}");
                }
            }
            update_tray_tooltip(&handle);
            // First run: no keys yet — show the settings right away.
            if st.cfg.anthropic.api_key.trim().is_empty() && st.cfg.openai.api_key.trim().is_empty() {
                let h = handle.clone();
                std::thread::spawn(move || open_settings(&h));
            } else if st.db.lock().unwrap().stats(DIR).map(|s| s.phrases == 0).unwrap_or(false) {
                // Keys are there but the base is empty: build it first.
                let h = handle.clone();
                std::thread::spawn(move || harvest::open_window(&h));
            }
            if std::env::args().any(|a| a == "--now") {
                spawn_open_quiz(&handle, true);
            }
            std::thread::spawn(move || scheduler(handle));
            Ok(())
        })
        .on_window_event(|win, e| {
            if win.label() == QUIZ && matches!(e, tauri::WindowEvent::Destroyed) {
                on_quiz_closed(win.app_handle());
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_app, event| {
        // Keep running in the tray when the popup closes.
        if let tauri::RunEvent::ExitRequested { code: None, api, .. } = event {
            api.prevent_exit();
        }
    });
}
