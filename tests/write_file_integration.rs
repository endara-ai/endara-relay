//! Integration test: tool → writeFile → disk round-trip for binary payloads.
//!
//! Starts relay with an image fixture MCP server whose `get_image` tool
//! returns a `{ type: "image", mimeType: "image/jpeg", data: <base64> }`
//! content block carrying a synthetic JPEG. A sandbox script finds the
//! image block and writes it via `writeFile(path, img.data, { encoding:
//! "base64" })` into a tempdir configured as the write root, proving the
//! payload reaches disk byte-for-byte without the base64 ever appearing
//! in the script's returned JSON. A companion negative case runs the same
//! script against a relay with no `relay.write_dirs` configured and
//! asserts the `__sandbox_error`-surfaced message names the config key
//! and the desktop setting.

use endara_relay::adapter::stdio::{StdioAdapter, StdioConfig};
use endara_relay::adapter::McpAdapter;
use endara_relay::js_sandbox::{MetaToolHandler, SharedWriteRoots};
use endara_relay::profile_registry::ProfileRegistry;
use endara_relay::registry::AdapterRegistry;
use endara_relay::server::{
    build_router, start_server, AppState, MetaToolSchemas, SessionIdentityStore,
};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// Base64 of the synthetic 322-byte JPEG served by the fixture — the same
/// string hardcoded as `IMG_B64` in `tests/fixtures/image_mcp_server.sh`.
/// Starts with SOI `FF D8 FF` and ends with EOI `FF D9`.
const JPEG_BASE64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAADChEYHyYtNDtCSVBXXmVsc3qBiI+WnaSrsrnAx87V3OPq8fj/Bg0UGyIpMDc+RUxTWmFob3Z9hIuSmaCnrrW8w8rR2N/m7fT7AgkQFx4lLDM6QUhPVl1ka3J5gIeOlZyjqrG4v8bN1Nvi6fD3/gUMExohKC82PURLUllgZ251fIOKkZifpq20u8LJ0Nfe5ezz+gEIDxYdJCsyOUBHTlVcY2pxeH+GjZSboqmwt77FzNPa4ejv9v0ECxIZICcuNTxDSlFYX2ZtdHuCiZCXnqWss7rByM/W3eTr8vkABw4VHCMqMTg/Rk1UW2JpcHd+hYyTmqGor7a9xMvS2eDn7vX8AwoRGB8mLTQ7QklQV15lbHN6gYiPlp2kq7K5wMfO1dzj6vH4/wYNFBsiKTD/2Q==";

fn image_script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("image_mcp_server.sh")
}

/// Start a relay in JS execution mode backed by the image fixture, with
/// the given `relay.write_dirs` allowlist (canonical roots). An empty
/// vec models a relay with no write directories configured.
async fn setup_server(write_roots: Vec<PathBuf>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new();
    let config = StdioConfig {
        command: "bash".to_string(),
        args: vec![image_script_path().to_string_lossy().to_string()],
        env: HashMap::new(),
        server_type_override: None,
        endpoint_name: "img-test".into(),
        ..Default::default()
    };
    let mut adapter = StdioAdapter::new(config);
    adapter.initialize().await.expect("adapter init failed");
    registry
        .register(
            "img-ep".into(),
            Box::new(adapter),
            "stdio".into(),
            None,
            Some("img_ep".into()),
        )
        .await;

    let registry_arc = Arc::new(registry.clone());
    let shared_roots: SharedWriteRoots = Arc::new(std::sync::RwLock::new(write_roots));
    let state = AppState {
        registry: registry.clone(),
        js_execution_mode: Arc::new(AtomicBool::new(true)),
        meta_tool_handler: Arc::new(
            MetaToolHandler::new(registry_arc, Duration::from_secs(30))
                .with_write_roots(shared_roots),
        ),
        profile_registry: Arc::new(ProfileRegistry::new(registry.clone())),
        oauth_flow_manager: None,
        token_manager: None,
        oauth_adapter_inners: None,
        setup_manager: None,
        started_at: std::time::Instant::now(),
        toon_enabled: false,
        session_identities: Arc::new(std::sync::Mutex::new(SessionIdentityStore::default())),
        meta_tool_schemas: MetaToolSchemas::new(),
    };
    let router = build_router(state);
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let (bound_addr, handle) = start_server(router, addr)
        .await
        .expect("server start failed");
    (bound_addr, handle)
}

