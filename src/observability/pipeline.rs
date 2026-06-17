//! Async ingestion pipeline tying the capture hot path to the two-tier store.
//!
//! The hot request path (`registry::route_tool_call`) builds a
//! [`CaptureRecord`] and hands it to [`Observability::capture`], which
//! `try_send`s onto a bounded channel and **never blocks**: when the channel is
//! full the record is dropped and the [`Observability`]-owned `dropped` counter
//! is bumped. A background consumer task drains the channel, batch-writes
//! metadata into the SQLite [`Store`] (R2) and inserts payloads into the
//! in-memory [`PayloadStore`] (R3), both keyed by `request_uid`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::ObservabilityConfig;

use super::payloads::PayloadStore;
use super::store::{CallRecord, Store};

/// Bounded capacity of the capture channel. Sized so brief bursts are absorbed
/// without backpressure on the request path; sustained overflow drops + counts.
const CAPTURE_CHANNEL_CAPACITY: usize = 1024;

/// Maximum metadata rows coalesced into a single SQLite transaction by the
/// background consumer before re-checking the channel.
const METADATA_BATCH_MAX: usize = 128;

/// Full request/response bodies captured alongside a [`CallRecord`]. Present
/// only when payload capture is enabled (`store_payloads`).
pub struct CapturedPayloads {
    pub request: Value,
    pub response: Value,
    pub streamed: bool,
}

/// A single captured `tools/call`, enqueued by the hot path for the background
/// consumer to persist. `record.request_uid` is the correlation key shared by
/// the metadata row and the buffered payloads.
pub struct CaptureRecord {
    pub record: CallRecord,
    pub payloads: Option<CapturedPayloads>,
}

struct ObservabilityInner {
    enabled: bool,
    store_payloads: bool,
    tx: mpsc::Sender<CaptureRecord>,
    dropped: AtomicU64,
    store: Arc<Store>,
    payloads: Arc<PayloadStore>,
}

/// Cheaply-cloneable handle to the observability ingestion pipeline. Owns the
/// metadata [`Store`], the [`PayloadStore`], the bounded capture channel sender
/// and the overflow `dropped` counter (R4-owned, not in R2/R3).
#[derive(Clone)]
pub struct Observability {
    inner: Arc<ObservabilityInner>,
}

impl Observability {
    /// Construct the handle from config and the two stores, spawning the
    /// background consumer when `config.enabled`. When disabled, no consumer is
    /// spawned and [`Observability::capture`] is a cheap no-op.
    pub fn new(
        config: &ObservabilityConfig,
        store: Arc<Store>,
        payloads: Arc<PayloadStore>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(CAPTURE_CHANNEL_CAPACITY);
        if config.enabled {
            tokio::spawn(run_consumer(rx, Arc::clone(&store), Arc::clone(&payloads)));
        }
        Self {
            inner: Arc::new(ObservabilityInner {
                enabled: config.enabled,
                store_payloads: config.store_payloads,
                tx,
                dropped: AtomicU64::new(0),
                store,
                payloads,
            }),
        }
    }

