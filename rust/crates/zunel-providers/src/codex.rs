use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::base::{
    ChatMessage, GenerationSettings, LLMProvider, LLMResponse, Role, StreamEvent, ToolSchema,
};
use crate::error::{Error, Result};
use crate::responses::{convert_messages, convert_tools, ResponsesStreamParser};
use crate::sse::SseBuffer;

pub const DEFAULT_CODEX_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const DEFAULT_CODEX_MODEL: &str = "gpt-5.4";
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_USER_AGENT: &str = "zunel (rust)";

const CODEX_LOGIN_HINT: &str =
    "Sign in with `codex login` using file-backed credentials, then retry.";

#[derive(Clone, PartialEq, Eq)]
pub struct CodexAuth {
    pub access_token: String,
    pub account_id: String,
}

impl std::fmt::Debug for CodexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAuth")
            .field("access_token", &"<redacted>")
            .field("account_id", &self.account_id)
            .finish()
    }
}

#[async_trait]
pub trait CodexAuthProvider: Send + Sync {
    async fn load(&self) -> Result<CodexAuth>;
}

#[derive(Debug, Clone)]
pub struct FileCodexAuthProvider {
    codex_home: PathBuf,
}

impl FileCodexAuthProvider {
    pub fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    pub fn from_env() -> Result<Self> {
        if let Ok(home) = std::env::var("CODEX_HOME") {
            return Ok(Self::new(PathBuf::from(home)));
        }
        let home = std::env::var("HOME").map_err(|_| {
            Error::Auth(format!(
                "Codex OAuth credentials unavailable: HOME is not set. {CODEX_LOGIN_HINT}"
            ))
        })?;
        Ok(Self::new(PathBuf::from(home).join(".codex")))
    }

    fn auth_path(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }
}

pub struct CodexProvider {
    client: reqwest::Client,
    api_base: String,
    auth: Arc<dyn CodexAuthProvider>,
}

impl CodexProvider {
    pub fn new(api_base: Option<String>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::limited(5))
                .dns_resolver(Arc::new(zunel_util::SsrfSafeResolver::new(false)))
                .build()?,
            api_base: api_base.unwrap_or_else(|| DEFAULT_CODEX_URL.to_string()),
            auth: Arc::new(FileCodexAuthProvider::from_env()?),
        })
    }

    pub fn with_auth(api_base: String, auth: Arc<dyn CodexAuthProvider>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::limited(5))
                .dns_resolver(Arc::new(zunel_util::SsrfSafeResolver::new(false)))
                .build()?,
            api_base,
            auth,
        })
    }

    pub fn default_model(&self) -> &'static str {
        DEFAULT_CODEX_MODEL
    }

    fn request_body(
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        settings: &GenerationSettings,
    ) -> Result<Value> {
        let converted = convert_messages(messages)?;
        let effective_model = if model.trim().is_empty() {
            DEFAULT_CODEX_MODEL
        } else {
            model
        };
        let mut body = json!({
            "model": effective_model,
            "store": false,
            "stream": true,
            "instructions": converted.instructions,
            "input": converted.input,
            "text": {"verbosity": "medium"},
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": prompt_cache_key(messages)?,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });
        if let Some(effort) = &settings.reasoning_effort {
            body["reasoning"] = json!({"effort": effort});
        }
        if !tools.is_empty() {
            body["tools"] = convert_tools(tools);
        }
        Ok(body)
    }
}

#[async_trait]
impl LLMProvider for CodexProvider {
    async fn generate(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        settings: &GenerationSettings,
    ) -> Result<LLMResponse> {
        let mut stream = self.generate_stream(model, messages, tools, settings);
        let mut final_response = None;
        use futures::StreamExt;
        while let Some(event) = stream.next().await {
            if let StreamEvent::Done(resp) = event? {
                final_response = Some(resp);
            }
        }
        final_response.ok_or_else(|| Error::Parse("codex stream ended without Done".into()))
    }

    fn generate_stream<'a>(
        &'a self,
        model: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSchema],
        settings: &'a GenerationSettings,
    ) -> BoxStream<'a, Result<StreamEvent>> {
        Box::pin(async_stream::try_stream! {
            let token = self.auth.load().await?;
            let body = Self::request_body(model, messages, tools, settings)?;
            let mut auth_header = reqwest::header::HeaderValue::from_str(
                &format!("Bearer {}", token.access_token),
            )
            .map_err(|e| Error::Auth(format!("invalid Codex access token header: {e}")))?;
            auth_header.set_sensitive(true);
            let response = self
                .client
                .post(&self.api_base)
                .header(reqwest::header::AUTHORIZATION, auth_header)
                .header("chatgpt-account-id", token.account_id)
                .header("OpenAI-Beta", "responses=experimental")
                .header("originator", CODEX_ORIGINATOR)
                .header(reqwest::header::USER_AGENT, CODEX_USER_AGENT)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await?;
            let status = response.status();
            if !status.is_success() {
                let text = zunel_util::read_text_capped(response, 64 * 1024)
                    .await
                    .unwrap_or_default();
                Err(Error::ProviderReturned {
                    status: status.as_u16(),
                    body: friendly_error(status.as_u16(), &text),
                })?;
                return;
            }

            let mut sse = SseBuffer::new();
            let mut parser = ResponsesStreamParser::new();
            let mut saw_done = false;
            let mut stream = response.bytes_stream();
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(Error::Network)?;
                let payloads = sse.feed(&chunk).map_err(|err| {
                    Error::Parse(format!("codex SSE buffer overflowed: {err}"))
                })?;
                for payload in payloads {
                    let Some(payload) = payload else {
                        if !saw_done {
                            for event in parser.finish()? {
                                yield event;
                            }
                        }
                        return;
                    };
                    let value: Value = serde_json::from_str(&payload)
                        .map_err(|e| Error::Parse(format!("codex event decode: {e}")))?;
                    for event in parser.accept(&value)? {
                        saw_done = saw_done || matches!(event, StreamEvent::Done(_));
                        yield event;
                    }
                }
            }
            if !saw_done {
                for event in parser.finish()? {
                    yield event;
                }
            }
        })
    }
}