/// The sandbox script: call the fixture tool, find the image content
/// block, and write its base64 payload to `dest` — returning only the
/// written path and mimeType (never the payload).
fn round_trip_script(dest: &Path) -> String {
    let dest_js = serde_json::to_string(dest.to_str().unwrap()).unwrap();
    format!(
        r#"
        var r = tools["get_image"]({{}});
        var img = null;
        for (var i = 0; i < r.content.length; i++) {{
            if (r.content[i].type === "image") {{ img = r.content[i]; break; }}
        }}
        if (!img) throw new Error("no image content block in tool result");
        var p = writeFile({dest_js}, img.data, {{ encoding: "base64" }});
        return {{ path: p, mimeType: img.mimeType }};
    "#
    )
}

async fn call_execute_tools(addr: SocketAddr, script: &str) -> serde_json::Value {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "execute_tools",
                "arguments": { "script": script }
            },
            "id": 1
        }))
        .send()
        .await
        .expect("request failed");
    resp.json().await.expect("invalid JSON response")
}

#[tokio::test]
async fn test_image_tool_write_file_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    // Canonicalize so the configured root and the expected returned path
    // survive platform symlinks (macOS /var → /private/var).
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (addr, _handle) = setup_server(vec![root.clone()]).await;

    let dest = root.join("whatsapp").join("msg-123.jpg");
    let body = call_execute_tools(addr, &round_trip_script(&dest)).await;

    // The base64 payload must never appear anywhere in the response.
    let body_str = serde_json::to_string(&body).unwrap();
    assert!(
        !body_str.contains(JPEG_BASE64),
        "base64 payload leaked into the script's returned JSON: {}",
        body_str
    );

    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected text content, got: {}", body));
    let inner: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(inner["mimeType"], "image/jpeg", "got: {}", inner);

    // writeFile returns the canonical absolute destination under the root.
    let returned = PathBuf::from(inner["path"].as_str().expect("path string"));
    assert!(
        returned.is_absolute(),
        "returned path not absolute: {inner}"
    );
    assert!(
        returned.starts_with(&root),
        "returned path {} not under root {}",
        returned.display(),
        root.display()
    );
    assert_eq!(returned, dest);

    // On-disk bytes match the source payload exactly.
    use base64::Engine as _;
    let expected = base64::engine::general_purpose::STANDARD
        .decode(JPEG_BASE64)
        .unwrap();
    let written = std::fs::read(&dest).expect("written file missing");
    assert_eq!(
        written.len(),
        expected.len(),
        "on-disk size differs from source payload"
    );
    assert_eq!(&written[..3], &[0xFF, 0xD8, 0xFF], "missing JPEG SOI bytes");
    assert_eq!(
        written, expected,
        "on-disk bytes differ from source payload"
    );
}

#[tokio::test]
async fn test_write_file_errors_when_no_write_dirs_configured() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    // No write_dirs configured — writing is disabled.
    let (addr, _handle) = setup_server(Vec::new()).await;

    let dest = root.join("whatsapp").join("msg-123.jpg");
    let body = call_execute_tools(addr, &round_trip_script(&dest)).await;

    let error = body
        .get("error")
        .unwrap_or_else(|| panic!("expected JSON-RPC error, got: {}", body));
    assert_eq!(error["code"].as_i64().unwrap(), -32603, "got: {error}");
    let msg = error["message"].as_str().unwrap_or("");
    // The uncaught writeFile throw is captured by the sandbox's
    // `__sandbox_error` catch handler and surfaces as a JsError.
    assert!(
        msg.contains("JavaScript error") && msg.contains("Error: writeFile"),
        "expected __sandbox_error-surfaced writeFile error, got: {msg}"
    );
    assert!(
        msg.contains("[relay] write_dirs in ~/.endara/config.toml"),
        "error should name the config key, got: {msg}"
    );
    assert!(
        msg.contains("Settings → Write directories"),
        "error should name the desktop setting, got: {msg}"
    );
    assert!(!dest.exists(), "no file must be written when disabled");
}
