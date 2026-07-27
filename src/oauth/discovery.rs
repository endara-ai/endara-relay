use reqwest::Client;
use std::time::Duration;
use url::Url;

use crate::oauth::url_guard::{self, UrlGuardError};

/// Protected Resource Metadata per RFC 9728.
/// Returned by `{resource_url}/.well-known/oauth-protected-resource`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProtectedResourceMetadata {
    /// The resource server's identifier (usually its URL)
    #[allow(dead_code)]
    pub resource: String,
    /// List of authorization server URLs that protect this resource
    pub authorization_servers: Vec<String>,
    /// Bearer token methods supported
    #[allow(dead_code)]
    #[serde(default)]
    pub bearer_methods_supported: Vec<String>,
    /// Scopes that the resource requires/supports
    #[allow(dead_code)]
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

/// OAuth 2.0 Authorization Server Metadata (RFC 8414).
/// Returned by `{auth_server_url}/.well-known/oauth-authorization-server`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// If present, DCR is supported
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub response_types_supported: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub grant_types_supported: Vec<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    /// Token endpoint auth methods (e.g., "client_secret_post", "client_secret_basic", "none")
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Vec<String>,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    /// Whether the authorization server advertises RFC 9207 authorization-response
    /// `iss` parameter support. When false/absent, a missing `iss` on the callback
    /// must not be treated as an error.
    #[serde(default)]
    pub authorization_response_iss_parameter_supported: bool,
    /// Whether the authorization server advertises support for Client ID Metadata
    /// Documents (CIMD). When false/absent, CIMD-based client identification is
    /// unavailable.
    #[serde(default)]
    pub client_id_metadata_document_supported: bool,
}

/// Resolved OAuth server discovery result.
pub struct DiscoveryResult {
    pub auth_server_url: String,
    /// The authorization server's issuer identifier (RFC 8414 `issuer`).
    /// Used for RFC 9207 `iss` validation on the authorization response.
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    #[allow(dead_code)]
    pub code_challenge_methods_supported: Vec<String>,
    #[allow(dead_code)]
    pub token_endpoint_auth_methods: Vec<String>,
    #[allow(dead_code)]
    pub revocation_endpoint: Option<String>,
    /// RFC 9207: whether the authorization server advertises support for the
    /// authorization-response `iss` parameter.
    pub authorization_response_iss_parameter_supported: bool,
    /// Whether the authorization server advertises support for Client ID
    /// Metadata Documents (CIMD).
    #[allow(dead_code)]
    pub client_id_metadata_document_supported: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("Protected resource metadata not found at {url} — server may not support RFC 9728. Provide oauth_server_url manually.")]
    MetadataNotFound { url: String },

    #[error("No authorization servers listed in protected resource metadata")]
    NoAuthorizationServer,

    #[error("Authorization server metadata not found at {url}")]
    AuthServerMetadataNotFound { url: String },

    #[error("Authorization server does not support S256 PKCE code challenge method")]
    S256NotSupported,

    #[error("Discovery HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Discovery timed out after {0}s")]
    Timeout(u64),

    #[error("Discovery URL rejected by SSRF guard: {0}")]
    UrlGuard(#[from] UrlGuardError),
}

impl DiscoveryError {
    /// Whether this failure is transient (network unreachable / timed out)
    /// rather than a genuine absence of metadata (404-class).
    ///
    /// Callers that fall back to convention-based endpoints when a server
    /// doesn't publish RFC 8414 metadata must NOT do so on a transient
    /// failure: the server likely does publish metadata and the guessed
    /// endpoints would be wrong.
    pub fn is_transient(&self) -> bool {
        match self {
            DiscoveryError::Timeout(_) => true,
            DiscoveryError::Http(e) => e.is_timeout() || e.is_connect(),
            _ => false,
        }
    }
}

/// Build a well-known URL per RFC 5785 §3 and RFC 8414 §3.1.
///
/// The `.well-known` segment is inserted between the host (with port) and any
/// existing path components. For example:
/// - `https://auth.example.com` + `oauth-authorization-server`
///   → `https://auth.example.com/.well-known/oauth-authorization-server`
/// - `https://github.com/login/oauth` + `oauth-authorization-server`
///   → `https://github.com/.well-known/oauth-authorization-server/login/oauth`
///
/// This path-aware suffix placement matches the 2026-07-28 spec clarification
/// for BOTH `oauth-protected-resource` (RFC 9728) and `oauth-authorization-server`
/// (RFC 8414): the suffix is inserted directly after the origin, ahead of the
/// resource/issuer path, with a root-only fallback handled by the callers.
fn build_well_known_url(base_url: &str, well_known_suffix: &str) -> Result<String, DiscoveryError> {
    let parsed = Url::parse(base_url).map_err(|_| DiscoveryError::MetadataNotFound {
        url: base_url.to_string(),
    })?;
    let original_path = parsed.path().trim_matches('/');
    let origin = parsed.origin().ascii_serialization();

    if original_path.is_empty() {
        Ok(format!("{origin}/.well-known/{well_known_suffix}"))
    } else {
        Ok(format!(
            "{origin}/.well-known/{well_known_suffix}/{original_path}"
        ))
    }
}

/// Build the root-only well-known URL (no path suffix).
/// Used as a fallback when the RFC 5785 path-based URL returns 404.
fn build_well_known_url_root(
    base_url: &str,
    well_known_suffix: &str,
) -> Result<String, DiscoveryError> {
    let parsed = Url::parse(base_url).map_err(|_| DiscoveryError::MetadataNotFound {
        url: base_url.to_string(),
    })?;
    let origin = parsed.origin().ascii_serialization();
    Ok(format!("{origin}/.well-known/{well_known_suffix}"))
}

