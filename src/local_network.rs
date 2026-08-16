//! macOS Local Network permission hint for LAN connect failures.
//!
//! On macOS, outbound connections to devices on the local network are gated
//! by the per-app Local Network privacy permission (TCC). When the user has
//! not granted it, connect attempts to private/LAN addresses fail with
//! opaque OS errors (typically "No route to host"). This module classifies
//! whether a connect target is a private/LAN host and, on macOS, appends an
//! actionable hint to transport-level connect error messages.

use std::net::{Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

/// Hint appended to connect-failure messages for private/LAN targets on macOS.
pub const LOCAL_NETWORK_HINT: &str = "this server is on your local network, so macOS may be \
    blocking the connection — check System Settings → Privacy & Security → Local Network and \
    allow the Endara app (or the terminal that launched the relay), then restart it after \
    granting";

/// Returns true when `host` is a private/LAN target: an RFC 1918 or
/// link-local IPv4 address, a unique-local or link-local IPv6 address, or an
/// mDNS `.local` hostname.
///
/// Loopback targets (`localhost`, `127.0.0.0/8`, `::1`) return false: the
/// macOS Local Network permission does not gate loopback traffic, so the
/// hint would be misleading.
pub fn is_private_or_local_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => is_mdns_local_domain(domain),
        Host::Ipv4(ip) => is_private_ipv4(*ip),
        Host::Ipv6(ip) => is_private_ipv6(*ip),
    }
}

/// RFC 1918 private ranges plus IPv4 link-local (`169.254.0.0/16`).
fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local()
}

/// IPv6 unique-local (`fc00::/7`) plus link-local (`fe80::/10`). An
/// IPv4-mapped address (`::ffff:a.b.c.d`) classifies as its embedded IPv4
/// address.
fn is_private_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_private_ipv4(v4);
    }
    let first = ip.segments()[0];
    (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
}

/// mDNS `.local` hostname (case-insensitive, tolerating a trailing dot).
fn is_mdns_local_domain(domain: &str) -> bool {
    let trimmed = domain.strip_suffix('.').unwrap_or(domain);
    trimmed
        .rsplit_once('.')
        .is_some_and(|(_, tld)| tld.eq_ignore_ascii_case("local"))
}

/// Appends [`LOCAL_NETWORK_HINT`] to `message` when running on macOS and
/// `url` targets a private/LAN host; otherwise returns `message` unchanged.
///
/// Messages that clearly indicate ECONNREFUSED are excluded: a refused
/// connection means the host was reachable, so the Local Network permission
/// is not the cause (TCC denial surfaces as "No route to host" / os error 65).
pub fn with_local_network_hint(url: &str, message: String) -> String {
    with_local_network_hint_inner(url, message, cfg!(target_os = "macos"))
}

/// Testable core of [`with_local_network_hint`] with the platform gate as a
/// parameter.
fn with_local_network_hint_inner(url: &str, message: String, is_macos: bool) -> String {
    if is_macos && url_targets_private_host(url) && !is_connection_refused(&message) {
        format!("{message} ({LOCAL_NETWORK_HINT})")
    } else {
        message
    }
}

/// True when the error text clearly names ECONNREFUSED.
fn is_connection_refused(message: &str) -> bool {
    message.to_ascii_lowercase().contains("connection refused")
}

/// True when `url` parses and its host classifies as private/LAN.
fn url_targets_private_host(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .as_ref()
        .and_then(Url::host)
        .is_some_and(|host| is_private_or_local_host(&host))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(url: &str) -> bool {
        url_targets_private_host(url)
    }

    #[test]
    fn rfc1918_and_link_local_ipv4_are_private() {
        assert!(host("http://10.0.0.5:8080/mcp"));
        assert!(host("http://172.16.0.1/sse"));
        assert!(host("http://172.31.255.254/"));
        assert!(host("http://192.168.1.10:8123/mcp_server/sse"));
        assert!(host("http://169.254.10.20/"));
    }

    #[test]
    fn public_and_non_private_ipv4_are_not_private() {
        assert!(!host("http://8.8.8.8/"));
        assert!(!host("http://172.15.0.1/"));
        assert!(!host("http://172.32.0.1/"));
        assert!(!host("http://1.2.3.4:9000/mcp"));
    }

    #[test]
    fn loopback_is_not_private() {
        assert!(!host("http://127.0.0.1:8000/mcp"));
        assert!(!host("http://localhost:8000/mcp"));
        assert!(!host("http://[::1]:8000/mcp"));
    }

    #[test]
    fn private_ipv6_ranges_are_private() {
        assert!(host("http://[fc00::1]/"));
        assert!(host("http://[fd12:3456:789a::1]:8080/mcp"));
        assert!(host("http://[fe80::1]/"));
    }

    #[test]
    fn public_ipv6_is_not_private() {
        assert!(!host("http://[2001:db8::1]/"));
        assert!(!host("http://[2606:4700::1111]/"));
    }

    #[test]
    fn ipv4_mapped_ipv6_classifies_as_embedded_ipv4() {
        assert!(host("http://[::ffff:192.168.1.10]:8123/"));
        assert!(host("http://[::ffff:10.0.0.5]/"));
        assert!(!host("http://[::ffff:8.8.8.8]/"));
        assert!(!host("http://[::ffff:127.0.0.1]/"));
    }

    #[test]
    fn dot_local_hostnames_are_private() {
        assert!(host("http://homeassistant.local:8123/mcp_server/sse"));
        assert!(host("http://Printer.LOCAL/"));
        assert!(host("http://hub.local./"));
    }

    #[test]
    fn other_domains_are_not_private() {
        assert!(!host("https://example.com/mcp"));
        assert!(!host("https://api.example.local.com/"));
        assert!(!host("http://local/"));
    }

    #[test]
    fn hint_suppressed_for_connection_refused() {
        let refused =
            "http://192.168.1.10:8123/: tcp connect error: Connection refused (os error 61)"
                .to_string();
        assert_eq!(
            with_local_network_hint_inner("http://192.168.1.10:8123/", refused.clone(), true),
            refused
        );

        let unreachable =
            "http://192.168.1.10:8123/: tcp connect error: No route to host (os error 65)"
                .to_string();
        let hinted = with_local_network_hint_inner("http://192.168.1.10:8123/", unreachable, true);
        assert!(hinted.contains("Privacy & Security → Local Network"));
    }

    #[test]
    fn hint_appended_only_on_macos_and_private_target() {
        let msg = || "connection failed".to_string();
        let hinted = with_local_network_hint_inner("http://192.168.1.10:8123/", msg(), true);
        assert!(hinted.starts_with("connection failed ("));
        assert!(hinted.contains("Privacy & Security → Local Network"));

        assert_eq!(
            with_local_network_hint_inner("https://example.com/", msg(), true),
            "connection failed"
        );
        assert_eq!(
            with_local_network_hint_inner("http://192.168.1.10:8123/", msg(), false),
            "connection failed"
        );
        assert_eq!(
            with_local_network_hint_inner("not a url", msg(), true),
            "connection failed"
        );
    }
}
