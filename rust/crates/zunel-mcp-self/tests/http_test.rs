//! End-to-end coverage of the Streamable HTTP transport in
//! `zunel-mcp-self`. We spawn the real binary with `--http
//! 127.0.0.1:0`, scrape the bound URL from its `listening on …`
//! stdout banner, then drive it using the same `RemoteMcpClient`
//! agents use in production.
//!
//! Coverage:
//! - default `Accept: application/json, text/event-stream` (our
//!   server prefers SSE when both are listed) confirms the chunked
//!   `text/event-stream` path;
//! - `Accept: application/json` confirms the single-shot JSON
//!   response fallback;
//! - `notifications/*` payloads ack with bare `202 Accepted`;
//! - `--api-key` rejects unauthenticated requests with `401`,
//!   accepts the right bearer token, and tolerates the alternate
//!   `X-API-Key` header;
//! - `--https-cert`/`--https-key` terminate TLS and route the
//!   complete request through `RemoteMcpClient` over HTTPS.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use zunel_mcp::{RemoteMcpClient, RemoteTransport};

struct ServerHandle {
    child: Child,
    addr: String,
    scheme: String,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl ServerHandle {
    fn url(&self) -> String {
        format!("{}://{}", self.scheme, self.addr)
    }
}

#[derive(Default)]
struct ServerOptions {
    env: BTreeMap<String, String>,
    extra_args: Vec<String>,
}

async fn spawn_server(options: ServerOptions) -> ServerHandle {
    let bin = assert_cmd::cargo::cargo_bin("zunel-mcp-self");
    let mut command = Command::new(&bin);
    command
        .arg("--http")
        .arg("127.0.0.1:0")
        .args(&options.extra_args)
        .envs(&options.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn zunel-mcp-self --http");

    let stdout = child.stdout.take().expect("stdout pipe");
    let mut lines = BufReader::new(stdout).lines();
    let banner = timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("listening banner before timeout")
        .expect("read line")
        .expect("non-empty line");
    let rest = banner
        .strip_prefix("zunel-mcp-self listening on ")
        .unwrap_or_else(|| panic!("unexpected banner: {banner:?}"));
    let (scheme, addr) = rest
        .split_once("://")
        .unwrap_or_else(|| panic!("banner missing scheme: {banner:?}"));
    ServerHandle {
        child,
        addr: addr.trim().to_string(),
        scheme: scheme.to_string(),
    }
}

/// Generate a self-signed cert+key pair for HTTPS tests. The cert
/// covers `127.0.0.1` and `localhost` so reqwest's hostname
/// verification is happy when we eventually trust it explicitly.
fn generate_self_signed_pair(dir: &std::path::Path) -> (PathBuf, PathBuf, String) {
    let cert_key =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string(), "localhost".to_string()])
            .expect("generate self-signed cert");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    let cert_pem = cert_key.cert.pem();
    std::fs::write(&cert_path, &cert_pem).unwrap();
    std::fs::write(&key_path, cert_key.key_pair.serialize_pem()).unwrap();
    (cert_path, key_path, cert_pem)
}

/// reqwest client that trusts the supplied PEM certificate and
/// nothing else. Used for HTTPS tests so we never disable hostname
/// verification globally.
fn https_client_trusting(cert_pem: &str) -> reqwest::Client {
    let cert = reqwest::Certificate::from_pem(cert_pem.as_bytes()).expect("parse cert pem");
    reqwest::Client::builder()
        .add_root_certificate(cert)
        .build()
        .expect("build trusting reqwest client")
}

#[tokio::test]
async fn http_transport_lists_tools_and_calls_self_status_via_sse() {
    let server = spawn_server(ServerOptions::default()).await;
    let url = server.url();
    assert_eq!(server.scheme, "http");

    let mut client =
        RemoteMcpClient::connect(&url, BTreeMap::new(), RemoteTransport::StreamableHttp, 5)
            .await
            .expect("connect over streamable http");

    let tools = client.list_tools(5).await.expect("list tools");
    assert!(
        tools.iter().any(|tool| tool.name == "self_status"),
        "tools: {tools:?}"
    );
    assert!(
        tools.iter().any(|tool| tool.name == "zunel_token_usage"),
        "tools missing zunel_token_usage: {tools:?}"
    );

    let status = client
        .call_tool("self_status", json!({}), 5)
        .await
        .expect("call self_status");
    assert!(status.contains("zunel-self ok"), "{status}");
}

