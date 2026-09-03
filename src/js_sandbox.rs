//! JavaScript execution sandbox using boa_engine.
//!
//! Provides a sandboxed JS runtime with access to MCP tools via a `tools` global object.
//! No network access is available from within the sandbox. Filesystem access is
//! limited to two allowlisted primitives: `writeFile(absPath, data, opts?)` and
//! `readFile(absPath, opts?)`. Both accept absolute paths only and validate the
//! fully-resolved path against the user-configured `relay.write_dirs` allowlist
//! (empty allowlist = filesystem access disabled).

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use boa_engine::property::Attribute;
use boa_engine::{Context, JsError, JsNativeError, JsResult, JsValue, NativeFunction, Source};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::Instrument;

use crate::adapter::{AdapterError, ToolInfo};
use crate::registry::MetaToolRegistry;

// Beneath-root open primitives for readFile/writeFile.
#[cfg(unix)]
mod open_beneath;

// ---------------------------------------------------------------------------
// Retry tuning (Wave 3)
// ---------------------------------------------------------------------------

/// Max number of retries beyond the first attempt (so total attempts at most
/// `1 + MAX_RETRIES`). The caller's `retry` value is clamped to this.
const MAX_RETRIES: usize = 3;

/// Base retry backoff schedule in ms — the self-documenting reference for
/// the production loop. Each per-attempt sleep is this base multiplied by a
/// ±25% jitter factor (see [`jittered_backoff_ms`]). Tests override the
/// effective schedule to zero via [`RETRY_BACKOFFS_MS`] to keep the suite
/// fast; `0 * jitter == 0`, so the call site formula is unchanged.
const RETRY_BACKOFF_SCHEDULE_MS: [u64; MAX_RETRIES] = [200, 400, 800];

/// Effective backoff schedule consulted by the retry loop. In production
/// builds this mirrors [`RETRY_BACKOFF_SCHEDULE_MS`]; in `cfg(test)` builds
/// it is zero so existing tests don't pay real backoff cost.
#[cfg(not(test))]
const RETRY_BACKOFFS_MS: [u64; MAX_RETRIES] = RETRY_BACKOFF_SCHEDULE_MS;
#[cfg(test)]
const RETRY_BACKOFFS_MS: [u64; MAX_RETRIES] = [0, 0, 0];

/// Substrings (lower-cased) that mark an `AdapterError` as transient and
/// therefore retryable. Matched against `AdapterError`'s `Display` text.
const RETRY_TRANSIENT_SUBSTRINGS: &[&str] =
    &["503", "502", "504", "timeout", "connection", "reset"];

/// Apply ±25% jitter to a backoff value: returns `(base as f64 * f) as u64`
/// where `f` is uniformly drawn from `[0.75, 1.25]`. A zero `base` always
/// returns zero, which preserves the test-suite zero-backoff override.
fn jittered_backoff_ms(base: u64, rng: &mut impl rand::Rng) -> u64 {
    use rand::RngExt;
    let factor: f64 = rng.random_range(0.75..=1.25);
    (base as f64 * factor) as u64
}

// ---------------------------------------------------------------------------
// Shared write-roots handle
// ---------------------------------------------------------------------------

/// Shared, hot-reloadable allowlist of canonical directory roots the sandbox
/// may write into (resolved from `relay.write_dirs` by
/// [`crate::config::resolve_write_roots`]). A single handle is created in
/// `main.rs` and shared by the global [`MetaToolHandler`], every per-profile
/// handler (via [`crate::profile_registry::ProfileRegistry`]), and the config
/// watcher — which swaps the contents on hot reload so all handlers observe
/// the new allowlist without being rebuilt. Uses `std::sync::RwLock` (not
/// tokio's): reads are brief snapshots taken without holding the guard across
/// an `.await`.
pub type SharedWriteRoots = Arc<std::sync::RwLock<Vec<PathBuf>>>;

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
    /// Catalog/routing source for the running script. Per locked decision
    /// Relay #2 this is `Arc<dyn MetaToolRegistry>` so the sandbox sees the
    /// same scoped view as its enclosing [`MetaToolHandler`] — global
    /// (`AdapterRegistry`) for `/mcp` or filtered
    /// (`ProfileRegistryView`) for `/mcp/{profile}`.
    registry: Arc<dyn MetaToolRegistry>,
    handle: tokio::runtime::Handle,
    /// Wall-clock deadline for the running script (`now + sandbox_timeout`,
    /// captured when `execute_in_sandbox` enters). The retry loop checks this
    /// before each jittered sleep so it can abort early — surfacing the last
    /// transient error — instead of being killed mid-sleep by the outer
    /// `tokio::time::timeout`.
    deadline: std::time::Instant,
    /// When `true`, the retry loop applies the production
    /// [`RETRY_BACKOFF_SCHEDULE_MS`] schedule even in `cfg(test)` builds.
    /// Defaults to `false`, which preserves the existing zero-backoff override
    /// that keeps the test suite fast.
    use_real_backoff: bool,
    /// JSON-serialised [`crate::events::ClientIdentity`] of the outer inbound
    /// request, captured before the sandbox hops onto the blocking thread.
    /// Empty when no caller identity is known. Used to re-establish a
    /// `request{client=...}` span around each inner `route_tool_call` so
    /// `SpanFieldCaptureLayer` / `current_request_context()` can surface the
    /// caller to the upstream adapter's event emitters and log lines — the
    /// blocking-thread hop otherwise drops the outer request span.
    client_json: String,
    /// Canonical per-inbound-request UID of the outer inbound request,
    /// captured before the sandbox hops onto the blocking thread. Empty when
    /// no UID was minted. Used to re-establish a `request{request_uid=...}`
    /// span around each inner `route_tool_call` so `SpanFieldCaptureLayer` /
    /// `current_request_context()` surface the UID to the upstream adapter's
    /// event emitters (`ToolCallEvent::Started.request_uid`) — the
    /// blocking-thread hop otherwise drops the outer request span.
    request_uid: String,
    /// Snapshot of the `relay.write_dirs` allowlist (canonical directory
    /// roots, resolved by [`crate::config::resolve_write_roots`]) taken when
    /// the script entered the sandbox. Empty means filesystem access is
    /// disabled. Read by the `writeFile` and `readFile` native functions to
    /// validate absolute paths.
    write_roots: Vec<PathBuf>,
    /// Per-run `writeFile` resource limits. Production runs always use
    /// [`WriteLimits::default`]; tests may shrink them via
    /// [`JsSandbox::with_write_limits`].
    write_limits: WriteLimits,
    /// Number of files successfully written by `writeFile` during this run.
    /// A fresh [`SandboxState`] is installed per run, so this resets per run.
    files_written: usize,
    /// Total bytes successfully written by `writeFile` during this run.
    /// A fresh [`SandboxState`] is installed per run, so this resets per run.
    bytes_written: usize,
    /// Per-run `readFile` resource limits. Production runs always use
    /// [`ReadLimits::default`]; tests may shrink them via
    /// [`JsSandbox::with_read_limits`].
    read_limits: ReadLimits,
    /// Number of files `readFile` has pulled from disk during this run,
    /// counted as soon as the filesystem read succeeds (whether or not the
    /// contents then decode). A fresh [`SandboxState`] is installed per run,
    /// so this resets per run.
    files_read: usize,
    /// Total bytes `readFile` has pulled from disk during this run, counted
    /// the same way as `files_read`. A fresh [`SandboxState`] is installed
    /// per run, so this resets per run.
    bytes_read: usize,
}

/// Per-run `writeFile` resource limits. These are per-script-run only —
/// there is no cross-run quota, no TTL sweep, and the relay never deletes
/// files: a breach throws before the offending file is written and leaves
/// every existing file untouched.
#[derive(Clone, Copy)]
struct WriteLimits {
    /// Maximum size of a single written file, checked before base64 decoding
    /// (payload size estimated as ≈3n/4 from the base64 string length).
    max_file_bytes: usize,
    /// Maximum number of files a single script run may write.
    max_files_per_run: usize,
    /// Maximum total bytes a single script run may write across all files.
    max_total_bytes_per_run: usize,
}

impl Default for WriteLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 32 * 1024 * 1024,
            max_files_per_run: 64,
            max_total_bytes_per_run: 256 * 1024 * 1024,
        }
    }
}

/// Per-run `readFile` resource limits. These are per-script-run only —
/// there is no cross-run quota. A breach throws before the offending file
/// is read.
#[derive(Clone, Copy)]
struct ReadLimits {
    /// Maximum size of a single read file, checked against the on-disk
    /// size before any bytes are read.
    max_file_bytes: usize,
    /// Maximum number of files a single script run may read.
    max_files_per_run: usize,
    /// Maximum total bytes a single script run may read across all files.
    max_total_bytes_per_run: usize,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 32 * 1024 * 1024,
            max_files_per_run: 64,
            max_total_bytes_per_run: 256 * 1024 * 1024,
        }
    }
}

