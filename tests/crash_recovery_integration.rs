//! Integration test: STDIO crash recovery.
//!
//! Starts a relay with crash-server fixture (--crash-after N),
//! makes successful tool calls, verifies crash detection via errors,
//! and checks that calls fail after the server exits.

use endara_relay::adapter::stdio::{StdioAdapter, StdioConfig};
use endara_relay::adapter::{HealthStatus, McpAdapter};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

fn crash_server_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-crash-server").to_string()
}

#[tokio::test]
async fn test_crash_server_successful_calls_then_failure() {
    // crash-after=5 means the crash server will process 5 JSON-RPC messages
    // before crashing. The initialize handshake uses 1 message, leaving room
    // for several successful tool calls before the crash.
    let config = StdioConfig {
        command: crash_server_bin(),
        args: vec!["--crash-after".to_string(), "5".to_string()],
        env: HashMap::new(),
    };

    let mut adapter = StdioAdapter::new(config);

    // Should start as Stopped
    assert_eq!(adapter.health(), HealthStatus::Stopped);

    // Initialize (uses 1 call on the crash counter)
    adapter.initialize().await.expect("initialize failed");
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    // Make tool calls until we hit the crash
    let mut success_count = 0;
    let mut got_error = false;
    for i in 0..10 {
        let result = adapter
            .call_tool("echo", json!({"message": format!("call {}", i)}))
            .await;
        match result {
            Ok(val) => {
                let content = val.get("content").expect("missing content");
                let text = content[0]["text"].as_str().unwrap();
                assert!(
                    text.contains(&format!("call {}", i)),
                    "expected echo, got: {}",
                    text
                );
                success_count += 1;
            }
            Err(_) => {
                // Server crashed — this is expected after crash-after calls
                got_error = true;
                break;
            }
        }
    }

    // We should have gotten at least 1 successful call before crash
    assert!(
        success_count >= 1,
        "expected at least 1 successful call before crash, got {}",
        success_count
    );

    // We should have hit an error (crash)
    assert!(got_error, "expected crash error but all calls succeeded");

    // After the crash, subsequent calls should also fail
    let result = adapter
        .call_tool("echo", json!({"message": "after crash"}))
        .await;
    assert!(result.is_err(), "expected error after crash");

    // Cleanup
    let _ = adapter.shutdown().await;
}

#[tokio::test]
async fn test_crash_server_immediate_crash() {
    // crash-after=2: initialize uses 1 call, so only 1 tool call
    // will succeed before the server crashes.
    let config = StdioConfig {
        command: crash_server_bin(),
        args: vec!["--crash-after".to_string(), "2".to_string()],
        env: HashMap::new(),
    };

    let mut adapter = StdioAdapter::new(config);
    adapter.initialize().await.expect("initialize failed");
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    // Try several tool calls — we expect at least one to fail
    let result = timeout(Duration::from_secs(5), async {
        let mut error_seen = false;
        for _ in 0..5 {
            let r = adapter
                .call_tool("echo", json!({"message": "trigger"}))
                .await;
            if r.is_err() {
                error_seen = true;
                break;
            }
        }
        error_seen
    })
    .await
    .expect("timed out waiting for crash detection");

    assert!(result, "expected crash to produce an error on tool call");

    let _ = adapter.shutdown().await;
}
