use std::fs;

use zunel_core::{ChatRole, Session, SessionManager};

#[test]
fn load_roundtrips_python_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture_src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/python_session.jsonl");
    let fixture_bytes = fs::read(&fixture_src).unwrap();

    // SessionManager expects `<workspace>/sessions/<safe_key>.jsonl`.
    let sessions_dir = tmp.path().join("sessions");
    fs::create_dir_all(&sessions_dir).unwrap();
    fs::write(sessions_dir.join("cli_direct.jsonl"), &fixture_bytes).unwrap();

    let manager = SessionManager::new(tmp.path());
    let session = manager.load("cli:direct").unwrap().expect("session exists");
    assert_eq!(session.key(), "cli:direct");
    assert_eq!(session.messages().len(), 2);
    assert_eq!(session.messages()[0]["role"].as_str(), Some("user"));
    assert_eq!(session.messages()[1]["content"].as_str(), Some("hello"));
}

#[test]
fn save_then_load_roundtrips() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(tmp.path());

    let mut session = Session::new("cli:direct");
    session.add_message(ChatRole::User, "hi");
    session.add_message(ChatRole::Assistant, "hello");
    manager.save(&session).unwrap();

    let loaded = manager.load("cli:direct").unwrap().expect("session exists");
    assert_eq!(loaded.messages().len(), 2);
    assert_eq!(loaded.messages()[0]["content"].as_str(), Some("hi"));
}

#[test]
fn save_produces_python_compatible_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(tmp.path());

    // Build a session whose timestamps are deterministic so we can assert
    // byte-exact file shape.
    let session = Session::for_test(
        "cli:direct".to_string(),
        vec![
            serde_json::json!({
                "role": "user",
                "content": "hi",
                "timestamp": "2026-04-24T10:58:27.000000"
            }),
            serde_json::json!({
                "role": "assistant",
                "content": "hello",
                "timestamp": "2026-04-24T10:58:30.000000"
            }),
        ],
        "2026-04-24T10:58:27.000000".to_string(),
        "2026-04-24T10:58:30.000000".to_string(),
    );
    manager.save(&session).unwrap();

    let written = fs::read_to_string(tmp.path().join("sessions/cli_direct.jsonl")).unwrap();
    let expected = r#"{"_type": "metadata", "key": "cli:direct", "created_at": "2026-04-24T10:58:27.000000", "updated_at": "2026-04-24T10:58:30.000000", "metadata": {}, "last_consolidated": 0}
{"role": "user", "content": "hi", "timestamp": "2026-04-24T10:58:27.000000"}
{"role": "assistant", "content": "hello", "timestamp": "2026-04-24T10:58:30.000000"}
"#;
    assert_eq!(
        written, expected,
        "session file bytes diverged from Python layout"
    );
}

#[test]
fn missing_session_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(tmp.path());
    assert!(manager.load("nope").unwrap().is_none());
}

#[test]
fn save_is_atomic_on_overwrite() {
    // Tests that the write goes via a temp file + rename. We simulate this
    // by checking the ``.tmp`` file does not remain after save.
    let tmp = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(tmp.path());

    let mut session = Session::new("cli:direct");
    session.add_message(ChatRole::User, "first");
    manager.save(&session).unwrap();

    session.add_message(ChatRole::User, "second");
    manager.save(&session).unwrap();

    let sessions_dir = tmp.path().join("sessions");
    let stray: Vec<_> = std::fs::read_dir(&sessions_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(stray.is_empty(), "leftover tmp files: {stray:?}");
}

#[test]
fn concurrent_saves_serialize_via_lock() {
    // Two threads racing to save the same session must produce a
    // valid final file matching one of the two contents — not a
    // partially-written interleaving. fd_lock serialises the writes
    // through `<key>.jsonl.lock`.
    let tmp = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(tmp.path());

    let a = std::thread::spawn({
        let mgr = manager.clone();
        move || {
            for i in 0..50 {
                let mut s = Session::new("cli:direct");
                s.add_message(ChatRole::User, format!("A-{i}"));
                mgr.save(&s).unwrap();
            }
        }
    });
    let b = std::thread::spawn({
        let mgr = manager.clone();
        move || {
            for i in 0..50 {
                let mut s = Session::new("cli:direct");
                s.add_message(ChatRole::User, format!("B-{i}"));
                mgr.save(&s).unwrap();
            }
        }
    });
    a.join().unwrap();
    b.join().unwrap();

    // After both threads finish, the file is parseable and the
    // content matches one of the two writers' last messages.
    let loaded = manager.load("cli:direct").unwrap().expect("present");
    let last_msg = loaded.messages()[0]["content"].as_str().unwrap_or("");
    assert!(
        last_msg.starts_with("A-") || last_msg.starts_with("B-"),
        "expected last message from A or B writer, got: {last_msg:?}"
    );
}
