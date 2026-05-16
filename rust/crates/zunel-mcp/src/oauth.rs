//! OAuth2 / PKCE flow for remote MCP servers, plus refresh-token rotation.
//!
//! This module is the single source of truth for all the cryptography and
//! IdP I/O involved in authorising a remote MCP server. There are three
//! public entry points the rest of the workspace builds on:
//!
//! - [`refresh_if_needed`] — long-standing helper that the gateway calls
//!   when it wires up a remote MCP server (and now from the periodic
//!   refresh task in `zunel gateway`). Reads the cached
//!   `<home>/mcp-oauth/<server>/token.json`, decides whether to attempt
//!   `grant_type=refresh_token`, and rewrites the cache atomically.
//! - [`start_flow`] — discovers the IdP's `authorize`/`token` endpoints,
//!   dynamic-client-registers if needed, generates state+PKCE verifier,
//!   and persists `pending.json` so a *separate* call can finish the
//!   exchange. No browser is opened, no callback server is bound — those
//!   are responsibilities of the caller (the CLI binds a localhost
//!   server; the chat-driven path posts the URL to Slack and waits for
//!   the user to paste back the redirect).
//! - [`complete_flow`] — accepts either a full pasted callback URL or a
//!   raw `?code=…&state=…` payload, loads `pending.json`, validates
//!   state, exchanges the authorisation code for a token, atomic-writes
//!   `token.json`, and deletes `pending.json`.
//!
//! The convenience [`refresh_all_oauth_servers`] walks every OAuth-enabled
//! server in a [`Config`] and runs `refresh_if_needed` on each, returning
//! a per-server outcome list for the periodic gateway task.
//!
//! ## On-disk state
//!
//! Pending login (lives only between `start_flow` and `complete_flow`):
//!
//! ```json
//! { "state": "...", "verifier": "...", "redirectUri": "...",
//!   "clientId": "...", "clientSecret": "...",
//!   "tokenUrl": "...", "authorizationUrl": "...",
//!   "scopeRequested": "...", "expiresAt": 1714752000 }
//! ```
//!
//! TTL is 10 minutes (`PENDING_TTL_SECS`). Both [`start_flow`] and
//! [`complete_flow`] purge expired pending files on entry.
//!
//! Final cached token (long-lived, refreshed in place): see
//! [`zunel_config::CachedMcpOAuthToken`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use reqwest::header::WWW_AUTHENTICATE;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zunel_config::{
    mcp_oauth::{load_token, save_token, unix_timestamp_now, DEFAULT_REFRESH_SKEW_SECS},
    CachedMcpOAuthToken, Config, McpOAuthConfig, McpServerConfig,
};

/// 10 minutes — IdPs typically allow ~5 min on the authorisation code
/// itself, plus we want some slack for Slack roundtrip + user
/// distraction. `start_flow` writes `pending.expiresAt = now + this`
/// and `complete_flow` rejects on expiry.
pub const PENDING_TTL_SECS: u64 = 600;

/// Hard ceiling on URL length we'll accept in `complete_flow`. Real
/// callback URLs land well under 4 KiB; rejecting megabyte-sized
/// pastes is a cheap DoS guard against accidental log dumps.
const MAX_CALLBACK_URL_LEN: usize = 16 * 1024;

/// Cap on a single OAuth discovery / token endpoint response. Real
/// payloads are tiny (RFC 8414 + 6749 fields); 256 KiB is room for a
/// pathologically verbose IdP without letting a hostile / compromised
/// IdP OOM the gateway by chunked-transferring megabytes.
const OAUTH_MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Read an OAuth-endpoint response with a body cap, then JSON-decode.
/// Centralises the "size-cap → text → from_str" sequence we use on
/// every IdP response.
async fn oauth_capped_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> anyhow::Result<T> {
    let text = zunel_util::read_text_capped(response, OAUTH_MAX_RESPONSE_BYTES)
        .await
        .with_context(|| "reading oauth response body")?;
    serde_json::from_str(&text).with_context(|| "parsing oauth response JSON")
}

/// Result of a [`refresh_if_needed`] call. Variants are exhaustive
/// so `register_mcp_tools` can pattern-match and emit appropriately
/// scoped log lines (info vs warn).
#[derive(Debug)]
pub enum Outcome {
    /// No `token.json` on disk yet; nothing to refresh.
    NotCached,
    /// Cached token is still within its TTL (minus skew). Includes
    /// remaining seconds for log breadcrumbs.
    StillFresh { secs_remaining: u64 },
    /// Token had no `expires_in` value; we treat it as long-lived
    /// and skip refresh. Caller may still get a 401 at connect
    /// time, in which case the cleanup path is `mcp login --force`.
    NoExpiry,
    /// Token was expired but no `refresh_token` is on file (e.g.
    /// the IdP didn't issue one). Caller should fall through to
    /// the existing dead-bearer code path; only `mcp login` can
    /// recover.
    NoRefreshToken,
    /// Token was expired and the cached metadata didn't include a
    /// `tokenUrl`. Same recovery as `NoRefreshToken`.
    NoTokenUrl,
    /// Refresh succeeded; the on-disk cache has been rewritten.
    Refreshed { new_expires_in: Option<u64> },
    /// Refresh attempt failed (network, 4xx from the IdP, malformed
    /// response). The on-disk cache is untouched.
    RefreshFailed(String),
}

impl Outcome {
    /// `true` when the caller can safely attach the cached
    /// `Authorization` header and expect it to work. False for
    /// states where the header is known to be stale (the caller
    /// will probably still try, but won't be surprised by a 401).
    pub fn header_likely_valid(&self) -> bool {
        matches!(
            self,
            Outcome::StillFresh { .. } | Outcome::Refreshed { .. } | Outcome::NoExpiry
        )
    }

