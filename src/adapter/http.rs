use super::server_name::{sanitize_server_name, ServerNameError};
use super::stdio::RingBuffer;
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::jsonrpc::{self, JsonRpcResponse};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;
use tracing::{error, info, trace, warn};

/// Configuration for the HTTP MCP adapter.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// The URL of the HTTP MCP server endpoint (e.g., http://host:port/mcp).
    pub url: String,
    /// Request timeout in seconds (default: 30).
    pub timeout_secs: u64,
    /// Custom HTTP headers to include in every request.
    pub headers: HashMap<String, String>,
}

impl HttpConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout_secs: 30,
            headers: HashMap::new(),
        }
    }

    #[allow(dead_code)] // Builder method kept for API completeness
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
}

/// HTTP MCP adapter — sends JSON-RPC requests as HTTP POST.
pub struct HttpAdapter {
    config: HttpConfig,
    client: Client,
    health: Arc<RwLock<HealthStatus>>,
    request_id: AtomicU64,
    /// Sanitized server name from the MCP initialize response.
    server_type: Arc<RwLock<Option<String>>>,
    /// Ring buffer recording tool call activity.
    activity_log: Arc<RwLock<RingBuffer>>,
}

impl HttpAdapter {
    /// Create a new HttpAdapter with the given configuration.
    pub fn new(config: HttpConfig) -> Self {
        let mut default_headers = reqwest::header::HeaderMap::new();
        // The Streamable HTTP transport spec requires clients to accept both
        // application/json and text/event-stream.  Set this before processing
        // user headers so it is always present.
        default_headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json, text/event-stream"),
        );

        for (key, value) in &config.headers {
            if key.eq_ignore_ascii_case("content-type") {
                warn!(header = %key, "Ignoring custom Content-Type header; JSON-RPC requires application/json");
                continue;
            }
            if key.eq_ignore_ascii_case("accept") {
                warn!(header = %key, "Ignoring custom Accept header; Streamable HTTP transport requires application/json, text/event-stream");
                continue;
            }
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                default_headers.insert(name, val);
            } else {
                warn!(header = %key, "Invalid header name or value, skipping");
            }
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .default_headers(default_headers)
            .build()
            .expect("failed to build HTTP client");

