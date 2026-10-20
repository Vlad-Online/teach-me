//! Text model for translation, rating and answer checking: Claude or OpenAI,
//! chosen by `[llm] provider`. Speech-to-text always goes through OpenAI.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::anthropic::Claude;
use crate::config::Config;
use crate::openai::{OpenAi, Purpose};

pub enum Llm {
    Claude(Claude),
    OpenAi(OpenAi),
}

impl Llm {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        match cfg.llm.provider.as_str() {
            "anthropic" => Ok(Llm::Claude(Claude::new(&cfg.anthropic)?)),
            "openai" => Ok(Llm::OpenAi(OpenAi::new(&cfg.openai)?)),
            other => bail!("unknown [llm] provider: {other} (anthropic | openai)"),
        }
    }

    pub fn json(&self, purpose: Purpose, system: &str, user: &str, name: &str, schema: Value) -> Result<Value> {
        match self {
            Llm::Claude(c) => c.chat_json(purpose, system, user, schema),
            Llm::OpenAi(o) => o.chat_json(purpose, system, user, name, schema),
        }
    }

    pub fn json_image(&self, purpose: Purpose, system: &str, user: &str, jpeg: &[u8], name: &str, schema: Value) -> Result<Value> {
        match self {
            Llm::Claude(c) => c.chat_json_image(purpose, system, user, jpeg, schema),
            Llm::OpenAi(o) => o.chat_json_image(purpose, system, user, jpeg, name, schema),
        }
    }
}
