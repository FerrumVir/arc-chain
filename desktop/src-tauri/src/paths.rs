use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::OnceLock;

static HOME_FALLBACK: OnceLock<PathBuf> = OnceLock::new();

pub fn set_home_fallback(path: PathBuf) {
    let _ = HOME_FALLBACK.set(path);
}

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

pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if s == "~" {
        return home_dir();
    }
    PathBuf::from(s)
}

pub fn display_with_home_redacted(path: &Path) -> String {
    redact_home_in(path, &home_dir(), HOME_PATHS_IGNORE_CASE)
}

const HOME_PATHS_IGNORE_CASE: bool = cfg!(any(target_os = "macos", target_os = "ios", windows));

fn redact_home_in(path: &Path, home: &Path, ignore_case: bool) -> String {
    if home.is_absolute() {
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

pub fn redact_home_text(text: &str) -> String {
    redact_home_text_at(text, &home_dir().to_string_lossy(), HOME_PATHS_IGNORE_CASE)
}

fn redact_home_text_at(text: &str, home: &str, ignore_case: bool) -> String {
    if home.len() < 2 || home == "." {
        return text.into();
    }
    let home = home.trim_end_matches(['/', '\\']);
    let mut result = text.to_owned();
    let slash_home = home.replace('\\', "/");
    let backslash_home = home.replace('/', "\\");
    let prefixes = if ignore_case {
        vec![
            format!(r"\\?\{backslash_home}"),
            format!("//?/{slash_home}"),
            backslash_home,
            slash_home,
            home.to_owned(),
        ]
    } else {
        vec![home.to_owned()]
    };
    for prefix in prefixes {
        let mut at = 0;
        loop {
            let haystack = if ignore_case {
                result.to_ascii_lowercase()
            } else {
                result.clone()
            };
            let needle = if ignore_case {
                prefix.to_ascii_lowercase()
            } else {
                prefix.clone()
            };
            let Some(found) = haystack[at..].find(&needle) else {
                break;
            };
            let start = at + found;
            let end = start + prefix.len();
            if result.is_char_boundary(start)
                && result.is_char_boundary(end)
                && result[end..].chars().next().map_or(true, |c| {
                    c.is_whitespace()
                        || matches!(
                            c,
                            '/' | '\\'
                                | ':'
                                | '.'
                                | ','
                                | ')'
                                | ']'
                                | '}'
                                | ';'
                                | '!'
                                | '?'
                                | '\''
                                | '"'
                        )
                })
            {
                result.replace_range(start..end, "~");
                at = start + 1;
            } else {
                at = end;
            }
        }
    }
    result
}

pub fn arc_home() -> PathBuf {
    home_dir().join(".arc")
}

pub fn local_host(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_text_hides_home_and_windows_verbatim_prefix() {
        assert_eq!(
            redact_home_text_at("failed /Users/person/.arc/data", "/Users/person", false),
            "failed ~/.arc/data"
        );
        assert_eq!(
            redact_home_text_at(r"failed \\?\C:\Users\Person\.arc", r"C:\Users\Person", true),
            r"failed ~\.arc"
        );
        assert_eq!(
            redact_home_text_at("/Users/person2/data", "/Users/person", false),
            "/Users/person2/data"
        );
    }

    #[test]
    fn diagnostic_home_redaction_covers_punctuation_and_windows_separators() {
        for suffix in [".", ",", ")", "]", "}", ";", "!", "?", "\t", "\r\n"] {
            assert_eq!(
                redact_home_text_at(
                    &format!("failed /custom/person{suffix}"),
                    "/custom/person",
                    false
                ),
                format!("failed ~{suffix}")
            );
        }
        for path in [
            "C:/Users/Person/.arc",
            r"C:\Users\Person\.arc",
            "//?/C:/Users/Person/.arc",
        ] {
            let result = redact_home_text_at(path, r"C:\Users\Person", true);
            assert!(result.starts_with('~'), "{result}");
            assert!(!result.contains("Person"));
        }
    }

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
        assert_eq!(
            redact_home_in(&model, &home, true),
            format!("~{sep}.arc{sep}standard.gguf")
        );
        assert_eq!(
            redact_home_in(&model, &home, false),
            model.display().to_string()
        );
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn redaction_matches_the_home_directory_in_any_case() {
        let home = home_dir();
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
        let other_drive = Path::new(r"D:\Users\Operator\standard.gguf");
        assert_eq!(
            redact_home_in(other_drive, &home, true),
            other_drive.display().to_string()
        );
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
        if home_dir().is_absolute() && home_dir().parent().is_some() {
            assert!(shown.starts_with('~'), "{shown}");
            assert!(
                !shown.contains(&home_dir().display().to_string()),
                "the home directory must not appear: {shown}"
            );
        }
    }
}
