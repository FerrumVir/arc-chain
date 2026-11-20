#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::Path;

pub const WRONG_LOCATION: &str =
    "ARC can't update itself from where it's running. Quit ARC, drag ARC Node into your Applications folder, then open it from there.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrongLocation {
    Translocated,
    ReadOnly,
    OtherVolume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BundleVolume {
    pub read_only: bool,
    pub device: u64,
    pub temp_device: u64,
}

pub fn classify(bundle: &Path, volume: BundleVolume) -> Option<WrongLocation> {
    if bundle
        .components()
        .any(|part| part.as_os_str() == "AppTranslocation")
    {
        Some(WrongLocation::Translocated)
    } else if volume.read_only {
        Some(WrongLocation::ReadOnly)
    } else if volume.device != volume.temp_device {
        Some(WrongLocation::OtherVolume)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
pub fn refusal() -> Option<&'static str> {
    let problem = tauri::utils::platform::current_exe()
        .map_err(anyhow::Error::from)
        .and_then(|executable| problem_for(&executable, &std::env::temp_dir()));
    match problem {
        Ok(None) => None,
        Ok(Some(problem)) => {
            tracing::warn!(
                ?problem,
                "the signed app update can't replace ARC where it runs"
            );
            Some(WRONG_LOCATION)
        }
        Err(error) => {
            tracing::warn!(%error, "could not check where ARC runs; refusing the in-place update");
            Some(WRONG_LOCATION)
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn refusal() -> Option<&'static str> {
    None
}

#[cfg(target_os = "macos")]
fn problem_for(executable: &Path, temp_dir: &Path) -> anyhow::Result<Option<WrongLocation>> {
    use std::os::unix::fs::MetadataExt as _;

    let bundle = tauri_plugin_updater::extract_path_from_executable(executable)?;
    let volume = BundleVolume {
        read_only: mounted_read_only(&bundle)?,
        device: std::fs::metadata(&bundle)?.dev(),
        temp_device: std::fs::metadata(temp_dir)?.dev(),
    };
    Ok(classify(&bundle, volume))
}

#[cfg(target_os = "macos")]
fn mounted_read_only(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte")
    })?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_flags & libc::MNT_RDONLY as u32) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINE: BundleVolume = BundleVolume {
        read_only: false,
        device: 16_777_230,
        temp_device: 16_777_230,
    };

    #[test]
    fn a_translocated_copy_is_refused_even_on_the_temporary_directorys_volume() {
        let bundle = Path::new(
            "/private/var/folders/zz/abc123/T/AppTranslocation/0A1B2C3D-4E5F-4789-ABCD-EF0123456789/d/ARC Node.app",
        );
        assert_eq!(classify(bundle, FINE), Some(WrongLocation::Translocated));
        let mount = BundleVolume {
            read_only: true,
            device: 16_777_244,
            ..FINE
        };
        assert_eq!(classify(bundle, mount), Some(WrongLocation::Translocated));
    }

    #[test]
    fn a_read_only_volume_is_refused() {
        let bundle = Path::new("/Volumes/ARC Node/ARC Node.app");
        let image = BundleVolume {
            read_only: true,
            device: 16_777_240,
            ..FINE
        };
        assert_eq!(classify(bundle, image), Some(WrongLocation::ReadOnly));
        let same_device = BundleVolume {
            read_only: true,
            ..FINE
        };
        assert_eq!(classify(bundle, same_device), Some(WrongLocation::ReadOnly));
    }

    #[test]
    fn another_volume_than_the_temporary_directory_is_refused() {
        let bundle = Path::new("/Volumes/External/Applications/ARC Node.app");
        let external = BundleVolume {
            device: 16_777_241,
            ..FINE
        };
        assert_eq!(classify(bundle, external), Some(WrongLocation::OtherVolume));
    }

    #[test]
    fn applications_on_the_temporary_directorys_volume_is_fine() {
        assert_eq!(
            classify(Path::new("/Applications/ARC Node.app"), FINE),
            None
        );
        assert_eq!(
            classify(Path::new("/Users/me/Applications/ARC Node.app"), FINE),
            None
        );
        assert_eq!(
            classify(
                Path::new("/Users/me/AppTranslocationNotes/ARC Node.app"),
                FINE
            ),
            None
        );
    }

    #[test]
    fn a_parent_folder_the_user_cannot_write_to_is_allowed() {
        assert_eq!(
            classify(Path::new("/Applications/ARC Node.app"), FINE),
            None
        );
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let root = tempfile::tempdir().unwrap();
            let parent = root.path().join("Applications");
            let bundle = parent.join("ARC Node.app");
            let executable = bundle.join("Contents/MacOS/arc-desktop");
            std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
            std::fs::write(&executable, b"").unwrap();
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
            let problem = problem_for(&executable, &std::env::temp_dir());
            let moved = std::fs::rename(&bundle, root.path().join("current_app"));
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(problem.unwrap(), None);
            if unsafe { libc::geteuid() } != 0 {
                assert_eq!(
                    moved.unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_bundle_is_the_path_the_updater_replaces() {
        assert_eq!(
            tauri_plugin_updater::extract_path_from_executable(Path::new(
                "/Applications/ARC Node.app/Contents/MacOS/arc-desktop"
            ))
            .unwrap(),
            Path::new("/Applications/ARC Node.app")
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn only_macos_is_checked() {
        assert_eq!(refusal(), None);
    }
}
