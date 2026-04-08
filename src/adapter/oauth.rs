use super::http::{HttpAdapter, HttpConfig};
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::oauth::OAuthError;
use crate::token_manager::{TokenManager, TokenSet};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{error, info, warn};

/// Internal state of an OAuth-authenticated endpoint.
///
/// Maps to `HealthStatus` in the `health()` method but carries richer
/// semantic meaning for lifecycle management (refresh scheduling,
/// startup restore, management API responses).
#[derive(Debug, Clone, PartialEq)]
pub enum OAuthState {
    /// No tokens, never authenticated.
    NeedsLogin,
    /// Valid tokens, inner adapter healthy.
    Authenticated,
    /// Proactive or reactive refresh in progress.
    Refreshing,
    /// Refresh failed, needs re-login.
    AuthRequired,
    /// Token valid but MCP server unreachable.
    ConnectionFailed,
    /// User explicitly disconnected.
    Disconnected,
}

/// Compute the deadline at which a proactive token refresh should fire.
///
/// Returns the earlier of:
/// - 75 % of token lifetime after `issued_at`
/// - 5 minutes before `expires_at`
///
/// If `expires_at` is unknown (server didn't return `expires_in`), returns
/// `None` — the caller should skip proactive refresh and rely on 401 retry.
pub fn refresh_deadline(issued_at: Instant, expires_at: Instant) -> Instant {
    let lifetime = expires_at - issued_at;
    let seventy_five_pct = issued_at + (lifetime * 3 / 4);
    let five_min_before = expires_at - Duration::from_secs(300);
    std::cmp::min(seventy_five_pct, five_min_before)
}

/// Configuration for an OAuth-authenticated MCP endpoint.
#[derive(Debug, Clone)]
pub struct OAuthAdapterConfig {
    /// Endpoint name in the registry (used for logging, token persistence key).
    pub endpoint_name: String,
    /// URL of the upstream MCP server (e.g. http://localhost:5000/mcp).
    pub url: String,
    /// Token endpoint URL for refresh grants.
    pub token_endpoint_url: String,
    /// OAuth client ID.
    pub client_id: String,
    /// OAuth client secret (optional for public clients).
    pub client_secret: Option<String>,
}

/// Shared inner state for an OAuth adapter, wrapped in `Arc` so it can be
/// referenced from the callback handler and proactive-refresh task.
pub struct OAuthAdapterInner {
    /// Current lifecycle state.
    pub state: RwLock<OAuthState>,
    /// Current token set (None when not authenticated).
    pub tokens: RwLock<Option<TokenSet>>,
    /// Static configuration.
    pub config: OAuthAdapterConfig,
    /// The inner HTTP/SSE adapter that talks to the upstream MCP server.
    inner_adapter: RwLock<Option<HttpAdapter>>,
    /// Token persistence layer.
    token_manager: Arc<TokenManager>,
    /// Shared HTTP client for token refresh requests.
    http_client: Client,
    /// Handle to the proactive refresh background task.
    refresh_task_handle: Mutex<Option<JoinHandle<()>>>,
}

