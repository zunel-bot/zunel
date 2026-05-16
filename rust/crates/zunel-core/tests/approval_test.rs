use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::time::{timeout, Duration};

use zunel_bus::{InboundMessage, MessageBus, MessageKind};
use zunel_core::approval::{
    summarize_tool_call, tool_requires_approval, ApprovalDecision, ApprovalHandler,
    ApprovalRequest, ApprovalScope, CachedApprovalHandler,
};
use zunel_core::BusApprovalHandler;

struct FakeHandler {
    answers: Arc<Mutex<Vec<ApprovalDecision>>>,
    calls: Arc<Mutex<Vec<ApprovalRequest>>>,
}

#[async_trait]
impl ApprovalHandler for FakeHandler {
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
        self.calls.lock().unwrap().push(req);
        self.answers
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(ApprovalDecision::Deny)
    }
}

#[tokio::test]
async fn summarize_tool_call_is_stable_across_arg_orders() {
    let a = summarize_tool_call("exec", &json!({"command": "ls", "timeout": 60}));
    let b = summarize_tool_call("exec", &json!({"timeout": 60, "command": "ls"}));
    assert_eq!(a, b);
}

#[test]
fn tool_requires_approval_respects_scope() {
    assert!(tool_requires_approval("exec", ApprovalScope::Shell));
    assert!(!tool_requires_approval("read_file", ApprovalScope::Shell));
    assert!(tool_requires_approval("write_file", ApprovalScope::Writes));
    assert!(!tool_requires_approval("read_file", ApprovalScope::Writes));
    assert!(tool_requires_approval("read_file", ApprovalScope::All));
}

#[tokio::test]
async fn cached_handler_only_prompts_once_per_tool_call_signature() {
    let inner = FakeHandler {
        answers: Arc::new(Mutex::new(vec![
            ApprovalDecision::Approve,
            ApprovalDecision::Approve,
        ])),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let calls = inner.calls.clone();
    let cached = CachedApprovalHandler::new(Arc::new(inner));
    let req1 = ApprovalRequest {
        tool_name: "exec".into(),
        args: json!({"command": "ls"}),
        description: "list".into(),
        scope: ApprovalScope::Shell,
    };
    let req2 = req1.clone();

    assert!(matches!(
        cached.request(req1).await,
        ApprovalDecision::Approve
    ));
    assert!(matches!(
        cached.request(req2).await,
        ApprovalDecision::Approve
    ));

    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn bus_approval_handler_round_trips_prompt_and_response() {
    let bus = Arc::new(MessageBus::new(8));
    let handler = BusApprovalHandler::new(bus.clone(), "slack:C123:T456".into());

    let request = ApprovalRequest {
        tool_name: "exec".into(),
        args: json!({"cmd": "rm -rf /tmp/nope"}),
        description: "Run exec".into(),
        scope: ApprovalScope::Shell,
    };
    let task = tokio::spawn(async move { handler.request(request).await });

    let outbound = bus.next_outbound().await.unwrap();
    assert_eq!(outbound.channel, "slack");
    assert_eq!(outbound.chat_id, "C123:T456");
    assert_eq!(outbound.kind, MessageKind::Approval);
    assert!(outbound.content.contains("exec"));
    let request_id = outbound.message_id.clone().unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "C123:T456".into(),
        user_id: Some("U123".into()),
        content: "approve:wrong".into(),
        media: Vec::new(),
        kind: MessageKind::ApprovalResponse,
    })
    .await
    .unwrap();
    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "C123:T456".into(),
        user_id: Some("U123".into()),
        content: format!("approve:{request_id}"),
        media: Vec::new(),
        kind: MessageKind::ApprovalResponse,
    })
    .await
    .unwrap();

    assert_eq!(task.await.unwrap(), ApprovalDecision::Approve);
}

#[tokio::test]
async fn bus_approval_handler_uses_unique_request_ids_for_same_tool_call() {
    let bus = Arc::new(MessageBus::new(8));
    let request = ApprovalRequest {
        tool_name: "exec".into(),
        args: json!({"cmd": "same"}),
        description: "Run exec".into(),
        scope: ApprovalScope::Shell,
    };

    let first_handler = BusApprovalHandler::new(bus.clone(), "slack:C123".into());
    let first_req = request.clone();
    let first_task = tokio::spawn(async move { first_handler.request(first_req).await });
    let first = bus.next_outbound().await.unwrap();
    let first_id = first.message_id.clone().unwrap();
    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "C123".into(),
        user_id: Some("U123".into()),
        content: format!("approve:{first_id}"),
        media: Vec::new(),
        kind: MessageKind::ApprovalResponse,
    })
    .await
    .unwrap();
    assert_eq!(first_task.await.unwrap(), ApprovalDecision::Approve);

    let second_handler = BusApprovalHandler::new(bus.clone(), "slack:C123".into());
    let second_task = tokio::spawn(async move { second_handler.request(request).await });
    let second = bus.next_outbound().await.unwrap();
    let second_id = second.message_id.clone().unwrap();
    assert_ne!(first_id, second_id);
    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "C123".into(),
        user_id: Some("U123".into()),
        content: format!("approve:{second_id}"),
        media: Vec::new(),
        kind: MessageKind::ApprovalResponse,
    })
    .await
    .unwrap();
    assert_eq!(second_task.await.unwrap(), ApprovalDecision::Approve);
}

#[tokio::test]
async fn bus_approval_handler_preserves_unrelated_inbound_messages() {
    let bus = Arc::new(MessageBus::new(8));
    let handler = BusApprovalHandler::new(bus.clone(), "slack:C123".into());
    let request = ApprovalRequest {
        tool_name: "exec".into(),
        args: json!({"cmd": "same"}),
        description: "Run exec".into(),
        scope: ApprovalScope::Shell,
    };
    let task = tokio::spawn(async move { handler.request(request).await });
    let approval = bus.next_outbound().await.unwrap();
    let request_id = approval.message_id.clone().unwrap();

    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "D456".into(),
        user_id: Some("U999".into()),
        content: "unrelated user message".into(),
        media: Vec::new(),
        kind: MessageKind::User,
    })
    .await
    .unwrap();
    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "C123".into(),
        user_id: Some("U123".into()),
        content: format!("approve:{request_id}"),
        media: Vec::new(),
        kind: MessageKind::ApprovalResponse,
    })
    .await
    .unwrap();

    assert_eq!(task.await.unwrap(), ApprovalDecision::Approve);
    let preserved = timeout(Duration::from_secs(1), bus.next_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preserved.chat_id, "D456");
    assert_eq!(preserved.content, "unrelated user message");
}
