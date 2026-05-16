//! Gateway-side ChatGPT-Codex OAuth token refresh.
//!
//! The bare `codex` CLI mints a 10-day access_token plus a long-lived
//! `refresh_token` and stashes both in `~/.codex/auth.json` (see
//! [`crate::codex::FileCodexAuthProvider`]). The CLI itself only
//! refreshes that access_token when *something invokes the CLI* — there
//! is no `codex` daemon. Long-running zunel gateways (`brew services
//! start zunel`, `zunel gateway` directly) therefore drift: they read
//! `auth.json` cold on every request and just re-send whatever
//! access_token was last persisted, eventually getting back HTTP 401
//! once the token's `exp` claim passes.
//!
//! This module mirrors the shape of [`zunel_channels::slack::bot_refresh`]
//! / [`zunel_aws::sso_refresh`] / [`zunel_mcp::oauth`]: a pure
//! state-transition module exposing [`refresh_if_near_expiry`] with a
//! typed [`RefreshOutcome`] / [`RefreshError`]. The gateway loop in
//! `zunel-cli/src/commands/gateway.rs` polls it every ~30 minutes and
//! atomically rewrites `auth.json` when the cached access_token is
//! within an hour of expiry.
//!
//! ## Wire contract
//!
//! Lifted from the public `openai/codex` crate
//! (`codex-rs/login/src/auth/manager.rs`, commit 9a8730f3) so this stays
//! byte-compatible with anything `codex login` produces:
//!
//! - `POST https://auth.openai.com/oauth/token` (overridable via
//!   `CODEX_REFRESH_TOKEN_URL_OVERRIDE`, same env var the CLI honours).
//! - `Content-Type: application/json`.
//! - Body: `{"client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
//!           "grant_type": "refresh_token",
//!           "refresh_token": "..."}`.
//! - Response: `{"id_token": "...", "access_token": "...",
//!              "refresh_token": "..."}` — every field is optional;
//!   we only overwrite `auth.json` keys that the IdP returned.
//! - `401` with body `{"error": "refresh_token_expired"}` (or
//!   `_reused` / `_invalidated`) is permanent: the user must re-run
//!   `codex login`. We surface those as
//!   [`RefreshError::RefreshTokenRejected`] so the gateway loop can
//!   log a stable, operator-actionable message.
//!
//! ## On-disk preservation
//!
//! `auth.json` carries fields zunel doesn't model
//! (`OPENAI_API_KEY`, nested `account_id` shapes seen in the wild —
//! see `tests/codex_auth_test.rs`). We round-trip the whole file as
//! `serde_json::Value` and only mutate the four keys we own
//! (`tokens.{access_token,id_token,refresh_token}`, `last_refresh`)
//! so downstream tools (`codex` itself, the codex desktop app, etc.)
//! see no surprise drift.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

/// Default Codex refresh endpoint. Matches the upstream constant
/// `REFRESH_TOKEN_URL` in `codex-rs/login/src/auth/manager.rs`. The
/// `CODEX_REFRESH_TOKEN_URL_OVERRIDE` env var swaps this for tests
/// or air-gapped deployments — the same env var the codex CLI checks.
pub const DEFAULT_REFRESH_ENDPOINT: &str = "https://auth.openai.com/oauth/token";

/// The same env var the codex CLI honours (named so a single export
/// transparently retargets both the codex CLI and zunel's loop).
pub const REFRESH_ENDPOINT_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";

