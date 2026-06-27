//! Enterprise-Managed Authorization (EMA, END-18) end-to-end integration tests.
//!
//! These are the acceptance-gate tests for the engineering spec §7: they drive
//! the *real* [`OAuthAdapter`] (initialize → ID-JAG chain → inner MCP handshake
//! → `tools/call`) against axum mock fixtures for the IdP token endpoint
//! (RFC 8693 token-exchange + OIDC `refresh_token` grant), the resource AS token
//! endpoint (RFC 7523 jwt-bearer), and the resource MCP server. They mirror the
//! `spawn_*_fixture` pattern used in `oauth_integration.rs` / `ema.rs`.
//!
//! Unit-level coverage of the grant clients and `ensure_access_token`
//! orchestration lives in `src/oauth/ema.rs`; here we pin the adapter *wiring*:
//! the full chain (M4–M8), silent refresh on a 401 (M9), IdP-token refresh (M9),
//! the no-silent-loop `ReauthRequired` surface (M9), the SSRF guard (M11), and
//! coalesced concurrent refreshes (S2).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};

use endara_relay::adapter::oauth::{EmaConfig, OAuthAdapter, OAuthAdapterConfig, OAuthState};
use endara_relay::adapter::McpAdapter;
use endara_relay::config::{Config, ConfigOrganization, RelayConfig};
use endara_relay::management::{management_routes, ManagementState};
use endara_relay::registry::AdapterRegistry;
use endara_relay::token_manager::{IdpCredentials, TokenManager, TokenSet};

/// Logical IdP issuer (also the IdP-credential store key in v1).
const IDP_ISS: &str = "https://acme.okta.com";
/// Logical resource AS issuer (RFC 8693 `audience`; ID-JAG `aud` claim).
const AS_ISS: &str = "https://as.example.com";
/// Logical MCP resource identifier (ID-JAG `resource` claim).
const RESOURCE: &str = "https://api.example.com/mcp/";

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Build an unsigned (`alg=none`) compact JWS carrying `claims`. The relay does
/// not verify the ID-JAG signature in v1 (design D4/S1), so a structural JWT is
/// enough for the validation + redemption path.
fn make_jwt(claims: Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    format!("{header}.{payload}.sig")
}

/// Per-grant request counters recorded by the mock fixture.
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
    /// Refresh succeeds, returning `new_id_token` (which the exchange leg then
    /// accepts as the only valid subject token).
    Succeed { new_id_token: String },
}

/// Shared mock state. `accept_id_token` is the single subject token the exchange
/// leg accepts (a successful refresh rotates it); `live_token` is the only
/// bearer the MCP server's `tools/call` accepts (each redemption rotates it, so
/// a stale access token yields a 401 that drives the silent-refresh path).
#[derive(Clone)]
struct Fx {
    counts: Arc<StdMutex<FxCounts>>,
    accept_id_token: Arc<StdMutex<String>>,
    refresh: RefreshOutcome,
    exchange_delay_ms: u64,
    access_seq: Arc<AtomicU64>,
    live_token: Arc<StdMutex<String>>,
    as_expires_in: u64,
}

/// Endpoints exposed by [`spawn_ema_fixture`].
struct FxUrls {
    idp_token_endpoint: String,
    as_token_endpoint: String,
    mcp_url: String,
}

/// Spawn the combined IdP + AS + MCP mock on `127.0.0.1:0`. Returns the resolved
/// endpoint URLs, the shared [`Fx`] handle (for counter/state assertions), and
/// the server task handle.
async fn spawn_ema_fixture(fx: Fx) -> (FxUrls, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/idp/token", post(idp_token))
        .route("/as/token", post(as_token))
        .route("/mcp", post(mcp))
        .with_state(fx);
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let urls = FxUrls {
        idp_token_endpoint: format!("{base}/idp/token"),
        as_token_endpoint: format!("{base}/as/token"),
        mcp_url: format!("{base}/mcp"),
    };
    (urls, handle)
}

