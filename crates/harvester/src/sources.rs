//! Video sources: the channel list (sources.toml), finding new videos on the
//! channels, and the user's own files from inbox/.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use teachme_core::db::SourceRow;
use teachme_core::openai::OpenAi;
use teachme_core::{Config, Db, Home};

use crate::report::Report;
use crate::{media, subs, ytdlp};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCfg {
    pub name: String,
    /// news | show | vlog
    pub kind: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_videos: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct SourcesFile {
    #[serde(default)]
    source: Vec<SourceCfg>,
}

pub fn load(home: &Home) -> Result<Vec<SourceCfg>> {
    let path = home.sources_path();
    if !path.exists() {
        return Ok(vec![]);
    }
    let text = std::fs::read_to_string(&path).context("reading sources.toml")?;
    Ok(toml::from_str::<SourcesFile>(&text).context("parsing sources.toml")?.source)
}

const SOURCES_HEADER: &str = "# Источники видео для сборщика: португальские (pt-PT) YouTube-каналы.
# Список редактируется в программе: «База фраз» → «Каналы».
# kind: news | show | vlog. max_videos: сколько последних видео брать с канала.

";

pub fn save(home: &Home, list: &[SourceCfg]) -> Result<()> {
    let body = toml::to_string(&SourcesFile { source: list.to_vec() })?;
    std::fs::write(home.sources_path(), format!("{SOURCES_HEADER}{body}"))?;
    Ok(())
}

/// Channel name for a YouTube link; also checks that the link works.
pub fn probe_channel(url: &str) -> Result<String> {
    ytdlp::channel_title(url)
}

/// Finds new videos on the channels and downloads their Portuguese subtitles.
pub fn discover(home: &Home, cfg: &Config, db: &Db, r: &dyn Report) -> Result<()> {
    let list = load(home)?;
    let subs_dir = home.cache_dir().join("subs");
    std::fs::create_dir_all(&subs_dir)?;
    let http = reqwest::blocking::Client::new();
    let h = &cfg.harvest;
    let mut found = 0;
    for (i, src) in list.iter().enumerate() {
        if r.cancelled() {
            return Ok(());
        }
        r.stage(format!("Ищу новые видео: {} ({}/{})", src.name, i + 1, list.len()));
        r.progress(i, list.len());
        let videos = match ytdlp::list_videos(&src.url, src.max_videos.unwrap_or(h.videos_per_source)) {
            Ok(v) => v,
            Err(e) => {
                r.log(format!("{}: {e}", src.name));
                continue;
            }
        };
        for v in videos {
            if r.cancelled() {
                return Ok(());
            }
            if db.source_status(&v.id)?.is_some() {
                continue;
            }
            let mins = v.duration / 60.0;
            if v.duration > 0.0 && (mins < h.min_video_minutes || mins > h.max_video_minutes) {
                continue;
            }
            let mut row = SourceRow {
                video_id: v.id.clone(),
                url: ytdlp::watch_url(&v.id),
                channel: src.name.clone(),
                title: v.title.clone(),
                kind: src.kind.clone(),
                duration: v.duration,
                subs_file: None,
                status: "skip".into(),
            };
            let info = match ytdlp::video_info(&v.id) {
                Ok(i) => i,
                Err(e) => {
                    r.log(format!("{}: {e}", v.id));
                    continue;
                }
            };
            if let Some(t) = info["title"].as_str() {
                row.title = t.to_string();
            }
            match ytdlp::choose_subs(&info) {
                Some((_track, url)) => {
                    let path = subs_dir.join(format!("{}.json3", v.id));
                    let body = http.get(&url).send().and_then(|r| r.error_for_status()).and_then(|r| r.bytes());
                    match body {
                        Ok(b) => {
                            std::fs::write(&path, &b)?;
                            row.subs_file = Some(path.to_string_lossy().into_owned());
                            row.status = "new".into();
                            found += 1;
                            r.log(format!("+ {}", row.title));
                        }
                        Err(e) => r.log(format!("{}: субтитры не скачались: {e}", v.id)),
                    }
                }
                None => r.log(format!("– не на португальском или без субтитров: {}", row.title)),
            }
            db.upsert_source(&row)?;
        }
    }
    r.progress(list.len(), list.len());
    r.log(format!("Новых видео с субтитрами: {found}"));
    Ok(())
}

pub const VIDEO_EXT: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "m4v"];

fn sidecar_subs(video: &Path) -> Option<PathBuf> {
    let dir = video.parent()?;
    let stem = video.file_stem()?.to_string_lossy().to_string();
    let mut found = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        let name = p.file_name()?.to_string_lossy().to_string();
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if (ext == "srt" || ext == "vtt") && name.starts_with(&stem) {
            // Prefer tracks marked as Portuguese: movie.pt.srt, movie.por.srt
            let lname = name.to_lowercase();
            if lname.contains(".pt") || lname.contains(".por") {
                return Some(p);
            }
            found.get_or_insert(p);
        }
    }
    found
}

/// The user's own videos in inbox/: subtitles next to the file, or speech
/// recognition when there are none.
pub fn import(home: &Home, cfg: &Config, db: &Db, r: &dyn Report) -> Result<()> {
    let inbox = home.inbox_dir();
    std::fs::create_dir_all(&inbox)?;
    let subs_dir = home.cache_dir().join("subs");
    std::fs::create_dir_all(&subs_dir)?;
    let mut api: Option<OpenAi> = None;
    for entry in std::fs::read_dir(&inbox)?.flatten() {
        if r.cancelled() {
            return Ok(());
        }
        let path = entry.path();
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !VIDEO_EXT.contains(&ext.as_str()) {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let id = format!("file:{name}");
        if db.source_status(&id)?.is_some() {
            continue;
        }
        r.stage(format!("Свой файл: {name}"));
        let subs_file = match sidecar_subs(&path) {
            Some(s) => s,
            None => {
                if api.is_none() {
                    api = Some(OpenAi::new(&cfg.openai)?);
                }
                r.log("субтитров нет — распознаю речь");
                let tmp = home.cache_dir().join("audio");
                let mut cues = vec![];
                for (offset, chunk) in media::audio_chunks(&path, &tmp, 20.0 * 60.0)? {
                    let v = api.as_ref().unwrap().transcribe_segments(std::fs::read(&chunk)?, "audio.mp3", "pt")?;
                    for s in v["segments"].as_array().into_iter().flatten() {
                        cues.push(subs::Cue {
                            start_ms: ((offset + s["start"].as_f64().unwrap_or(0.0)) * 1000.0) as i64,
                            end_ms: ((offset + s["end"].as_f64().unwrap_or(0.0)) * 1000.0) as i64,
                            text: s["text"].as_str().unwrap_or_default().trim().to_string(),
                        });
                    }
                    let _ = std::fs::remove_file(&chunk);
                }
                let safe: String = name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
                let out = subs_dir.join(format!("{safe}.cues.json"));
                std::fs::write(&out, serde_json::to_string(&cues)?)?;
                out
            }
        };
        db.upsert_source(&SourceRow {
            video_id: id,
            url: path.to_string_lossy().into_owned(),
            channel: "Свои файлы".into(),
            title: name,
            kind: "file".into(),
            duration: media::duration_secs(&path).unwrap_or(0.0),
            subs_file: Some(subs_file.to_string_lossy().into_owned()),
            status: "new".into(),
        })?;
    }
    Ok(())
}
