//! Slack channel implementation.
//!
//! Split into focused submodules:
//!
//! * [`api`] — REST primitives (`auth.test`, `apps.connections.open`,
//!   `chat.postMessage`, `reactions.{add,remove}`, file download).
//! * [`inbound`] — Socket Mode envelope → [`InboundMessage`] parsing
//!   plus all allow/policy/mention rules and tests.
//!
//! This file keeps the [`SlackChannel`] type, its [`Channel`] trait
//! implementation, and the long-running Socket Mode reconnect loop. The
//! loop intentionally lives inline so it can close over the lock-protected
//! `connected` flag without a separate plumbing struct.

mod api;
pub mod bot_refresh;
mod inbound;

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use zunel_bus::{MessageBus, MessageKind, OutboundMessage};
use zunel_config::SlackChannelConfig;

use crate::{Channel, ChannelStatus, Error, Result};

/// Shared, hot-swappable Slack bot token. The gateway's bot-refresh
/// task writes through this handle on every successful rotation so
/// the next outbound `chat.postMessage` (and inbound reaction /
/// file download) uses the fresh token without restarting the
/// process. See `bot_refresh.rs` for the rotation flow and
/// `docs/configuration.md > Background bot-token refresh` for the
/// operator-facing semantics.
pub type BotTokenHandle = Arc<RwLock<String>>;

pub struct SlackChannel {
    config: SlackChannelConfig,
    bot_token: BotTokenHandle,
    api_base: String,
    client: reqwest::Client,
    connected: Arc<Mutex<bool>>,
    socket_task: Mutex<Option<JoinHandle<()>>>,
}

impl SlackChannel {
    pub fn new(config: SlackChannelConfig) -> Self {
        let bot_token = Arc::new(RwLock::new(config.bot_token.clone().unwrap_or_default()));
        Self {
            config,
            bot_token,
            api_base: "https://slack.com".into(),
            client: reqwest::Client::new(),
            connected: Arc::new(Mutex::new(false)),
            socket_task: Mutex::new(None),
        }
    }

    pub fn with_api_base(mut self, api_base: String) -> Self {
        self.api_base = api_base.trim_end_matches('/').to_string();
        self
    }

    /// Hand-out the live bot-token cell so the gateway-side
    /// bot-refresh loop can splice in a freshly-rotated token after
    /// every successful `oauth.v2.access` exchange. The next
    /// outbound `chat.postMessage`, reactions write, and Slack file
    /// download will pick up the new value without a process
    /// restart.
    pub fn bot_token_handle(&self) -> BotTokenHandle {
        Arc::clone(&self.bot_token)
    }

    fn snapshot_bot_token(&self) -> String {
        self.bot_token
            .read()
            .expect("slack bot token handle poisoned")
            .clone()
    }

    pub async fn status(&self) -> ChannelStatus {
        self.build_status().await
    }

    async fn build_status(&self) -> ChannelStatus {
        if !self.config.enabled {
            return ChannelStatus {
                name: "slack".into(),
                enabled: false,
                connected: false,
                detail: Some("disabled".into()),
            };
        }

        let bot_token_snapshot = self.snapshot_bot_token();
        let missing: Vec<&str> = [
            (
                "bot token",
                Some(bot_token_snapshot.as_str()).filter(|s| !s.is_empty()),
            ),
            ("app token", self.config.app_token.as_deref()),
        ]
        .into_iter()
        .filter_map(|(label, value)| {
            value
                .filter(|s| !s.is_empty())
                .map(|_| ())
                .is_none()
                .then_some(label)
        })
        .collect();

        if !missing.is_empty() {
            return ChannelStatus {
                name: "slack".into(),
                enabled: true,
                connected: false,
                detail: Some(format!("missing {}", missing.join(" and "))),
            };
        }

        ChannelStatus {
            name: "slack".into(),
            enabled: true,
            connected: *self.connected.lock().await,
            detail: Some("socket mode configured".into()),
        }
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &'static str {
        "slack"
    }

