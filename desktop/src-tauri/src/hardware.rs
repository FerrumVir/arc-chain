use crate::types::HardwareInfo;
use sysinfo::System;

const GIB: u64 = 1024 * 1024 * 1024;

/// Product floor for running the canonical 7B worker, in marketed gigabytes
/// (what the machine's spec sheet says: a "16 GB" laptop).
pub const WORKER_MIN_NOMINAL_RAM_GB: u64 = 16;

/// Memory capacities machines are sold with, in GiB.
const MARKETED_RAM_SIZES_GB: &[u64] = &[
    2, 3, 4, 6, 8, 12, 16, 18, 20, 24, 32, 36, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024,
    1536, 2048,
];

/// Largest share of a marketed size that firmware, integrated graphics, and
/// the kernel are allowed to reserve before the OS reports its total (at
/// least 1 GiB). 15% of 16 GiB is 2.4 GiB, which covers an AMD APU's 2 GiB
/// UMA frame buffer.
const MAX_RESERVED_PERCENT: u64 = 15;

/// The marketed capacity ("16 GB") behind the memory the OS reports.
///
/// The OS total excludes memory reserved before the OS sees it. macOS
/// (`hw.memsize`) reports exactly 16 GiB on a 16 GB Mac, but Windows
/// (`GlobalMemoryStatusEx`) typically reports 15.7–15.9 GiB, Linux
/// (`/proc/meminfo` `MemTotal`) 15.2–15.6 GiB, and a 16 GB AMD APU with a
/// 2 GiB frame buffer about 13.9 GiB. Flooring those to whole GiB read
/// "15 GB" and sent 16 GB Windows and Linux machines to observer mode.
///
/// The visible total maps to the smallest marketed size at or above it when
/// the shortfall is within [`MAX_RESERVED_PERCENT`]; otherwise (a VM with an
/// unusual size, say) it stays at whole GiB, rounded down.
pub fn nominal_ram_gb(visible_bytes: u64) -> u64 {
    for &size_gb in MARKETED_RAM_SIZES_GB {
        let size_bytes = size_gb * GIB;
        if visible_bytes > size_bytes {
            continue;
        }
        let tolerance = (size_bytes / 100 * MAX_RESERVED_PERCENT).max(GIB);
        if size_bytes - visible_bytes <= tolerance {
            return size_gb;
        }
        break;
    }
    visible_bytes / GIB
}

pub fn detect() -> HardwareInfo {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_ram_bytes = sys.total_memory();
    let ram_gb = nominal_ram_gb(total_ram_bytes);
    let cpu_cores = sys.cpus().len() as u32;
    let cpu_model = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_else(|| "Unknown CPU".into());

    let (gpu_name, gpu_vram_gb) = detect_gpu();

    let platform = if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "Unknown"
    }
    .into();

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    }
    .into();

    // Recommend only a hardware-fitting role/model. Hardware cannot establish
    // demand, assignment, validator authorization, or a mined reward receipt,
    // so this API deliberately carries no earnings estimate.
    let (recommended_model, recommended_role) = recommend(ram_gb, gpu_vram_gb);

    HardwareInfo {
        platform,
        arch,
        cpu_model,
        cpu_cores,
        ram_gb,
        gpu_name,
        gpu_vram_gb,
        recommended_model: recommended_model.into(),
        recommended_role: recommended_role.into(),
    }
}

fn recommend(ram_gb: u64, _gpu_gb: Option<u64>) -> (&'static str, &'static str) {
    match ram_gb {
        r if r >= WORKER_MIN_NOMINAL_RAM_GB => {
            ("Llama-2-7B Q4_K_M (3.8 GB, ARC compatible)", "worker")
        }
        _ => (
            "Observer/router (16 GB RAM required for ARC 7B work)",
            "verifier",
        ),
    }
}

