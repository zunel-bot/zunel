//! End-to-end tests for the SSO refresh module.
//!
//! Each test stands up a tiny shell-script "aws" stub in a tempdir,
//! points [`RefreshContext`] at it, and asserts the parsed outcome.
//! This mirrors the pattern in
//! `zunel-channels/src/slack/bot_refresh.rs`'s wiremock-based tests:
//! we exercise the real subprocess plumbing rather than stubbing at
//! the function boundary, so the env/argv/stderr-classification logic
//! is covered for real.
//!
//! Every test that calls `write_aws_stub` is annotated with
//! `#[serial_test::serial]` to avoid a Linux ETXTBSY ("Text file
//! busy") race between sibling tests. cargo's test harness runs
//! `#[tokio::test]` cases on parallel OS threads, and on Linux a
//! `fork()` in any thread inherits every writable fd from every
//! other thread for the brief window between fork and exec. If
//! one thread is mid-write on its own `aws-stub.sh` while another
//! forks, the forked child briefly holds a writable fd to the
//! first thread's stub. Should that first thread happen to
//! `exec()` its own stub during that window, Linux returns
//! ETXTBSY. Serializing the fork+exec-using tests closes the
//! window cleanly.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use zunel_aws::sso_refresh::{
    refresh_profile_if_near_expiry, RefreshContext, RefreshError, RefreshOutcome,
};

/// Write a fake `aws` script that asserts it was invoked with the
/// expected argv (`configure export-credentials --profile <p> --format
/// process`), then prints the given stdout/stderr and exits with the
/// given code. Returns the path so the caller can hand it to
/// [`RefreshContext::with_aws_bin`].
///
/// The argv assertion is critical: AWS CLI v2 rejects `--format json`
/// (the value we naively shipped first), so a stub that ignored argv
/// silently let an integration regression slip past tests. By making
/// the stub `exit 64` on argv mismatch, any future drift in the call
/// shape produces a loud `AwsCommandFailed` test failure instead.
///
/// We bake the canned bytes into separate files and have the script
/// `cat` them so multi-line / quote-heavy fixtures don't have to be
/// shell-escaped inside the heredoc. Also makes the script trivially
/// debuggable by hand: `cat aws-stub.sh` shows exactly what it runs.
fn write_aws_stub(dir: &Path, exit_code: i32, stdout_body: &str, stderr_body: &str) -> PathBuf {
    let stdout_file = dir.join("stub-stdout.txt");
    let stderr_file = dir.join("stub-stderr.txt");
    fs::write(&stdout_file, stdout_body).unwrap();
    fs::write(&stderr_file, stderr_body).unwrap();

    let script_path = dir.join("aws-stub.sh");
    let mut f = fs::File::create(&script_path).unwrap();
    writeln!(
        f,
        "#!/bin/bash
# Reject any argv that doesn't match what zunel-aws is supposed to send.
if [ \"$1\" != \"configure\" ] || [ \"$2\" != \"export-credentials\" ] \\
   || [ \"$3\" != \"--profile\" ] || [ -z \"$4\" ] \\
   || [ \"$5\" != \"--format\" ] || [ \"$6\" != \"process\" ]; then
  echo \"aws-stub: unexpected argv: $*\" >&2
  exit 64
fi
cat {}
cat {} >&2
exit {}",
        shell_escape(&stdout_file),
        shell_escape(&stderr_file),
        exit_code,
    )
    .unwrap();
    let mut perm = fs::metadata(&script_path).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&script_path, perm).unwrap();
    script_path
}

/// Quote a path for safe interpolation inside the script. Tempdir
/// paths on macOS contain `/var/folders/...` with no special chars,
/// but be defensive in case a CI runner uses a path with a space.
fn shell_escape(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\'', "'\\''");
    format!("'{}'", s)
}

/// Build a minimal valid process-credentials JSON whose `Expiration`
/// is `secs_from_now` seconds in the future (negative for past).
fn process_creds_json(secs_from_now: i64) -> String {
    let expiration = (Utc::now() + Duration::seconds(secs_from_now)).to_rfc3339();
    format!(
        r#"{{
  "Version": 1,
  "AccessKeyId": "ASIAEXAMPLE",
  "SecretAccessKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
  "SessionToken": "AQoEXAMPLEH4aoAH0gNCAPyJxz4BlCFFxWNE1OPTgk5TthT+FvwqnKwRcOIfrRh3c/L",
  "Expiration": "{expiration}"
}}
"#,
    )
}

