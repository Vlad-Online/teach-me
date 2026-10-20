//! «База фраз» window: building the phrase base from the app instead of the
//! command line. The harvest runs on a background thread; the window (if open)
//! follows it through "harvest" events, the tray tooltip shows the progress.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

use harvester::sources::SourceCfg;
use harvester::tools::{self, ToolStatus};
use harvester::{Event, Report};
use teachme_core::db::Stats;

use crate::{err, AppState, CmdResult, DIR};

pub const HARVEST: &str = "harvest";
const LOG_LINES: usize = 300;

#[derive(Serialize, Clone, Default)]
pub struct HarvestState {
    pub running: bool,
    /// Installing a tool right now (its id).
    pub installing: Option<String>,
    pub stage: String,
    pub done: usize,
    pub total: usize,
    /// Phrases added in the current/last run: (pt, ru).
    pub added: Vec<(String, String)>,
    pub log: Vec<String>,
    /// Outcome of the last run, e.g. "Добавлено фраз: 30".
    pub result: Option<String>,
    pub failed: bool,
}

#[derive(Default)]
pub struct Harvest {
    pub state: Mutex<HarvestState>,
    cancel: Arc<AtomicBool>,
}

impl Harvest {
    fn update(&self, app: &AppHandle, f: impl FnOnce(&mut HarvestState)) {
        let snapshot = {
            let mut s = self.state.lock().unwrap();
            f(&mut s);
            if s.log.len() > LOG_LINES {
                let cut = s.log.len() - LOG_LINES;
                s.log.drain(..cut);
            }
            s.clone()
        };
        let _ = app.emit_to(HARVEST, "harvest", &snapshot);
        crate::update_tray_tooltip(app);
    }

    /// Short progress line for the tray tooltip, while running.
    pub fn tray_line(&self) -> Option<String> {
        let s = self.state.lock().unwrap();
        if s.running {
            Some(if s.total > 0 && s.stage.starts_with("Нарезаю") {
                format!("Сбор фраз: {}/{}", s.done, s.total)
            } else {
                format!("Сбор фраз: {}", s.stage)
            })
        } else {
            s.installing.as_ref().map(|t| format!("Установка: {t}"))
        }
    }
}

struct AppReport {
    app: AppHandle,
    cancel: Arc<AtomicBool>,
}

impl Report for AppReport {
    fn event(&self, e: Event) {
        let h = self.app.state::<Harvest>();
        h.update(&self.app, |s| match e {
            Event::Stage { text } => {
                s.log.push(format!("▶ {text}"));
                s.stage = text;
                s.done = 0;
                s.total = 0;
            }
            Event::Progress { done, total } => {
                s.done = done;
                s.total = total;
            }
            Event::Log { text } => s.log.push(text),
            Event::Added { pt, ru, done, want } => {
                s.log.push(format!("✓ {pt} → {ru}"));
                s.added.push((pt, ru));
                s.done = done;
                s.total = want;
            }
        });
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub fn open_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(HARVEST) {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, HARVEST, WebviewUrl::App("harvest.html".into()))
        .title("teach-me — база фраз")
        .inner_size(760.0, 840.0)
        .min_inner_size(560.0, 560.0)
        .center()
        .build();
}

#[derive(Serialize)]
pub struct Overview {
    state: HarvestState,
    tools: Vec<ToolStatus>,
    /// What must be fixed before a harvest can start (tools, keys).
    missing: Vec<String>,
    stats: Stats,
    /// Videos with Portuguese subtitles in the corpus.
    videos: i64,
    sources: Vec<SourceCfg>,
    level: String,
    macos: bool,
}

#[tauri::command]
pub async fn harvest_overview(app: AppHandle) -> CmdResult<Overview> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<AppState>();
        tools::refresh_path();
        let cfg = st.home.load_config().map_err(err)?;
        let (stats, videos) = {
            let db = st.db.lock().unwrap();
            let videos: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sources WHERE subs_file IS NOT NULL AND status != 'skip'
                     AND (variant IS NULL OR variant = 'pt-PT')",
                    [],
                    |r| r.get(0),
                )
                .map_err(err)?;
            (db.stats(DIR).map_err(err)?, videos)
        };
        Ok(Overview {
            state: app.state::<Harvest>().state.lock().unwrap().clone(),
            tools: tools::check(),
            missing: harvester::missing_requirements(&cfg),
            stats,
            videos,
            sources: harvester::sources::load(&st.home).map_err(err)?,
            level: cfg.harvest.level,
            macos: cfg!(target_os = "macos"),
        })
    })
    .await
    .map_err(err)?
}

