//! Resolves the user's login-shell PATH so that stdio child processes can find
//! commands installed via nvm, Homebrew, pyenv, etc.
//!
//! When the relay runs as a Tauri sidecar the inherited environment is minimal.
//! This module runs the user's login shell once to capture their full `$PATH`
//! and caches the result for the lifetime of the process.

use std::sync::OnceLock;
use tracing::{info, warn};

/// Cached login-shell PATH (resolved once, reused forever).
static SHELL_PATH: OnceLock<Option<String>> = OnceLock::new();

/// Return the user's login-shell PATH, resolving it on first call.
///
/// On macOS/Linux this spawns `$SHELL -l -c 'echo $PATH'` (falling back to
/// `/bin/bash` when `$SHELL` is unset). On Windows the current process PATH is
/// returned unchanged.
pub fn resolve_shell_path() -> Option<&'static str> {
    SHELL_PATH
        .get_or_init(|| {
            let path = platform_resolve();
            match &path {
                Some(p) => info!(path = %p, "resolved login-shell PATH"),
                None => warn!("could not resolve login-shell PATH; using inherited environment"),
            }
            path
        })
        .as_deref()
}

// ------------------------------------------------------------------
// Platform implementation
// ------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
fn platform_resolve() -> Option<String> {
    use std::process::Command;

    // Prefer the user's configured shell; fall back to /bin/bash.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

    let output = Command::new(&shell)
        .args(["-l", "-c", "echo $PATH"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if raw.is_empty() {
                warn!(shell = %shell, "login shell returned empty PATH");
                None
            } else {
                Some(raw)
            }
        }
        Ok(out) => {
            warn!(
                shell = %shell,
                status = ?out.status,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "login shell exited with error while resolving PATH"
            );
            None
        }
        Err(e) => {
            warn!(shell = %shell, error = %e, "failed to spawn login shell for PATH resolution");
            None
        }
    }
}

#[cfg(target_os = "windows")]
fn platform_resolve() -> Option<String> {
    // Windows inherits PATH normally; nothing special needed.
    std::env::var("PATH").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn resolve_returns_some_on_unix() {
        // On any Unix CI or dev machine this should succeed.
        let path = platform_resolve();
        assert!(path.is_some(), "expected Some(PATH) on unix");
        let p = path.unwrap();
        // /usr/bin should always be present.
        assert!(
            p.contains("/usr/bin"),
            "PATH should contain /usr/bin, got: {p}"
        );
    }

    #[test]
    fn cached_value_is_consistent() {
        let a = resolve_shell_path();
        let b = resolve_shell_path();
        assert_eq!(a, b);
    }
}
