//! Hot-reload of MCP tools through `AgentLoop::reload_mcp`. Pinned
//! to drive the registry through the same `Arc<RwLock<ToolRegistry>>`
//! the `process_streamed` snapshot reads from, so a successful reload
//! is visible to the next turn without re-execing the binary.

use std::fs;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::json;
use tempfile::tempdir;
use zunel_config::AgentDefaults;
use zunel_core::{AgentLoop, SessionManager};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema,
};

struct UnusedProvider;

#[async_trait]
impl LLMProvider for UnusedProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        unreachable!("provider never used in reload tests")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        unreachable!("provider never used in reload tests")
    }
}

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
    elif method == "tools/call":
        text = msg.get("params", {}).get("arguments", {}).get("text", "")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"content": [{"type": "text", "text": "echo:" + text}]}})
"#
    .to_string()
}

/// End-to-end check: an `AgentLoop` built with no MCP servers
/// configured doesn't expose `mcp_fixture_echo`, then after we
/// rewrite the on-disk config and call `reload_mcp(Some("fixture"))`
/// the live registry the loop reads from contains it. This pins the
/// "agent picks up redlab without restart" requirement at the
/// AgentLoop API surface, not just the standalone reload helper.
#[tokio::test]
async fn agent_loop_reload_mcp_picks_up_added_server_from_disk() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().to_path_buf();

    let script_path = dir.path().join("fixture_mcp.py");
    fs::write(&script_path, fixture_server_script()).unwrap();

    let config_path = dir.path().join("config.json");
    let initial_cfg = json!({
        "providers": {},
        "agents": {"defaults": {"model": "m"}},
        "tools": {"mcpServers": {}}
    });
    fs::write(&config_path, serde_json::to_string(&initial_cfg).unwrap()).unwrap();

    let provider: Arc<dyn LLMProvider> = Arc::new(UnusedProvider);
    let defaults = AgentDefaults {
        model: "m".into(),
        ..Default::default()
    };
    let agent = AgentLoop::with_sessions(provider, defaults, SessionManager::new(&workspace))
        .with_workspace(workspace.clone());

    assert!(
        agent.tools().get("mcp_fixture_echo").is_none(),
        "fixture must not be present before reload"
    );

    let updated_cfg = json!({
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
    fs::write(&config_path, serde_json::to_string(&updated_cfg).unwrap()).unwrap();

    let report = agent
        .reload_mcp(Some("fixture"), Some(&config_path))
        .await
        .expect("reload should not error");

    assert!(
        report.failed.is_empty(),
        "unexpected failures: {:?}",
        report.failed
    );
    assert_eq!(report.attempted, vec!["fixture".to_string()]);
    assert_eq!(report.succeeded, vec!["fixture".to_string()]);

    assert!(
        agent.tools().get("mcp_fixture_echo").is_some(),
        "fixture's echo tool should be live in the AgentLoop registry after reload"
    );
}

/// `tools_handle()` exposes the same `Arc<RwLock<ToolRegistry>>` that
/// the agent loop reads on each turn. Anything registered through
/// the handle (e.g. a native `mcp_reconnect` tool injected from the
/// CLI) must be visible via `agent.tools()` immediately, with no
/// further wiring.
#[tokio::test]
async fn agent_loop_tools_handle_shares_the_live_registry() {
    let dir = tempdir().unwrap();
    let provider: Arc<dyn LLMProvider> = Arc::new(UnusedProvider);
    let defaults = AgentDefaults {
        model: "m".into(),
        ..Default::default()
    };
    let agent = AgentLoop::with_sessions(provider, defaults, SessionManager::new(dir.path()))
        .with_workspace(dir.path().to_path_buf());

    let handle = agent.tools_handle();
    {
        let mut guard = handle.write().unwrap();
        Arc::make_mut(&mut *guard).register(Arc::new(zunel_tools::cron::CronTool::new(
            dir.path().join("cron").join("jobs.json"),
            "UTC",
        )));
    }

    assert!(agent.tools().get("cron").is_some());
}
