//! Gateway-side AWS SSO credential refresh.
//!
//! `zunel gateway` spawns a periodic task that, for each profile listed
//! in `aws.ssoProfiles`, runs `aws configure export-credentials --profile
//! <p> --format json` every ~10 minutes. The AWS CLI is the orchestrator:
//! it transparently re-uses cached SSO access tokens, runs the OIDC
//! `refresh_token` grant when the access token is near expiry, calls
//! `sso:GetRoleCredentials` for each requested role, and rewrites both
//! `~/.aws/sso/cache/<sha1>.json` and `~/.aws/cli/cache/<sha1>.json`. We
//! stay version-correct across `sso_session` and legacy `sso_start_url`
//! profile shapes by not touching those files ourselves.
//!
//! Mirrors the shape of [`zunel_channels::slack::bot_refresh`]: a pure
//! state-transition module with a typed [`RefreshOutcome`] /
//! [`RefreshError`], plus an `if_near_expiry` short-circuit so the
//! gateway-side caller can poll cheaply on a tick. Failures are
//! intentionally surfaced as typed errors (no `anyhow`) so the caller
//! can log differently for `SsoSessionExpired` (operator action needed)
//! vs `AwsCommandFailed` (transient, retry next tick).

use std::path::PathBuf;
use std::process::Stdio;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::process::Command;

/// Outcome of one refresh tick.
///
/// Both branches imply the AWS CLI just successfully read (and possibly
/// rewrote) `~/.aws/cli/cache/`, so any AWS SDK call in another process
/// will see fresh credentials. The distinction is purely cosmetic —
/// `Skipped` lets the gateway loop log at `debug` when the cache was
/// already comfortably ahead of the configured refresh window, and
/// `Refreshed` logs at `info` when a refresh likely just happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The cached creds still have more than `if_near_expiry` seconds
    /// of life left. The AWS CLI was nonetheless invoked (it's our only
    /// way to learn `Expiration` without re-implementing the cache file
    /// format), so the on-disk cache file mtime moves forward and any
    /// concurrent reader sees the same valid creds.
    Skipped {
        profile: String,
        secs_until_exp: i64,
        expires_at: i64,
    },
    /// The cached creds are inside the refresh window (or no window was
    /// supplied). The AWS CLI returned valid creds, having internally
    /// done whichever of "OIDC refresh-token grant" /
    /// "sso:GetRoleCredentials" / "cache hit" was needed.
    Refreshed {
        profile: String,
        secs_until_exp: i64,
        expires_at: i64,
    },
}

impl RefreshOutcome {
    pub fn profile(&self) -> &str {
        match self {
            RefreshOutcome::Skipped { profile, .. } | RefreshOutcome::Refreshed { profile, .. } => {
                profile
            }
        }
    }

    pub fn secs_until_exp(&self) -> i64 {
        match self {
            RefreshOutcome::Skipped { secs_until_exp, .. }
            | RefreshOutcome::Refreshed { secs_until_exp, .. } => *secs_until_exp,
        }
    }

    pub fn expires_at(&self) -> i64 {
        match self {
            RefreshOutcome::Skipped { expires_at, .. }
            | RefreshOutcome::Refreshed { expires_at, .. } => *expires_at,
        }
    }

    pub fn is_refreshed(&self) -> bool {
        matches!(self, Self::Refreshed { .. })
    }
}

/// Typed errors surfaced from a single refresh attempt. The gateway
/// loop pattern-matches on these so it can log at WARN with a stable,
/// operator-actionable message for [`SsoSessionExpired`] (which needs
/// a human to re-run `aws sso login`) vs at WARN-with-retry-next-tick
/// for the rest.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error(
        "aws CLI not found at `{}` (set ZUNEL_AWS_BIN to a working path or install the AWS CLI v2)",
        bin.display()
    )]
    AwsBinNotFound { bin: PathBuf },

    #[error("AWS profile `{profile}` is not configured: {stderr}")]
    ProfileNotConfigured { profile: String, stderr: String },

    #[error(
        "AWS SSO session for profile `{profile}` has expired ({stderr}); \
         re-run `aws sso login --profile {profile}`"
    )]
    SsoSessionExpired { profile: String, stderr: String },

    #[error("aws configure export-credentials --profile {profile} exited {exit_code}: {stderr}")]
    AwsCommandFailed {
        profile: String,
        exit_code: i32,
        stderr: String,
    },

    #[error("failed to spawn aws CLI for profile `{profile}`: {source}")]
    SpawnFailed {
        profile: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse aws CLI output for profile `{profile}`: {source}")]
    ParseOutput {
        profile: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("aws CLI returned an unparseable Expiration `{raw}` for profile `{profile}`")]
    ParseExpiration { profile: String, raw: String },
}

