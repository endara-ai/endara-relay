use reqwest::Client;
use std::time::Duration;
use url::Url;

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
    #[allow(dead_code)]
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
}

/// Resolved OAuth server discovery result.
pub struct DiscoveryResult {
    pub auth_server_url: String,
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
}

/// Build a well-known URL per RFC 5785 §3 and RFC 8414 §3.1.
///
/// The `.well-known` segment is inserted between the host (with port) and any
/// existing path components. For example:
/// - `https://auth.example.com` + `oauth-authorization-server`
///   → `https://auth.example.com/.well-known/oauth-authorization-server`
/// - `https://github.com/login/oauth` + `oauth-authorization-server`
///   → `https://github.com/.well-known/oauth-authorization-server/login/oauth`
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

/// Returns true if the base URL has a non-empty path component.
fn has_path(base_url: &str) -> bool {
    Url::parse(base_url)
        .map(|u| !u.path().trim_matches('/').is_empty())
        .unwrap_or(false)
}

/// Discover OAuth server metadata for a protected resource using RFC 9728.
///
/// 1. Fetches `{origin}/.well-known/oauth-protected-resource{path}`
///    - Falls back to `{origin}/.well-known/oauth-protected-resource` if 404 and path is non-empty
/// 2. Extracts the first authorization server URL
/// 3. Fetches `{origin}/.well-known/oauth-authorization-server{path}`
///    - Falls back to `{origin}/.well-known/oauth-authorization-server` if 404 and path is non-empty
/// 4. Validates S256 PKCE support
pub async fn discover_oauth_server(
    http_client: &Client,
    resource_url: &str,
) -> Result<DiscoveryResult, DiscoveryError> {
    // Step 1: Fetch protected resource metadata (with root fallback)
    let well_known_url = build_well_known_url(resource_url, "oauth-protected-resource")?;

    let resource_meta: ProtectedResourceMetadata =
        match fetch_well_known(http_client, &well_known_url).await {
            Ok(resp) => resp.json().await?,
            Err(DiscoveryError::MetadataNotFound { .. }) if has_path(resource_url) => {
                // Fallback: try root well-known URL without path
                let root_url = build_well_known_url_root(resource_url, "oauth-protected-resource")?;
                fetch_well_known(http_client, &root_url)
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

    // Step 2: Fetch authorization server metadata (with root fallback)
    let as_well_known = build_well_known_url(&auth_server_url, "oauth-authorization-server")
        .map_err(|_| DiscoveryError::AuthServerMetadataNotFound {
            url: auth_server_url.clone(),
        })?;

    let as_meta: AuthorizationServerMetadata =
        match fetch_well_known(http_client, &as_well_known).await {
            Ok(resp) => resp.json().await?,
            Err(DiscoveryError::MetadataNotFound { .. }) if has_path(&auth_server_url) => {
                let root_url =
                    build_well_known_url_root(&auth_server_url, "oauth-authorization-server")
                        .map_err(|_| DiscoveryError::AuthServerMetadataNotFound {
                            url: auth_server_url.clone(),
                        })?;
                fetch_well_known(http_client, &root_url)
                    .await
                    .map_err(|_| DiscoveryError::AuthServerMetadataNotFound {
                        url: as_well_known.clone(),
                    })?
                    .json()
                    .await?
            }
            Err(DiscoveryError::MetadataNotFound { url }) => {
                return Err(DiscoveryError::AuthServerMetadataNotFound { url });
            }
            Err(e) => return Err(e),
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
        auth_server_url,
        authorization_endpoint: as_meta.authorization_endpoint,
        token_endpoint: as_meta.token_endpoint,
        registration_endpoint: as_meta.registration_endpoint,
        scopes_supported: as_meta.scopes_supported,
        code_challenge_methods_supported: as_meta.code_challenge_methods_supported,
        token_endpoint_auth_methods: as_meta.token_endpoint_auth_methods_supported,
        revocation_endpoint: as_meta.revocation_endpoint,
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
        assert!(has_path("https://mcp.linear.app/sse"));
        assert!(has_path("https://api.githubcopilot.com/mcp/"));
        assert!(has_path("https://github.com/login/oauth"));
    }

    #[test]
    fn test_build_well_known_url_root() {
        let url =
            build_well_known_url_root("https://mcp.linear.app/sse", "oauth-protected-resource")
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
}
