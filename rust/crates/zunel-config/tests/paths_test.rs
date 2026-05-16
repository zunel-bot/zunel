use std::path::PathBuf;

use serial_test::serial;

#[test]
#[serial(zunel_home_env)]
fn zunel_home_respects_env_override() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("ZUNEL_HOME", tmp.path());
    assert_eq!(zunel_config::zunel_home().unwrap(), tmp.path());
    std::env::remove_var("ZUNEL_HOME");
}

#[test]
#[serial(zunel_home_env)]
fn config_path_defaults_to_config_json_inside_home() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("ZUNEL_HOME", tmp.path());
    let expected: PathBuf = tmp.path().join("config.json");
    assert_eq!(zunel_config::default_config_path().unwrap(), expected);
    std::env::remove_var("ZUNEL_HOME");
}
