use std::fs;

use assert_cmd::Command;
use futures::{SinkExt, StreamExt};
use predicates::str::contains;
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_response(chunks: &[&str]) -> String {
    let mut out = String::new();
    for (i, delta) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;
        let chunk = serde_json::json!({
            "id": format!("gateway-{i}"),
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

#[test]
fn gateway_dry_run_loads_config_and_reports_ready() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let config = home.path().join("config.json");
    fs::write(
        &config,
        format!(
            r#"{{
                "providers": {{ "custom": {{ "apiKey": "sk", "apiBase": "http://127.0.0.1:1" }} }},
                "agents": {{ "defaults": {{ "provider": "custom", "model": "m", "workspace": "{}" }} }}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("gateway")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(contains("gateway ready"))
        .stdout(contains("channels: 0"));
}

#[tokio::test(flavor = "multi_thread")]
async fn gateway_starts_configured_channels_without_dry_run() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());
    let socket_server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    });
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/apps.connections.open"))
        .and(header("authorization", "Bearer xapp-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "url": ws_url
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .and(header("authorization", "Bearer xoxb-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "user_id": "UBOT"
        })))
        .mount(&slack)
        .await;

    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let config = home.path().join("config.json");
    fs::write(
        &config,
        format!(
            r#"{{
                "providers": {{ "custom": {{ "apiKey": "sk", "apiBase": "http://127.0.0.1:1" }} }},
                "agents": {{ "defaults": {{ "provider": "custom", "model": "m", "workspace": "{}" }} }},
                "channels": {{
                    "slack": {{
                        "enabled": true,
                        "botToken": "xoxb-test",
                        "appToken": "xapp-test",
                        "allowFrom": ["*"]
                    }}
                }}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("ZUNEL_UNSAFE_SLACK_API_BASE", slack.uri())
        .arg("--config")
        .arg(&config)
        .arg("gateway")
        .arg("--startup-only")
        .assert()
        .success()
        .stdout(contains("gateway started"))
        .stdout(contains("channels: 1"));

    socket_server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn gateway_fails_startup_when_slack_bot_token_is_invalid() {
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .and(header("authorization", "Bearer xoxb-revoked"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": false,
            "error": "token_revoked"
        })))
        .expect(1)
        .mount(&slack)
        .await;

    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let config = home.path().join("config.json");
    fs::write(
        &config,
        format!(
            r#"{{
                "providers": {{ "custom": {{ "apiKey": "sk", "apiBase": "http://127.0.0.1:1" }} }},
                "agents": {{ "defaults": {{ "provider": "custom", "model": "m", "workspace": "{}" }} }},
                "channels": {{
                    "slack": {{
                        "enabled": true,
                        "botToken": "xoxb-revoked",
                        "appToken": "xapp-test",
                        "allowFrom": ["*"]
                    }}
                }}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("ZUNEL_UNSAFE_SLACK_API_BASE", slack.uri())
        .arg("--config")
        .arg(&config)
        .arg("gateway")
        .arg("--startup-only")
        .assert()
        .failure()
        .stderr(contains("token_revoked"));
}

#[tokio::test(flavor = "multi_thread")]
async fn gateway_processes_one_slack_socket_message_and_replies() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());
    let socket_server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({
                    "envelope_id": "env-1",
                    "type": "events_api",
                    "payload": {
                        "event": {
                            "type": "message",
                            "user": "U1",
                            "channel": "D1",
                            "channel_type": "im",
                            "text": "hello gateway",
                            "ts": "1713974400.000100"
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let ack = timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            ack.into_text().unwrap(),
            serde_json::json!({"envelope_id": "env-1"}).to_string()
        );
    });

    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/event-stream")
                .set_body_string(sse_response(&["reply from gateway"])),
        )
        .expect(1)
        .mount(&provider)
        .await;

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/apps.connections.open"))
        .and(header("authorization", "Bearer xapp-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "url": ws_url
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .and(header("authorization", "Bearer xoxb-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "user_id": "UBOT"
        })))
        .expect(1)
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(header("authorization", "Bearer xoxb-test"))
        .and(body_json(serde_json::json!({
            "channel": "D1",
            "text": "reply from gateway"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "1713974400.000200"
        })))
        .expect(1)
        .mount(&slack)
        .await;

    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let config = home.path().join("config.json");
    fs::write(
        &config,
        format!(
            r#"{{
                "providers": {{ "custom": {{ "apiKey": "sk", "apiBase": "{}" }} }},
                "agents": {{ "defaults": {{ "provider": "custom", "model": "m", "workspace": "{}" }} }},
                "channels": {{
                    "slack": {{
                        "enabled": true,
                        "botToken": "xoxb-test",
                        "appToken": "xapp-test",
                        "allowFrom": ["*"]
                    }}
                }}
            }}"#,
            provider.uri(),
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("ZUNEL_UNSAFE_SLACK_API_BASE", slack.uri())
        .arg("--config")
        .arg(&config)
        .arg("gateway")
        .arg("--max-inbound")
        .arg("1")
        .assert()
        .success()
        .stdout(contains("gateway started"))
        .stdout(contains("gateway processed inbound: 1"));

    socket_server.await.unwrap();
}
