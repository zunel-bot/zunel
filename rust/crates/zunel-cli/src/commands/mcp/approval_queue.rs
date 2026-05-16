//! Polling-based approval forwarding for Mode 2's `helper_ask`.
//!
//! The MCP transport is request-response only — there's no built-in
//! way for the helper to *push* an approval prompt back to the hub
//! and wait for a human's reply. So we expose two new tools:
//!
//!   * `helper_pending_approvals` — the hub polls this and gets a JSON
//!     snapshot of every approval request currently waiting for a
//!     decision (id, tool name, args, description).
//!   * `helper_approve` — the hub posts a decision (`approve` or
//!     `deny`) keyed by the approval id from the snapshot.
//!
//! Internally the queue is a tiny `HashMap<String, PendingApproval>`
//! behind a `Mutex`. Each pending entry holds a `oneshot::Sender`
//! for the decision; submitting a decision sends through it and the
//! `QueueApprovalHandler` (which runs inside the helper's
//! `AgentLoop`) wakes up. A per-approval timeout flips an entry to
//! "deny" if the hub never resolves it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use zunel_core::{ApprovalDecision, ApprovalHandler, ApprovalRequest};

/// Atomic counter used to derive monotonic approval ids. Cheap, no
/// crate dependency, and stable across the lifetime of the process.
fn next_approval_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("appr-{nanos:016x}-{seq:08x}")
}

struct PendingApproval {
    request: ApprovalRequest,
    decider: oneshot::Sender<ApprovalDecision>,
}

/// Shared, threadsafe queue of pending approvals.
#[derive(Default)]
pub struct ApprovalQueue {
    inner: Mutex<HashMap<String, PendingApproval>>,
}

impl ApprovalQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot the queue as a JSON array suitable for
    /// `helper_pending_approvals` to return. Each entry includes the
    /// approval id, tool name, args, description, and scope. Stable
    /// ordering — sorted by id (which is monotonic) so the hub can
    /// rely on a deterministic poll order.
    pub fn snapshot(&self) -> Vec<Value> {
        let map = self.inner.lock().expect("approval queue mutex");
        let mut entries: Vec<(&String, &PendingApproval)> = map.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        entries
            .into_iter()
            .map(|(id, pending)| {
                json!({
                    "id": id,
                    "tool_name": pending.request.tool_name,
                    "args": pending.request.args,
                    "description": pending.request.description,
                    "scope": format!("{:?}", pending.request.scope),
                })
            })
            .collect()
    }

    /// Submit a decision for an outstanding approval. Returns
    /// `Ok(())` when the lookup found the id, `Err(...)` otherwise so
    /// `helper_approve` can surface a structured error to the hub.
    pub fn resolve(&self, id: &str, decision: ApprovalDecision) -> Result<(), ApprovalQueueError> {
        let mut map = self.inner.lock().expect("approval queue mutex");
        let Some(pending) = map.remove(id) else {
            return Err(ApprovalQueueError::UnknownId(id.to_string()));
        };
        let _ = pending.decider.send(decision);
        Ok(())
    }

    fn enqueue(
        &self,
        request: ApprovalRequest,
        decider: oneshot::Sender<ApprovalDecision>,
    ) -> String {
        let id = next_approval_id();
        let mut map = self.inner.lock().expect("approval queue mutex");
        map.insert(id.clone(), PendingApproval { request, decider });
        id
    }

    fn cancel(&self, id: &str) {
        let mut map = self.inner.lock().expect("approval queue mutex");
        map.remove(id);
    }
}

#[derive(Debug)]
pub enum ApprovalQueueError {
    UnknownId(String),
}

impl std::fmt::Display for ApprovalQueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownId(id) => write!(f, "no pending approval with id {id:?}"),
        }
    }
}

impl std::error::Error for ApprovalQueueError {}

/// `ApprovalHandler` impl that funnels every request through an
/// [`ApprovalQueue`]. Used by Mode 2 when `--mode2-approval forward`
/// is set.
pub struct QueueApprovalHandler {
    queue: Arc<ApprovalQueue>,
    timeout: Duration,
}

impl QueueApprovalHandler {
    pub fn new(queue: Arc<ApprovalQueue>, timeout: Duration) -> Self {
        Self { queue, timeout }
    }
}

#[async_trait]
impl ApprovalHandler for QueueApprovalHandler {
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        let id = self.queue.enqueue(req, tx);
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => {
                // Sender dropped without sending. Treat as deny.
                ApprovalDecision::Deny
            }
            Err(_) => {
                // Wallclock timeout — clean up the queue entry so a
                // late `helper_approve` doesn't see a stale id.
                self.queue.cancel(&id);
                ApprovalDecision::Deny
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool_name: tool.into(),
            args: json!({"k": "v"}),
            description: "t".into(),
            scope: zunel_core::ApprovalScope::All,
        }
    }

    #[tokio::test]
    async fn queue_round_trip_approves() {
        let q = ApprovalQueue::new();
        let handler = QueueApprovalHandler::new(Arc::clone(&q), Duration::from_secs(5));
        let q_for_resolver = Arc::clone(&q);
        let resolver = tokio::spawn(async move {
            // Wait for the request to land.
            for _ in 0..100 {
                let snapshot = q_for_resolver.snapshot();
                if let Some(entry) = snapshot.first() {
                    let id = entry["id"].as_str().unwrap().to_string();
                    q_for_resolver
                        .resolve(&id, ApprovalDecision::Approve)
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("approval never enqueued");
        });

        let decision = handler.request(req("exec")).await;
        resolver.await.unwrap();
        assert_eq!(decision, ApprovalDecision::Approve);
        assert!(q.snapshot().is_empty(), "entry removed after resolve");
    }

    #[tokio::test]
    async fn queue_round_trip_denies() {
        let q = ApprovalQueue::new();
        let handler = QueueApprovalHandler::new(Arc::clone(&q), Duration::from_secs(5));
        let q_for_resolver = Arc::clone(&q);
        let resolver = tokio::spawn(async move {
            for _ in 0..100 {
                let snapshot = q_for_resolver.snapshot();
                if let Some(entry) = snapshot.first() {
                    let id = entry["id"].as_str().unwrap().to_string();
                    q_for_resolver.resolve(&id, ApprovalDecision::Deny).unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("approval never enqueued");
        });

        let decision = handler.request(req("write_file")).await;
        resolver.await.unwrap();
        assert_eq!(decision, ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn queue_timeout_returns_deny() {
        let q = ApprovalQueue::new();
        let handler = QueueApprovalHandler::new(Arc::clone(&q), Duration::from_millis(50));
        let decision = handler.request(req("exec")).await;
        assert_eq!(decision, ApprovalDecision::Deny);
        assert!(
            q.snapshot().is_empty(),
            "timed-out entry must be cleaned up"
        );
    }

    #[test]
    fn resolve_unknown_id_errors() {
        let q = ApprovalQueue::new();
        let err = q.resolve("nope", ApprovalDecision::Approve).unwrap_err();
        match err {
            ApprovalQueueError::UnknownId(id) => assert_eq!(id, "nope"),
        }
    }

    #[test]
    fn snapshot_contains_request_metadata() {
        let q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        let id = q.enqueue(req("exec"), tx);
        let snapshot = q.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0]["id"], id);
        assert_eq!(snapshot[0]["tool_name"], "exec");
        assert_eq!(snapshot[0]["args"]["k"], "v");
    }
}
