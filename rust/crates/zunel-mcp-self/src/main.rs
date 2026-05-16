//! `zunel-mcp-self` entrypoint.
//!
//! Two transports are supported:
//!
//! - **stdio** (default) — used when zunel itself spawns the binary as
//!   a subprocess.
//! - **Streamable HTTP / HTTPS** — opt in with `--http <addr>` or
//!   `ZUNEL_MCP_SELF_HTTP=<addr>`. Hosts the MCP server on a single
//!   `POST /` endpoint, replying with either `application/json` or
//!   `text/event-stream` depending on the client's `Accept` header.
//!   Pair with `--https-cert`/`--https-key` to terminate TLS in-process,
//!   and `--api-key` (or `ZUNEL_MCP_SELF_API_KEY`) to gate every
//!   request behind a bearer token.
//!
//! Both transports route through the [`SelfDispatcher`] so the
//! tool surface stays identical across modes.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;
use zunel_mcp::{read_frame, write_frame};
use zunel_mcp_self::{http, open_access_log, DispatchMeta, McpDispatcher, SelfDispatcher};

#[tokio::main]
async fn main() -> Result<()> {
    install_default_crypto_provider();
    let cli = Cli::parse_from_env()?;
    if let Some(addr) = cli.http_addr {
        let mut config = http::ServerConfig::default();
        match (cli.tls_cert, cli.tls_key) {
            (Some(cert), Some(key)) => {
                let acceptor = http::build_tls_acceptor(&cert, &key)
                    .context("loading TLS certificate and key")?;
                config = config.with_tls(acceptor);
            }
            (Some(_), None) | (None, Some(_)) => {
                anyhow::bail!(
                    "--https-cert and --https-key must be provided together \
                     (or set both ZUNEL_MCP_SELF_TLS_CERT and ZUNEL_MCP_SELF_TLS_KEY)"
                );
            }
            _ => {}
        }
        if !cli.api_keys.is_empty() {
            config = config.with_api_keys(cli.api_keys);
        }
        if let Some(cap) = cli.max_body_bytes {
            config = config.with_max_body_bytes(cap);
        }
        if let Some(path) = cli.access_log.as_deref() {
            let log = open_access_log(path)
                .await
                .with_context(|| format!("opening access log at {path:?}"))?;
            config = config.with_access_log(log);
        }
        let shutdown = CancellationToken::new();
        spawn_signal_watcher(shutdown.clone());
        return http::run(&addr, config, SelfDispatcher::new(), shutdown).await;
    }
    run_stdio().await
}

/// Pin rustls 0.23's process-wide CryptoProvider to `ring`.
///
/// See `zunel-cli/src/main.rs::install_default_crypto_provider` for
/// the full explanation. Mirrored here so the standalone HTTPS
/// transport (`zunel-mcp-self --http <addr> --https-cert ... --https-key ...`)
/// gets the same disambiguation when running outside the main
/// `zunel` binary — both bins consume `aws-config` transitively
/// through `zunel-providers`, so both need to pin the provider.
fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Spawn a small watcher task that flips `shutdown` when the
/// process receives SIGINT or (on Unix) SIGTERM. Lives in its own
/// helper so [`main`] reads top-down and the binary's behavior
/// stays aligned with the agent CLI's matching watcher.
fn spawn_signal_watcher(shutdown: CancellationToken) {
    tokio::spawn(async move {
        http::wait_for_shutdown_signal().await;
        tracing::info!("zunel-mcp-self: shutdown signal received, draining...");
        shutdown.cancel();
    });
}

/// Resolved command-line + environment configuration. All HTTP-mode
/// knobs live here so [`main`] stays a thin dispatcher; arg parsing
/// is intentionally hand-rolled to avoid pulling clap into a binary
/// that exposes only a tiny option surface.
#[derive(Default, Debug)]
struct Cli {
    /// Bind address for the HTTP transport. `None` selects stdio.
    http_addr: Option<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    /// Bearer-token allowlist. `--api-key`/`ZUNEL_MCP_SELF_API_KEY`
    /// contribute one entry each; `--api-key-file` reads multiple
    /// entries from disk (one per line). All three sources stack so
    /// rotation can keep an env-var token live while a file-based
    /// successor is rolled out, or vice-versa.
    api_keys: Vec<String>,
    /// Override [`http::DEFAULT_MAX_BODY_BYTES`]. `None` keeps the
    /// library default. The value is interpreted as raw bytes (no
    /// `1M`/`16K` suffixes); the helper `parse_bytes` lives in this
    /// module so the binary's CLI surface stays self-contained.
    max_body_bytes: Option<usize>,
    /// Where to emit per-request JSON-line access logs. `None`
    /// disables access logging entirely. `Some("-")` is the
    /// convention for "write to stdout" — useful when running under
    /// `journalctl`/`docker logs` — and any other value is treated
    /// as a file path opened in append mode.
    access_log: Option<String>,
}

