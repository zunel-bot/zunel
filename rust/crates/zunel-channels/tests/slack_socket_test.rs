use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zunel_bus::{MessageBus, MessageKind};
use zunel_channels::slack::SlackChannel;
use zunel_channels::Channel;
use zunel_config::SlackChannelConfig;

#[tokio::test]
async fn slack_channel_receives_socket_mode_message_and_publishes_inbound() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());

    let socket_server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                serde_json::to_string(&serde_json::json!({
                    "envelope_id": "env-1",
                    "type": "events_api",
                    "payload": {
                        "event": {
                            "type": "message",
                            "user": "U1",
                            "channel": "D1",
                            "channel_type": "im",
                            "text": "hello from slack",
                            "ts": "1713974400.000100"
                        }
                    }
                }))
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();

        let ack = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            ack.into_text().unwrap(),
            serde_json::json!({"envelope_id": "env-1"}).to_string()
        );
    });

    let slack_api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/apps.connections.open"))
        .and(header("authorization", "Bearer xapp-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "url": ws_url
        })))
        .expect(1)
        .mount(&slack_api)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .and(header("authorization", "Bearer xoxb-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "user_id": "UBOT"
        })))
        .expect(1)
        .mount(&slack_api)
        .await;

    let bus = Arc::new(MessageBus::new(8));
    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-test".into()),
        app_token: Some("xapp-test".into()),
        allow_from: vec!["*".into()],
        ..Default::default()
    })
    .with_api_base(slack_api.uri());

    channel.start(bus.clone()).await.unwrap();
    let status = channel.status().await;
    assert!(status.connected, "{status:?}");

    let inbound = timeout(Duration::from_secs(2), bus.next_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.channel, "slack");
    assert_eq!(inbound.chat_id, "D1");
    assert_eq!(inbound.user_id.as_deref(), Some("U1"));
    assert_eq!(inbound.content, "hello from slack");
    assert_eq!(inbound.kind, MessageKind::User);

    channel.stop().await.unwrap();
    socket_server.await.unwrap();
}

#[tokio::test]
async fn slack_channel_uses_auth_test_to_suppress_self_and_duplicate_mentions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());

    let socket_server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        for (envelope_id, event) in [
            (
                "env-bot",
                serde_json::json!({
                    "type": "message",
                    "user": "UBOT",
                    "channel": "D1",
                    "channel_type": "im",
                    "text": "bot echo",
                    "ts": "1.0"
                }),
            ),
            (
                "env-duplicate",
                serde_json::json!({
                    "type": "message",
                    "user": "U1",
                    "channel": "C1",
                    "channel_type": "channel",
                    "text": "<@UBOT> duplicate",
                    "ts": "2.0"
                }),
            ),
            (
                "env-mention",
                serde_json::json!({
                    "type": "app_mention",
                    "user": "U1",
                    "channel": "C1",
                    "channel_type": "channel",
                    "text": "<@UBOT> keep <@U2>",
                    "ts": "3.0"
                }),
            ),
        ] {
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "envelope_id": envelope_id,
                        "type": "events_api",
                        "payload": {"event": event}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let ack = timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                ack.into_text().unwrap(),
                serde_json::json!({"envelope_id": envelope_id}).to_string()
            );
        }
    });

    let slack_api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/apps.connections.open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "url": ws_url
        })))
        .mount(&slack_api)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .and(header("authorization", "Bearer xoxb-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "user_id": "UBOT"
        })))
        .expect(1)
        .mount(&slack_api)
        .await;

    let bus = Arc::new(MessageBus::new(8));
    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-test".into()),
        app_token: Some("xapp-test".into()),
        allow_from: vec!["*".into()],
        group_policy: "mention".into(),
        ..Default::default()
    })
    .with_api_base(slack_api.uri());

    channel.start(bus.clone()).await.unwrap();
    let inbound = timeout(Duration::from_secs(2), bus.next_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.chat_id, "C1:3.0");
    assert_eq!(inbound.content, "keep <@U2>");

    channel.stop().await.unwrap();
    socket_server.await.unwrap();
}

#[tokio::test]
async fn slack_channel_turns_interactive_approval_buttons_into_bus_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());

    let socket_server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({
                    "envelope_id": "env-approval",
                    "type": "interactive",
                    "payload": {
                        "user": {"id": "U1"},
                        "actions": [{
                            "action_id": "zunel_approve_once",
                            "value": serde_json::json!({
                                "session_key": "slack:D1",
                                "request_id": "req-1"
                            }).to_string()
                        }]
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let ack = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            ack.into_text().unwrap(),
            serde_json::json!({"envelope_id": "env-approval"}).to_string()
        );
    });

    let slack_api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/apps.connections.open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "url": ws_url
        })))
        .mount(&slack_api)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "user_id": "UBOT"
        })))
        .mount(&slack_api)
        .await;

    let bus = Arc::new(MessageBus::new(8));
    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-test".into()),
        app_token: Some("xapp-test".into()),
        allow_from: vec!["*".into()],
        ..Default::default()
    })
    .with_api_base(slack_api.uri());

    channel.start(bus.clone()).await.unwrap();
    let inbound = timeout(Duration::from_secs(2), bus.next_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.channel, "slack");
    assert_eq!(inbound.chat_id, "D1");
    assert_eq!(inbound.kind, MessageKind::ApprovalResponse);
    assert_eq!(inbound.content, "approve:req-1");

    channel.stop().await.unwrap();
    socket_server.await.unwrap();
}

