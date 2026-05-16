//! Cron scheduler service.

use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, TimeZone, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct CronService {
    store_path: PathBuf,
    store: CronStore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronRunOutcome {
    pub job_id: String,
    pub ok: bool,
    pub error: Option<String>,
}

impl CronService {
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            store: CronStore::default(),
        }
    }

    pub fn load_due_jobs(&mut self, now_ms: u64) -> Result<Vec<CronJob>> {
        self.load_store()?;
        let mut changed = false;
        for job in &mut self.store.jobs {
            if job.enabled && job.state.next_run_at_ms.is_none() {
                job.state.next_run_at_ms = compute_next_run(&job.schedule, now_ms);
                changed = true;
            }
        }
        if changed {
            self.save_store()?;
        }
        Ok(self
            .store
            .jobs
            .iter()
            .filter(|job| {
                job.enabled && job.state.next_run_at_ms.is_some_and(|next| next <= now_ms)
            })
            .cloned()
            .collect())
    }

    pub fn record_success(&mut self, job_id: &str, run_at_ms: u64, duration_ms: u64) -> Result<()> {
        self.record_result(job_id, run_at_ms, duration_ms, Ok(()))
    }

    pub fn run_due_jobs_once<F>(
        &mut self,
        now_ms: u64,
        mut on_job: F,
    ) -> Result<Vec<CronRunOutcome>>
    where
        F: FnMut(&CronJob) -> std::result::Result<(), String>,
    {
        let due = self.load_due_jobs(now_ms)?;
        let mut outcomes = Vec::with_capacity(due.len());
        for job in due {
            match on_job(&job) {
                Ok(()) => {
                    self.record_result(&job.id, now_ms, 0, Ok(()))?;
                    outcomes.push(CronRunOutcome {
                        job_id: job.id,
                        ok: true,
                        error: None,
                    });
                }
                Err(error) => {
                    self.record_result(&job.id, now_ms, 0, Err(error.clone()))?;
                    outcomes.push(CronRunOutcome {
                        job_id: job.id,
                        ok: false,
                        error: Some(error),
                    });
                }
            }
        }
        Ok(outcomes)
    }

    fn record_result(
        &mut self,
        job_id: &str,
        run_at_ms: u64,
        duration_ms: u64,
        result: std::result::Result<(), String>,
    ) -> Result<()> {
        self.load_store()?;
        if let Some(job) = self.store.jobs.iter_mut().find(|job| job.id == job_id) {
            let (status, error) = match result {
                Ok(()) => ("ok".to_string(), None),
                Err(error) => ("error".to_string(), Some(error)),
            };
            job.state.last_run_at_ms = Some(run_at_ms);
            job.state.last_status = Some(status.clone());
            job.state.last_error = error.clone();
            job.updated_at_ms = run_at_ms;
            job.state.run_history.push(CronRunRecord {
                run_at_ms,
                status,
                duration_ms,
                error,
            });
            if job.state.run_history.len() > 20 {
                let excess = job.state.run_history.len() - 20;
                job.state.run_history.drain(0..excess);
            }
            if job.schedule.kind == "at" {
                if !job.delete_after_run {
                    job.enabled = false;
                    job.state.next_run_at_ms = None;
                }
            } else {
                job.state.next_run_at_ms = compute_next_run(&job.schedule, run_at_ms);
            }
        }
        self.store
            .jobs
            .retain(|job| !(job.id == job_id && job.delete_after_run));
        self.save_store()
    }

    fn load_store(&mut self) -> Result<()> {
        if !self.store_path.exists() {
            self.store = CronStore::default();
            return Ok(());
        }
        let raw = std::fs::read_to_string(&self.store_path)?;
        self.store = serde_json::from_str(&raw)?;
        Ok(())
    }

    fn save_store(&self) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic write: serialize → temp file → fsync → rename. A crash
        // mid-write previously left a truncated `cron.json` that the
        // next load_store would silently fail to parse, dropping every
        // scheduled job. The rename is atomic on POSIX, and `sync_all`
        // forces the bytes through before the entry-swap so a power
        // loss between rename and writeback doesn't publish a
        // 0-byte file.
        let body = serde_json::to_string_pretty(&self.store)?;
        let tmp = self.store_path.with_extension("json.tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            use std::io::Write;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.store_path)?;
        Ok(())
    }
}

fn compute_next_run(schedule: &CronSchedule, now_ms: u64) -> Option<u64> {
    match schedule.kind.as_str() {
        "at" => schedule.at_ms.filter(|at| *at > now_ms),
        "every" => schedule
            .every_ms
            .filter(|every| *every > 0)
            .map(|every| now_ms + every),
        "expr" | "cron" => compute_next_expr_run(schedule, now_ms),
        _ => None,
    }
}

fn compute_next_expr_run(schedule: &CronSchedule, now_ms: u64) -> Option<u64> {
    let expr = normalize_cron_expr(schedule.expr.as_deref()?)?;
    let parsed = Schedule::from_str(&expr).ok()?;
    let tz = schedule
        .tz
        .as_deref()
        .unwrap_or("UTC")
        .parse::<chrono_tz::Tz>()
        .ok()?;
    let now = millis_to_utc(now_ms)?;
    let local_now = now.with_timezone(&tz);
    parsed
        .after(&local_now)
        .next()
        .and_then(|next| u64::try_from(next.with_timezone(&Utc).timestamp_millis()).ok())
}

fn normalize_cron_expr(expr: &str) -> Option<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    match fields.len() {
        // Standard five-field cron expressions (minute hour dom mon dow).
        5 => Some(format!(
            "0 {} {} {} {} {} *",
            fields[0], fields[1], fields[2], fields[3], fields[4]
        )),
        6 => Some(format!(
            "{} {} {} {} {} {} *",
            fields[0], fields[1], fields[2], fields[3], fields[4], fields[5]
        )),
        7 => Some(expr.to_string()),
        _ => None,
    }
}

fn millis_to_utc(ms: u64) -> Option<DateTime<Utc>> {
    let seconds = i64::try_from(ms / 1_000).ok()?;
    let nanos = ((ms % 1_000) * 1_000_000) as u32;
    Utc.timestamp_opt(seconds, nanos).single()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CronStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub jobs: Vec<CronJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJob {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub schedule: CronSchedule,
    #[serde(default)]
    pub payload: CronPayload,
    #[serde(default)]
    pub state: CronJobState,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub delete_after_run: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CronSchedule {
    pub kind: String,
    #[serde(default, rename = "atMs")]
    pub at_ms: Option<u64>,
    #[serde(default, rename = "everyMs")]
    pub every_ms: Option<u64>,
    #[serde(default)]
    pub expr: Option<String>,
    #[serde(default)]
    pub tz: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronPayload {
    #[serde(default = "default_payload_kind")]
    pub kind: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub deliver: bool,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
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
pub struct CronJobState {
    #[serde(default)]
    pub next_run_at_ms: Option<u64>,
    #[serde(default)]
    pub last_run_at_ms: Option<u64>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub run_history: Vec<CronRunRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronRunRecord {
    pub run_at_ms: u64,
    pub status: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub error: Option<String>,
}

fn default_version() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

fn default_payload_kind() -> String {
    "agent_turn".into()
}