    /// Hot-path enqueue. Non-blocking: `try_send` drops + counts on a full
    /// channel and returns immediately. No-op when the subsystem is disabled.
    pub fn capture(&self, record: CaptureRecord) {
        if !self.inner.enabled {
            return;
        }
        if self.inner.tx.try_send(record).is_err() {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether the subsystem is enabled (capture records metadata + payloads).
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled
    }

    /// Whether request/response payloads are captured into the ring buffer.
    pub fn store_payloads(&self) -> bool {
        self.inner.store_payloads
    }

    /// Number of capture records dropped due to a full channel (R4 overflow
    /// counter). Surfaced to R5's stats endpoint.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Shared metadata store handle (R5 query / R6 retention + delete cascade).
    pub fn store(&self) -> &Arc<Store> {
        &self.inner.store
    }

    /// Shared payload buffer handle (R5 drill-through / R6 sweep + cascade).
    pub fn payloads(&self) -> &Arc<PayloadStore> {
        &self.inner.payloads
    }

    /// Build a handle with an explicit channel capacity and **no** consumer
    /// task, returning the receiver so the caller can keep it alive (an open
    /// but undrained channel fills deterministically). Used by overflow tests
    /// to exercise the `try_send` drop-and-count path.
    #[cfg(test)]
    fn new_no_consumer(
        enabled: bool,
        store_payloads: bool,
        capacity: usize,
        store: Arc<Store>,
        payloads: Arc<PayloadStore>,
    ) -> (Self, mpsc::Receiver<CaptureRecord>) {
        let (tx, rx) = mpsc::channel(capacity);
        let obs = Self {
            inner: Arc::new(ObservabilityInner {
                enabled,
                store_payloads,
                tx,
                dropped: AtomicU64::new(0),
                store,
                payloads,
            }),
        };
        (obs, rx)
    }
}

/// Drain the capture channel until it closes, coalescing metadata writes into
/// batched transactions (off the async runtime via `spawn_blocking`) and
/// inserting payloads into the in-memory buffer.
async fn run_consumer(
    mut rx: mpsc::Receiver<CaptureRecord>,
    store: Arc<Store>,
    payloads: Arc<PayloadStore>,
) {
    while let Some(first) = rx.recv().await {
        let mut records: Vec<CallRecord> = Vec::new();
        let mut buffered: Vec<(String, CapturedPayloads)> = Vec::new();
        ingest(first, &mut records, &mut buffered);
        while records.len() < METADATA_BATCH_MAX {
            match rx.try_recv() {
                Ok(next) => ingest(next, &mut records, &mut buffered),
                Err(_) => break,
            }
        }

        let store_for_write = Arc::clone(&store);
        let to_insert = std::mem::take(&mut records);
        match tokio::task::spawn_blocking(move || store_for_write.insert_batch(&to_insert)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!(error = %e, "observability: failed to persist metadata batch"),
            Err(e) => warn!(error = %e, "observability: metadata writer task panicked"),
        }

        for (uid, p) in buffered {
            payloads.insert(uid, &p.request, &p.response, p.streamed);
        }
    }
    debug!("observability: capture channel closed, consumer task exiting");
}

/// Split a captured record into its metadata row and (optional) buffered
/// payloads, keyed by `request_uid`.
fn ingest(
    cr: CaptureRecord,
    records: &mut Vec<CallRecord>,
    buffered: &mut Vec<(String, CapturedPayloads)>,
) {
    if let (Some(uid), Some(p)) = (cr.record.request_uid.clone(), cr.payloads) {
        buffered.push((uid, p));
    }
    records.push(cr.record);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn config(enabled: bool, store_payloads: bool) -> ObservabilityConfig {
        ObservabilityConfig {
            enabled,
            store_payloads,
            ..ObservabilityConfig::default()
        }
    }

    fn record(uid: &str) -> CaptureRecord {
        CaptureRecord {
            record: CallRecord {
                request_uid: Some(uid.to_string()),
                server_name: Some("alpha".into()),
                tool: "alpha__do".into(),
                ts_start: 1000,
                ts_end: 1005,
                duration_ms: 5,
                success: true,
                ..Default::default()
            },
            payloads: Some(CapturedPayloads {
                request: json!({"a": 1}),
                response: json!({"ok": true}),
                streamed: false,
            }),
        }
    }

    #[tokio::test]
    async fn capture_persists_metadata_and_payload() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let payloads = Arc::new(PayloadStore::new(10, 128, 256 * 1024));
        let obs = Observability::new(
            &config(true, true),
            Arc::clone(&store),
            Arc::clone(&payloads),
        );

        obs.capture(record("uid-1"));

        // The consumer drains asynchronously; poll until the row lands.
        let mut found = None;
        for _ in 0..50 {
            if let Some(r) = store.get_by_request_uid("uid-1").unwrap() {
                found = Some(r);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let row = found.expect("metadata row persisted");
        assert_eq!(row.server_name.as_deref(), Some("alpha"));
        assert!(row.success);

        let buffered = payloads.get("uid-1").expect("payload buffered");
        assert_eq!(buffered.request, "{\"a\":1}");
        assert_eq!(buffered.response, "{\"ok\":true}");
        assert_eq!(obs.dropped(), 0);
    }

    #[tokio::test]
    async fn store_payloads_disabled_records_metadata_only() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let payloads = Arc::new(PayloadStore::new(10, 128, 256 * 1024));
        let obs = Observability::new(
            &config(true, false),
            Arc::clone(&store),
            Arc::clone(&payloads),
        );

        // Hot path omits payloads when store_payloads is off.
        let mut cr = record("uid-2");
        cr.payloads = None;
        obs.capture(cr);

        for _ in 0..50 {
            if store.get_by_request_uid("uid-2").unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(store.get_by_request_uid("uid-2").unwrap().is_some());
        assert!(payloads.get("uid-2").is_none());
    }

    #[tokio::test]
    async fn overflow_drops_without_blocking_and_counts() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let payloads = Arc::new(PayloadStore::new(10, 128, 256 * 1024));
        // Capacity 2, no consumer draining: the 3rd+ enqueue overflow-drops.
        // Keep `_rx` bound so the channel stays open (a dropped receiver would
        // close it and fail every `try_send`).
        let (obs, _rx) = Observability::new_no_consumer(true, true, 2, store, payloads);

        for i in 0..5 {
            obs.capture(record(&format!("uid-{i}")));
        }
        assert_eq!(obs.dropped(), 3);
    }

    #[tokio::test]
    async fn disabled_capture_is_noop() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let payloads = Arc::new(PayloadStore::new(10, 128, 256 * 1024));
        let obs = Observability::new(
            &config(false, true),
            Arc::clone(&store),
            Arc::clone(&payloads),
        );

        obs.capture(record("uid-x"));
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(store.get_by_request_uid("uid-x").unwrap().is_none());
        assert!(payloads.is_empty());
        assert_eq!(obs.dropped(), 0);
    }
}
