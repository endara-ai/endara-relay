//! Server-type advertisement to connected models.
//!
//! Renders a deduplicated, alphabetised list of `server_type` values across
//! **all** currently-registered adapters (regardless of [`HealthStatus`]) and
//! provides description builders for the meta-tools (`list_tools`,
//! `search_tools`, `execute_tools`) so each `tools/list` response reflects the
//! current registry. The same list also feeds `InitializeResult.instructions`.
//! Each adapter's rendered `server_type` is sourced from the cached upstream
//! handshake value when available and falls back to the configured
//! `server_type_override`, so endpoints surface immediately even before their
//! first successful `initialize`.
//!
//! See `Engineering Spec — Advertise Connected Servers to the Model` (§3) for
//! the full design.
//!
//! [`HealthStatus`]: crate::adapter::HealthStatus

use std::collections::HashSet;

use crate::profile_registry::ProfileRegistryView;
use crate::registry::AdapterRegistry;

/// Safety cap on the rendered server-type list. With deduplication a typical
/// deployment is well under 1KB, but the cap is retained as defence in depth
/// per the spec §2.4. When exceeded, trailing entries are truncated and a
/// `, …` suffix is appended.
const RENDER_BYTE_CAP: usize = 8 * 1024;

/// Base description for the `list_tools` meta-tool. Kept identical to the
/// literal in `server.rs` so behaviour is unchanged when no servers are
/// connected.
pub const LIST_TOOLS_BASE: &str = "List available tools with pagination. Returns `{ tools, total, limit, offset }`. Each tool has `name`, `description`, and `input_schema`. The `name` is the exact identifier to use when calling tools via `execute_tools`.";

/// Base description for the `search_tools` meta-tool. Kept identical to the
/// literal in `server.rs`.
pub const SEARCH_TOOLS_BASE: &str = "Fuzzy search across tool name, description, server endpoint, and input-schema property names. Typo-tolerant (Levenshtein), case-insensitive, and aware of camelCase / snake_case / kebab-case boundaries. Results are ranked by relevance (exact > prefix > substring > fuzzy; name > description > endpoint); tools matching more query tokens rank higher. Returns an array of matching tools, each with `name`, `description`, and `input_schema`.";

/// Base description for the `execute_tools` meta-tool. Kept identical to the
/// `concat!()` block in `server.rs`.
pub const EXECUTE_TOOLS_BASE: &str = concat!(
    "Execute a JavaScript snippet that can call tools. ",
    "Invoke a tool with `await call(\"tool_name\", { ...args })` — `call()` returns the unwrapped result directly, no manual envelope reading required. ",
    "Behind the scenes it returns `structuredContent` when the tool provides it, parses `content[0].text` when it begins with `[` or `{`, returns the text as-is otherwise, and throws an `Error` on `isError` envelopes (the message includes the tool name and `content[0].text`). ",
    "Multi-server tool names use `prefix__name` format (double underscore); single-server mode has no prefix. ",
    "Use `tools[\"tool_name\"](args)` only when you need the raw MCP envelope (`{ content, structuredContent, isError }`) — for example to inspect `isError` without throwing or to read the literal `content[0].text`. ",
    "Calling an unknown tool name throws an error that lists the closest matching tools. ",
    "Pass `{ retry: 3 }` as the third argument (e.g. `await call(\"name\", args, { retry: 3 })`) to retry transient errors on tools whose annotations declare `readOnlyHint` or `idempotentHint`. ",
    "Use `return` to send data back.\n\n",
    "Examples:\n",
    "```js\n",
    "// call() returns the unwrapped result — no need to read content[0].text yourself.\n",
    "const tasks = await call(\"todoist__get-tasks\", { limit: 5 });\n",
    "return tasks;\n",
    "```\n",
    "```js\n",
    "// Chain two tool calls and combine their results.\n",
    "const projects = await call(\"todoist__get-projects\", {});\n",
    "const tasks = await call(\"todoist__get-tasks\", { project_id: projects[0].id });\n",
    "return { projects, tasks };\n",
    "```\n",
    "```js\n",
    "// Opt into retry for read-only / idempotent tools.\n",
    "const issues = await call(\"github__list-issues\", { repo: \"endara-ai/relay\" }, { retry: 3 });\n",
    "return issues;\n",
    "```\n",
    "```js\n",
    "// Use the tools[...] indexer when you need the raw MCP envelope.\n",
    "const r = await tools[\"todoist__get-tasks\"]({ limit: 5 });\n",
    "return r.structuredContent;\n",
    "```",
);

