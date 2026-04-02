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
        assert!(ts.is_valid(), "Tokens with no expiry should be considered valid");
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
        assert!(!ts.is_valid(), "Token expiring within 30s buffer should be invalid");
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
        assert!(ts.is_valid(), "Token expiring in 31s should be valid (outside 30s buffer)");
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
}
