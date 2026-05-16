//! Native `mcp_reconnect` tool. The LLM (in any channel — CLI agent,
//! Slack gateway, anywhere it has tool access) can invoke this to
//! reconnect a previously-failing MCP server (or every configured
//! server) and have its tools spliced into the live registry without
//! re-execing the binary.
//!
//! Why a native tool, not a self-MCP tool: the self-MCP server runs
//! as a child process (stdio) and can't reach into the parent's
//! `Arc<RwLock<ToolRegistry>>` to mutate it. This tool lives in the
//! parent agent's registry alongside `spawn` and `self`, so it can
//! call `reload_mcp_servers` directly against the same handle the
//! agent loop reads from.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use zunel_tools::{Tool, ToolContext, ToolResult};

use crate::default_tools::reload_mcp_servers;
use crate::SharedToolRegistry;

pub struct McpReconnectTool {
    registry: SharedToolRegistry,
    config_path: Option<PathBuf>,
}

impl McpReconnectTool {
    pub fn new(registry: SharedToolRegistry, config_path: Option<PathBuf>) -> Self {
        Self {
            registry,
            config_path,
        }
    }
}

#[async_trait]
impl Tool for McpReconnectTool {
    fn name(&self) -> &'static str {
        "mcp_reconnect"
    }

    fn description(&self) -> &'static str {
        "Reconnect to MCP servers and refresh their exposed tools in the live agent registry. \
         Use this when an MCP server was unreachable at startup but is now healthy (e.g. the \
         operator restarted a backing container), or after editing `~/.zunel/config.json` to \
         add or change a server. Pass `server` to reload one entry; omit it to reload every \
         configured server. Returns a JSON report with `attempted`, `succeeded`, and `failed` \
         arrays."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Optional server name (matches a key under tools.mcpServers in config). Omit to reload all servers."
                }
            }
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let target = args
            .get("server")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let cfg = match zunel_config::load_config(self.config_path.as_deref()) {
            Ok(cfg) => cfg,
            Err(err) => {
                return ToolResult::err(format!(
                    "mcp_reconnect: failed to load zunel config: {err}"
                ));
            }
        };

        let report = reload_mcp_servers(&self.registry, &cfg, target).await;
        let body = json!({
            "attempted": report.attempted,
            "succeeded": report.succeeded,
            "failed": report.failed.iter().map(|(name, message)| json!({
                "server": name,
                "error": message,
            })).collect::<Vec<_>>(),
        });
        let body_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
        if !report.failed.is_empty() && report.succeeded.is_empty() {
            ToolResult::err(body_str)
        } else {
            ToolResult::ok(body_str)
        }
    }
}

/// Convenience constructor that wraps the tool in an `Arc<dyn Tool>`
/// ready for `ToolRegistry::register`. Saves CLI/gateway wiring code
/// from importing `Arc` and `dyn Tool` separately.
pub fn into_dyn_tool(registry: SharedToolRegistry, config_path: Option<PathBuf>) -> Arc<dyn Tool> {
    Arc::new(McpReconnectTool::new(registry, config_path))
}
