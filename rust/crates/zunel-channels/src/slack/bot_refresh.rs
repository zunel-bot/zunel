//! Gateway-side rotating Slack bot token refresh.
//!
//! Slack's bot tokens minted via the `oauth.v2.access` flow with token
//! rotation enabled live for ~12h and must be exchanged for a new token
//! using the cached `refresh_token` before they expire. This module owns
//! the **pure** state-transition (read `app_info.json` + `config.json`,
//! optionally hit `oauth.v2.access`, atomically rewrite both files) so
//! both the long-running gateway runtime and the `zunel slack refresh-bot`
//! CLI can call exactly the same code path.
//!
//! Historically this logic lived in
//! `zunel-cli/src/commands/slack.rs::refresh_bot`. It moved here so the
//! gateway can spawn a periodic task that calls
//! [`refresh_bot_if_near_expiry`] every ~30 minutes — making the
//! external `~/.zunel/bin/run-gateway.sh` wrapper + `com.zunel.gateway-rotate`
//! 6-hour kicker LaunchAgent strictly optional. `brew services start
//! zunel-bot/tap/zunel` is now functionally equivalent to that
//! hand-rolled LaunchAgent setup.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Outcome of a refresh attempt. Carries the current bot token in
/// **both** branches so the gateway-side refresh loop can converge
/// the in-process `SlackChannel` handle to the on-disk value on
/// every tick — including the case where some other process
/// (manual `zunel slack refresh-bot`, an out-of-band launchd timer,
/// a different gateway instance) rotated the token between ticks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// `--if-near-expiry SECS` was provided and the cached token still
    /// has more than `SECS` of life left, so we did not touch Slack or
    /// the filesystem. `bot_token` is the cached value just read from
    /// `app_info.json`; the loop uses this to keep the in-process
    /// `SlackChannel` handle aligned with disk even when no new
    /// rotation happened.
    Skipped {
        secs_until_exp: i64,
        expires_at: i64,
        bot_token: String,
    },
    /// The token was exchanged. `expires_at` is the new absolute epoch
    /// seconds; `expires_in` is what Slack returned (relative seconds);
    /// `bot_token` is the freshly-minted access token Slack returned —
    /// surfaced so the gateway-side refresh loop can splice it into
    /// the live `SlackChannel` handle without re-reading
    /// `app_info.json` from disk a second time.
    Refreshed {
        expires_at: i64,
        expires_in: i64,
        bot_token: String,
    },
}

impl RefreshOutcome {
    /// Best-known current bot token, regardless of whether a rotation
    /// just happened. The gateway's refresh loop uses this to keep the
    /// in-process [`crate::slack::BotTokenHandle`] aligned with disk.
    pub fn bot_token(&self) -> &str {
        match self {
            RefreshOutcome::Skipped { bot_token, .. } => bot_token,
            RefreshOutcome::Refreshed { bot_token, .. } => bot_token,
        }
    }
}

impl RefreshOutcome {
    /// `true` when a Slack call + filesystem write happened.
    pub fn is_refreshed(&self) -> bool {
        matches!(self, Self::Refreshed { .. })
    }
}