    /// `true` when the agent should prompt the human for an
    /// interactive `mcp login` (no cached refresh token, never
    /// logged in, missing token URL). The chat-driven login skill
    /// keys off this distinction.
    pub fn needs_interactive_login(&self) -> bool {
        matches!(
            self,
            Outcome::NotCached | Outcome::NoRefreshToken | Outcome::NoTokenUrl
        )
    }
}

/// Refresh the cached OAuth token for `server_name` if it's past
/// its expiry minus [`DEFAULT_REFRESH_SKEW_SECS`].
///
/// On success the on-disk cache is rewritten in place via the
/// atomic-rename path provided by [`zunel_config::mcp_oauth`]. The
/// function never panics on bad input; every failure mode maps to
/// an [`Outcome`] variant the caller can log.
pub async fn refresh_if_needed(home: &Path, server_name: &str) -> Outcome {
    refresh_if_needed_with(
        home,
        server_name,
        DEFAULT_REFRESH_SKEW_SECS,
        unix_timestamp_now(),
        &zunel_util::default_reqwest_client(false),
    )
    .await
}

/// Test-friendly variant of [`refresh_if_needed`] that lets tests
/// inject a deterministic clock and reqwest client (e.g. one
/// pointed at a `wiremock::MockServer`).
pub async fn refresh_if_needed_with(
    home: &Path,
    server_name: &str,
    skew_secs: u64,
    now_unix: u64,
    client: &reqwest::Client,
) -> Outcome {
    let mut token = match load_token(home, server_name) {
        Ok(Some(token)) => token,
        Ok(None) => return Outcome::NotCached,
        Err(err) => {
            tracing::warn!(
                server = server_name,
                error = %err,
                "ignoring invalid MCP OAuth token cache"
            );
            return Outcome::NotCached;
        }
    };

    let secs_remaining = match token.expires_at() {
        Some(exp) => exp.saturating_sub(now_unix),
        None => return Outcome::NoExpiry,
    };

    if !token.is_expired(now_unix, skew_secs) {
        return Outcome::StillFresh { secs_remaining };
    }

    let Some(refresh_token) = token.refresh_token.clone() else {
        return Outcome::NoRefreshToken;
    };
    if token.token_url.is_empty() {
        return Outcome::NoTokenUrl;
    }

    match exchange_refresh_token(client, &token, &refresh_token).await {
        Ok(refreshed) => {
            apply_refresh(&mut token, refreshed, now_unix);
            if let Err(err) = save_token(home, server_name, &token) {
                return Outcome::RefreshFailed(format!("persisting refreshed token: {err}"));
            }
            Outcome::Refreshed {
                new_expires_in: token.expires_in,
            }
        }
        Err(err) => Outcome::RefreshFailed(err),
    }
}

/// Walk every OAuth-enabled remote MCP server in `cfg` and call
/// [`refresh_if_needed`] on it. Designed for the periodic refresh task
/// in `zunel gateway`: it never errors out as a whole, and the per-server
/// outcomes are returned so the caller can log them at the right level
/// (`info` for `Refreshed`, `warn` for `RefreshFailed`, etc.).
pub async fn refresh_all_oauth_servers(home: &Path, cfg: &Config) -> Vec<(String, Outcome)> {
    let mut out = Vec::new();
    for (name, server) in &cfg.tools.mcp_servers {
        if !is_oauth_enabled_remote(server) {
            continue;
        }
        let outcome = refresh_if_needed(home, name).await;
        out.push((name.clone(), outcome));
    }
    out
}

fn is_oauth_enabled_remote(server: &McpServerConfig) -> bool {
    if server.url.is_none() {
        return false;
    }
    server
        .normalized_oauth()
        .map(|oauth| oauth.enabled)
        .unwrap_or(false)
}

