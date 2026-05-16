//! Default tool seeding.
//!
//! Builds a `ToolRegistry` populated with the standard zunel toolset
//! based on a [`Config`]. Read-only filesystem and search tools are
//! always seeded; `exec` and the web tools are gated behind their
//! respective `enable` flags so they stay opt-in.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use tokio::sync::Mutex;
use zunel_config::{Config, McpServerConfig, WebToolsConfig};
use zunel_mcp::{
    oauth as mcp_oauth_refresh, AuthHeaderProvider, McpAuthRequiredTool, McpToolWrapper,
    RemoteMcpClient, RemoteTransport, StdioMcpClient,
};
use zunel_tools::{
    cron::CronTool,
    fs::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool},
    path_policy::PathPolicy,
    search::{GlobTool, GrepTool},
    shell::ExecTool,
    web::{WebFetchTool, WebSearchTool},
    BraveProvider, DuckDuckGoProvider, StubProvider, ToolRegistry, WebSearchProvider,
};

pub fn build_default_registry(cfg: &Config, workspace: &Path) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    let mut policy = PathPolicy::restricted(workspace);
    if let Some(media_dir) = cfg.tools.filesystem.media_dir.as_deref() {
        policy = policy.with_media_dir(media_dir);
    }
    registry.register(Arc::new(ReadFileTool::new(policy.clone())));
    registry.register(Arc::new(WriteFileTool::new(policy.clone())));
    registry.register(Arc::new(EditFileTool::new(policy.clone())));
    registry.register(Arc::new(ListDirTool::new(policy.clone())));
    registry.register(Arc::new(GlobTool::new(policy.clone())));
    registry.register(Arc::new(GrepTool::new(policy)));

    if cfg.tools.exec.enable {
        let exec_env = cfg.tools.exec.env.clone().unwrap_or_default();
        registry.register(Arc::new(ExecTool::with_env(exec_env)));
    }
    if cfg.tools.web.enable {
        registry.register(Arc::new(WebFetchTool::new()));
        let provider = build_search_provider(&cfg.tools.web);
        registry.register(Arc::new(WebSearchTool::new(provider)));
    }
    registry.register(Arc::new(CronTool::new(
        workspace.join("cron").join("jobs.json"),
        "UTC",
    )));
    registry
}

pub async fn build_default_registry_async(cfg: &Config, workspace: &Path) -> ToolRegistry {
    let mut registry = build_default_registry(cfg, workspace);
    register_mcp_tools(&mut registry, cfg).await;
    registry
}

async fn register_mcp_tools(registry: &mut ToolRegistry, cfg: &Config) {
    for (server_name, server) in &cfg.tools.mcp_servers {
        register_one_mcp_server(registry, server_name, server).await;
    }

    // Auto-register the built-in self stdio MCP server when the user
    // hasn't pinned a `--server self` entry of their own and hasn't
    // opted out via `ZUNEL_DISABLE_SELF_MCP`. Synthesizing here (vs.
    // at config-load time) leaves `~/.zunel/config.json` untouched
    // and lets `brew upgrade` pick up new self-MCP tools without
    // users editing their config — important because `mcp_login_start`
    // / `mcp_login_complete` ship with the binary, not the config.
    if should_auto_register_self_mcp(cfg, &|key| std::env::var(key).ok()) {
        let (name, server) = synthesize_self_mcp_server();
        tracing::info!(
            server = %name,
            "auto-registering built-in self MCP server (set ZUNEL_DISABLE_SELF_MCP=1 to opt out)"
        );
        register_one_mcp_server(registry, &name, &server).await;
    }
}