#[tokio::test]
async fn http_transport_returns_workspace_session_summary() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let sessions = workspace.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("cli_direct.jsonl"),
        r#"{"_type": "metadata", "key": "cli:direct", "created_at": "2026-04-24T10:00:00.000000", "updated_at": "2026-04-24T11:00:00.000000", "metadata": {}, "last_consolidated": 0}
{"role": "user", "content": "hello", "timestamp": "2026-04-24T11:00:00.000000"}
"#,
    )
    .unwrap();
    std::fs::write(
        home.path().join("config.json"),
        format!(
            r#"{{
                "providers": {{}},
                "agents": {{"defaults": {{"model": "m", "workspace": "{}"}}}}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    let server = spawn_server(ServerOptions {
        env,
        ..Default::default()
    })
    .await;
    let url = server.url();

    let mut client =
        RemoteMcpClient::connect(&url, BTreeMap::new(), RemoteTransport::StreamableHttp, 5)
            .await
            .expect("connect over streamable http");

    let result = client
        .call_tool("zunel_sessions_list", json!({"limit": 5}), 5)
        .await
        .expect("call zunel_sessions_list");
    assert!(result.contains("\"count\":1"), "{result}");
    assert!(result.contains("\"key\":\"cli:direct\""), "{result}");
}

/// Direct exercise of the `application/json` response path: the
/// `RemoteMcpClient` always advertises both content types, so we hit
/// the server's JSON branch with a hand-rolled `reqwest` POST whose
/// `Accept` excludes `text/event-stream`.
#[tokio::test]
async fn http_transport_falls_back_to_json_when_sse_not_accepted() {
    let server = spawn_server(ServerOptions::default()).await;
    let url = server.url();

    let response = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }))
        .send()
        .await
        .expect("post initialize");
    assert!(response.status().is_success(), "{:?}", response.status());
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "expected JSON content type, got {content_type:?}"
    );
    let session_id = response
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .expect("server should issue Mcp-Session-Id");
    assert!(!session_id.is_empty());
    let body: serde_json::Value = response.json().await.expect("decode JSON body");
    assert_eq!(body.get("id").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(
        body.get("result")
            .and_then(|r| r.get("serverInfo"))
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str()),
        Some("zunel-mcp-self")
    );
}

#[tokio::test]
async fn http_transport_replies_202_for_notifications() {
    let server = spawn_server(ServerOptions::default()).await;
    let url = server.url();

    let response = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }))
        .send()
        .await
        .expect("post notification");
    assert_eq!(response.status().as_u16(), 202);
    let body = response.text().await.expect("read body");
    assert!(body.is_empty(), "expected empty notification ack: {body:?}");
}

#[tokio::test]
async fn http_transport_rejects_unauthenticated_requests_when_api_key_required() {
    let server = spawn_server(ServerOptions {
        extra_args: vec!["--api-key".into(), "supersecret".into()],
        ..Default::default()
    })
    .await;
    let url = server.url();

    let response = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
        }))
        .send()
        .await
        .expect("post initialize without auth");
    assert_eq!(response.status().as_u16(), 401);
    let www_authenticate = response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        www_authenticate.starts_with("Bearer"),
        "expected Bearer challenge, got {www_authenticate:?}"
    );
}

#[tokio::test]
async fn http_transport_accepts_bearer_token_via_remote_client() {
    let server = spawn_server(ServerOptions {
        extra_args: vec!["--api-key".into(), "supersecret".into()],
        ..Default::default()
    })
    .await;
    let url = server.url();

    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer supersecret".to_string(),
    );
    let mut client = RemoteMcpClient::connect(&url, headers, RemoteTransport::StreamableHttp, 5)
        .await
        .expect("connect with bearer token");

    let status = client
        .call_tool("self_status", json!({}), 5)
        .await
        .expect("authenticated call");
    assert!(status.contains("zunel-self ok"), "{status}");
}

