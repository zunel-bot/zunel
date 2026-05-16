//! Pure-function mapping between zunel's `ChatMessage`/`ToolSchema`
//! shape and the Bedrock `Converse` wire types.
//!
//! Kept separate from the SDK call site in [`super::provider`] so the
//! mapping is fully unit-testable without instantiating an
//! `aws_sdk_bedrockruntime::Client` (which requires real AWS
//! credentials and a tokio runtime).
//!
//! Three Bedrock-specific quirks the converters enforce:
//!
//! 1. **System lift.** Converse takes system prompts in a separate
//!    top-level `system` field, not as a `Message`. zunel's
//!    `ChatMessage::System` rows are pulled out of the message list
//!    and appended to a `Vec<SystemContentBlock>` in order.
//! 2. **Tool→User coalescing.** Converse requires alternating
//!    `User` / `Assistant` turns. zunel may emit several consecutive
//!    `Role::Tool` rows when an assistant turn produced multiple
//!    parallel tool calls; those must be folded into a single
//!    `Message { role: User, content: [ToolResult, ToolResult, ...] }`
//!    so the model sees one user turn carrying all the tool results.
//! 3. **JSON ↔ Document.** Tool inputs/outputs and tool input schemas
//!    travel as `aws_smithy_types::Document` on the wire, not
//!    `serde_json::Value`. The two share semantics (object/array/
//!    number/string/bool/null) but have distinct types, so we
//!    translate at the boundary and keep the rest of zunel using
//!    plain `serde_json`.

use std::collections::HashMap;

use aws_sdk_bedrockruntime::operation::converse::ConverseOutput;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, ConverseOutput as ConverseOutputPayload, Message, StopReason,
    SystemContentBlock, Tool, ToolConfiguration, ToolInputSchema, ToolResultBlock,
    ToolResultContentBlock, ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::{Document, Number};
use serde_json::Value;

use crate::base::{ChatMessage, LLMResponse, Role, ToolCallRequest, ToolSchema, Usage};
use crate::error::{Error, Result};

/// Output of [`convert_messages`].
///
/// The converter splits zunel's flat `ChatMessage` slice into the two
/// inputs Converse actually wants: a `system` list plus a
/// strictly-alternating user/assistant message list.
#[derive(Debug)]
pub struct ConvertedMessages {
    pub system: Vec<SystemContentBlock>,
    pub messages: Vec<Message>,
}

/// Convert zunel chat history into Bedrock Converse `system` +
/// `messages`. Performs the system lift and tool-result coalescing
/// described in the module docs.
pub fn convert_messages(messages: &[ChatMessage]) -> Result<ConvertedMessages> {
    let mut system: Vec<SystemContentBlock> = Vec::new();
    let mut out: Vec<Message> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                if !msg.content.is_empty() {
                    system.push(SystemContentBlock::Text(msg.content.clone()));
                }
            }
            Role::User => {
                let mut blocks = Vec::with_capacity(1);
                if !msg.content.is_empty() {
                    blocks.push(ContentBlock::Text(msg.content.clone()));
                }
                if blocks.is_empty() {
                    continue;
                }
                out.push(build_message(ConversationRole::User, blocks)?);
            }
            Role::Assistant => {
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if !msg.content.is_empty() {
                    blocks.push(ContentBlock::Text(msg.content.clone()));
                }
                for call in &msg.tool_calls {
                    let tool_use = ToolUseBlock::builder()
                        .tool_use_id(call.id.clone())
                        .name(call.name.clone())
                        .input(json_value_to_document(&call.arguments))
                        .build()
                        .map_err(|e| Error::Parse(format!("bedrock ToolUseBlock build: {e}")))?;
                    blocks.push(ContentBlock::ToolUse(tool_use));
                }
                if blocks.is_empty() {
                    continue;
                }
                out.push(build_message(ConversationRole::Assistant, blocks)?);
            }
            Role::Tool => {
                let tool_use_id = msg.tool_call_id.clone().ok_or_else(|| {
                    Error::Parse("bedrock: Role::Tool message missing tool_call_id".to_string())
                })?;
                let tool_result = ToolResultBlock::builder()
                    .tool_use_id(tool_use_id)
                    .content(ToolResultContentBlock::Text(msg.content.clone()))
                    .build()
                    .map_err(|e| Error::Parse(format!("bedrock ToolResultBlock build: {e}")))?;
                let block = ContentBlock::ToolResult(tool_result);
                if let Some(last) = out.last_mut() {
                    if matches!(last.role, ConversationRole::User) {
                        last.content.push(block);
                        continue;
                    }
                }
                out.push(build_message(ConversationRole::User, vec![block])?);
            }
        }
    }

    Ok(ConvertedMessages {
        system,
        messages: out,
    })
}

