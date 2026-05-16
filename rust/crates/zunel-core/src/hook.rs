use async_trait::async_trait;
use zunel_providers::{ChatMessage, LLMResponse, ToolCallRequest};
use zunel_tools::ToolResult;

#[derive(Debug, Clone)]
pub struct AgentHookContext {
    pub iteration: usize,
    pub messages: Vec<ChatMessage>,
    pub response: Option<LLMResponse>,
    pub tool_calls: Vec<ToolCallRequest>,
    pub tool_results: Vec<ToolResult>,
    pub final_content: Option<String>,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
}

impl AgentHookContext {
    pub fn new(iteration: usize, messages: Vec<ChatMessage>) -> Self {
        Self {
            iteration,
            messages,
            response: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            final_content: None,
            stop_reason: None,
            error: None,
        }
    }
}

#[async_trait]
pub trait AgentHook: Send + Sync {
    fn wants_streaming(&self) -> bool {
        false
    }

    async fn before_iteration(&self, _context: AgentHookContext) {}

    async fn on_stream(&self, _context: AgentHookContext, _delta: String) {}

    async fn on_stream_end(&self, _context: AgentHookContext, _resuming: bool) {}

    async fn before_execute_tools(&self, _context: AgentHookContext) {}

    async fn after_iteration(&self, _context: AgentHookContext) {}

    fn finalize_content(
        &self,
        _context: AgentHookContext,
        content: Option<String>,
    ) -> Option<String> {
        content
    }
}
