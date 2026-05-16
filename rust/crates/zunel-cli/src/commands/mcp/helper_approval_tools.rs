//! Mode 2 approval-forwarding tools (`helper_pending_approvals` and
//! `helper_approve`). Registered alongside `helper_ask` only when
//! `--mode2 --mode2-approval forward` is set; under any other policy
//! the tools are absent from the registry so the helper's filtered
//! surface stays unchanged.
//!
//! Both tools share the same [`ApprovalQueue`] the
//! [`QueueApprovalHandler`] uses inside the helper's `AgentLoop` —
//! that's how a poll-resolve round trip closes the loop without
//! introducing a new transport-level concept.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use zunel_core::ApprovalDecision;
use zunel_tools::{Tool, ToolContext, ToolResult};

use super::approval_queue::ApprovalQueue;

/// `helper_pending_approvals` MCP tool — read-only snapshot of every
/// approval currently waiting for a hub decision. The hub polls this
/// (with whatever cadence makes sense for its UX) and surfaces the
/// entries to a human.
pub struct HelperPendingApprovalsTool {
    queue: Arc<ApprovalQueue>,
}

impl HelperPendingApprovalsTool {
    pub fn new(queue: Arc<ApprovalQueue>) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl Tool for HelperPendingApprovalsTool {
    fn name(&self) -> &'static str {
        "helper_pending_approvals"
    }

    fn description(&self) -> &'static str {
        "Snapshot of every approval request currently waiting for the hub \
         to decide. Each entry carries an id (use it with helper_approve), \
         the tool name and arguments, a human-readable description, and \
         the configured approval scope."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> ToolResult {
        let entries = self.queue.snapshot();
        let count = entries.len();
        let body =
            serde_json::to_string(&json!({"approvals": entries})).unwrap_or_else(|_| "{}".into());
        ToolResult::ok(body).with_meta(json!({"count": count}))
    }
}

/// `helper_approve` MCP tool — submit a decision for one outstanding
/// approval. The hub takes an id from the most-recent
/// `helper_pending_approvals` snapshot and posts `approve` or
/// `deny`; the matching `QueueApprovalHandler::request` future
/// resolves with that decision.
pub struct HelperApproveTool {
    queue: Arc<ApprovalQueue>,
}

impl HelperApproveTool {
    pub fn new(queue: Arc<ApprovalQueue>) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl Tool for HelperApproveTool {
    fn name(&self) -> &'static str {
        "helper_approve"
    }

    fn description(&self) -> &'static str {
        "Resolve an outstanding approval request from the queue with \
         either `approve` or `deny`. The id comes from the most recent \
         helper_pending_approvals snapshot. Unknown ids return an error \
         (rather than a silent no-op) so the hub can surface a stale \
         pointer immediately."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["id", "decision"],
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Approval id from helper_pending_approvals."
                },
                "decision": {
                    "type": "string",
                    "enum": ["approve", "deny"],
                    "description": "Whether to approve or deny the gated tool call."
                }
            }
        })
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolResult::err("helper_approve: missing required `id`");
        };
        let Some(decision_str) = args.get("decision").and_then(Value::as_str) else {
            return ToolResult::err(
                "helper_approve: missing required `decision` (`approve` or `deny`)",
            );
        };
        let decision = match decision_str {
            "approve" => ApprovalDecision::Approve,
            "deny" => ApprovalDecision::Deny,
            other => {
                return ToolResult::err(format!(
                    "helper_approve: unknown decision {other:?}; expected 'approve' or 'deny'"
                ));
            }
        };
        match self.queue.resolve(id, decision) {
            Ok(()) => ToolResult::ok(format!("resolved {id}: {decision_str}")),
            Err(err) => ToolResult::err(format!("helper_approve: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn pending_approvals_returns_empty_when_queue_is_empty() {
        let queue = ApprovalQueue::new();
        let tool = HelperPendingApprovalsTool::new(Arc::clone(&queue));
        let tmp = tempdir().unwrap();
        let ctx = ToolContext::new_with_workspace(tmp.path().to_path_buf(), "k".into());
        let result = tool.execute(json!({}), &ctx).await;
        assert!(!result.is_error);
        let body: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(body["approvals"], json!([]));
        assert_eq!(result.meta.unwrap()["count"], 0);
    }

    #[tokio::test]
    async fn pending_approvals_serialises_each_entry() {
        use zunel_core::ApprovalHandler;
        let queue = ApprovalQueue::new();
        let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
        // Reach in via the ApprovalQueue's enqueue path. Since
        // `enqueue` is module-private, we drive it via a dummy
        // QueueApprovalHandler call that we deliberately don't await.
        let handler = super::super::approval_queue::QueueApprovalHandler::new(
            Arc::clone(&queue),
            std::time::Duration::from_secs(60),
        );
        let req = zunel_core::ApprovalRequest {
            tool_name: "exec".into(),
            args: json!({"cmd": "ls"}),
            description: "list files".into(),
            scope: zunel_core::ApprovalScope::Shell,
        };
        // Fire-and-forget: the resolver below will close the
        // request, but we want to assert the snapshot mid-flight.
        let queue_clone = Arc::clone(&queue);
        let task = tokio::spawn(async move { handler.request(req).await });
        // Wait until the entry lands.
        for _ in 0..100 {
            if !queue_clone.snapshot().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let pending = HelperPendingApprovalsTool::new(Arc::clone(&queue));
        let tmp = tempdir().unwrap();
        let ctx = ToolContext::new_with_workspace(tmp.path().to_path_buf(), "k".into());
        let result = pending.execute(json!({}), &ctx).await;
        let body: Value = serde_json::from_str(&result.content).unwrap();
        let entries = body["approvals"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["tool_name"], "exec");
        assert_eq!(entries[0]["scope"], "Shell");

        // Resolve it so the spawned task doesn't dangle until the
        // 60s timeout.
        let id = entries[0]["id"].as_str().unwrap();
        queue_clone.resolve(id, ApprovalDecision::Deny).unwrap();
        let _ = task.await;
        // Quiet the unused tx warning.
        drop(tx);
    }

    #[tokio::test]
    async fn approve_with_unknown_id_returns_error() {
        let queue = ApprovalQueue::new();
        let tool = HelperApproveTool::new(Arc::clone(&queue));
        let tmp = tempdir().unwrap();
        let ctx = ToolContext::new_with_workspace(tmp.path().to_path_buf(), "k".into());
        let result = tool
            .execute(json!({"id": "nope", "decision": "approve"}), &ctx)
            .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("no pending approval"),
            "expected error message: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn approve_with_invalid_decision_returns_error() {
        let queue = ApprovalQueue::new();
        let tool = HelperApproveTool::new(Arc::clone(&queue));
        let tmp = tempdir().unwrap();
        let ctx = ToolContext::new_with_workspace(tmp.path().to_path_buf(), "k".into());
        let result = tool
            .execute(json!({"id": "x", "decision": "maybe"}), &ctx)
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown decision"));
    }
}
