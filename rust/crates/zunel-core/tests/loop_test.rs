use std::sync::Arc;

use async_trait::async_trait;
use zunel_config::AgentDefaults;
use zunel_core::AgentLoop;
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, ToolSchema, Usage,
};

struct FakeProvider {
    reply: String,
}

#[async_trait]
impl LLMProvider for FakeProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        Ok(LLMResponse {
            content: Some(self.reply.clone()),
            tool_calls: Vec::new(),
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                cached_tokens: 0,
                reasoning_tokens: 0,
            },
            finish_reason: None,
        })
    }
}

#[tokio::test]
async fn process_direct_returns_provider_content() {
    let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider {
        reply: "pong".into(),
    });
    let defaults = AgentDefaults {
        provider: Some("custom".into()),
        model: "gpt-x".into(),
        ..Default::default()
    };
    let agent_loop = AgentLoop::new(provider, defaults);
    let result = agent_loop.process_direct("ping").await.unwrap();
    assert_eq!(result.content, "pong");
    assert!(result.tools_used.is_empty());
}

#[tokio::test]
async fn empty_provider_content_becomes_empty_string() {
    struct EmptyProvider;
    #[async_trait::async_trait]
    impl LLMProvider for EmptyProvider {
        async fn generate(
            &self,
            _model: &str,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _settings: &GenerationSettings,
        ) -> zunel_providers::Result<LLMResponse> {
            Ok(LLMResponse {
                content: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: None,
            })
        }
    }

    let provider: Arc<dyn LLMProvider> = Arc::new(EmptyProvider);
    let defaults = AgentDefaults {
        model: "m".into(),
        ..Default::default()
    };
    let agent_loop = AgentLoop::new(provider, defaults);
    let result = agent_loop.process_direct("hi").await.unwrap();
    assert_eq!(result.content, "");
}