#[cfg(target_os = "macos")]
fn detect_gpu() -> (Option<String>, Option<u64>) {
    use std::process::Command;
    // system_profiler is slow but reliable on macOS
    let out = Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            let gpu = v
                .get("SPDisplaysDataType")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first());
            let name = gpu
                .and_then(|g| g.get("sppci_model"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_string());
            // On Apple Silicon, VRAM == unified memory
            let mut sys = sysinfo::System::new_all();
            sys.refresh_memory();
            let unified_gb = nominal_ram_gb(sys.total_memory());
            (name, Some(unified_gb))
        }
        _ => (None, None),
    }
}

/// Windows GPU detection via PowerShell CIM.
///
/// Replaces `wmic`, which Windows 11 24H2 and Server 2025 no longer install
/// by default — the command simply failed to launch there, so VRAM read as 0,
/// hardware reporting could not be trusted for safe model sizing. Production
/// eligibility is now pinned to one 7B artifact and a 16 GB RAM floor; GPU
/// discovery remains useful diagnostic information only.
///
/// Two further fixes over the old CSV parse:
/// - It picked the *first* line containing a comma, which on a laptop is
///   usually the integrated adapter rather than the discrete GPU. Adapters
///   are now filtered and the highest-VRAM one wins.
/// - `AdapterRAM` is a signed 32-bit field, so it saturates at 4095 MB for
///   any card with more than 4 GB — exactly the cards worth detecting. The
///   registry's `qwMemorySize` is 64-bit and is preferred when readable.
#[cfg(target_os = "windows")]
fn detect_gpu() -> (Option<String>, Option<u64>) {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // Ask for every adapter plus the 64-bit registry VRAM value, so the
    // 4 GB AdapterRAM ceiling only applies as a last resort.
    const SCRIPT: &str = r#"
$ErrorActionPreference='SilentlyContinue'
$reg = Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}\*' |
       Where-Object { $_.'HardwareInformation.qwMemorySize' -ne $null }
Get-CimInstance Win32_VideoController | ForEach-Object {
  $n = $_.Name
  $qw = ($reg | Where-Object { $_.DriverDesc -eq $n } | Select-Object -First 1).'HardwareInformation.qwMemorySize'
  [pscustomobject]@{ Name = $n; Bytes = [uint64](if ($qw) { $qw } else { [uint64]$_.AdapterRAM }) }
} | ConvertTo-Json -Compress
"#;

    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    let Ok(o) = out else { return (None, None) };
    if !o.status.success() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&o.stdout);
    let parsed: serde_json::Value = match serde_json::from_str(text.trim()) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    // ConvertTo-Json emits a bare object for a single adapter, an array for
    // several.
    let adapters: Vec<serde_json::Value> = match parsed {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };

    let is_basic = |n: &str| {
        let l = n.to_ascii_lowercase();
        l.contains("basic display") || l.contains("basic render") || l.contains("remote display")
    };

    // Prefer the adapter reporting the most VRAM; that is the discrete GPU
    // whenever there is one.
    let best = adapters
        .iter()
        .filter_map(|a| {
            let name = a.get("Name").and_then(|x| x.as_str())?;
            if name.is_empty() || is_basic(name) {
                return None;
            }
            let bytes = a.get("Bytes").and_then(|x| x.as_u64()).unwrap_or(0);
            Some((name.to_string(), bytes))
        })
        .max_by_key(|(_, bytes)| *bytes);

    match best {
        Some((name, bytes)) => {
            let gb = bytes / 1024 / 1024 / 1024;
            (Some(name), (gb > 0).then_some(gb))
        }
        None => (None, None),
    }
}

