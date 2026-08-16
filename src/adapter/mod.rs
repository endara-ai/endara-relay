pub mod http;
pub mod oauth;
pub mod server_name;
pub mod server_type_resolution;
pub mod sse;
pub mod stdio;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::time::Duration;

/// Short, dedicated timeout for the upstream `server/discover` dialect probe.
///
/// The probe must fail fast to the legacy `initialize` fallback when an upstream
/// silently drops the unknown request (per the MCP `2026-07-28` stdio
/// Backward-Compatibility rule: "any other error, or no response within a
/// reasonable timeout → legacy"). Without this bound the probe would inherit the
/// 30s per-request timeout and stall startup. Only the probe is bounded this
/// short; normal (non-probe) requests keep their full transport timeout.
pub(crate) const DISCOVER_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Merge extra top-level `tools/call` params into an outgoing params object,
/// alongside the `name`/`arguments` the adapter already set. Used by the
/// transport adapters to transparently forward the MCP 2026-07-28 multi
/// round-trip fields (`inputResponses`, `requestState`) and any sibling params.
///
/// Keys never collide with `name`/`arguments` because the inbound handler
/// strips those before populating `request_params`. An empty map is a no-op, so
/// a normal terminal `tools/call` request is forwarded byte-for-byte unchanged.
pub(crate) fn merge_request_params(
    params: &mut Value,
    request_params: serde_json::Map<String, Value>,
) {
    if request_params.is_empty() {
        return;
    }
    if let Some(obj) = params.as_object_mut() {
        for (k, v) in request_params {
            obj.insert(k, v);
        }
    }
}

/// Health status of an adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HealthStatus {
    Healthy,
    Unhealthy(String),
    Starting,
    Stopped,
}

impl fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HealthStatus::Healthy => write!(f, "healthy"),
            HealthStatus::Unhealthy(reason) => write!(f, "unhealthy: {}", reason),
            HealthStatus::Starting => write!(f, "starting"),
            HealthStatus::Stopped => write!(f, "stopped"),
        }
    }
}

/// Information about a tool exposed by an MCP server.
///
/// Fields beyond the four originals (`name`, `description`, `inputSchema`,
/// `annotations`) are preserved verbatim so the merged `tools/list` catalog
/// stays lossless for MCP Apps clients:
/// - `title`, `output_schema` (`outputSchema`), and `_meta` are modeled
///   explicitly because they carry MCP Apps UI pointers
///   (`_meta.ui.resourceUri`, OpenAI alias `_meta["openai/outputTemplate"]`)
///   and structured-output metadata the relay must round-trip without
///   inventing or dropping siblings.
/// - `extra` catches any future tool-descriptor field not modeled above via
///   `#[serde(flatten)]` so the relay survives upstream schema additions
///   without a code change.
///
/// All new fields are `#[serde(default, skip_serializing_if = ...)]` so an
/// upstream tool that sends none of them re-serializes byte-for-byte
/// identically to the pre-extension shape, and `#[derive(Default)]` lets
/// existing struct literals stay terse via `..Default::default()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    /// User-facing tool title (MCP 2026-07-28). Passed through verbatim from
    /// the upstream tool descriptor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Structured-output schema describing the shape of `structuredContent`
    /// in `tools/call` results. Passed through verbatim; the relay does not
    /// validate against it today (mirrors `inputSchema` passthrough).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Tool-level `_meta` object. Preserved verbatim so downstream MCP Apps
    /// pointers (`_meta.ui.resourceUri`, alias `_meta["openai/outputTemplate"]`)
    /// and arbitrary upstream metadata reach the client. Subsequent tasks
    /// (T6) wrap UI pointers in place without losing siblings.
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    /// Catch-all for any tool-descriptor field not modeled above. Preserved
    /// across deserialize → re-serialize so future MCP spec additions survive
    /// aggregation through `merged_catalog` without a relay code change.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

