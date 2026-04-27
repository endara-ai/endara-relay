//! End-to-end integration test for the per-adapter tools cache.
//!
//! Exercises the user-visible behaviour wired up in T1–T5:
//! 1. Repeat reads of `/api/endpoints/<name>/tools` hit the cache.
//! 2. `/api/catalog` reuses the per-endpoint cache.
//! 3. Disable→enable invalidates the cache (lifecycle invalidation).
//! 4. Upstream `notifications/tools/list_changed` invalidates the cache and
//!    surfaces the new tool list (push-driven invalidation).

mod common;

use common::config::ConfigBuilder;
use common::harness::RelayHarness;
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

/// Read the fixture's `tools/list` counter file. Returns 0 when missing/empty.
fn read_count(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Poll `predicate` every 20ms until it returns true or `deadline` elapses.
async fn poll_until<F: FnMut() -> bool>(deadline: Duration, mut predicate: F) -> bool {
    let stop = Instant::now() + deadline;
    while Instant::now() < stop {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

/// GET `/api/endpoints/<name>/tools` and return the tool name set.
async fn fetch_tool_names(client: &reqwest::Client, url: &str) -> Vec<String> {
    let resp: Value = client.get(url).send().await.unwrap().json().await.unwrap();
    resp.as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
        .collect()
}

#[tokio::test]
async fn test_per_adapter_tools_cache_end_to_end() {
    // Counter file the fixture writes after every tools/list request.
    let count_file = NamedTempFile::new().expect("create count file");
    let count_path = count_file.path().to_path_buf();
    let count_path_str = count_path.to_str().unwrap().to_string();

    let fixture_bin = env!("CARGO_BIN_EXE_fixture-swap-notify-server");
    let config = ConfigBuilder::new()
        .add_stdio_with_env(
            "swapper",
            fixture_bin,
            &[],
            &[("FIXTURE_LIST_COUNT_FILE", count_path_str.as_str())],
        )
        .build();

    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("swapper", Duration::from_secs(15))
        .await
        .expect("swapper endpoint did not become healthy");

    let client = reqwest::Client::new();
    let base = harness.base_url();
    let tools_url = format!("{}/api/endpoints/swapper/tools", base);
    let catalog_url = format!("{}/api/catalog", base);

    // Step 1: First read populates per-endpoint cache → upstream count = 1.
    let names = fetch_tool_names(&client, &tools_url).await;
    assert!(
        names.iter().any(|n| n == "swap_tools") && names.iter().any(|n| n == "get_status"),
        "expected initial tools, got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "extra_tool_after_swap"),
        "extra tool should NOT be present before swap, got {names:?}"
    );
    assert!(
        poll_until(Duration::from_secs(2), || read_count(&count_path) >= 1).await,
        "fixture never recorded the first tools/list call"
    );
    assert_eq!(
        read_count(&count_path),
        1,
        "first GET should hit upstream exactly once"
    );

    // Step 2: Second read hits the per-endpoint cache → no upstream traffic.
    let _ = fetch_tool_names(&client, &tools_url).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        read_count(&count_path),
        1,
        "second GET should be a per-endpoint cache hit"
    );

    // Step 3: Catalog reuses the per-endpoint cache → still no upstream.
    let _: Value = client
        .get(&catalog_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        read_count(&count_path),
        1,
        "catalog should reuse the per-endpoint cache"
    );

    // Step 4: Disable → enable invalidates the cache; the next GET re-fetches.
    let resp = client
        .post(format!("{}/api/endpoints/swapper/disable", base))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "disable failed: {resp:?}");
    let resp = client
        .post(format!("{}/api/endpoints/swapper/enable", base))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "enable failed: {resp:?}");
    harness
        .wait_healthy("swapper", Duration::from_secs(15))
        .await
        .expect("swapper did not become healthy after re-enable");

    let _ = fetch_tool_names(&client, &tools_url).await;
    assert!(
        poll_until(Duration::from_secs(2), || read_count(&count_path) >= 2).await,
        "lifecycle invalidation: expected upstream count to reach 2, got {}",
        read_count(&count_path)
    );
    let count_after_lifecycle = read_count(&count_path);
    assert_eq!(
        count_after_lifecycle, 2,
        "lifecycle invalidation should produce exactly one upstream call"
    );

    // Step 5: Trigger upstream swap via tools/call → fixture emits
    // notifications/tools/list_changed → registry invalidates the cache →
    // next GET returns the swapped list and bumps the upstream counter by 1.
    let mut mcp = common::mcp_client::McpClient::new(base.clone());
    mcp.initialize().await.expect("initialize handshake failed");
    let call_resp = mcp
        .call_tool("swap_tools", json!({}))
        .await
        .expect("swap_tools call failed");
    assert!(
        call_resp.get("result").is_some(),
        "swap_tools returned no result: {call_resp:?}"
    );

    // Poll until the next GET returns the swapped tool list. Each pre-
    // invalidation read is a cache hit (no upstream traffic), so the upstream
    // counter only ticks once when the cache miss finally happens.
    let mut last_names: Vec<String> = Vec::new();
    let mut saw_new_tool = false;
    let stop = Instant::now() + Duration::from_secs(2);
    while Instant::now() < stop {
        let names = fetch_tool_names(&client, &tools_url).await;
        if names.iter().any(|n| n == "extra_tool_after_swap") {
            saw_new_tool = true;
            last_names = names;
            break;
        }
        last_names = names;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw_new_tool,
        "push-driven invalidation never surfaced the new tool list; last names: {last_names:?}"
    );

    let final_count = read_count(&count_path);
    assert_eq!(
        final_count,
        count_after_lifecycle + 1,
        "swap-driven invalidation should produce exactly one extra upstream tools/list \
         (before={count_after_lifecycle}, after={final_count})"
    );
}
