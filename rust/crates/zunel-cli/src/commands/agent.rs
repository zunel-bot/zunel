use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use zunel_core::{
    build_default_registry, build_default_registry_async, mcp_reconnect::McpReconnectTool,
    reconnect_unhealthy_mcp_servers, AgentLoop, ApprovalHandler, ApprovalScope, ReloadReport,
    RuntimeSelfStateProvider, SessionManager, SharedToolRegistry, SubagentManager,
};
use zunel_skills::SkillsLoader;
use zunel_tools::{self_tool::SelfTool, spawn::SpawnTool};

use crate::approval_cli::StdinApprovalHandler;
use crate::cli::AgentArgs;
use crate::renderer::StreamingRenderer;
use crate::repl::{run_repl, ReplConfig};

pub async fn run(args: AgentArgs, config_path: Option<&Path>) -> Result<()> {
    let cfg = zunel_config::load_config(config_path).with_context(|| "loading zunel config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults)
        .with_context(|| "resolving workspace path")?;
    zunel_config::guard_workspace(&workspace).with_context(|| "validating workspace path")?;
    zunel_util::ensure_dir(&workspace)
        .with_context(|| format!("creating workspace dir {}", workspace.display()))?;

    let provider = zunel_providers::build_provider(&cfg)
        .await
        .with_context(|| "building provider")?;
    let sessions = SessionManager::new(&workspace);
    let mut registry = build_default_registry_async(&cfg, &workspace).await;
    let child_tools = build_default_registry(&cfg, &workspace);
    let subagents = Arc::new(
        SubagentManager::new(
            provider.clone(),
            workspace.clone(),
            cfg.agents.defaults.model.clone(),
        )
        .with_child_tools(child_tools),
    );
    registry.register(Arc::new(SpawnTool::new(subagents.clone())));
    let mut tool_names: Vec<String> = registry.names().map(str::to_string).collect();
    tool_names.push("self".into());
    tool_names.push("mcp_reconnect".into());
    registry.register(Arc::new(SelfTool::from_provider(Arc::new(
        RuntimeSelfStateProvider {
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
        },
    ))));
    // Wrap the registry in a shared handle BEFORE registering
    // `mcp_reconnect`, because that tool needs to mutate the same
    // live registry the agent loop reads on every turn (so reload
    // requests show up immediately, no restart needed).
    let shared_registry: SharedToolRegistry = Arc::new(RwLock::new(Arc::new(registry)));
    {
        let mut guard = shared_registry
            .write()
            .expect("zunel tool registry lock poisoned");
        Arc::make_mut(&mut *guard).register(Arc::new(McpReconnectTool::new(
            Arc::clone(&shared_registry),
            config_path.map(Path::to_path_buf),
        )));
    }
    // Load skills from `<workspace>/skills/` plus the binary-bundled
    // builtins (e.g. `mcp-oauth-login`). User skills win on name
    // collisions; embedded builtins fill in otherwise.
    let skills = SkillsLoader::new(&workspace, None, &[]);
    let mut builder =
        AgentLoop::with_sessions(provider, cfg.agents.defaults.clone(), sessions.clone())
            .with_tools_arc(shared_registry)
            .with_workspace(workspace.clone())
            .with_skills(skills)
            .with_approval_required(cfg.tools.approval_required)
            .with_approval_scope(parse_approval_scope(&cfg.tools.approval_scope));
    if cfg.tools.approval_required {
        let handler: Arc<dyn ApprovalHandler> = Arc::new(StdinApprovalHandler::new());
        builder = builder.with_approval(handler);
    }
    let agent_loop = Arc::new(builder);

    let show_footer = args.show_tokens || cfg.cli.show_token_footer;
    match args.message {
        // One-shot `-m "…"` runs are short-lived; spinning up a 5-min
        // reconnect loop just to abort it on the next line would only
        // add log noise. The REPL path below is the only place
        // background self-healing earns its keep.
        Some(msg) => run_once(agent_loop.as_ref(), &args.session, &msg, show_footer).await,
        None => {
            let reconnect_task = spawn_mcp_reconnect_task(agent_loop.tools_handle(), config_path);
            let repl_cfg = ReplConfig {
                session_key: args.session.clone(),
                model_label: cfg.agents.defaults.model.clone(),
                show_token_footer: show_footer,
            };
            let result = run_repl(agent_loop, Arc::new(sessions), repl_cfg).await;
            if let Some(handle) = reconnect_task {
                handle.abort();
            }
            result
        }
    }
}

/// Wake every 5 minutes and reload any MCP server that has no live
/// tools registered. Same default-on, env-tunable behavior as the
/// gateway's [`crate::commands::gateway::spawn_mcp_reconnect_task`]
/// (see that comment for the deeper rationale): operators get a
/// long-running `zunel agent` that quietly heals when an MCP backend
/// comes back online, without having to type `/reload`.
///
/// Returns `None` when `ZUNEL_MCP_RECONNECT_DISABLED=1`.
fn spawn_mcp_reconnect_task(
    registry: SharedToolRegistry,
    config_path: Option<&Path>,
) -> Option<tokio::task::JoinHandle<()>> {
    if env_disabled("ZUNEL_MCP_RECONNECT_DISABLED") {
        tracing::info!("agent MCP auto-reconnect disabled via ZUNEL_MCP_RECONNECT_DISABLED");
        return None;
    }
    let tick_secs = parse_env_or(
        "ZUNEL_MCP_RECONNECT_TICK_SECS",
        MCP_RECONNECT_DEFAULT_TICK_SECS,
    );
    let cfg_path = config_path.map(Path::to_path_buf);
    Some(tokio::spawn(mcp_reconnect_loop(
        registry, cfg_path, tick_secs,
    )))
}

async fn mcp_reconnect_loop(
    registry: SharedToolRegistry,
    config_path: Option<PathBuf>,
    tick_secs: u64,
) {
    tracing::info!(tick_secs, "starting agent MCP auto-reconnect");
    let mut ticker = tokio::time::interval(Duration::from_secs(tick_secs));
    loop {
        ticker.tick().await;
        let cfg = match zunel_config::load_config(config_path.as_deref()) {
            Ok(cfg) => cfg,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "agent MCP auto-reconnect: failed to reload config; retrying next tick"
                );
                continue;
            }
        };
        let report = reconnect_unhealthy_mcp_servers(&registry, &cfg).await;
        if !report.attempted.is_empty() {
            log_mcp_reconnect_outcome(&report);
        }
    }
}

