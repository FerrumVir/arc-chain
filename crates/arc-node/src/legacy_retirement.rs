//! Offline, fail-closed retirement evidence for ARC v0.7 community nodes.
//!
//! This module is deliberately outside normal node startup: it opens no ARC
//! network connection, starts no listener, never signals a process, and never
//! opens legacy data writable. The only mutations are create-only intent and
//! receipt files plus the persistent create-only sibling namespace-lock file
//! that excludes v0.8 startup while the receipt crosses its durability barrier;
//! all are outside the legacy tree.

use crate::recovery_descriptor;
use anyhow::{Context, Result, bail, ensure};
use clap::{ArgGroup, Subcommand};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::fs::File;
use std::fs::{Metadata, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

const INTENT_SCHEMA: &str = "arc.migration.legacy-v07-community-retirement-intent.v1";
const RECEIPT_SCHEMA: &str = "arc.migration.legacy-v07-community-retirement-receipt.v1";
const STOP_EVIDENCE_SCHEMA: &str = "arc.migration.legacy-v07-term-only-stop-evidence.v1";
const PREEXISTING_EVIDENCE_SCHEMA: &str =
    "arc.migration.legacy-v07-preexisting-offline-evidence.v1";
const INSTALLER_BINDING_SCHEMA: &str = "arc.release-installer-binding.v1";
const INTERNAL_HANDOFF_SCHEMA: &str = "arc.release-manifest-handoff.v1";
const BOUNDARY_SCHEMA: &str = "arc.recovery.legacy-maintenance-boundary.v1";
const POLICY_SCHEMA: &str = "arc-cutover-policy/v1";
const DESCRIPTOR_SCHEMA: &str = "arc-recovery-checkpoint-descriptor/v1";
const SUPERVISOR_SCHEMA: &str = "arc.migration.legacy-v07-supervisor-binding.v1";
const REPOSITORY: &str = "FerrumVir/arc-chain";
const POLICY_ASSET: &str = "arc-cutover-policy.json";
const BOUNDARY_ASSET: &str = "arc-legacy-maintenance-boundary.json";
const DESCRIPTOR_ASSET: &str = "arc-recovery-checkpoint-descriptor.json";
const JOBS_DISPOSITION: &str = "expired_noncanonical_at_cutover";
const SOURCE_HEIGHT: u64 = 137_145;
const TRANSITION_HEIGHT: u64 = 137_146;
const MAX_JSON_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DESCRIPTOR_BYTES: u64 = 1024 * 1024;
const MAX_EXECUTABLE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_TREE_ENTRIES: usize = 250_000;
const MAX_TREE_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_PROCESS_EXECUTABLE_HASHES: usize = 128;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_PROCESS_EXECUTABLE_HASH_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const LEGACY_PORTS: [u16; 2] = [9090, 3001];

const FLEET: [(&str, &str); 6] = [
    ("nyc", "149.28.32.76"),
    ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"),
    ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"),
    ("sgp", "149.28.153.31"),
];

#[derive(Clone, Debug, Subcommand)]
// This command is parsed once at process startup. Keeping each signed artifact as a
// named clap field makes accidental cross-binding harder to review than boxing an
// arbitrary subset solely to reduce the enum's stack size.
#[allow(clippy::large_enum_variant)]
pub(crate) enum LegacyRetirementCommand {
    /// Seal an exact running or already-offline v0.7 stake-zero installation.
    #[command(group(
        ArgGroup::new("process_mode")
            .required(true)
            .multiple(false)
            .args(["legacy_pid", "already_offline"])
    ))]
    CreateIntent {
        #[arg(long)]
        intent_output: PathBuf,
        #[arg(long)]
        target_release: PathBuf,
        #[arg(long)]
        target_release_sha256: String,
        #[arg(long)]
        maintenance_boundary: PathBuf,
        #[arg(long)]
        maintenance_boundary_sha256: String,
        #[arg(long)]
        cutover_policy: PathBuf,
        #[arg(long)]
        cutover_policy_sha256: String,
        #[arg(long)]
        checkpoint_descriptor: PathBuf,
        #[arg(long)]
        checkpoint_descriptor_sha256: String,
        #[arg(long)]
        legacy_pid: Option<u32>,
        #[arg(long, default_value_t = false)]
        already_offline: bool,
        #[arg(long)]
        legacy_version: String,
        #[arg(long)]
        legacy_executable: PathBuf,
        #[arg(long)]
        legacy_executable_sha256: String,
        #[arg(long)]
        supervisor_definition: PathBuf,
        #[arg(long)]
        supervisor_definition_sha256: String,
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        v08_data_dir: PathBuf,
        /// Mandatory community-safe mode. Old forks are preserved, never migrated.
        #[arg(long, required = true)]
        forensic_only: bool,
    },
    /// Prove stable offline state and atomically publish the final receipt.
    Finalize {
        #[arg(long)]
        intent: PathBuf,
        #[arg(long)]
        intent_sha256: String,
        #[arg(long)]
        stop_evidence: PathBuf,
        #[arg(long)]
        stop_evidence_sha256: String,
        #[arg(long)]
        receipt_output: PathBuf,
        #[arg(long, default_value_t = 10)]
        stability_seconds: u64,
        #[arg(long, default_value_t = 3)]
        samples: u32,
    },
}

