use std::sync::Arc;

use zunel_config::Config;

use crate::base::LLMProvider;
use crate::bedrock::BedrockProvider;
use crate::codex::CodexProvider;
use crate::error::{Error, Result};
use crate::openai_compat::OpenAICompatProvider;

/// Build the concrete provider selected by `agents.defaults.provider`.
///
/// Async because the Bedrock arm has to load AWS credentials via
/// `aws_config::defaults(...).load().await`. The other arms are
/// effectively sync but live inside the same async fn so callers don't
/// need to branch on provider name to decide whether to await.
pub async fn build_provider(config: &Config) -> Result<Arc<dyn LLMProvider>> {
    let name = config
        .agents
        .defaults
        .provider
        .as_deref()
        .unwrap_or("custom");
    build_provider_named(config, name).await
}

/// Build a specific provider by name against the same `providers.*`
/// configuration. Used by Dream's `provider_override` so a single
/// process can run the main agent on (say) Bedrock while Dream uses
/// a cheaper OpenAI-compatible endpoint, without two configs.
pub async fn build_provider_named(config: &Config, name: &str) -> Result<Arc<dyn LLMProvider>> {
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "custom" | "openai" | "openai_compat" => {
            let custom = config.providers.custom.as_ref().ok_or_else(|| {
                Error::Config(
                    "providers.custom is required when agents.defaults.provider = custom".into(),
                )
            })?;
            let headers = custom.extra_headers.clone().unwrap_or_default();
            let provider = OpenAICompatProvider::new(
                custom.api_key.clone(),
                custom.api_base.clone(),
                headers,
            )?;
            Ok(Arc::new(provider))
        }
        "codex" => {
            let codex = config.providers.codex.clone().unwrap_or_default();
            Ok(Arc::new(CodexProvider::new(codex.api_base)?))
        }
        "bedrock" => {
            let bedrock_cfg = config.providers.bedrock.clone().unwrap_or_default();
            let provider = BedrockProvider::new(bedrock_cfg).await?;
            Ok(Arc::new(provider))
        }
        other => Err(Error::Config(format!("unknown provider '{other}'"))),
    }
}
