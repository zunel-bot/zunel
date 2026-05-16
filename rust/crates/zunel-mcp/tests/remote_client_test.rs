use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
use zunel_mcp::{Error as McpError, HeaderValue, RemoteMcpClient, RemoteTransport};

#[tokio::test]
async fn streamable_http_client_lists_and_calls_tool() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "session-1")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
                })),
        )
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
        .and(header("Mcp-Session-Id", "session-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo text",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
                }]
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .and(header("Mcp-Session-Id", "session-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {"content": [{"type": "text", "text": "echo:remote"}]}
        })))
        .mount(&server)
        .await;

    let mut client = RemoteMcpClient::connect(
        &format!("{}/mcp", server.uri()),
        BTreeMap::new(),
        RemoteTransport::StreamableHttp,
        5,
    )
    .await
    .unwrap();
    let tools = client.list_tools(5).await.unwrap();
    assert_eq!(tools[0].name, "echo");
    let result = client
        .call_tool("echo", json!({"text": "remote"}), 5)
        .await
        .unwrap();
    assert_eq!(result, "echo:remote");
}

#[tokio::test]
async fn sse_client_uses_endpoint_and_reads_streamed_responses() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: endpoint\n",
        "data: /messages\n\n",
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{}}}\n\n",
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"Echo text\",\"inputSchema\":{\"type\":\"object\",\"properties\":{}}}]}}\n\n",
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"echo:sse\"}]}}\n\n",
    );
    Mock::given(method("GET"))
        .and(path("/sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    let mut client = RemoteMcpClient::connect(
        &format!("{}/sse", server.uri()),
        BTreeMap::new(),
        RemoteTransport::Sse,
        5,
    )
    .await
    .unwrap();
    let tools = client.list_tools(5).await.unwrap();
    assert_eq!(tools[0].name, "echo");
    let result = client
        .call_tool("echo", json!({"text": "sse"}), 5)
        .await
        .unwrap();
    assert_eq!(result, "echo:sse");
}

/// `Respond` impl that captures every request's `Mcp-Call-Depth`
/// header (or its absence) and echoes the JSON-RPC request id back
/// in the response so `RemoteMcpClient` doesn't reject the reply for
/// id mismatch. The HTTP test below uses it to assert the
/// `RemoteMcpClient::call_tool_with_depth` path actually emits the
/// header — and that the no-depth path does *not* — without coupling
/// to wiremock's assertion DSL, which can't easily express
/// "header value equals N for the third request only".
struct DepthRecorder {
    captured: Arc<Mutex<Vec<Option<String>>>>,
}

impl Respond for DepthRecorder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let value = request
            .headers
            .get("Mcp-Call-Depth")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.captured.lock().expect("captured slot").push(value);
        let id = serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(serde_json::Value::Null);
        ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": "ok"}]}
        }))
    }
}

#[tokio::test]
async fn streamable_http_client_emits_call_depth_header_when_requested() {
    let server = MockServer::start().await;
    let captured: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));

    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "session-1")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
                })),
        )
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
        .and(body_string_contains("\"method\":\"tools/call\""))
        .respond_with(DepthRecorder {
            captured: captured.clone(),
        })
        .mount(&server)
        .await;

    let mut client = RemoteMcpClient::connect(
        &format!("{}/mcp", server.uri()),
        BTreeMap::new(),
        RemoteTransport::StreamableHttp,
        5,
    )
    .await
    .unwrap();

    // First call: with explicit outbound depth → header should be present.
    let _ = client
        .call_tool_with_depth("echo", json!({}), 5, Some(2))
        .await
        .unwrap();
    // Second call: legacy no-depth path → header should be absent.
    let _ = client.call_tool("echo", json!({}), 5).await.unwrap();
    // Third call: explicit None → also absent.
    let _ = client
        .call_tool_with_depth("echo", json!({}), 5, None)
        .await
        .unwrap();

    let recorded = captured.lock().expect("captured slot").clone();
    assert_eq!(recorded.len(), 3, "expected three tools/call requests");
    assert_eq!(recorded[0].as_deref(), Some("2"));
    assert!(
        recorded[1].is_none(),
        "default call_tool must not emit header"
    );
    assert!(recorded[2].is_none(), "explicit None must not emit header");
}

