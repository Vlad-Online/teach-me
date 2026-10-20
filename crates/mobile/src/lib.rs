//! teach-me for Android: the same trainer as the desktop, data synced with it
//! over the local network (see `teachme_core::sync`).

use anyhow::Context;
use chrono::{DateTime, Duration as ChronoDuration, Local, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};

use teachme_core::db::Stats;
use teachme_core::judge::{self, Verdict};
use teachme_core::llm::Llm;
use teachme_core::openai::OpenAi;
use teachme_core::sync::{self, Pairing, SyncReport};
use teachme_core::{Config, Db, Direction, Outcome, StudyCard};

const DIR: Direction = Direction::PtToRu;

struct AppState {
    data_dir: PathBuf,
    db: Mutex<Db>,
    /// One sync at a time.
    syncing: Mutex<()>,
    card: Mutex<Option<StudyCard>>,
}

type CmdResult<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

impl AppState {
    /// Study settings and API keys: set on the desktop, arrive with each sync.
    fn shared(&self) -> sync::SharedSettings {
        sync::shared_settings(&self.db.lock().unwrap()).unwrap_or_default()
    }

    fn new_per_day(&self) -> u32 {
        self.shared().new_per_day
    }

    fn clips_dir(&self) -> PathBuf {
        self.data_dir.join("clips")
    }

    fn pairing(&self) -> Option<Pairing> {
        let raw = self.db.lock().unwrap().meta("pairing").ok().flatten()?;
        serde_json::from_str(&raw).ok()
    }

    /// Models and keys from the desktop (last sync); before the first sync,
    /// the keys from the pairing QR code.
    fn config(&self) -> Config {
        let mut cfg = Config::default();
        let synced = sync::shared_settings(&self.db.lock().unwrap());
        if let Some(s) = synced {
            cfg.llm = s.llm;
            cfg.anthropic = s.anthropic;
            cfg.openai = s.openai;
        }
        if let Some(p) = self.pairing() {
            if cfg.anthropic.api_key.is_empty() {
                cfg.anthropic.api_key = p.anthropic_key.unwrap_or_default();
            }
            if cfg.openai.api_key.is_empty() {
                cfg.openai.api_key = p.openai_key.unwrap_or_default();
            }
        }
        if cfg.anthropic.api_key.is_empty() && !cfg.openai.api_key.is_empty() {
            cfg.llm.provider = "openai".into();
        }
        cfg
    }
}

#[derive(Serialize)]
struct Status {
    stats: Stats,
    new_per_day: u32,
    paired: bool,
    host: String,
    last_sync: Option<DateTime<Utc>>,
    has_llm: bool,
    has_stt: bool,
}

#[tauri::command]
fn status(st: State<AppState>) -> CmdResult<Status> {
    let cfg = st.config();
    let pairing = st.pairing();
    let new_per_day = st.new_per_day();
    let db = st.db.lock().unwrap();
    Ok(Status {
        stats: db.stats(DIR).map_err(err)?,
        new_per_day,
        paired: pairing.is_some(),
        host: pairing.map(|p| format!("{}:{}", p.host, p.port)).unwrap_or_default(),
        last_sync: db.meta("sync_last_at").ok().flatten().and_then(|s| s.parse().ok()),
        has_llm: !cfg.anthropic.api_key.is_empty() || !cfg.openai.api_key.is_empty(),
        has_stt: !cfg.openai.api_key.is_empty(),
    })
}

/// Contents of the QR code shown by the desktop ("Подключить телефон…").
#[tauri::command]
fn pair(st: State<AppState>, payload: String) -> CmdResult<()> {
    let p: Pairing = serde_json::from_str(payload.trim()).map_err(|_| "Это не QR-код teach-me".to_string())?;
    if p.key.len() < 16 {
        return Err("В QR-коде нет ключа сопряжения".into());
    }
    st.db.lock().unwrap().set_meta("pairing", &serde_json::to_string(&p).map_err(err)?).map_err(err)?;
    Ok(())
}

// ---------- reminders ----------

/// Reminder rhythm (interval, active hours) comes from the desktop; only
/// on/off is a setting of the phone itself.
#[derive(Serialize, Deserialize, Clone)]
struct Reminders {
    enabled: bool,
    #[serde(default)]
    interval_minutes: u32,
    #[serde(default)]
    from_hour: u32,
    #[serde(default)]
    to_hour: u32,
}

impl Default for Reminders {
    fn default() -> Self {
        Self { enabled: true, interval_minutes: 0, from_hour: 0, to_hour: 0 }
    }
}

/// Notification ids owned by the reminder schedule.
const REMINDER_IDS: std::ops::Range<i32> = 1000..1064;
/// How far ahead reminders are planned (re-planned on every app start and card).
const PLAN_HOURS: i64 = 48;

#[derive(Serialize)]
struct RemindersDto {
    #[serde(flatten)]
    settings: Reminders,
    /// Local time of the next reminder, e.g. "сегодня 14:30".
    next: Option<String>,
    scheduled: usize,
}

