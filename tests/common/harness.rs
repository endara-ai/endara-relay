use crate::common::api_client::ApiClient;
use serde_json::Value;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

/// Spawns the relay binary as a subprocess, picks random ports, writes a temp
/// config, waits for `/api/status` readiness, and kills on drop.
///
/// `/api/*` is served exclusively over a Unix-domain socket; `api()` returns
/// a client wired to that socket. `/mcp` and `/healthz` continue to be served
/// over TCP at `base_url()`.
#[allow(dead_code)]
pub struct RelayHarness {
    pub port: u16,
    pub config_path: PathBuf,
    pub token_dir: PathBuf,
    pub api_socket_path: PathBuf,
    pub temp_dir: TempDir,
    api: ApiClient,
    process: Child,
}

#[allow(dead_code)]
impl RelayHarness {
    /// Spawn a relay with the given config TOML body.
    ///
    /// Picks a free port, writes config to a temp dir, sets ENDARA_TOKEN_DIR,
    /// and waits for `/api/status` to return 200.
    pub async fn start(config_toml: &str) -> Self {
        let port = pick_free_port();
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let token_dir = temp_dir.path().join("tokens");
        let api_socket_path = temp_dir.path().join("api.sock");
        std::fs::create_dir_all(&token_dir).expect("failed to create token dir");
        std::fs::write(&config_path, config_toml).expect("failed to write config");

        let relay_bin = env!("CARGO_BIN_EXE_endara-relay");

        let process = Command::new(relay_bin)
            .args([
                "start",
                "--config",
                config_path.to_str().unwrap(),
                "--port",
                &port.to_string(),
            ])
            .env("ENDARA_TOKEN_DIR", &token_dir)
            .env("ENDARA_API_SOCKET", &api_socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn relay binary");

        let api = ApiClient::new(&api_socket_path);
        let harness = Self {
            port,
            config_path,
            token_dir,
            api_socket_path,
            temp_dir,
            api: api.clone(),
            process,
        };

        // Wait for the relay to become ready (UDS bound + /api/status 2xx).
        api.wait_ready(Duration::from_secs(30)).await;
        harness
    }

    /// Base URL for MCP endpoints (`/mcp`, `/healthz`, `/oauth/callback`).
    /// `/api/*` is **not** exposed on TCP; use `api()` instead.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// URL for the MCP endpoint.
    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base_url())
    }

    /// Client for the management API (`/api/*`) over the Unix-domain socket.
    pub fn api(&self) -> &ApiClient {
        &self.api
    }

    /// Send a JSON-RPC request to /mcp and return the parsed response.
    pub async fn rpc(&self, body: serde_json::Value) -> serde_json::Value {
        let resp = self.rpc_raw(body).await;
        resp.json::<serde_json::Value>()
            .await
            .expect("failed to parse JSON response")
    }

    /// Send a JSON-RPC request to /mcp and return the raw response.
    pub async fn rpc_raw(&self, body: serde_json::Value) -> reqwest::Response {
        let client = reqwest::Client::new();
        client
            .post(self.mcp_url())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .expect("failed to send request to relay")
    }

    /// Wait until the named endpoint reports Healthy via the management API.
    pub async fn wait_healthy(&self, endpoint_name: &str, timeout: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "Timed out waiting for endpoint '{}' to become healthy",
                    endpoint_name
                ));
            }

            let body = self.api.get("/api/endpoints").await;
            if let Some(arr) = body.as_array() {
                for ep in arr {
                    if ep["name"].as_str() == Some(endpoint_name)
                        && ep["health"].as_str() == Some("healthy")
                    {
                        return Ok(());
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for RelayHarness {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Pick a random free port by binding to port 0 and extracting the assigned port.
fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind to free port");
    listener.local_addr().unwrap().port()
}

/// Helper: poll /api/endpoints until we see a specific lifecycle state for an endpoint.
#[allow(dead_code)]
pub async fn wait_for_lifecycle_state(
    harness: &RelayHarness,
    endpoint_name: &str,
    expected_state: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_state: Option<String> = None;
    let mut last_body: Option<Value> = None;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "Timeout waiting for endpoint '{}' to reach state '{}'. Last state: {:?}, Last body: {:?}",
                endpoint_name, expected_state, last_state, last_body
            ));
        }

        let body = harness.api().get("/api/endpoints").await;
        last_body = Some(body.clone());
        if let Some(endpoints) = body.as_array() {
            for ep in endpoints {
                if ep["name"].as_str() == Some(endpoint_name) {
                    last_state = ep["lifecycle"]["state"].as_str().map(|s| s.to_string());
                    if let Some(state) = ep["lifecycle"]["state"].as_str() {
                        if state == expected_state {
                            return Ok(ep.clone());
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
