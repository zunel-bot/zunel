//! `zunel mcp login`: PKCE-based OAuth flow for remote MCP servers.
//!
//! Since v0.2.6 this command is a thin orchestrator over the
//! library functions in [`zunel_mcp::oauth`]:
//!
//! 1. Resolve cached-token state and bail early when one already
//!    exists and `--force` wasn't passed.
//! 2. Call [`zunel_mcp::oauth_start_flow`] which discovers the
//!    IdP endpoints, dynamic-client-registers if needed, generates
//!    the PKCE verifier, persists `pending.json`, and returns the
//!    authorize URL.
//! 3. Bind a local callback server (loopback `http://` or `https://`)
//!    or fall back to the manual paste flow when the redirect URI
//!    isn't loopback or `--url` was supplied.
//! 4. Call [`zunel_mcp::oauth_complete_flow`] with whichever callback
//!    URL we received. The library does state validation, code
//!    exchange, atomic token persistence, and pending-file cleanup.
//!
//! All cryptography, IdP I/O, and `~/.zunel/mcp-oauth/<server>/...`
//! file writes live in [`zunel_mcp::oauth`]; this command exists
//! purely to wire the local browser, callback server, and stdin
//! paste experience to that library.

use std::path::Path;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use zunel_config::mcp_oauth::mcp_oauth_token_path as shared_token_path;
use zunel_mcp::oauth as mcp_oauth;

use crate::cli::McpLoginArgs;
use crate::oauth_callback::{bind_callback_server, open_browser};

pub(super) async fn login(args: McpLoginArgs, config_path: Option<&Path>) -> Result<()> {
    let cfg = zunel_config::load_config(config_path).context("loading config")?;
    let server = cfg
        .tools
        .mcp_servers
        .get(&args.server)
        .with_context(|| format!("unknown MCP server '{}'", args.server))?;
    if server.url.is_none() {
        bail!("MCP server '{}' is not a remote server", args.server);
    }
    if matches!(server.normalized_oauth(), Some(ref oauth) if !oauth.enabled) {
        bail!("MCP server '{}' has OAuth disabled", args.server);
    }

    let home = zunel_config::zunel_home().context("resolving zunel home directory")?;
    let token_path = shared_token_path(&home, &args.server);
    if token_path.exists() && !args.force {
        println!(
            "MCP server '{}' already has a cached token at {}. Use --force to re-authenticate.",
            args.server,
            token_path.display()
        );
        println!(
            "Note: zunel will auto-refresh expired access tokens via the refresh_token grant; \
             --force is only needed if you've actually been signed out at the IdP."
        );
        return Ok(());
    }

    let started = mcp_oauth::start_flow(&home, &cfg, &args.server, args.state.as_deref())
        .await
        .context("starting MCP OAuth flow")?;

    println!(
        "Open this URL in your browser to authenticate '{}':",
        args.server
    );
    println!("{}", started.authorize_url);

    // The user can pre-supply the callback (for tests / CI / manual
    // paste-back) via `--url`. When that's set the loopback server is
    // never bound; otherwise the auto-loopback flow runs.
    let callback_server = if args.url_in.is_none() {
        bind_callback_server(&started.redirect_uri).await?
    } else {
        None
    };

    let callback_url = match args.url_in {
        Some(url) => url,
        None if callback_server.is_some() => {
            let server = callback_server.expect("checked is_some");
            open_browser(&started.authorize_url);
            println!("Waiting for OAuth callback on {}...", started.redirect_uri);
            server.wait_for_callback().await?
        }
        None => {
            println!("Paste the full callback URL here:");
            read_stdin_line().await?
        }
    };

    let completed = mcp_oauth::complete_flow(&home, &cfg, &args.server, &callback_url)
        .await
        .context("completing MCP OAuth flow")?;
    println!(
        "Cached OAuth token for '{}' at {}.",
        completed.server,
        completed.token_path.display()
    );
    Ok(())
}

async fn read_stdin_line() -> Result<String> {
    let mut line = String::new();
    let mut reader = BufReader::new(tokio::io::stdin());
    reader.read_line(&mut line).await?;
    Ok(line.trim().to_string())
}
