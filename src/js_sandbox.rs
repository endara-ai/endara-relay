//! JavaScript execution sandbox using boa_engine.
//!
//! Provides a sandboxed JS runtime with access to MCP tools via a `tools` global object.
//! No filesystem or network access is available from within the sandbox.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use boa_engine::property::Attribute;
use boa_engine::{Context, JsError, JsNativeError, JsResult, JsValue, NativeFunction, Source};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::ToolInfo;
use crate::registry::AdapterRegistry;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors from sandbox execution.
#[derive(Debug, thiserror::Error)]
pub enum JsSandboxError {
    #[error("script execution timed out after {0}s")]
    Timeout(u64),
    #[error("JavaScript error: {0}")]
    JsError(String),
    #[error("internal error: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// Thread-local state for tool calls from within JS
// ---------------------------------------------------------------------------

struct SandboxState {
    registry: Arc<AdapterRegistry>,
    handle: tokio::runtime::Handle,
}

thread_local! {
    static SANDBOX_STATE: RefCell<Option<SandboxState>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// JsSandbox
// ---------------------------------------------------------------------------

/// A sandboxed JavaScript execution environment.
pub struct JsSandbox {
    registry: Arc<AdapterRegistry>,
    timeout: Duration,
}

impl JsSandbox {
    /// Create a new sandbox backed by the given registry.
    pub fn new(registry: Arc<AdapterRegistry>, timeout: Duration) -> Self {
        Self { registry, timeout }
    }

    /// Execute a JavaScript script in the sandbox.
    pub async fn execute(&self, script: &str) -> Result<Value, JsSandboxError> {
        let registry = self.registry.clone();
        let timeout = self.timeout;
        let script = script.to_string();
        let handle = tokio::runtime::Handle::current();
        let catalog = self.registry.merged_catalog().await;

        let result = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || {
                execute_in_sandbox(&script, &catalog, &registry, &handle)
            }),
        )
        .await;

        match result {
            Ok(Ok(inner)) => inner,
            Ok(Err(e)) => Err(JsSandboxError::Internal(format!("task join error: {}", e))),
            Err(_) => Err(JsSandboxError::Timeout(timeout.as_secs())),
        }
    }
}

// ---------------------------------------------------------------------------
// Core sandbox execution (runs on a blocking thread)
// ---------------------------------------------------------------------------

fn execute_in_sandbox(
    script: &str,
    catalog: &[ToolInfo],
    registry: &Arc<AdapterRegistry>,
    handle: &tokio::runtime::Handle,
) -> Result<Value, JsSandboxError> {
    SANDBOX_STATE.with(|cell| {
        *cell.borrow_mut() = Some(SandboxState {
            registry: registry.clone(),
            handle: handle.clone(),
        });
    });
    let result = run_js(script, catalog);
    SANDBOX_STATE.with(|cell| {
        *cell.borrow_mut() = None;
    });
    result
}

fn run_js(script: &str, catalog: &[ToolInfo]) -> Result<Value, JsSandboxError> {
    let mut context = Context::default();

    // Set loop iteration limit to prevent infinite loops from hanging.
    // 1 million iterations is generous for legitimate scripts but will
    // stop `while(true) {}` from running forever.
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(1_000_000);

    register_call_tool(&mut context)?;
    register_tools_object(&mut context, catalog)?;
    register_json_parse_wrapper(&mut context)?;

    let wrapped = format!(
        "var __sandbox_result;\n\
         var __sandbox_error;\n\
         (async function() {{\n\
         {script}\n\
         }})().then(function(r) {{ __sandbox_result = r; }}).catch(function(e) {{ __sandbox_error = String(e); }});\n"
    );

    context
        .eval(Source::from_bytes(wrapped.as_bytes()))
        .map_err(|e| JsSandboxError::JsError(e.to_string()))?;

    context.run_jobs();

    let error_val = context
        .global_object()
        .get(boa_engine::js_string!("__sandbox_error"), &mut context)
        .map_err(|e| JsSandboxError::Internal(e.to_string()))?;

    if !error_val.is_undefined() && !error_val.is_null() {
        let msg = error_val
            .to_string(&mut context)
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_else(|_| "unknown JS error".into());
        return Err(JsSandboxError::JsError(msg));
    }

    let result_val = context
        .global_object()
        .get(boa_engine::js_string!("__sandbox_result"), &mut context)
        .map_err(|e| JsSandboxError::Internal(e.to_string()))?;

    js_value_to_json(&result_val, &mut context)
}

// ---------------------------------------------------------------------------
// Native function: __call_tool(name, args_json) -> result_json_string
// ---------------------------------------------------------------------------

