//! Typed tool-call event bus and JSON schema for the desktop overlay.
//!
//! Adapters emit a [`ToolCallEvent::Started`] at `call_tool` entry and a
//! matching [`ToolCallEvent::Completed`] or [`ToolCallEvent::Failed`] at
//! completion. Events are fanned out through a Tokio `broadcast` channel via
//! [`ToolCallEventBus`]; the management-API SSE handler at
//! `GET /api/events/tool-calls` subscribes once per client and forwards every
//! event. Slow / disconnected subscribers are dropped per Tokio `broadcast`
//! semantics so the relay never blocks tool-call execution on a stalled
//! overlay listener.
//!
//! The on-wire JSON schema matches the desktop overlay store; see the
//! workspace spec under "Event schema (SSE over mgmt socket)".

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

/// Default capacity for the broadcast channel. Lagged receivers (slow overlay
/// clients) drop the oldest events; 256 buffers ~1s of bursty traffic at a
/// generous 256 events/s without back-pressuring producers.
pub const DEFAULT_BUS_CAPACITY: usize = 256;

/// MCP tool annotations carried on every `started` event so the overlay can
/// render hint pills (destructive / open-world / read-only / idempotent)
/// without re-querying `tools/list`. Each field is `Option<bool>` because the
/// upstream server may report any subset of hints (or none at all).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAnnotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent: Option<bool>,
}

impl ToolAnnotations {
    /// Returns `true` when no hint fields were populated. Used by the
    /// serializer skip-guard so empty annotation objects are omitted entirely.
    pub fn is_empty(&self) -> bool {
        self.destructive.is_none()
            && self.open_world.is_none()
            && self.read_only.is_none()
            && self.idempotent.is_none()
    }
}

/// Parse the `annotations` field returned by `tools/list` (per MCP spec) into
/// the overlay's hint subset. The MCP wire fields are camelCased with a
/// `Hint` suffix (`destructiveHint`, `openWorldHint`, `readOnlyHint`,
/// `idempotentHint`) — see the MCP "Tool Annotations" docs. Anything that
/// isn't an object, or an object with no recognised hints, returns `None` so
/// the `started` event omits the `annotations` field entirely.
pub fn annotations_from_value(value: &Value) -> Option<ToolAnnotations> {
    let obj = value.as_object()?;
    let pick = |k: &str| obj.get(k).and_then(|v| v.as_bool());
    let ann = ToolAnnotations {
        destructive: pick("destructiveHint"),
        open_world: pick("openWorldHint"),
        read_only: pick("readOnlyHint"),
        idempotent: pick("idempotentHint"),
    };
    if ann.is_empty() {
        None
    } else {
        Some(ann)
    }
}

/// Typed tool-call lifecycle event published by adapters and consumed by the
/// SSE handler. Serialised as an internally-tagged JSON object with a `kind`
/// discriminator (`started` / `completed` / `failed`) so the desktop overlay
/// can branch on a single field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolCallEvent {
    /// Emitted at `call_tool` entry, before any network/process I/O.
    Started {
        request_id: String,
        ts: String,
        endpoint: String,
        transport: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ToolAnnotations>,
    },
    /// Emitted on `Ok(_)` return from the underlying `tools/call` request.
    Completed {
        request_id: String,
        ts: String,
        duration_ms: u64,
        /// Always `"ok"` for completed events; carried so the overlay can use
        /// a single status field across `completed` and `failed`.
        status: String,
    },
    /// Emitted on `Err(_)` return; carries the formatted error message so the
    /// overlay can render the failure reason without re-querying logs.
    Failed {
        request_id: String,
        ts: String,
        duration_ms: u64,
        /// Always `"error"` for failed events.
        status: String,
        error_message: String,
    },
}

/// Broadcast fan-out wrapper around `tokio::sync::broadcast::Sender`.
///
/// Cloning the bus is cheap (clones the inner sender). Producers call
/// [`ToolCallEventBus::send`]; subscribers call [`ToolCallEventBus::subscribe`]
/// to receive a fresh `Receiver`. Sends never block: on a full ring the
/// channel drops the oldest message and the corresponding subscriber sees
/// `RecvError::Lagged` on its next `recv`.
#[derive(Debug, Clone)]
pub struct ToolCallEventBus {
    tx: broadcast::Sender<ToolCallEvent>,
}

