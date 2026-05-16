//! Streamable HTTP transport for `zunel-mcp-self`.
//!
//! The server speaks the MCP "Streamable HTTP" transport on a single
//! `POST /` endpoint. The client posts a JSON-RPC request (or batch);
//! we dispatch via [`crate::handlers::handle_message`] and reply with
//! either:
//!
//! - `application/json` (single-shot result), or
//! - `text/event-stream` with one `message` event carrying the
//!   JSON-RPC response — preferred when the client's `Accept` header
//!   includes `text/event-stream` so the response is delivered as a
//!   bona-fide HTTP stream.
//!
//! Notifications (no `id`) get a bare `202 Accepted` per the spec.
//! `GET /` and `DELETE /` are stubbed: we do not yet push
//! server-initiated SSE streams or expose session teardown.
//!
//! Security knobs:
//!
//! - **TLS**: pass a [`ServerConfig::tls`] cert+key pair to terminate
//!   HTTPS in-process. Without it, the listener serves plain HTTP.
//! - **API key**: pass [`ServerConfig::api_key`] to require the same
//!   bearer token (constant-time compared) on every `POST /` request.
//!
//! The implementation parses HTTP/1.1 manually rather than pulling in
//! `axum`/`hyper`, matching the lightweight pattern used elsewhere in
//! the repo (see `zunel-cli/src/oauth_callback.rs`). Each accepted
//! connection runs in its own task so handlers can overlap.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig as RustlsServerConfig;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use crate::access_log::{token_fingerprint, AccessLog, AccessLogContext};
use crate::handlers::SERVER_NAME;
use crate::{DispatchMeta, McpDispatcher};

/// Default cap on `Mcp-Call-Depth`. Requests presenting a depth `>=`
/// this value are rejected with `403`. Generous for nested
/// agent-to-agent fan-outs but small enough that runaway recursion
/// (A→B→A→…) trips quickly.
pub const DEFAULT_MAX_CALL_DEPTH: u32 = 8;

/// Default ceiling on the JSON body we'll accept. MCP requests are
/// small JSON-RPC envelopes; 4 MiB leaves comfortable headroom while
/// still rejecting obvious abuse. Operators can override via
/// [`ServerConfig::with_max_body_bytes`] (and the matching
/// `--max-body-bytes` CLI flag on the binaries).
pub const DEFAULT_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Cap on the bytes we'll read for HTTP headers before declining the
/// request. 64 KiB is generous for legitimate MCP clients; protects
/// against an unbounded slow-loris stream filling memory.
const HEADER_LIMIT_BYTES: usize = 64 * 1024;

/// Maximum time to wait for in-flight connections to finish after
/// the shutdown token is cancelled. The accept loop stops immediately;
/// any task still serving a request after this grace period is
/// aborted by [`JoinSet`]'s drop. Five seconds is enough for the
/// typical MCP request shapes (single-shot JSON or short SSE) without
/// holding a stuck process indefinitely on a wedged client.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Inactivity timeout on each `read` syscall during request header /
/// body parsing. A slow-loris client that trickles one byte per minute
/// would otherwise tie up a connection task (and an accompanying
/// JoinSet slot) indefinitely. 30s is generous for any well-behaved
/// peer; the loop reads in 4 KiB chunks so the cap only fires when a
/// peer genuinely stops sending bytes.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Top-level configuration for the HTTP transport. All security
/// knobs default to "off"; `ServerConfig::default()` reproduces the
/// legacy plain-HTTP, no-auth, no-origin-check behavior with the
/// only opinion baked in being [`DEFAULT_MAX_BODY_BYTES`] as the
/// request body ceiling.
#[derive(Clone)]
pub struct ServerConfig {
    /// Optional TLS acceptor. When set, the listener terminates TLS
    /// in-process so clients reach `https://HOST:PORT`. Built with
    /// [`build_tls_acceptor`] from a PEM cert + key pair.
    pub tls: Option<Arc<TlsAcceptor>>,
    /// Optional bearer-token allowlist. When set, every `POST /`
    /// request must present a token matching **any** entry in the
    /// list (via `Authorization: Bearer <token>` or
    /// `X-API-Key: <token>`); otherwise the server replies
    /// `401 Unauthorized`. Holding multiple values lets operators
    /// rotate keys with overlap: deploy the new token alongside the
    /// old, roll clients, then drop the retired entry.
    pub api_keys: Option<Arc<Vec<String>>>,
    /// Optional `Origin` header allowlist. `None` (the default)
    /// disables Origin checking entirely — fine for stdio-style
    /// clients that don't send Origin. When `Some(set)`, the request's
    /// `Origin` header must either be missing, equal to the literal
    /// string `null`, or appear (case-insensitive) in `set`. Anything
    /// else is rejected with `403`.
    pub allowed_origins: Option<Arc<Vec<String>>>,
    /// Reject requests whose `Mcp-Call-Depth` header is greater than
    /// or equal to this value. `None` disables the check.
    pub max_call_depth: Option<u32>,
    /// Hard cap on the bytes we'll accept in a single request body.
    /// Requests presenting a `Content-Length` greater than this are
    /// rejected with `413 Payload Too Large` **before** any body
    /// bytes are read off the socket, so a malicious client can't
    /// keep us paging through gigabytes of garbage. Defaults to
    /// [`DEFAULT_MAX_BODY_BYTES`].
    pub max_body_bytes: usize,
    /// Optional access log. When set, every served request emits
    /// one JSON object (followed by `\n`) to the configured sink
    /// after the response has been flushed. See
    /// [`crate::access_log`] for the schema and policy.
    pub access_log: Option<Arc<AccessLog>>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tls: None,
            api_keys: None,
            allowed_origins: None,
            max_call_depth: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            access_log: None,
        }
    }
}

