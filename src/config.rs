use crate::prefix;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Top-level configuration structure.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub relay: RelayConfig,
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,
    /// Named subsets of `endpoints` served at `/mcp/{path}` with their own
    /// JS-execution and TOON-output toggles. `None` and `Some(empty)` are
    /// equivalent: no profiles configured. Per recon §D1, the TOML key is
    /// plural `[[profiles]]` to match the existing `[[endpoints]]` convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<ProfileConfig>>,
    /// Named identity-provider organizations (END-19). Each `[[organizations]]`
    /// block carries a stable `name` (used in endpoint `auth.organization` refs
    /// and the credential pool key), a provider template id, and the resolved
    /// IdP issuer URL. Tokens are NEVER stored here — only in the credential
    /// store. `#[serde(default)]` keeps pre-existing configs (no org blocks)
    /// parsing unchanged.
    #[serde(default)]
    pub organizations: Vec<ConfigOrganization>,
}

/// Relay-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RelayConfig {
    pub machine_name: String,
    #[serde(default)]
    pub local_js_execution: Option<bool>,
    #[serde(default)]
    pub token_dir: Option<String>,
    /// Permit `http://` URLs and loopback / link-local addresses for OAuth
    /// discovery and Dynamic Client Registration. Defaults to `false`
    /// (HTTPS-only, public addresses only) to mitigate SSRF and
    /// confused-deputy attacks against the host running the relay.
    #[serde(default)]
    pub allow_insecure_oauth: Option<bool>,
    /// Convert JSON tool-call responses to TOON (Token-Oriented Object
    /// Notation) before they reach the MCP client. Defaults to `true` when
    /// omitted; set to `false` (or pass `--no-toon` on the command line) to
    /// restore raw JSON pass-through.
    #[serde(default)]
    pub toon_output: Option<bool>,
    /// How long `main()` waits for background adapter initializations to
    /// settle before binding the MCP TCP listener on port 9400. When `None`
    /// the effective default is 60 seconds. A value of `0` skips the wait
    /// entirely: the MCP TCP listener binds immediately after the management
    /// socket and adapters keep initializing in the background.
    #[serde(default)]
    pub startup_init_timeout_secs: Option<u64>,
    /// Maximum number of cached `Mcp-Session-Id` → client-identity entries
    /// held by the inbound dispatch (see `server.rs`). `None` defers to
    /// [`DEFAULT_SESSION_IDENTITY_MAX_SESSIONS`]. When the cache is full an
    /// LRU eviction drops the least-recently-used entry; the per-request
    /// `User-Agent` / `Origin` fallback still produces a best-effort caller
    /// label so evicted sessions never block dispatch.
    #[serde(default)]
    pub session_identity_max_sessions: Option<usize>,
    /// Validate `tools/call` arguments against each tool's JSON Schema
    /// `inputSchema` at the relay before forwarding to the upstream MCP
    /// server. Defaults to `true` when omitted; set to `false` to bypass the
    /// validation layer entirely (escape hatch for servers with deliberately
    /// loose schemas). Hot-reloadable via [`crate::watcher::ConfigWatcher`].
    #[serde(default)]
    pub validate_inputs: Option<bool>,
    /// Agent-call observability store configuration (`[relay.observability]`).
    /// `#[serde(default)]` means a `config.toml` that omits the table entirely
    /// falls back to [`ObservabilityConfig::default`].
    #[serde(default)]
    pub observability: ObservabilityConfig,
    /// Number of days to retain daily-rotated relay log files. When `None`,
    /// defaults to 7 days at runtime. When `Some(0)`, log pruning is disabled
    /// entirely. Cleanup runs at startup; long-running relays accumulate logs
    /// until restarted.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_retention_days: Option<u32>,
    /// Allowlist of directories the JS sandbox's `writeFile` may write into.
    /// Each entry must be an absolute path (a leading `~/` is expanded to the
    /// user's home directory). Relative entries are a hard validation error.
    /// Entries that do not exist or are not directories are warned about and
    /// skipped at resolution time (see [`resolve_write_roots`]) — the relay
    /// never creates them. `None`/empty means writing is disabled entirely.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_dirs: Option<Vec<PathBuf>>,
}

impl Default for RelayConfig {
    /// Mirror [`default_config`]'s relay defaults: resolve `machine_name` from
    /// the system hostname (falling back to `"unknown"`) and leave every
    /// optional field unset. This is what `#[serde(default)]` on `Config::relay`
    /// uses when a `config.toml` omits the `[relay]` table entirely.
    fn default() -> Self {
        let machine_name = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string());

        RelayConfig {
            machine_name,
            local_js_execution: None,
            token_dir: None,
            allow_insecure_oauth: None,
            toon_output: None,
            startup_init_timeout_secs: None,
            session_identity_max_sessions: None,
            validate_inputs: None,
            observability: ObservabilityConfig::default(),
            log_retention_days: None,
            write_dirs: None,
        }
    }
}

/// Agent-call observability store configuration, nested under
/// `[relay.observability]`. Controls the durable metadata store (SQLite) and
/// the in-memory payload ring buffer. Every field carries a `#[serde(default
/// = "…")]` so a config that includes the `[relay.observability]` table but
/// omits individual keys keeps the documented defaults; the struct-level
/// `#[serde(default)]` on `RelayConfig::observability` covers a fully-omitted
/// table. This keeps configs that predate the feature working unchanged.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ObservabilityConfig {
    /// Master switch for the subsystem. When `false`, no metadata rows are
    /// recorded and no payloads are buffered. Default: `true`.
    #[serde(default = "default_observability_enabled")]
    pub enabled: bool,
    /// Capture full request/response payloads into the in-memory ring buffer.
    /// Metadata is still recorded when this is `false`. Default: `true`.
    #[serde(default = "default_observability_store_payloads")]
    pub store_payloads: bool,
    /// How long (minutes) payloads stay retrievable in the ring buffer before
    /// they expire. Default: `10`.
    #[serde(default = "default_observability_payload_window_minutes")]
    pub payload_window_minutes: u64,
    /// How long (days) metadata rows are retained before eviction. Default: `7`.
    #[serde(default = "default_observability_record_retention_days")]
    pub record_retention_days: u64,
    /// Maximum on-disk size (MB) of the metadata database; oldest rows are
    /// evicted first once the cap is reached. Default: `1024` (1 GB).
    #[serde(default = "default_observability_max_db_size_mb")]
    pub max_db_size_mb: u64,
    /// Maximum bytes captured per payload; larger payloads are truncated and
    /// flagged. Default: `262144` (256 KB).
    #[serde(default = "default_observability_max_payload_bytes")]
    pub max_payload_bytes: u64,
    /// Total memory budget (MB) for the payload ring buffer. Default: `128`.
    #[serde(default = "default_observability_payload_buffer_budget_mb")]
    pub payload_buffer_budget_mb: u64,
}

