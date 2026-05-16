use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::mpsc;
use zunel_core::{AgentRunSpec, AgentRunner};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema, Usage,
};
use zunel_tools::ToolRegistry;

struct CapturingProvider {
    settings: Arc<Mutex<Vec<GenerationSettings>>>,
}

#[async_trait]
impl LLMProvider for CapturingProvider {
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
        settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        self.settings.lock().unwrap().push(settings.clone());
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::Done(LLMResponse {
                content: Some("ok".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: None,
            }));
        })
    }
}

#[tokio::test]
async fn runner_uses_generation_settings_from_spec() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(CapturingProvider {
        settings: captured.clone(),
    });
    let runner = AgentRunner::new(
        provider,
        Arc::new(ToolRegistry::new()),
        Arc::new(zunel_core::AllowAllApprovalHandler),
    );
    let (tx, _rx) = mpsc::channel(8);

    runner
        .run(
            AgentRunSpec {
                initial_messages: vec![ChatMessage::user("hi")],
                model: "m".into(),
                settings: GenerationSettings {
                    temperature: Some(0.7),
                    max_tokens: Some(1234),
                    reasoning_effort: Some("high".into()),
                },
                ..Default::default()
            },
            tx,
        )
        .await
        .unwrap();

    let settings = captured.lock().unwrap();
    assert_eq!(settings[0].temperature, Some(0.7));
    assert_eq!(settings[0].max_tokens, Some(1234));
    assert_eq!(settings[0].reasoning_effort.as_deref(), Some("high"));
}
