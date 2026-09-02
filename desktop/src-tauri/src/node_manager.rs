use crate::paths;
use crate::types::{DataMigrationNotice, LogEntry, NodeConfig};
use anyhow::Context as _;
use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};
use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use zeroize::{Zeroize as _, Zeroizing};

const LOG_RING_SIZE: usize = 2000;
// Keep this synchronized with arc-node's managed shutdown contract and the
// systemd/launchd services emitted by install.sh. A public inference handler
// may validly run for 4,000 seconds and an already-claimed community job keeps
// a 300-second late-submit window; the remaining two minutes cover task joins
// and the final WAL durability barrier. Idle nodes still exit immediately.
const GRACEFUL_STOP_TIMEOUT_SECS: u64 = 4_420;
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(GRACEFUL_STOP_TIMEOUT_SECS);
const FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const DESKTOP_SHUTDOWN_CONTROL_DIR_NAME: &str =
    arc_crypto::secret_file::DESKTOP_SHUTDOWN_CONTROL_DIR_NAME;
const DESKTOP_SHUTDOWN_TOKEN_FILE_NAME: &str = "token";
const DESKTOP_SHUTDOWN_REQUEST_FILE_NAME: &str = "request";
const DESKTOP_SHUTDOWN_REQUEST_SCHEMA: &str = "arc.desktop.shutdown.v1";
const DESKTOP_LIFECYCLE_LOCK_FILE_NAME: &str =
    arc_crypto::secret_file::DESKTOP_LIFECYCLE_LOCK_FILE_NAME;
const DESKTOP_EXECUTABLE_IDENTITY_FILE_NAME: &str = "managed-executable.path";
const DESKTOP_EXECUTABLE_IDENTITY_SCHEMA: &str = "arc.desktop.executable-path.v1";
const DESKTOP_EXECUTABLE_IDENTITY_MAX_BYTES: u64 = 32 * 1024;
const DESKTOP_SHUTDOWN_FILE_MAX_BYTES: u64 = 256;
const DESKTOP_NETWORK_IDENTITY_DIR_NAME: &str = ".arc-desktop-network";
const DESKTOP_STABLE_SEEDS_FILE_NAME: &str = "testnet-seeds.txt";
const DESKTOP_STABLE_GENESIS_FILE_NAME: &str = "genesis.toml";
const DESKTOP_NETWORK_RESOURCE_MAX_BYTES: u64 = 4 * 1024 * 1024;
const DESKTOP_SHUTDOWN_PUBLICATION_RETRIES: usize = 250;
const DESKTOP_SHUTDOWN_PUBLICATION_RETRY_DELAY: Duration = Duration::from_millis(20);
const LEGACY_VALIDATOR_SEED_MAX_BYTES: usize = 1_024;

fn refresh_process_command_metadata(system: &mut sysinfo::System) {
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::new()
            .with_exe(sysinfo::UpdateKind::OnlyIfNotSet)
            .with_cmd(sysinfo::UpdateKind::OnlyIfNotSet)
            .with_cwd(sysinfo::UpdateKind::OnlyIfNotSet),
    );
}

struct DesktopShutdownControl {
    data_dir: PathBuf,
    token_file: PathBuf,
    request_file: PathBuf,
    #[cfg_attr(not(windows), allow(dead_code))]
    token: Zeroizing<String>,
    receipt_executable: Option<PathBuf>,
    receipt_genesis: Option<PathBuf>,
    receipt_nonce: Option<[u8; 32]>,
}

/// Cross-process ownership of one canonical managed data directory. The lock
/// is acquired before the final lifecycle-state check and is held across any
/// executable/stable-resource mutation plus receipt arm→spawn. A running
/// NodeManager retains it for the child's full lifetime; the OS releases it if
/// the GUI crashes, allowing a new GUI to reconcile the still-fenced child.
pub struct ManagedLifecycleLock {
    data_dir: PathBuf,
    _file: std::fs::File,
    namespace_guard: Option<arc_crypto::secret_file::PrivateDirectoryNamespaceLock>,
    namespace_prepared: bool,
    session_nonce: Option<[u8; 32]>,
}

#[derive(Clone)]
struct ExactLaunchFile {
    path: PathBuf,
    sha256: [u8; 32],
}

impl ExactLaunchFile {
    fn capture(path: &Path, name: &str) -> anyhow::Result<Self> {
        let path = path.canonicalize().with_context(|| {
            format!(
                "cannot canonicalize managed launch {name} {}",
                path.display()
            )
        })?;
        anyhow::ensure!(
            std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()),
            "managed launch {name} is not a regular file: {}",
            path.display()
        );
        Ok(Self {
            sha256: regular_file_sha256(&path)
                .with_context(|| format!("cannot hash managed launch {name} {}", path.display()))?,
            path,
        })
    }

    fn verify(&self, name: &str) -> anyhow::Result<PathBuf> {
        let path = self.path.canonicalize().with_context(|| {
            format!(
                "previously running managed-node {name} is unavailable: {}",
                self.path.display()
            )
        })?;
        anyhow::ensure!(
            path == self.path,
            "previously running managed-node {name} path changed"
        );
        anyhow::ensure!(
            std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()),
            "previously running managed-node {name} is not a regular file"
        );
        anyhow::ensure!(
            regular_file_sha256(&path)? == self.sha256,
            "previously running managed-node {name} bytes changed during the update attempt"
        );
        Ok(path)
    }
}

#[derive(Clone)]
struct ManagedLaunchPlan {
    config: NodeConfig,
    validator_keyfile: ExactLaunchFile,
    binary: ExactLaunchFile,
    seeds: ExactLaunchFile,
    genesis: ExactLaunchFile,
}

impl ManagedLaunchPlan {
    fn capture(
        config: &NodeConfig,
        validator_keyfile: &Path,
        binary: &Path,
        resources: &TestnetResources,
    ) -> anyhow::Result<Self> {
        let (seeds, genesis) = required_testnet_resources(resources)?;
        Ok(Self {
            config: config.clone(),
            validator_keyfile: ExactLaunchFile::capture(validator_keyfile, "validator key")?,
            binary: ExactLaunchFile::capture(binary, "executable")?,
            seeds: ExactLaunchFile::capture(seeds, "seed identity")?,
            genesis: ExactLaunchFile::capture(genesis, "genesis identity")?,
        })
    }

    fn verify(
        &self,
        lifecycle_lock: &ManagedLifecycleLock,
    ) -> anyhow::Result<(PathBuf, PathBuf, TestnetResources)> {
        let data_dir = resolve_data_dir(&self.config.data_dir)
            .canonicalize()
            .context("cannot canonicalize the previously running managed-node data directory")?;
        lifecycle_lock.ensure_data_dir(&data_dir)?;
        let validator_keyfile = self.validator_keyfile.verify("validator key")?;
        let binary = self.binary.verify("executable")?;
        let seeds_file = self.seeds.verify("seed identity")?;
        let genesis_file = self.genesis.verify("genesis identity")?;
        Ok((
            validator_keyfile,
            binary,
            TestnetResources {
                seeds_file: Some(seeds_file),
                genesis_file: Some(genesis_file),
            },
        ))
    }
}

fn regular_file_sha256(path: &Path) -> anyhow::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    anyhow::ensure!(file.metadata()?.is_file(), "file is not regular");
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

impl ManagedLifecycleLock {
    fn ensure_data_dir(&self, data_dir: &Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.data_dir == data_dir,
            "managed lifecycle lock does not belong to data directory {}",
            data_dir.display()
        );
        anyhow::ensure!(
            self.namespace_prepared && self.session_nonce.is_some(),
            "managed data-directory namespace has not completed its durability barrier"
        );
        Ok(())
    }

    fn session_nonce(&self) -> anyhow::Result<&[u8; 32]> {
        self.session_nonce.as_ref().ok_or_else(|| {
            anyhow::anyhow!("managed lifecycle lease has no prepared desktop handoff nonce")
        })
    }
}

fn has_exact_lifecycle_namespace_proof(
    data_dir: &Path,
    session_nonce: &[u8; 32],
) -> anyhow::Result<bool> {
    arc_crypto::secret_file::has_exact_desktop_lifecycle_namespace_proof(data_dir, session_nonce)
        .with_context(|| {
            format!(
                "cannot validate managed lifecycle namespace proof beneath {}",
                data_dir.display()
            )
        })
}

fn validate_configured_directory_ancestry_no_link(path: &Path) -> anyhow::Result<()> {
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                #[cfg(windows)]
                let is_link = {
                    use std::os::windows::fs::MetadataExt as _;
                    metadata.file_attributes()
                        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                        != 0
                };
                #[cfg(not(windows))]
                let is_link = metadata.file_type().is_symlink();
                anyhow::ensure!(
                    !is_link && metadata.is_dir(),
                    "managed data-directory ancestry contains a linked/non-directory component: {}",
                    ancestor.display()
                );
            }
            // The configured leaf may validly be absent. The existing parent
            // requirement below still rejects a missing custom ancestor, while
            // the managed-default helper may create exactly ~/.arc.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot inspect managed data-directory ancestry component {}",
                        ancestor.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn is_managed_default_data_namespace(data_dir: &Path, managed_root: &Path) -> bool {
    let expected = managed_root.join("data-v3");
    if data_dir == expected {
        return true;
    }
    let Some(data_leaf) = data_dir.file_name() else {
        return false;
    };
    #[cfg(windows)]
    let same_leaf = data_leaf.to_string_lossy().to_uppercase() == "DATA-V3";
    #[cfg(not(windows))]
    let same_leaf = data_leaf == std::ffi::OsStr::new("data-v3");
    if !same_leaf {
        return false;
    }
    let Some(data_parent) = data_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return false;
    };
    // Compare the parent directory entries by the same stable identity used
    // for their namespace locks. This catches Windows case variants without
    // following a different custom leaf target. A missing custom ancestor is
    // intentionally left to the existing custom-parent validation below.
    arc_crypto::secret_file::same_private_directory_namespace(data_parent, managed_root)
        .unwrap_or(false)
}

fn prepare_first_launch_managed_root_at(
    data_dir: &Path,
    managed_root: &Path,
) -> anyhow::Result<Option<arc_crypto::secret_file::PrivateDirectoryNamespaceLock>> {
    if !is_managed_default_data_namespace(data_dir, managed_root) {
        return Ok(None);
    }
    match std::fs::symlink_metadata(managed_root) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                "managed ARC root is not a regular non-link directory: {}",
                managed_root.display()
            );
            match std::fs::symlink_metadata(data_dir) {
                // Once the managed child exists, its own namespace/lifecycle
                // protocol proves every subsequent startup. Before then, a
                // visible root may be the late-visible result of a failed
                // first publication and must be rebarriered below.
                Ok(_) => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // The packaged desktop owns exactly ~/.arc, whose immediate parent is the
    // already-existing OS user home. Reserve and restore that one namespace
    // before its first creation so a crash cannot strand the whole managed
    // binary/data tree under a hidden Windows rebarrier name. Arbitrary custom
    // data parents remain operator-owned and must already exist.
    let namespace_lock =
        arc_crypto::secret_file::acquire_private_directory_namespace_lock(managed_root)
            .with_context(|| {
                format!(
                    "cannot lock first-launch ARC root namespace {}",
                    managed_root.display()
                )
            })?;
    namespace_lock.restore_interrupted().with_context(|| {
        format!(
            "cannot restore interrupted first-launch ARC root {}",
            managed_root.display()
        )
    })?;

    match std::fs::symlink_metadata(managed_root) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                "managed ARC root is not a regular non-link directory: {}",
                managed_root.display()
            );
            arc_crypto::secret_file::secure_private_directory(managed_root)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            arc_crypto::secret_file::secure_private_directory(managed_root)?;
        }
        Err(error) => return Err(error.into()),
    }
    // Re-check after acquiring the root guard. Another first-launch process
    // may have completed the child while this one waited. In that case its
    // successful child creation depended on an already-proven root. When the
    // child is still absent, repeat the root barrier now, including the retry
    // where an earlier write-through move made the root visible but reported
    // a late failure.
    match std::fs::symlink_metadata(data_dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            namespace_lock.rebarrier_existing().with_context(|| {
                format!(
                    "cannot durably publish first-launch ARC root {}",
                    managed_root.display()
                )
            })?;
        }
        Err(error) => return Err(error.into()),
    }
    // Retain this guard through the data child and internal lifecycle-lock
    // publication. Dropping it here would reopen a window in which another
    // first-launch process could move the parent while this caller creates
    // the child beneath it.
    Ok(Some(namespace_lock))
}

pub fn acquire_managed_lifecycle_lock(
    configured_data_dir: &str,
) -> anyhow::Result<ManagedLifecycleLock> {
    refresh_managed_lifecycle_namespace(acquire_managed_lifecycle_lock_for_reconciliation(
        configured_data_dir,
    )?)
}

/// Acquire a provisional desktop-only lease while retaining the stable outer
/// namespace guard. Startup reconciliation uses this form to drain an exact
/// orphaned child before Windows can rename the data directory. The lease is
/// deliberately not accepted by launch/mutation entrypoints and must be
/// converted by `refresh_managed_lifecycle_namespace` after every writer is
/// gone.
fn acquire_managed_lifecycle_lock_for_reconciliation(
    configured_data_dir: &str,
) -> anyhow::Result<ManagedLifecycleLock> {
    let data_dir = resolve_data_dir(configured_data_dir);
    // Validate the pathname exactly as configured before canonicalization or
    // namespace-lock acquisition. Otherwise a custom symlinked parent is
    // followed first and the lock sidecar mutates its external target before
    // the later private-tree walk rejects the link.
    validate_configured_directory_ancestry_no_link(&data_dir)?;
    let _managed_root_namespace_lock =
        prepare_first_launch_managed_root_at(&data_dir, &paths::arc_home())?;
    let data_parent = data_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = data_parent.canonicalize().with_context(|| {
        format!(
            "managed data-directory parent {} must already exist; the installer or operator must create it before node start",
            data_parent.display()
        )
    })?;
    anyhow::ensure!(
        canonical_parent.is_dir(),
        "managed data-directory parent is not a directory: {}",
        data_parent.display()
    );
    let data_namespace_lock = arc_crypto::secret_file::acquire_private_directory_namespace_lock(
        &data_dir,
    )
    .with_context(|| {
        format!(
            "cannot lock managed data-directory namespace {}",
            data_dir.display()
        )
    })?;
    data_namespace_lock.restore_interrupted().with_context(|| {
        format!(
            "cannot restore interrupted managed data-directory namespace {}",
            data_dir.display()
        )
    })?;
    arc_crypto::secret_file::secure_private_directory_tree(&data_dir).with_context(|| {
        format!(
            "cannot durably secure managed data directory before lifecycle lock: {}",
            data_dir.display()
        )
    })?;
    let data_dir = data_dir.canonicalize()?;
    let locked_target = data_namespace_lock.target();
    let same_namespace = data_dir == locked_target
        || arc_crypto::secret_file::same_private_directory_namespace(&data_dir, locked_target)?;
    anyhow::ensure!(
        same_namespace,
        "managed data directory resolved outside its locked namespace: {}",
        data_dir.display()
    );
    let control_dir = data_dir.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
    let control_namespace_lock =
        arc_crypto::secret_file::acquire_private_directory_namespace_lock(&control_dir)
            .with_context(|| {
                format!(
                    "cannot lock managed shutdown-control namespace {}",
                    control_dir.display()
                )
            })?;
    control_namespace_lock
        .restore_interrupted()
        .with_context(|| {
            format!(
                "cannot restore interrupted managed shutdown-control namespace {}",
                control_dir.display()
            )
        })?;
    arc_crypto::secret_file::secure_private_directory_tree(&control_dir)?;
    // A malformed current proof is never migration input. Missing, moved, or
    // retired proofs are safe here because this provisional lease authorizes
    // only exact detached-process reconciliation, not a new launch.
    let _ = has_exact_lifecycle_namespace_proof(&data_dir, &[0u8; 32])?;
    drop(control_namespace_lock);

    let lock_path = control_dir.join(DESKTOP_LIFECYCLE_LOCK_FILE_NAME);
    arc_crypto::secret_file::durably_publish_new_private(
        &lock_path,
        b"arc.desktop.lifecycle-lock.v1\n",
    )?;
    let file = arc_crypto::secret_file::open_private_read_write(&lock_path)?;
    file.try_lock_exclusive().map_err(|error| {
        anyhow::anyhow!(
            "another ARC desktop currently owns the managed node lifecycle for {}: {error}",
            data_dir.display()
        )
    })?;
    Ok(ManagedLifecycleLock {
        data_dir,
        _file: file,
        namespace_guard: Some(data_namespace_lock),
        namespace_prepared: false,
        session_nonce: None,
    })
}

fn retire_legacy_lifecycle_namespace_proofs(control_dir: &Path) -> anyhow::Result<()> {
    // v1 was path-independent. The fixed-name v2 entry was used only by a
    // pre-release candidate, but removing both after current-v3 publication
    // prevents an older build from reopening either downgrade path.
    for file_name in ["lifecycle.namespace-proof", "lifecycle.namespace-proof.v2"] {
        let path = control_dir.join(file_name);
        arc_crypto::secret_file::durably_replace_private(
            &path,
            b"arc.desktop.lifecycle-namespace.retired-by-v3\n",
        )
        .with_context(|| {
            format!(
                "cannot install lifecycle namespace downgrade tombstone {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

/// Convert a provisional/reused lifecycle lease into a fresh launch-capable
/// namespace. The outer sibling guard stays held while the descendant lock is
/// released, both directory names cross their durability barriers, the exact
/// proof is replaced, and the lifecycle lock is reacquired. This is the only
/// transition that sets `namespace_prepared`.
fn refresh_managed_lifecycle_namespace(
    mut lifecycle_lock: ManagedLifecycleLock,
) -> anyhow::Result<ManagedLifecycleLock> {
    let data_dir = lifecycle_lock.data_dir.clone();
    let data_namespace_lock = match lifecycle_lock.namespace_guard.take() {
        Some(guard) => guard,
        None => arc_crypto::secret_file::acquire_private_directory_namespace_lock(&data_dir)
            .with_context(|| {
                format!(
                    "cannot lock managed data-directory namespace {} for refresh",
                    data_dir.display()
                )
            })?,
    };
    data_namespace_lock.restore_interrupted().with_context(|| {
        format!(
            "cannot restore interrupted managed data-directory namespace {}",
            data_dir.display()
        )
    })?;
    let locked_target = data_namespace_lock.target();
    anyhow::ensure!(
        data_dir == locked_target
            || arc_crypto::secret_file::same_private_directory_namespace(&data_dir, locked_target,)?,
        "managed lifecycle lease resolved outside its locked namespace: {}",
        data_dir.display()
    );

    let control_dir = data_dir.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
    let control_namespace_lock =
        arc_crypto::secret_file::acquire_private_directory_namespace_lock(&control_dir)
            .with_context(|| {
                format!(
                    "cannot lock managed shutdown-control namespace {} for refresh",
                    control_dir.display()
                )
            })?;
    control_namespace_lock.restore_interrupted()?;
    arc_crypto::secret_file::secure_private_directory_tree(&control_dir)?;
    // Validate any proof at the one currently expected identity-bound name.
    // Well-formed stale identities are refreshable; malformed bytes fail
    // before the lifecycle lease or any live namespace name is released.
    let _ = has_exact_lifecycle_namespace_proof(&data_dir, &[0u8; 32])?;

    let node_lock_path = data_dir.join(".arc-node.lock");
    match arc_crypto::secret_file::open_private_read_write(&node_lock_path) {
        Ok(node_lock) => {
            node_lock.try_lock_exclusive().map_err(|error| {
                anyhow::anyhow!(
                    "managed node still owns {} after detached-process reconciliation: {error}",
                    node_lock_path.display()
                )
            })?;
            fs2::FileExt::unlock(&node_lock)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    fs2::FileExt::unlock(&lifecycle_lock._file)
        .context("cannot release provisional desktop lifecycle lock before namespace refresh")?;
    drop(lifecycle_lock._file);
    control_namespace_lock
        .rebarrier_existing()
        .with_context(|| {
            format!(
                "cannot rebarrier managed shutdown-control namespace {}",
                control_dir.display()
            )
        })?;
    drop(control_namespace_lock);
    data_namespace_lock.rebarrier_existing().with_context(|| {
        format!(
            "cannot rebarrier managed data-directory namespace {}",
            data_dir.display()
        )
    })?;

    let mut session_nonce = [0u8; 32];
    {
        use rand::RngCore as _;
        rand::rngs::OsRng.fill_bytes(&mut session_nonce);
    }
    let proof_path = arc_crypto::secret_file::desktop_lifecycle_namespace_proof_path(&data_dir)?;
    let proof =
        arc_crypto::secret_file::desktop_lifecycle_namespace_proof(&data_dir, &session_nonce)?;
    arc_crypto::secret_file::durably_replace_private(&proof_path, &proof).with_context(|| {
        format!(
            "cannot durably refresh managed lifecycle namespace proof {}",
            proof_path.display()
        )
    })?;
    anyhow::ensure!(
        has_exact_lifecycle_namespace_proof(&data_dir, &session_nonce)?,
        "managed lifecycle namespace proof disappeared during refresh: {}",
        proof_path.display()
    );
    retire_legacy_lifecycle_namespace_proofs(&control_dir)?;

    let lock_path = control_dir.join(DESKTOP_LIFECYCLE_LOCK_FILE_NAME);
    arc_crypto::secret_file::durably_replace_private(
        &lock_path,
        &arc_crypto::secret_file::desktop_lifecycle_lock_payload(&session_nonce),
    )?;
    let file = arc_crypto::secret_file::open_private_read_write(&lock_path)?;
    file.try_lock_exclusive().map_err(|error| {
        anyhow::anyhow!(
            "managed lifecycle ownership changed during namespace refresh for {}: {error}",
            data_dir.display()
        )
    })?;
    drop(data_namespace_lock);
    Ok(ManagedLifecycleLock {
        data_dir,
        _file: file,
        namespace_guard: None,
        namespace_prepared: true,
        session_nonce: Some(session_nonce),
    })
}

impl DesktopShutdownControl {
    fn token_bytes(&self) -> anyhow::Result<[u8; 32]> {
        let decoded = Zeroizing::new(
            hex::decode(self.token.as_str())
                .context("desktop shutdown control token is not valid hexadecimal")?,
        );
        anyhow::ensure!(
            decoded.len() == 32,
            "desktop shutdown control token has an invalid length"
        );
        let mut token = [0u8; 32];
        token.copy_from_slice(&decoded);
        Ok(token)
    }

    fn arm_receipt(&mut self, executable: &Path, genesis: &Path) -> anyhow::Result<()> {
        let token = self.token_bytes()?;
        let arm = arc_crypto::secret_file::arm_desktop_shutdown_receipt(
            &self.data_dir,
            &token,
            executable,
            genesis,
        )
        .context("failed to durably arm the managed-node shutdown receipt")?;
        self.receipt_executable = Some(executable.canonicalize()?);
        self.receipt_genesis = Some(genesis.canonicalize()?);
        self.receipt_nonce = Some(arm.nonce);
        Ok(())
    }

    fn bind_receipt_identity(&mut self, executable: &Path, genesis: &Path) -> anyhow::Result<()> {
        self.receipt_executable = Some(executable.canonicalize()?);
        self.receipt_genesis = Some(genesis.canonicalize()?);
        self.receipt_nonce = arc_crypto::secret_file::load_desktop_shutdown_receipt_nonce(
            &self.data_dir,
            &self.token_bytes()?,
            executable,
            genesis,
        )
        .context("failed to validate an inherited managed-node shutdown receipt")?;
        Ok(())
    }

    fn validate_armed_receipt(&self) -> anyhow::Result<bool> {
        let executable = self
            .receipt_executable
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no executable binding"))?;
        let genesis = self
            .receipt_genesis
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no genesis binding"))?;
        let nonce = self
            .receipt_nonce
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no request nonce"))?;
        arc_crypto::secret_file::validate_desktop_shutdown_receipt(
            &self.data_dir,
            &self.token_bytes()?,
            nonce,
            executable,
            genesis,
        )
        .context("failed to validate the managed-node shutdown receipt")
    }

    fn validate_clean_ack(&self) -> anyhow::Result<bool> {
        let executable = self
            .receipt_executable
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no executable binding"))?;
        let genesis = self
            .receipt_genesis
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no genesis binding"))?;
        let nonce = self
            .receipt_nonce
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no request nonce"))?;
        arc_crypto::secret_file::validate_desktop_shutdown_ack(
            &self.data_dir,
            &self.token_bytes()?,
            nonce,
            executable,
            genesis,
        )
        .context("failed to validate the managed-node clean shutdown ACK")
    }

    fn consume_clean_ack(&self) -> anyhow::Result<()> {
        let executable = self
            .receipt_executable
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no executable binding"))?;
        let genesis = self
            .receipt_genesis
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no genesis binding"))?;
        let nonce = self
            .receipt_nonce
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no request nonce"))?;
        arc_crypto::secret_file::consume_desktop_shutdown_ack(
            &self.data_dir,
            &self.token_bytes()?,
            nonce,
            executable,
            genesis,
        )
        .context("failed to consume the exact clean shutdown ACK/marker pair")
    }

    fn ensure_receipt_armed(&mut self) -> anyhow::Result<()> {
        let executable = self
            .receipt_executable
            .clone()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no executable binding"))?;
        let genesis = self
            .receipt_genesis
            .clone()
            .ok_or_else(|| anyhow::anyhow!("managed shutdown receipt has no genesis binding"))?;
        self.arm_receipt(&executable, &genesis)
    }
}

#[cfg(windows)]
fn same_desktop_shutdown_control(
    left: &DesktopShutdownControl,
    right: &DesktopShutdownControl,
) -> anyhow::Result<bool> {
    let left_token = left.token_bytes()?;
    let right_token = right.token_bytes()?;
    let token_matches = left_token
        .iter()
        .zip(right_token.iter())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0;
    Ok(token_matches
        && left.data_dir == right.data_dir
        && left.token_file == right.token_file
        && left.receipt_executable == right.receipt_executable
        && left.receipt_genesis == right.receipt_genesis
        && left.receipt_nonce == right.receipt_nonce)
}

#[derive(Clone)]
struct LegacyWindowsStopContext {
    data_dir: PathBuf,
    validator_seed: Zeroizing<String>,
    seeds_file: PathBuf,
    genesis_file: PathBuf,
    allowed_port_pairs: Vec<(u16, u16)>,
    model_path: Option<PathBuf>,
    worker_mode: bool,
}

fn read_private_shutdown_request(path: &Path) -> std::io::Result<Zeroizing<String>> {
    let mut file = arc_crypto::secret_file::open_private(path)?;
    if file.metadata()?.len() > DESKTOP_SHUTDOWN_FILE_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "private shutdown capability exceeds its bounded size",
        ));
    }
    let mut text = String::new();
    std::io::Read::by_ref(&mut file)
        .take(DESKTOP_SHUTDOWN_FILE_MAX_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > DESKTOP_SHUTDOWN_FILE_MAX_BYTES {
        text.zeroize();
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "private shutdown capability exceeds its bounded size",
        ));
    }
    Ok(Zeroizing::new(text))
}

fn read_private_hex_token(path: &Path) -> std::io::Result<Zeroizing<String>> {
    let mut text = read_private_shutdown_request(path)?;
    let token = Zeroizing::new(text.trim().to_string());
    text.zeroize();
    if token.len() != 64 || hex::decode(token.as_str()).map_or(true, |bytes| bytes.len() != 32) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "private shutdown capability must contain exactly 32 hexadecimal bytes",
        ));
    }
    Ok(token)
}

