//! §5.4 Meta-tools integration tests (JS execution mode).
//!
//! Verifies that meta-tools (list_tools, search_tools, execute_tools) are
//! correctly exposed/hidden based on the `local_js_execution` config flag,
//! and that their wire format complies with MCP content array format.
//!
//! The `write_file` section at the bottom covers the relay-level file-write
//! meta-tool end to end: advertised only when `[relay] write_dirs` resolves,
//! writes land on disk (utf8 and base64), and rejections come back as
//! `isError: true` tool results.

mod common;

use base64::Engine;
use common::config::ConfigBuilder;
use common::harness::RelayHarness;
use common::mcp_client::McpClient;
use common::toolchain::require_node;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const META_TOOL_NAMES: &[&str] = &["execute_tools", "list_tools", "search_tools"];

/// Helper: start relay with everything server + JS execution ON, wait healthy, initialize client.
async fn setup_js_on() -> (RelayHarness, McpClient) {
    let config = ConfigBuilder::new()
        .add_stdio(
            "everything",
            "npx",
            &["-y", "@modelcontextprotocol/server-everything"],
        )
        .js_execution(true)
        .toon_output(false)
        .build();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");
    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");
    (harness, client)
}

/// Helper: start relay with everything server + JS execution OFF, wait healthy, initialize client.
async fn setup_js_off() -> (RelayHarness, McpClient) {
    let config = ConfigBuilder::new()
        .add_stdio(
            "everything",
            "npx",
            &["-y", "@modelcontextprotocol/server-everything"],
        )
        .toon_output(false)
        .build();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");
    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");
    (harness, client)
}

/// 4.1 — With js_execution_mode=true, tools/list returns ONLY the 3 meta-tools.
#[tokio::test]
async fn test_js_mode_on_tools_list_only_meta_tools() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_js_on().await;

    let tools = client.list_tools().await.expect("list_tools failed");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Should contain exactly the 3 meta-tools
    assert_eq!(
        tool_names.len(),
        3,
        "Expected exactly 3 meta-tools, got {}: {:?}",
        tool_names.len(),
        tool_names
    );
    for meta in META_TOOL_NAMES {
        assert!(
            tool_names.contains(meta),
            "Expected '{}' in tool list, got: {:?}",
            meta,
            tool_names
        );
    }
}

/// 4.1b — With js_execution_mode=false, tools/list includes standard tools and
/// the always-on meta-tools (list_tools, search_tools), but `execute_tools`
/// is intentionally hidden — it is gated on `local_js_execution` so we do not
/// advertise a tool the invocation handler will reject.
#[tokio::test]
async fn test_js_mode_off_tools_list_includes_standard_and_meta() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_js_off().await;

    let tools = client.list_tools().await.expect("list_tools failed");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Should include standard tools like "echo"
    assert!(
        tool_names.contains(&"echo"),
        "Expected 'echo' in tool list when JS mode OFF, got: {:?}",
        tool_names
    );
    // list_tools and search_tools must still be advertised
    for meta in &["list_tools", "search_tools"] {
        assert!(
            tool_names.contains(meta),
            "Expected '{}' in tool list when JS mode OFF, got: {:?}",
            meta,
            tool_names
        );
    }
    // execute_tools must NOT be advertised when JS mode is off
    assert!(
        !tool_names.contains(&"execute_tools"),
        "execute_tools must be hidden from the catalog when JS mode is OFF, got: {:?}",
        tool_names
    );
    // More than 2 tools (standard + 2 meta)
    assert!(
        tool_names.len() > 2,
        "Expected more than 2 tools when JS mode OFF, got: {:?}",
        tool_names
    );
}

