//! Token counting wrapper around `tiktoken-rs`.
//!
//! The tokenizer is hardcoded to `cl100k_base`, the standard
//! OpenAI-compatible tokenizer. If a provider ships its own native
//! tokenizer it overrides via `estimate_prompt_tokens_chain`.

use serde_json::Value;
use tiktoken_rs::cl100k_base_singleton;

/// Approximate message-format overhead, matching OpenAI's documented
/// `tokens_per_message = 3` for gpt-3.5+ cl100k models.
const TOKENS_PER_MESSAGE: usize = 3;
/// Additional reply priming per OpenAI docs.
const TOKENS_REPLY_PRIMING: usize = 3;

/// Token count of a single string. Returns 0 on empty input.
pub fn estimate_prompt_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        cl100k_base_singleton().encode_ordinary(text).len()
    }
}

/// Token count of a list of OpenAI-shaped chat messages.
///
/// Mirrors the tally used by `zunel/utils/helpers.py::estimate_message_tokens`:
/// every message pays a 3-token overhead plus the tokens consumed by
/// `role`, `content`, `name`, `tool_call_id`, and any `tool_calls`
/// (id + function.name + function.arguments). Unknown fields are
/// ignored. A final 3-token reply priming is added once.
pub fn estimate_message_tokens(messages: &[Value]) -> usize {
    let enc = cl100k_base_singleton();
    let mut total = 0usize;
    for msg in messages {
        total += TOKENS_PER_MESSAGE;
        let Some(obj) = msg.as_object() else { continue };
        for (key, value) in obj {
            match key.as_str() {
                "content" | "role" | "name" | "tool_call_id" => {
                    if let Some(s) = value.as_str() {
                        total += enc.encode_ordinary(s).len();
                    }
                }
                "tool_calls" => {
                    let Some(arr) = value.as_array() else {
                        continue;
                    };
                    for call in arr {
                        let Some(call_obj) = call.as_object() else {
                            continue;
                        };
                        if let Some(id) = call_obj.get("id").and_then(Value::as_str) {
                            total += enc.encode_ordinary(id).len();
                        }
                        if let Some(func) = call_obj.get("function").and_then(Value::as_object) {
                            if let Some(name) = func.get("name").and_then(Value::as_str) {
                                total += enc.encode_ordinary(name).len();
                            }
                            if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                                total += enc.encode_ordinary(args).len();
                            }
                        }
                    }
                }
                _ => { /* ignore unknown fields */ }
            }
        }
    }
    total + TOKENS_REPLY_PRIMING
}

/// Two-stage estimator: use the provider's native `estimate_prompt_tokens`
/// if it returns `Some`, otherwise fall back to the local cl100k estimate.
///
/// Mirrors `zunel/utils/helpers.py::estimate_prompt_tokens_chain`.
pub fn estimate_prompt_tokens_chain<F>(messages: &[Value], provider_estimate: F) -> usize
where
    F: FnOnce(&[Value]) -> Option<usize>,
{
    provider_estimate(messages).unwrap_or_else(|| estimate_message_tokens(messages))
}