#[derive(Clone, Debug)]
struct CreateRequest {
    intent_output: PathBuf,
    target_release: PathBuf,
    target_release_sha256: String,
    maintenance_boundary: PathBuf,
    maintenance_boundary_sha256: String,
    cutover_policy: PathBuf,
    cutover_policy_sha256: String,
    checkpoint_descriptor: PathBuf,
    checkpoint_descriptor_sha256: String,
    mode: RetirementMode,
    legacy_version: String,
    legacy_executable: PathBuf,
    legacy_executable_sha256: String,
    supervisor_definition: PathBuf,
    supervisor_definition_sha256: String,
    data_dir: PathBuf,
    v08_data_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetirementMode {
    TermOnly(u32),
    PreexistingOffline,
}

impl RetirementMode {
    fn label(self) -> &'static str {
        match self {
            Self::TermOnly(_) => "term_only",
            Self::PreexistingOffline => "preexisting_offline",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileRecord {
    path: String,
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    sha256: String,
}

impl FileRecord {
    fn value(&self) -> Value {
        json!({
            "path": self.path,
            "device": self.device,
            "inode": self.inode,
            "mode": self.mode,
            "uid": self.uid,
            "gid": self.gid,
            "nlink": self.nlink,
            "size": self.size,
            "mtime_ns": self.mtime_ns,
            "ctime_ns": self.ctime_ns,
            "sha256": self.sha256,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ListenerEndpoint {
    family: String,
    address_hex: String,
    port: u16,
    inode: u64,
}

impl ListenerEndpoint {
    fn value(&self) -> Value {
        json!({
            "family": self.family,
            "address_hex": self.address_hex,
            "port": self.port,
            "inode": self.inode,
        })
    }
}

#[derive(Clone, Debug)]
struct ProcessObservation {
    pid: u32,
    boot_id: String,
    start_ticks: u64,
    uid: u32,
    gid: u32,
    executable: FileRecord,
    argv: Vec<String>,
    cwd: Option<String>,
    listeners: Vec<ListenerEndpoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ProcessExecutableIdentity {
    device: u64,
    inode: u64,
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ProcessExecutableIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        let identity = metadata_identity(metadata);
        Self {
            device: identity.0,
            inode: identity.1,
            size: identity.6,
            mtime_ns: identity.7,
            ctime_ns: identity.8,
        }
    }

    fn from_record(record: &FileRecord) -> Self {
        Self {
            device: record.device,
            inode: record.inode,
            size: record.size,
            mtime_ns: record.mtime_ns,
            ctime_ns: record.ctime_ns,
        }
    }
}

#[derive(Default)]
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ProcessExecutableHashCacheState {
    records: HashMap<ProcessExecutableIdentity, FileRecord>,
    hash_operations: usize,
    hashed_bytes: u64,
}

#[derive(Default)]
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ProcessExecutableHashCache {
    state: Mutex<ProcessExecutableHashCacheState>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ProcessExecutableHashCache {
    fn get_or_hash<F>(&self, identity: ProcessExecutableIdentity, hash: F) -> Result<FileRecord>
    where
        F: FnOnce() -> Result<FileRecord>,
    {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("process executable hash cache is poisoned"))?;
            if let Some(record) = state.records.get(&identity) {
                return Ok(record.clone());
            }
            ensure!(
                state.hash_operations < MAX_PROCESS_EXECUTABLE_HASHES,
                "same-owner process executable hash-count bound exceeded"
            );
            let next_bytes = state
                .hashed_bytes
                .checked_add(identity.size)
                .context("same-owner process executable hash-byte count overflow")?;
            ensure!(
                next_bytes <= MAX_PROCESS_EXECUTABLE_HASH_BYTES,
                "same-owner process executable hash-byte bound exceeded"
            );
            state.hash_operations += 1;
            state.hashed_bytes = next_bytes;
        }
        let record = hash()?;
        ensure!(
            ProcessExecutableIdentity::from_record(&record) == identity,
            "process executable identity changed while it was hashed"
        );
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("process executable hash cache is poisoned"))?;
        state.records.insert(identity, record.clone());
        Ok(record)
    }

    #[cfg(test)]
    fn hash_operations(&self) -> usize {
        self.state
            .lock()
            .expect("test process hash cache lock")
            .hash_operations
    }
}

#[derive(Default)]
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct ProcessExecutableHashCache;

trait RetirementHost {
    fn now(&self) -> String;
    fn sleep(&self, duration: Duration);
    fn verify_descriptor(
        &self,
        path: &Path,
    ) -> Result<recovery_descriptor::VerifiedDescriptorSummary>;
    fn observe_process(&self, pid: u32) -> Result<Option<ProcessObservation>>;
    fn all_process_ids(&self) -> Result<Vec<u32>>;
    fn active_listener_endpoints(&self) -> Result<Vec<ListenerEndpoint>>;
    fn matching_processes(
        &self,
        legacy_owner_uid: u32,
        data_dir: &str,
        executable_path: &str,
        executable_size: u64,
        executable_sha256: &str,
    ) -> Result<Vec<ProcessObservation>>
    where
        Self: Sized,
    {
        matching_replacement_processes(
            self,
            legacy_owner_uid,
            data_dir,
            executable_path,
            executable_size,
            executable_sha256,
        )
    }
}

#[derive(Default)]
struct SystemHost {
    process_executable_hashes: ProcessExecutableHashCache,
}

impl RetirementHost for SystemHost {
    fn now(&self) -> String {
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }

    fn verify_descriptor(
        &self,
        path: &Path,
    ) -> Result<recovery_descriptor::VerifiedDescriptorSummary> {
        recovery_descriptor::verify_for_retirement(path)
    }

    fn observe_process(&self, pid: u32) -> Result<Option<ProcessObservation>> {
        system_observe_process(pid)
    }

    fn all_process_ids(&self) -> Result<Vec<u32>> {
        system_all_process_ids()
    }

    fn active_listener_endpoints(&self) -> Result<Vec<ListenerEndpoint>> {
        system_active_listener_endpoints()
    }

    fn matching_processes(
        &self,
        legacy_owner_uid: u32,
        data_dir: &str,
        executable_path: &str,
        executable_size: u64,
        executable_sha256: &str,
    ) -> Result<Vec<ProcessObservation>> {
        system_matching_processes(
            &self.process_executable_hashes,
            legacy_owner_uid,
            data_dir,
            executable_path,
            executable_size,
            executable_sha256,
        )
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn require_lower_hash(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)),
        "{label} must be exactly 64 lowercase hexadecimal characters"
    );
    Ok(())
}

fn path_string(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{label} is not valid UTF-8"))
}

fn require_absolute_normal(path: &Path, label: &str) -> Result<()> {
    ensure!(path.is_absolute(), "{label} must be absolute");
    let rebuilt = path.components().collect::<PathBuf>();
    ensure!(
        rebuilt.as_os_str() == path.as_os_str()
            && path.components().all(|component| {
                !matches!(component, Component::CurDir | Component::ParentDir)
            }),
        "{label} must not contain dot segments"
    );
    ensure!(
        path.file_name().is_some(),
        "{label} cannot be a filesystem root"
    );
    Ok(())
}

#[cfg(unix)]
fn metadata_identity(metadata: &Metadata) -> (u64, u64, u32, u32, u32, u64, u64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.uid(),
        metadata.gid(),
        metadata.nlink(),
        metadata.size(),
        metadata.mtime().saturating_mul(1_000_000_000) + metadata.mtime_nsec(),
        metadata.ctime().saturating_mul(1_000_000_000) + metadata.ctime_nsec(),
    )
}

#[cfg(not(unix))]
fn metadata_identity(_metadata: &Metadata) -> (u64, u64, u32, u32, u32, u64, u64, i64, i64) {
    (0, 0, 0, 0, 0, 0, 0, 0, 0)
}

#[cfg(unix)]
fn validate_readable_metadata(metadata: &Metadata, path: &Path, label: &str) -> Result<()> {
    ensure!(metadata.is_file(), "{label} must be a regular file");
    ensure!(
        metadata.nlink() == 1,
        "{label} must have exactly one hard link"
    );
    ensure!(
        metadata.mode() & 0o022 == 0,
        "{label} must not be group/world writable"
    );
    let effective = unsafe { libc::geteuid() };
    ensure!(
        metadata.uid() == effective || metadata.uid() == 0,
        "{label} is not owned by the effective user or root: {}",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_readable_metadata(_metadata: &Metadata, _path: &Path, _label: &str) -> Result<()> {
    bail!("legacy-retirement file verification is supported only on Linux and macOS")
}

fn stable_read(path: &Path, label: &str, maximum: u64) -> Result<(Vec<u8>, FileRecord)> {
    require_absolute_normal(path, label)?;
    reject_symlink_ancestors(path, true, label)?;
    let before_path = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot stat {label} {}", path.display()))?;
    ensure!(
        !before_path.file_type().is_symlink(),
        "{label} must not be a symlink"
    );
    validate_readable_metadata(&before_path, path, label)?;
    ensure!(
        before_path.len() > 0 && before_path.len() <= maximum,
        "{label} size is outside its bounded contract"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot no-follow open {label} {}", path.display()))?;
    let before = file.metadata().context("cannot inspect opened file")?;
    ensure!(
        metadata_identity(&before) == metadata_identity(&before_path),
        "{label} pathname changed before its no-follow open"
    );
    let mut bytes = Vec::with_capacity(before.len().min(maximum) as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {label}"))?;
    ensure!(
        bytes.len() as u64 == before.len(),
        "{label} changed length while read"
    );
    let after = file.metadata().context("cannot reinspect opened file")?;
    let after_path =
        std::fs::symlink_metadata(path).with_context(|| format!("cannot restat {label}"))?;
    ensure!(
        metadata_identity(&after) == metadata_identity(&before)
            && metadata_identity(&after_path) == metadata_identity(&before),
        "{label} changed while it was read"
    );
    reject_symlink_ancestors(path, true, label)?;
    let identity = metadata_identity(&before);
    Ok((
        bytes.clone(),
        FileRecord {
            path: path_string(path, label)?,
            device: identity.0,
            inode: identity.1,
            mode: identity.2,
            uid: identity.3,
            gid: identity.4,
            nlink: identity.5,
            size: identity.6,
            mtime_ns: identity.7,
            ctime_ns: identity.8,
            sha256: sha256(&bytes),
        },
    ))
}

/// Emit the byte-for-byte form used by the frozen Python oracle:
/// `json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n"`.
///
/// In particular, Python's default `ensure_ascii=True` is consensus-relevant
/// here. `serde_json::to_vec` emits UTF-8 for non-ASCII strings, which would
/// otherwise give the Rust verifier a different digest for an entirely valid
/// Unicode home/data path.
fn canonical_bytes(input: &Value) -> Result<Vec<u8>> {
    fn string(output: &mut Vec<u8>, value: &str) {
        output.push(b'"');
        for character in value.chars() {
            match character {
                '"' => output.extend_from_slice(br#"\""#),
                '\\' => output.extend_from_slice(br#"\\"#),
                '\u{0008}' => output.extend_from_slice(br#"\b"#),
                '\u{000c}' => output.extend_from_slice(br#"\f"#),
                '\n' => output.extend_from_slice(br#"\n"#),
                '\r' => output.extend_from_slice(br#"\r"#),
                '\t' => output.extend_from_slice(br#"\t"#),
                '\u{0020}'..='\u{007e}' => output.push(character as u8),
                _ => {
                    let scalar = character as u32;
                    if scalar <= 0xffff {
                        output.extend_from_slice(format!("\\u{scalar:04x}").as_bytes());
                    } else {
                        let adjusted = scalar - 0x1_0000;
                        let high = 0xd800 + (adjusted >> 10);
                        let low = 0xdc00 + (adjusted & 0x3ff);
                        output.extend_from_slice(format!("\\u{high:04x}\\u{low:04x}").as_bytes());
                    }
                }
            }
        }
        output.push(b'"');
    }

    fn value(output: &mut Vec<u8>, input: &Value) -> Result<()> {
        match input {
            Value::Null => output.extend_from_slice(b"null"),
            Value::Bool(true) => output.extend_from_slice(b"true"),
            Value::Bool(false) => output.extend_from_slice(b"false"),
            Value::Number(number) => {
                ensure!(
                    number.is_i64() || number.is_u64(),
                    "canonical retirement JSON forbids non-integral numbers"
                );
                output.extend_from_slice(number.to_string().as_bytes());
            }
            Value::String(text) => string(output, text),
            Value::Array(items) => {
                output.push(b'[');
                for (index, item) in items.iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    value(output, item)?;
                }
                output.push(b']');
            }
            Value::Object(object) => {
                output.push(b'{');
                let mut keys = object.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for (index, key) in keys.into_iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    string(output, key);
                    output.push(b':');
                    value(output, &object[key])?;
                }
                output.push(b'}');
            }
        }
        Ok(())
    }

    let mut bytes = Vec::new();
    value(&mut bytes, input).context("cannot serialize canonical retirement JSON")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn load_canonical_json(
    path: &Path,
    label: &str,
    expected_sha256: &str,
    maximum: u64,
) -> Result<(Value, Vec<u8>, FileRecord)> {
    require_lower_hash(expected_sha256, &format!("{label} SHA-256"))?;
    let (bytes, record) = stable_read(path, label, maximum)?;
    ensure!(record.sha256 == expected_sha256, "{label} SHA-256 differs");
    let value: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("{label} is invalid JSON"))?;
    ensure!(value.is_object(), "{label} must be one JSON object");
    ensure!(
        canonical_bytes(&value)? == bytes,
        "{label} must be canonical JSON with one trailing newline"
    );
    Ok((value, bytes, record))
}

fn object_exact<'a>(
    value: &'a Value,
    keys: &[&str],
    label: &str,
) -> Result<&'a Map<String, Value>> {
    let object = value
        .as_object()
        .with_context(|| format!("{label} must be an object"))?;
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = keys.iter().copied().collect::<BTreeSet<_>>();
    ensure!(actual == expected, "{label} has missing or unknown fields");
    Ok(object)
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str, label: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("{label}.{key} must be a string"))
}

fn u64_field(object: &Map<String, Value>, key: &str, label: &str) -> Result<u64> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("{label}.{key} must be a non-negative integer"))
}

fn bool_field(object: &Map<String, Value>, key: &str, label: &str) -> Result<bool> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .with_context(|| format!("{label}.{key} must be boolean"))
}

fn expect_hash_field(object: &Map<String, Value>, key: &str, label: &str) -> Result<String> {
    let value = string_field(object, key, label)?;
    require_lower_hash(value, &format!("{label}.{key}"))?;
    Ok(value.to_owned())
}

fn validate_semver(tag: &str) -> Result<()> {
    let parts = tag
        .strip_prefix('v')
        .context("release tag must start with v")?
        .split('.')
        .collect::<Vec<_>>();
    ensure!(parts.len() == 3, "release tag must be canonical vX.Y.Z");
    let mut numbers = Vec::new();
    for part in parts {
        ensure!(
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == "0" || !part.starts_with('0')),
            "release tag must be canonical vX.Y.Z"
        );
        numbers.push(
            part.parse::<u64>()
                .context("release version overflows u64")?,
        );
    }
    ensure!(
        (numbers[0], numbers[1], numbers[2]) >= (0, 8, 0),
        "release predates v0.8.0"
    );
    Ok(())
}

#[derive(Clone, Debug)]
struct ReleaseBinding {
    projected: Value,
    repository: String,
    tag: String,
    commit: String,
    files: Map<String, Value>,
}

#[derive(Clone, Debug)]
struct BoundaryBinding {
    projected: Value,
    legacy_public_max_height: u64,
    freeze_plan_sha256: String,
    capture_id: String,
    first_quarantine_started_at: String,
    all_controlled_stopped_at: String,
}

fn validate_release_binding(
    value: &Value,
    policy_sha256: &str,
    boundary_sha256: &str,
    descriptor_sha256: &str,
) -> Result<ReleaseBinding> {
    let object = value
        .as_object()
        .context("target release binding must be an object")?;
    let schema = string_field(object, "schema", "target release")?;
    let (manifest_sha256, signature_sha256) = if schema == INSTALLER_BINDING_SCHEMA {
        object_exact(
            value,
            &[
                "schema",
                "repository",
                "tag",
                "commit",
                "signed_manifest_sha256",
                "manifest_signature_sha256",
                "files",
            ],
            "installer release binding",
        )?;
        (
            expect_hash_field(object, "signed_manifest_sha256", "target release")?,
            Some(expect_hash_field(
                object,
                "manifest_signature_sha256",
                "target release",
            )?),
        )
    } else if schema == INTERNAL_HANDOFF_SCHEMA {
        object_exact(
            value,
            &[
                "schema",
                "sealed",
                "repository",
                "commit",
                "tag",
                "workflow_run_id",
                "workflow_run_attempt",
                "files",
                "modes",
                "manifest_sha256",
            ],
            "internal release handoff",
        )?;
        ensure!(
            bool_field(object, "sealed", "target release")?,
            "internal release handoff is not sealed"
        );
        ensure!(
            u64_field(object, "workflow_run_id", "target release")? > 0
                && u64_field(object, "workflow_run_attempt", "target release")? > 0,
            "internal release workflow identity is invalid"
        );
        (
            expect_hash_field(object, "manifest_sha256", "target release")?,
            None,
        )
    } else {
        bail!("target release binding schema is unsupported");
    };
    let repository = string_field(object, "repository", "target release")?.to_owned();
    ensure!(
        repository == REPOSITORY,
        "target release repository differs"
    );
    let tag = string_field(object, "tag", "target release")?.to_owned();
    validate_semver(&tag)?;
    let commit = string_field(object, "commit", "target release")?.to_owned();
    ensure!(
        commit.len() == 40
            && commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "target release commit must be one lowercase full Git SHA"
    );
    let files = object
        .get("files")
        .and_then(Value::as_object)
        .context("target release files map is missing")?
        .clone();
    ensure!(!files.is_empty(), "target release files map is empty");
    for (name, digest) in &files {
        ensure!(
            !name.is_empty() && !name.contains('/') && !name.contains('\\'),
            "target release contains an unsafe asset name"
        );
        require_lower_hash(
            digest
                .as_str()
                .context("target release asset hash must be a string")?,
            &format!("target release asset {name}"),
        )?;
    }
    for (asset, expected) in [
        (POLICY_ASSET, policy_sha256),
        (BOUNDARY_ASSET, boundary_sha256),
        (DESCRIPTOR_ASSET, descriptor_sha256),
    ] {
        ensure!(
            files.get(asset).and_then(Value::as_str) == Some(expected),
            "{asset} is not the exact signed release asset"
        );
    }
    if schema == INTERNAL_HANDOFF_SCHEMA {
        let modes = object
            .get("modes")
            .and_then(Value::as_object)
            .context("internal release modes map is missing")?;
        ensure!(
            modes.keys().collect::<BTreeSet<_>>() == files.keys().collect::<BTreeSet<_>>(),
            "internal release file/mode maps differ"
        );
        for asset in [POLICY_ASSET, BOUNDARY_ASSET, DESCRIPTOR_ASSET] {
            ensure!(
                modes.get(asset).and_then(Value::as_u64) == Some(0o644),
                "internal release mode differs for {asset}"
            );
        }
    }
    let mut projected = json!({
        "binding_schema": schema,
        "repository": repository,
        "tag": tag,
        "commit": commit,
        "manifest_sha256": manifest_sha256,
        "inspector_asset": Value::Null,
        "inspector_sha256": Value::Null,
        "files": files,
    });
    if let Some(signature) = signature_sha256 {
        projected
            .as_object_mut()
            .expect("JSON object")
            .insert("manifest_signature_sha256".into(), signature.into());
    }
    Ok(ReleaseBinding {
        projected,
        repository,
        tag,
        commit,
        files,
    })
}

fn validate_utc(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 20 && value.ends_with('Z'),
        "{label} must use canonical UTC seconds"
    );
    let parsed = chrono::DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("{label} is invalid"))?;
    ensure!(
        parsed.to_rfc3339_opts(chrono::SecondsFormat::Secs, true) == value,
        "{label} must use canonical UTC seconds"
    );
    Ok(())
}

fn validate_boundary(value: &Value) -> Result<BoundaryBinding> {
    let object = value
        .as_object()
        .context("maintenance boundary must be an object")?;
    ensure!(
        string_field(object, "schema", "maintenance boundary")? == BOUNDARY_SCHEMA,
        "maintenance boundary schema differs"
    );
    let source_commit = string_field(object, "source_main_commit", "maintenance boundary")?;
    ensure!(
        (source_commit.len() == 40 || source_commit.len() == 64)
            && source_commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "maintenance boundary source commit is malformed"
    );
    let observed = u64_field(object, "observed_cutoff_height", "maintenance boundary")?;
    let margin = u64_field(object, "continuity_safety_margin", "maintenance boundary")?;
    let public_max = u64_field(object, "legacy_public_max_height", "maintenance boundary")?;
    ensure!(margin > 0, "maintenance continuity margin must be positive");
    ensure!(
        observed.checked_add(margin) == Some(public_max) && public_max == SOURCE_HEIGHT,
        "maintenance boundary must bind observed cutoff plus margin to H=137145"
    );
    ensure!(
        !bool_field(object, "global_absence_claimed", "maintenance boundary")?,
        "maintenance boundary must honestly disclaim global absence"
    );
    let official = object
        .get("official_origin_scope")
        .and_then(Value::as_object)
        .context("maintenance official-origin scope is missing")?;
    ensure!(
        official
            .get("global_absence_claimed")
            .and_then(Value::as_bool)
            == Some(false),
        "maintenance official-origin scope overclaims global absence"
    );
    let threat = object
        .get("threat_model")
        .and_then(Value::as_object)
        .context("maintenance threat model is missing")?;
    ensure!(
        threat
            .get("hostile_root_containment_claimed")
            .and_then(Value::as_bool)
            == Some(false),
        "maintenance threat model overclaims hostile-root containment"
    );
    let freeze = expect_hash_field(object, "freeze_plan_sha256", "maintenance boundary")?;
    let capture = expect_hash_field(object, "capture_id", "maintenance boundary")?;
    let first = string_field(
        object,
        "first_quarantine_started_at",
        "maintenance boundary",
    )?
    .to_owned();
    let stopped =
        string_field(object, "all_controlled_stopped_at", "maintenance boundary")?.to_owned();
    validate_utc(&first, "maintenance first-quarantine timestamp")?;
    validate_utc(&stopped, "maintenance all-stopped timestamp")?;
    ensure!(
        chrono::DateTime::parse_from_rfc3339(&stopped)?
            >= chrono::DateTime::parse_from_rfc3339(&first)?,
        "maintenance all-stopped timestamp predates quarantine"
    );
    Ok(BoundaryBinding {
        projected: json!({
            "source_main_commit": source_commit,
            "observed_cutoff_height": observed,
            "continuity_safety_margin": margin,
            "legacy_public_max_height": public_max,
            "freeze_plan_sha256": freeze,
            "capture_id": capture,
            "first_quarantine_started_at": first,
            "all_controlled_stopped_at": stopped,
            "global_absence_claimed": false,
        }),
        legacy_public_max_height: public_max,
        freeze_plan_sha256: freeze,
        capture_id: capture,
        first_quarantine_started_at: first,
        all_controlled_stopped_at: stopped,
    })
}

fn validate_descriptor_projection(
    value: &Value,
    release: &ReleaseBinding,
    boundary: &BoundaryBinding,
    verified: &recovery_descriptor::VerifiedDescriptorSummary,
) -> Result<Value> {
    let object = object_exact(
        value,
        &[
            "schema_version",
            "repository",
            "release_tag",
            "release_commit",
            "recovery_manifest_sha256",
            "freeze_plan_sha256",
            "capture_id",
            "inspector_binary_sha256",
            "checkpoint_file",
            "canonical_inspection",
            "checkpoint_certificate",
            "approved_validators",
            "verified_quorum",
        ],
        "checkpoint descriptor",
    )?;
    ensure!(
        string_field(object, "schema_version", "checkpoint descriptor")? == DESCRIPTOR_SCHEMA
            && string_field(object, "repository", "checkpoint descriptor")? == release.repository
            && string_field(object, "release_tag", "checkpoint descriptor")? == release.tag
            && string_field(object, "release_commit", "checkpoint descriptor")? == release.commit,
        "checkpoint descriptor release identity differs"
    );
    ensure!(
        string_field(object, "freeze_plan_sha256", "checkpoint descriptor")?
            == boundary.freeze_plan_sha256
            && string_field(object, "capture_id", "checkpoint descriptor")? == boundary.capture_id,
        "checkpoint descriptor freeze/capture identity differs"
    );
    let recovery_manifest_sha256 =
        expect_hash_field(object, "recovery_manifest_sha256", "checkpoint descriptor")?;
    let inspector_binary_sha256 =
        expect_hash_field(object, "inspector_binary_sha256", "checkpoint descriptor")?;
    ensure!(
        release
            .files
            .get("arc-node-linux-x86_64")
            .and_then(Value::as_str)
            == Some(inspector_binary_sha256.as_str()),
        "checkpoint descriptor inspector is not the exact Linux release verifier"
    );
    let checkpoint_file = object_exact(
        object
            .get("checkpoint_file")
            .context("checkpoint file is missing")?,
        &["filename", "size_bytes", "sha256"],
        "checkpoint file",
    )?;
    ensure!(
        string_field(checkpoint_file, "filename", "checkpoint file")? == "recovery.arcchkpt"
            && u64_field(checkpoint_file, "size_bytes", "checkpoint file")? > 0,
        "checkpoint full-file record is malformed"
    );
    expect_hash_field(checkpoint_file, "sha256", "checkpoint file")?;
    let inspection = object_exact(
        object
            .get("canonical_inspection")
            .context("canonical inspection is missing")?,
        &[
            "format_version",
            "chain_id",
            "manifest_hash",
            "payload_hash",
            "network_genesis_hash",
            "full_state_root",
            "source_height",
            "source_consensus_round",
            "created_at_unix_ms",
            "source_block_hash",
            "source_state_root",
            "transition_height",
            "transition_block_hash",
            "recovery_domain",
            "recovery_epoch",
            "validator_set_id",
            "protocol_version",
            "validator_count",
            "community_rewards_v1_activation_height",
        ],
        "checkpoint canonical inspection",
    )?;
    ensure!(
        u64_field(inspection, "format_version", "checkpoint inspection")? == 1
            && string_field(inspection, "chain_id", "checkpoint inspection")? == "0x415243"
            && u64_field(inspection, "source_height", "checkpoint inspection")?
                == boundary.legacy_public_max_height
            && u64_field(inspection, "source_height", "checkpoint inspection")? == SOURCE_HEIGHT
            && u64_field(inspection, "transition_height", "checkpoint inspection")?
                == TRANSITION_HEIGHT
            && u64_field(inspection, "recovery_epoch", "checkpoint inspection")? == 1
            && u64_field(inspection, "validator_set_id", "checkpoint inspection")? == 1
            && u64_field(inspection, "validator_count", "checkpoint inspection")? == 6
            && u64_field(
                inspection,
                "community_rewards_v1_activation_height",
                "checkpoint inspection",
            )? == TRANSITION_HEIGHT,
        "checkpoint canonical inspection differs from production cutover"
    );
    ensure!(
        string_field(inspection, "manifest_hash", "checkpoint inspection")?
            == verified.manifest_hash
            && string_field(inspection, "network_genesis_hash", "checkpoint inspection")?
                == verified.network_genesis_hash
            && string_field(inspection, "recovery_domain", "checkpoint inspection")?
                == verified.recovery_domain
            && verified.source_height == SOURCE_HEIGHT
            && verified.transition_height == TRANSITION_HEIGHT
            && verified.recovery_epoch == 1
            && verified.validator_set_id == 1
            && verified.validator_count == 6,
        "checkpoint projection differs from cryptographic verification"
    );
    for field in [
        "manifest_hash",
        "payload_hash",
        "network_genesis_hash",
        "full_state_root",
        "source_block_hash",
        "source_state_root",
        "transition_block_hash",
        "recovery_domain",
    ] {
        expect_hash_field(inspection, field, "checkpoint inspection")?;
    }
    ensure!(
        string_field(inspection, "protocol_version", "checkpoint inspection")? == "3.0.0",
        "checkpoint protocol version differs"
    );
    let certificate = object
        .get("checkpoint_certificate")
        .and_then(Value::as_object)
        .context("checkpoint certificate is missing")?;
    ensure!(
        string_field(certificate, "signing_hash", "checkpoint certificate")?
            == verified.signing_hash,
        "checkpoint certificate signing hash differs from verification"
    );
    let quorum = object
        .get("verified_quorum")
        .and_then(Value::as_object)
        .context("checkpoint quorum is missing")?;
    ensure!(
        string_field(quorum, "status", "checkpoint quorum")? == "VERIFIED_QUORUM"
            && u64_field(quorum, "required_signatures", "checkpoint quorum")? == 5
            && u64_field(quorum, "verified_signature_count", "checkpoint quorum")?
                == verified.verified_signature_count as u64
            && u64_field(quorum, "validator_count", "checkpoint quorum")? == 6
            && u64_field(quorum, "signed_stake", "checkpoint quorum")? == verified.signed_stake
            && u64_field(quorum, "total_stake", "checkpoint quorum")? == verified.total_stake,
        "checkpoint quorum differs from cryptographic verification"
    );
    let mut projected = object
        .get("canonical_inspection")
        .and_then(Value::as_object)
        .expect("validated inspection")
        .clone();
    for (key, value) in [
        ("descriptor_schema", Value::String(DESCRIPTOR_SCHEMA.into())),
        (
            "recovery_manifest_sha256",
            Value::String(recovery_manifest_sha256),
        ),
        (
            "inspector_binary_sha256",
            Value::String(inspector_binary_sha256),
        ),
        (
            "checkpoint_file",
            object.get("checkpoint_file").expect("validated").clone(),
        ),
        (
            "approved_validators",
            object
                .get("approved_validators")
                .expect("validated")
                .clone(),
        ),
        (
            "checkpoint_certificate",
            object
                .get("checkpoint_certificate")
                .expect("validated")
                .clone(),
        ),
        ("certificate_cryptographically_verified", Value::Bool(true)),
        (
            "verified_quorum",
            object.get("verified_quorum").expect("validated").clone(),
        ),
    ] {
        projected.insert(key.into(), value);
    }
    Ok(Value::Object(projected))
}

fn validate_policy(
    value: &Value,
    release: &ReleaseBinding,
    boundary: &BoundaryBinding,
    boundary_sha256: &str,
    descriptor: &Value,
    descriptor_sha256: &str,
) -> Result<Value> {
    let object = object_exact(
        value,
        &[
            "schema_version",
            "repository",
            "release_tag",
            "release_commit",
            "recovery_manifest_sha256",
            "legacy_maintenance_boundary_sha256",
            "recovery_checkpoint_descriptor_sha256",
            "recovery_checkpoint_file_sha256",
            "freeze_plan_sha256",
            "capture_id",
            "first_quarantine_started_at",
            "all_controlled_stopped_at",
            "legacy_admission_cutoff_utc",
            "canonical_boundary_height",
            "required_post_cutover_min_height",
            "required_recovery_epoch",
            "required_validator_set_id",
            "required_validator_count",
            "checkpoint_format_version",
            "chain_id",
            "protocol_version",
            "payload_hash",
            "community_rewards_v1_activation_height",
            "network_genesis_hash",
            "source_block_hash",
            "source_state_root",
            "transition_block_hash",
            "full_state_root",
            "recovery_domain",
            "checkpoint_manifest_hash",
            "checkpoint_source_consensus_round",
            "checkpoint_created_at_unix_ms",
            "checkpoint_quorum",
            "legacy_validators",
            "legacy_worker_rpc",
            "uncompleted_job_disposition",
            "legacy_exit_clean_claimed",
            "legacy_restart_allowed",
            "global_legacy_absence_claimed",
            "offline_retirement_receipt_required",
            "v08_start_requires_offline_receipt",
        ],
        "cutover policy",
    )?;
    ensure!(
        string_field(object, "schema_version", "cutover policy")? == POLICY_SCHEMA
            && string_field(object, "repository", "cutover policy")? == release.repository
            && string_field(object, "release_tag", "cutover policy")? == release.tag
            && string_field(object, "release_commit", "cutover policy")? == release.commit,
        "cutover policy release identity differs"
    );
    let descriptor_object = descriptor
        .as_object()
        .context("validated descriptor projection is not an object")?;
    ensure!(
        string_field(object, "recovery_manifest_sha256", "cutover policy")?
            == string_field(
                descriptor_object,
                "recovery_manifest_sha256",
                "checkpoint projection",
            )?
            && string_field(
                object,
                "legacy_maintenance_boundary_sha256",
                "cutover policy",
            )? == boundary_sha256
            && string_field(
                object,
                "recovery_checkpoint_descriptor_sha256",
                "cutover policy",
            )? == descriptor_sha256
            && string_field(object, "recovery_checkpoint_file_sha256", "cutover policy",)?
                == descriptor_object
                    .get("checkpoint_file")
                    .and_then(Value::as_object)
                    .and_then(|record| record.get("sha256"))
                    .and_then(Value::as_str)
                    .context("descriptor checkpoint SHA-256 is missing")?,
        "cutover policy artifact bindings differ"
    );
    for (field, expected) in [
        ("freeze_plan_sha256", boundary.freeze_plan_sha256.as_str()),
        ("capture_id", boundary.capture_id.as_str()),
        (
            "first_quarantine_started_at",
            boundary.first_quarantine_started_at.as_str(),
        ),
        (
            "all_controlled_stopped_at",
            boundary.all_controlled_stopped_at.as_str(),
        ),
    ] {
        ensure!(
            string_field(object, field, "cutover policy")? == expected,
            "cutover policy {field} differs from maintenance boundary"
        );
    }
    ensure!(
        string_field(object, "legacy_admission_cutoff_utc", "cutover policy")?
            == boundary.all_controlled_stopped_at,
        "cutover admission cutoff differs from controlled stop"
    );
    ensure!(
        u64_field(object, "canonical_boundary_height", "cutover policy")? == SOURCE_HEIGHT
            && u64_field(object, "required_post_cutover_min_height", "cutover policy",)?
                == TRANSITION_HEIGHT
            && u64_field(object, "required_recovery_epoch", "cutover policy")? == 1
            && u64_field(object, "required_validator_set_id", "cutover policy")? == 1
            && u64_field(object, "required_validator_count", "cutover policy")? == 6,
        "cutover policy height/epoch/validator constants differ"
    );
    for (policy_field, descriptor_field) in [
        ("checkpoint_format_version", "format_version"),
        ("chain_id", "chain_id"),
        ("protocol_version", "protocol_version"),
        ("payload_hash", "payload_hash"),
        (
            "community_rewards_v1_activation_height",
            "community_rewards_v1_activation_height",
        ),
        ("network_genesis_hash", "network_genesis_hash"),
        ("source_block_hash", "source_block_hash"),
        ("source_state_root", "source_state_root"),
        ("transition_block_hash", "transition_block_hash"),
        ("full_state_root", "full_state_root"),
        ("recovery_domain", "recovery_domain"),
        ("checkpoint_manifest_hash", "manifest_hash"),
        (
            "checkpoint_source_consensus_round",
            "source_consensus_round",
        ),
        ("checkpoint_created_at_unix_ms", "created_at_unix_ms"),
    ] {
        ensure!(
            object.get(policy_field) == descriptor_object.get(descriptor_field),
            "cutover policy {policy_field} differs from checkpoint"
        );
    }
    ensure!(
        object.get("checkpoint_quorum") == descriptor_object.get("verified_quorum"),
        "cutover policy quorum differs from checkpoint"
    );
    let validators = object
        .get("legacy_validators")
        .and_then(Value::as_array)
        .context("cutover policy validators are missing")?;
    ensure!(
        validators.len() == FLEET.len(),
        "cutover policy must bind six validators"
    );
    for (index, (validator, (name, host))) in validators.iter().zip(FLEET).enumerate() {
        let row = object_exact(
            validator,
            &["name", "host", "origin", "address", "stake"],
            &format!("cutover validator #{index}"),
        )?;
        ensure!(
            string_field(row, "name", "cutover validator")? == name
                && string_field(row, "host", "cutover validator")? == host
                && string_field(row, "origin", "cutover validator")?
                    == format!("http://{host}:9090")
                && u64_field(row, "stake", "cutover validator")? > 0,
            "cutover validator #{index} identity differs"
        );
        expect_hash_field(row, "address", "cutover validator")?;
    }
    ensure!(
        object.get("legacy_validators") == descriptor_object.get("approved_validators"),
        "cutover policy validator inventory differs from checkpoint"
    );
    ensure!(
        object.get("legacy_worker_rpc")
            == Some(&json!({
                "claim_path": "/community/claim_work",
                "submit_path": "/community/submit_work",
                "listener_ports": [9090, 3001],
            })),
        "cutover worker RPC contract differs"
    );
    ensure!(
        string_field(object, "uncompleted_job_disposition", "cutover policy")? == JOBS_DISPOSITION
            && !bool_field(object, "legacy_exit_clean_claimed", "cutover policy")?
            && !bool_field(object, "legacy_restart_allowed", "cutover policy")?
            && !bool_field(object, "global_legacy_absence_claimed", "cutover policy")?
            && bool_field(
                object,
                "offline_retirement_receipt_required",
                "cutover policy",
            )?
            && bool_field(
                object,
                "v08_start_requires_offline_receipt",
                "cutover policy",
            )?,
        "cutover retirement/start requirements are dishonest"
    );
    Ok(json!({
        "schema_version": POLICY_SCHEMA,
        "repository": release.repository,
        "release_tag": release.tag,
        "release_commit": release.commit,
        "recovery_manifest_sha256": object["recovery_manifest_sha256"],
        "legacy_maintenance_boundary_sha256": boundary_sha256,
        "recovery_checkpoint_descriptor_sha256": descriptor_sha256,
        "recovery_checkpoint_file_sha256": object["recovery_checkpoint_file_sha256"],
        "canonical_boundary_height": SOURCE_HEIGHT,
        "required_post_cutover_min_height": TRANSITION_HEIGHT,
        "required_recovery_epoch": 1,
        "required_validator_set_id": 1,
        "required_validator_count": 6,
        "checkpoint_format_version": 1,
        "chain_id": "0x415243",
        "payload_hash": descriptor_object["payload_hash"],
        "community_rewards_v1_activation_height": TRANSITION_HEIGHT,
        "legacy_validators": validators,
        "legacy_worker_rpc": object["legacy_worker_rpc"],
        "uncompleted_job_disposition": JOBS_DISPOSITION,
        "legacy_exit_clean_claimed": false,
        "legacy_restart_allowed": false,
        "global_legacy_absence_claimed": false,
        "offline_retirement_receipt_required": true,
        "v08_start_requires_offline_receipt": true,
    }))
}

fn parse_stake_zero_argv(argv: &[String], data_dir: &Path) -> Result<Value> {
    ensure!(!argv.is_empty(), "legacy command line is empty");
    let mut stakes = Vec::new();
    let mut minimums = Vec::new();
    let mut data_dirs = Vec::new();
    let mut community = false;
    let mut index = 1usize;
    while index < argv.len() {
        let token = &argv[index];
        ensure!(
            token != "--config" && token != "-c" && !token.starts_with("--config="),
            "legacy retirement forbids config-file overrides"
        );
        ensure!(
            token != "--benchmark" && token != "--proposer-mode",
            "legacy retirement rejects active role {token}"
        );
        if token == "--community-mode" {
            community = true;
            index += 1;
            continue;
        }
        let mut matched = false;
        for (name, values) in [
            ("--stake", &mut stakes),
            ("--min-stake", &mut minimums),
            ("--data-dir", &mut data_dirs),
        ] {
            if token == name {
                ensure!(index + 1 < argv.len(), "legacy {name} has no value");
                values.push(argv[index + 1].clone());
                index += 2;
                matched = true;
                break;
            }
            if let Some(value) = token.strip_prefix(&format!("{name}=")) {
                values.push(value.to_owned());
                index += 1;
                matched = true;
                break;
            }
        }
        if !matched {
            index += 1;
        }
    }
    ensure!(
        stakes == ["0"] && minimums == ["0"],
        "legacy process must explicitly contain exactly one --stake 0 and --min-stake 0"
    );
    ensure!(
        data_dirs.len() == 1,
        "legacy process must contain one --data-dir"
    );
    let selected = Path::new(&data_dirs[0]);
    require_absolute_normal(selected, "legacy process --data-dir")?;
    ensure!(
        selected == data_dir,
        "legacy process data directory differs"
    );
    Ok(json!({
        "stake": 0,
        "minimum_stake": 0,
        "data_dir": path_string(selected, "legacy process data directory")?,
        "community_mode_explicit": community,
        "community_mode_effective": true,
    }))
}

fn validate_supervisor(
    value: &Value,
    data_dir: &Path,
    executable: &Path,
    executable_sha256: &str,
) -> Result<(Value, FileRecord)> {
    let object = object_exact(
        value,
        &[
            "schema",
            "kind",
            "source_path",
            "source_sha256",
            "executable_path",
            "executable_sha256",
            "argv",
        ],
        "legacy supervisor binding",
    )?;
    ensure!(
        string_field(object, "schema", "legacy supervisor")? == SUPERVISOR_SCHEMA,
        "legacy supervisor schema differs"
    );
    ensure!(
        matches!(
            string_field(object, "kind", "legacy supervisor")?,
            "systemd" | "launchd" | "manual"
        ),
        "legacy supervisor kind is unsupported"
    );
    ensure!(
        string_field(object, "executable_path", "legacy supervisor")?
            == path_string(executable, "legacy executable")?
            && string_field(object, "executable_sha256", "legacy supervisor")? == executable_sha256,
        "legacy supervisor executable binding differs"
    );
    let argv = object
        .get("argv")
        .and_then(Value::as_array)
        .context("legacy supervisor argv is malformed")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("legacy supervisor argv contains a non-string")
        })
        .collect::<Result<Vec<_>>>()?;
    let semantics = parse_stake_zero_argv(&argv, data_dir)?;
    let source_path = PathBuf::from(string_field(object, "source_path", "legacy supervisor")?);
    let source_sha256 = expect_hash_field(object, "source_sha256", "legacy supervisor")?;
    let (_bytes, source_record) =
        stable_read(&source_path, "legacy supervisor source", MAX_JSON_BYTES)?;
    ensure!(
        source_record.sha256 == source_sha256,
        "legacy supervisor source hash differs"
    );
    Ok((semantics, source_record))
}

fn stable_hash_file(path: &Path, label: &str, maximum: u64) -> Result<FileRecord> {
    require_absolute_normal(path, label)?;
    reject_symlink_ancestors(path, true, label)?;
    let before_path = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot stat {label} {}", path.display()))?;
    ensure!(
        !before_path.file_type().is_symlink(),
        "{label} must not be a symlink"
    );
    validate_readable_metadata(&before_path, path, label)?;
    ensure!(
        before_path.len() > 0 && before_path.len() <= maximum,
        "{label} size is outside its bounded contract"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot no-follow open {label}"))?;
    let before = file.metadata().context("cannot inspect opened file")?;
    ensure!(
        metadata_identity(&before) == metadata_identity(&before_path),
        "{label} changed before open"
    );
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut observed = 0u64;
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("cannot hash {label}"))?;
        if count == 0 {
            break;
        }
        observed = observed
            .checked_add(count as u64)
            .context("hashed file length overflow")?;
        hasher.update(&buffer[..count]);
    }
    let after = file.metadata().context("cannot reinspect opened file")?;
    let after_path =
        std::fs::symlink_metadata(path).with_context(|| format!("cannot restat {label}"))?;
    ensure!(
        observed == before.len()
            && metadata_identity(&after) == metadata_identity(&before)
            && metadata_identity(&after_path) == metadata_identity(&before),
        "{label} changed while it was hashed"
    );
    reject_symlink_ancestors(path, true, label)?;
    let identity = metadata_identity(&before);
    Ok(FileRecord {
        path: path_string(path, label)?,
        device: identity.0,
        inode: identity.1,
        mode: identity.2,
        uid: identity.3,
        gid: identity.4,
        nlink: identity.5,
        size: identity.6,
        mtime_ns: identity.7,
        ctime_ns: identity.8,
        sha256: hex::encode(hasher.finalize()),
    })
}

