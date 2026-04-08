/// Guard: returns `Some(())` if `node` and `npx` are available in PATH.
/// Prints a SKIP message and returns `None` otherwise.
#[allow(dead_code)]
pub fn require_node() -> Option<()> {
    if which::which("node").is_err() || which::which("npx").is_err() {
        eprintln!("SKIP: node/npx not found in PATH");
        return None;
    }
    Some(())
}

/// Guard: returns `Some(())` if `uvx` is available in PATH.
/// Prints a SKIP message and returns `None` otherwise.
#[allow(dead_code)]
pub fn require_uvx() -> Option<()> {
    if which::which("uvx").is_err() {
        eprintln!("SKIP: uvx not found in PATH");
        return None;
    }
    Some(())
}
