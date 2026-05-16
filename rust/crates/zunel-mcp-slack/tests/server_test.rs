use assert_cmd::cargo::cargo_bin;
use std::collections::BTreeMap;
use std::fs;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zunel_mcp::StdioMcpClient;

#[tokio::test]
async fn native_slack_mcp_server_lists_and_calls_whoami_tool() {
    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert!(tools.iter().any(|tool| tool.name == "slack_whoami"));

    let result = client
        .call_tool("slack_whoami", serde_json::json!({}), 5)
        .await
        .unwrap();
    assert!(result.contains("slack token configured"), "{result}");
    assert!(!result.contains("xoxp-test-token"), "{result}");
}

#[tokio::test]
async fn native_slack_mcp_server_reads_user_token_from_zunel_home() {
    let home = tempfile::tempdir().unwrap();
    let token_path = home.path().join("slack-app-mcp").join("user_token.json");
    fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    fs::write(
        &token_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "access_token": "xoxp-file-token",
            "user_id": "UFILE",
            "team_id": "TFILE",
            "team_name": "File Team",
            "scope": "channels:history"
        }))
        .unwrap(),
    )
    .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "messages": [{"ts": "1.0", "user": "UFILE", "text": "from file token"}],
            "has_more": false
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let result = client
        .call_tool(
            "slack_channel_history",
            serde_json::json!({"channel": "C1"}),
            5,
        )
        .await
        .unwrap();
    assert!(result.contains("\"text\":\"from file token\""), "{result}");
    assert!(!result.contains("xoxp-file-token"), "{result}");
}

#[tokio::test]
async fn native_slack_mcp_server_refreshes_expired_user_token_from_app_info() {
    let home = tempfile::tempdir().unwrap();
    let app_dir = home.path().join("slack-app-mcp");
    let token_path = app_dir.join("user_token.json");
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(
        app_dir.join("app_info.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "client_id": "111.222",
            "client_secret": "secret"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &token_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "access_token": "xoxp-old-token",
            "refresh_token": "refresh-old",
            "expires_at": 1,
            "user_id": "UOLD",
            "team_id": "TOLD",
            "scope": "users:read"
        }))
        .unwrap(),
    )
    .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "access_token": "xoxp-new-token",
            "refresh_token": "refresh-new",
            "expires_in": 3600,
            "scope": "users:read",
            "token_type": "user",
            "authed_user": {"id": "UNEW"},
            "team": {"id": "TNEW", "name": "Team New"},
            "enterprise": {"id": "ENEW"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .and(header("authorization", "Bearer xoxp-new-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "members": [{"id": "UNEW", "name": "new", "profile": {"real_name": "New User"}}],
            "response_metadata": {"next_cursor": ""}
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let users = client
        .call_tool("slack_list_users", serde_json::json!({"limit": 2}), 5)
        .await
        .unwrap();
    assert!(users.contains("\"id\":\"UNEW\""), "{users}");

    let token: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&token_path).unwrap()).unwrap();
    assert_eq!(token["access_token"], "xoxp-new-token");
    assert_eq!(token["refresh_token"], "refresh-new");
    assert_eq!(token["user_id"], "UNEW");
    assert_eq!(token["team_id"], "TNEW");
    assert!(token["expires_at"].as_i64().unwrap() > 1);
}

#[tokio::test]
async fn native_slack_mcp_server_surfaces_refresh_failure_when_oauth_v2_access_rejects() {
    let home = tempfile::tempdir().unwrap();
    let app_dir = home.path().join("slack-app-mcp");
    let token_path = app_dir.join("user_token.json");
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(
        app_dir.join("app_info.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "client_id": "111.222",
            "client_secret": "secret"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &token_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "access_token": "xoxp-stale-token",
            "refresh_token": "refresh-revoked",
            "expires_at": 1,
            "user_id": "USTALE",
            "team_id": "TSTALE",
            "scope": "chat:write"
        }))
        .unwrap(),
    )
    .unwrap();

    let server = MockServer::start().await;
    // Slack rejects the cached refresh_token (e.g. it aged out beyond the
    // rotation window or was revoked) — the recovery path should surface
    // the underlying `invalid_refresh_token` and the remediation hint
    // instead of the bare `token_expired` from chat.postMessage.
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": false,
            "error": "invalid_refresh_token"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": false,
            "error": "token_expired"
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let result = client
        .call_tool(
            "slack_post_as_me",
            serde_json::json!({"channel": "C1", "text": "hi"}),
            5,
        )
        .await
        .unwrap();
    assert!(
        result.contains("invalid_refresh_token"),
        "expected refresh error to be surfaced, got: {result}"
    );
    assert!(
        result.contains("zunel slack login --force"),
        "expected remediation hint, got: {result}"
    );
    assert!(
        !result.contains("\"error\":\"token_expired\""),
        "bare `token_expired` should have been replaced, got: {result}"
    );
    // The cached file should not have been overwritten with garbage.
    let token: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&token_path).unwrap()).unwrap();
    assert_eq!(token["access_token"], "xoxp-stale-token");
    assert_eq!(token["refresh_token"], "refresh-revoked");
}

