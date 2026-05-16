//! Per-method dispatch for the `zunel-mcp-self` server. Lives outside
//! `main.rs` so both the stdio loop and the HTTP transport can share the
//! same handler set.
//!
//! Each public surface here is intentionally cheap and synchronous: the
//! HTTP server spawns one task per request and the stdio loop runs them
//! sequentially, so neither path benefits from `&mut self`-style state.
//!
//! Public surface: [`SelfDispatcher`] implements
//! [`crate::McpDispatcher`] and is the production wiring for the
//! zunel-self tool set.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDateTime};
use serde_json::{json, Value};

use crate::{DispatchMeta, McpDispatcher};

/// Server name reported on `initialize`. Kept here so both transports
/// agree on the value the host sees.
pub const SERVER_NAME: &str = "zunel-mcp-self";

/// Dispatcher that exposes the zunel-self tool set (sessions, cron,
/// channels, token usage, etc.). Stateless — the production stdio loop
/// and the Streamable-HTTP transport both instantiate it once and clone
/// the handle into their connection tasks.
#[derive(Clone, Default)]
pub struct SelfDispatcher;

impl SelfDispatcher {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpDispatcher for SelfDispatcher {
    async fn dispatch(&self, message: &Value, _meta: &DispatchMeta) -> Option<Value> {
        // The self-tool surface doesn't act on inbound depth — it
        // never fans out to other MCP servers — so the metadata is
        // intentionally ignored.
        handle_message(message).await
    }
}

/// Dispatch a single JSON-RPC message and return the response JSON-RPC
/// payload. Returns `None` for notifications (no `id`), and emits an
/// `error.code = -32601` (Method not found) envelope when the method
/// name isn't recognised — per JSON-RPC 2.0 §5.1, the prior `{}` result
/// for unknown methods was a protocol violation that some clients
/// would treat as a successful no-op.
pub async fn handle_message(msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    if method.starts_with("notifications/") {
        return None;
    }
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    match dispatch(method, msg).await {
        Some(result) => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })),
        None => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("method not found: {method}")
            }
        })),
    }
}

