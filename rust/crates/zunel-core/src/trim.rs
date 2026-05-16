//! History trim pipeline: orphan / backfill / microcompact / budget / snip.
//!
//! Operates on `Vec<Value>` matching the OpenAI wire format so the
//! trimmed output is JSON-serializable straight to disk and to the
//! provider.

use std::collections::HashSet;

use serde_json::{json, Value};

use zunel_tokens::estimate_message_tokens;

const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file",
    "exec",
    "grep",
    "glob",
    "web_search",
    "web_fetch",
    "list_dir",
];
const MICROCOMPACT_KEEP_RECENT: usize = 10;
const MICROCOMPACT_MIN_CHARS: usize = 500;
const BACKFILL_CONTENT: &str = "[Tool result unavailable — call was interrupted or lost]";
const TRUNCATION_MARKER: &str = "\n\n[output truncated by tool-result budget]";

/// Drop `role: "tool"` messages whose `tool_call_id` doesn't appear in
/// any preceding assistant `tool_calls`. Such orphans corrupt the
/// OpenAI request shape.
pub fn drop_orphan_tool_results(messages: &[Value]) -> Vec<Value> {
    let mut valid_ids: HashSet<String> = HashSet::new();
    for m in messages {
        if m.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(tool_calls) = m.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    if let Some(id) = tc.get("id").and_then(Value::as_str) {
                        valid_ids.insert(id.to_string());
                    }
                }
            }
        }
    }
    messages
        .iter()
        .filter(|m| {
            if m.get("role").and_then(Value::as_str) != Some("tool") {
                return true;
            }
            match m.get("tool_call_id").and_then(Value::as_str) {
                Some(id) => valid_ids.contains(id),
                None => false,
            }
        })
        .cloned()
        .collect()
}

/// Insert a placeholder tool message for every assistant tool call
/// that has no following result. Required because OpenAI rejects
/// histories with dangling tool calls.
pub fn backfill_missing_tool_results(messages: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for (idx, msg) in messages.iter().enumerate() {
        out.push(msg.clone());
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        let next_is_tool_for = |call_id: &str| {
            messages.iter().skip(idx + 1).any(|m| {
                m.get("role").and_then(Value::as_str) == Some("tool")
                    && m.get("tool_call_id").and_then(Value::as_str) == Some(call_id)
            })
        };
        for call in calls {
            let id = match call.get("id").and_then(Value::as_str) {
                Some(s) => s,
                None => continue,
            };
            let name = call
                .get("function")
                .and_then(Value::as_object)
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !next_is_tool_for(id) {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "name": name,
                    "content": BACKFILL_CONTENT,
                }));
            }
        }
    }
    out
}

/// Replace large historical tool results (older than the most recent
/// `MICROCOMPACT_KEEP_RECENT`) with a stub. Compaction list:
/// `read_file`, `exec`, `grep`, `glob`, `web_search`, `web_fetch`,
/// `list_dir`.
pub fn microcompact_old_tool_results(messages: &[Value]) -> Vec<Value> {
    let compactable_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.get("role").and_then(Value::as_str) == Some("tool")
                && m.get("name")
                    .and_then(Value::as_str)
                    .map(|n| COMPACTABLE_TOOLS.contains(&n))
                    .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();
    if compactable_indices.len() <= MICROCOMPACT_KEEP_RECENT {
        return messages.to_vec();
    }
    let stale_end = compactable_indices.len() - MICROCOMPACT_KEEP_RECENT;
    let stale: HashSet<usize> = compactable_indices
        .iter()
        .take(stale_end)
        .copied()
        .collect();
    let mut out: Vec<Value> = messages.to_vec();
    for idx in stale {
        let msg = &mut out[idx];
        let current_len = msg
            .get("content")
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0);
        if current_len < MICROCOMPACT_MIN_CHARS {
            continue;
        }
        let name = msg
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string();
        msg["content"] = Value::String(format!("[{name} result omitted from context]"));
    }
    out
}

/// Cap each tool message's content to `max_chars`. Truncated content
/// gets a clear marker so the model can recognise the truncation.
pub fn apply_tool_result_budget(messages: &[Value], max_chars: usize) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            if m.get("role").and_then(Value::as_str) != Some("tool") {
                return m.clone();
            }
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            if content.len() <= max_chars {
                return m.clone();
            }
            let mut new_msg = m.clone();
            let mut truncated = content[..max_chars].to_string();
            truncated.push_str(TRUNCATION_MARKER);
            new_msg["content"] = Value::String(truncated);
            new_msg
        })
        .collect()
}

/// Drop the oldest non-system messages until the total token estimate
/// fits inside `budget_tokens`. System messages are never removed and
/// the most recent non-system message is always kept so the model
/// still sees the latest user turn.
pub fn snip_history(messages: &[Value], budget_tokens: usize) -> Vec<Value> {
    let mut out: Vec<Value> = messages.to_vec();
    while estimate_message_tokens(&out) > budget_tokens {
        let last_non_system = out
            .iter()
            .rposition(|m| m.get("role").and_then(Value::as_str) != Some("system"));
        let pos = out
            .iter()
            .position(|m| m.get("role").and_then(Value::as_str) != Some("system"));
        match (pos, last_non_system) {
            (Some(p), Some(last)) if p < last => {
                out.remove(p);
            }
            _ => break,
        }
    }
    out
}

/// Bridge from the typed `ChatMessage` the runner uses to the wire-
/// shaped `Value` the trim functions expect. Assistant turns that
/// carry `tool_calls` serialize with `content: null` + an
/// OpenAI-shaped `tool_calls` array (JSON-string `arguments`).
pub fn chat_message_to_value(m: &zunel_providers::ChatMessage) -> Value {
    use zunel_providers::Role;
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut obj = serde_json::Map::new();
    obj.insert("role".into(), Value::String(role.into()));
    if m.role == Role::Assistant && !m.tool_calls.is_empty() {
        obj.insert("content".into(), Value::Null);
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default(),
                    },
                })
            })
            .collect();
        obj.insert("tool_calls".into(), Value::Array(calls));
    } else {
        obj.insert("content".into(), Value::String(m.content.clone()));
    }
    if let Some(id) = &m.tool_call_id {
        obj.insert("tool_call_id".into(), Value::String(id.clone()));
    }
    Value::Object(obj)
}

/// Inverse of `chat_message_to_value`. Tool-calls' `arguments` are
/// the OpenAI JSON-string encoding — parse them back into structured
/// JSON so the runner can redispatch.
pub fn value_to_chat_message(v: &Value) -> Result<zunel_providers::ChatMessage, String> {
    use zunel_providers::{ChatMessage, Role, ToolCallRequest};
    let role = match v.get("role").and_then(Value::as_str) {
        Some("system") => Role::System,
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("tool") => Role::Tool,
        other => return Err(format!("unknown role: {other:?}")),
    };
    let content = v
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let tool_call_id = v
        .get("tool_call_id")
        .and_then(Value::as_str)
        .map(String::from);
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
    if let Some(arr) = v.get("tool_calls").and_then(Value::as_array) {
        for (i, tc) in arr.iter().enumerate() {
            let id = tc
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let func = tc.get("function").unwrap_or(&Value::Null);
            let name = func
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args_str = func
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let arguments: Value = serde_json::from_str(args_str).unwrap_or(Value::Null);
            tool_calls.push(ToolCallRequest {
                id,
                name,
                arguments,
                index: i as u32,
            });
        }
    }
    Ok(ChatMessage {
        role,
        content,
        tool_call_id,
        tool_calls,
    })
}