/// Public OAuth client id baked into the codex CLI. Lifted from the
/// upstream `CLIENT_ID` constant in `codex-rs/login/src/auth/manager.rs`.
/// Stable across codex releases; if OpenAI ever rotates this, the
/// codex CLI itself stops working too — at which point any zunel user
/// will be reaching for `codex login` regardless.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// HTTP timeout for the refresh round-trip. Sized for a single-shot
/// JSON POST against `auth.openai.com` with no streaming; the codex
/// CLI uses an even tighter budget (its default reqwest timeout)
/// but we're more conservative to absorb gateway-side network blips.
const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Outcome of one refresh tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// `auth.json` does not exist at all — user has never run
    /// `codex login`. The loop logs once at debug and falls silent.
    NoAuthFile,

    /// `auth.json` has `auth_mode != "chatgpt"` (e.g. raw API-key
    /// mode). There is nothing to refresh; API keys don't expire.
    NotChatgptMode,

    /// `auth.json` exists but lacks `tokens.refresh_token`. Only
    /// `codex login` can recover.
    NoRefreshToken,

    /// `auth.json` exists but `tokens.access_token` isn't a JWT or
    /// has no `exp` claim. The loop treats this as "skip until the
    /// next manual `codex login` overwrites it".
    UnknownExpiry,

    /// `if_near_expiry` was supplied and the cached access_token
    /// still has more than that many seconds of life left. We did
    /// not contact the IdP or touch disk.
    Skipped {
        secs_until_exp: i64,
        expires_at: i64,
    },

    /// The refresh round-trip succeeded; `auth.json` has been
    /// rewritten in place. `secs_until_exp` is computed from the
    /// **new** access_token's `exp` claim (or `None` when the IdP
    /// declined to return a fresh access_token, which the upstream
    /// code accepts).
    Refreshed {
        secs_until_exp: Option<i64>,
        expires_at: Option<i64>,
    },
}

/// Typed errors surfaced from a single refresh attempt. The gateway
/// loop pattern-matches on these so it can log differently for
/// permanent failures (`RefreshTokenRejected` — operator must
/// `codex login` again) vs transient ones (network, 5xx — retry
/// next tick).
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("reading {}: {source}", path.display())]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("writing {}: {source}", path.display())]
    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("parsing {} as JSON: {source}", path.display())]
    ParseAuthFile {
        path: PathBuf,
        source: serde_json::Error,
    },

    #[error("Codex refresh endpoint POST failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Codex refresh endpoint returned non-JSON body: {0}")]
    DecodeResponse(String),

    #[error(
        "Codex refresh token rejected ({reason}); user must re-run `codex login` \
         (server returned: {detail})"
    )]
    RefreshTokenRejected {
        reason: RefreshTokenRejectionReason,
        detail: String,
    },

    #[error("Codex refresh endpoint returned HTTP {status}: {body_snippet}")]
    BackendStatus { status: u16, body_snippet: String },
}

/// Sub-classification of a 401 from the Codex refresh endpoint.
/// Mirrors the codex CLI's `RefreshTokenFailedReason` so log lines
/// stay congruent with what users see when running `codex` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshTokenRejectionReason {
    /// `refresh_token_expired` — the refresh_token's own TTL elapsed.
    Expired,
    /// `refresh_token_reused` — the rotation chain was broken (a
    /// stale refresh_token was presented).
    Exhausted,
    /// `refresh_token_invalidated` — server-side revocation, typically
    /// from a logout-everywhere or password change.
    Revoked,
    /// Any other 401 we couldn't classify.
    Other,
}

impl std::fmt::Display for RefreshTokenRejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Expired => "refresh_token_expired",
            Self::Exhausted => "refresh_token_reused",
            Self::Revoked => "refresh_token_invalidated",
            Self::Other => "other",
        };
        f.write_str(s)
    }
}

pub type RefreshResult<T> = std::result::Result<T, RefreshError>;

/// Inputs to a refresh attempt. Built from `~/.codex/auth.json` in
/// production but left explicit so tests can target a tempdir + a
/// `wiremock::MockServer`-backed endpoint.
#[derive(Debug, Clone)]
pub struct RefreshContext {
    pub auth_path: PathBuf,
    pub refresh_endpoint: String,
    pub client_id: String,
}

