use std::fs;
use std::path::Path;
use std::time::Duration;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Child;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Maximum attempts for the local-callback bind. Cross-binary parallelism
/// during `cargo test` can briefly steal the OS-picked ephemeral port
/// between our probe and the spawned child binding it, so we retry on
/// EADDRINUSE up to this many times before declaring a real failure.
const SLACK_LOGIN_BIND_ATTEMPTS: usize = 5;

/// EADDRINUSE markers we accept as a "retry, the port got snatched"
/// signal. Stderr from `bind_callback_server` already includes the
/// `Address already in use` clause; the second variant is hedge for
/// platforms whose `std::io::Error` rendering differs.
const EADDRINUSE_MARKERS: &[&str] = &[
    "Address already in use",
    "address already in use",
    "AddrInUse",
    "EADDRINUSE",
];

/// Convenience alias for "the handles a bound child needs to hand
/// off to the rest of the test." Lives at module scope so we can
/// reuse it from both `BindOutcome` and the retry-loop's
/// `Option<...>` accumulator without redeclaring the tuple.
type BoundChild = (
    Child,
    BufReader<tokio::process::ChildStdout>,
    tokio::task::JoinHandle<Vec<u8>>,
    String,
    String,
);

/// Outcome of a single spawn-and-wait-for-bind attempt. We keep this
/// type local to the test file because it's not generally useful and
/// lets us return either the now-healthy child handles to the rest of
/// the test or a structured retry signal without panicking mid-loop.
///
/// The `Bound` variant is intentionally large (it carries the full
/// child handle plus its stdio plumbing); we silence
/// `clippy::large_enum_variant` because every code path here either
/// constructs `Bound` exactly once and immediately destructures it,
/// or constructs the small variants and discards them. The size
/// asymmetry doesn't translate into wasted heap traffic.
#[allow(clippy::large_enum_variant)]
enum BindOutcome {
    Bound {
        child: Child,
        reader: BufReader<tokio::process::ChildStdout>,
        stderr_drain: tokio::task::JoinHandle<Vec<u8>>,
        transcript: String,
        redirect_uri: String,
    },
    BindFailedRetry {
        stderr: String,
    },
    Fatal {
        reason: String,
    },
}

/// Spawn `zunel slack login` against a freshly-picked loopback port
/// and wait until the child either prints `"Waiting for OAuth callback"`
/// (meaning the local callback server is bound and ready) or exits
/// with EADDRINUSE on stderr (meaning the port got stolen between
/// `pick_free_port` and the child's `bind`). The two cases drive the
/// retry loop in `slack_login_completes_oauth_flow_via_local_callback_server`.
async fn try_spawn_slack_login_with_callback(home: &Path, slack_uri: &str) -> BindOutcome {
    let probe = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(probe) => probe,
        Err(err) => {
            return BindOutcome::Fatal {
                reason: format!("probe bind failed: {err}"),
            };
        }
    };
    let port = match probe.local_addr() {
        Ok(addr) => addr.port(),
        Err(err) => {
            return BindOutcome::Fatal {
                reason: format!("probe local_addr failed: {err}"),
            };
        }
    };
    drop(probe);
    let redirect_uri = format!("https://127.0.0.1:{port}/slack/callback");
    let bin = assert_cmd::cargo::cargo_bin("zunel");

    let mut child = match tokio::process::Command::new(&bin)
        .env("ZUNEL_HOME", home)
        .env("SLACK_API_BASE", slack_uri)
        .args([
            "slack",
            "login",
            "--state",
            "loop-state",
            "--no-browser",
            "--redirect-uri",
            &redirect_uri,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return BindOutcome::Fatal {
                reason: format!("spawn zunel: {err}"),
            };
        }
    };

    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    let stderr_drain = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = BufReader::new(stderr).read_to_end(&mut buf).await;
        buf
    });

    let mut reader = BufReader::new(stdout);
    let mut transcript = String::new();
    loop {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(15), reader.read_line(&mut line)).await;
        match read {
            Ok(Ok(0)) => {
                // Child closed stdout before signaling readiness; this
                // is the EADDRINUSE branch. We need stderr to decide
                // whether to retry, so pull the drain task.
                let stderr_bytes = stderr_drain.await.unwrap_or_default();
                let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();
                if EADDRINUSE_MARKERS.iter().any(|m| stderr_str.contains(m))
                    || transcript
                        .lines()
                        .any(|l| EADDRINUSE_MARKERS.iter().any(|m| l.contains(m)))
                {
                    return BindOutcome::BindFailedRetry { stderr: stderr_str };
                }
                return BindOutcome::Fatal {
                    reason: format!(
                        "child exited before bind without an EADDRINUSE signal. \
                         transcript=\n{transcript}\nstderr=\n{stderr_str}"
                    ),
                };
            }
            Ok(Ok(_)) => {
                transcript.push_str(&line);
                if line.contains("Waiting for OAuth callback") {
                    return BindOutcome::Bound {
                        child,
                        reader,
                        stderr_drain,
                        transcript,
                        redirect_uri,
                    };
                }
            }
            Ok(Err(err)) => {
                return BindOutcome::Fatal {
                    reason: format!("stdout read failed: {err}"),
                };
            }
            Err(_) => {
                return BindOutcome::Fatal {
                    reason: format!(
                        "timed out waiting for callback-bind announcement. \
                         transcript so far=\n{transcript}"
                    ),
                };
            }
        }
    }
}

