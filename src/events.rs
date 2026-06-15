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
use tracing::field::{Field, Visit};
use tracing::span::Attributes;
use tracing::{Id, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Default capacity for the broadcast channel. Lagged receivers (slow overlay
/// clients) drop the oldest events; 256 buffers ~1s of bursty traffic at a
/// generous 256 events/s without back-pressuring producers.
pub const DEFAULT_BUS_CAPACITY: usize = 256;

/// Identity of the inbound MCP client making a request.
///
/// Populated from the `clientInfo` object sent on `initialize` (locked into a
/// `Mcp-Session-Id`-keyed session map by the relay), with a per-request
/// fallback to the `User-Agent` / `Origin` HTTP headers when no session is
/// known. All fields are `Option<String>` because the source signals are all
/// independently optional, and the on-wire JSON omits any `None` fields so
/// the overlay can render `name` / `name + version` / UA-only with the same
/// rendering code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ClientIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// Strip a trailing ` (via mcp-remote <version>)` suffix from a client name.
///
/// `mcp-remote` proxies append this marker to the upstream `clientInfo.name`
/// (e.g. `local-agent-mode-Endara Relay (via mcp-remote 0.1.37)`); the relay
/// normalizes against the underlying name, so the marker is removed before
/// matching. The match is case-insensitive and the returned slice is
/// trimmed of trailing whitespace. Returns the input unchanged when no marker
/// is present.
fn strip_mcp_remote_suffix(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    if let Some(idx) = lower.find(" (via mcp-remote") {
        name[..idx].trim_end()
    } else {
        name
    }
}

impl Serialize for ClientIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let label = self.client_label();
        let len = self.name.is_some() as usize
            + self.version.is_some() as usize
            + self.user_agent.is_some() as usize
            + self.origin.is_some() as usize
            + label.is_some() as usize;
        let mut map = serializer.serialize_map(Some(len))?;
        if let Some(name) = self.name.as_ref() {
            map.serialize_entry("name", name)?;
        }
        if let Some(version) = self.version.as_ref() {
            map.serialize_entry("version", version)?;
        }
        if let Some(user_agent) = self.user_agent.as_ref() {
            map.serialize_entry("user_agent", user_agent)?;
        }
        if let Some(origin) = self.origin.as_ref() {
            map.serialize_entry("origin", origin)?;
        }
        if let Some(label) = label.as_ref() {
            map.serialize_entry("label", label)?;
        }
        map.end()
    }
}