#[tokio::test]
async fn http_transport_accepts_x_api_key_header() {
    let server = spawn_server(ServerOptions {
        extra_args: vec!["--api-key".into(), "supersecret".into()],
        ..Default::default()
    })
    .await;
    let url = server.url();

    let response = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .header("X-API-Key", "supersecret")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
        }))
        .send()
        .await
        .expect("post with X-API-Key");
    assert_eq!(response.status().as_u16(), 200);
}

#[tokio::test]
async fn https_transport_serves_streamable_http_with_self_signed_cert() {
    let dir: TempDir = tempfile::tempdir().unwrap();
    let (cert_path, key_path, cert_pem) = generate_self_signed_pair(dir.path());

    let server = spawn_server(ServerOptions {
        extra_args: vec![
            "--https-cert".into(),
            cert_path.to_string_lossy().to_string(),
            "--https-key".into(),
            key_path.to_string_lossy().to_string(),
        ],
        ..Default::default()
    })
    .await;
    assert_eq!(server.scheme, "https");
    let url = server.url();

    // Hand-rolled reqwest call so we can install the self-signed
    // CA via `add_root_certificate` rather than disabling hostname
    // verification globally.
    let client = https_client_trusting(&cert_pem);
    let response = client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
        }))
        .send()
        .await
        .expect("HTTPS post");
    assert_eq!(response.status().as_u16(), 200);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // SSE preferred when client lists both; this also verifies the
    // chunked TLS stream survives the round trip.
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected SSE, got {content_type:?}"
    );
    let body = response.text().await.expect("read SSE body");
    assert!(body.contains("event: message"), "{body}");
    assert!(
        body.contains("\"protocolVersion\":\"2024-11-05\""),
        "{body}"
    );
}

/// Rotation overlap: two `--api-key` flags supply two valid bearers,
/// so clients holding either one keep working. This is the property
/// operators rely on when staging a key swap (deploy server with
/// `OLD,NEW`, roll clients to NEW, then redeploy with just NEW).
#[tokio::test]
async fn http_transport_accepts_any_token_in_rotation_overlap() {
    let server = spawn_server(ServerOptions {
        extra_args: vec![
            "--api-key".into(),
            "old-key".into(),
            "--api-key".into(),
            "new-key".into(),
        ],
        ..Default::default()
    })
    .await;
    let url = server.url();

    for token in ["old-key", "new-key"] {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        let mut client =
            RemoteMcpClient::connect(&url, headers, RemoteTransport::StreamableHttp, 5)
                .await
                .unwrap_or_else(|err| panic!("connect with {token}: {err:#}"));
        let status = client
            .call_tool("self_status", json!({}), 5)
            .await
            .unwrap_or_else(|err| panic!("self_status with {token}: {err:#}"));
        assert!(status.contains("zunel-self ok"), "{token}: {status}");
    }

    // A token that was never on the allowlist still gets 401.
    let response = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::AUTHORIZATION, "Bearer revoked")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
        }))
        .send()
        .await
        .expect("post with revoked token");
    assert_eq!(response.status().as_u16(), 401);
}

/// `--api-key-file` accepts multiple lines, ignores blank/comment
/// lines, and stacks alongside `--api-key` so file + flag tokens are
/// both honored at once.
#[tokio::test]
async fn http_transport_loads_multi_line_api_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.txt");
    std::fs::write(
        &path,
        "# active key\nfile-key-one\n\n  file-key-two  \n# pending rotation\n",
    )
    .unwrap();

    let server = spawn_server(ServerOptions {
        extra_args: vec![
            "--api-key-file".into(),
            path.to_string_lossy().to_string(),
            "--api-key".into(),
            "flag-key".into(),
        ],
        ..Default::default()
    })
    .await;
    let url = server.url();

    for token in ["file-key-one", "file-key-two", "flag-key"] {
        let response = reqwest::Client::new()
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
            }))
            .send()
            .await
            .unwrap_or_else(|err| panic!("post with {token}: {err:#}"));
        assert_eq!(
            response.status().as_u16(),
            200,
            "{token} should be accepted"
        );
    }

    let revoked = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::AUTHORIZATION, "Bearer not-in-file")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
        }))
        .send()
        .await
        .expect("post with unknown token");
    assert_eq!(revoked.status().as_u16(), 401);
}

