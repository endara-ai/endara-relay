use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Top-level configuration structure.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub relay: RelayConfig,
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,
}

/// Relay-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RelayConfig {
    pub machine_name: String,
    #[serde(default)]
    pub local_js_execution: Option<bool>,
}

/// Transport type for an endpoint.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Stdio,
    Sse,
    Http,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Transport::Stdio => write!(f, "stdio"),
            Transport::Sse => write!(f, "sse"),
            Transport::Http => write!(f, "http"),
        }
    }
}

/// Configuration for a single MCP endpoint.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EndpointConfig {
    pub name: String,
    pub transport: Transport,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub url: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

/// Errors that can occur during config loading.
#[derive(Debug)]
pub enum ConfigError {
    IoError(std::io::Error),
    ParseError(toml::de::Error),
    EnvVarMissing { var_name: String, endpoint: String },
    ValidationError(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::IoError(e) => write!(f, "IO error: {}", e),
            ConfigError::ParseError(e) => write!(f, "TOML parse error: {}", e),
            ConfigError::EnvVarMissing { var_name, endpoint } => {
                write!(
                    f,
                    "Environment variable '{}' not found for endpoint '{}'",
                    var_name, endpoint
                )
            }
            ConfigError::ValidationError(msg) => write!(f, "Validation error: {}", msg),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::IoError(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::ParseError(e)
    }
}

/// Expand `~` prefix to the user's home directory.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") || s == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(s.strip_prefix("~/").unwrap_or(""));
        }
    }
    path.to_path_buf()
}

/// Create a default configuration with the system hostname and no endpoints.
pub fn default_config() -> Config {
    let machine_name = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());

    Config {
        relay: RelayConfig {
            machine_name,
            local_js_execution: None,
        },
        endpoints: Vec::new(),
    }
}

/// Write a default config file to the given path, creating parent directories as needed.
pub fn create_default_config_file(path: &Path) -> Result<Config, ConfigError> {
    let resolved = expand_tilde(path);
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let config = default_config();
    let toml_str = toml::to_string_pretty(&config).map_err(|e| {
        ConfigError::ValidationError(format!("Failed to serialize default config: {}", e))
    })?;
    std::fs::write(&resolved, &toml_str)?;
    Ok(config)
}

/// Load, parse, resolve env vars, and validate a config file.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let resolved = expand_tilde(path);
    let contents = std::fs::read_to_string(&resolved)?;
    parse_and_validate(&contents)
}

/// Parse TOML string, resolve env vars, and validate.
pub fn parse_and_validate(contents: &str) -> Result<Config, ConfigError> {
    let mut config: Config = toml::from_str(contents)?;
    resolve_env_vars(&mut config)?;
    validate(&config)?;
    Ok(config)
}

/// Resolve environment variables in endpoint env maps.
fn resolve_env_vars(config: &mut Config) -> Result<(), ConfigError> {
    for endpoint in &mut config.endpoints {
        if let Some(ref mut env_map) = endpoint.env {
            let mut resolved = HashMap::new();
            for (key, value) in env_map.iter() {
                let resolved_value = resolve_env_value(value, &endpoint.name)?;
                resolved.insert(key.clone(), resolved_value);
            }
            *env_map = resolved;
        }
    }
    Ok(())
}

/// Resolve a single env value string.
/// - `$$` prefix → literal `$` (rest of string kept as-is)
/// - `$VAR` → look up VAR in process environment
/// - anything else → kept as-is
fn resolve_env_value(value: &str, endpoint_name: &str) -> Result<String, ConfigError> {
    if let Some(rest) = value.strip_prefix("$$") {
        Ok(format!("${}", rest))
    } else if let Some(var_name) = value.strip_prefix('$') {
        if var_name.is_empty() {
            return Ok(value.to_string());
        }
        match std::env::var(var_name) {
            Ok(val) => Ok(val),
            Err(_) => {
                tracing::warn!(
                    var = var_name,
                    endpoint = endpoint_name,
                    "Environment variable not found"
                );
                Err(ConfigError::EnvVarMissing {
                    var_name: var_name.to_string(),
                    endpoint: endpoint_name.to_string(),
                })
            }
        }
    } else {
        Ok(value.to_string())
    }
}

