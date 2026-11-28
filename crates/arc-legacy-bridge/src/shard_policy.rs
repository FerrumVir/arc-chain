use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

#[cfg(unix)]
use std::process::{Child, Stdio};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Instant;

use anyhow::Result;
use serde::Deserialize;

use crate::argv::LegacyKind;
use crate::consent::{self, Compute, WorkerModePreference};
use crate::exit;
use crate::layout::Layout;
use crate::logging::Log;
use crate::pins::{Pins, SHARD_REQUIRED_ADMITTED_LAYERS_FLOOR, ShardWorkerPin};
#[cfg(unix)]
use crate::release;

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const WORKER_MIN_NOMINAL_RAM_GB: u64 = 16;
pub const MARKETED_RAM_SIZES_GB: &[u64] = &[
    2, 3, 4, 6, 8, 12, 16, 18, 20, 24, 32, 36, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024,
    1536, 2048,
];
pub const MAX_RESERVED_RAM_PERCENT: u64 = 15;
pub const SHARD_RESERVE_RAM_BYTES: u64 = 4 * GIB;
pub const SHARD_MIN_AVAILABLE_BYTES: u64 = 6 * GIB;
pub const HEADER_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
pub const HEADER_CHECK_STDOUT_CAP_BYTES: usize = 256 * 1024;
pub const HEADER_CHECK_STDERR_CAP_BYTES: usize = 16 * 1024;
const CGROUP_FILE_MAX_BYTES: u64 = 4096;
const CGROUP_V1_UNBOUNDED_THRESHOLD_BYTES: u64 = 1u64 << 60;
const HEADER_ADMISSION_REPORT_MODE: &str = "header-admission-only";

#[cfg(target_os = "macos")]
const SYSTEM_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "macos")]
const SYSTEM_PROBE_OUTPUT_CAP_BYTES: usize = 64 * 1024;

const PREFLIGHT_WORKER_NAME: &str = "arc-legacy-bridge-preflight";
const ROLE_PREFIX_OFF: &str = "off: ";
const ROLE_PREFIX_LEGACY_WHOLE: &str = "on, with the verified model ";
const ROLE_PREFIX_FULL_WORKER: &str = "on (full integer worker";
const ROLE_PREFIX_PARTIAL_SHARD: &str = "on (partial shard, ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredComputeRole {
    Observer,
    FullIntegerWorker,
    CommunityShardWorker,
    Unknown,
}

pub fn classify_stored_compute(description: &str) -> StoredComputeRole {
    if description.starts_with(ROLE_PREFIX_PARTIAL_SHARD) {
        StoredComputeRole::CommunityShardWorker
    } else if description.starts_with(ROLE_PREFIX_FULL_WORKER)
        || description.starts_with(ROLE_PREFIX_LEGACY_WHOLE)
    {
        StoredComputeRole::FullIntegerWorker
    } else if description.starts_with(ROLE_PREFIX_OFF) {
        StoredComputeRole::Observer
    } else {
        StoredComputeRole::Unknown
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostMemory {
    pub visible_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerLaunchRole {
    Observer(String),
    FullIntegerWorker {
        model: PathBuf,
        fallback_reason: Option<String>,
    },
    CommunityShardWorker {
        model: PathBuf,
        pin: Box<ShardWorkerPin>,
        max_working_set_bytes: u64,
        admitted_max_resident_layers: usize,
    },
}

impl WorkerLaunchRole {
    pub fn from_compute(compute: &Compute) -> Self {
        match compute {
            Compute::Off(reason) => Self::Observer(reason.clone()),
            Compute::On(model) => Self::FullIntegerWorker {
                model: model.clone(),
                fallback_reason: None,
            },
        }
    }

    pub fn is_community_shard(&self) -> bool {
        matches!(self, Self::CommunityShardWorker { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Observer(reason) => format!("{ROLE_PREFIX_OFF}{reason}"),
            Self::FullIntegerWorker {
                model,
                fallback_reason: None,
            } => format!(
                "{ROLE_PREFIX_FULL_WORKER}), with the verified model {}",
                model.display()
            ),
            Self::FullIntegerWorker {
                model,
                fallback_reason: Some(reason),
            } => format!(
                "{ROLE_PREFIX_FULL_WORKER}; partial-shard preflight fell back: {reason}), with the verified model {}",
                model.display()
            ),
            Self::CommunityShardWorker {
                model,
                max_working_set_bytes,
                admitted_max_resident_layers,
                ..
            } => format!(
                "{ROLE_PREFIX_PARTIAL_SHARD}{admitted_max_resident_layers} resident layers, {max_working_set_bytes} byte working-set budget; rewards disabled), with the verified model {}",
                model.display()
            ),
        }
    }
}

#[derive(Debug)]
pub struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum BoundedCommandError {
    UnsupportedPlatform,
    Spawn(io::Error),
    Wait(io::Error),
    Timeout(Duration),
    OutputLimitExceeded {
        stream: &'static str,
        limit: usize,
    },
    ReadError {
        stream: &'static str,
        error: io::Error,
    },
}

impl fmt::Display for BoundedCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(
                f,
                "bounded process-group command execution is supported only on Unix"
            ),
            Self::Spawn(error) => write!(f, "could not spawn child process: {error}"),
            Self::Wait(error) => write!(f, "could not wait for child process: {error}"),
            Self::Timeout(timeout) => write!(f, "timed out after {} ms", timeout.as_millis()),
            Self::OutputLimitExceeded { stream, limit } => {
                write!(f, "{stream} exceeded the {limit}-byte capture limit")
            }
            Self::ReadError { stream, error } => {
                write!(f, "could not read child {stream}: {error}")
            }
        }
    }
}

impl std::error::Error for BoundedCommandError {}

pub fn nominal_ram_gb(visible_bytes: u64) -> u64 {
    for &size_gb in MARKETED_RAM_SIZES_GB {
        let size_bytes = size_gb.saturating_mul(GIB);
        if visible_bytes > size_bytes {
            continue;
        }
        let tolerance = ((size_bytes / 100) * MAX_RESERVED_RAM_PERCENT).max(GIB);
        if size_bytes - visible_bytes <= tolerance {
            return size_gb;
        }
        break;
    }
    visible_bytes / GIB
}

pub fn qualify_host_memory(memory: HostMemory) -> std::result::Result<u64, String> {
    let nominal_gb = nominal_ram_gb(memory.visible_bytes);
    if nominal_gb < WORKER_MIN_NOMINAL_RAM_GB {
        return Err(format!(
            "nominal RAM {nominal_gb} GB is below the {WORKER_MIN_NOMINAL_RAM_GB} GB shard worker requirement (visible={} bytes)",
            memory.visible_bytes
        ));
    }
    if memory.visible_bytes <= SHARD_RESERVE_RAM_BYTES {
        return Err(format!(
            "visible RAM {} bytes leaves no working-set budget above the 4 GiB OS reserve",
            memory.visible_bytes
        ));
    }
    if memory.available_bytes < SHARD_MIN_AVAILABLE_BYTES {
        return Err(format!(
            "available RAM {} bytes is below the {SHARD_MIN_AVAILABLE_BYTES}-byte shard worker floor",
            memory.available_bytes
        ));
    }
    let reserve_capped = memory.visible_bytes - SHARD_RESERVE_RAM_BYTES;
    let half_visible = memory.visible_bytes / 2;
    Ok(reserve_capped.min(half_visible))
}

fn parse_canonical_u64(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok()
}

pub fn parse_linux_meminfo(text: &str) -> std::result::Result<HostMemory, String> {
    let mut mem_total: Option<u64> = None;
    let mut mem_available: Option<u64> = None;
    let mut mem_free: Option<u64> = None;
    let mut buffers: Option<u64> = None;
    let mut cached: Option<u64> = None;

    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let target = match key {
            "MemTotal:" => &mut mem_total,
            "MemAvailable:" => &mut mem_available,
            "MemFree:" => &mut mem_free,
            "Buffers:" => &mut buffers,
            "Cached:" => &mut cached,
            _ => continue,
        };
        if target.is_some() {
            return Err(format!("duplicate {key} entry in /proc/meminfo"));
        }
        let Some(raw_value) = parts.next() else {
            return Err(format!("missing value for {key} in /proc/meminfo"));
        };
        let Some(unit) = parts.next() else {
            return Err(format!("missing unit for {key} in /proc/meminfo"));
        };
        if unit != "kB" || parts.next().is_some() {
            return Err(format!("unexpected format for {key} in /proc/meminfo"));
        }
        let kib = parse_canonical_u64(raw_value)
            .ok_or_else(|| format!("invalid integer for {key} in /proc/meminfo"))?;
        *target = Some(kib);
    }

    let total_kib = mem_total
        .filter(|value| *value > 0)
        .ok_or_else(|| "missing or zero MemTotal in /proc/meminfo".to_string())?;
    let available_kib = if let Some(available) = mem_available {
        available
    } else if let Some(free) = mem_free {
        free.checked_add(buffers.unwrap_or(0))
            .and_then(|sum| sum.checked_add(cached.unwrap_or(0)))
            .ok_or_else(|| "available memory overflow in /proc/meminfo".to_string())?
    } else {
        return Err("missing MemAvailable and MemFree in /proc/meminfo".to_string());
    };

    let visible_bytes = total_kib
        .checked_mul(1024)
        .ok_or_else(|| "MemTotal overflow in /proc/meminfo".to_string())?;
    let available_bytes = available_kib
        .checked_mul(1024)
        .ok_or_else(|| "available memory overflow in /proc/meminfo".to_string())?;
    Ok(HostMemory {
        visible_bytes,
        available_bytes,
    })
}

pub fn parse_macos_vm_stat(text: &str) -> std::result::Result<u64, String> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return Err("vm_stat produced empty output".to_string());
    };
    let page_size = parse_macos_page_size(header)?;
    let mut free_pages: Option<u64> = None;
    let mut inactive_pages: Option<u64> = None;
    let mut speculative_pages: Option<u64> = None;
    for line in lines {
        let (label, target, rest) = if let Some(rest) = line.strip_prefix("Pages free:") {
            ("Pages free:", &mut free_pages, rest)
        } else if let Some(rest) = line.strip_prefix("Pages inactive:") {
            ("Pages inactive:", &mut inactive_pages, rest)
        } else if let Some(rest) = line.strip_prefix("Pages speculative:") {
            ("Pages speculative:", &mut speculative_pages, rest)
        } else {
            continue;
        };
        if target.is_some() {
            return Err(format!("duplicate {label} entry in vm_stat output"));
        }
        let trimmed = rest.trim();
        let digits = trimmed.strip_suffix('.').unwrap_or(trimmed);
        let count = parse_canonical_u64(digits)
            .ok_or_else(|| format!("malformed vm_stat count in line {line:?}"))?;
        *target = Some(count);
    }
    if free_pages.is_none() && inactive_pages.is_none() && speculative_pages.is_none() {
        return Err(
            "vm_stat output contained no free, inactive, or speculative page counts".to_string(),
        );
    }
    let pages = free_pages
        .unwrap_or(0)
        .checked_add(inactive_pages.unwrap_or(0))
        .and_then(|sum| sum.checked_add(speculative_pages.unwrap_or(0)))
        .ok_or_else(|| "vm_stat page count overflow".to_string())?;
    pages
        .checked_mul(page_size)
        .ok_or_else(|| "vm_stat available memory overflow".to_string())
}

