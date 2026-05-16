//! End-to-end coverage of `zunel mcp agent`.
//!
//! We onboard a temporary `ZUNEL_HOME`, spawn the real `zunel` binary
//! with `mcp agent --bind 127.0.0.1:0`, scrape the bound URL from the
//! `listening on …` stdout banner, then drive the server with raw
//! HTTP requests so we can assert on transport-level details (status
//! codes, headers, JSON-RPC envelope shape) that a higher-level MCP
//! client would smooth over.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

struct AgentServer {
    child: Child,
    url: String,
    _home: TempDir,
}

impl Drop for AgentServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Runs `zunel onboard` against a fresh `ZUNEL_HOME` and returns the
/// resulting (TempDir, workspace path) pair so subsequent commands
/// can share the same environment.
fn fresh_home() -> (TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("tempdir");
    let zunel_home = home.path().to_path_buf();
    let status = std::process::Command::new(assert_cmd::cargo::cargo_bin("zunel"))
        .env("ZUNEL_HOME", &zunel_home)
        .args(["onboard"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run zunel onboard");
    assert!(status.success(), "onboard should succeed");
    let workspace = zunel_home.join("workspace");
    (home, workspace)
}

async fn spawn_agent(extra_args: &[&str]) -> AgentServer {
    let (home, _workspace) = fresh_home();
    let zunel_home = home.path().to_path_buf();
    let bin = assert_cmd::cargo::cargo_bin("zunel");
    let mut command = Command::new(bin);
    command
        .env("ZUNEL_HOME", &zunel_home)
        .arg("mcp")
        .arg("agent")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn zunel mcp agent");

    let stdout = child.stdout.take().expect("stdout pipe");
    let mut lines = BufReader::new(stdout).lines();
    // Find the listening banner; older lines may carry the warmup
    // logs if an instance-level event is emitted before the listener
    // banner.
    let mut url = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let line = match timeout(Duration::from_millis(500), lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(err)) => panic!("stdout read failed: {err:?}"),
            Err(_) => continue,
        };
        if let Some(rest) = line.strip_prefix("zunel-mcp-self listening on ") {
            url = Some(rest.trim().to_string());
            break;
        }
    }
    let url = url.expect("listener banner before deadline");
    AgentServer {
        child,
        url,
        _home: home,
    }
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
        .body(serde_json::to_vec(&body).unwrap())
        .send()
        .await
        .expect("post json")
}

async fn list_tool_names(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
) -> Vec<String> {
    let response = post_json(
        client,
        url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        headers,
    )
    .await;
    assert_eq!(response.status(), 200, "tools/list expected 200");
    let body: Value = response.json().await.expect("json body");
    body["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| {
            t["name"]
                .as_str()
                .map(ToString::to_string)
                .expect("tool name string")
        })
        .collect()
}

#[tokio::test]
async fn agent_default_exposes_read_only_registry() {
    let server = spawn_agent(&[]).await;
    let client = reqwest::Client::new();
    let names = list_tool_names(&client, &server.url, &[]).await;

    for required in ["read_file", "list_dir", "glob", "grep"] {
        assert!(
            names.iter().any(|n| n == required),
            "expected {required} to be exposed by default; got {names:?}"
        );
    }
    for blocked in [
        "write_file",
        "edit_file",
        "exec",
        "web_fetch",
        "web_search",
        "cron",
    ] {
        assert!(
            !names.iter().any(|n| n == blocked),
            "expected {blocked} to be blocked by default; got {names:?}"
        );
    }
}

#[tokio::test]
async fn agent_allow_write_unlocks_write_tools_only() {
    let server = spawn_agent(&["--allow-write"]).await;
    let client = reqwest::Client::new();
    let names = list_tool_names(&client, &server.url, &[]).await;
    for required in ["write_file", "edit_file", "cron"] {
        assert!(
            names.iter().any(|n| n == required),
            "--allow-write should expose {required}; got {names:?}"
        );
    }
    for still_blocked in ["exec", "web_fetch", "web_search"] {
        assert!(
            !names.iter().any(|n| n == still_blocked),
            "--allow-write must not unlock {still_blocked}; got {names:?}"
        );
    }
}

