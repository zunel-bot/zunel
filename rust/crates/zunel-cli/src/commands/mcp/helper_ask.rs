//! `helper_ask` — Mode 2's "agent-loop-as-tool" surface.
//!
//! Registered on `zunel mcp agent` only when `--mode2` is set. The
//! tool runs a fresh [`AgentLoop`] inside the helper instance per call
//! and returns the final assistant text plus a structured `_meta`
//! block (helper session id, iteration count, tools used, usage).
//!
//! Currently supports:
//!   * single-JSON response (no SSE / progress streaming yet),
//!   * `reject` and `allow_all` approval policies,
//!   * caller-controlled `session_id` namespaced by API-key fingerprint,
//!   * per-call `max_iterations` arg capped against the CLI ceiling.
//!
//! Streaming, approval-forwarding, and per-call timeouts are deferred —
//! see [`docs/instance-as-mcp-mode2.md`](../../../../../../../docs/instance-as-mcp-mode2.md).
//!
//! ## Threading model
//!
//! The tool itself is `Send + Sync` — every per-call AgentLoop is built
//! from cheaply-cloned handles (provider/sessions are `Arc`-backed,
//! defaults are owned `Clone`). That means a single dispatcher can
//! safely fan helper_ask calls out across concurrent MCP requests; the
//! only shared mutable state across calls is whatever
//! `SessionManager` flushes to disk, and that's already
//! atomic-temp-file-rename safe.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use zunel_config::AgentDefaults;
use zunel_core::{
    AgentLoop, AllowAllApprovalHandler, ApprovalHandler, ApprovalScope, RejectAllApprovalHandler,
    SessionManager,
};
use zunel_providers::{LLMProvider, StreamEvent};
use zunel_tools::{Tool, ToolContext, ToolRegistry, ToolResult};

use super::approval_queue::{ApprovalQueue, QueueApprovalHandler};
use super::cancel_registry::CancelRegistry;

/// Approval policy applied to tool calls *inside* the helper's
/// AgentLoop.
///
/// `reject` is the safe default: there is no human in the loop on the
/// helper side, so any tool that would normally prompt for approval
/// fails the call cleanly. `allow_all` is the explicit opt-in for
/// trusted operators running fully read-only helpers — it cannot be
/// reached without the operator passing `--mode2-approval allow_all`
/// at server boot. `forward` enqueues each request onto the shared
/// [`ApprovalQueue`] so the hub can poll via
/// `helper_pending_approvals` and resolve via `helper_approve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperApprovalPolicy {
    Reject,
    AllowAll,
    Forward,
}

impl HelperApprovalPolicy {
    pub fn from_cli_str(s: &str) -> Result<Self, String> {
        match s {
            "reject" => Ok(Self::Reject),
            "allow_all" => Ok(Self::AllowAll),
            "forward" => Ok(Self::Forward),
            other => Err(format!(
                "unknown --mode2-approval value {other:?}; expected 'reject', 'allow_all', or 'forward'"
            )),
        }
    }
}

/// `helper_ask` MCP tool.
pub struct HelperAskTool {
    provider: Arc<dyn LLMProvider>,
    defaults: AgentDefaults,
    sessions: Arc<SessionManager>,
    /// The helper's own tool registry as the inner AgentLoop will see
    /// it — Mode 1's filtered registry minus `helper_ask` itself.
    /// Cloning this per call is cheap because every entry is an
    /// `Arc<dyn Tool>`.
    inner_registry: ToolRegistry,
    workspace: std::path::PathBuf,
    approval_policy: HelperApprovalPolicy,
    /// CLI-supplied hard ceiling on iterations a single helper_ask
    /// call can spend. Defaults to the helper's own
    /// `agents.defaults.max_tool_iterations` when `None`.
    max_iterations_cap: Option<usize>,
    /// When `true`, the tool ignores any caller-supplied
    /// `system_prompt` arg and surfaces an `_meta.system_prompt`
    /// diagnostic so the hub can see why its prompt didn't take.
    /// Wired from `--mode2-disable-system-prompt`.
    system_prompt_disabled: bool,
    /// Shared registry of in-flight rpc-id → cancel-token entries.
    /// `None` skips cancellation entirely (useful for in-process
    /// tests; the dispatcher always wires this up at runtime).
    cancel_registry: Option<Arc<CancelRegistry>>,
    /// Per-call wallclock ceiling. When `Some`, the helper's
    /// AgentLoop is wrapped in `tokio::time::timeout`; on expiry we
    /// fire the cancel token and return a structured error so the
    /// hub can distinguish "I cancelled" from "the helper timed out
    /// on its own". `None` means "no per-call timeout".
    call_timeout: Option<Duration>,
    /// Shared approval queue used when `approval_policy ==
    /// HelperApprovalPolicy::Forward`. The CLI wires up the same
    /// `Arc<ApprovalQueue>` across this tool, the
    /// [`QueueApprovalHandler`] inside the helper's AgentLoop, and
    /// the two new `helper_pending_approvals` /
    /// `helper_approve` tools so a single decision flows end-to-end.
    /// `None` keeps the legacy reject/allow_all behaviour.
    approval_queue: Option<Arc<ApprovalQueue>>,
    /// Per-approval wallclock ceiling. After this duration with no
    /// matching `helper_approve` decision the queued request flips
    /// to "deny" so the helper's tool loop unblocks. Ignored unless
    /// `approval_policy == Forward`.
    approval_timeout: Duration,
}

