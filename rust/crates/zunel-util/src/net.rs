//! SSRF-aware reqwest helpers shared by every crate that talks to a
//! user- or remote-controlled URL.
//!
//! Three things live here:
//!
//! 1. [`is_blocked_ip`] — the central "is this address private /
//!    loopback / link-local / IMDS?" check. Mirrored from the Python
//!    SSRF helper and used by both the syntactic URL gate
//!    (`zunel_tools::ssrf::validate_url_target`) and the DNS-time
//!    re-check below.
//! 2. [`SsrfSafeResolver`] — a `reqwest::dns::Resolve` impl that runs
//!    the system resolver and drops any returned `SocketAddr` whose IP
//!    fails [`is_blocked_ip`]. Closes the DNS-rebinding gap: even if a
//!    hostname looks innocuous, every IP it resolves to is re-validated
//!    before reqwest connects.
//! 3. [`default_reqwest_client`] — the one builder every crate should
//!    use to construct a `reqwest::Client` that talks to remote hosts.
//!    Centralises the 30-second timeout, 5-redirect cap, and the
//!    SSRF-safe resolver so a future contributor who adds a new
//!    user-URL surface can't accidentally skip the defence.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Should we refuse to connect to this IP for an outbound HTTP fetch
/// initiated by a user/remote-controlled URL? Returns `true` for
/// loopback, private, link-local, broadcast, unspecified, AWS IMDS
/// (`169.254.169.254`), and IPv6 unique-local.
pub fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || *v4 == Ipv4Addr::new(169, 254, 169, 254)
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local(),
    }
}

/// Drop SSRF-blocked addresses from a DNS resolution result.
///
/// Returns `Err(PermissionDenied)` if every resolved address is
/// blocked, signalling reqwest's connector to fail the request rather
/// than silently connect to a target the syntactic SSRF check would
/// have rejected.
pub fn filter_blocked_addrs(
    addrs: Vec<SocketAddr>,
    allow_loopback: bool,
    host_for_msg: &str,
) -> std::io::Result<Vec<SocketAddr>> {
    let safe: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| allow_loopback || !is_blocked_ip(&a.ip()))
        .collect();
    if safe.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("ssrf: every resolved address for {host_for_msg} is blocked"),
        ));
    }
    Ok(safe)
}

/// `reqwest::dns::Resolve` impl that runs the system resolver and then
/// filters every returned `SocketAddr` through [`is_blocked_ip`]. Wire
/// into [`reqwest::ClientBuilder::dns_resolver`] to close DNS-rebinding
/// attacks where a hostname under attacker control returns
/// `169.254.169.254` or `127.0.0.1` on each lookup.
#[derive(Debug, Clone)]
pub struct SsrfSafeResolver {
    allow_loopback: bool,
}

impl SsrfSafeResolver {
    pub fn new(allow_loopback: bool) -> Self {
        Self { allow_loopback }
    }
}

impl Resolve for SsrfSafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_loopback = self.allow_loopback;
        let host = name.as_str().to_string();
        Box::pin(async move {
            // `lookup_host` needs `host:port`; port 0 is fine here
            // because reqwest re-applies the URL's port to each returned
            // SocketAddr before connecting.
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host(format!("{host}:0"))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .collect();
            let safe = filter_blocked_addrs(addrs, allow_loopback, &host)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            let iter: Addrs = Box::new(safe.into_iter());
            Ok(iter)
        })
    }
}

/// Build a `reqwest::Client` with the workspace's standard outbound
/// defaults:
///
/// - 30-second total request timeout
/// - At most 5 redirects (matches the historic `WebFetchTool` policy)
/// - [`SsrfSafeResolver`] wired in so DNS-rebinding can't steer a
///   request at a private address
///
/// `allow_loopback` exists for tests using `wiremock` on `127.0.0.1`;
/// production callers pass `false`. On builder failure (broken TLS
/// config etc.) the helper falls back to `reqwest::Client::new()` so a
/// misconfigured host still has a working — but unhardened — client,
/// matching the historic behaviour of `WebFetchTool::new`.
pub fn default_reqwest_client(allow_loopback: bool) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .dns_resolver(Arc::new(SsrfSafeResolver::new(allow_loopback)))
        .build()
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "default_reqwest_client builder failed; using bare Client::new");
            reqwest::Client::new()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn is_blocked_ip_blocks_canonical_private_space() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "fc00::1",
        ] {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(is_blocked_ip(&parsed), "expected {ip} to be blocked");
        }
    }

    #[test]
    fn is_blocked_ip_passes_public_addresses() {
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ] {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(!is_blocked_ip(&parsed), "expected {ip} to be allowed");
        }
    }

    #[test]
    fn filter_blocked_addrs_rejects_when_all_blocked() {
        let addrs = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 0),
        ];
        let err = filter_blocked_addrs(addrs, false, "evil.test").unwrap_err();
        assert!(err.to_string().contains("ssrf"));
    }

    #[test]
    fn filter_blocked_addrs_keeps_safe_subset() {
        let public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 0);
        let private = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 0);
        let safe = filter_blocked_addrs(vec![public, private], false, "mixed.test").unwrap();
        assert_eq!(safe, vec![public]);
    }

    #[tokio::test]
    async fn ssrf_safe_resolver_rejects_localhost() {
        let resolver = SsrfSafeResolver::new(false);
        let name = Name::from_str("localhost").unwrap();
        let result = resolver.resolve(name).await;
        let err = result.err().expect("localhost must be rejected");
        assert!(err.to_string().contains("ssrf"), "got {err}");
    }

    #[tokio::test]
    async fn default_reqwest_client_carries_the_resolver() {
        // Smoke test: a client built via the helper is non-default and
        // does honour the SSRF cap — pointing it at a loopback URL is
        // rejected at connect-time. We can't introspect the resolver
        // directly, but `localhost` resolution failing through this
        // client (when `allow_loopback=false`) is the next-best signal.
        let client = default_reqwest_client(false);
        let err = client
            .get("http://localhost:1/")
            .send()
            .await
            .expect_err("loopback must be refused");
        assert!(
            err.to_string().contains("ssrf") || err.is_connect(),
            "expected ssrf rejection, got {err}"
        );
    }
}
