//! Slack Web API primitives + media download helpers.
//!
//! Everything in this module is a thin wrapper around a single Slack HTTP
//! call so the [`super`] driver can stay focused on lifecycle (connect, run
//! socket loop, dispatch events). Each helper takes `client` /  `api_base` /
//! `bot_token` (or `app_token`) explicitly so the same primitives are usable
//! from the `start()` socket task without owning a `&self`.

use futures::StreamExt;
use reqwest::header::{HeaderValue, AUTHORIZATION};
use serde_json::{json, Value};
use zunel_bus::{MessageKind, OutboundMessage};
use zunel_config::SlackChannelConfig;

use crate::{Error, Result};

/// Hard ceiling on a single Slack-file attachment download. Even
/// `response.bytes()` would buffer the whole body in RAM before the agent
/// loop could decide whether it's interesting; cap at 25 MiB so a
/// pathological event (or a future Slack bug returning a forged URL)
/// can't OOM the gateway.
const SLACK_FILE_MAX_BYTES: usize = 25 * 1024 * 1024;

/// Hostnames Slack hands us in `url_private` / `url_private_download`.
/// Sending our bot token to anything outside this set is an exfiltration
/// risk — a Slack workspace integration that controls the `files` payload
/// shape, or any future bug that lets the agent surface a forged event,
/// would otherwise leak the bearer.
///
/// `api_base_host` is the host of the configured Slack API base (usually
/// `slack.com`, but tests via `SlackChannel::with_api_base` point it at
/// wiremock). If the file URL's host matches, allow — anything we trust
/// for `auth.test`/`chat.postMessage` we also trust for file downloads.
fn slack_file_host_allowed(file_host: &str, api_base_host: &str) -> bool {
    let lower = file_host.to_ascii_lowercase();
    if !api_base_host.is_empty() && lower == api_base_host.to_ascii_lowercase() {
        return true;
    }
    lower == "slack.com"
        || lower == "files.slack.com"
        || lower == "slack-files.com"
        || lower.ends_with(".slack.com")
        || lower.ends_with(".slack-edge.com")
        || lower.ends_with(".slack-files.com")
}

/// `auth.test` — returns the bot user id (when present) so the inbound loop
/// can suppress its own messages.
pub(super) async fn auth_test(
    client: &reqwest::Client,
    api_base: &str,
    bot_token: &str,
) -> Result<Option<String>> {
    let mut auth =
        HeaderValue::from_str(&format!("Bearer {bot_token}")).map_err(|e| Error::Channel {
            channel: "slack".into(),
            message: format!("invalid bot token header: {e}"),
        })?;
    auth.set_sensitive(true);
    let response = client
        .post(format!("{api_base}/api/auth.test"))
        .header(AUTHORIZATION, auth)
        .send()
        .await
        .map_err(|e| Error::Channel {
            channel: "slack".into(),
            message: e.to_string(),
        })?;
    let status = response.status();
    let payload: Value = response.json().await.map_err(|e| Error::Channel {
        channel: "slack".into(),
        message: e.to_string(),
    })?;
    if !status.is_success() || payload.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(Error::Channel {
            channel: "slack".into(),
            message: format!(
                "auth.test failed: {}",
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown_error")
            ),
        });
    }
    Ok(payload
        .get("user_id")
        .and_then(Value::as_str)
        .filter(|user_id| !user_id.is_empty())
        .map(str::to_string))
}