impl AppState {
    fn reminders(&self) -> Reminders {
        let enabled = self
            .db
            .lock()
            .unwrap()
            .meta("reminders")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Reminders>(&s).ok())
            .map_or(true, |r| r.enabled);
        let s = self.shared();
        Reminders {
            enabled,
            interval_minutes: s.interval_minutes,
            from_hour: s.active_from_hour,
            to_hour: s.active_to_hour,
        }
    }
}

/// One reminder: when, and what is waiting at that moment.
#[cfg_attr(not(mobile), allow(dead_code))]
struct Slot {
    at: DateTime<Local>,
    due: i64,
    new: i64,
}

/// Times within active hours, every `interval`, where something is to study.
fn plan_slots(st: &AppState, r: &Reminders) -> anyhow::Result<Vec<Slot>> {
    let per_day = st.new_per_day() as i64;
    let db = st.db.lock().unwrap();
    let unseen = db.stats(DIR)?.unseen;
    let new_today = db.new_today(DIR)?;
    let now = Local::now();
    let step = ChronoDuration::minutes(r.interval_minutes.max(5) as i64);
    let mut slots = vec![];
    let mut t = now + step;
    while t < now + ChronoDuration::hours(PLAN_HOURS) && slots.len() < REMINDER_IDS.len() {
        if t.hour() >= r.from_hour && t.hour() < r.to_hour {
            let due: i64 = db.conn.query_row(
                "SELECT COUNT(*) FROM reviews WHERE direction = ?1 AND due <= ?2",
                rusqlite_params(DIR, t.with_timezone(&Utc)),
                |row| row.get(0),
            )?;
            // The daily limit of new phrases restarts tomorrow.
            let left_today = if t.date_naive() == now.date_naive() { per_day - new_today } else { per_day };
            let new = unseen.min(left_today.max(0));
            if due + new > 0 {
                slots.push(Slot { at: t, due, new });
            }
        }
        t += step;
    }
    Ok(slots)
}

fn rusqlite_params(dir: Direction, t: DateTime<Utc>) -> (i64, DateTime<Utc>) {
    (dir as i64, t)
}

fn human_when(t: DateTime<Local>) -> String {
    let today = Local::now().date_naive();
    let day = if t.date_naive() == today {
        "сегодня"
    } else if t.date_naive() == today.succ_opt().unwrap_or(today) {
        "завтра"
    } else {
        "послезавтра"
    };
    format!("{day} {}", t.format("%H:%M"))
}

/// Replaces the scheduled reminders with a fresh plan. Android keeps the
/// alarms, so they fire even when the app is closed.
fn reschedule(app: &AppHandle) -> anyhow::Result<RemindersDto> {
    let st = app.state::<AppState>();
    let settings = st.reminders();
    let slots = if settings.enabled { plan_slots(&st, &settings)? } else { vec![] };
    #[cfg(mobile)]
    {
        use tauri_plugin_notification::{NotificationExt, Schedule};
        let n = app.notification();
        let _ = n.cancel(REMINDER_IDS.collect());
        for (id, s) in REMINDER_IDS.zip(&slots) {
            let body = match (s.due, s.new) {
                (0, n) => format!("Новых фраз: {n} — пара минут на слух"),
                (d, 0) => format!("К повтору: {d} — пара минут на слух"),
                (d, n) => format!("К повтору: {d} · новых: {n}"),
            };
            let date = time::OffsetDateTime::from_unix_timestamp(s.at.timestamp())?;
            n.builder()
                .id(id)
                .title("teach-me")
                .body(body)
                .auto_cancel()
                .extra("study", true)
                .schedule(Schedule::At { date, repeating: false, allow_while_idle: true })
                .show()?;
        }
    }
    Ok(RemindersDto {
        next: slots.first().map(|s| human_when(s.at)),
        scheduled: slots.len(),
        settings,
    })
}

fn reschedule_in_background(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = reschedule(&app) {
            eprintln!("reminders: {e}");
        }
    });
}

/// A reminder in 10 seconds: checks that notifications reach the user
/// (battery savers on some phones block them for closed apps).
#[tauri::command]
fn test_reminder(app: AppHandle) -> CmdResult<()> {
    #[cfg(mobile)]
    {
        use tauri_plugin_notification::{NotificationExt, Schedule};
        let date = time::OffsetDateTime::now_utc() + time::Duration::seconds(10);
        app.notification()
            .builder()
            .id(1099)
            .title("teach-me")
            .body("Проверка: напоминания работают. Нажмите, чтобы открыть карточку.")
            .auto_cancel()
            .extra("study", true)
            .schedule(Schedule::At { date, repeating: false, allow_while_idle: true })
            .show()
            .map_err(err)?;
    }
    #[cfg(not(mobile))]
    let _ = app;
    Ok(())
}

#[tauri::command]
async fn get_reminders(app: AppHandle) -> CmdResult<RemindersDto> {
    tauri::async_runtime::spawn_blocking(move || reschedule(&app).map_err(err))
        .await
        .map_err(err)?
}

