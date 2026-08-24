pub mod client;
pub mod dcr;
pub mod discovery;
pub mod idp_providers;
// EMA grant clients (END-18). Not yet wired into the binary's adapter/config in
// this slice, so the binary crate (private `mod oauth;`) sees it as dead; the
// lib crate exposes it as public API and the unit tests exercise it.
#[allow(dead_code)]
pub mod ema;
pub mod url_guard;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
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

    #[error("EMA token exchange failed: {0}")]
    Ema(String),

    #[error("Refresh abandoned for endpoint '{endpoint}': the grant it was started for was discarded or replaced")]
    StaleGrant { endpoint: String },
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

/// True when an authorize URL targets Google's authorization server
/// (`accounts.google.com`), which requires `access_type=offline` for a
/// refresh token to be issued. Host comparison only — a lookalike host like
/// `accounts.google.com.evil.test` does not match.
pub fn is_google_authorization_endpoint(authorization_endpoint: &str) -> bool {
    url::Url::parse(authorization_endpoint)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.eq_ignore_ascii_case("accounts.google.com"))
        })
        .unwrap_or(false)
}

/// Append Google-specific authorization parameters to a composed authorize
/// URL. Google's authorization server only issues a refresh token when
/// `access_type=offline` is requested — without it every grant is
/// access-token-only and proactive refresh is impossible. Other providers
/// ignore the unknown parameter, but it is scoped to Google to avoid noise.
/// Shared by every authorize-URL builder (management start/setup, JIT, EMA
/// IdP SSO, org SSO) so no path hands out a Google grant without it. The URL
/// must already carry at least one query parameter (`&` separator).
pub fn append_google_authorize_params(authorize_url: &mut String, authorization_endpoint: &str) {
    if is_google_authorization_endpoint(authorization_endpoint) {
        authorize_url.push_str("&access_type=offline");
    }
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
    /// Expected authorization server issuer (RFC 8414 `issuer`), when known.
    /// When `Some(_)`, the callback enforces RFC 9207 `iss` validation; when
    /// `None` (e.g. legacy convention-based config without discovery), the
    /// `iss` check is skipped to preserve existing behavior.
    pub issuer: Option<String>,
    /// RFC 9207: whether the authorization server advertised support for the
    /// authorization-response `iss` parameter. When `true`, a missing `iss` on
    /// the callback is rejected; when `false`, a missing `iss` is tolerated.
    pub iss_parameter_supported: bool,
    /// EMA (END-18) Step-1 marker. When `Some(idp_issuer)`, this
    /// authorization-code flow is an IdP SSO for an EMA endpoint, and the
    /// `/oauth/callback` handler captures the returned `id_token` (plus the IdP
    /// refresh token and ID-Token expiry) and persists it as `IdpCredentials`
    /// keyed by this issuer. `None` for ordinary resource OAuth flows, which are
    /// left completely unaffected.
    pub idp_issuer: Option<String>,
    /// Credential-pool key the captured `IdpCredentials` are persisted under
    /// (Wave 2). For an END-19 org-referencing EMA endpoint this is the org
    /// name, so every endpoint in the org shares one ID token; for a bare
    /// END-18 `idp` endpoint it is the issuer URL (back-compat). `None` for
    /// ordinary resource OAuth flows. When `None` on an EMA flow the callback
    /// falls back to `idp_issuer` as the key.
    pub idp_credential_key: Option<String>,
    pub created_at: Instant,
    /// Per-endpoint reset generation this flow was started under. A
    /// "Reset authorization" bumps the endpoint's generation (see
    /// [`OAuthFlowManager::invalidate_endpoint`]); the `/oauth/callback`
    /// commit path refuses flows from an older generation so a pre-reset
    /// callback — even one already past `consume_flow` and mid token
    /// exchange — cannot clobber the reset.
    pub generation: u64,
}

/// In-memory map holding pending OAuth flows. One entry per in-progress login.
/// Entries are created by `/oauth/start` and consumed by `/oauth/callback`.
pub struct OAuthFlowManager {
    pending: RwLock<HashMap<String, PendingFlow>>,
    /// Per-endpoint reset generation counters (absent entry == generation 0).
    /// Bumped by [`invalidate_endpoint`](Self::invalidate_endpoint).
    generations: RwLock<HashMap<String, u64>>,
    /// Per-endpoint commit locks serializing a callback's token commit
    /// (post-exchange generation check + token save + adapter apply)
    /// against a "Reset authorization" (generation bump + disconnect).
    /// Values are `Weak` so an entry lives only while a caller still holds
    /// the `Arc` — dead entries are pruned on each acquisition, keeping the
    /// map bounded even though setup callbacks use unique `setup:{uuid}`
    /// keys. See [`commit_lock`](Self::commit_lock).
    commit_locks: tokio::sync::Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
}

