//! Management API listener.
//!
//! The management API (`/api/*`) is exposed exclusively over a Unix-domain
//! socket on macOS / Linux, or a Windows named pipe on Windows. It is never
//! reachable over TCP. This eliminates the DNS-rebinding / CSRF attack surface
//! for the credential-bearing management routes (Cluster 1 of the security
//! audit).
//!
//! Path resolution (in order of preference):
//!
//! - **Linux**: `$XDG_RUNTIME_DIR/endara-relay/api.sock`
//! - **macOS**: `$TMPDIR/endara-relay-<uid>/api.sock`
//! - **Windows**: `\\.\pipe\endara-relay-<sessionid>`
//! - Fallback (any platform): `<data-dir>/api.sock` (or
//!   `\\.\pipe\endara-relay-<data-dir-hash>` on Windows).
//!
//! On Unix, the socket file is created with `0600` permissions and stale
//! socket files are removed at startup if no live process is listening.
//! Each accepted connection is verified against the relay's effective UID
//! via `SO_PEERCRED` (`UCred`); mismatched UIDs are rejected.

use std::io;
use std::path::{Path, PathBuf};

use axum::Router;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Resolve the management-API socket / pipe path.
///
/// Honors `ENDARA_API_SOCKET` for tests, otherwise picks a per-user runtime
/// directory and falls back to `<data_dir>/api.sock` if the runtime dir is
/// unavailable.
#[allow(clippy::needless_return)]
pub fn resolve_api_socket_path(#[allow(unused_variables)] data_dir: &Path) -> PathBuf {
    if let Ok(path) = std::env::var("ENDARA_API_SOCKET") {
        return PathBuf::from(path);
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            let runtime = PathBuf::from(xdg);
            if !runtime.as_os_str().is_empty() {
                return runtime.join("endara-relay").join("api.sock");
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let tmp = std::env::var("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
        let uid = unsafe { geteuid_u32() };
        return tmp.join(format!("endara-relay-{uid}")).join("api.sock");
    }

    #[cfg(windows)]
    {
        let session_id = current_user_pipe_suffix(data_dir);
        return PathBuf::from(format!(r"\\.\pipe\endara-relay-{session_id}"));
    }

    // Final fallback — reached on Linux when XDG_RUNTIME_DIR is unset, and on
    // any platform not covered by the cfg branches above. macOS / Windows
    // branches above always early-return, so they never fall through here.
    data_dir.join("api.sock")
}

#[cfg(unix)]
unsafe fn geteuid_u32() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    geteuid()
}

#[cfg(windows)]
fn current_user_pipe_suffix(data_dir: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    if let Ok(user) = std::env::var("USERNAME") {
        let mut h = DefaultHasher::new();
        user.hash(&mut h);
        return format!("{:x}", h.finish());
    }
    let mut h = DefaultHasher::new();
    data_dir.hash(&mut h);
    format!("{:x}", h.finish())
}

/// Start the management API listener.
///
/// Returns the resolved listener path and a `JoinHandle` for the server task.
/// The server runs until the process is shut down.
pub async fn serve_management_api(
    router: Router,
    path: PathBuf,
) -> io::Result<(PathBuf, JoinHandle<()>)> {
    #[cfg(unix)]
    {
        serve_unix(router, path).await
    }
    #[cfg(windows)]
    {
        serve_windows(router, path).await
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (router, path);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "management listener: unsupported platform",
        ))
    }
}

// ---------------------------------------------------------------------------
// Unix
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn serve_unix(router: Router, path: PathBuf) -> io::Result<(PathBuf, JoinHandle<()>)> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        // Best-effort: tighten parent dir to 0700 if writable.
        let _ = tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await;
    }

    cleanup_stale_unix_socket(&path).await?;

    let listener = UnixListener::bind(&path)?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;

    info!(path = %path.display(), "Management API listening on Unix socket");

    let expected_uid = unsafe { geteuid_u32() };
    let path_clone = path.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = run_unix_accept_loop(listener, router, expected_uid).await {
            warn!(error = %e, "Management API accept loop terminated");
        }
        let _ = tokio::fs::remove_file(&path_clone).await;
    });

    Ok((path, handle))
}

#[cfg(unix)]
async fn cleanup_stale_unix_socket(path: &Path) -> io::Result<()> {
    use tokio::net::UnixStream;
    if !path.exists() {
        return Ok(());
    }
    match tokio::time::timeout(
        std::time::Duration::from_millis(200),
        UnixStream::connect(path),
    )
    .await
    {
        Ok(Ok(_)) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another endara-relay instance is listening on {}",
                path.display()
            ),
        )),
        _ => {
            warn!(path = %path.display(), "Removing stale management socket");
            tokio::fs::remove_file(path).await?;
            Ok(())
        }
    }
}

