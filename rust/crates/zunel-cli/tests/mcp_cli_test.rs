use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command;
use std::collections::BTreeMap;
use std::fs;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zunel_mcp::StdioMcpClient;

#[tokio::test]
async fn cli_mcp_serve_self_exposes_self_status_tool() {
    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "self".to_string(),
    ];
    let mut client = StdioMcpClient::connect(
        bin.to_string_lossy().as_ref(),
        &args,
        BTreeMap::new(),
        &[],
        5,
    )
    .await
    .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert_eq!(tools[0].name, "self_status");

    let result = client
        .call_tool("self_status", serde_json::json!({}), 5)
        .await
        .unwrap();
    assert!(result.contains("zunel-self ok"), "{result}");
}

#[tokio::test]
async fn cli_mcp_serve_self_exposes_chat_oauth_login_tools() {
    // Regression: v0.2.6 shipped the `mcp_login_start` / `mcp_login_complete`
    // self-MCP handlers in zunel-mcp-self::handlers but forgot to plumb them
    // through zunel-cli's parallel serve.rs/tools.rs surface. The bundled
    // `zunel mcp serve --server self` is the path users actually configure
    // in `~/.zunel/config.json`, so this test pins both surfaces together.
    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "self".to_string(),
    ];
    let mut client = StdioMcpClient::connect(
        bin.to_string_lossy().as_ref(),
        &args,
        BTreeMap::new(),
        &[],
        5,
    )
    .await
    .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"mcp_login_start"),
        "expected mcp_login_start to be exposed by `zunel mcp serve --server self`; got {names:?}"
    );
    assert!(
        names.contains(&"mcp_login_complete"),
        "expected mcp_login_complete to be exposed by `zunel mcp serve --server self`; got {names:?}"
    );
}

#[tokio::test]
async fn cli_mcp_login_caches_remote_oauth_token() {
    let home = tempfile::tempdir().unwrap();
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=callback-code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "token-1",
            "refresh_token": "refresh-1",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "mcp"
        })))
        .mount(&auth)
        .await;

    fs::write(
        home.path().join("config.json"),
        format!(
            r#"{{
                "providers": {{}},
                "agents": {{"defaults": {{"model": "m"}}}},
                "tools": {{"mcpServers": {{"remote": {{
                    "type": "streamableHttp",
                    "url": "{base}/mcp",
                    "oauth": {{
                        "enabled": true,
                        "clientId": "client-1",
                        "authorizationUrl": "{base}/authorize",
                        "tokenUrl": "{base}/token",
                        "scope": "mcp",
                        "redirectUri": "http://127.0.0.1:33419/callback"
                    }}
                }}}}}}
            }}"#,
            base = auth.uri()
        ),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .args([
            "mcp",
            "login",
            "remote",
            "--state",
            "state-1",
            "--url",
            "http://127.0.0.1:33419/callback?code=callback-code&state=state-1",
        ])
        .assert()
        .success();

    let token_path = home
        .path()
        .join("mcp-oauth")
        .join("remote")
        .join("token.json");
    let token: serde_json::Value = serde_json::from_slice(&fs::read(token_path).unwrap()).unwrap();
    assert_eq!(token["accessToken"], "token-1");
    assert_eq!(token["refreshToken"], "refresh-1");
    assert_eq!(token["clientId"], "client-1");
}

#[tokio::test]
async fn cli_mcp_login_discovers_oauth_metadata_and_registers_client() {
    let home = tempfile::tempdir().unwrap();
    let auth = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(
                r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource""#,
                auth.uri()
            ),
        ))
        .mount(&auth)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "authorization_servers": [format!("{}/oauth", auth.uri())]
        })))
        .mount(&auth)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server/oauth"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "authorization_endpoint": format!("{}/authorize", auth.uri()),
            "token_endpoint": format!("{}/token", auth.uri()),
            "registration_endpoint": format!("{}/register", auth.uri())
        })))
        .mount(&auth)
        .await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "client_id": "registered-client"
        })))
        .mount(&auth)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("client_id=registered-client"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "token-2",
            "token_type": "Bearer"
        })))
        .mount(&auth)
        .await;

    fs::write(
        home.path().join("config.json"),
        format!(
            r#"{{
                "providers": {{}},
                "agents": {{"defaults": {{"model": "m"}}}},
                "tools": {{"mcpServers": {{"remote": {{
                    "type": "streamableHttp",
                    "url": "{}/mcp"
                }}}}}}
            }}"#,
            auth.uri()
        ),
    )
    .unwrap();

    Command::cargo_bin("zunel")
        .unwrap()
        .env("ZUNEL_HOME", home.path())
        .args([
            "mcp",
            "login",
            "remote",
            "--state",
            "state-2",
            "--url",
            "http://127.0.0.1:33419/callback?code=callback-code&state=state-2",
        ])
        .assert()
        .success();

    let token_path = home
        .path()
        .join("mcp-oauth")
        .join("remote")
        .join("token.json");
    let token: serde_json::Value = serde_json::from_slice(&fs::read(token_path).unwrap()).unwrap();
    assert_eq!(token["accessToken"], "token-2");
    assert_eq!(token["clientId"], "registered-client");
}