#[test]
fn slack_whoami_reports_cached_user_token_identity() {
    let home = tempfile::tempdir().unwrap();
    let token_path = home.path().join("slack-app-mcp").join("user_token.json");
    fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    fs::write(
        &token_path,
        serde_json::to_string_pretty(&json!({
            "team": {"id": "T1", "name": "Team One"},
            "authed_user": {"id": "U1", "access_token": "xoxp-secret"}
        }))
        .unwrap(),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .args(["slack", "whoami"])
        .assert()
        .success()
        .stdout(contains("team: Team One (T1)"))
        .stdout(contains("user: U1"))
        .stdout(contains("token: present"));
}

#[test]
fn slack_logout_deletes_cached_user_token() {
    let home = tempfile::tempdir().unwrap();
    let token_path = home.path().join("slack-app-mcp").join("user_token.json");
    fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    fs::write(&token_path, "{}").unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .args(["slack", "logout"])
        .assert()
        .success()
        .stdout(contains("Logged out"));

    assert!(!token_path.exists());
}

#[tokio::test]
async fn slack_login_exchanges_callback_and_writes_user_token_file() {
    let home = tempfile::tempdir().unwrap();
    let app_dir = home.path().join("slack-app-mcp");
    let token_path = app_dir.join("user_token.json");
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(
        app_dir.join("app_info.json"),
        serde_json::to_string_pretty(&json!({
            "client_id": "111.222",
            "client_secret": "shh"
        }))
        .unwrap(),
    )
    .unwrap();

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "authed_user": {
                "id": "U1",
                "access_token": "xoxp-user-token",
                "scope": "channels:history,users:read",
                "token_type": "user",
                "expires_in": 3600,
                "refresh_token": "refresh-1"
            },
            "team": {"id": "T1", "name": "Team One"},
            "enterprise": {"id": "E1"}
        })))
        .mount(&slack)
        .await;

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args([
            "slack",
            "login",
            "--state",
            "test-state",
            "--url",
            "https://slack.com/robots.txt?code=abc&state=test-state",
        ])
        .assert()
        .success()
        .stdout(contains("User token saved"))
        .stdout(contains("user_id:   U1"))
        .stdout(contains("team_id:   T1"));

    let token: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&token_path).unwrap()).unwrap();
    assert_eq!(token["access_token"], "xoxp-user-token");
    assert_eq!(token["user_id"], "U1");
    assert_eq!(token["team_id"], "T1");
    assert_eq!(token["team_name"], "Team One");
    assert_eq!(token["enterprise_id"], "E1");
    assert_eq!(token["refresh_token"], "refresh-1");
    assert!(token["expires_at"].as_i64().unwrap() > 0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&token_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn slack_login_forwards_custom_redirect_uri_to_token_exchange() {
    let home = tempfile::tempdir().unwrap();
    let app_dir = home.path().join("slack-app-mcp");
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(
        app_dir.join("app_info.json"),
        serde_json::to_string_pretty(&json!({
            "client_id": "111.222",
            "client_secret": "shh"
        }))
        .unwrap(),
    )
    .unwrap();

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains(
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A53682%2Fslack%2Fcallback",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "authed_user": {
                "id": "U2",
                "access_token": "xoxp-loop",
                "scope": "users:read",
                "token_type": "user"
            },
            "team": {"id": "T2", "name": "Team Two"}
        })))
        .mount(&slack)
        .await;

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args([
            "slack",
            "login",
            "--state",
            "test-state",
            "--redirect-uri",
            "http://127.0.0.1:53682/slack/callback",
            "--url",
            "http://127.0.0.1:53682/slack/callback?code=abc&state=test-state",
        ])
        .assert()
        .success()
        .stdout(contains("User token saved"));
}

