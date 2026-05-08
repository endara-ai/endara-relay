//! Test-only HTTP client that dials the relay's management API over its
//! Unix-domain socket (the `/api/*` routes are no longer reachable over TCP).

#![allow(dead_code)]

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use tokio::net::UnixStream;

/// Lightweight UDS client that supports the verbs used by the relay's
/// integration tests (GET / POST / DELETE with optional JSON body).
#[derive(Clone, Debug)]
pub struct ApiClient {
    socket_path: PathBuf,
}

#[allow(dead_code)]
impl ApiClient {
    pub fn new<P: AsRef<Path>>(socket_path: P) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Send a request and return `(status, json_body, raw_bytes)`.
    /// `body` may be `None` for GET / DELETE / empty POST.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, Bytes) {
        self.request_with_headers(method, path, body, &[]).await
    }

    pub async fn request_with_headers(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        extra_headers: &[(&str, &str)],
    ) -> (StatusCode, Value, Bytes) {
        #[cfg(unix)]
        {
            let stream = UnixStream::connect(&self.socket_path)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "ApiClient: connect to {} failed: {}",
                        self.socket_path.display(),
                        e
                    )
                });
            let io = TokioIo::new(stream);
            let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .expect("ApiClient: HTTP handshake failed");
            tokio::spawn(async move {
                let _ = conn.await;
            });

            let body_bytes = body
                .as_ref()
                .map(|v| Bytes::from(serde_json::to_vec(v).expect("serialize body")))
                .unwrap_or_default();
            let mut builder = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "relay.local")
                .header("accept", "application/json");
            if body.is_some() {
                builder = builder.header("content-type", "application/json");
            }
            for (k, v) in extra_headers {
                builder = builder.header(*k, *v);
            }
            let req = builder
                .body(Full::new(body_bytes))
                .expect("ApiClient: build request");

            let resp = sender
                .send_request(req)
                .await
                .expect("ApiClient: send_request failed");
            let status = resp.status();
            let raw = resp
                .into_body()
                .collect()
                .await
                .expect("ApiClient: read body")
                .to_bytes();
            let json = if raw.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&raw).unwrap_or(Value::Null)
            };
            (status, json, raw)
        }
        #[cfg(not(unix))]
        {
            let _ = (method, path, body, extra_headers);
            unimplemented!("ApiClient is Unix-only in tests");
        }
    }

    pub async fn get(&self, path: &str) -> Value {
        self.request("GET", path, None).await.1
    }

    pub async fn get_status_json(&self, path: &str) -> (StatusCode, Value) {
        let (s, v, _) = self.request("GET", path, None).await;
        (s, v)
    }

    pub async fn post_json(&self, path: &str, body: Value) -> Value {
        self.request("POST", path, Some(body)).await.1
    }

    pub async fn post_status_json(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let (s, v, _) = self.request("POST", path, Some(body)).await;
        (s, v)
    }

    pub async fn post_empty(&self, path: &str) -> Value {
        self.request("POST", path, None).await.1
    }

    pub async fn post_empty_status(&self, path: &str) -> (StatusCode, Value) {
        let (s, v, _) = self.request("POST", path, None).await;
        (s, v)
    }

    pub async fn delete(&self, path: &str) -> (StatusCode, Value) {
        let (s, v, _) = self.request("DELETE", path, None).await;
        (s, v)
    }

    /// Wait until the socket exists and `/api/status` responds with 2xx.
    pub async fn wait_ready(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "ApiClient: relay did not become ready within {:?} on {}",
                    timeout,
                    self.socket_path.display()
                );
            }
            if self.socket_path.exists() {
                #[cfg(unix)]
                if UnixStream::connect(&self.socket_path).await.is_ok() {
                    let (status, _, _) = self.request("GET", "/api/status", None).await;
                    if status.is_success() {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(75)).await;
        }
    }
}
