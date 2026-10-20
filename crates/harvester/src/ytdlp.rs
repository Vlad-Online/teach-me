use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct VideoRef {
    pub id: String,
    pub title: String,
    pub duration: f64,
}

pub fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

fn ytdlp() -> Command {
    let mut c = Command::new("yt-dlp");
    // YouTube needs a JS runtime for its player challenges; Node is installed.
    c.args(["--js-runtimes", "node", "--no-warnings", "--no-progress"]);
    c
}

fn run_json(mut cmd: Command) -> Result<Value> {
    let out = cmd.output().context("failed to start yt-dlp (is it installed and in PATH?)")?;
    if !out.status.success() {
        bail!("yt-dlp failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    serde_json::from_slice(&out.stdout).context("yt-dlp returned invalid JSON")
}

/// Latest uploads of a channel/playlist, without fetching each video.
pub fn list_videos(url: &str, limit: usize) -> Result<Vec<VideoRef>> {
    let mut cmd = ytdlp();
    cmd.args(["--flat-playlist", "-J", "--playlist-end", &limit.to_string(), url]);
    let v = run_json(cmd)?;
    let entries = v["entries"].as_array().cloned().unwrap_or_default();
    Ok(entries
        .iter()
        .filter_map(|e| {
            Some(VideoRef {
                id: e["id"].as_str()?.to_string(),
                title: e["title"].as_str().unwrap_or("").to_string(),
                duration: e["duration"].as_f64().unwrap_or(0.0),
            })
        })
        .collect())
}

pub fn video_info(id: &str) -> Result<Value> {
    let mut cmd = ytdlp();
    cmd.args(["-j", "--skip-download", &watch_url(id)]);
    run_json(cmd)
}

/// Picks a Portuguese subtitle track: manual pt subtitles, then the original
/// ASR track (`pt-orig`), then `pt` ASR if the video's language is Portuguese.
/// Returns (track name, json3 URL). None means the video is not in Portuguese.
pub fn choose_subs(info: &Value) -> Option<(String, String)> {
    let json3 = |tracks: &Value, key: &str| -> Option<String> {
        tracks[key]
            .as_array()?
            .iter()
            .find(|f| f["ext"] == "json3")
            .and_then(|f| f["url"].as_str())
            .map(String::from)
    };
    let manual = &info["subtitles"];
    for key in ["pt-PT", "pt"] {
        if let Some(url) = json3(manual, key) {
            return Some((format!("manual:{key}"), url));
        }
    }
    let auto = &info["automatic_captions"];
    if let Some(url) = json3(auto, "pt-orig") {
        return Some(("auto:pt-orig".into(), url));
    }
    let lang = info["language"].as_str().unwrap_or("");
    let has_orig = auto.as_object().is_some_and(|m| m.keys().any(|k| k.ends_with("-orig")));
    if lang.starts_with("pt") && !has_orig {
        if let Some(url) = json3(auto, "pt") {
            return Some(("auto:pt".into(), url));
        }
    }
    None
}

/// Downloads a video in ≤480p (H.264 + AAC when available) as mp4.
pub fn download_video(id: &str, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(format!("{id}.mp4"));
    if dest.is_file() {
        return Ok(dest);
    }
    let template = dir.join(format!("{id}.%(ext)s"));
    let mut cmd = ytdlp();
    cmd.args([
        "-f",
        "bv*[height<=480][vcodec^=avc1]+ba[ext=m4a]/bv*[height<=480]+ba/b[height<=480]/b",
        "--merge-output-format",
        "mp4",
        "-o",
    ])
    .arg(&template)
    .arg(watch_url(id));
    let out = cmd.output().context("failed to start yt-dlp")?;
    if !out.status.success() || !dest.is_file() {
        // Drop partial downloads (<id>.f135.mp4.part etc.).
        for entry in std::fs::read_dir(dir)?.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&format!("{id}.")) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        bail!("video download failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(dest)
}