#[derive(Debug)]
struct RefreshedFields {
    access_token: String,
    token_type: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

async fn exchange_refresh_token(
    client: &reqwest::Client,
    token: &CachedMcpOAuthToken,
    refresh_token: &str,
) -> Result<RefreshedFields, String> {
    let mut form: BTreeMap<&str, String> = BTreeMap::new();
    form.insert("grant_type", "refresh_token".into());
    form.insert("refresh_token", refresh_token.to_string());
    form.insert("client_id", token.client_id.clone());
    if let Some(secret) = &token.client_secret {
        form.insert("client_secret", secret.clone());
    }

    let response = client
        .post(&token.token_url)
        .form(&form)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|err| format!("token endpoint request failed: {err}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| format!("reading token endpoint body: {err}"))?;
    if !status.is_success() {
        // Truncate the body so a verbose IdP error page doesn't
        // dominate the log line; 256 chars is enough to spot
        // `invalid_grant` / `invalid_client` style markers.
        let snippet = body.chars().take(256).collect::<String>();
        return Err(format!(
            "token endpoint returned HTTP {status}: {}",
            snippet.trim()
        ));
    }
    let json: Value = serde_json::from_str(&body)
        .map_err(|err| format!("token endpoint returned non-JSON body: {err}"))?;
    let access_token = json
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "token endpoint response missing access_token".to_string())?
        .to_string();
    Ok(RefreshedFields {
        access_token,
        token_type: json
            .get("token_type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        refresh_token: json
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        expires_in: json.get("expires_in").and_then(Value::as_u64),
        scope: json
            .get("scope")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

fn apply_refresh(token: &mut CachedMcpOAuthToken, refreshed: RefreshedFields, now_unix: u64) {
    token.access_token = refreshed.access_token;
    if let Some(token_type) = refreshed.token_type {
        token.token_type = Some(token_type);
    }
    // Some IdPs rotate refresh tokens (RFC 6749 §10.4). Persist the
    // new value when present, but don't *erase* the existing one if
    // the response omits it.
    if let Some(rt) = refreshed.refresh_token {
        token.refresh_token = Some(rt);
    }
    if let Some(expires_in) = refreshed.expires_in {
        token.expires_in = Some(expires_in);
    }
    if let Some(scope) = refreshed.scope {
        token.scope = Some(scope);
    }
    token.obtained_at = now_unix;
}

// =============================================================================
// Authorisation code flow (start_flow / complete_flow)
// =============================================================================

/// Opaque, on-disk representation of an in-flight `start_flow`. The
/// CLI and the chat-driven login both write the same shape so that
/// either can finish what the other started (e.g. operator opens the
/// URL on a phone, completes via Slack message). camelCase to match
/// every other zunel-on-disk JSON; do not change without a migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingFlow {
    state: String,
    verifier: String,
    redirect_uri: String,
    client_id: String,
    client_secret: Option<String>,
    token_url: String,
    authorization_url: String,
    scope_requested: Option<String>,
    /// Server URL, mirrored at start time so `complete_flow` can
    /// pass it through as `resource=` on the token exchange even if
    /// the on-disk config is later rewritten between start + finish.
    resource: Option<String>,
    expires_at: u64,
}

impl PendingFlow {
    fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at
    }
}

/// Result of [`start_flow`]. The caller (CLI or chat-driven self-tool)
/// surfaces `authorize_url` to the human; everything else is bookkeeping
/// the caller forwards back into [`complete_flow`].
#[derive(Debug, Clone)]
pub struct StartedFlow {
    pub server: String,
    pub authorize_url: String,
    pub redirect_uri: String,
    pub state: String,
    /// Wall-clock seconds until the persisted pending file expires.
    pub expires_in: u64,
}

/// Result of [`complete_flow`]. `token_path` is exposed primarily so
/// the CLI can echo the location into stdout; chat callers ignore it.
#[derive(Debug, Clone)]
pub struct CompletedFlow {
    pub server: String,
    pub token_path: PathBuf,
    pub scopes: Option<String>,
    pub expires_in: Option<u64>,
}

/// Begin an OAuth login for `server_name`.
///
/// Steps performed:
///
/// 1. Discover authorize / token / registration endpoints (RFC 9728 →
///    `WWW-Authenticate: resource_metadata` → `.well-known/oauth-authorization-server`).
/// 2. Resolve client credentials, dynamic-client-registering if no
///    `clientId` is configured.
/// 3. Generate a fresh `state` and PKCE verifier (S256 challenge).
/// 4. Persist `pending.json` so the caller can finish later.
/// 5. Return the [`StartedFlow`] including the authorize URL.
///
/// `state_override` is for tests / determinism only; production callers
/// should pass `None` to get a fresh CSPRNG-derived value.
pub async fn start_flow(
    home: &Path,
    cfg: &Config,
    server_name: &str,
    state_override: Option<&str>,
) -> anyhow::Result<StartedFlow> {
    let server = cfg
        .tools
        .mcp_servers
        .get(server_name)
        .with_context(|| format!("unknown MCP server '{server_name}'"))?;
    if server.url.is_none() {
        bail!("MCP server '{server_name}' is not a remote server");
    }
    let oauth = match server.normalized_oauth() {
        Some(oauth) if oauth.enabled => oauth,
        Some(_) => bail!("MCP server '{server_name}' has OAuth disabled"),
        None => McpOAuthConfig::default(),
    };

    let client = zunel_util::default_reqwest_client(false);
    let metadata = discover_oauth_metadata(&client, server, &oauth).await?;
    let redirect_uri = redirect_uri(&oauth);
    let mut credentials = client_credentials(&oauth);
    if credentials.client_id.is_empty() {
        credentials = register_client(&client, &metadata, &redirect_uri)
            .await
            .context("registering OAuth client")?;
    }

    let state = match state_override {
        Some(s) => s.to_string(),
        None => generate_oauth_token(),
    };
    let verifier = generate_oauth_token();
    let challenge = pkce_challenge(&verifier);

    let authorize_url = build_authorization_url(
        &metadata,
        server,
        &oauth,
        &credentials.client_id,
        &redirect_uri,
        &state,
        &challenge,
    )?;

    let now = unix_timestamp_now();
    let pending = PendingFlow {
        state: state.clone(),
        verifier,
        redirect_uri: redirect_uri.clone(),
        client_id: credentials.client_id.clone(),
        client_secret: credentials.client_secret.clone(),
        token_url: metadata.token_endpoint.clone(),
        authorization_url: metadata.authorization_endpoint.clone(),
        scope_requested: oauth.scope.clone(),
        resource: server.url.clone(),
        expires_at: now.saturating_add(PENDING_TTL_SECS),
    };
    purge_expired_pending(home, server_name, now);
    save_pending(home, server_name, &pending)
        .with_context(|| format!("writing pending OAuth state for '{server_name}'"))?;

    Ok(StartedFlow {
        server: server_name.to_string(),
        authorize_url,
        redirect_uri,
        state,
        expires_in: PENDING_TTL_SECS,
    })
}

/// Finish a [`start_flow`] by exchanging the authorisation code for a
/// token and rewriting `token.json`. `callback_input` is either the full
/// pasted callback URL (whatever the IdP redirected the browser to) or
/// the raw query string (`?code=…&state=…`).
///
/// Strict on state mismatch: if the persisted `pending.state` doesn't
/// match what came back, the call returns an error and leaves the
/// pending file in place (the caller can retry the same authorize URL
/// — only the URL the user opens is bound to the verifier).
pub async fn complete_flow(
    home: &Path,
    cfg: &Config,
    server_name: &str,
    callback_input: &str,
) -> anyhow::Result<CompletedFlow> {
    if callback_input.len() > MAX_CALLBACK_URL_LEN {
        bail!(
            "callback URL is suspiciously long ({} bytes); refusing to parse",
            callback_input.len()
        );
    }
    let server = cfg
        .tools
        .mcp_servers
        .get(server_name)
        .with_context(|| format!("unknown MCP server '{server_name}'"))?;

    let now = unix_timestamp_now();
    purge_expired_pending(home, server_name, now);
    let pending = load_pending(home, server_name)
        .with_context(|| format!("reading pending OAuth state for '{server_name}'"))?
        .ok_or_else(|| {
            anyhow!(
                "no pending OAuth login for '{server_name}'; call start_flow first \
                 (or `zunel mcp login {server_name}`)"
            )
        })?;
    if pending.is_expired(now) {
        // We already purged above; this branch only fires when the
        // file just barely survived the purge but expired between
        // the read and now. Treat identically.
        delete_pending(home, server_name);
        bail!(
            "pending OAuth login for '{server_name}' has expired (>{} sec old); \
             restart the login",
            PENDING_TTL_SECS
        );
    }

    let code = authorization_code_from_callback(callback_input, &pending.state)?;

    let metadata_view = AuthorizationMetadataView {
        token_endpoint: pending.token_url.clone(),
        authorization_endpoint: pending.authorization_url.clone(),
    };
    let credentials = ClientCredentials {
        client_id: pending.client_id.clone(),
        client_secret: pending.client_secret.clone(),
    };
    let resource = pending.resource.clone().or_else(|| server.url.clone());

    let client = zunel_util::default_reqwest_client(false);
    let token = exchange_code_for_token(
        &client,
        &metadata_view,
        resource.as_deref(),
        &credentials,
        &pending.redirect_uri,
        &pending.verifier,
        &code,
    )
    .await
    .context("exchanging authorization code for token")?;

    save_token(home, server_name, &token)
        .with_context(|| format!("writing token cache for '{server_name}'"))?;
    delete_pending(home, server_name);

    Ok(CompletedFlow {
        server: server_name.to_string(),
        token_path: zunel_config::mcp_oauth::mcp_oauth_token_path(home, server_name),
        scopes: token.scope.clone(),
        expires_in: token.expires_in,
    })
}

// =============================================================================
// Discovery + endpoint helpers (lifted verbatim from the old CLI).
// =============================================================================

#[derive(Debug, Clone, Default, Deserialize)]
struct ResourceMetadata {
    #[serde(alias = "authorizationServers")]
    authorization_servers: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AuthorizationMetadata {
    #[serde(alias = "authorizationUrl")]
    authorization_endpoint: String,
    #[serde(alias = "tokenUrl")]
    token_endpoint: String,
    registration_endpoint: Option<String>,
}

/// Cut-down view used only on the token-exchange path: by the time
/// `complete_flow` runs we've already round-tripped through pending
/// storage, so we don't need (or have) the registration endpoint.
struct AuthorizationMetadataView {
    token_endpoint: String,
    authorization_endpoint: String,
}

#[derive(Debug, Clone, Default)]
struct ClientCredentials {
    client_id: String,
    client_secret: Option<String>,
}

async fn discover_oauth_metadata(
    client: &reqwest::Client,
    server: &McpServerConfig,
    oauth: &McpOAuthConfig,
) -> anyhow::Result<AuthorizationMetadata> {
    if let (Some(authorization_endpoint), Some(token_endpoint)) =
        (&oauth.authorization_url, &oauth.token_url)
    {
        return Ok(AuthorizationMetadata {
            authorization_endpoint: authorization_endpoint.clone(),
            token_endpoint: token_endpoint.clone(),
            registration_endpoint: None,
        });
    }

    if let Some(resource_metadata_url) = protected_resource_metadata_url(client, server).await? {
        let resp = client
            .get(&resource_metadata_url)
            .send()
            .await?
            .error_for_status()?;
        let resource_metadata: ResourceMetadata = oauth_capped_json(resp)
            .await
            .with_context(|| format!("reading resource metadata from {resource_metadata_url}"))?;
        if let Some(issuer) = resource_metadata.authorization_servers.first() {
            return fetch_authorization_metadata(client, issuer).await;
        }
    }

    let server_url = server
        .url
        .as_deref()
        .context("remote MCP server missing url")?;
    let url = reqwest::Url::parse(server_url).context("parsing MCP server URL")?;
    let issuer = format!(
        "{}://{}",
        url.scheme(),
        url.host_str().context("MCP server URL missing host")?
    );
    fetch_authorization_metadata(client, &issuer).await
}

async fn protected_resource_metadata_url(
    client: &reqwest::Client,
    server: &McpServerConfig,
) -> anyhow::Result<Option<String>> {
    let Some(server_url) = server.url.as_deref() else {
        return Ok(None);
    };
    let response = client.get(server_url).send().await?;
    let Some(value) = response.headers().get(WWW_AUTHENTICATE) else {
        return Ok(None);
    };
    let value = value.to_str().context("invalid WWW-Authenticate header")?;
    Ok(www_authenticate_param(value, "resource_metadata"))
}

async fn fetch_authorization_metadata(
    client: &reqwest::Client,
    issuer: &str,
) -> anyhow::Result<AuthorizationMetadata> {
    let metadata_url = authorization_metadata_url(issuer)?;
    let resp = client
        .get(metadata_url.as_str())
        .send()
        .await?
        .error_for_status()?;
    oauth_capped_json(resp)
        .await
        .with_context(|| format!("reading OAuth metadata from {metadata_url}"))
}

async fn register_client(
    client: &reqwest::Client,
    metadata: &AuthorizationMetadata,
    redirect_uri: &str,
) -> anyhow::Result<ClientCredentials> {
    let endpoint = metadata.registration_endpoint.as_deref().context(
        "OAuth metadata did not include a registration_endpoint and no clientId is configured",
    )?;
    let resp = client
        .post(endpoint)
        .json(&json!({
            "client_name": "zunel",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .await?
        .error_for_status()?;
    let response: Value = oauth_capped_json(resp).await?;
    let client_id = response
        .get("client_id")
        .and_then(Value::as_str)
        .context("dynamic client registration response missing client_id")?
        .to_string();
    let client_secret = response
        .get("client_secret")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Ok(ClientCredentials {
        client_id,
        client_secret,
    })
}

fn client_credentials(oauth: &McpOAuthConfig) -> ClientCredentials {
    ClientCredentials {
        client_id: oauth.client_id.clone().unwrap_or_default(),
        client_secret: oauth.client_secret.clone(),
    }
}

fn redirect_uri(oauth: &McpOAuthConfig) -> String {
    if let Some(uri) = &oauth.redirect_uri {
        return uri.clone();
    }
    let host = oauth
        .callback_host
        .clone()
        .unwrap_or_else(|| "127.0.0.1".into());
    let port = oauth.callback_port.unwrap_or(33419);
    format!("http://{host}:{port}/callback")
}

fn build_authorization_url(
    metadata: &AuthorizationMetadata,
    server: &McpServerConfig,
    oauth: &McpOAuthConfig,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> anyhow::Result<String> {
    let mut url =
        reqwest::Url::parse(&metadata.authorization_endpoint).context("parsing authorize URL")?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(scope) = &oauth.scope {
            query.append_pair("scope", scope);
        }
        if let Some(resource) = &server.url {
            query.append_pair("resource", resource);
        }
    }
    Ok(url.into())
}

async fn exchange_code_for_token(
    client: &reqwest::Client,
    metadata: &AuthorizationMetadataView,
    resource: Option<&str>,
    credentials: &ClientCredentials,
    redirect_uri: &str,
    verifier: &str,
    code: &str,
) -> anyhow::Result<CachedMcpOAuthToken> {
    let mut form = BTreeMap::from([
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", credentials.client_id.clone()),
        ("code_verifier", verifier.to_string()),
    ]);
    if let Some(secret) = &credentials.client_secret {
        form.insert("client_secret", secret.clone());
    }
    if let Some(resource) = resource {
        form.insert("resource", resource.to_string());
    }

    let resp = client
        .post(&metadata.token_endpoint)
        .form(&form)
        .send()
        .await?
        .error_for_status()?;
    let response: Value = oauth_capped_json(resp).await?;
    let access_token = response
        .get("access_token")
        .and_then(Value::as_str)
        .context("token response missing access_token")?
        .to_string();
    Ok(CachedMcpOAuthToken {
        access_token,
        token_type: response
            .get("token_type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        refresh_token: response
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        expires_in: response.get("expires_in").and_then(Value::as_u64),
        scope: response
            .get("scope")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        obtained_at: unix_timestamp_now(),
        client_id: credentials.client_id.clone(),
        client_secret: credentials.client_secret.clone(),
        authorization_url: metadata.authorization_endpoint.clone(),
        token_url: metadata.token_endpoint.clone(),
    })
}

fn authorization_code_from_callback(
    callback_url: &str,
    expected_state: &str,
) -> anyhow::Result<String> {
    let trimmed = callback_url.trim();
    if trimmed.is_empty() {
        bail!("callback URL is empty");
    }
    // Accept three shapes:
    //   1. Full URL:                 https://host/cb?code=…&state=…
    //   2. Path + query:             /cb?code=…&state=…
    //   3. Bare query string:        ?code=…&state=…  (or  code=…&state=…)
    // (3) is what users are most likely to paste from a Slack
    // message after copying just the query off the redirect page.
    let url = if let Ok(parsed) = reqwest::Url::parse(trimmed) {
        parsed
    } else {
        let synthesized = if trimmed.starts_with('/') {
            format!("https://placeholder.invalid{trimmed}")
        } else if trimmed.starts_with('?') {
            format!("https://placeholder.invalid/{trimmed}")
        } else {
            format!("https://placeholder.invalid/?{trimmed}")
        };
        reqwest::Url::parse(&synthesized).context("parsing callback URL")?
    };

    if let Some(error) = url
        .query_pairs()
        .find_map(|(name, value)| (name == "error").then(|| value.into_owned()))
    {
        bail!("OAuth callback returned error: {error}");
    }
    let state = url
        .query_pairs()
        .find_map(|(name, value)| (name == "state").then(|| value.into_owned()))
        .context("callback URL missing state")?;
    if state != expected_state {
        bail!("OAuth state mismatch");
    }
    url.query_pairs()
        .find_map(|(name, value)| (name == "code").then(|| value.into_owned()))
        .context("callback URL missing code")
}

fn www_authenticate_param(header: &str, name: &str) -> Option<String> {
    for part in header.split(',') {
        let (key, value) = part.trim().split_once('=')?;
        let key = key.trim().trim_start_matches("Bearer ").trim();
        if key == name {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn authorization_metadata_url(issuer: &str) -> anyhow::Result<reqwest::Url> {
    let issuer = reqwest::Url::parse(issuer).context("parsing OAuth issuer URL")?;
    let path = issuer.path().trim_start_matches('/');
    let mut url = issuer.clone();
    let metadata_path = if path.is_empty() {
        "/.well-known/oauth-authorization-server".to_string()
    } else {
        format!("/.well-known/oauth-authorization-server/{path}")
    };
    url.set_path(&metadata_path);
    url.set_query(None);
    Ok(url)
}

fn generate_oauth_token() -> String {
    let mut bytes = [0_u8; 32];
    // The system CSPRNG is the only acceptable source for OAuth state and
    // PKCE verifiers. Earlier revisions silently fell back to
    // `timestamp:pid` when `/dev/urandom` was unreadable, which trivially
    // defeated PKCE on platforms without that device (Windows).
    getrandom::fill(&mut bytes).expect("system CSPRNG must be available");
    base64_url(&bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64_url(&digest)
}

fn base64_url(input: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
}

// =============================================================================
// On-disk pending state I/O.
// =============================================================================

fn pending_path(home: &Path, server_name: &str) -> PathBuf {
    // Mirror `mcp_oauth_token_path`'s sanitisation (which is private)
    // by routing through the public path helper and replacing the
    // file component. Sanitisation rules match because the `<server>`
    // component is shared.
    let token_path = zunel_config::mcp_oauth::mcp_oauth_token_path(home, server_name);
    token_path.with_file_name("pending.json")
}

fn save_pending(home: &Path, server_name: &str, pending: &PendingFlow) -> std::io::Result<()> {
    let path = pending_path(home, server_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(pending)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn load_pending(home: &Path, server_name: &str) -> std::io::Result<Option<PendingFlow>> {
    let path = pending_path(home, server_name);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let pending: PendingFlow = serde_json::from_slice(&bytes)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    Ok(Some(pending))
}

fn delete_pending(home: &Path, server_name: &str) {
    let path = pending_path(home, server_name);
    let _ = std::fs::remove_file(path);
}

fn purge_expired_pending(home: &Path, server_name: &str, now_unix: u64) {
    let path = pending_path(home, server_name);
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(pending) = serde_json::from_slice::<PendingFlow>(&bytes) {
            if pending.is_expired(now_unix) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{body_string_contains, header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zunel_config::mcp_oauth::{load_token as load_cached, mcp_oauth_token_path};

    #[test]
    fn generate_oauth_token_returns_distinct_high_entropy_strings() {
        // 32 bytes base64url-encoded with no padding → 43 chars.
        let a = generate_oauth_token();
        let b = generate_oauth_token();
        assert_eq!(a.len(), 43, "unexpected verifier length: {a:?}");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "verifier must be url-safe base64 with no padding: {a:?}"
        );
        // Collision probability ~2^-256. If this trips, someone re-introduced
        // a predictable fallback.
        assert_ne!(a, b);
    }

    fn cached(token_url: &str) -> CachedMcpOAuthToken {
        CachedMcpOAuthToken {
            access_token: "old-access".into(),
            token_type: Some("Bearer".into()),
            refresh_token: Some("old-refresh".into()),
            expires_in: Some(3600),
            scope: Some("read".into()),
            obtained_at: 1_000,
            client_id: "client-id".into(),
            client_secret: None,
            authorization_url: "https://example.test/authorize".into(),
            token_url: token_url.into(),
        }
    }

    fn config_with_remote_oauth(url: &str, with_endpoints: bool) -> Config {
        let oauth = if with_endpoints {
            json!({
                "enabled": true,
                "clientId": "client-1",
                "authorizationUrl": format!("{url}/authorize"),
                "tokenUrl": format!("{url}/token"),
                "scope": "mcp",
                "redirectUri": "http://127.0.0.1:33419/callback"
            })
        } else {
            json!({"enabled": true})
        };
        let raw = json!({
            "providers": {},
            "agents": {"defaults": {"model": "m"}},
            "tools": {"mcpServers": {"remote": {
                "type": "streamableHttp",
                "url": format!("{url}/mcp"),
                "oauth": oauth
            }}}
        });
        serde_json::from_value(raw).expect("valid config")
    }

    #[tokio::test]
    async fn skips_refresh_when_token_is_fresh() {
        let dir = TempDir::new().unwrap();
        let token = cached("https://nope.example.test/token");
        save_token(dir.path(), "srv", &token).unwrap();

        let outcome = refresh_if_needed_with(
            dir.path(),
            "srv",
            60,
            token.obtained_at + 100,
            &reqwest::Client::new(),
        )
        .await;
        match outcome {
            Outcome::StillFresh { secs_remaining } => assert_eq!(secs_remaining, 3500),
            other => panic!("expected StillFresh, got {other:?}"),
        }

        let after = load_cached(dir.path(), "srv").unwrap().unwrap();
        assert_eq!(after.access_token, "old-access");
    }

    #[tokio::test]
    async fn refreshes_when_expired_and_persists_rotated_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new-access",
                "token_type": "Bearer",
                "refresh_token": "rotated-refresh",
                "expires_in": 7200,
                "scope": "read write"
            })))
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let token = cached(&format!("{}/token", server.uri()));
        save_token(dir.path(), "srv", &token).unwrap();

        let now = token.obtained_at + token.expires_in.unwrap() + 1;
        let outcome =
            refresh_if_needed_with(dir.path(), "srv", 60, now, &reqwest::Client::new()).await;
        match outcome {
            Outcome::Refreshed { new_expires_in } => assert_eq!(new_expires_in, Some(7200)),
            other => panic!("expected Refreshed, got {other:?}"),
        }

        let after = load_cached(dir.path(), "srv").unwrap().unwrap();
        assert_eq!(after.access_token, "new-access");
        assert_eq!(after.refresh_token.as_deref(), Some("rotated-refresh"));
        assert_eq!(after.expires_in, Some(7200));
        assert_eq!(after.obtained_at, now);
        assert_eq!(after.scope.as_deref(), Some("read write"));
    }

    #[tokio::test]
    async fn keeps_existing_refresh_token_when_response_omits_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new-access",
                "token_type": "Bearer",
                "expires_in": 1800
            })))
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let token = cached(&format!("{}/token", server.uri()));
        save_token(dir.path(), "srv", &token).unwrap();

        let now = token.obtained_at + token.expires_in.unwrap() + 1;
        let _ = refresh_if_needed_with(dir.path(), "srv", 60, now, &reqwest::Client::new()).await;

        let after = load_cached(dir.path(), "srv").unwrap().unwrap();
        assert_eq!(after.access_token, "new-access");
        assert_eq!(after.refresh_token.as_deref(), Some("old-refresh"));
    }

    #[tokio::test]
    async fn idp_400_leaves_cache_intact_and_reports_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "Refresh token is invalid"
            })))
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let token = cached(&format!("{}/token", server.uri()));
        save_token(dir.path(), "srv", &token).unwrap();

        let now = token.obtained_at + token.expires_in.unwrap() + 1;
        let outcome =
            refresh_if_needed_with(dir.path(), "srv", 60, now, &reqwest::Client::new()).await;
        let msg = match outcome {
            Outcome::RefreshFailed(msg) => msg,
            other => panic!("expected RefreshFailed, got {other:?}"),
        };
        assert!(msg.contains("HTTP 400"), "got: {msg}");
        assert!(msg.contains("invalid_grant"), "got: {msg}");

        let after = load_cached(dir.path(), "srv").unwrap().unwrap();
        assert_eq!(after.access_token, "old-access");
        assert_eq!(after.refresh_token.as_deref(), Some("old-refresh"));
    }

