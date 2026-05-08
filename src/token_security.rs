use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum TokenSecurityError {
    #[error("Token directory at {path} is not secure — other users may be able to read OAuth tokens. Fix permissions (chmod 0700) or set a different token_dir in config.")]
    InsecureDirectory { path: String },
    #[error("Failed to create token directory at {path}: {source}")]
    CreateFailed {
        path: String,
        source: std::io::Error,
    },
    #[error("Failed to set permissions on {path}: {source}")]
    PermissionSetFailed {
        path: String,
        source: std::io::Error,
    },
}

/// Ensure the token directory exists and has secure permissions (0700 on Unix).
///
/// Called once at relay startup, before any adapters initialize. This is a hard
/// gate — if it fails, the relay does not start.
///
/// 1. If dir doesn't exist: create with 0700
/// 2. If dir exists: check permissions
/// 3. If too open: attempt chmod 0700
/// 4. If unfixable: return Err (relay refuses to start)
/// 5. Return canonical path on success
pub fn ensure_token_dir_secure(path: &Path) -> Result<PathBuf, TokenSecurityError> {
    #[cfg(unix)]
    {
        ensure_token_dir_secure_unix(path)
    }

    #[cfg(not(unix))]
    {
        ensure_token_dir_secure_fallback(path)
    }
}

#[cfg(unix)]
fn ensure_token_dir_secure_unix(path: &Path) -> Result<PathBuf, TokenSecurityError> {
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if !path.exists() {
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(path)
            .map_err(|e| TokenSecurityError::CreateFailed {
                path: path.display().to_string(),
                source: e,
            })?;
        tracing::info!(path = %path.display(), "Created token directory with 0700 permissions");
    } else {
        let metadata = fs::metadata(path).map_err(|e| TokenSecurityError::CreateFailed {
            path: path.display().to_string(),
            source: e,
        })?;

        if !metadata.is_dir() {
            return Err(TokenSecurityError::CreateFailed {
                path: path.display().to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "Path exists but is not a directory",
                ),
            });
        }

        let mode = metadata.permissions().mode();
        // Check no group/other access (bits 0o077)
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{:o}", mode),
                "Token directory has insecure permissions, attempting to fix"
            );
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| {
                TokenSecurityError::PermissionSetFailed {
                    path: path.display().to_string(),
                    source: e,
                }
            })?;

            // Verify the fix was applied
            let new_mode = fs::metadata(path)
                .map_err(|e| TokenSecurityError::PermissionSetFailed {
                    path: path.display().to_string(),
                    source: e,
                })?
                .permissions()
                .mode();

            if new_mode & 0o077 != 0 {
                return Err(TokenSecurityError::InsecureDirectory {
                    path: path.display().to_string(),
                });
            }
            tracing::info!(path = %path.display(), "Fixed token directory permissions to 0700");
        }
    }

    path.canonicalize()
        .map_err(|e| TokenSecurityError::CreateFailed {
            path: path.display().to_string(),
            source: e,
        })
}

