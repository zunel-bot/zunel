//! `zunel mcp agent`: serve the active instance's tool registry as a
//! Streamable HTTP/HTTPS MCP server.
//!
//! This command is the user-facing entry point for the
//! "instance-as-MCP-server" feature. It:
//!
//! 1. resolves the named instance's `Config` (the `--instance` global
//!    flag flows through env into [`zunel_config::load_config`]);
//! 2. builds the default [`ToolRegistry`] for that instance,
//!    optionally subset-ing it via `--allow-write`/`--allow-exec`/
//!    `--allow-web`;
//! 3. wraps the registry in a [`RegistryDispatcher`];
//! 4. boots the same Streamable-HTTP/HTTPS server that
//!    `zunel-mcp-self` exposes, with stricter guard rails appropriate
//!    for a network-exposed surface (loopback bind, mandatory auth +
//!    HTTPS when binding non-loopback, Origin allowlist, call-depth
//!    cap, workspace foot-gun warning at boot).
//!
//! The transport layer lives in [`zunel_mcp_self::http`] so this
//! file stays focused on policy, not protocol.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zunel_config::Config;
use zunel_core::{build_default_registry_async, SessionManager};
use zunel_mcp_self::http::{self, ServerConfig, DEFAULT_MAX_CALL_DEPTH};
use zunel_mcp_self::open_access_log;
use zunel_tools::{ToolContext, ToolRegistry};

use crate::cli::McpAgentArgs;

use super::helper_ask::{HelperApprovalPolicy, HelperAskTool};
use super::registry_dispatcher::{DispatcherIdentity, RegistryDispatcher};

/// Tool names that mutate filesystem state. Hidden behind
/// `--allow-write`. `cron` mutates `cron/jobs.json` so it lives here
/// too: a network-exposed read-only registry should not be able to
/// schedule arbitrary jobs against the host's scheduler.
const WRITE_TOOLS: &[&str] = &["write_file", "edit_file", "cron"];

/// Tool names that execute arbitrary code. Hidden behind
/// `--allow-exec`.
const EXEC_TOOLS: &[&str] = &["exec"];

/// Tool names that touch the network. Hidden behind `--allow-web`.
const WEB_TOOLS: &[&str] = &["web_fetch", "web_search"];

