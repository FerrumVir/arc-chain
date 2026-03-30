//! Runtime hardware capability detection for verification kernel selection.
//!
//! Probes the system at startup to determine which accelerated verification
//! backends are available. Detection priority:
//!
//!   CUDA → Metal → AVX-512 → NEON → CPU (scalar)
//!
//! The caller (pipeline.rs) maps the [`HardwareProfile`] to a `VerifyMode`.

use tracing::info;

/// Summary of detected hardware capabilities.
#[derive(Debug, Clone)]
pub struct HardwareProfile {
    /// NVIDIA CUDA-capable GPU detected and runtime available.
    pub cuda_available: bool,
    /// Apple Metal GPU detected (macOS + Apple Silicon).
    pub metal_available: bool,
    /// x86_64 AVX-512F instruction set available.
    pub avx512_available: bool,
    /// ARM NEON SIMD available (mandatory on aarch64).
    pub neon_available: bool,
    /// GPU device name, if any GPU was detected.
    pub gpu_name: Option<String>,
    /// Number of logical CPU cores.
    pub cpu_cores: usize,
    /// Total system RAM in GB (0 if detection fails).
    pub ram_gb: u64,
}

impl HardwareProfile {
    /// Human-readable summary of the best available backend.
    pub fn best_backend_name(&self) -> &'static str {
        if self.cuda_available {
            "CUDA"
        } else if self.metal_available {
            "Metal"
        } else if self.avx512_available {
            "AVX-512"
        } else if self.neon_available {
            "NEON"
        } else {
            "CPU (scalar)"
        }
    }

    /// Recommend the optimal model size based on detected hardware.
    ///
    /// Decision matrix:
    ///   - RAM < 4 GB  → "none" (relay-only, skip model download)
    ///   - RAM < 8 GB  → "tiny" (TinyLlama 1.1B, ~638 MB)
    ///   - RAM >= 8 GB → "7b"   (Llama 2 7B Chat Q4, ~4.1 GB)
    ///   - RAM >= 32 GB + GPU → "7b" (future: could recommend 13B)
    pub fn recommended_model(&self) -> &'static str {
        if self.ram_gb < 4 {
            "none"
        } else if self.ram_gb < 8 {
            "tiny"
        } else {
            "7b"
        }
    }

    /// Human-readable description of the recommended model.
    pub fn recommended_model_label(&self) -> &'static str {
        match self.recommended_model() {
            "none" => "Relay only (insufficient RAM for local inference)",
            "tiny" => "TinyLlama 1.1B (638 MB) -- best for low-RAM devices",
            "7b" => "Llama 2 7B Chat Q4 (4.1 GB) -- full quality inference",
            _ => "Unknown",
        }
    }
}

/// Detect hardware capabilities at runtime.
///
/// This is designed to be called once at node startup. It probes:
/// - GPU via wgpu adapter (identifies Metal/CUDA/Vulkan + device name)
/// - CPU SIMD features via `is_x86_feature_detected!` / target arch
/// - Logical core count via `std::thread::available_parallelism`
/// - Total system RAM via platform-specific APIs
pub fn detect() -> HardwareProfile {
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    // --- GPU detection via wgpu ---
    let (gpu_name, gpu_backend) = probe_gpu_info();

    let metal_available = is_metal(&gpu_backend);
    let cuda_available = is_cuda_capable(&gpu_name, &gpu_backend);

    // --- CPU SIMD detection ---
    let avx512_available = detect_avx512();
    let neon_available = detect_neon();

    // --- RAM detection ---
    let ram_gb = detect_ram_gb();

    let profile = HardwareProfile {
        cuda_available,
        metal_available,
        avx512_available,
        neon_available,
        gpu_name,
        cpu_cores,
        ram_gb,
    };

    info!(
        cuda = profile.cuda_available,
        metal = profile.metal_available,
        avx512 = profile.avx512_available,
        neon = profile.neon_available,
        gpu = ?profile.gpu_name,
        cores = profile.cpu_cores,
        ram_gb = profile.ram_gb,
        recommended_model = profile.recommended_model(),
        best = profile.best_backend_name(),
        "Hardware detection complete"
    );

    profile
}

