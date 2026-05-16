use std::sync::Arc;

use wiremock::matchers::{body_json, body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zunel_bus::{MessageBus, MessageKind, OutboundMessage};
use zunel_channels::slack::bot_refresh::{
    refresh_bot_if_near_expiry, RefreshContext, RefreshOutcome,
};
use zunel_channels::slack::SlackChannel;
use zunel_channels::{build_channel_manager, Channel};
use zunel_config::{ChannelsConfig, SlackChannelConfig};

#[tokio::test]
async fn slack_channel_sends_final_message_via_chat_post_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(header("authorization", "Bearer xoxb-test"))
        .and(body_json(serde_json::json!({
            "channel": "C123",
            "text": "hello slack",
            "thread_ts": "T456"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "123.456"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/reactions.remove"))
        .and(header("authorization", "Bearer xoxb-test"))
        .and(body_json(serde_json::json!({
            "channel": "C123",
            "name": "eyes",
            "timestamp": "T456"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/reactions.add"))
        .and(header("authorization", "Bearer xoxb-test"))
        .and(body_json(serde_json::json!({
            "channel": "C123",
            "name": "white_check_mark",
            "timestamp": "T456"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;

    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-test".into()),
        app_token: Some("xapp-test".into()),
        reply_in_thread: true,
        ..Default::default()
    })
    .with_api_base(server.uri());

    channel
        .send(OutboundMessage {
            channel: "slack".into(),
            chat_id: "C123:T456".into(),
            message_id: None,
            content: "hello slack".into(),
            media: Vec::new(),
            kind: MessageKind::Final,
        })
        .await
        .unwrap();
}

/// Regression for the "rotated bot token isn't picked up in-process" bug:
/// the gateway's bot-refresh task rewrites `~/.zunel/slack-app/app_info.json`
/// + `config.json` with a fresh token every ~12h, but the in-memory
/// `SlackChannel` was caching the boot-time token by value. After ~12h
/// every outbound `chat.postMessage` failed with `token_expired` until
/// someone restarted the gateway. The fix exposes a shared handle so
/// the refresh loop can splice the new token into the running channel.
#[tokio::test]
async fn slack_channel_picks_up_a_hot_swapped_bot_token_on_next_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(header("authorization", "Bearer xoxb-fresh"))
        .and(body_string_contains("\"text\":\"after rotation\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "999.001"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-old".into()),
        app_token: Some("xapp-test".into()),
        reply_in_thread: false,
        ..Default::default()
    })
    .with_api_base(server.uri());

    let handle = channel.bot_token_handle();
    *handle.write().expect("bot token handle poisoned") = "xoxb-fresh".into();

    channel
        .send(OutboundMessage {
            channel: "slack".into(),
            chat_id: "C123".into(),
            message_id: None,
            content: "after rotation".into(),
            media: Vec::new(),
            kind: MessageKind::User,
        })
        .await
        .expect("post-rotation send must succeed against the fresh-token mock");
}

#[tokio::test]
async fn slack_channel_sends_approval_message_with_buttons() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(header("authorization", "Bearer xoxb-test"))
        .and(body_string_contains("\"channel\":\"C123\""))
        .and(body_string_contains("\"text\":\"Approval required\""))
        .and(body_string_contains("\"thread_ts\":\"T456\""))
        .and(body_string_contains("\"action_id\":\"zunel_approve_once\""))
        .and(body_string_contains("\"action_id\":\"zunel_approve_deny\""))
        .and(body_string_contains("slack:C123:T456"))
        .and(body_string_contains("req-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "123.456"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-test".into()),
        app_token: Some("xapp-test".into()),
        reply_in_thread: true,
        ..Default::default()
    })
    .with_api_base(server.uri());

    channel
        .send(OutboundMessage {
            channel: "slack".into(),
            chat_id: "C123:T456".into(),
            message_id: Some("req-1".into()),
            content: "Approval required".into(),
            media: Vec::new(),
            kind: MessageKind::Approval,
        })
        .await
        .unwrap();
}

/// End-to-end wiring proof for the bot-token rotation fix:
///
/// 1. `build_channel_manager` registers a `SlackChannel` AND hands
///    back its bot-token handle.
/// 2. The gateway's bot-refresh task calls
///    `refresh_bot_if_near_expiry`, which mints a new token via the
///    `oauth.v2.access` `refresh_token` grant and rewrites
///    `app_info.json` + `config.json`.
/// 3. The refresh outcome carries the new token; the loop writes it
///    into the shared handle.
/// 4. The next `chat.postMessage` against `ChannelManager::dispatch`
///    must use the **fresh** token, not the boot-time one.
#[tokio::test]
async fn refresh_pipeline_swaps_in_process_bot_token_for_live_channel() {
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=xoxe-1-old"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "access_token":  "xoxb-fresh",
            "refresh_token": "xoxe-1-fresh",
            "expires_in":    43200,
            "token_type":    "bot",
            "scope":         "chat:write",
        })))
        .mount(&slack)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(header("authorization", "Bearer xoxb-fresh"))
        .and(body_string_contains("\"text\":\"after refresh\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "999.001"
        })))
        .expect(1)
        .mount(&slack)
        .await;

    let home = tempfile::tempdir().expect("tempdir");
    let app_info_path = home.path().join("slack-app").join("app_info.json");
    let cfg_path = home.path().join("config.json");
    std::fs::create_dir_all(app_info_path.parent().unwrap()).unwrap();
    std::fs::write(
        &app_info_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "client_id":            "111.222",
            "client_secret":        "shh",
            "bot_token":            "xoxb-old",
            "bot_refresh_token":    "xoxe-1-old",
            "bot_token_expires_at": 1, // already expired -> forces refresh
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        &cfg_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "channels": {
                "slack": {
                    "enabled":   true,
                    "botToken":  "xoxb-old",
                    "appToken":  "xapp-test",
                    "replyInThread": false
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let cfg = ChannelsConfig {
        slack: Some(SlackChannelConfig {
            enabled: true,
            bot_token: Some("xoxb-old".into()),
            app_token: Some("xapp-test".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let bus = Arc::new(MessageBus::new(8));
    // The Slack mock server hosts the API; the channel needs to talk
    // to it for `chat.postMessage`. Same handle must be plumbed to
    // `RefreshContext` for `oauth.v2.access`.
    let api_base = slack.uri();
    std::env::set_var("ZUNEL_UNSAFE_SLACK_API_BASE", &api_base);
    let built = build_channel_manager(&cfg, bus);
    std::env::remove_var("ZUNEL_UNSAFE_SLACK_API_BASE");
    let handle = built
        .slack_bot_token
        .clone()
        .expect("slack_bot_token handle must be exposed when slack is configured");
    assert_eq!(*handle.read().unwrap(), "xoxb-old");

    let outcome = refresh_bot_if_near_expiry(
        &RefreshContext {
            app_info_path: app_info_path.clone(),
            config_path: cfg_path.clone(),
            slack_api_base: slack.uri(),
        },
        Some(1800),
    )
    .await
    .expect("refresh must succeed against the mocked oauth.v2.access");
    let RefreshOutcome::Refreshed { bot_token, .. } = outcome else {
        panic!("expected Refreshed, got {outcome:?}");
    };
    assert_eq!(bot_token, "xoxb-fresh");

    *handle.write().unwrap() = bot_token;

    built
        .manager
        .dispatch(OutboundMessage {
            channel: "slack".into(),
            chat_id: "C123".into(),
            message_id: None,
            content: "after refresh".into(),
            media: Vec::new(),
            kind: MessageKind::User,
        })
        .await
        .expect("post-refresh dispatch must use the fresh bot token");
}
