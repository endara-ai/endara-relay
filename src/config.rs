use crate::prefix;
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
    #[serde(default)]
    pub token_dir: Option<String>,
}

/// Transport type for an endpoint.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Stdio,
    Sse,
    Http,
    Oauth,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Transport::Stdio => write!(f, "stdio"),
            Transport::Sse => write!(f, "sse"),
            Transport::Http => write!(f, "http"),
            Transport::Oauth => write!(f, "oauth"),
        }
    }
}

/// Configuration for a single MCP endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EndpointConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_prefix: Option<String>,
    pub transport: Transport,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub url: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    #[serde(default)]
    pub oauth_server_url: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    #[serde(default)]
    pub token_endpoint: Option<String>,
}

impl EndpointConfig {
    /// Returns the tool prefix to use for this endpoint.
    ///
    /// Priority: explicit `tool_prefix` field → `sanitize_name(name)`.
    /// Returns `None` if both fail (e.g. unicode-only name with no tool_prefix).
    pub fn resolved_tool_prefix(&self) -> Option<String> {
        if let Some(ref tp) = self.tool_prefix {
            Some(tp.clone())
        } else {
            prefix::sanitize_name(&self.name)
        }
    }
}

/// Custom PartialEq that ignores `disabled` and `disabled_tools` so that
/// `diff_configs` treats an endpoint as "unchanged" when only these fields differ.
/// Note: `headers` IS included — changing headers should trigger adapter restart.
/// OAuth fields are included — changing OAuth config should trigger adapter restart.
impl PartialEq for EndpointConfig {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.tool_prefix == other.tool_prefix
            && self.transport == other.transport
            && self.command == other.command
            && self.args == other.args
            && self.url == other.url
            && self.env == other.env
            && self.headers == other.headers
            && self.oauth_server_url == other.oauth_server_url
            && self.client_id == other.client_id
            && self.client_secret == other.client_secret
            && self.scopes == other.scopes
    }
}

/// A per-endpoint validation warning. These do NOT prevent the relay from starting;
/// instead the endpoint is registered as a `FailedAdapter`.
#[derive(Debug, Clone)]
pub struct EndpointValidationWarning {
    /// The endpoint name (as written in the config).
    pub endpoint_name: String,
    /// Human-readable description of the problem.
    pub message: String,
}

impl fmt::Display for EndpointValidationWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Endpoint '{}': {}", self.endpoint_name, self.message)
    }
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
            token_dir: None,
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
#[allow(dead_code)]
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let resolved = expand_tilde(path);
    let contents = std::fs::read_to_string(&resolved)?;
    parse_and_validate(&contents)
}

/// Parse TOML string, resolve env vars, and validate.
#[allow(dead_code)]
pub fn parse_and_validate(contents: &str) -> Result<Config, ConfigError> {
    let mut config: Config = toml::from_str(contents)?;
    resolve_env_vars(&mut config)?;
    validate(&config)?;
    Ok(config)
}

/// Parse TOML string, resolve env vars, and validate **gracefully**.
///
/// Fatal errors (TOML syntax, missing `[relay]`) still return `Err`.
/// Per-endpoint issues (invalid name, missing command/url, duplicate names,
/// env var resolution failures) are collected as warnings. The returned
/// `Config` contains **all** endpoints (valid and invalid). The caller is
/// expected to register invalid endpoints as `FailedAdapter`.
pub fn parse_and_validate_graceful(
    contents: &str,
) -> Result<(Config, Vec<EndpointValidationWarning>), ConfigError> {
    let mut config: Config = toml::from_str(contents)?;
    let mut warnings = Vec::new();

    // Resolve env vars per-endpoint, collecting failures as warnings
    resolve_env_vars_graceful(&mut config, &mut warnings);

    // Validate per-endpoint, collecting failures as warnings
    validate_graceful(&config, &mut warnings);

    Ok((config, warnings))
}

/// Load, parse, resolve env vars, and validate a config file **gracefully**.
///
/// Same semantics as [`parse_and_validate_graceful`] but reads from a file path.
pub fn load_config_graceful(
    path: &Path,
) -> Result<(Config, Vec<EndpointValidationWarning>), ConfigError> {
    let resolved = expand_tilde(path);
    let contents = std::fs::read_to_string(&resolved)?;
    parse_and_validate_graceful(&contents)
}

