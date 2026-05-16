//! End-to-end Dream pipeline test.
//!
//! Walks the whole loop in one shot:
//!
//! 1. A session is large enough + stale enough to trigger idle
//!    compaction.
//! 2. `AgentLoop::process_streamed` runs a turn, which compacts the
//!    stale head AND (via the Stage 1 wire) appends the resulting
//!    summary to `memory/history.jsonl`.
//! 3. A fresh `DreamService::run` invocation reads that
//!    `history.jsonl`, performs its two-phase pass against a stubbed
//!    provider, and edits `MEMORY.md` via `write_file`.
//! 4. A second `process_streamed` turn picks up the freshly-edited
//!    `MEMORY.md` via the workspace-bootstrap system message.
//!
//! This is the single highest-leverage regression guard for the whole
//! Memory→Dream→prompt loop — without it, any one of the three
//! independent wire-ups can break silently and the agent loses
//! durable memory until someone notices `MEMORY.md` isn't growing.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use zunel_config::{AgentDefaults, DreamConfig};
use zunel_core::{AgentLoop, DreamService, MemoryStore, Session, SessionManager};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, Role, StreamEvent, ToolSchema, Usage,
};

/// Three-mode provider:
///
/// * `generate()` returns the compaction summary (called during the
///   idle-compaction path) AND the Dream phase-1 analysis (called
///   from `DreamService::run`). We hand back a long-enough analysis
///   string to clear the 32-byte phase-1 guard.
/// * `generate_stream()` first returns a `write_file` tool call
///   targeting `memory/MEMORY.md` (the Dream phase-2 edit), then
///   yields plain text on subsequent calls (the agent turns after
///   the compaction has already happened).
struct ScriptedProvider {
    compaction_summary: String,
    dream_analysis: String,
    write_target_path: String,
    write_target_content: String,
    streamed_messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    /// Counts how many Dream-phase-2 stream calls we've seen so the
    /// first one emits the `write_file` tool call and subsequent
    /// ones (after the tool result lands) emit a clean text stop.
    dream_phase2_calls: Mutex<u32>,
}

#[async_trait]
impl LLMProvider for ScriptedProvider {
    async fn generate(
        &self,
        _model: &str,
        messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        // The compaction prompt looks like "Summarize the following…",
        // the Dream phase-1 prompt mentions "Conversation History".
        // Use that to decide which canned answer to return.
        let last_user = messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::User))
            .map(|m| m.content.as_str())
            .unwrap_or_default();
        let body = if last_user.contains("Summarize the following") {
            self.compaction_summary.clone()
        } else {
            self.dream_analysis.clone()
        };
        Ok(LLMResponse {
            content: Some(body),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: Some("stop".into()),
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
        // Phase-2 routing: Dream's phase 2 message stack includes the
        // literal "## Analysis Result" line from memory.rs. First
        // phase-2 call emits the write_file tool call; subsequent
        // ones (after the tool result lands) emit a clean text stop.
        let is_dream_phase2 = messages
            .iter()
            .any(|m| m.content.contains("## Analysis Result"));
        let phase2_call = if is_dream_phase2 {
            let mut guard = self.dream_phase2_calls.lock().unwrap();
            let prev = *guard;
            *guard += 1;
            Some(prev)
        } else {
            None
        };
        let write_args = json!({
            "path": self.write_target_path,
            "content": self.write_target_content,
        })
        .to_string();
        Box::pin(async_stream::stream! {
            if phase2_call == Some(0) {
                yield Ok(StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-write".into()),
                    name: Some("write_file".into()),
                    arguments_fragment: Some(write_args),
                });
                yield Ok(StreamEvent::Done(LLMResponse {
                    content: None,
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    finish_reason: Some("tool_calls".into()),
                }));
            } else {
                yield Ok(StreamEvent::ContentDelta("ack".into()));
                yield Ok(StreamEvent::Done(LLMResponse {
                    content: Some("ack".into()),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    finish_reason: Some("stop".into()),
                }));
            }
        })
    }
}