thread_local! {
    static SANDBOX_STATE: RefCell<Option<SandboxState>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// JsSandbox
// ---------------------------------------------------------------------------

/// A sandboxed JavaScript execution environment.
pub struct JsSandbox {
    /// Catalog/routing source the sandbox sees. Held as
    /// `Arc<dyn MetaToolRegistry>` (locked decision Relay #2) so a profile
    /// view filters which tools `tools.call()` can reach without changing
    /// the sandbox's wiring.
    registry: Arc<dyn MetaToolRegistry>,
    timeout: Duration,
    /// Test-only override that opts a single sandbox into the real backoff
    /// schedule. Production callers leave this at `false` because production
    /// builds always apply the real schedule regardless.
    use_real_backoff: bool,
    /// JSON-serialised [`crate::events::ClientIdentity`] of the outer inbound
    /// request. Defaults to empty (no caller identity); set via
    /// [`Self::with_client`] by [`MetaToolHandler::execute_tools`] so inner
    /// upstream tool calls re-establish the caller's `request` span.
    client_json: String,
    /// Canonical per-inbound-request UID of the outer inbound request.
    /// Defaults to empty (no UID); set via [`Self::with_request_uid`] by
    /// [`MetaToolHandler::execute_tools`] so inner upstream tool calls
    /// re-establish the outer request's `request{request_uid=...}` span.
    request_uid: String,
    /// `relay.write_dirs` allowlist snapshot (canonical directory roots)
    /// carried into [`SandboxState`] for the running script. Defaults to
    /// empty (writing disabled); set via [`Self::with_write_roots`] by
    /// [`MetaToolHandler::execute_tools`].
    write_roots: Vec<PathBuf>,
    /// Per-run `writeFile` resource limits carried into [`SandboxState`].
    /// Defaults to [`WriteLimits::default`]; tests may shrink them via
    /// [`Self::with_write_limits`].
    write_limits: WriteLimits,
    /// Per-run `readFile` resource limits carried into [`SandboxState`].
    /// Defaults to [`ReadLimits::default`]; tests may shrink them via
    /// [`Self::with_read_limits`].
    read_limits: ReadLimits,
}

impl JsSandbox {
    /// Create a new sandbox backed by the given registry. Accepts any
    /// `Arc<R>` where `R: MetaToolRegistry + 'static`, so callers can pass
    /// either `Arc<AdapterRegistry>` (global) or
    /// `Arc<ProfileRegistryView>` (per-profile) without explicit
    /// `as Arc<dyn ...>` casting — the trait-object coercion fires at the
    /// `from_dyn` call site below.
    ///
    /// `allow(dead_code)`: production callers reach the sandbox through
    /// [`MetaToolHandler::execute_tools`], which uses [`Self::from_dyn`]
    /// directly. This generic convenience constructor is exercised by
    /// the unit tests in this module and by external lib consumers.
    #[allow(dead_code)]
    pub fn new<R>(registry: Arc<R>, timeout: Duration) -> Self
    where
        R: MetaToolRegistry + 'static,
    {
        Self::from_dyn(registry, timeout)
    }

    /// Construct directly from an already-typed `Arc<dyn MetaToolRegistry>`.
    /// Used by `MetaToolHandler::execute_tools`, which holds the trait
    /// object and forwards it into the sandbox without naming the concrete
    /// type.
    pub fn from_dyn(registry: Arc<dyn MetaToolRegistry>, timeout: Duration) -> Self {
        Self {
            registry,
            timeout,
            use_real_backoff: false,
            client_json: String::new(),
            request_uid: String::new(),
            write_roots: Vec::new(),
            write_limits: WriteLimits::default(),
            read_limits: ReadLimits::default(),
        }
    }

    /// Attach the outer inbound request's JSON-serialised
    /// [`crate::events::ClientIdentity`] so inner upstream tool calls
    /// re-establish a `request{client=...}` span on the sandbox's blocking
    /// thread. An empty string degrades to no caller identity. Used by
    /// [`MetaToolHandler::execute_tools`].
    pub fn with_client(mut self, client_json: String) -> Self {
        self.client_json = client_json;
        self
    }

    /// Attach the outer inbound request's canonical `request_uid` so inner
    /// upstream tool calls re-establish a `request{request_uid=...}` span on
    /// the sandbox's blocking thread. An empty string degrades to no UID.
    /// Used by [`MetaToolHandler::execute_tools`].
    pub fn with_request_uid(mut self, request_uid: String) -> Self {
        self.request_uid = request_uid;
        self
    }

    /// Attach the `relay.write_dirs` allowlist snapshot (canonical directory
    /// roots resolved by [`crate::config::resolve_write_roots`]) so the
    /// running script's `writeFile` and `readFile` can validate paths. An
    /// empty list means filesystem access is disabled. Used by
    /// [`MetaToolHandler::execute_tools`].
    pub fn with_write_roots(mut self, write_roots: Vec<PathBuf>) -> Self {
        self.write_roots = write_roots;
        self
    }

    /// Test-only: opt this sandbox into the real backoff schedule. Without
    /// this, `cfg(test)` builds short-circuit retry sleeps to zero so the
    /// suite stays fast. Used by the deadline-budget test.
    #[cfg(test)]
    pub(crate) fn with_real_backoff(mut self) -> Self {
        self.use_real_backoff = true;
        self
    }

    /// Test-only: shrink the per-run `writeFile` limits so limit-boundary
    /// behaviour can be exercised without multi-hundred-megabyte writes.
    /// Production runs always use [`WriteLimits::default`].
    #[cfg(test)]
    fn with_write_limits(mut self, write_limits: WriteLimits) -> Self {
        self.write_limits = write_limits;
        self
    }

    /// Test-only: shrink the per-run `readFile` limits so limit-boundary
    /// behaviour can be exercised without multi-hundred-megabyte reads.
    /// Production runs always use [`ReadLimits::default`].
    #[cfg(test)]
    fn with_read_limits(mut self, read_limits: ReadLimits) -> Self {
        self.read_limits = read_limits;
        self
    }

    /// Execute a JavaScript script in the sandbox.
    pub async fn execute(&self, script: &str) -> Result<Value, JsSandboxError> {
        let registry = self.registry.clone();
        let timeout = self.timeout;
        let use_real_backoff = self.use_real_backoff;
        let client_json = self.client_json.clone();
        let request_uid = self.request_uid.clone();
        let write_roots = self.write_roots.clone();
        let write_limits = self.write_limits;
        let read_limits = self.read_limits;
        let script = script.to_string();
        let handle = tokio::runtime::Handle::current();
        let catalog = self.registry.merged_catalog().await;

        let result = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || {
                execute_in_sandbox(
                    &script,
                    &catalog,
                    &registry,
                    &handle,
                    timeout,
                    use_real_backoff,
                    client_json,
                    request_uid,
                    write_roots,
                    write_limits,
                    read_limits,
                )
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

#[allow(clippy::too_many_arguments)]
fn execute_in_sandbox(
    script: &str,
    catalog: &[ToolInfo],
    registry: &Arc<dyn MetaToolRegistry>,
    handle: &tokio::runtime::Handle,
    sandbox_timeout: Duration,
    use_real_backoff: bool,
    client_json: String,
    request_uid: String,
    write_roots: Vec<PathBuf>,
    write_limits: WriteLimits,
    read_limits: ReadLimits,
) -> Result<Value, JsSandboxError> {
    let deadline = std::time::Instant::now() + sandbox_timeout;
    SANDBOX_STATE.with(|cell| {
        *cell.borrow_mut() = Some(SandboxState {
            registry: registry.clone(),
            handle: handle.clone(),
            deadline,
            use_real_backoff,
            client_json,
            request_uid,
            write_roots,
            write_limits,
            files_written: 0,
            bytes_written: 0,
            read_limits,
            files_read: 0,
            bytes_read: 0,
        });
    });
    let result = run_js(script, catalog);
    SANDBOX_STATE.with(|cell| {
        *cell.borrow_mut() = None;
    });
    result
}

/// Drive `fut` to completion on the sandbox's blocking thread, re-establishing
/// the outer inbound request's `request{request_uid=...,client=...}` span when
/// either of those signals was captured.
///
/// `JsSandbox::execute` hops onto a `spawn_blocking` thread, which does not
/// inherit the inbound `request` span. Without re-entering it here, the
/// upstream adapter's `current_request_context()` resolves no request UID and
/// no caller for sandbox-driven tool calls, so the "Tool call
/// completed/failed" log lines and
/// `ToolCallEvent::Started.{request_uid,client}` lose those signals.
/// Entering a fresh `request` span carrying `request_uid = %request_uid` and
/// `client = %client_json` lets `SpanFieldCaptureLayer` re-capture them exactly
/// as the direct path does. `SpanFieldCaptureLayer`'s visitor skips empty
/// `request_uid`/`client` values, so a missing signal records no field; when
/// both are empty the future runs without an extra span.
fn block_on_with_request_context<F>(
    handle: &tokio::runtime::Handle,
    client_json: &str,
    request_uid: &str,
    fut: F,
) -> F::Output
where
    F: std::future::Future,
{
    if client_json.is_empty() && request_uid.is_empty() {
        return handle.block_on(fut);
    }
    let span = tracing::info_span!(
        "request",
        method = "tools/call",
        request_uid = %request_uid,
        client = %client_json,
    );
    handle.block_on(fut.instrument(span))
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
    register_call_tool_with_retry(&mut context)?;
    register_write_file(&mut context)?;
    register_read_file(&mut context)?;
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

fn register_call_tool_with_retry(context: &mut Context) -> Result<(), JsSandboxError> {
    let f = NativeFunction::from_fn_ptr(call_tool_with_retry_native);
    let js_func = f.to_js_function(context.realm());
    context
        .register_global_property(
            boa_engine::js_string!("__call_tool_with_retry"),
            js_func,
            Attribute::READONLY | Attribute::NON_ENUMERABLE,
        )
        .map_err(|e| {
            JsSandboxError::Internal(format!("failed to register __call_tool_with_retry: {}", e))
        })?;
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
        // Pre-flight existence check so unknown tool names yield a fuzzy
        // suggestion error instead of the bare adapter "no tool found" text.
        // Argument validation (unknown keys, types, required fields, …) is
        // handled centrally by `route_tool_call` via JSON-Schema, so no
        // sandbox-side schema check is needed here.
        let catalog = state.handle.block_on(state.registry.merged_catalog());
        if !catalog.iter().any(|t| t.name == tool_name) {
            let msg = format_unknown_tool_error(&tool_name, &catalog);
            return Err(JsError::from(JsNativeError::error().with_message(msg)));
        }
        let res = block_on_with_request_context(
            &state.handle,
            &state.client_json,
            &state.request_uid,
            state.registry.route_tool_call(&tool_name, arguments),
        )
        .map_err(|e| {
            JsNativeError::error().with_message(format!("tool call '{}' failed: {}", tool_name, e))
        })?;
        Ok::<Value, JsError>(res)
    })?;

    let result_str = serde_json::to_string(&result)
        .map_err(|e| JsNativeError::error().with_message(format!("serialisation error: {}", e)))?;
    Ok(JsValue::from(boa_engine::js_string!(result_str.as_str())))
}

// ---------------------------------------------------------------------------
// Native function: __call_tool_with_retry(name, args_json, retry)
//   -> result_json_string
//
// Same pre-flight checks as `__call_tool` (existence, strict-schema), plus a
// retry-eligibility gate that requires the tool's annotations to declare it
// `readOnlyHint: true` or `idempotentHint: true` (and never `destructiveHint:
// true`). Loops up to `min(retry, MAX_RETRIES)` additional attempts on
// transient `AdapterError`s with backoff `RETRY_BACKOFFS_MS`.
//
// Application errors (`isError: true` envelopes) are returned as `Ok(value)`
// from `route_tool_call` — they are NOT retried; the JS `call()` helper turns
// them into a thrown `Error` on first occurrence.
// ---------------------------------------------------------------------------

fn call_tool_with_retry_native(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let tool_name = args
        .first()
        .ok_or_else(|| {
            JsNativeError::typ().with_message("__call_tool_with_retry: missing tool name")
        })?
        .to_string(context)?
        .to_std_string_escaped();

    let args_json_str = args
        .get(1)
        .ok_or_else(|| {
            JsNativeError::typ().with_message("__call_tool_with_retry: missing arguments")
        })?
        .to_string(context)?
        .to_std_string_escaped();

    let retry_requested = args
        .get(2)
        .and_then(|v| v.as_number())
        .map(|n| {
            if n.is_finite() && n > 0.0 {
                n as usize
            } else {
                0
            }
        })
        .unwrap_or(0);

    let arguments: Value = serde_json::from_str(&args_json_str).map_err(|e| {
        JsNativeError::typ()
            .with_message(format!("__call_tool_with_retry: invalid JSON args: {}", e))
    })?;

    let result = SANDBOX_STATE.with(|cell| {
        let borrow = cell.borrow();
        let state = borrow
            .as_ref()
            .ok_or_else(|| JsNativeError::error().with_message("sandbox state not initialised"))?;
        // Pre-flight existence + retry-eligibility checks. Argument validation
        // is handled centrally by `route_tool_call` via JSON-Schema, so the
        // eligibility gate is the only sandbox-side addition over `__call_tool`.
        let catalog = state.handle.block_on(state.registry.merged_catalog());
        let tool = match catalog.iter().find(|t| t.name == tool_name) {
            Some(t) => t,
            None => {
                let msg = format_unknown_tool_error(&tool_name, &catalog);
                return Err(JsError::from(JsNativeError::error().with_message(msg)));
            }
        };
        if !is_retry_eligible(tool.annotations.as_ref()) {
            return Err(JsError::from(JsNativeError::error().with_message(format!(
                "call('{}'): retry not allowed (tool not declared read-only or idempotent)",
                tool_name
            ))));
        }
        let max_retries = retry_requested.min(MAX_RETRIES);
        let registry = state.registry.clone();
        let deadline = state.deadline;
        let use_real_backoff = state.use_real_backoff;
        let res = block_on_with_request_context(
            &state.handle,
            &state.client_json,
            &state.request_uid,
            call_tool_with_retry_loop(
                registry.as_ref(),
                &tool_name,
                arguments,
                max_retries,
                deadline,
                use_real_backoff,
            ),
        )
        .map_err(|e| {
            JsNativeError::error().with_message(format!("tool call '{}' failed: {}", tool_name, e))
        })?;
        Ok::<Value, JsError>(res)
    })?;

    let result_str = serde_json::to_string(&result)
        .map_err(|e| JsNativeError::error().with_message(format!("serialisation error: {}", e)))?;
    Ok(JsValue::from(boa_engine::js_string!(result_str.as_str())))
}

/// Retry loop driving up to `1 + max_retries` attempts on transient errors.
/// Backoff is applied **between** attempts (no delay before the first try)
/// and is jittered by ±25% via [`jittered_backoff_ms`].
///
/// Aborts early when the next jittered sleep would push the wall clock past
/// `deadline`. The last-seen transient error is wrapped in an
/// `AdapterError::ProtocolError` whose message names the tool, the number of
/// attempts actually made, and embeds the underlying error's `Display` text.
/// This prevents the retry loop from being killed mid-sleep by the sandbox's
/// outer timeout while preserving the underlying cause for the caller.
///
/// `use_real_backoff` selects between [`RETRY_BACKOFF_SCHEDULE_MS`] and the
/// effective (possibly zero, in `cfg(test)`) [`RETRY_BACKOFFS_MS`] schedule.
/// Production callers pass `false` because `RETRY_BACKOFFS_MS` already mirrors
/// the real schedule outside `cfg(test)`. Tests pass `true` to opt back into
/// real sleeps.
async fn call_tool_with_retry_loop(
    registry: &dyn MetaToolRegistry,
    tool_name: &str,
    arguments: Value,
    max_retries: usize,
    deadline: std::time::Instant,
    use_real_backoff: bool,
) -> Result<Value, AdapterError> {
    let schedule: &[u64; MAX_RETRIES] = if use_real_backoff {
        &RETRY_BACKOFF_SCHEDULE_MS
    } else {
        &RETRY_BACKOFFS_MS
    };
    let mut attempt: usize = 0;
    let mut rng = rand::rng();
    loop {
        let res = registry.route_tool_call(tool_name, arguments.clone()).await;
        match res {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt < max_retries && is_transient_error(&e) {
                    let base_ms = schedule
                        .get(attempt)
                        .copied()
                        .unwrap_or(*schedule.last().unwrap_or(&0));
                    let backoff_ms = jittered_backoff_ms(base_ms, &mut rng);
                    // Deadline gate: if the next sleep would exceed the
                    // sandbox's wall-clock budget, wrap the last-seen
                    // transient error so the caller can distinguish a
                    // budget-exhausted retry from a single first-attempt
                    // failure. `attempt + 1` is the number of attempts made
                    // so far (the current failed call included).
                    if std::time::Instant::now() + Duration::from_millis(backoff_ms) >= deadline {
                        let attempts_made = attempt + 1;
                        return Err(AdapterError::ProtocolError(format!(
                            "call('{}'): retry deadline exceeded after {} attempts (last error: {})",
                            tool_name, attempts_made, e
                        )));
                    }
                    if backoff_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    }
                    attempt += 1;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Decide whether a tool may be retried based on its MCP annotations.
///
/// Allowed iff annotations exist AND (`readOnlyHint: true` OR
/// `idempotentHint: true`); `destructiveHint: true` overrides and disqualifies
/// even an idempotent-marked tool. Tools without annotations are conservatively
/// rejected.
fn is_retry_eligible(annotations: Option<&Value>) -> bool {
    let Some(obj) = annotations.and_then(|a| a.as_object()) else {
        return false;
    };
    if obj.get("destructiveHint").and_then(|v| v.as_bool()) == Some(true) {
        return false;
    }
    let read_only = obj.get("readOnlyHint").and_then(|v| v.as_bool()) == Some(true);
    let idempotent = obj.get("idempotentHint").and_then(|v| v.as_bool()) == Some(true);
    read_only || idempotent
}

/// Treat an `AdapterError` as transient if its `Display` text contains any of
/// the known retryable substrings (case-insensitive).
fn is_transient_error(err: &AdapterError) -> bool {
    let lower = err.to_string().to_lowercase();
    RETRY_TRANSIENT_SUBSTRINGS.iter().any(|s| lower.contains(s))
}

/// Build the message for a "tool not found" error, including up to three
/// fuzzy-matched suggestions when the catalog has nearby names.
fn format_unknown_tool_error(name: &str, catalog: &[ToolInfo]) -> String {
    let suggestions = suggest_tool_names(name, catalog);
    if suggestions.is_empty() {
        format!(
            "no tool named '{}'. Use list_tools or search_tools to discover available tools.",
            name
        )
    } else {
        let quoted: Vec<String> = suggestions.iter().map(|s| format!("'{}'", s)).collect();
        format!(
            "no tool named '{}'. Did you mean: {}? Use list_tools or search_tools to discover other tools.",
            name,
            quoted.join(", ")
        )
    }
}

/// Return up to three catalog tool names that are closest to `name` by
/// Optimal String Alignment distance (case-insensitive). Distances are
/// computed against both the full prefixed name and the suffix after `__`,
/// taking the smaller, so e.g. typo `ehco` suggests `ep__echo`.
fn suggest_tool_names(name: &str, catalog: &[ToolInfo]) -> Vec<String> {
    if catalog.is_empty() {
        return Vec::new();
    }
    let name_lower = name.to_lowercase();
    let len = name.chars().count();
    // Threshold scales gently with name length; clamped so very short names
    // still allow at least one edit and very long names don't drown in noise.
    let threshold = (len / 3).clamp(1, 4);
    let mut scored: Vec<(usize, &str)> = catalog
        .iter()
        .map(|t| {
            let full_lower = t.name.to_lowercase();
            let suffix_lower = t
                .name
                .split_once("__")
                .map(|(_, n)| n.to_lowercase())
                .unwrap_or_else(|| full_lower.clone());
            let d = strsim::osa_distance(&name_lower, &full_lower)
                .min(strsim::osa_distance(&name_lower, &suffix_lower));
            (d, t.name.as_str())
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored
        .into_iter()
        .filter(|(d, _)| *d <= threshold)
        .take(3)
        .map(|(_, n)| n.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Build the `tools` global object and `call` helper via JS eval.
//
// `tools` is a Proxy over a plain object containing one function per known
// catalog tool. Unknown property accesses still return a function (so
// `typeof tools["x"]` is `"function"` and `"x" in tools` is truthy), but
// invoking that function throws a fuzzy "no tool named …" error from the
// native side. Invoking `tools[name](args)` returns the *raw* MCP envelope
// (`{ content, structuredContent, isError, ... }`) — the indexer is the
// escape hatch for callers that need full envelope access.
//
// `call(name, args, opts)` is the recommended helper. It unwraps the
// envelope: on `isError` it throws an `Error` whose message includes the
// tool name and `content[0].text`; with `opts.raw === true` it returns the
// parsed envelope unchanged; otherwise it returns `structuredContent` when
// present, parses `content[0].text` when it begins with `[` or `{`, returns
// the text as-is when it doesn't look JSON-shaped, and falls back to the
// raw envelope when neither field is set.
//
// `opts.retry` (number) opts the call into transient-error retry. Routing
// switches to `__call_tool_with_retry`, which gates eligibility on the tool's
// `annotations` (`readOnlyHint` / `idempotentHint`, never `destructiveHint`)
// and retries on transient `AdapterError`s with capped exponential backoff.
// ---------------------------------------------------------------------------

fn register_tools_object(
    context: &mut Context,
    catalog: &[ToolInfo],
) -> Result<(), JsSandboxError> {
    let mut js_src = String::from("var __real_tools = {};\n");
    for tool in catalog {
        let name = &tool.name;
        js_src.push_str(&format!(
            "__real_tools[\"{name}\"] = function(args) {{ return JSON.parse(__call_tool(\"{name}\", JSON.stringify(args || {{}}))); }};\n"
        ));
    }
    js_src.push_str(
        r#"
function __unknown_tool_stub(name) {
  return function(args) {
    return JSON.parse(__call_tool(name, JSON.stringify(args || {})));
  };
}
var tools = new Proxy(__real_tools, {
  get: function(target, prop, receiver) {
    if (Object.prototype.hasOwnProperty.call(target, prop)) {
      return target[prop];
    }
    if (typeof prop === 'symbol' || prop in target) {
      return Reflect.get(target, prop, receiver);
    }
    return __unknown_tool_stub(prop);
  },
  has: function(target, prop) {
    if (typeof prop === 'string') return true;
    return prop in target;
  }
});
function call(name, args, opts) {
  var retry = (opts && typeof opts.retry === "number" && opts.retry > 0) ? opts.retry : 0;
  var r;
  if (retry > 0) {
    r = JSON.parse(__call_tool_with_retry(name, JSON.stringify(args || {}), retry));
  } else {
    r = JSON.parse(__call_tool(name, JSON.stringify(args || {})));
  }
  if (r && r.isError) {
    var msg = "";
    if (r.content && r.content[0] && typeof r.content[0].text === "string") {
      msg = r.content[0].text;
    }
    throw new Error("call('" + name + "') failed: " + msg);
  }
  if (opts && opts.raw) return r;
  if (r && r.structuredContent !== undefined) return r.structuredContent;
  if (r && r.content && r.content[0] && typeof r.content[0].text === "string") {
    var t = r.content[0].text;
    return /^\s*[\[{]/.test(t) ? JSON.parse(t) : t;
  }
  return r;
}
"#,
    );
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
// Native function: __write_file(path, data, encoding) -> canonical_path_string
//
// Backs the JS-facing `writeFile(absPath, data, opts?)` global. Absolute
// paths only; the fully-resolved destination must sit inside one of the
// canonical `relay.write_dirs` roots carried in [`SandboxState`]. Per-run
// resource limits ([`WriteLimits`]) cap the size of each file, the number
// of files, and the total bytes written by a single script run. Every
// rejection is a thrown JS Error and never leaves a partial file behind;
// the relay never deletes existing files.
// ---------------------------------------------------------------------------

fn register_write_file(context: &mut Context) -> Result<(), JsSandboxError> {
    let f = NativeFunction::from_fn_ptr(write_file_native);
    let js_func = f.to_js_function(context.realm());
    context
        .register_global_property(
            boa_engine::js_string!("__write_file"),
            js_func,
            Attribute::READONLY | Attribute::NON_ENUMERABLE,
        )
        .map_err(|e| JsSandboxError::Internal(format!("failed to register __write_file: {}", e)))?;
    const SRC: &str = r#"
function writeFile(path, data, opts) {
  var encoding = (opts && opts.encoding !== undefined) ? opts.encoding : "utf8";
  return __write_file(path, data, encoding);
}
"#;
    context
        .eval(Source::from_bytes(SRC.as_bytes()))
        .map_err(|e| {
            JsSandboxError::Internal(format!("failed to create writeFile helper: {}", e))
        })?;
    Ok(())
}

fn write_file_native(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let path_str = args
        .first()
        .ok_or_else(|| JsNativeError::typ().with_message("writeFile: missing path"))?
        .to_string(context)?
        .to_std_string_escaped();

    let data_val = args
        .get(1)
        .ok_or_else(|| JsNativeError::typ().with_message("writeFile: missing data"))?;
    if !data_val.is_string() {
        // Reject rather than coerce: ToString would silently write
        // "[object Object]" for objects or "1,2,3" for arrays.
        return Err(JsNativeError::typ()
            .with_message("writeFile: data must be a string")
            .into());
    }
    let data = data_val.to_string(context)?.to_std_string_escaped();

    let encoding = match args.get(2) {
        Some(v) if !v.is_undefined() && !v.is_null() => {
            v.to_string(context)?.to_std_string_escaped()
        }
        _ => "utf8".to_string(),
    };

    let (write_roots, limits, files_written, bytes_written) = SANDBOX_STATE.with(|cell| {
        let borrow = cell.borrow();
        let state = borrow
            .as_ref()
            .ok_or_else(|| JsNativeError::error().with_message("sandbox state not initialised"))?;
        Ok::<(Vec<PathBuf>, WriteLimits, usize, usize), JsError>((
            state.write_roots.clone(),
            state.write_limits,
            state.files_written,
            state.bytes_written,
        ))
    })?;

    if files_written >= limits.max_files_per_run {
        return Err(JsNativeError::error()
            .with_message(format!(
                "writeFile: per-run limit of {} file writes reached — no further files \
                 can be written by this script run",
                limits.max_files_per_run
            ))
            .into());
    }

    // Per-file size cap, checked before base64 decoding so an oversized
    // payload is rejected without allocating the decoded bytes. The ≈3n/4
    // estimate subtracts trailing '=' padding so it is an exact upper bound
    // on the decoded size — a payload whose decoded length is exactly the
    // cap is accepted, and one that passes here cannot exceed the cap.
    let estimated_len = match encoding.as_str() {
        "utf8" => Some(data.len()),
        "base64" => {
            let padding = data
                .as_bytes()
                .iter()
                .rev()
                .take(2)
                .filter(|&&b| b == b'=')
                .count();
            Some((data.len().saturating_mul(3) / 4).saturating_sub(padding))
        }
        _ => None,
    };
    if let Some(estimated_len) = estimated_len {
        if estimated_len > limits.max_file_bytes {
            return Err(JsNativeError::error()
                .with_message(format!(
                    "writeFile: data is {}{} bytes, which exceeds the per-file limit \
                     of {} bytes",
                    if encoding == "base64" {
                        "approximately "
                    } else {
                        ""
                    },
                    estimated_len,
                    limits.max_file_bytes
                ))
                .into());
        }
    }

    // Validate the destination before decoding: a disallowed path fails
    // with the actionable path error (instead of a decode error) and never
    // pays the decode allocation.
    let (dest, matched_root) = resolve_write_path("writeFile", &path_str, &write_roots)
        .map_err(|msg| JsNativeError::error().with_message(msg))?;

    let bytes: Vec<u8> = match encoding.as_str() {
        "utf8" => data.into_bytes(),
        "base64" => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|e| {
                    JsNativeError::error()
                        .with_message(format!("writeFile: invalid base64 data: {}", e))
                })?
        }
        other => {
            return Err(JsNativeError::typ()
                .with_message(format!(
                    "writeFile: unsupported encoding '{}' (expected \"utf8\" or \"base64\")",
                    other
                ))
                .into())
        }
    };

    if bytes_written.saturating_add(bytes.len()) > limits.max_total_bytes_per_run {
        return Err(JsNativeError::error()
            .with_message(format!(
                "writeFile: writing {} more bytes would exceed the per-run total \
                 write limit of {} bytes ({} bytes already written)",
                bytes.len(),
                limits.max_total_bytes_per_run,
                bytes_written
            ))
            .into());
    }

    write_atomic(&dest, &bytes, &matched_root)
        .map_err(|msg| JsNativeError::error().with_message(msg))?;

    SANDBOX_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.files_written += 1;
            state.bytes_written = state.bytes_written.saturating_add(bytes.len());
        }
    });

    let dest_str = dest.to_string_lossy();
    Ok(JsValue::from(boa_engine::js_string!(dest_str.as_ref())))
}

/// Validate `path_str` against the allowlist and return the canonical
/// path together with the allowlisted root it matched. `op` is the
/// JS-facing primitive name (`"writeFile"` or `"readFile"`) used to flavour
/// rejection messages. Rejections (empty/NUL path, relative path, `..`
/// components, paths outside every root — including via symlinks) come back
/// as `Err(message)` before anything touches the filesystem.
fn resolve_write_path(
    op: &str,
    path_str: &str,
    write_roots: &[PathBuf],
) -> Result<(PathBuf, PathBuf), String> {
    if path_str.is_empty() {
        return Err(format!(
            "{}: path must not be empty — {} requires an absolute \
             path inside a configured write directory",
            op, op
        ));
    }
    if path_str.contains('\0') {
        return Err(format!("{}: path must not contain a NUL byte", op));
    }
    let path = Path::new(path_str);
    if !path.is_absolute() {
        return Err(format!(
            "{}: '{}' is a relative path — {} requires an absolute path \
             inside a configured write directory",
            op, path_str, op
        ));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "{}: '{}' contains a '..' path component, which is not allowed",
            op, path_str
        ));
    }
    if write_roots.is_empty() {
        return Err(write_dirs_rejection_message(op, path_str, write_roots));
    }

    // Canonicalize the deepest existing ancestor, then re-append the
    // not-yet-existing components. This resolves symlinks in every existing
    // part of the path — so a symlink inside a root pointing outside is
    // caught by the prefix check below — without requiring the destination
    // itself to exist yet.
    let mut existing: &Path = path;
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                existing = parent;
            }
            _ => return Err(write_dirs_rejection_message(op, path_str, write_roots)),
        }
    }
    let mut resolved = std::fs::canonicalize(existing)
        .map_err(|e| format!("{}: failed to resolve '{}': {}", op, path_str, e))?;
    for name in missing.iter().rev() {
        resolved.push(name);
    }

    match write_roots.iter().find(|root| resolved.starts_with(root)) {
        Some(root) => Ok((resolved, root.clone())),
        None => Err(write_dirs_rejection_message(op, path_str, write_roots)),
    }
}

/// The actionable "not inside a configured write directory" message,
/// prefixed with the JS-facing primitive name `op`. When roots are
/// configured, the currently-allowed directories are listed. For `readFile`
/// the message notes that reads are scoped to the same allowlist as writes.
fn write_dirs_rejection_message(op: &str, path_str: &str, write_roots: &[PathBuf]) -> String {
    let scope_note = if op == "readFile" {
        " (readFile is scoped to the same [relay] write_dirs allowlist as writeFile)"
    } else {
        ""
    };
    let mut msg = format!(
        "{}: '{}' is not inside a configured write directory{}. Add one under \
         [relay] write_dirs in ~/.endara/config.toml, or in the Endara desktop app \
         under Settings → Write directories.",
        op, path_str, scope_note
    );
    if !write_roots.is_empty() {
        let list = write_roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        msg.push_str(&format!(" Currently allowed: {}.", list));
    }
    msg
}

/// Split `dest` into its parent directory and file name for [`write_atomic`].
fn split_write_dest(dest: &Path) -> Result<(&Path, &std::ffi::OsStr), String> {
    let dir = dest
        .parent()
        .ok_or_else(|| format!("writeFile: '{}' has no parent directory", dest.display()))?;
    let file_name = dest
        .file_name()
        .ok_or_else(|| format!("writeFile: '{}' does not name a file", dest.display()))?;
    Ok((dir, file_name))
}

/// Unique temp-file name next to `file_name` for [`write_atomic`].
fn write_tmp_name(file_name: &std::ffi::OsStr) -> String {
    // Process-wide monotonic counter: concurrent writes to the same
    // destination can observe the same SystemTime, so the timestamp alone
    // does not make the temp name unique within this process.
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        ".{}.{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        unique,
        seq
    )
}

/// Write `bytes` to `dest` via a temp file in the destination directory plus
/// an atomic rename, creating missing parent directories first. `dest` must
/// already be validated by [`resolve_write_path`] against `root` (the
/// allowlisted root it matched). Every filesystem step is anchored to a
/// directory handle of `root`: missing parents are created with `mkdirat`
/// along a beneath-root open that refuses to follow symlinks
/// ([`open_beneath::open_dir_beneath_creating`] — `openat2` re-resolving
/// from the root at every step on Linux, an `openat` walk elsewhere), the
/// temp file is created with `openat(O_CREAT | O_EXCL)` on the destination
/// directory handle, and the publish is a `renameat` on that same handle.
/// Because `dest` is canonical, any symlink met on the way is a
/// post-validation swap and fails the write (`ELOOP`), as does a directory
/// renamed out of the root while `openat2` resolves it (`EXDEV`): no lookup
/// here can be redirected through a symlink, on any tier. Containment is
/// enforced while the directory handle is resolved, not afterwards — the
/// handle is a capability, so if the destination directory itself is renamed
/// out of the root between `open_dir_beneath_creating` returning and the
/// `renameat`, the file lands wherever that directory now lives (a location
/// the renaming actor could already write to; see the caveat in
/// [`open_beneath`]). If the root itself was deleted
/// after config resolution the write fails (the root handle cannot be
/// opened); the "directories are never auto-created" stance in
/// [`crate::config::resolve_write_roots`] only covers resolution time. On
/// failure the temp file is removed — no partial destination file is ever
/// observable.
#[cfg(unix)]
fn write_atomic(dest: &Path, bytes: &[u8], root: &Path) -> Result<(), String> {
    use std::os::unix::io::{AsFd as _, AsRawFd as _};

    let (dir, file_name) = split_write_dest(dest)?;
    let escaped = || {
        format!(
            "writeFile: '{}' escaped the configured write directory during the write",
            dest.display()
        )
    };
    let rel_dir = dir.strip_prefix(root).map_err(|_| escaped())?;
    let parent_dirs_error = |e: std::io::Error| {
        if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::EXDEV)) {
            return escaped();
        }
        format!(
            "writeFile: failed to create parent directories for '{}': {}",
            dest.display(),
            e
        )
    };
    let root_fd = open_beneath::open_root(root).map_err(|e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            return escaped();
        }
        format!(
            "writeFile: configured write directory '{}' is not accessible: {} — recreate it \
             or update [relay] write_dirs in ~/.endara/config.toml",
            root.display(),
            e
        )
    })?;
    let dir_fd = open_beneath::open_dir_beneath_creating(root_fd.as_fd(), rel_dir)
        .map_err(parent_dirs_error)?;
    let tmp = std::ffi::OsString::from(write_tmp_name(file_name));
    // O_EXCL guarantees this call exclusively owns the temp file even if the
    // name somehow collides (e.g. across processes); 0o666 matches what
    // `OpenOptions::create_new` would apply before the umask.
    let write_result = open_beneath::openat(
        dir_fd.as_raw_fd(),
        &tmp,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
        0o666,
    )
    .map(std::fs::File::from)
    .and_then(|mut f| std::io::Write::write_all(&mut f, bytes));
    if let Err(e) = write_result {
        let _ = unlinkat(dir_fd.as_raw_fd(), &tmp);
        return Err(format!(
            "writeFile: failed to write '{}': {}",
            dest.display(),
            e
        ));
    }
    if let Err(e) = renameat(dir_fd.as_raw_fd(), &tmp, file_name) {
        let _ = unlinkat(dir_fd.as_raw_fd(), &tmp);
        return Err(format!(
            "writeFile: failed to finalise '{}': {}",
            dest.display(),
            e
        ));
    }
    Ok(())
}

