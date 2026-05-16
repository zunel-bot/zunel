use std::fs;

use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn status_reports_provider_model_and_workspace() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let config = home.path().join("config.json");
    fs::write(
        &config,
        format!(
            r#"{{
                "providers": {{"custom": {{"apiKey": "sk", "apiBase": "http://127.0.0.1:1"}}}},
                "agents": {{"defaults": {{"provider": "custom", "model": "m", "workspace": "{}"}}}}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("--config")
        .arg(&config)
        .arg("status")
        .assert()
        .success()
        .stdout(contains("provider: custom"))
        .stdout(contains("model: m"))
        .stdout(contains(format!("workspace: {}", workspace.display())));
}

#[test]
fn onboard_creates_default_config_and_workspace() {
    let home = tempfile::tempdir().unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .arg("onboard")
        .assert()
        .success()
        .stdout(contains("onboarded"));

    assert!(home.path().join("config.json").exists());
    assert!(home.path().join("workspace").is_dir());
    assert!(home.path().join("workspace").join("SOUL.md").exists());
    assert!(home.path().join("workspace").join("USER.md").exists());
    assert!(home.path().join("workspace").join("HEARTBEAT.md").exists());
    assert!(home
        .path()
        .join("workspace")
        .join("memory")
        .join("MEMORY.md")
        .exists());
}
