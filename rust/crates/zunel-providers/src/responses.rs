use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::base::{
    ChatMessage, LLMResponse, Role, StreamEvent, ToolCallRequest, ToolSchema, Usage,
};
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct ResponsesMessages {
    pub instructions: String,
    pub input: Value,
}

pub fn convert_messages(messages: &[ChatMessage]) -> Result<ResponsesMessages> {
    let mut instructions = String::new();
    let mut input_items = Vec::new();

    for (idx, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::System => {
                instructions = msg.content.clone();
            }
            Role::User => {
                input_items.push(json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": msg.content}],
                }));
            }
            Role::Assistant => {
                if !msg.content.is_empty() {
                    input_items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": msg.content}],
                        "status": "completed",
                        "id": format!("msg_{idx}"),
                    }));
                }
                for call in &msg.tool_calls {
                    let (call_id, item_id) = split_tool_call_id(&call.id, idx);
                    let arguments = serde_json::to_string(&call.arguments)
                        .map_err(|e| Error::Parse(format!("tool arguments encode: {e}")))?;
                    input_items.push(json!({
                        "type": "function_call",
                        "id": item_id.unwrap_or_else(|| format!("fc_{idx}")),
                        "call_id": call_id,
                        "name": call.name,
                        "arguments": arguments,
                    }));
                }
            }
            Role::Tool => {
                let id = msg.tool_call_id.as_deref().unwrap_or("call_0");
                let (call_id, _) = split_tool_call_id(id, idx);
                input_items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": msg.content,
                }));
            }
        }
    }

    Ok(ResponsesMessages {
        instructions,
        input: Value::Array(input_items),
    })
}

pub fn convert_tools(tools: &[ToolSchema]) -> Value {
    Value::Array(
        tools
            .iter()
            .filter(|tool| !tool.name.is_empty())
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect(),
    )
}

pub fn map_finish_reason(status: Option<&str>) -> &'static str {
    match status.unwrap_or("completed") {
        "completed" => "stop",
        "incomplete" => "length",
        "failed" | "cancelled" => "error",
        _ => "stop",
    }
}

#[derive(Debug, Default)]
pub struct ResponsesStreamParser {
    content: String,
    finish_reason: Option<String>,
    calls_by_id: BTreeMap<String, ResponseCallBuffer>,
    next_index: u32,
    usage: Usage,
}

#[derive(Debug, Clone)]
struct ResponseCallBuffer {
    index: u32,
    item_id: String,
    name: String,
    arguments: String,
}