/// `renameat(2)` of `from` to `to`, both relative to `dirfd`.
#[cfg(unix)]
fn renameat(
    dirfd: libc::c_int,
    from: &std::ffi::OsStr,
    to: &std::ffi::OsStr,
) -> std::io::Result<()> {
    let c_from = open_beneath::c_string(from)?;
    let c_to = open_beneath::c_string(to)?;
    // SAFETY: `dirfd` is an open directory fd and both names are valid
    // NUL-terminated strings that outlive the call.
    let rc = unsafe { libc::renameat(dirfd, c_from.as_ptr(), dirfd, c_to.as_ptr()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `unlinkat(2)` of the file `name` relative to `dirfd`.
#[cfg(unix)]
fn unlinkat(dirfd: libc::c_int, name: &std::ffi::OsStr) -> std::io::Result<()> {
    let c_name = open_beneath::c_string(name)?;
    // SAFETY: `dirfd` is an open directory fd and `c_name` a valid
    // NUL-terminated string that outlives the call.
    let rc = unsafe { libc::unlinkat(dirfd, c_name.as_ptr(), 0) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Non-unix [`write_atomic`]: after `create_dir_all` the destination
/// directory is re-canonicalized and containment is re-asserted, narrowing
/// (but not closing — this platform has no beneath-root open) the
/// validate→write window in which a checked directory could be swapped for
/// a symlink pointing outside the root. If the root itself was deleted after
/// config resolution, `create_dir_all` recreates it — still inside the
/// allowed prefix.
#[cfg(not(unix))]
fn write_atomic(dest: &Path, bytes: &[u8], root: &Path) -> Result<(), String> {
    let (dir, file_name) = split_write_dest(dest)?;
    std::fs::create_dir_all(dir).map_err(|e| {
        format!(
            "writeFile: failed to create parent directories for '{}': {}",
            dest.display(),
            e
        )
    })?;
    let canonical_dir = std::fs::canonicalize(dir)
        .map_err(|e| format!("writeFile: failed to resolve '{}': {}", dest.display(), e))?;
    if !canonical_dir.starts_with(root) {
        return Err(format!(
            "writeFile: '{}' escaped the configured write directory during the write",
            dest.display()
        ));
    }
    let final_dest = canonical_dir.join(file_name);
    let tmp = canonical_dir.join(write_tmp_name(file_name));
    // create_new(true) guarantees this call exclusively owns the temp file
    // even if the name somehow collides (e.g. across processes).
    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, bytes));
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "writeFile: failed to write '{}': {}",
            dest.display(),
            e
        ));
    }
    if let Err(e) = std::fs::rename(&tmp, &final_dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "writeFile: failed to finalise '{}': {}",
            dest.display(),
            e
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Native function: __read_file(path, encoding) -> contents_string
//
// Backs the JS-facing `readFile(absPath, opts?)` global. Absolute paths
// only; the fully-resolved file must sit inside one of the canonical
// `relay.write_dirs` roots carried in [`SandboxState`]. Per-run resource
// limits ([`ReadLimits`]) cap the on-disk size of each file (checked
// before any bytes are read), the number of reads, and the total bytes
// read by a single script run. Every rejection is a thrown JS Error.
// ---------------------------------------------------------------------------

fn register_read_file(context: &mut Context) -> Result<(), JsSandboxError> {
    let f = NativeFunction::from_fn_ptr(read_file_native);
    let js_func = f.to_js_function(context.realm());
    context
        .register_global_property(
            boa_engine::js_string!("__read_file"),
            js_func,
            Attribute::READONLY | Attribute::NON_ENUMERABLE,
        )
        .map_err(|e| JsSandboxError::Internal(format!("failed to register __read_file: {}", e)))?;
    const SRC: &str = r#"
function readFile(path, opts) {
  var encoding = (opts && opts.encoding !== undefined) ? opts.encoding : "utf8";
  return __read_file(path, encoding);
}
"#;
    context
        .eval(Source::from_bytes(SRC.as_bytes()))
        .map_err(|e| {
            JsSandboxError::Internal(format!("failed to create readFile helper: {}", e))
        })?;
    Ok(())
}

fn read_file_native(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let path_str = args
        .first()
        .ok_or_else(|| JsNativeError::typ().with_message("readFile: missing path"))?
        .to_string(context)?
        .to_std_string_escaped();

    let encoding = match args.get(1) {
        Some(v) if !v.is_undefined() && !v.is_null() => {
            v.to_string(context)?.to_std_string_escaped()
        }
        _ => "utf8".to_string(),
    };
    if encoding != "utf8" && encoding != "base64" {
        return Err(JsNativeError::typ()
            .with_message(format!(
                "readFile: unsupported encoding '{}' (expected \"utf8\" or \"base64\")",
                encoding
            ))
            .into());
    }

    let (write_roots, limits, files_read, bytes_read) = SANDBOX_STATE.with(|cell| {
        let borrow = cell.borrow();
        let state = borrow
            .as_ref()
            .ok_or_else(|| JsNativeError::error().with_message("sandbox state not initialised"))?;
        Ok::<(Vec<PathBuf>, ReadLimits, usize, usize), JsError>((
            state.write_roots.clone(),
            state.read_limits,
            state.files_read,
            state.bytes_read,
        ))
    })?;

    if files_read >= limits.max_files_per_run {
        return Err(JsNativeError::error()
            .with_message(format!(
                "readFile: per-run limit of {} file reads reached — no further files \
                 can be read by this script run",
                limits.max_files_per_run
            ))
            .into());
    }

    let (source, matched_root) = resolve_write_path("readFile", &path_str, &write_roots)
        .map_err(|msg| JsNativeError::error().with_message(msg))?;

    // Only regular files are readable. Opening a FIFO blocks until a writer
    // appears, and reading a device or socket can block indefinitely, inside
    // a native call the JS deadline cannot interrupt. Check the type first so
    // the common case never opens such an entry at all — `O_NONBLOCK` below
    // covers FIFOs, but a device driver's open can still block or have side
    // effects — then re-validate the handle metadata for whatever inode was
    // actually opened. On unix the pre-check is anchored beneath the root
    // like the open, so it never stats (or leaks the existence of) anything
    // a swapped-in symlink points at.
    precheck_for_read(&path_str, &source, &matched_root)?;

    // Open once and derive metadata from the handle so the type/size checks
    // and the bounded read below all apply to the same inode, rather than
    // to whatever the pathname resolves to on a second lookup. On unix the
    // open itself cannot block on a FIFO swapped in after the pre-check
    // (`O_NONBLOCK`) and is anchored beneath the matched root, so a symlink
    // swapped in for any component of the canonicalized path — not just the
    // final one — is rejected rather than followed; the fstat on the handle
    // is authoritative.
    let mut file = open_for_read(&source, &matched_root)
        .map_err(|e| JsNativeError::error().with_message(open_error_message(&path_str, &e)))?;
    let meta = file.metadata().map_err(|e| {
        JsNativeError::error()
            .with_message(format!("readFile: failed to read '{}': {}", path_str, e))
    })?;
    reject_non_regular(&path_str, &meta)?;

    // Size caps are checked against the on-disk size so an oversized file
    // is rejected before any bytes are read or allocated.
    let file_len = usize::try_from(meta.len()).unwrap_or(usize::MAX);
    if file_len > limits.max_file_bytes {
        return Err(JsNativeError::error()
            .with_message(format!(
                "readFile: '{}' is {} bytes, which exceeds the per-file limit \
                 of {} bytes",
                path_str, file_len, limits.max_file_bytes
            ))
            .into());
    }
    if bytes_read.saturating_add(file_len) > limits.max_total_bytes_per_run {
        return Err(JsNativeError::error()
            .with_message(format!(
                "readFile: reading {} more bytes would exceed the per-run total \
                 read limit of {} bytes ({} bytes already read)",
                file_len, limits.max_total_bytes_per_run, bytes_read
            ))
            .into());
    }

    // The metadata size is only a hint: the file can grow between the check
    // and the read. Bound the read itself by the remaining allowance and
    // reject if more bytes than that are available.
    let allowed = limits
        .max_file_bytes
        .min(limits.max_total_bytes_per_run.saturating_sub(bytes_read));
    let mut bytes = Vec::with_capacity(file_len.min(allowed));
    let outcome = read_bounded(&mut file, allowed, &mut bytes);
    drop(file);
    let read_len = bytes.len();

    // Account for the read as soon as the bytes have been pulled from disk —
    // on every exit: I/O error (which may have appended partial data),
    // over-cap, and before decoding. Otherwise a script could loop on a file
    // that keeps failing, growing past the cap, or that is not UTF-8, and
    // never consume quota.
    SANDBOX_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.files_read += 1;
            state.bytes_read = state.bytes_read.saturating_add(read_len);
        }
    });

    let exceeded = outcome.map_err(|e| {
        JsNativeError::error()
            .with_message(format!("readFile: failed to read '{}': {}", path_str, e))
    })?;
    if exceeded {
        return Err(JsNativeError::error()
            .with_message(format!(
                "readFile: '{}' exceeds the remaining read allowance of {} bytes \
                 (per-file limit {} bytes, per-run total limit {} bytes, {} bytes \
                 already read)",
                path_str,
                allowed,
                limits.max_file_bytes,
                limits.max_total_bytes_per_run,
                bytes_read
            ))
            .into());
    }

    let contents = match encoding.as_str() {
        "utf8" => String::from_utf8(bytes).map_err(|_| {
            JsNativeError::error().with_message(format!(
                "readFile: '{}' does not contain valid UTF-8 — use \
                 {{ encoding: \"base64\" }} to read binary files",
                path_str
            ))
        })?,
        _ => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        }
    };

    Ok(JsValue::from(boa_engine::js_string!(contents.as_str())))
}