#[cfg(target_os = "linux")]
fn detect_gpu() -> (Option<String>, Option<u64>) {
    use std::process::Command;
    // Try nvidia-smi first, fall back to lspci
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            let text = String::from_utf8_lossy(&o.stdout);
            if let Some(first) = text.lines().next() {
                let parts: Vec<&str> = first.split(',').collect();
                if parts.len() >= 2 {
                    let name = parts[0].trim().to_string();
                    let vram_mb = parts[1].trim().parse::<u64>().ok();
                    return (Some(name), vram_mb.map(|m| m / 1024));
                }
            }
        }
    }
    let lspci = Command::new("lspci").output();
    if let Ok(o) = lspci {
        let text = String::from_utf8_lossy(&o.stdout);
        let line = text.lines().find(|l| l.contains("VGA") || l.contains("3D"));
        if let Some(l) = line {
            return (
                Some(l.split(':').last().unwrap_or(l).trim().to_string()),
                None,
            );
        }
    }
    (None, None)
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn detect_gpu() -> (Option<String>, Option<u64>) {
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tenths` of a GiB, so the fixtures read like the OS dialogs do.
    fn gib_tenths(tenths: u64) -> u64 {
        tenths * GIB / 10
    }

    #[test]
    fn sixteen_gb_machines_count_as_sixteen_gb_on_every_os() {
        // macOS: `hw.memsize` reports the full installed amount.
        assert_eq!(nominal_ram_gb(16 * GIB), 16);
        // Windows: GlobalMemoryStatusEx excludes firmware and iGPU memory.
        for visible in [159, 158, 157, 154] {
            assert_eq!(nominal_ram_gb(gib_tenths(visible)), 16, "{visible}/10 GiB");
        }
        // Linux: MemTotal also excludes kernel and crash-kernel reservations.
        for visible in [156, 152, 149] {
            assert_eq!(nominal_ram_gb(gib_tenths(visible)), 16, "{visible}/10 GiB");
        }
        // A 16 GB AMD APU with a 2 GiB UMA frame buffer.
        assert_eq!(nominal_ram_gb(gib_tenths(139)), 16);
        // Exactly what the old floor-to-GiB arithmetic turned into "15 GB".
        assert_eq!(nominal_ram_gb(16 * GIB - 1), 16);
        assert_eq!((16 * GIB - 1) / GIB, 15);
    }

    #[test]
    fn sixteen_gb_machines_are_recommended_the_worker_role() {
        for visible in [16 * GIB, gib_tenths(158), gib_tenths(152), gib_tenths(139)] {
            let (_, role) = recommend(nominal_ram_gb(visible), None);
            assert_eq!(role, "worker", "{visible} visible bytes");
        }
    }

    #[test]
    fn smaller_machines_keep_their_own_size_and_stay_observers() {
        // 8 GB and 12 GB machines.
        assert_eq!(nominal_ram_gb(gib_tenths(77)), 8);
        assert_eq!(nominal_ram_gb(gib_tenths(116)), 12);
        // A 12.5 GiB VM is too far below 16 GB to be a reserved 16 GB machine.
        assert_eq!(nominal_ram_gb(gib_tenths(125)), 12);
        // 13.5 GiB is more than 15% below 16 GB.
        assert_eq!(nominal_ram_gb(gib_tenths(135)), 13);
        // Failed detection never rounds up into eligibility.
        assert_eq!(nominal_ram_gb(0), 0);
        for visible in [
            0,
            gib_tenths(77),
            gib_tenths(116),
            gib_tenths(125),
            gib_tenths(135),
        ] {
            let (_, role) = recommend(nominal_ram_gb(visible), None);
            assert_eq!(role, "verifier", "{visible} visible bytes");
        }
    }

    #[test]
    fn larger_machines_keep_their_marketed_size() {
        assert_eq!(nominal_ram_gb(18 * GIB), 18);
        assert_eq!(nominal_ram_gb(gib_tenths(235)), 24);
        assert_eq!(nominal_ram_gb(gib_tenths(313)), 32);
        assert_eq!(nominal_ram_gb(gib_tenths(628)), 64);
        // Beyond the table, whole GiB rounded down.
        assert_eq!(nominal_ram_gb(4096 * GIB), 4096);
    }
}