#[tokio::test]
async fn dream_loop_end_to_end_writes_memory_md_and_reaches_next_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    let manager = SessionManager::new(&workspace);

    // Seed a session whose last user turn is 2h old — well past
    // the 30-minute idle threshold the loop will be configured with.
    let mut session = Session::new("slack:DE2E");
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
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider {
        compaction_summary:
            "Earlier the user explored migrating the API to async/await and committed to it.".into(),
        dream_analysis:
            "Add to MEMORY.md: user committed to async/await migration for the API in 2026-05."
                .into(),
        write_target_path: "memory/MEMORY.md".into(),
        write_target_content:
            "# Memory\n\n- User committed to async/await migration for the API in 2026-05.\n".into(),
        streamed_messages: captured.clone(),
        dream_phase2_calls: Mutex::new(0),
    });

    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        idle_compact_after_minutes: Some(30),
        compaction_keep_tail: Some(4),
        session_history_window: Some(40),
        max_tool_iterations: Some(3),
        ..Default::default()
    };
    let loop_ = AgentLoop::with_sessions(provider.clone(), defaults.clone(), manager.clone())
        .with_workspace(workspace.clone())
        .with_memory_store(MemoryStore::new(workspace.clone()));

    // Turn 1: drives idle compaction → Stage 1 append.
    let (tx, _rx) = mpsc::channel(8);
    loop_
        .process_streamed("slack:DE2E", "still there?", tx)
        .await
        .expect("turn 1 ok");

    // Stage 1 must have written exactly one history entry holding
    // the compaction summary body.
    let history_path = workspace.join("memory").join("history.jsonl");
    let raw = std::fs::read_to_string(&history_path).expect("history.jsonl created");
    let entries: Vec<Value> = raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(entries.len(), 1, "exactly one Stage 1 entry");
    assert!(
        entries[0]["content"]
            .as_str()
            .unwrap()
            .contains("async/await"),
        "entry must carry the compaction summary: {entries:?}"
    );

    // Now drive Dream against the same workspace. Dream reads
    // history.jsonl, runs phase 1 (analysis) and phase 2 (edit
    // tools), and should write MEMORY.md.
    let dream = DreamService::new(
        MemoryStore::new(workspace.clone()),
        provider.clone(),
        defaults.model.clone(),
    )
    .with_config(&DreamConfig {
        // Disable interval-gated behaviour (we drive it directly).
        interval_h: Some(0),
        max_batch_size: Some(20),
        max_iterations: Some(5),
        ..DreamConfig::default()
    });
    let outcome = dream.run().await.expect("dream ok");
    assert!(
        outcome.processed_entries >= 1,
        "Dream should consume the Stage 1 entry, got: {outcome:?}"
    );
    assert!(
        outcome.is_active(),
        "Dream should mark active when phase 2 wrote a file, got: {outcome:?}"
    );
    assert!(
        outcome
            .edited_files
            .iter()
            .any(|t| t == "write_file" || t == "edit_file"),
        "edited_files should report the tool name: {outcome:?}"
    );

    // MEMORY.md must now hold the durable note Dream extracted.
    let memory_md = std::fs::read_to_string(workspace.join("memory").join("MEMORY.md"))
        .expect("MEMORY.md exists after Dream");
    assert!(
        memory_md.contains("async/await migration"),
        "MEMORY.md must carry Dream's edit, got: {memory_md:?}"
    );

    // Turn 2: another agent turn against the same loop. The
    // workspace-bootstrap system message must now include the
    // freshly-edited MEMORY.md — that's the wire that closes the
    // Memory→Dream→prompt loop.
    captured.lock().unwrap().clear();
    let (tx2, _rx2) = mpsc::channel(8);
    loop_
        .process_streamed("slack:DE2E", "any updates?", tx2)
        .await
        .expect("turn 2 ok");

    let turn2 = captured.lock().unwrap();
    let last_call = turn2.last().expect("turn 2 streamed");
    let bootstrap_msg = last_call
        .iter()
        .find(|m| matches!(m.role, Role::System) && m.content.starts_with("# Workspace Memory"))
        .expect("workspace-memory system block must be present in turn 2");
    assert!(
        bootstrap_msg.content.contains("async/await migration"),
        "Dream's MEMORY.md edit must reach the next turn's prompt, got: {}",
        bootstrap_msg.content
    );
}
