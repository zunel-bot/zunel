//! Verifies `Session::record_turn_usage` persists into metadata,
//! `usage_total` round-trips through `SessionManager`, and the
//! `turn_usage` array stays capped at `MAX_TURN_USAGE_ENTRIES`.

use tempfile::TempDir;
use zunel_core::{Session, SessionManager, MAX_TURN_USAGE_ENTRIES};
use zunel_providers::Usage;

fn workspace() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn record_turn_usage_round_trips_through_disk() {
    let tmp = workspace();
    let mgr = SessionManager::new(tmp.path());
    let mut session = Session::new("cli:direct");
    session.record_turn_usage(&Usage {
        prompt_tokens: 100,
        completion_tokens: 20,
        cached_tokens: 5,
        reasoning_tokens: 7,
    });
    session.record_turn_usage(&Usage {
        prompt_tokens: 50,
        completion_tokens: 30,
        cached_tokens: 0,
        reasoning_tokens: 0,
    });
    mgr.save(&session).unwrap();

    let reloaded = mgr.load("cli:direct").unwrap().expect("session");
    let total = reloaded.usage_total();
    assert_eq!(total.prompt_tokens, 150);
    assert_eq!(total.completion_tokens, 50);
    assert_eq!(total.cached_tokens, 5);
    assert_eq!(total.reasoning_tokens, 7);
    assert_eq!(reloaded.usage_turns(), 2);
    assert_eq!(reloaded.turn_usage().len(), 2);
}

#[test]
fn turn_usage_array_is_capped_but_total_keeps_growing() {
    // 5 over the cap to prove the head is dropped, not the tail.
    let n = MAX_TURN_USAGE_ENTRIES + 5;
    let mut session = Session::new("cli:direct");
    for _ in 0..n {
        session.record_turn_usage(&Usage {
            prompt_tokens: 1,
            completion_tokens: 1,
            cached_tokens: 0,
            reasoning_tokens: 0,
        });
    }

    assert_eq!(
        session.turn_usage().len(),
        MAX_TURN_USAGE_ENTRIES,
        "turn_usage caps at MAX_TURN_USAGE_ENTRIES"
    );
    let total = session.usage_total();
    assert_eq!(
        total.prompt_tokens as usize, n,
        "usage_total still counts every turn even after cap kicks in"
    );
    assert_eq!(total.completion_tokens as usize, n);
    assert_eq!(session.usage_turns() as usize, n);
}

#[test]
fn fully_zero_usage_is_skipped() {
    let mut session = Session::new("cli:direct");
    session.record_turn_usage(&Usage::default());
    assert_eq!(session.usage_turns(), 0);
    assert!(session.turn_usage().is_empty());
    assert_eq!(session.usage_total(), Usage::default());
}
