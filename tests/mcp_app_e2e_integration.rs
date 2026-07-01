//! End-to-end multi-endpoint MCP App integration test (T8).
//!
//! Drives the full MCP App round-trip across two upstreams through the
//! registry's real dispatch path (`merged_catalog_with_lookup`,
//! `route_tool_call`, `route_resource_read`) to prove:
//!
//! 1. `tools/list` carries endpoint-namespaced UI pointers — each tool's
//!    `_meta.ui.resourceUri` (and the `openai/outputTemplate` alias) is
//!    wrapped to its OWNING endpoint via `mcp-relay://` (slot #1).
//! 2. `tools/call` results carry wrapped resource refs scoped to the
//!    owning endpoint (`_meta` UI pointers — slot #2; `resource` /
//!    `resource_link` content blocks — slot #3).
//! 3. `resources/read` of each wrapped URI dispatches to the CORRECT
//!    upstream — endpoint A's wrapped URI never reaches B (slot #6),
//!    and the de-wrapped original URI is what the adapter receives.
//! 4. NEGATIVE (DD1): URL-shaped data inside a `text` block and inside
//!    `structuredContent` is NOT rewritten — only the enumerated slots
//!    are touched.

use async_trait::async_trait;
use endara_relay::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use endara_relay::registry::AdapterRegistry;
use endara_relay::resource_uri::decode_resource_uri;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A mock MCP App adapter that:
/// - advertises a single `open_app` tool whose descriptor carries
///   `_meta.ui.resourceUri = ui://app/{label}/main` + alias
///   `openai/outputTemplate` (distinct per endpoint),
/// - returns a `tools/call` result with `_meta` UI pointers + a mix of
///   `resource`/`resource_link`/`text`/`structuredContent` blocks,
/// - implements `read_resource` echoing the URI it received plus the
///   endpoint label so cross-routing is detectable from the body alone.
struct AppMockAdapter {
    /// Distinct per-endpoint label baked into every URI it advertises and
    /// surfaced in `read_resource` responses so cross-routing is visible.
    label: String,
    /// Captures the most recent URI passed to `read_resource` so the test
    /// can confirm slot #6 reversed the wrapper before dispatch.
    last_read_uri: Arc<Mutex<Option<String>>>,
}

impl AppMockAdapter {
    fn new(label: &str) -> Self {
        Self {
            label: label.into(),
            last_read_uri: Arc::new(Mutex::new(None)),
        }
    }

    fn ui_main(&self) -> String {
        format!("ui://app/{}/main", self.label)
    }
    fn ui_template(&self) -> String {
        format!("ui://app/{}/template", self.label)
    }
    fn ui_embedded(&self) -> String {
        format!("ui://app/{}/embedded", self.label)
    }
    fn ui_widget(&self) -> String {
        format!("ui://app/{}/widget", self.label)
    }
    /// URL-shaped data planted inside `text` and `structuredContent`. DD1
    /// promises this is preserved byte-for-byte because neither is an
    /// enumerated rewrite slot.
    fn payload_url(&self) -> String {
        format!("https://example.com/api/{}", self.label)
    }
}