#[tokio::test]
async fn slack_login_completes_oauth_flow_via_local_callback_server() {
    let home = tempfile::tempdir().unwrap();
    let app_dir = home.path().join("slack-app-mcp");
    let token_path = app_dir.join("user_token.json");
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(
        app_dir.join("app_info.json"),
        serde_json::to_string_pretty(&json!({
            "client_id": "111.222",
            "client_secret": "shh"
        }))
        .unwrap(),
    )
    .unwrap();

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "authed_user": {
                "id": "U7",
                "access_token": "xoxp-auto",
                "scope": "channels:history,users:read",
                "token_type": "user",
                "expires_in": 3600,
                "refresh_token": "refresh-auto"
            },
            "team": {"id": "T9", "name": "Auto Team"},
            "enterprise": {"id": "E0"}
        })))
        .mount(&slack)
        .await;

    let lenient_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("build lenient reqwest client");

    // Spawn the callback-server child up to N times: cross-binary
    // parallelism in `cargo test` can briefly steal the OS-picked
    // ephemeral port between our probe and the child's `bind`. Once
    // we have a child past the bind step we proceed with the full
    // OAuth flow exactly once.
    let (mut child, mut reader, stderr_drain, mut transcript, redirect_uri) = {
        let mut last_stderr = String::new();
        let mut bound: Option<BoundChild> = None;
        for attempt in 1..=SLACK_LOGIN_BIND_ATTEMPTS {
            match try_spawn_slack_login_with_callback(home.path(), &slack.uri()).await {
                BindOutcome::Bound {
                    child,
                    reader,
                    stderr_drain,
                    transcript,
                    redirect_uri,
                } => {
                    bound = Some((child, reader, stderr_drain, transcript, redirect_uri));
                    break;
                }
                BindOutcome::BindFailedRetry { stderr } => {
                    eprintln!(
                        "slack_login attempt {attempt}/{SLACK_LOGIN_BIND_ATTEMPTS} \
                         hit EADDRINUSE; retrying. stderr=\n{stderr}"
                    );
                    last_stderr = stderr;
                    continue;
                }
                BindOutcome::Fatal { reason } => {
                    panic!("slack_login fatal during bind: {reason}");
                }
            }
        }
        bound.unwrap_or_else(|| {
            panic!(
                "slack_login could not bind callback server after \
                 {SLACK_LOGIN_BIND_ATTEMPTS} attempts. last stderr=\n{last_stderr}"
            )
        })
    };

    // The child has already printed "Waiting for OAuth callback" by
    // construction; kick the canned callback into the local server.
    let url = format!("{redirect_uri}?code=loop-code&state=loop-state");
    {
        let client = lenient_client.clone();
        tokio::spawn(async move {
            let _ = client.get(&url).send().await;
        });
    }

    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(15), reader.read_line(&mut line))
            .await
            .expect("timed out waiting for slack login stdout")
            .expect("read line failed");
        if n == 0 {
            break;
        }
        transcript.push_str(&line);
        if line.contains("User token saved") || line.starts_with("x ") {
            break;
        }
    }

    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("zunel slack login did not exit")
        .expect("collecting child exit status");
    let stderr_bytes = stderr_drain.await.expect("stderr drain task");
    assert!(
        status.success(),
        "exit={:?} transcript=\n{transcript}\nstderr=\n{}",
        status,
        String::from_utf8_lossy(&stderr_bytes)
    );
    // The callback-marker assertion that used to live here is now
    // implicit: `try_spawn_slack_login_with_callback` only returns
    // `BindOutcome::Bound` after it has read that exact line.
    assert!(
        transcript.contains("User token saved"),
        "expected success line in transcript=\n{transcript}"
    );

    let token: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&token_path).unwrap()).unwrap();
    assert_eq!(token["access_token"], "xoxp-auto");
    assert_eq!(token["user_id"], "U7");
    assert_eq!(token["team_id"], "T9");
    assert_eq!(token["team_name"], "Auto Team");
    assert_eq!(token["refresh_token"], "refresh-auto");
}

