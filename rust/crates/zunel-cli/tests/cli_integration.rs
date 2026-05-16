use std::fs;

use assert_cmd::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
async fn agent_one_shot_prints_provider_reply() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/event-stream")
                .set_body_string(sse_response(&["integration ok"])),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let config_path = tmp.path().join("config.json");
    fs::write(
        &config_path,
        format!(
            r#"{{
                "providers": {{ "custom": {{ "apiKey": "sk", "apiBase": "{}" }} }},
                "agents": {{ "defaults": {{ "provider": "custom", "model": "gpt-x", "workspace": "{}" }} }}
            }}"#,
            server.uri(),
            workspace.display()
        ),
    )
    .unwrap();

    let assert = Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", tmp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("agent")
        .arg("-m")
        .arg("hi")
        .assert();

    assert
        .success()
        .stdout(predicates::str::contains("integration ok"));
}