/// Resolve environment variables in endpoint env maps and header values.
#[allow(dead_code)]
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
        if let Some(ref mut headers_map) = endpoint.headers {
            let mut resolved = HashMap::new();
            for (key, value) in headers_map.iter() {
                let resolved_value = resolve_header_value(value, &endpoint.name)?;
                resolved.insert(key.clone(), resolved_value);
            }
            *headers_map = resolved;
        }
    }
    Ok(())
}

/// Like [`resolve_env_vars`] but collects failures as warnings instead of
/// returning an error. Endpoints with env resolution failures are left with
/// their original (unresolved) values.
fn resolve_env_vars_graceful(config: &mut Config, warnings: &mut Vec<EndpointValidationWarning>) {
    for endpoint in &mut config.endpoints {
        let mut env_failed = false;
        if let Some(ref mut env_map) = endpoint.env {
            let mut resolved = HashMap::new();
            for (key, value) in env_map.iter() {
                match resolve_env_value(value, &endpoint.name) {
                    Ok(v) => {
                        resolved.insert(key.clone(), v);
                    }
                    Err(e) => {
                        env_failed = true;
                        warnings.push(EndpointValidationWarning {
                            endpoint_name: endpoint.name.clone(),
                            message: e.to_string(),
                        });
                        break;
                    }
                }
            }
            if !env_failed {
                *env_map = resolved;
            }
        }

        if !env_failed {
            if let Some(ref mut headers_map) = endpoint.headers {
                let mut resolved = HashMap::new();
                for (key, value) in headers_map.iter() {
                    match resolve_header_value(value, &endpoint.name) {
                        Ok(v) => {
                            resolved.insert(key.clone(), v);
                        }
                        Err(e) => {
                            warnings.push(EndpointValidationWarning {
                                endpoint_name: endpoint.name.clone(),
                                message: e.to_string(),
                            });
                            break;
                        }
                    }
                }
                if warnings.iter().all(|w| w.endpoint_name != endpoint.name) {
                    *headers_map = resolved;
                }
            }
        }
    }
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

/// Resolve a header value string, supporting embedded `$VAR` references.
/// - `$$` → literal `$`
/// - `$VAR` within any position → replaced with the env var value
/// - Supports mixed text like `Bearer $TOKEN`
fn resolve_header_value(value: &str, endpoint_name: &str) -> Result<String, ConfigError> {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.peek() == Some(&'$') {
                chars.next(); // consume second $
                result.push('$');
            } else {
                // Collect the variable name (alphanumeric + underscore)
                let mut var_name = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        var_name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if var_name.is_empty() {
                    result.push('$');
                } else {
                    match std::env::var(&var_name) {
                        Ok(val) => result.push_str(&val),
                        Err(_) => {
                            tracing::warn!(
                                var = %var_name,
                                endpoint = %endpoint_name,
                                "Environment variable not found in header value"
                            );
                            return Err(ConfigError::EnvVarMissing {
                                var_name,
                                endpoint: endpoint_name.to_string(),
                            });
                        }
                    }
                }
            }
        } else {
            result.push(ch);
        }
    }
    Ok(result)
}

