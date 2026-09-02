//! Race-resistant private-file creation and loading.
//!
//! Validator keyfiles must be born private, must never follow a symbolic link
//! or Windows reparse point, and must be validated through the already-open
//! handle. Keeping this boundary in one module lets the CLI, node, installer,
//! and desktop agree on the same filesystem contract.

use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

pub const DESKTOP_SHUTDOWN_CONTROL_DIR_NAME: &str = ".arc-desktop-control";
pub const DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME: &str = "shutdown-unproven";
pub const DESKTOP_SHUTDOWN_ACK_FILE_NAME: &str = "shutdown-clean-ack";
const DESKTOP_SHUTDOWN_RECEIPT_SCHEMA: &str = "arc.desktop.shutdown-unproven.v1";
const DESKTOP_SHUTDOWN_ACK_SCHEMA: &str = "arc.desktop.shutdown-clean-ack.v1";
const DESKTOP_SHUTDOWN_RECEIPT_MAX_BYTES: u64 = 768;

fn permission_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

/// Convert a filesystem path to an absolute Win32 extended-length path.
///
/// Raw `*W` APIs do not receive Rust std's automatic long-path normalization.
/// Prefixing drive paths with `\\?\` and UNC paths with `\\?\UNC\` keeps ARC
/// data directories working beyond legacy `MAX_PATH` without depending on a
/// process manifest or machine-wide registry setting.
#[cfg(windows)]
pub fn windows_extended_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::{Component, Prefix};

    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows path must not be empty",
        ));
    }

    // `std::path::absolute` delegates to GetFullPathNameW on Windows. Calling
    // it before adding the extended-length prefix reintroduces the very
    // MAX_PATH dependency this helper exists to avoid. Resolve ordinary
    // relative paths by joining the already-absolute process directory and
    // reject partially-qualified drive/root paths whose interpretation would
    // otherwise require Win32 normalization.
    let first = path.components().next();
    if let Some(Component::Prefix(prefix)) = first {
        match prefix.kind() {
            Prefix::Disk(_)
            | Prefix::UNC(_, _)
            | Prefix::VerbatimUNC(_, _)
            | Prefix::VerbatimDisk(_) => {}
            Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Windows device/verbatim namespace paths are not valid ARC data paths",
                ));
            }
        }
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        if matches!(first, Some(Component::Prefix(_) | Component::RootDir)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Windows data path must be fully qualified or ordinarily relative: {}",
                    path.display()
                ),
            ));
        }
        let current = std::env::current_dir()?;
        if !current.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows current directory is not fully qualified",
            ));
        }
        current.join(path)
    };

    let mut normalized = std::path::PathBuf::new();
    let mut normal_depth = 0usize;
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normal_depth == 0 || !normalized.pop() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Windows path escapes its filesystem root: {}",
                            path.display()
                        ),
                    ));
                }
                normal_depth -= 1;
            }
            Component::Prefix(prefix) => {
                match prefix.kind() {
                    Prefix::Disk(_)
                    | Prefix::UNC(_, _)
                    | Prefix::VerbatimUNC(_, _)
                    | Prefix::VerbatimDisk(_) => {}
                    Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Windows device/verbatim namespace paths are not valid ARC data paths",
                        ));
                    }
                }
                normalized.push(component.as_os_str());
            }
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(value) => {
                // Verbatim paths intentionally disable Win32 dot-segment
                // normalization. Canonicalized paths never contain these;
                // reject caller-supplied ambiguous verbatim components.
                if value == std::ffi::OsStr::new(".") || value == std::ffi::OsStr::new("..") {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Windows verbatim data paths must not contain dot segments",
                    ));
                }
                normalized.push(component.as_os_str());
                normal_depth = normal_depth.checked_add(1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Windows path depth overflows")
                })?;
            }
        }
    }
    if !normalized.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Windows path is not fully qualified: {}", path.display()),
        ));
    }
    let encoded = normalized.as_os_str().encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows path contains an interior NUL",
        ));
    }

    const SLASH: u16 = b'\\' as u16;
    const QUESTION: u16 = b'?' as u16;
    let mut extended = if encoded.starts_with(&[SLASH, SLASH, QUESTION, SLASH]) {
        encoded
    } else if encoded.starts_with(&[SLASH, SLASH]) {
        let mut value = vec![
            SLASH,
            SLASH,
            QUESTION,
            SLASH,
            b'U' as u16,
            b'N' as u16,
            b'C' as u16,
            SLASH,
        ];
        value.extend_from_slice(&encoded[2..]);
        value
    } else {
        let mut value = vec![SLASH, SLASH, QUESTION, SLASH];
        value.extend_from_slice(&encoded);
        value
    };
    extended.push(0);
    Ok(extended)
}

/// Move a file or directory through the documented Windows write-through
/// namespace primitive, using extended-length absolute paths on both sides.
#[cfg(windows)]
pub fn windows_move_path_write_through(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = windows_extended_path(source)?;
    let destination = windows_extended_path(destination)?;
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace_existing {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Create a brand-new private regular file without following links.
///
/// The target must not already exist. Unix files are born and revalidated at
/// mode 0600. Windows files are born owned by the current user with a protected
/// DACL granting full control only to that user, LocalSystem, and
/// Administrators, then owner and DACL are revalidated through the returned
/// handle.
pub fn create_new_private(path: &Path) -> io::Result<File> {
    platform::create_new_private(path)
}

/// Open an existing private regular file without following links and validate
/// its permissions through the open handle.
pub fn open_private(path: &Path) -> io::Result<File> {
    platform::open_private(path)
}

/// Open an existing private file read/write without following links. This is
/// reserved for OS-backed advisory lock files whose handle must remain open;
/// secret payload readers should keep using the read-only `open_private`.
pub fn open_private_read_write(path: &Path) -> io::Result<File> {
    platform::open_private_read_write(path)
}

/// Open an existing owner-controlled private regular file for append without
/// following links, tightening pre-hardening permissions through that exact
/// handle before returning it. The returned handle uses kernel append
/// semantics so a WAL writer cannot accidentally overwrite an existing tail.
pub fn open_private_append_owned_migration(path: &Path) -> io::Result<File> {
    platform::open_private_append_owned_migration(path)
}

/// Open an owner-controlled regular file for bounded comparison without
/// following links or changing its existing permission boundary. Callers may
/// subsequently tighten that exact handle only after proving idempotence.
pub fn open_owned_nofollow_read(path: &Path) -> io::Result<File> {
    platform::open_owned_nofollow_read(path)
}

/// Tighten an already-open, owner-verified regular file to ARC's private
/// permission boundary. The handle is revalidated before any mutation.
pub fn tighten_open_owned_private(file: &File, path: &Path) -> io::Result<()> {
    platform::tighten_open_owned_private(file, path)
}

/// Revalidate the private-file contract through an already-open handle.
pub fn validate_private(file: &File, path: &Path) -> io::Result<()> {
    platform::validate_private(file, path)
}

/// Remove the path naming an already-open validated private file while the
/// caller still retains that exact handle. This closes the consume/publish
/// gap in one-shot capability protocols: a replacement cannot be published
/// between dropping the read handle and unlinking the name.
pub fn remove_private_while_open(file: &File, path: &Path) -> io::Result<()> {
    platform::remove_private_while_open(file, path)
}

/// Durably remove a validated private lifecycle marker. Unix unlinks and
/// fsyncs the parent directory. Windows performs a same-directory
/// write-through rename to a unique tombstone before best-effort deletion, so
/// a reboot can resurrect only the harmless tombstone, never the live marker.
pub fn durably_remove_private_while_open(file: &File, path: &Path) -> io::Result<()> {
    platform::durably_remove_private_while_open(file, path)
}

/// Create a brand-new private directory without following a final link.
///
/// Unix directories are owned by the effective UID and mode 0700. Windows
/// directories use the same protected owner/DACL policy as private files.
pub fn create_new_private_directory(path: &Path) -> io::Result<()> {
    platform::create_new_private_directory(path)
}

#[cfg(windows)]
fn private_directory_namespace_rebarrier_path(path: &Path) -> io::Result<PathBuf> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("private directory has no leaf: {}", path.display()),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("private directory has no parent: {}", path.display()),
        )
    })?;
    Ok(parent.join(format!(
        ".arc-private-directory-namespace-{}.rebarrier",
        namespace_path_digest(path)?
    )))
}

/// Stable namespace identity for a target whose parent already exists.
/// Windows resolves ordinary file names case-insensitively, so case-fold the
/// leaf before hashing; otherwise `data-v3` and `Data-V3` could acquire
/// different outer locks while referring to the same live/staged namespace.
/// Conservative Unicode uppercasing may serialize a few distinct exotic
/// names together, which is safe; it must never split names Windows aliases.
pub fn namespace_path_digest(path: &Path) -> io::Result<String> {
    let leaf = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("namespace target has no leaf: {}", path.display()),
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    let mut hasher = Sha256::new();
    update_path_hash(&mut hasher, &parent);
    hasher.update([0xff]);
    #[cfg(windows)]
    hasher.update(leaf.to_string_lossy().to_uppercase().as_bytes());
    #[cfg(not(windows))]
    update_path_hash(&mut hasher, Path::new(leaf));
    Ok(hex::encode(hasher.finalize()))
}

/// Cross-process guard for publishing or rebarriering one private directory
/// name. The lock lives beside the target, so it remains continuously
/// addressable while the target itself moves through its deterministic
/// Windows write-through intermediate.
pub struct PrivateDirectoryNamespaceLock {
    target: PathBuf,
    _file: File,
}

impl PrivateDirectoryNamespaceLock {
    /// Restore a staged-only directory before an absent-name creation check.
    /// Callers must retain this guard through the following create/open step.
    pub fn restore_interrupted(&self) -> io::Result<()> {
        restore_private_directory_namespace_rebarrier(&self.target)
    }

    /// Re-prove an existing directory name after the caller has acquired the
    /// semantic lock inside it. Holding both locks prevents a second process
    /// from creating a split directory while the live name is briefly away.
    pub fn rebarrier_existing(&self) -> io::Result<()> {
        rebarrier_private_directory_namespace(&self.target)
    }

    pub fn target(&self) -> &Path {
        &self.target
    }
}

/// Acquire the deterministic parent-sibling lock for one directory target.
/// The target may be absent, but its parent must already be a real directory.
pub fn acquire_private_directory_namespace_lock(
    target: &Path,
) -> io::Result<PrivateDirectoryNamespaceLock> {
    let leaf = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("private directory must name a leaf: {}", target.display()),
        )
    })?;
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    validate_private_directory_parent(&parent)?;
    let target = parent.join(leaf);
    let lock_path = parent.join(format!(
        ".arc-private-directory-namespace-{}.lock",
        namespace_path_digest(&target)?
    ));
    let file = match create_new_private(&lock_path) {
        Ok(mut created) => {
            created.write_all(b"arc.private-directory.namespace-lock.v1\n")?;
            created.sync_all()?;
            drop(created);
            // Reopen with share-write before taking the advisory lock. A
            // CREATE_NEW handle is intentionally born exclusive on Windows;
            // retaining it would make contenders fail at open instead of wait
            // on the common kernel lock.
            open_private_read_write(&lock_path)?
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            open_private_read_write(&lock_path)?
        }
        Err(error) => return Err(error),
    };
    // Namespace preparation is bounded to two same-parent moves. Waiting for
    // an in-flight cooperating creator avoids surfacing spurious request
    // failures when several settlement publishers initialize one directory.
    file.lock()?;
    Ok(PrivateDirectoryNamespaceLock {
        target,
        _file: file,
    })
}