#[tokio::test]
async fn native_slack_mcp_server_posts_as_user_and_dm_self() {
    let home = tempfile::tempdir().unwrap();
    let token_path = home.path().join("slack-app-mcp").join("user_token.json");
    fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    fs::write(
        &token_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "access_token": "xoxp-file-token",
            "user_id": "UFILE",
            "team_id": "TFILE",
            "team_name": "File Team"
        }))
        .unwrap(),
    )
    .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channel": "C1",
            "ts": "1713974400.000100"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.getPermalink"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "permalink": "https://slack.example/msg"
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert!(tools.iter().any(|tool| tool.name == "slack_post_as_me"));
    assert!(tools.iter().any(|tool| tool.name == "slack_dm_self"));

    let empty = client
        .call_tool(
            "slack_post_as_me",
            serde_json::json!({"channel": "C1", "text": "   "}),
            5,
        )
        .await
        .unwrap();
    assert!(empty.contains("\"error\":\"empty_text\""), "{empty}");

    let posted = client
        .call_tool(
            "slack_post_as_me",
            serde_json::json!({"channel": "C1", "text": "hello"}),
            5,
        )
        .await
        .unwrap();
    assert!(posted.contains("\"ok\":true"), "{posted}");
    assert!(
        posted.contains("\"permalink\":\"https://slack.example/msg\""),
        "{posted}"
    );

    let dm = client
        .call_tool("slack_dm_self", serde_json::json!({"text": "note"}), 5)
        .await
        .unwrap();
    assert!(dm.contains("\"ok\":true"), "{dm}");
}

#[tokio::test]
async fn native_slack_mcp_server_searches_messages_users_and_files() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/assistant.search.context"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "results": {
                "messages": [{
                    "message_ts": "1713974400.000100",
                    "thread_ts": "1713974400.000100",
                    "channel_id": "C1",
                    "channel_name": "general",
                    "author_user_id": "U1",
                    "author_name": "Alice",
                    "content": "launch notes",
                    "permalink": "https://slack.example/msg"
                }],
                "users": [{
                    "user_id": "U1",
                    "full_name": "Alice User",
                    "email": "alice@example.com",
                    "title": "Engineer",
                    "timezone": "America/Los_Angeles",
                    "permalink": "https://slack.example/user"
                }],
                "files": [{
                    "file_id": "F1",
                    "title": "Plan",
                    "mimetype": "text/plain",
                    "channel_id": "C1",
                    "channel_name": "general",
                    "author_user_id": "U1",
                    "author_name": "Alice",
                    "message_ts": "1713974400.000100",
                    "permalink": "https://slack.example/file"
                }]
            }
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert!(tools
        .iter()
        .any(|tool| tool.name == "slack_search_messages"));
    assert!(tools.iter().any(|tool| tool.name == "slack_search_users"));
    assert!(tools.iter().any(|tool| tool.name == "slack_search_files"));

    let messages = client
        .call_tool(
            "slack_search_messages",
            serde_json::json!({"query": "launch", "limit": 3}),
            5,
        )
        .await
        .unwrap();
    assert!(messages.contains("\"matches\""), "{messages}");
    assert!(messages.contains("\"text\":\"launch notes\""), "{messages}");

    let users = client
        .call_tool(
            "slack_search_users",
            serde_json::json!({"query": "ray", "limit": 3}),
            5,
        )
        .await
        .unwrap();
    assert!(users.contains("\"users\""), "{users}");
    assert!(users.contains("\"email\":\"alice@example.com\""), "{users}");

    let files = client
        .call_tool(
            "slack_search_files",
            serde_json::json!({"query": "plan", "limit": 3}),
            5,
        )
        .await
        .unwrap();
    assert!(files.contains("\"files\""), "{files}");
    assert!(files.contains("\"id\":\"F1\""), "{files}");
}