#[tauri::command]
pub fn harvest_start(app: AppHandle, h: State<Harvest>, count: usize, find_new: bool) -> CmdResult<()> {
    {
        let s = h.state.lock().unwrap();
        if s.running || s.installing.is_some() {
            return Err("Сбор уже идёт".into());
        }
    }
    let count = count.clamp(1, 300);
    h.cancel.store(false, Ordering::Relaxed);
    h.update(&app, |s| {
        *s = HarvestState { running: true, stage: "Начинаю".into(), ..Default::default() };
    });
    let report = AppReport { app: app.clone(), cancel: h.cancel.clone() };
    std::thread::spawn(move || {
        let st = report.app.state::<AppState>();
        let result = st
            .home
            .load_config()
            .and_then(|cfg| harvester::harvest(&st.home, &cfg, count, find_new, &report));
        let stopped = report.cancelled();
        report.app.state::<Harvest>().update(&report.app, |s| {
            s.running = false;
            s.stage.clear();
            match &result {
                Ok(n) => {
                    s.result = Some(if stopped {
                        format!("Остановлено. Добавлено фраз: {n}")
                    } else if *n == 0 {
                        "Новых фраз не нашлось".to_string()
                    } else {
                        format!("Готово! Добавлено фраз: {n}")
                    });
                }
                Err(e) => {
                    s.failed = true;
                    s.result = Some(format!("Ошибка: {e}"));
                    s.log.push(format!("Ошибка: {e:#}"));
                }
            }
        });
    });
    Ok(())
}

#[tauri::command]
pub fn harvest_stop(app: AppHandle, h: State<Harvest>) {
    h.cancel.store(true, Ordering::Relaxed);
    h.update(&app, |s| s.log.push("Останавливаю после текущего шага…".into()));
}

/// Installs the missing tools one by one (winget on Windows, Homebrew on Mac).
#[tauri::command]
pub async fn tools_install(app: AppHandle, ids: Vec<String>) -> CmdResult<()> {
    {
        let s = app.state::<Harvest>();
        let s = s.state.lock().unwrap();
        if s.running || s.installing.is_some() {
            return Err("Подождите окончания текущей работы".into());
        }
    }
    tauri::async_runtime::spawn_blocking(move || {
        let h = app.state::<Harvest>();
        let mut errors = vec![];
        for id in ids {
            h.update(&app, |s| {
                s.installing = Some(id.clone());
                s.log.push(format!("▶ Устанавливаю {id}"));
            });
            let log = |line: String| h.update(&app, |s| s.log.push(line));
            if let Err(e) = tools::install(&id, &log) {
                errors.push(format!("{id}: {e}"));
                h.update(&app, |s| s.log.push(format!("✗ {id}: {e}")));
            } else {
                h.update(&app, |s| s.log.push(format!("✓ {id} установлен")));
            }
        }
        h.update(&app, |s| s.installing = None);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    })
    .await
    .map_err(err)?
}

#[tauri::command]
pub fn sources_save(st: State<AppState>, list: Vec<SourceCfg>) -> CmdResult<()> {
    harvester::sources::save(&st.home, &list).map_err(err)
}

/// Channel name for a pasted YouTube link (also checks that it opens).
#[tauri::command]
pub async fn source_probe(url: String) -> CmdResult<String> {
    tauri::async_runtime::spawn_blocking(move || {
        tools::refresh_path();
        harvester::sources::probe_channel(url.trim()).map_err(|e| format!("Не удалось открыть ссылку: {e}"))
    })
    .await
    .map_err(err)?
}

#[tauri::command]
pub fn show_settings(app: AppHandle) {
    std::thread::spawn(move || crate::open_settings(&app));
}
