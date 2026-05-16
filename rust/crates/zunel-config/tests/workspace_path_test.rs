use std::path::PathBuf;

use serial_test::serial;
use zunel_config::{default_workspace_path, workspace_path, AgentDefaults};

#[test]
#[serial(zunel_home_env)]
fn default_workspace_is_under_zunel_home() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("ZUNEL_HOME", tmp.path());
    let expected: PathBuf = tmp.path().join("workspace");
    assert_eq!(default_workspace_path().unwrap(), expected);
    std::env::remove_var("ZUNEL_HOME");
}

#[test]
#[serial(zunel_home_env)]
fn workspace_path_respects_agent_defaults_override() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("ZUNEL_HOME", tmp.path());
    let custom = tmp.path().join("elsewhere");
    let defaults = AgentDefaults {
        workspace: Some(custom.to_string_lossy().into_owned()),
        ..Default::default()
    };
    assert_eq!(workspace_path(&defaults).unwrap(), custom);
    std::env::remove_var("ZUNEL_HOME");
}

#[test]
#[serial(zunel_home_env)]
fn workspace_path_expands_tilde() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("ZUNEL_HOME", tmp.path());
    // Simulate Python's "~/.zunel/workspace" default by using the literal
    // string from Python's schema.
    let defaults = AgentDefaults {
        workspace: Some("~/.zunel/workspace".to_string()),
        ..Default::default()
    };
    let resolved = workspace_path(&defaults).unwrap();
    // After expansion, the path must be absolute and not contain a leading ~.
    assert!(resolved.is_absolute(), "got {resolved:?}");
    assert!(!resolved.to_string_lossy().starts_with('~'));
    std::env::remove_var("ZUNEL_HOME");
}