async fn dispatch(method: &str, msg: &Value) -> Option<Value> {
    match method {
        "initialize" => Some(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")}
        })),
        "tools/list" => Some(tools_list()),
        "tools/call" => Some(call_tool(msg).await),
        _ => None,
    }
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "self_status",
                "description": "Report native zunel self MCP server status",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "zunel_sessions_list",
                "description": "List zunel session summaries from the active workspace",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer"},
                        "search": {"type": "string"}
                    }
                }
            },
            {
                "name": "zunel_session_get",
                "description": "Get metadata for one zunel session",
                "inputSchema": {
                    "type": "object",
                    "properties": {"session_key": {"type": "string"}},
                    "required": ["session_key"]
                }
            },
            {
                "name": "zunel_session_messages",
                "description": "Get trailing messages for one zunel session",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_key": {"type": "string"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["session_key"]
                }
            },
            {
                "name": "zunel_channels_list",
                "description": "List configured zunel channels without secrets",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "zunel_mcp_servers_list",
                "description": "List configured MCP servers without secrets",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "zunel_cron_jobs_list",
                "description": "List cron jobs from the active workspace",
                "inputSchema": {
                    "type": "object",
                    "properties": {"include_disabled": {"type": "boolean"}}
                }
            },
            {
                "name": "zunel_cron_job_get",
                "description": "Get one cron job from the active workspace",
                "inputSchema": {
                    "type": "object",
                    "properties": {"job_id": {"type": "string"}},
                    "required": ["job_id"]
                }
            },
            {
                "name": "zunel_send_message_to_channel",
                "description": "Send text to a supported configured channel",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "channel": {"type": "string"},
                        "channel_id": {"type": "string"},
                        "text": {"type": "string"},
                        "thread_ts": {"type": "string"}
                    },
                    "required": ["channel", "channel_id", "text"]
                }
            },
            {
                "name": "zunel_token_usage",
                "description": "Report LLM token usage. With no args returns the lifetime grand total across every persisted session. With session_key returns that session's totals + per-turn breakdown. With since (e.g. 7d, 24h) sums turns newer than the cutoff.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_key": {"type": "string"},
                        "since": {"type": "string"}
                    }
                }
            },
            {
                "name": "mcp_login_start",
                "description": "Start an OAuth login flow for a remote MCP server. Returns the authorize URL the user should open in their browser; after they approve, they paste the redirect URL back into chat and the agent calls `mcp_login_complete`. Use this for `log me into <server>`, `reauth <server>`, or after an MCP tool call returns an error starting with `MCP_AUTH_REQUIRED:`. Side effect: writes `~/.zunel/mcp-oauth/<server>/pending.json` (10-min TTL).",
                "inputSchema": {
                    "type": "object",
                    "properties": {"server": {"type": "string"}},
                    "required": ["server"]
                }
            },
            {
                "name": "mcp_login_complete",
                "description": "Finish an OAuth login flow started by `mcp_login_start`. Pass the full redirect URL the IdP sent the user back to (or just the `?code=...&state=...` query string). Side effect: writes `~/.zunel/mcp-oauth/<server>/token.json` and removes the pending file on success.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "server": {"type": "string"},
                        "callback_url": {"type": "string"}
                    },
                    "required": ["server", "callback_url"]
                }
            },
            {
                "name": "zunel_dream_status",
                "description": "Report the most recent Dream consolidation pass: when it ran, how many history entries it processed, which files it edited, and whether the cursor advanced. Useful for answering 'did Dream run last night?' or debugging why MEMORY.md isn't updating.",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "zunel_config_get",
                "description": "Read a single value from ~/.zunel/config.json by dot-path (e.g. `agents.defaults.model`). Returns the value as JSON, or null when the path is missing. Read-only; pair with `zunel_config_set` to mutate.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            },
            {
                "name": "zunel_config_set",
                "description": "Write a single value into ~/.zunel/config.json at the given dot-path. The whole tree is re-validated against the Config schema before the file is replaced atomically — a value that would break boot is rejected without disk side effects. Use for self-tuning (e.g. switching `agents.defaults.model`, adding `tools.mcpServers.<name>`, toggling `tools.approvalRequired`). Note: server-affecting changes take effect on the next process start unless paired with `mcp_reconnect` or `/reload`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "value": {}
                    },
                    "required": ["path", "value"]
                }
            },
            {
                "name": "zunel_skill_add",
                "description": "Create a workspace skill at `<workspace>/skills/<name>/SKILL.md`. `content` should be the full SKILL.md body (YAML frontmatter + skill text). The skills loader picks up the new skill on the next agent turn; no reload needed. Rejects names containing path separators.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["name", "content"]
                }
            },
            {
                "name": "zunel_skill_remove",
                "description": "Remove a workspace skill at `<workspace>/skills/<name>/`. Idempotent — missing skill is treated as success.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            },
            {
                "name": "zunel_mcp_server_add",
                "description": "Register a new MCP server under `tools.mcpServers.<name>` in ~/.zunel/config.json. `config` is the server entry (transport, command/args/url, optional env/headers). The agent should call `mcp_reconnect` after this so the new server's tools appear on the next turn.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "config": {"type": "object"}
                    },
                    "required": ["name", "config"]
                }
            },
            {
                "name": "zunel_mcp_server_remove",
                "description": "Remove a server from `tools.mcpServers.<name>` in ~/.zunel/config.json. Atomic; idempotent. Pair with `mcp_reconnect` so the agent's tool registry drops the matching entries.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            },
            {
                "name": "zunel_heartbeat_task_add",
                "description": "Append a task line to `<workspace>/HEARTBEAT.md`. The line is appended verbatim (with a leading dash if missing) so the heartbeat scheduler can pick it up on the next tick. Multi-line values are joined with spaces.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }
            },
            {
                "name": "zunel_heartbeat_task_remove",
                "description": "Remove the first line of `<workspace>/HEARTBEAT.md` whose trimmed body equals `text`. Idempotent — no matching line is treated as success.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }
            },
            zunel_mcp_slack::capability_tool_descriptor()
        ]
    })
}

