use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Duration};
use url::Url;

use crate::schema::normalize_schema_for_openai;
use crate::stdio::render_call_result;
use crate::{Error, McpClient, McpToolDefinition, Result};

/// Read each request's `Authorization` header lazily, so a refresh
/// task that rewrites `~/.zunel/mcp-oauth/<server>/token.json` while
/// the gateway is running picks up the new value on the *next* MCP
/// call without having to tear down and re-`connect()` the client.
///
/// The closure returns `None` to mean "drop the header for this
/// request" (used when the cache file is missing or unreadable).
/// `static_headers` already on the client always go on the wire;
/// the provider only contributes the dynamic `Authorization`.
pub type AuthHeaderProvider = Arc<dyn Fn() -> Option<HeaderValue> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTransport {
    StreamableHttp,
    Sse,
}

impl RemoteTransport {
    pub fn from_config(value: Option<&str>) -> Self {
        match value.unwrap_or("streamableHttp") {
            "sse" => Self::Sse,
            _ => Self::StreamableHttp,
        }
    }
}

pub struct RemoteMcpClient {
    transport: RemoteTransport,
    client: reqwest::Client,
    url: String,
    endpoint: Option<String>,
    static_headers: HeaderMap,
    auth_provider: Option<AuthHeaderProvider>,
    session_id: Option<String>,
    next_id: u64,
    sse_rx: Option<mpsc::Receiver<Value>>,
}

impl RemoteMcpClient {
    pub async fn connect(
        url: &str,
        headers: BTreeMap<String, String>,
        transport: RemoteTransport,
        init_timeout_secs: u64,
    ) -> Result<Self> {
        Self::connect_with_auth(url, headers, None, transport, init_timeout_secs).await
    }

