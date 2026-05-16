//! `zunel mcp serve`: stdio JSON-RPC server.
//!
//! The serve loop reuses [`zunel_mcp::frame::read_frame`] /
//! [`zunel_mcp::frame::write_frame`] for Content-Length framing so the wire
//! format stays identical to the other built-in MCP servers
//! (`zunel-mcp-self`, `zunel-mcp-slack`).
//!
//! Method dispatch lives here; tool implementations live in
//! [`super::tools`] so this file stays focused on the protocol layer.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::BufReader;

use crate::cli::McpServeArgs;

use super::tools;

pub(super) async fn serve(args: McpServeArgs) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    while let Some(request) = next_frame(&mut reader).await? {
        let response = handle_request(&args.server, request).await;
        zunel_mcp::write_frame(&mut stdout, &response).await?;
    }
    Ok(())
}

/// Wrap [`zunel_mcp::read_frame`] so a clean stdin EOF surfaces as `Ok(None)`
/// to the serve loop instead of bubbling up as a protocol error. Any other
/// error (malformed header, IO failure) still propagates.
async fn next_frame<R>(reader: &mut R) -> Result<Option<Value>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match zunel_mcp::read_frame(reader).await {
        Ok(value) => Ok(Some(value)),
        Err(zunel_mcp::Error::Protocol(msg)) if msg.contains("stdin closed") => Ok(None),
        Err(err) => Err(err.into()),
    }
}

async fn handle_request(server: &str, request: Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "serverInfo": {"name": format!("zunel-{server}"), "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"tools": {}}
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": tools_for(server)}
        }),
        "tools/call" => {
            let name = request
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = request
                .get("params")
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": call_tool_with_args(server, name, &args).await}],
                    "isError": false
                }
            })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "method not found"}
        }),
    }
}

fn tools_for(server: &str) -> Vec<Value> {
    match server {
        "slack" => zunel_mcp_slack::tools(),
        _ => vec![
            json!({
                "name": "self_status",
                "description": "Report zunel self status",
                "inputSchema": {"type": "object", "properties": {}}
            }),
            json!({
                "name": "zunel_sessions_list",
                "description": "List zunel session summaries from the active workspace",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer"},
                        "search": {"type": "string"}
                    }
                }
            }),
            json!({
                "name": "zunel_session_get",
                "description": "Get metadata for one zunel session",
                "inputSchema": {
                    "type": "object",
                    "properties": {"session_key": {"type": "string"}},
                    "required": ["session_key"]
                }
            }),
            json!({
                "name": "zunel_session_messages",
                "description": "Get trailing messages for one zunel session",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_key": {"type": "string"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["session_key"]
                }
            }),
            json!({
                "name": "zunel_channels_list",
                "description": "List configured zunel channels without secrets",
                "inputSchema": {"type": "object", "properties": {}}
            }),
            json!({
                "name": "zunel_mcp_servers_list",
                "description": "List configured MCP servers without secrets",
                "inputSchema": {"type": "object", "properties": {}}
            }),
            json!({
                "name": "zunel_cron_jobs_list",
                "description": "List cron jobs from the active workspace",
                "inputSchema": {
                    "type": "object",
                    "properties": {"include_disabled": {"type": "boolean"}}
                }
            }),
            json!({
                "name": "zunel_cron_job_get",
                "description": "Get one cron job from the active workspace",
                "inputSchema": {
                    "type": "object",
                    "properties": {"job_id": {"type": "string"}},
                    "required": ["job_id"]
                }
            }),
            json!({
                "name": "zunel_send_message_to_channel",
                "description": "Send text to a supported configured channel",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "channel": {"type": "string"},
                        "channel_id": {"type": "string"},
                        "text": {"type": "string"},
                        "thread_ts": {"type": "string"}
                    },
                    "required": ["channel", "channel_id", "text"]
                }
            }),
            json!({
                "name": "mcp_login_start",
                "description": "Start an OAuth login flow for a remote MCP server. Returns the authorize URL the user should open in their browser; after they approve, they paste the redirect URL back into chat and the agent calls `mcp_login_complete`. Use this for `log me into <server>`, `reauth <server>`, or after an MCP tool call returns an error starting with `MCP_AUTH_REQUIRED:`. Side effect: writes `~/.zunel/mcp-oauth/<server>/pending.json` (10-min TTL).",
                "inputSchema": {
                    "type": "object",
                    "properties": {"server": {"type": "string"}},
                    "required": ["server"]
                }
            }),
            json!({
                "name": "mcp_login_complete",
                "description": "Finish an OAuth login flow started by `mcp_login_start`. Pass the full redirect URL the IdP sent the user back to (or just the `?code=...&state=...` query string). Side effect: writes `~/.zunel/mcp-oauth/<server>/token.json` and removes the pending file on success.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "server": {"type": "string"},
                        "callback_url": {"type": "string"}
                    },
                    "required": ["server", "callback_url"]
                }
            }),
            zunel_mcp_slack::capability_tool_descriptor(),
        ],
    }
}

async fn call_tool(server: &str, name: &str) -> String {
    match (server, name) {
        (_, "self_status") => "zunel-self ok".into(),
        (_, "sessions_list") => {
            tools::sessions_list(&json!({})).unwrap_or_else(|err| err.to_string())
        }
        _ => format!("unknown tool: {name}"),
    }
}

async fn call_tool_with_args(server: &str, name: &str, args: &Value) -> String {
    if server == "slack" {
        return zunel_mcp_slack::call_tool(name, args)
            .await
            .unwrap_or_else(|err| err.to_string());
    }
    match (server, name) {
        (_, "zunel_sessions_list" | "sessions_list") => {
            tools::sessions_list(args).unwrap_or_else(|err| err.to_string())
        }
        (_, "zunel_session_get" | "session_get") => {
            tools::session_get(args).unwrap_or_else(|err| err.to_string())
        }
        (_, "zunel_session_messages" | "session_messages") => {
            tools::session_messages(args).unwrap_or_else(|err| err.to_string())
        }
        (_, "zunel_channels_list" | "channels_list") => {
            tools::channels_list().unwrap_or_else(|err| err.to_string())
        }
        (_, "zunel_mcp_servers_list" | "mcp_servers_list") => {
            tools::mcp_servers_list().unwrap_or_else(|err| err.to_string())
        }
        (_, "zunel_cron_jobs_list" | "cron_jobs_list") => {
            tools::cron_jobs_list(args).unwrap_or_else(|err| err.to_string())
        }
        (_, "zunel_cron_job_get" | "cron_job_get") => {
            tools::cron_job_get(args).unwrap_or_else(|err| err.to_string())
        }
        (_, "zunel_send_message_to_channel" | "send_message_to_channel") => {
            tools::send_message_to_channel(args)
                .await
                .unwrap_or_else(|err| err.to_string())
        }
        (_, "zunel_slack_capability" | "slack_capability") => zunel_mcp_slack::capability_report(),
        (_, "mcp_login_start") => tools::mcp_login_start(args)
            .await
            .unwrap_or_else(|err| err.to_string()),
        (_, "mcp_login_complete") => tools::mcp_login_complete(args)
            .await
            .unwrap_or_else(|err| err.to_string()),
        _ => call_tool(server, name).await,
    }
}