pub(super) async fn run(args: McpAgentArgs, config_path: Option<&Path>) -> Result<()> {
    let cfg = zunel_config::load_config(config_path).context("loading zunel config")?;
    let workspace =
        zunel_config::workspace_path(&cfg.agents.defaults).context("resolving workspace path")?;
    zunel_config::guard_workspace(&workspace).context("validating workspace path")?;
    zunel_util::ensure_dir(&workspace)
        .with_context(|| format!("creating workspace dir {}", workspace.display()))?;

    let bind = args.bind.clone();
    let parsed_addr: std::net::SocketAddr = bind
        .parse()
        .with_context(|| format!("--bind expects HOST:PORT, got {bind:?}"))?;
    let api_keys = collect_api_keys(&args)?;
    enforce_loopback_guard(parsed_addr.ip(), &args, &api_keys)?;

    let instance = zunel_config::active_instance_name();

    // `--print-config` exits before the registry is built or any
    // socket is bound. We've already done flag-shape validation
    // (loopback guard, key parsing) so the snippet describes a
    // configuration the operator could actually boot, but we
    // intentionally short-circuit *before* TLS file I/O so users
    // can preview a snippet without having certs in place yet.
    // Stdout stays JSON-only; banners/warnings live on stderr.
    if args.print_config {
        let snippet = build_agent_config_snippet(&args, parsed_addr, &api_keys, &instance)?;
        println!("{}", serde_json::to_string_pretty(&snippet)?);
        return Ok(());
    }

    print_workspace_warning(&cfg, &workspace, parsed_addr.ip());

    let mut server_config = ServerConfig::default()
        .with_max_call_depth(args.max_call_depth.unwrap_or(DEFAULT_MAX_CALL_DEPTH));
    match (&args.https_cert, &args.https_key) {
        (Some(cert), Some(key)) => {
            let acceptor =
                http::build_tls_acceptor(cert, key).context("loading TLS certificate and key")?;
            server_config = server_config.with_tls(acceptor);
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("--https-cert and --https-key must be provided together");
        }
        _ => {}
    }
    if !api_keys.is_empty() {
        server_config = server_config.with_api_keys(api_keys);
    }
    if !args.allow_origin.is_empty() {
        server_config = server_config.with_allowed_origins(args.allow_origin.clone());
    }
    if let Some(raw) = args.max_body_bytes.as_deref() {
        let cap = parse_byte_count(raw).with_context(|| format!("--max-body-bytes was {raw:?}"))?;
        server_config = server_config.with_max_body_bytes(cap);
    }
    if let Some(path) = args.access_log.as_deref() {
        let log = open_access_log(path)
            .await
            .with_context(|| format!("opening --access-log at {path:?}"))?;
        server_config = server_config.with_access_log(log);
    }

    let mut registry = build_filtered_registry(&cfg, &workspace, &args).await;
    let mut cancel_registry: Option<Arc<super::cancel_registry::CancelRegistry>> = None;
    if args.mode2 {
        let approval_policy = HelperApprovalPolicy::from_cli_str(&args.mode2_approval)
            .map_err(|err| anyhow::anyhow!("invalid --mode2-approval value: {err}"))?;
        // Mode 2's helper_ask runs an inner AgentLoop with the same
        // tool surface Mode 1 exposes — *minus* helper_ask itself,
        // since wiring it in would make the loop self-recursive.
        // Cloning the registry is cheap (every tool is `Arc`-backed)
        // and keeps the inner-loop view immutable from Mode 1's POV.
        let inner_registry = registry.clone();
        let provider = zunel_providers::build_provider(&cfg)
            .await
            .context("building provider for Mode 2")?;
        let sessions = Arc::new(SessionManager::new(&workspace));
        // Shared cancel registry: helper_ask registers entries, the
        // dispatcher routes notifications/cancelled to the same
        // store. Both halves point at the same Arc so a single
        // cancel hits the right token.
        let registry_arc = super::cancel_registry::CancelRegistry::new();
        let call_timeout = args
            .mode2_call_timeout_secs
            .map(std::time::Duration::from_secs);

        // Approval-forwarding wiring. Build the queue and the
        // poll/resolve tools only when the operator opted in via
        // `--mode2-approval forward`; under reject/allow_all they
        // stay absent from the registry so the helper's surface
        // doesn't grow.
        let approval_queue = if matches!(approval_policy, HelperApprovalPolicy::Forward) {
            let queue = super::approval_queue::ApprovalQueue::new();
            registry.register(Arc::new(
                super::helper_approval_tools::HelperPendingApprovalsTool::new(Arc::clone(&queue)),
            ));
            registry.register(Arc::new(
                super::helper_approval_tools::HelperApproveTool::new(Arc::clone(&queue)),
            ));
            Some(queue)
        } else {
            None
        };

        let mut helper_ask = HelperAskTool::new(
            provider,
            cfg.agents.defaults.clone(),
            sessions,
            inner_registry,
            workspace.clone(),
            approval_policy,
            args.mode2_max_iterations,
        )
        .with_system_prompt_disabled(args.mode2_disable_system_prompt)
        .with_cancel_registry(Arc::clone(&registry_arc))
        .with_call_timeout(call_timeout)
        .with_approval_timeout(std::time::Duration::from_secs(
            args.mode2_approval_timeout_secs,
        ));
        if let Some(queue) = approval_queue {
            helper_ask = helper_ask.with_approval_queue(queue);
        }
        registry.register(Arc::new(helper_ask));
        cancel_registry = Some(registry_arc);
    }
    let identity = DispatcherIdentity {
        server_name: format!("zunel-agent:{instance}"),
        server_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let session_key = format!("mcp-agent:{instance}");
    let context = ToolContext::new_with_workspace(workspace.clone(), session_key);
    let mut dispatcher = RegistryDispatcher::new(identity, registry, context);
    if let Some(reg) = cancel_registry {
        dispatcher = dispatcher.with_cancel_registry(reg);
    }

    eprintln!(
        "zunel mcp agent: serving {} tool(s) for instance {instance} (workspace: {})",
        dispatcher.tools_for_banner(),
        workspace.display()
    );

    let shutdown = CancellationToken::new();
    {
        // Watch SIGINT/SIGTERM in a sibling task so the server's
        // accept loop keeps polling until the signal lands. The
        // watcher resolves once and then drops out of the runtime.
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            http::wait_for_shutdown_signal().await;
            eprintln!("zunel mcp agent: shutdown signal received, draining...");
            shutdown.cancel();
        });
    }
    http::run(&bind, server_config, dispatcher, shutdown).await
}

