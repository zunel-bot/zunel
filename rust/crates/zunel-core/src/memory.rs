use std::{path::PathBuf, sync::Arc};

use crate::error::{Error, Result};
use crate::{AgentRunSpec, AgentRunner, AllowAllApprovalHandler, StopReason};
use tokio::sync::mpsc;
use zunel_config::DreamConfig;
use zunel_providers::{ChatMessage, GenerationSettings, LLMProvider};
use zunel_tools::{
    fs::{EditFileTool, ReadFileTool, WriteFileTool},
    path_policy::PathPolicy,
    ToolRegistry,
};

const DEFAULT_DREAM_BATCH_SIZE: usize = 20;
const DEFAULT_DREAM_MAX_ITERATIONS: usize = 10;

/// Phase-1 system prompt for the cheap analysis pass. Goes beyond the
/// historical one-liner ("Analyze history…") to: (a) name the criteria
/// for "durable", (b) constrain to facts present in the input, (c)
/// require explicit keep/add/update/remove decisions so phase 2 has
/// concrete edits to apply.
const DREAM_PHASE1_SYSTEM: &str = "\
You are the analysis half of Zunel's Dream memory-consolidation \
pipeline. Read the recent conversation slices in the history block \
and produce a short, structured analysis of what (if anything) \
belongs in long-term memory.

Durable knowledge is information that will still matter in 30 days: \
the user's stable preferences, recurring constraints, project goals, \
ongoing decisions, and named entities (people, repos, services) the \
user works with. Skip transient state, in-flight troubleshooting, \
and one-off chit-chat.

Ground every recommendation in something explicitly present in the \
history block. Do not infer beyond it, and do not invent facts.

Compare your candidate facts against the existing MEMORY.md / \
SOUL.md / USER.md content provided. For each candidate fact, decide \
explicitly: keep, add, update, or remove. Output a short list of \
those decisions in plain prose (no JSON, no headings — phase 2 will \
read your prose and apply the edits with file tools).";

/// Phase-2 system prompt for the edit runner.
const DREAM_PHASE2_SYSTEM: &str = "\
You are the edit half of Zunel's Dream memory pipeline. Apply the \
analysis below by editing MEMORY.md / SOUL.md / USER.md (and, if \
appropriate, files under skills/). Use the read_file, edit_file, \
and write_file tools — nothing else. Make every edit small and \
targeted; do not rewrite a file wholesale unless the analysis \
explicitly asks for that.

Only edit files inside this restricted set:
- memory/MEMORY.md
- SOUL.md
- USER.md
- skills/<name>/SKILL.md

If the analysis suggests no concrete edit, simply stop.";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub cursor: u64,
    pub timestamp: String,
    pub content: String,
}

/// Structured outcome of a single `DreamService::run` pass.
///
/// `processed_entries == 0` means there was nothing in
/// `memory/history.jsonl` past the last cursor (a no-op tick).
/// `edited_files` lists the tool names the phase-2 runner invoked
/// (typically `write_file` / `edit_file`); an empty vec on a non-zero
/// processed count means the model produced analysis but did not apply
/// any concrete edits.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DreamOutcome {
    pub processed_entries: usize,
    pub edited_files: Vec<String>,
    pub cursor_advanced_to: Option<u64>,
}

impl DreamOutcome {
    /// True when the run consumed input AND the model actually wrote
    /// at least one edit. Used by callers to decide whether to record
    /// `last_dream_at` (so a no-op pass doesn't suppress the next).
    pub fn is_active(&self) -> bool {
        self.processed_entries > 0 && !self.edited_files.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    workspace: PathBuf,
    max_history_entries: usize,
}

pub struct DreamService {
    store: MemoryStore,
    provider: Arc<dyn LLMProvider>,
    model: String,
    max_batch_size: usize,
    max_iterations: usize,
    annotate_line_ages: bool,
    /// Provider settings for the phase-1 analysis call AND the
    /// phase-2 edit runner. Defaults to a deterministic, low-cost
    /// profile (temperature 0, modest max-tokens) to match
    /// `CompactionService` rather than the silently-defaulted
    /// `GenerationSettings::default()` Dream used to ship with —
    /// that ignored any user-configured temperature/max_tokens.
    settings: GenerationSettings,
}

impl DreamService {
    pub fn new(store: MemoryStore, provider: Arc<dyn LLMProvider>, model: String) -> Self {
        Self {
            store,
            provider,
            model,
            max_batch_size: DEFAULT_DREAM_BATCH_SIZE,
            max_iterations: DEFAULT_DREAM_MAX_ITERATIONS,
            annotate_line_ages: false,
            settings: GenerationSettings {
                temperature: Some(0.0),
                max_tokens: Some(8192),
                reasoning_effort: None,
            },
        }
    }

