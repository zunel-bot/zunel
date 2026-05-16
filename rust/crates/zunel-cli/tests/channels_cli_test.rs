use std::fs;

use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn channels_status_reports_empty_channel_set() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.json");
    fs::write(
        &config,
        format!(
            r#"{{
                "agents": {{ "defaults": {{ "model": "m", "workspace": "{}" }} }}
            }}"#,
            home.path()
                .join("workspace")
                .display()
                .to_string()
                .replace('\\', "/")
        ),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("channels")
        .arg("status")
        .assert()
        .success()
        .stdout(contains("channels: 0"));
}

#[test]
fn channels_status_reports_configured_slack_channel() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.json");
    fs::write(
        &config,
        format!(
            r#"{{
                "agents": {{ "defaults": {{ "model": "m", "workspace": "{}" }} }},
                "channels": {{
                    "slack": {{
                        "enabled": false
                    }}
                }}
            }}"#,
            home.path()
                .join("workspace")
                .display()
                .to_string()
                .replace('\\', "/")
        ),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("channels")
        .arg("status")
        .assert()
        .success()
        .stdout(contains("channels: 1"))
        .stdout(contains("slack: disconnected"));
}