    async fn start(&self, bus: Arc<MessageBus>) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let status = self.build_status().await;
        if status
            .detail
            .as_deref()
            .is_some_and(|d| d.starts_with("missing "))
        {
            return Err(Error::Channel {
                channel: "slack".into(),
                message: status.detail.unwrap_or_else(|| "invalid config".into()),
            });
        }
        let app_token = self
            .config
            .app_token
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Channel {
                channel: "slack".into(),
                message: "missing app token".into(),
            })?;
        let bot_token_snapshot = self.snapshot_bot_token();
        if bot_token_snapshot.is_empty() {
            return Err(Error::Channel {
                channel: "slack".into(),
                message: "missing bot token".into(),
            });
        }
        let bot_user_id = api::auth_test(&self.client, &self.api_base, &bot_token_snapshot).await?;
        let socket_url = api::open_socket_url(&self.client, &self.api_base, app_token).await?;
        let (socket, _) = tokio_tungstenite::connect_async(&socket_url)
            .await
            .map_err(|e| Error::Channel {
                channel: "slack".into(),
                message: format!("socket mode connect failed: {e}"),
            })?;
        let first_socket = socket.split();
        let config = self.config.clone();
        let bot_user_id = bot_user_id.clone();
        let connected = self.connected.clone();
        let client = self.client.clone();
        let api_base = self.api_base.clone();
        let bot_token_handle = self.bot_token_handle();
        let app_token = app_token.to_string();
        *connected.lock().await = true;
        let task = tokio::spawn(socket_loop(
            first_socket,
            config,
            bot_user_id,
            connected,
            client,
            api_base,
            bot_token_handle,
            app_token,
            bus,
        ));
        *self.socket_task.lock().await = Some(task);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(task) = self.socket_task.lock().await.take() {
            task.abort();
        }
        *self.connected.lock().await = false;
        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> Result<()> {
        if !self.config.enabled {
            return Err(Error::Channel {
                channel: "slack".into(),
                message: "disabled".into(),
            });
        }
        // Snapshot the live token under a brief read lock instead of
        // capturing it once at boot. Hot-swap from the bot-refresh
        // loop is therefore picked up on the very next outbound call,
        // no restart required.
        let token = self.snapshot_bot_token();
        if token.is_empty() {
            return Err(Error::Channel {
                channel: "slack".into(),
                message: "missing bot token".into(),
            });
        }
        api::send_outbound(&self.client, &self.api_base, &self.config, &token, &message).await
    }

    async fn status(&self) -> ChannelStatus {
        self.build_status().await
    }
}

type SocketHalves = (
    futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
);

/// Long-running Socket Mode loop. Owns the WS halves, runs forever (until
/// the spawned task is aborted by `stop()`), and silently reconnects on any
/// IO/transport failure with a 250ms backoff.
///
/// The bot token is read through `bot_token` ([`BotTokenHandle`]) on every
/// reactions/file-download call rather than captured by value at spawn,
/// so a successful `oauth.v2.access` rotation in the gateway's
/// bot-refresh loop is picked up immediately by the next inbound event
/// — same staleness fix as the outbound `send` path.
#[allow(clippy::too_many_arguments)]
async fn socket_loop(
    first_socket: SocketHalves,
    config: SlackChannelConfig,
    bot_user_id: Option<String>,
    connected: Arc<Mutex<bool>>,
    client: reqwest::Client,
    api_base: String,
    bot_token: BotTokenHandle,
    app_token: String,
    bus: Arc<MessageBus>,
) {
    let mut first_socket = Some(first_socket);
    // Counts consecutive reconnect failures so the loop doesn't hammer
    // `apps.connections.open` during a Slack outage. Resets after a
    // successful connect.
    let mut consecutive_failures: u32 = 0;
    loop {
        let (mut write, mut read) = if let Some(socket) = first_socket.take() {
            socket
        } else {
            *connected.lock().await = false;
            tokio::time::sleep(socket_reconnect_backoff(consecutive_failures)).await;
            let socket_url = match api::open_socket_url(&client, &api_base, &app_token).await {
                Ok(url) => url,
                Err(err) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if consecutive_failures >= 10 {
                        tracing::error!(
                            consecutive_failures,
                            error = %err,
                            "slack socket open failing repeatedly"
                        );
                    } else {
                        tracing::debug!(error = %err, "slack socket open failed; will retry");
                    }
                    continue;
                }
            };
            let socket = match tokio_tungstenite::connect_async(&socket_url).await {
                Ok((socket, _)) => socket,
                Err(err) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if consecutive_failures >= 10 {
                        tracing::error!(
                            consecutive_failures,
                            error = %err,
                            "slack websocket connect failing repeatedly"
                        );
                    } else {
                        tracing::debug!(error = %err, "slack websocket connect failed; will retry");
                    }
                    continue;
                }
            };
            *connected.lock().await = true;
            consecutive_failures = 0;
            socket.split()
        };
        while let Some(next) = read.next().await {
            let Ok(message) = next else {
                break;
            };
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(envelope_id) = value.get("envelope_id").and_then(Value::as_str) {
                let _ = write
                    .send(Message::Text(
                        json!({"envelope_id": envelope_id}).to_string().into(),
                    ))
                    .await;
            }
            if let Some(mut inbound) = inbound::socket_interactive_to_inbound(&config, &value)
                .or_else(|| {
                    inbound::socket_message_to_inbound(&config, bot_user_id.as_deref(), &value)
                })
            {
                if inbound.kind == MessageKind::User {
                    if let Some((channel, timestamp, emoji)) =
                        inbound::inbound_reaction_target(&config, &value)
                    {
                        let token = bot_token
                            .read()
                            .expect("slack bot token handle poisoned")
                            .clone();
                        let _ = api::post_reaction(
                            &client,
                            &api_base,
                            &token,
                            "reactions.add",
                            &channel,
                            &emoji,
                            &timestamp,
                        )
                        .await;
                    }
                    let token = bot_token
                        .read()
                        .expect("slack bot token handle poisoned")
                        .clone();
                    inbound.media =
                        api::download_slack_files(&client, &api_base, &token, &value).await;
                }
                let _ = bus.publish_inbound(inbound).await;
            }
        }
    }
}