fn default_observability_enabled() -> bool {
    true
}
fn default_observability_store_payloads() -> bool {
    true
}
fn default_observability_payload_window_minutes() -> u64 {
    10
}
fn default_observability_record_retention_days() -> u64 {
    7
}
fn default_observability_max_db_size_mb() -> u64 {
    1024
}
fn default_observability_max_payload_bytes() -> u64 {
    262144
}
fn default_observability_payload_buffer_budget_mb() -> u64 {
    128
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        ObservabilityConfig {
            enabled: default_observability_enabled(),
            store_payloads: default_observability_store_payloads(),
            payload_window_minutes: default_observability_payload_window_minutes(),
            record_retention_days: default_observability_record_retention_days(),
            max_db_size_mb: default_observability_max_db_size_mb(),
            max_payload_bytes: default_observability_max_payload_bytes(),
            payload_buffer_budget_mb: default_observability_payload_buffer_budget_mb(),
        }
    }
}

/// Effective default for `RelayConfig::startup_init_timeout_secs` when the
/// field is omitted from `config.toml`.
pub const DEFAULT_STARTUP_INIT_TIMEOUT_SECS: u64 = 60;

/// Effective default for `RelayConfig::session_identity_max_sessions` when
/// the field is omitted from `config.toml`. Caps the inbound session →
/// `ClientIdentity` cache so a misbehaving client that never reuses its
/// `Mcp-Session-Id` cannot grow the map unboundedly.
pub const DEFAULT_SESSION_IDENTITY_MAX_SESSIONS: usize = 1000;

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

/// Reserved profile paths — would collide with existing relay routes
/// (`/mcp/sse`, `/mcp/initialize`, `/mcp/tools/...`, `/oauth/...`, `/healthz`).
/// Matched case-insensitively per spec §2.3.
pub const RESERVED_PROFILE_PATHS: &[&str] = &["sse", "initialize", "tools", "oauth", "healthz"];

/// Configuration for a named endpoint profile.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileConfig {
    /// Human-friendly display name (freeform string).
    pub name: String,
    /// URL path segment — the profile is served at `/mcp/{path}`. Must
    /// satisfy [`validate_profile_path`].
    pub path: String,
    /// Endpoint names (matching [`EndpointConfig::name`]) included in this
    /// profile. Order is cosmetic — the merged catalog is sorted by the
    /// registry layer.
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// Per-profile JS-execution mode. When `true`, this profile's
    /// `/mcp/{path}` exposes the meta-tools (`list_tools`, `search_tools`,
    /// `execute_tools`) and hides direct tool calls. Required: the loader
    /// rejects configs whose profile block omits this field.
    pub js_execution: bool,
    /// Per-profile TOON output. When `true`, tool responses on this profile
    /// are converted to TOON. Required: the loader rejects configs whose
    /// profile block omits this field.
    pub toon_output: bool,
}

/// Validate a profile path slug.
///
/// A valid path:
/// 1. Matches `^[a-zA-Z0-9][a-zA-Z0-9_-]*$` (starts alphanumeric, then
///    alphanumeric / underscore / hyphen).
/// 2. Is not in [`RESERVED_PROFILE_PATHS`] (case-insensitive).
///
/// Returns `Err(message)` on failure; the message is suitable for surfacing
/// to the user.
pub fn validate_profile_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("profile path must not be empty".into());
    }
    static PATH_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = PATH_RE.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_-]*$").unwrap());
    if !re.is_match(path) {
        return Err(format!(
            "profile path '{}' contains invalid characters — \
             use letters, digits, hyphens, or underscores (must start with a letter or digit)",
            path
        ));
    }
    if RESERVED_PROFILE_PATHS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(path))
    {
        return Err(format!("profile path '{}' is reserved by the relay", path));
    }
    Ok(())
}