#[tokio::test]
async fn agent_api_key_required_when_configured() {
    let server = spawn_agent(&["--api-key", "secret-token"]).await;
    let client = reqwest::Client::new();

    let no_key = post_json(
        &client,
        &server.url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[],
    )
    .await;
    assert_eq!(no_key.status(), 401, "expected 401 without bearer token");

    let with_key = post_json(
        &client,
        &server.url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[("Authorization", "Bearer secret-token")],
    )
    .await;
    assert_eq!(
        with_key.status(),
        200,
        "expected 200 with valid bearer token"
    );
}

#[tokio::test]
async fn agent_rejects_call_depth_at_or_above_limit() {
    // max-call-depth=2 means depth 0 and 1 are allowed; 2+ is denied.
    let server = spawn_agent(&["--max-call-depth", "2"]).await;
    let client = reqwest::Client::new();

    let allowed = post_json(
        &client,
        &server.url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[("Mcp-Call-Depth", "1")],
    )
    .await;
    assert_eq!(allowed.status(), 200, "depth=1 with limit=2 should pass");

    let denied = post_json(
        &client,
        &server.url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[("Mcp-Call-Depth", "2")],
    )
    .await;
    assert_eq!(
        denied.status(),
        403,
        "depth=2 with limit=2 should be blocked"
    );
}

#[tokio::test]
async fn agent_origin_allowlist_blocks_unknown_origin() {
    let server = spawn_agent(&["--allow-origin", "https://example.com"]).await;
    let client = reqwest::Client::new();

    let unknown = post_json(
        &client,
        &server.url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[("Origin", "https://attacker.example")],
    )
    .await;
    assert_eq!(unknown.status(), 403, "unknown origin should be 403");

    let listed = post_json(
        &client,
        &server.url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[("Origin", "https://example.com")],
    )
    .await;
    assert_eq!(listed.status(), 200, "listed origin should pass");

    let missing = post_json(
        &client,
        &server.url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[],
    )
    .await;
    assert_eq!(
        missing.status(),
        200,
        "missing origin should bypass allowlist"
    );
}

#[tokio::test]
async fn agent_loopback_guard_blocks_non_loopback_bind_without_auth() {
    let home = tempfile::tempdir().expect("tempdir");
    let zunel_home = home.path().to_path_buf();
    let onboard = std::process::Command::new(assert_cmd::cargo::cargo_bin("zunel"))
        .env("ZUNEL_HOME", &zunel_home)
        .args(["onboard"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("onboard");
    assert!(onboard.success());

    // Bind to a documentation-range address that won't actually be
    // reachable; the guard should fail before bind() runs.
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("zunel"))
        .env("ZUNEL_HOME", &zunel_home)
        .args(["mcp", "agent", "--bind", "203.0.113.1:0"])
        .output()
        .expect("run agent");
    assert!(!output.status.success(), "loopback guard must fail-fast");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("non-loopback") || stderr.contains("loopback"),
        "stderr should mention loopback guard; got: {stderr}"
    );
}

#[tokio::test]
async fn agent_dispatches_read_file_tool_call_against_workspace() {
    let server = spawn_agent(&[]).await;
    let client = reqwest::Client::new();

    // The onboarded workspace ships SOUL.md with stable header text.
    let response = post_json(
        &client,
        &server.url,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "read_file",
                "arguments": {"path": "SOUL.md"}
            }
        }),
        &[],
    )
    .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json body");
    assert_eq!(
        body["result"]["isError"], false,
        "expected non-error result, body: {body}"
    );
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("content text");
    assert!(
        text.contains("SOUL"),
        "read_file should return SOUL.md content; got: {text}"
    );
}