#[tokio::test]
async fn streamable_http_client_omits_call_depth_header_on_initialize() {
    // The connect() handshake (initialize + notifications/initialized)
    // is top-level by definition and must not synthesize a depth
    // header — that would lock out servers that gate `initialize` on
    // anonymous origin-only checks.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .and(header_exists_negate("Mcp-Call-Depth"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "session-2")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("notifications/initialized"))
        .and(header_exists_negate("Mcp-Call-Depth"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    let _ = RemoteMcpClient::connect(
        &format!("{}/mcp", server.uri()),
        BTreeMap::new(),
        RemoteTransport::StreamableHttp,
        5,
    )
    .await
    .expect("connect must succeed without emitting Mcp-Call-Depth");
}

/// `RemoteMcpClient::connect_with_auth` consults the auth provider
/// closure on every outbound request, so a refresh task that
/// rewrites `~/.zunel/mcp-oauth/<server>/token.json` shows up live
/// without a reconnect. The closure here counts invocations and
/// returns a different bearer each call to assert the property.
#[tokio::test]
async fn streamable_http_client_reads_auth_provider_per_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "session-1")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    let captured: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .respond_with(AuthRecorder {
            captured: captured.clone(),
        })
        .mount(&server)
        .await;

    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter_for_provider = counter.clone();
    let auth_provider: zunel_mcp::AuthHeaderProvider = Arc::new(move || {
        let n = counter_for_provider.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        HeaderValue::from_str(&format!("Bearer token-{}", n)).ok()
    });

    let mut client = RemoteMcpClient::connect_with_auth(
        &format!("{}/mcp", server.uri()),
        BTreeMap::new(),
        Some(auth_provider),
        RemoteTransport::StreamableHttp,
        5,
    )
    .await
    .expect("connect_with_auth must succeed");

    let _ = client
        .call_tool("noop", json!({}), 5)
        .await
        .expect("first call");
    let _ = client
        .call_tool("noop", json!({}), 5)
        .await
        .expect("second call");

    let captured = captured.lock().unwrap();
    assert!(captured.len() >= 2, "got: {captured:?}");
    assert!(
        captured
            .iter()
            .any(|val| val.as_deref() == Some("Bearer token-2")),
        "expected at least one request to use a later token; got: {captured:?}"
    );
    assert!(
        captured
            .iter()
            .any(|val| val.as_deref() == Some("Bearer token-3")),
        "expected provider to be re-evaluated on the second tools/call; got: {captured:?}"
    );
}

/// `Respond` impl that records the inbound `Authorization` header
/// for the auth-provider live-reread test above. Lifted into its
/// own type so we don't re-use `DepthRecorder` for a different
/// header (which would inflate the test's vocabulary).
struct AuthRecorder {
    captured: Arc<Mutex<Vec<Option<String>>>>,
}

impl Respond for AuthRecorder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let value = request
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.captured.lock().expect("captured slot").push(value);
        let id = serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(serde_json::Value::Null);
        ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": "ok"}]}
        }))
    }
}

/// 401 mid-call must surface as the typed [`McpError::Unauthorized`]
/// variant carrying `WWW-Authenticate`, so [`McpToolWrapper`] can
/// translate it into the `MCP_AUTH_REQUIRED:` contract string the
/// chat-driven login skill watches for. Anything else (Protocol
/// strings, opaque HTTP errors) would be invisible to the agent.
#[tokio::test]
async fn streamable_http_client_maps_401_to_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("WWW-Authenticate", r#"Bearer error="invalid_token""#),
        )
        .mount(&server)
        .await;

    let result = RemoteMcpClient::connect(
        &format!("{}/mcp", server.uri()),
        BTreeMap::from([("Authorization".to_string(), "Bearer dead".to_string())]),
        RemoteTransport::StreamableHttp,
        5,
    )
    .await;

    let Err(err) = result else {
        panic!("401 must surface as Err");
    };
    match err {
        McpError::Unauthorized { www_authenticate } => {
            assert_eq!(
                www_authenticate.as_deref(),
                Some(r#"Bearer error="invalid_token""#)
            );
        }
        other => panic!("expected McpError::Unauthorized, got {other:?}"),
    }
}

/// 401 without a `WWW-Authenticate` header still maps to
/// [`McpError::Unauthorized`] — plenty of IdPs return a bare 401 on
/// expired tokens, and the AUTH_REQUIRED flow doesn't actually need
/// the header value.
#[tokio::test]
async fn streamable_http_client_maps_bare_401_to_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let result = RemoteMcpClient::connect(
        &format!("{}/mcp", server.uri()),
        BTreeMap::new(),
        RemoteTransport::StreamableHttp,
        5,
    )
    .await;
    let Err(err) = result else {
        panic!("401 must surface as Err");
    };
    match err {
        McpError::Unauthorized { www_authenticate } => {
            assert!(
                www_authenticate.is_none(),
                "no WWW-Authenticate ⇒ None, got {www_authenticate:?}"
            );
        }
        other => panic!("expected McpError::Unauthorized, got {other:?}"),
    }
}

/// Wiremock provides `header_exists` but not its negation, so we
/// build a tiny matcher that asserts the header is absent.
fn header_exists_negate(name: &'static str) -> NotHeader {
    NotHeader { name }
}

struct NotHeader {
    name: &'static str,
}

impl wiremock::Match for NotHeader {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key(self.name)
    }
}
