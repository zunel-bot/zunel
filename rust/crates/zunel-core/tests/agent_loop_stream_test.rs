use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::json;
use tokio::sync::mpsc;
use zunel_config::AgentDefaults;
use zunel_core::{AgentLoop, Session, SessionManager};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, Role, StreamEvent, ToolSchema, Usage,
};

struct StreamingFake {
    chunks: Vec<String>,
    captured_messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

struct AlwaysToolProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LLMProvider for AlwaysToolProvider {
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
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("missing_tool".into()),
                arguments_fragment: Some("{}".into()),
            });
            yield Ok(StreamEvent::Done(LLMResponse {
                content: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: Some("tool_calls".into()),
            }));
        })
    }
}

#[async_trait]
impl LLMProvider for StreamingFake {
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
        let chunks = self.chunks.clone();
        Box::pin(async_stream::stream! {
            let mut full = String::new();
            for c in &chunks {
                full.push_str(c);
                yield Ok(StreamEvent::ContentDelta(c.clone()));
            }
            yield Ok(StreamEvent::Done(LLMResponse {
                content: Some(full),
                tool_calls: Vec::new(),
                usage: Usage { prompt_tokens: 2, completion_tokens: 3, cached_tokens: 0, reasoning_tokens: 0 },
                finish_reason: None,
            }));
        })
    }
}

fn make_loop(tmp: &tempfile::TempDir) -> (AgentLoop, Arc<Mutex<Vec<Vec<ChatMessage>>>>) {
    let workspace: PathBuf = tmp.path().to_path_buf();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(StreamingFake {
        chunks: vec!["hel".into(), "lo".into()],
        captured_messages: captured.clone(),
    });
    let manager = SessionManager::new(&workspace);
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        ..Default::default()
    };
    let l = AgentLoop::with_sessions(provider, defaults, manager);
    (l, captured)
}

#[tokio::test]
async fn process_streamed_emits_deltas_and_persists_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (loop_, _) = make_loop(&tmp);
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(16);

    let handle = tokio::spawn(async move {
        let mut deltas = Vec::new();
        while let Some(event) = rx.recv().await {
            if let StreamEvent::ContentDelta(s) = event {
                deltas.push(s);
            }
        }
        deltas
    });

    let result = loop_
        .process_streamed("cli:direct", "hi", tx)
        .await
        .unwrap();
    assert_eq!(result.content, "hello");

    let deltas = handle.await.unwrap();
    assert_eq!(deltas, vec!["hel", "lo"]);

    // Session must now exist on disk with user + assistant messages.
    let manager = SessionManager::new(tmp.path());
    let session = manager.load("cli:direct").unwrap().expect("saved");
    assert_eq!(session.messages().len(), 2);
    assert_eq!(session.messages()[0]["role"].as_str(), Some("user"));
    assert_eq!(session.messages()[0]["content"].as_str(), Some("hi"));
    assert_eq!(session.messages()[1]["role"].as_str(), Some("assistant"));
    assert_eq!(session.messages()[1]["content"].as_str(), Some("hello"));
}

#[tokio::test]
async fn process_streamed_feeds_history_to_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let (loop_, captured) = make_loop(&tmp);

    // First turn seeds history.
    let (tx1, _rx1) = mpsc::channel::<StreamEvent>(16);
    loop_
        .process_streamed("cli:direct", "ping", tx1)
        .await
        .unwrap();

    let (tx2, _rx2) = mpsc::channel::<StreamEvent>(16);
    loop_
        .process_streamed("cli:direct", "again", tx2)
        .await
        .unwrap();

    let calls = captured.lock().unwrap();
    // Second call should see prior user + assistant + new user message.
    assert!(calls.len() >= 2);
    let second = &calls[1];
    assert!(second.len() >= 3, "expected ≥3 messages, got {second:?}");
    assert_eq!(second.last().unwrap().content, "again");
}

