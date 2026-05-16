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
use zunel_core::{AgentLoop, MemoryStore, Session, SessionManager};
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

/// Stage 1 wire-up: when AgentLoop has a MemoryStore attached and an
/// idle compaction fires, the produced summary must be appended to
/// `<workspace>/memory/history.jsonl` so the gateway's Dream service
/// has fresh input. Without this, Dream silently no-ops on every tick.
#[tokio::test]
async fn idle_compaction_appends_summary_to_memory_history() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    let manager = SessionManager::new(&workspace);
    let mut session = Session::new("slack:DSTAGE1");
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
        summary: "compaction summary body for stage 1".into(),
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
    let loop_ = AgentLoop::with_sessions(provider, defaults, manager.clone())
        .with_workspace(workspace.clone())
        .with_memory_store(MemoryStore::new(workspace.clone()));

    let (tx, _rx) = mpsc::channel(8);
    loop_
        .process_streamed("slack:DSTAGE1", "still there?", tx)
        .await
        .expect("turn ok");

    let history_path = workspace.join("memory").join("history.jsonl");
    assert!(
        history_path.exists(),
        "Stage 1 should have created memory/history.jsonl",
    );
    let raw = std::fs::read_to_string(&history_path).unwrap();
    let entries: Vec<Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(entries.len(), 1, "exactly one Stage 1 entry");
    let content = entries[0]
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        content.contains("compaction summary body for stage 1"),
        "history entry should hold the LLM summary body, got: {content}",
    );

    let cursor_path = workspace.join("memory").join(".cursor");
    let cursor: u64 = std::fs::read_to_string(&cursor_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        cursor >= 1,
        "cursor should have advanced past the new entry"
    );
}

/// Without a memory store, idle compaction must still succeed (no
/// regression for callers that opt out of Stage 1) but
/// `memory/history.jsonl` should not be created.
#[tokio::test]
async fn idle_compaction_without_memory_store_does_not_create_history() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    let manager = SessionManager::new(&workspace);
    let mut session = Session::new("slack:DNOSTAGE1");
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
        summary: "no-stage-1 summary".into(),
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
    let loop_ = AgentLoop::with_sessions(provider, defaults, manager.clone())
        .with_workspace(workspace.clone());

    let (tx, _rx) = mpsc::channel(8);
    loop_
        .process_streamed("slack:DNOSTAGE1", "ping", tx)
        .await
        .expect("turn ok");

    let history_path = workspace.join("memory").join("history.jsonl");
    assert!(
        !history_path.exists(),
        "no memory store ⇒ no history.jsonl created",
    );
}