/// Best-effort detection of common consumer cloud-sync providers in a path.
///
/// OAuth refresh tokens stored in a synced directory are silently uploaded
/// to a third-party server, copied to every other device on the account, and
/// retained in the provider's version history. This is almost never what a
/// user intends. We log a one-shot warning at startup so users notice it
/// without blocking the relay from running (the user may have intentionally
/// chosen this path, or be using a non-syncing subdirectory of a synced root).
///
/// Returns `Some(provider_name)` if a known provider segment is found, else
/// `None`. The match is case-insensitive against path components only — we
/// don't grep arbitrary substrings to avoid false positives like
/// "/home/dropbox-user/...".
pub fn detect_cloud_sync_provider(path: &Path) -> Option<&'static str> {
    // Case-insensitive component match. Each entry is (lowercase needle,
    // pretty name). Order matters only for stability of the returned name.
    const PROVIDERS: &[(&str, &str)] = &[
        ("dropbox", "Dropbox"),
        ("onedrive", "OneDrive"),
        ("google drive", "Google Drive"),
        ("googledrive", "Google Drive"),
        ("googledrivefs", "Google Drive"),
        ("drivefs", "Google Drive"),
        ("icloud", "iCloud Drive"),
        ("mobile documents", "iCloud Drive"),
        ("com~apple~clouddocs", "iCloud Drive"),
        // macOS umbrella root for File Provider-based sync (Dropbox,
        // OneDrive-Personal, GoogleDrive-AccountName, Box, etc.). The
        // sub-folder names carry account suffixes that won't match the
        // provider needles above on their own, so detect the root itself.
        ("cloudstorage", "macOS CloudStorage"),
        ("box sync", "Box"),
        ("boxdrive", "Box"),
        ("sync.com", "Sync.com"),
        ("pcloud", "pCloud"),
        ("mega", "MEGA"),
    ];

    // Lowercase each component once. Then iterate providers in the order
    // listed — earlier entries are more specific and win over later
    // catch-all entries like "cloudstorage".
    let lowered: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    for (needle, pretty) in PROVIDERS {
        if lowered.iter().any(|c| c == *needle) {
            return Some(pretty);
        }
    }
    None
}

/// Emit a one-shot warning if `path` looks like it lives inside a known
/// consumer cloud-sync provider (Dropbox, iCloud, OneDrive, Google Drive,
/// etc.). Non-fatal — the relay will still start.
pub fn warn_if_cloud_synced(path: &Path) {
    if let Some(provider) = detect_cloud_sync_provider(path) {
        tracing::warn!(
            path = %path.display(),
            provider = provider,
            "Token directory appears to live inside {provider}. \
             OAuth refresh tokens stored here will be uploaded to {provider} \
             and synced to every other device on the same account. \
             Move token_dir outside the synced folder unless this is intentional."
        );
    }
}

