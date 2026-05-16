//! Verifies the v0.2.8 `with_skills(loader)` plumbing on
//! [`AgentLoop`]. Two opposed cases:
//!
//! * loader present + at least one always-on skill → `process_streamed`
//!   prepends a single `system` message with the skill body.
//! * no `with_skills(...)` call → no system message is sent (preserves
//!   pre-v0.2.8 behavior, important so existing snapshot fixtures
//!   keep matching).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::mpsc;
use zunel_config::AgentDefaults;
use zunel_core::{AgentLoop, SessionManager};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, Role, StreamEvent, ToolSchema, Usage,
};
use zunel_skills::SkillsLoader;

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

fn write_always_skill(workspace: &std::path::Path, name: &str, marker: &str) {
    let dir = workspace.join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    // Raw string to preserve the YAML indentation that
    // `metadata.zunel.always` requires — the `\<newline>` form would
    // collapse all leading whitespace on each line and break the
    // nesting silently.
    let body = format!(
        r#"---
description: {name} test skill
metadata:
  zunel:
    always: true
---

# {name}

{marker}
"#
    );
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
}

fn make_loop_without_skills() -> (
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
        ..Default::default()
    };
    (
        AgentLoop::with_sessions(provider, defaults, manager),
        captured,
        tmp,
    )
}

#[tokio::test]
async fn process_streamed_prepends_skills_system_message_when_loader_configured() {
    let (loop_, captured, tmp) = make_loop_without_skills();
    write_always_skill(tmp.path(), "test_marker_skill", "ZUNEL_SKILL_MARKER_42");
    // SkillsLoader::new with no filesystem-builtin override — bundled
    // builtins (`mcp-oauth-login`) still load via `include_dir!`,
    // which is fine for this test because we're asserting on the
    // injected-skill marker, not the absence of other skills.
    let loader = SkillsLoader::new(tmp.path(), None, &[]);
    let loop_ = loop_.with_skills(loader);

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("session:test", "hi", tx)
        .await
        .unwrap();
    drain.abort();

    let calls = captured.lock().unwrap();
    assert!(!calls.is_empty(), "provider must be called at least once");
    let initial = &calls[0];
    let system_msg = initial
        .iter()
        .find(|m| m.role == Role::System)
        .expect("expected a skills system message at the head of initial_messages");
    // First message must be the system message, not in the middle.
    assert!(matches!(initial[0].role, Role::System));
    assert!(
        system_msg.content.contains("ZUNEL_SKILL_MARKER_42"),
        "system message should contain the always-on skill body, got: {}",
        system_msg.content
    );
    assert!(
        system_msg.content.contains("# Active Skills")
            || system_msg.content.contains("test_marker_skill"),
        "system message should be the skills section, got: {}",
        system_msg.content
    );
}

#[tokio::test]
async fn process_streamed_does_not_inject_system_message_without_loader() {
    let (loop_, captured, _tmp) = make_loop_without_skills();
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("session:test", "hi", tx)
        .await
        .unwrap();
    drain.abort();

    let calls = captured.lock().unwrap();
    let initial = &calls[0];
    assert!(
        !initial.iter().any(|m| m.role == Role::System),
        "expected no system message when with_skills(...) wasn't called, got: {initial:?}"
    );
}

/// When the loader resolves no always-on skills *and* the on-demand
/// summary is empty, `build_skills_system_message` returns `None` and
/// the agent loop should run with no system message — same pre-v0.2.8
/// behavior. Important for users who deliberately ship without skills
/// and don't want a 200-token tax per turn.
#[tokio::test]
async fn process_streamed_skips_system_message_when_loader_empty() {
    let (loop_, captured, tmp) = make_loop_without_skills();
    // Empty workspace (no skills/) + disable the bundled builtins so
    // both the always-blob and the summary collapse to "". Keep this
    // list in sync with `crates/zunel-skills/builtins/`.
    let disabled = vec!["mcp-oauth-login".to_string(), "gitlab-mr-write".to_string()];
    let loader = SkillsLoader::new(tmp.path(), None, &disabled);
    let loop_ = loop_.with_skills(loader);

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    loop_
        .process_streamed("session:test", "hi", tx)
        .await
        .unwrap();
    drain.abort();

    let calls = captured.lock().unwrap();
    let initial = &calls[0];
    assert!(
        !initial.iter().any(|m| m.role == Role::System),
        "expected no system message when loader has no always-on skills and empty summary"
    );
}