/// Per-call result of [`reload_mcp_servers`]. Surfaces which servers
/// the reload tried to (re)connect, which succeeded (i.e. registered
/// at least one tool), and which failed with a human-readable
/// message — used both by the `/reload` slash command's reply and
/// by the `mcp_reconnect` native tool's tool-result body.
#[derive(Debug, Default, Clone)]
pub struct ReloadReport {
    pub attempted: Vec<String>,
    pub succeeded: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Re-run MCP discovery for one or all configured servers and splice
/// the freshly registered tools into the live registry.
///
/// `target = None` reloads every server in `cfg.tools.mcp_servers`
/// plus the auto-synthesized `zunel_self` entry when applicable —
/// matching boot-time `register_mcp_tools` behavior. `target =
/// Some(name)` reloads only that one entry; "zunel_self" is
/// recognised as the auto-synthesized self-MCP server name.
///
/// Network I/O happens off-lock — we connect, list tools, and
/// build a per-server `ToolRegistry` chunk first, *then* take a
/// single brief write lock to (a) drop any existing
/// `mcp_<name>_*` (and `mcp_<name>_login_required`) entries for
/// the target servers and (b) splice in the newly built tools.
/// In-flight turns therefore never block on network I/O even when
/// the registry is being reloaded under them.
pub async fn reload_mcp_servers(
    registry: &Arc<RwLock<Arc<ToolRegistry>>>,
    cfg: &Config,
    target: Option<&str>,
) -> ReloadReport {
    let mut report = ReloadReport::default();

    let candidates: Vec<(String, McpServerConfig)> = match target {
        Some(name) => {
            if let Some(server) = cfg.tools.mcp_servers.get(name) {
                vec![(name.to_string(), server.clone())]
            } else if name == "zunel_self"
                && should_auto_register_self_mcp(cfg, &|key| std::env::var(key).ok())
            {
                let (n, s) = synthesize_self_mcp_server();
                vec![(n, s)]
            } else {
                report.failed.push((
                    name.to_string(),
                    format!("server `{name}` is not configured in ~/.zunel/config.json"),
                ));
                return report;
            }
        }
        None => {
            let mut out: Vec<(String, McpServerConfig)> = cfg
                .tools
                .mcp_servers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if should_auto_register_self_mcp(cfg, &|key| std::env::var(key).ok()) {
                out.push(synthesize_self_mcp_server());
            }
            out
        }
    };

    let mut per_server_chunks: Vec<(String, ToolRegistry)> = Vec::with_capacity(candidates.len());
    for (name, server) in &candidates {
        report.attempted.push(name.clone());
        let mut chunk = ToolRegistry::new();
        register_one_mcp_server(&mut chunk, name, server).await;
        let prefix = format!("mcp_{name}_");
        let stub_name = format!("mcp_{name}_login_required");
        let registered_count = chunk
            .names()
            .filter(|n| n.starts_with(&prefix) || *n == stub_name)
            .count();
        if registered_count == 0 {
            report.failed.push((
                name.clone(),
                format!(
                    "server `{name}` registered no tools \
                     (see logs for connect/list/auth errors)"
                ),
            ));
        } else {
            report.succeeded.push(name.clone());
        }
        per_server_chunks.push((name.clone(), chunk));
    }

    {
        let mut guard = registry.write().expect("zunel tool registry lock poisoned");
        // `Arc::make_mut` clones the inner ToolRegistry if other
        // snapshots are still alive (in-flight turns), then mutates the
        // freshly-cloned copy in place. The published Arc swaps to the
        // new contents atomically when the write guard drops.
        let w = Arc::make_mut(&mut *guard);
        for (name, _) in &per_server_chunks {
            let prefix = format!("mcp_{name}_");
            let stub_name = format!("mcp_{name}_login_required");
            w.retain(|tool_name| !tool_name.starts_with(&prefix) && tool_name != stub_name);
        }
        for (_, chunk) in per_server_chunks {
            w.extend(chunk);
        }
    }

    report
}

/// Periodic auto-reconnect: walk every user-configured MCP server,
/// reload only the ones that look unhealthy (no `mcp_<name>_*`
/// tools and no `mcp_<name>_login_required` stub), and return the
/// merged [`ReloadReport`] for logging.
///
/// "Unhealthy" deliberately excludes the OAuth auth-required stub
/// case — those need a chat-driven `mcp_login_complete` (or
/// `zunel mcp login --force`), not a periodic re-dial.
///
/// Healthy servers are skipped so the periodic tick is a near-no-op
/// once everything's connected. The synthesized `zunel_self` entry
/// is also skipped: it spawns the parent binary, which doesn't fail
/// in production and would otherwise generate test/CI noise.
pub async fn reconnect_unhealthy_mcp_servers(
    registry: &Arc<RwLock<Arc<ToolRegistry>>>,
    cfg: &Config,
) -> ReloadReport {
    let unhealthy: Vec<String> = {
        let r = registry.read().expect("zunel tool registry lock poisoned");
        cfg.tools
            .mcp_servers
            .keys()
            .filter(|name| is_mcp_server_unhealthy(&r, name))
            .cloned()
            .collect()
    };

    let mut report = ReloadReport::default();
    for name in &unhealthy {
        let single = reload_mcp_servers(registry, cfg, Some(name)).await;
        report.attempted.extend(single.attempted);
        report.succeeded.extend(single.succeeded);
        report.failed.extend(single.failed);
    }
    report
}

/// Decide whether the periodic auto-reconnect should retry this
/// server. "Unhealthy" means no `mcp_<name>_<tool>` tools registered
/// AND no `mcp_<name>_login_required` stub. The stub case is
/// excluded because it indicates a credentials issue that periodic
/// re-dials can't resolve — the chat-driven login skill or
/// `zunel mcp login --force` is the right escape hatch.
fn is_mcp_server_unhealthy(registry: &ToolRegistry, server_name: &str) -> bool {
    let prefix = format!("mcp_{server_name}_");
    let stub = format!("mcp_{server_name}_login_required");
    let has_real_tools = registry
        .names()
        .any(|n| n.starts_with(&prefix) && n != stub);
    let has_login_stub = registry.get(&stub).is_some();
    !has_real_tools && !has_login_stub
}

/// Decide whether `register_mcp_tools` should synthesize the
/// `zunel_self` stdio MCP entry. Pulled out as a pure function so the
/// gate is unit-testable without poking `register_mcp_tools` (which
/// in turn would try to spawn a child process).
fn should_auto_register_self_mcp(
    cfg: &Config,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> bool {
    if is_truthy_env(env_lookup("ZUNEL_DISABLE_SELF_MCP").as_deref()) {
        return false;
    }
    !cfg.tools.mcp_servers.values().any(is_self_serve_entry)
}

/// Connect to one MCP server (stdio or remote) and register its
/// tools. Extracted from the original `register_mcp_tools` loop body
/// so the synthesized self-MCP entry takes the same path as
/// user-configured ones — no behavior fork, no second copy of the
/// auth-required-stub logic.
async fn register_one_mcp_server(
    registry: &mut ToolRegistry,
    server_name: &str,
    server: &McpServerConfig,
) {
    let init_timeout = server.init_timeout.unwrap_or(10);
    let tool_timeout = server.tool_timeout.unwrap_or(30);
    let mut client: Box<dyn zunel_mcp::McpClient> = match mcp_transport(server) {
        McpTransport::Stdio => {
            let Some(command_raw) = server.command.as_deref() else {
                tracing::warn!(server = %server_name, "skipping stdio MCP server without command");
                return;
            };
            let resolved_command = match resolve_stdio_command(command_raw) {
                Ok(cmd) => cmd,
                Err(err) => {
                    tracing::warn!(
                        server = %server_name,
                        command = command_raw,
                        error = %err,
                        "failed to resolve stdio MCP command"
                    );
                    return;
                }
            };
            let args = server.args.clone().unwrap_or_default();
            let env = server.env.clone().unwrap_or_default();
            let passthrough_env = server.passthrough_env.clone().unwrap_or_default();
            match StdioMcpClient::connect(
                &resolved_command,
                &args,
                env,
                &passthrough_env,
                init_timeout,
            )
            .await
            {
                Ok(client) => Box::new(client),
                Err(err) => {
                    tracing::warn!(server = %server_name, error = %err, "failed to initialize MCP server");
                    return;
                }
            }
        }
        McpTransport::Remote(transport) => {
            let Some(url) = server.url.as_deref() else {
                tracing::warn!(server = %server_name, "skipping remote MCP server without url");
                return;
            };
            let mcp_oauth_state = remote_oauth_state(server_name, server).await;
            if mcp_oauth_state.needs_login() {
                register_auth_required_stub(registry, server_name, mcp_oauth_state.reason_tag());
                return;
            }
            let headers = mcp_oauth_state.static_headers.clone();
            let auth_provider = mcp_oauth_state.auth_provider.clone();
            let oauth_enabled = mcp_oauth_state.oauth_enabled;
            match RemoteMcpClient::connect_with_auth(
                url,
                headers,
                auth_provider,
                transport,
                init_timeout,
            )
            .await
            {
                Ok(client) => Box::new(client),
                Err(err) => {
                    tracing::warn!(server = %server_name, error = %err, "failed to initialize MCP server");
                    if oauth_enabled {
                        register_auth_required_stub(registry, server_name, "connect_failed");
                    }
                    return;
                }
            }
        }
        McpTransport::Unsupported(transport) => {
            tracing::warn!(server = %server_name, transport, "skipping unsupported MCP transport");
            return;
        }
    };
    let tools = match client.list_tools(tool_timeout).await {
        Ok(tools) => tools,
        Err(err) => {
            tracing::warn!(server = %server_name, error = %err, "failed to list MCP tools");
            if matches!(err, zunel_mcp::Error::Unauthorized { .. })
                && server
                    .normalized_oauth()
                    .map(|oauth| oauth.enabled)
                    .unwrap_or(false)
            {
                register_auth_required_stub(registry, server_name, "invalid_token");
            }
            return;
        }
    };
    let client = Arc::new(Mutex::new(client));
    for tool in tools {
        let wrapped_name = format!("mcp_{server_name}_{}", tool.name);
        if !valid_tool_name(&wrapped_name) {
            tracing::warn!(
                server = %server_name,
                tool = %tool.name,
                wrapped = %wrapped_name,
                "skipping MCP tool with invalid provider function name"
            );
            continue;
        }
        if !tool_enabled(server, &tool.name, &wrapped_name) {
            continue;
        }
        registry.register(Arc::new(McpToolWrapper::new(
            server_name,
            tool,
            Arc::clone(&client),
            tool_timeout,
        )));
    }
}

/// Synthesize the auto-registered self-MCP entry consumed by
/// `register_mcp_tools` when the user hasn't pinned a `--server self`
/// entry of their own. Spawns `<current_exe> mcp serve --server self`
/// — the `"self"` command sentinel resolves via [`resolve_stdio_command`]
/// at connect time, so this works under brew, deb, and `cargo install`
/// without hardcoding any prefix.
fn synthesize_self_mcp_server() -> (String, McpServerConfig) {
    let server = McpServerConfig {
        transport_type: Some("stdio".into()),
        command: Some("self".into()),
        args: Some(vec![
            "mcp".into(),
            "serve".into(),
            "--server".into(),
            "self".into(),
        ]),
        init_timeout: Some(15),
        tool_timeout: Some(30),
        ..Default::default()
    };
    ("zunel_self".into(), server)
}

/// Detect when an operator-supplied MCP entry is already serving
/// `--server self`, regardless of the JSON key name they used. We
/// look at args (not the server name) because users name their
/// entries however they want. Detection: any arg `self` immediately
/// preceded by `--server`.
fn is_self_serve_entry(server: &McpServerConfig) -> bool {
    let Some(args) = server.args.as_deref() else {
        return false;
    };
    let mut prev: Option<&str> = None;
    for arg in args {
        if prev == Some("--server") && arg == "self" {
            return true;
        }
        prev = Some(arg.as_str());
    }
    false
}

/// Cheap env truthiness check, kept local to `default_tools.rs` so
/// this module doesn't take a fresh dependency on `zunel-cli`'s
/// `env_disabled` (which lives in `gateway.rs` for unrelated
/// reasons). Accepts pre-fetched values via `&str` so tests can
/// exercise the matcher without touching the process env table.
fn is_truthy_env(value: Option<&str>) -> bool {
    matches!(
        value,
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn register_auth_required_stub(registry: &mut ToolRegistry, server_name: &str, reason: &str) {
    let wrapped_name = format!("mcp_{server_name}_login_required");
    if !valid_tool_name(&wrapped_name) {
        tracing::warn!(
            server = %server_name,
            wrapped = %wrapped_name,
            "cannot register MCP auth-required stub with invalid tool name"
        );
        return;
    }
    tracing::warn!(
        server = %server_name,
        reason = %reason,
        "registering MCP auth-required stub; chat-driven login skill will pick this up"
    );
    registry.register(Arc::new(McpAuthRequiredTool::new(server_name, reason)));
}

/// Resolve the `command` field of a stdio MCP server entry.
///
/// Most values are passed through verbatim and ultimately fed to
/// `Command::new`, which performs PATH lookup for bare names and
/// uses the literal argument for absolute/relative paths.
///
/// The single sentinel value `"self"` is special-cased: it expands
/// to the absolute path of the *currently running* zunel binary
/// (via [`std::env::current_exe`]). This lets users wire the
/// built-in `zunel mcp serve --server slack|self` MCP servers into
/// their config without hardcoding a Homebrew/cargo/install prefix:
///
/// ```json
/// {
///   "tools": {
///     "mcpServers": {
///       "slack_me": {
///         "type": "stdio",
///         "command": "self",
///         "args": ["mcp", "serve", "--server", "slack"]
///       }
///     }
///   }
/// }
/// ```
///
/// The motivating environment is `brew services start zunel`
/// (macOS launchd): brew's mxcl plist does not propagate
/// `/opt/homebrew/bin` to the gateway's `PATH`, so a bare
/// `"command": "zunel"` would fail to spawn. Resolving via
/// `current_exe()` is prefix-agnostic and works for cargo
/// installs, .deb installs, and direct binary drops as well.
fn resolve_stdio_command(command: &str) -> std::io::Result<String> {
    if command != "self" {
        return Ok(command.to_string());
    }
    let exe = std::env::current_exe()?;
    exe.to_str().map(ToOwned::to_owned).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("current_exe path is not valid UTF-8: {}", exe.display()),
        )
    })
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

enum McpTransport {
    Stdio,
    Remote(RemoteTransport),
    Unsupported(String),
}

fn mcp_transport(server: &McpServerConfig) -> McpTransport {
    let raw = server.transport_type.as_deref().unwrap_or_else(|| {
        if server.url.is_some() {
            "streamableHttp"
        } else {
            "stdio"
        }
    });
    match raw.to_ascii_lowercase().as_str() {
        "stdio" => McpTransport::Stdio,
        "streamablehttp" | "http" => McpTransport::Remote(RemoteTransport::StreamableHttp),
        "sse" => McpTransport::Remote(RemoteTransport::Sse),
        other => McpTransport::Unsupported(other.to_string()),
    }
}

fn tool_enabled(server: &McpServerConfig, raw_name: &str, wrapped_name: &str) -> bool {
    let Some(enabled) = &server.enabled_tools else {
        return true;
    };
    enabled
        .iter()
        .any(|name| name == "*" || name == raw_name || name == wrapped_name)
}

/// Snapshot of the OAuth bookkeeping for one remote MCP server, computed
/// in `register_mcp_tools` before we attempt to connect. Lets us decide
/// in one place whether to dial out at all (`needs_login()` ⇒ register
/// an AUTH_REQUIRED stub instead) and what `Authorization` header to
/// attach when we do.
struct McpRemoteOauthState {
    /// Operator-supplied headers (after `${VAR}` expansion). The
    /// `Authorization` header is *not* in here when zunel owns the
    /// OAuth lifecycle — that one comes from `auth_provider` below
    /// so refreshes show up live in the gateway without a
    /// reconnect.
    static_headers: BTreeMap<String, String>,
    oauth_enabled: bool,
    /// Live re-read of `~/.zunel/mcp-oauth/<server>/token.json` per
    /// outbound request. `None` when zunel doesn't own the bearer
    /// header for this server (operator pinned `Authorization`, or
    /// `zunel_home()` couldn't be resolved). When set, the
    /// `RemoteMcpClient` consults it on every request so the
    /// periodic refresh task's writes propagate immediately.
    auth_provider: Option<AuthHeaderProvider>,
    /// `Some` when zunel owns the bearer-header lifecycle for this
    /// server (not operator-supplied `Authorization`). Used to drive
    /// the auth-required stub decision: `Outcome::NotCached |
    /// NoRefreshToken | NoTokenUrl | RefreshFailed` ⇒ chat-driven
    /// re-login.
    refresh_outcome: Option<mcp_oauth_refresh::Outcome>,
}

impl McpRemoteOauthState {
    fn needs_login(&self) -> bool {
        if !self.oauth_enabled {
            return false;
        }
        match &self.refresh_outcome {
            Some(outcome) => match outcome {
                mcp_oauth_refresh::Outcome::NotCached
                | mcp_oauth_refresh::Outcome::NoRefreshToken
                | mcp_oauth_refresh::Outcome::NoTokenUrl
                | mcp_oauth_refresh::Outcome::RefreshFailed(_) => true,
                mcp_oauth_refresh::Outcome::StillFresh { .. }
                | mcp_oauth_refresh::Outcome::Refreshed { .. }
                | mcp_oauth_refresh::Outcome::NoExpiry => false,
            },
            None => false,
        }
    }

    fn reason_tag(&self) -> &'static str {
        match &self.refresh_outcome {
            Some(mcp_oauth_refresh::Outcome::NotCached) => "not_cached",
            Some(mcp_oauth_refresh::Outcome::NoRefreshToken) => "no_refresh_token",
            Some(mcp_oauth_refresh::Outcome::NoTokenUrl) => "no_token_url",
            Some(mcp_oauth_refresh::Outcome::RefreshFailed(_)) => "refresh_failed",
            _ => "unknown",
        }
    }
}

async fn remote_oauth_state(server_name: &str, server: &McpServerConfig) -> McpRemoteOauthState {
    let raw = server.headers.clone().unwrap_or_default();
    let static_headers = expand_header_envs(server_name, raw, &|name| std::env::var(name).ok());
    let oauth_enabled = server
        .normalized_oauth()
        .map(|oauth| oauth.enabled)
        .unwrap_or(false);
    if static_headers
        .keys()
        .any(|key| key.eq_ignore_ascii_case("authorization"))
    {
        // Operator-supplied `Authorization` header wins; don't
        // even read the cached OAuth token (and definitely don't
        // try to refresh it). The chat-driven login skill is
        // intentionally skipped here too — if the operator pinned
        // a header, they own the rotation.
        return McpRemoteOauthState {
            static_headers,
            oauth_enabled,
            auth_provider: None,
            refresh_outcome: None,
        };
    }
    let Ok(home) = zunel_config::zunel_home() else {
        return McpRemoteOauthState {
            static_headers,
            oauth_enabled,
            auth_provider: None,
            refresh_outcome: None,
        };
    };
    let outcome = mcp_oauth_refresh::refresh_if_needed(&home, server_name).await;
    log_oauth_refresh_outcome(server_name, &outcome);
    let auth_provider = if oauth_enabled {
        Some(make_oauth_auth_provider(&home, server_name))
    } else {
        None
    };
    McpRemoteOauthState {
        static_headers,
        oauth_enabled,
        auth_provider,
        refresh_outcome: Some(outcome),
    }
}

/// Build a closure the `RemoteMcpClient` consults on every outbound
/// request: read the cached token from disk, format
/// `<TokenType> <accessToken>`, and return it as a `HeaderValue`.
///
/// Costs roughly one ~1KB file read per MCP call, which is dwarfed
/// by the network round-trip. The win is that a refresh task that
/// rewrites `token.json` propagates to in-flight `RemoteMcpClient`s
/// on the next request — no reconnect, no tool re-registration.
fn make_oauth_auth_provider(home: &Path, server_name: &str) -> AuthHeaderProvider {
    let home = home.to_path_buf();
    let server_name = server_name.to_string();
    Arc::new(move || -> Option<zunel_mcp::HeaderValue> {
        match zunel_config::mcp_oauth::load_token(&home, &server_name) {
            Ok(Some(token)) => zunel_mcp::HeaderValue::from_str(&token.authorization_header()).ok(),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(
                    server = %server_name,
                    error = %err,
                    "ignoring invalid MCP OAuth token cache while building Authorization header"
                );
                None
            }
        }
    })
}