impl Cli {
    fn parse_from_env() -> Result<Self> {
        let mut cli = Cli {
            http_addr: env_optional("ZUNEL_MCP_SELF_HTTP"),
            tls_cert: env_optional("ZUNEL_MCP_SELF_TLS_CERT").map(PathBuf::from),
            tls_key: env_optional("ZUNEL_MCP_SELF_TLS_KEY").map(PathBuf::from),
            api_keys: Vec::new(),
            max_body_bytes: match env_optional("ZUNEL_MCP_SELF_MAX_BODY_BYTES") {
                Some(value) => Some(
                    parse_byte_count(&value)
                        .with_context(|| format!("ZUNEL_MCP_SELF_MAX_BODY_BYTES was {value:?}"))?,
                ),
                None => None,
            },
            access_log: env_optional("ZUNEL_MCP_SELF_ACCESS_LOG"),
        };
        if let Some(token) = env_optional("ZUNEL_MCP_SELF_API_KEY") {
            cli.api_keys.push(token);
        }
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--http" => cli.http_addr = next_value(&mut args, "--http")?.into(),
                value if value.starts_with("--http=") => {
                    cli.http_addr = Some(value["--http=".len()..].to_string());
                }
                "--https-cert" => {
                    cli.tls_cert = Some(PathBuf::from(next_value(&mut args, "--https-cert")?));
                }
                value if value.starts_with("--https-cert=") => {
                    cli.tls_cert = Some(PathBuf::from(&value["--https-cert=".len()..]));
                }
                "--https-key" => {
                    cli.tls_key = Some(PathBuf::from(next_value(&mut args, "--https-key")?));
                }
                value if value.starts_with("--https-key=") => {
                    cli.tls_key = Some(PathBuf::from(&value["--https-key=".len()..]));
                }
                "--api-key" => cli.api_keys.push(next_value(&mut args, "--api-key")?),
                value if value.starts_with("--api-key=") => {
                    cli.api_keys.push(value["--api-key=".len()..].to_string());
                }
                "--api-key-file" => {
                    let path = next_value(&mut args, "--api-key-file")?;
                    cli.api_keys.extend(read_token_file(&path)?);
                }
                value if value.starts_with("--api-key-file=") => {
                    cli.api_keys
                        .extend(read_token_file(&value["--api-key-file=".len()..])?);
                }
                "--max-body-bytes" => {
                    let raw = next_value(&mut args, "--max-body-bytes")?;
                    cli.max_body_bytes = Some(parse_byte_count(&raw)?);
                }
                value if value.starts_with("--max-body-bytes=") => {
                    cli.max_body_bytes =
                        Some(parse_byte_count(&value["--max-body-bytes=".len()..])?);
                }
                "--access-log" => {
                    cli.access_log = Some(next_value(&mut args, "--access-log")?);
                }
                value if value.starts_with("--access-log=") => {
                    cli.access_log = Some(value["--access-log=".len()..].to_string());
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown argument: {other}"),
            }
        }
        // Drop empty strings so callers can blank out an inherited env
        // var without disabling the option's CLI form too.
        cli.http_addr = cli.http_addr.filter(|v| !v.is_empty());
        cli.api_keys.retain(|value| !value.trim().is_empty());
        cli.access_log = cli.access_log.filter(|v| !v.is_empty());
        Ok(cli)
    }
}

fn env_optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &'static str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

/// Parse a byte count for `--max-body-bytes`. Accepts plain integers
/// (`4194304`) plus the friendly `K`/`M`/`G` suffixes
/// (`16K`/`4M`/`1G`, case-insensitive, base-1024) so operators don't
/// have to count zeroes when overriding the default. Hex / scientific
/// notation are deliberately rejected to keep the surface narrow.
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

/// Parse an API-key file. The format is intentionally simple so it
/// can be hand-edited during a rotation:
///
/// - one bearer token per non-blank line;
/// - leading/trailing whitespace per line is stripped;
/// - lines whose **first non-whitespace character** is `#` are
///   comments and ignored;
/// - blank lines are ignored.
///
/// All surviving lines are returned in source order so the operator
/// controls precedence (only meaningful for diagnostics, since
/// `is_authorized` walks the entire list in constant time per
/// request).
fn read_token_file(path: &str) -> Result<Vec<String>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading API key file at {path}"))?;
    let tokens: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToString::to_string)
        .collect();
    if tokens.is_empty() {
        anyhow::bail!(
            "no bearer tokens found in {path} (expected one token per line; \
             '#' comments and blank lines are skipped)"
        );
    }
    Ok(tokens)
}

