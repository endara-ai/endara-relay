//! Streamable-HTTP MCP client built on the Workers `fetch` API.
//!
//! The upstream session (`Mcp-Session-Id` + negotiated protocol version) is
//! cached so most requests cost one round trip. A `404` on a cached session
//! means the upstream dropped it: re-initialize once and retry.

use crate::cache::Cache;
use crate::config::Endpoint;
use crate::oauth::OAuth;
use futures_util::future::{select, Either};
use serde_json::{json, Value};
use std::cell::Cell;
use std::cell::RefCell;
use std::time::Duration;
use worker::wasm_bindgen::JsValue;
use worker::{Delay, Fetch, Headers, Method, Request, RequestInit};

const CLIENT_PROTOCOL_VERSION: &str = "2025-06-18";
const SESSION_TTL_SECS: u64 = 3600;
const MAX_ERROR_BODY: usize = 300;

thread_local! {
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

fn next_id() -> u64 {
    NEXT_ID.with(|c| {
        let id = c.get();
        c.set(id.wrapping_add(1));
        id
    })
}

#[derive(Debug)]
pub enum UpstreamError {
    /// The upstream answered with a JSON-RPC error object.
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// Transport, HTTP status, timeout or framing failure.
    Transport(String),
    /// HTTP 401 from an OAuth server: refresh the token and retry.
    Unauthorized(String),
}

impl UpstreamError {
    pub fn message(&self) -> String {
        match self {
            UpstreamError::Rpc { message, .. } => message.clone(),
            UpstreamError::Transport(m) | UpstreamError::Unauthorized(m) => m.clone(),
        }
    }

    pub fn is_method_not_found(&self) -> bool {
        matches!(self, UpstreamError::Rpc { code: -32601, .. })
    }
}

#[derive(Debug, Clone)]
struct Session {
    id: Option<String>,
    protocol_version: String,
}

pub struct Upstream<'a> {
    pub endpoint: &'a Endpoint,
    cache: &'a Cache,
    timeout: Duration,
    oauth: Option<&'a OAuth>,
    token: RefCell<Option<String>>,
}

struct HttpReply {
    status: u16,
    session_id: Option<String>,
    content_type: String,
    body: String,
}

impl<'a> Upstream<'a> {
    pub fn new(
        endpoint: &'a Endpoint,
        cache: &'a Cache,
        timeout_secs: u64,
        oauth: Option<&'a OAuth>,
    ) -> Self {
        Upstream {
            endpoint,
            cache,
            timeout: Duration::from_secs(timeout_secs),
            oauth,
            token: RefCell::new(None),
        }
    }

    async fn load_token(&self, force_refresh: bool) -> Result<(), UpstreamError> {
        if !self.endpoint.oauth {
            return Ok(());
        }
        let oauth = self.oauth.ok_or_else(|| {
            UpstreamError::Transport("OAuth is not available for this request".into())
        })?;
        let token = oauth
            .access_token(self.endpoint, force_refresh)
            .await
            .map_err(|e| UpstreamError::Transport(e.message()))?;
        *self.token.borrow_mut() = Some(token);
        Ok(())
    }

    fn session_key(&self) -> String {
        format!(
            "session:{}:{}",
            self.endpoint.name, self.endpoint.fingerprint
        )
    }

