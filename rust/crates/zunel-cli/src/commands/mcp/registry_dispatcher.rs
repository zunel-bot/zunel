//! Registry-backed MCP dispatcher used by `zunel mcp agent`.
//!
//! Wraps a [`zunel_tools::ToolRegistry`] behind the
//! [`zunel_mcp_self::McpDispatcher`] trait so the same Streamable
//! HTTP transport that powers `zunel-mcp-self` can serve any zunel
//! instance's tool surface. The dispatcher is intentionally stateless
//! across MCP requests *except* for the shared
//! [`zunel_tools::FileStateTracker`] inside the [`ToolContext`]: that
//! object is cloned (it's `Arc`-backed) into the context for every
//! call so the read→edit safety check still works for clients that
//! issue both calls within the same server lifetime.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use zunel_mcp_self::{DispatchMeta, McpDispatcher};
use zunel_tools::{RpcId, ToolContext, ToolRegistry};

use super::cancel_registry::CancelRegistry;

/// Server identity advertised on `initialize`. Includes the instance
/// name so multi-server hosts can disambiguate when more than one
/// instance is exposed.
pub struct DispatcherIdentity {
    pub server_name: String,
    pub server_version: String,
}

/// JSON-RPC dispatcher that exposes a `ToolRegistry` over MCP.
///
/// Construction is cheap (no work happens until [`dispatch`] is
/// invoked) and the dispatcher is `Send + Sync`, so callers may share
/// a single instance across all accepted connections.
pub struct RegistryDispatcher {
    identity: DispatcherIdentity,
    registry: Arc<ToolRegistry>,
    context: Arc<ToolContext>,
    /// Cached `tools/list` response. Built once at construction so
    /// `tools/list` requests don't re-walk the registry; the registry
    /// is immutable for the lifetime of the dispatcher.
    tools_descriptor: Value,
    /// Shared registry of in-flight rpc-id → cancel-token entries.
    /// `None` disables `notifications/cancelled` routing — the
    /// dispatcher silently drops the notification, which matches the
    /// spec ("notifications carry no reply"). The CLI wires a real
    /// registry whenever Mode 2 is enabled.
    cancel_registry: Option<Arc<CancelRegistry>>,
}

impl RegistryDispatcher {
    pub fn new(identity: DispatcherIdentity, registry: ToolRegistry, context: ToolContext) -> Self {
        let tools_descriptor = build_tools_descriptor(&registry);
        Self {
            identity,
            registry: Arc::new(registry),
            context: Arc::new(context),
            tools_descriptor,
            cancel_registry: None,
        }
    }

    /// Wire `notifications/cancelled` to a shared cancel registry.
    /// Mode 2's `helper_ask` registers cancel tokens under the
    /// inbound rpc id; routing here closes the loop so a hub
    /// notification fires the matching token.
    pub fn with_cancel_registry(mut self, registry: Arc<CancelRegistry>) -> Self {
        self.cancel_registry = Some(registry);
        self
    }

