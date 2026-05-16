//! Local tools for zunel: filesystem, search, shell, web, plus the
//! `Tool` trait and `ToolRegistry` everything else registers through.

pub mod cron;
pub mod error;
pub mod file_state;
pub mod fs;
pub mod message;
pub mod path_policy;
mod registry;
pub mod schema;
pub mod search;
pub mod self_tool;
pub mod shell;
pub mod spawn;
pub mod ssrf;
mod tool;
pub mod web;
mod web_search_providers;

pub use web_search_providers::{
    BraveProvider, DuckDuckGoProvider, StubProvider, WebSearchProvider, WebSearchResult,
};

pub use error::{Error, Result};
pub use registry::ToolRegistry;
pub use tool::{DynTool, RpcId, Tool, ToolContext, ToolResult};
