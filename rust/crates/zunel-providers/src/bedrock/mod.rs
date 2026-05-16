//! Amazon Bedrock provider (`Converse` / `ConverseStream`).
//!
//! Submodules:
//!  * [`wire`] — pure-function mapping between zunel `ChatMessage` /
//!    `ToolSchema` and Bedrock `Message` / `Tool` shapes (system lift,
//!    tool→user coalescing, JSON ↔ `Document`).
//!  * [`provider`] — owns [`BedrockProvider`] and the non-streaming
//!    [`crate::base::LLMProvider::generate`] impl.
//!  * [`streaming`] — `ConverseStream` → `StreamEvent` translation.
//!
//! Auth uses the standard AWS credential chain (env vars → SSO → IAM
//! role → instance profile) via `aws_config::defaults`. The user-facing
//! workflow is `aws sso login --profile <p>` followed by
//! `AWS_PROFILE=<p> zunel agent`.

mod provider;
mod streaming;
pub mod wire;

pub use provider::BedrockProvider;