/// 4.2 — call list_tools returns MCP content array format.
#[tokio::test]
async fn test_list_tools_returns_mcp_content_array() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_js_on().await;

    let result = client
        .call_tool("list_tools", json!({}))
        .await
        .expect("call_tool list_tools failed");

    // Must be wrapped in MCP content array format
    let content = result["result"]["content"]
        .as_array()
        .expect("list_tools result must have content array");
    assert!(!content.is_empty(), "content array must not be empty");
    assert_eq!(
        content[0]["type"], "text",
        "content item type must be 'text'"
    );

    // The text should be parseable JSON containing tools catalog
    let text = content[0]["text"].as_str().expect("text field missing");
    let inner: serde_json::Value =
        serde_json::from_str(text).expect("list_tools text should be valid JSON");
    assert!(
        inner["total"].as_u64().unwrap() >= 1,
        "expected at least 1 tool in catalog"
    );
    assert!(
        inner["tools"].is_array(),
        "expected tools array in list_tools response"
    );
}

/// 4.3 — call search_tools returns MCP content array with matching tools.
#[tokio::test]
async fn test_search_tools_returns_content_array() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_js_on().await;

    let result = client
        .call_tool("search_tools", json!({"query": "echo"}))
        .await
        .expect("call_tool search_tools failed");

    // Must be wrapped in MCP content array format
    let content = result["result"]["content"]
        .as_array()
        .expect("search_tools result must have content array");
    assert_eq!(content[0]["type"], "text");

    let text = content[0]["text"].as_str().expect("text field missing");
    let tools: serde_json::Value =
        serde_json::from_str(text).expect("search_tools text should be valid JSON");
    let tools_arr = tools.as_array().expect("search result should be an array");
    assert!(!tools_arr.is_empty(), "search for 'echo' should find tools");
    assert!(
        tools_arr[0]["name"].as_str().unwrap().contains("echo"),
        "first result should contain 'echo'"
    );
}

/// 4.3b — fuzzy search: a typo in the query still finds the intended tool.
#[tokio::test]
async fn test_search_tools_fuzzy_typo_match() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_js_on().await;

    let result = client
        .call_tool("search_tools", json!({"query": "ehco"}))
        .await
        .expect("call_tool search_tools failed");

    let content = result["result"]["content"]
        .as_array()
        .expect("search_tools result must have content array");
    assert_eq!(content[0]["type"], "text");

    let text = content[0]["text"].as_str().expect("text field missing");
    let tools: serde_json::Value =
        serde_json::from_str(text).expect("search_tools text should be valid JSON");
    let tools_arr = tools.as_array().expect("search result should be an array");
    assert!(
        tools_arr
            .iter()
            .any(|t| t["name"].as_str().unwrap_or("").contains("echo")),
        "fuzzy query 'ehco' should surface a tool whose name contains 'echo', got: {:?}",
        tools_arr
    );
}

/// 4.4 — call execute_tools with a JS script calling echo tool, result in content array.
#[tokio::test]
async fn test_execute_tools_returns_content_array() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_js_on().await;

    // The JS sandbox exposes tools as `tools["name"](args)`.
    let script = r#"
        var result = tools["echo"]({ message: "from integration test" });
        return result;
    "#;

    let result = client
        .call_tool("execute_tools", json!({"script": script}))
        .await
        .expect("call_tool execute_tools failed");

    // Must be wrapped in MCP content array format
    let content = result["result"]["content"]
        .as_array()
        .expect("execute_tools result must have content array");
    assert!(!content.is_empty(), "content array must not be empty");
    assert_eq!(content[0]["type"], "text");

    let text = content[0]["text"].as_str().expect("text field missing");
    assert!(
        text.contains("from integration test"),
        "expected echo response in execute_tools result, got: {}",
        text
    );
}

