use std::fs;

use serial_test::serial;

#[test]
fn load_from_explicit_path() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    fs::write(
        &path,
        r#"{
            "providers": { "custom": { "apiKey": "sk-x", "apiBase": "https://api.x" } },
            "agents": { "defaults": { "model": "m" } }
        }"#,
    )
    .unwrap();

    let cfg = zunel_config::load_config(Some(&path)).unwrap();
    assert_eq!(cfg.providers.custom.as_ref().unwrap().api_key, "sk-x");
    assert_eq!(cfg.agents.defaults.model, "m");
}

#[test]
#[serial(zunel_home_env)]
fn load_from_default_path_via_zunel_home() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("ZUNEL_HOME", tmp.path());
    let path = tmp.path().join("config.json");
    fs::write(
        &path,
        r#"{
            "providers": { "custom": { "apiKey": "sk-y", "apiBase": "https://b.y" } },
            "agents": { "defaults": { "model": "m2" } }
        }"#,
    )
    .unwrap();

    let cfg = zunel_config::load_config(None).unwrap();
    assert_eq!(cfg.providers.custom.as_ref().unwrap().api_key, "sk-y");
    std::env::remove_var("ZUNEL_HOME");
}

#[test]
fn missing_file_returns_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nope.json");
    let err = zunel_config::load_config(Some(&path)).unwrap_err();
    assert!(
        matches!(err, zunel_config::Error::NotFound(_)),
        "got {err:?}"
    );
}

#[test]
fn malformed_json_returns_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.json");
    fs::write(&path, "{").unwrap();
    let err = zunel_config::load_config(Some(&path)).unwrap_err();
    assert!(
        matches!(err, zunel_config::Error::Parse { .. }),
        "got {err:?}"
    );
}