#[test]
fn slack_login_rejects_missing_app_info_with_python_exit_code() {
    let home = tempfile::tempdir().unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .args([
            "slack",
            "login",
            "--state",
            "test-state",
            "--url",
            "https://slack.com/robots.txt?code=abc&state=test-state",
        ])
        .assert()
        .code(2)
        .stdout(contains("app_info.json not found"));
}

#[tokio::test]
async fn slack_refresh_bot_exchanges_refresh_token_and_writes_back() {
    let home = tempfile::tempdir().unwrap();
    let app_dir = home.path().join("slack-app");
    fs::create_dir_all(&app_dir).unwrap();
    let info_path = app_dir.join("app_info.json");
    fs::write(
        &info_path,
        serde_json::to_string_pretty(&json!({
            "client_id":           "111.222",
            "client_secret":       "shh",
            "bot_token":           "xoxb-old",
            "bot_refresh_token":   "xoxe-1-old",
            "bot_token_expires_at": 1, // already expired
        }))
        .unwrap(),
    )
    .unwrap();

    let cfg_path = home.path().join("config.json");
    fs::write(
        &cfg_path,
        serde_json::to_string_pretty(&json!({
            "channels": {
                "slack": {
                    "enabled":  true,
                    "mode":     "socket",
                    "botToken": "xoxb-old",
                    "appToken": "xapp-keep-me"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=xoxe-1-old"))
        .and(body_string_contains("client_id=111.222"))
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

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args(["slack", "refresh-bot"])
        .assert()
        .success()
        .stdout(contains("refreshed"))
        .stdout(contains("expires_at="));

    let info: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&info_path).unwrap()).unwrap();
    assert_eq!(info["bot_token"], "xoxb-fresh");
    assert_eq!(info["bot_refresh_token"], "xoxe-1-fresh");
    let new_exp = info["bot_token_expires_at"].as_i64().unwrap();
    assert!(
        new_exp > 100_000,
        "expected expires_at to be a real future timestamp, got {new_exp}"
    );

    let cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg_path).unwrap()).unwrap();
    assert_eq!(cfg["channels"]["slack"]["botToken"], "xoxb-fresh");
    // Sibling fields under channels.slack must be preserved.
    assert_eq!(cfg["channels"]["slack"]["appToken"], "xapp-keep-me");
    assert_eq!(cfg["channels"]["slack"]["mode"], "socket");
    assert_eq!(cfg["channels"]["slack"]["enabled"], true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&info_path).unwrap().permissions().mode() & 0o777,
            0o600,
            "app_info.json should land at 0600 after refresh"
        );
        assert_eq!(
            fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777,
            0o600,
            "config.json should land at 0600 after refresh"
        );
    }
}

