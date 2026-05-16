use std::sync::Arc;

use async_trait::async_trait;
use zunel_heartbeat::HeartbeatService;
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, ToolSchema, Usage,
};

struct DecisionProvider {
    reply: &'static str,
}

#[async_trait]
impl LLMProvider for DecisionProvider {
    async fn generate(
        &self,
        _model: &str,
        messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        assert!(messages
            .iter()
            .any(|message| message.content.contains("HEARTBEAT.md")));
        Ok(LLMResponse {
            content: Some(self.reply.into()),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: None,
        })
    }
}

#[tokio::test]
async fn trigger_now_runs_when_provider_reports_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("HEARTBEAT.md"), "- follow up with user").unwrap();
    let provider: Arc<dyn LLMProvider> = Arc::new(DecisionProvider {
        reply: "run: follow up with user",
    });
    let service = HeartbeatService::new(tmp.path().to_path_buf(), provider, "m".into());

    let decision = service.trigger_now().await.unwrap();

    assert_eq!(decision.as_deref(), Some("follow up with user"));
}

#[tokio::test]
async fn trigger_now_skips_when_file_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let provider: Arc<dyn LLMProvider> = Arc::new(DecisionProvider { reply: "run: nope" });
    let service = HeartbeatService::new(tmp.path().to_path_buf(), provider, "m".into());

    assert!(service.trigger_now().await.unwrap().is_none());
}
