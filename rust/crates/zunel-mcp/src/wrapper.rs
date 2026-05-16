use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use zunel_tools::{Tool, ToolContext, ToolResult};

use crate::{Error, McpClient, McpToolDefinition};

pub type SharedMcpClient = Arc<Mutex<Box<dyn McpClient>>>;

/// String prefix the chat-driven `mcp-oauth-login` skill watches for.
/// All "the user must (re)authenticate this MCP server" failure
/// modes — startup-time *and* mid-conversation — surface this exact
/// prefix so the agent has a single, machine-checkable signal.
pub const MCP_AUTH_REQUIRED_PREFIX: &str = "MCP_AUTH_REQUIRED:";

/// Format the agent-facing AUTH_REQUIRED contract string. `reason`
/// is a short tag (`invalid_token`, `not_cached`, `refresh_failed`,
/// …) that the skill includes verbatim in its prompt so the operator
/// can tell why the relogin is being asked for.
pub fn format_auth_required(server: &str, reason: &str) -> String {
    format!("{MCP_AUTH_REQUIRED_PREFIX}server={server}; reason={reason}")
}

pub struct McpToolWrapper {
    name: &'static str,
    original_name: String,
    server: String,
    description: &'static str,
    parameters: Value,
    client: SharedMcpClient,
    tool_timeout_secs: u64,
}

impl McpToolWrapper {
    pub fn new(
        server_name: &str,
        definition: McpToolDefinition,
        client: SharedMcpClient,
        tool_timeout_secs: u64,
    ) -> Self {
        let name = leak_string(format!("mcp_{server_name}_{}", definition.name));
        let description = leak_string(
            definition
                .description
                .clone()
                .unwrap_or_else(|| definition.name.clone()),
        );
        Self {
            name,
            original_name: definition.name,
            server: server_name.to_string(),
            description,
            parameters: definition.input_schema,
            client,
            tool_timeout_secs,
        }
    }
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let mut client = self.client.lock().await;
        // Forward the agent's depth in the chain so the receiving
        // server can enforce its own --max-call-depth cap. For local
        // / stdio MCP transports the default impl drops the value;
        // only the HTTP transport actually puts the header on the
        // wire (see `RemoteMcpClient::call_tool_with_depth`).
        let outbound_depth = Some(ctx.outbound_call_depth());
        match client
            .call_tool_with_depth(
                &self.original_name,
                args,
                self.tool_timeout_secs,
                outbound_depth,
            )
            .await
        {
            Ok(content) => ToolResult::ok(content),
            Err(Error::Unauthorized { www_authenticate }) => {
                // Mid-conversation 401 → emit the AUTH_REQUIRED contract
                // so the agent's `mcp-oauth-login` skill picks it up
                // and offers to relogin (instead of the operator
                // staring at a 401 they have no way to act on from
                // chat).
                tracing::warn!(
                    server = %self.server,
                    tool = %self.original_name,
                    www_authenticate = www_authenticate.as_deref().unwrap_or(""),
                    "MCP tool call returned 401; signalling auth-required"
                );
                ToolResult::err(format_auth_required(&self.server, "invalid_token"))
            }
            Err(err) => ToolResult::err(format!("(MCP tool call failed: {err})")),
        }
    }
}

/// Stub registered in place of an OAuth-enabled MCP server we
/// couldn't bring up: either the cached token is missing /
/// non-refreshable, or the `connect`/`tools/list` round-trip
/// failed at startup. Every call returns the agent-facing
/// AUTH_REQUIRED contract string so the chat-driven login skill
/// has a signal to act on instead of the tool silently
/// disappearing from the registry (the pre-v0.2.6 behaviour).
///
/// Carries no MCP client; nothing on the wire.
pub struct McpAuthRequiredTool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
    auth_required_message: String,
}

impl McpAuthRequiredTool {
    pub fn new(server_name: &str, reason: &str) -> Self {
        let name = leak_string(format!("mcp_{server_name}_login_required"));
        let description = leak_string(format!(
            "Stub: remote MCP server '{server_name}' needs OAuth login. Calling this tool \
             returns `MCP_AUTH_REQUIRED:server={server_name}; reason=...` so the agent's \
             chat-driven login skill picks the server up. The actual MCP tools come back \
             once the user logs in."
        ));
        Self {
            name,
            description,
            parameters: json!({"type": "object", "properties": {}}),
            auth_required_message: format_auth_required(server_name, reason),
        }
    }
}

#[async_trait]
impl Tool for McpAuthRequiredTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::err(self.auth_required_message.clone())
    }
}

fn leak_string(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auth_required_message_carries_server_and_reason() {
        let msg = format_auth_required("atlassian-jira", "not_cached");
        assert_eq!(
            msg,
            "MCP_AUTH_REQUIRED:server=atlassian-jira; reason=not_cached"
        );
        assert!(msg.starts_with(MCP_AUTH_REQUIRED_PREFIX));
    }

    #[tokio::test]
    async fn auth_required_tool_always_returns_contract_message() {
        let tool = McpAuthRequiredTool::new("atlassian-jira", "no_refresh_token");
        assert_eq!(tool.name(), "mcp_atlassian-jira_login_required");
        let result = tool.execute(json!({}), &ToolContext::for_test()).await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            "MCP_AUTH_REQUIRED:server=atlassian-jira; reason=no_refresh_token"
        );
    }
}