#[tokio::test]
async fn process_streamed_preserves_persisted_assistant_tool_call_history() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(tmp.path());
    let mut session = Session::new("cli:direct");
    session.add_message(zunel_core::ChatRole::User, "read file");
    session.append_raw_message(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "read_file",
                "arguments": "{\"path\":\"README.md\"}"
            }
        }]
    }));
    session.append_raw_message(json!({
        "role": "tool",
        "tool_call_id": "call_1",
        "name": "read_file",
        "content": "file contents"
    }));
    manager.save(&session).unwrap();

    let (loop_, captured) = make_loop(&tmp);
    let (tx, _rx) = mpsc::channel::<StreamEvent>(16);
    loop_
        .process_streamed("cli:direct", "continue", tx)
        .await
        .unwrap();

    let calls = captured.lock().unwrap();
    let first = &calls[0];
    assert!(first.iter().any(|message| {
        message.role == Role::Assistant
            && message.tool_calls.len() == 1
            && message.tool_calls[0].name == "read_file"
    }));
    assert!(first
        .iter()
        .any(|message| message.role == Role::Tool
            && message.tool_call_id.as_deref() == Some("call_1")));
}

#[tokio::test]
async fn process_streamed_respects_configured_max_tool_iterations() {
    let tmp = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn LLMProvider> = Arc::new(AlwaysToolProvider {
        calls: calls.clone(),
    });
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        max_tool_iterations: Some(2),
        ..Default::default()
    };
    let loop_ = AgentLoop::with_sessions(provider, defaults, SessionManager::new(tmp.path()));
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    loop_
        .process_streamed("cli:direct", "loop", tx)
        .await
        .unwrap();
    drain.abort();

    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// A provider that yields a small number of content deltas with a sleep
/// between each, so a test can race a `CancellationToken` against the
/// stream and reliably hit the mid-stream cancel checkpoint.
struct SlowStreamingProvider;

#[async_trait]
impl LLMProvider for SlowStreamingProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        unreachable!("streaming path only")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        Box::pin(async_stream::stream! {
            for piece in ["chunk-1 ", "chunk-2 ", "chunk-3 "] {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                yield Ok(StreamEvent::ContentDelta(piece.into()));
            }
            yield Ok(StreamEvent::Done(LLMResponse {
                content: Some("chunk-1 chunk-2 chunk-3 ".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: Some("stop".into()),
            }));
        })
    }
}

#[tokio::test]
async fn process_streamed_with_cancel_aborts_mid_stream() {
    // Regression guard for the REPL Ctrl+C wiring: when the per-call
    // CancellationToken fires mid-stream, the agent loop must surface
    // `Error::Cancelled` instead of running the turn to completion.
    use tokio_util::sync::CancellationToken;
    use zunel_core::Error as CoreError;

    let tmp = tempfile::tempdir().unwrap();
    let workspace: PathBuf = tmp.path().to_path_buf();
    let provider: Arc<dyn LLMProvider> = Arc::new(SlowStreamingProvider);
    let manager = SessionManager::new(&workspace);
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        ..Default::default()
    };
    let loop_ = AgentLoop::with_sessions(provider, defaults, manager);

    let cancel = CancellationToken::new();
    let cancel_for_fire = cancel.clone();
    tokio::spawn(async move {
        // Let the first chunk start arriving, then cancel.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        cancel_for_fire.cancel();
    });

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let err = loop_
        .process_streamed_with_cancel("cli:direct", "go", tx, cancel)
        .await
        .expect_err("cancel mid-stream must surface as an error");
    drain.abort();

    match err {
        CoreError::Cancelled => {}
        other => panic!("expected Error::Cancelled, got {other:?}"),
    }
}

#[tokio::test]
async fn process_streamed_with_cancel_completes_when_token_never_fires() {
    // Symmetric guard: a fresh, never-cancelled token must NOT interrupt
    // a normal turn — otherwise the REPL's per-turn token would block
    // every interaction.
    use tokio_util::sync::CancellationToken;

    let tmp = tempfile::tempdir().unwrap();
    let workspace: PathBuf = tmp.path().to_path_buf();
    let provider: Arc<dyn LLMProvider> = Arc::new(StreamingFake {
        chunks: vec!["hel".into(), "lo".into()],
        captured_messages: Arc::new(Mutex::new(Vec::new())),
    });
    let manager = SessionManager::new(&workspace);
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        ..Default::default()
    };
    let loop_ = AgentLoop::with_sessions(provider, defaults, manager);

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = loop_
        .process_streamed_with_cancel("cli:direct", "hi", tx, CancellationToken::new())
        .await
        .expect("uncancelled turn must complete");
    drain.abort();
    assert_eq!(result.content, "hello");
}