fn parse_macos_page_size(header: &str) -> std::result::Result<u64, String> {
    let trimmed = header.trim();
    let Some(rest) = trimmed.strip_prefix("Mach Virtual Memory Statistics:") else {
        return Err(format!("unexpected vm_stat header {header:?}"));
    };
    let Some((_, after_prefix)) = rest.split_once("(page size of ") else {
        return Err(format!("missing vm_stat page size in header {header:?}"));
    };
    let Some((digits, tail)) = after_prefix.split_once(" bytes)") else {
        return Err(format!("malformed vm_stat page size in header {header:?}"));
    };
    if !tail.trim().is_empty() {
        return Err(format!(
            "unexpected trailing text in vm_stat header {header:?}"
        ));
    }
    parse_canonical_u64(digits)
        .filter(|size| *size > 0 && size.is_power_of_two())
        .ok_or_else(|| format!("invalid vm_stat page size in {header:?}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeclaredMemoryCgroup {
    None,
    V1(String),
    V2(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CgroupMountKind {
    V1Memory,
    V2Unified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CgroupMount {
    kind: CgroupMountKind,
    root: String,
    mount_point: String,
}

fn validate_absolute_cgroup_path(path: &str, label: &str) -> std::result::Result<(), String> {
    let has_forbidden_byte = path
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b'\\');
    if !path.starts_with('/') || has_forbidden_byte {
        return Err(format!("{label} contains an invalid path {path:?}"));
    }
    if path == "/" {
        return Ok(());
    }
    for component in path[1..].split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(format!(
                "{label} contains an invalid or escaping path component in {path:?}"
            ));
        }
    }
    Ok(())
}

fn unescape_mountinfo_field(raw: &str, label: &str) -> std::result::Result<String, String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            if index + 3 >= bytes.len() {
                return Err(format!("{label} has a truncated escape sequence {raw:?}"));
            }
            let mut value = 0u16;
            for &digit in &bytes[index + 1..index + 4] {
                if !(b'0'..=b'7').contains(&digit) {
                    return Err(format!("{label} has an invalid octal escape in {raw:?}"));
                }
                value = value * 8 + u16::from(digit - b'0');
            }
            let byte = u8::try_from(value)
                .map_err(|_| format!("{label} has an out-of-range octal escape in {raw:?}"))?;
            if byte.is_ascii_control() {
                return Err(format!("{label} decodes to a control byte in {raw:?}"));
            }
            decoded.push(byte);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| format!("{label} is not valid UTF-8"))
}

fn parse_proc_self_cgroup(text: &str) -> std::result::Result<DeclaredMemoryCgroup, String> {
    let mut v1_memory: Option<String> = None;
    let mut v2_unified: Option<String> = None;

    for line in text.lines() {
        let Some((hierarchy_raw, rest)) = line.split_once(':') else {
            return Err(format!("malformed line in /proc/self/cgroup: {line:?}"));
        };
        let Some((controllers, cgroup_path)) = rest.split_once(':') else {
            return Err(format!("malformed line in /proc/self/cgroup: {line:?}"));
        };
        let hierarchy_id = parse_canonical_u64(hierarchy_raw)
            .ok_or_else(|| format!("invalid hierarchy id in /proc/self/cgroup: {line:?}"))?;
        validate_absolute_cgroup_path(cgroup_path, "/proc/self/cgroup path")?;
        if hierarchy_id == 0 {
            if !controllers.is_empty() {
                return Err(format!(
                    "malformed unified hierarchy entry in /proc/self/cgroup: {line:?}"
                ));
            }
            if v2_unified.is_some() {
                return Err("duplicate cgroup v2 entry in /proc/self/cgroup".to_string());
            }
            v2_unified = Some(cgroup_path.to_string());
        } else if controllers.split(',').any(|ctrl| ctrl == "memory") {
            if v1_memory.is_some() {
                return Err("duplicate cgroup v1 memory entry in /proc/self/cgroup".to_string());
            }
            v1_memory = Some(cgroup_path.to_string());
        }
    }

    if let Some(path) = v1_memory {
        Ok(DeclaredMemoryCgroup::V1(path))
    } else if let Some(path) = v2_unified {
        Ok(DeclaredMemoryCgroup::V2(path))
    } else {
        Ok(DeclaredMemoryCgroup::None)
    }
}

fn parse_proc_self_mountinfo(text: &str) -> std::result::Result<Vec<CgroupMount>, String> {
    let mut mounts = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            return Err(format!(
                "malformed entry without separator in /proc/self/mountinfo: {line:?}"
            ));
        };
        if separator < 6 || fields.len() < separator + 4 {
            return Err(format!("truncated entry in /proc/self/mountinfo: {line:?}"));
        }
        let fs_type = fields[separator + 1];
        let super_options = fields[separator + 3];
        let kind = if fs_type == "cgroup2" {
            Some(CgroupMountKind::V2Unified)
        } else if fs_type == "cgroup" && super_options.split(',').any(|opt| opt == "memory") {
            Some(CgroupMountKind::V1Memory)
        } else {
            None
        };
        if let Some(kind) = kind {
            let root = unescape_mountinfo_field(fields[3], "/proc/self/mountinfo root")?;
            let mount_point =
                unescape_mountinfo_field(fields[4], "/proc/self/mountinfo mount point")?;
            validate_absolute_cgroup_path(&root, "/proc/self/mountinfo root")?;
            validate_absolute_cgroup_path(&mount_point, "/proc/self/mountinfo mount point")?;
            mounts.push(CgroupMount {
                kind,
                root,
                mount_point,
            });
        }
    }
    Ok(mounts)
}

fn relative_cgroup_path<'a>(mount_root: &str, cgroup_path: &'a str) -> Option<&'a str> {
    if mount_root == "/" {
        Some(cgroup_path.trim_start_matches('/'))
    } else if cgroup_path == mount_root {
        Some("")
    } else {
        cgroup_path
            .strip_prefix(mount_root)
            .and_then(|rest| rest.strip_prefix('/'))
    }
}

fn select_cgroup_mount<'a, 'b>(
    mounts: &'a [CgroupMount],
    wanted: CgroupMountKind,
    cgroup_path: &'b str,
) -> Option<(&'a CgroupMount, &'b str)> {
    let mut best: Option<(&'a CgroupMount, &'b str)> = None;
    for mount in mounts {
        if mount.kind != wanted {
            continue;
        }
        if let Some(relative) = relative_cgroup_path(&mount.root, cgroup_path)
            && best.is_none_or(|(current, _)| mount.root.len() < current.root.len())
        {
            best = Some((mount, relative));
        }
    }
    best
}

fn cgroup_hierarchy_dirs(fs_root: &Path, mount_point: &str, relative: &str) -> Vec<PathBuf> {
    let mut current = fs_root.to_path_buf();
    if mount_point != "/" {
        for component in mount_point[1..].split('/') {
            current = current.join(component);
        }
    }
    let mut dirs = vec![current.clone()];
    if !relative.is_empty() {
        for component in relative.split('/') {
            current = current.join(component);
            dirs.push(current.clone());
        }
    }
    dirs
}

fn ensure_regular_cgroup_dir(dir: &Path) -> std::result::Result<(), String> {
    let metadata = fs::symlink_metadata(dir)
        .map_err(|error| format!("cannot inspect cgroup directory {}: {error}", dir.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(format!(
            "cgroup directory {} must be a real non-symlink directory",
            dir.display()
        ));
    }
    Ok(())
}

fn read_bounded_cgroup_file(path: &Path) -> std::result::Result<Option<String>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot inspect cgroup file {}: {error}",
                path.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!(
            "cgroup file {} must be a regular non-symlink file",
            path.display()
        ));
    }
    let file = fs::File::open(path)
        .map_err(|error| format!("cannot read cgroup file {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(CGROUP_FILE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read cgroup file {}: {error}", path.display()))?;
    if bytes.len() as u64 > CGROUP_FILE_MAX_BYTES {
        return Err(format!(
            "cgroup file {} exceeds the {CGROUP_FILE_MAX_BYTES}-byte limit",
            path.display()
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("cgroup file {} is not valid UTF-8", path.display()))?;
    Ok(Some(text))
}

fn record_finite_cgroup_bound(
    effective_limit: &mut Option<u64>,
    effective_headroom: &mut Option<u64>,
    limit: u64,
    usage: u64,
) {
    *effective_limit = Some(effective_limit.map_or(limit, |current| current.min(limit)));
    let headroom = limit.saturating_sub(usage);
    *effective_headroom =
        Some(effective_headroom.map_or(headroom, |current| current.min(headroom)));
}

pub fn apply_linux_cgroup_memory_limits(
    host: HostMemory,
    proc_cgroup_text: &str,
    proc_mountinfo_text: &str,
    fs_root: &Path,
) -> std::result::Result<HostMemory, String> {
    let declared = parse_proc_self_cgroup(proc_cgroup_text)?;
    let mounts = parse_proc_self_mountinfo(proc_mountinfo_text)?;
    let mut effective_limit: Option<u64> = None;
    let mut effective_headroom: Option<u64> = None;

    match declared {
        DeclaredMemoryCgroup::None => return Ok(host),
        DeclaredMemoryCgroup::V2(cgroup_path) => {
            let found = select_cgroup_mount(&mounts, CgroupMountKind::V2Unified, &cgroup_path);
            let Some((mount, relative)) = found else {
                return Err(format!(
                    "no matching cgroup2 mount in /proc/self/mountinfo covers {cgroup_path:?}"
                ));
            };
            let dirs = cgroup_hierarchy_dirs(fs_root, &mount.mount_point, relative);
            for (index, dir) in dirs.iter().enumerate() {
                ensure_regular_cgroup_dir(dir)?;
                let is_system_root = index == 0 && mount.root == "/";
                let max_path = dir.join("memory.max");
                match read_bounded_cgroup_file(&max_path)? {
                    Some(raw_max) => {
                        let trimmed = raw_max.trim();
                        if trimmed == "max" {
                            continue;
                        }
                        let Some(limit) = parse_canonical_u64(trimmed) else {
                            return Err(format!(
                                "malformed cgroup v2 limit in {}",
                                max_path.display()
                            ));
                        };
                        let current_path = dir.join("memory.current");
                        let Some(raw_current) = read_bounded_cgroup_file(&current_path)? else {
                            return Err(format!(
                                "missing cgroup v2 usage file {}",
                                current_path.display()
                            ));
                        };
                        let Some(usage) = parse_canonical_u64(raw_current.trim()) else {
                            return Err(format!(
                                "malformed cgroup v2 usage in {}",
                                current_path.display()
                            ));
                        };
                        record_finite_cgroup_bound(
                            &mut effective_limit,
                            &mut effective_headroom,
                            limit,
                            usage,
                        );
                    }
                    None if is_system_root => {}
                    None => {
                        let controllers_path = dir.join("cgroup.controllers");
                        let controllers = read_bounded_cgroup_file(&controllers_path)?;
                        let Some(raw_controllers) = controllers else {
                            return Err(format!(
                                "missing cgroup v2 control files in {}",
                                dir.display()
                            ));
                        };
                        if raw_controllers.split_whitespace().any(|c| c == "memory") {
                            return Err(format!(
                                "cgroup v2 directory {} declares memory controller without memory.max",
                                dir.display()
                            ));
                        }
                    }
                }
            }
        }
        DeclaredMemoryCgroup::V1(cgroup_path) => {
            let found = select_cgroup_mount(&mounts, CgroupMountKind::V1Memory, &cgroup_path);
            let Some((mount, relative)) = found else {
                return Err(format!(
                    "no matching cgroup v1 memory mount in /proc/self/mountinfo covers {cgroup_path:?}"
                ));
            };
            let dirs = cgroup_hierarchy_dirs(fs_root, &mount.mount_point, relative);
            for dir in &dirs {
                ensure_regular_cgroup_dir(dir)?;
                let limit_path = dir.join("memory.limit_in_bytes");
                let usage_path = dir.join("memory.usage_in_bytes");
                let Some(raw_limit) = read_bounded_cgroup_file(&limit_path)? else {
                    return Err(format!(
                        "missing cgroup v1 limit file {}",
                        limit_path.display()
                    ));
                };
                let Some(limit) = parse_canonical_u64(raw_limit.trim()) else {
                    return Err(format!(
                        "malformed cgroup v1 limit in {}",
                        limit_path.display()
                    ));
                };
                let Some(raw_usage) = read_bounded_cgroup_file(&usage_path)? else {
                    return Err(format!(
                        "missing cgroup v1 usage file {}",
                        usage_path.display()
                    ));
                };
                let Some(usage) = parse_canonical_u64(raw_usage.trim()) else {
                    return Err(format!(
                        "malformed cgroup v1 usage in {}",
                        usage_path.display()
                    ));
                };
                if limit < CGROUP_V1_UNBOUNDED_THRESHOLD_BYTES {
                    record_finite_cgroup_bound(
                        &mut effective_limit,
                        &mut effective_headroom,
                        limit,
                        usage,
                    );
                }
            }
        }
    }

    let visible_bytes = match effective_limit {
        Some(limit) => host.visible_bytes.min(limit),
        None => host.visible_bytes,
    };
    let available_bytes = match effective_headroom {
        Some(headroom) => host.available_bytes.min(headroom).min(visible_bytes),
        None => host.available_bytes.min(visible_bytes),
    };
    Ok(HostMemory {
        visible_bytes,
        available_bytes,
    })
}

#[cfg(target_os = "linux")]
pub fn measure_host_memory() -> std::result::Result<HostMemory, String> {
    let meminfo_text = fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("cannot read /proc/meminfo: {error}"))?;
    let host = parse_linux_meminfo(&meminfo_text)?;
    let cgroup_text = fs::read_to_string("/proc/self/cgroup")
        .map_err(|error| format!("cannot read /proc/self/cgroup: {error}"))?;
    let mountinfo_text = fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("cannot read /proc/self/mountinfo: {error}"))?;
    apply_linux_cgroup_memory_limits(host, &cgroup_text, &mountinfo_text, Path::new("/"))
}