/// Maximum length (UTF-8 bytes) of a caller-supplied `system_prompt`.
/// Bigger inputs are rejected with a structured error rather than
/// silently truncated, since silent truncation would mid-sentence the
/// caller's persona. 8 KiB is enough for any realistic operator
/// override and small enough that we never push the helper's own
/// system-message budget.
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 8 * 1024;

impl HelperAskTool {
    pub fn new(
        provider: Arc<dyn LLMProvider>,
        defaults: AgentDefaults,
        sessions: Arc<SessionManager>,
        inner_registry: ToolRegistry,
        workspace: std::path::PathBuf,
        approval_policy: HelperApprovalPolicy,
        max_iterations_cap: Option<usize>,
    ) -> Self {
        Self {
            provider,
            defaults,
            sessions,
            inner_registry,
            workspace,
            approval_policy,
            max_iterations_cap,
            system_prompt_disabled: false,
            cancel_registry: None,
            call_timeout: None,
            approval_queue: None,
            approval_timeout: Duration::from_secs(300),
        }
    }

    /// Builder hook used by the CLI when `--mode2-disable-system-prompt`
    /// is set. Once disabled the policy is per-tool and cannot be
    /// re-enabled per-call — the caller has no path to forge a way
    /// in.
    pub fn with_system_prompt_disabled(mut self, disabled: bool) -> Self {
        self.system_prompt_disabled = disabled;
        self
    }

    /// Wire the tool to a shared [`CancelRegistry`] so a
    /// `notifications/cancelled` from the hub can interrupt the
    /// helper mid-call. Without this opt-in the tool runs without
    /// cancellation support (still safe — no token is ever fired).
    pub fn with_cancel_registry(mut self, registry: Arc<CancelRegistry>) -> Self {
        self.cancel_registry = Some(registry);
        self
    }

    /// Wallclock ceiling for a single helper_ask call. Wired from
    /// `--mode2-call-timeout-secs`. `None` keeps the legacy
    /// "no timeout" behaviour.
    pub fn with_call_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Wire an [`ApprovalQueue`] for `--mode2-approval forward`.
    /// When the policy is `Forward`, every approval request from
    /// the helper's AgentLoop lands on this queue; the hub polls via
    /// `helper_pending_approvals` and resolves via `helper_approve`.
    /// When the policy is `Reject` or `AllowAll`, the queue is
    /// unused — the static handlers short-circuit before reaching
    /// it.
    pub fn with_approval_queue(mut self, queue: Arc<ApprovalQueue>) -> Self {
        self.approval_queue = Some(queue);
        self
    }

    /// Per-approval wallclock ceiling. Default is 5 minutes.
    pub fn with_approval_timeout(mut self, timeout: Duration) -> Self {
        self.approval_timeout = timeout;
        self
    }

    /// Resolve the effective iteration ceiling for one call. The
    /// caller's tool-arg, the CLI cap, and the helper's own default
    /// all participate; the tightest non-zero ceiling wins.
    fn effective_max_iterations(&self, caller_arg: Option<u64>) -> usize {
        let helper_default = self.defaults.max_tool_iterations.unwrap_or(15);
        let caller = caller_arg
            .map(|n| n.max(1) as usize)
            .unwrap_or(helper_default);
        let mut chosen = caller.min(helper_default);
        if let Some(cli_cap) = self.max_iterations_cap {
            chosen = chosen.min(cli_cap);
        }
        chosen.max(1)
    }
}

