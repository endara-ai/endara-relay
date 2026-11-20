//! The Coordinator Durable Object: one strongly consistent owner for every
//! state change that must not race.
//!
//! - Config saves: compare-and-set on the revision.
//! - Pending OAuth sign-ins: `flow/take` reads and deletes in one step, so a
//!   sign-in state is usable exactly once.
//! - OAuth records: sign-in, sign-out and token refresh are serialized per
//!   server. Concurrent refreshes share one upstream call, and a refresh
//!   result is committed only if the grant it started from is still current.
//!
//! A Durable Object handles one event at a time except while it awaits I/O
//! outside its own storage, so every read-modify-write below that does not
//! await a fetch is atomic. The only fetch (the token refresh) runs while a
//! per-server "refreshing" flag is set; the flag is checked and set without
//! an await in between. Other callers wait on their own timer (never on a
//! promise owned by another request, which the runtime treats as a hang)
//! and re-read storage. The refresh re-reads storage before it commits.

use crate::oauth::{self, Record, TokenError};
use crate::store::{Crypto, Stored};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;
use worker::wasm_bindgen::JsValue;
use worker::{
    durable_object, Date, Delay, DurableObject, Env, Headers, ListOptions, Method, Request,
    RequestInit, Response, Result, State, Stub,
};

pub const BINDING: &str = "RELAY_COORDINATOR";
const INSTANCE: &str = "relay";
const CONFIG_KEY: &str = "config";
const FLOW_PREFIX: &str = "flow:";
const OAUTH_PREFIX: &str = "oauth:";
/// Refresh this long before the access token expires.
const REFRESH_MARGIN_MS: u64 = 60_000;
/// How long a caller waits for another caller's refresh (100 ms steps).
const REFRESH_WAIT_STEPS: u32 = 150;

fn now_ms() -> u64 {
    Date::now().as_millis()
}

/// Short digest used to name "the token that was just rejected" without
/// sending the token itself.
pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---- client ----------------------------------------------------------------

#[derive(Clone)]
pub struct CoordinatorClient {
    stub: Rc<Stub>,
}

impl CoordinatorClient {
    pub fn new(env: &Env) -> std::result::Result<Self, String> {
        let missing =
            |_| "relay is not configured: missing the RELAY_COORDINATOR binding".to_string();
        let ns = env.durable_object(BINDING).map_err(missing)?;
        let stub = ns
            .id_from_name(INSTANCE)
            .and_then(|id| id.get_stub())
            .map_err(missing)?;
        Ok(CoordinatorClient {
            stub: Rc::new(stub),
        })
    }

    pub async fn call(&self, op: &str, body: Value) -> std::result::Result<Value, String> {
        let headers = Headers::new();
        let _ = headers.set("content-type", "application/json");
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(JsValue::from_str(&body.to_string())));
        let req = Request::new_with_init(&format!("https://coordinator/{op}"), &init)
            .map_err(|e| e.to_string())?;
        let mut resp = self
            .stub
            .fetch_with_request(req)
            .await
            .map_err(|e| format!("coordinator unavailable: {e}"))?;
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("coordinator reply: {e}"))?;
        match value.get("error").and_then(|e| e.as_str()) {
            Some(err) if resp.status_code() >= 500 => Err(err.to_string()),
            _ => Ok(value),
        }
    }
}

// ---- Durable Object ----------------------------------------------------------

#[durable_object]
pub struct Coordinator {
    state: State,
    env: Env,
    /// Servers with a token refresh in flight.
    refreshing: Rc<RefCell<HashSet<String>>>,
}

impl DurableObject for Coordinator {
    fn new(state: State, env: Env) -> Self {
        Coordinator {
            state,
            env,
            refreshing: Rc::new(RefCell::new(HashSet::new())),
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let op = req.path().trim_start_matches('/').to_string();
        let body: Value = req.json().await.unwrap_or(Value::Null);
        match self.handle(&op, body).await {
            Ok(v) => Response::from_json(&v),
            Err(e) => Ok(Response::from_json(&json!({ "error": e }))?.with_status(500)),
        }
    }
}

type R<T> = std::result::Result<T, String>;

fn s(v: &Value, k: &str) -> R<String> {
    v.get(k)
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("missing '{k}'"))
}

