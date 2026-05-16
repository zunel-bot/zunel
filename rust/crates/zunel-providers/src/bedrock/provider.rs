//! `BedrockProvider` — `LLMProvider` impl backed by
//! `aws-sdk-bedrockruntime::Client::converse{,_stream}`.
//!
//! Auth piggybacks on the standard AWS credential chain via
//! `aws_config::defaults`. The user-facing workflow is the standard
//! AWS one:
//!
//! ```sh
//! aws sso login --profile <your-profile>
//! AWS_PROFILE=<your-profile> zunel agent
//! ```
//!
//! When `providers.bedrock.profile` / `providers.bedrock.region` are
//! set in `~/.zunel/config.json` they pin the loader instead of relying
//! on env vars; otherwise the standard chain (`AWS_PROFILE`,
//! `AWS_REGION`, default profile, instance role, …) is used as-is.

use std::sync::Arc;

use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_bedrockruntime::Client;
use futures::stream::BoxStream;

use super::streaming::{build_inference_config, map_sdk_error};
use super::wire::{
    convert_messages, convert_tools, reasoning_to_additional_fields, to_llm_response,
};
use crate::base::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema,
};
use crate::error::Result;
use zunel_config::BedrockProvider as BedrockProviderConfig;

/// Bedrock-backed `LLMProvider`. Cheap to clone (the underlying
/// `aws_sdk_bedrockruntime::Client` is `Arc`-internal), so the agent
/// loop can hand it to subagents without re-loading credentials.
pub struct BedrockProvider {
    client: Arc<Client>,
}

impl BedrockProvider {
    /// Construct a Bedrock provider, loading AWS credentials via the
    /// standard chain. Honors `providers.bedrock.{profile,region}` when
    /// set; otherwise falls back to `AWS_PROFILE` / `AWS_REGION` /
    /// default profile, exactly like the AWS CLI does.
    pub async fn new(cfg: BedrockProviderConfig) -> Result<Self> {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(profile) = cfg.profile.as_deref().filter(|s| !s.is_empty()) {
            loader = loader.profile_name(profile);
        }
        if let Some(region) = cfg.region.as_deref().filter(|s| !s.is_empty()) {
            loader = loader.region(Region::new(region.to_string()));
        }
        let sdk_config = loader.load().await;
        let client = Client::new(&sdk_config);
        Ok(Self {
            client: Arc::new(client),
        })
    }

    pub(super) fn client(&self) -> Arc<Client> {
        self.client.clone()
    }
}

#[async_trait]
impl LLMProvider for BedrockProvider {
    async fn generate(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        settings: &GenerationSettings,
    ) -> Result<LLMResponse> {
        let converted = convert_messages(messages)?;
        let tool_cfg = convert_tools(tools)?;
        let additional = reasoning_to_additional_fields(settings.reasoning_effort.as_deref());

        let mut request = self.client.converse().model_id(model);
        for sys in converted.system {
            request = request.system(sys);
        }
        for msg in converted.messages {
            request = request.messages(msg);
        }
        if let Some(cfg) = tool_cfg {
            request = request.tool_config(cfg);
        }
        if let Some(infer) = build_inference_config(settings) {
            request = request.inference_config(infer);
        }
        if let Some(extra) = additional {
            request = request.additional_model_request_fields(extra);
        }

        let output = request
            .send()
            .await
            .map_err(|e| map_sdk_error("Converse", e))?;
        to_llm_response(output)
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
