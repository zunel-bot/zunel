//! Compact one-line token-usage footer formatter.
//!
//! Used at the outbound message boundary (Slack, CLI/REPL) when the
//! corresponding `showTokenFooter` config switch is on. Keeping the
//! formatting logic here ensures Slack and the terminal print
//! byte-identical strings — handy for screenshot/UX consistency and
//! for the `zunel tokens` CLI to reuse the same humanizer.
//!
//! Output shape:
//!
//! ```text
//! ─ 312 in · 4 out · 1.2k session
//! ```
//!
//! With reasoning tokens present:
//!
//! ```text
//! ─ 312 in · 4 out · 8.1k think · 1.2k session
//! ```
//!
//! - `in`  = `usage.prompt_tokens` for the just-completed turn.
//! - `out` = `usage.completion_tokens` for the just-completed turn.
//! - `think` = `usage.reasoning_tokens` for the turn (omitted when 0
//!   so non-reasoning models stay quiet).
//! - `session` = lifetime grand total (`prompt + completion + reasoning`)
//!   from `Session::usage_total`.
//!
//! The em-dash + middle dots are deliberately distinct from typical
//! model output so the footer is visually clear without needing color.

use zunel_providers::Usage;

/// Format a single-line footer summarizing this turn's token usage and
/// the session-lifetime total. `session_total` should already include
/// the just-completed turn (i.e. read after `Session::record_turn_usage`).
///
/// Returns an empty string when both `turn` and `session_total` are
/// fully zero — there is nothing meaningful to show, and printing
/// `─ 0 in · 0 out · 0 session` would just clutter the channel.
pub fn format_footer(turn: &Usage, session_total: &Usage) -> String {
    let session_total_tokens = session_total.total();
    if turn.prompt_tokens == 0
        && turn.completion_tokens == 0
        && turn.reasoning_tokens == 0
        && session_total_tokens == 0
    {
        return String::new();
    }

    format!(
        "─ {} · {} session",
        format_totals(turn),
        humanize(session_total_tokens)
    )
}

/// Format the breakdown portion of a `Usage` value as
/// `"{prompt} in · {comp} out [· {reasoning} think]"` — no leading
/// separator, no trailing aggregate. Used both as the turn-side of
/// [`format_footer`] and for stand-alone "lifetime totals" lines in
/// the `zunel tokens` CLI, so any cosmetic change happens in one
/// place.
///
/// Reasoning is omitted when zero, matching the footer convention so
/// non-reasoning models stay quiet. Prompt/completion are always
/// emitted so a zero turn still renders as `"0 in · 0 out"` (callers
/// that want to suppress that should check `Usage::total() == 0`
/// before calling).
pub fn format_totals(usage: &Usage) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    parts.push(format!("{} in", humanize(u64::from(usage.prompt_tokens))));
    parts.push(format!(
        "{} out",
        humanize(u64::from(usage.completion_tokens))
    ));
    if usage.reasoning_tokens > 0 {
        parts.push(format!(
            "{} think",
            humanize(u64::from(usage.reasoning_tokens))
        ));
    }
    parts.join(" · ")
}

/// Compact human-readable count: <1k stays as the raw integer, >=1k
/// renders as `1.2k`, >=1M as `3.4M`. Public so the `zunel tokens`
/// CLI can use the same scale rules as the inline footer.
pub fn humanize(n: u64) -> String {
    if n >= 1_000_000 {
        let v = n as f64 / 1_000_000.0;
        format_decimal(v, "M")
    } else if n >= 1_000 {
        let v = n as f64 / 1_000.0;
        format_decimal(v, "k")
    } else {
        n.to_string()
    }
}

fn format_decimal(v: f64, suffix: &str) -> String {
    // One decimal place is the right balance between resolution and
    // line length — `12.4k` reads better than `12345` and is more
    // honest than `12k`. We strip a trailing `.0` so round numbers
    // (`8.0k`) collapse to `8k`.
    let s = format!("{v:.1}");
    if let Some(stripped) = s.strip_suffix(".0") {
        format!("{stripped}{suffix}")
    } else {
        format!("{s}{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_handles_small_k_and_m() {
        assert_eq!(humanize(0), "0");
        assert_eq!(humanize(312), "312");
        assert_eq!(humanize(999), "999");
        assert_eq!(humanize(1_000), "1k");
        assert_eq!(humanize(1_200), "1.2k");
        assert_eq!(humanize(12_400), "12.4k");
        assert_eq!(humanize(1_000_000), "1M");
        assert_eq!(humanize(3_450_000), "3.5M");
    }

    #[test]
    fn footer_includes_reasoning_only_when_nonzero() {
        let turn = Usage {
            prompt_tokens: 312,
            completion_tokens: 4,
            cached_tokens: 0,
            reasoning_tokens: 0,
        };
        let total = Usage {
            prompt_tokens: 1_100,
            completion_tokens: 100,
            cached_tokens: 0,
            reasoning_tokens: 0,
        };
        assert_eq!(
            format_footer(&turn, &total),
            "─ 312 in · 4 out · 1.2k session"
        );

        let turn_with_think = Usage {
            prompt_tokens: 312,
            completion_tokens: 4,
            cached_tokens: 0,
            reasoning_tokens: 8_100,
        };
        let total = Usage {
            prompt_tokens: 1_100,
            completion_tokens: 100,
            cached_tokens: 0,
            reasoning_tokens: 8_100,
        };
        assert_eq!(
            format_footer(&turn_with_think, &total),
            "─ 312 in · 4 out · 8.1k think · 9.3k session"
        );
    }

    #[test]
    fn footer_empty_for_all_zeros() {
        assert_eq!(format_footer(&Usage::default(), &Usage::default()), "");
    }

    #[test]
    fn format_totals_renders_breakdown_without_separators_or_session_suffix() {
        let usage = Usage {
            prompt_tokens: 1_100,
            completion_tokens: 100,
            cached_tokens: 0,
            reasoning_tokens: 8_100,
        };
        assert_eq!(format_totals(&usage), "1.1k in · 100 out · 8.1k think");
    }

    #[test]
    fn format_totals_omits_reasoning_when_zero() {
        let usage = Usage {
            prompt_tokens: 1_100,
            completion_tokens: 100,
            cached_tokens: 0,
            reasoning_tokens: 0,
        };
        assert_eq!(format_totals(&usage), "1.1k in · 100 out");
    }

    #[test]
    fn format_totals_zeroes_render_explicitly() {
        // Caller-managed: format_totals always emits in/out for
        // single-call clarity. Suppression is the caller's job.
        assert_eq!(format_totals(&Usage::default()), "0 in · 0 out");
    }
}
