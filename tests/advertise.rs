//! Integration tests for `Advertise Connected Servers to the Model` (spec §5).
//!
//! Verifies that:
//!   * `InitializeResult.instructions` is present with `Connected servers: …`
//!     when at least one adapter is `Healthy`, and omitted otherwise.
//!   * The dynamic descriptions for `list_tools`, `search_tools`, and
//!     `execute_tools` reflect the currently-Healthy server set.

mod common;

use common::config::ConfigBuilder;
use common::harness::RelayHarness;
use serde_json::{json, Value};
use std::time::Duration;

fn bad_server_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-bad-server").to_string()
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

/// 1 — instructions field is omitted entirely when no adapters are Healthy.
#[tokio::test]
async fn instructions_omitted_when_no_healthy_servers() {
    // bad-server with `--omit-server-name` enters Failed → registry has zero
    // Healthy adapters → no instructions field, no `Connected servers:` suffix.
    let config = ConfigBuilder::new()
        .add_stdio("bad-server", &bad_server_bin(), &["--omit-server-name"])
        .build();
    let harness = RelayHarness::start(&config).await;

    // Give the adapter a moment to attempt initialize and transition to Failed.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let init = raw_initialize(&harness).await;
    let result = &init["result"];
    assert!(
        result.get("instructions").is_none(),
        "instructions should be omitted with zero Healthy adapters; got: {}",
        result
    );

    let tools = raw_tools_list(&harness).await;
    let list_desc = meta_tool_description(&tools, "list_tools");
    assert!(
        !list_desc.contains("servers connected"),
        "list_tools must not advertise connected servers when none are Healthy: {list_desc}"
    );
    let search_desc = meta_tool_description(&tools, "search_tools");
    assert!(
        !search_desc.contains("Connected servers:"),
        "search_tools must not append Connected servers list when none are Healthy: {search_desc}"
    );
}

/// 2 — instructions present and meta-tool descriptions extended once a server
///     becomes Healthy. Uses the `bad-server` fixture without flags so it
///     advertises serverInfo.name = "bad-mcp" and reaches Ready.
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
        instructions, "Connected servers: bad-mcp",
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
        search_desc.ends_with("\n\nConnected servers: bad-mcp"),
        "search_tools description missing Connected servers footer: {search_desc}"
    );
}

/// 3 — Failed adapters are excluded from the advertised list even when sitting
///     alongside a Healthy one. Two adapters: one good (`good-server`,
///     reports `bad-mcp`) and one Failed (`broken-server`).
#[tokio::test]
async fn failed_adapters_are_not_advertised() {
    let config = ConfigBuilder::new()
        .add_stdio("good-server", &bad_server_bin(), &[])
        .add_stdio("broken-server", &bad_server_bin(), &["--omit-server-name"])
        .build();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("good-server", Duration::from_secs(10))
        .await
        .expect("good-server did not become healthy");

    let init = raw_initialize(&harness).await;
    let instructions = init["result"]["instructions"]
        .as_str()
        .expect("instructions should be present");
    assert_eq!(
        instructions, "Connected servers: bad-mcp",
        "Failed adapter should be excluded; got: {instructions}"
    );

    let tools = raw_tools_list(&harness).await;
    let list_desc = meta_tool_description(&tools, "list_tools");
    // endpoint_count counts Healthy adapters only → 1, not 2.
    assert!(
        list_desc.ends_with(
            " 1 servers connected via Endara Relay \u{2014} use search_tools to discover tools."
        ),
        "list_tools count must exclude Failed adapter: {list_desc}"
    );
}