    /// Number of tools currently exposed. Used by the boot banner so
    /// operators can confirm at a glance that gating did what they
    /// expected.
    pub fn tools_for_banner(&self) -> usize {
        self.tools_descriptor
            .get("tools")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0)
    }

    fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": self.identity.server_name,
                "version": self.identity.server_version,
            }
        })
    }

    async fn call_tool(&self, msg: &Value, meta: &DispatchMeta) -> Value {
        self.call_tool_with_progress(msg, meta, None).await
    }

    /// Streaming-aware tool dispatch. Identical to [`call_tool`]
    /// except that when `progress_sink` is `Some` and the inbound
    /// request carries `params._meta.progressToken`, we stamp the
    /// sink onto [`ToolContext::progress_sink`] so a tool (notably
    /// Mode 2's `helper_ask`) can push mid-call
    /// `notifications/progress` envelopes onto the open SSE
    /// response.
    async fn call_tool_with_progress(
        &self,
        msg: &Value,
        meta: &DispatchMeta,
        progress_sink: Option<tokio::sync::mpsc::Sender<Value>>,
    ) -> Value {
        let name = msg
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = msg
            .get("params")
            .and_then(|p| p.get("arguments"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let progress_token = msg
            .get("params")
            .and_then(|p| p.get("_meta"))
            .and_then(|m| m.get("progressToken"))
            .cloned();
        // Clone the base context so this request gets its own
        // `incoming_call_depth` stamp without mutating shared state.
        // The clone is cheap: every field is either plain-old-data or
        // already `Arc`-backed (notably `FileStateTracker`, which we
        // intentionally share so read→edit safety still works across
        // sequential MCP requests on the same server).
        let mut ctx = self.context.as_ref().clone();
        ctx.incoming_call_depth = meta.call_depth;
        // Per-caller routing: tools that key state by who's calling
        // (Mode 2's `helper_ask`, future audit tools) read this from
        // the context. Loopback-no-auth runs leave it as `None`.
        ctx.caller_fingerprint = meta.caller_fingerprint.clone();
        // Slice 2 cancellation: helper_ask reads `rpc_id` to register
        // a cancellation token under the inbound JSON-RPC id so a
        // later `notifications/cancelled` can interrupt the call.
        ctx.rpc_id = meta.rpc_id.clone();
        // Slice 2 streaming: stamp the progress sink onto the ctx so
        // the tool (helper_ask) can push notifications/progress
        // envelopes mid-call. We bridge the raw progressToken
        // through a wrapper sender that prepends the JSON-RPC
        // notification envelope so tools don't need to know about
        // the wire format.
        if let (Some(sink), Some(token)) = (progress_sink, progress_token) {
            let (tool_tx, mut tool_rx) = tokio::sync::mpsc::channel::<Value>(64);
            ctx.progress_sink = Some(tool_tx);
            // Spawn a translator that wraps each raw progress payload
            // in a `notifications/progress` envelope and forwards it
            // to the transport sink. The task ends when the tool
            // drops its sender (call completes) or when the
            // transport sink is dropped.
            tokio::spawn(async move {
                while let Some(payload) = tool_rx.recv().await {
                    let envelope = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {
                            "progressToken": token,
                            "data": payload,
                        }
                    });
                    if sink.send(envelope).await.is_err() {
                        break;
                    }
                }
            });
        }
        // ToolRegistry::execute always returns Ok (errors are folded
        // into ToolResult::is_error) so the unwrap is total here. Use
        // `expect` defensively to surface any future Infallible
        // contract change.
        let result = self
            .registry
            .execute(name, args, &ctx)
            .await
            .expect("ToolRegistry::execute is Infallible");
        let mut response = json!({
            "content": [{"type": "text", "text": result.content}],
            "isError": result.is_error,
        });
        // MCP's `_meta` channel is the right place for structured
        // side-channel info (helper session ids, iteration counts,
        // usage). Tools that leave `meta = None` round-trip
        // identically to v1.
        if let Some(meta) = result.meta {
            response["_meta"] = meta;
        }
        response
    }
}

#[async_trait]
impl McpDispatcher for RegistryDispatcher {
    async fn dispatch(&self, message: &Value, meta: &DispatchMeta) -> Option<Value> {
        let method = message.get("method").and_then(Value::as_str)?;
        // Slice 2: hub-issued cancellation. Spec wire format:
        //   { "jsonrpc": "2.0", "method": "notifications/cancelled",
        //     "params": { "requestId": <id>, "reason": "..." } }
        // We find the matching in-flight call's CancellationToken
        // and fire it. Notifications are reply-less by spec, so we
        // still return None either way.
        if method == "notifications/cancelled" {
            if let Some(registry) = &self.cancel_registry {
                let request_id = message
                    .get("params")
                    .and_then(|p| p.get("requestId"))
                    .and_then(RpcId::from_json);
                if let Some(id) = request_id {
                    let fired = registry.cancel(&id);
                    tracing::debug!(?id, fired, "notifications/cancelled routed");
                }
            }
            return None;
        }
        if method.starts_with("notifications/") {
            return None;
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => self.initialize_result(),
            "tools/list" => self.tools_descriptor.clone(),
            "tools/call" => self.call_tool(message, meta).await,
            _ => json!({}),
        };
        Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }

