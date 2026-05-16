//! Tool-side SSRF gate.
//!
//! Wraps `validate_url_target` — the syntactic check the agent's web
//! tools run before opening a connection — and re-exports the workspace
//! SSRF resolver from [`zunel_util::net`] so existing imports
//! (`zunel_tools::ssrf::SsrfSafeResolver`, `filter_blocked_addrs`)
//! continue to resolve. The DNS-time check lives in `zunel-util` so
//! every other reqwest client in the workspace can wire it in without
//! depending on `zunel-tools`.

use std::net::IpAddr;

use url::Url;

use crate::error::{Error, Result};

pub use zunel_util::{filter_blocked_addrs, is_blocked_ip, SsrfSafeResolver};

/// Validate that a URL is safe to fetch. Mirrors
/// `zunel/security/network.py::validate_url_target`.
///
/// `tool` is captured into the resulting error so a single shared validator
/// reports a useful provenance ("web_fetch", "web_search", …) on the
/// diagnostic line.
///
/// This is the fast, syntactic gate. The accompanying [`SsrfSafeResolver`]
/// closes the DNS-rebinding hole: a hostname that resolves to a private
/// or loopback address still gets rejected at connection time even when
/// the URL itself contains no IP literal.
pub fn validate_url_target(url: &str, allow_loopback: bool, tool: &str) -> Result<Url> {
    let parsed = Url::parse(url).map_err(|e| Error::InvalidArgs {
        tool: tool.to_string(),
        message: format!("invalid url: {e}"),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::PolicyViolation {
            tool: tool.to_string(),
            reason: format!("scheme must be http or https, got {}", parsed.scheme()),
        });
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::InvalidArgs {
            tool: tool.to_string(),
            message: "url missing host".to_string(),
        })?
        .to_string();
    if !allow_loopback {
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked_ip(&ip) {
                return Err(Error::SsrfBlocked {
                    tool: tool.to_string(),
                    url: url.to_string(),
                    reason: format!("blocked ip: {ip}"),
                });
            }
        } else if host.eq_ignore_ascii_case("localhost") {
            return Err(Error::SsrfBlocked {
                tool: tool.to_string(),
                url: url.to_string(),
                reason: "localhost".to_string(),
            });
        }
    }
    Ok(parsed)
}