#[tokio::test]
async fn cli_mcp_serve_self_exposes_sessions_list_tool() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let sessions = workspace.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("cli_direct.jsonl"),
        r#"{"_type": "metadata", "key": "cli:direct", "created_at": "2026-04-24T10:00:00.000000", "updated_at": "2026-04-24T11:00:00.000000", "metadata": {}, "last_consolidated": 0}
{"role": "user", "content": "hello", "timestamp": "2026-04-24T11:00:00.000000"}
"#,
    )
    .unwrap();
    fs::write(
        home.path().join("config.json"),
        format!(
            r#"{{
                "providers": {{}},
                "agents": {{"defaults": {{"model": "m", "workspace": "{}"}}}}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "self".to_string(),
    ];
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &args, env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert!(tools.iter().any(|tool| tool.name == "zunel_sessions_list"));

    let result = client
        .call_tool("zunel_sessions_list", serde_json::json!({"limit": 5}), 5)
        .await
        .unwrap();
    assert!(result.contains("\"key\":\"cli:direct\""), "{result}");
}

#[tokio::test]
async fn cli_mcp_serve_self_gets_session_metadata_and_messages() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let sessions = workspace.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("cli_direct.jsonl"),
        r#"{"_type": "metadata", "key": "cli:direct", "created_at": "2026-04-24T10:00:00.000000", "updated_at": "2026-04-24T11:00:00.000000", "metadata": {"source": "cli"}, "last_consolidated": 0}
{"role": "user", "content": "hello", "timestamp": "2026-04-24T11:00:00.000000"}
{"role": "assistant", "content": "hi", "timestamp": "2026-04-24T11:00:01.000000"}
"#,
    )
    .unwrap();
    fs::write(
        home.path().join("config.json"),
        format!(
            r#"{{
                "providers": {{}},
                "agents": {{"defaults": {{"model": "m", "workspace": "{}"}}}}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "self".to_string(),
    ];
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &args, env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert!(tools.iter().any(|tool| tool.name == "zunel_session_get"));
    assert!(tools
        .iter()
        .any(|tool| tool.name == "zunel_session_messages"));

    let metadata = client
        .call_tool(
            "zunel_session_get",
            serde_json::json!({"session_key": "cli:direct"}),
            5,
        )
        .await
        .unwrap();
    assert!(metadata.contains("\"message_count\":2"), "{metadata}");
    assert!(metadata.contains("\"source\":\"cli\""), "{metadata}");

    let messages = client
        .call_tool(
            "zunel_session_messages",
            serde_json::json!({"session_key": "cli:direct", "limit": 1}),
            5,
        )
        .await
        .unwrap();
    assert!(messages.contains("\"count\":1"), "{messages}");
    assert!(messages.contains("\"content\":\"hi\""), "{messages}");
    assert!(!messages.contains("\"content\":\"hello\""), "{messages}");
}