#[async_trait]
impl McpAdapter for AppMockAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        Ok(vec![ToolInfo {
            name: "open_app".into(),
            description: Some(format!("Open the {} app", self.label)),
            input_schema: json!({"type": "object"}),
            meta: Some(json!({
                "ui": { "resourceUri": self.ui_main() },
                "openai/outputTemplate": self.ui_template(),
            })),
            ..Default::default()
        }])
    }

    async fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, AdapterError> {
        Ok(json!({
            "_meta": {
                "ui": { "resourceUri": self.ui_main() },
                "openai/outputTemplate": self.ui_template(),
            },
            "content": [
                // DD1 negative: a URL inside a `text` block must NOT be
                // rewritten — `type==text` is not an enumerated slot.
                { "type": "text", "text": format!("see {} for details", self.payload_url()) },
                // Slot #3a: `type==resource` → `resource.uri` wrapped.
                { "type": "resource", "resource": { "uri": self.ui_embedded(), "mimeType": "text/html" } },
                // Slot #3b: `type==resource_link` → `uri` wrapped.
                { "type": "resource_link", "uri": self.ui_widget(), "name": "Widget" },
            ],
            // DD1 negative: `structuredContent` is left untouched even when
            // it contains URI-shaped fields — the relay never tree-walks
            // arbitrary JSON for URI strings.
            "structuredContent": {
                "homepage": self.payload_url(),
                "uri": format!("ui://app/{}/inside-structured", self.label),
            },
            "isError": false,
        }))
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, AdapterError> {
        *self.last_read_uri.lock().await = Some(uri.to_string());
        Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "text/plain",
                // The body carries the endpoint label so cross-routing
                // (alpha's wrapper reaching beta) would be visible here.
                "text": format!("body-from-{}", self.label),
            }]
        }))
    }

    fn health(&self) -> HealthStatus {
        HealthStatus::Healthy
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// Register two `AppMockAdapter`s (`alpha`, `beta`) so the registry is in
/// multi-endpoint mode (active_count >= 2 → DD5 wrapping ON). Returns the
/// registry plus handles to each adapter's `last_read_uri` recorder so the
/// test can inspect what slot #6 actually dispatched.
async fn setup_two_endpoints() -> (
    AdapterRegistry,
    Arc<Mutex<Option<String>>>,
    Arc<Mutex<Option<String>>>,
) {
    let registry = AdapterRegistry::new();

    let alpha = AppMockAdapter::new("alpha");
    let alpha_last = alpha.last_read_uri.clone();
    registry
        .register(
            "alpha".into(),
            Box::new(alpha),
            "stdio".into(),
            None,
            Some("alpha".into()),
        )
        .await;

    let beta = AppMockAdapter::new("beta");
    let beta_last = beta.last_read_uri.clone();
    registry
        .register(
            "beta".into(),
            Box::new(beta),
            "stdio".into(),
            None,
            Some("beta".into()),
        )
        .await;

    (registry, alpha_last, beta_last)
}

#[tokio::test]
async fn mcp_app_round_trip_two_endpoints_full_pipeline() {
    let (registry, alpha_last, beta_last) = setup_two_endpoints().await;

    // ---------- 1. tools/list: endpoint-namespaced UI pointers ----------
    let (catalog, _lookup) = registry.merged_catalog_with_lookup().await;
    assert_eq!(catalog.len(), 2, "two endpoints, one tool each");

    let alpha_tool = catalog
        .iter()
        .find(|t| t.name == "alpha__open_app")
        .expect("alpha__open_app present in catalog");
    let beta_tool = catalog
        .iter()
        .find(|t| t.name == "beta__open_app")
        .expect("beta__open_app present in catalog");

    let alpha_descriptor_uri = alpha_tool
        .meta
        .as_ref()
        .and_then(|m| m["ui"]["resourceUri"].as_str())
        .expect("alpha descriptor _meta.ui.resourceUri wrapped");
    assert!(
        alpha_descriptor_uri.starts_with("mcp-relay://"),
        "slot #1 must wrap alpha's UI pointer; got {alpha_descriptor_uri}"
    );
    assert_eq!(
        decode_resource_uri(alpha_descriptor_uri).unwrap(),
        ("alpha".into(), "ui://app/alpha/main".into()),
        "alpha's descriptor must wrap to alpha, not beta"
    );

    let beta_descriptor_uri = beta_tool
        .meta
        .as_ref()
        .and_then(|m| m["ui"]["resourceUri"].as_str())
        .expect("beta descriptor _meta.ui.resourceUri wrapped");
    assert_eq!(
        decode_resource_uri(beta_descriptor_uri).unwrap(),
        ("beta".into(), "ui://app/beta/main".into()),
        "beta's descriptor must wrap to beta, not alpha"
    );

    // Slot #1 alias: `openai/outputTemplate` wrapped on each side.
    let alpha_descriptor_tmpl = alpha_tool.meta.as_ref().unwrap()["openai/outputTemplate"]
        .as_str()
        .expect("alpha descriptor openai/outputTemplate wrapped");
    assert_eq!(
        decode_resource_uri(alpha_descriptor_tmpl).unwrap(),
        ("alpha".into(), "ui://app/alpha/template".into()),
    );

    // ---------- 2. tools/call: per-endpoint wrapped result URIs ---------
    let alpha_call = registry
        .route_tool_call("alpha__open_app", json!({}))
        .await
        .expect("alpha tools/call dispatches");
    let beta_call = registry
        .route_tool_call("beta__open_app", json!({}))
        .await
        .expect("beta tools/call dispatches");

    // Slot #2: `_meta.ui.resourceUri` + `openai/outputTemplate` alias.
    let alpha_result_ui = alpha_call["_meta"]["ui"]["resourceUri"]
        .as_str()
        .expect("alpha result _meta.ui.resourceUri present");
    assert_eq!(
        decode_resource_uri(alpha_result_ui).unwrap(),
        ("alpha".into(), "ui://app/alpha/main".into()),
    );
    let alpha_result_tmpl = alpha_call["_meta"]["openai/outputTemplate"]
        .as_str()
        .expect("alpha result openai/outputTemplate present");
    assert_eq!(
        decode_resource_uri(alpha_result_tmpl).unwrap(),
        ("alpha".into(), "ui://app/alpha/template".into()),
    );

    // Slot #3: `content[type==resource].resource.uri` wrapped.
    let alpha_resource_uri = alpha_call["content"][1]["resource"]["uri"]
        .as_str()
        .expect("alpha resource block uri present");
    assert_eq!(
        decode_resource_uri(alpha_resource_uri).unwrap(),
        ("alpha".into(), "ui://app/alpha/embedded".into()),
    );
    // Slot #3: `content[type==resource_link].uri` wrapped.
    let alpha_link_uri = alpha_call["content"][2]["uri"]
        .as_str()
        .expect("alpha resource_link uri present");
    assert_eq!(
        decode_resource_uri(alpha_link_uri).unwrap(),
        ("alpha".into(), "ui://app/alpha/widget".into()),
    );

    // Mirror checks on beta confirm per-endpoint scoping (no cross-talk):
    // a beta tool result must wrap to `beta`, never to `alpha`.
    let beta_resource_uri = beta_call["content"][1]["resource"]["uri"].as_str().unwrap();
    assert_eq!(
        decode_resource_uri(beta_resource_uri).unwrap(),
        ("beta".into(), "ui://app/beta/embedded".into()),
    );
    let beta_link_uri = beta_call["content"][2]["uri"].as_str().unwrap();
    assert_eq!(
        decode_resource_uri(beta_link_uri).unwrap(),
        ("beta".into(), "ui://app/beta/widget".into()),
    );

    // ---------- 4. DD1 NEGATIVE: text + structuredContent untouched -----
    // The text body still carries the literal URL — DD1 forbids tree-walking
    // arbitrary JSON for URI-shaped strings.
    assert_eq!(
        alpha_call["content"][0]["text"], "see https://example.com/api/alpha for details",
        "slot #2/#3 rewrite must not touch text blocks (DD1)"
    );
    assert_eq!(
        alpha_call["structuredContent"]["homepage"], "https://example.com/api/alpha",
        "URL inside structuredContent must not be rewritten (DD1)"
    );
    assert_eq!(
        alpha_call["structuredContent"]["uri"], "ui://app/alpha/inside-structured",
        "ui:// inside structuredContent must not be rewritten (DD1)"
    );
    // Same negative checks for beta — the rewrite isn't accidentally
    // re-running on the second endpoint either.
    assert_eq!(
        beta_call["content"][0]["text"],
        "see https://example.com/api/beta for details",
    );
    assert_eq!(
        beta_call["structuredContent"]["uri"],
        "ui://app/beta/inside-structured",
    );

    // ---------- 3. resources/read: per-endpoint routing, no crosstalk ---
    // (a) Reading the wrapped URI taken from alpha's tools/call dispatches
    //     to alpha and the adapter receives the de-wrapped original.
    let alpha_read = registry
        .route_resource_read(alpha_resource_uri)
        .await
        .expect("alpha resources/read dispatches");
    assert_eq!(
        alpha_read["contents"][0]["text"], "body-from-alpha",
        "alpha's wrapped URI must reach alpha, never beta"
    );
    assert_eq!(
        alpha_read["contents"][0]["uri"], "ui://app/alpha/embedded",
        "adapter must receive the de-wrapped original URI"
    );
    assert_eq!(
        alpha_last.lock().await.as_deref(),
        Some("ui://app/alpha/embedded"),
        "alpha adapter recorded the de-wrapped URI"
    );
    assert!(
        beta_last.lock().await.is_none(),
        "beta adapter must NOT have served alpha's wrapped URI"
    );

    // (b) Symmetric: beta's wrapped descriptor URI reaches beta only.
    let beta_read = registry
        .route_resource_read(beta_descriptor_uri)
        .await
        .expect("beta resources/read dispatches");
    assert_eq!(beta_read["contents"][0]["text"], "body-from-beta");
    assert_eq!(beta_read["contents"][0]["uri"], "ui://app/beta/main");
    assert_eq!(
        beta_last.lock().await.as_deref(),
        Some("ui://app/beta/main"),
    );

    // (c) Closed-loop tools/call → resources/read on the second endpoint:
    //     the wrapped resource_link URI in beta's tool result round-trips
    //     back to its owning upstream with the original URI re-exposed.
    let beta_widget_read = registry
        .route_resource_read(beta_link_uri)
        .await
        .expect("beta widget read dispatches");
    assert_eq!(beta_widget_read["contents"][0]["text"], "body-from-beta");
    assert_eq!(
        beta_last.lock().await.as_deref(),
        Some("ui://app/beta/widget"),
        "later read overwrites the recorder with beta's widget URI"
    );

    // (d) Final cross-routing guard: re-reading alpha's descriptor URI
    //     after the beta reads must still land on alpha — the dispatcher
    //     keys off the wrapper authority, never on call order.
    let alpha_main_read = registry
        .route_resource_read(alpha_descriptor_uri)
        .await
        .expect("alpha descriptor read dispatches");
    assert_eq!(alpha_main_read["contents"][0]["text"], "body-from-alpha");
    assert_eq!(
        alpha_last.lock().await.as_deref(),
        Some("ui://app/alpha/main"),
    );
}
