//! OAuth for upstream MCP servers, per the MCP authorization spec:
//! protected-resource discovery (RFC 9728), authorization-server metadata
//! (RFC 8414 / OIDC), dynamic client registration (RFC 7591), authorization
//! code + PKCE S256, resource indicators (RFC 8707) and refresh tokens.
//!
//! One record per server lives in KV under `oauth:{endpoint id}`, with the
//! client secret and tokens encrypted. Pending sign-ins live under
//! `oauth-flow:{state}` for ten minutes and are single-use.

use crate::config::Endpoint;
use crate::store::Crypto;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use futures_util::future::{select, Either};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;
use worker::url::form_urlencoded;
use worker::wasm_bindgen::JsValue;
use worker::{Date, Delay, Fetch, Headers, KvStore, Method, Request, RequestInit, Url};

const FLOW_TTL_SECS: u64 = 600;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Refresh this long before the access token expires.
const REFRESH_MARGIN_MS: u64 = 60_000;
const CLIENT_NAME: &str = "Endara Relay";

/// (server url, access token, expires_at ms)
type CachedToken = (String, String, Option<u64>);

thread_local! {
    /// endpoint id -> cached access token
    static TOKENS: RefCell<HashMap<String, CachedToken>> = RefCell::new(HashMap::new());
}

fn now_ms() -> u64 {
    Date::now().as_millis()
}

#[derive(Debug)]
pub enum OAuthError {
    /// The user must (re-)authorize in the web app.
    NeedsSignIn(String),
    Other(String),
}

impl OAuthError {
    pub fn message(&self) -> String {
        match self {
            OAuthError::NeedsSignIn(m) | OAuthError::Other(m) => m.clone(),
        }
    }
}

impl From<String> for OAuthError {
    fn from(s: String) -> Self {
        OAuthError::Other(s)
    }
}

/// Persisted per-server OAuth state. `*_enc` fields are encrypted.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    url: String,
    redirect_uri: String,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    resource: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    authorization_response_iss: bool,
    client_id: String,
    #[serde(default)]
    client_secret_enc: Option<String>,
    #[serde(default)]
    token_auth_method: String,
    #[serde(default)]
    access_token_enc: Option<String>,
    #[serde(default)]
    refresh_token_enc: Option<String>,
    #[serde(default)]
    expires_at: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Flow {
    endpoint_id: String,
    endpoint_name: String,
    verifier: String,
    record: Record,
}

#[derive(Debug, Serialize)]
pub struct SignInState {
    pub signed_in: bool,
    pub expires_at: Option<u64>,
}

#[derive(Clone)]
pub struct OAuth {
    kv: KvStore,
    crypto: Crypto,
    allow_insecure: bool,
}

fn record_key(endpoint_id: &str) -> String {
    format!("oauth:{endpoint_id}")
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let _ = getrandom::getrandom(&mut buf);
    B64URL.encode(buf)
}

impl OAuth {
    pub fn new(kv: KvStore, crypto: Crypto, allow_insecure: bool) -> Self {
        OAuth {
            kv,
            crypto,
            allow_insecure,
        }
    }

    // ---- storage ---------------------------------------------------------

    async fn load(&self, endpoint_id: &str) -> Option<Record> {
        let text = self.kv.get(&record_key(endpoint_id)).text().await.ok()??;
        serde_json::from_str(&text).ok()
    }

    async fn save(&self, endpoint_id: &str, record: &Record) -> Result<(), String> {
        let body = serde_json::to_string(record).map_err(|e| e.to_string())?;
        self.kv
            .put(&record_key(endpoint_id), body)
            .map_err(|e| e.to_string())?
            .execute()
            .await
            .map_err(|e| e.to_string())
    }

    /// Drop everything for a server (it was removed).
    pub async fn forget(&self, endpoint_id: &str) {
        TOKENS.with(|t| t.borrow_mut().remove(endpoint_id));
        let _ = self.kv.delete(&record_key(endpoint_id)).await;
    }