/// Probe GPU via wgpu and return (device_name, backend_string).
fn probe_gpu_info() -> (Option<String>, String) {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });

    match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    })) {
        Ok(adapter) => {
            let info = adapter.get_info();
            let name = info.name.clone();
            let backend = format!("{:?}", info.backend);
            (Some(name), backend)
        }
        Err(_) => (None, "none".to_string()),
    }
}

/// Metal is available if wgpu reports a Metal backend (macOS + Apple Silicon).
fn is_metal(backend: &str) -> bool {
    backend == "Metal"
}

/// CUDA is available if we detect an NVIDIA GPU.
///
/// For now, we detect NVIDIA GPUs via their device name reported through wgpu
/// (Vulkan backend on Linux/Windows). When the `cuda` feature is added (week 3),
/// this will also check for cudarc runtime availability.
fn is_cuda_capable(gpu_name: &Option<String>, _backend: &str) -> bool {
    if let Some(name) = gpu_name {
        let lower = name.to_lowercase();
        // NVIDIA GPUs: GeForce, RTX, GTX, Tesla, A100, H100, L40, etc.
        lower.contains("nvidia")
            || lower.contains("geforce")
            || lower.contains("rtx")
            || lower.contains("gtx")
            || lower.contains("tesla")
            || lower.contains("a100")
            || lower.contains("h100")
            || lower.contains("l40")
    } else {
        false
    }
}

/// Detect AVX-512F support at runtime (x86_64 only).
#[cfg(target_arch = "x86_64")]
fn detect_avx512() -> bool {
    is_x86_feature_detected!("avx512f")
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_avx512() -> bool {
    false
}

/// Detect ARM NEON support. NEON is mandatory on aarch64, so this is
/// effectively a target_arch check.
#[cfg(target_arch = "aarch64")]
fn detect_neon() -> bool {
    // NEON is mandatory on aarch64 — always available.
    true
}

#[cfg(not(target_arch = "aarch64"))]
fn detect_neon() -> bool {
    false
}

/// Detect total system RAM in GB.
///
/// - macOS: uses `sysctl hw.memsize`
/// - Linux: reads `/proc/meminfo` for MemTotal
/// - Fallback: returns 0 (unknown)
fn detect_ram_gb() -> u64 {
    detect_ram_gb_impl()
}

#[cfg(target_os = "macos")]
fn detect_ram_gb_impl() -> u64 {
    // sysctl hw.memsize returns total RAM in bytes
    match std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout
                .trim()
                .parse::<u64>()
                .map(|bytes| bytes / (1024 * 1024 * 1024))
                .unwrap_or(0)
        }
        Err(_) => 0,
    }
}

