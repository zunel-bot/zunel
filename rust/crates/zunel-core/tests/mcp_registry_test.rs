use std::fs;
use std::sync::{Arc, RwLock};

use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zunel_config::{
    AgentDefaults, AgentsConfig, Config, McpServerConfig, ProvidersConfig, ToolsConfig,
};
use zunel_core::build_default_registry_async;
use zunel_core::default_tools::{reconnect_unhealthy_mcp_servers, reload_mcp_servers};
use zunel_tools::{ToolContext, ToolRegistry};

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
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [{"name": "echo", "description": "Echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}, {"name": "bad name", "description": "Invalid name", "inputSchema": {"type": "object", "properties": {}}}]}})
    elif method == "tools/call":
        text = msg.get("params", {}).get("arguments", {}).get("text", "")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"content": [{"type": "text", "text": "echo:" + text}]}})
"#
    .to_string()
}

#[tokio::test]
async fn async_default_registry_loads_stdio_mcp_tools_after_native_tools() {
    let dir = tempdir().unwrap();
    let script = dir.path().join("fixture_mcp.py");
    fs::write(&script, fixture_server_script()).unwrap();

    let mut cfg = Config {
        providers: ProvidersConfig::default(),
        agents: AgentsConfig {
            defaults: AgentDefaults {
                model: "m".into(),
                ..Default::default()
            },
        },
        channels: Default::default(),
        gateway: Default::default(),
        tools: ToolsConfig::default(),
        cli: Default::default(),
        ..Default::default()
    };
    cfg.tools.mcp_servers.insert(
        "fixture".into(),
        McpServerConfig {
            transport_type: Some("stdio".into()),
            command: Some("python3".into()),
            args: Some(vec![script.to_string_lossy().to_string()]),
            tool_timeout: Some(5),
            init_timeout: Some(5),
            enabled_tools: Some(vec!["echo".into()]),
            ..Default::default()
        },
    );

    let registry = build_default_registry_async(&cfg, dir.path()).await;
    assert!(registry.get("cron").is_some());
    assert!(registry.get("mcp_fixture_echo").is_some());
    assert!(registry.get("mcp_fixture_bad name").is_none());

    let definitions = registry.get_definitions();
    let names: Vec<&str> = definitions
        .iter()
        .map(|value| value["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.last(), Some(&"mcp_fixture_echo"));

    let result = registry
        .execute(
            "mcp_fixture_echo",
            json!({"text": "hello"}),
            &ToolContext::for_test(),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "echo:hello");
}

#[tokio::test]
async fn async_default_registry_loads_streamable_http_mcp_tools() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/list\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [{
                    "name": "remote_echo",
                    "description": "Remote echo",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
                }]
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {"content": [{"type": "text", "text": "remote:hello"}]}
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let mut cfg = Config {
        providers: ProvidersConfig::default(),
        agents: AgentsConfig {
            defaults: AgentDefaults {
                model: "m".into(),
                ..Default::default()
            },
        },
        channels: Default::default(),
        gateway: Default::default(),
        tools: ToolsConfig::default(),
        cli: Default::default(),
        ..Default::default()
    };
    cfg.tools.mcp_servers.insert(
        "remote".into(),
        McpServerConfig {
            transport_type: Some("streamableHttp".into()),
            url: Some(format!("{}/mcp", server.uri())),
            tool_timeout: Some(5),
            init_timeout: Some(5),
            enabled_tools: Some(vec!["*".into()]),
            ..Default::default()
        },
    );

    let registry = build_default_registry_async(&cfg, dir.path()).await;
    assert!(registry.get("mcp_remote_remote_echo").is_some());
    let result = registry
        .execute(
            "mcp_remote_remote_echo",
            json!({"text": "hello"}),
            &ToolContext::for_test(),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "remote:hello");
}

/// Reload should pick up a server that wasn't in the registry yet —
/// the redlab-came-online-after-startup scenario. We start with a
/// registry that has no MCP tools, add the fixture server to the
/// config, call reload, and assert the wrapped tool now exists and
/// the report flags it as succeeded.
#[tokio::test]
async fn reload_mcp_servers_picks_up_newly_added_server() {
    let dir = tempdir().unwrap();
    let script = dir.path().join("fixture_mcp.py");
    fs::write(&script, fixture_server_script()).unwrap();

    // Boot with no MCP servers configured.
    let mut cfg = empty_cfg();
    let registry = Arc::new(RwLock::new(Arc::new(
        build_default_registry_async(&cfg, dir.path()).await,
    )));
    assert!(registry.read().unwrap().get("mcp_fixture_echo").is_none());

    // Operator adds the fixture server to config (or it just came back online).
    cfg.tools.mcp_servers.insert(
        "fixture".into(),
        McpServerConfig {
            transport_type: Some("stdio".into()),
            command: Some("python3".into()),
            args: Some(vec![script.to_string_lossy().to_string()]),
            tool_timeout: Some(5),
            init_timeout: Some(5),
            enabled_tools: Some(vec!["echo".into()]),
            ..Default::default()
        },
    );

    let report = reload_mcp_servers(&registry, &cfg, None).await;
    // The auto-synthesized `zunel_self` entry resolves the parent
    // process's exe, which under `cargo test` is the test runner —
    // not the zunel CLI — so it can fail to serve. That's an
    // environmental quirk of the test, not a bug in reload, so we
    // ignore zunel_self when asserting on `failed`.
    let real_failures: Vec<_> = report
        .failed
        .iter()
        .filter(|(name, _)| name != "zunel_self")
        .collect();
    assert!(
        real_failures.is_empty(),
        "unexpected failures: {real_failures:?}"
    );
    assert!(
        report.succeeded.iter().any(|name| name == "fixture"),
        "fixture missing from succeeded={:?}",
        report.succeeded
    );

    let snapshot = registry.read().unwrap().clone();
    assert!(snapshot.get("mcp_fixture_echo").is_some());
    let result = snapshot
        .execute(
            "mcp_fixture_echo",
            json!({"text": "hello"}),
            &ToolContext::for_test(),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "echo:hello");
}

/// Targeted reload (`reload_mcp_servers(.., Some("foo"))`) must
/// only touch tools belonging to that server. Tools from other
/// servers stay registered, and tools from the target are
/// re-registered against the live MCP server.
#[tokio::test]
async fn reload_mcp_servers_targeted_does_not_drop_other_servers() {
    let dir = tempdir().unwrap();
    let script = dir.path().join("fixture_mcp.py");
    fs::write(&script, fixture_server_script()).unwrap();

    let mut cfg = empty_cfg();
    cfg.tools.mcp_servers.insert(
        "alpha".into(),
        McpServerConfig {
            transport_type: Some("stdio".into()),
            command: Some("python3".into()),
            args: Some(vec![script.to_string_lossy().to_string()]),
            tool_timeout: Some(5),
            init_timeout: Some(5),
            enabled_tools: Some(vec!["echo".into()]),
            ..Default::default()
        },
    );
    cfg.tools.mcp_servers.insert(
        "beta".into(),
        McpServerConfig {
            transport_type: Some("stdio".into()),
            command: Some("python3".into()),
            args: Some(vec![script.to_string_lossy().to_string()]),
            tool_timeout: Some(5),
            init_timeout: Some(5),
            enabled_tools: Some(vec!["echo".into()]),
            ..Default::default()
        },
    );

    let registry = Arc::new(RwLock::new(Arc::new(
        build_default_registry_async(&cfg, dir.path()).await,
    )));
    assert!(registry.read().unwrap().get("mcp_alpha_echo").is_some());
    assert!(registry.read().unwrap().get("mcp_beta_echo").is_some());

    let report = reload_mcp_servers(&registry, &cfg, Some("alpha")).await;
    assert!(report.failed.is_empty(), "failures: {:?}", report.failed);
    assert_eq!(report.attempted, vec!["alpha".to_string()]);

    let snapshot = registry.read().unwrap().clone();
    assert!(snapshot.get("mcp_alpha_echo").is_some());
    assert!(
        snapshot.get("mcp_beta_echo").is_some(),
        "targeted reload of `alpha` must not drop `beta` tools"
    );
}

/// Asking to reload a server that isn't in the config should
/// surface as a structured failure (so the slash command and the
/// MCP tool can both render a clear error) rather than silently
/// no-op.
#[tokio::test]
async fn reload_mcp_servers_unknown_target_returns_failure() {
    let dir = tempdir().unwrap();
    let cfg = empty_cfg();
    let registry = Arc::new(RwLock::new(Arc::new(
        build_default_registry_async(&cfg, dir.path()).await,
    )));

    let report = reload_mcp_servers(&registry, &cfg, Some("does-not-exist")).await;
    assert!(report.succeeded.is_empty());
    assert_eq!(report.failed.len(), 1, "report: {report:?}");
    assert_eq!(report.failed[0].0, "does-not-exist");
    assert!(
        report.failed[0].1.to_lowercase().contains("not configured"),
        "expected 'not configured' message, got {:?}",
        report.failed[0].1
    );
}

/// A reload-all that finds the fixture server unreachable should
/// not poison the registry: any MCP tools that *were* registered
/// previously stay registered. (We model "unreachable" by pointing
/// at a python script path that doesn't exist, which makes
/// StdioMcpClient::connect fail fast.)
#[tokio::test]
async fn reload_mcp_servers_failure_leaves_existing_tools_in_place_for_other_servers() {
    let dir = tempdir().unwrap();
    let working_script = dir.path().join("good.py");
    fs::write(&working_script, fixture_server_script()).unwrap();
    let missing_script = dir.path().join("does-not-exist.py");

    let mut cfg = empty_cfg();
    cfg.tools.mcp_servers.insert(
        "good".into(),
        McpServerConfig {
            transport_type: Some("stdio".into()),
            command: Some("python3".into()),
            args: Some(vec![working_script.to_string_lossy().to_string()]),
            tool_timeout: Some(5),
            init_timeout: Some(5),
            enabled_tools: Some(vec!["echo".into()]),
            ..Default::default()
        },
    );
    cfg.tools.mcp_servers.insert(
        "broken".into(),
        McpServerConfig {
            transport_type: Some("stdio".into()),
            command: Some("python3".into()),
            args: Some(vec![missing_script.to_string_lossy().to_string()]),
            tool_timeout: Some(2),
            init_timeout: Some(2),
            enabled_tools: Some(vec!["echo".into()]),
            ..Default::default()
        },
    );

    let registry = Arc::new(RwLock::new(Arc::new(ToolRegistry::new())));
    let report = reload_mcp_servers(&registry, &cfg, None).await;

    assert!(
        report.succeeded.iter().any(|name| name == "good"),
        "good should succeed: {report:?}"
    );
    assert!(
        report.failed.iter().any(|(name, _)| name == "broken"),
        "broken should be flagged failed: {report:?}"
    );
    let snapshot = registry.read().unwrap().clone();
    assert!(snapshot.get("mcp_good_echo").is_some());
    assert!(snapshot.get("mcp_broken_echo").is_none());
}

/// The auto-reconnect background task only retries servers that
/// are configured but missing from the live registry — i.e. the
/// "redlab failed at boot, then came online" case. A server that's
/// already serving tools must be skipped so the periodic tick is a
/// cheap no-op when everything's healthy.
#[tokio::test]
async fn reconnect_unhealthy_skips_servers_that_already_have_tools() {
    let dir = tempdir().unwrap();
    let script = dir.path().join("fixture_mcp.py");
    fs::write(&script, fixture_server_script()).unwrap();

    let mut cfg = empty_cfg();
    cfg.tools.mcp_servers.insert(
        "fixture".into(),
        McpServerConfig {
            transport_type: Some("stdio".into()),
            command: Some("python3".into()),
            args: Some(vec![script.to_string_lossy().to_string()]),
            tool_timeout: Some(5),
            init_timeout: Some(5),
            enabled_tools: Some(vec!["echo".into()]),
            ..Default::default()
        },
    );

    let registry = Arc::new(RwLock::new(Arc::new(
        build_default_registry_async(&cfg, dir.path()).await,
    )));
    assert!(registry.read().unwrap().get("mcp_fixture_echo").is_some());

    let report = reconnect_unhealthy_mcp_servers(&registry, &cfg).await;
    assert!(
        report.attempted.is_empty(),
        "healthy server should be skipped, got attempted={:?}",
        report.attempted
    );
}

/// The intended use case: a server that failed to register at boot
/// becomes reachable later. The periodic tick should detect it's
/// missing and reload it without anyone running `/reload`.
#[tokio::test]
async fn reconnect_unhealthy_picks_up_a_server_that_failed_to_register() {
    let dir = tempdir().unwrap();
    let script = dir.path().join("fixture_mcp.py");
    fs::write(&script, fixture_server_script()).unwrap();

    let mut cfg = empty_cfg();
    cfg.tools.mcp_servers.insert(
        "fixture".into(),
        McpServerConfig {
            transport_type: Some("stdio".into()),
            command: Some("python3".into()),
            args: Some(vec![script.to_string_lossy().to_string()]),
            tool_timeout: Some(5),
            init_timeout: Some(5),
            enabled_tools: Some(vec!["echo".into()]),
            ..Default::default()
        },
    );

    let registry = Arc::new(RwLock::new(Arc::new(ToolRegistry::new())));
    assert!(registry.read().unwrap().get("mcp_fixture_echo").is_none());

    let report = reconnect_unhealthy_mcp_servers(&registry, &cfg).await;
    assert_eq!(report.attempted, vec!["fixture".to_string()]);
    assert_eq!(report.succeeded, vec!["fixture".to_string()]);
    assert!(registry.read().unwrap().get("mcp_fixture_echo").is_some());
}

/// A server that registered an `mcp_<name>_login_required` stub is
/// in a "needs human" state, not a transient connect failure.
/// Auto-reconnect must NOT keep poking it on every tick — chat-driven
/// `mcp_login_complete` (or `zunel mcp login --force`) is the right
/// path for those.
#[tokio::test]
async fn reconnect_unhealthy_skips_servers_with_login_required_stub() {
    use std::sync::Arc as StdArc;
    use zunel_mcp::McpAuthRequiredTool;

    let dir = tempdir().unwrap();
    let mut cfg = empty_cfg();
    cfg.tools.mcp_servers.insert(
        "remote".into(),
        McpServerConfig {
            transport_type: Some("streamableHttp".into()),
            url: Some("http://localhost:9".into()),
            tool_timeout: Some(2),
            init_timeout: Some(2),
            ..Default::default()
        },
    );

    let registry = Arc::new(RwLock::new(Arc::new(
        build_default_registry_async(&cfg, dir.path()).await,
    )));
    // Pretend the OAuth path registered the auth-required stub for `remote`.
    {
        let mut guard = registry.write().unwrap();
        Arc::make_mut(&mut *guard).register(StdArc::new(McpAuthRequiredTool::new(
            "remote",
            "not_cached",
        )));
    }

    let report = reconnect_unhealthy_mcp_servers(&registry, &cfg).await;
    assert!(
        !report.attempted.iter().any(|name| name == "remote"),
        "auth-required stub should suppress auto-reconnect, attempted={:?}",
        report.attempted
    );
}

fn empty_cfg() -> Config {
    Config {
        providers: ProvidersConfig::default(),
        agents: AgentsConfig {
            defaults: AgentDefaults {
                model: "m".into(),
                ..Default::default()
            },
        },
        channels: Default::default(),
        gateway: Default::default(),
        tools: ToolsConfig::default(),
        cli: Default::default(),
        ..Default::default()
    }
}