/// Parse a byte count for `--max-body-bytes`. Accepts plain integers
/// (`4194304`) and the `K`/`M`/`G` suffixes (base-1024) so operators
/// don't have to count zeroes. Mirrors the parser in
/// `zunel-mcp-self`'s binary so the two CLIs feel the same.
fn parse_byte_count(raw: &str) -> Result<usize> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("byte count was empty");
    }
    let (digits, multiplier) = match trimmed.as_bytes().last().copied() {
        Some(b'k') | Some(b'K') => (&trimmed[..trimmed.len() - 1], 1024_usize),
        Some(b'm') | Some(b'M') => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        Some(b'g') | Some(b'G') => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
        _ => (trimmed, 1_usize),
    };
    let n: usize = digits
        .trim()
        .parse()
        .with_context(|| format!("byte count {trimmed:?} was not a positive integer"))?;
    n.checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("byte count {trimmed:?} overflows usize"))
}

/// Construct the `tools.mcpServers.<name>` snippet emitted by
/// `--print-config`. The shape mirrors the smoke test's hand-rolled
/// config so a copy-paste of this output into another instance's
/// `config.json` is a working hub registration.
///
/// Secrets policy: when `api_keys` is non-empty we always emit a
/// `${VAR}` placeholder rather than the literal token, which means
/// `zunel mcp agent --print-config > snippet.json` is safe to commit
/// or share. When no keys are configured (loopback-only deployments)
/// we omit `headers` entirely.
fn build_agent_config_snippet(
    args: &McpAgentArgs,
    bind: SocketAddr,
    api_keys: &[String],
    instance: &str,
) -> Result<Value> {
    let entry_name = args
        .public_name
        .clone()
        .unwrap_or_else(|| instance.to_string());
    let scheme = if args.https_cert.is_some() && args.https_key.is_some() {
        "https"
    } else {
        "http"
    };
    let url = match args.public_url.as_deref() {
        Some(explicit) => explicit.to_string(),
        None => format_public_url(scheme, bind),
    };

    let mut entry = json!({
        "type": "streamableHttp",
        "url": url,
    });
    if !api_keys.is_empty() {
        let env_name = args
            .public_env
            .clone()
            .unwrap_or_else(|| default_token_env_name(instance));
        validate_env_var_name(&env_name)?;
        entry["headers"] = json!({
            "Authorization": format!("Bearer ${{{env_name}}}"),
        });
    }

    Ok(json!({
        "tools": {
            "mcpServers": {
                entry_name: entry,
            },
        },
    }))
}

/// Format the URL to embed in a `--print-config` snippet. Wildcard
/// hosts (`0.0.0.0`, `::`) get a `<HOST>` placeholder because they
/// aren't routable from another machine; port `0` (OS-pick) gets
/// `<PORT>` because the chosen port isn't known until bind. Operators
/// with stable URLs should pass `--public-url` instead.
fn format_public_url(scheme: &str, addr: SocketAddr) -> String {
    let host = match addr.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => "<HOST>".to_string(),
        IpAddr::V6(v6) if v6.is_unspecified() => "<HOST>".to_string(),
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let port = if addr.port() == 0 {
        "<PORT>".to_string()
    } else {
        addr.port().to_string()
    };
    format!("{scheme}://{host}:{port}/")
}

/// Default env-var name for the bearer-token placeholder. Mirrors the
/// `ZUNEL_<INSTANCE>_TOKEN` pattern documented in `self-tool.md` and
/// used by the smoke tests, with non-alphanumerics folded to `_` so
/// instance names like `helper-1` become `ZUNEL_HELPER_1_TOKEN`.
fn default_token_env_name(instance: &str) -> String {
    let mut cleaned = String::with_capacity(instance.len());
    for ch in instance.chars() {
        if ch.is_ascii_alphanumeric() {
            cleaned.push(ch.to_ascii_uppercase());
        } else {
            cleaned.push('_');
        }
    }
    if cleaned.is_empty() {
        cleaned.push_str("INSTANCE");
    }
    format!("ZUNEL_{cleaned}_TOKEN")
}

/// Sanity-check a custom env-var name from `--public-env`. We accept
/// the conventional `[A-Za-z_][A-Za-z0-9_]*` shape because the
/// downstream `${VAR}` parser in `expand_header_envs` follows the same
/// shell-style convention; rejecting bad names early is a much better
/// failure mode than emitting a snippet that silently drops a header.
fn validate_env_var_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("--public-env was empty");
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty checked above");
    if !(first.is_ascii_alphabetic() || first == '_') {
        anyhow::bail!("--public-env {name:?} must start with an ASCII letter or underscore");
    }
    for ch in chars {
        if !(ch.is_ascii_alphanumeric() || ch == '_') {
            anyhow::bail!(
                "--public-env {name:?} may only contain ASCII letters, digits, and underscores"
            );
        }
    }
    Ok(())
}

