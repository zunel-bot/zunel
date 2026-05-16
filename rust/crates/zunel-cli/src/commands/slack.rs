use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use zunel_channels::slack::bot_refresh::{
    refresh_bot_if_near_expiry, RefreshContext, RefreshOutcome,
};

use crate::cli::{SlackArgs, SlackCommand, SlackLoginArgs, SlackPostArgs, SlackRefreshBotArgs};
use crate::oauth_callback::{bind_callback_server, open_browser};

const SLACK_AUTHORIZE_URL: &str = "https://slack.com/oauth/v2/authorize";
// Default user scopes minted by `zunel slack login`. The write scopes
// (`chat:write`, `im:write`, `files:write`) are included so re-logging in
// from a fresh app install yields a token that can drive the Slack write
// tools and the `zunel slack post` CLI; runtime use of those scopes is
// still gated by `channels.slack.userTokenReadOnly` and
// `channels.slack.writeAllow`. Users who want a strictly read-only token
// can pass `--scopes` to override this list.
const DEFAULT_USER_SCOPES: &[&str] = &[
    "channels:history",
    "groups:history",
    "im:history",
    "mpim:history",
    "search:read.im",
    "search:read.mpim",
    "search:read.private",
    "search:read.public",
    "search:read.users",
    "search:read.files",
    "users:read",
    "users:read.email",
    "chat:write",
    "im:write",
    "files:write",
];

pub async fn run(args: SlackArgs, config_path: Option<&Path>) -> Result<()> {
    match args.command {
        SlackCommand::Login(args) => login(args).await,
        SlackCommand::Whoami => whoami(),
        SlackCommand::Logout => logout(),
        SlackCommand::RefreshBot(args) => refresh_bot(args, config_path).await,
        SlackCommand::Post(args) => post(args).await,
    }
}

async fn login(args: SlackLoginArgs) -> Result<()> {
    let info_path = app_info_path()?;
    let token_path = user_token_path()?;
    if !info_path.exists() {
        println!(
            "x {} not found. The zunel Slack app must be created first.",
            info_path.display()
        );
        std::process::exit(2);
    }
    if token_path.exists() && !args.force {
        println!(
            "! {} already exists. Pass --force to re-run the OAuth flow.",
            token_path.display()
        );
        return Ok(());
    }

    let app_info: Value = serde_json::from_str(
        &std::fs::read_to_string(&info_path)
            .with_context(|| format!("reading {}", info_path.display()))?,
    )
    .with_context(|| format!("parsing {}", info_path.display()))?;
    let client_id = app_info
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            println!(
                "x {} is missing client_id/client_secret.",
                info_path.display()
            );
            std::process::exit(2);
        });
    let client_secret = app_info
        .get("client_secret")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            println!(
                "x {} is missing client_id/client_secret.",
                info_path.display()
            );
            std::process::exit(2);
        });

    let scopes: Vec<String> = args
        .scopes
        .as_deref()
        .map(|scopes| {
            scopes
                .split(',')
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(|| {
            DEFAULT_USER_SCOPES
                .iter()
                .map(|scope| (*scope).to_string())
                .collect()
        });
    let state = args.state.unwrap_or_else(generate_state);
    let redirect_uri = args.redirect_uri.clone();
    let authorize_url = build_authorize_url(
        client_id,
        &scopes,
        &redirect_uri,
        &state,
        args.team.as_deref(),
    )?;

    let callback_server = match args.url_in.as_deref() {
        Some(_) => None,
        None => bind_callback_server(&redirect_uri)
            .await
            .with_context(|| format!("binding local OAuth callback server at {redirect_uri}"))?,
    };

    println!("zunel slack login");
    println!("  Scopes:   {}", scopes.join(", "));
    println!("  Redirect: {redirect_uri}");
    println!("  Token:    {}", token_path.display());
    println!("1. Visit: {authorize_url}");
    if callback_server.is_some() {
        println!("2. Approve in your browser; the callback will be captured locally.");
    } else if args.url_in.is_none() {
        println!("2. Paste the full callback URL.");
    }

    let pasted = match (args.url_in, callback_server) {
        (Some(url), _) => url,
        (None, Some(server)) => {
            if !args.no_browser {
                open_browser(&authorize_url);
            }
            println!("Waiting for OAuth callback on {redirect_uri}...");
            server
                .wait_for_callback()
                .await
                .context("capturing OAuth callback from browser")?
        }
        (None, None) => {
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context("reading callback URL from stdin")?;
            line
        }
    };
    let code = parse_callback_url(&pasted, &state).unwrap_or_else(|err| {
        println!("x {err}");
        std::process::exit(1);
    });

    let response = exchange_oauth(client_id, client_secret, &code, &redirect_uri).await?;
    let payload = token_payload(response).unwrap_or_else(|err| {
        println!("x {err}");
        std::process::exit(1);
    });
    atomic_write_json(&token_path, &payload)?;

    println!("ok User token saved to {} (0600)", token_path.display());
    println!(
        "  user_id:   {}",
        payload.get("user_id").and_then(Value::as_str).unwrap_or("")
    );
    println!(
        "  team_id:   {}",
        payload.get("team_id").and_then(Value::as_str).unwrap_or("")
    );
    println!(
        "  scopes:    {}",
        payload.get("scope").and_then(Value::as_str).unwrap_or("")
    );
    Ok(())
}

fn whoami() -> Result<()> {
    let path = user_token_path()?;
    if !path.exists() {
        println!("Slack user token missing: {}", path.display());
        return Ok(());
    }
    let value: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    let team_id = value
        .get("team_id")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/team/id").and_then(Value::as_str))
        .unwrap_or("unknown");
    let team_name = value
        .get("team_name")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/team/name").and_then(Value::as_str))
        .unwrap_or("unknown");
    let user_id = value
        .get("user_id")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/authed_user/id").and_then(Value::as_str))
        .unwrap_or("unknown");
    let token_present = value
        .get("access_token")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/authed_user/access_token")
                .and_then(Value::as_str)
        })
        .is_some_and(|token| !token.is_empty());

    println!("team: {team_name} ({team_id})");
    println!("user: {user_id}");
    println!(
        "token: {}",
        if token_present { "present" } else { "missing" }
    );
    Ok(())
}

