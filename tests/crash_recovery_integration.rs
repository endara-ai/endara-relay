//! Integration test: STDIO crash recovery.
//!
//! Starts a relay with crash-server fixture (--crash-after N),
//! makes successful tool calls, verifies crash detection via errors,
//! and exercises the auto-respawn supervisor: automatic recovery after a
//! crash, suppression after an intentional shutdown, and the
//! Unhealthy-but-retry-forever policy under rapid crash loops.

use endara_relay::adapter::stdio::{StdioAdapter, StdioConfig};
use endara_relay::adapter::{HealthStatus, McpAdapter};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

fn crash_server_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-crash-server").to_string()
}

/// Drive `echo` tool calls until one fails (the crash), returning the number
/// of successful calls before the error. Panics if no error is seen.
async fn call_until_crash(adapter: &StdioAdapter) -> u32 {
    let mut success_count = 0;
    for i in 0..10 {
        match adapter
            .call_tool("echo", json!({"message": format!("call {}", i)}))
            .await
        {
            Ok(_) => success_count += 1,
            Err(_) => return success_count,
        }
    }
    panic!("expected the crash server to crash, but all calls succeeded");
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
        server_type_override: None,
        endpoint_name: "crash-test-5".into(),
        ..Default::default()
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

    // NOTE: calls no longer fail forever after a crash — the auto-respawn
    // supervisor recovers the server after its backoff (covered by
    // test_crash_server_auto_respawn_recovers_tool_calls below).

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
        server_type_override: None,
        endpoint_name: "crash-test-2".into(),
        ..Default::default()
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

/// A crashed server recovers WITHOUT the management restart endpoint: the
/// auto-respawn supervisor backs off, respawns the child, re-runs the MCP
/// handshake, emits a tools-changed tick, and subsequent tool calls succeed.
#[tokio::test]
async fn test_crash_server_auto_respawn_recovers_tool_calls() {
    // crash-after=5 leaves room for the handshake plus a couple of tool
    // calls before each crash, so the respawned server can serve calls too.
    let config = StdioConfig {
        command: crash_server_bin(),
        args: vec!["--crash-after".to_string(), "5".to_string()],
        env: HashMap::new(),
        server_type_override: None,
        endpoint_name: "crash-auto-respawn".into(),
        ..Default::default()
    };

    let mut adapter = StdioAdapter::new(config);
    adapter.initialize().await.expect("initialize failed");
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    // Subscribe BEFORE the crash so the post-respawn handshake tick is
    // buffered even if the respawn wins the race with `recv()`.
    let mut tools_changed = adapter
        .subscribe_tools_changed()
        .expect("stdio adapter should expose tools-changed");

    let success_count = call_until_crash(&adapter).await;
    assert!(
        success_count >= 1,
        "expected at least 1 successful call before crash, got {}",
        success_count
    );

    // First crash → 1s backoff, then respawn + re-handshake + tick. The
    // generous timeout absorbs CI scheduling noise; no POST /restart is
    // involved anywhere in this test.
    let tick = timeout(Duration::from_secs(20), tools_changed.recv()).await;
    assert!(
        matches!(tick, Ok(Ok(()))),
        "expected post-respawn tools-changed tick, got {tick:?}"
    );
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    // The respawned server answers tool calls again.
    let result = adapter
        .call_tool("echo", json!({"message": "recovered"}))
        .await
        .expect("tool call should succeed after auto-respawn");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("recovered"), "expected echo, got: {}", text);

    let _ = adapter.shutdown().await;
}