/// Build an OpenID Connect Discovery 1.0 URL (§4).
///
/// Unlike RFC 8414 / RFC 5785 (which insert `.well-known` directly after the
/// origin — see [`build_well_known_url`]), OIDC Discovery appends
/// `/.well-known/openid-configuration` to the END of the issuer's full path:
/// - `https://issuer.example.com/oauth2/default`
///   → `https://issuer.example.com/oauth2/default/.well-known/openid-configuration`
/// - `https://issuer.example.com` (or with a trailing slash)
///   → `https://issuer.example.com/.well-known/openid-configuration`
fn build_openid_configuration_url(base_url: &str) -> Result<String, DiscoveryError> {
    let parsed = Url::parse(base_url).map_err(|_| DiscoveryError::MetadataNotFound {
        url: base_url.to_string(),
    })?;
    let original_path = parsed.path().trim_matches('/');
    let origin = parsed.origin().ascii_serialization();

    if original_path.is_empty() {
        Ok(format!("{origin}/.well-known/openid-configuration"))
    } else {
        Ok(format!(
            "{origin}/{original_path}/.well-known/openid-configuration"
        ))
    }
}

/// Returns true if the base URL has a non-empty path component.
fn has_path(base_url: &str) -> bool {
    Url::parse(base_url)
        .map(|u| !u.path().trim_matches('/').is_empty())
        .unwrap_or(false)
}

/// Normalize an issuer/URL for comparison per RFC 8414 §3.3: tolerate a single
/// trailing-slash difference. Comparison is otherwise exact on scheme, host,
/// port, and path.
fn normalize_issuer(issuer: &str) -> &str {
    issuer.strip_suffix('/').unwrap_or(issuer)
}

/// Discover OAuth server metadata for a protected resource using RFC 9728.
///
/// 1. Fetches `{origin}/.well-known/oauth-protected-resource{path}`
///    - Falls back to `{origin}/.well-known/oauth-protected-resource` if 404 and path is non-empty
/// 2. Extracts the first authorization server URL
/// 3. Fetches `{origin}/.well-known/oauth-authorization-server{path}`
///    - Falls back to `{origin}/.well-known/oauth-authorization-server` if 404 and path is non-empty
/// 4. Validates S256 PKCE support
///
/// Both the resource URL and the authorization server URL (which may be on a
/// different origin and is server-supplied) are validated through
/// [`url_guard`] before any HTTP request is sent, and each request uses a
/// per-host pinned client to defeat DNS rebinding.
pub async fn discover_oauth_server(
    resource_url: &str,
    allow_insecure: bool,
) -> Result<DiscoveryResult, DiscoveryError> {
    // Step 1: Fetch protected resource metadata (with root fallback) using a
    // client pinned to the resource host.
    let well_known_url = build_well_known_url(resource_url, "oauth-protected-resource")?;
    let resource_client = url_guard::validated_client(&well_known_url, allow_insecure).await?;

    let resource_meta: ProtectedResourceMetadata =
        match fetch_well_known(&resource_client, &well_known_url).await {
            Ok(resp) => resp.json().await?,
            Err(DiscoveryError::MetadataNotFound { .. }) if has_path(resource_url) => {
                // Fallback: try root well-known URL without path (same host, reuse client)
                let root_url = build_well_known_url_root(resource_url, "oauth-protected-resource")?;
                fetch_well_known(&resource_client, &root_url)
                    .await
                    .map_err(|_| DiscoveryError::MetadataNotFound {
                        url: well_known_url.clone(),
                    })?
                    .json()
                    .await?
            }
            Err(e) => return Err(e),
        };

    let auth_server_url = resource_meta
        .authorization_servers
        .first()
        .ok_or(DiscoveryError::NoAuthorizationServer)?
        .clone();

    // Step 2: Fetch authorization server metadata (RFC 8414).
    discover_authorization_server(&auth_server_url, allow_insecure).await
}

/// Discover OAuth server metadata starting from an EXPLICIT RFC 9728
/// protected-resource metadata URL (e.g. the `resource_metadata` value of a
/// `WWW-Authenticate: Bearer` challenge).
///
/// Unlike [`discover_oauth_server`], the protected-resource metadata document
/// is fetched at the EXACT given URL — its full path is honored — rather than
/// re-deriving the conventional well-known location from an origin. RFC 9728
/// permits path-based protected-resource metadata
/// (e.g. `https://host/.well-known/oauth-protected-resource/<resource-path>`),
/// so when the server points us at a specific document we must fetch that one.
///
/// The URL is validated through [`url_guard`] before any HTTP request is sent,
/// and the request uses a per-host pinned client. After parsing, the first
/// listed authorization server is resolved via
/// [`discover_authorization_server`] (RFC 8414).
pub async fn discover_oauth_server_from_metadata(
    resource_metadata_url: &str,
    allow_insecure: bool,
) -> Result<DiscoveryResult, DiscoveryError> {
    let resource_client =
        url_guard::validated_client(resource_metadata_url, allow_insecure).await?;
    let resource_meta: ProtectedResourceMetadata =
        fetch_well_known(&resource_client, resource_metadata_url)
            .await?
            .json()
            .await?;

    let auth_server_url = resource_meta
        .authorization_servers
        .first()
        .ok_or(DiscoveryError::NoAuthorizationServer)?
        .clone();

    discover_authorization_server(&auth_server_url, allow_insecure).await
}