fn read_desktop_shutdown_control(token_file: &Path) -> anyhow::Result<DesktopShutdownControl> {
    let control_dir = token_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("desktop shutdown token has no parent directory"))?;
    arc_crypto::secret_file::validate_private_directory(control_dir).map_err(|error| {
        anyhow::anyhow!(
            "desktop shutdown control directory is not private {}: {error}",
            control_dir.display()
        )
    })?;
    if token_file.file_name() != Some(std::ffi::OsStr::new(DESKTOP_SHUTDOWN_TOKEN_FILE_NAME)) {
        anyhow::bail!("desktop shutdown token has an unexpected filename");
    }
    if control_dir.file_name() != Some(std::ffi::OsStr::new(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)) {
        anyhow::bail!("desktop shutdown token is outside the named control directory");
    }
    let token = read_private_hex_token(token_file).map_err(|error| {
        anyhow::anyhow!(
            "cannot open private desktop shutdown token {}: {error}",
            token_file.display()
        )
    })?;
    let request_file = token_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("desktop shutdown token has no parent directory"))?
        .join(DESKTOP_SHUTDOWN_REQUEST_FILE_NAME);
    let data_dir = control_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("desktop shutdown control has no data-directory parent"))?
        .canonicalize()
        .context("cannot canonicalize desktop shutdown data directory")?;
    Ok(DesktopShutdownControl {
        data_dir,
        token_file: token_file.to_path_buf(),
        request_file,
        token,
        receipt_executable: None,
        receipt_genesis: None,
        receipt_nonce: None,
    })
}

fn prepare_desktop_shutdown_control(data_dir: &Path) -> anyhow::Result<DesktopShutdownControl> {
    let data_dir = data_dir.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "cannot canonicalize desktop node data directory {}: {error}",
            data_dir.display()
        )
    })?;
    let control_dir = data_dir.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
    arc_crypto::secret_file::secure_private_directory_tree(&control_dir).map_err(|error| {
        anyhow::anyhow!(
            "cannot secure desktop shutdown control directory {}: {error}",
            control_dir.display()
        )
    })?;
    let token_file = control_dir.join(DESKTOP_SHUTDOWN_TOKEN_FILE_NAME);
    if !token_file.exists() {
        use rand::RngCore as _;
        let mut token_bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut token_bytes);
        let mut token_hex = Zeroizing::new(format!("{}\n", hex::encode(token_bytes)));
        token_bytes.fill(0);
        let publication =
            arc_crypto::secret_file::durably_publish_new_private(&token_file, token_hex.as_bytes());
        token_hex.as_mut_str().zeroize();
        publication.map_err(|error| {
            anyhow::anyhow!(
                "cannot durably publish private desktop shutdown token {}: {error}",
                token_file.display()
            )
        })?;
    }
    // Never delete an existing request here. A second desktop can reach this
    // pre-spawn path while the first desktop's node owns the data-directory
    // lock and is draining that exact request. Requests carry a target PID;
    // only arc-node, after taking the exclusive lock, may consume a stale one.
    read_desktop_shutdown_control(&token_file)
}

fn managed_executable_identity_file(data_dir: &Path) -> PathBuf {
    data_dir
        .join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)
        .join(DESKTOP_EXECUTABLE_IDENTITY_FILE_NAME)
}

fn persist_managed_executable_identity(
    data_dir: &Path,
    executable: &Path,
) -> anyhow::Result<PathBuf> {
    let executable = executable.canonicalize().with_context(|| {
        format!(
            "cannot canonicalize managed arc-node executable {}",
            executable.display()
        )
    })?;
    let path = executable.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "managed arc-node executable path is not valid UTF-8 and cannot be persisted safely"
        )
    })?;
    let payload = format!(
        "{DESKTOP_EXECUTABLE_IDENTITY_SCHEMA}\npath_utf8_hex={}\n",
        hex::encode(path.as_bytes())
    );
    anyhow::ensure!(
        payload.len() as u64 <= DESKTOP_EXECUTABLE_IDENTITY_MAX_BYTES,
        "managed arc-node executable identity exceeds its bounded size"
    );
    let identity_file = managed_executable_identity_file(data_dir);
    arc_crypto::secret_file::durably_replace_private(&identity_file, payload.as_bytes())
        .with_context(|| {
            format!(
                "cannot durably persist managed arc-node executable identity {}",
                identity_file.display()
            )
        })?;
    Ok(executable)
}

fn load_managed_executable_identity(data_dir: &Path) -> anyhow::Result<PathBuf> {
    let identity_file = managed_executable_identity_file(data_dir);
    let mut file = arc_crypto::secret_file::open_private(&identity_file).with_context(|| {
        format!(
            "managed-node recovery fence exists but exact executable identity {} is unavailable",
            identity_file.display()
        )
    })?;
    anyhow::ensure!(
        file.metadata()?.len() <= DESKTOP_EXECUTABLE_IDENTITY_MAX_BYTES,
        "managed arc-node executable identity exceeds its bounded size"
    );
    let mut payload = String::new();
    std::io::Read::by_ref(&mut file)
        .take(DESKTOP_EXECUTABLE_IDENTITY_MAX_BYTES + 1)
        .read_to_string(&mut payload)?;
    anyhow::ensure!(
        payload.len() as u64 <= DESKTOP_EXECUTABLE_IDENTITY_MAX_BYTES,
        "managed arc-node executable identity exceeds its bounded size"
    );
    let mut lines = payload.lines();
    anyhow::ensure!(
        lines.next() == Some(DESKTOP_EXECUTABLE_IDENTITY_SCHEMA),
        "managed arc-node executable identity has an invalid schema"
    );
    let encoded = lines
        .next()
        .and_then(|line| line.strip_prefix("path_utf8_hex="))
        .ok_or_else(|| anyhow::anyhow!("managed arc-node executable identity omits its path"))?;
    anyhow::ensure!(
        lines.next().is_none(),
        "managed arc-node executable identity contains unexpected fields"
    );
    let decoded = hex::decode(encoded)
        .map_err(|_| anyhow::anyhow!("managed arc-node executable identity is not hexadecimal"))?;
    let path = std::str::from_utf8(&decoded)
        .map_err(|_| anyhow::anyhow!("managed arc-node executable identity is not UTF-8"))?;
    let path = PathBuf::from(path);
    anyhow::ensure!(
        path.is_absolute(),
        "managed arc-node executable identity is not absolute"
    );
    path.canonicalize().with_context(|| {
        format!(
            "cannot recover exact receipt-bound arc-node executable {}",
            path.display()
        )
    })
}

#[cfg_attr(not(windows), allow(dead_code))]
fn write_desktop_shutdown_request(
    control: &DesktopShutdownControl,
    target_pid: u32,
) -> anyhow::Result<()> {
    let nonce = control
        .receipt_nonce
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("desktop shutdown receipt was not armed"))?;
    let request = Zeroizing::new(format!(
        "{DESKTOP_SHUTDOWN_REQUEST_SCHEMA}\npid={target_pid}\ntoken={}\nnonce={}\n",
        control.token.as_str(),
        hex::encode(nonce),
    ));
    for attempt in 0..DESKTOP_SHUTDOWN_PUBLICATION_RETRIES {
        let temporary_file = control.request_file.with_file_name(format!(
            ".request.{}.{}.tmp",
            std::process::id(),
            uuid_like()
        ));
        let mut file =
            arc_crypto::secret_file::create_new_private(&temporary_file).map_err(|error| {
                anyhow::anyhow!(
                    "cannot create private desktop shutdown request staging file {}: {error}",
                    temporary_file.display()
                )
            })?;
        if let Err(error) = file
            .write_all(request.as_bytes())
            .and_then(|_| file.sync_all())
        {
            drop(file);
            let _ = std::fs::remove_file(&temporary_file);
            return Err(anyhow::anyhow!(
                "cannot persist private desktop shutdown request {}: {error}",
                temporary_file.display()
            ));
        }
        drop(file);

        // A hard-link publication is a same-filesystem, atomic no-replace
        // operation on Unix and Windows. The watcher can therefore observe
        // either no request or the complete fsynced payload, never a torn
        // final file. Concurrent desktop instances converge on one final
        // request without deleting each other's request.
        match std::fs::hard_link(&temporary_file, &control.request_file) {
            Ok(()) => {
                arc_crypto::secret_file::sync_parent_directory(&control.request_file)?;
                let _ = std::fs::remove_file(&temporary_file);
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&temporary_file);
                match read_private_shutdown_request(&control.request_file) {
                    Ok(existing) if existing.as_str() == request.as_str() => return Ok(()),
                    Ok(existing) => {
                        let existing_token = existing
                            .lines()
                            .nth(2)
                            .and_then(|line| line.strip_prefix("token="));
                        if existing_token != Some(control.token.as_str()) {
                            anyhow::bail!(
                                "existing private desktop shutdown request is not authenticated by this control capability"
                            );
                        }
                        // A valid request for a prior PID may still be in the
                        // new node's early watcher or in the old node's drain.
                        // Wait for node-side consumption; never replace it or
                        // downgrade this race to force-kill.
                    }
                    Err(error)
                        if !matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::InvalidData
                        ) =>
                    {
                        return Err(anyhow::anyhow!(
                            "cannot validate existing private desktop shutdown request {}: {error}",
                            control.request_file.display()
                        ));
                    }
                    Err(_) => {
                        // Node-side consumption removes a bounded malformed or
                        // stale private request after reading a stable handle.
                    }
                }
            }
            Err(error) => {
                let _ = std::fs::remove_file(&temporary_file);
                return Err(anyhow::anyhow!(
                    "cannot atomically publish private desktop shutdown request {}: {error}",
                    control.request_file.display()
                ));
            }
        }
        if attempt + 1 < DESKTOP_SHUTDOWN_PUBLICATION_RETRIES {
            std::thread::sleep(DESKTOP_SHUTDOWN_PUBLICATION_RETRY_DELAY);
        }
    }
    anyhow::bail!("private desktop shutdown request publication did not settle")
}

#[derive(Clone, Debug, Default)]
struct StopSummary {
    stopped: usize,
    forced: usize,
}

struct StopLifecycleOutcome {
    lifecycle_lock: Option<ManagedLifecycleLock>,
    stopped: usize,
}

fn finish_legacy_reconciliation(
    slot: &mut Option<LegacyWindowsStopContext>,
    attempted: Option<LegacyWindowsStopContext>,
    result: anyhow::Result<StopSummary>,
) -> anyhow::Result<StopSummary> {
    match result {
        Ok(summary) => Ok(summary),
        Err(error) => {
            *slot = attempted;
            Err(error)
        }
    }
}

#[derive(Clone, Debug)]
struct ChildStopOutcome {
    forced_reason: Option<String>,
}

#[derive(Debug)]
struct ManagedDurabilityRecoveryRequired {
    reason: String,
}

impl std::fmt::Display for ManagedDurabilityRecoveryRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "managed-node durability recovery is unresolved; refusing Stop/update boundary: {}. Start the exact receipt-bound node and complete one authenticated clean shutdown before updating",
            self.reason
        )
    }
}

impl std::error::Error for ManagedDurabilityRecoveryRequired {}

pub fn is_managed_durability_recovery_required(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<ManagedDurabilityRecoveryRequired>())
}

pub struct NodeManager {
    child: Option<Child>,
    started_at: Option<Instant>,
    pub rpc_port: u16,
    pub logs: Arc<Mutex<VecDeque<LogEntry>>>,
    /// Set by the reaper when the child exits unexpectedly (we didn't call stop()).
    /// Surfaced through node_status.last_error so the UI can show a crash banner.
    pub crash_info: Arc<Mutex<Option<CrashInfo>>>,
    /// Intentional-stop flag - `stop()` sets this before killing, so the reaper
    /// doesn't misreport a clean shutdown as a crash.
    stopping: Arc<std::sync::atomic::AtomicBool>,
    /// Private file-capability used by packaged Windows nodes, which have no
    /// console signal channel under CREATE_NO_WINDOW. The token is persisted
    /// mode-0600 beside chain data so a restarted desktop can also drain a
    /// detached managed node without exposing a remote shutdown endpoint.
    shutdown_control: Option<DesktopShutdownControl>,
    lifecycle_lock: Option<ManagedLifecycleLock>,
    /// Cross-process fence retained after a successful updater preflight and
    /// through signed bundle installation/relaunch. Unlike `lifecycle_lock`,
    /// this guard intentionally exists while no child is running: it prevents
    /// a second GUI from starting a new writer after Stop has returned to the
    /// WebView but before the updater has replaced and relaunched the app.
    update_lifecycle_lock: Option<ManagedLifecycleLock>,
    /// Exact launch identity captured only after a managed child successfully
    /// spawns. A pre-handoff updater abort may consume this plan to restore
    /// the node that was running before Prepare, without consulting mutable
    /// WebView settings or re-resolving a different executable.
    active_launch_plan: Option<ManagedLaunchPlan>,
    /// Present only while Prepare has stopped a previously running owned node.
    /// It is consumed by the atomic abort-and-resume transaction, or retained
    /// with the update lock when a safe resume cannot be proven.
    update_restart_plan: Option<ManagedLaunchPlan>,
    /// One-way native boundary set immediately before the updater installer is
    /// invoked. Once set, abort is forbidden even if install() rejects.
    update_handoff_started: bool,
    managed_data_dir: Option<PathBuf>,
    durability_failure: Option<String>,
    legacy_windows_stop_context: Option<LegacyWindowsStopContext>,
    legacy_windows_stop_error: Option<String>,
    /// Core count the *running* child was actually launched with. Read back
    /// by `node_status` so the Dashboard reports the node's real compute
    /// width rather than whatever the config currently says — those diverge
    /// the moment the user moves the slider without applying it.
    pub active_worker_threads: Option<u32>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashInfo {
    pub exit_code: Option<i32>,
    pub message: String,
    pub at_millis: i64,
}

/// Paths to the testnet bootstrap config bundled with the app.
/// Callers resolve these from the Tauri resource dir via AppHandle, then
/// hand them to `start()` so we don't carry Tauri types deep in NodeManager.
#[derive(Clone, Debug, Default)]
pub struct TestnetResources {
    pub seeds_file: Option<PathBuf>,
    pub genesis_file: Option<PathBuf>,
}

fn read_bounded_bundle_resource(path: &Path, name: &str) -> anyhow::Result<Vec<u8>> {
    require_regular_bundle_resource(path, name)?;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("cannot open signed bundle resource {name}"))?;
    let length = file.metadata()?.len();
    anyhow::ensure!(
        length > 0 && length <= DESKTOP_NETWORK_RESOURCE_MAX_BYTES,
        "signed bundle resource {name} has invalid bounded size {length}"
    );
    let mut bytes = Vec::with_capacity(length as usize);
    std::io::Read::by_ref(&mut file)
        .take(DESKTOP_NETWORK_RESOURCE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= DESKTOP_NETWORK_RESOURCE_MAX_BYTES,
        "signed bundle resource {name} exceeded its bounded size while reading"
    );
    Ok(bytes)
}

fn stable_network_replacement_staging(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(
        path.file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("network-resource")),
    );
    name.push(".arc-stable-network.replace");
    path.with_file_name(name)
}

/// Replace one stable network resource through a deterministic, destination-
/// bound staging file. The lifecycle and network namespace guards serialize
/// this helper, so a crash can leave at most one bounded staging file per
/// resource; a retry truncates and reuses it without touching unrelated files.
fn durably_replace_stable_network_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if contents.is_empty() || contents.len() as u64 > DESKTOP_NETWORK_RESOURCE_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "stable network resource has an invalid bounded size",
        ));
    }
    let staging = stable_network_replacement_staging(path);
    let mut file = match arc_crypto::secret_file::open_private_read_write(&staging) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            arc_crypto::secret_file::create_new_private(&staging)?
        }
        Err(error) => return Err(error),
    };
    file.set_len(0)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    #[cfg(windows)]
    arc_crypto::secret_file::windows_move_path_write_through(&staging, path, true)?;
    #[cfg(unix)]
    {
        std::fs::rename(&staging, path)?;
        arc_crypto::secret_file::sync_parent_directory(path)?;
    }
    #[cfg(not(any(unix, windows)))]
    std::fs::rename(&staging, path)?;

    drop(arc_crypto::secret_file::open_private(path)?);
    Ok(())
}

/// Copy the signed bundle network identity into stable private app data before
/// the lifecycle receipt is armed. AppImage mount paths change every launch
/// and package/manual updates can replace bundle resources; a crash-recovery
/// receipt must therefore bind files whose path and bytes survive GUI updates.
fn materialize_stable_testnet_resources(
    data_dir: &Path,
    bundle: &TestnetResources,
    lifecycle_lock: &ManagedLifecycleLock,
) -> anyhow::Result<TestnetResources> {
    let data_dir = data_dir.canonicalize().with_context(|| {
        format!(
            "cannot canonicalize managed data directory before network identity materialization: {}",
            data_dir.display()
        )
    })?;
    lifecycle_lock.ensure_data_dir(&data_dir)?;
    let lifecycle = arc_crypto::secret_file::desktop_shutdown_lifecycle_state(&data_dir)
        .context("cannot inspect managed-node lifecycle before network identity materialization")?;
    let network_dir = data_dir.join(DESKTOP_NETWORK_IDENTITY_DIR_NAME);
    let network_namespace_lock =
        arc_crypto::secret_file::acquire_private_directory_namespace_lock(&network_dir)
            .with_context(|| {
                format!(
                    "cannot lock stable network identity namespace {}",
                    network_dir.display()
                )
            })?;
    network_namespace_lock
        .restore_interrupted()
        .with_context(|| {
            format!(
                "cannot restore interrupted stable network identity namespace {}",
                network_dir.display()
            )
        })?;
    let network_dir = network_namespace_lock.target().to_path_buf();
    let stable_seeds = network_dir.join(DESKTOP_STABLE_SEEDS_FILE_NAME);
    let stable_genesis = network_dir.join(DESKTOP_STABLE_GENESIS_FILE_NAME);

    if lifecycle.is_clear() {
        let (bundle_seeds, bundle_genesis) = required_testnet_resources(bundle)?;
        let seeds = read_bounded_bundle_resource(bundle_seeds, DESKTOP_STABLE_SEEDS_FILE_NAME)?;
        let genesis =
            read_bounded_bundle_resource(bundle_genesis, DESKTOP_STABLE_GENESIS_FILE_NAME)?;
        arc_crypto::secret_file::secure_private_directory_tree(&network_dir).with_context(
            || {
                format!(
                    "cannot durably create stable network identity directory {}",
                    network_dir.display()
                )
            },
        )?;
        durably_replace_stable_network_file(&stable_seeds, &seeds)
            .context("cannot durably materialize stable signed seed identity")?;
        durably_replace_stable_network_file(&stable_genesis, &genesis)
            .context("cannot durably materialize stable signed genesis identity")?;
        network_namespace_lock
            .rebarrier_existing()
            .with_context(|| {
                format!(
                    "cannot rebarrier stable network identity namespace {}",
                    network_dir.display()
                )
            })?;
    } else {
        // Never derive recovery identity from a new AppImage mount or replaced
        // application bundle. The old stable copy is immutable until the node
        // publishes a clean ACK and the supervisor consumes the marker.
        arc_crypto::secret_file::validate_private_directory(&network_dir).with_context(|| {
            format!(
                "managed-node recovery fence exists but stable network directory {} is unavailable",
                network_dir.display()
            )
        })?;
        for (path, name) in [
            (&stable_seeds, DESKTOP_STABLE_SEEDS_FILE_NAME),
            (&stable_genesis, DESKTOP_STABLE_GENESIS_FILE_NAME),
        ] {
            let file = arc_crypto::secret_file::open_private(path).with_context(|| {
                format!("managed-node recovery fence exists but stable {name} is unavailable")
            })?;
            let length = file.metadata()?.len();
            anyhow::ensure!(
                length > 0 && length <= DESKTOP_NETWORK_RESOURCE_MAX_BYTES,
                "stable recovery resource {name} has invalid bounded size {length}"
            );
        }
    }

    Ok(TestnetResources {
        seeds_file: Some(stable_seeds.canonicalize()?),
        genesis_file: Some(stable_genesis.canonicalize()?),
    })
}

