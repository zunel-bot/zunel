use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};
use zunel_bus::{InboundMessage, MessageBus, MessageKind};
use zunel_config::AgentDefaults;
use zunel_core::{AgentLoop, ApprovalScope, SessionManager};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema, Usage,
};
use zunel_tools::{Tool, ToolContext, ToolRegistry, ToolResult};

struct FinalProvider;

#[async_trait]
impl LLMProvider for FinalProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        unreachable!("gateway path is streamed")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::ContentDelta("gateway ".into()));
            yield Ok(StreamEvent::ContentDelta("reply".into()));
            yield Ok(StreamEvent::Done(LLMResponse {
                content: Some("gateway reply".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: None,
            }));
        })
    }
}

struct CapturingProvider {
    seen: Arc<Mutex<Vec<String>>>,
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
        unreachable!("gateway path is streamed")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        self.seen
            .lock()
            .unwrap()
            .push(messages.last().unwrap().content.clone());
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

struct ToolCallingProvider {
    calls: Mutex<usize>,
}

#[async_trait]
impl LLMProvider for ToolCallingProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        unreachable!("gateway path is streamed")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            let call = *calls;
            *calls += 1;
            call
        };
        Box::pin(async_stream::stream! {
            if call == 0 {
                yield Ok(StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-write".into()),
                    name: Some("write_file".into()),
                    arguments_fragment: Some(json!({"path": "x", "content": "y"}).to_string()),
                });
                yield Ok(StreamEvent::Done(LLMResponse {
                    content: None,
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    finish_reason: Some("tool_calls".into()),
                }));
            } else {
                yield Ok(StreamEvent::ContentDelta("approved".into()));
                yield Ok(StreamEvent::Done(LLMResponse {
                    content: Some("approved".into()),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    finish_reason: None,
                }));
            }
        })
    }
}

struct NoopWriteTool;

#[async_trait]
impl Tool for NoopWriteTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "test write tool"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::ok("wrote")
    }
}

#[tokio::test]
async fn process_inbound_once_publishes_final_outbound_message() {
    let tmp = tempfile::tempdir().unwrap();
    let provider: Arc<dyn LLMProvider> = Arc::new(FinalProvider);
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "m".into(),
        ..Default::default()
    };
    let agent = AgentLoop::with_sessions(provider, defaults, SessionManager::new(tmp.path()))
        .with_workspace(tmp.path().to_path_buf());
    let bus = Arc::new(MessageBus::new(8));
    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "C123:T456".into(),
        user_id: Some("U123".into()),
        content: "hello".into(),
        media: Vec::new(),
        kind: MessageKind::User,
    })
    .await
    .unwrap();

    agent.process_inbound_once(&bus).await.unwrap();

    let outbound = bus.next_outbound().await.unwrap();
    assert_eq!(outbound.channel, "slack");
    assert_eq!(outbound.chat_id, "C123:T456");
    assert_eq!(outbound.content, "gateway reply");
    assert_eq!(outbound.kind, MessageKind::Final);
}

#[tokio::test]
async fn process_inbound_once_extracts_document_media_before_model_call() {
    let tmp = tempfile::tempdir().unwrap();
    let doc = tmp.path().join("notes.md");
    std::fs::write(&doc, "remember document facts").unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(CapturingProvider { seen: seen.clone() });
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "m".into(),
        ..Default::default()
    };
    let agent = AgentLoop::with_sessions(provider, defaults, SessionManager::new(tmp.path()))
        .with_workspace(tmp.path().to_path_buf());
    let bus = Arc::new(MessageBus::new(8));
    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "D1".into(),
        user_id: Some("U123".into()),
        content: "hello".into(),
        media: vec![doc.display().to_string()],
        kind: MessageKind::User,
    })
    .await
    .unwrap();

    agent.process_inbound_once(&bus).await.unwrap();

    let content = seen.lock().unwrap().pop().unwrap();
    assert!(content.contains("hello"));
    assert!(content.contains("[File: notes.md]"));
    assert!(content.contains("remember document facts"));
}

#[tokio::test]
async fn process_inbound_once_prompts_for_remote_tool_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolCallingProvider {
        calls: Mutex::new(0),
    });
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "m".into(),
        ..Default::default()
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(NoopWriteTool));
    let agent = AgentLoop::with_sessions(provider, defaults, SessionManager::new(tmp.path()))
        .with_workspace(tmp.path().to_path_buf())
        .with_tools(tools)
        .with_approval_required(true)
        .with_approval_scope(ApprovalScope::Writes);
    let bus = Arc::new(MessageBus::new(8));
    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "C123:T456".into(),
        user_id: Some("U123".into()),
        content: "please write".into(),
        media: Vec::new(),
        kind: MessageKind::User,
    })
    .await
    .unwrap();

    let bus_for_task = bus.clone();
    let task = tokio::spawn(async move { agent.process_inbound_once(&bus_for_task).await });
    let approval = bus.next_outbound().await.unwrap();
    assert_eq!(approval.kind, MessageKind::Approval);
    assert_eq!(approval.channel, "slack");
    assert_eq!(approval.chat_id, "C123:T456");
    let request_id = approval.message_id.clone().unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "C123:T456".into(),
        user_id: Some("U123".into()),
        content: format!("approve:{request_id}"),
        media: Vec::new(),
        kind: MessageKind::ApprovalResponse,
    })
    .await
    .unwrap();

    task.await.unwrap().unwrap();
    let final_message = bus.next_outbound().await.unwrap();
    assert_eq!(final_message.kind, MessageKind::Final);
    assert_eq!(final_message.content, "approved");
}
