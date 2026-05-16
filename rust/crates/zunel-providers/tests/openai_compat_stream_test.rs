use std::collections::BTreeMap;

use futures::StreamExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, OpenAICompatProvider, StreamEvent,
};

fn sse_body(chunks: &[(&str, Option<u32>, Option<u32>)]) -> String {
    // `chunks` is [(content_delta, prompt_tokens_or_none_yet, completion_tokens_or_none_yet)]
    // Emits one chat.completion.chunk per entry + a final [DONE].
    let mut out = String::new();
    for (i, (delta, pt, ct)) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;
        let mut chunk = serde_json::json!({
            "id": format!("chunk-{i}"),
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": if delta.is_empty() { serde_json::json!({}) } else { serde_json::json!({ "content": delta }) },
                "finish_reason": if is_last { serde_json::json!("stop") } else { serde_json::Value::Null },
            }]
        });
        if let (Some(p), Some(c)) = (pt, ct) {
            chunk["usage"] = serde_json::json!({
                "prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c
            });
        }
        out.push_str(&format!(
            "data: {}\n\n",
            serde_json::to_string(&chunk).unwrap()
        ));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn streams_deltas_then_done() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/event-stream")
                .set_body_string(sse_body(&[
                    ("Hel", None, None),
                    ("lo, ", None, None),
                    ("world!", Some(5), Some(3)),
                ])),
        )
        .mount(&server)
        .await;

    let provider =
        OpenAICompatProvider::new("sk-test".into(), server.uri(), BTreeMap::new()).unwrap();

    let messages = [ChatMessage::user("hi")];
    let tools = [];
    let settings = GenerationSettings::default();
    let stream = provider.generate_stream("gpt-x", &messages, &tools, &settings);
    let events: Vec<_> = stream.collect().await;

    let deltas: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::ContentDelta(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["Hel", "lo, ", "world!"]);

    let done = events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::Done(resp)) => Some(resp.clone()),
            _ => None,
        })
        .next()
        .expect("done event present");
    assert_eq!(done.content.as_deref(), Some("Hello, world!"));
    assert_eq!(done.usage.prompt_tokens, 5);
    assert_eq!(done.usage.completion_tokens, 3);
}

#[tokio::test]
async fn request_body_asks_for_stream_and_usage() {
    use std::sync::{Arc, Mutex};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    struct Capture {
        out: Arc<Mutex<Option<serde_json::Value>>>,
    }
    impl Respond for Capture {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *self.out.lock().unwrap() = Some(body);
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n")
        }
    }

    let captured = Arc::new(Mutex::new(None));
    let responder = Capture {
        out: captured.clone(),
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(responder)
        .mount(&server)
        .await;

    let provider = OpenAICompatProvider::new("sk".into(), server.uri(), BTreeMap::new()).unwrap();

    let messages = [ChatMessage::user("hi")];
    let tools = [];
    let settings = GenerationSettings::default();
    let mut stream = provider.generate_stream("gpt-x", &messages, &tools, &settings);
    // Drain.
    while stream.next().await.is_some() {}

    let body = captured.lock().unwrap().take().expect("captured");
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(
        body["stream_options"]["include_usage"],
        serde_json::json!(true)
    );
}

#[tokio::test]
async fn non_streaming_error_still_emits_error_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("nope"))
        .mount(&server)
        .await;

    let provider = OpenAICompatProvider::new("sk".into(), server.uri(), BTreeMap::new()).unwrap();

    let messages = [ChatMessage::user("hi")];
    let tools = [];
    let settings = GenerationSettings::default();
    let stream = provider.generate_stream("gpt-x", &messages, &tools, &settings);
    let events: Vec<_> = stream.collect().await;
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        Err(zunel_providers::Error::ProviderReturned { status: 400, .. })
    ));
}
