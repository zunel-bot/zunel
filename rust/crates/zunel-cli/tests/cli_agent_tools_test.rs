use std::path::PathBuf;

use assert_cmd::Command;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(events: &[&str]) -> String {
    let mut out = String::new();
    for e in events {
        out.push_str("data: ");
        out.push_str(e);
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[test]
fn cli_tool_call_roundtrip_writes_file() {
    let server = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let server = MockServer::start().await;
        // Turn 1: model emits a write_file tool call. The arguments
        // string is the JSON object we want the runner to parse.
        let body1 = sse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"out.txt\",\"content\":\"hi\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        // Turn 2: model returns the final reply.
        let body2 = sse(&[
            r#"{"choices":[{"delta":{"content":"done"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body1)
                    .insert_header("content-type", "text/event-stream"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body2)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        server
    });

    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let config = home.path().join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{
              "providers": {{ "custom": {{ "apiKey": "sk", "apiBase": "{}" }} }},
              "agents": {{ "defaults": {{ "provider": "custom", "model": "m", "workspace": "{}" }} }},
              "tools": {{ "exec": {{ "enable": false }}, "web": {{ "enable": false }} }}
            }}"#,
            server.uri(),
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("zunel").unwrap();
    cmd.env("ZUNEL_HOME", home.path());
    cmd.arg("--config").arg(&config);
    cmd.arg("agent").arg("-m").arg("please write out.txt");
    let out = cmd.assert().success().get_output().stdout.clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("done"), "stdout missing 'done': {s}");

    let expected: PathBuf = workspace.join("out.txt");
    let body = std::fs::read_to_string(&expected).expect("write_file should have created out.txt");
    assert_eq!(body, "hi");
}
