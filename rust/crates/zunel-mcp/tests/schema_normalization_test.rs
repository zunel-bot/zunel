use serde_json::json;
use zunel_mcp::normalize_schema_for_openai;

#[test]
fn normalizes_nullable_type_arrays() {
    let input = json!({
        "type": "object",
        "properties": {
            "path": {"type": ["string", "null"]},
            "count": {"type": "integer"}
        }
    });

    let normalized = normalize_schema_for_openai(input);

    assert_eq!(normalized["type"], "object");
    assert_eq!(normalized["required"], json!([]));
    assert_eq!(normalized["properties"]["path"]["type"], "string");
    assert_eq!(normalized["properties"]["path"]["nullable"], true);
}

#[test]
fn normalizes_nullable_one_of_branch() {
    let input = json!({
        "oneOf": [
            {"type": "null"},
            {"type": "string", "description": "optional text"}
        ]
    });

    let normalized = normalize_schema_for_openai(input);

    assert_eq!(normalized["type"], "string");
    assert_eq!(normalized["description"], "optional text");
    assert_eq!(normalized["nullable"], true);
    assert!(normalized.get("oneOf").is_none());
}

#[test]
fn non_object_schemas_pass_through_after_recursive_normalization() {
    let normalized = normalize_schema_for_openai(json!({
        "type": "array",
        "items": {"type": ["string", "null"]}
    }));

    assert_eq!(normalized["type"], "array");
    assert_eq!(normalized["items"]["type"], "string");
    assert_eq!(normalized["items"]["nullable"], true);
}
