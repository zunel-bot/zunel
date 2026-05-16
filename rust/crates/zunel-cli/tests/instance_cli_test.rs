use std::fs;

use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn instance_use_show_list_and_remove_named_instance() {
    let home = tempfile::tempdir().unwrap();
    let default_home = home.path().join(".zunel");
    let dev_home = default_home.join("instances").join("dev");
    fs::create_dir_all(&dev_home).unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("ZUNEL_HOME")
        .args(["instance", "use", "dev"])
        .assert()
        .success()
        .stdout(contains("Active instance set to dev"));

    assert_eq!(
        fs::read_to_string(default_home.join("active_instance")).unwrap(),
        "dev\n"
    );

    Command::cargo_bin("zunel")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("ZUNEL_HOME")
        .args(["instance", "show"])
        .assert()
        .success()
        .stdout(contains("instance: dev"))
        .stdout(contains(format!("home: {}", dev_home.display())));

    Command::cargo_bin("zunel")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("ZUNEL_HOME")
        .args(["instance", "list"])
        .assert()
        .success()
        .stdout(contains("default"))
        .stdout(contains("dev"));

    Command::cargo_bin("zunel")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("ZUNEL_HOME")
        .args(["instance", "use", "default"])
        .assert()
        .success()
        .stdout(contains("Cleared sticky instance"));

    Command::cargo_bin("zunel")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("ZUNEL_HOME")
        .args(["instance", "rm", "dev", "--force"])
        .assert()
        .success()
        .stdout(contains("Removed"));

    assert!(!dev_home.exists());
}

#[test]
fn instance_rejects_unsafe_names() {
    let home = tempfile::tempdir().unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("ZUNEL_HOME")
        .args(["instance", "use", "../bad"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("Invalid instance name"));
}

#[test]
fn global_instance_flag_routes_command_to_named_home() {
    let home = tempfile::tempdir().unwrap();
    let dev_home = home.path().join(".zunel").join("instances").join("dev");

    Command::cargo_bin("zunel")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("ZUNEL_HOME")
        .args(["--instance", "dev", "onboard"])
        .assert()
        .success()
        .stdout(contains(format!("onboarded: {}", dev_home.display())));

    assert!(dev_home.join("config.json").exists());
    assert!(dev_home.join("workspace").is_dir());
}

/// After the rename, an existing `~/.zunel/profiles/` directory must
/// trigger a clear migration error rather than silently working. The
/// gate fires for any path that hits `resolve_instance_home` —
/// `instance list` is the simplest way to provoke it without needing
/// a populated profile.
#[test]
fn legacy_profiles_directory_blocks_with_migration_hint() {
    let home = tempfile::tempdir().unwrap();
    let default_home = home.path().join(".zunel");
    let legacy_dir = default_home.join("profiles").join("dev");
    fs::create_dir_all(&legacy_dir).unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("ZUNEL_HOME")
        .args(["--instance", "dev", "status"])
        .assert()
        .failure()
        .stderr(contains("legacy ~/.zunel/profiles/"))
        .stderr(contains("mv "));
}
