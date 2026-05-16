use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use zunel_providers::codex::{CodexAuth, CodexAuthProvider, CodexProvider};
use zunel_providers::{ChatMessage, GenerationSettings, LLMProvider, StreamEvent, ToolSchema};

#[derive(Debug)]
struct FixedAuth;

#[async_trait]
impl CodexAuthProvider for FixedAuth {
    async fn load(&self) -> zunel_providers::Result<CodexAuth> {
        Ok(CodexAuth {
            access_token: "access_fixture".into(),
            account_id: "acct_fixture".into(),
        })
    }
}

fn sse(events: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for event in events {
        out.push_str("data: ");
        out.push_str(&event.to_string());
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn codex_provider_sends_expected_headers_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer access_fixture"))
        .and(header("chatgpt-account-id", "acct_fixture"))
        .and(header("openai-beta", "responses=experimental"))
        .and(header("originator", "codex_cli_rs"))
        .and(header("user-agent", "zunel (rust)"))
        .and(body_json(json!({
            "model": "gpt-5.4",
            "store": false,
            "stream": true,
            "instructions": "system rules",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hello"}]}
            ],
            "text": {"verbosity": "medium"},
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "8bdd20513cb5f6c18d3e236e5c61e60803d2e36d3595c78069951efae722d609",
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "reasoning": {"effort": "medium"},
            "tools": [
                {
                    "type": "function",
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"]
                    }
                }
            ]
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(&[json!({
                    "type": "response.completed",
                    "response": {"status": "completed"}
                })])),
        )
        .mount(&server)
        .await;

    let provider = CodexProvider::with_auth(
        format!("{}/backend-api/codex/responses", server.uri()),
        Arc::new(FixedAuth),
    )
    .unwrap();
    let response = provider
        .generate(
            "gpt-5.4",
            &[
                ChatMessage::system("system rules"),
                ChatMessage::user("hello"),
            ],
            &[ToolSchema {
                name: "read_file".into(),
                description: "Read a file".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            }],
            &GenerationSettings {
                reasoning_effort: Some("medium".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(response.finish_reason.as_deref(), Some("stop"));
}

#[tokio::test]
async fn codex_provider_uses_default_model_when_requested_model_is_empty() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(body_json(json!({
            "model": "gpt-5.4",
            "store": false,
            "stream": true,
            "instructions": "",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hello"}]}
            ],
            "text": {"verbosity": "medium"},
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "fa82b1cf60263d970c98f29e8420cbd61e8c07e59e05008002d374235fa8abf3",
            "tool_choice": "auto",
            "parallel_tool_calls": true
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(&[json!({
                    "type": "response.completed",
                    "response": {"status": "completed"}
                })])),
        )
        .mount(&server)
        .await;

    let provider = CodexProvider::with_auth(
        format!("{}/backend-api/codex/responses", server.uri()),
        Arc::new(FixedAuth),
    )
    .unwrap();
    let response = provider
        .generate(
            "",
            &[ChatMessage::user("hello")],
            &[],
            &GenerationSettings::default(),
        )
        .await
        .unwrap();

    assert_eq!(response.finish_reason.as_deref(), Some("stop"));
}

#[tokio::test]
async fn codex_provider_streams_content_and_tool_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(&[
                    json!({"type": "response.output_text.delta", "delta": "done"}),
                    json!({
                        "type": "response.output_item.added",
                        "item": {
                            "type": "function_call",
                            "id": "fc_1",
                            "call_id": "call_1",
                            "name": "write_file",
                            "arguments": ""
                        }
                    }),
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "call_id": "call_1",
                        "delta": "{\"path\":\"out.txt\"}"
                    }),
                    json!({"type": "response.completed", "response": {"status": "completed"}}),
                ])),
        )
        .mount(&server)
        .await;

    let provider = CodexProvider::with_auth(
        format!("{}/backend-api/codex/responses", server.uri()),
        Arc::new(FixedAuth),
    )
    .unwrap();
    let messages = [ChatMessage::user("hello")];
    let settings = GenerationSettings::default();
    let mut stream = provider.generate_stream("gpt-5.4", &messages, &[], &settings);

    let mut saw_text = false;
    let mut saw_tool_delta = false;
    let mut done = None;
    use futures::StreamExt;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::ContentDelta(text) if text == "done" => saw_text = true,
            StreamEvent::ToolCallDelta {
                name: Some(name), ..
            } if name == "write_file" => saw_tool_delta = true,
            StreamEvent::Done(resp) => done = Some(resp),
            _ => {}
        }
    }

    assert!(saw_text);
    assert!(saw_tool_delta);
    let done = done.unwrap();
    assert_eq!(done.content.as_deref(), Some("done"));
    assert_eq!(done.tool_calls.len(), 1);
    assert_eq!(done.tool_calls[0].name, "write_file");
    assert_eq!(done.tool_calls[0].arguments, json!({"path": "out.txt"}));
}

#[tokio::test]
async fn codex_provider_surfaces_rejected_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
        .mount(&server)
        .await;

    let provider = CodexProvider::with_auth(
        format!("{}/backend-api/codex/responses", server.uri()),
        Arc::new(FixedAuth),
    )
    .unwrap();
    let err = provider
        .generate(
            "gpt-5.4",
            &[ChatMessage::user("hello")],
            &[],
            &GenerationSettings::default(),
        )
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("401"), "{err}");
    assert!(err.contains("codex login"), "{err}");
}
