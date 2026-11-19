//! Persistent relay config, edited through the web app.
//!
//! The config lives as one JSON document in KV. Header values (upstream
//! tokens) are AES-256-GCM encrypted with a key derived from the
//! `RELAY_ENCRYPTION_KEY` secret; the API only ever returns a masked preview.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::KvStore;

pub const CONFIG_KEY: &str = "config";

fn default_machine_name() -> String {
    "edge".into()
}
fn default_cache_ttl() -> u64 {
    300
}
fn default_timeout() -> u64 {
    30
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_machine_name")]
    pub machine_name: String,
    #[serde(default)]
    pub toon_output: bool,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_secs: u64,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            machine_name: default_machine_name(),
            toon_output: false,
            cache_ttl_secs: default_cache_ttl(),
            request_timeout_secs: default_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredHeader {
    pub name: String,
    /// base64(nonce || ciphertext)
    pub enc: String,
    /// Masked form shown in the UI, computed at encryption time.
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEndpoint {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tool_prefix: Option<String>,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<StoredHeader>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredProfile {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub endpoint_ids: Vec<String>,
    #[serde(default)]
    pub toon_output: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stored {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub endpoints: Vec<StoredEndpoint>,
    #[serde(default)]
    pub profiles: Vec<StoredProfile>,
}

// ---- API view -------------------------------------------------------------

/// A header as the browser sees it. `value` is write-only: omit it to keep
/// the stored secret for the same endpoint id and header name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiHeader {
    pub name: String,
    #[serde(default, skip_serializing)]
    pub value: Option<String>,
    #[serde(default, skip_deserializing)]
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiEndpoint {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tool_prefix: Option<String>,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<ApiHeader>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub endpoints: Vec<ApiEndpoint>,
    #[serde(default)]
    pub profiles: Vec<StoredProfile>,
}

impl Stored {
    pub fn to_api(&self) -> ApiConfig {
        ApiConfig {
            revision: self.revision,
            settings: self.settings.clone(),
            endpoints: self
                .endpoints
                .iter()
                .map(|e| ApiEndpoint {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    description: e.description.clone(),
                    tool_prefix: e.tool_prefix.clone(),
                    url: e.url.clone(),
                    headers: e
                        .headers
                        .iter()
                        .map(|h| ApiHeader {
                            name: h.name.clone(),
                            value: None,
                            preview: h.preview.clone(),
                        })
                        .collect(),
                    enabled: e.enabled,
                    disabled_tools: e.disabled_tools.clone(),
                })
                .collect(),
            profiles: self.profiles.clone(),
        }
    }

    pub fn endpoint(&self, id: &str) -> Option<&StoredEndpoint> {
        self.endpoints.iter().find(|e| e.id == id)
    }
}

/// Resolve write-only header values against `previous`: a header without a
/// value keeps the stored ciphertext for the same endpoint id and name.
pub fn resolve_headers(
    crypto: &Crypto,
    previous: Option<&StoredEndpoint>,
    endpoint_name: &str,
    headers: &[ApiHeader],
) -> Result<Vec<StoredHeader>, String> {
    let mut out = Vec::with_capacity(headers.len());
    for h in headers {
        let name = h.name.trim();
        if name.is_empty() {
            return Err(format!("'{endpoint_name}': a header has no name"));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            return Err(format!("'{endpoint_name}': invalid header name '{name}'"));
        }
        match &h.value {
            Some(v) => out.push(crypto.seal(name, v)?),
            None => {
                let kept = previous
                    .and_then(|p| p.headers.iter().find(|s| s.name.eq_ignore_ascii_case(name)))
                    .ok_or_else(|| format!("'{endpoint_name}': header '{name}' needs a value"))?;
                out.push(StoredHeader {
                    name: name.to_string(),
                    ..kept.clone()
                });
            }
        }
    }
    Ok(out)
}

pub fn new_id() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::getrandom(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `Bearer abcd…wxyz` → `Bearer ••••wxyz`; anything else → `••••wxyz`.
fn preview(value: &str) -> String {
    let (prefix, secret) = match value.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => ("Bearer ", rest),
        _ => ("", value),
    };
    let chars: Vec<char> = secret.chars().collect();
    let tail: String = if chars.len() > 8 {
        chars[chars.len() - 4..].iter().collect()
    } else {
        String::new()
    };
    format!("{prefix}••••{tail}")
}

pub struct Crypto {
    cipher: Aes256Gcm,
}

impl Crypto {
    pub fn new(secret: &str) -> Self {
        let mut h = Sha256::new();
        h.update(b"endara-relay/config/v1\0");
        h.update(secret.as_bytes());
        let key = h.finalize();
        Crypto {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
        }
    }

    pub fn seal(&self, name: &str, value: &str) -> Result<StoredHeader, String> {
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce).map_err(|e| e.to_string())?;
        let ct = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), value.as_bytes())
            .map_err(|_| "encryption failed".to_string())?;
        let mut blob = nonce.to_vec();
        blob.extend(ct);
        Ok(StoredHeader {
            name: name.to_string(),
            enc: B64.encode(blob),
            preview: preview(value),
        })
    }

    pub fn open(&self, header: &StoredHeader) -> Result<String, String> {
        let blob = B64
            .decode(&header.enc)
            .map_err(|_| "corrupt secret".to_string())?;
        if blob.len() < 12 {
            return Err("corrupt secret".into());
        }
        let (nonce, ct) = blob.split_at(12);
        let pt = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| {
                format!(
                    "cannot decrypt header '{}' (was RELAY_ENCRYPTION_KEY changed?)",
                    header.name
                )
            })?;
        String::from_utf8(pt).map_err(|_| "corrupt secret".to_string())
    }
}

pub async fn load(kv: &KvStore) -> Result<Stored, String> {
    // cache_ttl is KV's edge cache; the minimum keeps saves visible fast.
    match kv
        .get(CONFIG_KEY)
        .cache_ttl(30)
        .text()
        .await
        .map_err(|e| e.to_string())?
    {
        Some(text) => {
            serde_json::from_str(&text).map_err(|e| format!("stored config is invalid: {e}"))
        }
        None => Ok(Stored::default()),
    }
}

pub async fn save(kv: &KvStore, stored: &Stored) -> Result<(), String> {
    let body = serde_json::to_string(stored).map_err(|e| e.to_string())?;
    kv.put(CONFIG_KEY, body)
        .map_err(|e| e.to_string())?
        .execute()
        .await
        .map_err(|e| e.to_string())
}
