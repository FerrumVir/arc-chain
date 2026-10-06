//! Coarse facts about this computer, read with the operating system's own
//! tools: OS version, CPU and GPU model names, memory, free disk, CPU
//! features and (macOS) Thunderbolt 5. The kit never reads or reports the
//! hostname, user name, network addresses or any serial number; only the
//! fields listed here are parsed out of the tools' output.

use std::path::Path;
use std::process::{Command, Stdio};

use arc_inference::modern::proof::{self, DeviceProfile, IslandProfile};

/// What the probes found. Raw values stay on this computer; the result
/// carries only the coarse labels and classes derived from them.
#[derive(Debug, Default, Clone)]
pub(super) struct Facts {
    pub os_version: Option<String>,
    pub cpu_model: Option<String>,
    pub total_ram: Option<u64>,
    pub available_ram: Option<u64>,
    pub gpu_model: Option<String>,
    pub gpu_vram: Option<u64>,
    pub thunderbolt5: Option<bool>,
}

/// Run a tool and return its standard output (C locale, no input).
fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// The value after `key` on the first line that starts with it (after trimming).
#[cfg_attr(windows, allow(dead_code))]
fn value_after(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim_start().strip_prefix(key))
        .map(|rest| rest.trim().trim_start_matches(':').trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `"8 GB"`, `"1536 MB"` -> bytes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_size(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let number: u64 = parts.next()?.parse().ok()?;
    match parts.next()? {
        "GB" => Some(number << 30),
        "MB" => Some(number << 20),
        _ => None,
    }
}

/// `"15.1.1"` -> `"15.1"`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn major_minor(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// Thunderbolt 5 from the port speeds macOS reports: 80 Gb/s or more.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn thunderbolt5(text: &str) -> Option<bool> {
    let speeds: Vec<u32> = text
        .lines()
        .filter_map(|line| {
            let before = line
                .split("Gb/s")
                .next()
                .filter(|_| line.contains("Gb/s"))?;
            before.split_whitespace().next_back()?.parse().ok()
        })
        .collect();
    speeds.iter().max().map(|&fastest| fastest >= 80)
}

/// The display adapter name from `lspci -mm`, preferring a discrete GPU.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn lspci_display(text: &str) -> Option<String> {
    let virtual_adapters = [
        "Hyper-V",
        "VMware",
        "VirtualBox",
        "QXL",
        "Red Hat",
        "Cirrus",
        "ASPEED",
        "Matrox",
        "virtio",
        "Bochs",
    ];
    let mut found: Vec<String> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
        let [class, vendor, device, ..] = fields[..] else {
            continue;
        };
        if !(class.contains("VGA") || class.contains("3D") || class.contains("Display")) {
            continue;
        }
        if virtual_adapters
            .iter()
            .any(|&v| vendor.contains(v) || device.contains(v))
        {
            continue;
        }
        // "Advanced Micro Devices, Inc. [AMD/ATI]" -> "AMD/ATI";
        // "AD104 [GeForce RTX 4070]" -> "GeForce RTX 4070".
        let bracketed = |text: &str| {
            let start = text.rfind('[')?;
            let end = text[start..].find(']')? + start;
            Some(text[start + 1..end].to_string())
        };
        let vendor_name = bracketed(vendor)
            .or_else(|| vendor.split_whitespace().next().map(str::to_string))
            .unwrap_or_default();
        let device_name = bracketed(device).unwrap_or_else(|| device.to_string());
        found.push(format!("{vendor_name} {device_name}"));
    }
    let discrete = found
        .iter()
        .find(|name| !name.starts_with("Intel"))
        .cloned();
    discrete.or_else(|| found.into_iter().next())
}