fn build_legacy_windows_stop_context(
    config: &NodeConfig,
    data_dir: PathBuf,
    validator_seed: Zeroizing<String>,
    resources: &TestnetResources,
) -> anyhow::Result<LegacyWindowsStopContext> {
    let (seeds_file, genesis_file) = required_testnet_resources(resources)?;
    anyhow::ensure!(
        !validator_seed.is_empty() && validator_seed.len() <= LEGACY_VALIDATOR_SEED_MAX_BYTES,
        "persisted legacy validator identity has an invalid bounded length"
    );
    anyhow::ensure!(
        std::fs::symlink_metadata(&data_dir).is_ok_and(
            |metadata| metadata.file_type().is_dir() && !metadata.file_type().is_symlink()
        ),
        "preserved legacy node data directory is not a regular non-link directory"
    );
    Ok(LegacyWindowsStopContext {
        data_dir,
        validator_seed,
        seeds_file: seeds_file.canonicalize()?,
        genesis_file: genesis_file.canonicalize()?,
        allowed_port_pairs: (0..5)
            .map(|offset| {
                (
                    config.rpc_port.saturating_add(offset * 10),
                    config.p2p_port.saturating_add(offset * 10),
                )
            })
            .collect(),
        // v0.7 resolved relative model arguments in the child process cwd.
        // Preserve the raw config form and resolve it against that exact cwd
        // during process matching.
        model_path: config.model_path.as_deref().map(PathBuf::from),
        worker_mode: config.role == "worker" && config.model_path.is_some(),
    })
}

impl NodeManager {
    pub fn new() -> Self {
        Self {
            child: None,
            started_at: None,
            // Defaults to 9090 (community installer convention). Real value
            // is set from NodeConfig.rpc_port on start().
            rpc_port: 9090,
            logs: Arc::new(Mutex::new(VecDeque::with_capacity(LOG_RING_SIZE))),
            crash_info: Arc::new(Mutex::new(None)),
            stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown_control: None,
            lifecycle_lock: None,
            update_lifecycle_lock: None,
            active_launch_plan: None,
            update_restart_plan: None,
            update_handoff_started: false,
            managed_data_dir: None,
            durability_failure: None,
            legacy_windows_stop_context: None,
            legacy_windows_stop_error: None,
            active_worker_threads: None,
        }
    }

    pub async fn clear_crash(&self) {
        *self.crash_info.lock().await = None;
    }

    pub fn configure_legacy_windows_stop_context(
        &mut self,
        config: &NodeConfig,
        migration_notice: &DataMigrationNotice,
        validator_seed: Zeroizing<String>,
        resources: &TestnetResources,
    ) -> anyhow::Result<()> {
        let result = (|| {
            let active_data_dir = resolve_data_dir(&config.data_dir)
                .canonicalize()
                .with_context(|| {
                    format!(
                        "cannot canonicalize configured active node data directory {}",
                        resolve_data_dir(&config.data_dir).display()
                    )
                })?;
            let notice_active_data_dir = resolve_data_dir(&migration_notice.active_data_dir)
                .canonicalize()
                .context("cannot canonicalize migration notice active data directory")?;
            anyhow::ensure!(
                active_data_dir == notice_active_data_dir,
                "migration notice active directory does not match the persisted node config"
            );
            let data_dir = resolve_data_dir(&migration_notice.legacy_data_dir)
                .canonicalize()
                .context("cannot canonicalize preserved legacy node data directory")?;
            build_legacy_windows_stop_context(config, data_dir, validator_seed, resources)
        })();
        match result {
            Ok(context) => {
                self.legacy_windows_stop_context = Some(context);
                self.legacy_windows_stop_error = None;
                Ok(())
            }
            Err(error) => {
                self.legacy_windows_stop_context = None;
                self.legacy_windows_stop_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    pub fn block_legacy_windows_reconciliation(&mut self, reason: impl Into<String>) {
        self.legacy_windows_stop_context = None;
        self.legacy_windows_stop_error = Some(reason.into());
    }

    pub fn configure_managed_data_dir(&mut self, configured: &str) -> anyhow::Result<()> {
        let data_dir = resolve_data_dir(configured);
        if !data_dir.exists() {
            self.managed_data_dir = Some(data_dir);
            return Ok(());
        }
        let data_dir = data_dir.canonicalize().with_context(|| {
            format!(
                "cannot canonicalize configured managed node data directory {}",
                data_dir.display()
            )
        })?;
        self.managed_data_dir = Some(data_dir.clone());
        match arc_crypto::secret_file::desktop_shutdown_lifecycle_state(&data_dir) {
            Ok(state) if !state.is_clear() => {
                self.durability_failure = Some(
                    "a prior managed node shutdown did not durably acknowledge its final WAL barrier"
                        .into(),
                );
            }
            Ok(_) => self.durability_failure = None,
            Err(error) => {
                let reason = format!(
                    "the private managed-node shutdown receipt cannot be validated: {error}"
                );
                self.durability_failure = Some(reason.clone());
                anyhow::bail!(reason);
            }
        }
        Ok(())
    }

    fn refresh_durability_failure(&mut self) -> anyhow::Result<()> {
        let Some(data_dir) = self.managed_data_dir.as_deref() else {
            return Ok(());
        };
        match arc_crypto::secret_file::desktop_shutdown_lifecycle_state(data_dir) {
            Ok(state) if !state.is_clear() => {
                self.durability_failure = Some(
                    "the managed node has not acknowledged a successful final WAL durability barrier"
                        .into(),
                );
                Ok(())
            }
            Ok(_) => {
                self.durability_failure = None;
                Ok(())
            }
            Err(error) => {
                let reason = format!(
                    "the private managed-node shutdown receipt cannot be validated: {error}"
                );
                self.durability_failure = Some(reason.clone());
                anyhow::bail!(reason)
            }
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }

    pub fn is_running(&mut self) -> bool {
        // tokio::process::Child::try_wait doesn't exist on async Child;
        // checking id() is enough - if we haven't called wait() it's still alive.
        self.child.is_some()
    }

    /// Spawn arc-node with the CLI flags the chain actually accepts
    /// (verified against crates/arc-node/src/main.rs). Pass:
    ///   --rpc <ip>:<port>            bind the HTTP RPC server
    ///   --p2p-port <port>            QUIC P2P listener
    ///   --data-dir <dir>             where WAL + state live
    ///   --validator-key-file <path>  persistent app-owned Ed25519 identity
    ///   --seeds-file <path>          testnet peer bootstrap list
    ///   --genesis <path>             testnet genesis.toml
    ///   --eth-rpc-port 0             disable the extra EVM RPC port
    ///   --community-mode             (worker role only) register with seed
    ///                                gateways as a volunteer inference worker
    ///   --full-integer-worker        load full deterministic integer weights
    ///                                without announcing a validator shard
    ///   --model <path>               (optional) GGUF weights for local inference
    ///
    /// Any of these missing would leave the node either bound to wrong
    /// ports, isolated from the testnet, identity-mismatched, or silent
    /// as a worker. All are required for the "download → run → earn"
    /// operator flow.
    pub async fn start(
        &mut self,
        config: &NodeConfig,
        validator_keyfile: &Path,
        resources: &TestnetResources,
        lifecycle_lock: ManagedLifecycleLock,
    ) -> anyhow::Result<()> {
        self.start_while_lifecycle_locked(
            config,
            validator_keyfile,
            resources,
            &lifecycle_lock,
            None,
        )
        .await?;
        self.lifecycle_lock = Some(lifecycle_lock);
        Ok(())
    }

    /// Spawn while borrowing the caller's already-held lifecycle lock. The
    /// updater abort path uses this form so a failed resume can put the exact
    /// same guard back into `update_lifecycle_lock` without ever opening a
    /// cross-GUI writer race.
    async fn start_while_lifecycle_locked(
        &mut self,
        config: &NodeConfig,
        validator_keyfile: &Path,
        resources: &TestnetResources,
        lifecycle_lock: &ManagedLifecycleLock,
        resume_plan: Option<&ManagedLaunchPlan>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.update_lifecycle_lock.is_none(),
            "a signed desktop update is installing; refusing to start arc-node until this GUI relaunches"
        );
        if self.is_running() {
            anyhow::bail!("managed arc-node is already running");
        }

        // Network identity is application-owned, never WebView/config-owned.
        // Both files must resolve from the signed Tauri resource bundle and
        // must remain regular files. Falling back to arc-node defaults (or to
        // a same-named file in the process working directory) can silently
        // place a user's preserved history on a different network.
        let data_dir = resolve_data_dir(&config.data_dir);
        arc_crypto::secret_file::secure_private_directory_tree(&data_dir).map_err(|error| {
            anyhow::anyhow!(
                "cannot durably secure desktop node data directory {}: {error}",
                data_dir.display()
            )
        })?;
        // Use one canonical path for both argv and the private shutdown
        // control. In particular, Windows canonicalize may add a verbatim
        // `\\?\` prefix; mixing normal data argv with a verbatim token argv
        // makes later detached ownership recovery ambiguous.
        let data_dir = data_dir.canonicalize().with_context(|| {
            format!(
                "cannot canonicalize desktop node data directory {}",
                data_dir.display()
            )
        })?;
        lifecycle_lock.ensure_data_dir(&data_dir)?;
        self.managed_data_dir = Some(data_dir.clone());
        self.refresh_durability_failure()?;
        let recovery_launch = !arc_crypto::secret_file::desktop_shutdown_lifecycle_state(&data_dir)
            .context("cannot inspect managed-node lifecycle before executable selection")?
            .is_clear();
        let verified_resume = resume_plan
            .map(|plan| plan.verify(lifecycle_lock))
            .transpose()?;
        let binary = if let Some((_, binary, _)) = verified_resume.as_ref() {
            anyhow::ensure!(
                !recovery_launch,
                "cannot resume the pre-update node while its durable shutdown receipt is unresolved"
            );
            persist_managed_executable_identity(&data_dir, binary)?
        } else if recovery_launch {
            // The new GUI environment may not retain the ARC_NODE_BIN/PATH/dev
            // resolution that launched the fenced process. Recover from the
            // private durable exact path selected before that process spawned;
            // the receipt below still authenticates both path and bytes.
            load_managed_executable_identity(&data_dir)?
        } else {
            persist_managed_executable_identity(&data_dir, &resolve_binary()?)?
        };
        if !binary_supports_flag(&binary, "--validator-key-file") {
            anyhow::bail!(
                "arc-node at {} cannot load the persistent desktop identity; update the managed node before restarting so the wallet/node address is preserved",
                binary.display()
            );
        }
        let stable_resources = if let Some((_, _, exact_resources)) = verified_resume.as_ref() {
            exact_resources.clone()
        } else {
            materialize_stable_testnet_resources(&data_dir, resources, lifecycle_lock)?
        };
        let (seeds_file, genesis_file) = required_testnet_resources(&stable_resources)?;
        let validator_keyfile = if let Some((exact_validator_keyfile, _, _)) = verified_resume {
            anyhow::ensure!(
                exact_validator_keyfile == validator_keyfile.canonicalize()?,
                "pre-update validator identity path does not match the running node"
            );
            exact_validator_keyfile
        } else {
            validator_keyfile.to_path_buf()
        };
        let supports_desktop_shutdown =
            binary_supports_flag(&binary, "--desktop-shutdown-token-file");
        if !supports_desktop_shutdown {
            anyhow::bail!(
                "arc-node at {} lacks the authenticated durable desktop shutdown protocol; update the managed node before starting so Stop, Update, and rollback can prove its final WAL barrier",
                binary.display()
            );
        }
        if !binary_supports_flag(&binary, "--desktop-lifecycle-nonce") {
            anyhow::bail!(
                "arc-node at {} lacks the session-bound desktop namespace handoff; update the managed node before starting so Windows cannot bypass the data-directory durability barrier",
                binary.display()
            );
        }
        let mut shutdown_control = if supports_desktop_shutdown {
            let mut control = prepare_desktop_shutdown_control(&data_dir)?;
            // A stale receipt is a recovery fence, not a file to overwrite.
            // Only the same executable/genesis/data/token identity may start
            // against it, and the node will clear it only after full replay
            // plus a later authenticated clean shutdown.
            control.bind_receipt_identity(&binary, genesis_file)?;
            if control.receipt_nonce.is_some() {
                self.durability_failure = Some(
                    "a prior managed node shutdown did not durably acknowledge its final WAL barrier"
                        .into(),
                );
            }
            Some(control)
        } else {
            None
        };

        // Probe for an available port pair. First preference is the configured
        // port; fall back in 10-port increments up to 5 tries. This catches the
        // common case of an old node process still bound or Jupyter stealing 9090.
        let (rpc_port, p2p_port) = choose_port_pair(config.rpc_port, config.p2p_port)?;
        if rpc_port != config.rpc_port {
            push_log(
                &self.logs,
                "warn",
                format!(
                    "port {} busy - using {} instead (p2p {} → {})",
                    config.rpc_port, rpc_port, config.p2p_port, p2p_port
                ),
            )
            .await;
        }

        let mut cmd = Command::new(&binary);
        cmd.arg("--rpc")
            .arg(format!("127.0.0.1:{}", rpc_port))
            .arg("--p2p-port")
            .arg(p2p_port.to_string())
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--eth-rpc-port")
            .arg("0")
            // ── LIVE-NETWORK SAFETY: join as an observer, never a validator ──
            //
            // Current arc-node builds default `--stake` to 0, but every
            // released binary through v0.7.11 defaults to 5,000,000 ARC — and
            // this manager may spawn any of them. Passing the flag explicitly
            // keeps the desktop safe regardless of binary vintage: without it,
            // an old binary announces itself to the public seeds as a 5M-stake
            // validator and tries to shard-join the testnet — welding a
            // phantom validator into validator sets
            // that are currently frozen, on a network where four of six
            // seeds have not produced a block in ~6 days. Recovering from
            // that means hand-editing state on six VPSes.
            //
            // `--stake 0` is the observer path: full consensus participation
            // and DAG validation, zero claim on the validator set. It exists
            // in every arc-node version the desktop can encounter, which is
            // why it is passed unconditionally rather than probed for.
            //
            // TODO(chain-core): arc-node v0.7.11 gained `--community`, which
            // is exactly `--stake 0 --community-mode` plus GGUF
            // auto-discovery. Prefer it once the minimum supported node
            // version is >= 0.7.11 — passing an unknown flag to an older
            // binary makes clap abort before the node ever starts, so it
            // cannot simply be swapped in while 0.7.9 nodes are still in the
            // field. Gate it on `binary_supports_flag(&binary, "--community")`.
            .arg("--stake")
            .arg("0")
            // The wallet phrase never enters argv, the child environment, or
            // logs. identity.rs derives the exact historical Ed25519 key and
            // atomically materializes this private, app-owned JSON keyfile.
            // Keeping it persistent preserves the node/reward address across
            // desktop and machine restarts.
            .arg("--validator-key-file")
            .arg(&validator_keyfile)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(control) = shutdown_control.as_ref() {
            cmd.arg("--desktop-shutdown-token-file")
                .arg(&control.token_file)
                .arg("--desktop-lifecycle-nonce")
                .arg(hex::encode(lifecycle_lock.session_nonce()?));
        }

        // ── Compute contribution ────────────────────────────────────────
        // rayon sizes its global pool from RAYON_NUM_THREADS the first time
        // that pool is built, and arc-node only ever calls ThreadPoolBuilder
        // explicitly under `--benchmark`. Setting the env var is therefore
        // the one control that works on every shipped node version, with no
        // chain-side change required.
        //
        // `--threads` is the explicit flag the chain-core agent is adding.
        // It is probed rather than assumed for the same reason as
        // `--community` above: an unknown flag is a hard clap failure, and a
        // node that will not start is a worse outcome than a node running at
        // its default width.
        if let Some(n) = config.worker_threads.filter(|n| *n > 0) {
            cmd.env("RAYON_NUM_THREADS", n.to_string());
            if binary_supports_flag(&binary, "--threads") {
                cmd.arg("--threads").arg(n.to_string());
            } else {
                push_log(
                    &self.logs,
                    "info",
                    format!(
                        "limiting node to {} cores via RAYON_NUM_THREADS (this arc-node has no --threads flag)",
                        n
                    ),
                )
                .await;
            }
        }

        // Windows: detach from the GUI parent's console.
        //
        // arc-node is a console executable. When a Tauri GUI app spawns
        // it without these flags, Windows allocates a fresh console
        // window for the child and ties it to the parent's console
        // group. Two failure modes follow:
        //
        //   1. The black console pops up alongside the desktop window.
        //      Users (reasonably) close it. Closing a console sends
        //      CTRL_CLOSE_EVENT to every process attached to it; the
        //      C runtime's default handler exits with NTSTATUS
        //      0xC000013A (STATUS_CONTROL_C_EXIT, decimal -1073741510).
        //      That is exactly the exit code we see in field crash
        //      reports.
        //   2. Any Ctrl+C delivered to the parent console is also
        //      delivered to the child via the shared process group,
        //      same -1073741510 exit.
        //
        // CREATE_NO_WINDOW (0x08000000) tells Windows not to allocate
        // a console for the child. CREATE_NEW_PROCESS_GROUP (0x00000200)
        // additionally isolates it from any console signals the parent
        // does receive. Together they make the child immune to the
        // console-event class of crashes. Stdio::piped() above still
        // gives us the child's stdout/stderr, so log capture is
        // unaffected.
        #[cfg(windows)]
        {
            // `tokio::process::Command::creation_flags` is a Windows-only
            // inherent method (it forwards to CreateProcessW). No std
            // trait import needed.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        }

        // Required signed-bundle network bootstrap files. Their selection is
        // deliberately absent from NodeConfig, so stale or malicious WebView
        // IPC cannot redirect either one.
        cmd.arg("--seeds-file")
            .arg(seeds_file)
            .arg("--genesis")
            .arg(genesis_file);

        // Only register as a community inference worker if we actually
        // have a model to serve. role="worker" without model_path is
        // nonsense - the gateway would forward requests the node can't
        // answer. Default role is "observer" which just joins consensus,
        // validates blocks, and helps the network without requiring a
        // 4 GB model download.
        if config.role == "worker" && config.model_path.is_some() {
            if !binary_supports_flag(&binary, "--community-rpc-url") {
                anyhow::bail!(
                    "this arc-node predates secure community RPC origins; update arc-node before starting community mode"
                );
            }
            if !binary_supports_flag(&binary, "--full-integer-worker") {
                anyhow::bail!(
                    "this arc-node cannot run a deterministic community worker without announcing an overlapping shard; update arc-node before starting worker mode"
                );
            }
            cmd.arg("--community-mode").arg("--full-integer-worker");
            for origin in crate::rpc_client::PRODUCTION_RPC_ORIGINS {
                cmd.arg("--community-rpc-url").arg(origin);
            }
        }

        if let Some(model) = &config.model_path {
            cmd.arg("--model").arg(model);
        }

        // Capture every launch identity before arming the shutdown receipt or
        // spawning. Nothing after a successful spawn may fail before the
        // Child handle and its exact restart plan are installed in `self`.
        let launch_plan =
            ManagedLaunchPlan::capture(config, &validator_keyfile, &binary, &stable_resources)?;

        push_log(
            &self.logs,
            "info",
            format!(
                "spawning {} --rpc 127.0.0.1:{} --p2p-port {} --stake 0 --validator-key-file <app-owned-private-keyfile> {}{}{}",
                binary.display(),
                rpc_port,
                p2p_port,
                if config.role == "worker" && config.model_path.is_some() {
                    "--community-mode --full-integer-worker "
                } else {
                    ""
                },
                config
                    .worker_threads
                    .filter(|n| *n > 0)
                    .map(|n| format!("({} cores) ", n))
                    .unwrap_or_default(),
                config
                    .model_path
                    .as_deref()
                    .map(|p| format!("--model {}", p))
                    .unwrap_or_else(|| "(observer, no --model)".into()),
            ),
        )
        .await;

        if let Some(control) = shutdown_control.as_mut() {
            // All fallible identity/port/flag/resource preflight is complete.
            // Arm immediately before spawn so any later desktop/node/OS crash
            // leaves a durable recovery fence for the full process lifetime.
            control.ensure_receipt_armed()?;
            self.durability_failure = Some(
                "the running managed node has not yet acknowledged its final WAL durability barrier"
                    .into(),
            );
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                // Leave the published receipt fail-closed even when spawn
                // fails. Another desktop may have accepted the identical
                // marker and successfully spawned between our arm and this
                // error; cancelling it would leave that live writer unfenced.
                // An exact recovery start can reuse the marker and clear it
                // only through a later full replay + clean WAL ACK.
                self.refresh_durability_failure()?;
                return Err(anyhow::anyhow!(
                    "Failed to start arc-node at {}: {}. If this is your first launch, the binary should have been auto-downloaded. Check ~/.arc/bin/arc-node exists and is executable.",
                    binary.display(),
                    error
                ));
            }
        };

        self.rpc_port = rpc_port;
        self.started_at = Some(Instant::now());
        self.shutdown_control = shutdown_control;
        self.active_worker_threads = config.worker_threads.filter(|n| *n > 0);
        self.active_launch_plan = Some(launch_plan);

        // Drain stdout/stderr into the log ring.
        if let Some(stdout) = child.stdout.take() {
            let logs = self.logs.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    push_log(&logs, "info", line).await;
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let logs = self.logs.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let level = classify_level(&line);
                    push_log(&logs, level, line).await;
                }
            });
        }

        self.stopping
            .store(false, std::sync::atomic::Ordering::SeqCst);
        *self.crash_info.lock().await = None;
        self.child = Some(child);

        push_log(
            &self.logs,
            "info",
            format!("arc-node started on 127.0.0.1:{}", rpc_port),
        )
        .await;
        Ok(())
    }

