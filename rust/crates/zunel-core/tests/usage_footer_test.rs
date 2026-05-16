//! Integration test for the public `format_footer` re-export. Mirrors
//! the in-module unit tests but proves the symbol is reachable from
//! downstream crates (CLI, gateway, MCP).

use zunel_core::format_footer;
use zunel_providers::Usage;

#[test]
fn small_turn_renders_compact_footer() {
    let turn = Usage {
        prompt_tokens: 42,
        completion_tokens: 10,
        cached_tokens: 0,
        reasoning_tokens: 0,
    };
    let total = Usage {
        prompt_tokens: 42,
        completion_tokens: 10,
        cached_tokens: 0,
        reasoning_tokens: 0,
    };
    assert_eq!(
        format_footer(&turn, &total),
        "─ 42 in · 10 out · 52 session"
    );
}

#[test]
fn k_threshold_uses_decimal_suffix() {
    let turn = Usage {
        prompt_tokens: 1_200,
        completion_tokens: 800,
        cached_tokens: 0,
        reasoning_tokens: 0,
    };
    let total = Usage {
        prompt_tokens: 12_000,
        completion_tokens: 4_000,
        cached_tokens: 0,
        reasoning_tokens: 0,
    };
    assert_eq!(
        format_footer(&turn, &total),
        "─ 1.2k in · 800 out · 16k session"
    );
}

#[test]
fn reasoning_slice_appears_only_when_nonzero() {
    let with_think = Usage {
        prompt_tokens: 100,
        completion_tokens: 50,
        cached_tokens: 0,
        reasoning_tokens: 1_500,
    };
    let total = Usage {
        prompt_tokens: 100,
        completion_tokens: 50,
        cached_tokens: 0,
        reasoning_tokens: 1_500,
    };
    let footer = format_footer(&with_think, &total);
    assert!(footer.contains("1.5k think"), "{footer}");
    assert!(footer.contains("session"), "{footer}");
}

#[test]
fn zero_usage_returns_empty_string() {
    assert_eq!(format_footer(&Usage::default(), &Usage::default()), "");
}