fn register_call_tool(context: &mut Context) -> Result<(), JsSandboxError> {
    let call_tool_fn = NativeFunction::from_fn_ptr(call_tool_native);
    let js_func = call_tool_fn.to_js_function(context.realm());
    context
        .register_global_property(
            boa_engine::js_string!("__call_tool"),
            js_func,
            Attribute::READONLY | Attribute::NON_ENUMERABLE,
        )
        .map_err(|e| JsSandboxError::Internal(format!("failed to register __call_tool: {}", e)))?;
    Ok(())
}

fn call_tool_native(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let tool_name = args
        .first()
        .ok_or_else(|| JsNativeError::typ().with_message("__call_tool: missing tool name"))?
        .to_string(context)?
        .to_std_string_escaped();

    let args_json_str = args
        .get(1)
        .ok_or_else(|| JsNativeError::typ().with_message("__call_tool: missing arguments"))?
        .to_string(context)?
        .to_std_string_escaped();

    let arguments: Value = serde_json::from_str(&args_json_str).map_err(|e| {
        JsNativeError::typ().with_message(format!("__call_tool: invalid JSON args: {}", e))
    })?;

    let result = SANDBOX_STATE.with(|cell| {
        let borrow = cell.borrow();
        let state = borrow
            .as_ref()
            .ok_or_else(|| JsNativeError::error().with_message("sandbox state not initialised"))?;
        let res = state
            .handle
            .block_on(state.registry.route_tool_call(&tool_name, arguments))
            .map_err(|e| {
                JsNativeError::error()
                    .with_message(format!("tool call '{}' failed: {}", tool_name, e))
            })?;
        Ok::<Value, JsError>(res)
    })?;

    let result_str = serde_json::to_string(&result)
        .map_err(|e| JsNativeError::error().with_message(format!("serialisation error: {}", e)))?;
    Ok(JsValue::from(boa_engine::js_string!(result_str.as_str())))
}

// ---------------------------------------------------------------------------
// Build the `tools` global object via JS eval
// ---------------------------------------------------------------------------

