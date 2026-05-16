//! UTF-8-safe string helpers.
//!
//! These exist because `&s[..n]` panics when `n` lands inside a
//! multi-byte UTF-8 character — which is easy to trigger in practice
//! once chat history contains non-ASCII content (box-drawing
//! characters from terminal banners, emoji, CJK, …). Every call site
//! that wants a byte-budget-bounded prefix should go through here.

/// Truncate `s` to at most `max_bytes` of UTF-8 without ever splitting
/// a multi-byte character. Returns the (prefix, was_truncated) pair so
/// callers can decide whether to append an ellipsis.
///
/// When `s.len() <= max_bytes` the original slice is returned
/// unchanged with `was_truncated = false`. When truncation is needed,
/// the cut walks back from `max_bytes` to the nearest char boundary,
/// so the returned prefix may be slightly shorter than `max_bytes`.
///
/// `max_bytes = 0` always yields `("", true)` (the input was rejected
/// in full); the only `max_bytes = 0` non-truncating case is the
/// empty string.
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    (&s[..idx], true)
}

#[cfg(test)]
mod tests {
    use super::truncate_at_char_boundary;

    #[test]
    fn ascii_under_budget_passes_through_unchanged() {
        let (prefix, truncated) = truncate_at_char_boundary("hello", 80);
        assert_eq!(prefix, "hello");
        assert!(!truncated);
    }

    #[test]
    fn ascii_at_budget_does_not_truncate() {
        let s = "x".repeat(80);
        let (prefix, truncated) = truncate_at_char_boundary(&s, 80);
        assert_eq!(prefix.len(), 80);
        assert!(!truncated);
    }

    #[test]
    fn ascii_over_budget_truncates_at_byte_index() {
        let s = "x".repeat(100);
        let (prefix, truncated) = truncate_at_char_boundary(&s, 80);
        assert_eq!(prefix.len(), 80);
        assert!(truncated);
    }

    /// Regression: this exact input panicked at `&content[..1500]` in
    /// `compaction.rs` before the fix. Terminal banners that use
    /// U+2500 BOX DRAWINGS LIGHT HORIZONTAL ('─') — 3 UTF-8 bytes —
    /// can land any byte cut inside one of those three bytes, which
    /// blows up `&s[..n]`.
    #[test]
    fn cut_inside_multibyte_character_walks_back_to_boundary() {
        // 1499 single-byte chars + one '─' (3 bytes spanning 1499..1502).
        let mut s = String::with_capacity(1502);
        s.push_str(&"a".repeat(1499));
        s.push('─');
        assert_eq!(s.len(), 1502);
        assert!(!s.is_char_boundary(1500), "test setup invariant");

        let (prefix, truncated) = truncate_at_char_boundary(&s, 1500);
        assert!(truncated);
        // We walked back from 1500 → 1499 (the next boundary below).
        assert_eq!(prefix.len(), 1499);
        assert_eq!(prefix, "a".repeat(1499));
    }

    #[test]
    fn all_multibyte_content_truncates_at_a_boundary() {
        // Each 'み' is 3 bytes → 30 chars × 3 = 90 bytes.
        let s = "み".repeat(30);
        let (prefix, truncated) = truncate_at_char_boundary(&s, 50);
        assert!(truncated);
        // 50 → walk back to 48 (the largest multiple of 3 ≤ 50).
        assert_eq!(prefix.len(), 48);
        assert_eq!(prefix, "み".repeat(16));
    }

    #[test]
    fn empty_input_with_zero_budget_is_not_truncated() {
        let (prefix, truncated) = truncate_at_char_boundary("", 0);
        assert_eq!(prefix, "");
        assert!(!truncated);
    }

    #[test]
    fn zero_budget_on_nonempty_input_yields_empty_truncated() {
        let (prefix, truncated) = truncate_at_char_boundary("hello", 0);
        assert_eq!(prefix, "");
        assert!(truncated);
    }

    #[test]
    fn budget_just_above_string_size_does_not_truncate() {
        let (prefix, truncated) = truncate_at_char_boundary("hello", 6);
        assert_eq!(prefix, "hello");
        assert!(!truncated);
    }
}