fn parse_form(body: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

/// Mock IdP token endpoint: serves both the RFC 8693 token-exchange (Step 2)
/// and the OIDC `refresh_token` grant (M9), distinguished by `grant_type`.
async fn idp_token(State(fx): State<Fx>, body: String) -> Response {
    let form = parse_form(&body);
    let grant = form
        .get("grant_type")
        .map(String::as_str)
        .unwrap_or_default();

    if grant == "refresh_token" {
        fx.counts.lock().unwrap().refresh += 1;
        return match &fx.refresh {
            RefreshOutcome::Fail => (
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_grant","error_description":"refresh expired"}"#,
            )
                .into_response(),
            RefreshOutcome::Succeed { new_id_token } => {
                *fx.accept_id_token.lock().unwrap() = new_id_token.clone();
                let resp = json!({
                    "id_token": new_id_token,
                    "refresh_token": "rotated-refresh",
                    "expires_in": 3600,
                });
                (StatusCode::OK, Json(resp)).into_response()
            }
        };
    }

    // Token-exchange (Step 2): only the currently-accepted subject token works.
    fx.counts.lock().unwrap().exchange += 1;
    if fx.exchange_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(fx.exchange_delay_ms)).await;
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
    let id_jag = make_jwt(json!({
        "iss": IDP_ISS,
        "aud": AS_ISS,
        "resource": RESOURCE,
        "sub": "user-123",
        "exp": now_unix() + 600,
    }));
    (StatusCode::OK, Json(json!({ "access_token": id_jag }))).into_response()
}

/// Mock resource AS token endpoint (RFC 7523 jwt-bearer, Step 3). Each call
/// mints a fresh `access-{n}` token and rotates `live_token` so the MCP server
/// only accepts the most recently minted token.
async fn as_token(State(fx): State<Fx>, _body: String) -> Response {
    fx.counts.lock().unwrap().redeem += 1;
    let n = fx.access_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let access = format!("access-{n}");
    *fx.live_token.lock().unwrap() = access.clone();
    let resp = json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": fx.as_expires_in,
        "scope": "mcp",
    });
    (StatusCode::OK, Json(resp)).into_response()
}

/// Mock resource MCP server. `initialize`/`tools/list`/notifications are
/// ungated so the handshake and heartbeat probe always succeed; `tools/call` is
/// gated on `live_token` so a stale bearer returns HTTP 401 (the seam the
/// adapter's silent-refresh path keys on).
async fn mcp(State(fx): State<Fx>, headers: HeaderMap, body: String) -> Response {
    let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    match method {
        "initialize" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "ema-mcp", "version": "0.0.1"},
            },
        }))
        .into_response(),
        "tools/list" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": [{
                "name": "echo",
                "description": "echo",
                "inputSchema": {"type": "object"},
            }]},
        }))
        .into_response(),
        "tools/call" => {
            let want = format!("Bearer {}", fx.live_token.lock().unwrap());
            let got = headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if got != want {
                return (StatusCode::UNAUTHORIZED, "token expired").into_response();
            }
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"content": [{"type": "text", "text": "ok"}]},
            }))
            .into_response()
        }
        _ => {
            // notifications/initialized and other JSON-RPC notifications.
            if req.get("id").is_none() {
                (StatusCode::ACCEPTED, "").into_response()
            } else {
                Json(json!({"jsonrpc": "2.0", "id": id, "result": {}})).into_response()
            }
        }
    }
}

