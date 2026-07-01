//! Reversible per-endpoint wrapping for resource URIs (DD4 wrapper scheme).
//!
//! ## Rewrite boundary (DD1 + DD2)
//!
//! The relay rewrites URIs only at the **enumerated MCP protocol slots**
//! listed in the spec (DD1): outbound `tools/list` / `tools/call` /
//! `resources/list` / `resources/templates/list` / `prompts/get` results,
//! and inbound `resources/read` params. The encode/decode primitives in
//! this module are the lowest-level boundary for that rewrite.
//!
//! Per DD2, references **inside** a served resource body
//! (HTML/JS/CSS of a `ui://` app referencing another resource) are
//! intentionally NOT parsed or rewritten in v1. Apps are expected to use
//! either:
//!
//! - **relative references** within their own resource namespace, which
//!   the MCP Apps host resolves against the already-namespaced parent URI
//!   (e.g. an iframe at `mcp-relay://ep/ui%3A%2F%2Fapp%2Findex.html`
//!   referencing `./style.css` resolves to
//!   `mcp-relay://ep/ui%3A%2F%2Fapp%2Fstyle.css`); OR
//! - external assets reached via absolute `http(s)://` URLs allowlisted by
//!   `_meta.ui.csp`, which are left untouched on both legs.
//!
//! Absolute `ui://` references emitted from inside a body to another app's
//! resource are a documented v1 limitation. This boundary does NOT
//! validate or reject body content — `resources/read` returns upstream
//! bodies verbatim (see `mcp_resources_read` in `server.rs`).
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use std::fmt;

/// Wrapper scheme used to namespace resource URIs across endpoints.
#[allow(dead_code)]
pub const WRAPPER_SCHEME: &str = "mcp-relay";
const WRAPPER_PREFIX: &str = "mcp-relay://";

/// Characters to percent-encode when wrapping an original URI. Encodes every
/// ASCII character except RFC 3986 "unreserved" (ALPHA / DIGIT / `-` / `.` /
/// `_` / `~`), so the wrapper structure (`mcp-relay://endpoint/...`) is
/// unambiguous and the encoded payload round-trips losslessly regardless of
/// the original scheme.
const ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// RFC 6570 variant of [`ENCODE_SET`] for wrapping URI **templates** (slot
/// #5). Identical to `ENCODE_SET` except `{` and `}` are left unencoded so a
/// downstream MCP host that performs RFC 6570 expansion still sees the
/// literal `{var}` markers it needs to substitute. The reverse decoder in
/// [`decode_resource_uri`] is agnostic — both encoded and literal braces
/// round-trip cleanly through `percent_decode_str`.
const ENCODE_SET_TEMPLATE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'|');

/// Error type for resource-URI wrapping/unwrapping.
#[derive(Debug)]
#[allow(dead_code)]
pub enum ResourceUriError {
    /// The wrapped URI is missing the scheme prefix, separator, or has an
    /// empty/invalid segment.
    InvalidFormat(String),
}

impl fmt::Display for ResourceUriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceUriError::InvalidFormat(msg) => {
                write!(f, "invalid wrapped resource URI: {}", msg)
            }
        }
    }
}

impl std::error::Error for ResourceUriError {}

/// Encode an original resource URI into a reversible per-endpoint wrapper:
/// `mcp-relay://{endpoint}/{percent-encoded-original}`.
///
/// Reversible without a lookup table (DD4), which matters because result URIs
/// are non-enumerable. Idempotent only relative to TOON / non-URI passes —
/// re-encoding an already-wrapped URI nests it.
pub fn encode_resource_uri(endpoint: &str, original: &str) -> String {
    let encoded = utf8_percent_encode(original, ENCODE_SET);
    format!("{}{}/{}", WRAPPER_PREFIX, endpoint, encoded)
}

