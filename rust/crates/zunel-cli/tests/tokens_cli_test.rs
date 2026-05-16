//! End-to-end smoke for `zunel tokens [list|show|since]`.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::Value;

fn write_config(home: &Path) -> std::path::PathBuf {
    let workspace = home.join("workspace");
    fs::create_dir_all(workspace.join("sessions")).unwrap();
    let config = home.join("config.json");
    fs::write(
        &config,
        format!(
            r#"{{
                "agents": {{ "defaults": {{ "model": "gpt-x", "workspace": "{}" }} }}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    config
}

/// Write a minimal session fixture with a `metadata.usage_total` block
/// so the CLI has something to report on without needing to actually
/// drive the agent loop.
fn write_session_with_usage(
    home: &Path,
    key: &str,
    prompt: u64,
    completion: u64,
    reasoning: u64,
    turns: u64,
) {
    let workspace = home.join("workspace");
    let safe = key.replace(':', "_");
    let path = workspace.join("sessions").join(format!("{safe}.jsonl"));
    let now = chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%.6f")
        .to_string();
    let metadata = serde_json::json!({
        "_type": "metadata",
        "key": key,
        "created_at": now,
        "updated_at": now,
        "metadata": {
            "usage_total": {
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "reasoning_tokens": reasoning,
                "cached_tokens": 0,
                "turns": turns,
            },
            "turn_usage": [
                {"ts": now, "prompt": prompt, "completion": completion, "reasoning": reasoning, "cached": 0}
            ],
        },
        "last_consolidated": 0,
    });
    let mut body = serde_json::to_string(&metadata).unwrap();
    body.push('\n');
    body.push_str(&format!(
        r#"{{"role": "user", "content": "hi", "timestamp": "{now}"}}
"#
    ));
    fs::write(&path, body).unwrap();
}

#[test]
fn tokens_no_args_prints_grand_total() {
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    write_session_with_usage(home.path(), "slack:DA", 1_200, 300, 0, 5);
    write_session_with_usage(home.path(), "cli:direct", 800, 200, 0, 3);

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("tokens")
        .assert()
        .success()
        .stdout(contains("2k in"))
        .stdout(contains("500 out"))
        .stdout(contains("8 turns"));
}

#[test]
fn tokens_list_sorts_by_total_desc() {
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    write_session_with_usage(home.path(), "slack:DSMALL", 100, 50, 0, 1);
    write_session_with_usage(home.path(), "slack:DBIG", 5_000, 1_000, 0, 5);

    let output = Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("tokens")
        .arg("list")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr={:?}", output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let big_line = stdout.find("slack_DBIG").unwrap();
    let small_line = stdout.find("slack_DSMALL").unwrap();
    assert!(
        big_line < small_line,
        "expected DBIG before DSMALL in:\n{stdout}"
    );
}

#[test]
fn tokens_show_emits_json_when_requested() {
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    write_session_with_usage(home.path(), "slack:DA", 700, 100, 50, 4);

    let output = Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("tokens")
        .arg("show")
        .arg("slack_DA")
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr={:?}", output.stderr);
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    // The CLI normalizes the on-disk filename ("slack_DA") back into
    // the canonical session key ("slack:DA") that the agent would use.
    assert_eq!(json["key"], "slack:DA");
    assert_eq!(json["prompt_tokens"], 700);
    assert_eq!(json["completion_tokens"], 100);
    assert_eq!(json["reasoning_tokens"], 50);
    assert_eq!(json["turns"], 4);
}

#[test]
fn tokens_since_aggregates_recent_turns() {
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    write_session_with_usage(home.path(), "slack:DA", 700, 100, 0, 1);

    let output = Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("tokens")
        .arg("since")
        .arg("1d")
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr={:?}", output.stderr);
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["sessions"], 1);
    assert_eq!(json["turns"], 1);
    assert_eq!(json["prompt_tokens"], 700);
    assert_eq!(json["completion_tokens"], 100);
}
