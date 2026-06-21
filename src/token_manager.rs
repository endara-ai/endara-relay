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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
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
}

impl TokenManager {
    pub fn new(token_dir: PathBuf) -> Self {
        Self { token_dir }
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
    /// File permissions: 0600 on Unix.
    pub async fn save_dcr(
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
    pub async fn delete_dcr(&self, endpoint_name: &str) -> Result<(), TokenError> {
        let path = self.token_dir.join(format!("{}.dcr.json", endpoint_name));
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
}
