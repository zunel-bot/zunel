//! Native `zunel_dream_run` tool — the in-process counterpart of the
//! `/dream` slash command. Lets the agent (or a Slack user via the
//! agent) trigger a Dream consolidation pass without waiting for the
//! scheduler tick. Registered into the live tool registry by both
//! `zunel agent` and `zunel gateway`, so it works in CLI and Slack
//! contexts equivalently.
//!
//! This tool intentionally lives in `zunel-core` (next to
//! `DreamService` and `MemoryStore`) because `zunel-tools` doesn't
//! depend on `zunel-core` — and we need the Dream pipeline. The
//! dependency direction is fine because `zunel-core` already pulls
//! in `zunel-tools` for the `Tool` trait.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use zunel_config::DreamConfig;
use zunel_providers::LLMProvider;
use zunel_tools::{Tool, ToolContext, ToolResult};

use crate::memory::{DreamOutcome, DreamService, MemoryStore};

/// Shared handle to the Dream pipeline. Each tool call clones the
/// `MemoryStore` (it's a cheap two-field struct), bumps the provider
/// `Arc`, and constructs a fresh `DreamService`. Cheaper than holding
/// a long-lived `DreamService` because `DreamService::run` takes
/// `&self`, and the inputs change over time (config edits via reload
/// would require swapping anyway).
pub struct DreamRunTool {
    store: MemoryStore,
    provider: Arc<dyn LLMProvider>,
    model: String,
    dream_config: DreamConfig,
}

impl DreamRunTool {
    pub fn new(
        store: MemoryStore,
        provider: Arc<dyn LLMProvider>,
        model: String,
        dream_config: DreamConfig,
    ) -> Self {
        Self {
            store,
            provider,
            model,
            dream_config,
        }
    }

    async fn run_once(&self) -> Result<DreamOutcome, String> {
        let svc = DreamService::new(
            self.store.clone(),
            self.provider.clone(),
            self.model.clone(),
        )
        .with_config(&self.dream_config);
        svc.run().await.map_err(|e| e.to_string())
    }
}

#[async_trait]
impl Tool for DreamRunTool {
    fn name(&self) -> &'static str {
        "zunel_dream_run"
    }

    fn description(&self) -> &'static str {
        "Run a Dream memory-consolidation pass now. Dream reads new entries from memory/history.jsonl and edits MEMORY.md / SOUL.md / USER.md to capture durable knowledge from recent conversations. Use this when the user asks to 'consolidate memory now', 'run dream', or after a long burst of important conversations. Returns the count of processed entries plus the files that were edited."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> ToolResult {
        match self.run_once().await {
            Ok(outcome) if outcome.processed_entries == 0 => ToolResult::ok(
                serde_json::to_string(&json!({
                    "status": "noop",
                    "processed_entries": 0,
                    "edited_files": [],
                    "note": "no new history entries since last pass"
                }))
                .unwrap_or_default(),
            ),
            Ok(outcome) => ToolResult::ok(
                serde_json::to_string(&json!({
                    "status": if outcome.is_active() { "applied" } else { "analysed-no-edits" },
                    "processed_entries": outcome.processed_entries,
                    "edited_files": outcome.edited_files,
                    "cursor_advanced_to": outcome.cursor_advanced_to,
                }))
                .unwrap_or_default(),
            ),
            Err(err) => ToolResult::err(format!("dream run failed: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::sync::Mutex;
    use zunel_providers::{
        ChatMessage, GenerationSettings, LLMResponse, StreamEvent, ToolSchema, Usage,
    };

    /// Replays the same provider behaviour used in `memory_test.rs`'s
    /// happy-path Dream test: generate() returns an analysis blob,
    /// stream() first emits a write_file ToolCallDelta + Done, then on
    /// the follow-up call emits a clean text completion.
    struct DreamProvider {
        stream_calls: Mutex<u32>,
    }

    #[async_trait]
    impl LLMProvider for DreamProvider {
        async fn generate(
            &self,
            _model: &str,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _settings: &GenerationSettings,
        ) -> zunel_providers::Result<LLMResponse> {
            Ok(LLMResponse {
                content: Some(
                    "analysis: user prefers Rust over Go; add as a durable preference to MEMORY.md."
                        .into(),
                ),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: Some("stop".into()),
            })
        }

        fn generate_stream<'a>(
            &'a self,
            _model: &'a str,
            _messages: &'a [ChatMessage],
            _tools: &'a [ToolSchema],
            _settings: &'a GenerationSettings,
        ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
            let call = {
                let mut guard = self.stream_calls.lock().unwrap();
                let call = *guard;
                *guard += 1;
                call
            };
            Box::pin(async_stream::stream! {
                if call == 0 {
                    yield Ok(StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-write".into()),
                        name: Some("write_file".into()),
                        arguments_fragment: Some(json!({
                            "path": "memory/MEMORY.md",
                            "content": "# Memory\n\n- user likes Rust"
                        }).to_string()),
                    });
                    yield Ok(StreamEvent::Done(LLMResponse {
                        content: None,
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        finish_reason: Some("tool_calls".into()),
                    }));
                } else {
                    yield Ok(StreamEvent::ContentDelta("done".into()));
                    yield Ok(StreamEvent::Done(LLMResponse {
                        content: Some("done".into()),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        finish_reason: None,
                    }));
                }
            })
        }
    }

    #[tokio::test]
    async fn dream_run_tool_invokes_dream_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path().to_path_buf());
        store.append_history("user prefers Rust over Go").unwrap();

        let provider: Arc<dyn LLMProvider> = Arc::new(DreamProvider {
            stream_calls: Mutex::new(0),
        });
        let tool = DreamRunTool::new(store, provider, "m".into(), DreamConfig::default());
        let ctx = ToolContext::new_with_workspace(tmp.path().to_path_buf(), "test".into());
        let res = tool.execute(json!({}), &ctx).await;
        assert!(!res.is_error, "tool call should succeed: {res:?}");
        assert!(
            res.content.contains("\"status\":\"applied\""),
            "got {}",
            res.content
        );
        assert!(res.content.contains("write_file"));
    }

    #[tokio::test]
    async fn dream_run_tool_returns_noop_when_no_input() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path().to_path_buf());
        // No append_history — Dream should no-op.
        let provider: Arc<dyn LLMProvider> = Arc::new(DreamProvider {
            stream_calls: Mutex::new(0),
        });
        let tool = DreamRunTool::new(store, provider, "m".into(), DreamConfig::default());
        let ctx = ToolContext::new_with_workspace(tmp.path().to_path_buf(), "test".into());
        let res = tool.execute(json!({}), &ctx).await;
        assert!(!res.is_error);
        assert!(
            res.content.contains("\"status\":\"noop\""),
            "got {}",
            res.content
        );
    }
}