    /// Sign out: drop the tokens but keep the registered client.
    pub async fn sign_out(&self, endpoint_id: &str) -> Result<(), String> {
        TOKENS.with(|t| t.borrow_mut().remove(endpoint_id));
        if let Some(mut record) = self.load(endpoint_id).await {
            record.access_token_enc = None;
            record.refresh_token_enc = None;
            record.expires_at = None;
            self.save(endpoint_id, &record).await?;
        }
        Ok(())
    }

    pub async fn state(&self, ep: &Endpoint) -> SignInState {
        match self.load(&ep.id).await {
            Some(r) if r.url == ep.url && r.access_token_enc.is_some() => SignInState {
                signed_in: true,
                expires_at: r.expires_at,
            },
            _ => SignInState {
                signed_in: false,
                expires_at: None,
            },
        }
    }

    // ---- tokens for upstream requests -----------------------------------

    /// A valid access token for `ep`, refreshing it if it is about to expire.
    /// `force_refresh` is used after the upstream rejected the current token.
    pub async fn access_token(
        &self,
        ep: &Endpoint,
        force_refresh: bool,
    ) -> Result<String, OAuthError> {
        if !force_refresh {
            let cached = TOKENS.with(|t| t.borrow().get(&ep.id).cloned());
            if let Some((url, token, exp)) = cached {
                if url == ep.url && exp.is_none_or(|e| e > now_ms() + REFRESH_MARGIN_MS) {
                    return Ok(token);
                }
            }
        }
        let sign_in = || {
            OAuthError::NeedsSignIn(format!(
                "'{}' needs sign-in: open the relay web app and click Sign in",
                ep.name
            ))
        };
        let mut record = match self.load(&ep.id).await {
            Some(r) if r.url == ep.url => r,
            _ => return Err(sign_in()),
        };
        let Some(access_enc) = record.access_token_enc.clone() else {
            return Err(sign_in());
        };
        let fresh = record
            .expires_at
            .is_none_or(|e| e > now_ms() + REFRESH_MARGIN_MS);
        if fresh && !force_refresh {
            let token = self.crypto.open_str(&access_enc)?;
            TOKENS.with(|t| {
                t.borrow_mut().insert(
                    ep.id.clone(),
                    (ep.url.clone(), token.clone(), record.expires_at),
                )
            });
            return Ok(token);
        }
        let Some(refresh_enc) = record.refresh_token_enc.clone() else {
            return Err(sign_in());
        };
        let refresh_token = self.crypto.open_str(&refresh_enc)?;
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token),
            ("resource", record.resource.clone()),
        ];
        if let Some(scope) = &record.scope {
            form.push(("scope", scope.clone()));
        }
        match self.token_request(&record, form).await {
            Ok(tokens) => {
                let token = self
                    .store_tokens(&ep.id, &ep.url, &mut record, tokens)
                    .await?;
                Ok(token)
            }
            Err(TokenError::Rejected(msg)) => {
                // The refresh token is dead: require a new sign-in.
                record.access_token_enc = None;
                record.refresh_token_enc = None;
                record.expires_at = None;
                let _ = self.save(&ep.id, &record).await;
                TOKENS.with(|t| t.borrow_mut().remove(&ep.id));
                Err(OAuthError::NeedsSignIn(format!(
                    "'{}' needs sign-in again (refresh was rejected: {msg})",
                    ep.name
                )))
            }
            Err(TokenError::Other(msg)) => Err(OAuthError::Other(msg)),
        }
    }

    async fn store_tokens(
        &self,
        endpoint_id: &str,
        url: &str,
        record: &mut Record,
        tokens: Value,
    ) -> Result<String, String> {
        let access = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or("token response has no access_token")?
            .to_string();
        record.access_token_enc = Some(self.crypto.seal_str(&access)?);
        if let Some(refresh) = tokens.get("refresh_token").and_then(|v| v.as_str()) {
            record.refresh_token_enc = Some(self.crypto.seal_str(refresh)?);
        }
        record.expires_at = tokens
            .get("expires_in")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .map(|secs| now_ms() + secs * 1000);
        self.save(endpoint_id, record).await?;
        TOKENS.with(|t| {
            t.borrow_mut().insert(
                endpoint_id.to_string(),
                (url.to_string(), access.clone(), record.expires_at),
            )
        });
        Ok(access)
    }

    // ---- sign-in ---------------------------------------------------------

    /// Discover the server's authorization server, register a client when
    /// needed, and return the URL to send the user to.
    pub async fn start(&self, ep: &Endpoint, redirect_uri: &str) -> Result<String, String> {
        let existing = self.load(&ep.id).await;
        let record = match existing {
            Some(r) if r.url == ep.url && r.redirect_uri == redirect_uri => r,
            _ => {
                let discovered = self.discover(ep).await?;
                self.register(ep, redirect_uri, discovered).await?
            }
        };
        self.save(&ep.id, &record).await?;

        let verifier = random_token(32);
        let challenge = B64URL.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_token(24);
        let mut auth_url = Url::parse(&record.authorization_endpoint)
            .map_err(|_| "authorization endpoint is not a valid URL".to_string())?;
        {
            let mut q = auth_url.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &record.client_id)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &state)
                .append_pair("resource", &record.resource);
            if let Some(scope) = &record.scope {
                q.append_pair("scope", scope);
            }
        }
        let flow = Flow {
            endpoint_id: ep.id.clone(),
            endpoint_name: ep.name.clone(),
            verifier,
            record,
        };
        self.kv
            .put(
                &format!("oauth-flow:{state}"),
                serde_json::to_string(&flow).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?
            .expiration_ttl(FLOW_TTL_SECS)
            .execute()
            .await
            .map_err(|e| e.to_string())?;
        Ok(auth_url.to_string())
    }

    /// Finish a sign-in from `/oauth/callback`. Returns the server name.
    pub async fn finish(
        &self,
        state: &str,
        code: &str,
        iss: Option<&str>,
    ) -> Result<String, String> {
        let key = format!("oauth-flow:{state}");
        let text = self
            .kv
            .get(&key)
            .text()
            .await
            .map_err(|e| e.to_string())?
            .ok_or("this sign-in link expired or was already used; start again from the relay")?;
        let _ = self.kv.delete(&key).await;
        let flow: Flow = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let mut record = flow.record;
        // RFC 9207: reject a response from a different authorization server.
        match iss {
            Some(iss) if normalize_issuer(iss) != normalize_issuer(&record.issuer) => {
                return Err("the authorization response came from an unexpected issuer".into())
            }
            None if record.authorization_response_iss => {
                return Err("the authorization server did not identify itself (missing iss)".into())
            }
            _ => {}
        }
        let form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", record.redirect_uri.clone()),
            ("code_verifier", flow.verifier),
            ("resource", record.resource.clone()),
        ];
        let tokens = self
            .token_request(&record, form)
            .await
            .map_err(|e| match e {
                TokenError::Rejected(m) | TokenError::Other(m) => {
                    format!("token exchange failed: {m}")
                }
            })?;
        let url = record.url.clone();
        self.store_tokens(&flow.endpoint_id, &url, &mut record, tokens)
            .await?;
        Ok(flow.endpoint_name)
    }

    // ---- discovery + registration ---------------------------------------

    fn check_url(&self, url: &str, what: &str) -> Result<Url, String> {
        let parsed = Url::parse(url).map_err(|_| format!("{what} is not a valid URL: {url}"))?;
        match parsed.scheme() {
            "https" => Ok(parsed),
            "http" if self.allow_insecure => Ok(parsed),
            _ => Err(format!("{what} must use https: {url}")),
        }
    }

    async fn discover(&self, ep: &Endpoint) -> Result<Discovered, String> {
        let endpoint_url = self.check_url(&ep.url, "server URL")?;

        // The 401 challenge may name the metadata URL and the scope to ask for.
        let (metadata_hint, scope_hint) = self.challenge(ep).await;
        let mut candidates = Vec::new();
        if let Some(hint) = metadata_hint {
            candidates.push(hint);
        }
        let origin = endpoint_url.origin().ascii_serialization();
        let path = endpoint_url.path().trim_end_matches('/');
        if !path.is_empty() {
            candidates.push(format!(
                "{origin}/.well-known/oauth-protected-resource{path}"
            ));
        }
        candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));

        let mut prm = None;
        for url in &candidates {
            if self.check_url(url, "resource metadata URL").is_err() {
                continue;
            }
            if let Ok(Some(v)) = self.get_json(url).await {
                prm = Some(v);
                break;
            }
        }
        let prm = prm.ok_or_else(|| {
            format!(
                "'{}' does not publish OAuth metadata (RFC 9728); it may not support OAuth sign-in",
                ep.name
            )
        })?;
        let auth_server = prm
            .get("authorization_servers")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .ok_or("resource metadata lists no authorization server")?
            .to_string();
        let resource = prm
            .get("resource")
            .and_then(|v| v.as_str())
            .filter(|r| resource_matches(r, &ep.url))
            .unwrap_or(&ep.url)
            .to_string();
        let scope = scope_hint.or_else(|| {
            prm.get("scopes_supported")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.is_empty())
        });

        let as_meta = self.authorization_server(&auth_server).await?;
        Ok(Discovered {
            as_meta,
            resource,
            scope,
        })
    }

    /// Call the server without credentials and read its `WWW-Authenticate`.
    async fn challenge(&self, ep: &Endpoint) -> (Option<String>, Option<String>) {
        let body = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": { "name": "endara-relay-worker", "version": env!("CARGO_PKG_VERSION") } }
        });
        let headers = Headers::new();
        let _ = headers.set("content-type", "application/json");
        let _ = headers.set("accept", "application/json, text/event-stream");
        for (k, v) in &ep.headers {
            if !k.eq_ignore_ascii_case("authorization") {
                let _ = headers.set(k, v);
            }
        }
        let Ok(resp) = self
            .send(&ep.url, Method::Post, headers, Some(body.to_string()))
            .await
        else {
            return (None, None);
        };
        if resp.status != 401 {
            return (None, None);
        }
        let header = resp.www_authenticate.unwrap_or_default();
        (
            auth_param(&header, "resource_metadata"),
            auth_param(&header, "scope"),
        )
    }

    async fn authorization_server(&self, issuer_url: &str) -> Result<Value, String> {
        let parsed = self.check_url(issuer_url, "authorization server URL")?;
        let origin = parsed.origin().ascii_serialization();
        let path = parsed.path().trim_end_matches('/');
        let input_issuer = if path.is_empty() {
            origin.clone()
        } else {
            format!("{origin}{path}")
        };
        let mut candidates: Vec<(String, String)> = Vec::new();
        let mut push = |url: String, issuer: &str| {
            if !candidates.iter().any(|(u, _)| *u == url) {
                candidates.push((url, issuer.to_string()));
            }
        };
        push(
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            &input_issuer,
        );
        push(
            format!("{origin}/.well-known/oauth-authorization-server"),
            &origin,
        );
        push(
            format!("{input_issuer}/.well-known/openid-configuration"),
            &input_issuer,
        );
        push(
            format!("{origin}/.well-known/openid-configuration"),
            &origin,
        );

        for (url, expected) in candidates {
            let Ok(Some(meta)) = self.get_json(&url).await else {
                continue;
            };
            let issuer = meta.get("issuer").and_then(|v| v.as_str()).unwrap_or("");
            if normalize_issuer(issuer) != normalize_issuer(&expected) {
                continue;
            }
            let methods: Vec<&str> = meta
                .get("code_challenge_methods_supported")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
                .unwrap_or_default();
            if !methods.is_empty() && !methods.contains(&"S256") {
                return Err("the authorization server does not support PKCE S256".into());
            }
            for field in ["authorization_endpoint", "token_endpoint"] {
                let v = meta
                    .get(field)
                    .and_then(|v| v.as_str())
                    .ok_or(format!("authorization server metadata has no {field}"))?;
                self.check_url(v, field)?;
            }
            return Ok(meta);
        }
        Err(format!(
            "no authorization server metadata found for {issuer_url}"
        ))
    }

    async fn register(
        &self,
        ep: &Endpoint,
        redirect_uri: &str,
        d: Discovered,
    ) -> Result<Record, String> {
        let meta = &d.as_meta;
        let s = |k: &str| meta.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let registration = s("registration_endpoint").ok_or_else(|| {
            format!(
                "'{}' does not support dynamic client registration, so the relay cannot sign in to it automatically",
                ep.name
            )
        })?;
        self.check_url(&registration, "registration endpoint")?;
        let mut request = json!({
            "client_name": format!("{CLIENT_NAME} — {}", ep.name),
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": "web",
        });
        if let Some(scope) = &d.scope {
            request["scope"] = Value::String(scope.clone());
        }
        let headers = Headers::new();
        let _ = headers.set("content-type", "application/json");
        let _ = headers.set("accept", "application/json");
        let resp = self
            .send(
                &registration,
                Method::Post,
                headers,
                Some(request.to_string()),
            )
            .await?;
        if !(200..300).contains(&resp.status) {
            return Err(format!(
                "client registration failed (HTTP {}): {}",
                resp.status,
                snippet(&resp.body)
            ));
        }
        let reg: Value = serde_json::from_str(&resp.body)
            .map_err(|_| "client registration returned invalid JSON".to_string())?;
        let client_id = reg
            .get("client_id")
            .and_then(|v| v.as_str())
            .ok_or("client registration returned no client_id")?
            .to_string();
        let client_secret_enc = match reg.get("client_secret").and_then(|v| v.as_str()) {
            Some(secret) => Some(self.crypto.seal_str(secret)?),
            None => None,
        };
        let token_auth_method = reg
            .get("token_endpoint_auth_method")
            .and_then(|v| v.as_str())
            .unwrap_or(if client_secret_enc.is_some() {
                "client_secret_post"
            } else {
                "none"
            })
            .to_string();
        Ok(Record {
            url: ep.url.clone(),
            redirect_uri: redirect_uri.to_string(),
            issuer: s("issuer").unwrap_or_default(),
            authorization_endpoint: s("authorization_endpoint").unwrap_or_default(),
            token_endpoint: s("token_endpoint").unwrap_or_default(),
            authorization_response_iss: meta
                .get("authorization_response_iss_parameter_supported")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            resource: d.resource,
            scope: d.scope,
            client_id,
            client_secret_enc,
            token_auth_method,
            access_token_enc: None,
            refresh_token_enc: None,
            expires_at: None,
        })
    }

    // ---- HTTP --------------------------------------------------------------

    async fn token_request(
        &self,
        record: &Record,
        mut form: Vec<(&str, String)>,
    ) -> Result<Value, TokenError> {
        let headers = Headers::new();
        let _ = headers.set("content-type", "application/x-www-form-urlencoded");
        let _ = headers.set("accept", "application/json");
        let secret = match &record.client_secret_enc {
            Some(enc) => Some(self.crypto.open_str(enc).map_err(TokenError::Other)?),
            None => None,
        };
        match (record.token_auth_method.as_str(), &secret) {
            ("client_secret_basic", Some(secret)) => {
                let enc =
                    |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
                let basic = base64::engine::general_purpose::STANDARD.encode(format!(
                    "{}:{}",
                    enc(&record.client_id),
                    enc(secret)
                ));
                let _ = headers.set("authorization", &format!("Basic {basic}"));
            }
            (_, Some(secret)) => {
                form.push(("client_id", record.client_id.clone()));
                form.push(("client_secret", secret.clone()));
            }
            (_, None) => form.push(("client_id", record.client_id.clone())),
        }
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter().map(|(k, v)| (*k, v.as_str())))
            .finish();
        let resp = self
            .send(&record.token_endpoint, Method::Post, headers, Some(body))
            .await
            .map_err(TokenError::Other)?;
        let parsed: Option<Value> = serde_json::from_str(&resp.body).ok();
        if (200..300).contains(&resp.status) {
            return parsed
                .ok_or_else(|| TokenError::Other("token endpoint returned invalid JSON".into()));
        }
        let detail = parsed
            .as_ref()
            .and_then(|v| {
                let e = v.get("error")?.as_str()?;
                Some(match v.get("error_description").and_then(|d| d.as_str()) {
                    Some(d) => format!("{e}: {d}"),
                    None => e.to_string(),
                })
            })
            .unwrap_or_else(|| format!("HTTP {}: {}", resp.status, snippet(&resp.body)));
        if resp.status == 400 || resp.status == 401 {
            Err(TokenError::Rejected(detail))
        } else {
            Err(TokenError::Other(detail))
        }
    }

    async fn get_json(&self, url: &str) -> Result<Option<Value>, String> {
        let headers = Headers::new();
        let _ = headers.set("accept", "application/json");
        let resp = self.send(url, Method::Get, headers, None).await?;
        if resp.status != 200 {
            return Ok(None);
        }
        Ok(serde_json::from_str(&resp.body).ok())
    }

    async fn send(
        &self,
        url: &str,
        method: Method,
        headers: Headers,
        body: Option<String>,
    ) -> Result<HttpReply, String> {
        let mut init = RequestInit::new();
        init.with_method(method).with_headers(headers);
        if let Some(b) = body {
            init.with_body(Some(JsValue::from_str(&b)));
        }
        let req = Request::new_with_init(url, &init).map_err(|e| e.to_string())?;
        let exchange = async {
            let mut resp = Fetch::Request(req).send().await?;
            let status = resp.status_code();
            let www_authenticate = resp.headers().get("www-authenticate")?;
            let body = resp.text().await?;
            Ok::<_, worker::Error>(HttpReply {
                status,
                www_authenticate,
                body,
            })
        };
        match select(Box::pin(exchange), Box::pin(Delay::from(HTTP_TIMEOUT))).await {
            Either::Left((r, _)) => r.map_err(|e| format!("request to {url} failed: {e}")),
            Either::Right(_) => Err(format!("request to {url} timed out")),
        }
    }
}

