//! Tests for the workspace foot-gun guard
//! ([`zunel_config::guard_workspace`]).
//!
//! These tests share a single `serial(zunel_home_env)` lock with
//! the existing `workspace_path_test.rs` suite because both
//! mutate `ZUNEL_HOME` and `HOME` (and the new escape-hatch env
//! var) on the process. Running them under one named lock means
//! they don't poison each other when `cargo test` runs the
//! integration target with multiple threads.

use std::ffi::OsString;
use std::path::Path;

use serial_test::serial;
use zunel_config::{guard_workspace, Error, UNSAFE_WORKSPACE_ENV};

/// RAII guard for an env var: snapshots the value on entry,
/// optionally sets a new value, and restores the original on
/// drop. Avoids "test A leaks an env var into test B" failures
/// even when the test panics partway through.
struct EnvGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }

    fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(val) => std::env::set_var(self.key, val),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Helper that pins `ZUNEL_HOME` and `HOME` to ephemeral
/// tempdirs and clears the escape hatch so each test starts
/// from a known-good baseline. Returns the guards (drop order
/// matters: env vars are restored on drop).
fn pin_env() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    EnvGuard,
    EnvGuard,
    EnvGuard,
) {
    let zunel_home = tempfile::tempdir().unwrap();
    let user_home = tempfile::tempdir().unwrap();
    let zh = EnvGuard::set("ZUNEL_HOME", zunel_home.path());
    let h = EnvGuard::set("HOME", user_home.path());
    let bypass = EnvGuard::unset(UNSAFE_WORKSPACE_ENV);
    (zunel_home, user_home, zh, h, bypass)
}

#[test]
#[serial(zunel_home_env)]
fn safe_workspace_passes() {
    let (zunel_home, _user_home, _zh, _h, _b) = pin_env();
    let safe = zunel_home.path().join("workspace");
    guard_workspace(&safe).expect("a sibling under ZUNEL_HOME is safe");
}

#[test]
#[serial(zunel_home_env)]
fn workspace_in_unrelated_tempdir_passes() {
    let (_zunel_home, _user_home, _zh, _h, _b) = pin_env();
    let elsewhere = tempfile::tempdir().unwrap();
    guard_workspace(elsewhere.path()).expect("an unrelated tempdir is safe");
}

#[test]
#[serial(zunel_home_env)]
fn filesystem_root_is_rejected() {
    let (_zunel_home, _user_home, _zh, _h, _b) = pin_env();
    let err = guard_workspace(Path::new("/")).expect_err("`/` must be rejected");
    assert!(
        matches!(err, Error::UnsafeWorkspace { .. }),
        "expected UnsafeWorkspace, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("filesystem root"),
        "diagnostic should explain why; got {msg:?}"
    );
}

#[test]
#[serial(zunel_home_env)]
fn user_home_is_rejected() {
    let (_zunel_home, user_home, _zh, _h, _b) = pin_env();
    let err = guard_workspace(user_home.path()).expect_err("$HOME exactly must be rejected");
    let msg = err.to_string();
    assert!(matches!(err, Error::UnsafeWorkspace { .. }));
    assert!(
        msg.contains("home directory"),
        "diagnostic should call out $HOME; got {msg:?}"
    );
}

#[test]
#[serial(zunel_home_env)]
fn zunel_home_itself_is_rejected() {
    let (zunel_home, _user_home, _zh, _h, _b) = pin_env();
    let err =
        guard_workspace(zunel_home.path()).expect_err("workspace == ZUNEL_HOME must be rejected");
    let msg = err.to_string();
    assert!(matches!(err, Error::UnsafeWorkspace { .. }));
    assert!(
        msg.contains("zunel runtime home"),
        "diagnostic should call out the runtime home overlap; got {msg:?}"
    );
}

#[test]
#[serial(zunel_home_env)]
fn ancestor_of_zunel_home_is_rejected() {
    // Construct ZUNEL_HOME at a deep child so its parent is a
    // real, plausible workspace path that nonetheless contains
    // the runtime home.
    let parent = tempfile::tempdir().unwrap();
    let zunel_home = parent.path().join("inner").join(".zunel");
    std::fs::create_dir_all(&zunel_home).unwrap();
    let _zh = EnvGuard::set("ZUNEL_HOME", &zunel_home);
    let user_home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", user_home.path());
    let _b = EnvGuard::unset(UNSAFE_WORKSPACE_ENV);

    let err = guard_workspace(parent.path()).expect_err("ancestor of ZUNEL_HOME must be rejected");
    let msg = err.to_string();
    assert!(matches!(err, Error::UnsafeWorkspace { .. }));
    assert!(
        msg.contains("zunel runtime home"),
        "diagnostic should call out the runtime home overlap; got {msg:?}"
    );
}

#[test]
#[serial(zunel_home_env)]
fn sibling_with_shared_prefix_is_not_rejected() {
    // Regression for the `Path::starts_with` foot-gun: "/foo"
    // must not be treated as an ancestor of "/foobar" just
    // because the strings share a prefix. We engineer that
    // collision by making ZUNEL_HOME live at `<tmp>/zunel` and
    // pointing the workspace at `<tmp>/zunelagent`.
    let tmp = tempfile::tempdir().unwrap();
    let zunel_home = tmp.path().join("zunel");
    std::fs::create_dir_all(&zunel_home).unwrap();
    let workspace = tmp.path().join("zunelagent");
    std::fs::create_dir_all(&workspace).unwrap();

    let _zh = EnvGuard::set("ZUNEL_HOME", &zunel_home);
    let user_home = tempfile::tempdir().unwrap();
    let _h = EnvGuard::set("HOME", user_home.path());
    let _b = EnvGuard::unset(UNSAFE_WORKSPACE_ENV);

    guard_workspace(&workspace).expect("string-prefix-but-not-ancestor must pass");
}

#[test]
#[serial(zunel_home_env)]
fn escape_hatch_overrides_root() {
    let (_zunel_home, _user_home, _zh, _h, _b) = pin_env();
    let _bypass = EnvGuard::set(UNSAFE_WORKSPACE_ENV, "1");
    guard_workspace(Path::new("/")).expect("escape hatch must let `/` through");
}

#[test]
#[serial(zunel_home_env)]
fn escape_hatch_overrides_home() {
    let (_zunel_home, user_home, _zh, _h, _b) = pin_env();
    let _bypass = EnvGuard::set(UNSAFE_WORKSPACE_ENV, "yes");
    guard_workspace(user_home.path()).expect("escape hatch must let $HOME through");
}

#[test]
#[serial(zunel_home_env)]
fn empty_escape_hatch_does_not_bypass() {
    // We treat the env var as a presence-or-absence toggle, but
    // an explicitly-empty value is the same as "not set" so an
    // operator who exports `ZUNEL_ALLOW_UNSAFE_WORKSPACE=` (a
    // shell habit for clearing a var) doesn't accidentally
    // disable the guard.
    let (_zunel_home, _user_home, _zh, _h, _b) = pin_env();
    let _bypass = EnvGuard::set(UNSAFE_WORKSPACE_ENV, "");
    let err = guard_workspace(Path::new("/")).expect_err("empty value must not bypass");
    assert!(matches!(err, Error::UnsafeWorkspace { .. }));
}