/// `apps.connections.open` — exchange the app-level token for a Socket Mode
/// WebSocket URL (each URL is single-use).
pub(super) async fn open_socket_url(
    client: &reqwest::Client,
    api_base: &str,
    app_token: &str,
) -> Result<String> {
    let mut auth =
        HeaderValue::from_str(&format!("Bearer {app_token}")).map_err(|e| Error::Channel {
            channel: "slack".into(),
            message: format!("invalid app token header: {e}"),
        })?;
    auth.set_sensitive(true);
    let response = client
        .post(format!("{api_base}/api/apps.connections.open"))
        .header(AUTHORIZATION, auth)
        .send()
        .await
        .map_err(|e| Error::Channel {
            channel: "slack".into(),
            message: e.to_string(),
        })?;
    let status = response.status();
    let payload: Value = response.json().await.map_err(|e| Error::Channel {
        channel: "slack".into(),
        message: e.to_string(),
    })?;
    if !status.is_success() || payload.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(Error::Channel {
            channel: "slack".into(),
            message: payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("apps.connections.open failed")
                .to_string(),
        });
    }
    payload
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Channel {
            channel: "slack".into(),
            message: "apps.connections.open returned no url".into(),
        })
}

/// Generic `reactions.add` / `reactions.remove`.
pub(super) async fn post_reaction(
    client: &reqwest::Client,
    api_base: &str,
    bot_token: &str,
    method: &str,
    channel: &str,
    name: &str,
    timestamp: &str,
) -> Result<()> {
    let mut auth =
        HeaderValue::from_str(&format!("Bearer {bot_token}")).map_err(|e| Error::Channel {
            channel: "slack".into(),
            message: format!("invalid bot token header: {e}"),
        })?;
    auth.set_sensitive(true);
    let response = client
        .post(format!("{api_base}/api/{method}"))
        .header(AUTHORIZATION, auth)
        .json(&json!({
            "channel": channel,
            "name": name,
            "timestamp": timestamp
        }))
        .send()
        .await
        .map_err(|e| Error::Channel {
            channel: "slack".into(),
            message: e.to_string(),
        })?;
    let status = response.status();
    let payload: Value = response.json().await.map_err(|e| Error::Channel {
        channel: "slack".into(),
        message: e.to_string(),
    })?;
    if !status.is_success() || payload.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(Error::Channel {
            channel: "slack".into(),
            message: payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("reaction failed")
                .to_string(),
        });
    }
    Ok(())
}

/// `chat.postMessage` for an outbound zunel message. Handles the two
/// approval-button / done-emoji side-effects so the `Channel::send` impl can
/// stay a one-liner.
pub(super) async fn send_outbound(
    client: &reqwest::Client,
    api_base: &str,
    config: &SlackChannelConfig,
    bot_token: &str,
    message: &OutboundMessage,
) -> Result<()> {
    let (channel_id, thread_ts) = slack_target(&message.chat_id);
    let mut body = json!({
        "channel": channel_id,
        "text": message.content,
    });
    if message.kind == MessageKind::Approval {
        if let Some(request_id) = message.message_id.as_deref() {
            body["blocks"] = approval_blocks(&message.content, &message.chat_id, request_id);
        }
    }
    if config.reply_in_thread {
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = json!(thread_ts);
        }
    }
    let mut auth =
        HeaderValue::from_str(&format!("Bearer {bot_token}")).map_err(|e| Error::Channel {
            channel: "slack".into(),
            message: format!("invalid bot token header: {e}"),
        })?;
    auth.set_sensitive(true);
    let response = client
        .post(format!("{api_base}/api/chat.postMessage"))
        .header(AUTHORIZATION, auth)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Channel {
            channel: "slack".into(),
            message: e.to_string(),
        })?;
    let status = response.status();
    let payload: Value = response.json().await.map_err(|e| Error::Channel {
        channel: "slack".into(),
        message: e.to_string(),
    })?;
    if !status.is_success() || payload.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(Error::Channel {
            channel: "slack".into(),
            message: payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("chat.postMessage failed")
                .to_string(),
        });
    }
    if message.kind == MessageKind::Final {
        if let Some(thread_ts) = thread_ts {
            if let Some(emoji) = config.react_emoji.as_deref() {
                let _ = post_reaction(
                    client,
                    api_base,
                    bot_token,
                    "reactions.remove",
                    channel_id,
                    emoji,
                    thread_ts,
                )
                .await;
            }
            if let Some(emoji) = config.done_emoji.as_deref() {
                let _ = post_reaction(
                    client,
                    api_base,
                    bot_token,
                    "reactions.add",
                    channel_id,
                    emoji,
                    thread_ts,
                )
                .await;
            }
        }
    }
    Ok(())
}

