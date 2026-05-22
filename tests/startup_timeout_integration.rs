//! Integration tests for the startup init timeout flow.
//!
//! Asserts that:
//! 1. The management API socket binds immediately, before adapter inits
//!    have settled and before the MCP TCP listener binds.
//! 2. Every endpoint shows `lifecycle.state == "Initializing"` during the
//!    wait window.
//! 3. The MCP TCP port refuses connections during the wait window.
//! 4. With a small `startup_init_timeout_secs`, the MCP TCP port binds
//!    shortly after the timeout fires even when an adapter is still
//!    initializing.
//! 5. Late-arriving adapters published to the registry are reflected in
//!    subsequent `tools/list` responses over the MCP port.

#![cfg(unix)]

mod common;

use crate::common::api_client::ApiClient;
use crate::common::mcp_client::McpClient;
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::net::TcpStream;

/// Pick a random free port by binding to port 0 and capturing the assigned
/// port. The listener is dropped before returning, so the port is briefly
/// available for the relay to bind.
fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind free port");
    listener.local_addr().unwrap().port()
}

/// Spawn a tiny axum-based HTTP MCP mock that responds to `initialize` after
/// `init_delay`, then to `tools/list` with a single tool whose name is
/// `tool_name`. Returns the port and a shutdown sender.
async fn spawn_slow_http_mcp(
    server_name: &'static str,
    tool_name: &'static str,
    init_delay: Duration,
) -> (u16, tokio::sync::oneshot::Sender<()>) {
    use axum::{routing::post, Json, Router};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind slow http mcp");
    let port = listener.local_addr().unwrap().port();

    let app = Router::new().route(
        "/mcp",
        post(move |Json(body): Json<Value>| async move {
            let method = body["method"].as_str().unwrap_or("");
            let id = body["id"].as_u64().unwrap_or(0);
            match method {
                "initialize" => {
                    tokio::time::sleep(init_delay).await;
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": server_name, "version": "0.1.0" },
                        },
                    }))
                }
                "tools/list" => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": tool_name,
                            "description": "test tool",
                            "inputSchema": { "type": "object", "properties": {} },
                        }],
                    },
                })),
                _ => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {},
                })),
            }
        }),
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });

    (port, shutdown_tx)
}

/// Bind a plain TCP listener that accepts connections but never writes
/// anything back, simulating a hung MCP server. The HTTP adapter will keep
/// `initialize()` pending until its internal request timeout fires.
fn spawn_stalled_tcp() -> (u16, std::sync::mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled tcp");
    let port = listener.local_addr().unwrap().port();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    std::thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("set stalled listener nonblocking");
        let mut held = Vec::new();
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => held.push(stream),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });

    (port, stop_tx)
}

struct SpawnedRelay {
    process: Child,
    port: u16,
    api_socket: PathBuf,
    _temp: TempDir,
    started_at: Instant,
}