impl OAuthFlowManager {
    pub fn new() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            generations: RwLock::new(HashMap::new()),
            commit_locks: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Register a new pending flow. Returns the generated state parameter.
    ///
    /// `token_endpoint` should be the fully resolved token endpoint URL
    /// (from discovery or built from config convention).
    #[allow(clippy::too_many_arguments)]
    pub async fn start_flow(
        &self,
        endpoint_name: &str,
        token_endpoint: &str,
        client_id: &str,
        client_secret: Option<&str>,
        pkce: PkceChallenge,
        redirect_uri: &str,
        issuer: Option<&str>,
        iss_parameter_supported: bool,
    ) -> String {
        let state = generate_state();
        // Hold the generations read lock through the pending insert so
        // `invalidate_endpoint` (generations write → pending write) cannot
        // bump the generation between sampling and insertion — that would
        // hand the caller an authorize URL whose callback is guaranteed to
        // be rejected as stale. Same lock order as `invalidate_endpoint`.
        let generations = self.generations.read().await;
        let flow = PendingFlow {
            endpoint_name: endpoint_name.to_string(),
            code_verifier: pkce.code_verifier,
            token_endpoint: token_endpoint.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.map(|s| s.to_string()),
            redirect_uri: redirect_uri.to_string(),
            issuer: issuer.map(|s| s.to_string()),
            iss_parameter_supported,
            idp_issuer: None,
            idp_credential_key: None,
            created_at: Instant::now(),
            generation: *generations.get(endpoint_name).unwrap_or(&0),
        };
        self.pending.write().await.insert(state.clone(), flow);
        drop(generations);
        state
    }

