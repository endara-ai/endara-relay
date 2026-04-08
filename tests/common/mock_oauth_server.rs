//! Mock OAuth 2.0 authorization server fixture for integration tests.
//!
//! An axum-based HTTP server that simulates an OAuth 2.0 authorization server
//! with discovery, DCR, authorize, token, revoke, and protected-resource endpoints.
//! All incoming requests are recorded for test assertions.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

/// How the /token endpoint should respond.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TokenResponseMode {
    /// Return a valid token response.
    Success,
    /// Return an OAuth error (e.g., "invalid_grant").
    Error(String),
    /// Delay for the given duration, then return success.
    Slow(Duration),
}

/// How the /register (DCR) endpoint should respond.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DcrResponseMode {
    /// Return a successful registration with client_secret.
    ConfidentialClient,
    /// Return a successful registration without client_secret (public client).
    PublicClient,
    /// Return an error response.
    Error(StatusCode, String),
}

/// A recorded HTTP request for test assertions.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub query_params: HashMap<String, String>,
}

/// Shared mutable state for the mock server.
#[derive(Clone)]
struct MockState {
    inner: Arc<MockStateInner>,
}

struct MockStateInner {
    port: u16,
    token_response_mode: RwLock<TokenResponseMode>,
    dcr_response_mode: RwLock<DcrResponseMode>,
    /// Access token TTL in seconds (for expires_in in token response).
    token_ttl_secs: RwLock<u64>,
    /// All recorded requests across all endpoints.
    recorded_requests: RwLock<Vec<RecordedRequest>>,
    /// Counter specifically for /token calls (convenience for thundering herd assertions).
    token_call_count: RwLock<usize>,
    /// The fixed authorization code that /authorize issues and /token accepts.
    auth_code: String,
    /// Counter for generating unique tokens.
    token_counter: RwLock<u64>,
}

/// A mock OAuth 2.0 authorization server for integration tests.
#[allow(dead_code)]
pub struct MockOAuthServer {
    state: MockState,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    port: u16,
}

#[allow(dead_code)]
impl MockOAuthServer {
    /// Start a mock OAuth server on a random free port.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind mock OAuth server");
        let port = listener.local_addr().unwrap().port();

        let state = MockState {
            inner: Arc::new(MockStateInner {
                port,
                token_response_mode: RwLock::new(TokenResponseMode::Success),
                dcr_response_mode: RwLock::new(DcrResponseMode::ConfidentialClient),
                token_ttl_secs: RwLock::new(3600),
                recorded_requests: RwLock::new(Vec::new()),
                token_call_count: RwLock::new(0),
                auth_code: "mock-auth-code-12345".to_string(),
                token_counter: RwLock::new(1),
            }),
        };

        let app = build_router(state.clone());
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("mock OAuth server failed");
        });

        Self {
            state,
            shutdown_tx,
            port,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn token_url(&self) -> String {
        format!("{}/token", self.base_url())
    }

    pub fn authorization_url(&self) -> String {
        format!("{}/authorize", self.base_url())
    }

    pub fn registration_url(&self) -> String {
        format!("{}/register", self.base_url())
    }

    pub fn revocation_url(&self) -> String {
        format!("{}/revoke", self.base_url())
    }

    /// The fixed authorization code issued by /authorize.
    pub fn auth_code(&self) -> &str {
        &self.state.inner.auth_code
    }

    /// How many times /token has been called.
    pub async fn token_call_count(&self) -> usize {
        *self.state.inner.token_call_count.read().await
    }

    /// Get all recorded requests.
    pub async fn recorded_requests(&self) -> Vec<RecordedRequest> {
        self.state.inner.recorded_requests.read().await.clone()
    }

    /// Get recorded requests filtered by path.
    pub async fn requests_to(&self, path: &str) -> Vec<RecordedRequest> {
        self.state
            .inner
            .recorded_requests
            .read()
            .await
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }

    /// Set the token endpoint response mode.
    pub async fn set_token_response(&self, mode: TokenResponseMode) {
        *self.state.inner.token_response_mode.write().await = mode;
    }

    /// Set the DCR endpoint response mode.
    pub async fn set_dcr_response(&self, mode: DcrResponseMode) {
        *self.state.inner.dcr_response_mode.write().await = mode;
    }

    /// Set the access token TTL (expires_in) in seconds.
    pub async fn set_token_ttl(&self, secs: u64) {
        *self.state.inner.token_ttl_secs.write().await = secs;
    }

    /// Reset all recorded requests and counters.
    pub async fn reset(&self) {
        self.state.inner.recorded_requests.write().await.clear();
        *self.state.inner.token_call_count.write().await = 0;
        *self.state.inner.token_counter.write().await = 1;
    }
}

