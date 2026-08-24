//! Enterprise-Managed Authorization (EMA) token-exchange grant clients.
//!
//! Implements the two new token-exchange legs of the EMA chain plus ID-JAG
//! claim validation. Step 1 (IdP SSO) reuses the existing authorization-code
//! machinery; this module covers:
//!
//! - Step 2 (RFC 8693): ID Token → ID-JAG ([`exchange_for_id_jag`], M4/M5).
//! - ID-JAG claim validation between steps ([`validate_id_jag`], M6).
//! - Step 3 (RFC 7523): ID-JAG → access token ([`redeem_id_jag`], M7/M8).
//!
//! Every URL is routed through [`url_guard`] (M11). Per D4/S1, the relay does
//! NOT verify the ID-JAG signature against the IdP JWKS for v1: the assertion
//! arrives over TLS from the IdP and the downstream AS verifies the signature
//! in Step 3. This is a documented hardening follow-up.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::oauth::client::ENDARA_CLIENT_METADATA_URL;
use crate::oauth::url_guard::{self, UrlGuardError};
use crate::token_manager::{IdpCredentials, TokenManager, TokenSet};

/// RFC 8693 token-exchange grant type (Step 2 request `grant_type`).
const GRANT_TYPE_TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
/// RFC 8693 `requested_token_type` selecting an ID-JAG (Step 2).
const REQUESTED_TOKEN_TYPE_ID_JAG: &str = "urn:ietf:params:oauth:token-type:id-jag";
/// RFC 8693 `subject_token_type` for the inbound OIDC ID Token (Step 2).
const SUBJECT_TOKEN_TYPE_ID_TOKEN: &str = "urn:ietf:params:oauth:token-type:id_token";
/// RFC 7523 JWT-bearer grant type (Step 3 request `grant_type`).
const GRANT_TYPE_JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// OIDC `refresh_token` grant type used to mint a fresh ID Token at the IdP.
const GRANT_TYPE_REFRESH_TOKEN: &str = "refresh_token";

/// Compose the IdP-facing scope (Step-1 SSO authorize + the IdP `refresh_token`
/// grant) from the resource's *configured* scopes. `openid` is always present so
/// the IdP mints an ID Token; any configured resource scopes are appended
/// (deduplicated); and `offline_access` is appended when a refresh token is
/// desired. With no configured scope this yields the historical
/// `openid offline_access` (or `openid` when no refresh token is wanted), so
/// behavior is unchanged for endpoints that configure no scope.
pub fn compose_idp_scope(resource_scope: Option<&str>, want_offline: bool) -> String {
    let mut parts: Vec<&str> = vec!["openid"];
    if let Some(scope) = resource_scope {
        for tok in scope.split_whitespace() {
            if !parts.contains(&tok) {
                parts.push(tok);
            }
        }
    }
    if want_offline && !parts.contains(&"offline_access") {
        parts.push("offline_access");
    }
    parts.join(" ")
}

/// Clock-skew buffer (seconds), mirroring `TokenSet::is_valid`.
const EXP_SKEW_SECS: u64 = 30;

/// Dedicated error type for EMA token-exchange failures.
///
/// IdP authorization denials ([`EmaError::AuthorizationDenied`]) are kept
/// distinct from transport/expiry-class failures ([`EmaError::TokenEndpoint`],
/// [`EmaError::Http`]) so the refresh chain can surface a terminal,
/// non-retryable "your org hasn't approved this server" state (M5).
#[derive(Debug, thiserror::Error)]
pub enum EmaError {
    /// IdP rejected the exchange because the user's group lacks access. The
    /// `error` is the OAuth error code; non-retryable (M5).
    #[error("IdP denied authorization (non-retryable): {error}: {description}")]
    AuthorizationDenied { error: String, description: String },

    /// The ID-JAG failed claim validation (iss/aud/resource/exp/sub) or could
    /// not be decoded (M6).
    #[error("ID-JAG validation failed: {reason}")]
    InvalidIdJag { reason: String },

    /// The IdP refresh token is gone/rejected; interactive re-SSO is required.
    /// Surfaced so the caller never loops silently (M9).
    #[error("re-authentication (SSO) required: {reason}")]
    ReauthRequired { reason: String },

    /// A token endpoint returned a non-success status that is not a terminal
    /// authorization denial (transport/expiry-class; may be retried).
    #[error("token endpoint returned {status}: {body}")]
    TokenEndpoint { status: StatusCode, body: String },

    /// A token endpoint response was missing a required field.
    #[error("token endpoint response missing field '{field}'")]
    MalformedResponse { field: String },

    /// Underlying transport failure (connect/TLS/timeout).
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// Token endpoint returned a body that could not be parsed as JSON.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// An IdP/AS/token URL was rejected by the SSRF guard (M11).
    #[error("EMA URL rejected by SSRF guard: {0}")]
    UrlGuard(#[from] UrlGuardError),

    /// A token-store read/write failed while loading IdP credentials or
    /// persisting the resulting access token.
    #[error("token storage error: {0}")]
    Storage(String),

    /// The grant this refresh ran against was discarded or replaced
    /// (disconnect/reset or a new login) while the chain was in flight; the
    /// minted token was NOT persisted (see [`GrantGuard`]).
    #[error("grant discarded or replaced while EMA refresh was in flight")]
    StaleGrant,
}

/// Epoch guard for the endpoint-token persistence at the end of
/// [`ensure_access_token`].
///
/// The OAuth adapter's grant epoch is bumped (under its `apply_lock`) by both
/// `disconnect()` (reset/revoke) and a NEW-grant token install, so a refresh
/// that outlives its grant must not write its result to disk — that would
/// resurrect a revoked grant after restart or overwrite the replacement
/// grant's persisted tokens. When a guard is supplied, the save runs under
/// `apply_lock` and only if `grant_epoch` still equals `expected_epoch`;
/// otherwise the chain abandons with [`EmaError::StaleGrant`]. Epoch bumps
/// happen under this same lock, so a bump ordered after the guarded save also
/// cleans up or overwrites what it wrote — the disk can never end up holding
/// a stale grant's token.
pub struct GrantGuard<'a> {
    /// The adapter's `apply_lock`: serializes token installs, disconnects,
    /// and epoch bumps.
    pub apply_lock: &'a Mutex<()>,
    /// The adapter's grant epoch counter.
    pub grant_epoch: &'a AtomicU64,
    /// The caller's epoch snapshot, taken BEFORE the refresh began.
    pub expected_epoch: u64,
}

