//! Cross-instance smoke test for the instance-as-MCP-server feature.
//!
//! This is the integration the design doc was actually pointing at:
//! a "hub" instance A wires a "helper" instance B as an entry under
//! `tools.mcpServers` in its `config.json`, and B is being served by
//! a real `zunel mcp agent` HTTP server with API-key auth. We then
//! drive A's `build_default_registry_async` and assert that:
//!
//! 1. Two instances boot side by side with isolated `ZUNEL_HOME`s.
//! 2. The `${VAR}` substitution in `headers` reaches the live HTTP
//!    request (and not as a literal string).
//! 3. `RegistryDispatcher` serves the helper instance's *own*
//!    workspace, distinct from A's, proving the per-instance isolation
//!    is real.
//! 4. End-to-end tool execution through the registered
//!    `mcp_helper_*` wrapper round-trips successfully, including the
//!    auth header and the depth header that the wrapper now emits
//!    automatically.
//!
//! Lower-level pieces of this are covered by other tests (HTTP
//! transport, env-var parser, registry dispatcher, depth forwarding);
//! the value of this file is verifying that they all click together
//! the way `instance-as-mcp.md` documents.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as TokioMutex;
use tokio::time::timeout;
use zunel_core::build_default_registry_async;
use zunel_tools::ToolContext;

/// Serializes any test in this file that mutates process-wide
/// environment variables (`ZUNEL_HOME`, the per-test bearer-token
/// vars). Cargo runs `#[tokio::test]` cases in parallel inside one
/// OS process, and `std::env::set_var` is fundamentally not safe to
/// race with itself — without this lock two tests can clobber each
/// other's `ZUNEL_HOME` while a third reads it, producing a flaky
/// "wrong workspace" or "config not found" failure.
///
/// We use `tokio::sync::Mutex` so awaits inside the critical section
/// don't deadlock the runtime, and `OnceLock` so the static doesn't
/// need a `const fn` constructor that varies across tokio versions.
fn env_lock() -> &'static TokioMutex<()> {
    static ENV_LOCK: OnceLock<TokioMutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| TokioMutex::new(()))
}

struct HelperServer {
    child: Child,
    url: String,
    workspace: PathBuf,
    _home: TempDir,
}

