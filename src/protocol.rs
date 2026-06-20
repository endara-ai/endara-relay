//! Shared MCP protocol-version primitive.
//!
//! The relay must distinguish three dialects per peer — legacy `2024-11-05`,
//! legacy `2025-03-26`, and the new `2026-07-28` — because every 2026 behavior
//! branches on the negotiated version of *that specific peer*. This module is
//! the single source of truth for parsing, classifying, and detecting that
//! version so later behavioral tasks reference one type instead of scattered
//! string literals.
//!
//! This is plumbing only: no handshake or dispatch behavior changes here.

use serde_json::Value;

/// Wire string for the legacy `2024-11-05` dialect.
pub const VERSION_2024_11_05: &str = "2024-11-05";
/// Wire string for the legacy `2025-03-26` dialect (the relay's advertised
/// server baseline today).
pub const VERSION_2025_03_26: &str = "2025-03-26";
/// Wire string for the new `2026-07-28` dialect.
pub const VERSION_2026_07_28: &str = "2026-07-28";

/// HTTP header (lowercased, per `reqwest`/`http` storage) that conveys the
/// per-request protocol version for Streamable HTTP peers.
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// HTTP header (lowercased) that mirrors the JSON-RPC `method` of a 2026
/// Streamable HTTP request, enabling routing/observability without parsing the
/// body. Required for 2026 clients.
pub const MCP_METHOD_HEADER: &str = "mcp-method";

/// HTTP header (lowercased) that mirrors the tool name (`params.name`) of a
/// 2026 `tools/call` request. Absent for methods without a tool name.
pub const MCP_NAME_HEADER: &str = "mcp-name";

/// Reverse-DNS key under `params._meta` that 2026 clients use to attach their
/// identity on every request (there is no `initialize` handshake in 2026).
pub const META_CLIENT_INFO_KEY: &str = "io.modelcontextprotocol/clientInfo";

/// A negotiated MCP protocol dialect for a single peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolVersion {
    /// Legacy `2024-11-05`.
    V2024_11_05,
    /// Legacy `2025-03-26`.
    V2025_03_26,
    /// New `2026-07-28`.
    V2026_07_28,
}

impl ProtocolVersion {
    /// Fallback dialect when no explicit signal distinguishes a legacy peer.
    /// Matches the relay's advertised server baseline (`2025-03-26`).
    pub const LEGACY_DEFAULT: ProtocolVersion = ProtocolVersion::V2025_03_26;

    /// Parse a wire version string into a known dialect. Returns `None` for
    /// any string the relay does not model (e.g. `2025-06-18`, garbage).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            VERSION_2024_11_05 => Some(ProtocolVersion::V2024_11_05),
            VERSION_2025_03_26 => Some(ProtocolVersion::V2025_03_26),
            VERSION_2026_07_28 => Some(ProtocolVersion::V2026_07_28),
            _ => None,
        }
    }

    /// The canonical wire string for this dialect.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProtocolVersion::V2024_11_05 => VERSION_2024_11_05,
            ProtocolVersion::V2025_03_26 => VERSION_2025_03_26,
            ProtocolVersion::V2026_07_28 => VERSION_2026_07_28,
        }
    }

    /// `true` for the `2026-07-28` dialect — the gate every additive 2026
    /// behavior branches on. Consumed by T3–T13.
    pub fn is_2026(&self) -> bool {
        matches!(self, ProtocolVersion::V2026_07_28)
    }

    /// `true` for any pre-2026 (legacy) dialect. Consumed by T3–T13.
    #[allow(dead_code)]
    pub fn is_legacy(&self) -> bool {
        !self.is_2026()
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        ProtocolVersion::LEGACY_DEFAULT
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProtocolVersion {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ProtocolVersion::parse(s).ok_or(())
    }
}

/// Borrow the `params._meta["io.modelcontextprotocol/clientInfo"]` value, if
/// present. 2026 clients attach this on every request in lieu of a handshake.
pub fn meta_client_info(params: Option<&Value>) -> Option<&Value> {
    params?.get("_meta")?.get(META_CLIENT_INFO_KEY)
}

/// Detect the dialect of an inbound (relay-as-server) request from the three
/// signals defined by the spec, in priority order:
///
/// 1. An explicit, recognized `MCP-Protocol-Version` header wins outright.
/// 2. Otherwise, the absence of an `initialize` handshake combined with a
///    `_meta` `clientInfo` payload identifies a stateless 2026 client.
/// 3. Otherwise fall back to the legacy baseline.
///
/// This is pure detection — it records nothing and changes no behavior.
pub fn detect_inbound_dialect(
    has_initialize: bool,
    protocol_header: Option<&str>,
    params: Option<&Value>,
) -> ProtocolVersion {
    if let Some(version) = protocol_header.and_then(ProtocolVersion::parse) {
        return version;
    }
    if !has_initialize && meta_client_info(params).is_some() {
        return ProtocolVersion::V2026_07_28;
    }
    ProtocolVersion::LEGACY_DEFAULT
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_as_str_round_trip() {
        for v in [
            ProtocolVersion::V2024_11_05,
            ProtocolVersion::V2025_03_26,
            ProtocolVersion::V2026_07_28,
        ] {
            assert_eq!(ProtocolVersion::parse(v.as_str()), Some(v));
            assert_eq!(v.to_string(), v.as_str());
            assert_eq!(v.as_str().parse::<ProtocolVersion>(), Ok(v));
        }
    }

    #[test]
    fn parse_rejects_unknown_strings() {
        for s in ["", "2025-06-18", "2026-07-28 ", "garbage", "2024"] {
            assert_eq!(ProtocolVersion::parse(s), None);
            assert_eq!(s.parse::<ProtocolVersion>(), Err(()));
        }
    }

    #[test]
    fn classification_gates() {
        assert!(ProtocolVersion::V2026_07_28.is_2026());
        assert!(!ProtocolVersion::V2026_07_28.is_legacy());
        for v in [ProtocolVersion::V2024_11_05, ProtocolVersion::V2025_03_26] {
            assert!(!v.is_2026());
            assert!(v.is_legacy());
        }
        assert_eq!(ProtocolVersion::default(), ProtocolVersion::V2025_03_26);
    }

    #[test]
    fn detect_from_header_wins() {
        assert_eq!(
            detect_inbound_dialect(true, Some("2026-07-28"), None),
            ProtocolVersion::V2026_07_28
        );
        assert_eq!(
            detect_inbound_dialect(false, Some("2024-11-05"), None),
            ProtocolVersion::V2024_11_05
        );
    }

    #[test]
    fn detect_from_meta_client_info_without_initialize() {
        let params = json!({
            "_meta": { META_CLIENT_INFO_KEY: { "name": "demo", "version": "1.0" } }
        });
        assert_eq!(
            detect_inbound_dialect(false, None, Some(&params)),
            ProtocolVersion::V2026_07_28
        );
        assert!(meta_client_info(Some(&params)).is_some());
    }

    #[test]
    fn detect_falls_back_to_legacy_baseline() {
        assert_eq!(
            detect_inbound_dialect(true, None, None),
            ProtocolVersion::V2025_03_26
        );
        let params = json!({
            "_meta": { META_CLIENT_INFO_KEY: { "name": "demo" } }
        });
        assert_eq!(
            detect_inbound_dialect(true, None, Some(&params)),
            ProtocolVersion::V2025_03_26
        );
        assert_eq!(
            detect_inbound_dialect(false, Some("garbage"), None),
            ProtocolVersion::V2025_03_26
        );
    }
}