    /// Override the per-pass [`GenerationSettings`]. The gateway
    /// scheduler passes the same settings the main agent uses (or a
    /// Dream-specific override) so a configured `temperature` /
    /// `max_tokens` actually reaches the analysis call.
    pub fn with_settings(mut self, settings: GenerationSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Apply user-facing `agents.defaults.dream` overrides.
    ///
    /// `model_override` swaps the analysis model (typically a cheaper
    /// one); the other knobs cap per-run cost and toggle the optional
    /// `[age=Nm]` annotation on each history line so the analysis
    /// model can reason about staleness.
    pub fn with_config(mut self, cfg: &DreamConfig) -> Self {
        if let Some(override_model) = cfg.model_override.as_ref() {
            if !override_model.is_empty() {
                self.model = override_model.clone();
            }
        }
        if let Some(batch) = cfg.max_batch_size {
            self.max_batch_size = (batch as usize).max(1);
        }
        if let Some(iters) = cfg.max_iterations {
            self.max_iterations = (iters as usize).max(1);
        }
        if let Some(annotate) = cfg.annotate_line_ages {
            self.annotate_line_ages = annotate;
        }
        self
    }

    pub fn with_max_batch_size(mut self, max_batch_size: usize) -> Self {
        self.max_batch_size = max_batch_size.max(1);
        self
    }

    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations.max(1);
        self
    }

    pub fn max_batch_size(&self) -> usize {
        self.max_batch_size
    }

    pub fn max_iterations(&self) -> usize {
        self.max_iterations
    }

