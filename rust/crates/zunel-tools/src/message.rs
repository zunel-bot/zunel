use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use zunel_bus::{MessageKind, OutboundMessage, OutboundPublisher};

use crate::{Tool, ToolContext, ToolResult};

#[async_trait]
pub trait MessageSink: Send + Sync {
    async fn send(&self, message: OutboundMessage) -> Result<(), String>;
}

#[async_trait]
impl MessageSink for OutboundPublisher {
    async fn send(&self, message: OutboundMessage) -> Result<(), String> {
        OutboundPublisher::send(self, message)
            .await
            .map_err(|err| err.to_string())
    }
}

pub struct MessageTool {
    sink: Arc<dyn MessageSink>,
}

impl MessageTool {
    pub fn new(sink: Arc<dyn MessageSink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl Tool for MessageTool {
    fn name(&self) -> &'static str {
        "message"
    }

    fn description(&self) -> &'static str {
        "Send a message to the user, optionally targeting a specific channel and chat."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "The message content to send"},
                "channel": {"type": "string", "description": "Optional target channel, for example cli or slack"},
                "chat_id": {"type": "string", "description": "Optional target chat/user ID"},
                "media": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional file paths to attach"
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return ToolResult::err("missing required field: content");
        };
        let (default_channel, default_chat_id) = match parse_session_key(&ctx.session_key) {
            Some(parts) => parts,
            None => return ToolResult::err("Error: No target channel/chat specified"),
        };
        let channel = args
            .get("channel")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&default_channel);
        let chat_id = args
            .get("chat_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&default_chat_id);
        if channel.is_empty() || chat_id.is_empty() {
            return ToolResult::err("Error: No target channel/chat specified");
        }
        let media = args
            .get("media")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let message = OutboundMessage {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            message_id: None,
            content: strip_think(content),
            media,
            kind: MessageKind::Final,
        };
        match self.sink.send(message).await {
            Ok(()) => ToolResult::ok(format!("Message sent to {channel}:{chat_id}")),
            Err(err) => ToolResult::err(format!("Error sending message: {err}")),
        }
    }
}

fn parse_session_key(session_key: &str) -> Option<(String, String)> {
    let (channel, chat_id) = session_key.split_once(':')?;
    Some((channel.to_string(), chat_id.to_string()))
}

fn strip_think(content: &str) -> String {
    let re = Regex::new(r"(?s)<think>.*?</think>").expect("valid think regex");
    re.replace_all(content, "").to_string()
}