/// `zunel slack post` — thin shell over the same `slack_post_as_me` and
/// `slack_dm_self` paths the agent uses, which means the user-token safety
/// knobs (`channels.slack.userTokenReadOnly`, `channels.slack.writeAllow`)
/// gate this command identically. If the agent isn't allowed to post into
/// a channel, neither is the human at the shell.
async fn post(args: SlackPostArgs) -> Result<()> {
    let text = match (args.text, args.stdin) {
        (Some(text), false) => text,
        (None, true) => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading message body from stdin")?;
            buf
        }
        (None, false) => {
            return Err(anyhow!(
                "missing message body: pass --text \"...\" or --stdin"
            ));
        }
        (Some(_), true) => unreachable!("clap conflicts_with prevents this"),
    };
    if text.trim().is_empty() {
        return Err(anyhow!("refusing to post an empty/whitespace-only message"));
    }

    let result = if args.to_self {
        let mut payload = json!({"text": text});
        if let Some(thread_ts) = args.thread_ts.as_deref().filter(|s| !s.is_empty()) {
            payload["thread_ts"] = json!(thread_ts);
        }
        zunel_mcp_slack::call_tool("slack_dm_self", &payload).await?
    } else {
        let channel = args
            .channel
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing target: pass --channel <ID> or --to-self"))?;
        let mut payload = json!({"channel": channel, "text": text});
        if let Some(thread_ts) = args.thread_ts.as_deref().filter(|s| !s.is_empty()) {
            payload["thread_ts"] = json!(thread_ts);
        }
        zunel_mcp_slack::call_tool("slack_post_as_me", &payload).await?
    };

    let parsed: Value =
        serde_json::from_str(&result).unwrap_or_else(|_| json!({"ok": false, "raw": result}));
    if args.json {
        println!("{}", serde_json::to_string(&parsed).unwrap_or(result));
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            std::process::exit(1);
        }
        return Ok(());
    }

    if parsed.get("ok").and_then(Value::as_bool) == Some(true) {
        let channel = parsed.get("channel").and_then(Value::as_str).unwrap_or("?");
        let ts = parsed.get("ts").and_then(Value::as_str).unwrap_or("?");
        let permalink = parsed
            .get("permalink")
            .and_then(Value::as_str)
            .unwrap_or("");
        if permalink.is_empty() {
            println!("ok posted to {channel} ts={ts}");
        } else {
            println!("ok posted to {channel} ts={ts} {permalink}");
        }
        Ok(())
    } else {
        let err = parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        println!("x post failed: {err}");
        if let Some(hint) = parsed.get("hint").and_then(Value::as_str) {
            println!("  hint: {hint}");
        }
        std::process::exit(1);
    }
}

