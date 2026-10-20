//! Claude Messages API over raw HTTP (no official Rust SDK).

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::AnthropicConfig;
use crate::openai::Purpose;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

pub struct Claude {
    cfg: AnthropicConfig,
    http: Client,
}

impl Claude {
    pub fn new(cfg: &AnthropicConfig) -> Result<Self> {
        if cfg.api_key.trim().is_empty() {
            bail!("Anthropic api_key is empty in config.toml");
        }
        let http = Client::builder().timeout(Duration::from_secs(180)).build()?;
        Ok(Self { cfg: cfg.clone(), http })
    }

    /// Model ids available to this key. Also a free way to check the key.
    pub fn list_models(api_key: &str) -> Result<Vec<String>> {
        let http = Client::builder().timeout(Duration::from_secs(20)).build()?;
        let resp = http
            .get("https://api.anthropic.com/v1/models?limit=100")
            .header("x-api-key", api_key.trim())
            .header("anthropic-version", API_VERSION)
            .send()?;
        let status = resp.status();
        let v: Value = resp.json().context("Claude: invalid JSON response")?;
        if !status.is_success() {
            bail!("{}", v["error"]["message"].as_str().unwrap_or("ключ не подошёл"));
        }
        Ok(v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect())
    }

    pub fn chat_json(&self, purpose: Purpose, system: &str, user: &str, schema: Value) -> Result<Value> {
        self.request(purpose, system, json!(user), schema)
    }

    pub fn chat_json_image(&self, purpose: Purpose, system: &str, user: &str, jpeg: &[u8], schema: Value) -> Result<Value> {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
        let content = json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": b64}},
            {"type": "text", "text": user}
        ]);
        self.request(purpose, system, content, schema)
    }

    fn request(&self, purpose: Purpose, system: &str, content: Value, schema: Value) -> Result<Value> {
        let (model, effort, thinking) = match purpose {
            Purpose::Enrich => (&self.cfg.enrich_model, &self.cfg.enrich_effort, true),
            Purpose::Judge => (&self.cfg.judge_model, &self.cfg.judge_effort, self.cfg.judge_thinking),
        };
        let mut body = json!({
            "model": model,
            // Thinking counts toward max_tokens: leave room beyond the short JSON answer.
            "max_tokens": if thinking { 8000 } else { 1024 },
            "system": system,
            "messages": [{"role": "user", "content": content}],
            "output_config": {
                "effort": effort,
                "format": {"type": "json_schema", "schema": schema}
            }
        });
        if !thinking {
            // Answer checking: the user is waiting, skip thinking (allowed at effort ≤ high).
            body["thinking"] = json!({"type": "disabled"});
        }
        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()?;
        let status = resp.status();
        let v: Value = resp.json().context("Claude: invalid JSON response")?;
        if !status.is_success() {
            let msg = v["error"]["message"].as_str().unwrap_or("unknown error");
            bail!("Claude {status}: {msg}");
        }
        match v["stop_reason"].as_str() {
            Some("refusal") => bail!("Claude declined the request"),
            Some("max_tokens") => bail!("Claude: max_tokens reached before the answer"),
            _ => {}
        }
        // A response can start with thinking blocks: take the text block.
        let text = v["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
            .and_then(|b| b["text"].as_str())
            .ok_or_else(|| anyhow!("Claude: no text in response"))?;
        serde_json::from_str(text).context("Claude: answer is not valid JSON")
    }
}