impl RefreshContext {
    /// Build a context anchored at `<codex_home>/auth.json`. Honours
    /// `CODEX_REFRESH_TOKEN_URL_OVERRIDE` so a single env var swaps
    /// both the codex CLI and zunel's refresh loop in lockstep.
    pub fn from_codex_home(codex_home: &Path) -> Self {
        Self {
            auth_path: codex_home.join("auth.json"),
            refresh_endpoint: std::env::var(REFRESH_ENDPOINT_OVERRIDE_ENV_VAR)
                .unwrap_or_else(|_| DEFAULT_REFRESH_ENDPOINT.to_string()),
            client_id: CLIENT_ID.to_string(),
        }
    }
}

/// Refresh the cached Codex access_token if it's within `if_near_expiry`
/// seconds of expiring (or unconditionally when `if_near_expiry == None`).
///
/// This is fail-soft against missing/half-formed `auth.json` files: a
/// brand-new gateway with no `~/.codex/auth.json` returns
/// [`RefreshOutcome::NoAuthFile`], not an error, so the periodic loop
/// can log once at debug and stay quiet.
pub async fn refresh_if_near_expiry(
    ctx: &RefreshContext,
    if_near_expiry: Option<i64>,
) -> RefreshResult<RefreshOutcome> {
    refresh_if_near_expiry_with(
        ctx,
        if_near_expiry,
        current_epoch_secs(),
        &reqwest::Client::builder()
            .timeout(REFRESH_HTTP_TIMEOUT)
            .build()
            .map_err(RefreshError::from)?,
    )
    .await
}

/// Test-friendly variant of [`refresh_if_near_expiry`] that injects a
/// deterministic clock and `reqwest::Client` (so the unit tests can
/// point one at a `wiremock::MockServer`).
pub async fn refresh_if_near_expiry_with(
    ctx: &RefreshContext,
    if_near_expiry: Option<i64>,
    now_unix: i64,
    client: &reqwest::Client,
) -> RefreshResult<RefreshOutcome> {
    if !ctx.auth_path.exists() {
        return Ok(RefreshOutcome::NoAuthFile);
    }
    let mut value = read_auth_json(&ctx.auth_path)?;

    // Only the chatgpt-OAuth flow has a refresh_token; raw API-key
    // mode (`auth_mode == "apikey"`) just reads `OPENAI_API_KEY`.
    if value
        .get("auth_mode")
        .and_then(Value::as_str)
        .is_some_and(|m| m != "chatgpt")
    {
        return Ok(RefreshOutcome::NotChatgptMode);
    }

    let Some(refresh_token) = value
        .pointer("/tokens/refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return Ok(RefreshOutcome::NoRefreshToken);
    };

    let cached_access = value
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let cached_exp = jwt_exp(cached_access);

    if let (Some(window), Some(exp)) = (if_near_expiry, cached_exp) {
        let secs_until_exp = exp - now_unix;
        if secs_until_exp > window {
            return Ok(RefreshOutcome::Skipped {
                secs_until_exp,
                expires_at: exp,
            });
        }
    } else if cached_exp.is_none() && if_near_expiry.is_some() {
        // We were asked to gate on remaining lifetime but we can't
        // read it. Fail-soft: do nothing this tick and let the user
        // re-run `codex login` if their access_token shape ever
        // diverges from a JWT-with-exp.
        return Ok(RefreshOutcome::UnknownExpiry);
    }

    let response = exchange_refresh_token(client, ctx, &refresh_token).await?;

    apply_refresh(&mut value, &response);
    write_auth_json(&ctx.auth_path, &value)?;

    let new_exp = response
        .access_token
        .as_deref()
        .and_then(jwt_exp)
        .or_else(|| {
            value
                .pointer("/tokens/access_token")
                .and_then(Value::as_str)
                .and_then(jwt_exp)
        });
    Ok(RefreshOutcome::Refreshed {
        secs_until_exp: new_exp.map(|exp| exp - now_unix),
        expires_at: new_exp,
    })
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

async fn exchange_refresh_token(
    client: &reqwest::Client,
    ctx: &RefreshContext,
    refresh_token: &str,
) -> RefreshResult<RefreshResponse> {
    let body = json!({
        "client_id": ctx.client_id,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    });
    let response = client
        .post(&ctx.refresh_endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    // OAuth token responses are tiny (a few KB at most); cap at 64 KiB
    // so a buggy or hostile IdP can't OOM the refresh task.
    let body_text = zunel_util::read_text_capped(response, 64 * 1024)
        .await
        .map_err(|err| RefreshError::DecodeResponse(format!("body read failed: {err}")))?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        let reason = classify_refresh_failure(&body_text);
        return Err(RefreshError::RefreshTokenRejected {
            reason,
            detail: snippet(&body_text),
        });
    }
    if !status.is_success() {
        return Err(RefreshError::BackendStatus {
            status: status.as_u16(),
            body_snippet: snippet(&body_text),
        });
    }
    serde_json::from_str::<RefreshResponse>(&body_text)
        .map_err(|err| RefreshError::DecodeResponse(format!("{err}")))
}

fn classify_refresh_failure(body: &str) -> RefreshTokenRejectionReason {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return RefreshTokenRejectionReason::Other;
    };
    let code = value
        .get("error")
        .and_then(|err| match err {
            Value::String(s) => Some(s.clone()),
            Value::Object(obj) => obj
                .get("code")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            _ => None,
        })
        .or_else(|| {
            value
                .get("code")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
    match code.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("refresh_token_expired") => RefreshTokenRejectionReason::Expired,
        Some("refresh_token_reused") => RefreshTokenRejectionReason::Exhausted,
        Some("refresh_token_invalidated") => RefreshTokenRejectionReason::Revoked,
        _ => RefreshTokenRejectionReason::Other,
    }
}

/// Apply the refresh response to the round-tripped `auth.json` value
/// in place. We only overwrite fields the IdP returned (the upstream
/// `persist_tokens` does the same), so callers that have manually
/// edited unrelated keys see no drift.
fn apply_refresh(value: &mut Value, response: &RefreshResponse) {
    let tokens = value
        .as_object_mut()
        .map(|obj| obj.entry("tokens").or_insert_with(|| json!({})))
        .and_then(Value::as_object_mut);
    let Some(tokens) = tokens else {
        return;
    };
    if let Some(access) = &response.access_token {
        tokens.insert("access_token".into(), json!(access));
    }
    if let Some(id_token) = &response.id_token {
        tokens.insert("id_token".into(), json!(id_token));
    }
    if let Some(refresh) = &response.refresh_token {
        tokens.insert("refresh_token".into(), json!(refresh));
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "last_refresh".into(),
            json!(chrono::Utc::now().to_rfc3339()),
        );
    }
}