    pub fn annotates_line_ages(&self) -> bool {
        self.annotate_line_ages
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn run(&self) -> Result<DreamOutcome> {
        let cursor = DreamCursor::new(self.store.workspace.clone());
        let last_cursor = cursor.read()?;
        let entries = self.store.read_unprocessed_history(last_cursor)?;
        if entries.is_empty() {
            return Ok(DreamOutcome::default());
        }
        let batch: Vec<HistoryEntry> = entries.into_iter().take(self.max_batch_size).collect();
        let processed_entries = batch.len();
        let now = chrono::Local::now();
        let history_text = batch
            .iter()
            .map(|entry| {
                let prefix = if self.annotate_line_ages {
                    let age = entry_age_minutes(now, &entry.timestamp);
                    format!("[{} | age={age}m]", entry.timestamp)
                } else {
                    format!("[{}]", entry.timestamp)
                };
                format!("{prefix} {}", entry.content)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let file_context = format!(
            "## Current MEMORY.md\n{}\n\n## Current SOUL.md\n{}\n\n## Current USER.md\n{}",
            empty_marker(self.store.read_memory()?),
            empty_marker(self.store.read_soul()?),
            empty_marker(self.store.read_user()?),
        );
        let phase1 = self
            .provider
            .generate(
                &self.model,
                &[
                    ChatMessage::system(DREAM_PHASE1_SYSTEM),
                    ChatMessage::user(format!(
                        "## Conversation History\n{history_text}\n\n{file_context}"
                    )),
                ],
                &[],
                &self.settings,
            )
            .await
            .map_err(Error::Provider)?;
        let analysis = phase1.content.unwrap_or_default();
        // Guard against unusable analyses — empty output or a few
        // stray tokens means the cheap analysis model returned
        // nothing actionable. Skip phase 2 and don't advance the
        // cursor so the batch is retried on the next pass.
        if analysis.trim().len() < 32 {
            tracing::warn!(
                bytes = analysis.trim().len(),
                "dream: phase 1 analysis too short; skipping phase 2 and leaving cursor untouched"
            );
            return Ok(DreamOutcome {
                processed_entries,
                edited_files: Vec::new(),
                cursor_advanced_to: None,
            });
        }

        let mut tools = ToolRegistry::new();
        // Tight policy: Dream may read/write only the durable-memory
        // surface — `memory/` (MEMORY.md, history.jsonl, .cursor),
        // `SOUL.md`, `USER.md`, and `skills/`. Everything else under
        // the workspace (sessions/, .zunel/, cron/) is off-limits so
        // a confused model cannot corrupt session logs, scheduler
        // state, or cron jobs.
        let memory_root = self.store.workspace.join("memory");
        let policy = PathPolicy::restricted(&memory_root)
            .with_allowed_extra(&self.store.workspace.join("SOUL.md"))
            .with_allowed_extra(&self.store.workspace.join("USER.md"))
            .with_allowed_extra(&self.store.workspace.join("skills"));
        tools.register(Arc::new(ReadFileTool::new(policy.clone())));
        tools.register(Arc::new(EditFileTool::new(policy.clone())));
        tools.register(Arc::new(WriteFileTool::new(policy)));
        let runner = AgentRunner::new(
            self.provider.clone(),
            Arc::new(tools),
            Arc::new(AllowAllApprovalHandler),
        );
        let (tx, _rx) = mpsc::channel(8);
        let result = runner
            .run(
                AgentRunSpec {
                    initial_messages: vec![
                        ChatMessage::system(DREAM_PHASE2_SYSTEM),
                        ChatMessage::user(format!(
                            "## Analysis Result\n{analysis}\n\n{file_context}"
                        )),
                    ],
                    model: self.model.clone(),
                    settings: self.settings.clone(),
                    max_iterations: self.max_iterations,
                    workspace: self.store.workspace.clone(),
                    session_key: "dream:memory".into(),
                    ..Default::default()
                },
                tx,
            )
            .await?;

        let new_cursor = batch
            .last()
            .map(|entry| entry.cursor)
            .unwrap_or(last_cursor);
        // Only advance the cursor when the model actually produced at
        // least one tool-driven edit AND reached `Completed`. A
        // `MaxIterations` outcome or a zero-edit return means the
        // analysis was unusable — retry the same batch on the next
        // tick rather than silently consuming the input.
        let made_edits =
            result.stop_reason == StopReason::Completed && !result.tools_used.is_empty();
        let cursor_advanced_to = if made_edits {
            cursor.write(new_cursor)?;
            self.store.compact_history()?;
            Some(new_cursor)
        } else {
            tracing::warn!(
                stop_reason = ?result.stop_reason,
                tools_used = ?result.tools_used,
                "dream: phase 2 made no edits — cursor not advanced; batch will be retried"
            );
            None
        };
        Ok(DreamOutcome {
            processed_entries,
            edited_files: result.tools_used.clone(),
            cursor_advanced_to,
        })
    }
}

impl MemoryStore {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            max_history_entries: 1000,
        }
    }

    pub fn with_max_history_entries(mut self, max_history_entries: usize) -> Self {
        self.max_history_entries = max_history_entries;
        self
    }

    pub fn read_memory(&self) -> Result<String> {
        read_file_or_empty(self.memory_path())
    }

    pub fn write_memory(&self, content: &str) -> Result<()> {
        write_file(self.memory_path(), content)
    }

    pub fn read_soul(&self) -> Result<String> {
        read_file_or_empty(self.soul_path())
    }

    pub fn write_soul(&self, content: &str) -> Result<()> {
        write_file(self.soul_path(), content)
    }

    pub fn read_user(&self) -> Result<String> {
        read_file_or_empty(self.user_path())
    }

    pub fn write_user(&self, content: &str) -> Result<()> {
        write_file(self.user_path(), content)
    }

    pub fn append_history(&self, content: &str) -> Result<u64> {
        let cursor = self.next_cursor()?;
        let entry = HistoryEntry {
            cursor,
            timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            content: strip_think(content.trim_end()),
        };
        let history_path = self.history_path();
        if let Some(parent) = history_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Session {
                path: parent.to_path_buf(),
                source: Box::new(source),
            })?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&history_path)
            .map_err(|source| Error::Session {
                path: history_path.clone(),
                source: Box::new(source),
            })?;
        use std::io::Write;
        writeln!(
            file,
            "{}",
            serde_json::to_string(&entry).unwrap_or_else(|_| "{}".into())
        )
        .map_err(|source| Error::Session {
            path: history_path,
            source: Box::new(source),
        })?;
        write_file(self.cursor_path(), &cursor.to_string())?;
        Ok(cursor)
    }

    pub fn read_unprocessed_history(&self, since_cursor: u64) -> Result<Vec<HistoryEntry>> {
        Ok(self
            .read_entries()?
            .into_iter()
            .filter(|entry| entry.cursor > since_cursor)
            .collect())
    }

