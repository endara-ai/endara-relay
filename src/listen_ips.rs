//! Eligibility rules for extra MCP listener bind addresses.
//!
//! `[relay] listen_ips` opts the relay into binding its MCP TCP listener on
//! additional local IPs beyond the always-bound `127.0.0.1`. Only
//! private-scope addresses may be listed: IPv4 RFC 1918 (`10.0.0.0/8`,
//! `172.16.0.0/12`, `192.168.0.0/16`), IPv4 CGNAT `100.64.0.0/10`
//! (Tailscale), and IPv6 unique-local `fc00::/7`. Wildcards (`0.0.0.0`,
//! `::`), public addresses, multicast, broadcast, and link-local addresses
//! are never bound. Entries that fail the check are skipped with a warning
//! instead of aborting startup — loopback remains the guaranteed listener.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tracing::warn;

/// Classification of one candidate `listen_ips` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenIpClass {
    /// Private-scope address the relay may bind: RFC 1918, CGNAT, or ULA.
    Eligible,
    /// Loopback — accepted, but redundant with the always-bound loopback
    /// listener.
    Loopback,
    /// Never bound; carries the reason for the startup warning.
    Ineligible(&'static str),
}

/// Classify an IP against the `listen_ips` eligibility rules.
pub fn classify_listen_ip(ip: IpAddr) -> ListenIpClass {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(ip: Ipv4Addr) -> ListenIpClass {
    if ip.is_loopback() {
        return ListenIpClass::Loopback;
    }
    if ip.is_unspecified() {
        return ListenIpClass::Ineligible(
            "unspecified address (0.0.0.0) would expose the relay on every interface",
        );
    }
    if ip.is_broadcast() {
        return ListenIpClass::Ineligible("broadcast address");
    }
    if ip.is_multicast() {
        return ListenIpClass::Ineligible("multicast address");
    }
    if ip.is_link_local() {
        return ListenIpClass::Ineligible("IPv4 link-local (169.254.0.0/16) is not supported");
    }
    if ip.is_private() || is_cgnat(ip) {
        return ListenIpClass::Eligible;
    }
    ListenIpClass::Ineligible("public IPv4 address")
}

fn classify_v6(ip: Ipv6Addr) -> ListenIpClass {
    // An IPv4-mapped address (`::ffff:a.b.c.d`) classifies as its embedded
    // IPv4 address, mirroring `local_network.rs`.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return classify_v4(v4);
    }
    if ip.is_loopback() {
        return ListenIpClass::Loopback;
    }
    if ip.is_unspecified() {
        return ListenIpClass::Ineligible(
            "unspecified address (::) would expose the relay on every interface",
        );
    }
    if ip.is_multicast() {
        return ListenIpClass::Ineligible("multicast address");
    }
    let first = ip.segments()[0];
    // Unique-local fc00::/7.
    if (first & 0xfe00) == 0xfc00 {
        return ListenIpClass::Eligible;
    }
    // Link-local fe80::/10.
    if (first & 0xffc0) == 0xfe80 {
        return ListenIpClass::Ineligible("IPv6 link-local (fe80::/10) is not supported");
    }
    ListenIpClass::Ineligible("public IPv6 address")
}

/// IPv4 CGNAT / shared address space `100.64.0.0/10` (RFC 6598), used by
/// Tailscale for its per-node addresses.
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0b1100_0000) == 64
}

/// Sub-classification of an [`ListenIpClass::Eligible`] address for the
/// management API's `GET /api/network-interfaces` payload: `"private"`
/// (RFC 1918), `"cgnat"` (100.64.0.0/10), or `"ula"` (fc00::/7). Returns
/// `None` for anything `classify_listen_ip` does not deem eligible
/// (loopback, unspecified, link-local, public, ...), so those addresses can
/// never surface through the API.
pub fn eligible_ip_kind(ip: IpAddr) -> Option<&'static str> {
    if classify_listen_ip(ip) != ListenIpClass::Eligible {
        return None;
    }
    // An IPv4-mapped IPv6 address takes its embedded IPv4 kind, mirroring
    // `classify_v6`.
    let effective = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    Some(match effective {
        IpAddr::V4(v4) if is_cgnat(v4) => "cgnat",
        IpAddr::V4(_) => "private",
        IpAddr::V6(_) => "ula",
    })
}

