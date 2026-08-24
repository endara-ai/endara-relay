mod heartbeat;
pub mod jit;
pub mod metrics;
mod state;

// Re-export submodule public items so external callers are unaffected.
pub use state::{derive_health, do_transition, refresh_deadline, OAuthState, TransitionRecord};

use self::metrics::{generate_correlation_id, OAuthMetrics};
use super::http::{HttpAdapter, HttpConfig};
use super::server_type_resolution::effective_server_type;
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::oauth::client::ENDARA_CLIENT_METADATA_URL;
use crate::oauth::discovery::{discover_oauth_server, DiscoveryResult};
use crate::oauth::ema::{self, EmaError};
use crate::oauth::{OAuthError, OAuthFlowManager, PkceChallenge};
use crate::token_manager::{TokenError, TokenManager, TokenSet};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;
use tracing::{error, info, warn, Instrument};

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

/// `application/x-www-form-urlencoded` byte-serialize a single value for use in
/// an authorize-URL query parameter (mirrors the JIT path's encoder).
fn form_urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Wall-clock timeout applied to the OAuth refresh `reqwest::Client`.
///
/// Bounds recovery time when the configured token endpoint stops responding
/// (e.g. a half-open TCP connection); without it `do_token_refresh` could pin
/// the adapter in `Refreshing` indefinitely. Pinning the value as a `const`
/// gives `refresh_http_client_uses_30s_timeout` a stable hook to assert
/// against and forces a deliberate review of any future change.
const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Detect an RFC 6749 §5.2 `invalid_client` token-endpoint error body.
///
/// Returns `true` iff `body` parses as JSON with a top-level `"error"` string
/// exactly equal to `"invalid_client"`. Non-JSON bodies, missing/other error
/// codes, and non-string `error` values all return `false`, so this sniff is
/// safe to run on any token-endpoint response body without accidentally
/// triggering the DCR self-heal on unrelated errors (`invalid_grant`,
/// `invalid_request`, HTML error pages, etc.).
pub(crate) fn is_invalid_client_error(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
        .as_deref()
        == Some("invalid_client")
}

/// Resolved Enterprise-Managed Authorization (EMA, END-18) parameters for an
/// endpoint whose `[endpoints.auth] type = "ema"`. Present on
/// [`OAuthAdapterConfig::ema`] only for EMA endpoints; `None` leaves the adapter
/// on the standard OAuth `refresh_token` grant. The URLs are resolved via
/// discovery at construction time and are re-validated through `url_guard`
/// inside [`crate::oauth::ema`] on every leg.
#[derive(Debug, Clone)]
pub struct EmaConfig {
    /// Token-store key for the IdP credentials (a sanitized IdP issuer in v1).
    pub idp_key: String,
    /// IdP issuer URL (e.g. `https://acme.okta.com`).
    pub idp_issuer: String,
    /// IdP authorization endpoint (for the SSO kick-off authorize URL).
    pub idp_authorization_endpoint: String,
    /// IdP token endpoint (token-exchange Step 2 + IdP refresh grant).
    pub idp_token_endpoint: String,
    /// Resource AS issuer (RFC 8693 `audience`; ID-JAG `aud` claim).
    pub as_issuer: String,
    /// Resource AS token endpoint (RFC 7523 Step 3 redemption).
    pub as_token_endpoint: String,
    /// Target MCP server URL the access token is minted for.
    pub resource: String,
    /// Optional pre-registered org `client_id` (Okta/Entra). Presented verbatim
    /// in the IdP authorize URL and every EMA token leg. `None` keeps the legs
    /// on the hosted CIMD `client_id` ([`ENDARA_CLIENT_METADATA_URL`]).
    pub client_id: Option<String>,
    /// Optional pre-registered org `client_secret` (confidential IdP client).
    /// Loaded from the `{org}.dcr.json` credential store at adapter init and
    /// sent on the IdP-facing legs (RFC 8693 token-exchange Step 2 and the IdP
    /// `refresh_token` grant) so confidential clients authenticate with
    /// `client_secret_post`. `None` keeps the legs on the public/PKCE flow.
    /// **Never** presented at the resource AS (Step 3 stays a public client).
    pub client_secret: Option<String>,
    /// Optional **resource** `client_id` presented at the MAS in Step 3
    /// (RFC 7523 ID-JAG redemption). Distinct from `client_id`, which is the
    /// requesting client used for SSO / the Step 2 exchange. Per-resource, so
    /// R3 loads it from the *endpoint* DCR store (`{name}.dcr.json`) rather than
    /// the org record. `None` keeps Step 3 identifying as the requesting
    /// `client_id` (org id, else the hosted CIMD `client_id`).
    pub resource_client_id: Option<String>,
    /// Optional **resource** `client_secret` paired with `resource_client_id`,
    /// presented via `client_secret_post` at the MAS in Step 3. Never sent on
    /// the IdP-facing legs and never substituted by the requesting
    /// `client_secret` (R1): when `None`, Step 3 sends no secret at the MAS.
    pub resource_client_secret: Option<String>,
    /// Optional resource scopes (space-delimited) configured for this endpoint
    /// (R2). Threaded verbatim onto the Step 2 exchange and Step 3 redemption,
    /// and composed with `openid`/`offline_access` for the SSO authorize URL and
    /// the IdP `refresh_token` grant via [`crate::oauth::ema::compose_idp_scope`].
    /// `None` keeps the historical scopes (regression-safe).
    pub resource_scope: Option<String>,
}

/// SSO kick-off wiring for an EMA endpoint: the shared OAuth flow manager and
/// the relay loopback port used to compose the IdP authorize URL and register
/// the pending IdP SSO flow (via [`OAuthFlowManager::start_idp_flow`]). Kept off
/// [`OAuthAdapterConfig`] (which is `Debug`/`Clone`) because it carries shared
/// runtime handles.
#[derive(Clone)]
pub struct EmaSsoWiring {
    pub flow_manager: Arc<OAuthFlowManager>,
    pub relay_port: u16,
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
    /// Per-probe timeout in seconds (default: 10).
    pub probe_timeout_secs: u64,
    /// Number of consecutive probe failures required before flipping
    /// `inner_health` to `Unhealthy` (default: 3). Counts both transport-dead
    /// failures (connect failure/timeout, reported as "upstream unreachable")
    /// and alive-but-erroring upstream failures (HTTP status > 0 other than
    /// 401, JSON-RPC or protocol errors, reported with the actual error
    /// text). HTTP 401 is excluded: it bypasses the hysteresis and
    /// immediately transitions to `AuthRequired`. Hysteresis to avoid
    /// flapping on a single transient failure.
    pub probe_failure_threshold: u32,
    /// Optional override for the advertised `server_type` name. Forwarded to
    /// the inner [`HttpAdapter`] when it is constructed.
    pub server_type_override: Option<String>,
    /// Permit HTTP and loopback / link-local addresses when running OAuth
    /// discovery against the resource URL during the refresh-time fallback.
    /// Mirrors `config.relay.allow_insecure_oauth`; defaults to `false` for
    /// production callers and is set to `true` only by tests that mock the
    /// well-known endpoints on `127.0.0.1`.
    pub allow_insecure_oauth: bool,
    /// Enterprise-Managed Authorization (EMA, END-18) parameters. `Some(_)` for
    /// `[endpoints.auth] type = "ema"` endpoints, routing token acquisition /
    /// 401-expiry refresh through [`crate::oauth::ema::ensure_access_token`]
    /// instead of the standard `refresh_token` grant; post-token behaviour
    /// (`tools/list`, `tools/call`) is identical to OAuth (decision D1). `None`
    /// for ordinary OAuth endpoints, which are completely unaffected.
    pub ema: Option<EmaConfig>,
}

/// Outcome of a single POST to the OAuth token endpoint. Returned by
/// `OAuthAdapterInner::execute_token_post` so the caller can special-case
/// HTTP 404 (which triggers the discovery fallback) without prematurely
/// transitioning the adapter state machine.
#[derive(Debug)]
enum TokenPostOutcome {
    Success(TokenSet),
    NotFound {
        status: reqwest::StatusCode,
        body: String,
    },
    HttpError {
        status: reqwest::StatusCode,
        body: String,
    },
    Network(reqwest::Error),
    InvalidJson(reqwest::Error),
}

impl TokenPostOutcome {
    fn short_label(&self) -> &'static str {
        match self {
            TokenPostOutcome::Success(_) => "success",
            TokenPostOutcome::NotFound { .. } => "not_found",
            TokenPostOutcome::HttpError { .. } => "http_error",
            TokenPostOutcome::Network(_) => "network",
            TokenPostOutcome::InvalidJson(_) => "invalid_json",
        }
    }
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
    /// In-memory override of `config.token_endpoint_url`. Populated when a
    /// refresh-time RFC 9728 → RFC 8414 rediscovery turns up a different
    /// token endpoint than the one persisted in `config.toml`, so subsequent
    /// refreshes skip the discovery round-trip and post directly to the new
    /// URL. Cleared only on adapter reconstruction; not persisted to disk
    /// (that is handled separately by the startup-time migration path).
    token_endpoint_override: RwLock<Option<String>>,
    /// In-memory override of `config.client_id` / `config.client_secret`.
    /// Populated by the management `/oauth/callback` handler after a
    /// successful interactive re-authorization that produced a fresh RFC
    /// 7591 registration, so subsequent proactive refreshes POST the newly
    /// minted requesting client credentials instead of the stale ones baked
    /// into `config.toml` at startup. Cleared only on adapter reconstruction;
    /// not persisted to disk (config-file coherence is handled separately).
    client_credentials_override: RwLock<Option<(String, Option<String>)>>,
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
    /// Outer tools-changed broadcast — the registry subscribes here once and
    /// keeps that receiver across inner-adapter swaps. Forwarder tasks pump
    /// ticks from each inner adapter's `subscribe_tools_changed` receiver into
    /// this sender.
    outer_tools_changed_tx: broadcast::Sender<()>,
    /// Abort handle for the current inner→outer tools-changed forwarder task,
    /// if any. Re-bound on every inner-adapter swap.
    inner_forwarder_handle: Mutex<Option<AbortHandle>>,
    /// Per-endpoint tracing span. Every adapter method instruments its async
    /// body with this span so events emitted directly by `OAuthAdapter` /
    /// `OAuthAdapterInner` (state transitions, refresh, heartbeat) carry
    /// `endpoint`/`transport="oauth"` (and `server_type` once the inner MCP
    /// handshake completes).
    pub span: tracing::Span,
    /// Shared `OnceLock` cell for the desktop overlay's event bus. Owned at
    /// the OAuth layer so that every inner HTTP adapter rebuilt during a
    /// token swap shares the same slot — the outer `set_event_bus` call
    /// thus reaches both the current inner adapter and any future one.
    event_bus: Arc<OnceLock<crate::events::ToolCallEventBus>>,
    /// EMA (END-18) SSO kick-off wiring. `Some(_)` only for EMA endpoints built
    /// with a flow manager + relay port; consulted by [`compose_idp_authorize_url`]
    /// to register the IdP SSO pending flow and compose the authorize URL when
    /// the EMA chain reports re-authentication is required.
    ema_sso: Option<EmaSsoWiring>,
    /// The most recent EMA IdP authorize URL composed when the chain surfaced a
    /// re-SSO-required state. Surfaced to callers/desktop and asserted by tests.
    pending_authorize_url: RwLock<Option<String>>,
    /// Once-guard for the span's `server_type` field. `Span::record` appends
    /// each write to the span's field list, so recording on every
    /// [`Self::apply_tokens`] (e.g. across token refreshes) grows the
    /// `endpoint{…}` header without bound. This flag is flipped the first
    /// time a non-empty `server_type` is written so subsequent applies skip
    /// the record call.
    server_type_recorded: AtomicBool,
    /// Lifecycle generation counter, bumped on EVERY `OAuthState` write,
    /// inside the same `state` write-lock critical section as the write
    /// itself (see `transition_to`, the inline `Refreshing` entry in
    /// `apply_tokens_inner`, and the heartbeat's `AuthFailed` arm). The
    /// heartbeat snapshots state + generation under one `state` read lock
    /// when it dispatches a probe; if the generation still matches under
    /// the write lock when the result lands, no state transition — and
    /// therefore no apply/publish (every apply passes through
    /// `Refreshing`) — happened mid-probe, so the result belongs to the
    /// currently published inner adapter. This closes the ABA hole where
    /// an entire apply (Authenticated → Refreshing → Authenticated)
    /// completes mid-probe and a stale 401 would stomp the rebuilt
    /// adapter with `AuthRequired`.
    pub lifecycle_generation: AtomicU64,
    /// Fingerprint (hash of the sorted, serialized tool list) probed from the
    /// inner adapter after each successful [`Self::apply_tokens`] rebuild.
    /// Compared across Some→Some swaps to detect an actual tool-set change
    /// (e.g. an OAuth callback re-login under a different account/scope, see
    /// PR #140 review) without spamming clients with `list_changed` on
    /// routine token refreshes. `None` until a probe succeeds; cleared when
    /// the inner adapter is torn down AND whenever the forwarder relays an
    /// inner `tools_changed` tick (the upstream drifted from the probed
    /// baseline, so the next Some→Some swap must tick to be safe — see
    /// `swap_tools_forwarder`). `Arc` so the forwarder task can share it.
    last_tools_fingerprint: Arc<RwLock<Option<u64>>>,
    /// Serializes [`Self::apply_tokens`] end-to-end. The OAuth callback,
    /// proactive refresh, and reactive (401) refresh can all apply tokens
    /// concurrently; without serialization, interleaved applies could
    /// publish adapter B while a resumed apply A overwrites the fingerprint
    /// baseline with A's — a later catalog equal to A would then be treated
    /// as unchanged and the invalidation tick suppressed (PR #140 review).
    apply_lock: Mutex<()>,
}

impl OAuthAdapterInner {
    /// Build an inner HttpAdapter with a Bearer token in the default headers.
    /// `span` is the OAuth adapter's persistent `endpoint` span: the inner
    /// adapter instruments its async bodies with it so its tracing lines
    /// (tool-call completed/failed, handshake logging) carry the
    /// `endpoint`/`transport="oauth"` fields and reach the per-server Logs
    /// tab. Every rebuild (token swap) shares the same span.
    fn build_inner_adapter(
        url: &str,
        access_token: &str,
        server_type_override: Option<String>,
        endpoint_name: String,
        span: tracing::Span,
    ) -> HttpAdapter {
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
        let mut http_config = HttpConfig::new(url);
        http_config.server_type_override = server_type_override;
        http_config.endpoint_name = endpoint_name;
        HttpAdapter::new_with_client_inner(http_config, client, span)
    }