/// Max character count retained from `HttpError.body` when formatting an
/// `AdapterError` for display. Upstream MCP servers occasionally return
/// multi-KB HTML error pages or stack traces; including those verbatim
/// in `Failed` events would leak noise into the management UI / logs.
pub const HTTP_ERROR_BODY_MAX_CHARS: usize = 200;

/// Return at most `max_chars` characters from `s`, appending `"…"` if the
/// input was longer. Counts by `char` (not byte) so the result is always a
/// valid UTF-8 boundary even for multi-byte input.
pub fn truncate_for_display(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

/// Format an error together with its full `source()` chain, joined with ": ".
///
/// reqwest's top-level `Display` for transport failures is just "error sending
/// request for url (…)" — the actionable detail (e.g. "tcp connect error:
/// No route to host (os error 65)") lives in the source chain, so walk it and
/// append every layer. Layers whose text is already embedded in the message
/// are skipped to avoid duplication.
pub(crate) fn format_error_chain(err: &dyn std::error::Error) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        let layer = s.to_string();
        if !msg.contains(&layer) {
            msg.push_str(": ");
            msg.push_str(&layer);
        }
        source = s.source();
    }
    msg
}

/// Chain-format a connect error, prefixing `url` only when the chain does not
/// already name it: reqwest's top-level `Display` usually embeds
/// "for url (…)", so an unconditional prefix would duplicate the URL. The
/// containment check also tries the parsed/normalized form of `url` (e.g.
/// reqwest renders "http://host:1" as "http://host:1/").
pub(crate) fn connect_error_message(url: &str, err: &dyn std::error::Error) -> String {
    let chain = format_error_chain(err);
    if message_names_url(url, &chain) {
        chain
    } else {
        format!("{url}: {chain}")
    }
}

/// True when `message` already contains `url`, either verbatim or in its
/// parsed/normalized form.
fn message_names_url(url: &str, message: &str) -> bool {
    if message.contains(url) {
        return true;
    }
    url::Url::parse(url).is_ok_and(|u| message.contains(u.as_str()))
}

