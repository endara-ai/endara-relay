//! Integration tests for `Advertise Connected Servers to the Model` (spec §5).
//!
//! Verifies that:
//!   * `InitializeResult.instructions` is present with `Connected server types: …`
//!     when at least one registered adapter has a rendered `server_type`,
//!     present as just the lead-in when adapters are registered but none have
//!     a rendered type, and omitted entirely when no adapters are registered.
//!   * The dynamic descriptions for `list_tools`, `search_tools`, and
//!     `execute_tools` reflect **all** registered adapters (not only the
//!     `Healthy` ones).

mod common;

use common::config::ConfigBuilder;
use common::harness::RelayHarness;
use serde_json::{json, Value};
use std::time::Duration;

fn bad_server_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-bad-server").to_string()
}

fn multi_tool_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-multi-tool-server").to_string()
}

/// Lead-in sentence required at the top of `instructions` per spec §3.2.
const INSTRUCTIONS_LEAD_IN: &str = "Endara Relay aggregates MCP servers behind a single endpoint.";

/// Poll `tools/list` until the `list_tools` description count suffix reads
/// `N servers connected …`, or the timeout elapses.
async fn wait_for_list_tools_count(harness: &RelayHarness, expected: usize, timeout: Duration) {
    let needle = format!(" {} servers connected via Endara Relay", expected);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let tools = raw_tools_list(harness).await;
        if let Some(t) = tools
            .iter()
            .find(|t| t["name"].as_str() == Some("list_tools"))
        {
            if let Some(d) = t["description"].as_str() {
                if d.contains(&needle) {
                    return;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for list_tools count to reach {} (needle: {:?})",
                expected, needle
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Run a JSON-RPC `initialize` against the relay using a fresh client. We do
/// **not** use `McpClient::initialize()` because that helper hard-codes a
/// session and consumes the response — here we want the raw result.
async fn raw_initialize(harness: &RelayHarness) -> Value {
    harness
        .rpc(json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "advertise-test", "version": "0.1"}
            },
            "id": 1
        }))
        .await
}