#[cfg(target_os = "macos")]
fn probe_os() -> Facts {
    let sysctl = |name: &str| output("sysctl", &["-n", name]).map(|s| s.trim().to_string());
    let total = sysctl("hw.memsize").and_then(|s| s.parse::<u64>().ok());
    // The kernel's own "free memory percentage" (what `memory_pressure` shows).
    let free_percent = sysctl("kern.memorystatus_level").and_then(|s| s.parse::<u64>().ok());
    let available = total
        .zip(free_percent)
        .map(|(total, percent)| total / 100 * percent.min(100));
    let displays = output("system_profiler", &["SPDisplaysDataType"]).unwrap_or_default();
    let thunderbolt = output("system_profiler", &["SPThunderboltDataType"]).unwrap_or_default();
    Facts {
        os_version: output("sw_vers", &["-productVersion"])
            .map(|v| format!("macOS {}", major_minor(v.trim()))),
        cpu_model: sysctl("machdep.cpu.brand_string"),
        total_ram: total,
        available_ram: available,
        gpu_model: value_after(&displays, "Chipset Model"),
        gpu_vram: value_after(&displays, "VRAM (Total)")
            .or_else(|| value_after(&displays, "VRAM (Dynamic, Max)"))
            .and_then(|v| parse_size(&v)),
        thunderbolt5: thunderbolt5(&thunderbolt),
    }
}

#[cfg(target_os = "linux")]
fn probe_os() -> Facts {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kib = |key: &str| {
        value_after(&meminfo, key)
            .and_then(|v| {
                v.split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .map(|n| n * 1024)
    };
    let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |key: &str| {
        release
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    };
    let os_version = match (field("ID="), field("VERSION_ID=")) {
        (Some(id), Some(version)) => Some(format!("{id} {version}")),
        (id, _) => id,
    };
    let cpu_model = output("lscpu", &[])
        .and_then(|text| value_after(&text, "Model name"))
        .or_else(|| {
            let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
            value_after(&cpuinfo, "model name")
        });
    let (gpu_model, gpu_vram) = linux_gpu();
    Facts {
        os_version,
        cpu_model,
        total_ram: kib("MemTotal:"),
        available_ram: kib("MemAvailable:"),
        gpu_model,
        gpu_vram,
        thunderbolt5: None,
    }
}

#[cfg(target_os = "linux")]
fn linux_gpu() -> (Option<String>, Option<u64>) {
    // NVIDIA's driver tool reports the name and memory (MiB) directly.
    if let Some(text) = output(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ],
    ) && let Some(line) = text.lines().next()
    {
        let mut parts = line.split(',');
        let name = parts.next().map(|n| n.trim().to_string());
        let mib = parts.next().and_then(|m| m.trim().parse::<u64>().ok());
        return (name, mib.map(|m| m << 20));
    }
    let name = output("lspci", &["-mm"]).and_then(|text| lspci_display(&text));
    // amdgpu reports its memory in sysfs.
    let vram = std::fs::read_dir("/sys/class/drm")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            std::fs::read_to_string(entry.path().join("device").join("mem_info_vram_total")).ok()
        })
        .filter_map(|text| text.trim().parse::<u64>().ok())
        .max();
    (name, vram)
}

#[cfg(windows)]
const WINDOWS_PROBE: &str = r#"$ErrorActionPreference = 'SilentlyContinue'
$os = Get-CimInstance Win32_OperatingSystem
$cs = Get-CimInstance Win32_ComputerSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
'os_version=' + $os.Version
'ram_total=' + $cs.TotalPhysicalMemory
'ram_free_kib=' + $os.FreePhysicalMemory
'cpu=' + $cpu.Name
Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}\0*' | ForEach-Object { 'gpu=' + $_.DriverDesc + '|' + $_.'HardwareInformation.qwMemorySize' }
"#;