impl ServerConfig {
    pub fn with_tls(mut self, acceptor: TlsAcceptor) -> Self {
        self.tls = Some(Arc::new(acceptor));
        self
    }

    /// Convenience for the single-token case. Currently only used in
    /// tests, so suppress the dead-code lint without losing the
    /// helper from the published surface.
    #[allow(dead_code)]
    pub fn with_api_key(self, token: impl Into<String>) -> Self {
        self.with_api_keys(vec![token.into()])
    }

    /// Replace the bearer-token allowlist. Empty input clears the
    /// allowlist, which disables auth.
    pub fn with_api_keys(mut self, tokens: Vec<String>) -> Self {
        let tokens: Vec<String> = tokens
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        self.api_keys = if tokens.is_empty() {
            None
        } else {
            Some(Arc::new(tokens))
        };
        self
    }

    /// Replace the Origin allowlist. Empty input clears the allowlist,
    /// which disables Origin checking. Entries are normalized to
    /// lowercase since RFC 6454 origins are case-insensitive on the
    /// scheme + host components.
    pub fn with_allowed_origins(mut self, origins: Vec<String>) -> Self {
        let origins: Vec<String> = origins
            .into_iter()
            .map(|o| o.trim().to_ascii_lowercase())
            .filter(|o| !o.is_empty())
            .collect();
        self.allowed_origins = if origins.is_empty() {
            None
        } else {
            Some(Arc::new(origins))
        };
        self
    }

    /// Set the maximum permitted `Mcp-Call-Depth` header value.
    /// Requests with a depth `>=` this value are rejected with `403`.
    pub fn with_max_call_depth(mut self, depth: u32) -> Self {
        self.max_call_depth = Some(depth);
        self
    }

    /// Override the request body ceiling. Requests whose
    /// `Content-Length` exceeds `bytes` are answered with
    /// `413 Payload Too Large` and the connection is closed without
    /// reading the body. A value of zero disables the entire POST
    /// surface (every body fails the check), which is occasionally
    /// useful for parking a hostname behind a placeholder.
    pub fn with_max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = bytes;
        self
    }

    /// Attach an access log to this server. Per-request lines are
    /// emitted after the response is flushed; see
    /// [`crate::access_log`] for the JSON schema and secrets policy.
    pub fn with_access_log(mut self, log: Arc<AccessLog>) -> Self {
        self.access_log = Some(log);
        self
    }
}

/// Build a [`TlsAcceptor`] from PEM-encoded cert and key files on
/// disk. Surfaces user-facing context strings so misconfiguration
/// errors point at the offending path. The handshake itself uses
/// the rustls `ring` provider that the rest of the workspace
/// already pulls in.
pub fn build_tls_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor> {
    let (certs, key) = load_pem_cert_and_key(cert_path, key_path)?;
    let config = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building rustls server config")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn load_pem_cert_and_key(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_pem = std::fs::read(cert_path)
        .with_context(|| format!("reading TLS cert at {}", cert_path.display()))?;
    let key_pem = std::fs::read(key_path)
        .with_context(|| format!("reading TLS key at {}", key_path.display()))?;

    let mut cert_reader = std::io::BufReader::new(cert_pem.as_slice());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parsing TLS cert at {}", cert_path.display()))?;
    if certs.is_empty() {
        anyhow::bail!(
            "no certificates found in {} (expected at least one PEM-encoded CERTIFICATE block)",
            cert_path.display()
        );
    }

    let mut key_reader = std::io::BufReader::new(key_pem.as_slice());
    let key = rustls_pemfile::private_key(&mut key_reader)
        .with_context(|| format!("parsing TLS key at {}", key_path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no private key found in {} (expected a PEM-encoded PRIVATE KEY block)",
                key_path.display()
            )
        })?;

    Ok((certs, key))
}

