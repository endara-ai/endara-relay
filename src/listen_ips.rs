//! Eligibility rules for extra MCP listener bind addresses.
//!
//! `[relay] listen_ips` opts the relay into binding its MCP TCP listener on
//! additional local IPs beyond the always-bound `127.0.0.1`. Only
//! private-scope addresses may be listed: IPv4 RFC 1918 (`10.0.0.0/8`,
//! `172.16.0.0/12`, `192.168.0.0/16`), IPv4 CGNAT `100.64.0.0/10`
//! (Tailscale), and IPv6 unique-local `fc00::/7`. Wildcards (`0.0.0.0`,
//! `::`), public addresses, multicast, broadcast, link-local, and loopback
//! addresses are never bound (loopback is redundant with the always-bound
//! listener). Entries that fail the check are skipped with a warning instead
//! of aborting startup — loopback remains the guaranteed listener.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tracing::warn;

/// Classification of one candidate `listen_ips` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenIpClass {
    /// Private-scope address the relay may bind: RFC 1918, CGNAT, or ULA.
    Eligible,
    /// Loopback — never bound as an extra listener: redundant with the
    /// always-bound `127.0.0.1` and outside the private/CGNAT/ULA allowlist.
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

/// Shared acceptance core for `[relay] listen_ips`: trim + parse each entry,
/// drop unparseable, ineligible, and loopback entries (loopback is redundant
/// with the always-bound `127.0.0.1` listener), and dedupe on the parsed
/// `IpAddr` (so equivalent spellings like `fd12:0:0::1` and `fd12::1`
/// collapse). `warn_on_skip` gates the startup warnings so read-only callers
/// (the management API echo) stay silent.
fn accepted_listen_ips(listen_ips: Option<&[String]>, warn_on_skip: bool) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = Vec::new();
    for entry in listen_ips.unwrap_or_default() {
        let Ok(mut ip) = entry.trim().parse::<IpAddr>() else {
            if warn_on_skip {
                warn!(entry = %entry, "Ignoring unparseable [relay] listen_ips entry");
            }
            continue;
        };
        // Normalize IPv4-mapped IPv6 (`::ffff:a.b.c.d`) to the embedded IPv4
        // address before classification and dedup, matching how the
        // classifier already treats mapped addresses. This makes the default
        // `127.0.0.1` redundancy drop catch `::ffff:127.0.0.1`, collapses a
        // mapped entry against its plain-v4 duplicate, and binds an AF_INET
        // socket instead of a platform-dependent AF_INET6 mapped bind.
        if let IpAddr::V6(v6) = ip {
            if let Some(v4) = v6.to_ipv4_mapped() {
                ip = IpAddr::V4(v4);
            }
        }
        match classify_listen_ip(ip) {
            ListenIpClass::Ineligible(reason) => {
                if warn_on_skip {
                    warn!(
                        entry = %entry,
                        reason,
                        "Ignoring ineligible [relay] listen_ips entry; only RFC 1918, CGNAT (100.64.0.0/10), and IPv6 ULA (fc00::/7) addresses may be bound"
                    );
                }
                continue;
            }
            ListenIpClass::Loopback => {
                if warn_on_skip && ip != IpAddr::V4(Ipv4Addr::LOCALHOST) {
                    warn!(
                        entry = %entry,
                        "Ignoring loopback [relay] listen_ips entry; 127.0.0.1 is always bound and only RFC 1918, CGNAT (100.64.0.0/10), and IPv6 ULA (fc00::/7) addresses may be added"
                    );
                }
                continue;
            }
            ListenIpClass::Eligible => {}
        }
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    ips
}

/// Resolve `[relay] listen_ips` into the extra socket addresses to bind
/// alongside the always-bound `127.0.0.1:port` listener.
///
/// Unparseable, ineligible, and loopback entries are skipped with a `warn!`
/// rather than aborting startup (the default `127.0.0.1` is dropped
/// silently — it is exactly what the primary listener binds), and duplicate
/// entries are dropped so no address is ever bound twice.
pub fn resolve_extra_listen_addrs(listen_ips: Option<&[String]>, port: u16) -> Vec<SocketAddr> {
    accepted_listen_ips(listen_ips, true)
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect()
}

