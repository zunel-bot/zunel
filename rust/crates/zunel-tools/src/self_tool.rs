use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolContext, ToolResult};

#[derive(Debug, Clone, Default)]
pub struct SelfState {
    pub model: String,
    pub provider: String,
    pub workspace: String,
    pub max_iterations: u32,
    pub current_iteration: u32,
    pub tools: Vec<String>,
    pub subagents: Vec<SubagentSummary>,
}

#[derive(Debug, Clone, Default)]
pub struct SubagentSummary {
    pub id: String,
    pub label: String,
    pub phase: String,
    pub iteration: u32,
}

pub struct SelfTool {
    provider: Arc<dyn SelfStateProvider>,
}

impl SelfTool {
    pub fn new(state: SelfState) -> Self {
        Self {
            provider: Arc::new(StaticSelfStateProvider(state)),
        }
    }

    pub fn from_provider(provider: Arc<dyn SelfStateProvider>) -> Self {
        Self { provider }
    }

    fn render_summary(&self) -> String {
        let state = self.provider.state();
        let mut lines = vec![
            format!("model: {}", empty_as_unknown(&state.model)),
            format!("provider: {}", empty_as_unknown(&state.provider)),
            format!("workspace: {}", empty_as_unknown(&state.workspace)),
            format!(
                "iterations: {}/{}",
                state.current_iteration, state.max_iterations
            ),
            format!(
                "tools: {} registered - {}",
                state.tools.len(),
                state.tools.join(", ")
            ),
        ];
        if state.subagents.is_empty() {
            lines.push("subagents: none".into());
        } else {
            lines.push(format!("subagents: {}", state.subagents.len()));
            for subagent in &state.subagents {
                lines.push(format!(
                    "  [{}] {} - phase: {}, iteration: {}",
                    subagent.id, subagent.label, subagent.phase, subagent.iteration
                ));
            }
        }
        lines.join("\n")
    }
}

pub trait SelfStateProvider: Send + Sync {
    fn state(&self) -> SelfState;
}

struct StaticSelfStateProvider(SelfState);

impl SelfStateProvider for StaticSelfStateProvider {
    fn state(&self) -> SelfState {
        self.0.clone()
    }
}

#[async_trait]
impl Tool for SelfTool {
    fn name(&self) -> &'static str {
        "self"
    }

    fn description(&self) -> &'static str {
        "Inspect safe read-only runtime state such as model, workspace, registered tools, and subagent status. Secrets are never exposed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["check", "set"]},
                "key": {"type": "string"},
                "value": {}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        match args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("check")
        {
            "check" => ToolResult::ok(self.render_summary()),
            "set" => ToolResult::err("self tool is read-only"),
            other => ToolResult::err(format!("Unknown self action: {other}")),
        }
    }
}

fn empty_as_unknown(value: &str) -> &str {
    if value.is_empty() {
        "unknown"
    } else {
        value
    }
}
