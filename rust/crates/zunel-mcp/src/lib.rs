use async_trait::async_trait;
use serde_json::Value;

pub mod frame;
pub mod http;
pub mod oauth;
pub mod schema;
pub mod stdio;
pub mod wrapper;

pub use frame::{read_frame, write_frame};
pub use http::{AuthHeaderProvider, RemoteMcpClient, RemoteTransport};
pub use oauth::{
    complete_flow as oauth_complete_flow, refresh_all_oauth_servers,
    refresh_if_needed as refresh_oauth_if_needed, start_flow as oauth_start_flow, CompletedFlow,
    Outcome as OAuthRefreshOutcome, StartedFlow,
};
pub use reqwest::header::HeaderValue;
pub use schema::normalize_schema_for_openai;
pub use stdio::{McpToolDefinition, StdioMcpClient};
pub use wrapper::{
    format_auth_required as format_mcp_auth_required, McpAuthRequiredTool, McpToolWrapper,
    SharedMcpClient, MCP_AUTH_REQUIRED_PREFIX,
};

#[async_trait]
pub trait McpClient: Send + Sync {
    async fn list_tools(&mut self, timeout_secs: u64) -> Result<Vec<McpToolDefinition>>;

    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout_secs: u64,
    ) -> Result<String>;

    /// Call a tool while announcing the agent's depth in the chain of
    /// nested MCP calls. Receiving servers compare this against their
    /// `--max-call-depth` and refuse to recurse further when the cap
    /// is hit, which is the only thing standing between an oblivious
    /// A→B→A configuration and an unbounded blow-up.
    ///
    /// Default impl forwards to [`McpClient::call_tool`] and ignores
    /// the depth — appropriate for in-process / stdio transports
    /// where there is no header to carry. The HTTP transport
    /// ([`crate::RemoteMcpClient`]) overrides this to attach
    /// `Mcp-Call-Depth: <outbound>` to the outbound request.
    async fn call_tool_with_depth(
        &mut self,
        name: &str,
        arguments: Value,
        timeout_secs: u64,
        outbound_call_depth: Option<u32>,
    ) -> Result<String> {
        let _ = outbound_call_depth;
        self.call_tool(name, arguments, timeout_secs).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("mcp io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("mcp http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("mcp json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mcp header error: {0}")]
    Header(String),
    #[error("mcp protocol error: {0}")]
    Protocol(String),
    #[error("mcp timeout after {0}s")]
    Timeout(u64),
    #[error("mcp url error: {0}")]
    Url(String),
    /// 401 with a `WWW-Authenticate: Bearer ...` (or no auth scheme)
    /// from the remote MCP server. Surfaced separately so the
    /// [`McpToolWrapper`] can convert it into the `MCP_AUTH_REQUIRED:`
    /// contract string the chat-driven login skill watches for.
    /// `www_authenticate` carries whatever the server returned, for
    /// the `error="invalid_token"` etc. parsing the agent might do.
    #[error("mcp unauthorized (401){}",
        match www_authenticate {
            Some(value) => format!(": {value}"),
            None => String::new(),
        }
    )]
    Unauthorized { www_authenticate: Option<String> },
}

pub type Result<T> = std::result::Result<T, Error>;