pub type RefreshResult<T> = std::result::Result<T, RefreshError>;

/// Inputs to one refresh attempt.
///
/// `aws_bin` defaults to the `aws` on `$PATH` but is overridable from
/// `ZUNEL_AWS_BIN` so deployments that prefer a pinned version (or the
/// crate's own integration tests, which substitute a stand-in shell
/// script) can redirect without touching the call sites.
#[derive(Debug, Clone)]
pub struct RefreshContext {
    pub aws_bin: PathBuf,
}

impl RefreshContext {
    /// Build a context using `ZUNEL_AWS_BIN` when set, otherwise `aws`.
    pub fn new() -> Self {
        let bin = std::env::var_os("ZUNEL_AWS_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("aws"));
        Self { aws_bin: bin }
    }

    /// Build a context with an explicit `aws` binary path. Used by the
    /// integration tests so each test can write its own stand-in script.
    pub fn with_aws_bin(aws_bin: PathBuf) -> Self {
        Self { aws_bin }
    }
}

impl Default for RefreshContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Shape of `aws configure export-credentials --format json` output.
/// Matches the standard `process-credentials` schema; we only care about
/// `Expiration` for the refresh decision but parse the whole object to
/// detect malformed responses early.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProcessCredentials {
    #[allow(dead_code)]
    version: Option<u32>,
    #[allow(dead_code)]
    access_key_id: String,
    #[allow(dead_code)]
    secret_access_key: String,
    #[allow(dead_code)]
    session_token: Option<String>,
    expiration: String,
}

