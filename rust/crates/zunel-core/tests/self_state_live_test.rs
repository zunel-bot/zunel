//! `RuntimeSelfStateProvider` must report live values:
//!
//! * `current_iteration` must reflect the running counter (not the
//!   historical hard-coded 0).
//! * `tools` must mirror the live `SharedToolRegistry`, so an
//!   MCP-driven add/remove via `mcp_reconnect` is visible to the
//!   agent on the next `self` call.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{json, Value};
use zunel_core::{RuntimeSelfStateProvider, SharedToolRegistry, SubagentManager};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema,
};
use zunel_tools::{self_tool::SelfStateProvider, Tool, ToolContext, ToolRegistry, ToolResult};

struct DummyProvider;

#[async_trait]
impl LLMProvider for DummyProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        unreachable!()
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> futures::stream::BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        unreachable!()
    }
}

struct DummyTool {
    name: &'static str,
}

#[async_trait]
impl Tool for DummyTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "noop"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::ok("ok")
    }
}

#[test]
fn current_iteration_reflects_live_atomic() {
    let provider: Arc<dyn LLMProvider> = Arc::new(DummyProvider);
    let subagents = Arc::new(SubagentManager::new(
        provider,
        std::env::temp_dir(),
        "m".into(),
    ));
    let counter = Arc::new(AtomicUsize::new(0));
    let state_provider = RuntimeSelfStateProvider {
        model: "m".into(),
        provider: "p".into(),
        workspace: "/tmp".into(),
        max_iterations: 15,
        tools: Vec::new(),
        subagents,
        iteration_counter: Some(counter.clone()),
        live_tools: None,
    };

    let s = state_provider.state();
    assert_eq!(s.current_iteration, 0, "starts idle");

    counter.store(7, Ordering::Relaxed);
    let s = state_provider.state();
    assert_eq!(s.current_iteration, 7, "follows the atomic");
}

#[test]
fn tools_list_reflects_live_registry() {
    let provider: Arc<dyn LLMProvider> = Arc::new(DummyProvider);
    let subagents = Arc::new(SubagentManager::new(
        provider,
        std::env::temp_dir(),
        "m".into(),
    ));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(DummyTool { name: "read_file" }));
    reg.register(Arc::new(DummyTool { name: "write_file" }));
    let shared: SharedToolRegistry = Arc::new(RwLock::new(Arc::new(reg)));

    let state_provider = RuntimeSelfStateProvider {
        model: "m".into(),
        provider: "p".into(),
        workspace: "/tmp".into(),
        max_iterations: 15,
        tools: vec!["should-not-be-used".into()],
        subagents,
        iteration_counter: None,
        live_tools: Some(Arc::clone(&shared)),
    };

    let snap = state_provider.state();
    assert_eq!(
        snap.tools.iter().filter(|n| *n == "read_file").count(),
        1,
        "live registry visible: {:?}",
        snap.tools
    );
    assert_eq!(snap.tools.iter().filter(|n| *n == "write_file").count(), 1);
    assert!(
        !snap.tools.iter().any(|n| n == "should-not-be-used"),
        "static fallback ignored when live registry is set: {:?}",
        snap.tools
    );

    // Mutate the live registry — the next state() must see it.
    {
        let mut guard = shared.write().unwrap();
        Arc::make_mut(&mut *guard).register(Arc::new(DummyTool { name: "shell" }));
    }
    let snap = state_provider.state();
    assert!(
        snap.tools.iter().any(|n| n == "shell"),
        "MCP-driven add visible immediately: {:?}",
        snap.tools
    );
}

#[test]
fn tools_falls_back_to_static_without_live_registry() {
    let provider: Arc<dyn LLMProvider> = Arc::new(DummyProvider);
    let subagents = Arc::new(SubagentManager::new(
        provider,
        std::env::temp_dir(),
        "m".into(),
    ));
    let state_provider = RuntimeSelfStateProvider {
        model: "m".into(),
        provider: "p".into(),
        workspace: "/tmp".into(),
        max_iterations: 15,
        tools: vec!["only".into(), "static".into()],
        subagents,
        iteration_counter: None,
        live_tools: None,
    };
    let snap = state_provider.state();
    assert_eq!(snap.tools, vec!["only".to_string(), "static".to_string()]);
}