    async fn dispatch_streaming(
        &self,
        message: &Value,
        meta: &DispatchMeta,
        progress_sink: tokio::sync::mpsc::Sender<Value>,
    ) -> Option<Value> {
        let method = message.get("method").and_then(Value::as_str)?;
        // Streaming is only meaningful for tools/call — for every
        // other method the caller is asking for a static reply that
        // doesn't have intermediate state to forward, so we delegate
        // to the non-streaming path.
        if method != "tools/call" {
            return self.dispatch(message, meta).await;
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let result = self
            .call_tool_with_progress(message, meta, Some(progress_sink))
            .await;
        Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }
}

/// Format the registry as an MCP `tools/list` payload. The
/// per-tool object uses MCP's `inputSchema` field name (note the
/// camelCase) which differs from OpenAI's `parameters`.
fn build_tools_descriptor(registry: &ToolRegistry) -> Value {
    // Reuse get_definitions for ordering parity with the agent loop
    // (native tools first, mcp_* last) but rewrite the field names
    // since MCP and OpenAI disagree on the wire format.
    let mut tools: Vec<Value> = Vec::new();
    for definition in registry.get_definitions() {
        let Some(function) = definition.get("function") else {
            continue;
        };
        let name = function.get("name").and_then(Value::as_str).unwrap_or("");
        let description = function
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let parameters = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        tools.push(json!({
            "name": name,
            "description": description,
            "inputSchema": parameters,
        }));
    }
    json!({ "tools": tools })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use zunel_tools::{Tool, ToolResult};

    struct FakeTool {
        name: &'static str,
        description: &'static str,
        echo: &'static str,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            self.description
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult::ok(self.echo)
        }
    }

    /// Tool that captures the inbound depth from its `ToolContext`
    /// into a shared slot so tests can assert what the dispatcher
    /// stamped onto the per-call context.
    struct DepthCapturingTool {
        captured: Arc<Mutex<Option<Option<u32>>>>,
    }