/// Run the Streamable HTTP MCP server until `shutdown` is cancelled
/// (or the listener errors out fatally).
///
/// `addr` is forwarded to [`TcpListener::bind`]; pass `127.0.0.1:0`
/// for an OS-assigned ephemeral port. The bound address (including
/// the resolved port and scheme) is logged to stderr **and** echoed on
/// stdout in a `listening on <scheme>://HOST:PORT` line so test
/// harnesses can scrape the URL reliably.
///
/// The provided `dispatcher` is shared (cloned via `Arc`) across all
/// accepted connections; implementations should be cheap to clone or
/// otherwise share.
///
/// Shutdown semantics: when `shutdown` is cancelled the accept loop
/// stops taking new connections immediately and the function then
/// waits up to [`SHUTDOWN_GRACE`] for in-flight handlers to finish.
/// Any handler still running after the grace period is aborted via
/// [`JoinSet`]'s drop. Tests that don't need cancellation can pass
/// `CancellationToken::new()` (an uncancelled token), which keeps the
/// server running for the lifetime of its task — matching the
/// pre-existing "run forever" behavior.
pub async fn run<D>(
    addr: &str,
    config: ServerConfig,
    dispatcher: D,
    shutdown: CancellationToken,
) -> Result<()>
where
    D: McpDispatcher,
{
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding zunel-mcp-self HTTP listener at {addr}"))?;
    let bound = listener.local_addr().context("reading bound address")?;
    // Loud warning when an operator binds to a non-loopback address
    // without any authentication configured. The default config
    // (api_keys = None, allowed_origins = None) was safe on
    // `127.0.0.1:0`; on `0.0.0.0:N` it would publish an unauthenticated
    // MCP transport on the network. We don't refuse — there are legit
    // single-host setups where the cap is paranoid — but make it
    // operator-visible at WARN.
    if !bound.ip().is_loopback() && config.api_keys.is_none() {
        tracing::warn!(
            bind = %bound,
            "zunel-mcp-self bound to a non-loopback address with no api_keys configured; \
             anyone reachable on this network can speak MCP unauthenticated. Pass \
             --api-key <token> (or ZUNEL_MCP_SELF_API_KEY) to require a bearer."
        );
    }
    let scheme = if config.tls.is_some() {
        "https"
    } else {
        "http"
    };
    let banner = format!("zunel-mcp-self listening on {scheme}://{bound}");
    tracing::info!("{banner}");
    // Keep the stdout println: test harnesses spawning the binary
    // scrape this line for the bound address before probing the port.
    // The tracing::info! above carries the same content for production
    // log consumers.
    println!("{banner}");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    let session_id = Arc::new(generate_session_id());
    let config = Arc::new(config);
    let dispatcher: Arc<dyn McpDispatcher> = Arc::new(dispatcher);
    let mut tasks: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            // Service the cancel branch first so a flood of incoming
            // connections can't starve a pending shutdown.
            biased;
            _ = shutdown.cancelled() => {
                tracing::info!("zunel-mcp-self received shutdown; closing listener");
                break;
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(err) => {
                        tracing::warn!(error = %err, "zunel-mcp-self accept error");
                        continue;
                    }
                };
                let session_id = session_id.clone();
                let config = config.clone();
                let dispatcher = dispatcher.clone();
                tasks.spawn(async move {
                    if let Err(err) = serve_one(stream, peer, session_id, config, dispatcher).await {
                        tracing::warn!(error = format!("{err:#}"), "zunel-mcp-self http connection error");
                    }
                });
            }
        }
    }

    drop(listener);
    drain_tasks(&mut tasks).await;
    Ok(())
}

/// Wait up to [`SHUTDOWN_GRACE`] for every spawned connection task to
/// finish. Tasks still running past the deadline are aborted when
/// `tasks` is dropped at the end of [`run`]. Splitting this into its
/// own helper keeps [`run`]'s control flow obvious and makes the
/// drain testable in isolation.
async fn drain_tasks(tasks: &mut JoinSet<()>) {
    if tasks.is_empty() {
        return;
    }
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    while !tasks.is_empty() {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            tracing::warn!(
                grace_s = SHUTDOWN_GRACE.as_secs(),
                still_running = tasks.len(),
                "zunel-mcp-self shutdown grace elapsed with tasks still running; aborting"
            );
            tasks.abort_all();
            // Drain the abort-induced JoinError(s) so the JoinSet's
            // drop doesn't have to.
            while tasks.join_next().await.is_some() {}
            return;
        }
        let remaining = deadline - now;
        match tokio::time::timeout(remaining, tasks.join_next()).await {
            Ok(Some(_)) => {}
            // join_next on an empty set returns None; our `while`
            // guard catches that on the next iteration.
            Ok(None) => return,
            Err(_) => {
                // Top-of-loop deadline check will handle the abort
                // path on the next iteration.
            }
        }
    }
}

/// Block until the process receives a graceful-shutdown signal
/// (SIGINT/Ctrl-C on every platform; SIGTERM on Unix). Designed to be
/// the body of a shutdown-watcher task that flips a [`CancellationToken`]
/// for [`run`] to observe.
///
/// On Unix we listen for both signals so the binary cooperates with
/// whatever supervisor (`systemd`, `launchd`, `docker stop`) is
/// driving it. Falls back to ctrl-c only when SIGTERM registration
/// fails for any reason (e.g. running unprivileged inside a sandbox
/// that blocks `signal()`).
pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
                return;
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "zunel-mcp-self: SIGTERM handler registration failed; falling back to SIGINT only"
                );
            }
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Serve one accepted TCP stream. When TLS is configured we wrap
/// the stream in `tokio_rustls::server::TlsStream` and reuse the
/// generic [`handle_connection`] body; otherwise we drive the bytes
/// directly. Centralising the branch here keeps the handler oblivious
/// to which transport accepted the request.
async fn serve_one(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    session_id: Arc<String>,
    config: Arc<ServerConfig>,
    dispatcher: Arc<dyn McpDispatcher>,
) -> Result<()> {
    if let Some(acceptor) = config.tls.as_ref() {
        let tls = match acceptor.accept(stream).await {
            Ok(stream) => stream,
            Err(err) => {
                // Browsers/probes often abort during the handshake (cert
                // warning, ALPN mismatch, …). Log and move on rather
                // than killing the listener.
                tracing::warn!(error = %err, "zunel-mcp-self TLS handshake failed");
                return Ok(());
            }
        };
        handle_connection(tls, peer, session_id, config, dispatcher).await
    } else {
        handle_connection(stream, peer, session_id, config, dispatcher).await
    }
}

