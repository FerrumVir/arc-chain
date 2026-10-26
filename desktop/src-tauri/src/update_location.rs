//! Whether the signed app update can replace ARC where it runs. Checked on
//! macOS at launch and before either update path stops the node or takes
//! its lifecycle fence.
//!
//! tauri-plugin-updater 2.10's macOS install (`Update::install_inner`)
//! extracts the new bundle into a fresh directory under
//! `std::env::temp_dir()`, then renames the running bundle into a second one
//! there. A rename refused with PermissionDenied asks for an administrator's
//! password and moves the bundles as root; any other refusal fails the
//! install. By then the node is stopped and the one-way handoff has begun, so
//! the failed install keeps the node stopped until ARC exits, and the same
//! copy offers the same update again when it reopens.
//!
//! The rename is refused that way when the bundle runs:
//! - under App Translocation, the randomized read-only mount Gatekeeper runs
//!   a quarantined app from when it is opened where it was downloaded;
//! - from a read-only volume, such as the mounted .dmg (EROFS);
//! - from another volume than the temporary directory (EXDEV).
//!
//! A parent folder this user can't write to is not refused: there the rename
//! fails with PermissionDenied, and the updater asks for an administrator, as
//! for any app in a shared /Applications.
//!
//! The classifier is pure, so every case is tested on every OS. Only the
//! probe reads the filesystem, and only macOS calls it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::Path;

/// What an app update is told when the updater can't replace the bundle
/// where it runs. `update-controller.ts` recognises it, and Home and Settings
/// show the same sentence from `plain-copy.ts` (`updates.wrongLocation`).
pub const WRONG_LOCATION: &str =
    "ARC can't update itself from where it's running. Quit ARC, drag ARC Node into your Applications folder, then open it from there.";

/// Why the updater's rename can't move the running bundle aside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrongLocation {
    /// App Translocation runs a randomized read-only copy.
    Translocated,
    /// A read-only volume, such as the mounted .dmg.
    ReadOnly,
    /// Another volume than the temporary directory.
    OtherVolume,
}

/// What that rename depends on, as the probe reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BundleVolume {
    /// `statfs` reports the bundle's volume mounted read-only (`MNT_RDONLY`).
    pub read_only: bool,
    /// `st_dev` of the bundle.
    pub device: u64,
    /// `st_dev` of `std::env::temp_dir()`, where the updater moves it.
    pub temp_device: u64,
}

/// Why the updater can't replace `bundle` where it is, or None when it can,
/// perhaps after asking for an administrator. Whether this user can write to
/// the parent folder is deliberately not an input.
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

/// The refusal for an app update from where this process runs, or None when
/// the updater can replace its bundle there. A probe that can't read the
/// bundle or the temporary directory refuses too: the install would fail on
/// the same paths, after the node stopped.
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

/// Only macOS is checked.
#[cfg(not(target_os = "macos"))]
pub fn refusal() -> Option<&'static str> {
    None
}

/// The problem for `executable`'s bundle. The bundle is the path the updater
/// replaces, derived the way its builder derives it (`UpdaterBuilder::build`
/// in 2.10): Tauri's starting binary, then `extract_path_from_executable`.
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

/// `statfs(2)` reports `MNT_RDONLY` for the volume holding `path`.
#[cfg(target_os = "macos")]
fn mounted_read_only(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte")
    })?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and outlives the call; `stats` points
    // to writable storage for exactly the struct statfs fills.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: statfs returned 0, so it initialized `stats`.
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_flags & libc::MNT_RDONLY as u32) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundle and the temporary directory on one writable volume.
    const FINE: BundleVolume = BundleVolume {
        read_only: false,
        device: 16_777_230,
        temp_device: 16_777_230,
    };

    #[test]
    fn a_translocated_copy_is_refused_even_on_the_temporary_directorys_volume() {
        // Gatekeeper's mount for a quarantined app opened from Downloads.
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
        // The mounted .dmg is read-only and its own device.
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
        // Only the mount's own path component counts, not a name containing it.
        assert_eq!(
            classify(
                Path::new("/Users/me/AppTranslocationNotes/ARC Node.app"),
                FINE
            ),
            None
        );
    }

    /// A shared /Applications this user can't write to. The updater's rename
    /// is refused for permissions there, which is what makes it ask for an
    /// administrator, so the location is fine: a refusal would block the one
    /// install that works.
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
            // The updater's own first move, from a folder this user can't write.
            let moved = std::fs::rename(&bundle, root.path().join("current_app"));
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(problem.unwrap(), None);
            // Root may move it anyway; anyone else is refused for permissions.
            if unsafe { libc::geteuid() } != 0 {
                assert_eq!(
                    moved.unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
            }
        }
    }

    /// The bundle is the path the updater replaces, not the executable.
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
