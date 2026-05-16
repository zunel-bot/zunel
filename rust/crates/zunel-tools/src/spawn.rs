use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolContext, ToolResult};

#[async_trait]
pub trait SpawnHandle: Send + Sync {
    async fn spawn(&self, task: String, label: Option<String>) -> Result<String, String>;
}

pub struct SpawnTool {
    handle: Arc<dyn SpawnHandle>,
}

impl SpawnTool {
    pub fn new(handle: Arc<dyn SpawnHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> &'static str {
        "spawn"
    }

    fn description(&self) -> &'static str {
        "Spawn a subagent to handle a task in the background. Use this for complex or time-consuming tasks that can run independently."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The task for the subagent to complete"},
                "label": {"type": "string", "description": "Optional short label for display"}
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if task.is_empty() {
            return ToolResult::err("task is required");
        }
        let label = args
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        match self.handle.spawn(task.to_string(), label).await {
            Ok(message) => ToolResult::ok(message),
            Err(err) => ToolResult::err(err),
        }
    }
}
