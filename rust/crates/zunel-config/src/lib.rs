//! Config loading, schema types, and `~/.zunel` path resolution.

mod error;
mod instance;
mod loader;
pub mod mcp_oauth;
mod paths;
mod schema;

pub use error::{Error, Result};
pub use instance::{
    active_instance_home, active_instance_name, default_zunel_root, list_instances,
    resolve_instance_home, set_sticky_instance, DEFAULT_INSTANCE_NAME,
};
pub use loader::load_config;
pub use mcp_oauth::{
    load_token as load_mcp_oauth_token, mcp_oauth_token_path, save_token as save_mcp_oauth_token,
    unix_timestamp_now, CachedMcpOAuthToken, DEFAULT_REFRESH_SKEW_SECS,
};
pub use paths::{
    cli_history_path, default_config_path, default_workspace_path, guard_workspace, sessions_dir,
    workspace_path, workspace_path_safe, zunel_home, UNSAFE_WORKSPACE_ENV,
};
pub use schema::{
    AgentDefaults, AgentsConfig, AwsConfig, BedrockProvider, ChannelsConfig, CliConfig,
    CodexProvider, Config, CustomProvider, DreamConfig, ExecToolsConfig, FilesystemToolsConfig,
    GatewayConfig, HeartbeatConfig, McpOAuthConfig, McpServerConfig, ProvidersConfig,
    SlackChannelConfig, SlackDmConfig, ToolsConfig, WebToolsConfig, DEFAULT_COMPACTION_KEEP_TAIL,
    DEFAULT_CONTEXT_WINDOW_TOKENS, DEFAULT_MAX_TOKENS_FALLBACK, DEFAULT_SESSION_HISTORY_WINDOW,
    DEFAULT_TOOL_RESULT_BUDGET_CHARS, HISTORY_BUDGET_HEADROOM_TOKENS,
};