/// Build the registry for the agent server. Starts from the instance's
/// default registry and prunes anything outside the requested gate set.
/// `mcp_*` tools are always pruned: the depth-forwarding plumbing is in
/// place, but the design doc defers MCP-of-MCP re-export pending product
/// decisions about per-call timeouts, OAuth-token visibility across
/// hops, and audit logging.
async fn build_filtered_registry(
    cfg: &Config,
    workspace: &Path,
    args: &McpAgentArgs,
) -> ToolRegistry {
    let raw = build_default_registry_async(cfg, workspace).await;

    let mut blocked: BTreeSet<&str> = BTreeSet::new();
    if !args.allow_write {
        blocked.extend(WRITE_TOOLS.iter().copied());
    }
    if !args.allow_exec {
        blocked.extend(EXEC_TOOLS.iter().copied());
    }
    if !args.allow_web {
        blocked.extend(WEB_TOOLS.iter().copied());
    }

    let mut filtered = ToolRegistry::new();
    for name in raw.names().map(str::to_string).collect::<Vec<_>>() {
        if name.starts_with("mcp_") {
            // Re-enabling this is a one-line change once the product
            // questions captured in `instance-as-mcp.md` (Limitations
            // §1) are answered. The runtime-side guard rails (depth
            // header forwarding, origin allowlist, depth cap) are
            // already in place.
            continue;
        }
        if blocked.contains(name.as_str()) {
            continue;
        }
        if let Some(tool) = raw.get(&name) {
            filtered.register(Arc::clone(tool));
        }
    }
    filtered
}

/// Read the API-key allowlist from CLI flags + file inputs. Mirrors
/// the parser in `zunel-mcp-self`'s binary so operators get the same
/// rotation ergonomics.
fn collect_api_keys(args: &McpAgentArgs) -> Result<Vec<String>> {
    let mut tokens: Vec<String> = args.api_key.clone();
    for path in &args.api_key_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading API key file at {}", path.display()))?;
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            tokens.push(trimmed.to_string());
        }
    }
    tokens.retain(|t| !t.trim().is_empty());
    Ok(tokens)
}

/// Hard-fail when binding a non-loopback address without both HTTPS
/// and at least one API key. The doc calls this out as the single
/// most important guardrail for the agent surface — without it,
/// `--bind 0.0.0.0:9000` would hand a machine-credentialed shell to
/// anyone reachable on the LAN.
fn enforce_loopback_guard(ip: IpAddr, args: &McpAgentArgs, api_keys: &[String]) -> Result<()> {
    if ip.is_loopback() {
        return Ok(());
    }
    let has_tls = args.https_cert.is_some() && args.https_key.is_some();
    let has_auth = !api_keys.is_empty();
    if has_tls && has_auth {
        return Ok(());
    }
    let mut missing: Vec<&str> = Vec::new();
    if !has_tls {
        missing.push("--https-cert/--https-key");
    }
    if !has_auth {
        missing.push("--api-key/--api-key-file");
    }
    anyhow::bail!(
        "refusing to expose `zunel mcp agent` on non-loopback address {ip} without {}. \
         Either bind to 127.0.0.1 (loopback) or supply the missing flag(s).",
        missing.join(" and ")
    );
}