    /// Called on every node_status poll. If our child has exited and we didn't
    /// ask it to, record a CrashInfo. Non-blocking; safe to call frequently.
    pub async fn try_reap_if_crashed(&mut self) {
        if self.child.is_none() {
            return;
        }
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                let was_stopping = self
                    .stopping
                    .swap(false, std::sync::atomic::Ordering::SeqCst);
                self.child = None;
                self.started_at = None;
                self.shutdown_control = None;
                self.lifecycle_lock = None;
                self.active_worker_threads = None;
                self.active_launch_plan = None;
                if !was_stopping {
                    let code = status.code();
                    let message = format!(
                        "arc-node exited unexpectedly{}",
                        code.map(|c| format!(" (code {})", c)).unwrap_or_default()
                    );
                    push_log(&self.logs, "error", message.clone()).await;
                    *self.crash_info.lock().await = Some(CrashInfo {
                        exit_code: code,
                        message,
                        at_millis: chrono::Utc::now().timestamp_millis(),
                    });
                }
            }
            Ok(None) => { /* still running */ }
            Err(_) => { /* treat as still running; will re-check next tick */ }
        }
    }

    async fn stop_owned_child(&mut self) -> anyhow::Result<usize> {
        let mut stopped = 0usize;
        if let Some(mut child) = self.child.take() {
            let mut shutdown_control = self.shutdown_control.take();
            // Tell the crash reaper this is intentional so it doesn't fire.
            self.stopping
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let outcome =
                terminate_owned_child(&mut child, shutdown_control.as_mut(), GRACEFUL_STOP_TIMEOUT)
                    .await;
            self.stopping
                .store(false, std::sync::atomic::Ordering::SeqCst);
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    // A request, termination, or reap error is not proof that
                    // the WAL writer is gone. Preserve the exact owned Child
                    // handle and its capability whenever it may still be live,
                    // so the UI cannot launch another writer against the same
                    // data directory and a later Stop can safely retry.
                    let receipt_error = self.refresh_durability_failure().err();
                    self.restore_child_after_failed_stop(child, shutdown_control);
                    if let Some(receipt_error) = receipt_error {
                        return Err(error.context(format!(
                            "shutdown receipt state is also invalid: {receipt_error}"
                        )));
                    }
                    return Err(error);
                }
            };
            if let Some(reason) = outcome.forced_reason {
                push_log(
                    &self.logs,
                    "warn",
                    format!(
                        "arc-node did not complete a graceful shutdown ({}); forced termination was required after the bounded wait",
                        reason
                    ),
                )
                .await;
            }
            stopped += 1;
            self.refresh_durability_failure()?;
            self.lifecycle_lock = None;
            self.started_at = None;
            self.active_worker_threads = None;
            self.active_launch_plan = None;
            *self.crash_info.lock().await = None;
        }
        Ok(stopped)
    }

    fn restore_child_after_failed_stop(
        &mut self,
        mut child: Child,
        shutdown_control: Option<DesktopShutdownControl>,
    ) {
        match child.try_wait() {
            Ok(Some(_status)) => {
                self.started_at = None;
                self.active_worker_threads = None;
                self.lifecycle_lock = None;
                self.active_launch_plan = None;
            }
            Ok(None) | Err(_) => {
                self.child = Some(child);
                self.shutdown_control = shutdown_control;
            }
        }
    }

    async fn stop_and_retain_lifecycle_lock(&mut self) -> anyhow::Result<StopLifecycleOutcome> {
        anyhow::ensure!(
            self.update_lifecycle_lock.is_none(),
            "a signed desktop update already owns the managed-node lifecycle fence"
        );
        if let Some(error) = self.legacy_windows_stop_error.as_deref() {
            anyhow::bail!(
                "legacy desktop node reconciliation is unresolved; refusing Stop/update boundary: {error}"
            );
        }
        // Transfer the owned-child guard into this stop transaction. Keeping
        // it in `self` is insufficient: `stop_owned_child` consumes the ACK
        // and clears `self.lifecycle_lock`, which used to leave a cross-GUI
        // race before the detached scan and final lifecycle-state check. A
        // second desktop could start a new writer in that gap and this caller
        // could still report an update-safe boundary. A restarted GUI instead
        // acquires the same guard here before inspecting detached processes.
        let mut lifecycle_lock = match self.lifecycle_lock.take() {
            Some(lock) => Some(lock),
            None => self
                .managed_data_dir
                .as_deref()
                .map(|data_dir| {
                    acquire_managed_lifecycle_lock_for_reconciliation(&data_dir.to_string_lossy())
                })
                .transpose()?,
        };
        let mut stopped = match self.stop_owned_child().await {
            Ok(stopped) => stopped,
            Err(error) => {
                // A failed owned-child stop may restore the live Child and its
                // shutdown capability. Reassociate the same OS-held guard so
                // no other desktop can enter the lifecycle while it remains
                // live. If the child is already gone, the durable receipt is
                // the fail-closed recovery fence and the local guard may drop.
                if self.child.is_some() {
                    self.lifecycle_lock = lifecycle_lock.take();
                }
                return Err(error);
            }
        };
        // No managed child handle. This happens when the Tauri process
        // restarted (e.g. cargo rebuild in dev) while arc-node — spawned
        // with CREATE_NEW_PROCESS_GROUP — kept running detached. Locate
        // it by the managed binary path and kill it so the UI's Stop
        // button is not a no-op.
        //
        // Run this even after stopping an owned child. Multiple app instances
        // or an interrupted updater can leave more than one matching detached
        // process, and a release boundary must drain all of them.
        // The tokenless v0.7 proof is a one-time upgrade capability. Move it
        // out before reconciliation so its Zeroizing seed is dropped after a
        // successful scan (including zero matches). Restore it only when the
        // exact proof/termination operation errors and a safe retry is needed.
        let legacy_context = self.legacy_windows_stop_context.take();
        let reconciliation =
            stop_detached_arc_node(legacy_context.as_ref(), self.managed_data_dir.as_deref()).await;
        let detached = finish_legacy_reconciliation(
            &mut self.legacy_windows_stop_context,
            legacy_context,
            reconciliation,
        )?;
        stopped += detached.stopped;
        if detached.forced > 0 {
            push_log(
                &self.logs,
                "warn",
                format!(
                    "{} detached arc-node process(es) did not complete a graceful shutdown; forced termination was required after the bounded wait",
                    detached.forced
                ),
            )
            .await;
        }
        if stopped > 0 {
            self.started_at = None;
            self.active_worker_threads = None;
            *self.crash_info.lock().await = None;
            push_log(
                &self.logs,
                "info",
                format!("arc-node stopped ({} managed process(es))", stopped),
            )
            .await;
        }
        self.refresh_durability_failure()?;
        if let Some(error) = self.durability_failure.as_deref() {
            return Err(ManagedDurabilityRecoveryRequired {
                reason: error.to_owned(),
            }
            .into());
        }
        lifecycle_lock = lifecycle_lock
            .map(refresh_managed_lifecycle_namespace)
            .transpose()?;
        // Return the freshly prepared, still-live guard so callers performing a local mutation
        // or update can extend the same transaction without a release/reopen
        // gap. Plain Stop drops it immediately after this function returns.
        Ok(StopLifecycleOutcome {
            lifecycle_lock,
            stopped,
        })
    }

    pub async fn stop(&mut self) -> anyhow::Result<()> {
        drop(self.stop_and_retain_lifecycle_lock().await?.lifecycle_lock);
        Ok(())
    }

    /// Stop every proven managed writer and retain the exact data-directory
    /// lifecycle lock for the caller's following mutation/start transaction.
    pub async fn stop_for_local_mutation(&mut self) -> anyhow::Result<ManagedLifecycleLock> {
        self.stop_and_retain_lifecycle_lock()
            .await?
            .lifecycle_lock
            .ok_or_else(|| anyhow::anyhow!("managed node data directory is not configured"))
    }

    /// Establish the native half of the updater boundary. The guard remains
    /// owned by this NodeManager after IPC returns and is released only by
    /// process exit/relaunch or an explicit failed-install abort transaction.
    pub async fn prepare_update_relaunch(&mut self) -> anyhow::Result<()> {
        if self.update_lifecycle_lock.is_some() {
            // A second prepare is never a harmless idempotent success. The
            // previous pre-install abort may have retained this fence because
            // detached-process reconciliation failed even though no receipt
            // was visible. Reporting success here would let a UI retry enter
            // install() without re-proving that boundary. The controller
            // coalesces legitimate concurrent clicks, so fail closed until a
            // successful abort releases the existing fence or this GUI exits.
            anyhow::bail!("a signed desktop update already owns the managed-node lifecycle fence");
        }
        anyhow::ensure!(
            !self.update_handoff_started && self.update_restart_plan.is_none(),
            "a prior desktop update lifecycle transaction is unresolved"
        );

        // Capture the exact launch before Stop consumes the child state. If a
        // running owned node somehow lacks this post-spawn identity, do not
        // stop it: an updater failure could not restore what was running.
        let owned_was_running = self.child.is_some();
        let restart_plan = if owned_was_running {
            Some(self.active_launch_plan.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "running managed node has no exact native launch identity; refusing the update boundary"
                )
            })?)
        } else {
            None
        };
        let outcome = self.stop_and_retain_lifecycle_lock().await?;
        let lifecycle_lock = outcome
            .lifecycle_lock
            .ok_or_else(|| anyhow::anyhow!("managed node data directory is not configured"))?;

        // A restarted GUI normally reconciles detached children during app
        // startup, before the updater can run. If one appears here, or if
        // duplicate managed writers were drained, retain the fence and fail
        // closed: there is no single exact owned launch to restore.
        if outcome.stopped > 0 && (restart_plan.is_none() || outcome.stopped != 1) {
            self.update_lifecycle_lock = Some(lifecycle_lock);
            anyhow::bail!(
                "update preparation stopped {count} managed node process(es) but cannot prove one exact owned launch to restore; the node remains stopped and the lifecycle fence remains active",
                count = outcome.stopped
            );
        }
        self.update_restart_plan = restart_plan;
        self.update_handoff_started = false;
        self.update_lifecycle_lock = Some(lifecycle_lock);
        Ok(())
    }

    /// Commit the irreversible updater handoff immediately before the signed
    /// installer is invoked. A frontend or IPC failure after this call must
    /// never route through the pre-install abort-and-resume path.
    pub fn begin_update_handoff(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.update_lifecycle_lock.is_some(),
            "cannot begin updater handoff without a prepared lifecycle fence"
        );
        anyhow::ensure!(
            self.child.is_none(),
            "cannot begin updater handoff while an owned node is live"
        );
        anyhow::ensure!(
            !self.update_handoff_started,
            "updater handoff has already begun"
        );
        self.update_handoff_started = true;
        Ok(())
    }

    /// Abort only after download/signature/preparation rejects before native
    /// handoff, and only after re-proving that no owned or detached writer
    /// exists and the authenticated lifecycle receipt is clear. If Prepare
    /// stopped an owned node, restore its exact launch under the same guard.
    pub async fn abort_update_relaunch(&mut self) -> anyhow::Result<()> {
        if self.update_lifecycle_lock.is_none() {
            return Ok(());
        }
        anyhow::ensure!(
            !self.update_handoff_started,
            "cannot abort or resume the old node after updater handoff has begun"
        );
        anyhow::ensure!(
            self.child.is_none(),
            "cannot release update lifecycle fence while an owned node is live"
        );

        let legacy_context = self.legacy_windows_stop_context.take();
        let reconciliation =
            stop_detached_arc_node(legacy_context.as_ref(), self.managed_data_dir.as_deref()).await;
        let detached = finish_legacy_reconciliation(
            &mut self.legacy_windows_stop_context,
            legacy_context,
            reconciliation,
        )?;
        anyhow::ensure!(
            detached.forced == 0,
            "cannot release update lifecycle fence after an unproven forced stop"
        );
        self.refresh_durability_failure()?;
        if let Some(error) = self.durability_failure.as_deref() {
            return Err(ManagedDurabilityRecoveryRequired {
                reason: error.to_owned(),
            }
            .into());
        }

        let Some(restart_plan) = self.update_restart_plan.take() else {
            // The node was already stopped before Prepare. Release only the
            // updater fence and preserve that stopped state.
            self.update_lifecycle_lock = None;
            self.update_handoff_started = false;
            return Ok(());
        };
        let lifecycle_lock = self
            .update_lifecycle_lock
            .take()
            .expect("checked update lifecycle lock above");
        let resources = TestnetResources {
            seeds_file: Some(restart_plan.seeds.path.clone()),
            genesis_file: Some(restart_plan.genesis.path.clone()),
        };
        let result = self
            .start_while_lifecycle_locked(
                &restart_plan.config,
                &restart_plan.validator_keyfile.path,
                &resources,
                &lifecycle_lock,
                Some(&restart_plan),
            )
            .await;
        match result {
            Ok(()) => {
                // Convert the same continuously-held OS lock from updater
                // ownership back into ordinary child-lifecycle ownership.
                self.lifecycle_lock = Some(lifecycle_lock);
                self.update_handoff_started = false;
                Ok(())
            }
            Err(error) => {
                // Keep both the exact plan and the same lock so a failed
                // identity/preflight/spawn cannot open a cross-GUI writer
                // window or be mistaken for a harmless download error.
                self.update_lifecycle_lock = Some(lifecycle_lock);
                self.update_restart_plan = Some(restart_plan);
                Err(error.context(
                    "could not safely restore the exact pre-update node; the node remains stopped, the update lifecycle fence remains active, and a manual restart is required",
                ))
            }
        }
    }

    pub async fn restart(
        &mut self,
        config: &NodeConfig,
        validator_keyfile: &Path,
        resources: &TestnetResources,
    ) -> anyhow::Result<()> {
        self.stop().await?;
        let lifecycle_lock = acquire_managed_lifecycle_lock(&config.data_dir)?;
        self.start(config, validator_keyfile, resources, lifecycle_lock)
            .await
    }

    pub async fn logs_snapshot(&self, limit: usize) -> Vec<LogEntry> {
        let guard = self.logs.lock().await;
        let n = guard.len().min(limit);
        guard
            .iter()
            .rev()
            .take(n)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

/// Ask the node to run its signal-driven shutdown path first so it can drain
/// RPC work and sync the WAL. A force-kill is a bounded, explicitly reported
/// fallback only; merely accepting a termination request is never treated as
/// a completed updater boundary.
async fn terminate_owned_child(
    child: &mut Child,
    shutdown_control: Option<&mut DesktopShutdownControl>,
    graceful_timeout: Duration,
) -> anyhow::Result<ChildStopOutcome> {
    if let Some(status) = child
        .try_wait()
        .map_err(|error| anyhow::anyhow!("failed to inspect managed arc-node process: {error}"))?
    {
        anyhow::ensure!(
            status.success(),
            "managed arc-node exited with {status} before the shutdown boundary; final WAL durability was not proven"
        );
        if let Some(control) = shutdown_control.as_deref() {
            anyhow::ensure!(
                control.validate_clean_ack()?,
                "managed arc-node exited successfully but did not publish its authenticated clean shutdown ACK"
            );
            control.consume_clean_ack()?;
        }
        return Ok(ChildStopOutcome {
            forced_reason: None,
        });
    }

    let control = shutdown_control
        .ok_or_else(|| anyhow::anyhow!("managed node has no private durable shutdown control"))?;
    control.ensure_receipt_armed()?;
    anyhow::ensure!(
        control.validate_armed_receipt()?,
        "managed-node shutdown receipt disappeared before request publication"
    );
    request_graceful_stop(child, Some(control)).with_context(|| {
        "the platform graceful-stop request failed; refusing an unauthenticated force-stop"
    })?;
    let forced_reason = match tokio::time::timeout(graceful_timeout, child.wait()).await {
        Ok(Ok(status)) => {
            anyhow::ensure!(
                status.success(),
                "managed arc-node exited with {status} after the authenticated shutdown request; final WAL durability was not proven"
            );
            anyhow::ensure!(
                control.validate_clean_ack()?,
                "managed arc-node exited successfully but did not publish its authenticated clean shutdown ACK"
            );
            control.consume_clean_ack()?;
            return Ok(ChildStopOutcome {
                forced_reason: None,
            });
        }
        Ok(Err(error)) => {
            return Err(anyhow::anyhow!(
                "waiting for graceful managed arc-node exit failed: {error}"
            ));
        }
        Err(_) => format!(
            "the process remained alive for {} seconds after the authenticated graceful request",
            graceful_timeout.as_secs_f32()
        ),
    };

    // Avoid a force-kill if the process exited in the narrow interval between
    // the timeout/request error and this fallback decision.
    if let Some(status) = child.try_wait().map_err(|error| {
        anyhow::anyhow!("failed to re-inspect managed arc-node process: {error}")
    })? {
        anyhow::ensure!(
            status.success(),
            "managed arc-node exited with {status} after the authenticated shutdown request; final WAL durability was not proven"
        );
        anyhow::ensure!(
            control.validate_clean_ack()?,
            "managed arc-node exited successfully but did not publish its authenticated clean shutdown ACK"
        );
        control.consume_clean_ack()?;
        return Ok(ChildStopOutcome {
            forced_reason: None,
        });
    } else {
        child.start_kill().map_err(|error| {
            anyhow::anyhow!(
                "failed to force-stop managed arc-node process after {forced_reason}: {error}"
            )
        })?;
        match tokio::time::timeout(FORCE_STOP_TIMEOUT, child.wait()).await {
            Ok(Ok(_status)) => {}
            Ok(Err(error)) => {
                anyhow::bail!("forced arc-node process could not be reaped: {error}")
            }
            Err(_) => anyhow::bail!(
                "timed out after {} seconds reaping the force-stopped arc-node process",
                FORCE_STOP_TIMEOUT.as_secs()
            ),
        }
    }
    anyhow::bail!(
        "managed arc-node required force termination after {forced_reason}; its unproven receipt remains armed and the updater boundary is blocked"
    )
}

#[cfg(unix)]
fn request_graceful_stop(
    child: &Child,
    shutdown_control: Option<&DesktopShutdownControl>,
) -> anyhow::Result<()> {
    let target_pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("managed process has no live PID"))?;
    write_desktop_shutdown_request(
        shutdown_control.ok_or_else(|| {
            anyhow::anyhow!("managed Unix node has no authenticated shutdown control")
        })?,
        target_pid,
    )
}

#[cfg(windows)]
fn request_graceful_stop(
    child: &Child,
    shutdown_control: Option<&DesktopShutdownControl>,
) -> anyhow::Result<()> {
    // CREATE_NO_WINDOW deliberately removes the console-control channel.
    // Use the private local file capability shared with arc-node instead;
    // there is no remotely reachable shutdown route or bearer token in argv.
    let target_pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("managed process has no live PID"))?;
    write_desktop_shutdown_request(
        shutdown_control.ok_or_else(|| {
            anyhow::anyhow!("managed Windows node has no authenticated shutdown control")
        })?,
        target_pid,
    )
}

#[cfg(not(any(unix, windows)))]
fn request_graceful_stop(
    _child: &Child,
    _shutdown_control: Option<&DesktopShutdownControl>,
) -> anyhow::Result<()> {
    anyhow::bail!("this platform exposes no supported graceful process signal")
}

async fn push_log(logs: &Arc<Mutex<VecDeque<LogEntry>>>, level: &str, message: String) {
    let entry = LogEntry {
        id: format!("log-{}", uuid_like()),
        timestamp: chrono::Utc::now().timestamp_millis(),
        level: level.into(),
        message,
    };
    let mut guard = logs.lock().await;
    if guard.len() == LOG_RING_SIZE {
        guard.pop_front();
    }
    guard.push_back(entry);
}

fn uuid_like() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 6];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn classify_level(line: &str) -> &'static str {
    let lower = line.to_ascii_lowercase();
    if lower.contains("error") || lower.contains("panic") {
        "error"
    } else if lower.contains("warn") {
        "warn"
    } else if lower.contains(" ok ") || lower.contains("✓") {
        "ok"
    } else {
        "info"
    }
}

/// An explicitly configured binary path, if the operator set one.
///
/// `ARC_NODE_BIN` is the documented name; `ARC_NODE_BINARY` is kept because
/// existing test fixtures and dev scripts already export it. Both win over
/// every other resolution step — if someone names a binary, that is the
/// binary, and `ensure_binary` must not second-guess it with a download.
pub fn env_binary_override() -> Option<PathBuf> {
    for key in ["ARC_NODE_BIN", "ARC_NODE_BINARY"] {
        if let Some(v) = std::env::var_os(key) {
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
}

/// A locally built `target/release/arc-node`, if this app is running from a
/// repo checkout.
///
/// This is the path the demo machine takes: the repo has a freshly built
/// arc-node that matches the desktop version, while the published GitHub
/// release ships no arc-node assets at all. Without this, a dev-mode run has
/// nothing to launch even with the binary sitting two directories away.
///
/// Searched relative to both the working directory (`cargo tauri dev` runs
/// from `desktop/src-tauri`, some tooling from `desktop/`) and the running
/// executable's ancestors (`target/debug/arc-desktop` lives inside the same
/// checkout), so it resolves whichever way the app was launched.
pub fn dev_build_binary() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        "arc-node.exe"
    } else {
        "arc-node"
    };

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        // desktop/src-tauri → ../.. = repo root; desktop → .. = repo root.
        roots.push(cwd.join("..").join(".."));
        roots.push(cwd.join(".."));
        roots.push(cwd.clone());
    }
    if let Ok(exe) = std::env::current_exe() {
        // Walk up from the executable; one of these ancestors is the repo
        // root when running a dev build out of target/.
        roots.extend(exe.ancestors().take(8).map(PathBuf::from));
    }

    for root in roots {
        let cand = root.join("target").join("release").join(exe_name);
        if cand.is_file() {
            // Canonicalize so the `../..` forms turn into a clean absolute
            // path in logs and in the "detached process" comparison.
            return Some(cand.canonicalize().unwrap_or(cand));
        }
    }
    None
}

fn resolve_binary() -> anyhow::Result<PathBuf> {
    // 1. Explicit override (env). Highest precedence, no questions asked.
    if let Some(p) = env_binary_override() {
        return Ok(p);
    }

    // 2. Canonical app-managed path. Auto-download (commands::ensure_binary)
    //    writes here on first launch.
    let managed = managed_binary_path();
    if managed.exists() {
        return Ok(managed);
    }

    // 3. A release build sitting in this repo checkout (dev + demo machines).
    if let Some(p) = dev_build_binary() {
        return Ok(p);
    }

    // 4. PATH lookup, for devs who installed arc-node system-wide.
    if let Ok(p) = which_on_path("arc-node") {
        return Ok(p);
    }

    Err(anyhow::anyhow!(
        "arc-node binary not found. Looked at $ARC_NODE_BIN, {}, ./target/release/arc-node in this checkout, and PATH. \
         Build one with `cargo build --release -p arc-node`, or set ARC_NODE_BIN to an existing binary.",
        managed.display()
    ))
}

fn required_testnet_resources(resources: &TestnetResources) -> anyhow::Result<(&Path, &Path)> {
    let seeds = resources.seeds_file.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "ARC refused to start because signed bundle resource testnet-seeds.txt is missing"
        )
    })?;
    let genesis = resources.genesis_file.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "ARC refused to start because signed bundle resource genesis.toml is missing"
        )
    })?;

    require_regular_bundle_resource(seeds, "testnet-seeds.txt")?;
    require_regular_bundle_resource(genesis, "genesis.toml")?;
    Ok((seeds, genesis))
}

fn require_regular_bundle_resource(path: &Path, name: &str) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        anyhow::anyhow!(
            "ARC refused to start because signed bundle resource {name} at {} cannot be inspected: {error}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "ARC refused to start because signed bundle resource {name} at {} is not a regular file",
            path.display()
        );
    }
    Ok(())
}

/// Does this arc-node accept `flag`?
///
/// clap aborts the process on an unrecognized argument, so every optional
/// flag must be probed before use or a node that would have started fine at
/// default settings refuses to start at all. `--help` output is the only
/// version-independent way to ask.
///
/// Cached per (binary, flag): `--help` costs a process spawn, and `start()`
/// runs on every user-visible Start click.
#[cfg(windows)]
fn windows_binary_file_identity(path: &Path) -> Option<String> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let file = std::fs::File::open(path).ok()?;
    // SAFETY: the all-zero value is a valid output buffer for the Win32 API.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the File owns a live handle and `information` is writable for
    // the duration of this synchronous call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } == 0 {
        return None;
    }
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Some(format!("{}:{file_index}", information.dwVolumeSerialNumber))
}

pub fn binary_supports_flag(binary: &Path, flag: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex as StdMutex, OnceLock};

    static CACHE: OnceLock<StdMutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| StdMutex::new(HashMap::new()));
    let canonical = binary
        .canonicalize()
        .unwrap_or_else(|_| binary.to_path_buf());
    let freshness = std::fs::metadata(&canonical)
        .map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            let created = metadata
                .created()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            #[cfg(unix)]
            let stable_identity = {
                use std::os::unix::fs::MetadataExt as _;
                format!("{}:{}", metadata.dev(), metadata.ino())
            };
            #[cfg(windows)]
            let stable_identity = windows_binary_file_identity(&canonical)
                .unwrap_or_else(|| "unavailable-file-identity".into());
            format!("{stable_identity}:{}:{modified}:{created}", metadata.len())
        })
        .unwrap_or_else(|_| "missing".into());
    // The updater replaces the executable at the same managed path. Include
    // file freshness so a v0.7 negative probe cannot poison the subsequent
    // v0.8 capability check in the same desktop process.
    let key = format!("{}\u{0}{freshness}\u{0}{flag}", canonical.display());
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(&key) {
            return *hit;
        }
    }

    let mut cmd = std::process::Command::new(&canonical);
    cmd.arg("--help");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let supported = match cmd.output() {
        Ok(out) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            // Match the flag followed by a boundary so `--threads` does not
            // report true because `--bench-rayon-threads` is present.
            text.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
                .any(|tok| tok == flag)
        }
        Err(_) => false,
    };
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, supported);
    }
    supported
}

/// Canonical location for the auto-downloaded arc-node binary.
/// `~/.arc/bin/arc-node` (or `.exe` on Windows). Public so commands.rs can
/// write to the same path during auto-download.
fn unique_command_flag_value<'a>(
    command: &'a [std::ffi::OsString],
    flag: &str,
) -> Option<&'a std::ffi::OsStr> {
    let flag = std::ffi::OsStr::new(flag);
    let mut values = command
        .windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1].as_os_str());
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

