//! End-to-end coverage of the workspace foot-gun guard wired
//! through the CLI.
//!
//! These tests exercise the *integration* between the
//! `--i-know-what-im-doing` global flag, the
//! `ZUNEL_ALLOW_UNSAFE_WORKSPACE` env var, and the entry-point
//! commands that call `zunel_config::guard_workspace`. Unit-level
//! coverage of the trigger conditions lives in
//! `zunel-config/tests/workspace_guard_test.rs`.
//!
//! We drive the test through `zunel mcp agent --print-config`
//! because that subcommand:
//!   * runs the guard at the same point as the production
//!     code-path (immediately after `workspace_path`),
//!   * exits without binding a socket or speaking to a model,
//!   * deterministically prints a JSON snippet on the safe
//!     branch we can assert against.
//!
//! The guard fires when the resolved workspace is an ancestor of
//! `$ZUNEL_HOME`. We engineer that by pointing `ZUNEL_HOME` at a
//! deep child (`<root>/inner/.zunel`) and the workspace at the
//! parent (`<root>`). That gives us a "bad" but realistic
//! configuration without trampling on the real user's `$HOME`.

use std::fs;

use assert_cmd::Command;
use predicates::str::contains;

/// Build a config.json whose `agents.defaults.workspace` points
/// at `workspace`, with a stub provider that satisfies
/// `load_config` without ever being dialed.
fn write_config(zunel_home: &std::path::Path, workspace: &std::path::Path) {
    let config = zunel_home.join("config.json");
    fs::create_dir_all(zunel_home).unwrap();
    fs::write(
        &config,
        format!(
            r#"{{
                "providers": {{"custom": {{"apiKey": "sk", "apiBase": "http://127.0.0.1:1"}}}},
                "agents": {{"defaults": {{
                    "provider": "custom",
                    "model": "m",
                    "workspace": "{}"
                }}}}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
}

/// Lay out the "ancestor of ZUNEL_HOME" trap and return
/// `(zunel_home, workspace)`. Both paths are created on disk so
/// `ensure_dir` won't change behavior between guarded and
/// unguarded runs.
fn lay_out_unsafe_workspace(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let zunel_home = root.join("inner").join(".zunel");
    fs::create_dir_all(&zunel_home).unwrap();
    let workspace = root.to_path_buf();
    write_config(&zunel_home, &workspace);
    (zunel_home, workspace)
}

#[test]
fn mcp_agent_refuses_to_start_when_workspace_contains_zunel_home() {
    let root = tempfile::tempdir().unwrap();
    let (zunel_home, _workspace) = lay_out_unsafe_workspace(root.path());

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", &zunel_home)
        .env_remove("ZUNEL_ALLOW_UNSAFE_WORKSPACE")
        .arg("mcp")
        .arg("agent")
        .arg("--print-config")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .assert()
        .failure()
        .stderr(contains("refusing to start with workspace"))
        .stderr(contains("ZUNEL_ALLOW_UNSAFE_WORKSPACE"))
        .stderr(contains("--i-know-what-im-doing"));
}

#[test]
fn i_know_what_im_doing_flag_bypasses_guard() {
    let root = tempfile::tempdir().unwrap();
    let (zunel_home, _workspace) = lay_out_unsafe_workspace(root.path());

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", &zunel_home)
        .env_remove("ZUNEL_ALLOW_UNSAFE_WORKSPACE")
        .arg("--i-know-what-im-doing")
        .arg("mcp")
        .arg("agent")
        .arg("--print-config")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .assert()
        .success()
        .stdout(contains("mcpServers"));
}

#[test]
fn env_var_bypasses_guard() {
    let root = tempfile::tempdir().unwrap();
    let (zunel_home, _workspace) = lay_out_unsafe_workspace(root.path());

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", &zunel_home)
        .env("ZUNEL_ALLOW_UNSAFE_WORKSPACE", "1")
        .arg("mcp")
        .arg("agent")
        .arg("--print-config")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .assert()
        .success()
        .stdout(contains("mcpServers"));
}

#[test]
fn safe_workspace_does_not_trip_guard() {
    // Sanity: the guard must not regress the happy path. We use
    // the default-shape "<ZUNEL_HOME>/workspace" layout that
    // `zunel onboard` produces.
    let zunel_home = tempfile::tempdir().unwrap();
    let workspace = zunel_home.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_config(zunel_home.path(), &workspace);

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", zunel_home.path())
        .env_remove("ZUNEL_ALLOW_UNSAFE_WORKSPACE")
        .arg("mcp")
        .arg("agent")
        .arg("--print-config")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .assert()
        .success()
        .stdout(contains("mcpServers"));
}
