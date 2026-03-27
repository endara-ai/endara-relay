use std::fmt;

/// Error type for prefix operations.
#[derive(Debug)]
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

/// Encode a tool name with machine and endpoint prefixes.
///
/// Format: `machine__endpoint__tool`
pub fn encode_tool_name(machine: &str, endpoint: &str, tool: &str) -> String {
    format!("{}__{}__{}", machine, endpoint, tool)
}

/// Decode a prefixed tool name into (machine, endpoint, tool) components.
///
/// Only the first two `__` segments are treated as machine and endpoint.
/// Everything after the second `__` is the tool name (which may itself contain `__`).
pub fn decode_tool_name(prefixed: &str) -> Result<(String, String, String), PrefixError> {
    let mut parts = prefixed.splitn(3, "__");

    let machine = parts
        .next()
        .ok_or_else(|| PrefixError::InvalidFormat("empty string".to_string()))?;

    let endpoint = parts
        .next()
        .ok_or_else(|| PrefixError::InvalidFormat(format!("missing endpoint segment in '{}'", prefixed)))?;

    let tool = parts
        .next()
        .ok_or_else(|| PrefixError::InvalidFormat(format!("missing tool segment in '{}'", prefixed)))?;

    if machine.is_empty() || endpoint.is_empty() || tool.is_empty() {
        return Err(PrefixError::InvalidFormat(format!(
            "empty segment in '{}'",
            prefixed
        )));
    }

    Ok((machine.to_string(), endpoint.to_string(), tool.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_basic() {
        assert_eq!(
            encode_tool_name("laptop", "vscode", "read_file"),
            "laptop__vscode__read_file"
        );
    }

    #[test]
    fn test_decode_basic() {
        let (m, e, t) = decode_tool_name("laptop__vscode__read_file").unwrap();
        assert_eq!(m, "laptop");
        assert_eq!(e, "vscode");
        assert_eq!(t, "read_file");
    }

    #[test]
    fn test_roundtrip() {
        let encoded = encode_tool_name("my-machine", "my-endpoint", "my-tool");
        let (m, e, t) = decode_tool_name(&encoded).unwrap();
        assert_eq!(m, "my-machine");
        assert_eq!(e, "my-endpoint");
        assert_eq!(t, "my-tool");
    }

    #[test]
    fn test_tool_name_with_double_underscore() {
        // Tool name itself contains "__" — only first two segments are machine/endpoint
        let encoded = encode_tool_name("m", "e", "tool__with__underscores");
        assert_eq!(encoded, "m__e__tool__with__underscores");
        let (m, e, t) = decode_tool_name(&encoded).unwrap();
        assert_eq!(m, "m");
        assert_eq!(e, "e");
        assert_eq!(t, "tool__with__underscores");
    }

    #[test]
    fn test_decode_missing_segments() {
        assert!(decode_tool_name("only_one").is_err());
        assert!(decode_tool_name("machine__endpoint").is_err());
    }

    #[test]
    fn test_decode_empty_segments() {
        assert!(decode_tool_name("__endpoint__tool").is_err());
        assert!(decode_tool_name("machine____tool").is_err());
        assert!(decode_tool_name("machine__endpoint__").is_err());
    }

    #[test]
    fn test_encode_special_chars() {
        let encoded = encode_tool_name("my-laptop", "code-server", "file.read");
        let (m, e, t) = decode_tool_name(&encoded).unwrap();
        assert_eq!(m, "my-laptop");
        assert_eq!(e, "code-server");
        assert_eq!(t, "file.read");
    }
}

