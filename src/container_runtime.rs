//! Detects a usable OCI container runtime (Docker or Podman) on the host.
//!
//! Probes well-known runtime socket locations (Docker Desktop, OrbStack,
//! Rancher Desktop, Podman, ...) and the user's login-shell PATH for a
//! `docker`/`podman` CLI. The result is cached for the process lifetime,
//! mirroring [`crate::shell_env`].

use crate::shell_env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing::{info, warn};

/// Which container engine was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    Docker,
    Podman,
}

impl RuntimeKind {
    /// CLI binary name for this runtime.
    pub fn cli_name(&self) -> &'static str {
        match self {
            RuntimeKind::Docker => "docker",
            RuntimeKind::Podman => "podman",
        }
    }
}

/// A detected, usable container runtime. At least one of `cli_path` /
/// `socket` is always `Some`.
#[derive(Debug, Clone)]
pub struct RuntimeInfo {
    pub kind: RuntimeKind,
    /// Resolved CLI binary (e.g. `/usr/local/bin/docker`), if found on PATH.
    pub cli_path: Option<PathBuf>,
    /// Engine API socket (Unix socket path, or named pipe on Windows).
    pub socket: Option<PathBuf>,
    /// CLI client version (e.g. `27.3.1`), if the CLI was found.
    pub version: Option<String>,
}

/// Cached detection result (resolved once, reused forever).
static DETECTED: OnceLock<Option<RuntimeInfo>> = OnceLock::new();

/// Detect a usable container runtime, caching the result for the process
/// lifetime. Returns `None` when neither a runtime socket nor a
/// `docker`/`podman` CLI is found.
pub fn detect_runtime() -> Option<&'static RuntimeInfo> {
    DETECTED
        .get_or_init(|| {
            let path_env = resolve_path_env();
            let candidates = candidate_sockets(&Probe::from_env());
            let info = detect_from(&candidates, path_env.as_deref());
            match &info {
                Some(i) => info!(
                    kind = i.kind.cli_name(),
                    cli = ?i.cli_path,
                    socket = ?i.socket,
                    version = ?i.version,
                    "detected container runtime"
                ),
                None => warn!("no container runtime detected (docker/podman)"),
            }
            info
        })
        .as_ref()
}

/// Environment inputs for socket-candidate enumeration. Separated from
/// process globals so unit tests can construct arbitrary probes.
struct Probe {
    /// `$ENDARA_DOCKER_SOCKET` override (may carry a `unix://` prefix).
    env_socket: Option<String>,
    home: Option<PathBuf>,
    xdg_runtime_dir: Option<String>,
}

impl Probe {
    fn from_env() -> Self {
        Self {
            env_socket: std::env::var("ENDARA_DOCKER_SOCKET").ok(),
            home: dirs::home_dir(),
            xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(),
        }
    }
}

/// PATH used for CLI probing: login-shell PATH when resolvable, otherwise
/// the inherited process PATH.
fn resolve_path_env() -> Option<String> {
    shell_env::resolve_shell_path()
        .map(str::to_string)
        .or_else(|| std::env::var("PATH").ok())
}

/// Well-known runtime socket locations, in priority order.
fn candidate_sockets(probe: &Probe) -> Vec<(RuntimeKind, PathBuf)> {
    let mut out = Vec::new();
    if let Some(s) = &probe.env_socket {
        let p = s.strip_prefix("unix://").unwrap_or(s);
        if !p.is_empty() {
            out.push((RuntimeKind::Docker, PathBuf::from(p)));
        }
    }
    out.push((RuntimeKind::Docker, PathBuf::from("/var/run/docker.sock")));
    if let Some(home) = &probe.home {
        out.push((RuntimeKind::Docker, home.join(".docker/run/docker.sock")));
        out.push((RuntimeKind::Docker, home.join(".orbstack/run/docker.sock")));
        out.push((RuntimeKind::Docker, home.join(".rd/docker.sock")));
    }
    if let Some(xdg) = &probe.xdg_runtime_dir {
        out.push((
            RuntimeKind::Podman,
            Path::new(xdg).join("podman/podman.sock"),
        ));
    }
    if let Some(home) = &probe.home {
        out.push((
            RuntimeKind::Podman,
            home.join(".local/share/containers/podman/machine/podman.sock"),
        ));
    }
    #[cfg(target_os = "windows")]
    out.push((
        RuntimeKind::Docker,
        PathBuf::from(r"\\.\pipe\docker_engine"),
    ));
    out
}

