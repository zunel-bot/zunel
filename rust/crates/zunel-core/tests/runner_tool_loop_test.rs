use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use zunel_core::{
    runner::{AgentRunSpec, AgentRunner, StopReason},
    ApprovalDecision, ApprovalHandler, ApprovalRequest,
};
use zunel_providers::{
    error::Result as ProviderResult, ChatMessage, GenerationSettings, LLMProvider, LLMResponse,
    StreamEvent, ToolSchema, Usage,
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
    ) -> ProviderResult<LLMResponse> {
        unreachable!("runner only calls generate_stream")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, ProviderResult<StreamEvent>> {
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

#[tokio::test]
async fn runner_executes_tool_then_final_content() {
    let turns_raw: Vec<Vec<StreamEvent>> = vec![
        vec![
            StreamEvent::ContentDelta("done!".into()),
            done(Some("done!"), "stop"),
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

    let runner = AgentRunner::new(
        provider.clone(),
        Arc::new(registry),
        Arc::new(AlwaysApprove),
    );
    let spec = AgentRunSpec {
        initial_messages: vec![ChatMessage::user("please echo")],
        model: "m".into(),
        max_iterations: 5,
        workspace: std::env::temp_dir(),
        session_key: "cli:direct".into(),
        ..Default::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let result = runner.run(spec, tx).await.unwrap();
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.tools_used, vec!["echo".to_string()]);
    assert!(result.content.contains("done!"));
    rx.close();
}

#[tokio::test]
async fn runner_emits_tool_progress_events_around_each_call() {
    let turns_raw: Vec<Vec<StreamEvent>> = vec![
        vec![
            StreamEvent::ContentDelta("ok".into()),
            done(Some("ok"), "stop"),
        ],
        vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("echo".into()),
                arguments_fragment: Some(r#"{"text":"hello"}"#.into()),
            },
            done(None, "tool_calls"),
        ],
    ];
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider {
        turns: Arc::new(Mutex::new(turns_raw)),
    });

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let runner = AgentRunner::new(provider, Arc::new(registry), Arc::new(AlwaysApprove));
    let spec = AgentRunSpec {
        initial_messages: vec![ChatMessage::user("call echo")],
        model: "m".into(),
        max_iterations: 5,
        workspace: std::env::temp_dir(),
        session_key: "cli:direct".into(),
        ..Default::default()
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    });

    let _ = runner.run(spec, tx).await.unwrap();
    let events = collector.await.unwrap();

    let mut start_seen = false;
    let mut done_seen = false;
    for e in &events {
        if let StreamEvent::ToolProgress(p) = e {
            match p {
                zunel_providers::ToolProgress::Start { name, .. } => {
                    assert_eq!(name, "echo");
                    start_seen = true;
                }
                zunel_providers::ToolProgress::Done { name, ok, .. } => {
                    assert_eq!(name, "echo");
                    assert!(*ok);
                    done_seen = true;
                }
            }
        }
    }
    assert!(start_seen, "no ToolProgress::Start emitted: {events:?}");
    assert!(done_seen, "no ToolProgress::Done emitted: {events:?}");
}

#[tokio::test]
async fn runner_reports_max_iterations_when_model_never_stops() {
    let mut turns_raw: Vec<Vec<StreamEvent>> = Vec::new();
    for _ in 0..4 {
        turns_raw.push(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_loop".into()),
                name: Some("echo".into()),
                arguments_fragment: Some(r#"{"text":"x"}"#.into()),
            },
            done(None, "tool_calls"),
        ]);
    }
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider {
        turns: Arc::new(Mutex::new(turns_raw)),
    });

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));

    let runner = AgentRunner::new(provider, Arc::new(registry), Arc::new(AlwaysApprove));
    let spec = AgentRunSpec {
        initial_messages: vec![ChatMessage::user("loop")],
        model: "m".into(),
        max_iterations: 3,
        workspace: std::env::temp_dir(),
        session_key: "cli:direct".into(),
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(128);
    let result = runner.run(spec, tx).await.unwrap();
    assert_eq!(result.stop_reason, StopReason::MaxIterations);
    assert_eq!(result.tools_used.len(), 3);
}
