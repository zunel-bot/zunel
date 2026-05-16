//! End-to-end discovery tests against on-disk fixtures.
//!
//! Complements the unit tests inside `profiles.rs` by exercising the
//! filesystem and env-var path the gateway actually takes:
//!
//! - `discover_sso_profiles(path)` parsing a realistic mixed-shape
//!   `~/.aws/config` (the layout the original feature request spotted).
//! - `resolve_aws_config_path()` honoring `AWS_CONFIG_FILE` over
//!   `$HOME/.aws/config`.

use std::fs;

use zunel_aws::profiles::{discover_sso_profiles, resolve_aws_config_path};

/// Mirrors the user's actual `~/.aws/config`: nine profiles, eight
/// SSO-bearing, one keypair-only — plus an `[sso-session ...]` block
/// that must NOT show up in the result.
const REALISTIC_CONFIG: &str = "\
[profile zillow-sandbox]
sso_start_url = https://zillow.awsapps.com/start
sso_region = us-west-2
sso_account_id = 717279734741
sso_role_name = Zillow-Sandbox-Developer

[profile da-test]
sso_start_url = https://zillow.awsapps.com/start
sso_region = us-west-2
sso_account_id = 958504467206
sso_role_name = ZG-Developer

[profile da-prod]
sso_start_url = https://zillow.awsapps.com/start
sso_region = us-west-2
sso_account_id = 516298499892
sso_role_name = ZG-Developer

[profile zg-prod]
sso_start_url = https://zillow.awsapps.com/start
sso_region = us-west-2
sso_account_id = 150122898372
sso_role_name = ZG-ViewOnly

[profile zg-prod-write-only]

[profile dataacquisition-test]
sso_start_url = https://zillow.awsapps.com/start
sso_region = us-west-2
sso_account_id = 958504467206
sso_role_name = ZG-Developer

[profile dataacquisition-prod]
sso_start_url = https://zillow.awsapps.com/start
sso_region = us-west-2
sso_account_id = 516298499892
sso_role_name = ZG-Developer

[profile zillow-prod]
sso_start_url = https://zillow.awsapps.com/start
sso_region = us-west-2
sso_account_id = 150122898372
sso_role_name = ZG-ViewOnly

[profile allegro]
sso_start_url = https://zillow.awsapps.com/start
sso_region = us-west-2
sso_account_id = 009681858859
sso_role_name = ZG-Developer

[sso-session zillow]
sso_start_url = https://zillow.awsapps.com/start
sso_region = us-west-2
";

#[test]
fn discovers_every_sso_profile_in_realistic_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config");
    fs::write(&cfg, REALISTIC_CONFIG).unwrap();

    let mut found = discover_sso_profiles(&cfg).unwrap();
    found.sort();

    let mut expected = vec![
        "allegro",
        "da-prod",
        "da-test",
        "dataacquisition-prod",
        "dataacquisition-test",
        "zg-prod",
        "zillow-prod",
        "zillow-sandbox",
    ];
    expected.sort();

    let expected: Vec<String> = expected.into_iter().map(String::from).collect();
    assert_eq!(found, expected);

    // The bare `[profile zg-prod-write-only]` (no SSO keys) and the
    // `[sso-session zillow]` block must NOT appear.
    assert!(!found.iter().any(|p| p == "zg-prod-write-only"));
    assert!(!found.iter().any(|p| p == "zillow"));
}

#[test]
fn missing_config_file_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("does-not-exist");
    let found = discover_sso_profiles(&cfg).unwrap();
    assert!(found.is_empty());
}

/// `resolve_aws_config_path` is `unsafe`-adjacent: it reads process
/// env, which is racy across parallel tests. Bundle every env-var
/// branch into one `#[test]` so we mutate `AWS_CONFIG_FILE` /
/// `HOME` exactly once per branch sequentially.
#[test]
fn resolve_aws_config_path_honors_env_overrides() {
    use std::env;

    let prior_aws = env::var_os("AWS_CONFIG_FILE");
    let prior_home = env::var_os("HOME");

    env::remove_var("AWS_CONFIG_FILE");
    env::set_var("HOME", "/some/home");
    assert_eq!(
        resolve_aws_config_path().unwrap(),
        std::path::PathBuf::from("/some/home/.aws/config"),
        "with AWS_CONFIG_FILE unset, falls back to $HOME/.aws/config"
    );

    env::set_var("AWS_CONFIG_FILE", "/explicit/aws.cfg");
    assert_eq!(
        resolve_aws_config_path().unwrap(),
        std::path::PathBuf::from("/explicit/aws.cfg"),
        "AWS_CONFIG_FILE wins when set"
    );

    env::set_var("AWS_CONFIG_FILE", "");
    assert_eq!(
        resolve_aws_config_path().unwrap(),
        std::path::PathBuf::from("/some/home/.aws/config"),
        "empty AWS_CONFIG_FILE is treated as unset"
    );

    match prior_aws {
        Some(v) => env::set_var("AWS_CONFIG_FILE", v),
        None => env::remove_var("AWS_CONFIG_FILE"),
    }
    match prior_home {
        Some(v) => env::set_var("HOME", v),
        None => env::remove_var("HOME"),
    }
}
