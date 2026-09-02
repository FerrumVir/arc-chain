//! Cross-platform path + host helpers.
//!
//! Two separate bugs motivated this module.
//!
//! 1. `HOME` is normally unset on Windows, but four call sites resolved the
//!    user's home directory with `std::env::var_os("HOME")` alone and fell
//!    back to `PathBuf::from(".")`. On Windows that silently turned the
//!    default `~/.arc` data dir into `./.arc` relative to the GUI process's
//!    CWD — typically the install dir under Program Files, which a standard
//!    user cannot write. arc-node then failed to open its WAL with a
//!    permissions error that looked nothing like the real cause.
//!    `home_dir()` is the single resolver everything now goes through:
//!    HOME → USERPROFILE → the Tauri PathResolver value captured at startup.
//!
//! 2. The desktop conflated "my node" with "the chain". `local_host()` is the
//!    address of the arc-node child this app spawned; chain reads go
//!    somewhere else entirely (see `commands::chain_host`).

use std::path::PathBuf;
use std::sync::OnceLock;

/// Home directory resolved by Tauri's `PathResolver` during `setup()`. Used
/// only when neither `HOME` nor `USERPROFILE` is set — a rare but real case
/// for services and some Windows login shells. Set once; later sets are
/// ignored.
static HOME_FALLBACK: OnceLock<PathBuf> = OnceLock::new();

/// Record the Tauri-resolved home directory. Called once from `lib.rs`
/// `setup()`, where an `AppHandle` (and therefore a `PathResolver`) exists.
pub fn set_home_fallback(path: PathBuf) {
    let _ = HOME_FALLBACK.set(path);
}

/// The user's home directory.
///
/// Order: `HOME` (unix, and unix-y Windows shells like Git Bash) →
/// `USERPROFILE` (the Windows norm) → the Tauri-resolved fallback → `.`.
/// Empty env vars are treated as unset, because an exported-but-blank `HOME`
/// would otherwise resolve every path to the filesystem root.
pub fn home_dir() -> PathBuf {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(v) = std::env::var_os(key) {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    if let Some(p) = HOME_FALLBACK.get() {
        return p.clone();
    }
    PathBuf::from(".")
}

/// Expand a leading `~/` against [`home_dir`]. Any other string is taken
/// literally, so absolute and relative paths configured by the user are
/// passed through untouched.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if s == "~" {
        return home_dir();
    }
    PathBuf::from(s)
}

/// `~/.arc` — the root the node's data dir, managed binary and models all
/// live under by default.
pub fn arc_home() -> PathBuf {
    home_dir().join(".arc")
}

/// RPC origin of the arc-node process running on THIS machine.
///
/// Everything that answers the question "is the user's node up, and what is
/// it doing?" must go here. Reading a remote seed instead is what made the
/// Dashboard report a datacenter's health as the user's own.
pub fn local_host(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_is_never_empty() {
        assert!(!home_dir().as_os_str().is_empty());
    }

    #[test]
    fn expand_tilde_replaces_leading_home() {
        let p = expand_tilde("~/.arc");
        assert!(
            !p.starts_with("~"),
            "tilde should be expanded: {}",
            p.display()
        );
        assert!(p.ends_with(".arc"));
    }

    #[test]
    fn expand_tilde_leaves_absolute_paths_alone() {
        let p = expand_tilde("/var/lib/arc");
        assert_eq!(p, PathBuf::from("/var/lib/arc"));
    }

    #[test]
    fn expand_tilde_does_not_touch_embedded_tilde() {
        let p = expand_tilde("/opt/back~up");
        assert_eq!(p, PathBuf::from("/opt/back~up"));
    }

    #[test]
    fn local_host_is_loopback() {
        assert_eq!(local_host(9090), "http://127.0.0.1:9090");
    }
}