/// Recover a capability only from the exact argv shape emitted by the
/// desktop: one canonical data directory and the exact token beneath that
/// directory's named private control boundary. Merely executing the same
/// arc-node binary is never evidence that a manual/headless node belongs to
/// the desktop.
#[cfg(any(unix, windows, test))]
fn desktop_shutdown_control_from_command(
    command: &[std::ffi::OsString],
) -> anyhow::Result<Option<DesktopShutdownControl>> {
    let token_flag = std::ffi::OsStr::new("--desktop-shutdown-token-file");
    let token_occurrences = command.iter().filter(|value| *value == token_flag).count();
    if token_occurrences == 0 {
        return Ok(None);
    }
    anyhow::ensure!(
        token_occurrences == 1,
        "managed desktop shutdown capability flag is duplicated"
    );
    let token_file_arg = unique_command_flag_value(command, "--desktop-shutdown-token-file")
        .ok_or_else(|| anyhow::anyhow!("managed desktop shutdown capability has no value"))?;
    let data_flag = std::ffi::OsStr::new("--data-dir");
    anyhow::ensure!(
        command.iter().filter(|value| *value == data_flag).count() == 1,
        "managed desktop shutdown capability requires exactly one data directory"
    );
    let data_dir_arg = unique_command_flag_value(command, "--data-dir")
        .ok_or_else(|| anyhow::anyhow!("managed desktop data directory has no value"))?;
    let raw_data_dir = Path::new(data_dir_arg);
    let raw_token_file = Path::new(token_file_arg);
    let Some(raw_control_dir) = raw_token_file.parent() else {
        anyhow::bail!("managed desktop shutdown capability has no parent directory");
    };
    if raw_token_file.file_name() != Some(std::ffi::OsStr::new(DESKTOP_SHUTDOWN_TOKEN_FILE_NAME))
        || raw_control_dir.file_name()
            != Some(std::ffi::OsStr::new(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME))
    {
        anyhow::bail!("managed desktop shutdown capability has an unexpected path shape");
    }
    // Once argv has the exact desktop-emitted path binding, recovery is
    // fail-closed: a deleted token, corrupt private directory, or access/DACL
    // failure means the updater cannot prove a safe stop boundary.
    let data_dir = raw_data_dir.canonicalize().with_context(|| {
        format!(
            "cannot recover exact managed desktop data directory {}",
            raw_data_dir.display()
        )
    })?;
    let expected_control_dir = data_dir.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
    let control_dir = raw_control_dir.canonicalize().with_context(|| {
        format!(
            "cannot recover advertised desktop shutdown control directory {}",
            raw_control_dir.display()
        )
    })?;
    if control_dir != expected_control_dir {
        anyhow::bail!("managed desktop shutdown capability is not bound to its data directory");
    }
    let token_file = raw_token_file.canonicalize().with_context(|| {
        format!(
            "cannot recover exact managed desktop shutdown token {}",
            raw_token_file.display()
        )
    })?;
    let expected_token_file = control_dir.join(DESKTOP_SHUTDOWN_TOKEN_FILE_NAME);
    anyhow::ensure!(
        token_file == expected_token_file,
        "exact managed desktop shutdown token changed identity"
    );
    read_desktop_shutdown_control(&token_file)
        .map(Some)
        .with_context(|| {
            format!(
                "cannot recover exact managed desktop shutdown control {}",
                token_file.display()
            )
        })
}

fn constant_time_legacy_seed_eq(candidate: &std::ffi::OsStr, expected: &Zeroizing<String>) -> bool {
    let Some(candidate) = candidate.to_str() else {
        return false;
    };
    let candidate = candidate.trim().as_bytes();
    let expected = expected.trim().as_bytes();
    if candidate.len() > LEGACY_VALIDATOR_SEED_MAX_BYTES
        || expected.len() > LEGACY_VALIDATOR_SEED_MAX_BYTES
    {
        return false;
    }
    // Both identities are compared across the same public upper bound. This
    // avoids a secret-dependent early mismatch while retaining exact equality
    // (including length) for the recovery phrase that old desktop builds put
    // in argv. The expected copy remains Zeroizing and is never formatted.
    let mut difference = candidate.len() ^ expected.len();
    for index in 0..LEGACY_VALIDATOR_SEED_MAX_BYTES {
        let left = candidate.get(index).copied().unwrap_or(0);
        let right = expected.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn canonical_process_path(value: &std::ffi::OsStr, process_cwd: Option<&Path>) -> Option<PathBuf> {
    let path = Path::new(value);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        process_cwd?.join(path)
    };
    resolved.canonicalize().ok()
}

fn legacy_packaged_resource_matches(
    value: &std::ffi::OsStr,
    process_cwd: Option<&Path>,
    current_signed_resource: &Path,
    expected_name: &str,
) -> bool {
    if let Some(canonical) = canonical_process_path(value, process_cwd) {
        return canonical == current_signed_resource;
    }
    // Linux AppImage resources live beneath an ephemeral /tmp/.mount_* tree
    // that disappears when the old GUI relaunches even though its detached
    // node survives. The real public tags always emitted both resources from
    // the signed bundle's `resources/` directory. When that mount is gone, the
    // remaining exact evidence is the immutable tag argv shape plus managed
    // executable, persisted secret seed, data dir, ports, role and model. Only
    // accept that exact packaged lexical suffix; arbitrary missing paths do
    // not qualify.
    let path = Path::new(value);
    path.file_name() == Some(std::ffi::OsStr::new(expected_name))
        && path.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("resources"))
}

fn legacy_windows_command_matches(
    command: &[std::ffi::OsString],
    process_cwd: Option<&Path>,
    context: &LegacyWindowsStopContext,
) -> bool {
    use std::collections::HashMap;

    if unique_command_flag_value(command, "--desktop-shutdown-token-file").is_some() {
        return false;
    }
    let mut values = HashMap::<&str, Vec<&std::ffi::OsStr>>::new();
    let mut flags = Vec::<&str>::new();
    let mut index = usize::from(
        command
            .first()
            .and_then(|arg| arg.to_str())
            .is_some_and(|arg| !arg.starts_with("--")),
    );
    while index < command.len() {
        let Some(flag) = command[index].to_str() else {
            return false;
        };
        match flag {
            "--community-mode" => {
                flags.push(flag);
                index += 1;
            }
            "--rpc" | "--p2p-port" | "--data-dir" | "--eth-rpc-port" | "--validator-seed"
            | "--seeds-file" | "--genesis" | "--model" => {
                let Some(value) = command.get(index + 1) else {
                    return false;
                };
                values.entry(flag).or_default().push(value.as_os_str());
                index += 2;
            }
            _ => return false,
        }
    }
    let exactly_one = |flag: &str| values.get(flag).filter(|found| found.len() == 1);
    let canonical_value = |flag: &str| {
        exactly_one(flag).and_then(|found| canonical_process_path(found[0], process_cwd))
    };
    let resources_match = match (values.get("--seeds-file"), values.get("--genesis")) {
        (Some(seeds), Some(genesis)) if seeds.len() == 1 && genesis.len() == 1 => {
            legacy_packaged_resource_matches(
                seeds[0],
                process_cwd,
                &context.seeds_file,
                "testnet-seeds.txt",
            ) && legacy_packaged_resource_matches(
                genesis[0],
                process_cwd,
                &context.genesis_file,
                "genesis.toml",
            )
        }
        _ => false,
    };
    if canonical_value("--data-dir").as_ref() != Some(&context.data_dir)
        || !resources_match
        || exactly_one("--eth-rpc-port").and_then(|v| v[0].to_str()) != Some("0")
        || !exactly_one("--validator-seed")
            .is_some_and(|values| constant_time_legacy_seed_eq(values[0], &context.validator_seed))
    {
        return false;
    }
    let Some(rpc) = exactly_one("--rpc").and_then(|v| v[0].to_str()) else {
        return false;
    };
    let Some(rpc_port) = rpc
        .strip_prefix("127.0.0.1:")
        .and_then(|port| port.parse::<u16>().ok())
    else {
        return false;
    };
    let Some(p2p_port) = exactly_one("--p2p-port")
        .and_then(|v| v[0].to_str())
        .and_then(|port| port.parse::<u16>().ok())
    else {
        return false;
    };
    if !context.allowed_port_pairs.contains(&(rpc_port, p2p_port)) {
        return false;
    }
    let model_matches = match (&context.model_path, exactly_one("--model")) {
        (None, None) => true,
        (Some(expected), Some(found)) => {
            canonical_process_path(found[0], process_cwd)
                == canonical_process_path(expected.as_os_str(), process_cwd)
        }
        _ => false,
    };
    if !model_matches {
        return false;
    }
    flags.sort_unstable();
    let expected_flags = if context.worker_mode {
        vec!["--community-mode"]
    } else {
        Vec::new()
    };
    if flags != expected_flags {
        return false;
    }
    std::fs::symlink_metadata(&context.data_dir)
        .is_ok_and(|metadata| metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
}

pub fn detect_running_legacy_v07_data_dir(
    config: &NodeConfig,
    validator_seed: &Zeroizing<String>,
    resources: &TestnetResources,
) -> anyhow::Result<Option<PathBuf>> {
    let managed_path = managed_binary_path();
    let managed = match managed_path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot canonicalize managed legacy executable"),
    };
    let mut system = sysinfo::System::new();
    refresh_process_command_metadata(&mut system);
    let mut matches = Vec::new();
    for process in system.processes().values() {
        let Some(executable) = process.exe() else {
            continue;
        };
        if executable
            .canonicalize()
            .unwrap_or_else(|_| executable.to_path_buf())
            != managed
        {
            continue;
        }
        let Some(data_value) = unique_command_flag_value(process.cmd(), "--data-dir") else {
            continue;
        };
        let Some(data_dir) = canonical_process_path(data_value, process.cwd()) else {
            continue;
        };
        let context = build_legacy_windows_stop_context(
            config,
            data_dir.clone(),
            validator_seed.clone(),
            resources,
        )?;
        if legacy_windows_command_matches(process.cmd(), process.cwd(), &context) {
            matches.push(data_dir);
        }
    }
    anyhow::ensure!(
        matches.len() <= 1,
        "more than one exact managed v0.7 desktop process is live; refusing ambiguous data migration"
    );
    Ok(matches.pop())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DetachedProcessIdentity {
    pid: sysinfo::Pid,
    start_time: u64,
    executable: PathBuf,
}

struct ManagedDetachedProcess {
    identity: DetachedProcessIdentity,
    #[cfg_attr(not(windows), allow(dead_code))]
    shutdown_control: DesktopShutdownControl,
    #[cfg(windows)]
    handle: WindowsProcessHandle,
}

#[cfg(windows)]
struct WindowsProcessHandle {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
// Windows process handles are kernel object references and may be waited or
// terminated from any thread. Ownership remains unique in this RAII wrapper.
unsafe impl Send for WindowsProcessHandle {}
#[cfg(windows)]
// Waiting/querying a process kernel object through shared references is safe;
// termination remains serialized by the owning NodeManager stop operation.
unsafe impl Sync for WindowsProcessHandle {}

#[cfg(windows)]
impl WindowsProcessHandle {
    fn open(pid: u32, expected_start_time: u64) -> std::io::Result<Self> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            PROCESS_TERMINATE,
        };
        // SAFETY: OpenProcess receives a numeric PID and returns an owned
        // kernel handle; handle inheritance is explicitly disabled.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: outputs point to initialized FILETIME storage and the
        // retained handle has query access.
        if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0
        {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
            return Err(std::io::Error::last_os_error());
        }
        let windows_ticks =
            ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
        const WINDOWS_TO_UNIX_EPOCH_SECONDS: u64 = 11_644_473_600;
        let start_time = windows_ticks
            .checked_div(10_000_000)
            .and_then(|seconds| seconds.checked_sub(WINDOWS_TO_UNIX_EPOCH_SECONDS))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed Windows process has an invalid creation timestamp",
                )
            })?;
        if start_time != expected_start_time {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "managed Windows PID was recycled while its identity handle was captured",
            ));
        }
        Ok(Self { handle })
    }

    fn is_live(&self) -> std::io::Result<bool> {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: this wrapper owns a valid process handle for its lifetime.
        match unsafe { WaitForSingleObject(self.handle, 0) } {
            WAIT_TIMEOUT => Ok(true),
            WAIT_OBJECT_0 => Ok(false),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    fn exit_code(&self) -> std::io::Result<Option<u32>> {
        use windows_sys::Win32::Foundation::STILL_ACTIVE;
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        let mut code = 0u32;
        // SAFETY: this wrapper owns a query-capable process handle and `code`
        // is a live writable output for the duration of the call.
        if unsafe { GetExitCodeProcess(self.handle, &mut code) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((code != STILL_ACTIVE as u32).then_some(code))
    }

    fn image_path(&self) -> std::io::Result<PathBuf> {
        use std::os::windows::ffi::OsStringExt as _;
        use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;

        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        // SAFETY: the buffer is writable for `length` UTF-16 units and this
        // wrapper owns a query-capable process handle.
        if unsafe { QueryFullProcessImageNameW(self.handle, 0, buffer.as_mut_ptr(), &mut length) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate(length as usize);
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
    }

    fn terminate(&self) -> std::io::Result<()> {
        use windows_sys::Win32::System::Threading::TerminateProcess;
        // SAFETY: this is the retained handle opened for the exact managed
        // process before the graceful wait; PID reuse cannot retarget it.
        if unsafe { TerminateProcess(self.handle, 1) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsProcessHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the successful OpenProcess handle.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(target_os = "linux")]
struct UnixLegacyProcessHandle {
    pidfd: std::fs::File,
}

#[cfg(target_os = "linux")]
impl UnixLegacyProcessHandle {
    fn open(pid: u32, _expected_executable: &Path) -> anyhow::Result<Self> {
        use std::os::fd::FromRawFd as _;

        // SAFETY: pidfd_open takes a numeric pid and zero flags, retains no
        // caller pointers, and returns a new owned descriptor on success.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as i32;
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("Linux pidfd_open is unavailable for legacy reconciliation");
        }
        // SAFETY: the successful syscall returned a uniquely owned descriptor.
        Ok(Self {
            pidfd: unsafe { std::fs::File::from_raw_fd(fd) },
        })
    }

    fn signal_term(&self) -> anyhow::Result<()> {
        use std::os::fd::AsRawFd as _;

        // SAFETY: pidfd_send_signal targets the retained process object, not a
        // reusable numeric PID. siginfo is null and flags are zero.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
                .context("kernel-bound SIGTERM to legacy arc-node failed")
        }
    }

    fn is_live(&self) -> anyhow::Result<bool> {
        use std::os::fd::AsRawFd as _;

        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd for this call.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("polling legacy pidfd failed");
        }
        Ok(result == 0)
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MacAuditToken {
    values: [u32; 8],
}

#[cfg(target_os = "macos")]
struct UnixLegacyProcessHandle {
    token: MacAuditToken,
    pidpath: unsafe extern "C" fn(*mut MacAuditToken, *mut std::ffi::c_void, u32) -> i32,
    signal: unsafe extern "C" fn(*mut MacAuditToken, i32) -> i32,
}

#[cfg(target_os = "macos")]
impl UnixLegacyProcessHandle {
    fn symbol<T: Copy>(name: &'static [u8]) -> anyhow::Result<T> {
        anyhow::ensure!(
            name.last() == Some(&0),
            "dynamic symbol name is not terminated"
        );
        // SAFETY: RTLD_DEFAULT asks dyld for a process-global symbol. The
        // caller supplies the exact public SDK function signature.
        let pointer =
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr().cast::<libc::c_char>()) };
        anyhow::ensure!(
            !pointer.is_null(),
            "required macOS audit-token process API is unavailable"
        );
        // SAFETY: dlsym returned the named function; T is the matching ABI.
        Ok(unsafe { std::mem::transmute_copy::<*mut std::ffi::c_void, T>(&pointer) })
    }

    fn open(pid: u32, expected_executable: &Path) -> anyhow::Result<Self> {
        type MachPort = u32;
        type KernReturn = i32;
        const KERN_SUCCESS: KernReturn = 0;
        const TASK_AUDIT_TOKEN: i32 = 15;
        const TASK_AUDIT_TOKEN_COUNT: u32 = 8;

        unsafe extern "C" {
            static mach_task_self_: MachPort;
            fn task_name_for_pid(
                target_task: MachPort,
                pid: libc::c_int,
                task_name: *mut MachPort,
            ) -> KernReturn;
            fn task_info(
                target_task: MachPort,
                flavor: i32,
                task_info: *mut i32,
                task_info_count: *mut u32,
            ) -> KernReturn;
            fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
        }

        let self_task = unsafe { mach_task_self_ };
        let mut name = 0;
        let status = unsafe { task_name_for_pid(self_task, pid as i32, &mut name) };
        anyhow::ensure!(
            status == KERN_SUCCESS && name != 0,
            "cannot retain a macOS task-name port for legacy pid {pid}"
        );
        let mut token = MacAuditToken { values: [0; 8] };
        let mut count = TASK_AUDIT_TOKEN_COUNT;
        let status = unsafe {
            task_info(
                name,
                TASK_AUDIT_TOKEN,
                token.values.as_mut_ptr().cast::<i32>(),
                &mut count,
            )
        };
        // The audit token remains stable after releasing the temporary task
        // port and includes the kernel pidversion that rejects PID reuse.
        let _ = unsafe { mach_port_deallocate(self_task, name) };
        anyhow::ensure!(
            status == KERN_SUCCESS && count == TASK_AUDIT_TOKEN_COUNT,
            "cannot obtain a macOS audit token for legacy pid {pid}"
        );
        let pidpath = Self::symbol::<
            unsafe extern "C" fn(*mut MacAuditToken, *mut std::ffi::c_void, u32) -> i32,
        >(b"proc_pidpath_audittoken\0")?;
        let signal = Self::symbol::<unsafe extern "C" fn(*mut MacAuditToken, i32) -> i32>(
            b"proc_signal_with_audittoken\0",
        )?;
        let handle = Self {
            token,
            pidpath,
            signal,
        };
        let actual = handle.image_path()?;
        let actual = actual.canonicalize().unwrap_or(actual);
        anyhow::ensure!(
            actual == expected_executable,
            "macOS audit token names a different legacy executable"
        );
        Ok(handle)
    }

    fn image_path(&self) -> anyhow::Result<PathBuf> {
        use std::os::unix::ffi::OsStringExt as _;

        let mut token = self.token;
        let mut buffer = vec![0u8; 4096];
        let length = unsafe {
            (self.pidpath)(
                &mut token,
                buffer.as_mut_ptr().cast::<std::ffi::c_void>(),
                buffer.len() as u32,
            )
        };
        if length <= 0 {
            return Err(std::io::Error::last_os_error())
                .context("macOS audit-token process path is unavailable");
        }
        buffer.truncate(length as usize);
        if buffer.last() == Some(&0) {
            buffer.pop();
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
    }

    fn signal_term(&self) -> anyhow::Result<()> {
        let mut token = self.token;
        let result = unsafe { (self.signal)(&mut token, libc::SIGTERM) };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
                .context("audit-token-bound SIGTERM to legacy arc-node failed")
        }
    }

    fn is_live(&self) -> anyhow::Result<bool> {
        match self.image_path() {
            Ok(_) => Ok(true),
            Err(error) => {
                let gone = error
                    .chain()
                    .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                    .any(|cause| {
                        cause.raw_os_error().is_some_and(|code| {
                            code == libc::ESRCH || code == libc::ENOENT || code == libc::EINVAL
                        })
                    });
                if gone {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
struct UnixLegacyProcessHandle;

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
impl UnixLegacyProcessHandle {
    fn open(_pid: u32, _expected_executable: &Path) -> anyhow::Result<Self> {
        anyhow::bail!("this Unix platform has no supported kernel-bound legacy process handle")
    }

    fn signal_term(&self) -> anyhow::Result<()> {
        anyhow::bail!("this Unix platform has no supported kernel-bound legacy signal API")
    }

    fn is_live(&self) -> anyhow::Result<bool> {
        anyhow::bail!("this Unix platform has no supported kernel-bound legacy liveness API")
    }
}

/// Find and stop only detached arc-node processes that prove desktop ownership
/// through an exact data-dir-bound private capability in argv. Same-binary
/// manual/headless nodes are deliberately ignored. Unix nodes receive SIGTERM;
/// Windows nodes use the private request and a retained process handle so PID
/// reuse can never redirect the force fallback.
async fn stop_detached_arc_node(
    legacy_context: Option<&LegacyWindowsStopContext>,
    expected_data_dir: Option<&Path>,
) -> anyhow::Result<StopSummary> {
    let mut sys = sysinfo::System::new();
    refresh_process_command_metadata(&mut sys);
    let mut targeted = Vec::<ManagedDetachedProcess>::new();
    for proc_ in sys.processes().values() {
        let Some(expected_data_dir) = expected_data_dir else {
            continue;
        };
        let token_flag = std::ffi::OsStr::new("--desktop-shutdown-token-file");
        if !proc_.cmd().iter().any(|argument| argument == token_flag) {
            continue;
        }
        let Some(exe) = proc_.exe() else { continue };
        let exe_canon = exe.canonicalize().unwrap_or(exe.to_path_buf());
        let mut shutdown_control = match desktop_shutdown_control_from_command(proc_.cmd()) {
            Ok(Some(control)) if control.data_dir == expected_data_dir => control,
            Ok(Some(_)) => continue,
            Ok(None) => continue,
            Err(error) => {
                let data_targets_expected = unique_command_flag_value(proc_.cmd(), "--data-dir")
                    .and_then(|value| canonical_process_path(value, proc_.cwd()))
                    .as_deref()
                    == Some(expected_data_dir);
                let token_targets_expected = proc_
                    .cmd()
                    .windows(2)
                    .filter(|pair| pair[0] == token_flag)
                    .any(|pair| {
                        let token = Path::new(&pair[1]);
                        token.file_name()
                            == Some(std::ffi::OsStr::new(DESKTOP_SHUTDOWN_TOKEN_FILE_NAME))
                            && token
                                .parent()
                                .and_then(Path::parent)
                                .and_then(|parent| parent.canonicalize().ok())
                                .as_deref()
                                == Some(expected_data_dir)
                    });
                if !data_targets_expected && !token_targets_expected {
                    continue;
                }
                return Err(error).with_context(|| {
                    format!(
                        "managed detached arc-node pid {} advertises an unrecoverable desktop shutdown capability; refusing to cross the update boundary",
                        proc_.pid()
                    )
                });
            }
        };
        let genesis_arg = unique_command_flag_value(proc_.cmd(), "--genesis").ok_or_else(|| {
            anyhow::anyhow!(
                "managed detached arc-node pid {} does not advertise exactly one genesis file",
                proc_.pid()
            )
        })?;
        let genesis = canonical_process_path(genesis_arg, proc_.cwd()).ok_or_else(|| {
            anyhow::anyhow!(
                "managed detached arc-node pid {} has an unreadable genesis path",
                proc_.pid()
            )
        })?;
        shutdown_control
            .bind_receipt_identity(&exe_canon, &genesis)
            .with_context(|| {
                format!(
                    "managed detached arc-node pid {} has an invalid inherited shutdown receipt",
                    proc_.pid()
                )
            })?;
        let identity = DetachedProcessIdentity {
            pid: proc_.pid(),
            start_time: proc_.start_time(),
            executable: exe_canon,
        };
        #[cfg(windows)]
        let handle = WindowsProcessHandle::open(proc_.pid().as_u32(), proc_.start_time())
            .with_context(|| {
                format!(
                    "failed to retain exact identity handle for managed detached arc-node pid {}",
                    proc_.pid()
                )
            })?;
        #[cfg(windows)]
        ensure_windows_process_image(&handle, &identity)?;
        #[cfg(windows)]
        {
            // sysinfo timestamps have one-second resolution. After retaining
            // the kernel handle, refresh argv/cwd and repeat the full private
            // capability + receipt proof for this PID so same-exe PID reuse
            // within that second cannot inherit the stale initial proof.
            let mut refreshed = sysinfo::System::new();
            refresh_process_command_metadata(&mut refreshed);
            let process = refreshed
                .process(identity.pid)
                .filter(|process| process_matches_identity(process, &identity))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "managed detached process identity changed while retaining pid {}",
                        identity.pid
                    )
                })?;
            let mut fresh_control = desktop_shutdown_control_from_command(process.cmd())?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "managed detached process pid {} lost its shutdown capability",
                        identity.pid
                    )
                })?;
            let fresh_genesis_arg = unique_command_flag_value(process.cmd(), "--genesis")
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "managed detached process pid {} lost its exact genesis argument",
                        identity.pid
                    )
                })?;
            let fresh_genesis = canonical_process_path(fresh_genesis_arg, process.cwd())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "managed detached process pid {} has an unreadable refreshed genesis path",
                        identity.pid
                    )
                })?;
            fresh_control.bind_receipt_identity(&identity.executable, &fresh_genesis)?;
            anyhow::ensure!(
                same_desktop_shutdown_control(&shutdown_control, &fresh_control)?,
                "managed detached process capability changed while retaining pid {}",
                identity.pid
            );
            ensure_windows_process_image(&handle, &identity)?;
        }
        targeted.push(ManagedDetachedProcess {
            identity,
            shutdown_control,
            #[cfg(windows)]
            handle,
        });
    }
    if targeted.is_empty() {
        if let Some(context) = legacy_context {
            return stop_one_proven_legacy_node(context).await;
        }
        return Ok(StopSummary::default());
    }

    // Arm every exact-bound receipt before publishing any request. This
    // avoids partially draining a multi-process set when another target's
    // durable receipt cannot be established.
    for target in &mut targeted {
        target
            .shutdown_control
            .ensure_receipt_armed()
            .with_context(|| {
                format!(
                    "failed to arm durable shutdown receipt for managed detached arc-node pid {}",
                    target.identity.pid
                )
            })?;
    }
    for target in &targeted {
        write_desktop_shutdown_request(
            &target.shutdown_control,
            target.identity.pid.as_u32(),
        )
        .with_context(|| {
            format!(
                "failed to deliver authenticated graceful stop to managed detached arc-node pid {}; refusing force fallback",
                target.identity.pid
            )
        })?;
    }

    let still_targeted = wait_for_managed_processes(&targeted, GRACEFUL_STOP_TIMEOUT).await?;
    for (index, target) in targeted.iter().enumerate() {
        if still_targeted.contains(&index) {
            continue;
        }
        #[cfg(windows)]
        {
            let code = target.handle.exit_code()?.ok_or_else(|| {
                anyhow::anyhow!(
                    "managed detached arc-node pid {} remained live after its wait completed",
                    target.identity.pid
                )
            })?;
            anyhow::ensure!(
                code == 0,
                "managed detached arc-node pid {} exited with code {code}; final WAL durability was not proven",
                target.identity.pid
            );
        }
        anyhow::ensure!(
            target.shutdown_control.validate_clean_ack()?,
            "managed detached arc-node pid {} exited without publishing its authenticated clean shutdown ACK",
            target.identity.pid
        );
        target.shutdown_control.consume_clean_ack()?;
    }
    #[cfg(unix)]
    if let Some(index) = still_targeted.first() {
        anyhow::bail!(
            "managed detached arc-node pid {} exceeded the graceful shutdown budget; Unix exposes no retained child exit status/kernel handle here, so refusing a numeric-PID force fallback and keeping its receipt armed",
            targeted[*index].identity.pid
        );
    }
    #[cfg(windows)]
    for index in &still_targeted {
        let target = &targeted[*index];
        ensure_windows_process_image(&target.handle, &target.identity)?;
        target.handle.terminate().with_context(|| {
                format!(
                    "operating system refused the retained-handle force fallback for detached arc-node pid {}",
                    target.identity.pid
                )
            })?;
    }

    let remaining =
        wait_for_managed_process_indices(&targeted, &still_targeted, FORCE_STOP_TIMEOUT).await?;
    if !remaining.is_empty() {
        anyhow::bail!(
            "timed out waiting for force-stopped detached arc-node process(es) {} to exit",
            remaining
                .iter()
                .map(|index| targeted[*index].identity.pid.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !still_targeted.is_empty() {
        anyhow::bail!(
            "{} managed detached arc-node process(es) required force termination; their unproven receipts remain armed and the updater boundary is blocked",
            still_targeted.len()
        );
    }

    let summary = StopSummary {
        stopped: targeted.len(),
        forced: still_targeted.len(),
    };
    let mut summary = summary;
    if let Some(context) = legacy_context {
        let legacy = stop_one_proven_legacy_node(context).await?;
        summary.stopped += legacy.stopped;
        summary.forced += legacy.forced;
    }
    Ok(summary)
}

#[cfg(windows)]
async fn stop_one_proven_legacy_node(
    context: &LegacyWindowsStopContext,
) -> anyhow::Result<StopSummary> {
    let managed_path = managed_binary_path();
    let managed = match managed_path.canonicalize() {
        Ok(managed) => managed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StopSummary::default());
        }
        Err(error) => {
            return Err(error)
                .context("cannot canonicalize the legacy managed arc-node executable");
        }
    };
    if binary_supports_flag(&managed, "--desktop-shutdown-token-file") {
        // The running-image lock prevents replacing a live legacy executable
        // at this exact managed path on Windows. A capability-aware image is
        // therefore proof that the one-time tokenless upgrade edge is over.
        return Ok(StopSummary::default());
    }
    let mut system = sysinfo::System::new();
    refresh_process_command_metadata(&mut system);
    let matches = system
        .processes()
        .values()
        .filter(|process| {
            process
                .exe()
                .map(|exe| exe.canonicalize().unwrap_or(exe.to_path_buf()) == managed)
                .unwrap_or(false)
                && legacy_windows_command_matches(process.cmd(), process.cwd(), context)
        })
        .map(|process| DetachedProcessIdentity {
            pid: process.pid(),
            start_time: process.start_time(),
            executable: managed.clone(),
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Ok(StopSummary::default());
    }
    anyhow::ensure!(
        matches.len() == 1,
        "refusing tokenless legacy migration because more than one exact desktop argv match is live"
    );
    let identity = &matches[0];
    let handle = WindowsProcessHandle::open(identity.pid.as_u32(), identity.start_time)
        .context("failed to retain the one-time legacy migration process handle")?;
    ensure_windows_process_image(&handle, identity)?;

    let mut refreshed = sysinfo::System::new();
    refresh_process_command_metadata(&mut refreshed);
    refreshed
        .process(identity.pid)
        .filter(|process| process_matches_identity(process, identity))
        .filter(|process| legacy_windows_command_matches(process.cmd(), process.cwd(), context))
        .ok_or_else(|| {
            anyhow::anyhow!("legacy managed process identity changed during migration proof")
        })?;
    ensure_windows_process_image(&handle, identity)?;
    tracing::warn!(
        pid = identity.pid.as_u32(),
        data_dir = %context.data_dir.display(),
        "LEGACY MIGRATION RECEIPT: force-stopping one exact tokenless pre-v0.8 desktop node; no graceful channel exists, chain data is preserved, and this is not a clean shutdown"
    );
    handle
        .terminate()
        .context("one-time legacy retained-handle termination failed")?;
    let deadline = Instant::now() + FORCE_STOP_TIMEOUT;
    while handle.is_live()? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out reaping the one-time legacy Windows node"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(StopSummary {
        stopped: 1,
        forced: 1,
    })
}