fn directory_record(path: &Path, label: &str) -> Result<Value> {
    require_absolute_normal(path, label)?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot stat {label} {}", path.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a non-symlink directory"
    );
    #[cfg(unix)]
    {
        ensure!(
            metadata.mode() & 0o022 == 0,
            "{label} must not be group/world writable"
        );
        let effective = unsafe { libc::geteuid() };
        ensure!(
            metadata.uid() == effective || metadata.uid() == 0,
            "{label} is not owned by the effective user or root"
        );
    }
    let identity = metadata_identity(&metadata);
    Ok(json!({
        "path": path_string(path, label)?,
        "device": identity.0,
        "inode": identity.1,
        "mode": identity.2,
        "uid": identity.3,
        "gid": identity.4,
        "nlink": identity.5,
        "size": identity.6,
        "mtime_ns": identity.7,
        "ctime_ns": identity.8,
    }))
}

fn reject_symlink_ancestors(path: &Path, include_leaf: bool, label: &str) -> Result<()> {
    require_absolute_normal(path, label)?;
    let target = if include_leaf {
        path
    } else {
        path.parent().context("path has no parent")?
    };
    let mut current = PathBuf::new();
    for component in target.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::RootDir | Component::Prefix(_)) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&current)
            .with_context(|| format!("cannot inspect {label} ancestor {}", current.display()))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "{label} contains a symlinked ancestor: {}",
            current.display()
        );
    }
    Ok(())
}

fn ensure_disjoint_absent_v08(v08: &Path, legacy: &Path) -> Result<()> {
    require_absolute_normal(v08, "v0.8 data directory")?;
    require_absolute_normal(legacy, "legacy data directory")?;
    reject_symlink_ancestors(legacy, true, "legacy data directory")?;
    let legacy_physical =
        std::fs::canonicalize(legacy).context("cannot resolve physical legacy data directory")?;
    match std::fs::symlink_metadata(v08) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => bail!("v0.8 data directory must remain absent until receipt"),
        Err(error) => return Err(error).context("cannot inspect v0.8 data directory"),
    }
    reject_symlink_ancestors(v08, false, "v0.8 data directory")?;
    let v08_parent = v08.parent().context("v0.8 data directory has no parent")?;
    directory_record(v08_parent, "v0.8 data parent")?;
    let v08_physical = std::fs::canonicalize(v08_parent)
        .context("cannot resolve physical v0.8 data parent")?
        .join(v08.file_name().context("v0.8 data directory has no name")?);
    ensure!(
        !v08_physical.starts_with(&legacy_physical) && !legacy_physical.starts_with(&v08_physical),
        "v0.8 and legacy data directories must be physically disjoint"
    );
    Ok(())
}

fn ensure_output_path(path: &Path, legacy: &Path, label: &str) -> Result<()> {
    require_absolute_normal(path, label)?;
    reject_symlink_ancestors(legacy, true, "legacy data directory")?;
    let legacy_physical =
        std::fs::canonicalize(legacy).context("cannot resolve physical legacy data directory")?;
    reject_symlink_ancestors(path, false, label)?;
    let parent = path.parent().context("output has no parent")?;
    directory_record(parent, "output parent")?;
    let physical = std::fs::canonicalize(parent)
        .with_context(|| format!("cannot resolve {label} parent"))?
        .join(path.file_name().context("output path has no name")?);
    ensure!(
        !physical.starts_with(&legacy_physical),
        "{label} must be physically outside the legacy tree"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TreeSnapshot {
    root_sha256: String,
    root: Value,
    entries: Vec<Value>,
    entry_count: usize,
    total_file_bytes: u64,
    state_wal_sha256: String,
}

fn tree_snapshot(root: &Path) -> Result<TreeSnapshot> {
    reject_symlink_ancestors(root, true, "legacy data directory")?;
    let root_record = directory_record(root, "legacy data directory")?;
    let mut entries = Vec::new();
    let mut total = 0u64;
    let mut state_wal_sha256 = None;

    fn walk(
        root: &Path,
        directory: &Path,
        entries: &mut Vec<Value>,
        total: &mut u64,
        state_wal_sha256: &mut Option<String>,
    ) -> Result<()> {
        let before = std::fs::symlink_metadata(directory)
            .with_context(|| format!("cannot stat legacy directory {}", directory.display()))?;
        ensure!(
            before.is_dir() && !before.file_type().is_symlink(),
            "legacy directory changed into a non-directory: {}",
            directory.display()
        );
        let mut children = std::fs::read_dir(directory)
            .with_context(|| format!("cannot enumerate legacy directory {}", directory.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            ensure!(
                entries.len() < MAX_TREE_ENTRIES,
                "legacy data tree has too many entries"
            );
            let name = child.file_name();
            ensure!(
                name != OsStr::new(".") && name != OsStr::new(".."),
                "legacy data tree contains an unsafe entry name"
            );
            let path = child.path();
            let relative = path
                .strip_prefix(root)
                .context("legacy entry escaped its root")?;
            let relative = path_string(relative, "legacy relative path")?;
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| format!("cannot stat legacy entry {relative}"))?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "legacy data tree contains a symlink: {relative}"
            );
            #[cfg(unix)]
            {
                ensure!(
                    metadata.mode() & 0o022 == 0,
                    "legacy data entry is group/world writable: {relative}"
                );
                let effective = unsafe { libc::geteuid() };
                ensure!(
                    metadata.uid() == effective || metadata.uid() == 0,
                    "legacy data entry is not owned by the effective user or root: {relative}"
                );
            }
            let identity = metadata_identity(&metadata);
            let mut record = json!({
                "path": relative,
                "device": identity.0,
                "inode": identity.1,
                "mode": identity.2,
                "uid": identity.3,
                "gid": identity.4,
                "nlink": identity.5,
                "mtime_ns": identity.7,
                "ctime_ns": identity.8,
            });
            if metadata.is_dir() {
                record
                    .as_object_mut()
                    .expect("object")
                    .insert("kind".into(), "directory".into());
                entries.push(record);
                walk(root, &path, entries, total, state_wal_sha256)?;
            } else if metadata.is_file() {
                let file = stable_hash_file(&path, "legacy data file", MAX_TREE_BYTES)?;
                ensure!(
                    (
                        file.device,
                        file.inode,
                        file.mode,
                        file.uid,
                        file.gid,
                        file.nlink,
                        file.size,
                        file.mtime_ns,
                        file.ctime_ns,
                    ) == (
                        identity.0, identity.1, identity.2, identity.3, identity.4, identity.5,
                        identity.6, identity.7, identity.8,
                    ),
                    "legacy data file changed between enumeration and hashing: {relative}"
                );
                *total = total
                    .checked_add(file.size)
                    .context("legacy tree byte count overflow")?;
                ensure!(
                    *total <= MAX_TREE_BYTES,
                    "legacy data tree exceeds byte bound"
                );
                let object = record.as_object_mut().expect("object");
                object.insert("kind".into(), "file".into());
                object.insert("size".into(), file.size.into());
                object.insert("sha256".into(), file.sha256.clone().into());
                if relative == "state.wal" {
                    ensure!(state_wal_sha256.is_none(), "legacy tree repeats state.wal");
                    *state_wal_sha256 = Some(file.sha256);
                }
                entries.push(record);
            } else {
                bail!("legacy data tree contains a special file: {relative}");
            }
        }
        let after = std::fs::symlink_metadata(directory)
            .with_context(|| format!("cannot restat legacy directory {}", directory.display()))?;
        ensure!(
            after.is_dir()
                && !after.file_type().is_symlink()
                && metadata_identity(&after) == metadata_identity(&before),
            "legacy directory changed while inspected: {}",
            directory.display()
        );
        Ok(())
    }

    walk(root, root, &mut entries, &mut total, &mut state_wal_sha256)?;
    let state_wal_sha256 = state_wal_sha256.context("legacy tree has no top-level state.wal")?;
    let semantic = json!({
        "schema": "arc.migration.legacy-v07-data-tree.v1",
        "root": root_record,
        "entries": entries,
        "entry_count": entries.len(),
        "total_file_bytes": total,
    });
    let root_sha256 = sha256(&canonical_bytes(&semantic)?);
    Ok(TreeSnapshot {
        root_sha256,
        root: semantic["root"].clone(),
        entries,
        entry_count: semantic["entry_count"].as_u64().expect("count") as usize,
        total_file_bytes: total,
        state_wal_sha256,
    })
}

fn wal_prefix_record(path: &Path) -> Result<Value> {
    let record = stable_hash_file(path, "legacy state WAL", MAX_TREE_BYTES)?;
    ensure!(record.size > 0, "legacy state WAL is empty");
    Ok(json!({
        "path": record.path,
        "device": record.device,
        "inode": record.inode,
        "mode": record.mode,
        "uid": record.uid,
        "gid": record.gid,
        "nlink": record.nlink,
        "observed_prefix_bytes": record.size,
        "observed_prefix_sha256": record.sha256,
    }))
}

fn verify_wal_prefix(path: &Path, expected: &Value) -> Result<()> {
    let expected = expected
        .as_object()
        .context("intent WAL prefix is malformed")?;
    let current = stable_hash_file(path, "legacy state WAL", MAX_TREE_BYTES)?;
    for (field, actual) in [
        ("path", Value::String(current.path.clone())),
        ("device", current.device.into()),
        ("inode", current.inode.into()),
        ("mode", current.mode.into()),
        ("uid", current.uid.into()),
        ("gid", current.gid.into()),
        ("nlink", current.nlink.into()),
    ] {
        ensure!(
            expected.get(field) == Some(&actual),
            "legacy WAL {field} differs from intent"
        );
    }
    let prefix_bytes = u64_field(expected, "observed_prefix_bytes", "intent WAL")?;
    let prefix_sha256 = expect_hash_field(expected, "observed_prefix_sha256", "intent WAL")?;
    ensure!(
        current.size >= prefix_bytes,
        "legacy WAL shrank after intent"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut file = options.open(path).context("cannot reopen legacy WAL")?;
    let reopened = file
        .metadata()
        .context("cannot inspect reopened legacy WAL")?;
    let expected_identity = (
        u64_field(expected, "device", "intent WAL")?,
        u64_field(expected, "inode", "intent WAL")?,
        u64_field(expected, "mode", "intent WAL")? as u32,
        u64_field(expected, "uid", "intent WAL")? as u32,
        u64_field(expected, "gid", "intent WAL")? as u32,
        u64_field(expected, "nlink", "intent WAL")?,
    );
    let reopened_identity = metadata_identity(&reopened);
    ensure!(
        (
            reopened_identity.0,
            reopened_identity.1,
            reopened_identity.2,
            reopened_identity.3,
            reopened_identity.4,
            reopened_identity.5,
        ) == expected_identity
            && reopened.len() >= prefix_bytes,
        "legacy WAL identity changed before prefix verification"
    );
    let mut take = (&mut file).take(prefix_bytes);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    loop {
        let count = take.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .context("legacy WAL prefix length overflow")?;
        hasher.update(&buffer[..count]);
    }
    ensure!(
        copied == prefix_bytes,
        "legacy WAL shrank while checking prefix"
    );
    ensure!(
        hex::encode(hasher.finalize()) == prefix_sha256,
        "legacy WAL changed inside its sealed intent prefix"
    );
    let reopened_after = file.metadata().context("cannot reinspect legacy WAL")?;
    let path_after = std::fs::symlink_metadata(path).context("cannot restat legacy WAL")?;
    ensure!(
        metadata_identity(&reopened_after) == metadata_identity(&reopened)
            && metadata_identity(&path_after) == metadata_identity(&reopened),
        "legacy WAL changed while its prefix was verified"
    );
    reject_symlink_ancestors(path, true, "legacy state WAL")?;
    Ok(())
}

fn publish_create_only(path: &Path, value: &Value, label: &str) -> Result<String> {
    let bytes = canonical_bytes(value)?;
    let digest = sha256(&bytes);
    if path.exists() {
        let (existing, existing_bytes, _) =
            load_canonical_json(path, label, &digest, MAX_JSON_BYTES)?;
        ensure!(
            existing == *value && existing_bytes == bytes,
            "existing {label} differs"
        );
        return Ok(digest);
    }
    let published = arc_crypto::secret_file::durably_publish_new_private(path, &bytes)
        .with_context(|| format!("cannot publish {label} {}", path.display()))?;
    if !published {
        let (existing, existing_bytes, _) =
            load_canonical_json(path, label, &digest, MAX_JSON_BYTES)?;
        ensure!(
            existing == *value && existing_bytes == bytes,
            "concurrent {label} differs"
        );
    }
    Ok(digest)
}

#[cfg(target_os = "linux")]
fn open_process_executable(path: &Path, display_path: String) -> Result<FileRecord> {
    let mut file = File::open(path)
        .with_context(|| format!("cannot open process executable {}", path.display()))?;
    let before = file
        .metadata()
        .context("cannot inspect process executable")?;
    ensure!(
        before.is_file() && before.len() > 0 && before.len() <= MAX_EXECUTABLE_BYTES,
        "process executable is not a bounded regular file"
    );
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut observed = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        observed = observed
            .checked_add(count as u64)
            .context("process executable length overflow")?;
        hasher.update(&buffer[..count]);
    }
    let after = file
        .metadata()
        .context("cannot reinspect process executable")?;
    ensure!(
        observed == before.len() && metadata_identity(&after) == metadata_identity(&before),
        "process executable changed while hashed"
    );
    let identity = metadata_identity(&before);
    Ok(FileRecord {
        path: display_path,
        device: identity.0,
        inode: identity.1,
        mode: identity.2,
        uid: identity.3,
        gid: identity.4,
        nlink: identity.5,
        size: identity.6,
        mtime_ns: identity.7,
        ctime_ns: identity.8,
        sha256: hex::encode(hasher.finalize()),
    })
}

#[cfg(target_os = "linux")]
fn linux_boot_id() -> Result<String> {
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("cannot read Linux boot ID")?;
    let value = value.trim().to_owned();
    ensure!(value.len() == 36, "Linux boot ID is malformed");
    Ok(value)
}

#[cfg(target_os = "linux")]
fn linux_tcp_table(path: &Path, family: &str) -> Result<Vec<ListenerEndpoint>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read kernel TCP table {}", path.display()))?;
    let mut rows = Vec::new();
    for line in text.lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 10 || fields[3] != "0A" {
            continue;
        }
        let (address, port) = fields[1]
            .split_once(':')
            .context("kernel TCP address is malformed")?;
        rows.push(ListenerEndpoint {
            family: family.to_owned(),
            address_hex: address.to_ascii_uppercase(),
            port: u16::from_str_radix(port, 16).context("kernel TCP port is malformed")?,
            inode: fields[9].parse().context("kernel TCP inode is malformed")?,
        });
    }
    Ok(rows)
}

#[cfg(target_os = "linux")]
fn linux_listeners_in(net_root: &Path) -> Result<Vec<ListenerEndpoint>> {
    let mut rows = linux_tcp_table(&net_root.join("tcp"), "tcp4")?;
    let tcp6 = net_root.join("tcp6");
    match linux_tcp_table(&tcp6, "tcp6") {
        Ok(ipv6) => rows.extend(ipv6),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|source| source.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    Ok(rows)
}

#[cfg(target_os = "linux")]
fn linux_observe_process(
    pid: u32,
    include_listener_inventory: bool,
) -> Result<Option<ProcessObservation>> {
    let root = PathBuf::from(format!("/proc/{pid}"));
    let root_metadata = match std::fs::metadata(&root) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot inspect process directory"),
    };
    let stat = match std::fs::read_to_string(root.join("stat")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot read process stat"),
    };
    let close = stat.rfind(')').context("process stat is malformed")?;
    let fields = stat
        .get(close + 2..)
        .context("process stat suffix is malformed")?
        .split_whitespace()
        .collect::<Vec<_>>();
    ensure!(fields.len() > 19, "process stat omits start ticks");
    let start_ticks = fields[19]
        .parse::<u64>()
        .context("process start ticks are malformed")?;
    ensure!(start_ticks > 0, "process start ticks are zero");
    let exe_link = root.join("exe");
    let executable_path = match std::fs::read_link(&exe_link) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot resolve process executable"),
    };
    let executable = open_process_executable(
        &exe_link,
        path_string(&executable_path, "process executable path")?,
    )?;
    let command = std::fs::read(root.join("cmdline")).context("cannot read process argv")?;
    let argv = command
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| {
            std::str::from_utf8(part)
                .map(str::to_owned)
                .context("process argv is not UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(!argv.is_empty(), "process argv is empty");
    let cwd = std::fs::read_link(root.join("cwd"))
        .ok()
        .map(|path| path_string(&path, "process cwd"))
        .transpose()?;
    let listeners = if include_listener_inventory {
        let socket_inodes = match std::fs::read_dir(root.join("fd")) {
            Ok(entries) => entries
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| std::fs::read_link(entry.path()).ok())
                .filter_map(|target| {
                    let text = target.to_string_lossy();
                    text.strip_prefix("socket:[")
                        .and_then(|value| value.strip_suffix(']'))
                        .and_then(|value| value.parse::<u64>().ok())
                })
                .collect::<HashSet<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("cannot inspect process file descriptors"),
        };
        linux_listeners_in(&root.join("net"))?
            .into_iter()
            .filter(|row| socket_inodes.contains(&row.inode))
            .collect()
    } else {
        Vec::new()
    };
    let stat_after = match std::fs::read_to_string(root.join("stat")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot reread process stat"),
    };
    let close_after = stat_after
        .rfind(')')
        .context("repeated process stat is malformed")?;
    let fields_after = stat_after
        .get(close_after + 2..)
        .context("repeated process stat suffix is malformed")?
        .split_whitespace()
        .collect::<Vec<_>>();
    ensure!(
        fields_after.len() > 19,
        "repeated process stat omits start ticks"
    );
    ensure!(
        fields_after[19]
            .parse::<u64>()
            .context("repeated process start ticks are malformed")?
            == start_ticks,
        "process identity changed while inspected"
    );
    Ok(Some(ProcessObservation {
        pid,
        boot_id: linux_boot_id()?,
        start_ticks,
        uid: root_metadata.uid(),
        gid: root_metadata.gid(),
        executable,
        argv,
        cwd,
        listeners,
    }))
}

#[cfg(target_os = "linux")]
fn system_observe_process(pid: u32) -> Result<Option<ProcessObservation>> {
    linux_observe_process(pid, true)
}

#[cfg(target_os = "linux")]
fn system_all_process_ids() -> Result<Vec<u32>> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc").context("cannot enumerate Linux processes")? {
        let entry = entry?;
        if let Some(value) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            pids.push(value);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

#[cfg(target_os = "linux")]
fn system_active_listener_endpoints() -> Result<Vec<ListenerEndpoint>> {
    linux_listeners_in(Path::new("/proc/net"))
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MacProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [libc::c_char; 16],
    pbi_name: [libc::c_char; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_listpids(
        process_type: u32,
        type_info: u32,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
    fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, buffersize: u32) -> libc::c_int;
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
}

#[cfg(target_os = "macos")]
fn mac_sysctl_string(name: &[u8]) -> Result<String> {
    ensure!(
        name.last() == Some(&0),
        "sysctl name must be NUL terminated"
    );
    let mut size = 0usize;
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    ensure!(status == 0 && size > 1, "cannot size macOS sysctl value");
    let mut bytes = vec![0u8; size];
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast(),
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    ensure!(status == 0, "cannot read macOS sysctl value");
    bytes.truncate(size);
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes).context("macOS sysctl value is not UTF-8")
}

#[cfg(target_os = "macos")]
fn mac_process_argv(pid: u32) -> Result<Vec<String>> {
    let mut mib = [libc::CTL_KERN, 49, pid as libc::c_int];
    let mut size = 0usize;
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    ensure!(status == 0 && size >= 4, "cannot size macOS process argv");
    let mut bytes = vec![0u8; size];
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    ensure!(status == 0 && size >= 4, "cannot read macOS process argv");
    bytes.truncate(size);
    let argc = i32::from_ne_bytes(bytes[..4].try_into().expect("four bytes"));
    ensure!(argc > 0, "macOS process argv has invalid argc");
    let mut cursor = 4usize;
    while cursor < bytes.len() && bytes[cursor] != 0 {
        cursor += 1;
    }
    while cursor < bytes.len() && bytes[cursor] == 0 {
        cursor += 1;
    }
    let mut argv = Vec::with_capacity(argc as usize);
    while argv.len() < argc as usize && cursor < bytes.len() {
        let end = bytes[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| cursor + offset)
            .unwrap_or(bytes.len());
        argv.push(
            std::str::from_utf8(&bytes[cursor..end])
                .context("macOS process argv is not UTF-8")?
                .to_owned(),
        );
        cursor = end.saturating_add(1);
    }
    ensure!(
        argv.len() == argc as usize,
        "macOS process argv is truncated"
    );
    Ok(argv)
}

#[cfg(target_os = "macos")]
fn mac_lsof_listeners(pid: Option<u32>) -> Result<Vec<ListenerEndpoint>> {
    let mut command = std::process::Command::new("/usr/sbin/lsof");
    command.args(["-nP", "-sTCP:LISTEN", "-Ffn"]);
    if let Some(pid) = pid {
        command.args(["-a", "-p", &pid.to_string(), "-iTCP"]);
    } else {
        command.args(["-iTCP:9090", "-iTCP:3001"]);
    }
    let output = command
        .output()
        .context("cannot execute trusted macOS lsof")?;
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(
        output.status.success(),
        "macOS lsof listener inspection failed"
    );
    let text = std::str::from_utf8(&output.stdout).context("lsof output is not UTF-8")?;
    let mut fd = 1u64;
    let mut endpoints = Vec::new();
    for line in text.lines() {
        if let Some(raw) = line.strip_prefix('f') {
            fd = raw
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>()
                .parse::<u64>()
                .unwrap_or(1)
                .max(1);
        } else if let Some(raw) = line.strip_prefix('n') {
            let endpoint = raw.split(" (LISTEN)").next().unwrap_or(raw);
            let port_text = endpoint
                .rsplit_once(':')
                .map(|(_, port)| port)
                .context("lsof listener endpoint has no port")?;
            let port = port_text.parse::<u16>().context("lsof port is malformed")?;
            let ipv6 = endpoint.starts_with('[') || endpoint.matches(':').count() > 1;
            endpoints.push(ListenerEndpoint {
                family: if ipv6 { "tcp6" } else { "tcp4" }.into(),
                address_hex: if ipv6 { "0".repeat(32) } else { "0".repeat(8) },
                port,
                inode: fd,
            });
        }
    }
    endpoints.sort();
    endpoints.dedup_by(|left, right| {
        left.family == right.family
            && left.address_hex == right.address_hex
            && left.port == right.port
    });
    Ok(endpoints)
}