impl OAuthAdapterInner {
    /// Build an inner HttpAdapter with a Bearer token in the default headers.
    fn build_inner_adapter(url: &str, access_token: &str) -> HttpAdapter {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    reqwest::header::ACCEPT,
                    reqwest::header::HeaderValue::from_static(
                        "application/json, text/event-stream",
                    ),
                );
                if let Ok(val) =
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", access_token))
                {
                    headers.insert(reqwest::header::AUTHORIZATION, val);
                }
                headers
            })
            .build()
            .expect("failed to build HTTP client");
        HttpAdapter::new_with_client(HttpConfig::new(url), client)
    }

    /// Perform a token refresh using the refresh_token grant.
    ///
    /// POSTs to the token endpoint with grant_type=refresh_token.
    /// On success, calls `apply_tokens_inner` with the new token set.
    /// On failure, transitions to `AuthRequired`.
    pub async fn do_token_refresh(self: &Arc<Self>) -> Result<TokenSet, OAuthError> {
        let refresh_token = {
            let tokens = self.tokens.read().await;
            match tokens.as_ref().and_then(|t| t.refresh_token.clone()) {
                Some(rt) => rt,
                None => {
                    *self.state.write().await = OAuthState::AuthRequired;
                    return Err(OAuthError::NoRefreshToken {
                        endpoint: self.config.endpoint_name.clone(),
                    });
                }
            }
        };

        // Mark as refreshing
        *self.state.write().await = OAuthState::Refreshing;
        info!(endpoint = %self.config.endpoint_name, "Starting token refresh");

        let mut form_parts: Vec<(&str, String)> = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token),
            ("client_id", self.config.client_id.clone()),
        ];
        if let Some(ref secret) = self.config.client_secret {
            form_parts.push(("client_secret", secret.clone()));
        }

        let form_body: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form_parts.iter())
            .finish();

        let resp = self
            .http_client
            .post(&self.config.token_endpoint_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body)
            .send()
            .await
            .map_err(|e| {
                // Network error during refresh
                OAuthError::Http(e)
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(
                endpoint = %self.config.endpoint_name,
                %status,
                body = %body,
                "Token refresh failed"
            );
            *self.state.write().await = OAuthState::AuthRequired;
            return Err(OAuthError::RefreshFailed { status, body });
        }

        let token_json: serde_json::Value = resp.json().await?;
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Handle token rotation: if the server returns a new refresh_token, use it;
        // otherwise keep the old one.
        let old_refresh_token = {
            let tokens = self.tokens.read().await;
            tokens.as_ref().and_then(|t| t.refresh_token.clone())
        };

        let new_token_set = TokenSet {
            access_token: token_json["access_token"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            refresh_token: token_json["refresh_token"]
                .as_str()
                .map(|s| s.to_string())
                .or(old_refresh_token),
            expires_at: token_json["expires_in"]
                .as_u64()
                .map(|secs| now_secs + secs),
            token_type: token_json["token_type"]
                .as_str()
                .unwrap_or("Bearer")
                .to_string(),
            scope: token_json["scope"].as_str().map(|s| s.to_string()),
            issued_at: Some(now_secs),
        };

        info!(endpoint = %self.config.endpoint_name, "Token refresh successful");
        Ok(new_token_set)
    }

    /// Apply a new token set: update in-memory state, persist to disk,
    /// abort any existing refresh task, spawn a new proactive refresh task,
    /// and rebuild the inner adapter.
    pub fn apply_tokens(
        self: &Arc<Self>,
        token_set: TokenSet,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.apply_tokens_inner(token_set))
    }

    async fn apply_tokens_inner(self: &Arc<Self>, token_set: TokenSet) {
        let endpoint = &self.config.endpoint_name;

        // 1. Persist to disk
        if let Err(e) = self.token_manager.save(endpoint, &token_set).await {
            error!(endpoint = %endpoint, error = %e, "Failed to persist tokens");
        }

        // 2. Abort old refresh task
        {
            let mut handle = self.refresh_task_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }

        // 3. Rebuild inner adapter
        let access_token = token_set.access_token.clone();
        let mut adapter = Self::build_inner_adapter(&self.config.url, &access_token);
        match adapter.initialize().await {
            Ok(()) => {
                *self.inner_adapter.write().await = Some(adapter);
                *self.state.write().await = OAuthState::Authenticated;
                info!(endpoint = %endpoint, "Inner adapter rebuilt with new token");
            }
            Err(e) => {
                *self.inner_adapter.write().await = None;
                *self.state.write().await = OAuthState::ConnectionFailed;
                warn!(endpoint = %endpoint, error = %e, "Inner adapter init failed after token apply");
            }
        }

        // 4. Update in-memory tokens
        let issued_at_secs = token_set.issued_at;
        let expires_at_secs = token_set.expires_at;
        let has_refresh_token = token_set.refresh_token.is_some();
        *self.tokens.write().await = Some(token_set);

        // 5. Schedule proactive refresh if we have a refresh token and expiry info
        if !has_refresh_token {
            info!(endpoint = %endpoint, "No refresh token, skipping proactive refresh");
        } else if let (Some(issued), Some(expires)) = (issued_at_secs, expires_at_secs) {
            if expires > issued {
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let now_instant = Instant::now();
                // Convert Unix timestamps to Instants relative to now
                let issued_instant =
                    now_instant - Duration::from_secs(now_secs.saturating_sub(issued));
                let expires_instant =
                    now_instant + Duration::from_secs(expires.saturating_sub(now_secs));
                let deadline = refresh_deadline(issued_instant, expires_instant);

                let inner = self.clone();
                let handle = tokio::spawn(async move {
                    tokio::time::sleep_until(deadline).await;
                    info!(endpoint = %inner.config.endpoint_name, "Proactive refresh timer fired");
                    match inner.do_token_refresh().await {
                        Ok(new_tokens) => {
                            // Recursively apply — this will schedule the next refresh
                            inner.apply_tokens(new_tokens).await;
                        }
                        Err(e) => {
                            warn!(
                                endpoint = %inner.config.endpoint_name,
                                error = %e,
                                "Proactive refresh failed, retrying in 60s"
                            );
                            // Retry once after 60 seconds
                            tokio::time::sleep(Duration::from_secs(60)).await;
                            if let Ok(new_tokens) = inner.do_token_refresh().await {
                                inner.apply_tokens(new_tokens).await;
                            } else {
                                warn!(
                                    endpoint = %inner.config.endpoint_name,
                                    "Proactive refresh retry also failed"
                                );
                            }
                        }
                    }
                });
                self.refresh_task_handle.lock().await.replace(handle);
            }
        }
    }

    /// Disconnect: abort refresh task, clear tokens, delete from disk, set Disconnected.
    pub async fn disconnect(self: &Arc<Self>) {
        let endpoint = &self.config.endpoint_name;

        // Abort refresh task
        {
            let mut handle = self.refresh_task_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }

        // Shut down inner adapter
        {
            let mut guard = self.inner_adapter.write().await;
            if let Some(ref mut adapter) = *guard {
                let _ = adapter.shutdown().await;
            }
            *guard = None;
        }

        // Clear in-memory tokens
        *self.tokens.write().await = None;

        // Delete tokens from disk
        if let Err(e) = self.token_manager.delete(endpoint).await {
            error!(endpoint = %endpoint, error = %e, "Failed to delete tokens from disk");
        }

        // Delete DCR credentials from disk
        if let Err(e) = self.token_manager.delete_dcr(endpoint).await {
            error!(endpoint = %endpoint, error = %e, "Failed to delete DCR credentials from disk");
        }

        // Set state
        *self.state.write().await = OAuthState::Disconnected;
        info!(endpoint = %endpoint, "OAuth adapter disconnected");
    }
}

