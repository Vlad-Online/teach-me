use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{multipart, Client};
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::OpenAiConfig;

/// Which configured model to use.
#[derive(Debug, Clone, Copy)]
pub enum Purpose {
    /// Harvesting: rating, translation, checks. Quality over latency.
    Enrich,
    /// Answer checking while the user waits.
    Judge,
}

pub struct OpenAi {
    cfg: OpenAiConfig,
    http: Client,
}

impl OpenAi {
    pub fn new(cfg: &OpenAiConfig) -> Result<Self> {
        if cfg.api_key.trim().is_empty() {
            bail!("OpenAI api_key is empty in config.toml");
        }
        let http = Client::builder().timeout(Duration::from_secs(120)).build()?;
        Ok(Self { cfg: cfg.clone(), http })
    }

    /// Model ids available to this key. Also a free way to check the key.
    pub fn list_models(base_url: &str, api_key: &str) -> Result<Vec<String>> {
        let http = Client::builder().timeout(Duration::from_secs(20)).build()?;
        let resp = http
            .get(format!("{}/models", base_url.trim_end_matches('/')))
            .bearer_auth(api_key.trim())
            .send()?;
        let body = Self::check(resp)?;
        let mut ids: Vec<String> = body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect();
        ids.sort();
        Ok(ids)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.cfg.base_url.trim_end_matches('/'), path)
    }

    fn check(resp: reqwest::blocking::Response) -> Result<Value> {
        let status = resp.status();
        let body: Value = resp.json().context("OpenAI: invalid JSON response")?;
        if !status.is_success() {
            let msg = body["error"]["message"].as_str().unwrap_or("unknown error");
            bail!("OpenAI {status}: {msg}");
        }
        Ok(body)
    }

    /// Speech to text. `language` is ISO-639-1 ("ru", "pt").
    pub fn transcribe(&self, audio: Vec<u8>, filename: &str, language: &str, prompt: &str) -> Result<String> {
        self.transcribe_with(&self.cfg.transcribe_model.clone(), audio, filename, language, prompt)
    }

    pub fn transcribe_with(&self, model: &str, audio: Vec<u8>, filename: &str, language: &str, prompt: &str) -> Result<String> {
        let part = multipart::Part::bytes(audio).file_name(filename.to_string());
        let mut form = multipart::Form::new()
            .part("file", part)
            .text("model", model.to_string())
            .text("language", language.to_string())
            .text("response_format", "json");
        if !prompt.is_empty() {
            form = form.text("prompt", prompt.to_string());
        }
        let resp = self
            .http
            .post(self.url("audio/transcriptions"))
            .bearer_auth(&self.cfg.api_key)
            .multipart(form)
            .send()?;
        let body = Self::check(resp)?;
        Ok(body["text"].as_str().unwrap_or_default().trim().to_string())
    }

    /// `whisper-1` with segment timestamps, for files without subtitles.
    /// Returns `verbose_json` as is.
    pub fn transcribe_segments(&self, audio: Vec<u8>, filename: &str, language: &str) -> Result<Value> {
        let part = multipart::Part::bytes(audio).file_name(filename.to_string());
        let form = multipart::Form::new()
            .part("file", part)
            .text("model", "whisper-1")
            .text("language", language.to_string())
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "segment");
        let resp = self
            .http
            .post(self.url("audio/transcriptions"))
            .bearer_auth(&self.cfg.api_key)
            .multipart(form)
            .send()?;
        Self::check(resp)
    }

    /// Chat completion constrained to a JSON schema; returns the parsed object.
    pub fn chat_json(&self, purpose: Purpose, system: &str, user: &str, schema_name: &str, schema: Value) -> Result<Value> {
        self.chat_json_content(purpose, system, json!(user), schema_name, schema)
    }

    /// Same, with a JPEG image attached (low detail: cheap).
    pub fn chat_json_image(&self, purpose: Purpose, system: &str, user: &str, jpeg: &[u8], schema_name: &str, schema: Value) -> Result<Value> {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
        let content = json!([
            {"type": "text", "text": user},
            {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{b64}"), "detail": "low"}}
        ]);
        self.chat_json_content(purpose, system, content, schema_name, schema)
    }

    fn chat_json_content(&self, purpose: Purpose, system: &str, user: Value, schema_name: &str, schema: Value) -> Result<Value> {
        let (model, effort) = match purpose {
            Purpose::Enrich => (&self.cfg.enrich_model, &self.cfg.enrich_reasoning),
            Purpose::Judge => (&self.cfg.judge_model, &self.cfg.judge_reasoning),
        };
        let mut req = json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": schema_name, "strict": true, "schema": schema}
            }
        });
        // Reasoning models take an effort level and reject custom temperature;
        // an empty effort means a classic model.
        if effort.is_empty() {
            req["temperature"] = json!(0.2);
        } else {
            req["reasoning_effort"] = json!(effort);
        }
        let resp = self
            .http
            .post(self.url("chat/completions"))
            .bearer_auth(&self.cfg.api_key)
            .json(&req)
            .send()?;
        let body = Self::check(resp)?;
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow!("OpenAI: empty completion"))?;
        serde_json::from_str(content).context("OpenAI: completion is not valid JSON")
    }
}
