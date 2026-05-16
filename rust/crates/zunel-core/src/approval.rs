//! Approval handler trait + per-session cache.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::time::{timeout, Duration};
use zunel_bus::{InboundMessage, MessageBus, MessageKind, OutboundMessage};

static APPROVAL_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalScope {
    #[default]
    All,
    Shell,
    Writes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub args: Value,
    pub description: String,
    pub scope: ApprovalScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision;
}

/// Stable cache key for `(tool, args)`. JSON-keys are sorted so the
/// same tool call produces the same hash regardless of argument
/// iteration order.
pub fn summarize_tool_call(tool: &str, args: &Value) -> String {
    let sorted = sort_json(args);
    let serialized = serde_json::to_string(&sorted).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(tool.as_bytes());
    hasher.update(b":");
    hasher.update(serialized.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn sort_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(String, Value)> =
                map.iter().map(|(k, v)| (k.clone(), sort_json(v))).collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_json).collect()),
        other => other.clone(),
    }
}

/// Given a tool name + desired scope, returns whether running the
/// tool requires user consent.
pub fn tool_requires_approval(tool_name: &str, scope: ApprovalScope) -> bool {
    const SHELL_TOOLS: &[&str] = &["exec"];
    const WRITE_TOOLS: &[&str] = &["exec", "write_file", "edit_file"];
    match scope {
        ApprovalScope::All => true,
        ApprovalScope::Shell => SHELL_TOOLS.contains(&tool_name),
        ApprovalScope::Writes => WRITE_TOOLS.contains(&tool_name),
    }
}

/// Wraps another `ApprovalHandler` with a per-call cache. The cache
/// key is `summarize_tool_call(req.tool_name, &req.args)`.
pub struct CachedApprovalHandler {
    inner: Arc<dyn ApprovalHandler>,
    cache: Mutex<HashMap<String, ApprovalDecision>>,
}

impl CachedApprovalHandler {
    pub fn new(inner: Arc<dyn ApprovalHandler>) -> Self {
        Self {
            inner,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl ApprovalHandler for CachedApprovalHandler {
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
        let key = summarize_tool_call(&req.tool_name, &req.args);
        if let Some(d) = self.cache.lock().unwrap().get(&key).copied() {
            return d;
        }
        let decision = self.inner.request(req).await;
        self.cache.lock().unwrap().insert(key, decision);
        decision
    }
}

/// Always-approve handler (used as the default when approval is off).
pub struct AllowAllApprovalHandler;

#[async_trait]
impl ApprovalHandler for AllowAllApprovalHandler {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}

/// Always-deny handler used when there is no human in the loop.
///
/// This is the default approval policy for Mode 2 (`helper_ask` /
/// agent-loop-as-tool): the helper agent runs inside an MCP request
/// from another machine, so there's no console to prompt and no
/// approver to wait for. Auto-rejecting any approval-gated tool call
/// makes the policy explicit at the boundary — the helper either
/// completes using the allow-listed surface or it surfaces a
/// "denied: no approver available" string back to the caller, which
/// the parent agent can act on.
pub struct RejectAllApprovalHandler;

#[async_trait]
impl ApprovalHandler for RejectAllApprovalHandler {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

pub struct BusApprovalHandler {
    bus: Arc<MessageBus>,
    channel: String,
    chat_id: String,
    timeout: Duration,
}

impl BusApprovalHandler {
    pub fn new(bus: Arc<MessageBus>, session_key: String) -> Self {
        let (channel, chat_id) = session_key
            .split_once(':')
            .map(|(channel, chat_id)| (channel.to_string(), chat_id.to_string()))
            .unwrap_or_else(|| ("cli".into(), session_key));
        Self {
            bus,
            channel,
            chat_id,
            timeout: Duration::from_secs(300),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl ApprovalHandler for BusApprovalHandler {
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
        let request_id = format!(
            "{}:{}",
            summarize_tool_call(&req.tool_name, &req.args),
            next_approval_nonce()
        );
        let prompt = format!(
            "Approval required for tool `{}`: {}\nArgs: {}",
            req.tool_name, req.description, req.args
        );
        if self
            .bus
            .publish_outbound(OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                message_id: Some(request_id.clone()),
                content: prompt,
                media: Vec::new(),
                kind: MessageKind::Approval,
            })
            .await
            .is_err()
        {
            return ApprovalDecision::Deny;
        }

        let wait = async {
            let Some(message) = self
                .bus
                .next_inbound_matching(|message| {
                    matching_approval_response(message, &self.channel, &self.chat_id, &request_id)
                })
                .await
            else {
                return ApprovalDecision::Deny;
            };
            parse_approval_response(&message.content, &request_id)
        };
        timeout(self.timeout, wait)
            .await
            .unwrap_or(ApprovalDecision::Deny)
    }
}

fn next_approval_nonce() -> u64 {
    let counter = APPROVAL_NONCE.fetch_add(1, Ordering::Relaxed);
    let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default() as u64;
    now ^ counter.rotate_left(13)
}

fn matching_approval_response(
    message: &InboundMessage,
    channel: &str,
    chat_id: &str,
    request_id: &str,
) -> bool {
    message.kind == MessageKind::ApprovalResponse
        && message.channel == channel
        && message.chat_id == chat_id
        && message
            .content
            .split_once(':')
            .is_some_and(|(_, id)| id == request_id)
}

fn parse_approval_response(content: &str, request_id: &str) -> ApprovalDecision {
    let Some((decision, id)) = content.trim().split_once(':') else {
        return ApprovalDecision::Deny;
    };
    if id != request_id {
        return ApprovalDecision::Deny;
    }
    match decision.to_ascii_lowercase().as_str() {
        "approve" | "approved" | "yes" | "allow" | "once" | "session" | "always" => {
            ApprovalDecision::Approve
        }
        _ => ApprovalDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req() -> ApprovalRequest {
        ApprovalRequest {
            tool_name: "exec".into(),
            args: json!({"cmd": "ls"}),
            description: "list files".into(),
            scope: ApprovalScope::All,
        }
    }

    #[tokio::test]
    async fn allow_all_handler_approves_everything() {
        let h = AllowAllApprovalHandler;
        assert_eq!(h.request(req()).await, ApprovalDecision::Approve);
    }

    #[tokio::test]
    async fn reject_all_handler_denies_everything() {
        let h = RejectAllApprovalHandler;
        assert_eq!(h.request(req()).await, ApprovalDecision::Deny);
    }
}