#[tokio::test]
#[serial_test::serial]
async fn refreshed_when_expiration_inside_window() {
    let dir = tempfile::tempdir().unwrap();
    let aws_bin = write_aws_stub(dir.path(), 0, &process_creds_json(600), "");
    let ctx = RefreshContext::with_aws_bin(aws_bin);

    // 600s of life, refresh-window = 900s -> inside window -> Refreshed.
    let outcome = refresh_profile_if_near_expiry(&ctx, "dev", Some(900))
        .await
        .unwrap();
    let RefreshOutcome::Refreshed {
        profile,
        secs_until_exp,
        ..
    } = outcome
    else {
        panic!("expected Refreshed, got {outcome:?}");
    };
    assert_eq!(profile, "dev");
    assert!(
        (550..=650).contains(&secs_until_exp),
        "secs_until_exp out of bounds: {secs_until_exp}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn skipped_when_expiration_outside_window() {
    let dir = tempfile::tempdir().unwrap();
    let aws_bin = write_aws_stub(dir.path(), 0, &process_creds_json(7200), "");
    let ctx = RefreshContext::with_aws_bin(aws_bin);

    // 7200s of life, refresh-window = 900s -> well outside window -> Skipped.
    let outcome = refresh_profile_if_near_expiry(&ctx, "prod", Some(900))
        .await
        .unwrap();
    let RefreshOutcome::Skipped {
        profile,
        secs_until_exp,
        ..
    } = outcome
    else {
        panic!("expected Skipped, got {outcome:?}");
    };
    assert_eq!(profile, "prod");
    assert!(
        secs_until_exp > 900,
        "Skipped variant must have secs_until_exp > window, got {secs_until_exp}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn refreshed_when_no_window_supplied() {
    let dir = tempfile::tempdir().unwrap();
    let aws_bin = write_aws_stub(dir.path(), 0, &process_creds_json(86_400), "");
    let ctx = RefreshContext::with_aws_bin(aws_bin);

    // No window -> always Refreshed regardless of remaining life.
    let outcome = refresh_profile_if_near_expiry(&ctx, "dev", None)
        .await
        .unwrap();
    assert!(
        outcome.is_refreshed(),
        "expected Refreshed, got {outcome:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn sso_session_expired_when_aws_cli_reports_expired_token() {
    let dir = tempfile::tempdir().unwrap();
    let aws_bin = write_aws_stub(
        dir.path(),
        255,
        "",
        "Error loading SSO Token: Token has expired and refresh failed\n",
    );
    let ctx = RefreshContext::with_aws_bin(aws_bin);

    let err = refresh_profile_if_near_expiry(&ctx, "dev", Some(900))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RefreshError::SsoSessionExpired { ref profile, .. } if profile == "dev"),
        "expected SsoSessionExpired, got {err:?}"
    );
    // Display should mention the operator-actionable command.
    let msg = format!("{err}");
    assert!(
        msg.contains("aws sso login --profile dev"),
        "missing actionable message: {msg}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn profile_not_configured_when_aws_cli_says_profile_missing() {
    let dir = tempfile::tempdir().unwrap();
    let aws_bin = write_aws_stub(
        dir.path(),
        255,
        "",
        "The config profile (ghost) could not be found\n",
    );
    let ctx = RefreshContext::with_aws_bin(aws_bin);

    let err = refresh_profile_if_near_expiry(&ctx, "ghost", Some(900))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RefreshError::ProfileNotConfigured { ref profile, .. } if profile == "ghost"),
        "expected ProfileNotConfigured, got {err:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn aws_command_failed_for_unknown_error() {
    let dir = tempfile::tempdir().unwrap();
    let aws_bin = write_aws_stub(
        dir.path(),
        1,
        "",
        "Connection timeout reaching sso-portal\n",
    );
    let ctx = RefreshContext::with_aws_bin(aws_bin);

    let err = refresh_profile_if_near_expiry(&ctx, "dev", Some(900))
        .await
        .unwrap_err();
    let RefreshError::AwsCommandFailed {
        profile,
        exit_code,
        stderr,
    } = err
    else {
        panic!("expected AwsCommandFailed, got {err:?}");
    };
    assert_eq!(profile, "dev");
    assert_eq!(exit_code, 1);
    assert!(stderr.contains("Connection timeout"));
}

#[tokio::test]
#[serial_test::serial]
async fn parse_output_when_aws_stdout_is_not_json() {
    let dir = tempfile::tempdir().unwrap();
    let aws_bin = write_aws_stub(dir.path(), 0, "definitely not json", "");
    let ctx = RefreshContext::with_aws_bin(aws_bin);

    let err = refresh_profile_if_near_expiry(&ctx, "dev", Some(900))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RefreshError::ParseOutput { ref profile, .. } if profile == "dev"),
        "expected ParseOutput, got {err:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn parse_expiration_when_aws_stdout_has_unparseable_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let stdout = r#"{
  "Version": 1,
  "AccessKeyId": "ASIAEXAMPLE",
  "SecretAccessKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
  "SessionToken": "tok",
  "Expiration": "tomorrow morning"
}
"#;
    let aws_bin = write_aws_stub(dir.path(), 0, stdout, "");
    let ctx = RefreshContext::with_aws_bin(aws_bin);

    let err = refresh_profile_if_near_expiry(&ctx, "dev", None)
        .await
        .unwrap_err();
    let RefreshError::ParseExpiration { profile, raw } = err else {
        panic!("expected ParseExpiration, got {err:?}");
    };
    assert_eq!(profile, "dev");
    assert_eq!(raw, "tomorrow morning");
}

#[tokio::test]
async fn aws_bin_not_found_when_path_is_invalid() {
    let ctx = RefreshContext::with_aws_bin(PathBuf::from(
        "/definitely/nonexistent/path/to/aws-binary-that-shouldnt-exist",
    ));

    let err = refresh_profile_if_near_expiry(&ctx, "dev", None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, RefreshError::AwsBinNotFound { .. }),
        "expected AwsBinNotFound, got {err:?}"
    );
}
