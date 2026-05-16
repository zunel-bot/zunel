use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use zunel_bus::MessageBus;
use zunel_channels::build_channel_manager;

use crate::cli::{ChannelsArgs, ChannelsCommand};

pub async fn run(args: ChannelsArgs, config_path: Option<&Path>) -> Result<()> {
    match args.command {
        ChannelsCommand::Status => status(config_path).await,
    }
}

async fn status(config_path: Option<&Path>) -> Result<()> {
    let cfg = zunel_config::load_config(config_path).with_context(|| "loading zunel config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults)
        .with_context(|| "resolving workspace path")?;
    zunel_util::ensure_dir(&workspace)
        .with_context(|| format!("creating workspace dir {}", workspace.display()))?;

    let manager = build_channel_manager(&cfg.channels, Arc::new(MessageBus::new(256))).manager;
    let statuses = manager.statuses().await;
    println!("channels: {}", statuses.len());
    for channel in statuses {
        let state = if channel.connected {
            "connected"
        } else {
            "disconnected"
        };
        println!("{}: {state}", channel.name);
    }
    Ok(())
}