async fn raw_tools_list(harness: &RelayHarness) -> Vec<Value> {
    let resp = harness
        .rpc(json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "id": 2
        }))
        .await;
    resp["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn meta_tool_description<'a>(tools: &'a [Value], name: &str) -> &'a str {
    tools
        .iter()
        .find(|t| t["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("meta-tool '{}' not found in tools/list", name))["description"]
        .as_str()
        .unwrap_or_else(|| panic!("'{}' description was not a string", name))
}

/// 1a — instructions field is omitted entirely when **no adapters at all**
///       are registered in the relay config.
#[tokio::test]
async fn instructions_omitted_when_no_endpoints_configured() {
    // No endpoints in config → registry is empty → no instructions field, no
    // `Connected server types:` suffix, and no count on list_tools.
    let config = ConfigBuilder::new().build();
    let harness = RelayHarness::start(&config).await;

    let init = raw_initialize(&harness).await;
    let result = &init["result"];
    assert!(
        result.get("instructions").is_none(),
        "instructions should be omitted with zero registered adapters; got: {}",
        result
    );

    let tools = raw_tools_list(&harness).await;
    let list_desc = meta_tool_description(&tools, "list_tools");
    assert!(
        !list_desc.contains("servers connected"),
        "list_tools must not advertise connected servers when none are registered: {list_desc}"
    );
    let search_desc = meta_tool_description(&tools, "search_tools");
    assert!(
        !search_desc.contains("Connected server types:"),
        "search_tools must not append Connected server types list when none are registered: {search_desc}"
    );
}

/// 1b — instructions field carries only the lead-in (no `Connected server
///       types:` sub-line) when a registered adapter exists but neither
///       handshook successfully nor carries a `server_type_override`.
#[tokio::test]
async fn instructions_lead_in_only_when_registered_adapter_has_no_renderable_type() {
    // `bad-server --omit-server-name` enters Failed without capturing
    // `serverInfo.name`; without a `server_type_override` it renders nothing,
    // but the endpoint is still registered → lead-in present, list omitted.
    let config = ConfigBuilder::new()
        .add_stdio("bad-server", &bad_server_bin(), &["--omit-server-name"])
        .build();
    let harness = RelayHarness::start(&config).await;

    // Give the adapter a moment to attempt initialize and transition to Failed.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let init = raw_initialize(&harness).await;
    let result = &init["result"];
    let instructions = result["instructions"]
        .as_str()
        .expect("instructions should be present once an adapter is registered");
    assert_eq!(
        instructions, INSTRUCTIONS_LEAD_IN,
        "instructions should be lead-in only when no server_type renders: {instructions}"
    );

    let tools = raw_tools_list(&harness).await;
    let list_desc = meta_tool_description(&tools, "list_tools");
    assert!(
        list_desc.ends_with(
            " 1 servers connected via Endara Relay \u{2014} use search_tools to discover tools."
        ),
        "list_tools count must include the registered adapter regardless of health: {list_desc}"
    );
    let search_desc = meta_tool_description(&tools, "search_tools");
    assert!(
        !search_desc.contains("Connected server types:"),
        "search_tools must not append Connected server types list when nothing renders: {search_desc}"
    );
}

/// 2 — instructions present and meta-tool descriptions extended once a server
///     becomes Healthy. Uses the `bad-server` fixture without flags so it
///     advertises serverInfo.name = "bad-mcp"; the relay strips the `-mcp`
///     suffix and advertises `bad`.
#[tokio::test]
async fn instructions_and_descriptions_reflect_healthy_server() {
    let config = ConfigBuilder::new()
        .add_stdio("good-server", &bad_server_bin(), &[])
        .build();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("good-server", Duration::from_secs(10))
        .await
        .expect("good-server did not become healthy");

    let init = raw_initialize(&harness).await;
    let result = &init["result"];
    let instructions = result["instructions"]
        .as_str()
        .expect("instructions should be present once an adapter is Healthy");
    assert_eq!(
        instructions,
        format!("{}\n\nConnected server types: bad", INSTRUCTIONS_LEAD_IN),
        "unexpected instructions string: {instructions}"
    );

    let tools = raw_tools_list(&harness).await;
    let list_desc = meta_tool_description(&tools, "list_tools");
    assert!(
        list_desc.ends_with(
            " 1 servers connected via Endara Relay \u{2014} use search_tools to discover tools."
        ),
        "list_tools description missing connected-servers suffix: {list_desc}"
    );
    let search_desc = meta_tool_description(&tools, "search_tools");
    assert!(
        search_desc.ends_with("\n\nConnected server types: bad"),
        "search_tools description missing Connected server types footer: {search_desc}"
    );
}

/// 3 — Failed adapters with a configured `server_type_override` are now
///     **included** in the advertised list alongside Healthy ones, and the
///     endpoint count covers all registered adapters regardless of health.
///     Two adapters: one good (`good-server`, reports `bad-mcp` which is
///     stripped to `bad`) and one Failed with override (`broken-server` with
///     `server_type_override = "broken"`).
#[tokio::test]
async fn failed_adapter_with_override_included_in_advertised_list() {
    let config = ConfigBuilder::new()
        .add_stdio("good-server", &bad_server_bin(), &[])
        .add_stdio("broken-server", &bad_server_bin(), &["--omit-server-name"])
        .with_server_type_override("broken")
        .build();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("good-server", Duration::from_secs(10))
        .await
        .expect("good-server did not become healthy");
    // Give broken-server a moment to attempt initialize and transition to Failed.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let init = raw_initialize(&harness).await;
    let instructions = init["result"]["instructions"]
        .as_str()
        .expect("instructions should be present");
    assert_eq!(
        instructions,
        format!(
            "{}\n\nConnected server types: bad, broken",
            INSTRUCTIONS_LEAD_IN
        ),
        "Failed adapter with override should now be included; got: {instructions}"
    );

    let tools = raw_tools_list(&harness).await;
    let list_desc = meta_tool_description(&tools, "list_tools");
    // endpoint_count covers all registered adapters regardless of health → 2.
    assert!(
        list_desc.ends_with(
            " 2 servers connected via Endara Relay \u{2014} use search_tools to discover tools."
        ),
        "list_tools count must include the Failed adapter: {list_desc}"
    );
    let search_desc = meta_tool_description(&tools, "search_tools");
    assert!(
        search_desc.ends_with("\n\nConnected server types: bad, broken"),
        "search_tools description must include the Failed adapter's override: {search_desc}"
    );
}

/// 4 — §5.3 row 1: three Healthy endpoints with three distinct serverInfo.name
///     values render verbatim in `initialize.instructions`, `search_tools`, and
///     count to 3 in `list_tools` / `execute_tools` (JS-mode off → list_tools
///     count assertion only).
#[tokio::test]
async fn multi_distinct_types_rendered_in_alphabetical_order() {
    let config = ConfigBuilder::new()
        .add_stdio("ep-zebra", &multi_tool_bin(), &["--name", "zebra"])
        .add_stdio("ep-alpha", &multi_tool_bin(), &["--name", "alpha"])
        .add_stdio("ep-mango", &multi_tool_bin(), &["--name", "mango"])
        .build();
    let harness = RelayHarness::start(&config).await;
    for ep in ["ep-zebra", "ep-alpha", "ep-mango"] {
        harness
            .wait_healthy(ep, Duration::from_secs(10))
            .await
            .unwrap_or_else(|_| panic!("{ep} did not become healthy"));
    }

    let expected_list = "alpha, mango, zebra";

    let init = raw_initialize(&harness).await;
    let instructions = init["result"]["instructions"]
        .as_str()
        .expect("instructions should be present with 3 Healthy adapters");
    assert_eq!(
        instructions,
        format!(
            "{}\n\nConnected server types: {}",
            INSTRUCTIONS_LEAD_IN, expected_list
        ),
        "instructions string did not match spec §3.2 format: {instructions}"
    );

    let tools = raw_tools_list(&harness).await;
    let list_desc = meta_tool_description(&tools, "list_tools");
    assert!(
        list_desc.ends_with(
            " 3 servers connected via Endara Relay \u{2014} use search_tools to discover tools."
        ),
        "list_tools description missing count=3 suffix: {list_desc}"
    );
    let search_desc = meta_tool_description(&tools, "search_tools");
    assert!(
        search_desc.ends_with(&format!("\n\nConnected server types: {}", expected_list)),
        "search_tools description does not end with full alphabetised list: {search_desc}"
    );
    // execute_tools is hidden when JS mode is off; covered separately.
}

/// 5 — §5.3 row 4 (kill endpoint): hot-reload removes one of three endpoints
///     via config rewrite; the relay's file watcher applies the diff and the
///     advertised type list / count drop accordingly.
#[tokio::test]
async fn hot_reload_kill_endpoint_drops_from_advertised_list() {
    let initial = ConfigBuilder::new()
        .add_stdio("ep-alpha", &multi_tool_bin(), &["--name", "alpha"])
        .add_stdio("ep-mango", &multi_tool_bin(), &["--name", "mango"])
        .add_stdio("ep-zebra", &multi_tool_bin(), &["--name", "zebra"])
        .build();
    let harness = RelayHarness::start(&initial).await;
    for ep in ["ep-alpha", "ep-mango", "ep-zebra"] {
        harness
            .wait_healthy(ep, Duration::from_secs(10))
            .await
            .unwrap_or_else(|_| panic!("{ep} did not become healthy"));
    }
    wait_for_list_tools_count(&harness, 3, Duration::from_secs(5)).await;

    // Rewrite config without ep-mango.
    let updated = ConfigBuilder::new()
        .add_stdio("ep-alpha", &multi_tool_bin(), &["--name", "alpha"])
        .add_stdio("ep-zebra", &multi_tool_bin(), &["--name", "zebra"])
        .build();
    std::fs::write(&harness.config_path, updated).expect("failed to rewrite config");

    // Watcher debounce is 500ms; allow extra slack for fs notify + reload.
    wait_for_list_tools_count(&harness, 2, Duration::from_secs(15)).await;

    let tools = raw_tools_list(&harness).await;
    let search_desc = meta_tool_description(&tools, "search_tools");
    assert!(
        search_desc.ends_with("\n\nConnected server types: alpha, zebra"),
        "expected mango to be removed from search_tools description: {search_desc}"
    );
    let init = raw_initialize(&harness).await;
    let instructions = init["result"]["instructions"].as_str().unwrap_or("");
    assert!(
        !instructions.contains("mango"),
        "instructions should not mention removed endpoint type: {instructions}"
    );
}

/// 6 — §5.3 row 5 (add endpoint): hot-reload adds a new endpoint via config
///     rewrite; the new type appears in `tools/list` and the count goes up.
#[tokio::test]
async fn hot_reload_add_endpoint_appears_in_advertised_list() {
    let initial = ConfigBuilder::new()
        .add_stdio("ep-alpha", &multi_tool_bin(), &["--name", "alpha"])
        .add_stdio("ep-zebra", &multi_tool_bin(), &["--name", "zebra"])
        .build();
    let harness = RelayHarness::start(&initial).await;
    for ep in ["ep-alpha", "ep-zebra"] {
        harness
            .wait_healthy(ep, Duration::from_secs(10))
            .await
            .unwrap_or_else(|_| panic!("{ep} did not become healthy"));
    }
    wait_for_list_tools_count(&harness, 2, Duration::from_secs(5)).await;

    // Rewrite config with an extra endpoint reporting "mango".
    let updated = ConfigBuilder::new()
        .add_stdio("ep-alpha", &multi_tool_bin(), &["--name", "alpha"])
        .add_stdio("ep-zebra", &multi_tool_bin(), &["--name", "zebra"])
        .add_stdio("ep-mango", &multi_tool_bin(), &["--name", "mango"])
        .build();
    std::fs::write(&harness.config_path, updated).expect("failed to rewrite config");

    harness
        .wait_healthy("ep-mango", Duration::from_secs(15))
        .await
        .expect("ep-mango did not become healthy after hot-reload add");
    wait_for_list_tools_count(&harness, 3, Duration::from_secs(15)).await;

    let tools = raw_tools_list(&harness).await;
    let search_desc = meta_tool_description(&tools, "search_tools");
    assert!(
        search_desc.ends_with("\n\nConnected server types: alpha, mango, zebra"),
        "expected mango to appear in alphabetised list: {search_desc}"
    );
}

/// 7 — §5.3 row 6 (second instance of existing type): two endpoints reporting
///     the same `serverInfo.name`; the type list deduplicates but the count
///     reflects both Healthy endpoints.
#[tokio::test]
async fn duplicate_type_dedupes_but_count_includes_both() {
    let config = ConfigBuilder::new()
        .add_stdio("ep-alpha-1", &multi_tool_bin(), &["--name", "alpha"])
        .add_stdio("ep-alpha-2", &multi_tool_bin(), &["--name", "alpha"])
        .build();
    let harness = RelayHarness::start(&config).await;
    for ep in ["ep-alpha-1", "ep-alpha-2"] {
        harness
            .wait_healthy(ep, Duration::from_secs(10))
            .await
            .unwrap_or_else(|_| panic!("{ep} did not become healthy"));
    }
    wait_for_list_tools_count(&harness, 2, Duration::from_secs(10)).await;

    let init = raw_initialize(&harness).await;
    let instructions = init["result"]["instructions"]
        .as_str()
        .expect("instructions should be present");
    assert_eq!(
        instructions,
        format!("{}\n\nConnected server types: alpha", INSTRUCTIONS_LEAD_IN),
        "duplicate type should dedupe to a single entry: {instructions}"
    );

    let tools = raw_tools_list(&harness).await;
    let list_desc = meta_tool_description(&tools, "list_tools");
    assert!(
        list_desc.ends_with(
            " 2 servers connected via Endara Relay \u{2014} use search_tools to discover tools."
        ),
        "list_tools count must reflect both Healthy endpoints: {list_desc}"
    );
    let search_desc = meta_tool_description(&tools, "search_tools");
    assert!(
        search_desc.ends_with("\n\nConnected server types: alpha"),
        "search_tools description should carry deduplicated single entry: {search_desc}"
    );
}

/// 8 — JS-execution-mode `execute_tools` carries the count suffix when at
///     least one adapter is Healthy. Verifies the meta-tool is exposed and
///     the description is built via `execute_tools_description`.
#[tokio::test]
async fn js_mode_execute_tools_carries_count_suffix() {
    let config = ConfigBuilder::new()
        .js_execution(true)
        .add_stdio("ep-alpha", &multi_tool_bin(), &["--name", "alpha"])
        .build();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("ep-alpha", Duration::from_secs(10))
        .await
        .expect("ep-alpha did not become healthy");
    wait_for_list_tools_count(&harness, 1, Duration::from_secs(10)).await;

    let tools = raw_tools_list(&harness).await;
    // execute_tools is only exposed in JS mode.
    let exec_desc = meta_tool_description(&tools, "execute_tools");
    assert!(
        exec_desc.ends_with(
            " 1 servers connected via Endara Relay \u{2014} use search_tools to discover tools."
        ),
        "execute_tools description missing count suffix in JS mode: ...{}",
        &exec_desc[exec_desc.len().saturating_sub(160)..]
    );
}