fn validate_private_directory_parent(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(permission_error(format!(
            "private directory parent is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn restore_private_directory_namespace_rebarrier(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let staging = private_directory_namespace_rebarrier_path(path)?;
        let live = std::fs::symlink_metadata(path);
        let staged = std::fs::symlink_metadata(&staging);
        match (live, staged) {
            (Ok(live), Ok(staged)) => {
                if live.file_type().is_symlink()
                    || !live.is_dir()
                    || staged.file_type().is_symlink()
                    || !staged.is_dir()
                {
                    return Err(permission_error(
                        "private directory namespace contains a linked/non-directory object",
                    ));
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "private directory exists in both live and rebarrier namespaces: {}",
                        path.display()
                    ),
                ));
            }
            (Ok(live), Err(error)) if error.kind() == io::ErrorKind::NotFound => {
                if live.file_type().is_symlink() || !live.is_dir() {
                    return Err(permission_error(format!(
                        "private directory is linked or not a directory: {}",
                        path.display()
                    )));
                }
            }
            (Err(error), Ok(staged)) if error.kind() == io::ErrorKind::NotFound => {
                if staged.file_type().is_symlink() || !staged.is_dir() {
                    return Err(permission_error(format!(
                        "private directory rebarrier is linked or not a directory: {}",
                        staging.display()
                    )));
                }
                windows_move_path_write_through(&staging, path, false)?;
                validate_private_directory(path)?;
            }
            (Err(live), Err(staged))
                if live.kind() == io::ErrorKind::NotFound
                    && staged.kind() == io::ErrorKind::NotFound => {}
            (Err(error), _) | (_, Err(error)) => return Err(error),
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

fn rebarrier_private_directory_namespace(path: &Path) -> io::Result<()> {
    restore_private_directory_namespace_rebarrier(path)?;
    validate_private_directory(path)?;
    #[cfg(windows)]
    {
        let staging = private_directory_namespace_rebarrier_path(path)?;
        windows_move_path_write_through(path, &staging, false)?;
        windows_move_path_write_through(&staging, path, false)?;
        validate_private_directory(path)
    }
    #[cfg(not(windows))]
    sync_parent_directory(path)
}

/// Create every missing component of a user-selected private directory tree.
///
/// The deepest existing ancestor is canonicalized first so platform aliases
/// such as macOS `/var -> /private/var` do not make otherwise valid data paths
/// unusable. Every newly-created component is then born private and its parent
/// namespace is made durable by the platform primitive. The final component is
/// always revalidated/tightened as an ARC-private directory.
///
/// Unlike [`secure_private_directory_tree`], this helper intentionally accepts
/// links in the already-existing ancestor prefix. It is therefore appropriate
/// for an operator-selected storage path, but not for a capability directory
/// whose complete ancestor chain must be link-free.
pub fn create_private_directory_tree(path: &Path) -> io::Result<()> {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "private directory path must not contain '..': {}",
                        path.display()
                    ),
                ));
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    if normalized.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "private directory must name a leaf component: {}",
                path.display()
            ),
        ));
    }
    let requested = if normalized.is_absolute() {
        normalized
    } else {
        std::env::current_dir()?.join(normalized)
    };
    let mut missing_names = Vec::new();
    let mut cursor = requested.as_path();
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if !metadata.is_dir() && !metadata.file_type().is_symlink() {
                    return Err(permission_error(format!(
                        "private directory tree contains a non-directory component: {}",
                        cursor.display()
                    )));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = cursor.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "private directory has no existing ancestor: {}",
                            path.display()
                        ),
                    )
                })?;
                missing_names.push(name.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "private directory has no existing ancestor: {}",
                            path.display()
                        ),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }

    let mut resolved = cursor.canonicalize()?;
    if !resolved.is_dir() {
        return Err(permission_error(format!(
            "private directory ancestor is not a directory: {}",
            resolved.display()
        )));
    }
    for name in missing_names.iter().rev() {
        resolved.push(name);
        match create_new_private_directory(&resolved) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                secure_private_directory(&resolved)?;
                // A prior attempt (or a racing creator) may have made this
                // name visible but failed before its parent-directory barrier.
                // Repeating that barrier makes retry success meaningful.
                sync_parent_directory(&resolved)?;
            }
            Err(error) => return Err(error),
        }
    }
    secure_private_directory(&resolved)?;
    // This also covers the retry case where the requested leaf already
    // existed when the walk began. On Windows the create/move and subsequent
    // child publications use write-through namespace operations; the portable
    // parent-sync abstraction is intentionally a no-op there.
    sync_parent_directory(&resolved)
}

/// Open and validate an existing private directory through an OS handle.
pub fn validate_private_directory(path: &Path) -> io::Result<()> {
    platform::validate_private_directory(path)
}

/// Ensure an app-owned secret directory exists at the platform's private
/// owner boundary. An existing owner-controlled directory may have its Unix
/// mode or Windows inherited DACL tightened through the already-open handle,
/// which safely migrates pre-hardening per-user app directories.
pub fn secure_private_directory(path: &Path) -> io::Result<()> {
    platform::secure_private_directory(path)
}

/// Ensure every missing component of an application-private directory tree is
/// created through the platform's protected, durable directory publication
/// primitive. Existing final directories are owner-verified and tightened.
///
/// This is used before publishing durable lifecycle capabilities: persisting a
/// token or receipt *inside* a directory is not enough if a power loss can
/// still discard the directory entry that contains them.
///
/// Threat boundary: this pathname walk rejects links/reparse points that are
/// present while each component is inspected and then tightens app-owned
/// directories to mode 0700/a protected owner DACL. It is not a handle-relative
/// walk: a malicious process already running as the same OS user could swap a
/// component between validation and `canonicalize`. Such a process is outside
/// this primitive's isolation boundary because it can already read or replace
/// the same user's secret files directly. Callers still use their lifecycle
/// lock to serialize cooperating ARC processes.
pub fn secure_private_directory_tree(path: &Path) -> io::Result<()> {
    // Validate each existing pathname component at the component's own name.
    // Inspecting only the deepest existing path is insufficient: lstat/Windows
    // metadata for `root/link/child` describes `child` after an intermediate
    // symlink or junction has already been followed. Walking root-to-leaf makes
    // the link/reparse component itself the final object inspected, before the
    // existing prefix is canonicalized below.
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match platform::validate_directory_component_no_link(ancestor) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    let mut missing_names = Vec::new();
    let mut cursor = path;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(permission_error(format!(
                        "private directory tree contains a linked/non-directory component: {}",
                        cursor.display()
                    )));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = cursor.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "private directory has an unnamed component: {}",
                            path.display()
                        ),
                    )
                })?;
                missing_names.push(name.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "private directory has no existing ancestor: {}",
                            path.display()
                        ),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
    // Resolve the checked existing prefix once, then create beneath that
    // canonical location. This rejects stable pre-existing intermediate links;
    // it does not claim handle-relative protection from a concurrent same-user
    // namespace swap (see the explicit threat boundary above).
    let mut resolved = cursor.canonicalize()?;
    for name in missing_names.iter().rev() {
        resolved.push(name);
        match create_new_private_directory(&resolved) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                secure_private_directory(&resolved)?;
                sync_parent_directory(&resolved)?;
            }
            Err(error) => return Err(error),
        }
    }
    secure_private_directory(&resolved)?;
    sync_parent_directory(&resolved)
}

/// Open a pre-hardening app secret file and migrate only an owner-controlled
/// permission boundary through the already-open handle. Unix tightens the mode
/// to 0600 and strips a macOS ACL; Windows applies the protected private DACL.
pub fn open_private_owned_migration(path: &Path) -> io::Result<File> {
    platform::open_private_owned_migration(path)
}

/// Durably publish a same-directory file creation on platforms that support
/// directory fsync.
pub fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Atomically publish one already-complete private file at an absent sibling
/// name. The staging file is revalidated and fsynced through a writable handle
/// before Unix hard-link/parent-fsync or Windows write-through move.
pub fn durably_publish_existing_private_no_replace(
    staging: &Path,
    final_path: &Path,
) -> io::Result<()> {
    fn parent(path: &Path) -> &Path {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    }
    if parent(staging) != parent(final_path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable private publication requires same-directory paths",
        ));
    }
    let file = open_private_read_write(staging)?;
    file.sync_all()?;
    drop(file);
    publish_private_no_replace(staging, final_path)
}

#[cfg(unix)]
fn publish_private_no_replace(staging: &Path, final_path: &Path) -> io::Result<()> {
    std::fs::hard_link(staging, final_path)?;
    sync_parent_directory(final_path)?;
    std::fs::remove_file(staging)
}

#[cfg(windows)]
fn publish_private_no_replace(staging: &Path, final_path: &Path) -> io::Result<()> {
    // No REPLACE_EXISTING flag: publication is atomic create-only. The
    // write-through flag asks NTFS to flush the directory-entry mutation
    // before success is reported, closing the power-loss gap left by the
    // otherwise no-op Windows parent-directory fsync abstraction.
    windows_move_path_write_through(staging, final_path, false)?;
    let final_handle = open_private(final_path)?;
    validate_private(&final_handle, final_path)
}

#[cfg(not(any(unix, windows)))]
fn publish_private_no_replace(staging: &Path, final_path: &Path) -> io::Result<()> {
    std::fs::rename(staging, final_path)
}

#[cfg(unix)]
fn replace_private_write_through(staging: &Path, final_path: &Path) -> io::Result<()> {
    std::fs::rename(staging, final_path)?;
    sync_parent_directory(final_path)
}

#[cfg(windows)]
fn replace_private_write_through(staging: &Path, final_path: &Path) -> io::Result<()> {
    windows_move_path_write_through(staging, final_path, true)?;
    let final_handle = open_private(final_path)?;
    validate_private(&final_handle, final_path)
}

#[cfg(not(any(unix, windows)))]
fn replace_private_write_through(staging: &Path, final_path: &Path) -> io::Result<()> {
    std::fs::rename(staging, final_path)
}

