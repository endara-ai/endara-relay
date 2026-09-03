//! Beneath-root path opens for the sandbox file primitives (unix only).
//!
//! [`resolve_write_path`](super::resolve_write_path) canonicalizes a path up
//! front, so by the time the sandbox touches the filesystem every existing
//! component is known to be a real directory inside an allowlisted root. A
//! plain `open(2)` of that canonical path still re-resolves each component,
//! so a symlink swapped in for any of them between canonicalize and open
//! would be followed — possibly out of the root. The helpers here anchor the
//! open to a directory handle of the root and refuse to follow any symlink,
//! which turns that swap into a hard error instead of an escape.
//!
//! Two strategies share one contract:
//! - Linux ≥ 5.6: a single `openat2(2)` with `RESOLVE_BENEATH |
//!   RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`, enforced by the kernel.
//!   `ENOSYS`/`EINVAL` (old kernel, seccomp) is remembered once and every
//!   later call takes the walk.
//! - Everywhere else (macOS, old Linux): a component-by-component `openat(2)`
//!   walk. Intermediates are opened `O_DIRECTORY | O_NOFOLLOW`, the final
//!   component with the caller's flags plus `O_NOFOLLOW`, so a symlink at any
//!   depth fails with `ELOOP`. Every step is relative to the previous
//!   directory handle, so no lookup ever re-resolves an ancestor: a component
//!   swapped for a symlink is caught, and `..`/absolute components never
//!   reach the kernel. One caveat inherent to `openat` walks: a directory
//!   that has already been opened and is then renamed out of the root stays
//!   pinned, and the remaining components resolve inside it wherever it now
//!   lives. `openat2` additionally detects that case (`EXDEV`); the walk
//!   cannot, though it still never follows a symlink or a `..`.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::io::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::path::{Component, Path};

/// `O_CLOEXEC` is always added so the handles never leak into spawned MCP
/// server processes.
const INTERMEDIATE_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// Open the directory at `path` as an anchor for the beneath-root helpers.
/// `path` is a trusted (canonical) allowlisted root.
pub(super) fn open_root(path: &Path) -> io::Result<OwnedFd> {
    let c_path = c_string(path.as_os_str())?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened descriptor exclusively owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open `rel_path` beneath `root`, refusing to follow a symlink at any
/// component. `rel_path` must be a non-empty relative path made of plain
/// components only (no `.`/`..`/root); anything else is rejected with
/// `InvalidInput` before any syscall. `final_flags` are the `open(2)` flags
/// for the last component (`O_CLOEXEC` and `O_NOFOLLOW` are always added);
/// `O_CREAT` is not supported — create files through the directory handle
/// returned by [`open_dir_beneath_creating`] instead. A symlink component
/// surfaces as `ELOOP`, on both the `openat2` and the walk paths.
pub(super) fn open_beneath(
    root: BorrowedFd<'_>,
    rel_path: &Path,
    final_flags: libc::c_int,
) -> io::Result<File> {
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    let components = validated_open_components(rel_path, final_flags)?;
    #[cfg(target_os = "linux")]
    {
        // Rebuild the path from the validated components so `openat2` sees
        // exactly what the walk would (no trailing `/`, no `//`, no `./`).
        let normalized: std::path::PathBuf = components.iter().collect();
        if let Some(result) = openat2::open(root, &normalized, final_flags) {
            return result;
        }
    }
    open_beneath_walk(root, rel_path, final_flags)
}

/// The portable `openat(2)` walk behind [`open_beneath`]. Exposed separately
/// so the fallback is testable on kernels where `openat2` is available.
pub(super) fn open_beneath_walk(
    root: BorrowedFd<'_>,
    rel_path: &Path,
    final_flags: libc::c_int,
) -> io::Result<File> {
    let components = validated_open_components(rel_path, final_flags)?;
    let (last, parents) = components
        .split_last()
        .expect("validated_open_components rejects empty paths");
    let mut dir: Option<OwnedFd> = None;
    for name in parents {
        let at = dir.as_ref().map_or(root.as_raw_fd(), |d| d.as_raw_fd());
        dir = Some(open_intermediate(at, name)?);
    }
    let at = dir.as_ref().map_or(root.as_raw_fd(), |d| d.as_raw_fd());
    let fd = openat(
        at,
        last,
        final_flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    Ok(File::from(fd))
}

/// Open one intermediate component as a directory without following
/// symlinks. Linux reports a symlink here as `ENOTDIR` (the link itself is
/// not a directory once `O_NOFOLLOW` stops resolution); that case is
/// normalised to `ELOOP` so callers see the same error the final component,
/// `openat2`, and macOS produce.
fn open_intermediate(dirfd: libc::c_int, name: &OsStr) -> io::Result<OwnedFd> {
    match openat(dirfd, name, INTERMEDIATE_FLAGS, 0) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) && is_symlink_at(dirfd, name) => {
            Err(io::Error::from_raw_os_error(libc::ELOOP))
        }
        other => other,
    }
}

