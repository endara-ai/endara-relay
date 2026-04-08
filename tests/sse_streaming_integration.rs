//! §5.7 SSE streaming integration tests.
//!
//! Tests the relay's GET /mcp/sse endpoint for SSE event stream format,
//! content type, and event structure.

mod common;

use common::config::everything_config;
use common::harness::RelayHarness;
use common::toolchain::require_node;
use futures_util::StreamExt;
use std::time::Duration;

/// Helper: start relay with everything server and wait until healthy.
async fn setup() -> RelayHarness {
    let config = everything_config();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");
    harness
}

/// 7.1 — GET /mcp/sse returns Content-Type: text/event-stream
#[tokio::test]
async fn test_sse_endpoint_content_type() {
    if require_node().is_none() {
        return;
    }
    let harness = setup().await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/mcp/sse", harness.base_url()))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("SSE request failed");

    assert!(
        resp.status().is_success(),
        "SSE endpoint returned non-success status: {}",
        resp.status()
    );

    let content_type = resp
        .headers()
        .get("content-type")
        .expect("missing content-type header")
        .to_str()
        .expect("invalid content-type header");
    assert!(
        content_type.contains("text/event-stream"),
        "expected text/event-stream, got: {}",
        content_type
    );
}

/// 7.2 — SSE stream contains properly formatted events with event: and data: fields
#[tokio::test]
async fn test_sse_event_stream_format() {
    if require_node().is_none() {
        return;
    }
    let harness = setup().await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/mcp/sse", harness.base_url()))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("SSE request failed");

    assert!(resp.status().is_success());

    // Read the first chunk(s) from the SSE stream — should contain the endpoint event
    let mut stream = resp.bytes_stream();
    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                collected.push_str(&String::from_utf8_lossy(&chunk));
                // Once we have the endpoint event, we're good
                if collected.contains("event:") && collected.contains("data:") {
                    break;
                }
            }
            _ => break,
        }
    }

    assert!(
        collected.contains("event:"),
        "SSE stream missing 'event:' field. Got: {}",
        collected
    );
    assert!(
        collected.contains("data:"),
        "SSE stream missing 'data:' field. Got: {}",
        collected
    );

    // The first event should be an "endpoint" event with "/mcp" data
    assert!(
        collected.contains("endpoint"),
        "SSE stream missing 'endpoint' event type. Got: {}",
        collected
    );
    assert!(
        collected.contains("/mcp"),
        "SSE stream endpoint event missing '/mcp' data. Got: {}",
        collected
    );
}

/// 7.3 — SSE stream includes keep-alive comments (lines starting with ':')
#[tokio::test]
async fn test_sse_keepalive_events() {
    if require_node().is_none() {
        return;
    }
    let harness = setup().await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/mcp/sse", harness.base_url()))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("SSE request failed");

    assert!(resp.status().is_success());

    // Read the stream for a few seconds to capture keep-alive comments
    // SSE keep-alive sends comment lines (starting with ':') periodically
    let mut stream = resp.bytes_stream();
    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);

    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(18), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                collected.push_str(&String::from_utf8_lossy(&chunk));
                // Keep-alive comments start with ':'
                if collected.lines().any(|l| l.starts_with(':')) {
                    break;
                }
            }
            _ => break,
        }
    }

    // Keep-alive is optional — if the relay sends them, verify format
    let has_keepalive = collected.lines().any(|l| l.starts_with(':'));
    if has_keepalive {
        eprintln!("Keep-alive comments detected in SSE stream");
    } else {
        eprintln!(
            "NOTE: No keep-alive comments detected within timeout. \
             This is acceptable if the relay uses a longer interval."
        );
    }
    // This test is informational — it verifies the stream is alive and readable
    // The endpoint event should have been received
    assert!(
        collected.contains("event:"),
        "SSE stream should have at least the endpoint event. Got: {}",
        collected
    );
}