/// Build the RFC 8693 token-exchange form body for the ID Token → ID-JAG leg
/// (M4). Kept pure so the exact wire form can be asserted in isolation.
///
/// `client_id` is the IdP client identifier — the org's pre-registered id when
/// present, else the hosted CIMD `client_id` ([`ENDARA_CLIENT_METADATA_URL`])
/// when `None`. Okta's token-exchange grant requires this. `client_secret` is
/// appended only when `Some` (confidential clients); a public client must omit
/// it. `scope` (the resource's configured scopes) is appended only when `Some`
/// and non-empty; with `None` the exchange omits `scope` exactly as before
/// (regression-safe).
fn build_id_jag_exchange_form(
    id_token: &str,
    resource_as_issuer: &str,
    resource: &str,
    scope: Option<&str>,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> String {
    let client_id = client_id.unwrap_or(ENDARA_CLIENT_METADATA_URL);
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    ser.append_pair("grant_type", GRANT_TYPE_TOKEN_EXCHANGE)
        .append_pair("requested_token_type", REQUESTED_TOKEN_TYPE_ID_JAG)
        .append_pair("audience", resource_as_issuer)
        .append_pair("resource", resource)
        .append_pair("subject_token", id_token)
        .append_pair("subject_token_type", SUBJECT_TOKEN_TYPE_ID_TOKEN);
    if let Some(scope) = scope.filter(|s| !s.is_empty()) {
        ser.append_pair("scope", scope);
    }
    ser.append_pair("client_id", client_id);
    if let Some(secret) = client_secret {
        ser.append_pair("client_secret", secret);
    }
    ser.finish()
}

/// Build the RFC 7523 jwt-bearer form body for the ID-JAG → access-token leg
/// (M7). The relay identifies itself via `client_id` — the org's pre-registered
/// id when present, else the hosted CIMD `client_id`
/// ([`ENDARA_CLIENT_METADATA_URL`]) when `None`. `client_secret` is appended
/// only when `Some` (confidential clients authenticating via
/// `client_secret_post`); a public client must omit it. `scope` carries the
/// same resource scopes used on the Step 2 exchange and is appended only when
/// `Some` and non-empty; with `None` the form omits `scope` exactly as before
/// (regression-safe).
fn build_jwt_bearer_form(
    id_jag: &str,
    scope: Option<&str>,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> String {
    let client_id = client_id.unwrap_or(ENDARA_CLIENT_METADATA_URL);
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    ser.append_pair("grant_type", GRANT_TYPE_JWT_BEARER)
        .append_pair("assertion", id_jag);
    if let Some(scope) = scope.filter(|s| !s.is_empty()) {
        ser.append_pair("scope", scope);
    }
    ser.append_pair("client_id", client_id);
    if let Some(secret) = client_secret {
        ser.append_pair("client_secret", secret);
    }
    ser.finish()
}

/// Build the OIDC `refresh_token` grant body used to mint a fresh ID Token at
/// the IdP (M9). The relay is a public client identified by `client_id` — the
/// org's pre-registered id when present, else the hosted CIMD `client_id`
/// ([`ENDARA_CLIENT_METADATA_URL`]) when `None`. `client_secret` is appended
/// only when `Some` (confidential clients); a public client must omit it.
/// `scope` is the IdP-facing scope composed by [`compose_idp_scope`] — it always
/// carries `openid` and `offline_access` (so the IdP keeps re-minting ID Tokens
/// and a refresh token) plus any configured resource scopes.
fn build_refresh_token_form(
    refresh_token: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    scope: &str,
) -> String {
    let client_id = client_id.unwrap_or(ENDARA_CLIENT_METADATA_URL);
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    ser.append_pair("grant_type", GRANT_TYPE_REFRESH_TOKEN)
        .append_pair("refresh_token", refresh_token)
        .append_pair("client_id", client_id)
        .append_pair("scope", scope);
    if let Some(secret) = client_secret {
        ser.append_pair("client_secret", secret);
    }
    ser.finish()
}

/// Map a non-success IdP token-exchange response to an [`EmaError`] (M5).
///
/// An OAuth `access_denied` (or `authorization_request_denied`) error on HTTP
/// 400 is a terminal authorization denial; every other status/body is treated
/// as a transport/expiry-class failure the refresh chain may retry.
fn classify_exchange_error(status: StatusCode, body: &str) -> EmaError {
    if status == StatusCode::BAD_REQUEST {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body) {
            if let Some(err) = map.get("error").and_then(|v| v.as_str()) {
                if err == "access_denied" || err == "authorization_request_denied" {
                    let description = map
                        .get("error_description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    return EmaError::AuthorizationDenied {
                        error: err.to_string(),
                        description,
                    };
                }
            }
        }
    }
    EmaError::TokenEndpoint {
        status,
        body: body.to_string(),
    }
}

/// Current Unix time in whole seconds.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Step 2 (RFC 8693): exchange an OIDC ID Token for an ID-JAG scoped to a
/// single resource (M4).
///
/// POSTs the token-exchange grant to the **IdP** token endpoint and returns the
/// ID-JAG JWT (the response `access_token`). The URL is validated through
/// [`url_guard`] (M11). HTTP 400 `access_denied` maps to a terminal
/// [`EmaError::AuthorizationDenied`]; other failures stay transport/expiry-class
/// (M5). `scope` carries the resource's configured scopes onto the exchange form
/// (omitted when `None`).
#[allow(clippy::too_many_arguments)]
pub async fn exchange_for_id_jag(
    idp_token_endpoint: &str,
    id_token: &str,
    resource_as_issuer: &str,
    resource: &str,
    scope: Option<&str>,
    allow_insecure: bool,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<String, EmaError> {
    let client = url_guard::validated_client(idp_token_endpoint, allow_insecure).await?;
    let form_body = build_id_jag_exchange_form(
        id_token,
        resource_as_issuer,
        resource,
        scope,
        client_id,
        client_secret,
    );

    let resp = client
        .post(idp_token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(classify_exchange_error(status, &body));
    }

    let json: Value = resp.json().await?;
    let id_jag = json["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if id_jag.is_empty() {
        return Err(EmaError::MalformedResponse {
            field: "access_token".to_string(),
        });
    }
    Ok(id_jag)
}

/// Step 3 (RFC 7523): redeem an ID-JAG for an ordinary access token at the
/// resource AS (M7/M8).
///
/// POSTs the jwt-bearer grant to the **resource AS** token endpoint with
/// `client_id` (the org's pre-registered id, or the hosted CIMD `client_id`
/// when `None`). Confidential clients additionally authenticate via
/// `client_secret_post` when `client_secret` is `Some`; public clients pass
/// `None`. Parses the response into a [`TokenSet`] mirroring the existing
/// OAuth token parsing.
pub async fn redeem_id_jag(
    as_token_endpoint: &str,
    id_jag: &str,
    scope: Option<&str>,
    allow_insecure: bool,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<TokenSet, EmaError> {
    let client = url_guard::validated_client(as_token_endpoint, allow_insecure).await?;
    let form_body = build_jwt_bearer_form(id_jag, scope, client_id, client_secret);

    let resp = client
        .post(as_token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(EmaError::TokenEndpoint { status, body });
    }

    let json: Value = resp.json().await?;
    let access_token = json["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        return Err(EmaError::MalformedResponse {
            field: "access_token".to_string(),
        });
    }
    let now_secs = now_unix();
    Ok(TokenSet {
        access_token,
        refresh_token: json["refresh_token"].as_str().map(|s| s.to_string()),
        expires_at: json["expires_in"].as_u64().map(|secs| now_secs + secs),
        token_type: json["token_type"].as_str().unwrap_or("Bearer").to_string(),
        scope: json["scope"].as_str().map(|s| s.to_string()),
        issued_at: Some(now_secs),
    })
}

/// Claims carried out of a validated ID-JAG for downstream use/attribution.
#[derive(Debug, Clone)]
pub struct IdJagClaims {
    pub iss: String,
    pub aud: String,
    pub resource: String,
    pub sub: String,
    pub exp: u64,
}

/// Decode the (unverified) claims segment of a compact JWS. No signature
/// verification is performed (D4/S1).
fn decode_jwt_claims(jwt: &str) -> Result<Value, EmaError> {
    let mut parts = jwt.split('.');
    let payload = match (parts.next(), parts.next(), parts.next()) {
        (Some(_header), Some(payload), Some(_sig)) => payload,
        _ => {
            return Err(EmaError::InvalidIdJag {
                reason: "malformed JWT: expected three dot-separated segments".to_string(),
            })
        }
    };
    let payload = payload.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| EmaError::InvalidIdJag {
            reason: format!("base64url decode of claims failed: {e}"),
        })?;
    serde_json::from_slice(&bytes).map_err(|e| EmaError::InvalidIdJag {
        reason: format!("claims are not valid JSON: {e}"),
    })
}

/// Extract a required string claim, erroring with a typed reason if absent.
fn require_str<'a>(claims: &'a Value, field: &str) -> Result<&'a str, EmaError> {
    claims[field]
        .as_str()
        .ok_or_else(|| EmaError::InvalidIdJag {
            reason: format!("missing or non-string '{field}' claim"),
        })
}

/// RFC 7519 `aud` may be a single string or an array of strings.
fn aud_contains(aud: &Value, expected: &str) -> bool {
    match aud {
        Value::String(s) => s == expected,
        Value::Array(items) => items.iter().any(|v| v.as_str() == Some(expected)),
        _ => false,
    }
}

