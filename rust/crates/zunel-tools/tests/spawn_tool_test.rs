use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use zunel_tools::spawn::{SpawnHandle, SpawnTool};
use zunel_tools::{Tool, ToolContext};

#[derive(Default)]
struct FakeSpawner {
    calls: Mutex<Vec<(String, Option<String>)>>,
}

#[async_trait]
impl SpawnHandle for FakeSpawner {
    async fn spawn(&self, task: String, label: Option<String>) -> Result<String, String> {
        self.calls.lock().unwrap().push((task, label));
        Ok("Subagent [demo] started (id: abc123).".into())
    }
}

#[tokio::test]
async fn spawn_tool_delegates_to_handle() {
    let spawner = Arc::new(FakeSpawner::default());
    let tool = SpawnTool::new(spawner.clone());

    let result = tool
        .execute(
            json!({"task": "inspect the repo", "label": "demo"}),
            &ToolContext::for_test(),
        )
        .await;

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "Subagent [demo] started (id: abc123).");
    assert_eq!(
        spawner.calls.lock().unwrap().as_slice(),
        &[("inspect the repo".to_string(), Some("demo".to_string()))]
    );
}

#[tokio::test]
async fn spawn_tool_requires_task() {
    let tool = SpawnTool::new(Arc::new(FakeSpawner::default()));
    let result = tool
        .execute(json!({"label": "demo"}), &ToolContext::for_test())
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("task"));
}