#[tokio::test]
async fn cli_mcp_serve_self_lists_channels_mcp_servers_and_cron_jobs() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    fs::create_dir_all(workspace.join("cron")).unwrap();
    fs::write(
        home.path().join("config.json"),
        format!(
            r#"{{
                "providers": {{}},
                "agents": {{"defaults": {{"model": "m", "workspace": "{}"}}}},
                "channels": {{"slack": {{"enabled": true, "mode": "socket", "botToken": "xoxb-secret"}}}},
                "tools": {{"mcpServers": {{"local": {{"type": "stdio", "command": "zunel-mcp-self", "env": {{"SECRET": "hidden"}}}}}}}}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("cron").join("jobs.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "jobs": [{
                "id": "job_1",
                "name": "daily",
                "enabled": true,
                "schedule": {"kind": "every", "everyMs": 60000},
                "payload": {"kind": "agent_turn", "message": "tick"},
                "state": {"nextRunAtMs": 1000, "runHistory": []}
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "self".to_string(),
    ];
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &args, env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert!(tools.iter().any(|tool| tool.name == "zunel_channels_list"));
    assert!(tools
        .iter()
        .any(|tool| tool.name == "zunel_mcp_servers_list"));
    assert!(tools.iter().any(|tool| tool.name == "zunel_cron_jobs_list"));
    assert!(tools.iter().any(|tool| tool.name == "zunel_cron_job_get"));

    let channels = client
        .call_tool("zunel_channels_list", serde_json::json!({}), 5)
        .await
        .unwrap();
    assert!(channels.contains("\"name\":\"slack\""), "{channels}");
    assert!(!channels.contains("xoxb-secret"), "{channels}");

    let servers = client
        .call_tool("zunel_mcp_servers_list", serde_json::json!({}), 5)
        .await
        .unwrap();
    assert!(servers.contains("\"name\":\"local\""), "{servers}");
    assert!(!servers.contains("hidden"), "{servers}");

    let job = client
        .call_tool(
            "zunel_cron_job_get",
            serde_json::json!({"job_id": "job_1"}),
            5,
        )
        .await
        .unwrap();
    assert!(job.contains("\"found\":true"), "{job}");
    assert!(job.contains("\"message\":\"tick\""), "{job}");
}

#[tokio::test]
async fn cli_mcp_serve_self_sends_slack_message_to_channel() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channel": "C1",
            "ts": "1713974400.000100"
        })))
        .mount(&slack)
        .await;

    fs::write(
        home.path().join("config.json"),
        format!(
            r#"{{
                "providers": {{}},
                "agents": {{"defaults": {{"model": "m", "workspace": "{}"}}}},
                "channels": {{"slack": {{"enabled": true, "botToken": "xoxb-secret"}}}}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "self".to_string(),
    ];
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), slack.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &args, env, &[], 5)
        .await
        .unwrap();

    let sent = client
        .call_tool(
            "zunel_send_message_to_channel",
            serde_json::json!({"channel": "slack", "channel_id": "C1", "text": "hello"}),
            5,
        )
        .await
        .unwrap();
    assert!(sent.contains("\"ok\":true"), "{sent}");
    assert!(sent.contains("\"channel\":\"C1\""), "{sent}");
    assert!(!sent.contains("xoxb-secret"), "{sent}");
}