/// Surface the [`mcp_oauth_refresh::Outcome`] in `tracing` so the
/// nightly "the Atlassian token went stale and nothing's wired up
/// to Jira anymore" failure mode is visible at info/warn instead
/// of being inferred from a downstream 401 buried in
/// `gateway.err.log`.
fn log_oauth_refresh_outcome(server_name: &str, outcome: &mcp_oauth_refresh::Outcome) {
    use mcp_oauth_refresh::Outcome::*;
    match outcome {
        NotCached | NoExpiry => {}
        StillFresh { secs_remaining } => {
            tracing::debug!(
                server = server_name,
                secs_remaining = *secs_remaining,
                "MCP OAuth token still fresh"
            );
        }
        Refreshed { new_expires_in } => {
            tracing::info!(
                server = server_name,
                new_expires_in = ?new_expires_in,
                "refreshed MCP OAuth access token via refresh_token grant"
            );
        }
        NoRefreshToken => {
            tracing::warn!(
                server = server_name,
                "MCP OAuth access token expired and no refresh_token is cached; \
                 ask the agent to log you in (or run `zunel mcp login {server_name} --force`)",
                server_name = server_name,
            );
        }
        NoTokenUrl => {
            tracing::warn!(
                server = server_name,
                "MCP OAuth token cache is missing the tokenUrl needed to refresh; \
                 ask the agent to log you in (or run `zunel mcp login {server_name} --force`)",
                server_name = server_name,
            );
        }
        RefreshFailed(err) => {
            tracing::warn!(
                server = server_name,
                error = %err,
                "MCP OAuth refresh attempt failed; continuing with cached (likely-stale) token",
            );
        }
    }
}

