//! Resolve the effective `server_type` for an adapter from an optional
//! per-endpoint override and the upstream-sanitized name reported via
//! `serverInfo.name`.
//!
//! Rules:
//! - If a `server_type_override` is supplied, it is run through
//!   [`sanitize_server_name`] and used directly. The `-mcp-server` strip is
//!   **never** applied to overrides — the user is taken at their word.
//! - If the override is missing **or** fails sanitization, the upstream
//!   sanitized name is used, with [`strip_mcp_server_suffix`] applied to remove
//!   one of the placeholder suffixes that upstream servers commonly bake into
//!   their `serverInfo.name`.
//! - The strip is a closed list of four suffixes (`-mcp-server`, `_mcp_server`,
//!   `-mcp`, `_mcp`); strips that would empty the name return the original.

use super::server_name::sanitize_server_name;

/// The closed list of upstream suffixes that are stripped from the
/// upstream-derived `server_type`. Order matters: longer variants first so
/// `something-mcp-server` strips down to `something` rather than
/// `something-mcp`.
const STRIP_SUFFIXES: &[&str] = &["-mcp-server", "_mcp_server", "-mcp", "_mcp"];

/// Strip a single trailing `-mcp-server` / `_mcp_server` / `-mcp` / `_mcp`
/// suffix from the upstream-derived sanitized name.
///
/// The strip is applied at most once and is skipped if it would reduce the
/// name to empty (e.g. a pathological `"_mcp"` upstream name keeps as-is).
pub fn strip_mcp_server_suffix(name: String) -> String {
    for suffix in STRIP_SUFFIXES {
        if let Some(stripped) = name.strip_suffix(suffix) {
            if !stripped.is_empty() {
                return stripped.to_string();
            }
        }
    }
    name
}

/// Resolve the effective `server_type` advertised to connected models.
///
/// `override_field` is the user-supplied `server_type_override` from
/// `EndpointConfig` (already env-resolved). `upstream_sanitized` is the result
/// of [`sanitize_server_name`] on `serverInfo.name`.
///
/// Falls back to the upstream-stripped name when the override fails
/// sanitization. Adapters should log a warning at the call site when this
/// fallback is taken so the misconfiguration is visible.
pub fn effective_server_type(
    override_field: Option<String>,
    upstream_sanitized: Option<String>,
) -> Option<String> {
    if let Some(o) = override_field {
        if let Ok(sanitized) = sanitize_server_name(&o) {
            return Some(sanitized);
        }
        // Override was supplied but unusable; fall through to the
        // upstream-derived name so the endpoint still advertises something.
    }
    upstream_sanitized.map(strip_mcp_server_suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- strip_mcp_server_suffix -------------------------------------------

    #[test]
    fn strip_dash_mcp_server() {
        assert_eq!(
            strip_mcp_server_suffix("linear-mcp-server".into()),
            "linear"
        );
    }

    #[test]
    fn strip_underscore_mcp_server() {
        assert_eq!(
            strip_mcp_server_suffix("linear_mcp_server".into()),
            "linear"
        );
    }

    #[test]
    fn strip_dash_mcp() {
        assert_eq!(strip_mcp_server_suffix("linear-mcp".into()), "linear");
    }

    #[test]
    fn strip_underscore_mcp() {
        assert_eq!(strip_mcp_server_suffix("linear_mcp".into()), "linear");
    }

    #[test]
    fn strip_no_suffix_match_is_noop() {
        assert_eq!(strip_mcp_server_suffix("linear".into()), "linear");
        assert_eq!(
            strip_mcp_server_suffix("statelessserver".into()),
            "statelessserver"
        );
        // Substring (not suffix) is left alone.
        assert_eq!(
            strip_mcp_server_suffix("mcp-server-x".into()),
            "mcp-server-x"
        );
    }

    #[test]
    fn strip_prefers_longest_suffix_first() {
        // "-mcp-server" is tried before "-mcp", so the result is `linear`,
        // not `linear-server`.
        assert_eq!(
            strip_mcp_server_suffix("linear-mcp-server".into()),
            "linear"
        );
    }

    #[test]
    fn strip_skipped_when_it_would_empty_the_name() {
        // Pathological all-suffix names: stripping would leave empty, so the
        // original is preserved.
        for s in ["-mcp-server", "_mcp_server", "-mcp", "_mcp"] {
            assert_eq!(
                strip_mcp_server_suffix(s.into()),
                s,
                "skip-empty for {:?}",
                s
            );
        }
    }

    #[test]
    fn strip_is_idempotent() {
        let first = strip_mcp_server_suffix("foo-mcp-server".into());
        let second = strip_mcp_server_suffix(first.clone());
        assert_eq!(first, second);
        assert_eq!(second, "foo");
    }

    // --- effective_server_type ---------------------------------------------

    #[test]
    fn override_happy_path_takes_precedence_over_strip() {
        // Override wins even when the upstream name would have been stripped,
        // and the override is taken verbatim (not stripped).
        let out = effective_server_type(
            Some("google-drive-mcp-server".into()),
            Some("statelessserver".into()),
        );
        assert_eq!(out, Some("google-drive-mcp-server".into()));
    }

    #[test]
    fn override_is_sanitized() {
        // `Google Drive` sanitizes to `google-drive`.
        let out = effective_server_type(Some("Google Drive".into()), Some("upstream".into()));
        assert_eq!(out, Some("google-drive".into()));
    }

    #[test]
    fn override_failing_sanitization_falls_back_to_stripped_upstream() {
        // Pure-emoji override is unsanitizable; we fall back to the
        // upstream-stripped name rather than returning `None`.
        let out = effective_server_type(Some("🚀🚀🚀".into()), Some("foo-mcp-server".into()));
        assert_eq!(out, Some("foo".into()));
    }

    #[test]
    fn override_failing_with_no_upstream_returns_none() {
        let out = effective_server_type(Some("🚀".into()), None);
        assert_eq!(out, None);
    }

    #[test]
    fn no_override_strips_upstream() {
        let out = effective_server_type(None, Some("linear-mcp-server".into()));
        assert_eq!(out, Some("linear".into()));
    }

    #[test]
    fn no_override_no_upstream_returns_none() {
        assert_eq!(effective_server_type(None, None), None);
    }

    #[test]
    fn no_override_unstripped_upstream_passes_through() {
        let out = effective_server_type(None, Some("linear".into()));
        assert_eq!(out, Some("linear".into()));
    }
}
