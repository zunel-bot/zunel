//! On-disk schema and helpers for the cached MCP OAuth tokens that
//! live under `<zunel_home>/mcp-oauth/<server>/token.json`.
//!
//! This module is intentionally **transport-agnostic**: it only knows
//! how to read/write the JSON file and answer "is this token still
//! fresh?". The actual `grant_type=refresh_token` HTTP exchange lives
//! in [`zunel-mcp::oauth`] (which has reqwest as a dep), and the
//! `authorization_code` exchange lives in `zunel-cli`'s
//! `mcp login` command.
//!
//! Keeping the schema here means **one** definition of the on-disk
//! shape that both the login flow (writer) and the agent-loop tool
//! registry (reader + refresher) agree on, so the camelCase field
//! names stay in lock-step across crates.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Default clock skew leeway when checking expiry: refresh `60s`
/// before the token would actually expire so we don't race the
/// server clock and burn an `initialize` round-trip on a 401.
pub const DEFAULT_REFRESH_SKEW_SECS: u64 = 60;

/// Cached OAuth token for a remote MCP server.
///
/// The on-disk JSON is camelCase to match what `zunel mcp login`
/// has been writing since slice 1; do not change the rename rule
/// without a migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedMcpOAuthToken {
    pub access_token: String,
    pub token_type: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub scope: Option<String>,
    pub obtained_at: u64,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub authorization_url: String,
    pub token_url: String,
}

impl CachedMcpOAuthToken {
    /// Unix timestamp (seconds) at which the access token stops
    /// being valid, or `None` when the issuer didn't return an
    /// `expires_in` (some IdPs treat the token as long-lived).
    pub fn expires_at(&self) -> Option<u64> {
        self.expires_in
            .map(|secs| self.obtained_at.saturating_add(secs))
    }

    /// Returns `true` when the token is past its expiry minus
    /// `skew_secs` of leeway. Tokens without an `expires_in` are
    /// treated as never-expired here; the caller can still attempt
    /// a refresh on a 401 if it wants belt-and-suspenders.
    pub fn is_expired(&self, now_unix: u64, skew_secs: u64) -> bool {
        match self.expires_at() {
            Some(exp) => now_unix.saturating_add(skew_secs) >= exp,
            None => false,
        }
    }

    /// Authorization header value (`{tokenType} {accessToken}`)
    /// using `Bearer` as the default token type. The fold to
    /// canonical-case `Bearer` matches what zunel-core was doing
    /// inline before this module existed.
    pub fn authorization_header(&self) -> String {
        let raw = self.token_type.as_deref().unwrap_or("Bearer");
        let token_type = if raw.eq_ignore_ascii_case("bearer") {
            "Bearer"
        } else {
            raw
        };
        format!("{token_type} {}", self.access_token)
    }
}

/// `<home>/mcp-oauth/<sanitized server>/token.json`.
pub fn mcp_oauth_token_path(home: &Path, server_name: &str) -> PathBuf {
    home.join("mcp-oauth")
        .join(safe_path_component(server_name))
        .join("token.json")
}

/// Read and decode the cached token for `server_name`, if any.
///
/// Returns `Ok(None)` for the common "no token cached yet" case so
/// callers don't have to special-case `NotFound`. Malformed JSON
/// surfaces as `Err` so we can warn and fall through instead of
/// silently masking corruption.
pub fn load_token(home: &Path, server_name: &str) -> io::Result<Option<CachedMcpOAuthToken>> {
    let path = mcp_oauth_token_path(home, server_name);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let token: CachedMcpOAuthToken = serde_json::from_slice(&bytes)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(Some(token))
}