impl ClientIdentity {
    /// Returns `true` when every field is `None`. Used by the serializer
    /// skip-guard so empty identity objects are omitted from the wire entirely.
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.version.is_none()
            && self.user_agent.is_none()
            && self.origin.is_none()
    }

    /// Friendly client label for the `client_name` audit field and the overlay.
    ///
    /// Strips the ` (via mcp-remote <version>)` marker from `clientInfo.name`
    /// and maps known client identifiers (case-insensitive) to a human
    /// readable label (e.g. `local-agent-mode-*` -> `Claude Cowork`,
    /// `claude-ai` -> `Claude Desktop`, `claude-code` -> `Claude Code`).
    /// Unrecognized names pass through as the suffix-stripped, trimmed raw
    /// name. When no usable name is present, falls back to a concise
    /// `User-Agent` token (the substring before the first `/` or whitespace),
    /// else `None`.
    pub fn client_label(&self) -> Option<String> {
        if let Some(name) = self.name.as_ref() {
            let stripped = strip_mcp_remote_suffix(name).trim();
            if !stripped.is_empty() {
                let lower = stripped.to_ascii_lowercase();
                if lower.starts_with("local-agent-mode-") {
                    return Some("Claude Cowork".to_string());
                }
                if lower == "claude-ai" {
                    return Some("Claude Desktop".to_string());
                }
                if lower == "claude-code" {
                    return Some("Claude Code".to_string());
                }
                if lower == "anthropic"
                    || lower.starts_with("anthropic-")
                    || lower.starts_with("anthropic ")
                {
                    return Some("Claude".to_string());
                }
                if lower.starts_with("cursor-vscode") || lower == "cursor" {
                    return Some("Cursor".to_string());
                }
                return Some(stripped.to_string());
            }
        }
        if let Some(ua) = self.user_agent.as_ref() {
            let token = ua
                .split(|c: char| c == '/' || c.is_whitespace())
                .next()
                .unwrap_or("")
                .trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
        None
    }
}

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
        /// JSON-RPC envelope id (as serialised by `serde_json::Value::to_string`)
        /// extracted from the surrounding `request` tracing span. Used by the
        /// desktop overlay to click-to-jump from an overlay card to the
        /// matching `request{id="..."}` log row. `None` when no `request` span
        /// is on the stack (e.g. internal callers).
        #[serde(skip_serializing_if = "Option::is_none")]
        jsonrpc_id: Option<String>,
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
        /// Identity of the calling MCP client, captured from the surrounding
        /// `request` tracing span (populated by the inbound dispatch from
        /// `clientInfo` + `User-Agent`/`Origin`). `None` when no caller
        /// signal is known (e.g. internal background calls).
        #[serde(skip_serializing_if = "Option::is_none")]
        client: Option<ClientIdentity>,
    },
    /// Emitted on `Ok(_)` return from the underlying `tools/call` request.
    Completed {
        request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        jsonrpc_id: Option<String>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        jsonrpc_id: Option<String>,
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

/// Subset of the `request{...}` and `mcp_request{...}` span fields captured
/// into span extensions by [`SpanFieldCaptureLayer`] and surfaced to adapter
/// code via [`current_request_context`]. Adapters use this to populate
/// `jsonrpc_id` (from the inner `request` span) and `profile` (from the outer
/// `mcp_request` span) on every emitted [`ToolCallEvent`] without taking a
/// breaking change to the [`crate::adapter::McpAdapter::call_tool`] signature.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RequestSpanContext {
    /// JSON-RPC envelope id as serialised by `Value::to_string` (numbers
    /// render unquoted, strings render with quotes). `None` when no
    /// `request` span is on the stack or when the id was the literal
    /// `"null"` sentinel (notifications, which don't reach `call_tool`).
    pub jsonrpc_id: Option<String>,
    /// Profile path segment from `/mcp/{profile}`. `None` for the global
    /// `/mcp` endpoint.
    pub profile: Option<String>,
    /// Identity of the inbound MCP caller, captured from the `request`
    /// span's `client` field (a JSON-serialised [`ClientIdentity`] recorded
    /// by the inbound dispatch). `None` when no `request` span is on the
    /// stack, when the dispatch recorded no client signal, or when the
    /// span's `client` field could not be parsed back.
    pub client: Option<ClientIdentity>,
}

/// Fields captured per span by [`SpanFieldCaptureLayer`]. Stored in the
/// span's extensions so [`current_request_context`] can read them back when
/// an adapter publishes a [`ToolCallEvent`]. Carrying all fields in one
/// extension keeps the per-span allocation count to one.
#[derive(Debug, Default, Clone)]
struct CapturedSpanFields {
    jsonrpc_id: Option<String>,
    profile: Option<String>,
    client: Option<ClientIdentity>,
}

/// Visits a span's recorded fields once at span creation time, capturing the
/// JSON-RPC id from the `request` span and the profile path from the
/// `mcp_request` span. Other fields are ignored.
struct CapturingVisitor<'a> {
    captured: &'a mut CapturedSpanFields,
    is_request: bool,
    is_mcp_request: bool,
}

impl<'a> Visit for CapturingVisitor<'a> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `info_span!("...", x = %expr)` may route through `record_debug`
        // when the value is not a primitive `String`/`&str`; the formatted
        // output matches what the tracing fmt layer prints.
        self.record(field.name(), format!("{:?}", value));
    }
}

