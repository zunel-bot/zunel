use serde_json::json;

use zunel_tokens::{estimate_message_tokens, estimate_prompt_tokens, estimate_prompt_tokens_chain};

#[test]
fn empty_prompt_has_zero_tokens() {
    assert_eq!(estimate_prompt_tokens(""), 0);
}

#[test]
fn ascii_prompt_counts_as_expected_for_cl100k() {
    // "hello world" -> 2 tokens under cl100k_base.
    assert_eq!(estimate_prompt_tokens("hello world"), 2);
}

#[test]
fn non_ascii_prompt_is_counted_with_multibyte_safety() {
    // We do not assert a specific count (tokenizer-version sensitive),
    // only that non-ASCII text yields >0 tokens and no panic.
    let tokens = estimate_prompt_tokens("café 🎉 hello");
    assert!(tokens > 0);
}

#[test]
fn estimate_message_tokens_sums_role_and_content() {
    let msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": "hello"}),
    ];
    let total = estimate_message_tokens(&msgs);
    let content_only = estimate_prompt_tokens("You are helpful.")
        + estimate_prompt_tokens("hi")
        + estimate_prompt_tokens("hello");
    assert!(
        total >= content_only,
        "total={total} content_only={content_only}"
    );
}

#[test]
fn estimate_message_tokens_handles_tool_messages() {
    let msgs = vec![json!({
        "role": "tool",
        "tool_call_id": "call_abc",
        "name": "read_file",
        "content": "file body"
    })];
    let total = estimate_message_tokens(&msgs);
    assert!(total > 0);
}

#[test]
fn estimate_message_tokens_handles_assistant_tool_calls() {
    let msgs = vec![json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": "call_abc",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"x\"}"}
        }]
    })];
    let total = estimate_message_tokens(&msgs);
    assert!(total > 0);
}

#[test]
fn chain_prefers_provider_estimate_when_present() {
    let msgs = vec![json!({"role": "user", "content": "hi"})];
    let from_chain = estimate_prompt_tokens_chain(&msgs, |_| Some(42));
    assert_eq!(from_chain, 42);
}

#[test]
fn chain_falls_back_to_local_estimate_when_provider_returns_none() {
    let msgs = vec![json!({"role": "user", "content": "hello"})];
    let local = estimate_message_tokens(&msgs);
    let chained = estimate_prompt_tokens_chain(&msgs, |_| None);
    assert_eq!(chained, local);
}