/// A reasonably-unique opaque token to issue as `Mcp-Session-Id`. We
/// don't require globally-unique identifiers; the MCP spec just wants
/// per-session stability, and the same token for the lifetime of this
/// process is sufficient for a stateless self-inspection server.
fn generate_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{SERVER_NAME}-{pid:x}-{nanos:x}-{count:x}")
}

async fn handle_connection<S>(
    mut stream: S,
    peer: SocketAddr,
    session_id: Arc<String>,
    config: Arc<ServerConfig>,
    dispatcher: Arc<dyn McpDispatcher>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Allocate a log context up front so every exit path either
    // emits an entry or — in the case of a peer that closed the
    // socket without sending bytes — explicitly skips emission.
    // We keep the context as `Option` so callees can `.take()` it
    // and finalize at the moment of writing the response.
    let mut log_ctx = config
        .access_log
        .as_ref()
        .map(|_| AccessLogContext::new(peer.to_string()));

    let request = match read_request(&mut stream, config.max_body_bytes).await? {
        RequestRead::Ready(req) => req,
        RequestRead::Closed => {
            // Idle peer probe: don't emit a log line. Logging
            // browser-style "TCP test then RST" handshakes would
            // dominate the file without telling the operator
            // anything actionable.
            return Ok(());
        }
        RequestRead::BodyTooLarge {
            content_length,
            limit,
        } => {
            let body =
                format!("Content-Length {content_length} exceeds server cap of {limit} bytes");
            let result = write_status(&mut stream, 413, "Payload Too Large", body.as_bytes()).await;
            if let Some(mut ctx) = log_ctx.take() {
                ctx.status = 413;
                emit_log(&config, ctx).await;
            }
            return result;
        }
    };

    if let Some(ctx) = log_ctx.as_mut() {
        ctx.depth = request.call_depth;
    }

    let result = match request.method.as_str() {
        "POST" => {
            handle_post(
                &mut stream,
                &request,
                &session_id,
                &config,
                &dispatcher,
                log_ctx.as_mut(),
            )
            .await
        }
        "GET" => {
            if let Some(ctx) = log_ctx.as_mut() {
                ctx.method = Some("GET".to_string());
                ctx.status = 405;
            }
            write_status(
                &mut stream,
                405,
                "Method Not Allowed",
                b"server-initiated SSE not supported",
            )
            .await
        }
        "DELETE" => {
            // Stateless server: nothing to tear down. Return 200 to
            // satisfy clients that always issue a teardown.
            if let Some(ctx) = log_ctx.as_mut() {
                ctx.method = Some("DELETE".to_string());
                ctx.status = 200;
            }
            write_status(&mut stream, 200, "OK", b"").await
        }
        "OPTIONS" => {
            if let Some(ctx) = log_ctx.as_mut() {
                ctx.method = Some("OPTIONS".to_string());
                ctx.status = 204;
            }
            write_options(&mut stream).await
        }
        _ => {
            if let Some(ctx) = log_ctx.as_mut() {
                ctx.method = Some(request.method.clone());
                ctx.status = 405;
            }
            write_status(&mut stream, 405, "Method Not Allowed", b"").await
        }
    };

    if let Some(ctx) = log_ctx {
        emit_log(&config, ctx).await;
    }

    result
}

/// Emit one access-log line if the server has a sink configured.
/// Centralizing this keeps the call sites symmetric and avoids
/// double-emitting when an exit path forgets to `.take()` the
/// context.
async fn emit_log(config: &ServerConfig, ctx: AccessLogContext) {
    if let Some(log) = config.access_log.as_ref() {
        let entry = ctx.finish();
        log.emit(&entry).await;
    }
}

