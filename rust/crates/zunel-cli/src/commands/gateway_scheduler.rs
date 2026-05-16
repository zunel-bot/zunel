//! Background scheduler for the long-lived gateway.
//!
//! Ticks every `TICK_SECS` seconds and triggers two configurable
//! services on their own cadence:
//!
//! - `DreamService` every `dream.interval_h` hours, using
//!   `DreamConfig` to pick the cheaper analysis model.
//! - `HeartbeatService` every `heartbeat.interval_s` seconds when
//!   `heartbeat.enabled` is true.
//!
//! Last-run timestamps are persisted to
//! `<workspace>/.zunel/scheduler.json` so a process restart doesn't
//! immediately re-fire either service.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use zunel_config::{Config, DreamConfig, HeartbeatConfig};
use zunel_core::{DreamService, MemoryStore};
use zunel_heartbeat::HeartbeatService;
use zunel_providers::LLMProvider;

const TICK_SECS: u64 = 30;
const STATE_FILENAME: &str = "scheduler.json";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SchedulerState {
    last_dream_at: Option<DateTime<Utc>>,
    last_heartbeat_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct GatewayScheduler {
    workspace: PathBuf,
    provider: Arc<dyn LLMProvider>,
    model: String,
    dream_config: DreamConfig,
    heartbeat_config: HeartbeatConfig,
    state: Arc<Mutex<SchedulerState>>,
    state_path: PathBuf,
}

impl GatewayScheduler {
    pub fn from_config(
        cfg: &Config,
        workspace: PathBuf,
        provider: Arc<dyn LLMProvider>,
    ) -> Result<Self> {
        let state_path = state_path(&workspace);
        let state = load_state(&state_path).unwrap_or_default();
        Ok(Self {
            workspace,
            provider,
            model: cfg.agents.defaults.model.clone(),
            dream_config: cfg.agents.defaults.dream.clone(),
            heartbeat_config: cfg.gateway.heartbeat.clone(),
            state: Arc::new(Mutex::new(state)),
            state_path,
        })
    }

    /// Spawn the scheduler loop on the current Tokio runtime. Returns
    /// the join handle so the gateway can `abort()` it on shutdown.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move { self.run_loop().await })
    }

    async fn run_loop(self) {
        let mut ticker = tokio::time::interval(Duration::from_secs(TICK_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.tick_once(Utc::now()).await;
        }
    }

    /// Single scheduler iteration. Public + clocked so tests can
    /// drive it deterministically without spawning a runtime ticker.
    pub async fn tick_once(&self, now: DateTime<Utc>) {
        if let Err(err) = self.maybe_dream(now).await {
            tracing::warn!(error = %err, "scheduler: dream pass failed");
        }
        if let Err(err) = self.maybe_heartbeat(now).await {
            tracing::warn!(error = %err, "scheduler: heartbeat pass failed");
        }
    }

    async fn maybe_dream(&self, now: DateTime<Utc>) -> Result<()> {
        let interval_h = match self.dream_config.interval_h {
            Some(0) | None => return Ok(()),
            Some(h) => h as i64,
        };
        let interval = chrono::Duration::hours(interval_h);
        {
            let state = self.state.lock().await;
            if let Some(last) = state.last_dream_at {
                if now - last < interval {
                    return Ok(());
                }
            }
        }
        tracing::info!("scheduler: running dream pass");
        let store = MemoryStore::new(self.workspace.clone());
        let svc = DreamService::new(store, self.provider.clone(), self.model.clone())
            .with_config(&self.dream_config);
        let _ = svc.run().await; // failures already logged inside DreamService
        let mut state = self.state.lock().await;
        state.last_dream_at = Some(now);
        save_state(&self.state_path, &state)?;
        Ok(())
    }

    async fn maybe_heartbeat(&self, now: DateTime<Utc>) -> Result<()> {
        if !self.heartbeat_config.enabled || self.heartbeat_config.interval_s == 0 {
            return Ok(());
        }
        let interval = chrono::Duration::seconds(self.heartbeat_config.interval_s as i64);
        {
            let state = self.state.lock().await;
            if let Some(last) = state.last_heartbeat_at {
                if now - last < interval {
                    return Ok(());
                }
            }
        }
        tracing::info!("scheduler: running heartbeat pass");
        let svc = HeartbeatService::new(
            self.workspace.clone(),
            self.provider.clone(),
            self.model.clone(),
        )
        .with_config(self.heartbeat_config.clone());
        match svc.trigger_now().await {
            Ok(Some(tasks)) => {
                tracing::info!(?tasks, "scheduler: heartbeat suggested follow-up tasks");
            }
            Ok(None) => {}
            Err(err) => tracing::warn!(error = %err, "scheduler: heartbeat trigger failed"),
        }
        let mut state = self.state.lock().await;
        state.last_heartbeat_at = Some(now);
        save_state(&self.state_path, &state)?;
        Ok(())
    }

    #[cfg(test)]
    async fn snapshot(&self) -> SchedulerState {
        self.state.lock().await.clone()
    }
}

fn state_path(workspace: &Path) -> PathBuf {
    workspace.join(".zunel").join(STATE_FILENAME)
}

fn load_state(path: &Path) -> Option<SchedulerState> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_state(path: &Path, state: &SchedulerState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating scheduler state dir {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(state)?;
    std::fs::write(path, body)
        .with_context(|| format!("writing scheduler state {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zunel_providers::{
        ChatMessage, GenerationSettings, LLMResponse, StreamEvent, ToolSchema, Usage,
    };

    struct FakeProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LLMProvider for FakeProvider {
        async fn generate(
            &self,
            _model: &str,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _settings: &GenerationSettings,
        ) -> zunel_providers::Result<LLMResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(LLMResponse {
                content: Some("ok".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: None,
            })
        }

        fn generate_stream<'a>(
            &'a self,
            _model: &'a str,
            _messages: &'a [ChatMessage],
            _tools: &'a [ToolSchema],
            _settings: &'a GenerationSettings,
        ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
            Box::pin(futures::stream::empty())
        }
    }

    fn cfg(dream_h: u32, heartbeat_s: u64) -> Config {
        let mut cfg = Config::default();
        cfg.agents.defaults.model = "gpt-x".into();
        cfg.agents.defaults.dream = DreamConfig {
            interval_h: Some(dream_h),
            ..DreamConfig::default()
        };
        cfg.gateway.heartbeat = HeartbeatConfig {
            enabled: true,
            interval_s: heartbeat_s,
            keep_recent_messages: 8,
        };
        cfg
    }

    #[tokio::test]
    async fn skips_when_under_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider {
            calls: calls.clone(),
        });
        let scheduler =
            GatewayScheduler::from_config(&cfg(2, 1800), tmp.path().to_path_buf(), provider)
                .unwrap();
        let now = Utc::now();
        scheduler.tick_once(now).await;
        let after_first = scheduler.snapshot().await;
        assert!(after_first.last_dream_at.is_some());
        assert!(after_first.last_heartbeat_at.is_some());
        // Second tick a few seconds later — both intervals not elapsed.
        scheduler
            .tick_once(now + chrono::Duration::seconds(5))
            .await;
        let after_second = scheduler.snapshot().await;
        assert_eq!(
            after_first.last_dream_at, after_second.last_dream_at,
            "dream should not refire under interval"
        );
        assert_eq!(
            after_first.last_heartbeat_at, after_second.last_heartbeat_at,
            "heartbeat should not refire under interval"
        );
    }

    #[tokio::test]
    async fn fires_after_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider {
            calls: calls.clone(),
        });
        let scheduler =
            GatewayScheduler::from_config(&cfg(1, 30), tmp.path().to_path_buf(), provider).unwrap();
        let now = Utc::now();
        scheduler.tick_once(now).await;
        let first = scheduler.snapshot().await;
        scheduler.tick_once(now + chrono::Duration::hours(2)).await;
        let second = scheduler.snapshot().await;
        assert_ne!(
            first.last_dream_at, second.last_dream_at,
            "dream should refire after interval"
        );
        assert_ne!(
            first.last_heartbeat_at, second.last_heartbeat_at,
            "heartbeat should refire after interval"
        );
    }
}
