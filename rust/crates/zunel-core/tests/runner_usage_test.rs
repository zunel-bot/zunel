//! Verifies `AgentRunner::run` aggregates `Usage` across iterations and
//! exposes the running total on `AgentRunResult.usage`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::json;
use tokio::sync::mpsc;
use zunel_core::{AgentRunSpec, AgentRunner};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolCallRequest,
    ToolSchema, Usage,
};
use zunel_tools::{Tool, ToolContext, ToolRegistry, ToolResult};

/// Provider that emits a tool call on the first iteration and a plain
/// text reply on the second, with distinct `Usage` numbers each time
/// so the runner aggregation is testable.
struct TwoIterationProvider {
    iteration: Mutex<u32>,
}

#[async_trait]
impl LLMProvider for TwoIterationProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        unreachable!("runner uses streaming")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        let iter = {
            let mut guard = self.iteration.lock().unwrap();
            let v = *guard;
            *guard += 1;
            v
        };
        Box::pin(async_stream::stream! {
            if iter == 0 {
                // Emit a tool call via delta events; the runner's
                // `ToolCallAccumulator` only assembles calls from
                // `ToolCallDelta` events (mirrors live provider streams).
                yield Ok(StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("echo".into()),
                    arguments_fragment: Some("{\"x\":1}".into()),
                });
                yield Ok(StreamEvent::Done(LLMResponse {
                    content: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "call_1".into(),
                        name: "echo".into(),
                        arguments: json!({"x": 1}),
                        index: 0,
                    }],
                    usage: Usage {
                        prompt_tokens: 100,
                        completion_tokens: 20,
                        cached_tokens: 5,
                        reasoning_tokens: 7,
                    },
                    finish_reason: Some("tool_use".into()),
                }));
            } else {
                yield Ok(StreamEvent::ContentDelta("done".into()));
                yield Ok(StreamEvent::Done(LLMResponse {
                    content: Some("done".into()),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        prompt_tokens: 50,
                        completion_tokens: 30,
                        cached_tokens: 0,
                        reasoning_tokens: 0,
                    },
                    finish_reason: Some("stop".into()),
                }));
            }
        })
    }
}

struct EchoTool {
    invocations: Arc<Mutex<u32>>,
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echo"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        *self.invocations.lock().unwrap() += 1;
        ToolResult::ok("echoed")
    }
}

#[tokio::test]
async fn runner_sums_usage_across_iterations() {
    let provider: Arc<dyn LLMProvider> = Arc::new(TwoIterationProvider {
        iteration: Mutex::new(0),
    });
    let mut tools = ToolRegistry::new();
    let invocations = Arc::new(Mutex::new(0u32));
    tools.register(Arc::new(EchoTool {
        invocations: invocations.clone(),
    }));
    let runner = AgentRunner::new(
        provider,
        Arc::new(tools),
        Arc::new(zunel_core::AllowAllApprovalHandler),
    );
    let (tx, _rx) = mpsc::channel(8);

    let result = runner
        .run(
            AgentRunSpec {
                initial_messages: vec![ChatMessage::user("hi")],
                model: "m".into(),
                ..Default::default()
            },
            tx,
        )
        .await
        .unwrap();

    assert_eq!(
        *invocations.lock().unwrap(),
        1,
        "echo tool ran exactly once"
    );
    assert_eq!(result.usage.prompt_tokens, 150);
    assert_eq!(result.usage.completion_tokens, 50);
    assert_eq!(result.usage.cached_tokens, 5);
    assert_eq!(result.usage.reasoning_tokens, 7);
}