/// Validate an ID-JAG's claims before redemption (M6).
///
/// Checks `iss`/`aud`/`resource` equality, requires `exp` to be more than
/// [`EXP_SKEW_SECS`] in the future, and requires `sub`. Signature is NOT
/// verified (D4/S1). Returns the validated claims on success; a typed
/// [`EmaError::InvalidIdJag`] on any mismatch.
pub fn validate_id_jag(
    jwt: &str,
    expected_iss: &str,
    expected_aud: &str,
    expected_resource: &str,
) -> Result<IdJagClaims, EmaError> {
    let claims = decode_jwt_claims(jwt)?;

    let iss = require_str(&claims, "iss")?;
    if iss != expected_iss {
        return Err(EmaError::InvalidIdJag {
            reason: format!("iss mismatch: expected '{expected_iss}', got '{iss}'"),
        });
    }

    if !aud_contains(&claims["aud"], expected_aud) {
        return Err(EmaError::InvalidIdJag {
            reason: format!("aud mismatch: expected '{expected_aud}'"),
        });
    }

    let resource = require_str(&claims, "resource")?;
    if resource != expected_resource {
        return Err(EmaError::InvalidIdJag {
            reason: format!("resource mismatch: expected '{expected_resource}', got '{resource}'"),
        });
    }

    let exp = claims["exp"]
        .as_u64()
        .ok_or_else(|| EmaError::InvalidIdJag {
            reason: "missing or non-numeric 'exp' claim".to_string(),
        })?;
    let now = now_unix();
    if exp <= now + EXP_SKEW_SECS {
        return Err(EmaError::InvalidIdJag {
            reason: format!("token expired or within skew window (exp={exp}, now={now})"),
        });
    }

    let sub = require_str(&claims, "sub")?;

    Ok(IdJagClaims {
        iss: iss.to_string(),
        aud: expected_aud.to_string(),
        resource: resource.to_string(),
        sub: sub.to_string(),
        exp,
    })
}

/// A fresh ID Token (plus any rotated refresh token) minted by the IdP
/// `refresh_token` grant.
struct RefreshedIdToken {
    id_token: String,
    refresh_token: Option<String>,
    id_token_expires_at: Option<u64>,
}

/// OIDC `refresh_token` grant at the IdP: trade the stored refresh token for a
/// fresh ID Token (M9). POSTs to the **IdP** token endpoint (routed through
/// [`url_guard`], M11) and parses the new `id_token` (+ rotation) from the
/// response. Any non-success status is surfaced as transport/expiry-class
/// [`EmaError::TokenEndpoint`]; the caller maps a failed refresh to
/// [`EmaError::ReauthRequired`]. `scope` is the IdP-facing scope composed by
/// [`compose_idp_scope`] (always `openid offline_access`, plus any configured
/// resource scopes).
async fn refresh_idp_token(
    idp_token_endpoint: &str,
    refresh_token: &str,
    allow_insecure: bool,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    scope: &str,
) -> Result<RefreshedIdToken, EmaError> {
    let client = url_guard::validated_client(idp_token_endpoint, allow_insecure).await?;
    let form_body = build_refresh_token_form(refresh_token, client_id, client_secret, scope);

    let resp = client
        .post(idp_token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(EmaError::TokenEndpoint { status, body });
    }

    let json: Value = resp.json().await?;
    let id_token = json["id_token"].as_str().unwrap_or_default().to_string();
    if id_token.is_empty() {
        return Err(EmaError::MalformedResponse {
            field: "id_token".to_string(),
        });
    }
    // Prefer the ID Token's own `exp` claim; fall back to `expires_in`.
    let id_token_expires_at = id_token_exp(&id_token)
        .or_else(|| json["expires_in"].as_u64().map(|secs| now_unix() + secs));
    Ok(RefreshedIdToken {
        id_token,
        refresh_token: json["refresh_token"].as_str().map(|s| s.to_string()),
        id_token_expires_at,
    })
}

/// Best-effort extraction of the `exp` claim from a (compact) JWT ID Token.
/// Returns `None` if the token can't be decoded or carries no numeric `exp`.
fn id_token_exp(id_token: &str) -> Option<u64> {
    decode_jwt_claims(id_token)
        .ok()
        .and_then(|c| c["exp"].as_u64())
}

/// True when the stored ID Token is known to be expired (or within the skew
/// window). Credentials without a recorded expiry are treated as not-expired;
/// a failed exchange still triggers a reactive refresh.
fn id_token_expired(creds: &IdpCredentials) -> bool {
    match creds.id_token_expires_at {
        Some(exp) => exp <= now_unix() + EXP_SKEW_SECS,
        None => false,
    }
}

/// True when a Step 2 failure looks like an expired/invalid subject token
/// (refreshable) rather than a policy denial or other terminal error. Used to
/// decide whether to attempt a single IdP-token refresh before retrying.
fn is_subject_token_invalid(err: &EmaError) -> bool {
    match err {
        EmaError::TokenEndpoint { status, body } => {
            *status == StatusCode::UNAUTHORIZED
                || body.contains("invalid_grant")
                || body.contains("invalid_token")
        }
        _ => false,
    }
}

/// Run the IdP `refresh_token` grant, persist the rotated credentials, and
/// return the fresh ID Token. A missing refresh token or any grant failure is
/// surfaced as [`EmaError::ReauthRequired`] (M9: no silent loop). `resource_scope`
/// is the resource's configured scopes; it is composed with `openid` and
/// `offline_access` via [`compose_idp_scope`] for the refresh grant.
#[allow(clippy::too_many_arguments)]
async fn refresh_and_persist_idp_token(
    token_manager: &TokenManager,
    idp_token_endpoint: &str,
    idp_key: &str,
    creds: &IdpCredentials,
    allow_insecure: bool,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    resource_scope: Option<&str>,
) -> Result<String, EmaError> {
    let refresh_token = creds
        .refresh_token
        .as_deref()
        .ok_or_else(|| EmaError::ReauthRequired {
            reason: "ID Token expired and no IdP refresh token is stored".to_string(),
        })?;

    let refreshed = refresh_idp_token(
        idp_token_endpoint,
        refresh_token,
        allow_insecure,
        client_id,
        client_secret,
        &compose_idp_scope(resource_scope, true),
    )
    .await
    .map_err(|e| EmaError::ReauthRequired {
        reason: format!("IdP refresh-token grant failed: {e}"),
    })?;

    let new_creds = IdpCredentials {
        idp_issuer: creds.idp_issuer.clone(),
        id_token: refreshed.id_token.clone(),
        refresh_token: refreshed
            .refresh_token
            .or_else(|| creds.refresh_token.clone()),
        id_token_expires_at: refreshed.id_token_expires_at,
        obtained_at: now_unix(),
    };
    token_manager
        .save_idp(idp_key, &new_creds)
        .await
        .map_err(|e| EmaError::Storage(e.to_string()))?;

    Ok(refreshed.id_token)
}