/// Walk the configured `headers` map and substitute `${VAR}` /
/// `${VAR:-default}` placeholders against `lookup` (which is just
/// `std::env::var` in production but stubbable from tests). Any
/// header whose value references an unset variable with no default
/// is dropped so we never put the literal `${...}` token onto the
/// wire — operators rely on this so they can keep secrets out of
/// `config.json` and the dotenv-style `${X:-fallback}` form lets
/// them ship safe defaults for non-secret values.
fn expand_header_envs(
    server: &str,
    headers: BTreeMap<String, String>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    headers
        .into_iter()
        .filter_map(|(key, value)| {
            expand_env_placeholders(server, &key, &value, lookup).map(|expanded| (key, expanded))
        })
        .collect()
}

/// Expand a single header value. Returns `None` (after logging at
/// `warn`) when an unset variable was referenced without a default,
/// when a `${` block was unterminated, or when the variable name was
/// not a valid POSIX-style identifier. Returning `None` instructs
/// the caller to drop the header entirely.
fn expand_env_placeholders(
    server: &str,
    header: &str,
    raw: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            // Fast-path: copy the run of non-`$` bytes in one go.
            // Safe to slice on a byte boundary because `$` is ASCII
            // and never falls inside a UTF-8 multibyte sequence.
            let next = bytes[i..]
                .iter()
                .position(|&b| b == b'$')
                .map(|p| i + p)
                .unwrap_or(bytes.len());
            out.push_str(&raw[i..next]);
            i = next;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'$') => {
                // `$$` escapes a literal `$` so users can write a real
                // dollar sign without a placeholder being inferred.
                out.push('$');
                i += 2;
            }
            Some(b'{') => {
                let Some(close_rel) = bytes[i + 2..].iter().position(|&b| b == b'}') else {
                    tracing::warn!(
                        server,
                        header,
                        "unterminated `${{` in header value; dropping header"
                    );
                    return None;
                };
                let close = i + 2 + close_rel;
                let inside = &raw[i + 2..close];
                let (var_name, default) = match inside.split_once(":-") {
                    Some((name, default)) => (name.trim(), Some(default)),
                    None => (inside.trim(), None),
                };
                if !valid_env_var_name(var_name) {
                    tracing::warn!(
                        server,
                        header,
                        env_var = var_name,
                        "invalid env var name in header value; dropping header"
                    );
                    return None;
                }
                match lookup(var_name) {
                    Some(value) if !value.is_empty() => out.push_str(&value),
                    _ => match default {
                        Some(default) => out.push_str(default),
                        None => {
                            tracing::warn!(
                                server,
                                header,
                                env_var = var_name,
                                "environment variable referenced in header value is unset; \
                                 dropping header. Use ${{VAR:-default}} to provide a fallback."
                            );
                            return None;
                        }
                    },
                }
                i = close + 1;
            }
            _ => {
                // A bare `$` not followed by `{` or `$` is left as-is
                // for forward compatibility (e.g. JWTs that contain
                // `$argon2id$` literals).
                out.push('$');
                i += 1;
            }
        }
    }
    Some(out)
}