fn logout() -> Result<()> {
    let path = user_token_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => println!("Logged out: {}", path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "No Slack user token at {}; nothing to remove.",
                path.display()
            );
        }
        Err(err) => return Err(err).with_context(|| format!("removing {}", path.display())),
    }
    Ok(())
}

fn user_token_path() -> Result<PathBuf> {
    Ok(slack_app_dir()?.join("user_token.json"))
}

fn app_info_path() -> Result<PathBuf> {
    Ok(slack_app_dir()?.join("app_info.json"))
}

fn slack_app_dir() -> Result<PathBuf> {
    Ok(zunel_config::zunel_home()?.join("slack-app-mcp"))
}

fn build_authorize_url(
    client_id: &str,
    scopes: &[String],
    redirect_uri: &str,
    state: &str,
    team: Option<&str>,
) -> Result<String> {
    let mut params = vec![
        ("client_id", client_id.to_string()),
        ("user_scope", scopes.join(",")),
        ("redirect_uri", redirect_uri.to_string()),
        ("state", state.to_string()),
    ];
    if let Some(team) = team {
        params.push(("team", team.to_string()));
    }
    Ok(reqwest::Url::parse_with_params(SLACK_AUTHORIZE_URL, params)?.to_string())
}

fn parse_callback_url(pasted: &str, expected_state: &str) -> std::result::Result<String, String> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        return Err("empty paste; expected the full URL from your browser".into());
    }
    let url =
        reqwest::Url::parse(pasted).map_err(|err| format!("malformed callback URL: {err}"))?;
    let mut code = None;
    let mut state = None;
    let mut oauth_error = None;
    let mut oauth_error_description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => oauth_error = Some(value.into_owned()),
            "error_description" => oauth_error_description = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = oauth_error {
        let desc = oauth_error_description.unwrap_or_default();
        return Err(format!("Slack authorization error: {error} {desc}")
            .trim()
            .to_string());
    }
    if state.as_deref() != Some(expected_state) {
        return Err("OAuth state mismatch; potential CSRF, aborting. Re-run `zunel slack login` to get a fresh state.".into());
    }
    code.filter(|code| !code.is_empty()).ok_or_else(|| {
        "callback URL has no 'code' parameter; expected <redirect_uri>?code=...&state=...".into()
    })
}

async fn exchange_oauth(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<Value> {
    let base = std::env::var("SLACK_API_BASE").unwrap_or_else(|_| "https://slack.com".into());
    reqwest::Client::new()
        .post(format!(
            "{}/api/oauth.v2.access",
            base.trim_end_matches('/')
        ))
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .context("Slack oauth.v2.access failed")?
        .error_for_status()
        .context("Slack oauth.v2.access failed")?
        .json::<Value>()
        .await
        .context("decoding Slack oauth.v2.access response")
}

fn token_payload(data: Value) -> std::result::Result<Value, String> {
    if data.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "Slack oauth.v2.access returned error: {}",
            data.get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
    }
    let authed_user = data
        .get("authed_user")
        .and_then(Value::as_object)
        .ok_or_else(|| "Slack oauth.v2.access returned no authed_user".to_string())?;
    let user_token = authed_user
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !(user_token.starts_with("xoxp-") || user_token.starts_with("xoxe.xoxp-")) {
        return Err("oauth.v2.access succeeded but returned no user token. Ensure user_scope (not scope) was requested.".into());
    }
    let expires_in = authed_user
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let expires_at = if expires_in > 0 {
        current_epoch_secs() + expires_in
    } else {
        0
    };
    Ok(json!({
        "access_token": user_token,
        "scope": authed_user.get("scope").and_then(Value::as_str).unwrap_or(""),
        "user_id": authed_user.get("id").and_then(Value::as_str).unwrap_or(""),
        "team_id": data.pointer("/team/id").and_then(Value::as_str).unwrap_or(""),
        "team_name": data.pointer("/team/name").and_then(Value::as_str).unwrap_or(""),
        "enterprise_id": data.pointer("/enterprise/id").and_then(Value::as_str).unwrap_or(""),
        "token_type": authed_user.get("token_type").and_then(Value::as_str).unwrap_or("user"),
        "refresh_token": authed_user.get("refresh_token").and_then(Value::as_str).unwrap_or(""),
        "expires_at": expires_at
    }))
}