fn register_tools_object(
    context: &mut Context,
    catalog: &[ToolInfo],
) -> Result<(), JsSandboxError> {
    let mut js_src = String::from("var tools = {};\n");
    for tool in catalog {
        let name = &tool.name;
        js_src.push_str(&format!(
            "tools[\"{name}\"] = function(args) {{ return JSON.parse(__call_tool(\"{name}\", JSON.stringify(args || {{}}))); }};\n"
        ));
    }
    context
        .eval(Source::from_bytes(js_src.as_bytes()))
        .map_err(|e| JsSandboxError::Internal(format!("failed to create tools object: {}", e)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Replace `JSON.parse` with a wrapper that produces actionable errors.
//
// On failure, rethrows a `SyntaxError` whose message identifies the
// `JSON.parse` call, the kind and length of the input, and a short
// preview of the coerced input — while preserving the original serde_json
// error text. Behavior on success is unchanged.
// ---------------------------------------------------------------------------

fn register_json_parse_wrapper(context: &mut Context) -> Result<(), JsSandboxError> {
    const SRC: &str = r#"
(function() {
  var __origParse = JSON.parse;
  JSON.parse = function(text, reviver) {
    try {
      return __origParse(text, reviver);
    } catch (e) {
      var kind = (typeof text === 'string') ? 'string' : typeof text;
      var coerced;
      try { coerced = String(text); } catch (_) { coerced = '<uncoercible>'; }
      var len = coerced.length;
      var MAX = 80;
      var preview;
      if (len > MAX) {
        preview = JSON.stringify(coerced.slice(0, MAX)) + '\u2026';
      } else {
        preview = JSON.stringify(coerced);
      }
      var origMsg = (e && e.message) ? e.message : String(e);
      throw new SyntaxError(
        'JSON.parse failed: ' + origMsg +
        '; input (' + kind + ', len=' + len + '): ' + preview
      );
    }
  };
})();
"#;
    context
        .eval(Source::from_bytes(SRC.as_bytes()))
        .map_err(|e| {
            JsSandboxError::Internal(format!("failed to install JSON.parse wrapper: {}", e))
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// JS value → serde_json::Value conversion
// ---------------------------------------------------------------------------

fn js_value_to_json(val: &JsValue, context: &mut Context) -> Result<Value, JsSandboxError> {
    if val.is_undefined() || val.is_null() {
        return Ok(Value::Null);
    }
    if let Some(b) = val.as_boolean() {
        return Ok(Value::Bool(b));
    }
    if let Some(n) = val.as_number() {
        // If the float is a whole number that fits in i64, use integer representation
        // so that `json!(42)` == the result (serde_json distinguishes i64 vs f64).
        if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
            return Ok(Value::Number(serde_json::Number::from(n as i64)));
        }
        return Ok(serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null));
    }
    if val.is_string() {
        let s = val
            .to_string(context)
            .map_err(|e| JsSandboxError::Internal(e.to_string()))?
            .to_std_string_escaped();
        return Ok(Value::String(s));
    }
    // For objects/arrays use JSON.stringify on the JS side.
    let json_global = context
        .global_object()
        .get(boa_engine::js_string!("JSON"), context)
        .map_err(|e| JsSandboxError::Internal(e.to_string()))?;
    let stringify = json_global
        .as_object()
        .ok_or_else(|| JsSandboxError::Internal("JSON global not an object".into()))?
        .get(boa_engine::js_string!("stringify"), context)
        .map_err(|e| JsSandboxError::Internal(e.to_string()))?;
    let stringify_fn = stringify
        .as_object()
        .ok_or_else(|| JsSandboxError::Internal("JSON.stringify not a function".into()))?;
    let result = stringify_fn
        .call(&json_global, std::slice::from_ref(val), context)
        .map_err(|e| JsSandboxError::JsError(format!("JSON.stringify failed: {}", e)))?;
    let json_str = result
        .to_string(context)
        .map_err(|e| JsSandboxError::Internal(e.to_string()))?
        .to_std_string_escaped();
    serde_json::from_str(&json_str)
        .map_err(|e| JsSandboxError::Internal(format!("failed to parse JSON output: {}", e)))
}

// ---------------------------------------------------------------------------
// MetaToolHandler — list_tools, search_tools, execute_tools
// ---------------------------------------------------------------------------

/// Response for the `list_tools` meta-tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResponse {
    pub tools: Vec<ToolInfoSlim>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

/// Slim tool info returned in meta-tool responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfoSlim {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

impl From<&ToolInfo> for ToolInfoSlim {
    fn from(t: &ToolInfo) -> Self {
        Self {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
            annotations: t.annotations.clone(),
        }
    }
}

/// Handles the three meta-tools: list_tools, search_tools, execute_tools.
pub struct MetaToolHandler {
    registry: Arc<AdapterRegistry>,
    sandbox_timeout: Duration,
}

impl MetaToolHandler {
    pub fn new(registry: Arc<AdapterRegistry>, sandbox_timeout: Duration) -> Self {
        Self {
            registry,
            sandbox_timeout,
        }
    }

    /// `list_tools` — paginated catalog.
    pub async fn list_tools(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Value, JsSandboxError> {
        let catalog = self.registry.merged_catalog().await;
        let total = catalog.len();
        let limit = limit.unwrap_or(50).min(200);
        let offset = offset.unwrap_or(0).min(total);
        let page: Vec<ToolInfoSlim> = catalog
            .iter()
            .skip(offset)
            .take(limit)
            .map(Into::into)
            .collect();
        let resp = ListToolsResponse {
            tools: page,
            total,
            limit,
            offset,
        };
        serde_json::to_value(resp).map_err(|e| JsSandboxError::Internal(e.to_string()))
    }

    /// `search_tools` — token-based keyword search in name/description/endpoint.
    pub async fn search_tools(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Value, JsSandboxError> {
        let (catalog, lookup) = self.registry.merged_catalog_with_lookup().await;
        let query_lower = query.to_lowercase();
        let tokens: Vec<&str> = query_lower.split_whitespace().collect();
        let limit = limit.unwrap_or(20).min(200);

        if tokens.is_empty() {
            // Empty query returns first `limit` tools
            let page: Vec<ToolInfoSlim> = catalog.iter().take(limit).map(Into::into).collect();
            return serde_json::to_value(&page)
                .map_err(|e| JsSandboxError::Internal(e.to_string()));
        }

        let matches: Vec<ToolInfoSlim> = catalog
            .iter()
            .filter(|t| {
                let name_lower = t.name.to_lowercase();
                let desc_lower = t
                    .description
                    .as_ref()
                    .map(|d| d.to_lowercase())
                    .unwrap_or_default();
                // Also get endpoint name from lookup
                let endpoint_lower = lookup
                    .get(&t.name)
                    .map(|(ep, _)| ep.to_lowercase())
                    .unwrap_or_default();

                // ALL tokens must match somewhere
                tokens.iter().all(|token| {
                    name_lower.contains(token)
                        || desc_lower.contains(token)
                        || endpoint_lower.contains(token)
                        // Also check __ segments of the name
                        || name_lower.split("__").any(|seg| seg.contains(token))
                })
            })
            .take(limit)
            .map(Into::into)
            .collect();
        serde_json::to_value(&matches).map_err(|e| JsSandboxError::Internal(e.to_string()))
    }

    /// `execute_tools` — run JS in sandbox.
    pub async fn execute_tools(&self, script: &str) -> Result<Value, JsSandboxError> {
        let sandbox = JsSandbox::new(self.registry.clone(), self.sandbox_timeout);
        sandbox.execute(script).await
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter};
    use async_trait::async_trait;
    use serde_json::json;

    // --- Mock adapter ---

    struct MockAdapter {
        tools: Vec<ToolInfo>,
    }

    impl MockAdapter {
        fn new(tools: Vec<ToolInfo>) -> Self {
            Self { tools }
        }
    }

    #[async_trait]
    impl McpAdapter for MockAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
            Ok(json!({ "called": name, "args": arguments }))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    fn make_tool(name: &str, desc: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: Some(desc.to_string()),
            input_schema: json!({"type": "object"}),
            annotations: None,
        }
    }

    async fn make_registry() -> Arc<AdapterRegistry> {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::new(vec![
                    make_tool("echo", "Echo tool"),
                    make_tool("add", "Add numbers"),
                    make_tool("greet", "Greeting tool"),
                ])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        Arc::new(registry)
    }

    // --- list_tools tests ---

    #[tokio::test]
    async fn test_js_sandbox_list_tools_default_pagination() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.list_tools(None, None).await.unwrap();
        let resp: ListToolsResponse = serde_json::from_value(result).unwrap();
        assert_eq!(resp.total, 3);
        assert_eq!(resp.tools.len(), 3);
        assert_eq!(resp.offset, 0);
    }

    #[tokio::test]
    async fn test_js_sandbox_list_tools_with_limit() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.list_tools(Some(2), None).await.unwrap();
        let resp: ListToolsResponse = serde_json::from_value(result).unwrap();
        assert_eq!(resp.tools.len(), 2);
        assert_eq!(resp.total, 3);
        assert_eq!(resp.limit, 2);
    }

    #[tokio::test]
    async fn test_js_sandbox_list_tools_with_offset() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.list_tools(Some(10), Some(2)).await.unwrap();
        let resp: ListToolsResponse = serde_json::from_value(result).unwrap();
        assert_eq!(resp.tools.len(), 1);
        assert_eq!(resp.offset, 2);
    }

    // --- search_tools tests ---

    #[tokio::test]
    async fn test_js_sandbox_search_tools_by_name() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("echo", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].name.contains("echo"));
    }

    #[tokio::test]
    async fn test_js_sandbox_search_tools_by_description() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("Greeting", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].name.contains("greet"));
    }

    #[tokio::test]
    async fn test_js_sandbox_search_tools_case_insensitive() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("ECHO", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert_eq!(tools.len(), 1);
    }

    #[tokio::test]
    async fn test_js_sandbox_search_tools_by_endpoint_name() {
        // Multi-endpoint registry so tools get prefixed
        let registry = AdapterRegistry::new();
        registry
            .register(
                "todoist_mcp".into(),
                Box::new(MockAdapter::new(vec![
                    make_tool("get_tasks", "List tasks"),
                    make_tool("create_task", "Create a task"),
                ])),
                "stdio".into(),
                None,
                Some("todoist_mcp".into()),
            )
            .await;
        registry
            .register(
                "github_mcp".into(),
                Box::new(MockAdapter::new(vec![make_tool(
                    "list_issues",
                    "List issues",
                )])),
                "stdio".into(),
                None,
                Some("github_mcp".into()),
            )
            .await;
        let reg = Arc::new(registry);
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));

        // Search "todoist" should match tools from todoist_mcp endpoint
        let result = handler.search_tools("todoist", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().all(|t| t.name.contains("todoist_mcp")));
    }

    #[tokio::test]
    async fn test_js_sandbox_search_tools_multi_word_query() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        // "echo tool" should match the echo tool (name contains "echo", desc contains "tool")
        let result = handler.search_tools("echo tool", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].name.contains("echo"));

        // "echo numbers" should NOT match anything (no single tool has both)
        let result = handler.search_tools("echo numbers", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert_eq!(tools.len(), 0);
    }

    #[tokio::test]
    async fn test_js_sandbox_search_tools_empty_query() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        // Empty query returns all tools (first page)
        assert_eq!(tools.len(), 3);
    }

    // --- JS execution tests ---

    #[tokio::test]
    async fn test_js_sandbox_simple_return_value() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute("return 42;").await.unwrap();
        assert_eq!(result, json!(42));
    }

    #[tokio::test]
    async fn test_js_sandbox_return_object() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return {a: 1, b: "hello"};"#)
            .await
            .unwrap();
        assert_eq!(result["a"], 1);
        assert_eq!(result["b"], "hello");
    }

    #[tokio::test]
    async fn test_js_sandbox_return_string() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return "hello world";"#).await.unwrap();
        assert_eq!(result, json!("hello world"));
    }

    #[tokio::test]
    async fn test_js_sandbox_calls_mock_tool() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"const r = await tools.echo({text: "hi"}); return r;"#)
            .await
            .unwrap();
        assert_eq!(result["called"], "echo");
        assert_eq!(result["args"]["text"], "hi");
    }

    #[tokio::test]
    async fn test_js_sandbox_calls_tool_sync() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        // Without await — should also work since tool calls are synchronous
        let result = sandbox
            .execute(r#"const r = tools.echo({msg: "sync"}); return r;"#)
            .await
            .unwrap();
        assert_eq!(result["called"], "echo");
        assert_eq!(result["args"]["msg"], "sync");
    }

    #[tokio::test]
    async fn test_js_sandbox_timeout() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(10));
        let result = sandbox.execute("while(true) {}").await;
        assert!(
            result.is_err(),
            "infinite loop should be stopped by loop iteration limit"
        );
        // boa's RuntimeLimits throws a JsError when the loop iteration limit is exceeded
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Maximum loop iteration limit") || err_msg.contains("loop"),
            "error should mention loop limit: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_js_sandbox_no_filesystem_access() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        // require / import / Deno / process should not exist
        let result = sandbox.execute(r#"return typeof require;"#).await.unwrap();
        assert_eq!(result, json!("undefined"));
    }

    #[tokio::test]
    async fn test_js_sandbox_no_network_access() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return typeof fetch;"#).await.unwrap();
        assert_eq!(result, json!("undefined"));
    }

    #[tokio::test]
    async fn test_js_sandbox_js_error_propagates() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute("throw new Error('boom');").await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("boom"),
            "error should contain 'boom': {}",
            err_msg
        );
    }

    // --- Integration test: execute script that calls tools ---

    #[tokio::test]
    async fn test_js_sandbox_integration_multi_tool_calls() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(10));
        let script = r#"
            const r1 = await tools.echo({text: "first"});
            const r2 = await tools.add({a: 1, b: 2});
            return { echo_result: r1, add_result: r2 };
        "#;
        let result = sandbox.execute(script).await.unwrap();
        assert_eq!(result["echo_result"]["called"], "echo");
        assert_eq!(result["add_result"]["called"], "add");
        assert_eq!(result["echo_result"]["args"]["text"], "first");
    }

    // --- JSON.parse wrapper tests ---

    #[tokio::test]
    async fn test_json_parse_error_includes_preview_and_marker() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return JSON.parse("not json");"#).await;
        assert!(result.is_err(), "expected JSON.parse to fail");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("SyntaxError"),
            "missing SyntaxError: {}",
            err_msg
        );
        assert!(
            err_msg.contains("JSON.parse"),
            "missing JSON.parse marker: {}",
            err_msg
        );
        assert!(
            err_msg.contains("not json"),
            "missing input preview: {}",
            err_msg
        );
        // Original serde_json text preserved. Depending on the input,
        // serde_json may produce either "expected value" or "expected ident"
        // (e.g. "not json" starts with 'n' so it tries to parse `null`).
        // Either way the canonical "expected " and position info survive.
        assert!(
            err_msg.contains("expected ") && err_msg.contains("line 1"),
            "missing original serde_json text: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_json_parse_error_with_long_input_truncates_preview() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let script = r#"
            var s = "";
            for (var i = 0; i < 500; i++) s += "a";
            return JSON.parse(s);
        "#;
        let result = sandbox.execute(script).await;
        assert!(result.is_err(), "expected JSON.parse to fail");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains('\u{2026}') || err_msg.contains("..."),
            "missing truncation marker: {}",
            err_msg
        );
        assert!(
            !err_msg.contains(&"a".repeat(500)),
            "full 500-char input should not be present in the error message: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_json_parse_error_with_non_string_input() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return JSON.parse(undefined);"#).await;
        assert!(result.is_err(), "expected JSON.parse to fail");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("JSON.parse"),
            "missing JSON.parse marker: {}",
            err_msg
        );
        assert!(
            err_msg.contains("undefined"),
            "missing coerced form: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_json_parse_success_unchanged() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return JSON.parse('{"a":1}');"#)
            .await
            .unwrap();
        assert_eq!(result, json!({"a": 1}));
    }
}