    #[async_trait]
    impl Tool for DepthCapturingTool {
        fn name(&self) -> &'static str {
            "depth_probe"
        }
        fn description(&self) -> &'static str {
            "Capture incoming_call_depth"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value, ctx: &ToolContext) -> ToolResult {
            *self.captured.lock().expect("captured slot") = Some(ctx.incoming_call_depth);
            ToolResult::ok("captured")
        }
    }

    fn registry_with_one_tool() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(FakeTool {
            name: "ping",
            description: "Reply with pong",
            echo: "pong",
        }));
        reg
    }

    fn dispatcher_for(registry: ToolRegistry) -> RegistryDispatcher {
        RegistryDispatcher::new(
            DispatcherIdentity {
                server_name: "zunel-test".into(),
                server_version: "0.0.0".into(),
            },
            registry,
            ToolContext::for_test(),
        )
    }

    #[tokio::test]
    async fn initialize_returns_serverinfo() {
        let dispatcher = dispatcher_for(registry_with_one_tool());
        let response = dispatcher
            .dispatch(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
                &DispatchMeta::default(),
            )
            .await
            .expect("response");
        let info = &response["result"]["serverInfo"];
        assert_eq!(info["name"], "zunel-test");
        assert_eq!(info["version"], "0.0.0");
    }

    #[tokio::test]
    async fn tools_list_uses_input_schema_field() {
        let dispatcher = dispatcher_for(registry_with_one_tool());
        let response = dispatcher
            .dispatch(
                &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                &DispatchMeta::default(),
            )
            .await
            .expect("response");
        let tools = response["result"]["tools"].as_array().expect("array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "ping");
        assert!(tools[0]["inputSchema"].is_object());
        // MCP's field is inputSchema, *not* parameters; assert we
        // didn't accidentally leak the OpenAI-shaped definition.
        assert!(tools[0].get("parameters").is_none());
    }

    #[tokio::test]
    async fn tools_call_routes_to_registry() {
        let dispatcher = dispatcher_for(registry_with_one_tool());
        let response = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {"name": "ping", "arguments": {}}
                }),
                &DispatchMeta::default(),
            )
            .await
            .expect("response");
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(response["result"]["content"][0]["text"], "pong");
    }

    #[tokio::test]
    async fn tools_call_returns_error_for_unknown_tool() {
        let dispatcher = dispatcher_for(registry_with_one_tool());
        let response = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "tools/call",
                    "params": {"name": "nope", "arguments": {}}
                }),
                &DispatchMeta::default(),
            )
            .await
            .expect("response");
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("unknown tool"));
    }

    #[tokio::test]
    async fn notifications_return_none() {
        let dispatcher = dispatcher_for(registry_with_one_tool());
        let response = dispatcher
            .dispatch(
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                &DispatchMeta::default(),
            )
            .await;
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn dispatch_meta_call_depth_lands_on_tool_context() {
        let captured: Arc<Mutex<Option<Option<u32>>>> = Arc::new(Mutex::new(None));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DepthCapturingTool {
            captured: captured.clone(),
        }));
        let dispatcher = dispatcher_for(reg);

        let _ = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "tools/call",
                    "params": {"name": "depth_probe", "arguments": {}}
                }),
                &DispatchMeta {
                    call_depth: Some(3),
                    caller_fingerprint: None,
                    rpc_id: None,
                },
            )
            .await
            .expect("response");

        let observed = captured
            .lock()
            .expect("captured slot")
            .expect("depth captured");
        assert_eq!(observed, Some(3));
    }

    /// Tool that emits a structured `_meta` payload alongside its
    /// text content. Mode 2's `helper_ask` uses this to surface
    /// helper session ids; this test pins the dispatcher's contract
    /// that `_meta` lands as a sibling of `content`/`isError` so MCP
    /// clients see it as the spec-defined `_meta` field.
    struct MetaEmittingTool;

    #[async_trait]
    impl Tool for MetaEmittingTool {
        fn name(&self) -> &'static str {
            "meta_probe"
        }
        fn description(&self) -> &'static str {
            "Emit a result with a _meta side-channel"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult::ok("answer")
                .with_meta(json!({"session_id": "mode2:abc:test", "iterations": 2}))
        }
    }

    #[tokio::test]
    async fn tools_call_surfaces_meta_when_tool_attaches_it() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(MetaEmittingTool));
        let dispatcher = dispatcher_for(reg);
        let response = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 99,
                    "method": "tools/call",
                    "params": {"name": "meta_probe", "arguments": {}}
                }),
                &DispatchMeta::default(),
            )
            .await
            .expect("response");
        assert_eq!(response["result"]["content"][0]["text"], "answer");
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(response["result"]["_meta"]["session_id"], "mode2:abc:test");
        assert_eq!(response["result"]["_meta"]["iterations"], 2);
    }

    #[tokio::test]
    async fn tools_call_omits_meta_when_tool_does_not_attach_it() {
        let dispatcher = dispatcher_for(registry_with_one_tool());
        let response = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 100,
                    "method": "tools/call",
                    "params": {"name": "ping", "arguments": {}}
                }),
                &DispatchMeta::default(),
            )
            .await
            .expect("response");
        assert!(
            response["result"].get("_meta").is_none(),
            "_meta should be absent when tool returns meta=None; got {:#?}",
            response["result"]
        );
    }

    #[tokio::test]
    async fn dispatch_meta_default_means_top_level_call() {
        let captured: Arc<Mutex<Option<Option<u32>>>> = Arc::new(Mutex::new(None));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DepthCapturingTool {
            captured: captured.clone(),
        }));
        let dispatcher = dispatcher_for(reg);

        let _ = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "tools/call",
                    "params": {"name": "depth_probe", "arguments": {}}
                }),
                &DispatchMeta::default(),
            )
            .await
            .expect("response");

        let observed = captured
            .lock()
            .expect("captured slot")
            .expect("depth captured");
        assert_eq!(observed, None);
    }

    /// Tool that captures the inbound JSON-RPC id from its
    /// `ToolContext` so the dispatcher's slice-2 plumbing can be
    /// asserted in isolation.
    struct RpcIdCapturingTool {
        captured: Arc<Mutex<Option<Option<zunel_tools::RpcId>>>>,
    }

    #[async_trait]
    impl Tool for RpcIdCapturingTool {
        fn name(&self) -> &'static str {
            "rpc_probe"
        }
        fn description(&self) -> &'static str {
            "Capture rpc_id"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value, ctx: &ToolContext) -> ToolResult {
            *self.captured.lock().expect("captured slot") = Some(ctx.rpc_id.clone());
            ToolResult::ok("captured")
        }
    }

    #[tokio::test]
    async fn dispatch_meta_rpc_id_lands_on_tool_context() {
        let captured: Arc<Mutex<Option<Option<zunel_tools::RpcId>>>> = Arc::new(Mutex::new(None));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(RpcIdCapturingTool {
            captured: captured.clone(),
        }));
        let dispatcher = dispatcher_for(reg);

        let _ = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "id": "req-42",
                    "method": "tools/call",
                    "params": {"name": "rpc_probe", "arguments": {}}
                }),
                &DispatchMeta {
                    call_depth: None,
                    caller_fingerprint: None,
                    rpc_id: Some(zunel_tools::RpcId::String("req-42".into())),
                },
            )
            .await
            .expect("response");

        let observed = captured
            .lock()
            .expect("captured slot")
            .clone()
            .expect("rpc_id captured");
        assert_eq!(observed, Some(zunel_tools::RpcId::String("req-42".into())));
    }

    #[tokio::test]
    async fn notifications_cancelled_fires_registered_token() {
        use crate::commands::mcp::cancel_registry::CancelRegistry;
        let registry = CancelRegistry::new();
        // Pre-register an entry so we can observe the token firing.
        let guard = registry.register(zunel_tools::RpcId::String("req-cancel-test".into()));
        let token = guard.token();

        let dispatcher =
            dispatcher_for(registry_with_one_tool()).with_cancel_registry(Arc::clone(&registry));

        let response = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": "req-cancel-test", "reason": "user pressed esc"}
                }),
                &DispatchMeta::default(),
            )
            .await;

        assert!(response.is_none(), "notifications produce no reply");
        assert!(token.is_cancelled(), "registered token should fire");
        // Guard is still alive — drop it explicitly so we don't leak
        // the entry between tests (other tests build fresh registries
        // anyway, but discipline matters).
        drop(guard);
    }

    #[tokio::test]
    async fn notifications_cancelled_with_unknown_id_is_a_safe_noop() {
        use crate::commands::mcp::cancel_registry::CancelRegistry;
        let registry = CancelRegistry::new();
        let dispatcher =
            dispatcher_for(registry_with_one_tool()).with_cancel_registry(Arc::clone(&registry));

        let response = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": 9999}
                }),
                &DispatchMeta::default(),
            )
            .await;

        assert!(response.is_none());
    }

    #[tokio::test]
    async fn dispatch_meta_default_leaves_rpc_id_unset() {
        let captured: Arc<Mutex<Option<Option<zunel_tools::RpcId>>>> = Arc::new(Mutex::new(None));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(RpcIdCapturingTool {
            captured: captured.clone(),
        }));
        let dispatcher = dispatcher_for(reg);

        let _ = dispatcher
            .dispatch(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "tools/call",
                    "params": {"name": "rpc_probe", "arguments": {}}
                }),
                &DispatchMeta::default(),
            )
            .await
            .expect("response");

        let observed = captured
            .lock()
            .expect("captured slot")
            .clone()
            .expect("rpc_id captured");
        assert_eq!(observed, None);
    }
}