/// Build an `auth.type="ema"` adapter config pointed at the fixture endpoints.
/// `allow_insecure_oauth` is `true` so the loopback fixture URLs pass the SSRF
/// guard; the SSRF test overrides it to `false`.
fn ema_config(urls: &FxUrls) -> OAuthAdapterConfig {
    OAuthAdapterConfig {
        endpoint_name: "ema-ep".to_string(),
        url: urls.mcp_url.clone(),
        token_endpoint_url: urls.as_token_endpoint.clone(),
        client_id: "https://endara.ai/oauth/client-metadata.json".to_string(),
        client_secret: None,
        heartbeat_interval_secs: 3600,
        probe_timeout_secs: 10,
        probe_failure_threshold: 3,
        server_type_override: None,
        allow_insecure_oauth: true,
        ema: Some(EmaConfig {
            idp_key: IDP_ISS.to_string(),
            idp_issuer: IDP_ISS.to_string(),
            idp_authorization_endpoint: format!("{IDP_ISS}/authorize"),
            idp_token_endpoint: urls.idp_token_endpoint.clone(),
            as_issuer: AS_ISS.to_string(),
            as_token_endpoint: urls.as_token_endpoint.clone(),
            resource: RESOURCE.to_string(),
            client_id: None,
        }),
    }
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

/// An access `TokenSet` whose `expires_at` is in the past so `is_valid()` is
/// false and `ensure_access_token` is forced to re-mint.
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

/// Default fixture state: the exchange accepts `accept_id_token`, the refresh
/// grant behaves per `refresh`, redemptions mint long-lived (`3600s`) tokens,
/// and the exchange responds without delay.
fn fx(refresh: RefreshOutcome, accept_id_token: &str) -> Fx {
    Fx {
        counts: Arc::new(StdMutex::new(FxCounts::default())),
        accept_id_token: Arc::new(StdMutex::new(accept_id_token.to_string())),
        refresh,
        exchange_delay_ms: 0,
        access_seq: Arc::new(AtomicU64::new(0)),
        live_token: Arc::new(StdMutex::new(String::new())),
        as_expires_in: 3600,
    }
}

// ---------------------------------------------------------------------------
// Happy path (M4–M8): mock IdP ID Token → token-exchange ID-JAG → AS access
// token → valid TokenSet → a `tools/call` through `OAuthAdapter` succeeds.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn happy_path_full_chain_tools_call_succeeds() {
    let state = fx(RefreshOutcome::Fail, "good-id-token");
    let (urls, server) = spawn_ema_fixture(state.clone()).await;

    let tmp = tempfile::tempdir().unwrap();
    let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
    tm.save_idp(
        IDP_ISS,
        &idp_creds("good-id-token", Some(now_unix() + 3600), None),
    )
    .await
    .unwrap();

    let mut adapter = OAuthAdapter::new(ema_config(&urls), tm.clone());
    adapter.initialize().await.unwrap();

    // The full ID-JAG chain ran once and left the adapter authenticated.
    assert_eq!(
        *adapter.shared_inner().state.read().await,
        OAuthState::Authenticated,
        "EMA chain must leave the adapter Authenticated"
    );
    let persisted = tm.load("ema-ep").await.unwrap().expect("token persisted");
    assert!(persisted.is_valid(), "minted access token must be valid");

    let result = adapter.call_tool("echo", json!({})).await.unwrap();
    assert_eq!(result["content"][0]["text"].as_str(), Some("ok"));

    {
        let c = state.counts.lock().unwrap();
        assert_eq!(c.exchange, 1, "exactly one Step 2");
        assert_eq!(c.redeem, 1, "exactly one Step 3");
        assert_eq!(c.refresh, 0, "no IdP refresh on the happy path");
    }
    server.abort();
}