impl<'a> CapturingVisitor<'a> {
    fn record(&mut self, name: &str, value: String) {
        if self.is_request && name == "id" {
            // The `request` span uses `id = %id_str` where `id_str` is
            // `"null"` for notifications. `call_tool` is never reached for
            // notifications, but we still skip the sentinel so a stray
            // emitter cannot leak a fake id into an event.
            if value != "null" {
                self.captured.jsonrpc_id = Some(value);
            }
        } else if self.is_request && name == "client" {
            // The `request` span carries `client = %client_json` where
            // `client_json` is a JSON-serialised `ClientIdentity`. Parse it
            // back so `current_request_context()` can hand a typed value
            // to adapter event emitters. Silently ignore parse failures so
            // a malformed field cannot break dispatch.
            if let Ok(identity) = serde_json::from_str::<ClientIdentity>(&value) {
                if !identity.is_empty() {
                    self.captured.client = Some(identity);
                }
            }
        } else if self.is_mcp_request && name == "profile" {
            self.captured.profile = Some(value);
        }
    }
}

/// Tracing [`Layer`] that captures the JSON-RPC id and profile fields from
/// the relay's `request` and `mcp_request` spans into span extensions so
/// [`current_request_context`] can surface them to adapter code without a
/// trait-level signature change. Install once at process start in
/// `main.rs` via `tracing_subscriber::registry().with(SpanFieldCaptureLayer)`.
pub struct SpanFieldCaptureLayer;

impl<S> Layer<S> for SpanFieldCaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let name = attrs.metadata().name();
        let is_request = name == "request";
        let is_mcp_request = name == "mcp_request";
        if !(is_request || is_mcp_request) {
            return;
        }
        let mut captured = CapturedSpanFields::default();
        let mut visitor = CapturingVisitor {
            captured: &mut captured,
            is_request,
            is_mcp_request,
        };
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(captured);
        }
    }
}

