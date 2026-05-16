use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Chat role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A message sent to an LLM provider. Currently plain-text content
/// only; multipart content (images, documents) is not yet supported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Populated on `role == "assistant"` turns that emit tool calls.
    /// Empty for text-only turns and for tool / user / system roles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRequest>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
    /// Assistant message carrying tool calls. Content may be empty; the
    /// wire serializer emits `content: null` when `tool_calls` is
    /// non-empty to keep the on-disk JSONL stable.
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCallRequest>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls,
        }
    }
    /// Tool-result message correlating with an earlier assistant tool call.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: Vec::new(),
        }
    }
}

/// OpenAI-style tool call, reassembled from SSE deltas or emitted
/// whole by non-streaming responses. The `arguments` value is the
/// *parsed* JSON object (kept as `serde_json::Value` so callers
/// don't re-parse on each access).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// Provider-supplied opaque ID, e.g. `"call_abc"`.
    pub id: String,
    /// Tool name, e.g. `"read_file"`. Matches `function.name`.
    pub name: String,
    /// Parsed `function.arguments`. Always an object at dispatch time.
    pub arguments: serde_json::Value,
    /// Assistant-message index. Providers may emit multiple tool
    /// calls within a single response; `index` preserves their order.
    #[serde(default)]
    pub index: u32,
}

/// One chunk of a streamed tool call. Multiple `ToolCallDelta` events
/// with the same `index` combine into a single `ToolCallRequest` once
/// `ToolCallAccumulator::finalize` runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallDelta {
    pub index: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments_fragment: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default)]
    pub cached_tokens: u32,
    /// Reasoning tokens billed separately by reasoning-capable models
    /// (Codex Responses, gpt-5.x reasoning effort). Always 0 for plain
    /// chat-completion endpoints.
    #[serde(default)]
    pub reasoning_tokens: u32,
}

impl Usage {
    /// Total token count across input + output + reasoning. Cached is
    /// already a subset of input/prompt tokens (provider-side discount
    /// signal), so it is excluded to avoid double-counting.
    pub fn total(&self) -> u64 {
        self.prompt_tokens as u64 + self.completion_tokens as u64 + self.reasoning_tokens as u64
    }

    /// Saturating in-place add. Used by the runner to aggregate per-iteration
    /// usage into a per-turn total without panicking on absurd values.
    pub fn add_assign(&mut self, other: &Usage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(other.cached_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }
}

impl std::ops::AddAssign<&Usage> for Usage {
    fn add_assign(&mut self, rhs: &Usage) {
        Usage::add_assign(self, rhs);
    }
}

#[derive(Debug, Clone)]
pub struct LLMResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallRequest>,
    pub usage: Usage,
    /// Terminal-stream / response `finish_reason`: "stop", "length",
    /// "tool_calls", "content_filter", or `None` when the provider
    /// omitted it. The agent runner reads this to decide whether
    /// to continue the tool loop or retry with a larger token cap.
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GenerationSettings {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
}

/// Minimal tool schema type — slice 1 always passes an empty slice.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A single frame of a streaming response.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Incremental assistant text.
    ContentDelta(String),
    /// Partial tool call fragment. Consumers must pass these through
    /// `ToolCallAccumulator` to materialize executable
    /// `ToolCallRequest`s — a single logical call is often split
    /// across many deltas (typically id+name in the first, JSON
    /// arguments streamed across the rest).
    ToolCallDelta {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments_fragment: Option<String>,
    },
    /// Synthetic event emitted by the agent runner (not the SSE
    /// parser) when a tool starts/finishes executing. Used by the CLI
    /// renderer to print `[tool: name → ok]` style progress lines
    /// without re-implementing tool-call accumulation in the renderer.
    ToolProgress(ToolProgress),
    /// Terminal event carrying the complete response (content, tool calls,
    /// usage, finish_reason). Producers must emit exactly one `Done`
    /// per stream.
    Done(LLMResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolProgress {
    Start {
        index: u32,
        name: String,
    },
    Done {
        index: u32,
        name: String,
        ok: bool,
        snippet: String,
    },
}

#[async_trait]
pub trait LLMProvider: Send + Sync {
    /// Generate a single non-streaming completion.
    async fn generate(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        settings: &GenerationSettings,
    ) -> Result<LLMResponse>;

    /// Generate a streaming completion. Default impl synthesizes a single
    /// `ContentDelta` + `Done` from `generate()` — override for true
    /// token-by-token streaming.
    fn generate_stream<'a>(
        &'a self,
        model: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSchema],
        settings: &'a GenerationSettings,
    ) -> BoxStream<'a, Result<StreamEvent>> {
        Box::pin(async_stream::try_stream! {
            let response = self.generate(model, messages, tools, settings).await?;
            if let Some(ref content) = response.content {
                if !content.is_empty() {
                    yield StreamEvent::ContentDelta(content.clone());
                }
            }
            yield StreamEvent::Done(response);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::StreamExt;

    struct Constant(String);

    #[async_trait]
    impl LLMProvider for Constant {
        async fn generate(
            &self,
            _model: &str,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _settings: &GenerationSettings,
        ) -> Result<LLMResponse> {
            Ok(LLMResponse {
                content: Some(self.0.clone()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn default_generate_stream_yields_delta_then_done() {
        let provider = Constant("hello".into());
        let messages = [ChatMessage::user("hi")];
        let tools: [ToolSchema; 0] = [];
        let settings = GenerationSettings::default();
        let stream = provider.generate_stream("m", &messages, &tools, &settings);
        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 2);
        match &events[0] {
            Ok(StreamEvent::ContentDelta(s)) => assert_eq!(s, "hello"),
            other => panic!("expected ContentDelta, got {other:?}"),
        }
        match &events[1] {
            Ok(StreamEvent::Done(resp)) => {
                assert_eq!(resp.content.as_deref(), Some("hello"));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_content_skips_delta_but_emits_done() {
        struct Empty;
        #[async_trait]
        impl LLMProvider for Empty {
            async fn generate(
                &self,
                _model: &str,
                _messages: &[ChatMessage],
                _tools: &[ToolSchema],
                _settings: &GenerationSettings,
            ) -> Result<LLMResponse> {
                Ok(LLMResponse {
                    content: None,
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    finish_reason: None,
                })
            }
        }
        let provider = Empty;
        let messages = [ChatMessage::user("hi")];
        let tools: [ToolSchema; 0] = [];
        let settings = GenerationSettings::default();
        let stream = provider.generate_stream("m", &messages, &tools, &settings);
        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Ok(StreamEvent::Done(_))));
    }
}