#[cfg(target_os = "linux")]
fn detect_ram_gb_impl() -> u64 {
    // /proc/meminfo first line: "MemTotal:    16384000 kB"
    match std::fs::read_to_string("/proc/meminfo") {
        Ok(contents) => {
            for line in contents.lines() {
                if line.starts_with("MemTotal:") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        return parts[1]
                            .parse::<u64>()
                            .map(|kb| kb / (1024 * 1024))
                            .unwrap_or(0);
                    }
                }
            }
            0
        }
        Err(_) => 0,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn detect_ram_gb_impl() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_returns_valid_profile() {
        let profile = detect();
        // Must always have at least 1 core
        assert!(profile.cpu_cores >= 1);
        // best_backend_name must return a non-empty string
        assert!(!profile.best_backend_name().is_empty());
        // RAM should be detected on macOS and Linux
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(profile.ram_gb > 0, "RAM should be detected on this platform");
        }
        // recommended_model must return a known value
        assert!(
            ["none", "tiny", "7b"].contains(&profile.recommended_model()),
            "recommended_model returned unexpected value: {}",
            profile.recommended_model()
        );
    }

    #[test]
    fn test_platform_specific_detection() {
        let profile = detect();

        // On aarch64, NEON must be available
        if cfg!(target_arch = "aarch64") {
            assert!(profile.neon_available, "NEON must be available on aarch64");
        }

        // On macOS + aarch64, Metal should be available
        if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            assert!(
                profile.metal_available,
                "Metal should be available on Apple Silicon macOS"
            );
        }

        // AVX-512 is only possible on x86_64 (may or may not be present)
        if cfg!(not(target_arch = "x86_64")) {
            assert!(
                !profile.avx512_available,
                "AVX-512 cannot be available on non-x86_64"
            );
        }
    }

    #[test]
    fn test_best_backend_priority() {
        // Verify priority order by constructing profiles manually
        let cuda_profile = HardwareProfile {
            cuda_available: true,
            metal_available: true,
            avx512_available: true,
            neon_available: true,
            gpu_name: Some("NVIDIA H100".into()),
            cpu_cores: 96,
            ram_gb: 128,
        };
        assert_eq!(cuda_profile.best_backend_name(), "CUDA");

        let metal_profile = HardwareProfile {
            cuda_available: false,
            metal_available: true,
            avx512_available: false,
            neon_available: true,
            gpu_name: Some("Apple M4 Max".into()),
            cpu_cores: 16,
            ram_gb: 64,
        };
        assert_eq!(metal_profile.best_backend_name(), "Metal");

        let avx_profile = HardwareProfile {
            cuda_available: false,
            metal_available: false,
            avx512_available: true,
            neon_available: false,
            gpu_name: None,
            cpu_cores: 96,
            ram_gb: 256,
        };
        assert_eq!(avx_profile.best_backend_name(), "AVX-512");

        let neon_profile = HardwareProfile {
            cuda_available: false,
            metal_available: false,
            avx512_available: false,
            neon_available: true,
            gpu_name: None,
            cpu_cores: 8,
            ram_gb: 16,
        };
        assert_eq!(neon_profile.best_backend_name(), "NEON");

        let cpu_profile = HardwareProfile {
            cuda_available: false,
            metal_available: false,
            avx512_available: false,
            neon_available: false,
            gpu_name: None,
            cpu_cores: 4,
            ram_gb: 4,
        };
        assert_eq!(cpu_profile.best_backend_name(), "CPU (scalar)");
    }

    #[test]
    fn test_recommended_model_by_ram() {
        // < 4 GB: relay only
        let low = HardwareProfile {
            cuda_available: false, metal_available: false, avx512_available: false,
            neon_available: false, gpu_name: None, cpu_cores: 2, ram_gb: 2,
        };
        assert_eq!(low.recommended_model(), "none");
        assert!(low.recommended_model_label().contains("Relay"));

        // 4-7 GB: tiny
        let mid = HardwareProfile {
            cuda_available: false, metal_available: false, avx512_available: false,
            neon_available: true, gpu_name: None, cpu_cores: 4, ram_gb: 6,
        };
        assert_eq!(mid.recommended_model(), "tiny");
        assert!(mid.recommended_model_label().contains("TinyLlama"));

        // >= 8 GB: 7b
        let high = HardwareProfile {
            cuda_available: false, metal_available: true, avx512_available: false,
            neon_available: true, gpu_name: Some("Apple M2".into()), cpu_cores: 8, ram_gb: 16,
        };
        assert_eq!(high.recommended_model(), "7b");
        assert!(high.recommended_model_label().contains("Llama 2"));
    }

    #[test]
    fn test_ram_detection_runs() {
        let ram = detect_ram_gb();
        // On CI or test machines, just check it doesn't panic
        // On real macOS/Linux, it should return > 0
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(ram > 0, "detect_ram_gb should return > 0 on macOS/Linux");
        }
    }
}