#[cfg(windows)]
fn probe_os() -> Facts {
    let text = output(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", WINDOWS_PROBE],
    )
    .unwrap_or_default();
    let get = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let skip = [
        "Microsoft Basic",
        "Microsoft Remote",
        "Hyper-V",
        "Virtual",
        "Parsec",
    ];
    let gpu = text
        .lines()
        .filter_map(|line| line.strip_prefix("gpu="))
        .filter_map(|entry| {
            let (name, memory) = entry.split_once('|')?;
            let name = name.trim();
            if name.is_empty() || skip.iter().any(|&s| name.contains(s)) {
                return None;
            }
            Some((name.to_string(), memory.trim().parse::<u64>().ok()))
        })
        .max_by_key(|(_, memory)| memory.unwrap_or(0));
    Facts {
        os_version: get("os_version=").map(|v| format!("Windows {v}")),
        cpu_model: get("cpu="),
        total_ram: get("ram_total=").and_then(|v| v.parse().ok()),
        available_ram: get("ram_free_kib=")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kib| kib * 1024),
        gpu_model: gpu.as_ref().map(|(name, _)| name.clone()),
        gpu_vram: gpu.and_then(|(_, memory)| memory),
        thunderbolt5: None,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn probe_os() -> Facts {
    Facts::default()
}

/// Probe this computer (a few seconds at most).
pub(super) fn probe() -> Facts {
    probe_os()
}

/// Free bytes on the file system holding `dir`.
pub(super) fn free_disk(dir: &Path) -> Option<u64> {
    if cfg!(windows) {
        let text = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(New-Object System.IO.DriveInfo([System.IO.Path]::GetPathRoot($env:ARC_PROOF_KIT_PROBE_PATH))).AvailableFreeSpace",
            ])
            .env("ARC_PROOF_KIT_PROBE_PATH", dir)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?
            .stdout;
        return String::from_utf8(text).ok()?.trim().parse().ok();
    }
    let dir = dir.to_str()?;
    let text = output("df", &["-Pk", dir])?;
    let line = text.lines().nth(1)?;
    let kib: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kib * 1024)
}

/// CPU features from a fixed list (no other CPU data is read).
fn cpu_features() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut found = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            found.push("avx2");
        }
        if std::arch::is_x86_feature_detected!("fma") {
            found.push("fma");
        }
        if std::arch::is_x86_feature_detected!("avx512f") {
            found.push("avx512f");
        }
        if std::arch::is_x86_feature_detected!("avx512bw") {
            found.push("avx512bw");
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("dotprod") {
            found.push("dotprod");
        }
        if std::arch::is_aarch64_feature_detected!("i8mm") {
            found.push("i8mm");
        }
        if std::arch::is_aarch64_feature_detected!("sve") {
            found.push("sve");
        }
        if std::arch::is_aarch64_feature_detected!("sve2") {
            found.push("sve2");
        }
    }
    found
}

/// The device profile the result carries.
pub(super) fn device_profile(facts: &Facts) -> DeviceProfile {
    DeviceProfile {
        os: std::env::consts::OS.to_string(),
        os_version: facts.os_version.as_deref().and_then(proof::label),
        arch: std::env::consts::ARCH.to_string(),
        cpu_model: facts.cpu_model.as_deref().and_then(proof::label),
        logical_cpus: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        cpu_features: cpu_features(),
        gpu_model: facts.gpu_model.as_deref().and_then(proof::label),
    }
}

