//! `CompactionService::compact_session` collapses the stale head of a
//! session into a single summary row produced by the provider, while
//! leaving the tail untouched and bumping `last_consolidated`.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::json;
use zunel_core::{ChatRole, CompactionService, Session};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, StreamEvent, ToolSchema, Usage,
};

struct CannedSummaryProvider {
    summary: String,
}

#[async_trait]
impl LLMProvider for CannedSummaryProvider {
    async fn generate(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _settings: &GenerationSettings,
    ) -> zunel_providers::Result<LLMResponse> {
        Ok(LLMResponse {
            content: Some(self.summary.clone()),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: None,
        })
    }

    fn generate_stream<'a>(
        &'a self,
        _model: &'a str,
        _messages: &'a [ChatMessage],
        _tools: &'a [ToolSchema],
        _settings: &'a GenerationSettings,
    ) -> BoxStream<'a, zunel_providers::Result<StreamEvent>> {
        unreachable!("compaction uses the non-streaming generate()")
    }
}

#[tokio::test]
async fn compact_session_collapses_stale_head() {
    let mut session = Session::new("test:compact");
    for i in 0..30 {
        let role = if i % 2 == 0 {
            ChatRole::User
        } else {
            ChatRole::Assistant
        };
        session.add_message(role, format!("msg #{i}"));
    }
    let provider: Arc<dyn LLMProvider> = Arc::new(CannedSummaryProvider {
        summary: "user discussed feature X with assistant; open todo: write tests.".into(),
    });
    let svc = CompactionService::new(provider, "gpt-x".into());
    let collapsed = svc
        .compact_session(&mut session, 8)
        .await
        .expect("compaction succeeded");

    assert!(collapsed > 0, "should collapse stale head");
    assert_eq!(
        session.messages().len(),
        9,
        "expected 1 summary + 8 tail rows, got {}",
        session.messages().len()
    );
    assert_eq!(
        session.last_consolidated(),
        0,
        "summary row sits at the boundary of unconsolidated history",
    );
    let summary = &session.messages()[0];
    assert_eq!(summary["role"].as_str(), Some("system"));
    assert!(summary["content"]
        .as_str()
        .unwrap()
        .starts_with("[Prior conversation summary]"));
    assert_eq!(
        session.messages().last().unwrap()["content"].as_str(),
        Some("msg #29"),
        "tail of N is preserved verbatim",
    );
}

#[tokio::test]
async fn compact_session_is_noop_when_under_threshold() {
    let mut session = Session::new("test:compact");
    for i in 0..5 {
        session.add_message(ChatRole::User, format!("msg #{i}"));
    }
    let provider: Arc<dyn LLMProvider> = Arc::new(CannedSummaryProvider {
        summary: "should never be called".into(),
    });
    let svc = CompactionService::new(provider, "gpt-x".into());
    let collapsed = svc
        .compact_session(&mut session, 8)
        .await
        .expect("noop succeeded");
    assert_eq!(collapsed, 0);
    assert_eq!(session.messages().len(), 5);
}

#[test]
fn replace_range_with_summary_advances_last_consolidated() {
    let mut session = Session::new("test:replace");
    session.add_message(ChatRole::User, "a");
    session.add_message(ChatRole::Assistant, "b");
    session.add_message(ChatRole::User, "c");
    session.add_message(ChatRole::Assistant, "d");
    session.replace_range_with_summary(
        0,
        2,
        json!({"role": "system", "content": "[Prior conversation summary]\nshort"}),
    );
    assert_eq!(session.messages().len(), 3);
    assert_eq!(session.last_consolidated(), 0);
    assert_eq!(session.messages()[0]["role"].as_str(), Some("system"));
    assert_eq!(session.messages()[1]["content"].as_str(), Some("c"));
}
