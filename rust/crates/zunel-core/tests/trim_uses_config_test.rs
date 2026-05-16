//! `trim_messages_for_provider` must honor `agents.defaults` budgets,
//! not the legacy in-runner constants. We assert two things:
//!
//! 1. A small `max_tool_result_chars` truncates oversized tool messages
//!    even when the legacy 16K floor would have left them alone.
//! 2. A small `context_window_tokens` causes `snip_history` to drop
//!    older non-system messages while keeping the most recent turn.

use zunel_config::AgentDefaults;
use zunel_core::{trim_messages_for_provider, TrimBudgets};
use zunel_providers::{ChatMessage, Role};

fn tool_message(id: &str, body: String) -> ChatMessage {
    ChatMessage {
        role: Role::Tool,
        content: body,
        tool_call_id: Some(id.into()),
        tool_calls: Vec::new(),
    }
}

#[test]
fn tool_result_chars_cap_honors_config() {
    let mut messages = vec![
        ChatMessage::user("read the file"),
        ChatMessage {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: vec![zunel_providers::ToolCallRequest {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "x.txt"}),
                index: 0,
            }],
        },
    ];
    messages.push(tool_message("call_1", "x".repeat(2_000)));
    messages.push(ChatMessage::user("ack"));

    let defaults = AgentDefaults {
        max_tool_result_chars: Some(256),
        ..Default::default()
    };
    let budgets = TrimBudgets::from_defaults(&defaults);
    let trimmed = trim_messages_for_provider(&messages, budgets).expect("trim ok");
    let tool_msg = trimmed
        .iter()
        .find(|m| matches!(m.role, Role::Tool))
        .expect("tool message preserved");
    assert!(
        tool_msg.content.len() <= 256 + 100,
        "tool body should respect max_tool_result_chars + truncation marker, got {}",
        tool_msg.content.len()
    );
    assert!(
        tool_msg
            .content
            .contains("[output truncated by tool-result budget]"),
        "expected truncation marker, got {:?}",
        tool_msg.content
    );
}

#[test]
fn context_window_tokens_cap_drops_old_messages() {
    let mut messages = Vec::new();
    for i in 0..50 {
        messages.push(ChatMessage::user(format!(
            "older message #{i} {}",
            "lorem ipsum dolor sit amet ".repeat(40)
        )));
    }
    messages.push(ChatMessage::user("most recent ping"));

    let defaults = AgentDefaults {
        max_tokens: Some(0),
        context_window_tokens: Some(4_096 + 800),
        ..Default::default()
    };
    let budgets = TrimBudgets::from_defaults(&defaults);
    let trimmed = trim_messages_for_provider(&messages, budgets).expect("trim ok");
    assert!(
        trimmed.len() < messages.len(),
        "expected snip to remove some history, got {} messages",
        trimmed.len()
    );
    assert_eq!(
        trimmed.last().unwrap().content,
        "most recent ping",
        "the latest user turn must always survive snipping",
    );
}

#[test]
fn from_defaults_falls_back_to_legacy_constants() {
    let defaults = AgentDefaults::default();
    let budgets = TrimBudgets::from_defaults(&defaults);
    assert_eq!(
        budgets.tool_result_chars,
        zunel_config::DEFAULT_TOOL_RESULT_BUDGET_CHARS
    );
    let expected_history = (zunel_config::DEFAULT_CONTEXT_WINDOW_TOKENS
        - zunel_config::DEFAULT_MAX_TOKENS_FALLBACK
        - zunel_config::HISTORY_BUDGET_HEADROOM_TOKENS) as usize;
    assert_eq!(budgets.history_tokens, expected_history);
}
