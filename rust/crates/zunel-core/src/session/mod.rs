//! Session value type, time helpers, and submodules.
//!
//! Split into focused submodules so each concern lives in one obvious place:
//!
//! * [`storage`] — `SessionManager` (load/save/list/delete) and the
//!   compact JSON line formatter used for on-disk session files.
//! * [`usage`] — per-turn / lifetime token-usage accounting on the same
//!   `Session` value (exposed via [`Session::record_turn_usage`] and friends).
//!
//! This file owns the bare [`Session`] struct, [`ChatRole`] enum, and the
//! handful of timestamp helpers that are needed everywhere. Re-exports keep
//! the public `zunel_core::session::*` surface stable.

mod storage;
mod usage;

pub use storage::SessionManager;
pub use usage::MAX_TURN_USAGE_ENTRIES;

use chrono::{DateTime, Local, NaiveDateTime};
use serde_json::{json, Value};

/// Canonical chat role string mapping used across sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
    Tool,
}

impl ChatRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Tool => "tool",
        }
    }
}

/// Naive local timestamp, e.g. ``2026-04-24T10:58:27.123456``: local
/// wall-clock time, microsecond precision, no timezone suffix.
pub fn naive_local_iso_now() -> String {
    naive_local_iso(Local::now())
}

pub fn naive_local_iso(ts: DateTime<Local>) -> String {
    let naive: NaiveDateTime = ts.naive_local();
    naive.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
}

/// Parse a `naive_local_iso`-formatted timestamp back into a
/// timezone-aware `DateTime<Local>`. Returns `None` for malformed
/// inputs (e.g. ISO strings written by older zunel versions that
/// included a timezone suffix).
fn parse_naive_local(ts: &str) -> Option<DateTime<Local>> {
    NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .and_then(|naive| naive.and_local_timezone(Local).single())
}

/// A single conversation session.
#[derive(Debug, Clone)]
pub struct Session {
    pub(super) key: String,
    pub(super) messages: Vec<Value>,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) metadata: Value,
    pub(super) last_consolidated: usize,
}

