//! Permission-sealed Unix listeners used by the production RPC origin plane.
//!
//! A loopback TCP port is not an identity boundary: any local process can bind
//! a crashed service's port and impersonate it.  These helpers require an
//! owner-controlled systemd-style runtime directory, bind one exact socket,
//! set its mode to `0660`, and remove only the inode created by this process.

#![cfg(unix)]

use std::{
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
};

const MAX_UNIX_SOCKET_PATH_BYTES: usize = 100;
const RUNTIME_DIRECTORY_MODE: u32 = 0o750;
const LISTENER_MODE: u32 = 0o660;

fn effective_ids() -> (u32, u32) {
    // SAFETY: geteuid/getegid are side-effect-free POSIX process queries.
    unsafe { (libc::geteuid(), libc::getegid()) }
}

fn validate_path(path: &Path) -> anyhow::Result<()> {
    let raw = path.as_os_str().as_bytes();
    anyhow::ensure!(
        raw.first() == Some(&b'/')
            && raw.len() > 1
            && !raw.ends_with(b"/")
            && raw[1..]
                .split(|byte| *byte == b'/')
                .all(|part| !part.is_empty() && part != b"." && part != b".."),
        "Unix listener path must be an absolute normalized path"
    );
    anyhow::ensure!(
        raw.len() <= MAX_UNIX_SOCKET_PATH_BYTES,
        "Unix listener path exceeds the conservative {MAX_UNIX_SOCKET_PATH_BYTES}-byte limit"
    );
    Ok(())
}

fn validate_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Unix listener path has no parent"))?;
    let metadata = fs::symlink_metadata(parent).map_err(|error| {
        anyhow::anyhow!(
            "cannot inspect Unix listener parent {}: {error}",
            parent.display()
        )
    })?;
    let (euid, egid) = effective_ids();
    anyhow::ensure!(
        metadata.file_type().is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == euid
            && metadata.gid() == egid
            && metadata.mode() & 0o7777 == RUNTIME_DIRECTORY_MODE,
        "Unix listener parent must be a non-symlink directory owned by the effective uid/gid with mode 0750"
    );
    Ok(())
}

fn exact_socket(metadata: &fs::Metadata, euid: u32, egid: u32) -> bool {
    metadata.file_type().is_socket()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == euid
        && metadata.gid() == egid
        && metadata.nlink() == 1
        && metadata.mode() & 0o7777 == LISTENER_MODE
}

fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let (euid, egid) = effective_ids();
    anyhow::ensure!(
        exact_socket(&metadata, euid, egid),
        "existing Unix listener path is not the exact owned 0660 socket"
    );
    match UnixStream::connect(path) {
        Ok(_connection) => anyhow::bail!(
            "Unix listener {} is already accepting connections",
            path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) => {}
        Err(error) => {
            return Err(anyhow::anyhow!(
                "cannot prove existing Unix listener {} is stale: {error}",
                path.display()
            ));
        }
    }
    fs::remove_file(path)?;
    Ok(())
}

/// Owns one socket inode and removes only that inode when the server exits.
pub(crate) struct UnixSocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
}

impl Drop for UnixSocketGuard {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && !metadata.file_type().is_symlink()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
            && metadata.uid() == self.uid
            && metadata.gid() == self.gid
            && metadata.nlink() == 1
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Bind one sealed Unix listener and retain a cleanup guard for its exact inode.
pub(crate) fn bind(path: &Path) -> anyhow::Result<(tokio::net::UnixListener, UnixSocketGuard)> {
    validate_path(path)?;
    validate_parent(path)?;
    remove_stale_socket(path)?;

    let listener = tokio::net::UnixListener::bind(path).map_err(|error| {
        anyhow::anyhow!("cannot bind Unix listener {}: {error}", path.display())
    })?;
    let metadata = fs::symlink_metadata(path)?;
    let (euid, egid) = effective_ids();
    let guard = UnixSocketGuard {
        path: path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
        gid: metadata.gid(),
    };
    anyhow::ensure!(
        metadata.file_type().is_socket()
            && metadata.uid() == euid
            && metadata.gid() == egid
            && metadata.nlink() == 1,
        "new Unix listener identity differs"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(LISTENER_MODE))?;
    let sealed = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        exact_socket(&sealed, euid, egid)
            && sealed.dev() == guard.device
            && sealed.ino() == guard.inode,
        "sealed Unix listener identity or mode differs"
    );
    Ok((listener, guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener as StdUnixListener;

    fn runtime_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o750)).unwrap();
        let (_, egid) = effective_ids();
        // Some systems create temporary directories with an inherited group.
        // SAFETY: chown receives one live NUL-terminated path and preserves uid.
        let path = std::ffi::CString::new(directory.path().as_os_str().as_bytes()).unwrap();
        let result = unsafe { libc::chown(path.as_ptr(), u32::MAX, egid) };
        assert_eq!(result, 0);
        directory
    }

    #[tokio::test]
    async fn sealed_listener_rejects_a_second_live_binder_and_cleans_up() {
        let directory = runtime_directory();
        let path = directory.path().join("rpc.sock");
        let (listener, guard) = bind(&path).unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o660);
        let error = bind(&path).err().unwrap().to_string();
        assert!(error.contains("already accepting"), "{error}");
        drop(listener);
        drop(guard);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn exact_stale_socket_is_replaced_but_other_file_types_fail_closed() {
        let directory = runtime_directory();
        let path = directory.path().join("rpc.sock");
        let stale = StdUnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o660)).unwrap();
        drop(stale);
        let (listener, guard) = bind(&path).unwrap();
        drop(listener);
        drop(guard);

        fs::write(&path, b"not a socket").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o660)).unwrap();
        let error = bind(&path).err().unwrap().to_string();
        assert!(error.contains("exact owned 0660 socket"), "{error}");
    }

    #[tokio::test]
    async fn unsafe_parent_mode_and_non_normal_path_are_rejected() {
        let directory = runtime_directory();
        let path = directory.path().join("rpc.sock");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o770)).unwrap();
        assert!(bind(&path).is_err());
        assert!(validate_path(Path::new("/tmp/../tmp/rpc.sock")).is_err());
        assert!(validate_path(Path::new("/tmp/./rpc.sock")).is_err());
        assert!(validate_path(Path::new("/tmp//rpc.sock")).is_err());
        assert!(validate_path(Path::new("relative.sock")).is_err());
    }
}