/// Errors that can occur in adapter operations.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("failed to spawn process: {0}")]
    ProcessSpawnFailed(String),

    #[error("process crashed: {0}")]
    ProcessCrashed(String),

    #[error("operation timed out after {0}s")]
    Timeout(u64),

    #[error("JSON-RPC error {code}: {message}")]
    JsonRpcError {
        code: i64,
        message: String,
        data: Option<Value>,
    },

    #[error("protocol error: {0}")]
    ProtocolError(String),

    #[error("adapter not initialized")]
    NotInitialized,

    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error(
        "HTTP error {status}: {}",
        truncate_for_display(body, HTTP_ERROR_BODY_MAX_CHARS)
    )]
    HttpError { status: u16, body: String },

    #[error("Authentication required for endpoint '{endpoint}': {message}")]
    AuthenticationRequired { endpoint: String, message: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Trait for MCP server adapters.
///
/// Each adapter manages the lifecycle of a connection to an MCP server
/// and provides methods to interact with it.
#[async_trait]
pub trait McpAdapter: Send + Sync {
    /// Initialize the adapter and perform the MCP handshake.
    async fn initialize(&mut self) -> Result<(), AdapterError>;

    /// List the tools available from the MCP server.
    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError>;

    /// Upstream-provided `ttlMs` freshness hint (SEP-2549) from the most recent
    /// successful [`Self::list_tools`] call, in milliseconds.
    ///
    /// Returns `Some(ms)` only when the upstream peer negotiated the 2026-07-28
    /// dialect **and** included a top-level `ttlMs` on its `tools/list` result;
    /// the relay-as-client honors it as the cache freshness window in
    /// [`crate::registry::RegisteredAdapter::cached_list_tools`]. Returns `None`
    /// for legacy upstreams or when no hint was sent, preserving the existing
    /// purely event-driven cache behavior. Implementors clamp a negative
    /// upstream `ttlMs` to `0` (immediately stale).
    ///
    /// The default impl returns `None` (placeholder / read-only adapters never
    /// emit a caching hint).
    async fn list_tools_ttl_ms(&self) -> Option<u64> {
        None
    }

    /// Call a tool on the MCP server.
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError>;

    /// Call a tool, forwarding extra top-level `tools/call` params verbatim to
    /// the upstream alongside `name`/`arguments`. Used to transparently proxy
    /// the MCP 2026-07-28 multi round-trip fields (`inputResponses`,
    /// `requestState`) and any other sibling params (e.g. `_meta`) so the relay
    /// neither interprets nor strips them.
    ///
    /// The default implementation ignores the extra params and delegates to
    /// [`Self::call_tool`]; transports that proxy raw JSON-RPC override it to
    /// merge `request_params` into the outgoing params object.
    async fn call_tool_with_request_params(
        &self,
        name: &str,
        arguments: Value,
        _request_params: serde_json::Map<String, Value>,
    ) -> Result<Value, AdapterError> {
        self.call_tool(name, arguments).await
    }

    /// List concrete resources advertised by the upstream MCP server.
    ///
    /// Returns the raw `resources` array from the upstream `resources/list`
    /// response (each element preserved verbatim so MCP fields like `name`,
    /// `description`, `mimeType`, `_meta`, etc. round-trip untouched). The
    /// registry wraps each `uri` per slot #4 before forwarding to the client.
    ///
    /// The default implementation returns an empty list so adapters that do
    /// not support resources (placeholder adapters, transports that haven't
    /// implemented the passthrough yet) participate silently. Concrete
    /// transports override this to forward `resources/list` upstream.
    async fn list_resources(&self) -> Result<Vec<Value>, AdapterError> {
        Ok(vec![])
    }

    /// List resource templates (RFC 6570 URI templates) advertised by the
    /// upstream MCP server.
    ///
    /// Returns the raw `resourceTemplates` array from the upstream
    /// `resources/templates/list` response. The registry wraps each
    /// `uriTemplate` per slot #5 before forwarding to the client; the
    /// wrapper-scheme encoding preserves RFC 6570 `{var}` braces so variable
    /// expansion on the client side is unaffected.
    ///
    /// The default implementation returns an empty list so adapters that do
    /// not support resource templates participate silently.
    async fn list_resource_templates(&self) -> Result<Vec<Value>, AdapterError> {
        Ok(vec![])
    }

    /// List prompts advertised by the upstream MCP server.
    ///
    /// Returns the raw `prompts` array from the upstream `prompts/list`
    /// response (each element preserved verbatim so MCP fields like
    /// `description`, `arguments`, `_meta`, etc. round-trip untouched). The
    /// registry namespaces each `name` per slot #8 before forwarding to the
    /// client, mirroring the tool-prefix scheme so a prefixed prompt name
    /// later routes back to its owning upstream.
    ///
    /// The default implementation returns an empty list so adapters that do
    /// not support prompts (placeholder adapters, transports that haven't
    /// implemented the passthrough yet) participate silently. Concrete
    /// transports override this to forward `prompts/list` upstream.
    async fn list_prompts(&self) -> Result<Vec<Value>, AdapterError> {
        Ok(vec![])
    }

    /// Fetch a single prompt by name from the upstream MCP server.
    ///
    /// `name` is the original (reverse-prefixed) prompt name — slot #9 inbound
    /// name un-prefixing happens in the registry before dispatch, so adapters
    /// receive the same name the upstream originally advertised via
    /// `prompts/list`. `arguments` is the optional client-supplied `arguments`
    /// object (forwarded verbatim, `null`/missing → no field on the wire). The
    /// returned `Value` is the raw `result` object from the upstream
    /// (`{ messages: [...] , description?: "..." }`); the registry rewrites
    /// the enumerated slot #9 URIs on the returned messages before forwarding
    /// to the client.
    ///
    /// The default implementation returns a JSON-RPC method-not-found error so
    /// adapters that do not support prompts (placeholder adapters, transports
    /// that haven't implemented the passthrough yet) reject fetches cleanly
    /// instead of silently succeeding with an empty payload. Concrete
    /// transports override this to forward `prompts/get` upstream.
    async fn get_prompt(
        &self,
        _name: &str,
        _arguments: Option<Value>,
    ) -> Result<Value, AdapterError> {
        Err(AdapterError::JsonRpcError {
            code: -32601,
            message: "prompts/get not supported by this adapter".to_string(),
            data: None,
        })
    }

    /// Read a single resource by URI from the upstream MCP server.
    ///
    /// `uri` is the original (de-wrapped) resource URI — slot #6 reverse
    /// rewriting happens in the registry before dispatch, so adapters receive
    /// the same URI the upstream originally advertised via `resources/list` or
    /// `resources/templates/list`. The returned `Value` is the raw `result`
    /// object from the upstream (`{ contents: [...] }`); per DD2 the relay
    /// returns it to the client unmodified — URIs inside the resource body
    /// are not rewritten in v1.
    ///
    /// The default implementation returns a JSON-RPC method-not-found error so
    /// adapters that do not support resources (placeholder adapters, transports
    /// that haven't implemented the passthrough yet) reject reads cleanly
    /// instead of silently succeeding with an empty payload. Concrete
    /// transports override this to forward `resources/read` upstream.
    async fn read_resource(&self, _uri: &str) -> Result<Value, AdapterError> {
        Err(AdapterError::JsonRpcError {
            code: -32601,
            message: "resources/read not supported by this adapter".to_string(),
            data: None,
        })
    }

    /// Get the current health status.
    fn health(&self) -> HealthStatus;

    /// Shut down the adapter gracefully.
    async fn shutdown(&mut self) -> Result<(), AdapterError>;

    /// Return recent stderr lines from the adapter (if any).
    ///
    /// The default implementation returns an empty list. Adapters that capture
    /// stderr (e.g. STDIO) override this to return buffered output.
    async fn stderr_lines(&self) -> Vec<String> {
        vec![]
    }

    /// Return recent activity log lines (e.g. tool call records).
    ///
    /// The default implementation returns an empty list. Adapters that record
    /// tool call activity (e.g. SSE, HTTP) override this.
    async fn activity_log(&self) -> Vec<String> {
        vec![]
    }

    /// Return the sanitized server name reported by the MCP server during initialize.
    ///
    /// The default implementation returns `None`. Adapters that capture
    /// `serverInfo.name` from the initialize response override this.
    #[allow(dead_code)] // Will be used by upstream callers once prefix routing is wired
    fn server_type(&self) -> Option<String> {
        None
    }

    /// Upstream-derived server name (sanitized + suffix-stripped) independent
    /// of any `server_type_override`. Returns `None` until the initialize
    /// handshake has populated it; adapters that do not capture
    /// `serverInfo.name` keep the default.
    ///
    /// This mirrors what [`McpAdapter::server_type`] would have returned if no
    /// override were configured, and lets the management API surface the
    /// "default name the upstream reports" alongside the effective name so the
    /// desktop UI can show users what they would revert to.
    fn upstream_server_name(&self) -> Option<String> {
        None
    }

    /// Returns the configured `server_type_override` (sanitized through the
    /// existing `effective_server_type` resolver with `None` for the upstream
    /// value), or `None` if no override is configured. Used by the registry to
    /// advertise endpoints before their first successful `initialize` handshake.
    fn configured_server_type(&self) -> Option<String> {
        None
    }

    /// Subscribe to MCP `notifications/tools/list_changed` ticks emitted by the
    /// underlying server. Each `recv()` represents at least one change
    /// notification observed since the previous receive; the registry treats
    /// every tick as an opaque cache-invalidation signal.
    ///
    /// The default implementation returns `None`, indicating the adapter does
    /// not surface tools-changed events. Adapters that handle the MCP
    /// notification override this and return a fresh `Receiver` from a
    /// long-lived `broadcast::Sender`.
    fn subscribe_tools_changed(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        None
    }

    /// Latest container resource stats for the endpoint.
    ///
    /// The default implementation returns `None` (direct-spawn endpoints and
    /// transports that never run containers). The STDIO adapter overrides
    /// this with the most recent sample from its background stats poller
    /// when the endpoint runs inside a container.
    fn container_stats(&self) -> Option<crate::container_stats::ContainerStats> {
        None
    }

    /// Isolation outcome (configured vs actual) of the endpoint's last spawn.
    ///
    /// The default implementation returns `None` (transports that never spawn
    /// processes). The STDIO adapter overrides this with the outcome recorded
    /// at spawn time, including the configured-container → direct-spawn
    /// fallback when no container runtime is available.
    fn isolation_state(&self) -> Option<crate::adapter::stdio::IsolationState> {
        None
    }

    /// Wire a shared [`crate::events::ToolCallEventBus`] into the adapter so
    /// `call_tool` can publish typed `started` / `completed` / `failed`
    /// events for the desktop overlay's SSE stream.
    ///
    /// Default impl is a no-op so placeholder adapters
    /// ([`FailedAdapter`], [`StartingAdapter`]) and any future read-only
    /// adapters don't need to participate. Concrete transports (STDIO, SSE,
    /// HTTP, OAuth) override this and stash the bus for use during
    /// `call_tool`.
    fn set_event_bus(&self, _bus: crate::events::ToolCallEventBus) {}
}

/// A placeholder adapter registered when the real adapter fails to initialize.
///
/// Reports [`HealthStatus::Unhealthy`] so the endpoint appears in the management
/// UI as offline. Restarting the endpoint via the management API will call
/// `initialize()` on the real adapter again (handled by the restart endpoint).
pub struct FailedAdapter {
    error_message: String,
    /// Sanitized `server_type_override` carried over from the originating
    /// `EndpointConfig`. Surfaced via [`McpAdapter::configured_server_type`]
    /// so endpoints with a configured override still appear in the
    /// advertisement list when their real adapter fails to initialize.
    server_type_override: Option<String>,
}

impl FailedAdapter {
    /// Create a new failed adapter with the error message from initialization.
    pub fn new(error_message: String) -> Self {
        Self {
            error_message,
            server_type_override: None,
        }
    }

    /// Attach the originating endpoint's `server_type_override` so this
    /// failed adapter still advertises its configured type via
    /// [`McpAdapter::configured_server_type`].
    pub fn with_server_type_override(mut self, override_field: Option<String>) -> Self {
        self.server_type_override = override_field;
        self
    }
}

#[async_trait]
impl McpAdapter for FailedAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        // A failed adapter cannot be re-initialized in place.
        // The restart endpoint should replace this with a real adapter.
        Err(AdapterError::ConnectionFailed(format!(
            "server failed to initialize: {}",
            self.error_message
        )))
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        Ok(vec![])
    }

    async fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, AdapterError> {
        Err(AdapterError::ConnectionFailed(format!(
            "server failed to initialize: {}",
            self.error_message
        )))
    }

    fn health(&self) -> HealthStatus {
        HealthStatus::Unhealthy(self.error_message.clone())
    }

    fn configured_server_type(&self) -> Option<String> {
        crate::adapter::server_type_resolution::effective_server_type(
            self.server_type_override.clone(),
            None,
        )
        .map(|s| s.to_lowercase())
    }

    async fn stderr_lines(&self) -> Vec<String> {
        vec![format!("[ERROR] {}", self.error_message)]
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// A placeholder adapter registered while the real adapter is initializing.
///
/// Reports [`HealthStatus::Starting`] so the endpoint appears in the management
/// UI with a spinner. Once initialization completes, the caller replaces this
/// adapter in the registry with the real (or failed) adapter.
pub struct StartingAdapter;

#[async_trait]
impl McpAdapter for StartingAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        Err(AdapterError::NotInitialized)
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        Ok(vec![])
    }

    async fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, AdapterError> {
        Err(AdapterError::NotInitialized)
    }

    fn health(&self) -> HealthStatus {
        HealthStatus::Starting
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A validation-failed endpoint registered as a `FailedAdapter` should
    /// still surface its configured `server_type_override` via
    /// [`McpAdapter::configured_server_type`], mirroring the bootstrap path in
    /// `main.rs` and the runtime path in `watcher.rs`.
    #[test]
    fn failed_adapter_surfaces_server_type_override() {
        let adapter = FailedAdapter::new("validation failed".to_string())
            .with_server_type_override(Some("Broken".to_string()));
        assert_eq!(adapter.configured_server_type(), Some("broken".to_string()));
    }

    /// Without an override, `configured_server_type` returns `None` so the
    /// endpoint is omitted from the advertisement list until it transitions
    /// to a real adapter that knows its server type.
    #[test]
    fn failed_adapter_without_override_returns_none() {
        let adapter = FailedAdapter::new("validation failed".to_string());
        assert_eq!(adapter.configured_server_type(), None);
    }

    /// `format_error_chain` walks the full `source()` chain, joining each
    /// layer with ": " and skipping layers already embedded in the message.
    #[test]
    fn format_error_chain_joins_all_source_layers() {
        #[derive(Debug)]
        struct Layer {
            msg: &'static str,
            source: Option<Box<dyn std::error::Error>>,
        }
        impl std::fmt::Display for Layer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.msg)
            }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.source.as_deref()
            }
        }

        let root = Layer {
            msg: "No route to host (os error 65)",
            source: None,
        };
        let mid = Layer {
            msg: "tcp connect error",
            source: Some(Box::new(root)),
        };
        let top = Layer {
            msg: "error sending request",
            source: Some(Box::new(mid)),
        };
        assert_eq!(
            format_error_chain(&top),
            "error sending request: tcp connect error: No route to host (os error 65)"
        );
    }

    /// When the chain already names the URL (reqwest's "for url (…)"), no
    /// prefix is added — the URL must appear exactly once.
    #[test]
    fn connect_error_message_skips_prefix_when_chain_names_url() {
        let url = "http://192.168.1.10:8123/mcp";
        let err = std::io::Error::other(format!(
            "error sending request for url ({url}): tcp connect error"
        ));
        let msg = connect_error_message(url, &err);
        assert_eq!(msg, err.to_string());
        assert_eq!(msg.matches(url).count(), 1);
    }

    /// reqwest renders the parsed URL, which may differ from the configured
    /// string (e.g. a trailing slash added to an empty path) — the normalized
    /// form also counts as "already named".
    #[test]
    fn connect_error_message_recognizes_normalized_url() {
        let err = std::io::Error::other(
            "error sending request for url (http://127.0.0.1:1/): tcp connect error",
        );
        let msg = connect_error_message("http://127.0.0.1:1", &err);
        assert_eq!(msg, err.to_string());
    }

    /// When the chain does not mention the URL, it is prefixed so the message
    /// still identifies the endpoint.
    #[test]
    fn connect_error_message_prefixes_url_when_absent_from_chain() {
        let err = std::io::Error::other("tcp connect error: Connection refused (os error 111)");
        assert_eq!(
            connect_error_message("http://192.168.1.10:8123/mcp", &err),
            "http://192.168.1.10:8123/mcp: tcp connect error: Connection refused (os error 111)"
        );
    }

    /// A layer whose text is already embedded in the accumulated message is
    /// not appended again (some errors include their source in `Display`).
    #[test]
    fn format_error_chain_skips_duplicated_layers() {
        let io = std::io::Error::other("disk full");
        let wrapped = std::io::Error::other(io);
        assert_eq!(format_error_chain(&wrapped), "disk full");
    }

    /// End-to-end against a real reqwest transport failure: a request to a
    /// closed local port must surface the OS-level cause (e.g. "Connection
    /// refused"), not just reqwest's top-level "error sending request" text.
    #[tokio::test]
    async fn format_error_chain_surfaces_os_cause_for_connect_refused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let err = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect_err("request to a closed port must fail");
        assert!(err.is_connect());

        let chain = format_error_chain(&err);
        assert!(
            chain.starts_with(&err.to_string()),
            "chain must begin with the top-level message, got {chain:?}"
        );
        assert!(
            chain.len() > err.to_string().len(),
            "chain must include more than the top-level message, got {chain:?}"
        );
        assert!(
            chain.to_lowercase().contains("refused"),
            "chain must name the OS-level cause, got {chain:?}"
        );
    }

    #[test]
    fn truncate_for_display_passes_short_input_unchanged() {
        assert_eq!(truncate_for_display("short body", 200), "short body");
        let exact: String = "a".repeat(200);
        assert_eq!(truncate_for_display(&exact, 200), exact);
    }

    #[test]
    fn truncate_for_display_truncates_long_input_with_ellipsis() {
        let body: String = "a".repeat(500);
        let out = truncate_for_display(&body, 200);
        assert!(
            out.ends_with('…'),
            "expected trailing ellipsis, got {out:?}"
        );
        let n_chars = out.chars().count();
        assert_eq!(n_chars, 201, "expected 200 chars + ellipsis, got {n_chars}");
    }

    #[test]
    fn truncate_for_display_respects_char_boundaries_for_multibyte() {
        // A 300-char string of multi-byte chars must truncate on a char
        // boundary (no UTF-8 panic) and stay within max_chars + 1.
        let body: String = "✓".repeat(300);
        let out = truncate_for_display(&body, 200);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 201);
    }

    #[test]
    fn http_error_display_truncates_oversized_body() {
        let body: String = "x".repeat(1024);
        let err = AdapterError::HttpError {
            status: 500,
            body: body.clone(),
        };
        let rendered = err.to_string();
        assert!(rendered.starts_with("HTTP error 500: "));
        assert!(rendered.ends_with('…'));
        // "HTTP error 500: " (16 chars) + 200 chars + "…" (1 char) = 217.
        assert_eq!(rendered.chars().count(), 16 + HTTP_ERROR_BODY_MAX_CHARS + 1);
    }

    #[test]
    fn http_error_display_preserves_short_body_verbatim() {
        let err = AdapterError::HttpError {
            status: 404,
            body: "not found".to_string(),
        };
        assert_eq!(err.to_string(), "HTTP error 404: not found");
    }

    /// D13 — forward direction: an inbound `_meta` carrying W3C Trace Context
    /// (`traceparent`/`tracestate`/`baggage`) is merged onto the outgoing
    /// `tools/call` params verbatim, so the relay forwards it to the upstream
    /// unmodified. The relay is intentionally key-agnostic for `_meta` siblings
    /// — it copies the whole inbound `_meta` rather than enumerating trace keys.
    #[test]
    fn merge_request_params_forwards_meta_trace_context() {
        let mut params = serde_json::json!({ "name": "echo", "arguments": {} });
        let mut request_params = serde_json::Map::new();
        request_params.insert(
            "_meta".to_string(),
            serde_json::json!({
                "traceparent": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
                "tracestate": "vendor=abc",
                "baggage": "userId=42"
            }),
        );
        merge_request_params(&mut params, request_params);
        assert_eq!(
            params["_meta"]["traceparent"],
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
        );
        assert_eq!(params["_meta"]["tracestate"], "vendor=abc");
        assert_eq!(params["_meta"]["baggage"], "userId=42");
        // name/arguments are untouched.
        assert_eq!(params["name"], "echo");
    }

    /// D13 — with no inbound siblings, `merge_request_params` is a no-op, so a
    /// legacy frame that carried no `_meta` stays byte-for-byte unchanged and
    /// the relay never injects an empty `_meta` of its own.
    #[test]
    fn merge_request_params_no_meta_leaves_params_unchanged() {
        let mut params = serde_json::json!({ "name": "echo", "arguments": {} });
        merge_request_params(&mut params, serde_json::Map::new());
        assert!(params.get("_meta").is_none());
    }
}
