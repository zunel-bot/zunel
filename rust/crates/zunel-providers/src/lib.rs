//! LLM provider trait and implementations.

mod base;
pub mod bedrock;
mod build;
pub mod codex;
pub mod codex_refresh;
pub mod error;
mod openai_compat;
pub mod responses;
pub mod sse;
mod tool_call_accumulator;

pub use base::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, Role, StreamEvent, ToolCallDelta,
    ToolCallRequest, ToolProgress, ToolSchema, Usage,
};
pub use bedrock::BedrockProvider;
pub use build::build_provider;
pub use error::{Error, Result};
pub use openai_compat::OpenAICompatProvider;
pub use tool_call_accumulator::{ToolCallAccumulator, ToolCallAssemblyError};
