use std::fmt;

/// Error type for prefix operations.
#[derive(Debug)]
#[allow(dead_code)]
pub enum PrefixError {
    /// The prefixed name does not contain enough segments.
    InvalidFormat(String),
}

impl fmt::Display for PrefixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrefixError::InvalidFormat(msg) => write!(f, "invalid prefixed tool name: {}", msg),
        }
    }
}

impl std::error::Error for PrefixError {}

/// Encode a tool name with collision-aware prefixes.
///
/// - When `instance_name` is `None`: `{server_type}__{tool}`
/// - When `instance_name` is `Some`: `{server_type}__{instance}__{tool}`
pub fn encode_tool_name(server_type: &str, instance_name: Option<&str>, tool: &str) -> String {
    match instance_name {
        Some(instance) => format!("{}__{}__{}", server_type, instance, tool),
        None => format!("{}__{}", server_type, tool),
    }
}

/// Decode a prefixed tool name into `(server_type, Option<instance>, tool)`.
///
/// Because the two-segment format (`server_type__tool`) and three-segment format
/// (`server_type__instance__tool`) are ambiguous without context, callers should
/// prefer the reverse-lookup map built by the registry instead of this function.
///
/// This function splits on the **first** `__` delimiter:
/// - If there is exactly one `__`, returns `(server_type, None, tool)`.
/// - If there are two or more `__`, returns `(server_type, Some(instance), tool)`
///   where `tool` is everything after the second `__`.
#[allow(dead_code)]
pub fn decode_tool_name(prefixed: &str) -> Result<(String, Option<String>, String), PrefixError> {
    let mut parts = prefixed.splitn(3, "__");

    let server_type = parts
        .next()
        .ok_or_else(|| PrefixError::InvalidFormat("empty string".to_string()))?;

    let second = parts.next().ok_or_else(|| {
        PrefixError::InvalidFormat(format!("missing segment in '{}'", prefixed))
    })?;

    if server_type.is_empty() || second.is_empty() {
        return Err(PrefixError::InvalidFormat(format!(
            "empty segment in '{}'",
            prefixed
        )));
    }

    match parts.next() {
        // Three segments: server_type__instance__tool
        Some(tool) => {
            if tool.is_empty() {
                return Err(PrefixError::InvalidFormat(format!(
                    "empty tool segment in '{}'",
                    prefixed
                )));
            }
            Ok((
                server_type.to_string(),
                Some(second.to_string()),
                tool.to_string(),
            ))
        }
        // Two segments: server_type__tool
        None => Ok((server_type.to_string(), None, second.to_string())),
    }
}

/// Sanitize an MCP server name for use as an identifier.
///
/// - Converts to lowercase
/// - Replaces spaces with underscores
/// - Strips characters not matching `[a-z0-9_-]`
/// - Returns `None` if the result is empty
pub fn sanitize_name(name: &str) -> Option<String> {
    let sanitized: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c == ' ' { '_' } else { c })
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
        .collect();

    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_without_instance() {
        assert_eq!(
            encode_tool_name("echo_mcp", None, "echo"),
            "echo_mcp__echo"
        );
    }

    #[test]
    fn test_encode_with_instance() {
        assert_eq!(
            encode_tool_name("echo_mcp", Some("work"), "echo"),
            "echo_mcp__work__echo"
        );
    }

    #[test]
    fn test_decode_two_segments() {
        let (st, inst, t) = decode_tool_name("echo_mcp__echo").unwrap();
        assert_eq!(st, "echo_mcp");
        assert!(inst.is_none());
        assert_eq!(t, "echo");
    }

    #[test]
    fn test_decode_three_segments() {
        let (st, inst, t) = decode_tool_name("echo_mcp__work__echo").unwrap();
        assert_eq!(st, "echo_mcp");
        assert_eq!(inst.as_deref(), Some("work"));
        assert_eq!(t, "echo");
    }

    #[test]
    fn test_roundtrip_no_instance() {
        let encoded = encode_tool_name("fs", None, "read_file");
        let (st, inst, t) = decode_tool_name(&encoded).unwrap();
        assert_eq!(st, "fs");
        assert!(inst.is_none());
        assert_eq!(t, "read_file");
    }

    #[test]
    fn test_roundtrip_with_instance() {
        let encoded = encode_tool_name("fs", Some("my-ep"), "read_file");
        let (st, inst, t) = decode_tool_name(&encoded).unwrap();
        assert_eq!(st, "fs");
        assert_eq!(inst.as_deref(), Some("my-ep"));
        assert_eq!(t, "read_file");
    }

    #[test]
    fn test_tool_name_with_double_underscore() {
        // Three-segment decode: tool part gets everything after second __
        let encoded = encode_tool_name("m", Some("e"), "tool__with__underscores");
        assert_eq!(encoded, "m__e__tool__with__underscores");
        let (st, inst, t) = decode_tool_name(&encoded).unwrap();
        assert_eq!(st, "m");
        assert_eq!(inst.as_deref(), Some("e"));
        assert_eq!(t, "tool__with__underscores");
    }

    #[test]
    fn test_decode_missing_segments() {
        assert!(decode_tool_name("only_one").is_err());
    }

    #[test]
    fn test_decode_empty_segments() {
        assert!(decode_tool_name("__tool").is_err());
        assert!(decode_tool_name("server__").is_err());
        assert!(decode_tool_name("__inst__tool").is_err());
        assert!(decode_tool_name("server__inst__").is_err());
    }

    #[test]
    fn test_encode_special_chars() {
        let encoded = encode_tool_name("my-server", Some("code-ep"), "file.read");
        let (st, inst, t) = decode_tool_name(&encoded).unwrap();
        assert_eq!(st, "my-server");
        assert_eq!(inst.as_deref(), Some("code-ep"));
        assert_eq!(t, "file.read");
    }

    #[test]
    fn test_sanitize_name_basic() {
        assert_eq!(sanitize_name("echo-mcp"), Some("echo-mcp".to_string()));
    }

    #[test]
    fn test_sanitize_name_spaces() {
        assert_eq!(
            sanitize_name("My MCP Server"),
            Some("my_mcp_server".to_string())
        );
    }

    #[test]
    fn test_sanitize_name_special_chars() {
        assert_eq!(
            sanitize_name("server@v2.0!"),
            Some("serverv20".to_string())
        );
    }

    #[test]
    fn test_sanitize_name_uppercase() {
        assert_eq!(
            sanitize_name("MyServer"),
            Some("myserver".to_string())
        );
    }

    #[test]
    fn test_sanitize_name_empty() {
        assert_eq!(sanitize_name(""), None);
    }

    #[test]
    fn test_sanitize_name_only_special_chars() {
        assert_eq!(sanitize_name("@#$%^&*"), None);
    }

    #[test]
    fn test_sanitize_name_unicode() {
        // Unicode chars are stripped; only ASCII alphanumeric, underscore, dash kept
        assert_eq!(sanitize_name("café"), Some("caf".to_string()));
        assert_eq!(sanitize_name("日本語"), None);
    }

    #[test]
    fn test_sanitize_name_mixed() {
        assert_eq!(
            sanitize_name("My Server - v2.0 (beta)"),
            Some("my_server_-_v20_beta".to_string())
        );
    }

    #[test]
    fn test_sanitize_name_hyphens_underscores() {
        assert_eq!(
            sanitize_name("my-server_name"),
            Some("my-server_name".to_string())
        );
    }

    #[test]
    fn test_sanitize_name_digits() {
        assert_eq!(sanitize_name("server123"), Some("server123".to_string()));
    }
}