#[async_trait]
impl Tool for HelperAskTool {
    fn name(&self) -> &'static str {
        "helper_ask"
    }

    fn description(&self) -> &'static str {
        "Ask the helper agent to handle a prompt with its own LLM and tool registry. \
         Each call runs a fresh AgentLoop inside the helper instance and returns its \
         final answer."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Question or instruction to send to the helper agent."
                },
                "session_id": {
                    "type": "string",
                    "description": "Caller-supplied session key. Omit (or empty) for a fresh session. The helper namespaces the key with the matched API key fingerprint so two unrelated callers cannot collide."
                },
                "max_iterations": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Per-call upper bound on tool-loop iterations. Capped against the helper's CLI ceiling and its own configured default."
                },
                "system_prompt": {
                    "type": "string",
                    "maxLength": MAX_SYSTEM_PROMPT_BYTES as u64,
                    "description": "Optional operator persona prepended ahead of the helper's skills system message for this call only. Not persisted into the helper's session log. The helper may be configured (--mode2-disable-system-prompt) to ignore this field; in that case the tool surfaces _meta.system_prompt='ignored'."
                }
            }
        })
    }

    fn concurrency_safe(&self) -> bool {
        // helper_ask is *not* concurrency-safe within a single batch:
        // running two helper_ask calls back-to-back against the same
        // session would race on session writes. The MCP transport
        // already serialises requests on a single connection, but
        // the agent runner's batching path explicitly tags this so a
        // future tool-batch implementation respects it.
        false
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(prompt) = args.get("prompt").and_then(Value::as_str) else {
            return ToolResult::err("helper_ask: missing required string argument `prompt`");
        };
        if prompt.is_empty() {
            return ToolResult::err("helper_ask: `prompt` was empty");
        }

        let caller_max_iters = args.get("max_iterations").and_then(Value::as_u64);
        let max_iterations = self.effective_max_iterations(caller_max_iters);

        let caller_supplied_session = args
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let session_id = build_namespaced_session_id(
            ctx.caller_fingerprint.as_deref(),
            caller_supplied_session.as_deref(),
        );

        // Resolve the operator-persona override. Three opposed
        // outcomes feed `_meta.system_prompt` so the hub can debug:
        //   * "applied"  — caller passed a non-empty value, server
        //                  honoured it, AgentLoop got it via
        //                  with_extra_system_message.
        //   * "ignored"  — caller passed a value but the server is
        //                  configured to drop it (--mode2-
        //                  disable-system-prompt). The agent runs
        //                  with no extra persona.
        //   * absent     — caller omitted the field. We don't surface
        //                  `_meta.system_prompt` at all in that case
        //                  so the meta block stays compact.
        let raw_system_prompt = args
            .get("system_prompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(ref s) = raw_system_prompt {
            if s.len() > MAX_SYSTEM_PROMPT_BYTES {
                return ToolResult::err(format!(
                    "helper_ask: `system_prompt` exceeds maximum length \
                     ({} bytes, max {})",
                    s.len(),
                    MAX_SYSTEM_PROMPT_BYTES
                ));
            }
        }
        let (effective_system_prompt, system_prompt_status): (
            Option<String>,
            Option<&'static str>,
        ) = match (raw_system_prompt, self.system_prompt_disabled) {
            (None, _) => (None, None),
            (Some(s), _) if s.is_empty() => (None, None),
            (Some(_), true) => (None, Some("ignored")),
            (Some(s), false) => (Some(s), Some("applied")),
        };

        // Per-call AgentLoop. We rebuild from scratch so two
        // concurrent helper_ask calls don't share approval / token-
        // accounting state. All inputs are Arc-backed or cheap to
        // clone, so the cost is negligible compared to the LLM round
        // trip we're about to make.
        let approval_handler: Arc<dyn ApprovalHandler> = match self.approval_policy {
            HelperApprovalPolicy::Reject => Arc::new(RejectAllApprovalHandler),
            HelperApprovalPolicy::AllowAll => Arc::new(AllowAllApprovalHandler),
            HelperApprovalPolicy::Forward => match self.approval_queue.as_ref() {
                Some(queue) => Arc::new(QueueApprovalHandler::new(
                    Arc::clone(queue),
                    self.approval_timeout,
                )),
                None => {
                    // Misconfiguration: forward policy without a
                    // queue. Fail closed (deny) rather than crash
                    // the helper, and surface it through _meta so
                    // the operator notices.
                    return ToolResult::err(
                        "helper_ask: approval policy 'forward' requires an ApprovalQueue \
                         (server misconfigured)",
                    );
                }
            },
        };

        let mut defaults = self.defaults.clone();
        defaults.max_tool_iterations = Some(max_iterations);

        // Register a cancel guard under the inbound rpc id when both
        // the registry and the id are available; otherwise run with
        // a fresh never-cancelled token. Holding the guard across
        // the call ensures a panic / early return removes the
        // registry entry automatically (RAII).
        let cancel_guard = match (self.cancel_registry.as_ref(), ctx.rpc_id.as_ref()) {
            (Some(reg), Some(id)) => Some(reg.register(id.clone())),
            _ => None,
        };
        let cancel_token = cancel_guard
            .as_ref()
            .map(|g| g.token())
            .unwrap_or_else(tokio_util::sync::CancellationToken::new);

        let agent =
            AgentLoop::with_sessions(self.provider.clone(), defaults, (*self.sessions).clone())
                .with_tools(self.inner_registry.clone())
                .with_workspace(self.workspace.clone())
                .with_approval(approval_handler)
                // Approval-required mirrors the chosen policy: in `reject`
                // mode we want the runner to *consult* the approval handler
                // (which always denies), so the call fails with a clear
                // error rather than silently bypassing the gate. In
                // `allow_all` the handler approves unconditionally so the
                // flag's value is moot — set it true for consistency with
                // the docs.
                .with_approval_required(true)
                .with_approval_scope(ApprovalScope::All)
                .with_extra_system_message(effective_system_prompt)
                .with_cancel(cancel_token.clone());

        // The streaming sink: when the dispatcher gave us a progress
        // sink (the hub passed `_meta.progressToken` and accepts
        // text/event-stream), we translate every StreamEvent into a
        // structured progress payload and forward it. Otherwise we
        // just drain the sink locally so the agent loop's senders
        // don't block.
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
        let progress_sink = ctx.progress_sink.clone();
        let drain = tokio::spawn(async move {
            // We forward the full event stream as small JSON payloads
            // through the dispatcher-provided progress sink; the
            // dispatcher wraps each in an MCP `notifications/progress`
            // envelope before pushing it onto the SSE wire. With no
            // sink we still drain so the agent loop doesn't block.
            while let Some(event) = rx.recv().await {
                if let Some(sink) = progress_sink.as_ref() {
                    if let Some(payload) = stream_event_to_progress_payload(&event) {
                        if sink.send(payload).await.is_err() {
                            // Client disconnected — keep draining
                            // so the agent loop doesn't block, but
                            // stop forwarding.
                            while rx.recv().await.is_some() {}
                            return;
                        }
                    }
                }
            }
        });

        // Wrap the helper turn in an optional timeout. The outer
        // race between the timeout and the agent run lets us
        // distinguish "operator-set ceiling exceeded" from
        // "hub-issued cancel" in the result we surface back.
        let agent_fut = agent.process_streamed(&session_id, prompt, tx);
        let result = match self.call_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, agent_fut).await {
                Ok(Ok(r)) => Ok(r),
                Ok(Err(err)) => Err(err),
                Err(_) => {
                    // Timeout: trigger the cancel so any
                    // still-running tool call notices, then
                    // surface a structured error. We let the drain
                    // task finish forwarding any events it already
                    // pulled off the queue before tearing down so
                    // the hub still sees the partial progress.
                    cancel_token.cancel();
                    let _ = drain.await;
                    drop(cancel_guard);
                    return ToolResult::err(format!(
                        "helper_ask: per-call timeout exceeded ({}s)",
                        timeout.as_secs()
                    ));
                }
            },
            None => agent_fut.await,
        };

        // Wait for the drain task to finish forwarding the buffered
        // events that the agent loop pushed before it returned. If
        // we aborted here we'd race the drain task against the
        // function exit and lose progress notifications that the
        // hub was about to see.
        let _ = drain.await;

        let result = match result {
            Ok(r) => r,
            Err(err) => {
                drop(cancel_guard);
                // Cancellation surfaces as a typed AgentLoop error so
                // we can echo the application-defined code-32800
                // contract through the dispatcher. Other errors stay
                // generic.
                let msg = if matches!(err, zunel_core::Error::Cancelled) {
                    "helper_ask: cancelled by hub".to_string()
                } else {
                    format!("helper_ask: {err}")
                };
                return ToolResult::err(msg);
            }
        };
        drop(cancel_guard);

        // Build the structured `_meta` payload. The shape mirrors
        // `docs/instance-as-mcp-mode2.md` so callers that already
        // parse it can rely on the field set.
        let mut meta = json!({
            "session_id": session_id,
            "tools_used": result.tools_used,
            "usage": {
                "input": result.usage.prompt_tokens,
                "output": result.usage.completion_tokens,
                "reasoning": result.usage.reasoning_tokens,
                "cached": result.usage.cached_tokens,
            },
        });
        if let Some(status) = system_prompt_status {
            meta["system_prompt"] = json!(status);
        }

        let content = if result.content.is_empty() {
            // Empty assistant text is a real outcome (e.g. the model
            // produced only tool calls and never said anything). We
            // surface a placeholder so the caller has *something* to
            // render and the `_meta` still carries the diagnostic.
            "(helper produced no text response)".to_string()
        } else {
            result.content
        };

        ToolResult::ok(content).with_meta(meta)
    }
}