#[cfg(target_os = "macos")]
fn mac_observe_process(
    pid: u32,
    include_listener_inventory: bool,
) -> Result<Option<ProcessObservation>> {
    let mut info = std::mem::MaybeUninit::<MacProcBsdInfo>::zeroed();
    let received = unsafe {
        proc_pidinfo(
            pid as libc::c_int,
            3,
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<MacProcBsdInfo>() as libc::c_int,
        )
    };
    if received <= 0 {
        let error = std::io::Error::last_os_error();
        return if matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::ENOENT)) {
            Ok(None)
        } else {
            Err(error).context("cannot inspect macOS process identity")
        };
    }
    ensure!(
        received as usize == std::mem::size_of::<MacProcBsdInfo>(),
        "macOS process identity record is truncated"
    );
    let info = unsafe { info.assume_init() };
    let mut path_buffer = vec![0u8; 4096];
    let length = unsafe {
        proc_pidpath(
            pid as libc::c_int,
            path_buffer.as_mut_ptr().cast(),
            path_buffer.len() as u32,
        )
    };
    if length <= 0 {
        let error = std::io::Error::last_os_error();
        return if matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::ENOENT)) {
            Ok(None)
        } else {
            Err(error).context("cannot resolve macOS process executable")
        };
    }
    path_buffer.truncate(length as usize);
    let executable_path = PathBuf::from(
        std::str::from_utf8(&path_buffer)
            .context("macOS process path is not UTF-8")?
            .trim_end_matches('\0'),
    );
    let executable = stable_hash_file(
        &executable_path,
        "macOS process executable",
        MAX_EXECUTABLE_BYTES,
    )?;
    let start_ticks = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(info.pbi_start_tvusec))
        .context("macOS process start identity overflow")?;
    ensure!(start_ticks > 0, "macOS process start identity is zero");
    let observation = ProcessObservation {
        pid,
        boot_id: mac_sysctl_string(b"kern.bootsessionuuid\0")?.to_ascii_lowercase(),
        start_ticks,
        uid: info.pbi_uid,
        gid: info.pbi_gid,
        executable,
        argv: mac_process_argv(pid)?,
        cwd: None,
        listeners: if include_listener_inventory {
            mac_lsof_listeners(Some(pid))?
        } else {
            Vec::new()
        },
    };
    let mut repeated = std::mem::MaybeUninit::<MacProcBsdInfo>::zeroed();
    let repeated_size = unsafe {
        proc_pidinfo(
            pid as libc::c_int,
            3,
            0,
            repeated.as_mut_ptr().cast(),
            std::mem::size_of::<MacProcBsdInfo>() as libc::c_int,
        )
    };
    if repeated_size <= 0 {
        let error = std::io::Error::last_os_error();
        return if matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::ENOENT)) {
            Ok(None)
        } else {
            Err(error).context("cannot repeat macOS process identity inspection")
        };
    }
    ensure!(
        repeated_size as usize == std::mem::size_of::<MacProcBsdInfo>(),
        "repeated macOS process identity is truncated"
    );
    let repeated = unsafe { repeated.assume_init() };
    ensure!(
        repeated.pbi_start_tvsec == info.pbi_start_tvsec
            && repeated.pbi_start_tvusec == info.pbi_start_tvusec
            && repeated.pbi_uid == info.pbi_uid
            && repeated.pbi_gid == info.pbi_gid,
        "macOS process identity changed while inspected"
    );
    Ok(Some(observation))
}

#[cfg(target_os = "macos")]
fn system_observe_process(pid: u32) -> Result<Option<ProcessObservation>> {
    mac_observe_process(pid, true)
}

#[cfg(target_os = "macos")]
fn mac_process_ids_for_uid(uid: u32) -> Result<Vec<u32>> {
    const PROC_UID_ONLY: u32 = 4;
    let required_bytes = unsafe { proc_listpids(PROC_UID_ONLY, uid, std::ptr::null_mut(), 0) };
    ensure!(
        required_bytes > 0,
        "cannot size macOS effective-user process list"
    );
    let mut capacity = required_bytes as usize / std::mem::size_of::<i32>() + 128;
    let mut pids;
    loop {
        pids = vec![0i32; capacity];
        let byte_capacity = pids
            .len()
            .checked_mul(std::mem::size_of::<i32>())
            .and_then(|value| libc::c_int::try_from(value).ok())
            .context("macOS process list capacity exceeds c_int")?;
        let returned =
            unsafe { proc_listpids(PROC_UID_ONLY, uid, pids.as_mut_ptr().cast(), byte_capacity) };
        ensure!(
            returned >= 0,
            "cannot read macOS effective-user process list"
        );
        let returned = returned as usize;
        ensure!(
            returned.is_multiple_of(std::mem::size_of::<i32>()),
            "macOS process list byte count is malformed"
        );
        if returned < byte_capacity as usize {
            pids.truncate(returned / std::mem::size_of::<i32>());
            break;
        }
        capacity = capacity
            .checked_mul(2)
            .filter(|value| *value <= 1_048_576)
            .context("macOS process list remained truncated")?;
    }
    let mut pids = pids
        .into_iter()
        .filter(|pid| *pid > 0)
        .map(|pid| pid as u32)
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

#[cfg(target_os = "macos")]
fn system_all_process_ids() -> Result<Vec<u32>> {
    mac_process_ids_for_uid(unsafe { libc::geteuid() })
}

#[cfg(target_os = "macos")]
fn system_active_listener_endpoints() -> Result<Vec<ListenerEndpoint>> {
    mac_lsof_listeners(None)
}

#[cfg(target_os = "macos")]
fn mac_hash_process_executable(path: &Path) -> Result<FileRecord> {
    let before_path = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot stat macOS process executable {}", path.display()))?;
    ensure!(
        before_path.is_file() && !before_path.file_type().is_symlink(),
        "macOS process executable path is not a regular non-symlink file"
    );
    ensure!(
        before_path.len() > 0 && before_path.len() <= MAX_EXECUTABLE_BYTES,
        "macOS process executable size is outside its bound"
    );
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot open macOS process executable {}", path.display()))?;
    let before = file
        .metadata()
        .context("cannot inspect opened macOS process executable")?;
    ensure!(
        metadata_identity(&before) == metadata_identity(&before_path),
        "macOS process executable changed before open"
    );
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut observed = 0u64;
    loop {
        let count = file
            .read(&mut buffer)
            .context("cannot hash macOS process executable")?;
        if count == 0 {
            break;
        }
        observed = observed
            .checked_add(count as u64)
            .context("macOS process executable length overflow")?;
        hasher.update(&buffer[..count]);
    }
    let after = file
        .metadata()
        .context("cannot reinspect macOS process executable")?;
    let after_path =
        std::fs::symlink_metadata(path).context("cannot restat macOS process executable")?;
    ensure!(
        observed == before.len()
            && metadata_identity(&after) == metadata_identity(&before)
            && metadata_identity(&after_path) == metadata_identity(&before),
        "macOS process executable changed while hashed"
    );
    let identity = metadata_identity(&before);
    Ok(FileRecord {
        path: path_string(path, "macOS process executable")?,
        device: identity.0,
        inode: identity.1,
        mode: identity.2,
        uid: identity.3,
        gid: identity.4,
        nlink: identity.5,
        size: identity.6,
        mtime_ns: identity.7,
        ctime_ns: identity.8,
        sha256: hex::encode(hasher.finalize()),
    })
}

#[cfg(target_os = "macos")]
fn system_matching_processes(
    executable_hashes: &ProcessExecutableHashCache,
    legacy_owner_uid: u32,
    data_dir: &str,
    _executable_path: &str,
    executable_size: u64,
    executable_sha256: &str,
) -> Result<Vec<ProcessObservation>> {
    let mut matches = Vec::new();
    for pid in mac_process_ids_for_uid(legacy_owner_uid)? {
        let mut basic = std::mem::MaybeUninit::<MacProcBsdInfo>::zeroed();
        let received = unsafe {
            proc_pidinfo(
                pid as libc::c_int,
                3,
                0,
                basic.as_mut_ptr().cast(),
                std::mem::size_of::<MacProcBsdInfo>() as libc::c_int,
            )
        };
        if received <= 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::ENOENT)) {
                continue;
            }
            return Err(error).context("cannot inspect same-owner macOS process identity");
        }
        ensure!(
            received as usize == std::mem::size_of::<MacProcBsdInfo>(),
            "same-owner macOS process identity record is truncated"
        );
        let basic = unsafe { basic.assume_init() };
        ensure!(
            basic.pbi_uid == legacy_owner_uid,
            "macOS UID-scoped process enumeration returned another owner"
        );
        // Zombie and in-exit processes can no longer execute legacy work, and
        // KERN_PROCARGS2 may block indefinitely while their kernel teardown is
        // stuck. Tree/listener stability still catches any pending effects.
        if basic.pbi_status == 5 || basic.pbi_flags & 4 != 0 {
            continue;
        }
        let mut path_buffer = vec![0u8; 4096];
        let length = unsafe {
            proc_pidpath(
                pid as libc::c_int,
                path_buffer.as_mut_ptr().cast(),
                path_buffer.len() as u32,
            )
        };
        if length <= 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::ENOENT)) {
                continue;
            }
            return Err(error).context("cannot prefilter macOS process executable");
        }
        path_buffer.truncate(length as usize);
        let selected_path = std::str::from_utf8(&path_buffer)
            .context("macOS process path is not UTF-8")?
            .trim_end_matches('\0');
        let selected_path = Path::new(selected_path);
        let candidate_metadata = match std::fs::symlink_metadata(selected_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                // A process that vanished between the UID-scoped enumeration
                // and its executable open is harmless churn. Any persistent
                // same-owner inspection failure is fail-closed.
                let mut info = std::mem::MaybeUninit::<MacProcBsdInfo>::zeroed();
                let received = unsafe {
                    proc_pidinfo(
                        pid as libc::c_int,
                        3,
                        0,
                        info.as_mut_ptr().cast(),
                        std::mem::size_of::<MacProcBsdInfo>() as libc::c_int,
                    )
                };
                if received <= 0
                    && matches!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ESRCH) | Some(libc::ENOENT)
                    )
                {
                    continue;
                }
                return Err(error).context("cannot stat same-owner macOS process executable");
            }
        };
        let semantic_match = match mac_process_argv(pid) {
            Ok(argv) => parse_stake_zero_argv(&argv, Path::new(data_dir)).is_ok(),
            Err(error) if error.downcast_ref::<std::str::Utf8Error>().is_some() => {
                // Unix argv is byte-valued. Invalid UTF-8 is a semantic
                // nonmatch; a same-size executable is still hashed below so
                // this cannot hide a renamed byte-identical legacy binary.
                false
            }
            Err(error) => return Err(error).context("cannot inspect same-owner macOS argv"),
        };
        let hash_match = if candidate_metadata.len() == executable_size {
            let identity = ProcessExecutableIdentity::from_metadata(&candidate_metadata);
            match executable_hashes
                .get_or_hash(identity, || mac_hash_process_executable(selected_path))
            {
                Ok(record) => record.sha256 == executable_sha256,
                Err(error) => {
                    let mut info = std::mem::MaybeUninit::<MacProcBsdInfo>::zeroed();
                    let received = unsafe {
                        proc_pidinfo(
                            pid as libc::c_int,
                            3,
                            0,
                            info.as_mut_ptr().cast(),
                            std::mem::size_of::<MacProcBsdInfo>() as libc::c_int,
                        )
                    };
                    if received <= 0
                        && matches!(
                            std::io::Error::last_os_error().raw_os_error(),
                            Some(libc::ESRCH) | Some(libc::ENOENT)
                        )
                    {
                        continue;
                    }
                    return Err(error).context("cannot hash same-owner macOS process executable");
                }
            }
        } else {
            false
        };
        if !hash_match && !semantic_match {
            continue;
        }
        if let Some(process) = mac_observe_process(pid, false)? {
            ensure!(
                process.uid == legacy_owner_uid,
                "macOS process owner changed while inspected"
            );
            if process.executable.sha256 == executable_sha256
                || parse_stake_zero_argv(&process.argv, Path::new(data_dir)).is_ok()
            {
                matches.push(process);
            }
        }
    }
    Ok(matches)
}

#[cfg(target_os = "linux")]
fn system_matching_processes(
    executable_hashes: &ProcessExecutableHashCache,
    legacy_owner_uid: u32,
    data_dir: &str,
    executable_path: &str,
    executable_size: u64,
    executable_sha256: &str,
) -> Result<Vec<ProcessObservation>> {
    linux_matching_processes_for_pids(
        system_all_process_ids()?,
        executable_hashes,
        legacy_owner_uid,
        data_dir,
        executable_path,
        executable_size,
        executable_sha256,
    )
}

