//! SSRF allow-list and DNS-rebinding guard for OAuth-related HTTP requests.
//!
//! OAuth discovery / DCR fetches URLs that originate from configuration or
//! from server-supplied metadata (e.g. `authorization_servers` in RFC 9728).
//! Both vectors are partially attacker-controlled, so any HTTP client used
//! for OAuth must:
//!
//! 1. Reject `http://` schemes by default (HTTPS only).
//! 2. Reject loopback / link-local / multicast / unspecified destinations
//!    by default to block SSRF against the host running the relay.
//! 3. Pin the resolved address set so that a later DNS lookup cannot
//!    redirect the connection to a different IP (DNS rebinding).
//!
//! `allow_insecure_oauth` (config) opts back into HTTP and loopback /
//! link-local addresses for development and integration testing.
//! Unspecified, multicast and broadcast addresses remain rejected
//! unconditionally.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use reqwest::Client;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum UrlGuardError {
    #[error("OAuth URL is not absolute or has no host: {url}")]
    InvalidUrl { url: String },

    #[error("OAuth URL scheme '{scheme}' is not allowed (set relay.allow_insecure_oauth=true to permit http://): {url}")]
    SchemeNotAllowed { scheme: String, url: String },

    #[error("OAuth URL resolves to disallowed address {addr} for host '{host}' (set relay.allow_insecure_oauth=true to permit loopback/link-local during development)")]
    AddressNotAllowed { addr: IpAddr, host: String },

    #[error("Failed to resolve OAuth URL host '{host}': {source}")]
    ResolveFailed {
        host: String,
        #[source]
        source: std::io::Error,
    },

    #[error("DNS resolution returned no addresses for host '{host}'")]
    NoAddresses { host: String },

    #[error("Failed to build HTTP client: {0}")]
    Client(#[source] reqwest::Error),
}

/// Parsed + validated OAuth URL with its pinned address set.
#[derive(Debug)]
pub struct PinnedTarget {
    pub host: String,
    pub addrs: Vec<SocketAddr>,
}

/// True if `ip` is an SSRF-sensitive address that should be rejected unless
/// the operator explicitly opts in via `allow_insecure_oauth`.
fn is_loopback_or_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            // IPv6 link-local prefix fe80::/10 — `Ipv6Addr::is_unicast_link_local`
            // is unstable, so test the prefix manually.
            let segs = v6.segments();
            (segs[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Always-reject categories regardless of `allow_insecure_oauth`.
fn is_always_blocked(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast(),
    }
}

/// Validate `url` and resolve its host, returning the pinned target.
pub async fn validate_and_resolve(
    url: &str,
    allow_insecure: bool,
) -> Result<PinnedTarget, UrlGuardError> {
    let parsed = Url::parse(url).map_err(|_| UrlGuardError::InvalidUrl {
        url: url.to_string(),
    })?;
    let scheme = parsed.scheme();
    if scheme != "https" && !(allow_insecure && scheme == "http") {
        return Err(UrlGuardError::SchemeNotAllowed {
            scheme: scheme.to_string(),
            url: url.to_string(),
        });
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| UrlGuardError::InvalidUrl {
            url: url.to_string(),
        })?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| UrlGuardError::InvalidUrl {
            url: url.to_string(),
        })?;

    let lookups: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| UrlGuardError::ResolveFailed {
            host: host.clone(),
            source: e,
        })?
        .collect();

    if lookups.is_empty() {
        return Err(UrlGuardError::NoAddresses { host: host.clone() });
    }

    for sa in &lookups {
        let ip = sa.ip();
        if is_always_blocked(&ip) {
            return Err(UrlGuardError::AddressNotAllowed {
                addr: ip,
                host: host.clone(),
            });
        }
        if !allow_insecure && is_loopback_or_link_local(&ip) {
            return Err(UrlGuardError::AddressNotAllowed {
                addr: ip,
                host: host.clone(),
            });
        }
    }

    Ok(PinnedTarget {
        host,
        addrs: lookups,
    })
}

/// Build an HTTP client whose DNS resolution for `target.host` is pinned to
/// the addresses validated above. A new client is built per target so the
/// pin cannot leak across calls to different hosts.
pub fn build_pinned_client(target: &PinnedTarget) -> Result<Client, UrlGuardError> {
    Client::builder()
        .resolve_to_addrs(&target.host, &target.addrs)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(UrlGuardError::Client)
}

/// Convenience: validate `url` and build the pinned client in one call.
pub async fn validated_client(url: &str, allow_insecure: bool) -> Result<Client, UrlGuardError> {
    let target = validate_and_resolve(url, allow_insecure).await?;
    build_pinned_client(&target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn ipv4_loopback_and_link_local_classified() {
        assert!(is_loopback_or_link_local(&IpAddr::V4(Ipv4Addr::new(
            127, 0, 0, 1
        ))));
        assert!(is_loopback_or_link_local(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 1, 2
        ))));
        assert!(!is_loopback_or_link_local(&IpAddr::V4(Ipv4Addr::new(
            192, 168, 1, 1
        ))));
        assert!(!is_loopback_or_link_local(&IpAddr::V4(Ipv4Addr::new(
            8, 8, 8, 8
        ))));
    }

    #[test]
    fn ipv6_loopback_and_link_local_classified() {
        assert!(is_loopback_or_link_local(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_loopback_or_link_local(&IpAddr::V6(
            "fe80::1".parse().unwrap()
        )));
        assert!(!is_loopback_or_link_local(&IpAddr::V6(
            "2606:4700::1".parse().unwrap()
        )));
    }

    #[test]
    fn always_blocked_categories() {
        assert!(is_always_blocked(&IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(is_always_blocked(&IpAddr::V4(Ipv4Addr::BROADCAST)));
        assert!(is_always_blocked(&IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))));
        assert!(is_always_blocked(&IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
        assert!(is_always_blocked(&IpAddr::V6("ff02::1".parse().unwrap())));
        assert!(!is_always_blocked(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
    }

    #[tokio::test]
    async fn rejects_http_scheme_when_secure() {
        let err = validate_and_resolve("http://example.com/x", false)
            .await
            .unwrap_err();
        assert!(matches!(err, UrlGuardError::SchemeNotAllowed { .. }));
    }

    #[tokio::test]
    async fn rejects_loopback_when_secure() {
        let err = validate_and_resolve("https://127.0.0.1/x", false)
            .await
            .unwrap_err();
        assert!(matches!(err, UrlGuardError::AddressNotAllowed { .. }));
    }

    #[tokio::test]
    async fn allows_loopback_http_when_insecure() {
        let target = validate_and_resolve("http://127.0.0.1:8080/x", true)
            .await
            .expect("should accept loopback under insecure mode");
        assert_eq!(target.host, "127.0.0.1");
        assert!(target.addrs.iter().any(|a| a.port() == 8080));
    }

    #[tokio::test]
    async fn rejects_unspecified_even_when_insecure() {
        let err = validate_and_resolve("http://0.0.0.0:80/x", true)
            .await
            .unwrap_err();
        assert!(matches!(err, UrlGuardError::AddressNotAllowed { .. }));
    }

    #[tokio::test]
    async fn rejects_invalid_url() {
        let err = validate_and_resolve("not a url", false).await.unwrap_err();
        assert!(matches!(err, UrlGuardError::InvalidUrl { .. }));
    }
}
