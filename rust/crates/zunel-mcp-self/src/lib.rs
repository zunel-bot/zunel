//! Library surface for the `zunel-mcp-self` crate.
//!
//! The dispatcher is expressed behind the [`McpDispatcher`] trait and the
//! transport layer in [`http`] runs against any implementation of that
//! trait, so other commands (notably `zunel mcp agent --instance NAME`)
//! can reuse the same Streamable-HTTP/HTTPS transport with a different
//! tool set.
//!
//! Two dispatchers ship in this crate:
//!
//! - [`SelfDispatcher`] — the zunel-self tools (sessions, cron,
//!   channels, token usage, …). Backs both the stdio binary entry point
//!   and `zunel mcp serve`.
//! - [`MapDispatcher`] — a thin construct used in tests to assert
//!   transport behavior without standing up a real registry.
//!
//! Downstream callers (e.g. `zunel-cli`) implement their own
//! `McpDispatcher` and pass it to [`http::run`].

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use zunel_tools::RpcId;

pub mod access_log;
pub mod handlers;
pub mod http;

pub use access_log::{
    open_from_cli as open_access_log, AccessLog, AccessLogContext, AccessLogEntry,
};
pub use handlers::SelfDispatcher;

/// Per-request metadata threaded from the transport layer into the
/// dispatcher. The HTTP transport populates [`DispatchMeta::call_depth`]
/// from the parsed `Mcp-Call-Depth` header so dispatchers (notably
/// `RegistryDispatcher`) can stamp it onto the per-call context and
/// have outbound MCP fan-outs forward `incoming + 1` on their own
/// requests. Stdio and other in-process transports use
/// [`DispatchMeta::default`], which leaves `call_depth` as `None`.
#[derive(Debug, Clone, Default)]
pub struct DispatchMeta {
    /// Depth of the inbound request as parsed from `Mcp-Call-Depth`.
    /// `None` means "no header present" — typically a top-level client
    /// like a CLI or a stdio host.
    pub call_depth: Option<u32>,
    /// Stable fingerprint of the bearer token that authenticated this
    /// request, when one was matched. Computed via
    /// [`access_log::token_fingerprint`] (first 8 hex chars of
    /// SHA-256) so it's safe to log, namespace by, or correlate
    /// without leaking the raw secret. `None` for unauthenticated
    /// transports (loopback-no-auth runs, stdio, etc.).
    ///
    /// Mode 2's `helper_ask` reads this to namespace per-caller
    /// session ids; other dispatchers ignore it.
    pub caller_fingerprint: Option<String>,
    /// Parsed JSON-RPC `id` from the inbound request. `None` for
    /// notifications (no id) or when the transport hasn't been told
    /// about the wire-level id (stdio dispatch, in-process tests).
    /// Slice 2's `helper_ask` reads this to register a cancel token
    /// in the dispatcher's `CancelRegistry`.
    pub rpc_id: Option<RpcId>,
}

/// JSON-RPC message dispatcher. Receives the parsed request envelope
/// plus per-request [`DispatchMeta`] and returns the response envelope,
/// or `None` to indicate "no response should be written" (used for
/// `notifications/*` and other fire-and-forget messages, per the MCP
/// spec).
///
/// Implementations should be cheap to clone or otherwise share —
/// transports may dispatch concurrent requests against the same
/// dispatcher instance from multiple Tokio tasks.
#[async_trait]
pub trait McpDispatcher: Send + Sync + 'static {
    async fn dispatch(&self, message: &Value, meta: &DispatchMeta) -> Option<Value>;

    /// Streaming-aware dispatch. When the transport opens an SSE
    /// channel because `params._meta.progressToken` was present, it
    /// calls this method instead of [`dispatch`]; mid-call progress
    /// notifications go to `progress_sink` and the eventual final
    /// response is the return value (the transport writes both as
    /// SSE `data:` events).
    ///
    /// The default implementation just calls [`dispatch`] and drops
    /// the sink, which is the right behaviour for any dispatcher
    /// that doesn't have anything streaming-worthy to emit.
    async fn dispatch_streaming(
        &self,
        message: &Value,
        meta: &DispatchMeta,
        _progress_sink: mpsc::Sender<Value>,
    ) -> Option<Value> {
        self.dispatch(message, meta).await
    }
}

/// Function pointer type for the test-only [`MapDispatcher`]. Lifted
/// into its own alias so clippy's `type_complexity` lint doesn't fire
/// on every `cfg(test)` use site.
#[cfg(any(test, feature = "test-support"))]
pub type MapResponder =
    std::sync::Arc<dyn Fn(&Value, &DispatchMeta) -> Option<Value> + Send + Sync + 'static>;

/// Test-only dispatcher whose behavior is fully data-driven. The HTTP
/// integration tests use this to verify transport semantics (auth,
/// origin, depth, content negotiation) without depending on any real
/// tool implementation. The responder is given the parsed
/// [`DispatchMeta`] so depth-forwarding tests can assert what the
/// transport handed in.
#[cfg(any(test, feature = "test-support"))]
pub struct MapDispatcher {
    pub responder: MapResponder,
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl McpDispatcher for MapDispatcher {
    async fn dispatch(&self, message: &Value, meta: &DispatchMeta) -> Option<Value> {
        (self.responder)(message, meta)
    }
}
