use std::path::Path;

use anyhow::{Context, Result};

pub async fn run(config_path: Option<&Path>) -> Result<()> {
    let cfg = zunel_config::load_config(config_path).with_context(|| "loading zunel config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults)
        .with_context(|| "resolving workspace path")?;
    let provider = cfg.agents.defaults.provider.as_deref().unwrap_or("custom");
    println!("provider: {provider}");
    println!("model: {}", cfg.agents.defaults.model);
    println!("workspace: {}", workspace.display());
    println!("channels: {}", usize::from(cfg.channels.slack.is_some()));
    Ok(())
}