async fn call_tool(msg: &Value) -> Value {
    let name = msg
        .get("params")
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match name {
        "self_status" => wrap(self_status()),
        "zunel_sessions_list" | "sessions_list" => {
            let args = call_args(msg);
            wrap(sessions_list(&args))
        }
        "zunel_session_get" | "session_get" => {
            let args = call_args(msg);
            wrap(session_get(&args))
        }
        "zunel_session_messages" | "session_messages" => {
            let args = call_args(msg);
            wrap(session_messages(&args))
        }
        "zunel_channels_list" | "channels_list" => wrap(channels_list()),
        "zunel_mcp_servers_list" | "mcp_servers_list" => wrap(mcp_servers_list()),
        "zunel_cron_jobs_list" | "cron_jobs_list" => {
            let args = call_args(msg);
            wrap(cron_jobs_list(&args))
        }
        "zunel_cron_job_get" | "cron_job_get" => {
            let args = call_args(msg);
            wrap(cron_job_get(&args))
        }
        "zunel_send_message_to_channel" | "send_message_to_channel" => {
            let args = call_args(msg);
            wrap(send_message_to_channel(&args).await)
        }
        "zunel_token_usage" | "token_usage" => {
            let args = call_args(msg);
            wrap(token_usage(&args))
        }
        "zunel_slack_capability" | "slack_capability" => {
            wrap(Ok(zunel_mcp_slack::capability_report()))
        }
        "zunel_dream_status" | "dream_status" => wrap(dream_status()),
        "zunel_config_get" | "config_get" => wrap(config_get(&call_args(msg))),
        "zunel_config_set" | "config_set" => wrap(config_set(&call_args(msg))),
        "zunel_skill_add" | "skill_add" => wrap(skill_add(&call_args(msg))),
        "zunel_skill_remove" | "skill_remove" => wrap(skill_remove(&call_args(msg))),
        "zunel_mcp_server_add" | "mcp_server_add" => wrap(mcp_server_add(&call_args(msg))),
        "zunel_mcp_server_remove" | "mcp_server_remove" => wrap(mcp_server_remove(&call_args(msg))),
        "zunel_heartbeat_task_add" | "heartbeat_task_add" => {
            wrap(heartbeat_task_add(&call_args(msg)))
        }
        "zunel_heartbeat_task_remove" | "heartbeat_task_remove" => {
            wrap(heartbeat_task_remove(&call_args(msg)))
        }
        "mcp_login_start" => {
            let args = call_args(msg);
            wrap(mcp_login_start(&args).await)
        }
        "mcp_login_complete" => {
            let args = call_args(msg);
            wrap(mcp_login_complete(&args).await)
        }
        _ => {
            json!({"content": [{"type": "text", "text": format!("unknown tool: {name}")}], "isError": true})
        }
    }
}

fn wrap(result: Result<String>) -> Value {
    match result {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(err) => {
            json!({"content": [{"type": "text", "text": err.to_string()}], "isError": true})
        }
    }
}

