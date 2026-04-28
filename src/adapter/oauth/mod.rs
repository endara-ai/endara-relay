mod heartbeat;
pub mod metrics;
mod state;

// Re-export submodule public items so external callers are unaffected.
pub use state::{derive_health, do_transition, refresh_deadline, OAuthState, TransitionRecord};

use self::metrics::{generate_correlation_id, OAuthMetrics};
use super::http::{HttpAdapter, HttpConfig};
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::oauth::OAuthError;
use crate::token_manager::{TokenManager, TokenSet};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{error, info, warn};

/// Returns the `Instant` at which a proactive refresh should fire when
/// `issued_at` is unknown (or nonsensical relative to `expires_at`).
///
/// The fallback heuristic is "refresh 5 minutes before expiry", clamped to
/// `now` if we're already inside that window so the returned deadline is
/// always `>= now`. Uses `checked_sub` so values far in the past don't
/// underflow `Instant` arithmetic.
fn fallback_refresh_deadline(now: Instant, expires_at: Instant) -> Instant {
    let five_min = Duration::from_secs(300);
    let target = expires_at.checked_sub(five_min).unwrap_or(now);
    target.max(now)
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
    /// Heartbeat probe interval in seconds (default: 30).
    pub heartbeat_interval_secs: u64,
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
    /// Health of the wrapped inner adapter (updated by heartbeat probe).
    pub inner_health: RwLock<HealthStatus>,
    /// Handle to the heartbeat background task.
    heartbeat_task_handle: Mutex<Option<JoinHandle<()>>>,
    /// Ring buffer of recent state transitions (max TRANSITION_RING_BUFFER_CAPACITY).
    pub transition_history: RwLock<VecDeque<TransitionRecord>>,
    /// In-process metric counters.
    pub metrics: OAuthMetrics,
    /// Guards concurrent refresh attempts so only one proceeds at a time.
    refresh_mutex: Mutex<()>,
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

    /// Transition to a new `OAuthState`.
    ///
    /// This is the **single writer** of `self.state`. It acquires the state
    /// write lock, validates the transition via `assert_legal_transition`,
    /// records it in the ring buffer, emits a tracing event, and updates
    /// the state.
    pub async fn transition_to(&self, new_state: OAuthState, reason: &str) {
        let mut state = self.state.write().await;
        let mut history = self.transition_history.write().await;
        let old = do_transition(&mut state, new_state.clone(), reason, &mut history);
        self.metrics.inc_state_transition();
        info!(
            endpoint = %self.config.endpoint_name,
            from = ?old,
            to = ?new_state,
            oauth_state = ?new_state,
            reason = %reason,
            "OAuth state transition"
        );
    }

    /// Perform a token refresh using the refresh_token grant.
    ///
    /// POSTs to the token endpoint with grant_type=refresh_token.
    /// On success, calls `apply_tokens_inner` with the new token set.
    /// On failure, transitions to `AuthRequired`.
    pub async fn do_token_refresh(self: &Arc<Self>) -> Result<TokenSet, OAuthError> {
        // Snapshot the current access token before acquiring the mutex.
        // If it changes while we wait, another concurrent refresh succeeded.
        let pre_token = {
            let tokens = self.tokens.read().await;
            tokens.as_ref().map(|t| t.access_token.clone())
        };

        // Coalesce concurrent refresh attempts: only one thread does the actual
        // HTTP POST; others wait and then return the already-refreshed tokens.
        let _guard = self.refresh_mutex.lock().await;

        // Check if another concurrent caller already refreshed successfully
        // while we were waiting for the mutex (token changed → skip refresh).
        {
            let tokens = self.tokens.read().await;
            if let Some(ref t) = *tokens {
                if Some(&t.access_token) != pre_token.as_ref() {
                    // Token was refreshed by another concurrent caller.
                    return Ok(t.clone());
                }
            }
        }

        let correlation_id = generate_correlation_id();
        let refresh_token = {
            let tokens = self.tokens.read().await;
            match tokens.as_ref().and_then(|t| t.refresh_token.clone()) {
                Some(rt) => rt,
                None => {
                    warn!(
                        endpoint = %self.config.endpoint_name,
                        correlation_id = %correlation_id,
                        "No refresh token available"
                    );
                    self.transition_to(OAuthState::AuthRequired, "no refresh token")
                        .await;
                    self.metrics.inc_refresh_failure();
                    return Err(OAuthError::NoRefreshToken {
                        endpoint: self.config.endpoint_name.clone(),
                    });
                }
            }
        };

        info!(
            endpoint = %self.config.endpoint_name,
            correlation_id = %correlation_id,
            "Starting token refresh"
        );

        // Mark as refreshing
        self.transition_to(OAuthState::Refreshing, "starting token refresh")
            .await;

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
                self.metrics.inc_refresh_failure();
                OAuthError::Http(e)
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(
                endpoint = %self.config.endpoint_name,
                correlation_id = %correlation_id,
                %status,
                body = %body,
                "Token refresh failed"
            );
            self.metrics.inc_refresh_failure();
            self.transition_to(OAuthState::AuthRequired, "token refresh failed")
                .await;
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

        self.metrics.inc_refresh_success();
        info!(
            endpoint = %self.config.endpoint_name,
            correlation_id = %correlation_id,
            "Token refresh successful"
        );
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
                // Reflect the freshly initialized inner adapter's health
                // immediately so health() reports Healthy without waiting for
                // the next heartbeat tick.
                *self.inner_health.write().await = HealthStatus::Healthy;
                *self.inner_adapter.write().await = Some(adapter);
                self.transition_to(
                    OAuthState::Authenticated,
                    "tokens applied, inner adapter ready",
                )
                .await;
            }
            Err(e) => {
                // Capture inner adapter's health before clearing it
                *self.inner_health.write().await = adapter.health();
                *self.inner_adapter.write().await = None;
                self.transition_to(
                    OAuthState::ConnectionFailed,
                    &format!("inner adapter init failed: {}", e),
                )
                .await;
            }
        }

        // 4. Update in-memory tokens
        let issued_at_secs = token_set.issued_at;
        let expires_at_secs = token_set.expires_at;
        let has_refresh_token = token_set.refresh_token.is_some();
        *self.tokens.write().await = Some(token_set);

        // 5. Schedule proactive refresh if we have a refresh token and an
        //    `expires_at`. We schedule even when `issued_at` is missing —
        //    tokens persisted before `issued_at` was added will deserialize
        //    with `issued_at: None` (it's `#[serde(default)]`), but still
        //    deserve a proactive refresh.
        let deadline = if !has_refresh_token {
            info!(endpoint = %endpoint, "No refresh token, skipping proactive refresh");
            None
        } else if let Some(expires) = expires_at_secs {
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let now_instant = Instant::now();
            let expires_instant =
                now_instant + Duration::from_secs(expires.saturating_sub(now_secs));
            match issued_at_secs {
                Some(issued) if expires > issued => {
                    // Both timestamps known — use the standard heuristic
                    // (75% of lifetime, capped at 5 min before expiry).
                    let issued_instant =
                        now_instant - Duration::from_secs(now_secs.saturating_sub(issued));
                    Some(refresh_deadline(issued_instant, expires_instant))
                }
                _ => {
                    // No `issued_at` (or nonsensical ordering): fall back to
                    // refreshing 5 minutes before expiry, clamped to "now"
                    // if we're already inside that window.
                    Some(fallback_refresh_deadline(now_instant, expires_instant))
                }
            }
        } else {
            info!(
                endpoint = %endpoint,
                "No expires_at on token, skipping proactive refresh"
            );
            None
        };

        if let Some(deadline) = deadline {
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
        self.transition_to(OAuthState::Disconnected, "user disconnected")
            .await;
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
                inner_health: RwLock::new(HealthStatus::Starting),
                heartbeat_task_handle: Mutex::new(None),
                transition_history: RwLock::new(VecDeque::new()),
                metrics: OAuthMetrics::new(),
                refresh_mutex: Mutex::new(()),
            }),
        }
    }

    /// Get a clone of the shared inner state (for use by callback handlers).
    pub fn shared_inner(&self) -> Arc<OAuthAdapterInner> {
        self.inner.clone()
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
                        self.inner
                            .transition_to(OAuthState::AuthRequired, "startup refresh failed")
                            .await;
                    }
                }
            } else {
                self.inner
                    .transition_to(
                        OAuthState::AuthRequired,
                        "expired tokens without refresh token",
                    )
                    .await;
            }
        } else {
            self.inner
                .transition_to(OAuthState::NeedsLogin, "no existing tokens at startup")
                .await;
        }

        // Spawn the heartbeat probe loop
        let weak = Arc::downgrade(&self.inner);
        let handle = tokio::spawn(heartbeat::heartbeat_loop(weak));
        self.inner
            .heartbeat_task_handle
            .lock()
            .await
            .replace(handle);

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
                                self.inner
                                    .transition_to(
                                        OAuthState::AuthRequired,
                                        "401 on list_tools, refresh failed",
                                    )
                                    .await;
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
                        self.inner
                            .transition_to(
                                OAuthState::AuthRequired,
                                "401 on call_tool, refresh failed",
                            )
                            .await;
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
        // Abort heartbeat task
        {
            let mut handle = self.inner.heartbeat_task_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
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
        self.inner
            .transition_to(OAuthState::Disconnected, "shutdown")
            .await;
        Ok(())
    }

    fn health(&self) -> HealthStatus {
        let state = match self.inner.state.try_read() {
            Ok(s) => s.clone(),
            Err(_) => return HealthStatus::Starting,
        };
        let inner = match self.inner.inner_health.try_read() {
            Ok(h) => h.clone(),
            Err(_) => HealthStatus::Starting,
        };
        derive_health(&state, &inner)
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
            heartbeat_interval_secs: 30,
        }
    }

    fn make_adapter(config: OAuthAdapterConfig) -> OAuthAdapter {
        let tmp = tempfile::tempdir().unwrap().keep();
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
        // the adapter should report connection failed after apply_tokens,
        // preserving the inner adapter's error details (not just hardcoded text).
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
                // The inner adapter's error should be preserved, which includes
                // "connection failed" and the URL from the actual inner error.
                assert!(
                    msg.contains("connection failed"),
                    "expected 'connection failed' in message, got: {}",
                    msg
                );
                assert!(
                    msg.contains("127.0.0.1:19999"),
                    "expected inner error URL details in message, got: {}",
                    msg
                );
            }
            other => panic!(
                "expected Unhealthy with inner error details, got {:?}",
                other
            ),
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
    fn derive_health_table() {
        let cases = [
            // Authenticated propagates inner
            (
                OAuthState::Authenticated,
                HealthStatus::Healthy,
                HealthStatus::Healthy,
            ),
            (
                OAuthState::Authenticated,
                HealthStatus::Starting,
                HealthStatus::Starting,
            ),
            (
                OAuthState::Authenticated,
                HealthStatus::Unhealthy("upstream timeout".into()),
                HealthStatus::Unhealthy("upstream timeout".into()),
            ),
            (
                OAuthState::Authenticated,
                HealthStatus::Stopped,
                HealthStatus::Stopped,
            ),
            // Refreshing always wins
            (
                OAuthState::Refreshing,
                HealthStatus::Healthy,
                HealthStatus::Starting,
            ),
            (
                OAuthState::Refreshing,
                HealthStatus::Stopped,
                HealthStatus::Starting,
            ),
            // Hard-error states ignore inner
            (
                OAuthState::AuthRequired,
                HealthStatus::Healthy,
                HealthStatus::Unhealthy("auth required".into()),
            ),
            (
                OAuthState::ConnectionFailed,
                HealthStatus::Healthy,
                HealthStatus::Unhealthy("connection failed".into()),
            ),
            (
                OAuthState::NeedsLogin,
                HealthStatus::Healthy,
                HealthStatus::Unhealthy("needs login".into()),
            ),
            (
                OAuthState::Disconnected,
                HealthStatus::Healthy,
                HealthStatus::Stopped,
            ),
        ];
        for (state, inner, expected) in cases {
            let got = derive_health(&state, &inner);
            assert_eq!(got, expected, "state={:?} inner={:?}", state, inner);
        }
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

    // --- fallback_refresh_deadline tests (issued_at unknown path) ---

    /// 60 minutes of remaining lifetime: deadline should be 5 minutes before
    /// expiry (= now + 55 min). Task 2's fallback heuristic is "5 min before
    /// expiry", not 75% of remaining — assert what the implementation does.
    #[test]
    fn fallback_refresh_deadline_uses_5min_before_expiry() {
        let now = Instant::now();
        let expires_at = now + Duration::from_secs(3600);
        let deadline = fallback_refresh_deadline(now, expires_at);
        let expected = now + Duration::from_secs(3600 - 300);
        assert!(deadline >= expected - Duration::from_millis(1));
        assert!(deadline <= expected + Duration::from_millis(1));
    }

    /// If `expires_at` is in the past or within the 5-minute guard window,
    /// the deadline must clamp to `now` — never panic, never underflow, and
    /// the returned deadline must be `>= now`.
    #[test]
    fn fallback_refresh_deadline_clamps_to_minimum_when_expiring_soon() {
        let now = Instant::now();

        // expires_at exactly at now → 5-min subtraction underflows → clamp to now.
        let deadline = fallback_refresh_deadline(now, now);
        assert_eq!(
            deadline, now,
            "expected clamp to `now` when expires_at == now"
        );

        // expires_at within the 5-minute window (60s out) → still clamps to now.
        let near = now + Duration::from_secs(60);
        let deadline = fallback_refresh_deadline(now, near);
        assert_eq!(
            deadline, now,
            "expected clamp to `now` when expires_at is inside the 5-min guard"
        );

        // expires_at strictly before now (1s in the past per Instant semantics)
        // — saturating subtraction inside `checked_sub` must not panic and the
        // clamp keeps the deadline at `now`.
        let past = now - Duration::from_secs(1);
        let deadline = fallback_refresh_deadline(now, past);
        assert!(
            deadline >= now,
            "expected deadline >= now when expires_at is in the past, got delta = {:?}",
            now.saturating_duration_since(deadline)
        );
    }

    /// `expires_at` 100 years out must not panic. Tokio's `Instant` is backed
    /// by a monotonic clock so addition can theoretically saturate, but the
    /// helper performs a simple subtraction and `max`, both of which are safe.
    #[test]
    fn fallback_refresh_deadline_handles_far_future_without_overflow() {
        let now = Instant::now();
        // ~100 years in seconds.
        let far = now + Duration::from_secs(100 * 365 * 24 * 3600);
        let deadline = fallback_refresh_deadline(now, far);
        let expected = far - Duration::from_secs(300);
        assert_eq!(
            deadline, expected,
            "expected deadline to be exactly 5 min before expiry for far-future timestamps"
        );
    }

    // --- apply_tokens proactive-refresh & inner_health tests ---

    /// Spawn a minimal in-process MCP server that responds to `initialize`
    /// with a valid `serverInfo.name` and accepts `notifications/initialized`.
    /// Returns the URL of the `/mcp` endpoint and a JoinHandle for the server.
    async fn spawn_minimal_mcp_server() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handle(Json(body): Json<Value>) -> Json<Value> {
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method == "initialize" {
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": {"name": "test-server", "version": "0.0.1"},
                    },
                }))
            } else {
                // Notifications and anything else: empty 200 OK.
                Json(json!({"jsonrpc": "2.0", "id": id, "result": {}}))
            }
        }

        let router = Router::new().route("/mcp", post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        // Tiny delay to let the server start accepting connections.
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/mcp", addr.port()), handle)
    }

    /// Regression: a TokenSet with `expires_at` set but `issued_at = None`
    /// (the shape persisted by older relay versions) must still spawn a
    /// proactive refresh task.
    #[tokio::test]
    async fn apply_tokens_schedules_refresh_without_issued_at() {
        let mut config = make_config();
        // Point at an unreachable URL so initialize() fails quickly. The
        // proactive-refresh scheduling must not depend on init success.
        config.url = "http://127.0.0.1:19999/mcp".to_string();
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token_set = TokenSet {
            access_token: "test-access".to_string(),
            refresh_token: Some("test-refresh".to_string()),
            expires_at: Some(now_secs + 3600),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_tokens(token_set).await;

        // A refresh task must be scheduled even though issued_at is None.
        let handle = adapter.inner.refresh_task_handle.lock().await;
        assert!(
            handle.is_some(),
            "expected proactive refresh task to be scheduled when expires_at is set and issued_at is None"
        );
        drop(handle);

        // The inner adapter init failed (URL is unreachable), so inner_health
        // must NOT be `Healthy`: the success-path branch sets it to `Healthy`,
        // so a regression that flips the branches would leave it `Healthy`
        // here. Failed init mirrors the inner adapter's own health, which is
        // `Unhealthy(_)` after a connection failure.
        let inner_health = adapter.inner.inner_health.read().await.clone();
        assert!(
            !matches!(inner_health, HealthStatus::Healthy),
            "inner_health must not be Healthy after a failed inner adapter init, got {:?}",
            inner_health
        );
    }

    /// On successful re-init, `inner_health` must be set to `Healthy`
    /// immediately so the management API doesn't show a stale `Starting`
    /// until the next heartbeat tick.
    #[tokio::test]
    async fn apply_tokens_sets_inner_health_healthy_on_success() {
        let (url, server) = spawn_minimal_mcp_server().await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        let token_set = TokenSet {
            access_token: "test-access".to_string(),
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_tokens(token_set).await;

        let inner_health = adapter.inner.inner_health.read().await.clone();
        assert_eq!(
            inner_health,
            HealthStatus::Healthy,
            "inner_health should be Healthy immediately after a successful inner adapter initialize"
        );

        server.abort();
    }
}