    /// Like [`start_flow`](Self::start_flow), but refuses the insert when the
    /// endpoint's reset generation no longer equals `expected_generation`
    /// (sampled by the caller via [`generation`](Self::generation) at
    /// auth-start entry, BEFORE any network-bound discovery/DCR work).
    ///
    /// This closes the discovery-phase race: a `/oauth/start` that began
    /// before a reset has no pending flow yet, so `invalidate_endpoint`
    /// cannot remove it — without this check it would later insert a flow
    /// stamped with the already-bumped generation and hand out an authorize
    /// URL lacking the reset's `prompt=consent`, silently reusing the old
    /// provider grant. Returns `None` when superseded; the caller must fail
    /// the start rather than hand out a pre-reset URL.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_flow_if_current(
        &self,
        expected_generation: u64,
        endpoint_name: &str,
        token_endpoint: &str,
        client_id: &str,
        client_secret: Option<&str>,
        pkce: PkceChallenge,
        redirect_uri: &str,
        issuer: Option<&str>,
        iss_parameter_supported: bool,
    ) -> Option<String> {
        let state = generate_state();
        // Same lock discipline as `start_flow`: hold the generations read
        // lock through the pending insert so the check and the insert are
        // atomic with respect to `invalidate_endpoint`.
        let generations = self.generations.read().await;
        if *generations.get(endpoint_name).unwrap_or(&0) != expected_generation {
            return None;
        }
        let flow = PendingFlow {
            endpoint_name: endpoint_name.to_string(),
            code_verifier: pkce.code_verifier,
            token_endpoint: token_endpoint.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.map(|s| s.to_string()),
            redirect_uri: redirect_uri.to_string(),
            issuer: issuer.map(|s| s.to_string()),
            iss_parameter_supported,
            idp_issuer: None,
            idp_credential_key: None,
            created_at: Instant::now(),
            generation: expected_generation,
        };
        self.pending.write().await.insert(state.clone(), flow);
        drop(generations);
        Some(state)
    }

    /// Register a pending **EMA IdP SSO** flow (END-18 Step 1). Behaves exactly
    /// like [`start_flow`] but tags the pending flow with the IdP `idp_issuer`
    /// so the `/oauth/callback` handler captures the returned `id_token` (plus
    /// the IdP refresh token and ID-Token expiry) and persists it as
    /// `IdpCredentials`. Callers request the `openid offline_access` scope (M1)
    /// when composing the authorize URL so the IdP returns a refresh token.
    ///
    /// `idp_credential_key` is the credential-pool key the callback persists the
    /// captured `IdpCredentials` under (Wave 2): the org name for an END-19
    /// org-referencing endpoint (so the whole org shares one ID token), or the
    /// issuer URL for a bare END-18 `idp` endpoint. `idp_issuer` always remains
    /// the real issuer URL stored inside the credentials.
    ///
    /// Consumed by the EMA OAuth adapter (END-18 T6) when it composes the IdP
    /// authorize URL for an endpoint that needs (re-)SSO.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_idp_flow(
        &self,
        endpoint_name: &str,
        token_endpoint: &str,
        client_id: &str,
        client_secret: Option<&str>,
        pkce: PkceChallenge,
        redirect_uri: &str,
        issuer: Option<&str>,
        iss_parameter_supported: bool,
        idp_issuer: &str,
        idp_credential_key: &str,
    ) -> String {
        let state = generate_state();
        // Generations read lock held through the insert — see `start_flow`.
        let generations = self.generations.read().await;
        let flow = PendingFlow {
            endpoint_name: endpoint_name.to_string(),
            code_verifier: pkce.code_verifier,
            token_endpoint: token_endpoint.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.map(|s| s.to_string()),
            redirect_uri: redirect_uri.to_string(),
            issuer: issuer.map(|s| s.to_string()),
            iss_parameter_supported,
            idp_issuer: Some(idp_issuer.to_string()),
            idp_credential_key: Some(idp_credential_key.to_string()),
            created_at: Instant::now(),
            generation: *generations.get(endpoint_name).unwrap_or(&0),
        };
        self.pending.write().await.insert(state.clone(), flow);
        drop(generations);
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

    /// The endpoint's current reset generation (0 until first invalidation).
    /// Sample this at auth-start entry (before discovery/DCR network work)
    /// and pass it to [`start_flow_if_current`](Self::start_flow_if_current)
    /// so a reset landing mid-discovery supersedes the start.
    pub async fn generation(&self, endpoint_name: &str) -> u64 {
        self.current_generation(endpoint_name).await
    }

    async fn current_generation(&self, endpoint_name: &str) -> u64 {
        *self
            .generations
            .read()
            .await
            .get(endpoint_name)
            .unwrap_or(&0)
    }

    /// Invalidate every pending flow for `endpoint_name` and bump its reset
    /// generation, so flows started before this call can neither be consumed
    /// (removed here) nor committed if already consumed and mid token
    /// exchange (their `generation` is now stale — see [`is_current`]).
    /// Called by "Reset authorization" before it starts the replacement flow.
    /// Returns the number of pending flows removed.
    ///
    /// [`is_current`]: Self::is_current
    pub async fn invalidate_endpoint(&self, endpoint_name: &str) -> usize {
        // Hold both locks across the bump+removal so a concurrent
        // `start_flow` cannot interleave a flow stamped with the old
        // generation after removal.
        let mut generations = self.generations.write().await;
        let mut pending = self.pending.write().await;
        *generations.entry(endpoint_name.to_string()).or_insert(0) += 1;
        let before = pending.len();
        pending.retain(|_, f| f.endpoint_name != endpoint_name);
        before - pending.len()
    }

    /// Whether `flow` was started under the endpoint's current reset
    /// generation. `/oauth/callback` checks this before committing tokens so
    /// a callback from a pre-reset flow cannot clobber the reset.
    pub async fn is_current(&self, flow: &PendingFlow) -> bool {
        flow.generation == self.current_generation(&flow.endpoint_name).await
    }

    /// The endpoint's commit lock. `/oauth/callback` holds it across its
    /// post-exchange generation check AND the token save / adapter apply;
    /// "Reset authorization" holds it across the generation bump +
    /// disconnect. This closes the time-of-check/time-of-use window where a
    /// reset lands after the callback's generation check but before its
    /// commit finishes — with the lock, either the commit completes first
    /// (and the reset then wipes the freshly saved tokens) or the reset
    /// completes first (and the callback's check sees the stale generation).
    ///
    /// The map stores `Weak` references and prunes dead entries on every
    /// acquisition, so keys whose lock is no longer held anywhere (e.g. the
    /// unique `setup:{uuid}` names from completed setup callbacks) do not
    /// accumulate for the process lifetime. Concurrent callers for the same
    /// key always get the same mutex: the upgrade-or-replace happens under
    /// the outer map lock.
    pub async fn commit_lock(&self, endpoint_name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.commit_locks.lock().await;
        locks.retain(|_, weak| weak.strong_count() > 0);
        match locks.get(endpoint_name).and_then(std::sync::Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(endpoint_name.to_string(), Arc::downgrade(&lock));
                lock
            }
        }
    }

    /// Test-only: number of live entries in the commit-lock map.
    #[cfg(test)]
    pub(crate) async fn commit_lock_count(&self) -> usize {
        self.commit_locks.lock().await.len()
    }

    /// Test-only: re-insert a flow under a chosen state key, preserving its
    /// recorded `generation` (used to exercise the stale-generation guard).
    #[cfg(test)]
    pub(crate) async fn reinsert_flow_for_test(&self, state: &str, flow: PendingFlow) {
        self.pending.write().await.insert(state.to_string(), flow);
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
    /// Claimed by an in-flight `POST /commit`: the session can be neither
    /// cancelled nor committed again until the commit finishes (a failed
    /// commit reverts to [`Authorized`](Self::Authorized) via
    /// [`OAuthSetupManager::release_commit_claim`]).
    Committing,
}

/// Outcome of [`OAuthSetupManager::claim_for_commit`].
pub enum CommitClaim {
    /// The session was `Authorized`; it is now `Committing` and this claim
    /// carries a snapshot of it.
    Claimed(Box<OAuthSetupSession>),
    /// Another commit request already claimed the session.
    AlreadyCommitting,
    /// The session exists but authorization has not completed.
    NotAuthorized,
    /// No live session with this ID.
    NotFound,
}

