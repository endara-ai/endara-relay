//! Server-type advertisement to connected models.
//!
//! Renders a deduplicated, alphabetised list of `server_type` values from
//! adapters currently in [`HealthStatus::Healthy`][crate::adapter::HealthStatus::Healthy]
//! state, and provides description builders for the meta-tools (`list_tools`,
//! `search_tools`, `execute_tools`) so each `tools/list` response reflects the
//! current registry. The same list also feeds `InitializeResult.instructions`.
//!
//! See `Engineering Spec — Advertise Connected Servers to the Model` (§3) for
//! the full design.

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
pub struct ServerTypeList<'a> {
    registry: &'a AdapterRegistry,
}

impl<'a> ServerTypeList<'a> {
    /// Bind a renderer to the given registry.
    pub fn new(registry: &'a AdapterRegistry) -> Self {
        Self { registry }
    }

    /// Returns `Some("a, b, c")` if at least one Healthy adapter has a
    /// `server_type`; `None` otherwise. The list is enforced under the
    /// 8KB safety cap; trailing entries are dropped and a `, …` suffix
    /// appended if the cap is exceeded.
    pub async fn render(&self) -> Option<String> {
        let types = self.registry.ready_server_types().await;
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

    /// Number of `Healthy` adapter instances (NOT deduplicated by type).
    pub async fn endpoint_count(&self) -> usize {
        self.registry.ready_endpoint_count().await
    }
}

/// Lead-in sentence prepended to the `Connected servers: …` line in
/// `InitializeResult.instructions`. Per Engineering Spec §3.2, the literal
/// blank line between the lead-in and the server list is part of the payload.
pub const INSTRUCTIONS_LEAD_IN: &str =
    "Endara Relay aggregates MCP servers behind a single endpoint.";

/// Build the `InitializeResult.instructions` string. Returns `None` when no
/// adapter is currently `Healthy` with a `server_type`, so the field is
/// omitted from the response (per spec §2.1).
pub async fn instructions(registry: &AdapterRegistry) -> Option<String> {
    ServerTypeList::new(registry)
        .render()
        .await
        .map(|list| format!("{}\n\nConnected servers: {}", INSTRUCTIONS_LEAD_IN, list))
}

/// Build the `search_tools` description. Appends `\n\nConnected servers: {list}`
/// when the registry has at least one Healthy adapter with a `server_type`.
pub async fn search_tools_description(registry: &AdapterRegistry) -> String {
    match ServerTypeList::new(registry).render().await {
        Some(list) => format!("{}\n\nConnected servers: {}", SEARCH_TOOLS_BASE, list),
        None => SEARCH_TOOLS_BASE.to_string(),
    }
}

/// Build the `list_tools` description. Appends `" {count} servers connected …"`
/// when at least one adapter is `Healthy`.
pub async fn list_tools_description(registry: &AdapterRegistry) -> String {
    let count = ServerTypeList::new(registry).endpoint_count().await;
    if count > 0 {
        format!(
            "{} {} servers connected via Endara Relay — use search_tools to discover tools.",
            LIST_TOOLS_BASE, count
        )
    } else {
        LIST_TOOLS_BASE.to_string()
    }
}

/// Build the `execute_tools` description. Same suffix as
/// [`list_tools_description`], appended to the long base block.
pub async fn execute_tools_description(registry: &AdapterRegistry) -> String {
    let count = ServerTypeList::new(registry).endpoint_count().await;
    if count > 0 {
        format!(
            "{} {} servers connected via Endara Relay — use search_tools to discover tools.",
            EXECUTE_TOOLS_BASE, count
        )
    } else {
        EXECUTE_TOOLS_BASE.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
    use async_trait::async_trait;
    use serde_json::Value;

    /// Minimal mock adapter — `health` and `server_type` are configurable; the
    /// rest is stubbed. Mirrors the pattern in `registry::tests::MockAdapter`.
    struct MockAdapter {
        health: HealthStatus,
        server_type_val: Option<String>,
    }

    impl MockAdapter {
        fn ready(server_type: &str) -> Self {
            Self {
                health: HealthStatus::Healthy,
                server_type_val: Some(server_type.to_string()),
            }
        }

        fn ready_no_type() -> Self {
            Self {
                health: HealthStatus::Healthy,
                server_type_val: None,
            }
        }

        fn failed() -> Self {
            Self {
                health: HealthStatus::Unhealthy("test".into()),
                server_type_val: Some("gmail".into()),
            }
        }

        fn starting() -> Self {
            Self {
                health: HealthStatus::Starting,
                server_type_val: Some("gmail".into()),
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
    async fn render_returns_none_when_no_healthy_adapters() {
        let reg = AdapterRegistry::new();
        assert!(ServerTypeList::new(&reg).render().await.is_none());
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 0);
    }

    #[tokio::test]
    async fn render_returns_none_when_only_failed_or_starting() {
        let reg = AdapterRegistry::new();
        register(&reg, "a", MockAdapter::failed()).await;
        register(&reg, "b", MockAdapter::starting()).await;
        assert!(ServerTypeList::new(&reg).render().await.is_none());
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 0);
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
        // endpoint_count counts all Healthy adapters regardless of server_type.
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 2);
    }

    #[tokio::test]
    async fn render_excludes_unhealthy_adapters() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("notion")).await;
        register(&reg, "ep2", MockAdapter::failed()).await;
        register(&reg, "ep3", MockAdapter::starting()).await;
        let list = ServerTypeList::new(&reg).render().await.unwrap();
        assert_eq!(list, "notion");
        assert_eq!(ServerTypeList::new(&reg).endpoint_count().await, 1);
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
        let desc = search_tools_description(&reg).await;
        assert_eq!(desc, SEARCH_TOOLS_BASE);
    }

    #[tokio::test]
    async fn search_tools_description_appends_connected_servers() {
        let reg = AdapterRegistry::new();
        register(&reg, "ep1", MockAdapter::ready("github")).await;
        register(&reg, "ep2", MockAdapter::ready("gmail")).await;
        let desc = search_tools_description(&reg).await;
        assert!(desc.starts_with(SEARCH_TOOLS_BASE));
        assert!(desc.ends_with("\n\nConnected servers: github, gmail"));
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
}