#[tokio::test]
async fn agent_initialize_reports_instance_in_server_name() {
    let server = spawn_agent(&[]).await;
    let client = reqwest::Client::new();
    let response = post_json(
        &client,
        &server.url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
        &[],
    )
    .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json body");
    let name = body["result"]["serverInfo"]["name"]
        .as_str()
        .expect("server name");
    assert!(
        name.starts_with("zunel-agent:"),
        "expected zunel-agent:<instance>, got {name}"
    );
}

/// End-to-end coverage of the `Mcp-Call-Depth` round-trip: a real
/// `RemoteMcpClient` calling `call_tool_with_depth` against the
/// spawned `zunel mcp agent` should produce an HTTP 403 from the
/// server (which `RemoteMcpClient` surfaces as a protocol error)
/// when the depth is at or above the agent's configured cap. This
/// proves both ends of the wire are wired correctly without needing
/// to instrument the agent's internals from the test.
#[tokio::test]
async fn remote_client_depth_forwarding_round_trips_to_agent_403() {
    let server = spawn_agent(&["--max-call-depth", "2"]).await;
    let mut client = zunel_mcp::RemoteMcpClient::connect(
        &server.url,
        BTreeMap::new(),
        zunel_mcp::RemoteTransport::StreamableHttp,
        5,
    )
    .await
    .expect("connect to agent");

    // Depth 1 is below the cap of 2 → tool call must succeed.
    let ok = client
        .call_tool_with_depth("read_file", json!({"path": "SOUL.md"}), 5, Some(1))
        .await
        .expect("depth=1 call below cap should succeed");
    assert!(
        ok.contains("SOUL"),
        "expected SOUL.md content from agent; got: {ok}"
    );

    // Depth 2 is at the cap → server returns 403, RemoteMcpClient
    // raises a protocol error.
    let blocked = client
        .call_tool_with_depth("read_file", json!({"path": "SOUL.md"}), 5, Some(2))
        .await;
    let err = blocked.expect_err("depth=2 at cap=2 should be rejected");
    let rendered = err.to_string();
    assert!(
        rendered.contains("403"),
        "expected HTTP 403 protocol error, got: {rendered}"
    );
}

/// `--max-body-bytes 128` causes the agent to reject any oversize
/// request with `413 Payload Too Large`, mirroring the
/// `zunel-mcp-self` binary's behavior. Important because the agent
/// command builds its own `ServerConfig`; a regression that forgot
/// to plumb `with_max_body_bytes` would silently fall back to the
/// 4 MiB default.
#[tokio::test]
async fn agent_rejects_oversize_body_with_413() {
    let server = spawn_agent(&["--max-body-bytes", "128"]).await;
    let filler = "x".repeat(256);
    let response = reqwest::Client::new()
        .post(&server.url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "read_file", "arguments": {"path": filler}}
        }))
        .send()
        .await
        .expect("post oversize body to agent");
    assert_eq!(response.status().as_u16(), 413);
}