/// Outcome of [`OAuthSetupManager::cancel_session`].
pub enum CancelOutcome {
    /// The session was removed.
    Cancelled,
    /// The session is claimed by an in-flight commit and was NOT removed.
    CommitInProgress,
    /// No live session with this ID.
    NotFound,
}

/// A transient OAuth setup session that does NOT write to config.toml until
/// explicitly committed. Created by `POST /api/oauth/setup`.
#[derive(Clone)]
pub struct OAuthSetupSession {
    /// Display name for the endpoint.
    pub name: String,
    /// MCP server URL.
    pub url: String,
    /// Requested scopes (space-separated string).
    pub scopes: Option<String>,
    /// Tool prefix override (None = auto-derive from name).
    pub tool_prefix: Option<String>,
    /// Optional override for the advertised server type. When `Some(_)`, this
    /// value is sanitized and used in place of the upstream-derived server
    /// name in `server_type()` and is persisted to `config.toml` on commit.
    pub server_type_override: Option<String>,
    /// Discovered authorization endpoint.
    pub authorization_endpoint: Option<String>,
    /// Discovered token endpoint.
    pub token_endpoint: Option<String>,
    /// Discovered registration endpoint (if DCR is available).
    pub registration_endpoint: Option<String>,
    /// OAuth server base URL (if configured or discovered).
    pub oauth_server_url: Option<String>,
    /// Discovered authorization server issuer (RFC 8414 `issuer`), used for
    /// RFC 9207 `iss` validation on the callback. `None` when not discovered.
    pub issuer: Option<String>,
    /// Client ID (from DCR or manual input).
    pub client_id: Option<String>,
    /// Client secret (optional).
    pub client_secret: Option<String>,
    /// `client_secret_expires_at` from the registration response (RFC 7591;
    /// 0 = never expires). Written by the commit-time defensive save so a
    /// recovered record carries the provider's real expiry instead of
    /// silently becoming non-expiring. 0 for manual credentials.
    pub client_secret_expires_at: u64,
    /// Whether the session's client credentials were minted via true DCR
    /// (RFC 7591) during this setup. Persisted as the DCR record's
    /// `registered_via_dcr` when the commit has to defensively save the
    /// credentials, so recovered records keep their re-registration
    /// self-heal eligibility. `false` for manual/CIMD credentials.
    pub registered_via_dcr: bool,
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

