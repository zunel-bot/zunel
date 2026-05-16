//! Wire types shared between non-streaming and streaming requests.
//!
//! The serde structs intentionally mirror the OpenAI `chat.completions`
//! shape (and its `cached_tokens` / `reasoning_tokens` post-2024 details
//! sub-objects) so providers that follow the OpenAI schema work without
//! per-vendor branches in the response decoder.

use serde::{Deserialize, Serialize};

use crate::base::{ChatMessage, GenerationSettings, Role, ToolCallRequest, ToolSchema, Usage};
use crate::error::{Error, Result};

#[derive(Serialize)]
pub(super) struct RequestBody<'a> {
    pub model: &'a str,
    pub messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
}

#[derive(Serialize)]
pub(super) struct WireMessage<'a> {
    pub role: &'a str,
    /// `null` for assistant messages that only carry `tool_calls`; a
    /// string for every other role. OpenAI accepts either form; this
    /// shape keeps session fixtures stable on disk.
    pub content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireToolCall<'a>>>,
}

#[derive(Serialize)]
pub(super) struct WireToolCall<'a> {
    pub id: &'a str,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireToolFunction<'a>,
}

#[derive(Serialize)]
pub(super) struct WireToolFunction<'a> {
    pub name: &'a str,
    /// OpenAI emits `function.arguments` as a JSON-encoded string, not
    /// a parsed object. We serialize the stored `Value` back to a
    /// compact string so round-tripping is exact.
    pub arguments: String,
}

#[derive(Serialize)]
pub(super) struct WireTool<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireToolFn<'a>,
}

#[derive(Serialize)]
pub(super) struct WireToolFn<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub parameters: &'a serde_json::Value,
}

impl<'a> RequestBody<'a> {
    pub(super) fn new(
        model: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSchema],
        settings: &GenerationSettings,
    ) -> Self {
        let wire = messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };
                let tool_calls = if m.tool_calls.is_empty() {
                    None
                } else {
                    Some(
                        m.tool_calls
                            .iter()
                            .map(|tc| WireToolCall {
                                id: &tc.id,
                                kind: "function",
                                function: WireToolFunction {
                                    name: &tc.name,
                                    arguments: tc.arguments.to_string(),
                                },
                            })
                            .collect(),
                    )
                };
                // Assistant messages that only carry tool_calls emit
                // `content: null`; every other message keeps its string
                // (even if empty).
                let content = if tool_calls.is_some() && m.content.is_empty() {
                    None
                } else {
                    Some(m.content.as_str())
                };
                WireMessage {
                    role,
                    content,
                    tool_call_id: m.tool_call_id.as_deref(),
                    tool_calls,
                }
            })
            .collect();

        let (wire_tools, tool_choice) = if tools.is_empty() {
            (None, None)
        } else {
            let wrapped = tools
                .iter()
                .map(|t| WireTool {
                    kind: "function",
                    function: WireToolFn {
                        name: &t.name,
                        description: &t.description,
                        parameters: &t.parameters,
                    },
                })
                .collect();
            (Some(wrapped), Some("auto"))
        };

        Self {
            model,
            messages: wire,
            temperature: settings.temperature,
            max_tokens: settings.max_tokens,
            tools: wire_tools,
            tool_choice,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct ResponseBody {
    pub choices: Vec<Choice>,
    pub usage: Option<WireUsage>,
}

#[derive(Deserialize)]
pub(super) struct Choice {
    pub message: ResponseMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ResponseMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<WireToolCallResponse>>,
}

#[derive(Deserialize)]
pub(super) struct WireToolCallResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<WireToolFunctionResponse>,
}

#[derive(Deserialize)]
pub(super) struct WireToolFunctionResponse {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Deserialize, Default)]
pub(super) struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: u32,
    #[serde(default)]
    pub completion_tokens_details: Option<WireCompletionTokensDetails>,
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
}

#[derive(Deserialize, Default)]
pub(super) struct WireCompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: u32,
}

#[derive(Deserialize, Default)]
pub(super) struct WirePromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

impl From<WireUsage> for Usage {
    fn from(value: WireUsage) -> Self {
        let cached_tokens = if value.cached_tokens != 0 {
            value.cached_tokens
        } else {
            value
                .prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .unwrap_or(0)
        };
        let reasoning_tokens = if value.reasoning_tokens != 0 {
            value.reasoning_tokens
        } else {
            value
                .completion_tokens_details
                .as_ref()
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0)
        };
        Usage {
            prompt_tokens: value.prompt_tokens,
            completion_tokens: value.completion_tokens,
            cached_tokens,
            reasoning_tokens,
        }
    }
}

/// Convert a `function.tool_calls[]` entry from a non-streaming response
/// into the runner-facing [`ToolCallRequest`]. Errors when the embedded
/// `arguments` JSON string fails to parse — providers occasionally emit
/// truncated JSON for very long tool calls, and surfacing the raw bytes
/// in the error makes that diagnosable from logs.
pub(super) fn parse_wire_tool_call(
    index: usize,
    wc: WireToolCallResponse,
) -> Result<ToolCallRequest> {
    let args_raw = wc
        .function
        .as_ref()
        .and_then(|f| f.arguments.as_deref())
        .unwrap_or("{}");
    let arguments: serde_json::Value = serde_json::from_str(args_raw).map_err(|e| {
        Error::Parse(format!(
            "tool_call {} arguments not valid JSON: {e}. raw = {args_raw:?}",
            wc.id.as_deref().unwrap_or("<unknown>")
        ))
    })?;
    Ok(ToolCallRequest {
        id: wc.id.unwrap_or_default(),
        name: wc.function.and_then(|f| f.name).unwrap_or_default(),
        arguments,
        index: index as u32,
    })
}
