use zunel_core::{ChatRole, Session};

#[test]
fn new_session_is_empty_and_keyed() {
    let session = Session::new("cli:direct");
    assert_eq!(session.key(), "cli:direct");
    assert!(session.messages().is_empty());
    assert_eq!(session.last_consolidated(), 0);
}

#[test]
fn add_message_appends_and_updates_timestamp() {
    let mut session = Session::new("cli:direct");
    let before = session.updated_at().to_string();
    std::thread::sleep(std::time::Duration::from_millis(2));
    session.add_message(ChatRole::User, "hello");
    assert_eq!(session.messages().len(), 1);
    assert!(session.updated_at() > before.as_str());

    let entry = &session.messages()[0];
    assert_eq!(entry["role"].as_str(), Some("user"));
    assert_eq!(entry["content"].as_str(), Some("hello"));
    assert!(
        entry["timestamp"].is_string(),
        "timestamp present, got {entry}"
    );
}

#[test]
fn get_history_clones_messages_without_timestamp() {
    let mut session = Session::new("cli:direct");
    session.add_message(ChatRole::User, "hi");
    session.add_message(ChatRole::Assistant, "hello back");

    let history = session.get_history(10);
    assert_eq!(history.len(), 2);
    // Slice 2 strips timestamps when replaying for the LLM — matches Python.
    assert!(history[0].get("timestamp").is_none());
    assert_eq!(history[0]["role"].as_str(), Some("user"));
    assert_eq!(history[1]["content"].as_str(), Some("hello back"));
}

#[test]
fn get_history_respects_max_messages() {
    let mut session = Session::new("cli:direct");
    for i in 0..5 {
        session.add_message(ChatRole::User, format!("m{i}"));
    }
    let last_two = session.get_history(2);
    assert_eq!(last_two.len(), 2);
    assert_eq!(last_two[0]["content"].as_str(), Some("m3"));
}

#[test]
fn clear_removes_messages() {
    let mut session = Session::new("cli:direct");
    session.add_message(ChatRole::User, "hi");
    session.clear();
    assert!(session.messages().is_empty());
    assert_eq!(session.last_consolidated(), 0);
}
