use std::fs;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
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
async fn repl_echoes_help_and_streams_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/event-stream")
                .set_body_string(sse_response(&["stream", "ed"])),
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

    let binary = assert_cmd::cargo::cargo_bin("zunel");
    let mut child = Command::new(binary)
        .env("ZUNEL_HOME", tmp.path())
        .env("NO_COLOR", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zunel");

    let stdin = child.stdin.as_mut().expect("stdin");
    stdin.write_all(b"/help\n").await.unwrap();
    stdin.write_all(b"hello\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    stdin.shutdown().await.unwrap();

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("repl timed out")
        .expect("wait");

    assert!(output.status.success(), "repl failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/help"),
        "expected help output in:\n{stdout}"
    );
    assert!(
        stdout.contains("streamed"),
        "expected streamed response in:\n{stdout}"
    );
}