/// Validate the top-level `profiles` block.
///
/// Fail-fast: any invalid path, duplicate path (case-insensitive), duplicate
/// profile name, or reference to an unknown endpoint becomes a hard startup
/// error per spec §2.4. Empty `endpoints` lists are allowed (a profile may
/// exist with no members and simply serve an empty catalog).
pub fn validate_profiles(config: &Config) -> Result<(), Vec<String>> {
    let profiles = match &config.profiles {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(()),
    };

    let mut errors: Vec<String> = Vec::new();
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    let endpoint_names: std::collections::HashSet<&str> =
        config.endpoints.iter().map(|e| e.name.as_str()).collect();

    for profile in profiles {
        if profile.name.trim().is_empty() {
            errors.push("Profile name must not be empty".to_string());
        } else if !seen_names.insert(profile.name.clone()) {
            errors.push(format!("Duplicate profile name: '{}'", profile.name));
        }

        if let Err(msg) = validate_profile_path(&profile.path) {
            errors.push(format!("Profile '{}': {}", profile.name, msg));
        } else {
            let path_key = profile.path.to_ascii_lowercase();
            if !seen_paths.insert(path_key) {
                errors.push(format!(
                    "Duplicate profile path '{}' (paths are case-insensitive)",
                    profile.path
                ));
            }
        }

        for ep_name in &profile.endpoints {
            if !endpoint_names.contains(ep_name.as_str()) {
                errors.push(format!(
                    "Profile '{}' references unknown endpoint '{}'",
                    profile.name, ep_name
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate `relay.write_dirs`: every entry must be an absolute path after
/// tilde expansion. Fail-fast like [`validate_profiles`] — a relative entry
/// is a configuration error, never a warning. Existence is deliberately NOT
/// checked here; missing/non-directory entries are warned about and skipped
/// at resolution time by [`resolve_write_roots`].
pub fn validate_write_dirs(config: &Config) -> Result<(), Vec<String>> {
    let dirs = match &config.relay.write_dirs {
        Some(d) if !d.is_empty() => d,
        _ => return Ok(()),
    };

    let errors: Vec<String> = dirs
        .iter()
        .filter(|d| !expand_tilde(d).is_absolute())
        .map(|d| {
            format!(
                "relay.write_dirs entry '{}' must be an absolute path",
                d.display()
            )
        })
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// A named identity-provider organization (`[[organizations]]`, END-19).
///
/// Organizations are the stable identity source EMA endpoints reference via
/// `auth.organization`, replacing END-18's provisional per-endpoint `idp`
/// field. The `name` is the stable key used both in endpoint references and as
/// the credential-pool key (Wave 2). `provider` is a template id
/// (`okta|entra|google|ping|custom`) and `idp` is the resolved issuer URL.
///
/// **Tokens are NEVER part of this struct** — the ID token / refresh token live
/// only in the credential store (`TokenManager`). Only `name`/`provider`/`idp`
/// are ever serialized into `config.toml`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ConfigOrganization {
    /// Stable key used in endpoint `auth.organization` refs and the credential
    /// pool. Human-readable (e.g. `"Acme Corp"`).
    pub name: String,
    /// Provider template id: `okta`, `entra`, `google`, `ping`, or `custom`.
    pub provider: String,
    /// Resolved IdP issuer URL (built from a provider template or pasted).
    pub idp: String,
    /// Optional pre-registered OAuth `client_id` for this org's IdP (e.g. an
    /// Okta/Entra app registration). When present it is used verbatim across the
    /// authorize URL and every EMA token leg; when absent the relay falls back to
    /// the shared resolution chain (CIMD → DCR) and the legs keep sending the
    /// hosted CIMD `client_id`. Omitted from `config.toml` when unset so existing
    /// configs round-trip unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// Authentication configuration nested under `[endpoints.auth]`.
///
/// Currently the only supported `type` is `"ema"` (Enterprise-Managed
/// Authorization). An EMA block references an org via `organization` (the
/// preferred END-19 form) and carries a `resource` (the MCP server URL the
/// minted access token is scoped to). The `idp` field is a **DEPRECATED**
/// back-compat seam from END-18: a bare `idp` issuer URL (with no
/// `organization`) still validates so existing configs keep working, but new
/// configs should reference an `[[organizations]]` entry by name instead.
///
/// `organization`/`idp`/`resource` are modeled as `Option` (rather than via a
/// serde-tagged enum) so that a missing field surfaces as a clear
/// [`ConfigError::ValidationError`] / per-endpoint warning through the existing
/// validation paths instead of a raw TOML parse error, preserving the graceful
/// per-endpoint loading model.
///
/// Client credentials are intentionally **not** modeled here, and the two
/// credential kinds live at different scopes (never in `config.toml`):
///   * the requesting `client_secret` is genuinely org-level (the shared
///     SSO/requesting app) — persisted in `{org}.dcr.json` (0600) keyed by org
///     name, captured via `POST`/`PUT /api/organizations`.
///   * the optional EMA **resource** credential pair
///     (`resource_client_id`/`resource_client_secret`, presented at the MAS in
///     Step 3) is **per-resource**, so R3 keys it by **endpoint** —  persisted in
///     `{name}.dcr.json` (0600) and captured via
///     `POST /api/endpoints/{name}/credentials`, never on the org record.
///
/// Both are loaded at adapter init.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EndpointAuthConfig {
    /// Discriminator for the auth scheme. Only `"ema"` is currently supported.
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Name of the `[[organizations]]` entry this EMA endpoint authenticates
    /// against (the preferred END-19 form). The IdP issuer is resolved from the
    /// named org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// IdP issuer URL for `type = "ema"` (e.g. `https://acme.okta.com`).
    /// **DEPRECATED** END-18 back-compat seam — prefer `organization`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idp: Option<String>,
    /// Target MCP server URL the EMA access token is minted for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
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
    /// **Legacy** — read on adapter init only for backwards compatibility with
    /// existing `config.toml` files. New callers should write client credentials
    /// via `POST /api/endpoints/{name}/credentials`, which persists them through
    /// the `TokenManager` (DCR file) instead of TOML.
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    /// Optional override for the advertised `server_type` name. When set,
    /// this value is sanitized through `sanitize_server_name` and used in
    /// place of the upstream-derived name in the `instructions` field and
    /// meta-tool descriptions. The auto-strip of `-mcp-server` and friends
    /// is **never** applied to overrides — the user is taken at face value.
    /// Tool-name routing (the `tool_prefix`) is unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_type_override: Option<String>,
    /// Process isolation mode for stdio endpoints: `"container"` or `"none"`.
    /// Omitted means `"none"` (direct spawn), so pre-existing configs keep
    /// working unchanged on upgrade; the desktop writes an explicit
    /// `isolation = "container"` for newly created endpoints.
    /// Ignored for non-stdio transports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
    /// OCI image used when `isolation = "container"`. Defaults to
    /// `ghcr.io/endara-ai/mcp-runner:latest` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_image: Option<String>,
    /// Host bind mounts (`"/host/path:/container/path"`) applied when
    /// `isolation = "container"`. Default: no host filesystem access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<String>>,
    /// Optional `[endpoints.auth]` block. Present only for endpoints using a
    /// non-default auth scheme (currently `type = "ema"`). Omitted for ordinary
    /// stdio/sse/http/oauth endpoints, keeping pre-existing configs unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<EndpointAuthConfig>,
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
/// Isolation fields (`isolation`, `container_image`, `mounts`) are included —
/// changing them should trigger adapter restart.
/// The `auth` block is included — changing an EMA endpoint's IdP/resource (or
/// adding/removing the block) should trigger adapter restart.
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
            && self.server_type_override == other.server_type_override
            && self.isolation == other.isolation
            && self.container_image == other.container_image
            && self.mounts == other.mounts
            && self.auth == other.auth
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

/// Expand `~` prefix to the user's home directory. `HOME` takes precedence
/// (tests override it); `dirs::home_dir()` covers platforms where `HOME`
/// is unset, e.g. Windows.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") || s == "~" {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir);
        if let Some(home) = home {
            return home.join(s.strip_prefix("~/").unwrap_or(""));
        }
    }
    path.to_path_buf()
}

/// Resolve `relay.write_dirs` into the effective allowlist of write roots.
///
/// Each configured entry is tilde-expanded and canonicalized. Entries that do
/// not exist or are not directories are **warned about and skipped** — the
/// relay never creates them (locked design decision: warn-and-skip, no
/// auto-creation). Canonicalization resolves symlinks so later path-containment
/// checks against these roots cannot be escaped via a symlinked root.
///
/// Returns the (possibly empty) list of canonical, existing directory roots.
pub fn resolve_write_roots(config: &Config) -> Vec<PathBuf> {
    let dirs = match &config.relay.write_dirs {
        Some(d) if !d.is_empty() => d,
        _ => return Vec::new(),
    };

    let mut roots = Vec::new();
    for dir in dirs {
        let expanded = expand_tilde(dir);
        match std::fs::canonicalize(&expanded) {
            Ok(canonical) if canonical.is_dir() => roots.push(canonical),
            Ok(_) => {
                tracing::warn!(
                    path = %expanded.display(),
                    "relay.write_dirs entry is not a directory; skipping"
                );
            }
            Err(e) => {
                tracing::warn!(
                    path = %expanded.display(),
                    error = %e,
                    "relay.write_dirs entry does not exist or is inaccessible; skipping (directories are never auto-created)"
                );
            }
        }
    }
    roots
}

/// Create a default configuration with the system hostname and no endpoints.
pub fn default_config() -> Config {
    Config {
        relay: RelayConfig::default(),
        endpoints: Vec::new(),
        profiles: None,
        organizations: Vec::new(),
    }
}

/// Write `contents` to `path` and tighten permissions to 0o600 on Unix.
///
/// This is the preferred helper for any callsite that writes `config.toml`, so
/// that secrets that legacy callers may still place in TOML are at least
/// protected at rest. The mode change is best-effort: if it fails it is
/// surfaced as the underlying io error.
pub fn write_config_file(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
    write_config_file(&resolved, &toml_str)?;
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
    validate_profiles(&config).map_err(|errors| ConfigError::ValidationError(errors.join("; ")))?;
    validate_write_dirs(&config)
        .map_err(|errors| ConfigError::ValidationError(errors.join("; ")))?;
    Ok(config)
}

/// Parse TOML string, resolve env vars, and validate **gracefully**.
///
/// Fatal errors (TOML syntax) still return `Err`. A missing `[relay]` table is
/// tolerated: it falls back to [`RelayConfig::default`] (hostname-derived
/// `machine_name`), so a desktop-scaffolded config without `[relay]` still
/// starts.
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

    // Profile validation is fail-fast (per spec §2.4): invalid paths,
    // duplicate paths, and missing endpoint refs are hard startup errors,
    // never per-endpoint warnings.
    validate_profiles(&config).map_err(|errors| ConfigError::ValidationError(errors.join("; ")))?;

    // write_dirs validation is also fail-fast: a relative entry is a
    // configuration error, mirroring profile validation above.
    validate_write_dirs(&config)
        .map_err(|errors| ConfigError::ValidationError(errors.join("; ")))?;

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

/// Validate an endpoint's `[endpoints.auth]` block.
///
/// Returns `Err(message)` for an unknown `type`, or — for `type = "ema"` — a
/// missing/empty `resource`, or neither an `organization` nor a bare `idp`. An
/// EMA endpoint is valid when it has `resource` AND (`organization` OR `idp`);
/// a bare `idp` (no `organization`) still validates for END-18 back-compat but
/// is deprecated in favor of an `[[organizations]]` reference. Returns `Ok(())`
/// when there is no auth block or it is valid. The message is suitable for
/// surfacing to the user (it is prefixed with the endpoint name by callers).
pub(crate) fn validate_endpoint_auth(ep: &EndpointConfig) -> Result<(), String> {
    let auth = match &ep.auth {
        Some(a) => a,
        None => return Ok(()),
    };
    match auth.auth_type.as_str() {
        "ema" => {
            if auth.resource.as_deref().unwrap_or("").trim().is_empty() {
                return Err(
                    "auth.type = \"ema\" requires a non-empty 'resource' (MCP server URL)"
                        .to_string(),
                );
            }
            let has_org = !auth.organization.as_deref().unwrap_or("").trim().is_empty();
            let has_idp = !auth.idp.as_deref().unwrap_or("").trim().is_empty();
            if !has_org && !has_idp {
                return Err(
                    "auth.type = \"ema\" requires an 'organization' (the name of an \
                     [[organizations]] entry) or a bare 'idp' (IdP issuer URL; deprecated)"
                        .to_string(),
                );
            }
            Ok(())
        }
        other => Err(format!(
            "unknown auth.type '{}' — supported values: \"ema\"",
            other
        )),
    }
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

            if let Err(msg) = validate_endpoint_auth(ep) {
                errors.push(format!("Endpoint '{}': {}", ep.name, msg));
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

            // Validate the isolation mode (relevant for stdio, but any invalid
            // value is worth flagging regardless of transport).
            if let Some(ref iso) = ep.isolation {
                if iso != "container" && iso != "none" {
                    warnings.push(EndpointValidationWarning {
                        endpoint_name: ep.name.clone(),
                        message: format!(
                            "invalid isolation value '{}' — expected \"container\" or \"none\"",
                            iso
                        ),
                    });
                }
            }

            // Validate the `[endpoints.auth]` block (currently `type = "ema"`).
            if let Err(msg) = validate_endpoint_auth(ep) {
                warnings.push(EndpointValidationWarning {
                    endpoint_name: ep.name.clone(),
                    message: msg,
                });
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
    /// Retained for tests (e.g. `tests/hot_reload_integration.rs`); no
    /// production reader remains after the watcher's unchanged-loop removal.
    #[allow(dead_code)]
    pub unchanged: Vec<String>,
    /// `true` when the `[[organizations]]` set differs between the two configs
    /// (any org added, removed, or changed). EMA endpoints resolve their IdP
    /// through their named org, so an org change must participate in the
    /// hot-reload path just like an endpoint change. Wave 2 wires the watcher to
    /// act on this; it is surfaced here so the diff is the single source of
    /// truth for what changed.
    #[allow(dead_code)]
    pub organizations_changed: bool,
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
        organizations_changed: old.organizations != new.organizations,
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

    #[test]
    fn missing_relay_section_uses_default() {
        // A config that omits `[relay]` entirely (e.g. the desktop's default
        // scaffolded config) must still parse, falling back to the
        // hostname-derived default `machine_name`. Unknown tables such as
        // `[desktop.overlay]` and `[meta]` are ignored by `Config`.
        let toml_str = r#"
[desktop.overlay]
enabled = true

[meta]
version = 1
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert!(!config.relay.machine_name.is_empty());
        assert!(config.endpoints.is_empty());
    }

    #[test]
    fn startup_init_timeout_defaults_to_none() {
        let toml_str = r#"
[relay]
machine_name = "test"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.startup_init_timeout_secs, None);
    }

    #[test]
    fn startup_init_timeout_zero_parses() {
        let toml_str = r#"
[relay]
machine_name = "test"
startup_init_timeout_secs = 0
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.startup_init_timeout_secs, Some(0));
    }

    #[test]
    fn startup_init_timeout_nonzero_parses() {
        let toml_str = r#"
[relay]
machine_name = "test"
startup_init_timeout_secs = 5
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.startup_init_timeout_secs, Some(5));
    }

    #[test]
    fn session_identity_max_sessions_defaults_to_none() {
        let toml_str = r#"
[relay]
machine_name = "test"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.session_identity_max_sessions, None);
    }

    #[test]
    fn session_identity_max_sessions_nonzero_parses() {
        let toml_str = r#"
[relay]
machine_name = "test"
session_identity_max_sessions = 250
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.session_identity_max_sessions, Some(250));
    }

    #[test]
    fn session_identity_max_sessions_default_const_is_1000() {
        assert_eq!(DEFAULT_SESSION_IDENTITY_MAX_SESSIONS, 1000);
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

    // --- Profile validation tests (spec §11 rows #1–#6) ---

    /// §11 row #1 — valid paths.
    #[test]
    fn profile_path_accepts_valid() {
        for p in &["work", "my-project", "A1", "abc_123", "0", "a", "Project-2"] {
            assert!(
                validate_profile_path(p).is_ok(),
                "expected '{}' to be a valid profile path",
                p
            );
        }
    }

    /// §11 row #2 — invalid paths.
    #[test]
    fn profile_path_rejects_invalid_chars() {
        for p in &[
            "",
            "-leading-hyphen",
            "_leads_underscore",
            "has space",
            "café",
        ] {
            assert!(
                validate_profile_path(p).is_err(),
                "expected '{}' to be rejected",
                p
            );
        }
    }

    /// §11 row #3 — reserved paths (case-insensitive).
    #[test]
    fn profile_path_rejects_reserved() {
        for p in &[
            "sse",
            "SSE",
            "Sse",
            "initialize",
            "INITIALIZE",
            "tools",
            "Tools",
            "oauth",
            "OAuth",
            "healthz",
            "Healthz",
        ] {
            let err = validate_profile_path(p)
                .err()
                .unwrap_or_else(|| panic!("'{}' should be reserved", p));
            assert!(
                err.contains("reserved"),
                "error for '{}' should mention 'reserved': {}",
                p,
                err
            );
        }
    }

    /// §11 row #4 — TOML with `[[profiles]]` parses correctly.
    #[test]
    fn profile_toml_parses() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "gmail"
transport = "stdio"
command = "echo"

[[endpoints]]
name = "linear"
transport = "stdio"
command = "cat"

[[profiles]]
name = "Work"
path = "work"
endpoints = ["gmail", "linear"]
js_execution = true
toon_output = true

[[profiles]]
name = "Personal"
path = "personal"
endpoints = ["gmail"]
js_execution = false
toon_output = false
"#;
        let config = parse_and_validate(toml_str).expect("config should parse");
        let profiles = config.profiles.expect("profiles should be present");
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].name, "Work");
        assert_eq!(profiles[0].path, "work");
        assert_eq!(profiles[0].endpoints, vec!["gmail", "linear"]);
        assert!(profiles[0].js_execution);
        assert!(profiles[0].toon_output);
        assert!(!profiles[1].js_execution);
        assert!(!profiles[1].toon_output);
    }

    /// §11 row #5 — profile referencing a non-existent endpoint is a startup error.
    #[test]
    fn profile_missing_endpoint_ref_errors() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "gmail"
transport = "stdio"
command = "echo"

[[profiles]]
name = "Work"
path = "work"
endpoints = ["gmail", "ghost"]
js_execution = false
toon_output = true
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(
                    msg.contains("ghost"),
                    "error should mention 'ghost': {}",
                    msg
                );
                assert!(
                    msg.to_lowercase().contains("unknown endpoint"),
                    "error should mention unknown endpoint: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    /// §11 row #6 — duplicate paths (case-insensitive) are a startup error.
    #[test]
    fn profile_duplicate_paths_error() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[profiles]]
name = "Lower"
path = "shared"
js_execution = false
toon_output = true

[[profiles]]
name = "Upper"
path = "SHARED"
js_execution = false
toon_output = true
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(
                    msg.to_lowercase().contains("duplicate profile path"),
                    "error should flag duplicate profile path: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    /// Graceful path also enforces profile validation as a hard error.
    #[test]
    fn profile_validation_is_hard_error_on_graceful_path() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[profiles]]
name = "Bad"
path = "sse"
js_execution = false
toon_output = true
"#;
        let err = parse_and_validate_graceful(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(
                    msg.contains("reserved"),
                    "error should mention reserved: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    /// Duplicate profile names are also caught.
    #[test]
    fn profile_duplicate_names_error() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[profiles]]
name = "Work"
path = "work-a"
js_execution = false
toon_output = true

[[profiles]]
name = "Work"
path = "work-b"
js_execution = false
toon_output = true
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(
                    msg.contains("Duplicate profile name"),
                    "error should flag duplicate name: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    /// Empty `endpoints` list on a profile is allowed.
    #[test]
    fn profile_with_no_endpoints_is_ok() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[profiles]]
name = "Empty"
path = "empty"
js_execution = false
toon_output = true
"#;
        let config = parse_and_validate(toml_str).expect("empty profile should be valid");
        assert_eq!(config.profiles.as_ref().unwrap()[0].endpoints.len(), 0);
    }

    /// A profile block missing `js_execution` is rejected at parse time —
    /// the loader treats the omission as a config error rather than silently
    /// falling back to a default.
    #[test]
    fn profile_missing_js_execution_is_parse_error() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[profiles]]
