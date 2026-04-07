pub mod dcr;
pub mod discovery;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::token_manager::{TokenError, TokenSet};

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
    ///
    /// `token_endpoint` should be the fully resolved token endpoint URL
    /// (from discovery or built from config convention).
    pub async fn start_flow(
        &self,
        endpoint_name: &str,
        token_endpoint: &str,
        client_id: &str,
        client_secret: Option<&str>,
        pkce: PkceChallenge,
        redirect_uri: &str,
    ) -> String {
        let state = generate_state();
        let flow = PendingFlow {
            endpoint_name: endpoint_name.to_string(),
            code_verifier: pkce.code_verifier,
            token_endpoint: token_endpoint.to_string(),
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

// ---------------------------------------------------------------------------
// OAuth Setup Session (preflight flow)
// ---------------------------------------------------------------------------

/// Status of a transient OAuth setup session.
#[derive(Debug, Clone, PartialEq)]
pub enum SetupSessionStatus {
    /// Waiting for manual credentials (DCR unsupported).
    AwaitingCredentials,
    /// Waiting for user to authorize in the browser.
    AwaitingAuth,
    /// Authorization complete — tokens obtained.
    Authorized,
}

/// A transient OAuth setup session that does NOT write to config.toml until
/// explicitly committed. Created by `POST /api/oauth/setup`.
pub struct OAuthSetupSession {
    /// Display name for the endpoint.
    pub name: String,
    /// MCP server URL.
    pub url: String,
    /// Requested scopes (space-separated string).
    pub scopes: Option<String>,
    /// Tool prefix override (None = auto-derive from name).
    pub tool_prefix: Option<String>,
    /// Discovered authorization endpoint.
    pub authorization_endpoint: Option<String>,
    /// Discovered token endpoint.
    pub token_endpoint: Option<String>,
    /// Discovered registration endpoint (if DCR is available).
    pub registration_endpoint: Option<String>,
    /// OAuth server base URL (if configured or discovered).
    pub oauth_server_url: Option<String>,
    /// Client ID (from DCR or manual input).
    pub client_id: Option<String>,
    /// Client secret (optional).
    pub client_secret: Option<String>,
    /// Obtained tokens (populated after callback).
    pub tokens: Option<TokenSet>,
    /// Current session status.
    pub status: SetupSessionStatus,
    /// When this session was created.
    pub created_at: Instant,
}

/// Manages transient OAuth setup sessions. Sessions live only in memory
/// and expire after 10 minutes.
pub struct OAuthSetupManager {
    sessions: RwLock<HashMap<Uuid, OAuthSetupSession>>,
}

/// Maximum age for a setup session before cleanup.
const SETUP_SESSION_MAX_AGE: Duration = Duration::from_secs(600);

impl OAuthSetupManager {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new setup session. Returns the session ID.
    pub async fn create_session(
        &self,
        name: String,
        url: String,
        scopes: Option<String>,
        tool_prefix: Option<String>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let session = OAuthSetupSession {
            name,
            url,
            scopes,
            tool_prefix,
            authorization_endpoint: None,
            token_endpoint: None,
            registration_endpoint: None,
            oauth_server_url: None,
            client_id: None,
            client_secret: None,
            tokens: None,
            status: SetupSessionStatus::AwaitingCredentials,
            created_at: Instant::now(),
        };
        self.sessions.write().await.insert(id, session);
        id
    }

    /// Get mutable access to a session by ID.
    pub async fn get_session_mut<F, R>(&self, id: &Uuid, f: F) -> Option<R>
    where
        F: FnOnce(&mut OAuthSetupSession) -> R,
    {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(id)?;
        if session.created_at.elapsed() > SETUP_SESSION_MAX_AGE {
            sessions.remove(id);
            return None;
        }
        Some(f(session))
    }

    /// Get read-only access to a session by ID.
    pub async fn get_session<F, R>(&self, id: &Uuid, f: F) -> Option<R>
    where
        F: FnOnce(&OAuthSetupSession) -> R,
    {
        let sessions = self.sessions.read().await;
        let session = sessions.get(id)?;
        if session.created_at.elapsed() > SETUP_SESSION_MAX_AGE {
            return None;
        }
        Some(f(session))
    }

    /// Remove a session (cancel or after commit).
    pub async fn remove_session(&self, id: &Uuid) -> Option<OAuthSetupSession> {
        self.sessions.write().await.remove(id)
    }

    /// Mark a session as authorized with the obtained tokens.
    /// Called from the OAuth callback handler.
    pub async fn mark_authorized(&self, id: &Uuid, tokens: TokenSet) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(id) {
            session.tokens = Some(tokens);
            session.status = SetupSessionStatus::Authorized;
            true
        } else {
            false
        }
    }

    /// Periodic cleanup of expired sessions.
    #[allow(dead_code)]
    pub async fn cleanup_stale(&self) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, s| s.created_at.elapsed() < SETUP_SESSION_MAX_AGE);
    }
}

