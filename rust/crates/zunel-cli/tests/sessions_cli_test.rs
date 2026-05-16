//! End-to-end smoke for the `zunel sessions` subcommands.
//!
//! `compact` is exercised via the `CompactionService` unit tests in
//! `zunel-core` because hitting it from the CLI requires a real
//! provider configured in `config.json`. Here we focus on the
//! file-touching subcommands (`list`, `show`, `clear`, `prune`) that
//! work entirely off-line.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::str::contains;

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

fn write_fixture_session(home: &Path, key: &str, msg_count: usize, age_minutes: i64) {
    let workspace = home.join("workspace");
    let safe = key.replace(':', "_");
    let path = workspace.join("sessions").join(format!("{safe}.jsonl"));
    let stale = chrono::Local::now() - chrono::Duration::minutes(age_minutes);
    let stale_iso = stale.format("%Y-%m-%dT%H:%M:%S%.6f").to_string();
    let mut body = format!(
        r#"{{"_type": "metadata", "key": "{key}", "created_at": "{stale_iso}", "updated_at": "{stale_iso}", "metadata": {{}}, "last_consolidated": 0}}
"#
    );
    for i in 0..msg_count {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        body.push_str(&format!(
            r#"{{"role": "{role}", "content": "msg #{i}", "timestamp": "{stale_iso}"}}
"#
        ));
    }
    fs::write(&path, body).unwrap();
}

#[test]
fn sessions_list_prints_known_sessions_sorted_by_size() {
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    write_fixture_session(home.path(), "slack:DBIG", 50, 60);
    write_fixture_session(home.path(), "cli:direct", 5, 60);

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("sessions")
        .arg("list")
        .assert()
        .success()
        .stdout(contains("KEY"))
        .stdout(contains("slack_DBIG"))
        .stdout(contains("cli_direct"));
}

#[test]
fn sessions_show_prints_recent_rows() {
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    write_fixture_session(home.path(), "slack:DBIG", 12, 60);

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("sessions")
        .arg("show")
        .arg("slack_DBIG")
        .arg("--tail")
        .arg("3")
        .assert()
        .success()
        .stdout(contains("12 messages"))
        .stdout(contains("msg #11"))
        .stdout(contains("msg #10"))
        .stdout(contains("msg #9"));
}

#[test]
fn sessions_clear_truncates_file() {
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    write_fixture_session(home.path(), "slack:DBIG", 8, 60);

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("sessions")
        .arg("clear")
        .arg("slack_DBIG")
        .arg("--yes")
        .assert()
        .success()
        .stdout(contains("cleared"));

    let body = fs::read_to_string(
        home.path()
            .join("workspace")
            .join("sessions")
            .join("slack_DBIG.jsonl"),
    )
    .unwrap();
    let lines: Vec<_> = body.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "only metadata line should remain");
}

#[test]
fn sessions_prune_dry_run_lists_without_deleting() {
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    write_fixture_session(home.path(), "slack:DSTALE", 3, 60 * 24 * 40);
    write_fixture_session(home.path(), "slack:DFRESH", 3, 5);

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("sessions")
        .arg("prune")
        .arg("--older-than")
        .arg("30d")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(contains("would delete"))
        .stdout(contains("slack_DSTALE"));

    assert!(home
        .path()
        .join("workspace/sessions/slack_DSTALE.jsonl")
        .exists());
    assert!(home
        .path()
        .join("workspace/sessions/slack_DFRESH.jsonl")
        .exists());
}

#[test]
fn sessions_prune_deletes_old_sessions() {
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    write_fixture_session(home.path(), "slack:DSTALE", 3, 60 * 24 * 40);
    write_fixture_session(home.path(), "slack:DFRESH", 3, 5);

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("sessions")
        .arg("prune")
        .arg("--older-than")
        .arg("30d")
        .assert()
        .success()
        .stdout(contains("pruned 1"));

    assert!(!home
        .path()
        .join("workspace/sessions/slack_DSTALE.jsonl")
        .exists());
    assert!(home
        .path()
        .join("workspace/sessions/slack_DFRESH.jsonl")
        .exists());
}
