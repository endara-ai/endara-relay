//! Edge config: the same TOML shape as the local relay, restricted to what a
//! Worker can serve. STDIO / SSE endpoints and OAuth-managed endpoints are kept
//! in `skipped` with a reason so `/healthz` can explain why they are absent.

use crate::prefix;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub const DEFAULT_CACHE_TTL_SECS: u64 = 300;
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    relay: RawRelay,
    #[serde(default)]
    endpoints: Vec<RawEndpoint>,
    #[serde(default)]
    profiles: Vec<RawProfile>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRelay {
    #[serde(default)]
    machine_name: Option<String>,
    #[serde(default)]
    toon_output: Option<bool>,
    #[serde(default)]
    cache_ttl_secs: Option<u64>,
    #[serde(default)]
    request_timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawEndpoint {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tool_prefix: Option<String>,
    transport: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    disabled_tools: Vec<String>,
    #[serde(default)]
    oauth_server_url: Option<String>,
    #[serde(default)]
    auth: Option<toml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawProfile {
    name: String,
    path: String,
    #[serde(default)]
    endpoints: Vec<String>,
    #[serde(default)]
    toon_output: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub name: String,
    pub label: String,
    pub prefix: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub disabled_tools: HashSet<String>,
    /// Stable digest of everything that changes what the upstream returns.
    /// Cache keys include it so a config edit never serves stale catalogs.
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub path: String,
    pub endpoints: HashSet<String>,
    pub toon_output: bool,
}

#[derive(Debug, Clone)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub machine_name: String,
    pub toon_output: bool,
    pub cache_ttl_secs: u64,
    pub request_timeout_secs: u64,
    pub endpoints: Vec<Endpoint>,
    pub profiles: Vec<Profile>,
    pub skipped: Vec<Skipped>,
}

/// Reserved first path segments under `/mcp/` (mirrors the local relay).
const RESERVED_PROFILE_PATHS: &[&str] = &["sse", "initialize", "tools", "oauth", "healthz"];

impl Config {
    /// Parse `raw` TOML. `lookup` resolves `$VAR` / `${VAR}` references in
    /// endpoint URLs and header values (Worker secrets and vars).
    pub fn parse(raw: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Config, String> {
        let parsed: RawConfig = toml::from_str(raw).map_err(|e| format!("invalid config: {e}"))?;

        let mut endpoints = Vec::new();
        let mut skipped = Vec::new();
        let mut seen_names = HashSet::new();
        let mut seen_prefixes = HashSet::new();

        for ep in parsed.endpoints {
            if !seen_names.insert(ep.name.clone()) {
                return Err(format!("duplicate endpoint name '{}'", ep.name));
            }
            if ep.disabled {
                continue;
            }
            let skip = |reason: String| Skipped {
                name: ep.name.clone(),
                reason,
            };
            match ep.transport.as_str() {
                "http" => {}
                "stdio" => {
                    skipped.push(skip(
                        "stdio servers need a local process; Workers cannot spawn processes".into(),
                    ));
                    continue;
                }
                "sse" => {
                    skipped.push(skip(
                        "legacy SSE transport is not supported on the edge; use streamable HTTP"
                            .into(),
                    ));
                    continue;
                }
                other => {
                    skipped.push(skip(format!("unknown transport '{other}'")));
                    continue;
                }
            }
            if ep.oauth_server_url.is_some() || ep.auth.is_some() {
                skipped.push(skip(
                    "OAuth-managed endpoints are not supported yet; pass a token via headers"
                        .into(),
                ));
                continue;
            }
            let Some(url) = ep.url.as_deref() else {
                skipped.push(skip("http endpoint has no url".into()));
                continue;
            };
            let url = match expand_vars(url, lookup) {
                Ok(u) => u,
                Err(e) => {
                    skipped.push(skip(e));
                    continue;
                }
            };
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                skipped.push(skip(format!("url must be http(s), got '{url}'")));
                continue;
            }
            let mut headers = Vec::new();
            let mut header_err = None;
            let mut names: Vec<_> = ep.headers.unwrap_or_default().into_iter().collect();
            names.sort();
            for (k, v) in names {
                match expand_vars(&v, lookup) {
                    Ok(v) => headers.push((k, v)),
                    Err(e) => {
                        header_err = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = header_err {
                skipped.push(skip(e));
                continue;
            }
            let Some(prefix) = ep
                .tool_prefix
                .clone()
                .or_else(|| prefix::sanitize_name(&ep.name))
            else {
                return Err(format!(
                    "endpoint '{}' needs a tool_prefix (name has no usable characters)",
                    ep.name
                ));
            };
            if !seen_prefixes.insert(prefix.clone()) {
                return Err(format!("duplicate tool prefix '{prefix}'"));
            }

            let mut hasher = Sha256::new();
            hasher.update(ep.name.as_bytes());
            hasher.update([0]);
            hasher.update(url.as_bytes());
            for (k, v) in &headers {
                hasher.update([0]);
                hasher.update(k.as_bytes());
                hasher.update([0]);
                hasher.update(v.as_bytes());
            }
            let digest = hasher.finalize();
            let fingerprint = digest[..8].iter().map(|b| format!("{b:02x}")).collect();

            endpoints.push(Endpoint {
                label: ep.description.clone().unwrap_or_else(|| ep.name.clone()),
                name: ep.name,
                prefix,
                url,
                headers,
                disabled_tools: ep.disabled_tools.into_iter().collect(),
                fingerprint,
            });
        }

        let global_toon = parsed.relay.toon_output.unwrap_or(false);
        let mut profiles = Vec::new();
        let mut seen_paths = HashSet::new();
        for p in parsed.profiles {
            if p.path.is_empty()
                || RESERVED_PROFILE_PATHS.contains(&p.path.as_str())
                || !p
                    .path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!("invalid profile path '{}'", p.path));
            }
            if !seen_paths.insert(p.path.clone()) {
                return Err(format!("duplicate profile path '{}'", p.path));
            }
            profiles.push(Profile {
                name: p.name,
                path: p.path,
                endpoints: p.endpoints.into_iter().collect(),
                toon_output: p.toon_output.unwrap_or(global_toon),
            });
        }

        Ok(Config {
            machine_name: parsed
                .relay
                .machine_name
                .unwrap_or_else(|| "cloudflare".to_string()),
            toon_output: global_toon,
            cache_ttl_secs: parsed
                .relay
                .cache_ttl_secs
                .unwrap_or(DEFAULT_CACHE_TTL_SECS),
            request_timeout_secs: parsed
                .relay
                .request_timeout_secs
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS)
                .max(1),
            endpoints,
            profiles,
            skipped,
        })
    }

    pub fn profile(&self, path: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.path == path)
    }
}

/// Expand `$NAME` and `${NAME}` from `lookup`. `$$` is a literal `$`.
fn expand_vars(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let name: String = match chars.peek() {
            Some((_, '$')) => {
                chars.next();
                out.push('$');
                continue;
            }
            Some((_, '{')) => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for (_, c) in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if !closed {
                    return Err(format!("unterminated '${{' in '{input}'"));
                }
                name
            }
            _ => {
                let mut name = String::new();
                while let Some((_, c)) = chars.peek() {
                    if c.is_ascii_alphanumeric() || *c == '_' {
                        name.push(*c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if name.is_empty() {
                    out.push('$');
                    continue;
                }
                name
            }
        };
        match lookup(&name) {
            Some(v) => out.push_str(&v),
            None => return Err(format!("missing secret or var '{name}'")),
        }
    }
    Ok(out)
}