/// Compute the sleep duration before the next Slack Socket-Mode
/// reconnect attempt.
///
/// `consecutive_failures = 0` keeps the snappy 250ms behaviour the
/// pre-backoff code had so a single dropped websocket reconnects fast;
/// each subsequent failure doubles the delay up to a 30s ceiling.
/// A small (±20%) jitter is mixed in so multiple gateway processes
/// reconnecting after a Slack-side outage don't all hit
/// `apps.connections.open` on the same tick.
fn socket_reconnect_backoff(consecutive_failures: u32) -> Duration {
    const BASE_MS: u64 = 250;
    const CEILING_MS: u64 = 30_000;

    if consecutive_failures == 0 {
        return Duration::from_millis(BASE_MS);
    }
    let exp = (consecutive_failures - 1).min(8);
    let base_ms = BASE_MS.saturating_mul(1_u64 << exp).min(CEILING_MS);

    // `subsec_nanos % 41` lands in 0..=40 which we re-centre to -20..=20%.
    // Using wall-clock for jitter is fine here: this is not a security
    // primitive, just anti-thundering-herd.
    let jitter_pct = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 41)
        .unwrap_or(20) as i64
        - 20;
    let adjusted = base_ms as i64 + (base_ms as i64 * jitter_pct / 100);
    Duration::from_millis(adjusted.max(BASE_MS as i64 / 2) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_reconnect_backoff_zero_failures_is_baseline() {
        // First attempt after a clean drop: no backoff, just the original
        // 250ms snappy reconnect.
        assert_eq!(
            socket_reconnect_backoff(0),
            Duration::from_millis(250),
            "with zero failures we want the historical snappy reconnect"
        );
    }

    #[test]
    fn socket_reconnect_backoff_grows_then_caps() {
        // Jitter is ±20% so use ranges. The growth pattern is doubling
        // from 250ms (failure=1) up to 30s (failure=8+).
        let cases = [
            (1, 200..=300),     // 250 ±20%
            (2, 400..=600),     // 500
            (3, 800..=1_200),   // 1_000
            (4, 1_600..=2_400), // 2_000
            (8, 24_000..=36_000),
            (12, 24_000..=36_000), // capped
        ];
        for (failures, range) in cases {
            let got = socket_reconnect_backoff(failures).as_millis() as u64;
            assert!(
                range.contains(&got),
                "failures={failures} produced {got}ms, expected within {range:?}"
            );
        }
    }
}