fn atomic_write_json(path: &PathBuf, payload: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(payload)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn generate_state() -> String {
    let mut bytes = [0u8; 24];
    // OAuth state is the only thing standing between this login flow and
    // a CSRF; insist on the system CSPRNG.
    getrandom::fill(&mut bytes).expect("system CSPRNG must be available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn current_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// CLI wrapper around
/// [`zunel_channels::slack::bot_refresh::refresh_bot_if_near_expiry`].
///
/// All the actual state-transition lives in the library now (so the
/// long-running `zunel gateway` process can call exactly the same code
/// path on a periodic timer). This function is responsible only for
/// resolving the on-disk paths from `zunel-config`, mapping the typed
/// `RefreshError` to `anyhow::Result` at the binary edge, and printing
/// the human/JSON one-liner the existing `slack_cli_test.rs` integration
/// suite asserts on.
async fn refresh_bot(args: SlackRefreshBotArgs, config_path: Option<&Path>) -> Result<()> {
    let cfg_path = match config_path {
        Some(path) => path.to_path_buf(),
        None => zunel_config::default_config_path()?,
    };
    let home = zunel_config::zunel_home()?;
    let ctx = RefreshContext::from_zunel_home(&home, cfg_path);
    let outcome = refresh_bot_if_near_expiry(&ctx, args.if_near_expiry)
        .await
        .with_context(|| "refreshing slack bot token")?;
    print_refresh_outcome(&args, &outcome);
    Ok(())
}

fn print_refresh_outcome(args: &SlackRefreshBotArgs, outcome: &RefreshOutcome) {
    if args.json {
        let payload = match outcome {
            RefreshOutcome::Skipped {
                secs_until_exp,
                expires_at,
                ..
            } => json!({
                "ok": true,
                "skipped": true,
                "reason": "token_still_valid",
                "secs_until_exp": secs_until_exp,
                "expires_at": expires_at,
            }),
            RefreshOutcome::Refreshed {
                expires_at,
                expires_in,
                ..
            } => json!({
                "ok": true,
                "skipped": false,
                "expires_at": expires_at,
                "expires_in": expires_in,
            }),
        };
        println!(
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
        );
        return;
    }
    match outcome {
        RefreshOutcome::Skipped { secs_until_exp, .. } => {
            println!("ok skipped: bot token still valid for {secs_until_exp}s");
        }
        RefreshOutcome::Refreshed {
            expires_at,
            expires_in,
            ..
        } => {
            println!("ok refreshed: new expires_at={expires_at} (in {expires_in}s)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_state_is_24_byte_hex_and_not_constant() {
        // 24 bytes of CSPRNG output, hex-encoded → 48 lowercase-hex chars.
        let a = generate_state();
        let b = generate_state();
        assert_eq!(a.len(), 48, "unexpected state length: {a:?}");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "expected lowercase hex, got {a:?}"
        );
        // Two CSPRNG draws colliding has probability ~2^-192. If this
        // ever flakes it's because someone reintroduced a predictable
        // fallback path — not bad luck.
        assert_ne!(a, b, "two state values must differ");
    }
}
