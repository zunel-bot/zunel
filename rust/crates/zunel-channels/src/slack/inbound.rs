//! Inbound-event parsing for Slack Socket Mode.
//!
//! These helpers turn raw Socket Mode JSON envelopes into [`InboundMessage`]
//! values the bus can publish, applying the configured allow/policy rules
//! along the way. Pure functions over `&Value` so they're trivial to unit
//! test against canned envelopes.

use serde_json::Value;
use zunel_bus::{InboundMessage, MessageKind};
use zunel_config::SlackChannelConfig;

/// Convert a `events_api` `message` / `app_mention` envelope into an inbound
/// message. Returns `None` for events that should be ignored (bot echoes,
/// disallowed senders, channels outside the configured policy, etc.).
pub(super) fn socket_message_to_inbound(
    config: &SlackChannelConfig,
    bot_user_id: Option<&str>,
    value: &Value,
) -> Option<InboundMessage> {
    if value.get("type").and_then(Value::as_str) != Some("events_api") {
        return None;
    }
    let event = value.get("payload")?.get("event")?;
    let event_type = event.get("type").and_then(Value::as_str)?;
    if event_type != "message" && event_type != "app_mention" {
        return None;
    }
    let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
    if !matches!(subtype, "" | "file_share") {
        return None;
    }
    let user_id = event.get("user").and_then(Value::as_str)?.to_string();
    if bot_user_id.is_some_and(|bot_user_id| bot_user_id == user_id) {
        return None;
    }
    let chat_id = event.get("channel").and_then(Value::as_str)?.to_string();
    let channel_type = event
        .get("channel_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !slack_event_allowed(config, &user_id, &chat_id, channel_type) {
        return None;
    }
    let mut content = event
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if event_type == "message"
        && bot_user_id.is_some_and(|bot_user_id| content.contains(&format!("<@{bot_user_id}>")))
    {
        return None;
    }
    if channel_type != "im"
        && !should_respond_in_channel(config, event_type, &content, &chat_id, bot_user_id)
    {
        return None;
    }
    if event_type == "app_mention" {
        content = strip_bot_mention(&content, bot_user_id);
    }
    let thread_ts = event.get("thread_ts").and_then(Value::as_str).or_else(|| {
        (config.reply_in_thread && channel_type != "im")
            .then(|| event.get("ts").and_then(Value::as_str))
            .flatten()
    });
    let chat_id = if let Some(thread_ts) = thread_ts {
        if channel_type != "im" {
            format!("{chat_id}:{thread_ts}")
        } else {
            chat_id
        }
    } else {
        chat_id
    };
    Some(InboundMessage {
        channel: "slack".into(),
        chat_id,
        user_id: Some(user_id),
        content,
        media: Vec::new(),
        kind: MessageKind::User,
    })
}

/// Convert an `interactive` (button click) envelope from the approval card
/// into an [`InboundMessage`] of kind [`MessageKind::ApprovalResponse`].
pub(super) fn socket_interactive_to_inbound(
    config: &SlackChannelConfig,
    value: &Value,
) -> Option<InboundMessage> {
    if value.get("type").and_then(Value::as_str) != Some("interactive") {
        return None;
    }
    let user_id = value
        .get("payload")?
        .get("user")?
        .get("id")
        .and_then(Value::as_str)?;
    if !sender_allowed(&config.allow_from, user_id) {
        return None;
    }
    let action = value.get("payload")?.get("actions")?.as_array()?.first()?;
    let action_id = action.get("action_id").and_then(Value::as_str)?;
    let content = match action_id {
        "zunel_approve_once" | "zunel_approve_session" | "zunel_approve_always" => "approve",
        "zunel_approve_deny" => "deny",
        _ => return None,
    };
    let raw_value = action.get("value").and_then(Value::as_str)?;
    let payload: Value = serde_json::from_str(raw_value).ok()?;
    let session_key = payload.get("session_key").and_then(Value::as_str)?;
    let request_id = payload.get("request_id").and_then(Value::as_str)?;
    let (channel, chat_id) = session_key.split_once(':')?;
    Some(InboundMessage {
        channel: channel.into(),
        chat_id: chat_id.into(),
        user_id: None,
        content: format!("{content}:{request_id}"),
        media: Vec::new(),
        kind: MessageKind::ApprovalResponse,
    })
}

/// When the bot reacts to a freshly-received user message, return the
/// `(channel, ts, emoji)` it should react with. `None` when no `react_emoji`
/// is configured or the envelope is the wrong shape.
pub(super) fn inbound_reaction_target(
    config: &SlackChannelConfig,
    value: &Value,
) -> Option<(String, String, String)> {
    let emoji = config
        .react_emoji
        .as_deref()
        .filter(|emoji| !emoji.is_empty())?;
    if value.get("type").and_then(Value::as_str) != Some("events_api") {
        return None;
    }
    let event = value.get("payload")?.get("event")?;
    Some((
        event.get("channel").and_then(Value::as_str)?.to_string(),
        event.get("ts").and_then(Value::as_str)?.to_string(),
        emoji.to_string(),
    ))
}

fn slack_event_allowed(
    config: &SlackChannelConfig,
    user_id: &str,
    chat_id: &str,
    channel_type: &str,
) -> bool {
    if !sender_allowed(&config.allow_from, user_id) {
        return false;
    }
    if channel_type == "im" {
        if !config.dm.enabled {
            return false;
        }
        return config.dm.policy != "allowlist"
            || config.dm.allow_from.iter().any(|id| id == user_id);
    }
    config.group_policy != "allowlist" || config.group_allow_from.iter().any(|id| id == chat_id)
}