/// Write the token cache atomically: serialize → write to a
/// sibling `*.tmp` file → `rename` over the real path. The rename
/// is atomic on POSIX and avoids leaving a half-written
/// `token.json` if the process is killed mid-write while the
/// gateway is rotating credentials in the background.
pub fn save_token(home: &Path, server_name: &str, token: &CachedMcpOAuthToken) -> io::Result<()> {
    let path = mcp_oauth_token_path(home, server_name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(token)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body)?;
    // The file holds a long-lived refresh_token + client_secret. Restrict
    // to owner-only before publishing under the real path so a malicious
    // co-tenant on a shared host can't read it via the umask-default
    // 0644 the file got from `fs::write`. Matches the same hardening
    // that `codex_refresh.rs` and `slack/bot_refresh.rs` already do.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Best-effort wall-clock seconds since the Unix epoch. Tests that
/// need a deterministic clock pass their own value into
/// [`CachedMcpOAuthToken::is_expired`] directly.
pub fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn safe_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_token() -> CachedMcpOAuthToken {
        CachedMcpOAuthToken {
            access_token: "atk".into(),
            token_type: Some("Bearer".into()),
            refresh_token: Some("rtk".into()),
            expires_in: Some(3600),
            scope: Some("read:jira-work".into()),
            obtained_at: 1_000_000,
            client_id: "cid".into(),
            client_secret: None,
            authorization_url: "https://example.test/authorize".into(),
            token_url: "https://example.test/token".into(),
        }
    }

    #[test]
    fn token_path_sanitizes_server_name() {
        let home = Path::new("/tmp/zunel");
        assert_eq!(
            mcp_oauth_token_path(home, "atlassian-jira").to_string_lossy(),
            "/tmp/zunel/mcp-oauth/atlassian-jira/token.json"
        );
        assert_eq!(
            mcp_oauth_token_path(home, "bad/name").to_string_lossy(),
            "/tmp/zunel/mcp-oauth/bad_name/token.json"
        );
    }

    #[test]
    fn is_expired_handles_skew_and_missing_ttl() {
        let mut token = sample_token();
        // expires_at = 1_000_000 + 3600 = 1_003_600
        assert!(!token.is_expired(1_000_000, 60));
        assert!(!token.is_expired(1_003_500, 60)); // 100s left, skew 60 → still fresh
        assert!(token.is_expired(1_003_540, 60)); // 60s left, hits skew → expired
        assert!(token.is_expired(1_010_000, 60));

        token.expires_in = None;
        assert!(!token.is_expired(u64::MAX / 2, 60));
    }

    #[test]
    fn authorization_header_normalizes_bearer_casing() {
        let mut token = sample_token();
        token.token_type = Some("bearer".into());
        assert_eq!(token.authorization_header(), "Bearer atk");

        token.token_type = Some("MAC".into()); // exotic; preserved verbatim
        assert_eq!(token.authorization_header(), "MAC atk");

        token.token_type = None;
        assert_eq!(token.authorization_header(), "Bearer atk");
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new().expect("tempdir");
        let token = sample_token();
        save_token(dir.path(), "atlassian-jira", &token).expect("save");

        let loaded = load_token(dir.path(), "atlassian-jira")
            .expect("load")
            .expect("present");
        assert_eq!(loaded.access_token, token.access_token);
        assert_eq!(loaded.refresh_token, token.refresh_token);
        assert_eq!(loaded.expires_in, token.expires_in);
        assert_eq!(loaded.token_url, token.token_url);
    }

    #[test]
    fn load_missing_token_returns_none() {
        let dir = TempDir::new().expect("tempdir");
        assert!(load_token(dir.path(), "nope").expect("load").is_none());
    }

    #[test]
    fn load_malformed_token_surfaces_error() {
        let dir = TempDir::new().expect("tempdir");
        let path = mcp_oauth_token_path(dir.path(), "broken");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        let err = load_token(dir.path(), "broken").expect_err("must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn save_token_writes_file_with_0600_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().expect("tempdir");
        save_token(dir.path(), "atlassian-jira", &sample_token()).expect("save");
        let path = mcp_oauth_token_path(dir.path(), "atlassian-jira");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "OAuth tokens contain long-lived refresh credentials; file mode must be 0600"
        );
    }
}
