//! Two-level cache: per-isolate memory first, then the optional `RELAY_CACHE`
//! KV namespace so cold isolates skip upstream handshakes and catalog fetches.

use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use worker::{Date, Env, KvStore};

pub const KV_BINDING: &str = "RELAY_CACHE";
/// KV rejects `expiration_ttl` below 60 seconds.
const KV_MIN_TTL_SECS: u64 = 60;

thread_local! {
    static MEMORY: RefCell<HashMap<String, (u64, Value)>> = RefCell::new(HashMap::new());
}

fn now_ms() -> u64 {
    Date::now().as_millis()
}

pub struct Cache {
    kv: Option<KvStore>,
}

impl Cache {
    pub fn new(env: &Env) -> Self {
        Cache {
            kv: env.kv(KV_BINDING).ok(),
        }
    }

    pub async fn get(&self, key: &str) -> Option<Value> {
        let hit = MEMORY.with(|m| {
            let mut m = m.borrow_mut();
            match m.get(key) {
                Some((exp, v)) if *exp > now_ms() => Some(v.clone()),
                Some(_) => {
                    m.remove(key);
                    None
                }
                None => None,
            }
        });
        if hit.is_some() {
            return hit;
        }
        let kv = self.kv.as_ref()?;
        let text = kv.get(key).text().await.ok().flatten()?;
        let wrapped: Value = serde_json::from_str(&text).ok()?;
        let exp = wrapped.get("exp")?.as_u64()?;
        if exp <= now_ms() {
            return None;
        }
        let value = wrapped.get("v")?.clone();
        MEMORY.with(|m| m.borrow_mut().insert(key.to_string(), (exp, value.clone())));
        Some(value)
    }

    /// Memory-only entry; used for short negative caching of failing upstreams.
    pub fn put_local(&self, key: &str, value: Value, ttl_secs: u64) {
        let exp = now_ms() + ttl_secs * 1000;
        MEMORY.with(|m| m.borrow_mut().insert(key.to_string(), (exp, value)));
    }

    pub async fn put(&self, key: &str, value: Value, ttl_secs: u64) {
        if ttl_secs == 0 {
            return;
        }
        let exp = now_ms() + ttl_secs * 1000;
        MEMORY.with(|m| m.borrow_mut().insert(key.to_string(), (exp, value.clone())));
        if let Some(kv) = &self.kv {
            let body = serde_json::json!({ "exp": exp, "v": value }).to_string();
            if let Ok(builder) = kv.put(key, body) {
                let _ = builder
                    .expiration_ttl(ttl_secs.max(KV_MIN_TTL_SECS))
                    .execute()
                    .await;
            }
        }
    }

    pub async fn delete(&self, key: &str) {
        MEMORY.with(|m| m.borrow_mut().remove(key));
        if let Some(kv) = &self.kv {
            let _ = kv.delete(key).await;
        }
    }
}