/// Decode a wrapped resource URI into `(endpoint, original)`. Rejects any
/// malformed input (wrong scheme, missing separator, empty endpoint, invalid
/// percent-encoding) with a typed `ResourceUriError`.
pub fn decode_resource_uri(wrapped: &str) -> Result<(String, String), ResourceUriError> {
    let body = wrapped.strip_prefix(WRAPPER_PREFIX).ok_or_else(|| {
        ResourceUriError::InvalidFormat(format!(
            "missing '{}' scheme prefix in '{}'",
            WRAPPER_PREFIX, wrapped
        ))
    })?;

    let slash_idx = body.find('/').ok_or_else(|| {
        ResourceUriError::InvalidFormat(format!(
            "missing '/' separator after endpoint in '{}'",
            wrapped
        ))
    })?;

    let endpoint = &body[..slash_idx];
    let encoded_original = &body[slash_idx + 1..];

    if endpoint.is_empty() {
        return Err(ResourceUriError::InvalidFormat(format!(
            "empty endpoint segment in '{}'",
            wrapped
        )));
    }

    let original = percent_decode_str(encoded_original)
        .decode_utf8()
        .map_err(|e| {
            ResourceUriError::InvalidFormat(format!(
                "invalid percent-encoding in '{}': {}",
                wrapped, e
            ))
        })?;

    Ok((endpoint.to_string(), original.into_owned()))
}

/// DD5 single-endpoint passthrough on the outbound (wrap) path: when only one
/// active endpoint exists, the caller passes `skip_wrap = true` so resource
/// URIs flow through unchanged (mirrors `build_catalog`'s `skip_prefix` flag).
#[allow(dead_code)]
pub fn maybe_encode_resource_uri(endpoint: &str, original: &str, skip_wrap: bool) -> String {
    if skip_wrap {
        original.to_string()
    } else {
        encode_resource_uri(endpoint, original)
    }
}

/// Template variant of [`encode_resource_uri`] for slot #5
/// (`resourceTemplates[].uriTemplate`). Uses [`ENCODE_SET_TEMPLATE`] so RFC
/// 6570 `{var}` markers survive the wrap unencoded — without this, a host
/// that expands templates would never see literal braces and variable
/// substitution would silently no-op.
#[allow(dead_code)]
pub fn encode_resource_uri_template(endpoint: &str, original: &str) -> String {
    let encoded = utf8_percent_encode(original, ENCODE_SET_TEMPLATE);
    format!("{}{}/{}", WRAPPER_PREFIX, endpoint, encoded)
}

/// DD5 single-endpoint passthrough for templates; mirrors
/// [`maybe_encode_resource_uri`] but routes through
/// [`encode_resource_uri_template`] so `{var}` survives the wrap.
#[allow(dead_code)]
pub fn maybe_encode_resource_uri_template(
    endpoint: &str,
    original: &str,
    skip_wrap: bool,
) -> String {
    if skip_wrap {
        original.to_string()
    } else {
        encode_resource_uri_template(endpoint, original)
    }
}