#[tokio::test]
async fn slack_refresh_bot_skips_when_token_is_outside_near_expiry_window() {
    let home = tempfile::tempdir().unwrap();
    let app_dir = home.path().join("slack-app");
    fs::create_dir_all(&app_dir).unwrap();
    let info_path = app_dir.join("app_info.json");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let still_valid = now + 7200; // 2h in the future
    fs::write(
        &info_path,
        serde_json::to_string_pretty(&json!({
            "client_id":           "111.222",
            "client_secret":       "shh",
            "bot_token":           "xoxb-still-good",
            "bot_refresh_token":   "xoxe-1-still-good",
            "bot_token_expires_at": still_valid,
        }))
        .unwrap(),
    )
    .unwrap();

    let cfg_path = home.path().join("config.json");
    fs::write(
        &cfg_path,
        serde_json::to_string_pretty(&json!({
            "channels": {
                "slack": {
                    "enabled":  true,
                    "mode":     "socket",
                    "botToken": "xoxb-still-good"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    // Mock server WITHOUT a matcher for oauth.v2.access. If the command
    // hits Slack despite the skip window, it gets a 404 and the test
    // fails; success here proves the skip path made no network call.
    let slack = MockServer::start().await;

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args(["slack", "refresh-bot", "--if-near-expiry", "1800"])
        .assert()
        .success()
        .stdout(contains("skipped"));

    let info: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&info_path).unwrap()).unwrap();
    assert_eq!(info["bot_token"], "xoxb-still-good");
    assert_eq!(info["bot_refresh_token"], "xoxe-1-still-good");
    assert_eq!(
        info["bot_token_expires_at"].as_i64().unwrap(),
        still_valid,
        "skip path must not rewrite expires_at"
    );
}

/// Helper: write a minimal user-token file plus an optional config.json
/// pinning the safety knobs. Returns the temp home so the caller can
/// pass it via `ZUNEL_HOME`.
fn slack_post_test_home(safety_block: Option<&str>) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let token_dir = home.path().join("slack-app-mcp");
    fs::create_dir_all(&token_dir).unwrap();
    fs::write(
        token_dir.join("user_token.json"),
        serde_json::to_string_pretty(&json!({
            "access_token": "xoxp-cli-post-token",
            "user_id": "UCLI",
            "team_id": "TCLI"
        }))
        .unwrap(),
    )
    .unwrap();
    if let Some(safety) = safety_block {
        fs::write(
            home.path().join("config.json"),
            format!(
                r#"{{
                    "providers": {{}},
                    "agents": {{"defaults": {{"model": "m"}}}},
                    "channels": {{"slack": {safety}}}
                }}"#
            ),
        )
        .unwrap();
    }
    home
}

/// `zunel slack post --channel C... --text "..."` happy path: posts via
/// `chat.postMessage`, prints the resolved channel/permalink one-liner,
/// and never echoes the bearer token.
#[tokio::test]
async fn slack_post_to_channel_calls_chat_postmessage_and_prints_permalink() {
    let home = slack_post_test_home(None);
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(body_string_contains("channel=C42"))
        .and(body_string_contains("text=hello"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "channel": "C42",
            "ts": "1713974400.000100"
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.getPermalink"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "permalink": "https://slack.example/cli-msg"
        })))
        .mount(&slack)
        .await;

    let assert = Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args(["slack", "post", "--channel", "C42", "--text", "hello"])
        .assert()
        .success()
        .stdout(contains("ok posted to C42"))
        .stdout(contains("https://slack.example/cli-msg"));
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    assert!(
        !stdout.contains("xoxp-cli-post-token"),
        "bearer token must never reach stdout: {stdout}"
    );
}

/// `--to-self` resolves the authenticated user_id from the cached token
/// file and posts to that DM. The agent's `slack_dm_self` MCP tool runs
/// the same path; this test guards both.
#[tokio::test]
async fn slack_post_to_self_resolves_user_id_and_posts() {
    let home = slack_post_test_home(None);
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(body_string_contains("channel=UCLI"))
        .and(body_string_contains("text=note+to+self"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "channel": "DCLI",
            "ts": "1713974400.000200"
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.getPermalink"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "permalink": "https://slack.example/self"
        })))
        .mount(&slack)
        .await;

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args(["slack", "post", "--to-self", "--text", "note to self"])
        .assert()
        .success()
        .stdout(contains("ok posted to DCLI"));
}