/// Core detection: first existing socket wins (CLI then resolved for that
/// runtime); otherwise fall back to a pure CLI probe (docker, then podman).
fn detect_from(
    candidates: &[(RuntimeKind, PathBuf)],
    path_env: Option<&str>,
) -> Option<RuntimeInfo> {
    if let Some((kind, socket)) = candidates.iter().find(|(_, p)| p.exists()) {
        let cli_path = find_in_path(kind.cli_name(), path_env);
        let version = cli_path.as_deref().and_then(cli_version);
        return Some(RuntimeInfo {
            kind: *kind,
            cli_path,
            socket: Some(socket.clone()),
            version,
        });
    }
    for kind in [RuntimeKind::Docker, RuntimeKind::Podman] {
        if let Some(cli_path) = find_in_path(kind.cli_name(), path_env) {
            let version = cli_version(&cli_path);
            return Some(RuntimeInfo {
                kind,
                cli_path: Some(cli_path),
                socket: None,
                version,
            });
        }
    }
    None
}

/// Search a PATH-style string for an executable named `name`.
fn find_in_path(name: &str, path_env: Option<&str>) -> Option<PathBuf> {
    let path_env = path_env?;
    for dir in std::env::split_paths(path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in exe_candidates(&dir, name) {
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn exe_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name)]
}

#[cfg(target_os = "windows")]
fn exe_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(format!("{name}.exe")), dir.join(name)]
}

#[cfg(not(target_os = "windows"))]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Run `<cli> --version` and parse out the version number. `--version` only
/// touches the client binary (never the daemon), so it is fast even when the
/// engine VM is stopped.
fn cli_version(cli: &Path) -> Option<String> {
    // Spawning a freshly written/copied binary can transiently fail with
    // ETXTBSY ("text file busy") while another handle still has it open for
    // writing. Retry a bounded number of times before giving up.
    const MAX_ATTEMPTS: usize = 10;
    let mut output = std::process::Command::new(cli).arg("--version").output();
    for _ in 1..MAX_ATTEMPTS {
        match &output {
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    || e.raw_os_error() == Some(26) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
                output = std::process::Command::new(cli).arg("--version").output();
            }
            _ => break,
        }
    }
    match output {
        Ok(out) if out.status.success() => parse_cli_version(&String::from_utf8_lossy(&out.stdout)),
        Ok(out) => {
            warn!(cli = %cli.display(), status = ?out.status, "runtime CLI --version failed");
            None
        }
        Err(e) => {
            warn!(cli = %cli.display(), error = %e, "failed to run runtime CLI --version");
            None
        }
    }
}