#[cfg(target_os = "linux")]
fn linux_matching_processes_for_pids<I>(
    pids: I,
    executable_hashes: &ProcessExecutableHashCache,
    legacy_owner_uid: u32,
    data_dir: &str,
    _executable_path: &str,
    executable_size: u64,
    executable_sha256: &str,
) -> Result<Vec<ProcessObservation>>
where
    I: IntoIterator<Item = u32>,
{
    let mut matches = Vec::new();
    for pid in pids {
        let root = PathBuf::from(format!("/proc/{pid}"));
        // Ownership is checked before exe/cmdline so an unprivileged verifier
        // never attempts protected reads from unrelated users' /proc entries.
        let owner = match std::fs::metadata(&root) {
            Ok(metadata) => metadata.uid(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("cannot inspect Linux process owner"),
        };
        if owner != legacy_owner_uid {
            continue;
        }
        let exe_link = root.join("exe");
        let selected_path = match std::fs::read_link(&exe_link) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                if !root.exists() {
                    continue;
                }
                return Err(error).context("cannot inspect same-owner Linux process executable");
            }
        };
        let selected_path_text = path_string(&selected_path, "Linux process executable path")?;
        let candidate_metadata = match std::fs::metadata(&exe_link) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                if !root.exists() {
                    continue;
                }
                return Err(error).context("cannot stat same-owner Linux process executable");
            }
        };
        let command = match std::fs::read(root.join("cmdline")) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                if !root.exists() {
                    continue;
                }
                return Err(error).context("cannot inspect same-owner Linux process argv");
            }
        };
        let argv = command
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(std::str::from_utf8)
            .collect::<std::result::Result<Vec<_>, _>>();
        let semantic_match = match argv {
            Ok(argv) => {
                let argv = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
                parse_stake_zero_argv(&argv, Path::new(data_dir)).is_ok()
            }
            // Byte-valued argv on a differently-hashed process is not an
            // error. Same-sized executables remain subject to SHA comparison.
            Err(_) => false,
        };
        let hash_match = if candidate_metadata.len() == executable_size {
            let identity = ProcessExecutableIdentity::from_metadata(&candidate_metadata);
            match executable_hashes.get_or_hash(identity, || {
                open_process_executable(&exe_link, selected_path_text)
            }) {
                Ok(record) => record.sha256 == executable_sha256,
                Err(error) => {
                    if !root.exists() {
                        continue;
                    }
                    return Err(error).context("cannot hash same-owner Linux process executable");
                }
            }
        } else {
            false
        };
        if !hash_match && !semantic_match {
            continue;
        }
        if let Some(process) = linux_observe_process(pid, false)?
            && process.uid == legacy_owner_uid
            && (process.executable.sha256 == executable_sha256
                || parse_stake_zero_argv(&process.argv, Path::new(data_dir)).is_ok())
        {
            matches.push(process);
        }
    }
    Ok(matches)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_observe_process(_pid: u32) -> Result<Option<ProcessObservation>> {
    bail!("legacy-retirement runtime inspection is supported only on Linux and macOS")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_all_process_ids() -> Result<Vec<u32>> {
    bail!("legacy-retirement runtime inspection is supported only on Linux and macOS")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_active_listener_endpoints() -> Result<Vec<ListenerEndpoint>> {
    bail!("legacy-retirement runtime inspection is supported only on Linux and macOS")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_matching_processes(
    _executable_hashes: &ProcessExecutableHashCache,
    _legacy_owner_uid: u32,
    _data_dir: &str,
    _executable_path: &str,
    _executable_size: u64,
    _executable_sha256: &str,
) -> Result<Vec<ProcessObservation>> {
    bail!("legacy-retirement runtime inspection is supported only on Linux and macOS")
}

fn matching_replacement_processes<H: RetirementHost>(
    host: &H,
    legacy_owner_uid: u32,
    data_dir: &str,
    _executable_path: &str,
    _executable_size: u64,
    executable_sha256: &str,
) -> Result<Vec<ProcessObservation>> {
    let mut matches = Vec::new();
    for pid in host.all_process_ids()? {
        match host.observe_process(pid) {
            Ok(Some(process))
                if process.uid == legacy_owner_uid
                    && (process.executable.sha256 == executable_sha256
                        || parse_stake_zero_argv(&process.argv, Path::new(data_dir)).is_ok()) =>
            {
                matches.push(process);
            }
            Ok(_) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect process {pid}"));
            }
        }
    }
    Ok(matches)
}

fn prove_stably_offline<H: RetirementHost>(
    host: &H,
    old_process: &Map<String, Value>,
    legacy_owner_uid: u32,
    data_dir: &str,
    stability_seconds: u64,
    samples: u32,
) -> Result<Value> {
    ensure!(
        (3..=20).contains(&samples),
        "offline proof requires between three and 20 samples"
    );
    ensure!(
        (5..=300).contains(&stability_seconds),
        "offline proof requires 5 to 300 seconds of stability"
    );
    let pid = old_process
        .get("pid")
        .and_then(Value::as_u64)
        .map(|value| u32::try_from(value).context("intent PID exceeds u32"))
        .transpose()?;
    let boot_id = old_process.get("boot_id").and_then(Value::as_str);
    let start_ticks = old_process.get("start_ticks").and_then(Value::as_u64);
    let executable_sha256 = old_process
        .get("executable")
        .and_then(Value::as_object)
        .and_then(|value| value.get("sha256"))
        .and_then(Value::as_str)
        .context("intent process executable hash is missing")?;
    let executable_path = old_process
        .get("executable")
        .and_then(Value::as_object)
        .and_then(|value| value.get("path"))
        .and_then(Value::as_str)
        .context("intent process executable path is missing")?;
    let executable_size = old_process
        .get("executable")
        .and_then(Value::as_object)
        .and_then(|value| value.get("size"))
        .and_then(Value::as_u64)
        .context("intent process executable size is missing")?;
    let recorded = old_process
        .get("listeners")
        .and_then(Value::as_array)
        .context("intent process listeners are malformed")?
        .iter()
        .map(|value| {
            let row = value
                .as_object()
                .context("intent listener is not an object")?;
            Ok((
                string_field(row, "family", "intent listener")?.to_owned(),
                string_field(row, "address_hex", "intent listener")?.to_owned(),
                u16::try_from(u64_field(row, "port", "intent listener")?)
                    .context("intent listener port exceeds u16")?,
            ))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let interval = if samples > 1 {
        Duration::from_secs_f64(stability_seconds as f64 / (samples - 1) as f64)
    } else {
        Duration::ZERO
    };
    for index in 0..samples {
        if let (Some(pid), Some(boot_id), Some(start_ticks)) = (pid, boot_id, start_ticks)
            && let Some(observed) = host.observe_process(pid)?
            && observed.boot_id == boot_id
            && observed.start_ticks == start_ticks
        {
            bail!("the exact legacy process identity is still running");
        }
        let replacements = host.matching_processes(
            legacy_owner_uid,
            data_dir,
            executable_path,
            executable_size,
            executable_sha256,
        )?;
        ensure!(
            replacements.is_empty(),
            "a legacy executable/data-tree writer is still running: {:?}",
            replacements
                .iter()
                .map(|process| (process.pid, process.start_ticks))
                .collect::<Vec<_>>()
        );
        let active = host.active_listener_endpoints()?;
        let occupied_ports = active
            .iter()
            .filter(|listener| LEGACY_PORTS.contains(&listener.port))
            .map(|listener| listener.port)
            .collect::<BTreeSet<_>>();
        ensure!(
            occupied_ports.is_empty(),
            "required legacy listener ports remain active: {occupied_ports:?}"
        );
        let occupied_recorded = active
            .iter()
            .filter(|listener| {
                recorded.contains(&(
                    listener.family.clone(),
                    listener.address_hex.clone(),
                    listener.port,
                ))
            })
            .collect::<Vec<_>>();
        ensure!(
            occupied_recorded.is_empty(),
            "recorded legacy listener endpoints remain active"
        );
        if index + 1 < samples && !interval.is_zero() {
            host.sleep(interval);
        }
    }
    Ok(json!({
        "sample_count": samples,
        "stability_seconds": stability_seconds,
        "exact_process_identity_absent": true,
        "replacement_legacy_writer_absent": true,
        "recorded_listener_endpoints_absent": true,
        "listener_endpoints": recorded
            .iter()
            .map(|(family, address_hex, port)| json!({
                "family": family,
                "address_hex": address_hex,
                "port": port,
            }))
            .collect::<Vec<_>>(),
        "required_absent_listener_ports": LEGACY_PORTS,
    }))
}

#[derive(Clone, Debug)]
struct IntentView {
    mode: RetirementMode,
    legacy_owner_uid: u32,
    data_dir: PathBuf,
    v08_data_dir: PathBuf,
}

fn merge_fields(
    mut value: Value,
    fields: impl IntoIterator<Item = (&'static str, Value)>,
) -> Value {
    let object = value.as_object_mut().expect("merge target is an object");
    for (key, field) in fields {
        object.insert(key.to_owned(), field);
    }
    value
}

fn load_canonical_unpinned(
    path: &Path,
    label: &str,
    maximum: u64,
) -> Result<(Value, Vec<u8>, FileRecord)> {
    let (bytes, record) = stable_read(path, label, maximum)?;
    let value: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("{label} is invalid JSON"))?;
    ensure!(value.is_object(), "{label} must be one JSON object");
    ensure!(
        canonical_bytes(&value)? == bytes,
        "{label} must be canonical JSON with one trailing newline"
    );
    Ok((value, bytes, record))
}

fn validate_legacy_version(value: &str) -> Result<()> {
    let patch = value
        .strip_prefix("0.7.")
        .context("legacy version must be strict 0.7.PATCH")?;
    ensure!(
        !patch.is_empty()
            && patch.bytes().all(|byte| byte.is_ascii_digit())
            && (patch == "0" || !patch.starts_with('0')),
        "legacy version must be strict 0.7.PATCH"
    );
    patch
        .parse::<u64>()
        .context("legacy version patch exceeds u64")?;
    Ok(())
}

fn argv_sha256(argv: &[String]) -> String {
    let mut hasher = Sha256::new();
    for argument in argv {
        hasher.update(argument.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn same_process(left: &ProcessObservation, right: &ProcessObservation) -> bool {
    left.pid == right.pid
        && left.boot_id == right.boot_id
        && left.start_ticks == right.start_ticks
        && left.executable.sha256 == right.executable.sha256
        && left.executable.device == right.executable.device
        && left.executable.inode == right.executable.inode
        && left.argv == right.argv
}

fn process_record(observed: &ProcessObservation, semantics: &Value) -> Value {
    let mut listeners = observed.listeners.clone();
    listeners.sort();
    listeners.dedup();
    json!({
        "retirement_mode": "term_only",
        "pid": observed.pid,
        "boot_id": observed.boot_id,
        "start_ticks": observed.start_ticks,
        "uid": observed.uid,
        "gid": observed.gid,
        "executable": observed.executable.value(),
        "argv_sha256": argv_sha256(&observed.argv),
        "cwd": observed.cwd,
        "stake_zero_semantics": semantics,
        "listeners": listeners.iter().map(ListenerEndpoint::value).collect::<Vec<_>>(),
        "required_absent_listener_ports": LEGACY_PORTS,
    })
}

fn validate_file_record<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>> {
    let object = object_exact(
        value,
        &[
            "path", "device", "inode", "mode", "uid", "gid", "nlink", "size", "mtime_ns",
            "ctime_ns", "sha256",
        ],
        label,
    )?;
    let path = Path::new(string_field(object, "path", label)?);
    require_absolute_normal(path, &format!("{label}.path"))?;
    for key in ["device", "inode", "mode", "uid", "gid", "nlink", "size"] {
        u64_field(object, key, label)?;
    }
    ensure!(
        u64_field(object, "nlink", label)? == 1,
        "{label} has a hard link"
    );
    ensure!(u64_field(object, "size", label)? > 0, "{label} is empty");
    ensure!(
        object.get("mtime_ns").and_then(Value::as_i64).is_some()
            && object.get("ctime_ns").and_then(Value::as_i64).is_some(),
        "{label} timestamps are malformed"
    );
    expect_hash_field(object, "sha256", label)?;
    Ok(object)
}

fn same_file_binding(actual: &FileRecord, expected: &Value, label: &str) -> Result<()> {
    validate_file_record(expected, label)?;
    ensure!(
        actual.value() == *expected,
        "{label} differs from the retirement intent"
    );
    Ok(())
}

fn parse_retirement_mode(value: &str, pid: Option<u32>) -> Result<RetirementMode> {
    match value {
        "term_only" => Ok(RetirementMode::TermOnly(
            pid.context("TERM-only intent omits its PID")?,
        )),
        "preexisting_offline" => {
            ensure!(pid.is_none(), "preexisting-offline intent invents a PID");
            Ok(RetirementMode::PreexistingOffline)
        }
        _ => bail!("intent retirement mode is unsupported"),
    }
}

fn validate_intent(value: &Value) -> Result<IntentView> {
    let object = object_exact(
        value,
        &[
            "schema",
            "protocol_id",
            "created_at",
            "scope",
            "target_release",
            "maintenance_boundary",
            "cutover_policy",
            "checkpoint",
            "inspector",
            "legacy_release",
            "old_process",
            "old_data",
            "v08_start",
            "replay_inputs",
            "retirement_policy",
        ],
        "retirement intent",
    )?;
    ensure!(
        string_field(object, "schema", "retirement intent")? == INTENT_SCHEMA,
        "retirement intent schema differs"
    );
    expect_hash_field(object, "protocol_id", "retirement intent")?;
    validate_utc(
        string_field(object, "created_at", "retirement intent")?,
        "intent created_at",
    )?;
    ensure!(
        string_field(object, "scope", "retirement intent")? == "v0.7-stake-zero-community-worker",
        "retirement intent scope differs"
    );
    ensure!(
        object.get("retirement_policy")
            == Some(&json!({
                "network_access": "forbidden",
                "legacy_data_writes": "forbidden",
                "stop_signal_policy": "external-supervisor-term-only-no-sigkill",
                "legacy_exit_clean_claimed": false,
                "legacy_jobs_disposition": JOBS_DISPOSITION,
            })),
        "retirement intent policy differs"
    );

    let release = object
        .get("target_release")
        .and_then(Value::as_object)
        .context("intent target release is malformed")?;
    ensure!(
        matches!(
            string_field(release, "binding_schema", "intent target release")?,
            INSTALLER_BINDING_SCHEMA | INTERNAL_HANDOFF_SCHEMA
        ),
        "intent target release schema differs"
    );
    ensure!(
        string_field(release, "repository", "intent target release")? == REPOSITORY,
        "intent target repository differs"
    );
    validate_semver(string_field(release, "tag", "intent target release")?)?;
    expect_hash_field(release, "binding_sha256", "intent target release")?;
    expect_hash_field(release, "manifest_sha256", "intent target release")?;
    let release_path = Path::new(string_field(
        release,
        "binding_path",
        "intent target release",
    )?);
    require_absolute_normal(release_path, "intent target release path")?;
    ensure!(
        release.get("inspector_asset") == Some(&Value::Null)
            && release.get("inspector_sha256") == Some(&Value::Null),
        "forensic-only intent must not bind a local inspector"
    );

    let inspector = object_exact(
        object
            .get("inspector")
            .context("intent inspector is missing")?,
        &["path", "asset", "sha256"],
        "intent inspector",
    )?;
    ensure!(
        inspector.values().all(Value::is_null),
        "forensic-only intent binds an inspector"
    );

    let legacy = object_exact(
        object
            .get("legacy_release")
            .context("intent legacy release is missing")?,
        &[
            "version",
            "executable_sha256",
            "executable",
            "supervisor_definition",
            "supervisor_source",
        ],
        "intent legacy release",
    )?;
    validate_legacy_version(string_field(legacy, "version", "intent legacy release")?)?;
    let executable_sha = expect_hash_field(legacy, "executable_sha256", "intent legacy release")?;
    let executable = validate_file_record(
        legacy
            .get("executable")
            .context("intent executable is missing")?,
        "intent legacy executable",
    )?;
    ensure!(
        executable.get("sha256").and_then(Value::as_str) == Some(executable_sha.as_str()),
        "intent legacy executable hashes differ"
    );
    ensure!(
        u64_field(executable, "mode", "intent legacy executable")? & 0o111 != 0,
        "intent legacy executable has no execute bit"
    );
    validate_file_record(
        legacy
            .get("supervisor_definition")
            .context("intent supervisor is missing")?,
        "intent supervisor definition",
    )?;
    validate_file_record(
        legacy
            .get("supervisor_source")
            .context("intent supervisor source is missing")?,
        "intent supervisor source",
    )?;

    let process = object_exact(
        object
            .get("old_process")
            .context("intent old process is missing")?,
        &[
            "retirement_mode",
            "pid",
            "boot_id",
            "start_ticks",
            "uid",
            "gid",
            "executable",
            "argv_sha256",
            "cwd",
            "stake_zero_semantics",
            "listeners",
            "required_absent_listener_ports",
        ],
        "intent old process",
    )?;
    let pid = match process.get("pid") {
        Some(Value::Null) => None,
        Some(value) => {
            let value = value.as_u64().context("intent PID must be an integer")?;
            ensure!(
                value > 1 && value <= u32::MAX as u64,
                "intent PID is out of range"
            );
            Some(value as u32)
        }
        None => bail!("intent PID is missing"),
    };
    let mode = parse_retirement_mode(
        string_field(process, "retirement_mode", "intent old process")?,
        pid,
    )?;
    ensure!(
        process.get("executable") == legacy.get("executable"),
        "intent process executable differs"
    );
    let semantics = object_exact(
        process
            .get("stake_zero_semantics")
            .context("intent stake semantics are missing")?,
        &[
            "stake",
            "minimum_stake",
            "data_dir",
            "community_mode_explicit",
            "community_mode_effective",
        ],
        "intent stake-zero semantics",
    )?;
    ensure!(
        u64_field(semantics, "stake", "intent semantics")? == 0
            && u64_field(semantics, "minimum_stake", "intent semantics")? == 0
            && bool_field(semantics, "community_mode_effective", "intent semantics")?,
        "intent is not an effective stake-zero community worker"
    );
    let data_dir = PathBuf::from(string_field(semantics, "data_dir", "intent semantics")?);
    require_absolute_normal(&data_dir, "intent legacy data directory")?;
    match mode {
        RetirementMode::TermOnly(_) => {
            let boot = string_field(process, "boot_id", "intent old process")?;
            ensure!(
                !boot.is_empty() && boot.len() <= 128,
                "intent boot identity is malformed"
            );
            ensure!(
                u64_field(process, "start_ticks", "intent old process")? > 0,
                "intent start identity is zero"
            );
            u64_field(process, "uid", "intent old process")?;
            u64_field(process, "gid", "intent old process")?;
            expect_hash_field(process, "argv_sha256", "intent old process")?;
            ensure!(
                process.get("cwd").is_some(),
                "intent process cwd field is missing"
            );
        }
        RetirementMode::PreexistingOffline => {
            for field in [
                "pid",
                "boot_id",
                "start_ticks",
                "uid",
                "gid",
                "argv_sha256",
                "cwd",
            ] {
                ensure!(
                    process.get(field) == Some(&Value::Null),
                    "preexisting-offline intent invents process identity"
                );
            }
        }
    }
    ensure!(
        process.get("required_absent_listener_ports") == Some(&json!(LEGACY_PORTS)),
        "intent required listener ports differ"
    );
    let listeners = process
        .get("listeners")
        .and_then(Value::as_array)
        .context("intent listener records are malformed")?;
    if mode == RetirementMode::PreexistingOffline {
        ensure!(
            listeners.is_empty(),
            "preexisting-offline intent invents listeners"
        );
    }
    let mut endpoints = BTreeSet::new();
    for (index, listener) in listeners.iter().enumerate() {
        let row = object_exact(
            listener,
            &["family", "address_hex", "port", "inode"],
            &format!("intent listener #{index}"),
        )?;
        let family = string_field(row, "family", "intent listener")?;
        ensure!(
            matches!(family, "tcp4" | "tcp6"),
            "intent listener family differs"
        );
        let address = string_field(row, "address_hex", "intent listener")?;
        ensure!(!address.is_empty(), "intent listener address is empty");
        let port = u64_field(row, "port", "intent listener")?;
        ensure!(
            (1..=65535).contains(&port),
            "intent listener port is invalid"
        );
        ensure!(
            u64_field(row, "inode", "intent listener")? > 0,
            "intent listener identity is zero"
        );
        ensure!(
            endpoints.insert((family.to_owned(), address.to_owned(), port)),
            "intent repeats a listener endpoint"
        );
    }

    let old_data = object_exact(
        object
            .get("old_data")
            .context("intent old data is missing")?,
        &["root_anchor", "wal_prefix"],
        "intent old data",
    )?;
    let root = old_data
        .get("root_anchor")
        .and_then(Value::as_object)
        .context("intent data root anchor is malformed")?;
    ensure!(
        string_field(root, "path", "intent data root")?
            == path_string(&data_dir, "intent data dir")?,
        "intent data-root paths differ"
    );
    for field in ["device", "inode", "mode", "uid", "gid", "nlink"] {
        u64_field(root, field, "intent data root")?;
    }
    let legacy_owner_uid = u32::try_from(u64_field(root, "uid", "intent data root")?)
        .context("intent legacy owner UID exceeds u32")?;
    if mode.label() == "term_only" {
        ensure!(
            u64_field(process, "uid", "intent old process")? == legacy_owner_uid as u64,
            "intent legacy process owner differs from the bound data-tree owner"
        );
    }
    let wal = object_exact(
        old_data
            .get("wal_prefix")
            .context("intent WAL is missing")?,
        &[
            "path",
            "device",
            "inode",
            "mode",
            "uid",
            "gid",
            "nlink",
            "observed_prefix_bytes",
            "observed_prefix_sha256",
        ],
        "intent WAL prefix",
    )?;
    ensure!(
        string_field(wal, "path", "intent WAL")?
            == path_string(&data_dir.join("state.wal"), "intent WAL")?,
        "intent WAL path differs"
    );
    ensure!(
        u64_field(wal, "observed_prefix_bytes", "intent WAL")? > 0,
        "intent WAL prefix is empty"
    );
    expect_hash_field(wal, "observed_prefix_sha256", "intent WAL")?;
    ensure!(
        u64_field(wal, "nlink", "intent WAL")? == 1,
        "intent WAL has a hard link"
    );

    let v08 = object_exact(
        object
            .get("v08_start")
            .context("intent v0.8 start is missing")?,
        &[
            "data_dir",
            "must_be_absent_until_receipt",
            "canonical_history_source",
            "old_wal_migration_allowed",
        ],
        "intent v0.8 start",
    )?;
    let v08_data_dir = PathBuf::from(string_field(v08, "data_dir", "intent v0.8 start")?);
    require_absolute_normal(&v08_data_dir, "intent v0.8 data directory")?;
    ensure!(
        bool_field(v08, "must_be_absent_until_receipt", "intent v0.8 start")?
            && string_field(v08, "canonical_history_source", "intent v0.8 start")?
                == "signed_recovery_checkpoint"
            && !bool_field(v08, "old_wal_migration_allowed", "intent v0.8 start")?,
        "intent v0.8 start policy differs"
    );
    ensure!(
        !v08_data_dir.starts_with(&data_dir) && !data_dir.starts_with(&v08_data_dir),
        "intent old and v0.8 data directories overlap"
    );
    ensure!(
        object.get("replay_inputs")
            == Some(&json!({
                "mode": "forensic-only",
                "snapshot": null,
                "genesis": null,
                "legacy_validator_set": null,
                "allow_unbound_legacy_wal": null,
            })),
        "compiled retirement accepts only honest forensic-only mode"
    );

    let roots = json!({
        "release_binding_sha256": release["binding_sha256"],
        "maintenance_boundary_sha256": object["maintenance_boundary"]["sha256"],
        "cutover_policy_sha256": object["cutover_policy"]["sha256"],
        "checkpoint_descriptor_sha256": object["checkpoint"]["descriptor_sha256"],
        "checkpoint_file_sha256": object["checkpoint"]["checkpoint_file"]["sha256"],
        "checkpoint_manifest_hash": object["checkpoint"]["manifest_hash"],
        "local_inspector_sha256": null,
        "legacy_executable_sha256": legacy["executable_sha256"],
        "retirement_mode": process["retirement_mode"],
        "process_boot_id": process["boot_id"],
        "process_pid": process["pid"],
        "process_start_ticks": process["start_ticks"],
        "data_directory_device": root["device"],
        "data_directory_inode": root["inode"],
        "wal_prefix_sha256": wal["observed_prefix_sha256"],
        "v08_data_dir": v08["data_dir"],
        "replay_mode": "forensic-only",
    });
    let mut protocol_hasher = Sha256::new();
    protocol_hasher.update(b"ARC-v0.7-stake-zero-retirement-intent-v1\0");
    protocol_hasher.update(canonical_bytes(&roots)?);
    ensure!(
        string_field(object, "protocol_id", "retirement intent")?
            == hex::encode(protocol_hasher.finalize()),
        "retirement intent protocol id does not bind its roots"
    );
    Ok(IntentView {
        mode,
        legacy_owner_uid,
        data_dir,
        v08_data_dir,
    })
}

fn existing_intent_matches(value: &Value, request: &CreateRequest) -> Result<bool> {
    let object = value.as_object().expect("validated intent object");
    let expected = [
        (
            "target_release",
            "binding_sha256",
            request.target_release_sha256.as_str(),
        ),
        (
            "maintenance_boundary",
            "sha256",
            request.maintenance_boundary_sha256.as_str(),
        ),
        (
            "cutover_policy",
            "sha256",
            request.cutover_policy_sha256.as_str(),
        ),
        (
            "checkpoint",
            "descriptor_sha256",
            request.checkpoint_descriptor_sha256.as_str(),
        ),
        (
            "legacy_release",
            "executable_sha256",
            request.legacy_executable_sha256.as_str(),
        ),
    ];
    for (section, field, selected) in expected {
        ensure!(
            object
                .get(section)
                .and_then(Value::as_object)
                .and_then(|row| row.get(field))
                .and_then(Value::as_str)
                == Some(selected),
            "existing retirement intent {section} differs from this request"
        );
    }
    let path_expectations = [
        ("target_release", "binding_path", &request.target_release),
        (
            "maintenance_boundary",
            "path",
            &request.maintenance_boundary,
        ),
        ("cutover_policy", "path", &request.cutover_policy),
        ("checkpoint", "path", &request.checkpoint_descriptor),
    ];
    for (section, field, selected) in path_expectations {
        ensure!(
            object[section][field].as_str()
                == Some(path_string(selected, &format!("selected {section}"))?.as_str()),
            "existing retirement intent {section} path differs from this request"
        );
    }
    let legacy = object["legacy_release"]
        .as_object()
        .expect("validated legacy release");
    ensure!(
        legacy.get("version").and_then(Value::as_str) == Some(request.legacy_version.as_str())
            && legacy["executable"]["path"].as_str()
                == Some(
                    path_string(&request.legacy_executable, "selected legacy executable")?.as_str()
                )
            && legacy["supervisor_definition"]["path"].as_str()
                == Some(
                    path_string(&request.supervisor_definition, "selected supervisor")?.as_str()
                )
            && legacy["supervisor_definition"]["sha256"].as_str()
                == Some(request.supervisor_definition_sha256.as_str()),
        "existing retirement intent legacy release differs from this request"
    );
    let old_process = object["old_process"]
        .as_object()
        .expect("validated old process");
    let existing_mode = old_process["retirement_mode"]
        .as_str()
        .expect("validated retirement mode");
    let offline_resume =
        request.mode == RetirementMode::PreexistingOffline && existing_mode == "term_only";
    match request.mode {
        RetirementMode::TermOnly(pid) => ensure!(
            existing_mode == "term_only"
                && old_process.get("pid").and_then(Value::as_u64) == Some(pid as u64),
            "existing retirement intent PID/mode differs from this request"
        ),
        RetirementMode::PreexistingOffline => ensure!(
            matches!(existing_mode, "preexisting_offline" | "term_only"),
            "existing retirement intent mode differs from this offline resume"
        ),
    }
    ensure!(
        object["old_data"]["root_anchor"]["path"].as_str()
            == Some(path_string(&request.data_dir, "selected legacy data directory")?.as_str())
            && object["v08_start"]["data_dir"].as_str()
                == Some(
                    path_string(&request.v08_data_dir, "selected v0.8 data directory")?.as_str()
                ),
        "existing retirement intent data directory differs from this request"
    );
    Ok(offline_resume)
}

fn create_intent_with_host<H: RetirementHost>(
    request: &CreateRequest,
    host: &H,
) -> Result<(Value, String)> {
    for (hash, label) in [
        (
            &request.target_release_sha256,
            "target release binding SHA-256",
        ),
        (
            &request.maintenance_boundary_sha256,
            "maintenance boundary SHA-256",
        ),
        (&request.cutover_policy_sha256, "cutover policy SHA-256"),
        (
            &request.checkpoint_descriptor_sha256,
            "checkpoint descriptor SHA-256",
        ),
        (
            &request.legacy_executable_sha256,
            "legacy executable SHA-256",
        ),
        (
            &request.supervisor_definition_sha256,
            "supervisor definition SHA-256",
        ),
    ] {
        require_lower_hash(hash, label)?;
    }
    validate_legacy_version(&request.legacy_version)?;
    ensure_output_path(
        &request.intent_output,
        &request.data_dir,
        "retirement intent output",
    )?;
    if request.intent_output.exists() {
        let (existing, raw, _) = load_canonical_unpinned(
            &request.intent_output,
            "existing retirement intent",
            MAX_JSON_BYTES,
        )?;
        let existing_view = validate_intent(&existing)?;
        let offline_resume = existing_intent_matches(&existing, request)?;
        if offline_resume {
            let process = existing["old_process"]
                .as_object()
                .expect("validated old process");
            prove_stably_offline(
                host,
                process,
                existing_view.legacy_owner_uid,
                &path_string(&request.data_dir, "legacy data directory")?,
                10,
                3,
            )?;
        }
        return Ok((existing, sha256(&raw)));
    }
    ensure_disjoint_absent_v08(&request.v08_data_dir, &request.data_dir)?;
    for (path, label) in [
        (&request.target_release, "target release binding"),
        (&request.maintenance_boundary, "maintenance boundary"),
        (&request.cutover_policy, "cutover policy"),
        (&request.checkpoint_descriptor, "checkpoint descriptor"),
        (&request.legacy_executable, "legacy executable"),
        (&request.supervisor_definition, "supervisor definition"),
        (&request.data_dir, "legacy data directory"),
    ] {
        require_absolute_normal(path, label)?;
    }

    let (release_value, _release_raw, _) = load_canonical_json(
        &request.target_release,
        "target release binding",
        &request.target_release_sha256,
        MAX_JSON_BYTES,
    )?;
    let release = validate_release_binding(
        &release_value,
        &request.cutover_policy_sha256,
        &request.maintenance_boundary_sha256,
        &request.checkpoint_descriptor_sha256,
    )?;
    let (boundary_value, _boundary_raw, _) = load_canonical_json(
        &request.maintenance_boundary,
        "maintenance boundary",
        &request.maintenance_boundary_sha256,
        MAX_JSON_BYTES,
    )?;
    let boundary = validate_boundary(&boundary_value)?;
    let (descriptor_value, descriptor_raw, descriptor_record) = load_canonical_json(
        &request.checkpoint_descriptor,
        "checkpoint descriptor",
        &request.checkpoint_descriptor_sha256,
        MAX_DESCRIPTOR_BYTES,
    )?;
    let verified = host.verify_descriptor(&request.checkpoint_descriptor)?;
    ensure!(
        verified.status == "VERIFIED_DESCRIPTOR_QUORUM",
        "checkpoint descriptor is not verified quorum"
    );
    let (descriptor_after, descriptor_raw_after, descriptor_record_after) = load_canonical_json(
        &request.checkpoint_descriptor,
        "checkpoint descriptor after cryptographic verification",
        &request.checkpoint_descriptor_sha256,
        MAX_DESCRIPTOR_BYTES,
    )?;
    ensure!(
        descriptor_after == descriptor_value
            && descriptor_raw_after == descriptor_raw
            && descriptor_record_after == descriptor_record,
        "checkpoint descriptor changed while cryptographically verified"
    );
    let checkpoint =
        validate_descriptor_projection(&descriptor_value, &release, &boundary, &verified)?;
    let (policy_value, _policy_raw, _) = load_canonical_json(
        &request.cutover_policy,
        "cutover policy",
        &request.cutover_policy_sha256,
        MAX_JSON_BYTES,
    )?;
    let policy = validate_policy(
        &policy_value,
        &release,
        &boundary,
        &request.maintenance_boundary_sha256,
        &checkpoint,
        &request.checkpoint_descriptor_sha256,
    )?;

    let executable = stable_hash_file(
        &request.legacy_executable,
        "legacy v0.7 executable",
        MAX_EXECUTABLE_BYTES,
    )?;
    ensure!(
        executable.sha256 == request.legacy_executable_sha256,
        "legacy v0.7 executable SHA-256 differs"
    );
    ensure!(
        executable.mode & 0o111 != 0,
        "legacy v0.7 executable has no execute bit"
    );
    let (supervisor_value, _supervisor_raw, supervisor_record) = load_canonical_json(
        &request.supervisor_definition,
        "legacy supervisor definition",
        &request.supervisor_definition_sha256,
        MAX_JSON_BYTES,
    )?;
    let (semantics, supervisor_source) = validate_supervisor(
        &supervisor_value,
        &request.data_dir,
        &request.legacy_executable,
        &request.legacy_executable_sha256,
    )?;
    let supervisor_argv = supervisor_value["argv"]
        .as_array()
        .expect("validated supervisor argv")
        .iter()
        .map(|value| value.as_str().expect("validated argv string").to_owned())
        .collect::<Vec<_>>();

    // The preserved data-tree owner is the retirement namespace owner. This
    // remains correct when a Linux installer itself runs as root on behalf of
    // SUDO_USER/a managed system user, and prevents either missing that user's
    // replacement process or scanning unrelated users' protected /proc rows.
    let root_anchor = directory_record(&request.data_dir, "legacy data directory")?;
    let legacy_owner_uid = u32::try_from(
        root_anchor["uid"]
            .as_u64()
            .context("legacy data-directory UID is malformed")?,
    )
    .context("legacy data-directory UID exceeds u32")?;
    let wal = wal_prefix_record(&request.data_dir.join("state.wal"))?;

    let old_process = match request.mode {
        RetirementMode::TermOnly(pid) => {
            ensure!(pid > 1, "legacy PID must be greater than one");
            let observed = host
                .observe_process(pid)?
                .context("legacy process is not running; use --already-offline instead")?;
            ensure!(
                observed.executable.sha256 == executable.sha256
                    && observed.executable.device == executable.device
                    && observed.executable.inode == executable.inode
                    && observed.executable.path == executable.path,
                "running legacy process executable differs from selected v0.7 bytes"
            );
            ensure!(
                observed.argv == supervisor_argv,
                "running legacy process argv differs from verified supervisor binding"
            );
            ensure!(
                observed.uid == legacy_owner_uid,
                "running legacy process owner differs from the selected data-tree owner"
            );
            let repeated = host
                .observe_process(pid)?
                .context("legacy process disappeared while its identity was sealed")?;
            ensure!(
                same_process(&observed, &repeated),
                "legacy process identity changed while intent was created"
            );
            process_record(&observed, &semantics)
        }
        RetirementMode::PreexistingOffline => {
            let process = json!({
                "retirement_mode": "preexisting_offline",
                "pid": null,
                "boot_id": null,
                "start_ticks": null,
                "uid": null,
                "gid": null,
                "executable": executable.value(),
                "argv_sha256": null,
                "cwd": null,
                "stake_zero_semantics": semantics,
                "listeners": [],
                "required_absent_listener_ports": LEGACY_PORTS,
            });
            prove_stably_offline(
                host,
                process.as_object().expect("process object"),
                legacy_owner_uid,
                &path_string(&request.data_dir, "legacy data directory")?,
                10,
                3,
            )?;
            process
        }
    };

    ensure_disjoint_absent_v08(&request.v08_data_dir, &request.data_dir)?;
    let roots = json!({
        "release_binding_sha256": request.target_release_sha256,
        "maintenance_boundary_sha256": request.maintenance_boundary_sha256,
        "cutover_policy_sha256": request.cutover_policy_sha256,
        "checkpoint_descriptor_sha256": request.checkpoint_descriptor_sha256,
        "checkpoint_file_sha256": checkpoint["checkpoint_file"]["sha256"],
        "checkpoint_manifest_hash": checkpoint["manifest_hash"],
        "local_inspector_sha256": null,
        "legacy_executable_sha256": request.legacy_executable_sha256,
        "retirement_mode": request.mode.label(),
        "process_boot_id": old_process["boot_id"],
        "process_pid": old_process["pid"],
        "process_start_ticks": old_process["start_ticks"],
        "data_directory_device": root_anchor["device"],
        "data_directory_inode": root_anchor["inode"],
        "wal_prefix_sha256": wal["observed_prefix_sha256"],
        "v08_data_dir": path_string(&request.v08_data_dir, "v0.8 data directory")?,
        "replay_mode": "forensic-only",
    });
    let mut protocol_hasher = Sha256::new();
    protocol_hasher.update(b"ARC-v0.7-stake-zero-retirement-intent-v1\0");
    protocol_hasher.update(canonical_bytes(&roots)?);
    let protocol_id = hex::encode(protocol_hasher.finalize());

    let intent = json!({
        "schema": INTENT_SCHEMA,
        "protocol_id": protocol_id,
        "created_at": host.now(),
        "scope": "v0.7-stake-zero-community-worker",
        "target_release": merge_fields(release.projected.clone(), [
            ("binding_path", Value::String(path_string(&request.target_release, "target release")?)),
            ("binding_sha256", Value::String(request.target_release_sha256.clone())),
        ]),
        "maintenance_boundary": merge_fields(boundary.projected.clone(), [
            ("path", Value::String(path_string(&request.maintenance_boundary, "maintenance boundary")?)),
            ("sha256", Value::String(request.maintenance_boundary_sha256.clone())),
        ]),
        "cutover_policy": merge_fields(policy, [
            ("path", Value::String(path_string(&request.cutover_policy, "cutover policy")?)),
            ("sha256", Value::String(request.cutover_policy_sha256.clone())),
        ]),
        "checkpoint": merge_fields(checkpoint, [
            ("path", Value::String(path_string(&request.checkpoint_descriptor, "checkpoint descriptor")?)),
            ("descriptor_sha256", Value::String(request.checkpoint_descriptor_sha256.clone())),
        ]),
        "inspector": {"path": null, "asset": null, "sha256": null},
        "legacy_release": {
            "version": request.legacy_version,
            "executable_sha256": request.legacy_executable_sha256,
            "executable": executable.value(),
            "supervisor_definition": supervisor_record.value(),
            "supervisor_source": supervisor_source.value(),
        },
        "old_process": old_process,
        "old_data": {"root_anchor": root_anchor, "wal_prefix": wal},
        "v08_start": {
            "data_dir": path_string(&request.v08_data_dir, "v0.8 data directory")?,
            "must_be_absent_until_receipt": true,
            "canonical_history_source": "signed_recovery_checkpoint",
            "old_wal_migration_allowed": false,
        },
        "replay_inputs": {
            "mode": "forensic-only",
            "snapshot": null,
            "genesis": null,
            "legacy_validator_set": null,
            "allow_unbound_legacy_wal": null,
        },
        "retirement_policy": {
            "network_access": "forbidden",
            "legacy_data_writes": "forbidden",
            "stop_signal_policy": "external-supervisor-term-only-no-sigkill",
            "legacy_exit_clean_claimed": false,
            "legacy_jobs_disposition": JOBS_DISPOSITION,
        },
    });
    validate_intent(&intent)?;
    ensure_disjoint_absent_v08(&request.v08_data_dir, &request.data_dir)?;
    let digest = publish_create_only(&request.intent_output, &intent, "retirement intent")?;
    Ok((intent, digest))
}

fn validate_stop_evidence(value: &Value, intent: &Value, intent_sha256: &str) -> Result<()> {
    let object = object_exact(
        value,
        &[
            "schema",
            "intent_sha256",
            "process_identity",
            "supervisor",
            "observation_started_at",
            "offline_observed_at",
            "legacy_exit_clean_claimed",
        ],
        "offline stop evidence",
    )?;
    let process = intent["old_process"]
        .as_object()
        .context("intent old process is malformed")?;
    let mode = string_field(process, "retirement_mode", "intent process")?;
    let expected_schema = if mode == "term_only" {
        STOP_EVIDENCE_SCHEMA
    } else {
        PREEXISTING_EVIDENCE_SCHEMA
    };
    ensure!(
        string_field(object, "schema", "offline evidence")? == expected_schema,
        "offline evidence schema differs from retirement mode"
    );
    ensure!(
        string_field(object, "intent_sha256", "offline evidence")? == intent_sha256,
        "offline evidence is not bound to the exact retirement intent"
    );
    require_lower_hash(intent_sha256, "retirement intent SHA-256")?;
    let expected_identity = if mode == "term_only" {
        json!({
            "boot_id": process["boot_id"],
            "pid": process["pid"],
            "start_ticks": process["start_ticks"],
        })
    } else {
        Value::Null
    };
    ensure!(
        object.get("process_identity") == Some(&expected_identity),
        "offline evidence process identity differs from the intent"
    );
    let supervisor = object_exact(
        object
            .get("supervisor")
            .context("offline evidence supervisor is missing")?,
        &[
            "mechanism",
            "signals_sent",
            "send_sigkill_configured",
            "sigkill_sent",
            "escalation_used",
            "exit_status_observed",
        ],
        "offline evidence supervisor",
    )?;
    let mechanism = string_field(supervisor, "mechanism", "offline evidence supervisor")?;
    let (allowed, expected_signals) = if mode == "term_only" {
        (
            matches!(
                mechanism,
                "systemd-send-sigkill-no" | "launchd-term-only" | "direct-term-only"
            ),
            json!(["SIGTERM"]),
        )
    } else {
        (
            mechanism == "preexisting-offline-verified-supervisor",
            json!([]),
        )
    };
    ensure!(
        allowed,
        "offline evidence supervisor mechanism is unsupported"
    );
    ensure!(
        supervisor.get("signals_sent") == Some(&expected_signals),
        "offline evidence signal sequence differs from retirement mode"
    );
    ensure!(
        !bool_field(
            supervisor,
            "send_sigkill_configured",
            "offline evidence supervisor"
        )? && !bool_field(supervisor, "sigkill_sent", "offline evidence supervisor")?
            && !bool_field(supervisor, "escalation_used", "offline evidence supervisor")?,
        "retirement refuses any configured, sent, or escalated SIGKILL path"
    );
    bool_field(
        supervisor,
        "exit_status_observed",
        "offline evidence supervisor",
    )?;
    ensure!(
        !bool_field(object, "legacy_exit_clean_claimed", "offline evidence")?,
        "v0.7 retirement must not claim a clean legacy exit"
    );
    let started = string_field(object, "observation_started_at", "offline evidence")?;
    let offline = string_field(object, "offline_observed_at", "offline evidence")?;
    validate_utc(started, "offline evidence observation_started_at")?;
    validate_utc(offline, "offline evidence offline_observed_at")?;
    ensure!(
        chrono::DateTime::parse_from_rfc3339(offline)?
            >= chrono::DateTime::parse_from_rfc3339(started)?,
        "offline evidence completion predates its observation start"
    );
    Ok(())
}

fn expected_receipt_checkpoint(intent: &Value) -> Value {
    let checkpoint = &intent["checkpoint"];
    json!({
        "descriptor_sha256": checkpoint["descriptor_sha256"],
        "full_file_sha256": checkpoint["checkpoint_file"]["sha256"],
        "full_file_size_bytes": checkpoint["checkpoint_file"]["size_bytes"],
        "format_version": checkpoint["format_version"],
        "chain_id": checkpoint["chain_id"],
        "manifest_hash": checkpoint["manifest_hash"],
        "payload_hash": checkpoint["payload_hash"],
        "community_rewards_v1_activation_height": checkpoint["community_rewards_v1_activation_height"],
        "certificate_signing_hash": checkpoint["checkpoint_certificate"]["signing_hash"],
        "certificate_cryptographically_verified": true,
        "verified_signature_count": checkpoint["verified_quorum"]["verified_signature_count"],
        "signed_validator_addresses": checkpoint["verified_quorum"]["signed_validator_addresses"],
        "signed_stake": checkpoint["verified_quorum"]["signed_stake"],
        "total_stake": checkpoint["verified_quorum"]["total_stake"],
        "source_height": checkpoint["source_height"],
        "source_block_hash": checkpoint["source_block_hash"],
        "source_state_root": checkpoint["source_state_root"],
        "transition_height": checkpoint["transition_height"],
        "transition_block_hash": checkpoint["transition_block_hash"],
        "canonical_history_source": "signed_recovery_checkpoint",
    })
}

fn validate_receipt(value: &Value) -> Result<()> {
    let object = object_exact(
        value,
        &[
            "schema",
            "verified_at",
            "intent_sha256",
            "stop_evidence_sha256",
            "protocol_id",
            "scope",
            "target_release",
            "maintenance_boundary",
            "cutover_policy",
            "checkpoint",
            "old_process",
            "offline_stability",
            "old_data_tree",
            "v08_start",
            "local_legacy_replay",
            "retirement_result",
        ],
        "retirement receipt",
    )?;
    ensure!(
        string_field(object, "schema", "retirement receipt")? == RECEIPT_SCHEMA,
        "retirement receipt schema differs"
    );
    validate_utc(
        string_field(object, "verified_at", "retirement receipt")?,
        "receipt verified_at",
    )?;
    for field in ["intent_sha256", "stop_evidence_sha256", "protocol_id"] {
        expect_hash_field(object, field, "retirement receipt")?;
    }
    ensure!(
        string_field(object, "scope", "retirement receipt")? == "v0.7-stake-zero-community-worker",
        "retirement receipt scope differs"
    );
    let process = object
        .get("old_process")
        .and_then(Value::as_object)
        .context("receipt old process is malformed")?;
    let mode = string_field(process, "retirement_mode", "receipt old process")?;
    ensure!(
        matches!(mode, "term_only" | "preexisting_offline"),
        "receipt process mode differs"
    );
    ensure!(
        process.get("signals_sent")
            == Some(&if mode == "term_only" {
                json!(["SIGTERM"])
            } else {
                json!([])
            }),
        "receipt signal sequence differs from process mode"
    );
    let checkpoint = object_exact(
        object
            .get("checkpoint")
            .context("receipt checkpoint is missing")?,
        &[
            "descriptor_sha256",
            "full_file_sha256",
            "full_file_size_bytes",
            "format_version",
            "chain_id",
            "manifest_hash",
            "payload_hash",
            "community_rewards_v1_activation_height",
            "certificate_signing_hash",
            "certificate_cryptographically_verified",
            "verified_signature_count",
            "signed_validator_addresses",
            "signed_stake",
            "total_stake",
            "source_height",
            "source_block_hash",
            "source_state_root",
            "transition_height",
            "transition_block_hash",
            "canonical_history_source",
        ],
        "receipt checkpoint",
    )?;
    for field in [
        "descriptor_sha256",
        "full_file_sha256",
        "manifest_hash",
        "payload_hash",
        "certificate_signing_hash",
        "source_block_hash",
        "source_state_root",
        "transition_block_hash",
    ] {
        expect_hash_field(checkpoint, field, "receipt checkpoint")?;
    }
    let verified = u64_field(checkpoint, "verified_signature_count", "receipt checkpoint")?;
    let signed_stake = u64_field(checkpoint, "signed_stake", "receipt checkpoint")?;
    let total_stake = u64_field(checkpoint, "total_stake", "receipt checkpoint")?;
    let signers = checkpoint
        .get("signed_validator_addresses")
        .and_then(Value::as_array)
        .context("receipt signed validator addresses are malformed")?;
    let mut distinct = BTreeSet::new();
    for signer in signers {
        let signer = signer.as_str().context("receipt signer must be a string")?;
        require_lower_hash(signer, "receipt signer address")?;
        ensure!(distinct.insert(signer), "receipt repeats a signer");
    }
    ensure!(
        bool_field(
            checkpoint,
            "certificate_cryptographically_verified",
            "receipt checkpoint"
        )? && (5..=6).contains(&verified)
            && signers.len() as u64 == verified
            && signed_stake > 0
            && total_stake > 0
            && (signed_stake as u128) * 3 > (total_stake as u128) * 2
            && u64_field(checkpoint, "format_version", "receipt checkpoint")? == 1
            && string_field(checkpoint, "chain_id", "receipt checkpoint")? == "0x415243"
            && u64_field(
                checkpoint,
                "community_rewards_v1_activation_height",
                "receipt checkpoint",
            )? == TRANSITION_HEIGHT
            && u64_field(checkpoint, "source_height", "receipt checkpoint")? == SOURCE_HEIGHT
            && u64_field(checkpoint, "transition_height", "receipt checkpoint")?
                == TRANSITION_HEIGHT
            && string_field(checkpoint, "canonical_history_source", "receipt checkpoint")?
                == "signed_recovery_checkpoint",
        "receipt checkpoint certificate/quorum binding differs"
    );
    ensure!(
        object.get("local_legacy_replay")
            == Some(&json!({
                "performed": false,
                "classification": "preserved_noncanonical_forensic_not_migrated",
                "canonical_history_source": "signed_recovery_checkpoint",
                "inspection": null,
            })),
        "forensic receipt overclaims local replay"
    );
    let offline = object_exact(
        object
            .get("offline_stability")
            .context("receipt offline stability is missing")?,
        &[
            "sample_count",
            "stability_seconds",
            "exact_process_identity_absent",
            "replacement_legacy_writer_absent",
            "recorded_listener_endpoints_absent",
            "listener_endpoints",
            "required_absent_listener_ports",
        ],
        "receipt offline stability",
    )?;
    ensure!(
        (3..=20).contains(&u64_field(
            offline,
            "sample_count",
            "receipt offline stability"
        )?) && (5..=300).contains(&u64_field(
            offline,
            "stability_seconds",
            "receipt offline stability",
        )?) && bool_field(
            offline,
            "exact_process_identity_absent",
            "receipt offline stability",
        )? && bool_field(
            offline,
            "replacement_legacy_writer_absent",
            "receipt offline stability",
        )? && bool_field(
            offline,
            "recorded_listener_endpoints_absent",
            "receipt offline stability",
        )? && offline.get("required_absent_listener_ports") == Some(&json!(LEGACY_PORTS)),
        "receipt offline stability proof is malformed"
    );
    let listener_rows = offline
        .get("listener_endpoints")
        .and_then(Value::as_array)
        .context("receipt offline listener endpoints are malformed")?;
    let mut prior: Option<(String, String, u64)> = None;
    for (index, row) in listener_rows.iter().enumerate() {
        let row = object_exact(
            row,
            &["family", "address_hex", "port"],
            &format!("receipt offline listener #{index}"),
        )?;
        let current = (
            string_field(row, "family", "receipt offline listener")?.to_owned(),
            string_field(row, "address_hex", "receipt offline listener")?.to_owned(),
            u64_field(row, "port", "receipt offline listener")?,
        );
        ensure!(
            matches!(current.0.as_str(), "tcp4" | "tcp6"),
            "receipt listener family differs"
        );
        ensure!(
            (1..=65535).contains(&current.2),
            "receipt listener port is invalid"
        );
        if let Some(previous) = &prior {
            ensure!(
                previous < &current,
                "receipt listener endpoints are not strictly sorted"
            );
        }
        prior = Some(current);
    }
    let tree = object_exact(
        object
            .get("old_data_tree")
            .context("receipt old data tree is missing")?,
        &[
            "path",
            "root_sha256",
            "entry_count",
            "total_file_bytes",
            "state_wal_sha256",
            "intent_wal_prefix_bytes",
            "intent_wal_prefix_sha256",
        ],
        "receipt old data tree",
    )?;
    require_absolute_normal(
        Path::new(string_field(tree, "path", "receipt old data tree")?),
        "receipt old data tree path",
    )?;
    expect_hash_field(tree, "root_sha256", "receipt old data tree")?;
    expect_hash_field(tree, "state_wal_sha256", "receipt old data tree")?;
    expect_hash_field(tree, "intent_wal_prefix_sha256", "receipt old data tree")?;
    ensure!(
        u64_field(tree, "entry_count", "receipt old data tree")? > 0
            && u64_field(tree, "total_file_bytes", "receipt old data tree")? > 0
            && u64_field(tree, "intent_wal_prefix_bytes", "receipt old data tree")? > 0,
        "receipt old data tree is empty"
    );
    ensure!(
        object.get("retirement_result")
            == Some(&json!({
                "retired": true,
                "stake": 0,
                "legacy_process_stably_absent": true,
                "legacy_listeners_stably_absent": true,
                "sigkill_sent": false,
                "legacy_exit_clean_claimed": false,
                "legacy_jobs_disposition": JOBS_DISPOSITION,
                "legacy_data_opened_writable_by_verifier": false,
                "legacy_data_changed_during_verification": false,
                "legacy_data_disposition": "preserved_noncanonical_forensic_not_migrated",
                "canonical_history_source": "signed_recovery_checkpoint",
                "old_wal_copied_to_v08": false,
                "v08_data_dir_fresh_at_receipt": true,
                "canonical_chain_history_rewritten": false,
            })),
        "retirement receipt result is dishonest or unsupported"
    );
    Ok(())
}

fn validate_existing_receipt_bindings(
    receipt: &Value,
    intent: &Value,
    stop: &Value,
    intent_sha256: &str,
    stop_sha256: &str,
    stability_seconds: u64,
    samples: u32,
) -> Result<()> {
    ensure!(
        receipt["intent_sha256"].as_str() == Some(intent_sha256)
            && receipt["stop_evidence_sha256"].as_str() == Some(stop_sha256)
            && receipt["protocol_id"] == intent["protocol_id"]
            && receipt["scope"] == intent["scope"],
        "existing receipt belongs to another protocol execution"
    );
    let release = &intent["target_release"];
    ensure!(
        receipt["target_release"]
            == json!({
                "tag": release["tag"],
                "commit": release["commit"],
                "binding_schema": release["binding_schema"],
                "binding_sha256": release["binding_sha256"],
                "manifest_sha256": release["manifest_sha256"],
                "manifest_signature_sha256": release.get("manifest_signature_sha256").cloned().unwrap_or(Value::Null),
                "local_inspector_sha256": null,
            }),
        "existing receipt release binding differs from intent"
    );
    ensure!(
        receipt["maintenance_boundary"]
            == json!({
                "sha256": intent["maintenance_boundary"]["sha256"],
                "observed_cutoff_height": intent["maintenance_boundary"]["observed_cutoff_height"],
                "legacy_public_max_height": intent["maintenance_boundary"]["legacy_public_max_height"],
                "global_absence_claimed": false,
            })
            && receipt["cutover_policy"]
                == json!({
                    "sha256": intent["cutover_policy"]["sha256"],
                    "uncompleted_job_disposition": JOBS_DISPOSITION,
                    "legacy_exit_clean_claimed": false,
                    "legacy_restart_allowed": false,
                    "global_legacy_absence_claimed": false,
                    "offline_retirement_receipt_required": true,
                    "v08_start_requires_offline_receipt": true,
                }),
        "existing receipt cutover binding differs from intent"
    );
    ensure!(
        receipt["checkpoint"] == expected_receipt_checkpoint(intent),
        "existing receipt checkpoint differs from intent"
    );
    let process = &intent["old_process"];
    ensure!(
        receipt["old_process"]
            == json!({
                "retirement_mode": process["retirement_mode"],
                "pid": process["pid"],
                "boot_id": process["boot_id"],
                "start_ticks": process["start_ticks"],
                "legacy_version": intent["legacy_release"]["version"],
                "executable_sha256": intent["legacy_release"]["executable_sha256"],
                "signals_sent": stop["supervisor"]["signals_sent"],
                "exit_status_observed": stop["supervisor"]["exit_status_observed"],
            }),
        "existing receipt process binding differs from stop evidence"
    );
    let tree = object_exact(
        &receipt["old_data_tree"],
        &[
            "path",
            "root_sha256",
            "entry_count",
            "total_file_bytes",
            "state_wal_sha256",
            "intent_wal_prefix_bytes",
            "intent_wal_prefix_sha256",
        ],
        "existing receipt old data tree",
    )?;
    ensure!(
        tree.get("path") == intent["old_data"]["root_anchor"].get("path")
            && tree.get("intent_wal_prefix_bytes")
                == intent["old_data"]["wal_prefix"].get("observed_prefix_bytes")
            && tree.get("intent_wal_prefix_sha256")
                == intent["old_data"]["wal_prefix"].get("observed_prefix_sha256"),
        "existing receipt old-data binding differs from intent"
    );
    expect_hash_field(tree, "root_sha256", "existing receipt old tree")?;
    expect_hash_field(tree, "state_wal_sha256", "existing receipt old tree")?;
    ensure!(
        u64_field(tree, "entry_count", "existing receipt old tree")? > 0,
        "existing receipt tree is empty"
    );
    ensure!(
        u64_field(tree, "total_file_bytes", "existing receipt old tree")? > 0,
        "existing receipt tree has no bytes"
    );
    ensure!(
        receipt["v08_start"]
            == json!({
                "data_dir": intent["v08_start"]["data_dir"],
                "data_dir_fresh_and_absent": true,
                "canonical_history_source": "signed_recovery_checkpoint",
                "old_wal_migration_allowed": false,
            }),
        "existing receipt v0.8 start binding differs from intent"
    );
    let expected_endpoints = intent["old_process"]["listeners"]
        .as_array()
        .expect("validated intent listeners")
        .iter()
        .map(|row| {
            json!({
                "family": row["family"],
                "address_hex": row["address_hex"],
                "port": row["port"],
            })
        })
        .collect::<Vec<_>>();
    ensure!(
        receipt["offline_stability"]["sample_count"].as_u64() == Some(samples as u64)
            && receipt["offline_stability"]["stability_seconds"].as_u64()
                == Some(stability_seconds)
            && receipt["offline_stability"]["listener_endpoints"] == json!(expected_endpoints),
        "existing receipt offline proof differs from this finalize request or intent"
    );
    Ok(())
}

fn revalidate_intent_assets<H: RetirementHost>(intent: &Value, host: &H) -> Result<String> {
    let release_intent = intent["target_release"]
        .as_object()
        .context("intent target release is malformed")?;
    let boundary_intent = intent["maintenance_boundary"]
        .as_object()
        .context("intent maintenance boundary is malformed")?;
    let policy_intent = intent["cutover_policy"]
        .as_object()
        .context("intent cutover policy is malformed")?;
    let checkpoint_intent = intent["checkpoint"]
        .as_object()
        .context("intent checkpoint is malformed")?;

    let release_path = PathBuf::from(string_field(
        release_intent,
        "binding_path",
        "intent target release",
    )?);
    let release_sha = string_field(release_intent, "binding_sha256", "intent target release")?;
    let (release_value, _release_raw, _) = load_canonical_json(
        &release_path,
        "target release binding",
        release_sha,
        MAX_JSON_BYTES,
    )?;
    let release = validate_release_binding(
        &release_value,
        string_field(policy_intent, "sha256", "intent cutover policy")?,
        string_field(boundary_intent, "sha256", "intent maintenance boundary")?,
        string_field(checkpoint_intent, "descriptor_sha256", "intent checkpoint")?,
    )?;
    let expected_release = merge_fields(
        release.projected.clone(),
        [
            (
                "binding_path",
                Value::String(path_string(&release_path, "target release")?),
            ),
            ("binding_sha256", Value::String(release_sha.to_owned())),
        ],
    );
    ensure!(
        expected_release == intent["target_release"],
        "target release differs from intent"
    );

    let boundary_path = PathBuf::from(string_field(
        boundary_intent,
        "path",
        "intent maintenance boundary",
    )?);
    let boundary_sha = string_field(boundary_intent, "sha256", "intent maintenance boundary")?;
    let (boundary_value, _boundary_raw, _) = load_canonical_json(
        &boundary_path,
        "maintenance boundary",
        boundary_sha,
        MAX_JSON_BYTES,
    )?;
    let boundary = validate_boundary(&boundary_value)?;
    let expected_boundary = merge_fields(
        boundary.projected.clone(),
        [
            (
                "path",
                Value::String(path_string(&boundary_path, "maintenance boundary")?),
            ),
            ("sha256", Value::String(boundary_sha.to_owned())),
        ],
    );
    ensure!(
        expected_boundary == intent["maintenance_boundary"],
        "maintenance boundary differs from intent"
    );

    let checkpoint_path = PathBuf::from(string_field(
        checkpoint_intent,
        "path",
        "intent checkpoint",
    )?);
    let checkpoint_sha = string_field(checkpoint_intent, "descriptor_sha256", "intent checkpoint")?;
    let (descriptor_value, descriptor_raw, descriptor_record) = load_canonical_json(
        &checkpoint_path,
        "checkpoint descriptor",
        checkpoint_sha,
        MAX_DESCRIPTOR_BYTES,
    )?;
    let verified = host.verify_descriptor(&checkpoint_path)?;
    ensure!(
        verified.status == "VERIFIED_DESCRIPTOR_QUORUM",
        "checkpoint descriptor is not verified quorum"
    );
    let (descriptor_after, descriptor_raw_after, descriptor_record_after) = load_canonical_json(
        &checkpoint_path,
        "checkpoint descriptor after cryptographic verification",
        checkpoint_sha,
        MAX_DESCRIPTOR_BYTES,
    )?;
    ensure!(
        descriptor_after == descriptor_value
            && descriptor_raw_after == descriptor_raw
            && descriptor_record_after == descriptor_record,
        "checkpoint descriptor changed while cryptographically verified"
    );
    let checkpoint =
        validate_descriptor_projection(&descriptor_value, &release, &boundary, &verified)?;
    let expected_checkpoint = merge_fields(
        checkpoint.clone(),
        [
            (
                "path",
                Value::String(path_string(&checkpoint_path, "checkpoint descriptor")?),
            ),
            (
                "descriptor_sha256",
                Value::String(checkpoint_sha.to_owned()),
            ),
        ],
    );
    ensure!(
        expected_checkpoint == intent["checkpoint"],
        "checkpoint descriptor differs from intent"
    );

    let policy_path = PathBuf::from(string_field(
        policy_intent,
        "path",
        "intent cutover policy",
    )?);
    let policy_sha = string_field(policy_intent, "sha256", "intent cutover policy")?;
    let (policy_value, _policy_raw, _) =
        load_canonical_json(&policy_path, "cutover policy", policy_sha, MAX_JSON_BYTES)?;
    let policy = validate_policy(
        &policy_value,
        &release,
        &boundary,
        boundary_sha,
        &checkpoint,
        checkpoint_sha,
    )?;
    let expected_policy = merge_fields(
        policy,
        [
            (
                "path",
                Value::String(path_string(&policy_path, "cutover policy")?),
            ),
            ("sha256", Value::String(policy_sha.to_owned())),
        ],
    );
    ensure!(
        expected_policy == intent["cutover_policy"],
        "cutover policy differs from intent"
    );
    let cutoff = policy_value["legacy_admission_cutoff_utc"]
        .as_str()
        .context("cutover admission cutoff is malformed")?;
    validate_utc(cutoff, "cutover legacy admission cutoff")?;
    Ok(cutoff.to_owned())
}

// The arguments deliberately mirror the signed CLI boundary one-for-one. Grouping
// independent paths and digests would make a security-sensitive binding less clear.
#[allow(clippy::too_many_arguments)]
fn finalize_with_host<H: RetirementHost>(
    intent_path: &Path,
    expected_intent_sha256: &str,
    stop_evidence_path: &Path,
    expected_stop_evidence_sha256: &str,
    receipt_output: &Path,
    stability_seconds: u64,
    samples: u32,
    host: &H,
) -> Result<(Value, String)> {
    require_lower_hash(expected_intent_sha256, "retirement intent SHA-256")?;
    require_lower_hash(
        expected_stop_evidence_sha256,
        "offline stop evidence SHA-256",
    )?;
    let (intent, _intent_raw, _) = load_canonical_json(
        intent_path,
        "retirement intent",
        expected_intent_sha256,
        MAX_JSON_BYTES,
    )?;
    let view = validate_intent(&intent)?;
    ensure_output_path(receipt_output, &view.data_dir, "retirement receipt output")?;
    let (stop, _stop_raw, _) = load_canonical_json(
        stop_evidence_path,
        "offline stop evidence",
        expected_stop_evidence_sha256,
        MAX_JSON_BYTES,
    )?;
    validate_stop_evidence(&stop, &intent, expected_intent_sha256)?;
    if receipt_output.exists() {
        let (existing, raw, _) = load_canonical_unpinned(
            receipt_output,
            "existing retirement receipt",
            MAX_JSON_BYTES,
        )?;
        validate_receipt(&existing)?;
        validate_existing_receipt_bindings(
            &existing,
            &intent,
            &stop,
            expected_intent_sha256,
            expected_stop_evidence_sha256,
            stability_seconds,
            samples,
        )?;
        return Ok((existing, sha256(&raw)));
    }
    ensure_disjoint_absent_v08(&view.v08_data_dir, &view.data_dir)?;

    let legacy = intent["legacy_release"]
        .as_object()
        .context("intent legacy release is malformed")?;
    let executable_expected = legacy
        .get("executable")
        .context("intent legacy executable is missing")?;
    let executable_path = PathBuf::from(
        executable_expected["path"]
            .as_str()
            .context("intent executable path is malformed")?,
    );
    let executable = stable_hash_file(
        &executable_path,
        "legacy v0.7 executable",
        MAX_EXECUTABLE_BYTES,
    )?;
    ensure!(
        executable.sha256 == string_field(legacy, "executable_sha256", "intent legacy release")?,
        "legacy executable hash differs from intent"
    );
    same_file_binding(&executable, executable_expected, "legacy v0.7 executable")?;
    let supervisor_expected = legacy
        .get("supervisor_definition")
        .context("intent supervisor definition is missing")?;
    let supervisor_path = PathBuf::from(
        supervisor_expected["path"]
            .as_str()
            .context("intent supervisor path is malformed")?,
    );
    let (supervisor_value, _supervisor_raw, supervisor_record) = load_canonical_json(
        &supervisor_path,
        "legacy supervisor definition",
        supervisor_expected["sha256"]
            .as_str()
            .context("intent supervisor hash is malformed")?,
        MAX_JSON_BYTES,
    )?;
    same_file_binding(
        &supervisor_record,
        supervisor_expected,
        "legacy supervisor definition",
    )?;
    let (semantics, supervisor_source) = validate_supervisor(
        &supervisor_value,
        &view.data_dir,
        &executable_path,
        &executable.sha256,
    )?;
    ensure!(
        semantics == intent["old_process"]["stake_zero_semantics"],
        "legacy supervisor stake-zero semantics differ from intent"
    );
    same_file_binding(
        &supervisor_source,
        &legacy["supervisor_source"],
        "legacy supervisor source",
    )?;
    if view.mode.label() == "term_only" {
        let argv = supervisor_value["argv"]
            .as_array()
            .expect("validated argv")
            .iter()
            .map(|value| value.as_str().expect("validated argv string").to_owned())
            .collect::<Vec<_>>();
        ensure!(
            intent["old_process"]["argv_sha256"].as_str() == Some(argv_sha256(&argv).as_str()),
            "supervisor argv differs from intent process binding"
        );
    }

    let cutoff = revalidate_intent_assets(&intent, host)?;
    ensure!(
        chrono::DateTime::parse_from_rfc3339(
            stop["offline_observed_at"]
                .as_str()
                .context("offline evidence time is malformed")?,
        )? >= chrono::DateTime::parse_from_rfc3339(&cutoff)?,
        "offline evidence predates the global legacy-admission cutoff"
    );

    let root_now = directory_record(&view.data_dir, "legacy data directory")?;
    let root_expected = intent["old_data"]["root_anchor"]
        .as_object()
        .context("intent data root is malformed")?;
    for field in ["path", "device", "inode", "mode", "uid", "gid", "nlink"] {
        ensure!(
            root_now.get(field) == root_expected.get(field),
            "legacy data-directory {field} differs from intent"
        );
    }
    verify_wal_prefix(
        &view.data_dir.join("state.wal"),
        &intent["old_data"]["wal_prefix"],
    )?;
    let process = intent["old_process"]
        .as_object()
        .context("intent process is malformed")?;
    let data_dir_text = path_string(&view.data_dir, "legacy data directory")?;
    let offline = prove_stably_offline(
        host,
        process,
        view.legacy_owner_uid,
        &data_dir_text,
        stability_seconds,
        samples,
    )?;
    let tree_before = tree_snapshot(&view.data_dir)?;
    let tree_after = tree_snapshot(&view.data_dir)?;
    ensure!(
        tree_after == tree_before,
        "legacy data tree changed during verification"
    );
    let offline_after = prove_stably_offline(
        host,
        process,
        view.legacy_owner_uid,
        &data_dir_text,
        stability_seconds,
        samples,
    )?;
    ensure!(
        offline_after == offline,
        "offline process/listener proof changed during verification"
    );
    let _v08_namespace_lock =
        arc_crypto::secret_file::try_acquire_private_directory_namespace_lock(&view.v08_data_dir)
            .with_context(|| {
            format!(
                "cannot exclusively lock fresh v0.8 namespace {}",
                view.v08_data_dir.display()
            )
        })?;
    ensure_disjoint_absent_v08(&view.v08_data_dir, &view.data_dir)?;

    let verified_at = host.now();
    validate_utc(&verified_at, "receipt verified_at")?;
    ensure!(
        chrono::DateTime::parse_from_rfc3339(&verified_at)?
            >= chrono::DateTime::parse_from_rfc3339(
                stop["offline_observed_at"]
                    .as_str()
                    .expect("validated stop time"),
            )?,
        "receipt verification time predates offline evidence"
    );
    let release = &intent["target_release"];
    let stop_supervisor = &stop["supervisor"];
    let receipt = json!({
        "schema": RECEIPT_SCHEMA,
        "verified_at": verified_at,
        "intent_sha256": expected_intent_sha256,
        "stop_evidence_sha256": expected_stop_evidence_sha256,
        "protocol_id": intent["protocol_id"],
        "scope": intent["scope"],
        "target_release": {
            "tag": release["tag"],
            "commit": release["commit"],
            "binding_schema": release["binding_schema"],
            "binding_sha256": release["binding_sha256"],
            "manifest_sha256": release["manifest_sha256"],
            "manifest_signature_sha256": release.get("manifest_signature_sha256").cloned().unwrap_or(Value::Null),
            "local_inspector_sha256": null,
        },
        "maintenance_boundary": {
            "sha256": intent["maintenance_boundary"]["sha256"],
            "observed_cutoff_height": intent["maintenance_boundary"]["observed_cutoff_height"],
            "legacy_public_max_height": intent["maintenance_boundary"]["legacy_public_max_height"],
            "global_absence_claimed": false,
        },
        "cutover_policy": {
            "sha256": intent["cutover_policy"]["sha256"],
            "uncompleted_job_disposition": JOBS_DISPOSITION,
            "legacy_exit_clean_claimed": false,
            "legacy_restart_allowed": false,
            "global_legacy_absence_claimed": false,
            "offline_retirement_receipt_required": true,
            "v08_start_requires_offline_receipt": true,
        },
        "checkpoint": expected_receipt_checkpoint(&intent),
        "old_process": {
            "retirement_mode": intent["old_process"]["retirement_mode"],
            "pid": intent["old_process"]["pid"],
            "boot_id": intent["old_process"]["boot_id"],
            "start_ticks": intent["old_process"]["start_ticks"],
            "legacy_version": intent["legacy_release"]["version"],
            "executable_sha256": intent["legacy_release"]["executable_sha256"],
            "signals_sent": stop_supervisor["signals_sent"],
            "exit_status_observed": stop_supervisor["exit_status_observed"],
        },
        "offline_stability": offline,
        "old_data_tree": {
            "path": data_dir_text,
            "root_sha256": tree_before.root_sha256,
            "entry_count": tree_before.entry_count,
            "total_file_bytes": tree_before.total_file_bytes,
            "state_wal_sha256": tree_before.state_wal_sha256,
            "intent_wal_prefix_bytes": intent["old_data"]["wal_prefix"]["observed_prefix_bytes"],
            "intent_wal_prefix_sha256": intent["old_data"]["wal_prefix"]["observed_prefix_sha256"],
        },
        "v08_start": {
            "data_dir": intent["v08_start"]["data_dir"],
            "data_dir_fresh_and_absent": true,
            "canonical_history_source": "signed_recovery_checkpoint",
            "old_wal_migration_allowed": false,
        },
        "local_legacy_replay": {
            "performed": false,
            "classification": "preserved_noncanonical_forensic_not_migrated",
            "canonical_history_source": "signed_recovery_checkpoint",
            "inspection": null,
        },
        "retirement_result": {
            "retired": true,
            "stake": 0,
            "legacy_process_stably_absent": true,
            "legacy_listeners_stably_absent": true,
            "sigkill_sent": false,
            "legacy_exit_clean_claimed": false,
            "legacy_jobs_disposition": JOBS_DISPOSITION,
            "legacy_data_opened_writable_by_verifier": false,
            "legacy_data_changed_during_verification": false,
            "legacy_data_disposition": "preserved_noncanonical_forensic_not_migrated",
            "canonical_history_source": "signed_recovery_checkpoint",
            "old_wal_copied_to_v08": false,
            "v08_data_dir_fresh_at_receipt": true,
            "canonical_chain_history_rewritten": false,
        },
    });
    validate_receipt(&receipt)?;
    ensure_disjoint_absent_v08(&view.v08_data_dir, &view.data_dir)?;
    let digest = publish_create_only(receipt_output, &receipt, "retirement receipt")?;
    Ok((receipt, digest))
}

fn create_intent_summary(intent: &Value, digest: &str, output: &Path) -> Result<Value> {
    let mode = intent["old_process"]["retirement_mode"]
        .as_str()
        .context("validated intent lost its retirement mode")?;
    let process_identity = if mode == "term_only" {
        json!({
            "boot_id": intent["old_process"]["boot_id"],
            "pid": intent["old_process"]["pid"],
            "start_ticks": intent["old_process"]["start_ticks"],
        })
    } else {
        Value::Null
    };
    Ok(json!({
        "status": "RETIREMENT_INTENT_CREATED",
        "schema": INTENT_SCHEMA,
        "path": path_string(output, "retirement intent")?,
        "sha256": digest,
        "retirement_mode": mode,
        "process_identity": process_identity,
        "legacy_pid": intent["old_process"]["pid"],
        "legacy_boot_id": intent["old_process"]["boot_id"],
        "legacy_start_ticks": intent["old_process"]["start_ticks"],
    }))
}

pub(crate) fn run(command: LegacyRetirementCommand) -> Result<()> {
    match command {
        LegacyRetirementCommand::CreateIntent {
            intent_output,
            target_release,
            target_release_sha256,
            maintenance_boundary,
            maintenance_boundary_sha256,
            cutover_policy,
            cutover_policy_sha256,
            checkpoint_descriptor,
            checkpoint_descriptor_sha256,
            legacy_pid,
            already_offline,
            legacy_version,
            legacy_executable,
            legacy_executable_sha256,
            supervisor_definition,
            supervisor_definition_sha256,
            data_dir,
            v08_data_dir,
            forensic_only,
        } => {
            ensure!(
                forensic_only,
                "compiled legacy retirement requires --forensic-only"
            );
            let mode = match (legacy_pid, already_offline) {
                (Some(pid), false) => RetirementMode::TermOnly(pid),
                (None, true) => RetirementMode::PreexistingOffline,
                _ => bail!("select exactly one of --legacy-pid or --already-offline"),
            };
            let request = CreateRequest {
                intent_output,
                target_release,
                target_release_sha256,
                maintenance_boundary,
                maintenance_boundary_sha256,
                cutover_policy,
                cutover_policy_sha256,
                checkpoint_descriptor,
                checkpoint_descriptor_sha256,
                mode,
                legacy_version,
                legacy_executable,
                legacy_executable_sha256,
                supervisor_definition,
                supervisor_definition_sha256,
                data_dir,
                v08_data_dir,
            };
            let host = SystemHost::default();
            let (intent, digest) = create_intent_with_host(&request, &host)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&create_intent_summary(
                    &intent,
                    &digest,
                    &request.intent_output,
                )?)?
            );
            Ok(())
        }
        LegacyRetirementCommand::Finalize {
            intent,
            intent_sha256,
            stop_evidence,
            stop_evidence_sha256,
            receipt_output,
            stability_seconds,
            samples,
        } => {
            let host = SystemHost::default();
            let (_receipt, digest) = finalize_with_host(
                &intent,
                &intent_sha256,
                &stop_evidence,
                &stop_evidence_sha256,
                &receipt_output,
                stability_seconds,
                samples,
                &host,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "status": "RETIREMENT_RECEIPT_CREATED",
                    "schema": RECEIPT_SCHEMA,
                    "path": path_string(&receipt_output, "retirement receipt")?,
                    "sha256": digest,
                }))?
            );
            Ok(())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use clap::Parser as _;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io;
    use std::os::unix::fs::PermissionsExt as _;

    fn hash_number(value: u8) -> String {
        format!("{value:064x}")
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn write_bytes(path: &Path, bytes: &[u8], mode: u32) -> String {
        std::fs::write(path, bytes).unwrap();
        set_mode(path, mode);
        sha256(bytes)
    }

    fn write_json(path: &Path, value: &Value) -> String {
        let bytes = canonical_bytes(value).unwrap();
        write_bytes(path, &bytes, 0o600)
    }

    #[test]
    fn canonical_json_matches_python_ensure_ascii_for_unicode_and_controls() {
        let value = json!({
            "z": "é😀",
            "a": "\u{0001}\u{0008}\u{007f}",
            "é": [true, 7, null],
        });
        assert_eq!(
            String::from_utf8(canonical_bytes(&value).unwrap()).unwrap(),
            "{\"a\":\"\\u0001\\b\\u007f\",\"z\":\"\\u00e9\\ud83d\\ude00\",\"\\u00e9\":[true,7,null]}\n"
        );
        assert!(canonical_bytes(&json!({"fraction": 1.5})).is_err());
    }

    #[test]
    fn process_executable_hash_cache_is_identity_stable_and_count_bounded() {
        let temporary = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(temporary.path())
            .unwrap()
            .join("candidate");
        write_bytes(&path, b"one stable executable identity", 0o500);
        let record = stable_hash_file(&path, "test executable", MAX_EXECUTABLE_BYTES).unwrap();
        let identity = ProcessExecutableIdentity::from_record(&record);
        let cache = ProcessExecutableHashCache::default();
        assert_eq!(
            cache.get_or_hash(identity, || Ok(record.clone())).unwrap(),
            record
        );
        assert_eq!(
            cache
                .get_or_hash(identity, || bail!("cache miss for stable identity"))
                .unwrap(),
            record
        );
        assert_eq!(cache.hash_operations(), 1);
    }

    #[derive(Default)]
    struct FakeHost {
        now: RefCell<String>,
        descriptor: RefCell<Option<recovery_descriptor::VerifiedDescriptorSummary>>,
        processes: RefCell<HashMap<u32, ProcessObservation>>,
        observe_errors: RefCell<BTreeSet<u32>>,
        listeners: RefCell<Vec<ListenerEndpoint>>,
        descriptor_swap: RefCell<Option<(PathBuf, Vec<u8>)>>,
        sleep_calls: RefCell<Vec<Duration>>,
    }

    impl FakeHost {
        fn new(summary: recovery_descriptor::VerifiedDescriptorSummary) -> Self {
            Self {
                now: RefCell::new("2026-02-01T00:00:00Z".into()),
                descriptor: RefCell::new(Some(summary)),
                ..Self::default()
            }
        }
    }

    impl RetirementHost for FakeHost {
        fn now(&self) -> String {
            self.now.borrow().clone()
        }

        fn sleep(&self, duration: Duration) {
            self.sleep_calls.borrow_mut().push(duration);
        }

        fn verify_descriptor(
            &self,
            _path: &Path,
        ) -> Result<recovery_descriptor::VerifiedDescriptorSummary> {
            if let Some((path, bytes)) = self.descriptor_swap.borrow_mut().take() {
                std::fs::write(path, bytes)?;
            }
            self.descriptor
                .borrow()
                .clone()
                .context("test descriptor summary is absent")
        }

        fn observe_process(&self, pid: u32) -> Result<Option<ProcessObservation>> {
            if self.observe_errors.borrow().contains(&pid) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "test denial").into());
            }
            Ok(self.processes.borrow().get(&pid).cloned())
        }

        fn all_process_ids(&self) -> Result<Vec<u32>> {
            let mut values = self
                .processes
                .borrow()
                .keys()
                .copied()
                .chain(self.observe_errors.borrow().iter().copied())
                .collect::<Vec<_>>();
            values.sort_unstable();
            values.dedup();
            Ok(values)
        }

        fn active_listener_endpoints(&self) -> Result<Vec<ListenerEndpoint>> {
            Ok(self.listeners.borrow().clone())
        }
    }

    struct Fixture {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        request: CreateRequest,
        summary: recovery_descriptor::VerifiedDescriptorSummary,
        supervisor_argv: Vec<String>,
    }

    impl Fixture {
        fn new(mode: RetirementMode) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temporary.path()).unwrap();
            set_mode(&root, 0o700);
            let data_dir = root.join("data-v0.7");
            std::fs::create_dir(&data_dir).unwrap();
            set_mode(&data_dir, 0o700);
            write_bytes(&data_dir.join("state.wal"), b"legacy-wal-prefix\n", 0o600);
            write_bytes(&data_dir.join("observer.db"), b"fork-local\n", 0o600);
            let executable = root.join("arc-node-v0.7");
            let executable_sha = write_bytes(&executable, b"legacy executable\n", 0o500);
            let supervisor_source = root.join("legacy.service");
            let source_sha = write_bytes(&supervisor_source, b"ExecStart=arc-node\n", 0o600);
            let supervisor_argv = vec![
                path_string(&executable, "test executable").unwrap(),
                "--stake".into(),
                "0".into(),
                "--min-stake".into(),
                "0".into(),
                "--data-dir".into(),
                path_string(&data_dir, "test data directory").unwrap(),
                "--community-mode".into(),
            ];
            let supervisor = root.join("supervisor.json");
            let supervisor_sha = write_json(
                &supervisor,
                &json!({
                    "schema": SUPERVISOR_SCHEMA,
                    "kind": "manual",
                    "source_path": path_string(&supervisor_source, "test source").unwrap(),
                    "source_sha256": source_sha,
                    "executable_path": path_string(&executable, "test executable").unwrap(),
                    "executable_sha256": executable_sha,
                    "argv": supervisor_argv,
                }),
            );

            let freeze = hash_number(90);
            let capture = hash_number(91);
            let boundary_path = root.join(BOUNDARY_ASSET);
            let boundary_sha = write_json(
                &boundary_path,
                &json!({
                    "schema": BOUNDARY_SCHEMA,
                    "source_main_commit": "a".repeat(40),
                    "observed_cutoff_height": 137017,
                    "continuity_safety_margin": 128,
                    "legacy_public_max_height": SOURCE_HEIGHT,
                    "freeze_plan_sha256": freeze,
                    "capture_id": capture,
                    "first_quarantine_started_at": "2026-01-01T00:00:00Z",
                    "all_controlled_stopped_at": "2026-01-01T00:01:00Z",
                    "global_absence_claimed": false,
                    "official_origin_scope": {"global_absence_claimed": false},
                    "threat_model": {"hostile_root_containment_claimed": false},
                }),
            );
            let validators = FLEET
                .iter()
                .enumerate()
                .map(|(index, (name, host))| {
                    json!({
                        "name": name,
                        "host": host,
                        "origin": format!("http://{host}:9090"),
                        "address": hash_number(index as u8 + 1),
                        "stake": 10,
                    })
                })
                .collect::<Vec<_>>();
            let signed_addresses = validators[..5]
                .iter()
                .map(|row| row["address"].clone())
                .collect::<Vec<_>>();
            let quorum = json!({
                "status": "VERIFIED_QUORUM",
                "required_signatures": 5,
                "verified_signature_count": 5,
                "validator_count": 6,
                "signed_validator_addresses": signed_addresses,
                "signed_stake": 50,
                "total_stake": 60,
            });
            let manifest = hash_number(20);
            let signing = hash_number(21);
            let network = hash_number(22);
            let recovery_domain = hash_number(23);
            let payload = hash_number(24);
            let full_root = hash_number(25);
            let source_block = hash_number(26);
            let source_root = hash_number(27);
            let transition_block = hash_number(28);
            let recovery_manifest_sha = hash_number(29);
            let inspector_sha = hash_number(30);
            let checkpoint_file_sha = hash_number(31);
            let descriptor_path = root.join(DESCRIPTOR_ASSET);
            let descriptor = json!({
                "schema_version": DESCRIPTOR_SCHEMA,
                "repository": REPOSITORY,
                "release_tag": "v0.8.0",
                "release_commit": "b".repeat(40),
                "recovery_manifest_sha256": recovery_manifest_sha,
                "freeze_plan_sha256": freeze,
                "capture_id": capture,
                "inspector_binary_sha256": inspector_sha,
                "checkpoint_file": {
                    "filename": "recovery.arcchkpt",
                    "size_bytes": 4096,
                    "sha256": checkpoint_file_sha,
                },
                "canonical_inspection": {
                    "format_version": 1,
                    "chain_id": "0x415243",
                    "manifest_hash": manifest,
                    "payload_hash": payload,
                    "network_genesis_hash": network,
                    "full_state_root": full_root,
                    "source_height": SOURCE_HEIGHT,
                    "source_consensus_round": 137200,
                    "created_at_unix_ms": 1767225660000u64,
                    "source_block_hash": source_block,
                    "source_state_root": source_root,
                    "transition_height": TRANSITION_HEIGHT,
                    "transition_block_hash": transition_block,
                    "recovery_domain": recovery_domain,
                    "recovery_epoch": 1,
                    "validator_set_id": 1,
                    "protocol_version": "3.0.0",
                    "validator_count": 6,
                    "community_rewards_v1_activation_height": TRANSITION_HEIGHT,
                },
                "checkpoint_certificate": {
                    "signing_hash": signing,
                    "validators": [],
                    "signatures": [],
                },
                "approved_validators": validators,
                "verified_quorum": quorum,
            });
            let descriptor_sha = write_json(&descriptor_path, &descriptor);
            let policy_path = root.join(POLICY_ASSET);
            let policy_sha = write_json(
                &policy_path,
                &json!({
                    "schema_version": POLICY_SCHEMA,
                    "repository": REPOSITORY,
                    "release_tag": "v0.8.0",
                    "release_commit": "b".repeat(40),
                    "recovery_manifest_sha256": recovery_manifest_sha,
                    "legacy_maintenance_boundary_sha256": boundary_sha,
                    "recovery_checkpoint_descriptor_sha256": descriptor_sha,
                    "recovery_checkpoint_file_sha256": checkpoint_file_sha,
                    "freeze_plan_sha256": freeze,
                    "capture_id": capture,
                    "first_quarantine_started_at": "2026-01-01T00:00:00Z",
                    "all_controlled_stopped_at": "2026-01-01T00:01:00Z",
                    "legacy_admission_cutoff_utc": "2026-01-01T00:01:00Z",
                    "canonical_boundary_height": SOURCE_HEIGHT,
                    "required_post_cutover_min_height": TRANSITION_HEIGHT,
                    "required_recovery_epoch": 1,
                    "required_validator_set_id": 1,
                    "required_validator_count": 6,
                    "checkpoint_format_version": 1,
                    "chain_id": "0x415243",
                    "protocol_version": "3.0.0",
                    "payload_hash": payload,
                    "community_rewards_v1_activation_height": TRANSITION_HEIGHT,
                    "network_genesis_hash": network,
                    "source_block_hash": source_block,
                    "source_state_root": source_root,
                    "transition_block_hash": transition_block,
                    "full_state_root": full_root,
                    "recovery_domain": recovery_domain,
                    "checkpoint_manifest_hash": manifest,
                    "checkpoint_source_consensus_round": 137200,
                    "checkpoint_created_at_unix_ms": 1767225660000u64,
                    "checkpoint_quorum": quorum,
                    "legacy_validators": validators,
                    "legacy_worker_rpc": {
                        "claim_path": "/community/claim_work",
                        "submit_path": "/community/submit_work",
                        "listener_ports": [9090, 3001],
                    },
                    "uncompleted_job_disposition": JOBS_DISPOSITION,
                    "legacy_exit_clean_claimed": false,
                    "legacy_restart_allowed": false,
                    "global_legacy_absence_claimed": false,
                    "offline_retirement_receipt_required": true,
                    "v08_start_requires_offline_receipt": true,
                }),
            );
            let release_path = root.join("arc-release-installer-binding.json");
            let release_sha = write_json(
                &release_path,
                &json!({
                    "schema": INSTALLER_BINDING_SCHEMA,
                    "repository": REPOSITORY,
                    "tag": "v0.8.0",
                    "commit": "b".repeat(40),
                    "signed_manifest_sha256": hash_number(70),
                    "manifest_signature_sha256": hash_number(71),
                    "files": {
                        "arc-node-linux-x86_64": inspector_sha,
                        POLICY_ASSET: policy_sha,
                        BOUNDARY_ASSET: boundary_sha,
                        DESCRIPTOR_ASSET: descriptor_sha,
                    },
                }),
            );
            let summary = recovery_descriptor::VerifiedDescriptorSummary {
                status: "VERIFIED_DESCRIPTOR_QUORUM",
                manifest_hash: manifest,
                signing_hash: signing,
                network_genesis_hash: network,
                recovery_domain,
                recovery_epoch: 1,
                validator_set_id: 1,
                source_height: SOURCE_HEIGHT,
                transition_height: TRANSITION_HEIGHT,
                validator_count: 6,
                verified_signature_count: 5,
                signed_stake: 50,
                total_stake: 60,
            };
            let request = CreateRequest {
                intent_output: root.join("retirement-intent.json"),
                target_release: release_path,
                target_release_sha256: release_sha,
                maintenance_boundary: boundary_path,
                maintenance_boundary_sha256: boundary_sha,
                cutover_policy: policy_path,
                cutover_policy_sha256: policy_sha,
                checkpoint_descriptor: descriptor_path,
                checkpoint_descriptor_sha256: descriptor_sha,
                mode,
                legacy_version: "0.7.11".into(),
                legacy_executable: executable,
                legacy_executable_sha256: executable_sha,
                supervisor_definition: supervisor,
                supervisor_definition_sha256: supervisor_sha,
                data_dir,
                v08_data_dir: root.join("data-v0.8"),
            };
            Self {
                _temporary: temporary,
                root,
                request,
                summary,
                supervisor_argv,
            }
        }

        fn host(&self) -> FakeHost {
            FakeHost::new(self.summary.clone())
        }

        fn stop(&self, intent_sha: &str, mode: RetirementMode) -> Value {
            let identity = match mode {
                RetirementMode::TermOnly(pid) => json!({
                    "boot_id": "00000000-0000-0000-0000-000000000001",
                    "pid": pid,
                    "start_ticks": 999,
                }),
                RetirementMode::PreexistingOffline => Value::Null,
            };
            json!({
                "schema": if matches!(mode, RetirementMode::TermOnly(_)) {
                    STOP_EVIDENCE_SCHEMA
                } else {
                    PREEXISTING_EVIDENCE_SCHEMA
                },
                "intent_sha256": intent_sha,
                "process_identity": identity,
                "supervisor": {
                    "mechanism": if matches!(mode, RetirementMode::TermOnly(_)) {
                        "direct-term-only"
                    } else {
                        "preexisting-offline-verified-supervisor"
                    },
                    "signals_sent": if matches!(mode, RetirementMode::TermOnly(_)) {
                        json!(["SIGTERM"])
                    } else {
                        json!([])
                    },
                    "send_sigkill_configured": false,
                    "sigkill_sent": false,
                    "escalation_used": false,
                    "exit_status_observed": true,
                },
                "observation_started_at": "2026-01-01T00:02:00Z",
                "offline_observed_at": "2026-01-01T00:03:00Z",
                "legacy_exit_clean_claimed": false,
            })
        }
    }

    #[test]
    fn preexisting_create_finalize_are_create_only_and_idempotent() {
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let host = fixture.host();
        let (intent, intent_sha) = create_intent_with_host(&fixture.request, &host).unwrap();
        assert_eq!(
            validate_intent(&intent).unwrap().mode,
            RetirementMode::PreexistingOffline
        );
        assert_eq!(
            std::fs::metadata(&fixture.request.intent_output)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let summary =
            create_intent_summary(&intent, &intent_sha, &fixture.request.intent_output).unwrap();
        assert!(summary["process_identity"].is_null());
        assert!(summary["legacy_pid"].is_null());
        assert!(summary["legacy_boot_id"].is_null());
        assert!(summary["legacy_start_ticks"].is_null());

        *host.now.borrow_mut() = "2026-02-02T00:00:00Z".into();
        let (resumed, resumed_sha) = create_intent_with_host(&fixture.request, &host).unwrap();
        assert_eq!(resumed, intent);
        assert_eq!(resumed_sha, intent_sha);
        let mut changed = fixture.request.clone();
        changed.legacy_version = "0.7.12".into();
        assert!(create_intent_with_host(&changed, &host).is_err());

        let stop_path = fixture.root.join("stop.json");
        let stop = fixture.stop(&intent_sha, RetirementMode::PreexistingOffline);
        let stop_sha = write_json(&stop_path, &stop);
        let receipt_path = fixture.root.join("receipt.json");
        let (receipt, receipt_sha) = finalize_with_host(
            &fixture.request.intent_output,
            &intent_sha,
            &stop_path,
            &stop_sha,
            &receipt_path,
            5,
            3,
            &host,
        )
        .unwrap();
        validate_receipt(&receipt).unwrap();
        assert_eq!(
            receipt["local_legacy_replay"]["classification"],
            "preserved_noncanonical_forensic_not_migrated"
        );
        assert_eq!(receipt["retirement_result"]["sigkill_sent"], false);
        assert_eq!(
            std::fs::metadata(&receipt_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::create_dir(&fixture.request.v08_data_dir).unwrap();
        set_mode(&fixture.request.v08_data_dir, 0o700);
        let (same_receipt, same_sha) = finalize_with_host(
            &fixture.request.intent_output,
            &intent_sha,
            &stop_path,
            &stop_sha,
            &receipt_path,
            5,
            3,
            &host,
        )
        .unwrap();
        assert_eq!(same_receipt, receipt);
        assert_eq!(same_sha, receipt_sha);
    }

    #[test]
    fn term_intent_can_resume_offline_without_losing_bound_identity() {
        let pid = 4242;
        let fixture = Fixture::new(RetirementMode::TermOnly(pid));
        let host = fixture.host();
        let executable = stable_hash_file(
            &fixture.request.legacy_executable,
            "test executable",
            MAX_EXECUTABLE_BYTES,
        )
        .unwrap();
        host.processes.borrow_mut().insert(
            pid,
            ProcessObservation {
                pid,
                boot_id: "00000000-0000-0000-0000-000000000001".into(),
                start_ticks: 999,
                uid: unsafe { libc::geteuid() },
                gid: unsafe { libc::getegid() },
                executable,
                argv: fixture.supervisor_argv.clone(),
                cwd: Some(fixture.root.to_string_lossy().into_owned()),
                listeners: vec![],
            },
        );
        let (intent, digest) = create_intent_with_host(&fixture.request, &host).unwrap();
        let summary =
            create_intent_summary(&intent, &digest, &fixture.request.intent_output).unwrap();
        assert_eq!(summary["legacy_pid"], pid);
        assert_eq!(summary["legacy_start_ticks"], 999);
        assert_eq!(summary["process_identity"]["pid"], pid);

        let mut offline_request = fixture.request.clone();
        offline_request.mode = RetirementMode::PreexistingOffline;
        assert!(create_intent_with_host(&offline_request, &host).is_err());
        host.processes.borrow_mut().clear();
        let (resumed, resumed_digest) = create_intent_with_host(&offline_request, &host).unwrap();
        assert_eq!(resumed_digest, digest);
        assert_eq!(resumed["old_process"]["pid"], pid);
        assert_eq!(resumed["old_process"]["start_ticks"], 999);
        let stop_path = fixture.root.join("term-stop.json");
        let stop = fixture.stop(&digest, RetirementMode::TermOnly(pid));
        let stop_sha = write_json(&stop_path, &stop);
        let (receipt, _) = finalize_with_host(
            &fixture.request.intent_output,
            &digest,
            &stop_path,
            &stop_sha,
            &fixture.root.join("term-receipt.json"),
            5,
            3,
            &host,
        )
        .unwrap();
        assert_eq!(receipt["old_process"]["pid"], pid);
        assert_eq!(receipt["old_process"]["signals_sent"], json!(["SIGTERM"]));
    }

    #[test]
    fn sigkill_or_clean_exit_evidence_is_rejected() {
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let host = fixture.host();
        let (intent, digest) = create_intent_with_host(&fixture.request, &host).unwrap();
        for field in ["send_sigkill_configured", "sigkill_sent", "escalation_used"] {
            let mut stop = fixture.stop(&digest, RetirementMode::PreexistingOffline);
            stop["supervisor"][field] = Value::Bool(true);
            assert!(validate_stop_evidence(&stop, &intent, &digest).is_err());
        }
        let mut stop = fixture.stop(&digest, RetirementMode::PreexistingOffline);
        stop["legacy_exit_clean_claimed"] = Value::Bool(true);
        assert!(validate_stop_evidence(&stop, &intent, &digest).is_err());
    }

    #[test]
    fn descriptor_path_swap_during_crypto_verification_is_rejected() {
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let host = fixture.host();
        host.descriptor_swap.borrow_mut().replace((
            fixture.request.checkpoint_descriptor.clone(),
            canonical_bytes(&json!({"tampered": true})).unwrap(),
        ));
        let error = create_intent_with_host(&fixture.request, &host)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("SHA-256 differs") || error.contains("changed while"),
            "unexpected error: {error}"
        );
        assert!(!fixture.request.intent_output.exists());
    }

    #[test]
    fn replacement_semantics_and_inspection_denial_fail_closed() {
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let host = fixture.host();
        let unrelated = stable_hash_file(
            &fixture.request.supervisor_definition,
            "test unrelated executable",
            MAX_EXECUTABLE_BYTES,
        )
        .unwrap();
        host.processes.borrow_mut().insert(
            5000,
            ProcessObservation {
                pid: 5000,
                boot_id: "00000000-0000-0000-0000-000000000002".into(),
                start_ticks: 2,
                uid: unsafe { libc::geteuid() },
                gid: unsafe { libc::getegid() },
                executable: unrelated,
                argv: fixture.supervisor_argv.clone(),
                cwd: None,
                listeners: vec![],
            },
        );
        assert!(create_intent_with_host(&fixture.request, &host).is_err());
        host.processes.borrow_mut().clear();
        host.observe_errors.borrow_mut().insert(6000);
        assert!(create_intent_with_host(&fixture.request, &host).is_err());
    }

    #[test]
    fn wal_prefix_allows_append_but_rejects_prefix_mutation_and_hardlink() {
        let temporary = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temporary.path()).unwrap();
        set_mode(&root, 0o700);
        let wal = root.join("state.wal");
        write_bytes(&wal, b"prefix", 0o600);
        let prefix = wal_prefix_record(&wal).unwrap();
        {
            use std::io::Write as _;
            let mut file = OpenOptions::new().append(true).open(&wal).unwrap();
            file.write_all(b"-append").unwrap();
        }
        verify_wal_prefix(&wal, &prefix).unwrap();
        std::fs::write(&wal, b"PREFIX-append").unwrap();
        assert!(verify_wal_prefix(&wal, &prefix).is_err());
        let second = root.join("hardlink.wal");
        std::fs::hard_link(&wal, &second).unwrap();
        assert!(
            wal_prefix_record(&wal)
                .unwrap_err()
                .to_string()
                .contains("hard link")
        );
    }

    #[test]
    fn tree_and_output_reject_links_aliases_and_existing_v08() {
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let linked = fixture.request.data_dir.join("linked");
        std::os::unix::fs::symlink(fixture.root.join("legacy.service"), &linked).unwrap();
        assert!(tree_snapshot(&fixture.request.data_dir).is_err());
        std::fs::remove_file(&linked).unwrap();
        let hard = fixture.request.data_dir.join("hard.db");
        std::fs::hard_link(fixture.request.data_dir.join("observer.db"), &hard).unwrap();
        assert!(tree_snapshot(&fixture.request.data_dir).is_err());
        assert!(
            ensure_output_path(
                &fixture.request.data_dir.join("receipt.json"),
                &fixture.request.data_dir,
                "test output",
            )
            .is_err()
        );
        std::fs::create_dir(&fixture.request.v08_data_dir).unwrap();
        set_mode(&fixture.request.v08_data_dir, 0o700);
        assert!(
            ensure_disjoint_absent_v08(&fixture.request.v08_data_dir, &fixture.request.data_dir,)
                .is_err()
        );
    }

    #[test]
    fn cli_enforces_process_mode_and_forensic_flag() {
        let base = [
            "arc-node",
            "legacy-retirement",
            "create-intent",
            "--intent-output",
            "/private/tmp/i",
            "--target-release",
            "/private/tmp/r",
            "--target-release-sha256",
            &"1".repeat(64),
            "--maintenance-boundary",
            "/private/tmp/b",
            "--maintenance-boundary-sha256",
            &"2".repeat(64),
            "--cutover-policy",
            "/private/tmp/p",
            "--cutover-policy-sha256",
            &"3".repeat(64),
            "--checkpoint-descriptor",
            "/private/tmp/d",
            "--checkpoint-descriptor-sha256",
            &"4".repeat(64),
            "--legacy-version",
            "0.7.11",
            "--legacy-executable",
            "/private/tmp/e",
            "--legacy-executable-sha256",
            &"5".repeat(64),
            "--supervisor-definition",
            "/private/tmp/s",
            "--supervisor-definition-sha256",
            &"6".repeat(64),
            "--data-dir",
            "/private/tmp/old",
            "--v08-data-dir",
            "/private/tmp/new",
        ];
        assert!(crate::Cli::try_parse_from(base).is_err());
        let mut no_forensic = base.to_vec();
        no_forensic.push("--already-offline");
        assert!(crate::Cli::try_parse_from(no_forensic).is_err());
        let mut valid = base.to_vec();
        valid.extend(["--already-offline", "--forensic-only"]);
        assert!(crate::Cli::try_parse_from(valid).is_ok());
        let mut both = base.to_vec();
        both.extend(["--already-offline", "--legacy-pid", "12", "--forensic-only"]);
        assert!(crate::Cli::try_parse_from(both).is_err());
    }

    #[test]
    fn offline_bounds_and_listener_occupancy_are_enforced() {
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let host = fixture.host();
        let executable = stable_hash_file(
            &fixture.request.legacy_executable,
            "test executable",
            MAX_EXECUTABLE_BYTES,
        )
        .unwrap();
        let process = json!({
            "pid": null,
            "boot_id": null,
            "start_ticks": null,
            "executable": executable.value(),
            "listeners": [],
        });
        assert!(
            prove_stably_offline(
                &host,
                process.as_object().unwrap(),
                std::fs::metadata(&fixture.request.data_dir).unwrap().uid(),
                fixture.request.data_dir.to_str().unwrap(),
                4,
                3,
            )
            .is_err()
        );
        host.listeners.borrow_mut().push(ListenerEndpoint {
            family: "tcp4".into(),
            address_hex: "00000000".into(),
            port: 9090,
            inode: 1,
        });
        assert!(
            prove_stably_offline(
                &host,
                process.as_object().unwrap(),
                std::fs::metadata(&fixture.request.data_dir).unwrap().uid(),
                fixture.request.data_dir.to_str().unwrap(),
                5,
                3,
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_precreated_receipt_and_protocol_tamper_are_rejected() {
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let host = fixture.host();
        let (intent, intent_sha) = create_intent_with_host(&fixture.request, &host).unwrap();
        let stop_path = fixture.root.join("stop-tamper.json");
        let stop = fixture.stop(&intent_sha, RetirementMode::PreexistingOffline);
        let stop_sha = write_json(&stop_path, &stop);
        let receipt_path = fixture.root.join("receipt-tamper.json");
        let (mut receipt, _) = finalize_with_host(
            &fixture.request.intent_output,
            &intent_sha,
            &stop_path,
            &stop_sha,
            &receipt_path,
            5,
            3,
            &host,
        )
        .unwrap();
        receipt["offline_stability"] = Value::Null;
        write_json(&receipt_path, &receipt);
        assert!(
            finalize_with_host(
                &fixture.request.intent_output,
                &intent_sha,
                &stop_path,
                &stop_sha,
                &receipt_path,
                5,
                3,
                &host,
            )
            .is_err()
        );

        let mut tampered_intent = intent;
        tampered_intent["protocol_id"] = Value::String(hash_number(99));
        write_json(&fixture.request.intent_output, &tampered_intent);
        assert!(create_intent_with_host(&fixture.request, &host).is_err());
    }

    #[test]
    fn finalize_rejects_precutoff_evidence_and_existing_v08() {
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let host = fixture.host();
        let (_intent, intent_sha) = create_intent_with_host(&fixture.request, &host).unwrap();
        let stop_path = fixture.root.join("stop-precutoff.json");
        let mut stop = fixture.stop(&intent_sha, RetirementMode::PreexistingOffline);
        stop["observation_started_at"] = Value::String("2025-12-31T23:58:00Z".into());
        stop["offline_observed_at"] = Value::String("2025-12-31T23:59:00Z".into());
        let stop_sha = write_json(&stop_path, &stop);
        assert!(
            finalize_with_host(
                &fixture.request.intent_output,
                &intent_sha,
                &stop_path,
                &stop_sha,
                &fixture.root.join("receipt-precutoff.json"),
                5,
                3,
                &host,
            )
            .unwrap_err()
            .to_string()
            .contains("predates")
        );

        let valid_stop = fixture.stop(&intent_sha, RetirementMode::PreexistingOffline);
        let valid_stop_sha = write_json(&stop_path, &valid_stop);
        std::fs::create_dir(&fixture.request.v08_data_dir).unwrap();
        set_mode(&fixture.request.v08_data_dir, 0o700);
        assert!(
            finalize_with_host(
                &fixture.request.intent_output,
                &intent_sha,
                &stop_path,
                &valid_stop_sha,
                &fixture.root.join("receipt-existing-v08.json"),
                5,
                3,
                &host,
            )
            .is_err()
        );
    }

    #[test]
    fn finalize_cannot_cross_a_live_v08_namespace_owner() {
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let host = fixture.host();
        let (_intent, intent_sha) = create_intent_with_host(&fixture.request, &host).unwrap();
        let stop_path = fixture.root.join("stop-lock.json");
        let stop = fixture.stop(&intent_sha, RetirementMode::PreexistingOffline);
        let stop_sha = write_json(&stop_path, &stop);
        let _owner = arc_crypto::secret_file::try_acquire_private_directory_namespace_lock(
            &fixture.request.v08_data_dir,
        )
        .unwrap();
        assert!(
            finalize_with_host(
                &fixture.request.intent_output,
                &intent_sha,
                &stop_path,
                &stop_sha,
                &fixture.root.join("receipt-locked.json"),
                5,
                3,
                &host,
            )
            .is_err()
        );
    }

    #[test]
    fn noncanonical_paths_and_unsafe_stability_values_are_rejected() {
        assert!(require_absolute_normal(Path::new("/tmp/./arc"), "test path").is_err());
        assert!(require_absolute_normal(Path::new("/tmp//arc"), "test path").is_err());
        assert!(require_absolute_normal(Path::new("/tmp/arc/"), "test path").is_err());
        let fixture = Fixture::new(RetirementMode::PreexistingOffline);
        let host = fixture.host();
        let executable = stable_hash_file(
            &fixture.request.legacy_executable,
            "test executable",
            MAX_EXECUTABLE_BYTES,
        )
        .unwrap();
        let process = json!({
            "pid": null,
            "boot_id": null,
            "start_ticks": null,
            "executable": executable.value(),
            "listeners": [],
        });
        assert!(
            prove_stably_offline(
                &host,
                process.as_object().unwrap(),
                std::fs::metadata(&fixture.request.data_dir).unwrap().uid(),
                fixture.request.data_dir.to_str().unwrap(),
                301,
                3,
            )
            .is_err()
        );
        assert!(
            prove_stably_offline(
                &host,
                process.as_object().unwrap(),
                std::fs::metadata(&fixture.request.data_dir).unwrap().uid(),
                fixture.request.data_dir.to_str().unwrap(),
                5,
                21,
            )
            .is_err()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn system_scan_detects_a_renamed_byte_identical_executable_by_sha() {
        let temporary = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temporary.path()).unwrap();
        let selected = root.join("selected-legacy-node");
        std::fs::copy("/bin/sleep", &selected).unwrap();
        set_mode(&selected, 0o500);
        let expected =
            stable_hash_file(&selected, "selected test executable", MAX_EXECUTABLE_BYTES).unwrap();
        // The live path is /bin/sleep while the bound path is a distinct copy:
        // path-prefiltering cannot find it, but exact bytes/SHA must.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let cache = ProcessExecutableHashCache::default();
        let started = std::time::Instant::now();
        #[cfg(target_os = "linux")]
        let scanned = linux_matching_processes_for_pids(
            std::iter::once(child.id()),
            &cache,
            unsafe { libc::geteuid() },
            "/private/tmp/arc-retirement-nonsemantic-data",
            expected.path.as_str(),
            expected.size,
            &expected.sha256,
        );
        #[cfg(target_os = "macos")]
        let scanned = system_matching_processes(
            &cache,
            unsafe { libc::geteuid() },
            "/private/tmp/arc-retirement-nonsemantic-data",
            expected.path.as_str(),
            expected.size,
            &expected.sha256,
        );
        let _ = child.kill();
        let _ = child.wait();
        let matches = scanned.unwrap();
        assert!(matches.iter().any(|process| process.pid == child.id()));
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(cache.hash_operations() <= MAX_PROCESS_EXECUTABLE_HASHES);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_libproc_surfaces_compile_and_observe_self() {
        assert_eq!(std::mem::size_of::<MacProcBsdInfo>(), 136);
        let current = std::process::id();
        assert!(system_all_process_ids().unwrap().contains(&current));
        let observed = system_observe_process(current).unwrap().unwrap();
        assert_eq!(observed.pid, current);
        assert!(observed.start_ticks > 0);
        assert!(!observed.argv.is_empty());
        let executable_hashes = ProcessExecutableHashCache::default();
        let started = std::time::Instant::now();
        assert!(
            system_matching_processes(
                &executable_hashes,
                unsafe { libc::geteuid() },
                "/private/tmp/arc-retirement-nonexistent-data",
                &observed.executable.path,
                observed.executable.size,
                &hash_number(254),
            )
            .unwrap()
            .is_empty()
        );
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(
            executable_hashes.hash_operations() <= MAX_PROCESS_EXECUTABLE_HASHES,
            "system scan exceeded its hard hash-count bound"
        );
    }
}