// ── Router ──────────────────────────────────────────────────────────────

fn build_router(state: MockState) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(handle_protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(handle_discovery),
        )
        .route("/register", post(handle_register))
        .route("/authorize", get(handle_authorize))
        .route("/token", post(handle_token))
        .route("/revoke", post(handle_revoke))
        .with_state(state)
}

// ── Endpoint Handlers ───────────────────────────────────────────────────

/// GET /.well-known/oauth-protected-resource
async fn handle_protected_resource(State(state): State<MockState>) -> Json<Value> {
    record_simple(&state, "GET", "/.well-known/oauth-protected-resource", None).await;
    Json(json!({
        "resource": state.inner.base_url(),
        "authorization_servers": [state.inner.base_url()]
    }))
}

/// GET /.well-known/oauth-authorization-server
async fn handle_discovery(State(state): State<MockState>) -> Json<Value> {
    record_simple(
        &state,
        "GET",
        "/.well-known/oauth-authorization-server",
        None,
    )
    .await;
    let base = state.inner.base_url();
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{}/authorize", base),
        "token_endpoint": format!("{}/token", base),
        "registration_endpoint": format!("{}/register", base),
        "revocation_endpoint": format!("{}/revoke", base),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "none"]
    }))
}

/// POST /register — Dynamic Client Registration
async fn handle_register(State(state): State<MockState>, Json(body): Json<Value>) -> Response {
    record_simple(&state, "POST", "/register", Some(&body.to_string())).await;
    let mode = state.inner.dcr_response_mode.read().await.clone();
    match mode {
        DcrResponseMode::ConfidentialClient => {
            let resp = json!({
                "client_id": "mock-client-id",
                "client_secret": "mock-client-secret",
                "client_id_issued_at": chrono::Utc::now().timestamp(),
                "client_secret_expires_at": 0,
                "redirect_uris": body.get("redirect_uris").cloned().unwrap_or(json!([])),
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "client_secret_post"
            });
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        DcrResponseMode::PublicClient => {
            let resp = json!({
                "client_id": "mock-public-client-id",
                "client_id_issued_at": chrono::Utc::now().timestamp(),
                "client_secret_expires_at": 0,
                "redirect_uris": body.get("redirect_uris").cloned().unwrap_or(json!([])),
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none"
            });
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        DcrResponseMode::Error(status, description) => {
            let resp = json!({
                "error": "invalid_client_metadata",
                "error_description": description
            });
            (status, Json(resp)).into_response()
        }
    }
}

#[derive(Deserialize)]
struct AuthorizeParams {
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    code_challenge: Option<String>,
    #[serde(default)]
    code_challenge_method: Option<String>,
    #[serde(default)]
    response_type: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
}

/// GET /authorize — redirect back with a fixed auth code
async fn handle_authorize(
    State(state): State<MockState>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    let mut qp: HashMap<String, String> = HashMap::new();
    if let Some(ref v) = params.redirect_uri {
        qp.insert("redirect_uri".to_string(), v.clone());
    }
    if let Some(ref v) = params.state {
        qp.insert("state".to_string(), v.clone());
    }
    if let Some(ref v) = params.code_challenge {
        qp.insert("code_challenge".to_string(), v.clone());
    }
    if let Some(ref v) = params.code_challenge_method {
        qp.insert("code_challenge_method".to_string(), v.clone());
    }
    if let Some(ref v) = params.response_type {
        qp.insert("response_type".to_string(), v.clone());
    }
    if let Some(ref v) = params.client_id {
        qp.insert("client_id".to_string(), v.clone());
    }
    let empty_headers: HashMap<String, String> = HashMap::new();
    record_request(&state, "GET", "/authorize", &empty_headers, None, &qp).await;

    let redirect_uri = params
        .redirect_uri
        .unwrap_or_else(|| "http://localhost/callback".into());
    let code = &state.inner.auth_code;
    let mut redirect = format!("{}?code={}", redirect_uri, code);
    if let Some(s) = params.state {
        redirect.push_str(&format!("&state={}", s));
    }
    Redirect::temporary(&redirect).into_response()
}

#[derive(Deserialize)]
struct TokenParams {
    #[serde(default)]
    grant_type: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
}

/// POST /token — exchange code or refresh token for access token
async fn handle_token(
    State(state): State<MockState>,
    axum::Form(params): axum::Form<TokenParams>,
) -> Response {
    // Record the request
    let mut body_parts: HashMap<String, String> = HashMap::new();
    if let Some(ref v) = params.grant_type {
        body_parts.insert("grant_type".to_string(), v.clone());
    }
    if let Some(ref v) = params.code {
        body_parts.insert("code".to_string(), v.clone());
    }
    if let Some(ref v) = params.refresh_token {
        body_parts.insert("refresh_token".to_string(), v.clone());
    }
    if let Some(ref v) = params.code_verifier {
        body_parts.insert("code_verifier".to_string(), v.clone());
    }
    if let Some(ref v) = params.client_id {
        body_parts.insert("client_id".to_string(), v.clone());
    }
    if let Some(ref v) = params.client_secret {
        body_parts.insert("client_secret".to_string(), v.clone());
    }
    if let Some(ref v) = params.redirect_uri {
        body_parts.insert("redirect_uri".to_string(), v.clone());
    }
    let body_str = serde_json::to_string(&body_parts).unwrap_or_default();
    let empty: HashMap<String, String> = HashMap::new();
    record_request(&state, "POST", "/token", &empty, Some(&body_str), &empty).await;

    // Increment token call count
    {
        let mut count = state.inner.token_call_count.write().await;
        *count += 1;
    }

    // Check response mode
    let mode = state.inner.token_response_mode.read().await.clone();
    match mode {
        TokenResponseMode::Slow(delay) => {
            tokio::time::sleep(delay).await;
            // Fall through to success after delay
        }
        TokenResponseMode::Error(error_code) => {
            let resp = json!({
                "error": error_code,
                "error_description": format!("Mock error: {}", error_code)
            });
            return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
        }
        TokenResponseMode::Success => {}
    }

    // Validate grant_type
    let grant_type = params.grant_type.as_deref().unwrap_or("");
    match grant_type {
        "authorization_code" => {
            let code = params.code.as_deref().unwrap_or("");
            if code != state.inner.auth_code {
                let resp = json!({
                    "error": "invalid_grant",
                    "error_description": "Invalid authorization code"
                });
                return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
            }
        }
        "refresh_token" => {
            if params.refresh_token.is_none() {
                let resp = json!({
                    "error": "invalid_request",
                    "error_description": "Missing refresh_token parameter"
                });
                return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
            }
        }
        _ => {
            let resp = json!({
                "error": "unsupported_grant_type",
                "error_description": format!("Unsupported grant_type: {}", grant_type)
            });
            return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
        }
    }

    // Generate tokens
    let counter = {
        let mut c = state.inner.token_counter.write().await;
        let val = *c;
        *c += 1;
        val
    };
    let ttl = *state.inner.token_ttl_secs.read().await;

    let resp = json!({
        "access_token": format!("mock-access-token-{}", counter),
        "token_type": "Bearer",
        "expires_in": ttl,
        "refresh_token": format!("mock-refresh-token-{}", counter)
    });
    Json(resp).into_response()
}

/// POST /revoke — RFC 7009 token revocation
async fn handle_revoke(
    State(state): State<MockState>,
    axum::Form(params): axum::Form<HashMap<String, String>>,
) -> StatusCode {
    let body_str = serde_json::to_string(&params).unwrap_or_default();
    record_simple(&state, "POST", "/revoke", Some(&body_str)).await;
    StatusCode::OK
}

// ── Helpers ─────────────────────────────────────────────────────────────

impl MockStateInner {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

async fn record_request(
    state: &MockState,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: Option<&str>,
    query_params: &HashMap<String, String>,
) {
    let req = RecordedRequest {
        method: method.to_string(),
        path: path.to_string(),
        headers: headers.clone(),
        body: body.map(|s| s.to_string()),
        query_params: query_params.clone(),
    };
    state.inner.recorded_requests.write().await.push(req);
}

/// Convenience wrapper that records a request with empty headers and query params.
async fn record_simple(state: &MockState, method: &str, path: &str, body: Option<&str>) {
    record_request(state, method, path, &HashMap::new(), body, &HashMap::new()).await;
}