/// Ensure a valid access token for an EMA endpoint, minting one through the
/// full ID-JAG chain when needed (§4.5, M9/S2).
///
/// Order of operations:
/// 1. If a still-valid `TokenSet` is persisted for `endpoint`, return it (no
///    lock, no network).
/// 2. Acquire `refresh_mutex` to coalesce concurrent refreshes per endpoint
///    (S2), then re-check the persisted token in case a peer just refreshed.
/// 3. Load the IdP credentials (`idp_key`); absence ⇒ [`EmaError::ReauthRequired`].
/// 4. Step 2 (`exchange_for_id_jag`). If the ID Token is known-expired this is
///    preceded by a refresh; an `invalid_grant`/`401` exchange failure triggers
///    a single refresh-and-retry. A failed refresh ⇒ [`EmaError::ReauthRequired`]
///    (no silent loop); an [`EmaError::AuthorizationDenied`] propagates as-is (M5).
/// 5. [`validate_id_jag`] (M6) then Step 3 (`redeem_id_jag`), persisting the
///    resulting `TokenSet` via [`TokenManager::save`] (M8). With a
///    [`GrantGuard`] the save runs under the adapter's `apply_lock` and only
///    while the grant epoch still matches the caller's snapshot; a stale
///    chain abandons with [`EmaError::StaleGrant`] instead of writing a
///    discarded grant's token to disk.
///
/// `refresh_mutex` is supplied by the caller (the OAuth adapter owns one per
/// endpoint), mirroring the adapter's existing refresh-coalescing guard.
/// `grant_guard` is `None` only for callers with no adapter grant lifecycle
/// (unit tests); the adapter always passes `Some`.
///
/// **Credential routing (R1).** `client_id`/`client_secret` are the *requesting*
/// client credentials and are used only on the IdP-facing legs (Step 2 exchange
/// and the IdP `refresh_token` grant). `resource_client_id`/
/// `resource_client_secret` are the optional *resource* credential presented at
/// the MAS in Step 3 (`redeem_id_jag`). When no resource credential is
/// configured, Step 3 reuses the requesting `client_id` (org id, else CIMD) for
/// identification only and sends **no** `client_secret` — the requesting secret
/// is never substituted at the MAS.
///
/// **Scope alignment (R2).** `resource_scope` carries the resource's configured
/// scopes. It is threaded onto the Step 2 exchange and the Step 3 redemption
/// verbatim (omitted when `None`), and composed with `openid`/`offline_access`
/// via [`compose_idp_scope`] for the IdP `refresh_token` grant. With `None` the
/// exchange/redemption omit `scope` and the refresh grant falls back to the
/// historical `openid offline_access` (regression-safe).
#[allow(clippy::too_many_arguments)]
pub async fn ensure_access_token(
    token_manager: &TokenManager,
    refresh_mutex: &Mutex<()>,
    endpoint: &str,
    idp_key: &str,
    idp_token_endpoint: &str,
    as_issuer: &str,
    as_token_endpoint: &str,
    resource: &str,
    resource_scope: Option<&str>,
    allow_insecure: bool,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    resource_client_id: Option<&str>,
    resource_client_secret: Option<&str>,
    grant_guard: Option<GrantGuard<'_>>,
) -> Result<TokenSet, EmaError> {
    // Fast path: a still-valid persisted token needs neither lock nor network.
    if let Some(ts) = load_token(token_manager, endpoint).await? {
        if ts.is_valid() {
            return Ok(ts);
        }
    }

    // Coalesce concurrent refreshes (S2): only one caller runs the chain; the
    // rest wait here and reuse the token it persists.
    let _guard = refresh_mutex.lock().await;
    if let Some(ts) = load_token(token_manager, endpoint).await? {
        if ts.is_valid() {
            return Ok(ts);
        }
    }

    let creds = token_manager
        .load_idp(idp_key)
        .await
        .map_err(|e| EmaError::Storage(e.to_string()))?
        .ok_or_else(|| EmaError::ReauthRequired {
            reason: format!("no stored IdP credentials for key '{idp_key}'"),
        })?;

    let idp_issuer = creds.idp_issuer.clone();
    let mut id_token = creds.id_token.clone();
    let mut refreshed = false;

    // Proactively refresh a known-expired ID Token before a doomed exchange.
    if id_token_expired(&creds) {
        id_token = refresh_and_persist_idp_token(
            token_manager,
            idp_token_endpoint,
            idp_key,
            &creds,
            allow_insecure,
            client_id,
            client_secret,
            resource_scope,
        )
        .await?;
        refreshed = true;
    }

    // Step 2 (RFC 8693): ID Token → ID-JAG, with a single refresh-and-retry on
    // an invalid/expired subject token.
    let id_jag = match exchange_for_id_jag(
        idp_token_endpoint,
        &id_token,
        as_issuer,
        resource,
        resource_scope,
        allow_insecure,
        client_id,
        client_secret,
    )
    .await
    {
        Ok(jag) => jag,
        Err(e) if !refreshed && is_subject_token_invalid(&e) => {
            let new_id_token = refresh_and_persist_idp_token(
                token_manager,
                idp_token_endpoint,
                idp_key,
                &creds,
                allow_insecure,
                client_id,
                client_secret,
                resource_scope,
            )
            .await?;
            exchange_for_id_jag(
                idp_token_endpoint,
                &new_id_token,
                as_issuer,
                resource,
                resource_scope,
                allow_insecure,
                client_id,
                client_secret,
            )
            .await?
        }
        Err(e) => return Err(e),
    };

    // M6 claim validation, then Step 3 (RFC 7523) → access token. Step 3
    // authenticates at the MAS with the optional **resource** credential, never
    // the requesting client's secret (R1): the resource `client_id` is used when
    // set (else the requesting `client_id`/CIMD for identification only) and the
    // resource secret is sent solely when present.
    validate_id_jag(&id_jag, &idp_issuer, as_issuer, resource)?;
    let token_set = redeem_id_jag(
        as_token_endpoint,
        &id_jag,
        resource_scope,
        allow_insecure,
        resource_client_id.or(client_id),
        resource_client_secret,
    )
    .await?;
    // Persist the minted token — epoch-guarded when the caller has a grant
    // lifecycle. The guard's `apply_lock` is the same lock under which the
    // adapter bumps the epoch (disconnect / new-grant install), so a chain
    // that outlived its grant is detected HERE, before its token reaches
    // disk, instead of resurrecting a revoked grant after restart or
    // overwriting a replacement grant's persisted tokens. Lock order:
    // `refresh_mutex` → `apply_lock`, same as the non-EMA refresh path (no
    // `apply_lock` holder acquires the `refresh_mutex`).
    match grant_guard {
        Some(guard) => {
            let _apply = guard.apply_lock.lock().await;
            if guard.expected_epoch != guard.grant_epoch.load(Ordering::Acquire) {
                return Err(EmaError::StaleGrant);
            }
            token_manager
                .save(endpoint, &token_set)
                .await
                .map_err(|e| EmaError::Storage(e.to_string()))?;
        }
        None => {
            token_manager
                .save(endpoint, &token_set)
                .await
                .map_err(|e| EmaError::Storage(e.to_string()))?;
        }
    }
    Ok(token_set)
}