fn log_mcp_reconnect_outcome(report: &ReloadReport) {
    if !report.succeeded.is_empty() {
        tracing::info!(
            servers = ?report.succeeded,
            count = report.succeeded.len(),
            "agent MCP auto-reconnect: brought server(s) online"
        );
    }
    for (server, error) in &report.failed {
        tracing::warn!(
            server = %server,
            error = %error,
            "agent MCP auto-reconnect: still unable to reach server"
        );
    }
}

const MCP_RECONNECT_DEFAULT_TICK_SECS: u64 = 300;

fn env_disabled(key: &str) -> bool {
    matches!(
        std::env::var(key).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("True") | Ok("yes") | Ok("on")
    )
}

fn parse_env_or(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(default)
}

fn parse_approval_scope(s: &str) -> ApprovalScope {
    match s.to_ascii_lowercase().as_str() {
        "shell" => ApprovalScope::Shell,
        "writes" | "write" => ApprovalScope::Writes,
        // "all", empty string, "none", anything else collapses to All;
        // gating happens at `approval_required = false` for the off case.
        _ => ApprovalScope::All,
    }
}

async fn run_once(
    agent_loop: &AgentLoop,
    session_key: &str,
    message: &str,
    show_footer: bool,
) -> Result<()> {
    let (tx, rx) = mpsc::channel(64);
    let renderer = StreamingRenderer::start();
    let render_task = tokio::spawn(async move { renderer.drive(rx).await });
    let result = agent_loop
        .process_streamed(session_key, message, tx)
        .await
        .with_context(|| "running agent")?;
    render_task
        .await
        .map_err(|e| anyhow::anyhow!("render task failed: {e}"))??;
    if show_footer {
        let footer = zunel_core::format_footer(&result.usage, &result.session_total_usage);
        if !footer.is_empty() {
            println!("{footer}");
        }
    }
    Ok(())
}