/// OAuth MCP adapter — wraps an HttpAdapter with Bearer token injection.
///
/// Owns its token state internally via `Arc<OAuthAdapterInner>`. The callback
/// handler and proactive refresh tasks use the same `Arc` to apply new tokens.
pub struct OAuthAdapter {
    inner: Arc<OAuthAdapterInner>,
}

impl OAuthAdapter {
    /// Create a new OAuthAdapter.
    pub fn new(config: OAuthAdapterConfig, token_manager: Arc<TokenManager>) -> Self {
        Self {
            inner: Arc::new(OAuthAdapterInner {
                state: RwLock::new(OAuthState::NeedsLogin),
                tokens: RwLock::new(None),
                config,
                inner_adapter: RwLock::new(None),
                token_manager,
                http_client: Client::new(),
                refresh_task_handle: Mutex::new(None),
            }),
        }
    }

    /// Get a clone of the shared inner state (for use by callback handlers).
    pub fn shared_inner(&self) -> Arc<OAuthAdapterInner> {
        self.inner.clone()
    }

    /// Map OAuthState → HealthStatus.
    fn map_health(state: &OAuthState) -> HealthStatus {
        match state {
            OAuthState::NeedsLogin => HealthStatus::Unhealthy("needs login".to_string()),
            OAuthState::Authenticated => HealthStatus::Healthy,
            OAuthState::Refreshing => HealthStatus::Healthy, // still serving with current token
            OAuthState::AuthRequired => {
                HealthStatus::Unhealthy("authentication required".to_string())
            }
            OAuthState::ConnectionFailed => {
                HealthStatus::Unhealthy("connection failed".to_string())
            }
            OAuthState::Disconnected => HealthStatus::Stopped,
        }
    }
}