/// Top-level POST handler. Validates Origin / call-depth / auth
/// (in that order so an out-of-policy request never reveals whether
/// authentication would have succeeded), parses the JSON body,
/// dispatches each JSON-RPC message via the supplied
/// [`McpDispatcher`], then writes either `application/json` (single
/// result) or `text/event-stream` (one SSE message event) depending
/// on the client's `Accept` header.
async fn handle_post<S>(
    stream: &mut S,
    request: &HttpRequest,
    session_id: &str,
    config: &ServerConfig,
    dispatcher: &Arc<dyn McpDispatcher>,
    log_ctx: Option<&mut AccessLogContext>,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    // Helper closure usable on every exit branch to stamp the
    // outgoing status onto the log context. Marking it `&mut` lets
    // each branch reach into the option without taking ownership;
    // the caller in `handle_connection` finalizes the emit.
    fn stamp(log_ctx: &mut Option<&mut AccessLogContext>, status: u16) {
        if let Some(ctx) = log_ctx.as_mut() {
            ctx.status = status;
        }
    }

    let mut log_ctx = log_ctx;
    if let Some(ctx) = log_ctx.as_mut() {
        ctx.method = Some("POST".to_string());
    }

    if let Some(allowed) = config.allowed_origins.as_deref() {
        if !origin_allowed(request.origin.as_deref(), allowed) {
            stamp(&mut log_ctx, 403);
            return write_status(stream, 403, "Forbidden", b"origin not allowed").await;
        }
    }
    if let Some(limit) = config.max_call_depth {
        if let Some(depth) = request.call_depth {
            if depth >= limit {
                stamp(&mut log_ctx, 403);
                let body = format!(
                    "Mcp-Call-Depth {depth} exceeds limit {limit}; refusing to recurse further"
                );
                return write_status(stream, 403, "Forbidden", body.as_bytes()).await;
            }
        }
    }
    // Bearer-token fingerprint is computed once and shared across the
    // access log (where it identifies the matched key without leaking
    // the secret) and the DispatchMeta (where Mode 2's `helper_ask`
    // uses it to namespace per-caller sessions). Loopback-no-auth
    // deployments skip this branch entirely and leave both surfaces
    // populated with `None`, which is the right thing — there's no
    // identity to attribute the call to.
    let mut caller_fingerprint: Option<String> = None;
    if let Some(allowlist) = config.api_keys.as_deref() {
        match matched_token(request, allowlist) {
            Some(token) => {
                let fingerprint = token_fingerprint(token);
                if let Some(ctx) = log_ctx.as_mut() {
                    ctx.key = Some(fingerprint.clone());
                }
                caller_fingerprint = Some(fingerprint);
            }
            None => {
                stamp(&mut log_ctx, 401);
                return write_unauthorized(stream).await;
            }
        }
    }
    if request.body.is_empty() {
        stamp(&mut log_ctx, 400);
        return write_status(stream, 400, "Bad Request", b"empty body").await;
    }
    let parsed: Value = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(err) => {
            stamp(&mut log_ctx, 400);
            let msg = format!("invalid JSON: {err}");
            return write_status(stream, 400, "Bad Request", msg.as_bytes()).await;
        }
    };

    // Now that we have a parsed JSON-RPC envelope (or batch), let the
    // log context overwrite the bare `POST` method tag with the
    // actual JSON-RPC method (or `*batch`), plus the rpc_id and
    // tool name when applicable.
    if let Some(ctx) = log_ctx.as_mut() {
        ctx.record_rpc(&parsed);
    }

    let meta = DispatchMeta {
        call_depth: request.call_depth,
        caller_fingerprint: caller_fingerprint.clone(),
        rpc_id: parsed.get("id").and_then(zunel_tools::RpcId::from_json),
    };

    // Slice 2 streaming gate: when the caller passed
    // `params._meta.progressToken` AND accepts `text/event-stream`, we
    // fan out via the streaming dispatcher path so the tool can emit
    // mid-call `notifications/progress` events. Batches are excluded
    // — there's no clean way to interleave per-item progress on a
    // single SSE response, and the spec doesn't define batched
    // streaming.
    let streaming_request = !parsed.is_array()
        && accepts_event_stream(&request.accept)
        && parsed
            .get("params")
            .and_then(|p| p.get("_meta"))
            .and_then(|m| m.get("progressToken"))
            .is_some();

    if streaming_request {
        stamp(&mut log_ctx, 200);
        // Streaming path runs the dispatcher and the SSE writer
        // side-by-side: the writer drains a progress channel until
        // the dispatch future resolves, then emits the final
        // response as the last event and closes.
        return write_streaming_response(stream, session_id, dispatcher.clone(), parsed, meta)
            .await;
    }

    let responses = match &parsed {
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                // Each batch item carries its own JSON-RPC id; build a
                // per-item DispatchMeta so a single batched
                // `notifications/cancelled` can target the right call.
                let item_meta = DispatchMeta {
                    call_depth: request.call_depth,
                    caller_fingerprint: caller_fingerprint.clone(),
                    rpc_id: item.get("id").and_then(zunel_tools::RpcId::from_json),
                };
                if let Some(resp) = dispatcher.dispatch(item, &item_meta).await {
                    out.push(resp);
                }
            }
            out
        }
        _ => dispatcher
            .dispatch(&parsed, &meta)
            .await
            .into_iter()
            .collect(),
    };

    if responses.is_empty() {
        stamp(&mut log_ctx, 202);
        return write_status(stream, 202, "Accepted", b"").await;
    }

    let payload: Value = if responses.len() == 1 && !parsed.is_array() {
        responses.into_iter().next().unwrap()
    } else {
        Value::Array(responses)
    };

    stamp(&mut log_ctx, 200);
    if accepts_event_stream(&request.accept) {
        write_sse_response(stream, &payload, session_id).await
    } else {
        write_json_response(stream, &payload, session_id).await
    }
}

/// RFC 6454 §7.3 "null" Origin and missing Origin are both treated as
/// "not a browser making a CORS request", so they bypass the allowlist.
/// Any literal Origin must appear (case-insensitively) in `allowed`.
fn origin_allowed(origin: Option<&str>, allowed: &[String]) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let trimmed = origin.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        return true;
    }
    let needle = trimmed.to_ascii_lowercase();
    allowed.iter().any(|entry| entry == &needle)
}

