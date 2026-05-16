use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

use reqwest::dns::Resolve;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use zunel_tools::ssrf::{filter_blocked_addrs, SsrfSafeResolver};
use zunel_tools::{web::WebFetchTool, Tool, ToolContext};

#[tokio::test]
async fn web_fetch_returns_markdown_of_response_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/doc"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<html><body><h1>Title</h1><p>body text</p></body></html>")
                .insert_header("content-type", "text/html; charset=utf-8"),
        )
        .mount(&server)
        .await;

    let tool = WebFetchTool::for_test();
    let url = format!("{}/doc", server.uri());
    let res = tool
        .execute(json!({"url": url}), &ToolContext::for_test())
        .await;
    assert!(!res.is_error, "{res:?}");
    assert!(res.content.contains("Title"));
    assert!(res.content.contains("body text"));
}

#[tokio::test]
async fn web_fetch_rejects_loopback_when_ssrf_enabled() {
    let tool = WebFetchTool::new();
    let res = tool
        .execute(
            json!({"url": "http://127.0.0.1:65432/blocked"}),
            &ToolContext::for_test(),
        )
        .await;
    assert!(res.is_error);
    assert!(res.content.to_lowercase().contains("ssrf") || res.content.contains("loopback"));
}

#[test]
fn filter_blocked_addrs_rejects_all_blocked() {
    // DNS rebinding scenario: a hostname resolves only to private/loopback IPs.
    // The resolver must reject before connecting.
    let addrs = vec![
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)), 0),
    ];
    let err = filter_blocked_addrs(addrs, false, "evil.test").unwrap_err();
    assert!(
        err.to_string().contains("ssrf"),
        "expected ssrf error, got {err}"
    );
}

#[test]
fn filter_blocked_addrs_passes_safe_ips() {
    let addrs = vec![SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
        0,
    )];
    let safe = filter_blocked_addrs(addrs.clone(), false, "example.com").unwrap();
    assert_eq!(safe, addrs);
}

#[test]
fn filter_blocked_addrs_strips_blocked_subset() {
    // Mixed result: keep only the public IP, drop the private one.
    let public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 0);
    let private = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 0);
    let safe = filter_blocked_addrs(vec![public, private], false, "mixed.test").unwrap();
    assert_eq!(safe, vec![public]);
}

#[test]
fn filter_blocked_addrs_allow_loopback_passes_through() {
    let addrs = vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0)];
    let safe = filter_blocked_addrs(addrs.clone(), true, "localhost").unwrap();
    assert_eq!(safe, addrs);
}

#[tokio::test]
async fn ssrf_safe_resolver_rejects_localhost_resolution() {
    // 'localhost' resolves via the system resolver to 127.0.0.1 / ::1.
    // Both are blocked, so the resolver must surface an error rather than
    // hand connection-ready addresses back to reqwest.
    let resolver = SsrfSafeResolver::new(false);
    let name = reqwest::dns::Name::from_str("localhost").expect("localhost is a valid Name");
    let result = Arc::new(resolver).resolve(name).await;
    let err = match result {
        Ok(_) => panic!("localhost must be rejected when allow_loopback is false"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("ssrf"),
        "expected ssrf error, got {err}"
    );
}