/// Refresh one profile's SSO credentials when needed.
///
/// Spawns `aws configure export-credentials --profile <profile> --format
/// json` against `ctx.aws_bin`. On success parses the JSON output and
/// computes `secs_until_exp` from `Expiration`. When `if_near_expiry ==
/// Some(window)` and `secs_until_exp > window`, returns
/// [`RefreshOutcome::Skipped`]; otherwise returns
/// [`RefreshOutcome::Refreshed`].
///
/// On failure surfaces a typed [`RefreshError`] so the gateway loop can
/// fail-soft (log at WARN, keep polling) without crashing.
pub async fn refresh_profile_if_near_expiry(
    ctx: &RefreshContext,
    profile: &str,
    if_near_expiry: Option<i64>,
) -> RefreshResult<RefreshOutcome> {
    // `--format process` (NOT `--format json`) is the AWS CLI v2
    // option that emits the standard `process-credentials` JSON
    // shape we parse below. AWS CLI v2.13+ rejects `--format json`
    // outright; older v1 builds never supported it either.
    let output = Command::new(&ctx.aws_bin)
        .arg("configure")
        .arg("export-credentials")
        .arg("--profile")
        .arg(profile)
        .arg("--format")
        .arg("process")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => RefreshError::AwsBinNotFound {
                bin: ctx.aws_bin.clone(),
            },
            _ => RefreshError::SpawnFailed {
                profile: profile.to_string(),
                source,
            },
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);
        return Err(classify_failure(profile, exit_code, stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let creds: ProcessCredentials =
        serde_json::from_str(stdout.trim()).map_err(|source| RefreshError::ParseOutput {
            profile: profile.to_string(),
            source,
        })?;

    let expires_at_dt = DateTime::parse_from_rfc3339(&creds.expiration).map_err(|_| {
        RefreshError::ParseExpiration {
            profile: profile.to_string(),
            raw: creds.expiration.clone(),
        }
    })?;
    let expires_at = expires_at_dt.with_timezone(&Utc).timestamp();
    let secs_until_exp = expires_at - Utc::now().timestamp();

    if let Some(window) = if_near_expiry {
        if secs_until_exp > window {
            return Ok(RefreshOutcome::Skipped {
                profile: profile.to_string(),
                secs_until_exp,
                expires_at,
            });
        }
    }

    Ok(RefreshOutcome::Refreshed {
        profile: profile.to_string(),
        secs_until_exp,
        expires_at,
    })
}

/// Map a non-zero `aws` exit + stderr to the most actionable typed error.
///
/// Matched substrings are deliberately conservative — they target the
/// stable phrasing AWS CLI v2 has used for several releases. A novel
/// failure mode falls through to [`RefreshError::AwsCommandFailed`],
/// which the gateway loop logs and retries next tick.
fn classify_failure(profile: &str, exit_code: i32, stderr: String) -> RefreshError {
    let s = stderr.to_lowercase();
    // The first three substrings cover AWS CLI v2.0–v2.12 phrasings.
    // The fourth is the v2.13+ message
    // ("The SSO session associated with this profile has expired or
    // is otherwise invalid."), where "session" and "has expired"
    // are no longer adjacent so the literal "session has expired"
    // miss it. We require both `sso session` and an
    // expired/invalid keyword to keep this conjunctive enough to
    // avoid false positives on unrelated AWS errors that happen to
    // mention "expired" or "invalid".
    let sso_session_dead = s.contains("token has expired")
        || s.contains("session has expired")
        || s.contains("session has been invalidated")
        || (s.contains("sso session") && (s.contains("expired") || s.contains("invalid")));
    if sso_session_dead {
        return RefreshError::SsoSessionExpired {
            profile: profile.to_string(),
            stderr,
        };
    }
    if s.contains("could not be found")
        || s.contains("does not exist")
        || s.contains("profilenotfound")
    {
        return RefreshError::ProfileNotConfigured {
            profile: profile.to_string(),
            stderr,
        };
    }
    RefreshError::AwsCommandFailed {
        profile: profile.to_string(),
        exit_code,
        stderr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_failure_recognises_expired_sso_session() {
        let err = classify_failure(
            "dev",
            255,
            "Token has expired and refresh failed".to_string(),
        );
        assert!(matches!(err, RefreshError::SsoSessionExpired { .. }));
    }

    /// AWS CLI v2.13+ wording for the same condition on a legacy
    /// `sso_start_url` profile: the words "session" and "has expired"
    /// are not adjacent (`associated with this profile` sits between
    /// them), so the older substrings never match. Pin this here so
    /// the gateway's per-profile WARN log surfaces the actionable
    /// "re-run aws sso login --profile X" message instead of falling
    /// through to the generic `AwsCommandFailed` branch.
    #[test]
    fn classify_failure_recognises_session_associated_with_profile_expired() {
        let stderr = "The SSO session associated with this profile has expired or is \
             otherwise invalid. To refresh this SSO session run aws sso login with the \
             corresponding profile.";
        let err = classify_failure("da-prod", 255, stderr.to_string());
        assert!(
            matches!(err, RefreshError::SsoSessionExpired { ref profile, .. } if profile == "da-prod"),
            "expected SsoSessionExpired, got {err:?}"
        );
    }

    /// Sibling: same v2.13+ phrasing path with "is otherwise invalid"
    /// only — covers the case where AWS CLI rotates wording slightly
    /// but keeps the "sso session ... invalid" structure.
    #[test]
    fn classify_failure_recognises_sso_session_otherwise_invalid() {
        let err = classify_failure(
            "dev",
            255,
            "The SSO session associated with this profile is otherwise invalid.".to_string(),
        );
        assert!(matches!(err, RefreshError::SsoSessionExpired { .. }));
    }

    #[test]
    fn classify_failure_recognises_unknown_profile() {
        let err = classify_failure(
            "ghost",
            255,
            "The config profile (ghost) could not be found".to_string(),
        );
        assert!(matches!(err, RefreshError::ProfileNotConfigured { .. }));
    }

    #[test]
    fn classify_failure_falls_through_to_command_failed_for_unknown_stderr() {
        let err = classify_failure("dev", 1, "Connection timeout".to_string());
        assert!(matches!(err, RefreshError::AwsCommandFailed { .. }));
    }

    #[test]
    fn refresh_outcome_accessors_round_trip() {
        let r = RefreshOutcome::Refreshed {
            profile: "dev".into(),
            secs_until_exp: 600,
            expires_at: 1234,
        };
        assert_eq!(r.profile(), "dev");
        assert_eq!(r.secs_until_exp(), 600);
        assert_eq!(r.expires_at(), 1234);
        assert!(r.is_refreshed());

        let s = RefreshOutcome::Skipped {
            profile: "prod".into(),
            secs_until_exp: 7200,
            expires_at: 9999,
        };
        assert_eq!(s.profile(), "prod");
        assert!(!s.is_refreshed());
    }
}