// ---------------------------------------------------------------------------
// Access-token expiry (M9): a 401 on `tools/call` drives a silent Steps 2+3
// re-run (no IdP refresh, no new SSO) and the retried call succeeds.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn access_token_expiry_silently_reruns_chain_via_call_tool() {
    let state = fx(RefreshOutcome::Fail, "good-id-token");
    let (urls, server) = spawn_ema_fixture(state.clone()).await;

    let tmp = tempfile::tempdir().unwrap();
    let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
    tm.save_idp(
        IDP_ISS,
        &idp_creds("good-id-token", Some(now_unix() + 3600), None),
    )
    .await
    .unwrap();

    let mut adapter = OAuthAdapter::new(ema_config(&urls), tm.clone());
    adapter.initialize().await.unwrap();

    // Simulate access-token expiry at the resource server: the live token no
    // longer matches the bearer the inner adapter holds, and the persisted
    // TokenSet is expired so the chain is forced to re-mint.
    *state.live_token.lock().unwrap() = "__expired__".to_string();
    tm.save("ema-ep", &expired_token_set()).await.unwrap();

    let result = adapter.call_tool("echo", json!({})).await.unwrap();
    assert_eq!(
        result["content"][0]["text"].as_str(),
        Some("ok"),
        "retry after silent refresh must succeed"
    );

    {
        let c = state.counts.lock().unwrap();
        assert_eq!(c.exchange, 2, "initial + silent re-run = two Step 2");
        assert_eq!(c.redeem, 2, "initial + silent re-run = two Step 3");
        assert_eq!(
            c.refresh, 0,
            "silent refresh must NOT touch the IdP refresh grant"
        );
    }
    server.abort();
}

// ---------------------------------------------------------------------------
// ID-Token expiry (M9): a known-expired ID Token triggers a `refresh_token`
// grant at the IdP, the rotated credentials are persisted, then Steps 2+3 run.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn id_token_expiry_refreshes_then_runs_chain() {
    let state = fx(
        RefreshOutcome::Succeed {
            new_id_token: "fresh-id-token".to_string(),
        },
        "old-id-token",
    );
    let (urls, server) = spawn_ema_fixture(state.clone()).await;

    let tmp = tempfile::tempdir().unwrap();
    let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
    tm.save_idp(
        IDP_ISS,
        &idp_creds("old-id-token", Some(1000), Some("idp-refresh")),
    )
    .await
    .unwrap();

    let adapter = OAuthAdapter::new(ema_config(&urls), tm.clone());
    let inner = adapter.shared_inner();
    let tokens = inner
        .do_token_refresh()
        .await
        .expect("refresh then chain must succeed");
    assert!(tokens.is_valid());

    {
        let c = state.counts.lock().unwrap();
        assert_eq!(c.refresh, 1, "one proactive IdP refresh");
        assert_eq!(c.exchange, 1, "one Step 2 with the fresh ID Token");
        assert_eq!(c.redeem, 1, "one Step 3");
    }
    let rotated = tm.load_idp(IDP_ISS).await.unwrap().unwrap();
    assert_eq!(
        rotated.id_token, "fresh-id-token",
        "rotated creds persisted"
    );
    assert_eq!(rotated.refresh_token.as_deref(), Some("rotated-refresh"));
    server.abort();
}

// ---------------------------------------------------------------------------
// Refresh-token failure (M9): a failed IdP refresh surfaces `ReauthRequired`
// (mapped to `AuthRequired`) with exactly one refresh attempt — no silent loop.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn refresh_token_failure_surfaces_reauth_required_without_loop() {
    let state = fx(RefreshOutcome::Fail, "old-id-token");
    let (urls, server) = spawn_ema_fixture(state.clone()).await;

    let tmp = tempfile::tempdir().unwrap();
    let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
    tm.save_idp(
        IDP_ISS,
        &idp_creds("old-id-token", Some(1000), Some("idp-refresh")),
    )
    .await
    .unwrap();

    let adapter = OAuthAdapter::new(ema_config(&urls), tm.clone());
    let inner = adapter.shared_inner();
    let err = inner.do_token_refresh().await.unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("re-auth")
            || err.to_string().to_lowercase().contains("sso"),
        "expected a re-auth-required error, got: {err}"
    );
    assert_eq!(
        *inner.state.read().await,
        OAuthState::AuthRequired,
        "a re-auth-required outcome must be terminal (AuthRequired)"
    );
    {
        let c = state.counts.lock().unwrap();
        assert_eq!(c.refresh, 1, "exactly one refresh attempt — no silent loop");
        assert_eq!(c.exchange, 0, "no exchange once the refresh fails");
    }
    server.abort();
}

