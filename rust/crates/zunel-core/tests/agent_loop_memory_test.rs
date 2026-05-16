//! `AgentLoop::build_memory_system_message` ‐ when a `MemoryStore` is
//! attached, every turn must inject `USER.md` / `SOUL.md` /
//! `MEMORY.md` into the system message stack so Dream's consolidated
//! knowledge actually reaches the model. Without this the
//! Memory→Dream→prompt loop is silently broken.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::mpsc;
use zunel_config::AgentDefaults;
use zunel_core::{AgentLoop, MemoryStore, SessionManager};
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

fn make_loop(
    workspace: &Path,
    attach_memory: bool,
) -> (AgentLoop, Arc<Mutex<Vec<Vec<ChatMessage>>>>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(CapturingProvider {
        captured_messages: captured.clone(),
    });
    let manager = SessionManager::new(workspace);
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        max_tool_iterations: Some(1),
        ..Default::default()
    };
    let mut agent = AgentLoop::with_sessions(provider, defaults, manager)
        .with_workspace(workspace.to_path_buf());
    if attach_memory {
        agent = agent.with_memory_store(MemoryStore::new(workspace.to_path_buf()));
    }
    (agent, captured)
}

#[tokio::test]
async fn memory_files_are_stacked_into_system_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    let store = MemoryStore::new(workspace.clone());
    store.write_user("- prefers concise answers").unwrap();
    store
        .write_soul("You are a careful, deliberate assistant.")
        .unwrap();
    store
        .write_memory("Last week the user shipped feature X with mixed reviews.")
        .unwrap();

    let (loop_, captured) = make_loop(&workspace, true);
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_.process_streamed("test:mem", "hi", tx).await.unwrap();
    drain.abort();

    let history = captured.lock().unwrap();
    let first_turn = history.first().expect("provider was called");
    let memory_system = first_turn
        .iter()
        .find(|m| matches!(m.role, Role::System) && m.content.starts_with("# Workspace Memory"))
        .expect("workspace-memory system block must be injected");
    assert!(memory_system.content.contains("prefers concise answers"));
    assert!(memory_system.content.contains("careful, deliberate"));
    assert!(memory_system.content.contains("feature X"));
    assert!(memory_system.content.contains("## USER.md"));
    assert!(memory_system.content.contains("## SOUL.md"));
    assert!(memory_system.content.contains("## MEMORY.md"));
}

#[tokio::test]
async fn no_memory_store_means_no_workspace_memory_block() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    // Write files but DON'T attach the store — they must not leak in.
    std::fs::write(workspace.join("USER.md"), "secret note").unwrap();

    let (loop_, captured) = make_loop(&workspace, false);
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("test:nomem", "hi", tx)
        .await
        .unwrap();
    drain.abort();

    let history = captured.lock().unwrap();
    let first_turn = history.first().expect("provider was called");
    for msg in first_turn {
        assert!(
            !msg.content.contains("# Workspace Memory"),
            "without memory store, no bootstrap system block should appear",
        );
        assert!(!msg.content.contains("secret note"));
    }
}

#[tokio::test]
async fn empty_memory_files_emit_no_block() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    // Memory store attached but all files empty.
    let (loop_, captured) = make_loop(&workspace, true);
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("test:empty-mem", "hi", tx)
        .await
        .unwrap();
    drain.abort();

    let history = captured.lock().unwrap();
    let first_turn = history.first().expect("provider was called");
    for msg in first_turn {
        assert!(
            !msg.content.contains("# Workspace Memory"),
            "all empty memory files ⇒ skip the bootstrap block entirely",
        );
    }
}

#[tokio::test]
async fn oversized_memory_file_is_truncated() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    let store = MemoryStore::new(workspace.clone());
    // 30 KB MEMORY.md — should be capped at 10 KB.
    let big = "x".repeat(30 * 1024);
    store.write_memory(&big).unwrap();

    let (loop_, captured) = make_loop(&workspace, true);
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("test:big-mem", "hi", tx)
        .await
        .unwrap();
    drain.abort();

    let history = captured.lock().unwrap();
    let first_turn = history.first().expect("provider was called");
    let memory_system = first_turn
        .iter()
        .find(|m| matches!(m.role, Role::System) && m.content.starts_with("# Workspace Memory"))
        .expect("memory block expected");
    assert!(
        memory_system.content.contains("…(truncated;"),
        "oversized file should include the truncation marker",
    );
    assert!(
        memory_system.content.len() < 30 * 1024,
        "block should not carry the entire 30 KB body",
    );
}