impl Drop for SpawnedRelay {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Spawn the relay binary with the given config TOML and capture timing
/// without blocking on management API readiness (we want to observe the
/// short pre-management-up window in some tests).
fn spawn_relay(config_toml: &str) -> SpawnedRelay {
    let port = pick_free_port();
    let temp = TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");
    let token_dir = temp.path().join("tokens");
    let api_socket = temp.path().join("api.sock");
    std::fs::create_dir_all(&token_dir).expect("token dir");
    std::fs::write(&config_path, config_toml).expect("write config");

    let relay_bin = env!("CARGO_BIN_EXE_endara-relay");
    let started_at = Instant::now();
    let process = Command::new(relay_bin)
        .args([
            "start",
            "--config",
            config_path.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .env("ENDARA_TOKEN_DIR", &token_dir)
        .env("ENDARA_API_SOCKET", &api_socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn relay binary");

    SpawnedRelay {
        process,
        port,
        api_socket,
        _temp: temp,
        started_at,
    }
}

async fn wait_management_ready(api: &ApiClient) -> Duration {
    let start = Instant::now();
    api.wait_ready(Duration::from_secs(15)).await;
    start.elapsed()
}

/// Returns true if a TCP connect to `port` on 127.0.0.1 succeeds within
/// `timeout`. Returns false on connection refused / timeout.
async fn tcp_open(port: u16, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, TcpStream::connect(("127.0.0.1", port)))
        .await
        .ok()
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Poll `/api/endpoints` until the named endpoint reaches `state`, or fail
/// after `timeout`. Returns the matching entry as JSON.
async fn wait_lifecycle(api: &ApiClient, name: &str, state: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let body = api.get("/api/endpoints").await;
        if let Some(arr) = body.as_array() {
            for ep in arr {
                if ep["name"].as_str() == Some(name) {
                    last = ep.clone();
                    if ep["lifecycle"]["state"].as_str() == Some(state) {
                        return ep.clone();
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(75)).await;
    }
    panic!("endpoint '{name}' did not reach state '{state}' within {timeout:?}; last: {last:#}");
}

#[tokio::test]
async fn management_ready_before_mcp_tcp_with_all_initializing() {
    // Stalled HTTP endpoint forces every adapter init to stay pending past
    // the management-bind point. We pick a long-ish timeout so the window
    // is observable even on slow CI hardware.
    let (stalled_port, _stop) = spawn_stalled_tcp();
    let config = format!(
        r#"
[relay]
machine_name = "test-machine"
startup_init_timeout_secs = 5

[[endpoints]]
name = "stalled"
transport = "http"
url = "http://127.0.0.1:{stalled_port}/mcp"
"#
    );

    let relay = spawn_relay(&config);
    let api = ApiClient::new(&relay.api_socket);

    // Management API should be up well before the 5s startup timeout fires
    // and before the MCP TCP listener binds. Spec says "~200 ms"; we allow
    // 500 ms of CI slop while still failing fast if a regression pushes
    // mgmt bind significantly past the spec.
    let mgmt_elapsed = wait_management_ready(&api).await;
    assert!(
        mgmt_elapsed < Duration::from_millis(500),
        "management API took {mgmt_elapsed:?} to become ready (expected <500ms)"
    );

    let endpoints = api.get("/api/endpoints").await;
    let arr = endpoints
        .as_array()
        .expect("/api/endpoints returned non-array");
    assert!(!arr.is_empty(), "/api/endpoints empty");
    for ep in arr {
        let state = ep["lifecycle"]["state"].as_str();
        assert_eq!(
            state,
            Some("Initializing"),
            "endpoint '{}' should be Initializing, got {state:?}",
            ep["name"]
        );
    }

    // MCP TCP listener should still be refusing connections during the wait
    // window (we are well before the 5s timeout).
    let elapsed_since_spawn = relay.started_at.elapsed();
    assert!(
        elapsed_since_spawn < Duration::from_secs(3),
        "test is racing; spawn-to-check took {elapsed_since_spawn:?}"
    );
    assert!(
        !tcp_open(relay.port, Duration::from_millis(200)).await,
        "MCP TCP port {} was reachable during startup wait window",
        relay.port
    );
}

#[tokio::test]
async fn mcp_tcp_binds_after_timeout_and_catalog_refreshes() {
    // One healthy endpoint that responds instantly and one slow endpoint
    // whose `initialize()` blocks for ~2 s — longer than the 1 s startup
    // wait, so the MCP TCP listener must bind while the slow endpoint is
    // still Initializing. The slow endpoint then completes, and a fresh
    // `tools/list` should reflect its newly-published tool.
    let (fast_port, _fast_stop) =
        spawn_slow_http_mcp("fast-srv", "fast_echo", Duration::from_millis(0)).await;
    let (slow_port, _slow_stop) =
        spawn_slow_http_mcp("slow-srv", "slow_echo", Duration::from_millis(2_500)).await;

    let config = format!(
        r#"
[relay]
machine_name = "test-machine"
startup_init_timeout_secs = 1

[[endpoints]]
name = "fast"
transport = "http"
url = "http://127.0.0.1:{fast_port}/mcp"

[[endpoints]]
name = "slow"
transport = "http"
url = "http://127.0.0.1:{slow_port}/mcp"
"#
    );

    let relay = spawn_relay(&config);
    let api = ApiClient::new(&relay.api_socket);
    wait_management_ready(&api).await;

    // Wait until the MCP TCP port accepts connections; this must happen
    // soon after the 1 s startup timeout fires. We give generous CI slop on
    // top of the configured 1 s.
    let bind_deadline = relay.started_at + Duration::from_secs(5);
    let mut tcp_bound = false;
    while Instant::now() < bind_deadline {
        if tcp_open(relay.port, Duration::from_millis(100)).await {
            tcp_bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let bind_elapsed = relay.started_at.elapsed();
    assert!(tcp_bound, "MCP TCP port never bound within 5s of spawn");
    assert!(
        bind_elapsed >= Duration::from_millis(900),
        "MCP TCP bound at {bind_elapsed:?}, expected at least 0.9s (startup_init_timeout_secs=1)"
    );
    assert!(
        bind_elapsed < Duration::from_secs(4),
        "MCP TCP bound at {bind_elapsed:?}, expected well before slow endpoint settles (~2.5s)"
    );

    // While slow is still Initializing, tools/list over MCP TCP should
    // contain fast's tool but not slow's.
    let mut mcp = McpClient::new(format!("http://127.0.0.1:{}", relay.port));
    mcp.initialize().await.expect("relay initialize");
    let tools = mcp.list_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("fast_echo")),
        "fast endpoint's tool missing from tools/list: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("slow_echo")),
        "slow endpoint's tool appeared before initialization settled: {names:?}"
    );

    // /api/endpoints should still show the slow endpoint as Initializing.
    let body = api.get("/api/endpoints").await;
    let arr = body.as_array().expect("/api/endpoints array");
    let slow_entry = arr
        .iter()
        .find(|e| e["name"].as_str() == Some("slow"))
        .expect("slow endpoint in /api/endpoints");
    assert_eq!(
        slow_entry["lifecycle"]["state"].as_str(),
        Some("Initializing"),
        "slow endpoint should still be Initializing; got {slow_entry:#}"
    );

    // Once slow's initialize completes (a few hundred ms later), the
    // adapter swaps in and the registry invalidates the catalog cache.
    wait_lifecycle(&api, "slow", "Ready", Duration::from_secs(8)).await;

    let tools_after = mcp.list_tools().await.expect("tools/list after settle");
    let names_after: Vec<&str> = tools_after
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names_after.iter().any(|n| n.contains("slow_echo")),
        "slow_echo missing from refreshed tools/list: {names_after:?}"
    );
    assert!(
        names_after.iter().any(|n| n.contains("fast_echo")),
        "fast_echo missing from refreshed tools/list: {names_after:?}"
    );
}

#[tokio::test]
async fn management_ready_when_oauth_server_url_is_unreachable() {
    // OAuth endpoint pointing at an unrouteable RFC 5737 TEST-NET-1 address
    // with no explicit `token_endpoint`. RFC 8414 discovery would normally
    // run synchronously in the per-endpoint loop and block until the HTTP
    // connect times out (potentially many seconds). The discovery must now
    // run inside the OAuth spawn task so the management API stays responsive.
    let config = r#"
[relay]
machine_name = "test-machine"
allow_insecure_oauth = true
startup_init_timeout_secs = 0

[[endpoints]]
name = "blackhole"
transport = "oauth"
url = "http://192.0.2.1:9/mcp"
oauth_server_url = "http://192.0.2.1:9"
client_id = "stub"
"#;

    let relay = spawn_relay(config);
    let api = ApiClient::new(&relay.api_socket);

    let mgmt_elapsed = wait_management_ready(&api).await;
    assert!(
        mgmt_elapsed < Duration::from_millis(500),
        "management API took {mgmt_elapsed:?} to become ready when an OAuth \
         endpoint's oauth_server_url is unreachable (expected <500ms)"
    );

    // The endpoint should be registered (visible via /api/endpoints) even
    // though its OAuth init is still in flight in the background.
    let endpoints = api.get("/api/endpoints").await;
    let arr = endpoints
        .as_array()
        .expect("/api/endpoints returned non-array");
    assert!(
        arr.iter()
            .any(|ep| ep["name"].as_str() == Some("blackhole")),
        "blackhole endpoint missing from /api/endpoints: {arr:#?}"
    );
}