fn build_message(role: ConversationRole, content: Vec<ContentBlock>) -> Result<Message> {
    let mut builder = Message::builder().role(role);
    for block in content {
        builder = builder.content(block);
    }
    builder
        .build()
        .map_err(|e| Error::Parse(format!("bedrock Message build: {e}")))
}

/// Convert zunel tool schemas into Bedrock `ToolConfiguration`. Returns
/// `None` when there are no tools (Converse rejects an empty
/// `ToolConfiguration.tools` vec, so we omit the field entirely).
pub fn convert_tools(tools: &[ToolSchema]) -> Result<Option<ToolConfiguration>> {
    let specs: Vec<Tool> = tools
        .iter()
        .filter(|t| !t.name.is_empty())
        .map(|t| {
            let schema = ToolInputSchema::Json(json_value_to_document(&t.parameters));
            let mut spec_builder = ToolSpecification::builder()
                .name(t.name.clone())
                .input_schema(schema);
            if !t.description.is_empty() {
                spec_builder = spec_builder.description(t.description.clone());
            }
            let spec = spec_builder
                .build()
                .map_err(|e| Error::Parse(format!("bedrock ToolSpecification build: {e}")))?;
            Ok(Tool::ToolSpec(spec))
        })
        .collect::<Result<Vec<_>>>()?;

    if specs.is_empty() {
        return Ok(None);
    }

    let mut builder = ToolConfiguration::builder();
    for tool in specs {
        builder = builder.tools(tool);
    }
    let cfg = builder
        .build()
        .map_err(|e| Error::Parse(format!("bedrock ToolConfiguration build: {e}")))?;
    Ok(Some(cfg))
}

/// Map a non-streaming Converse response into zunel's `LLMResponse`.
///
/// `ConverseOutput` is the operation output struct (carries
/// `stop_reason`, `usage`, and `output: Option<ConverseOutputPayload>`)
/// which itself wraps the assistant message blocks.
pub fn to_llm_response(output: ConverseOutput) -> Result<LLMResponse> {
    let stop_reason = stop_reason_to_finish_reason(&output.stop_reason);
    let usage = output
        .usage
        .as_ref()
        .map(token_usage_to_usage)
        .unwrap_or_default();

    let Some(payload) = output.output else {
        return Ok(LLMResponse {
            content: None,
            tool_calls: Vec::new(),
            usage,
            finish_reason: Some(stop_reason),
        });
    };
    let ConverseOutputPayload::Message(msg) = payload else {
        return Ok(LLMResponse {
            content: None,
            tool_calls: Vec::new(),
            usage,
            finish_reason: Some(stop_reason),
        });
    };

    let mut content_chunks: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
    for (idx, block) in msg.content.into_iter().enumerate() {
        match block {
            ContentBlock::Text(text) if !text.is_empty() => {
                content_chunks.push(text);
            }
            ContentBlock::ToolUse(tu) => {
                tool_calls.push(ToolCallRequest {
                    id: tu.tool_use_id,
                    name: tu.name,
                    arguments: document_to_json_value(&tu.input),
                    index: idx as u32,
                });
            }
            _ => {
                // Reasoning, citations, images, etc. are accepted by the
                // SDK but not surfaced to the agent loop. Drop them silently
                // so the model is free to emit them without forcing a hard
                // failure here.
            }
        }
    }

    let content = if content_chunks.is_empty() {
        None
    } else {
        Some(content_chunks.join(""))
    };

    Ok(LLMResponse {
        content,
        tool_calls,
        usage,
        finish_reason: Some(stop_reason),
    })
}