#[tokio::test]
async fn native_slack_mcp_server_reads_channel_history_and_user_info() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "messages": [{
                "ts": "1713974400.000100",
                "user": "U1",
                "text": "hello from slack",
                "thread_ts": "1713974400.000100",
                "reply_count": 2
            }],
            "has_more": false,
            "response_metadata": {"next_cursor": ""}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "user": {
                "id": "U1",
                "name": "ray",
                "real_name": "Alice",
                "profile": {"display_name": "Alice", "email": "alice@example.com", "title": "Engineer"},
                "is_bot": false,
                "deleted": false,
                "tz": "America/Los_Angeles"
            }
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let tools = client.list_tools(5).await.unwrap();
    assert!(tools
        .iter()
        .any(|tool| tool.name == "slack_channel_history"));
    assert!(tools.iter().any(|tool| tool.name == "slack_user_info"));

    let history = client
        .call_tool(
            "slack_channel_history",
            serde_json::json!({"channel": "C1", "limit": 5}),
            5,
        )
        .await
        .unwrap();
    assert!(history.contains("\"ok\":true"), "{history}");
    assert!(history.contains("\"channel\":\"C1\""), "{history}");
    assert!(
        history.contains("\"text\":\"hello from slack\""),
        "{history}"
    );

    let user = client
        .call_tool("slack_user_info", serde_json::json!({"user": "U1"}), 5)
        .await
        .unwrap();
    assert!(user.contains("\"ok\":true"), "{user}");
    assert!(user.contains("\"email\":\"alice@example.com\""), "{user}");
    assert!(user.contains("\"title\":\"Engineer\""), "{user}");
}

#[tokio::test]
async fn native_slack_mcp_server_resolves_user_id_to_dm_for_channel_history() {
    // Slack's `conversations.history` only accepts a channel ID (C…/G…/D…),
    // not a user ID. The Slack MCP catalog used to bubble that limitation
    // up to the agent, which made answering "show me my DM with @thomas"
    // require a separate manual lookup of the D… channel ID. The fix is
    // an automatic `conversations.open?users=U…` resolve step before the
    // history call, so this test pins:
    //
    // 1. A `U…` `channel` arg triggers a `conversations.open` call carrying
    //    `users=U…` (so a regression that drops the resolution would no
    //    longer match this mock).
    // 2. The DM channel ID returned by `conversations.open` is the one
    //    actually passed to `conversations.history` (so a regression that
    //    forwarded the raw user ID would miss the wiremock that's keyed
    //    on the resolved D… ID).
    // 3. The rendered response carries the resolved D… channel, not the
    //    raw user ID.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.open"))
        .and(body_string_contains("users=UHT8B2B8X"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channel": {"id": "DRESOLVED"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .and(body_string_contains("channel=DRESOLVED"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "messages": [{
                "ts": "1713974400.000100",
                "user": "UHT8B2B8X",
                "text": "from DM",
            }],
            "has_more": false,
            "response_metadata": {"next_cursor": ""}
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let history = client
        .call_tool(
            "slack_channel_history",
            serde_json::json!({"channel": "UHT8B2B8X", "limit": 5}),
            5,
        )
        .await
        .unwrap();
    assert!(history.contains("\"ok\":true"), "{history}");
    assert!(
        history.contains("\"channel\":\"DRESOLVED\""),
        "expected resolved DM channel in render, got: {history}"
    );
    assert!(history.contains("\"text\":\"from DM\""), "{history}");
    assert!(
        !history.contains("\"channel\":\"UHT8B2B8X\""),
        "raw user id leaked into render: {history}"
    );
}

