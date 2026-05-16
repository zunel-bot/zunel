use serde_json::json;
use std::io::Cursor;
use std::time::Duration;

use zunel_cli::approval_cli::StdinApprovalHandler;
use zunel_core::{ApprovalDecision, ApprovalHandler, ApprovalRequest, ApprovalScope};

fn req(scope: ApprovalScope) -> ApprovalRequest {
    ApprovalRequest {
        tool_name: "exec".into(),
        args: json!({"command": "echo hi"}),
        description: "run shell".into(),
        scope,
    }
}

#[tokio::test]
async fn empty_input_means_deny() {
    let handler = StdinApprovalHandler::with_reader(Cursor::new(Vec::<u8>::new()))
        .with_timeout(Duration::from_millis(100));
    let decision = handler.request(req(ApprovalScope::Shell)).await;
    assert!(matches!(decision, ApprovalDecision::Deny));
}

#[tokio::test]
async fn yes_means_approve() {
    let handler = StdinApprovalHandler::with_reader(Cursor::new(b"y\n".to_vec()));
    let decision = handler.request(req(ApprovalScope::Shell)).await;
    assert!(matches!(decision, ApprovalDecision::Approve));
}

#[tokio::test]
async fn anything_else_means_deny() {
    let handler = StdinApprovalHandler::with_reader(Cursor::new(b"\n".to_vec()));
    let decision = handler.request(req(ApprovalScope::Shell)).await;
    assert!(matches!(decision, ApprovalDecision::Deny));
}