/// `zunel_slack_capability` is the introspection tool that lets the agent
/// answer "can you post to Slack?" from runtime truth. It must report:
/// - the live tool names (filtered by `userTokenReadOnly`)
/// - whether a user OAuth token is present
/// - the safety posture (read_only flag, writeAllow count + sample)
/// And it must NEVER leak the actual access token bytes.
#[tokio::test]
async fn cli_mcp_serve_self_reports_slack_capability_with_safety_posture() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let token_dir = home.path().join("slack-app-mcp");
    fs::create_dir_all(&token_dir).unwrap();
    fs::write(
        token_dir.join("user_token.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "access_token": "xoxp-capability-token-secret",
            "user_id": "UCAP",
            "team_id": "TCAP"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        home.path().join("config.json"),
        format!(
            r#"{{
                "providers": {{}},
                "agents": {{"defaults": {{"model": "m", "workspace": "{}"}}}},
                "channels": {{"slack": {{
                    "enabled": true,
                    "userTokenReadOnly": true,
                    "writeAllow": ["UCAP", "C42"]
                }}}}
            }}"#,
            workspace.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "self".to_string(),
    ];
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &args, env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert!(tools
        .iter()
        .any(|tool| tool.name == "zunel_slack_capability"));

    let payload = client
        .call_tool("zunel_slack_capability", serde_json::json!({}), 5)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();

    assert_eq!(parsed["user_token_present"], true);
    assert_eq!(
        parsed["safety"]["user_token_read_only"], true,
        "should reflect the userTokenReadOnly config flag"
    );
    assert_eq!(
        parsed["safety"]["write_allow_count"], 2,
        "should count the writeAllow entries without exposing them all"
    );
    let sample = parsed["safety"]["write_allow_sample"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    assert!(sample.contains(&"UCAP"), "{sample:?}");
    assert!(sample.contains(&"C42"), "{sample:?}");

    let names: Vec<&str> = parsed["tool_names"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(names.contains(&"slack_whoami"), "{names:?}");
    assert!(names.contains(&"slack_channel_history"), "{names:?}");
    assert!(
        !names.contains(&"slack_post_as_me"),
        "userTokenReadOnly must hide write tools in capability report: {names:?}"
    );
    assert!(
        !names.contains(&"slack_dm_self"),
        "userTokenReadOnly must hide write tools in capability report: {names:?}"
    );
    assert_eq!(parsed["write_tools_exposed"], false);

    assert!(
        !payload.contains("xoxp-capability-token-secret"),
        "capability tool must never echo the bearer token bytes: {payload}"
    );
}

/// `zunel mcp serve --server slack` historically only registered a single
/// `slack_whoami` stub, even though the standalone `zunel-mcp-slack` binary
/// already had a full read+write surface. That meant any agent host that
/// followed the documented invocation in `docs/cli-reference.md` was silently
/// stripped of every Slack tool except the auth check. This test guards
/// against that regression by asserting the CLI dispatcher exposes the same
/// catalog as the standalone library, including a callable `slack_post_as_me`
/// when `userTokenReadOnly` is left at its default (false).
#[tokio::test]
async fn cli_mcp_serve_slack_exposes_full_tool_catalog_and_can_post() {
    let home = tempfile::tempdir().unwrap();
    let token_path = home.path().join("slack-app-mcp").join("user_token.json");
    fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    fs::write(
        &token_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "access_token": "xoxp-cli-token",
            "user_id": "UCLI",
            "team_id": "TCLI"
        }))
        .unwrap(),
    )
    .unwrap();

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(header("authorization", "Bearer xoxp-cli-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channel": "C42",
            "ts": "1713974400.000100"
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.getPermalink"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "permalink": "https://slack.example/cli-msg"
        })))
        .mount(&slack)
        .await;

    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "slack".to_string(),
    ];
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), slack.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &args, env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    for expected in &[
        "slack_whoami",
        "slack_channel_history",
        "slack_channel_replies",
        "slack_search_messages",
        "slack_search_users",
        "slack_search_files",
        "slack_search_channels",
        "slack_list_users",
        "slack_user_info",
        "slack_permalink",
        "slack_post_as_me",
        "slack_dm_self",
        "slack_schedule_message",
        "slack_create_canvas",
        "slack_read_canvas",
        "slack_update_canvas",
    ] {
        assert!(
            names.contains(expected),
            "`zunel mcp serve --server slack` is missing {expected}: {names:?}"
        );
    }

    let posted = client
        .call_tool(
            "slack_post_as_me",
            serde_json::json!({"channel": "C42", "text": "hello from cli dispatcher"}),
            5,
        )
        .await
        .unwrap();
    assert!(posted.contains("\"ok\":true"), "{posted}");
    assert!(posted.contains("\"channel\":\"C42\""), "{posted}");
    assert!(
        posted.contains("\"permalink\":\"https://slack.example/cli-msg\""),
        "{posted}"
    );
    assert!(!posted.contains("xoxp-cli-token"), "{posted}");
}

/// `channels.slack.writeAllow` adds an allowlist on top of the binary
/// `userTokenReadOnly` switch: write tools stay registered so the agent
/// can post into the listed channels, but are refused for anything else.
/// This is what lets a host say "the agent may DM me, but no one else".
#[tokio::test]
async fn cli_mcp_serve_slack_honors_write_allow_scope_restriction() {
    let home = tempfile::tempdir().unwrap();
    let token_path = home.path().join("slack-app-mcp").join("user_token.json");
    fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    fs::write(
        &token_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "access_token": "xoxp-cli-token",
            "user_id": "UCLI",
            "team_id": "TCLI"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        home.path().join("config.json"),
        r#"{
            "providers": {},
            "agents": {"defaults": {"model": "m"}},
            "channels": {"slack": {"enabled": true, "writeAllow": ["UCLI", "C42"]}}
        }"#,
    )
    .unwrap();

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(body_string_contains("channel=C42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channel": "C42",
            "ts": "1713974400.000400"
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.getPermalink"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "permalink": "https://slack.example/allowed"
        })))
        .mount(&slack)
        .await;

    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "slack".to_string(),
    ];
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), slack.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &args, env, &[], 5)
        .await
        .unwrap();

    // Listed channel is allowed.
    let allowed = client
        .call_tool(
            "slack_post_as_me",
            serde_json::json!({"channel": "C42", "text": "hi"}),
            5,
        )
        .await
        .unwrap();
    assert!(allowed.contains("\"ok\":true"), "{allowed}");

    // Unlisted channel is refused without ever touching the network.
    let refused = client
        .call_tool(
            "slack_post_as_me",
            serde_json::json!({"channel": "CSomeoneElse", "text": "should refuse"}),
            5,
        )
        .await
        .unwrap();
    assert!(
        refused.contains("\"error\":\"channel_not_in_write_allow\""),
        "{refused}"
    );
    assert!(
        refused.contains("\"channel\":\"CSomeoneElse\""),
        "{refused}"
    );

    // `slack_dm_self` resolves to the authenticated user_id (UCLI), which
    // IS on the allowlist, so it passes. (Reusing the same chat.postMessage
    // mock because both end up at the same endpoint — wiremock matches on
    // body_string_contains regardless of which path called.)
    let dm = client
        .call_tool("slack_dm_self", serde_json::json!({"text": "self"}), 5)
        .await
        .unwrap();
    // The mock only accepts channel=C42 in the body; UCLI won't match,
    // so we expect a 404 from wiremock surfaced as a non-ok payload.
    // What we DO assert is that the refusal text is absent — i.e. the
    // allowlist let the call through to the network layer.
    assert!(
        !dm.contains("channel_not_in_write_allow"),
        "slack_dm_self should not be refused when the user_id is on the writeAllow list: {dm}"
    );
}