impl Coordinator {
    fn crypto(&self) -> R<Crypto> {
        self.env
            .secret("RELAY_ENCRYPTION_KEY")
            .map(|s| s.to_string())
            .or_else(|_| self.env.var("RELAY_ENCRYPTION_KEY").map(|v| v.to_string()))
            .map(|k| Crypto::new(&k))
            .map_err(|_| "missing RELAY_ENCRYPTION_KEY".to_string())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> R<Option<T>> {
        self.state
            .storage()
            .get::<T>(key)
            .await
            .map_err(|e| format!("storage read failed: {e}"))
    }

    async fn put<T: serde::Serialize>(&self, key: &str, value: &T) -> R<()> {
        self.state
            .storage()
            .put(key, value)
            .await
            .map_err(|e| format!("storage write failed: {e}"))
    }

    async fn delete(&self, key: &str) -> R<()> {
        self.state
            .storage()
            .delete(key)
            .await
            .map(|_| ())
            .map_err(|e| format!("storage delete failed: {e}"))
    }

    async fn handle(&self, op: &str, body: Value) -> R<Value> {
        match op {
            "config/get" => Ok(json!({ "doc": self.get::<Stored>(CONFIG_KEY).await? })),
            "config/put" => self.config_put(body).await,
            "flow/put" => self.flow_put(body).await,
            "flow/take" => self.flow_take(body).await,
            "oauth/get" => {
                let id = s(&body, "id")?;
                Ok(json!({ "record": self.get::<Record>(&format!("{OAUTH_PREFIX}{id}")).await? }))
            }
            "oauth/register" => self.oauth_register(body).await,
            "oauth/seed" => self.oauth_seed(body).await,
            "oauth/install" => self.oauth_install(body).await,
            "oauth/sign-out" => self.oauth_sign_out(body).await,
            "oauth/forget" => {
                let id = s(&body, "id")?;
                self.delete(&format!("{OAUTH_PREFIX}{id}")).await?;
                Ok(json!({ "ok": true }))
            }
            "oauth/token" => self.oauth_token(body).await,
            _ => Err(format!("unknown coordinator op '{op}'")),
        }
    }

    // ---- config ------------------------------------------------------------

    /// Compare-and-set. `seed: true` writes only when nothing is stored yet
    /// (first boot after the move from KV-only storage).
    async fn config_put(&self, body: Value) -> R<Value> {
        let doc: Stored = serde_json::from_value(body.get("doc").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid config: {e}"))?;
        let current: Option<Stored> = self.get(CONFIG_KEY).await?;
        if body.get("seed").and_then(|v| v.as_bool()).unwrap_or(false) {
            return match current {
                Some(existing) => Ok(json!({ "ok": true, "doc": existing })),
                None => {
                    self.put(CONFIG_KEY, &doc).await?;
                    Ok(json!({ "ok": true, "doc": doc }))
                }
            };
        }
        let expected = body.get("expected").and_then(|v| v.as_u64()).unwrap_or(0);
        let current_revision = current.as_ref().map(|c| c.revision).unwrap_or(0);
        if expected != current_revision {
            return Ok(json!({ "ok": false, "revision": current_revision }));
        }
        self.put(CONFIG_KEY, &doc).await?;
        Ok(json!({ "ok": true, "doc": doc }))
    }

    // ---- sign-in flows -----------------------------------------------------

    async fn flow_put(&self, body: Value) -> R<Value> {
        let state = s(&body, "state")?;
        let expires_at = body.get("expires_at").and_then(|v| v.as_u64()).unwrap_or(0);
        let flow = body.get("flow").cloned().unwrap_or(Value::Null);
        self.purge_expired_flows().await;
        self.put(
            &format!("{FLOW_PREFIX}{state}"),
            &json!({ "flow": flow, "expires_at": expires_at }),
        )
        .await?;
        Ok(json!({ "ok": true }))
    }

    /// Read and delete in one step: no second caller can ever see the flow.
    async fn flow_take(&self, body: Value) -> R<Value> {
        let key = format!("{FLOW_PREFIX}{}", s(&body, "state")?);
        let Some(entry) = self.get::<Value>(&key).await? else {
            return Ok(json!({ "flow": null }));
        };
        self.delete(&key).await?;
        let expires_at = entry
            .get("expires_at")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if expires_at <= now_ms() {
            return Ok(json!({ "flow": null }));
        }
        Ok(json!({ "flow": entry.get("flow").cloned().unwrap_or(Value::Null) }))
    }

    async fn purge_expired_flows(&self) {
        let opts = ListOptions::new().prefix(FLOW_PREFIX).limit(200);
        let Ok(map) = self.state.storage().list_with_options(opts).await else {
            return;
        };
        let now = now_ms();
        let mut expired = Vec::new();
        for entry in map.entries() {
            let Ok(pair) = entry else { continue };
            let pair: js_pair::Pair = pair.into();
            if let (Some(key), Some(value)) = (pair.key(), pair.value()) {
                if value
                    .get("expires_at")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    <= now
                {
                    expired.push(key);
                }
            }
        }
        if !expired.is_empty() {
            let _ = self.state.storage().delete_multiple(expired).await;
        }
    }

    // ---- OAuth records -----------------------------------------------------

    async fn record(&self, id: &str) -> R<Option<Record>> {
        self.get(&format!("{OAUTH_PREFIX}{id}")).await
    }

    async fn save_record(&self, id: &str, record: &mut Record) -> R<()> {
        record.version += 1;
        self.put(&format!("{OAUTH_PREFIX}{id}"), record).await
    }

    /// Store a client registration unless an equivalent one already exists
    /// (another sign-in registered first). Returns the record to use.
    async fn oauth_register(&self, body: Value) -> R<Value> {
        let id = s(&body, "id")?;
        let mut new: Record =
            serde_json::from_value(body.get("record").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid record: {e}"))?;
        if let Some(existing) = self.record(&id).await? {
            if existing.url == new.url && existing.redirect_uri == new.redirect_uri {
                return Ok(json!({ "record": existing }));
            }
            new.version = existing.version;
        }
        new.clear_tokens();
        self.save_record(&id, &mut new).await?;
        Ok(json!({ "record": new }))
    }

    /// Store a migrated record only if none exists; return the one kept.
    async fn oauth_seed(&self, body: Value) -> R<Value> {
        let id = s(&body, "id")?;
        if let Some(existing) = self.record(&id).await? {
            return Ok(json!({ "record": existing }));
        }
        let mut record: Record =
            serde_json::from_value(body.get("record").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid record: {e}"))?;
        self.save_record(&id, &mut record).await?;
        Ok(json!({ "record": record }))
    }

    /// Install the tokens of a completed sign-in as a brand-new grant.
    async fn oauth_install(&self, body: Value) -> R<Value> {
        let id = s(&body, "id")?;
        let mut record: Record =
            serde_json::from_value(body.get("record").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid record: {e}"))?;
        let tokens = body.get("tokens").cloned().unwrap_or(Value::Null);
        if let Some(existing) = self.record(&id).await? {
            record.version = existing.version;
        }
        let crypto = self.crypto()?;
        let access = oauth::apply_tokens(&crypto, &mut record, &tokens, true)?;
        self.save_record(&id, &mut record).await?;
        Ok(json!({ "token": access, "grant_id": record.grant(), "expires_at": record.expires_at }))
    }

    async fn oauth_sign_out(&self, body: Value) -> R<Value> {
        let id = s(&body, "id")?;
        if let Some(mut record) = self.record(&id).await? {
            record.clear_tokens();
            self.save_record(&id, &mut record).await?;
        }
        Ok(json!({ "ok": true }))
    }

    /// Mark `id` as refreshing, unless someone already is. Synchronous, so
    /// it is atomic within the Durable Object.
    fn try_start_refresh(&self, id: &str) -> Option<RefreshGuard> {
        if !self.refreshing.borrow_mut().insert(id.to_string()) {
            return None;
        }
        Some(RefreshGuard {
            set: self.refreshing.clone(),
            id: id.to_string(),
        })
    }

    /// A usable access token for a server. Refreshes when the token is about
    /// to expire, or when `stale` names the token that the upstream rejected
    /// and it is still the current one.
    async fn oauth_token(&self, body: Value) -> R<Value> {
        let id = s(&body, "id")?;
        let url = s(&body, "url")?;
        let stale = body
            .get("stale")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let crypto = self.crypto()?;

        let needs_sign_in = |msg: &str| -> R<Value> { Ok(json!({ "needs_sign_in": msg })) };
        let ready = |record: &Record, crypto: &Crypto| -> R<Option<Value>> {
            let Some(enc) = &record.access_token_enc else {
                return Ok(None);
            };
            let token = crypto.open_str(enc)?;
            let expiring = record
                .expires_at
                .is_some_and(|e| e <= now_ms() + REFRESH_MARGIN_MS);
            let rejected = stale.as_deref() == Some(token_hash(&token).as_str());
            Ok((!expiring && !rejected).then(|| {
                json!({ "token": token, "grant_id": record.grant(), "expires_at": record.expires_at })
            }))
        };

        // Fast path without the lock.
        match self.record(&id).await? {
            Some(r) if r.url == url && r.access_token_enc.is_some() => {
                if let Some(reply) = ready(&r, &crypto)? {
                    return Ok(reply);
                }
            }
            _ => return needs_sign_in("not signed in"),
        }

        // Single flight: one caller refreshes; the others wait and re-read,
        // and usually find the token the first caller just stored.
        let mut guard = None;
        let mut before = None;
        for _ in 0..REFRESH_WAIT_STEPS {
            let current = match self.record(&id).await? {
                Some(r) if r.url == url && r.access_token_enc.is_some() => r,
                _ => return needs_sign_in("not signed in"),
            };
            if let Some(reply) = ready(&current, &crypto)? {
                return Ok(reply);
            }
            if let Some(g) = self.try_start_refresh(&id) {
                guard = Some(g);
                before = Some(current);
                break;
            }
            Delay::from(Duration::from_millis(100)).await;
        }
        let (Some(_guard), Some(mut before)) = (guard, before) else {
            return Err("timed out waiting for another token refresh".into());
        };
        let Some(refresh_enc) = before.refresh_token_enc.clone() else {
            return needs_sign_in("the sign-in expired and there is no refresh token");
        };
        let refresh_token = crypto.open_str(&refresh_enc)?;
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token),
            ("resource", before.resource.clone()),
        ];
        if let Some(scope) = &before.scope {
            form.push(("scope", scope.clone()));
        }
        let outcome = oauth::token_request(&crypto, &before, form).await;

        // Commit only if nothing replaced or cleared this grant meanwhile.
        let current = self.record(&id).await?;
        let unchanged = current.as_ref().is_some_and(|c| {
            c.grant_id == before.grant_id
                && c.refresh_token_enc == before.refresh_token_enc
                && c.version == before.version
        });
        match outcome {
            Ok(tokens) if unchanged => {
                let access = oauth::apply_tokens(&crypto, &mut before, &tokens, false)?;
                self.save_record(&id, &mut before).await?;
                Ok(
                    json!({ "token": access, "grant_id": before.grant(), "expires_at": before.expires_at }),
                )
            }
            Err(TokenError::Rejected(msg)) if unchanged => {
                before.clear_tokens();
                self.save_record(&id, &mut before).await?;
                needs_sign_in(&format!("the refresh was rejected ({msg})"))
            }
            Err(TokenError::Other(msg)) if unchanged => Err(msg),
            // A sign-out, a new sign-in or another refresh won: use its state.
            _ => match current {
                Some(c) if c.url == url && c.access_token_enc.is_some() => {
                    let token = crypto.open_str(c.access_token_enc.as_deref().unwrap_or(""))?;
                    Ok(json!({ "token": token, "grant_id": c.grant(), "expires_at": c.expires_at }))
                }
                _ => needs_sign_in("signed out while the token was being refreshed"),
            },
        }
    }
}

/// `Map.entries()` yields `[key, value]` arrays; read them as typed values.
mod js_pair {
    use serde_json::Value;
    use worker::js_sys::Array;
    use worker::wasm_bindgen::{JsCast, JsValue};

    pub struct Pair(Array);

    impl From<JsValue> for Pair {
        fn from(v: JsValue) -> Self {
            Pair(v.unchecked_into())
        }
    }

    impl Pair {
        pub fn key(&self) -> Option<String> {
            self.0.get(0).as_string()
        }

        pub fn value(&self) -> Option<Value> {
            serde_wasm_bindgen::from_value(self.0.get(1)).ok()
        }
    }
}

/// Clears the "refreshing" flag on every exit path.
struct RefreshGuard {
    set: Rc<RefCell<HashSet<String>>>,
    id: String,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.set.borrow_mut().remove(&self.id);
    }
}