pub fn token_usage_to_usage(usage: &aws_sdk_bedrockruntime::types::TokenUsage) -> Usage {
    Usage {
        prompt_tokens: usage.input_tokens.max(0) as u32,
        completion_tokens: usage.output_tokens.max(0) as u32,
        cached_tokens: usage.cache_read_input_tokens.unwrap_or(0).max(0) as u32,
        reasoning_tokens: 0,
    }
}

/// Translate a Bedrock `StopReason` into the finish_reason strings
/// (`stop`, `length`, `tool_calls`, `content_filter`) the rest of zunel
/// already understands. Unknown future variants fall through to a
/// snake-cased rendering of `as_str` so downstream code can still log
/// them without crashing.
pub fn stop_reason_to_finish_reason(reason: &StopReason) -> String {
    match reason {
        StopReason::EndTurn => "stop".to_string(),
        StopReason::MaxTokens => "length".to_string(),
        StopReason::ToolUse => "tool_calls".to_string(),
        StopReason::ContentFiltered => "content_filter".to_string(),
        StopReason::StopSequence => "stop".to_string(),
        StopReason::GuardrailIntervened => "content_filter".to_string(),
        StopReason::ModelContextWindowExceeded => "length".to_string(),
        other => other.as_str().to_string(),
    }
}

/// Map zunel's `reasoning_effort` knob into the extended-thinking budget
/// shape Bedrock accepts via `additional_model_request_fields` for
/// thinking-capable models. Returns `None` when the model shouldn't
/// think (default), so the field is omitted entirely and non-thinking
/// models keep working unchanged.
pub fn reasoning_to_additional_fields(reasoning_effort: Option<&str>) -> Option<Document> {
    let budget = match reasoning_effort.unwrap_or("").to_ascii_lowercase().as_str() {
        "low" | "minimal" => 1024i64,
        "medium" => 4096i64,
        "high" => 16384i64,
        _ => return None,
    };
    let mut thinking = HashMap::new();
    thinking.insert("type".to_string(), Document::String("enabled".to_string()));
    thinking.insert(
        "budget_tokens".to_string(),
        Document::Number(Number::PosInt(budget as u64)),
    );
    let mut root = HashMap::new();
    root.insert("thinking".to_string(), Document::Object(thinking));
    Some(Document::Object(root))
}

/// Recursive `serde_json::Value` → `aws_smithy_types::Document`.
pub fn json_value_to_document(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i >= 0 {
                    Document::Number(Number::PosInt(i as u64))
                } else {
                    Document::Number(Number::NegInt(i))
                }
            } else if let Some(f) = n.as_f64() {
                Document::Number(Number::Float(f))
            } else {
                // Fallback: stringify (preserves arbitrary-precision
                // numbers without panicking).
                Document::String(n.to_string())
            }
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(items) => Document::Array(items.iter().map(json_value_to_document).collect()),
        Value::Object(map) => {
            let mut out = HashMap::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), json_value_to_document(v));
            }
            Document::Object(out)
        }
    }
}

/// Recursive `aws_smithy_types::Document` → `serde_json::Value`.
pub fn document_to_json_value(doc: &Document) -> Value {
    match doc {
        Document::Null => Value::Null,
        Document::Bool(b) => Value::Bool(*b),
        Document::Number(n) => match n {
            Number::PosInt(u) => Value::from(*u),
            Number::NegInt(i) => Value::from(*i),
            Number::Float(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        },
        Document::String(s) => Value::String(s.clone()),
        Document::Array(items) => Value::Array(items.iter().map(document_to_json_value).collect()),
        Document::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), document_to_json_value(v));
            }
            Value::Object(out)
        }
    }
}