/// Renders the deduplicated, sorted server-type list for a registry.
///
/// When `allowed_endpoints` is `Some`, the rendered list and the endpoint
/// count are restricted to adapters whose endpoint name is in the set —
/// this powers the `_for_profile` advertising variants. When `None`, the
/// renderer reflects every registered adapter (the global `/mcp` path).
pub struct ServerTypeList<'a> {
    registry: &'a AdapterRegistry,
    allowed_endpoints: Option<&'a HashSet<String>>,
}

impl<'a> ServerTypeList<'a> {
    /// Bind a renderer to the given registry (unfiltered — every
    /// registered adapter contributes).
    pub fn new(registry: &'a AdapterRegistry) -> Self {
        Self {
            registry,
            allowed_endpoints: None,
        }
    }

    /// Bind a renderer scoped to a profile's allowed endpoint set. Adapters
    /// whose endpoint name is not in `allowed_endpoints` are skipped in
    /// both [`Self::render`] and [`Self::endpoint_count`].
    pub fn for_profile(
        registry: &'a AdapterRegistry,
        allowed_endpoints: &'a HashSet<String>,
    ) -> Self {
        Self {
            registry,
            allowed_endpoints: Some(allowed_endpoints),
        }
    }

    /// Returns `Some("a, b, c")` if at least one in-scope adapter has a
    /// rendered `server_type` (cached upstream value or configured
    /// override); `None` otherwise. The list is enforced under the
    /// 8KB safety cap; trailing entries are dropped and a `, …` suffix
    /// appended if the cap is exceeded.
    pub async fn render(&self) -> Option<String> {
        let types = match self.allowed_endpoints {
            Some(allowed) => self.registry.server_types_in(allowed).await,
            None => self.registry.all_server_types().await,
        };
        if types.is_empty() {
            return None;
        }

        let mut out = String::new();
        const SUFFIX: &str = ", …";
        let cap_with_suffix = RENDER_BYTE_CAP.saturating_sub(SUFFIX.len());
        let mut truncated = false;

        for ty in &types {
            let sep_len = if out.is_empty() { 0 } else { 2 };
            // Reserve room for the "…" suffix so we can always append it
            // cleanly when truncation is needed.
            if out.len() + sep_len + ty.len() > cap_with_suffix {
                truncated = true;
                break;
            }
            if !out.is_empty() {
                out.push_str(", ");
            }
            out.push_str(ty);
        }

        if truncated {
            // If even the first entry didn't fit (extremely pathological),
            // emit just the suffix so callers can still detect overflow.
            out.push_str(SUFFIX);
        }
        Some(out)
    }

    /// Number of in-scope adapter instances regardless of health (NOT
    /// deduplicated by type). When scoped to a profile, only adapters
    /// whose endpoint name is in `allowed_endpoints` are counted.
    pub async fn endpoint_count(&self) -> usize {
        match self.allowed_endpoints {
            Some(allowed) => self.registry.endpoint_count_in(allowed).await,
            None => self.registry.all_endpoint_count().await,
        }
    }
}

/// Lead-in sentence prepended to the `Connected server types: …` line in
/// `InitializeResult.instructions`. Per Engineering Spec §3.2, the literal
/// blank line between the lead-in and the server list is part of the payload.
pub const INSTRUCTIONS_LEAD_IN: &str =
    "Endara Relay aggregates MCP servers behind a single endpoint.";