/// Walk the current tracing span scope and pull the JSON-RPC id (from the
/// nearest `request` span) and profile (from the nearest `mcp_request`
/// span) previously captured by [`SpanFieldCaptureLayer`]. Returns a
/// fully-`None` [`RequestSpanContext`] when no subscriber is installed (e.g.
/// in unit tests that do not configure tracing) or when the capture layer is
/// not in the layer stack — adapters degrade to omitting both fields.
pub fn current_request_context() -> RequestSpanContext {
    let mut ctx = RequestSpanContext::default();
    tracing::Span::current().with_subscriber(|(id, sub)| {
        let Some(reg) = sub.downcast_ref::<tracing_subscriber::Registry>() else {
            return;
        };
        let Some(span) = reg.span(id) else {
            return;
        };
        for s in span.scope() {
            let ext = s.extensions();
            if let Some(captured) = ext.get::<CapturedSpanFields>() {
                if ctx.jsonrpc_id.is_none() {
                    if let Some(v) = &captured.jsonrpc_id {
                        ctx.jsonrpc_id = Some(v.clone());
                    }
                }
                if ctx.profile.is_none() {
                    if let Some(v) = &captured.profile {
                        ctx.profile = Some(v.clone());
                    }
                }
                if ctx.client.is_none() {
                    if let Some(v) = &captured.client {
                        ctx.client = Some(v.clone());
                    }
                }
            }
        }
    });
    ctx
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
            jsonrpc_id: Some("42".into()),
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
            client: Some(ClientIdentity {
                name: Some("claude-ai".into()),
                version: Some("0.1.0".into()),
                user_agent: None,
                origin: None,
            }),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["kind"], "started");
        assert_eq!(v["tool"], "list_issues");
        assert_eq!(v["annotations"]["read_only"], true);
        assert_eq!(v["jsonrpc_id"], "42");
        assert_eq!(v["client"]["name"], "claude-ai");
        assert_eq!(v["client"]["version"], "0.1.0");
        assert!(
            v["client"].get("user_agent").is_none(),
            "user_agent should be omitted when None, got {v}"
        );
        assert!(
            v["client"].get("origin").is_none(),
            "origin should be omitted when None, got {v}"
        );
    }

    #[test]
    fn started_event_omits_jsonrpc_id_when_none() {
        let ev = ToolCallEvent::Started {
            request_id: "rid-1".into(),
            jsonrpc_id: None,
            ts: "t".into(),
            endpoint: "github".into(),
            transport: "stdio".into(),
            server_type: None,
            server_name: None,
            profile: None,
            tool: "list_issues".into(),
            annotations: None,
            client: None,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert!(
            v.get("jsonrpc_id").is_none(),
            "jsonrpc_id should be omitted when None, got {v}"
        );
        assert!(
            v.get("client").is_none(),
            "client should be omitted when None, got {v}"
        );
    }

    #[test]
    fn client_identity_round_trip_serialization() {
        let full = ClientIdentity {
            name: Some("Claude Desktop".into()),
            version: Some("0.7.0".into()),
            user_agent: Some("claude-desktop/0.7.0".into()),
            origin: Some("https://claude.ai".into()),
        };
        let v = serde_json::to_value(&full).unwrap();
        assert_eq!(v["name"], "Claude Desktop");
        assert_eq!(v["version"], "0.7.0");
        assert_eq!(v["user_agent"], "claude-desktop/0.7.0");
        assert_eq!(v["origin"], "https://claude.ai");
        let back: ClientIdentity = serde_json::from_value(v).unwrap();
        assert_eq!(back, full);
        assert!(!full.is_empty());

        let empty = ClientIdentity::default();
        let v = serde_json::to_value(&empty).unwrap();
        assert_eq!(v, json!({}));
        assert!(empty.is_empty());
    }

    #[test]
    fn client_identity_client_label_prefers_name_then_ua_token() {
        // `clientInfo.name` wins when present (unknown name passes through).
        let with_name = ClientIdentity {
            name: Some("Claude Desktop".into()),
            version: Some("0.7.0".into()),
            user_agent: Some("claude-desktop/0.7.0".into()),
            origin: None,
        };
        assert_eq!(with_name.client_label().as_deref(), Some("Claude Desktop"));

        // UA-only callers derive a concise token (before the first `/`).
        let ua_only = ClientIdentity {
            name: None,
            version: None,
            user_agent: Some("cursor-vscode/0.42 (Cursor IDE)".into()),
            origin: None,
        };
        assert_eq!(ua_only.client_label().as_deref(), Some("cursor-vscode"));

        // UA whose leading token ends at whitespace (no slash).
        let ua_space = ClientIdentity {
            name: None,
            version: None,
            user_agent: Some("CustomAgent some details".into()),
            origin: None,
        };
        assert_eq!(ua_space.client_label().as_deref(), Some("CustomAgent"));

        // Empty identity yields no label.
        assert_eq!(ClientIdentity::default().client_label(), None);

        // An empty `name` string falls through to the UA (None here).
        let empty_name = ClientIdentity {
            name: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(empty_name.client_label(), None);
    }

    #[test]
    fn client_identity_client_label_normalizes_known_clients() {
        let label = |name: &str| {
            ClientIdentity {
                name: Some(name.into()),
                ..Default::default()
            }
            .client_label()
        };

        // Cowork: `local-agent-mode-*` prefix, including the mcp-remote marker.
        assert_eq!(
            label("local-agent-mode-foo").as_deref(),
            Some("Claude Cowork")
        );
        assert_eq!(
            label("local-agent-mode-Endara Relay (via mcp-remote 0.1.37)").as_deref(),
            Some("Claude Cowork"),
        );
        // Desktop / Code exact matches.
        assert_eq!(label("claude-ai").as_deref(), Some("Claude Desktop"));
        assert_eq!(label("claude-code").as_deref(), Some("Claude Code"));
        // Anthropic, with and without a service suffix, maps to Claude.
        assert_eq!(label("Anthropic").as_deref(), Some("Claude"));
        assert_eq!(label("anthropic-mcp").as_deref(), Some("Claude"));
        // Cursor variants.
        assert_eq!(label("cursor").as_deref(), Some("Cursor"));
        assert_eq!(label("cursor-vscode").as_deref(), Some("Cursor"));
        // Matching is case-insensitive.
        assert_eq!(label("CLAUDE-AI").as_deref(), Some("Claude Desktop"));

        // Unknown names pass through (suffix-stripped, trimmed).
        assert_eq!(label("Some Editor").as_deref(), Some("Some Editor"));
        assert_eq!(
            label("Some Editor (via mcp-remote 1.2.3)").as_deref(),
            Some("Some Editor"),
        );

        // Empty / no name falls back to the UA token, else None.
        assert_eq!(ClientIdentity::default().client_label(), None);
        let ua_only = ClientIdentity {
            user_agent: Some("cursor-vscode/0.42 (Cursor IDE)".into()),
            ..Default::default()
        };
        assert_eq!(ua_only.client_label().as_deref(), Some("cursor-vscode"));
    }

    #[test]
    fn client_identity_serializes_friendly_label_and_keeps_raw_name() {
        let id = ClientIdentity {
            name: Some("local-agent-mode-Endara Relay (via mcp-remote 0.1.37)".into()),
            version: Some("0.1.0".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&id).unwrap();
        // Raw name is preserved untouched.
        assert_eq!(
            v["name"],
            "local-agent-mode-Endara Relay (via mcp-remote 0.1.37)"
        );
        assert_eq!(v["version"], "0.1.0");
        // Friendly label is surfaced alongside it.
        assert_eq!(v["label"], "Claude Cowork");

        // `label` is omitted when there is no usable identity.
        let empty = serde_json::to_value(ClientIdentity::default()).unwrap();
        assert!(
            empty.get("label").is_none(),
            "label should be omitted, got {empty}"
        );
    }

    #[test]
    fn debug_formatted_client_name_field_is_quoted_for_multiword_names() {
        // The audit and tool-call log lines emit `client_name = ?value`, which
        // the tracing fmt layer renders via `Debug`. Confirm a multi-word name
        // is wrapped in quotes so the desktop `EVENT_FIELD_RE`
        // (`(\w+)=("([^"]*)"|(\S*))`) captures it intact rather than truncating
        // at the space.
        let name = "Claude Desktop";
        let field = format!("client_name={name:?}");
        assert_eq!(field, "client_name=\"Claude Desktop\"");
        let quoted = field.strip_prefix("client_name=").unwrap();
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        let inner = &quoted[1..quoted.len() - 1];
        assert_eq!(inner, "Claude Desktop");

        // An empty value still renders as the quoted empty string the desktop
        // parser treats as "skip".
        assert_eq!(format!("client_name={:?}", ""), "client_name=\"\"");
    }

    #[test]
    fn completed_and_failed_serialize_with_status() {
        let completed = ToolCallEvent::Completed {
            request_id: "rid-1".into(),
            jsonrpc_id: Some("\"abc\"".into()),
            ts: "t".into(),
            duration_ms: 12,
            status: "ok".into(),
        };
        let failed = ToolCallEvent::Failed {
            request_id: "rid-2".into(),
            jsonrpc_id: None,
            ts: "t".into(),
            duration_ms: 9,
            status: "error".into(),
            error_message: "boom".into(),
        };
        let c = serde_json::to_value(&completed).unwrap();
        let f = serde_json::to_value(&failed).unwrap();
        assert_eq!(c["kind"], "completed");
        assert_eq!(c["status"], "ok");
        assert_eq!(c["jsonrpc_id"], "\"abc\"");
        assert_eq!(f["kind"], "failed");
        assert_eq!(f["status"], "error");
        assert_eq!(f["error_message"], "boom");
        assert!(
            f.get("jsonrpc_id").is_none(),
            "failed jsonrpc_id None should be omitted, got {f}"
        );
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
            jsonrpc_id: None,
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
                jsonrpc_id: None,
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
            jsonrpc_id: None,
            ts: "t".into(),
            duration_ms: 1,
            status: "ok".into(),
        });
    }

    /// With [`SpanFieldCaptureLayer`] installed, an adapter running inside an
    /// `mcp_request{profile=...}` > `request{id=...}` scope can pull both
    /// values via [`current_request_context`] — this is the foundation for
    /// per-event `jsonrpc_id` plumbing without a `call_tool` trait change.
    #[test]
    fn current_request_context_walks_request_and_mcp_request_spans() {
        use tracing::Instrument;
        use tracing_subscriber::prelude::*;

        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let id_str = "7".to_string();
                let profile = "work".to_string();
                let outer = tracing::info_span!("mcp_request", profile = %profile);
                let captured = async {
                    let inner = tracing::info_span!("request", method = "tools/call", id = %id_str);
                    async { current_request_context() }.instrument(inner).await
                }
                .instrument(outer)
                .await;
                assert_eq!(captured.jsonrpc_id.as_deref(), Some("7"));
                assert_eq!(captured.profile.as_deref(), Some("work"));
            });
        });
    }

    /// No `request` span on the stack → both fields stay `None`. Adapters
    /// running outside an HTTP-routed request (e.g. background init) must
    /// degrade silently.
    #[test]
    fn current_request_context_without_spans_returns_none() {
        use tracing_subscriber::prelude::*;
        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let ctx = current_request_context();
            assert_eq!(ctx, RequestSpanContext::default());
        });
    }

    /// Sanity-check the "null" id sentinel: notifications would record
    /// `id = "null"`; the visitor must not propagate that as a real id.
    #[test]
    fn null_jsonrpc_id_is_ignored_by_capture_visitor() {
        use tracing::Instrument;
        use tracing_subscriber::prelude::*;
        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let id_str = "null".to_string();
                let span = tracing::info_span!("request", method = "x", id = %id_str);
                let ctx = async { current_request_context() }.instrument(span).await;
                assert_eq!(ctx.jsonrpc_id, None);
            });
        });
    }

    /// The `request` span's `client` field carries a JSON-serialised
    /// [`ClientIdentity`]; [`current_request_context`] must parse it back to
    /// a typed value so adapter event emitters can populate
    /// [`ToolCallEvent::Started::client`] without re-deriving it.
    #[test]
    fn current_request_context_captures_client_from_request_span() {
        use tracing::Instrument;
        use tracing_subscriber::prelude::*;

        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let identity = ClientIdentity {
                    name: Some("claude-ai".into()),
                    version: Some("0.1.0".into()),
                    user_agent: Some("claude-ai/0.1.0".into()),
                    origin: None,
                };
                let client_json = serde_json::to_string(&identity).unwrap();
                let id_str = "9".to_string();
                let span = tracing::info_span!(
                    "request",
                    method = "tools/call",
                    id = %id_str,
                    client = %client_json,
                );
                let ctx = async { current_request_context() }.instrument(span).await;
                assert_eq!(ctx.jsonrpc_id.as_deref(), Some("9"));
                assert_eq!(ctx.client.as_ref(), Some(&identity));
            });
        });
    }

    /// A malformed `client` field on the `request` span must not propagate;
    /// the visitor silently drops the value so dispatch stays resilient.
    #[test]
    fn current_request_context_ignores_malformed_client_field() {
        use tracing::Instrument;
        use tracing_subscriber::prelude::*;
        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let client_str = "not-json".to_string();
                let id_str = "1".to_string();
                let span = tracing::info_span!(
                    "request",
                    method = "tools/call",
                    id = %id_str,
                    client = %client_str,
                );
                let ctx = async { current_request_context() }.instrument(span).await;
                assert_eq!(ctx.client, None);
            });
        });
    }
}