fn sender_allowed(allow_from: &[String], user_id: &str) -> bool {
    if allow_from.is_empty() {
        return false;
    }
    allow_from
        .iter()
        .any(|allowed| allowed == "*" || allowed == user_id)
}

fn should_respond_in_channel(
    config: &SlackChannelConfig,
    event_type: &str,
    text: &str,
    chat_id: &str,
    bot_user_id: Option<&str>,
) -> bool {
    match config.group_policy.as_str() {
        "open" => true,
        "mention" => {
            event_type == "app_mention"
                || bot_user_id
                    .is_some_and(|bot_user_id| text.contains(&format!("<@{bot_user_id}>")))
        }
        "allowlist" => config.group_allow_from.iter().any(|id| id == chat_id),
        _ => false,
    }
}

fn strip_bot_mention(text: &str, bot_user_id: Option<&str>) -> String {
    let Some(bot_user_id) = bot_user_id else {
        return text
            .split_whitespace()
            .filter(|part| !(part.starts_with("<@") && part.ends_with('>')))
            .collect::<Vec<_>>()
            .join(" ");
    };
    text.replace(&format!("<@{bot_user_id}>"), "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn socket_event(event: Value) -> Value {
        json!({
            "envelope_id": "env-1",
            "type": "events_api",
            "payload": {"event": event}
        })
    }

    #[test]
    fn empty_allow_from_denies_dm_messages() {
        let cfg = SlackChannelConfig {
            enabled: true,
            ..Default::default()
        };
        let inbound = socket_message_to_inbound(
            &cfg,
            None,
            &socket_event(json!({
                "type": "message",
                "user": "U1",
                "channel": "D1",
                "channel_type": "im",
                "text": "hello"
            })),
        );
        assert!(inbound.is_none());
    }

    #[test]
    fn wildcard_allow_from_allows_dm_messages() {
        let cfg = SlackChannelConfig {
            enabled: true,
            allow_from: vec!["*".into()],
            ..Default::default()
        };
        let inbound = socket_message_to_inbound(
            &cfg,
            None,
            &socket_event(json!({
                "type": "message",
                "user": "U1",
                "channel": "D1",
                "channel_type": "im",
                "text": "hello"
            })),
        )
        .unwrap();
        assert_eq!(inbound.content, "hello");
    }

    #[test]
    fn mention_policy_ignores_plain_channel_messages() {
        let cfg = SlackChannelConfig {
            enabled: true,
            allow_from: vec!["*".into()],
            group_policy: "mention".into(),
            ..Default::default()
        };
        let inbound = socket_message_to_inbound(
            &cfg,
            None,
            &socket_event(json!({
                "type": "message",
                "user": "U1",
                "channel": "C1",
                "channel_type": "channel",
                "text": "hello channel"
            })),
        );
        assert!(inbound.is_none());
    }

    #[test]
    fn mention_policy_allows_app_mentions() {
        let cfg = SlackChannelConfig {
            enabled: true,
            allow_from: vec!["*".into()],
            group_policy: "mention".into(),
            ..Default::default()
        };
        let inbound = socket_message_to_inbound(
            &cfg,
            None,
            &socket_event(json!({
                "type": "app_mention",
                "user": "U1",
                "channel": "C1",
                "channel_type": "channel",
                "text": "<@UBOT> hello"
            })),
        )
        .unwrap();
        assert_eq!(inbound.chat_id, "C1");
        assert_eq!(inbound.content, "hello");
    }

    #[test]
    fn suppresses_messages_from_bot_user() {
        let cfg = SlackChannelConfig {
            enabled: true,
            allow_from: vec!["*".into()],
            ..Default::default()
        };
        let inbound = socket_message_to_inbound(
            &cfg,
            Some("UBOT"),
            &socket_event(json!({
                "type": "message",
                "user": "UBOT",
                "channel": "D1",
                "channel_type": "im",
                "text": "bot echo"
            })),
        );
        assert!(inbound.is_none());
    }

    #[test]
    fn suppresses_duplicate_message_event_when_bot_is_mentioned() {
        let cfg = SlackChannelConfig {
            enabled: true,
            allow_from: vec!["*".into()],
            group_policy: "mention".into(),
            ..Default::default()
        };
        let inbound = socket_message_to_inbound(
            &cfg,
            Some("UBOT"),
            &socket_event(json!({
                "type": "message",
                "user": "U1",
                "channel": "C1",
                "channel_type": "channel",
                "text": "<@UBOT> hello"
            })),
        );
        assert!(inbound.is_none());
    }

    #[test]
    fn mention_policy_ignores_mentions_of_other_users() {
        let cfg = SlackChannelConfig {
            enabled: true,
            allow_from: vec!["*".into()],
            group_policy: "mention".into(),
            ..Default::default()
        };
        let inbound = socket_message_to_inbound(
            &cfg,
            Some("UBOT"),
            &socket_event(json!({
                "type": "message",
                "user": "U1",
                "channel": "C1",
                "channel_type": "channel",
                "text": "<@U2> hello"
            })),
        );
        assert!(inbound.is_none());
    }

    #[test]
    fn app_mention_strips_only_bot_mention() {
        let cfg = SlackChannelConfig {
            enabled: true,
            allow_from: vec!["*".into()],
            group_policy: "mention".into(),
            ..Default::default()
        };
        let inbound = socket_message_to_inbound(
            &cfg,
            Some("UBOT"),
            &socket_event(json!({
                "type": "app_mention",
                "user": "U1",
                "channel": "C1",
                "channel_type": "channel",
                "text": "<@UBOT> ask <@U2>"
            })),
        )
        .unwrap();
        assert_eq!(inbound.content, "ask <@U2>");
    }
}