    /// Connect with a live auth-header provider that the client
    /// consults on every outbound request. Used by the gateway to
    /// pick up tokens rewritten by the periodic refresh task without
    /// reconnecting; `connect()` (above) is sugar for "no provider".
    pub async fn connect_with_auth(
        url: &str,
        headers: BTreeMap<String, String>,
        auth_provider: Option<AuthHeaderProvider>,
        transport: RemoteTransport,
        init_timeout_secs: u64,
    ) -> Result<Self> {
        let mut client = Self {
            transport,
            client: zunel_util::default_reqwest_client(false),
            url: url.trim_end_matches('/').to_string(),
            endpoint: None,
            static_headers: headers_to_map(headers)?,
            auth_provider,
            session_id: None,
            next_id: 1,
            sse_rx: None,
        };
        if transport == RemoteTransport::Sse {
            timeout(
                Duration::from_secs(init_timeout_secs),
                client.start_sse_session(),
            )
            .await
            .map_err(|_| Error::Timeout(init_timeout_secs))??;
        }
        timeout(
            Duration::from_secs(init_timeout_secs),
            client.request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "zunel-rust", "version": env!("CARGO_PKG_VERSION")}
                }),
            ),
        )
        .await
        .map_err(|_| Error::Timeout(init_timeout_secs))??;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    /// Compose `static_headers + auth_provider()` into a fresh
    /// [`HeaderMap`] for one outbound request. The provider's
    /// `None` is honored as "drop the Authorization header" so
    /// the gateway can intentionally clear a stale bearer between
    /// refreshes if it wants to (current callers don't, but the
    /// shape is the right one).
    fn outbound_headers(&self) -> HeaderMap {
        let mut headers = self.static_headers.clone();
        if let Some(provider) = &self.auth_provider {
            if let Some(value) = provider() {
                headers.insert(reqwest::header::AUTHORIZATION, value);
            } else {
                headers.remove(reqwest::header::AUTHORIZATION);
            }
        }
        headers
    }

    pub async fn list_tools(&mut self, timeout_secs: u64) -> Result<Vec<McpToolDefinition>> {
        let response = timeout(
            Duration::from_secs(timeout_secs),
            self.request("tools/list", json!({})),
        )
        .await
        .map_err(|_| Error::Timeout(timeout_secs))??;
        let tools = response
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Protocol("tools/list response missing tools array".into()))?;
        tools
            .iter()
            .map(|tool| {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Protocol("MCP tool missing name".into()))?;
                Ok(McpToolDefinition {
                    name: name.to_string(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    input_schema: normalize_schema_for_openai(
                        tool.get("inputSchema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    ),
                })
            })
            .collect()
    }

    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout_secs: u64,
    ) -> Result<String> {
        self.call_tool_with_depth(name, arguments, timeout_secs, None)
            .await
    }

    /// HTTP-aware variant of [`McpClient::call_tool_with_depth`]. When
    /// `outbound_call_depth` is `Some`, attach `Mcp-Call-Depth: <n>`
    /// to the outbound JSON-RPC request so the receiving server can
    /// enforce its `--max-call-depth` cap.
    pub async fn call_tool_with_depth(
        &mut self,
        name: &str,
        arguments: Value,
        timeout_secs: u64,
        outbound_call_depth: Option<u32>,
    ) -> Result<String> {
        let response = timeout(
            Duration::from_secs(timeout_secs),
            self.request_with_depth(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
                outbound_call_depth,
            ),
        )
        .await
        .map_err(|_| Error::Timeout(timeout_secs))??;
        Ok(render_call_result(&response))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_with_depth(method, params, None).await
    }

    async fn request_with_depth(
        &mut self,
        method: &str,
        params: Value,
        outbound_call_depth: Option<u32>,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        match self.transport {
            RemoteTransport::StreamableHttp => {
                let url = self.url.clone();
                let response = self.post_json(&url, &request, outbound_call_depth).await?;
                response_result(response, id, method)
            }
            RemoteTransport::Sse => {
                let endpoint = self
                    .endpoint
                    .clone()
                    .ok_or_else(|| Error::Protocol("SSE MCP session missing endpoint".into()))?;
                self.post_json_no_response(&endpoint, &request, outbound_call_depth)
                    .await?;
                self.wait_sse_response(id, method).await
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match self.transport {
            RemoteTransport::StreamableHttp => {
                let url = self.url.clone();
                self.post_json_no_response(&url, &notification, None).await
            }
            RemoteTransport::Sse => {
                let endpoint = self
                    .endpoint
                    .clone()
                    .ok_or_else(|| Error::Protocol("SSE MCP session missing endpoint".into()))?;
                self.post_json_no_response(&endpoint, &notification, None)
                    .await
            }
        }
    }

    async fn post_json(
        &mut self,
        url: &str,
        body: &Value,
        outbound_call_depth: Option<u32>,
    ) -> Result<Value> {
        let mut response = self
            .client
            .post(url)
            .headers(self.outbound_headers())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2024-11-05");
        if let Some(depth) = outbound_call_depth {
            // Receiving servers compare against `--max-call-depth` and
            // refuse to recurse further on overflow; this header is
            // the only thing keeping A→B→A→… chains bounded.
            response = response.header("Mcp-Call-Depth", depth.to_string());
        }
        let response = if let Some(session_id) = &self.session_id {
            response.header("Mcp-Session-Id", session_id)
        } else {
            response
        }
        .json(body)
        .send()
        .await?;
        self.capture_session_id(&response);
        let response = ensure_success(response).await?;
        parse_http_response(response).await
    }

    async fn post_json_no_response(
        &mut self,
        url: &str,
        body: &Value,
        outbound_call_depth: Option<u32>,
    ) -> Result<()> {
        let mut response = self
            .client
            .post(url)
            .headers(self.outbound_headers())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2024-11-05");
        if let Some(depth) = outbound_call_depth {
            response = response.header("Mcp-Call-Depth", depth.to_string());
        }
        let response = if let Some(session_id) = &self.session_id {
            response.header("Mcp-Session-Id", session_id)
        } else {
            response
        }
        .json(body)
        .send()
        .await?;
        self.capture_session_id(&response);
        ensure_success(response).await?;
        Ok(())
    }

    fn capture_session_id(&mut self, response: &reqwest::Response) {
        if self.session_id.is_none() {
            self.session_id = response
                .headers()
                .get("Mcp-Session-Id")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
        }
    }

    async fn start_sse_session(&mut self) -> Result<()> {
        let response = self
            .client
            .get(&self.url)
            .headers(self.outbound_headers())
            .header(ACCEPT, "text/event-stream")
            .send()
            .await?;
        let response = ensure_success(response).await?;
        let (tx, rx) = mpsc::channel(128);
        let (endpoint_tx, endpoint_rx) = oneshot::channel();
        let base_url = self.url.clone();
        tokio::spawn(async move {
            read_sse_stream(response, base_url, tx, endpoint_tx).await;
        });
        let endpoint = endpoint_rx
            .await
            .map_err(|_| Error::Protocol("SSE MCP stream ended before endpoint".into()))?;
        self.endpoint = Some(endpoint);
        self.sse_rx = Some(rx);
        Ok(())
    }

    async fn wait_sse_response(&mut self, id: u64, method: &str) -> Result<Value> {
        let rx = self
            .sse_rx
            .as_mut()
            .ok_or_else(|| Error::Protocol("SSE MCP receiver missing".into()))?;
        loop {
            let value = rx
                .recv()
                .await
                .ok_or_else(|| Error::Protocol("SSE MCP stream closed".into()))?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return response_result(value, id, method);
        }
    }
}

#[async_trait::async_trait]
impl McpClient for RemoteMcpClient {
    async fn list_tools(&mut self, timeout_secs: u64) -> Result<Vec<McpToolDefinition>> {
        RemoteMcpClient::list_tools(self, timeout_secs).await
    }

    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout_secs: u64,
    ) -> Result<String> {
        RemoteMcpClient::call_tool(self, name, arguments, timeout_secs).await
    }

    async fn call_tool_with_depth(
        &mut self,
        name: &str,
        arguments: Value,
        timeout_secs: u64,
        outbound_call_depth: Option<u32>,
    ) -> Result<String> {
        RemoteMcpClient::call_tool_with_depth(
            self,
            name,
            arguments,
            timeout_secs,
            outbound_call_depth,
        )
        .await
    }
}