    #[tokio::test]
    async fn no_refresh_token_short_circuits() {
        let dir = TempDir::new().unwrap();
        let mut token = cached("https://example.test/token");
        token.refresh_token = None;
        save_token(dir.path(), "srv", &token).unwrap();

        let now = token.obtained_at + token.expires_in.unwrap() + 1;
        let outcome =
            refresh_if_needed_with(dir.path(), "srv", 60, now, &reqwest::Client::new()).await;
        assert!(matches!(outcome, Outcome::NoRefreshToken));
    }

    #[tokio::test]
    async fn no_cached_token_short_circuits() {
        let dir = TempDir::new().unwrap();
        let outcome = refresh_if_needed_with(
            dir.path(),
            "never-logged-in",
            60,
            42,
            &reqwest::Client::new(),
        )
        .await;
        assert!(matches!(outcome, Outcome::NotCached));
        assert!(!mcp_oauth_token_path(dir.path(), "never-logged-in").exists());
    }

    #[test]
    fn parses_full_callback_url_and_validates_state() {
        let code = authorization_code_from_callback(
            "http://127.0.0.1:33419/callback?code=abc&state=xyz",
            "xyz",
        )
        .unwrap();
        assert_eq!(code, "abc");
    }

    #[test]
    fn parses_path_only_callback() {
        let code = authorization_code_from_callback("/cb?code=abc&state=xyz", "xyz").unwrap();
        assert_eq!(code, "abc");
    }

