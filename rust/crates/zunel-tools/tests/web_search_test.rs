use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use zunel_tools::{web::WebSearchTool, Tool, ToolContext};

#[tokio::test]
async fn brave_search_returns_formatted_results() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "web": {
            "results": [
                {"title": "Rust homepage", "url": "https://rust-lang.org", "description": "The Rust programming language"},
                {"title": "Docs", "url": "https://doc.rust-lang.org", "description": "Rust docs"},
            ]
        }
    });
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let tool = WebSearchTool::brave_with_endpoint("test-key".into(), server.uri());
    let res = tool
        .execute(json!({"query": "rust", "n": 2}), &ToolContext::for_test())
        .await;
    assert!(!res.is_error, "{res:?}");
    assert!(res.content.contains("Rust homepage"));
    assert!(res.content.contains("https://rust-lang.org"));
}

#[tokio::test]
async fn unimplemented_provider_emits_clear_error() {
    let tool = WebSearchTool::stub("tavily");
    let res = tool
        .execute(json!({"query": "rust"}), &ToolContext::for_test())
        .await;
    assert!(res.is_error);
    assert!(res.content.contains("tavily"));
    assert!(res.content.to_lowercase().contains("not implemented"));
}