/// SIGTERM should drive the agent through the same drain path the
/// transport library exposes; the process must exit on its own.
/// Skipping on non-Unix because we shell out to `kill -TERM`.
#[cfg(unix)]
#[tokio::test]
async fn agent_shuts_down_cleanly_on_sigterm() {
    let mut server = spawn_agent(&[]).await;
    // Confirm the listener is up before signalling — otherwise a
    // `spawn_agent` regression could be misread as "shutdown
    // succeeded".
    let warmup = reqwest::Client::new()
        .post(&server.url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .send()
        .await
        .expect("warm-up post");
    assert_eq!(warmup.status().as_u16(), 200);

    let pid = server.child.id().expect("child has pid");
    let kill_status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("invoke kill -TERM");
    assert!(kill_status.success(), "kill -TERM failed: {kill_status:?}");

    let _exit = timeout(Duration::from_secs(8), server.child.wait())
        .await
        .expect("agent exits within 8s of SIGTERM")
        .expect("wait on child");
}

/// `--print-config` is the operator-facing helper for the
/// instance-as-MCP-server feature: it emits a paste-ready
/// `tools.mcpServers.<name>` JSON snippet for the active instance.
/// We invoke the real binary, parse stdout as JSON, and assert on
/// shape so a regression in the helper would be caught here rather
/// than via the (more expensive) cross-instance smoke test.
fn run_print_config(args: &[&str]) -> Value {
    let (home, _workspace) = fresh_home();
    let zunel_home = home.path().to_path_buf();
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("zunel"));
    command
        .env("ZUNEL_HOME", &zunel_home)
        .arg("mcp")
        .arg("agent")
        .arg("--print-config")
        .args(args);
    let output = command
        .output()
        .expect("run zunel mcp agent --print-config");
    assert!(
        output.status.success(),
        "--print-config exited non-zero: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout from --print-config must be valid JSON")
}

#[test]
fn print_config_default_instance_loopback_has_no_headers() {
    // We pin `--public-name` because the test instance's actual name
    // is derived from a randomized tempdir, which would otherwise
    // make the assertion path non-deterministic.
    let snippet = run_print_config(&["--bind", "127.0.0.1:9090", "--public-name", "helper"]);
    let entry = &snippet["tools"]["mcpServers"]["helper"];
    assert_eq!(entry["type"], "streamableHttp");
    assert_eq!(entry["url"], "http://127.0.0.1:9090/");
    assert!(
        entry.get("headers").is_none(),
        "snippet for unauthenticated loopback bind must not emit headers; got {entry:#?}"
    );
}

#[test]
fn print_config_with_api_key_emits_bearer_placeholder_only() {
    let snippet = run_print_config(&[
        "--bind",
        "127.0.0.1:9090",
        "--api-key",
        "do-not-leak-this-secret",
        "--public-name",
        "helper",
        "--public-env",
        "HELPER_TOKEN",
    ]);
    let entry = &snippet["tools"]["mcpServers"]["helper"];
    assert_eq!(
        entry["headers"]["Authorization"], "Bearer ${HELPER_TOKEN}",
        "explicit --public-env must control the placeholder; got {entry:#?}"
    );
    let raw = serde_json::to_string(&snippet).expect("re-serialize snippet");
    assert!(
        !raw.contains("do-not-leak-this-secret"),
        "raw API key must never appear in --print-config stdout; got {raw}"
    );
}

#[test]
fn print_config_short_circuits_before_loopback_guard() {
    // The loopback guard hard-fails an unauthenticated non-loopback
    // bind during `serve`, but `--print-config` exits before that
    // check runs (the snippet is the user's *plan*, not a live
    // configuration). This test fails if someone refactors the
    // guard ahead of the print-config branch.
    //
    // We supply API keys + cert/key paths that don't have to
    // actually exist on disk — print-config short-circuits before
    // TLS file loading too.
    let snippet = run_print_config(&[
        "--bind",
        "0.0.0.0:9090",
        "--api-key",
        "rotation-token",
        "--https-cert",
        "/nonexistent/cert.pem",
        "--https-key",
        "/nonexistent/key.pem",
        "--public-name",
        "helper",
    ]);
    let entry = &snippet["tools"]["mcpServers"]["helper"];
    assert_eq!(
        entry["url"], "https://<HOST>:9090/",
        "wildcard bind must render <HOST> placeholder; cert flag must select https"
    );
}

#[allow(dead_code)] // shared scaffolding; some tests don't need it
fn require_btreemap_eq<K, V>(a: &BTreeMap<K, V>, b: &BTreeMap<K, V>)
where
    K: Ord + std::fmt::Debug,
    V: PartialEq + std::fmt::Debug,
{
    assert_eq!(a, b);
}
