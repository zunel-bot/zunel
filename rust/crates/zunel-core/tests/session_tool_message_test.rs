use serde_json::json;
use tempfile::tempdir;

use zunel_core::{Session, SessionManager};

fn load_or_new(mgr: &SessionManager, key: &str) -> Session {
    mgr.load(key).unwrap().unwrap_or_else(|| Session::new(key))
}

#[test]
fn assistant_tool_call_message_round_trips_with_content_null() {
    let tmp = tempdir().unwrap();
    let mgr = SessionManager::new(tmp.path());
    let mut session = load_or_new(&mgr, "cli:direct");

    session.append_raw_message(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"README.md\"}"}
        }],
    }));
    session.append_raw_message(json!({
        "role": "tool",
        "tool_call_id": "call_1",
        "name": "read_file",
        "content": "file body",
    }));
    mgr.save(&session).unwrap();

    let reloaded = SessionManager::new(tmp.path())
        .load("cli:direct")
        .unwrap()
        .expect("session round-trip");
    let msgs = reloaded.messages();
    assert_eq!(msgs.len(), 2);
    assert!(msgs[0]["content"].is_null());
    assert_eq!(msgs[0]["tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[1]["tool_call_id"], "call_1");
}

#[test]
fn session_file_preserves_key_order_for_tool_messages() {
    let tmp = tempdir().unwrap();
    let mgr = SessionManager::new(tmp.path());
    let mut session = load_or_new(&mgr, "cli:direct");
    session.append_raw_message(json!({
        "role": "tool",
        "tool_call_id": "c1",
        "name": "exec",
        "content": "ok",
    }));
    mgr.save(&session).unwrap();
    let path = tmp.path().join("sessions").join("cli_direct.jsonl");
    let body = std::fs::read_to_string(path).unwrap();
    // First line is metadata; the message is line 2.
    let line = body.lines().nth(1).expect("expected message line");
    let idx_role = line.find("\"role\"").unwrap();
    let idx_tool_call_id = line.find("\"tool_call_id\"").unwrap();
    let idx_name = line.find("\"name\"").unwrap();
    let idx_content = line.find("\"content\"").unwrap();
    assert!(
        idx_role < idx_tool_call_id && idx_tool_call_id < idx_name && idx_name < idx_content,
        "unexpected key order in: {line}"
    );
}