impl Default for OAuthSetupManager {
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
                "https://auth.example.com/token",
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
                "https://auth.example.com/token",
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
                "https://auth.example.com/token",
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
    fn token_endpoint_stored_as_is() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mgr = OAuthFlowManager::new();
            let pkce = PkceChallenge::generate();
            let state = mgr
                .start_flow(
                    "ep",
                    "https://auth.example.com/oauth/token",
                    "cid",
                    None,
                    pkce,
                    "http://localhost/cb",
                )
                .await;
            let flow = mgr.consume_flow(&state).await.unwrap();
            assert_eq!(flow.token_endpoint, "https://auth.example.com/oauth/token");
        });
    }

    // -----------------------------------------------------------------------
    // OAuthSetupManager tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn setup_manager_create_and_get_session() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session(
                "test-ep".into(),
                "https://mcp.example.com".into(),
                Some("read write".into()),
                Some("test".into()),
            )
            .await;

        let data = mgr
            .get_session(&id, |s| {
                (
                    s.name.clone(),
                    s.url.clone(),
                    s.scopes.clone(),
                    s.tool_prefix.clone(),
                    s.status.clone(),
                )
            })
            .await
            .unwrap();

        assert_eq!(data.0, "test-ep");
        assert_eq!(data.1, "https://mcp.example.com");
        assert_eq!(data.2.as_deref(), Some("read write"));
        assert_eq!(data.3.as_deref(), Some("test"));
        assert_eq!(data.4, SetupSessionStatus::AwaitingCredentials);
    }

    #[tokio::test]
    async fn setup_manager_get_nonexistent_session_returns_none() {
        let mgr = OAuthSetupManager::new();
        let fake_id = Uuid::new_v4();
        let result = mgr.get_session(&fake_id, |s| s.name.clone()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn setup_manager_remove_session() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None)
            .await;

        let removed = mgr.remove_session(&id).await;
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().name, "ep");

        // Second remove returns None
        assert!(mgr.remove_session(&id).await.is_none());
    }

    #[tokio::test]
    async fn setup_manager_mark_authorized() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None)
            .await;

        let tokens = crate::token_manager::TokenSet {
            access_token: "access-tok".into(),
            refresh_token: Some("refresh-tok".into()),
            expires_at: Some(9999999999),
            token_type: "Bearer".into(),
            scope: None,
            issued_at: None,
        };

        assert!(mgr.mark_authorized(&id, tokens).await);

        let status = mgr.get_session(&id, |s| s.status.clone()).await.unwrap();
        assert_eq!(status, SetupSessionStatus::Authorized);

        let has_tokens = mgr.get_session(&id, |s| s.tokens.is_some()).await.unwrap();
        assert!(has_tokens);
    }

    #[tokio::test]
    async fn setup_manager_mark_authorized_nonexistent_returns_false() {
        let mgr = OAuthSetupManager::new();
        let fake_id = Uuid::new_v4();
        let tokens = crate::token_manager::TokenSet {
            access_token: "x".into(),
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".into(),
            scope: None,
            issued_at: None,
        };
        assert!(!mgr.mark_authorized(&fake_id, tokens).await);
    }

    #[tokio::test]
    async fn setup_manager_expired_session_is_invisible() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None)
            .await;

        // Manually expire the session
        {
            let mut sessions = mgr.sessions.write().await;
            if let Some(s) = sessions.get_mut(&id) {
                s.created_at = Instant::now() - Duration::from_secs(700);
            }
        }

        // get_session should return None for expired sessions
        assert!(mgr.get_session(&id, |s| s.name.clone()).await.is_none());

        // get_session_mut should also return None and remove the entry
        assert!(mgr.get_session_mut(&id, |s| s.name.clone()).await.is_none());
    }

    #[tokio::test]
    async fn setup_manager_cleanup_stale() {
        let mgr = OAuthSetupManager::new();
        let fresh_id = mgr
            .create_session("fresh".into(), "https://a.com".into(), None, None)
            .await;
        let stale_id = mgr
            .create_session("stale".into(), "https://b.com".into(), None, None)
            .await;

        // Make one session stale
        {
            let mut sessions = mgr.sessions.write().await;
            if let Some(s) = sessions.get_mut(&stale_id) {
                s.created_at = Instant::now() - Duration::from_secs(700);
            }
        }

        mgr.cleanup_stale().await;

        // Fresh session still exists
        assert!(mgr.get_session(&fresh_id, |_| ()).await.is_some());
        // Stale session removed
        assert!(mgr.remove_session(&stale_id).await.is_none());
    }

    #[tokio::test]
    async fn setup_manager_get_session_mut_modifies() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None)
            .await;

        mgr.get_session_mut(&id, |s| {
            s.client_id = Some("my-client".into());
            s.status = SetupSessionStatus::AwaitingAuth;
        })
        .await;

        let (cid, status) = mgr
            .get_session(&id, |s| (s.client_id.clone(), s.status.clone()))
            .await
            .unwrap();
        assert_eq!(cid.as_deref(), Some("my-client"));
        assert_eq!(status, SetupSessionStatus::AwaitingAuth);
    }
}
