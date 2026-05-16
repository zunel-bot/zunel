use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;

use serde_json::json;
use tempfile::tempdir;
use tokio::sync::Mutex;
use zunel_mcp::{McpToolWrapper, StdioMcpClient};
use zunel_tools::{Tool, ToolContext};

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
        send({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"message": "listing"}})
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [{"name": "echo", "description": "Echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": ["string", "null"]}}}}]}})
    elif method == "tools/call":
        text = msg.get("params", {}).get("arguments", {}).get("text", "")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"content": [{"type": "text", "text": "echo:" + text}]}})
    else:
        send({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "unknown method"}})
"#
    .to_string()
}

#[tokio::test]
async fn stdio_client_lists_and_calls_fixture_tool() {
    let dir = tempdir().unwrap();
    let script = dir.path().join("fixture_mcp.py");
    fs::write(&script, fixture_server_script()).unwrap();

    let mut client = StdioMcpClient::connect(
        "python3",
        &[script.to_string_lossy().to_string()],
        BTreeMap::new(),
        &[],
        5,
    )
    .await
    .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(
        tools[0].input_schema["properties"]["text"]["nullable"],
        true
    );

    let output = client
        .call_tool("echo", json!({"text": "hello"}), 5)
        .await
        .unwrap();
    assert_eq!(output, "echo:hello");
}

fn env_probe_script() -> String {
    // Spawns a tiny MCP server whose `tools/call` payload echoes whether
    // a specific env var was inherited from the parent process. Used by
    // the env-scrubbing regression test below.
    r#"
import json
import os
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
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "probe", "version": "1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [{"name": "envcheck", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}}}]}})
    elif method == "tools/call":
        name = msg.get("params", {}).get("arguments", {}).get("name", "")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"content": [{"type": "text", "text": os.environ.get(name, "<unset>")}]}})
    else:
        send({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "unknown"}})
"#
    .to_string()
}

#[tokio::test]
async fn stdio_child_does_not_inherit_parent_env_by_default() {
    // Set a sentinel env var on the parent, then spawn the MCP child with
    // no passthrough_env entries — the child must NOT see the value.
    // This is the regression guard for the env-leak fix: an earlier
    // revision used `Command::envs(env)` on an unscrubbed child, leaking
    // every secret (AWS_*, *_TOKEN, *_KEY) in the gateway process env to
    // every stdio MCP server on first connect.
    let sentinel_name = "ZUNEL_STDIO_ENV_SCRUB_TEST_SENTINEL";
    let sentinel_value = "leak-me-if-you-can";
    // SAFETY: tests run with `--test-threads` but this var is unique to
    // the test and not asserted-on by anything else.
    std::env::set_var(sentinel_name, sentinel_value);

    let dir = tempdir().unwrap();
    let script = dir.path().join("env_probe.py");
    fs::write(&script, env_probe_script()).unwrap();

    let mut client = StdioMcpClient::connect(
        "python3",
        &[script.to_string_lossy().to_string()],
        BTreeMap::new(),
        &[],
        5,
    )
    .await
    .unwrap();

    let leaked = client
        .call_tool("envcheck", json!({"name": sentinel_name}), 5)
        .await
        .unwrap();
    assert_eq!(
        leaked, "<unset>",
        "stdio MCP child must not inherit arbitrary parent env vars; got {leaked:?}"
    );

    // Sanity-check the baseline is still passed through so children that
    // need PATH to find e.g. python3 still work.
    let path = client
        .call_tool("envcheck", json!({"name": "PATH"}), 5)
        .await
        .unwrap();
    assert_ne!(path, "<unset>", "PATH must remain in the baseline env");

    std::env::remove_var(sentinel_name);
}

#[tokio::test]
async fn stdio_child_passthrough_env_forwards_named_vars() {
    let sentinel_name = "ZUNEL_STDIO_PASSTHROUGH_SENTINEL";
    let sentinel_value = "carry-this";
    std::env::set_var(sentinel_name, sentinel_value);

    let dir = tempdir().unwrap();
    let script = dir.path().join("env_probe.py");
    fs::write(&script, env_probe_script()).unwrap();

    let mut client = StdioMcpClient::connect(
        "python3",
        &[script.to_string_lossy().to_string()],
        BTreeMap::new(),
        &[sentinel_name.to_string()],
        5,
    )
    .await
    .unwrap();

    let seen = client
        .call_tool("envcheck", json!({"name": sentinel_name}), 5)
        .await
        .unwrap();
    assert_eq!(
        seen, sentinel_value,
        "passthrough_env entry must forward the parent value"
    );
    std::env::remove_var(sentinel_name);
}

#[tokio::test]
async fn wrapper_exposes_mcp_tool_as_zunel_tool() {
    let dir = tempdir().unwrap();
    let script = dir.path().join("fixture_mcp.py");
    fs::write(&script, fixture_server_script()).unwrap();

    let mut client = StdioMcpClient::connect(
        "python3",
        &[script.to_string_lossy().to_string()],
        BTreeMap::new(),
        &[],
        5,
    )
    .await
    .unwrap();
    let tool_def = client.list_tools(5).await.unwrap().remove(0);
    let wrapper = McpToolWrapper::new(
        "fixture",
        tool_def,
        Arc::new(Mutex::new(Box::new(client))),
        5,
    );

    assert_eq!(wrapper.name(), "mcp_fixture_echo");
    assert_eq!(wrapper.description(), "Echo text");
    assert_eq!(wrapper.parameters()["properties"]["text"]["nullable"], true);

    let result = wrapper
        .execute(json!({"text": "wrapped"}), &ToolContext::for_test())
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "echo:wrapped");
}