name = "Work"
path = "work"
toon_output = true
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ParseError(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("js_execution"),
                    "parse error should mention js_execution: {}",
                    msg
                );
            }
            other => panic!("Expected ParseError, got: {:?}", other),
        }
    }

    /// A profile block missing `toon_output` is rejected at parse time.
    #[test]
    fn profile_missing_toon_output_is_parse_error() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[profiles]]
name = "Work"
path = "work"
js_execution = false
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ParseError(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("toon_output"),
                    "parse error should mention toon_output: {}",
                    msg
                );
            }
            other => panic!("Expected ParseError, got: {:?}", other),
        }
    }

    // --- Config diff tests ---

    fn make_config(endpoints: Vec<EndpointConfig>) -> Config {
        Config {
            relay: RelayConfig {
                machine_name: "test".to_string(),
                local_js_execution: None,
                token_dir: None,
                allow_insecure_oauth: None,
                toon_output: None,
                startup_init_timeout_secs: None,
                session_identity_max_sessions: None,
                validate_inputs: None,
                observability: ObservabilityConfig::default(),
                log_retention_days: None,
                write_dirs: None,
            },
            endpoints,
            profiles: None,
            organizations: Vec::new(),
        }
    }

    // --- Observability config tests ---

    #[test]
    fn observability_default_impl_matches_documented_defaults() {
        let obs = ObservabilityConfig::default();
        assert!(obs.enabled);
        assert!(obs.store_payloads);
        assert_eq!(obs.payload_window_minutes, 10);
        assert_eq!(obs.record_retention_days, 7);
        assert_eq!(obs.max_db_size_mb, 1024);
        assert_eq!(obs.max_payload_bytes, 262144);
        assert_eq!(obs.payload_buffer_budget_mb, 128);
    }

    #[test]
    fn observability_defaults_when_section_omitted() {
        // No `[relay.observability]` table at all — the struct-level
        // `#[serde(default)]` on `RelayConfig::observability` applies.
        let toml_str = r#"
[relay]
machine_name = "test"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.observability, ObservabilityConfig::default());
    }

    #[test]
    fn observability_partial_section_keeps_other_defaults() {
        // Table present but only some keys set — omitted keys fall back to the
        // per-field `#[serde(default = "…")]` values, not bool/u64 zero values.
        let toml_str = r#"
[relay]
machine_name = "test"

[relay.observability]
enabled = false
payload_window_minutes = 30
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let obs = &config.relay.observability;
        assert!(!obs.enabled);
        assert_eq!(obs.payload_window_minutes, 30);
        assert!(obs.store_payloads);
        assert_eq!(obs.record_retention_days, 7);
        assert_eq!(obs.max_db_size_mb, 1024);
        assert_eq!(obs.max_payload_bytes, 262144);
        assert_eq!(obs.payload_buffer_budget_mb, 128);
    }

    #[test]
    fn observability_full_round_trip() {
        let original = ObservabilityConfig {
            enabled: false,
            store_payloads: false,
            payload_window_minutes: 42,
            record_retention_days: 3,
            max_db_size_mb: 512,
            max_payload_bytes: 1024,
            payload_buffer_budget_mb: 64,
        };
        let config = Config {
            relay: RelayConfig {
                observability: original.clone(),
                ..RelayConfig::default()
            },
            endpoints: vec![],
            profiles: None,
            organizations: Vec::new(),
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.relay.observability, original);
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
            server_type_override: None,
            isolation: None,
            container_image: None,
            mounts: None,
            auth: None,
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
            server_type_override: None,
            isolation: None,
            container_image: None,
            mounts: None,
            auth: None,
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
    fn graceful_missing_relay_section_uses_default() {
        // A missing `[relay]` table is no longer fatal: it falls back to
        // `RelayConfig::default()` so a desktop-scaffolded config still starts.
        let toml_str = r#"
[[endpoints]]
name = "test"
transport = "stdio"
command = "echo"
"#;
        let (config, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert!(!config.relay.machine_name.is_empty());
        assert_eq!(config.endpoints.len(), 1);
        assert!(warnings.is_empty());
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
            server_type_override: None,
            isolation: None,
            container_image: None,
            mounts: None,
            auth: None,
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

    // --- server_type_override tests ----------------------------------------

    #[test]
    fn parse_server_type_override_field() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "drive"
transport = "oauth"
url = "https://drivemcp.googleapis.com/mcp/v1"
server_type_override = "google-drive"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(
            config.endpoints[0].server_type_override.as_deref(),
            Some("google-drive")
        );
    }

    #[test]
    fn server_type_override_optional_defaults_to_none() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "echo"
transport = "stdio"
command = "echo"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert!(config.endpoints[0].server_type_override.is_none());
    }

    #[test]
    fn server_type_override_change_triggers_config_diff() {
        // Hot-reload requirement: changing only the override must restart
        // the adapter so the new advertised name takes effect.
        let mut ep1 = sse_ep("remote", "http://localhost:3000/sse");
        ep1.server_type_override = Some("old-name".to_string());
        let mut ep2 = sse_ep("remote", "http://localhost:3000/sse");
        ep2.server_type_override = Some("new-name".to_string());

        let old = make_config(vec![ep1]);
        let new = make_config(vec![ep2]);
        let diff = diff_configs(&old, &new);
        assert_eq!(
            diff.changed.len(),
            1,
            "server_type_override change should trigger diff"
        );
        assert!(diff.unchanged.is_empty());
    }

    #[test]
    fn server_type_override_added_or_removed_triggers_diff() {
        let ep1 = sse_ep("remote", "http://localhost:3000/sse");
        let mut ep2 = sse_ep("remote", "http://localhost:3000/sse");
        ep2.server_type_override = Some("override".to_string());

        let old = make_config(vec![ep1]);
        let new = make_config(vec![ep2]);
        let diff = diff_configs(&old, &new);
        assert_eq!(diff.changed.len(), 1);
    }

    // --- Isolation / container field tests ---

    #[test]
    fn isolation_fields_parse_from_toml() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "boxed"
transport = "stdio"
command = "npx"
isolation = "container"
container_image = "example.com/custom:1"
mounts = ["/host/a:/ctr/a"]
"#;
        let (config, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {:?}", warnings);
        let ep = &config.endpoints[0];
        assert_eq!(ep.isolation.as_deref(), Some("container"));
        assert_eq!(ep.container_image.as_deref(), Some("example.com/custom:1"));
        assert_eq!(ep.mounts, Some(vec!["/host/a:/ctr/a".to_string()]));
    }

    #[test]
    fn graceful_invalid_isolation_value_returns_warning() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "boxed"
transport = "stdio"
command = "echo"
isolation = "bogus"
"#;
        let (config, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("invalid isolation value"));
    }

    #[test]
    fn graceful_valid_isolation_values_no_warning() {
        for value in ["container", "none"] {
            let toml_str = format!(
                r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "boxed"
transport = "stdio"
command = "echo"
isolation = "{}"
"#,
                value
            );
            let (_, warnings) = parse_and_validate_graceful(&toml_str).unwrap();
            assert!(
                warnings.is_empty(),
                "unexpected warnings for '{}': {:?}",
                value,
                warnings
            );
        }
    }

    #[test]
    fn isolation_change_triggers_config_diff() {
        let mut ep1 = stdio_ep("boxed", "npx");
        ep1.isolation = Some("container".to_string());
        let mut ep2 = stdio_ep("boxed", "npx");
        ep2.isolation = Some("none".to_string());

        let old = make_config(vec![ep1]);
        let new = make_config(vec![ep2]);
        let diff = diff_configs(&old, &new);
        assert_eq!(
            diff.changed.len(),
            1,
            "isolation change should trigger diff"
        );
        assert!(diff.unchanged.is_empty());
    }

    #[test]
    fn container_image_change_triggers_config_diff() {
        let mut ep1 = stdio_ep("boxed", "npx");
        ep1.container_image = Some("example.com/a:1".to_string());
        let mut ep2 = stdio_ep("boxed", "npx");
        ep2.container_image = Some("example.com/b:2".to_string());

        let old = make_config(vec![ep1]);
        let new = make_config(vec![ep2]);
        let diff = diff_configs(&old, &new);
        assert_eq!(
            diff.changed.len(),
            1,
            "container_image change should trigger diff"
        );
        assert!(diff.unchanged.is_empty());
    }

    #[test]
    fn mounts_change_triggers_config_diff() {
        let ep1 = stdio_ep("boxed", "npx");
        let mut ep2 = stdio_ep("boxed", "npx");
        ep2.mounts = Some(vec!["/host:/ctr".to_string()]);

        let old = make_config(vec![ep1]);
        let new = make_config(vec![ep2]);
        let diff = diff_configs(&old, &new);
        assert_eq!(diff.changed.len(), 1, "mounts change should trigger diff");
        assert!(diff.unchanged.is_empty());
    }

    // --- EMA endpoint auth tests (M10) ---

    #[test]
    fn ema_endpoint_parses() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "ema"