/// DD5 mirror for the inbound (unwrap) path: when `skip_wrap` is `true`, the
/// caller treats the incoming URI as already-original (no endpoint hint, as
/// there is only one); otherwise the wrapper is unwrapped to
/// `(Some(endpoint), original)`.
#[allow(dead_code)]
pub fn maybe_decode_resource_uri(
    incoming: &str,
    skip_wrap: bool,
) -> Result<(Option<String>, String), ResourceUriError> {
    if skip_wrap {
        Ok((None, incoming.to_string()))
    } else {
        let (ep, orig) = decode_resource_uri(incoming)?;
        Ok((Some(ep), orig))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_roundtrip(endpoint: &str, original: &str) {
        let wrapped = encode_resource_uri(endpoint, original);
        let (ep, orig) = decode_resource_uri(&wrapped).expect("decode should succeed");
        assert_eq!(ep, endpoint, "endpoint mismatch (wrapped: {})", wrapped);
        assert_eq!(orig, original, "original mismatch (wrapped: {})", wrapped);
    }

    #[test]
    fn test_encode_basic_shape() {
        let wrapped = encode_resource_uri("work", "ui://app/main");
        assert!(wrapped.starts_with("mcp-relay://work/"), "got: {}", wrapped);
        assert!(
            !wrapped.contains("ui://"),
            "scheme must be percent-encoded, got: {}",
            wrapped
        );
    }

    #[test]
    fn test_roundtrip_ui_scheme() {
        assert_roundtrip("work", "ui://app/main");
        assert_roundtrip("work", "ui://my-app/path/to/resource");
    }

    #[test]
    fn test_roundtrip_file_scheme() {
        assert_roundtrip("local", "file:///Users/alice/notes.md");
        assert_roundtrip("local", "file:///tmp/with%20space");
    }

    #[test]
    fn test_roundtrip_https_scheme() {
        assert_roundtrip("web", "https://example.com/path?q=1&r=2#frag");
        assert_roundtrip("web", "https://example.com/");
    }

    #[test]
    fn test_roundtrip_assorted_schemes() {
        for (ep, uri) in [
            ("a", "custom-scheme:opaque-payload"),
            ("b", "data:text/plain;base64,SGVsbG8="),
            ("c", "git+ssh://git@example.com:org/repo.git"),
        ] {
            assert_roundtrip(ep, uri);
        }
    }

    #[test]
    fn test_roundtrip_percent_edge_cases() {
        assert_roundtrip("ep", "https://x/%20already%20encoded");
        assert_roundtrip("ep", "ui://app/with spaces and /slashes");
        assert_roundtrip("ep", "ui://app/?query=a&b=c#hash");
        assert_roundtrip("ep", "ui://app/unicode/café/日本語");
        assert_roundtrip("ep", "ui://app/punct!*'();:@&=+$,?#[]");
        assert_roundtrip("ep", "ui://app/control\nchar");
    }

    #[test]
    fn test_roundtrip_empty_original() {
        let wrapped = encode_resource_uri("ep", "");
        assert_eq!(wrapped, "mcp-relay://ep/");
        let (ep, orig) = decode_resource_uri(&wrapped).expect("empty original is allowed");
        assert_eq!(ep, "ep");
        assert_eq!(orig, "");
    }

    #[test]
    fn test_encode_idempotent_within_pass_is_nested() {
        // Encoding an already-wrapped URI nests it (DD3: idempotent vs TOON,
        // not vs itself). The nested form still round-trips losslessly.
        let inner = encode_resource_uri("a", "ui://x/y");
        let outer = encode_resource_uri("b", &inner);
        let (ep_outer, orig_outer) = decode_resource_uri(&outer).unwrap();
        assert_eq!(ep_outer, "b");
        assert_eq!(orig_outer, inner);
        let (ep_inner, orig_inner) = decode_resource_uri(&orig_outer).unwrap();
        assert_eq!(ep_inner, "a");
        assert_eq!(orig_inner, "ui://x/y");
    }

    #[test]
    fn test_decode_rejects_empty_string() {
        assert!(matches!(
            decode_resource_uri(""),
            Err(ResourceUriError::InvalidFormat(_))
        ));
    }

    #[test]
    fn test_decode_rejects_wrong_scheme() {
        assert!(decode_resource_uri("ui://app/main").is_err());
        assert!(decode_resource_uri("https://example.com/").is_err());
        assert!(decode_resource_uri("mcp-relay:/ep/foo").is_err());
        assert!(decode_resource_uri("mcp-relayy://ep/foo").is_err());
    }

    #[test]
    fn test_decode_rejects_missing_separator() {
        // No '/' after the endpoint.
        assert!(decode_resource_uri("mcp-relay://ep").is_err());
        assert!(decode_resource_uri("mcp-relay://").is_err());
    }

    #[test]
    fn test_decode_rejects_empty_endpoint() {
        assert!(decode_resource_uri("mcp-relay:///foo").is_err());
        assert!(decode_resource_uri("mcp-relay:///").is_err());
    }

    #[test]
    fn test_decode_rejects_invalid_utf8_percent_encoding() {
        // Decoded bytes that aren't valid UTF-8 are rejected. The
        // `percent-encoding` crate is intentionally forgiving on malformed
        // escapes themselves (e.g. `%ZZ` is preserved verbatim), so only the
        // UTF-8 validity of the decoded payload is enforced here.
        assert!(decode_resource_uri("mcp-relay://ep/%FF%FE").is_err());
        assert!(decode_resource_uri("mcp-relay://ep/%C3%28").is_err());
    }

    #[test]
    fn test_decode_preserves_non_escape_percent() {
        // `%ZZ` is not a valid escape but the decoder leaves it intact.
        // Round-trip with a real encode goes through the percent-encoder,
        // so `%` from the original is encoded as `%25` and survives cleanly.
        let (ep, orig) = decode_resource_uri("mcp-relay://ep/%ZZ").unwrap();
        assert_eq!(ep, "ep");
        assert_eq!(orig, "%ZZ");
    }

    #[test]
    fn test_maybe_encode_skip_wrap_passthrough() {
        let original = "ui://app/main";
        assert_eq!(
            maybe_encode_resource_uri("work", original, true),
            original,
            "skip_wrap=true must pass through unchanged"
        );
        assert_eq!(
            maybe_encode_resource_uri("work", original, false),
            encode_resource_uri("work", original)
        );
    }

    #[test]
    fn test_maybe_decode_skip_wrap_passthrough() {
        let raw = "ui://app/main";
        let (ep, orig) = maybe_decode_resource_uri(raw, true).unwrap();
        assert!(ep.is_none(), "skip_wrap=true returns no endpoint hint");
        assert_eq!(orig, raw);

        let wrapped = encode_resource_uri("work", raw);
        let (ep, orig) = maybe_decode_resource_uri(&wrapped, false).unwrap();
        assert_eq!(ep.as_deref(), Some("work"));
        assert_eq!(orig, raw);
    }

    #[test]
    fn test_maybe_decode_skip_wrap_false_validates() {
        // skip_wrap=false must still reject malformed wrappers.
        assert!(maybe_decode_resource_uri("not-a-wrapper", false).is_err());
    }

    #[test]
    fn test_encode_template_preserves_rfc6570_braces() {
        // Slot #5 wrappers must keep `{var}` literal so RFC 6570 expansion on
        // the client still finds the markers to substitute.
        let wrapped = encode_resource_uri_template("ep", "ui://app/items/{id}");
        assert!(wrapped.starts_with("mcp-relay://ep/"));
        assert!(
            wrapped.contains("{id}"),
            "braces must be literal in template wrap, got {}",
            wrapped
        );
        // The wrap is still reversible: a decode yields the original template
        // with its braces intact.
        let (ep, orig) = decode_resource_uri(&wrapped).expect("template wrap round-trips");
        assert_eq!(ep, "ep");
        assert_eq!(orig, "ui://app/items/{id}");
    }

    #[test]
    fn test_encode_template_handles_multiple_vars() {
        let wrapped = encode_resource_uri_template("ep", "file:///docs/{section}/{page}");
        assert!(wrapped.contains("{section}"));
        assert!(wrapped.contains("{page}"));
        let (_, orig) = decode_resource_uri(&wrapped).unwrap();
        assert_eq!(orig, "file:///docs/{section}/{page}");
    }

    #[test]
    fn test_maybe_encode_template_skip_wrap_passthrough() {
        let original = "ui://app/items/{id}";
        assert_eq!(
            maybe_encode_resource_uri_template("ep", original, true),
            original
        );
        assert_eq!(
            maybe_encode_resource_uri_template("ep", original, false),
            encode_resource_uri_template("ep", original)
        );
    }

    #[test]
    fn test_encode_template_still_encodes_path_metachars() {
        // Template wrap leaves `{`/`}` alone but still percent-encodes other
        // reserved characters (`?`, `#`, `/`, spaces, etc.) so the wrapper
        // structure stays unambiguous.
        let wrapped = encode_resource_uri_template("ep", "ui://app/?q={q}#frag");
        // The leading scheme separator inside the original is encoded.
        assert!(!wrapped.contains("://app/"));
        // Braces survive verbatim.
        assert!(wrapped.contains("{q}"));
        let (_, orig) = decode_resource_uri(&wrapped).unwrap();
        assert_eq!(orig, "ui://app/?q={q}#frag");
    }
}