struct Discovered {
    as_meta: Value,
    resource: String,
    scope: Option<String>,
}

struct HttpReply {
    status: u16,
    www_authenticate: Option<String>,
    body: String,
}

enum TokenError {
    /// The grant itself was refused (invalid_grant and friends).
    Rejected(String),
    Other(String),
}

fn snippet(body: &str) -> String {
    let s: String = body.chars().take(200).collect();
    if s.len() < body.len() {
        format!("{s}…")
    } else {
        s
    }
}

fn normalize_issuer(s: &str) -> &str {
    s.trim_end_matches('/')
}

/// The RFC 9728 `resource` must identify this server (same origin, and the
/// server URL at or below its path).
fn resource_matches(resource: &str, endpoint_url: &str) -> bool {
    let (Ok(r), Ok(e)) = (Url::parse(resource), Url::parse(endpoint_url)) else {
        return false;
    };
    if r.origin() != e.origin() {
        return false;
    }
    let rp = r.path().trim_end_matches('/');
    let ep = e.path().trim_end_matches('/');
    ep == rp
        || ep
            .strip_prefix(rp)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Read `name="value"` (or `name=value`) from a `WWW-Authenticate` header.
fn auth_param(header: &str, name: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut search = 0;
    while let Some(pos) = lower[search..].find(&needle) {
        let start = search + pos;
        let boundary = start == 0 || matches!(lower.as_bytes()[start - 1], b' ' | b',' | b'\t');
        let value_start = start + needle.len();
        if boundary {
            let rest = &header[value_start..];
            return Some(if let Some(quoted) = rest.strip_prefix('"') {
                quoted.split('"').next().unwrap_or("").to_string()
            } else {
                rest.split([',', ' ']).next().unwrap_or("").to_string()
            })
            .filter(|v| !v.is_empty());
        }
        search = value_start;
    }
    None
}
