//! `mcp_reconnect` is a native tool registered alongside `spawn` and
//! `self`. The LLM (CLI agent or any channel like Slack) can call
//! it with `{}` to reload all configured MCP servers, or with
//! `{"server": "<name>"}` to reload one. Tests pin both code paths
//! using the same in-process stdio fixture the reload-helper tests
//! already use, so we know the tool actually mutates the live
//! registry under the same `Arc<RwLock<ToolRegistry>>` the agent
//! loop reads from.

use std::fs;
use std::sync::{Arc, RwLock};

use serde_json::{json, Value};
use tempfile::tempdir;
use zunel_core::mcp_reconnect::McpReconnectTool;
use zunel_tools::{Tool, ToolContext, ToolRegistry};

fn fixture_server_script() -> String {
    r#"
import json
import sys

def read_msg():
    header = b""
    while b"\r\n\r\n" not in header:
        chunk = sys.stdin.buffer.read(1)
        if not chunk:
            raise SystemExit(0)
        header += chunk
    length = 0
    for line in header.decode("utf-8").split("\r\n"):
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
    return json.loads(sys.stdin.buffer.read(length).decode("utf-8"))

def send(obj):
    body = json.dumps(obj, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fixture", "version": "1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [{"name": "echo", "description": "Echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}]}})
"#
    .to_string()
}

fn write_config_with_fixture(dir: &std::path::Path, script_path: &std::path::Path) {
    let cfg = json!({
        "providers": {},
        "agents": {"defaults": {"model": "m"}},
        "tools": {"mcpServers": {
            "fixture": {
                "type": "stdio",
                "command": "python3",
                "args": [script_path.to_string_lossy()],
                "tool_timeout": 5,
                "init_timeout": 5,
                "enabled_tools": ["echo"]
            }
        }}
    });
    fs::write(
        dir.join("config.json"),
        serde_json::to_string(&cfg).unwrap(),
    )
    .unwrap();
}

#[test]
fn mcp_reconnect_tool_advertises_optional_server_argument() {
    let registry = Arc::new(RwLock::new(Arc::new(ToolRegistry::new())));
    let tool = McpReconnectTool::new(registry, None);

    assert_eq!(tool.name(), "mcp_reconnect");

    let params = tool.parameters();
    assert_eq!(params.get("type").and_then(Value::as_str), Some("object"));
    let props = params
        .get("properties")
        .and_then(Value::as_object)
        .expect("parameters should expose properties");
    assert!(props.contains_key("server"));
    let required = params
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        required.is_empty(),
        "`server` must be optional so the LLM can reload everything: required={required:?}"
    );
}

#[tokio::test]
async fn mcp_reconnect_tool_targets_one_server_and_splices_into_live_registry() {
    let dir = tempdir().unwrap();
    let script_path = dir.path().join("fixture_mcp.py");
    fs::write(&script_path, fixture_server_script()).unwrap();
    write_config_with_fixture(dir.path(), &script_path);

    let registry = Arc::new(RwLock::new(Arc::new(ToolRegistry::new())));
    let tool = McpReconnectTool::new(Arc::clone(&registry), Some(dir.path().join("config.json")));

    let result = tool
        .execute(json!({"server": "fixture"}), &ToolContext::for_test())
        .await;
    assert!(!result.is_error, "tool errored: {}", result.content);

    let parsed: Value = serde_json::from_str(&result.content)
        .expect("tool result must be JSON, got: {result.content}");
    assert_eq!(
        parsed
            .get("succeeded")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_str),
        Some("fixture")
    );
    assert!(parsed
        .get("failed")
        .and_then(Value::as_array)
        .map(|arr| arr.is_empty())
        .unwrap_or(false));

    assert!(registry.read().unwrap().get("mcp_fixture_echo").is_some());
}

#[tokio::test]
async fn mcp_reconnect_tool_with_no_args_reloads_all_servers() {
    let dir = tempdir().unwrap();
    let script_path = dir.path().join("fixture_mcp.py");
    fs::write(&script_path, fixture_server_script()).unwrap();
    write_config_with_fixture(dir.path(), &script_path);

    let registry = Arc::new(RwLock::new(Arc::new(ToolRegistry::new())));
    let tool = McpReconnectTool::new(Arc::clone(&registry), Some(dir.path().join("config.json")));

    let result = tool.execute(json!({}), &ToolContext::for_test()).await;
    assert!(!result.is_error, "tool errored: {}", result.content);

    let parsed: Value = serde_json::from_str(&result.content).unwrap();
    let attempted: Vec<&str> = parsed
        .get("attempted")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        attempted.contains(&"fixture"),
        "fixture not in attempted={attempted:?}"
    );

    assert!(registry.read().unwrap().get("mcp_fixture_echo").is_some());
}

#[tokio::test]
async fn mcp_reconnect_tool_returns_error_when_target_unknown() {
    let dir = tempdir().unwrap();
    let cfg = json!({
        "providers": {},
        "agents": {"defaults": {"model": "m"}},
        "tools": {"mcpServers": {}}
    });
    fs::write(
        dir.path().join("config.json"),
        serde_json::to_string(&cfg).unwrap(),
    )
    .unwrap();

    let registry = Arc::new(RwLock::new(Arc::new(ToolRegistry::new())));
    let tool = McpReconnectTool::new(registry, Some(dir.path().join("config.json")));

    let result = tool
        .execute(json!({"server": "nope"}), &ToolContext::for_test())
        .await;
    let parsed: Value = serde_json::from_str(&result.content)
        .unwrap_or_else(|_| panic!("non-JSON result: {}", result.content));
    let failed = parsed
        .get("failed")
        .and_then(Value::as_array)
        .expect("expected `failed` array");
    assert_eq!(failed.len(), 1);
    let (name, message) = (
        failed[0][0]
            .as_str()
            .or_else(|| failed[0]["server"].as_str()),
        failed[0][1]
            .as_str()
            .or_else(|| failed[0]["error"].as_str()),
    );
    assert_eq!(name, Some("nope"));
    assert!(
        message
            .unwrap_or_default()
            .to_lowercase()
            .contains("not configured"),
        "expected `not configured` in error message, got: {message:?}"
    );
}