    pub fn compact_history(&self) -> Result<()> {
        if self.max_history_entries == 0 {
            return Ok(());
        }
        let mut entries = self.read_entries()?;
        if entries.len() <= self.max_history_entries {
            return Ok(());
        }
        entries = entries.split_off(entries.len() - self.max_history_entries);
        let body = entries
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap_or_else(|_| "{}".into()))
            .collect::<Vec<_>>()
            .join("\n");
        let body = if body.is_empty() {
            String::new()
        } else {
            body + "\n"
        };
        write_file(self.history_path(), &body)
    }

    pub fn raw_archive(&self, messages: &[serde_json::Value]) -> Result<u64> {
        let formatted = messages
            .iter()
            .filter_map(|message| {
                let role = message.get("role").and_then(serde_json::Value::as_str)?;
                let content = message.get("content").and_then(serde_json::Value::as_str)?;
                (!content.is_empty()).then(|| format!("{}: {}", role.to_ascii_uppercase(), content))
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.append_history(&format!("[RAW] {} messages\n{}", messages.len(), formatted))
    }

    fn next_cursor(&self) -> Result<u64> {
        let cursor_path = self.cursor_path();
        if let Ok(raw) = std::fs::read_to_string(&cursor_path) {
            if let Ok(cursor) = raw.trim().parse::<u64>() {
                return Ok(cursor + 1);
            }
        }
        Ok(self
            .read_entries()?
            .into_iter()
            .map(|entry| entry.cursor)
            .max()
            .unwrap_or(0)
            + 1)
    }

    fn read_entries(&self) -> Result<Vec<HistoryEntry>> {
        let path = self.history_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&path).map_err(|source| Error::Session {
            path: path.clone(),
            source: Box::new(source),
        })?;
        Ok(raw
            .lines()
            .filter_map(|line| serde_json::from_str::<HistoryEntry>(line).ok())
            .collect())
    }

    fn memory_path(&self) -> PathBuf {
        self.workspace.join("memory").join("MEMORY.md")
    }

    fn history_path(&self) -> PathBuf {
        self.workspace.join("memory").join("history.jsonl")
    }

    fn cursor_path(&self) -> PathBuf {
        self.workspace.join("memory").join(".cursor")
    }

    fn soul_path(&self) -> PathBuf {
        self.workspace.join("SOUL.md")
    }

    fn user_path(&self) -> PathBuf {
        self.workspace.join("USER.md")
    }
}

pub struct DreamCursor {
    workspace: PathBuf,
}

fn read_file_or_empty(path: PathBuf) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&path).map_err(|source| Error::Session {
        path,
        source: Box::new(source),
    })
}

fn write_file(path: PathBuf, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Session {
            path: parent.to_path_buf(),
            source: Box::new(source),
        })?;
    }
    std::fs::write(&path, content).map_err(|source| Error::Session {
        path,
        source: Box::new(source),
    })
}

fn strip_think(content: &str) -> String {
    let mut out = String::new();
    let mut rest = content;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find("</think>") else {
            break;
        };
        rest = &rest[start + end + "</think>".len()..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn entry_age_minutes(now: chrono::DateTime<chrono::Local>, raw: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M")
        .ok()
        .and_then(|naive| naive.and_local_timezone(chrono::Local).single())
        .map(|then| (now - then).num_minutes().max(0))
        .unwrap_or(0)
}

fn empty_marker(content: String) -> String {
    if content.is_empty() {
        "(empty)".into()
    } else {
        content
    }
}

impl DreamCursor {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    pub fn read(&self) -> Result<u64> {
        let path = self.cursor_path();
        if !path.exists() {
            return Ok(0);
        }
        let raw = std::fs::read_to_string(&path).map_err(|source| Error::Session {
            path: path.clone(),
            source: Box::new(source),
        })?;
        Ok(raw.trim().parse().unwrap_or(0))
    }

    pub fn write(&self, offset: u64) -> Result<()> {
        let path = self.cursor_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Session {
                path: parent.to_path_buf(),
                source: Box::new(source),
            })?;
        }
        std::fs::write(&path, offset.to_string()).map_err(|source| Error::Session {
            path,
            source: Box::new(source),
        })
    }

    fn cursor_path(&self) -> PathBuf {
        self.workspace.join("memory").join(".dream_cursor")
    }
}