/// Shared body of [`instructions`] / [`instructions_for_profile`]. Takes a
/// pre-bound [`ServerTypeList`] so the same render logic powers the global
/// `/mcp` path and per-profile `/mcp/{profile}` advertising.
async fn build_instructions(list: ServerTypeList<'_>) -> Option<String> {
    let rendered = list.render().await;
    let count = list.endpoint_count().await;
    match (rendered, count) {
        (Some(rendered), _) => Some(format!(
            "{}\n\nConnected server types: {}",
            INSTRUCTIONS_LEAD_IN, rendered
        )),
        (None, 0) => None,
        (None, _) => Some(INSTRUCTIONS_LEAD_IN.to_string()),
    }
}

/// Build the `InitializeResult.instructions` string for the global `/mcp`
/// path (every registered adapter contributes).
///
/// - Returns `Some("{LEAD_IN}\n\nConnected server types: {list}")` whenever
///   the rendered list is non-empty.
/// - Returns `Some("{LEAD_IN}")` (just the lead-in, no `Connected server
///   types:` line) when the registry has at least one registered adapter but
///   no rendered server types.
/// - Returns `None` when the registry has zero registered adapters so the
///   field is omitted from the response.
pub async fn instructions(registry: &AdapterRegistry) -> Option<String> {
    build_instructions(ServerTypeList::new(registry)).await
}

/// Profile-scoped variant of [`instructions`]. Renders against the profile's
/// allowed endpoint set so per-profile `InitializeResult.instructions` only
/// advertises servers the profile is authorised to see — including the
/// fallback lead-in / `None` shapes that the global helper uses for the
/// zero-adapter case.
pub async fn instructions_for_profile(view: &ProfileRegistryView) -> Option<String> {
    build_instructions(ServerTypeList::for_profile(
        view.inner(),
        view.allowed_endpoints(),
    ))
    .await
}

/// Hint appended to the `search_tools` description when TOON output is
/// enabled. Tells the model that tool responses arrive in TOON (Token-Oriented
/// Object Notation) instead of JSON, so it doesn't try to `JSON.parse()` them.
pub const TOON_OUTPUT_HINT: &str = "Tool responses are returned in TOON format (Token-Oriented Object Notation) for reduced token usage. TOON is a compact alternative to JSON — indentation-based, no braces, tabular arrays. Parse it like structured text.";

/// Shared body of [`search_tools_description`] /
/// [`search_tools_description_for_profile`].
async fn build_search_tools_description(list: ServerTypeList<'_>, toon_enabled: bool) -> String {
    let base = match list.render().await {
        Some(rendered) => format!(
            "{}\n\nConnected server types: {}",
            SEARCH_TOOLS_BASE, rendered
        ),
        None => SEARCH_TOOLS_BASE.to_string(),
    };
    if toon_enabled {
        format!("{}\n\n{}", base, TOON_OUTPUT_HINT)
    } else {
        base
    }
}

/// Build the `search_tools` description. Appends `\n\nConnected server types: {list}`
/// when at least one registered adapter has a rendered `server_type`, and
/// the TOON hint when `toon_enabled` is true.
pub async fn search_tools_description(registry: &AdapterRegistry, toon_enabled: bool) -> String {
    build_search_tools_description(ServerTypeList::new(registry), toon_enabled).await
}

/// Profile-scoped variant of [`search_tools_description`]. Only adapters in
/// the profile's allowed endpoint set contribute to the appended
/// `Connected server types:` line; the TOON hint follows the per-profile
/// `toon_enabled`.
pub async fn search_tools_description_for_profile(
    view: &ProfileRegistryView,
    toon_enabled: bool,
) -> String {
    build_search_tools_description(
        ServerTypeList::for_profile(view.inner(), view.allowed_endpoints()),
        toon_enabled,
    )
    .await
}

/// Shared body of [`list_tools_description`] /
/// [`list_tools_description_for_profile`] and the matching `execute_tools`
/// helpers — appends `" {count} servers connected …"` to `base` when the
/// in-scope endpoint count is non-zero.
async fn build_count_suffix_description(list: ServerTypeList<'_>, base: &str) -> String {
    let count = list.endpoint_count().await;
    if count > 0 {
        format!(
            "{} {} servers connected via Endara Relay — use search_tools to discover tools.",
            base, count
        )
    } else {
        base.to_string()
    }
}

