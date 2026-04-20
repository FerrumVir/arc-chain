use crate::types::HardwareInfo;
use sysinfo::System;

pub fn detect() -> HardwareInfo {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_ram_bytes = sys.total_memory();
    let ram_gb = total_ram_bytes / 1024 / 1024 / 1024;
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

    // Pick a recommended model + estimated earnings based on RAM + GPU.
    let (recommended_model, estimated_daily_arc, recommended_role) =
        recommend(ram_gb, gpu_vram_gb);

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
        estimated_daily_arc,
    }
}

fn recommend(ram_gb: u64, gpu_gb: Option<u64>) -> (&'static str, f64, &'static str) {
    let vram = gpu_gb.unwrap_or(0);
    match (ram_gb, vram) {
        (r, v) if r >= 64 && v >= 24 => {
            ("Llama-2-70B Q4_K_M (39 GB)", 1200.0, "worker")
        }
        (r, v) if r >= 32 && v >= 16 => {
            ("Llama-2-13B Q4_K_M (7.3 GB)", 420.0, "worker")
        }
        (r, _) if r >= 16 => ("Llama-2-7B Q4_K_M (3.8 GB)", 180.0, "worker"),
        (r, _) if r >= 8 => ("TinyLlama-1.1B Q4 (0.6 GB)", 40.0, "worker"),
        _ => ("Verifier only (no model)", 8.0, "verifier"),
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
            let unified_gb = sys.total_memory() / 1024 / 1024 / 1024;
            (name, Some(unified_gb))
        }
        _ => (None, None),
    }
}

#[cfg(target_os = "windows")]
fn detect_gpu() -> (Option<String>, Option<u64>) {
    // Cheap heuristic: wmic path Win32_VideoController get Name
    use std::process::Command;
    let out = Command::new("wmic")
        .args(["path", "Win32_VideoController", "get", "Name,AdapterRAM", "/format:csv"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let line = text.lines().find(|l| l.contains(","));
            if let Some(l) = line {
                let parts: Vec<&str> = l.split(',').collect();
                if parts.len() >= 3 {
                    let ram = parts[1].trim().parse::<u64>().ok().map(|b| b / 1024 / 1024 / 1024);
                    let name = parts[2].trim().to_string();
                    if !name.is_empty() {
                        return (Some(name), ram);
                    }
                }
            }
            (None, None)
        }
        _ => (None, None),
    }
}

#[cfg(target_os = "linux")]
fn detect_gpu() -> (Option<String>, Option<u64>) {
    use std::process::Command;
    // Try nvidia-smi first, fall back to lspci
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"])
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
