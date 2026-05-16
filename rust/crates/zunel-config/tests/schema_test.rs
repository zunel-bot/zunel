use std::path::PathBuf;

#[test]
fn parses_minimal_fixture() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal.json");
    let raw = std::fs::read_to_string(&path).unwrap();
    let cfg: zunel_config::Config = serde_json::from_str(&raw).unwrap();

    let custom = cfg
        .providers
        .custom
        .as_ref()
        .expect("custom provider present");
    assert_eq!(custom.api_key, "sk-test");
    assert_eq!(custom.api_base, "https://api.openai.com/v1");
    assert_eq!(
        custom
            .extra_headers
            .as_ref()
            .unwrap()
            .get("X-Demo")
            .unwrap(),
        "1"
    );

    let d = &cfg.agents.defaults;
    assert_eq!(d.provider.as_deref(), Some("custom"));
    assert_eq!(d.model, "gpt-4o-mini");
    assert_eq!(d.temperature, Some(0.2));
    assert_eq!(d.max_tokens, Some(1024));
}

#[test]
fn tools_section_defaults_when_absent() {
    let json = r#"{
        "providers": {},
        "agents": { "defaults": { "model": "m" } }
    }"#;
    let cfg: zunel_config::Config = serde_json::from_str(json).unwrap();
    assert!(!cfg.tools.exec.enable, "exec must be opt-in");
    assert!(!cfg.tools.web.enable, "web must be opt-in");
    assert!(
        cfg.tools.web.search_provider.is_empty(),
        "search_provider defaults to unset"
    );
    assert!(
        !cfg.tools.approval_required,
        "approval_required defaults off"
    );
    assert!(cfg.tools.filesystem.media_dir.is_none());
}

#[test]
fn unused_custom_provider_accepts_null_fields() {
    let json = r#"{
        "providers": {
            "custom": {
                "apiKey": null,
                "apiBase": null,
                "extraHeaders": null
            },
            "codex": {
                "apiBase": null
            }
        },
        "agents": {
            "defaults": {
                "provider": "codex",
                "model": "gpt-5.4"
            }
        }
    }"#;
    let cfg: zunel_config::Config = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.agents.defaults.provider.as_deref(), Some("codex"));
    assert_eq!(cfg.providers.custom.as_ref().unwrap().api_key, "");
    assert_eq!(cfg.providers.custom.as_ref().unwrap().api_base, "");
}

#[test]
fn tools_section_round_trips() {
    let json = r#"{
        "providers": {},
        "agents": { "defaults": { "model": "m" } },
        "tools": {
            "approval_required": true,
            "approval_scope": "shell",
            "exec": {
                "enable": true,
                "default_timeout_secs": 30,
                "max_timeout_secs": 600,
                "env": {
                    "PATH": "$HOME/.cargo/bin:${PATH}",
                    "LANG": "en_US.UTF-8"
                }
            },
            "web": {
                "enable": true,
                "search_provider": "brave",
                "brave_api_key": "k"
            },
            "filesystem": {
                "media_dir": "/tmp/media"
            }
        }
    }"#;
    let cfg: zunel_config::Config = serde_json::from_str(json).unwrap();
    assert!(cfg.tools.exec.enable);
    assert_eq!(cfg.tools.exec.default_timeout_secs, 30);
    assert_eq!(cfg.tools.exec.max_timeout_secs, 600);
    let exec_env = cfg
        .tools
        .exec
        .env
        .as_ref()
        .expect("tools.exec.env should round-trip");
    assert_eq!(
        exec_env.get("PATH").map(String::as_str),
        Some("$HOME/.cargo/bin:${PATH}")
    );
    assert_eq!(
        exec_env.get("LANG").map(String::as_str),
        Some("en_US.UTF-8")
    );
    assert!(cfg.tools.web.enable);
    assert_eq!(cfg.tools.web.search_provider, "brave");
    assert_eq!(cfg.tools.web.brave_api_key.as_deref(), Some("k"));
    assert_eq!(
        cfg.tools.filesystem.media_dir.as_deref(),
        Some(std::path::Path::new("/tmp/media"))
    );
    assert!(cfg.tools.approval_required);
    assert_eq!(cfg.tools.approval_scope, "shell");
}