    #[test]
    fn parses_bare_query_string_callback() {
        let code = authorization_code_from_callback("?code=abc&state=xyz", "xyz").unwrap();
        assert_eq!(code, "abc");
        let code = authorization_code_from_callback("code=abc&state=xyz", "xyz").unwrap();
        assert_eq!(code, "abc");
    }

    #[test]
    fn rejects_state_mismatch() {
        let err = authorization_code_from_callback(
            "http://127.0.0.1/cb?code=abc&state=other",
            "expected",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("state mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_idp_error_payload() {
        let err = authorization_code_from_callback(
            "http://127.0.0.1/cb?error=access_denied&state=s",
            "s",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("access_denied"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn start_flow_persists_pending_and_returns_authorize_url() {
        let auth = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        let cfg = config_with_remote_oauth(&auth.uri(), true);

        let started = start_flow(dir.path(), &cfg, "remote", Some("state-1"))
            .await
            .expect("start_flow");

        assert_eq!(started.state, "state-1");
        assert!(started
            .authorize_url
            .starts_with(&format!("{}/authorize", auth.uri())));
        assert!(started.authorize_url.contains("code_challenge"));
        assert!(started.authorize_url.contains("client_id=client-1"));
        assert_eq!(started.expires_in, PENDING_TTL_SECS);

        // Pending file is on disk and round-trips.
        let pending = load_pending(dir.path(), "remote")
            .expect("read pending")
            .expect("pending exists");
        assert_eq!(pending.state, "state-1");
        assert_eq!(pending.client_id, "client-1");
        assert_eq!(pending.token_url, format!("{}/token", auth.uri()));
        assert!(!pending.verifier.is_empty());
    }

    #[tokio::test]
    async fn complete_flow_writes_token_and_purges_pending() {
        let auth = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=abc"))
            .and(body_string_contains("client_id=client-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "atk",
                "refresh_token": "rtk",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "mcp"
            })))
            .mount(&auth)
            .await;

        let dir = TempDir::new().unwrap();
        let cfg = config_with_remote_oauth(&auth.uri(), true);
        let _ = start_flow(dir.path(), &cfg, "remote", Some("state-2"))
            .await
            .expect("start_flow");

        let completed = complete_flow(
            dir.path(),
            &cfg,
            "remote",
            "http://127.0.0.1:33419/callback?code=abc&state=state-2",
        )
        .await
        .expect("complete_flow");

        assert_eq!(completed.scopes.as_deref(), Some("mcp"));
        assert_eq!(completed.expires_in, Some(3600));
        assert!(completed.token_path.exists());

        let token = load_cached(dir.path(), "remote").unwrap().unwrap();
        assert_eq!(token.access_token, "atk");
        assert_eq!(token.refresh_token.as_deref(), Some("rtk"));
        assert_eq!(token.client_id, "client-1");

        // Pending file is deleted on success.
        assert!(load_pending(dir.path(), "remote").unwrap().is_none());
    }

    #[tokio::test]
    async fn complete_flow_rejects_state_mismatch_and_keeps_pending() {
        let auth = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        let cfg = config_with_remote_oauth(&auth.uri(), true);
        let _ = start_flow(dir.path(), &cfg, "remote", Some("good-state"))
            .await
            .expect("start_flow");

        let err = complete_flow(
            dir.path(),
            &cfg,
            "remote",
            "http://127.0.0.1/cb?code=abc&state=evil",
        )
        .await
        .expect_err("state mismatch must error");
        assert!(
            err.to_string().contains("state mismatch"),
            "unexpected error: {err}"
        );

        // Pending file is *retained* on state mismatch so the user can
        // retry the same authorize URL after correcting whatever went
        // wrong.
        assert!(load_pending(dir.path(), "remote").unwrap().is_some());
    }

    #[tokio::test]
    async fn complete_flow_purges_expired_pending_and_errors() {
        let auth = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        let cfg = config_with_remote_oauth(&auth.uri(), true);
        let _ = start_flow(dir.path(), &cfg, "remote", Some("s"))
            .await
            .expect("start_flow");

        // Forge an expiry by rewriting the pending file's expiresAt.
        let path = pending_path(dir.path(), "remote");
        let mut pending: PendingFlow =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        pending.expires_at = 1; // ancient
        std::fs::write(&path, serde_json::to_vec_pretty(&pending).unwrap()).unwrap();

        let err = complete_flow(dir.path(), &cfg, "remote", "?code=abc&state=s")
            .await
            .expect_err("expired pending must error");
        let msg = err.to_string();
        assert!(
            msg.contains("expired") || msg.contains("no pending"),
            "unexpected error: {msg}"
        );
        // And the file is gone.
        assert!(load_pending(dir.path(), "remote").unwrap().is_none());
    }

    #[tokio::test]
    async fn complete_flow_errors_when_no_pending_started() {
        let auth = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        let cfg = config_with_remote_oauth(&auth.uri(), true);

        let err = complete_flow(dir.path(), &cfg, "remote", "?code=abc&state=s")
            .await
            .expect_err("must require start_flow first");
        assert!(
            err.to_string().contains("no pending"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn refresh_all_oauth_servers_reports_per_server_outcomes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/token"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "fresh",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        // Cached token for `expired-srv` is stale → expect Refreshed.
        let mut expired = cached(&format!("{}/token", server.uri()));
        expired.obtained_at = 1;
        expired.expires_in = Some(60);
        save_token(dir.path(), "expired-srv", &expired).unwrap();
        // Cached token for `fresh-srv` is brand new → expect StillFresh.
        let mut fresh = cached(&format!("{}/token", server.uri()));
        fresh.obtained_at = unix_timestamp_now();
        save_token(dir.path(), "fresh-srv", &fresh).unwrap();
        // `disabled-srv` has OAuth disabled → must NOT be visited.

        let raw = json!({
            "providers": {},
            "agents": {"defaults": {"model": "m"}},
            "tools": {"mcpServers": {
                "expired-srv": {
                    "type": "streamableHttp",
                    "url": format!("{}/mcp", server.uri()),
                    "oauth": {"enabled": true}
                },
                "fresh-srv": {
                    "type": "streamableHttp",
                    "url": format!("{}/mcp", server.uri()),
                    "oauth": {"enabled": true}
                },
                "disabled-srv": {
                    "type": "streamableHttp",
                    "url": format!("{}/mcp", server.uri()),
                    "oauth": {"enabled": false}
                },
                "stdio-srv": {
                    "type": "stdio",
                    "command": "/bin/true",
                    "oauth": {"enabled": true}
                }
            }}
        });
        let cfg: Config = serde_json::from_value(raw).unwrap();

        let outcomes = refresh_all_oauth_servers(dir.path(), &cfg).await;
        let names: Vec<_> = outcomes.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"expired-srv"));
        assert!(names.contains(&"fresh-srv"));
        assert!(
            !names.contains(&"disabled-srv"),
            "OAuth-disabled servers must not be touched"
        );
        assert!(
            !names.contains(&"stdio-srv"),
            "stdio MCP servers must not be touched"
        );

        for (name, outcome) in &outcomes {
            match (name.as_str(), outcome) {
                ("expired-srv", Outcome::Refreshed { .. }) => {}
                ("fresh-srv", Outcome::StillFresh { .. }) => {}
                (other, outcome) => panic!("unexpected outcome for {other}: {outcome:?}"),
            }
        }
    }
}