    /// Create a new setup session. Returns the session ID, or `None` when
    /// another live session already holds `name`.
    ///
    /// The name reservation is checked and taken atomically under the
    /// sessions write lock, so two concurrent same-name setup requests can
    /// never both proceed — without it, the loser's setup-time DCR save
    /// could overwrite the winner's validated credentials between commit's
    /// store check and its config write.
    pub async fn create_session(
        &self,
        name: String,
        url: String,
        scopes: Option<String>,
        tool_prefix: Option<String>,
        server_type_override: Option<String>,
    ) -> Option<Uuid> {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, s| s.created_at.elapsed() < SETUP_SESSION_MAX_AGE);
        if sessions.values().any(|s| s.name == name) {
            return None;
        }
        let id = Uuid::new_v4();
        let session = OAuthSetupSession {
            name,
            url,
            scopes,
            tool_prefix,
            server_type_override,
            authorization_endpoint: None,
            token_endpoint: None,
            registration_endpoint: None,
            oauth_server_url: None,
            issuer: None,
            client_id: None,
            client_secret: None,
            client_secret_expires_at: 0,
            registered_via_dcr: false,
            tokens: None,
            status: SetupSessionStatus::AwaitingCredentials,
            created_at: Instant::now(),
        };
        sessions.insert(id, session);
        Some(id)
    }

    /// Atomically claim a session for an exclusive commit attempt. Under the
    /// sessions write lock, an `Authorized` session transitions to
    /// [`SetupSessionStatus::Committing`] and a snapshot is returned; any
    /// other state is reported without mutating the session. While claimed,
    /// [`cancel_session`](Self::cancel_session) refuses to remove the session
    /// and a duplicate commit request gets
    /// [`CommitClaim::AlreadyCommitting`], so exactly one commit can consume
    /// the session. A failed commit must revert the claim via
    /// [`release_commit_claim`](Self::release_commit_claim).
    ///
    /// Claiming refreshes the session's expiry lease (`created_at`), so the
    /// age sweeps in [`create_session`](Self::create_session) /
    /// [`cleanup_stale`](Self::cleanup_stale) cannot drop an actively
    /// committing session — and release its name reservation — mid-write. An
    /// abandoned claim (commit task died without releasing) still expires,
    /// one full max-age after the claim instead of after creation.
    pub async fn claim_for_commit(&self, id: &Uuid) -> CommitClaim {
        let mut sessions = self.sessions.write().await;
        let Some(session) = sessions.get_mut(id) else {
            return CommitClaim::NotFound;
        };
        if session.created_at.elapsed() > SETUP_SESSION_MAX_AGE {
            sessions.remove(id);
            return CommitClaim::NotFound;
        }
        match session.status {
            SetupSessionStatus::Committing => CommitClaim::AlreadyCommitting,
            SetupSessionStatus::Authorized => {
                session.status = SetupSessionStatus::Committing;
                session.created_at = Instant::now();
                let mut snapshot = session.clone();
                // The snapshot reflects the pre-claim state so a caller
                // persisting it never leaks the transient claim marker.
                snapshot.status = SetupSessionStatus::Authorized;
                CommitClaim::Claimed(Box::new(snapshot))
            }
            _ => CommitClaim::NotAuthorized,
        }
    }

    /// Revert a commit claim after a failed commit: a `Committing` session
    /// returns to `Authorized` so it can be retried or cancelled. No-op when
    /// the session is gone or not claimed.
    pub async fn release_commit_claim(&self, id: &Uuid) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(id) {
            if session.status == SetupSessionStatus::Committing {
                session.status = SetupSessionStatus::Authorized;
            }
        }
    }

    /// Cancel (remove) a session unless an in-flight commit has claimed it.
    /// The check-and-remove runs atomically under the sessions write lock,
    /// so a DELETE racing a commit can never yank the session out from
    /// under the commit's DCR/config writes after the claim succeeded.
    ///
    /// Expiry is checked first: an expired session — including an abandoned
    /// `Committing` claim whose lease has lapsed — is removed and reported
    /// `NotFound` ("no live session"), so a dead claim cannot answer DELETE
    /// with `CommitInProgress` forever while nothing else sweeps it.
    pub async fn cancel_session(&self, id: &Uuid) -> CancelOutcome {
        let mut sessions = self.sessions.write().await;
        match sessions.get(id) {
            None => CancelOutcome::NotFound,
            Some(s) if s.created_at.elapsed() > SETUP_SESSION_MAX_AGE => {
                sessions.remove(id);
                CancelOutcome::NotFound
            }
            Some(s) if s.status == SetupSessionStatus::Committing => {
                CancelOutcome::CommitInProgress
            }
            Some(_) => {
                sessions.remove(id);
                CancelOutcome::Cancelled
            }
        }
    }

    /// Get mutable access to a session by ID. Returns `None` for a session
    /// claimed by an in-flight commit ([`SetupSessionStatus::Committing`]):
    /// a claimed session's state is frozen until the claim is released, so a
    /// late credentials submission cannot flip it back to `AwaitingAuth` and
    /// reopen the cancel/duplicate-commit races the claim closed.
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
        if session.status == SetupSessionStatus::Committing {
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

    /// Remove a session (after commit, or setup-flow cleanup).
    pub async fn remove_session(&self, id: &Uuid) -> Option<OAuthSetupSession> {
        self.sessions.write().await.remove(id)
    }

    /// Mark a session as authorized with the obtained tokens.
    /// Called from the OAuth callback handler. Returns `false` for a session
    /// claimed by an in-flight commit: a late/duplicate callback must not
    /// overwrite the tokens being committed or flip the status out of
    /// `Committing` (which would let a cancel or second commit consume it).
    pub async fn mark_authorized(&self, id: &Uuid, tokens: TokenSet) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(id) {
            if session.status == SetupSessionStatus::Committing {
                return false;
            }
            session.tokens = Some(tokens);
            session.status = SetupSessionStatus::Authorized;
            true
        } else {
            false
        }
    }

    /// Whether a live (unexpired) setup session currently reserves `name`.
    /// The endpoint create/rename APIs consult this so a name mid-setup
    /// cannot be taken by a regular endpoint mutation and later collide
    /// with the session's own commit.
    pub async fn is_name_reserved(&self, name: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .any(|s| s.name == name && s.created_at.elapsed() < SETUP_SESSION_MAX_AGE)
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
    fn append_google_authorize_params_scoped_to_google_host() {
        // Google host: access_type=offline appended (case-insensitive host).
        let mut url = "https://accounts.google.com/o/oauth2/v2/auth?response_type=code".to_string();
        append_google_authorize_params(&mut url, "https://accounts.google.com/o/oauth2/v2/auth");
        assert!(url.ends_with("&access_type=offline"));

        let mut url = "https://ACCOUNTS.GOOGLE.COM/auth?response_type=code".to_string();
        append_google_authorize_params(&mut url, "https://ACCOUNTS.GOOGLE.COM/auth");
        assert!(url.ends_with("&access_type=offline"));

        // Non-Google and lookalike hosts: untouched.
        for endpoint in [
            "https://auth.example.com/authorize",
            "https://accounts.google.com.evil.test/auth",
            "not a url",
        ] {
            let mut url = format!("{}?response_type=code", endpoint);
            let before = url.clone();
            append_google_authorize_params(&mut url, endpoint);
            assert_eq!(
                url, before,
                "non-Google endpoint must be untouched: {}",
                endpoint
            );
        }
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
                Some("https://auth.example.com"),
                true,
            )
            .await;

        let flow = mgr.consume_flow(&state).await.unwrap();
        assert_eq!(flow.endpoint_name, "test-ep");
        assert_eq!(flow.code_verifier, verifier);
        assert_eq!(flow.token_endpoint, "https://auth.example.com/token");
        assert_eq!(flow.client_id, "client123");
        assert_eq!(flow.client_secret.as_deref(), Some("secret"));
        assert_eq!(flow.issuer.as_deref(), Some("https://auth.example.com"));
        assert!(flow.iss_parameter_supported);
    }

    #[tokio::test]
    async fn flow_manager_stores_none_issuer() {
        // Legacy/convention-based flows pass `None` for issuer; the stored flow
        // must reflect that so the callback skips RFC 9207 `iss` validation.
        let mgr = OAuthFlowManager::new();
        let pkce = PkceChallenge::generate();
        let state = mgr
            .start_flow(
                "legacy-ep",
                "https://auth.example.com/token",
                "cid",
                None,
                pkce,
                "http://127.0.0.1:9400/oauth/callback",
                None,
                false,
            )
            .await;
        let flow = mgr.consume_flow(&state).await.unwrap();
        assert!(flow.issuer.is_none());
        assert!(!flow.iss_parameter_supported);
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
                None,
                false,
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
                None,
                false,
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

    async fn start_test_flow(mgr: &OAuthFlowManager, endpoint: &str) -> String {
        mgr.start_flow(
            endpoint,
            "https://auth.example.com/token",
            "cid",
            None,
            PkceChallenge::generate(),
            "http://localhost/cb",
            None,
            false,
        )
        .await
    }

    #[tokio::test]
    async fn invalidate_endpoint_removes_only_that_endpoints_flows() {
        let mgr = OAuthFlowManager::new();
        let state_a = start_test_flow(&mgr, "ep-a").await;
        let state_a2 = start_test_flow(&mgr, "ep-a").await;
        let state_b = start_test_flow(&mgr, "ep-b").await;

        let removed = mgr.invalidate_endpoint("ep-a").await;
        assert_eq!(removed, 2);

        // ep-a flows are gone; ep-b's flow is untouched and still current.
        assert!(mgr.consume_flow(&state_a).await.is_none());
        assert!(mgr.consume_flow(&state_a2).await.is_none());
        let flow_b = mgr.consume_flow(&state_b).await.unwrap();
        assert!(mgr.is_current(&flow_b).await);
    }

    #[tokio::test]
    async fn invalidate_endpoint_marks_consumed_flow_stale() {
        // A callback consumed BEFORE the reset (mid token exchange) must be
        // detectable as stale afterwards: is_current flips to false once the
        // endpoint's generation is bumped.
        let mgr = OAuthFlowManager::new();
        let state = start_test_flow(&mgr, "ep").await;
        let flow = mgr.consume_flow(&state).await.unwrap();
        assert!(mgr.is_current(&flow).await);

        mgr.invalidate_endpoint("ep").await;
        assert!(!mgr.is_current(&flow).await);

        // A flow started AFTER the reset carries the new generation.
        let state2 = start_test_flow(&mgr, "ep").await;
        let flow2 = mgr.consume_flow(&state2).await.unwrap();
        assert!(mgr.is_current(&flow2).await);
    }

    #[tokio::test]
    async fn start_flow_if_current_rejects_when_generation_bumped() {
        // The discovery-phase race: an auth-start samples the generation at
        // entry, a reset lands mid-discovery, and the deferred flow insert
        // must be refused — otherwise the start would register a flow under
        // the NEW generation whose authorize URL lacks prompt=consent.
        let mgr = OAuthFlowManager::new();
        let g = mgr.generation("ep").await;
        mgr.invalidate_endpoint("ep").await;

        let pkce = PkceChallenge::generate();
        let refused = mgr
            .start_flow_if_current(
                g,
                "ep",
                "https://auth.example.com/token",
                "cid",
                None,
                pkce,
                "http://localhost/cb",
                None,
                false,
            )
            .await;
        assert!(refused.is_none(), "pre-reset start must be superseded");

        // Sampling after the bump succeeds and the flow is current.
        let g2 = mgr.generation("ep").await;
        let pkce = PkceChallenge::generate();
        let state = mgr
            .start_flow_if_current(
                g2,
                "ep",
                "https://auth.example.com/token",
                "cid",
                None,
                pkce,
                "http://localhost/cb",
                None,
                false,
            )
            .await
            .expect("start sampled after the reset must succeed");
        let flow = mgr.consume_flow(&state).await.unwrap();
        assert!(mgr.is_current(&flow).await);
    }

    #[tokio::test]
    async fn commit_lock_map_prunes_released_entries() {
        // Unique setup:{uuid} keys must not accumulate forever: once no
        // caller holds a lock's Arc, the entry is pruned on the next
        // acquisition.
        let mgr = OAuthFlowManager::new();
        for i in 0..100 {
            let lock = mgr.commit_lock(&format!("setup:{i}")).await;
            let _guard = lock.lock().await;
        }
        // All 100 Arcs were dropped; acquiring a new key prunes them.
        let _keep = mgr.commit_lock("ep").await;
        assert_eq!(mgr.commit_lock_count().await, 1);

        // Same key while held returns the SAME mutex (not a replacement).
        let again = mgr.commit_lock("ep").await;
        assert!(Arc::ptr_eq(&_keep, &again));
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
                    None,
                    false,
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
                None,
            )
            .await
            .unwrap();

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
    async fn setup_manager_rejects_duplicate_live_name() {
        let mgr = OAuthSetupManager::new();
        let first = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await;
        assert!(first.is_some());

        // Same name while the first session is live → reservation rejects.
        let second = mgr
            .create_session("ep".into(), "https://y.com".into(), None, None, None)
            .await;
        assert!(second.is_none());

        // A different name is unaffected.
        assert!(mgr
            .create_session("other".into(), "https://y.com".into(), None, None, None)
            .await
            .is_some());

        // Removing the first session frees the name.
        mgr.remove_session(&first.unwrap()).await;
        assert!(mgr
            .create_session("ep".into(), "https://y.com".into(), None, None, None)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn setup_manager_expired_session_does_not_hold_name() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

        // Manually expire the session
        {
            let mut sessions = mgr.sessions.write().await;
            if let Some(s) = sessions.get_mut(&id) {
                s.created_at = Instant::now() - Duration::from_secs(700);
            }
        }

        // The expired session no longer reserves the name.
        assert!(mgr
            .create_session("ep".into(), "https://y.com".into(), None, None, None)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn setup_manager_remove_session() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

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
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

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
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

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
            .create_session("fresh".into(), "https://a.com".into(), None, None, None)
            .await
            .unwrap();
        let stale_id = mgr
            .create_session("stale".into(), "https://b.com".into(), None, None, None)
            .await
            .unwrap();

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
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

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

    /// Exactly one commit can claim an `Authorized` session; while claimed,
    /// a duplicate commit and a cancel are both refused, and releasing the
    /// claim restores `Authorized` so retry/cancel work again.
    #[tokio::test]
    async fn setup_manager_commit_claim_locks_out_cancel_and_duplicate() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

        // Not yet authorized → claim refused.
        assert!(matches!(
            mgr.claim_for_commit(&id).await,
            CommitClaim::NotAuthorized
        ));

        mgr.get_session_mut(&id, |s| s.status = SetupSessionStatus::Authorized)
            .await;

        // First claim wins and the snapshot carries the pre-claim status.
        let CommitClaim::Claimed(snapshot) = mgr.claim_for_commit(&id).await else {
            panic!("authorized session must be claimable");
        };
        assert_eq!(snapshot.status, SetupSessionStatus::Authorized);

        // Duplicate commit and cancel are locked out while claimed.
        assert!(matches!(
            mgr.claim_for_commit(&id).await,
            CommitClaim::AlreadyCommitting
        ));
        assert!(matches!(
            mgr.cancel_session(&id).await,
            CancelOutcome::CommitInProgress
        ));
        assert!(mgr.get_session(&id, |_| ()).await.is_some());

        // Releasing the claim (failed commit) restores Authorized: the
        // session can be re-claimed or cancelled.
        mgr.release_commit_claim(&id).await;
        let status = mgr.get_session(&id, |s| s.status.clone()).await.unwrap();
        assert_eq!(status, SetupSessionStatus::Authorized);
        assert!(matches!(
            mgr.cancel_session(&id).await,
            CancelOutcome::Cancelled
        ));
        assert!(matches!(
            mgr.cancel_session(&id).await,
            CancelOutcome::NotFound
        ));
    }

    /// Unknown IDs report `NotFound` for both claim and cancel.
    #[tokio::test]
    async fn setup_manager_claim_and_cancel_nonexistent_return_not_found() {
        let mgr = OAuthSetupManager::new();
        let fake_id = Uuid::new_v4();
        assert!(matches!(
            mgr.claim_for_commit(&fake_id).await,
            CommitClaim::NotFound
        ));
        assert!(matches!(
            mgr.cancel_session(&fake_id).await,
            CancelOutcome::NotFound
        ));
        // Releasing a claim on a missing session is a harmless no-op.
        mgr.release_commit_claim(&fake_id).await;
    }

    /// A claimed (`Committing`) session is frozen: `get_session_mut` and
    /// `mark_authorized` are both refused, so a late credentials submission
    /// or duplicate callback cannot flip the status and reopen the
    /// cancel/duplicate-commit races the claim closed. Releasing the claim
    /// unfreezes it.
    #[tokio::test]
    async fn setup_manager_claimed_session_rejects_mutation_and_callback() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();
        mgr.get_session_mut(&id, |s| s.status = SetupSessionStatus::Authorized)
            .await;
        assert!(matches!(
            mgr.claim_for_commit(&id).await,
            CommitClaim::Claimed(_)
        ));

        let tokens = crate::token_manager::TokenSet {
            access_token: "late".into(),
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".into(),
            scope: None,
            issued_at: None,
        };
        assert!(
            !mgr.mark_authorized(&id, tokens.clone()).await,
            "a late callback must not touch a claimed session"
        );
        assert!(
            mgr.get_session_mut(&id, |s| s.status = SetupSessionStatus::AwaitingAuth)
                .await
                .is_none(),
            "mutation must be refused while claimed"
        );
        let status = mgr.get_session(&id, |s| s.status.clone()).await.unwrap();
        assert_eq!(status, SetupSessionStatus::Committing);

        // After release both work again.
        mgr.release_commit_claim(&id).await;
        assert!(mgr.mark_authorized(&id, tokens).await);
        assert!(mgr
            .get_session_mut(&id, |s| s.client_id = Some("cid".into()))
            .await
            .is_some());
    }

    /// Claiming refreshes the session's expiry lease so the age sweeps
    /// cannot drop an actively committing session (releasing its name
    /// reservation mid-write), while an abandoned claim still expires.
    #[tokio::test]
    async fn setup_manager_claim_refreshes_expiry_lease() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();
        mgr.get_session_mut(&id, |s| s.status = SetupSessionStatus::Authorized)
            .await;

        // Age the session close to expiry, then claim: the claim must
        // restart the age clock. (`checked_sub` guards hosts whose
        // monotonic clock is younger than the backdate.)
        let Some(aged) =
            Instant::now().checked_sub(SETUP_SESSION_MAX_AGE - Duration::from_secs(10))
        else {
            return;
        };
        mgr.sessions.write().await.get_mut(&id).unwrap().created_at = aged;
        assert!(matches!(
            mgr.claim_for_commit(&id).await,
            CommitClaim::Claimed(_)
        ));
        let lease_age = mgr
            .sessions
            .read()
            .await
            .get(&id)
            .unwrap()
            .created_at
            .elapsed();
        assert!(
            lease_age < Duration::from_secs(60),
            "claim must refresh the expiry lease, got age {lease_age:?}"
        );

        // The sweep keeps the freshly claimed session (and its name
        // reservation): a same-name create_session is still refused.
        mgr.cleanup_stale().await;
        assert!(mgr.get_session(&id, |_| ()).await.is_some());
        assert!(mgr
            .create_session("ep".into(), "https://y.com".into(), None, None, None)
            .await
            .is_none());

        // An abandoned claim (never released) still expires one max-age
        // after the claim.
        let Some(expired) =
            Instant::now().checked_sub(SETUP_SESSION_MAX_AGE + Duration::from_secs(1))
        else {
            return;
        };
        mgr.sessions.write().await.get_mut(&id).unwrap().created_at = expired;
        mgr.cleanup_stale().await;
        assert!(mgr.sessions.read().await.get(&id).is_none());
    }

    /// Cancelling an expired `Committing` session (an abandoned claim whose
    /// lease has lapsed) removes it and reports `NotFound` — DELETE must not
    /// keep answering `CommitInProgress` forever when nothing else sweeps
    /// the dead claim.
    #[tokio::test]
    async fn setup_manager_cancel_expired_claim_returns_not_found() {
        let mgr = OAuthSetupManager::new();
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();
        mgr.get_session_mut(&id, |s| s.status = SetupSessionStatus::Authorized)
            .await;
        assert!(matches!(
            mgr.claim_for_commit(&id).await,
            CommitClaim::Claimed(_)
        ));

        // A live claim still refuses cancellation.
        assert!(matches!(
            mgr.cancel_session(&id).await,
            CancelOutcome::CommitInProgress
        ));

        // Lapse the claim's lease: cancel now removes the session and
        // reports NotFound instead of an endless 409.
        let Some(expired) =
            Instant::now().checked_sub(SETUP_SESSION_MAX_AGE + Duration::from_secs(1))
        else {
            return;
        };
        mgr.sessions.write().await.get_mut(&id).unwrap().created_at = expired;
        assert!(matches!(
            mgr.cancel_session(&id).await,
            CancelOutcome::NotFound
        ));
        assert!(mgr.sessions.read().await.get(&id).is_none());
    }

    /// `is_name_reserved` reflects live sessions only.
    #[tokio::test]
    async fn setup_manager_is_name_reserved() {
        let mgr = OAuthSetupManager::new();
        assert!(!mgr.is_name_reserved("ep").await);
        let id = mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();
        assert!(mgr.is_name_reserved("ep").await);
        assert!(!mgr.is_name_reserved("other").await);
        mgr.remove_session(&id).await;
        assert!(!mgr.is_name_reserved("ep").await);
    }
}
