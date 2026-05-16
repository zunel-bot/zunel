use async_trait::async_trait;
use serde_json::{json, Value};

use zunel_tools::{Tool, ToolContext, ToolRegistry, ToolResult};

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "Echo the input back."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"},
            },
            "required": ["text"],
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        ToolResult::ok(text)
    }
}

#[tokio::test]
async fn registry_dispatches_registered_tool() {
    let mut registry = ToolRegistry::new();
    registry.register(std::sync::Arc::new(EchoTool));

    let ctx = ToolContext::for_test();
    let result = registry
        .execute("echo", json!({"text": "hi"}), &ctx)
        .await
        .unwrap();
    assert_eq!(result.content, "hi");
    assert!(!result.is_error);
}

#[tokio::test]
async fn registry_rejects_unknown_tool_with_hint_suffix() {
    let registry = ToolRegistry::new();
    let ctx = ToolContext::for_test();
    let result = registry.execute("nope", json!({}), &ctx).await.unwrap();
    assert!(result.is_error);
    assert!(
        result
            .content
            .ends_with("\n\n[Analyze the error above and try a different approach.]"),
        "missing hint suffix: {}",
        result.content
    );
    assert!(result.content.contains("unknown tool"));
}

#[tokio::test]
async fn registry_rejects_invalid_args_with_hint_suffix() {
    let mut registry = ToolRegistry::new();
    registry.register(std::sync::Arc::new(EchoTool));
    let ctx = ToolContext::for_test();
    let result = registry
        .execute("echo", json!({"not_text": 1}), &ctx)
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(result
        .content
        .ends_with("\n\n[Analyze the error above and try a different approach.]"));
}

#[tokio::test]
async fn unregister_removes_a_previously_registered_tool() {
    let mut registry = ToolRegistry::new();
    registry.register(std::sync::Arc::new(EchoTool));
    assert!(registry.get("echo").is_some());

    let removed = registry.unregister("echo");
    assert!(
        removed.is_some(),
        "unregister should return the dropped tool"
    );
    assert!(registry.get("echo").is_none());

    let result = registry
        .execute("echo", json!({"text": "hi"}), &ToolContext::for_test())
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("unknown tool"));
}

#[test]
fn unregister_returns_none_when_tool_was_never_registered() {
    let mut registry = ToolRegistry::new();
    assert!(registry.unregister("never_existed").is_none());
}

#[test]
fn get_definitions_orders_mcp_tools_last() {
    struct Tool1;
    struct Tool2;

    #[async_trait]
    impl Tool for Tool1 {
        fn name(&self) -> &'static str {
            "alpha"
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn parameters(&self) -> Value {
            json!({"type":"object"})
        }
        async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
            ToolResult::ok("")
        }
    }
    #[async_trait]
    impl Tool for Tool2 {
        fn name(&self) -> &'static str {
            "mcp_slack_post"
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn parameters(&self) -> Value {
            json!({"type":"object"})
        }
        async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
            ToolResult::ok("")
        }
    }

    let mut registry = ToolRegistry::new();
    registry.register(std::sync::Arc::new(Tool2));
    registry.register(std::sync::Arc::new(Tool1));
    let defs = registry.get_definitions();
    let names: Vec<&str> = defs
        .iter()
        .map(|d| d["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "mcp_slack_post"]);
}
