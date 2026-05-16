use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use zunel_core::SubagentManager;
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema, Usage,
};
use zunel_tools::spawn::SpawnHandle;

struct FinalProvider;

#[async_trait]
impl LLMProvider for FinalProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        unreachable!("runner only calls generate_stream")
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        Box::pin(async_stream::try_stream! {
            yield StreamEvent::ContentDelta("child result".into());
            yield StreamEvent::Done(LLMResponse {
                content: Some("child result".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: Some("stop".into()),
            });
        })
    }
}

#[tokio::test]
async fn subagent_manager_spawns_isolated_child_and_records_result() {
    let manager = SubagentManager::new(Arc::new(FinalProvider), std::env::temp_dir(), "m".into());

    let message = manager
        .spawn("summarize".into(), Some("demo".into()))
        .await
        .unwrap();
    assert!(message.contains("Subagent [demo] started"));
    assert!(message.contains("self tool"));
    let id = message
        .split("id: ")
        .nth(1)
        .unwrap()
        .trim_end_matches("). Use the self tool to inspect status and results.");

    for _ in 0..50 {
        if manager
            .status(id)
            .and_then(|status| status.result)
            .is_some()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let status = manager.status(id).unwrap();
    assert_eq!(status.phase, "done");
    assert_eq!(status.result.as_deref(), Some("child result"));
    assert_eq!(status.label, "demo");
}