#[cfg(not(unix))]
fn ensure_token_dir_secure_fallback(path: &Path) -> Result<PathBuf, TokenSecurityError> {
    use std::fs;

    if !path.exists() {
        fs::create_dir_all(path).map_err(|e| TokenSecurityError::CreateFailed {
            path: path.display().to_string(),
            source: e,
        })?;
        tracing::info!(path = %path.display(), "Created token directory");
    }

    path.canonicalize()
        .map_err(|e| TokenSecurityError::CreateFailed {
            path: path.display().to_string(),
            source: e,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn creates_dir_if_not_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let token_dir = tmp.path().join("tokens");
        assert!(!token_dir.exists());
        let result = ensure_token_dir_secure(&token_dir).unwrap();
        assert!(result.exists());
        assert!(result.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn creates_dir_with_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let token_dir = tmp.path().join("tokens");
        ensure_token_dir_secure(&token_dir).unwrap();
        let mode = fs::metadata(&token_dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "Expected 0700, got {:o}", mode & 0o777);
    }

    #[cfg(unix)]
    #[test]
    fn fixes_insecure_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let token_dir = tmp.path().join("tokens");
        fs::create_dir_all(&token_dir).unwrap();
        fs::set_permissions(&token_dir, fs::Permissions::from_mode(0o777)).unwrap();
        ensure_token_dir_secure(&token_dir).unwrap();
        let mode = fs::metadata(&token_dir).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "Expected 0700 after fix, got {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn error_if_path_is_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not_a_dir");
        fs::write(&file_path, "hello").unwrap();
        let result = ensure_token_dir_secure(&file_path);
        assert!(result.is_err());
    }

    #[test]
    fn creates_nested_dirs_if_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b").join("tokens");
        assert!(!nested.exists());
        let result = ensure_token_dir_secure(&nested).unwrap();
        assert!(result.exists());
        assert!(result.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn nested_dir_leaf_has_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("x").join("y").join("tokens");
        ensure_token_dir_secure(&nested).unwrap();
        let mode = fs::metadata(&nested).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "Leaf directory expected 0700, got {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn idempotent_on_existing_secure_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let token_dir = tmp.path().join("tokens");
        // Call twice — both should succeed
        let r1 = ensure_token_dir_secure(&token_dir).unwrap();
        let r2 = ensure_token_dir_secure(&token_dir).unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn concurrent_ensure_does_not_panic() {
        use std::sync::Arc;
        let tmp = tempfile::tempdir().unwrap();
        let base = Arc::new(tmp.path().to_path_buf());

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let base = Arc::clone(&base);
                std::thread::spawn(move || {
                    let dir = base.join("concurrent_tokens");
                    ensure_token_dir_secure(&dir)
                })
            })
            .collect();

        for h in handles {
            // All threads should succeed (no panics, no errors)
            h.join().unwrap().unwrap();
        }
    }

    #[test]
    fn detect_cloud_sync_dropbox() {
        let p = PathBuf::from("/Users/alice/Dropbox/relay/tokens");
        assert_eq!(detect_cloud_sync_provider(&p), Some("Dropbox"));
    }

    #[test]
    fn detect_cloud_sync_dropbox_case_insensitive() {
        let p = PathBuf::from("/home/alice/dropbox/relay/tokens");
        assert_eq!(detect_cloud_sync_provider(&p), Some("Dropbox"));
    }

    #[test]
    fn detect_cloud_sync_icloud_macos() {
        let p =
            PathBuf::from("/Users/alice/Library/Mobile Documents/com~apple~CloudDocs/relay/tokens");
        assert_eq!(detect_cloud_sync_provider(&p), Some("iCloud Drive"));
    }

    #[test]
    fn detect_cloud_sync_onedrive() {
        let p = PathBuf::from("/Users/alice/OneDrive/relay/tokens");
        assert_eq!(detect_cloud_sync_provider(&p), Some("OneDrive"));
    }

    #[test]
    fn detect_cloud_sync_google_drive_with_space() {
        let p = PathBuf::from("/Users/alice/Google Drive/relay/tokens");
        assert_eq!(detect_cloud_sync_provider(&p), Some("Google Drive"));
    }

    #[test]
    fn detect_cloud_sync_google_drivefs() {
        let p = PathBuf::from("/Users/alice/Library/CloudStorage/GoogleDrive/relay/tokens");
        assert_eq!(detect_cloud_sync_provider(&p), Some("Google Drive"));
    }

    #[test]
    fn detect_cloud_sync_cloudstorage_umbrella_with_suffixed_provider() {
        // macOS File Provider-based sync uses suffixed folder names like
        // "OneDrive-Personal" or "GoogleDrive-alice@example.com". The
        // CloudStorage umbrella catches them even when the sub-folder name
        // doesn't match a known provider needle exactly.
        let p = PathBuf::from("/Users/alice/Library/CloudStorage/OneDrive-Personal/relay/tokens");
        assert_eq!(detect_cloud_sync_provider(&p), Some("macOS CloudStorage"));
    }

    #[test]
    fn detect_cloud_sync_no_false_positive_substring() {
        // "dropbox-user" is a substring of "dropbox" but not a path component.
        let p = PathBuf::from("/home/dropbox-user/relay/tokens");
        assert_eq!(detect_cloud_sync_provider(&p), None);
    }

    #[test]
    fn detect_cloud_sync_returns_none_for_normal_paths() {
        let p = PathBuf::from("/Users/alice/.local/share/endara-relay/tokens");
        assert_eq!(detect_cloud_sync_provider(&p), None);
    }

    #[test]
    fn warn_if_cloud_synced_does_not_panic() {
        // Smoke test — both paths must complete without panicking.
        warn_if_cloud_synced(&PathBuf::from("/Users/alice/Dropbox/relay/tokens"));
        warn_if_cloud_synced(&PathBuf::from(
            "/Users/alice/.local/share/endara-relay/tokens",
        ));
    }
}