idp = "https://acme.okta.com"
resource = "https://api.githubcopilot.com/mcp/"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let auth = config.endpoints[0].auth.as_ref().unwrap();
        assert_eq!(auth.auth_type, "ema");
        assert_eq!(auth.idp.as_deref(), Some("https://acme.okta.com"));
        assert_eq!(
            auth.resource.as_deref(),
            Some("https://api.githubcopilot.com/mcp/")
        );
    }

    #[test]
    fn ema_endpoint_round_trips_through_serialize() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "ema"
idp = "https://acme.okta.com"
resource = "https://api.githubcopilot.com/mcp/"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let serialized = toml::to_string_pretty(&config).unwrap();
        let reparsed = parse_and_validate(&serialized).unwrap();
        assert_eq!(
            reparsed.endpoints[0].auth, config.endpoints[0].auth,
            "EMA auth block should survive a serialize → parse round trip"
        );
    }

    #[test]
    fn ema_missing_resource_is_rejected() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "ema"
idp = "https://acme.okta.com"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(
                    msg.contains("resource"),
                    "Error should mention resource: {}",
                    msg
                );
                assert!(msg.contains("ema"), "Error should mention ema: {}", msg);
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn ema_missing_idp_is_rejected() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "ema"
resource = "https://api.githubcopilot.com/mcp/"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(msg.contains("idp"), "Error should mention idp: {}", msg);
                assert!(msg.contains("ema"), "Error should mention ema: {}", msg);
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn ema_empty_idp_is_rejected() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "ema"
idp = "   "
resource = "https://api.githubcopilot.com/mcp/"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(msg.contains("idp"), "Error should mention idp: {}", msg);
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn unknown_auth_type_is_rejected() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "saml"
idp = "https://acme.okta.com"
resource = "https://api.githubcopilot.com/mcp/"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(
                    msg.contains("unknown auth.type"),
                    "Error should mention unknown auth.type: {}",
                    msg
                );
                assert!(
                    msg.contains("saml"),
                    "Error should mention the value: {}",
                    msg
                );
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn non_ema_config_unaffected_by_auth_field() {
        // A config without any `[endpoints.auth]` block parses unchanged and
        // leaves `auth` as `None` (backward compatibility).
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "plain"
transport = "stdio"
command = "echo"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert!(config.endpoints[0].auth.is_none());
    }

    #[test]
    fn ema_missing_fields_surface_as_graceful_warning() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "ema"