/// Compose `mode2:<fingerprint-or-anon>:<caller-or-fresh>`.
///
/// The fingerprint hop prevents two unrelated hub callers from
/// colliding on the same caller-supplied session key; the
/// fresh-per-call default avoids accidentally appending unrelated
/// turns to whatever session-id the model happened to invent. Both
/// behaviours match the spec in `docs/instance-as-mcp-mode2.md` Q4.
fn build_namespaced_session_id(
    caller_fingerprint: Option<&str>,
    caller_supplied: Option<&str>,
) -> String {
    let owner = caller_fingerprint.unwrap_or("anon");
    let suffix = match caller_supplied {
        Some(s) => s.to_string(),
        // Fresh-per-call: derive a stable nanosecond-precision token
        // so repeated invocations with no `session_id` arg get
        // distinct sessions but the value is still deterministic
        // within a single process tick (helps tests and tracing).
        None => fresh_session_suffix(),
    };
    format!("mode2:{owner}:{suffix}")
}

/// Slice 2 streaming: project a [`StreamEvent`] into a stable JSON
/// shape that helper_ask emits as the `data` field of an MCP
/// `notifications/progress` envelope. Returns `None` for events that
/// carry no caller-visible information (e.g. tool-call deltas the
/// client doesn't need to see).
///
/// The wire shape is:
///   * `{"kind": "content", "delta": "..."}`
///   * `{"kind": "tool_progress", "stage": "start"|"done", "name": "..."}`
///   * `{"kind": "done", "finish_reason": "..." (optional)}`
fn stream_event_to_progress_payload(event: &StreamEvent) -> Option<Value> {
    use zunel_providers::ToolProgress;
    match event {
        StreamEvent::ContentDelta(s) if !s.is_empty() => {
            Some(json!({"kind": "content", "delta": s}))
        }
        StreamEvent::ContentDelta(_) => None,
        StreamEvent::ToolProgress(progress) => match progress {
            ToolProgress::Start { name, .. } => {
                Some(json!({"kind": "tool_progress", "stage": "start", "name": name}))
            }
            ToolProgress::Done { name, ok, .. } => {
                Some(json!({"kind": "tool_progress", "stage": "done", "name": name, "ok": ok}))
            }
        },
        StreamEvent::Done(resp) => {
            let mut payload = json!({"kind": "done"});
            if let Some(reason) = &resp.finish_reason {
                payload["finish_reason"] = json!(reason);
            }
            Some(payload)
        }
        // Tool-call deltas are part of how the model assembles a
        // function-call payload — not interesting to a hub watching
        // progress, so we drop them.
        StreamEvent::ToolCallDelta { .. } => None,
    }
}

