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
}
