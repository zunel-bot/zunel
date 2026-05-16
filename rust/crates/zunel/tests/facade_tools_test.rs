use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use zunel::{Tool, ToolContext, ToolResult, Zunel};

struct CounterTool {
    name: &'static str,
}

#[async_trait]
impl Tool for CounterTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "counts"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
        ToolResult::ok("1")
    }
}

#[tokio::test]
async fn register_tool_shows_up_in_tools_listing_alongside_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{
              "providers": {{ "custom": {{ "apiKey": "sk", "apiBase": "http://localhost:0" }} }},
              "agents": {{ "defaults": {{ "provider": "custom", "model": "m", "workspace": "{}" }} }}
            }}"#,
            tmp.path().display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    let mut bot = Zunel::from_config(Some(&config_path)).await.unwrap();
    bot.register_tool(Arc::new(CounterTool { name: "counter" }));
    let names: Vec<String> = bot.tools().names().map(|s| s.to_string()).collect();
    assert!(
        names.iter().any(|n| n == "counter"),
        "missing custom tool, got {names:?}"
    );
    // Defaults seeded by from_config:
    assert!(
        names.iter().any(|n| n == "read_file"),
        "missing read_file in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "list_dir"),
        "missing list_dir in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "spawn"),
        "missing spawn in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "self"),
        "missing self in {names:?}"
    );
}

#[tokio::test]
async fn unregister_tool_drops_a_previously_added_custom_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{
              "providers": {{ "custom": {{ "apiKey": "sk", "apiBase": "http://localhost:0" }} }},
              "agents": {{ "defaults": {{ "provider": "custom", "model": "m", "workspace": "{}" }} }}
            }}"#,
            tmp.path().display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    let mut bot = Zunel::from_config(Some(&config_path)).await.unwrap();
    bot.register_tool(Arc::new(CounterTool {
        name: "scratch_tool",
    }));
    assert!(bot.tools().get("scratch_tool").is_some());

    let removed = bot.unregister_tool("scratch_tool");
    assert!(
        removed,
        "unregister_tool should report the tool was dropped"
    );
    assert!(
        bot.tools().get("scratch_tool").is_none(),
        "scratch_tool should not be in the registry after unregister"
    );

    // Defaults still around.
    assert!(bot.tools().get("read_file").is_some());

    // Unregistering an unknown tool reports false instead of panicking.
    assert!(!bot.unregister_tool("nonexistent_tool"));
}