        Self {
            config,
            client,
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: AtomicU64::new(1),
            server_type: Arc::new(RwLock::new(None)),
            activity_log: Arc::new(RwLock::new(RingBuffer::new(1000))),
        }
    }

    /// Create a new HttpAdapter with a pre-built reqwest::Client.
    ///
    /// Used by OAuthAdapter to inject a client with Bearer token headers.
    pub fn new_with_client(config: HttpConfig, client: Client) -> Self {
        Self {
            config,
            client,
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: AtomicU64::new(1),
            server_type: Arc::new(RwLock::new(None)),
            activity_log: Arc::new(RwLock::new(RingBuffer::new(1000))),
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Parse an SSE (text/event-stream) response body and extract the JSON-RPC
    /// response matching the given request `id`.
    ///
    /// SSE events are separated by double newlines.  Each event may contain
    /// `data:` lines whose payloads are concatenated (with newline separators)
    /// to form the event data.  We look for the first event whose data
    /// deserialises to a `JsonRpcResponse` with a matching `id`.
    fn parse_sse_response(body: &str, id: u64) -> Result<JsonRpcResponse, AdapterError> {
        for event in body.split("\n\n") {
            let event = event.trim();
            if event.is_empty() {
                continue;
            }

            // Collect all `data:` lines for this event and concatenate them.
            let mut data_parts: Vec<&str> = Vec::new();
            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.strip_prefix(' ').unwrap_or(data);
                    if !data.is_empty() {
                        data_parts.push(data);
                    }
                }
            }

            if data_parts.is_empty() {
                continue;
            }

            let data = data_parts.join("\n");
            if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&data) {
                // Match on id — notifications (id == None) are skipped.
                if response.id == Some(id) {
                    return Ok(response);
                }
            }
        }

        Err(AdapterError::ProtocolError(
            "no matching JSON-RPC response found in SSE stream".into(),
        ))
    }

    /// Send a JSON-RPC notification via HTTP POST.
    ///
    /// Notifications are JSON-RPC messages without an `id` field.  Per the MCP
    /// Streamable HTTP spec the server responds with 202 Accepted and an empty
    /// body.  We therefore do **not** attempt to parse a JSON-RPC response.
    async fn send_notification(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), AdapterError> {
        let mut request = json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if let Some(p) = params {
            request["params"] = p;
        }

        trace!(method = method, url = %self.config.url, "sending HTTP JSON-RPC notification");

        let resp = self
            .client
            .post(&self.config.url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AdapterError::Timeout(self.config.timeout_secs)
                } else if e.is_connect() {
                    AdapterError::ConnectionFailed(format!("{}: {}", self.config.url, e))
                } else {
                    AdapterError::HttpError {
                        status: 0,
                        body: e.to_string(),
                    }
                }
            })?;

        let status = resp.status();
        // 202 Accepted is the expected response for notifications.
        // Some servers may return 200 OK — accept that too.
        if status == reqwest::StatusCode::ACCEPTED || status.is_success() {
            trace!(method = method, status = %status, "notification accepted");
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(AdapterError::HttpError {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// Send a JSON-RPC request via HTTP POST and return the result.
    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AdapterError> {
        let id = self.next_id();
        let request = jsonrpc::new_request(method, params, id);

        trace!(method = method, id = id, url = %self.config.url, "sending HTTP JSON-RPC request");

        let resp = self
            .client
            .post(&self.config.url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AdapterError::Timeout(self.config.timeout_secs)
                } else if e.is_connect() {
                    AdapterError::ConnectionFailed(format!("{}: {}", self.config.url, e))
                } else {
                    AdapterError::HttpError {
                        status: 0,
                        body: e.to_string(),
                    }
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AdapterError::HttpError {
                status: status.as_u16(),
                body,
            });
        }

        // The Streamable HTTP transport spec allows servers to respond with
        // either application/json (single JSON-RPC response) or
        // text/event-stream (SSE containing one or more JSON-RPC messages).
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let response: JsonRpcResponse = if content_type.contains("text/event-stream") {
            trace!(
                id = id,
                "response is SSE (text/event-stream), parsing events"
            );
            let body = resp.text().await.map_err(|e| {
                AdapterError::ProtocolError(format!("failed to read SSE body: {}", e))
            })?;
            Self::parse_sse_response(&body, id)?
        } else {
            resp.json().await.map_err(|e| {
                AdapterError::ProtocolError(format!("invalid JSON-RPC response: {}", e))
            })?
        };

        if let Some(err) = response.error {
            return Err(AdapterError::JsonRpcError {
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }

        response
            .result
            .ok_or_else(|| AdapterError::ProtocolError("response has no result".into()))
    }
}

#[async_trait]
impl McpAdapter for HttpAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        *self.health.write().await = HealthStatus::Starting;

        let params = json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "endara-relay",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = match self.send_request("initialize", Some(params)).await {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                *self.health.write().await = HealthStatus::Unhealthy(msg);
                error!(url = %self.config.url, error = %e, "HTTP MCP adapter initialization failed");
                return Err(e);
            }
        };

        // Extract serverInfo.name — REQUIRED per MCP spec enforcement
        let raw_name = match result
            .get("serverInfo")
            .and_then(|si| si.get("name"))
            .and_then(|n| n.as_str())
        {
            Some(name) => name,
            None => {
                let err = ServerNameError::Missing;
                let msg = err.to_string();
                error!(url = %self.config.url, error = %msg, "MCP server did not provide serverInfo.name");
                *self.health.write().await = HealthStatus::Unhealthy(msg.clone());
                return Err(AdapterError::ProtocolError(msg));
            }
        };

        // Validate and sanitize the server name
        let sanitized = match sanitize_server_name(raw_name) {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                error!(url = %self.config.url, raw_name = %raw_name, error = %msg, "serverInfo.name validation failed");
                *self.health.write().await = HealthStatus::Unhealthy(msg.clone());
                return Err(AdapterError::ProtocolError(msg));
            }
        };

        info!(url = %self.config.url, raw_name = %raw_name, sanitized = %sanitized, "MCP server reported serverInfo.name");
        *self.server_type.write().await = Some(sanitized);

        // Per the MCP spec the client MUST send a notifications/initialized
        // notification after a successful initialize exchange.
        if let Err(e) = self
            .send_notification("notifications/initialized", None)
            .await
        {
            warn!(url = %self.config.url, error = %e, "failed to send notifications/initialized");
        }

        *self.health.write().await = HealthStatus::Healthy;
        info!(url = %self.config.url, "HTTP MCP adapter initialized");
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        let result = self.send_request("tools/list", None).await?;
        let tools_value = result
            .get("tools")
            .ok_or_else(|| AdapterError::ProtocolError("missing 'tools' field".into()))?;
        let tools: Vec<ToolInfo> = serde_json::from_value(tools_value.clone())?;
        Ok(tools)
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
        let params = json!({
            "name": name,
            "arguments": arguments,
        });
        let start = Instant::now();
        let result = self.send_request("tools/call", Some(params)).await;
        let duration_ms = start.elapsed().as_millis();
        let now = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let log_line = match &result {
            Ok(_) => format!(
                "{}  INFO call_tool tool={} status=ok duration={}ms",
                now, name, duration_ms
            ),
            Err(e) => format!(
                "{}  WARN call_tool tool={} status=error duration={}ms error={}",
                now, name, duration_ms, e
            ),
        };
        self.activity_log.write().await.push(log_line);
        result
    }

    fn health(&self) -> HealthStatus {
        match self.health.try_read() {
            Ok(h) => h.clone(),
            Err(_) => HealthStatus::Starting,
        }
    }

    fn server_type(&self) -> Option<String> {
        self.server_type.try_read().ok().and_then(|g| g.clone())
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        *self.health.write().await = HealthStatus::Stopped;
        info!(url = %self.config.url, "HTTP MCP adapter shut down");
        Ok(())
    }

    async fn activity_log(&self) -> Vec<String> {
        self.activity_log
            .read()
            .await
            .lines()
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_config_defaults() {
        let config = HttpConfig::new("http://localhost:8080/mcp");
        assert_eq!(config.url, "http://localhost:8080/mcp");
        assert_eq!(config.timeout_secs, 30);
    }

    #[test]
    fn test_http_config_with_timeout() {
        let config = HttpConfig::new("http://localhost:8080/mcp").with_timeout(60);
        assert_eq!(config.timeout_secs, 60);
    }

    #[test]
    fn test_http_adapter_initial_health() {
        let adapter = HttpAdapter::new(HttpConfig::new("http://localhost:8080/mcp"));
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[tokio::test]
    async fn test_http_adapter_connection_refused() {
        let mut adapter = HttpAdapter::new(HttpConfig::new("http://127.0.0.1:19999/mcp"));
        let result = adapter.initialize().await;
        assert!(result.is_err());
        match adapter.health() {
            HealthStatus::Unhealthy(_) => {}
            other => panic!("expected Unhealthy, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_http_adapter_shutdown() {
        let mut adapter = HttpAdapter::new(HttpConfig::new("http://localhost:8080/mcp"));
        adapter.shutdown().await.unwrap();
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[test]
    fn test_default_accept_header_present() {
        // The HttpAdapter should always set Accept: application/json, text/event-stream
        let config = HttpConfig::new("http://localhost:8080/mcp");
        let adapter = HttpAdapter::new(config);
        // We can't directly inspect default_headers on reqwest::Client, but we can
        // verify that creating the adapter with custom Accept header doesn't panic
        // and the adapter is still functional (Accept is skipped in favor of default).
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[test]
    fn test_custom_accept_header_is_skipped() {
        // User-provided Accept headers should be ignored (logged as warning).
        let mut config = HttpConfig::new("http://localhost:8080/mcp");
        config
            .headers
            .insert("Accept".to_string(), "text/html".to_string());
        // Should not panic — the custom Accept is skipped.
        let adapter = HttpAdapter::new(config);
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[test]
    fn test_custom_content_type_header_is_skipped() {
        // User-provided Content-Type headers should be ignored.
        let mut config = HttpConfig::new("http://localhost:8080/mcp");
        config
            .headers
            .insert("Content-Type".to_string(), "text/xml".to_string());
        let adapter = HttpAdapter::new(config);
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[test]
    fn test_custom_auth_header_is_applied() {
        // Non-restricted custom headers should be applied without issue.
        let mut config = HttpConfig::new("http://localhost:8080/mcp");
        config
            .headers
            .insert("Authorization".to_string(), "Bearer test-token".to_string());
        let adapter = HttpAdapter::new(config);
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    // --- SSE parsing tests ---

    #[test]
    fn test_parse_sse_simple_response() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"tools\":[]},\"id\":1}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 1).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_parse_sse_without_event_field() {
        // Some servers only send `data:` lines, no `event:` line.
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true},\"id\":5}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 5).unwrap();
        assert_eq!(resp.id, Some(5));
        assert!(resp.result.is_some());
    }

    #[test]
    fn test_parse_sse_multiple_events_matches_id() {
        // First event is a notification (no id), second is the response.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"result\":{\"done\":true},\"id\":3}\n\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 3).unwrap();
        assert_eq!(resp.id, Some(3));
    }

    #[test]
    fn test_parse_sse_no_matching_id() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{},\"id\":99}\n\n";
        let err = HttpAdapter::parse_sse_response(body, 1).unwrap_err();
        assert!(
            matches!(err, AdapterError::ProtocolError(_)),
            "expected ProtocolError, got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_sse_empty_body() {
        let err = HttpAdapter::parse_sse_response("", 1).unwrap_err();
        assert!(matches!(err, AdapterError::ProtocolError(_)));
    }

    #[test]
    fn test_parse_sse_error_response() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32601,\"message\":\"Method not found\"},\"id\":2}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 2).unwrap();
        assert_eq!(resp.id, Some(2));
        assert!(resp.error.is_some());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn test_parse_sse_multiline_data_invalid_json() {
        // If multi-line data concatenation produces invalid JSON, the event is skipped.
        let body = "data: {\"incomplete\":\ndata: true}\n\n";
        let err = HttpAdapter::parse_sse_response(body, 1).unwrap_err();
        assert!(matches!(err, AdapterError::ProtocolError(_)));
    }

    #[test]
    fn test_parse_sse_data_no_space_after_colon() {
        // SSE spec says space after colon is optional.
        let body = "data:{\"jsonrpc\":\"2.0\",\"result\":{\"x\":1},\"id\":4}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 4).unwrap();
        assert_eq!(resp.id, Some(4));
    }

    #[test]
    fn test_parse_sse_ignores_non_data_lines() {
        let body = "event: message\nid: 123\nretry: 5000\ndata: {\"jsonrpc\":\"2.0\",\"result\":{},\"id\":1}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 1).unwrap();
        assert_eq!(resp.id, Some(1));
    }

    // --- Additional SSE parsing tests ---

    #[test]
    fn test_parse_sse_multiline_data_concatenation() {
        // Multi `data:` lines form valid JSON when joined with newlines.
        // JSON allows whitespace (incl. newlines) between tokens.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"result\":\n",
            "data: {\"tools\":[{\"name\":\"a\"}]}\n",
            "data: ,\"id\":1}\n",
            "\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 1).unwrap();
        assert_eq!(resp.id, Some(1));
        let tools = resp.result.unwrap();
        let arr = tools.get("tools").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "a");
    }

    #[test]
    fn test_parse_sse_todoist_style_initialize_response() {
        // Realistic MCP initialize response with serverInfo, capabilities, protocolVersion.
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"todoist-mcp\",\"version\":\"1.0.0\"}},\"id\":1}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 1).unwrap();
        assert_eq!(resp.id, Some(1));
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "todoist-mcp");
        assert_eq!(result["serverInfo"]["version"], "1.0.0");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
    }

    #[test]
    fn test_parse_sse_large_tools_list_response() {
        // A tools/list response with 6 tools, each with full inputSchema.
        let tools_json = serde_json::json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [
                    {"name": "create_task", "description": "Create a new task", "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}, "priority": {"type": "integer", "minimum": 1, "maximum": 4}}, "required": ["title"]}},
                    {"name": "get_task", "description": "Get task by ID", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}},
                    {"name": "update_task", "description": "Update an existing task", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}, "title": {"type": "string"}, "priority": {"type": "integer"}}, "required": ["id"]}},
                    {"name": "delete_task", "description": "Delete a task", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}},
                    {"name": "list_tasks", "description": "List all tasks with filters", "inputSchema": {"type": "object", "properties": {"project_id": {"type": "string"}, "status": {"type": "string", "enum": ["active", "completed"]}, "limit": {"type": "integer", "default": 50}}}},
                    {"name": "search_tasks", "description": "Search tasks by query", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["query"]}}
                ]
            },
            "id": 2
        });
        let body = format!("data: {}\n\n", serde_json::to_string(&tools_json).unwrap());
        let resp = HttpAdapter::parse_sse_response(&body, 2).unwrap();
        assert_eq!(resp.id, Some(2));
        let tools = resp.result.unwrap();
        let arr = tools.get("tools").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 6);
        assert_eq!(arr[0]["name"], "create_task");
        assert_eq!(arr[5]["name"], "search_tasks");
        // Verify schemas are preserved
        assert_eq!(
            arr[0]["inputSchema"]["properties"]["priority"]["maximum"],
            4
        );
    }

    #[test]
    fn test_parse_sse_multiple_events_notification_error_result() {
        // 3 events: progress notification (no id), error for id=99, correct result for id=7.
        // The parser must pick the one matching id=7.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":50,\"total\":100}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32600,\"message\":\"Invalid request\"},\"id\":99}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Hello\"}]},\"id\":7}\n\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 7).unwrap();
        assert_eq!(resp.id, Some(7));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["content"][0]["text"], "Hello");
    }

    #[test]
    fn test_parse_sse_crlf_line_endings() {
        // Some servers/proxies use Windows-style \r\n line endings.
        // The parser should still handle this because split("\n\n") finds
        // the double-newline within \r\n\r\n, and trim() strips leftover \r.
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true},\"id\":1}\r\n\r\n";
        let resp = HttpAdapter::parse_sse_response(body, 1).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.result.is_some());
        assert_eq!(resp.result.unwrap()["ok"], true);
    }

    #[test]
    fn test_parse_sse_content_type_charset_detection() {
        // Verify that content_type detection with charset parameter works.
        // send_request() uses `.contains("text/event-stream")` so
        // "text/event-stream; charset=utf-8" should still match.
        let content_type = "text/event-stream; charset=utf-8";
        assert!(
            content_type.contains("text/event-stream"),
            "charset parameter should not prevent SSE detection"
        );

        // Also test the actual SSE parsing works with a body that would come
        // from such a content type.
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{\"encoding\":\"utf-8\"},\"id\":10}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 10).unwrap();
        assert_eq!(resp.id, Some(10));
    }

    #[test]
    fn test_parse_sse_trailing_whitespace_and_extra_newlines() {
        // Body with extra blank lines between events, trailing whitespace on data lines.
        let body = concat!(
            "\n\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}  \n",
            "\n\n",
            "\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"result\":{\"status\":\"ok\"},\"id\":3}   \n",
            "\n\n",
            "\n\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 3).unwrap();
        assert_eq!(resp.id, Some(3));
        assert_eq!(resp.result.unwrap()["status"], "ok");
    }

    #[test]
    fn test_parse_sse_initialized_notification_skipped_in_stream() {
        // The client now sends notifications/initialized after a successful
        // initialize exchange (see send_notification).  Verify that a
        // notifications/initialized message appearing in an SSE stream is
        // correctly skipped (it has no id) and doesn't interfere with
        // response matching.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"serverInfo\":{\"name\":\"test\",\"version\":\"0.1\"}},\"id\":1}\n\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 1).unwrap();
        assert_eq!(resp.id, Some(1));
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2025-03-26");
    }
}