fn fresh_session_suffix() -> String {
    // We deliberately *don't* use a UUID crate — adding a dep for a
    // local cache key is overkill. Nanos-since-epoch + a fast
    // counter make collisions cosmically unlikely while keeping the
    // session id readable in `zunel sessions list`.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("fresh-{nanos:016x}-{seq:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;
    use zunel_config::AgentDefaults;
    use zunel_providers::{ChatMessage, GenerationSettings, LLMResponse, ToolSchema, Usage};

    /// Provider that always replies with a fixed string and counts
    /// calls. Sufficient for slice-1 helper_ask tests where the
    /// helper's job is "handle this prompt" (i.e. no tool-call loop).
    struct FakeProvider {
        reply: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LLMProvider for FakeProvider {
        async fn generate(
            &self,
            _model: &str,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _settings: &GenerationSettings,
        ) -> zunel_providers::Result<LLMResponse> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(LLMResponse {
                content: Some(self.reply.clone()),
                tool_calls: Vec::new(),
                usage: Usage {
                    prompt_tokens: 7,
                    completion_tokens: 11,
                    cached_tokens: 0,
                    reasoning_tokens: 0,
                },
                finish_reason: None,
            })
        }
    }

    fn make_tool(approval: HelperApprovalPolicy, workspace: &std::path::Path) -> HelperAskTool {
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider {
            reply: "helper-says-hi".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let defaults = AgentDefaults {
            provider: Some("custom".into()),
            model: "fake-x".into(),
            max_tool_iterations: Some(3),
            ..Default::default()
        };
        let sessions = Arc::new(SessionManager::new(workspace));
        HelperAskTool::new(
            provider,
            defaults,
            sessions,
            ToolRegistry::new(),
            workspace.to_path_buf(),
            approval,
            None,
        )
    }

    #[test]
    fn build_namespaced_session_id_uses_fingerprint_when_present() {
        let id = build_namespaced_session_id(Some("abcd1234"), Some("research-2026-04"));
        assert_eq!(id, "mode2:abcd1234:research-2026-04");
    }

    #[test]
    fn build_namespaced_session_id_falls_back_to_anon() {
        let id = build_namespaced_session_id(None, Some("research-2026-04"));
        assert_eq!(id, "mode2:anon:research-2026-04");
    }

    #[test]
    fn build_namespaced_session_id_generates_fresh_suffix_when_caller_omits_one() {
        let a = build_namespaced_session_id(Some("abcd1234"), None);
        let b = build_namespaced_session_id(Some("abcd1234"), None);
        assert!(a.starts_with("mode2:abcd1234:fresh-"));
        assert!(b.starts_with("mode2:abcd1234:fresh-"));
        assert_ne!(a, b, "fresh suffixes must collide-avoid via the counter");
    }

    #[test]
    fn approval_policy_parses_known_values() {
        assert_eq!(
            HelperApprovalPolicy::from_cli_str("reject").unwrap(),
            HelperApprovalPolicy::Reject
        );
        assert_eq!(
            HelperApprovalPolicy::from_cli_str("allow_all").unwrap(),
            HelperApprovalPolicy::AllowAll
        );
        assert_eq!(
            HelperApprovalPolicy::from_cli_str("forward").unwrap(),
            HelperApprovalPolicy::Forward
        );
        assert!(HelperApprovalPolicy::from_cli_str("nonsense").is_err());
    }

    #[test]
    fn effective_max_iterations_clamps_against_helper_default_and_cli_cap() {
        let tmp = tempdir().expect("tmpdir");
        let mut tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());
        // helper default is 3 (set in make_tool); caller asks for 10
        // -> clamp to 3.
        assert_eq!(tool.effective_max_iterations(Some(10)), 3);
        // Caller below default keeps the smaller number.
        assert_eq!(tool.effective_max_iterations(Some(2)), 2);
        // Floor at 1 even when caller passes 0.
        assert_eq!(tool.effective_max_iterations(Some(0)), 1);
        // CLI cap further tightens the ceiling.
        tool.max_iterations_cap = Some(2);
        assert_eq!(tool.effective_max_iterations(Some(10)), 2);
    }

    #[tokio::test]
    async fn helper_ask_returns_text_and_meta_with_namespaced_session() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());

        let mut ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        ctx.caller_fingerprint = Some("deadbeef".into());

        let result = tool
            .execute(
                json!({
                    "prompt": "draft me a poem",
                    "session_id": "lit-review",
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert_eq!(result.content, "helper-says-hi");

        let meta = result.meta.expect("_meta should be populated");
        assert_eq!(meta["session_id"], "mode2:deadbeef:lit-review");
        assert_eq!(meta["usage"]["input"], 7);
        assert_eq!(meta["usage"]["output"], 11);
        // Empty tools_used is the expected outcome here: the
        // helper's loop didn't call any tools (the test registered
        // an empty inner registry).
        assert_eq!(meta["tools_used"], json!([]));
    }

    #[tokio::test]
    async fn helper_ask_persists_session_so_caller_can_resume() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());

        let mut ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        ctx.caller_fingerprint = Some("cafef00d".into());

        // First call seeds the session.
        let first = tool
            .execute(
                json!({"prompt": "what's the plan", "session_id": "shared"}),
                &ctx,
            )
            .await;
        let first_meta = first.meta.expect("_meta on first call");
        let session_id = first_meta["session_id"]
            .as_str()
            .expect("session_id is a string")
            .to_string();
        assert_eq!(session_id, "mode2:cafef00d:shared");

        // Second call with the same key should resume the same
        // namespaced session — verify by reading the on-disk
        // session file and checking it has the second user turn.
        let _ = tool
            .execute(
                json!({"prompt": "and step two?", "session_id": "shared"}),
                &ctx,
            )
            .await;

        let session = SessionManager::new(tmp.path())
            .load(&session_id)
            .expect("session load")
            .expect("session exists after two calls");
        let turns: Vec<_> = session
            .messages()
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .collect();
        assert_eq!(
            turns.len(),
            2,
            "both user prompts should land in the same session"
        );
    }

    #[tokio::test]
    async fn helper_ask_rejects_missing_prompt() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());
        let ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        let result = tool.execute(json!({}), &ctx).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("prompt"),
            "expected prompt-missing error, got {}",
            result.content
        );
    }

    #[tokio::test]
    async fn helper_ask_falls_back_to_anon_namespace_when_unauthenticated() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());
        // No caller_fingerprint set.
        let ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        let result = tool
            .execute(json!({"prompt": "hi", "session_id": "loopback-dev"}), &ctx)
            .await;
        let meta = result.meta.expect("_meta");
        assert_eq!(meta["session_id"], "mode2:anon:loopback-dev");
    }

    #[tokio::test]
    async fn helper_ask_applies_system_prompt_and_reports_status_in_meta() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());
        let ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());

        let result = tool
            .execute(
                json!({
                    "prompt": "go",
                    "system_prompt": "You are a research helper.",
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "unexpected error: {}", result.content);
        let meta = result.meta.expect("_meta");
        assert_eq!(meta["system_prompt"], "applied");
    }

    #[tokio::test]
    async fn helper_ask_omits_system_prompt_status_when_caller_omits_field() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());
        let ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        let result = tool.execute(json!({"prompt": "go"}), &ctx).await;
        let meta = result.meta.expect("_meta");
        assert!(
            meta.get("system_prompt").is_none(),
            "no system_prompt key in meta when caller omits it: {meta}"
        );
    }

    #[tokio::test]
    async fn helper_ask_reports_ignored_when_disable_flag_is_set() {
        let tmp = tempdir().expect("tmpdir");
        let tool =
            make_tool(HelperApprovalPolicy::Reject, tmp.path()).with_system_prompt_disabled(true);
        let ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        let result = tool
            .execute(
                json!({"prompt": "go", "system_prompt": "ignored persona"}),
                &ctx,
            )
            .await;
        let meta = result.meta.expect("_meta");
        assert_eq!(meta["system_prompt"], "ignored");
    }

    #[tokio::test]
    async fn helper_ask_rejects_oversized_system_prompt() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());
        let ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        let too_big = "a".repeat(MAX_SYSTEM_PROMPT_BYTES + 1);
        let result = tool
            .execute(json!({"prompt": "go", "system_prompt": too_big}), &ctx)
            .await;
        assert!(result.is_error, "expected oversize rejection");
        assert!(
            result.content.contains("system_prompt"),
            "error mentions field name: {}",
            result.content
        );
    }

    /// Provider that sleeps inside `generate_stream` so the test can
    /// reliably win the cancel race. Used by the cancellation tests.
    struct SlowStreamProvider;

    #[async_trait]
    impl LLMProvider for SlowStreamProvider {
        async fn generate(
            &self,
            _model: &str,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _settings: &GenerationSettings,
        ) -> zunel_providers::Result<LLMResponse> {
            unreachable!("streaming path only")
        }

        fn generate_stream<'a>(
            &'a self,
            _model: &'a str,
            _messages: &'a [ChatMessage],
            _tools: &'a [ToolSchema],
            _settings: &'a GenerationSettings,
        ) -> futures::stream::BoxStream<'a, zunel_providers::Result<zunel_providers::StreamEvent>>
        {
            Box::pin(async_stream::stream! {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                yield Ok(zunel_providers::StreamEvent::Done(LLMResponse {
                    content: Some("never seen".into()),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    finish_reason: None,
                }));
            })
        }
    }

    fn make_tool_with_slow_provider(
        approval: HelperApprovalPolicy,
        workspace: &std::path::Path,
    ) -> HelperAskTool {
        let provider: Arc<dyn LLMProvider> = Arc::new(SlowStreamProvider);
        let defaults = AgentDefaults {
            provider: Some("custom".into()),
            model: "fake-x".into(),
            max_tool_iterations: Some(3),
            ..Default::default()
        };
        let sessions = Arc::new(SessionManager::new(workspace));
        HelperAskTool::new(
            provider,
            defaults,
            sessions,
            ToolRegistry::new(),
            workspace.to_path_buf(),
            approval,
            None,
        )
    }

    #[tokio::test]
    async fn helper_ask_returns_cancelled_error_when_registry_fires() {
        let tmp = tempdir().expect("tmpdir");
        let registry = CancelRegistry::new();
        let tool = make_tool_with_slow_provider(HelperApprovalPolicy::Reject, tmp.path())
            .with_cancel_registry(Arc::clone(&registry));
        let mut ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        ctx.rpc_id = Some(zunel_tools::RpcId::String("req-cancel-1".into()));

        // Trigger a cancel shortly after dispatch, while the provider
        // is still inside its 5s sleep.
        let registry_clone = Arc::clone(&registry);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(registry_clone.cancel(&zunel_tools::RpcId::String("req-cancel-1".into())));
        });

        let result = tool.execute(json!({"prompt": "long task"}), &ctx).await;
        assert!(result.is_error, "expected cancel error: {}", result.content);
        assert!(
            result.content.contains("cancelled"),
            "expected cancelled message: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn helper_ask_call_timeout_triggers_structured_error() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool_with_slow_provider(HelperApprovalPolicy::Reject, tmp.path())
            .with_call_timeout(Some(std::time::Duration::from_millis(50)));
        let ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());

        let result = tool.execute(json!({"prompt": "long task"}), &ctx).await;
        assert!(
            result.is_error,
            "expected timeout error: {}",
            result.content
        );
        assert!(
            result.content.contains("timeout"),
            "expected timeout message: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn helper_ask_runs_without_registry_when_no_rpc_id() {
        // No registry / no rpc_id: the tool must still answer normally.
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());
        let ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        let result = tool.execute(json!({"prompt": "hi"}), &ctx).await;
        assert!(!result.is_error);
    }

    #[test]
    fn stream_event_to_progress_payload_shapes() {
        use zunel_providers::{LLMResponse, ToolProgress, Usage};

        // Content delta: forwarded as kind=content + delta.
        let payload = stream_event_to_progress_payload(&StreamEvent::ContentDelta("hi".into()))
            .expect("content delta yields payload");
        assert_eq!(payload["kind"], "content");
        assert_eq!(payload["delta"], "hi");

        // Empty content delta: dropped (no payload).
        assert!(
            stream_event_to_progress_payload(&StreamEvent::ContentDelta(String::new())).is_none()
        );

        // Tool start.
        let payload =
            stream_event_to_progress_payload(&StreamEvent::ToolProgress(ToolProgress::Start {
                index: 0,
                name: "echo".into(),
            }))
            .expect("tool start yields payload");
        assert_eq!(payload["kind"], "tool_progress");
        assert_eq!(payload["stage"], "start");
        assert_eq!(payload["name"], "echo");

        // Tool done.
        let payload =
            stream_event_to_progress_payload(&StreamEvent::ToolProgress(ToolProgress::Done {
                index: 0,
                name: "echo".into(),
                ok: true,
                snippet: "hi".into(),
            }))
            .expect("tool done yields payload");
        assert_eq!(payload["stage"], "done");
        assert_eq!(payload["ok"], true);

        // Done with finish_reason.
        let resp = LLMResponse {
            content: None,
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: Some("stop".into()),
        };
        let payload = stream_event_to_progress_payload(&StreamEvent::Done(resp))
            .expect("done yields payload");
        assert_eq!(payload["kind"], "done");
        assert_eq!(payload["finish_reason"], "stop");

        // Tool-call deltas dropped.
        assert!(
            stream_event_to_progress_payload(&StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_fragment: None,
            })
            .is_none()
        );
    }

    #[tokio::test]
    async fn helper_ask_with_forward_policy_misconfigured_without_queue_returns_error() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Forward, tmp.path());
        let ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        let result = tool.execute(json!({"prompt": "hi"}), &ctx).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("ApprovalQueue"),
            "expected misconfiguration error: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn helper_ask_forwards_progress_when_sink_is_set() {
        let tmp = tempdir().expect("tmpdir");
        let tool = make_tool(HelperApprovalPolicy::Reject, tmp.path());
        let mut ctx =
            ToolContext::new_with_workspace(tmp.path().to_path_buf(), "mcp-agent:test".into());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(32);
        ctx.progress_sink = Some(tx);

        let result = tool.execute(json!({"prompt": "stream me"}), &ctx).await;
        assert!(!result.is_error);

        // FakeProvider's default `generate` impl synthesises one
        // ContentDelta + Done. Drain anything the helper forwarded
        // and assert the shape.
        drop(ctx); // drop our remaining sink ref so rx eventually closes
        let mut payloads = Vec::new();
        while let Ok(Some(payload)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {
            payloads.push(payload);
        }
        // We expect at least one progress event for the content
        // delta (the final response goes back via the tool result,
        // not through the progress channel).
        let has_content = payloads
            .iter()
            .any(|p| p.get("kind").and_then(Value::as_str) == Some("content"));
        assert!(
            has_content,
            "expected a content progress event, saw {payloads:?}"
        );
    }
}