impl Session {
    pub fn new(key: impl Into<String>) -> Self {
        let now = naive_local_iso_now();
        Self {
            key: key.into(),
            messages: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            metadata: Value::Object(Default::default()),
            last_consolidated: 0,
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn messages(&self) -> &[Value] {
        &self.messages
    }

    pub fn last_consolidated(&self) -> usize {
        self.last_consolidated
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }

    pub fn metadata(&self) -> &Value {
        &self.metadata
    }

    pub fn add_message(&mut self, role: ChatRole, content: impl Into<String>) {
        let msg = json!({
            "role": role.as_str(),
            "content": content.into(),
            "timestamp": naive_local_iso_now(),
        });
        self.messages.push(msg);
        self.updated_at = naive_local_iso_now();
    }

    /// Append a pre-built JSON message verbatim (apart from a
    /// `timestamp` field that is filled in if missing). Used by the
    /// runner for assistant tool-call messages (`content: null` +
    /// `tool_calls`) and tool-result messages, which `add_message`
    /// cannot express.
    pub fn append_raw_message(&mut self, mut value: Value) {
        if let Some(obj) = value.as_object_mut() {
            obj.entry("timestamp".to_string())
                .or_insert_with(|| Value::String(naive_local_iso_now()));
        }
        self.messages.push(value);
        self.updated_at = naive_local_iso_now();
    }

    /// Return up to ``max_messages`` of the most recent unconsolidated
    /// messages, stripped of the ``timestamp`` field (LLMs don't need it).
    pub fn get_history(&self, max_messages: usize) -> Vec<Value> {
        let unconsolidated = &self.messages[self.last_consolidated..];
        let start = unconsolidated.len().saturating_sub(max_messages);
        unconsolidated[start..]
            .iter()
            .map(|m| {
                let mut clone = m.clone();
                if let Some(obj) = clone.as_object_mut() {
                    obj.remove("timestamp");
                }
                clone
            })
            .collect()
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.last_consolidated = 0;
        self.updated_at = naive_local_iso_now();
    }

    /// Replace `messages[start..end]` with a single `summary` row.
    /// Used by `CompactionService::compact_session` to collapse an
    /// arbitrary slice of stale history into one row without disturbing
    /// the preserved tail.
    ///
    /// `last_consolidated` is moved to `start` — i.e., the inserted
    /// summary becomes the first row of replayable history so
    /// [`get_history`] still surfaces it to the LLM, while subsequent
    /// compaction passes naturally re-summarize the previous summary
    /// plus whatever has accumulated since.
    ///
    /// Out-of-range or non-monotonic slices are clamped to no-op: this
    /// is intentionally lenient so callers can pass `(consolidated, len)`
    /// without first re-validating after a concurrent save.
    pub fn replace_range_with_summary(&mut self, start: usize, end: usize, summary: Value) {
        if start >= self.messages.len() || end > self.messages.len() || start >= end {
            return;
        }
        let _: Vec<Value> = self.messages.drain(start..end).collect();
        self.messages.insert(start, summary);
        self.last_consolidated = start;
        self.updated_at = naive_local_iso_now();
    }

    /// Timestamp of the most recent user-turn row, parsed from the
    /// per-message `timestamp` field. Returns `None` when no user turn
    /// is recorded or every timestamp is malformed.
    pub fn last_user_turn_at(&self) -> Option<DateTime<Local>> {
        self.messages
            .iter()
            .rev()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .find_map(|m| m.get("timestamp").and_then(Value::as_str))
            .and_then(parse_naive_local)
    }

    /// Whole minutes elapsed since the last persisted user turn. `None`
    /// when no user turn is recorded so callers can short-circuit
    /// idle-compaction.
    pub fn idle_minutes_since_last_user_turn(&self) -> Option<i64> {
        let last = self.last_user_turn_at()?;
        let elapsed = Local::now() - last;
        Some(elapsed.num_minutes())
    }

    pub(crate) fn from_parts(
        key: String,
        messages: Vec<Value>,
        created_at: String,
        updated_at: String,
        metadata: Value,
        last_consolidated: usize,
    ) -> Self {
        Self {
            key,
            messages,
            created_at,
            updated_at,
            metadata,
            last_consolidated,
        }
    }

    pub(crate) fn metadata_line(&self) -> Value {
        json!({
            "_type": "metadata",
            "key": self.key,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "metadata": self.metadata,
            "last_consolidated": self.last_consolidated,
        })
    }

    /// Test-only constructor for deterministic fixtures.
    #[doc(hidden)]
    pub fn for_test(
        key: String,
        messages: Vec<Value>,
        created_at: String,
        updated_at: String,
    ) -> Self {
        Self {
            key,
            messages,
            created_at,
            updated_at,
            metadata: Value::Object(Default::default()),
            last_consolidated: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn iso_format_matches_python_shape() {
        let naive = NaiveDate::from_ymd_opt(2026, 4, 24)
            .unwrap()
            .and_hms_micro_opt(10, 58, 27, 123_456)
            .unwrap();
        let ts: DateTime<Local> = naive.and_local_timezone(Local).unwrap();
        let iso = naive_local_iso(ts);
        assert_eq!(iso, "2026-04-24T10:58:27.123456");
    }

    #[test]
    fn safe_key_matches_python_safe_filename() {
        // Colons become underscores (SessionManager's standard namespacing).
        assert_eq!(SessionManager::safe_key("cli:direct"), "cli_direct");
        assert_eq!(SessionManager::safe_key("agent:foo:bar"), "agent_foo_bar");
        // Windows-unsafe chars are stripped, everything else passes through.
        assert_eq!(
            SessionManager::safe_key("hello world.txt"),
            "hello world.txt"
        );
        assert_eq!(SessionManager::safe_key("dir/name"), "dir_name");
        assert_eq!(SessionManager::safe_key("a<b>c|d?e*f"), "a_b_c_d_e_f");
        assert_eq!(SessionManager::safe_key("émoji🎉"), "émoji🎉");
        assert_eq!(SessionManager::safe_key("  padded  "), "padded");
    }
}