/// Load the persisted access token for `endpoint`, mapping store errors into
/// [`EmaError::Storage`].
async fn load_token(
    token_manager: &TokenManager,
    endpoint: &str,
) -> Result<Option<TokenSet>, EmaError> {
    token_manager
        .load(endpoint)
        .await
        .map_err(|e| EmaError::Storage(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    const IDP_ISS: &str = "https://acme.okta.com";
    const AS_ISS: &str = "https://as.example.com";
    const RESOURCE: &str = "https://api.githubcopilot.com/mcp/";
    /// Historical IdP-facing scope when no resource scope is configured:
    /// `openid` re-mints an ID Token, `offline_access` keeps a refresh token.
    const REFRESH_SCOPE: &str = "openid offline_access";

    fn parse_form(body: &str) -> HashMap<String, String> {
        url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect()
    }

    fn make_jwt(claims: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.sig")
    }

    // ---- M4: token-exchange (Step 2) form exactness --------------------------

    #[test]
    fn id_jag_exchange_form_has_exact_rfc8693_params() {
        let body = build_id_jag_exchange_form("the-id-token", AS_ISS, RESOURCE, None, None, None);
        let f = parse_form(&body);
        assert_eq!(
            f.get("grant_type").map(String::as_str),
            Some("urn:ietf:params:oauth:grant-type:token-exchange")
        );
        assert_eq!(
            f.get("requested_token_type").map(String::as_str),
            Some("urn:ietf:params:oauth:token-type:id-jag")
        );
        assert_eq!(f.get("audience").map(String::as_str), Some(AS_ISS));
        assert_eq!(f.get("resource").map(String::as_str), Some(RESOURCE));
        assert_eq!(
            f.get("subject_token").map(String::as_str),
            Some("the-id-token")
        );
        assert_eq!(
            f.get("subject_token_type").map(String::as_str),
            Some("urn:ietf:params:oauth:token-type:id_token")
        );
        // Step 2 now always carries a `client_id` (Okta requires it); `None`
        // resolves to the hosted CIMD `client_id`, and no secret is sent.
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some(ENDARA_CLIENT_METADATA_URL)
        );
        assert!(
            !f.contains_key("client_secret"),
            "public client must not send a secret"
        );
        assert_eq!(f.len(), 7, "exactly seven params, no extras");
    }

    /// An explicit org `client_id` is sent verbatim on the Step 2 token-exchange
    /// form; `None` keeps the hosted CIMD `client_id`. `client_secret` is
    /// appended only when `Some` (confidential client).
    #[test]
    fn id_jag_exchange_form_client_id_and_secret() {
        // Public client with an explicit org client_id, no secret.
        let body = build_id_jag_exchange_form(
            "the-id-token",
            AS_ISS,
            RESOURCE,
            None,
            Some("org-okta-client"),
            None,
        );
        let f = parse_form(&body);
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some("org-okta-client")
        );
        assert!(!f.contains_key("client_secret"));
        assert_eq!(f.len(), 7, "no extras when secret is None");

        // Confidential client with both client_id and client_secret.
        let body = build_id_jag_exchange_form(
            "the-id-token",
            AS_ISS,
            RESOURCE,
            None,
            Some("org-okta-client"),
            Some("super-secret"),
        );
        let f = parse_form(&body);
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some("org-okta-client")
        );
        assert_eq!(
            f.get("client_secret").map(String::as_str),
            Some("super-secret")
        );
        assert_eq!(f.len(), 8, "exactly eight params when secret is set");

        // `None` client_id falls back to the hosted CIMD URL.
        let body = build_id_jag_exchange_form("the-id-token", AS_ISS, RESOURCE, None, None, None);
        let f = parse_form(&body);
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some(ENDARA_CLIENT_METADATA_URL)
        );
    }

    // ---- M7: jwt-bearer (Step 3) form exactness ------------------------------

    #[test]
    fn jwt_bearer_form_has_exact_rfc7523_params_and_no_secret() {
        let body = build_jwt_bearer_form("the-id-jag", None, None, None);
        let f = parse_form(&body);
        assert_eq!(
            f.get("grant_type").map(String::as_str),
            Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
        );
        assert_eq!(f.get("assertion").map(String::as_str), Some("the-id-jag"));
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some(ENDARA_CLIENT_METADATA_URL)
        );
        assert!(
            !f.contains_key("client_secret"),
            "public client must not send a secret"
        );
        assert_eq!(f.len(), 3, "exactly three params, no extras");
    }

    /// An explicit org `client_id` is sent verbatim on the Step 3 jwt-bearer
    /// form; `None` keeps the hosted CIMD `client_id`.
    #[test]
    fn jwt_bearer_form_client_id_some_vs_none() {
        let with_explicit =
            build_jwt_bearer_form("the-id-jag", None, Some("org-okta-client"), None);
        let f = parse_form(&with_explicit);
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some("org-okta-client")
        );
        assert!(!f.contains_key("client_secret"));
        assert_eq!(f.len(), 3, "exactly three params, no extras");

        let with_none = build_jwt_bearer_form("the-id-jag", None, None, None);
        let f = parse_form(&with_none);
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some(ENDARA_CLIENT_METADATA_URL)
        );
    }

    /// `client_secret` is appended to the Step 3 jwt-bearer form only when
    /// `Some`; the rest of the form is unchanged. Public clients (`None`) keep
    /// the exact three-param wire form.
    #[test]
    fn jwt_bearer_form_client_secret_present_vs_absent() {
        let with_secret = build_jwt_bearer_form(
            "the-id-jag",
            None,
            Some("org-okta-client"),
            Some("super-secret"),
        );
        let f = parse_form(&with_secret);
        assert_eq!(
            f.get("grant_type").map(String::as_str),
            Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
        );
        assert_eq!(f.get("assertion").map(String::as_str), Some("the-id-jag"));
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some("org-okta-client")
        );
        assert_eq!(
            f.get("client_secret").map(String::as_str),
            Some("super-secret")
        );
        assert_eq!(f.len(), 4, "exactly four params when secret is set");

        let without_secret =
            build_jwt_bearer_form("the-id-jag", None, Some("org-okta-client"), None);
        let f = parse_form(&without_secret);
        assert!(!f.contains_key("client_secret"));
        assert_eq!(f.len(), 3);
    }

    /// An explicit org `client_id` is sent verbatim on the IdP refresh form;
    /// `None` keeps the hosted CIMD `client_id`. `scope` and absence of a secret
    /// are preserved when no secret is supplied.
    #[test]
    fn refresh_token_form_client_id_some_vs_none() {
        let with_explicit =
            build_refresh_token_form("the-refresh", Some("org-okta-client"), None, REFRESH_SCOPE);
        let f = parse_form(&with_explicit);
        assert_eq!(
            f.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            f.get("refresh_token").map(String::as_str),
            Some("the-refresh")
        );
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some("org-okta-client")
        );
        assert_eq!(f.get("scope").map(String::as_str), Some(REFRESH_SCOPE));
        assert!(!f.contains_key("client_secret"));
        assert_eq!(f.len(), 4, "exactly four params, no extras");

        let with_none = build_refresh_token_form("the-refresh", None, None, REFRESH_SCOPE);
        let f = parse_form(&with_none);
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some(ENDARA_CLIENT_METADATA_URL)
        );
        assert_eq!(f.get("scope").map(String::as_str), Some(REFRESH_SCOPE));
    }

    /// `client_secret` is appended to the IdP refresh form only when `Some`;
    /// the rest of the form is unchanged.
    #[test]
    fn refresh_token_form_client_secret_present_vs_absent() {
        let with_secret = build_refresh_token_form(
            "the-refresh",
            Some("org-okta-client"),
            Some("super-secret"),
            REFRESH_SCOPE,
        );
        let f = parse_form(&with_secret);
        assert_eq!(
            f.get("client_id").map(String::as_str),
            Some("org-okta-client")
        );
        assert_eq!(
            f.get("client_secret").map(String::as_str),
            Some("super-secret")
        );
        assert_eq!(f.get("scope").map(String::as_str), Some(REFRESH_SCOPE));
        assert_eq!(f.len(), 5, "exactly five params when secret is set");

        let without_secret =
            build_refresh_token_form("the-refresh", Some("org-okta-client"), None, REFRESH_SCOPE);
        let f = parse_form(&without_secret);
        assert!(!f.contains_key("client_secret"));
        assert_eq!(f.len(), 4);
    }

    // ---- M6: ID-JAG validation -----------------------------------------------

    fn good_claims() -> Value {
        serde_json::json!({
            "iss": IDP_ISS,
            "aud": AS_ISS,
            "resource": RESOURCE,
            "sub": "user-123",
            "exp": now_unix() + 600,
        })
    }

    // ---- R2: resource-scope composition + threading -------------------------

    /// [`compose_idp_scope`] always leads with `openid`, appends configured
    /// resource scopes (deduplicated), and appends `offline_access` only when a
    /// refresh token is wanted. With no resource scope it reproduces the
    /// historical `openid offline_access` (regression-safe).
    #[test]
    fn compose_idp_scope_cases() {
        assert_eq!(compose_idp_scope(None, true), "openid offline_access");
        assert_eq!(compose_idp_scope(None, false), "openid");
        assert_eq!(
            compose_idp_scope(Some("todos.read todos.write"), true),
            "openid todos.read todos.write offline_access"
        );
        assert_eq!(
            compose_idp_scope(Some("todos.read"), false),
            "openid todos.read"
        );
        // Duplicates (incl. `openid`/`offline_access`) are not repeated.
        assert_eq!(
            compose_idp_scope(Some("openid todos.read offline_access todos.read"), true),
            "openid todos.read offline_access"
        );
        // Empty/whitespace resource scope behaves like `None`.
        assert_eq!(
            compose_idp_scope(Some("   "), true),
            "openid offline_access"
        );
    }

    /// A configured resource `scope` is appended to the Step 2 exchange and the
    /// Step 3 jwt-bearer forms; `None`/empty omits `scope` entirely.
    #[test]
    fn resource_scope_appended_to_exchange_and_redeem_forms() {
        let body = build_id_jag_exchange_form(
            "the-id-token",
            AS_ISS,
            RESOURCE,
            Some("todos.read"),
            None,
            None,
        );
        let f = parse_form(&body);
        assert_eq!(f.get("scope").map(String::as_str), Some("todos.read"));
        assert_eq!(f.len(), 8, "scope adds exactly one param");

        let body = build_jwt_bearer_form("the-id-jag", Some("todos.read"), None, None);
        let f = parse_form(&body);
        assert_eq!(f.get("scope").map(String::as_str), Some("todos.read"));
        assert_eq!(f.len(), 4, "scope adds exactly one param");

        // `None` and empty scope both omit the `scope` param (regression-safe).
        for empty in [None, Some("")] {
            let body = build_id_jag_exchange_form("t", AS_ISS, RESOURCE, empty, None, None);
            assert!(!parse_form(&body).contains_key("scope"));
            let body = build_jwt_bearer_form("j", empty, None, None);
            assert!(!parse_form(&body).contains_key("scope"));
        }
    }

    #[test]
    fn validate_passes_on_good_claims() {
        let jwt = make_jwt(good_claims());
        let claims = validate_id_jag(&jwt, IDP_ISS, AS_ISS, RESOURCE).expect("should validate");
        assert_eq!(claims.sub, "user-123");
        assert_eq!(claims.resource, RESOURCE);
    }

    #[test]
    fn validate_accepts_aud_as_array() {
        let mut c = good_claims();
        c["aud"] = serde_json::json!(["other", AS_ISS]);
        let jwt = make_jwt(c);
        assert!(validate_id_jag(&jwt, IDP_ISS, AS_ISS, RESOURCE).is_ok());
    }

    #[test]
    fn validate_rejects_iss_mismatch() {
        let jwt = make_jwt(good_claims());
        let err = validate_id_jag(&jwt, "https://evil.okta.com", AS_ISS, RESOURCE).unwrap_err();
        assert!(matches!(err, EmaError::InvalidIdJag { .. }));
    }

    #[test]
    fn validate_rejects_aud_mismatch() {
        let jwt = make_jwt(good_claims());
        let err =
            validate_id_jag(&jwt, IDP_ISS, "https://other-as.example.com", RESOURCE).unwrap_err();
        assert!(matches!(err, EmaError::InvalidIdJag { .. }));
    }

    #[test]
    fn validate_rejects_resource_mismatch() {
        let jwt = make_jwt(good_claims());
        let err =
            validate_id_jag(&jwt, IDP_ISS, AS_ISS, "https://other.example.com/mcp/").unwrap_err();
        assert!(matches!(err, EmaError::InvalidIdJag { .. }));
    }

    #[test]
    fn validate_rejects_expired_exp() {
        let mut c = good_claims();
        c["exp"] = serde_json::json!(now_unix() + 5); // within the 30s skew window
        let jwt = make_jwt(c);
        let err = validate_id_jag(&jwt, IDP_ISS, AS_ISS, RESOURCE).unwrap_err();
        assert!(matches!(err, EmaError::InvalidIdJag { .. }));
    }

    #[test]
    fn validate_rejects_missing_sub() {
        let mut c = good_claims();
        c.as_object_mut().unwrap().remove("sub");
        let jwt = make_jwt(c);
        let err = validate_id_jag(&jwt, IDP_ISS, AS_ISS, RESOURCE).unwrap_err();
        assert!(matches!(err, EmaError::InvalidIdJag { .. }));
    }

    #[test]
    fn validate_rejects_malformed_jwt() {
        let err = validate_id_jag("not-a-jwt", IDP_ISS, AS_ISS, RESOURCE).unwrap_err();
        assert!(matches!(err, EmaError::InvalidIdJag { .. }));
    }

    // ---- M5: denial vs transport classification ------------------------------

    #[test]
    fn access_denied_400_maps_to_authorization_denied() {
        let body = r#"{"error":"access_denied","error_description":"not in group"}"#;
        let err = classify_exchange_error(StatusCode::BAD_REQUEST, body);
        match err {
            EmaError::AuthorizationDenied { error, description } => {
                assert_eq!(error, "access_denied");
                assert_eq!(description, "not in group");
            }
            other => panic!("expected AuthorizationDenied, got {other:?}"),
        }
    }

    #[test]
    fn server_error_is_transport_class_not_denial() {
        let err = classify_exchange_error(StatusCode::SERVICE_UNAVAILABLE, "upstream down");
        assert!(
            matches!(err, EmaError::TokenEndpoint { .. }),
            "5xx must be retryable transport-class, distinct from AuthorizationDenied"
        );
    }

    #[test]
    fn invalid_grant_400_is_transport_class_not_denial() {
        // An expired subject token (invalid_grant) is an expiry/refresh signal,
        // not a policy denial — must stay distinct from AuthorizationDenied.
        let body = r#"{"error":"invalid_grant","error_description":"token expired"}"#;
        let err = classify_exchange_error(StatusCode::BAD_REQUEST, body);
        assert!(matches!(err, EmaError::TokenEndpoint { .. }));
    }

    // ---- §4.5 / M9 / S2: ensure_access_token orchestration -------------------

    /// Per-grant request counters recorded by the mock token server.
    #[derive(Default)]
    struct FxCounts {
        /// RFC 8693 token-exchange (Step 2) hits on the IdP endpoint.
        exchange: u32,
        /// OIDC `refresh_token` (M9) hits on the IdP endpoint.
        refresh: u32,
        /// RFC 7523 jwt-bearer (Step 3) hits on the AS endpoint.
        redeem: u32,
    }

    /// Programmed behaviour for the IdP `refresh_token` grant.
    #[derive(Clone)]
    enum RefreshOutcome {
        /// Refresh fails with 400 `invalid_grant` (→ `ReauthRequired`).
        Fail,
        /// Refresh succeeds and returns `new_id_token`, which also becomes the
        /// only subject token the exchange leg will accept afterwards.
        Succeed { new_id_token: String },
    }

    /// Shared state for the mock token server. `accept_id_token` is the single
    /// subject token the exchange leg accepts; a successful refresh rotates it.
    #[derive(Clone)]
    struct TokenFx {
        counts: Arc<std::sync::Mutex<FxCounts>>,
        accept_id_token: Arc<std::sync::Mutex<String>>,
        refresh: RefreshOutcome,
        delay_ms: u64,
    }

    /// Spawn a mock token server on `127.0.0.1:0` exposing the IdP token
    /// endpoint (`/idp/token`, serving both token-exchange and refresh grants)
    /// and the AS token endpoint (`/as/token`, serving jwt-bearer). Returns
    /// `(idp_token_endpoint, as_token_endpoint, counts, handle)`.
    async fn spawn_token_fixture(
        refresh: RefreshOutcome,
        accept_id_token: &str,
        delay_ms: u64,
    ) -> (
        String,
        String,
        Arc<std::sync::Mutex<FxCounts>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};

        async fn idp_token(State(fx): State<TokenFx>, body: String) -> axum::response::Response {
            let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect();
            let grant = form
                .get("grant_type")
                .map(String::as_str)
                .unwrap_or_default();

            if grant == GRANT_TYPE_REFRESH_TOKEN {
                fx.counts.lock().unwrap().refresh += 1;
                return match &fx.refresh {
                    RefreshOutcome::Fail => (
                        StatusCode::BAD_REQUEST,
                        r#"{"error":"invalid_grant","error_description":"refresh expired"}"#,
                    )
                        .into_response(),
                    RefreshOutcome::Succeed { new_id_token } => {
                        *fx.accept_id_token.lock().unwrap() = new_id_token.clone();
                        let resp = serde_json::json!({
                            "id_token": new_id_token,
                            "refresh_token": "rotated-refresh",
                            "expires_in": 3600,
                        });
                        (StatusCode::OK, Json(resp)).into_response()
                    }
                };
            }

            // Token-exchange (Step 2).
            fx.counts.lock().unwrap().exchange += 1;
            if fx.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(fx.delay_ms)).await;
            }
            let subject = form
                .get("subject_token")
                .map(String::as_str)
                .unwrap_or_default();
            if subject != fx.accept_id_token.lock().unwrap().as_str() {
                return (
                    StatusCode::BAD_REQUEST,
                    r#"{"error":"invalid_grant","error_description":"subject token expired"}"#,
                )
                    .into_response();
            }
            let id_jag = make_jwt(serde_json::json!({
                "iss": IDP_ISS,
                "aud": AS_ISS,
                "resource": RESOURCE,
                "sub": "user-123",
                "exp": now_unix() + 600,
            }));
            (
                StatusCode::OK,
                Json(serde_json::json!({ "access_token": id_jag })),
            )
                .into_response()
        }

        async fn as_token(State(fx): State<TokenFx>, _body: String) -> axum::response::Response {
            fx.counts.lock().unwrap().redeem += 1;
            let resp = serde_json::json!({
                "access_token": "final-access-token",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "mcp",
            });
            (StatusCode::OK, Json(resp)).into_response()
        }

        let counts = Arc::new(std::sync::Mutex::new(FxCounts::default()));
        let fx = TokenFx {
            counts: counts.clone(),
            accept_id_token: Arc::new(std::sync::Mutex::new(accept_id_token.to_string())),
            refresh,
            delay_ms,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());

        let router = Router::new()
            .route("/idp/token", post(idp_token))
            .route("/as/token", post(as_token))
            .with_state(fx);

        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (
            format!("{base}/idp/token"),
            format!("{base}/as/token"),
            counts,
            handle,
        )
    }

    fn idp_creds(
        id_token: &str,
        id_token_expires_at: Option<u64>,
        refresh_token: Option<&str>,
    ) -> IdpCredentials {
        IdpCredentials {
            idp_issuer: IDP_ISS.to_string(),
            id_token: id_token.to_string(),
            refresh_token: refresh_token.map(|s| s.to_string()),
            id_token_expires_at,
            obtained_at: now_unix(),
        }
    }

    fn valid_token_set(access: &str) -> TokenSet {
        TokenSet {
            access_token: access.to_string(),
            refresh_token: None,
            expires_at: Some(now_unix() + 3600),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: Some(now_unix()),
        }
    }

    fn expired_token_set() -> TokenSet {
        TokenSet {
            access_token: "stale-access".to_string(),
            refresh_token: None,
            expires_at: Some(1000),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: Some(900),
        }
    }

    /// A still-valid persisted token short-circuits before any lock or network:
    /// the bogus endpoints are never contacted.
    #[tokio::test]
    async fn ensure_returns_valid_cached_token_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save("ep", &valid_token_set("cached-access"))
            .await
            .unwrap();
        let lock = Mutex::new(());

        let ts = ensure_access_token(
            &mgr,
            &lock,
            "ep",
            IDP_ISS,
            "http://127.0.0.1:1/idp/token",
            AS_ISS,
            "http://127.0.0.1:1/as/token",
            RESOURCE,
            None,
            true,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("valid cached token must be returned");
        assert_eq!(ts.access_token, "cached-access");
    }

    /// Access token expired but the ID Token is fine: Steps 2+3 re-run once and
    /// the fresh token is persisted. No IdP refresh occurs.
    #[tokio::test]
    async fn ensure_reruns_chain_on_access_token_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save("ep", &expired_token_set()).await.unwrap();
        mgr.save_idp(
            IDP_ISS,
            &idp_creds("good-id-token", Some(now_unix() + 3600), None),
        )
        .await
        .unwrap();
        let (idp_ep, as_ep, counts, server) =
            spawn_token_fixture(RefreshOutcome::Fail, "good-id-token", 0).await;
        let lock = Mutex::new(());

        let ts = ensure_access_token(
            &mgr, &lock, "ep", IDP_ISS, &idp_ep, AS_ISS, &as_ep, RESOURCE, None, true, None, None,
            None, None, None,
        )
        .await
        .expect("chain must succeed");

        assert_eq!(ts.access_token, "final-access-token");
        {
            let c = counts.lock().unwrap();
            assert_eq!(c.exchange, 1, "one Step 2");
            assert_eq!(c.refresh, 0, "no IdP refresh needed");
            assert_eq!(c.redeem, 1, "one Step 3");
        }
        assert!(mgr.load("ep").await.unwrap().unwrap().is_valid());
        server.abort();
    }

    /// ID Token known-expired: a proactive IdP refresh precedes the exchange,
    /// the rotated credentials are persisted, then Steps 2+3 complete.
    #[tokio::test]
    async fn ensure_refreshes_id_token_then_runs_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp(
            IDP_ISS,
            &idp_creds("old-id-token", Some(1000), Some("idp-refresh")),
        )
        .await
        .unwrap();
        let (idp_ep, as_ep, counts, server) = spawn_token_fixture(
            RefreshOutcome::Succeed {
                new_id_token: "fresh-id-token".to_string(),
            },
            "old-id-token",
            0,
        )
        .await;
        let lock = Mutex::new(());

        let ts = ensure_access_token(
            &mgr, &lock, "ep", IDP_ISS, &idp_ep, AS_ISS, &as_ep, RESOURCE, None, true, None, None,
            None, None, None,
        )
        .await
        .expect("refresh then chain must succeed");

        assert_eq!(ts.access_token, "final-access-token");
        {
            let c = counts.lock().unwrap();
            assert_eq!(c.refresh, 1, "one proactive IdP refresh");
            assert_eq!(c.exchange, 1, "one Step 2 with the fresh ID Token");
            assert_eq!(c.redeem, 1, "one Step 3");
        }
        let rotated = mgr.load_idp(IDP_ISS).await.unwrap().unwrap();
        assert_eq!(
            rotated.id_token, "fresh-id-token",
            "rotated creds persisted"
        );
        assert_eq!(rotated.refresh_token.as_deref(), Some("rotated-refresh"));
        server.abort();
    }

    /// Exchange reports the subject token invalid (stale ID Token with no
    /// recorded expiry): a single refresh-and-retry mints a fresh ID Token and
    /// the second exchange succeeds.
    #[tokio::test]
    async fn ensure_refreshes_reactively_when_exchange_rejects_subject() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp(
            IDP_ISS,
            &idp_creds("stale-id-token", None, Some("idp-refresh")),
        )
        .await
        .unwrap();
        let (idp_ep, as_ep, counts, server) = spawn_token_fixture(
            RefreshOutcome::Succeed {
                new_id_token: "fresh-id-token".to_string(),
            },
            "fresh-id-token",
            0,
        )
        .await;
        let lock = Mutex::new(());

        let ts = ensure_access_token(
            &mgr, &lock, "ep", IDP_ISS, &idp_ep, AS_ISS, &as_ep, RESOURCE, None, true, None, None,
            None, None, None,
        )
        .await
        .expect("reactive refresh then retry must succeed");

        assert_eq!(ts.access_token, "final-access-token");
        let c = counts.lock().unwrap();
        assert_eq!(
            c.exchange, 2,
            "first exchange rejected, retry after refresh"
        );
        assert_eq!(c.refresh, 1, "one reactive refresh");
        assert_eq!(c.redeem, 1, "one Step 3 after the retry");
        drop(c);
        server.abort();
    }

    /// A failing IdP refresh surfaces `ReauthRequired` instead of looping.
    #[tokio::test]
    async fn ensure_surfaces_reauth_required_on_refresh_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp(
            IDP_ISS,
            &idp_creds("old-id-token", Some(1000), Some("idp-refresh")),
        )
        .await
        .unwrap();
        let (idp_ep, as_ep, _counts, server) =
            spawn_token_fixture(RefreshOutcome::Fail, "old-id-token", 0).await;
        let lock = Mutex::new(());

        let err = ensure_access_token(
            &mgr, &lock, "ep", IDP_ISS, &idp_ep, AS_ISS, &as_ep, RESOURCE, None, true, None, None,
            None, None, None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, EmaError::ReauthRequired { .. }),
            "got {err:?}"
        );
        server.abort();
    }

    /// An expired ID Token with no stored refresh token is terminal:
    /// `ReauthRequired`, with no network call.
    #[tokio::test]
    async fn ensure_reauth_required_when_no_refresh_token() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp(IDP_ISS, &idp_creds("old-id-token", Some(1000), None))
            .await
            .unwrap();
        let lock = Mutex::new(());

        let err = ensure_access_token(
            &mgr,
            &lock,
            "ep",
            IDP_ISS,
            "http://127.0.0.1:1/idp/token",
            AS_ISS,
            "http://127.0.0.1:1/as/token",
            RESOURCE,
            None,
            true,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, EmaError::ReauthRequired { .. }),
            "got {err:?}"
        );
    }

    /// Missing IdP credentials are terminal: `ReauthRequired`.
    #[tokio::test]
    async fn ensure_reauth_required_when_no_idp_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let lock = Mutex::new(());

        let err = ensure_access_token(
            &mgr,
            &lock,
            "ep",
            IDP_ISS,
            "http://127.0.0.1:1/idp/token",
            AS_ISS,
            "http://127.0.0.1:1/as/token",
            RESOURCE,
            None,
            true,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, EmaError::ReauthRequired { .. }),
            "got {err:?}"
        );
    }

    /// Concurrent callers coalesce behind `refresh_mutex`: only one runs the
    /// chain; the rest reuse the token it persists. Exactly one Step 2 fires.
    #[tokio::test]
    async fn ensure_coalesces_concurrent_refreshes() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        mgr.save_idp(
            IDP_ISS,
            &idp_creds("good-id-token", Some(now_unix() + 3600), None),
        )
        .await
        .unwrap();
        let (idp_ep, as_ep, counts, server) =
            spawn_token_fixture(RefreshOutcome::Fail, "good-id-token", 100).await;
        let lock = Arc::new(Mutex::new(()));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let mgr = mgr.clone();
            let lock = lock.clone();
            let idp_ep = idp_ep.clone();
            let as_ep = as_ep.clone();
            handles.push(tokio::spawn(async move {
                ensure_access_token(
                    &mgr, &lock, "ep", IDP_ISS, &idp_ep, AS_ISS, &as_ep, RESOURCE, None, true,
                    None, None, None, None, None,
                )
                .await
            }));
        }
        for h in handles {
            let ts = h.await.unwrap().expect("each caller must get a token");
            assert_eq!(ts.access_token, "final-access-token");
        }

        let c = counts.lock().unwrap();
        assert_eq!(
            c.exchange, 1,
            "coalesced: exactly one Step 2 across 5 callers"
        );
        assert_eq!(c.redeem, 1, "coalesced: exactly one Step 3");
        drop(c);
        server.abort();
    }

    /// A stale [`GrantGuard`] (epoch bumped mid-chain, e.g. disconnect or a
    /// replacement login) abandons the chain with `StaleGrant` and leaves NO
    /// endpoint token on disk — the minted token must not resurrect a revoked
    /// grant or overwrite a replacement grant's persisted tokens.
    #[tokio::test]
    async fn stale_grant_guard_abandons_without_persisting() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp(
            IDP_ISS,
            &idp_creds("good-id-token", Some(now_unix() + 3600), None),
        )
        .await
        .unwrap();
        let (idp_ep, as_ep, counts, server) =
            spawn_token_fixture(RefreshOutcome::Fail, "good-id-token", 0).await;
        let lock = Mutex::new(());
        let apply_lock = Mutex::new(());
        let epoch = AtomicU64::new(0);

        // Simulate a disconnect/replacement landing while the chain is in
        // flight: the guard's snapshot (0) no longer matches the counter.
        epoch.store(1, Ordering::Release);

        let err = ensure_access_token(
            &mgr,
            &lock,
            "ep",
            IDP_ISS,
            &idp_ep,
            AS_ISS,
            &as_ep,
            RESOURCE,
            None,
            true,
            None,
            None,
            None,
            None,
            Some(GrantGuard {
                apply_lock: &apply_lock,
                grant_epoch: &epoch,
                expected_epoch: 0,
            }),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, EmaError::StaleGrant), "got {err:?}");
        // The chain ran (the guard fires only at persistence)…
        {
            let c = counts.lock().unwrap();
            assert_eq!(c.exchange, 1, "Step 2 ran before the guard fired");
            assert_eq!(c.redeem, 1, "Step 3 ran before the guard fired");
        }
        // …but nothing reached disk.
        assert!(
            mgr.load("ep").await.unwrap().is_none(),
            "stale chain must not persist an endpoint token"
        );
        server.abort();
    }

    /// A current [`GrantGuard`] (epoch unchanged) persists and returns the
    /// minted token exactly like the guard-less path.
    #[tokio::test]
    async fn current_grant_guard_persists_normally() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp(
            IDP_ISS,
            &idp_creds("good-id-token", Some(now_unix() + 3600), None),
        )
        .await
        .unwrap();
        let (idp_ep, as_ep, _counts, server) =
            spawn_token_fixture(RefreshOutcome::Fail, "good-id-token", 0).await;
        let lock = Mutex::new(());
        let apply_lock = Mutex::new(());
        let epoch = AtomicU64::new(7);

        let ts = ensure_access_token(
            &mgr,
            &lock,
            "ep",
            IDP_ISS,
            &idp_ep,
            AS_ISS,
            &as_ep,
            RESOURCE,
            None,
            true,
            None,
            None,
            None,
            None,
            Some(GrantGuard {
                apply_lock: &apply_lock,
                grant_epoch: &epoch,
                expected_epoch: 7,
            }),
        )
        .await
        .expect("current-epoch chain must succeed");

        assert_eq!(ts.access_token, "final-access-token");
        assert!(
            mgr.load("ep").await.unwrap().unwrap().is_valid(),
            "current chain persists the minted token"
        );
        server.abort();
    }

    // ---- R1: requesting vs resource credential routing -----------------------

    /// Captured request forms for the credential-routing tests.
    #[derive(Clone, Default)]
    struct CapturedForms {
        exchange: Arc<std::sync::Mutex<Option<HashMap<String, String>>>>,
        redeem: Arc<std::sync::Mutex<Option<HashMap<String, String>>>>,
    }

    /// Spawn a mock token server that records the Step 2 (exchange) and Step 3
    /// (redeem) request forms so the credential routing can be asserted. The IdP
    /// endpoint returns a valid ID-JAG; the AS endpoint returns an access token.
    async fn spawn_capturing_fixture(
    ) -> (String, String, CapturedForms, tokio::task::JoinHandle<()>) {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};

        async fn idp_token(
            State(cap): State<CapturedForms>,
            body: String,
        ) -> axum::response::Response {
            let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect();
            *cap.exchange.lock().unwrap() = Some(form);
            let id_jag = make_jwt(serde_json::json!({
                "iss": IDP_ISS,
                "aud": AS_ISS,
                "resource": RESOURCE,
                "sub": "user-123",
                "exp": now_unix() + 600,
            }));
            (
                StatusCode::OK,
                Json(serde_json::json!({ "access_token": id_jag })),
            )
                .into_response()
        }

        async fn as_token(
            State(cap): State<CapturedForms>,
            body: String,
        ) -> axum::response::Response {
            let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect();
            *cap.redeem.lock().unwrap() = Some(form);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "access_token": "final-access-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                })),
            )
                .into_response()
        }

        let cap = CapturedForms::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let router = Router::new()
            .route("/idp/token", post(idp_token))
            .route("/as/token", post(as_token))
            .with_state(cap.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (
            format!("{base}/idp/token"),
            format!("{base}/as/token"),
            cap,
            handle,
        )
    }

    /// Requesting creds authenticate the IdP-facing Step 2 exchange; the
    /// resource creds authenticate the MAS-facing Step 3 redemption.
    #[tokio::test]
    async fn ensure_routes_requesting_creds_to_step2_and_resource_creds_to_step3() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp(
            IDP_ISS,
            &idp_creds("good-id-token", Some(now_unix() + 3600), None),
        )
        .await
        .unwrap();
        let (idp_ep, as_ep, cap, server) = spawn_capturing_fixture().await;
        let lock = Mutex::new(());

        let ts = ensure_access_token(
            &mgr,
            &lock,
            "ep",
            IDP_ISS,
            &idp_ep,
            AS_ISS,
            &as_ep,
            RESOURCE,
            None,
            true,
            Some("req-client"),
            Some("req-secret"),
            Some("res-client"),
            Some("res-secret"),
            None,
        )
        .await
        .expect("chain must succeed");
        assert_eq!(ts.access_token, "final-access-token");

        let exchange = cap.exchange.lock().unwrap().clone().expect("Step 2 ran");
        assert_eq!(
            exchange.get("client_id").map(String::as_str),
            Some("req-client"),
            "Step 2 uses the requesting client_id"
        );
        assert_eq!(
            exchange.get("client_secret").map(String::as_str),
            Some("req-secret"),
            "Step 2 uses the requesting secret"
        );

        let redeem = cap.redeem.lock().unwrap().clone().expect("Step 3 ran");
        assert_eq!(
            redeem.get("client_id").map(String::as_str),
            Some("res-client"),
            "Step 3 uses the resource client_id"
        );
        assert_eq!(
            redeem.get("client_secret").map(String::as_str),
            Some("res-secret"),
            "Step 3 uses the resource secret"
        );
        server.abort();
    }

    /// With no resource credential configured, Step 3 identifies as the
    /// requesting client_id but sends NO secret — and never the requesting
    /// secret (R1). Step 2 still authenticates with the requesting confidential
    /// creds.
    #[tokio::test]
    async fn step3_omits_secret_and_never_uses_requesting_secret_when_no_resource_cred() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp(
            IDP_ISS,
            &idp_creds("good-id-token", Some(now_unix() + 3600), None),
        )
        .await
        .unwrap();
        let (idp_ep, as_ep, cap, server) = spawn_capturing_fixture().await;
        let lock = Mutex::new(());

        ensure_access_token(
            &mgr,
            &lock,
            "ep",
            IDP_ISS,
            &idp_ep,
            AS_ISS,
            &as_ep,
            RESOURCE,
            None,
            true,
            Some("req-client"),
            Some("req-secret"),
            None,
            None,
            None,
        )
        .await
        .expect("chain must succeed");

        let exchange = cap.exchange.lock().unwrap().clone().expect("Step 2 ran");
        assert_eq!(
            exchange.get("client_secret").map(String::as_str),
            Some("req-secret"),
            "Step 2 still authenticates with the requesting secret"
        );

        let redeem = cap.redeem.lock().unwrap().clone().expect("Step 3 ran");
        assert_eq!(
            redeem.get("client_id").map(String::as_str),
            Some("req-client"),
            "Step 3 identifies as the requesting client_id when no resource id is set"
        );
        assert!(
            !redeem.contains_key("client_secret"),
            "Step 3 must omit client_secret when no resource credential is configured"
        );
        assert_ne!(
            redeem.get("client_secret").map(String::as_str),
            Some("req-secret"),
            "Step 3 must never send the requesting secret"
        );
        server.abort();
    }
}
