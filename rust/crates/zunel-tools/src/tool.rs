use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::file_state::FileStateTracker;

/// JSON-RPC request id as observed on the wire. Slice 2 uses this to
/// route `notifications/cancelled` back to the in-flight `helper_ask`
/// call that originally claimed the id.
///
/// The MCP / JSON-RPC 2.0 spec permits Number, String, or null ids;
/// `null` is reserved for notifications and is not representable
/// here — `RpcId::from_json(&Value::Null)` returns `None`, which is
/// the correct behaviour for routing (notifications never need to be
/// cancelled).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RpcId {
    Number(i64),
    String(String),
}

impl RpcId {
    /// Parse a JSON-RPC id from its on-wire form. Returns `None` for
    /// `null` or non-string-non-number ids; those are treated as
    /// "no routable id" by callers.
    pub fn from_json(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_i64().map(Self::Number),
            Value::String(s) => Some(Self::String(s.clone())),
            _ => None,
        }
    }

    /// Render back to a JSON value so a dispatcher can echo it onto an
    /// outbound response.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Number(n) => Value::Number((*n).into()),
            Self::String(s) => Value::String(s.clone()),
        }
    }
}

/// Per-call context a tool can read. Exposes the workspace, the
/// session key, a cancellation token so long-running tools (`exec`,
/// `web_fetch`) can be aborted when the parent agent loop is
/// cancelled, and a `FileStateTracker` shared between read/write/edit
/// tools.
#[derive(Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub session_key: String,
    pub cancel: tokio_util::sync::CancellationToken,
    pub file_state: FileStateTracker,
    /// Depth of the inbound MCP request that triggered this tool call,
    /// if the caller is itself an MCP server. `None` means "this tool
    /// call originated locally" (e.g. from the CLI agent loop or a
    /// direct in-process invocation). When a tool then fans out to
    /// another MCP server, the wrapper increments this value and emits
    /// `Mcp-Call-Depth: incoming + 1` on the outbound HTTP request,
    /// which lets the receiving server enforce its own depth cap and
    /// short-circuit cycles like A→B→A→…
    pub incoming_call_depth: Option<u32>,
    /// Stable fingerprint of the bearer token that authenticated the
    /// inbound MCP request, when one was matched. The transport
    /// computes this once via SHA-256-first-8-hex of the matched token
    /// and threads it through `DispatchMeta` so logging *and*
    /// per-tool routing can use the same opaque identifier.
    ///
    /// `None` means either the call originated locally (CLI agent
    /// loop, in-process tests) or the transport runs without an API
    /// key allowlist (loopback-no-auth, stdio). Tools that key
    /// per-caller state (notably Mode 2's `helper_ask`, which
    /// namespaces session ids by fingerprint) MUST tolerate this:
    /// fall back to a deterministic "anonymous" bucket so the
    /// loopback-dev story still works.
    pub caller_fingerprint: Option<String>,
    /// JSON-RPC request id of the inbound MCP request that triggered
    /// this tool call, when one is available. Slice 2's `helper_ask`
    /// reads this to register a cancellation token under the id so a
    /// later `notifications/cancelled` from the hub can interrupt the
    /// in-flight call. `None` means the call originated locally (CLI,
    /// in-process tests) or via a notification.
    pub rpc_id: Option<RpcId>,
    /// Optional progress sink — when present, the dispatcher is
    /// streaming the response (`text/event-stream` with
    /// `_meta.progressToken`) and a tool may push fully-formed
    /// JSON-RPC `notifications/progress` envelopes onto this channel
    /// to be flushed to the client mid-call. Tools that don't stream
    /// just leave it untouched.
    pub progress_sink: Option<mpsc::Sender<Value>>,
}

impl ToolContext {
    pub fn new_with_workspace(workspace: PathBuf, session_key: String) -> Self {
        Self {
            workspace,
            session_key,
            cancel: tokio_util::sync::CancellationToken::new(),
            file_state: FileStateTracker::default(),
            incoming_call_depth: None,
            caller_fingerprint: None,
            rpc_id: None,
            progress_sink: None,
        }
    }

    /// Build a throw-away context for tests.
    pub fn for_test() -> Self {
        Self::new_with_workspace(std::env::temp_dir(), "cli:direct".into())
    }

    /// Return the value an outbound MCP wrapper should put on the
    /// wire: the inbound depth plus one (`None` ⇒ 1). Centralised so
    /// the wrapper and tests agree on the off-by-one convention.
    pub fn outbound_call_depth(&self) -> u32 {
        self.incoming_call_depth.unwrap_or(0).saturating_add(1)
    }
}

/// Uniform return type for tool execution. `is_error` is true when the
/// tool raised — the runner appends the content as a tool message
/// either way, the flag only drives the `tools_used` stat and logging
/// color.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    /// Structured side-channel a tool can attach for callers that
    /// understand it. The MCP transport surfaces this as the `_meta`
    /// field on the wire (per the MCP `tools/call` convention) and
    /// in-process callers that don't recognise it just ignore it.
    ///
    /// Mode 2's `helper_ask` uses this to return the helper session
    /// id, iteration count, and `Usage` figures alongside the text
    /// answer; v1 tools all leave it `None` and nothing changes for
    /// them.
    pub meta: Option<Value>,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            meta: None,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            meta: None,
        }
    }
    /// Attach a structured `_meta` payload to a result. Returns
    /// `self` for fluent chaining: `ToolResult::ok(..).with_meta(..)`.
    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = Some(meta);
        self
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema `function.parameters` object.
    fn parameters(&self) -> Value;
    /// Whether this tool is safe to run concurrently with other
    /// `concurrency_safe` tools in the same batch.
    fn concurrency_safe(&self) -> bool {
        false
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult;
}

pub type DynTool = Arc<dyn Tool>;

#[cfg(test)]
mod rpc_id_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rpc_id_round_trips_number() {
        let v = json!(42);
        let id = RpcId::from_json(&v).expect("number id");
        assert_eq!(id, RpcId::Number(42));
        assert_eq!(id.to_json(), json!(42));
    }

    #[test]
    fn rpc_id_round_trips_string() {
        let v = json!("abc-123");
        let id = RpcId::from_json(&v).expect("string id");
        assert_eq!(id, RpcId::String("abc-123".into()));
        assert_eq!(id.to_json(), json!("abc-123"));
    }

    #[test]
    fn rpc_id_rejects_null() {
        assert!(RpcId::from_json(&json!(null)).is_none());
    }

    #[test]
    fn rpc_id_rejects_object() {
        assert!(RpcId::from_json(&json!({"x": 1})).is_none());
    }

    #[test]
    fn rpc_id_rejects_float() {
        // serde_json represents non-integer numbers as Number but
        // as_i64 fails; we treat that as "not a routable id".
        assert!(RpcId::from_json(&json!(2.5)).is_none());
    }
}