/// Errors that can be surfaced from a refresh attempt. We use a typed
/// error here (rather than `anyhow::Error`) because the refresh runs
/// inside library code now; the CLI wrapper still re-wraps with
/// `anyhow::Context` at the binary edge.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error(
        "{} not found. The gateway-side Slack app must be installed first.",
        path.display()
    )]
    AppInfoMissing { path: PathBuf },

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

    #[error("parsing {}: {source}", path.display())]
    ParseJson {
        path: PathBuf,
        source: serde_json::Error,
    },

    #[error("{} missing required field `{field}`", path.display())]
    MissingField { path: PathBuf, field: &'static str },

    #[error("{} channels.slack is not an object", path.display())]
    ConfigSlackShape { path: PathBuf },

    #[error("Slack oauth.v2.access HTTP error: {0}")]
    SlackHttp(#[from] reqwest::Error),

    #[error("Slack oauth.v2.access returned error: {0}")]
    SlackApi(String),

    #[error("Slack oauth.v2.access response missing `{0}`")]
    SlackResponseMissing(&'static str),
}

pub type RefreshResult<T> = std::result::Result<T, RefreshError>;

/// Inputs to a refresh attempt. Built from `zunel_config::zunel_home()`
/// in production but left explicit so tests can target a tempdir.
///
/// `slack_api_base` defaults to Slack's real API but is overridable
/// from `SLACK_API_BASE` so the wiremock-based tests under
/// `zunel-cli/tests/slack_cli_test.rs` keep working unchanged.
#[derive(Debug, Clone)]
pub struct RefreshContext {
    pub app_info_path: PathBuf,
    pub config_path: PathBuf,
    pub slack_api_base: String,
}

impl RefreshContext {
    /// Build a context from `zunel-config`'s default paths. Returns
    /// `None` only when the home dir resolution fails — callers in
    /// production paths should treat that as fatal.
    pub fn from_zunel_home(home: &Path, config_path: PathBuf) -> Self {
        Self {
            app_info_path: home.join("slack-app").join("app_info.json"),
            config_path,
            slack_api_base: std::env::var("SLACK_API_BASE")
                .unwrap_or_else(|_| "https://slack.com".into()),
        }
    }
}

/// Refresh the gateway-side rotating bot token if needed.
///
/// When `if_near_expiry` is `Some(window)` and the cached token still
/// has more than `window` seconds of life left, this returns
/// [`RefreshOutcome::Skipped`] without contacting Slack or touching
/// disk. Otherwise it runs the `refresh_token` grant against
/// `oauth.v2.access`, atomically rewrites `app_info.json` (with new
/// `bot_token`/`bot_refresh_token`/`bot_token_expires_at`) and
/// `config.json` (with the new `channels.slack.botToken`), and returns
/// [`RefreshOutcome::Refreshed`].
///
/// Both atomic writes lock the resulting file to `0600` and preserve
/// existing parent directory permissions (we don't own the policy on
/// `~/.zunel/`).
pub async fn refresh_bot_if_near_expiry(
    ctx: &RefreshContext,
    if_near_expiry: Option<i64>,
) -> RefreshResult<RefreshOutcome> {
    if !ctx.app_info_path.exists() {
        return Err(RefreshError::AppInfoMissing {
            path: ctx.app_info_path.clone(),
        });
    }
    let app_info = read_json(&ctx.app_info_path)?;

    let cached_exp = app_info
        .get("bot_token_expires_at")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let secs_until_exp = cached_exp - current_epoch_secs();

    if let Some(window) = if_near_expiry {
        if secs_until_exp > window {
            // Read the on-disk `bot_token` so the loop can converge
            // its in-process handle even on a "skipped" tick — covers
            // the external-refresh case (someone ran `zunel slack
            // refresh-bot` from a launchd timer, etc.).
            let cached_bot = app_info
                .get("bot_token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Ok(RefreshOutcome::Skipped {
                secs_until_exp,
                expires_at: cached_exp,
                bot_token: cached_bot,
            });
        }
    }

    let client_id = required_field(&app_info, "client_id", &ctx.app_info_path)?;
    let client_secret = required_field(&app_info, "client_secret", &ctx.app_info_path)?;
    let refresh_token = required_field(&app_info, "bot_refresh_token", &ctx.app_info_path)?;

    let response = exchange_refresh_token(
        &ctx.slack_api_base,
        &client_id,
        &client_secret,
        &refresh_token,
    )
    .await?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        let err = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        return Err(RefreshError::SlackApi(err));
    }
    let new_bot = required_response_string(&response, "access_token")?;
    let new_refresh = required_response_string(&response, "refresh_token")?;
    let expires_in = response
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|secs| *secs > 0)
        .ok_or(RefreshError::SlackResponseMissing("expires_in"))?;
    let new_exp = current_epoch_secs() + expires_in;

    update_app_info(&ctx.app_info_path, &new_bot, &new_refresh, new_exp)?;
    update_config_bot_token(&ctx.config_path, &new_bot)?;

    Ok(RefreshOutcome::Refreshed {
        expires_at: new_exp,
        expires_in,
        bot_token: new_bot,
    })
}

