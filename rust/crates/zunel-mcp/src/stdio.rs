use std::collections::BTreeMap;
use std::process::Stdio;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, Duration};

use crate::frame::MAX_FRAME_BODY_BYTES;
use crate::schema::normalize_schema_for_openai;
use crate::{Error, McpClient, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

pub struct StdioMcpClient {
    _child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    next_id: u64,
}

/// Minimal env an stdio MCP child needs to function. Anything else from
/// the parent process is dropped before spawn so a compromised MCP server
/// can't read `AWS_*`, provider API keys, `SLACK_*`, `SSH_AUTH_SOCK`, etc.
const BASELINE_ENV: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TZ"];

impl StdioMcpClient {
    /// Spawn an stdio MCP child with a hardened environment.
    ///
    /// The child sees only:
    /// 1. The fixed [`BASELINE_ENV`] set, restored from the parent process
    ///    when present.
    /// 2. Any vars listed in `passthrough_env` whose values exist in the
    ///    parent — for operators that genuinely need to forward (e.g.)
    ///    `AWS_PROFILE` to a Slack/AWS MCP server.
    /// 3. The explicit `env` map from config, applied last so it overrides
    ///    anything inherited above.
    ///
    /// Earlier revisions inherited the full parent env via `.envs(env)`
    /// on top of an unscrubbed child, which leaked every secret in the
    /// gateway's environment to every stdio MCP server on first connect.
    pub async fn connect(
        command: &str,
        args: &[String],
        env: BTreeMap<String, String>,
        passthrough_env: &[String],
        init_timeout_secs: u64,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for var in BASELINE_ENV {
            if let Ok(value) = std::env::var(var) {
                cmd.env(var, value);
            }
        }
        for var in passthrough_env {
            if let Ok(value) = std::env::var(var) {
                cmd.env(var, value);
            }
        }
        cmd.envs(env);
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("stdio MCP child missing stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("stdio MCP child missing stdout".into()))?;
        let mut client = Self {
            _child: child,
            stdin,
            stdout,
            next_id: 1,
        };
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

    pub async fn list_tools(&mut self, timeout_secs: u64) -> Result<Vec<McpToolDefinition>> {
        let response = timeout(
            Duration::from_secs(timeout_secs),
            self.request("tools/list", json!({})),
        )
        .await
        .map_err(|_| {
            self.kill_child();
            Error::Timeout(timeout_secs)
        })??;
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
        let response = timeout(
            Duration::from_secs(timeout_secs),
            self.request("tools/call", json!({"name": name, "arguments": arguments})),
        )
        .await
        .map_err(|_| {
            self.kill_child();
            Error::Timeout(timeout_secs)
        })??;
        Ok(render_call_result(&response))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        write_frame(&mut self.stdin, &request).await?;
        loop {
            let response = read_frame(&mut self.stdout).await?;
            let Some(response_id) = response.get("id") else {
                // Server-initiated notification (no `id` field) — skip
                // without breaking the loop.
                continue;
            };
            if !response_id_matches(id, response_id) {
                tracing::debug!(
                    method,
                    expected = id,
                    got = ?response_id,
                    "ignoring MCP response for a different request"
                );
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(Error::Protocol(format!("MCP {method} failed: {error}")));
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| Error::Protocol(format!("MCP {method} response missing result")));
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        write_frame(&mut self.stdin, &request).await
    }

    fn kill_child(&mut self) {
        let _ = self._child.start_kill();
    }
}

#[async_trait::async_trait]
impl McpClient for StdioMcpClient {
    async fn list_tools(&mut self, timeout_secs: u64) -> Result<Vec<McpToolDefinition>> {
        StdioMcpClient::list_tools(self, timeout_secs).await
    }

    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout_secs: u64,
    ) -> Result<String> {
        StdioMcpClient::call_tool(self, name, arguments, timeout_secs).await
    }
}

async fn write_frame(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}

/// Per-syscall inactivity timeout while reading the header bytes of an
/// MCP frame. The outer `request()` wraps the whole exchange in
/// `tokio::time::timeout(tool_timeout_secs, ...)`, but a slow / wedged
/// child can still chew through the per-tool timeout one byte at a
/// time. A 15s deadline on each read kills the response fast without
/// affecting legitimate latency (every real MCP server sends a frame
/// header in one syscall).
const READ_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(15);

async fn read_frame(stdout: &mut ChildStdout) -> Result<Value> {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        let n = timeout(READ_INACTIVITY_TIMEOUT, stdout.read(&mut byte))
            .await
            .map_err(|_| Error::Protocol("MCP child stdout idle for too long".into()))??;
        if n == 0 {
            return Err(Error::Protocol("MCP child closed stdout".into()));
        }
        header.push(byte[0]);
        if header.len() > 8192 {
            return Err(Error::Protocol("MCP frame header too large".into()));
        }
    }
    let header = String::from_utf8(header)
        .map_err(|e| Error::Protocol(format!("MCP frame header is not UTF-8: {e}")))?;
    let content_length = parse_content_length(&header)?;
    let mut body = vec![0_u8; content_length];
    timeout(READ_INACTIVITY_TIMEOUT, stdout.read_exact(&mut body))
        .await
        .map_err(|_| Error::Protocol("MCP child stdout idle while reading body".into()))??;
    Ok(serde_json::from_slice(&body)?)
}

/// Does an inbound JSON-RPC `id` field match the integer id we sent?
///
/// Per JSON-RPC 2.0 §4 the server must echo our id verbatim, but a
/// non-trivial number of real-world MCP servers stringify the integer
/// (`"1"` instead of `1`). The earlier `Value::as_u64`-only comparison
/// rejected the string form and the client looped forever reading
/// non-matching responses. Accept either shape; anything else (null,
/// array, object) is rejected as not-for-us.
fn response_id_matches(expected: u64, value: &Value) -> bool {
    match value {
        Value::Number(n) => n.as_u64() == Some(expected),
        Value::String(s) => s.parse::<u64>().ok() == Some(expected),
        _ => false,
    }
}

/// Pull the `Content-Length` value out of a parsed MCP frame header,
/// rejecting bodies larger than [`MAX_FRAME_BODY_BYTES`] before the
/// caller tries to allocate. Extracted so the cap is unit-testable
/// without standing up a stdio child.
fn parse_content_length(header: &str) -> Result<usize> {
    let len = header
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| Error::Protocol("MCP frame missing Content-Length".into()))?;
    if len > MAX_FRAME_BODY_BYTES {
        return Err(Error::Protocol(format!(
            "MCP frame body too large: {len} bytes (cap {MAX_FRAME_BODY_BYTES})"
        )));
    }
    Ok(len)
}

pub(crate) fn render_call_result(value: &Value) -> String {
    let Some(content) = value.get("content").and_then(Value::as_array) else {
        return value.to_string();
    };
    let parts: Vec<String> = content
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str) {
            Some("text") => item
                .get("text")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        value.to_string()
    } else {
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_length_accepts_small_body() {
        let header = "Content-Length: 42\r\n\r\n";
        assert_eq!(parse_content_length(header).unwrap(), 42);
    }

    #[test]
    fn parse_content_length_rejects_oversized_body() {
        // A malicious stdio MCP child could otherwise OOM the host by
        // announcing a huge Content-Length and never sending the body.
        let over = MAX_FRAME_BODY_BYTES + 1;
        let header = format!("Content-Length: {over}\r\n\r\n");
        let err = parse_content_length(&header).unwrap_err();
        match err {
            Error::Protocol(msg) => assert!(
                msg.contains("too large"),
                "expected body-too-large error, got {msg:?}"
            ),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn parse_content_length_rejects_missing_header() {
        let header = "Other-Header: foo\r\n\r\n";
        let err = parse_content_length(header).unwrap_err();
        match err {
            Error::Protocol(msg) => assert!(msg.contains("Content-Length")),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn response_id_matches_integer_form() {
        assert!(response_id_matches(7, &json!(7)));
        assert!(!response_id_matches(7, &json!(8)));
    }

    #[test]
    fn response_id_matches_stringified_integer_form() {
        // Some real-world MCP servers (notably a handful of Python
        // implementations) stringify the id even though we sent an
        // integer. Accept the string form to interop with them; the
        // earlier u64-only comparison hung the client on first call.
        assert!(response_id_matches(7, &json!("7")));
        assert!(!response_id_matches(7, &json!("8")));
        assert!(!response_id_matches(7, &json!("not-a-number")));
    }

    #[test]
    fn response_id_matches_rejects_other_shapes() {
        assert!(!response_id_matches(7, &json!(null)));
        assert!(!response_id_matches(7, &json!([])));
        assert!(!response_id_matches(7, &json!({})));
        assert!(!response_id_matches(7, &json!(7.5)));
    }
}
