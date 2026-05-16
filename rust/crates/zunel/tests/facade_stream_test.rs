use std::fs;

use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zunel::{StreamEvent, Zunel};

fn sse_response(chunks: &[&str]) -> String {
    let mut out = String::new();
    for (i, delta) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;
        let chunk = serde_json::json!({
            "id": format!("c-{i}"),
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": { "content": delta },
                "finish_reason": if is_last { serde_json::json!("stop") } else { serde_json::Value::Null },
            }],
        });
        out.push_str(&format!("data: {}\n\n", chunk));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn run_streamed_emits_deltas_and_persists_history() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/event-stream")
                .set_body_string(sse_response(&["from ", "facade"])),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("c.json");
    fs::write(
        &config_path,
        format!(
            r#"{{
                "providers": {{ "custom": {{ "apiKey": "sk", "apiBase": "{}" }} }},
                "agents": {{ "defaults": {{ "provider": "custom", "model": "m", "workspace": "{}" }} }}
            }}"#,
            server.uri(),
            tmp.path().display()
        ),
    )
    .unwrap();

    let bot = Zunel::from_config(Some(&config_path)).await.unwrap();
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(32);
    let collector = tokio::spawn(async move {
        let mut out = String::new();
        while let Some(event) = rx.recv().await {
            if let StreamEvent::ContentDelta(s) = event {
                out.push_str(&s);
            }
        }
        out
    });
    let result = bot.run_streamed("cli:direct", "hi", tx).await.unwrap();
    assert_eq!(result.content, "from facade");
    let streamed = collector.await.unwrap();
    assert_eq!(streamed, "from facade");
}
