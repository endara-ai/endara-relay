//! MCP dispatch for the edge relay: aggregation, namespacing and routing.
//!
//! Naming and URI wrapping follow the local relay exactly (shared modules):
//! a lone endpoint passes names and URIs through untouched, otherwise tool and
//! prompt names get `<prefix>__` and resource URIs get the `mcp-relay://`
//! wrapper.

use crate::cache::Cache;
use crate::config::{Config, Endpoint};
use crate::upstream::{Upstream, UpstreamError};
use crate::{prefix, protocol, resource_uri, tool_call_rewrite, toon_convert};
use futures_util::future::join_all;
use serde_json::{json, Map, Value};

const MAX_LIST_PAGES: usize = 20;
const NEGATIVE_CACHE_SECS: u64 = 30;
const SERVER_NAME: &str = "Endara Relay (Cloudflare)";

const LIST_TOOLS_DESC: &str = "List every tool available through this relay, with descriptions and input schemas. Supports pagination via `limit` (default 50, max 200) and `offset`.";
const SEARCH_TOOLS_DESC: &str = "Search the relay's tools by keyword across names, descriptions and endpoint labels. Returns the best matches first (default 20, max 200).";

pub struct Scope<'a> {
    pub endpoints: Vec<&'a Endpoint>,
    pub toon: bool,
}

impl<'a> Scope<'a> {
    pub fn global(config: &'a Config) -> Self {
        Scope {
            endpoints: config.endpoints.iter().collect(),
            toon: config.toon_output,
        }
    }

    pub fn profile(config: &'a Config, path: &str) -> Option<Self> {
        let p = config.profile(path)?;
        Some(Scope {
            endpoints: config
                .endpoints
                .iter()
                .filter(|e| p.endpoints.contains(&e.name))
                .collect(),
            toon: p.toon_output,
        })
    }

    /// DD5: a single endpoint is served without prefixes or URI wrapping.
    fn passthrough(&self) -> bool {
        self.endpoints.len() <= 1
    }

    fn by_name(&self, name: &str) -> Option<&'a Endpoint> {
        self.endpoints.iter().copied().find(|e| e.name == name)
    }

    /// Resolve a client-facing tool/prompt name to `(endpoint, upstream name)`
    /// from the prefix alone, so calls never wait on a catalog fetch.
    fn resolve_name(&self, name: &str) -> Option<(&'a Endpoint, String)> {
        if self.passthrough() {
            return self.endpoints.first().map(|e| (*e, name.to_string()));
        }
        self.endpoints
            .iter()
            .copied()
            .filter_map(|e| {
                let rest = name.strip_prefix(&e.prefix)?.strip_prefix("__")?;
                (!rest.is_empty()).then(|| (e, rest.to_string()))
            })
            .max_by_key(|(e, _)| e.prefix.len())
    }
}

pub struct Relay<'a> {
    pub config: &'a Config,
    pub cache: &'a Cache,
}