// ---------------------------------------------------------------------------
// SSRF (M11): an IdP token URL that resolves to loopback is rejected by the
// url_guard when `allow_insecure_oauth` is false, before any request is sent.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn ssrf_blocked_idp_url_is_rejected_by_url_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
    tm.save_idp(
        IDP_ISS,
        &idp_creds("good-id-token", Some(now_unix() + 3600), None),
    )
    .await
    .unwrap();

    // Loopback IdP token endpoint over https + secure mode ⇒ AddressNotAllowed.
    let mut config = ema_config(&FxUrls {
        idp_token_endpoint: "https://127.0.0.1:9/idp/token".to_string(),
        as_token_endpoint: "https://127.0.0.1:9/as/token".to_string(),
        mcp_url: "https://127.0.0.1:9/mcp".to_string(),
    });
    config.allow_insecure_oauth = false;

    let adapter = OAuthAdapter::new(config, tm.clone());
    let inner = adapter.shared_inner();
    let err = inner.do_token_refresh().await.unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("ssrf"),
        "expected an SSRF-guard rejection, got: {err}"
    );
    assert_eq!(
        *inner.state.read().await,
        OAuthState::ConnectionFailed,
        "a guard rejection is transport-class (ConnectionFailed)"
    );
}

// ---------------------------------------------------------------------------
// Concurrency (S2): N concurrent refreshes coalesce behind the adapter's
// per-endpoint refresh mutex into a single exchange + redemption.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn concurrent_refreshes_coalesce_into_single_exchange() {
    let mut state = fx(RefreshOutcome::Fail, "good-id-token");
    state.exchange_delay_ms = 100;
    let (urls, server) = spawn_ema_fixture(state.clone()).await;

    let tmp = tempfile::tempdir().unwrap();
    let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
    tm.save_idp(
        IDP_ISS,
        &idp_creds("good-id-token", Some(now_unix() + 3600), None),
    )
    .await
    .unwrap();

    let adapter = OAuthAdapter::new(ema_config(&urls), tm.clone());
    let inner = adapter.shared_inner();

    let mut handles = Vec::new();
    for _ in 0..5 {
        let inner = inner.clone();
        handles.push(tokio::spawn(async move { inner.do_token_refresh().await }));
    }
    for h in handles {
        let ts = h.await.unwrap().expect("each caller must get a token");
        assert!(ts.is_valid());
    }

    let c = state.counts.lock().unwrap();
    assert_eq!(
        c.exchange, 1,
        "coalesced: exactly one Step 2 across 5 callers"
    );
    assert_eq!(c.redeem, 1, "coalesced: exactly one Step 3");
    drop(c);
    server.abort();
}