impl ToolCallEventBus {
    /// Create a bus backed by a broadcast channel of `capacity` events.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Create a bus with [`DEFAULT_BUS_CAPACITY`].
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_BUS_CAPACITY)
    }

    /// Publish an event. Returns silently when no subscribers are attached
    /// (the normal idle case) — slow subscribers are surfaced via the
    /// per-receiver `Lagged` error, not here.
    pub fn send(&self, event: ToolCallEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe a fresh receiver. Each SSE client gets its own.
    pub fn subscribe(&self) -> broadcast::Receiver<ToolCallEvent> {
        self.tx.subscribe()
    }

    /// Current subscriber count (used by tests).
    #[cfg(test)]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for ToolCallEventBus {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::broadcast::error::RecvError;

    #[test]
    fn started_event_serializes_with_kind_tag() {
        let ev = ToolCallEvent::Started {
            request_id: "rid-1".into(),
            ts: "2026-05-27T04:36:29.710Z".into(),
            endpoint: "github".into(),
            transport: "stdio".into(),
            server_type: Some("github".into()),
            server_name: Some("github".into()),
            profile: Some("default".into()),
            tool: "list_issues".into(),
            annotations: Some(ToolAnnotations {
                destructive: Some(false),
                open_world: Some(true),
                read_only: Some(true),
                idempotent: Some(true),
            }),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["kind"], "started");
        assert_eq!(v["tool"], "list_issues");
        assert_eq!(v["annotations"]["read_only"], true);
    }

    #[test]
    fn completed_and_failed_serialize_with_status() {
        let completed = ToolCallEvent::Completed {
            request_id: "rid-1".into(),
            ts: "t".into(),
            duration_ms: 12,
            status: "ok".into(),
        };
        let failed = ToolCallEvent::Failed {
            request_id: "rid-2".into(),
            ts: "t".into(),
            duration_ms: 9,
            status: "error".into(),
            error_message: "boom".into(),
        };
        let c = serde_json::to_value(&completed).unwrap();
        let f = serde_json::to_value(&failed).unwrap();
        assert_eq!(c["kind"], "completed");
        assert_eq!(c["status"], "ok");
        assert_eq!(f["kind"], "failed");
        assert_eq!(f["status"], "error");
        assert_eq!(f["error_message"], "boom");
    }

    #[test]
    fn annotations_from_value_maps_mcp_hint_keys() {
        let v = json!({
            "destructiveHint": true,
            "openWorldHint": false,
            "readOnlyHint": false,
            "idempotentHint": true,
            "title": "ignored"
        });
        let ann = annotations_from_value(&v).expect("annotations parsed");
        assert_eq!(ann.destructive, Some(true));
        assert_eq!(ann.open_world, Some(false));
        assert_eq!(ann.read_only, Some(false));
        assert_eq!(ann.idempotent, Some(true));
    }

    #[test]
    fn annotations_from_value_returns_none_when_no_hints() {
        assert!(annotations_from_value(&json!({"title": "foo"})).is_none());
        assert!(annotations_from_value(&json!("not-an-object")).is_none());
        assert!(annotations_from_value(&json!({})).is_none());
    }

    #[tokio::test]
    async fn bus_fans_out_to_every_subscriber() {
        let bus = ToolCallEventBus::with_default_capacity();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.send(ToolCallEvent::Completed {
            request_id: "r".into(),
            ts: "t".into(),
            duration_ms: 1,
            status: "ok".into(),
        });
        let ra = a.recv().await.unwrap();
        let rb = b.recv().await.unwrap();
        assert_eq!(ra, rb);
    }

    #[tokio::test]
    async fn bus_drops_oldest_on_lagged_subscriber_without_blocking_producer() {
        // Capacity 2 keeps the test cheap; producer sends 10 events while
        // the slow subscriber is parked. Tokio broadcast drops the oldest
        // events and surfaces a single `Lagged` error to the slow receiver
        // on its next recv() — the producer never blocks.
        let bus = ToolCallEventBus::new(2);
        let mut slow = bus.subscribe();
        for i in 0..10 {
            bus.send(ToolCallEvent::Completed {
                request_id: format!("r-{i}"),
                ts: "t".into(),
                duration_ms: i,
                status: "ok".into(),
            });
        }
        let err = tokio::time::timeout(Duration::from_millis(100), slow.recv())
            .await
            .expect("recv should not hang")
            .expect_err("first recv after lag returns Lagged");
        assert!(matches!(err, RecvError::Lagged(_)));
        let next = slow.recv().await.unwrap();
        if let ToolCallEvent::Completed { request_id, .. } = next {
            assert!(request_id.starts_with("r-"));
        } else {
            panic!("expected Completed event");
        }
    }

    #[tokio::test]
    async fn bus_send_with_no_subscribers_is_silent() {
        let bus = ToolCallEventBus::with_default_capacity();
        assert_eq!(bus.receiver_count(), 0);
        bus.send(ToolCallEvent::Completed {
            request_id: "r".into(),
            ts: "t".into(),
            duration_ms: 1,
            status: "ok".into(),
        });
    }
}
