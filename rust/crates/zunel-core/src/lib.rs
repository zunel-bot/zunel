//! Agent loop, runner, context, memory.

mod agent_loop;
pub mod approval;
pub mod command;
pub mod compaction;
pub mod default_tools;
pub mod document;
mod error;
pub mod hook;
pub mod mcp_reconnect;
mod memory;
pub mod runner;
mod session;
pub mod subagent;
pub mod trim;
pub mod usage_footer;

pub use agent_loop::{AgentLoop, RunResult, SharedToolRegistry};
pub use approval::{
    summarize_tool_call, tool_requires_approval, AllowAllApprovalHandler, ApprovalDecision,
    ApprovalHandler, ApprovalRequest, ApprovalScope, BusApprovalHandler, CachedApprovalHandler,
    RejectAllApprovalHandler,
};
pub use command::{CommandContext, CommandOutcome, CommandRouter};
pub use compaction::CompactionService;
pub use default_tools::{
    build_default_registry, build_default_registry_async, reconnect_unhealthy_mcp_servers,
    reload_mcp_servers, ReloadReport,
};
pub use document::{extract_documents, extract_documents_with_limit};
pub use error::{Error, Result};
pub use hook::{AgentHook, AgentHookContext};
pub use memory::{DreamCursor, DreamService, HistoryEntry, MemoryStore};
pub use runner::{
    trim_messages_for_provider, AgentRunResult, AgentRunSpec, AgentRunner, StopReason, TrimBudgets,
};
pub use session::{ChatRole, Session, SessionManager, MAX_TURN_USAGE_ENTRIES};
pub use subagent::{RuntimeSelfStateProvider, SubagentManager, SubagentStatus};
pub use usage_footer::{format_footer, format_totals, humanize};
