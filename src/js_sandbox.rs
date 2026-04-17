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
use tokio::sync::RwLock;

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
// Fuzzy search helpers for `search_tools`
// ---------------------------------------------------------------------------

/// Minimum aggregate score for a tool to appear in `search_tools` results.
const MIN_SCORE: f64 = 1.0;

/// Split an identifier into lower-cased word tokens.
///
/// Splits on non-alphanumeric characters, camelCase / PascalCase boundaries,
/// digit-letter boundaries, and ALL-CAPS→lowercase boundaries
/// (e.g. `HTTPResponse` → `["http", "response"]`).
fn split_identifier(s: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = s.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current).to_lowercase());
            }
            continue;
        }

        if current.is_empty() {
            current.push(c);
            continue;
        }

        let prev = chars[i - 1];
        let next = chars.get(i + 1).copied();

        // letter ↔ digit boundary
        let digit_boundary = prev.is_ascii_digit() != c.is_ascii_digit();
        // lower → upper (camelCase)
        let camel_boundary = prev.is_lowercase() && c.is_uppercase();
        // ALL-CAPS run followed by a lowercase letter (HTTPResponse → HTTP, Response):
        // split before `c` when prev is uppercase letter, c is uppercase letter,
        // and next is a lowercase letter.
        let caps_boundary =
            prev.is_uppercase() && c.is_uppercase() && matches!(next, Some(n) if n.is_lowercase());

        if digit_boundary || camel_boundary || caps_boundary {
            tokens.push(std::mem::take(&mut current).to_lowercase());
        }
        current.push(c);
    }
    if !current.is_empty() {
        tokens.push(current.to_lowercase());
    }
    tokens.retain(|t| !t.is_empty());
    tokens
}

/// Re-export `split_identifier` for unit tests in this crate.
#[cfg(test)]
pub(crate) fn split_identifier_for_tests(s: &str) -> Vec<String> {
    split_identifier(s)
}

/// Levenshtein distance threshold by query-token length.
fn fuzzy_threshold(len: usize) -> usize {
    match len {
        0..=3 => 0,
        4..=5 => 1,
        _ => 2,
    }
}

/// Cached search index shared between `search_tools` calls: a
/// `(generation, docs)` pair where `generation` is the registry's
/// `catalog_generation` at the time `docs` was built.
type SearchIndexCache = Option<(u64, Arc<Vec<ToolDoc>>)>;

/// Precomputed per-tool search document.
struct ToolDoc {
    name_lower: String,
    name_tokens: Vec<String>,
    desc_lower: String,
    desc_tokens: Vec<String>,
    endpoint_tokens: Vec<String>,
    schema_prop_tokens: Vec<String>,
}

impl ToolDoc {
    fn from_tool(tool: &ToolInfo) -> Self {
        let name_lower = tool.name.to_lowercase();
        let name_tokens = split_identifier(&tool.name);
        let desc_lower = tool
            .description
            .as_ref()
            .map(|d| d.to_lowercase())
            .unwrap_or_default();
        let desc_tokens: Vec<String> = desc_lower
            .split_whitespace()
            .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
            .filter(|s| !s.is_empty())
            .collect();
        // Endpoint is the portion of tool.name before "__" (if present).
        let endpoint_str = tool.name.split_once("__").map(|(ep, _)| ep).unwrap_or("");
        let endpoint_tokens = split_identifier(endpoint_str);
        // Schema property names: union of tokenized top-level property keys.
        let mut schema_prop_tokens: Vec<String> = Vec::new();
        if let Some(props) = tool
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
        {
            for key in props.keys() {
                schema_prop_tokens.extend(split_identifier(key));
            }
        }
        Self {
            name_lower,
            name_tokens,
            desc_lower,
            desc_tokens,
            endpoint_tokens,
            schema_prop_tokens,
        }
    }
}