pub enum Reply {
    Result(Value),
    Error {
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

impl From<UpstreamError> for Reply {
    fn from(e: UpstreamError) -> Self {
        match e {
            UpstreamError::Rpc {
                code,
                message,
                data,
            } => Reply::Error {
                code,
                message,
                data,
            },
            UpstreamError::Transport(message) => Reply::Error {
                code: -32603,
                message,
                data: None,
            },
        }
    }
}

fn invalid_params(message: impl Into<String>) -> Reply {
    Reply::Error {
        code: -32602,
        message: message.into(),
        data: None,
    }
}

impl<'a> Relay<'a> {
    fn upstream(&self, ep: &'a Endpoint) -> Upstream<'a> {
        Upstream::new(ep, self.cache, self.config.request_timeout_secs)
    }

    pub async fn dispatch(&self, scope: &Scope<'a>, method: &str, params: Value) -> Reply {
        match method {
            "initialize" => Reply::Result(self.initialize_result(scope, &params)),
            "ping" => Reply::Result(json!({})),
            "tools/list" => Reply::Result(json!({ "tools": self.tools_list(scope).await })),
            "tools/call" => self.tools_call(scope, params).await,
            "resources/list" => Reply::Result(json!({
                "resources": self.merged_list(scope, "resources/list", "resources", Some("uri"), false).await
            })),
            "resources/templates/list" => Reply::Result(json!({
                "resourceTemplates": self.merged_list(scope, "resources/templates/list", "resourceTemplates", Some("uriTemplate"), true).await
            })),
            "resources/read" => self.resources_read(scope, params).await,
            "prompts/list" => Reply::Result(json!({
                "prompts": self.merged_list(scope, "prompts/list", "prompts", None, false).await
            })),
            "prompts/get" => self.prompts_get(scope, params).await,
            "logging/setLevel" | "completion/complete" => Reply::Result(json!({})),
            _ => Reply::Error {
                code: -32601,
                message: format!("Method not found: {method}"),
                data: None,
            },
        }
    }

    fn initialize_result(&self, scope: &Scope<'a>, params: &Value) -> Value {
        let requested = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .and_then(protocol::ProtocolVersion::parse);
        let version = match requested {
            Some(v) if !v.is_2026() => v.as_str(),
            _ => protocol::VERSION_2025_03_26,
        };
        let mut result = json!({
            "protocolVersion": version,
            "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
        });
        if !scope.endpoints.is_empty() {
            let labels: Vec<&str> = scope.endpoints.iter().map(|e| e.label.as_str()).collect();
            result["instructions"] = Value::String(format!(
                "This relay ({}) aggregates these MCP servers: {}. Use search_tools to find a tool by keyword.",
                self.config.machine_name,
                labels.join(", ")
            ));
        }
        result
    }

    /// Fetch one endpoint's full list for `method`, following `nextCursor`.
    /// Successful lists are cached for `cache_ttl_secs`; failures briefly in
    /// memory so one dead upstream does not stall every listing.
    async fn endpoint_list(
        &self,
        ep: &'a Endpoint,
        method: &str,
        key: &str,
    ) -> Result<Vec<Value>, String> {
        let cache_key = format!("list:{}:{}:{}", method, ep.name, ep.fingerprint);
        if let Some(v) = self.cache.get(&cache_key).await {
            return match v {
                Value::Array(items) => Ok(items),
                Value::String(err) => Err(err),
                _ => Ok(Vec::new()),
            };
        }
        let upstream = self.upstream(ep);
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            match upstream.request(method, params).await {
                Ok(page) => {
                    if let Some(Value::Array(batch)) = page.get(key) {
                        items.extend(batch.iter().cloned());
                    }
                    cursor = page
                        .get("nextCursor")
                        .and_then(|c| c.as_str())
                        .map(str::to_string);
                    if cursor.is_none() {
                        break;
                    }
                }
                Err(e) if e.is_method_not_found() => break,
                Err(e) => {
                    let msg = e.message();
                    self.cache.put_local(
                        &cache_key,
                        Value::String(msg.clone()),
                        NEGATIVE_CACHE_SECS,
                    );
                    return Err(msg);
                }
            }
        }
        self.cache
            .put(
                &cache_key,
                Value::Array(items.clone()),
                self.config.cache_ttl_secs,
            )
            .await;
        Ok(items)
    }

    async fn tools_list(&self, scope: &Scope<'a>) -> Vec<Value> {
        let mut tools = self.catalog(scope).await;
        tools.push(json!({
            "name": "list_tools",
            "description": LIST_TOOLS_DESC,
            "inputSchema": { "type": "object", "properties": {
                "limit": { "type": "integer" }, "offset": { "type": "integer" } } }
        }));
        tools.push(json!({
            "name": "search_tools",
            "description": SEARCH_TOOLS_DESC,
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string" }, "limit": { "type": "integer" } },
                "required": ["query"] }
        }));
        tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        tools
    }

    /// Merged, namespaced tool catalog (without the meta-tools).
    async fn catalog(&self, scope: &Scope<'a>) -> Vec<Value> {
        let passthrough = scope.passthrough();
        let lists = join_all(
            scope
                .endpoints
                .iter()
                .map(|ep| self.endpoint_list(ep, "tools/list", "tools")),
        )
        .await;
        let mut tools = Vec::new();
        for (ep, list) in scope.endpoints.iter().zip(lists) {
            let Ok(list) = list else { continue };
            for mut tool in list {
                let Some(name) = tool
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                if ep.disabled_tools.contains(&name) {
                    continue;
                }
                let Some(obj) = tool.as_object_mut() else {
                    continue;
                };
                if !passthrough {
                    obj.insert(
                        "name".into(),
                        Value::String(prefix::encode_tool_name(&ep.prefix, None, &name)),
                    );
                    if let Some(Value::Object(meta)) = obj.get_mut("_meta") {
                        tool_call_rewrite::rewrite_meta_ui_pointers(meta, &ep.name);
                    }
                }
                let desc = match obj.get("description").and_then(|d| d.as_str()) {
                    Some(d) => format!("[{}] {}", ep.label, d),
                    None => format!("[{}]", ep.label),
                };
                obj.insert("description".into(), Value::String(desc));
                tools.push(tool);
            }
        }
        tools
    }

    async fn tools_call(&self, scope: &Scope<'a>, params: Value) -> Reply {
        let Some(name) = params
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::to_string)
        else {
            return invalid_params("missing 'name' in params");
        };
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        match name.as_str() {
            "list_tools" => {
                let catalog = self.catalog(scope).await;
                let total = catalog.len();
                let limit = arguments
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50)
                    .min(200) as usize;
                let offset = (arguments
                    .get("offset")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize)
                    .min(total);
                let page: Vec<Value> = catalog.iter().skip(offset).take(limit).map(slim).collect();
                return Reply::Result(meta_result(
                    json!({ "tools": page, "total": total, "limit": limit, "offset": offset }),
                    scope.toon,
                ));
            }
            "search_tools" => {
                let query = arguments
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let limit = arguments
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20)
                    .min(200) as usize;
                let catalog = self.catalog(scope).await;
                return Reply::Result(meta_result(
                    Value::Array(search(&catalog, query, limit)),
                    scope.toon,
                ));
            }
            _ => {}
        }

        let Some((ep, upstream_name)) = scope.resolve_name(&name) else {
            return Reply::Error {
                code: -32603,
                message: format!("no tool found for prefixed name '{name}'"),
                data: None,
            };
        };
        if ep.disabled_tools.contains(&upstream_name) {
            return Reply::Error {
                code: -32603,
                message: format!(
                    "tool '{upstream_name}' is disabled on endpoint '{}'",
                    ep.name
                ),
                data: None,
            };
        }
        // Forward 2026 round-trip siblings (`_meta`, `inputResponses`, …) verbatim.
        let mut forwarded: Map<String, Value> = match params {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        forwarded.insert("name".into(), Value::String(upstream_name));
        forwarded.insert("arguments".into(), arguments);

        match self
            .upstream(ep)
            .request("tools/call", Value::Object(forwarded))
            .await
        {
            Ok(result) => {
                let result = tool_call_rewrite::rewrite_tool_call_result(
                    result,
                    &ep.name,
                    scope.passthrough(),
                );
                let result = if scope.toon {
                    toon_convert::toonify_call_result(result)
                } else {
                    result
                };
                Reply::Result(result)
            }
            Err(e) => e.into(),
        }
    }

    async fn merged_list(
        &self,
        scope: &Scope<'a>,
        method: &str,
        key: &str,
        uri_field: Option<&str>,
        is_template: bool,
    ) -> Vec<Value> {
        let passthrough = scope.passthrough();
        let lists = join_all(
            scope
                .endpoints
                .iter()
                .map(|ep| self.endpoint_list(ep, method, key)),
        )
        .await;
        let mut merged = Vec::new();
        for (ep, list) in scope.endpoints.iter().zip(lists) {
            let Ok(list) = list else { continue };
            for mut item in list {
                if let Some(obj) = item.as_object_mut() {
                    match uri_field {
                        Some(field) => {
                            if let Some(uri) = obj.get(field).and_then(|v| v.as_str()) {
                                let wrapped = if is_template {
                                    resource_uri::maybe_encode_resource_uri_template(
                                        &ep.name,
                                        uri,
                                        passthrough,
                                    )
                                } else {
                                    resource_uri::maybe_encode_resource_uri(
                                        &ep.name,
                                        uri,
                                        passthrough,
                                    )
                                };
                                obj.insert(field.into(), Value::String(wrapped));
                            }
                        }
                        None if !passthrough => {
                            if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                                let encoded = prefix::encode_tool_name(&ep.prefix, None, name);
                                obj.insert("name".into(), Value::String(encoded));
                            }
                        }
                        None => {}
                    }
                }
                merged.push(item);
            }
        }
        merged
    }

    async fn resources_read(&self, scope: &Scope<'a>, params: Value) -> Reply {
        let Some(uri) = params.get("uri").and_then(|u| u.as_str()) else {
            return invalid_params("missing 'uri' in params");
        };
        let (hint, original) =
            match resource_uri::maybe_decode_resource_uri(uri, scope.passthrough()) {
                Ok(v) => v,
                Err(e) => return invalid_params(format!("invalid resource URI '{uri}': {e}")),
            };
        let ep = match hint {
            Some(name) => scope.by_name(&name),
            None => scope.endpoints.first().copied(),
        };
        let Some(ep) = ep else {
            return Reply::Error {
                code: -32603,
                message: format!("no endpoint can serve resource '{uri}'"),
                data: None,
            };
        };
        let mut forwarded = match params {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        forwarded.insert("uri".into(), Value::String(original));
        match self
            .upstream(ep)
            .request("resources/read", Value::Object(forwarded))
            .await
        {
            Ok(v) => Reply::Result(v),
            Err(e) => e.into(),
        }
    }

    async fn prompts_get(&self, scope: &Scope<'a>, params: Value) -> Reply {
        let Some(name) = params.get("name").and_then(|n| n.as_str()) else {
            return invalid_params("missing 'name' in params");
        };
        let Some((ep, upstream_name)) = scope.resolve_name(name) else {
            return Reply::Error {
                code: -32603,
                message: format!("no prompt found for name '{name}'"),
                data: None,
            };
        };
        let mut forwarded = match params {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        forwarded.insert("name".into(), Value::String(upstream_name));
        match self
            .upstream(ep)
            .request("prompts/get", Value::Object(forwarded))
            .await
        {
            Ok(v) => Reply::Result(tool_call_rewrite::rewrite_prompt_get_result(
                v,
                &ep.name,
                scope.passthrough(),
            )),
            Err(e) => e.into(),
        }
    }

    /// Per-endpoint reachability for `/healthz?probe=1`.
    pub async fn probe(&self) -> Vec<Value> {
        let results = join_all(
            self.config
                .endpoints
                .iter()
                .map(|ep| async move { (ep, self.upstream(ep).request("ping", json!({})).await) }),
        )
        .await;
        results
            .into_iter()
            .map(|(ep, r)| match r {
                Ok(_) => json!({ "name": ep.name, "status": "ok" }),
                Err(e) if e.is_method_not_found() => json!({ "name": ep.name, "status": "ok" }),
                Err(e) => json!({ "name": ep.name, "status": "error", "error": e.message() }),
            })
            .collect()
    }
}

fn slim(tool: &Value) -> Value {
    let mut out = json!({
        "name": tool.get("name").cloned().unwrap_or(Value::Null),
        "description": tool.get("description").cloned().unwrap_or(Value::Null),
        "input_schema": tool.get("inputSchema").cloned().unwrap_or(json!({})),
    });
    if let Some(a) = tool.get("annotations") {
        out["annotations"] = a.clone();
    }
    out
}

fn meta_result(value: Value, toon: bool) -> Value {
    let text = if toon {
        toon_convert::toonify_value(&value)
    } else {
        serde_json::to_string_pretty(&value).unwrap_or_default()
    };
    json!({ "content": [{ "type": "text", "text": text }] })
}

/// Token-scored keyword search: name hits outrank description hits, and
/// matching every query token earns a bonus.
fn search(catalog: &[Value], query: &str, limit: usize) -> Vec<Value> {
    let query = query.to_lowercase();
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return catalog.iter().take(limit).map(slim).collect();
    }
    let mut scored: Vec<(f64, &Value)> = catalog
        .iter()
        .filter_map(|tool| {
            let name = tool
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_lowercase();
            let desc = tool
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_lowercase();
            let name_parts: Vec<&str> = name.split(|c: char| !c.is_ascii_alphanumeric()).collect();
            let mut total = 0.0;
            let mut matched = 0;
            for t in &tokens {
                let score = if name_parts.iter().any(|p| p == t) {
                    4.0
                } else if name.contains(t) {
                    2.5
                } else if desc.split(|c: char| !c.is_alphanumeric()).any(|w| w == *t) {
                    1.5
                } else if desc.contains(t) {
                    1.0
                } else {
                    0.0
                };
                if score > 0.0 {
                    matched += 1;
                }
                total += score;
            }
            if matched == 0 {
                return None;
            }
            total += matched as f64;
            if matched == tokens.len() {
                total += 2.0;
            }
            Some((total, tool))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1["name"].as_str().cmp(&b.1["name"].as_str()))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, t)| slim(t))
        .collect()
}