    /// Send one JSON-RPC request and return its `result`. For OAuth servers
    /// a 401 forces one token refresh and one retry.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, UpstreamError> {
        self.load_token(false).await?;
        match self.request_once(method, &params).await {
            Err(UpstreamError::Unauthorized(_)) => {
                self.load_token(true).await?;
                match self.request_once(method, &params).await {
                    Err(UpstreamError::Unauthorized(m)) => Err(UpstreamError::Transport(format!(
                        "'{}' rejected a fresh token; sign in again in the relay web app ({m})",
                        self.endpoint.name
                    ))),
                    r => r,
                }
            }
            r => r,
        }
    }

    async fn request_once(&self, method: &str, params: &Value) -> Result<Value, UpstreamError> {
        let cached = self.cached_session().await;
        let had_cached = cached.is_some();
        let session = match cached {
            Some(s) => s,
            None => self.initialize().await?,
        };
        match self.send_request(&session, method, params).await {
            Err(SendError::SessionExpired) if had_cached => {
                self.cache.delete(&self.session_key()).await;
                let fresh = self.initialize().await?;
                match self.send_request(&fresh, method, params).await {
                    Err(SendError::SessionExpired) => Err(UpstreamError::Transport(
                        "upstream rejected a fresh session (HTTP 404)".into(),
                    )),
                    Err(SendError::Other(e)) => Err(e),
                    Ok(v) => Ok(v),
                }
            }
            Err(SendError::SessionExpired) => Err(UpstreamError::Transport(
                "upstream returned HTTP 404".into(),
            )),
            Err(SendError::Other(e)) => Err(e),
            Ok(v) => Ok(v),
        }
    }

    async fn cached_session(&self) -> Option<Session> {
        let v = self.cache.get(&self.session_key()).await?;
        Some(Session {
            id: v.get("id").and_then(|s| s.as_str()).map(str::to_string),
            protocol_version: v.get("pv")?.as_str()?.to_string(),
        })
    }

    async fn initialize(&self) -> Result<Session, UpstreamError> {
        let id = next_id();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": CLIENT_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "endara-relay-worker", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        let reply = self.post(&body, None, None).await?;
        if !(200..300).contains(&reply.status) {
            return Err(status_error("initialize", &reply));
        }
        let result = extract_result(&reply, id)?;
        let session = Session {
            id: reply.session_id.clone(),
            protocol_version: result
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(CLIENT_PROTOCOL_VERSION)
                .to_string(),
        };
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let ack = self
            .post(
                &note,
                session.id.as_deref(),
                Some(&session.protocol_version),
            )
            .await?;
        if !(200..300).contains(&ack.status) {
            return Err(status_error("notifications/initialized", &ack));
        }
        self.cache
            .put(
                &self.session_key(),
                json!({ "id": session.id, "pv": session.protocol_version }),
                SESSION_TTL_SECS,
            )
            .await;
        Ok(session)
    }

    async fn send_request(
        &self,
        session: &Session,
        method: &str,
        params: &Value,
    ) -> Result<Value, SendError> {
        let id = next_id();
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let reply = self
            .post(
                &body,
                session.id.as_deref(),
                Some(&session.protocol_version),
            )
            .await
            .map_err(SendError::Other)?;
        if reply.status == 404 && session.id.is_some() {
            return Err(SendError::SessionExpired);
        }
        if !(200..300).contains(&reply.status) {
            return Err(SendError::Other(status_error(method, &reply)));
        }
        extract_result(&reply, id).map_err(SendError::Other)
    }

    async fn post(
        &self,
        body: &Value,
        session_id: Option<&str>,
        protocol_version: Option<&str>,
    ) -> Result<HttpReply, UpstreamError> {
        let t = |e: worker::Error| UpstreamError::Transport(e.to_string());
        let headers = Headers::new();
        for (k, v) in &self.endpoint.headers {
            headers.set(k, v).map_err(t)?;
        }
        headers.set("content-type", "application/json").map_err(t)?;
        headers
            .set("accept", "application/json, text/event-stream")
            .map_err(t)?;
        if let Some(sid) = session_id {
            headers.set("mcp-session-id", sid).map_err(t)?;
        }
        if let Some(pv) = protocol_version {
            headers.set("mcp-protocol-version", pv).map_err(t)?;
        }
        if let Some(token) = self.token.borrow().as_deref() {
            headers
                .set("authorization", &format!("Bearer {token}"))
                .map_err(t)?;
        }
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(JsValue::from_str(&body.to_string())));
        let req = Request::new_with_init(&self.endpoint.url, &init).map_err(t)?;

        let exchange = async {
            let mut resp = Fetch::Request(req).send().await?;
            let status = resp.status_code();
            let session_id = resp.headers().get("mcp-session-id")?;
            let content_type = resp.headers().get("content-type")?.unwrap_or_default();
            let body = resp.text().await?;
            Ok::<_, worker::Error>(HttpReply {
                status,
                session_id,
                content_type,
                body,
            })
        };
        match select(Box::pin(exchange), Box::pin(Delay::from(self.timeout))).await {
            Either::Left((Ok(reply), _)) if reply.status == 401 && self.endpoint.oauth => Err(
                UpstreamError::Unauthorized(format!("'{}' returned HTTP 401", self.endpoint.name)),
            ),
            Either::Left((r, _)) => r.map_err(|e| {
                UpstreamError::Transport(format!("request to '{}' failed: {e}", self.endpoint.name))
            }),
            Either::Right(_) => Err(UpstreamError::Transport(format!(
                "upstream '{}' timed out after {}s",
                self.endpoint.name,
                self.timeout.as_secs()
            ))),
        }
    }
}

enum SendError {
    SessionExpired,
    Other(UpstreamError),
}

fn status_error(method: &str, reply: &HttpReply) -> UpstreamError {
    let mut snippet: String = reply.body.chars().take(MAX_ERROR_BODY).collect();
    if snippet.len() < reply.body.len() {
        snippet.push('…');
    }
    let hint = if reply.status == 401 || reply.status == 403 {
        " (check the endpoint's auth headers)"
    } else {
        ""
    };
    UpstreamError::Transport(format!(
        "{method}: upstream returned HTTP {}{hint}: {snippet}",
        reply.status
    ))
}

/// Find the response to request `id` in a JSON body or an SSE stream.
fn extract_result(reply: &HttpReply, id: u64) -> Result<Value, UpstreamError> {
    let messages: Vec<Value> = if reply.content_type.starts_with("text/event-stream") {
        parse_sse_messages(&reply.body)
    } else {
        match serde_json::from_str::<Value>(&reply.body) {
            Ok(Value::Array(items)) => items,
            Ok(v) => vec![v],
            Err(e) => {
                return Err(UpstreamError::Transport(format!(
                    "upstream sent invalid JSON: {e}"
                )))
            }
        }
    };
    let response = messages
        .into_iter()
        .find(|m| m.get("id").and_then(|v| v.as_u64()) == Some(id) && m.get("method").is_none())
        .ok_or_else(|| {
            UpstreamError::Transport("upstream sent no response for the request".into())
        })?;
    if let Some(err) = response.get("error") {
        return Err(UpstreamError::Rpc {
            code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32603),
            message: err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("upstream error")
                .to_string(),
            data: err.get("data").cloned(),
        });
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

fn parse_sse_messages(body: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut data = String::new();
    let flush = |data: &mut String, out: &mut Vec<Value>| {
        if !data.is_empty() {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                match v {
                    Value::Array(items) => out.extend(items),
                    v => out.push(v),
                }
            }
            data.clear();
        }
    };
    for line in body.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            flush(&mut data, &mut out);
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    flush(&mut data, &mut out);
    out
}