/// When `userTokenReadOnly: true`, the CLI inherits the same refusal as
/// the agent's MCP path: no Slack API call is made and the command exits
/// non-zero with a hint pointing at the config flag.
#[tokio::test]
async fn slack_post_refuses_when_user_token_read_only_is_set() {
    let home = slack_post_test_home(Some(r#"{"enabled": true, "userTokenReadOnly": true}"#));
    // No mounts on this MockServer: any HTTP call returns 404 and would
    // tank the assertion. Success means the refusal short-circuited
    // before reaching the network.
    let slack = MockServer::start().await;

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args([
            "slack",
            "post",
            "--channel",
            "C42",
            "--text",
            "should refuse",
        ])
        .assert()
        .failure()
        .stdout(contains("user_token_read_only"));
}

/// When `writeAllow` is set and the target isn't on it, the CLI refuses
/// before the network and surfaces the allowlist as a hint. This is the
/// allowlist-scoped-write defense layered on top of the binary read-only
/// switch.
#[tokio::test]
async fn slack_post_refuses_when_target_not_in_write_allow() {
    let home = slack_post_test_home(Some(r#"{"enabled": true, "writeAllow": ["UCLI"]}"#));
    let slack = MockServer::start().await;

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args([
            "slack",
            "post",
            "--channel",
            "CSomeoneElse",
            "--text",
            "nope",
        ])
        .assert()
        .failure()
        .stdout(contains("channel_not_in_write_allow"));
}

/// `writeAllow` listing the authenticated user's own ID makes the
/// `--to-self` shortcut work while still blocking sends to anyone else.
/// This is the "agent can DM me but no one else" posture from the docs.
#[tokio::test]
async fn slack_post_to_self_works_when_user_id_is_in_write_allow() {
    let home = slack_post_test_home(Some(r#"{"enabled": true, "writeAllow": ["UCLI"]}"#));
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(body_string_contains("channel=UCLI"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "channel": "DCLI",
            "ts": "1713974400.000300"
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.getPermalink"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "permalink": "https://slack.example/allowed-self"
        })))
        .mount(&slack)
        .await;

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args(["slack", "post", "--to-self", "--text", "allowed"])
        .assert()
        .success()
        .stdout(contains("ok posted to DCLI"));
}

/// `zunel slack login` (no `--scopes` override) must include the write
/// scopes in the `user_scope=` query parameter so a fresh app install
/// yields a token that can drive `slack_post_as_me` / `slack_dm_self`
/// and the `zunel slack post` CLI. Runtime use of those scopes stays
/// gated by `userTokenReadOnly` + `writeAllow`.
#[tokio::test]
async fn slack_login_default_scopes_request_chat_im_files_write() {
    let home = tempfile::tempdir().unwrap();
    let app_dir = home.path().join("slack-app-mcp");
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(
        app_dir.join("app_info.json"),
        serde_json::to_string_pretty(&json!({
            "client_id": "111.222",
            "client_secret": "shh"
        }))
        .unwrap(),
    )
    .unwrap();

    // Stub oauth.v2.access so the command exits successfully after
    // printing the authorize URL — we don't actually care about the
    // exchange result here, only the URL-shaped scope set.
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "authed_user": {
                "id": "U1",
                "access_token": "xoxp-default-scopes",
                "scope": "channels:history,users:read,chat:write,im:write,files:write",
                "token_type": "user",
                "expires_in": 3600
            },
            "team": {"id": "T1", "name": "Team One"}
        })))
        .mount(&slack)
        .await;

    let assert = Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args([
            "slack",
            "login",
            "--state",
            "test-state",
            "--url",
            "https://slack.com/robots.txt?code=abc&state=test-state",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    for scope in ["chat%3Awrite", "im%3Awrite", "files%3Awrite"] {
        assert!(
            stdout.contains(scope),
            "default authorize URL must include {scope}; stdout=\n{stdout}"
        );
    }
}

/// When the cached user token is expired AND the cached refresh_token is
/// rejected by Slack (`invalid_refresh_token`, the typical "you went on
/// vacation longer than the rotation window" case), the CLI's
/// `x post failed: …` line should name the underlying refresh error and
/// point at the remediation, not just say `token_expired` and leave the
/// user wondering which knob to turn.
#[tokio::test]
async fn slack_post_surfaces_refresh_failure_with_remediation_hint() {
    let home = tempfile::tempdir().unwrap();
    let token_dir = home.path().join("slack-app-mcp");
    fs::create_dir_all(&token_dir).unwrap();
    fs::write(
        token_dir.join("app_info.json"),
        serde_json::to_string_pretty(&json!({
            "client_id": "111.222",
            "client_secret": "secret"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        token_dir.join("user_token.json"),
        serde_json::to_string_pretty(&json!({
            "access_token": "xoxp-cli-stale-token",
            "refresh_token": "refresh-revoked",
            "expires_at": 1,
            "user_id": "UCLI",
            "team_id": "TCLI"
        }))
        .unwrap(),
    )
    .unwrap();

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": false,
            "error": "invalid_refresh_token"
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": false,
            "error": "token_expired"
        })))
        .mount(&slack)
        .await;

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .env("SLACK_API_BASE", slack.uri())
        .args(["slack", "post", "--to-self", "--text", "remind me"])
        .assert()
        .failure()
        .stdout(contains("invalid_refresh_token"))
        .stdout(contains("zunel slack login --force"));
}

/// Empty/whitespace-only message bodies are rejected before any RPC,
/// matching the agent-side `empty_text` refusal so cron pipelines can't
/// accidentally spam Slack with whitespace when their command produces
/// no output.
#[tokio::test]
async fn slack_post_rejects_empty_body() {
    let home = slack_post_test_home(None);
    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .args(["slack", "post", "--channel", "C42", "--text", "   "])
        .assert()
        .failure()
        .stderr(contains("empty"));
}