// ---------------------------------------------------------------------------
// Wave 2 (END-19) credential pooling: two EMA endpoints that reference the same
// org share ONE pooled IdP credential, keyed by the org name (`idp_key`) rather
// than the issuer. A single `.idp.json` drives both adapters' EMA chains; each
// persists its OWN resource `TokenSet` under its own endpoint name.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn org_pool_two_endpoints_share_one_idp_credential() {
    const ORG_KEY: &str = "Acme Corp";

    let state = fx(RefreshOutcome::Fail, "good-id-token");
    let (urls, server) = spawn_ema_fixture(state.clone()).await;

    let tmp = tempfile::tempdir().unwrap();
    let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));

    // ONE IdP credential, saved under the ORG key (not the raw issuer). Its
    // `idp_issuer` remains the real issuer URL the EMA chain validates against.
    tm.save_idp(
        ORG_KEY,
        &idp_creds("good-id-token", Some(now_unix() + 3600), None),
    )
    .await
    .unwrap();

    // Two endpoints in the same org: distinct endpoint names, same pooled key.
    let mut cfg_a = ema_config(&urls);
    cfg_a.endpoint_name = "github-acme".to_string();
    cfg_a.ema.as_mut().unwrap().idp_key = ORG_KEY.to_string();
    let mut cfg_b = ema_config(&urls);
    cfg_b.endpoint_name = "jira-acme".to_string();
    cfg_b.ema.as_mut().unwrap().idp_key = ORG_KEY.to_string();

    let mut adapter_a = OAuthAdapter::new(cfg_a, tm.clone());
    let mut adapter_b = OAuthAdapter::new(cfg_b, tm.clone());
    adapter_a.initialize().await.unwrap();
    adapter_b.initialize().await.unwrap();

    assert_eq!(
        *adapter_a.shared_inner().state.read().await,
        OAuthState::Authenticated
    );
    assert_eq!(
        *adapter_b.shared_inner().state.read().await,
        OAuthState::Authenticated
    );

    // Each endpoint persisted its OWN resource TokenSet from the shared cred...
    let tok_a = tm
        .load("github-acme")
        .await
        .unwrap()
        .expect("endpoint A token");
    let tok_b = tm
        .load("jira-acme")
        .await
        .unwrap()
        .expect("endpoint B token");
    assert!(tok_a.is_valid() && tok_b.is_valid());
    assert_ne!(
        tok_a.access_token, tok_b.access_token,
        "two endpoints mint distinct resource access tokens"
    );

    // ...from the SINGLE pooled IdP credential: exactly one `.idp.json` on disk,
    // resolvable by the org key.
    let idp_file_count = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".idp.json"))
        .count();
    assert_eq!(
        idp_file_count, 1,
        "org pool must use exactly one IdP credential file"
    );
    assert!(
        tm.load_idp(ORG_KEY).await.unwrap().is_some(),
        "the pooled credential resolves by org key"
    );

    server.abort();
}

// ---------------------------------------------------------------------------
// Wave 4 (END-19): EMA capability-probe API.
//
// `POST /api/organizations/{org}/probe` runs, per desktop-supplied resource,
// RFC 9728 → 8414 discovery + an RFC 8693 ID-JAG exchange against the org IdP
// and maps the outcome to accessible / denied / unreachable. These tests drive
// the real management router (served over TCP, exercised with `reqwest`) against
// a combined discovery + IdP-token mock, and pin the "persist nothing" rule.
// ---------------------------------------------------------------------------

/// Mock state for the probe fixture: counts ID-JAG exchange hits and selects a
/// granting vs. denying IdP token endpoint.
#[derive(Clone)]
struct ProbeFx {
    /// Number of RFC 8693 token-exchange (ID-JAG) hits on `/token`.
    exchange: Arc<AtomicU64>,
    /// When true, `/token` returns HTTP 400 `access_denied` (→ denied).
    deny: bool,
}

/// Build a mock origin issuer from the request `Host` header so the AS metadata
/// advertises an issuer matching its own origin (RFC 8414 §3.3), exactly like
/// the management-layer org mocks.
fn probe_mock_issuer(headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    format!("http://{host}")
}

/// Spawn a single-origin mock that serves RFC 9728 protected-resource metadata,
/// RFC 8414 AS metadata (used for BOTH the resource AS and the org IdP issuer),
/// and the IdP token-exchange endpoint. Returns its base URL and task handle.
async fn spawn_probe_fixture(fx: ProbeFx) -> (String, tokio::task::JoinHandle<()>) {
    async fn protected_resource(headers: HeaderMap) -> Response {
        let issuer = probe_mock_issuer(&headers);
        Json(json!({
            "resource": issuer,
            "authorization_servers": [issuer],
        }))
        .into_response()
    }
    async fn auth_server(headers: HeaderMap) -> Response {
        let issuer = probe_mock_issuer(&headers);
        Json(json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "code_challenge_methods_supported": ["S256"],
        }))
        .into_response()
    }
    async fn token(State(fx): State<ProbeFx>, _body: String) -> Response {
        fx.exchange.fetch_add(1, Ordering::SeqCst);
        if fx.deny {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"error":"access_denied","error_description":"group not entitled"}"#,
            )
                .into_response();
        }
        // The probe discards the ID-JAG, so any structural token works.
        Json(json!({ "access_token": make_jwt(json!({"sub": "u"})) })).into_response()
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route("/.well-known/oauth-authorization-server", get(auth_server))
        .route("/token", post(token))
        .with_state(fx);
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (base, handle)
}

