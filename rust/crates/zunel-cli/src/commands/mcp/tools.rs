//! Implementations for the built-in MCP tools served by `zunel mcp serve`.
//!
//! Each function returns a `Result<String>` whose `Ok` value is a JSON string
//! ready to drop into the `tools/call` `content[].text` payload (see
//! [`super::serve::call_tool_with_args`]). Errors bubble up to the dispatch
//! layer where they are surfaced to the caller via stringified messages.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

pub(super) fn sessions_list(args: &Value) -> Result<String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let search = args.get("search").and_then(Value::as_str);
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let mut sessions = read_session_summaries(&workspace, search)?;
    sessions.sort_by(|a, b| {
        b.get("updated_at")
            .and_then(Value::as_str)
            .cmp(&a.get("updated_at").and_then(Value::as_str))
    });
    sessions.truncate(limit);
    Ok(serde_json::to_string(&json!({
        "count": sessions.len(),
        "sessions": sessions
    }))?)
}

pub(super) fn session_get(args: &Value) -> Result<String> {
    let key = required_session_key(args)?;
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let Some(path) = session_path(&workspace, key) else {
        return Ok(serde_json::to_string(&json!({"found": false, "key": key}))?);
    };
    let (metadata, messages) = read_session_file(&path)?;
    let Some(mut metadata) = metadata else {
        return Ok(serde_json::to_string(&json!({"found": false, "key": key}))?);
    };
    if let Some(obj) = metadata.as_object_mut() {
        obj.remove("_type");
        obj.insert("found".into(), json!(true));
        obj.insert("message_count".into(), json!(messages.len()));
    }
    Ok(serde_json::to_string(&metadata)?)
}

pub(super) fn session_messages(args: &Value) -> Result<String> {
    let key = required_session_key(args)?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let Some(path) = session_path(&workspace, key) else {
        return Ok(serde_json::to_string(&json!({
            "key": key,
            "count": 0,
            "messages": []
        }))?);
    };
    let (_metadata, mut messages) = read_session_file(&path)?;
    if messages.len() > limit {
        messages = messages.split_off(messages.len() - limit);
    }
    Ok(serde_json::to_string(&json!({
        "key": key,
        "count": messages.len(),
        "messages": messages
    }))?)
}

pub(super) fn channels_list() -> Result<String> {
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let mut channels = Vec::new();
    if let Some(slack) = cfg.channels.slack {
        channels.push(json!({
            "name": "slack",
            "enabled": slack.enabled,
            "mode": slack.mode,
            "allow_from_count": slack.allow_from.len(),
            "group_policy": slack.group_policy,
            "group_allow_from_count": slack.group_allow_from.len(),
            "reply_in_thread": slack.reply_in_thread,
            "dm": {
                "enabled": slack.dm.enabled,
                "policy": slack.dm.policy,
                "allow_from_count": slack.dm.allow_from.len()
            }
        }));
    }
    Ok(serde_json::to_string(&json!({
        "count": channels.len(),
        "channels": channels
    }))?)
}

