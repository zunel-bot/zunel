//! Channel runtime for gateway integrations.

pub mod slack;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use zunel_bus::{MessageBus, OutboundMessage};
use zunel_config::ChannelsConfig;

use crate::slack::{BotTokenHandle, SlackChannel};

/// Acquire a poison-tolerant lock on the channel registry.
///
/// The registry only stores `Arc<dyn Channel>` handles; if a task panicked
/// while holding the lock, the map is still safe to read/write — recovering
/// keeps the gateway alive instead of crashing the whole runtime.
fn lock_channels<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown channel: {0}")]
    UnknownChannel(String),
    #[error("channel {channel} failed: {message}")]
    Channel { channel: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelStatus {
    pub name: String,
    pub enabled: bool,
    pub connected: bool,
    pub detail: Option<String>,
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
    async fn start(&self, bus: Arc<MessageBus>) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send(&self, message: OutboundMessage) -> Result<()>;
    async fn status(&self) -> ChannelStatus;
}

#[derive(Clone)]
pub struct ChannelManager {
    bus: Arc<MessageBus>,
    channels: Arc<Mutex<BTreeMap<String, Arc<dyn Channel>>>>,
}

impl ChannelManager {
    pub fn new(bus: Arc<MessageBus>) -> Self {
        Self {
            bus,
            channels: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn register(&self, channel: Arc<dyn Channel>) {
        let name = channel.name().to_string();
        lock_channels(&self.channels).insert(name, channel);
    }

    pub async fn start_all(&self) -> Result<()> {
        let channels = self.snapshot().await;
        for channel in channels {
            channel.start(self.bus.clone()).await?;
        }
        Ok(())
    }

    pub async fn stop_all(&self) -> Result<()> {
        let channels = self.snapshot().await;
        for channel in channels {
            channel.stop().await?;
        }
        Ok(())
    }

    pub async fn dispatch(&self, message: OutboundMessage) -> Result<()> {
        let channel = lock_channels(&self.channels).get(&message.channel).cloned();
        let Some(channel) = channel else {
            return Err(Error::UnknownChannel(message.channel));
        };
        channel.send(message).await
    }

    pub async fn dispatch_next_outbound(&self) -> Result<bool> {
        let Some(message) = self.bus.next_outbound().await else {
            return Ok(false);
        };
        self.dispatch(message).await?;
        Ok(true)
    }

    pub async fn statuses(&self) -> Vec<ChannelStatus> {
        let channels = self.snapshot().await;
        let mut statuses = Vec::with_capacity(channels.len());
        for channel in channels {
            statuses.push(channel.status().await);
        }
        statuses
    }

    async fn snapshot(&self) -> Vec<Arc<dyn Channel>> {
        lock_channels(&self.channels).values().cloned().collect()
    }
}

/// What [`build_channel_manager`] hands back: the manager itself
/// plus a per-channel "live config handle" map for things the
/// gateway-side background tasks need to splice into running
/// channels (today: just the rotating Slack bot token, see the
/// `slack_bot_token` field).
///
/// Returning this struct instead of just the manager lets the
/// caller wire the bot-refresh loop straight into the same
/// `Arc<RwLock<String>>` the live `SlackChannel` is reading from on
/// every outbound `chat.postMessage`. Without this, the refresh
/// loop's writes to `~/.zunel/slack-app/app_info.json` +
/// `config.json` only became visible to the running gateway after a
/// process restart.
pub struct BuiltChannels {
    pub manager: ChannelManager,
    /// Live handle on the Slack bot token. `None` when no
    /// `channels.slack` block is configured (so no Slack channel was
    /// registered).
    pub slack_bot_token: Option<BotTokenHandle>,
}

pub fn build_channel_manager(config: &ChannelsConfig, bus: Arc<MessageBus>) -> BuiltChannels {
    let manager = ChannelManager::new(bus);
    let mut slack_bot_token: Option<BotTokenHandle> = None;
    if let Some(slack) = config.slack.clone() {
        let mut channel = SlackChannel::new(slack);
        if let Ok(api_base) = std::env::var("ZUNEL_UNSAFE_SLACK_API_BASE") {
            channel = channel.with_api_base(api_base);
        }
        slack_bot_token = Some(channel.bot_token_handle());
        manager.register(Arc::new(channel));
    }
    BuiltChannels {
        manager,
        slack_bot_token,
    }
}