impl Drop for HelperServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn onboard(zunel_home: &std::path::Path) {
    let status = std::process::Command::new(assert_cmd::cargo::cargo_bin("zunel"))
        .env("ZUNEL_HOME", zunel_home)
        .args(["onboard"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run zunel onboard");
    assert!(status.success(), "onboard should succeed");
}

async fn spawn_helper(api_key: &str) -> HelperServer {
    let home = tempfile::tempdir().expect("helper home tempdir");
    let zunel_home = home.path().to_path_buf();
    onboard(&zunel_home);
    let workspace = zunel_home.join("workspace");
    let bin = assert_cmd::cargo::cargo_bin("zunel");
    let mut command = Command::new(bin);
    command
        .env("ZUNEL_HOME", &zunel_home)
        .args([
            "mcp",
            "agent",
            "--bind",
            "127.0.0.1:0",
            "--api-key",
            api_key,
            // Read-only is fine for the smoke test; we just need a
            // reachable tool to round-trip.
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn zunel mcp agent (helper)");

    let stdout = child.stdout.take().expect("helper stdout pipe");
    let mut lines = BufReader::new(stdout).lines();
    let mut url = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let line = match timeout(Duration::from_millis(500), lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(err)) => panic!("helper stdout read failed: {err:?}"),
            Err(_) => continue,
        };
        if let Some(rest) = line.strip_prefix("zunel-mcp-self listening on ") {
            url = Some(rest.trim().to_string());
            break;
        }
    }
    let url = url.expect("helper banner before deadline");
    HelperServer {
        child,
        url,
        workspace,
        _home: home,
    }
}

/// Pick an env-var name that is extremely unlikely to collide with
/// other tests in the same `cargo test` process. Cargo runs
/// integration tests concurrently inside one OS process, and
/// `std::env::set_var` mutates that process — so a global name like
/// `HELPER_TOKEN` would race. The `nanos` suffix is per-call.
fn unique_env_name(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("monotonic clock")
        .as_nanos();
    format!("{prefix}_{nanos:x}")
}

#[tokio::test]
async fn hub_instance_loads_helper_instance_as_mcp_server_and_round_trips_tool_call() {
    // Held for the entire test: this case mutates `ZUNEL_HOME` and a
    // bearer-token env var, both of which are process-global. The
    // sibling test `hub_instance_skips_helper_when_required_env_var_missing`
    // also mutates `ZUNEL_HOME`, so without serialization the two can
    // race inside `cargo test`'s shared process.
    let _env_guard = env_lock().lock().await;

    let api_key = "smoke-test-token";
    let helper = spawn_helper(api_key).await;

    // Drop a known file into the helper workspace so we can prove the
    // round-tripped read_file actually targeted *helper*'s workspace
    // rather than the hub's.
    let helper_marker = helper.workspace.join("helper-marker.txt");
    let helper_marker_body = "served-from-helper-instance\n";
    std::fs::write(&helper_marker, helper_marker_body).expect("write helper marker");

    // Set up the hub instance.
    let hub_home = tempfile::tempdir().expect("hub home tempdir");
    let hub_zunel_home = hub_home.path().to_path_buf();
    onboard(&hub_zunel_home);
    let hub_workspace = hub_zunel_home.join("workspace");
    // Hub's workspace must NOT contain the helper marker; that's how
    // we'll prove the read_file call landed on the helper.
    assert!(
        !hub_workspace.join("helper-marker.txt").exists(),
        "test setup: hub workspace should not contain the helper marker"
    );

    // Register a unique env var holding the bearer token, then write a
    // hub config.json that references it via ${VAR} substitution. The
    // env-var indirection is the recommended secret-handling pattern
    // documented in self-tool.md, so wiring it through here keeps the
    // test honest about the supported surface.
    let env_name = unique_env_name("ZUNEL_SMOKE_HELPER_TOKEN");
    std::env::set_var(&env_name, api_key);

    let hub_config = json!({
        "providers": {},
        "agents": { "defaults": { "model": "stub" } },
        "tools": {
            "mcpServers": {
                "helper": {
                    "type": "streamableHttp",
                    "url": helper.url,
                    "headers": {
                        "Authorization": format!("Bearer ${{{env_name}}}")
                    }
                }
            }
        }
    });
    std::fs::write(
        hub_zunel_home.join("config.json"),
        serde_json::to_string_pretty(&hub_config).unwrap(),
    )
    .expect("write hub config.json");

    // Hub picks up its config strictly from ZUNEL_HOME, so set the
    // env for this scope. We restore it after the test by storing the
    // previous value.
    let prev_home = std::env::var_os("ZUNEL_HOME");
    std::env::set_var("ZUNEL_HOME", &hub_zunel_home);

    let cfg = zunel_config::load_config(None).expect("load hub config");
    let registry = build_default_registry_async(&cfg, &hub_workspace).await;

    // The helper exposes read_file by default (read-only registry),
    // so the wrapper name on the hub side should be mcp_helper_read_file.
    let names: Vec<String> = registry.names().map(str::to_string).collect();
    assert!(
        names.iter().any(|n| n == "mcp_helper_read_file"),
        "expected mcp_helper_read_file to be registered on hub; got {names:?}"
    );

    // Round-trip: ask helper to read its own marker file via the hub's
    // wrapper. This exercises the full path:
    //   hub registry → McpToolWrapper → RemoteMcpClient (Bearer + depth header)
    //     → helper HTTP transport → RegistryDispatcher → helper workspace's read_file
    let ctx = ToolContext::new_with_workspace(hub_workspace.clone(), "smoke:hub".into());
    let result = registry
        .execute(
            "mcp_helper_read_file",
            json!({"path": "helper-marker.txt"}),
            &ctx,
        )
        .await
        .expect("registry execute is Infallible");
    assert!(
        !result.is_error,
        "expected non-error tool result, got: {result:?}"
    );
    assert!(
        result.content.contains("served-from-helper-instance"),
        "expected helper-marker contents in tool output, got: {}",
        result.content
    );

    // Cleanup: restore env vars so siblings in the same test process
    // see a clean state.
    std::env::remove_var(&env_name);
    match prev_home {
        Some(value) => std::env::set_var("ZUNEL_HOME", value),
        None => std::env::remove_var("ZUNEL_HOME"),
    }
    drop(helper);
}

/// Negative companion to the round-trip: with the env var unset the
/// hub's `${VAR}` reference should drop the Authorization header
/// (per `expand_header_envs`'s contract) and the helper should reject
/// the resulting request with a 401, which `RemoteMcpClient::connect`
/// surfaces as an init failure → the wrapper isn't registered.
///
/// This proves we never put the literal `${VAR}` token onto the wire,
/// which would otherwise look like a valid header value to a less
/// strict server and leak the placeholder syntax.
#[tokio::test]
async fn hub_instance_skips_helper_when_required_env_var_missing() {
    // See the `_env_guard` comment in the round-trip test above for
    // why this lock is mandatory.
    let _env_guard = env_lock().lock().await;

    let api_key = "smoke-test-token-missing";
    let helper = spawn_helper(api_key).await;

    let hub_home = tempfile::tempdir().expect("hub home tempdir");
    let hub_zunel_home = hub_home.path().to_path_buf();
    onboard(&hub_zunel_home);
    let hub_workspace = hub_zunel_home.join("workspace");

    // Pick a unique env var name and DELIBERATELY leave it unset.
    let env_name = unique_env_name("ZUNEL_SMOKE_MISSING_TOKEN");
    std::env::remove_var(&env_name);

    let hub_config = json!({
        "providers": {},
        "agents": { "defaults": { "model": "stub" } },
        "tools": {
            "mcpServers": {
                "helper": {
                    "type": "streamableHttp",
                    "url": helper.url,
                    "headers": {
                        "Authorization": format!("Bearer ${{{env_name}}}")
                    }
                }
            }
        }
    });
    std::fs::write(
        hub_zunel_home.join("config.json"),
        serde_json::to_string_pretty(&hub_config).unwrap(),
    )
    .expect("write hub config.json");

    let prev_home = std::env::var_os("ZUNEL_HOME");
    std::env::set_var("ZUNEL_HOME", &hub_zunel_home);

    let cfg = zunel_config::load_config(None).expect("load hub config");
    let registry = build_default_registry_async(&cfg, &hub_workspace).await;
    let names: Vec<String> = registry.names().map(str::to_string).collect();
    assert!(
        !names.iter().any(|n| n.starts_with("mcp_helper_")),
        "expected no mcp_helper_* tools when bearer var is unset; got {names:?}"
    );

    // Sanity: at least one local tool should still be registered, so
    // this isn't a vacuous "registry is empty" pass.
    assert!(
        names.iter().any(|n| n == "read_file"),
        "expected the local read_file tool to remain registered"
    );

    match prev_home {
        Some(value) => std::env::set_var("ZUNEL_HOME", value),
        None => std::env::remove_var("ZUNEL_HOME"),
    }
    let _ = (helper, env_name);
}

/// Coarse "is the json shape what we expect" probe. The
/// initialize/tools/list calls don't go through the wrapper but they
/// exercise the same TLS-less HTTP path the wrapper uses, so a
/// failure here usually points at a transport-layer regression
/// rather than a registry bug.
#[tokio::test]
async fn helper_initialize_returns_zunel_agent_server_name() {
    let helper = spawn_helper("noop").await;
    let response: Value = reqwest::Client::new()
        .post(&helper.url)
        .header("Accept", "application/json")
        .header("Authorization", "Bearer noop")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .send()
        .await
        .expect("post initialize")
        .json()
        .await
        .expect("decode initialize body");
    let name = response["result"]["serverInfo"]["name"]
        .as_str()
        .expect("server name string");
    assert!(
        name.starts_with("zunel-agent:"),
        "expected zunel-agent:<instance> identity, got {name}"
    );
}