/// Print a one-line warning when the workspace looks like an active
/// dev tree (has a `.git` directory) and the bind isn't loopback.
/// Doesn't bail — operators may genuinely want to expose a working
/// repo over the LAN — but the prompt makes the foot-gun loud.
fn print_workspace_warning(_cfg: &Config, workspace: &Path, ip: IpAddr) {
    if ip.is_loopback() {
        return;
    }
    let git_dir = workspace.join(".git");
    if !git_dir.exists() {
        return;
    }
    eprintln!(
        "warning: workspace {} is a git repository and the agent server is bound to a \
         non-loopback address ({ip}). Any client with a valid API key will be able to \
         read (and, with --allow-write, modify) files in this tree.",
        workspace.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Helper: build an `McpAgentArgs` from the same argv shape the
    /// CLI parser sees, so test cases stay close to the real command
    /// line. We embed the args in a tiny driver struct because
    /// `McpAgentArgs` itself doesn't carry the global `--instance`/etc.
    /// surface clap expects at the top level.
    #[derive(Parser, Debug)]
    struct Driver {
        #[clap(flatten)]
        args: McpAgentArgs,
    }
    fn parse_args(extra: &[&str]) -> McpAgentArgs {
        let mut argv: Vec<&str> = vec!["test"];
        argv.extend_from_slice(extra);
        Driver::parse_from(argv).args
    }

    #[test]
    fn loopback_no_auth_snippet_omits_headers_and_uses_http() {
        let args = parse_args(&[]);
        let bind: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let snippet = build_agent_config_snippet(&args, bind, &[], "default").unwrap();
        let entry = &snippet["tools"]["mcpServers"]["default"];
        assert_eq!(entry["type"], "streamableHttp");
        assert_eq!(entry["url"], "http://127.0.0.1:9000/");
        assert!(
            entry.get("headers").is_none(),
            "loopback-no-auth must not emit headers; got {entry:#?}"
        );
    }

    #[test]
    fn https_with_api_key_emits_bearer_placeholder_not_secret() {
        let args = parse_args(&["--https-cert", "cert.pem", "--https-key", "key.pem"]);
        let bind: SocketAddr = "0.0.0.0:9000".parse().unwrap();
        let keys = vec!["super-secret-token".to_string()];
        let snippet = build_agent_config_snippet(&args, bind, &keys, "helper-1").unwrap();
        let entry = &snippet["tools"]["mcpServers"]["helper-1"];
        assert_eq!(entry["url"], "https://<HOST>:9000/");
        assert_eq!(
            entry["headers"]["Authorization"],
            "Bearer ${ZUNEL_HELPER_1_TOKEN}"
        );
        let serialized = serde_json::to_string(&snippet).unwrap();
        assert!(
            !serialized.contains("super-secret-token"),
            "snippet must never include the literal API key"
        );
    }

    #[test]
    fn public_url_override_replaces_bind_url() {
        let args = parse_args(&[
            "--print-config",
            "--public-url",
            "https://agent.example.com/",
        ]);
        let bind: SocketAddr = "0.0.0.0:9000".parse().unwrap();
        let snippet =
            build_agent_config_snippet(&args, bind, &["k".to_string()], "default").unwrap();
        assert_eq!(
            snippet["tools"]["mcpServers"]["default"]["url"],
            "https://agent.example.com/"
        );
    }

    #[test]
    fn public_env_override_changes_placeholder() {
        let args = parse_args(&["--print-config", "--public-env", "MY_TOKEN"]);
        let bind: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let snippet =
            build_agent_config_snippet(&args, bind, &["k".to_string()], "default").unwrap();
        assert_eq!(
            snippet["tools"]["mcpServers"]["default"]["headers"]["Authorization"],
            "Bearer ${MY_TOKEN}"
        );
    }

    #[test]
    fn public_name_override_renames_mcpservers_key() {
        let args = parse_args(&["--print-config", "--public-name", "myhelper"]);
        let bind: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let snippet = build_agent_config_snippet(&args, bind, &[], "default").unwrap();
        assert!(snippet["tools"]["mcpServers"]["myhelper"].is_object());
        assert!(snippet["tools"]["mcpServers"]["default"].is_null());
    }

    #[test]
    fn port_zero_in_bind_renders_port_placeholder() {
        let args = parse_args(&[]);
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let snippet = build_agent_config_snippet(&args, bind, &[], "default").unwrap();
        assert_eq!(
            snippet["tools"]["mcpServers"]["default"]["url"],
            "http://127.0.0.1:<PORT>/"
        );
    }

    #[test]
    fn ipv6_bind_renders_bracketed_host() {
        let args = parse_args(&[]);
        let bind: SocketAddr = "[::1]:9000".parse().unwrap();
        let snippet = build_agent_config_snippet(&args, bind, &[], "default").unwrap();
        assert_eq!(
            snippet["tools"]["mcpServers"]["default"]["url"],
            "http://[::1]:9000/"
        );
    }

    #[test]
    fn default_token_env_normalizes_instance_chars() {
        assert_eq!(default_token_env_name("default"), "ZUNEL_DEFAULT_TOKEN");
        assert_eq!(default_token_env_name("helper-1"), "ZUNEL_HELPER_1_TOKEN");
        assert_eq!(default_token_env_name("a.b/c"), "ZUNEL_A_B_C_TOKEN");
        assert_eq!(default_token_env_name(""), "ZUNEL_INSTANCE_TOKEN");
    }

    #[test]
    fn validate_env_var_name_rejects_bogus_inputs() {
        assert!(validate_env_var_name("FOO").is_ok());
        assert!(validate_env_var_name("_FOO").is_ok());
        assert!(validate_env_var_name("FOO_BAR_1").is_ok());
        assert!(validate_env_var_name("").is_err());
        assert!(validate_env_var_name("1FOO").is_err());
        assert!(validate_env_var_name("FOO BAR").is_err());
        assert!(validate_env_var_name("FOO-BAR").is_err());
    }
}
