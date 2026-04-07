use reqwest::Client;
use std::time::Duration;

/// Dynamic Client Registration request per RFC 7591.
#[derive(Debug, serde::Serialize)]
pub struct ClientRegistrationRequest {
    /// Display name shown in consent screen
    pub client_name: String,
    /// Callback URLs for the authorization code flow
    pub redirect_uris: Vec<String>,
    /// Only "authorization_code" for our use case
    pub grant_types: Vec<String>,
    /// "code" for authorization code flow
    pub response_types: Vec<String>,
    /// How we authenticate at the token endpoint
    pub token_endpoint_auth_method: String,
    /// Scopes we're requesting (informational for DCR)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Dynamic Client Registration response per RFC 7591.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ClientRegistrationResponse {
    pub client_id: String,
    pub client_secret: Option<String>,
    /// When the client_secret expires (0 = never)
    #[serde(default)]
    pub client_secret_expires_at: u64,
    pub client_name: Option<String>,
    /// Additional fields from the server
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum DcrError {
    #[error("Dynamic Client Registration failed ({status}): {body}")]
    RegistrationFailed { status: u16, body: String },

    #[error("DCR HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

/// Register a client dynamically with an OAuth authorization server.
///
/// Sends a POST to the `registration_endpoint` with client metadata.
/// Registers as a public client (`token_endpoint_auth_method: "none"`).
pub async fn register_client(
    http_client: &Client,
    registration_endpoint: &str,
    redirect_uri: &str,
    endpoint_name: &str,
) -> Result<ClientRegistrationResponse, DcrError> {
    let request = ClientRegistrationRequest {
        client_name: format!("Endara Relay — {}", endpoint_name),
        redirect_uris: vec![redirect_uri.to_string()],
        grant_types: vec![
            "authorization_code".to_string(),
            "refresh_token".to_string(),
        ],
        response_types: vec!["code".to_string()],
        token_endpoint_auth_method: "none".to_string(),
        scope: None,
    };

    let resp = http_client
        .post(registration_endpoint)
        .json(&request)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::BAD_REQUEST {
        let body = resp.text().await.unwrap_or_default();
        return Err(DcrError::RegistrationFailed { status: 400, body });
    }

    let response: ClientRegistrationResponse = resp.error_for_status()?.json().await?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_registration_request() {
        let req = ClientRegistrationRequest {
            client_name: "Endara Relay — Linear".to_string(),
            redirect_uris: vec!["http://127.0.0.1:9400/oauth/callback".to_string()],
            grant_types: vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ],
            response_types: vec!["code".to_string()],
            token_endpoint_auth_method: "none".to_string(),
            scope: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["client_name"], "Endara Relay — Linear");
        assert_eq!(
            json["redirect_uris"][0],
            "http://127.0.0.1:9400/oauth/callback"
        );
        assert_eq!(json["grant_types"][0], "authorization_code");
        assert_eq!(json["grant_types"][1], "refresh_token");
        assert_eq!(json["response_types"][0], "code");
        assert_eq!(json["token_endpoint_auth_method"], "none");
        // scope should be absent when None (skip_serializing_if)
        assert!(json.get("scope").is_none());
    }

    #[test]
    fn serialize_registration_request_with_scope() {
        let req = ClientRegistrationRequest {
            client_name: "Test".to_string(),
            redirect_uris: vec!["http://localhost/cb".to_string()],
            grant_types: vec!["authorization_code".to_string()],
            response_types: vec!["code".to_string()],
            token_endpoint_auth_method: "none".to_string(),
            scope: Some("read write".to_string()),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["scope"], "read write");
    }

    #[test]
    fn parse_registration_response() {
        let json = r#"{
            "client_id": "abc123",
            "client_secret": "secret456",
            "client_secret_expires_at": 0,
            "client_name": "Endara Relay — Linear"
        }"#;
        let resp: ClientRegistrationResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.client_id, "abc123");
        assert_eq!(resp.client_secret.as_deref(), Some("secret456"));
        assert_eq!(resp.client_secret_expires_at, 0);
        assert_eq!(resp.client_name.as_deref(), Some("Endara Relay — Linear"));
    }

    #[test]
    fn parse_registration_response_minimal() {
        let json = r#"{
            "client_id": "abc123"
        }"#;
        let resp: ClientRegistrationResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.client_id, "abc123");
        assert!(resp.client_secret.is_none());
        assert_eq!(resp.client_secret_expires_at, 0);
        assert!(resp.client_name.is_none());
    }

    #[test]
    fn parse_registration_response_with_extra_fields() {
        let json = r#"{
            "client_id": "abc123",
            "client_id_issued_at": 1234567890,
            "custom_field": "hello"
        }"#;
        let resp: ClientRegistrationResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.client_id, "abc123");
        // Extra fields captured in `extra`
        assert_eq!(resp.extra["client_id_issued_at"], 1234567890);
        assert_eq!(resp.extra["custom_field"], "hello");
    }

    #[test]
    fn test_github_has_no_dcr() {
        // GitHub's AS metadata has no registration_endpoint → DCR is not available.
        // This verifies our code path: when registration_endpoint is None, we skip DCR.
        use crate::oauth::discovery::AuthorizationServerMetadata;

        let json = r#"{
            "issuer": "https://github.com/login/oauth",
            "authorization_endpoint": "https://github.com/login/oauth/authorize",
            "token_endpoint": "https://github.com/login/oauth/access_token",
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code","refresh_token"],
            "code_challenge_methods_supported": ["S256"]
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert!(
            meta.registration_endpoint.is_none(),
            "GitHub does not support DCR — registration_endpoint should be None"
        );
    }

    #[test]
    fn registration_response_roundtrip() {
        let resp = ClientRegistrationResponse {
            client_id: "test-id".to_string(),
            client_secret: Some("test-secret".to_string()),
            client_secret_expires_at: 1700000000,
            client_name: Some("Test Client".to_string()),
            extra: serde_json::json!({}),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ClientRegistrationResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.client_id, "test-id");
        assert_eq!(parsed.client_secret.as_deref(), Some("test-secret"));
        assert_eq!(parsed.client_secret_expires_at, 1700000000);
    }
}
