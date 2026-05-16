//! Cancellation tests for [`AgentRunner`]. Three opposed cases:
//!
//! 1. token cancelled before the first iteration → runner unwinds
//!    immediately with `Error::Cancelled`, never calls the provider.
//! 2. token cancelled mid-stream → the active provider stream is
//!    aborted on the next event poll and the runner unwinds with
//!    `Error::Cancelled`.
//! 3. baseline: token never cancelled → runner completes normally.
//!
//! These pin the runtime contract `helper_ask` relies on to honour
//! `notifications/cancelled` from the hub.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;
use zunel_core::{
    runner::{AgentRunSpec, AgentRunner, StopReason},
    AllowAllApprovalHandler,
};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema, Usage,
};
use zunel_tools::ToolRegistry;

/// Provider whose stream produces a single Done event, but only after
/// a delay. The delay is long enough that a mid-flight cancel always
/// wins the `tokio::select!` race in the runner.
struct SlowProvider;

#[async_trait]
impl LLMProvider for SlowProvider {
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
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        Box::pin(async_stream::stream! {
            // Long enough that the cancellation triggered after this
            // function returns has time to land before the sleep
            // resolves. 5s is overkill on a modern CI box but keeps
            // the test rock-solid in slow runners (qemu, gha
            // micro-runners).
            tokio::time::sleep(Duration::from_secs(5)).await;
            yield Ok(StreamEvent::ContentDelta("never seen".into()));
            yield Ok(StreamEvent::Done(LLMResponse {
                content: Some("never seen".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: None,
            }));
        })
    }
}

/// Provider that completes promptly so the baseline (no cancel) test
/// has something deterministic to assert against.
struct FastProvider;

#[async_trait]
impl LLMProvider for FastProvider {
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
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::ContentDelta("hi".into()));
            yield Ok(StreamEvent::Done(LLMResponse {
                content: Some("hi".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: Some("stop".into()),
            }));
        })
    }
}

fn baseline_spec(cancel: CancellationToken) -> AgentRunSpec {
    AgentRunSpec {
        initial_messages: vec![ChatMessage::user("go")],
        model: "fake".into(),
        max_iterations: 5,
        cancel,
        ..Default::default()
    }
}

#[tokio::test]
async fn runner_unwinds_immediately_when_cancelled_before_first_iteration() {
    let cancel = CancellationToken::new();
    cancel.cancel(); // already done

    let runner = AgentRunner::new(
        Arc::new(SlowProvider),
        Arc::new(ToolRegistry::new()),
        Arc::new(AllowAllApprovalHandler),
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let err = match runner.run(baseline_spec(cancel), tx).await {
        Ok(_) => panic!("expected Cancelled, got Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, zunel_core::Error::Cancelled),
        "expected Error::Cancelled, got {err:?}"
    );
}

#[tokio::test]
async fn runner_aborts_mid_stream_when_token_fires() {
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    tokio::spawn(async move {
        // Give the runner enough time to call into generate_stream
        // and start awaiting events before we trip the cancel.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_for_task.cancel();
    });

    let runner = AgentRunner::new(
        Arc::new(SlowProvider),
        Arc::new(ToolRegistry::new()),
        Arc::new(AllowAllApprovalHandler),
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let err = match runner.run(baseline_spec(cancel), tx).await {
        Ok(_) => panic!("expected Cancelled, got Ok"),
        Err(e) => e,
    };
    assert!(matches!(err, zunel_core::Error::Cancelled));
}

#[tokio::test]
async fn runner_completes_normally_when_token_never_fires() {
    let cancel = CancellationToken::new();
    let runner = AgentRunner::new(
        Arc::new(FastProvider),
        Arc::new(ToolRegistry::new()),
        Arc::new(AllowAllApprovalHandler),
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let result = runner.run(baseline_spec(cancel), tx).await.unwrap();
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.content, "hi");
}