#[tokio::test]
async fn native_slack_mcp_server_surfaces_conversations_open_errors_for_user_id_channel() {
    // When Slack rejects the auto-`conversations.open` call (e.g. the user
    // token lacks `im:write`, the workspace blocks DMs, etc.), we want the
    // agent to see Slack's own `{ok:false, error:…}` payload — not a
    // confusing `channel_not_found` two layers down from a fallback that
    // forwarded the raw user ID. Pin that the open-error short-circuits
    // the history call entirely.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": false,
            "error": "missing_scope",
            "needed": "im:write",
            "provided": "channels:history,im:history"
        })))
        .mount(&server)
        .await;
    // No `conversations.history` mock is registered: if the resolver
    // failed open and the dispatcher fell through to history anyway,
    // wiremock would return a 404 and the test would notice.

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let history = client
        .call_tool(
            "slack_channel_history",
            serde_json::json!({"channel": "UHT8B2B8X"}),
            5,
        )
        .await
        .unwrap();
    assert!(
        history.contains("\"error\":\"missing_scope\""),
        "expected open error to surface verbatim, got: {history}"
    );
    assert!(
        history.contains("\"needed\":\"im:write\""),
        "expected scope hint to flow through, got: {history}"
    );
}

#[tokio::test]
async fn native_slack_mcp_server_search_channels_filters_paginated_conversations_list() {
    // `conversations.list` has no server-side query parameter, so
    // `slack_search_channels` walks pages and filters client-side. Pin
    // the contract:
    //
    // 1. The tool keeps paging until it has `limit` matches OR exhausts
    //    `next_cursor`.
    // 2. Matching is case-insensitive across name / topic / purpose.
    // 3. Channels that don't match the query are omitted from the
    //    response (so the agent isn't paying tokens for irrelevant
    //    rows).
    // 4. The compact summary carries id/name/is_private/is_archived
    //    plus the topic/purpose strings — enough for the agent to
    //    decide which channel to read next.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.list"))
        .and(body_string_contains("cursor=cursor1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channels": [
                {"id": "C2", "name": "alpha-launch", "is_private": false, "is_archived": false,
                 "topic": {"value": "Launch coordination"}, "purpose": {"value": ""}}
            ],
            "response_metadata": {"next_cursor": ""}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channels": [
                {"id": "C1", "name": "general", "is_private": false, "is_archived": false,
                 "topic": {"value": "Welcome!"}, "purpose": {"value": ""}}
            ],
            "response_metadata": {"next_cursor": "cursor1"}
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let response = client
        .call_tool(
            "slack_search_channels",
            serde_json::json!({"query": "LAUNCH", "limit": 10}),
            5,
        )
        .await
        .unwrap();
    assert!(response.contains("\"ok\":true"), "{response}");
    assert!(
        response.contains("\"id\":\"C2\""),
        "expected page-2 match: {response}"
    );
    assert!(
        !response.contains("\"id\":\"C1\""),
        "page-1 row didn't match `launch` and should have been filtered out: {response}"
    );
    assert!(
        response.contains("\"pages_scanned\":2"),
        "expected paging metadata: {response}"
    );
}

#[tokio::test]
async fn native_slack_mcp_server_schedule_message_resolves_user_id_and_calls_chat_schedule() {
    // The schedule path mirrors `slack_post_as_me`: a user-id channel
    // is auto-resolved to a DM via `conversations.open`, and the
    // resolved D… ID is what `chat.scheduleMessage` sees. Pinning this
    // catches a regression where the resolver was bypassed for the
    // schedule case.
    //
    // We pin `ZUNEL_HOME` to an empty tempdir so `load_safety()` falls
    // back to its permissive default; otherwise the test would inherit
    // the developer's real `~/.zunel/config.json` and a populated
    // `channels.slack.writeAllow` would refuse the schedule call.
    let home = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.open"))
        .and(body_string_contains("users=UTHOMAS"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channel": {"id": "DTHOMAS"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.scheduleMessage"))
        .and(body_string_contains("channel=DTHOMAS"))
        .and(body_string_contains("post_at=9999999999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "channel": "DTHOMAS",
            "scheduled_message_id": "Q1234ABCD",
            "post_at": 9_999_999_999_i64
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    env.insert(
        "ZUNEL_HOME".to_string(),
        home.path().to_string_lossy().to_string(),
    );
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let response = client
        .call_tool(
            "slack_schedule_message",
            serde_json::json!({
                "channel": "UTHOMAS",
                "text": "ping in the morning",
                "post_at": 9_999_999_999_i64,
            }),
            5,
        )
        .await
        .unwrap();
    assert!(response.contains("\"ok\":true"), "{response}");
    assert!(
        response.contains("\"scheduled_message_id\":\"Q1234ABCD\""),
        "{response}"
    );
    assert!(
        response.contains("\"channel\":\"DTHOMAS\""),
        "expected resolved channel id in response, got: {response}"
    );
}

#[tokio::test]
async fn native_slack_mcp_server_create_canvas_calls_canvases_create_and_returns_permalink() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/canvases.create"))
        .and(body_string_contains("\"markdown\":\"# Hello\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "canvas_id": "F0CANVAS"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/files.info"))
        .and(body_string_contains("file=F0CANVAS"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "file": {"permalink": "https://slack.example/files/F0CANVAS"}
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let response = client
        .call_tool(
            "slack_create_canvas",
            serde_json::json!({"title": "Plan", "content": "# Hello"}),
            5,
        )
        .await
        .unwrap();
    assert!(response.contains("\"ok\":true"), "{response}");
    assert!(
        response.contains("\"canvas_id\":\"F0CANVAS\""),
        "{response}"
    );
    assert!(
        response.contains("\"permalink\":\"https://slack.example/files/F0CANVAS\""),
        "expected files.info permalink to flow back, got: {response}"
    );
}

#[tokio::test]
async fn native_slack_mcp_server_read_canvas_returns_sections_and_files_info_metadata() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/canvases.sections.lookup"))
        .and(body_string_contains("\"canvas_id\":\"F0CANVAS\""))
        .and(body_string_contains("\"section_types\":[\"any_header\"]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "sections": [
                {"id": "temp:C:abc123", "type": "h1"},
                {"id": "temp:C:def456", "type": "h2"}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/files.info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "file": {
                "title": "Project Plan",
                "permalink": "https://slack.example/files/F0CANVAS",
                "canvas": {"content": "# Plan\nbody"}
            }
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let response = client
        .call_tool(
            "slack_read_canvas",
            serde_json::json!({"canvas_id": "F0CANVAS"}),
            5,
        )
        .await
        .unwrap();
    assert!(response.contains("\"ok\":true"), "{response}");
    assert!(
        response.contains("\"title\":\"Project Plan\""),
        "{response}"
    );
    assert!(response.contains("\"id\":\"temp:C:abc123\""), "{response}");
    assert!(
        response.contains("\"content\":\"# Plan\\nbody\""),
        "{response}"
    );
}

#[tokio::test]
async fn native_slack_mcp_server_update_canvas_maps_action_vocabulary_to_slack_operations() {
    // Cursor's plugin uses `append`/`prepend`/`replace`. Slack's
    // underlying `canvases.edit` expects `insert_at_end` /
    // `insert_at_start` / `replace`. Pin the mapping by asserting the
    // Slack call carries the translated operation name.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/canvases.edit"))
        .and(body_string_contains("\"operation\":\"insert_at_end\""))
        .and(body_string_contains("\"section_id\":\"temp:C:abc123\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/files.info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "file": {"permalink": "https://slack.example/files/F0CANVAS"}
        })))
        .mount(&server)
        .await;

    let bin = cargo_bin("zunel-mcp-slack");
    let mut env = BTreeMap::new();
    env.insert(
        "SLACK_USER_TOKEN".to_string(),
        "xoxp-test-token".to_string(),
    );
    env.insert("SLACK_API_BASE".to_string(), server.uri());
    let mut client = StdioMcpClient::connect(bin.to_string_lossy().as_ref(), &[], env, &[], 5)
        .await
        .unwrap();

    let response = client
        .call_tool(
            "slack_update_canvas",
            serde_json::json!({
                "canvas_id": "F0CANVAS",
                "action": "append",
                "content": "## More\nstuff",
                "section_id": "temp:C:abc123",
            }),
            5,
        )
        .await
        .unwrap();
    assert!(response.contains("\"ok\":true"), "{response}");
    assert!(response.contains("\"action\":\"append\""), "{response}");
    assert!(
        response.contains("\"permalink\":\"https://slack.example/files/F0CANVAS\""),
        "{response}"
    );

    // An unknown action must short-circuit before the Slack call (no
    // canvases.edit mock for this branch — wiremock returns 404 and the
    // test would catch a regression that fell through).
    let bad = client
        .call_tool(
            "slack_update_canvas",
            serde_json::json!({
                "canvas_id": "F0CANVAS",
                "action": "rewrite-please",
                "content": "y",
            }),
            5,
        )
        .await
        .unwrap();
    assert!(bad.contains("\"error\":\"invalid_action\""), "{bad}");
    assert!(bad.contains("\"action\":\"rewrite-please\""), "{bad}");
}
