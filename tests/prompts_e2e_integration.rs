//! T11 — end-to-end multi-endpoint prompts integration test.
//!
//! Drives the real `/mcp` HTTP handler with two in-process mock upstreams and
//! exercises the full prompts round-trip: `prompts/list` -> `prompts/get` ->
//! `resources/read`. Proves the slot #8 namespacing + slot #9 URI wrapping
//! agree with the slot #6 `resources/read` decoder so a pointer the client
//! receives from `prompts/get` reverses back to the owning upstream without
//! cross-routing or payload corruption (DD1).

use async_trait::async_trait;
use endara_relay::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use endara_relay::js_sandbox::MetaToolHandler;
use endara_relay::profile_registry::ProfileRegistry;
use endara_relay::registry::AdapterRegistry;
use endara_relay::server::{
    build_router, start_server, AppState, MetaToolSchemas, SessionIdentityStore,
};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Recorded `get_prompt` calls: `(raw_name, arguments)` per invocation.
type PromptCallLog = Arc<Mutex<Vec<(String, Option<Value>)>>>;
/// Recorded `read_resource` URIs (already unwrapped by the registry).
type ReadCallLog = Arc<Mutex<Vec<String>>>;

/// In-process upstream that records every `get_prompt` / `read_resource` it
/// served so the test can assert the per-endpoint dispatch path without
/// reaching for a subprocess fixture. `prompt_name` is the upstream-raw name
/// (no relay prefix); `get_prompt` returns a message embedding a text block
/// (DD1 regression target), a `resource` block, and a `resource_link` block.
struct PromptMockAdapter {
    endpoint_label: String,
    prompt_name: String,
    prompt_get_calls: PromptCallLog,
    read_resource_calls: ReadCallLog,
}

#[async_trait]
impl McpAdapter for PromptMockAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        Ok(vec![])
    }
    async fn call_tool(&self, _name: &str, _args: Value) -> Result<Value, AdapterError> {
        Ok(json!({}))
    }
    async fn list_prompts(&self) -> Result<Vec<Value>, AdapterError> {
        Ok(vec![json!({
            "name": self.prompt_name,
            "description": format!("{} prompt", self.endpoint_label),
        })])
    }
    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<Value, AdapterError> {
        self.prompt_get_calls
            .lock()
            .unwrap()
            .push((name.to_string(), arguments.clone()));
        // Tag the response with the endpoint that served it so the round-trip
        // test can assert which adapter was reached, and echo the raw `name`
        // for the reverse-prefix assertion. The message intentionally embeds
        // the three relevant content shapes: a `text` block carrying a
        // URL-shaped string (DD1: must round-trip verbatim), a `resource`
        // block, and a `resource_link` block (slot #9: both URIs must be
        // wrapped to the owning endpoint).
        Ok(json!({
            "description": format!("{} response", self.endpoint_label),
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "see https://example.com/x for details" },
                        {
                            "type": "resource",
                            "resource": { "uri": "ui://app/inline", "mimeType": "text/html" }
                        },
                        { "type": "resource_link", "uri": "ui://app/link", "name": "Open" }
                    ]
                }
            ],
            "_endpoint_tag": self.endpoint_label,
            "_raw_name": name,
        }))
    }
    async fn read_resource(&self, uri: &str) -> Result<Value, AdapterError> {
        self.read_resource_calls
            .lock()
            .unwrap()
            .push(uri.to_string());
        Ok(json!({
            "_endpoint_tag": self.endpoint_label,
            "contents": [{
                "uri": uri,
                "mimeType": "text/plain",
                "text": format!("body from {}", self.endpoint_label),
            }]
        }))
    }
    fn health(&self) -> HealthStatus {
        HealthStatus::Healthy
    }
    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// Spin up the real `/mcp` axum router with two in-process upstreams so the