/// Download every Slack file attached to an inbound event into
/// `~/.zunel/media/` (or `$TMPDIR/zunel-media/` when home is unavailable),
/// returning the on-disk paths so the agent loop can attach them.
pub(super) async fn download_slack_files(
    client: &reqwest::Client,
    api_base: &str,
    bot_token: &str,
    value: &Value,
) -> Vec<String> {
    // Extract the host of the configured API base so wiremock-style
    // test overrides via `SlackChannel::with_api_base` extend the
    // allowlist naturally — anything we trust for `chat.postMessage`
    // we trust for file downloads.
    let api_base_host = reqwest::Url::parse(api_base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let Some(files) = value
        .get("payload")
        .and_then(|payload| payload.get("event"))
        .and_then(|event| event.get("files"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let media_dir = match zunel_config::zunel_home() {
        Ok(home) => home.join("media"),
        Err(_) => std::env::temp_dir().join("zunel-media"),
    };
    if tokio::fs::create_dir_all(&media_dir).await.is_err() {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for file in files {
        let Some(url) = file
            .get("url_private_download")
            .or_else(|| file.get("url_private"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        // Refuse to attach the bot token to anything outside Slack's
        // own file hosts. The URL comes from message JSON; trusting it
        // unconditionally is a token-exfiltration foot-gun.
        let Ok(parsed) = reqwest::Url::parse(url) else {
            tracing::warn!(url, "skipping slack file with unparseable URL");
            continue;
        };
        let host = parsed.host_str().unwrap_or("");
        if !slack_file_host_allowed(host, &api_base_host) {
            tracing::warn!(
                url,
                host,
                "refusing to attach slack bot token to non-allowlisted host"
            );
            continue;
        }
        let name = file
            .get("name")
            .or_else(|| file.get("id"))
            .and_then(Value::as_str)
            .map(sanitize_filename)
            .unwrap_or_else(|| "slack-file".into());
        let path = media_dir.join(name);
        let mut auth = match HeaderValue::from_str(&format!("Bearer {bot_token}")).map_err(|e| {
            Error::Channel {
                channel: "slack".into(),
                message: format!("invalid bot token header: {e}"),
            }
        }) {
            Ok(auth) => auth,
            Err(_) => continue,
        };
        auth.set_sensitive(true);
        let Ok(response) = client.get(url).header(AUTHORIZATION, auth).send().await else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        // Stream-and-cap so a hostile (or accidentally huge) attachment
        // can't OOM the gateway. `response.bytes()` would buffer the
        // whole body before we got a chance to inspect the length.
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        let mut overflowed = false;
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                buf.clear();
                break;
            };
            if buf.len().saturating_add(chunk.len()) > SLACK_FILE_MAX_BYTES {
                tracing::warn!(
                    url,
                    cap = SLACK_FILE_MAX_BYTES,
                    "slack file exceeded size cap; dropping"
                );
                overflowed = true;
                break;
            }
            buf.extend_from_slice(&chunk);
        }
        if overflowed || buf.is_empty() {
            continue;
        }
        if tokio::fs::write(&path, &buf).await.is_ok() {
            paths.push(path.display().to_string());
        }
    }
    paths
}

/// Split a zunel `chat_id` of the form `<channel>:<thread_ts>` into its
/// channel and (optional) thread-timestamp parts. Plain `chat_id` values
/// without a `:` are returned as `(chat_id, None)`.
pub(super) fn slack_target(chat_id: &str) -> (&str, Option<&str>) {
    match chat_id.split_once(':') {
        Some((channel, thread_ts)) if !thread_ts.is_empty() => (channel, Some(thread_ts)),
        _ => (chat_id, None),
    }
}

fn sanitize_filename(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    // Bare `.` / `..` (and anything else whose stripped form is empty)
    // resolves via `media_dir.join(...)` to the media dir itself or its
    // parent — escapes the workspace boundary. Reject and use the
    // fallback.
    if sanitized.is_empty()
        || sanitized == "."
        || sanitized == ".."
        || sanitized.trim_matches('.').is_empty()
    {
        return "slack-file".into();
    }
    sanitized
}

fn approval_blocks(content: &str, session_key: &str, request_id: &str) -> Value {
    let approve_value = json!({
        "session_key": format!("slack:{session_key}"),
        "request_id": request_id
    })
    .to_string();
    let deny_value = json!({
        "session_key": format!("slack:{session_key}"),
        "request_id": request_id
    })
    .to_string();
    json!([
        {
            "type": "section",
            "text": {"type": "mrkdwn", "text": content}
        },
        {
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "text": {"type": "plain_text", "text": "Approve"},
                    "style": "primary",
                    "action_id": "zunel_approve_once",
                    "value": approve_value
                },
                {
                    "type": "button",
                    "text": {"type": "plain_text", "text": "Deny"},
                    "style": "danger",
                    "action_id": "zunel_approve_deny",
                    "value": deny_value
                }
            ]
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_file_host_allowed_accepts_canonical_slack_hosts() {
        // `api_base_host` left blank — only the static allowlist applies.
        for host in [
            "files.slack.com",
            "slack.com",
            "slack-files.com",
            "FILES.SLACK.COM", // case-insensitive
            "edge-1.slack-edge.com",
            "team-2.slack.com",
            "abc.slack-files.com",
        ] {
            assert!(
                slack_file_host_allowed(host, ""),
                "expected {host} to be allowlisted"
            );
        }
    }

    #[test]
    fn slack_file_host_allowed_rejects_attacker_hosts() {
        // The whole point: a forged or future-bug-leaked URL with a
        // non-Slack host must NOT receive our bot token.
        for host in [
            "attacker.example",
            "slack.com.attacker.example",
            "files.slack.attacker.example",
            "192.168.1.1",
            "127.0.0.1",
            "slackk.com",
        ] {
            assert!(
                !slack_file_host_allowed(host, ""),
                "expected {host} to be rejected"
            );
        }
    }

    #[test]
    fn slack_file_host_allowed_honours_api_base_override() {
        // Test/staging path: `SlackChannel::with_api_base` may point at
        // a non-slack.com host. File URLs on that same host are then
        // implicitly trusted (we already trust it for `chat.postMessage`).
        assert!(slack_file_host_allowed("127.0.0.1", "127.0.0.1"));
        assert!(slack_file_host_allowed("wiremock.test", "wiremock.test"));
        // The override is exact-host-match only — it doesn't open up to
        // arbitrary attacker hosts just because one base was overridden.
        assert!(!slack_file_host_allowed(
            "attacker.example",
            "wiremock.test"
        ));
    }

    #[test]
    fn sanitize_filename_blocks_dot_traversal() {
        // bare `.` and `..` would resolve via media_dir.join() to the
        // media dir itself or its parent — escapes the boundary.
        assert_eq!(sanitize_filename("."), "slack-file");
        assert_eq!(sanitize_filename(".."), "slack-file");
        assert_eq!(sanitize_filename("..."), "slack-file");
        assert_eq!(sanitize_filename(""), "slack-file");
        // legitimate filenames pass through.
        assert_eq!(sanitize_filename("image.png"), "image.png");
        assert_eq!(sanitize_filename("doc-2024.pdf"), "doc-2024.pdf");
        // Path separators get neutralised to '_'. The remaining dots
        // are fine as literal filename characters — only the bare
        // `.` / `..` components above would be traversal-relevant,
        // and those are caught by the explicit branch in sanitize.
        assert_eq!(sanitize_filename("foo/bar"), "foo_bar");
        assert_eq!(sanitize_filename("../../etc/passwd"), ".._.._etc_passwd");
    }
}
