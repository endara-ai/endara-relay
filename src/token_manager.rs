use std::path::PathBuf;

/// Error type for token persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// A set of OAuth tokens for a single endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix timestamp (seconds) when the access token expires.
    pub expires_at: Option<u64>,
    /// Token type, typically "Bearer".
    pub token_type: String,
    /// Space-delimited scopes as returned by the authorization server.
    pub scope: Option<String>,
    /// Unix timestamp (seconds) when the token was issued.
    #[serde(default)]
    pub issued_at: Option<u64>,
}

impl TokenSet {
    /// Returns `true` if the token has not expired, using a 30-second buffer
    /// for clock skew. Tokens with no `expires_at` are assumed valid.
    pub fn is_valid(&self) -> bool {
        match self.expires_at {
            Some(exp) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                exp > now + 30 // 30-second buffer for clock skew
            }
            None => true, // No expiry = assume valid
        }
    }
}

/// A set of DCR (Dynamic Client Registration) credentials for a single endpoint.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DcrCredentials {
    pub client_id: String,
    pub client_secret: Option<String>,
    /// When the client_secret expires (0 = never), unix timestamp.
    pub client_secret_expires_at: u64,
    /// Unix timestamp (seconds) when the client was registered.
    pub registered_at: u64,
    /// Authorization server `issuer` (RFC 8414) these credentials were
    /// registered against. Bound so a stored DCR `client_id` is only reused
    /// with the SAME issuing AS; an issuer change triggers re-registration
    /// (RFC 7591). `None` for legacy credential files saved before this field
    /// existed (treated as "reuse as-is" for backward compatibility).
    #[serde(default)]
    pub issuer: Option<String>,
    /// Optional EMA **resource** client id used only at the MCP Authorization
    /// Server (Step 3, RFC 7523 ID-JAG redemption). Distinct from `client_id`,
    /// which is the *requesting* client used for SSO / the ID-JAG exchange at
    /// the IdP. Spec-compliant MASes need no resource credential; xaa.dev/Okta
    /// style MASes require this per-pairing credential. `None` (the common case)
    /// keeps Step 3 on the requesting client_id with no secret. This pair is
    /// **per-resource**, so R3 persists it on the *endpoint* DCR record
    /// (`{name}.dcr.json`, 0600) — captured via
    /// `POST /api/endpoints/{name}/credentials` — not the org record. For a
    /// resource-only EMA endpoint the record's `client_id` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_client_id: Option<String>,
    /// Optional EMA **resource** client secret paired with `resource_client_id`,
    /// presented via `client_secret_post` at the MAS in Step 3. Never sent on
    /// the IdP-facing legs and never substituted by the requesting secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_client_secret: Option<String>,
    /// Provenance marker: `true` only when the requesting `client_id` came
    /// from a dynamic client registration (RFC 7591) — i.e. the relay minted
    /// it against a discovered `registration_endpoint`. `false` for
    /// manually-supplied credentials (config.toml, `POST
    /// /api/endpoints/{name}/credentials`, or `POST
    /// /api/endpoints/{name}/oauth/credentials`) and for CIMD-resolved
    /// clients. Legacy files predating this field deserialize as `false`
    /// (conservative: never auto-discarded, same style as `issuer`).
    #[serde(default)]
    pub registered_via_dcr: bool,
}

/// Per-IdP credentials captured during EMA Step 1 (IdP SSO). Holds the ID Token
/// and IdP refresh token used to mint per-resource ID-JAG grants (RFC 8693).
///
/// Persisted via `TokenManager::{save_idp,load_idp,delete_idp}`, keyed by a
/// caller-supplied key (a sanitized IdP issuer in v1; END-19 re-keys to org).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct IdpCredentials {
    /// IdP issuer these credentials belong to, e.g. `https://acme.okta.com`.
    pub idp_issuer: String,
    /// OIDC ID Token from the IdP token response.
    pub id_token: String,
    /// IdP refresh token (present when `offline_access` was granted).
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix timestamp (seconds) when the ID Token expires.
    #[serde(default)]
    pub id_token_expires_at: Option<u64>,
    /// Unix timestamp (seconds) when these credentials were obtained.
    pub obtained_at: u64,
}