#[cfg(unix)]
fn legacy_rpc_port(command: &[std::ffi::OsString]) -> Option<u16> {
    unique_command_flag_value(command, "--rpc")?
        .to_str()?
        .strip_prefix("127.0.0.1:")?
        .parse()
        .ok()
}

#[cfg(unix)]
async fn wait_for_legacy_v07_shutdown_readiness(rpc_port: u16) -> anyhow::Result<()> {
    const LEGACY_READINESS_TIMEOUT: Duration = Duration::from_secs(300);
    const LEGACY_VERSIONS: [&str; 3] = ["0.7.7", "0.7.10", "0.7.11"];

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let url = format!("http://127.0.0.1:{rpc_port}/health");
    let deadline = Instant::now() + LEGACY_READINESS_TIMEOUT;
    loop {
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                if let Ok(body) = response.json::<serde_json::Value>().await {
                    if legacy_v07_health_ready(&body, &LEGACY_VERSIONS)? {
                        return Ok(());
                    }
                }
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "exact legacy arc-node never reached v0.7 readiness; its SIGTERM WAL handler may not be installed, so manual recovery is required"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(unix)]
fn legacy_v07_health_ready(
    body: &serde_json::Value,
    allowed_versions: &[&str],
) -> anyhow::Result<bool> {
    let version = body.get("version").and_then(serde_json::Value::as_str);
    let status = body.get("status").and_then(serde_json::Value::as_str);
    if status == Some("ok") && version.is_some_and(|value| allowed_versions.contains(&value)) {
        return Ok(true);
    }
    if let Some(version) = version {
        anyhow::bail!(
            "legacy argv matched but loopback health reports unexpected version {version}; refusing to signal"
        );
    }
    Ok(false)
}

#[cfg(unix)]
async fn stop_one_proven_legacy_node(
    context: &LegacyWindowsStopContext,
) -> anyhow::Result<StopSummary> {
    let managed_path = managed_binary_path();
    let managed = match managed_path.canonicalize() {
        Ok(managed) => managed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StopSummary::default());
        }
        Err(error) => {
            return Err(error).context("cannot canonicalize legacy managed arc-node executable");
        }
    };
    let mut system = sysinfo::System::new();
    refresh_process_command_metadata(&mut system);
    let matches = system
        .processes()
        .values()
        .filter_map(|process| {
            let executable = process.exe()?;
            let executable = executable
                .canonicalize()
                .unwrap_or_else(|_| executable.to_path_buf());
            (executable == managed
                && legacy_windows_command_matches(process.cmd(), process.cwd(), context))
            .then(|| {
                Some((
                    DetachedProcessIdentity {
                        pid: process.pid(),
                        start_time: process.start_time(),
                        executable,
                    },
                    legacy_rpc_port(process.cmd())?,
                ))
            })?
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Ok(StopSummary::default());
    }
    anyhow::ensure!(
        matches.len() == 1,
        "refusing tokenless legacy migration because more than one exact desktop argv match is live"
    );
    let (identity, rpc_port) = &matches[0];
    let handle = UnixLegacyProcessHandle::open(identity.pid.as_u32(), &identity.executable)
        .context("failed to retain kernel-bound identity for the legacy desktop node")?;

    // Reprove the full argv/cwd/executable grammar after retaining the kernel
    // identity. If PID reuse happened before handle acquisition, this catches
    // it; if it happens later, pidfd/audit-token signaling remains bound to the
    // old process object and cannot target the replacement.
    let mut refreshed = sysinfo::System::new();
    refresh_process_command_metadata(&mut refreshed);
    refreshed
        .process(identity.pid)
        .filter(|process| process_matches_identity(process, identity))
        .filter(|process| legacy_windows_command_matches(process.cmd(), process.cwd(), context))
        .ok_or_else(|| {
            anyhow::anyhow!("legacy managed process identity changed during retained proof")
        })?;

    wait_for_legacy_v07_shutdown_readiness(*rpc_port).await?;
    anyhow::ensure!(
        handle.is_live()?,
        "legacy node exited before the token-bound graceful signal"
    );
    tracing::warn!(
        pid = identity.pid.as_u32(),
        data_dir = %context.data_dir.display(),
        "LEGACY MIGRATION RECEIPT: sending kernel-identity-bound SIGTERM to one exact ready v0.7 desktop node; preserved legacy data will not be replayed by v0.8"
    );
    handle.signal_term()?;
    let deadline = Instant::now() + GRACEFUL_STOP_TIMEOUT;
    while handle.is_live()? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out waiting for exact legacy v0.7 node to finish its WAL handler; refusing numeric-PID force fallback"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(StopSummary {
        stopped: 1,
        forced: 0,
    })
}

#[cfg(windows)]
fn ensure_windows_process_image(
    handle: &WindowsProcessHandle,
    identity: &DetachedProcessIdentity,
) -> anyhow::Result<()> {
    let image = handle
        .image_path()
        .context("failed to query retained Windows process image")?;
    let image = image.canonicalize().unwrap_or(image);
    anyhow::ensure!(
        image == identity.executable,
        "retained Windows process image changed from the exact managed executable"
    );
    Ok(())
}

fn process_matches_identity(
    process: &sysinfo::Process,
    identity: &DetachedProcessIdentity,
) -> bool {
    process.start_time() == identity.start_time
        && process
            .exe()
            .map(|exe| exe.canonicalize().unwrap_or(exe.to_path_buf()) == identity.executable)
            .unwrap_or(false)
}

async fn wait_for_managed_processes(
    processes: &[ManagedDetachedProcess],
    timeout: Duration,
) -> anyhow::Result<Vec<usize>> {
    wait_for_managed_process_indices(
        processes,
        &(0..processes.len()).collect::<Vec<_>>(),
        timeout,
    )
    .await
}