/// Open `source` read-only for `readFile`. `source` must already be
/// validated by [`resolve_write_path`] against `root` (the allowlisted root
/// it matched). The open is anchored to a directory handle of `root` and
/// walks `source`'s path relative to it via [`open_beneath::open_beneath`],
/// which refuses to follow a symlink at any component: because `source` is
/// canonical, any symlink met is a post-validation swap and fails with
/// `ELOOP` instead of being followed — possibly out of the root. The open
/// also carries `O_NONBLOCK`, so a FIFO with no writer returns a handle
/// immediately instead of parking the sandbox thread (this is guaranteed for
/// FIFOs only; a device driver's open may still block, which is why the
/// caller keeps its path-level type pre-check), and `O_NOCTTY`, so a tty
/// device node can never become the controlling terminal as a side effect of
/// the open (std does not set it). The caller must still fstat the returned
/// handle and reject anything that is not a regular file; regular files are
/// unaffected by these flags, so the bounded read that follows behaves
/// exactly as a plain open would.
#[cfg(unix)]
fn open_for_read(source: &Path, root: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::AsFd as _;

    let rel = source.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path escaped its matched write directory before the open",
        )
    })?;
    let root_fd = open_beneath::open_root(root)?;
    open_beneath::open_beneath(
        root_fd.as_fd(),
        rel,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY,
    )
}

/// Non-unix [`open_for_read`]: a plain read-only open of the canonical path.
/// This platform has no beneath-root open, so the validate→open window is
/// not closed here.
#[cfg(not(unix))]
fn open_for_read(source: &Path, _root: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).open(source)
}

/// Path-level type pre-check for `readFile`: rejects directories and
/// non-regular entries (FIFOs, devices, sockets) before anything is opened.
/// On unix the stat is anchored beneath `root` with the same containment as
/// [`open_for_read`] — no symlink is followed at any component, and no
/// lookup is re-resolved from `/` — and it never opens the entry, so a
/// device driver's `open` cannot run. Errors use the same messages as a
/// failed open.
#[cfg(unix)]
fn precheck_for_read(path_str: &str, source: &Path, root: &Path) -> JsResult<()> {
    let st = stat_for_read(source, root)
        .map_err(|e| JsNativeError::error().with_message(open_error_message(path_str, &e)))?;
    let kind = st.st_mode & libc::S_IFMT;
    reject_non_regular_kind(path_str, kind == libc::S_IFDIR, kind == libc::S_IFREG)
}

/// Non-unix [`precheck_for_read`]: a plain `metadata` of the canonical path.
#[cfg(not(unix))]
fn precheck_for_read(path_str: &str, source: &Path, _root: &Path) -> JsResult<()> {
    let meta = std::fs::metadata(source)
        .map_err(|e| JsNativeError::error().with_message(open_error_message(path_str, &e)))?;
    reject_non_regular(path_str, &meta)
}

/// Anchored `stat` of `source` beneath `root` (see [`open_for_read`] for the
/// containment contract; this is its non-opening counterpart).
#[cfg(unix)]
fn stat_for_read(source: &Path, root: &Path) -> std::io::Result<libc::stat> {
    use std::os::unix::io::AsFd as _;

    let rel = source.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path escaped its matched write directory before the open",
        )
    })?;
    let root_fd = open_beneath::open_root(root)?;
    open_beneath::stat_beneath(root_fd.as_fd(), rel)
}

/// JS-facing message for a failed [`open_for_read`]. A missing file and, on
/// unix, `ELOOP` get self-explanatory messages; everything else carries the
/// OS error text. `ELOOP` is what the beneath-root open returns for a
/// symlink at any component of the path (final or intermediate) and what
/// the kernel returns for a symlink loop, so the message covers all three.
fn open_error_message(path_str: &str, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        return format!("readFile: '{}' does not exist", path_str);
    }
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ELOOP) {
        return format!(
            "readFile: '{}' refers to a symbolic link or a path containing a \
             symbolic link loop, which cannot be read",
            path_str
        );
    }
    format!("readFile: failed to read '{}': {}", path_str, e)
}

fn reject_non_regular(path_str: &str, meta: &std::fs::Metadata) -> JsResult<()> {
    reject_non_regular_kind(path_str, meta.is_dir(), meta.is_file())
}

fn reject_non_regular_kind(path_str: &str, is_dir: bool, is_file: bool) -> JsResult<()> {
    if is_dir {
        return Err(JsNativeError::error()
            .with_message(format!(
                "readFile: '{}' is a directory, not a file",
                path_str
            ))
            .into());
    }
    if !is_file {
        return Err(JsNativeError::error()
            .with_message(format!(
                "readFile: '{}' is not a regular file (FIFOs, devices, and \
                 sockets cannot be read)",
                path_str
            ))
            .into());
    }
    Ok(())
}