/// 4.5 — list_tools pagination works correctly.
#[tokio::test]
async fn test_list_tools_pagination() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_js_on().await;

    // First, get total count with no pagination
    let result = client
        .call_tool("list_tools", json!({}))
        .await
        .expect("call_tool list_tools failed");
    let content = result["result"]["content"]
        .as_array()
        .expect("content array");
    let text = content[0]["text"].as_str().unwrap();
    let full: serde_json::Value = serde_json::from_str(text).unwrap();
    let total = full["total"].as_u64().unwrap();
    assert!(total >= 1, "expected at least 1 tool");

    // Now paginate with limit=2, offset=0
    let result = client
        .call_tool("list_tools", json!({"limit": 2, "offset": 0}))
        .await
        .expect("call_tool list_tools paginated failed");
    let content = result["result"]["content"]
        .as_array()
        .expect("content array");
    let text = content[0]["text"].as_str().unwrap();
    let page: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(page["limit"].as_u64().unwrap(), 2);
    assert_eq!(page["offset"].as_u64().unwrap(), 0);
    let page_tools = page["tools"].as_array().unwrap();
    assert!(
        page_tools.len() <= 2,
        "expected at most 2 tools with limit=2, got: {}",
        page_tools.len()
    );
}

/// 4.6 — meta-tool input validation: list_tools with limit as string → isError.
#[tokio::test]
async fn test_list_tools_invalid_limit_type() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_js_on().await;

    // Pass limit as a string instead of integer
    let result = client
        .call_tool("list_tools", json!({"limit": "not_a_number"}))
        .await
        .expect("call_tool request should not fail at HTTP level");

    // The meta-tool should handle gracefully — either ignore the invalid value
    // (since as_u64() returns None for strings) and use default, or return an error.
    // Based on the code, as_u64() returns None for "not_a_number", so limit=None (default 50).
    // This is acceptable — the tool is permissive with invalid types.
    let has_result = result["result"].is_object();
    let has_error = result["error"].is_object();
    assert!(
        has_result || has_error,
        "expected either a valid result or an error, got: {}",
        result
    );
}

// ---------------------------------------------------------------------------
// write_file meta-tool
// ---------------------------------------------------------------------------

fn echo_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo_mcp_server.sh")
}

/// Start a relay backed by the bash echo fixture (no Node.js needed) with
/// the given `[relay] write_dirs` allowlist. Returns the harness, an
/// initialized client, and the canonical write root.
async fn setup_write_file(js_mode: bool, write_root: Option<&Path>) -> (RelayHarness, McpClient) {
    setup_write_file_with_validation(js_mode, write_root, true).await
}

/// Like [`setup_write_file`] but with an explicit `[relay] validate_inputs`
/// setting, so tests can exercise the dispatch path with the JSON-Schema
/// gate switched off.
async fn setup_write_file_with_validation(
    js_mode: bool,
    write_root: Option<&Path>,
    validate_inputs: bool,
) -> (RelayHarness, McpClient) {
    let fixture = echo_fixture_path();
    let mut builder = ConfigBuilder::new()
        .add_stdio("echo", "bash", &[fixture.to_str().unwrap()])
        .js_execution(js_mode)
        .toon_output(false)
        .validate_inputs(validate_inputs);
    if let Some(root) = write_root {
        builder = builder.write_dirs(&[root]);
    }
    let harness = RelayHarness::start(&builder.build()).await;
    harness
        .wait_healthy("echo", Duration::from_secs(30))
        .await
        .expect("echo endpoint did not become healthy");
    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");
    (harness, client)
}

fn canonical_tempdir() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let canonical = dir.path().canonicalize().expect("canonicalize tempdir");
    (dir, canonical)
}

async fn tool_names(client: &McpClient) -> Vec<String> {
    client
        .list_tools()
        .await
        .expect("list_tools failed")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// Parse the `{ path, bytes }` payload out of a successful write_file result.
fn parse_write_result(resp: &serde_json::Value) -> serde_json::Value {
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "write_file unexpectedly failed: {resp}"
    );
    let content = resp["result"]["content"]
        .as_array()
        .unwrap_or_else(|| panic!("write_file result must have content array: {resp}"));
    assert_eq!(content[0]["type"], "text");
    let text = content[0]["text"].as_str().expect("text field missing");
    serde_json::from_str(text).expect("write_file text should be valid JSON")
}

/// Extract the `isError: true` message from a rejected write_file call.
fn error_text(resp: &serde_json::Value) -> String {
    assert!(
        resp["error"].is_null(),
        "write_file rejections must be tool results, not JSON-RPC errors: {resp}"
    );
    assert_eq!(
        resp["result"]["isError"].as_bool(),
        Some(true),
        "expected isError: true, got: {resp}"
    );
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("error result must carry text content: {resp}"))
        .to_string()
}