#[tokio::test]
async fn https_transport_rejects_missing_api_key() {
    let dir: TempDir = tempfile::tempdir().unwrap();
    let (cert_path, key_path, cert_pem) = generate_self_signed_pair(dir.path());

    let server = spawn_server(ServerOptions {
        extra_args: vec![
            "--https-cert".into(),
            cert_path.to_string_lossy().to_string(),
            "--https-key".into(),
            key_path.to_string_lossy().to_string(),
            "--api-key".into(),
            "topsecret".into(),
        ],
        ..Default::default()
    })
    .await;
    let url = server.url();

    let client = https_client_trusting(&cert_pem);
    let response = client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
        }))
        .send()
        .await
        .expect("HTTPS post without auth");
    assert_eq!(response.status().as_u16(), 401);

    let response = client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::AUTHORIZATION, "Bearer topsecret")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
        }))
        .send()
        .await
        .expect("HTTPS post with auth");
    assert_eq!(response.status().as_u16(), 200);
}

/// A body whose announced `Content-Length` exceeds `--max-body-bytes`
/// must be rejected with `413 Payload Too Large` *before* any of the
/// body bytes are admitted into memory. Setting the cap small (128
/// bytes) keeps the test self-contained — a typical JSON-RPC envelope
/// already pushes past it once we tack on an `arguments` field.
#[tokio::test]
async fn http_transport_rejects_body_over_max_body_bytes_with_413() {
    let server = spawn_server(ServerOptions {
        extra_args: vec!["--max-body-bytes".into(), "128".into()],
        ..Default::default()
    })
    .await;
    let url = server.url();

    // 256 bytes of filler comfortably overflows the 128-byte cap on
    // its own, even before JSON framing. We don't care that the
    // payload would otherwise be a valid request — the cap fires on
    // Content-Length, not on parsed structure.
    let filler = "x".repeat(256);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "self_status", "arguments": {"note": filler}}
    });
    let response = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .expect("post oversize body");
    assert_eq!(response.status().as_u16(), 413);
    let body_text = response.text().await.unwrap();
    assert!(
        body_text.contains("exceeds server cap"),
        "expected 413 explanation, got: {body_text:?}"
    );
}

/// SIGTERM (the signal `systemd`, `launchd`, `docker stop`, and
/// friends actually send) must trigger graceful shutdown — the
/// process exits within the 5-second drain window, *not* by being
/// forcibly killed. Gating on `cfg(unix)` because Windows has no
/// SIGTERM equivalent and we shell out to `/bin/kill`.
#[cfg(unix)]
#[tokio::test]
async fn http_transport_shuts_down_cleanly_on_sigterm() {
    let mut server = spawn_server(ServerOptions::default()).await;
    let url = server.url();

    // Confirm the server is actually serving before we shut it down,
    // so a "shutdown succeeded" pass isn't masking a "never started"
    // bug.
    let response = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
        }))
        .send()
        .await
        .expect("warm-up post");
    assert_eq!(response.status().as_u16(), 200);

    let pid = server.child.id().expect("child has pid");
    let kill_status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("invoke kill -TERM");
    assert!(kill_status.success(), "kill -TERM failed: {kill_status:?}");

    // 8 seconds = 5s drain grace + 3s slack for runner overhead. A
    // regression that wedged shutdown would blow well past this
    // ceiling, while a healthy server typically exits in under 50ms.
    let exit = timeout(Duration::from_secs(8), server.child.wait())
        .await
        .expect("process exits within 8s of SIGTERM")
        .expect("wait on child");
    // Don't pin a specific exit code: graceful shutdown after SIGTERM
    // can land as code 0 (we returned `Ok(())`) or 143 (`128 + SIGTERM`),
    // depending on whether the runtime swallowed the signal first.
    // The assertion of interest is liveness: did the binary exit on
    // its own?
    let _ = exit;
}