/// Parse version output like `Docker version 27.3.1, build abc123` or
/// `podman version 5.2.0`.
fn parse_cli_version(output: &str) -> Option<String> {
    let mut tokens = output.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok.eq_ignore_ascii_case("version") {
            return tokens.next().map(|v| v.trim_end_matches(',').to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(env_socket: Option<&str>, home: Option<&Path>, xdg: Option<&str>) -> Probe {
        Probe {
            env_socket: env_socket.map(str::to_string),
            home: home.map(Path::to_path_buf),
            xdg_runtime_dir: xdg.map(str::to_string),
        }
    }

    #[test]
    fn candidate_sockets_priority_order() {
        let home = PathBuf::from("/home/u");
        let p = probe(
            Some("unix:///custom/docker.sock"),
            Some(&home),
            Some("/run/user/1000"),
        );
        let candidates = candidate_sockets(&p);
        let paths: Vec<_> = candidates.iter().map(|(_, p)| p.clone()).collect();
        assert_eq!(paths[0], PathBuf::from("/custom/docker.sock"));
        assert_eq!(paths[1], PathBuf::from("/var/run/docker.sock"));
        assert_eq!(paths[2], home.join(".docker/run/docker.sock"));
        assert_eq!(paths[3], home.join(".orbstack/run/docker.sock"));
        assert_eq!(paths[4], home.join(".rd/docker.sock"));
        assert_eq!(paths[5], PathBuf::from("/run/user/1000/podman/podman.sock"));
        assert_eq!(
            paths[6],
            home.join(".local/share/containers/podman/machine/podman.sock")
        );
        assert_eq!(candidates[5].0, RuntimeKind::Podman);
        assert_eq!(candidates[0].0, RuntimeKind::Docker);
    }

    #[test]
    fn candidate_sockets_skips_empty_env_socket() {
        let candidates = candidate_sockets(&probe(Some(""), None, None));
        assert_eq!(candidates[0].1, PathBuf::from("/var/run/docker.sock"));
    }

    #[test]
    fn detect_from_prefers_socket_over_cli() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("podman.sock");
        std::fs::write(&sock, b"").unwrap();
        let candidates = vec![
            (RuntimeKind::Docker, dir.path().join("missing-docker.sock")),
            (RuntimeKind::Podman, sock.clone()),
        ];
        let info = detect_from(&candidates, None).expect("socket should be detected");
        assert_eq!(info.kind, RuntimeKind::Podman);
        assert_eq!(info.socket, Some(sock));
        assert_eq!(info.cli_path, None);
        assert_eq!(info.version, None);
    }

    #[test]
    fn detect_from_returns_none_without_socket_or_cli() {
        let dir = tempfile::tempdir().unwrap();
        let candidates = vec![(RuntimeKind::Docker, dir.path().join("missing.sock"))];
        let empty_path = dir.path().join("empty-bin");
        std::fs::create_dir_all(&empty_path).unwrap();
        let info = detect_from(&candidates, Some(empty_path.to_str().unwrap()));
        assert!(info.is_none());
    }

    #[cfg(unix)]
    fn write_cli_stub(dir: &Path, name: &str, stdout: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho \"{stdout}\"\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    #[cfg(unix)]
    fn detect_from_falls_back_to_cli_probe() {
        let dir = tempfile::tempdir().unwrap();
        let stub = write_cli_stub(dir.path(), "docker", "Docker version 27.3.1, build abc123");
        let candidates = vec![(RuntimeKind::Docker, dir.path().join("missing.sock"))];
        let info = detect_from(&candidates, Some(dir.path().to_str().unwrap()))
            .expect("CLI should be detected");
        assert_eq!(info.kind, RuntimeKind::Docker);
        assert_eq!(info.cli_path, Some(stub));
        assert_eq!(info.socket, None);
        assert_eq!(info.version, Some("27.3.1".to_string()));
    }

    #[test]
    #[cfg(unix)]
    fn detect_from_socket_resolves_matching_cli() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("docker.sock");
        std::fs::write(&sock, b"").unwrap();
        let stub = write_cli_stub(dir.path(), "docker", "Docker version 26.0.0, build def");
        let candidates = vec![(RuntimeKind::Docker, sock.clone())];
        let info = detect_from(&candidates, Some(dir.path().to_str().unwrap()))
            .expect("socket should be detected");
        assert_eq!(info.kind, RuntimeKind::Docker);
        assert_eq!(info.socket, Some(sock));
        assert_eq!(info.cli_path, Some(stub));
        assert_eq!(info.version, Some("26.0.0".to_string()));
    }

    #[test]
    #[cfg(unix)]
    fn find_in_path_skips_non_executable_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("docker"), b"not executable").unwrap();
        assert_eq!(
            find_in_path("docker", Some(dir.path().to_str().unwrap())),
            None
        );
    }

    #[test]
    fn find_in_path_handles_missing_path() {
        assert_eq!(find_in_path("docker", None), None);
    }

    #[test]
    fn parse_cli_version_docker_and_podman() {
        assert_eq!(
            parse_cli_version("Docker version 27.3.1, build ce12230"),
            Some("27.3.1".to_string())
        );
        assert_eq!(
            parse_cli_version("podman version 5.2.0"),
            Some("5.2.0".to_string())
        );
        assert_eq!(parse_cli_version("garbage output"), None);
    }

    #[test]
    fn detect_runtime_is_cached() {
        let a = detect_runtime().map(|i| i as *const RuntimeInfo);
        let b = detect_runtime().map(|i| i as *const RuntimeInfo);
        assert_eq!(a, b);
    }
}