/// test drives the full server dispatch path (not just the registry helpers).
async fn setup_two_endpoint_server() -> (
    SocketAddr,
    PromptCallLog,
    ReadCallLog,
    PromptCallLog,
    ReadCallLog,
    tokio::task::JoinHandle<()>,
) {
    let alpha_prompt_calls = Arc::new(Mutex::new(Vec::new()));
    let alpha_read_calls = Arc::new(Mutex::new(Vec::new()));
    let beta_prompt_calls = Arc::new(Mutex::new(Vec::new()));
    let beta_read_calls = Arc::new(Mutex::new(Vec::new()));

    let registry = AdapterRegistry::new();
    registry
        .register(
            "alpha".into(),
            Box::new(PromptMockAdapter {
                endpoint_label: "alpha".into(),
                prompt_name: "alpha_prompt".into(),
                prompt_get_calls: alpha_prompt_calls.clone(),
                read_resource_calls: alpha_read_calls.clone(),
            }),
            "stdio".into(),
            None,
            Some("alpha".into()),
        )
        .await;
    registry
        .register(
            "beta".into(),
            Box::new(PromptMockAdapter {
                endpoint_label: "beta".into(),
                prompt_name: "beta_prompt".into(),
                prompt_get_calls: beta_prompt_calls.clone(),
                read_resource_calls: beta_read_calls.clone(),
            }),
            "stdio".into(),
            None,
            Some("beta".into()),
        )
        .await;

    let registry_arc = Arc::new(registry.clone());
    let state = AppState {
        registry: registry.clone(),
        js_execution_mode: Arc::new(AtomicBool::new(false)),
        meta_tool_handler: Arc::new(MetaToolHandler::new(registry_arc, Duration::from_secs(30))),
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
    let (bound, handle) = start_server(router, addr, tokio::sync::watch::channel(false).1)
        .await
        .expect("server start");

    (
        bound,
        alpha_prompt_calls,
        alpha_read_calls,
        beta_prompt_calls,
        beta_read_calls,
        handle,
    )
}

async fn post_mcp(client: &reqwest::Client, addr: SocketAddr, body: &Value) -> Value {
    let resp = client
        .post(format!("http://{}/mcp", addr))
        .json(body)
        .send()
        .await
        .expect("send /mcp");
    assert_eq!(resp.status(), 200, "/mcp returned non-200");
    resp.json::<Value>().await.expect("parse /mcp body")
}

/// One end-to-end test exercising `prompts/list` -> `prompts/get` ->
/// `resources/read` across two upstreams. Asserts (multi-endpoint mode,
/// `active_count == 2` so wrapping/prefixing is ON):
///
/// 1. `prompts/list` carries endpoint-namespaced names (`{prefix}__{name}`).
/// 2. `prompts/get` of each namespaced name reaches the correct upstream
///    with the raw (de-prefixed) name.
/// 3. Slot #9 resource refs in returned messages are wrapped to the OWNING
///    endpoint and reverse correctly via `resources/read` (no cross-routing).
/// 4. DD1: `text` blocks whose body is URL-shaped pass through verbatim.
#[tokio::test]
async fn prompts_round_trip_across_two_upstreams() {
    let (addr, alpha_prompt_calls, alpha_read_calls, beta_prompt_calls, beta_read_calls, _handle) =
        setup_two_endpoint_server().await;
    let client = reqwest::Client::new();

    // ---- (1) prompts/list returns prefixed names per endpoint ---------------
    let list = post_mcp(
        &client,
        addr,
        &json!({"jsonrpc":"2.0","method":"prompts/list","id":1}),
    )
    .await;
    let prompts = list["result"]["prompts"].as_array().expect("prompts array");
    let names: Vec<String> = prompts
        .iter()
        .map(|p| p["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "alpha__alpha_prompt"),
        "missing alpha__alpha_prompt in {:?}",
        names
    );
    assert!(
        names.iter().any(|n| n == "beta__beta_prompt"),
        "missing beta__beta_prompt in {:?}",
        names
    );

    // ---- (2) prompts/get dispatches to the right upstream with raw name ----
    let alpha_get = post_mcp(
        &client,
        addr,
        &json!({
            "jsonrpc":"2.0",
            "method":"prompts/get",
            "id":2,
            "params":{ "name":"alpha__alpha_prompt", "arguments": { "k":"v" } }
        }),
    )
    .await;
    let alpha_result = &alpha_get["result"];
    assert_eq!(alpha_result["_endpoint_tag"], "alpha");
    assert_eq!(alpha_result["_raw_name"], "alpha_prompt");
    {
        let calls = alpha_prompt_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "alpha get_prompt should fire exactly once");
        assert_eq!(calls[0].0, "alpha_prompt");
        assert_eq!(calls[0].1, Some(json!({ "k":"v" })));
        assert!(
            beta_prompt_calls.lock().unwrap().is_empty(),
            "beta must not receive alpha's prompts/get"
        );
    }

    // ---- (3) slot #9 URIs in returned messages are wrapped to `alpha` ------
    let alpha_resource_uri = alpha_result["messages"][0]["content"][1]["resource"]["uri"]
        .as_str()
        .expect("alpha resource uri");
    let alpha_link_uri = alpha_result["messages"][0]["content"][2]["uri"]
        .as_str()
        .expect("alpha resource_link uri");
    assert!(
        alpha_resource_uri.starts_with("mcp-relay://alpha/"),
        "alpha resource uri not wrapped to alpha: {}",
        alpha_resource_uri
    );
    assert!(
        alpha_link_uri.starts_with("mcp-relay://alpha/"),
        "alpha resource_link uri not wrapped to alpha: {}",
        alpha_link_uri
    );

    // ---- (4) DD1: text block with URL-shaped data is NOT rewritten ---------
    assert_eq!(
        alpha_result["messages"][0]["content"][0]["text"], "see https://example.com/x for details",
        "DD1 violation: `text` block must round-trip verbatim",
    );

    // Same round-trip for beta — proves multi-endpoint dispatch picks the
    // right adapter rather than just always-alpha.
    let beta_get = post_mcp(
        &client,
        addr,
        &json!({
            "jsonrpc":"2.0",
            "method":"prompts/get",
            "id":3,
            "params":{ "name":"beta__beta_prompt" }
        }),
    )
    .await;
    let beta_result = &beta_get["result"];
    assert_eq!(beta_result["_endpoint_tag"], "beta");
    assert_eq!(beta_result["_raw_name"], "beta_prompt");
    {
        let calls = beta_prompt_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "beta get_prompt should fire exactly once");
        assert_eq!(calls[0].0, "beta_prompt");
    }
    let beta_resource_uri = beta_result["messages"][0]["content"][1]["resource"]["uri"]
        .as_str()
        .expect("beta resource uri");
    let beta_link_uri = beta_result["messages"][0]["content"][2]["uri"]
        .as_str()
        .expect("beta resource_link uri");
    assert!(
        beta_resource_uri.starts_with("mcp-relay://beta/"),
        "beta resource uri not wrapped to beta: {}",
        beta_resource_uri
    );
    assert!(
        beta_link_uri.starts_with("mcp-relay://beta/"),
        "beta resource_link uri not wrapped to beta: {}",
        beta_link_uri
    );

    // ---- (5) resources/read reverses the wrap back to the OWNING upstream --
    let alpha_read = post_mcp(
        &client,
        addr,
        &json!({
            "jsonrpc":"2.0",
            "method":"resources/read",
            "id":4,
            "params":{ "uri": alpha_resource_uri }
        }),
    )
    .await;
    assert_eq!(
        alpha_read["result"]["_endpoint_tag"], "alpha",
        "wrapped alpha URI routed to the wrong upstream",
    );
    assert_eq!(
        alpha_read["result"]["contents"][0]["uri"], "ui://app/inline",
        "alpha upstream must see the unwrapped original URI",
    );
    {
        let reads = alpha_read_calls.lock().unwrap();
        assert_eq!(
            reads.as_slice(),
            &["ui://app/inline".to_string()],
            "alpha read_resource must receive the unwrapped URI exactly once",
        );
        assert!(
            beta_read_calls.lock().unwrap().is_empty(),
            "beta must not see alpha's resources/read",
        );
    }

    let beta_read = post_mcp(
        &client,
        addr,
        &json!({
            "jsonrpc":"2.0",
            "method":"resources/read",
            "id":5,
            "params":{ "uri": beta_link_uri }
        }),
    )
    .await;
    assert_eq!(
        beta_read["result"]["_endpoint_tag"], "beta",
        "wrapped beta URI routed to the wrong upstream",
    );
    assert_eq!(
        beta_read["result"]["contents"][0]["uri"], "ui://app/link",
        "beta upstream must see the unwrapped original URI",
    );
    {
        let reads = beta_read_calls.lock().unwrap();
        assert_eq!(
            reads.as_slice(),
            &["ui://app/link".to_string()],
            "beta read_resource must receive the unwrapped URI exactly once",
        );
        // alpha still only saw its own read above — no cross-routing.
        let alpha_reads = alpha_read_calls.lock().unwrap();
        assert_eq!(alpha_reads.len(), 1);
    }
}
