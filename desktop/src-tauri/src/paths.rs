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

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf, Prefix};
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

/// Show `path` with a leading home directory shortened to `~`, the inverse
/// of [`expand_tilde`]. Use it for any path written to the Logs screen:
/// people paste that screen into public bug reports, and a full home path
/// usually carries their account name.
pub fn display_with_home_redacted(path: &Path) -> String {
    redact_home_in(path, &home_dir(), HOME_PATHS_IGNORE_CASE)
}

/// macOS and Windows file systems ignore case by default, so a path can
/// reach the home directory with different capitals than `home_dir()` has.
const HOME_PATHS_IGNORE_CASE: bool = cfg!(any(target_os = "macos", target_os = "ios", windows));

fn redact_home_in(path: &Path, home: &Path, ignore_case: bool) -> String {
    // A relative fallback home (".") must never match, or every relative
    // path would be shown as if it were under the home directory.
    if home.is_absolute() {
        // Whole components are compared, so a sibling such as `/home/alice2`
        // is not shortened against a home of `/home/alice`.
        let mut rest = path.components();
        let under_home = home.components().all(|home_part| {
            rest.next().is_some_and(|part| {
                comparable(part, ignore_case) == comparable(home_part, ignore_case)
            })
        });
        if under_home {
            let rest = rest.as_path();
            return if rest.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
            };
        }
    }
    path.display().to_string()
}

/// One path component in a form that compares the way the file system does.
/// A Windows verbatim prefix (`\\?\C:`, `\\?\UNC\server\share`) equals its
/// ordinary form (`C:`, `\\server\share`), drive letters never differ by
/// case, and names ignore case where the file system does.
fn comparable(component: Component<'_>, ignore_case: bool) -> String {
    let name = |value: &OsStr| {
        let value = value.to_string_lossy();
        if ignore_case {
            value.to_lowercase()
        } else {
            value.into_owned()
        }
    };
    match component {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                format!("disk:{}", char::from(drive).to_ascii_lowercase())
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                format!("unc:{}\\{}", name(server), name(share))
            }
            Prefix::Verbatim(value) => format!("verbatim:{}", name(value)),
            Prefix::DeviceNS(value) => format!("device:{}", name(value)),
        },
        Component::RootDir => "root".to_owned(),
        Component::CurDir => ".".to_owned(),
        Component::ParentDir => "..".to_owned(),
        Component::Normal(value) => format!("name:{}", name(value)),
    }
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

    #[test]
    fn redaction_shortens_the_home_directory_to_a_tilde() {
        // An absolute fixture home on every platform, with no real account.
        let home = std::env::temp_dir()
            .join("arc-home-fixture")
            .join("operator");
        let model = home.join(".arc").join("models").join("standard.gguf");
        let sep = std::path::MAIN_SEPARATOR;
        for ignore_case in [false, true] {
            let shown = redact_home_in(&model, &home, ignore_case);
            assert_eq!(shown, format!("~{sep}.arc{sep}models{sep}standard.gguf"));
            assert!(!shown.contains("operator"), "{shown}");
            assert_eq!(redact_home_in(&home, &home, ignore_case), "~");
        }
    }

    #[test]
    fn redaction_leaves_paths_outside_the_home_directory_alone() {
        let root = std::env::temp_dir().join("arc-home-fixture");
        let home = root.join("operator");
        for ignore_case in [false, true] {
            // A sibling whose name merely starts with the home directory's name.
            let sibling = root.join("operator2").join("model.gguf");
            assert_eq!(
                redact_home_in(&sibling, &home, ignore_case),
                sibling.display().to_string()
            );
            let elsewhere = root.join("shared-models").join("model.gguf");
            assert_eq!(
                redact_home_in(&elsewhere, &home, ignore_case),
                elsewhere.display().to_string()
            );
            // A relative fallback home never matches a relative path.
            let relative = PathBuf::from("models").join("model.gguf");
            assert_eq!(
                redact_home_in(&relative, Path::new("."), ignore_case),
                relative.display().to_string()
            );
        }
    }

    #[test]
    fn redaction_ignores_case_only_where_the_file_system_does() {
        let root = std::env::temp_dir().join("arc-home-fixture");
        let home = root.join("Operator");
        let model = root.join("OPERATOR").join(".arc").join("standard.gguf");
        let sep = std::path::MAIN_SEPARATOR;
        // macOS and Windows: the same directory, so it is shortened.
        assert_eq!(
            redact_home_in(&model, &home, true),
            format!("~{sep}.arc{sep}standard.gguf")
        );
        // A case-sensitive file system: a different directory.
        assert_eq!(
            redact_home_in(&model, &home, false),
            model.display().to_string()
        );
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn redaction_matches_the_home_directory_in_any_case() {
        let home = home_dir();
        // Meaningful only for a real home below the filesystem root.
        if !home.is_absolute() || home.parent().is_none() {
            return;
        }
        for variant in [
            home.to_string_lossy().to_uppercase(),
            home.to_string_lossy().to_lowercase(),
        ] {
            let model = PathBuf::from(variant).join(".arc").join("standard.gguf");
            let shown = display_with_home_redacted(&model);
            let sep = std::path::MAIN_SEPARATOR;
            assert_eq!(shown, format!("~{sep}.arc{sep}standard.gguf"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn redaction_treats_windows_verbatim_paths_as_their_ordinary_form() {
        let home = PathBuf::from(r"C:\Users\Operator");
        assert_eq!(
            redact_home_in(
                Path::new(r"\\?\C:\Users\operator\.arc\models\standard.gguf"),
                &home,
                true
            ),
            r"~\.arc\models\standard.gguf"
        );
        assert_eq!(
            redact_home_in(
                Path::new(r"c:\users\operator\standard.gguf"),
                Path::new(r"\\?\C:\Users\Operator"),
                true
            ),
            r"~\standard.gguf"
        );
        assert_eq!(
            redact_home_in(
                Path::new(r"\\?\UNC\fileserver\homes\operator\standard.gguf"),
                Path::new(r"\\fileserver\homes\operator"),
                true
            ),
            r"~\standard.gguf"
        );
        // Another drive is another place.
        let other_drive = Path::new(r"D:\Users\Operator\standard.gguf");
        assert_eq!(
            redact_home_in(other_drive, &home, true),
            other_drive.display().to_string()
        );
        // And through the real home directory, when it is on a drive. A
        // verbatim path takes `/` literally, so only a backslash form is used.
        let real_home = home_dir();
        if matches!(real_home.components().next(), Some(Component::Prefix(_)))
            && real_home.parent().is_some()
            && !real_home.to_string_lossy().contains('/')
        {
            let verbatim =
                PathBuf::from(format!(r"\\?\{}", real_home.display())).join("standard.gguf");
            assert_eq!(display_with_home_redacted(&verbatim), r"~\standard.gguf");
        }
    }

    #[test]
    fn redaction_uses_the_resolved_home_directory() {
        let model = home_dir().join(".arc").join("models").join("standard.gguf");
        let shown = display_with_home_redacted(&model);
        // Meaningful only for a real home below the filesystem root.
        if home_dir().is_absolute() && home_dir().parent().is_some() {
            assert!(shown.starts_with('~'), "{shown}");
            assert!(
                !shown.contains(&home_dir().display().to_string()),
                "the home directory must not appear: {shown}"
            );
        }
    }
}