#[test]
fn mcp_servers_section_round_trips_python_shape() {
    let json = r#"{
        "providers": {},
        "agents": { "defaults": { "model": "m" } },
        "tools": {
            "mcpServers": {
                "self": {
                    "type": "stdio",
                    "command": "python",
                    "args": ["-m", "zunel.mcp.zunel_self"],
                    "env": { "A": "B" },
                    "headers": { "Authorization": "Bearer ${TOKEN}" },
                    "toolTimeout": 30,
                    "initTimeout": 10,
                    "enabledTools": ["sessions", "mcp_self_status"],
                    "oauth": {
                        "enabled": true,
                        "clientId": "client-1",
                        "clientSecret": "secret-1",
                        "authorizationUrl": "https://auth.example/authorize",
                        "tokenUrl": "https://auth.example/token",
                        "scope": "read write"
                    }
                }
            }
        }
    }"#;
    let cfg: zunel_config::Config = serde_json::from_str(json).unwrap();
    let server = cfg.tools.mcp_servers.get("self").unwrap();
    assert_eq!(server.transport_type.as_deref(), Some("stdio"));
    assert_eq!(server.command.as_deref(), Some("python"));
    assert_eq!(
        server.args.as_deref(),
        Some(&["-m".to_string(), "zunel.mcp.zunel_self".to_string()][..])
    );
    assert_eq!(server.env.as_ref().unwrap().get("A").unwrap(), "B");
    assert_eq!(
        server
            .headers
            .as_ref()
            .unwrap()
            .get("Authorization")
            .unwrap(),
        "Bearer ${TOKEN}"
    );
    assert_eq!(server.tool_timeout, Some(30));
    assert_eq!(server.init_timeout, Some(10));
    assert_eq!(
        server.enabled_tools.as_deref(),
        Some(&["sessions".to_string(), "mcp_self_status".to_string()][..])
    );
    let oauth = server.oauth.as_ref().unwrap();
    assert!(oauth.enabled);
    assert_eq!(oauth.client_id.as_deref(), Some("client-1"));
    assert_eq!(oauth.client_secret.as_deref(), Some("secret-1"));
    assert_eq!(
        oauth.authorization_url.as_deref(),
        Some("https://auth.example/authorize")
    );
    assert_eq!(
        oauth.token_url.as_deref(),
        Some("https://auth.example/token")
    );
    assert_eq!(oauth.scope.as_deref(), Some("read write"));

    let value = serde_json::to_value(&cfg).unwrap();
    assert!(value["tools"]["mcpServers"]["self"].is_object());
    assert_eq!(value["tools"]["mcpServers"]["self"]["toolTimeout"], 30);
    assert_eq!(value["tools"]["mcpServers"]["self"]["initTimeout"], 10);
}

#[test]
fn channels_slack_section_round_trips_python_shape() {
    let json = r#"{
        "providers": {},
        "agents": { "defaults": { "model": "m" } },
        "channels": {
            "sendProgress": true,
            "sendToolHints": true,
            "sendMaxRetries": 4,
            "slack": {
                "enabled": true,
                "mode": "socket",
                "botToken": "xoxb-token",
                "appToken": "xapp-token",
                "allowFrom": ["U1"],
                "groupPolicy": "mention",
                "groupAllowFrom": ["C1"],
                "replyInThread": true,
                "reactEmoji": "eyes",
                "doneEmoji": "white_check_mark",
                "dm": {
                    "enabled": true,
                    "policy": "open",
                    "allowFrom": ["U2"]
                }
            }
        }
    }"#;
    let cfg: zunel_config::Config = serde_json::from_str(json).unwrap();
    assert!(cfg.channels.send_progress);
    assert!(cfg.channels.send_tool_hints);
    assert_eq!(cfg.channels.send_max_retries, 4);
    let slack = cfg.channels.slack.as_ref().unwrap();
    assert!(slack.enabled);
    assert_eq!(slack.mode, "socket");
    assert_eq!(slack.bot_token.as_deref(), Some("xoxb-token"));
    assert_eq!(slack.app_token.as_deref(), Some("xapp-token"));
    assert_eq!(slack.allow_from, vec!["U1"]);
    assert_eq!(slack.group_policy, "mention");
    assert_eq!(slack.group_allow_from, vec!["C1"]);
    assert!(slack.reply_in_thread);
    assert_eq!(slack.react_emoji.as_deref(), Some("eyes"));
    assert_eq!(slack.done_emoji.as_deref(), Some("white_check_mark"));
    assert!(slack.dm.enabled);
    assert_eq!(slack.dm.policy, "open");
    assert_eq!(slack.dm.allow_from, vec!["U2"]);

    let value = serde_json::to_value(&cfg).unwrap();
    assert_eq!(value["channels"]["sendMaxRetries"], 4);
    assert_eq!(value["channels"]["slack"]["botToken"], "xoxb-token");
}

