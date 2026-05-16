//! `zunel mcp …` subcommand: stdio MCP server (`mcp serve`) plus OAuth login
//! (`mcp login`).
//!
//! The original 1k-line `mcp.rs` was split into focused submodules so each
//! concern stays easy to scan and to reason about in isolation:
//!
//! * [`oauth`] — interactive `mcp login` flow: dynamic client registration,
//!   PKCE, authorization-code redemption, and on-disk token cache.
//! * [`serve`] — stdio JSON-RPC server entry point and method dispatch.
//! * [`tools`] — implementations for every built-in MCP tool (read-only views
//!   over zunel sessions/channels/cron, plus the Slack message poster).
//!
//! Only [`run`] is reachable from `main.rs`; the submodules expose
//! `pub(super)` helpers that intentionally stay private to this command.

use std::path::Path;

use anyhow::Result;

use crate::cli::{McpArgs, McpCommand};

mod agent;
mod approval_queue;
mod cancel_registry;
mod helper_approval_tools;
mod helper_ask;
mod oauth;
mod registry_dispatcher;
mod serve;
mod tools;

pub async fn run(args: McpArgs, config_path: Option<&Path>) -> Result<()> {
    match args.command {
        McpCommand::Serve(args) => serve::serve(args).await,
        McpCommand::Login(args) => oauth::login(args, config_path).await,
        McpCommand::Agent(args) => agent::run(*args, config_path).await,
    }
}
