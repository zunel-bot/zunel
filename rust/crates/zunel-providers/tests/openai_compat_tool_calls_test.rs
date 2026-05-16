use futures::StreamExt;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use zunel_config::{AgentDefaults, AgentsConfig, Config, CustomProvider, ProvidersConfig};
use zunel_providers::{
    build_provider, ChatMessage, GenerationSettings, StreamEvent, ToolCallAccumulator, ToolSchema,
};

fn sse(lines: &[&str]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str("data: ");
        out.push_str(line);
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn config_with_base(api_base: String) -> Config {
    Config {
        providers: ProvidersConfig {
            custom: Some(CustomProvider {
                api_key: "sk".into(),
                api_base,
                extra_headers: None,
            }),
            codex: None,
            bedrock: None,
        },
        agents: AgentsConfig {
            defaults: AgentDefaults {
                provider: Some("custom".into()),
                model: "m".into(),
                ..Default::default()
            },
        },
        channels: Default::default(),
        gateway: Default::default(),
        tools: Default::default(),
        cli: Default::default(),
        ..Default::default()
    }
}

#[tokio::test]
async fn openai_compat_emits_tool_call_delta_events() {
    let server = MockServer::start().await;
    // Two tool-call chunks + a terminal finish_reason chunk. First
    // chunk carries id/name and the opening of `arguments`; second
    // carries the tail. OpenAI's SSE usually splits arguments this way.
    let body = sse(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"./README.md\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let cfg = config_with_base(server.uri());
    let provider = build_provider(&cfg).await.unwrap();
    let messages = [ChatMessage::user("read it")];
    let tools: [ToolSchema; 0] = [];
    let settings = GenerationSettings::default();

    let stream = provider.generate_stream("m", &messages, &tools, &settings);
    futures::pin_mut!(stream);
    let mut acc = ToolCallAccumulator::default();
    let mut saw_delta = false;
    let mut finish_reason: Option<String> = None;
    while let Some(event) = stream.next().await {
        let event = event.unwrap();
        if matches!(event, StreamEvent::ToolCallDelta { .. }) {
            saw_delta = true;
        }
        if let StreamEvent::Done(resp) = &event {
            finish_reason = resp.finish_reason.clone();
        }
        acc.push(event);
    }
    assert!(saw_delta, "expected at least one ToolCallDelta event");
    let calls = acc.finalize().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].arguments, json!({"path": "./README.md"}));
    assert_eq!(finish_reason.as_deref(), Some("tool_calls"));
}

#[tokio::test]
async fn non_stream_generate_parses_tool_calls() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "chatcmpl-2",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call_99",
                        "type": "function",
                        "function": {
                            "name": "list_dir",
                            "arguments": "{\"path\":\".\"}"
                        }
                    }
                ]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let cfg = config_with_base(server.uri());
    let provider = build_provider(&cfg).await.unwrap();
    let response = provider
        .generate(
            "m",
            &[ChatMessage::user("list")],
            &[],
            &GenerationSettings::default(),
        )
        .await
        .unwrap();

    assert!(response.content.is_none());
    assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "call_99");
    assert_eq!(response.tool_calls[0].name, "list_dir");
    assert_eq!(response.tool_calls[0].arguments, json!({"path": "."}));
    assert_eq!(response.tool_calls[0].index, 0);
}

#[tokio::test]
async fn wire_format_serializes_tool_messages_and_tools_catalog() {
    use std::sync::{Arc, Mutex};
    use wiremock::{Request, Respond};

    struct CaptureRequest {
        captured: Arc<Mutex<Option<serde_json::Value>>>,
    }

    impl Respond for CaptureRequest {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *self.captured.lock().unwrap() = Some(body);
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "x",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }))
        }
    }

    let captured = Arc::new(Mutex::new(None));
    let responder = CaptureRequest {
        captured: captured.clone(),
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(responder)
        .mount(&server)
        .await;

    let cfg = config_with_base(server.uri());
    let provider = build_provider(&cfg).await.unwrap();

    let assistant_with_tc = ChatMessage::assistant_with_tool_calls(
        "",
        vec![zunel_providers::ToolCallRequest {
            id: "call_7".into(),
            name: "read_file".into(),
            arguments: json!({"path": "README.md"}),
            index: 0,
        }],
    );
    let tool_result = ChatMessage::tool("call_7", "file contents here");
    let messages = [
        ChatMessage::system("sys"),
        ChatMessage::user("read it"),
        assistant_with_tc,
        tool_result,
    ];
    let tools = [ToolSchema {
        name: "read_file".into(),
        description: "reads a file".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
    }];
    let settings = GenerationSettings::default();

    provider
        .generate("m", &messages, &tools, &settings)
        .await
        .unwrap();

    let body = captured.lock().unwrap().take().expect("request captured");

    assert_eq!(body["model"], json!("m"));
    // Messages round-trip tool turns correctly.
    let msgs = body["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[2]["role"], "assistant");
    assert!(
        msgs[2]["content"].is_null(),
        "content must be null when assistant carries tool_calls"
    );
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_7");
    assert_eq!(msgs[2]["tool_calls"][0]["type"], "function");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "read_file");
    // arguments is a JSON-encoded string per OpenAI spec.
    let args_str = msgs[2]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a string");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(args_str).unwrap(),
        json!({"path": "README.md"})
    );
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_7");
    assert_eq!(msgs[3]["content"], "file contents here");

    // Tools catalog is forwarded with tool_choice=auto.
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(body["tool_choice"], "auto");
}
