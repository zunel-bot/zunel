use serde_json::json;
use zunel_providers::responses::{
    convert_messages, convert_tools, map_finish_reason, ResponsesStreamParser,
};
use zunel_providers::{ChatMessage, StreamEvent, ToolCallRequest, ToolSchema};

#[test]
fn converts_chat_messages_to_responses_input_items() {
    let messages = vec![
        ChatMessage::system("system rules"),
        ChatMessage::user("hello"),
        ChatMessage::assistant("previous answer"),
        ChatMessage::assistant_with_tool_calls(
            "",
            vec![ToolCallRequest {
                id: "call_1|fc_1".into(),
                name: "write_file".into(),
                arguments: json!({"path": "out.txt", "content": "hi"}),
                index: 0,
            }],
        ),
        ChatMessage::tool("call_1|fc_1", "ok"),
    ];

    let converted = convert_messages(&messages).unwrap();

    assert_eq!(converted.instructions, "system rules");
    assert_eq!(
        converted.input,
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "hello"}]},
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "previous answer"}],
                "status": "completed",
                "id": "msg_2"
            },
            {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "write_file",
                "arguments": "{\"path\":\"out.txt\",\"content\":\"hi\"}"
            },
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ])
    );
}

#[test]
fn converts_tool_schemas_to_responses_functions() {
    let converted = convert_tools(&[ToolSchema {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
    }]);

    assert_eq!(
        converted,
        json!([
            {
                "type": "function",
                "name": "read_file",
                "description": "Read a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        ])
    );
}

#[test]
fn parses_text_and_tool_call_responses_events_to_stream_events() {
    let mut parser = ResponsesStreamParser::new();
    let events = [
        json!({"type": "response.output_text.delta", "delta": "hel"}),
        json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "write_file",
                "arguments": ""
            }
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "call_id": "call_1",
            "delta": "{\"path\":\"out.txt\""
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "call_id": "call_1",
            "delta": "}"
        }),
        json!({"type": "response.completed", "response": {"status": "completed"}}),
    ];

    let mut out = Vec::new();
    for event in events {
        out.extend(parser.accept(&event).unwrap());
    }

    assert!(matches!(&out[0], StreamEvent::ContentDelta(text) if text == "hel"));
    assert!(matches!(
        &out[1],
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some(id),
            name: Some(name),
            arguments_fragment: None,
        } if id == "call_1|fc_1" && name == "write_file"
    ));
    assert!(matches!(
        &out[2],
        StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments_fragment: Some(fragment),
        } if fragment == "{\"path\":\"out.txt\""
    ));
    assert!(matches!(
        out.last().unwrap(),
        StreamEvent::Done(resp)
            if resp.content.as_deref() == Some("hel")
                && resp.finish_reason.as_deref() == Some("stop")
    ));
}

#[test]
fn emits_arguments_delta_when_arguments_only_arrive_on_output_item_done() {
    let mut parser = ResponsesStreamParser::new();

    let added = parser
        .accept(&json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "write_file",
                "arguments": ""
            }
        }))
        .unwrap();
    assert_eq!(added.len(), 1);

    let done = parser
        .accept(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "write_file",
                "arguments": "{\"path\":\"late.txt\"}"
            }
        }))
        .unwrap();

    assert!(matches!(
        &done[0],
        StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments_fragment: Some(fragment),
        } if fragment == "{\"path\":\"late.txt\"}"
    ));
}

#[test]
fn maps_responses_status_to_chat_finish_reason() {
    assert_eq!(map_finish_reason(Some("completed")), "stop");
    assert_eq!(map_finish_reason(Some("incomplete")), "length");
    assert_eq!(map_finish_reason(Some("failed")), "error");
    assert_eq!(map_finish_reason(Some("cancelled")), "error");
    assert_eq!(map_finish_reason(None), "stop");
}

#[test]
fn response_failed_event_returns_error() {
    let mut parser = ResponsesStreamParser::new();
    let err = parser
        .accept(&json!({
            "type": "response.failed",
            "error": {"message": "bad auth"}
        }))
        .unwrap_err()
        .to_string();
    assert!(err.contains("Response failed"), "{err}");
    assert!(err.contains("bad auth"), "{err}");
}