async fn exchange_refresh_token(
    base: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> RefreshResult<Value> {
    let url = format!("{}/api/oauth.v2.access", base.trim_end_matches('/'));
    let value: Value = reqwest::Client::new()
        .post(url)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(value)
}

fn read_json(path: &Path) -> RefreshResult<Value> {
    let text = std::fs::read_to_string(path).map_err(|source| RefreshError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| RefreshError::ParseJson {
        path: path.to_path_buf(),
        source,
    })
}

fn update_app_info(path: &Path, bot: &str, refresh: &str, exp: i64) -> RefreshResult<()> {
    let mut value = read_json(path)?;
    let obj = value
        .as_object_mut()
        .ok_or(RefreshError::ConfigSlackShape {
            path: path.to_path_buf(),
        })?;
    obj.insert("bot_token".into(), json!(bot));
    obj.insert("bot_refresh_token".into(), json!(refresh));
    obj.insert("bot_token_expires_at".into(), json!(exp));
    write_json_preserving_parent_perms(path, &value)
}

fn update_config_bot_token(path: &Path, bot: &str) -> RefreshResult<()> {
    let mut value = read_json(path)?;
    let slack = value
        .pointer_mut("/channels/slack")
        .ok_or(RefreshError::ConfigSlackShape {
            path: path.to_path_buf(),
        })?
        .as_object_mut()
        .ok_or(RefreshError::ConfigSlackShape {
            path: path.to_path_buf(),
        })?;
    slack.insert("botToken".into(), json!(bot));
    write_json_preserving_parent_perms(path, &value)
}

/// Atomic JSON write that locks the resulting file to 0600. Mirrors the
/// long-standing `zunel-cli` helper of the same name; reproduced here
/// to keep `zunel-channels` free of `zunel-cli` dependencies.
fn write_json_preserving_parent_perms(path: &Path, payload: &Value) -> RefreshResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| RefreshError::WriteFile {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(payload).map_err(|source| RefreshError::ParseJson {
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::write(&tmp, body).map_err(|source| RefreshError::WriteFile {
        path: tmp.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|source| RefreshError::WriteFile {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn required_field(value: &Value, key: &'static str, path: &Path) -> RefreshResult<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(RefreshError::MissingField {
            path: path.to_path_buf(),
            field: key,
        })
}

fn required_response_string(value: &Value, key: &'static str) -> RefreshResult<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(RefreshError::SlackResponseMissing(key))
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
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn refreshed_when_near_expiry_window_elapsed() {
        let home = tempfile::tempdir().unwrap();
        let info = home.path().join("slack-app").join("app_info.json");
        let cfg = home.path().join("config.json");
        write_json(
            &info,
            &json!({
                "client_id":            "111.222",
                "client_secret":        "shh",
                "bot_token":            "xoxb-old",
                "bot_refresh_token":    "xoxe-1-old",
                "bot_token_expires_at": 1, // already expired
            }),
        );
        write_json(
            &cfg,
            &json!({
                "channels": {"slack": {"botToken": "xoxb-old", "appToken": "xapp-keep"}}
            }),
        );

        let slack = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth.v2.access"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=xoxe-1-old"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "access_token":  "xoxb-fresh",
                "refresh_token": "xoxe-1-fresh",
                "expires_in":    43200,
                "token_type":    "bot",
                "scope":         "chat:write,reactions:write"
            })))
            .mount(&slack)
            .await;

        let ctx = RefreshContext {
            app_info_path: info.clone(),
            config_path: cfg.clone(),
            slack_api_base: slack.uri(),
        };
        let outcome = refresh_bot_if_near_expiry(&ctx, Some(1800)).await.unwrap();
        let RefreshOutcome::Refreshed { bot_token, .. } = &outcome else {
            panic!("expected Refreshed, got {outcome:?}");
        };
        assert_eq!(
            bot_token, "xoxb-fresh",
            "Refreshed must surface the freshly-minted token so the gateway loop can hot-swap in-process"
        );

        let info_v: Value = serde_json::from_str(&std::fs::read_to_string(&info).unwrap()).unwrap();
        assert_eq!(info_v["bot_token"], "xoxb-fresh");
        assert_eq!(info_v["bot_refresh_token"], "xoxe-1-fresh");

        let cfg_v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(cfg_v["channels"]["slack"]["botToken"], "xoxb-fresh");
        assert_eq!(cfg_v["channels"]["slack"]["appToken"], "xapp-keep");
    }

    #[tokio::test]
    async fn skipped_when_token_outside_window() {
        let home = tempfile::tempdir().unwrap();
        let info = home.path().join("slack-app").join("app_info.json");
        let cfg = home.path().join("config.json");
        let still_valid = current_epoch_secs() + 7200;
        write_json(
            &info,
            &json!({
                "client_id":            "111.222",
                "client_secret":        "shh",
                "bot_token":            "xoxb-still-good",
                "bot_refresh_token":    "xoxe-1-still-good",
                "bot_token_expires_at": still_valid,
            }),
        );
        write_json(
            &cfg,
            &json!({"channels": {"slack": {"botToken": "xoxb-still-good"}}}),
        );

        let ctx = RefreshContext {
            app_info_path: info.clone(),
            config_path: cfg.clone(),
            slack_api_base: "http://127.0.0.1:1".into(), // any: not contacted
        };
        let outcome = refresh_bot_if_near_expiry(&ctx, Some(1800)).await.unwrap();
        let RefreshOutcome::Skipped { bot_token, .. } = &outcome else {
            panic!("expected Skipped, got {outcome:?}");
        };
        assert_eq!(
            bot_token, "xoxb-still-good",
            "Skipped must carry the on-disk bot token so the gateway loop can converge its in-process handle"
        );

        // Files untouched.
        let info_v: Value = serde_json::from_str(&std::fs::read_to_string(&info).unwrap()).unwrap();
        assert_eq!(info_v["bot_token"], "xoxb-still-good");
    }

    #[tokio::test]
    async fn missing_app_info_yields_typed_error() {
        let home = tempfile::tempdir().unwrap();
        let info = home.path().join("slack-app").join("app_info.json"); // doesn't exist
        let cfg = home.path().join("config.json");
        write_json(&cfg, &json!({"channels": {"slack": {}}}));

        let ctx = RefreshContext {
            app_info_path: info.clone(),
            config_path: cfg.clone(),
            slack_api_base: "http://127.0.0.1:1".into(),
        };
        let err = refresh_bot_if_near_expiry(&ctx, None).await.unwrap_err();
        assert!(matches!(err, RefreshError::AppInfoMissing { .. }));
    }

    #[tokio::test]
    async fn slack_error_response_surfaces_typed_error() {
        let home = tempfile::tempdir().unwrap();
        let info = home.path().join("slack-app").join("app_info.json");
        let cfg = home.path().join("config.json");
        write_json(
            &info,
            &json!({
                "client_id":            "111.222",
                "client_secret":        "shh",
                "bot_token":            "xoxb-old",
                "bot_refresh_token":    "xoxe-1-old",
                "bot_token_expires_at": 1,
            }),
        );
        write_json(
            &cfg,
            &json!({"channels": {"slack": {"botToken": "xoxb-old"}}}),
        );

        let slack = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth.v2.access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": false, "error": "invalid_refresh_token"
            })))
            .mount(&slack)
            .await;

        let ctx = RefreshContext {
            app_info_path: info,
            config_path: cfg,
            slack_api_base: slack.uri(),
        };
        let err = refresh_bot_if_near_expiry(&ctx, None).await.unwrap_err();
        assert!(matches!(err, RefreshError::SlackApi(ref msg) if msg == "invalid_refresh_token"));
    }
}