fn sessions_list(args: &Value) -> Result<String> {
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

fn session_get(args: &Value) -> Result<String> {
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

fn session_messages(args: &Value) -> Result<String> {
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

fn channels_list() -> Result<String> {
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

/// Structured replacement for the historical "zunel-self ok" string.
/// Returns a single JSON object the agent can reason over directly:
/// `{ server, version, model, provider, workspace, last_dream_at,
/// last_heartbeat_at, mcp_servers, channels, sessions_dir_exists }`.
/// Cheaper than calling four separate self tools just to figure out
/// "is this thing healthy".
fn self_status() -> Result<String> {
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let scheduler_path = workspace.join(".zunel").join("scheduler.json");
    let (last_dream_at, last_heartbeat_at) = if scheduler_path.exists() {
        match std::fs::read_to_string(&scheduler_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        {
            Some(state) => (
                state.get("last_dream_at").cloned().unwrap_or(Value::Null),
                state
                    .get("last_dream_outcome")
                    .and_then(|o| o.get("at"))
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            None => (Value::Null, Value::Null),
        }
    } else {
        (Value::Null, Value::Null)
    };
    Ok(serde_json::to_string(&json!({
        "server": SERVER_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "model": cfg.agents.defaults.model,
        "provider": cfg.agents.defaults.provider,
        "workspace": workspace.display().to_string(),
        "mcp_servers": cfg.tools.mcp_servers.len(),
        "channels_enabled": cfg.channels.slack.as_ref().map(|s| s.enabled).unwrap_or(false),
        "sessions_dir_exists": workspace.join("sessions").exists(),
        "last_dream_at": last_dream_at,
        "last_heartbeat_at": last_heartbeat_at
    }))?)
}

/// Read `<workspace>/.zunel/scheduler.json` and surface the last
/// recorded Dream pass. Returns a structured payload (not just a
/// human-readable string) so the agent can reason over it. If the
/// gateway has never run, returns `{ "ever_ran": false }` rather than
/// an error — the caller will typically interpret that as "Dream
/// isn't configured or the gateway has never been started".
fn dream_status() -> Result<String> {
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let path = workspace.join(".zunel").join("scheduler.json");
    if !path.exists() {
        return Ok(serde_json::to_string(&json!({
            "ever_ran": false,
            "interval_h": cfg.agents.defaults.dream.interval_h,
            "note": "no scheduler.json yet — gateway hasn't run, or Dream interval is unset"
        }))?);
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let state: Value = serde_json::from_str(&raw).context("parsing scheduler.json")?;
    let last_at = state.get("last_dream_at").cloned().unwrap_or(Value::Null);
    let last_outcome = state
        .get("last_dream_outcome")
        .cloned()
        .unwrap_or(Value::Null);
    Ok(serde_json::to_string(&json!({
        "ever_ran": !last_at.is_null(),
        "interval_h": cfg.agents.defaults.dream.interval_h,
        "last_dream_at": last_at,
        "last_outcome": last_outcome
    }))?)
}

// ---------------------------------------------------------------------------
// Self-modify surface: schema-validated tools that let the agent (or the user
// via the agent) edit its own config, skills, MCP servers, and heartbeat
// tasks. Every mutation is approval-gated by the calling agent's normal
// tool-call dispatch — these handlers always honour the call.
// ---------------------------------------------------------------------------

/// Read a JSON value out of `~/.zunel/config.json` at the given
/// dot-path. Returns `null` for a path that doesn't exist (rather
/// than an error) so the agent can use this for existence checks
/// without ceremony.
fn config_get(args: &Value) -> Result<String> {
    let path = required_str(args, "path")?;
    let tree = zunel_config::load_config_json(None).context("loading config")?;
    let value = walk_json_path(&tree, path).cloned().unwrap_or(Value::Null);
    Ok(serde_json::to_string(&json!({
        "path": path,
        "value": value
    }))?)
}

/// Write a JSON value into `~/.zunel/config.json` at the given
/// dot-path. The whole tree is re-validated against the `Config`
/// schema before the file is atomically replaced; a value that
/// would break boot is rejected without disk side effects.
fn config_set(args: &Value) -> Result<String> {
    let path = required_str(args, "path")?;
    let value = args
        .get("value")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing required arg: value"))?;
    let mut tree = zunel_config::load_config_json(None).context("loading config")?;
    set_json_path(&mut tree, path, value.clone())
        .map_err(|e| anyhow::anyhow!("set {path}: {e}"))?;
    zunel_config::save_config_json(None, &tree).map_err(|e| anyhow::anyhow!("save config: {e}"))?;
    Ok(serde_json::to_string(&json!({
        "path": path,
        "value": value,
        "note": "config saved; some changes require process restart or mcp_reconnect"
    }))?)
}

/// Add (or overwrite) a workspace skill at
/// `<workspace>/skills/<name>/SKILL.md`. `name` must not contain
/// `/`, `\\`, or `..` so a stray segment can't escape the skills
/// dir. Existing skill dir + body is overwritten — callers that
/// want create-only semantics should check with `read_file` first.
fn skill_add(args: &Value) -> Result<String> {
    let name = required_str(args, "name")?;
    validate_skill_name(name)?;
    let content = required_str(args, "content")?;
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let skill_dir = workspace.join("skills").join(name);
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("creating {}", skill_dir.display()))?;
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(&skill_path, content)
        .with_context(|| format!("writing {}", skill_path.display()))?;
    Ok(serde_json::to_string(&json!({
        "status": "ok",
        "path": skill_path.display().to_string(),
        "bytes": content.len()
    }))?)
}

/// Remove a workspace skill directory at
/// `<workspace>/skills/<name>/`. Idempotent: a missing skill
/// returns `removed: false` rather than an error so the agent can
/// safely call this without first checking.
fn skill_remove(args: &Value) -> Result<String> {
    let name = required_str(args, "name")?;
    validate_skill_name(name)?;
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let skill_dir = workspace.join("skills").join(name);
    if !skill_dir.exists() {
        return Ok(serde_json::to_string(&json!({
            "status": "ok",
            "removed": false,
            "note": "skill did not exist"
        }))?);
    }
    std::fs::remove_dir_all(&skill_dir)
        .with_context(|| format!("removing {}", skill_dir.display()))?;
    Ok(serde_json::to_string(&json!({
        "status": "ok",
        "removed": true,
        "path": skill_dir.display().to_string()
    }))?)
}

/// Splice a new MCP server into `tools.mcpServers.<name>` in the
/// raw JSON config (preserves unknown sibling fields and ordering).
/// The whole tree round-trips through the `Config` schema before
/// the write commits, so an invalid server config is rejected.
fn mcp_server_add(args: &Value) -> Result<String> {
    let name = required_str(args, "name")?;
    let config = args
        .get("config")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing required arg: config"))?;
    if !config.is_object() {
        anyhow::bail!("config must be a JSON object");
    }
    let mut tree = zunel_config::load_config_json(None).context("loading config")?;
    set_json_path(&mut tree, &format!("tools.mcpServers.{name}"), config)
        .map_err(|e| anyhow::anyhow!("set tools.mcpServers.{name}: {e}"))?;
    zunel_config::save_config_json(None, &tree).map_err(|e| anyhow::anyhow!("save config: {e}"))?;
    Ok(serde_json::to_string(&json!({
        "status": "ok",
        "name": name,
        "note": "call mcp_reconnect to load the new server's tools"
    }))?)
}

/// Remove `tools.mcpServers.<name>` from the config and persist.
/// Idempotent.
fn mcp_server_remove(args: &Value) -> Result<String> {
    let name = required_str(args, "name")?;
    let mut tree = zunel_config::load_config_json(None).context("loading config")?;
    let removed = remove_json_path(&mut tree, &format!("tools.mcpServers.{name}"));
    if !removed {
        return Ok(serde_json::to_string(&json!({
            "status": "ok",
            "removed": false,
            "note": "no such MCP server"
        }))?);
    }
    zunel_config::save_config_json(None, &tree).map_err(|e| anyhow::anyhow!("save config: {e}"))?;
    Ok(serde_json::to_string(&json!({
        "status": "ok",
        "removed": true,
        "name": name,
        "note": "call mcp_reconnect so the agent drops the matching tools"
    }))?)
}

/// Append a task line to `<workspace>/HEARTBEAT.md`. Multi-line
/// input is joined with spaces so each task remains a single line
/// (the heartbeat parser is line-oriented). A leading `- ` is
/// added if missing so all entries follow the same list style.
fn heartbeat_task_add(args: &Value) -> Result<String> {
    let text = required_str(args, "text")?.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("text must not be empty");
    }
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let path = workspace.join("HEARTBEAT.md");
    let line = if text.starts_with("- ") {
        text
    } else {
        format!("- {text}")
    };
    let line = line.replace('\n', " ");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&line);
    body.push('\n');
    std::fs::write(&path, &body).with_context(|| format!("writing {}", path.display()))?;
    Ok(serde_json::to_string(&json!({
        "status": "ok",
        "path": path.display().to_string(),
        "appended_line": line
    }))?)
}

/// Remove the first line of `<workspace>/HEARTBEAT.md` whose
/// trimmed (and leading-`-` stripped) body equals `text`.
/// Idempotent — a non-matching call is reported as `removed: false`
/// rather than an error.
fn heartbeat_task_remove(args: &Value) -> Result<String> {
    let needle = required_str(args, "text")?.trim().to_string();
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let path = workspace.join("HEARTBEAT.md");
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            return Ok(serde_json::to_string(&json!({
                "status": "ok",
                "removed": false,
                "note": "HEARTBEAT.md does not exist"
            }))?)
        }
    };
    let mut removed = false;
    let mut out_lines: Vec<&str> = Vec::new();
    for line in existing.lines() {
        let stripped = line.trim_start();
        let body = stripped.strip_prefix("- ").unwrap_or(stripped).trim();
        if !removed && body == needle {
            removed = true;
            continue;
        }
        out_lines.push(line);
    }
    if !removed {
        return Ok(serde_json::to_string(&json!({
            "status": "ok",
            "removed": false,
            "note": "no matching line"
        }))?);
    }
    let mut body = out_lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    std::fs::write(&path, &body).with_context(|| format!("writing {}", path.display()))?;
    Ok(serde_json::to_string(&json!({
        "status": "ok",
        "removed": true,
        "path": path.display().to_string()
    }))?)
}

/// Walk a dot-path through a JSON tree; returns `None` if any
/// segment is missing or the parent is not an object.
fn walk_json_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for segment in path.split('.') {
        if segment.is_empty() {
            return None;
        }
        cur = cur.get(segment)?;
    }
    Some(cur)
}

/// Set a value at the given dot-path, creating intermediate
/// objects as needed. Returns an error if a non-object value
/// blocks the path.
fn set_json_path(
    root: &mut Value,
    path: &str,
    value: Value,
) -> std::result::Result<(), &'static str> {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err("path must not be empty");
    }
    let mut cur = root;
    for segment in &segments[..segments.len() - 1] {
        if !cur.is_object() {
            return Err("non-object value blocks path");
        }
        let obj = cur.as_object_mut().unwrap();
        if !obj.contains_key(*segment) {
            obj.insert(
                (*segment).to_string(),
                Value::Object(serde_json::Map::new()),
            );
        }
        cur = obj.get_mut(*segment).unwrap();
    }
    if !cur.is_object() {
        return Err("non-object value blocks final segment");
    }
    cur.as_object_mut()
        .unwrap()
        .insert(segments.last().unwrap().to_string(), value);
    Ok(())
}