/// An intentional `shutdown()` must NOT trigger a respawn: killing the child
/// during teardown fires the same stdout-EOF hook as a crash, but the
/// shutdown_requested flag makes the supervisor stand down.
#[tokio::test]
async fn test_crash_server_no_respawn_after_shutdown() {
    // crash-after=100: the server never crashes on its own; the only child
    // exit in this test is the intentional shutdown.
    let config = StdioConfig {
        command: crash_server_bin(),
        args: vec!["--crash-after".to_string(), "100".to_string()],
        env: HashMap::new(),
        server_type_override: None,
        endpoint_name: "crash-shutdown".into(),
        ..Default::default()
    };

    let mut adapter = StdioAdapter::new(config);
    adapter.initialize().await.expect("initialize failed");
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    adapter.shutdown().await.expect("shutdown failed");
    assert_eq!(adapter.health(), HealthStatus::Stopped);

    // Give an (erroneous) supervisor time to run its 1s first backoff and
    // complete a respawn — if one ran, health would flip back to Healthy
    // and the call below would succeed.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        adapter.health(),
        HealthStatus::Stopped,
        "health must stay Stopped after an intentional shutdown"
    );
    let result = adapter
        .call_tool("echo", json!({"message": "zombie"}))
        .await;
    assert!(
        result.is_err(),
        "no child may serve calls after an intentional shutdown"
    );
}