/// Build a `ManagementState` with one organization (`Acme`, IdP issuer
/// `idp_issuer`) and a token manager rooted at `token_dir`, with insecure
/// (loopback) OAuth permitted so the mock fixtures pass the SSRF guard.
fn probe_management_state(
    idp_issuer: &str,
    token_dir: std::path::PathBuf,
) -> (ManagementState, Arc<TokenManager>) {
    let tm = Arc::new(TokenManager::new(token_dir));
    let cfg = Config {
        relay: RelayConfig {
            allow_insecure_oauth: Some(true),
            ..Default::default()
        },
        endpoints: Vec::new(),
        profiles: None,
        organizations: vec![ConfigOrganization {
            name: "Acme".to_string(),
            provider: "custom".to_string(),
            idp: idp_issuer.to_string(),
            client_id: None,
        }],
    };
    let state = ManagementState {
        registry: Arc::new(AdapterRegistry::new()),
        config: Arc::new(tokio::sync::RwLock::new(cfg)),
        start_time: std::time::Instant::now(),
        config_path: None,
        oauth_flow_manager: None,
        relay_port: 9400,
        oauth_adapter_inners: None,
        token_manager: Some(tm.clone()),
        setup_manager: None,
        profile_registry: None,
        event_bus: None,
    };
    (state, tm)
}

/// Serve `management_routes` over an ephemeral TCP port and return its base URL.
async fn serve_management(state: ManagementState) -> (String, tokio::task::JoinHandle<()>) {
    let app = management_routes(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (base, handle)
}

/// Seed the org-keyed pooled IdP credential (Wave 2) the probe loads.
async fn seed_org_idp(tm: &TokenManager, idp_issuer: &str) {
    tm.save_idp(
        "Acme",
        &IdpCredentials {
            idp_issuer: idp_issuer.to_string(),
            id_token: "good-id-token".to_string(),
            refresh_token: None,
            id_token_expires_at: Some(now_unix() + 3600),
            obtained_at: now_unix(),
        },
    )
    .await
    .unwrap();
}

/// POST a probe request and return the parsed JSON body.
async fn post_probe(mgmt_base: &str, resources: &[&str]) -> Value {
    let resp = reqwest::Client::new()
        .post(format!("{mgmt_base}/api/organizations/Acme/probe"))
        .json(&json!({ "resources": resources }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "probe must return 200"
    );
    resp.json().await.unwrap()
}

#[tokio::test]
async fn ema_probe_accessible_when_idp_grants() {
    let fx = ProbeFx {
        exchange: Arc::new(AtomicU64::new(0)),
        deny: false,
    };
    let (base, fixture) = spawn_probe_fixture(fx).await;
    let tmp = tempfile::tempdir().unwrap();
    let (state, tm) = probe_management_state(&base, tmp.path().to_path_buf());
    seed_org_idp(&tm, &base).await;
    let (mgmt, server) = serve_management(state).await;

    let body = post_probe(&mgmt, &[&base]).await;
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["resource"], base);
    assert_eq!(results[0]["status"], "accessible");
    assert_eq!(results[0]["server_as_issuer"], base);

    server.abort();
    fixture.abort();
}

#[tokio::test]
async fn ema_probe_denied_on_access_denied() {
    let fx = ProbeFx {
        exchange: Arc::new(AtomicU64::new(0)),
        deny: true,
    };
    let (base, fixture) = spawn_probe_fixture(fx).await;
    let tmp = tempfile::tempdir().unwrap();
    let (state, tm) = probe_management_state(&base, tmp.path().to_path_buf());
    seed_org_idp(&tm, &base).await;
    let (mgmt, server) = serve_management(state).await;

    let body = post_probe(&mgmt, &[&base]).await;
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results[0]["status"], "denied");
    // Discovery succeeded, so the resource AS issuer is still reported.
    assert_eq!(results[0]["server_as_issuer"], base);

    server.abort();
    fixture.abort();
}