/// Sanitize a logical IdP key into a filesystem-safe filename stem. Issuer URLs
/// carry `/`, `:` and other characters unsafe for paths, so map anything that
/// isn't ASCII alphanumeric, `-`, or `_` to `_`. A simple character map can
/// collide distinct issuers (e.g. `a/b` and `a_b`), letting one IdP overwrite
/// another's `*.idp.json`. To make the stem collision-resistant we hash the key
/// with SHA-256 and encode it URL-safely (the alphabet `[A-Za-z0-9_-]` is
/// filename-safe). Deterministic so the same key always resolves to the same
/// `.idp.json` file; the original issuer is still stored inside the JSON.
/// END-19 swaps the key *source* (issuer → org) without touching this hashing.
fn sanitize_idp_key(key: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Decide whether persisted DCR credentials may be reused with the current
/// authorization server, or must be discarded and re-registered (RFC 7591).
///
/// Returns `true` to reuse, `false` to re-register. The decision is pure so it
/// can be unit-tested directly:
/// - stored issuer `None` (legacy creds) → reuse (backward compatibility)
/// - current issuer `None` (issuer unknown) → reuse (cannot detect a migration)
/// - both `Some` and equal → reuse
/// - both `Some` and differ → re-register (issuer migration)
pub fn dcr_issuer_allows_reuse(stored: Option<&str>, current: Option<&str>) -> bool {
    match (stored, current) {
        (Some(s), Some(c)) => s == c,
        _ => true,
    }
}

/// Compute the set-union of previously-granted scopes and the scopes that would
/// be requested today, for step-up authorization. Prior scopes are emitted
/// first (stable order), then any newly-requested scopes not already present;
/// duplicates are removed. Whitespace-delimited, per the OAuth `scope` syntax.
pub fn merge_scopes(prior: Option<&str>, requested: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for s in prior
        .unwrap_or("")
        .split_whitespace()
        .chain(requested.split_whitespace())
    {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out.join(" ")
}

/// Owns token persistence. One instance shared across all OAuth adapters via `Arc<TokenManager>`.
///
/// No in-memory caching — this is purely a persistence layer. The OAuthAdapter holds
/// its current access token in an `RwLock`.
pub struct TokenManager {
    token_dir: PathBuf,
    /// Serializes DCR read-modify-write operations (`save_dcr`, `delete_dcr`,
    /// `clear_dcr_requesting_client`) so a self-heal cannot interleave with a
    /// concurrent re-registration or manual credential update and either
    /// overwrite a newer record or be overwritten itself. Read-only paths
    /// (`load_dcr`) do not take this lock; they observe whichever
    /// atomically-renamed snapshot is on disk when they run.
    dcr_write_lock: tokio::sync::Mutex<()>,
}

impl TokenManager {
    pub fn new(token_dir: PathBuf) -> Self {
        Self {
            token_dir,
            dcr_write_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Save tokens for an endpoint. File written atomically (write to .tmp, rename).
    /// File permissions: 0600 on Unix.
    pub async fn save(&self, endpoint_name: &str, tokens: &TokenSet) -> Result<(), TokenError> {
        let path = self.token_dir.join(format!("{}.json", endpoint_name));
        let tmp_path = self.token_dir.join(format!(".{}.json.tmp", endpoint_name));
        let json = serde_json::to_string_pretty(tokens)?;
        tokio::fs::write(&tmp_path, json.as_bytes()).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    /// Load tokens for an endpoint. Returns None if file doesn't exist.
    pub async fn load(&self, endpoint_name: &str) -> Result<Option<TokenSet>, TokenError> {
        let path = self.token_dir.join(format!("{}.json", endpoint_name));
        match tokio::fs::read_to_string(&path).await {
            Ok(json) => Ok(Some(serde_json::from_str(&json)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(TokenError::Io(e)),
        }
    }

    /// Delete tokens for an endpoint.
    #[allow(dead_code)]
    pub async fn delete(&self, endpoint_name: &str) -> Result<(), TokenError> {
        let path = self.token_dir.join(format!("{}.json", endpoint_name));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TokenError::Io(e)),
        }
    }

    /// Save DCR credentials for an endpoint. File written atomically (write to .tmp, rename).
    /// File permissions: 0600 on Unix. Serialized against other DCR writers
    /// (`delete_dcr`, `clear_dcr_requesting_client`) via `dcr_write_lock` so
    /// a self-heal running concurrently cannot delete/overwrite a newer
    /// record persisted here.
    pub async fn save_dcr(
        &self,
        endpoint_name: &str,
        creds: &DcrCredentials,
    ) -> Result<(), TokenError> {
        let _guard = self.dcr_write_lock.lock().await;
        self.save_dcr_locked(endpoint_name, creds).await
    }

    /// Inner `save_dcr` body executed while the DCR write lock is held. Split
    /// out so `clear_dcr_requesting_client` can perform its read-modify-write
    /// (load → compare → save/delete) under a single lock acquisition.
    async fn save_dcr_locked(
        &self,
        endpoint_name: &str,
        creds: &DcrCredentials,
    ) -> Result<(), TokenError> {
        let path = self.token_dir.join(format!("{}.dcr.json", endpoint_name));
        let tmp_path = self
            .token_dir
            .join(format!(".{}.dcr.json.tmp", endpoint_name));
        let json = serde_json::to_string_pretty(creds)?;
        tokio::fs::write(&tmp_path, json.as_bytes()).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    /// Load DCR credentials for an endpoint. Returns None if file doesn't exist.
    pub async fn load_dcr(
        &self,
        endpoint_name: &str,
    ) -> Result<Option<DcrCredentials>, TokenError> {
        let path = self.token_dir.join(format!("{}.dcr.json", endpoint_name));
        match tokio::fs::read_to_string(&path).await {
            Ok(json) => Ok(Some(serde_json::from_str(&json)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(TokenError::Io(e)),
        }
    }

    /// Delete DCR credentials for an endpoint. No-op if file doesn't exist.
    /// Serialized against other DCR writers (`save_dcr`,
    /// `clear_dcr_requesting_client`) via `dcr_write_lock`.
    pub async fn delete_dcr(&self, endpoint_name: &str) -> Result<(), TokenError> {
        let _guard = self.dcr_write_lock.lock().await;
        self.delete_dcr_locked(endpoint_name).await
    }

    /// Atomically load-then-{save|delete} a DCR record for `endpoint_name`.
    /// The `update` closure receives the freshly-loaded record (or `None`
    /// when the file does not exist) and returns a decision:
    /// * `Ok(Some(creds))` — persist `creds` via an atomic write.
    /// * `Ok(None)` — remove the DCR file if it exists.
    /// * `Err(e)` — propagate to the caller without touching disk.
    ///
    /// The load and the subsequent write both happen while
    /// `dcr_write_lock` is held so no other writer (`save_dcr`,
    /// `delete_dcr`, `clear_dcr_requesting_client`, another `update_dcr`)
    /// can interleave a stale-write clobber between the read and the
    /// write. This is the helper `set_endpoint_credentials` uses to fold
    /// caller-supplied fields into the existing record without racing a
    /// concurrent `invalid_client` self-heal.
    ///
    /// Returning the loaded record unchanged is a true no-op: the save is
    /// skipped when the closure's `Some` output equals what was loaded, so
    /// "keep the existing record" never rewrites the file (no mtime churn,
    /// no redundant tmp-file rename).
    pub async fn update_dcr<F, E>(
        &self,
        endpoint_name: &str,
        update: F,
    ) -> Result<Option<DcrCredentials>, E>
    where
        F: FnOnce(Option<DcrCredentials>) -> Result<Option<DcrCredentials>, E>,
        E: From<TokenError>,
    {
        let _guard = self.dcr_write_lock.lock().await;
        let existing = self.load_dcr(endpoint_name).await?;
        match update(existing.clone())? {
            Some(creds) => {
                if existing.as_ref() != Some(&creds) {
                    self.save_dcr_locked(endpoint_name, &creds).await?;
                }
                Ok(Some(creds))
            }
            None => {
                self.delete_dcr_locked(endpoint_name).await?;
                Ok(None)
            }
        }
    }

    /// Inner `delete_dcr` body executed while the DCR write lock is held.
    async fn delete_dcr_locked(&self, endpoint_name: &str) -> Result<(), TokenError> {
        let path = self.token_dir.join(format!("{}.dcr.json", endpoint_name));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TokenError::Io(e)),
        }
    }

    /// Atomically invalidate the requesting `client_id`/`client_secret` pair
    /// on a DCR record iff the stored `client_id` equals `expected_client_id`.
    /// Used by the `invalid_client` self-heal paths (refresh grant + auth-code
    /// exchange): the AS's `invalid_client` response only proves that the
    /// requesting `client_id` we just presented is gone, so a concurrent
    /// re-registration that replaced the file with a NEWER `client_id` must
    /// survive this call. The optional `resource_client_id` /
    /// `resource_client_secret` pair is a distinct registration and is always
    /// preserved.
    ///
    /// The cleared record intentionally KEEPS `registered_via_dcr = true`
    /// even for pure-DCR (no resource pair) records: it is the auth-start
    /// resolution signal that this endpoint must re-register via RFC 7591
    /// rather than reuse the stale `client_id` still baked into
    /// `config.toml` by the setup commit. If the file were deleted (or
    /// `registered_via_dcr` cleared to `false`), the next interactive
    /// Authorize would fall through to the config-branch, POST the dead
    /// TOML `client_id`, and loop.
    ///
    /// Serialized against concurrent `save_dcr` / `delete_dcr` /
    /// `clear_dcr_requesting_client` via `dcr_write_lock`, so the
    /// load-then-write sequence is race-free with respect to a concurrent
    /// re-registration that replaced the record with a newer `client_id`.
    ///
    /// The provenance check is re-evaluated on the freshly-loaded record
    /// under the lock. Callers already gate on `registered_via_dcr`
    /// before invoking this method, but that check reads a snapshot taken
    /// outside the lock; between the caller's read and the lock
    /// acquisition here, an operator can rotate to manual credentials
    /// with the same `client_id` via `POST /api/endpoints/{name}/credentials`
    /// (which sets `registered_via_dcr = false`). Re-checking under the
    /// lock preserves the "manual credentials are never auto-discarded"
    /// promise even for that same-id rotation race.
    ///
    /// Returns:
    /// * `Ok(true)` when the requesting pair was cleared because
    ///   `expected_client_id` matched.
    /// * `Ok(false)` when the record was absent, is no longer
    ///   `registered_via_dcr`, OR the stored `client_id` did not match
    ///   (nothing was mutated).
    /// * `Err(_)` on IO / serialization failure.
    pub async fn clear_dcr_requesting_client(
        &self,
        endpoint_name: &str,
        expected_client_id: &str,
    ) -> Result<bool, TokenError> {
        let _guard = self.dcr_write_lock.lock().await;
        let Some(existing) = self.load_dcr(endpoint_name).await? else {
            return Ok(false);
        };
        if !existing.registered_via_dcr {
            // Same-id rotation to manual creds landed between the caller's
            // provenance check and this lock acquisition. Manually-supplied
            // credentials are never auto-discarded.
            return Ok(false);
        }
        if existing.client_id != expected_client_id {
            return Ok(false);
        }
        let cleared = DcrCredentials {
            client_id: String::new(),
            client_secret: None,
            client_secret_expires_at: 0,
            registered_at: existing.registered_at,
            issuer: None,
            resource_client_id: existing.resource_client_id,
            resource_client_secret: existing.resource_client_secret,
            // Preserve the DCR provenance signal so the next interactive
            // Authorize takes the RFC 7591 re-registration heal path.
            registered_via_dcr: existing.registered_via_dcr,
        };
        self.save_dcr_locked(endpoint_name, &cleared).await?;
        Ok(true)
    }

    /// Save IdP credentials under `key` (a sanitized IdP issuer in v1; END-19
    /// re-keys to org). File written atomically (write to .tmp, rename).
    /// File permissions: 0600 on Unix.
    pub async fn save_idp(&self, key: &str, creds: &IdpCredentials) -> Result<(), TokenError> {
        let stem = sanitize_idp_key(key);
        let path = self.token_dir.join(format!("{}.idp.json", stem));
        let tmp_path = self.token_dir.join(format!(".{}.idp.json.tmp", stem));
        let json = serde_json::to_string_pretty(creds)?;
        tokio::fs::write(&tmp_path, json.as_bytes()).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    /// Load IdP credentials for `key`. Returns None if file doesn't exist.
    pub async fn load_idp(&self, key: &str) -> Result<Option<IdpCredentials>, TokenError> {
        let stem = sanitize_idp_key(key);
        let path = self.token_dir.join(format!("{}.idp.json", stem));
        match tokio::fs::read_to_string(&path).await {
            Ok(json) => Ok(Some(serde_json::from_str(&json)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(TokenError::Io(e)),
        }
    }

    /// Delete IdP credentials for `key`. No-op if file doesn't exist.
    #[allow(dead_code)]
    pub async fn delete_idp(&self, key: &str) -> Result<(), TokenError> {
        let stem = sanitize_idp_key(key);
        let path = self.token_dir.join(format!("{}.idp.json", stem));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TokenError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_token_set() -> TokenSet {
        TokenSet {
            access_token: "test-access-token".to_string(),
            refresh_token: Some("test-refresh-token".to_string()),
            expires_at: Some(1700000000),
            token_type: "Bearer".to_string(),
            scope: Some("read write".to_string()),
            issued_at: Some(1699996400),
        }
    }

    #[tokio::test]
    async fn save_and_load_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let tokens = make_token_set();

        mgr.save("test-ep", &tokens).await.unwrap();
        let loaded = mgr.load("test-ep").await.unwrap().unwrap();
        assert_eq!(loaded.access_token, "test-access-token");
        assert_eq!(loaded.refresh_token.as_deref(), Some("test-refresh-token"));
        assert_eq!(loaded.expires_at, Some(1700000000));
        assert_eq!(loaded.token_type, "Bearer");
        assert_eq!(loaded.scope.as_deref(), Some("read write"));
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let result = mgr.load("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_existing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save("del-me", &make_token_set()).await.unwrap();
        assert!(mgr.load("del-me").await.unwrap().is_some());
        mgr.delete("del-me").await.unwrap();
        assert!(mgr.load("del-me").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.delete("nonexistent").await.unwrap(); // Should not error
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saved_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save("perm-test", &make_token_set()).await.unwrap();
        let path = tmp.path().join("perm-test.json");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "Expected 0600, got {:o}", mode & 0o777);
    }

    // --- TokenSet::is_valid() tests ---

    #[test]
    fn is_valid_with_future_expiry() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut ts = make_token_set();
        ts.expires_at = Some(now + 3600); // expires in 1 hour
        assert!(ts.is_valid());
    }

    #[test]
    fn is_valid_with_past_expiry() {
        let mut ts = make_token_set();
        ts.expires_at = Some(1000); // long expired
        assert!(!ts.is_valid());
    }

    #[test]
    fn is_valid_with_no_expiry() {
        let mut ts = make_token_set();
        ts.expires_at = None;
        assert!(
            ts.is_valid(),
            "Tokens with no expiry should be considered valid"
        );
    }

    #[test]
    fn is_valid_within_30s_buffer() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut ts = make_token_set();
        // Expires in 20 seconds — within the 30-second buffer, so should be invalid
        ts.expires_at = Some(now + 20);
        assert!(
            !ts.is_valid(),
            "Token expiring within 30s buffer should be invalid"
        );
    }

    #[test]
    fn is_valid_just_outside_buffer() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut ts = make_token_set();
        // Expires in 31 seconds — just outside the 30-second buffer
        ts.expires_at = Some(now + 31);
        assert!(
            ts.is_valid(),
            "Token expiring in 31s should be valid (outside 30s buffer)"
        );
    }

    #[test]
    fn is_valid_issued_at_field_present() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ts = TokenSet {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: Some(now + 3600),
            token_type: "Bearer".into(),
            scope: None,
            issued_at: Some(now),
        };
        assert!(ts.is_valid());
        assert_eq!(ts.issued_at, Some(now));
    }

    #[test]
    fn issued_at_defaults_to_none_on_deserialize() {
        // Tokens saved by Slice 1 (without issued_at) should deserialize with issued_at = None
        let json = r#"{"access_token":"tok","refresh_token":null,"expires_at":999999999999,"token_type":"Bearer","scope":null}"#;
        let ts: TokenSet = serde_json::from_str(json).unwrap();
        assert_eq!(ts.issued_at, None);
    }

    // --- DCR credential persistence tests ---

    fn make_dcr_creds() -> DcrCredentials {
        DcrCredentials {
            client_id: "dcr-client-id".to_string(),
            client_secret: Some("dcr-client-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 1700000000,
            issuer: Some("https://auth.example.com".to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn dcr_save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let creds = make_dcr_creds();

        mgr.save_dcr("test-ep", &creds).await.unwrap();
        let loaded = mgr.load_dcr("test-ep").await.unwrap().unwrap();
        assert_eq!(loaded, creds);
    }

    #[tokio::test]
    async fn dcr_load_nonexistent_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let result = mgr.load_dcr("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn dcr_delete_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_dcr("del-me", &make_dcr_creds()).await.unwrap();
        assert!(mgr.load_dcr("del-me").await.unwrap().is_some());
        mgr.delete_dcr("del-me").await.unwrap();
        assert!(mgr.load_dcr("del-me").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn dcr_delete_nonexistent_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        // Should not error on first or second call
        mgr.delete_dcr("nonexistent").await.unwrap();
        mgr.delete_dcr("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn clear_dcr_requesting_client_absent_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let cleared = mgr
            .clear_dcr_requesting_client("missing", "whatever")
            .await
            .unwrap();
        assert!(!cleared);
        assert!(mgr.load_dcr("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn clear_dcr_requesting_client_pure_dcr_retains_provenance_stub() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let creds = DcrCredentials {
            client_id: "dead-client".to_string(),
            client_secret: Some("dead-secret".to_string()),
            registered_via_dcr: true,
            ..make_dcr_creds()
        };
        mgr.save_dcr("ep", &creds).await.unwrap();

        let cleared = mgr
            .clear_dcr_requesting_client("ep", "dead-client")
            .await
            .unwrap();
        assert!(cleared);
        let loaded = mgr
            .load_dcr("ep")
            .await
            .unwrap()
            .expect("pure-DCR self-heal must retain a stub record so auth-start re-registers");
        assert_eq!(loaded.client_id, "");
        assert!(loaded.client_secret.is_none());
        assert!(loaded.issuer.is_none());
        assert!(
            loaded.registered_via_dcr,
            "registered_via_dcr must survive so auth-start prefers re-registration over the stale config.toml client_id"
        );
    }

    #[tokio::test]
    async fn clear_dcr_requesting_client_preserves_resource_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let creds = DcrCredentials {
            client_id: "dead-client".to_string(),
            client_secret: Some("dead-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 1_700_000_000,
            issuer: Some("https://as.example.com".to_string()),
            resource_client_id: Some("mas-resource".to_string()),
            resource_client_secret: Some("mas-resource-secret".to_string()),
            registered_via_dcr: true,
        };
        mgr.save_dcr("ep", &creds).await.unwrap();

        let cleared = mgr
            .clear_dcr_requesting_client("ep", "dead-client")
            .await
            .unwrap();
        assert!(cleared);

        let loaded = mgr.load_dcr("ep").await.unwrap().expect("record persists");
        assert_eq!(loaded.client_id, "");
        assert!(loaded.client_secret.is_none());
        assert!(
            loaded.registered_via_dcr,
            "mixed-record self-heal must retain the DCR provenance flag so auth-start prefers re-registration over the stale config.toml client_id"
        );
        assert!(loaded.issuer.is_none());
        assert_eq!(
            loaded.resource_client_id.as_deref(),
            Some("mas-resource"),
            "operator-set MAS resource client must survive requesting-pair clear"
        );
        assert_eq!(
            loaded.resource_client_secret.as_deref(),
            Some("mas-resource-secret")
        );
        assert_eq!(loaded.registered_at, 1_700_000_000);
    }

    #[tokio::test]
    async fn clear_dcr_requesting_client_mismatched_id_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        // Simulate a concurrent re-registration: the on-disk record now
        // holds a NEWER client_id than the caller's stale failing id.
        let newer = DcrCredentials {
            client_id: "fresh-client".to_string(),
            client_secret: Some("fresh-secret".to_string()),
            registered_via_dcr: true,
            ..make_dcr_creds()
        };
        mgr.save_dcr("ep", &newer).await.unwrap();

        let cleared = mgr
            .clear_dcr_requesting_client("ep", "stale-client")
            .await
            .unwrap();
        assert!(
            !cleared,
            "stale self-heal must not touch a concurrently re-registered record"
        );

        let loaded = mgr.load_dcr("ep").await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "fresh-client");
        assert_eq!(loaded.client_secret.as_deref(), Some("fresh-secret"));
        assert!(loaded.registered_via_dcr);
    }

    /// R3-1: an operator can rotate to manual credentials with the SAME
    /// `client_id` between the self-heal caller's provenance snapshot and
    /// `clear_dcr_requesting_client` acquiring `dcr_write_lock`. The
    /// re-check under the lock must observe the flipped provenance flag
    /// and leave the newly-manual credentials untouched.
    #[tokio::test]
    async fn clear_dcr_requesting_client_manual_same_id_rotation_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        // Same client_id as the caller's failing id, but the record has
        // just been rotated to manual (registered_via_dcr = false) via
        // POST /credentials.
        let rotated_manual = DcrCredentials {
            client_id: "shared-id".to_string(),
            client_secret: Some("operator-secret".to_string()),
            registered_via_dcr: false,
            ..make_dcr_creds()
        };
        mgr.save_dcr("ep", &rotated_manual).await.unwrap();

        let cleared = mgr
            .clear_dcr_requesting_client("ep", "shared-id")
            .await
            .unwrap();
        assert!(
            !cleared,
            "manual credentials with a matching client_id must survive the self-heal"
        );

        let loaded = mgr.load_dcr("ep").await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "shared-id");
        assert_eq!(loaded.client_secret.as_deref(), Some("operator-secret"));
        assert!(!loaded.registered_via_dcr);
    }

    /// R3-5: `update_dcr` must serialize its load-modify-save cycle
    /// against a concurrent `clear_dcr_requesting_client`. Simulates the
    /// race between a `POST /credentials` update that only touches the
    /// resource pair and a token-endpoint `invalid_client` self-heal
    /// firing on the same record: whichever writer runs second must
    /// observe the other's committed state, never a pre-lock snapshot.
    #[tokio::test]
    async fn update_dcr_and_clear_dcr_requesting_client_serialize_via_write_lock() {
        use std::sync::Arc;

        let tmp = tempfile::tempdir().unwrap();
        let mgr = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        // Seed a mixed record: DCR-provenanced requesting pair alongside a
        // separately-configured resource pair.
        let seed = DcrCredentials {
            client_id: "dcr-client".to_string(),
            client_secret: Some("dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 100,
            issuer: Some("https://as.example.com".to_string()),
            resource_client_id: Some("res-client".to_string()),
            resource_client_secret: Some("res-secret".to_string()),
            registered_via_dcr: true,
        };
        mgr.save_dcr("ep", &seed).await.unwrap();

        // Concurrent writers: (A) resource-only manual update that must
        // preserve the requesting pair, (B) invalid_client self-heal that
        // clears the requesting pair. Both fight over `dcr_write_lock`.
        let a_mgr = mgr.clone();
        let a = tokio::spawn(async move {
            a_mgr
                .update_dcr(
                    "ep",
                    |existing| -> Result<Option<DcrCredentials>, TokenError> {
                        let base = existing.unwrap();
                        Ok(Some(DcrCredentials {
                            resource_client_secret: Some("res-secret-v2".to_string()),
                            ..base
                        }))
                    },
                )
                .await
        });
        let b_mgr = mgr.clone();
        let b = tokio::spawn(
            async move { b_mgr.clear_dcr_requesting_client("ep", "dcr-client").await },
        );
        let (a_res, b_res) = tokio::join!(a, b);
        a_res.unwrap().unwrap();
        b_res.unwrap().unwrap();

        // Regardless of interleaving, the on-disk record must be a
        // consistent product of the two serialized writes:
        //   * If A ran first: A wrote {dcr-client, dcr-secret,
        //     res-secret-v2}; B then cleared the requesting pair,
        //     leaving {"", None, res-secret-v2}.
        //   * If B ran first: B cleared to {"", None, res-secret}; A then
        //     merged onto the tombstone → {"", None, res-secret-v2}.
        // Either way the resource_client_secret bump survives, and the
        // requesting pair ends up cleared (B always runs at some point).
        let final_state = mgr.load_dcr("ep").await.unwrap().unwrap();
        assert!(
            final_state.client_id.is_empty(),
            "self-heal must have cleared the requesting client_id"
        );
        assert!(
            final_state.client_secret.is_none(),
            "self-heal must have cleared the requesting client_secret"
        );
        assert_eq!(
            final_state.resource_client_secret.as_deref(),
            Some("res-secret-v2"),
            "the manual resource update must not be silently clobbered by the self-heal"
        );
        assert!(
            final_state.registered_via_dcr,
            "post-self-heal record retains DCR provenance"
        );
    }

    /// `update_dcr` deletes the DCR file when the closure returns `Ok(None)`.
    #[tokio::test]
    async fn update_dcr_deletes_when_closure_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_dcr("ep", &make_dcr_creds()).await.unwrap();

        let outcome = mgr
            .update_dcr("ep", |existing| -> Result<_, TokenError> {
                assert!(existing.is_some());
                Ok(None)
            })
            .await
            .unwrap();
        assert!(outcome.is_none());
        assert!(mgr.load_dcr("ep").await.unwrap().is_none());
    }

    /// Returning the loaded record unchanged from the closure is a true
    /// no-op: the DCR file is not rewritten (mtime unchanged), while a
    /// modified record still is.
    #[tokio::test]
    async fn update_dcr_skips_save_when_record_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_dcr("ep", &make_dcr_creds()).await.unwrap();

        let path = tmp.path().join("ep.dcr.json");
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        let outcome = mgr
            .update_dcr("ep", |existing| -> Result<_, TokenError> { Ok(existing) })
            .await
            .unwrap();
        assert_eq!(outcome, Some(make_dcr_creds()));
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "an unchanged record must not rewrite the DCR file"
        );

        // A genuinely modified record still lands on disk.
        let updated = mgr
            .update_dcr("ep", |existing| -> Result<_, TokenError> {
                let mut c = existing.unwrap();
                c.client_secret = Some("rotated".to_string());
                Ok(Some(c))
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.client_secret.as_deref(), Some("rotated"));
        let loaded = mgr.load_dcr("ep").await.unwrap().unwrap();
        assert_eq!(loaded.client_secret.as_deref(), Some("rotated"));
    }

    /// `update_dcr` propagates closure errors without touching disk.
    #[tokio::test]
    async fn update_dcr_propagates_closure_errors_without_writing() {
        #[derive(Debug)]
        enum E {
            Business,
            #[allow(dead_code)]
            Token(TokenError),
        }
        impl From<TokenError> for E {
            fn from(e: TokenError) -> Self {
                E::Token(e)
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let seed = make_dcr_creds();
        mgr.save_dcr("ep", &seed).await.unwrap();

        let outcome = mgr
            .update_dcr("ep", |_existing| -> Result<Option<DcrCredentials>, E> {
                Err(E::Business)
            })
            .await;
        assert!(matches!(outcome, Err(E::Business)));

        // On-disk record is untouched.
        let loaded = mgr.load_dcr("ep").await.unwrap().unwrap();
        assert_eq!(loaded, seed);
    }

    // --- Token construction tests (mirrors server.rs logic) ---

    #[test]
    fn token_construction_with_expires_in() {
        // Simulate the token_set construction from server.rs
        let now_secs: u64 = 1_700_000_000;
        let expires_in: u64 = 3600;
        let token_set = TokenSet {
            access_token: "access-tok".to_string(),
            refresh_token: Some("refresh-tok".to_string()),
            expires_at: Some(now_secs + expires_in),
            token_type: "Bearer".to_string(),
            scope: Some("read write".to_string()),
            issued_at: Some(now_secs),
        };
        assert_eq!(token_set.expires_at, Some(1_700_003_600));
        assert_eq!(token_set.issued_at, Some(now_secs));
    }

    #[test]
    fn token_construction_without_expires_in() {
        // When expires_in is missing from the response, expires_at = None
        let now_secs: u64 = 1_700_000_000;
        let expires_in: Option<u64> = None;
        let token_set = TokenSet {
            access_token: "access-tok".to_string(),
            refresh_token: None,
            expires_at: expires_in.map(|secs| now_secs + secs),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: Some(now_secs),
        };
        assert_eq!(token_set.expires_at, None);
        assert_eq!(token_set.issued_at, Some(now_secs));
    }

    #[test]
    fn token_construction_issued_at_is_now() {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token_set = TokenSet {
            access_token: "access-tok".to_string(),
            refresh_token: None,
            expires_at: Some(now_secs + 3600),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: Some(now_secs),
        };
        // issued_at should be within 1 second of now
        let issued = token_set.issued_at.unwrap();
        assert!(issued >= now_secs && issued <= now_secs + 1);
    }

    #[tokio::test]
    async fn dcr_save_without_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let creds = DcrCredentials {
            client_id: "public-client".to_string(),
            client_secret: None,
            client_secret_expires_at: 0,
            registered_at: 1700000000,
            issuer: None,
            ..Default::default()
        };

        mgr.save_dcr("public-ep", &creds).await.unwrap();
        let loaded = mgr.load_dcr("public-ep").await.unwrap().unwrap();
        assert_eq!(loaded, creds);
        assert!(loaded.client_secret.is_none());
    }

    #[tokio::test]
    async fn dcr_round_trip_preserves_issuer() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let creds = make_dcr_creds();
        assert_eq!(creds.issuer.as_deref(), Some("https://auth.example.com"));

        mgr.save_dcr("issuer-ep", &creds).await.unwrap();
        let loaded = mgr.load_dcr("issuer-ep").await.unwrap().unwrap();
        assert_eq!(loaded.issuer.as_deref(), Some("https://auth.example.com"));
        assert_eq!(loaded, creds);
    }

    #[test]
    fn dcr_legacy_file_without_issuer_loads_as_none() {
        // Credential files written before the `issuer` field existed must still
        // deserialize, with issuer = None (no spurious re-registration).
        let json = r#"{"client_id":"legacy-id","client_secret":"legacy-secret","client_secret_expires_at":0,"registered_at":1700000000}"#;
        let creds: DcrCredentials = serde_json::from_str(json).unwrap();
        assert_eq!(creds.client_id, "legacy-id");
        assert_eq!(creds.issuer, None);
    }

    #[test]
    fn dcr_legacy_file_without_registered_via_dcr_loads_as_false() {
        // Credential files written before the `registered_via_dcr` field
        // existed must still deserialize, with the flag = false (treated as
        // manual: never auto-discarded).
        let json = r#"{"client_id":"legacy-id","client_secret":"legacy-secret","client_secret_expires_at":0,"registered_at":1700000000,"issuer":"https://auth.example.com"}"#;
        let creds: DcrCredentials = serde_json::from_str(json).unwrap();
        assert_eq!(creds.client_id, "legacy-id");
        assert!(!creds.registered_via_dcr);
    }

    #[tokio::test]
    async fn dcr_round_trip_preserves_registered_via_dcr() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let creds = DcrCredentials {
            client_id: "dcr-minted".to_string(),
            client_secret: Some("dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 1_700_000_000,
            issuer: Some("https://auth.example.com".to_string()),
            registered_via_dcr: true,
            ..Default::default()
        };
        mgr.save_dcr("dcr-ep", &creds).await.unwrap();
        let loaded = mgr.load_dcr("dcr-ep").await.unwrap().unwrap();
        assert!(loaded.registered_via_dcr);
        assert_eq!(loaded, creds);
    }

    // --- dcr_issuer_allows_reuse() decision tests ---

    #[test]
    fn issuer_reuse_legacy_none_is_reused() {
        // Stored issuer None (legacy) → reuse regardless of current issuer.
        assert!(dcr_issuer_allows_reuse(None, Some("https://a.example")));
        assert!(dcr_issuer_allows_reuse(None, None));
    }

    #[test]
    fn issuer_reuse_matching_is_reused() {
        assert!(dcr_issuer_allows_reuse(
            Some("https://a.example"),
            Some("https://a.example")
        ));
    }

    #[test]
    fn issuer_reuse_differing_triggers_reregister() {
        assert!(!dcr_issuer_allows_reuse(
            Some("https://a.example"),
            Some("https://b.example")
        ));
    }

    #[test]
    fn issuer_reuse_unknown_current_is_reused() {
        // Current issuer unknown (discovery fallback) → cannot detect a
        // migration, so reuse to avoid spurious re-registration.
        assert!(dcr_issuer_allows_reuse(Some("https://a.example"), None));
    }

    // --- merge_scopes() tests ---

    #[test]
    fn merge_scopes_empty_prior_returns_requested() {
        assert_eq!(merge_scopes(None, "read write"), "read write");
        assert_eq!(merge_scopes(Some(""), "read write"), "read write");
    }

    #[test]
    fn merge_scopes_overlapping_dedups() {
        assert_eq!(
            merge_scopes(Some("read write"), "write delete"),
            "read write delete"
        );
    }

    #[test]
    fn merge_scopes_disjoint_unions() {
        assert_eq!(merge_scopes(Some("read"), "write"), "read write");
    }

    #[test]
    fn merge_scopes_preserves_prior_first_order() {
        // Prior scopes are emitted first, then new ones; dedup keeps first seen.
        assert_eq!(merge_scopes(Some("b a"), "a c b"), "b a c");
    }

    #[test]
    fn merge_scopes_empty_requested_keeps_prior() {
        assert_eq!(merge_scopes(Some("read write"), ""), "read write");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dcr_saved_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_dcr("perm-test", &make_dcr_creds()).await.unwrap();
        let path = tmp.path().join("perm-test.dcr.json");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "Expected 0600, got {:o}", mode & 0o777);
    }

    // --- IdP credential persistence tests ---

    fn make_idp_creds() -> IdpCredentials {
        IdpCredentials {
            idp_issuer: "https://acme.okta.com".to_string(),
            id_token: "idp-id-token".to_string(),
            refresh_token: Some("idp-refresh-token".to_string()),
            id_token_expires_at: Some(1700003600),
            obtained_at: 1700000000,
        }
    }

    #[tokio::test]
    async fn idp_save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let creds = make_idp_creds();

        mgr.save_idp("https://acme.okta.com", &creds).await.unwrap();
        let loaded = mgr
            .load_idp("https://acme.okta.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded, creds);
    }

    #[tokio::test]
    async fn idp_load_nonexistent_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let result = mgr.load_idp("https://nobody.okta.com").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn idp_delete_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp("https://acme.okta.com", &make_idp_creds())
            .await
            .unwrap();
        assert!(mgr
            .load_idp("https://acme.okta.com")
            .await
            .unwrap()
            .is_some());
        mgr.delete_idp("https://acme.okta.com").await.unwrap();
        assert!(mgr
            .load_idp("https://acme.okta.com")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn idp_delete_nonexistent_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        // Should not error on first or second call
        mgr.delete_idp("https://nobody.okta.com").await.unwrap();
        mgr.delete_idp("https://nobody.okta.com").await.unwrap();
    }

    #[tokio::test]
    async fn idp_save_without_optional_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let creds = IdpCredentials {
            idp_issuer: "https://acme.okta.com".to_string(),
            id_token: "idp-id-token".to_string(),
            refresh_token: None,
            id_token_expires_at: None,
            obtained_at: 1700000000,
        };

        mgr.save_idp("https://acme.okta.com", &creds).await.unwrap();
        let loaded = mgr
            .load_idp("https://acme.okta.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded, creds);
        assert!(loaded.refresh_token.is_none());
        assert!(loaded.id_token_expires_at.is_none());
    }

    #[test]
    fn idp_tolerates_missing_optional_fields_on_deserialize() {
        // Records written without the optional fields must still deserialize,
        // with refresh_token = None and id_token_expires_at = None.
        let json =
            r#"{"idp_issuer":"https://acme.okta.com","id_token":"tok","obtained_at":1700000000}"#;
        let creds: IdpCredentials = serde_json::from_str(json).unwrap();
        assert_eq!(creds.idp_issuer, "https://acme.okta.com");
        assert_eq!(creds.id_token, "tok");
        assert_eq!(creds.refresh_token, None);
        assert_eq!(creds.id_token_expires_at, None);
        assert_eq!(creds.obtained_at, 1700000000);
    }

    #[test]
    fn idp_distinct_issuers_resolve_to_distinct_files() {
        // The sanitized key must keep different issuers in different files so
        // multiple IdPs don't clobber one another.
        assert_ne!(
            sanitize_idp_key("https://acme.okta.com"),
            sanitize_idp_key("https://other.okta.com")
        );
    }

    #[test]
    fn idp_keys_that_collided_under_char_map_now_resolve_distinctly() {
        // Under the previous character-map sanitization (every non
        // `[A-Za-z0-9_-]` char → `_`), these distinct issuers both mapped to the
        // same stem `https___acme.example.com_a_b`, so one IdP's credentials
        // overwrote the other's. Hashing must keep them in separate files.
        assert_ne!(
            sanitize_idp_key("https://acme.example.com/a/b"),
            sanitize_idp_key("https://acme.example.com/a_b"),
        );
        assert_ne!(
            sanitize_idp_key("https://acme.example.com:8443"),
            sanitize_idp_key("https://acme.example.com_8443"),
        );
    }

    #[tokio::test]
    async fn idp_colliding_keys_do_not_clobber_each_other() {
        // End-to-end guard: saving under two issuers that previously collided
        // must persist two independent records, not overwrite one with the other.
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        let mut a = make_idp_creds();
        a.idp_issuer = "https://acme.example.com/a/b".to_string();
        a.id_token = "token-a".to_string();
        let mut b = make_idp_creds();
        b.idp_issuer = "https://acme.example.com/a_b".to_string();
        b.id_token = "token-b".to_string();

        mgr.save_idp(&a.idp_issuer, &a).await.unwrap();
        mgr.save_idp(&b.idp_issuer, &b).await.unwrap();

        let loaded_a = mgr.load_idp(&a.idp_issuer).await.unwrap().unwrap();
        let loaded_b = mgr.load_idp(&b.idp_issuer).await.unwrap().unwrap();
        assert_eq!(loaded_a.id_token, "token-a");
        assert_eq!(loaded_b.id_token, "token-b");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn idp_saved_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TokenManager::new(tmp.path().to_path_buf());
        mgr.save_idp("https://acme.okta.com", &make_idp_creds())
            .await
            .unwrap();
        let path = tmp.path().join(format!(
            "{}.idp.json",
            sanitize_idp_key("https://acme.okta.com")
        ));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "Expected 0600, got {:o}", mode & 0o777);
    }
}
