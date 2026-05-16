use std::fs;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zunel::Zunel;

#[tokio::test]
async fn from_config_and_run() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "x",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "from facade" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("c.json");
    fs::write(
        &path,
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

    let bot = Zunel::from_config(Some(&path)).await.unwrap();
    let result = bot.run("hi").await.unwrap();
    assert_eq!(result.content, "from facade");
}