#[async_trait]
impl McpAdapter for OAuthAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        // Try to load existing tokens from disk
        let loaded = self
            .inner
            .token_manager
            .load(&self.inner.config.endpoint_name)
            .await;

        if let Ok(Some(token_set)) = loaded {
            if token_set.is_valid() {
                info!(
                    endpoint = %self.inner.config.endpoint_name,
                    "Loaded valid OAuth tokens from disk"
                );
                self.inner.apply_tokens(token_set).await;
            } else if token_set.refresh_token.is_some() {
                info!(
                    endpoint = %self.inner.config.endpoint_name,
                    "Loaded expired tokens with refresh token, attempting refresh"
                );
                // Store expired tokens so refresh can use the refresh_token
                *self.inner.tokens.write().await = Some(token_set);
                match self.inner.do_token_refresh().await {
                    Ok(new_tokens) => {
                        self.inner.apply_tokens(new_tokens).await;
                    }
                    Err(e) => {
                        warn!(
                            endpoint = %self.inner.config.endpoint_name,
                            error = %e,
                            "Token refresh at startup failed"
                        );
                        *self.inner.state.write().await = OAuthState::AuthRequired;
                    }
                }
            } else {
                info!(
                    endpoint = %self.inner.config.endpoint_name,
                    "Loaded expired tokens without refresh token"
                );
                *self.inner.state.write().await = OAuthState::AuthRequired;
            }
        } else {
            info!(
                endpoint = %self.inner.config.endpoint_name,
                "No existing OAuth tokens, awaiting login"
            );
            *self.inner.state.write().await = OAuthState::NeedsLogin;
        }

        Ok(()) // initialize always succeeds for OAuth
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        let guard = self.inner.inner_adapter.read().await;
        match guard.as_ref() {
            Some(adapter) => {
                match adapter.list_tools().await {
                    Ok(tools) => Ok(tools),
                    Err(AdapterError::HttpError { status: 401, .. }) => {
                        // Drop the read lock before refreshing
                        drop(guard);

                        info!(
                            endpoint = %self.inner.config.endpoint_name,
                            "Got 401 on list_tools, attempting token refresh"
                        );

                        match self.inner.do_token_refresh().await {
                            Ok(new_tokens) => {
                                self.inner.apply_tokens(new_tokens).await;
                                // Retry with new token
                                let guard = self.inner.inner_adapter.read().await;
                                match guard.as_ref() {
                                    Some(adapter) => adapter.list_tools().await,
                                    None => Ok(vec![]),
                                }
                            }
                            Err(e) => {
                                warn!(
                                    endpoint = %self.inner.config.endpoint_name,
                                    error = %e,
                                    "Token refresh after 401 on list_tools failed"
                                );
                                *self.inner.state.write().await = OAuthState::AuthRequired;
                                Err(AdapterError::AuthenticationRequired {
                                    endpoint: self.inner.config.endpoint_name.clone(),
                                    message: "Token expired and refresh failed. Re-authenticate in Endara Desktop.".to_string(),
                                })
                            }
                        }
                    }
                    Err(other) => Err(other),
                }
            }
            None => Ok(vec![]),
        }
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
        let guard = self.inner.inner_adapter.read().await;
        let adapter = match guard.as_ref() {
            Some(a) => a,
            None => {
                return Err(AdapterError::ConnectionFailed(
                    "not authenticated — complete OAuth login first".to_string(),
                ));
            }
        };

        match adapter.call_tool(name, arguments.clone()).await {
            Ok(result) => Ok(result),
            Err(AdapterError::HttpError { status: 401, .. }) => {
                // Drop the read lock before refreshing
                drop(guard);

                info!(
                    endpoint = %self.inner.config.endpoint_name,
                    "Got 401, attempting token refresh"
                );

                match self.inner.do_token_refresh().await {
                    Ok(new_tokens) => {
                        self.inner.apply_tokens(new_tokens).await;
                        // Retry with new token
                        let guard = self.inner.inner_adapter.read().await;
                        let adapter = guard.as_ref().ok_or_else(|| {
                            AdapterError::ConnectionFailed(
                                "Adapter lost during refresh".to_string(),
                            )
                        })?;
                        adapter.call_tool(name, arguments).await
                    }
                    Err(e) => {
                        warn!(
                            endpoint = %self.inner.config.endpoint_name,
                            error = %e,
                            "Token refresh after 401 failed"
                        );
                        *self.inner.state.write().await = OAuthState::AuthRequired;
                        Err(AdapterError::AuthenticationRequired {
                            endpoint: self.inner.config.endpoint_name.clone(),
                            message: "Token expired and refresh failed. Re-authenticate in Endara Desktop.".to_string(),
                        })
                    }
                }
            }
            Err(other) => Err(other),
        }
    }

    fn server_type(&self) -> Option<String> {
        self.inner
            .inner_adapter
            .try_read()
            .ok()
            .and_then(|g| g.as_ref().and_then(|a| a.server_type()))
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        // Abort refresh task
        {
            let mut handle = self.inner.refresh_task_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
        let mut guard = self.inner.inner_adapter.write().await;
        if let Some(ref mut adapter) = *guard {
            adapter.shutdown().await?;
        }
        *guard = None;
        *self.inner.state.write().await = OAuthState::Disconnected;
        info!(endpoint = %self.inner.config.endpoint_name, "OAuth adapter shut down");
        Ok(())
    }

    fn health(&self) -> HealthStatus {
        match self.inner.state.try_read() {
            Ok(s) => Self::map_health(&s),
            Err(_) => HealthStatus::Starting,
        }
    }

    async fn activity_log(&self) -> Vec<String> {
        let guard = self.inner.inner_adapter.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.activity_log().await,
            None => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> OAuthAdapterConfig {
        OAuthAdapterConfig {
            endpoint_name: "test".to_string(),
            url: "http://localhost/mcp".to_string(),
            token_endpoint_url: "http://localhost/token".to_string(),
            client_id: "test-client".to_string(),
            client_secret: None,
        }
    }

    fn make_adapter(config: OAuthAdapterConfig) -> OAuthAdapter {
        let tmp = tempfile::tempdir().unwrap().into_path();
        let tm = Arc::new(TokenManager::new(tmp));
        OAuthAdapter::new(config, tm)
    }

    #[tokio::test]
    async fn health_no_tokens_is_unhealthy() {
        let mut adapter = make_adapter(make_config());
        adapter.initialize().await.unwrap();
        match adapter.health() {
            HealthStatus::Unhealthy(msg) => assert_eq!(msg, "needs login"),
            other => panic!("expected Unhealthy('needs login'), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn list_tools_no_tokens_returns_empty() {
        let mut adapter = make_adapter(make_config());
        adapter.initialize().await.unwrap();
        let tools = adapter.list_tools().await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn call_tool_no_tokens_returns_error() {
        let mut adapter = make_adapter(make_config());
        adapter.initialize().await.unwrap();
        let result = adapter.call_tool("any", serde_json::json!({})).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            AdapterError::ConnectionFailed(msg) => {
                assert!(msg.contains("not authenticated"));
            }
            other => panic!("expected ConnectionFailed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn health_with_token_but_unreachable_is_connection_failed() {
        // If we have a token but the upstream server is unreachable,
        // the adapter should report connection failed after apply_tokens
        let mut config = make_config();
        config.url = "http://127.0.0.1:19999/mcp".to_string();
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        let token_set = TokenSet {
            access_token: "fake-token".to_string(),
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_tokens(token_set).await;

        match adapter.health() {
            HealthStatus::Unhealthy(msg) => {
                assert!(msg.contains("connection failed"));
            }
            other => panic!("expected Unhealthy('connection failed'), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn shutdown_sets_stopped() {
        let mut adapter = make_adapter(make_config());
        adapter.initialize().await.unwrap();
        adapter.shutdown().await.unwrap();
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[tokio::test]
    async fn apply_tokens_then_disconnect() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let config = make_config();
        let mut adapter = OAuthAdapter::new(config, tm.clone());
        adapter.initialize().await.unwrap();

        // Apply tokens (will fail to connect, but tokens are stored)
        let token_set = TokenSet {
            access_token: "test-access".to_string(),
            refresh_token: Some("test-refresh".to_string()),
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_tokens(token_set).await;

        // Verify tokens are persisted
        let loaded = tm.load("test").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().access_token, "test-access");

        // Disconnect
        adapter.inner.disconnect().await;
        assert_eq!(adapter.health(), HealthStatus::Stopped);

        // Verify tokens are deleted from disk
        let loaded = tm.load("test").await.unwrap();
        assert!(loaded.is_none());

        // Verify in-memory tokens cleared
        let tokens = adapter.inner.tokens.read().await;
        assert!(tokens.is_none());
    }

    #[test]
    fn health_mapping() {
        assert_eq!(
            OAuthAdapter::map_health(&OAuthState::NeedsLogin),
            HealthStatus::Unhealthy("needs login".to_string())
        );
        assert_eq!(
            OAuthAdapter::map_health(&OAuthState::Authenticated),
            HealthStatus::Healthy
        );
        assert_eq!(
            OAuthAdapter::map_health(&OAuthState::Refreshing),
            HealthStatus::Healthy
        );
        assert_eq!(
            OAuthAdapter::map_health(&OAuthState::AuthRequired),
            HealthStatus::Unhealthy("authentication required".to_string())
        );
        assert_eq!(
            OAuthAdapter::map_health(&OAuthState::ConnectionFailed),
            HealthStatus::Unhealthy("connection failed".to_string())
        );
        assert_eq!(
            OAuthAdapter::map_health(&OAuthState::Disconnected),
            HealthStatus::Stopped
        );
    }

    // --- OAuthState enum tests ---

    #[test]
    fn oauth_state_variants_are_distinct() {
        let states = [
            OAuthState::NeedsLogin,
            OAuthState::Authenticated,
            OAuthState::Refreshing,
            OAuthState::AuthRequired,
            OAuthState::ConnectionFailed,
            OAuthState::Disconnected,
        ];
        for (i, a) in states.iter().enumerate() {
            for (j, b) in states.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn oauth_state_clone_and_debug() {
        let state = OAuthState::Authenticated;
        let cloned = state.clone();
        assert_eq!(state, cloned);
        // Debug should not panic
        let _dbg = format!("{:?}", state);
    }

    // --- refresh_deadline tests ---

    #[test]
    fn refresh_deadline_1h_token() {
        // 1-hour token: 75% = 45min, 5-min-before = 55min. Min = 45min.
        let issued = Instant::now();
        let expires = issued + Duration::from_secs(3600);
        let deadline = refresh_deadline(issued, expires);
        let expected = issued + Duration::from_secs(2700); // 45 min
                                                           // Allow 1ms tolerance for Instant arithmetic
        assert!(deadline >= expected - Duration::from_millis(1));
        assert!(deadline <= expected + Duration::from_millis(1));
    }

    #[test]
    fn refresh_deadline_10min_token() {
        // 10-min token: 75% = 7.5min (450s), 5-min-before = 5min (300s). Min = 5min.
        let issued = Instant::now();
        let expires = issued + Duration::from_secs(600);
        let deadline = refresh_deadline(issued, expires);
        let expected = issued + Duration::from_secs(300); // 5 min before
        assert!(deadline >= expected - Duration::from_millis(1));
        assert!(deadline <= expected + Duration::from_millis(1));
    }

    #[test]
    fn refresh_deadline_2min_token() {
        // 2-min token: 75% = 90s, 5-min-before would be negative → clamped.
        // Instant subtraction that underflows saturates to zero, so
        // five_min_before = expires_at - 300s. If lifetime=120s, this would be
        // issued_at - 180s which saturates to Instant(0) or earlier.
        // 75% = issued + 90s. min(issued+90s, saturated) → the saturated value.
        // But that's in the past — which is correct: refresh immediately.
        let issued = Instant::now();
        let expires = issued + Duration::from_secs(120);
        let deadline = refresh_deadline(issued, expires);
        // For very short tokens, deadline should be before 75% mark
        // (5-min-before goes negative/past, which means refresh ASAP)
        assert!(deadline <= issued + Duration::from_secs(90));
    }

    #[test]
    fn refresh_deadline_exactly_20min_token() {
        // 20-min token: 75% = 15min (900s), 5-min-before = 15min (900s). Equal.
        let issued = Instant::now();
        let expires = issued + Duration::from_secs(1200);
        let deadline = refresh_deadline(issued, expires);
        let expected = issued + Duration::from_secs(900);
        assert!(deadline >= expected - Duration::from_millis(1));
        assert!(deadline <= expected + Duration::from_millis(1));
    }
}