#[tokio::test]
async fn ema_probe_unreachable_on_discovery_failure() {
    // The org IdP fixture is reachable (so IdP discovery resolves), but the
    // probed resource points at a dead loopback port: discovery fails.
    let fx = ProbeFx {
        exchange: Arc::new(AtomicU64::new(0)),
        deny: false,
    };
    let (base, fixture) = spawn_probe_fixture(fx.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (state, tm) = probe_management_state(&base, tmp.path().to_path_buf());
    seed_org_idp(&tm, &base).await;
    let (mgmt, server) = serve_management(state).await;

    let body = post_probe(&mgmt, &["http://127.0.0.1:1/mcp"]).await;
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results[0]["status"], "unreachable");
    assert!(
        results[0].get("server_as_issuer").is_none(),
        "discovery failed, so no AS issuer is known"
    );
    assert_eq!(
        fx.exchange.load(Ordering::SeqCst),
        0,
        "a discovery failure must never reach the ID-JAG exchange"
    );

    server.abort();
    fixture.abort();
}

#[tokio::test]
async fn ema_probe_persists_no_token_files() {
    let fx = ProbeFx {
        exchange: Arc::new(AtomicU64::new(0)),
        deny: false,
    };
    let (base, fixture) = spawn_probe_fixture(fx).await;
    let tmp = tempfile::tempdir().unwrap();
    let (state, tm) = probe_management_state(&base, tmp.path().to_path_buf());
    seed_org_idp(&tm, &base).await;
    let (mgmt, server) = serve_management(state).await;

    let body = post_probe(&mgmt, &[&base]).await;
    assert_eq!(body["results"][0]["status"], "accessible");

    // A probe mints an ID-JAG then discards it: NO access-token files are
    // written, and the only credential file is the seeded org `.idp.json`.
    let entries: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let token_files = entries
        .iter()
        .filter(|n| n.ends_with(".token.json"))
        .count();
    let idp_files = entries.iter().filter(|n| n.ends_with(".idp.json")).count();
    assert_eq!(
        token_files, 0,
        "a probe must not persist any resource access-token files, found: {entries:?}"
    );
    assert_eq!(
        idp_files, 1,
        "only the pre-seeded org IdP credential should exist, found: {entries:?}"
    );

    server.abort();
    fixture.abort();
}

#[tokio::test]
async fn ema_probe_cache_hit_skips_second_exchange() {
    let fx = ProbeFx {
        exchange: Arc::new(AtomicU64::new(0)),
        deny: false,
    };
    let (base, fixture) = spawn_probe_fixture(fx.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (state, tm) = probe_management_state(&base, tmp.path().to_path_buf());
    seed_org_idp(&tm, &base).await;
    let (mgmt, server) = serve_management(state).await;

    let first = post_probe(&mgmt, &[&base]).await;
    assert_eq!(first["results"][0]["status"], "accessible");
    assert_eq!(
        fx.exchange.load(Ordering::SeqCst),
        1,
        "first probe runs exactly one ID-JAG exchange"
    );

    // Second probe of the same (org, resource) within the TTL is served from
    // cache: the ID-JAG exchange is NOT re-run.
    let second = post_probe(&mgmt, &[&base]).await;
    assert_eq!(second["results"][0]["status"], "accessible");
    assert_eq!(
        fx.exchange.load(Ordering::SeqCst),
        1,
        "cache hit within TTL must skip the re-exchange"
    );

    server.abort();
    fixture.abort();
}