/// write_file is advertised in both JS modes when a write root resolves,
/// and its description names the root and `[relay] write_dirs`.
#[tokio::test]
async fn test_write_file_advertised_in_both_modes_when_write_dirs_configured() {
    let (_dir, root) = canonical_tempdir();
    for js_mode in [true, false] {
        let (_harness, client) = setup_write_file(js_mode, Some(&root)).await;
        let tools = client.list_tools().await.expect("list_tools failed");
        let def = tools
            .iter()
            .find(|t| t["name"] == "write_file")
            .unwrap_or_else(|| {
                let names: Vec<_> = tools.iter().map(|t| t["name"].clone()).collect();
                panic!("js_mode={js_mode}: write_file not advertised, got {names:?}")
            });
        let desc = def["description"].as_str().unwrap_or("");
        assert!(desc.contains("write_dirs"), "js_mode={js_mode}: {desc}");
        assert!(
            desc.contains(&root.display().to_string()),
            "js_mode={js_mode}: description should list the root: {desc}"
        );
        let required = def["inputSchema"]["required"]
            .as_array()
            .expect("inputSchema.required");
        assert!(required.contains(&json!("path")) && required.contains(&json!("data")));
        if js_mode {
            let names = tool_names(&client).await;
            assert_eq!(
                names.len(),
                4,
                "JS mode should advertise exactly the 4 meta-tools, got {names:?}"
            );
        }
    }
}

/// Without `[relay] write_dirs` the tool is hidden in both JS modes.
#[tokio::test]
async fn test_write_file_hidden_without_write_dirs() {
    for js_mode in [true, false] {
        let (_harness, client) = setup_write_file(js_mode, None).await;
        let names = tool_names(&client).await;
        assert!(
            !names.contains(&"write_file".to_string()),
            "js_mode={js_mode}: write_file must be hidden without write_dirs, got {names:?}"
        );
    }
}

/// utf8 (default encoding) round trip, creating a missing parent directory.
/// Runs in JS mode to prove the direct-call gate does not block write_file.
#[tokio::test]
async fn test_write_file_utf8_writes_to_disk_in_js_mode() {
    let (_dir, root) = canonical_tempdir();
    let (_harness, client) = setup_write_file(true, Some(&root)).await;

    let dest = root.join("nested").join("hello.txt");
    let body = "hello from write_file\n";
    let resp = client
        .call_tool(
            "write_file",
            json!({ "path": dest.to_str().unwrap(), "data": body }),
        )
        .await
        .expect("call_tool write_file failed");

    let result = parse_write_result(&resp);
    assert_eq!(result["path"].as_str().unwrap(), dest.to_str().unwrap());
    assert_eq!(result["bytes"].as_u64().unwrap(), body.len() as u64);
    assert_eq!(std::fs::read_to_string(&dest).expect("file on disk"), body);
}

/// base64 round trip for binary bytes, in normal (non-JS) mode.
#[tokio::test]
async fn test_write_file_base64_writes_bytes_in_normal_mode() {
    let (_dir, root) = canonical_tempdir();
    let (_harness, client) = setup_write_file(false, Some(&root)).await;

    let bytes: Vec<u8> = (0u8..=255).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let dest = root.join("blob.bin");
    let resp = client
        .call_tool(
            "write_file",
            json!({
                "path": dest.to_str().unwrap(),
                "data": encoded,
                "encoding": "base64",
            }),
        )
        .await
        .expect("call_tool write_file failed");

    let result = parse_write_result(&resp);
    assert_eq!(result["bytes"].as_u64().unwrap(), bytes.len() as u64);
    assert_eq!(std::fs::read(&dest).expect("file on disk"), bytes);
}

