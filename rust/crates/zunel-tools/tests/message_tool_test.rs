use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;
use zunel_bus::{MessageBus, MessageKind, OutboundMessage};
use zunel_tools::message::{MessageSink, MessageTool};
use zunel_tools::{Tool, ToolContext};

#[derive(Default)]
struct RecordingSink {
    messages: Mutex<Vec<OutboundMessage>>,
}

#[async_trait]
impl MessageSink for RecordingSink {
    async fn send(&self, message: OutboundMessage) -> Result<(), String> {
        self.messages.lock().await.push(message);
        Ok(())
    }
}

#[tokio::test]
async fn message_tool_sends_to_current_session_by_default() {
    let sink = Arc::new(RecordingSink::default());
    let tool = MessageTool::new(sink.clone());
    let ctx = ToolContext::new_with_workspace(std::env::temp_dir(), "slack:C123:T456".into());

    let result = tool
        .execute(json!({"content": "hello <think>hidden</think>world"}), &ctx)
        .await;

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "Message sent to slack:C123:T456");
    let messages = sink.messages.lock().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].channel, "slack");
    assert_eq!(messages[0].chat_id, "C123:T456");
    assert_eq!(messages[0].content, "hello world");
    assert_eq!(messages[0].kind, MessageKind::Final);
}

#[tokio::test]
async fn message_tool_allows_explicit_target_override() {
    let sink = Arc::new(RecordingSink::default());
    let tool = MessageTool::new(sink.clone());
    let ctx = ToolContext::new_with_workspace(std::env::temp_dir(), "slack:C123:T456".into());

    let result = tool
        .execute(
            json!({"content": "status update", "channel": "cron", "chat_id": "job-1"}),
            &ctx,
        )
        .await;

    assert!(!result.is_error, "{}", result.content);
    let messages = sink.messages.lock().await;
    assert_eq!(messages[0].channel, "cron");
    assert_eq!(messages[0].chat_id, "job-1");
}

#[tokio::test]
async fn message_tool_can_send_through_bus_outbound_publisher() {
    let bus = MessageBus::new(8);
    let tool = MessageTool::new(Arc::new(bus.outbound_publisher()));
    let ctx = ToolContext::new_with_workspace(std::env::temp_dir(), "slack:C123:T456".into());

    let result = tool.execute(json!({"content": "through bus"}), &ctx).await;

    assert!(!result.is_error, "{}", result.content);
    let outbound = bus.next_outbound().await.unwrap();
    assert_eq!(outbound.channel, "slack");
    assert_eq!(outbound.chat_id, "C123:T456");
    assert_eq!(outbound.content, "through bus");
}
