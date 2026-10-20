use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd.output().with_context(|| format!("failed to start {what}"))?;
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn duration_secs(path: &Path) -> Result<f64> {
    let out = crate::tools::command("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .context("failed to start ffprobe")?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .context("ffprobe: no duration")
}

/// One video frame as JPEG (512 px wide), for the burned-in subtitle check.
pub fn frame_jpeg(video: &Path, at_ms: i64) -> Result<Vec<u8>> {
    let out = crate::tools::command("ffmpeg")
        .args(["-v", "error", "-ss", &format!("{:.3}", at_ms as f64 / 1000.0), "-i"])
        .arg(video)
        .args(["-frames:v", "1", "-vf", "scale=512:-2", "-q:v", "5", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1"])
        .output()
        .context("failed to start ffmpeg")?;
    if !out.status.success() || out.stdout.is_empty() {
        bail!("ffmpeg (frame) failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Cuts [start, end) into a small, loudness-normalized 480p mp4.
/// `blur` = (top, bottom) as fractions of the frame height: a band with
/// burned-in subtitles that must not give the answer away.
pub fn cut_clip(video: &Path, start_ms: i64, end_ms: i64, blur: Option<(f64, f64)>, out: &Path) -> Result<()> {
    let start = start_ms as f64 / 1000.0;
    let dur = (end_ms - start_ms) as f64 / 1000.0;
    let fade_out = (dur - 0.12).max(0.0);
    let scale = "scale=-2:'min(480,ih)':flags=bicubic,format=yuv420p";
    let video_filter = match blur {
        None => format!("[0:v]{scale}[v]"),
        Some((top, bottom)) => {
            let h = (bottom - top).max(0.12);
            format!(
                "[0:v]{scale},split[m][s];\
                 [s]crop=iw:trunc(ih*{h:.3}/2)*2:0:trunc(ih*{top:.3}/2)*2,boxblur=luma_radius=min(h/5\\,14):luma_power=3:chroma_radius=min(ch/5\\,7)[b];\
                 [m][b]overlay=0:trunc(main_h*{top:.3}/2)*2[v]"
            )
        }
    };
    run(
        crate::tools::command("ffmpeg")
            .args(["-y", "-v", "error", "-ss", &format!("{start:.3}"), "-i"])
            .arg(video)
            .args([
                "-t",
                &format!("{dur:.3}"),
                "-filter_complex",
                &video_filter,
                "-map",
                "[v]",
                "-map",
                "0:a:0?",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "27",
                "-c:a",
                "aac",
                "-b:a",
                "80k",
                "-ar",
                "44100",
                "-af",
                &format!("loudnorm=I=-16:TP=-1.5:LRA=11,afade=t=in:d=0.04,afade=t=out:st={fade_out:.3}:d=0.12"),
                "-movflags",
                "+faststart",
            ])
            .arg(out),
        "ffmpeg (cut)",
    )
}

/// Mono 16 kHz mp3 chunks of at most `chunk_secs`, for whisper (25 MB limit).
pub fn audio_chunks(video: &Path, dir: &Path, chunk_secs: f64) -> Result<Vec<(f64, PathBuf)>> {
    std::fs::create_dir_all(dir)?;
    let total = duration_secs(video)?;
    let mut chunks = vec![];
    let mut t = 0.0;
    let mut i = 0;
    while t < total {
        let out = dir.join(format!("chunk{i:03}.mp3"));
        run(
            crate::tools::command("ffmpeg")
                .args(["-y", "-v", "error", "-ss", &format!("{t:.3}"), "-i"])
                .arg(video)
                .args(["-t", &format!("{chunk_secs:.3}"), "-vn", "-ac", "1", "-ar", "16000", "-b:a", "32k"])
                .arg(&out),
            "ffmpeg (audio)",
        )?;
        chunks.push((t, out));
        t += chunk_secs;
        i += 1;
    }
    Ok(chunks)
}