async fn wait_for_managed_process_indices(
    processes: &[ManagedDetachedProcess],
    indices: &[usize],
    timeout: Duration,
) -> anyhow::Result<Vec<usize>> {
    let deadline = Instant::now() + timeout;
    loop {
        #[cfg(unix)]
        let mut refreshed = sysinfo::System::new();
        #[cfg(unix)]
        refreshed.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let mut live = Vec::new();
        for index in indices {
            let target = &processes[*index];
            #[cfg(unix)]
            let is_live = refreshed
                .process(target.identity.pid)
                .map(|process| process_matches_identity(process, &target.identity))
                .unwrap_or(false);
            #[cfg(windows)]
            let is_live = target.handle.is_live().with_context(|| {
                format!(
                    "failed to inspect retained process handle for detached arc-node pid {}",
                    target.identity.pid
                )
            })?;
            if is_live {
                live.push(*index);
            }
        }
        if live.is_empty() || Instant::now() >= deadline {
            return Ok(live);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn managed_binary_path() -> PathBuf {
    paths::arc_home().join("bin").join(if cfg!(windows) {
        "arc-node.exe"
    } else {
        "arc-node"
    })
}

fn which_on_path(name: &str) -> anyhow::Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| anyhow::anyhow!("no PATH"))?;
    let exe = if cfg!(windows) {
        format!("{}.exe", name)
    } else {
        name.into()
    };
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(&exe);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(anyhow::anyhow!("{} not found on PATH", name))
}

/// Resolve the configured data dir. Goes through [`paths::expand_tilde`] so
/// the default `~/.arc` lands in the user's profile on Windows instead of
/// `./.arc` relative to the GUI's CWD.
pub fn resolve_data_dir(s: &str) -> PathBuf {
    paths::expand_tilde(s)
}

/// Inspect a stale durability receipt before any updater/download is allowed
/// to replace the exact executable or signed genesis bytes it binds. A true
/// result means Start is a recovery launch and must use the installed bytes
/// already on disk; NodeManager::start performs the full binding check.
pub fn managed_shutdown_recovery_required(configured_data_dir: &str) -> anyhow::Result<bool> {
    let data_dir = resolve_data_dir(configured_data_dir);
    if !data_dir.exists() {
        return Ok(false);
    }
    arc_crypto::secret_file::desktop_shutdown_lifecycle_state(&data_dir)
        .map(|state| !state.is_clear())
        .with_context(|| {
            format!(
                "cannot inspect managed-node durability receipt/ACK under {}",
                data_dir.display()
            )
        })
}

/// Can we bind TCP on this port? Correct probe for the RPC listener.
fn tcp_available(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpListener::bind(addr).is_ok()
}

/// Can we bind UDP on this port? Correct probe for the P2P listener, which
/// is QUIC and therefore UDP.
///
/// The previous code TCP-probed the P2P port, which made the probe blind to
/// the exact failure it was written to survive: with WSL2 or Docker Desktop
/// installed, Hyper-V reserves dynamic UDP exclusion ranges that frequently
/// cover 9000-9100. UDP 9091 is then un-bindable by any user-mode process
/// while TCP 9091 binds fine — so the probe passed, arc-node got a port it
/// could not use, and the only sign was a silent fall back to an ephemeral
/// port with no inbound reachability.
fn udp_available(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    UdpSocket::bind(addr).is_ok()
}

fn choose_port_pair_with_probes(
    preferred_rpc: u16,
    preferred_p2p: u16,
    mut rpc_available: impl FnMut(u16) -> bool,
    mut p2p_available: impl FnMut(u16) -> bool,
) -> anyhow::Result<(u16, u16)> {
    // Try up to 5 offsets in +10 increments. RPC must be TCP-bindable and
    // P2P must be UDP-bindable.
    for i in 0..5 {
        let rpc = preferred_rpc.saturating_add(i * 10);
        let p2p = preferred_p2p.saturating_add(i * 10);
        if rpc_available(rpc) && p2p_available(p2p) {
            return Ok((rpc, p2p));
        }
    }
    Err(anyhow::anyhow!(
        "RPC ports {}+ (TCP) and P2P ports {}+ (UDP) are all busy across 5 fallbacks. \
         On Windows this is usually a Hyper-V/WSL2 UDP exclusion range - run \
         `netsh int ipv4 show excludedportrange protocol=udp` to check. Change the RPC port in Settings to move both.",
        preferred_rpc,
        preferred_p2p
    ))
}

fn choose_port_pair(preferred_rpc: u16, preferred_p2p: u16) -> anyhow::Result<(u16, u16)> {
    choose_port_pair_with_probes(preferred_rpc, preferred_p2p, tcp_available, udp_available)
}

impl Default for NodeManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
fn _path_sanity(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn private_directory_rebarrier_staging(path: &Path) -> PathBuf {
        path.parent().unwrap().join(format!(
            ".arc-private-directory-namespace-{}.rebarrier",
            arc_crypto::secret_file::namespace_path_digest(path).unwrap()
        ))
    }

    fn resource_test_dir(label: &str) -> PathBuf {
        // macOS exposes its temporary directory through `/var`, which is a
        // compatibility symlink to `/private/var`. Production lifecycle paths
        // deliberately reject linked ancestors, so build test paths beneath
        // the already-resolved temporary-directory prefix.
        std::env::temp_dir().canonicalize().unwrap().join(format!(
            "arc-desktop-network-resource-{label}-{}-{}",
            std::process::id(),
            uuid_like()
        ))
    }

    fn receipt_control_for_executable(
        dir: &Path,
        executable: &Path,
    ) -> (DesktopShutdownControl, PathBuf) {
        arc_crypto::secret_file::secure_private_directory_tree(dir).unwrap();
        let genesis = dir.join("receipt-genesis.toml");
        arc_crypto::secret_file::durably_publish_new_private(
            &genesis,
            b"[chain]\nname='desktop-stop-test'\n",
        )
        .unwrap();
        let mut control = prepare_desktop_shutdown_control(dir).unwrap();
        control.arm_receipt(executable, &genesis).unwrap();
        (control, genesis)
    }

    #[test]
    fn receipt_bound_executable_identity_survives_new_gui_environment_resolution() {
        let dir = resource_test_dir("persisted-executable");
        arc_crypto::secret_file::secure_private_directory_tree(&dir).unwrap();
        let _control = prepare_desktop_shutdown_control(&dir).unwrap();
        let original = std::env::current_exe().unwrap().canonicalize().unwrap();
        let persisted = persist_managed_executable_identity(&dir, &original).unwrap();
        assert_eq!(persisted, original);

        // Recovery must not recompute ARC_NODE_BIN/PATH/dev candidates from a
        // new desktop process. The private exact path selected before spawn is
        // independent of that ephemeral environment and is re-authenticated
        // against the receipt's executable path+content digests at Start.
        let recovered = load_managed_executable_identity(&dir).unwrap();
        assert_eq!(recovered, original);

        let identity = managed_executable_identity_file(&dir);
        arc_crypto::secret_file::durably_replace_private(
            &identity,
            b"arc.desktop.executable-path.v1\npath_utf8_hex=zz\n",
        )
        .unwrap();
        assert!(load_managed_executable_identity(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn testnet_resources_are_required_regular_bundle_files() {
        let dir = resource_test_dir("regular");
        std::fs::create_dir_all(&dir).unwrap();
        let seeds = dir.join("testnet-seeds.txt");
        let genesis = dir.join("genesis.toml");
        std::fs::write(&seeds, "127.0.0.1:9000\n").unwrap();
        std::fs::write(&genesis, "chain_id = \"arc-testnet\"\n").unwrap();

        let resources = TestnetResources {
            seeds_file: Some(seeds.clone()),
            genesis_file: Some(genesis.clone()),
        };
        let (validated_seeds, validated_genesis) =
            required_testnet_resources(&resources).expect("regular bundle files are valid");
        assert_eq!(validated_seeds, seeds);
        assert_eq!(validated_genesis, genesis);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_testnet_resource_fails_closed() {
        let dir = resource_test_dir("missing");
        std::fs::create_dir_all(&dir).unwrap();
        let seeds = dir.join("testnet-seeds.txt");
        std::fs::write(&seeds, "127.0.0.1:9000\n").unwrap();
        let resources = TestnetResources {
            seeds_file: Some(seeds),
            genesis_file: None,
        };

        let error = required_testnet_resources(&resources).unwrap_err();
        assert!(error.to_string().contains("genesis.toml is missing"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stable_network_identity_survives_bundle_mount_path_change_while_fenced() {
        let root = resource_test_dir("stable-appimage-recovery");
        let data = root.join("data-v3");
        let mount_a = root.join(".mount_A/resources");
        let mount_b = root.join(".mount_B/resources");
        std::fs::create_dir_all(&mount_a).unwrap();
        std::fs::create_dir_all(&mount_b).unwrap();
        let lifecycle_lock = acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap();
        let seeds_a = mount_a.join("testnet-seeds.txt");
        let genesis_a = mount_a.join("genesis.toml");
        std::fs::write(&seeds_a, b"seed-A\n").unwrap();
        std::fs::write(&genesis_a, b"[chain]\nname='A'\n").unwrap();
        let first = materialize_stable_testnet_resources(
            &data,
            &TestnetResources {
                seeds_file: Some(seeds_a),
                genesis_file: Some(genesis_a),
            },
            &lifecycle_lock,
        )
        .unwrap();
        let stable_genesis = first.genesis_file.clone().unwrap();
        let old_bytes = std::fs::read(&stable_genesis).unwrap();

        let mut control = prepare_desktop_shutdown_control(&data).unwrap();
        control
            .arm_receipt(&std::env::current_exe().unwrap(), &stable_genesis)
            .unwrap();
        std::fs::remove_dir_all(root.join(".mount_A")).unwrap();
        std::fs::write(mount_b.join("testnet-seeds.txt"), b"seed-B\n").unwrap();
        std::fs::write(mount_b.join("genesis.toml"), b"[chain]\nname='B'\n").unwrap();

        let recovered = materialize_stable_testnet_resources(
            &data,
            &TestnetResources {
                seeds_file: Some(mount_b.join("testnet-seeds.txt")),
                genesis_file: Some(mount_b.join("genesis.toml")),
            },
            &lifecycle_lock,
        )
        .unwrap();
        assert_eq!(recovered.genesis_file.unwrap(), stable_genesis);
        assert_eq!(std::fs::read(stable_genesis).unwrap(), old_bytes);
        drop(lifecycle_lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_lifecycle_lock_serializes_desktop_processes() {
        let root = resource_test_dir("lifecycle-lock");
        arc_crypto::secret_file::secure_private_directory_tree(&root).unwrap();
        let configured = root.join("data-v3").to_string_lossy().into_owned();
        let first = acquire_managed_lifecycle_lock(&configured).unwrap();
        let first_nonce = *first.session_nonce().unwrap();
        let error = match acquire_managed_lifecycle_lock(&configured) {
            Ok(_) => panic!("a second desktop must not acquire the managed lifecycle"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("another ARC desktop currently owns the managed node lifecycle"),
            "an established namespace must reach the lifecycle-lock check: {error}"
        );
        drop(first);
        let second = acquire_managed_lifecycle_lock(&configured).unwrap();
        assert_ne!(first_nonce, *second.session_nonce().unwrap());
        let data = PathBuf::from(&configured);
        assert!(
            !has_exact_lifecycle_namespace_proof(&data, &first_nonce).unwrap(),
            "a completed prior desktop session must not authorize a delayed child"
        );
        assert!(
            has_exact_lifecycle_namespace_proof(&data, second.session_nonce().unwrap()).unwrap()
        );
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provisional_orphan_lease_cannot_launch_and_prepares_after_node_lock_reaps() {
        let root = resource_test_dir("lifecycle-orphan-transition");
        arc_crypto::secret_file::secure_private_directory_tree(&root).unwrap();
        let data = root.join("data-v3");
        drop(acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap());

        let node_lock_path = data.join(".arc-node.lock");
        arc_crypto::secret_file::durably_publish_new_private(
            &node_lock_path,
            b"schema=arc.node.data-lock.v1\npid=1\n",
        )
        .unwrap();
        let node_lock = arc_crypto::secret_file::open_private_read_write(&node_lock_path).unwrap();
        node_lock.try_lock_exclusive().unwrap();

        let provisional =
            acquire_managed_lifecycle_lock_for_reconciliation(&data.to_string_lossy()).unwrap();
        let error = provisional
            .ensure_data_dir(&data.canonicalize().unwrap())
            .expect_err("a provisional orphan lease must not authorize launch or mutation");
        assert!(error.to_string().contains("durability barrier"));
        fs2::FileExt::unlock(&node_lock).unwrap();
        drop(node_lock);

        let prepared = refresh_managed_lifecycle_namespace(provisional).unwrap();
        prepared
            .ensure_data_dir(&data.canonicalize().unwrap())
            .unwrap();
        drop(prepared);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_lifecycle_lock_migrates_v1_proof_and_lock_residue() {
        let root = resource_test_dir("lifecycle-lock-v1-proof-residue");
        let data = root.join("data-v3");
        let control = data.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
        arc_crypto::secret_file::secure_private_directory_tree(&control).unwrap();
        let retired_v1_proof = control.join("lifecycle.namespace-proof");
        arc_crypto::secret_file::durably_publish_new_private(
            &retired_v1_proof,
            b"arc.desktop.lifecycle-namespace.v1\n",
        )
        .unwrap();
        arc_crypto::secret_file::durably_publish_new_private(
            &control.join(DESKTOP_LIFECYCLE_LOCK_FILE_NAME),
            b"arc.desktop.lifecycle-lock.v1\n",
        )
        .unwrap();

        let lock = acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap();
        assert!(has_exact_lifecycle_namespace_proof(&data, lock.session_nonce().unwrap()).unwrap());
        assert_eq!(
            std::fs::read(&retired_v1_proof).unwrap(),
            b"arc.desktop.lifecycle-namespace.retired-by-v3\n"
        );

        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_lifecycle_lock_rejects_invalid_namespace_proof() {
        let root = resource_test_dir("lifecycle-lock-invalid-proof");
        let data = root.join("data-v3");
        let control = data.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
        arc_crypto::secret_file::secure_private_directory_tree(&control).unwrap();
        let proof_path =
            arc_crypto::secret_file::desktop_lifecycle_namespace_proof_path(&data).unwrap();
        arc_crypto::secret_file::durably_publish_new_private(
            &proof_path,
            b"arc.desktop.lifecycle-namespace.v3\n",
        )
        .unwrap();

        let error = match acquire_managed_lifecycle_lock(&data.to_string_lossy()) {
            Ok(_) => panic!("an invalid namespace proof must fail closed"),
            Err(error) => error,
        };
        let error_chain = format!("{error:#}");
        assert!(
            error_chain.contains("namespace proof has invalid contents"),
            "unexpected error: {error_chain}"
        );
        assert!(!control.join(DESKTOP_LIFECYCLE_LOCK_FILE_NAME).exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_launch_prepares_only_the_managed_arc_root() {
        let home = resource_test_dir("first-launch-root");
        arc_crypto::secret_file::secure_private_directory_tree(&home).unwrap();
        let managed_root = home.join(".arc");
        let data = managed_root.join("data-v3");

        let root_guard = prepare_first_launch_managed_root_at(&data, &managed_root)
            .unwrap()
            .expect("first launch must retain the managed-root namespace guard");
        assert!(managed_root.is_dir());
        let lock = acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap();
        assert!(data.is_dir());

        drop(lock);
        drop(root_guard);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn first_launch_rebarriers_a_visible_managed_root_before_creating_its_child() {
        let home = resource_test_dir("first-launch-visible-root-retry");
        arc_crypto::secret_file::secure_private_directory_tree(&home).unwrap();
        let managed_root = home.join(".arc");
        // A configured case variant still names the exact managed namespaces
        // on Windows and therefore must participate in the outer-root fence.
        let data = home.join(".ARC").join("DATA-V3");

        // Model the observable state after MoveFileExW made the first root
        // publication visible but reported a late write-through failure. A
        // retry must not treat mere visibility as proof that the parent name
        // is durable, and must retain its namespace guard through child
        // creation.
        arc_crypto::secret_file::secure_private_directory(&managed_root).unwrap();
        let root_guard = prepare_first_launch_managed_root_at(&data, &managed_root)
            .unwrap()
            .expect("visible root without data must re-enter the namespace protocol");
        assert!(arc_crypto::secret_file::same_private_directory_namespace(
            root_guard.target(),
            &managed_root,
        )
        .unwrap());
        assert!(managed_root.is_dir());
        assert!(!private_directory_rebarrier_staging(&managed_root).exists());

        let lock = acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap();
        assert!(data.is_dir());
        drop(lock);
        drop(root_guard);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn custom_data_parent_must_already_exist() {
        let root = resource_test_dir("custom-parent-missing");
        let data = root.join("operator-owned").join("data-v3");

        let error = match acquire_managed_lifecycle_lock(&data.to_string_lossy()) {
            Ok(_) => panic!("custom missing parent must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("must already exist"));
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn custom_symlinked_parent_is_rejected_before_external_sidecar_mutation() {
        use std::os::unix::fs::symlink;

        let root = resource_test_dir("custom-parent-link-no-mutation");
        let outside = root.join("outside");
        arc_crypto::secret_file::secure_private_directory_tree(&outside).unwrap();
        let linked_parent = root.join("operator-owned");
        symlink(&outside, &linked_parent).unwrap();
        let data = linked_parent.join("data-v3");

        let error = match acquire_managed_lifecycle_lock(&data.to_string_lossy()) {
            Ok(_) => panic!("a linked custom parent must fail closed"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("linked/non-directory"),
            "{error}"
        );
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "validation must not create a namespace-lock sidecar in the symlink target"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn managed_lifecycle_lock_restores_staged_data_directory() {
        let root = resource_test_dir("lifecycle-lock-staged-data");
        arc_crypto::secret_file::secure_private_directory_tree(&root).unwrap();
        let data = root.canonicalize().unwrap().join("data-v3");
        let staged = private_directory_rebarrier_staging(&data);
        arc_crypto::secret_file::secure_private_directory_tree(&staged).unwrap();

        let lock = acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap();
        assert!(data.is_dir());
        assert!(!staged.exists());
        assert!(data
            .join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)
            .join(DESKTOP_LIFECYCLE_LOCK_FILE_NAME)
            .is_file());

        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn managed_lifecycle_lock_restores_staged_control_directory() {
        let root = resource_test_dir("lifecycle-lock-staged-control");
        let data = root.join("data-v3");
        arc_crypto::secret_file::secure_private_directory_tree(&data).unwrap();
        let data = data.canonicalize().unwrap();
        let control = data.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
        let staged = private_directory_rebarrier_staging(&control);
        arc_crypto::secret_file::secure_private_directory_tree(&staged).unwrap();

        let lock = acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap();
        assert!(control.is_dir());
        assert!(!staged.exists());
        assert!(control.join(DESKTOP_LIFECYCLE_LOCK_FILE_NAME).is_file());

        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn managed_lifecycle_lock_accepts_existing_case_variant_namespace() {
        let root = resource_test_dir("lifecycle-lock-case-variant");
        let data = root.join("data-v3");
        arc_crypto::secret_file::secure_private_directory_tree(&data).unwrap();
        let canonical_data = data.canonicalize().unwrap();
        let configured = root.join("DATA-V3");

        let lock = acquire_managed_lifecycle_lock(&configured.to_string_lossy()).unwrap();
        assert_eq!(lock.data_dir, canonical_data);
        assert!(canonical_data
            .join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)
            .join(DESKTOP_LIFECYCLE_LOCK_FILE_NAME)
            .is_file());

        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn stable_network_materialization_restores_staged_directory() {
        let root = resource_test_dir("stable-network-staged-directory");
        arc_crypto::secret_file::secure_private_directory_tree(&root).unwrap();
        let data = root.join("data-v3");
        let lifecycle_lock = acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap();
        let data = data.canonicalize().unwrap();
        let network = data.join(DESKTOP_NETWORK_IDENTITY_DIR_NAME);
        let staged = private_directory_rebarrier_staging(&network);
        arc_crypto::secret_file::secure_private_directory_tree(&staged).unwrap();

        let bundle = root.join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        let seeds = bundle.join("testnet-seeds.txt");
        let genesis = bundle.join("genesis.toml");
        std::fs::write(&seeds, b"seed.example:9001\n").unwrap();
        std::fs::write(&genesis, b"[chain]\nname='arc-testnet'\n").unwrap();
        let stable = materialize_stable_testnet_resources(
            &data,
            &TestnetResources {
                seeds_file: Some(seeds),
                genesis_file: Some(genesis),
            },
            &lifecycle_lock,
        )
        .unwrap();

        assert!(network.is_dir());
        assert!(!staged.exists());
        assert_eq!(
            std::fs::read(stable.seeds_file.unwrap()).unwrap(),
            b"seed.example:9001\n"
        );
        assert_eq!(
            std::fs::read(stable.genesis_file.unwrap()).unwrap(),
            b"[chain]\nname='arc-testnet'\n"
        );

        drop(lifecycle_lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stable_network_materialization_reuses_only_bounded_destination_staging() {
        let root = resource_test_dir("stable-network-bounded-staging");
        arc_crypto::secret_file::secure_private_directory_tree(&root).unwrap();
        let data = root.join("data-v3");
        let lifecycle_lock = acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap();
        let data = data.canonicalize().unwrap();
        let network = data.join(DESKTOP_NETWORK_IDENTITY_DIR_NAME);
        arc_crypto::secret_file::secure_private_directory_tree(&network).unwrap();
        let stable_seeds = network.join(DESKTOP_STABLE_SEEDS_FILE_NAME);
        let stable_genesis = network.join(DESKTOP_STABLE_GENESIS_FILE_NAME);
        let seeds_staging = stable_network_replacement_staging(&stable_seeds);
        let genesis_staging = stable_network_replacement_staging(&stable_genesis);
        for staging in [&seeds_staging, &genesis_staging] {
            let mut file = arc_crypto::secret_file::create_new_private(staging).unwrap();
            file.write_all(b"partial crash residue").unwrap();
            file.sync_all().unwrap();
        }
        let unrelated = network.join(".testnet-seeds.txt.operator.replace");
        arc_crypto::secret_file::durably_publish_new_private(&unrelated, b"operator-owned\n")
            .unwrap();

        let bundle = root.join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        let seeds = bundle.join("testnet-seeds.txt");
        let genesis = bundle.join("genesis.toml");
        std::fs::write(&seeds, b"seed.example:9001\n").unwrap();
        std::fs::write(&genesis, b"[chain]\nname='arc-testnet'\n").unwrap();
        let stable = materialize_stable_testnet_resources(
            &data,
            &TestnetResources {
                seeds_file: Some(seeds),
                genesis_file: Some(genesis),
            },
            &lifecycle_lock,
        )
        .unwrap();

        assert!(!seeds_staging.exists());
        assert!(!genesis_staging.exists());
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"operator-owned\n");
        assert_eq!(
            std::fs::read(stable.seeds_file.unwrap()).unwrap(),
            b"seed.example:9001\n"
        );
        assert_eq!(
            std::fs::read(stable.genesis_file.unwrap()).unwrap(),
            b"[chain]\nname='arc-testnet'\n"
        );

        drop(lifecycle_lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn updater_fence_survives_ipc_window_and_failed_install_abort_releases_it() {
        let root = resource_test_dir("updater-lifecycle-window");
        arc_crypto::secret_file::secure_private_directory_tree(&root).unwrap();
        let configured = root.join("data-v3").to_string_lossy().into_owned();
        let mut installer_gui = NodeManager::new();
        installer_gui
            .configure_managed_data_dir(&configured)
            .unwrap();
        let mut competing_gui = NodeManager::new();
        competing_gui
            .configure_managed_data_dir(&configured)
            .unwrap();

        installer_gui.prepare_update_relaunch().await.unwrap();
        assert!(installer_gui.update_lifecycle_lock.is_some());
        assert!(
            acquire_managed_lifecycle_lock(&configured).is_err(),
            "a second GUI must not start during download/install after prepare IPC returns"
        );
        assert!(
            installer_gui.prepare_update_relaunch().await.is_err(),
            "an already-held fence must not be reported as a fresh successful prepare after a failed abort/reconciliation"
        );
        assert!(
            competing_gui.prepare_update_relaunch().await.is_err(),
            "a second NodeManager must not enter its updater transaction during the installer window"
        );
        assert!(
            installer_gui.stop().await.is_err(),
            "ordinary lifecycle commands must not accidentally release the updater fence"
        );

        // A rejected installer calls the explicit native abort. Native code
        // re-scans for detached writers and revalidates the receipt before it
        // releases the guard, after which a second GUI may own the lifecycle.
        installer_gui.abort_update_relaunch().await.unwrap();
        assert!(installer_gui.update_lifecycle_lock.is_none());
        assert!(installer_gui.child.is_none());
        competing_gui.prepare_update_relaunch().await.unwrap();
        assert!(competing_gui.update_lifecycle_lock.is_some());
        competing_gui.abort_update_relaunch().await.unwrap();
        assert!(competing_gui.child.is_none());
        drop(acquire_managed_lifecycle_lock(&configured).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn updater_handoff_is_a_one_way_native_fence() {
        let root = resource_test_dir("updater-one-way-handoff");
        arc_crypto::secret_file::secure_private_directory_tree(&root).unwrap();
        let configured = root.join("data-v3").to_string_lossy().into_owned();
        let mut manager = NodeManager::new();
        manager.configure_managed_data_dir(&configured).unwrap();

        manager.prepare_update_relaunch().await.unwrap();
        manager.begin_update_handoff().unwrap();
        let error = manager
            .abort_update_relaunch()
            .await
            .expect_err("post-handoff abort must never resume the old node");
        assert!(error
            .to_string()
            .contains("after updater handoff has begun"));
        assert!(manager.update_lifecycle_lock.is_some());
        assert!(manager.child.is_none());

        // Process exit is the only release after handoff.
        drop(manager);
        drop(acquire_managed_lifecycle_lock(&configured).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn updater_resume_plan_binds_exact_config_identity_resources_and_binary_bytes() {
        let root = resource_test_dir("updater-exact-resume-plan");
        let data = root.join("data-v3");
        arc_crypto::secret_file::secure_private_directory_tree(&data).unwrap();
        let configured = data.to_string_lossy().into_owned();
        let lock = acquire_managed_lifecycle_lock(&configured).unwrap();
        let binary = root.join("arc-node");
        let validator_key = root.join("validator.json");
        let seeds = root.join("testnet-seeds.txt");
        let genesis = root.join("genesis.toml");
        std::fs::write(&binary, b"exact executable bytes").unwrap();
        std::fs::write(&validator_key, b"exact validator identity").unwrap();
        std::fs::write(&seeds, b"seed.example:9001\n").unwrap();
        std::fs::write(&genesis, b"[chain]\nname='arc-testnet'\n").unwrap();
        let config = NodeConfig {
            data_dir: configured,
            rpc_port: 19_090,
            p2p_port: 19_091,
            worker_threads: Some(3),
            ..NodeConfig::default()
        };
        let plan = ManagedLaunchPlan::capture(
            &config,
            &validator_key,
            &binary,
            &TestnetResources {
                seeds_file: Some(seeds),
                genesis_file: Some(genesis),
            },
        )
        .unwrap();
        let (verified_key, verified_binary, verified_resources) = plan.verify(&lock).unwrap();
        assert_eq!(verified_key, validator_key.canonicalize().unwrap());
        assert_eq!(verified_binary, binary.canonicalize().unwrap());
        assert_eq!(plan.config.rpc_port, 19_090);
        assert_eq!(plan.config.p2p_port, 19_091);
        assert_eq!(plan.config.worker_threads, Some(3));
        assert!(required_testnet_resources(&verified_resources).is_ok());

        std::fs::write(&binary, b"different executable bytes").unwrap();
        assert!(plan
            .verify(&lock)
            .unwrap_err()
            .to_string()
            .contains("executable bytes changed"));
        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    fn updater_resume_fixture(label: &str) -> (PathBuf, String, NodeManager, ManagedLaunchPlan) {
        use std::os::unix::fs::PermissionsExt as _;

        let root = resource_test_dir(label);
        let data = root.join("data-v3");
        arc_crypto::secret_file::secure_private_directory_tree(&data).unwrap();
        let configured = data.to_string_lossy().into_owned();
        let binary = root.join("arc-node-fixture");
        let validator_key = root.join("validator.json");
        let seeds = root.join("testnet-seeds.txt");
        let genesis = root.join("genesis.toml");
        std::fs::write(
            &binary,
            b"#!/bin/sh\nif [ \"${1-}\" = --help ]; then\n  printf '%s\\n' '--validator-key-file --desktop-shutdown-token-file --desktop-lifecycle-nonce'\n  exit 0\nfi\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&validator_key, b"exact validator identity").unwrap();
        std::fs::write(&seeds, b"seed.example:9001\n").unwrap();
        std::fs::write(&genesis, b"[chain]\nname='arc-testnet'\n").unwrap();
        let config = NodeConfig {
            role: "observer".into(),
            model_path: None,
            data_dir: configured.clone(),
            rpc_port: 29_090,
            p2p_port: 29_091,
            worker_threads: None,
            ..NodeConfig::default()
        };
        let resources = TestnetResources {
            seeds_file: Some(seeds),
            genesis_file: Some(genesis),
        };
        let plan =
            ManagedLaunchPlan::capture(&config, &validator_key, &binary, &resources).unwrap();
        let mut manager = NodeManager::new();
        manager.configure_managed_data_dir(&configured).unwrap();
        manager.update_lifecycle_lock = Some(acquire_managed_lifecycle_lock(&configured).unwrap());
        manager.update_restart_plan = Some(plan.clone());
        (root, configured, manager, plan)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_download_abort_atomically_resumes_the_exact_pre_update_node() {
        let (root, configured, mut manager, _plan) =
            updater_resume_fixture("updater-atomic-resume");

        manager.abort_update_relaunch().await.unwrap();
        assert!(manager.child.is_some());
        assert!(manager.lifecycle_lock.is_some());
        assert!(manager.update_lifecycle_lock.is_none());
        assert!(manager.update_restart_plan.is_none());
        assert!(
            acquire_managed_lifecycle_lock(&configured).is_err(),
            "the same lifecycle guard must remain continuously owned by the resumed child"
        );

        let mut child = manager.child.take().unwrap();
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        manager.shutdown_control = None;
        manager.lifecycle_lock = None;
        manager.active_launch_plan = None;
        drop(manager);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_download_abort_retains_fence_when_exact_resume_identity_changed() {
        let (root, configured, mut manager, plan) =
            updater_resume_fixture("updater-resume-failure");
        std::fs::write(&plan.binary.path, b"tampered while download was in flight").unwrap();

        let error = manager
            .abort_update_relaunch()
            .await
            .expect_err("changed executable must block automatic resume");
        let detail = error.to_string();
        assert!(detail.contains("node remains stopped"), "{detail}");
        assert!(detail.contains("manual restart is required"), "{detail}");
        assert!(manager.child.is_none());
        assert!(manager.lifecycle_lock.is_none());
        assert!(manager.update_lifecycle_lock.is_some());
        assert!(manager.update_restart_plan.is_some());
        assert!(
            acquire_managed_lifecycle_lock(&configured).is_err(),
            "resume failure must retain the updater lifecycle fence"
        );

        drop(manager);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn local_mutation_guard_spans_exact_peer_cache_delete() {
        let root = resource_test_dir("peer-cache-mutation-window");
        arc_crypto::secret_file::secure_private_directory_tree(&root).unwrap();
        let data = root.join("data-v3");
        let configured = data.to_string_lossy().into_owned();
        let peers = data.join("known_peers.json");
        let mut reset_gui = NodeManager::new();
        reset_gui.configure_managed_data_dir(&configured).unwrap();

        let lifecycle_lock = reset_gui.stop_for_local_mutation().await.unwrap();
        std::fs::write(&peers, b"[]\n").unwrap();
        assert!(
            acquire_managed_lifecycle_lock(&configured).is_err(),
            "a competing GUI must stay fenced before the peer-cache mutation"
        );
        std::fs::remove_file(&peers).unwrap();
        assert!(
            acquire_managed_lifecycle_lock(&configured).is_err(),
            "the same lifecycle guard must remain held after the exact delete and until restart/start accepts it"
        );
        drop(lifecycle_lock);
        drop(acquire_managed_lifecycle_lock(&configured).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stable_network_directory_symlink_is_rejected_without_external_write() {
        use std::os::unix::fs::symlink;

        let root = resource_test_dir("stable-network-symlink");
        let data = root.join("data-v3");
        let outside = root.join("outside");
        let bundle = root.join("bundle");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&bundle).unwrap();
        let lifecycle_lock = acquire_managed_lifecycle_lock(&data.to_string_lossy()).unwrap();
        symlink(&outside, data.join(DESKTOP_NETWORK_IDENTITY_DIR_NAME)).unwrap();
        let seeds = bundle.join("testnet-seeds.txt");
        let genesis = bundle.join("genesis.toml");
        std::fs::write(&seeds, b"seed\n").unwrap();
        std::fs::write(&genesis, b"[chain]\n").unwrap();
        assert!(materialize_stable_testnet_resources(
            &data,
            &TestnetResources {
                seeds_file: Some(seeds),
                genesis_file: Some(genesis),
            },
            &lifecycle_lock,
        )
        .is_err());
        assert!(!outside.join(DESKTOP_STABLE_GENESIS_FILE_NAME).exists());
        drop(lifecycle_lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_testnet_resource_fails_closed() {
        use std::os::unix::fs::symlink;

        let dir = resource_test_dir("symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let actual_seeds = dir.join("actual-seeds.txt");
        let seeds_link = dir.join("testnet-seeds.txt");
        let genesis = dir.join("genesis.toml");
        std::fs::write(&actual_seeds, "127.0.0.1:9000\n").unwrap();
        symlink(&actual_seeds, &seeds_link).unwrap();
        std::fs::write(&genesis, "chain_id = \"arc-testnet\"\n").unwrap();
        let resources = TestnetResources {
            seeds_file: Some(seeds_link),
            genesis_file: Some(genesis),
        };

        let error = required_testnet_resources(&resources).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn managed_binary_is_under_home_bin() {
        let p = managed_binary_path();
        let s = p.to_string_lossy();
        assert!(s.contains(".arc"), "path should contain .arc: {s}");
        assert!(s.ends_with("arc-node") || s.ends_with("arc-node.exe"));
    }

    #[test]
    fn resolve_data_dir_expands_tilde() {
        let p = resolve_data_dir("~/foo");
        assert!(!p.starts_with("~"));
    }

    #[test]
    fn port_pair_probes_fallback() {
        // Exercise selection independently of the host ephemeral-port range
        // and other parallel tests. Binding port 0 and then releasing one of
        // the results left this regression racing every socket user on the
        // machine, and high ephemeral allocations could also collapse several
        // saturating +10 candidates onto 65535.
        let preferred_rpc = 31_000;
        let preferred_p2p = 31_001;
        let mut rpc_probes = Vec::new();
        let mut p2p_probes = Vec::new();
        let result = choose_port_pair_with_probes(
            preferred_rpc,
            preferred_p2p,
            |port| {
                rpc_probes.push(port);
                port != preferred_rpc
            },
            |port| {
                p2p_probes.push(port);
                true
            },
        )
        .expect("the deterministic fallback probe should find its second pair");
        assert_eq!(result, (preferred_rpc + 10, preferred_p2p + 10));
        assert_eq!(rpc_probes, [preferred_rpc, preferred_rpc + 10]);
        // Short-circuiting must not probe UDP for a pair whose RPC port is
        // already unavailable.
        assert_eq!(p2p_probes, [preferred_p2p + 10]);
    }

    /// The P2P listener is QUIC/UDP. A TCP bind on the same number proves
    /// nothing about it, which is what made the old probe blind to Hyper-V's
    /// UDP exclusion ranges.
    #[test]
    fn p2p_probe_is_udp_not_tcp() {
        // Windows may allocate an ephemeral UDP port from a range that is
        // excluded for TCP. Select the port through TCP first, then hold UDP
        // on the same number so the fixture does not depend on the two
        // protocols sharing an ephemeral-port allocation policy.
        let (udp, busy_udp) = (0..128)
            .find_map(|_| {
                let tcp = TcpListener::bind("127.0.0.1:0").ok()?;
                let addr = tcp.local_addr().ok()?;
                let udp = UdpSocket::bind(addr).ok()?;
                drop(tcp);
                tcp_available(addr.port()).then_some((udp, addr.port()))
            })
            .expect("should find a UDP-held port that remains TCP-available");
        assert_eq!(udp.local_addr().unwrap().port(), busy_udp);
        assert!(!udp_available(busy_udp), "held UDP port must read as busy");
        // The successful selection above observed tcp_available(port) while
        // UDP held the same number, independently proving the exact blind spot
        // in the old TCP-based P2P probe.
    }

    #[test]
    fn choose_port_pair_skips_udp_busy_p2p() {
        let preferred_rpc = 32_000;
        let preferred_p2p = 32_001;
        let mut rpc_probes = Vec::new();
        let mut p2p_probes = Vec::new();
        let selected = choose_port_pair_with_probes(
            preferred_rpc,
            preferred_p2p,
            |port| {
                rpc_probes.push(port);
                true
            },
            |port| {
                p2p_probes.push(port);
                port != preferred_p2p
            },
        );
        assert_eq!(
            selected.unwrap(),
            (preferred_rpc + 10, preferred_p2p + 10),
            "must not hand back a UDP-busy p2p port"
        );
        assert_eq!(rpc_probes, [preferred_rpc, preferred_rpc + 10]);
        assert_eq!(p2p_probes, [preferred_p2p, preferred_p2p + 10]);
    }

    /// `--threads` must not report as supported just because
    /// `--bench-rayon-threads` appears in the same help text.
    #[test]
    fn flag_probe_requires_whole_token_match() {
        // `/bin/echo` is not arc-node, so the probe returns false rather
        // than panicking - the safe default for an unknown binary.
        assert!(!binary_supports_flag(
            Path::new("/nonexistent/arc-node"),
            "--threads"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn flag_probe_cache_tracks_same_path_binary_replacement() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = resource_test_dir("flag-cache-replacement");
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("arc-node");
        let flag = "--desktop-shutdown-token-file";
        let old_help = format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", "x".repeat(flag.len()));
        let new_help = format!("#!/bin/sh\nprintf '%s\\n' '{flag}'\n");
        assert_eq!(old_help.len(), new_help.len());
        std::fs::write(&binary, old_help).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!binary_supports_flag(&binary, flag));

        let replacement = dir.join("arc-node.new");
        std::fs::write(&replacement, new_help).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(&replacement, &binary).unwrap();
        assert!(binary_supports_flag(&binary, flag));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn production_community_origins_are_six_distinct_https_origins() {
        let unique: std::collections::HashSet<_> = crate::rpc_client::PRODUCTION_RPC_ORIGINS
            .into_iter()
            .collect();
        assert_eq!(unique.len(), 6);
        for origin in unique {
            let host = origin.strip_prefix("https://").unwrap();
            assert!(!origin.contains(":9090"));
            assert!(!host.contains('/'));
            let ip: std::net::Ipv4Addr = host.parse().expect("origin host must be literal IPv4");
            let octets = ip.octets();
            assert!(
                !ip.is_private()
                    && !ip.is_loopback()
                    && !ip.is_link_local()
                    && !ip.is_unspecified()
                    && !ip.is_broadcast()
                    && !ip.is_multicast()
                    && !ip.is_documentation()
                    && octets[0] != 0
                    && octets[0] < 240
                    && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
                    && !(octets[0] == 198 && (18..=19).contains(&octets[1])),
                "origin must be a public IPv4 literal: {ip}"
            );
        }
    }

    #[test]
    fn data_dir_lands_under_home_not_cwd() {
        // The Windows bug in miniature: `~/.arc` must never resolve to a
        // relative `./.arc`.
        let p = resolve_data_dir("~/.arc");
        assert!(p.is_absolute() || p.starts_with(paths::home_dir()));
        assert!(!p.starts_with("./"));
    }

    #[test]
    fn desktop_stop_budget_matches_the_managed_node_durability_window() {
        assert_eq!(GRACEFUL_STOP_TIMEOUT_SECS, 4_420);
        assert_eq!(GRACEFUL_STOP_TIMEOUT, Duration::from_secs(4_420));
    }

    #[test]
    fn desktop_shutdown_request_uses_a_persistent_private_file_capability() {
        let dir = resource_test_dir("shutdown-capability");
        std::fs::create_dir_all(&dir).unwrap();
        let (control, _genesis) =
            receipt_control_for_executable(&dir, &std::env::current_exe().unwrap());
        assert_eq!(
            control.token_file.file_name().unwrap(),
            DESKTOP_SHUTDOWN_TOKEN_FILE_NAME
        );
        assert_eq!(hex::decode(control.token.as_str()).unwrap().len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&control.token_file)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }

        write_desktop_shutdown_request(&control, 4_242).unwrap();
        // Idempotent concurrent/repeated publication must converge on the
        // exact complete payload and never replace it.
        write_desktop_shutdown_request(&control, 4_242).unwrap();
        let request = read_private_shutdown_request(&control.request_file).unwrap();
        assert_eq!(
            request.as_str(),
            format!(
                "{DESKTOP_SHUTDOWN_REQUEST_SCHEMA}\npid=4242\ntoken={}\nnonce={}\n",
                control.token.as_str(),
                hex::encode(control.receipt_nonce.unwrap()),
            )
        );

        let command = vec![
            std::ffi::OsString::from("arc-node"),
            std::ffi::OsString::from("--data-dir"),
            dir.canonicalize().unwrap().into_os_string(),
            std::ffi::OsString::from("--desktop-shutdown-token-file"),
            control.token_file.clone().into_os_string(),
        ];
        let recovered = desktop_shutdown_control_from_command(&command)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.request_file, control.request_file);
        assert_eq!(recovered.token.as_str(), control.token.as_str());

        // A second desktop reaching pre-spawn preparation must not erase the
        // first node's live request while that node owns the data-dir lock.
        let _ = prepare_desktop_shutdown_control(&dir).unwrap();
        assert!(control.request_file.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn detached_shutdown_requires_exact_data_dir_bound_control_argv() {
        let dir = resource_test_dir("shutdown-command-binding");
        let other = resource_test_dir("shutdown-command-other");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let control = prepare_desktop_shutdown_control(&dir).unwrap();

        let manual = vec![
            std::ffi::OsString::from("arc-node"),
            std::ffi::OsString::from("--data-dir"),
            dir.canonicalize().unwrap().into_os_string(),
        ];
        assert!(desktop_shutdown_control_from_command(&manual)
            .unwrap()
            .is_none());

        let mismatched = vec![
            std::ffi::OsString::from("arc-node"),
            std::ffi::OsString::from("--data-dir"),
            other.canonicalize().unwrap().into_os_string(),
            std::ffi::OsString::from("--desktop-shutdown-token-file"),
            control.token_file.clone().into_os_string(),
        ];
        assert!(desktop_shutdown_control_from_command(&mismatched).is_err());

        let exact = vec![
            std::ffi::OsString::from("arc-node"),
            std::ffi::OsString::from("--data-dir"),
            dir.canonicalize().unwrap().into_os_string(),
            std::ffi::OsString::from("--desktop-shutdown-token-file"),
            control.token_file.clone().into_os_string(),
        ];
        assert!(desktop_shutdown_control_from_command(&exact)
            .unwrap()
            .is_some());
        let nested = dir.join("path-form-fixture");
        std::fs::create_dir(&nested).unwrap();
        let noncanonical_data = nested.join("..");
        let path_form = vec![
            std::ffi::OsString::from("arc-node"),
            std::ffi::OsString::from("--data-dir"),
            noncanonical_data.into_os_string(),
            std::ffi::OsString::from("--desktop-shutdown-token-file"),
            control.token_file.clone().into_os_string(),
        ];
        assert!(
            desktop_shutdown_control_from_command(&path_form)
                .unwrap()
                .is_some(),
            "normal/noncanonical data argv and canonical token argv must recover one control"
        );
        let mut duplicate_data = exact.clone();
        duplicate_data.extend([
            std::ffi::OsString::from("--data-dir"),
            dir.canonicalize().unwrap().into_os_string(),
        ]);
        assert!(desktop_shutdown_control_from_command(&duplicate_data).is_err());
        let mut duplicate_token = exact.clone();
        duplicate_token.extend([
            std::ffi::OsString::from("--desktop-shutdown-token-file"),
            control.token_file.clone().into_os_string(),
        ]);
        assert!(desktop_shutdown_control_from_command(&duplicate_token).is_err());
        std::fs::remove_file(&control.token_file).unwrap();
        assert!(
            desktop_shutdown_control_from_command(&exact).is_err(),
            "an exact managed argv with a deleted token must block Stop/update"
        );
        let mut corrupt = arc_crypto::secret_file::create_new_private(&control.token_file).unwrap();
        corrupt.write_all(b"not-a-token\n").unwrap();
        corrupt.sync_all().unwrap();
        drop(corrupt);
        assert!(
            desktop_shutdown_control_from_command(&exact).is_err(),
            "an exact managed argv with a corrupt token must block Stop/update"
        );
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn tokenless_legacy_migration_requires_the_exact_desktop_argv_grammar() {
        let dir = resource_test_dir("legacy-argv");
        let data = dir.join("legacy-v07-data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("state.wal"), b"preserved-v07-state").unwrap();
        let seeds = dir.join("testnet-seeds.txt");
        let genesis = dir.join("genesis.toml");
        std::fs::write(&seeds, "127.0.0.1:9000\n").unwrap();
        std::fs::write(&genesis, "chain_id='arc-testnet'\n").unwrap();
        let validator_seed = "test recovery phrase stays private";
        let old_config = NodeConfig {
            data_dir: data.to_string_lossy().into_owned(),
            ..NodeConfig::default()
        };
        let mut store = crate::store::Store {
            identity: Some(crate::types::Identity {
                address: "11".repeat(32),
                public_key: format!("0x{}", "22".repeat(32)),
                seed_phrase: validator_seed.to_string(),
                created_at: 1,
            }),
            config: Some(old_config),
            data_migration_notice: None,
        };
        let notice = store
            .protect_legacy_v07_data()
            .unwrap()
            .expect("first v0.8 launch fences the v0.7 WAL");
        let migrated_config = store.config.as_ref().unwrap();
        let resources = TestnetResources {
            seeds_file: Some(seeds.clone()),
            genesis_file: Some(genesis.clone()),
        };
        let mut manager = NodeManager::new();
        manager
            .configure_legacy_windows_stop_context(
                migrated_config,
                &notice,
                Zeroizing::new(validator_seed.to_string()),
                &resources,
            )
            .expect("legacy proof does not depend on a first-launch v0.8 keyfile");
        assert!(!dir.join("identity/validator-key.json").exists());
        let context = manager.legacy_windows_stop_context.as_ref().unwrap();

        // Exact argv emitted by the v0.7.7/v0.7.10/v0.7.11 release launcher.
        // The newly configured data-v3 path did not exist in this already-
        // running process, and those tags had no stake/current-worker flags.
        let exact = vec![
            "arc-node.exe".into(),
            "--rpc".into(),
            "127.0.0.1:9090".into(),
            "--p2p-port".into(),
            "9091".into(),
            "--data-dir".into(),
            data.clone().into_os_string(),
            "--validator-seed".into(),
            validator_seed.into(),
            "--eth-rpc-port".into(),
            "0".into(),
            "--seeds-file".into(),
            seeds.clone().into_os_string(),
            "--genesis".into(),
            genesis.clone().into_os_string(),
        ];
        assert!(legacy_windows_command_matches(&exact, None, context));
        let without_bundle_resources = vec![
            "arc-node.exe".into(),
            "--rpc".into(),
            "127.0.0.1:9090".into(),
            "--p2p-port".into(),
            "9091".into(),
            "--data-dir".into(),
            data.clone().into_os_string(),
            "--validator-seed".into(),
            validator_seed.into(),
            "--eth-rpc-port".into(),
            "0".into(),
        ];
        assert!(!legacy_windows_command_matches(
            &without_bundle_resources,
            None,
            context
        ));
        let mut one_sided_resources = without_bundle_resources.clone();
        one_sided_resources.extend(["--seeds-file".into(), seeds.clone().into_os_string()]);
        assert!(!legacy_windows_command_matches(
            &one_sided_resources,
            None,
            context
        ));

        for extra in [
            vec!["--archive".into()],
            vec!["--stake".into(), "0".into()],
            vec!["--data-dir".into(), data.clone().into_os_string()],
            vec!["--rpc".into(), "127.0.0.1:9090".into()],
            vec!["--genesis".into(), genesis.clone().into_os_string()],
            vec!["--validator-seed".into(), validator_seed.into()],
            vec!["--validator-key-file".into(), "missing.json".into()],
        ] {
            let mut near_miss = exact.clone();
            near_miss.extend(extra);
            assert!(!legacy_windows_command_matches(&near_miss, None, context));
        }
        let mut wrong_pair = exact.clone();
        *wrong_pair
            .iter_mut()
            .find(|value| *value == "9091")
            .unwrap() = "9101".into();
        assert!(!legacy_windows_command_matches(&wrong_pair, None, context));
        let mut wrong_seed = exact.clone();
        *wrong_seed
            .iter_mut()
            .find(|value| *value == validator_seed)
            .unwrap() = "different recovery phrase".into();
        assert!(!legacy_windows_command_matches(&wrong_seed, None, context));
        let attempted = manager.legacy_windows_stop_context.take();
        assert!(finish_legacy_reconciliation(
            &mut manager.legacy_windows_stop_context,
            attempted,
            Err(anyhow::anyhow!("simulated retained-handle refusal")),
        )
        .is_err());
        assert!(manager.legacy_windows_stop_context.is_some());

        let model = dir.join("legacy-worker.gguf");
        std::fs::write(&model, b"legacy worker fixture").unwrap();
        let mut worker_config = migrated_config.clone();
        worker_config.role = "worker".into();
        worker_config.model_path = Some(model.to_string_lossy().into_owned());
        let mut worker_manager = NodeManager::new();
        worker_manager
            .configure_legacy_windows_stop_context(
                &worker_config,
                &notice,
                Zeroizing::new(validator_seed.to_string()),
                &resources,
            )
            .unwrap();
        let worker_context = worker_manager.legacy_windows_stop_context.as_ref().unwrap();
        let mut worker_exact = exact.clone();
        let insertion = worker_exact.len();
        worker_exact.splice(
            insertion..insertion,
            [
                "--community-mode".into(),
                "--model".into(),
                model.into_os_string(),
            ],
        );
        assert!(legacy_windows_command_matches(
            &worker_exact,
            None,
            worker_context
        ));
        let mut current_origin_near_miss = worker_exact.clone();
        current_origin_near_miss.extend([
            "--community-rpc-url".into(),
            crate::rpc_client::PRODUCTION_RPC_ORIGINS[0].into(),
        ]);
        assert!(!legacy_windows_command_matches(
            &current_origin_near_miss,
            None,
            worker_context
        ));

        // Successful first-upgrade reconciliation consumes and zeroizes the
        // legacy proof. A later stop of the capability-aware v0.8 child sees
        // no tokenless context and therefore cannot re-enter force migration.
        let attempted = worker_manager.legacy_windows_stop_context.take();
        assert!(attempted.is_some());
        let first = finish_legacy_reconciliation(
            &mut worker_manager.legacy_windows_stop_context,
            attempted,
            Ok(StopSummary {
                stopped: 1,
                forced: 1,
            }),
        )
        .unwrap();
        assert_eq!(first.stopped, 1);
        assert!(worker_manager.legacy_windows_stop_context.is_none());
        let second_attempt = worker_manager.legacy_windows_stop_context.take();
        let second = finish_legacy_reconciliation(
            &mut worker_manager.legacy_windows_stop_context,
            second_attempt,
            Ok(StopSummary::default()),
        )
        .unwrap();
        assert_eq!(second.stopped, 0);
        assert!(worker_manager.legacy_windows_stop_context.is_none());

        // Public Windows tags used HOME only for `~/.arc`; with HOME absent
        // they emitted relative `.arc` even though the managed executable was
        // under USERPROFILE. Resolve that argv against the target process cwd,
        // never the new v0.8 GUI cwd, and fence the exact resulting history.
        let old_cwd = dir.join("v07-old-cwd");
        let new_cwd = dir.join("v08-new-cwd");
        let relative_data = old_cwd.join(".arc");
        std::fs::create_dir_all(&relative_data).unwrap();
        std::fs::create_dir_all(&new_cwd).unwrap();
        std::fs::write(relative_data.join("state.wal"), b"relative v07 history").unwrap();
        let relative_config = NodeConfig::default();
        let relative_context = build_legacy_windows_stop_context(
            &relative_config,
            relative_data.canonicalize().unwrap(),
            Zeroizing::new(validator_seed.to_string()),
            &resources,
        )
        .unwrap();
        let relative_argv = vec![
            "arc-node.exe".into(),
            "--rpc".into(),
            "127.0.0.1:9090".into(),
            "--p2p-port".into(),
            "9091".into(),
            "--data-dir".into(),
            ".arc".into(),
            "--validator-seed".into(),
            validator_seed.into(),
            "--eth-rpc-port".into(),
            "0".into(),
            "--seeds-file".into(),
            seeds.into_os_string(),
            "--genesis".into(),
            genesis.into_os_string(),
        ];
        assert!(legacy_windows_command_matches(
            &relative_argv,
            Some(&old_cwd),
            &relative_context
        ));
        assert!(!legacy_windows_command_matches(
            &relative_argv,
            Some(&new_cwd),
            &relative_context
        ));
        let mut relative_store = crate::store::Store {
            identity: Some(crate::types::Identity {
                address: "55".repeat(32),
                public_key: format!("0x{}", "66".repeat(32)),
                seed_phrase: validator_seed.into(),
                created_at: 2,
            }),
            config: Some(relative_config),
            data_migration_notice: None,
        };
        let relative_notice = relative_store
            .protect_legacy_v07_data_at(&relative_data)
            .unwrap()
            .unwrap();
        assert_eq!(
            PathBuf::from(relative_notice.legacy_data_dir)
                .canonicalize()
                .unwrap(),
            relative_data.canonicalize().unwrap()
        );
        assert!(PathBuf::from(relative_notice.active_data_dir).starts_with(&relative_data));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_stop_preserves_a_live_owned_writer_and_control() {
        let dir = resource_test_dir("failed-stop-ownership");
        std::fs::create_dir_all(&dir).unwrap();
        let control = prepare_desktop_shutdown_control(&dir).unwrap();
        let child = Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn disposable live child");
        let pid = child.id().unwrap();
        let mut manager = NodeManager::new();
        manager.started_at = Some(Instant::now());
        manager.restore_child_after_failed_stop(child, Some(control));
        assert_eq!(manager.pid(), Some(pid));
        assert!(manager.shutdown_control.is_some());
        assert!(
            manager.is_running(),
            "the command-level Start guard must observe the preserved owned writer"
        );

        let child = manager.child.as_mut().unwrap();
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        manager.child = None;
        manager.shutdown_control = None;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_requests_graceful_exit_reaps_child_and_clears_stop_state() {
        let dir = resource_test_dir("graceful-stop");
        arc_crypto::secret_file::secure_private_directory_tree(&dir).unwrap();
        let marker = dir.join("graceful");
        let ack_ready = dir.join("ack-ready");
        let shell = PathBuf::from("/bin/sh").canonicalize().unwrap();
        let (control, genesis) = receipt_control_for_executable(&dir, &shell);
        let request = control.request_file.clone();
        let child = Command::new("sh")
            .args([
                "-c",
                "while [ ! -f \"$ARC_STOP_REQUEST\" ]; do sleep 0.01; done; printf graceful > \"$ARC_STOP_MARKER\"; while [ ! -f \"$ARC_ACK_READY\" ]; do sleep 0.01; done; exit 0",
            ])
            .env("ARC_STOP_REQUEST", &request)
            .env("ARC_STOP_MARKER", &marker)
            .env("ARC_ACK_READY", &ack_ready)
            .spawn()
            .expect("spawn disposable child");
        let pid = child.id().expect("child pid");
        let token = control.token_bytes().unwrap();
        let data = control.data_dir.clone();
        let executable = control.receipt_executable.clone().unwrap();
        let nonce = control.receipt_nonce.unwrap();
        let ack_request = request.clone();
        let ack_task = tokio::spawn(async move {
            for _ in 0..500 {
                if ack_request.is_file() {
                    arc_crypto::secret_file::acknowledge_desktop_shutdown_receipt(
                        &data,
                        &token,
                        &nonce,
                        &executable,
                        &genesis,
                    )
                    .unwrap();
                    std::fs::write(ack_ready, b"ready").unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("authenticated shutdown request was never published");
        });
        let mut manager = NodeManager::new();
        manager.child = Some(child);
        manager.shutdown_control = Some(control);
        manager.started_at = Some(Instant::now());

        // Exercise only the child handle created by this test. Calling the
        // public `stop()` here would also scan production managed paths and
        // could terminate a real ARC node running on the developer's machine.
        assert_eq!(manager.stop_owned_child().await.expect("verified stop"), 1);
        ack_task.await.unwrap();
        assert!(manager.child.is_none());
        assert!(manager.started_at.is_none());
        assert!(!manager.stopping.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "graceful",
            "authenticated request handler must run before the child is reaped"
        );

        let mut processes = sysinfo::System::new();
        processes.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        assert!(
            processes.process(sysinfo::Pid::from_u32(pid)).is_none(),
            "stop must reap the old child before an updater relaunch"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unresponsive_child_is_force_killed_only_after_grace_period() {
        let dir = resource_test_dir("unresponsive-stop");
        let shell = PathBuf::from("/bin/sh").canonicalize().unwrap();
        let (mut control, _genesis) = receipt_control_for_executable(&dir, &shell);
        let mut child = Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn TERM-resistant disposable child");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = Instant::now();
        let error =
            terminate_owned_child(&mut child, Some(&mut control), Duration::from_millis(100))
                .await
                .expect_err("force fallback must leave the updater boundary blocked");
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(
            error.to_string().contains("required force termination"),
            "force-kill must be reported and leave the receipt armed: {error}"
        );
        assert!(child.try_wait().unwrap().is_some(), "child must be reaped");
        assert!(
            arc_crypto::secret_file::desktop_shutdown_receipt_exists(&dir).unwrap(),
            "an unproven forced stop must retain its durable fence"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn durability_recovery_error_remains_typed_through_context() {
        let error = anyhow::Error::new(ManagedDurabilityRecoveryRequired {
            reason: "test fence".into(),
        })
        .context("startup reconciliation");
        assert!(is_managed_durability_recovery_required(&error));
        assert!(!is_managed_durability_recovery_required(&anyhow::anyhow!(
            "detached process is still live"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_health_requires_exact_ready_public_release_version() {
        let versions = ["0.7.7", "0.7.10", "0.7.11"];
        assert!(legacy_v07_health_ready(
            &serde_json::json!({"status": "ok", "version": "0.7.11"}),
            &versions,
        )
        .unwrap());
        assert!(
            !legacy_v07_health_ready(&serde_json::json!({"status": "starting"}), &versions)
                .unwrap()
        );
        assert!(legacy_v07_health_ready(
            &serde_json::json!({"status": "ok", "version": "0.8.0"}),
            &versions,
        )
        .is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix_legacy_handle_signals_only_the_retained_process_identity() {
        let executable = PathBuf::from("/bin/sleep").canonicalize().unwrap();
        let mut target = std::process::Command::new(&executable)
            .arg("30")
            .spawn()
            .unwrap();
        let mut unrelated = std::process::Command::new(&executable)
            .arg("30")
            .spawn()
            .unwrap();
        let result = (|| -> anyhow::Result<()> {
            let handle = UnixLegacyProcessHandle::open(target.id(), &executable)?;
            anyhow::ensure!(handle.is_live()?, "retained target must start live");
            handle.signal_term()?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while handle.is_live()? {
                anyhow::ensure!(Instant::now() < deadline, "target did not exit");
                std::thread::sleep(Duration::from_millis(10));
            }
            anyhow::ensure!(
                unrelated.try_wait()?.is_none(),
                "kernel-bound signal affected an unrelated process"
            );
            Ok(())
        })();
        if target.try_wait().unwrap().is_none() {
            let _ = target.kill();
        }
        let _ = target.wait();
        if unrelated.try_wait().unwrap().is_none() {
            let _ = unrelated.kill();
        }
        let _ = unrelated.wait();
        result.unwrap();
    }
}