fn read_auth_json(path: &Path) -> RefreshResult<Value> {
    let text = std::fs::read_to_string(path).map_err(|source| RefreshError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| RefreshError::ParseAuthFile {
        path: path.to_path_buf(),
        source,
    })
}

fn write_auth_json(path: &Path, value: &Value) -> RefreshResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| RefreshError::WriteFile {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let body = serde_json::to_string_pretty(value).map_err(|err| RefreshError::WriteFile {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, err),
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|source| RefreshError::WriteFile {
        path: tmp.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 0600 matches what `codex login` writes; keeping the perms
        // tight is the whole reason we don't just `serde_json::to_writer`
        // the existing path.
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|source| RefreshError::WriteFile {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Decode the `exp` claim of a JWT. Returns `None` when the input
/// isn't a three-part JWT, the payload isn't valid base64url-no-pad,
/// or the payload doesn't carry an integer `exp` field. We never
/// verify the signature — the IdP enforces that on the wire, and
/// zunel only reads the claim to size its refresh window.
fn jwt_exp(jwt: &str) -> Option<i64> {
    let mut parts = jwt.split('.');
    let (_header, payload, _sig) = (parts.next()?, parts.next()?, parts.next()?);
    if payload.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp").and_then(Value::as_i64)
}

fn snippet(body: &str) -> String {
    body.chars()
        .take(256)
        .collect::<String>()
        .trim()
        .to_string()
}

fn current_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;
    use std::path::Path;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Fabricate a JWT whose payload carries the given `exp` claim.
    /// We don't touch header / signature; both are opaque to the
    /// `jwt_exp` helper.
    fn jwt_with_exp(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"exp": exp})).unwrap());
        format!("h.{payload}.s")
    }

    fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn jwt_exp_round_trips_through_url_safe_base64_no_pad() {
        // exp values in the wild are ~10-digit unix epoch seconds;
        // pick one that exercises >32-bit math.
        let exp = 1_777_827_011_i64;
        let jwt = jwt_with_exp(exp);
        assert_eq!(jwt_exp(&jwt), Some(exp));
    }

    #[test]
    fn jwt_exp_returns_none_for_non_jwt_strings() {
        assert_eq!(jwt_exp(""), None);
        assert_eq!(jwt_exp("not-a-jwt"), None);
        assert_eq!(jwt_exp("only.two"), None, "two-part input is not a JWT");
        assert_eq!(jwt_exp("h..s"), None, "empty payload is not a JWT");
        // base64-decodable but not JSON
        assert_eq!(jwt_exp("h.QUJD.s"), None);
    }

    #[test]
    fn jwt_exp_returns_none_when_payload_lacks_exp() {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub": "x"})).unwrap());
        assert_eq!(jwt_exp(&format!("h.{payload}.s")), None);
    }

    #[test]
    fn classify_refresh_failure_reads_object_and_string_error_shapes() {
        for body in [
            r#"{"error": "refresh_token_expired"}"#,
            r#"{"error": {"code": "refresh_token_expired"}}"#,
            r#"{"code": "refresh_token_expired"}"#,
        ] {
            assert_eq!(
                classify_refresh_failure(body),
                RefreshTokenRejectionReason::Expired,
                "body: {body}"
            );
        }
        assert_eq!(
            classify_refresh_failure(r#"{"error": "refresh_token_reused"}"#),
            RefreshTokenRejectionReason::Exhausted
        );
        assert_eq!(
            classify_refresh_failure(r#"{"error": "refresh_token_invalidated"}"#),
            RefreshTokenRejectionReason::Revoked
        );
        assert_eq!(
            classify_refresh_failure(r#"{"error": "something_else"}"#),
            RefreshTokenRejectionReason::Other
        );
        assert_eq!(
            classify_refresh_failure("not json"),
            RefreshTokenRejectionReason::Other
        );
    }

    #[tokio::test]
    async fn no_auth_file_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = RefreshContext {
            auth_path: dir.path().join("auth.json"),
            refresh_endpoint: "http://127.0.0.1:1".into(),
            client_id: CLIENT_ID.into(),
        };
        let outcome = refresh_if_near_expiry_with(&ctx, Some(3600), 0, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(outcome, RefreshOutcome::NoAuthFile);
    }

    #[tokio::test]
    async fn api_key_mode_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        write_json(
            &auth,
            &json!({
                "auth_mode": "apikey",
                "OPENAI_API_KEY": "sk-test"
            }),
        );
        let ctx = RefreshContext {
            auth_path: auth,
            refresh_endpoint: "http://127.0.0.1:1".into(),
            client_id: CLIENT_ID.into(),
        };
        let outcome = refresh_if_near_expiry_with(&ctx, Some(3600), 0, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(outcome, RefreshOutcome::NotChatgptMode);
    }

    #[tokio::test]
    async fn no_refresh_token_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        write_json(
            &auth,
            &json!({
                "auth_mode": "chatgpt",
                "tokens": {"access_token": jwt_with_exp(0)}
            }),
        );
        let ctx = RefreshContext {
            auth_path: auth,
            refresh_endpoint: "http://127.0.0.1:1".into(),
            client_id: CLIENT_ID.into(),
        };
        let outcome = refresh_if_near_expiry_with(&ctx, Some(3600), 0, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(outcome, RefreshOutcome::NoRefreshToken);
    }

    #[tokio::test]
    async fn skips_when_token_is_outside_window() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let now = 1_000_000_i64;
        let exp = now + 7200;
        write_json(
            &auth,
            &json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": jwt_with_exp(exp),
                    "refresh_token": "rt-keep",
                    "id_token": "id-keep"
                }
            }),
        );
        let ctx = RefreshContext {
            auth_path: auth.clone(),
            refresh_endpoint: "http://127.0.0.1:1".into(), // would fail if hit
            client_id: CLIENT_ID.into(),
        };
        let outcome = refresh_if_near_expiry_with(&ctx, Some(3600), now, &reqwest::Client::new())
            .await
            .unwrap();
        match outcome {
            RefreshOutcome::Skipped {
                secs_until_exp,
                expires_at,
            } => {
                assert_eq!(secs_until_exp, 7200);
                assert_eq!(expires_at, exp);
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
        // File untouched — refresh_token, id_token preserved verbatim.
        let after = read_json(&auth);
        assert_eq!(after["tokens"]["refresh_token"], "rt-keep");
        assert_eq!(after["tokens"]["id_token"], "id-keep");
    }

    #[tokio::test]
    async fn refreshes_when_inside_window_and_rewrites_auth_json() {
        let server = MockServer::start().await;
        let new_exp = 9_999_999_i64;
        let new_access = jwt_with_exp(new_exp);
        let new_id = jwt_with_exp(new_exp + 60);
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(header("content-type", "application/json"))
            .and(body_string_contains(r#""grant_type":"refresh_token""#))
            .and(body_string_contains(CLIENT_ID))
            .and(body_string_contains("rt-old"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id_token":      new_id,
                "access_token":  new_access,
                "refresh_token": "rt-new",
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let now = 1_000_000_i64;
        write_json(
            &auth,
            &json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "account_id": "acct_keep",
                "last_refresh": "2026-04-23T16:50:12Z",
                "tokens": {
                    "access_token":  jwt_with_exp(now + 60), // ~expiring
                    "refresh_token": "rt-old",
                    "id_token":      "id-old",
                    "account_id":    "acct_nested"
                }
            }),
        );
        let ctx = RefreshContext {
            auth_path: auth.clone(),
            refresh_endpoint: format!("{}/oauth/token", server.uri()),
            client_id: CLIENT_ID.into(),
        };
        let outcome = refresh_if_near_expiry_with(&ctx, Some(3600), now, &reqwest::Client::new())
            .await
            .unwrap();
        let secs = match outcome {
            RefreshOutcome::Refreshed {
                secs_until_exp,
                expires_at,
            } => {
                assert_eq!(expires_at, Some(new_exp));
                secs_until_exp.expect("computed from new access_token JWT")
            }
            other => panic!("expected Refreshed, got {other:?}"),
        };
        assert_eq!(secs, new_exp - now);

        let after = read_json(&auth);
        // Three rotated fields applied verbatim from the IdP response.
        assert_eq!(after["tokens"]["access_token"], new_access);
        assert_eq!(after["tokens"]["refresh_token"], "rt-new");
        assert_eq!(after["tokens"]["id_token"], new_id);
        // Untouched fields preserved verbatim. This is the key
        // invariant — the codex CLI / desktop app may stash extra
        // metadata in here that we don't model.
        assert_eq!(after["auth_mode"], "chatgpt");
        assert_eq!(after["account_id"], "acct_keep");
        assert_eq!(after["tokens"]["account_id"], "acct_nested");
        // last_refresh moved forward (we don't pin to a fake clock
        // here; just assert it changed).
        assert_ne!(after["last_refresh"], "2026-04-23T16:50:12Z");
        let last = after["last_refresh"]
            .as_str()
            .expect("last_refresh is a string");
        assert!(
            chrono::DateTime::parse_from_rfc3339(last).is_ok(),
            "last_refresh `{last}` must round-trip RFC3339"
        );
    }

    #[tokio::test]
    async fn refresh_keeps_existing_refresh_token_when_response_omits_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": jwt_with_exp(9_999_999),
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let now = 1_000_000_i64;
        write_json(
            &auth,
            &json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token":  jwt_with_exp(now + 60),
                    "refresh_token": "rt-keep",
                    "id_token":      "id-keep"
                }
            }),
        );
        let ctx = RefreshContext {
            auth_path: auth.clone(),
            refresh_endpoint: format!("{}/oauth/token", server.uri()),
            client_id: CLIENT_ID.into(),
        };
        let _ = refresh_if_near_expiry_with(&ctx, Some(3600), now, &reqwest::Client::new())
            .await
            .unwrap();
        let after = read_json(&auth);
        // refresh_token + id_token preserved when IdP omits them.
        assert_eq!(after["tokens"]["refresh_token"], "rt-keep");
        assert_eq!(after["tokens"]["id_token"], "id-keep");
    }

    #[tokio::test]
    async fn refresh_endpoint_401_classifies_to_typed_rejection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "refresh_token_expired",
                "error_description": "refresh token has expired"
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let now = 1_000_000_i64;
        write_json(
            &auth,
            &json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token":  jwt_with_exp(now + 60),
                    "refresh_token": "rt-stale"
                }
            }),
        );
        let ctx = RefreshContext {
            auth_path: auth.clone(),
            refresh_endpoint: format!("{}/oauth/token", server.uri()),
            client_id: CLIENT_ID.into(),
        };
        let err = refresh_if_near_expiry_with(&ctx, Some(3600), now, &reqwest::Client::new())
            .await
            .unwrap_err();
        let RefreshError::RefreshTokenRejected { reason, detail } = err else {
            panic!("expected RefreshTokenRejected, got {err}");
        };
        assert_eq!(reason, RefreshTokenRejectionReason::Expired);
        assert!(detail.contains("refresh_token_expired"), "{detail}");

        // On a permanent failure we leave the on-disk file untouched
        // so the user can still see what they had when running
        // `codex login` for the recovery.
        let after = read_json(&auth);
        assert_eq!(after["tokens"]["refresh_token"], "rt-stale");
    }

    #[tokio::test]
    async fn refresh_endpoint_500_surfaces_typed_backend_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(503).set_body_string("temporarily unavailable"))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let now = 1_000_000_i64;
        write_json(
            &auth,
            &json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token":  jwt_with_exp(now + 60),
                    "refresh_token": "rt-stale"
                }
            }),
        );
        let ctx = RefreshContext {
            auth_path: auth,
            refresh_endpoint: format!("{}/oauth/token", server.uri()),
            client_id: CLIENT_ID.into(),
        };
        let err = refresh_if_near_expiry_with(&ctx, Some(3600), now, &reqwest::Client::new())
            .await
            .unwrap_err();
        let RefreshError::BackendStatus {
            status,
            body_snippet,
        } = err
        else {
            panic!("expected BackendStatus, got {err}");
        };
        assert_eq!(status, 503);
        assert!(body_snippet.contains("unavailable"), "{body_snippet}");
    }

    #[tokio::test]
    async fn from_codex_home_honours_endpoint_override_env() {
        let dir = tempfile::tempdir().unwrap();
        let prior = std::env::var_os(REFRESH_ENDPOINT_OVERRIDE_ENV_VAR);
        std::env::set_var(REFRESH_ENDPOINT_OVERRIDE_ENV_VAR, "http://override.test/x");
        let ctx = RefreshContext::from_codex_home(dir.path());
        match prior {
            Some(v) => std::env::set_var(REFRESH_ENDPOINT_OVERRIDE_ENV_VAR, v),
            None => std::env::remove_var(REFRESH_ENDPOINT_OVERRIDE_ENV_VAR),
        }
        assert_eq!(ctx.refresh_endpoint, "http://override.test/x");
        assert_eq!(ctx.auth_path, dir.path().join("auth.json"));
        assert_eq!(ctx.client_id, CLIENT_ID);
    }
}
