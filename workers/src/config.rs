//! Runtime config, built and validated from the stored JSON document.

use crate::prefix;
use crate::store::{Crypto, Stored};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// Reserved first path segments under `/mcp/` (mirrors the local relay).
const RESERVED_PROFILE_PATHS: &[&str] = &["sse", "initialize", "tools", "oauth", "healthz"];

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub label: String,
    pub prefix: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub enabled: bool,
    pub oauth: bool,
    pub disabled_tools: HashSet<String>,
    /// Digest of everything that changes what the upstream returns. Cache
    /// keys include it, so an edit never serves a stale catalog.
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub path: String,
    pub endpoints: HashSet<String>,
    pub toon_output: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub revision: u64,
    pub machine_name: String,
    pub toon_output: bool,
    pub cache_ttl_secs: u64,
    pub request_timeout_secs: u64,
    /// Every endpoint, enabled or not; serving paths filter on `enabled`.
    pub endpoints: Vec<Endpoint>,
    pub profiles: Vec<Profile>,
}

pub fn fingerprint(name: &str, url: &str, headers: &[(String, String)], oauth: bool) -> String {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update([oauth as u8]);
    h.update([0]);
    h.update(url.as_bytes());
    for (k, v) in headers {
        h.update([0]);
        h.update(k.to_ascii_lowercase().as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
    }
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn validate_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) || url.len() < 10 {
        return Err(format!("URL must start with https:// (got '{url}')"));
    }
    Ok(())
}

impl Config {
    pub fn build(stored: &Stored, crypto: &Crypto) -> Result<Config, String> {
        let s = &stored.settings;
        if s.request_timeout_secs == 0 || s.request_timeout_secs > 300 {
            return Err("request timeout must be between 1 and 300 seconds".into());
        }
        if s.cache_ttl_secs > 86_400 {
            return Err("cache time must be at most 86400 seconds".into());
        }

        let mut endpoints = Vec::new();
        let mut names = HashSet::new();
        let mut prefixes = HashSet::new();
        for ep in &stored.endpoints {
            let name = ep.name.trim();
            if name.is_empty() {
                return Err("every server needs a name".into());
            }
            if !names.insert(name.to_string()) {
                return Err(format!("two servers are named '{name}'"));
            }
            validate_url(&ep.url)?;
            let prefix = ep
                .tool_prefix
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .or_else(|| prefix::sanitize_name(name))
                .ok_or_else(|| {
                    format!("'{name}' needs a tool prefix (its name has no usable characters)")
                })?;
            if prefix.contains("__") {
                return Err(format!("tool prefix '{prefix}' must not contain '__'"));
            }
            if ep.enabled && !prefixes.insert(prefix.clone()) {
                return Err(format!(
                    "two enabled servers use the tool prefix '{prefix}'"
                ));
            }
            let headers = ep
                .headers
                .iter()
                .map(|h| crypto.open(h).map(|v| (h.name.clone(), v)))
                .collect::<Result<Vec<_>, _>>()?;
            endpoints.push(Endpoint {
                id: ep.id.clone(),
                label: ep
                    .description
                    .clone()
                    .filter(|d| !d.trim().is_empty())
                    .unwrap_or_else(|| name.to_string()),
                fingerprint: fingerprint(name, &ep.url, &headers, ep.oauth),
                name: name.to_string(),
                prefix,
                url: ep.url.trim().to_string(),
                headers,
                enabled: ep.enabled,
                oauth: ep.oauth,
                disabled_tools: ep.disabled_tools.iter().cloned().collect(),
            });
        }

        let id_to_name: HashMap<&str, &str> = stored
            .endpoints
            .iter()
            .map(|e| (e.id.as_str(), e.name.trim()))
            .collect();
        let mut profiles = Vec::new();
        let mut paths = HashSet::new();
        for p in &stored.profiles {
            let path = p.path.trim();
            if path.is_empty()
                || RESERVED_PROFILE_PATHS.contains(&path)
                || !path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!(
                    "profile path '{path}' must use letters, digits, '-' or '_'"
                ));
            }
            if !paths.insert(path.to_string()) {
                return Err(format!("two profiles use the path '{path}'"));
            }
            profiles.push(Profile {
                path: path.to_string(),
                endpoints: p
                    .endpoint_ids
                    .iter()
                    .filter_map(|id| id_to_name.get(id.as_str()).map(|n| n.to_string()))
                    .collect(),
                toon_output: p.toon_output.unwrap_or(s.toon_output),
            });
        }

        Ok(Config {
            revision: stored.revision,
            machine_name: s.machine_name.trim().to_string(),
            toon_output: s.toon_output,
            cache_ttl_secs: s.cache_ttl_secs,
            request_timeout_secs: s.request_timeout_secs,
            endpoints,
            profiles,
        })
    }

    pub fn profile(&self, path: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.path == path)
    }
}
