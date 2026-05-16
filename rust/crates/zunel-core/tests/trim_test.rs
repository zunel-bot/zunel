use serde_json::json;

use zunel_core::trim::{
    apply_tool_result_budget, backfill_missing_tool_results, drop_orphan_tool_results,
    microcompact_old_tool_results, snip_history,
};

#[test]
fn drop_orphan_removes_tool_messages_without_parent_call() {
    let msgs = vec![
        json!({"role":"user","content":"hi"}),
        json!({"role":"tool","tool_call_id":"ghost","name":"read_file","content":"x"}),
    ];
    let out = drop_orphan_tool_results(&msgs);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["role"], "user");
}

#[test]
fn backfill_adds_placeholder_for_missing_results() {
    let msgs = vec![json!({
        "role":"assistant",
        "content":null,
        "tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{}"}}]
    })];
    let out = backfill_missing_tool_results(&msgs);
    assert_eq!(out.len(), 2);
    let placeholder = &out[1];
    assert_eq!(placeholder["role"], "tool");
    assert_eq!(placeholder["tool_call_id"], "call_1");
    assert!(
        placeholder["content"]
            .as_str()
            .unwrap()
            .contains("Tool result unavailable"),
        "got {}",
        placeholder["content"]
    );
}

#[test]
fn microcompact_rewrites_oldest_compactable_results_above_threshold() {
    let big = "a".repeat(1_000);
    let mut msgs: Vec<_> = Vec::new();
    for i in 0..15 {
        msgs.push(json!({
            "role":"tool",
            "tool_call_id": format!("call_{i}"),
            "name":"read_file",
            "content": big.clone(),
        }));
    }
    let out = microcompact_old_tool_results(&msgs);
    let compacted: Vec<_> = out.iter().take(5).collect();
    for c in compacted {
        assert!(c["content"].as_str().unwrap().contains("result omitted"));
    }
    for keep in out.iter().skip(5) {
        assert!(!keep["content"].as_str().unwrap().contains("result omitted"));
    }
}

#[test]
fn apply_tool_result_budget_truncates_large_content() {
    let huge = "b".repeat(30_000);
    let msgs = vec![json!({
        "role":"tool",
        "tool_call_id":"c",
        "name":"read_file",
        "content": huge,
    })];
    let out = apply_tool_result_budget(&msgs, 1_000);
    let body = out[0]["content"].as_str().unwrap();
    assert!(body.len() <= 1_200, "got {}", body.len());
    assert!(body.contains("truncated"));
}

#[test]
fn snip_history_keeps_system_and_most_recent_until_budget() {
    let msgs = vec![
        json!({"role":"system","content":"S"}),
        json!({"role":"user","content":"old"}),
        json!({"role":"assistant","content":"older"}),
        json!({"role":"user","content":"recent"}),
    ];
    let out = snip_history(&msgs, 5);
    assert_eq!(out[0]["role"], "system");
    assert_eq!(out.last().unwrap()["content"], "recent");
    assert!(out.len() < msgs.len(), "snip should drop at least one");
}