    /// Hash the adapter's tool list into an order-insensitive fingerprint.
    /// Returns `None` when `tools/list` fails — callers treat an unknown
    /// probe conservatively (see `apply_tokens_inner`).
    async fn probe_tools_fingerprint(adapter: &HttpAdapter) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let mut tools = adapter.list_tools().await.ok()?;
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let serialized = serde_json::to_string(&tools).ok()?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        serialized.hash(&mut hasher);
        Some(hasher.finish())
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
        // Bumped inside the write critical section so "generation
        // unchanged" observed under either state lock proves no
        // transition interleaved (see `lifecycle_generation`).
        self.lifecycle_generation.fetch_add(1, Ordering::Relaxed);
        self.metrics.inc_state_transition();
        info!(
            from = ?old,
            to = ?new_state,
            oauth_state = ?new_state,
            reason = %reason,
            "OAuth state transition"
        );
    }

    /// Record `server_type` on the per-endpoint span at most once. `Span::record`
    /// appends each write to the span's field list, so recording on every
    /// [`Self::apply_tokens`] call (e.g. across token refreshes) grows the
    /// `endpoint{…}` header without bound. The guard flips the first time a
    /// non-empty name is written so subsequent applies are a no-op.
    fn record_server_type_once(&self, name: &str) {
        if !self.server_type_recorded.swap(true, Ordering::Relaxed) {
            self.span
                .record("server_type", tracing::field::display(name));
        }
    }

    /// Test-only accessor for the `server_type` once-guard state.
    #[cfg(test)]
    pub(crate) fn server_type_recorded_flag(&self) -> bool {
        self.server_type_recorded.load(Ordering::Relaxed)
    }

    /// Resolve the URL that the next token-refresh POST should target.
    ///
    /// Returns the in-memory `token_endpoint_override` if a previous refresh
    /// rediscovered a new endpoint, otherwise the URL persisted in
    /// `config.token_endpoint_url`. Checked at the top of every refresh
    /// attempt so that the second and subsequent refreshes after a successful
    /// rediscovery go straight to the new URL without re-running discovery.
    pub async fn effective_token_endpoint(&self) -> String {
        if let Some(url) = self.token_endpoint_override.read().await.clone() {
            url
        } else {
            self.config.token_endpoint_url.clone()
        }
    }

    /// Install (or replace) the in-memory token endpoint override.
    ///
    /// Used by the management `/oauth/callback` handler to propagate the
    /// token endpoint freshly discovered via RFC 8414 during an
    /// authorization-code exchange, so that the next proactive refresh POSTs
    /// to the discovered URL instead of any stale `config.token_endpoint_url`
    /// baked in at startup. Mirrors the override-write path in
    /// `handle_refresh_404`.
    pub async fn set_token_endpoint_override(&self, url: String) {
        *self.token_endpoint_override.write().await = Some(url);
    }

    /// Resolve the `client_id` that the next token-refresh POST should
    /// present.
    ///
    /// Returns the in-memory `client_credentials_override` if a previous
    /// interactive re-authorization propagated a freshly re-registered
    /// client, otherwise the immutable `config.client_id` baked in at
    /// adapter construction. Checked at the top of every refresh attempt
    /// so subsequent refreshes after a successful re-registration go
    /// straight to the new `client_id` without needing to rebuild the
    /// adapter.
    pub async fn effective_client_id(&self) -> String {
        if let Some((id, _)) = self.client_credentials_override.read().await.as_ref() {
            id.clone()
        } else {
            self.config.client_id.clone()
        }
    }

    /// Resolve the `client_secret` that the next token-refresh POST should
    /// present, mirroring [`Self::effective_client_id`].
    pub async fn effective_client_secret(&self) -> Option<String> {
        if let Some((_, secret)) = self.client_credentials_override.read().await.as_ref() {
            secret.clone()
        } else {
            self.config.client_secret.clone()
        }
    }

    /// Snapshot the effective `(client_id, client_secret)` pair under a
    /// single `client_credentials_override` read guard. Callers must use
    /// this — not two `effective_client_*` reads — whenever they need both
    /// halves atomically, because `set_client_credentials` can rotate the
    /// pair between two separate acquisitions and let a refresh POST an
    /// old id with a new secret (or vice-versa).
    pub async fn effective_client_pair(&self) -> (String, Option<String>) {
        if let Some((id, secret)) = self.client_credentials_override.read().await.as_ref() {
            (id.clone(), secret.clone())
        } else {
            (
                self.config.client_id.clone(),
                self.config.client_secret.clone(),
            )
        }
    }

    /// Install (or replace) the in-memory client-credentials override.
    ///
    /// Used by the management `/oauth/callback` handler to propagate the
    /// requesting `client_id` / `client_secret` freshly minted by an RFC
    /// 7591 re-registration during an interactive re-authorization, so
    /// that the next proactive refresh POSTs the new credentials instead
    /// of the stale ones from startup config. Mirrors
    /// [`Self::set_token_endpoint_override`].
    pub async fn set_client_credentials(&self, client_id: String, client_secret: Option<String>) {
        *self.client_credentials_override.write().await = Some((client_id, client_secret));
    }

    /// Perform a token refresh using the refresh_token grant.
    ///
    /// POSTs to the token endpoint (using `effective_token_endpoint`) with
    /// grant_type=refresh_token. On success, returns the new `TokenSet`. On
    /// failure, transitions to `AuthRequired` or `ConnectionFailed` to match
    /// the failure category.
    ///
    /// When the POST returns HTTP 404, the adapter runs RFC 9728 → RFC 8414
    /// discovery against `self.config.url` and, if the rediscovered token
    /// endpoint differs from the one we just hit, updates the in-memory
    /// override and retries the POST exactly once. If discovery is not
    /// feasible (empty `config.url`), discovery fails, returns the same URL,
    /// or the retry also fails, the original 404 error is returned and the
    /// adapter transitions to `AuthRequired` (matching the pre-existing
    /// non-2xx behavior).
    pub async fn do_token_refresh(self: &Arc<Self>) -> Result<TokenSet, OAuthError> {
        // EMA (END-18) endpoints mint/refresh their access token through the
        // ID-JAG chain (Steps 2+3, with a stored ID Token) instead of the
        // `refresh_token` grant. `ensure_access_token` coalesces concurrent
        // refreshes on the same per-endpoint `refresh_mutex` (S2), so branch
        // before acquiring it here to avoid a double lock.
        if self.config.ema.is_some() {
            return self.do_ema_refresh().await;
        }

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
            correlation_id = %correlation_id,
            "Starting token refresh"
        );

        // Mark as refreshing
        self.transition_to(OAuthState::Refreshing, "starting token refresh")
            .await;

        // Snapshot the requesting (client_id, client_secret) under a single
        // `client_credentials_override` read guard so a concurrent
        // `set_client_credentials` cannot swap the pair between the two
        // reads and let this refresh POST an old id with a new secret.
        // The exact posted client_id is threaded into
        // `handle_invalid_client_if_present` so an eventual `invalid_client`
        // heal targets the id we actually presented — never one re-read
        // after the fact.
        let (posted_client_id, posted_client_secret) = self.effective_client_pair().await;
        let mut form_parts: Vec<(&str, String)> = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token),
            ("client_id", posted_client_id.clone()),
        ];
        if let Some(secret) = posted_client_secret {
            form_parts.push(("client_secret", secret));
        }

        let form_body: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form_parts.iter())
            .finish();

        let initial_url = self.effective_token_endpoint().await;
        match self
            .execute_token_post(&form_body, &initial_url, &correlation_id)
            .await
        {
            TokenPostOutcome::Success(token_set) => {
                self.metrics.inc_refresh_success();
                info!(
                    correlation_id = %correlation_id,
                    "Token refresh successful"
                );
                Ok(token_set)
            }
            TokenPostOutcome::NotFound { status, body } => {
                self.handle_refresh_404(
                    &form_body,
                    &initial_url,
                    &correlation_id,
                    &posted_client_id,
                    status,
                    body,
                )
                .await
            }
            TokenPostOutcome::HttpError { status, body } => {
                error!(
                    correlation_id = %correlation_id,
                    %status,
                    body = %body,
                    "Token refresh failed"
                );
                self.metrics.inc_refresh_failure();
                let reason = self
                    .handle_invalid_client_if_present(&body, &posted_client_id)
                    .await;
                self.transition_to(OAuthState::AuthRequired, reason).await;
                Err(OAuthError::RefreshFailed { status, body })
            }
            TokenPostOutcome::Network(e) => {
                error!(
                    correlation_id = %correlation_id,
                    error = %e,
                    "Token refresh network error"
                );
                self.metrics.inc_refresh_failure();
                self.transition_to(OAuthState::ConnectionFailed, "token refresh network error")
                    .await;
                Err(OAuthError::Http(e))
            }
            TokenPostOutcome::InvalidJson(e) => {
                error!(
                    correlation_id = %correlation_id,
                    error = %e,
                    "Token refresh JSON parse error"
                );
                self.metrics.inc_refresh_failure();
                self.transition_to(
                    OAuthState::ConnectionFailed,
                    "token refresh response not JSON",
                )
                .await;
                Err(OAuthError::Http(e))
            }
        }
    }

    /// Inspect an OAuth token-endpoint error body for `invalid_client` (RFC
    /// 6749 §5.2). If the current endpoint's DCR record was minted by the
    /// relay (`registered_via_dcr == true`) AND the stored `client_id` still
    /// matches `posted_client_id` (the id this refresh actually presented),
    /// atomically clear ONLY the requesting `client_id`/`client_secret`
    /// pair (via [`TokenManager::clear_dcr_requesting_client`]) so the next
    /// authorize triggers a fresh RFC 7591 registration and returns a
    /// distinct transition reason. Manually-supplied credentials
    /// (`registered_via_dcr == false`), a stored `client_id` that no longer
    /// matches (i.e. a concurrent re-registration replaced the record), and
    /// endpoints with no DCR record are never auto-discarded; the caller
    /// receives the generic `"token refresh failed"` reason. Operator-set
    /// `resource_client_id`/`resource_client_secret` are preserved.
    ///
    /// `posted_client_id` is snapshotted by the caller under the same
    /// `client_credentials_override` guard as the posted secret, so a
    /// concurrent `set_client_credentials` cannot cause the self-heal to
    /// target a different id than the one that actually failed.
    async fn handle_invalid_client_if_present(
        &self,
        body: &str,
        posted_client_id: &str,
    ) -> &'static str {
        if !is_invalid_client_error(body) {
            return "token refresh failed";
        }
        let endpoint = &self.config.endpoint_name;
        match self.token_manager.load_dcr(endpoint).await {
            Ok(Some(creds)) if creds.registered_via_dcr => {
                match self
                    .token_manager
                    .clear_dcr_requesting_client(endpoint, posted_client_id)
                    .await
                {
                    Ok(true) => {
                        info!(
                            endpoint = %endpoint,
                            client_id = %posted_client_id,
                            "Cleared stale DCR requesting client after invalid_client at token endpoint"
                        );
                        "client registration invalidated; re-authorize to re-register"
                    }
                    Ok(false) => {
                        info!(
                            endpoint = %endpoint,
                            failing_client_id = %posted_client_id,
                            stored_client_id = %creds.client_id,
                            "invalid_client for stale client_id; a newer registration is already persisted"
                        );
                        "token refresh failed"
                    }
                    Err(e) => {
                        error!(
                            endpoint = %endpoint,
                            error = %e,
                            "Failed to clear stale DCR requesting client after invalid_client"
                        );
                        "token refresh failed"
                    }
                }
            }
            _ => "token refresh failed",
        }
    }

    /// EMA (END-18) refresh path: mint or refresh the endpoint's access token
    /// through the full ID-JAG chain ([`ema::ensure_access_token`]) instead of
    /// the OAuth `refresh_token` grant. Drives the same state machine as
    /// `do_token_refresh` (Refreshing → Authenticated on success; AuthRequired /
    /// ConnectionFailed on failure) and reuses the adapter's per-endpoint
    /// `refresh_mutex` for coalescing (S2). On a re-SSO-required outcome it
    /// composes and stores an IdP authorize URL (M1/M9) so the user can sign in.
    async fn do_ema_refresh(self: &Arc<Self>) -> Result<TokenSet, OAuthError> {
        let ema = self
            .config
            .ema
            .as_ref()
            .expect("do_ema_refresh called without EMA config");

        self.transition_to(OAuthState::Refreshing, "starting EMA token exchange")
            .await;

        match ema::ensure_access_token(
            &self.token_manager,
            &self.refresh_mutex,
            &self.config.endpoint_name,
            &ema.idp_key,
            &ema.idp_token_endpoint,
            &ema.as_issuer,
            &ema.as_token_endpoint,
            &ema.resource,
            ema.resource_scope.as_deref(),
            self.config.allow_insecure_oauth,
            ema.client_id.as_deref(),
            ema.client_secret.as_deref(),
            ema.resource_client_id.as_deref(),
            ema.resource_client_secret.as_deref(),
        )
        .await
        {
            Ok(token_set) => {
                self.metrics.inc_refresh_success();
                // The endpoint is authenticated again; drop any stale IdP
                // sign-in URL composed by a prior re-SSO-required outcome so
                // callers don't keep surfacing it (M9).
                *self.pending_authorize_url.write().await = None;
                info!("EMA token exchange successful");
                Ok(token_set)
            }
            Err(e) => {
                self.metrics.inc_refresh_failure();
                // A re-SSO-required outcome surfaces an IdP authorize URL so the
                // user can sign in again (no silent loop, M9).
                if matches!(e, EmaError::ReauthRequired { .. }) {
                    if let Some(url) = self.compose_idp_authorize_url().await {
                        *self.pending_authorize_url.write().await = Some(url);
                    }
                }
                // Re-auth and policy denials are terminal (AuthRequired);
                // transport/expiry-class errors are retryable (ConnectionFailed).
                let (target, reason): (OAuthState, &str) = match &e {
                    EmaError::ReauthRequired { .. } | EmaError::AuthorizationDenied { .. } => {
                        (OAuthState::AuthRequired, "EMA re-authentication required")
                    }
                    _ => (OAuthState::ConnectionFailed, "EMA token exchange failed"),
                };
                error!(error = %e, "EMA token exchange failed");
                self.transition_to(target, reason).await;
                Err(OAuthError::Ema(e.to_string()))
            }
        }
    }

    /// Compose the IdP authorize URL for this EMA endpoint's Step-1 SSO and
    /// register the pending IdP flow via [`OAuthFlowManager::start_idp_flow`]
    /// (which tags the flow with `idp_issuer` so the `/oauth/callback` handler
    /// persists the returned ID Token as `IdpCredentials`). The requested scope
    /// is composed via [`ema::compose_idp_scope`] (R2): always `openid` and
    /// `offline_access` (M1) so the IdP returns a refresh token the EMA chain can
    /// later use to re-mint ID Tokens silently, plus any configured resource
    /// scopes. Returns `None` when the adapter was built without EMA SSO wiring
    /// (e.g. unit tests).
    async fn compose_idp_authorize_url(&self) -> Option<String> {
        let ema = self.config.ema.as_ref()?;
        let sso = self.ema_sso.as_ref()?;

        let pkce = PkceChallenge::generate();
        let code_challenge = pkce.code_challenge.clone();
        let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", sso.relay_port);

        // Use the org's pre-registered client_id when set; otherwise fall back to
        // the hosted CIMD client_id (byte-for-byte unchanged for bare END-18).
        let client_id = ema
            .client_id
            .as_deref()
            .unwrap_or(ENDARA_CLIENT_METADATA_URL);

        let state_param = sso
            .flow_manager
            .start_idp_flow(
                &self.config.endpoint_name,
                &ema.idp_token_endpoint,
                client_id,
                None,
                pkce,
                &redirect_uri,
                Some(&ema.idp_issuer),
                false,
                &ema.idp_issuer,
                // Wave 2: persist the captured ID token under the pooled key
                // (org name, or issuer for bare END-18 endpoints) so all of an
                // org's EMA endpoints share one credential.
                &ema.idp_key,
            )
            .await;

        // Append a `?` or `&` depending on whether the discovered IdP authorize
        // endpoint already carries a query string (mirrors the JIT path).
        let sep = if ema.idp_authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let scope = ema::compose_idp_scope(ema.resource_scope.as_deref(), true);
        let mut authorize_url = format!(
            "{}{}response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}",
            ema.idp_authorization_endpoint,
            sep,
            form_urlencode(client_id),
            form_urlencode(&redirect_uri),
            form_urlencode(&state_param),
            form_urlencode(&code_challenge),
            form_urlencode(&scope),
        );
        // Google needs `access_type=offline` for a refresh token (shared
        // helper); a Google-fronted IdP grant is access-token-only without it.
        crate::oauth::append_google_authorize_params(
            &mut authorize_url,
            &ema.idp_authorization_endpoint,
        );
        info!(
            endpoint = %self.config.endpoint_name,
            idp_issuer = %ema.idp_issuer,
            scope = %scope,
            "EMA IdP SSO authorize URL composed"
        );
        Some(authorize_url)
    }

    /// The most recent EMA IdP authorize URL composed when the chain surfaced a
    /// re-SSO-required state, if any.
    pub async fn pending_authorize_url(&self) -> Option<String> {
        self.pending_authorize_url.read().await.clone()
    }

    /// Drive the discovery-and-retry fallback for an HTTP 404 from the token
    /// POST. Returns the refreshed `TokenSet` if rediscovery yields a new
    /// endpoint and the retry succeeds; otherwise returns the original
    /// `RefreshFailed` error (the retry-failure path also transitions to
    /// `AuthRequired`, matching the pre-existing non-2xx behavior).
    ///
    /// `posted_client_id` is threaded through so that a retry against the
    /// rediscovered token endpoint which returns an OAuth error body
    /// containing `invalid_client` triggers the same self-heal as the
    /// primary refresh path — otherwise a rediscovered endpoint proving the
    /// requesting client dead would leave the stale DCR record intact and
    /// loop.
    async fn handle_refresh_404(
        self: &Arc<Self>,
        form_body: &str,
        initial_url: &str,
        correlation_id: &str,
        posted_client_id: &str,
        status: reqwest::StatusCode,
        body: String,
    ) -> Result<TokenSet, OAuthError> {
        let original_err = OAuthError::RefreshFailed {
            status,
            body: body.clone(),
        };

        // Discovery is only feasible if we know the resource URL.
        if self.config.url.is_empty() {
            warn!(
                correlation_id = %correlation_id,
                %status,
                "Token refresh got 404 but resource URL is empty; skipping discovery fallback"
            );
            self.metrics.inc_refresh_failure();
            self.transition_to(OAuthState::AuthRequired, "token refresh failed")
                .await;
            return Err(original_err);
        }

        info!(
            correlation_id = %correlation_id,
            %status,
            attempted_url = %initial_url,
            "Token endpoint returned 404; attempting OAuth discovery fallback"
        );

        let disc: DiscoveryResult =
            match discover_oauth_server(&self.config.url, self.config.allow_insecure_oauth).await {
                Ok(d) => d,
                Err(e) => {
                    warn!(
                        correlation_id = %correlation_id,
                        error = %e,
                        "OAuth discovery fallback failed; returning original 404"
                    );
                    self.metrics.inc_refresh_failure();
                    self.transition_to(OAuthState::AuthRequired, "token refresh failed")
                        .await;
                    return Err(original_err);
                }
            };

        if disc.token_endpoint == initial_url {
            warn!(
                correlation_id = %correlation_id,
                token_endpoint = %disc.token_endpoint,
                "OAuth discovery returned the same token endpoint that just 404'd; not retrying"
            );
            self.metrics.inc_refresh_failure();
            self.transition_to(OAuthState::AuthRequired, "token refresh failed")
                .await;
            return Err(original_err);
        }

        info!(
            correlation_id = %correlation_id,
            old_token_endpoint = %initial_url,
            new_token_endpoint = %disc.token_endpoint,
            "OAuth discovery rediscovered a new token endpoint; retrying refresh once"
        );
        *self.token_endpoint_override.write().await = Some(disc.token_endpoint.clone());

        match self
            .execute_token_post(form_body, &disc.token_endpoint, correlation_id)
            .await
        {
            TokenPostOutcome::Success(token_set) => {
                self.metrics.inc_refresh_success();
                info!(
                    correlation_id = %correlation_id,
                    "Token refresh successful after rediscovery"
                );
                Ok(token_set)
            }
            // A rediscovered token endpoint may itself return an OAuth
            // error body (e.g. `invalid_client` when the AS purged our
            // registration); run the same self-heal as the primary refresh
            // path so the rediscovery-and-retry route can also invalidate
            // stale DCR credentials.
            TokenPostOutcome::HttpError {
                status: retry_status,
                body: retry_body,
            } => {
                warn!(
                    correlation_id = %correlation_id,
                    %retry_status,
                    "Token refresh retry after rediscovery failed with HTTP error; returning original 404"
                );
                self.metrics.inc_refresh_failure();
                let reason = self
                    .handle_invalid_client_if_present(&retry_body, posted_client_id)
                    .await;
                self.transition_to(OAuthState::AuthRequired, reason).await;
                Err(original_err)
            }
            other => {
                warn!(
                    correlation_id = %correlation_id,
                    retry_outcome = ?other.short_label(),
                    "Token refresh retry after rediscovery failed; returning original 404"
                );
                self.metrics.inc_refresh_failure();
                self.transition_to(OAuthState::AuthRequired, "token refresh failed")
                    .await;
                Err(original_err)
            }
        }
    }

    /// Execute a single POST to the token endpoint, returning a structured
    /// outcome so callers can special-case HTTP 404. State transitions and
    /// metric updates are intentionally NOT done here — the caller is
    /// responsible for that so the rediscovery fallback can swallow a 404
    /// without leaking a `RefreshFailed` transition.
    async fn execute_token_post(
        &self,
        form_body: &str,
        target_url: &str,
        _correlation_id: &str,
    ) -> TokenPostOutcome {
        let resp = match self
            .http_client
            .post(target_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body.to_string())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return TokenPostOutcome::Network(e),
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::NOT_FOUND {
                return TokenPostOutcome::NotFound { status, body };
            }
            return TokenPostOutcome::HttpError { status, body };
        }

        let token_json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return TokenPostOutcome::InvalidJson(e),
        };
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

        TokenPostOutcome::Success(new_token_set)
    }

    /// Apply a new token set: update in-memory state, persist to disk,
    /// abort any existing refresh task, spawn a new proactive refresh task,
    /// and rebuild the inner adapter.
    pub fn apply_tokens(
        self: &Arc<Self>,
        token_set: TokenSet,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.apply_tokens_inner(token_set, false))
    }

    /// Apply a token set produced by REFRESHING the current grant (proactive
    /// timer, reactive 401, heartbeat recovery, manual `/oauth/refresh`).
    /// Unlike [`Self::apply_tokens`], this refuses to commit when the grant
    /// was discarded (disconnect / reset) after the refresh's network
    /// exchange started: `disconnect()` clears the in-memory tokens under
    /// the same `apply_lock`, so a refresh commit that acquires the lock
    /// after a disconnect observes `tokens == None` and drops its result
    /// instead of resurrecting the discarded grant on disk. Callback logins
    /// (a NEW grant) must keep using `apply_tokens` — they legitimately
    /// apply while no tokens exist.
    pub fn apply_refreshed_tokens(
        self: &Arc<Self>,
        token_set: TokenSet,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.apply_tokens_inner(token_set, true))
    }

    async fn apply_tokens_inner(self: &Arc<Self>, token_set: TokenSet, refresh_of_current: bool) {
        // Serialize the whole apply: callback, proactive refresh, and
        // reactive refresh may overlap, and interleaved applies could pair
        // the published adapter with another apply's fingerprint baseline
        // (see `apply_lock`). Applies are rare (login/refresh), so a full
        // mutex is the simple correct choice over rebuild versioning.
        let _apply_guard = self.apply_lock.lock().await;
        if refresh_of_current && self.tokens.read().await.is_none() {
            warn!(
                "Dropping refreshed tokens: grant was discarded (disconnect/reset) while the refresh was in flight"
            );
            return;
        }
        let endpoint = &self.config.endpoint_name;

        // Surface the apply as a transitional state right away: the inner
        // adapter rebuild below (init handshake + tools fingerprint probe)
        // takes seconds of network time, and leaving a prior error state
        // (AuthRequired/ConnectionFailed) in place keeps `derive_health`
        // reporting the stale error as if it were a fresh failure. Refresh
        // paths already enter the apply in `Refreshing` — skip the no-op
        // self-transition to keep the ring buffer free of noise. The check
        // and transition happen under one state write lock so a concurrent
        // `do_token_refresh` (guarded by `refresh_mutex`, not `apply_lock`)
        // cannot interleave between them and produce a noisy
        // Refreshing→Refreshing self-record.
        {
            let mut state = self.state.write().await;
            if *state != OAuthState::Refreshing {
                let mut history = self.transition_history.write().await;
                let old = do_transition(
                    &mut state,
                    OAuthState::Refreshing,
                    "applying new tokens",
                    &mut history,
                );
                self.lifecycle_generation.fetch_add(1, Ordering::Relaxed);
                self.metrics.inc_state_transition();
                info!(
                    from = ?old,
                    to = ?OAuthState::Refreshing,
                    oauth_state = ?OAuthState::Refreshing,
                    reason = "applying new tokens",
                    "OAuth state transition"
                );
            }
        }

        // 1. Persist to disk
        if let Err(e) = self.token_manager.save(endpoint, &token_set).await {
            error!(error = %e, "Failed to persist tokens");
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
        let mut adapter = Self::build_inner_adapter(
            &self.config.url,
            &access_token,
            self.config.server_type_override.clone(),
            self.config.endpoint_name.clone(),
            self.span.clone(),
        );
        // Share the OAuth adapter's event-bus OnceLock with the new inner so
        // overlay events fire from the inner's `call_tool` once the bus is
        // wired, even across token swaps. The outer `set_event_bus` writes
        // through this same cell.
        adapter.set_event_bus_handle(self.event_bus.clone());
        match adapter.initialize().await {
            Ok(()) => {
                // Mirror the stdio/sse/http adapters: once the inner MCP
                // handshake reports a `server_type`, record it on the outer
                // OAuth span so subsequent events render with the resolved
                // name in the `endpoint:` header.
                if let Some(name) = adapter.server_type() {
                    self.record_server_type_once(&name);
                }
                // Reflect the freshly initialized inner adapter's health
                // immediately so health() reports Healthy without waiting for
                // the next heartbeat tick.
                // Order matters: set the inner adapter BEFORE flipping
                // inner_health to Healthy so that any reader that observes
                // `health() == Healthy` is guaranteed to also see
                // `inner_adapter == Some(_)`.
                // Bind a forwarder against the new inner's tools-changed
                // receiver (if any) before publishing it. A synthetic tick is
                // emitted on the outer broadcast when the inner's readiness
                // changed (None→Some here; Some→None on the failure branch
                // below) and when a Some→Some swap actually changed the tool
                // set — an OAuth callback re-login under a different
                // account/scope rebuilds the inner adapter too, and its
                // different catalog must reach the registry's caches.
                // Change detection probes the new inner's tool list once and
                // compares its fingerprint against the previous successful
                // probe, so routine Some→Some token refreshes with an
                // unchanged tool set stay silent and clients aren't spammed
                // with `list_changed` on every refresh.
                let rx = adapter.subscribe_tools_changed();
                let new_fingerprint = Self::probe_tools_fingerprint(&adapter).await;
                let was_listable = {
                    let mut guard = self.inner_adapter.write().await;
                    let was = guard.is_some();
                    *guard = Some(adapter);
                    was
                };
                *self.inner_health.write().await = HealthStatus::Healthy;
                self.swap_tools_forwarder(rx).await;
                let should_tick = if !was_listable {
                    true
                } else {
                    let old_fingerprint = *self.last_tools_fingerprint.read().await;
                    match (old_fingerprint, new_fingerprint) {
                        // Both probes succeeded: tick only on an actual change.
                        (Some(old), Some(new)) => old != new,
                        // Baseline unknown (previous probe failed): cached
                        // tools may be stale — tick to be safe.
                        (None, Some(_)) => true,
                        // New probe failed: a change is undetectable, and the
                        // registry's own refetch would fail too — stay silent
                        // and keep the previous baseline.
                        (_, None) => false,
                    }
                };
                if new_fingerprint.is_some() {
                    *self.last_tools_fingerprint.write().await = new_fingerprint;
                }
                self.transition_to(
                    OAuthState::Authenticated,
                    "tokens applied, inner adapter ready",
                )
                .await;
                // Tick only after the Authenticated transition so a racing
                // registry rebuild can't read pre-transition health (e.g.
                // Refreshing→Starting) and cache the new tools with a stale
                // UNAVAILABLE label; inner+health are already published.
                if should_tick {
                    let _ = self.outer_tools_changed_tx.send(());
                }
            }
            Err(e) => {
                // Capture inner adapter's health before clearing it
                *self.inner_health.write().await = adapter.health();
                let was_listable = self.inner_adapter.write().await.take().is_some();
                *self.last_tools_fingerprint.write().await = None;
                self.swap_tools_forwarder(None).await;
                // Some→None: the endpoint just lost its tools — tick so the
                // registry drops them from the merged catalog.
                if was_listable {
                    let _ = self.outer_tools_changed_tx.send(());
                }
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
        // EMA endpoints refresh through the ID-JAG chain (using stored IdP
        // credentials), so they schedule a proactive refresh whenever an
        // `expires_at` is known even if the persisted `TokenSet` carries no
        // `refresh_token`. Non-EMA endpoints still require a refresh token.
        let deadline = if !has_refresh_token && self.config.ema.is_none() {
            info!("No refresh token, skipping proactive refresh");
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
            info!("No expires_at on token, skipping proactive refresh");
            None
        };

        if let Some(deadline) = deadline {
            let inner = self.clone();
            let refresh_span = self.span.clone();
            let fut = async move {
                tokio::time::sleep_until(deadline).await;
                info!("Proactive refresh timer fired");
                // Detach our own handle from the slot before re-entering
                // `apply_tokens`. Step 2 of `apply_tokens_inner`
                // (`refresh_task_handle.take()` followed by `h.abort()`)
                // would otherwise abort the future we are currently
                // executing — the rc.4 self-cancellation bug that pinned
                // OAuth endpoints in `Refreshing` indefinitely.
                {
                    let _ = inner.refresh_task_handle.lock().await.take();
                }
                match inner.do_token_refresh().await {
                    Ok(new_tokens) => {
                        // Recursively apply — this will schedule the next refresh
                        inner.apply_refreshed_tokens(new_tokens).await;
                        // Defensive: if the recursive apply_tokens ever
                        // returns while still in `Refreshing`, surface a
                        // loud warn-log so a future regression is not
                        // silent the way the rc.4 self-abort was.
                        let state = inner.state.read().await.clone();
                        if matches!(state, OAuthState::Refreshing) {
                            warn!(
                                "Proactive refresh recursive apply_tokens returned with state still Refreshing — possible regression"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "Proactive refresh failed, retrying in 60s"
                        );
                        // Retry once after 60 seconds
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        // Detach again before the retry: an external
                        // caller may have stored a fresh handle in the
                        // slot during the 60 s sleep, and the recursive
                        // `apply_tokens` in the retry-success arm would
                        // hit the same self-abort foot-gun.
                        {
                            let _ = inner.refresh_task_handle.lock().await.take();
                        }
                        match inner.do_token_refresh().await {
                            Ok(new_tokens) => {
                                inner.apply_refreshed_tokens(new_tokens).await;
                                let state = inner.state.read().await.clone();
                                if matches!(state, OAuthState::Refreshing) {
                                    warn!(
                                        "Proactive refresh retry recursive apply_tokens returned with state still Refreshing — possible regression"
                                    );
                                }
                            }
                            Err(retry_err) => {
                                // `do_token_refresh` already transitioned to
                                // a terminal state on its own error path; the
                                // explicit transition here is defensive so a
                                // future error path that forgets to do so
                                // still leaves Refreshing for AuthRequired
                                // (auth-style failure) or ConnectionFailed
                                // (network/JSON failure).
                                warn!(
                                    error = %retry_err,
                                    "Proactive refresh retry also failed"
                                );
                                let target = match &retry_err {
                                    OAuthError::RefreshFailed { .. }
                                    | OAuthError::NoRefreshToken { .. } => OAuthState::AuthRequired,
                                    _ => OAuthState::ConnectionFailed,
                                };
                                inner
                                    .transition_to(target, "proactive refresh retry failed")
                                    .await;
                            }
                        }
                    }
                }
            };
            let handle = tokio::spawn(fut.instrument(refresh_span));
            self.refresh_task_handle.lock().await.replace(handle);
        }
    }

    /// Abort any existing inner→outer tools-changed forwarder and, if `rx` is
    /// `Some`, spawn a fresh forwarder that pumps each inner tick into
    /// `outer_tools_changed_tx`. `Lagged` is forwarded as a tick (matching the
    /// registry listener); `Closed` ends the task.
    ///
    /// Each relayed tick also clears `last_tools_fingerprint`: an inner
    /// `tools_changed` notification means the upstream tool set drifted from
    /// the baseline probed at `apply_tokens` time, so a later Some→Some swap
    /// that happens to reproduce the original set must still tick (the
    /// registry's caches followed the drift). An unknown baseline makes the
    /// next swap tick unconditionally — safe, at worst one extra tick.
    async fn swap_tools_forwarder(&self, rx: Option<broadcast::Receiver<()>>) {
        let mut handle_guard = self.inner_forwarder_handle.lock().await;
        if let Some(h) = handle_guard.take() {
            h.abort();
        }
        let Some(mut rx) = rx else { return };
        let outer_tx = self.outer_tools_changed_tx.clone();
        let fingerprint = self.last_tools_fingerprint.clone();
        let join = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(()) => {
                        *fingerprint.write().await = None;
                        let _ = outer_tx.send(());
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        *fingerprint.write().await = None;
                        let _ = outer_tx.send(());
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *handle_guard = Some(join.abort_handle());
    }

    /// Disconnect: abort refresh task, clear tokens, delete from disk, set
    /// Disconnected. Returns `Err` when the persisted tokens could not be
    /// deleted from disk — the grant then survives a restart, so callers
    /// (revoke / reset) must surface the failure instead of reporting a
    /// clean disconnect. DCR-record deletion failures are logged but do not
    /// fail the disconnect: the record holds client credentials, not the
    /// grant.
    pub async fn disconnect(self: &Arc<Self>) -> Result<(), TokenError> {
        // Serialize against a token apply: a refresh whose network exchange
        // already succeeded commits (persist + rebuild) under this same
        // lock, so acquiring it here means no refresh is mid-commit while
        // we tear down — and a refresh commit that acquires it after us
        // observes the cleared in-memory tokens and drops its result (see
        // `apply_tokens_inner`) instead of resurrecting the grant.
        let _apply_guard = self.apply_lock.lock().await;
        let endpoint = &self.config.endpoint_name;

        // Abort refresh task
        {
            let mut handle = self.refresh_task_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }

        // Abort inner→outer tools-changed forwarder
        self.swap_tools_forwarder(None).await;

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

        // Delete tokens from disk (propagated to the caller on failure)
        let delete_result = self.token_manager.delete(endpoint).await;
        if let Err(ref e) = delete_result {
            error!(error = %e, "Failed to delete tokens from disk");
        }

        // Delete DCR credentials from disk
        if let Err(e) = self.token_manager.delete_dcr(endpoint).await {
            error!(error = %e, "Failed to delete DCR credentials from disk");
        }

        // Set state
        self.transition_to(OAuthState::Disconnected, "user disconnected")
            .await;

        delete_result
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
        Self::new_inner(config, token_manager, None)
    }

    /// Create a new OAuthAdapter for an EMA endpoint, attaching the SSO kick-off
    /// wiring (flow manager + relay port) used to compose the IdP authorize URL
    /// when the chain reports re-authentication is required.
    pub fn new_ema(
        config: OAuthAdapterConfig,
        token_manager: Arc<TokenManager>,
        ema_sso: EmaSsoWiring,
    ) -> Self {
        Self::new_inner(config, token_manager, Some(ema_sso))
    }

    fn new_inner(
        config: OAuthAdapterConfig,
        token_manager: Arc<TokenManager>,
        ema_sso: Option<EmaSsoWiring>,
    ) -> Self {
        let (outer_tools_changed_tx, _) = broadcast::channel(16);
        let span = tracing::info_span!(
            "endpoint",
            endpoint = %config.endpoint_name,
            transport = "oauth",
            server_type = tracing::field::Empty,
        );
        Self {
            inner: Arc::new(OAuthAdapterInner {
                state: RwLock::new(OAuthState::NeedsLogin),
                tokens: RwLock::new(None),
                config,
                token_endpoint_override: RwLock::new(None),
                client_credentials_override: RwLock::new(None),
                inner_adapter: RwLock::new(None),
                token_manager,
                http_client: Client::builder()
                    .timeout(REFRESH_HTTP_TIMEOUT)
                    .build()
                    .expect("static OAuth refresh HTTP client config"),
                refresh_task_handle: Mutex::new(None),
                inner_health: RwLock::new(HealthStatus::Starting),
                heartbeat_task_handle: Mutex::new(None),
                transition_history: RwLock::new(VecDeque::new()),
                metrics: OAuthMetrics::new(),
                refresh_mutex: Mutex::new(()),
                outer_tools_changed_tx,
                inner_forwarder_handle: Mutex::new(None),
                span,
                event_bus: Arc::new(OnceLock::new()),
                ema_sso,
                pending_authorize_url: RwLock::new(None),
                server_type_recorded: AtomicBool::new(false),
                lifecycle_generation: AtomicU64::new(0),
                last_tools_fingerprint: Arc::new(RwLock::new(None)),
                apply_lock: Mutex::new(()),
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
        let span = self.inner.span.clone();
        async {
            // EMA endpoints acquire/refresh their access token through the
            // ID-JAG chain rather than loading a `refresh_token` from disk.
            // `do_token_refresh` dispatches to the EMA path, which returns a
            // valid cached token without network when one is persisted, mints a
            // fresh one from stored IdP credentials otherwise, or surfaces a
            // re-SSO-required state (composing an IdP authorize URL) when there
            // are none.
            if self.inner.config.ema.is_some() {
                match self.inner.do_token_refresh().await {
                    Ok(new_tokens) => {
                        self.inner.apply_tokens(new_tokens).await;
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "EMA token acquisition at startup failed (sign-in may be required)"
                        );
                        // `do_ema_refresh` already transitioned to a terminal
                        // state (AuthRequired / ConnectionFailed) on its error
                        // path.
                    }
                }

                // Spawn the heartbeat probe loop (shared with the OAuth path).
                let weak = Arc::downgrade(&self.inner);
                let hb_span = self.inner.span.clone();
                let handle = tokio::spawn(heartbeat::heartbeat_loop(weak).instrument(hb_span));
                self.inner
                    .heartbeat_task_handle
                    .lock()
                    .await
                    .replace(handle);

                return Ok(());
            }

            // Try to load existing tokens from disk
            let loaded = self
                .inner
                .token_manager
                .load(&self.inner.config.endpoint_name)
                .await;

            if let Ok(Some(token_set)) = loaded {
                if token_set.is_valid() {
                    info!("Loaded valid OAuth tokens from disk");
                    self.inner.apply_tokens(token_set).await;
                } else if token_set.refresh_token.is_some() {
                    info!("Loaded expired tokens with refresh token, attempting refresh");
                    // Store expired tokens so refresh can use the refresh_token
                    *self.inner.tokens.write().await = Some(token_set);
                    match self.inner.do_token_refresh().await {
                        Ok(new_tokens) => {
                            self.inner.apply_refreshed_tokens(new_tokens).await;
                        }
                        Err(e) => {
                            warn!(
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

            // Spawn the heartbeat probe loop, instrumented with the per-endpoint
            // span so events emitted from `heartbeat::apply_probe_action` render
            // under the `endpoint:` span header.
            let weak = Arc::downgrade(&self.inner);
            let hb_span = self.inner.span.clone();
            let handle = tokio::spawn(heartbeat::heartbeat_loop(weak).instrument(hb_span));
            self.inner
                .heartbeat_task_handle
                .lock()
                .await
                .replace(handle);

            Ok(()) // initialize always succeeds for OAuth
        }
        .instrument(span)
        .await
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        async {
            let guard = self.inner.inner_adapter.read().await;
            match guard.as_ref() {
                Some(adapter) => {
                    match adapter.list_tools().await {
                        Ok(tools) => Ok(tools),
                        Err(AdapterError::HttpError { status: 401, .. }) => {
                            // Drop the read lock before refreshing
                            drop(guard);

                            info!("Got 401 on list_tools, attempting token refresh");

                            match self.inner.do_token_refresh().await {
                                Ok(new_tokens) => {
                                    self.inner.apply_refreshed_tokens(new_tokens).await;
                                    // Retry with new token
                                    let guard = self.inner.inner_adapter.read().await;
                                    match guard.as_ref() {
                                        Some(adapter) => adapter.list_tools().await,
                                        None => Ok(vec![]),
                                    }
                                }
                                Err(e) => {
                                    warn!(
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
        .instrument(self.inner.span.clone())
        .await
    }

    async fn list_tools_ttl_ms(&self) -> Option<u64> {
        // Delegate to the inner transport adapter, which captured the upstream
        // `ttlMs` (gated on its own negotiated dialect) during `list_tools`.
        let guard = self.inner.inner_adapter.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.list_tools_ttl_ms().await,
            None => None,
        }
    }

    async fn list_resources(&self) -> Result<Vec<Value>, AdapterError> {
        // Delegate to the inner transport adapter. Upstreams that do not
        // expose `resources/list` return `-32601`, which the registry
        // tolerates by skipping the endpoint.
        let guard = self.inner.inner_adapter.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.list_resources().await,
            None => Ok(vec![]),
        }
    }

    async fn list_resource_templates(&self) -> Result<Vec<Value>, AdapterError> {
        let guard = self.inner.inner_adapter.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.list_resource_templates().await,
            None => Ok(vec![]),
        }
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, AdapterError> {
        // Delegate to the inner transport adapter. The inner adapter forwards
        // `resources/read` upstream and returns the raw `result`; the registry
        // returns it to the client unmodified per DD2.
        let guard = self.inner.inner_adapter.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.read_resource(uri).await,
            None => Err(AdapterError::NotInitialized),
        }
    }

    async fn list_prompts(&self) -> Result<Vec<Value>, AdapterError> {
        // Delegate to the inner transport adapter. Upstreams that do not
        // expose `prompts/list` return `-32601`, which the registry
        // tolerates by skipping the endpoint.
        let guard = self.inner.inner_adapter.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.list_prompts().await,
            None => Ok(vec![]),
        }
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<Value, AdapterError> {
        // Delegate to the inner transport adapter. The inner adapter forwards
        // `prompts/get` upstream and returns the raw `result`; the registry
        // rewrites any enumerated resource URIs on returned messages before
        // forwarding to the client.
        let guard = self.inner.inner_adapter.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.get_prompt(name, arguments).await,
            None => Err(AdapterError::NotInitialized),
        }
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
        self.call_tool_with_request_params(name, arguments, serde_json::Map::new())
            .await
    }

    async fn call_tool_with_request_params(
        &self,
        name: &str,
        arguments: Value,
        request_params: serde_json::Map<String, Value>,
    ) -> Result<Value, AdapterError> {
        // NB: do NOT wrap the inner adapter's `call_tool` invocations in
        // `.instrument(self.inner.span)`. The inner `HttpAdapter::call_tool`
        // captures the caller's per-request span context (`request{request_uid}`,
        // `mcp_request{profile}`) BEFORE entering its own span so it can
        // attach `request_uid`/`profile` to the `ToolCallEvent::Started` it
        // publishes (the matching `Completed`/`Failed` are terse and correlate
        // back via the shared per-call `request_id`).
        // OAuth's `inner.span` is the persistent endpoint span built at init
        // time with no parent linkage to per-request spans, so wrapping the
        // inner call here would zero out those fields on the `Started` event for
        // every OAuth-routed tool call. The inner adapter was built with this
        // same endpoint span (see `build_inner_adapter`) and enters it via its
        // own `.instrument(self.span)` AFTER the capture, so its tool-call
        // tracing lines still carry `endpoint`/`transport="oauth"` for the
        // Logs tab. The OAuth endpoint span is still applied around the
        // refresh / state-transition branches below where it actually adds
        // useful context.
        let guard = self.inner.inner_adapter.read().await;
        let adapter = match guard.as_ref() {
            Some(a) => a,
            None => {
                return Err(AdapterError::ConnectionFailed(
                    "not authenticated — complete OAuth login first".to_string(),
                ));
            }
        };

        match adapter
            .call_tool_with_request_params(name, arguments.clone(), request_params.clone())
            .await
        {
            Ok(result) => Ok(result),
            Err(AdapterError::HttpError { status: 401, .. }) => {
                // Drop the read lock before refreshing
                drop(guard);

                let refresh_result = async {
                    info!("Got 401, attempting token refresh");
                    self.inner.do_token_refresh().await
                }
                .instrument(self.inner.span.clone())
                .await;

                match refresh_result {
                    Ok(new_tokens) => {
                        self.inner.apply_refreshed_tokens(new_tokens).await;
                        // Retry with new token — again in caller's span so
                        // the inner adapter sees the per-request scope.
                        let guard = self.inner.inner_adapter.read().await;
                        let adapter = guard.as_ref().ok_or_else(|| {
                            AdapterError::ConnectionFailed(
                                "Adapter lost during refresh".to_string(),
                            )
                        })?;
                        adapter
                            .call_tool_with_request_params(name, arguments, request_params)
                            .await
                    }
                    Err(e) => {
                        async {
                            warn!(
                                error = %e,
                                "Token refresh after 401 failed"
                            );
                            self.inner
                                .transition_to(
                                    OAuthState::AuthRequired,
                                    "401 on call_tool, refresh failed",
                                )
                                .await;
                        }
                        .instrument(self.inner.span.clone())
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

    fn upstream_server_name(&self) -> Option<String> {
        self.inner
            .inner_adapter
            .try_read()
            .ok()
            .and_then(|g| g.as_ref().and_then(|a| a.upstream_server_name()))
    }

    fn configured_server_type(&self) -> Option<String> {
        effective_server_type(self.inner.config.server_type_override.clone(), None)
            .map(|s| s.to_lowercase())
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        let span = self.inner.span.clone();
        async {
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
            // Abort inner→outer tools-changed forwarder
            self.inner.swap_tools_forwarder(None).await;
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
        .instrument(span)
        .await
    }

    fn subscribe_tools_changed(&self) -> Option<broadcast::Receiver<()>> {
        Some(self.inner.outer_tools_changed_tx.subscribe())
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

    fn set_event_bus(&self, bus: crate::events::ToolCallEventBus) {
        // Writes through the shared `OnceLock` cell. Every inner HTTP
        // adapter built (now or after a token swap) shares this cell via
        // [`HttpAdapter::set_event_bus_handle`], so the bus reaches both
        // the current inner adapter and any future one.
        let _ = self.inner.event_bus.set(bus);
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
            probe_timeout_secs: 10,
            probe_failure_threshold: 3,
            server_type_override: None,
            allow_insecure_oauth: false,
            ema: None,
        }
    }

    fn make_adapter(config: OAuthAdapterConfig) -> OAuthAdapter {
        let tmp = tempfile::tempdir().unwrap().keep();
        let tm = Arc::new(TokenManager::new(tmp));
        OAuthAdapter::new(config, tm)
    }

    // --- EMA refresh branch (END-18 T6) -------------------------------------

    fn make_ema_config(idp: &str, resource: &str) -> OAuthAdapterConfig {
        OAuthAdapterConfig {
            endpoint_name: "ema-ep".to_string(),
            url: resource.to_string(),
            token_endpoint_url: format!("{}/as/token", resource),
            client_id: ENDARA_CLIENT_METADATA_URL.to_string(),
            client_secret: None,
            heartbeat_interval_secs: 30,
            probe_timeout_secs: 10,
            probe_failure_threshold: 3,
            server_type_override: None,
            allow_insecure_oauth: true,
            ema: Some(EmaConfig {
                idp_key: idp.to_string(),
                idp_issuer: idp.to_string(),
                idp_authorization_endpoint: format!("{}/authorize", idp),
                idp_token_endpoint: format!("{}/token", idp),
                as_issuer: format!("{}/as", resource),
                as_token_endpoint: format!("{}/as/token", resource),
                resource: resource.to_string(),
                client_id: None,
                client_secret: None,
                resource_client_id: None,
                resource_client_secret: None,
                resource_scope: None,
            }),
        }
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// An EMA endpoint's refresh routes through `ema::ensure_access_token`; a
    /// still-valid persisted access token is returned via the chain's fast path
    /// with no network contact (the bogus IdP/AS endpoints are never hit).
    #[tokio::test]
    async fn ema_refresh_returns_valid_cached_token_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let token = TokenSet {
            access_token: "cached-ema-access".to_string(),
            refresh_token: None,
            expires_at: Some(now_secs() + 3600),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: Some(now_secs()),
        };
        tm.save("ema-ep", &token).await.unwrap();

        let config = make_ema_config("http://127.0.0.1:1", "http://127.0.0.1:2");
        let adapter = OAuthAdapter::new(config, tm);
        let ts = adapter
            .inner
            .do_token_refresh()
            .await
            .expect("EMA refresh returns the cached token");
        assert_eq!(ts.access_token, "cached-ema-access");
    }

    /// A successful EMA refresh clears any stale IdP authorize URL left over from
    /// a prior re-SSO-required outcome, so callers stop surfacing a sign-in link
    /// once the endpoint re-authenticates.
    #[tokio::test]
    async fn ema_refresh_success_clears_pending_authorize_url() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let token = TokenSet {
            access_token: "cached-ema-access".to_string(),
            refresh_token: None,
            expires_at: Some(now_secs() + 3600),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: Some(now_secs()),
        };
        tm.save("ema-ep", &token).await.unwrap();

        let config = make_ema_config("http://127.0.0.1:1", "http://127.0.0.1:2");
        let adapter = OAuthAdapter::new(config, tm);
        // Seed a stale authorize URL as if a prior refresh required re-SSO.
        *adapter.inner.pending_authorize_url.write().await =
            Some("https://stale.example/authorize".to_string());

        adapter
            .inner
            .do_token_refresh()
            .await
            .expect("EMA refresh returns the cached token");
        assert!(
            adapter.inner.pending_authorize_url().await.is_none(),
            "stale authorize URL must be cleared after a successful EMA refresh"
        );
    }

    /// With no stored IdP credentials the EMA chain is terminal: the adapter
    /// reports the EMA error, transitions to `AuthRequired`, and — without SSO
    /// wiring — composes no authorize URL.
    #[tokio::test]
    async fn ema_refresh_without_idp_credentials_sets_auth_required() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let config = make_ema_config("http://127.0.0.1:1", "http://127.0.0.1:2");
        let adapter = OAuthAdapter::new(config, tm);

        let err = adapter.inner.do_token_refresh().await.unwrap_err();
        assert!(matches!(err, OAuthError::Ema(_)), "got {err:?}");
        assert!(adapter.inner.pending_authorize_url().await.is_none());
        assert_eq!(
            adapter.inner.state.read().await.clone(),
            OAuthState::AuthRequired
        );
    }

    /// When SSO wiring is present, a re-auth-required EMA refresh composes an
    /// IdP authorize URL via `start_idp_flow` with scope `openid offline_access`
    /// (M1) and registers a pending flow tagged with the IdP issuer so the
    /// `/oauth/callback` handler persists `IdpCredentials`.
    #[tokio::test]
    async fn ema_refresh_reauth_composes_idp_authorize_url_with_offline_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let flow_mgr = Arc::new(OAuthFlowManager::new());
        let config = make_ema_config("https://acme.okta.com", "https://api.example.com/mcp");
        let adapter = OAuthAdapter::new_ema(
            config,
            tm,
            EmaSsoWiring {
                flow_manager: flow_mgr.clone(),
                relay_port: 9400,
            },
        );

        let err = adapter.inner.do_token_refresh().await.unwrap_err();
        assert!(matches!(err, OAuthError::Ema(_)), "got {err:?}");

        let url = adapter
            .inner
            .pending_authorize_url()
            .await
            .expect("authorize URL composed on re-auth");
        assert!(
            url.starts_with("https://acme.okta.com/authorize?"),
            "got {url}"
        );
        assert!(
            url.contains("scope=openid+offline_access"),
            "scope must include openid offline_access; got {url}"
        );
        assert!(url.contains("code_challenge_method=S256"), "got {url}");
        assert!(
            url.contains(&format!(
                "client_id={}",
                form_urlencode(ENDARA_CLIENT_METADATA_URL)
            )),
            "got {url}"
        );

        let parsed = url::Url::parse(&url).unwrap();
        let state_param = parsed
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("state param present");
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending IdP flow registered");
        assert_eq!(flow.endpoint_name, "ema-ep");
        assert_eq!(flow.idp_issuer.as_deref(), Some("https://acme.okta.com"));
        assert_eq!(flow.client_id, ENDARA_CLIENT_METADATA_URL);
        assert_eq!(flow.token_endpoint, "https://acme.okta.com/token");
        assert_eq!(
            adapter.inner.state.read().await.clone(),
            OAuthState::AuthRequired
        );
    }

    /// A non-EMA adapter is unaffected: `do_token_refresh` follows the standard
    /// `refresh_token` path and fails with `NoRefreshToken` (never the EMA
    /// branch) when no refresh token is present.
    #[tokio::test]
    async fn non_ema_refresh_uses_standard_path() {
        let mut adapter = make_adapter(make_config());
        adapter.initialize().await.unwrap();
        let err = adapter.inner.do_token_refresh().await.unwrap_err();
        assert!(
            matches!(err, OAuthError::NoRefreshToken { .. }),
            "got {err:?}"
        );
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

    /// `set_token_endpoint_override` populates the in-memory override so that
    /// `effective_token_endpoint` returns the new URL on the next refresh.
    /// Covers the management `/oauth/callback` propagation path (the bug
    /// where a freshly discovered token endpoint was not seen by the
    /// proactive refresh fired ~45 minutes later).
    #[tokio::test]
    async fn set_token_endpoint_override_takes_effect() {
        let adapter = make_adapter(make_config());
        // Before the override is set, `effective_token_endpoint` returns the
        // URL from `config.token_endpoint_url`.
        assert_eq!(
            adapter.inner.effective_token_endpoint().await,
            "http://localhost/token"
        );

        adapter
            .inner
            .set_token_endpoint_override("https://oauth2.googleapis.com/token".to_string())
            .await;

        assert_eq!(
            adapter.inner.effective_token_endpoint().await,
            "https://oauth2.googleapis.com/token"
        );

        // Replacing again overwrites the previous value (idempotent setter).
        adapter
            .inner
            .set_token_endpoint_override("https://oauth2.googleapis.com/v2/token".to_string())
            .await;
        assert_eq!(
            adapter.inner.effective_token_endpoint().await,
            "https://oauth2.googleapis.com/v2/token"
        );
    }

    /// `set_client_credentials` installs an in-memory override that
    /// supersedes `config.client_id` / `config.client_secret` for every
    /// subsequent `effective_client_*` read. Mirrors the behaviour of
    /// `set_token_endpoint_override`.
    #[tokio::test]
    async fn set_client_credentials_takes_effect() {
        let adapter = make_adapter(make_config());
        assert_eq!(adapter.inner.effective_client_id().await, "test-client");
        assert!(adapter.inner.effective_client_secret().await.is_none());

        adapter
            .inner
            .set_client_credentials("fresh-client".to_string(), Some("fresh-secret".to_string()))
            .await;

        assert_eq!(adapter.inner.effective_client_id().await, "fresh-client");
        assert_eq!(
            adapter.inner.effective_client_secret().await.as_deref(),
            Some("fresh-secret")
        );

        // Replacing again overwrites the previous value (idempotent setter).
        adapter
            .inner
            .set_client_credentials("even-fresher".to_string(), None)
            .await;
        assert_eq!(adapter.inner.effective_client_id().await, "even-fresher");
        assert!(adapter.inner.effective_client_secret().await.is_none());
    }

    /// After `set_client_credentials`, the refresh POST body must carry
    /// the override `client_id` (and `client_secret` when present) — not
    /// the stale `config.client_id` / `config.client_secret` baked in at
    /// adapter construction. Guards Finding 2 of PR #130: without the
    /// propagation, the next refresh after a DCR re-registration would
    /// keep POSTing the pre-re-registration client_id and loop through
    /// the `invalid_client` self-heal.
    #[tokio::test]
    async fn refresh_uses_overridden_client_credentials() {
        use axum::extract::Form;
        use axum::http::StatusCode;
        use axum::{response::IntoResponse, routing::post, Router};
        use std::collections::HashMap;
        use std::sync::Mutex;

        // Mock token endpoint that captures the last POSTed form and
        // always returns 401 invalid_client (we only care about the body
        // this adapter sent, not the response shape).
        type CapturedForm = Arc<Mutex<Option<HashMap<String, String>>>>;
        let captured: CapturedForm = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        async fn handler(
            axum::extract::State(cap): axum::extract::State<CapturedForm>,
            Form(form): Form<HashMap<String, String>>,
        ) -> impl IntoResponse {
            *cap.lock().unwrap() = Some(form);
            (StatusCode::UNAUTHORIZED, "{\"error\":\"invalid_client\"}")
        }
        let router = Router::new()
            .route("/token", post(handler))
            .with_state(captured_clone);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let url = format!("http://127.0.0.1:{}/token", addr.port());

        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let mut config = make_config();
        config.token_endpoint_url = url;
        // Config baked in at startup carries the STALE credentials.
        config.client_id = "stale-client".to_string();
        config.client_secret = Some("stale-secret".to_string());
        let adapter = make_adapter_with_shared_tm(config, tm.clone()).await;

        // A successful interactive re-authorization propagated the
        // freshly re-registered credentials into the override.
        adapter
            .inner
            .set_client_credentials("fresh-client".to_string(), Some("fresh-secret".to_string()))
            .await;

        let _ = adapter.inner.do_token_refresh().await;

        let form = captured
            .lock()
            .unwrap()
            .clone()
            .expect("refresh POST must have reached the mock token endpoint");
        assert_eq!(
            form.get("client_id").map(String::as_str),
            Some("fresh-client"),
            "refresh must POST the overridden client_id, not the stale config value"
        );
        assert_eq!(
            form.get("client_secret").map(String::as_str),
            Some("fresh-secret"),
            "refresh must POST the overridden client_secret, not the stale config value"
        );

        server.abort();
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
        adapter.inner.disconnect().await.unwrap();
        assert_eq!(adapter.health(), HealthStatus::Stopped);

        // Verify tokens are deleted from disk
        let loaded = tm.load("test").await.unwrap();
        assert!(loaded.is_none());

        // Verify in-memory tokens cleared
        let tokens = adapter.inner.tokens.read().await;
        assert!(tokens.is_none());
    }

    /// Reset-vs-refresh race (PR #145 review): a refresh whose network
    /// exchange finished BEFORE the reset's disconnect must not commit its
    /// tokens afterwards — `apply_refreshed_tokens` observes the cleared
    /// in-memory tokens under the `apply_lock` and drops the result instead
    /// of resurrecting the discarded grant on disk.
    #[tokio::test]
    async fn refreshed_tokens_dropped_after_disconnect() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let config = make_config();
        let mut adapter = OAuthAdapter::new(config, tm.clone());
        adapter.initialize().await.unwrap();

        let token_set = TokenSet {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_tokens(token_set).await;
        assert!(tm.load("test").await.unwrap().is_some());

        // The reset's disconnect lands first.
        adapter.inner.disconnect().await.unwrap();
        assert!(tm.load("test").await.unwrap().is_none());

        // A refresh that raced the disconnect now tries to commit its
        // result: it must be dropped, not persisted.
        let refreshed = TokenSet {
            access_token: "refreshed-access".to_string(),
            refresh_token: Some("refreshed-refresh".to_string()),
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_refreshed_tokens(refreshed).await;

        assert!(
            tm.load("test").await.unwrap().is_none(),
            "post-disconnect refresh commit must not resurrect the grant on disk"
        );
        assert!(adapter.inner.tokens.read().await.is_none());

        // A NEW grant (callback login after the replacement start flow, which
        // moves the endpoint back out of Disconnected) still applies from the
        // clean slate.
        adapter
            .inner
            .transition_to(OAuthState::NeedsLogin, "test: replacement start flow")
            .await;
        let new_grant = TokenSet {
            access_token: "new-access".to_string(),
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_tokens(new_grant).await;
        assert_eq!(
            tm.load("test").await.unwrap().unwrap().access_token,
            "new-access"
        );
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

    // --- do_token_refresh error-path tests ---

    /// Spawn a tiny axum server on `127.0.0.1:0` that serves the configured
    /// canned response on `POST /token`. Used to exercise the failure paths
    /// of `do_token_refresh` without a real OAuth provider.
    async fn spawn_token_server(mode: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        use axum::http::StatusCode;
        use axum::{response::IntoResponse, routing::post, Router};

        async fn bad_request() -> impl IntoResponse {
            (StatusCode::BAD_REQUEST, "{\"error\":\"invalid_grant\"}")
        }
        async fn malformed() -> impl IntoResponse {
            (StatusCode::OK, "not json")
        }
        async fn invalid_client() -> impl IntoResponse {
            (StatusCode::UNAUTHORIZED, "{\"error\":\"invalid_client\"}")
        }

        let router = match mode {
            "400" => Router::new().route("/token", post(bad_request)),
            "malformed" => Router::new().route("/token", post(malformed)),
            "invalid_client" => Router::new().route("/token", post(invalid_client)),
            other => panic!("unknown spawn_token_server mode: {}", other),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        // Tiny delay to let the server start accepting connections.
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/token", addr.port()), handle)
    }

    /// Helper: build an adapter pre-loaded with a refresh token so
    /// `do_token_refresh` reaches the network call.
    async fn make_adapter_with_refresh_token(config: OAuthAdapterConfig) -> OAuthAdapter {
        let adapter = make_adapter(config);
        *adapter.inner.tokens.write().await = Some(TokenSet {
            access_token: "old-access".to_string(),
            refresh_token: Some("test-refresh".to_string()),
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        });
        adapter
    }

    /// Network error (closed port → ECONNREFUSED) must transition out of
    /// `Refreshing` into `ConnectionFailed`, not leave the state stuck.
    #[tokio::test]
    async fn do_token_refresh_network_error_transitions_to_connection_failed() {
        let mut config = make_config();
        // Port 1 is reserved and not bound; this triggers an immediate
        // connection-refused error rather than waiting for the 30s timeout.
        config.token_endpoint_url = "http://127.0.0.1:1/token".to_string();
        let adapter = make_adapter_with_refresh_token(config).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(
            result.is_err(),
            "expected refresh to fail with network error"
        );

        let state = adapter.inner.state.read().await.clone();
        assert_eq!(
            state,
            OAuthState::ConnectionFailed,
            "network error must transition out of Refreshing into ConnectionFailed"
        );
        assert!(
            adapter
                .inner
                .metrics
                .snapshot()
                .oauth_token_refresh_total_failure
                >= 1,
            "refresh failure metric must increment"
        );
    }

    /// Non-2xx HTTP response (4xx) must transition into `AuthRequired`.
    #[tokio::test]
    async fn do_token_refresh_4xx_transitions_to_auth_required() {
        let (url, server) = spawn_token_server("400").await;
        let mut config = make_config();
        config.token_endpoint_url = url;
        let adapter = make_adapter_with_refresh_token(config).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(
            matches!(result, Err(OAuthError::RefreshFailed { .. })),
            "expected RefreshFailed error, got {:?}",
            result
        );

        let state = adapter.inner.state.read().await.clone();
        assert_eq!(
            state,
            OAuthState::AuthRequired,
            "4xx must transition out of Refreshing into AuthRequired"
        );
        assert!(
            adapter
                .inner
                .metrics
                .snapshot()
                .oauth_token_refresh_total_failure
                >= 1,
            "refresh failure metric must increment"
        );

        server.abort();
    }

    /// 200 OK with a non-JSON body must transition into `ConnectionFailed`
    /// (treat as a transient upstream-quality problem so retry can recover).
    #[tokio::test]
    async fn do_token_refresh_malformed_json_transitions_to_connection_failed() {
        let (url, server) = spawn_token_server("malformed").await;
        let mut config = make_config();
        config.token_endpoint_url = url;
        let adapter = make_adapter_with_refresh_token(config).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(
            result.is_err(),
            "expected refresh to fail with JSON parse error"
        );

        let state = adapter.inner.state.read().await.clone();
        assert_eq!(
            state,
            OAuthState::ConnectionFailed,
            "JSON parse failure must transition out of Refreshing into ConnectionFailed"
        );
        assert!(
            adapter
                .inner
                .metrics
                .snapshot()
                .oauth_token_refresh_total_failure
                >= 1,
            "refresh failure metric must increment"
        );

        server.abort();
    }

    /// Helper: build an adapter that shares the given `TokenManager` (so the
    /// test can pre-seed `{endpoint}.dcr.json`) and is pre-loaded with a
    /// refresh token so `do_token_refresh` reaches the network call.
    async fn make_adapter_with_shared_tm(
        config: OAuthAdapterConfig,
        tm: Arc<TokenManager>,
    ) -> OAuthAdapter {
        let adapter = OAuthAdapter::new(config, tm);
        *adapter.inner.tokens.write().await = Some(TokenSet {
            access_token: "old-access".to_string(),
            refresh_token: Some("test-refresh".to_string()),
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        });
        adapter
    }

    /// `invalid_client` (RFC 6749 §5.2) from the token endpoint with a
    /// DCR-registered record whose `client_id` matches the requesting
    /// client must atomically clear the stale requesting pair, land in
    /// `AuthRequired`, and record a distinct transition reason so callers
    /// surface a re-authorize hint. The record itself is preserved as a
    /// stub with `registered_via_dcr = true` so the next authorize prefers
    /// re-registration over any stale `config.toml` `client_id`.
    #[tokio::test]
    async fn do_token_refresh_invalid_client_clears_dcr_registered_record() {
        use crate::token_manager::DcrCredentials;

        let (url, server) = spawn_token_server("invalid_client").await;
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        // Seed a DCR-minted credential record that matches the adapter's
        // configured `client_id` — that pair is what the refresh POST will
        // present to the token endpoint.
        tm.save_dcr(
            "test",
            &DcrCredentials {
                client_id: "test-client".to_string(),
                registered_via_dcr: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let mut config = make_config();
        config.token_endpoint_url = url;
        let adapter = make_adapter_with_shared_tm(config, tm.clone()).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(
            matches!(result, Err(OAuthError::RefreshFailed { .. })),
            "expected RefreshFailed, got {:?}",
            result
        );

        let loaded = tm
            .load_dcr("test")
            .await
            .unwrap()
            .expect("pure-DCR self-heal must retain a stub record so auth-start re-registers");
        assert_eq!(loaded.client_id, "");
        assert!(loaded.client_secret.is_none());
        assert!(
            loaded.registered_via_dcr,
            "registered_via_dcr must survive so auth-start prefers re-registration over the stale config.toml client_id"
        );
        assert_eq!(
            adapter.inner.state.read().await.clone(),
            OAuthState::AuthRequired,
            "invalid_client must still land in AuthRequired"
        );
        let history = adapter.inner.transition_history.read().await;
        assert!(
            history
                .iter()
                .any(|r| r.reason == "client registration invalidated; re-authorize to re-register"),
            "expected the distinct invalid_client transition reason, got: {:?}",
            history.iter().map(|r| r.reason.clone()).collect::<Vec<_>>()
        );

        server.abort();
    }

    /// A mixed DCR record (the requesting `client_id`/`client_secret` sits
    /// alongside an operator-set `resource_client_id`/`resource_client_secret`)
    /// must have its requesting pair cleared on `invalid_client` while the
    /// resource pair survives — the resource credential is a distinct
    /// registration used only at the MAS in Step 3.
    #[tokio::test]
    async fn do_token_refresh_invalid_client_preserves_resource_pair() {
        use crate::token_manager::DcrCredentials;

        let (url, server) = spawn_token_server("invalid_client").await;
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let mixed = DcrCredentials {
            client_id: "test-client".to_string(),
            client_secret: Some("test-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 1_700_000_000,
            issuer: Some("https://as.example.com".to_string()),
            resource_client_id: Some("mas-resource".to_string()),
            resource_client_secret: Some("mas-resource-secret".to_string()),
            registered_via_dcr: true,
        };
        tm.save_dcr("test", &mixed).await.unwrap();

        let mut config = make_config();
        config.token_endpoint_url = url;
        let adapter = make_adapter_with_shared_tm(config, tm.clone()).await;

        let _ = adapter.inner.do_token_refresh().await;

        let loaded = tm
            .load_dcr("test")
            .await
            .unwrap()
            .expect("mixed record must persist so the resource pair is retained");
        assert_eq!(loaded.client_id, "");
        assert!(loaded.client_secret.is_none());
        assert!(
            loaded.registered_via_dcr,
            "mixed-record self-heal must retain the DCR provenance flag so auth-start prefers re-registration over the stale config.toml client_id"
        );
        assert_eq!(loaded.resource_client_id.as_deref(), Some("mas-resource"));
        assert_eq!(
            loaded.resource_client_secret.as_deref(),
            Some("mas-resource-secret")
        );

        server.abort();
    }

    /// A concurrent re-registration replaces the DCR record with a NEWER
    /// `client_id` while an in-flight refresh is still using the previous
    /// one. When that stale refresh eventually returns `invalid_client`,
    /// the self-heal must not touch the newer record: `invalid_client`
    /// only proves the presented `client_id` is gone.
    #[tokio::test]
    async fn do_token_refresh_invalid_client_leaves_newer_registration_intact() {
        use crate::token_manager::DcrCredentials;

        let (url, server) = spawn_token_server("invalid_client").await;
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        // Persisted record already carries a NEWER client_id (a concurrent
        // re-registration succeeded first). The adapter's config still has
        // the old client_id — that is the one this refresh will present.
        let newer = DcrCredentials {
            client_id: "fresh-client".to_string(),
            client_secret: Some("fresh-secret".to_string()),
            registered_via_dcr: true,
            ..Default::default()
        };
        tm.save_dcr("test", &newer).await.unwrap();

        let mut config = make_config();
        config.token_endpoint_url = url;
        assert_eq!(config.client_id, "test-client");
        let adapter = make_adapter_with_shared_tm(config, tm.clone()).await;

        let _ = adapter.inner.do_token_refresh().await;

        let loaded = tm
            .load_dcr("test")
            .await
            .unwrap()
            .expect("newer registration must survive stale invalid_client");
        assert_eq!(loaded.client_id, "fresh-client");
        assert_eq!(loaded.client_secret.as_deref(), Some("fresh-secret"));
        assert!(loaded.registered_via_dcr);

        let history = adapter.inner.transition_history.read().await;
        assert!(
            !history
                .iter()
                .any(|r| r.reason == "client registration invalidated; re-authorize to re-register"),
            "stale invalid_client for a superseded client_id must NOT emit the re-register reason"
        );

        server.abort();
    }

    /// A manually-supplied credential record (`registered_via_dcr == false`)
    /// must survive an `invalid_client` at the token endpoint — only
    /// DCR-minted records are auto-discarded.
    #[tokio::test]
    async fn do_token_refresh_invalid_client_preserves_manual_record() {
        use crate::token_manager::DcrCredentials;

        let (url, server) = spawn_token_server("invalid_client").await;
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        // Seed a manually-supplied credential record (registered_via_dcr = false).
        let manual = DcrCredentials {
            client_id: "manual-client-abc".to_string(),
            registered_via_dcr: false,
            ..Default::default()
        };
        tm.save_dcr("test", &manual).await.unwrap();

        let mut config = make_config();
        config.token_endpoint_url = url;
        let adapter = make_adapter_with_shared_tm(config, tm.clone()).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(matches!(result, Err(OAuthError::RefreshFailed { .. })));

        let loaded = tm
            .load_dcr("test")
            .await
            .unwrap()
            .expect("manual credential record must be preserved");
        assert_eq!(loaded, manual);
        assert_eq!(
            adapter.inner.state.read().await.clone(),
            OAuthState::AuthRequired
        );
        let history = adapter.inner.transition_history.read().await;
        assert!(
            !history
                .iter()
                .any(|r| r.reason == "client registration invalidated; re-authorize to re-register"),
            "manual credential must NOT trigger the invalid_client re-register reason"
        );

        server.abort();
    }

    /// Other OAuth errors (e.g. `invalid_grant`) must never touch a
    /// DCR-registered credential record — only `invalid_client` triggers
    /// the self-heal.
    #[tokio::test]
    async fn do_token_refresh_invalid_grant_preserves_dcr_registered_record() {
        use crate::token_manager::DcrCredentials;

        let (url, server) = spawn_token_server("400").await;
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let dcr = DcrCredentials {
            client_id: "dcr-client-xyz".to_string(),
            registered_via_dcr: true,
            ..Default::default()
        };
        tm.save_dcr("test", &dcr).await.unwrap();

        let mut config = make_config();
        config.token_endpoint_url = url;
        let adapter = make_adapter_with_shared_tm(config, tm.clone()).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(matches!(result, Err(OAuthError::RefreshFailed { .. })));

        let loaded = tm
            .load_dcr("test")
            .await
            .unwrap()
            .expect("invalid_grant must not delete the DCR credential record");
        assert_eq!(loaded, dcr);

        server.abort();
    }

    /// The token-body sniff must be strictly `error == "invalid_client"`:
    /// unrelated JSON shapes and non-JSON bodies must not trigger deletion.
    #[test]
    fn is_invalid_client_error_shape_and_negatives() {
        assert!(is_invalid_client_error("{\"error\":\"invalid_client\"}"));
        assert!(is_invalid_client_error(
            "{\"error\":\"invalid_client\",\"error_description\":\"unknown client\"}"
        ));
        assert!(!is_invalid_client_error("{\"error\":\"invalid_grant\"}"));
        assert!(!is_invalid_client_error("{\"error\":\"invalid_request\"}"));
        assert!(!is_invalid_client_error("{}"));
        assert!(!is_invalid_client_error(""));
        assert!(!is_invalid_client_error("not json"));
        assert!(!is_invalid_client_error("<html>error</html>"));
        assert!(!is_invalid_client_error("{\"error\":123}"));
    }

    /// Wait until the shared call counter reaches `target`. Drives the
    /// runtime by advancing virtual time in tiny steps (which yields and
    /// gives in-flight HTTP I/O on the spawned proactive task a chance to
    /// progress against the real reactor). A real-time deadline guards
    /// against an actual hang.
    async fn wait_for_calls(counter: &Arc<std::sync::atomic::AtomicUsize>, target: usize) {
        use std::sync::atomic::Ordering;
        let real_start = std::time::Instant::now();
        while counter.load(Ordering::SeqCst) < target {
            if real_start.elapsed() > std::time::Duration::from_secs(10) {
                panic!(
                    "timed out waiting for refresh call {} (got {})",
                    target,
                    counter.load(Ordering::SeqCst)
                );
            }
            tokio::time::advance(Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
        }
    }

    /// Wait until the transition history contains a record with `reason`.
    /// Mirrors `wait_for_calls`: drives virtual time forward in tiny steps
    /// and yields so the spawned proactive task can record its final
    /// transition, with a real-time deadline guarding against an actual hang.
    async fn wait_for_transition_reason(inner: &OAuthAdapterInner, reason: &str) {
        let real_start = std::time::Instant::now();
        loop {
            {
                let history = inner.transition_history.read().await;
                if history.iter().any(|r| r.reason == reason) {
                    return;
                }
            }
            if real_start.elapsed() > std::time::Duration::from_secs(10) {
                let history = inner.transition_history.read().await;
                panic!(
                    "timed out waiting for transition reason {:?}; got: {:?}",
                    reason,
                    history
                        .iter()
                        .map(|r| (r.from.clone(), r.to.clone(), r.reason.clone()))
                        .collect::<Vec<_>>()
                );
            }
            tokio::time::advance(Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
        }
    }

    /// Spawn an axum token endpoint that always responds 500 and bumps a
    /// shared counter on every request. Used to drive the proactive-refresh
    /// retry path (1st failure + 2nd failure) in
    /// `apply_tokens_proactive_refresh_retry_transitions_on_second_failure`.
    async fn spawn_token_server_always_500(
        counter: Arc<std::sync::atomic::AtomicUsize>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::{response::IntoResponse, routing::post, Router};

        async fn handler(
            State(counter): State<Arc<std::sync::atomic::AtomicUsize>>,
        ) -> impl IntoResponse {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "{\"error\":\"server_error\"}",
            )
        }

        let router = Router::new()
            .route("/token", post(handler))
            .with_state(counter);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        // Tiny delay to let the server start accepting connections.
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/token", addr.port()), handle)
    }

    /// Spawn an axum token endpoint that always responds 200 OK with a valid
    /// refresh response (`expires_in` long enough that the next proactive
    /// deadline is far in the future, so the test sees exactly one
    /// proactive-refresh cycle).
    async fn spawn_token_server_success() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handler() -> Json<Value> {
            Json(json!({
                "access_token": "new-access-token",
                "token_type": "Bearer",
                "expires_in": 3600u64,
                "refresh_token": "new-refresh-token",
            }))
        }

        let router = Router::new().route("/token", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/token", addr.port()), handle)
    }

    /// rc.5 regression: the proactive-refresh task must NOT abort itself when
    /// it recursively re-enters `apply_tokens` after a successful
    /// `do_token_refresh`. In rc.4 the spawned task stored its own
    /// `AbortHandle` in `refresh_task_handle`; step 2 of `apply_tokens_inner`
    /// (`handle.take().abort()`) then killed the running task before it could
    /// rebuild the inner adapter and transition to `Authenticated`, leaving
    /// the state pinned in `Refreshing` forever.
    ///
    /// This test arranges for the proactive deadline to fire immediately,
    /// lets the task run on real time (no `tokio::time::pause()`), and
    /// asserts the state reaches `Authenticated` within a short wall-clock
    /// budget. Without the fix the state would remain `Refreshing` past the
    /// timeout and the assertion would fail.
    #[tokio::test]
    async fn proactive_refresh_does_not_self_abort() {
        let (mcp_url, mcp_server) = spawn_minimal_mcp_server().await;
        let (token_url, token_server) = spawn_token_server_success().await;

        let mut config = make_config();
        config.url = mcp_url;
        config.token_endpoint_url = token_url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        // expires_at = now ⇒ proactive deadline collapses to `Instant::now()`,
        // so the spawned task fires on its very first poll.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token_set = TokenSet {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: Some(now_secs),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_tokens(token_set).await;

        // Poll for the *new* access token to be installed in memory. The
        // first `apply_tokens` above already transitions to `Authenticated`
        // (inner-adapter init against the minimal MCP server succeeds), so
        // the state alone is not a useful signal here. The recursive
        // `apply_tokens` invoked from inside the proactive-refresh task is
        // what installs the new token (step 4 of `apply_tokens_inner`):
        //   - With the fix, the task detaches its own handle before that
        //     recursive call, finishes all five steps, and the in-memory
        //     token flips to "new-access-token".
        //   - Without the fix, step 2 of the recursive call aborts the
        //     running task before step 4, so the in-memory token stays
        //     "old-access" forever and this poll times out.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let access = {
                let tokens = adapter.inner.tokens.read().await;
                tokens
                    .as_ref()
                    .map(|t| t.access_token.clone())
                    .unwrap_or_default()
            };
            if access == "new-access-token" {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let state = adapter.inner.state.read().await.clone();
                let history = adapter.inner.transition_history.read().await;
                panic!(
                    "proactive refresh task self-aborted: access_token still {:?}, state {:?} after 5s; transitions: {:?}",
                    access,
                    state,
                    history
                        .iter()
                        .map(|r| (r.from.clone(), r.to.clone(), r.reason.clone()))
                        .collect::<Vec<_>>()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Final state must be `Authenticated` — the recursive
        // `apply_tokens` ran step 3 (rebuild inner adapter) and the
        // accompanying `transition_to(Authenticated, ...)`.
        let state = adapter.inner.state.read().await.clone();
        assert_eq!(
            state,
            OAuthState::Authenticated,
            "after a successful proactive refresh the state must be Authenticated"
        );

        // A fresh proactive refresh task must be scheduled for the new
        // token (its `expires_in` is 3600 s, so its deadline is far in the
        // future and the test does not race a second refresh).
        let handle = adapter.inner.refresh_task_handle.lock().await;
        assert!(
            handle.is_some(),
            "expected a fresh proactive refresh task to be scheduled for the new token"
        );
        drop(handle);

        token_server.abort();
        mcp_server.abort();
    }

    /// HIGH #1 (audit): Task 1 added a defensive `transition_to(...)` call
    /// inside the proactive-refresh retry block that runs after the
    /// **second** consecutive `do_token_refresh` failure (it was previously
    /// just a `warn!` log). This regression test forces the proactive task
    /// to fire, makes the token endpoint fail twice in a row, and asserts
    /// that the retry block recorded its `"proactive refresh retry failed"`
    /// transition in the ring buffer.
    ///
    /// The negative case (only one failure ⇒ no retry-block transition) is
    /// covered implicitly: the test pins the request count to exactly 2,
    /// and the retry-block transition is only reached after the 2nd call
    /// returns `Err`.
    #[tokio::test]
    async fn apply_tokens_proactive_refresh_retry_transitions_on_second_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let (url, server) = spawn_token_server_always_500(calls.clone()).await;

        let mut config = make_config();
        // Point the inner adapter URL at an unreachable port so initialize()
        // returns ECONNREFUSED quickly without consuming the inner client's
        // 30 s timeout.
        config.url = "http://127.0.0.1:19999/mcp".to_string();
        config.token_endpoint_url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        // Abort the heartbeat so its recovery-from-ConnectionFailed branch
        // cannot fire extra do_token_refresh calls and perturb the proactive
        // refresh call count. (tokio interval's first tick is immediate, so an
        // interval bump can't isolate this.) Stopping the task keeps the call
        // count purely from proactive refresh, so the exact == 2 assertion is
        // valid and can't be masked by a heartbeat-supplied call.
        if let Some(h) = adapter.inner.heartbeat_task_handle.lock().await.take() {
            h.abort();
        }

        // Pause time *after* the server is up so the 60 s retry sleep in
        // the proactive task is virtual (we drive it forward with
        // `tokio::time::advance`). Setup before this point — TCP listener
        // bind, axum spawn — runs on real time so I/O works normally.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        tokio::time::pause();

        // expires_at = now_secs ⇒ proactive deadline collapses to
        // `Instant::now()`, so the spawned task's `sleep_until` returns
        // immediately on first poll.
        let token_set = TokenSet {
            access_token: "old-access".to_string(),
            refresh_token: Some("test-refresh".to_string()),
            expires_at: Some(now_secs),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_tokens(token_set).await;

        // Wait for the 1st refresh attempt to reach the fake endpoint.
        // Real I/O still progresses under paused time; we yield to give
        // the spawned proactive task a chance to be polled.
        wait_for_calls(&calls, 1).await;

        // Advance past the proactive task's 60 s retry sleep so the 2nd
        // attempt fires.
        tokio::time::advance(Duration::from_secs(61)).await;

        // Wait for the 2nd refresh attempt.
        wait_for_calls(&calls, 2).await;

        // Deterministically wait for the retry block to run its defensive
        // `transition_to(...)` call after the 2nd Err returns, instead of a
        // fixed yield count that races CI scheduling under paused time.
        wait_for_transition_reason(&adapter.inner, "proactive refresh retry failed").await;

        // Both refresh attempts must have actually hit the fake endpoint —
        // not just one. This pins the off-by-one the audit flagged.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "expected exactly 2 token endpoint calls (initial + 1 retry)"
        );

        // The retry block in `apply_tokens_inner` must have recorded its
        // defensive transition AFTER the 2nd failure.
        let history = adapter.inner.transition_history.read().await;
        assert!(
            history
                .iter()
                .any(|r| r.reason == "proactive refresh retry failed"),
            "expected a transition with reason 'proactive refresh retry failed' \
             after the 2nd refresh failure; got: {:?}",
            history
                .iter()
                .map(|r| (r.from.clone(), r.to.clone(), r.reason.clone()))
                .collect::<Vec<_>>()
        );

        // Final state must be a non-`Refreshing`, non-`Healthy` terminal
        // state. With 500 responses both `do_token_refresh` and the retry
        // block target `AuthRequired`.
        let state = adapter.inner.state.read().await.clone();
        assert_eq!(
            state,
            OAuthState::AuthRequired,
            "after 2nd RefreshFailed the adapter must land in AuthRequired, \
             not stay stuck in Refreshing"
        );

        server.abort();
    }

    /// MED #6 (audit): Task 1 added a 30 s timeout to the OAuth refresh
    /// `reqwest::Client` so a stuck token endpoint can no longer pin the
    /// state machine in `Refreshing` indefinitely. `reqwest::Client` does
    /// not expose its configured timeout post-build, so we pin the value
    /// at the `REFRESH_HTTP_TIMEOUT` constant the builder consumes —
    /// changing the constant forces this assertion to be revisited.
    #[test]
    fn refresh_http_client_uses_30s_timeout() {
        assert_eq!(
            REFRESH_HTTP_TIMEOUT,
            Duration::from_secs(30),
            "OAuth refresh HTTP client timeout must be 30 seconds"
        );
    }

    // --- Tools-changed forwarder (T5) ---

    /// Drain any already-queued ticks so subsequent `recv` reflects new sends.
    async fn drain(rx: &mut broadcast::Receiver<()>) {
        loop {
            match tokio::time::timeout(Duration::from_millis(20), rx.recv()).await {
                Ok(Ok(())) => continue,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => break,
            }
        }
    }

    /// Wait briefly for an outer tick. Returns whether one arrived.
    async fn recv_tick(rx: &mut broadcast::Receiver<()>, timeout: Duration) -> bool {
        matches!(
            tokio::time::timeout(timeout, rx.recv()).await,
            Ok(Ok(())) | Ok(Err(broadcast::error::RecvError::Lagged(_)))
        )
    }

    #[tokio::test]
    async fn subscribe_tools_changed_returns_some() {
        let adapter = make_adapter(make_config());
        // OAuth always exposes its outer broadcast — registry subscribes once.
        assert!(adapter.subscribe_tools_changed().is_some());
    }

    #[tokio::test]
    async fn forwarder_propagates_inner_tick_to_outer_subscriber() {
        let adapter = make_adapter(make_config());
        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");

        let (inner_tx, inner_rx) = broadcast::channel::<()>(16);
        adapter.inner.swap_tools_forwarder(Some(inner_rx)).await;

        inner_tx.send(()).expect("inner send");
        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "outer subscriber should receive tick forwarded from inner"
        );
    }

    #[tokio::test]
    async fn inner_swap_aborts_old_forwarder_and_rebinds_new() {
        let adapter = make_adapter(make_config());
        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");

        // Bind forwarder to inner A.
        let (inner_tx_a, inner_rx_a) = broadcast::channel::<()>(16);
        adapter.inner.swap_tools_forwarder(Some(inner_rx_a)).await;
        // Sanity-check: A propagates.
        inner_tx_a.send(()).expect("inner A send");
        assert!(recv_tick(&mut outer_rx, Duration::from_millis(500)).await);

        // Swap to inner B (simulates inner-adapter replacement).
        let (inner_tx_b, inner_rx_b) = broadcast::channel::<()>(16);
        adapter.inner.swap_tools_forwarder(Some(inner_rx_b)).await;
        drain(&mut outer_rx).await;

        // Firing on the OLD inner must NOT propagate (forwarder aborted →
        // the only subscriber was dropped, so `send` may return SendError;
        // either way nothing should reach the outer subscriber).
        let _ = inner_tx_a.send(());
        assert!(
            !recv_tick(&mut outer_rx, Duration::from_millis(150)).await,
            "old inner sender must not propagate after swap"
        );

        // Firing on the NEW inner DOES propagate (forwarder rebound).
        inner_tx_b.send(()).expect("inner B send");
        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "new inner sender must propagate after swap"
        );
    }

    #[tokio::test]
    async fn apply_tokens_rebinds_forwarder_aborting_prior_inner() {
        // G3 coverage: apply_tokens must drive swap_tools_forwarder so that
        // a stale forwarder bound to a previous inner is aborted. Here the
        // upstream URL is unreachable, so apply_tokens hits the
        // ConnectionFailed branch which calls swap_tools_forwarder(None) —
        // the prior forwarder must stop propagating ticks regardless.
        let mut config = make_config();
        config.url = "http://127.0.0.1:19998/mcp".to_string();
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();
        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");

        // Pre-bind a forwarder to a fake "previous" inner receiver.
        let (prev_inner_tx, prev_inner_rx) = broadcast::channel::<()>(16);
        adapter
            .inner
            .swap_tools_forwarder(Some(prev_inner_rx))
            .await;
        // Sanity: forwarder propagates ticks before apply_tokens runs.
        prev_inner_tx.send(()).expect("pre-apply send");
        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "pre-apply forwarder should propagate"
        );
        drain(&mut outer_rx).await;

        // apply_tokens with unreachable URL → ConnectionFailed branch →
        // swap_tools_forwarder(None) → prior forwarder is aborted.
        let token_set = TokenSet {
            access_token: "fake-token".to_string(),
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        };
        adapter.inner.apply_tokens(token_set).await;

        // Ticks on the previous inner sender must no longer propagate.
        let _ = prev_inner_tx.send(());
        assert!(
            !recv_tick(&mut outer_rx, Duration::from_millis(150)).await,
            "apply_tokens must rebind the forwarder, aborting the prior one"
        );

        // The outer broadcast itself remains alive (registry subscription
        // survives across rebinds): a fresh forwarder bound after apply_tokens
        // still reaches the same outer subscriber.
        let (post_inner_tx, post_inner_rx) = broadcast::channel::<()>(16);
        adapter
            .inner
            .swap_tools_forwarder(Some(post_inner_rx))
            .await;
        post_inner_tx.send(()).expect("post-apply send");
        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "outer broadcast must survive apply_tokens rebind"
        );
    }

    #[tokio::test]
    async fn forwarder_swap_to_none_disables_forwarding() {
        // Simulates inner adapters that don't expose `subscribe_tools_changed`
        // (returns `None`): no forwarder is spawned and OAuth still works.
        let adapter = make_adapter(make_config());
        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");

        let (inner_tx, inner_rx) = broadcast::channel::<()>(16);
        adapter.inner.swap_tools_forwarder(Some(inner_rx)).await;
        adapter.inner.swap_tools_forwarder(None).await;

        // After abort, the forwarder's receiver is dropped — `send` may return
        // SendError (no subscribers). Either way, nothing reaches outer.
        let _ = inner_tx.send(());
        assert!(
            !recv_tick(&mut outer_rx, Duration::from_millis(150)).await,
            "no tick should reach outer subscriber after forwarder disabled"
        );
    }

    // --- apply_tokens readiness-change synthetic tick ---

    fn make_token_set(access: &str) -> TokenSet {
        TokenSet {
            access_token: access.to_string(),
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        }
    }

    /// Regression: when `apply_tokens` transitions the inner adapter from
    /// `None` to `Some` (endpoint just became listable, e.g. after the OAuth
    /// callback), a synthetic tick must reach the outer tools-changed
    /// broadcast so the registry invalidates its stale merged catalog.
    #[tokio::test]
    async fn apply_tokens_none_to_some_emits_outer_tick() {
        let (url, server) = spawn_minimal_mcp_server().await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();
        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");

        adapter
            .inner
            .apply_tokens(make_token_set("test-access"))
            .await;

        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "None→Some inner readiness transition must emit a synthetic outer tick"
        );
        server.abort();
    }

    /// A routine Some→Some token refresh (inner adapter rebuilt, endpoint
    /// stays listable) must NOT emit a synthetic tick — that would spam
    /// clients with `notifications/tools/list_changed` on every refresh.
    /// This variant pins the probe-failure path: the minimal server answers
    /// `tools/list` without a `tools` field, so both fingerprint probes fail
    /// and the swap must stay silent (a change is undetectable and the
    /// registry's own refetch would fail too). The unchanged-tools no-tick
    /// path is pinned by `apply_tokens_some_to_some_tool_change_emits_tick`.
    #[tokio::test]
    async fn apply_tokens_some_to_some_refresh_emits_no_tick() {
        let (url, server) = spawn_minimal_mcp_server().await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        // First apply: None→Some (tick expected, not asserted here).
        adapter.inner.apply_tokens(make_token_set("first")).await;

        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");
        drain(&mut outer_rx).await;

        // Second apply: Some→Some routine refresh.
        adapter.inner.apply_tokens(make_token_set("second")).await;

        assert!(
            !recv_tick(&mut outer_rx, Duration::from_millis(200)).await,
            "routine Some→Some token refresh must not emit a synthetic tick"
        );
        server.abort();
    }

    /// Spawn an in-process MCP server whose `initialize` response blocks
    /// until the returned `Notify` is notified — lets a test observe the
    /// adapter's state/health while `apply_tokens` is mid-rebuild.
    async fn spawn_blocking_init_mcp_server() -> (
        String,
        Arc<tokio::sync::Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::extract::State;
        use axum::{routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handle(
            State(gate): State<Arc<tokio::sync::Notify>>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method == "initialize" {
                gate.notified().await;
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
                Json(json!({"jsonrpc": "2.0", "id": id, "result": {}}))
            }
        }

        let gate = Arc::new(tokio::sync::Notify::new());
        let router = Router::new()
            .route("/mcp", post(handle))
            .with_state(gate.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (
            format!("http://127.0.0.1:{}/mcp", addr.port()),
            gate,
            handle,
        )
    }

    /// An apply entered from an error state (here AuthRequired) must report
    /// `Starting` while the inner adapter rebuild is in flight — not the
    /// stale error — and still end `Authenticated`/`Healthy` on success.
    #[tokio::test]
    async fn apply_tokens_from_error_state_reports_starting_mid_apply() {
        let (url, gate, server) = spawn_blocking_init_mcp_server().await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        // Pin the adapter in an error state, as after a failed refresh.
        adapter
            .inner
            .transition_to(OAuthState::AuthRequired, "test: pin error state")
            .await;
        assert!(
            matches!(adapter.health(), HealthStatus::Unhealthy(_)),
            "precondition: AuthRequired must report Unhealthy"
        );

        let inner = adapter.inner.clone();
        let apply = tokio::spawn(async move {
            inner.apply_tokens(make_token_set("mid-apply")).await;
        });

        // The inner init is blocked on the gate: wait for the apply to enter
        // Refreshing and assert health reports Starting, not the stale error.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let state = adapter.inner.state.read().await.clone();
            if state == OAuthState::Refreshing {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for Refreshing mid-apply; state {:?}",
                state
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            adapter.health(),
            HealthStatus::Starting,
            "mid-apply health must be Starting, not the stale AuthRequired error"
        );

        // Release the inner init and let the apply finish.
        gate.notify_one();
        apply.await.unwrap();
        assert_eq!(
            adapter.inner.state.read().await.clone(),
            OAuthState::Authenticated
        );
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        server.abort();
    }

    /// Spawn an in-process MCP server whose `initialize` deterministically
    /// fails with a JSON-RPC error — pins the apply's failure branch without
    /// depending on a fixed port being unbound on the host.
    async fn spawn_failing_init_mcp_server() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handle(Json(body): Json<Value>) -> Json<Value> {
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": "init rejected"},
            }))
        }

        let router = Router::new().route("/mcp", post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/mcp", addr.port()), handle)
    }

    /// A failing apply entered from an error state must transition through
    /// Refreshing (recorded with the "applying new tokens" reason) and still
    /// end ConnectionFailed with the inner init error preserved.
    #[tokio::test]
    async fn apply_tokens_failure_from_error_state_transitions_via_refreshing() {
        let (url, server) = spawn_failing_init_mcp_server().await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        adapter
            .inner
            .transition_to(OAuthState::ConnectionFailed, "test: pin error state")
            .await;

        adapter
            .inner
            .apply_tokens(make_token_set("will-fail"))
            .await;

        assert_eq!(
            adapter.inner.state.read().await.clone(),
            OAuthState::ConnectionFailed,
            "failed apply must still end ConnectionFailed"
        );
        let history = adapter.inner.transition_history.read().await;
        assert!(
            history
                .iter()
                .any(|r| r.from == OAuthState::ConnectionFailed
                    && r.to == OAuthState::Refreshing
                    && r.reason == "applying new tokens"),
            "expected ConnectionFailed → Refreshing 'applying new tokens'; got: {:?}",
            history
                .iter()
                .map(|r| (r.from.clone(), r.to.clone(), r.reason.clone()))
                .collect::<Vec<_>>()
        );
        server.abort();
    }

    /// Refresh paths (proactive/reactive) enter the apply already in
    /// `Refreshing` — the apply must not record a no-op self-transition.
    #[tokio::test]
    async fn apply_tokens_already_refreshing_skips_transition() {
        let (url, server) = spawn_minimal_mcp_server().await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        adapter
            .inner
            .transition_to(OAuthState::Refreshing, "test: refresh in progress")
            .await;

        adapter
            .inner
            .apply_tokens(make_token_set("refreshed"))
            .await;

        assert_eq!(
            adapter.inner.state.read().await.clone(),
            OAuthState::Authenticated
        );
        let history = adapter.inner.transition_history.read().await;
        assert!(
            !history.iter().any(|r| r.reason == "applying new tokens"),
            "apply entered in Refreshing must not record a self-transition; got: {:?}",
            history
                .iter()
                .map(|r| (r.from.clone(), r.to.clone(), r.reason.clone()))
                .collect::<Vec<_>>()
        );
        server.abort();
    }

    /// Spawn an in-process MCP server whose `tools/list` response reflects
    /// the current value of the shared `tool_name` — lets a test change the
    /// upstream tool set between `apply_tokens` calls.
    async fn spawn_mcp_server_with_mutable_tools(
        tool_name: Arc<std::sync::RwLock<String>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::State;
        use axum::{routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handle(
            State(tool_name): State<Arc<std::sync::RwLock<String>>>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            match method {
                "initialize" => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": {"name": "test-server", "version": "0.0.1"},
                    },
                })),
                "tools/list" => {
                    let name = tool_name.read().unwrap().clone();
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": name,
                                "description": "a tool",
                                "inputSchema": {"type": "object"},
                            }],
                        },
                    }))
                }
                _ => Json(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
            }
        }

        let router = Router::new()
            .route("/mcp", post(handle))
            .with_state(tool_name);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/mcp", addr.port()), handle)
    }

    /// PR #140 review follow-up: a Some→Some swap that actually changes the
    /// tool set (e.g. an OAuth callback re-login under a different
    /// account/scope) must emit a tick so the registry's caches don't go
    /// stale, while an unchanged Some→Some swap stays silent.
    #[tokio::test]
    async fn apply_tokens_some_to_some_tool_change_emits_tick() {
        let tool_name = Arc::new(std::sync::RwLock::new("alpha".to_string()));
        let (url, server) = spawn_mcp_server_with_mutable_tools(tool_name.clone()).await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();
        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");

        // None→Some: tick, and the fingerprint baseline is stored.
        adapter.inner.apply_tokens(make_token_set("first")).await;
        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "None→Some must tick"
        );
        drain(&mut outer_rx).await;

        // Some→Some with an unchanged tool set: silent.
        adapter.inner.apply_tokens(make_token_set("second")).await;
        assert!(
            !recv_tick(&mut outer_rx, Duration::from_millis(200)).await,
            "unchanged Some→Some swap must not emit a tick"
        );

        // Some→Some with a changed tool set: tick.
        *tool_name.write().unwrap() = "beta".to_string();
        adapter.inner.apply_tokens(make_token_set("third")).await;
        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "Some→Some swap with a changed tool set must emit a tick"
        );
        server.abort();
    }

    /// PR #140 review follow-up (round 2): a forwarded inner `tools_changed`
    /// tick must invalidate the stored fingerprint. Sequence pinned here:
    /// tool set A is probed (baseline = A), the inner notifies a change (the
    /// registry's caches follow the drift, baseline must be cleared), then a
    /// re-login reproduces A — without invalidation the A==A comparison
    /// would suppress the tick and leave the registry's caches stale.
    #[tokio::test]
    async fn inner_tick_invalidates_fingerprint_so_relogin_to_same_tools_ticks() {
        let tool_name = Arc::new(std::sync::RwLock::new("alpha".to_string()));
        let (url, server) = spawn_mcp_server_with_mutable_tools(tool_name.clone()).await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();
        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");

        // Baseline: apply with tool set A → fingerprint stored.
        adapter.inner.apply_tokens(make_token_set("first")).await;
        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "None→Some must tick"
        );
        assert!(
            adapter.inner.last_tools_fingerprint.read().await.is_some(),
            "baseline fingerprint must be stored after a successful probe"
        );

        // Simulate the inner adapter notifying a tool change (upstream
        // drifted to B): bind the forwarder to a manual channel and tick it.
        let (inner_tx, inner_rx) = broadcast::channel::<()>(16);
        adapter.inner.swap_tools_forwarder(Some(inner_rx)).await;
        inner_tx.send(()).expect("inner send");
        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "forwarded inner tick must reach the outer broadcast"
        );
        assert!(
            adapter.inner.last_tools_fingerprint.read().await.is_none(),
            "forwarded inner tick must clear the fingerprint baseline"
        );
        drain(&mut outer_rx).await;

        // Re-login reproducing the ORIGINAL tool set A: the baseline is
        // unknown, so the Some→Some swap must tick even though the probed
        // set equals the pre-drift baseline.
        adapter.inner.apply_tokens(make_token_set("second")).await;
        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "re-login after an inner tool change must tick even if the \
             probed set matches the stale baseline"
        );
        server.abort();
    }

    /// When `apply_tokens` fails inner init while a previous inner adapter
    /// existed (Some→None: endpoint just lost its tools), a synthetic tick
    /// must fire so the registry drops the stale tools from the catalog.
    #[tokio::test]
    async fn apply_tokens_some_to_none_failure_emits_outer_tick() {
        let (url, server) = spawn_minimal_mcp_server().await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();

        // First apply succeeds: inner becomes Some.
        adapter.inner.apply_tokens(make_token_set("first")).await;
        assert!(adapter.inner.inner_adapter.read().await.is_some());

        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");
        drain(&mut outer_rx).await;

        // Kill the upstream so the next inner init fails: Some→None.
        server.abort();
        tokio::time::sleep(Duration::from_millis(50)).await;
        adapter.inner.apply_tokens(make_token_set("second")).await;
        assert!(adapter.inner.inner_adapter.read().await.is_none());

        assert!(
            recv_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "Some→None inner readiness transition must emit a synthetic outer tick"
        );
    }

    /// Spawn an in-process MCP server that also answers `tools/list` with a
    /// single tool, so a registry-level test can observe the merged catalog
    /// rebuild after `apply_tokens`.
    async fn spawn_mcp_server_with_tools() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handle(Json(body): Json<Value>) -> Json<Value> {
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            match method {
                "initialize" => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": {"name": "test-server", "version": "0.0.1"},
                    },
                })),
                "tools/list" => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": "oauth_tool",
                            "description": "a tool",
                            "inputSchema": {"type": "object"},
                        }],
                    },
                })),
                _ => Json(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
            }
        }

        let router = Router::new().route("/mcp", post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/mcp", addr.port()), handle)
    }

    /// Registry-level regression: after `apply_tokens` makes an OAuth
    /// endpoint listable (None→Some), the registry's tools-changed listener
    /// must bump `catalog_generation` and the merged catalog must rebuild
    /// with the endpoint's tools — no restart required.
    #[tokio::test]
    async fn registry_rebuilds_merged_catalog_after_apply_tokens() {
        use crate::registry::AdapterRegistry;

        let (url, server) = spawn_mcp_server_with_tools().await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();
        let inner = adapter.shared_inner();

        let registry = AdapterRegistry::new();
        registry
            .register(
                "test".into(),
                Box::new(adapter),
                "oauth".into(),
                None,
                Some("test".into()),
            )
            .await;

        // Before tokens the inner adapter is None → empty merged catalog.
        let catalog = registry.merged_catalog().await;
        assert!(
            catalog.is_empty(),
            "expected empty catalog before apply_tokens, got {:?}",
            catalog.iter().map(|t| &t.name).collect::<Vec<_>>()
        );

        let gen_before = registry.catalog_generation();
        inner.apply_tokens(make_token_set("test-access")).await;

        // The synthetic tick reaches the registry listener asynchronously.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while registry.catalog_generation() <= gen_before && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            registry.catalog_generation() > gen_before,
            "catalog generation must bump after apply_tokens readiness change"
        );

        // Merged catalog rebuild now sees the endpoint's tools (single
        // active adapter → no prefix).
        let catalog = registry.merged_catalog().await;
        assert_eq!(
            catalog.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["oauth_tool"],
            "merged catalog must contain the endpoint's tools after apply_tokens"
        );

        server.abort();
    }

    /// Registry-level regression for the PR #140 review: a Some→Some
    /// `apply_tokens` swap that changes the tool set (OAuth callback
    /// re-login under a different account/scope) must reach the registry —
    /// generation bump + merged catalog rebuilt with the new tools.
    #[tokio::test]
    async fn registry_rebuilds_merged_catalog_after_some_to_some_tool_change() {
        use crate::registry::AdapterRegistry;

        let tool_name = Arc::new(std::sync::RwLock::new("alpha".to_string()));
        let (url, server) = spawn_mcp_server_with_mutable_tools(tool_name.clone()).await;
        let mut config = make_config();
        config.url = url;
        let mut adapter = make_adapter(config);
        adapter.initialize().await.unwrap();
        let inner = adapter.shared_inner();

        let registry = AdapterRegistry::new();
        registry
            .register(
                "test".into(),
                Box::new(adapter),
                "oauth".into(),
                None,
                Some("test".into()),
            )
            .await;

        // First apply: None→Some, catalog picks up "alpha".
        let gen_before = inner_apply_and_wait(&registry, &inner, "first").await;
        let catalog = registry.merged_catalog().await;
        assert_eq!(
            catalog.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha"],
            "merged catalog must contain the initial tool set"
        );

        // Re-login changes the upstream tool set; the Some→Some swap must
        // tick so the registry rebuilds with "beta".
        *tool_name.write().unwrap() = "beta".to_string();
        inner.apply_tokens(make_token_set("second")).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while registry.catalog_generation() <= gen_before && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            registry.catalog_generation() > gen_before,
            "catalog generation must bump after a Some→Some tool change"
        );
        let catalog = registry.merged_catalog().await;
        assert_eq!(
            catalog.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["beta"],
            "merged catalog must reflect the changed tool set"
        );

        server.abort();
    }

    /// Apply tokens and wait for the registry's generation to settle past
    /// the tick triggered by the apply; returns the settled generation.
    async fn inner_apply_and_wait(
        registry: &crate::registry::AdapterRegistry,
        inner: &Arc<OAuthAdapterInner>,
        token: &str,
    ) -> u64 {
        let gen_before = registry.catalog_generation();
        inner.apply_tokens(make_token_set(token)).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while registry.catalog_generation() <= gen_before && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        registry.catalog_generation()
    }

    // --- Refresh-time token endpoint discovery fallback tests ---

    /// Shared in/out counters and canned responses for the discovery test
    /// fixture. `None` for any of the metadata fields means "respond 404".
    ///
    /// `new_token_error_body`, when set, overrides `new_token_response` and
    /// causes `/new/token` to respond with HTTP 400 and a raw OAuth error
    /// body (e.g. `{"error":"invalid_client"}`) — used by the round-2 R6
    /// regression that exercises `handle_refresh_404`'s retry path
    /// self-heal.
    #[derive(Clone, Default)]
    struct DiscoveryFixtureOpts {
        new_token_count: Arc<std::sync::atomic::AtomicUsize>,
        old_token_count: Arc<std::sync::atomic::AtomicUsize>,
        pr_count: Arc<std::sync::atomic::AtomicUsize>,
        as_count: Arc<std::sync::atomic::AtomicUsize>,
        pr_metadata: Option<serde_json::Value>,
        as_metadata: Option<serde_json::Value>,
        new_token_response: Option<serde_json::Value>,
        new_token_error_body: Option<String>,
    }

    /// Build a `DiscoveryFixtureOpts` pre-populated with a valid protected-
    /// resource document pointing at the fixture's own host as the
    /// authorization server, and a valid AS metadata document whose
    /// `token_endpoint` is the fixture's `/new/token` URL.
    fn happy_discovery_opts(base: &str) -> DiscoveryFixtureOpts {
        use serde_json::json;
        DiscoveryFixtureOpts {
            pr_metadata: Some(json!({
                "resource": base,
                "authorization_servers": [base],
            })),
            as_metadata: Some(json!({
                "issuer": base,
                "authorization_endpoint": format!("{}/authorize", base),
                "token_endpoint": format!("{}/new/token", base),
                "code_challenge_methods_supported": ["S256"],
            })),
            new_token_response: Some(json!({
                "access_token": "rediscovered-access",
                "token_type": "Bearer",
                "expires_in": 3600u64,
                "refresh_token": "rediscovered-refresh",
            })),
            ..Default::default()
        }
    }

    /// Bind a TCP listener on an ephemeral port without consuming it so we
    /// know the URL the fixture will use before we spawn the server. The
    /// listener is returned and dropped only when the caller hands it to the
    /// fixture's `spawn_on`.
    async fn reserve_port() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, format!("http://127.0.0.1:{}", addr.port()))
    }

    /// Variant of `spawn_discovery_fixture` that takes a pre-bound listener
    /// so the test can build URLs referencing the eventual base URL before
    /// the server starts.
    async fn spawn_discovery_fixture_on(
        listener: tokio::net::TcpListener,
        opts: DiscoveryFixtureOpts,
    ) -> tokio::task::JoinHandle<()> {
        use axum::http::StatusCode;
        use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
        use serde_json::Value;

        #[derive(Clone)]
        struct Fx {
            new_token_count: Arc<std::sync::atomic::AtomicUsize>,
            old_token_count: Arc<std::sync::atomic::AtomicUsize>,
            pr_count: Arc<std::sync::atomic::AtomicUsize>,
            as_count: Arc<std::sync::atomic::AtomicUsize>,
            pr_metadata: Arc<Option<Value>>,
            as_metadata: Arc<Option<Value>>,
            new_token_response: Arc<Option<Value>>,
            new_token_error_body: Arc<Option<String>>,
        }

        async fn old_token(State(fx): State<Fx>) -> impl IntoResponse {
            fx.old_token_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (StatusCode::NOT_FOUND, "not found").into_response()
        }
        async fn new_token(State(fx): State<Fx>) -> impl IntoResponse {
            fx.new_token_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(body) = fx.new_token_error_body.as_ref() {
                return (StatusCode::BAD_REQUEST, body.clone()).into_response();
            }
            match fx.new_token_response.as_ref() {
                Some(v) => (StatusCode::OK, Json(v.clone())).into_response(),
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        }
        async fn pr_meta(State(fx): State<Fx>) -> axum::response::Response {
            fx.pr_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match fx.pr_metadata.as_ref() {
                Some(v) => (StatusCode::OK, Json(v.clone())).into_response(),
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        }
        async fn as_meta(State(fx): State<Fx>) -> axum::response::Response {
            fx.as_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match fx.as_metadata.as_ref() {
                Some(v) => (StatusCode::OK, Json(v.clone())).into_response(),
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        }

        let fx = Fx {
            new_token_count: opts.new_token_count,
            old_token_count: opts.old_token_count,
            pr_count: opts.pr_count,
            as_count: opts.as_count,
            pr_metadata: Arc::new(opts.pr_metadata),
            as_metadata: Arc::new(opts.as_metadata),
            new_token_response: Arc::new(opts.new_token_response),
            new_token_error_body: Arc::new(opts.new_token_error_body),
        };

        let router = Router::new()
            .route("/old/token", post(old_token))
            .route("/new/token", post(new_token))
            .route(
                "/.well-known/oauth-protected-resource",
                axum::routing::get(pr_meta),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                axum::routing::get(as_meta),
            )
            .with_state(fx);

        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle
    }

    /// Happy path: the configured token endpoint returns 404, discovery
    /// succeeds, the rediscovered endpoint differs, and the retry succeeds.
    /// The new tokens are returned and the in-memory override is populated.
    #[tokio::test]
    async fn refresh_discovery_fallback_success() {
        let (listener, base) = reserve_port().await;
        let opts = happy_discovery_opts(&base);
        let server = spawn_discovery_fixture_on(listener, opts).await;

        let mut config = make_config();
        config.url = format!("{}/mcp", base);
        config.token_endpoint_url = format!("{}/old/token", base);
        config.allow_insecure_oauth = true;
        let adapter = make_adapter_with_refresh_token(config).await;

        let token_set = adapter
            .inner
            .do_token_refresh()
            .await
            .expect("refresh should succeed after rediscovery");
        assert_eq!(token_set.access_token, "rediscovered-access");

        let override_url = adapter.inner.token_endpoint_override.read().await.clone();
        assert_eq!(override_url, Some(format!("{}/new/token", base)));

        server.abort();
    }

    /// Discovery itself fails (no protected-resource metadata served).
    /// The original 404 must be surfaced to the caller and the override must
    /// remain unset so we don't poison subsequent refreshes.
    #[tokio::test]
    async fn refresh_discovery_fallback_failure_discovery_fails() {
        let (listener, base) = reserve_port().await;
        let opts = DiscoveryFixtureOpts {
            pr_metadata: None,
            as_metadata: None,
            ..Default::default()
        };
        let server = spawn_discovery_fixture_on(listener, opts).await;

        let mut config = make_config();
        config.url = format!("{}/mcp", base);
        config.token_endpoint_url = format!("{}/old/token", base);
        config.allow_insecure_oauth = true;
        let adapter = make_adapter_with_refresh_token(config).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(
            matches!(result, Err(OAuthError::RefreshFailed { ref body, .. }) if body.contains("not found")),
            "expected original 404 RefreshFailed, got {:?}",
            result
        );
        assert!(adapter.inner.token_endpoint_override.read().await.is_none());

        let state = adapter.inner.state.read().await.clone();
        assert_eq!(state, OAuthState::AuthRequired);

        server.abort();
    }

    /// Discovery succeeds but returns the same token endpoint URL we just
    /// hit. The adapter must NOT retry (that would just 404 again) and must
    /// surface the original 404.
    #[tokio::test]
    async fn refresh_discovery_fallback_failure_no_token_endpoint() {
        use serde_json::json;
        let (listener, base) = reserve_port().await;
        let opts = DiscoveryFixtureOpts {
            pr_metadata: Some(json!({
                "resource": base,
                "authorization_servers": [base.clone()],
            })),
            as_metadata: Some(json!({
                "issuer": base,
                "authorization_endpoint": format!("{}/authorize", base),
                // Discovery returns the SAME URL we just got a 404 from.
                "token_endpoint": format!("{}/old/token", base),
                "code_challenge_methods_supported": ["S256"],
            })),
            ..Default::default()
        };
        let server = spawn_discovery_fixture_on(listener, opts).await;

        let mut config = make_config();
        config.url = format!("{}/mcp", base);
        config.token_endpoint_url = format!("{}/old/token", base);
        config.allow_insecure_oauth = true;
        let adapter = make_adapter_with_refresh_token(config).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(
            matches!(result, Err(OAuthError::RefreshFailed { .. })),
            "expected RefreshFailed, got {:?}",
            result
        );
        assert!(adapter.inner.token_endpoint_override.read().await.is_none());
        server.abort();
    }

    /// Memoization: after a successful rediscovery the override is cached, so
    /// a second refresh posts directly to the new endpoint without re-running
    /// discovery (and without touching the old endpoint).
    #[tokio::test]
    async fn refresh_discovery_fallback_memoized() {
        let (listener, base) = reserve_port().await;
        let opts = happy_discovery_opts(&base);
        let pr_count = opts.pr_count.clone();
        let as_count = opts.as_count.clone();
        let new_token_count = opts.new_token_count.clone();
        let old_token_count = opts.old_token_count.clone();
        let server = spawn_discovery_fixture_on(listener, opts).await;

        let mut config = make_config();
        config.url = format!("{}/mcp", base);
        config.token_endpoint_url = format!("{}/old/token", base);
        config.allow_insecure_oauth = true;
        let adapter = make_adapter_with_refresh_token(config).await;

        // First refresh: discovery runs, override is populated.
        let first = adapter
            .inner
            .do_token_refresh()
            .await
            .expect("first refresh");
        assert_eq!(first.access_token, "rediscovered-access");
        let pr_after_first = pr_count.load(std::sync::atomic::Ordering::SeqCst);
        let as_after_first = as_count.load(std::sync::atomic::Ordering::SeqCst);
        let new_token_after_first = new_token_count.load(std::sync::atomic::Ordering::SeqCst);
        let old_token_after_first = old_token_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(pr_after_first >= 1, "discovery must hit protected-resource");
        assert!(
            as_after_first >= 1,
            "discovery must hit auth-server metadata"
        );
        assert_eq!(
            new_token_after_first, 1,
            "new token endpoint must be POSTed exactly once"
        );
        assert_eq!(
            old_token_after_first, 1,
            "old token endpoint must be POSTed exactly once"
        );

        // Second refresh: must use the cached override; no new discovery hits
        // and no hit on the old endpoint.
        let second = adapter
            .inner
            .do_token_refresh()
            .await
            .expect("second refresh uses cached override");
        assert_eq!(second.access_token, "rediscovered-access");
        assert_eq!(
            pr_count.load(std::sync::atomic::Ordering::SeqCst),
            pr_after_first,
            "second refresh must not re-run protected-resource discovery"
        );
        assert_eq!(
            as_count.load(std::sync::atomic::Ordering::SeqCst),
            as_after_first,
            "second refresh must not re-run auth-server discovery"
        );
        assert_eq!(
            old_token_count.load(std::sync::atomic::Ordering::SeqCst),
            old_token_after_first,
            "second refresh must not POST to the stale old token endpoint"
        );
        assert_eq!(
            new_token_count.load(std::sync::atomic::Ordering::SeqCst),
            new_token_after_first + 1,
            "second refresh must POST to the new token endpoint once"
        );

        server.abort();
    }

    // --- PR #69 audit gap 1 & 5: discovery short-circuits -------------------

    /// Gap 1: when the resource URL is empty, a 404 from the token endpoint
    /// must NOT trigger RFC 9728 / 8414 discovery (discovery would have
    /// nothing to discover against). The original 404 is surfaced.
    #[tokio::test]
    async fn refresh_404_with_empty_config_url_skips_discovery() {
        let (listener, base) = reserve_port().await;
        // Serve PR/AS metadata so we can prove they were NOT requested.
        let opts = happy_discovery_opts(&base);
        let pr_count = opts.pr_count.clone();
        let as_count = opts.as_count.clone();
        let new_token_count = opts.new_token_count.clone();
        let old_token_count = opts.old_token_count.clone();
        let server = spawn_discovery_fixture_on(listener, opts).await;

        let mut config = make_config();
        // `url` empty triggers the early-return branch in `handle_refresh_404`.
        config.url = String::new();
        config.token_endpoint_url = format!("{}/old/token", base);
        config.allow_insecure_oauth = true;
        let adapter = make_adapter_with_refresh_token(config).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(
            matches!(result, Err(OAuthError::RefreshFailed { ref body, .. }) if body.contains("not found")),
            "expected original 404 RefreshFailed, got {:?}",
            result
        );
        assert!(adapter.inner.token_endpoint_override.read().await.is_none());

        let state = adapter.inner.state.read().await.clone();
        assert_eq!(state, OAuthState::AuthRequired);

        assert_eq!(
            old_token_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "old token endpoint must be hit exactly once"
        );
        assert_eq!(
            pr_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "protected-resource discovery must NOT run when config.url is empty"
        );
        assert_eq!(
            as_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "auth-server discovery must NOT run when config.url is empty"
        );
        assert_eq!(
            new_token_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "new token endpoint must NOT be POSTed without discovery"
        );

        server.abort();
    }

    /// Regression for PR #130 round-2 finding R6: when the primary token
    /// endpoint returns 404 and OAuth discovery rediscovers a new endpoint,
    /// but that retried endpoint returns an OAuth error body containing
    /// `invalid_client`, `handle_refresh_404` must run the same self-heal
    /// as the primary refresh path — clearing the requesting DCR pair and
    /// emitting the "client registration invalidated" transition reason
    /// — otherwise a rediscovery-and-retry route with a purged AS
    /// registration would leave the stale DCR record intact and loop.
    #[tokio::test]
    async fn refresh_404_retry_invalid_client_triggers_self_heal() {
        use crate::token_manager::DcrCredentials;

        let (listener, base) = reserve_port().await;
        let mut opts = happy_discovery_opts(&base);
        opts.new_token_error_body = Some(
            "{\"error\":\"invalid_client\",\"error_description\":\"unknown client\"}".to_string(),
        );
        // Clear the happy-path 200 body so we exercise the 400 branch.
        opts.new_token_response = None;
        let new_token_count = opts.new_token_count.clone();
        let old_token_count = opts.old_token_count.clone();
        let server = spawn_discovery_fixture_on(listener, opts).await;

        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        tm.save_dcr(
            "test",
            &DcrCredentials {
                client_id: "test-client".to_string(),
                client_secret: Some("test-secret".to_string()),
                registered_via_dcr: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let mut config = make_config();
        config.url = base.clone();
        config.token_endpoint_url = format!("{}/old/token", base);
        config.client_secret = Some("test-secret".to_string());
        config.allow_insecure_oauth = true;
        let adapter = make_adapter_with_shared_tm(config, tm.clone()).await;

        let result = adapter.inner.do_token_refresh().await;
        assert!(
            matches!(result, Err(OAuthError::RefreshFailed { .. })),
            "expected RefreshFailed after failed retry, got {:?}",
            result
        );

        assert_eq!(
            old_token_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "old token endpoint must be hit exactly once"
        );
        assert_eq!(
            new_token_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "rediscovered token endpoint must be retried exactly once"
        );

        // Self-heal must fire even though the failure surfaced through the
        // rediscovery-and-retry path: the record persists as a stub with
        // the requesting pair cleared but the DCR provenance flag intact.
        let loaded = tm
            .load_dcr("test")
            .await
            .unwrap()
            .expect("post-self-heal stub must persist");
        assert_eq!(loaded.client_id, "");
        assert!(loaded.client_secret.is_none());
        assert!(
            loaded.registered_via_dcr,
            "registered_via_dcr must survive so the next authorize re-registers"
        );

        assert_eq!(
            adapter.inner.state.read().await.clone(),
            OAuthState::AuthRequired,
            "retry-with-invalid_client must still land in AuthRequired"
        );
        let history = adapter.inner.transition_history.read().await;
        assert!(
            history
                .iter()
                .any(|r| r.reason == "client registration invalidated; re-authorize to re-register"),
            "rediscovery/retry self-heal must emit the same distinct transition reason as the primary refresh path, got: {:?}",
            history.iter().map(|r| r.reason.clone()).collect::<Vec<_>>()
        );

        server.abort();
    }

    /// Gap 5: non-404 HTTP errors (5xx, 4xx other than 404) must NOT trigger
    /// the discovery fallback — the original status is surfaced as-is and no
    /// PR/AS metadata requests are issued.
    #[tokio::test]
    async fn refresh_5xx_does_not_trigger_discovery() {
        use axum::http::StatusCode;
        use axum::{response::IntoResponse, routing::get, routing::post, Router};

        // Counters: token POSTs, plus would-be discovery hits.
        let token_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pr_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let as_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        #[derive(Clone)]
        struct Fx {
            token_count: Arc<std::sync::atomic::AtomicUsize>,
            pr_count: Arc<std::sync::atomic::AtomicUsize>,
            as_count: Arc<std::sync::atomic::AtomicUsize>,
        }

        async fn token_500(
            axum::extract::State(fx): axum::extract::State<Fx>,
        ) -> impl IntoResponse {
            fx.token_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (StatusCode::INTERNAL_SERVER_ERROR, "upstream broke")
        }
        async fn pr(axum::extract::State(fx): axum::extract::State<Fx>) -> impl IntoResponse {
            fx.pr_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (StatusCode::NOT_FOUND, "")
        }
        async fn r#as(axum::extract::State(fx): axum::extract::State<Fx>) -> impl IntoResponse {
            fx.as_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (StatusCode::NOT_FOUND, "")
        }

        let fx = Fx {
            token_count: token_count.clone(),
            pr_count: pr_count.clone(),
            as_count: as_count.clone(),
        };
        let router = Router::new()
            .route("/token", post(token_500))
            .route("/.well-known/oauth-protected-resource", get(pr))
            .route("/.well-known/oauth-authorization-server", get(r#as))
            .with_state(fx);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let base = format!("http://127.0.0.1:{}", addr.port());

        let mut config = make_config();
        config.url = format!("{}/mcp", base);
        config.token_endpoint_url = format!("{}/token", base);
        config.allow_insecure_oauth = true;
        let adapter = make_adapter_with_refresh_token(config).await;

        let result = adapter.inner.do_token_refresh().await;
        match result {
            Err(OAuthError::RefreshFailed { status, .. }) => {
                assert_eq!(
                    status,
                    reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    "non-404 status must surface unchanged"
                );
            }
            other => panic!("expected RefreshFailed(500), got {:?}", other),
        }

        assert_eq!(
            token_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "token endpoint must be POSTed exactly once (no retry)"
        );
        assert_eq!(
            pr_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "5xx must NOT trigger protected-resource discovery"
        );
        assert_eq!(
            as_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "5xx must NOT trigger auth-server discovery"
        );
        assert!(adapter.inner.token_endpoint_override.read().await.is_none());

        let state = adapter.inner.state.read().await.clone();
        assert_eq!(state, OAuthState::AuthRequired);

        server.abort();
    }

    /// Regression: a `call_tool` routed through `OAuthAdapter` (which wraps an
    /// inner `HttpAdapter`) must publish a [`ToolCallEvent::Started`] whose
    /// `request_uid` and `profile` fields are populated from the caller's
    /// per-request span scope (`mcp_request{profile}` > `request{request_uid}`).
    ///
    /// Before the fix, `OAuthAdapter::call_tool` wrapped the inner-adapter
    /// invocation in `.instrument(self.inner.span)` — the OAuth endpoint span
    /// has no parent linkage to the per-request spans, so the inner
    /// `HttpAdapter::call_tool`'s `current_request_context()` walk found
    /// neither field and emitted `request_uid: None` / `profile: None` for
    /// every OAuth-authenticated endpoint.
    ///
    /// `#[test]` (not `#[tokio::test]`) because we install the capture layer
    /// via `with_default(...)` and drive an inner current-thread runtime so
    /// the dispatcher stays attached across `tokio::spawn`'d tasks.
    #[test]
    #[serial_test::serial(tracing)]
    fn call_tool_publishes_request_uid_and_profile_from_request_span() {
        crate::test_tracing::init_permissive_tracing();
        use crate::events::{SpanFieldCaptureLayer, ToolCallEvent, ToolCallEventBus};
        use tracing::Instrument;
        use tracing_subscriber::prelude::*;

        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (url, server) = spawn_minimal_mcp_server().await;
                let mut config = make_config();
                config.url = url;
                let adapter = make_adapter(config);

                // Wire the event bus before applying tokens so the rebuilt
                // inner HttpAdapter sees the shared OnceLock immediately.
                let bus = ToolCallEventBus::with_default_capacity();
                adapter.set_event_bus(bus.clone());
                let mut rx = bus.subscribe();

                adapter
                    .inner
                    .apply_tokens(TokenSet {
                        access_token: "test-access".to_string(),
                        refresh_token: None,
                        expires_at: None,
                        token_type: "Bearer".to_string(),
                        scope: None,
                        issued_at: None,
                    })
                    .await;

                let uid_str = "uid-42".to_string();
                let profile_str = "test".to_string();
                let mcp_span = tracing::info_span!(
                    "mcp_request",
                    profile = %profile_str,
                );
                let req_span = tracing::info_span!(parent: &mcp_span, "request", method = "tools/call", request_uid = %uid_str);

                let result =
                    async { adapter.call_tool("ping", serde_json::json!({})).await }
                        .instrument(req_span)
                        .await;
                assert!(result.is_ok(), "expected Ok from minimal MCP server, got {result:?}");

                let started = rx.try_recv().expect("started event must be buffered");
                match started {
                    ToolCallEvent::Started {
                        request_uid,
                        profile,
                        transport,
                        ..
                    } => {
                        assert_eq!(request_uid.as_deref(), Some("uid-42"));
                        assert_eq!(profile.as_deref(), Some("test"));
                        // Inner is HttpAdapter, so transport should be "http".
                        assert_eq!(transport, "http");
                    }
                    other => panic!("expected Started event, got {other:?}"),
                }
                let completed = rx.try_recv().expect("completed event must be buffered");
                match completed {
                    ToolCallEvent::Completed { .. } => {}
                    other => panic!("expected Completed event, got {other:?}"),
                }

                server.abort();
            });
        });
    }

    /// Regression: the inner `HttpAdapter`'s tool-call tracing lines must be
    /// emitted inside the OAuth adapter's persistent `endpoint` span so the
    /// desktop Logs tab (which attributes lines via the `endpoint` span field)
    /// shows them. Before the fix, `build_inner_adapter` constructed the inner
    /// adapter with `tracing::Span::none()`, so its
    /// `.instrument(self.span.clone())` was a no-op and the "Tool call failed"
    /// WARN for an `isError` envelope carried no `endpoint` scope — the Logs
    /// tab dropped it.
    ///
    /// Asserts the WARN's span scope contains EXACTLY ONE `endpoint` span
    /// (endpoint name + `transport="oauth"`), guarding both the missing-span
    /// regression and the duplicated-`endpoint`-field concern.
    ///
    /// `#[test]` (not `#[tokio::test]`) because we install the capture layer
    /// via `with_default(...)` and drive an inner current-thread runtime so
    /// the dispatcher stays attached across `tokio::spawn`'d tasks.
    #[test]
    #[serial_test::serial(tracing)]
    fn call_tool_iserror_warn_carries_oauth_endpoint_span() {
        crate::test_tracing::init_permissive_tracing();
        use std::sync::Mutex as StdMutex;
        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::Context;
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::registry::LookupSpan;
        use tracing_subscriber::Layer;

        #[derive(Default, Clone, Debug)]
        struct EndpointSpanFields {
            endpoint: Option<String>,
            transport: Option<String>,
        }

        /// WARN message → the `endpoint` spans (with fields) in its scope.
        type CapturedWarns = Arc<StdMutex<Vec<(String, Vec<EndpointSpanFields>)>>>;

        struct SpanFieldVisitor<'a>(&'a mut EndpointSpanFields);
        impl Visit for SpanFieldVisitor<'_> {
            fn record_str(&mut self, field: &Field, value: &str) {
                match field.name() {
                    "endpoint" => self.0.endpoint = Some(value.to_string()),
                    "transport" => self.0.transport = Some(value.to_string()),
                    _ => {}
                }
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                let rendered = format!("{value:?}").trim_matches('"').to_string();
                match field.name() {
                    "endpoint" => self.0.endpoint = Some(rendered),
                    "transport" => self.0.transport = Some(rendered),
                    _ => {}
                }
            }
        }

        struct MessageVisitor(String);
        impl Visit for MessageVisitor {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }

        /// Captures every WARN event's message plus the `endpoint` spans
        /// (with their recorded fields) found in its span scope.
        struct WarnScopeCaptureLayer {
            warns: CapturedWarns,
        }

        impl<S> Layer<S> for WarnScopeCaptureLayer
        where
            S: tracing::Subscriber + for<'a> LookupSpan<'a>,
        {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                id: &tracing::span::Id,
                ctx: Context<'_, S>,
            ) {
                if attrs.metadata().name() != "endpoint" {
                    return;
                }
                let mut fields = EndpointSpanFields::default();
                attrs.record(&mut SpanFieldVisitor(&mut fields));
                if let Some(span) = ctx.span(id) {
                    span.extensions_mut().insert(fields);
                }
            }

            fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
                if *event.metadata().level() != tracing::Level::WARN {
                    return;
                }
                let mut msg = MessageVisitor(String::new());
                event.record(&mut msg);
                let mut endpoint_spans = Vec::new();
                if let Some(scope) = ctx.event_scope(event) {
                    for span in scope {
                        if span.name() == "endpoint" {
                            let fields = span
                                .extensions()
                                .get::<EndpointSpanFields>()
                                .cloned()
                                .unwrap_or_default();
                            endpoint_spans.push(fields);
                        }
                    }
                }
                self.warns.lock().unwrap().push((msg.0, endpoint_spans));
            }
        }

        let warns: CapturedWarns = Arc::new(StdMutex::new(Vec::new()));
        let layer = WarnScopeCaptureLayer {
            warns: warns.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (url, server) = spawn_iserror_mcp_server().await;
                let mut config = make_config();
                config.url = url;
                let adapter = make_adapter(config);

                adapter
                    .inner
                    .apply_tokens(TokenSet {
                        access_token: "test-access".to_string(),
                        refresh_token: None,
                        expires_at: None,
                        token_type: "Bearer".to_string(),
                        scope: None,
                        issued_at: None,
                    })
                    .await;

                // Once-guard interplay: the OAuth layer recorded the inner
                // handshake's `server_type` on the shared span; the inner
                // adapter's own guard is pre-flipped so token-swap rebuilds
                // never re-append the field.
                assert!(adapter.inner.server_type_recorded_flag());
                {
                    let guard = adapter.inner.inner_adapter.read().await;
                    let inner = guard.as_ref().expect("inner adapter after apply_tokens");
                    assert!(
                        inner.server_type_recorded_flag(),
                        "inner adapter's server_type guard must be pre-flipped"
                    );
                }

                // The isError envelope is forwarded unchanged (transport Ok).
                let result = adapter.call_tool("boom", serde_json::json!({})).await;
                assert!(result.is_ok(), "envelope must be forwarded, got {result:?}");

                server.abort();
            });
        });

        let warns = warns.lock().unwrap();
        let failed: Vec<_> = warns
            .iter()
            .filter(|(msg, _)| msg == "Tool call failed")
            .collect();
        assert_eq!(
            failed.len(),
            1,
            "expected exactly one 'Tool call failed' WARN, got {warns:?}"
        );
        let (_, endpoint_spans) = failed[0];
        assert_eq!(
            endpoint_spans.len(),
            1,
            "WARN must be scoped to exactly one `endpoint` span (no duplicates), got {endpoint_spans:?}"
        );
        assert_eq!(endpoint_spans[0].endpoint.as_deref(), Some("test"));
        assert_eq!(endpoint_spans[0].transport.as_deref(), Some("oauth"));
    }

    /// Like [`spawn_minimal_mcp_server`], but `tools/call` returns a
    /// tool-level error envelope (`isError: true`) so the adapter's WARN
    /// "Tool call failed" tracing path fires on a transport-level `Ok`.
    async fn spawn_iserror_mcp_server() -> (String, tokio::task::JoinHandle<()>) {
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
            } else if method == "tools/call" {
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": "invalid_grant"}],
                        "isError": true,
                    },
                }))
            } else {
                Json(json!({"jsonrpc": "2.0", "id": id, "result": {}}))
            }
        }

        let router = Router::new().route("/mcp", post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/mcp", addr.port()), handle)
    }

    /// Repeated `apply_tokens` calls (token refresh, reconnect) must NOT
    /// re-append `server_type` to the per-endpoint span's field list. The
    /// once-guard flips on the first record and short-circuits every later
    /// call, preventing unbounded `endpoint{…}` log-header growth.
    #[test]
    fn record_server_type_once_guards_repeated_calls() {
        let adapter = make_adapter(make_config());
        let inner = adapter.shared_inner();
        assert!(!inner.server_type_recorded_flag());

        inner.record_server_type_once("some-server");
        assert!(inner.server_type_recorded_flag());

        // Second and subsequent calls (e.g. after a token refresh rebuilds
        // the inner adapter) must NOT re-record — the guard has already
        // flipped, so this is a no-op.
        inner.record_server_type_once("some-server");
        inner.record_server_type_once("other-name");
        assert!(inner.server_type_recorded_flag());
    }
}
