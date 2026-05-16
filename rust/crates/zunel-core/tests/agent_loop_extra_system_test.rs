//! Tests for `AgentLoop::with_extra_system_message` — Mode 2's
//! per-call operator persona override. Two opposed cases:
//!
//! * extra message set → it appears as the first system message in
//!   the stream the provider sees, *before* any skills system
//!   message would be (verified with no skills loader configured).
//! * extra message must not be persisted into the session log.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::sync::mpsc;
use zunel_config::AgentDefaults;
use zunel_core::{AgentLoop, SessionManager};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, Role, StreamEvent, ToolSchema, Usage,
};

struct CapturingProvider {
    captured_messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

#[async_trait]
impl LLMProvider for CapturingProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        unreachable!("streaming path only in this test")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        self.captured_messages
            .lock()
            .unwrap()
            .push(messages.to_vec());
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::ContentDelta("ok".into()));
            yield Ok(StreamEvent::Done(LLMResponse {
                content: Some("ok".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: None,
            }));
        })
    }
}

fn make_loop_with_extra(
    extra: Option<&str>,
) -> (
    AgentLoop,
    Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let workspace: PathBuf = tmp.path().to_path_buf();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(CapturingProvider {
        captured_messages: captured.clone(),
    });
    let manager = SessionManager::new(&workspace);
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        max_tool_iterations: Some(1),
        ..Default::default()
    };
    let mut agent =
        AgentLoop::with_sessions(provider, defaults, manager).with_workspace(workspace.clone());
    if let Some(s) = extra {
        agent = agent.with_extra_system_message(Some(s.to_string()));
    }
    (agent, captured, tmp)
}

#[tokio::test]
async fn extra_system_message_prepends_at_index_zero() {
    let (loop_, captured, _tmp) = make_loop_with_extra(Some("You are a research helper."));
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("test:extra", "hello", tx)
        .await
        .unwrap();
    drain.abort();

    let history = captured.lock().unwrap();
    let first_turn = history.first().expect("provider was called");
    assert_eq!(first_turn[0].role, Role::System);
    assert!(
        first_turn[0].content.contains("You are a research helper."),
        "expected extra system message at index 0, got {:?}",
        first_turn[0].content
    );
}

#[tokio::test]
async fn no_extra_system_message_means_no_system_message_at_all() {
    let (loop_, captured, _tmp) = make_loop_with_extra(None);
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("test:noextra", "hello", tx)
        .await
        .unwrap();
    drain.abort();

    let history = captured.lock().unwrap();
    let first_turn = history.first().expect("provider was called");
    // Without any system messages the very first message is the
    // user turn we just sent.
    assert_eq!(first_turn[0].role, Role::User);
}

#[tokio::test]
async fn extra_system_message_is_not_persisted_to_session() {
    let (loop_, _captured, tmp) = make_loop_with_extra(Some("ephemeral persona"));
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("persist:session", "hello", tx)
        .await
        .unwrap();
    drain.abort();

    let session = SessionManager::new(tmp.path())
        .load("persist:session")
        .expect("load")
        .expect("session exists");
    for msg in session.messages() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
        assert!(
            !(role == "system" && content.contains("ephemeral persona")),
            "extra system message must not land in persisted session"
        );
    }
}

#[tokio::test]
async fn empty_extra_system_message_is_normalised_to_none() {
    let (loop_, captured, _tmp) = make_loop_with_extra(Some(""));
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("test:empty", "hello", tx)
        .await
        .unwrap();
    drain.abort();

    let history = captured.lock().unwrap();
    let first_turn = history.first().expect("provider was called");
    // No system message should be injected for the empty string.
    assert_eq!(first_turn[0].role, Role::User);
}