fn invalid_receipt(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn update_path_hash(hasher: &mut Sha256, path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        for code_unit in path.as_os_str().encode_wide() {
            hasher.update(code_unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(path.to_string_lossy().as_bytes());
}

fn path_digest(path: &Path) -> String {
    let mut hasher = Sha256::new();
    update_path_hash(&mut hasher, path);
    hex::encode(hasher.finalize())
}

fn file_digest(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid_receipt(format!(
            "shutdown receipt identity is not a regular file: {}",
            path.display()
        )));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn desktop_shutdown_bound_payload(
    data_dir: &Path,
    token: &[u8; 32],
    nonce: &[u8; 32],
    executable: &Path,
    genesis: &Path,
    schema: &str,
    file_name: &str,
) -> io::Result<(std::path::PathBuf, String)> {
    let data_dir = data_dir.canonicalize()?;
    let executable = executable.canonicalize()?;
    let genesis = genesis.canonicalize()?;
    let control_dir = data_dir.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
    validate_private_directory(&control_dir)?;
    let receipt = control_dir.join(file_name);
    let payload = format!(
        "{schema}\nnonce={}\ndata_dir_sha256={}\ntoken_sha256={}\nexecutable_path_sha256={}\nexecutable_sha256={}\ngenesis_path_sha256={}\ngenesis_sha256={}\n",
        hex::encode(nonce),
        path_digest(&data_dir),
        hex::encode(Sha256::digest(token)),
        path_digest(&executable),
        file_digest(&executable)?,
        path_digest(&genesis),
        file_digest(&genesis)?,
    );
    if payload.len() as u64 > DESKTOP_SHUTDOWN_RECEIPT_MAX_BYTES {
        return Err(invalid_receipt(
            "desktop shutdown receipt payload is too large",
        ));
    }
    Ok((receipt, payload))
}

fn desktop_shutdown_receipt_payload(
    data_dir: &Path,
    token: &[u8; 32],
    nonce: &[u8; 32],
    executable: &Path,
    genesis: &Path,
) -> io::Result<(std::path::PathBuf, String)> {
    desktop_shutdown_bound_payload(
        data_dir,
        token,
        nonce,
        executable,
        genesis,
        DESKTOP_SHUTDOWN_RECEIPT_SCHEMA,
        DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME,
    )
}

fn desktop_shutdown_ack_payload(
    data_dir: &Path,
    token: &[u8; 32],
    nonce: &[u8; 32],
    executable: &Path,
    genesis: &Path,
) -> io::Result<(std::path::PathBuf, String)> {
    desktop_shutdown_bound_payload(
        data_dir,
        token,
        nonce,
        executable,
        genesis,
        DESKTOP_SHUTDOWN_ACK_SCHEMA,
        DESKTOP_SHUTDOWN_ACK_FILE_NAME,
    )
}

fn bound_file_nonce(text: &str, schema: &str) -> io::Result<[u8; 32]> {
    let mut lines = text.lines();
    if lines.next() != Some(schema) {
        return Err(invalid_receipt(
            "desktop shutdown receipt has an invalid schema",
        ));
    }
    let encoded = lines
        .next()
        .and_then(|line| line.strip_prefix("nonce="))
        .ok_or_else(|| invalid_receipt("desktop shutdown receipt omits its nonce"))?;
    let decoded = hex::decode(encoded)
        .map_err(|_| invalid_receipt("desktop shutdown receipt nonce is not hexadecimal"))?;
    if decoded.len() != 32 {
        return Err(invalid_receipt(
            "desktop shutdown receipt nonce must contain exactly 32 bytes",
        ));
    }
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&decoded);
    Ok(nonce)
}

fn receipt_nonce(text: &str) -> io::Result<[u8; 32]> {
    bound_file_nonce(text, DESKTOP_SHUTDOWN_RECEIPT_SCHEMA)
}

fn read_private_receipt(file: &mut File) -> io::Result<String> {
    if file.metadata()?.len() > DESKTOP_SHUTDOWN_RECEIPT_MAX_BYTES {
        return Err(invalid_receipt("desktop shutdown receipt is too large"));
    }
    let mut text = String::new();
    file.take(DESKTOP_SHUTDOWN_RECEIPT_MAX_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > DESKTOP_SHUTDOWN_RECEIPT_MAX_BYTES {
        return Err(invalid_receipt("desktop shutdown receipt is too large"));
    }
    Ok(text)
}

/// Durably arm the desktop-managed shutdown boundary before the supervisor
/// signals or spawns the node. The receipt binds the exact canonical data
/// directory, private capability, executable bytes, and genesis bytes. An
/// existing receipt is accepted only when every binding is identical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopShutdownReceiptArm {
    pub nonce: [u8; 32],
    pub created_this_attempt: bool,
}

fn publish_bound_private_file(
    path: &Path,
    contents: &str,
    _lifecycle_nonce: &[u8; 32],
) -> io::Result<bool> {
    use rand::RngCore as _;

    // A crash can leave staging behind and a reboot can reuse both the PID and
    // inherited lifecycle nonce. Give every publication a fresh suffix so an
    // orphan staging inode never permanently fences a recovery ACK.
    let mut publication_nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut publication_nonce);
    let staging = path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("shutdown-boundary"),
        std::process::id(),
        hex::encode(publication_nonce)
    ));
    let mut staging_file = create_new_private(&staging)?;
    if let Err(error) = staging_file
        .write_all(contents.as_bytes())
        .and_then(|_| staging_file.sync_all())
    {
        drop(staging_file);
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    drop(staging_file);
    match publish_private_no_replace(&staging, path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&staging);
            Ok(false)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            Err(error)
        }
    }
}

/// Durably publish a complete private file at a previously absent name.
///
/// The complete contents are written and fsynced in a same-directory staging
/// file before an atomic no-replace publication. Windows uses a write-through
/// rename so the directory entry is durable before success; Unix fsyncs the
/// parent directory. `Ok(false)` means another complete file already owns the
/// final name and this call did not modify it.
pub fn durably_publish_new_private(path: &Path, contents: &[u8]) -> io::Result<bool> {
    use rand::RngCore as _;

    let mut staging_nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut staging_nonce);
    let staging = path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("private"),
        std::process::id(),
        hex::encode(staging_nonce)
    ));
    let mut staging_file = create_new_private(&staging)?;
    if let Err(error) = staging_file
        .write_all(contents)
        .and_then(|_| staging_file.sync_all())
    {
        drop(staging_file);
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    drop(staging_file);
    match publish_private_no_replace(&staging, path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&staging);
            Ok(false)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            Err(error)
        }
    }
}

