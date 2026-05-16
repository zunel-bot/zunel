//! Pure-function tests for the Bedrock wire mapping.
//!
//! Exercises [`zunel_providers::bedrock::wire`] without instantiating
//! an `aws_sdk_bedrockruntime::Client`, so these run in CI without
//! requiring AWS credentials.

use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, SystemContentBlock, Tool, ToolInputSchema,
    ToolResultContentBlock,
};
use aws_smithy_types::{Document, Number};
use serde_json::json;

use zunel_providers::bedrock::wire::{
    convert_messages, convert_tools, document_to_json_value, json_value_to_document,
    reasoning_to_additional_fields, stop_reason_to_finish_reason,
};
use zunel_providers::{ChatMessage, ToolCallRequest, ToolSchema};

#[test]
fn lifts_system_messages_into_system_field() {
    let messages = [
        ChatMessage::system("you are zunel"),
        ChatMessage::user("hi"),
        ChatMessage::system("more system context"),
        ChatMessage::assistant("hello"),
    ];
    let converted = convert_messages(&messages).expect("convert");
    assert_eq!(converted.system.len(), 2);
    let first = match &converted.system[0] {
        SystemContentBlock::Text(s) => s.clone(),
        other => panic!("expected Text system block, got {other:?}"),
    };
    assert_eq!(first, "you are zunel");
    assert_eq!(converted.messages.len(), 2);
    assert!(matches!(converted.messages[0].role, ConversationRole::User));
    assert!(matches!(
        converted.messages[1].role,
        ConversationRole::Assistant
    ));
}

#[test]
fn drops_empty_system_and_user_blocks() {
    let messages = [
        ChatMessage::system(""),
        ChatMessage::user(""),
        ChatMessage::user("real"),
    ];
    let converted = convert_messages(&messages).expect("convert");
    assert!(
        converted.system.is_empty(),
        "empty system should not produce a block"
    );
    assert_eq!(
        converted.messages.len(),
        1,
        "empty user message should be dropped, only the real one survives"
    );
}

#[test]
fn coalesces_consecutive_tool_results_into_one_user_message() {
    // Assistant emits two parallel tool calls; the agent loop replies
    // with two Role::Tool rows in a row. Bedrock requires alternating
    // user/assistant turns, so they must collapse into a single
    // User message carrying both ToolResult blocks.
    let messages = [
        ChatMessage::user("kick off"),
        ChatMessage::assistant_with_tool_calls(
            "",
            vec![
                ToolCallRequest {
                    id: "call_1".into(),
                    name: "echo".into(),
                    arguments: json!({"x": 1}),
                    index: 0,
                },
                ToolCallRequest {
                    id: "call_2".into(),
                    name: "echo".into(),
                    arguments: json!({"x": 2}),
                    index: 1,
                },
            ],
        ),
        ChatMessage::tool("call_1", "result one"),
        ChatMessage::tool("call_2", "result two"),
        ChatMessage::user("now what"),
    ];
    let converted = convert_messages(&messages).expect("convert");

    // Expect: user, assistant(text+toolUse+toolUse), user(toolResult+toolResult), user
    assert_eq!(converted.messages.len(), 4);
    assert!(matches!(converted.messages[0].role, ConversationRole::User));
    assert!(matches!(
        converted.messages[1].role,
        ConversationRole::Assistant
    ));
    assert!(matches!(converted.messages[2].role, ConversationRole::User));
    assert_eq!(
        converted.messages[2].content.len(),
        2,
        "two tool results should coalesce into one User message"
    );
    for block in &converted.messages[2].content {
        match block {
            ContentBlock::ToolResult(tr) => {
                assert!(
                    tr.tool_use_id == "call_1" || tr.tool_use_id == "call_2",
                    "unexpected tool_use_id {}",
                    tr.tool_use_id
                );
                let text_block = tr.content.first().expect("tool result content");
                match text_block {
                    ToolResultContentBlock::Text(s) => {
                        assert!(s == "result one" || s == "result two");
                    }
                    other => panic!("expected Text tool result, got {other:?}"),
                }
            }
            other => panic!("expected ToolResult block, got {other:?}"),
        }
    }
    assert!(matches!(converted.messages[3].role, ConversationRole::User));
}

#[test]
fn assistant_tool_call_round_trip_carries_args_as_document() {
    let messages = [ChatMessage::assistant_with_tool_calls(
        "thinking",
        vec![ToolCallRequest {
            id: "call_x".into(),
            name: "search".into(),
            arguments: json!({"q": "claude bedrock", "limit": 5}),
            index: 0,
        }],
    )];
    let converted = convert_messages(&messages).expect("convert");
    let assistant = &converted.messages[0];
    assert_eq!(assistant.content.len(), 2);
    let tool_use = assistant
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::ToolUse(tu) => Some(tu),
            _ => None,
        })
        .expect("tool use block");
    assert_eq!(tool_use.tool_use_id, "call_x");
    assert_eq!(tool_use.name, "search");
    let round_tripped = document_to_json_value(&tool_use.input);
    assert_eq!(round_tripped, json!({"q": "claude bedrock", "limit": 5}));
}