/// Constant-time bearer-token check kept for unit tests. Production
/// code uses [`matched_token`] directly so the access logger can
/// record a stable fingerprint of the matched entry; the bool form
/// is a sliver thinner and exercises the same code path.
#[cfg(test)]
fn is_authorized(request: &HttpRequest, allowlist: &[String]) -> bool {
    matched_token(request, allowlist).is_some()
}

/// Like [`is_authorized`] but returns a reference to the allowlist
/// entry that matched, so the access logger can record a stable
/// per-key fingerprint (see [`crate::access_log::token_fingerprint`]).
/// Walks the entire allowlist with constant-time comparison even
/// after a hit so the runtime stays independent of which entry
/// matched — same side-channel argument as [`is_authorized`].
fn matched_token<'a>(request: &HttpRequest, allowlist: &'a [String]) -> Option<&'a str> {
    let presented = if let Some(value) = request.authorization.as_deref() {
        let trimmed = value.trim();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        match (parts.next(), parts.next()) {
            (Some(scheme), Some(token)) if scheme.eq_ignore_ascii_case("Bearer") => token.trim(),
            _ => return None,
        }
    } else if let Some(value) = request.api_key_header.as_deref() {
        value.trim()
    } else {
        return None;
    };
    let presented = presented.as_bytes();
    let mut hit: Option<&str> = None;
    for entry in allowlist {
        let matched = constant_time_eq(presented, entry.as_bytes());
        if matched && hit.is_none() {
            hit = Some(entry.as_str());
        }
    }
    hit
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn write_unauthorized<S>(stream: &mut S) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let body = b"unauthorized";
    let header = format!(
        "HTTP/1.1 401 Unauthorized\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         WWW-Authenticate: Bearer realm=\"zunel-mcp-self\"\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn accepts_event_stream(accept: &str) -> bool {
    accept.split(',').any(|part| {
        part.trim()
            .to_ascii_lowercase()
            .starts_with("text/event-stream")
    })
}

async fn write_json_response<S>(stream: &mut S, payload: &Value, session_id: &str) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(payload)?;
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Mcp-Session-Id: {}\r\n\
         Connection: close\r\n\r\n",
        body.len(),
        session_id
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Streamable HTTP server-sent-events response. We chunk the body so
/// the connection stays open for the duration of the dispatch (in
/// practice instant for the self tools, but the framing is real and
/// matches what longer-running MCP servers will do).
async fn write_sse_response<S>(stream: &mut S, payload: &Value, session_id: &str) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache, no-transform\r\n\
         Mcp-Session-Id: {}\r\n\
         Transfer-Encoding: chunked\r\n\
         Connection: close\r\n\r\n",
        session_id
    );
    stream.write_all(header.as_bytes()).await?;
    stream.flush().await?;

    let event_body = format!(
        "event: message\ndata: {}\n\n",
        serde_json::to_string(payload)?
    );
    write_chunk(stream, event_body.as_bytes()).await?;
    // Final zero-length chunk closes the chunked stream.
    write_chunk(stream, b"").await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn write_chunk<S>(stream: &mut S, bytes: &[u8]) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let header = format!("{:x}\r\n", bytes.len());
    stream.write_all(header.as_bytes()).await?;
    if !bytes.is_empty() {
        stream.write_all(bytes).await?;
    }
    stream.write_all(b"\r\n").await?;
    Ok(())
}

/// Slice 2 streaming response. Drives the dispatcher's
/// [`McpDispatcher::dispatch_streaming`] path, fans out each progress
/// notification through the open SSE connection, then writes the final
/// JSON-RPC response as the last `data:` event before closing the
/// chunked stream.
///
/// We hold off opening the SSE response headers until the first event
/// is ready to write — that way the chunked envelope only opens once
/// we know we'll send something. (The dispatcher could in principle
/// resolve immediately if it has nothing streaming-worthy to say, in
/// which case the client just sees a single SSE event with the final
/// response.)
async fn write_streaming_response<S>(
    stream: &mut S,
    session_id: &str,
    dispatcher: Arc<dyn McpDispatcher>,
    parsed: Value,
    meta: DispatchMeta,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache, no-transform\r\n\
         Mcp-Session-Id: {}\r\n\
         Transfer-Encoding: chunked\r\n\
         Connection: close\r\n\r\n",
        session_id
    );
    stream.write_all(header.as_bytes()).await?;
    stream.flush().await?;

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<Value>(64);
    // Spawn the dispatcher in its own task so we can interleave
    // progress events and the eventual final response on the SSE
    // wire. The task takes its own owned dispatcher Arc + parsed
    // message so the parent future can keep `&mut stream` for
    // writing.
    let dispatcher_task = tokio::spawn({
        let dispatcher = Arc::clone(&dispatcher);
        async move {
            dispatcher
                .dispatch_streaming(&parsed, &meta, progress_tx)
                .await
        }
    });

    // Drain progress events as they arrive. The channel closes when
    // the dispatcher task drops its sink (either intentionally on
    // completion, or because the task panicked / was aborted).
    while let Some(event) = progress_rx.recv().await {
        let event_body = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&event)?
        );
        write_chunk(stream, event_body.as_bytes()).await?;
        stream.flush().await?;
    }

    // Wait for the final response from the dispatcher.
    let final_response = match dispatcher_task.await {
        Ok(resp) => resp,
        Err(join_err) => {
            // Tokio JoinError: the dispatcher task panicked. Surface
            // a generic JSON-RPC error so the client doesn't hang
            // waiting for a final event.
            tracing::error!(error = ?join_err, "zunel-mcp-self: streaming dispatcher panicked");
            Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {
                    "code": -32603,
                    "message": "internal error: dispatcher panicked"
                }
            }))
        }
    };

    if let Some(payload) = final_response {
        let event_body = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&payload)?
        );
        write_chunk(stream, event_body.as_bytes()).await?;
    }
    write_chunk(stream, b"").await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn write_status<S>(stream: &mut S, code: u16, reason: &str, body: &[u8]) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let header = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn write_options<S>(stream: &mut S) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let header = "HTTP/1.1 204 No Content\r\n\
         Allow: POST, OPTIONS\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: content-type, accept, authorization, mcp-session-id, mcp-protocol-version, mcp-call-depth, x-api-key\r\n\
         Connection: close\r\n\r\n";
    stream.write_all(header.as_bytes()).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    accept: String,
    body: Vec<u8>,
    authorization: Option<String>,
    api_key_header: Option<String>,
    /// Raw `Origin` header value if present. Empty string vs missing
    /// is preserved so [`origin_allowed`] can apply RFC-6454-style
    /// "null/empty bypasses CORS allowlist" semantics.
    origin: Option<String>,
    /// Parsed `Mcp-Call-Depth` header. Treated as `None` (i.e. depth 0)
    /// when missing or unparseable so that legacy clients aren't
    /// rejected merely for omitting the header.
    call_depth: Option<u32>,
}

