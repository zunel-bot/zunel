//! Verifies that `ResponsesStreamParser` extracts the `usage` block
//! from `response.completed` events and surfaces it on the terminal
//! `StreamEvent::Done` payload.

use serde_json::json;
use zunel_providers::responses::ResponsesStreamParser;
use zunel_providers::StreamEvent;

#[test]
fn extracts_usage_from_response_completed_payload() {
    let mut parser = ResponsesStreamParser::new();
    parser
        .accept(&json!({"type": "response.output_text.delta", "delta": "ok"}))
        .unwrap();
    let events = parser
        .accept(&json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "usage": {
                    "input_tokens": 123,
                    "input_tokens_details": {"cached_tokens": 12},
                    "output_tokens": 45,
                    "output_tokens_details": {"reasoning_tokens": 678},
                    "total_tokens": 846
                }
            }
        }))
        .unwrap();

    let done = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Done(resp) => Some(resp),
            _ => None,
        })
        .expect("response.completed produces a Done event");
    assert_eq!(done.usage.prompt_tokens, 123);
    assert_eq!(done.usage.completion_tokens, 45);
    assert_eq!(done.usage.cached_tokens, 12);
    assert_eq!(done.usage.reasoning_tokens, 678);
}

#[test]
fn missing_usage_block_yields_default_zero_usage() {
    let mut parser = ResponsesStreamParser::new();
    let events = parser
        .accept(&json!({
            "type": "response.completed",
            "response": {"status": "completed"}
        }))
        .unwrap();

    let done = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Done(resp) => Some(resp),
            _ => None,
        })
        .expect("response.completed produces a Done event");
    assert_eq!(done.usage.prompt_tokens, 0);
    assert_eq!(done.usage.completion_tokens, 0);
    assert_eq!(done.usage.reasoning_tokens, 0);
    assert_eq!(done.usage.cached_tokens, 0);
}
