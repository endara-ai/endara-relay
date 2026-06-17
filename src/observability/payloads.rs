//! Ephemeral, thread-safe payload ring buffer for the observability store.
//!
//! Tier-2 of the two-tier store: full request/response payloads are kept in RAM
//! only, keyed by `request_uid`. Entries are bounded by an age window and by a
//! total memory budget (oldest-first eviction), and each payload is truncated to
//! `max_payload_bytes` with a `truncated` flag. Nothing here is persisted to disk.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

/// A captured request/response pair retained in memory for the payload window.
#[derive(Clone, Debug, Serialize)]
pub struct StoredPayloads {
    /// Serialized request payload (JSON), possibly truncated.
    pub request: String,
    /// Serialized response payload (JSON), possibly truncated.
    pub response: String,
    /// Set when the request payload exceeded `max_payload_bytes`.
    pub request_truncated: bool,
    /// Set when the response payload exceeded `max_payload_bytes`.
    pub response_truncated: bool,
    /// Whether the response was aggregated from a streamed/SSE result.
    pub streamed: bool,
    /// Capture time in Unix epoch milliseconds (UTC).
    pub captured_at_ms: i64,
}

struct Entry {
    payloads: StoredPayloads,
    instant: Instant,
    size: usize,
}

struct Inner {
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
    total_bytes: usize,
}

/// In-memory, time- and budget-bounded payload store keyed by `request_uid`.
pub struct PayloadStore {
    inner: Mutex<Inner>,
    window: Duration,
    budget_bytes: usize,
    max_payload_bytes: usize,
}

impl PayloadStore {
    /// Create a store with the configured payload window (minutes), memory budget
    /// (megabytes), and per-payload truncation cap (bytes).
    pub fn new(window_minutes: u64, budget_mb: u64, max_payload_bytes: usize) -> Self {
        let budget_bytes = (budget_mb as usize).saturating_mul(1024 * 1024);
        Self::from_raw(
            Duration::from_secs(window_minutes.saturating_mul(60)),
            budget_bytes,
            max_payload_bytes,
        )
    }