idp = "https://acme.okta.com"
"#;
        let (config, warnings) = parse_and_validate_graceful(toml_str).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].endpoint_name, "github-acme");
        assert!(warnings[0].message.contains("resource"));
    }

    #[test]
    fn ema_auth_change_triggers_config_diff() {
        let mut ep1 = sse_ep("github-acme", "https://api.githubcopilot.com/mcp/");
        ep1.auth = Some(EndpointAuthConfig {
            auth_type: "ema".to_string(),
            organization: None,
            idp: Some("https://acme.okta.com".to_string()),
            resource: Some("https://api.githubcopilot.com/mcp/".to_string()),
        });
        let mut ep2 = sse_ep("github-acme", "https://api.githubcopilot.com/mcp/");
        ep2.auth = Some(EndpointAuthConfig {
            auth_type: "ema".to_string(),
            organization: None,
            idp: Some("https://other.okta.com".to_string()),
            resource: Some("https://api.githubcopilot.com/mcp/".to_string()),
        });

        let old = make_config(vec![ep1]);
        let new = make_config(vec![ep2]);
        let diff = diff_configs(&old, &new);
        assert_eq!(diff.changed.len(), 1, "EMA auth change should trigger diff");
        assert!(diff.unchanged.is_empty());
    }

    // --- [[organizations]] config tests (M1) ---

    #[test]
    fn organizations_parse_from_toml() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[organizations]]
