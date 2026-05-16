mod approval_cli;
mod cli;
mod commands;
mod oauth_callback;
mod renderer;
mod repl;
mod spinner;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    install_default_crypto_provider();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    apply_instance_override(cli.instance.as_deref())?;
    apply_unsafe_workspace_override(cli.i_know_what_im_doing);
    match cli.command {
        Command::Onboard(args) => commands::onboard::run(args).await?,
        Command::Agent(args) => commands::agent::run(args, cli.config.as_deref()).await?,
        Command::Gateway(args) => commands::gateway::run(args, cli.config.as_deref()).await?,
        Command::Status => commands::status::run(cli.config.as_deref()).await?,
        Command::Mcp(args) => commands::mcp::run(args, cli.config.as_deref()).await?,
        Command::Instance(args) => commands::instance::run(args).await?,
        Command::Slack(args) => commands::slack::run(args, cli.config.as_deref()).await?,
        Command::Channels(args) => commands::channels::run(args, cli.config.as_deref()).await?,
        Command::Sessions(args) => commands::sessions::run(args, cli.config.as_deref()).await?,
        Command::Tokens(args) => commands::tokens::run(args, cli.config.as_deref()).await?,
    }
    Ok(())
}

/// Forward `--i-know-what-im-doing` into the env-var the guard in
/// `zunel-config::paths::guard_workspace` actually reads. Pulling
/// the toggle through the env keeps the guard reusable from
/// places that don't see the CLI flag (tests, future bins).
/// Pre-set values are left alone — operators who already exported
/// the env var get the same behavior with or without the flag.
fn apply_unsafe_workspace_override(flag: bool) {
    if !flag {
        return;
    }
    if std::env::var_os(zunel_config::UNSAFE_WORKSPACE_ENV).is_some() {
        return;
    }
    std::env::set_var(zunel_config::UNSAFE_WORKSPACE_ENV, "1");
}

/// Pin rustls 0.23's process-wide CryptoProvider to `ring`.
///
/// Without this, rustls panics at first use whenever multiple
/// `rustls/{ring,aws_lc_rs}` features are enabled in the dep graph
/// — which happens once `aws-config` is in the tree. The AWS SDK's
/// `aws-smithy-http-client` enables `rustls/aws_lc_rs`, while
/// `reqwest`, `tokio-tungstenite`, and our own `oauth_callback` +
/// `zunel-mcp-self::http` enable `rustls/ring`. zunel-cli pins
/// rustls with the `ring` feature, so installing the ring provider
/// keeps our existing `ServerConfig::builder()` calls working
/// unchanged. The AWS SDK's HTTPS client always builds its config
/// with `rustls/aws_lc_rs` explicitly, so it doesn't depend on the
/// installed default.
///
/// Idempotent: `install_default` returns `Err` on the second call,
/// which we swallow.
fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn apply_instance_override(instance: Option<&str>) -> Result<()> {
    if std::env::var_os("ZUNEL_HOME").is_some() {
        return Ok(());
    }
    let Some(instance) = instance else {
        return Ok(());
    };
    match zunel_config::resolve_instance_home(instance) {
        Ok(home) => {
            std::env::set_var("ZUNEL_HOME", home);
            Ok(())
        }
        Err(err @ zunel_config::Error::InvalidInstanceName(_)) => {
            eprintln!("Error: {err}");
            std::process::exit(2);
        }
        Err(err) => Err(err.into()),
    }
}
