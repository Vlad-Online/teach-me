use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sync: SyncConfig,
    pub llm: LlmConfig,
    pub anthropic: AnthropicConfig,
    pub openai: OpenAiConfig,
    pub trainer: TrainerConfig,
    pub harvest: HarvestConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Desktop: answer phones on the local network.
    pub enabled: bool,
    pub udp_port: u16,
    pub http_port: u16,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self { enabled: true, udp_port: crate::sync::UDP_PORT, http_port: crate::sync::HTTP_PORT }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// Text tasks (rating, translation, answer checking): "anthropic" | "openai".
    pub provider: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self { provider: "anthropic".into() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub enrich_model: String,
    /// output_config.effort: low | medium | high
    pub enrich_effort: String,
    pub judge_model: String,
    pub judge_effort: String,
    /// Thinking while the user waits for the verdict; off = faster.
    pub judge_thinking: bool,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            enrich_model: "claude-haiku-5-5".into(),
            enrich_effort: "medium".into(),
            judge_model: "claude-haiku-5-5".into(),
            judge_effort: "low".into(),
            judge_thinking: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub base_url: String,
    /// Speech-to-text for the user's answers.
    pub transcribe_model: String,
    /// Translation and phrase enrichment (once per phrase).
    pub enrich_model: String,
    /// Answer checking (once per attempt, must be fast and cheap).
    pub judge_model: String,
    /// `reasoning_effort` for reasoning models ("none", "low", ...);
    /// empty for classic models that take a temperature instead.
    pub enrich_reasoning: String,
    pub judge_reasoning: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TrainerConfig {
    /// Minutes between regular popups.
    pub interval_minutes: u32,
    /// Phrases answered wrong come back after this many minutes (if due).
    pub relearn_minutes: u32,
    /// Popups are shown only between these hours (local time, 0-24).
    pub active_from_hour: u32,
    pub active_to_hour: u32,
    pub new_per_day: u32,
    /// Do not pop up if there was no keyboard/mouse input for this long.
    pub idle_minutes: u32,
    /// Push-to-talk global hotkey, Tauri syntax.
    pub hotkey: String,
    /// Hotkey that opens a card right now.
    pub card_now_hotkey: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HarvestConfig {
    pub min_words: usize,
    pub max_words: usize,
    pub min_seconds: f64,
    pub max_seconds: f64,
    /// Videos to look at per source when discovering.
    pub videos_per_source: usize,
    pub min_video_minutes: f64,
    pub max_video_minutes: f64,
    /// Keep downloaded full videos after cutting clips.
    pub keep_videos: bool,
    pub level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sync: SyncConfig::default(),
            llm: LlmConfig::default(),
            anthropic: AnthropicConfig::default(),
            openai: OpenAiConfig::default(),
            trainer: TrainerConfig::default(),
            harvest: HarvestConfig::default(),
        }
    }
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".into(),
            transcribe_model: "gpt-4o-mini-transcribe".into(),
            enrich_model: "gpt-6-luna".into(),
            judge_model: "gpt-6-luna".into(),
            enrich_reasoning: "low".into(),
            judge_reasoning: "none".into(),
        }
    }
}

impl Default for TrainerConfig {
    fn default() -> Self {
        Self {
            interval_minutes: 25,
            relearn_minutes: 15,
            active_from_hour: 9,
            active_to_hour: 23,
            new_per_day: 10,
            idle_minutes: 3,
            hotkey: "Ctrl+Space".into(),
            card_now_hotkey: "Ctrl+Alt+P".into(),
        }
    }
}

impl Default for HarvestConfig {
    fn default() -> Self {
        Self {
            min_words: 3,
            max_words: 8,
            min_seconds: 1.0,
            max_seconds: 5.5,
            videos_per_source: 6,
            min_video_minutes: 1.0,
            max_video_minutes: 60.0,
            keep_videos: false,
            level: "A2-B1".into(),
        }
    }
}

/// Project home: holds config.toml, sources.toml and data/.
#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
}

impl Home {
    /// `TEACH_ME_HOME`, otherwise the first directory with config.toml
    /// walking up from the current dir and from the executable's dir.
    pub fn locate() -> Result<Self> {
        if let Ok(dir) = std::env::var("TEACH_ME_HOME") {
            return Ok(Self { root: PathBuf::from(dir) });
        }
        let mut starts = vec![];
        if let Ok(cwd) = std::env::current_dir() {
            starts.push(cwd);
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                starts.push(dir.to_path_buf());
            }
        }
        for start in starts {
            for dir in start.ancestors() {
                if dir.join(CONFIG_FILE).is_file() {
                    return Ok(Self { root: dir.to_path_buf() });
                }
            }
        }
        bail!("config.toml not found (set TEACH_ME_HOME or run from the project folder)")
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }
    pub fn sources_path(&self) -> PathBuf {
        self.root.join("sources.toml")
    }
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }
    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("teach-me.db")
    }
    pub fn clips_dir(&self) -> PathBuf {
        self.data_dir().join("clips")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir().join("cache")
    }
    pub fn inbox_dir(&self) -> PathBuf {
        self.root.join("inbox")
    }

    pub fn load_config(&self) -> Result<Config> {
        load_config(&self.config_path())
    }
}

pub fn load_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}