/// Build the `list_tools` description. Appends `" {count} servers connected …"`
/// when the registry has at least one registered adapter.
pub async fn list_tools_description(registry: &AdapterRegistry) -> String {
    build_count_suffix_description(ServerTypeList::new(registry), LIST_TOOLS_BASE).await
}

/// Profile-scoped variant of [`list_tools_description`] — the count reflects
/// only adapters in the profile's allowed endpoint set.
pub async fn list_tools_description_for_profile(view: &ProfileRegistryView) -> String {
    build_count_suffix_description(
        ServerTypeList::for_profile(view.inner(), view.allowed_endpoints()),
        LIST_TOOLS_BASE,
    )
    .await
}

/// Build the `execute_tools` description. Same suffix as
/// [`list_tools_description`], appended to the long base block.
pub async fn execute_tools_description(registry: &AdapterRegistry) -> String {
    build_count_suffix_description(ServerTypeList::new(registry), EXECUTE_TOOLS_BASE).await
}

/// Profile-scoped variant of [`execute_tools_description`] — the count
/// reflects only adapters in the profile's allowed endpoint set.
pub async fn execute_tools_description_for_profile(view: &ProfileRegistryView) -> String {
    build_count_suffix_description(
        ServerTypeList::for_profile(view.inner(), view.allowed_endpoints()),
        EXECUTE_TOOLS_BASE,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
    use async_trait::async_trait;
    use serde_json::Value;

    /// Minimal mock adapter — `health`, `server_type`, and
    /// `configured_server_type` are independently configurable; the rest is
    /// stubbed. Mirrors the pattern in `registry::tests::MockAdapter`.
    struct MockAdapter {
        health: HealthStatus,
        server_type_val: Option<String>,
        configured_val: Option<String>,
    }

    impl MockAdapter {
        fn ready(server_type: &str) -> Self {
            Self {
                health: HealthStatus::Healthy,
                server_type_val: Some(server_type.to_string()),
                configured_val: None,
            }
        }

        fn ready_no_type() -> Self {
            Self {
                health: HealthStatus::Healthy,
                server_type_val: None,
                configured_val: None,
            }
        }

        fn failed() -> Self {
            Self {
                health: HealthStatus::Unhealthy("test".into()),
                server_type_val: Some("gmail".into()),
                configured_val: None,
            }
        }

        fn starting() -> Self {
            Self {
                health: HealthStatus::Starting,
                server_type_val: Some("gmail".into()),
                configured_val: None,
            }
        }

        /// `Starting` adapter that has not yet captured an upstream
        /// `serverInfo.name` but does carry a `server_type_override` — used
        /// to exercise the override-only render path.
        fn starting_with_override(override_val: &str) -> Self {
            Self {
                health: HealthStatus::Starting,
                server_type_val: None,
                configured_val: Some(override_val.to_string()),
            }
        }

        /// Adapter with neither an upstream-captured `server_type` nor a
        /// configured override — exercises the "registered but renders
        /// nothing" path that drops the `Connected server types:` sub-line.
        fn starting_no_type() -> Self {
            Self {
                health: HealthStatus::Starting,
                server_type_val: None,
                configured_val: None,
            }
        }
    }

    #[async_trait]
    impl McpAdapter for MockAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(vec![])
        }
        async fn call_tool(&self, _name: &str, _args: Value) -> Result<Value, AdapterError> {
            Ok(Value::Null)
        }
        fn health(&self) -> HealthStatus {
            self.health.clone()
        }
        fn server_type(&self) -> Option<String> {
            self.server_type_val.clone()
        }
        fn configured_server_type(&self) -> Option<String> {
            self.configured_val.clone()
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    async fn register(reg: &AdapterRegistry, name: &str, adapter: MockAdapter) {
        reg.register(
            name.into(),
            Box::new(adapter),
            "stdio".into(),
            None,
            Some(name.into()),
        )
        .await;
    }

    #[tokio::test]
    async fn render_returns_none_when_registry_empty() {
        let reg = AdapterRegistry::new();
        assert!(ServerTypeList::new(&reg).render().await.is_none());
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 0);
    }

    #[tokio::test]
    async fn render_includes_failed_and_starting_adapters_with_known_types() {
        let reg = AdapterRegistry::new();
        register(&reg, "a", MockAdapter::failed()).await;
        register(&reg, "b", MockAdapter::starting()).await;
        // Both adapters have server_type = "gmail" → deduped to a single entry.
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        assert_eq!(list, "gmail");
        // endpoint_count covers all registered adapters regardless of health.
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 2);
    }

    #[tokio::test]
    async fn render_lists_single_healthy_adapter() {
        let reg = AdapterRegistry::new();
        register(&reg, "gmail-personal", MockAdapter::ready("gmail")).await;
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        assert_eq!(list, "gmail");
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 1);
    }

    #[tokio::test]
    async fn render_dedups_and_sorts_alphabetically() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("Gmail")).await;
        register(&reg, "ep2", MockAdapter::ready("gmail")).await;
        register(&reg, "ep3", MockAdapter::ready("notion")).await;
        register(&reg, "ep4", MockAdapter::ready("github")).await;
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        assert_eq!(list, "github, gmail, notion");
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 4);
    }

    #[tokio::test]
    async fn render_skips_adapters_without_server_type() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("notion")).await;
        register(&reg, "ep2", MockAdapter::ready_no_type()).await;
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        assert_eq!(list, "notion");
        // endpoint_count counts all registered adapters regardless of server_type.
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 2);
    }

    #[tokio::test]
    async fn render_includes_unhealthy_adapters_in_list_and_count() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("notion")).await;
        register(&reg, "ep2", MockAdapter::failed()).await;
        register(&reg, "ep3", MockAdapter::starting()).await;
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        // Failed and Starting both carry server_type = "gmail" → deduped.
        assert_eq!(list, "gmail, notion");
        // endpoint_count includes the non-Healthy adapters.
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 3);
    }

    /// New scenario: a `Failed` adapter that still has a cached upstream
    /// `server_type` from an earlier successful handshake is included.
    #[tokio::test]
    async fn render_includes_failed_adapter_with_cached_server_type() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep-healthy", MockAdapter::ready("notion")).await;
        // `MockAdapter::failed()` carries `server_type_val = Some("gmail")`,
        // matching an adapter that handshook successfully and later flipped to
        // Unhealthy without losing its cached upstream name.
        register(&reg, "ep-failed", MockAdapter::failed()).await;
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        assert_eq!(list, "gmail, notion");
    }

    /// New scenario: a `Starting` adapter with no upstream value yet but with
    /// a configured `server_type_override` renders via the override.
    #[tokio::test]
    async fn render_includes_starting_adapter_via_configured_override() {
        let reg = AdapterRegistry::new();
        register(
            &reg,
            "ep-pending",
            MockAdapter::starting_with_override("gmail"),
        )
        .await;
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        assert_eq!(list, "gmail");
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 1);
    }

    /// New scenario: the endpoint count includes non-Healthy adapters.
    #[tokio::test]
    async fn endpoint_count_includes_non_healthy_adapters() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("notion")).await;
        register(&reg, "ep2", MockAdapter::failed()).await;
        register(&reg, "ep3", MockAdapter::starting()).await;
        register(&reg, "ep4", MockAdapter::ready_no_type()).await;
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 4);
    }

    #[tokio::test]
    async fn render_truncates_above_byte_cap() {
        let reg = AdapterRegistry::new();
        // Each entry is ~70 bytes; ~150 entries pushes us above 8KB.
        for i in 0..200 {
            let st = format!("server-type-{:04}-padding-padding-padding-padding", i);
            register(&reg, &format!("ep{}", i), MockAdapter::ready(&st)).await;
        }
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        assert!(
            list.len() <= RENDER_BYTE_CAP,
            "rendered list exceeded byte cap: {} bytes",
            list.len()
        );
        assert!(
            list.ends_with(", \u{2026}"),
            "expected truncation suffix, got tail: {:?}",
            &list[list.len().saturating_sub(20)..]
        );
    }

    #[tokio::test]
    async fn search_tools_description_no_servers() {
        let reg = AdapterRegistry::new();
        let desc = search_tools_description(&reg, false).await;
        assert_eq!(desc, SEARCH_TOOLS_BASE);
    }

    #[tokio::test]
    async fn search_tools_description_appends_connected_servers() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("github")).await;
        register(&reg, "ep2", MockAdapter::ready("gmail")).await;
        let desc = search_tools_description(&reg, false).await;
        assert!(desc.starts_with(SEARCH_TOOLS_BASE));
        assert!(desc.ends_with("\n\nConnected server types: github, gmail"));
    }

    #[tokio::test]
    async fn search_tools_description_appends_toon_hint_when_enabled() {
        let reg = AdapterRegistry::new();
        let desc = search_tools_description(&reg, true).await;
        assert!(desc.starts_with(SEARCH_TOOLS_BASE));
        assert!(desc.ends_with(TOON_OUTPUT_HINT));
        assert!(desc.contains("TOON format"));
    }

    #[tokio::test]
    async fn search_tools_description_omits_toon_hint_when_disabled() {
        let reg = AdapterRegistry::new();
        let desc = search_tools_description(&reg, false).await;
        assert!(!desc.contains("TOON"));
    }

    #[tokio::test]
    async fn search_tools_description_combines_servers_and_toon_hint() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("github")).await;
        let desc = search_tools_description(&reg, true).await;
        assert!(desc.starts_with(SEARCH_TOOLS_BASE));
        assert!(desc.contains("Connected server types: github"));
        assert!(desc.ends_with(TOON_OUTPUT_HINT));
    }

    #[tokio::test]
    async fn list_tools_description_no_servers() {
        let reg = AdapterRegistry::new();
        let desc = list_tools_description(&reg).await;
        assert_eq!(desc, LIST_TOOLS_BASE);
    }

    #[tokio::test]
    async fn list_tools_description_appends_count() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("github")).await;
        register(&reg, "ep2", MockAdapter::ready("gmail")).await;
        register(&reg, "ep3", MockAdapter::ready("gmail")).await;
        let desc = list_tools_description(&reg).await;
        assert!(desc.starts_with(LIST_TOOLS_BASE));
        assert!(
            desc.ends_with(" 3 servers connected via Endara Relay \u{2014} use search_tools to discover tools."),
            "unexpected suffix: {}",
            desc
        );
    }

    #[tokio::test]
    async fn execute_tools_description_no_servers() {
        let reg = AdapterRegistry::new();
        let desc = execute_tools_description(&reg).await;
        assert_eq!(desc, EXECUTE_TOOLS_BASE);
    }

    #[tokio::test]
    async fn execute_tools_description_appends_count() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("github")).await;
        register(&reg, "ep2", MockAdapter::ready("gmail")).await;
        let desc = execute_tools_description(&reg).await;
        assert!(desc.starts_with(EXECUTE_TOOLS_BASE));
        assert!(
            desc.ends_with(" 2 servers connected via Endara Relay \u{2014} use search_tools to discover tools."),
            "unexpected suffix: {}",
            &desc[desc.len().saturating_sub(120)..]
        );
    }

    #[tokio::test]
    async fn instructions_none_when_registry_empty() {
        let reg = AdapterRegistry::new();
        assert!(instructions(&reg).await.is_none());
    }

    #[tokio::test]
    async fn instructions_lead_in_only_when_registered_but_no_types_render() {
        let reg = AdapterRegistry::new();
        // Registered adapter with neither an upstream type nor an override.
        register(&reg, "ep1", MockAdapter::starting_no_type()).await;
        let s = instructions(&reg).await.expect("instructions present");
        assert_eq!(s, INSTRUCTIONS_LEAD_IN);
        assert!(
            !s.contains("Connected server types:"),
            "no list expected when nothing renders: {s}"
        );
    }

    #[tokio::test]
    async fn instructions_full_form_when_types_render() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("github")).await;
        register(&reg, "ep2", MockAdapter::ready("gmail")).await;
        let s = instructions(&reg).await.expect("instructions present");
        assert_eq!(
            s,
            format!(
                "{}\n\nConnected server types: github, gmail",
                INSTRUCTIONS_LEAD_IN
            )
        );
    }

    /// 50 synthetic distinct server-types render well under 1KB — sanity-check
    /// the dedup/render path before the 8KB safety cap engages.
    #[tokio::test]
    async fn render_50_distinct_types_under_1024_bytes() {
        let reg = AdapterRegistry::new();
        for i in 0..50 {
            let ty = format!("type-{:02}", i);
            register(&reg, &format!("ep{}", i), MockAdapter::ready(&ty)).await;
        }
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        assert!(
            list.len() < 1024,
            "50 distinct types rendered to {} bytes, expected < 1024",
            list.len()
        );
        // Defence in depth: confirm we didn't truncate.
        assert!(
            !list.ends_with(", \u{2026}"),
            "50 types should fit without truncation: {}",
            list
        );
    }

    // ----------------------------------------------------------------------
    // Profile-aware advertising (R3.C, matrix row #15).
    //
    // The `_for_profile` variants render the same shapes as the global
    // builders but filter the server-type list and the endpoint count to
    // adapters in `allowed_endpoints`. The tests below pin both halves of
    // that contract: only the profile's types appear, the count reflects
    // only the profile's endpoints, and the empty-profile / TOON-hint
    // edge cases behave identically to the global helpers.
    // ----------------------------------------------------------------------

    /// Build a [`ProfileRegistryView`] scoped to `names` for the per-
    /// profile advertising tests. Mirrors how [`crate::profile_registry::
    /// ProfileRegistry::rebuild`] constructs views at runtime.
    fn view_for(reg: &AdapterRegistry, names: &[&str]) -> ProfileRegistryView {
        let allowed: HashSet<String> = names.iter().map(|s| (*s).to_string()).collect();
        ProfileRegistryView::new(reg.clone(), allowed)
    }

    /// Matrix row #15 — happy path. `instructions_for_profile` only mentions
    /// the profile's server types and uses the profile's endpoint count.
    #[tokio::test]
    async fn instructions_for_profile_filters_server_types_and_count() {
        let reg = AdapterRegistry::new();
        register(&reg, "gmail", MockAdapter::ready("gmail")).await;
        register(&reg, "linear", MockAdapter::ready("linear")).await;
        register(&reg, "todoist", MockAdapter::ready("todoist")).await;
        register(&reg, "github", MockAdapter::ready("github")).await;

        let view = view_for(&reg, &["gmail", "linear"]);
        let s = instructions_for_profile(&view)
            .await
            .expect("instructions present");
        assert_eq!(
            s,
            format!(
                "{}\n\nConnected server types: gmail, linear",
                INSTRUCTIONS_LEAD_IN
            )
        );
        // The unrelated endpoints must not leak into the rendered list.
        assert!(!s.contains("todoist"));
        assert!(!s.contains("github"));
    }

    /// An empty profile (no overlap with any registered endpoint) returns
    /// `None`, matching the global behaviour for an empty registry. This is
    /// the "Connected server types: " line guard: a misconfigured profile
    /// must not advertise the global server list.
    #[tokio::test]
    async fn instructions_for_profile_none_when_no_overlap() {
        let reg = AdapterRegistry::new();
        register(&reg, "gmail", MockAdapter::ready("gmail")).await;
        register(&reg, "linear", MockAdapter::ready("linear")).await;
        let view = view_for(&reg, &["does-not-exist"]);
        assert!(instructions_for_profile(&view).await.is_none());
    }

    /// A profile whose endpoints are all registered but none have a rendered
    /// `server_type` returns the lead-in only — same shape as the global
    /// `instructions_lead_in_only_when_registered_but_no_types_render` test.
    #[tokio::test]
    async fn instructions_for_profile_lead_in_only_when_in_scope_but_no_types() {
        let reg = AdapterRegistry::new();
        register(&reg, "pending", MockAdapter::starting_no_type()).await;
        let view = view_for(&reg, &["pending"]);
        let s = instructions_for_profile(&view)
            .await
            .expect("instructions present");
        assert_eq!(s, INSTRUCTIONS_LEAD_IN);
        assert!(!s.contains("Connected server types:"));
    }

    /// Matrix #15 (description side) — `list_tools_description_for_profile`
    /// uses the profile's endpoint count, not the registry-wide count.
    #[tokio::test]
    async fn list_tools_description_for_profile_counts_only_profile_endpoints() {
        let reg = AdapterRegistry::new();
        register(&reg, "gmail", MockAdapter::ready("gmail")).await;
        register(&reg, "linear", MockAdapter::ready("linear")).await;
        register(&reg, "todoist", MockAdapter::ready("todoist")).await;
        let view = view_for(&reg, &["gmail", "linear"]);
        let desc = list_tools_description_for_profile(&view).await;
        assert!(desc.starts_with(LIST_TOOLS_BASE));
        assert!(
            desc.ends_with(" 2 servers connected via Endara Relay \u{2014} use search_tools to discover tools."),
            "expected count = 2 for the 2-endpoint profile, got: {}",
            desc
        );
    }

    /// `search_tools_description_for_profile` filters the appended
    /// `Connected server types:` line and respects the per-profile TOON
    /// toggle exactly like the global helper.
    #[tokio::test]
    async fn search_tools_description_for_profile_filters_types_and_respects_toon() {
        let reg = AdapterRegistry::new();
        register(&reg, "gmail", MockAdapter::ready("gmail")).await;
        register(&reg, "todoist", MockAdapter::ready("todoist")).await;
        let view = view_for(&reg, &["gmail"]);

        let no_toon = search_tools_description_for_profile(&view, false).await;
        assert!(no_toon.starts_with(SEARCH_TOOLS_BASE));
        assert!(no_toon.contains("Connected server types: gmail"));
        assert!(!no_toon.contains("todoist"));
        assert!(!no_toon.contains("TOON"));

        let with_toon = search_tools_description_for_profile(&view, true).await;
        assert!(with_toon.contains("Connected server types: gmail"));
        assert!(!with_toon.contains("todoist"));
        assert!(with_toon.ends_with(TOON_OUTPUT_HINT));
    }

    /// `execute_tools_description_for_profile` mirrors the list-tools
    /// suffix on the profile's count.
    #[tokio::test]
    async fn execute_tools_description_for_profile_counts_only_profile_endpoints() {
        let reg = AdapterRegistry::new();
        register(&reg, "gmail", MockAdapter::ready("gmail")).await;
        register(&reg, "linear", MockAdapter::ready("linear")).await;
        register(&reg, "todoist", MockAdapter::ready("todoist")).await;
        let view = view_for(&reg, &["gmail"]);
        let desc = execute_tools_description_for_profile(&view).await;
        assert!(desc.starts_with(EXECUTE_TOOLS_BASE));
        assert!(
            desc.ends_with(" 1 servers connected via Endara Relay \u{2014} use search_tools to discover tools."),
            "expected count = 1 for the 1-endpoint profile, got tail: {}",
            &desc[desc.len().saturating_sub(120)..]
        );
    }

    /// Empty profile: descriptions fall back to the base text with no
    /// appended suffix — same as the global helper when the registry is
    /// empty. This is the regression guard against accidentally appending
    /// "0 servers connected …" to misconfigured profiles.
    #[tokio::test]
    async fn description_for_profile_no_overlap_returns_base() {
        let reg = AdapterRegistry::new();
        register(&reg, "gmail", MockAdapter::ready("gmail")).await;
        let view = view_for(&reg, &["nope"]);
        assert_eq!(
            list_tools_description_for_profile(&view).await,
            LIST_TOOLS_BASE
        );
        assert_eq!(
            execute_tools_description_for_profile(&view).await,
            EXECUTE_TOOLS_BASE
        );
        assert_eq!(
            search_tools_description_for_profile(&view, false).await,
            SEARCH_TOOLS_BASE
        );
    }
}