/// Remove a key at the given dot-path. Returns `true` when the
/// key existed and was removed, `false` otherwise.
fn remove_json_path(root: &mut Value, path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return false;
    }
    let mut cur = root;
    for segment in &segments[..segments.len() - 1] {
        let Some(next) = cur.get_mut(*segment) else {
            return false;
        };
        if !next.is_object() {
            return false;
        }
        cur = next;
    }
    let Some(obj) = cur.as_object_mut() else {
        return false;
    };
    obj.remove(*segments.last().unwrap()).is_some()
}

/// Reject skill names that could escape the skills directory or
/// contain shell metacharacters. Allows letters, digits, `-`, `_`.
fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.starts_with('.')
    {
        anyhow::bail!("invalid skill name: {name}");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("skill name must be [a-zA-Z0-9_-]: {name}");
    }
    Ok(())
}

fn mcp_servers_list() -> Result<String> {
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

fn cron_jobs_list(args: &Value) -> Result<String> {
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

fn cron_job_get(args: &Value) -> Result<String> {
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

/// Implements the `zunel_token_usage` MCP tool.
///
/// Modes:
/// - No args → grand-total summary across every persisted session.
/// - `session_key` → that session's lifetime totals plus its capped
///   per-turn breakdown (whatever `record_turn_usage` retained, up to
///   ~200 rows).
/// - `since` (e.g. `7d`, `24h`, `45m`) → roll-up of every turn whose
///   `ts` is newer than the cutoff, across every session.
///
/// Output is always JSON so the agent can re-parse it without
/// guessing at column widths. Token field names match the CLI's
/// `--json` output so a downstream renderer can be reused.
fn token_usage(args: &Value) -> Result<String> {
    let cfg = zunel_config::load_config(None).context("loading config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults).context("workspace")?;
    let dir = workspace.join("sessions");

    if let Some(key) = args
        .get("session_key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
    {
        let Some(path) = session_path(&workspace, key) else {
            return Ok(serde_json::to_string(&json!({"found": false, "key": key}))?);
        };
        let (metadata, _messages) = read_session_file(&path)?;
        let total = read_usage_total(metadata.as_ref());
        let turns = read_usage_turns(metadata.as_ref());
        let turn_usage = read_turn_usage(metadata.as_ref());
        return Ok(serde_json::to_string(&json!({
            "found": true,
            "key": key,
            "turns": turns,
            "prompt_tokens": total.prompt,
            "completion_tokens": total.completion,
            "reasoning_tokens": total.reasoning,
            "cached_tokens": total.cached,
            "total_tokens": total.sum(),
            "turn_usage": turn_usage,
        }))?);
    }

    let cutoff = args
        .get("since")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|raw| {
            parse_cutoff(raw)
                .ok_or_else(|| anyhow::anyhow!("invalid since {raw:?} (try 7d, 24h, 45m)"))
        })
        .transpose()?;

    if !dir.exists() {
        return Ok(serde_json::to_string(&json!({
            "sessions": 0, "turns": 0,
            "prompt_tokens": 0, "completion_tokens": 0,
            "reasoning_tokens": 0, "cached_tokens": 0, "total_tokens": 0,
        }))?);
    }

    let mut grand = TokenTotal::default();
    let mut turns: u64 = 0;
    let mut sessions: usize = 0;
    let now = Local::now();
    let threshold = cutoff.map(|d| now - d);

    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let (metadata, _messages) = read_session_file(&entry.path())?;
        let metadata_ref = metadata.as_ref();
        if let Some(threshold) = threshold {
            let mut hits = 0u64;
            for row in read_turn_usage(metadata_ref) {
                let ts = match row.get("ts").and_then(Value::as_str).and_then(parse_ts) {
                    Some(t) => t,
                    None => continue,
                };
                if ts < threshold {
                    continue;
                }
                grand.add_row(&row);
                hits += 1;
            }
            if hits > 0 {
                sessions += 1;
                turns += hits;
            }
        } else {
            let total = read_usage_total(metadata_ref);
            if total.sum() == 0 {
                continue;
            }
            grand.add_total(&total);
            turns += read_usage_turns(metadata_ref);
            sessions += 1;
        }
    }

    let mut payload = json!({
        "sessions": sessions,
        "turns": turns,
        "prompt_tokens": grand.prompt,
        "completion_tokens": grand.completion,
        "reasoning_tokens": grand.reasoning,
        "cached_tokens": grand.cached,
        "total_tokens": grand.sum(),
    });
    if let Some(raw) = args.get("since").and_then(Value::as_str) {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("since".into(), json!(raw));
        }
    }
    Ok(serde_json::to_string(&payload)?)
}

#[derive(Default, Clone)]
struct TokenTotal {
    prompt: u64,
    completion: u64,
    reasoning: u64,
    cached: u64,
}

impl TokenTotal {
    fn sum(&self) -> u64 {
        self.prompt + self.completion + self.reasoning
    }
    fn add_total(&mut self, other: &TokenTotal) {
        self.prompt = self.prompt.saturating_add(other.prompt);
        self.completion = self.completion.saturating_add(other.completion);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.cached = self.cached.saturating_add(other.cached);
    }
    fn add_row(&mut self, row: &Value) {
        self.prompt = self.prompt.saturating_add(field(row, "prompt"));
        self.completion = self.completion.saturating_add(field(row, "completion"));
        self.reasoning = self.reasoning.saturating_add(field(row, "reasoning"));
        self.cached = self.cached.saturating_add(field(row, "cached"));
    }
}

fn field(row: &Value, key: &str) -> u64 {
    row.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn read_usage_total(metadata: Option<&Value>) -> TokenTotal {
    let total = match metadata
        .and_then(|m| m.get("metadata"))
        .and_then(|m| m.get("usage_total"))
    {
        Some(v) => v,
        None => return TokenTotal::default(),
    };
    TokenTotal {
        prompt: total
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        completion: total
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning: total
            .get("reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached: total
            .get("cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

fn read_usage_turns(metadata: Option<&Value>) -> u64 {
    metadata
        .and_then(|m| m.get("metadata"))
        .and_then(|m| m.get("usage_total"))
        .and_then(|t| t.get("turns"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn read_turn_usage(metadata: Option<&Value>) -> Vec<Value> {
    metadata
        .and_then(|m| m.get("metadata"))
        .and_then(|m| m.get("turn_usage"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn parse_ts(raw: &str) -> Option<DateTime<Local>> {
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .and_then(|n| n.and_local_timezone(Local).single())
}

fn parse_cutoff(raw: &str) -> Option<chrono::Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (num, unit) = raw.split_at(raw.len() - 1);
    let n: i64 = num.parse().ok()?;
    if n <= 0 {
        return None;
    }
    match unit {
        "d" | "D" => Some(chrono::Duration::days(n)),
        "h" | "H" => Some(chrono::Duration::hours(n)),
        "m" | "M" => Some(chrono::Duration::minutes(n)),
        _ => None,
    }
}

async fn send_message_to_channel(args: &Value) -> Result<String> {
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

/// Begin an OAuth login flow for a remote MCP server.
///
/// Wraps [`zunel_mcp::oauth::start_flow`] so the agent can post the
/// authorize URL into Slack and instruct the user to paste back the
/// redirect. Returns a JSON document the agent can render verbatim;
/// the `instructions` field is intentionally human-friendly so it can
/// be rewritten without a binary change as the chat UX evolves.
async fn mcp_login_start(args: &Value) -> Result<String> {
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

/// Finish a `mcp_login_start` flow by exchanging the pasted redirect
/// URL for an access token.
///
/// Wraps [`zunel_mcp::oauth::complete_flow`]. Returns either
/// `{ok: true, ...}` on success or `{ok: false, error: "..."}` on
/// any failure path so the agent doesn't have to special-case
/// non-zero error variants.
async fn mcp_login_complete(args: &Value) -> Result<String> {
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

fn call_args(msg: &Value) -> Value {
    msg.get("params")
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}))
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

/// Sanitise a session key into a filesystem-safe slug.
///
/// MUST stay in sync with `SessionManager::safe_key` in `zunel-core`,
/// which is the canonical writer side — if the two diverge, sessions
/// written by the agent loop become invisible to this MCP server.
/// The set of unsafe chars (`<>:"/\|?*`) is the Windows-reserved
/// filename set; every other Unicode codepoint passes through, so
/// non-ASCII session keys (`émoji🎉`) round-trip cleanly. Path
/// traversal via `..` is harmless here: `/` is filtered, so `..` can
/// only appear in the final basename, not as a directory traversal.
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

#[cfg(test)]
mod handler_protocol_tests {
    use super::*;

    #[tokio::test]
    async fn unknown_method_emits_jsonrpc_method_not_found_error() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "totally.bogus.method"
        });
        let resp = handle_message(&request).await.expect("response emitted");
        assert_eq!(resp["jsonrpc"], json!("2.0"));
        assert_eq!(resp["id"], json!(42));
        assert!(
            resp.get("result").is_none(),
            "unknown method must not be reported as success: {resp}"
        );
        let err = resp.get("error").expect("error envelope present");
        assert_eq!(err["code"], json!(-32601));
        assert!(err["message"]
            .as_str()
            .unwrap_or_default()
            .contains("totally.bogus.method"));
    }

    #[tokio::test]
    async fn notifications_still_emit_no_response() {
        let request = json!({"jsonrpc": "2.0", "method": "notifications/cancelled"});
        assert!(handle_message(&request).await.is_none());
    }

    #[tokio::test]
    async fn initialize_still_succeeds() {
        // Regression guard: the dispatch refactor mustn't break the
        // known-good methods on its way to fixing the unknown-method
        // contract.
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        let resp = handle_message(&request).await.unwrap();
        assert_eq!(resp["id"], json!(1));
        assert!(resp.get("result").is_some(), "{resp}");
        assert!(resp.get("error").is_none(), "{resp}");
    }

    #[test]
    fn safe_session_key_matches_canonical_session_manager_mapping() {
        // Must mirror `SessionManager::safe_key` in zunel-core, since
        // sessions written there end up at
        // `sessions/<safe_key>.jsonl` and this MCP server has to
        // resolve the same path back from a chat-supplied key.
        assert_eq!(super::safe_session_key("slack:DTEST"), "slack_DTEST");
        assert_eq!(super::safe_session_key("cli:direct"), "cli_direct");
        assert_eq!(super::safe_session_key("agent:foo:bar"), "agent_foo_bar");
        // Non-ASCII passes through unchanged (matches canonical behaviour).
        assert_eq!(super::safe_session_key("émoji🎉"), "émoji🎉");
        // Trailing whitespace is trimmed.
        assert_eq!(super::safe_session_key("  padded  "), "padded");
    }

    #[test]
    fn safe_session_key_neutralises_separators_and_metacharacters() {
        // Path separators and shell metacharacters become `_` so a
        // chat-supplied session key can't escape `sessions/`.
        assert_eq!(super::safe_session_key("a/b"), "a_b");
        assert_eq!(super::safe_session_key("a\\b"), "a_b");
        // `..` is harmless on its own because `/` is already filtered,
        // so `../etc/passwd` collapses to a file literally named
        // `.._etc_passwd.jsonl` inside `sessions/` — no traversal.
        assert_eq!(super::safe_session_key("../etc/passwd"), ".._etc_passwd");
        assert_eq!(super::safe_session_key("a*b?c"), "a_b_c");
    }

    #[test]
    fn walk_and_set_json_path_round_trip() {
        let mut tree = json!({"agents": {"defaults": {"model": "gpt-x"}}});
        // Read existing.
        assert_eq!(
            super::walk_json_path(&tree, "agents.defaults.model"),
            Some(&json!("gpt-x"))
        );
        // Read missing — must be None, not a panic.
        assert_eq!(super::walk_json_path(&tree, "missing"), None);
        // Set overwrites.
        super::set_json_path(&mut tree, "agents.defaults.model", json!("gpt-5")).unwrap();
        assert_eq!(
            super::walk_json_path(&tree, "agents.defaults.model"),
            Some(&json!("gpt-5"))
        );
        // Set creates intermediate objects.
        super::set_json_path(
            &mut tree,
            "tools.mcpServers.linear",
            json!({"type": "stdio"}),
        )
        .unwrap();
        assert_eq!(
            super::walk_json_path(&tree, "tools.mcpServers.linear.type"),
            Some(&json!("stdio"))
        );
    }

    #[test]
    fn remove_json_path_handles_present_and_missing() {
        let mut tree = json!({"tools": {"mcpServers": {"foo": {"x": 1}, "bar": {"y": 2}}}});
        assert!(super::remove_json_path(&mut tree, "tools.mcpServers.foo"));
        assert!(tree.pointer("/tools/mcpServers/foo").is_none());
        assert!(tree.pointer("/tools/mcpServers/bar").is_some());
        // Missing key returns false, doesn't mutate.
        assert!(!super::remove_json_path(&mut tree, "tools.mcpServers.foo"));
        // Non-existent parent path returns false.
        assert!(!super::remove_json_path(&mut tree, "nope.nada"));
    }

    #[test]
    fn validate_skill_name_rejects_traversal_and_metacharacters() {
        assert!(super::validate_skill_name("ok-name").is_ok());
        assert!(super::validate_skill_name("ok_name_123").is_ok());
        assert!(super::validate_skill_name("").is_err());
        assert!(super::validate_skill_name(".").is_err());
        assert!(super::validate_skill_name("..").is_err());
        assert!(super::validate_skill_name(".hidden").is_err());
        assert!(super::validate_skill_name("a/b").is_err());
        assert!(super::validate_skill_name("a\\b").is_err());
        assert!(super::validate_skill_name("a b").is_err());
        assert!(super::validate_skill_name("a.b").is_err());
        assert!(super::validate_skill_name("a*b").is_err());
    }
}