/// Validate the parsed config.
fn validate(config: &Config) -> Result<(), ConfigError> {
    let mut seen_names = std::collections::HashSet::new();
    for ep in &config.endpoints {
        if ep.name.is_empty() {
            return Err(ConfigError::ValidationError(
                "Endpoint name must not be empty".to_string(),
            ));
        }
        if !seen_names.insert(&ep.name) {
            return Err(ConfigError::ValidationError(format!(
                "Duplicate endpoint name: '{}'",
                ep.name
            )));
        }
        match ep.transport {
            Transport::Stdio => {
                if ep.command.is_none() || ep.command.as_deref() == Some("") {
                    return Err(ConfigError::ValidationError(format!(
                        "Endpoint '{}': stdio transport requires a 'command' field",
                        ep.name
                    )));
                }
            }
            Transport::Sse | Transport::Http => {
                if ep.url.is_none() || ep.url.as_deref() == Some("") {
                    return Err(ConfigError::ValidationError(format!(
                        "Endpoint '{}': {} transport requires a 'url' field",
                        ep.name, ep.transport
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Result of comparing two configs to determine what changed.
#[derive(Debug, Clone)]
pub struct ConfigDiff {
    /// Endpoints present in new config but not in old.
    pub added: Vec<EndpointConfig>,
    /// Names of endpoints present in old config but not in new.
    pub removed: Vec<String>,
    /// Endpoints present in both but with different settings (name, new config).
    pub changed: Vec<(String, EndpointConfig)>,
    /// Names of endpoints that are identical in both configs.
    pub unchanged: Vec<String>,
}

/// Compare two configs and produce a diff of endpoint changes.
///
/// Endpoints are matched by name. An endpoint is "changed" if any of its
/// fields (transport, command, args, url, env) differ.
pub fn diff_configs(old: &Config, new: &Config) -> ConfigDiff {
    use std::collections::HashMap;

    let old_map: HashMap<&str, &EndpointConfig> =
        old.endpoints.iter().map(|e| (e.name.as_str(), e)).collect();
    let new_map: HashMap<&str, &EndpointConfig> =
        new.endpoints.iter().map(|e| (e.name.as_str(), e)).collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();

    // Check new endpoints: added or changed
    for (name, new_ep) in &new_map {
        match old_map.get(name) {
            None => added.push((*new_ep).clone()),
            Some(old_ep) => {
                if *old_ep == *new_ep {
                    unchanged.push(name.to_string());
                } else {
                    changed.push((name.to_string(), (*new_ep).clone()));
                }
            }
        }
    }

    // Check for removed endpoints
    for name in old_map.keys() {
        if !new_map.contains_key(name) {
            removed.push(name.to_string());
        }
    }

    ConfigDiff {
        added,
        removed,
        changed,
        unchanged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let toml_str = std::fs::read_to_string("tests/fixtures/minimal.toml").unwrap();
        let config = parse_and_validate(&toml_str).unwrap();
        assert_eq!(config.relay.machine_name, "test");
        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(config.endpoints[0].name, "echo");
        assert_eq!(config.endpoints[0].transport, Transport::Stdio);
        assert_eq!(config.endpoints[0].command.as_deref(), Some("echo"));
    }

    #[test]
    fn parse_all_transports() {
        let toml_str = r#"
[relay]
machine_name = "dev"

[[endpoints]]
name = "local"
transport = "stdio"
command = "cat"
args = ["-"]

[[endpoints]]
name = "remote-sse"
transport = "sse"
url = "http://localhost:3000/sse"

[[endpoints]]
name = "remote-http"
transport = "http"
url = "http://localhost:4000/mcp"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.endpoints.len(), 3);
        assert_eq!(config.endpoints[0].transport, Transport::Stdio);
        assert_eq!(config.endpoints[1].transport, Transport::Sse);
        assert_eq!(config.endpoints[2].transport, Transport::Http);
    }

    #[test]
    fn env_var_resolution() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "test-ep"
transport = "stdio"
command = "echo"
env = { HOME_VAL = "$HOME" }
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let env = config.endpoints[0].env.as_ref().unwrap();
        let expected = std::env::var("HOME").unwrap();
        assert_eq!(env.get("HOME_VAL").unwrap(), &expected);
    }

    #[test]
    fn env_var_escape() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "test-ep"
transport = "stdio"
command = "echo"
env = { LITERAL = "$$HOME" }
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let env = config.endpoints[0].env.as_ref().unwrap();
        assert_eq!(env.get("LITERAL").unwrap(), "$HOME");
    }

    #[test]
    fn missing_env_var() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "test-ep"
transport = "stdio"
command = "echo"
env = { TOKEN = "$DEFINITELY_NOT_A_REAL_ENV_VAR_12345" }
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::EnvVarMissing { var_name, endpoint } => {
                assert_eq!(var_name, "DEFINITELY_NOT_A_REAL_ENV_VAR_12345");
                assert_eq!(endpoint, "test-ep");
            }
            other => panic!("Expected EnvVarMissing, got: {:?}", other),
        }
    }

    #[test]
    fn missing_command_for_stdio() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "bad"
transport = "stdio"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(msg.contains("stdio"), "Error should mention stdio: {}", msg);
                assert!(
                    msg.contains("command"),
                    "Error should mention command: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn missing_url_for_sse() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "bad"
transport = "sse"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(msg.contains("sse"), "Error should mention sse: {}", msg);
                assert!(msg.contains("url"), "Error should mention url: {}", msg);
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn duplicate_endpoint_names() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "dup"
transport = "stdio"
command = "echo"

[[endpoints]]
name = "dup"
transport = "stdio"
command = "cat"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(
                    msg.contains("Duplicate"),
                    "Error should mention duplicate: {}",
                    msg
                );
                assert!(
                    msg.contains("dup"),
                    "Error should mention the name: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn empty_config_is_valid() {
        let toml_str = r#"
[relay]
machine_name = "test"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.machine_name, "test");
        assert!(config.endpoints.is_empty());
    }

    // --- Config diff tests ---

    fn make_config(endpoints: Vec<EndpointConfig>) -> Config {
        Config {
            relay: RelayConfig {
                machine_name: "test".to_string(),
                local_js_execution: None,
            },
            endpoints,
        }
    }

    fn stdio_ep(name: &str, cmd: &str) -> EndpointConfig {
        EndpointConfig {
            name: name.to_string(),
            transport: Transport::Stdio,
            command: Some(cmd.to_string()),
            args: None,
            url: None,
            env: None,
        }
    }

    fn sse_ep(name: &str, url: &str) -> EndpointConfig {
        EndpointConfig {
            name: name.to_string(),
            transport: Transport::Sse,
            command: None,
            args: None,
            url: Some(url.to_string()),
            env: None,
        }
    }

    #[test]
    fn config_diff_added_removed_changed_unchanged() {
        let old = make_config(vec![
            stdio_ep("keep", "echo"),
            stdio_ep("remove_me", "cat"),
            stdio_ep("change_me", "old_cmd"),
        ]);
        let new = make_config(vec![
            stdio_ep("keep", "echo"),         // unchanged
            stdio_ep("change_me", "new_cmd"), // changed
            sse_ep("new_ep", "http://x"),     // added
        ]);

        let diff = diff_configs(&old, &new);
        assert_eq!(diff.unchanged, vec!["keep"]);
        assert_eq!(diff.removed, vec!["remove_me"]);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].0, "change_me");
        assert_eq!(diff.changed[0].1.command, Some("new_cmd".to_string()));
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].name, "new_ep");
    }

    #[test]
    fn config_diff_no_changes() {
        let cfg = make_config(vec![stdio_ep("a", "echo"), stdio_ep("b", "cat")]);
        let diff = diff_configs(&cfg, &cfg);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
        assert_eq!(diff.unchanged.len(), 2);
    }

    #[test]
    fn config_diff_all_different() {
        let old = make_config(vec![stdio_ep("a", "echo"), stdio_ep("b", "cat")]);
        let new = make_config(vec![stdio_ep("c", "ls"), sse_ep("d", "http://y")]);
        let diff = diff_configs(&old, &new);
        assert_eq!(diff.added.len(), 2);
        assert_eq!(diff.removed.len(), 2);
        assert!(diff.changed.is_empty());
        assert!(diff.unchanged.is_empty());
    }
}
