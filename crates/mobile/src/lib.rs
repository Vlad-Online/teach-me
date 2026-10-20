//! teach-me for Android: the same trainer as the desktop, data synced with it
//! over the local network (see `teachme_core::sync`).

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Serialize;
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
const NEW_PER_DAY: u32 = 10;

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
    fn clips_dir(&self) -> PathBuf {
        self.data_dir.join("clips")
    }

    fn pairing(&self) -> Option<Pairing> {
        let raw = self.db.lock().unwrap().meta("pairing").ok().flatten()?;
        serde_json::from_str(&raw).ok()
    }

    /// API keys come with the pairing QR code.
    fn config(&self) -> Config {
        let mut cfg = Config::default();
        if let Some(p) = self.pairing() {
            cfg.anthropic.api_key = p.anthropic_key.unwrap_or_default();
            cfg.openai.api_key = p.openai_key.unwrap_or_default();
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
    let db = st.db.lock().unwrap();
    Ok(Status {
        stats: db.stats(DIR).map_err(err)?,
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
    let db = st.db.lock().unwrap();
    sync::sync_with(addr, &pairing.key, &db, &st.clips_dir())
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
    let db = st.db.lock().unwrap();
    let Some(card) = db.next_card(DIR, Utc::now(), NEW_PER_DAY).map_err(err)? else {
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
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());
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
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![status, pair, sync_now, next_card, check_answer, finish, transcribe])
        .run(tauri::generate_context!())
        .expect("error while running teach-me");
}