#[tokio::test]
async fn slack_channel_reconnects_after_socket_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());

    let socket_server = tokio::spawn(async move {
        let (first, _) = listener.accept().await.unwrap();
        let first_socket = tokio_tungstenite::accept_async(first).await.unwrap();
        drop(first_socket);

        let (second, _) = listener.accept().await.unwrap();
        let mut second_socket = tokio_tungstenite::accept_async(second).await.unwrap();
        second_socket
            .send(Message::Text(
                serde_json::json!({
                    "envelope_id": "env-after-reconnect",
                    "type": "events_api",
                    "payload": {
                        "event": {
                            "type": "message",
                            "user": "U1",
                            "channel": "D1",
                            "channel_type": "im",
                            "text": "after reconnect",
                            "ts": "4.0"
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let ack = timeout(Duration::from_secs(3), second_socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            ack.into_text().unwrap(),
            serde_json::json!({"envelope_id": "env-after-reconnect"}).to_string()
        );
    });

    let slack_api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/apps.connections.open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "url": ws_url
        })))
        .expect(2)
        .mount(&slack_api)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "user_id": "UBOT"
        })))
        .mount(&slack_api)
        .await;

    let bus = Arc::new(MessageBus::new(8));
    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-test".into()),
        app_token: Some("xapp-test".into()),
        allow_from: vec!["*".into()],
        ..Default::default()
    })
    .with_api_base(slack_api.uri());

    channel.start(bus.clone()).await.unwrap();
    let inbound = timeout(Duration::from_secs(3), bus.next_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.content, "after reconnect");

    channel.stop().await.unwrap();
    socket_server.await.unwrap();
}

#[tokio::test]
async fn slack_channel_adds_reaction_for_inbound_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());

    let socket_server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({
                    "envelope_id": "env-react",
                    "type": "events_api",
                    "payload": {
                        "event": {
                            "type": "message",
                            "user": "U1",
                            "channel": "D1",
                            "channel_type": "im",
                            "text": "react please",
                            "ts": "5.0"
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let _ack = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    });

    let slack_api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/apps.connections.open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "url": ws_url
        })))
        .mount(&slack_api)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "user_id": "UBOT"
        })))
        .mount(&slack_api)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/reactions.add"))
        .and(body_json(serde_json::json!({
            "channel": "D1",
            "name": "eyes",
            "timestamp": "5.0"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
        .expect(1)
        .mount(&slack_api)
        .await;

    let bus = Arc::new(MessageBus::new(8));
    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-test".into()),
        app_token: Some("xapp-test".into()),
        allow_from: vec!["*".into()],
        ..Default::default()
    })
    .with_api_base(slack_api.uri());

    channel.start(bus.clone()).await.unwrap();
    let inbound = timeout(Duration::from_secs(2), bus.next_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.content, "react please");

    channel.stop().await.unwrap();
    socket_server.await.unwrap();
}

#[tokio::test]
async fn slack_channel_downloads_file_share_media_paths() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ZUNEL_HOME", home.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());
    let slack_api = MockServer::start().await;
    let file_url = format!("{}/files/notes.txt", slack_api.uri());

    let socket_server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({
                    "envelope_id": "env-file",
                    "type": "events_api",
                    "payload": {
                        "event": {
                            "type": "message",
                            "subtype": "file_share",
                            "user": "U1",
                            "channel": "D1",
                            "channel_type": "im",
                            "text": "see attached",
                            "ts": "6.0",
                            "files": [{
                                "id": "F1",
                                "name": "notes.txt",
                                "url_private_download": file_url
                            }]
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let _ack = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    });

    Mock::given(method("POST"))
        .and(path("/api/apps.connections.open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "url": ws_url
        })))
        .mount(&slack_api)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "user_id": "UBOT"
        })))
        .mount(&slack_api)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/notes.txt"))
        .and(header("authorization", "Bearer xoxb-test"))
        .respond_with(ResponseTemplate::new(200).set_body_string("document body"))
        .expect(1)
        .mount(&slack_api)
        .await;

    let bus = Arc::new(MessageBus::new(8));
    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-test".into()),
        app_token: Some("xapp-test".into()),
        allow_from: vec!["*".into()],
        ..Default::default()
    })
    .with_api_base(slack_api.uri());

    channel.start(bus.clone()).await.unwrap();
    let inbound = timeout(Duration::from_secs(2), bus.next_inbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.content, "see attached");
    assert_eq!(inbound.media.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&inbound.media[0]).unwrap(),
        "document body"
    );

    channel.stop().await.unwrap();
    socket_server.await.unwrap();
    std::env::remove_var("ZUNEL_HOME");
}
