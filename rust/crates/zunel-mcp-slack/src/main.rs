//! Standalone stdio entrypoint for the built-in Slack MCP server.
//!
//! All tool logic lives in [`zunel_mcp_slack`] (the library half of this
//! crate) so `zunel mcp serve --server slack` can register the same surface
//! without spawning a separate process.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::BufReader;
use zunel_mcp::{read_frame, write_frame};

#[tokio::main]
async fn main() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);

    loop {
        let msg = match read_frame(&mut reader).await {
            Ok(msg) => msg,
            Err(_) => break,
        };
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            continue;
        };
        if method.starts_with("notifications/") {
            continue;
        }
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "zunel-mcp-slack", "version": env!("CARGO_PKG_VERSION")}
            }),
            "tools/list" => json!({"tools": zunel_mcp_slack::tools()}),
            "tools/call" => {
                let name = msg
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = msg
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match zunel_mcp_slack::call_tool(name, &args).await {
                    Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                    Err(err) => {
                        json!({"content": [{"type": "text", "text": err.to_string()}], "isError": true})
                    }
                }
            }
            _ => json!({}),
        };
        write_frame(
            &mut stdout,
            &json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
        .await?;
    }
    Ok(())
}