fn headers_to_map(headers: BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|err| Error::Header(format!("invalid header name {name:?}: {err}")))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|err| Error::Header(format!("invalid header value for {name}: {err}")))?;
        map.insert(name, value);
    }
    Ok(map)
}

/// Cap on a single non-streamed MCP HTTP response body. Mirrors the
/// stdio frame cap (`MAX_FRAME_BODY_BYTES = 16 MiB`) so a hostile or
/// malfunctioning remote MCP server can't OOM the host by chunked-
/// transferring arbitrary bytes into `response.text()`.
const MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

async fn parse_http_response(response: reqwest::Response) -> Result<Value> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let text = zunel_util::read_text_capped(response, MAX_HTTP_RESPONSE_BYTES)
        .await
        .map_err(|err| Error::Protocol(format!("response body read failed: {err}")))?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    if content_type.contains("text/event-stream") {
        let values = parse_sse_values(&text);
        return values
            .into_iter()
            .next()
            .ok_or_else(|| Error::Protocol("SSE response did not contain JSON data".into()));
    }
    Ok(serde_json::from_str(&text)?)
}

async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        // Surface 401s as a typed variant so the per-tool wrapper can
        // convert them into the `MCP_AUTH_REQUIRED:` contract string
        // the chat-driven login skill watches for. `WWW-Authenticate`
        // is optional — RFC 9728 IdPs put `error="invalid_token"`
        // there for the agent to inspect, but plenty of servers omit
        // it on 401.
        let www_authenticate = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        return Err(Error::Unauthorized { www_authenticate });
    }
    let url = response.url().clone();
    // Error responses are typically tiny (RFC 9457 problem+json,
    // or a short HTML stub). Cap at 64 KiB so a buggy server returning
    // a megabyte of stack trace can't tank the gateway.
    let body = zunel_util::read_text_capped(response, 64 * 1024)
        .await
        .unwrap_or_default();
    Err(Error::Protocol(format!(
        "HTTP {status} for {url}: {}",
        body.trim()
    )))
}

fn response_result(value: Value, id: u64, method: &str) -> Result<Value> {
    let response = find_response(value, id)
        .ok_or_else(|| Error::Protocol(format!("MCP {method} response missing id {id}")))?;
    if let Some(error) = response.get("error") {
        return Err(Error::Protocol(format!("MCP {method} failed: {error}")));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| Error::Protocol(format!("MCP {method} response missing result")))
}

fn find_response(value: Value, id: u64) -> Option<Value> {
    if value.get("id").and_then(Value::as_u64) == Some(id) {
        return Some(value);
    }
    value
        .as_array()?
        .iter()
        .find(|item| item.get("id").and_then(Value::as_u64) == Some(id))
        .cloned()
}

/// Cap on the SSE line accumulator and per-event data. A misbehaving (or
/// hostile) remote MCP server streaming an `\n`-free body indefinitely
/// would otherwise grow these buffers without bound. 1 MiB is well above
/// any legitimate MCP SSE event.
const SSE_BUFFER_CAP: usize = 1024 * 1024;

async fn read_sse_stream(
    response: reqwest::Response,
    base_url: String,
    tx: mpsc::Sender<Value>,
    endpoint_tx: oneshot::Sender<String>,
) {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut event = SseEvent::default();
    let mut endpoint_tx = Some(endpoint_tx);
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        if buffer.len() > SSE_BUFFER_CAP {
            tracing::warn!(
                base_url,
                len = buffer.len(),
                cap = SSE_BUFFER_CAP,
                "MCP SSE line accumulator exceeded cap; aborting stream"
            );
            return;
        }
        while let Some(pos) = buffer.find('\n') {
            let mut line = buffer[..pos].to_string();
            if line.ends_with('\r') {
                line.pop();
            }
            buffer.drain(..=pos);
            if line.is_empty() {
                dispatch_sse_event(&mut event, &base_url, &tx, &mut endpoint_tx).await;
            } else if let Some(value) = line.strip_prefix("event:") {
                event.name = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                if !event.data.is_empty() {
                    event.data.push('\n');
                }
                event.data.push_str(value.trim_start());
                if event.data.len() > SSE_BUFFER_CAP {
                    tracing::warn!(
                        base_url,
                        len = event.data.len(),
                        cap = SSE_BUFFER_CAP,
                        "MCP SSE event payload exceeded cap; aborting stream"
                    );
                    return;
                }
            }
        }
    }
    if !event.data.is_empty() {
        dispatch_sse_event(&mut event, &base_url, &tx, &mut endpoint_tx).await;
    }
}