/// Discover OAuth authorization server metadata directly (RFC 8414 + OIDC).
///
/// Unlike [`discover_oauth_server`], this skips the RFC 9728 protected
/// resource step and fetches the AS metadata against `auth_server_url`
/// itself, probing the following locations in order and falling through ONLY
/// on a 404 / [`DiscoveryError::MetadataNotFound`]:
///
/// 1. `{origin}/.well-known/oauth-authorization-server{path}` (RFC 8414 path-insert)
/// 2. `{origin}/.well-known/oauth-authorization-server` (RFC 8414 root)
/// 3. `{origin}{path}/.well-known/openid-configuration` (OIDC Discovery end-append)
/// 4. `{origin}/.well-known/openid-configuration` (OIDC Discovery root)
///
/// Candidates that collapse to an already-probed URL (e.g. when
/// `auth_server_url` has no path) are skipped. Each candidate's metadata
/// `issuer` is validated against the expected issuer identifier for that
/// candidate form (the origin for the root well-known forms, the full input
/// URL for the path-insert / end-append forms) per RFC 8414 §3.3; metadata
/// advertising a mismatching issuer is rejected and probing continues, so we
/// never accept metadata for a different AS sharing the same origin. S256 PKCE
/// support is then validated.
///
/// All candidates share the same origin, so a single per-host pinned client —
/// validated through [`url_guard`] against the first URL before any HTTP
/// request is sent — is reused, preserving the SSRF guard and DNS-rebinding
/// protections.
///
/// The returned `DiscoveryResult.auth_server_url` is set to the input
/// `auth_server_url` so callers can label discovery output consistently.
pub async fn discover_authorization_server(
    auth_server_url: &str,
    allow_insecure: bool,
) -> Result<DiscoveryResult, DiscoveryError> {
    let map_err = || DiscoveryError::AuthServerMetadataNotFound {
        url: auth_server_url.to_string(),
    };

    // Expected issuer identifiers per RFC 8414 §3.3 / OIDC Discovery: the
    // origin for the root well-known forms, and the full (normalized) input
    // URL for the path-insert / end-append forms.
    let parsed = Url::parse(auth_server_url).map_err(|_| map_err())?;
    let origin = parsed.origin().ascii_serialization();
    let input_path = parsed.path().trim_end_matches('/');
    let input_issuer = if input_path.is_empty() {
        origin.clone()
    } else {
        format!("{origin}{input_path}")
    };

    // Build the ordered probe list. All candidates share the same origin. Each
    // entry pairs a candidate URL with the issuer we expect its metadata to
    // advertise, so we never accept metadata for a different AS on the origin.
    let mut candidates: Vec<(String, String)> = Vec::new();
    let as_path_insert = build_well_known_url(auth_server_url, "oauth-authorization-server")
        .map_err(|_| map_err())?;
    candidates.push((as_path_insert.clone(), input_issuer.clone()));

    let as_root = build_well_known_url_root(auth_server_url, "oauth-authorization-server")
        .map_err(|_| map_err())?;
    if !candidates.iter().any(|(url, _)| url == &as_root) {
        candidates.push((as_root, origin.clone()));
    }

    let oidc_append = build_openid_configuration_url(auth_server_url).map_err(|_| map_err())?;
    if !candidates.iter().any(|(url, _)| url == &oidc_append) {
        candidates.push((oidc_append, input_issuer.clone()));
    }

    let oidc_root = build_well_known_url_root(auth_server_url, "openid-configuration")
        .map_err(|_| map_err())?;
    if !candidates.iter().any(|(url, _)| url == &oidc_root) {
        candidates.push((oidc_root, origin.clone()));
    }

    let as_client = url_guard::validated_client(&as_path_insert, allow_insecure).await?;

    // Probe each candidate, falling through on MetadataNotFound (404) or on an
    // issuer mismatch (RFC 8414 §3.3): metadata whose `issuer` does not match
    // the expected identifier for that candidate form is rejected and treated
    // like a miss so we keep probing the remaining candidates.
    let mut as_meta: Option<AuthorizationServerMetadata> = None;
    let mut last_not_found: Option<String> = None;
    for (candidate, expected_issuer) in &candidates {
        match fetch_well_known(&as_client, candidate).await {
            Ok(resp) => {
                let meta: AuthorizationServerMetadata = resp.json().await?;
                if normalize_issuer(&meta.issuer) != normalize_issuer(expected_issuer) {
                    last_not_found = Some(candidate.clone());
                    continue;
                }
                as_meta = Some(meta);
                break;
            }
            Err(DiscoveryError::MetadataNotFound { url }) => {
                last_not_found = Some(url);
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    let as_meta = match as_meta {
        Some(meta) => meta,
        None => {
            return Err(DiscoveryError::AuthServerMetadataNotFound {
                url: last_not_found.unwrap_or(as_path_insert),
            });
        }
    };

    // Validate S256 is supported (required for PKCE)
    if !as_meta.code_challenge_methods_supported.is_empty()
        && !as_meta
            .code_challenge_methods_supported
            .contains(&"S256".to_string())
    {
        return Err(DiscoveryError::S256NotSupported);
    }

    Ok(DiscoveryResult {
        auth_server_url: auth_server_url.to_string(),
        issuer: as_meta.issuer,
        authorization_endpoint: as_meta.authorization_endpoint,
        token_endpoint: as_meta.token_endpoint,
        registration_endpoint: as_meta.registration_endpoint,
        scopes_supported: as_meta.scopes_supported,
        code_challenge_methods_supported: as_meta.code_challenge_methods_supported,
        token_endpoint_auth_methods: as_meta.token_endpoint_auth_methods_supported,
        revocation_endpoint: as_meta.revocation_endpoint,
        authorization_response_iss_parameter_supported: as_meta
            .authorization_response_iss_parameter_supported,
        client_id_metadata_document_supported: as_meta.client_id_metadata_document_supported,
    })
}

/// Fetch a well-known URL and return the response, mapping errors appropriately.
async fn fetch_well_known(
    http_client: &Client,
    url: &str,
) -> Result<reqwest::Response, DiscoveryError> {
    http_client
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                DiscoveryError::Timeout(10)
            } else {
                DiscoveryError::Http(e)
            }
        })?
        .error_for_status()
        .map_err(|e| {
            if e.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                DiscoveryError::MetadataNotFound {
                    url: url.to_string(),
                }
            } else {
                DiscoveryError::Http(e)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_protected_resource_metadata() {
        // Includes unknown field "resource_name" to verify we tolerate extra fields
        let json = r#"{
            "resource": "https://mcp.linear.app",
            "authorization_servers": ["https://linear.app/oauth"],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["read", "write"],
            "resource_name": "Test Server"
        }"#;
        let meta: ProtectedResourceMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.resource, "https://mcp.linear.app");
        assert_eq!(meta.authorization_servers, vec!["https://linear.app/oauth"]);
        assert_eq!(meta.bearer_methods_supported, vec!["header"]);
        assert_eq!(meta.scopes_supported, vec!["read", "write"]);
    }

    #[test]
    fn parse_protected_resource_metadata_minimal() {
        let json = r#"{
            "resource": "https://mcp.example.com",
            "authorization_servers": ["https://auth.example.com"]
        }"#;
        let meta: ProtectedResourceMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.resource, "https://mcp.example.com");
        assert_eq!(meta.authorization_servers, vec!["https://auth.example.com"]);
        assert!(meta.bearer_methods_supported.is_empty());
        assert!(meta.scopes_supported.is_empty());
    }

    #[test]
    fn parse_protected_resource_metadata_missing_required_field() {
        let json = r#"{
            "authorization_servers": ["https://auth.example.com"]
        }"#;
        let result: Result<ProtectedResourceMetadata, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn parse_authorization_server_metadata() {
        let json = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
            "registration_endpoint": "https://auth.example.com/register",
            "scopes_supported": ["read", "write"],
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
            "revocation_endpoint": "https://auth.example.com/revoke"
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.issuer, "https://auth.example.com");
        assert_eq!(
            meta.authorization_endpoint,
            "https://auth.example.com/authorize"
        );
        assert_eq!(meta.token_endpoint, "https://auth.example.com/token");
        assert_eq!(
            meta.registration_endpoint.as_deref(),
            Some("https://auth.example.com/register")
        );
        assert_eq!(meta.code_challenge_methods_supported, vec!["S256"]);
        assert_eq!(
            meta.token_endpoint_auth_methods_supported,
            vec!["none", "client_secret_post"]
        );
        assert_eq!(
            meta.revocation_endpoint.as_deref(),
            Some("https://auth.example.com/revoke")
        );
    }

    #[test]
    fn parse_authorization_server_metadata_minimal() {
        let json = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token"
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.issuer, "https://auth.example.com");
        assert!(meta.registration_endpoint.is_none());
        assert!(meta.scopes_supported.is_empty());
        assert!(meta.code_challenge_methods_supported.is_empty());
        assert!(meta.token_endpoint_auth_methods_supported.is_empty());
        assert!(meta.revocation_endpoint.is_none());
    }

    #[test]
    fn parse_authorization_server_metadata_missing_required_field() {
        let json = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize"
        }"#;
        let result: Result<AuthorizationServerMetadata, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn s256_validation_rejects_non_s256_only() {
        let methods = ["plain".to_string()];
        let should_reject = !methods.is_empty() && !methods.contains(&"S256".to_string());
        assert!(should_reject);
    }

    #[test]
    fn s256_validation_passes_when_empty() {
        let methods: Vec<String> = vec![];
        let should_reject = !methods.is_empty() && !methods.contains(&"S256".to_string());
        assert!(!should_reject);
    }

    #[test]
    fn s256_validation_passes_when_s256_present() {
        let methods = ["S256".to_string(), "plain".to_string()];
        let should_reject = !methods.is_empty() && !methods.contains(&"S256".to_string());
        assert!(!should_reject);
    }

    // --- build_well_known_url tests ---

    #[test]
    fn test_build_well_known_url_no_path() {
        let url =
            build_well_known_url("https://auth.example.com", "oauth-authorization-server").unwrap();
        assert_eq!(
            url,
            "https://auth.example.com/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn test_build_well_known_url_with_path() {
        let url = build_well_known_url(
            "https://github.com/login/oauth",
            "oauth-authorization-server",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://github.com/.well-known/oauth-authorization-server/login/oauth"
        );
    }

    #[test]
    fn test_build_well_known_url_with_trailing_slash() {
        let url = build_well_known_url(
            "https://api.githubcopilot.com/mcp/",
            "oauth-protected-resource",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn test_build_well_known_url_with_port() {
        let url = build_well_known_url(
            "https://localhost:8080/api/auth",
            "oauth-authorization-server",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://localhost:8080/.well-known/oauth-authorization-server/api/auth"
        );
    }

    // --- GitHub real-world metadata parsing tests ---

    #[test]
    fn test_parse_github_protected_resource_metadata() {
        let json = r#"{
            "resource": "https://api.githubcopilot.com/mcp",
            "authorization_servers": ["https://github.com/login/oauth"],
            "scopes_supported": ["repo","read:org","read:user","user:email","read:packages","write:packages","read:project","project","gist","notifications","workflow","codespace"],
            "bearer_methods_supported": ["header"],
            "resource_name": "GitHub MCP Server"
        }"#;
        let meta: ProtectedResourceMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.resource, "https://api.githubcopilot.com/mcp");
        assert_eq!(
            meta.authorization_servers,
            vec!["https://github.com/login/oauth"]
        );
        assert_eq!(meta.scopes_supported.len(), 12);
        assert!(meta.scopes_supported.contains(&"repo".to_string()));
        assert!(meta.scopes_supported.contains(&"codespace".to_string()));
        assert_eq!(meta.bearer_methods_supported, vec!["header"]);
    }

    #[test]
    fn test_parse_github_authorization_server_metadata() {
        let json = r#"{
            "issuer": "https://github.com/login/oauth",
            "authorization_endpoint": "https://github.com/login/oauth/authorize",
            "token_endpoint": "https://github.com/login/oauth/access_token",
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code","refresh_token"],
            "service_documentation": "https://docs.github.com/apps/creating-github-apps/registering-a-github-app/registering-a-github-app",
            "code_challenge_methods_supported": ["S256"]
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.issuer, "https://github.com/login/oauth");
        assert_eq!(
            meta.authorization_endpoint,
            "https://github.com/login/oauth/authorize"
        );
        assert_eq!(
            meta.token_endpoint,
            "https://github.com/login/oauth/access_token"
        );
        assert!(meta.registration_endpoint.is_none());
        assert_eq!(meta.code_challenge_methods_supported, vec!["S256"]);
        assert_eq!(meta.response_types_supported, vec!["code"]);
        assert_eq!(
            meta.grant_types_supported,
            vec!["authorization_code", "refresh_token"]
        );
    }

    #[test]
    fn test_github_no_registration_endpoint() {
        // GitHub's AS metadata has no registration_endpoint → DCR is unavailable
        let json = r#"{
            "issuer": "https://github.com/login/oauth",
            "authorization_endpoint": "https://github.com/login/oauth/authorize",
            "token_endpoint": "https://github.com/login/oauth/access_token",
            "code_challenge_methods_supported": ["S256"]
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.registration_endpoint.is_none());
    }

    #[test]
    fn test_github_no_token_endpoint_auth_methods() {
        // GitHub doesn't list token_endpoint_auth_methods_supported → defaults to empty vec
        let json = r#"{
            "issuer": "https://github.com/login/oauth",
            "authorization_endpoint": "https://github.com/login/oauth/authorize",
            "token_endpoint": "https://github.com/login/oauth/access_token",
            "code_challenge_methods_supported": ["S256"]
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.token_endpoint_auth_methods_supported.is_empty());
    }

    // --- Root fallback helper tests ---

    #[test]
    fn test_has_path_empty() {
        assert!(!has_path("https://mcp.linear.app"));
        assert!(!has_path("https://mcp.linear.app/"));
    }

    #[test]
    fn test_has_path_with_path() {
        assert!(has_path("https://mcp.linear.app/mcp"));
        assert!(has_path("https://api.githubcopilot.com/mcp/"));
        assert!(has_path("https://github.com/login/oauth"));
    }

    #[test]
    fn test_build_well_known_url_root() {
        let url =
            build_well_known_url_root("https://mcp.linear.app/mcp", "oauth-protected-resource")
                .unwrap();
        assert_eq!(
            url,
            "https://mcp.linear.app/.well-known/oauth-protected-resource"
        );
    }

    // --- Linear real-world metadata parsing tests ---

    #[test]
    fn test_parse_linear_protected_resource() {
        let json = r#"{"resource":"https://mcp.linear.app","authorization_servers":["https://mcp.linear.app"],"bearer_methods_supported":["header"]}"#;
        let meta: ProtectedResourceMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.resource, "https://mcp.linear.app");
        assert_eq!(meta.authorization_servers, vec!["https://mcp.linear.app"]);
        assert_eq!(meta.bearer_methods_supported, vec!["header"]);
        assert!(meta.scopes_supported.is_empty());
    }

    #[test]
    fn test_parse_linear_auth_server() {
        let json = r#"{"issuer":"https://mcp.linear.app","authorization_endpoint":"https://mcp.linear.app/authorize","token_endpoint":"https://mcp.linear.app/token","registration_endpoint":"https://mcp.linear.app/register","response_types_supported":["code"],"response_modes_supported":["query"],"grant_types_supported":["authorization_code","refresh_token"],"token_endpoint_auth_methods_supported":["client_secret_basic","client_secret_post","none"],"revocation_endpoint":"https://mcp.linear.app/token","code_challenge_methods_supported":["plain","S256"],"client_id_metadata_document_supported":false}"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.issuer, "https://mcp.linear.app");
        assert_eq!(
            meta.authorization_endpoint,
            "https://mcp.linear.app/authorize"
        );
        assert_eq!(meta.token_endpoint, "https://mcp.linear.app/token");
        assert_eq!(
            meta.registration_endpoint.as_deref(),
            Some("https://mcp.linear.app/register")
        );
        assert_eq!(meta.code_challenge_methods_supported, vec!["plain", "S256"]);
        assert_eq!(
            meta.token_endpoint_auth_methods_supported,
            vec!["client_secret_basic", "client_secret_post", "none"]
        );
        assert_eq!(
            meta.revocation_endpoint.as_deref(),
            Some("https://mcp.linear.app/token")
        );
    }

    // --- Notion real-world metadata parsing tests ---

    #[test]
    fn test_parse_notion_protected_resource() {
        let json = r#"{"resource":"https://mcp.notion.com","resource_name":"Notion MCP (Beta)","resource_documentation":"https://developers.notion.com/docs/mcp","authorization_servers":["https://mcp.notion.com"],"bearer_methods_supported":["header"]}"#;
        let meta: ProtectedResourceMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.resource, "https://mcp.notion.com");
        assert_eq!(meta.authorization_servers, vec!["https://mcp.notion.com"]);
        assert_eq!(meta.bearer_methods_supported, vec!["header"]);
    }

    #[test]
    fn test_parse_notion_auth_server() {
        let json = r#"{"issuer":"https://mcp.notion.com","authorization_endpoint":"https://mcp.notion.com/authorize","token_endpoint":"https://mcp.notion.com/token","registration_endpoint":"https://mcp.notion.com/register","response_types_supported":["code"],"response_modes_supported":["query"],"grant_types_supported":["authorization_code","refresh_token"],"token_endpoint_auth_methods_supported":["client_secret_basic","client_secret_post","none"],"revocation_endpoint":"https://mcp.notion.com/token","code_challenge_methods_supported":["plain","S256"]}"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.issuer, "https://mcp.notion.com");
        assert_eq!(
            meta.authorization_endpoint,
            "https://mcp.notion.com/authorize"
        );
        assert_eq!(meta.token_endpoint, "https://mcp.notion.com/token");
        assert_eq!(
            meta.registration_endpoint.as_deref(),
            Some("https://mcp.notion.com/register")
        );
        assert_eq!(meta.code_challenge_methods_supported, vec!["plain", "S256"]);
    }

    // --- Slack real-world metadata parsing tests ---

    #[test]
    fn test_parse_slack_protected_resource() {
        let json = r#"{"resource":"https://mcp.slack.com","authorization_servers":["https://mcp.slack.com"],"bearer_methods_supported":["header","form"],"scopes_supported":["search:read.public","chat:write"],"resource_name":"Slack API","resource_documentation":"https://api.slack.com","tls_client_certificate_bound_access_tokens":false}"#;
        let meta: ProtectedResourceMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.resource, "https://mcp.slack.com");
        assert_eq!(meta.authorization_servers, vec!["https://mcp.slack.com"]);
        assert_eq!(meta.bearer_methods_supported, vec!["header", "form"]);
        assert_eq!(
            meta.scopes_supported,
            vec!["search:read.public", "chat:write"]
        );
    }

    #[test]
    fn test_parse_slack_auth_server() {
        let json = r#"{"issuer":"https://slack.com","authorization_endpoint":"https://slack.com/oauth/v2_user/authorize","token_endpoint":"https://slack.com/api/oauth.v2.user.access","response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"token_endpoint_auth_methods_supported":["client_secret_post"],"code_challenge_methods_supported":["S256"],"scopes_supported":["search:read.public","chat:write"]}"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.issuer, "https://slack.com");
        assert_eq!(
            meta.authorization_endpoint,
            "https://slack.com/oauth/v2_user/authorize"
        );
        assert_eq!(
            meta.token_endpoint,
            "https://slack.com/api/oauth.v2.user.access"
        );
        assert!(meta.registration_endpoint.is_none());
        assert_eq!(meta.code_challenge_methods_supported, vec!["S256"]);
        assert_eq!(
            meta.token_endpoint_auth_methods_supported,
            vec!["client_secret_post"]
        );
        assert_eq!(
            meta.scopes_supported,
            vec!["search:read.public", "chat:write"]
        );
    }

    // --- PR #69 audit gap 2a: trailing-slash regression ---------------------

    /// `build_well_known_url` must collapse a trailing slash on the base URL
    /// rather than producing `…//.well-known/…`. The originally reported bug
    /// was against `https://accounts.google.com/`.
    #[test]
    fn test_build_well_known_url_trailing_slash_no_path() {
        let url =
            build_well_known_url("https://accounts.google.com/", "oauth-authorization-server")
                .unwrap();
        assert_eq!(
            url,
            "https://accounts.google.com/.well-known/oauth-authorization-server"
        );
    }

    // --- PR #69 audit gap 3: discover_authorization_server direct tests -----

    /// Spawn an axum server on `127.0.0.1:0` that serves AS metadata. Returns
    /// `(base_url, handle)`. The `path_status` controls what the path-shaped
    /// `oauth-authorization-server` well-known URL returns.
    ///
    /// `build_bodies` is invoked with the bound base URL (so fixtures can embed
    /// the dynamic origin/port in their `issuer` fields) and returns
    /// `(root_body, path_body, oidc_root_body, oidc_append_body)`:
    /// - `root_body` is served at `/.well-known/oauth-authorization-server`,
    /// - `path_body` at the path-shaped `oauth-authorization-server` URL,
    /// - `oidc_root_body` at `/.well-known/openid-configuration`, and
    /// - `oidc_append_body` at any URL ending in
    ///   `/.well-known/openid-configuration` (the OIDC end-append form).
    ///
    /// Each serves 404 when its `Option` is `None`.
    async fn spawn_as_fixture(
        path_status: axum::http::StatusCode,
        build_bodies: impl FnOnce(
            &str,
        ) -> (
            Option<serde_json::Value>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
        ),
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::{Path as AxPath, State};
        use axum::http::{StatusCode, Uri};
        use axum::{response::IntoResponse, routing::get, Json, Router};

        #[derive(Clone)]
        struct Fx {
            path_status: StatusCode,
            root_body: std::sync::Arc<Option<serde_json::Value>>,
            path_body: std::sync::Arc<Option<serde_json::Value>>,
            oidc_root_body: std::sync::Arc<Option<serde_json::Value>>,
            oidc_append_body: std::sync::Arc<Option<serde_json::Value>>,
        }

        async fn root(State(fx): State<Fx>) -> axum::response::Response {
            match fx.root_body.as_ref() {
                Some(v) => (StatusCode::OK, Json(v.clone())).into_response(),
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        }
        async fn path_handler(
            State(fx): State<Fx>,
            AxPath(_): AxPath<String>,
        ) -> axum::response::Response {
            if fx.path_status == StatusCode::OK {
                match fx.path_body.as_ref() {
                    Some(v) => (StatusCode::OK, Json(v.clone())).into_response(),
                    None => (StatusCode::OK, "{}").into_response(),
                }
            } else {
                (fx.path_status, "not found").into_response()
            }
        }
        async fn oidc_root(State(fx): State<Fx>) -> axum::response::Response {
            match fx.oidc_root_body.as_ref() {
                Some(v) => (StatusCode::OK, Json(v.clone())).into_response(),
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        }
        // Catch-all: serves the OIDC end-append body for any URL ending in
        // `/.well-known/openid-configuration`; 404 otherwise.
        async fn oidc_append_fallback(State(fx): State<Fx>, uri: Uri) -> axum::response::Response {
            if uri.path().ends_with("/.well-known/openid-configuration") {
                match fx.oidc_append_body.as_ref() {
                    Some(v) => (StatusCode::OK, Json(v.clone())).into_response(),
                    None => (StatusCode::NOT_FOUND, "not found").into_response(),
                }
            } else {
                (StatusCode::NOT_FOUND, "not found").into_response()
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let (root_body, path_body, oidc_root_body, oidc_append_body) = build_bodies(&base);

        let fx = Fx {
            path_status,
            root_body: std::sync::Arc::new(root_body),
            path_body: std::sync::Arc::new(path_body),
            oidc_root_body: std::sync::Arc::new(oidc_root_body),
            oidc_append_body: std::sync::Arc::new(oidc_append_body),
        };

        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(root))
            .route(
                "/.well-known/oauth-authorization-server/{*tail}",
                get(path_handler),
            )
            .route("/.well-known/openid-configuration", get(oidc_root))
            .fallback(oidc_append_fallback)
            .with_state(fx);

        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        // Tiny delay so the server is accepting connections.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (base, handle)
    }

    /// 3a. URL has a path → path-shaped well-known 404 → root-only fallback
    /// returns the parsed metadata. The root form's issuer must match the
    /// origin per RFC 8414 §3.3.
    #[tokio::test]
    async fn discover_authorization_server_path_404_falls_back_to_root() {
        use serde_json::json;
        let (base, server) = spawn_as_fixture(axum::http::StatusCode::NOT_FOUND, |base| {
            let root_meta = json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "code_challenge_methods_supported": ["S256"],
            });
            (Some(root_meta), None, None, None)
        })
        .await;

        // Input URL has a path so build_well_known_url emits the path variant
        // first; the fixture returns 404 there and the root fallback hits.
        let url = format!("{}/some/path", base);
        let result = discover_authorization_server(&url, true)
            .await
            .expect("root fallback must succeed");
        assert_eq!(result.authorization_endpoint, format!("{base}/authorize"));
        assert_eq!(result.token_endpoint, format!("{base}/token"));
        assert_eq!(result.auth_server_url, url);

        server.abort();
    }

    /// 3b. AS metadata advertises only `plain` (no `S256`) → S256NotSupported.
    #[tokio::test]
    async fn discover_authorization_server_rejects_when_s256_missing() {
        use serde_json::json;
        // URL has no path so the first fetch hits the root well-known route;
        // serve the metadata there with an origin-matching issuer.
        let (base, server) = spawn_as_fixture(axum::http::StatusCode::OK, |base| {
            let root_meta = json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "code_challenge_methods_supported": ["plain"],
            });
            (Some(root_meta), None, None, None)
        })
        .await;

        let result = discover_authorization_server(&base, true).await;
        assert!(
            matches!(result, Err(DiscoveryError::S256NotSupported)),
            "expected S256NotSupported, got {:?}",
            result.err()
        );

        server.abort();
    }

    /// 3c. SSRF guard rejects a loopback URL when `allow_insecure=false`,
    /// surfaced as `DiscoveryError::UrlGuard`.
    #[tokio::test]
    async fn discover_authorization_server_ssrf_guard_rejects_loopback() {
        // 127.0.0.1 is loopback → guard rejects when allow_insecure=false.
        let result = discover_authorization_server("http://127.0.0.1:1/", false).await;
        match result {
            Err(DiscoveryError::UrlGuard(_)) => {}
            Err(other) => panic!("expected DiscoveryError::UrlGuard, got {:?}", other),
            Ok(_) => panic!("expected DiscoveryError::UrlGuard, got Ok(_)"),
        }
    }

    // --- DiscoveryError::is_transient classification -------------------------

    /// Timeout is transient; 404-class metadata absence is not.
    #[test]
    fn is_transient_classifies_timeout_and_not_found() {
        assert!(DiscoveryError::Timeout(10).is_transient());
        assert!(!DiscoveryError::MetadataNotFound {
            url: "https://x".into()
        }
        .is_transient());
        assert!(!DiscoveryError::AuthServerMetadataNotFound {
            url: "https://x".into()
        }
        .is_transient());
        assert!(!DiscoveryError::NoAuthorizationServer.is_transient());
        assert!(!DiscoveryError::S256NotSupported.is_transient());
    }

    /// A connection-refused reqwest error wrapped in `Http` is transient.
    #[tokio::test]
    async fn is_transient_classifies_connect_error() {
        // Bind a listener to reserve a port, then drop it so the connection
        // is refused.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let err = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .expect_err("connection should be refused");
        assert!(DiscoveryError::Http(err).is_transient());
    }

    // --- build_openid_configuration_url tests --------------------------------

    #[test]
    fn test_build_openid_configuration_url_no_path() {
        let url = build_openid_configuration_url("https://issuer.example.com").unwrap();
        assert_eq!(
            url,
            "https://issuer.example.com/.well-known/openid-configuration"
        );
    }

    #[test]
    fn test_build_openid_configuration_url_with_path() {
        let url =
            build_openid_configuration_url("https://issuer.example.com/oauth2/default").unwrap();
        assert_eq!(
            url,
            "https://issuer.example.com/oauth2/default/.well-known/openid-configuration"
        );
    }

    #[test]
    fn test_build_openid_configuration_url_trailing_slash() {
        // Root + trailing slash collapses to the root form.
        let url = build_openid_configuration_url("https://issuer.example.com/").unwrap();
        assert_eq!(
            url,
            "https://issuer.example.com/.well-known/openid-configuration"
        );
        // Path + trailing slash collapses the trailing slash too.
        let url =
            build_openid_configuration_url("https://issuer.example.com/oauth2/default/").unwrap();
        assert_eq!(
            url,
            "https://issuer.example.com/oauth2/default/.well-known/openid-configuration"
        );
    }

    #[test]
    fn test_build_openid_configuration_url_with_port() {
        let url = build_openid_configuration_url("https://localhost:8080/api/auth").unwrap();
        assert_eq!(
            url,
            "https://localhost:8080/api/auth/.well-known/openid-configuration"
        );
    }

    // --- openid-configuration fallback discovery tests -----------------------

    /// oauth-as path 404 + oauth-as root 404 → openid-configuration end-append
    /// succeeds. The end-append form's issuer must match the full input URL.
    #[tokio::test]
    async fn discover_authorization_server_falls_back_to_openid_configuration() {
        use serde_json::json;
        // oauth-as path 404 (path_status), oauth-as root 404 (root_body None),
        // openid-configuration end-append served with an input-matching issuer.
        let (base, server) = spawn_as_fixture(axum::http::StatusCode::NOT_FOUND, |base| {
            let oidc_meta = json!({
                "issuer": format!("{base}/oauth2/default"),
                "authorization_endpoint": format!("{base}/oauth2/default/authorize"),
                "token_endpoint": format!("{base}/oauth2/default/token"),
                "code_challenge_methods_supported": ["S256"],
            });
            (None, None, None, Some(oidc_meta))
        })
        .await;

        let url = format!("{}/oauth2/default", base);
        let result = discover_authorization_server(&url, true)
            .await
            .expect("openid-configuration end-append must succeed");
        assert_eq!(
            result.authorization_endpoint,
            format!("{base}/oauth2/default/authorize")
        );
        assert_eq!(
            result.token_endpoint,
            format!("{base}/oauth2/default/token")
        );
        assert_eq!(result.auth_server_url, url);

        server.abort();
    }

    /// oauth-as path 404 + oauth-as root 404 + openid-configuration end-append
    /// 404 → openid-configuration root fallback succeeds. The root form's
    /// issuer must match the origin.
    #[tokio::test]
    async fn discover_authorization_server_falls_back_to_openid_configuration_root() {
        use serde_json::json;
        // Only the openid-configuration root URL serves metadata.
        let (base, server) = spawn_as_fixture(axum::http::StatusCode::NOT_FOUND, |base| {
            let oidc_meta = json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "code_challenge_methods_supported": ["S256"],
            });
            (None, None, Some(oidc_meta), None)
        })
        .await;

        let url = format!("{}/oauth2/default", base);
        let result = discover_authorization_server(&url, true)
            .await
            .expect("openid-configuration root fallback must succeed");
        assert_eq!(result.authorization_endpoint, format!("{base}/authorize"));
        assert_eq!(result.token_endpoint, format!("{base}/token"));

        server.abort();
    }

    // --- client_id_metadata_document_supported parsing + threading -----------

    #[test]
    fn test_cimd_parses_true_false_absent() {
        let with_true = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
            "client_id_metadata_document_supported": true
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(with_true).unwrap();
        assert!(meta.client_id_metadata_document_supported);

        let with_false = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
            "client_id_metadata_document_supported": false
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(with_false).unwrap();
        assert!(!meta.client_id_metadata_document_supported);

        let absent = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token"
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(absent).unwrap();
        assert!(!meta.client_id_metadata_document_supported);
    }

    /// `client_id_metadata_document_supported` is threaded onto `DiscoveryResult`.
    #[tokio::test]
    async fn discover_authorization_server_surfaces_cimd_flag() {
        use serde_json::json;
        let (base, server) = spawn_as_fixture(axum::http::StatusCode::OK, |base| {
            let root_meta = json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "code_challenge_methods_supported": ["S256"],
                "client_id_metadata_document_supported": true,
            });
            (Some(root_meta), None, None, None)
        })
        .await;

        let result = discover_authorization_server(&base, true)
            .await
            .expect("discovery must succeed");
        assert!(result.client_id_metadata_document_supported);

        server.abort();
    }

    // --- RFC 8414 §3.3 issuer-validation tests -------------------------------

    /// Metadata served on the same origin but advertising a DIFFERENT issuer
    /// than expected must be rejected — discovery must not accept metadata for a
    /// different AS sharing the origin, and falls through to not-found.
    #[tokio::test]
    async fn discover_authorization_server_rejects_issuer_mismatch() {
        use serde_json::json;
        // openid-configuration root serves metadata whose issuer points at a
        // wholly different authorization server. All other candidates 404.
        let (base, server) = spawn_as_fixture(axum::http::StatusCode::NOT_FOUND, |_base| {
            let oidc_meta = json!({
                "issuer": "https://evil.example.com",
                "authorization_endpoint": "https://evil.example.com/authorize",
                "token_endpoint": "https://evil.example.com/token",
                "code_challenge_methods_supported": ["S256"],
            });
            (None, None, Some(oidc_meta), None)
        })
        .await;

        let result = discover_authorization_server(&base, true).await;
        assert!(
            matches!(
                result,
                Err(DiscoveryError::AuthServerMetadataNotFound { .. })
            ),
            "expected AuthServerMetadataNotFound on issuer mismatch, got {:?}",
            result.err()
        );

        server.abort();
    }

    /// Regression: an `oauth-authorization-server` path-form AS whose issuer
    /// matches the full input URL is accepted.
    #[tokio::test]
    async fn discover_authorization_server_accepts_matching_issuer_path_as() {
        use serde_json::json;
        let (base, server) = spawn_as_fixture(axum::http::StatusCode::OK, |base| {
            let path_meta = json!({
                "issuer": format!("{base}/login/oauth"),
                "authorization_endpoint": format!("{base}/login/oauth/authorize"),
                "token_endpoint": format!("{base}/login/oauth/token"),
                "code_challenge_methods_supported": ["S256"],
            });
            (None, Some(path_meta), None, None)
        })
        .await;

        let url = format!("{}/login/oauth", base);
        let result = discover_authorization_server(&url, true)
            .await
            .expect("matching path-form AS must succeed");
        assert_eq!(result.issuer, format!("{base}/login/oauth"));

        server.abort();
    }

    /// Regression: an OpenID Connect end-append AS whose issuer matches the full
    /// input URL is accepted.
    #[tokio::test]
    async fn discover_authorization_server_accepts_matching_issuer_oidc_as() {
        use serde_json::json;
        let (base, server) = spawn_as_fixture(axum::http::StatusCode::NOT_FOUND, |base| {
            let oidc_meta = json!({
                "issuer": format!("{base}/oauth2/default"),
                "authorization_endpoint": format!("{base}/oauth2/default/authorize"),
                "token_endpoint": format!("{base}/oauth2/default/token"),
                "code_challenge_methods_supported": ["S256"],
            });
            (None, None, None, Some(oidc_meta))
        })
        .await;

        let url = format!("{}/oauth2/default", base);
        let result = discover_authorization_server(&url, true)
            .await
            .expect("matching end-append OIDC AS must succeed");
        assert_eq!(result.issuer, format!("{base}/oauth2/default"));

        server.abort();
    }
}