/// When `channels.slack.userTokenReadOnly = true`, the CLI Slack server
/// must (a) hide the write tools from `tools/list` so the agent never even
/// sees them and (b) refuse a direct write call as defense-in-depth. The
/// safety knob has to be enforced at the MCP surface, not just at the
/// gateway, because the agent reaches Slack via this stdio MCP server.
#[tokio::test]
async fn cli_mcp_serve_slack_honors_user_token_read_only_flag() {
    let home = tempfile::tempdir().unwrap();
    let token_path = home.path().join("slack-app-mcp").join("user_token.json");
    fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    fs::write(
        &token_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "access_token": "xoxp-cli-token",
            "user_id": "UCLI",
            "team_id": "TCLI"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        home.path().join("config.json"),
        r#"{
            "providers": {},
            "agents": {"defaults": {"model": "m"}},
            "channels": {"slack": {"enabled": true, "userTokenReadOnly": true}}
        }"#,
    )
    .unwrap();

    let bin = cargo_bin("zunel");
    let args = vec![
        "mcp".to_string(),
        "serve".to_string(),
        "--server".to_string(),
        "slack".to_string(),
    ];
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &args, env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(
        names.contains(&"slack_whoami"),
        "read tools should still be exposed: {names:?}"
    );
    assert!(
        names.contains(&"slack_channel_history"),
        "read tools should still be exposed: {names:?}"
    );
    for hidden in &[
        "slack_post_as_me",
        "slack_dm_self",
        "slack_schedule_message",
        "slack_create_canvas",
        "slack_update_canvas",
    ] {
        assert!(
            !names.contains(hidden),
            "userTokenReadOnly must hide {hidden}: {names:?}"
        );
    }
    // `slack_read_canvas` is read-only and stays exposed; this guards
    // against accidentally folding it into WRITE_TOOLS in the future.
    assert!(
        names.contains(&"slack_read_canvas"),
        "slack_read_canvas should remain exposed in read-only mode: {names:?}"
    );

    let refused = client
        .call_tool(
            "slack_post_as_me",
            serde_json::json!({"channel": "C42", "text": "should be refused"}),
            5,
        )
        .await
        .unwrap();
    assert!(
        refused.contains("\"error\":\"user_token_read_only\""),
        "{refused}"
    );

    // Each of the new write tools must also defense-in-depth refuse a
    // direct call when read-only is on (a malicious host that built its
    // own tool catalog and dispatched anyway must not be able to bypass
    // the safety knob).
    for (tool, payload) in [
        (
            "slack_schedule_message",
            serde_json::json!({"channel": "C42", "text": "x", "post_at": 9_999_999_999_i64}),
        ),
        (
            "slack_create_canvas",
            serde_json::json!({"title": "x", "content": "y"}),
        ),
        (
            "slack_update_canvas",
            serde_json::json!({"canvas_id": "F1", "action": "append", "content": "y"}),
        ),
    ] {
        let refused = client.call_tool(tool, payload, 5).await.unwrap();
        assert!(
            refused.contains("\"error\":\"user_token_read_only\""),
            "{tool} must refuse under read-only: {refused}"
        );
    }
}