/// Canonicalize `[relay] listen_ips` for the management API's
/// `GET /api/network-interfaces` toggle-state echo: the same acceptance
/// rules as [`resolve_extra_listen_addrs`] (trim, parse, drop
/// unparseable/ineligible/loopback entries, dedupe), rendered via
/// `IpAddr`'s canonical `Display` so entries compare equal to the interface
/// list's `ip` strings. Emits no warnings — this is a read-side view, not
/// startup validation.
pub fn canonical_listen_ips(listen_ips: Option<&[String]>) -> Vec<String> {
    accepted_listen_ips(listen_ips, false)
        .into_iter()
        .map(|ip| ip.to_string())
        .collect()
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
    fn all_loopback_entries_are_dropped() {
        assert!(resolve(&["::1"], 9400).is_empty());
        assert!(resolve(&["127.0.0.2"], 9400).is_empty());
    }

    #[test]
    fn duplicates_are_deduped_and_whitespace_trimmed() {
        assert_eq!(
            resolve(&[" 10.0.0.7 ", "10.0.0.7"], 9400),
            vec!["10.0.0.7:9400".parse().unwrap()]
        );
    }

    #[test]
    fn ipv4_mapped_loopback_is_dropped_as_redundant() {
        assert!(resolve(&["::ffff:127.0.0.1"], 9400).is_empty());
    }

    #[test]
    fn ipv4_mapped_entries_normalize_to_plain_v4_binds() {
        assert_eq!(
            resolve(&["::ffff:192.168.1.10"], 9400),
            vec!["192.168.1.10:9400".parse().unwrap()]
        );
    }

    #[test]
    fn ipv4_mapped_entries_dedupe_against_plain_v4_duplicates() {
        assert_eq!(
            resolve(&["::ffff:10.0.0.7", "10.0.0.7"], 9400),
            vec!["10.0.0.7:9400".parse().unwrap()]
        );
    }

    fn canonical(entries: &[&str]) -> Vec<String> {
        let owned: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        canonical_listen_ips(Some(&owned))
    }

    #[test]
    fn canonical_listen_ips_trims_and_canonicalizes() {
        assert_eq!(
            canonical(&[" fd12:0:0::1 ", " 10.0.0.7"]),
            vec!["fd12::1".to_string(), "10.0.0.7".to_string()]
        );
    }

    #[test]
    fn canonical_listen_ips_dedupes_equivalent_spellings() {
        assert_eq!(
            canonical(&["fd12:0:0::1", "fd12::1", "fd12:0000::1"]),
            vec!["fd12::1".to_string()]
        );
    }

    #[test]
    fn canonical_listen_ips_drops_what_binding_would_not_accept() {
        assert_eq!(
            canonical(&["8.8.8.8", "0.0.0.0", "not-an-ip", "127.0.0.1"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn canonical_listen_ips_drops_loopback_like_binding() {
        assert!(canonical(&["::1", "127.0.0.2"]).is_empty());
    }

    #[test]
    fn canonical_listen_ips_normalizes_ipv4_mapped_to_plain_v4() {
        assert_eq!(
            canonical(&["::ffff:192.168.1.10", "::ffff:127.0.0.1"]),
            vec!["192.168.1.10".to_string()]
        );
    }

    #[test]
    fn canonical_listen_ips_matches_resolve_extra_listen_addrs() {
        let entries: Vec<String> = [" fd12:0:0::1 ", "192.168.1.5", "8.8.8.8", "127.0.0.1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let bound: Vec<String> = resolve_extra_listen_addrs(Some(&entries), 9400)
            .iter()
            .map(|a| a.ip().to_string())
            .collect();
        assert_eq!(canonical_listen_ips(Some(&entries)), bound);
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