pub(super) fn mcp_servers_list() -> Result<String> {
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let mut servers = Vec::new();
    for (name, server) in cfg.tools.mcp_servers {
        servers.push(json!({
            "name": name,
            "type": server.transport_type,
            "command": server.command,
            "args": server.args,
            "url": server.url,
            "tool_timeout": server.tool_timeout,
            "init_timeout": server.init_timeout,
            "enabled_tools": server.enabled_tools,
            "env_keys": server.env.as_ref().map(|env| env.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
            "header_keys": server.headers.as_ref().map(|headers| headers.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
            "oauth_enabled": server.normalized_oauth().map(|oauth| oauth.enabled).unwrap_or(false)
        }));
    }
    Ok(serde_json::to_string(&json!({
        "count": servers.len(),
        "servers": servers
    }))?)
}

pub(super) fn cron_jobs_list(args: &Value) -> Result<String> {
    let include_disabled = args
        .get("include_disabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut jobs = read_cron_jobs()?;
    if !include_disabled {
        jobs.retain(|job| job.get("enabled").and_then(Value::as_bool).unwrap_or(true));
    }
    Ok(serde_json::to_string(&json!({
        "count": jobs.len(),
        "jobs": jobs
    }))?)
}

pub(super) fn cron_job_get(args: &Value) -> Result<String> {
    let job_id = args
        .get("job_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("job_id is required"))?;
    let jobs = read_cron_jobs()?;
    let Some(mut job) = jobs
        .into_iter()
        .find(|job| job.get("id").and_then(Value::as_str) == Some(job_id))
    else {
        return Ok(serde_json::to_string(
            &json!({"found": false, "id": job_id}),
        )?);
    };
    if let Some(obj) = job.as_object_mut() {
        obj.insert("found".into(), json!(true));
    }
    Ok(serde_json::to_string(&job)?)
}

/// Start the chat-driven OAuth flow for a remote MCP server. Mirrors
/// the `mcp_login_start` handler in [`zunel_mcp_self::handlers`] so the
/// surface stays identical between the standalone `zunel-mcp-self`
/// binary and the unified `zunel mcp serve --server self` path.
pub(super) async fn mcp_login_start(args: &Value) -> Result<String> {
    let server = args
        .get("server")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("server is required"))?;
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let home = zunel_config::zunel_home().context("resolving zunel home directory")?;
    let started = zunel_mcp::oauth::start_flow(&home, &cfg, server, None)
        .await
        .with_context(|| format!("starting OAuth flow for '{server}'"))?;
    let instructions = format!(
        "Open the URL above in your browser. After you approve in your browser, the page \
         will show the redirect URL (or your browser will land on a `127.0.0.1` page). Copy \
         that full URL and paste it back to me as your next message — I'll finish the login \
         by calling `mcp_login_complete`. The pending login expires in {} minutes.",
        started.expires_in / 60
    );
    Ok(serde_json::to_string(&json!({
        "ok": true,
        "server": started.server,
        "authorize_url": started.authorize_url,
        "redirect_uri": started.redirect_uri,
        "expires_in": started.expires_in,
        "instructions": instructions,
    }))?)
}

/// Finish the chat-driven OAuth flow by exchanging the pasted redirect
/// URL for an access token. Returns `{ok: true, ...}` on success or
/// `{ok: false, error: "..."}` on any failure path so the agent doesn't
/// have to special-case error variants.
pub(super) async fn mcp_login_complete(args: &Value) -> Result<String> {
    let server = args
        .get("server")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("server is required"))?;
    let callback_url = args
        .get("callback_url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("callback_url is required"))?;
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let home = zunel_config::zunel_home().context("resolving zunel home directory")?;
    match zunel_mcp::oauth::complete_flow(&home, &cfg, server, callback_url).await {
        Ok(completed) => Ok(serde_json::to_string(&json!({
            "ok": true,
            "server": completed.server,
            "scopes": completed.scopes,
            "expires_in": completed.expires_in,
            "token_path": completed.token_path.display().to_string(),
        }))?),
        Err(err) => Ok(serde_json::to_string(&json!({
            "ok": false,
            "server": server,
            "error": err.to_string(),
        }))?),
    }
}

pub(super) async fn send_message_to_channel(args: &Value) -> Result<String> {
    let channel = required_str(args, "channel")?;
    if channel != "slack" {
        return Ok(serde_json::to_string(&json!({
            "ok": false,
            "error": format!("unsupported channel: {channel}")
        }))?);
    }
    let channel_id = required_str(args, "channel_id")?;
    let text = required_str(args, "text")?;
    if text.trim().is_empty() {
        return Err(anyhow::anyhow!("text is required"));
    }
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let token = std::env::var("SLACK_BOT_TOKEN")
        .ok()
        .or_else(|| cfg.channels.slack.and_then(|slack| slack.bot_token))
        .or_else(resolve_slack_bot_token_from_app_info)
        .ok_or_else(|| anyhow::anyhow!("Slack bot token is required"))?;
    let mut form = vec![
        ("channel".to_string(), channel_id.to_string()),
        ("text".to_string(), text.to_string()),
    ];
    if let Some(thread_ts) = args.get("thread_ts").and_then(Value::as_str) {
        if !thread_ts.is_empty() {
            form.push(("thread_ts".to_string(), thread_ts.to_string()));
        }
    }
    let base = std::env::var("SLACK_API_BASE").unwrap_or_else(|_| "https://slack.com".into());
    let response: Value = reqwest::Client::new()
        .post(format!(
            "{}/api/chat.postMessage",
            base.trim_end_matches('/')
        ))
        .bearer_auth(token)
        .form(&form)
        .send()
        .await
        .context("posting Slack message")?
        .json()
        .await
        .context("decoding Slack response")?;
    Ok(serde_json::to_string(&response)?)
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{key} is required"))
}

fn resolve_slack_bot_token_from_app_info() -> Option<String> {
    let path = zunel_config::zunel_home()
        .ok()?
        .join("slack-app")
        .join("app_info.json");
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    value
        .get("bot_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn required_session_key(args: &Value) -> Result<&str> {
    args.get("session_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| anyhow::anyhow!("session_key is required"))
}

fn read_session_summaries(workspace: &Path, search: Option<&str>) -> Result<Vec<Value>> {
    let dir = workspace.join("sessions");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let needle = search.map(str::to_lowercase);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(summary) = read_session_summary(&entry.path())? {
            let matches = needle
                .as_deref()
                .map(|needle| {
                    summary
                        .get("key")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(needle)
                })
                .unwrap_or(true);
            if matches {
                out.push(summary);
            }
        }
    }
    Ok(out)
}

fn session_path(workspace: &Path, key: &str) -> Option<std::path::PathBuf> {
    let path = workspace
        .join("sessions")
        .join(format!("{}.jsonl", safe_session_key(key)));
    path.exists().then_some(path)
}

fn safe_session_key(key: &str) -> String {
    const UNSAFE: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    key.chars()
        .map(|c| if UNSAFE.contains(&c) { '_' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

fn read_session_summary(path: &Path) -> Result<Option<Value>> {
    let (metadata, messages) = read_session_file(path)?;
    let Some(mut meta) = metadata else {
        return Ok(None);
    };
    if let Some(obj) = meta.as_object_mut() {
        obj.remove("_type");
        obj.insert("message_count".into(), json!(messages.len()));
    }
    Ok(Some(meta))
}

fn read_session_file(path: &Path) -> Result<(Option<Value>, Vec<Value>)> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut metadata: Option<Value> = None;
    let mut messages = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line)?;
        if value.get("_type").and_then(Value::as_str) == Some("metadata") {
            metadata = Some(value);
        } else {
            messages.push(value);
        }
    }
    Ok((metadata, messages))
}

fn read_cron_jobs() -> Result<Vec<Value>> {
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let path = workspace.join("cron").join("jobs.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let store: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
    )?;
    Ok(store
        .get("jobs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}