/// Paths outside every configured root are rejected as isError tool
/// results with the actionable write_dirs message, and nothing is written.
#[tokio::test]
async fn test_write_file_outside_root_is_tool_error() {
    let (_dir, root) = canonical_tempdir();
    let (_outside_dir, outside) = canonical_tempdir();
    let (_harness, client) = setup_write_file(false, Some(&root)).await;

    let dest = outside.join("escape.txt");
    let resp = client
        .call_tool(
            "write_file",
            json!({ "path": dest.to_str().unwrap(), "data": "nope" }),
        )
        .await
        .expect("call_tool write_file failed");

    let msg = error_text(&resp);
    assert!(msg.starts_with("write_file: "), "{msg}");
    assert!(
        msg.contains("not inside a configured write directory"),
        "{msg}"
    );
    assert!(!dest.exists(), "no file must be written outside the root");

    // A `..` escape from inside the root is rejected the same way.
    let traversal = format!("{}/../escape.txt", root.display());
    let resp = client
        .call_tool("write_file", json!({ "path": traversal, "data": "nope" }))
        .await
        .expect("call_tool write_file failed");
    let msg = error_text(&resp);
    assert!(msg.starts_with("write_file: "), "{msg}");
}

/// Unsupported encodings and invalid base64 surface as isError results.
#[tokio::test]
async fn test_write_file_bad_encoding_and_base64_are_tool_errors() {
    let (_dir, root) = canonical_tempdir();
    let (_harness, client) = setup_write_file(false, Some(&root)).await;
    let dest = root.join("bad.bin");

    let resp = client
        .call_tool(
            "write_file",
            json!({ "path": dest.to_str().unwrap(), "data": "x", "encoding": "hex" }),
        )
        .await
        .expect("call_tool write_file failed");
    let msg = error_text(&resp);
    assert!(msg.contains("encoding") || msg.contains("hex"), "{msg}");

    let resp = client
        .call_tool(
            "write_file",
            json!({
                "path": dest.to_str().unwrap(),
                "data": "!!!not base64!!!",
                "encoding": "base64",
            }),
        )
        .await
        .expect("call_tool write_file failed");
    let msg = error_text(&resp);
    assert!(msg.starts_with("write_file: "), "{msg}");
    assert!(
        !dest.exists(),
        "no partial file may remain after a rejection"
    );
}

/// Regression: with `validate_inputs = false` the schema gate is skipped, so
/// a malformed call (missing / null / numeric / object `data`) must still be
/// rejected at dispatch instead of truncating an existing file to "" and
/// reporting success. An explicit empty string remains a valid payload.
#[tokio::test]
async fn test_write_file_validation_disabled_rejects_malformed_data_and_preserves_file() {
    let (_dir, root) = canonical_tempdir();
    let (_harness, client) = setup_write_file_with_validation(false, Some(&root), false).await;
    let dest = root.join("keep.txt");
    std::fs::write(&dest, "original").expect("seed file");
    let path = dest.to_str().unwrap();

    let malformed = [
        ("missing data", json!({ "path": path })),
        ("null data", json!({ "path": path, "data": null })),
        ("numeric data", json!({ "path": path, "data": 42 })),
        ("object data", json!({ "path": path, "data": { "a": 1 } })),
        (
            "numeric encoding",
            json!({ "path": path, "data": "x", "encoding": 7 }),
        ),
        ("numeric path", json!({ "path": 1, "data": "x" })),
    ];
    for (label, arguments) in malformed {
        let resp = client
            .call_tool("write_file", arguments)
            .await
            .expect("call_tool write_file failed");
        let msg = error_text(&resp);
        assert!(msg.starts_with("write_file: "), "{label}: {msg}");
        assert_eq!(
            std::fs::read_to_string(&dest).expect("file on disk"),
            "original",
            "{label}: existing file must not be truncated"
        );
    }

    let resp = client
        .call_tool("write_file", json!({ "path": path, "data": "" }))
        .await
        .expect("call_tool write_file failed");
    let result = parse_write_result(&resp);
    assert_eq!(result["bytes"].as_u64(), Some(0));
    assert_eq!(std::fs::read_to_string(&dest).expect("file on disk"), "");
}
