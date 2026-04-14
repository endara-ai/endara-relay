//! Server name sanitization and validation for MCP `serverInfo.name`.
//!
//! The sanitized form is used as the `{server_type}__` prefix for tool names.

use serde::Serialize;

/// Errors from server name validation.
#[allow(dead_code)] // Will be used by adapter `initialize()` implementations
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize)]
pub enum ServerNameError {
    #[error("server did not report `serverInfo.name` in initialize response")]
    Missing,
    #[error("`serverInfo.name` was present but empty after trimming")]
    Empty,
    #[error("`serverInfo.name` {raw:?} reduced to empty after sanitization")]
    Unsanitizable { raw: String },
    #[error("`serverInfo.name` length {length} exceeds 64-character limit")]
    TooLong { length: usize },
}

/// Sanitize and validate a server-declared name from the MCP `initialize` response.
///
/// Returns `Ok(sanitized)` if the name is usable as a prefix, `Err(reason)` otherwise.
/// The sanitized form is lower-cased, trimmed, with whitespace runs collapsed to a
/// single `-`, and any character outside `[a-z0-9_-]` replaced with `-`.
#[allow(dead_code)] // Will be used by adapter `initialize()` implementations
pub fn sanitize_server_name(raw: &str) -> Result<String, ServerNameError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ServerNameError::Empty);
    }

    let mut out = String::with_capacity(trimmed.len());
    let mut last_was_dash = false;
    for ch in trimmed.chars().flat_map(|c| c.to_lowercase()) {
        let mapped = match ch {
            'a'..='z' | '0'..='9' | '_' => {
                last_was_dash = false;
                ch
            }
            '-' | ' ' | '\t' | '.' | '/' | ':' => {
                if last_was_dash {
                    continue;
                }
                last_was_dash = true;
                '-'
            }
            _ => {
                if last_was_dash {
                    continue;
                }
                last_was_dash = true;
                '-'
            }
        };
        out.push(mapped);
    }
    let sanitized = out.trim_matches('-').to_string();

    if sanitized.is_empty() {
        return Err(ServerNameError::Unsanitizable {
            raw: raw.to_string(),
        });
    }
    if sanitized.len() > 64 {
        return Err(ServerNameError::TooLong {
            length: sanitized.len(),
        });
    }
    Ok(sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Row 1: "linear" → Ok("linear")
    #[test]
    fn test_simple_lowercase() {
        assert_eq!(sanitize_server_name("linear"), Ok("linear".to_string()));
    }

    // Row 2: "Linear MCP Server" → Ok("linear-mcp-server")
    #[test]
    fn test_mixed_case_with_spaces() {
        assert_eq!(
            sanitize_server_name("Linear MCP Server"),
            Ok("linear-mcp-server".to_string())
        );
    }

    // Row 3: " spaces " → Ok("spaces")
    #[test]
    fn test_leading_trailing_spaces() {
        assert_eq!(sanitize_server_name(" spaces "), Ok("spaces".to_string()));
    }

    // Row 4: "a/b/c" → Ok("a-b-c")
    #[test]
    fn test_slashes() {
        assert_eq!(sanitize_server_name("a/b/c"), Ok("a-b-c".to_string()));
    }

    // Row 5: "UPPER_lower_123" → Ok("upper_lower_123")
    #[test]
    fn test_uppercase_underscore_digits() {
        assert_eq!(
            sanitize_server_name("UPPER_lower_123"),
            Ok("upper_lower_123".to_string())
        );
    }

    // Row 6: "a..b..c" → Ok("a-b-c") (collapse)
    #[test]
    fn test_collapse_dots() {
        assert_eq!(sanitize_server_name("a..b..c"), Ok("a-b-c".to_string()));
    }

    // Row 7: "" → Err(Empty)
    #[test]
    fn test_empty_string() {
        assert!(matches!(
            sanitize_server_name(""),
            Err(ServerNameError::Empty)
        ));
    }

    // Row 8: " " → Err(Empty)
    #[test]
    fn test_whitespace_only() {
        assert!(matches!(
            sanitize_server_name(" "),
            Err(ServerNameError::Empty)
        ));
    }

    // Row 9: "🚀" → Err(Unsanitizable)
    #[test]
    fn test_single_emoji() {
        assert!(matches!(
            sanitize_server_name("🚀"),
            Err(ServerNameError::Unsanitizable { .. })
        ));
    }

    // Row 10: "🚀🚀🚀" → Err(Unsanitizable)
    #[test]
    fn test_multiple_emoji() {
        assert!(matches!(
            sanitize_server_name("🚀🚀🚀"),
            Err(ServerNameError::Unsanitizable { .. })
        ));
    }

    // Row 11: "a".repeat(65) → Err(TooLong)
    #[test]
    fn test_too_long() {
        let input = "a".repeat(65);
        assert!(matches!(
            sanitize_server_name(&input),
            Err(ServerNameError::TooLong { length: 65 })
        ));
    }

    // Row 12: "a".repeat(64) → Ok(64-char string) (boundary)
    #[test]
    fn test_boundary_64_chars() {
        let input = "a".repeat(64);
        let result = sanitize_server_name(&input);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 64);
    }

    // Row 13: "中文" → Err(Unsanitizable) (non-ASCII all gets replaced)
    #[test]
    fn test_non_ascii_chinese() {
        assert!(matches!(
            sanitize_server_name("中文"),
            Err(ServerNameError::Unsanitizable { .. })
        ));
    }

    // Row 14: "my-server.v2" → Ok("my-server-v2")
    #[test]
    fn test_dots_in_name() {
        assert_eq!(
            sanitize_server_name("my-server.v2"),
            Ok("my-server-v2".to_string())
        );
    }

    // Row 15: Idempotence: sanitize(sanitize(x)) == sanitize(x) for rows 1-6 and 14
    #[test]
    fn test_idempotence() {
        let test_cases = [
            "linear",
            "Linear MCP Server",
            " spaces ",
            "a/b/c",
            "UPPER_lower_123",
            "a..b..c",
            "my-server.v2",
        ];

        for input in test_cases {
            let first = sanitize_server_name(input).expect("first sanitize should succeed");
            let second = sanitize_server_name(&first).expect("second sanitize should succeed");
            assert_eq!(first, second, "idempotence failed for input: {:?}", input);
        }
    }
}