/// Outcome of [`read_request`]. We distinguish "peer closed the
/// connection without sending anything" (a common browser probe
/// pattern, not worth logging) from "headers parsed but the
/// announced body would exceed the server's cap", because the
/// caller wants to translate the latter into a `413` HTTP response
/// rather than dropping the connection mid-handshake.
enum RequestRead {
    /// Idle peer closed before bytes were observed. The handler
    /// should silently return without touching the socket.
    Closed,
    /// A complete request (headers + body) was read successfully.
    Ready(HttpRequest),
    /// The request announced a `Content-Length` larger than
    /// `limit`. The body has **not** been read off the wire; the
    /// caller should respond with `413 Payload Too Large` and close
    /// the connection.
    BodyTooLarge { content_length: usize, limit: usize },
}

/// Read one HTTP/1.1 request from `stream`. The `max_body_bytes`
/// argument is the per-request body ceiling pulled from
/// [`ServerConfig::max_body_bytes`]; oversized bodies short-circuit
/// the read so callers can answer `413` before any body bytes are
/// ever drained off the socket.
async fn read_request<S>(stream: &mut S, max_body_bytes: usize) -> Result<RequestRead>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0_u8; 4096];

    let header_end = loop {
        let n = tokio::time::timeout(REQUEST_READ_TIMEOUT, stream.read(&mut tmp))
            .await
            .map_err(|_| anyhow::anyhow!("client idle for too long during header read"))??;
        if n == 0 {
            if buf.is_empty() {
                return Ok(RequestRead::Closed);
            }
            anyhow::bail!("peer closed before headers terminated");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
        if buf.len() > HEADER_LIMIT_BYTES {
            anyhow::bail!("HTTP headers exceeded {HEADER_LIMIT_BYTES} bytes");
        }
    };

    let header_text =
        std::str::from_utf8(&buf[..header_end]).context("HTTP headers were not valid UTF-8")?;
    let (request_line, header_block) = header_text.split_once("\r\n").unwrap_or((header_text, ""));
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP method"))?
        .to_ascii_uppercase();
    let _path = request_parts.next().unwrap_or("/");

    let mut content_length: usize = 0;
    let mut accept = String::new();
    let mut authorization: Option<String> = None;
    let mut api_key_header: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut call_depth: Option<u32> = None;
    for line in header_block.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                content_length = value
                    .parse()
                    .context("content-length header was not a positive integer")?;
            }
            "accept" => accept = value.to_string(),
            "authorization" => authorization = Some(value.to_string()),
            "x-api-key" => api_key_header = Some(value.to_string()),
            "origin" => origin = Some(value.to_string()),
            "mcp-call-depth" => call_depth = value.parse().ok(),
            _ => {}
        }
    }
    if content_length > max_body_bytes {
        // Don't drain the body — that's the whole point of the cap.
        // Return a structured outcome and let `handle_connection`
        // emit a single `413` response.
        return Ok(RequestRead::BodyTooLarge {
            content_length,
            limit: max_body_bytes,
        });
    }

    let body_start = header_end + 4;
    let mut body = if body_start <= buf.len() {
        buf[body_start..].to_vec()
    } else {
        Vec::new()
    };
    while body.len() < content_length {
        let n = tokio::time::timeout(REQUEST_READ_TIMEOUT, stream.read(&mut tmp))
            .await
            .map_err(|_| anyhow::anyhow!("client idle for too long during body read"))??;
        if n == 0 {
            anyhow::bail!(
                "peer closed before body completed ({}/{} bytes)",
                body.len(),
                content_length
            );
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Ok(RequestRead::Ready(HttpRequest {
        method,
        accept,
        body,
        authorization,
        api_key_header,
        origin,
        call_depth,
    }))
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(authorization: Option<&str>, api_key: Option<&str>) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            accept: "application/json".into(),
            body: Vec::new(),
            authorization: authorization.map(ToOwned::to_owned),
            api_key_header: api_key.map(ToOwned::to_owned),
            origin: None,
            call_depth: None,
        }
    }

    #[test]
    fn accepts_event_stream_handles_compound_accept_header() {
        assert!(accepts_event_stream("application/json, text/event-stream"));
        assert!(accepts_event_stream("text/event-stream"));
        assert!(accepts_event_stream(
            "text/event-stream;q=1, application/json;q=0.9"
        ));
        assert!(!accepts_event_stream("application/json"));
        assert!(!accepts_event_stream(""));
    }

    #[test]
    fn find_double_crlf_locates_header_terminator() {
        let buf = b"POST / HTTP/1.1\r\nA: b\r\n\r\nbody";
        let pos = find_double_crlf(buf).expect("terminator present");
        assert_eq!(&buf[pos..pos + 4], b"\r\n\r\n");
        assert_eq!(&buf[pos + 4..], b"body");
    }

    #[test]
    fn find_double_crlf_returns_none_when_absent() {
        assert!(find_double_crlf(b"POST / HTTP/1.1\r\nA: b\r\n").is_none());
    }

    fn allow(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn is_authorized_accepts_bearer_header() {
        let req = make_request(Some("Bearer secret"), None);
        assert!(is_authorized(&req, &allow(&["secret"])));
    }

    #[test]
    fn is_authorized_accepts_lowercase_scheme() {
        let req = make_request(Some("bearer secret"), None);
        assert!(is_authorized(&req, &allow(&["secret"])));
    }

    #[test]
    fn is_authorized_accepts_x_api_key_header() {
        let req = make_request(None, Some("secret"));
        assert!(is_authorized(&req, &allow(&["secret"])));
    }

    #[test]
    fn is_authorized_rejects_missing_credentials() {
        let req = make_request(None, None);
        assert!(!is_authorized(&req, &allow(&["secret"])));
    }

    #[test]
    fn is_authorized_rejects_wrong_token() {
        let req = make_request(Some("Bearer not-it"), None);
        assert!(!is_authorized(&req, &allow(&["secret"])));
    }

    #[test]
    fn is_authorized_rejects_non_bearer_scheme() {
        let req = make_request(Some("Basic secret"), None);
        assert!(!is_authorized(&req, &allow(&["secret"])));
    }

    #[test]
    fn is_authorized_accepts_either_key_during_rotation_overlap() {
        let allowlist = allow(&["old-key", "new-key"]);
        assert!(is_authorized(
            &make_request(Some("Bearer old-key"), None),
            &allowlist
        ));
        assert!(is_authorized(
            &make_request(Some("Bearer new-key"), None),
            &allowlist
        ));
        assert!(!is_authorized(
            &make_request(Some("Bearer revoked"), None),
            &allowlist
        ));
    }

    #[test]
    fn with_api_keys_filters_blank_lines_and_clears_when_empty() {
        let cfg = ServerConfig::default().with_api_keys(vec![
            "  ".into(),
            "real".into(),
            "".into(),
            " other ".into(),
        ]);
        let stored: Vec<String> = cfg.api_keys.as_deref().cloned().unwrap_or_default();
        assert_eq!(stored, vec!["real".to_string(), "other".to_string()]);

        let empty = ServerConfig::default().with_api_keys(vec!["  ".into(), "".into()]);
        assert!(empty.api_keys.is_none());
    }

    #[test]
    fn constant_time_eq_distinguishes_lengths() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }

    #[test]
    fn origin_allowed_passes_when_no_origin_header() {
        assert!(origin_allowed(None, &["https://example.com".to_string()]));
    }

    #[test]
    fn origin_allowed_passes_for_literal_null() {
        assert!(origin_allowed(
            Some("null"),
            &["https://example.com".to_string()]
        ));
        assert!(origin_allowed(
            Some("NULL"),
            &["https://example.com".to_string()]
        ));
    }

    #[test]
    fn origin_allowed_passes_for_listed_entry_case_insensitive() {
        assert!(origin_allowed(
            Some("HTTPS://EXAMPLE.COM"),
            &["https://example.com".to_string()]
        ));
    }

    #[test]
    fn origin_allowed_rejects_unknown_origin() {
        assert!(!origin_allowed(
            Some("https://attacker.example"),
            &["https://example.com".to_string()]
        ));
    }

    #[test]
    fn with_allowed_origins_normalizes_and_clears_when_empty() {
        let cfg =
            ServerConfig::default().with_allowed_origins(vec!["  HTTPS://Foo  ".into(), "".into()]);
        let stored: Vec<String> = cfg.allowed_origins.as_deref().cloned().unwrap_or_default();
        assert_eq!(stored, vec!["https://foo".to_string()]);

        let cleared = ServerConfig::default().with_allowed_origins(vec![" ".into()]);
        assert!(cleared.allowed_origins.is_none());
    }

    #[test]
    fn server_config_max_call_depth_setter() {
        let cfg = ServerConfig::default().with_max_call_depth(4);
        assert_eq!(cfg.max_call_depth, Some(4));
    }
}
