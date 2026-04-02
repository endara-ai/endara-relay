use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::token_manager::TokenError;

/// Dedicated error type for OAuth-specific failures.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("No refresh token available for endpoint '{endpoint}'")]
    NoRefreshToken { endpoint: String },

    #[error("Token refresh failed — {status}: {body}")]
    RefreshFailed {
        status: reqwest::StatusCode,
        body: String,
    },

    #[error("Token exchange failed — {status}: {body}")]
    ExchangeFailed {
        status: reqwest::StatusCode,
        body: String,
    },

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Token storage error: {0}")]
    Storage(#[from] TokenError),
}

/// PKCE (Proof Key for Code Exchange) challenge pair for OAuth 2.0 S256.
pub struct PkceChallenge {
    /// The code verifier: 43-char URL-safe base64 string from 32 random bytes.
    pub code_verifier: String,
    /// The code challenge: BASE64URL(SHA256(code_verifier)).
    pub code_challenge: String,
}

impl PkceChallenge {
    /// Generate a new PKCE challenge pair using cryptographically secure random bytes.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("failed to generate random bytes");
        let code_verifier = URL_SAFE_NO_PAD.encode(bytes);
        let mut hasher = Sha256::new();
        hasher.update(code_verifier.as_bytes());
        let code_challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());
        Self {
            code_verifier,
            code_challenge,
        }
    }
}

/// Generate a cryptographically random state parameter for OAuth 2.0.
/// Returns a 22-char URL-safe base64 string from 16 random bytes.
pub fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("failed to generate random bytes");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Maximum age for a pending OAuth flow before it's considered stale.
const FLOW_MAX_AGE: Duration = Duration::from_secs(600); // 10 minutes

/// A pending OAuth authorization flow, created by `/oauth/start` and consumed
/// by `/oauth/callback`.
pub struct PendingFlow {
    pub endpoint_name: String,
    pub code_verifier: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub created_at: Instant,
}

/// In-memory map holding pending OAuth flows. One entry per in-progress login.
/// Entries are created by `/oauth/start` and consumed by `/oauth/callback`.
pub struct OAuthFlowManager {
    pending: RwLock<HashMap<String, PendingFlow>>,
}

impl OAuthFlowManager {
    pub fn new() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new pending flow. Returns the generated state parameter.
    pub async fn start_flow(
        &self,
        endpoint_name: &str,
        oauth_server_url: &str,
        client_id: &str,
        client_secret: Option<&str>,
        pkce: PkceChallenge,
        redirect_uri: &str,
    ) -> String {
        let state = generate_state();
        let flow = PendingFlow {
            endpoint_name: endpoint_name.to_string(),
            code_verifier: pkce.code_verifier,
            token_endpoint: format!("{}/token", oauth_server_url.trim_end_matches('/')),
            client_id: client_id.to_string(),
            client_secret: client_secret.map(|s| s.to_string()),
            redirect_uri: redirect_uri.to_string(),
            created_at: Instant::now(),
        };
        self.pending.write().await.insert(state.clone(), flow);
        state
    }

    /// Consume a pending flow (called by /oauth/callback).
    /// Returns None if the state is invalid or the flow has expired.
    pub async fn consume_flow(&self, state: &str) -> Option<PendingFlow> {
        let mut pending = self.pending.write().await;
        let flow = pending.remove(state)?;
        if flow.created_at.elapsed() > FLOW_MAX_AGE {
            return None;
        }
        Some(flow)
    }

    /// Periodic cleanup of stale flows (call from a background task or on each access).
    #[allow(dead_code)]
    pub async fn cleanup_stale(&self) {
        let mut pending = self.pending.write().await;
        pending.retain(|_, f| f.created_at.elapsed() < FLOW_MAX_AGE);
    }
}

impl Default for OAuthFlowManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_generates_valid_pair() {
        let pkce = PkceChallenge::generate();
        // code_verifier should be 43 chars (32 bytes → base64url no padding)
        assert_eq!(pkce.code_verifier.len(), 43);
        // code_challenge should be 43 chars (32 bytes SHA256 → base64url no padding)
        assert_eq!(pkce.code_challenge.len(), 43);
        // Verify the challenge matches the verifier
        let mut hasher = Sha256::new();
        hasher.update(pkce.code_verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(pkce.code_challenge, expected);
    }

    #[test]
    fn pkce_generates_unique_pairs() {
        let a = PkceChallenge::generate();
        let b = PkceChallenge::generate();
        assert_ne!(a.code_verifier, b.code_verifier);
        assert_ne!(a.code_challenge, b.code_challenge);
    }

    #[test]
    fn generate_state_produces_22_char_string() {
        let state = generate_state();
        // 16 bytes → 22 chars base64url no padding
        assert_eq!(state.len(), 22);
    }

    #[test]
    fn generate_state_is_unique() {
        let a = generate_state();
        let b = generate_state();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn flow_manager_start_and_consume() {
        let mgr = OAuthFlowManager::new();
        let pkce = PkceChallenge::generate();
        let verifier = pkce.code_verifier.clone();

        let state = mgr
            .start_flow(
                "test-ep",
                "https://auth.example.com",
                "client123",
                Some("secret"),
                pkce,
                "http://127.0.0.1:9400/oauth/callback",
            )
            .await;

        let flow = mgr.consume_flow(&state).await.unwrap();
        assert_eq!(flow.endpoint_name, "test-ep");
        assert_eq!(flow.code_verifier, verifier);
        assert_eq!(flow.token_endpoint, "https://auth.example.com/token");
        assert_eq!(flow.client_id, "client123");
        assert_eq!(flow.client_secret.as_deref(), Some("secret"));
    }

    #[tokio::test]
    async fn consume_flow_removes_entry() {
        let mgr = OAuthFlowManager::new();
        let pkce = PkceChallenge::generate();
        let state = mgr
            .start_flow(
                "ep",
                "https://auth.example.com",
                "cid",
                None,
                pkce,
                "http://localhost/cb",
            )
            .await;

        // First consume succeeds
        assert!(mgr.consume_flow(&state).await.is_some());
        // Second consume returns None (already consumed)
        assert!(mgr.consume_flow(&state).await.is_none());
    }

    #[tokio::test]
    async fn consume_invalid_state_returns_none() {
        let mgr = OAuthFlowManager::new();
        assert!(mgr.consume_flow("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn cleanup_stale_removes_old_flows() {
        let mgr = OAuthFlowManager::new();
        let pkce = PkceChallenge::generate();

        let state = mgr
            .start_flow(
                "ep",
                "https://auth.example.com",
                "cid",
                None,
                pkce,
                "http://localhost/cb",
            )
            .await;

        {
            let mut pending = mgr.pending.write().await;
            if let Some(flow) = pending.get_mut(&state) {
                flow.created_at = Instant::now() - Duration::from_secs(660);
            }
        }

        mgr.cleanup_stale().await;
        let pending = mgr.pending.read().await;
        assert!(pending.is_empty());
    }

    #[test]
    fn trailing_slash_in_oauth_server_url_is_handled() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mgr = OAuthFlowManager::new();
            let pkce = PkceChallenge::generate();
            let state = mgr
                .start_flow(
                    "ep",
                    "https://auth.example.com/",
                    "cid",
                    None,
                    pkce,
                    "http://localhost/cb",
                )
                .await;
            let flow = mgr.consume_flow(&state).await.unwrap();
            assert_eq!(flow.token_endpoint, "https://auth.example.com/token");
        });
    }
}