/// Read at most `allowed + 1` bytes from `reader` into `bytes`. Returns
/// `true` when the reader yielded more than `allowed` bytes, so a file that
/// grows after its size was checked cannot exceed the caller's budget.
/// Whatever was pulled stays appended to `bytes` on every outcome — including
/// an I/O error after partial data — so the caller can account for the I/O
/// that actually happened.
fn read_bounded<R: std::io::Read>(
    reader: &mut R,
    allowed: usize,
    bytes: &mut Vec<u8>,
) -> std::io::Result<bool> {
    use std::io::Read as _;
    let take_len = u64::try_from(allowed).unwrap_or(u64::MAX).saturating_add(1);
    let start = bytes.len();
    reader.take(take_len).read_to_end(bytes)?;
    Ok(bytes.len() - start > allowed)
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
    /// Catalog/routing source for the three meta-tools. Held as
    /// `Arc<dyn MetaToolRegistry>` (locked decision Relay #2) so a profile
    /// view filters which tools `list_tools`/`search_tools` see and which
    /// tools `execute_tools` can reach via the JS sandbox.
    registry: Arc<dyn MetaToolRegistry>,
    sandbox_timeout: Duration,
    /// Shared, hot-reloadable `relay.write_dirs` allowlist handle. The
    /// handler snapshots it per `execute_tools` call (see
    /// [`Self::execute_tools`]) so a running script keeps the allowlist it
    /// started with while later scripts observe a hot-reloaded value.
    /// Defaults to an empty (writing-disabled) handle; production wiring
    /// passes the process-wide handle via [`Self::with_write_roots`].
    write_roots: SharedWriteRoots,
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
    /// Build a handler backed by the given registry. Accepts any `Arc<R>`
    /// where `R: MetaToolRegistry + 'static`, so callers can pass either
    /// `Arc<AdapterRegistry>` (global `/mcp`) or
    /// `Arc<ProfileRegistryView>` (per-profile `/mcp/{profile}`) without
    /// explicit `as Arc<dyn ...>` casting at call sites — the trait-object
    /// coercion fires at the [`Self::from_dyn`] call site.
    ///
    /// `allow(dead_code)`: production code paths build per-profile
    /// handlers via `MetaToolHandler::new(Arc::new(view.clone()), ...)`
    /// in `ProfileRegistry::rebuild`, which goes through this generic
    /// constructor; the bin compilation unit (which doesn't see the
    /// rebuild path) still flags it without this.
    #[allow(dead_code)]
    pub fn new<R>(registry: Arc<R>, sandbox_timeout: Duration) -> Self
    where
        R: MetaToolRegistry + 'static,
    {
        Self::from_dyn(registry, sandbox_timeout)
    }

    /// Construct directly from an already-typed `Arc<dyn MetaToolRegistry>`.
    /// Used internally and by callers that hold the trait object without
    /// the concrete type in scope.
    pub fn from_dyn(registry: Arc<dyn MetaToolRegistry>, sandbox_timeout: Duration) -> Self {
        Self {
            registry,
            sandbox_timeout,
            write_roots: Arc::new(std::sync::RwLock::new(Vec::new())),
            search_index_cache: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            search_index_rebuild_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Attach the shared `relay.write_dirs` allowlist handle. The handle is
    /// shared with the config watcher, which swaps its contents on hot
    /// reload; the handler reads a snapshot per `execute_tools` call. Called
    /// by `main.rs` (global handler) and `ProfileRegistry::rebuild`
    /// (per-profile handlers).
    pub fn with_write_roots(mut self, write_roots: SharedWriteRoots) -> Self {
        self.write_roots = write_roots;
        self
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

    /// `execute_tools` — run JS in sandbox. The sandbox inherits the
    /// handler's [`MetaToolRegistry`] backing, so a per-profile handler
    /// gives the script a profile-filtered `tools.call()`.
    ///
    /// `client_json` is the JSON-serialised [`crate::events::ClientIdentity`]
    /// of the outer inbound request (empty when no caller identity is known)
    /// and `request_uid` is the outer request's canonical collision-free UID
    /// (empty when none was minted). Both are threaded into the sandbox so
    /// inner upstream tool calls re-establish the caller's
    /// `request{request_uid=...,client=...}` span across the blocking-thread
    /// hop — keeping the UID and caller visible on the aggregated "Tool call
    /// completed/failed" log lines and `ToolCallEvent::Started`.
    pub async fn execute_tools(
        &self,
        script: &str,
        client_json: &str,
        request_uid: &str,
    ) -> Result<Value, JsSandboxError> {
        // Snapshot the shared allowlist for this script run. The brief
        // std-RwLock read never crosses an `.await`; a poisoned lock (a
        // writer panicked mid-swap) degrades to writing-disabled rather
        // than propagating the panic into the request path.
        let write_roots = self
            .write_roots
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let sandbox = JsSandbox::from_dyn(self.registry.clone(), self.sandbox_timeout)
            .with_client(client_json.to_string())
            .with_request_uid(request_uid.to_string())
            .with_write_roots(write_roots);
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
    use crate::registry::AdapterRegistry;
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
            ..Default::default()
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
            ..Default::default()
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

    // --- call() helper + fuzzy tool-not-found tests ---

    #[tokio::test]
    async fn test_js_sandbox_call_helper_known_tool() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("echo", { text: "hi" });"#)
            .await
            .unwrap();
        assert_eq!(result["called"], "echo");
        assert_eq!(result["args"]["text"], "hi");
    }

    #[tokio::test]
    async fn test_js_sandbox_call_helper_omitted_args_defaults_to_object() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return call("echo");"#).await.unwrap();
        assert_eq!(result["called"], "echo");
        assert!(result["args"].is_object(), "args should default to {{}}");
    }

    #[tokio::test]
    async fn test_js_sandbox_call_unknown_tool_suggests_close_match() {
        // "ehco" is one transposition away from "echo" — must be suggested.
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return call("ehco", {});"#).await;
        assert!(result.is_err(), "expected unknown tool to throw");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("no tool named 'ehco'"),
            "error should name the missing tool: {}",
            err
        );
        assert!(
            err.contains("Did you mean") && err.contains("'echo'"),
            "error should suggest 'echo': {}",
            err
        );
        assert!(
            err.contains("list_tools") && err.contains("search_tools"),
            "error should point at list_tools / search_tools: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_js_sandbox_tools_indexer_unknown_throws_on_invocation() {
        // typeof + `in` must remain truthy (throw-on-invocation, not on access),
        // and invoking the unknown stub must surface the same fuzzy error.
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let probe = sandbox
            .execute(r#"return { tof: typeof tools["ehco"], inOp: ("ehco" in tools) };"#)
            .await
            .unwrap();
        assert_eq!(probe["tof"], "function");
        assert_eq!(probe["inOp"], true);

        let result = sandbox.execute(r#"return tools["ehco"]({});"#).await;
        assert!(result.is_err(), "invoking unknown tool stub must throw");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("no tool named 'ehco'") && err.contains("'echo'"),
            "indexer-form unknown tool must produce the same fuzzy error: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_js_sandbox_call_unknown_tool_no_close_match_omits_did_you_mean() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("zzzzzzzzzzzzzz", {});"#)
            .await;
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("no tool named 'zzzzzzzzzzzzzz'"),
            "error should name the missing tool: {}",
            err
        );
        assert!(
            !err.contains("Did you mean"),
            "no nearby names — should not include Did you mean: {}",
            err
        );
        assert!(
            err.contains("list_tools") && err.contains("search_tools"),
            "error should still point at list_tools / search_tools: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_js_sandbox_tools_known_indexer_still_works() {
        // Sanity: the Proxy must not change behavior for real catalog tools.
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return tools["echo"]({ msg: "via-indexer" });"#)
            .await
            .unwrap();
        assert_eq!(result["called"], "echo");
        assert_eq!(result["args"]["msg"], "via-indexer");
    }

    // --- call() envelope-unwrapping tests ---
    //
    // These use a dedicated adapter that returns a per-tool fixed envelope
    // so we can drive each branch of the unwrap logic deterministically.

    struct EnvelopeAdapter {
        tools: Vec<ToolInfo>,
        responses: std::collections::HashMap<String, Value>,
    }

    #[async_trait]
    impl McpAdapter for EnvelopeAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(&self, name: &str, _arguments: Value) -> Result<Value, AdapterError> {
            Ok(self.responses.get(name).cloned().unwrap_or(Value::Null))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    async fn make_envelope_registry(responses: Vec<(&str, Value)>) -> Arc<AdapterRegistry> {
        let registry = AdapterRegistry::new();
        let tools: Vec<ToolInfo> = responses
            .iter()
            .map(|(n, _)| make_tool(n, "envelope tool"))
            .collect();
        let map: std::collections::HashMap<String, Value> = responses
            .into_iter()
            .map(|(n, v)| (n.to_string(), v))
            .collect();
        registry
            .register(
                "env".into(),
                Box::new(EnvelopeAdapter {
                    tools,
                    responses: map,
                }),
                "stdio".into(),
                None,
                None,
            )
            .await;
        Arc::new(registry)
    }

    #[tokio::test]
    async fn test_js_sandbox_call_returns_structured_content() {
        let reg =
            make_envelope_registry(vec![("sc", json!({ "structuredContent": { "foo": 1 } }))])
                .await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return call("sc", {});"#).await.unwrap();
        assert_eq!(result, json!({ "foo": 1 }));
    }

    #[tokio::test]
    async fn test_js_sandbox_call_parses_json_text_content() {
        let reg = make_envelope_registry(vec![(
            "tj",
            json!({ "content": [{ "type": "text", "text": "{\"x\":42}" }] }),
        )])
        .await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return call("tj", {});"#).await.unwrap();
        assert_eq!(result, json!({ "x": 42 }));
    }

    #[tokio::test]
    async fn test_js_sandbox_call_returns_text_when_not_json_shaped() {
        let reg = make_envelope_registry(vec![(
            "tt",
            json!({ "content": [{ "type": "text", "text": "hello" }] }),
        )])
        .await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return call("tt", {});"#).await.unwrap();
        assert_eq!(result, json!("hello"));
    }

    #[tokio::test]
    async fn test_js_sandbox_call_throws_on_is_error_envelope() {
        let reg = make_envelope_registry(vec![(
            "boom",
            json!({
                "isError": true,
                "content": [{ "type": "text", "text": "upstream broke" }]
            }),
        )])
        .await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox.execute(r#"return call("boom", {});"#).await;
        assert!(result.is_err(), "expected isError envelope to throw");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("boom"),
            "error should mention the tool name: {}",
            err
        );
        assert!(
            err.contains("upstream broke"),
            "error should include content[0].text: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_js_sandbox_call_raw_opt_returns_envelope() {
        let reg = make_envelope_registry(vec![(
            "raw",
            json!({
                "structuredContent": { "foo": 1 },
                "content": [{ "type": "text", "text": "{\"foo\":1}" }]
            }),
        )])
        .await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("raw", {}, { raw: true });"#)
            .await
            .unwrap();
        assert_eq!(result["structuredContent"], json!({ "foo": 1 }));
        assert_eq!(result["content"][0]["text"], "{\"foo\":1}");
    }

    // --- unknown-parameter rejection tests ---
    //
    // These exercise the strict-schema check in `__call_tool` that rejects
    // arg keys not declared by the tool's `input_schema.properties` before
    // routing to the upstream adapter. The mock adapter echoes its input,
    // so a successful pass-through round-trips the args back to the test.

    async fn registry_with_single_tool(tool: ToolInfo) -> Arc<AdapterRegistry> {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::new(vec![tool])),
                "stdio".into(),
                None,
                None,
            )
            .await;
        Arc::new(registry)
    }

    // `additionalProperties: false` makes the schema closed so the centralised
    // `route_tool_call` JSON-Schema layer (spec §4.4) rejects unknown keys —
    // the sandbox no longer runs its own permissive-by-default arg check.
    fn echo_strict_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to echo back" },
                "count": { "type": "number", "description": "Repeat count" }
            },
            "required": ["text"],
            "additionalProperties": false
        })
    }

    #[tokio::test]
    async fn test_js_sandbox_strict_schema_rejects_unknown_param() {
        // Migrated to the centralised `route_tool_call` JSON-Schema path (spec
        // §10.5 #29/#30): the structured error names the tool, the offending
        // key, and the valid parameter list so the model can self-correct.
        let tool = make_tool_with_schema("echo", "Echo tool", echo_strict_schema());
        let reg = registry_with_single_tool(tool).await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("echo", { text: "hi", zzz: 1, qqq: 2 });"#)
            .await;
        assert!(result.is_err(), "unknown params should reject");
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("'echo'"), "error names tool: {}", err);
        assert!(
            err.contains("'zzz'") && err.contains("'qqq'"),
            "error lists every unknown key: {}",
            err
        );
        assert!(
            err.contains("unknown parameter"),
            "error explains the rejection: {}",
            err
        );
        // The valid parameter names surface (descriptions are no longer part of
        // the centralised message format).
        assert!(
            err.contains("text") && err.contains("count"),
            "valid params listed: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_js_sandbox_additional_properties_true_is_permissive() {
        let mut schema = echo_strict_schema();
        schema["additionalProperties"] = json!(true);
        let tool = make_tool_with_schema("echo", "Echo tool", schema);
        let reg = registry_with_single_tool(tool).await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("echo", { text: "hi", extra: "ok" });"#)
            .await
            .expect("additionalProperties:true should not reject");
        assert_eq!(result["called"], "echo");
        assert_eq!(result["args"]["extra"], "ok");
    }

    #[tokio::test]
    async fn test_js_sandbox_additional_properties_false_rejects() {
        let mut schema = echo_strict_schema();
        schema["additionalProperties"] = json!(false);
        let tool = make_tool_with_schema("echo", "Echo tool", schema);
        let reg = registry_with_single_tool(tool).await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("echo", { text: "hi", extra: "nope" });"#)
            .await;
        assert!(result.is_err(), "additionalProperties:false should reject");
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("'extra'"), "error lists unknown key: {}", err);
    }

    #[tokio::test]
    async fn test_js_sandbox_schema_without_properties_allows_arbitrary_args() {
        // `make_tool` defaults to `{"type": "object"}` with no `properties`
        // field — the validator must treat this as "accepts arbitrary args".
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("echo", { anything: 1, goes: true });"#)
            .await
            .expect("no properties → no rejection");
        assert_eq!(result["args"]["anything"], 1);
        assert_eq!(result["args"]["goes"], true);
    }

    #[tokio::test]
    async fn test_js_sandbox_unknown_param_rejected_via_tools_indexer() {
        // The indexer path (`tools["name"](args)`) returns the *raw* MCP
        // envelope, so a centralised `route_tool_call` validation failure
        // surfaces as an `isError: true` result rather than a thrown error
        // (unlike `call()`, which unwraps and throws). The schema rejection is
        // still reported with the offending key and tool name.
        let tool = make_tool_with_schema("echo", "Echo tool", echo_strict_schema());
        let reg = registry_with_single_tool(tool).await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return tools["echo"]({ text: "hi", bogus: 1 });"#)
            .await
            .expect("indexer returns the raw isError envelope, not a thrown error");
        assert_eq!(
            result["isError"], true,
            "indexer should surface the validation failure envelope: {result}"
        );
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("'bogus'"),
            "error lists unknown key: {}",
            text
        );
        assert!(text.contains("'echo'"), "error names tool: {}", text);
    }

    // --- suggest_tool_names unit tests ---

    #[test]
    fn test_suggest_tool_names_prefers_close_match() {
        let catalog = vec![
            make_tool("echo", "Echo tool"),
            make_tool("add", "Add numbers"),
            make_tool("greet", "Greeting tool"),
        ];
        let suggestions = suggest_tool_names("ehco", &catalog);
        assert!(
            suggestions.first().map(|s| s.as_str()) == Some("echo"),
            "expected 'echo' first, got {:?}",
            suggestions
        );
    }

    #[test]
    fn test_suggest_tool_names_matches_suffix_after_prefix() {
        let catalog = vec![
            make_tool("ep__echo", "Echo tool"),
            make_tool("ep__add", "Add numbers"),
        ];
        let suggestions = suggest_tool_names("ehco", &catalog);
        assert!(
            suggestions.iter().any(|s| s == "ep__echo"),
            "should match suffix after '__': {:?}",
            suggestions
        );
    }

    #[test]
    fn test_suggest_tool_names_returns_empty_for_total_mismatch() {
        let catalog = vec![make_tool("echo", "x"), make_tool("add", "y")];
        let suggestions = suggest_tool_names("zzzzzzzzzzzzzz", &catalog);
        assert!(
            suggestions.is_empty(),
            "very dissimilar query should yield no suggestions: {:?}",
            suggestions
        );
    }

    #[tokio::test]
    async fn test_js_sandbox_timeout() {
        let reg = make_registry().await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(10));
        let result = sandbox.execute("while(true) {}").await;
        assert!(result.is_err(), "infinite loop should be stopped");
        // boa's RuntimeLimits throws a JsError when the loop iteration limit is exceeded;
        // under heavy CI load the wallclock timeout may fire first instead.
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Maximum loop iteration limit")
                || err_msg.contains("loop")
                || err_msg.contains("timed out"),
            "error should indicate the infinite loop was stopped (loop limit or wallclock timeout): {}",
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

    // --- writeFile tests ---

    /// Build a sandbox whose `writeFile` allowlist is `roots`.
    async fn write_sandbox(roots: Vec<PathBuf>) -> JsSandbox {
        let reg = make_registry().await;
        JsSandbox::new(reg, Duration::from_secs(10)).with_write_roots(roots)
    }

    /// Canonicalized tempdir path, matching what `resolve_write_roots`
    /// produces for configured roots (on macOS `/var/...` → `/private/var/...`).
    fn canonical_root(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().unwrap()
    }

    /// Quote a Rust string as a JS string literal (handles NUL, quotes, …).
    fn js_quote(s: &str) -> String {
        serde_json::to_string(s).unwrap()
    }

    /// Sorted file names directly inside `dir`.
    fn dir_entries(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn test_write_file_utf8_returns_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        // Pass the possibly non-canonical tempdir path; the returned path
        // must be the canonical one and sit under the canonical root.
        let raw = dir.path().join("out.txt");
        let script = format!(
            "return writeFile({}, \"hello ✓ writeFile\");",
            js_quote(raw.to_str().unwrap())
        );
        let result = sandbox.execute(&script).await.unwrap();
        let expected = root.join("out.txt");
        assert_eq!(result, json!(expected.to_string_lossy()));
        assert!(PathBuf::from(result.as_str().unwrap()).starts_with(&root));
        assert_eq!(
            std::fs::read(&expected).unwrap(),
            "hello ✓ writeFile".as_bytes()
        );
    }

    #[tokio::test]
    async fn test_write_file_base64_decodes_bytes() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let bytes: Vec<u8> = (0u8..=255).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let dest = root.join("blob.bin");
        let script = format!(
            "return writeFile({}, {}, {{ encoding: \"base64\" }});",
            js_quote(dest.to_str().unwrap()),
            js_quote(&b64)
        );
        let result = sandbox.execute(&script).await.unwrap();
        assert_eq!(result, json!(dest.to_string_lossy()));
        assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    }

    #[tokio::test]
    async fn test_write_file_creates_nested_parents_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let dest = root.join("a/b/c/file.txt");
        let script = format!(
            "return writeFile({}, \"nested\");",
            js_quote(dest.to_str().unwrap())
        );
        sandbox.execute(&script).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"nested");
        assert!(root.join("a/b/c").is_dir());
    }

    #[tokio::test]
    async fn test_write_file_overwrite_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let dest = root.join("out.txt");
        for content in ["first", "second"] {
            let script = format!(
                "return writeFile({}, {});",
                js_quote(dest.to_str().unwrap()),
                js_quote(content)
            );
            sandbox.execute(&script).await.unwrap();
        }
        assert_eq!(std::fs::read(&dest).unwrap(), b"second");
        assert_eq!(dir_entries(&root), vec!["out.txt".to_string()]);
    }

    #[tokio::test]
    async fn test_write_file_second_allowlisted_root_writable() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let root_a = canonical_root(&dir_a);
        let root_b = canonical_root(&dir_b);
        let sandbox = write_sandbox(vec![root_a.clone(), root_b.clone()]).await;
        let dest = root_b.join("b.txt");
        let script = format!(
            "return writeFile({}, \"in b\");",
            js_quote(dest.to_str().unwrap())
        );
        let result = sandbox.execute(&script).await.unwrap();
        assert_eq!(result, json!(dest.to_string_lossy()));
        assert_eq!(std::fs::read(&dest).unwrap(), b"in b");
        assert!(dir_entries(&root_a).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_rejects_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let err = sandbox
            .execute(r#"return writeFile("relative/x.txt", "d");"#)
            .await
            .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("requires an absolute path"),
            "unexpected error: {}",
            msg
        );
        assert!(dir_entries(&root).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_rejects_dotdot_component() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let path = format!("{}/sub/../x.txt", root.display());
        let script = format!("return writeFile({}, \"d\");", js_quote(&path));
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("'..'"), "unexpected error: {}", msg);
        assert!(dir_entries(&root).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_rejects_root_prefix_string_trick() {
        // "/tmp/rootXsibling/…" must not match root "/tmp/rootX" — the
        // prefix check is component-wise, not a string prefix.
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let sibling = format!("{}sibling", root.display());
        let path = format!("{}/x.txt", sibling);
        let script = format!("return writeFile({}, \"d\");", js_quote(&path));
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("not inside a configured write directory"),
            "unexpected error: {}",
            msg
        );
        assert!(!PathBuf::from(&sibling).exists());
        assert!(dir_entries(&root).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_write_file_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        let path = root.join("link/x.txt");
        let script = format!(
            "return writeFile({}, \"d\");",
            js_quote(path.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("not inside a configured write directory"),
            "unexpected error: {}",
            msg
        );
        assert!(dir_entries(outside.path()).is_empty());
    }

    /// Simulates the validate→write race: `dest` is the canonical path
    /// `resolve_write_path` produced, but by the time `write_atomic` runs an
    /// intermediate component has been swapped for a symlink (out of the root,
    /// and — separately — to a directory inside it). The dirfd-anchored walk
    /// must reject the swap at every depth and create nothing anywhere: no
    /// parents, no temp file, no destination.
    #[cfg(unix)]
    #[test]
    fn test_write_atomic_rejects_intermediate_symlink_swap() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::create_dir_all(root.join("keep/deep")).unwrap();
        // "swapped" was validated as a real directory; now it is a link out.
        std::os::unix::fs::symlink(outside.path(), root.join("swapped")).unwrap();
        // A swap deeper in an existing chain.
        std::os::unix::fs::symlink(outside.path(), root.join("keep/deep/out")).unwrap();
        // A swap that stays inside the root is still a swap and still rejected.
        std::os::unix::fs::symlink(root.join("real"), root.join("inner")).unwrap();

        for rel in [
            "swapped/x.txt",
            "swapped/new/sub/x.txt",
            "keep/deep/out/x.txt",
            "keep/deep/out/new/x.txt",
            "inner/x.txt",
            "inner/new/x.txt",
        ] {
            let dest = root.join(rel);
            let err = write_atomic(&dest, b"d", &root).unwrap_err();
            assert!(
                err.contains("escaped the configured write directory during the write"),
                "{}: unexpected error: {}",
                rel,
                err
            );
        }
        assert!(dir_entries(outside.path()).is_empty());
        assert!(dir_entries(&root.join("real")).is_empty());
        assert_eq!(
            dir_entries(&root.join("keep/deep")),
            vec!["out".to_string()]
        );
        assert_eq!(
            dir_entries(&root),
            vec![
                "inner".to_string(),
                "keep".to_string(),
                "real".to_string(),
                "swapped".to_string()
            ]
        );

        // Sanity: a genuine (symlink-free) chain still gets created and written.
        let ok_dest = root.join("keep/deep/fresh/x.txt");
        write_atomic(&ok_dest, b"ok", &root).unwrap();
        assert_eq!(std::fs::read(&ok_dest).unwrap(), b"ok");
        assert_eq!(
            dir_entries(&root.join("keep/deep/fresh")),
            vec!["x.txt".to_string()]
        );
    }

    /// A root deleted after config resolution is not recreated (the anchor
    /// cannot be opened), and the error names the root and the config knob
    /// rather than blaming parent-directory creation. A root swapped for a
    /// symlink is still reported as an escape.
    #[cfg(unix)]
    #[test]
    fn test_write_atomic_reports_missing_root_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir).join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::remove_dir(&root).unwrap();

        let dest = root.join("a/b.txt");
        let err = write_atomic(&dest, b"d", &root).unwrap_err();
        assert!(
            err.contains(&format!(
                "configured write directory '{}' is not accessible",
                root.display()
            )) && err.contains("[relay] write_dirs"),
            "unexpected error: {}",
            err
        );
        assert!(
            !err.contains("failed to create parent directories"),
            "{}",
            err
        );
        assert!(!root.exists(), "root must not be recreated");

        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        let err = write_atomic(&dest, b"d", &root).unwrap_err();
        assert!(
            err.contains("escaped the configured write directory during the write"),
            "unexpected error: {}",
            err
        );
        assert!(dir_entries(outside.path()).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_rejects_empty_and_nul_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;

        let err = sandbox
            .execute(r#"return writeFile("", "d");"#)
            .await
            .unwrap_err();
        assert!(
            format!("{}", err).contains("must not be empty"),
            "unexpected error: {}",
            err
        );

        let path = format!("{}/x\u{0}.txt", root.display());
        let script = format!("return writeFile({}, \"d\");", js_quote(&path));
        let err = sandbox.execute(&script).await.unwrap_err();
        assert!(
            format!("{}", err).contains("NUL"),
            "unexpected error: {}",
            err
        );
        assert!(dir_entries(&root).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_rejects_path_under_no_root_lists_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let path = outside.path().join("x.txt");
        let script = format!(
            "return writeFile({}, \"d\");",
            js_quote(path.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("not inside a configured write directory"),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains("[relay] write_dirs in ~/.endara/config.toml"),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains("Settings → Write directories"),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains(&format!("Currently allowed: {}", root.display())),
            "unexpected error: {}",
            msg
        );
        assert!(dir_entries(outside.path()).is_empty());
        assert!(dir_entries(&root).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_rejects_when_no_roots_configured() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = write_sandbox(Vec::new()).await;
        let path = dir.path().join("x.txt");
        let script = format!(
            "return writeFile({}, \"d\");",
            js_quote(path.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("not inside a configured write directory"),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains("[relay] write_dirs in ~/.endara/config.toml"),
            "unexpected error: {}",
            msg
        );
        assert!(
            !msg.contains("Currently allowed"),
            "empty allowlist must not list directories: {}",
            msg
        );
        assert!(dir_entries(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_rejects_bad_encoding_and_bad_base64() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let dest = root.join("x.txt");

        let script = format!(
            "return writeFile({}, \"d\", {{ encoding: \"hex\" }});",
            js_quote(dest.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        assert!(
            format!("{}", err).contains("unsupported encoding"),
            "unexpected error: {}",
            err
        );

        let script = format!(
            "return writeFile({}, \"not base64!!\", {{ encoding: \"base64\" }});",
            js_quote(dest.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        assert!(
            format!("{}", err).contains("invalid base64"),
            "unexpected error: {}",
            err
        );
        assert!(dir_entries(&root).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_per_file_cap_one_byte_over_throws() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let dest = root.join("big.txt");
        // One byte over the 32 MB per-file cap.
        let script = format!(
            "return writeFile({}, \"a\".repeat({}));",
            js_quote(dest.to_str().unwrap()),
            32 * 1024 * 1024 + 1
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("exceeds the per-file limit"),
            "unexpected error: {}",
            msg
        );
        assert!(dir_entries(&root).is_empty(), "no partial file may remain");
    }

    #[tokio::test]
    async fn test_write_file_per_file_cap_base64_checked_before_decode() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let dest = root.join("big.bin");
        // '!' is not valid base64: if the payload were decoded first this
        // would fail with "invalid base64". The ≈3n/4 pre-decode estimate
        // must reject it as oversized instead.
        let repeat = (32usize * 1024 * 1024 / 3) * 4 + 8;
        let script = format!(
            "return writeFile({}, \"!\".repeat({}), {{ encoding: \"base64\" }});",
            js_quote(dest.to_str().unwrap()),
            repeat
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("exceeds the per-file limit"),
            "unexpected error: {}",
            msg
        );
        assert!(
            !msg.contains("invalid base64"),
            "size cap must fire before base64 decoding: {}",
            msg
        );
        assert!(dir_entries(&root).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_base64_exactly_at_cap_with_padding_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_write_limits(WriteLimits {
                max_file_bytes: 4,
                max_files_per_run: 64,
                max_total_bytes_per_run: 1024,
            });
        let dest = root.join("exact.bin");
        // "YWJjZA==" decodes to exactly 4 bytes ("abcd"). The naive 3n/4
        // estimate (6) would over-count the two padding bytes and reject a
        // payload that is exactly at the cap.
        let script = format!(
            "return writeFile({}, \"YWJjZA==\", {{ encoding: \"base64\" }});",
            js_quote(dest.to_str().unwrap())
        );
        let result = sandbox.execute(&script).await.unwrap();
        assert_eq!(result, json!(dest.to_str().unwrap()));
        assert_eq!(std::fs::read(&dest).unwrap(), b"abcd");
    }

    #[tokio::test]
    async fn test_write_file_rejects_non_string_data() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let dest = root.join("obj.txt");
        let script = format!(
            "return writeFile({}, {{ a: 1 }});",
            js_quote(dest.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("data must be a string"),
            "unexpected error: {}",
            msg
        );
        assert!(dir_entries(&root).is_empty());
    }

    #[tokio::test]
    async fn test_write_file_path_error_precedes_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        // Disallowed path AND invalid base64: the actionable path error
        // must win, and the payload must never be decoded.
        let script =
            "return writeFile(\"/definitely/not/allowed/x.bin\", \"!!!!\", { encoding: \"base64\" });";
        let err = sandbox.execute(script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("not inside a configured write directory"),
            "unexpected error: {}",
            msg
        );
        assert!(
            !msg.contains("invalid base64"),
            "path validation must precede base64 decoding: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_write_file_per_run_file_count_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let script = format!(
            "for (var i = 0; i < 65; i++) {{ writeFile({} + \"/f\" + i + \".txt\", \"x\"); }}",
            js_quote(root.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("per-run limit of 64 file writes"),
            "unexpected error: {}",
            msg
        );
        // The first 64 files remain intact; the 65th was never written.
        let entries = dir_entries(&root);
        assert_eq!(entries.len(), 64);
        for i in 0..64 {
            let f = root.join(format!("f{}.txt", i));
            assert_eq!(std::fs::read(&f).unwrap(), b"x", "missing {}", f.display());
        }
        assert!(!root.join("f64.txt").exists());
    }

    #[tokio::test]
    async fn test_write_file_per_run_total_bytes_limit_preexisting_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_write_limits(WriteLimits {
                max_file_bytes: 64,
                max_files_per_run: 64,
                max_total_bytes_per_run: 100,
            });
        // A pre-existing file the relay must never touch.
        let existing = root.join("existing.txt");
        std::fs::write(&existing, b"pre-existing").unwrap();
        // 40 + 40 = 80 bytes fit; the third 40-byte write would reach 120.
        let script = format!(
            r#"
var root = {};
writeFile(root + "/a.txt", "a".repeat(40));
writeFile(root + "/b.txt", "b".repeat(40));
writeFile(root + "/c.txt", "c".repeat(40));
"#,
            js_quote(root.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("per-run total write limit"),
            "unexpected error: {}",
            msg
        );
        assert_eq!(std::fs::read(&existing).unwrap(), b"pre-existing");
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), vec![b'a'; 40]);
        assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), vec![b'b'; 40]);
        assert!(!root.join("c.txt").exists());
    }

    #[tokio::test]
    async fn test_write_file_limits_reset_between_runs() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_write_limits(WriteLimits {
                max_file_bytes: 1024,
                max_files_per_run: 2,
                max_total_bytes_per_run: 100,
            });
        // Each run writes 2 files totalling 80 bytes — exactly at the
        // per-run file limit and near the byte limit. If the counters did
        // not reset between runs the second run would throw.
        for run in ["first", "second"] {
            let script = format!(
                r#"
var root = {};
writeFile(root + "/{run}-1.txt", "x".repeat(40));
writeFile(root + "/{run}-2.txt", "y".repeat(40));
return "ok";
"#,
                js_quote(root.to_str().unwrap())
            );
            let result = sandbox.execute(&script).await.unwrap();
            assert_eq!(result, json!("ok"), "run '{}' should succeed", run);
        }
        assert_eq!(dir_entries(&root).len(), 4);
    }

    // --- readFile tests ---

    #[tokio::test]
    async fn test_read_file_utf8_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let src = root.join("in.txt");
        std::fs::write(&src, "hello ✓ readFile").unwrap();
        let script = format!("return readFile({});", js_quote(src.to_str().unwrap()));
        let result = sandbox.execute(&script).await.unwrap();
        assert_eq!(result, json!("hello ✓ readFile"));
    }

    #[tokio::test]
    async fn test_read_file_base64_round_trips_binary_bytes() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let bytes: Vec<u8> = (0u8..=255).collect();
        let src = root.join("blob.bin");
        std::fs::write(&src, &bytes).unwrap();
        let script = format!(
            "return readFile({}, {{ encoding: \"base64\" }});",
            js_quote(src.to_str().unwrap())
        );
        let result = sandbox.execute(&script).await.unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(result.as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, bytes);
    }

    #[tokio::test]
    async fn test_read_file_rejects_path_outside_roots_lists_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let path = outside.path().join("x.txt");
        std::fs::write(&path, "secret").unwrap();
        let script = format!("return readFile({});", js_quote(path.to_str().unwrap()));
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("readFile: ") && msg.contains("not inside a configured write directory"),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains(
                "readFile is scoped to the same [relay] write_dirs allowlist as writeFile"
            ),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains("[relay] write_dirs in ~/.endara/config.toml"),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains("Settings → Write directories"),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains(&format!("Currently allowed: {}", root.display())),
            "unexpected error: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_write_file_then_read_file_round_trip_same_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let path = root.join("exports").join("report.json");
        let script = format!(
            r#"
var p = {};
var written = writeFile(p, JSON.stringify({{ ok: true, n: 3 }}));
return {{ written: written, back: JSON.parse(readFile(written)) }};
"#,
            js_quote(path.to_str().unwrap())
        );
        let result = sandbox.execute(&script).await.unwrap();
        assert_eq!(result["written"], json!(path.to_str().unwrap()));
        assert_eq!(result["back"], json!({ "ok": true, "n": 3 }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_file_allows_symlink_to_file_inside_same_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let target = root.join("real.txt");
        std::fs::write(&target, "via-link").unwrap();
        // A symlink whose target also resolves under the root is contained
        // and must be readable.
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let script = format!("return readFile({});", js_quote(link.to_str().unwrap()));
        let result = sandbox.execute(&script).await.unwrap();
        assert_eq!(result, json!("via-link"));
    }

    #[tokio::test]
    async fn test_read_file_rejects_when_no_roots_configured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.txt");
        std::fs::write(&path, "data").unwrap();
        let sandbox = write_sandbox(Vec::new()).await;
        let script = format!("return readFile({});", js_quote(path.to_str().unwrap()));
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("not inside a configured write directory"),
            "unexpected error: {}",
            msg
        );
        assert!(
            !msg.contains("Currently allowed"),
            "empty allowlist must not list directories: {}",
            msg
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_file_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        std::fs::write(outside.path().join("target.txt"), "s3cr3t-content").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        let path = root.join("link/target.txt");
        let script = format!("return readFile({});", js_quote(path.to_str().unwrap()));
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("not inside a configured write directory"),
            "unexpected error: {}",
            msg
        );
        assert!(
            !msg.contains("s3cr3t-content"),
            "content must not leak: {}",
            msg
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_file_rejects_file_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let target = outside.path().join("target.txt");
        std::fs::write(&target, "s3cr3t-content").unwrap();
        // The symlink itself lives inside the root, but the file it points
        // at does not — reading through it must be rejected.
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let script = format!("return readFile({});", js_quote(link.to_str().unwrap()));
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("not inside a configured write directory"),
            "unexpected error: {}",
            msg
        );
        assert!(
            !msg.contains("s3cr3t-content"),
            "content must not leak: {}",
            msg
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_file_rejects_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let fifo = root.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must be available on unix");
        assert!(status.success(), "mkfifo failed: {:?}", status);
        let script = format!("return readFile({});", js_quote(fifo.to_str().unwrap()));
        // With no writer attached, a blocking open of the FIFO would park the
        // sandbox thread forever — the rejection must happen before any read
        // is attempted. The path-level pre-check catches this case; the
        // handle-path test below covers a FIFO that reaches the open itself.
        let err = tokio::time::timeout(Duration::from_secs(5), sandbox.execute(&script))
            .await
            .expect("readFile on a FIFO must not block")
            .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("is not a regular file"),
            "unexpected error: {}",
            msg
        );
    }

    /// The handle path on its own: with `O_NONBLOCK` the open of a writer-less
    /// FIFO returns a handle instead of blocking, and it is the fstat on that
    /// handle — not a path-level check — that identifies it as non-regular.
    /// This is the property that closes the regular-file→FIFO swap window.
    #[cfg(unix)]
    #[test]
    fn test_open_for_read_fifo_returns_handle_and_fstat_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must be available on unix");
        assert!(status.success(), "mkfifo failed: {:?}", status);

        let root = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(open_for_read(&fifo, &root).map(|f| f.metadata()));
        });
        let opened = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("open of a writer-less FIFO must not block")
            .expect("O_NONBLOCK open of a FIFO must succeed");
        let meta = opened.expect("fstat on the FIFO handle must succeed");
        assert!(!meta.is_file(), "FIFO must not fstat as a regular file");
        assert!(!meta.is_dir());
        assert!(
            reject_non_regular("pipe", &meta).is_err(),
            "handle fstat must reject the FIFO"
        );
    }

    /// `O_NOFOLLOW` still rejects a symlink in the final component at open
    /// time — the swap-after-canonicalize case — and the `ELOOP` it returns
    /// is surfaced as a self-explanatory symlink message rather than the raw
    /// OS text, while a regular file opened with the same flags reads
    /// normally. (End to end this arm is only reachable via the swap race:
    /// `resolve_write_path` canonicalizes a pre-existing final-component
    /// symlink away before the open.)
    #[cfg(unix)]
    #[test]
    fn test_open_for_read_rejects_final_component_symlink() {
        use std::io::Read as _;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.txt");
        std::fs::write(&target, "plain").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = open_for_read(&link, dir.path()).expect_err("O_NOFOLLOW must refuse a symlink");
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ELOOP),
            "unexpected error: {}",
            err
        );
        let msg = open_error_message("/root/link.txt", &err);
        assert_eq!(
            msg,
            "readFile: '/root/link.txt' refers to a symbolic link or a path \
             containing a symbolic link loop, which cannot be read"
        );
        assert!(
            !msg.contains("Too many levels"),
            "raw ELOOP text must not leak: {}",
            msg
        );

        let mut file = open_for_read(&target, dir.path()).unwrap();
        assert!(file.metadata().unwrap().is_file());
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "plain");
    }

    /// The beneath-root open rejects a symlink swapped in for an
    /// *intermediate* component of the already-canonicalized path — the case
    /// a plain `open(2)` with `O_NOFOLLOW` would happily follow — whether the
    /// link points out of the root or back inside it, and surfaces it through
    /// the same friendly `ELOOP` message. A path that is not beneath the
    /// matched root at all is refused before any handle is opened. (End to
    /// end this arm is only reachable via the swap race: `resolve_write_path`
    /// canonicalizes pre-existing symlinks away before the open.)
    #[cfg(unix)]
    #[test]
    fn test_open_for_read_rejects_intermediate_symlink_swap() {
        use std::io::Read as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("f.txt"), "s3cr3t-content").unwrap();
        std::fs::create_dir_all(root.join("real/deep")).unwrap();
        std::fs::write(root.join("real/deep/f.txt"), "plain").unwrap();
        // Out-of-root swap at the first component.
        std::os::unix::fs::symlink(outside.path(), root.join("swapped")).unwrap();
        // Out-of-root swap at a middle component.
        std::os::unix::fs::symlink(outside.path(), root.join("real/out")).unwrap();
        // In-root swap: still a post-canonicalize change, still rejected.
        std::os::unix::fs::symlink(root.join("real"), root.join("inner")).unwrap();

        for rel in ["swapped/f.txt", "real/out/f.txt", "inner/deep/f.txt"] {
            let source = root.join(rel);
            let err = match open_for_read(&source, root) {
                Ok(_) => panic!("{}: intermediate symlink must be refused", rel),
                Err(e) => e,
            };
            assert_eq!(
                err.raw_os_error(),
                Some(libc::ELOOP),
                "{}: unexpected error: {}",
                rel,
                err
            );
            let msg = open_error_message(&format!("/root/{}", rel), &err);
            assert!(
                msg.contains("refers to a symbolic link or a path containing a symbolic link loop"),
                "{}: unexpected message: {}",
                rel,
                msg
            );
            assert!(
                !msg.contains("Too many levels") && !msg.contains("Not a directory"),
                "{}: raw OS text must not leak: {}",
                rel,
                msg
            );
            // The type pre-check is anchored the same way: it must not stat
            // through the swapped-in link either.
            let err = stat_for_read(&source, root)
                .expect_err("pre-check must refuse an intermediate symlink");
            assert_eq!(err.raw_os_error(), Some(libc::ELOOP), "{}: {}", rel, err);
        }

        // Not beneath the matched root at all: refused without opening.
        let err = open_for_read(&outside.path().join("f.txt"), root)
            .expect_err("a path outside the root must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{}", err);
        let err = stat_for_read(&outside.path().join("f.txt"), root)
            .expect_err("a path outside the root must not be statted");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{}", err);

        // The genuine nested path pre-checks as a regular file.
        let st = stat_for_read(&root.join("real/deep/f.txt"), root).unwrap();
        assert_eq!(st.st_mode & libc::S_IFMT, libc::S_IFREG);

        // Sanity: the genuine (symlink-free) nested path still reads.
        let mut file = open_for_read(&root.join("real/deep/f.txt"), root).unwrap();
        assert!(file.metadata().unwrap().is_file());
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "plain");
    }

    #[tokio::test]
    async fn test_read_file_rejects_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let err = sandbox
            .execute(r#"return readFile("relative/x.txt");"#)
            .await
            .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("readFile") && msg.contains("requires an absolute path"),
            "unexpected error: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_read_file_rejects_empty_and_nul_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;

        let err = sandbox
            .execute(r#"return readFile("");"#)
            .await
            .unwrap_err();
        assert!(
            format!("{}", err).contains("must not be empty"),
            "unexpected error: {}",
            err
        );

        let script = format!("return readFile({});", js_quote("/tmp/x\0y.txt"));
        let err = sandbox.execute(&script).await.unwrap_err();
        assert!(
            format!("{}", err).contains("NUL"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_read_file_rejects_unknown_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let src = root.join("x.txt");
        std::fs::write(&src, "data").unwrap();
        let script = format!(
            "return readFile({}, {{ encoding: \"hex\" }});",
            js_quote(src.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        assert!(
            format!("{}", err).contains("unsupported encoding"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_read_file_rejects_non_utf8_bytes_under_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let src = root.join("binary.bin");
        std::fs::write(&src, [0xff, 0xfe, 0x00, 0x41]).unwrap();
        let script = format!("return readFile({});", js_quote(src.to_str().unwrap()));
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("valid UTF-8"), "unexpected error: {}", msg);
        assert!(
            msg.contains("base64"),
            "error should suggest base64: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_read_file_per_file_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_read_limits(ReadLimits {
                max_file_bytes: 4,
                max_files_per_run: 64,
                max_total_bytes_per_run: 1024,
            });
        let src = root.join("big.txt");
        std::fs::write(&src, "12345").unwrap();
        let script = format!("return readFile({});", js_quote(src.to_str().unwrap()));
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("exceeds the per-file limit"),
            "unexpected error: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_read_file_per_run_count_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_read_limits(ReadLimits {
                max_file_bytes: 1024,
                max_files_per_run: 2,
                max_total_bytes_per_run: 1024,
            });
        for i in 0..3 {
            std::fs::write(root.join(format!("f{}.txt", i)), "x").unwrap();
        }
        let script = format!(
            r#"
var root = {};
readFile(root + "/f0.txt");
readFile(root + "/f1.txt");
readFile(root + "/f2.txt");
"#,
            js_quote(root.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("per-run limit of 2 file reads"),
            "unexpected error: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_read_file_per_run_total_bytes_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_read_limits(ReadLimits {
                max_file_bytes: 64,
                max_files_per_run: 64,
                max_total_bytes_per_run: 100,
            });
        // 40 + 40 = 80 bytes fit; the third 40-byte read would reach 120.
        for name in ["a", "b", "c"] {
            std::fs::write(root.join(format!("{}.txt", name)), vec![b'x'; 40]).unwrap();
        }
        let script = format!(
            r#"
var root = {};
readFile(root + "/a.txt");
readFile(root + "/b.txt");
readFile(root + "/c.txt");
"#,
            js_quote(root.to_str().unwrap())
        );
        let err = sandbox.execute(&script).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("per-run total") && msg.contains("read limit"),
            "unexpected error: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_read_file_limits_reset_between_runs() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_read_limits(ReadLimits {
                max_file_bytes: 1024,
                max_files_per_run: 2,
                max_total_bytes_per_run: 100,
            });
        std::fs::write(root.join("a.txt"), vec![b'x'; 40]).unwrap();
        std::fs::write(root.join("b.txt"), vec![b'y'; 40]).unwrap();
        // Each run reads 2 files totalling 80 bytes — exactly at the
        // per-run read count limit and near the byte limit. If the counters
        // did not reset between runs the second run would throw.
        for run in ["first", "second"] {
            let script = format!(
                r#"
var root = {};
readFile(root + "/a.txt");
readFile(root + "/b.txt");
return "ok";
"#,
                js_quote(root.to_str().unwrap())
            );
            let result = sandbox.execute(&script).await.unwrap();
            assert_eq!(result, json!("ok"), "run '{}' should succeed", run);
        }
    }

    #[tokio::test]
    async fn test_read_file_utf8_decode_failure_consumes_file_quota() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_read_limits(ReadLimits {
                max_file_bytes: 1024,
                max_files_per_run: 1,
                max_total_bytes_per_run: 1024,
            });
        std::fs::write(root.join("bin.dat"), [0xffu8, 0xfe, 0xfd]).unwrap();
        std::fs::write(root.join("ok.txt"), "fine").unwrap();
        // The first read pulls bytes from disk and only then fails to decode;
        // it must still count against the per-run file quota so the second
        // read is refused rather than succeeding.
        let script = format!(
            r#"
var root = {};
var first = null;
try {{ readFile(root + "/bin.dat"); }} catch (e) {{ first = String(e); }}
var second = null;
try {{ readFile(root + "/ok.txt"); }} catch (e) {{ second = String(e); }}
return {{ first: first, second: second }};
"#,
            js_quote(root.to_str().unwrap())
        );
        let result = sandbox.execute(&script).await.unwrap();
        let first = result["first"].as_str().unwrap_or_default();
        let second = result["second"].as_str().unwrap_or_default();
        assert!(
            first.contains("does not contain valid UTF-8"),
            "unexpected first error: {}",
            first
        );
        assert!(
            second.contains("per-run limit of 1 file reads"),
            "unexpected second error: {}",
            second
        );
    }

    #[test]
    fn test_read_bounded_rejects_growth_past_cap() {
        // Simulates a file whose size check passed but which grew before the
        // read: the reader yields more bytes than the allowance.
        // Exactly allowed + 1 bytes are pulled so the caller can account
        // for the I/O that happened even though the read is rejected.
        let mut grown = std::io::Cursor::new(vec![b'x'; 50]);
        let mut buf = Vec::new();
        assert!(read_bounded(&mut grown, 10, &mut buf).unwrap());
        assert_eq!(buf, vec![b'x'; 11]);

        let mut exact = std::io::Cursor::new(vec![b'x'; 10]);
        let mut buf = Vec::new();
        assert!(!read_bounded(&mut exact, 10, &mut buf).unwrap());
        assert_eq!(buf, vec![b'x'; 10]);

        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        let mut buf = Vec::new();
        assert!(!read_bounded(&mut empty, 0, &mut buf).unwrap());
        assert!(buf.is_empty());
        let mut one = std::io::Cursor::new(vec![b'x']);
        let mut buf = Vec::new();
        assert!(read_bounded(&mut one, 0, &mut buf).unwrap());
        assert_eq!(buf, vec![b'x']);
    }

    /// A reader that yields some data and then fails, like a network or FUSE
    /// filesystem returning partial content before an error.
    struct PartialThenError {
        remaining: usize,
    }

    impl std::io::Read for PartialThenError {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("link dropped"));
            }
            let n = self.remaining.min(out.len());
            out[..n].fill(b'p');
            self.remaining -= n;
            Ok(n)
        }
    }

    #[test]
    fn test_read_bounded_keeps_partial_bytes_on_io_error() {
        let mut reader = PartialThenError { remaining: 7 };
        let mut buf = Vec::new();
        let err = read_bounded(&mut reader, 100, &mut buf).unwrap_err();
        assert_eq!(err.to_string(), "link dropped");
        // The partial data is still in the buffer so the caller can charge
        // quota for the I/O that happened before the failure.
        assert_eq!(buf, vec![b'p'; 7]);
    }

    /// End-to-end check that a read failing with an I/O error still counts
    /// against the per-run file quota. On Linux `/proc/self/mem` is a regular
    /// file whose read at offset 0 fails with EIO (the address is unmapped),
    /// so it deterministically produces a real read error after a successful
    /// open. Reading one's own `/proc/self/mem` needs ptrace access to self;
    /// if the environment forbids it at `open`, there is no read-error seam
    /// to exercise and the test bails out early.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_read_file_io_error_consumes_file_quota() {
        let root = std::fs::canonicalize("/proc/self").unwrap();
        let mem = root.join("mem");
        if std::fs::File::open(&mem).is_err() {
            eprintln!("skipping: /proc/self/mem cannot be opened in this environment");
            return;
        }
        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_read_limits(ReadLimits {
                max_file_bytes: 64,
                max_files_per_run: 3,
                max_total_bytes_per_run: 1024,
            });
        let script = format!(
            r#"
var p = {};
var errors = [];
for (var i = 0; i < 4; i++) {{
  try {{ readFile(p); errors.push(null); }} catch (e) {{ errors.push(String(e)); }}
}}
return errors;
"#,
            js_quote(mem.to_str().unwrap())
        );
        let result = sandbox.execute(&script).await.unwrap();
        let errors = result.as_array().unwrap();
        assert_eq!(errors.len(), 4);
        for err in &errors[..3] {
            let msg = err.as_str().unwrap_or_default();
            assert!(
                msg.contains("readFile: failed to read"),
                "unexpected error: {}",
                msg
            );
        }
        let last = errors[3].as_str().unwrap_or_default();
        assert!(
            last.contains("per-run limit of 3 file reads"),
            "unexpected error: {}",
            last
        );
    }

    /// A file that yields more bytes than its `fstat` size claims stands in
    /// for one that grew between the size check and the read. On Linux,
    /// `/proc/<pid>/status` is a regular file reporting a length of 0 whose
    /// contents are non-empty, and `/proc/<pid>` canonicalizes stably, so it
    /// can act as a read root without any timing dependence.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_read_file_over_cap_bounded_read_consumes_quota() {
        let root = std::fs::canonicalize("/proc/self").unwrap();
        let status = root.join("status");
        let meta = std::fs::metadata(&status).unwrap();
        assert!(
            meta.is_file() && meta.len() == 0,
            "precondition: {:?}",
            meta
        );

        let sandbox = write_sandbox(vec![root.clone()])
            .await
            .with_read_limits(ReadLimits {
                max_file_bytes: 8,
                max_files_per_run: 3,
                max_total_bytes_per_run: 1024,
            });
        // Each over-cap read pulls 9 bytes and fails, but still counts as a
        // file read; the 4th attempt must be refused by the per-run count.
        let script = format!(
            r#"
var p = {};
var errors = [];
for (var i = 0; i < 4; i++) {{
  try {{ readFile(p); errors.push(null); }} catch (e) {{ errors.push(String(e)); }}
}}
return errors;
"#,
            js_quote(status.to_str().unwrap())
        );
        let result = sandbox.execute(&script).await.unwrap();
        let errors = result.as_array().unwrap();
        assert_eq!(errors.len(), 4);
        for err in &errors[..3] {
            let msg = err.as_str().unwrap_or_default();
            assert!(
                msg.contains("exceeds the remaining read allowance of 8 bytes"),
                "unexpected error: {}",
                msg
            );
        }
        let last = errors[3].as_str().unwrap_or_default();
        assert!(
            last.contains("per-run limit of 3 file reads"),
            "unexpected error: {}",
            last
        );
    }

    #[tokio::test]
    async fn test_read_file_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let path = root.join("missing.txt");
        let script = format!("return readFile({});", js_quote(path.to_str().unwrap()));
        let err = sandbox.execute(&script).await.unwrap_err();
        assert!(
            format!("{}", err).contains("does not exist"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_read_file_directory_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let sandbox = write_sandbox(vec![root.clone()]).await;
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        // A subdirectory and the allowlisted root itself (an empty relative
        // path beneath the anchor) both report "is a directory".
        for target in [&sub, &root] {
            let script = format!("return readFile({});", js_quote(target.to_str().unwrap()));
            let err = sandbox.execute(&script).await.unwrap_err();
            assert!(
                format!("{}", err).contains("is a directory"),
                "{}: unexpected error: {}",
                target.display(),
                err
            );
        }
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

    // ----------------------------------------------------------------------
    // Wave 3 — opt-in retry for read-only / idempotent tools
    // ----------------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock adapter that fails its first `failures_remaining` `call_tool`
    /// invocations with a configurable transient error, then succeeds.
    struct FlakyAdapter {
        tools: Vec<ToolInfo>,
        failures_remaining: AtomicUsize,
        call_count: Arc<AtomicUsize>,
        // When true, the failure is `HttpError { status: 503, .. }`; when
        // false, it's `AuthenticationRequired` (a non-transient error).
        transient: bool,
    }

    #[async_trait]
    impl McpAdapter for FlakyAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if self.failures_remaining.load(Ordering::SeqCst) > 0 {
                self.failures_remaining.fetch_sub(1, Ordering::SeqCst);
                return Err(if self.transient {
                    AdapterError::HttpError {
                        status: 503,
                        body: "service unavailable".into(),
                    }
                } else {
                    AdapterError::AuthenticationRequired {
                        endpoint: "ep".into(),
                        message: "token expired".into(),
                    }
                });
            }
            Ok(json!({ "called": name, "args": arguments }))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    fn tool_with_annotations(name: &str, annotations: Value) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: Some(format!("{} tool", name)),
            input_schema: json!({"type": "object"}),
            annotations: Some(annotations),
            ..Default::default()
        }
    }

    /// Build a registry with a single flaky adapter exposing one tool whose
    /// annotations the caller controls, plus a shared call-counter handle.
    async fn make_flaky_registry(
        tool: ToolInfo,
        failures: usize,
        transient: bool,
    ) -> (Arc<AdapterRegistry>, Arc<AtomicUsize>) {
        let registry = AdapterRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        registry
            .register(
                "ep".into(),
                Box::new(FlakyAdapter {
                    tools: vec![tool],
                    failures_remaining: AtomicUsize::new(failures),
                    call_count: Arc::clone(&counter),
                    transient,
                }),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        (Arc::new(registry), counter)
    }

    // --- jittered_backoff_ms -----------------------------------------------

    #[test]
    fn jittered_backoff_ms_within_bounds() {
        // For every base in the production schedule, 200 samples of
        // `jittered_backoff_ms` must land in `[floor(base*0.75), ceil(base*1.25)]`.
        // `0` is also covered explicitly to guard the zero-backoff override.
        let mut rng = rand::rng();
        let mut bases: Vec<u64> = RETRY_BACKOFF_SCHEDULE_MS.to_vec();
        bases.push(0);
        bases.push(50);
        for base in bases {
            let lo = (base as f64 * 0.75).floor() as u64;
            let hi = (base as f64 * 1.25).ceil() as u64;
            for _ in 0..200 {
                let j = jittered_backoff_ms(base, &mut rng);
                assert!(
                    j >= lo && j <= hi,
                    "jitter out of bounds: base={}, j={}, expected [{},{}]",
                    base,
                    j,
                    lo,
                    hi
                );
            }
        }
        // Zero base must be exactly zero — preserves the test-suite override.
        for _ in 0..50 {
            assert_eq!(jittered_backoff_ms(0, &mut rng), 0);
        }
    }

    // --- is_transient_error -------------------------------------------------

    #[test]
    fn is_transient_error_matches_known_substrings() {
        assert!(is_transient_error(&AdapterError::HttpError {
            status: 503,
            body: "x".into(),
        }));
        assert!(is_transient_error(&AdapterError::HttpError {
            status: 502,
            body: "x".into(),
        }));
        assert!(is_transient_error(&AdapterError::HttpError {
            status: 504,
            body: "x".into(),
        }));
        assert!(is_transient_error(&AdapterError::ProtocolError(
            "upstream request timeout exceeded".into()
        )));
        assert!(is_transient_error(&AdapterError::ConnectionFailed(
            "stream closed".into()
        )));
        assert!(is_transient_error(&AdapterError::ProtocolError(
            "connection reset by peer".into()
        )));
    }

    #[test]
    fn is_transient_error_rejects_unrelated_errors() {
        assert!(!is_transient_error(&AdapterError::NotInitialized));
        assert!(!is_transient_error(&AdapterError::ProtocolError(
            "schema mismatch".into()
        )));
        assert!(!is_transient_error(&AdapterError::JsonRpcError {
            code: -32000,
            message: "bad arg".into(),
            data: None,
        }));
    }

    // --- is_retry_eligible --------------------------------------------------

    #[test]
    fn is_retry_eligible_read_only_hint_allows() {
        let ann = json!({ "readOnlyHint": true });
        assert!(is_retry_eligible(Some(&ann)));
    }

    #[test]
    fn is_retry_eligible_idempotent_hint_allows() {
        let ann = json!({ "idempotentHint": true });
        assert!(is_retry_eligible(Some(&ann)));
    }

    #[test]
    fn is_retry_eligible_destructive_hint_overrides_idempotent() {
        let ann = json!({ "idempotentHint": true, "destructiveHint": true });
        assert!(!is_retry_eligible(Some(&ann)));
    }

    #[test]
    fn is_retry_eligible_no_annotations_rejects() {
        assert!(!is_retry_eligible(None));
        assert!(!is_retry_eligible(Some(&json!({}))));
        assert!(!is_retry_eligible(Some(&json!({ "title": "Echo" }))));
    }

    // --- end-to-end through the JS `call(..., { retry })` helper ------------

    #[tokio::test]
    async fn call_with_retry_recovers_from_transient_error() {
        // Read-only tool, 2 transient failures then success → 3 total calls,
        // and the JS layer sees a successful result.
        let tool = tool_with_annotations("echo", json!({ "readOnlyHint": true }));
        let (reg, counter) = make_flaky_registry(tool, 2, true).await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("echo", { text: "hi" }, { retry: 3 });"#)
            .await
            .unwrap();
        assert_eq!(result["called"], "echo");
        assert_eq!(result["args"]["text"], "hi");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "expected 1 initial + 2 retried call_tool invocations"
        );
    }

    #[tokio::test]
    async fn call_with_retry_blocks_non_eligible_tool() {
        // Tool has no annotations → eligibility gate must fail before any
        // adapter call_tool invocation happens, with a clear error message.
        let tool = make_tool("echo", "plain echo");
        let (reg, counter) = make_flaky_registry(tool, 0, true).await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("echo", {}, { retry: 3 });"#)
            .await;
        assert!(result.is_err(), "expected eligibility gate to reject");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("retry not allowed"),
            "error should explain retry was rejected, got: {}",
            err
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "no adapter call_tool calls should be made when retry is rejected"
        );
    }

    #[tokio::test]
    async fn call_with_retry_does_not_retry_non_transient_error() {
        // Read-only tool, but the failure is non-transient (auth) → loop
        // returns immediately after the first failure.
        let tool = tool_with_annotations("echo", json!({ "readOnlyHint": true }));
        let (reg, counter) = make_flaky_registry(tool, 5, false).await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("echo", {}, { retry: 3 });"#)
            .await;
        assert!(result.is_err(), "non-transient error must propagate");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "non-transient errors must not be retried"
        );
    }

    #[tokio::test]
    async fn call_with_retry_clamps_to_max_retries() {
        // Always-failing transient tool with retry: 999 must be clamped to
        // MAX_RETRIES (3) → 4 total calls and the final transient error.
        let tool = tool_with_annotations("echo", json!({ "idempotentHint": true }));
        let (reg, counter) = make_flaky_registry(tool, usize::MAX, true).await;
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("echo", {}, { retry: 999 });"#)
            .await;
        assert!(result.is_err(), "exhausted retries must surface the error");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1 + MAX_RETRIES,
            "should attempt 1 + MAX_RETRIES total calls"
        );
    }

    /// Adapter returning a fixed `isError: true` envelope as `Ok(value)` and
    /// counting `call_tool` invocations. Used to verify that application-level
    /// errors are surfaced on the first attempt without retry.
    struct IsErrorEnvelopeAdapter {
        tools: Vec<ToolInfo>,
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl McpAdapter for IsErrorEnvelopeAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, AdapterError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(json!({
                "isError": true,
                "content": [{ "type": "text", "text": "upstream broke" }]
            }))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn call_with_retry_does_not_retry_is_error_envelope() {
        // `isError: true` envelopes come back from `route_tool_call` as
        // `Ok(value)` — the retry loop only retries `Err`. The JS `call()`
        // helper turns the envelope into a thrown `Error` on first occurrence,
        // and the adapter must be invoked exactly once.
        let counter = Arc::new(AtomicUsize::new(0));
        let tool = tool_with_annotations("boom", json!({ "readOnlyHint": true }));
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(IsErrorEnvelopeAdapter {
                    tools: vec![tool],
                    call_count: Arc::clone(&counter),
                }),
                "stdio".into(),
                None,
                None,
            )
            .await;
        let reg = Arc::new(registry);
        let sandbox = JsSandbox::new(reg, Duration::from_secs(5));
        let result = sandbox
            .execute(r#"return call("boom", {}, { retry: 3 });"#)
            .await;
        assert!(result.is_err(), "isError envelope must throw in JS");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("upstream broke"),
            "error should include content text: {}",
            err
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "isError envelopes must not be retried, got {} calls",
            counter.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn call_with_retry_aborts_when_backoff_exceeds_budget() {
        // Real backoff schedule starts at 200ms; with a 50ms sandbox timeout,
        // the very first jittered sleep (>=150ms) is guaranteed to exceed the
        // remaining wall-clock budget. The retry loop must surface the
        // last-seen transient error rather than sleep into the outer timeout.
        let tool = tool_with_annotations("flaky", json!({ "readOnlyHint": true }));
        let (reg, counter) = make_flaky_registry(tool, usize::MAX, true).await;
        let sandbox = JsSandbox::new(reg, Duration::from_millis(50)).with_real_backoff();
        let started = std::time::Instant::now();
        let result = sandbox
            .execute(r#"return call("flaky", {}, { retry: 3 });"#)
            .await;
        let elapsed = started.elapsed();
        // Exactly one adapter call: first attempt fails transiently, the
        // deadline gate fires before any retry sleep, and no further calls
        // are issued.
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "deadline gate must abort before any retry attempt"
        );
        assert!(result.is_err(), "exhausted retry must surface as JS error");
        let err = format!("{}", result.unwrap_err());
        // Wrapped message must explicitly flag the deadline-exhaustion path,
        // name the tool, and embed the underlying transient error so callers
        // can distinguish a budget-exhausted retry from a single first-attempt
        // failure.
        assert!(
            err.contains("retry deadline exceeded"),
            "error should flag deadline exhaustion, got: {}",
            err
        );
        assert!(
            err.contains("call('flaky')"),
            "error should name the tool, got: {}",
            err
        );
        assert!(
            err.contains("503") || err.to_lowercase().contains("service"),
            "error should embed the underlying transient cause, got: {}",
            err
        );
        assert!(
            !err.to_lowercase().contains("script execution timed out"),
            "deadline gate must beat the sandbox timeout, got: {}",
            err
        );
        // Sanity check: must return well before the first real backoff
        // (>=150ms) would have completed — well within sandbox_timeout + 100ms.
        assert!(
            elapsed < Duration::from_millis(150),
            "retry loop should abort before first real sleep, took {:?}",
            elapsed
        );
    }

    /// R1.2: a sandbox-driven upstream tool call must re-establish the outer
    /// inbound request's caller identity across the `spawn_blocking` thread
    /// hop. [`block_on_with_request_context`] enters a fresh `request{client=...}` span
    /// before driving [`MetaToolRegistry::route_tool_call`], so the adapter's
    /// `current_request_context().client` (the same signal feeding the "Tool
    /// call completed/failed" log lines and `ToolCallEvent::Started.client`)
    /// resolves the aggregating client rather than `None`. This is the exact
    /// failure the task reproduced live before the fix.
    #[test]
    #[serial_test::serial(tracing)]
    fn sandbox_tool_call_reestablishes_client_context() {
        crate::test_tracing::init_permissive_tracing();
        use crate::events::{current_request_context, ClientIdentity, SpanFieldCaptureLayer};
        use std::sync::Mutex;
        use tracing_subscriber::prelude::*;

        // Adapter that records the caller identity visible via the request
        // span at `call_tool` time, proving the span was re-established.
        struct ClientCapturingAdapter {
            tools: Vec<ToolInfo>,
            seen: Arc<Mutex<Option<ClientIdentity>>>,
        }

        #[async_trait]
        impl McpAdapter for ClientCapturingAdapter {
            async fn initialize(&mut self) -> Result<(), AdapterError> {
                Ok(())
            }
            async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
                Ok(self.tools.clone())
            }
            async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
                *self.seen.lock().unwrap() = current_request_context().client;
                Ok(json!({ "called": name, "args": arguments }))
            }
            fn health(&self) -> HealthStatus {
                HealthStatus::Healthy
            }
            async fn shutdown(&mut self) -> Result<(), AdapterError> {
                Ok(())
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let seen_clone = Arc::clone(&seen);
        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let registry = rt.block_on(async {
                let registry = AdapterRegistry::new();
                registry
                    .register(
                        "ep".into(),
                        Box::new(ClientCapturingAdapter {
                            tools: vec![make_tool("echo", "Echo tool")],
                            seen: seen_clone,
                        }),
                        "stdio".into(),
                        None,
                        None,
                    )
                    .await;
                Arc::new(registry)
            });

            let identity = ClientIdentity {
                name: Some("Claude Desktop".into()),
                version: Some("0.1.0".into()),
                user_agent: None,
                origin: None,
            };
            let client_json = serde_json::to_string(&identity).unwrap();
            let reg: Arc<dyn MetaToolRegistry> = registry;
            let result = block_on_with_request_context(
                rt.handle(),
                &client_json,
                "",
                reg.route_tool_call("echo", json!({})),
            );
            assert!(result.is_ok(), "tool call should succeed: {:?}", result);
        });

        let captured = seen.lock().unwrap().clone();
        assert_eq!(
            captured,
            Some(ClientIdentity {
                name: Some("Claude Desktop".into()),
                version: Some("0.1.0".into()),
                user_agent: None,
                origin: None,
            }),
            "sandbox-driven tool call must re-establish the caller identity \
             in the request context"
        );
    }

    /// A sandbox-driven upstream tool call must re-establish the outer inbound
    /// request's canonical `request_uid` across the `spawn_blocking` thread
    /// hop. [`block_on_with_request_context`] enters a fresh
    /// `request{request_uid=...}` span before driving
    /// [`MetaToolRegistry::route_tool_call`], so the adapter's
    /// `current_request_context().request_uid` (the signal feeding
    /// `ToolCallEvent::Started.request_uid` and the desktop's collision-free
    /// row/overlay key) resolves the outer UID rather than `None`.
    #[test]
    #[serial_test::serial(tracing)]
    fn sandbox_tool_call_reestablishes_request_uid() {
        crate::test_tracing::init_permissive_tracing();
        use crate::events::{current_request_context, SpanFieldCaptureLayer};
        use std::sync::Mutex;
        use tracing_subscriber::prelude::*;

        // Adapter that records the request UID visible via the request span at
        // `call_tool` time, proving the span was re-established.
        struct UidCapturingAdapter {
            tools: Vec<ToolInfo>,
            seen: Arc<Mutex<Option<String>>>,
        }

        #[async_trait]
        impl McpAdapter for UidCapturingAdapter {
            async fn initialize(&mut self) -> Result<(), AdapterError> {
                Ok(())
            }
            async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
                Ok(self.tools.clone())
            }
            async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
                *self.seen.lock().unwrap() = current_request_context().request_uid;
                Ok(json!({ "called": name, "args": arguments }))
            }
            fn health(&self) -> HealthStatus {
                HealthStatus::Healthy
            }
            async fn shutdown(&mut self) -> Result<(), AdapterError> {
                Ok(())
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let seen_clone = Arc::clone(&seen);
        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let registry = rt.block_on(async {
                let registry = AdapterRegistry::new();
                registry
                    .register(
                        "ep".into(),
                        Box::new(UidCapturingAdapter {
                            tools: vec![make_tool("echo", "Echo tool")],
                            seen: seen_clone,
                        }),
                        "stdio".into(),
                        None,
                        None,
                    )
                    .await;
                Arc::new(registry)
            });

            let reg: Arc<dyn MetaToolRegistry> = registry;
            let result = block_on_with_request_context(
                rt.handle(),
                "",
                "req-uid-123",
                reg.route_tool_call("echo", json!({})),
            );
            assert!(result.is_ok(), "tool call should succeed: {:?}", result);
        });

        assert_eq!(
            *seen.lock().unwrap(),
            Some("req-uid-123".to_string()),
            "sandbox-driven tool call must re-establish the outer request_uid \
             in the request context"
        );
    }

    /// With no caller identity captured (empty `client_json`) and no UID (empty
    /// `request_uid`), [`block_on_with_request_context`] must skip the extra
    /// span and drive the future directly, leaving
    /// `current_request_context().client` as `None` rather than fabricating an
    /// empty identity.
    #[test]
    #[serial_test::serial(tracing)]
    fn sandbox_tool_call_without_client_leaves_context_none() {
        crate::test_tracing::init_permissive_tracing();
        use crate::events::{current_request_context, ClientIdentity, SpanFieldCaptureLayer};
        use std::sync::Mutex;
        use tracing_subscriber::prelude::*;

        struct ClientCapturingAdapter {
            tools: Vec<ToolInfo>,
            seen: Arc<Mutex<Option<ClientIdentity>>>,
        }

        #[async_trait]
        impl McpAdapter for ClientCapturingAdapter {
            async fn initialize(&mut self) -> Result<(), AdapterError> {
                Ok(())
            }
            async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
                Ok(self.tools.clone())
            }
            async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
                *self.seen.lock().unwrap() = current_request_context().client;
                Ok(json!({ "called": name, "args": arguments }))
            }
            fn health(&self) -> HealthStatus {
                HealthStatus::Healthy
            }
            async fn shutdown(&mut self) -> Result<(), AdapterError> {
                Ok(())
            }
        }

        let seen = Arc::new(Mutex::new(Some(ClientIdentity::default())));
        let seen_clone = Arc::clone(&seen);
        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let registry = rt.block_on(async {
                let registry = AdapterRegistry::new();
                registry
                    .register(
                        "ep".into(),
                        Box::new(ClientCapturingAdapter {
                            tools: vec![make_tool("echo", "Echo tool")],
                            seen: seen_clone,
                        }),
                        "stdio".into(),
                        None,
                        None,
                    )
                    .await;
                Arc::new(registry)
            });

            let reg: Arc<dyn MetaToolRegistry> = registry;
            let result = block_on_with_request_context(
                rt.handle(),
                "",
                "",
                reg.route_tool_call("echo", json!({})),
            );
            assert!(result.is_ok(), "tool call should succeed: {:?}", result);
        });

        assert_eq!(
            *seen.lock().unwrap(),
            None,
            "empty client_json must not fabricate a caller identity"
        );
    }
}