#[tauri::command]
async fn set_reminders(app: AppHandle, enabled: bool) -> CmdResult<RemindersDto> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let r = Reminders { enabled, ..Default::default() };
        st.db
            .lock()
            .unwrap()
            .set_meta("reminders", &serde_json::to_string(&r).map_err(err)?)
            .map_err(err)?;
        reschedule(&app).map_err(err)
    })
    .await
    .map_err(err)?
}

fn do_sync(app: &AppHandle) -> anyhow::Result<SyncReport> {
    let st = app.state::<AppState>();
    let _one = st.syncing.lock().unwrap();
    let pairing = st.pairing().context("Телефон не подключён к ПК: отсканируйте QR-код")?;
    // The address from the QR code first, then broadcast discovery
    // (the desktop's address may have changed since pairing).
    let known: Vec<IpAddr> = pairing.host.parse().ok().into_iter().collect();
    let device = st.db.lock().unwrap().device_id.clone();
    let addr = sync::discover(&device, sync::UDP_PORT, &known, Duration::from_secs(4))?
        .or_else(|| known.first().map(|ip| SocketAddr::new(*ip, pairing.port)))
        .context("ПК не найден в сети")?;
    let report = {
        let db = st.db.lock().unwrap();
        sync::sync_with(addr, &pairing.key, &db, &st.clips_dir())?
    };
    // New phrases and answers from the desktop change what is due.
    reschedule_in_background(app);
    Ok(report)
}

#[tauri::command]
async fn sync_now(app: AppHandle) -> CmdResult<SyncReport> {
    tauri::async_runtime::spawn_blocking(move || do_sync(&app).map_err(err))
        .await
        .map_err(err)?
}

#[derive(Serialize)]
struct CardDto {
    #[serde(flatten)]
    card: StudyCard,
    clip_path: String,
}

#[tauri::command]
fn next_card(st: State<AppState>) -> CmdResult<Option<CardDto>> {
    let new_per_day = st.new_per_day();
    let db = st.db.lock().unwrap();
    let Some(card) = db.next_card(DIR, Utc::now(), new_per_day).map_err(err)? else {
        return Ok(None);
    };
    db.mark_clip_shown(card.clip_id).map_err(err)?;
    let clip_path = st.clips_dir().join(&card.clip_file).to_string_lossy().into_owned();
    *st.card.lock().unwrap() = Some(card.clone());
    Ok(Some(CardDto { card, clip_path }))
}

#[tauri::command]
async fn check_answer(app: AppHandle, answer: String, attempt: i64) -> CmdResult<Verdict> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let card = st.card.lock().unwrap().clone().ok_or("нет карточки")?;
        let llm = Llm::from_config(&st.config()).ok();
        let v = judge::check(llm.as_ref(), &card, DIR, &answer).map_err(err)?;
        let _ = st
            .db
            .lock()
            .unwrap()
            .log_attempt(card.phrase_id, DIR, attempt, &answer, &v.verdict, &v.feedback, 0);
        Ok(v)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn finish(app: AppHandle, st: State<AppState>, outcome: Outcome, answer: String) -> CmdResult<String> {
    let phrase_id = st.card.lock().unwrap().as_ref().map(|c| c.phrase_id).ok_or("нет карточки")?;
    let due = st
        .db
        .lock()
        .unwrap()
        .record_outcome(phrase_id, DIR, outcome, &answer, Utc::now())
        .map_err(err)?;
    // Push the answer to the desktop right away if it is around.
    reschedule_in_background(&app);
    let app = app.clone();
    std::thread::spawn(move || {
        let _ = do_sync(&app);
    });
    Ok(human_due(due))
}

/// Speech-to-text for a recording made in the WebView (MediaRecorder).
#[tauri::command]
async fn transcribe(app: AppHandle, audio_b64: String, mime: String) -> CmdResult<String> {
    tauri::async_runtime::spawn_blocking(move || {
        use base64::Engine;
        let st = app.state::<AppState>();
        let audio = base64::engine::general_purpose::STANDARD.decode(audio_b64).map_err(err)?;
        let api = OpenAi::new(&st.config().openai).map_err(|_| "Нет ключа OpenAI: подключите телефон к ПК".to_string())?;
        let ext = if mime.contains("mp4") { "m4a" } else if mime.contains("ogg") { "ogg" } else { "webm" };
        api.transcribe(audio, &format!("answer.{ext}"), "ru", "Перевод короткой фразы на русский язык.")
            .map_err(err)
    })
    .await
    .map_err(err)?
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(mobile)]
    let builder = builder
        .plugin(tauri_plugin_barcode_scanner::init())
        .plugin(tauri_plugin_notification::init());
    builder
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(data_dir.join("clips"))?;
            let db = Db::open(&data_dir.join("teach-me.db"))?;
            app.manage(AppState {
                data_dir,
                db: Mutex::new(db),
                syncing: Mutex::new(()),
                card: Mutex::new(None),
            });
            reschedule_in_background(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            status,
            pair,
            sync_now,
            next_card,
            check_answer,
            finish,
            transcribe,
            get_reminders,
            set_reminders,
            test_reminder
        ])
        .run(tauri::generate_context!())
        .expect("error while running teach-me");
}
