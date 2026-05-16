use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::{Tool, ToolContext, ToolResult};

pub struct CronTool {
    state_path: PathBuf,
    default_timezone: &'static str,
}

impl CronTool {
    pub fn new(state_path: PathBuf, default_timezone: &'static str) -> Self {
        Self {
            state_path,
            default_timezone,
        }
    }

    fn load_store(&self) -> Result<CronStore> {
        if !self.state_path.exists() {
            return Ok(CronStore::default());
        }
        let raw = std::fs::read_to_string(&self.state_path).map_err(|source| Error::Io {
            path: self.state_path.clone(),
            source,
        })?;
        if raw.trim_start().starts_with('[') {
            let jobs: Vec<CronJob> = serde_json::from_str(&raw).map_err(|source| Error::Json {
                tool: "cron".into(),
                action: "parse",
                source,
            })?;
            return Ok(CronStore { version: 1, jobs });
        }
        serde_json::from_str(&raw).map_err(|source| Error::Json {
            tool: "cron".into(),
            action: "parse",
            source,
        })
    }

    fn save_store(&self, store: &CronStore) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let raw = serde_json::to_string_pretty(store).map_err(|source| Error::Json {
            tool: "cron".into(),
            action: "encode",
            source,
        })?;
        std::fs::write(&self.state_path, raw).map_err(|source| Error::Io {
            path: self.state_path.clone(),
            source,
        })
    }

    fn add_job(&self, args: &Value) -> ToolResult {
        let message = args
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if message.is_empty() {
            return ToolResult::err("message is required when action='add'");
        }
        let schedule = match parse_schedule(args, self.default_timezone) {
            Ok(schedule) => schedule,
            Err(err) => return ToolResult::err(err),
        };
        let mut store = match self.load_store() {
            Ok(store) => store,
            Err(err) => return ToolResult::err(err.to_string()),
        };
        let id = format!("job_{}", unix_millis());
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| message.chars().take(30).collect());
        let now = unix_millis() as u64;
        store.jobs.push(CronJob {
            id: id.clone(),
            name: name.clone(),
            enabled: true,
            schedule,
            payload: CronPayload {
                kind: "agent_turn".into(),
                message: message.to_string(),
                deliver: args.get("deliver").and_then(Value::as_bool).unwrap_or(true),
                channel: None,
                to: None,
            },
            state: CronJobState::default(),
            created_at_ms: now,
            updated_at_ms: now,
            delete_after_run: args.get("at").is_some(),
        });
        if let Err(err) = self.save_store(&store) {
            return ToolResult::err(err.to_string());
        }
        ToolResult::ok(format!("Created job '{name}' (id: {id})"))
    }

    fn list_jobs(&self) -> ToolResult {
        let store = match self.load_store() {
            Ok(store) => store,
            Err(err) => return ToolResult::err(err.to_string()),
        };
        if store.jobs.is_empty() {
            return ToolResult::ok("No scheduled jobs.");
        }
        let lines: Vec<String> = store
            .jobs
            .iter()
            .map(|job| {
                let mut line = format!(
                    "- {} (id: {}, {})",
                    job.name,
                    job.id,
                    format_schedule(&job.schedule)
                );
                if job.is_protected() {
                    line.push_str("\n  Protected: visible for inspection, but cannot be removed.");
                }
                line
            })
            .collect();
        ToolResult::ok(format!("Scheduled jobs:\n{}", lines.join("\n")))
    }

    fn remove_job(&self, args: &Value) -> ToolResult {
        let job_id = args
            .get("job_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if job_id.is_empty() {
            return ToolResult::err("job_id is required when action='remove'");
        }
        let mut store = match self.load_store() {
            Ok(store) => store,
            Err(err) => return ToolResult::err(err.to_string()),
        };
        if let Some(job) = store.jobs.iter().find(|job| job.id == job_id) {
            if job.is_protected() {
                return ToolResult::err("Protected system job cannot be removed.");
            }
        } else {
            return ToolResult::err(format!("No job found with id: {job_id}"));
        }
        store.jobs.retain(|job| job.id != job_id);
        if let Err(err) = self.save_store(&store) {
            return ToolResult::err(err.to_string());
        }
        ToolResult::ok(format!("Removed job {job_id}"))
    }
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &'static str {
        "cron"
    }

    fn description(&self) -> &'static str {
        "Schedule reminders and recurring tasks. Actions: add, list, remove. This Rust slice stores cron jobs but does not run a scheduler."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "remove"]},
                "name": {"type": "string"},
                "message": {"type": "string"},
                "every_seconds": {"type": "integer"},
                "cron_expr": {"type": "string"},
                "tz": {"type": "string"},
                "at": {"type": "string"},
                "deliver": {"type": "boolean"},
                "job_id": {"type": "string"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        match args.get("action").and_then(Value::as_str) {
            Some("add") => self.add_job(&args),
            Some("list") => self.list_jobs(),
            Some("remove") => self.remove_job(&args),
            Some(other) => ToolResult::err(format!("Unknown cron action: {other}")),
            None => ToolResult::err("action is required"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CronStore {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    jobs: Vec<CronJob>,
}

impl Default for CronStore {
    fn default() -> Self {
        Self {
            version: 1,
            jobs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CronJob {
    id: String,
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    schedule: CronSchedule,
    #[serde(default)]
    payload: CronPayload,
    #[serde(default)]
    state: CronJobState,
    #[serde(default)]
    created_at_ms: u64,
    #[serde(default)]
    updated_at_ms: u64,
    #[serde(default)]
    delete_after_run: bool,
}

impl CronJob {
    fn is_protected(&self) -> bool {
        self.payload.kind == "system_event" || self.name == "dream"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CronPayload {
    #[serde(default = "default_payload_kind")]
    kind: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    deliver: bool,
    channel: Option<String>,
    to: Option<String>,
}

impl Default for CronPayload {
    fn default() -> Self {
        Self {
            kind: default_payload_kind(),
            message: String::new(),
            deliver: false,
            channel: None,
            to: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CronJobState {
    next_run_at_ms: Option<u64>,
    last_run_at_ms: Option<u64>,
    last_status: Option<String>,
    last_error: Option<String>,
    #[serde(default)]
    run_history: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
enum CronSchedule {
    #[serde(rename = "every")]
    Every {
        #[serde(rename = "everyMs")]
        every_ms: u64,
    },
    #[serde(rename = "cron")]
    Cron { expr: String, tz: Option<String> },
    #[serde(rename = "at")]
    At {
        #[serde(rename = "atMs")]
        at_ms: u64,
    },
}

fn parse_schedule(
    args: &Value,
    default_timezone: &str,
) -> std::result::Result<CronSchedule, String> {
    if let Some(seconds) = args.get("every_seconds").and_then(Value::as_u64) {
        if seconds == 0 {
            return Err("every_seconds must be greater than 0".into());
        }
        return Ok(CronSchedule::Every {
            every_ms: seconds * 1000,
        });
    }
    if let Some(expr) = args.get("cron_expr").and_then(Value::as_str) {
        let tz = args
            .get("tz")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(default_timezone.to_string()));
        return Ok(CronSchedule::Cron {
            expr: expr.to_string(),
            tz,
        });
    }
    if let Some(at) = args.get("at").and_then(Value::as_str) {
        if !at.contains('T') {
            return Err("at must be an ISO datetime like 2026-04-24T10:30:00".into());
        }
        return Ok(CronSchedule::At {
            at_ms: unix_millis() as u64,
        });
    }
    Err("either every_seconds, cron_expr, or at is required".into())
}

fn format_schedule(schedule: &CronSchedule) -> String {
    match schedule {
        CronSchedule::Every { every_ms } => format!("every {}s", every_ms / 1000),
        CronSchedule::Cron { expr, tz } => match tz {
            Some(tz) => format!("cron: {expr} ({tz})"),
            None => format!("cron: {expr}"),
        },
        CronSchedule::At { at_ms } => format!("at {at_ms}"),
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn default_true() -> bool {
    true
}

fn default_version() -> u32 {
    1
}

fn default_payload_kind() -> String {
    "agent_turn".into()
}
