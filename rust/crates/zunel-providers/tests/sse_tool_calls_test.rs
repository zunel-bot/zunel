use serde_json::json;

use zunel_providers::{StreamEvent, ToolCallAccumulator, ToolCallRequest};

#[test]
fn accumulator_reassembles_single_tool_call_from_two_chunks() {
    let mut acc = ToolCallAccumulator::default();
    acc.push(StreamEvent::ToolCallDelta {
        index: 0,
        id: Some("call_abc".into()),
        name: Some("read_file".into()),
        arguments_fragment: Some(r#"{"path": "/tmp/"#.into()),
    });
    acc.push(StreamEvent::ToolCallDelta {
        index: 0,
        id: None,
        name: None,
        arguments_fragment: Some(r#"README.md"}"#.into()),
    });

    let calls = acc.finalize().expect("finalize");
    assert_eq!(calls.len(), 1);
    let ToolCallRequest {
        id,
        name,
        arguments,
        ..
    } = &calls[0];
    assert_eq!(id, "call_abc");
    assert_eq!(name, "read_file");
    assert_eq!(arguments, &json!({"path": "/tmp/README.md"}));
}

#[test]
fn accumulator_handles_two_parallel_tool_calls_interleaved() {
    let mut acc = ToolCallAccumulator::default();
    // OpenAI streams tool_calls with an `index` to disambiguate.
    acc.push(StreamEvent::ToolCallDelta {
        index: 0,
        id: Some("call_a".into()),
        name: Some("list_dir".into()),
        arguments_fragment: Some(r#"{"path":"."}"#.into()),
    });
    acc.push(StreamEvent::ToolCallDelta {
        index: 1,
        id: Some("call_b".into()),
        name: Some("glob".into()),
        arguments_fragment: Some(r#"{"pattern":"*.rs"}"#.into()),
    });

    let calls = acc.finalize().expect("finalize");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "call_a");
    assert_eq!(calls[0].name, "list_dir");
    assert_eq!(calls[1].id, "call_b");
    assert_eq!(calls[1].name, "glob");
}

#[test]
fn accumulator_ignores_content_delta_events() {
    let mut acc = ToolCallAccumulator::default();
    acc.push(StreamEvent::ContentDelta("hello".into()));
    assert!(acc.finalize().unwrap().is_empty());
}

#[test]
fn accumulator_rejects_partial_json_on_finalize() {
    let mut acc = ToolCallAccumulator::default();
    acc.push(StreamEvent::ToolCallDelta {
        index: 0,
        id: Some("call_bad".into()),
        name: Some("read_file".into()),
        arguments_fragment: Some(r#"{"path":"/broken"#.into()),
    });
    let err = acc.finalize().unwrap_err();
    assert!(err.to_string().contains("call_bad"), "got {err}");
}
