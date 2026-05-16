//! On-demand and idle-triggered LLM summarization for long sessions.
//!
//! A `Session`'s `messages[last_consolidated..len-keep_tail]` slice is
//! collapsed into a single `system` row containing a "Prior conversation
//! summary" produced by the provider; `last_consolidated` then advances
//! past that new summary row so subsequent
//! `Session::get_history(...)` only replays the summary plus the
//! preserved tail.
//!
//! The transformation is reversible only via JSONL backup — callers
//! keep the original by snapshotting the file before invoking
//! `compact_session` (the `zunel sessions compact` CLI does this).

use std::sync::Arc;

use serde_json::{json, Value};

use zunel_providers::{ChatMessage, GenerationSettings, LLMProvider};
use zunel_util::truncate_at_char_boundary;

use crate::error::{Error, Result};
use crate::session::{naive_local_iso_now, Session};

const SUMMARY_SYSTEM_PROMPT: &str = "\
You compress a chat transcript so a downstream agent can keep context \
without re-reading every turn. Preserve: open tasks, decisions made, \
file paths and command names referenced, and unresolved questions. \
Drop chit-chat and verbose tool output. Reply with a single dense \
prose paragraph; no bullet lists, no headings, no preamble.";

const SUMMARY_PREFIX: &str = "[Prior conversation summary]\n";

pub struct CompactionService {
    provider: Arc<dyn LLMProvider>,
    model: String,
    settings: GenerationSettings,
}

impl CompactionService {
    pub fn new(provider: Arc<dyn LLMProvider>, model: String) -> Self {
        Self {
            provider,
            model,
            // Cheap, deterministic summarization; reasoning_effort left
            // unset so providers that gate on it stay fast.
            settings: GenerationSettings {
                temperature: Some(0.0),
                max_tokens: Some(2048),
                reasoning_effort: None,
            },
        }
    }

    pub fn with_settings(mut self, settings: GenerationSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Single-shot summarization used by both the idle trigger and the
    /// `zunel sessions compact` CLI. The transcript is rendered as
    /// numbered `ROLE: content` lines so the model knows what each
    /// snippet is even though tool-result JSON has been stripped down.
    pub async fn summarize(&self, messages: &[Value]) -> Result<String> {
        if messages.is_empty() {
            return Ok(String::new());
        }
        let transcript = render_transcript(messages);
        let prompt = format!(
            "Summarize the following conversation slice:\n\n{transcript}\n\nWrite the summary now."
        );
        let response = self
            .provider
            .generate(
                &self.model,
                &[
                    ChatMessage::system(SUMMARY_SYSTEM_PROMPT),
                    ChatMessage::user(prompt),
                ],
                &[],
                &self.settings,
            )
            .await
            .map_err(Error::Provider)?;
        let body = response.content.unwrap_or_default();
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return Err(Error::Other("compaction summary was empty".into()));
        }
        Ok(trimmed.to_string())
    }

    /// Replace the unconsolidated head of `session` (everything older
    /// than the last `keep_tail` messages) with a single system summary
    /// row, then bump `last_consolidated` past it. Returns the number
    /// of source messages that were collapsed.
    ///
    /// Returns `Ok(0)` when there is nothing to compact (history
    /// shorter than `keep_tail + 2`), so callers can treat it as a
    /// no-op without raising.
    pub async fn compact_session(&self, session: &mut Session, keep_tail: usize) -> Result<usize> {
        let total = session.messages().len();
        let consolidated = session.last_consolidated();
        // Need at least 2 stale messages to make a summary worthwhile.
        if total <= consolidated + keep_tail.saturating_add(2) {
            return Ok(0);
        }
        let stale_end = total.saturating_sub(keep_tail);
        let stale = session.messages()[consolidated..stale_end].to_vec();
        if stale.is_empty() {
            return Ok(0);
        }
        let stale_count = stale.len();
        let summary_body = self.summarize(&stale).await?;
        let summary = json!({
            "role": "system",
            "content": format!("{SUMMARY_PREFIX}{summary_body}"),
            "timestamp": naive_local_iso_now(),
        });
        session.replace_range_with_summary(consolidated, stale_end, summary);
        Ok(stale_count)
    }
}

fn render_transcript(messages: &[Value]) -> String {
    let mut out = String::new();
    for (i, msg) in messages.iter().enumerate() {
        let role = msg
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_uppercase();
        let content = msg
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('\n', " ");
        let trimmed = match truncate_at_char_boundary(&content, 1500) {
            (prefix, true) => format!("{prefix}…"),
            (_, false) => content,
        };
        let tool_name = msg.get("name").and_then(Value::as_str).unwrap_or("");
        if !tool_name.is_empty() {
            out.push_str(&format!("{}. {role} ({tool_name}): {trimmed}\n", i + 1));
        } else {
            out.push_str(&format!("{}. {role}: {trimmed}\n", i + 1));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::render_transcript;
    use serde_json::json;

    /// Regression guard: `render_transcript` slices at a fixed byte
    /// offset (1500) and must never panic on a multi-byte character
    /// straddling that boundary. Without `truncate_at_char_boundary`,
    /// box-drawing characters like U+2500 (3 bytes) can land across
    /// the cut and panic, killing the in-flight worker.
    #[test]
    fn render_transcript_does_not_panic_on_multibyte_cut_at_1500() {
        // 1499 ASCII bytes + one '─' (U+2500, 3 bytes).
        // Cut at 1500 lands in the middle of the box-drawing char.
        let mut content = String::with_capacity(1502);
        content.push_str(&"a".repeat(1499));
        content.push('─');
        let messages = vec![json!({
            "role": "user",
            "content": content,
        })];

        let rendered = render_transcript(&messages);

        assert!(rendered.contains("…"), "rendered: {rendered}");
        assert!(rendered.contains("USER"), "rendered: {rendered}");
    }

    #[test]
    fn render_transcript_preserves_short_messages_verbatim() {
        let messages = vec![json!({
            "role": "assistant",
            "content": "hello world",
        })];
        let rendered = render_transcript(&messages);
        assert!(rendered.contains("ASSISTANT: hello world"));
        assert!(!rendered.contains("…"));
    }
}
