//! External programs: yt-dlp, ffmpeg/ffprobe and a JavaScript runtime
//! (yt-dlp needs one for YouTube). Finding them, running them without a
//! console window, installing what is missing.

use anyhow::{bail, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Makes freshly installed tools visible to this process. A GUI app keeps the
/// PATH it was started with (and on macOS, apps started from Finder do not
/// see Homebrew at all).
pub fn refresh_path() {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut extra: Vec<PathBuf> = vec![];
    #[cfg(windows)]
    {
        for key in [
            r"HKCU\Environment",
            r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
        ] {
            let mut cmd = Command::new("reg");
            cmd.args(["query", key, "/v", "Path"]);
            no_window(&mut cmd);
            if let Ok(out) = cmd.output() {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = text.lines().find(|l| l.trim_start().starts_with("Path")) {
                    if let Some(value) = line.split("REG_").nth(1).and_then(|v| v.split_once(' ')).map(|(_, v)| v.trim()) {
                        extra.extend(std::env::split_paths(&expand_env(value)));
                    }
                }
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            extra.push(PathBuf::from(local).join(r"Microsoft\WinGet\Links"));
        }
        extra.push(PathBuf::from(r"C:\Program Files\nodejs"));
    }
    #[cfg(target_os = "macos")]
    {
        extra.push("/opt/homebrew/bin".into());
        extra.push("/usr/local/bin".into());
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        extra.push(PathBuf::from(home).join(".deno").join("bin"));
    }
    for d in extra {
        if !d.as_os_str().is_empty() && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    if let Ok(path) = std::env::join_paths(dirs) {
        std::env::set_var("PATH", path);
    }
}

/// `%LOCALAPPDATA%\x` → value of the variable.
#[cfg(windows)]
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                out.push_str(&std::env::var(name).unwrap_or_else(|_| format!("%{name}%")));
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

/// Full path of a program found in PATH.
pub fn find(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p).map(|d| d.join(&exe)).find(|f| f.is_file())
    })
}

/// A command for an external tool: resolved path, no console window.
pub fn command(name: &str) -> Command {
    let mut cmd = Command::new(find(name).unwrap_or_else(|| PathBuf::from(name)));
    no_window(&mut cmd);
    cmd
}

/// JavaScript runtime for yt-dlp: Deno (yt-dlp's default) or Node.js.
pub fn js_runtime() -> Option<&'static str> {
    ["deno", "node"].into_iter().find(|n| find(n).is_some())
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    /// "yt-dlp" | "ffmpeg" | "js"
    pub id: String,
    pub title: String,
    pub found: bool,
    pub version: String,
    pub purpose: String,
}

fn version_of(name: &str, arg: &str) -> Option<String> {
    let out = command(name).arg(arg).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?.trim().to_string();
    // "ffmpeg version 7.1 Copyright…" → "7.1"
    Some(first.strip_prefix("ffmpeg version ").map(|s| s.split_whitespace().next().unwrap_or(s).to_string()).unwrap_or(first))
}

pub fn check() -> Vec<ToolStatus> {
    let ytdlp = find("yt-dlp").and_then(|_| version_of("yt-dlp", "--version"));
    let ffmpeg = (find("ffmpeg").is_some() && find("ffprobe").is_some())
        .then(|| version_of("ffmpeg", "-version"))
        .flatten();
    let js = js_runtime().and_then(|n| version_of(n, "--version").map(|v| format!("{n} {}", v.trim_start_matches("deno "))));
    vec![
        ToolStatus {
            id: "yt-dlp".into(),
            title: "yt-dlp".into(),
            found: ytdlp.is_some(),
            version: ytdlp.unwrap_or_default(),
            purpose: "скачивает субтитры и видео с YouTube".into(),
        },
        ToolStatus {
            id: "ffmpeg".into(),
            title: "FFmpeg".into(),
            found: ffmpeg.is_some(),
            version: ffmpeg.unwrap_or_default(),
            purpose: "вырезает клипы и размывает вшитые субтитры".into(),
        },
        ToolStatus {
            id: "js".into(),
            title: "Deno".into(),
            found: js.is_some(),
            version: js.unwrap_or_default(),
            purpose: "нужен yt-dlp, чтобы открывать YouTube".into(),
        },
    ]
}

/// Package manager command that installs one tool.
fn install_command(id: &str) -> Result<Command> {
    #[cfg(windows)]
    {
        let pkg = match id {
            "yt-dlp" => "yt-dlp.yt-dlp",
            "ffmpeg" => "Gyan.FFmpeg",
            "js" => "DenoLand.Deno",
            _ => bail!("unknown tool {id}"),
        };
        if find("winget").is_none() {
            bail!("winget не найден: установите «Установщик приложений» (App Installer) из Microsoft Store");
        }
        let mut cmd = command("winget");
        cmd.args([
            "install",
            "--id",
            pkg,
            "--exact",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
            "--disable-interactivity",
        ]);
        Ok(cmd)
    }
    #[cfg(not(windows))]
    {
        let pkg = match id {
            "yt-dlp" => "yt-dlp",
            "ffmpeg" => "ffmpeg",
            "js" => "deno",
            _ => bail!("unknown tool {id}"),
        };
        if find("brew").is_none() {
            bail!("Homebrew не найден: установите его с https://brew.sh, затем повторите");
        }
        let mut cmd = command("brew");
        cmd.args(["install", pkg]);
        Ok(cmd)
    }
}

/// Installs a tool, sending output lines to `log`. Returns when done.
pub fn install(id: &str, log: &dyn Fn(String)) -> Result<()> {
    use std::io::{BufRead, BufReader};
    let mut cmd = install_command(id)?;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            // winget redraws progress bars with \r: keep the last state, skip bare bars.
            let line = line.rsplit('\r').next().unwrap_or("").trim().to_string();
            if line.chars().filter(|c| c.is_alphanumeric()).count() >= 3 {
                log(line);
            }
        }
    }
    let status = child.wait()?;
    refresh_path();
    let ok = match id {
        "js" => js_runtime().is_some(),
        "ffmpeg" => find("ffmpeg").is_some() && find("ffprobe").is_some(),
        other => find(other).is_some(),
    };
    if !ok {
        bail!("установка не удалась (код {})", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Where the tools are, for the log: helps when something is not found.
pub fn describe(name: &str) -> String {
    find(name).map(|p| p.display().to_string()).unwrap_or_else(|| "не найден".into())
}