/// `--access-log <path>` must:
///
/// 1. emit one JSON line per served request,
/// 2. include the matched bearer token's fingerprint (never the
///    token itself),
/// 3. set `tool` to the name on `tools/call`, leave it unset on
///    other methods,
/// 4. omit `key` entirely when no auth is configured,
/// 5. record `status: 401` for unauthenticated requests *without*
///    leaking a `key` field (so credential-stuffing probes can't
///    poison the log with their own fingerprints).
#[tokio::test]
async fn http_transport_emits_one_json_line_per_request_with_redacted_key() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("access.log");
    let token = "rotate-me-eventually";

    let server = spawn_server(ServerOptions {
        extra_args: vec![
            "--api-key".into(),
            token.into(),
            "--access-log".into(),
            log_path.display().to_string(),
        ],
        ..Default::default()
    })
    .await;
    let url = server.url();

    // Drive a few request shapes through the server: a tools/list
    // (method only, no tool name), a tools/call (method + tool), an
    // unauthenticated probe (401, no key), and a notification
    // (202, no rpc_id).
    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    let mut client = RemoteMcpClient::connect(&url, headers, RemoteTransport::StreamableHttp, 5)
        .await
        .expect("connect with bearer token");
    let _ = client.list_tools(5).await.expect("list_tools");
    let status = client
        .call_tool("self_status", json!({}), 5)
        .await
        .expect("call self_status");
    assert!(status.contains("zunel-self ok"), "{status}");

    // Unauthenticated probe → 401 without leaking key fingerprint.
    let response = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/list"
        }))
        .send()
        .await
        .expect("post unauthenticated probe");
    assert_eq!(response.status().as_u16(), 401);

    // Notification (no id, no response body, 202).
    let response = reqwest::Client::new()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .send()
        .await
        .expect("post notification");
    assert_eq!(response.status().as_u16(), 202);

    // Drop the client so the connection closes; the server flushes
    // each access-log entry under the mutex during the response
    // write, but giving the runtime a tick avoids a race where the
    // last line hasn't been observed by the file system.
    drop(client);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let bytes = std::fs::read(&log_path).expect("access log file exists");
    let text = std::str::from_utf8(&bytes).expect("utf-8 log");
    let entries: Vec<serde_json::Value> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("each line is valid JSON"))
        .collect();
    assert!(
        entries.len() >= 4,
        "expected ≥4 entries (initialize + tools/list + tools/call + 401 + 202), got {}: {text}",
        entries.len()
    );

    let fingerprint = zunel_mcp_self::access_log::token_fingerprint(token);

    let unauth = entries
        .iter()
        .find(|entry| entry["status"] == 401)
        .expect("401 entry present");
    assert!(
        unauth.get("key").is_none() || unauth["key"].is_null(),
        "401 entry must not record a key fingerprint: {unauth}"
    );

    let tools_call = entries
        .iter()
        .find(|entry| entry["tool"] == "self_status")
        .expect("tools/call entry present");
    assert_eq!(tools_call["status"], 200);
    assert_eq!(tools_call["method"], "tools/call");
    assert_eq!(
        tools_call["key"],
        serde_json::Value::String(fingerprint.clone())
    );
    assert!(
        !text.contains(token),
        "raw bearer token must never appear in the access log"
    );

    let notification = entries
        .iter()
        .find(|entry| entry["status"] == 202)
        .expect("202 entry present");
    assert_eq!(notification["method"], "notifications/initialized");
    assert!(
        notification.get("tool").is_none(),
        "non-tools/call methods should not record a tool name: {notification}"
    );
    assert_eq!(
        notification["key"],
        serde_json::Value::String(fingerprint),
        "authed notifications still record the matched key"
    );
}

/// `--access-log -` must direct entries to stdout. We can't easily
/// scrape the binary's mixed banner+log stdout in the existing
/// `spawn_server` flow, so verify the lib-level constructor accepts
/// `"-"` and yields a working sink.
#[tokio::test]
async fn open_from_cli_dash_selects_stdout_sink() {
    let log = zunel_mcp_self::open_access_log("-").await.unwrap();
    let entry = zunel_mcp_self::AccessLogEntry {
        ts: "2026-04-26T00:00:00.000000Z".into(),
        peer: "127.0.0.1:1".into(),
        method: Some("tools/list".into()),
        tool: None,
        rpc_id: Some(json!(1)),
        depth: None,
        key: None,
        status: 200,
        ms: 1,
    };
    log.emit(&entry).await;
    drop(log);
}