impl ResponsesStreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept(&mut self, event: &Value) -> Result<Vec<StreamEvent>> {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "response.output_text.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if delta.is_empty() {
                    Ok(Vec::new())
                } else {
                    self.content.push_str(delta);
                    Ok(vec![StreamEvent::ContentDelta(delta.to_string())])
                }
            }
            "response.output_item.added" => self.accept_output_item_added(event),
            "response.function_call_arguments.delta" => self.accept_arguments_delta(event),
            "response.function_call_arguments.done" => self.accept_arguments_done(event),
            "response.output_item.done" => self.accept_output_item_done(event),
            "response.completed" => self.accept_completed(event),
            "error" | "response.failed" => Err(Error::ProviderReturned {
                status: 500,
                body: format!("Response failed: {}", render_error_detail(event)),
            }),
            _ => Ok(Vec::new()),
        }
    }

    pub fn finish(&mut self) -> Result<Vec<StreamEvent>> {
        Ok(vec![self.done_event(
            self.finish_reason
                .clone()
                .unwrap_or_else(|| "stop".to_string()),
        )])
    }

    fn accept_output_item_added(&mut self, event: &Value) -> Result<Vec<StreamEvent>> {
        let item = event.get("item").unwrap_or(&Value::Null);
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return Ok(Vec::new());
        }
        let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        let item_id = item.get("id").and_then(Value::as_str).unwrap_or("fc_0");
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        let index = self.next_index;
        self.next_index += 1;
        self.calls_by_id.insert(
            call_id.to_string(),
            ResponseCallBuffer {
                index,
                item_id: item_id.to_string(),
                name: name.to_string(),
                arguments: item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
        );
        Ok(vec![StreamEvent::ToolCallDelta {
            index,
            id: Some(format!("{call_id}|{item_id}")),
            name: Some(name.to_string()),
            arguments_fragment: None,
        }])
    }

    fn accept_arguments_delta(&mut self, event: &Value) -> Result<Vec<StreamEvent>> {
        let Some(call_id) = event.get("call_id").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        let Some(call) = self.calls_by_id.get_mut(call_id) else {
            return Ok(Vec::new());
        };
        let delta = event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return Ok(Vec::new());
        }
        call.arguments.push_str(delta);
        Ok(vec![StreamEvent::ToolCallDelta {
            index: call.index,
            id: None,
            name: None,
            arguments_fragment: Some(delta.to_string()),
        }])
    }

    fn accept_arguments_done(&mut self, event: &Value) -> Result<Vec<StreamEvent>> {
        let Some(call_id) = event.get("call_id").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        let Some(call) = self.calls_by_id.get_mut(call_id) else {
            return Ok(Vec::new());
        };
        let Some(arguments) = event.get("arguments").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        if call.arguments.is_empty() && !arguments.is_empty() {
            call.arguments = arguments.to_string();
            return Ok(vec![StreamEvent::ToolCallDelta {
                index: call.index,
                id: None,
                name: None,
                arguments_fragment: Some(arguments.to_string()),
            }]);
        }
        call.arguments = arguments.to_string();
        Ok(Vec::new())
    }

    fn accept_output_item_done(&mut self, event: &Value) -> Result<Vec<StreamEvent>> {
        let item = event.get("item").unwrap_or(&Value::Null);
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return Ok(Vec::new());
        }
        let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        let Some(call) = self.calls_by_id.get_mut(call_id) else {
            return Ok(Vec::new());
        };
        if call.arguments.is_empty() {
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string();
            call.arguments = arguments.clone();
            if !arguments.is_empty() {
                return Ok(vec![StreamEvent::ToolCallDelta {
                    index: call.index,
                    id: None,
                    name: None,
                    arguments_fragment: Some(arguments),
                }]);
            }
        }
        Ok(Vec::new())
    }

    fn accept_completed(&mut self, event: &Value) -> Result<Vec<StreamEvent>> {
        let response = event.get("response");
        let status = response
            .and_then(|v| v.get("status"))
            .and_then(Value::as_str);
        let finish_reason = map_finish_reason(status).to_string();
        self.finish_reason = Some(finish_reason.clone());
        if let Some(usage_val) = response.and_then(|v| v.get("usage")) {
            self.usage = parse_responses_usage(usage_val);
        }

        Ok(vec![self.done_event(finish_reason)])
    }

    fn done_event(&self, finish_reason: String) -> StreamEvent {
        let mut tool_calls = Vec::with_capacity(self.calls_by_id.len());
        for (call_id, call) in &self.calls_by_id {
            let args_raw = if call.arguments.is_empty() {
                "{}"
            } else {
                call.arguments.as_str()
            };
            let arguments =
                serde_json::from_str(args_raw).unwrap_or_else(|_| json!({"raw": args_raw}));
            tool_calls.push(ToolCallRequest {
                id: format!("{call_id}|{}", call.item_id),
                name: call.name.clone(),
                arguments,
                index: call.index,
            });
        }
        tool_calls.sort_by_key(|call| call.index);

        StreamEvent::Done(LLMResponse {
            content: if self.content.is_empty() {
                None
            } else {
                Some(self.content.clone())
            },
            tool_calls,
            usage: self.usage.clone(),
            finish_reason: Some(finish_reason),
        })
    }
}

/// Parse the `usage` block on a Codex Responses `response.completed`
/// event. The wire shape is:
///
/// ```json
/// { "input_tokens": 123,
///   "input_tokens_details": { "cached_tokens": 0 },
///   "output_tokens": 45,
///   "output_tokens_details": { "reasoning_tokens": 678 },
///   "total_tokens": 846 }
/// ```
///
/// Missing fields default to 0 so partial / older payloads still parse.
fn parse_responses_usage(value: &Value) -> Usage {
    let prompt_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let completion_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let cached_tokens = value
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let reasoning_tokens = value
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    Usage {
        prompt_tokens,
        completion_tokens,
        cached_tokens,
        reasoning_tokens,
    }
}

fn split_tool_call_id(raw: &str, fallback_idx: usize) -> (String, Option<String>) {
    if raw.is_empty() {
        return (format!("call_{fallback_idx}"), None);
    }
    if let Some((call_id, item_id)) = raw.split_once('|') {
        return (
            call_id.to_string(),
            (!item_id.is_empty()).then(|| item_id.to_string()),
        );
    }
    (raw.to_string(), None)
}

fn render_error_detail(event: &Value) -> String {
    if let Some(message) = event
        .get("error")
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
    {
        return message.to_string();
    }
    if let Some(message) = event.get("message").and_then(Value::as_str) {
        return message.to_string();
    }
    event.to_string()
}