name = "Acme Corp"
provider = "okta"
idp = "https://acme.okta.com"

[[organizations]]
name = "Globex"
provider = "entra"
idp = "https://login.microsoftonline.com/globex/v2.0"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.organizations.len(), 2);
        assert_eq!(config.organizations[0].name, "Acme Corp");
        assert_eq!(config.organizations[0].provider, "okta");
        assert_eq!(config.organizations[0].idp, "https://acme.okta.com");
        assert_eq!(config.organizations[1].name, "Globex");
        assert_eq!(config.organizations[1].provider, "entra");
    }

    #[test]
    fn organizations_default_to_empty_when_omitted() {
        let toml_str = r#"
[relay]
machine_name = "test"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert!(config.organizations.is_empty());
    }

    #[test]
    fn organizations_round_trip_and_never_carry_tokens() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[organizations]]
name = "Acme Corp"
provider = "okta"
idp = "https://acme.okta.com"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let serialized = toml::to_string_pretty(&config).unwrap();
        // The serialized org block must carry only name/provider/idp.
        assert!(serialized.contains("Acme Corp"));
        assert!(serialized.contains("okta"));
        assert!(serialized.contains("https://acme.okta.com"));
        // Tokens are stored only in the credential store, never in toml.
        assert!(
            !serialized.to_lowercase().contains("token"),
            "org toml must never carry tokens: {}",
            serialized
        );
        let reparsed = parse_and_validate(&serialized).unwrap();
        assert_eq!(
            reparsed.organizations, config.organizations,
            "organizations should survive a serialize → parse round trip"
        );
    }

    // --- EMA endpoint organization-ref tests (M3) ---

    #[test]
    fn ema_endpoint_with_organization_validates() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[organizations]]