    fn from_raw(window: Duration, budget_bytes: usize, max_payload_bytes: usize) -> Self {
        PayloadStore {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                total_bytes: 0,
            }),
            window,
            budget_bytes,
            max_payload_bytes,
        }
    }

    /// Insert (or overwrite) the payloads for a `request_uid`. Triggers age and
    /// budget eviction. Both payloads are truncated to `max_payload_bytes`.
    pub fn insert(
        &self,
        request_uid: impl Into<String>,
        request: &Value,
        response: &Value,
        streamed: bool,
    ) {
        self.insert_at(
            request_uid.into(),
            request,
            response,
            streamed,
            Instant::now(),
        );
    }

    fn insert_at(
        &self,
        uid: String,
        request: &Value,
        response: &Value,
        streamed: bool,
        instant: Instant,
    ) {
        let (request, request_truncated) = self.truncate(request);
        let (response, response_truncated) = self.truncate(response);
        let size = request.len() + response.len() + uid.len();
        let payloads = StoredPayloads {
            request,
            response,
            request_truncated,
            response_truncated,
            streamed,
            captured_at_ms: Utc::now().timestamp_millis(),
        };
        let mut inner = self.inner.lock().unwrap();
        if let Some(old) = inner.entries.remove(&uid) {
            inner.total_bytes = inner.total_bytes.saturating_sub(old.size);
            inner.order.retain(|k| k != &uid);
        }
        inner.entries.insert(
            uid.clone(),
            Entry {
                payloads,
                instant,
                size,
            },
        );
        inner.order.push_back(uid);
        inner.total_bytes += size;
        Self::evict_expired_locked(&mut inner, self.window, instant);
        Self::evict_over_budget_locked(&mut inner, self.budget_bytes);
    }

    /// Fetch payloads for a `request_uid`, evicting expired entries first.
    pub fn get(&self, request_uid: &str) -> Option<StoredPayloads> {
        let mut inner = self.inner.lock().unwrap();
        Self::evict_expired_locked(&mut inner, self.window, Instant::now());
        inner.entries.get(request_uid).map(|e| e.payloads.clone())
    }

    /// Evict every entry older than the configured window. Safe to call from a
    /// background sweep task (R6).
    pub fn sweep_expired(&self) {
        let mut inner = self.inner.lock().unwrap();
        Self::evict_expired_locked(&mut inner, self.window, Instant::now());
    }

    /// Drop all buffered payloads (global purge).
    pub fn purge_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.clear();
        inner.order.clear();
        inner.total_bytes = 0;
    }

    /// Drop buffered payloads for the given set of `request_uid`s. The buffer is
    /// keyed by `request_uid`, so callers (R4/R6) supply the request_uid→server
    /// mapping and pass the set of UIDs belonging to the deleted server.
    pub fn remove_for_server(&self, drop_uids: &HashSet<String>) {
        self.remove_where(|uid| drop_uids.contains(uid));
    }

    /// Drop every buffered payload whose `request_uid` matches the predicate.
    pub fn remove_where<F: Fn(&str) -> bool>(&self, predicate: F) {
        let mut inner = self.inner.lock().unwrap();
        let drop: Vec<String> = inner
            .entries
            .keys()
            .filter(|k| predicate(k))
            .cloned()
            .collect();
        for k in drop {
            if let Some(old) = inner.entries.remove(&k) {
                inner.total_bytes = inner.total_bytes.saturating_sub(old.size);
            }
        }
        let Inner { entries, order, .. } = &mut *inner;
        order.retain(|k| entries.contains_key(k));
    }

    /// Number of buffered entries.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total bytes currently accounted toward the memory budget.
    pub fn total_bytes(&self) -> usize {
        self.inner.lock().unwrap().total_bytes
    }

    /// Configured age window.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Configured memory budget in bytes.
    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// Configured per-payload truncation cap in bytes.
    pub fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    fn truncate(&self, value: &Value) -> (String, bool) {
        let s = value.to_string();
        if s.len() <= self.max_payload_bytes {
            return (s, false);
        }
        let mut end = self.max_payload_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        (s[..end].to_string(), true)
    }

    fn evict_expired_locked(inner: &mut Inner, window: Duration, now: Instant) {
        while let Some(front) = inner.order.front() {
            let expired = inner
                .entries
                .get(front)
                .map(|e| now.saturating_duration_since(e.instant) > window)
                .unwrap_or(true);
            if !expired {
                break;
            }
            let key = inner.order.pop_front().unwrap();
            if let Some(old) = inner.entries.remove(&key) {
                inner.total_bytes = inner.total_bytes.saturating_sub(old.size);
            }
        }
    }

    fn evict_over_budget_locked(inner: &mut Inner, budget_bytes: usize) {
        while inner.total_bytes > budget_bytes && inner.order.len() > 1 {
            let key = match inner.order.pop_front() {
                Some(k) => k,
                None => break,
            };
            if let Some(old) = inner.entries.remove(&key) {
                inner.total_bytes = inner.total_bytes.saturating_sub(old.size);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store_bytes(
        window: Duration,
        budget_bytes: usize,
        max_payload_bytes: usize,
    ) -> PayloadStore {
        PayloadStore::from_raw(window, budget_bytes, max_payload_bytes)
    }

    #[test]
    fn get_hit_returns_stored_payloads() {
        let store = PayloadStore::new(10, 128, 256 * 1024);
        store.insert("uid-1", &json!({"a": 1}), &json!({"ok": true}), true);
        let got = store.get("uid-1").expect("present");
        assert_eq!(got.request, "{\"a\":1}");
        assert_eq!(got.response, "{\"ok\":true}");
        assert!(got.streamed);
        assert!(!got.request_truncated);
        assert!(!got.response_truncated);
        assert!(store.get("missing").is_none());
    }

    #[test]
    fn per_payload_truncation_sets_flag() {
        let store = store_bytes(Duration::from_secs(600), 1024 * 1024, 10);
        let big = json!("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
        store.insert("uid", &big, &json!(null), false);
        let got = store.get("uid").expect("present");
        assert!(got.request_truncated);
        assert!(got.request.len() <= 10);
        assert!(!got.response_truncated);
        assert_eq!(got.response, "null");
    }

    #[test]
    fn age_eviction_drops_expired_entries() {
        let store = store_bytes(Duration::from_secs(600), 1024 * 1024, 1024);
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(601))
            .expect("instant");
        store.insert_at("old".into(), &json!({"x": 1}), &json!(null), false, stale);
        store.insert("fresh", &json!({"y": 2}), &json!(null), false);
        store.sweep_expired();
        assert!(store.get("old").is_none());
        assert!(store.get("fresh").is_some());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn miss_after_expiry() {
        let store = store_bytes(Duration::from_secs(60), 1024 * 1024, 1024);
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(120))
            .expect("instant");
        store.insert_at("uid".into(), &json!({"x": 1}), &json!(null), false, stale);
        assert!(store.get("uid").is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn budget_eviction_drops_oldest_first() {
        let store = store_bytes(Duration::from_secs(600), 40, 1024);
        let req = json!({"k": "v"});
        let resp = json!({"k": "v"});
        store.insert("a", &req, &resp, false);
        store.insert("b", &req, &resp, false);
        assert!(store.get("a").is_some());
        assert!(store.get("b").is_some());
        store.insert("c", &req, &resp, false);
        assert!(store.get("a").is_none());
        assert!(store.get("b").is_some());
        assert!(store.get("c").is_some());
        assert!(store.total_bytes() <= store.budget_bytes());
    }

    #[test]
    fn remove_for_server_drops_matching_uids() {
        let store = PayloadStore::new(10, 128, 256 * 1024);
        store.insert("s1-a", &json!(1), &json!(2), false);
        store.insert("s1-b", &json!(1), &json!(2), false);
        store.insert("s2-a", &json!(1), &json!(2), false);
        let drop: HashSet<String> = ["s1-a".to_string(), "s1-b".to_string()]
            .into_iter()
            .collect();
        store.remove_for_server(&drop);
        assert!(store.get("s1-a").is_none());
        assert!(store.get("s1-b").is_none());
        assert!(store.get("s2-a").is_some());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn purge_all_clears_buffer() {
        let store = PayloadStore::new(10, 128, 256 * 1024);
        store.insert("a", &json!(1), &json!(2), false);
        store.insert("b", &json!(1), &json!(2), false);
        store.purge_all();
        assert!(store.is_empty());
        assert_eq!(store.total_bytes(), 0);
        assert!(store.get("a").is_none());
    }
}
