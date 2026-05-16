//! Public library facade for zunel.
//!
//! ```no_run
//! use zunel::Zunel;
//!
//! # async fn run() -> anyhow::Result<()> {
//! let bot = Zunel::from_config(None).await?;
//! let result = bot.run("Summarize this repo.").await?;
//! println!("{}", result.content);
//! # Ok(()) }
//! ```

use std::path::Path;
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;

pub use zunel_config::{Config, Error as ConfigError};
pub use zunel_core::{
    AgentLoop, ApprovalDecision, ApprovalHandler, ApprovalRequest, ApprovalScope, ChatRole,
    CommandContext, CommandOutcome, CommandRouter, Error as CoreError, ReloadReport, RunResult,
    RuntimeSelfStateProvider, Session, SessionManager, SharedToolRegistry, SubagentManager,
};
pub use zunel_providers::{Error as ProviderError, LLMProvider, StreamEvent, ToolProgress};
pub use zunel_skills::{Skill, SkillsLoader};
pub use zunel_tools::{Tool, ToolContext, ToolRegistry, ToolResult};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Core(#[from] CoreError),
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Zunel {
    inner: AgentLoop,
}

impl Zunel {
    /// Build a `Zunel` instance from a config file. If `path` is `None`, uses
    /// `<zunel_home>/config.json`.
    pub async fn from_config(path: Option<&Path>) -> Result<Self> {
        let cfg = zunel_config::load_config(path)?;
        let workspace = zunel_config::workspace_path(&cfg.agents.defaults)?;
        zunel_config::guard_workspace(&workspace)?;
        zunel_util::ensure_dir(&workspace).map_err(|source| CoreError::Session {
            path: workspace.clone(),
            source: Box::new(source),
        })?;
        let provider: Arc<dyn LLMProvider> = zunel_providers::build_provider(&cfg).await?;
        let sessions = SessionManager::new(&workspace);
        let mut registry = zunel_core::build_default_registry_async(&cfg, &workspace).await;
        let child_tools = zunel_core::build_default_registry(&cfg, &workspace);
        let subagents = Arc::new(
            SubagentManager::new(
                provider.clone(),
                workspace.clone(),
                cfg.agents.defaults.model.clone(),
            )
            .with_child_tools(child_tools),
        );
        registry.register(Arc::new(zunel_tools::spawn::SpawnTool::new(
            subagents.clone(),
        )));
        let mut tool_names: Vec<String> = registry.names().map(str::to_string).collect();
        tool_names.push("self".into());
        tool_names.push("mcp_reconnect".into());
        registry.register(Arc::new(zunel_tools::self_tool::SelfTool::from_provider(
            Arc::new(RuntimeSelfStateProvider {
                model: cfg.agents.defaults.model.clone(),
                provider: cfg
                    .agents
                    .defaults
                    .provider
                    .clone()
                    .unwrap_or_else(|| "custom".into()),
                workspace: workspace.display().to_string(),
                max_iterations: 15,
                tools: tool_names,
                subagents,
                iteration_counter: None,
                live_tools: None,
            }),
        )));
        // Wrap in a shared handle so `mcp_reconnect` mutates the same
        // registry the agent loop reads from on every turn. The
        // facade forwards the user-supplied `config_path` to the
        // tool so subsequent `agent.mcp_reconnect` calls re-read the
        // exact file the SDK consumer pointed at on construction.
        let shared_registry: zunel_core::SharedToolRegistry =
            Arc::new(RwLock::new(Arc::new(registry)));
        {
            let mut guard = shared_registry
                .write()
                .expect("zunel tool registry lock poisoned");
            Arc::make_mut(&mut *guard).register(Arc::new(
                zunel_core::mcp_reconnect::McpReconnectTool::new(
                    Arc::clone(&shared_registry),
                    path.map(Path::to_path_buf),
                ),
            ));
        }
        let inner = AgentLoop::with_sessions(provider, cfg.agents.defaults.clone(), sessions)
            .with_tools_arc(shared_registry)
            .with_workspace(workspace);
        Ok(Self { inner })
    }

    /// Read-only snapshot of the registered tool set. Includes both
    /// the defaults seeded by `from_config` and anything later
    /// registered via [`Self::register_tool`]. Returns an
    /// `Arc<ToolRegistry>` so call sites like
    /// `bot.tools().get("foo")` continue to work via auto-deref and
    /// the snapshot survives a concurrent MCP reload (the reload
    /// publishes a fresh Arc in the inner cell; the caller's existing
    /// Arc remains valid until dropped).
    pub fn tools(&self) -> Arc<ToolRegistry> {
        self.inner.tools()
    }

    /// Add a custom tool to the registry. Subsequent turns will see
    /// it in the function-call schema sent to the provider.
    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        self.inner.register_tool(tool);
    }

    /// Drop a previously-registered tool by name. Returns `true` if
    /// the registry contained a matching tool, `false` otherwise.
    /// Useful for tearing down an SDK-injected tool without
    /// rebuilding the whole `Zunel` instance.
    pub fn unregister_tool(&mut self, name: &str) -> bool {
        self.inner.unregister_tool(name)
    }

    /// Reload MCP servers from disk and splice the freshly listed
    /// tools into the live registry. See
    /// [`AgentLoop::reload_mcp`] for details.
    pub async fn reload_mcp(
        &self,
        target: Option<&str>,
        config_path: Option<&Path>,
    ) -> Result<ReloadReport> {
        Ok(self.inner.reload_mcp(target, config_path).await?)
    }

    /// Shared handle to the live tool registry. Use this to plumb
    /// the registry into a side component (e.g. an `mcp_reconnect`
    /// tool) that needs to splice MCP entries in/out at runtime.
    pub fn tools_handle(&self) -> SharedToolRegistry {
        self.inner.tools_handle()
    }

    /// One-shot: run a single prompt with no session persistence.
    /// Kept for slice-1 compatibility; prefer `run_streamed` for new code.
    pub async fn run(&self, message: &str) -> Result<RunResult> {
        Ok(self.inner.process_direct(message).await?)
    }

    /// Streaming turn with session persistence. Deltas arrive on `sink`;
    /// the final `RunResult` returns when the turn ends. Drop or close
    /// the receiver on `sink` to propagate-cancel the render consumer;
    /// the provider stream always runs to completion so the session
    /// file remains consistent.
    pub async fn run_streamed(
        &self,
        session_key: &str,
        message: &str,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<RunResult> {
        Ok(self
            .inner
            .process_streamed(session_key, message, sink)
            .await?)
    }
}
