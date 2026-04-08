use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// A lightweight MCP client for integration tests.
///
/// Sends properly-formed JSON-RPC requests with the headers a real MCP client
/// would send (Content-Type, Accept, Mcp-Session-Id).
#[allow(dead_code)]
pub struct McpClient {
    base_url: String,
    session_id: Option<String>,
    next_id: AtomicU64,
    http: reqwest::Client,
}

#[allow(dead_code)]
impl McpClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            session_id: None,
            next_id: AtomicU64::new(1),
            http: reqwest::Client::new(),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base_url)
    }

    /// Build a request with standard MCP headers.
    fn request(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(self.mcp_url())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(ref sid) = self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }

        req.json(body)
    }

    /// Perform the initialize → notifications/initialized handshake.
    /// Returns the full initialize result JSON.
    pub async fn initialize(&mut self) -> Result<Value, String> {
        let id = self.next_id();
        let body = json!({
          "jsonrpc": "2.0",
          "method": "initialize",
          "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "integration-test", "version": "0.1" }
          },
          "id": id
        });

        let resp = self
            .request(&body)
            .send()
            .await
            .map_err(|e| format!("initialize request failed: {e}"))?;

        // Capture session id from response header if present
        if let Some(sid) = resp.headers().get("mcp-session-id") {
            self.session_id = sid.to_str().ok().map(|s| s.to_string());
        }

        let result: Value = resp
            .json()
            .await
            .map_err(|e| format!("initialize parse failed: {e}"))?;

        // Send notifications/initialized
        let notif = json!({
          "jsonrpc": "2.0",
          "method": "notifications/initialized"
        });
        let _ = self.request(&notif).send().await;

        Ok(result)
    }

    /// Call tools/list and return the tools array.
    pub async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let id = self.next_id();
        let body = json!({
          "jsonrpc": "2.0",
          "method": "tools/list",
          "id": id
        });
        let resp: Value = self
            .request(&body)
            .send()
            .await
            .map_err(|e| format!("list_tools request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("list_tools parse failed: {e}"))?;

        resp["result"]["tools"]
            .as_array()
            .cloned()
            .ok_or_else(|| format!("unexpected list_tools response: {resp}"))
    }

    /// Call a tool by name with given arguments.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value, String> {
        let id = self.next_id();
        let body = json!({
          "jsonrpc": "2.0",
          "method": "tools/call",
          "params": { "name": name, "arguments": args },
          "id": id
        });
        let resp: Value = self
            .request(&body)
            .send()
            .await
            .map_err(|e| format!("call_tool request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("call_tool parse failed: {e}"))?;

        Ok(resp)
    }

    /// Call resources/list.
    pub async fn list_resources(&self) -> Result<Vec<Value>, String> {
        let id = self.next_id();
        let body = json!({
          "jsonrpc": "2.0",
          "method": "resources/list",
          "id": id
        });
        let resp: Value = self
            .request(&body)
            .send()
            .await
            .map_err(|e| format!("list_resources failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("list_resources parse failed: {e}"))?;

        resp["result"]["resources"]
            .as_array()
            .cloned()
            .ok_or_else(|| format!("unexpected list_resources response: {resp}"))
    }

    /// Call prompts/list.
    pub async fn list_prompts(&self) -> Result<Vec<Value>, String> {
        let id = self.next_id();
        let body = json!({
          "jsonrpc": "2.0",
          "method": "prompts/list",
          "id": id
        });
        let resp: Value = self
            .request(&body)
            .send()
            .await
            .map_err(|e| format!("list_prompts failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("list_prompts parse failed: {e}"))?;

        resp["result"]["prompts"]
            .as_array()
            .cloned()
            .ok_or_else(|| format!("unexpected list_prompts response: {resp}"))
    }

    /// Get a prompt by name with arguments.
    pub async fn get_prompt(&self, name: &str, args: Value) -> Result<Value, String> {
        let id = self.next_id();
        let body = json!({
          "jsonrpc": "2.0",
          "method": "prompts/get",
          "params": { "name": name, "arguments": args },
          "id": id
        });
        let resp: Value = self
            .request(&body)
            .send()
            .await
            .map_err(|e| format!("get_prompt failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("get_prompt parse failed: {e}"))?;

        Ok(resp)
    }

    /// Send a raw notification (no id field). Verifies 202 Accepted.
    pub async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        let body = json!({
          "jsonrpc": "2.0",
          "method": method,
          "params": params
        });
        let resp = self
            .request(&body)
            .send()
            .await
            .map_err(|e| format!("notification failed: {e}"))?;

        let status = resp.status().as_u16();
        if status == 202 || status == 200 {
            Ok(())
        } else {
            Err(format!("expected 202 for notification, got {status}"))
        }
    }

    /// Send a batch of JSON-RPC messages and return the batched response.
    pub async fn batch(&self, items: Vec<Value>) -> Result<Vec<Value>, String> {
        let body = Value::Array(items);
        let resp: Value = self
            .request(&body)
            .send()
            .await
            .map_err(|e| format!("batch request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("batch parse failed: {e}"))?;

        resp.as_array()
            .cloned()
            .ok_or_else(|| format!("expected array response for batch, got: {resp}"))
    }
}