#[cfg(unix)]
async fn run_unix_accept_loop(
    listener: tokio::net::UnixListener,
    router: Router,
    expected_uid: u32,
) -> io::Result<()> {
    use hyper::body::Incoming;
    use hyper::Request;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::ServiceExt;

    loop {
        let (stream, _addr) = listener.accept().await?;

        match stream.peer_cred() {
            Ok(cred) => {
                let peer_uid = cred.uid();
                if peer_uid != expected_uid {
                    warn!(
                        peer_uid,
                        expected_uid, "Rejecting management API connection: UID mismatch"
                    );
                    drop(stream);
                    continue;
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to read peer credentials; rejecting connection");
                drop(stream);
                continue;
            }
        }

        let svc = router.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service =
                hyper::service::service_fn(move |req: Request<Incoming>| svc.clone().oneshot(req));
            if let Err(e) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, service)
                .await
            {
                if is_benign_connection_close(&*e) {
                    tracing::trace!(error = %e, "Management API connection closed");
                } else {
                    tracing::debug!(error = %e, "Management API connection ended with error");
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
async fn serve_windows(router: Router, path: PathBuf) -> io::Result<(PathBuf, JoinHandle<()>)> {
    let pipe_name = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-utf8 pipe name"))?
        .to_string();

    info!(name = %pipe_name, "Management API listening on Windows named pipe");

    let path_clone = path.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = run_windows_accept_loop(pipe_name, router).await {
            warn!(error = %e, "Management API accept loop terminated");
        }
    });

    Ok((path_clone, handle))
}

#[cfg(windows)]
async fn run_windows_accept_loop(pipe_name: String, router: Router) -> io::Result<()> {
    use hyper::body::Incoming;
    use hyper::Request;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tokio::net::windows::named_pipe::ServerOptions;
    use tower::ServiceExt;

    // Windows named-pipe instances inherit the process's primary token DACL,
    // which restricts access to the current user by default. `first_pipe_instance`
    // on the initial server prevents another process from hijacking the name.
    let mut first = true;
    loop {
        let mut opts = ServerOptions::new();
        if first {
            opts.first_pipe_instance(true);
            first = false;
        }
        let server = opts.create(&pipe_name)?;
        server.connect().await?;

        let svc = router.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(server);
            let service =
                hyper::service::service_fn(move |req: Request<Incoming>| svc.clone().oneshot(req));
            if let Err(e) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, service)
                .await
            {
                if is_benign_connection_close(&*e) {
                    tracing::trace!(error = %e, "Management API connection closed");
                } else {
                    tracing::debug!(error = %e, "Management API connection ended with error");
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Connection-close error classification
// ---------------------------------------------------------------------------

/// Classify an error returned by `serve_connection` as a benign per-connection
/// close (the client opened a fresh connection, did one round-trip, and
/// dropped its sender) versus a genuinely unexpected failure.
///
/// The desktop's API proxy opens a new UDS / Named-Pipe HTTP/1 connection per
/// `/api/*` request, so hyper consistently surfaces a half-close error once
/// per request. Logging those at `debug!` produces a constant stream of noise;
/// route them to `trace!` instead and keep `debug!` for genuine errors.
fn is_benign_connection_close(err: &(dyn std::error::Error + 'static)) -> bool {
    let msg = err.to_string();
    if msg.contains("error shutting down connection")
        || msg.contains("connection closed before message completed")
        || msg.contains("IncompleteMessage")
    {
        return true;
    }
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            if matches!(
                io.kind(),
                BrokenPipe | UnexpectedEof | ConnectionAborted | ConnectionReset
            ) {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_api_socket_path_honors_env_var() {
        std::env::set_var("ENDARA_API_SOCKET", "/tmp/endara-test.sock");
        let p = resolve_api_socket_path(Path::new("/tmp"));
        assert_eq!(p, PathBuf::from("/tmp/endara-test.sock"));
        std::env::remove_var("ENDARA_API_SOCKET");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_stale_socket_removes_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        tokio::fs::write(&path, b"").await.unwrap();
        assert!(path.exists());
        cleanup_stale_unix_socket(&path).await.unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn benign_close_matches_broken_pipe_io_error() {
        let err = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(is_benign_connection_close(&err));
    }

    #[test]
    fn benign_close_matches_unexpected_eof_io_error() {
        let err = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert!(is_benign_connection_close(&err));
    }

    #[test]
    fn benign_close_matches_known_message_substring() {
        #[derive(Debug)]
        struct FakeErr;
        impl std::fmt::Display for FakeErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error shutting down connection")
            }
        }
        impl std::error::Error for FakeErr {}
        let err = FakeErr;
        assert!(is_benign_connection_close(&err));
    }

    #[test]
    fn benign_close_rejects_unexpected_io_error() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!is_benign_connection_close(&err));
    }

    #[test]
    fn benign_close_rejects_unrelated_error() {
        #[derive(Debug)]
        struct OtherErr;
        impl std::fmt::Display for OtherErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("something went wrong")
            }
        }
        impl std::error::Error for OtherErr {}
        let err = OtherErr;
        assert!(!is_benign_connection_close(&err));
    }
}