fn is_symlink_at(dirfd: libc::c_int, name: &OsStr) -> bool {
    let Ok(c_name) = c_string(name) else {
        return false;
    };
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `dirfd` is an open descriptor, `c_name` a valid C string, and
    // `st` is only read after fstatat reports success.
    let rc = unsafe {
        libc::fstatat(
            dirfd,
            c_name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return false;
    }
    // SAFETY: fstatat succeeded, so `st` is initialised.
    let st = unsafe { st.assume_init() };
    st.st_mode & libc::S_IFMT == libc::S_IFLNK
}

/// [`validated_components`] plus the file-open rules: the path must name at
/// least one component and `final_flags` must not ask for `O_CREAT`.
fn validated_open_components(rel_path: &Path, final_flags: libc::c_int) -> io::Result<Vec<&OsStr>> {
    if final_flags & libc::O_CREAT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "open_beneath does not support O_CREAT",
        ));
    }
    let components = validated_components(rel_path)?;
    if components.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "open_beneath requires a non-empty relative path",
        ));
    }
    Ok(components)
}

/// Walk `rel_dir` beneath `root` like [`open_beneath_walk`], creating any
/// missing directory with `mkdirat(2)` (mode `0o777`, subject to umask) and
/// returning a handle to the final directory. `writeFile` creates its temp
/// file and `renameat`s it relative to that handle, so nothing it does can
/// land outside the root. An empty `rel_dir` returns a fresh handle to `root`
/// itself. Pre-existing symlinks in the chain are rejected with `ELOOP`, so
/// this always uses the walk (there is no create-parents `openat2`).
pub(super) fn open_dir_beneath_creating(
    root: BorrowedFd<'_>,
    rel_dir: &Path,
) -> io::Result<OwnedFd> {
    let components = validated_components(rel_dir)?;
    let mut dir = openat(root.as_raw_fd(), OsStr::new("."), INTERMEDIATE_FLAGS, 0)?;
    for name in &components {
        dir = match open_intermediate(dir.as_raw_fd(), name) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let c_name = c_string(name)?;
                // SAFETY: `dir` is an open directory fd and `c_name` a valid C string.
                let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), c_name.as_ptr(), 0o777) };
                if rc < 0 {
                    let e = io::Error::last_os_error();
                    // Lost a race with a concurrent creator: the open below
                    // decides whether what appeared is an acceptable directory.
                    if e.kind() != io::ErrorKind::AlreadyExists {
                        return Err(e);
                    }
                }
                open_intermediate(dir.as_raw_fd(), name)?
            }
            Err(e) => return Err(e),
        };
    }
    Ok(dir)
}

/// Split `rel_path` into its plain components, rejecting absolute paths,
/// `.`/`..`, and NUL bytes. Canonical paths from `resolve_write_path` never
/// contain these; rejecting them here keeps the helper safe on its own.
fn validated_components(rel_path: &Path) -> io::Result<Vec<&OsStr>> {
    let mut out = Vec::new();
    for component in rel_path.components() {
        match component {
            Component::Normal(name) => {
                if name.as_bytes().contains(&0) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "path component contains a NUL byte",
                    ));
                }
                out.push(name);
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "path must be relative with plain components only, found {:?}",
                        other.as_os_str()
                    ),
                ));
            }
        }
    }
    Ok(out)
}