/// Atomically and durably replace a private regular file with complete bytes.
/// The caller must place the file in an already validated private directory.
pub fn durably_replace_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use rand::RngCore as _;

    let mut staging_nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut staging_nonce);
    let staging = path.with_file_name(format!(
        ".{}.{}.{}.replace",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("private"),
        std::process::id(),
        hex::encode(staging_nonce)
    ));
    let mut staging_file = create_new_private(&staging)?;
    if let Err(error) = staging_file
        .write_all(contents)
        .and_then(|_| staging_file.sync_all())
    {
        drop(staging_file);
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    drop(staging_file);
    if let Err(error) = replace_private_write_through(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    let final_file = open_private(path)?;
    validate_private(&final_file, path)
}

pub fn arm_desktop_shutdown_receipt(
    data_dir: &Path,
    token: &[u8; 32],
    executable: &Path,
    genesis: &Path,
) -> io::Result<DesktopShutdownReceiptArm> {
    use rand::RngCore as _;

    let canonical_data_dir = data_dir.canonicalize()?;
    let control_dir = canonical_data_dir.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
    validate_private_directory(&control_dir)?;
    let receipt = control_dir.join(DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME);
    match open_private(&receipt) {
        Ok(mut file) => {
            let actual = read_private_receipt(&mut file)?;
            let nonce = receipt_nonce(&actual)?;
            let (_, expected) = desktop_shutdown_receipt_payload(
                &canonical_data_dir,
                token,
                &nonce,
                executable,
                genesis,
            )?;
            if actual != expected {
                return Err(invalid_receipt(
                    "existing desktop shutdown receipt does not match this node identity",
                ));
            }
            drop(file);
            // The preceding publication may have made this exact receipt
            // visible while reporting a late parent-directory/write-through
            // failure. The managed lifecycle lock serializes this bounded
            // replacement, so retry success proves the unproven marker before
            // the supervisor is allowed to spawn a node beneath it.
            durably_replace_private(&receipt, expected.as_bytes())?;
            return Ok(DesktopShutdownReceiptArm {
                nonce,
                created_this_attempt: false,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // An ACK without a marker can only be the fail-safe residue of a prior
    // supervisor crash after it consumed the marker. Remove that private
    // complete record before selecting the next unique lifecycle nonce.
    let ack = control_dir.join(DESKTOP_SHUTDOWN_ACK_FILE_NAME);
    match open_private(&ack) {
        Ok(mut file) => {
            let text = read_private_receipt(&mut file)?;
            let old_nonce = bound_file_nonce(&text, DESKTOP_SHUTDOWN_ACK_SCHEMA)?;
            let (_, expected_ack) = desktop_shutdown_ack_payload(
                &canonical_data_dir,
                token,
                &old_nonce,
                executable,
                genesis,
            )?;
            if text != expected_ack {
                return Err(invalid_receipt(
                    "ACK-only shutdown residue does not match this exact node identity",
                ));
            }
            durably_remove_private_while_open(&file, &ack)?;
            drop(file);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let (receipt, expected) =
        desktop_shutdown_receipt_payload(&canonical_data_dir, token, &nonce, executable, genesis)?;
    match publish_bound_private_file(&receipt, &expected, &nonce) {
        Ok(true) => Ok(DesktopShutdownReceiptArm {
            nonce,
            created_this_attempt: true,
        }),
        Ok(false) => {
            let mut file = open_private(&receipt)?;
            let actual = read_private_receipt(&mut file)?;
            let nonce = receipt_nonce(&actual)?;
            let (_, expected) =
                desktop_shutdown_receipt_payload(data_dir, token, &nonce, executable, genesis)?;
            if actual != expected {
                return Err(invalid_receipt(
                    "existing desktop shutdown receipt does not match this node identity",
                ));
            }
            drop(file);
            // `Ok(false)` proves only that an identical publisher won the
            // create-only race. Repeat the namespace barrier before treating
            // that visible receipt as a durable launch fence.
            durably_replace_private(&receipt, expected.as_bytes())?;
            Ok(DesktopShutdownReceiptArm {
                nonce,
                created_this_attempt: false,
            })
        }
        Err(error) => Err(error),
    }
}

/// Validate that an armed receipt still binds this exact managed node.
pub fn validate_desktop_shutdown_receipt(
    data_dir: &Path,
    token: &[u8; 32],
    nonce: &[u8; 32],
    executable: &Path,
    genesis: &Path,
) -> io::Result<bool> {
    let (receipt, expected) =
        desktop_shutdown_receipt_payload(data_dir, token, nonce, executable, genesis)?;
    let mut file = match open_private(&receipt) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let actual = read_private_receipt(&mut file)?;
    if actual != expected {
        return Err(invalid_receipt(
            "desktop shutdown receipt does not match this node identity",
        ));
    }
    Ok(true)
}

/// Load and validate an inherited receipt for an exact recovery launch.
/// Absence is the normal state for a newly started managed node.
pub fn load_desktop_shutdown_receipt_nonce(
    data_dir: &Path,
    token: &[u8; 32],
    executable: &Path,
    genesis: &Path,
) -> io::Result<Option<[u8; 32]>> {
    let data_dir = data_dir.canonicalize()?;
    let receipt = data_dir
        .join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)
        .join(DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME);
    let mut file = match open_private(&receipt) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let actual = read_private_receipt(&mut file)?;
    let nonce = receipt_nonce(&actual)?;
    let (_, expected) =
        desktop_shutdown_receipt_payload(&data_dir, token, &nonce, executable, genesis)?;
    if actual != expected {
        return Err(invalid_receipt(
            "desktop shutdown receipt does not match this recovery node identity",
        ));
    }
    Ok(Some(nonce))
}

/// Publish the node's positive clean-shutdown ACK while leaving the unproven
/// marker in place. The node calls this as its final fallible operation only
/// after all writers join and the final WAL fsync succeeds. The supervisor
/// validates both files after exact process death, then consumes them.
pub fn acknowledge_desktop_shutdown_receipt(
    data_dir: &Path,
    token: &[u8; 32],
    nonce: &[u8; 32],
    executable: &Path,
    genesis: &Path,
) -> io::Result<()> {
    let (receipt, expected_receipt) =
        desktop_shutdown_receipt_payload(data_dir, token, nonce, executable, genesis)?;
    let mut receipt_file = open_private(&receipt)?;
    let actual = read_private_receipt(&mut receipt_file)?;
    if actual != expected_receipt {
        return Err(invalid_receipt(
            "desktop shutdown receipt does not match this node identity",
        ));
    }
    let (ack, expected_ack) =
        desktop_shutdown_ack_payload(data_dir, token, nonce, executable, genesis)?;
    let created = publish_bound_private_file(&ack, &expected_ack, nonce)?;
    if !created {
        let mut ack_file = open_private(&ack)?;
        if read_private_receipt(&mut ack_file)? != expected_ack {
            return Err(invalid_receipt(
                "existing desktop shutdown ACK does not match this node identity",
            ));
        }
    }
    Ok(())
}

pub fn validate_desktop_shutdown_ack(
    data_dir: &Path,
    token: &[u8; 32],
    nonce: &[u8; 32],
    executable: &Path,
    genesis: &Path,
) -> io::Result<bool> {
    let (ack, expected) =
        desktop_shutdown_ack_payload(data_dir, token, nonce, executable, genesis)?;
    let mut file = match open_private(&ack) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if read_private_receipt(&mut file)? != expected {
        return Err(invalid_receipt(
            "desktop shutdown ACK does not match this node identity",
        ));
    }
    Ok(true)
}

/// Consume a positive ACK and its unproven marker after the supervisor has
/// independently proved exact process death (and exit status where the OS
/// exposes it). ACK-first removal is fail-safe: a supervisor crash can leave
/// the unproven marker armed, but can never leave an ACK-only residue whose old
/// identity an updater might invalidate before cleanup.
pub fn consume_desktop_shutdown_ack(
    data_dir: &Path,
    token: &[u8; 32],
    nonce: &[u8; 32],
    executable: &Path,
    genesis: &Path,
) -> io::Result<()> {
    let (receipt, expected_receipt) =
        desktop_shutdown_receipt_payload(data_dir, token, nonce, executable, genesis)?;
    let (ack, expected_ack) =
        desktop_shutdown_ack_payload(data_dir, token, nonce, executable, genesis)?;
    let mut receipt_file = open_private(&receipt)?;
    let mut ack_file = open_private(&ack)?;
    if read_private_receipt(&mut receipt_file)? != expected_receipt
        || read_private_receipt(&mut ack_file)? != expected_ack
    {
        return Err(invalid_receipt(
            "desktop shutdown marker/ACK pair does not match this node identity",
        ));
    }
    durably_remove_private_while_open(&ack_file, &ack)?;
    drop(ack_file);
    durably_remove_private_while_open(&receipt_file, &receipt)?;
    drop(receipt_file);
    Ok(())
}

/// Cancel the exact just-armed receipt only when the supervisor has proof
/// that process creation itself failed and therefore no node ever owned the
/// data directory. This is intentionally the sole supervisor-side removal
/// path; once spawn succeeds, only the node's clean WAL barrier may ack it.
pub fn cancel_desktop_shutdown_receipt_before_spawn(
    data_dir: &Path,
    token: &[u8; 32],
    nonce: &[u8; 32],
    executable: &Path,
    genesis: &Path,
) -> io::Result<()> {
    let (receipt, expected) =
        desktop_shutdown_receipt_payload(data_dir, token, nonce, executable, genesis)?;
    let mut file = open_private(&receipt)?;
    if read_private_receipt(&mut file)? != expected {
        return Err(invalid_receipt(
            "desktop shutdown receipt does not match the failed spawn attempt",
        ));
    }
    durably_remove_private_while_open(&file, &receipt)?;
    drop(file);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopShutdownLifecycleState {
    Clear,
    Armed,
    AckOnly,
    ArmedAndAcked,
}

impl DesktopShutdownLifecycleState {
    pub fn is_clear(self) -> bool {
        self == Self::Clear
    }
}

/// Inspect the durable managed-node lifecycle boundary without weakening its
/// private directory/file policy. Both marker and ACK names are mutation
/// fences: an ACK-only residue from an interrupted older cleanup must not let
/// an updater replace the executable or stable network identity before exact
/// validation can finish cleanup.
pub fn desktop_shutdown_lifecycle_state(
    data_dir: &Path,
) -> io::Result<DesktopShutdownLifecycleState> {
    let data_dir = data_dir.canonicalize()?;
    let control_dir = data_dir.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
    let receipt = control_dir.join(DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME);
    let ack = control_dir.join(DESKTOP_SHUTDOWN_ACK_FILE_NAME);
    let exists = |path: &Path| -> io::Result<bool> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    };
    let receipt_exists = exists(&receipt)?;
    let ack_exists = exists(&ack)?;
    if !receipt_exists && !ack_exists {
        return Ok(DesktopShutdownLifecycleState::Clear);
    }
    validate_private_directory(&control_dir)?;
    if receipt_exists {
        let _ = open_private(&receipt)?;
    }
    if ack_exists {
        let _ = open_private(&ack)?;
    }
    Ok(match (receipt_exists, ack_exists) {
        (true, true) => DesktopShutdownLifecycleState::ArmedAndAcked,
        (true, false) => DesktopShutdownLifecycleState::Armed,
        (false, true) => DesktopShutdownLifecycleState::AckOnly,
        (false, false) => DesktopShutdownLifecycleState::Clear,
    })
}

/// Backward-compatible marker predicate. Mutation/update guards should prefer
/// `desktop_shutdown_lifecycle_state` so ACK-only residues remain fail-closed.
pub fn desktop_shutdown_receipt_exists(data_dir: &Path) -> io::Result<bool> {
    Ok(matches!(
        desktop_shutdown_lifecycle_state(data_dir)?,
        DesktopShutdownLifecycleState::Armed | DesktopShutdownLifecycleState::ArmedAndAcked
    ))
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

    #[cfg(target_os = "macos")]
    mod macos_acl {
        use super::*;
        use std::ffi::c_void;
        use std::os::fd::AsRawFd as _;

        const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
        const ACL_FIRST_ENTRY: libc::c_int = 0;

        unsafe extern "C" {
            fn acl_free(object: *mut c_void) -> libc::c_int;
            fn acl_get_entry(
                acl: *mut c_void,
                entry_id: libc::c_int,
                entry: *mut *mut c_void,
            ) -> libc::c_int;
            fn acl_get_fd(fd: libc::c_int) -> *mut c_void;
            fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut c_void;
            fn acl_init(count: libc::c_int) -> *mut c_void;
            fn acl_set_fd(fd: libc::c_int, acl: *mut c_void) -> libc::c_int;
            fn acl_set_fd_np(
                fd: libc::c_int,
                acl: *mut c_void,
                acl_type: libc::c_int,
            ) -> libc::c_int;
        }

        struct Acl(*mut c_void);

        impl Drop for Acl {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    // SAFETY: this pointer was returned by a successful ACL API.
                    unsafe {
                        acl_free(self.0);
                    }
                }
            }
        }

        pub(super) fn strip(file: &File) -> io::Result<()> {
            // An empty extended ACL is the mode-only private policy. Applying
            // it through the already-open handle avoids a pathname race.
            let empty = Acl(unsafe { acl_init(0) });
            if empty.0.is_null() {
                return Err(io::Error::last_os_error());
            }
            if unsafe { acl_set_fd(file.as_raw_fd(), empty.0) } != 0
                && unsafe { acl_set_fd_np(file.as_raw_fd(), empty.0, ACL_TYPE_EXTENDED) } != 0
            {
                let error = io::Error::last_os_error();
                // macOS reports ENOENT when the object already has no extended
                // ACL to delete; that is precisely the required state.
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
            Ok(())
        }

        pub(super) fn validate_absent(file: &File, path: &Path, object: &str) -> io::Result<()> {
            let mut acl = Acl(unsafe { acl_get_fd(file.as_raw_fd()) });
            if acl.0.is_null() {
                acl = Acl(unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) });
            }
            if acl.0.is_null() {
                let error = io::Error::last_os_error();
                return if error.kind() == io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(error)
                };
            }
            let mut entry = std::ptr::null_mut();
            match unsafe { acl_get_entry(acl.0, ACL_FIRST_ENTRY, &mut entry) } {
                0 => Err(permission_error(format!(
                    "private {object} {} must not contain an extended ACL",
                    path.display()
                ))),
                _ => {
                    let error = io::Error::last_os_error();
                    // Darwin reports EINVAL when ACL_FIRST_ENTRY is requested
                    // from a valid empty ACL.
                    if error.raw_os_error() == Some(libc::EINVAL) {
                        Ok(())
                    } else {
                        Err(error)
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    mod macos_acl {
        use super::*;

        pub(super) fn strip(_file: &File) -> io::Result<()> {
            Ok(())
        }

        pub(super) fn validate_absent(_file: &File, _path: &Path, _object: &str) -> io::Result<()> {
            Ok(())
        }
    }

    fn validate_owner_as(
        metadata: &fs::Metadata,
        path: &Path,
        object: &str,
        effective_uid: libc::uid_t,
    ) -> io::Result<()> {
        if metadata.uid() != effective_uid {
            return Err(permission_error(format!(
                "private {object} {} must be owned by effective uid {effective_uid} (found uid {})",
                path.display(),
                metadata.uid()
            )));
        }
        Ok(())
    }

    fn validate_owner(metadata: &fs::Metadata, path: &Path, object: &str) -> io::Result<()> {
        // SAFETY: geteuid has no preconditions and does not retain pointers.
        let effective_uid = unsafe { libc::geteuid() };
        validate_owner_as(metadata, path, object, effective_uid)
    }

    pub(super) fn validate_directory_component_no_link(path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(permission_error(format!(
                "private directory tree contains a linked/non-directory component: {}",
                path.display()
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn validate_owner_for_test(
        path: &Path,
        effective_uid: libc::uid_t,
    ) -> io::Result<()> {
        validate_owner_as(&fs::metadata(path)?, path, "file", effective_uid)
    }

    pub(super) fn create_new_private(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        macos_acl::strip(&file)?;
        validate_private(&file, path)?;
        Ok(file)
    }

    pub(super) fn open_private(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let file = options.open(path)?;
        validate_private(&file, path)?;
        Ok(file)
    }

    pub(super) fn open_private_read_write(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let file = options.open(path)?;
        validate_private(&file, path)?;
        Ok(file)
    }

    pub(super) fn open_private_append_owned_migration(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(permission_error(format!(
                "private file is not a regular file: {}",
                path.display()
            )));
        }
        validate_owner(&metadata, path, "file")?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        macos_acl::strip(&file)?;
        validate_private(&file, path)?;
        Ok(file)
    }

    pub(super) fn open_owned_nofollow_read(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(permission_error(format!(
                "owner-controlled file is not a regular file: {}",
                path.display()
            )));
        }
        validate_owner(&metadata, path, "file")?;
        Ok(file)
    }

    pub(super) fn tighten_open_owned_private(file: &File, path: &Path) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(permission_error(format!(
                "owner-controlled file is not a regular file: {}",
                path.display()
            )));
        }
        validate_owner(&metadata, path, "file")?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        macos_acl::strip(file)?;
        validate_private(file, path)
    }

    pub(super) fn validate_private(file: &File, path: &Path) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(permission_error(format!(
                "private file is not a regular file: {}",
                path.display()
            )));
        }
        validate_owner(&metadata, path, "file")?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(permission_error(format!(
                "private file {} must have mode 0600 (found {mode:04o})",
                path.display()
            )));
        }
        macos_acl::validate_absent(file, path, "file")?;
        Ok(())
    }

    pub(super) fn remove_private_while_open(file: &File, path: &Path) -> io::Result<()> {
        validate_private(file, path)?;
        // Unix unlink operates on the directory entry while `file` keeps the
        // exact opened inode alive through completion.
        fs::remove_file(path)
    }

    pub(super) fn durably_remove_private_while_open(file: &File, path: &Path) -> io::Result<()> {
        remove_private_while_open(file, path)?;
        super::sync_parent_directory(path)
    }

    fn open_private_directory(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        );
        options.open(path)
    }

    fn validate_private_directory_handle(directory: &File, path: &Path) -> io::Result<()> {
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(permission_error(format!(
                "private directory is not a directory: {}",
                path.display()
            )));
        }
        validate_owner(&metadata, path, "directory")?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(permission_error(format!(
                "private directory {} must have mode 0700 (found {mode:04o})",
                path.display()
            )));
        }
        macos_acl::validate_absent(directory, path, "directory")?;
        Ok(())
    }

    pub(super) fn create_new_private_directory(path: &Path) -> io::Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)?;
        let directory = open_private_directory(path)?;
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        macos_acl::strip(&directory)?;
        validate_private_directory_handle(&directory, path)?;
        super::sync_parent_directory(path)
    }

    pub(super) fn validate_private_directory(path: &Path) -> io::Result<()> {
        let directory = open_private_directory(path)?;
        validate_private_directory_handle(&directory, path)
    }

    pub(super) fn secure_private_directory(path: &Path) -> io::Result<()> {
        let directory = match open_private_directory(path) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return create_new_private_directory(path);
            }
            Err(error) => return Err(error),
        };
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(permission_error(format!(
                "private directory is not a directory: {}",
                path.display()
            )));
        }
        validate_owner(&metadata, path, "directory")?;
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        macos_acl::strip(&directory)?;
        validate_private_directory_handle(&directory, path)
    }

    pub(super) fn open_private_owned_migration(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(permission_error(format!(
                "private file is not a regular file: {}",
                path.display()
            )));
        }
        validate_owner(&metadata, path, "file")?;
        // Public v0.7 desktop tags wrote store.json with std::fs::write, which
        // commonly inherited mode 0644. Tighten only the already-open,
        // nofollow, owner-verified regular file; never chmod by pathname.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        macos_acl::strip(&file)?;
        validate_private(&file, path)?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "arc-private-file-{label}-{}-{:016x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn private_file_round_trip_and_no_replace() {
        let dir = TestDir::new("roundtrip");
        let path = dir.0.join("secret.json");
        let mut file = create_new_private(&path).unwrap();
        file.write_all(b"secret").unwrap();
        file.sync_all().unwrap();
        drop(file);
        open_private(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
        assert!(create_new_private(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
    }

    #[test]
    fn opened_consume_cannot_unlink_a_concurrently_published_replacement() {
        use std::io::Write as _;

        let dir = TestDir::new("consume-publish-race");
        let request = dir.0.join("request");
        let mut old = create_new_private(&request).unwrap();
        old.write_all(b"old").unwrap();
        old.sync_all().unwrap();
        drop(old);
        let opened_old = open_private(&request).unwrap();

        let staging = dir.0.join("request.new");
        let mut new = create_new_private(&staging).unwrap();
        new.write_all(b"new").unwrap();
        new.sync_all().unwrap();
        drop(new);
        let request_for_publisher = request.clone();
        let staging_for_publisher = staging.clone();
        let publisher = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match std::fs::hard_link(&staging_for_publisher, &request_for_publisher) {
                    Ok(()) => return,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::AlreadyExists
                                | io::ErrorKind::PermissionDenied
                                | io::ErrorKind::WouldBlock
                        ) && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(error) => panic!("replacement publication failed: {error}"),
                }
            }
        });

        remove_private_while_open(&opened_old, &request).unwrap();
        drop(opened_old);
        publisher.join().unwrap();
        assert_eq!(std::fs::read(&request).unwrap(), b"new");
    }

    fn shutdown_fixture(
        label: &str,
    ) -> (TestDir, [u8; 32], std::path::PathBuf, std::path::PathBuf) {
        let dir = TestDir::new(label);
        secure_private_directory(&dir.0).unwrap();
        secure_private_directory_tree(&dir.0.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)).unwrap();
        let genesis = dir.0.join("stable-genesis.toml");
        durably_publish_new_private(&genesis, b"[chain]\nname='receipt-test'\n").unwrap();
        let executable = std::env::current_exe().unwrap();
        (dir, [0x5a; 32], executable, genesis)
    }

    #[test]
    fn operator_private_directory_tree_creates_nested_components_durably() {
        let root = TestDir::new("operator-private-tree");
        let nested = root.0.join("one").join("two").join("three");
        create_private_directory_tree(&nested).unwrap();
        for directory in [root.0.join("one"), root.0.join("one/two"), nested] {
            validate_private_directory(&directory).unwrap();
        }
    }

    #[test]
    fn private_directory_namespace_guard_preserves_children_across_rebarrier() {
        let root = TestDir::new("directory-namespace-guard");
        let requested = root.0.join("semantic-child");
        let guard = acquire_private_directory_namespace_lock(&requested).unwrap();
        guard.restore_interrupted().unwrap();
        create_private_directory_tree(guard.target()).unwrap();
        let payload = guard.target().join("history");
        let mut file = create_new_private(&payload).unwrap();
        file.write_all(b"canonical history").unwrap();
        file.sync_all().unwrap();
        drop(file);
        guard.rebarrier_existing().unwrap();
        assert_eq!(std::fs::read(&payload).unwrap(), b"canonical history");
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_namespace_guard_restores_staged_only_directory() {
        let root = TestDir::new("directory-namespace-staged-only");
        let requested = root.0.join("semantic-child");
        let guard = acquire_private_directory_namespace_lock(&requested).unwrap();
        create_private_directory_tree(guard.target()).unwrap();
        let staging = private_directory_namespace_rebarrier_path(guard.target()).unwrap();
        windows_move_path_write_through(guard.target(), &staging, false).unwrap();
        assert!(!guard.target().exists());
        guard.restore_interrupted().unwrap();
        assert!(guard.target().is_dir());
        assert!(!staging.exists());
    }

    #[test]
    fn operator_private_directory_tree_rejects_parent_traversal_before_mutation() {
        let root = TestDir::new("operator-private-tree-parent");
        // Append the lexical components without `PathBuf::push`/`join`.
        // Windows normalizes dot segments while pushing onto a verbatim
        // (`\\?\`) path, which would silently turn this negative fixture into
        // the harmless sibling `root/target` before the function saw it.
        // Operator input arrives as one raw OS string, so preserve that exact
        // representation here and exercise the real parser boundary.
        let mut requested = root.0.as_os_str().to_os_string();
        requested.push(std::path::MAIN_SEPARATOR_STR);
        requested.push("missing");
        requested.push(std::path::MAIN_SEPARATOR_STR);
        requested.push("..");
        requested.push(std::path::MAIN_SEPARATOR_STR);
        requested.push("target");
        let requested = std::path::PathBuf::from(requested);
        assert!(
            requested
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)),
            "fixture must retain a lexical '..' component before the security boundary"
        );
        #[cfg(unix)]
        let before = std::fs::metadata(&root.0).unwrap().permissions();
        let error = create_private_directory_tree(&requested)
            .expect_err("parent traversal must fail before filesystem mutation");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!root.0.join("missing").exists());
        assert!(!root.0.join("target").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&root.0).unwrap().permissions().mode() & 0o777,
                before.mode() & 0o777
            );
        }
    }

    #[test]
    fn shutdown_marker_ack_lifecycle_is_exact_and_fail_closed() {
        let (dir, token, executable, genesis) = shutdown_fixture("receipt-lifecycle");
        let first = arm_desktop_shutdown_receipt(&dir.0, &token, &executable, &genesis).unwrap();
        assert!(first.created_this_attempt);
        assert_eq!(
            desktop_shutdown_lifecycle_state(&dir.0).unwrap(),
            DesktopShutdownLifecycleState::Armed
        );
        let inherited =
            arm_desktop_shutdown_receipt(&dir.0, &token, &executable, &genesis).unwrap();
        assert!(!inherited.created_this_attempt);
        assert_eq!(inherited.nonce, first.nonce);

        acknowledge_desktop_shutdown_receipt(&dir.0, &token, &first.nonce, &executable, &genesis)
            .unwrap();
        assert_eq!(
            desktop_shutdown_lifecycle_state(&dir.0).unwrap(),
            DesktopShutdownLifecycleState::ArmedAndAcked
        );
        assert!(
            validate_desktop_shutdown_ack(&dir.0, &token, &first.nonce, &executable, &genesis)
                .unwrap()
        );
        consume_desktop_shutdown_ack(&dir.0, &token, &first.nonce, &executable, &genesis).unwrap();
        assert!(desktop_shutdown_lifecycle_state(&dir.0).unwrap().is_clear());
    }

    #[cfg(unix)]
    #[test]
    fn existing_visible_shutdown_receipt_is_rebarriered_before_retry_success() {
        use std::os::unix::fs::MetadataExt as _;

        let (dir, token, executable, genesis) = shutdown_fixture("receipt-visible-retry");
        let first = arm_desktop_shutdown_receipt(&dir.0, &token, &executable, &genesis).unwrap();
        let receipt = dir
            .0
            .join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)
            .join(DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME);
        let original_inode = std::fs::metadata(&receipt).unwrap().ino();

        let retried = arm_desktop_shutdown_receipt(&dir.0, &token, &executable, &genesis).unwrap();
        assert!(!retried.created_this_attempt);
        assert_eq!(retried.nonce, first.nonce);
        assert_ne!(
            std::fs::metadata(&receipt).unwrap().ino(),
            original_inode,
            "an existing visible receipt must cross a fresh replacement barrier"
        );
        assert!(
            validate_desktop_shutdown_receipt(&dir.0, &token, &first.nonce, &executable, &genesis)
                .unwrap()
        );
    }

    #[test]
    fn shutdown_ack_ignores_orphan_staging_from_reused_pid_and_nonce() {
        let (dir, token, executable, genesis) = shutdown_fixture("receipt-orphan-staging");
        let arm = arm_desktop_shutdown_receipt(&dir.0, &token, &executable, &genesis).unwrap();
        let old_staging = dir.0.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME).join(format!(
            ".{DESKTOP_SHUTDOWN_ACK_FILE_NAME}.{}.{}.tmp",
            std::process::id(),
            hex::encode(arm.nonce)
        ));
        durably_publish_new_private(&old_staging, b"torn-old-publication").unwrap();
        acknowledge_desktop_shutdown_receipt(&dir.0, &token, &arm.nonce, &executable, &genesis)
            .unwrap();
        assert!(
            validate_desktop_shutdown_ack(&dir.0, &token, &arm.nonce, &executable, &genesis)
                .unwrap()
        );
    }

    #[test]
    fn concurrent_receipt_arms_converge_without_overwrite() {
        let (dir, token, executable, genesis) = shutdown_fixture("receipt-concurrent");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let data = dir.0.clone();
            let executable = executable.clone();
            let genesis = genesis.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                arm_desktop_shutdown_receipt(&data, &token, &executable, &genesis).unwrap()
            }));
        }
        barrier.wait();
        let left = workers.remove(0).join().unwrap();
        let right = workers.remove(0).join().unwrap();
        assert_eq!(left.nonce, right.nonce);
        assert_ne!(left.created_this_attempt, right.created_this_attempt);
    }

    #[test]
    fn malformed_final_receipt_is_never_overwritten() {
        let (dir, token, executable, genesis) = shutdown_fixture("receipt-torn-final");
        let receipt = dir
            .0
            .join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)
            .join(DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME);
        durably_publish_new_private(&receipt, b"torn").unwrap();
        assert!(arm_desktop_shutdown_receipt(&dir.0, &token, &executable, &genesis).is_err());
        assert_eq!(std::fs::read(receipt).unwrap(), b"torn");
    }

    #[cfg(unix)]
    #[test]
    fn linked_shutdown_receipt_is_rejected_without_mutating_its_target() {
        use std::os::unix::fs::symlink;

        let (dir, token, executable, genesis) = shutdown_fixture("receipt-linked-final");
        let outside = dir.0.join("operator-owned-target");
        std::fs::write(&outside, b"must remain untouched").unwrap();
        let receipt = dir
            .0
            .join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)
            .join(DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME);
        symlink(&outside, &receipt).unwrap();

        assert!(arm_desktop_shutdown_receipt(&dir.0, &token, &executable, &genesis).is_err());
        assert!(
            std::fs::symlink_metadata(&receipt)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(outside).unwrap(), b"must remain untouched");
    }

    #[cfg(unix)]
    #[test]
    fn unix_private_file_rejects_permissive_mode_and_symlink() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let dir = TestDir::new("unix-policy");
        let path = dir.0.join("secret.json");
        let file = create_new_private(&path).unwrap();
        drop(file);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(open_private(&path).is_err());

        let victim = dir.0.join("victim");
        let victim_file = create_new_private(&victim).unwrap();
        drop(victim_file);
        let link = dir.0.join("link");
        symlink(&victim, &link).unwrap();
        assert!(open_private(&link).is_err());
        assert!(create_new_private(&link).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_private_file_requires_the_effective_uid_owner() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = TestDir::new("unix-owner");
        let path = dir.0.join("secret.json");
        drop(create_new_private(&path).unwrap());
        let actual_uid = std::fs::metadata(&path).unwrap().uid();
        let foreign_uid = if actual_uid == libc::uid_t::MAX {
            actual_uid - 1
        } else {
            actual_uid + 1
        };
        let error = platform::validate_owner_for_test(&path, foreign_uid).unwrap_err();
        assert!(error.to_string().contains("must be owned by effective uid"));

        // Exercise the full open boundary when the test runner has authority
        // to create a genuinely foreign-owned fixture.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            use std::os::unix::ffi::OsStrExt as _;
            let encoded = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: encoded is a live NUL-terminated path; uid 1 is foreign
            // to the root test process and the group is left unchanged.
            assert_eq!(unsafe { libc::chown(encoded.as_ptr(), 1, !0) }, 0);
            assert!(open_private(&path).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_private_directory_is_owned_mode_0700_and_no_follow() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let dir = TestDir::new("unix-directory");
        let identity = dir.0.join("identity");
        create_new_private_directory(&identity).unwrap();
        validate_private_directory(&identity).unwrap();
        assert_eq!(
            std::fs::metadata(&identity).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(create_new_private_directory(&identity).is_err());

        std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(validate_private_directory(&identity).is_err());
        std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o700)).unwrap();

        let link = dir.0.join("identity-link");
        symlink(&identity, &link).unwrap();
        assert!(validate_private_directory(&link).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_v07_store_modes_migrate_through_verified_handles() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = TestDir::new("unix-v07-store-migration");
        let legacy_dir = root.0.join("legacy-app-data");
        std::fs::create_dir(&legacy_dir).unwrap();
        std::fs::set_permissions(&legacy_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = legacy_dir.join("store.json");
        std::fs::write(&store, b"legacy identity store").unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o644)).unwrap();

        secure_private_directory(&legacy_dir).unwrap();
        assert_eq!(
            std::fs::metadata(&legacy_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(open_private_owned_migration(&store).unwrap());
        assert_eq!(
            std::fs::metadata(&store).unwrap().permissions().mode() & 0o777,
            0o600
        );
        open_private(&store).unwrap();

        let link = legacy_dir.join("store-link.json");
        symlink(&store, &link).unwrap();
        assert!(open_private_owned_migration(&link).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_extended_acl_is_rejected_and_secure_migration_strips_it() {
        use std::process::Command;

        let root = TestDir::new("macos-acl");
        secure_private_directory(&root.0).unwrap();
        let file = root.0.join("secret");
        drop(create_new_private(&file).unwrap());
        assert!(
            Command::new("chmod")
                .args(["+a", "everyone allow read"])
                .arg(&file)
                .status()
                .unwrap()
                .success()
        );
        assert!(open_private(&file).is_err());
        drop(open_private_owned_migration(&file).unwrap());
        open_private(&file).unwrap();

        let directory = root.0.join("private-child");
        create_new_private_directory(&directory).unwrap();
        assert!(
            Command::new("chmod")
                .args(["+a", "everyone allow list,search"])
                .arg(&directory)
                .status()
                .unwrap()
                .success()
        );
        assert!(validate_private_directory(&directory).is_err());
        secure_private_directory(&directory).unwrap();
        validate_private_directory(&directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_tree_rejects_existing_symlink_component() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("private-tree-symlink");
        let outside = root.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let linked = root.0.join("linked");
        symlink(&outside, &linked).unwrap();
        assert!(secure_private_directory_tree(&linked.join("control")).is_err());
        assert!(!outside.join("control").exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_tree_rejects_fully_existing_tree_beneath_intermediate_symlink() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("private-tree-existing-intermediate-symlink");
        let outside = root.0.join("outside");
        let child = outside.join("already-exists");
        std::fs::create_dir_all(&child).unwrap();
        let linked = root.0.join("linked");
        symlink(&outside, &linked).unwrap();
        assert!(secure_private_directory_tree(&linked.join("already-exists")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn private_directory_tree_rejects_fully_existing_tree_beneath_junction() {
        let root = TestDir::new("private-tree-existing-intermediate-junction");
        let outside = root.0.join("outside");
        let child = outside.join("already-exists");
        std::fs::create_dir_all(&child).unwrap();
        let linked = root.0.join("linked");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&linked)
            .arg(&outside)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "failed to create Windows junction fixture"
        );
        assert!(secure_private_directory_tree(&linked.join("already-exists")).is_err());
        std::fs::remove_dir(&linked).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_owned_inherited_directory_migrates_to_private_dacl() {
        let root = TestDir::new("windows-directory-migration");
        let inherited = root.0.join("legacy-app-data");
        std::fs::create_dir(&inherited).unwrap();
        assert!(
            validate_private_directory(&inherited).is_err(),
            "std-created fixture unexpectedly began with a protected private DACL"
        );
        secure_private_directory(&inherited).unwrap();
        validate_private_directory(&inherited).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_owned_inherited_file_migrates_to_private_dacl() {
        let root = TestDir::new("windows-file-migration");
        let inherited = root.0.join("legacy-store.json");
        std::fs::write(&inherited, b"legacy identity store").unwrap();
        assert!(
            open_private(&inherited).is_err(),
            "std-created fixture unexpectedly began with a protected private DACL"
        );
        drop(open_private_owned_migration(&inherited).unwrap());
        open_private(&inherited).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_append_only_private_handle_can_cross_a_flush_barrier() {
        let root = TestDir::new("windows-append-flush");
        let directory = root.0.join("private");
        create_private_directory_tree(&directory).unwrap();
        let path = directory.join("state.wal");
        assert!(durably_publish_new_private(&path, b"first").unwrap());

        let mut file = open_private_append_owned_migration(&path).unwrap();
        file.write_all(b"-second").unwrap();
        file.sync_all().unwrap();
        drop(file);

        let mut contents = Vec::new();
        open_private(&path)
            .unwrap()
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, b"first-second");
    }

    #[cfg(windows)]
    #[test]
    fn windows_extended_path_is_lexical_and_rejects_partial_or_device_paths() {
        let decoded = |path: &str| {
            let mut wide = super::windows_extended_path(Path::new(path)).unwrap();
            assert_eq!(wide.pop(), Some(0));
            String::from_utf16(&wide).unwrap()
        };

        assert_eq!(decoded(r"C:\arc\.\state\..\wal"), r"\\?\C:\arc\wal");
        assert_eq!(
            decoded(r"\\server\share\arc\..\state"),
            r"\\?\UNC\server\share\state"
        );
        assert!(super::windows_extended_path(Path::new(r"C:relative")).is_err());
        assert!(super::windows_extended_path(Path::new(r"\root-relative")).is_err());
        assert!(super::windows_extended_path(Path::new(r"\\.\PhysicalDrive0")).is_err());
        assert_eq!(decoded(r"\\?\C:\arc"), r"\\?\C:\arc");
        assert!(
            super::windows_extended_path(Path::new(r"\\?\GLOBALROOT\Device\Harddisk0")).is_err()
        );

        let relative = decoded(r"arc-relative\state.wal");
        assert!(relative.starts_with(r"\\?\"));
        assert!(relative.ends_with(r"\arc-relative\state.wal"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_private_publication_supports_extended_length_paths() {
        use std::os::windows::ffi::OsStrExt as _;

        let root = TestDir::new("windows-extended-private-path");
        let component = |tag: char| std::iter::repeat_n(tag, 90).collect::<String>();
        let deep = root
            .0
            .join(component('a'))
            .join(component('b'))
            .join(component('c'));
        assert!(
            deep.as_os_str().encode_wide().count() > 260,
            "fixture must exceed legacy MAX_PATH"
        );

        // Directory and file creation exercise the SDDL/SID conversion too;
        // those strings must remain raw UTF-16 rather than path-normalized.
        create_private_directory_tree(&deep).unwrap();
        validate_private_directory(&deep).unwrap();
        let path = deep.join("state.wal");
        assert!(durably_publish_new_private(&path, b"first").unwrap());
        durably_replace_private(&path, b"second").unwrap();
        let mut file = open_private(&path).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"second");
    }

    #[cfg(windows)]
    #[test]
    fn windows_new_private_objects_are_owned_by_the_exact_user_sid() {
        let root = TestDir::new("windows-explicit-user-owner");
        let private_directory = root.0.join("private");
        create_new_private_directory(&private_directory).unwrap();
        assert!(
            platform::directory_owner_is_current_user_for_test(&private_directory).unwrap(),
            "private-directory creation must override an administrator token's group default owner"
        );
        validate_private_directory(&private_directory).unwrap();

        let private_file = private_directory.join("secret");
        drop(create_new_private(&private_file).unwrap());
        assert!(
            platform::file_owner_is_current_user_for_test(&private_file).unwrap(),
            "private-file creation must override an administrator token's group default owner"
        );
        open_private(&private_file).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_private_directory_keeps_owned_chain_descendants_accessible() {
        let root = TestDir::new("windows-private-descendant-inheritance");
        let private_directory = root.0.join("private-app-data");
        create_new_private_directory(&private_directory).unwrap();

        // Chain directories are intentionally ordinary filesystem objects,
        // not secret-file primitives. They must inherit the same narrow trust
        // set so a hardened app-data parent cannot strand preserved WAL bytes
        // behind an empty/non-inheritable DACL.
        let chain_directory = private_directory.join("data-v3");
        std::fs::create_dir(&chain_directory).unwrap();
        let wal = chain_directory.join("state.wal");
        std::fs::write(&wal, b"preserved ARC chain history").unwrap();
        assert_eq!(std::fs::read(&wal).unwrap(), b"preserved ARC chain history");
        assert!(
            std::fs::symlink_metadata(&chain_directory)
                .unwrap()
                .is_dir()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_owner_policy_rejects_every_sid_outside_the_explicit_trust_set() {
        let user = "S-1-5-21-111-222-333-1001";
        assert!(platform::owner_policy_accepts_for_test(user, user).unwrap());
        assert!(
            platform::owner_policy_accepts_for_test("S-1-5-32-544", user).unwrap(),
            "Administrators is already an explicit full-control DACL trustee"
        );
        assert!(!platform::owner_policy_accepts_for_test("S-1-5-32-545", user).unwrap());
        assert!(!platform::owner_policy_accepts_for_test("S-1-5-18", user).unwrap());
        assert!(
            !platform::owner_policy_accepts_for_test("S-1-5-21-111-222-333-1002", user).unwrap()
        );
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetTokenInformation,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
        OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: this RAII value owns the successful Win32 handle.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from a LocalAlloc-returning Win32 API.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    struct OwnedSid {
        storage: Vec<usize>,
    }

    impl OwnedSid {
        fn copy_from(sid: PSID) -> io::Result<Self> {
            if sid.is_null() {
                return Err(permission_error("Windows account SID is null"));
            }
            // SAFETY: caller supplies a SID returned by a successful Win32 API.
            let length = unsafe { GetLengthSid(sid) } as usize;
            if length == 0 {
                return Err(io::Error::last_os_error());
            }
            let words = length.div_ceil(size_of::<usize>());
            let mut storage = vec![0usize; words];
            // SAFETY: destination owns at least `length` bytes; SID length was
            // obtained from the same validated SID pointer.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    sid.cast::<u8>(),
                    storage.as_mut_ptr().cast::<u8>(),
                    length,
                );
            }
            Ok(Self { storage })
        }

        fn as_ptr(&self) -> PSID {
            self.storage.as_ptr().cast_mut().cast::<c_void>()
        }
    }

    fn wide(value: &std::ffi::OsStr) -> io::Result<Vec<u16>> {
        let mut encoded = value.encode_wide().collect::<Vec<_>>();
        if encoded.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows string contains an interior NUL",
            ));
        }
        encoded.push(0);
        Ok(encoded)
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        super::windows_extended_path(path)
    }

    fn current_user_sid() -> io::Result<OwnedSid> {
        let mut token = null_mut();
        // SAFETY: output pointer is valid and initialized; pseudo-process
        // handle is provided by the OS.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(token);
        let mut length = 0u32;
        // SAFETY: the documented sizing call accepts a null buffer.
        unsafe {
            GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut length);
        }
        if length < size_of::<TOKEN_USER>() as u32 {
            return Err(io::Error::last_os_error());
        }
        let words = (length as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0usize; words];
        // SAFETY: the aligned buffer has the exact requested byte capacity.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful TokenUser output begins with TOKEN_USER.
        let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
        OwnedSid::copy_from(user.User.Sid)
    }

    fn sid_string(sid: PSID) -> io::Result<String> {
        let mut value = null_mut();
        // SAFETY: SID is valid and the API initializes the LocalAlloc pointer.
        if unsafe { ConvertSidToStringSidW(sid, &mut value) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let allocation = LocalAllocation(value.cast::<c_void>());
        let mut length = 0usize;
        // SAFETY: the API returned a NUL-terminated UTF-16 string.
        unsafe {
            while *value.add(length) != 0 {
                length += 1;
            }
        }
        // SAFETY: `length` was bounded by the first NUL in the API allocation.
        let text = String::from_utf16(unsafe { std::slice::from_raw_parts(value, length) })
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Windows SID is not UTF-16"))?;
        drop(allocation);
        Ok(text)
    }

    fn parse_sid(value: &str) -> io::Result<LocalAllocation> {
        let wide = wide(std::ffi::OsStr::new(value))?;
        let mut sid = null_mut();
        // SAFETY: input is NUL-terminated and output receives a LocalAlloc SID.
        if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(LocalAllocation(sid))
    }

    fn private_security_attributes() -> io::Result<(LocalAllocation, SECURITY_ATTRIBUTES)> {
        let user = current_user_sid()?;
        let user_text = sid_string(user.as_ptr())?;
        // An administrator token's Windows default-owner SID is normally the
        // built-in Administrators group, not its TOKEN_USER SID. Pin the owner
        // explicitly so every newly created ARC secret remains owned by the
        // exact account even when the process is elevated.
        // Keep the private trust boundary inheritable inside app-owned
        // directories. Without OI/CI, tightening an existing Windows app
        // data directory leaves subsequently created ordinary descendants
        // (for example `data-v3` and its WAL) with no usable inherited ACEs;
        // the creating account can then receive ERROR_ACCESS_DENIED when it
        // reopens its own chain history. Inheritance does not widen trust:
        // only the exact user, LocalSystem, and Administrators receive the
        // full-control ACEs, and every secret file/directory created by this
        // module is still born with its own protected DACL and revalidated.
        let sddl =
            format!("O:{user_text}D:P(A;OICI;FA;;;{user_text})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
        let sddl = wide(std::ffi::OsStr::new(&sddl))?;
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: input is NUL-terminated and output receives a LocalAlloc
        // security descriptor.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let descriptor_guard = LocalAllocation(descriptor);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        Ok((descriptor_guard, attributes))
    }

    pub(super) fn create_new_private(path: &Path) -> io::Result<File> {
        let (descriptor_guard, attributes) = private_security_attributes()?;
        let path_wide = wide_path(path)?;
        // SAFETY: all pointers remain alive for the call; CREATE_NEW prevents
        // overwrite and OPEN_REPARSE_POINT prevents traversal of a final link.
        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
                0,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        drop(descriptor_guard);
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful handle is uniquely transferred to File.
        let file = unsafe { File::from_raw_handle(handle.cast::<c_void>()) };
        validate_private(&file, path)?;
        Ok(file)
    }

    pub(super) fn create_new_private_directory(path: &Path) -> io::Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        // A deterministic destination-bound staging name makes directory
        // creation resumable and bounds crash residue to one empty directory.
        // Callers that need to rebarrier an already-live semantic directory
        // additionally hold `PrivateDirectoryNamespaceLock`.
        let staging = parent.join(format!(
            ".arc-private-directory-create-{}.staging",
            super::namespace_path_digest(path)?
        ));
        let (descriptor_guard, attributes) = private_security_attributes()?;
        let staging_wide = wide_path(&staging)?;
        // SAFETY: path and descriptor remain live for the call. CreateDirectoryW
        // fails if the staging path already exists and applies the protected
        // DACL before another process can open the directory.
        let created = unsafe { CreateDirectoryW(staging_wide.as_ptr(), &attributes) };
        drop(descriptor_guard);
        if created == 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        if let Err(error) = validate_private_directory(&staging) {
            if created != 0 {
                let _ = std::fs::remove_dir(&staging);
            }
            return Err(error);
        }
        if std::fs::read_dir(&staging)?.next().transpose()?.is_some() {
            return Err(permission_error(format!(
                "private directory creation staging is not empty: {}",
                staging.display()
            )));
        }
        if let Err(error) = super::windows_move_path_write_through(&staging, path, false) {
            if created != 0 {
                let _ = std::fs::remove_dir(&staging);
            }
            return Err(error);
        }
        validate_private_directory(path)
    }

    pub(super) fn open_private(path: &Path) -> io::Result<File> {
        let file = open_private_raw(path)?;
        validate_private(&file, path)?;
        Ok(file)
    }

    pub(super) fn open_private_read_write(path: &Path) -> io::Result<File> {
        let file = open_private_raw_with_access_and_share(
            path,
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        )?;
        validate_private(&file, path)?;
        Ok(file)
    }

    pub(super) fn open_private_append_owned_migration(path: &Path) -> io::Result<File> {
        let file = open_private_raw_with_access(
            path,
            GENERIC_READ | (FILE_GENERIC_WRITE & !FILE_WRITE_DATA) | READ_CONTROL | WRITE_DAC,
        )?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(permission_error(format!(
                "private file is not a non-reparse regular file: {}",
                path.display()
            )));
        }
        validate_private_owner(&file, path, "file")?;
        if validate_private_security(&file, path, "file").is_err() {
            apply_private_dacl(&file)?;
        }
        validate_private_security(&file, path, "file")?;
        Ok(file)
    }

    pub(super) fn open_owned_nofollow_read(path: &Path) -> io::Result<File> {
        let file = open_private_raw_with_access(path, GENERIC_READ | READ_CONTROL | WRITE_DAC)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(permission_error(format!(
                "owner-controlled file is not a non-reparse regular file: {}",
                path.display()
            )));
        }
        validate_private_owner(&file, path, "file")?;
        Ok(file)
    }

    pub(super) fn tighten_open_owned_private(file: &File, path: &Path) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(permission_error(format!(
                "owner-controlled file is not a non-reparse regular file: {}",
                path.display()
            )));
        }
        validate_private_owner(file, path, "file")?;
        if validate_private_security(file, path, "file").is_err() {
            apply_private_dacl(file)?;
        }
        validate_private_security(file, path, "file")
    }

    fn open_private_raw(path: &Path) -> io::Result<File> {
        open_private_raw_with_access(path, GENERIC_READ | READ_CONTROL)
    }

    fn open_private_raw_with_access(path: &Path, desired_access: u32) -> io::Result<File> {
        open_private_raw_with_access_and_share(
            path,
            desired_access,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
        )
    }

    fn open_private_raw_with_access_and_share(
        path: &Path,
        desired_access: u32,
        share_mode: u32,
    ) -> io::Result<File> {
        let path_wide = wide_path(path)?;
        // SAFETY: input is a live NUL-terminated path. OPEN_REPARSE_POINT
        // opens the link itself so validation can reject it.
        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                desired_access,
                share_mode,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful handle is uniquely transferred to File.
        Ok(unsafe { File::from_raw_handle(handle.cast::<c_void>()) })
    }

    fn equal_sid(left: PSID, right: PSID) -> bool {
        // SAFETY: both pointers refer to validated SID allocations.
        unsafe { EqualSid(left, right) != 0 }
    }

    fn owner_matches_private_policy(owner: PSID, current_user: PSID, administrators: PSID) -> bool {
        !owner.is_null() && (equal_sid(owner, current_user) || equal_sid(owner, administrators))
    }

    pub(super) fn validate_private(file: &File, path: &Path) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(permission_error(format!(
                "private file is not a non-reparse regular file: {}",
                path.display()
            )));
        }
        validate_private_security(file, path, "file")
    }

    pub(super) fn remove_private_while_open(file: &File, path: &Path) -> io::Result<()> {
        validate_private(file, path)?;
        let path_wide = wide_path(path)?;
        // The retained validated handle was opened with FILE_SHARE_DELETE.
        // DeleteFileW marks this exact still-existing name for deletion; a
        // CREATE_NEW publisher cannot install a replacement until the handle
        // closes and the old name is gone.
        if unsafe { windows_sys::Win32::Storage::FileSystem::DeleteFileW(path_wide.as_ptr()) } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn durably_remove_private_while_open(file: &File, path: &Path) -> io::Result<()> {
        use rand::RngCore as _;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };

        validate_private(file, path)?;
        let mut random = [0u8; 8];
        rand::rngs::OsRng.fill_bytes(&mut random);
        let tombstone = path.with_file_name(format!(
            ".{}.{}.{}.tombstone",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("private"),
            std::process::id(),
            hex::encode(random)
        ));
        super::windows_move_path_write_through(path, &tombstone, false)?;

        // Re-open the tombstone and compare stable volume/file indices with
        // the retained validated source handle before treating disappearance
        // of the final name as durable.
        let tombstone_file = open_private(&tombstone)?;
        let identity = |candidate: &File| -> io::Result<(u32, u64)> {
            let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                GetFileInformationByHandle(candidate.as_raw_handle().cast(), &mut information)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok((
                information.dwVolumeSerialNumber,
                (u64::from(information.nFileIndexHigh) << 32)
                    | u64::from(information.nFileIndexLow),
            ))
        };
        if identity(file)? != identity(&tombstone_file)? {
            return Err(permission_error(
                "write-through tombstone does not name the validated private file",
            ));
        }
        drop(tombstone_file);
        // Deletion is cleanup only. If it is interrupted, the uniquely named
        // private tombstone is fail-safe and cannot be mistaken for an armed
        // receipt or a clean ACK.
        let _ = std::fs::remove_file(&tombstone);
        Ok(())
    }

    pub(super) fn validate_private_directory(path: &Path) -> io::Result<()> {
        let directory = open_private_directory_raw(path)?;
        validate_private_directory_handle(&directory, path)
    }

    fn open_private_directory_raw(path: &Path) -> io::Result<File> {
        open_private_directory_raw_with_access(path, READ_CONTROL)
    }

    fn open_private_directory_raw_with_access(
        path: &Path,
        desired_access: u32,
    ) -> io::Result<File> {
        let path_wide = wide_path(path)?;
        // SAFETY: OPEN_REPARSE_POINT opens a final directory reparse point
        // itself; BACKUP_SEMANTICS permits opening a real directory handle.
        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                desired_access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful directory handle is uniquely transferred.
        Ok(unsafe { File::from_raw_handle(handle.cast::<c_void>()) })
    }

    pub(super) fn validate_directory_component_no_link(path: &Path) -> io::Result<()> {
        // Open this component as the final path with OPEN_REPARSE_POINT, then
        // reject every reparse tag (junctions included), not only Rust's
        // `is_symlink` subset.
        let directory = open_private_directory_raw_with_access(path, 0)?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(permission_error(format!(
                "private directory tree contains a reparse/non-directory component: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn validate_private_directory_handle(directory: &File, path: &Path) -> io::Result<()> {
        let metadata = directory.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(permission_error(format!(
                "private directory is not a non-reparse directory: {}",
                path.display()
            )));
        }
        validate_private_security(&directory, path, "directory")
    }

    fn validate_private_owner(file: &File, path: &Path, object: &str) -> io::Result<()> {
        let current_user = current_user_sid()?;
        let administrators = parse_sid("S-1-5-32-544")?;
        let mut owner: PSID = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the handle remains owned by `file`; GetSecurityInfo returns
        // one LocalAlloc descriptor containing the owner SID.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle().cast::<c_void>(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _descriptor_guard = LocalAllocation(descriptor);
        // Windows deliberately uses BUILTIN\Administrators as the default
        // owner for administrator tokens. Accepting that one group does not
        // expand this module's trust boundary: the protected DACL below
        // already grants that exact SID full control. Every other group or
        // account owner remains rejected.
        if !owner_matches_private_policy(owner, current_user.as_ptr(), administrators.0) {
            return Err(permission_error(format!(
                "private {object} is not owned by the current Windows user or trusted Administrators group: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn apply_private_dacl(file: &File) -> io::Result<()> {
        let (descriptor_guard, attributes) = private_security_attributes()?;
        let mut dacl_present = 0;
        let mut dacl_defaulted = 0;
        let mut dacl: *mut ACL = null_mut();
        // SAFETY: the protected descriptor remains live through SetSecurityInfo.
        if unsafe {
            GetSecurityDescriptorDacl(
                attributes.lpSecurityDescriptor,
                &mut dacl_present,
                &mut dacl,
                &mut dacl_defaulted,
            )
        } == 0
            || dacl_present == 0
            || dacl.is_null()
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the caller owns the object, the handle remains live, and
        // `dacl` is backed by descriptor_guard until this call returns.
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle().cast::<c_void>(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        drop(descriptor_guard);
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    pub(super) fn secure_private_directory(path: &Path) -> io::Result<()> {
        let directory = match open_private_directory_raw_with_access(path, READ_CONTROL | WRITE_DAC)
        {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return create_new_private_directory(path);
            }
            Err(error) => return Err(error),
        };
        let metadata = directory.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(permission_error(format!(
                "private directory is not a non-reparse directory: {}",
                path.display()
            )));
        }
        validate_private_owner(&directory, path, "directory")?;
        if validate_private_security(&directory, path, "directory").is_err() {
            apply_private_dacl(&directory)?;
        }
        validate_private_security(&directory, path, "directory")
    }

    pub(super) fn open_private_owned_migration(path: &Path) -> io::Result<File> {
        let file = open_private_raw_with_access(path, GENERIC_READ | READ_CONTROL | WRITE_DAC)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(permission_error(format!(
                "private file is not a non-reparse regular file: {}",
                path.display()
            )));
        }
        validate_private_owner(&file, path, "file")?;
        if validate_private_security(&file, path, "file").is_err() {
            apply_private_dacl(&file)?;
        }
        validate_private_security(&file, path, "file")?;
        Ok(file)
    }

    fn validate_private_security(file: &File, path: &Path, object: &str) -> io::Result<()> {
        let current_user = current_user_sid()?;
        let system = parse_sid("S-1-5-18")?;
        let administrators = parse_sid("S-1-5-32-544")?;
        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the raw handle remains owned by `file`; output pointers are
        // valid and the returned descriptor is freed below.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle().cast::<c_void>(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _descriptor_guard = LocalAllocation(descriptor);
        if !owner_matches_private_policy(owner, current_user.as_ptr(), administrators.0) {
            return Err(permission_error(format!(
                "private {object} is not owned by the current Windows user or trusted Administrators group: {}",
                path.display()
            )));
        }
        if dacl.is_null() {
            return Err(permission_error(format!(
                "private {object} has a null (world-accessible) Windows DACL: {}",
                path.display()
            )));
        }
        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: descriptor came from successful GetSecurityInfo.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Err(permission_error(format!(
                "private {object} Windows DACL inherits from its parent: {}",
                path.display()
            )));
        }

        let mut current_user_full_control = false;
        // SAFETY: non-null ACL came from the security descriptor.
        let ace_count = unsafe { (*dacl).AceCount } as u32;
        for index in 0..ace_count {
            let mut raw_ace = null_mut();
            // SAFETY: index is bounded by the ACL's AceCount.
            if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful GetAce returns at least ACE_HEADER.
            let header = unsafe { &*(raw_ace.cast::<ACE_HEADER>()) };
            if header.AceType as u32 == ACCESS_DENIED_ACE_TYPE {
                continue;
            }
            if header.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE
                || (header.AceSize as usize) < size_of::<ACCESS_ALLOWED_ACE>()
            {
                return Err(permission_error(format!(
                    "private {object} has an unsupported Windows allow ACE: {}",
                    path.display()
                )));
            }
            // SAFETY: type and minimum size were checked above.
            let ace = unsafe { &*(raw_ace.cast::<ACCESS_ALLOWED_ACE>()) };
            let sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
            let is_current = equal_sid(sid, current_user.as_ptr());
            let is_system = equal_sid(sid, system.0);
            let is_admin = equal_sid(sid, administrators.0);
            if !(is_current || is_system || is_admin) {
                return Err(permission_error(format!(
                    "private {object} grants Windows access outside the current user, LocalSystem, and Administrators: {}",
                    path.display()
                )));
            }
            if ace.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS {
                return Err(permission_error(format!(
                    "private {object} has a noncanonical Windows access rule: {}",
                    path.display()
                )));
            }
            current_user_full_control |= is_current;
        }
        if !current_user_full_control {
            return Err(permission_error(format!(
                "private {object} does not grant the current Windows user full control: {}",
                path.display()
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn owner_policy_accepts_for_test(owner: &str, user: &str) -> io::Result<bool> {
        let owner = parse_sid(owner)?;
        let user = parse_sid(user)?;
        let administrators = parse_sid("S-1-5-32-544")?;
        Ok(owner_matches_private_policy(
            owner.0,
            user.0,
            administrators.0,
        ))
    }

    #[cfg(test)]
    fn handle_owner_is_current_user_for_test(handle: &File) -> io::Result<bool> {
        let current_user = current_user_sid()?;
        let mut owner: PSID = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the object handle remains live and GetSecurityInfo owns
        // the returned LocalAlloc descriptor until the guard below is dropped.
        let status = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle().cast::<c_void>(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _descriptor_guard = LocalAllocation(descriptor);
        Ok(!owner.is_null() && equal_sid(owner, current_user.as_ptr()))
    }

    #[cfg(test)]
    pub(super) fn directory_owner_is_current_user_for_test(path: &Path) -> io::Result<bool> {
        let directory = open_private_directory_raw(path)?;
        handle_owner_is_current_user_for_test(&directory)
    }

    #[cfg(test)]
    pub(super) fn file_owner_is_current_user_for_test(path: &Path) -> io::Result<bool> {
        let file = open_private_raw(path)?;
        handle_owner_is_current_user_for_test(&file)
    }
}