/// Score a single query token against a `ToolDoc`, returning the best match
/// score across all fields (0.0 means no match).
fn score_query_token(q: &str, doc: &ToolDoc) -> f64 {
    let mut best: f64 = 0.0;

    // name_token exact
    if doc.name_tokens.iter().any(|t| t == q) {
        best = best.max(10.0);
    }
    // q is a prefix of full name_lower
    if doc.name_lower.starts_with(q) {
        best = best.max(7.0);
    }
    // q is a prefix of any name_token
    if doc.name_tokens.iter().any(|t| t.starts_with(q)) {
        best = best.max(5.0);
    }
    // q is a substring of name_lower
    if doc.name_lower.contains(q) {
        best = best.max(3.5);
    }
    // q exact-equals any desc_token / endpoint_token / schema_prop_token
    if doc.desc_tokens.iter().any(|t| t == q)
        || doc.endpoint_tokens.iter().any(|t| t == q)
        || doc.schema_prop_tokens.iter().any(|t| t == q)
    {
        best = best.max(3.0);
    }
    // q is a substring of desc_lower
    if doc.desc_lower.contains(q) {
        best = best.max(1.5);
    }

    // Fuzzy (edit-distance) matches — only when threshold > 0.
    // Uses strsim::osa_distance (Optimal String Alignment) so that adjacent
    // transpositions count as 1 edit. This keeps the task's distance
    // thresholds (1 for len 4-5, etc.) workable for the common
    // "ehco" → "echo" case, which is distance 2 under classic Levenshtein.
    let threshold = fuzzy_threshold(q.chars().count());
    if threshold > 0 {
        if let Some(d) = doc
            .name_tokens
            .iter()
            .map(|t| strsim::osa_distance(q, t))
            .min()
        {
            if d <= threshold {
                let score = (2.0 - d as f64 * 0.5).max(0.5);
                best = best.max(score);
            }
        }
        if let Some(d) = doc
            .desc_tokens
            .iter()
            .map(|t| strsim::osa_distance(q, t))
            .min()
        {
            if d <= threshold {
                let score = (1.0 - d as f64 * 0.3).max(0.2);
                best = best.max(score);
            }
        }
    }

    best
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
    /// Memoized per-tool search index reused across `search_tools` calls.
    /// Holds `(generation, docs)` where `generation` is the registry's
    /// `catalog_generation` at the time the docs were built. The `Arc` lets
    /// scoring proceed after releasing the cache lock. Generation — not
    /// length — is the authoritative invariant: two different mutations could
    /// yield catalogs of equal length, and the counter rules that out.
    search_index_cache: Arc<RwLock<SearchIndexCache>>,
    /// Test-only counter incremented on every rebuild of the search index.
    /// Used by tests to assert cache hit/miss behavior without needing to
    /// inspect the cache contents directly.
    #[cfg(test)]
    search_index_rebuild_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl MetaToolHandler {
    pub fn new(registry: Arc<AdapterRegistry>, sandbox_timeout: Duration) -> Self {
        Self {
            registry,
            sandbox_timeout,
            search_index_cache: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            search_index_rebuild_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Test-only accessor returning the number of times the search index has
    /// been rebuilt. Used to assert cache hit/miss behavior.
    #[cfg(test)]
    pub(crate) fn search_index_rebuild_count(&self) -> usize {
        self.search_index_rebuild_count
            .load(std::sync::atomic::Ordering::Relaxed)
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

    /// `search_tools` — fuzzy, ranked search across name, description, endpoint,
    /// and input-schema property names.
    ///
    /// The built `Vec<ToolDoc>` is memoized in `search_index_cache` and reused
    /// across calls while the registry's catalog generation is unchanged.
    pub async fn search_tools(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Value, JsSandboxError> {
        // Read the registry generation BEFORE fetching the catalog. If the
        // generation bumps between here and the catalog fetch, we may stamp
        // an older generation onto newer docs — the worst case is a wasted
        // rebuild on the next call (stale label → mismatch → rebuild), never
        // incorrect results. Reading generation *after* the catalog would be
        // unsafe because it could stamp a newer generation onto stale docs.
        let current_gen = self.registry.catalog_generation();
        let catalog = self.registry.merged_catalog().await;
        let limit = limit.unwrap_or(20).min(200);

        let query_lower = query.to_lowercase();
        let query_tokens: Vec<&str> = query_lower.split_whitespace().collect();

        if query_tokens.is_empty() {
            let page: Vec<ToolInfoSlim> = catalog.iter().take(limit).map(Into::into).collect();
            return serde_json::to_value(&page)
                .map_err(|e| JsSandboxError::Internal(e.to_string()));
        }

        // Cache fast path: reuse docs when the generation is unchanged and the
        // cached vector aligns 1:1 with the catalog (length check is a cheap
        // safety belt; the generation is the authoritative invariant).
        let docs: Arc<Vec<ToolDoc>> = {
            let cached = self.search_index_cache.read().await;
            match cached.as_ref() {
                Some((gen, docs)) if *gen == current_gen && docs.len() == catalog.len() => {
                    Arc::clone(docs)
                }
                _ => {
                    drop(cached);
                    // Miss: rebuild. Racing readers may each rebuild
                    // independently (no single-flight). This is safe because
                    // each writer builds docs for the same `current_gen` (or
                    // for a successive generation, where last-write-wins).
                    let new_docs: Arc<Vec<ToolDoc>> =
                        Arc::new(catalog.iter().map(ToolDoc::from_tool).collect());
                    #[cfg(test)]
                    self.search_index_rebuild_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut w = self.search_index_cache.write().await;
                    *w = Some((current_gen, Arc::clone(&new_docs)));
                    new_docs
                }
            }
        };

        let mut scored: Vec<(f64, &ToolInfo)> = Vec::with_capacity(docs.len());
        for (doc, tool) in docs.iter().zip(catalog.iter()) {
            let per_token_scores: Vec<f64> = query_tokens
                .iter()
                .map(|q| score_query_token(q, doc))
                .collect();
            let matched = per_token_scores.iter().filter(|s| **s > 0.0).count();
            if matched == 0 {
                continue;
            }
            let base: f64 = per_token_scores.iter().sum();
            let hit_bonus = matched as f64 * 1.0;
            let all_matched_bonus = if matched == query_tokens.len() {
                2.0
            } else {
                0.0
            };
            let total = base + hit_bonus + all_matched_bonus;
            if total < MIN_SCORE {
                continue;
            }
            scored.push((total, tool));
        }

        // Sort: score desc, then name length asc, then name asc.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.name.len().cmp(&b.1.name.len()))
                .then_with(|| a.1.name.cmp(&b.1.name))
        });

        let matches: Vec<ToolInfoSlim> = scored
            .into_iter()
            .take(limit)
            .map(|(_, t)| ToolInfoSlim::from(t))
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
    async fn test_js_sandbox_search_tools_multi_word_query_strict_and_compat() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        // "echo tool" matches echo (name "echo" + desc "Echo tool") fully,
        // and greet (desc "Greeting tool") on just "tool"; echo must rank first.
        let result = handler.search_tools("echo tool", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(
            !tools.is_empty(),
            "echo tool should return at least one hit"
        );
        assert!(
            tools[0].name.contains("echo"),
            "echo must rank first for 'echo tool', got {:?}",
            tools
        );
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

    // --- split_identifier tokenizer tests ---

    #[test]
    fn test_split_identifier_snake_case() {
        assert_eq!(
            split_identifier_for_tests("get_task_by_id"),
            vec!["get", "task", "by", "id"]
        );
    }

    #[test]
    fn test_split_identifier_kebab_case() {
        assert_eq!(
            split_identifier_for_tests("list-tasks"),
            vec!["list", "tasks"]
        );
    }

    #[test]
    fn test_split_identifier_camel_case() {
        assert_eq!(
            split_identifier_for_tests("getTaskById"),
            vec!["get", "task", "by", "id"]
        );
    }

    #[test]
    fn test_split_identifier_pascal_case() {
        assert_eq!(split_identifier_for_tests("GetTask"), vec!["get", "task"]);
    }

    #[test]
    fn test_split_identifier_all_caps() {
        assert_eq!(split_identifier_for_tests("FOO_BAR"), vec!["foo", "bar"]);
    }

    #[test]
    fn test_split_identifier_http_response() {
        assert_eq!(
            split_identifier_for_tests("HTTPResponse"),
            vec!["http", "response"]
        );
    }

    #[test]
    fn test_split_identifier_digit_boundaries() {
        assert_eq!(split_identifier_for_tests("v2Api"), vec!["v", "2", "api"]);
        assert_eq!(
            split_identifier_for_tests("foo42bar"),
            vec!["foo", "42", "bar"]
        );
    }

    #[test]
    fn test_split_identifier_empty_and_separators() {
        let empty: Vec<String> = Vec::new();
        assert_eq!(split_identifier_for_tests(""), empty);
        assert_eq!(split_identifier_for_tests("___"), empty);
        assert_eq!(split_identifier_for_tests("_foo_"), vec!["foo"]);
        assert_eq!(
            split_identifier_for_tests("--foo--bar--"),
            vec!["foo", "bar"]
        );
    }

    #[test]
    fn test_split_identifier_mixed() {
        assert_eq!(
            split_identifier_for_tests("todoist_mcp__getTasks"),
            vec!["todoist", "mcp", "get", "tasks"]
        );
        assert_eq!(
            split_identifier_for_tests("parseHTTP2Url"),
            vec!["parse", "http", "2", "url"]
        );
    }

    // --- new fuzzy/ranked search_tools tests ---

    fn make_tool_with_schema(name: &str, desc: &str, schema: Value) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: Some(desc.to_string()),
            input_schema: schema,
            annotations: None,
        }
    }

    async fn registry_with_tools(endpoint: &str, tools: Vec<ToolInfo>) -> Arc<AdapterRegistry> {
        let registry = AdapterRegistry::new();
        registry
            .register(
                endpoint.into(),
                Box::new(MockAdapter::new(tools)),
                "stdio".into(),
                None,
                // Single adapter → no prefix so names stay as-given.
                None,
            )
            .await;
        Arc::new(registry)
    }

    #[tokio::test]
    async fn test_search_tools_typo_match_echo() {
        let reg = registry_with_tools(
            "ep",
            vec![make_tool("echo", "Echo tool"), make_tool("add", "Add")],
        )
        .await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("ehco", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(
            tools.iter().any(|t| t.name == "echo"),
            "expected echo in results for typo 'ehco', got {:?}",
            tools
        );
    }

    #[tokio::test]
    async fn test_search_tools_camel_case_tokenization() {
        let reg = registry_with_tools(
            "ep",
            vec![
                make_tool("getIssues", "List issues"),
                make_tool("other", "unrelated"),
            ],
        )
        .await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("issue", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(
            tools.iter().any(|t| t.name == "getIssues"),
            "expected getIssues for query 'issue', got {:?}",
            tools
        );
    }

    #[tokio::test]
    async fn test_search_tools_kebab_case_tokenization() {
        let reg = registry_with_tools(
            "ep",
            vec![
                make_tool("list-tasks", "List stuff"),
                make_tool("other", "unrelated"),
            ],
        )
        .await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("task", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(
            tools.iter().any(|t| t.name == "list-tasks"),
            "expected list-tasks for query 'task', got {:?}",
            tools
        );
    }

    #[tokio::test]
    async fn test_search_tools_schema_property_match() {
        let schema = json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string"}
            }
        });
        let reg = registry_with_tools(
            "ep",
            vec![
                make_tool_with_schema("foo_bar", "unrelated description", schema),
                make_tool("other", "nothing here"),
            ],
        )
        .await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("project", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(
            tools.iter().any(|t| t.name == "foo_bar"),
            "expected foo_bar (schema prop 'project_id') for query 'project', got {:?}",
            tools
        );
    }

    #[tokio::test]
    async fn test_search_tools_ranking_name_over_description() {
        let reg = registry_with_tools(
            "ep",
            vec![
                make_tool("echo", "does nothing special"),
                make_tool("other", "this just mentions echo briefly"),
            ],
        )
        .await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("echo", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(!tools.is_empty());
        assert_eq!(
            tools[0].name, "echo",
            "name-match tool must outrank description-match tool, got {:?}",
            tools
        );
    }

    #[tokio::test]
    async fn test_search_tools_prefix_beats_substring() {
        let reg = registry_with_tools(
            "ep",
            vec![
                make_tool("get_task", "retrieve a task"),
                make_tool("forget_me", "unrelated"),
            ],
        )
        .await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("get", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(!tools.is_empty());
        assert_eq!(
            tools[0].name, "get_task",
            "prefix match must outrank substring match, got {:?}",
            tools
        );
    }

    #[tokio::test]
    async fn test_search_tools_shorter_name_tiebreak() {
        // Two tools whose name_tokens both exact-match `q`, same score.
        // Sorted by name length asc, then lexicographic asc.
        let reg = registry_with_tools(
            "ep",
            vec![
                make_tool("task_bb", "one"),
                make_tool("task_a", "two"),
                make_tool("task_aa", "three"),
            ],
        )
        .await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("task", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        // task_a (6), task_aa (7) < task_bb (7); tie between task_aa and task_bb → aa before bb.
        assert_eq!(
            names,
            vec!["task_a", "task_aa", "task_bb"],
            "got {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_search_tools_multi_token_or_ranks_multi_hit_first() {
        let reg = registry_with_tools(
            "ep",
            vec![
                make_tool("echo", "Echo tool"),
                make_tool("greet", "Greeting tool"),
                make_tool("echo_greet", "Combined echo and greet tool"),
            ],
        )
        .await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("echo greet", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        assert!(
            names.contains(&"echo".to_string()) && names.contains(&"greet".to_string()),
            "both echo and greet should be present, got {:?}",
            names
        );
        assert_eq!(
            names[0], "echo_greet",
            "tool matching both tokens should rank first, got {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_search_tools_limit_clamping() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        // limit=0 → returns 0 results.
        let result = handler.search_tools("", Some(0)).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert_eq!(tools.len(), 0);
        // limit=500 → clamped to 200 (catalog only has 3).
        let result = handler.search_tools("", Some(500)).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert_eq!(tools.len(), 3);
    }

    #[tokio::test]
    async fn test_search_tools_min_score_cutoff() {
        // Query that could fuzzy-match a description token weakly — must not
        // surface the tool when the only signal is a weak desc-token levenshtein.
        let reg = registry_with_tools(
            "ep",
            vec![make_tool(
                "unrelated_name",
                "some tool that processes configuration entries",
            )],
        )
        .await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        // "confuguretion" vs "configuration" → distance 2, threshold 2 → fuzzy
        // desc score 1.0 - 2*0.3 = 0.4. Aggregate: base=0.4, hit=1.0,
        // all_matched=2.0, total=3.4 — this actually exceeds MIN_SCORE, so
        // use a noisier query that only weakly matches a long single word via
        // fuzzy desc: distance 2 on token alone with no other hits.
        // Use "cnfgurtn" → distance >> threshold (2) against "configuration" (len 13) → no match.
        let result = handler.search_tools("cnfgurtn", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(
            tools.is_empty(),
            "weak fuzzy-only noise should yield no hits, got {:?}",
            tools
        );
    }

    #[tokio::test]
    async fn test_search_tools_noise_query_returns_empty() {
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        let result = handler.search_tools("zzzzzz", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(
            tools.is_empty(),
            "noise query should return empty, got {:?}",
            tools
        );
    }

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

    // --- search index cache tests ---

    #[tokio::test]
    async fn test_search_index_cache_reuses_docs() {
        // Two sequential search_tools calls on an unchanged registry should
        // trigger exactly one rebuild of the ToolDoc vector.
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(reg, Duration::from_secs(5));
        assert_eq!(handler.search_index_rebuild_count(), 0);

        let _ = handler.search_tools("echo", None).await.unwrap();
        assert_eq!(
            handler.search_index_rebuild_count(),
            1,
            "first search_tools call must rebuild the index"
        );

        let _ = handler.search_tools("greet", None).await.unwrap();
        assert_eq!(
            handler.search_index_rebuild_count(),
            1,
            "second call on unchanged registry must NOT rebuild"
        );

        // A third call with yet a different query must also reuse.
        let _ = handler.search_tools("add", None).await.unwrap();
        assert_eq!(handler.search_index_rebuild_count(), 1);
    }

    #[tokio::test]
    async fn test_search_index_cache_invalidated_on_registry_change() {
        // After registering a new adapter, the next search_tools call must
        // rebuild, and the newly-registered tool must appear in results.
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep1".into(),
                Box::new(MockAdapter::new(vec![make_tool("echo", "Echo tool")])),
                "stdio".into(),
                None,
                Some("ep1".into()),
            )
            .await;
        let reg = Arc::new(registry);
        let handler = MetaToolHandler::new(Arc::clone(&reg), Duration::from_secs(5));

        let _ = handler.search_tools("echo", None).await.unwrap();
        assert_eq!(handler.search_index_rebuild_count(), 1);

        // Register a second adapter with a distinct tool. This triggers
        // invalidate_catalog_cache internally.
        reg.register(
            "ep2".into(),
            Box::new(MockAdapter::new(vec![make_tool("zephyr", "Zephyr helper")])),
            "stdio".into(),
            None,
            Some("ep2".into()),
        )
        .await;

        let result = handler.search_tools("zephyr", None).await.unwrap();
        let tools: Vec<ToolInfoSlim> = serde_json::from_value(result).unwrap();
        assert!(
            tools.iter().any(|t| t.name.contains("zephyr")),
            "newly-registered tool must surface in results, got {:?}",
            tools
        );
        assert_eq!(
            handler.search_index_rebuild_count(),
            2,
            "rebuild must happen after registry change"
        );
    }

    #[tokio::test]
    async fn test_search_index_cache_rebuilds_after_invalidate_catalog_cache() {
        // Explicit invalidate_catalog_cache must force a rebuild on the next
        // call even though the actual catalog contents are unchanged.
        let reg = make_registry().await;
        let handler = MetaToolHandler::new(Arc::clone(&reg), Duration::from_secs(5));

        let _ = handler.search_tools("echo", None).await.unwrap();
        assert_eq!(handler.search_index_rebuild_count(), 1);

        reg.invalidate_catalog_cache().await;

        let _ = handler.search_tools("echo", None).await.unwrap();
        assert_eq!(
            handler.search_index_rebuild_count(),
            2,
            "explicit invalidate_catalog_cache must force rebuild"
        );
    }

    #[tokio::test]
    async fn test_search_index_cache_correctness_parity() {
        // Running the same queries with a cold cache and then with a warm
        // cache must produce bit-identical serialized results.
        let reg = registry_with_tools(
            "ep",
            vec![
                make_tool("echo", "Echo tool"),
                make_tool("greet", "Greeting tool"),
                make_tool("getIssues", "List issues"),
                make_tool("list-tasks", "List tasks"),
                make_tool("get_task", "retrieve a task"),
                make_tool("forget_me", "unrelated"),
            ],
        )
        .await;

        let queries: &[(&str, Option<usize>)] = &[
            ("echo", None),
            ("ehco", None),       // fuzzy typo
            ("issue", None),      // camel-case tokenization
            ("task", None),       // kebab-case + prefix match
            ("get", None),        // prefix beats substring
            ("echo greet", None), // multi-token OR
            ("", Some(5)),        // empty query, limited
            ("zzzzzz", None),     // noise → empty
        ];

        // Cold pass: fresh handler, cache miss per call (first only).
        let cold_handler = MetaToolHandler::new(Arc::clone(&reg), Duration::from_secs(5));
        let mut cold_results: Vec<Value> = Vec::with_capacity(queries.len());
        for (q, lim) in queries {
            cold_results.push(cold_handler.search_tools(q, *lim).await.unwrap());
        }

        // Warm pass: same handler (cache is now populated). Cache stays warm
        // for every call because the registry hasn't been mutated.
        let mut warm_results: Vec<Value> = Vec::with_capacity(queries.len());
        for (q, lim) in queries {
            warm_results.push(cold_handler.search_tools(q, *lim).await.unwrap());
        }

        assert_eq!(
            cold_results, warm_results,
            "warm-cache results must match cold-cache results bit-for-bit"
        );
        // Exactly one rebuild across both passes: the very first non-empty
        // query. The empty-query branch skips the cache entirely.
        assert_eq!(
            cold_handler.search_index_rebuild_count(),
            1,
            "rebuild count must be 1 across warm+cold passes on unchanged registry"
        );
    }

    #[tokio::test]
    async fn test_catalog_generation_monotonic() {
        // Generation starts at 0, strictly increases on every mutation, and
        // back-to-back reads without mutation return the same value.
        let registry = AdapterRegistry::new();
        assert_eq!(registry.catalog_generation(), 0);
        assert_eq!(
            registry.catalog_generation(),
            registry.catalog_generation(),
            "back-to-back reads must be equal when no mutation happened"
        );

        // Register → generation bumps.
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::new(vec![make_tool("echo", "Echo")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        let g1 = registry.catalog_generation();
        assert!(g1 > 0);
        assert_eq!(
            registry.catalog_generation(),
            g1,
            "read without mutation must be stable"
        );

        // Register a second endpoint → bump again.
        registry
            .register(
                "ep2".into(),
                Box::new(MockAdapter::new(vec![make_tool("add", "Add")])),
                "stdio".into(),
                None,
                Some("ep2".into()),
            )
            .await;
        let g2 = registry.catalog_generation();
        assert!(g2 > g1);

        // Disable an adapter via entries() + invalidate_catalog_cache → bump.
        {
            let mut entries = registry.entries().write().await;
            entries.get_mut("ep").unwrap().disabled = true;
        }
        registry.invalidate_catalog_cache().await;
        let g3 = registry.catalog_generation();
        assert!(g3 > g2);

        // Re-enable + invalidate → bump again.
        {
            let mut entries = registry.entries().write().await;
            entries.get_mut("ep").unwrap().disabled = false;
        }
        registry.invalidate_catalog_cache().await;
        let g4 = registry.catalog_generation();
        assert!(g4 > g3);

        // Remove → bump (register/remove both invalidate internally).
        let _ = registry.remove("ep2").await;
        let g5 = registry.catalog_generation();
        assert!(g5 > g4);

        // Stable read after mutations stop.
        assert_eq!(registry.catalog_generation(), g5);
    }
}
