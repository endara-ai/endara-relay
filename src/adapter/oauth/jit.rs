//! Just-in-time (JIT) OAuth interception for plain HTTP MCP endpoints.
//!
//! When an upstream MCP server gates a tool call behind OAuth and signals it
//! with a hard `HTTP 401` + `WWW-Authenticate: Bearer ...` (RFC 6750 / 9728),
//! the relay must NOT forward that challenge downstream. Instead it becomes the
//! OAuth client: it discovers the authorization server (RFC 9728 → 8414),
//! dynamically registers a client (RFC 7591) and composes a PKCE authorize URL
//! pointing back at the relay's loopback callback.
//!
//! This module is the DETECTION + self-initiation half of Wave 2 Path B. It
//! deliberately triggers ONLY on a hard 401 + `WWW-Authenticate`; 200-`isError`
//! tool results are passed through unchanged (see [`should_intercept_outcome`]).
//!
//! Surfacing the produced `authorize_url` to a headless client and retrying the
//! tool call after the loopback callback completes builds on this module:
//! [`surface_authorize_url`] formats the actionable downstream tool result and
//! [`JitInterceptor::current_bearer`] supplies the persisted bearer for the
//! authenticated retry (wired into the live call path in
//! [`crate::adapter::http::HttpAdapter`]).

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};
use url::Url;

use super::state::OAuthState;
use crate::adapter::AdapterError;
use crate::oauth::dcr::{self, ClientRegistrationResponse};
use crate::oauth::discovery::{self, DiscoveryError, DiscoveryResult};
use crate::oauth::{OAuthFlowManager, PkceChallenge};
use crate::token_manager::{dcr_issuer_allows_reuse, merge_scopes, DcrCredentials, TokenManager};
use serde_json::{json, Value};

/// A parsed `WWW-Authenticate: Bearer ...` challenge.
#[derive(Debug, Clone, PartialEq)]
pub struct BearerChallenge {
    pub realm: Option<String>,
    /// RFC 9728 `resource_metadata` URL pointing at the protected-resource doc.
    pub resource_metadata: Option<String>,
}

/// Parse a `WWW-Authenticate` header value, returning `Some` only for a
/// `Bearer` challenge (the only scheme the relay self-initiates on). Quoted
/// `auth-param` values are unquoted; commas inside quoted values are not
/// expected here (the relevant params are realms and URLs).
pub fn parse_bearer_challenge(header: &str) -> Option<BearerChallenge> {
    let trimmed = header.trim();
    if trimmed.len() < 6 || !trimmed[..6].eq_ignore_ascii_case("bearer") {
        return None;
    }
    let rest = &trimmed[6..];
    // The scheme must be delimited by whitespace (or be the whole value), so
    // that e.g. "BearerToken" is not mistaken for the Bearer scheme.
    if let Some(c) = rest.chars().next() {
        if !c.is_whitespace() {
            return None;
        }
    }
    let mut realm = None;
    let mut resource_metadata = None;
    for part in rest.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().trim_matches('"').to_string();
            match key.as_str() {
                "realm" => realm = Some(val),
                "resource_metadata" => resource_metadata = Some(val),
                _ => {}
            }
        }
    }
    Some(BearerChallenge {
        realm,
        resource_metadata,
    })
}

/// The 401-only interception policy: a tool-call outcome is intercepted ONLY
/// when it is a hard `HTTP 401`. Everything else — including a 200 JSON-RPC
/// result carrying `isError: true` (which surfaces as `Ok(_)`) — is passed
/// through unchanged. This is the deliberate "no 200-`isError` body sniffing"
/// rule from the spec.
pub fn should_intercept_outcome(result: &Result<Value, AdapterError>) -> bool {
    matches!(result, Err(AdapterError::HttpError { status: 401, .. }))
}

/// Build the downstream-facing tool result that surfaces an `authorize_url` to
/// a headless client.
///
/// Returns an MCP `CallToolResult`-shaped value with `isError: true` so the
/// model/CLI sees an actionable "open this to sign in" instruction instead of a
/// protocol-level failure. The same URL is surfaced regardless of how it is
/// opened (desktop auto-open vs a printed line). The raw upstream `401` /
/// `WWW-Authenticate` challenge is deliberately NOT included — downstream
/// clients must never see the upstream credential challenge.
pub fn surface_authorize_url(authorize_url: &str) -> Value {
    let text = format!(
        "Sign-in required to use this tool. Open the following URL in a browser to authorize, \
         then retry the tool call:\n\n{}",
        authorize_url
    );
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    })
}