fn print_help() {
    println!(
        "zunel-mcp-self {} — built-in self-inspection MCP server\n\n\
         USAGE:\n  zunel-mcp-self                 # stdio transport (default)\n  \
         zunel-mcp-self --http <addr>   # Streamable HTTP/HTTPS transport\n\n\
         OPTIONS:\n  --http <addr>           Bind the HTTP transport (e.g. 127.0.0.1:0)\n  \
         --https-cert <path>     PEM-encoded TLS certificate (enables HTTPS)\n  \
         --https-key <path>      PEM-encoded TLS private key  (enables HTTPS)\n  \
         --api-key <token>       Require Authorization: Bearer <token> (or X-API-Key);\n                            \
                            may be repeated to allow several tokens at once\n  \
         --api-key-file <path>   Read tokens from a file, one per line\n                            \
                            ('#' comments and blank lines are skipped); may be repeated\n  \
         --max-body-bytes <N>    Max accepted request body, in bytes (also K/M/G\n                            \
                            suffixes, base-1024). Default {default} bytes.\n                            \
                            Oversized requests are rejected with 413 before the body\n                            \
                            is read off the socket.\n  \
         --access-log <path>     Emit one JSON line per served request to <path>.\n                            \
                            Use '-' for stdout. The file is opened in append mode\n                            \
                            so logrotate copytruncate works without a SIGHUP.\n  \
         -h, --help              Show this help and exit\n\n\
         ENVIRONMENT:\n  ZUNEL_MCP_SELF_HTTP            Default for --http\n  \
         ZUNEL_MCP_SELF_TLS_CERT        Default for --https-cert\n  \
         ZUNEL_MCP_SELF_TLS_KEY         Default for --https-key\n  \
         ZUNEL_MCP_SELF_API_KEY         Adds one token to the allowlist\n  \
         ZUNEL_MCP_SELF_MAX_BODY_BYTES  Default for --max-body-bytes\n  \
         ZUNEL_MCP_SELF_ACCESS_LOG      Default for --access-log\n\n\
         Multiple --api-key, --api-key-file, and ZUNEL_MCP_SELF_API_KEY values\n         \
         stack so a key rotation can keep the old token live while clients adopt\n         \
         the replacement; remove the retired entry once every client is on the\n         \
         new key.\n\n\
         The server stops cleanly on SIGINT (Ctrl-C) and SIGTERM, allowing in-flight\n         \
         connections up to a 5s grace period before aborting them.",
        env!("CARGO_PKG_VERSION"),
        default = http::DEFAULT_MAX_BODY_BYTES,
    );
}

async fn run_stdio() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let dispatcher = SelfDispatcher::new();
    // stdio has no transport-level metadata; all inbound calls are
    // top-level by definition.
    let meta = DispatchMeta::default();

    loop {
        let msg = match read_frame(&mut reader).await {
            Ok(msg) => msg,
            Err(_) => break,
        };
        // Use the dispatcher trait so the stdio transport stays
        // semantically identical to the HTTP transport.
        if let Some(response) = dispatcher.dispatch(&msg, &meta).await {
            write_frame(&mut stdout, &response).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn read_token_file_parses_multiple_tokens_with_comments() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "# active key").unwrap();
        writeln!(file, "key-one").unwrap();
        writeln!(file).unwrap();
        writeln!(file, "  key-two  ").unwrap();
        writeln!(file, "# next-rotation key (still pending)").unwrap();
        writeln!(file, "key-three").unwrap();
        let path = file.path().to_string_lossy().to_string();
        let tokens = read_token_file(&path).unwrap();
        assert_eq!(tokens, vec!["key-one", "key-two", "key-three"]);
    }

    #[test]
    fn read_token_file_rejects_empty_files() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "# only comments").unwrap();
        writeln!(file).unwrap();
        let path = file.path().to_string_lossy().to_string();
        let err = read_token_file(&path).unwrap_err();
        assert!(format!("{err:#}").contains("no bearer tokens"));
    }

    #[test]
    fn parse_byte_count_handles_plain_integers_and_suffixes() {
        assert_eq!(parse_byte_count("0").unwrap(), 0);
        assert_eq!(parse_byte_count("128").unwrap(), 128);
        assert_eq!(parse_byte_count("1K").unwrap(), 1024);
        assert_eq!(parse_byte_count("4k").unwrap(), 4 * 1024);
        assert_eq!(parse_byte_count("8M").unwrap(), 8 * 1024 * 1024);
        assert_eq!(parse_byte_count("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_byte_count("  16M  ").unwrap(), 16 * 1024 * 1024);
    }

    #[test]
    fn parse_byte_count_rejects_garbage_and_overflow() {
        assert!(parse_byte_count("").is_err());
        assert!(parse_byte_count("not-a-number").is_err());
        // -1 is rejected because the parser targets `usize`; this
        // also keeps "negative cap" out of the surface.
        assert!(parse_byte_count("-1").is_err());
        // usize::MAX in K-units overflows.
        let huge = format!("{}K", usize::MAX);
        assert!(parse_byte_count(&huge).is_err());
    }
}
