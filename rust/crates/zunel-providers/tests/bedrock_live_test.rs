//! End-to-end Bedrock test that hits real AWS.
//!
//! Skipped by default (`#[ignore]`) so CI never needs AWS credentials.
//! To run locally:
//!
//! ```sh
//! aws sso login --profile <your-profile>
//! AWS_PROFILE=<your-profile> \
//! BEDROCK_TEST_MODEL=us.anthropic.claude-sonnet-4-5-20250929-v1:0 \
//! BEDROCK_TEST_REGION=us-west-2 \
//!   cargo test -p zunel-providers --test bedrock_live_test \
//!     -- --ignored --nocapture
//! ```
//!
//! Verifies the standard AWS-credential-chain workflow against Bedrock
//! works end-to-end through zunel: load creds via the standard chain,
//! sign with Sig V4, run a Converse + ConverseStream round-trip, decode
//! tokens.

use futures::StreamExt;
use zunel_config::BedrockProvider as BedrockProviderConfig;
use zunel_providers::{
    bedrock::BedrockProvider, ChatMessage, GenerationSettings, LLMProvider, StreamEvent, ToolSchema,
};

fn skip_unless_configured() -> Option<(String, BedrockProviderConfig)> {
    let model = std::env::var("BEDROCK_TEST_MODEL").ok()?;
    let region = std::env::var("BEDROCK_TEST_REGION").ok();
    let profile = std::env::var("AWS_PROFILE").ok();
    Some((model, BedrockProviderConfig { region, profile }))
}

#[ignore = "requires AWS credentials and BEDROCK_TEST_MODEL"]
#[tokio::test]
async fn live_converse_round_trip() {
    let Some((model, cfg)) = skip_unless_configured() else {
        eprintln!("BEDROCK_TEST_MODEL not set; skipping live Bedrock test");
        return;
    };
    let provider = BedrockProvider::new(cfg)
        .await
        .expect("BedrockProvider::new");
    let messages = [
        ChatMessage::system("You are a terse test fixture. Reply with a single short sentence."),
        ChatMessage::user("Say hello."),
    ];
    let tools: [ToolSchema; 0] = [];
    let settings = GenerationSettings {
        max_tokens: Some(64),
        ..Default::default()
    };
    let response = provider
        .generate(&model, &messages, &tools, &settings)
        .await
        .expect("converse");
    let content = response.content.as_deref().expect("response has content");
    assert!(!content.is_empty(), "response should have non-empty text");
    assert!(
        response.usage.prompt_tokens > 0,
        "usage.prompt_tokens should be populated"
    );
    eprintln!(
        "live converse: model={model} content={:?} usage={:?} finish={:?}",
        content, response.usage, response.finish_reason
    );
}

#[ignore = "requires AWS credentials and BEDROCK_TEST_MODEL"]
#[tokio::test]
async fn live_converse_stream_round_trip() {
    let Some((model, cfg)) = skip_unless_configured() else {
        eprintln!("BEDROCK_TEST_MODEL not set; skipping live Bedrock test");
        return;
    };
    let provider = BedrockProvider::new(cfg)
        .await
        .expect("BedrockProvider::new");
    let messages = [
        ChatMessage::system("You are a terse test fixture. Reply with a single short sentence."),
        ChatMessage::user("Say hello in three words."),
    ];
    let tools: [ToolSchema; 0] = [];
    let settings = GenerationSettings {
        max_tokens: Some(64),
        ..Default::default()
    };
    let stream = provider.generate_stream(&model, &messages, &tools, &settings);
    futures::pin_mut!(stream);
    let mut accumulated = String::new();
    let mut saw_done = false;
    while let Some(event) = stream.next().await {
        match event.expect("stream event ok") {
            StreamEvent::ContentDelta(text) => accumulated.push_str(&text),
            StreamEvent::Done(resp) => {
                saw_done = true;
                let final_content = resp.content.unwrap_or_default();
                assert_eq!(
                    final_content, accumulated,
                    "Done.content should equal accumulated deltas"
                );
                assert!(
                    resp.usage.prompt_tokens > 0,
                    "usage.prompt_tokens should be populated"
                );
                eprintln!(
                    "live stream: model={model} content={:?} usage={:?} finish={:?}",
                    final_content, resp.usage, resp.finish_reason
                );
            }
            _ => {}
        }
    }
    assert!(saw_done, "stream must emit a final Done event");
    assert!(!accumulated.is_empty(), "stream should yield text");
}