#[cfg(target_os = "macos")]
pub fn measure_host_memory() -> std::result::Result<HostMemory, String> {
    let memsize_output = run_bounded_command(
        Path::new("/usr/sbin/sysctl"),
        &[OsString::from("-n"), OsString::from("hw.memsize")],
        SYSTEM_PROBE_TIMEOUT,
        SYSTEM_PROBE_OUTPUT_CAP_BYTES,
        HEADER_CHECK_STDERR_CAP_BYTES,
    )
    .map_err(|error| format!("sysctl hw.memsize failed: {error}"))?;
    if !memsize_output.status.success() {
        return Err(format!(
            "sysctl hw.memsize exited with {}",
            memsize_output.status
        ));
    }
    let memsize_text = std::str::from_utf8(&memsize_output.stdout)
        .map_err(|_| "sysctl hw.memsize output was not UTF-8".to_string())?;
    let visible_bytes = parse_canonical_u64(memsize_text.trim())
        .filter(|value| *value > 0)
        .ok_or_else(|| "sysctl hw.memsize returned an invalid byte count".to_string())?;

    let vm_output = run_bounded_command(
        Path::new("/usr/bin/vm_stat"),
        &[],
        SYSTEM_PROBE_TIMEOUT,
        SYSTEM_PROBE_OUTPUT_CAP_BYTES,
        HEADER_CHECK_STDERR_CAP_BYTES,
    )
    .map_err(|error| format!("vm_stat failed: {error}"))?;
    if !vm_output.status.success() {
        return Err(format!("vm_stat exited with {}", vm_output.status));
    }
    let vm_text = std::str::from_utf8(&vm_output.stdout)
        .map_err(|_| "vm_stat output was not UTF-8".to_string())?;
    let available_bytes = parse_macos_vm_stat(vm_text)?;
    Ok(HostMemory {
        visible_bytes,
        available_bytes,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn measure_host_memory() -> std::result::Result<HostMemory, String> {
    Err("headless shard worker is not supported on this platform".to_string())
}

#[cfg(unix)]
struct StreamStatus {
    done: AtomicBool,
    overflow: AtomicBool,
    read_error: AtomicBool,
}

#[cfg(unix)]
impl StreamStatus {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: AtomicBool::new(false),
            overflow: AtomicBool::new(false),
            read_error: AtomicBool::new(false),
        })
    }
}

#[cfg(unix)]
struct StreamDoneGuard(Arc<StreamStatus>);

#[cfg(unix)]
impl Drop for StreamDoneGuard {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.read_error.store(true, Ordering::Release);
        }
        self.0.done.store(true, Ordering::Release);
    }
}

#[cfg(unix)]
enum BoundedWaitOutcome {
    Exited(ExitStatus),
    Timeout,
    Overflow(&'static str, usize),
    ReadError,
    WaitError(io::Error),
}

#[cfg(unix)]
fn read_capped_stream<R: Read>(
    mut reader: R,
    limit: usize,
    status: Arc<StreamStatus>,
) -> io::Result<Vec<u8>> {
    let _done = StreamDoneGuard(Arc::clone(&status));
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(buffer),
            Ok(count) => {
                let remaining = limit.saturating_sub(buffer.len());
                if count > remaining {
                    status.overflow.store(true, Ordering::Release);
                    return Ok(buffer);
                }
                buffer.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                status.read_error.store(true, Ordering::Release);
                return Err(error);
            }
        }
    }
}

#[cfg(unix)]
fn terminate_child(child: &mut Child) {
    if let Some(pgid) = libc::pid_t::try_from(child.id())
        .ok()
        .filter(|pgid| *pgid > 0)
    {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
pub fn run_bounded_command(
    _program: &Path,
    _args: &[OsString],
    _timeout: Duration,
    _stdout_cap: usize,
    _stderr_cap: usize,
) -> std::result::Result<BoundedOutput, BoundedCommandError> {
    Err(BoundedCommandError::UnsupportedPlatform)
}

#[cfg(unix)]
pub fn run_bounded_command(
    program: &Path,
    args: &[OsString],
    timeout: Duration,
    stdout_cap: usize,
    stderr_cap: usize,
) -> std::result::Result<BoundedOutput, BoundedCommandError> {
    use std::os::unix::process::CommandExt;

    let mut command = release::quiet_command(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(BoundedCommandError::Spawn)?;
    let Some(stdout_pipe) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err(BoundedCommandError::Spawn(io::Error::other(
            "child stdout pipe unavailable",
        )));
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        terminate_child(&mut child);
        return Err(BoundedCommandError::Spawn(io::Error::other(
            "child stderr pipe unavailable",
        )));
    };

    let stdout_status = StreamStatus::new();
    let stderr_status = StreamStatus::new();
    let stdout_thread_status = Arc::clone(&stdout_status);
    let stderr_thread_status = Arc::clone(&stderr_status);

    let stdout_handle = match thread::Builder::new()
        .name("bridge-stdout".to_string())
        .spawn(move || read_capped_stream(stdout_pipe, stdout_cap, stdout_thread_status))
    {
        Ok(handle) => handle,
        Err(error) => {
            terminate_child(&mut child);
            return Err(BoundedCommandError::Spawn(error));
        }
    };
    let stderr_handle = match thread::Builder::new()
        .name("bridge-stderr".to_string())
        .spawn(move || read_capped_stream(stderr_pipe, stderr_cap, stderr_thread_status))
    {
        Ok(handle) => handle,
        Err(error) => {
            terminate_child(&mut child);
            let _ = stdout_handle.join();
            return Err(BoundedCommandError::Spawn(error));
        }
    };

    let started = Instant::now();
    let poll_interval = Duration::from_millis(5);
    let outcome = loop {
        let stdout_done = stdout_status.done.load(Ordering::Acquire);
        let stderr_done = stderr_status.done.load(Ordering::Acquire);
        let both_done = stdout_done && stderr_done;
        if stdout_status.overflow.load(Ordering::Acquire) {
            terminate_child(&mut child);
            break BoundedWaitOutcome::Overflow("stdout", stdout_cap);
        }
        if stderr_status.overflow.load(Ordering::Acquire) {
            terminate_child(&mut child);
            break BoundedWaitOutcome::Overflow("stderr", stderr_cap);
        }
        if stdout_status.read_error.load(Ordering::Acquire)
            || stderr_status.read_error.load(Ordering::Acquire)
        {
            terminate_child(&mut child);
            break BoundedWaitOutcome::ReadError;
        }
        if both_done {
            match child.try_wait() {
                Ok(Some(status)) => break BoundedWaitOutcome::Exited(status),
                Ok(None) => {}
                Err(error) => {
                    terminate_child(&mut child);
                    break BoundedWaitOutcome::WaitError(error);
                }
            }
        }
        if started.elapsed() >= timeout {
            terminate_child(&mut child);
            break BoundedWaitOutcome::Timeout;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(poll_interval));
    };

    let stdout_result = stdout_handle
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("stdout reader thread panicked")));
    let stderr_result = stderr_handle
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("stderr reader thread panicked")));

    match outcome {
        BoundedWaitOutcome::Timeout => Err(BoundedCommandError::Timeout(timeout)),
        BoundedWaitOutcome::Overflow(stream, limit) => {
            Err(BoundedCommandError::OutputLimitExceeded { stream, limit })
        }
        BoundedWaitOutcome::WaitError(error) => Err(BoundedCommandError::Wait(error)),
        BoundedWaitOutcome::ReadError => {
            if let Err(error) = stdout_result {
                return Err(BoundedCommandError::ReadError {
                    stream: "stdout",
                    error,
                });
            }
            if let Err(error) = stderr_result {
                return Err(BoundedCommandError::ReadError {
                    stream: "stderr",
                    error,
                });
            }
            Err(BoundedCommandError::ReadError {
                stream: "stdout",
                error: io::Error::other("reader thread aborted"),
            })
        }
        BoundedWaitOutcome::Exited(status) => {
            if stdout_status.overflow.load(Ordering::Acquire) {
                return Err(BoundedCommandError::OutputLimitExceeded {
                    stream: "stdout",
                    limit: stdout_cap,
                });
            }
            if stderr_status.overflow.load(Ordering::Acquire) {
                return Err(BoundedCommandError::OutputLimitExceeded {
                    stream: "stderr",
                    limit: stderr_cap,
                });
            }
            let stdout = stdout_result.map_err(|error| BoundedCommandError::ReadError {
                stream: "stdout",
                error,
            })?;
            let stderr = stderr_result.map_err(|error| BoundedCommandError::ReadError {
                stream: "stderr",
                error,
            })?;
            Ok(BoundedOutput {
                status,
                stdout,
                stderr,
            })
        }
    }
}

