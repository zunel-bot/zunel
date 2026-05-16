//! OpenAI-compatible (`/v1/chat/completions`) provider.
//!
//! Submodules:
//!  * [`wire`] — serde request/response shapes shared by both code paths,
//!    plus the `WireUsage → Usage` flattening of OpenAI's post-2024
//!    `*_tokens_details` sub-objects.
//!  * [`streaming`] — `text/event-stream` decoder built on top of
//!    `SseBuffer` and `ToolCallAccumulator`. Yields `ContentDelta` /
//!    `ToolCallDelta` incrementally and `Done(LLMResponse)` at end.
//!
//! This file owns the [`OpenAICompatProvider`] type and its non-streaming
//! [`LLMProvider::generate`] implementation, including the single 429
//! retry with `Retry-After` honoring.

mod streaming;
mod wire;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::{header, Client};
use tokio::time::sleep;

use crate::base::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema,
};
use crate::error::{Error, Result};

use self::wire::{parse_wire_tool_call, RequestBody, ResponseBody};

/// Provider hitting any OpenAI `chat.completions`-compatible endpoint.
pub struct OpenAICompatProvider {
    pub(super) client: Client,
    pub(super) api_base: String,
}

impl OpenAICompatProvider {
    pub fn new(
        api_key: String,
        api_base: String,
        extra_headers: BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        let bearer = format!("Bearer {api_key}");
        let mut auth = header::HeaderValue::from_str(&bearer)
            .map_err(|e| Error::Config(format!("invalid api key: {e}")))?;
        auth.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth);
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        for (k, v) in extra_headers {
            let name = header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::Config(format!("bad extra header name {k}: {e}")))?;
            let value = header::HeaderValue::from_str(&v)
                .map_err(|e| Error::Config(format!("bad extra header value for {k}: {e}")))?;
            headers.insert(name, value);
        }
        // Reuse the workspace default reqwest builder so the SSRF
        // resolver + 30s timeout + 5-redirect cap apply to every
        // provider client. If the builder fails (broken TLS, etc.),
        // fall through to a bare client with the same default headers.
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .dns_resolver(std::sync::Arc::new(zunel_util::SsrfSafeResolver::new(
                false,
            )))
            .default_headers(headers)
            .build()
            .map_err(Error::Network)?;
        Ok(Self {
            client,
            api_base: api_base.trim_end_matches('/').to_string(),
        })
    }
}

#[async_trait]
impl LLMProvider for OpenAICompatProvider {
    async fn generate(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        settings: &GenerationSettings,
    ) -> Result<LLMResponse> {
        const MAX_ATTEMPTS: u32 = 2;
        const MAX_WAIT: Duration = Duration::from_secs(5);

        let body = RequestBody::new(model, messages, tools, settings);
        let url = format!("{}/chat/completions", self.api_base);

        let mut last_retry_after: Option<Duration> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let response = self.client.post(&url).json(&body).send().await?;
            let status = response.status();

            if status.is_success() {
                // 64 MiB is generous for a single chat-completions reply:
                // long tool-call arg strings + reasoning + multi-paragraph
                // text fit easily and we still block the OOM-by-chunked-
                // body class for a buggy or malicious provider.
                let raw = zunel_util::read_text_capped(response, 64 * 1024 * 1024)
                    .await
                    .map_err(|e| Error::Parse(format!("response body read failed: {e}")))?;
                let parsed: ResponseBody = serde_json::from_str(&raw)
                    .map_err(|e| Error::Parse(format!("json decode: {e}")))?;
                let choice = parsed
                    .choices
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::Parse("response had no choices".into()))?;
                let tool_calls = choice
                    .message
                    .tool_calls
                    .unwrap_or_default()
                    .into_iter()
                    .enumerate()
                    .map(|(i, wc)| parse_wire_tool_call(i, wc))
                    .collect::<Result<Vec<_>>>()?;
                return Ok(LLMResponse {
                    content: choice.message.content,
                    tool_calls,
                    usage: parsed.usage.unwrap_or_default().into(),
                    finish_reason: choice.finish_reason,
                });
            }

            if status.as_u16() == 429 && attempt < MAX_ATTEMPTS {
                let retry = parse_retry_after(response.headers())
                    .unwrap_or(Duration::from_millis(500))
                    .min(MAX_WAIT);
                last_retry_after = Some(retry);
                tracing::warn!(
                    attempt = attempt,
                    retry_after_ms = retry.as_millis() as u64,
                    "openai-compat: 429, retrying"
                );
                sleep(retry).await;
                continue;
            }

            if status.as_u16() == 429 {
                return Err(Error::RateLimited {
                    retry_after: last_retry_after,
                });
            }

            let text = zunel_util::read_text_capped(response, 64 * 1024)
                .await
                .unwrap_or_default();
            return Err(Error::ProviderReturned {
                status: status.as_u16(),
                body: text,
            });
        }
        unreachable!("loop always returns")
    }

    fn generate_stream<'a>(
        &'a self,
        model: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSchema],
        settings: &'a GenerationSettings,
    ) -> BoxStream<'a, Result<StreamEvent>> {
        self.stream_impl(model, messages, tools, settings)
    }
}

pub(super) fn parse_retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    let v = headers.get(header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = v.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    None
}