/// The coarse island-planning facts the result carries (unless `--no-island`).
pub(super) fn island_profile(
    facts: &Facts,
    download_bits_per_second: Option<f64>,
) -> IslandProfile {
    IslandProfile {
        memory_class_gb: facts.total_ram.and_then(proof::memory_class_gb),
        unified_memory: cfg!(all(target_os = "macos", target_arch = "aarch64")),
        gpu_vram_class_gb: facts.gpu_vram.and_then(proof::vram_class_gb),
        thunderbolt5: facts.thunderbolt5,
        download_mbps_class: download_bits_per_second.and_then(proof::network_class_mbps),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_sizes_and_versions_parse() {
        let displays = "Graphics/Displays:\n\n    Apple M2 Pro:\n\n      Chipset Model: Apple M2 Pro\n      Type: GPU\n";
        assert_eq!(
            value_after(displays, "Chipset Model").as_deref(),
            Some("Apple M2 Pro")
        );
        assert_eq!(value_after(displays, "VRAM (Total)"), None);
        let intel =
            "      Chipset Model: Intel UHD Graphics 630\n      VRAM (Dynamic, Max): 1536 MB\n";
        assert_eq!(
            value_after(intel, "VRAM (Dynamic, Max)").and_then(|v| parse_size(&v)),
            Some(1536 << 20)
        );
        assert_eq!(parse_size("8 GB"), Some(8 << 30));
        assert_eq!(parse_size("8 TB"), None);
        assert_eq!(major_minor("15.1.1"), "15.1");
        assert_eq!(major_minor("26"), "26");
        let lscpu = "Architecture:            x86_64\nModel name:              AMD EPYC 7763 64-Core Processor\n";
        assert_eq!(
            value_after(lscpu, "Model name").as_deref(),
            Some("AMD EPYC 7763 64-Core Processor")
        );
        let meminfo = "MemTotal:       16374628 kB\nMemAvailable:   12101420 kB\n";
        assert_eq!(
            value_after(meminfo, "MemAvailable:").as_deref(),
            Some("12101420 kB")
        );
    }

    #[test]
    fn thunderbolt_5_is_80_gbps_or_more() {
        let tb4 = "Port:\n  Speed: Up to 40 Gb/s x1\nPort:\n  Speed: Up to 40 Gb/s\n";
        let tb5 = "Port (Receptacle 1):\n  Speed: Up to 120 Gb/s\nPort:\n  Speed: Up to 40 Gb/s\n";
        assert_eq!(thunderbolt5(tb4), Some(false));
        assert_eq!(thunderbolt5(tb5), Some(true));
        assert_eq!(thunderbolt5(""), None);
    }

    #[test]
    fn lspci_names_prefer_a_discrete_gpu_and_skip_virtual_ones() {
        let text = concat!(
            "00:02.0 \"VGA compatible controller\" \"Intel Corporation\" \"Alder Lake-P GT2 [Iris Xe Graphics]\" -r0c -p00 \"Dell\" \"Device 0b14\"\n",
            "01:00.0 \"3D controller\" \"NVIDIA Corporation\" \"AD104 [GeForce RTX 4070]\" -ra1 \"Dell\" \"Device 0b14\"\n",
        );
        assert_eq!(
            lspci_display(text).as_deref(),
            Some("NVIDIA GeForce RTX 4070")
        );
        let amd = "03:00.0 \"VGA compatible controller\" \"Advanced Micro Devices, Inc. [AMD/ATI]\" \"Navi 31 [Radeon RX 7900 XT/7900 XTX]\" -rc8\n";
        assert_eq!(
            lspci_display(amd).as_deref(),
            Some("AMD/ATI Radeon RX 7900 XT/7900 XTX")
        );
        let hyperv = "00:08.0 \"VGA compatible controller\" \"Microsoft Corporation\" \"Hyper-V virtual VGA\"\n";
        assert_eq!(lspci_display(hyperv), None);
        let intel_only =
            "00:02.0 \"VGA compatible controller\" \"Intel Corporation\" \"UHD Graphics 630\"\n";
        assert_eq!(
            lspci_display(intel_only).as_deref(),
            Some("Intel UHD Graphics 630")
        );
    }

    #[test]
    fn the_device_profile_uses_only_labels() {
        let facts = Facts {
            os_version: Some("ubuntu 24.04".into()),
            cpu_model: Some("AMD EPYC\t7763 64-Core Processor".into()),
            total_ram: Some(16_374_628 * 1024),
            available_ram: Some(12_101_420 * 1024),
            gpu_model: Some("[NVIDIA] GeForce RTX 4070\u{2122}".into()),
            gpu_vram: Some(12_282 << 20),
            thunderbolt5: None,
        };
        let device = device_profile(&facts);
        assert_eq!(
            device.cpu_model.as_deref(),
            Some("AMD EPYC 7763 64-Core Processor")
        );
        assert_eq!(device.gpu_model.as_deref(), Some("NVIDIA GeForce RTX 4070"));
        assert!(device.logical_cpus >= 1);
        let island = island_profile(&facts, Some(940e6));
        assert_eq!(island.memory_class_gb, Some(16));
        assert_eq!(island.gpu_vram_class_gb, Some(12));
        assert_eq!(island.download_mbps_class, Some(500));
    }
}