#[derive(Default)]
struct SseEvent {
    name: String,
    data: String,
}

async fn dispatch_sse_event(
    event: &mut SseEvent,
    base_url: &str,
    tx: &mpsc::Sender<Value>,
    endpoint_tx: &mut Option<oneshot::Sender<String>>,
) {
    let data = event.data.trim();
    if data.is_empty() {
        *event = SseEvent::default();
        return;
    }
    if event.name == "endpoint" {
        if let Some(sender) = endpoint_tx.take() {
            match resolve_endpoint(base_url, data) {
                Ok(endpoint) => {
                    let _ = sender.send(endpoint);
                }
                Err(err) => {
                    // Drop the sender so the handshake fails closed — we
                    // never want to silently fall back to a remote-supplied
                    // URL that crosses origin (which would leak our bearer).
                    tracing::warn!(
                        base_url,
                        endpoint = data,
                        error = %err,
                        "refusing MCP SSE endpoint that fails resolve_endpoint"
                    );
                }
            }
        }
    } else if let Ok(value) = serde_json::from_str::<Value>(data) {
        let _ = tx.send(value).await;
    }
    *event = SseEvent::default();
}

fn parse_sse_values(text: &str) -> Vec<Value> {
    let mut values = Vec::new();
    let mut event = SseEvent::default();
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(event.data.trim()) {
                values.push(value);
            }
            event = SseEvent::default();
        } else if let Some(value) = line.strip_prefix("event:") {
            event.name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !event.data.is_empty() {
                event.data.push('\n');
            }
            event.data.push_str(value.trim_start());
        }
    }
    if !event.data.trim().is_empty() {
        if let Ok(value) = serde_json::from_str::<Value>(event.data.trim()) {
            values.push(value);
        }
    }
    values
}

/// Resolve the post-handshake JSON-RPC endpoint a remote SSE MCP server
/// hands us via `event: endpoint`. Rejects anything that crosses origin
/// (scheme + host + port) from the configured base URL — the endpoint
/// payload comes from the remote server, and every subsequent request
/// carries our OAuth bearer token in the Authorization header. A
/// compromised remote could otherwise emit
/// `event: endpoint\ndata: https://attacker.example/relay` and steer
/// our token to its own host.
fn resolve_endpoint(base_url: &str, endpoint: &str) -> Result<String> {
    let base = Url::parse(base_url).map_err(|err| Error::Url(err.to_string()))?;
    let joined = base
        .join(endpoint)
        .map_err(|err| Error::Url(err.to_string()))?;
    if joined.origin() != base.origin() {
        return Err(Error::Url(format!(
            "MCP SSE endpoint {joined} crosses origin from configured base {base}"
        )));
    }
    Ok(joined.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_endpoint_keeps_same_origin_absolute_url() {
        let got =
            resolve_endpoint("https://mcp.example/v1/sse", "https://mcp.example/v1/rpc").unwrap();
        assert_eq!(got, "https://mcp.example/v1/rpc");
    }

    #[test]
    fn resolve_endpoint_joins_relative_path() {
        let got = resolve_endpoint("https://mcp.example/v1/sse", "/v1/rpc").unwrap();
        assert_eq!(got, "https://mcp.example/v1/rpc");
    }

    #[test]
    fn resolve_endpoint_rejects_cross_host() {
        let err = resolve_endpoint(
            "https://mcp.example/v1/sse",
            "https://attacker.example/relay",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("crosses origin"),
            "expected cross-origin rejection, got {err}"
        );
    }

    #[test]
    fn resolve_endpoint_rejects_cross_scheme() {
        // https → http downgrade is a cross-origin shift; reject.
        let err = resolve_endpoint("https://mcp.example/v1/sse", "http://mcp.example/v1/rpc")
            .unwrap_err();
        assert!(err.to_string().contains("crosses origin"), "{err}");
    }

    #[test]
    fn resolve_endpoint_rejects_cross_port() {
        let err = resolve_endpoint(
            "https://mcp.example/v1/sse",
            "https://mcp.example:8443/v1/rpc",
        )
        .unwrap_err();
        assert!(err.to_string().contains("crosses origin"), "{err}");
    }
}
