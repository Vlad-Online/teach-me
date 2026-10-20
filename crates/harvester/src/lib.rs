//! Building the phrase base: Portuguese videos → subtitles → popular phrases
//! → clips with translation. Used by the desktop app («База фраз» window)
//! and by the `harvester` command-line tool.

pub mod media;
pub mod mine;
pub mod report;
pub mod sources;
pub mod subs;
pub mod tools;
pub mod ytdlp;

use anyhow::{bail, Result};

pub use report::{Console, Event, Report};
use teachme_core::llm::Llm;
use teachme_core::openai::OpenAi;
use teachme_core::{Config, Db, Home};

/// Everything needed before a harvest can start; empty when ready.
pub fn missing_requirements(cfg: &Config) -> Vec<String> {
    let mut out = vec![];
    for t in tools::check() {
        if !t.found {
            out.push(format!("Не установлен {} — {}", t.title, t.purpose));
        }
    }
    if Llm::from_config(cfg).is_err() {
        let who = if cfg.llm.provider == "openai" { "OpenAI" } else { "Claude" };
        out.push(format!("Нет ключа {who} (перевод фраз) — откройте «Настройки»"));
    }
    if OpenAi::new(&cfg.openai).is_err() {
        out.push("Нет ключа OpenAI (распознавание речи в клипах) — откройте «Настройки»".into());
    }
    out
}

/// One full run: new videos on the channels (optional), the user's own files,
/// then `want` new phrases. Returns how many phrases were added.
pub fn harvest(home: &Home, cfg: &Config, want: usize, find_new_videos: bool, r: &dyn Report) -> Result<usize> {
    tools::refresh_path();
    let missing = missing_requirements(cfg);
    if !missing.is_empty() {
        bail!("{}", missing.join("; "));
    }
    let db = Db::open(&home.db_path())?;
    if find_new_videos {
        sources::discover(home, cfg, &db, r)?;
    }
    if r.cancelled() {
        return Ok(0);
    }
    sources::import(home, cfg, &db, r)?;
    if r.cancelled() {
        return Ok(0);
    }
    mine::run(home, cfg, &db, want, false, r)
}