#[test]
fn tool_messages_without_id_error_loudly() {
    // Manually construct a Role::Tool with no tool_call_id to
    // simulate corrupted upstream state. Should hard-fail rather than
    // silently send a malformed Bedrock request.
    let mut bad = ChatMessage::user("placeholder");
    bad.role = zunel_providers::Role::Tool;
    bad.content = "orphan".into();
    bad.tool_call_id = None;
    let err = convert_messages(&[bad]).expect_err("missing tool_call_id");
    assert!(
        err.to_string().contains("tool_call_id"),
        "error mentions the missing field: {err}"
    );
}

#[test]
fn convert_tools_returns_none_for_empty_or_unnamed() {
    let none_present: Vec<ToolSchema> = Vec::new();
    assert!(convert_tools(&none_present).expect("no tools").is_none());

    let only_unnamed = [ToolSchema {
        name: String::new(),
        description: "ignored".into(),
        parameters: json!({}),
    }];
    assert!(
        convert_tools(&only_unnamed).expect("filtered").is_none(),
        "tools with empty names are dropped (matches openai-compat behavior)"
    );
}

#[test]
fn convert_tools_preserves_name_description_and_schema() {
    let tools = [ToolSchema {
        name: "ping".into(),
        description: "send a ping".into(),
        parameters: json!({"type": "object", "properties": {"target": {"type": "string"}}}),
    }];
    let cfg = convert_tools(&tools)
        .expect("valid tools")
        .expect("non-empty");
    assert_eq!(cfg.tools.len(), 1);
    let tool_spec = match &cfg.tools[0] {
        Tool::ToolSpec(spec) => spec,
        other => panic!("expected ToolSpec, got {other:?}"),
    };
    assert_eq!(tool_spec.name, "ping");
    assert_eq!(tool_spec.description.as_deref(), Some("send a ping"));
    let schema_doc = match tool_spec.input_schema.as_ref() {
        Some(ToolInputSchema::Json(doc)) => doc,
        other => panic!("expected Json schema, got {other:?}"),
    };
    let round = document_to_json_value(schema_doc);
    assert_eq!(
        round,
        json!({"type": "object", "properties": {"target": {"type": "string"}}})
    );
}

#[test]
fn reasoning_effort_maps_to_extended_thinking_budget() {
    assert!(reasoning_to_additional_fields(None).is_none());
    assert!(reasoning_to_additional_fields(Some("")).is_none());
    assert!(reasoning_to_additional_fields(Some("unknown")).is_none());

    for (effort, want_budget) in [("low", 1024u64), ("medium", 4096), ("high", 16384)] {
        let doc = reasoning_to_additional_fields(Some(effort))
            .unwrap_or_else(|| panic!("{effort} should map"));
        let Document::Object(root) = doc else {
            panic!("root should be Object");
        };
        let Document::Object(thinking) = root.get("thinking").expect("thinking key").clone() else {
            panic!("thinking should be Object");
        };
        let Document::String(kind) = thinking.get("type").expect("type key").clone() else {
            panic!("type should be String");
        };
        assert_eq!(kind, "enabled", "extended thinking is enabled for {effort}");
        let Document::Number(Number::PosInt(budget)) =
            thinking.get("budget_tokens").expect("budget").clone()
        else {
            panic!("budget should be PosInt");
        };
        assert_eq!(
            budget, want_budget,
            "{effort} maps to {want_budget} budget tokens"
        );
    }
}

#[test]
fn stop_reason_maps_into_openai_finish_reasons() {
    use aws_sdk_bedrockruntime::types::StopReason;
    assert_eq!(stop_reason_to_finish_reason(&StopReason::EndTurn), "stop");
    assert_eq!(
        stop_reason_to_finish_reason(&StopReason::MaxTokens),
        "length"
    );
    assert_eq!(
        stop_reason_to_finish_reason(&StopReason::ToolUse),
        "tool_calls"
    );
    assert_eq!(
        stop_reason_to_finish_reason(&StopReason::ContentFiltered),
        "content_filter"
    );
    assert_eq!(
        stop_reason_to_finish_reason(&StopReason::GuardrailIntervened),
        "content_filter"
    );
    assert_eq!(
        stop_reason_to_finish_reason(&StopReason::ModelContextWindowExceeded),
        "length"
    );
    assert_eq!(
        stop_reason_to_finish_reason(&StopReason::StopSequence),
        "stop"
    );
}

#[test]
fn json_document_round_trip_preserves_shape() {
    let payload = json!({
        "string": "hi",
        "int": 7,
        "neg": -3,
        "float": 1.5,
        "bool": true,
        "null": null,
        "arr": [1, "two", false, null, {"nested": [1, 2]}],
        "obj": {"k": "v"}
    });
    let doc = json_value_to_document(&payload);
    let back = document_to_json_value(&doc);
    assert_eq!(back, payload);
}
