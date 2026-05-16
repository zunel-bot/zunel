use anyhow::{Context, Result};
use serde_json::json;

use crate::cli::OnboardArgs;

pub async fn run(args: OnboardArgs) -> Result<()> {
    let home = zunel_config::zunel_home().with_context(|| "resolving zunel home")?;
    let config_path =
        zunel_config::default_config_path().with_context(|| "resolving config path")?;
    let workspace =
        zunel_config::default_workspace_path().with_context(|| "resolving workspace")?;
    zunel_config::guard_workspace(&workspace).with_context(|| "validating workspace path")?;
    zunel_util::ensure_dir(&home).with_context(|| format!("creating {}", home.display()))?;
    zunel_util::ensure_dir(&workspace)
        .with_context(|| format!("creating {}", workspace.display()))?;
    zunel_util::ensure_dir(&workspace.join("memory"))
        .with_context(|| format!("creating {}", workspace.join("memory").display()))?;

    if args.force || !config_path.exists() {
        let config = json!({
            "providers": {},
            "agents": {
                "defaults": {
                    "provider": "custom",
                    "model": "gpt-4o-mini",
                    "workspace": workspace.display().to_string()
                }
            },
            "channels": {},
            "tools": {}
        });
        // `config.json` holds provider apiKey values, the Brave search key,
        // and (after the first slack refresh) the Slack bot token. Write
        // with restricted perms from the start so it never sits at the
        // umask-default 0644 between onboard and the first refresh-driven
        // re-write.
        write_secret_json(&config_path, &serde_json::to_string_pretty(&config)?)
            .with_context(|| format!("writing {}", config_path.display()))?;
    }

    write_if_missing(
        &workspace.join("SOUL.md"),
        "# SOUL\n\nDescribe how Zunel should sound and behave.\n",
    )?;
    write_if_missing(
        &workspace.join("USER.md"),
        "# USER\n\nCapture stable information about the user here.\n",
    )?;
    write_if_missing(
        &workspace.join("HEARTBEAT.md"),
        "# HEARTBEAT\n\n## Periodic Tasks\n\n",
    )?;
    write_if_missing(
        &workspace.join("memory").join("MEMORY.md"),
        "# MEMORY\n\nDurable project facts and decisions live here.\n",
    )?;

    println!("onboarded: {}", home.display());
    println!("config: {}", config_path.display());
    println!("workspace: {}", workspace.display());
    Ok(())
}

fn write_if_missing(path: &std::path::Path, content: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

/// Atomic-write `content` to `path` with mode 0600 on Unix (no-op
/// elsewhere). Mirrors the pattern in `slack/bot_refresh.rs` and
/// `mcp_oauth::save_token` so all on-disk credential storage is
/// owner-only by default.
fn write_secret_json(path: &std::path::Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, content)
        .with_context(|| format!("writing temp file {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn write_secret_json_sets_0600_perms() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        write_secret_json(&path, "{\"hi\":1}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "onboard's config.json must land at 0600 to protect API keys"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"hi\":1}");
    }
}
