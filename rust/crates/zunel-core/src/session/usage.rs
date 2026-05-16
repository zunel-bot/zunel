//! Per-turn / lifetime token-usage accounting attached to [`Session`].
//!
//! The on-disk representation lives on `metadata` so it round-trips with the
//! existing JSONL format without a schema bump:
//!
//! * `metadata.usage_total` — `{prompt_tokens, completion_tokens,
//!   reasoning_tokens, cached_tokens, turns}`. Always-growing counters.
//! * `metadata.turn_usage` — capped array of `{ts, prompt, completion,
//!   reasoning, cached}` rows for `zunel tokens show <key>`.
//!
//! Older `turn_usage` rows roll into `usage_total` automatically when the
//! cap kicks in, so no precision is lost.

use serde_json::{json, Map, Value};
use zunel_providers::Usage;

use super::{naive_local_iso_now, Session};

/// Cap on the number of per-turn usage rows kept in `metadata.turn_usage`.
/// Older turns roll into the running `metadata.usage_total` aggregate so the
/// session file size stays bounded even on long-lived chats. ~6 weeks of
/// typical Slack DM throughput in the user's environment.
pub const MAX_TURN_USAGE_ENTRIES: usize = 200;

impl Session {
    /// Add a single turn's [`Usage`] onto this session's running totals.
    ///
    /// Updates two things under `self.metadata`:
    ///
    /// * `usage_total`: an always-growing counter object
    ///   `{ prompt_tokens, completion_tokens, reasoning_tokens, cached_tokens, turns }`.
    ///   Rolls over older `turn_usage` entries automatically — no
    ///   precision is lost when the cap kicks in.
    /// * `turn_usage`: a capped array (last [`MAX_TURN_USAGE_ENTRIES`])
    ///   of `{ ts, prompt, completion, reasoning, cached }` rows for
    ///   the `zunel tokens show <key>` per-turn breakdown.
    ///
    /// Skips persistence when every counter is zero (e.g. providers
    /// that don't report usage) so we don't bloat metadata with empty
    /// rows. Resilient to externally-mutated metadata: a non-object
    /// `usage_total` or non-array `turn_usage` is silently replaced
    /// rather than panicking.
    pub fn record_turn_usage(&mut self, usage: &Usage) {
        if usage.prompt_tokens == 0
            && usage.completion_tokens == 0
            && usage.reasoning_tokens == 0
            && usage.cached_tokens == 0
        {
            return;
        }

        let metadata = match self.metadata.as_object_mut() {
            Some(obj) => obj,
            None => {
                self.metadata = Value::Object(Map::new());
                match self.metadata.as_object_mut() {
                    Some(obj) => obj,
                    None => return,
                }
            }
        };

        let total_obj = metadata
            .entry("usage_total".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !total_obj.is_object() {
            *total_obj = Value::Object(Map::new());
        }
        if let Some(total) = total_obj.as_object_mut() {
            bump_u64(total, "prompt_tokens", usage.prompt_tokens as u64);
            bump_u64(total, "completion_tokens", usage.completion_tokens as u64);
            bump_u64(total, "reasoning_tokens", usage.reasoning_tokens as u64);
            bump_u64(total, "cached_tokens", usage.cached_tokens as u64);
            bump_u64(total, "turns", 1);
        }

        let turn_usage = metadata
            .entry("turn_usage".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !turn_usage.is_array() {
            *turn_usage = Value::Array(Vec::new());
        }
        if let Some(arr) = turn_usage.as_array_mut() {
            arr.push(json!({
                "ts": naive_local_iso_now(),
                "prompt": usage.prompt_tokens,
                "completion": usage.completion_tokens,
                "reasoning": usage.reasoning_tokens,
                "cached": usage.cached_tokens,
            }));
            // Drop the head if we exceeded the cap. The dropped rows are
            // already counted in `usage_total`, so no information is lost.
            let overflow = arr.len().saturating_sub(MAX_TURN_USAGE_ENTRIES);
            if overflow > 0 {
                arr.drain(..overflow);
            }
        }

        self.updated_at = naive_local_iso_now();
    }

    /// Read back the lifetime token totals previously recorded via
    /// [`record_turn_usage`]. Returns `Usage::default()` when no turns
    /// have been recorded yet, so callers can always treat the result
    /// as additive.
    pub fn usage_total(&self) -> Usage {
        let total = match self.metadata.get("usage_total").and_then(Value::as_object) {
            Some(obj) => obj,
            None => return Usage::default(),
        };
        Usage {
            prompt_tokens: total
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            completion_tokens: total
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            cached_tokens: total
                .get("cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            reasoning_tokens: total
                .get("reasoning_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        }
    }

    /// Number of turns recorded via [`record_turn_usage`]. Useful for
    /// `zunel tokens` row counts without having to re-load the full
    /// `turn_usage` array.
    pub fn usage_turns(&self) -> u64 {
        self.metadata
            .get("usage_total")
            .and_then(|v| v.get("turns"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    /// Per-turn usage rows previously appended via [`record_turn_usage`].
    /// Returns an empty slice when the metadata key is missing or has
    /// the wrong shape.
    pub fn turn_usage(&self) -> &[Value] {
        self.metadata
            .get("turn_usage")
            .and_then(Value::as_array)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

/// Saturating `+= delta` on a JSON object slot, treating missing or
/// non-numeric values as 0. Keeps the slot stored as `Number(u64)` so
/// the on-disk metadata stays compact.
fn bump_u64(map: &mut Map<String, Value>, key: &str, delta: u64) {
    let current = map.get(key).and_then(Value::as_u64).unwrap_or(0);
    map.insert(key.to_string(), Value::from(current.saturating_add(delta)));
}