fn valid_env_var_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn build_search_provider(cfg: &WebToolsConfig) -> Box<dyn WebSearchProvider> {
    match cfg.search_provider.as_str() {
        "brave" => {
            let key = cfg.brave_api_key.clone().unwrap_or_default();
            Box::new(BraveProvider::new(key))
        }
        "duckduckgo" | "ddg" => Box::new(DuckDuckGoProvider::new()),
        // Empty string + anything unknown collapses to a stub provider
        // that returns a clear "unimplemented" error at call time.
        _ => Box::new(StubProvider {
            provider_name: "stub",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zunel_config::mcp_oauth::{save_token, CachedMcpOAuthToken};

    fn full_token(access_token: &str, token_type: Option<&str>) -> CachedMcpOAuthToken {
        CachedMcpOAuthToken {
            access_token: access_token.into(),
            token_type: token_type.map(ToOwned::to_owned),
            refresh_token: None,
            expires_in: None,
            scope: None,
            obtained_at: 0,
            client_id: "cid".into(),
            client_secret: None,
            authorization_url: "https://example.test/authorize".into(),
            token_url: "https://example.test/token".into(),
        }
    }

    #[test]
    fn resolve_stdio_command_passes_through_non_sentinel_values() {
        assert_eq!(resolve_stdio_command("zunel").unwrap(), "zunel".to_string());
        assert_eq!(
            resolve_stdio_command("/opt/homebrew/bin/zunel").unwrap(),
            "/opt/homebrew/bin/zunel".to_string()
        );
        // Empty string is not the sentinel and is returned as-is so the
        // downstream warning surfaces the real misconfiguration instead
        // of getting swallowed by current_exe() success.
        assert_eq!(resolve_stdio_command("").unwrap(), "".to_string());
    }

    #[test]
    fn resolve_stdio_command_self_sentinel_expands_to_current_exe() {
        let resolved = resolve_stdio_command("self").expect("current_exe must succeed in tests");
        let expected = std::env::current_exe()
            .expect("current_exe in tests")
            .to_string_lossy()
            .into_owned();
        assert_eq!(resolved, expected);
        assert!(
            std::path::Path::new(&resolved).is_absolute(),
            "expected absolute path from current_exe, got {resolved}"
        );
    }

    #[test]
    fn oauth_auth_provider_reads_cached_token_per_call() {
        let home = tempfile::tempdir().unwrap();
        save_token(
            home.path(),
            "remote",
            &full_token("token-1", Some("bearer")),
        )
        .unwrap();

        let provider = make_oauth_auth_provider(home.path(), "remote");
        let value = provider().expect("authorization header present");
        assert_eq!(value, "Bearer token-1");

        // Live re-read: rewriting the cache reflects in the next call
        // without re-creating the provider — this is the property the
        // periodic refresh task relies on.
        save_token(
            home.path(),
            "remote",
            &full_token("token-2", Some("bearer")),
        )
        .unwrap();
        let value = provider().expect("authorization header present after refresh");
        assert_eq!(value, "Bearer token-2");
    }

    #[test]
    fn oauth_auth_provider_returns_none_when_token_cache_missing() {
        let home = tempfile::tempdir().unwrap();
        let provider = make_oauth_auth_provider(home.path(), "never-logged-in");
        assert!(provider().is_none());
    }

    fn lookup_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn expand_substitutes_simple_var() {
        let lookup = lookup_from(&[("API_KEY", "supersecret")]);
        assert_eq!(
            expand_env_placeholders("self", "Authorization", "Bearer ${API_KEY}", &lookup),
            Some("Bearer supersecret".to_string())
        );
    }

    #[test]
    fn expand_chains_multiple_vars_and_literals() {
        let lookup = lookup_from(&[("USER", "ada"), ("ORG", "tunnel")]);
        assert_eq!(
            expand_env_placeholders(
                "self",
                "X-Trace",
                "user=${USER};org=${ORG};static=ok",
                &lookup
            ),
            Some("user=ada;org=tunnel;static=ok".to_string())
        );
    }

    #[test]
    fn expand_drops_header_when_var_missing_and_no_default() {
        let lookup = lookup_from(&[]);
        assert_eq!(
            expand_env_placeholders("self", "Authorization", "Bearer ${MISSING}", &lookup),
            None,
        );
    }

    #[test]
    fn expand_uses_default_when_var_missing() {
        let lookup = lookup_from(&[]);
        assert_eq!(
            expand_env_placeholders(
                "self",
                "Authorization",
                "Bearer ${MISSING:-fallback-token}",
                &lookup,
            ),
            Some("Bearer fallback-token".to_string())
        );
    }

    #[test]
    fn expand_uses_default_when_var_empty() {
        let lookup = lookup_from(&[("EMPTY", "")]);
        assert_eq!(
            expand_env_placeholders(
                "self",
                "Authorization",
                "Bearer ${EMPTY:-fallback}",
                &lookup,
            ),
            Some("Bearer fallback".to_string())
        );
    }

    #[test]
    fn expand_treats_double_dollar_as_literal() {
        let lookup = lookup_from(&[("X", "should-not-appear")]);
        assert_eq!(
            expand_env_placeholders("self", "X-Hash", "$$X $${X} $$$$", &lookup),
            Some("$X ${X} $$".to_string())
        );
    }

    #[test]
    fn expand_passes_bare_dollar_through() {
        // Argon2/PHC strings begin with `$` and must survive untouched.
        let lookup = lookup_from(&[]);
        assert_eq!(
            expand_env_placeholders(
                "self",
                "X-Hash",
                "$argon2id$v=19$m=65536,t=3,p=4$abc$def",
                &lookup
            ),
            Some("$argon2id$v=19$m=65536,t=3,p=4$abc$def".to_string())
        );
    }

    #[test]
    fn expand_drops_header_on_unterminated_brace() {
        let lookup = lookup_from(&[("API_KEY", "x")]);
        assert_eq!(
            expand_env_placeholders("self", "Authorization", "Bearer ${API_KEY", &lookup),
            None,
        );
    }

    #[test]
    fn expand_rejects_invalid_var_name() {
        let lookup = lookup_from(&[("9NOPE", "x")]);
        // Identifiers can't start with a digit.
        assert_eq!(
            expand_env_placeholders("self", "Authorization", "Bearer ${9NOPE}", &lookup),
            None,
        );
    }

    #[test]
    fn expand_default_value_is_taken_literally_including_dollars() {
        let lookup = lookup_from(&[]);
        assert_eq!(
            expand_env_placeholders(
                "self",
                "X-Default",
                "${MISSING:-some$weird:-value}",
                &lookup
            ),
            Some("some$weird:-value".to_string())
        );
    }

    #[test]
    fn expand_header_envs_drops_only_failing_entries() {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".into(), "Bearer ${API_KEY}".into());
        headers.insert("X-Static".into(), "fixed".into());
        headers.insert("X-Missing".into(), "v=${NOPE}".into());

        let lookup = lookup_from(&[("API_KEY", "abc")]);
        let expanded = expand_header_envs("self", headers, &lookup);

        assert_eq!(expanded.len(), 2);
        assert_eq!(
            expanded.get("Authorization").map(String::as_str),
            Some("Bearer abc")
        );
        assert_eq!(expanded.get("X-Static").map(String::as_str), Some("fixed"));
        assert!(!expanded.contains_key("X-Missing"));
    }

    /// OAuth-enabled remote MCP server with no cached token must
    /// surface as an `mcp_<server>_login_required` stub in the
    /// registry — that's the signal the chat-driven login skill
    /// keys off (the alternative being a silent
    /// "this MCP just disappeared from the registry" failure mode).
    #[tokio::test]
    async fn registers_auth_required_stub_when_oauth_token_not_cached() {
        let home = tempfile::tempdir().unwrap();
        let raw = json!({
            "providers": {},
            "agents": {"defaults": {"model": "m"}},
            "tools": {"mcpServers": {
                "atlassian-jira": {
                    "type": "streamableHttp",
                    "url": "http://127.0.0.1:1/mcp",
                    "oauth": {"enabled": true}
                }
            }}
        });
        let cfg: Config = serde_json::from_value(raw).unwrap();
        // Force `zunel_home()` to look at the tempdir so
        // `refresh_if_needed` reads "no cached token" instead of
        // whatever the dev box happens to have under `~/.zunel`.
        std::env::set_var("ZUNEL_HOME", home.path());

        let mut registry = ToolRegistry::new();
        register_mcp_tools(&mut registry, &cfg).await;

        let names: Vec<&str> = registry.names().collect();
        assert!(
            names.contains(&"mcp_atlassian-jira_login_required"),
            "expected an auth-required stub in the registry, got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("mcp_atlassian-jira_")
                && n != &"mcp_atlassian-jira_login_required"),
            "no real MCP tools should be registered before OAuth login: {names:?}"
        );
    }

    /// `Outcome::StillFresh` (or any non-needs-login variant) must
    /// NOT register a stub: that path is supposed to dial out and
    /// register the real tools. We can't test "actually connects"
    /// without a real MCP server here, but we can assert the
    /// `needs_login` decision is false for fresh-token state.
    #[test]
    fn remote_oauth_state_needs_login_only_for_login_required_outcomes() {
        use mcp_oauth_refresh::Outcome::*;
        for outcome in [
            StillFresh { secs_remaining: 60 },
            Refreshed {
                new_expires_in: Some(3600),
            },
            NoExpiry,
        ] {
            let state = McpRemoteOauthState {
                static_headers: BTreeMap::new(),
                oauth_enabled: true,
                auth_provider: None,
                refresh_outcome: Some(outcome),
            };
            assert!(!state.needs_login());
        }
        for (outcome, reason) in [
            (NotCached, "not_cached"),
            (NoRefreshToken, "no_refresh_token"),
            (NoTokenUrl, "no_token_url"),
            (RefreshFailed("oh no".into()), "refresh_failed"),
        ] {
            let state = McpRemoteOauthState {
                static_headers: BTreeMap::new(),
                oauth_enabled: true,
                auth_provider: None,
                refresh_outcome: Some(outcome),
            };
            assert!(state.needs_login(), "should require login for: {reason}");
            assert_eq!(state.reason_tag(), reason);
        }
        // OAuth disabled => no stub even for "needs login" outcomes.
        let disabled = McpRemoteOauthState {
            static_headers: BTreeMap::new(),
            oauth_enabled: false,
            auth_provider: None,
            refresh_outcome: Some(NotCached),
        };
        assert!(!disabled.needs_login());
    }

    /// `synthesize_self_mcp_server()` is the single source of truth
    /// for the auto-registered entry's transport + args. If anyone
    /// changes them we want this test to fail loudly because the
    /// matching `is_self_serve_entry` detection logic depends on
    /// `--server self` appearing back-to-back in args.
    #[test]
    fn synthesize_self_mcp_server_returns_stdio_entry_serving_self() {
        let (name, server) = synthesize_self_mcp_server();
        assert_eq!(name, "zunel_self");
        assert_eq!(server.transport_type.as_deref(), Some("stdio"));
        assert_eq!(server.command.as_deref(), Some("self"));
        assert_eq!(
            server.args.as_deref(),
            Some(&["mcp", "serve", "--server", "self"][..])
                .map(|s| s.iter().map(|x| x.to_string()).collect::<Vec<_>>())
                .as_deref()
        );
        // Synthesized entry must round-trip through detection so a
        // user copying the auto-registered config into their own
        // file won't end up with two `zunel_self` stubs.
        assert!(is_self_serve_entry(&server));
    }

    #[test]
    fn is_self_serve_entry_recognises_dash_dash_server_self() {
        let s = McpServerConfig {
            args: Some(vec![
                "mcp".into(),
                "serve".into(),
                "--server".into(),
                "self".into(),
            ]),
            ..Default::default()
        };
        assert!(is_self_serve_entry(&s));
    }

    #[test]
    fn is_self_serve_entry_rejects_non_self_servers() {
        let slack = McpServerConfig {
            args: Some(vec![
                "mcp".into(),
                "serve".into(),
                "--server".into(),
                "slack".into(),
            ]),
            ..Default::default()
        };
        assert!(!is_self_serve_entry(&slack));

        let empty = McpServerConfig {
            args: Some(vec![]),
            ..Default::default()
        };
        assert!(!is_self_serve_entry(&empty));

        let none = McpServerConfig::default();
        assert!(!is_self_serve_entry(&none));
    }

    #[test]
    fn is_self_serve_entry_rejects_self_without_dash_dash_server_prefix() {
        // Bare `self` not preceded by `--server` is not a self-serve
        // invocation. Important because the script-binary auto-resolve
        // sentinel is `command: "self"`, not an args-level value.
        let spurious = McpServerConfig {
            args: Some(vec!["self".into(), "--banana".into()]),
            ..Default::default()
        };
        assert!(!is_self_serve_entry(&spurious));
    }

    #[test]
    fn is_truthy_env_recognises_documented_truthy_values() {
        for v in ["1", "true", "TRUE", "yes", "YES"] {
            assert!(is_truthy_env(Some(v)), "expected truthy: {v}");
        }
        for v in ["", "0", "false", "FALSE", "no", "yEs", "True"] {
            assert!(!is_truthy_env(Some(v)), "expected falsy: {v}");
        }
        assert!(!is_truthy_env(None));
    }

    fn lookup_pairs<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn cfg_with_no_mcp_servers() -> Config {
        let raw = json!({
            "providers": {},
            "agents": {"defaults": {"model": "m"}},
            "tools": {"mcpServers": {}}
        });
        serde_json::from_value(raw).unwrap()
    }

    fn cfg_with_user_self_mcp() -> Config {
        // User hand-wired their own `--server self` under a custom
        // server name. We must NOT auto-register a second one.
        let raw = json!({
            "providers": {},
            "agents": {"defaults": {"model": "m"}},
            "tools": {"mcpServers": {
                "my_zunel_self": {
                    "type": "stdio",
                    "command": "zunel",
                    "args": ["mcp", "serve", "--server", "self"]
                }
            }}
        });
        serde_json::from_value(raw).unwrap()
    }

    #[test]
    fn auto_register_gate_synthesizes_for_empty_config() {
        let cfg = cfg_with_no_mcp_servers();
        assert!(should_auto_register_self_mcp(&cfg, &lookup_pairs(&[])));
    }

    #[test]
    fn auto_register_gate_skips_when_disable_env_set() {
        let cfg = cfg_with_no_mcp_servers();
        for v in ["1", "true", "TRUE", "yes", "YES"] {
            assert!(
                !should_auto_register_self_mcp(
                    &cfg,
                    &lookup_pairs(&[("ZUNEL_DISABLE_SELF_MCP", v)]),
                ),
                "expected ZUNEL_DISABLE_SELF_MCP={v} to suppress auto-reg",
            );
        }
    }

    #[test]
    fn auto_register_gate_skips_when_user_already_serves_self() {
        let cfg = cfg_with_user_self_mcp();
        assert!(!should_auto_register_self_mcp(&cfg, &lookup_pairs(&[])));
    }

    #[test]
    fn auto_register_gate_synthesizes_when_user_has_unrelated_mcp_only() {
        let raw = json!({
            "providers": {},
            "agents": {"defaults": {"model": "m"}},
            "tools": {"mcpServers": {
                "atlassian-jira": {
                    "type": "streamableHttp",
                    "url": "https://example.test/mcp",
                    "oauth": {"enabled": true}
                }
            }}
        });
        let cfg: Config = serde_json::from_value(raw).unwrap();
        assert!(should_auto_register_self_mcp(&cfg, &lookup_pairs(&[])));
    }
}