#[test]
fn gateway_agent_and_mcp_oauth_python_shape_round_trips() {
    let json = r#"{
        "providers": {},
        "agents": {
            "defaults": {
                "model": "m",
                "contextWindowTokens": 65536,
                "contextBlockLimit": 12,
                "maxToolIterations": 200,
                "maxToolResultChars": 16000,
                "providerRetryMode": "persistent",
                "timezone": "America/Los_Angeles",
                "unifiedSession": true,
                "disabledSkills": ["alpha"],
                "sessionTtlMinutes": 30,
                "dream": {
                    "intervalH": 4,
                    "modelOverride": "dream-model",
                    "maxBatchSize": 25,
                    "maxIterations": 12,
                    "annotateLineAges": false
                }
            }
        },
        "gateway": {
            "heartbeat": {
                "enabled": true,
                "intervalS": 120,
                "keepRecentMessages": 9
            }
        },
        "tools": {
            "mcpServers": {
                "remote": {
                    "type": "streamableHttp",
                    "url": "https://mcp.example",
                    "oauth": true,
                    "oauthScope": "read write",
                    "oauthCallbackHost": "127.0.0.1",
                    "oauthCallbackPort": 33418,
                    "oauthClientId": "client-1",
                    "oauthClientSecret": "secret-1",
                    "oauthRedirectUri": "http://127.0.0.1/callback"
                }
            }
        }
    }"#;

    let cfg: zunel_config::Config = serde_json::from_str(json).unwrap();
    let defaults = &cfg.agents.defaults;
    assert_eq!(defaults.context_window_tokens, Some(65_536));
    assert_eq!(defaults.context_block_limit, Some(12));
    assert_eq!(defaults.max_tool_iterations, Some(200));
    assert_eq!(defaults.max_tool_result_chars, Some(16_000));
    assert_eq!(defaults.provider_retry_mode.as_deref(), Some("persistent"));
    assert_eq!(defaults.timezone.as_deref(), Some("America/Los_Angeles"));
    assert_eq!(defaults.unified_session, Some(true));
    assert_eq!(defaults.disabled_skills, vec!["alpha"]);
    assert_eq!(defaults.session_ttl_minutes, Some(30));
    assert_eq!(defaults.dream.interval_h, Some(4));
    assert_eq!(
        defaults.dream.model_override.as_deref(),
        Some("dream-model")
    );
    assert_eq!(defaults.dream.max_batch_size, Some(25));
    assert_eq!(defaults.dream.max_iterations, Some(12));
    assert_eq!(defaults.dream.annotate_line_ages, Some(false));
    assert!(cfg.gateway.heartbeat.enabled);
    assert_eq!(cfg.gateway.heartbeat.interval_s, 120);
    assert_eq!(cfg.gateway.heartbeat.keep_recent_messages, 9);

    let remote = cfg.tools.mcp_servers.get("remote").unwrap();
    let oauth = remote.normalized_oauth().unwrap();
    assert!(oauth.enabled);
    assert_eq!(oauth.scope.as_deref(), Some("read write"));
    assert_eq!(oauth.client_id.as_deref(), Some("client-1"));
    assert_eq!(oauth.client_secret.as_deref(), Some("secret-1"));
    assert_eq!(oauth.callback_host.as_deref(), Some("127.0.0.1"));
    assert_eq!(oauth.callback_port, Some(33418));
    assert_eq!(
        oauth.redirect_uri.as_deref(),
        Some("http://127.0.0.1/callback")
    );
}

#[test]
fn unknown_fields_ignored() {
    let json = r#"{
        "providers": {},
        "agents": { "defaults": { "model": "m" } },
        "extraTopLevelThing": { "nested": true }
    }"#;
    let cfg: zunel_config::Config = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.agents.defaults.model, "m");
}
