//! Heartbeat service.
//!
//! Periodically checks `HEARTBEAT.md` and asks the LLM whether the
//! contents describe active work that warrants a follow-up turn. The
//! gateway scheduler ticks this on `gateway.heartbeat.interval_s` (see
//! `zunel-cli::gateway`); the per-call summary uses
//! `gateway.heartbeat.keep_recent_messages` to bound prompt size.

use std::path::PathBuf;
use std::sync::Arc;

use zunel_config::HeartbeatConfig;
use zunel_providers::{ChatMessage, GenerationSettings, LLMProvider};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("provider error: {0}")]
    Provider(#[from] zunel_providers::Error),
}

pub struct HeartbeatService {
    workspace: PathBuf,
    provider: Arc<dyn LLMProvider>,
    model: String,
    config: HeartbeatConfig,
}

impl HeartbeatService {
    pub fn new(workspace: PathBuf, provider: Arc<dyn LLMProvider>, model: String) -> Self {
        Self {
            workspace,
            provider,
            model,
            config: HeartbeatConfig::default(),
        }
    }

    pub fn with_config(mut self, config: HeartbeatConfig) -> Self {
        self.config = config;
        self
    }

    pub fn config(&self) -> &HeartbeatConfig {
        &self.config
    }

    /// How many most-recent session messages a heartbeat-driven turn
    /// should keep when re-priming the LLM. Wired from
    /// `gateway.heartbeat.keep_recent_messages`.
    pub fn keep_recent_messages(&self) -> usize {
        self.config.keep_recent_messages.max(1)
    }

    pub async fn trigger_now(&self) -> Result<Option<String>> {
        if !self.config.enabled {
            return Ok(None);
        }
        let heartbeat_path = self.workspace.join("HEARTBEAT.md");
        if !heartbeat_path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&heartbeat_path)?;
        if content.trim().is_empty() {
            return Ok(None);
        }
        let response = self
            .provider
            .generate(
                &self.model,
                &[
                    ChatMessage::system(
                        "You are a heartbeat agent. Decide whether HEARTBEAT.md has active tasks.",
                    ),
                    ChatMessage::user(format!(
                        "Review HEARTBEAT.md and respond with `skip` or `run: <tasks>`.\n\n{content}"
                    )),
                ],
                &[],
                &GenerationSettings::default(),
            )
            .await?;
        Ok(parse_decision(
            response.content.as_deref().unwrap_or_default(),
        ))
    }
}

fn parse_decision(content: &str) -> Option<String> {
    let trimmed = content.trim();
    let rest = trimmed.strip_prefix("run:")?.trim();
    (!rest.is_empty()).then(|| rest.to_string())
}