#[derive(Deserialize)]
struct HeaderAdmissionEstimateReport {
    admitted_max_resident_layers: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderAdmissionReport {
    mode: String,
    artifact_verified: bool,
    coordinator_url: String,
    coordinator_id: String,
    transaction_domain: String,
    expected_model_id: String,
    max_working_set_bytes: u64,
    admission: HeaderAdmissionEstimateReport,
}

pub fn parse_and_verify_header_admission_report(
    stdout: &[u8],
    pin: &ShardWorkerPin,
    expected_budget: u64,
) -> std::result::Result<usize, String> {
    if stdout.is_empty() {
        return Err("community-shard-worker --check-header-only produced empty output".to_string());
    }
    let report: HeaderAdmissionReport = serde_json::from_slice(stdout).map_err(|error| {
        format!("community-shard-worker --check-header-only produced invalid JSON: {error}")
    })?;
    if report.mode != HEADER_ADMISSION_REPORT_MODE {
        return Err(format!(
            "community-shard-worker --check-header-only returned unexpected mode {:?}",
            report.mode
        ));
    }
    if report.artifact_verified {
        return Err(
            "community-shard-worker --check-header-only must report artifact_verified=false"
                .to_string(),
        );
    }
    let normalized_url = report
        .coordinator_url
        .strip_suffix('/')
        .unwrap_or(&report.coordinator_url);
    if normalized_url != pin.coordinator_url {
        return Err(format!(
            "community-shard-worker --check-header-only coordinator_url {:?} did not match pinned {}",
            report.coordinator_url, pin.coordinator_url
        ));
    }
    if report.coordinator_id != pin.coordinator_id {
        return Err(
            "community-shard-worker --check-header-only coordinator_id mismatch".to_string(),
        );
    }
    if report.transaction_domain != pin.transaction_domain {
        return Err(
            "community-shard-worker --check-header-only transaction_domain mismatch".to_string(),
        );
    }
    if report.expected_model_id != pin.expected_model_id {
        return Err(
            "community-shard-worker --check-header-only expected_model_id mismatch".to_string(),
        );
    }
    if report.max_working_set_bytes != expected_budget {
        return Err(format!(
            "community-shard-worker --check-header-only max_working_set_bytes {} did not match {expected_budget}",
            report.max_working_set_bytes
        ));
    }
    let required_layers = pin
        .required_admitted_layers
        .max(SHARD_REQUIRED_ADMITTED_LAYERS_FLOOR);
    if report.admission.admitted_max_resident_layers < required_layers {
        return Err(format!(
            "admitted_max_resident_layers={} is below the required {required_layers}-layer tile floor",
            report.admission.admitted_max_resident_layers
        ));
    }
    Ok(report.admission.admitted_max_resident_layers)
}

fn header_check_args(
    pin: &ShardWorkerPin,
    model: &Path,
    max_working_set_bytes: u64,
    required_layers: usize,
) -> Vec<OsString> {
    vec![
        OsString::from("community-shard-worker"),
        OsString::from("--coordinator-url"),
        OsString::from(&pin.coordinator_url),
        OsString::from("--coordinator-id"),
        OsString::from(&pin.coordinator_id),
        OsString::from("--transaction-domain"),
        OsString::from(&pin.transaction_domain),
        OsString::from("--expected-model-id"),
        OsString::from(&pin.expected_model_id),
        OsString::from("--model-path"),
        model.as_os_str().to_os_string(),
        OsString::from("--worker-name"),
        OsString::from(PREFLIGHT_WORKER_NAME),
        OsString::from("--max-working-set-bytes"),
        OsString::from(max_working_set_bytes.to_string()),
        OsString::from("--max-resident-layers"),
        OsString::from(required_layers.to_string()),
        OsString::from("--threads"),
        OsString::from("1"),
        OsString::from("--check-header-only"),
    ]
}

fn first_stderr_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn ensure_regular_model_file(model: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(model).map_err(|error| {
        exit::refused(format!("cannot inspect model {}: {error}", model.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(exit::refused(format!(
            "model path {} must be a regular file, not a symlink or directory",
            model.display()
        )));
    }
    Ok(())
}

fn try_qualify_shard(
    pins: &Pins,
    node_binary: &Path,
    model: &Path,
    preflight_timeout: Duration,
    mut measure_memory: impl FnMut() -> std::result::Result<HostMemory, String>,
) -> std::result::Result<WorkerLaunchRole, String> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err("headless shard worker is not supported on this platform".to_string());
    }
    let Some(pin) = pins.shard_worker.as_ref() else {
        return Err("the pinned release does not include a shard worker configuration".to_string());
    };

    let preflight_memory = measure_memory()?;
    let preflight_budget = qualify_host_memory(preflight_memory)?;

    let required_layers = pin
        .required_admitted_layers
        .max(SHARD_REQUIRED_ADMITTED_LAYERS_FLOOR);
    let args = header_check_args(pin, model, preflight_budget, required_layers);
    let output = run_bounded_command(
        node_binary,
        &args,
        preflight_timeout,
        HEADER_CHECK_STDOUT_CAP_BYTES,
        HEADER_CHECK_STDERR_CAP_BYTES,
    )
    .map_err(|error| format!("community-shard-worker --check-header-only failed: {error}"))?;
    if !output.status.success() {
        return Err(match first_stderr_line(&output.stderr) {
            Some(line) => format!(
                "community-shard-worker --check-header-only exited with {}: {line}",
                output.status
            ),
            None => format!(
                "community-shard-worker --check-header-only exited with {}",
                output.status
            ),
        });
    }
    let admitted_layers =
        parse_and_verify_header_admission_report(&output.stdout, pin, preflight_budget)?;

    let launch_memory = measure_memory()?;
    let launch_budget = qualify_host_memory(launch_memory)?;
    if launch_budget < preflight_budget {
        return Err(format!(
            "available host memory budget dropped from {preflight_budget} to {launch_budget} bytes after preflight"
        ));
    }

    Ok(WorkerLaunchRole::CommunityShardWorker {
        model: model.to_path_buf(),
        pin: Box::new(pin.clone()),
        max_working_set_bytes: preflight_budget,
        admitted_max_resident_layers: admitted_layers,
    })
}

pub fn resolve_worker_role(
    layout: &Layout,
    pins: &Pins,
    node_binary: &Path,
    compute: &Compute,
    log: &mut Log,
) -> Result<WorkerLaunchRole> {
    let Compute::On(model) = compute else {
        return Ok(WorkerLaunchRole::from_compute(compute));
    };
    if layout.kind == LegacyKind::Desktop {
        return Ok(WorkerLaunchRole::Observer(
            "the updated ARC desktop app asks whether this computer keeps contributing compute"
                .to_string(),
        ));
    }
    let mode = consent::read_worker_mode(layout)?;
    resolve_worker_role_with(
        mode,
        pins,
        node_binary,
        model,
        log,
        HEADER_CHECK_TIMEOUT,
        measure_host_memory,
    )
}

pub(crate) fn resolve_worker_role_with(
    mode: WorkerModePreference,
    pins: &Pins,
    node_binary: &Path,
    model: &Path,
    log: &mut Log,
    preflight_timeout: Duration,
    measure_memory: impl FnMut() -> std::result::Result<HostMemory, String>,
) -> Result<WorkerLaunchRole> {
    ensure_regular_model_file(model)?;
    match mode {
        WorkerModePreference::Full => Ok(WorkerLaunchRole::FullIntegerWorker {
            model: model.to_path_buf(),
            fallback_reason: None,
        }),
        WorkerModePreference::Auto => {
            match try_qualify_shard(pins, node_binary, model, preflight_timeout, measure_memory) {
                Ok(role) => Ok(role),
                Err(reason) => {
                    log.warn(&format!(
                        "partial-shard worker qualification fell back to full integer worker: {reason}"
                    ));
                    Ok(WorkerLaunchRole::FullIntegerWorker {
                        model: model.to_path_buf(),
                        fallback_reason: Some(reason),
                    })
                }
            }
        }
        WorkerModePreference::Shard => {
            try_qualify_shard(pins, node_binary, model, preflight_timeout, measure_memory).map_err(
                |reason| exit::refused(format!("explicit shard worker mode refused: {reason}")),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pin() -> ShardWorkerPin {
        ShardWorkerPin {
            coordinator_url: "https://149.28.153.31".to_string(),
            coordinator_id: "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b"
                .to_string(),
            transaction_domain: "3b9147e44d878646caf2d0729d06bd2e082b265e0525760595bd10605f37a10f"
                .to_string(),
            expected_model_id: "934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67"
                .to_string(),
            required_admitted_layers: 6,
        }
    }

    fn pins_with_shard() -> Pins {
        let mut pins = Pins::embedded().unwrap();
        pins.shard_worker = Some(sample_pin());
        pins
    }

    fn valid_header_admission_report_json(
        pin: &ShardWorkerPin,
        budget: u64,
        layers: usize,
    ) -> serde_json::Value {
        serde_json::json!({
            "mode": HEADER_ADMISSION_REPORT_MODE,
            "artifact_verified": false,
            "coordinator_url": pin.coordinator_url,
            "coordinator_id": pin.coordinator_id,
            "transaction_domain": pin.transaction_domain,
            "expected_model_id": pin.expected_model_id,
            "max_working_set_bytes": budget,
            "admission": {
                "admitted_max_resident_layers": layers,
                "future_nested_field": 1
            }
        })
    }

    fn write_fixture_file(root: &Path, relative: &str, content: &str) -> PathBuf {
        let mut path = root.to_path_buf();
        for part in relative.split('/') {
            path = path.join(part);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn nominal_ram_and_memory_qualification_match_installer_policy() {
        assert_eq!(nominal_ram_gb(12 * GIB), 12);
        assert_eq!(nominal_ram_gb(13 * GIB), 13);
        assert_eq!(nominal_ram_gb(14 * GIB), 16);
        assert_eq!(nominal_ram_gb(16 * GIB), 16);
        assert_eq!(nominal_ram_gb(30 * GIB), 32);

        let budget_16 = qualify_host_memory(HostMemory {
            visible_bytes: 16 * GIB,
            available_bytes: 6 * GIB,
        })
        .unwrap();
        assert_eq!(budget_16, 8 * GIB);

        let budget_14 = qualify_host_memory(HostMemory {
            visible_bytes: 14 * GIB,
            available_bytes: 6 * GIB,
        })
        .unwrap();
        assert_eq!(budget_14, 7 * GIB);

        let budget_32 = qualify_host_memory(HostMemory {
            visible_bytes: 32 * GIB,
            available_bytes: 20 * GIB,
        })
        .unwrap();
        assert_eq!(budget_32, 16 * GIB);

        assert!(
            qualify_host_memory(HostMemory {
                visible_bytes: 12 * GIB,
                available_bytes: 10 * GIB,
            })
            .is_err()
        );
        assert!(
            qualify_host_memory(HostMemory {
                visible_bytes: 16 * GIB,
                available_bytes: 6 * GIB - 1,
            })
            .is_err()
        );
        assert!(
            qualify_host_memory(HostMemory {
                visible_bytes: 4 * GIB,
                available_bytes: 6 * GIB,
            })
            .is_err()
        );
    }

    #[test]
    fn linux_meminfo_parser_handles_available_fallback_and_rejects_malformed() {
        let with_available = "\
MemTotal:       16777216 kB
MemFree:         2097152 kB
MemAvailable:    8388608 kB
Buffers:          524288 kB
Cached:          1048576 kB
";
        assert_eq!(
            parse_linux_meminfo(with_available).unwrap(),
            HostMemory {
                visible_bytes: 16 * GIB,
                available_bytes: 8 * GIB,
            }
        );

        let fallback = "\
MemTotal:       16777216 kB
MemFree:         4194304 kB
Buffers:         1048576 kB
Cached:          2097152 kB
";
        assert_eq!(
            parse_linux_meminfo(fallback).unwrap(),
            HostMemory {
                visible_bytes: 16 * GIB,
                available_bytes: 7 * GIB,
            }
        );

        for bad in [
            "",
            "MemTotal: 0 kB\nMemAvailable: 1024 kB\n",
            "MemTotal: 16777216 kB\n",
            "MemTotal: 16777216 kB\nMemTotal: 16777216 kB\nMemAvailable: 8388608 kB\n",
            "MemTotal: 16777216 MB\nMemAvailable: 8388608 kB\n",
            "MemTotal: abc kB\nMemAvailable: 8388608 kB\n",
        ] {
            assert!(parse_linux_meminfo(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn macos_vm_stat_parser_handles_page_sizes_and_rejects_malformed() {
        let arm64 = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                               131072.
Pages active:                             200000.
Pages inactive:                           262144.
Pages speculative:                        131072.
";
        assert_eq!(parse_macos_vm_stat(arm64).unwrap(), 8 * GIB);

        let intel = "\
Mach Virtual Memory Statistics: (page size of 4096 bytes)
Pages free: 1000.
Pages inactive: 2000.
";
        assert_eq!(parse_macos_vm_stat(intel).unwrap(), 3000 * 4096);

        for bad in [
            "",
            "Mach Virtual Memory Statistics:\nPages free: 1000.\n",
            "Unexpected Header: (page size of 4096 bytes)\nPages free: 1000.\n",
            "Mach Virtual Memory Statistics: (page size of 16384)\nPages free: 1000.\n",
            "Mach Virtual Memory Statistics: (page size of 16384 bytes) extra\nPages free: 1000.\n",
            "Mach Virtual Memory Statistics: (page size of 0 bytes)\nPages free: 1.\n",
            "Mach Virtual Memory Statistics: (page size of 12345 bytes)\nPages free: 1.\n",
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages active: 10.\n",
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: bad.\n",
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 10..\n",
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 10.\nPages free: 20.\n",
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages inactive: 10.\nPages inactive: 20.\n",
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages speculative: 10.\nPages speculative: 20.\n",
        ] {
            assert!(parse_macos_vm_stat(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn classify_stored_compute_uses_anchored_prefixes_not_substrings() {
        let observer = WorkerLaunchRole::Observer(
            "partial shard model present at /srv/on (partial shard, /llama.gguf".to_string(),
        );
        assert_eq!(
            classify_stored_compute(&observer.describe()),
            StoredComputeRole::Observer
        );

        let full_with_tricky_path = WorkerLaunchRole::FullIntegerWorker {
            model: PathBuf::from("/srv/partial shard/llama.gguf"),
            fallback_reason: None,
        };
        assert_eq!(
            classify_stored_compute(&full_with_tricky_path.describe()),
            StoredComputeRole::FullIntegerWorker
        );

        let fallback_with_tricky_path = WorkerLaunchRole::FullIntegerWorker {
            model: PathBuf::from("/srv/on (partial shard, /llama.gguf"),
            fallback_reason: Some("partial shard preflight failed".to_string()),
        };
        assert_eq!(
            classify_stored_compute(&fallback_with_tricky_path.describe()),
            StoredComputeRole::FullIntegerWorker
        );

        let legacy_whole =
            Compute::On(PathBuf::from("/opt/partial shard/llama2-7b.gguf")).describe();
        assert_eq!(
            classify_stored_compute(&legacy_whole),
            StoredComputeRole::FullIntegerWorker
        );

        let shard = WorkerLaunchRole::CommunityShardWorker {
            model: PathBuf::from("/models/llama.gguf"),
            pin: Box::new(sample_pin()),
            max_working_set_bytes: 8 * GIB,
            admitted_max_resident_layers: 6,
        };
        assert_eq!(
            classify_stored_compute(&shard.describe()),
            StoredComputeRole::CommunityShardWorker
        );

        assert_eq!(
            classify_stored_compute("arbitrary text mentioning partial shard"),
            StoredComputeRole::Unknown
        );
    }

    #[test]
    fn header_admission_report_validation_checks_all_pinned_fields() {
        assert_eq!(HEADER_CHECK_TIMEOUT, Duration::from_secs(10));
        let pin = sample_pin();
        let budget = 8 * GIB;
        let valid =
            serde_json::to_vec(&valid_header_admission_report_json(&pin, budget, 6)).unwrap();
        assert_eq!(
            parse_and_verify_header_admission_report(&valid, &pin, budget).unwrap(),
            6
        );

        let mut trailing_slash = valid_header_admission_report_json(&pin, budget, 8);
        trailing_slash["coordinator_url"] = serde_json::json!("https://149.28.153.31/");
        let bytes = serde_json::to_vec(&trailing_slash).unwrap();
        assert_eq!(
            parse_and_verify_header_admission_report(&bytes, &pin, budget).unwrap(),
            8
        );

        assert!(parse_and_verify_header_admission_report(b"", &pin, budget).is_err());
        assert!(parse_and_verify_header_admission_report(b"not-json", &pin, budget).is_err());

        for bad_mode_value in ["check-only", "run"] {
            let mut bad_mode = valid_header_admission_report_json(&pin, budget, 6);
            bad_mode["mode"] = serde_json::json!(bad_mode_value);
            let bytes = serde_json::to_vec(&bad_mode).unwrap();
            assert!(
                parse_and_verify_header_admission_report(&bytes, &pin, budget).is_err(),
                "{bad_mode_value}"
            );
        }

        let mut verified_true = valid_header_admission_report_json(&pin, budget, 6);
        verified_true["artifact_verified"] = serde_json::json!(true);
        let bytes = serde_json::to_vec(&verified_true).unwrap();
        assert!(parse_and_verify_header_admission_report(&bytes, &pin, budget).is_err());

        let mut missing_verified = valid_header_admission_report_json(&pin, budget, 6);
        missing_verified
            .as_object_mut()
            .unwrap()
            .remove("artifact_verified");
        let bytes = serde_json::to_vec(&missing_verified).unwrap();
        assert!(parse_and_verify_header_admission_report(&bytes, &pin, budget).is_err());

        let mut legacy_model_key = valid_header_admission_report_json(&pin, budget, 6);
        let obj = legacy_model_key.as_object_mut().unwrap();
        obj.remove("expected_model_id");
        obj.insert(
            "model_id".to_string(),
            serde_json::json!(pin.expected_model_id),
        );
        let bytes = serde_json::to_vec(&legacy_model_key).unwrap();
        assert!(parse_and_verify_header_admission_report(&bytes, &pin, budget).is_err());

        let mut extra_verified_model_id = valid_header_admission_report_json(&pin, budget, 6);
        extra_verified_model_id.as_object_mut().unwrap().insert(
            "model_id".to_string(),
            serde_json::json!(pin.expected_model_id),
        );
        let bytes = serde_json::to_vec(&extra_verified_model_id).unwrap();
        assert!(parse_and_verify_header_admission_report(&bytes, &pin, budget).is_err());

        let mut unknown_outer_field = valid_header_admission_report_json(&pin, budget, 6);
        unknown_outer_field
            .as_object_mut()
            .unwrap()
            .insert("unknown_outer_field".to_string(), serde_json::json!(true));
        let bytes = serde_json::to_vec(&unknown_outer_field).unwrap();
        assert!(parse_and_verify_header_admission_report(&bytes, &pin, budget).is_err());

        let mut bad_url = valid_header_admission_report_json(&pin, budget, 6);
        bad_url["coordinator_url"] = serde_json::json!("https://149.28.153.31//");
        let bytes = serde_json::to_vec(&bad_url).unwrap();
        assert!(parse_and_verify_header_admission_report(&bytes, &pin, budget).is_err());

        for field in ["coordinator_id", "transaction_domain", "expected_model_id"] {
            let mut tampered = valid_header_admission_report_json(&pin, budget, 6);
            tampered[field] = serde_json::json!("1".repeat(64));
            let bytes = serde_json::to_vec(&tampered).unwrap();
            assert!(
                parse_and_verify_header_admission_report(&bytes, &pin, budget).is_err(),
                "{field}"
            );
        }

        let bad_budget =
            serde_json::to_vec(&valid_header_admission_report_json(&pin, budget - 1, 6)).unwrap();
        assert!(parse_and_verify_header_admission_report(&bad_budget, &pin, budget).is_err());

        let too_few_layers =
            serde_json::to_vec(&valid_header_admission_report_json(&pin, budget, 5)).unwrap();
        assert!(parse_and_verify_header_admission_report(&too_few_layers, &pin, budget).is_err());
    }

    #[test]
    fn linux_cgroup_v2_respects_nested_ancestors_custom_mounts_and_unbounded_max() {
        use crate::layout::test_support::TempDir;

        let host = HostMemory {
            visible_bytes: 32 * GIB,
            available_bytes: 20 * GIB,
        };

        let nested = TempDir::new("cgroup-v2-nested");
        fs::create_dir_all(nested.path().join("sys").join("fs").join("cgroup")).unwrap();
        write_fixture_file(
            nested.path(),
            "sys/fs/cgroup/parent/memory.max",
            &format!("{}\n", 16 * GIB),
        );
        write_fixture_file(
            nested.path(),
            "sys/fs/cgroup/parent/memory.current",
            &format!("{}\n", 4 * GIB),
        );
        write_fixture_file(
            nested.path(),
            "sys/fs/cgroup/parent/child/memory.max",
            "max\n",
        );
        write_fixture_file(nested.path(), "mnt/narrow/memory.max", "max\n");

        let mountinfo_root_only = "28 24 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n";
        let clamped_ancestor = apply_linux_cgroup_memory_limits(
            host,
            "0::/parent/child\n",
            mountinfo_root_only,
            nested.path(),
        )
        .unwrap();
        assert_eq!(
            clamped_ancestor,
            HostMemory {
                visible_bytes: 16 * GIB,
                available_bytes: 12 * GIB,
            }
        );

        let mountinfo_with_narrow_bind = "\
40 28 0:26 /parent/child /mnt/narrow rw - cgroup2 cgroup2 rw
28 24 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw
";
        let clamped_prefers_shallow = apply_linux_cgroup_memory_limits(
            host,
            "0::/parent/child\n",
            mountinfo_with_narrow_bind,
            nested.path(),
        )
        .unwrap();
        assert_eq!(clamped_prefers_shallow, clamped_ancestor);

        write_fixture_file(
            nested.path(),
            "sys/fs/cgroup/parent/child/memory.max",
            &format!("{}\n", 10 * GIB),
        );
        write_fixture_file(
            nested.path(),
            "sys/fs/cgroup/parent/child/memory.current",
            &format!("{}\n", 2 * GIB),
        );
        let clamped_leaf = apply_linux_cgroup_memory_limits(
            host,
            "0::/parent/child\n",
            mountinfo_root_only,
            nested.path(),
        )
        .unwrap();
        assert_eq!(
            clamped_leaf,
            HostMemory {
                visible_bytes: 10 * GIB,
                available_bytes: 8 * GIB,
            }
        );

        let custom = TempDir::new("cgroup-v2-custom");
        write_fixture_file(
            custom.path(),
            "mnt/custom-cgroup2/memory.max",
            &format!("{}\n", 18 * GIB),
        );
        write_fixture_file(
            custom.path(),
            "mnt/custom-cgroup2/memory.current",
            &format!("{}\n", 2 * GIB),
        );
        write_fixture_file(
            custom.path(),
            "mnt/custom-cgroup2/worker/memory.max",
            "max\n",
        );
        let custom_mountinfo =
            "42 20 0:26 /docker/abc /mnt/custom-cgroup2 ro - cgroup2 cgroup2 rw\n";
        let clamped_custom = apply_linux_cgroup_memory_limits(
            host,
            "0::/docker/abc/worker\n",
            custom_mountinfo,
            custom.path(),
        )
        .unwrap();
        assert_eq!(
            clamped_custom,
            HostMemory {
                visible_bytes: 18 * GIB,
                available_bytes: 16 * GIB,
            }
        );

        let private_ns = TempDir::new("cgroup-v2-private-ns");
        write_fixture_file(
            private_ns.path(),
            "mnt/ns-cgroup2/memory.max",
            &format!("{}\n", 12 * GIB),
        );
        write_fixture_file(
            private_ns.path(),
            "mnt/ns-cgroup2/memory.current",
            &format!("{}\n", 2 * GIB),
        );
        let private_mountinfo = "28 24 0:26 / /mnt/ns-cgroup2 rw - cgroup2 cgroup2 rw\n";
        let clamped_private =
            apply_linux_cgroup_memory_limits(host, "0::/\n", private_mountinfo, private_ns.path())
                .unwrap();
        assert_eq!(
            clamped_private,
            HostMemory {
                visible_bytes: 12 * GIB,
                available_bytes: 10 * GIB,
            }
        );

        let unbounded = TempDir::new("cgroup-v2-unbounded");
        fs::create_dir_all(unbounded.path().join("mnt").join("cg2")).unwrap();
        write_fixture_file(unbounded.path(), "mnt/cg2/parent/memory.max", "max\n");
        write_fixture_file(unbounded.path(), "mnt/cg2/parent/child/memory.max", "max\n");
        let unbounded_mountinfo = "28 24 0:26 / /mnt/cg2 rw - cgroup2 cgroup2 rw\n";
        let unchanged = apply_linux_cgroup_memory_limits(
            host,
            "0::/parent/child\n",
            unbounded_mountinfo,
            unbounded.path(),
        )
        .unwrap();
        assert_eq!(unchanged, host);
    }

    #[test]
    fn linux_cgroup_v1_respects_nested_ancestors_hybrid_and_unbounded_sentinel() {
        use crate::layout::test_support::TempDir;

        let host = HostMemory {
            visible_bytes: 32 * GIB,
            available_bytes: 20 * GIB,
        };
        let temp = TempDir::new("cgroup-v1-nested");
        write_fixture_file(
            temp.path(),
            "mnt/cg1/memory/memory.limit_in_bytes",
            "9223372036854771712\n",
        );
        write_fixture_file(
            temp.path(),
            "mnt/cg1/memory/memory.usage_in_bytes",
            &format!("{GIB}\n"),
        );
        write_fixture_file(
            temp.path(),
            "mnt/cg1/memory/slice/memory.limit_in_bytes",
            &format!("{}\n", 16 * GIB),
        );
        write_fixture_file(
            temp.path(),
            "mnt/cg1/memory/slice/memory.usage_in_bytes",
            &format!("{}\n", 6 * GIB),
        );
        write_fixture_file(
            temp.path(),
            "mnt/cg1/memory/slice/app/memory.limit_in_bytes",
            "9223372036854771712\n",
        );
        write_fixture_file(
            temp.path(),
            "mnt/cg1/memory/slice/app/memory.usage_in_bytes",
            &format!("{}\n", 2 * GIB),
        );

        let proc_cgroup = "8:memory:/slice/app\n0::/slice/app\n";
        let proc_mountinfo = "\
30 24 0:28 / /mnt/cg1/memory rw,nosuid - cgroup cgroup rw,memory
31 24 0:29 / /mnt/cg2 rw - cgroup2 cgroup2 rw
";
        let clamped =
            apply_linux_cgroup_memory_limits(host, proc_cgroup, proc_mountinfo, temp.path())
                .unwrap();
        assert_eq!(
            clamped,
            HostMemory {
                visible_bytes: 16 * GIB,
                available_bytes: 10 * GIB,
            }
        );
    }

    #[test]
    fn linux_cgroup_handles_absent_controller_and_limit_below_usage() {
        use crate::layout::test_support::TempDir;

        let host = HostMemory {
            visible_bytes: 32 * GIB,
            available_bytes: 20 * GIB,
        };
        let temp = TempDir::new("cgroup-absent-and-over");

        let no_memory_ctrl = apply_linux_cgroup_memory_limits(
            host,
            "2:cpu,cpuacct:/app\n",
            "28 24 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
            temp.path(),
        )
        .unwrap();
        assert_eq!(no_memory_ctrl, host);

        fs::create_dir_all(temp.path().join("mnt").join("no-mem")).unwrap();
        write_fixture_file(
            temp.path(),
            "mnt/no-mem/app/cgroup.controllers",
            "cpu pids\n",
        );
        let disabled_v2 = apply_linux_cgroup_memory_limits(
            host,
            "0::/app\n",
            "28 24 0:26 / /mnt/no-mem rw - cgroup2 cgroup2 rw\n",
            temp.path(),
        )
        .unwrap();
        assert_eq!(disabled_v2, host);

        fs::create_dir_all(temp.path().join("mnt").join("over-v2")).unwrap();
        write_fixture_file(
            temp.path(),
            "mnt/over-v2/app/memory.max",
            &format!("{}\n", 16 * GIB),
        );
        write_fixture_file(
            temp.path(),
            "mnt/over-v2/app/memory.current",
            &format!("{}\n", 18 * GIB),
        );
        let over_v2 = apply_linux_cgroup_memory_limits(
            host,
            "0::/app\n",
            "28 24 0:26 / /mnt/over-v2 rw - cgroup2 cgroup2 rw\n",
            temp.path(),
        )
        .unwrap();
        assert_eq!(
            over_v2,
            HostMemory {
                visible_bytes: 16 * GIB,
                available_bytes: 0,
            }
        );
        assert!(qualify_host_memory(over_v2).is_err());

        write_fixture_file(
            temp.path(),
            "mnt/over-v1/memory.limit_in_bytes",
            "9223372036854771712\n",
        );
        write_fixture_file(temp.path(), "mnt/over-v1/memory.usage_in_bytes", "0\n");
        write_fixture_file(
            temp.path(),
            "mnt/over-v1/app/memory.limit_in_bytes",
            &format!("{}\n", 16 * GIB),
        );
        write_fixture_file(
            temp.path(),
            "mnt/over-v1/app/memory.usage_in_bytes",
            &format!("{}\n", 18 * GIB),
        );
        let over_v1 = apply_linux_cgroup_memory_limits(
            host,
            "8:memory:/app\n",
            "30 24 0:28 / /mnt/over-v1 rw - cgroup cgroup rw,memory\n",
            temp.path(),
        )
        .unwrap();
        assert_eq!(
            over_v1,
            HostMemory {
                visible_bytes: 16 * GIB,
                available_bytes: 0,
            }
        );
        assert!(qualify_host_memory(over_v1).is_err());
    }

    #[test]
    fn linux_cgroup_fails_closed_on_path_escapes_and_malformed_controllers() {
        use crate::layout::test_support::TempDir;

        let host = HostMemory {
            visible_bytes: 32 * GIB,
            available_bytes: 20 * GIB,
        };
        let temp = TempDir::new("cgroup-bad");
        let valid_v2_mount = "28 24 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n";

        for bad_cgroup in [
            "0::/../../etc\n",
            "8:memory:/a/../b\n",
            "0::/a/./b\n",
            "0::relative/path\n",
            "0:memory:/app\n",
            "not-a-valid-line\n",
            "0::/a\n0::/b\n",
            "8:memory:/a\n9:memory:/b\n",
        ] {
            assert!(
                apply_linux_cgroup_memory_limits(host, bad_cgroup, valid_v2_mount, temp.path())
                    .is_err(),
                "{bad_cgroup:?}"
            );
        }

        for bad_mountinfo in [
            "malformed without dash separator\n",
            "28 24 0:26 /../escape /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
            "28 24 0:26 / /sys/fs/../cgroup rw - cgroup2 cgroup2 rw\n",
            "28 24 0:26 / /sys/fs/\\0 rw - cgroup2 cgroup2 rw\n",
            "28 24 0:26 /other /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
        ] {
            assert!(
                apply_linux_cgroup_memory_limits(host, "0::/app\n", bad_mountinfo, temp.path())
                    .is_err(),
                "{bad_mountinfo:?}"
            );
        }

        fs::create_dir_all(temp.path().join("bad-max")).unwrap();
        write_fixture_file(temp.path(), "bad-max/app/memory.max", "not-a-number\n");
        assert!(
            apply_linux_cgroup_memory_limits(
                host,
                "0::/app\n",
                "28 24 0:26 / /bad-max rw - cgroup2 cgroup2 rw\n",
                temp.path(),
            )
            .is_err()
        );

        fs::create_dir_all(temp.path().join("missing-current")).unwrap();
        write_fixture_file(
            temp.path(),
            "missing-current/app/memory.max",
            &format!("{}\n", 16 * GIB),
        );
        assert!(
            apply_linux_cgroup_memory_limits(
                host,
                "0::/app\n",
                "28 24 0:26 / /missing-current rw - cgroup2 cgroup2 rw\n",
                temp.path(),
            )
            .is_err()
        );

        fs::create_dir_all(temp.path().join("missing-max")).unwrap();
        write_fixture_file(
            temp.path(),
            "missing-max/app/cgroup.controllers",
            "cpu memory pids\n",
        );
        assert!(
            apply_linux_cgroup_memory_limits(
                host,
                "0::/app\n",
                "28 24 0:26 / /missing-max rw - cgroup2 cgroup2 rw\n",
                temp.path(),
            )
            .is_err()
        );

        fs::create_dir_all(temp.path().join("missing-v1").join("app")).unwrap();
        assert!(
            apply_linux_cgroup_memory_limits(
                host,
                "8:memory:/app\n",
                "30 24 0:28 / /missing-v1 rw - cgroup cgroup rw,memory\n",
                temp.path(),
            )
            .is_err()
        );

        #[cfg(unix)]
        {
            let target_file = write_fixture_file(temp.path(), "outside.txt", "max\n");
            let sym_file_root = temp.path().join("sym-file").join("app");
            fs::create_dir_all(&sym_file_root).unwrap();
            std::os::unix::fs::symlink(&target_file, sym_file_root.join("memory.max")).unwrap();
            assert!(
                apply_linux_cgroup_memory_limits(
                    host,
                    "0::/app\n",
                    "28 24 0:26 / /sym-file rw - cgroup2 cgroup2 rw\n",
                    temp.path(),
                )
                .is_err()
            );

            let real_dir = temp.path().join("real-dir");
            write_fixture_file(&real_dir, "memory.max", "max\n");
            let sym_dir_root = temp.path().join("sym-dir");
            fs::create_dir_all(&sym_dir_root).unwrap();
            std::os::unix::fs::symlink(&real_dir, sym_dir_root.join("app")).unwrap();
            assert!(
                apply_linux_cgroup_memory_limits(
                    host,
                    "0::/app\n",
                    "28 24 0:26 / /sym-dir rw - cgroup2 cgroup2 rw\n",
                    temp.path(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn full_mode_and_off_compute_bypass_preflight_and_missing_pin_fails_closed_for_shard() {
        use crate::layout::test_support::TempDir;

        let temp = TempDir::new("shard-full-missing-pin");
        let model = temp.path().join("model.gguf");
        fs::write(&model, b"regular-model").unwrap();
        let mut pins = Pins::embedded().unwrap();
        pins.shard_worker = None;
        let binary = PathBuf::from("/nonexistent/arc-node");
        let mut log = Log::stderr_only();
        let mut probe_calls = 0usize;

        let role = resolve_worker_role_with(
            WorkerModePreference::Full,
            &pins,
            &binary,
            &model,
            &mut log,
            Duration::from_secs(1),
            || {
                probe_calls += 1;
                Err("memory probe must not run in full mode".to_string())
            },
        )
        .unwrap();
        assert_eq!(probe_calls, 0);
        assert_eq!(
            role,
            WorkerLaunchRole::FullIntegerWorker {
                model: model.clone(),
                fallback_reason: None,
            }
        );

        let fallback = resolve_worker_role_with(
            WorkerModePreference::Auto,
            &pins,
            &binary,
            &model,
            &mut log,
            Duration::from_secs(1),
            || {
                probe_calls += 1;
                Ok(HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 8 * GIB,
                })
            },
        )
        .unwrap();
        assert_eq!(probe_calls, 0);
        assert!(matches!(
            fallback,
            WorkerLaunchRole::FullIntegerWorker {
                fallback_reason: Some(_),
                ..
            }
        ));

        let refused = resolve_worker_role_with(
            WorkerModePreference::Shard,
            &pins,
            &binary,
            &model,
            &mut log,
            Duration::from_secs(1),
            || {
                probe_calls += 1;
                Ok(HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 8 * GIB,
                })
            },
        )
        .unwrap_err();
        assert_eq!(probe_calls, 0);
        assert_eq!(exit::code_for(&refused), exit::EX_CONFIG);

        for mode in [
            WorkerModePreference::Auto,
            WorkerModePreference::Full,
            WorkerModePreference::Shard,
        ] {
            let dir_err = resolve_worker_role_with(
                mode,
                &pins,
                &binary,
                temp.path(),
                &mut log,
                Duration::from_secs(1),
                || {
                    probe_calls += 1;
                    Ok(HostMemory {
                        visible_bytes: 16 * GIB,
                        available_bytes: 8 * GIB,
                    })
                },
            )
            .unwrap_err();
            assert_eq!(probe_calls, 0);
            assert_eq!(exit::code_for(&dir_err), exit::EX_CONFIG);
        }
    }

    #[test]
    fn compute_off_stays_observer_even_with_shard_pin_and_shard_override() {
        use crate::argv::LegacyInvocation;
        use crate::layout::test_support::{TempDir, fake_arc_dir};
        use crate::layout::{prepare, resolve};
        use std::net::SocketAddr;

        let temp = TempDir::new("shard-off-stays-off");
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);
        let invocation = LegacyInvocation {
            kind: LegacyKind::Desktop,
            rpc: "127.0.0.1:9944".parse::<SocketAddr>().unwrap(),
            p2p_port: 9945,
            data_dir: root.clone(),
            seeds_file: None,
            genesis: None,
            model: None,
            community_mode: true,
        };
        let layout = resolve(&launcher, &invocation, temp.path()).unwrap();
        prepare(&layout).unwrap();
        consent::write_worker_mode(&layout, WorkerModePreference::Shard).unwrap();

        let pins = pins_with_shard();
        let mut log = Log::stderr_only();
        let role = resolve_worker_role(
            &layout,
            &pins,
            Path::new("/nonexistent/arc-node"),
            &Compute::Off("the operator turned compute contribution off".to_string()),
            &mut log,
        )
        .unwrap();
        assert_eq!(
            role,
            WorkerLaunchRole::Observer("the operator turned compute contribution off".to_string())
        );
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_platform_memory_probe_fails_closed() {
        assert!(measure_host_memory().is_err());
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_bounded_command_and_shard_qualification_fail_closed_without_spawning() {
        use crate::layout::test_support::TempDir;

        let temp = TempDir::new("non-unix-no-spawn");
        let model = temp.path().join("model.gguf");
        fs::write(&model, b"regular-model").unwrap();
        let sentinel = temp.path().join("spawned.txt");

        let cmd_err = run_bounded_command(
            Path::new("cmd.exe"),
            &[
                OsString::from("/C"),
                OsString::from(format!("echo bad > {}", sentinel.display())),
            ],
            Duration::from_secs(1),
            HEADER_CHECK_STDOUT_CAP_BYTES,
            HEADER_CHECK_STDERR_CAP_BYTES,
        )
        .unwrap_err();
        assert!(matches!(cmd_err, BoundedCommandError::UnsupportedPlatform));
        assert!(!sentinel.exists());

        let pins = pins_with_shard();
        let mut log = Log::stderr_only();
        let mut probe_calls = 0usize;

        let full_role = resolve_worker_role_with(
            WorkerModePreference::Full,
            &pins,
            Path::new("cmd.exe"),
            &model,
            &mut log,
            Duration::from_secs(1),
            || {
                probe_calls += 1;
                Ok(HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 8 * GIB,
                })
            },
        )
        .unwrap();
        assert_eq!(probe_calls, 0);
        assert_eq!(
            full_role,
            WorkerLaunchRole::FullIntegerWorker {
                model: model.clone(),
                fallback_reason: None,
            }
        );
        assert!(!sentinel.exists());

        let auto_role = resolve_worker_role_with(
            WorkerModePreference::Auto,
            &pins,
            Path::new("cmd.exe"),
            &model,
            &mut log,
            Duration::from_secs(1),
            || {
                probe_calls += 1;
                Ok(HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 8 * GIB,
                })
            },
        )
        .unwrap();
        assert_eq!(probe_calls, 0);
        assert!(matches!(
            auto_role,
            WorkerLaunchRole::FullIntegerWorker {
                fallback_reason: Some(_),
                ..
            }
        ));
        assert!(!sentinel.exists());

        let shard_err = resolve_worker_role_with(
            WorkerModePreference::Shard,
            &pins,
            Path::new("cmd.exe"),
            &model,
            &mut log,
            Duration::from_secs(1),
            || {
                probe_calls += 1;
                Ok(HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 8 * GIB,
                })
            },
        )
        .unwrap_err();
        assert_eq!(probe_calls, 0);
        assert_eq!(exit::code_for(&shard_err), exit::EX_CONFIG);
        assert!(!sentinel.exists());
    }

    #[cfg(unix)]
    mod unix_preflight_tests {
        use super::*;
        use crate::layout::test_support::TempDir;
        use std::os::unix::fs::PermissionsExt;

        fn write_script(path: &Path, body: &str) {
            fs::write(path, body.as_bytes()).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn assert_pid_exited(pid: libc::pid_t) {
            assert!(pid > 0);
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let probe = unsafe { libc::kill(pid, 0) };
                if probe == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "descendant pid {pid} is still running after bounded command termination"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        fn valid_preflight_qualifies_and_rechecks_memory_after_header_check() {
            let temp = TempDir::new("shard-preflight-ok");
            let model = temp.path().join("model.gguf");
            fs::write(&model, b"regular-model").unwrap();
            let pins = pins_with_shard();
            let pin = pins.shard_worker.as_ref().unwrap();
            let report =
                serde_json::to_string(&valid_header_admission_report_json(pin, 8 * GIB, 6))
                    .unwrap();
            let args_log = temp.path().join("args.txt");
            let script = temp.path().join("fake-node");
            write_script(
                &script,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' '{report}'\n",
                    args_log.display()
                ),
            );

            let mut log = Log::stderr_only();
            let mut readings = [
                HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 8 * GIB,
                },
                HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 7 * GIB,
                },
            ]
            .into_iter();
            let role = resolve_worker_role_with(
                WorkerModePreference::Auto,
                &pins,
                &script,
                &model,
                &mut log,
                Duration::from_secs(5),
                || Ok(readings.next().unwrap()),
            )
            .unwrap();
            assert_eq!(readings.next(), None);
            assert_eq!(
                role,
                WorkerLaunchRole::CommunityShardWorker {
                    model: model.clone(),
                    pin: Box::new(pin.clone()),
                    max_working_set_bytes: 8 * GIB,
                    admitted_max_resident_layers: 6,
                }
            );
            assert!(role.describe().contains("rewards disabled"));
            let captured_args = fs::read_to_string(&args_log).unwrap();
            let lines: Vec<&str> = captured_args.lines().collect();
            assert_eq!(lines[0], "community-shard-worker");
            assert!(lines.contains(&"--check-header-only"));
            assert!(!lines.contains(&"--check-only"));
            assert!(lines.contains(&"--max-working-set-bytes"));
            assert!(lines.contains(&"--max-resident-layers"));
            assert!(lines.contains(&"6"));
        }

        #[test]
        fn post_preflight_memory_drop_falls_back_in_auto_and_refuses_in_shard() {
            let temp = TempDir::new("shard-preflight-drop");
            let model = temp.path().join("model.gguf");
            fs::write(&model, b"regular-model").unwrap();
            let pins = pins_with_shard();
            let pin = pins.shard_worker.as_ref().unwrap();
            let report =
                serde_json::to_string(&valid_header_admission_report_json(pin, 8 * GIB, 6))
                    .unwrap();
            let script = temp.path().join("fake-node");
            write_script(&script, &format!("#!/bin/sh\nprintf '%s\\n' '{report}'\n"));

            let mut log = Log::stderr_only();
            let mut auto_readings = [
                HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 8 * GIB,
                },
                HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 5 * GIB,
                },
            ]
            .into_iter();
            let auto_role = resolve_worker_role_with(
                WorkerModePreference::Auto,
                &pins,
                &script,
                &model,
                &mut log,
                Duration::from_secs(5),
                || Ok(auto_readings.next().unwrap()),
            )
            .unwrap();
            assert!(matches!(
                auto_role,
                WorkerLaunchRole::FullIntegerWorker {
                    fallback_reason: Some(_),
                    ..
                }
            ));

            let mut shard_readings = [
                HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 8 * GIB,
                },
                HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 5 * GIB,
                },
            ]
            .into_iter();
            let shard_err = resolve_worker_role_with(
                WorkerModePreference::Shard,
                &pins,
                &script,
                &model,
                &mut log,
                Duration::from_secs(5),
                || Ok(shard_readings.next().unwrap()),
            )
            .unwrap_err();
            assert_eq!(exit::code_for(&shard_err), exit::EX_CONFIG);
        }

        #[test]
        fn symlinked_and_non_regular_models_are_rejected_for_all_modes_without_probe_or_spawn() {
            let temp = TempDir::new("shard-symlink-model");
            let real_model = temp.path().join("real.gguf");
            fs::write(&real_model, b"regular-model").unwrap();
            let link_model = temp.path().join("link.gguf");
            std::os::unix::fs::symlink(&real_model, &link_model).unwrap();
            let sentinel = temp.path().join("spawned");
            let script = temp.path().join("fake-node");
            write_script(
                &script,
                &format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
            );

            let pins = pins_with_shard();
            let mut log = Log::stderr_only();
            let mut probe_calls = 0usize;
            for bad_model in [link_model.as_path(), temp.path()] {
                for mode in [
                    WorkerModePreference::Auto,
                    WorkerModePreference::Full,
                    WorkerModePreference::Shard,
                ] {
                    let err = resolve_worker_role_with(
                        mode,
                        &pins,
                        &script,
                        bad_model,
                        &mut log,
                        Duration::from_secs(5),
                        || {
                            probe_calls += 1;
                            Ok(HostMemory {
                                visible_bytes: 16 * GIB,
                                available_bytes: 8 * GIB,
                            })
                        },
                    )
                    .unwrap_err();
                    assert_eq!(exit::code_for(&err), exit::EX_CONFIG, "{mode:?}");
                    assert_eq!(probe_calls, 0, "{mode:?} must reject before memory probe");
                    assert!(!sentinel.exists(), "{mode:?} must reject before spawn");
                }
            }
        }

        #[test]
        fn bounded_command_enforces_timeout_and_stdout_stderr_caps() {
            let temp = TempDir::new("shard-bounded-cmd");

            let sleeper = temp.path().join("sleep-child");
            write_script(&sleeper, "#!/bin/sh\nsleep 30\n");
            let started = Instant::now();
            let timeout_err = run_bounded_command(
                &sleeper,
                &[],
                Duration::from_millis(150),
                HEADER_CHECK_STDOUT_CAP_BYTES,
                HEADER_CHECK_STDERR_CAP_BYTES,
            )
            .unwrap_err();
            assert!(matches!(timeout_err, BoundedCommandError::Timeout(_)));
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "timeout must kill and reap process group promptly"
            );

            let loud_stdout = temp.path().join("loud-stdout");
            write_script(
                &loud_stdout,
                "#!/bin/sh\ndd if=/dev/zero bs=4096 count=80 2>/dev/null\n",
            );
            let stdout_err = run_bounded_command(
                &loud_stdout,
                &[],
                Duration::from_secs(5),
                HEADER_CHECK_STDOUT_CAP_BYTES,
                HEADER_CHECK_STDERR_CAP_BYTES,
            )
            .unwrap_err();
            assert!(matches!(
                stdout_err,
                BoundedCommandError::OutputLimitExceeded {
                    stream: "stdout",
                    limit: HEADER_CHECK_STDOUT_CAP_BYTES
                }
            ));

            let loud_stderr = temp.path().join("loud-stderr");
            write_script(
                &loud_stderr,
                "#!/bin/sh\ndd if=/dev/zero bs=1024 count=24 >&2 2>/dev/null\n",
            );
            let stderr_err = run_bounded_command(
                &loud_stderr,
                &[],
                Duration::from_secs(5),
                HEADER_CHECK_STDOUT_CAP_BYTES,
                HEADER_CHECK_STDERR_CAP_BYTES,
            )
            .unwrap_err();
            assert!(matches!(
                stderr_err,
                BoundedCommandError::OutputLimitExceeded {
                    stream: "stderr",
                    limit: HEADER_CHECK_STDERR_CAP_BYTES
                }
            ));

            let dual_pipes = temp.path().join("dual-pipes");
            write_script(
                &dual_pipes,
                "#!/bin/sh\ndd if=/dev/zero bs=4096 count=32 2>/dev/null\ndd if=/dev/zero bs=1024 count=16 >&2 2>/dev/null\n",
            );
            let output = run_bounded_command(
                &dual_pipes,
                &[],
                Duration::from_secs(5),
                HEADER_CHECK_STDOUT_CAP_BYTES,
                HEADER_CHECK_STDERR_CAP_BYTES,
            )
            .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout.len(), 128 * 1024);
            assert_eq!(output.stderr.len(), HEADER_CHECK_STDERR_CAP_BYTES);
        }

        #[test]
        fn bounded_command_kills_descendant_holding_pipes_after_parent_exits() {
            let temp = TempDir::new("shard-bounded-descendant");
            let pid_file = temp.path().join("descendant.pid");
            let script = temp.path().join("orphan-pipes");
            write_script(
                &script,
                &format!(
                    "#!/bin/sh\nsleep 30 &\nprintf '%s\\n' \"$!\" > '{}'\nprintf 'parent-exiting\\n'\nprintf 'parent-stderr\\n' >&2\nexit 0\n",
                    pid_file.display()
                ),
            );

            let started = Instant::now();
            let err = run_bounded_command(
                &script,
                &[],
                Duration::from_millis(200),
                HEADER_CHECK_STDOUT_CAP_BYTES,
                HEADER_CHECK_STDERR_CAP_BYTES,
            )
            .unwrap_err();
            assert!(matches!(err, BoundedCommandError::Timeout(_)));
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "deadline must cover both child exit and reader joins"
            );

            let pid_text = fs::read_to_string(&pid_file).unwrap();
            let descendant_pid: libc::pid_t = pid_text.trim().parse().unwrap();
            assert_pid_exited(descendant_pid);
        }

        #[test]
        fn bounded_command_handles_normal_output_and_fast_exit_overflow_races() {
            let temp = TempDir::new("shard-bounded-races");
            let timeout = Duration::from_secs(5);

            let normal = temp.path().join("normal-out");
            write_script(
                &normal,
                "#!/bin/sh\nprintf 'hello-stdout\\n'\nprintf 'hello-stderr\\n' >&2\n",
            );
            let output = run_bounded_command(&normal, &[], timeout, 1024, 256).unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"hello-stdout\n");
            assert_eq!(output.stderr, b"hello-stderr\n");

            let exact_caps = temp.path().join("exact-caps");
            write_script(
                &exact_caps,
                "#!/bin/sh\ndd if=/dev/zero bs=1024 count=1 2>/dev/null\ndd if=/dev/zero bs=256 count=1 >&2 2>/dev/null\n",
            );
            let exact = run_bounded_command(&exact_caps, &[], timeout, 1024, 256).unwrap();
            assert!(exact.status.success());
            assert_eq!(exact.stdout.len(), 1024);
            assert_eq!(exact.stderr.len(), 256);

            let fast_stdout = temp.path().join("fast-stdout-overflow");
            write_script(
                &fast_stdout,
                "#!/bin/sh\ndd if=/dev/zero bs=1025 count=1 2>/dev/null\nexit 0\n",
            );
            let stdout_err =
                run_bounded_command(&fast_stdout, &[], timeout, 1024, 256).unwrap_err();
            assert!(matches!(
                stdout_err,
                BoundedCommandError::OutputLimitExceeded {
                    stream: "stdout",
                    limit: 1024
                }
            ));

            let fast_stderr = temp.path().join("fast-stderr-overflow");
            write_script(
                &fast_stderr,
                "#!/bin/sh\ndd if=/dev/zero bs=257 count=1 >&2 2>/dev/null\nexit 0\n",
            );
            let stderr_err =
                run_bounded_command(&fast_stderr, &[], timeout, 1024, 256).unwrap_err();
            assert!(matches!(
                stderr_err,
                BoundedCommandError::OutputLimitExceeded {
                    stream: "stderr",
                    limit: 256
                }
            ));

            let overflow_pid_file = temp.path().join("overflow-descendant.pid");
            let overflow_child = temp.path().join("overflow-descendant");
            write_script(
                &overflow_child,
                &format!(
                    "#!/bin/sh\nsleep 30 &\nprintf '%s\\n' \"$!\" > '{}'\ndd if=/dev/zero bs=1025 count=1 2>/dev/null\nexit 0\n",
                    overflow_pid_file.display()
                ),
            );
            let started = Instant::now();
            let overflow_err =
                run_bounded_command(&overflow_child, &[], timeout, 1024, 256).unwrap_err();
            assert!(matches!(
                overflow_err,
                BoundedCommandError::OutputLimitExceeded {
                    stream: "stdout",
                    limit: 1024
                }
            ));
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "overflow must terminate descendants promptly instead of waiting for timeout"
            );
            let pid_text = fs::read_to_string(&overflow_pid_file).unwrap();
            let descendant_pid: libc::pid_t = pid_text.trim().parse().unwrap();
            assert_pid_exited(descendant_pid);
        }

        #[test]
        fn tampered_or_failing_header_check_falls_back_in_auto_and_refuses_in_shard() {
            let temp = TempDir::new("shard-preflight-fail");
            let model = temp.path().join("model.gguf");
            fs::write(&model, b"regular-model").unwrap();
            let pins = pins_with_shard();
            let pin = pins.shard_worker.as_ref().unwrap();

            let bad_script = temp.path().join("tampered-node");
            let bad_report =
                serde_json::to_string(&valid_header_admission_report_json(pin, 8 * GIB, 5))
                    .unwrap();
            write_script(
                &bad_script,
                &format!("#!/bin/sh\nprintf '%s\\n' '{bad_report}'\n"),
            );

            let failing_script = temp.path().join("failing-node");
            write_script(
                &failing_script,
                "#!/bin/sh\nprintf 'header admission failed\\n' >&2\nexit 1\n",
            );

            let legacy_flag_script = temp.path().join("legacy-v0827-node");
            write_script(
                &legacy_flag_script,
                "#!/bin/sh\nprintf \"error: unexpected argument '--check-header-only' found\\n\" >&2\nexit 2\n",
            );

            let legacy_mode_script = temp.path().join("legacy-mode-node");
            let legacy_report = serde_json::json!({
                "mode": "check-only",
                "coordinator_url": pin.coordinator_url,
                "coordinator_id": pin.coordinator_id,
                "transaction_domain": pin.transaction_domain,
                "model_id": pin.expected_model_id,
                "max_working_set_bytes": 8 * GIB,
                "admission": {
                    "admitted_max_resident_layers": 6
                }
            })
            .to_string();
            write_script(
                &legacy_mode_script,
                &format!("#!/bin/sh\nprintf '%s\\n' '{legacy_report}'\n"),
            );

            let ok_memory = || {
                Ok(HostMemory {
                    visible_bytes: 16 * GIB,
                    available_bytes: 8 * GIB,
                })
            };
            let mut log = Log::stderr_only();

            for script in [
                bad_script.as_path(),
                failing_script.as_path(),
                legacy_flag_script.as_path(),
                legacy_mode_script.as_path(),
            ] {
                let auto_role = resolve_worker_role_with(
                    WorkerModePreference::Auto,
                    &pins,
                    script,
                    &model,
                    &mut log,
                    Duration::from_secs(5),
                    ok_memory,
                )
                .unwrap();
                assert!(matches!(
                    auto_role,
                    WorkerLaunchRole::FullIntegerWorker {
                        fallback_reason: Some(_),
                        ..
                    }
                ));

                let shard_err = resolve_worker_role_with(
                    WorkerModePreference::Shard,
                    &pins,
                    script,
                    &model,
                    &mut log,
                    Duration::from_secs(5),
                    ok_memory,
                )
                .unwrap_err();
                assert_eq!(exit::code_for(&shard_err), exit::EX_CONFIG);
            }
        }
    }
}