/// Resolve `[relay] listen_ips` into the extra socket addresses to bind
/// alongside the always-bound `127.0.0.1:port` listener.
///
/// Unparseable or ineligible entries are skipped with a `warn!` rather than
/// aborting startup. Loopback entries are accepted (a non-default loopback
/// address like `::1` is bound), but the default `127.0.0.1` itself and
/// duplicate entries are dropped so no address is ever bound twice.
pub fn resolve_extra_listen_addrs(listen_ips: Option<&[String]>, port: u16) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for entry in listen_ips.unwrap_or_default() {
        let Ok(ip) = entry.trim().parse::<IpAddr>() else {
            warn!(entry = %entry, "Ignoring unparseable [relay] listen_ips entry");
            continue;
        };
        match classify_listen_ip(ip) {
            ListenIpClass::Ineligible(reason) => {
                warn!(
                    entry = %entry,
                    reason,
                    "Ignoring ineligible [relay] listen_ips entry; only RFC 1918, CGNAT (100.64.0.0/10), and IPv6 ULA (fc00::/7) addresses may be bound"
                );
                continue;
            }
            ListenIpClass::Loopback if ip == IpAddr::V4(Ipv4Addr::LOCALHOST) => continue,
            ListenIpClass::Loopback | ListenIpClass::Eligible => {}
        }
        let addr = SocketAddr::new(ip, port);
        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }
    addrs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(s: &str) -> ListenIpClass {
        classify_listen_ip(s.parse().unwrap())
    }

    fn is_eligible(s: &str) -> bool {
        classify(s) == ListenIpClass::Eligible
    }

    fn is_ineligible(s: &str) -> bool {
        matches!(classify(s), ListenIpClass::Ineligible(_))
    }

    #[test]
    fn rfc1918_private_is_eligible() {
        assert!(is_eligible("10.0.0.1"));
        assert!(is_eligible("10.255.255.254"));
        assert!(is_eligible("172.16.0.1"));
        assert!(is_eligible("172.31.255.254"));
        assert!(is_eligible("192.168.0.1"));
        assert!(is_eligible("192.168.255.254"));
    }

    #[test]
    fn cgnat_is_eligible_with_exact_bounds() {
        assert!(is_eligible("100.64.0.0"));
        assert!(is_eligible("100.101.102.103"));
        assert!(is_eligible("100.127.255.255"));
        // Just outside 100.64.0.0/10 is public.
        assert!(is_ineligible("100.63.255.255"));
        assert!(is_ineligible("100.128.0.0"));
    }

    #[test]
    fn ipv6_ula_is_eligible() {
        assert!(is_eligible("fc00::1"));
        assert!(is_eligible("fd12:3456:789a::1"));
        assert!(is_eligible("fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"));
    }

    #[test]
    fn public_addresses_are_ineligible() {
        assert!(is_ineligible("8.8.8.8"));
        assert!(is_ineligible("172.32.0.1"));
        assert!(is_ineligible("2001:db8::1"));
        assert!(is_ineligible("2606:4700::1111"));
    }

    #[test]
    fn unspecified_is_ineligible() {
        assert!(is_ineligible("0.0.0.0"));
        assert!(is_ineligible("::"));
    }

    #[test]
    fn multicast_and_broadcast_are_ineligible() {
        assert!(is_ineligible("224.0.0.1"));
        assert!(is_ineligible("255.255.255.255"));
        assert!(is_ineligible("ff02::1"));
    }

    #[test]
    fn link_local_is_ineligible() {
        assert!(is_ineligible("169.254.1.1"));
        assert!(is_ineligible("fe80::1"));
        assert!(is_ineligible("febf::1"));
    }

    #[test]
    fn loopback_classifies_as_loopback() {
        assert_eq!(classify("127.0.0.1"), ListenIpClass::Loopback);
        assert_eq!(classify("127.0.0.2"), ListenIpClass::Loopback);
        assert_eq!(classify("::1"), ListenIpClass::Loopback);
    }

    #[test]
    fn ipv4_mapped_ipv6_classifies_as_embedded_ipv4() {
        assert!(is_eligible("::ffff:192.168.1.10"));
        assert!(is_eligible("::ffff:100.64.0.1"));
        assert!(is_ineligible("::ffff:8.8.8.8"));
        assert_eq!(classify("::ffff:127.0.0.1"), ListenIpClass::Loopback);
    }

    fn resolve(entries: &[&str], port: u16) -> Vec<SocketAddr> {
        let owned: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        resolve_extra_listen_addrs(Some(&owned), port)
    }

    #[test]
    fn omitted_or_empty_resolves_to_no_extras() {
        assert!(resolve_extra_listen_addrs(None, 9400).is_empty());
        assert!(resolve(&[], 9400).is_empty());
    }

    #[test]
    fn eligible_entries_bind_on_the_given_port() {
        assert_eq!(
            resolve(&["100.101.102.103", "192.168.1.5"], 9400),
            vec![
                "100.101.102.103:9400".parse().unwrap(),
                "192.168.1.5:9400".parse().unwrap()
            ]
        );
    }

    #[test]
    fn ineligible_and_unparseable_entries_are_skipped() {
        assert_eq!(
            resolve(&["8.8.8.8", "0.0.0.0", "::", "not-an-ip", "10.1.2.3"], 9400),
            vec!["10.1.2.3:9400".parse().unwrap()]
        );
    }

    #[test]
    fn default_loopback_entry_is_redundant_and_dropped() {
        assert!(resolve(&["127.0.0.1"], 9400).is_empty());
    }

    #[test]
    fn non_default_loopback_entry_is_accepted() {
        assert_eq!(resolve(&["::1"], 9400), vec!["[::1]:9400".parse().unwrap()]);
    }

    #[test]
    fn duplicates_are_deduped_and_whitespace_trimmed() {
        assert_eq!(
            resolve(&[" 10.0.0.7 ", "10.0.0.7"], 9400),
            vec!["10.0.0.7:9400".parse().unwrap()]
        );
    }

    fn kind(s: &str) -> Option<&'static str> {
        eligible_ip_kind(s.parse().unwrap())
    }

    #[test]
    fn eligible_ip_kind_subclassifies_eligible_ranges() {
        assert_eq!(kind("10.0.0.1"), Some("private"));
        assert_eq!(kind("172.16.0.1"), Some("private"));
        assert_eq!(kind("192.168.1.5"), Some("private"));
        assert_eq!(kind("100.101.102.103"), Some("cgnat"));
        assert_eq!(kind("fd12:3456:789a::1"), Some("ula"));
        // IPv4-mapped IPv6 takes its embedded IPv4 kind.
        assert_eq!(kind("::ffff:192.168.1.10"), Some("private"));
        assert_eq!(kind("::ffff:100.64.0.1"), Some("cgnat"));
    }

    #[test]
    fn eligible_ip_kind_is_none_for_everything_else() {
        for s in [
            "127.0.0.1",
            "::1",
            "0.0.0.0",
            "::",
            "169.254.1.1",
            "fe80::1",
            "8.8.8.8",
            "2001:db8::1",
            "224.0.0.1",
            "255.255.255.255",
            "::ffff:8.8.8.8",
        ] {
            assert_eq!(kind(s), None, "expected no kind for {s}");
        }
    }
}
