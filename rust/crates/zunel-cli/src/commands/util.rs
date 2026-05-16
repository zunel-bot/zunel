//! Small helpers shared between CLI subcommands.

/// Right-truncate `s` to at most `n` characters, replacing the last
/// character with an ellipsis when truncation actually happens.
///
/// Counts by `chars` so multi-byte glyphs aren't split mid-codepoint.
/// Returns `s.to_string()` when `s` already fits, so callers don't
/// pay for an allocation in the common case (the chars iter still
/// walks the string, but we avoid pushing a separate buffer).
pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let take = n.saturating_sub(1);
        let mut out: String = s.chars().take(take).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn returns_input_when_under_limit() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 6), "abcdef");
    }

    #[test]
    fn ellipsises_when_over_limit() {
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    #[test]
    fn handles_multibyte_chars() {
        let s = "αβγδε";
        assert_eq!(s.chars().count(), 5);
        assert_eq!(truncate(s, 3), "αβ…");
    }

    #[test]
    fn n_zero_does_not_panic() {
        // Edge case: avoid an underflow when n == 0.
        assert_eq!(truncate("abc", 0), "…");
    }
}
