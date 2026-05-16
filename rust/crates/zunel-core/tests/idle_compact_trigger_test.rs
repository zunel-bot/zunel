//! When `agents.defaults.idle_compact_after_minutes` is set and the
//! session's most recent user turn is older than the threshold,
//! `process_streamed` should compact the stale history before sending
//! the next request to the provider.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use zunel_config::AgentDefaults;
use zunel_core::{AgentLoop, Session, SessionManager};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, Role, StreamEvent, ToolSchema, Usage,
};

/// Provider that returns a canned summary on the non-streaming path
/// (compaction) and a one-line reply on the streaming path
/// (subsequent agent turn). Records each streaming call so the test
/// can assert it received a compacted, not bloated, history.
struct DualProvider {
    summary: String,
    streamed_messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

#[async_trait]
impl LLMProvider for DualProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        Ok(LLMResponse {
            content: Some(self.summary.clone()),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: None,
        })
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        self.streamed_messages
            .lock()
            .unwrap()
            .push(messages.to_vec());
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::ContentDelta("ack".into()));
            yield Ok(StreamEvent::Done(LLMResponse {
                content: Some("ack".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: None,
            }));
        })
    }
}

#[tokio::test]
async fn idle_compaction_collapses_history_before_next_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(tmp.path());
    let mut session = Session::new("slack:DTEST");
    let stale_ts = (chrono::Local::now() - chrono::Duration::hours(2))
        .naive_local()
        .format("%Y-%m-%dT%H:%M:%S%.6f")
        .to_string();
    for i in 0..30 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        session.append_raw_message(json!({
            "role": role,
            "content": format!("stale msg #{i}"),
            "timestamp": stale_ts.clone(),
        }));
    }
    manager.save(&session).unwrap();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(DualProvider {
        summary: "user/assistant exchanged 30 stale msgs about feature X".into(),
        streamed_messages: captured.clone(),
    });
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        idle_compact_after_minutes: Some(30),
        compaction_keep_tail: Some(4),
        session_history_window: Some(40),
        ..Default::default()
    };
    let loop_ = AgentLoop::with_sessions(provider, defaults, manager.clone());

    let (tx, _rx) = mpsc::channel(8);
    loop_
        .process_streamed("slack:DTEST", "still there?", tx)
        .await
        .expect("turn ok");

    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 1, "exactly one streaming call");
    let sent = &calls[0];
    let summary_count = sent
        .iter()
        .filter(|m| {
            matches!(m.role, Role::System) && m.content.starts_with("[Prior conversation summary]")
        })
        .count();
    assert_eq!(summary_count, 1, "compaction should inject one summary row");
    assert!(
        sent.len() <= 8,
        "expected compacted+tail to be ~6 messages, got {} ({:?})",
        sent.len(),
        sent.iter().map(|m| &m.content).collect::<Vec<_>>()
    );

    let saved = manager.load("slack:DTEST").unwrap().unwrap();
    let saved_summary: usize = saved
        .messages()
        .iter()
        .filter(|m: &&Value| {
            m.get("role").and_then(Value::as_str) == Some("system")
                && m.get("content")
                    .and_then(Value::as_str)
                    .map(|s| s.starts_with("[Prior conversation summary]"))
                    .unwrap_or(false)
        })
        .count();
    assert_eq!(saved_summary, 1, "summary row persisted to disk");
    assert_eq!(
        saved.last_consolidated(),
        0,
        "summary row sits at the start of replayable history",
    );
}

#[tokio::test]
async fn idle_compaction_skips_when_session_recent() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(tmp.path());
    let mut session = Session::new("slack:DRECENT");
    for i in 0..10 {
        let role = if i % 2 == 0 {
            zunel_core::ChatRole::User
        } else {
            zunel_core::ChatRole::Assistant
        };
        session.add_message(role, format!("recent msg #{i}"));
    }
    manager.save(&session).unwrap();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(DualProvider {
        summary: "should not be called".into(),
        streamed_messages: captured.clone(),
    });
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        idle_compact_after_minutes: Some(60),
        ..Default::default()
    };
    let loop_ = AgentLoop::with_sessions(provider, defaults, manager.clone());
    let (tx, _rx) = mpsc::channel(8);
    loop_
        .process_streamed("slack:DRECENT", "ping", tx)
        .await
        .expect("turn ok");

    let calls = captured.lock().unwrap();
    let summary_count = calls[0]
        .iter()
        .filter(|m| {
            matches!(m.role, Role::System) && m.content.starts_with("[Prior conversation summary]")
        })
        .count();
    assert_eq!(summary_count, 0, "no summary injected for recent session");
}
