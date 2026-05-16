use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use zunel_core::hook::{AgentHook, AgentHookContext};
use zunel_core::{AgentRunSpec, AgentRunner, ApprovalDecision, ApprovalHandler, ApprovalRequest};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema, Usage,
};
use zunel_tools::{Tool, ToolContext, ToolRegistry, ToolResult};

fn done(content: Option<&str>, finish: &str) -> StreamEvent {
    StreamEvent::Done(LLMResponse {
        content: content.map(String::from),
        tool_calls: Vec::new(),
        usage: Usage::default(),
        finish_reason: Some(finish.into()),
    })
}

struct ScriptedProvider {
    turns: Arc<Mutex<Vec<Vec<StreamEvent>>>>,
}

#[async_trait]
impl LLMProvider for ScriptedProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        unreachable!("runner only calls generate_stream")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        let events = self.turns.lock().unwrap().pop().unwrap_or_default();
        Box::pin(async_stream::try_stream! {
            for e in events { yield e; }
        })
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echo"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, args: Value, _: &ToolContext) -> ToolResult {
        ToolResult::ok(args.get("text").and_then(Value::as_str).unwrap_or(""))
    }
}

struct AlwaysApprove;

#[async_trait]
impl ApprovalHandler for AlwaysApprove {
    async fn request(&self, _: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}

#[derive(Default)]
struct RecordingHook {
    events: Mutex<Vec<String>>,
}

#[async_trait]
impl AgentHook for RecordingHook {
    async fn before_iteration(&self, context: AgentHookContext) {
        self.events
            .lock()
            .unwrap()
            .push(format!("before:{}", context.iteration));
    }

    async fn before_execute_tools(&self, context: AgentHookContext) {
        self.events.lock().unwrap().push(format!(
            "before_tools:{}:{}",
            context.iteration,
            context.tool_calls.len()
        ));
    }

    async fn after_iteration(&self, context: AgentHookContext) {
        self.events.lock().unwrap().push(format!(
            "after:{}:{}",
            context.iteration,
            context.tool_results.len()
        ));
    }

    fn finalize_content(
        &self,
        _context: AgentHookContext,
        content: Option<String>,
    ) -> Option<String> {
        content.map(|content| format!("{content}!"))
    }
}

#[tokio::test]
async fn runner_invokes_hook_lifecycle_in_order() {
    let turns_raw = vec![
        vec![
            StreamEvent::ContentDelta("done".into()),
            done(Some("done"), "stop"),
        ],
        vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("echo".into()),
                arguments_fragment: Some(r#"{"text":"hi"}"#.into()),
            },
            done(None, "tool_calls"),
        ],
    ];
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider {
        turns: Arc::new(Mutex::new(turns_raw)),
    });
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let hook = Arc::new(RecordingHook::default());

    let runner = AgentRunner::new(provider, Arc::new(registry), Arc::new(AlwaysApprove));
    let spec = AgentRunSpec {
        initial_messages: vec![ChatMessage::user("call echo")],
        model: "m".into(),
        max_iterations: 5,
        workspace: std::env::temp_dir(),
        session_key: "cli:direct".into(),
        hook: Some(hook.clone()),
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel(64);
    let result = runner.run(spec, tx).await.unwrap();

    assert_eq!(result.content, "done!");
    assert_eq!(
        hook.events.lock().unwrap().as_slice(),
        &[
            "before:0".to_string(),
            "before_tools:0:1".to_string(),
            "after:0:1".to_string(),
            "before:1".to_string(),
            "after:1:0".to_string(),
        ]
    );
}
