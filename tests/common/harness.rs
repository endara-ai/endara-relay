use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

/// Spawns the relay binary as a subprocess, picks random ports, writes a temp
/// config, waits for `/api/status` readiness, and kills on drop.
#[allow(dead_code)]
pub struct RelayHarness {
    pub port: u16,
    pub config_path: PathBuf,
    pub token_dir: PathBuf,
    pub temp_dir: TempDir,
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn relay binary");

        let harness = Self {
            port,
            config_path,
            token_dir,
            temp_dir,
            process,
        };

        // Wait for the relay to become ready
        harness.wait_ready(Duration::from_secs(30)).await;
        harness
    }

    /// Base URL for MCP and management endpoints.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// URL for the MCP endpoint.
    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base_url())
    }

    /// URL for the management API status.
    pub fn mgmt_url(&self) -> String {
        format!("{}/api/status", self.base_url())
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
        let client = reqwest::Client::new();
        let endpoints_url = format!("{}/api/endpoints", self.base_url());
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "Timed out waiting for endpoint '{}' to become healthy",
                    endpoint_name
                ));
            }

            if let Ok(resp) = client.get(endpoints_url.as_str()).send().await {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(arr) = body.as_array() {
                        for ep in arr {
                            if ep["name"].as_str() == Some(endpoint_name)
                                && ep["health"].as_str() == Some("healthy")
                            {
                                return Ok(());
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Poll `/api/status` until it returns 200, or panic after timeout.
    async fn wait_ready(&self, timeout: Duration) {
        let client = reqwest::Client::new();
        let url = self.mgmt_url();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "Relay did not become ready within {:?}. URL: {}",
                    timeout, url
                );
            }

            match client.get(url.as_str()).send().await {
                Ok(resp) if resp.status().is_success() => return,
                _ => {}
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
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