name = "Acme Corp"
provider = "okta"
idp = "https://acme.okta.com"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "ema"
organization = "Acme Corp"
resource = "https://api.githubcopilot.com/mcp/"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let auth = config.endpoints[0].auth.as_ref().unwrap();
        assert_eq!(auth.organization.as_deref(), Some("Acme Corp"));
        assert!(auth.idp.is_none());
    }

    #[test]
    fn ema_endpoint_organization_round_trips_through_serialize() {
        let mut ep = sse_ep("github-acme", "https://api.githubcopilot.com/mcp/");
        ep.transport = Transport::Http;
        ep.auth = Some(EndpointAuthConfig {
            auth_type: "ema".to_string(),
            organization: Some("Acme Corp".to_string()),
            idp: None,
            resource: Some("https://api.githubcopilot.com/mcp/".to_string()),
        });
        let config = make_config(vec![ep]);
        let serialized = toml::to_string_pretty(&config).unwrap();
        let reparsed = parse_and_validate(&serialized).unwrap();
        assert_eq!(reparsed.endpoints[0].auth, config.endpoints[0].auth);
    }

    #[test]
    fn ema_endpoint_bare_idp_still_validates() {
        // END-18 back-compat: a bare `idp` with no `organization` keeps working.
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "ema"
idp = "https://acme.okta.com"
resource = "https://api.githubcopilot.com/mcp/"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        let auth = config.endpoints[0].auth.as_ref().unwrap();
        assert!(auth.organization.is_none());
        assert_eq!(auth.idp.as_deref(), Some("https://acme.okta.com"));
    }

    #[test]
    fn ema_endpoint_without_org_or_idp_is_rejected() {
        let toml_str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "github-acme"
url = "https://api.githubcopilot.com/mcp/"
transport = "http"

[endpoints.auth]
type = "ema"
resource = "https://api.githubcopilot.com/mcp/"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        match err {
            ConfigError::ValidationError(msg) => {
                assert!(msg.contains("ema"), "Error should mention ema: {}", msg);
                assert!(
                    msg.contains("organization"),
                    "Error should mention organization: {}",
                    msg
                );
                assert!(msg.contains("idp"), "Error should mention idp: {}", msg);
            }
            other => panic!("Expected ValidationError, got: {:?}", other),
        }
    }

    #[test]
    fn organization_add_remove_change_triggers_config_diff() {
        let acme = ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "okta".to_string(),
            idp: "https://acme.okta.com".to_string(),
            client_id: None,
        };
        let mut with_org = make_config(vec![stdio_ep("a", "echo")]);
        with_org.organizations = vec![acme.clone()];
        let without_org = make_config(vec![stdio_ep("a", "echo")]);

        // Added.
        assert!(
            diff_configs(&without_org, &with_org).organizations_changed,
            "adding an org should be reflected in the diff"
        );
        // Removed.
        assert!(
            diff_configs(&with_org, &without_org).organizations_changed,
            "removing an org should be reflected in the diff"
        );
        // Changed (idp issuer differs).
        let mut changed = make_config(vec![stdio_ep("a", "echo")]);
        changed.organizations = vec![ConfigOrganization {
            idp: "https://acme.okta.com/changed".to_string(),
            ..acme.clone()
        }];
        assert!(
            diff_configs(&with_org, &changed).organizations_changed,
            "changing an org should be reflected in the diff"
        );
        // Unchanged.
        assert!(
            !diff_configs(&with_org, &with_org).organizations_changed,
            "identical orgs should not be flagged as changed"
        );
    }

    #[test]
    fn log_retention_days_defaults_to_none() {
        let toml_str = r#"
[relay]
machine_name = "test"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.log_retention_days, None);
    }

    #[test]
    fn log_retention_days_parses_nonzero() {
        let toml_str = r#"
[relay]
machine_name = "test"
log_retention_days = 14
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.log_retention_days, Some(14));
    }

    #[test]
    fn log_retention_days_parses_zero() {
        let toml_str = r#"
[relay]
machine_name = "test"
log_retention_days = 0
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.log_retention_days, Some(0));
    }

    #[test]
    fn log_retention_days_round_trips() {
        let config = Config {
            relay: RelayConfig {
                machine_name: "test".to_string(),
                log_retention_days: Some(14),
                ..RelayConfig::default()
            },
            endpoints: vec![],
            profiles: None,
            organizations: Vec::new(),
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.relay.log_retention_days, Some(14));
    }

    // --- write_dirs tests ---

    #[test]
    fn write_dirs_defaults_to_none() {
        let toml_str = r#"
[relay]
machine_name = "test"
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.write_dirs, None);
    }

    #[test]
    fn write_dirs_parses_absolute_entries() {
        let toml_str = r#"
[relay]
machine_name = "test"
write_dirs = ["/tmp/media", "/var/data/out"]
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(
            config.relay.write_dirs,
            Some(vec![
                PathBuf::from("/tmp/media"),
                PathBuf::from("/var/data/out")
            ])
        );
    }

    #[test]
    fn write_dirs_relative_entry_is_hard_error() {
        let toml_str = r#"
[relay]
machine_name = "test"
write_dirs = ["relative/path"]
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        assert!(
            matches!(err, ConfigError::ValidationError(ref msg) if msg.contains("relative/path")
                && msg.contains("absolute")),
            "expected write_dirs validation error, got {err:?}"
        );
    }

    #[test]
    fn write_dirs_relative_entry_is_hard_error_in_graceful_path() {
        let toml_str = r#"
[relay]
machine_name = "test"
write_dirs = ["relative/path"]
"#;
        let err = parse_and_validate_graceful(toml_str).unwrap_err();
        assert!(
            matches!(err, ConfigError::ValidationError(_)),
            "graceful path must also fail-fast on relative write_dirs, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_dirs_tilde_entry_passes_validation() {
        // `~/…` expands to an absolute path under $HOME, so validation
        // accepts it even though the literal entry is not absolute.
        let toml_str = r#"
[relay]
machine_name = "test"
write_dirs = ["~/endara-media"]
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(
            config.relay.write_dirs,
            Some(vec![PathBuf::from("~/endara-media")])
        );
    }

    #[test]
    fn write_dirs_empty_list_is_valid() {
        let toml_str = r#"
[relay]
machine_name = "test"
write_dirs = []
"#;
        let config = parse_and_validate(toml_str).unwrap();
        assert_eq!(config.relay.write_dirs, Some(vec![]));
        assert!(resolve_write_roots(&config).is_empty());
    }

    #[test]
    fn write_dirs_round_trips() {
        let config = Config {
            relay: RelayConfig {
                machine_name: "test".to_string(),
                write_dirs: Some(vec![PathBuf::from("/tmp/media")]),
                ..RelayConfig::default()
            },
            endpoints: vec![],
            profiles: None,
            organizations: Vec::new(),
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed.relay.write_dirs,
            Some(vec![PathBuf::from("/tmp/media")])
        );
    }

    // --- resolve_write_roots tests ---

    fn config_with_write_dirs(dirs: Vec<PathBuf>) -> Config {
        Config {
            relay: RelayConfig {
                machine_name: "test".to_string(),
                write_dirs: Some(dirs),
                ..RelayConfig::default()
            },
            endpoints: vec![],
            profiles: None,
            organizations: Vec::new(),
        }
    }

    #[test]
    fn resolve_write_roots_none_yields_empty() {
        let config = default_config();
        assert!(resolve_write_roots(&config).is_empty());
    }

    #[test]
    fn resolve_write_roots_keeps_existing_directories_canonicalized() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_write_dirs(vec![tmp.path().to_path_buf()]);
        let roots = resolve_write_roots(&config);
        assert_eq!(roots, vec![tmp.path().canonicalize().unwrap()]);
    }

    #[test]
    fn resolve_write_roots_skips_missing_directories_without_creating() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let config = config_with_write_dirs(vec![missing.clone(), tmp.path().to_path_buf()]);
        let roots = resolve_write_roots(&config);
        assert_eq!(
            roots,
            vec![tmp.path().canonicalize().unwrap()],
            "missing entry must be skipped, existing one kept"
        );
        assert!(
            !missing.exists(),
            "resolve must never create the missing directory"
        );
    }

    #[test]
    fn resolve_write_roots_skips_plain_files() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir.txt");
        std::fs::write(&file, "x").unwrap();
        let config = config_with_write_dirs(vec![file]);
        assert!(
            resolve_write_roots(&config).is_empty(),
            "a plain file must not become a write root"
        );
    }
}
