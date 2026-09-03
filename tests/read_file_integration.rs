//! Integration test: disk → readFile → remote client round-trip for binary
//! payloads.
//!
//! Places a synthetic JPEG inside a tempdir configured as the relay's
//! `relay.write_dirs` root, then runs an `execute_tools` script over HTTP
//! that returns `readFile(path, { encoding: "base64" })` — proving a remote
//! MCP client can retrieve the file byte-for-byte through the sandbox. A
//! companion negative case runs the same script against a relay with no
//! `relay.write_dirs` configured and asserts the `__sandbox_error`-surfaced
//! message names the config key and the desktop setting.

use endara_relay::js_sandbox::{MetaToolHandler, SharedWriteRoots};
use endara_relay::profile_registry::ProfileRegistry;
use endara_relay::registry::AdapterRegistry;
use endara_relay::server::{
    build_router, start_server, AppState, MetaToolSchemas, SessionIdentityStore,
};
use serde_json::json;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// Base64 of the synthetic 322-byte JPEG — the same string used by
/// `write_file_integration.rs`. Starts with SOI `FF D8 FF` and ends with
/// EOI `FF D9`.
const JPEG_BASE64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAADChEYHyYtNDtCSVBXXmVsc3qBiI+WnaSrsrnAx87V3OPq8fj/Bg0UGyIpMDc+RUxTWmFob3Z9hIuSmaCnrrW8w8rR2N/m7fT7AgkQFx4lLDM6QUhPVl1ka3J5gIeOlZyjqrG4v8bN1Nvi6fD3/gUMExohKC82PURLUllgZ251fIOKkZifpq20u8LJ0Nfe5ezz+gEIDxYdJCsyOUBHTlVcY2pxeH+GjZSboqmwt77FzNPa4ejv9v0ECxIZICcuNTxDSlFYX2ZtdHuCiZCXnqWss7rByM/W3eTr8vkABw4VHCMqMTg/Rk1UW2JpcHd+hYyTmqGor7a9xMvS2eDn7vX8AwoRGB8mLTQ7QklQV15lbHN6gYiPlp2kq7K5wMfO1dzj6vH4/wYNFBsiKTD/2Q==";

/// Start a relay in JS execution mode with the given `relay.write_dirs`
/// allowlist (canonical roots). An empty vec models a relay with no write
/// directories configured. No upstream MCP servers are needed — the script
/// under test only exercises the `readFile` sandbox global.
async fn setup_server(write_roots: Vec<PathBuf>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new();
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
    let (bound_addr, handle) = start_server(router, addr, tokio::sync::watch::channel(false).1)
        .await
        .expect("server start failed");
    (bound_addr, handle)
}

/// The sandbox script: return the file's contents as base64.
fn read_script(src: &Path) -> String {
    let src_js = serde_json::to_string(src.to_str().unwrap()).unwrap();
    format!(r#"return readFile({src_js}, {{ encoding: "base64" }});"#)
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
async fn test_read_file_base64_round_trip() {
    use base64::Engine as _;
    let dir = tempfile::tempdir().unwrap();
    // Canonicalize so the configured root survives platform symlinks
    // (macOS /var → /private/var).
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let source = base64::engine::general_purpose::STANDARD
        .decode(JPEG_BASE64)
        .unwrap();
    let src = root.join("whatsapp").join("msg-123.jpg");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, &source).unwrap();

    let (addr, _handle) = setup_server(vec![root.clone()]).await;
    let body = call_execute_tools(addr, &read_script(&src)).await;

    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected text content, got: {}", body));
    // The script returned a JS string, so the wrapped text is a JSON string.
    let inner: serde_json::Value = serde_json::from_str(text).unwrap();
    let payload = inner.as_str().expect("script result should be a string");

    // The response body decodes byte-for-byte to the source file.
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .expect("script result should be valid base64");
    assert_eq!(
        decoded.len(),
        source.len(),
        "decoded size differs from source file"
    );
    assert_eq!(&decoded[..3], &[0xFF, 0xD8, 0xFF], "missing JPEG SOI bytes");
    assert_eq!(decoded, source, "decoded bytes differ from source file");
}

#[tokio::test]
async fn test_read_file_errors_when_no_write_dirs_configured() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let src = root.join("whatsapp").join("msg-123.jpg");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, b"jpeg bytes").unwrap();
    // No write_dirs configured — filesystem access is disabled.
    let (addr, _handle) = setup_server(Vec::new()).await;

    let body = call_execute_tools(addr, &read_script(&src)).await;

    let error = body
        .get("error")
        .unwrap_or_else(|| panic!("expected JSON-RPC error, got: {}", body));
    assert_eq!(error["code"].as_i64().unwrap(), -32603, "got: {error}");
    let msg = error["message"].as_str().unwrap_or("");
    // The uncaught readFile throw is captured by the sandbox's
    // `__sandbox_error` catch handler and surfaces as a JsError.
    assert!(
        msg.contains("JavaScript error") && msg.contains("Error: readFile"),
        "expected __sandbox_error-surfaced readFile error, got: {msg}"
    );
    assert!(
        msg.contains("[relay] write_dirs in ~/.endara/config.toml"),
        "error should name the config key, got: {msg}"
    );
    assert!(
        msg.contains("Settings → Write directories"),
        "error should name the desktop setting, got: {msg}"
    );
    assert!(
        !msg.contains("jpeg bytes"),
        "file contents must never leak into the error, got: {msg}"
    );
}
