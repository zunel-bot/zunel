use serde_json::json;
use zunel_tools::self_tool::{SelfState, SelfTool, SubagentSummary};
use zunel_tools::{Tool, ToolContext};

#[tokio::test]
async fn self_tool_reports_safe_runtime_summary() {
    let tool = SelfTool::new(SelfState {
        model: "gpt-5.4".into(),
        provider: "codex".into(),
        workspace: "/tmp/work".into(),
        max_iterations: 12,
        current_iteration: 3,
        tools: vec!["read_file".into(), "mcp_self_status".into()],
        subagents: vec![SubagentSummary {
            id: "abc123".into(),
            label: "demo".into(),
            phase: "running".into(),
            iteration: 2,
        }],
    });

    let result = tool
        .execute(json!({"action": "check"}), &ToolContext::for_test())
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("model: gpt-5.4"));
    assert!(result.content.contains("provider: codex"));
    assert!(result.content.contains("tools: 2 registered"));
    assert!(result.content.contains("[abc123] demo"));
    assert!(!result.content.to_ascii_lowercase().contains("token"));
}

#[tokio::test]
async fn self_tool_rejects_set_action() {
    let tool = SelfTool::new(SelfState::default());
    let result = tool
        .execute(
            json!({"action": "set", "key": "model", "value": "x"}),
            &ToolContext::for_test(),
        )
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("read-only"));
}