/// Build the downstream-facing tool result for when a sign-in is required but
/// the relay could not start the OAuth flow (e.g. discovery or DCR failed).
///
/// Like [`surface_authorize_url`], this returns an MCP `CallToolResult`-shaped
/// value with `isError: true` carrying an actionable, sanitized message. The
/// raw upstream `401` status and `WWW-Authenticate` challenge are deliberately
/// NOT included — downstream clients must never see the upstream credential
/// challenge. The underlying error is logged server-side (see
/// [`crate::adapter::http::HttpAdapter`]) rather than surfaced here.
pub fn surface_oauth_unavailable() -> Value {
    let text = "Sign-in is required to use this tool, but the sign-in flow could not be started \
         right now. Please retry in a moment; if the problem persists, contact the server \
         administrator.";
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    })
}

/// Errors raised while self-initiating the OAuth flow.
#[derive(Debug, thiserror::Error)]
pub enum JitError {
    #[error("WWW-Authenticate is not a Bearer challenge")]
    NotABearerChallenge,

    #[error("OAuth discovery failed: {0}")]
    Discovery(#[from] DiscoveryError),

    #[error("Dynamic client registration failed: {0}")]
    Dcr(#[from] dcr::DcrError),

    #[error("no client credentials: server does not support DCR and none are stored")]
    NoClientCredentials,
}

/// Self-initiation engine for a single endpoint. Holds the relay loopback port
/// and shared OAuth machinery so that [`JitInterceptor::intercept`] can run the
/// full discovery → DCR → authorize-URL flow without the desktop.
pub struct JitInterceptor {
    relay_port: u16,
    flow_manager: Arc<OAuthFlowManager>,
    token_manager: Option<Arc<TokenManager>>,
    allow_insecure_oauth: bool,
    state: RwLock<OAuthState>,
    authorize_url: RwLock<Option<String>>,
}

impl JitInterceptor {
    /// Construct a new interceptor.
    #[allow(dead_code)] // wired into the live call path by follow-up task 098e0e03
    pub fn new(
        relay_port: u16,
        flow_manager: Arc<OAuthFlowManager>,
        token_manager: Option<Arc<TokenManager>>,
        allow_insecure_oauth: bool,
    ) -> Self {
        Self {
            relay_port,
            flow_manager,
            token_manager,
            allow_insecure_oauth,
            state: RwLock::new(OAuthState::Disconnected),
            authorize_url: RwLock::new(None),
        }
    }

    /// Current lifecycle state. Becomes [`OAuthState::NeedsLogin`] after a
    /// successful intercept. SEAM (098e0e03): the follow-up reads this.
    #[allow(dead_code)]
    pub async fn state(&self) -> OAuthState {
        self.state.read().await.clone()
    }

    /// The authorize URL produced by the most recent successful intercept.
    /// SEAM (098e0e03): the follow-up surfaces this to the client + retries.
    #[allow(dead_code)]
    pub async fn pending_authorize_url(&self) -> Option<String> {
        self.authorize_url.read().await.clone()
    }

    /// The retry-after-sign-in seam: load the currently stored bearer access
    /// token for `endpoint_name`, if a valid (unexpired) one has been persisted.
    ///
    /// After the human completes the loopback `/oauth/callback`, the callback
    /// handler exchanges the code for tokens and saves them to disk via the
    /// shared [`TokenManager`] keyed by the endpoint name. A subsequent retry of
    /// the same tool call then picks the token up here and injects it as a
    /// `Bearer` header (see `HttpAdapter::send_request`), so the call succeeds
    /// without re-triggering the JIT flow. Returns `None` when no token manager
    /// is attached, no token is stored, or the stored token has expired.
    pub async fn current_bearer(&self, endpoint_name: &str) -> Option<String> {
        let tm = self.token_manager.as_ref()?;
        let tokens = tm.load(endpoint_name).await.ok().flatten()?;
        if !tokens.is_valid() {
            return None;
        }
        Some(tokens.access_token)
    }

    /// Self-initiate the OAuth flow for a gated 401.
    ///
    /// Runs RFC 9728 → 8414 discovery against the resource (preferring the
    /// `resource_metadata` origin from the challenge), dynamically registers a
    /// client (RFC 7591) when needed, composes a PKCE authorize URL with the
    /// relay loopback `redirect_uri`, and registers the pending flow so the
    /// `/oauth/callback` handler can complete the exchange. On success the
    /// interceptor transitions to [`OAuthState::NeedsLogin`] and stores the
    /// authorize URL; the URL is also returned for convenience.
    pub async fn intercept(
        &self,
        resource_url: &str,
        www_authenticate: &str,
        endpoint_name: &str,
    ) -> Result<String, JitError> {
        let challenge =
            parse_bearer_challenge(www_authenticate).ok_or(JitError::NotABearerChallenge)?;

        // Prefer the RFC 9728 `resource_metadata` URL from the challenge and
        // honor its FULL path: per RFC 9728 the protected-resource metadata may
        // live at a path-based location (e.g.
        // `…/.well-known/oauth-protected-resource/<resource-path>`), so we fetch
        // the exact document the server pointed us at rather than re-deriving
        // the conventional well-known location from its origin. Fall back to the
        // resource (endpoint) URL when the challenge carries no parseable
        // `resource_metadata`.
        let metadata_url = challenge
            .resource_metadata
            .as_deref()
            .filter(|m| Url::parse(m).is_ok());

        let disc = match metadata_url {
            Some(m) => {
                discovery::discover_oauth_server_from_metadata(m, self.allow_insecure_oauth).await?
            }
            None => {
                discovery::discover_oauth_server(resource_url, self.allow_insecure_oauth).await?
            }
        };

        let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", self.relay_port);

        let (client_id, client_secret) = self
            .resolve_client(&disc, &redirect_uri, endpoint_name)
            .await?;

        let pkce = PkceChallenge::generate();
        let code_challenge = pkce.code_challenge.clone();
        let state_param = self
            .flow_manager
            .start_flow(
                endpoint_name,
                &disc.token_endpoint,
                &client_id,
                client_secret.as_deref(),
                pkce,
                &redirect_uri,
                Some(&disc.issuer),
            )
            .await;

        // Choose the query separator based on whether the discovered
        // authorization endpoint already carries a query string; appending a
        // bare `?` to an endpoint that already has one would yield a malformed
        // double-`?` URL. Subsequent params (including `&scope=`) always use `&`
        // because at least one query param is present after this point.
        let sep = if disc.authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let mut authorize_url = format!(
            "{}{}response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
            disc.authorization_endpoint,
            sep,
            urlencoding(&client_id),
            urlencoding(&redirect_uri),
            urlencoding(&state_param),
            urlencoding(&code_challenge),
        );
        // Scope accumulation for step-up authorization: when this endpoint
        // already has a persisted token with a granted scope, request the
        // UNION of previously-granted scopes and the scopes we'd request today
        // so the user never silently loses access they already granted.
        let prior_scope = if let Some(ref tm) = self.token_manager {
            tm.load(endpoint_name)
                .await
                .ok()
                .flatten()
                .and_then(|t| t.scope)
        } else {
            None
        };
        let requested_scope = disc.scopes_supported.join(" ");
        let merged_scope = merge_scopes(prior_scope.as_deref(), &requested_scope);
        if !merged_scope.is_empty() {
            authorize_url.push_str(&format!("&scope={}", urlencoding(&merged_scope)));
        }

        info!(
            endpoint = %endpoint_name,
            "JIT OAuth self-initiated on upstream 401; authorize URL composed"
        );
        *self.state.write().await = OAuthState::NeedsLogin;
        *self.authorize_url.write().await = Some(authorize_url.clone());
        Ok(authorize_url)
    }

    /// Resolve `(client_id, client_secret)` for the flow: reuse persisted DCR
    /// credentials when present, otherwise dynamically register. The RETURNED
    /// auth method/secret is honored (some servers issue a `client_secret`
    /// even when `none` was requested), and freshly registered credentials are
    /// persisted when a token manager is available.
    async fn resolve_client(
        &self,
        disc: &DiscoveryResult,
        redirect_uri: &str,
        endpoint_name: &str,
    ) -> Result<(String, Option<String>), JitError> {
        if let Some(ref tm) = self.token_manager {
            if let Ok(Some(creds)) = tm.load_dcr(endpoint_name).await {
                // Credential-to-issuer binding: only reuse a stored client_id
                // with the SAME authorization server that issued it. If the AS
                // issuer changed, discard and re-register (RFC 7591). Legacy
                // creds with no stored issuer are reused as-is.
                if dcr_issuer_allows_reuse(creds.issuer.as_deref(), Some(disc.issuer.as_str())) {
                    return Ok((creds.client_id, creds.client_secret));
                }
                info!(
                    endpoint = %endpoint_name,
                    stored_issuer = ?creds.issuer,
                    current_issuer = %disc.issuer,
                    "DCR credential issuer changed; discarding stored credentials and re-registering"
                );
            }
        }

        let Some(ref reg_endpoint) = disc.registration_endpoint else {
            return Err(JitError::NoClientCredentials);
        };

        let resp: ClientRegistrationResponse = dcr::register_client(
            reg_endpoint,
            redirect_uri,
            endpoint_name,
            self.allow_insecure_oauth,
        )
        .await?;

        if let Some(ref tm) = self.token_manager {
            let creds = DcrCredentials {
                client_id: resp.client_id.clone(),
                client_secret: resp.client_secret.clone(),
                client_secret_expires_at: resp.client_secret_expires_at,
                registered_at: now_secs(),
                issuer: Some(disc.issuer.clone()),
            };
            if let Err(e) = tm.save_dcr(endpoint_name, &creds).await {
                warn!(error = %e, "failed to persist JIT-registered DCR credentials");
            }
        }

        Ok((resp.client_id, resp.client_secret))
    }
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- parse_bearer_challenge ---

    #[test]
    fn parse_bearer_with_realm_and_resource_metadata() {
        let h = r#"Bearer realm="FlightPoints", resource_metadata="https://mcpv1.flightpoints.com/.well-known/oauth-protected-resource""#;
        let c = parse_bearer_challenge(h).unwrap();
        assert_eq!(c.realm.as_deref(), Some("FlightPoints"));
        assert_eq!(
            c.resource_metadata.as_deref(),
            Some("https://mcpv1.flightpoints.com/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn parse_bearer_scheme_only() {
        let c = parse_bearer_challenge("Bearer").unwrap();
        assert!(c.realm.is_none());
        assert!(c.resource_metadata.is_none());
    }

    #[test]
    fn parse_bearer_case_insensitive() {
        let c = parse_bearer_challenge(
            "bearer resource_metadata=https://x/.well-known/oauth-protected-resource",
        )
        .unwrap();
        assert_eq!(
            c.resource_metadata.as_deref(),
            Some("https://x/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn parse_rejects_non_bearer() {
        assert!(parse_bearer_challenge("Basic realm=\"x\"").is_none());
        // A token that merely starts with "Bearer" but isn't the scheme.
        assert!(parse_bearer_challenge("BearerToken foo=bar").is_none());
        assert!(parse_bearer_challenge("").is_none());
    }

    // --- should_intercept_outcome: the 401-only policy ---

    #[test]
    fn intercept_only_on_http_401() {
        assert!(should_intercept_outcome(&Err(AdapterError::HttpError {
            status: 401,
            body: String::new(),
        })));
        assert!(!should_intercept_outcome(&Err(AdapterError::HttpError {
            status: 403,
            body: String::new(),
        })));
    }

    /// A 200 JSON-RPC result carrying `isError: true` surfaces as `Ok(_)` and
    /// MUST be passed through unchanged (NOT intercepted) — the deliberate
    /// 401-only policy with no 200-`isError` body sniffing.
    #[test]
    fn two_hundred_is_error_is_not_intercepted() {
        let ok = Ok(json!({
            "isError": true,
            "content": [{ "type": "text", "text": "Sign in required… sign in via OAuth" }]
        }));
        assert!(!should_intercept_outcome(&ok));
    }

    // --- surface_oauth_unavailable: sanitized failure surface ---

    /// The sign-in-unavailable surface must be an actionable `isError: true`
    /// tool result that NEVER leaks the raw upstream `401` status or the
    /// `WWW-Authenticate` challenge text downstream.
    #[test]
    fn surface_oauth_unavailable_is_sanitized() {
        let v = surface_oauth_unavailable();
        assert_eq!(v["isError"], json!(true));
        let text = v["content"][0]["text"]
            .as_str()
            .expect("text content present");
        let lower = text.to_ascii_lowercase();
        assert!(!text.contains("401"), "must not leak status code: {}", text);
        assert!(
            !lower.contains("www-authenticate"),
            "must not leak challenge header: {}",
            text
        );
        assert!(
            lower.contains("sign-in") || lower.contains("sign in"),
            "should be actionable: {}",
            text
        );
    }

    // --- self-initiation engine integration ---

    /// Spawn an axum fixture on `127.0.0.1:0` advertising full standard OAuth:
    /// RFC 9728 protected-resource metadata, RFC 8414 AS metadata (S256), and
    /// an RFC 7591 registration endpoint that — like Flightpoints — returns a
    /// `client_secret` even though `none` was requested.
    async fn spawn_oauth_fixture() -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::State;
        use axum::routing::{get, post};
        use axum::{Json, Router};

        async fn protected_resource(State(base): State<String>) -> Json<Value> {
            Json(json!({
                "resource": base,
                "authorization_servers": [base],
                "bearer_methods_supported": ["header"],
            }))
        }
        async fn auth_server(State(base): State<String>) -> Json<Value> {
            Json(json!({
                "issuer": base,
                "authorization_endpoint": format!("{}/authorize", base),
                "token_endpoint": format!("{}/token", base),
                "registration_endpoint": format!("{}/register", base),
                "code_challenge_methods_supported": ["S256"],
                "scopes_supported": ["read", "write"],
            }))
        }
        async fn register() -> Json<Value> {
            Json(json!({
                "client_id": "jit-client-123",
                "client_secret": "jit-secret-456",
                "client_secret_expires_at": 0,
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let router = Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(protected_resource),
            )
            .route("/.well-known/oauth-authorization-server", get(auth_server))
            .route("/register", post(register))
            .with_state(base.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (base, handle)
    }

    #[tokio::test]
    async fn intercept_401_transitions_to_needs_login_and_produces_authorize_url() {
        let (base, server) = spawn_oauth_fixture().await;
        let flow_mgr = Arc::new(OAuthFlowManager::new());
        // allow_insecure_oauth=true so the loopback fixture passes the SSRF guard.
        let interceptor = JitInterceptor::new(9400, flow_mgr.clone(), None, true);

        // Precondition: not yet in NeedsLogin.
        assert_ne!(interceptor.state().await, OAuthState::NeedsLogin);

        let www_authenticate = format!(
            "Bearer realm=\"Test\", resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            base
        );
        let resource_url = format!("{}/mcp", base);
        let authorize_url = interceptor
            .intercept(&resource_url, &www_authenticate, "flightpoints")
            .await
            .expect("intercept should self-initiate the OAuth flow");

        // Transitioned to NeedsLogin and stored the URL (the follow-up seam).
        assert_eq!(interceptor.state().await, OAuthState::NeedsLogin);
        assert_eq!(
            interceptor.pending_authorize_url().await.as_deref(),
            Some(authorize_url.as_str())
        );

        // The authorize URL targets the discovered endpoint and carries PKCE.
        assert!(
            authorize_url.starts_with(&format!("{}/authorize?", base)),
            "got: {}",
            authorize_url
        );
        let url = Url::parse(&authorize_url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            q.get("client_id").map(String::as_str),
            Some("jit-client-123")
        );
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:9400/oauth/callback")
        );
        assert!(q.contains_key("code_challenge"));
        let state_param = q.get("state").expect("state param present").clone();

        // The pending flow carries the DCR-issued client_secret (honored even
        // though `none` was requested) and the discovered token endpoint.
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow registered");
        assert_eq!(flow.endpoint_name, "flightpoints");
        assert_eq!(flow.client_id, "jit-client-123");
        assert_eq!(flow.client_secret.as_deref(), Some("jit-secret-456"));
        assert_eq!(flow.token_endpoint, format!("{}/token", base));
        assert_eq!(flow.redirect_uri, "http://127.0.0.1:9400/oauth/callback");

        server.abort();
    }

    #[tokio::test]
    async fn intercept_rejects_non_bearer_challenge() {
        let flow_mgr = Arc::new(OAuthFlowManager::new());
        let interceptor = JitInterceptor::new(9400, flow_mgr, None, true);
        let err = interceptor
            .intercept("http://127.0.0.1:1/mcp", "Basic realm=\"x\"", "ep")
            .await
            .unwrap_err();
        assert!(matches!(err, JitError::NotABearerChallenge));
    }

    /// Spawn an OAuth fixture that additionally serves RFC 9728
    /// protected-resource metadata at a PATH-based well-known location
    /// (`/.well-known/oauth-protected-resource/{*tail}`) and lets the AS
    /// `authorization_endpoint` optionally carry a pre-existing query string.
    async fn spawn_oauth_fixture_ex(
        authorize_query: Option<&'static str>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::State;
        use axum::routing::{get, post};
        use axum::{Json, Router};

        #[derive(Clone)]
        struct Cfg {
            base: String,
            authorize_query: Option<&'static str>,
        }

        async fn protected_resource(State(cfg): State<Cfg>) -> Json<Value> {
            Json(json!({
                "resource": cfg.base,
                "authorization_servers": [cfg.base],
                "bearer_methods_supported": ["header"],
            }))
        }
        async fn auth_server(State(cfg): State<Cfg>) -> Json<Value> {
            let authorization_endpoint = match cfg.authorize_query {
                Some(q) => format!("{}/authorize?{}", cfg.base, q),
                None => format!("{}/authorize", cfg.base),
            };
            Json(json!({
                "issuer": cfg.base,
                "authorization_endpoint": authorization_endpoint,
                "token_endpoint": format!("{}/token", cfg.base),
                "registration_endpoint": format!("{}/register", cfg.base),
                "code_challenge_methods_supported": ["S256"],
                "scopes_supported": ["read", "write"],
            }))
        }
        async fn register() -> Json<Value> {
            Json(json!({
                "client_id": "jit-client-123",
                "client_secret": "jit-secret-456",
                "client_secret_expires_at": 0,
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let cfg = Cfg {
            base: base.clone(),
            authorize_query,
        };
        let router = Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(protected_resource),
            )
            .route(
                "/.well-known/oauth-protected-resource/{*tail}",
                get(protected_resource),
            )
            .route("/.well-known/oauth-authorization-server", get(auth_server))
            .route("/register", post(register))
            .with_state(cfg);
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (base, handle)
    }

    /// Finding 2 (RFC 9728 path): when the challenge supplies a PATH-based
    /// `resource_metadata` URL, discovery must fetch that EXACT document — not
    /// the origin-rooted well-known location (which the fixture does NOT serve
    /// at root for this path), so honoring the path is what makes it succeed.
    #[tokio::test]
    async fn intercept_honors_path_based_resource_metadata() {
        let (base, server) = spawn_oauth_fixture_ex(None).await;
        let flow_mgr = Arc::new(OAuthFlowManager::new());
        let interceptor = JitInterceptor::new(9400, flow_mgr.clone(), None, true);

        let metadata_url = format!("{}/.well-known/oauth-protected-resource/tenant-a", base);
        let www_authenticate = format!("Bearer resource_metadata=\"{}\"", metadata_url);
        let resource_url = format!("{}/mcp", base);

        let authorize_url = interceptor
            .intercept(&resource_url, &www_authenticate, "tenant-a")
            .await
            .expect("intercept should honor the path-based resource_metadata URL");

        assert!(
            authorize_url.starts_with(&format!("{}/authorize?", base)),
            "got: {}",
            authorize_url
        );
        assert_eq!(interceptor.state().await, OAuthState::NeedsLogin);

        server.abort();
    }

    /// Finding 3 (authorize-URL query separator): when the discovered
    /// `authorization_endpoint` already contains a `?query`, the composed
    /// authorize URL must remain valid — exactly one `?`, the pre-existing
    /// param preserved, and all PKCE params present.
    #[tokio::test]
    async fn intercept_authorize_endpoint_with_existing_query_stays_valid() {
        let (base, server) = spawn_oauth_fixture_ex(Some("audience=test-aud")).await;
        let flow_mgr = Arc::new(OAuthFlowManager::new());
        let interceptor = JitInterceptor::new(9400, flow_mgr.clone(), None, true);

        let www_authenticate = format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            base
        );
        let resource_url = format!("{}/mcp", base);

        let authorize_url = interceptor
            .intercept(&resource_url, &www_authenticate, "ep")
            .await
            .expect("intercept should self-initiate the OAuth flow");

        assert_eq!(
            authorize_url.matches('?').count(),
            1,
            "exactly one '?' expected, got: {}",
            authorize_url
        );
        let url = Url::parse(&authorize_url).expect("authorize URL must be parseable");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q.get("audience").map(String::as_str), Some("test-aud"));
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            q.get("client_id").map(String::as_str),
            Some("jit-client-123")
        );
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert!(q.contains_key("code_challenge"));
        assert!(q.contains_key("scope"));

        server.abort();
    }
}
