use zunel_channels::slack::SlackChannel;
use zunel_config::SlackChannelConfig;

#[tokio::test]
async fn slack_status_reports_disabled_channel() {
    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: false,
        ..Default::default()
    });

    let status = channel.status().await;
    assert_eq!(status.name, "slack");
    assert!(!status.enabled);
    assert!(!status.connected);
    assert_eq!(status.detail.as_deref(), Some("disabled"));
}

#[tokio::test]
async fn slack_status_reports_missing_tokens_without_leaking_values() {
    let channel = SlackChannel::new(SlackChannelConfig {
        enabled: true,
        bot_token: Some("xoxb-secret".into()),
        app_token: None,
        ..Default::default()
    });

    let status = channel.status().await;
    assert!(status.enabled);
    assert!(!status.connected);
    let detail = status.detail.unwrap();
    assert!(detail.contains("missing app token"));
    assert!(!detail.contains("xoxb-secret"));
}