pub(super) fn c_string(s: &OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// `openat(2)` of `name` relative to `dirfd`; the caller supplies every flag
/// (including `O_CLOEXEC`/`O_NOFOLLOW`) and the creation `mode`.
pub(super) fn openat(
    dirfd: libc::c_int,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<OwnedFd> {
    let c_name = c_string(name)?;
    // SAFETY: `dirfd` is an open descriptor owned by the caller and `c_name`
    // a valid NUL-terminated string that outlives the call.
    let fd = unsafe { libc::openat(dirfd, c_name.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened descriptor exclusively owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
mod openat2 {
    use std::fs::File;
    use std::io;
    use std::os::unix::io::{AsRawFd as _, BorrowedFd, FromRawFd as _};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::c_string;

    /// `struct open_how` from `linux/openat2.h`. Defined locally because
    /// glibc has no `openat2` wrapper and libc's copy is `#[non_exhaustive]`.
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    /// Set once `openat2` reports `ENOSYS`/`EINVAL`, so the probe is paid
    /// once per process and every later open goes straight to the walk.
    static UNSUPPORTED: AtomicBool = AtomicBool::new(false);

    /// `Some(result)` when `openat2` handled the open, `None` when the
    /// syscall is unavailable and the caller must fall back to the walk.
    pub(super) fn open(
        root: BorrowedFd<'_>,
        rel_path: &Path,
        final_flags: libc::c_int,
    ) -> Option<io::Result<File>> {
        if UNSUPPORTED.load(Ordering::Relaxed) {
            return None;
        }
        let c_path = match c_string(rel_path.as_os_str()) {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        let flags = final_flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let how = OpenHow {
            flags: u64::from(flags as u32),
            mode: 0,
            resolve: libc::RESOLVE_BENEATH
                | libc::RESOLVE_NO_SYMLINKS
                | libc::RESOLVE_NO_MAGICLINKS,
        };
        // SAFETY: `root` is an open directory fd, `c_path` a valid C string and
        // `how` a correctly sized, initialised `struct open_how`; all outlive
        // the call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                c_path.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if matches!(e.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EINVAL)) {
                UNSUPPORTED.store(true, Ordering::Relaxed);
                return None;
            }
            return Some(Err(e));
        }
        // SAFETY: `fd` is a freshly opened descriptor exclusively owned here.
        Some(Ok(unsafe { File::from_raw_fd(fd as libc::c_int) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::symlink;
    use std::os::unix::io::AsFd as _;

    const READ_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY;

    type Opener = fn(BorrowedFd<'_>, &Path, libc::c_int) -> io::Result<File>;

    /// Every open scenario is run through both the public entry point (which
    /// takes `openat2` where the kernel offers it) and the walk directly, so
    /// the fallback is covered even on kernels that never fall back.
    const OPENERS: [(&str, Opener); 2] = [
        ("open_beneath", open_beneath),
        ("open_beneath_walk", open_beneath_walk),
    ];

    fn read_to_string(mut file: File) -> String {
        let mut s = String::new();
        file.read_to_string(&mut s).unwrap();
        s
    }

    fn raw_os_error(name: &str, result: io::Result<File>) -> i32 {
        match result {
            Ok(_) => panic!("{}: expected an error, got a handle", name),
            Err(e) => e
                .raw_os_error()
                .unwrap_or_else(|| panic!("{}: not an OS error: {}", name, e)),
        }
    }

    /// Root containing `a/b/c.txt` (= "nested") and `top.txt` (= "top").
    fn nested_root() -> (tempfile::TempDir, OwnedFd) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/c.txt"), "nested").unwrap();
        std::fs::write(dir.path().join("top.txt"), "top").unwrap();
        let root = open_root(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn test_open_beneath_opens_nested_regular_file() {
        let (_dir, root) = nested_root();
        for (name, opener) in OPENERS {
            let file = opener(root.as_fd(), Path::new("a/b/c.txt"), READ_FLAGS)
                .unwrap_or_else(|e| panic!("{}: {}", name, e));
            assert!(file.metadata().unwrap().is_file(), "{}", name);
            assert_eq!(read_to_string(file), "nested", "{}", name);
            let top = opener(root.as_fd(), Path::new("top.txt"), READ_FLAGS).unwrap();
            assert_eq!(read_to_string(top), "top", "{}", name);
            // Spellings `Path::components` normalizes away must behave the
            // same on both openers (the openat2 path is rebuilt from the
            // validated components rather than passed through verbatim).
            for spelling in ["a/b/c.txt/", "a//b/c.txt", "a/./b/c.txt"] {
                let file = opener(root.as_fd(), Path::new(spelling), READ_FLAGS)
                    .unwrap_or_else(|e| panic!("{}: {:?}: {}", name, spelling, e));
                assert_eq!(read_to_string(file), "nested", "{}: {:?}", name, spelling);
            }
        }
    }

    #[test]
    fn test_open_beneath_missing_file_is_not_found() {
        let (_dir, root) = nested_root();
        for (name, opener) in OPENERS {
            let e = opener(root.as_fd(), Path::new("a/b/missing.txt"), READ_FLAGS).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::NotFound, "{}: {}", name, e);
            let e = opener(root.as_fd(), Path::new("nope/c.txt"), READ_FLAGS).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::NotFound, "{}: {}", name, e);
        }
    }

    /// A symlink as the FIRST component is rejected even when it points to a
    /// directory inside the same root: the path was canonical, so any symlink
    /// is a post-canonicalize swap.
    #[test]
    fn test_open_beneath_rejects_symlink_at_first_component() {
        let (dir, root) = nested_root();
        symlink(dir.path().join("a"), dir.path().join("link")).unwrap();
        for (name, opener) in OPENERS {
            let errno = raw_os_error(
                name,
                opener(root.as_fd(), Path::new("link/b/c.txt"), READ_FLAGS),
            );
            assert_eq!(errno, libc::ELOOP, "{}", name);
        }
    }

    #[test]
    fn test_open_beneath_rejects_symlink_at_middle_component() {
        let (dir, root) = nested_root();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("c.txt"), "outside").unwrap();
        symlink(outside.path(), dir.path().join("a/out")).unwrap();
        for (name, opener) in OPENERS {
            let errno = raw_os_error(
                name,
                opener(root.as_fd(), Path::new("a/out/c.txt"), READ_FLAGS),
            );
            assert_eq!(errno, libc::ELOOP, "{}", name);
        }
    }

    #[test]
    fn test_open_beneath_rejects_symlink_at_final_component() {
        let (dir, root) = nested_root();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "outside").unwrap();
        symlink(
            outside.path().join("secret"),
            dir.path().join("a/b/escape.txt"),
        )
        .unwrap();
        symlink("c.txt", dir.path().join("a/b/inside.txt")).unwrap();
        symlink("dangling", dir.path().join("a/b/dangling.txt")).unwrap();
        for (name, opener) in OPENERS {
            for target in ["a/b/escape.txt", "a/b/inside.txt", "a/b/dangling.txt"] {
                let errno = raw_os_error(name, opener(root.as_fd(), Path::new(target), READ_FLAGS));
                assert_eq!(errno, libc::ELOOP, "{}: {}", name, target);
            }
        }
    }

    #[test]
    fn test_open_beneath_rejects_non_plain_relative_paths() {
        let (dir, root) = nested_root();
        for (name, opener) in OPENERS {
            for bad in [
                "/etc/passwd",
                "../a/b/c.txt",
                "a/../top.txt",
                "./top.txt",
                "",
            ] {
                let e = opener(root.as_fd(), Path::new(bad), READ_FLAGS).unwrap_err();
                assert_eq!(
                    e.kind(),
                    io::ErrorKind::InvalidInput,
                    "{}: {:?}: {}",
                    name,
                    bad,
                    e
                );
            }
            let e = opener(
                root.as_fd(),
                Path::new("a/b/new.txt"),
                READ_FLAGS | libc::O_CREAT,
            )
            .unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{}", name);
            assert!(!dir.path().join("a/b/new.txt").exists(), "{}", name);
        }
    }

    /// The walk stays anchored to the handle chain: a `..` smuggled in as a
    /// raw component never reaches `openat` because validation rejects it,
    /// and an absolute component (which `openat` would treat as absolute)
    /// is likewise rejected up front.
    #[test]
    fn test_open_beneath_walk_rejects_paths_before_any_syscall() {
        let (_dir, root) = nested_root();
        let e = open_beneath_walk(
            root.as_fd(),
            Path::new("a/b/../../../etc/passwd"),
            READ_FLAGS,
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        let e = open_beneath_walk(root.as_fd(), Path::new("/"), READ_FLAGS).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_openat2_fast_path_matches_walk_when_available() {
        let (dir, root) = nested_root();
        symlink(dir.path().join("a"), dir.path().join("link")).unwrap();
        // `None` means a kernel without openat2: the fallback is what
        // open_beneath uses and is exercised directly by the other tests.
        if let Some(result) = openat2::open(root.as_fd(), Path::new("a/b/c.txt"), READ_FLAGS) {
            assert_eq!(read_to_string(result.unwrap()), "nested");
            let via_link = openat2::open(root.as_fd(), Path::new("link/b/c.txt"), READ_FLAGS)
                .expect("openat2 still available")
                .unwrap_err();
            assert_eq!(via_link.raw_os_error(), Some(libc::ELOOP));
        }
    }

    fn create_file_at(dir: &OwnedFd, name: &str, contents: &str) {
        let fd = openat(
            dir.as_raw_fd(),
            OsStr::new(name),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o644,
        )
        .unwrap();
        File::from(fd).write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn test_open_dir_beneath_creating_creates_missing_chain_and_returns_usable_dirfd() {
        let (dir, root) = nested_root();
        let leaf = open_dir_beneath_creating(root.as_fd(), Path::new("a/new/deeper/leaf")).unwrap();
        assert!(dir.path().join("a/new/deeper/leaf").is_dir());
        create_file_at(&leaf, "f.txt", "via dirfd");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/new/deeper/leaf/f.txt")).unwrap(),
            "via dirfd"
        );

        // Second walk over the now-existing chain succeeds without recreating.
        let again =
            open_dir_beneath_creating(root.as_fd(), Path::new("a/new/deeper/leaf")).unwrap();
        create_file_at(&again, "g.txt", "again");
        assert!(dir.path().join("a/new/deeper/leaf/g.txt").is_file());
    }

    #[test]
    fn test_open_dir_beneath_creating_empty_path_returns_root_handle() {
        let (dir, root) = nested_root();
        let handle = open_dir_beneath_creating(root.as_fd(), Path::new("")).unwrap();
        create_file_at(&handle, "at_root.txt", "root");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("at_root.txt")).unwrap(),
            "root"
        );
    }

    #[test]
    fn test_open_dir_beneath_creating_rejects_symlink_components() {
        let (dir, root) = nested_root();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("a/out")).unwrap();
        symlink(outside.path(), dir.path().join("first")).unwrap();

        for rel in ["a/out", "a/out/sub", "first/x/y"] {
            let e = open_dir_beneath_creating(root.as_fd(), Path::new(rel)).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::ELOOP), "{}: {}", rel, e);
        }
        assert!(!outside.path().join("sub").exists());
        assert!(!outside.path().join("x").exists());
    }

    #[test]
    fn test_open_dir_beneath_creating_rejects_non_plain_relative_paths() {
        let (dir, root) = nested_root();
        for bad in ["/tmp", "../x", "a/../x", "./a"] {
            let e = open_dir_beneath_creating(root.as_fd(), Path::new(bad)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{:?}: {}", bad, e);
        }
        assert!(!dir.path().join("x").exists());
    }

    #[test]
    fn test_open_dir_beneath_creating_fails_when_component_is_a_file() {
        let (_dir, root) = nested_root();
        let e = open_dir_beneath_creating(root.as_fd(), Path::new("top.txt/sub")).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOTDIR), "{}", e);
    }
}