#[async_trait]
impl CodexAuthProvider for FileCodexAuthProvider {
    async fn load(&self) -> Result<CodexAuth> {
        let path = self.auth_path();
        // tokio::fs delegates to spawn_blocking under the hood so the
        // runtime worker isn't pinned across the (small but synchronous)
        // auth.json read. Called once per turn in the hot path.
        let raw = tokio::fs::read_to_string(&path).await.map_err(|e| {
            Error::Auth(format!(
                "Codex OAuth credentials unavailable: failed to read {}: {e}. {CODEX_LOGIN_HINT}",
                path.display()
            ))
        })?;
        let value: Value = serde_json::from_str(&raw).map_err(|e| {
            Error::Auth(format!(
                "Codex OAuth credentials unavailable: failed to parse {}: {e}. {CODEX_LOGIN_HINT}",
                path.display()
            ))
        })?;
        let access_token = value
            .pointer("/tokens/access_token")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::Auth(format!(
                    "Codex OAuth credentials unavailable: auth.json does not contain an access token. {CODEX_LOGIN_HINT}"
                ))
            })?
            .to_string();
        let account_id = find_account_id(&value).ok_or_else(|| {
            Error::Auth(format!(
                "Codex OAuth credentials unavailable: auth.json does not contain a ChatGPT account id. {CODEX_LOGIN_HINT}"
            ))
        })?;

        Ok(CodexAuth {
            access_token,
            account_id,
        })
    }
}

fn find_account_id(value: &Value) -> Option<String> {
    [
        "/account_id",
        "/chatgpt_account_id",
        "/account/id",
        "/profile/account_id",
        "/tokens/account_id",
    ]
    .iter()
    .find_map(|pointer| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
    })
}

/// Stable cache-key fingerprint for the prompt's **prefix**, not the
/// full transcript.
///
/// Codex's `prompt_cache_key` is a hint that lets the provider bucket
/// requests for cross-call caching; the key should be stable across
/// requests that share a common prefix and distinct otherwise. The
/// earlier implementation re-serialised the entire message history,
/// sorted every nested key alphabetically, and SHA-256'd the result
/// on every turn — O(n log n) in the history length for what should
/// be an O(1) operation. For sessions with 100+ messages and
/// tool-call loops where each iteration re-keys, that cost is real.
///
/// Hash just the first message (typically the system prompt). Within
/// a session every turn shares the same prefix, so the key stays
/// stable; across sessions with different system prompts the keys
/// diverge. Two sessions with identical system prompts collide
/// intentionally — that's a *cache hit*, which saves the user money.
fn prompt_cache_key(messages: &[ChatMessage]) -> Result<String> {
    let mut hasher = Sha256::new();
    if let Some(first) = messages.first() {
        // Role + content uniquely identifies the prefix without
        // pulling in the rest of the (per-turn-variable) history.
        let role_tag: &str = match first.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        hasher.update(role_tag.as_bytes());
        hasher.update(b"\n");
        hasher.update(first.content.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn friendly_error(status: u16, raw: &str) -> String {
    match status {
        401 | 403 => format!(
            "HTTP {status}: Codex credentials were rejected. Re-run `codex login` and retry."
        ),
        429 => "ChatGPT usage quota exceeded or rate limit triggered. Please try again later."
            .to_string(),
        _ => format!(
            "HTTP {status}: {}",
            raw.chars().take(500).collect::<String>()
        ),
    }
}

#[cfg(test)]
mod prompt_cache_key_tests {
    use super::*;

    #[test]
    fn key_is_stable_across_tail_growth() {
        // Two conversations sharing the same first message must
        // produce the same prompt_cache_key, regardless of how the
        // tail has evolved. This is the property that makes Codex's
        // prompt cache layer actually hit — the earlier
        // hash-the-whole-history implementation broke it by
        // generating a fresh key on every turn.
        let short = vec![
            ChatMessage::system("you are a helpful agent"),
            ChatMessage::user("hi"),
        ];
        let long = vec![
            ChatMessage::system("you are a helpful agent"),
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
            ChatMessage::user("what's 2+2"),
            ChatMessage::assistant("4"),
        ];
        assert_eq!(
            prompt_cache_key(&short).unwrap(),
            prompt_cache_key(&long).unwrap()
        );
    }

    #[test]
    fn key_differs_when_first_message_differs() {
        // Different system prompts → different keys.
        let a = vec![ChatMessage::system("persona A"), ChatMessage::user("hi")];
        let b = vec![ChatMessage::system("persona B"), ChatMessage::user("hi")];
        assert_ne!(prompt_cache_key(&a).unwrap(), prompt_cache_key(&b).unwrap());
    }

    #[test]
    fn key_differs_when_first_role_differs() {
        // Same text body but different role at position 0 should
        // produce different keys — otherwise a user prompt of
        // "do X" collides with a system message of "do X".
        let as_system = vec![ChatMessage::system("do X")];
        let as_user = vec![ChatMessage::user("do X")];
        assert_ne!(
            prompt_cache_key(&as_system).unwrap(),
            prompt_cache_key(&as_user).unwrap()
        );
    }

    #[test]
    fn key_for_empty_history_is_stable() {
        let empty: Vec<ChatMessage> = Vec::new();
        // SHA-256 of the empty input is a well-known constant.
        assert_eq!(
            prompt_cache_key(&empty).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