/// Returns `true` if `name` matches the allowed endpoint name pattern:
/// starts with `[a-z0-9]`, followed by zero or more `[a-z0-9_-]`.
fn is_valid_endpoint_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl Config {
    /// Validate the config, collecting **all** errors instead of stopping at the first.
    ///
    /// Each endpoint must have a resolvable tool_prefix (either explicitly set or
    /// auto-sanitized from the name) that matches `^[a-z0-9][a-z0-9_-]*$` and is
    /// unique across all endpoints.
    ///
    /// Returns `Ok(())` when valid, or `Err(Vec<String>)` with every validation
    /// error found.
    #[allow(dead_code)]
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors: Vec<String> = Vec::new();
        let mut seen_prefixes = std::collections::HashSet::new();

        for ep in &self.endpoints {
            if ep.name.is_empty() {
                errors.push("Endpoint name must not be empty".to_string());
            }

            // Validate resolved tool_prefix
            match ep.resolved_tool_prefix() {
                None => {
                    errors.push(format!(
                        "Endpoint '{}': name cannot be sanitized into a valid tool prefix. Set 'tool_prefix' explicitly.",
                        ep.name
                    ));
                }
                Some(ref tp) if !is_valid_endpoint_name(tp) => {
                    errors.push(format!(
                        "Endpoint '{}': tool_prefix '{}' must match ^[a-z0-9][a-z0-9_-]*$ (lowercase alphanumeric, hyphens, underscores; must start with letter or digit)",
                        ep.name, tp
                    ));
                }
                Some(ref tp) => {
                    if !seen_prefixes.insert(tp.clone()) {
                        errors.push(format!(
                            "Duplicate tool_prefix '{}' (from endpoint '{}')",
                            tp, ep.name
                        ));
                    }
                }
            }

            match ep.transport {
                Transport::Stdio => {
                    if ep.command.is_none() || ep.command.as_deref() == Some("") {
                        errors.push(format!(
                            "Endpoint '{}': stdio transport requires a 'command' field",
                            ep.name
                        ));
                    }
                    if ep.headers.as_ref().is_some_and(|h| !h.is_empty()) {
                        tracing::warn!(
                            endpoint = %ep.name,
                            "Headers are set on a stdio transport endpoint and will be ignored"
                        );
                    }
                }
                Transport::Sse | Transport::Http => {
                    if ep.url.is_none() || ep.url.as_deref() == Some("") {
                        errors.push(format!(
                            "Endpoint '{}': {} transport requires a 'url' field",
                            ep.name, ep.transport
                        ));
                    }
                }
                Transport::Oauth => {
                    if ep.url.is_none() || ep.url.as_deref() == Some("") {
                        errors.push(format!(
                            "Endpoint '{}': oauth transport requires a 'url' field",
                            ep.name
                        ));
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Validate the parsed config (internal wrapper for backward compatibility).
#[allow(dead_code)]
fn validate(config: &Config) -> Result<(), ConfigError> {
    config
        .validate()
        .map_err(|errors| ConfigError::ValidationError(errors.join("; ")))
}

/// Per-endpoint validation that collects warnings instead of erroring.
///
/// Checks: resolved tool_prefix validity, missing command/url, duplicate prefixes.
/// First occurrence of a duplicate wins; subsequent duplicates are warned.
fn validate_graceful(config: &Config, warnings: &mut Vec<EndpointValidationWarning>) {
    let mut seen_prefixes = std::collections::HashSet::new();

    for ep in &config.endpoints {
        // Check if this endpoint already has an env-var warning (skip further checks)
        let already_warned = warnings.iter().any(|w| w.endpoint_name == ep.name);

        if ep.name.is_empty() {
            warnings.push(EndpointValidationWarning {
                endpoint_name: ep.name.clone(),
                message: "Endpoint name must not be empty".to_string(),
            });
        }

        // Validate resolved tool_prefix
        match ep.resolved_tool_prefix() {
            None => {
                warnings.push(EndpointValidationWarning {
                    endpoint_name: ep.name.clone(),
                    message: format!(
                        "Endpoint '{}': name cannot be sanitized into a valid tool prefix. Set 'tool_prefix' explicitly.",
                        ep.name
                    ),
                });
            }
            Some(ref tp) if !is_valid_endpoint_name(tp) => {
                warnings.push(EndpointValidationWarning {
                    endpoint_name: ep.name.clone(),
                    message: format!(
                        "Endpoint '{}': tool_prefix '{}' is invalid: must match ^[a-z0-9][a-z0-9_-]*$",
                        ep.name, tp
                    ),
                });
            }
            Some(ref tp) => {
                if !seen_prefixes.insert(tp.clone()) {
                    warnings.push(EndpointValidationWarning {
                        endpoint_name: ep.name.clone(),
                        message: format!(
                            "Duplicate tool_prefix '{}' (from endpoint '{}')",
                            tp, ep.name
                        ),
                    });
                }
            }
        }

        // Only check transport requirements if not already warned (env var issue)
        if !already_warned {
            match ep.transport {
                Transport::Stdio => {
                    if ep.command.is_none() || ep.command.as_deref() == Some("") {
                        warnings.push(EndpointValidationWarning {
                            endpoint_name: ep.name.clone(),
                            message: "stdio transport requires a 'command' field".to_string(),
                        });
                    }
                }
                Transport::Sse | Transport::Http => {
                    if ep.url.is_none() || ep.url.as_deref() == Some("") {
                        warnings.push(EndpointValidationWarning {
                            endpoint_name: ep.name.clone(),
                            message: format!("{} transport requires a 'url' field", ep.transport),
                        });
                    }
                }
                Transport::Oauth => {
                    if ep.url.is_none() || ep.url.as_deref() == Some("") {
                        warnings.push(EndpointValidationWarning {
                            endpoint_name: ep.name.clone(),
                            message: "oauth transport requires a 'url' field".to_string(),
                        });
                    }
                }
            }
        }
    }
}

/// Helper: get the set of endpoint names that have validation warnings.
pub fn warned_endpoint_names(
    warnings: &[EndpointValidationWarning],
) -> std::collections::HashSet<String> {
    warnings.iter().map(|w| w.endpoint_name.clone()).collect()
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

    // --- Endpoint name format validation tests ---

    #[test]
    fn valid_endpoint_names() {
        // These names are all directly valid tool_prefix values
        for name in &[
            "echo",
            "my-server",
            "test_ep",
            "a",
            "0day",
            "abc-123",
            "a-b_c",
        ] {
            let toml_str = format!(
                r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "{}"
transport = "stdio"
command = "echo"
"#,
                name
            );
            assert!(
                parse_and_validate(&toml_str).is_ok(),
                "Expected '{}' to be a valid endpoint name",
                name
            );
        }
    }

    #[test]
    fn freeform_name_uppercase_is_valid() {
        // Freeform names: "MyServer" sanitizes to "myserver" which is valid
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "MyServer"
transport = "stdio"
command = "echo"
"#;
        assert!(parse_and_validate(toml_str).is_ok());
    }

    #[test]
    fn freeform_name_with_spaces_is_valid() {
        // "my server" sanitizes to "my_server" which is valid
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "my server"
transport = "stdio"
command = "echo"
"#;
        assert!(parse_and_validate(toml_str).is_ok());
    }

    #[test]
    fn invalid_endpoint_name_starts_with_hyphen() {
        // "-bad" sanitizes to "-bad" which starts with hyphen → invalid
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "-bad"
transport = "stdio"
command = "echo"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(
                    msg.contains("-bad"),
                    "Error should mention the prefix: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn unicode_only_name_fails() {
        // Unicode-only name with no sanitizable ASCII chars → tool_prefix is None
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "日本語"
transport = "stdio"
command = "echo"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(
                    msg.contains("cannot be sanitized"),
                    "Error should mention sanitization: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn explicit_tool_prefix_overrides_name() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "日本語サーバー"
tool_prefix = "japanese_server"
transport = "stdio"
command = "echo"
"#;
        assert!(parse_and_validate(toml_str).is_ok());
    }

    #[test]
    fn validate_collects_multiple_errors() {
        // "_also_bad" sanitizes to "_also_bad" which starts with underscore → invalid
        let config = make_config(vec![
            stdio_ep("_also_bad", "echo"),
            stdio_ep("-starts-with-hyphen", "cat"),
        ]);
        let errors = config.validate().unwrap_err();
        assert_eq!(errors.len(), 2, "Should have 2 errors: {:?}", errors);
        assert!(errors[0].contains("_also_bad"));
        assert!(errors[1].contains("-starts-with-hyphen"));
    }

    #[test]
    fn validate_empty_config_is_ok() {
        let config = make_config(vec![]);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_reports_duplicates() {
        let config = make_config(vec![stdio_ep("good", "echo"), stdio_ep("good", "cat")]);
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("Duplicate")));
    }

    #[test]
    fn validate_duplicate_resolved_prefix() {
        // "My Server" and "my_server" both resolve to "my_server"
        let config = make_config(vec![
            stdio_ep("My Server", "echo"),
            stdio_ep("my_server", "cat"),
        ]);
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("Duplicate tool_prefix")));
    }

    // --- Config diff tests ---

    fn make_config(endpoints: Vec<EndpointConfig>) -> Config {
        Config {
            relay: RelayConfig {
                machine_name: "test".to_string(),
                local_js_execution: None,
                token_dir: None,
            },
            endpoints,
        }
    }

    fn stdio_ep(name: &str, cmd: &str) -> EndpointConfig {
        EndpointConfig {
            name: name.to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Stdio,
            command: Some(cmd.to_string()),
            args: None,
            url: None,
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: None,
            client_id: None,
            client_secret: None,
            scopes: None,
            token_endpoint: None,
        }
    }

    fn sse_ep(name: &str, url: &str) -> EndpointConfig {
        EndpointConfig {
            name: name.to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Sse,
            command: None,
            args: None,
            url: Some(url.to_string()),
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: None,
            client_id: None,
            client_secret: None,
            scopes: None,
            token_endpoint: None,
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

    #[test]
    fn parse_headers_on_http_endpoint() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "remote"
transport = "http"
url = "http://localhost:4000/mcp"
headers = { Authorization = "Bearer my-token", X-Custom = "value" }
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let headers = config.endpoints[0].headers.as_ref().unwrap();
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer my-token");
        assert_eq!(headers.get("X-Custom").unwrap(), "value");
    }

    #[test]
    fn headers_env_var_resolution() {
        std::env::set_var("TEST_HEADER_TOKEN_12345", "secret-value");
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "remote"
transport = "sse"
url = "http://localhost:3000/sse"
headers = { Authorization = "Bearer $TEST_HEADER_TOKEN_12345" }
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let headers = config.endpoints[0].headers.as_ref().unwrap();
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer secret-value");
        std::env::remove_var("TEST_HEADER_TOKEN_12345");
    }

    #[test]
    fn headers_change_triggers_config_diff() {
        let mut ep1 = sse_ep("remote", "http://localhost:3000/sse");
        ep1.headers = Some(HashMap::from([("Auth".to_string(), "old".to_string())]));

        let mut ep2 = sse_ep("remote", "http://localhost:3000/sse");
        ep2.headers = Some(HashMap::from([("Auth".to_string(), "new".to_string())]));

        let old = make_config(vec![ep1]);
        let new = make_config(vec![ep2]);
        let diff = diff_configs(&old, &new);
        assert_eq!(diff.changed.len(), 1);
        assert!(diff.unchanged.is_empty());
    }

    // --- Graceful validation tests ---

    #[test]
    fn graceful_freeform_name_is_valid() {
        // "Sequential Thinking" sanitizes to "sequential_thinking" — now valid
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "Sequential Thinking"
transport = "stdio"
command = "echo"
"#;
        let (config, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        assert!(
            warnings.is_empty(),
            "Expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn graceful_unicode_only_name_returns_warning() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "日本語"
transport = "stdio"
command = "echo"
"#;
        let (config, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("cannot be sanitized"));
    }

    #[test]
    fn graceful_mixed_valid_and_invalid_endpoints() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "good-endpoint"
transport = "stdio"
command = "echo"

[[endpoints]]
name = "日本語"
transport = "stdio"
command = "cat"

[[endpoints]]
name = "another-good"
transport = "stdio"
command = "ls"
"#;
        let (config, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert_eq!(config.endpoints.len(), 3);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].endpoint_name, "日本語");
        let warned = warned_endpoint_names(&warnings);
        assert!(warned.contains("日本語"));
        assert!(!warned.contains("good-endpoint"));
        assert!(!warned.contains("another-good"));
    }

    #[test]
    fn graceful_missing_command_returns_warning() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "no-cmd"
transport = "stdio"
"#;
        let (_, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("command"));
    }

    #[test]
    fn graceful_missing_url_returns_warning() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "no-url"
transport = "sse"
"#;
        let (_, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("url"));
    }

    #[test]
    fn graceful_duplicate_names_warned() {
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
        let (config, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert_eq!(config.endpoints.len(), 2);
        assert!(warnings.iter().any(|w| w.message.contains("Duplicate")));
    }

    #[test]
    fn graceful_toml_syntax_error_still_fatal() {
        let toml_str = "this is not valid toml [[[";
        assert!(parse_and_validate_graceful(toml_str).is_err());
    }

    #[test]
    fn graceful_missing_relay_section_still_fatal() {
        let toml_str = r#"
[[endpoints]]
name = "test"
transport = "stdio"
command = "echo"
"#;
        assert!(parse_and_validate_graceful(toml_str).is_err());
    }

    #[test]
    fn graceful_no_warnings_for_valid_config() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "echo"
transport = "stdio"
command = "echo"
"#;
        let (config, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        assert!(warnings.is_empty());
    }

    // --- OAuth transport tests ---

    #[test]
    fn parse_oauth_transport_with_all_fields() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "oauth-ep"
transport = "oauth"
url = "http://localhost:5000/mcp"
client_id = "my-client-id"
client_secret = "my-secret"
oauth_server_url = "https://auth.example.com"
scopes = ["read", "write"]
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        let ep = &config.endpoints[0];
        assert_eq!(ep.transport, Transport::Oauth);
        assert_eq!(ep.url.as_deref(), Some("http://localhost:5000/mcp"));
        assert_eq!(ep.client_id.as_deref(), Some("my-client-id"));
        assert_eq!(ep.client_secret.as_deref(), Some("my-secret"));
        assert_eq!(
            ep.oauth_server_url.as_deref(),
            Some("https://auth.example.com")
        );
        assert_eq!(
            ep.scopes.as_deref(),
            Some(&["read".to_string(), "write".to_string()][..])
        );
    }

    #[test]
    fn oauth_missing_client_id_is_valid() {
        // client_id is now optional — can be auto-registered via DCR
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "oauth-ep"
transport = "oauth"
url = "http://localhost:5000/mcp"
oauth_server_url = "https://auth.example.com"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.endpoints[0].client_id, None);
    }

    #[test]
    fn oauth_missing_server_url_is_valid() {
        // oauth_server_url is now optional — can be auto-discovered via RFC 9728
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "oauth-ep"
transport = "oauth"
url = "http://localhost:5000/mcp"
client_id = "my-client-id"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.endpoints[0].oauth_server_url, None);
    }

    #[test]
    fn oauth_no_fields_reports_missing_url_only() {
        // Only url is required for OAuth transport now
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "oauth-ep"
transport = "oauth"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(msg.contains("url"), "Error should mention url: {}", msg);
                assert!(
                    !msg.contains("client_id"),
                    "Error should NOT mention client_id: {}",
                    msg
                );
                assert!(
                    !msg.contains("oauth_server_url"),
                    "Error should NOT mention oauth_server_url: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn oauth_scopes_and_client_secret_are_optional() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "oauth-ep"
transport = "oauth"
url = "http://localhost:5000/mcp"
client_id = "my-client-id"
oauth_server_url = "https://auth.example.com"
"#;
        // Should succeed without scopes and client_secret
        let config = parse_and_validate(toml_str).unwrap();
        assert!(config.endpoints[0].scopes.is_none());
        assert!(config.endpoints[0].client_secret.is_none());
    }

    #[test]
    fn oauth_client_secret_optional_some() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "oauth-ep"
transport = "oauth"
url = "http://localhost:5000/mcp"
client_id = "my-client-id"
client_secret = "s3cret"
oauth_server_url = "https://auth.example.com"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.endpoints[0].client_secret.as_deref(), Some("s3cret"));
    }

    #[test]
    fn oauth_scopes_parses_as_vec() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "oauth-ep"
transport = "oauth"
url = "http://localhost:5000/mcp"
client_id = "my-client-id"
oauth_server_url = "https://auth.example.com"
scopes = ["openid", "profile", "email"]
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let scopes = config.endpoints[0].scopes.as_ref().unwrap();
        assert_eq!(
            scopes,
            &vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string()
            ]
        );
    }

    #[test]
    fn oauth_field_change_triggers_config_diff() {
        let ep1 = EndpointConfig {
            name: "oauth-ep".to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Oauth,
            command: None,
            args: None,
            url: Some("http://localhost:5000/mcp".to_string()),
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: Some("https://auth.example.com".to_string()),
            client_id: Some("old-client".to_string()),
            client_secret: None,
            scopes: None,
            token_endpoint: None,
        };

        let mut ep2 = ep1.clone();
        ep2.client_id = Some("new-client".to_string());

        let old = make_config(vec![ep1]);
        let new = make_config(vec![ep2]);
        let diff = diff_configs(&old, &new);
        assert_eq!(
            diff.changed.len(),
            1,
            "OAuth field change should trigger diff"
        );
        assert!(diff.unchanged.is_empty());
    }
}
