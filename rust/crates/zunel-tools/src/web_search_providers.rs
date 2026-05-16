use async_trait::async_trait;
use serde_json::Value;
use zunel_util::{default_reqwest_client, read_text_capped};

use crate::error::{Error, Result};

/// Cap on a single web-search response body. Search providers return
/// JSON or HTML in the high-KB range; 8 MiB is generous for either and
/// blocks the OOM-by-chunked-body class.
const WEB_SEARCH_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
}

impl WebSearchResult {
    pub fn render(&self) -> String {
        format!("- {} ({})\n  {}", self.title, self.url, self.description)
    }
}

#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn search(&self, query: &str, n: usize) -> Result<Vec<WebSearchResult>>;
}

pub struct BraveProvider {
    api_key: String,
    endpoint: String,
    client: reqwest::Client,
}

impl BraveProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_endpoint(api_key, "https://api.search.brave.com".to_string())
    }
    pub fn with_endpoint(api_key: String, endpoint: String) -> Self {
        Self {
            api_key,
            endpoint,
            client: default_reqwest_client(false),
        }
    }
}

#[async_trait]
impl WebSearchProvider for BraveProvider {
    fn name(&self) -> &'static str {
        "brave"
    }

    async fn search(&self, query: &str, n: usize) -> Result<Vec<WebSearchResult>> {
        let url = format!("{}/res/v1/web/search", self.endpoint);
        let resp = self
            .client
            .get(url)
            .header("X-Subscription-Token", &self.api_key)
            .query(&[("q", query), ("count", &n.to_string())])
            .send()
            .await
            .map_err(|source| Error::Network {
                tool: "web_search".into(),
                source,
            })?;
        let text = read_text_capped(resp, WEB_SEARCH_MAX_BODY_BYTES)
            .await
            .map_err(|err| Error::Other {
                tool: "web_search".into(),
                message: format!("brave body read failed: {err}"),
            })?;
        let body: Value = serde_json::from_str(&text).map_err(|err| Error::Other {
            tool: "web_search".into(),
            message: format!("brave returned invalid JSON: {err}"),
        })?;
        let mut out = Vec::new();
        if let Some(results) = body.pointer("/web/results").and_then(|v| v.as_array()) {
            for r in results.iter().take(n) {
                out.push(WebSearchResult {
                    title: r.get("title").and_then(Value::as_str).unwrap_or("").into(),
                    url: r.get("url").and_then(Value::as_str).unwrap_or("").into(),
                    description: r
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                });
            }
        }
        Ok(out)
    }
}

pub struct DuckDuckGoProvider {
    client: reqwest::Client,
}

impl DuckDuckGoProvider {
    pub fn new() -> Self {
        Self {
            client: default_reqwest_client(false),
        }
    }
}
impl Default for DuckDuckGoProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WebSearchProvider for DuckDuckGoProvider {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }

    async fn search(&self, query: &str, n: usize) -> Result<Vec<WebSearchResult>> {
        let resp = self
            .client
            .get("https://duckduckgo.com/html/")
            .query(&[("q", query)])
            .send()
            .await
            .map_err(|source| Error::Network {
                tool: "web_search".into(),
                source,
            })?;
        let html = read_text_capped(resp, WEB_SEARCH_MAX_BODY_BYTES)
            .await
            .map_err(|err| Error::Other {
                tool: "web_search".into(),
                message: format!("ddg body read failed: {err}"),
            })?;
        let mut out = Vec::new();
        for block in html.split("result__body").skip(1).take(n) {
            let title = extract_between(block, "result__a\">", "</a>").unwrap_or("");
            let url = extract_between(block, r#"href="/l/?kh=-1&uddg="#, r#"""#).unwrap_or("");
            let desc = extract_between(block, "result__snippet\">", "</a>").unwrap_or("");
            if !title.is_empty() {
                out.push(WebSearchResult {
                    title: strip_html(title),
                    url: url.to_string(),
                    description: strip_html(desc),
                });
            }
        }
        Ok(out)
    }
}

fn extract_between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let start_idx = s.find(start)? + start.len();
    let tail = &s[start_idx..];
    let end_idx = tail.find(end)?;
    Some(&tail[..end_idx])
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match (ch, in_tag) {
            ('<', _) => in_tag = true,
            ('>', true) => in_tag = false,
            (c, false) => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Unimplemented-provider stub. Emits a clear runtime error.
pub struct StubProvider {
    pub provider_name: &'static str,
}

#[async_trait]
impl WebSearchProvider for StubProvider {
    fn name(&self) -> &'static str {
        self.provider_name
    }
    async fn search(&self, _query: &str, _n: usize) -> Result<Vec<WebSearchResult>> {
        Err(Error::Unimplemented {
            what: format!("web_search provider '{}'", self.provider_name),
        })
    }
}