/// Retry policy: repeated rapid crashes mark the endpoint Unhealthy (3+
/// crashes within 60s), but the supervisor keeps retrying indefinitely and
/// recovers on its own once the underlying failure clears — confirmed
/// retry-forever-at-60s-cap policy, no manual restart involved.
#[tokio::test]
async fn test_crash_server_rapid_crashes_mark_unhealthy_then_recover() {
    // Wrapper script: while the marker file exists every (re)spawn dies
    // immediately, producing a rapid crash loop; removing the marker lets
    // the next respawn attempt succeed.
    let marker_dir = tempfile::tempdir().expect("tempdir");
    let marker = marker_dir.path().join("respawn-blocked");

    let mut env = HashMap::new();
    env.insert("CRASH_BIN".to_string(), crash_server_bin());
    env.insert(
        "CRASH_MARKER".to_string(),
        marker.to_string_lossy().to_string(),
    );

    let config = StdioConfig {
        command: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            r#"if [ -e "$CRASH_MARKER" ]; then exit 1; fi; exec "$CRASH_BIN" --crash-after 5"#
                .to_string(),
        ],
        env,
        server_type_override: None,
        endpoint_name: "crash-unhealthy-policy".into(),
        ..Default::default()
    };

    let mut adapter = StdioAdapter::new(config);
    adapter.initialize().await.expect("initialize failed");
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    // Shrink the backoff unit so the whole schedule (1-1-2-4-8-…-60 units)
    // plays out in milliseconds: even if a loaded CI runner delays the
    // Unhealthy poll below past the capped step, recovery still lands well
    // inside the recv() timeout instead of flaking on a real 60s wait.
    adapter
        .set_backoff_unit_for_test(Duration::from_millis(50))
        .await;

    let mut tools_changed = adapter
        .subscribe_tools_changed()
        .expect("stdio adapter should expose tools-changed");

    // Block respawns, then crash the running child via tool calls.
    std::fs::write(&marker, b"blocked").expect("write marker");
    call_until_crash(&adapter).await;

    // Crashes stack up on the early backoff steps: the 3rd crash within the
    // rolling window flips health to Unhealthy while retries continue.
    let unhealthy = timeout(Duration::from_secs(30), async {
        loop {
            if matches!(adapter.health(), HealthStatus::Unhealthy(_)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        unhealthy.is_ok(),
        "expected Unhealthy after 3 rapid crashes, still {:?}",
        adapter.health()
    );

    // Clear the failure. The supervisor is still retrying, so the endpoint
    // recovers with no manual restart: respawn + re-handshake +
    // tools-changed tick.
    std::fs::remove_file(&marker).expect("remove marker");
    let tick = timeout(Duration::from_secs(40), tools_changed.recv()).await;
    assert!(
        matches!(tick, Ok(Ok(()))),
        "expected tools-changed tick after recovery, got {tick:?}"
    );
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    let result = adapter
        .call_tool("echo", json!({"message": "back alive"}))
        .await
        .expect("tool call should succeed after crash-loop recovery");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("back alive"), "expected echo, got: {}", text);

    let _ = adapter.shutdown().await;
}

/// Retry-forever-at-the-cap policy, time-controlled: with an injected
/// backoff unit the supervisor walks the entire 1-1-2-4-8 schedule, reaches
/// the 60-unit cap, keeps retrying at that capped interval for multiple
/// attempts, and still recovers once the failure clears. An implementation
/// that gave up after a fixed number of attempts — or never reached the cap —
/// fails this test.
#[tokio::test]
async fn test_crash_server_retries_at_backoff_cap_then_recovers() {
    let marker_dir = tempfile::tempdir().expect("tempdir");
    let marker = marker_dir.path().join("respawn-blocked");
    let attempts = marker_dir.path().join("spawn-attempts");

    let mut env = HashMap::new();
    env.insert("CRASH_BIN".to_string(), crash_server_bin());
    env.insert(
        "CRASH_MARKER".to_string(),
        marker.to_string_lossy().to_string(),
    );
    env.insert(
        "CRASH_ATTEMPTS".to_string(),
        attempts.to_string_lossy().to_string(),
    );

    // Every spawn appends a line to the attempts file, so the test can count
    // how many respawns happened while blocked at the cap.
    let config = StdioConfig {
        command: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            r#"echo x >> "$CRASH_ATTEMPTS"; if [ -e "$CRASH_MARKER" ]; then exit 1; fi; exec "$CRASH_BIN" --crash-after 100"#
                .to_string(),
        ],
        env,
        server_type_override: None,
        endpoint_name: "crash-backoff-cap".into(),
        ..Default::default()
    };

    let count_attempts = |path: &std::path::Path| -> usize {
        std::fs::read_to_string(path)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };

    let mut adapter = StdioAdapter::new(config);
    adapter.initialize().await.expect("initialize failed");
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    // 25ms unit: schedule = 25,25,50,100,200ms then capped at 1500ms.
    adapter
        .set_backoff_unit_for_test(Duration::from_millis(25))
        .await;

    let mut tools_changed = adapter
        .subscribe_tools_changed()
        .expect("stdio adapter should expose tools-changed");

    // Block respawns, then simulate an unexpected crash of the healthy
    // child (crash-after=100 is never reached organically) so the supervisor
    // arms and every subsequent respawn dies on the marker.
    std::fs::write(&marker, b"blocked").expect("write marker");
    let spawned_before = count_attempts(&attempts);
    adapter.kill_child_for_test().await;

    // Walking 25+25+50+100+200ms puts the supervisor at the 1500ms cap in
    // well under a second of backoff time. Wait long enough to cover the
    // ramp plus at least 3 capped retries.
    let ramp_and_caps = Duration::from_millis(400 + 3 * 1500 + 1000);
    tokio::time::sleep(ramp_and_caps).await;

    let while_blocked = count_attempts(&attempts) - spawned_before;
    // Ramp = 5 attempts (after the 25,25,50,100,200ms steps), then one
    // attempt per 1500ms cap interval. Requiring >= 7 proves at least two
    // capped retries happened; requiring it while the marker still exists
    // proves it never stopped retrying.
    assert!(
        while_blocked >= 7,
        "expected the supervisor to keep retrying at the cap, saw only {} respawn attempts",
        while_blocked
    );
    assert!(
        matches!(adapter.health(), HealthStatus::Unhealthy(_)),
        "expected Unhealthy while crash-looping, got {:?}",
        adapter.health()
    );

    // Clear the failure: the next capped retry (≤1500ms away) recovers.
    std::fs::remove_file(&marker).expect("remove marker");
    let tick = timeout(Duration::from_secs(10), tools_changed.recv()).await;
    assert!(
        matches!(tick, Ok(Ok(()))),
        "expected tools-changed tick after cap recovery, got {tick:?}"
    );
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    let result = adapter
        .call_tool("echo", json!({"message": "capped but alive"}))
        .await
        .expect("tool call should succeed after capped-retry recovery");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("capped but alive"),
        "expected echo, got: {}",
        text
    );

    let _ = adapter.shutdown().await;
}
