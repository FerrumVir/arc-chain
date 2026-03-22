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
}

/// Detect hardware capabilities at runtime.
///
/// This is designed to be called once at node startup. It probes:
/// - GPU via wgpu adapter (identifies Metal/CUDA/Vulkan + device name)
/// - CPU SIMD features via `is_x86_feature_detected!` / target arch
/// - Logical core count via `std::thread::available_parallelism`
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

    let profile = HardwareProfile {
        cuda_available,
        metal_available,
        avx512_available,
        neon_available,
        gpu_name,
        cpu_cores,
    };

    info!(
        cuda = profile.cuda_available,
        metal = profile.metal_available,
        avx512 = profile.avx512_available,
        neon = profile.neon_available,
        gpu = ?profile.gpu_name,
        cores = profile.cpu_cores,
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
        };
        assert_eq!(cuda_profile.best_backend_name(), "CUDA");

        let metal_profile = HardwareProfile {
            cuda_available: false,
            metal_available: true,
            avx512_available: false,
            neon_available: true,
            gpu_name: Some("Apple M4 Max".into()),
            cpu_cores: 16,
        };
        assert_eq!(metal_profile.best_backend_name(), "Metal");

        let avx_profile = HardwareProfile {
            cuda_available: false,
            metal_available: false,
            avx512_available: true,
            neon_available: false,
            gpu_name: None,
            cpu_cores: 96,
        };
        assert_eq!(avx_profile.best_backend_name(), "AVX-512");

        let neon_profile = HardwareProfile {
            cuda_available: false,
            metal_available: false,
            avx512_available: false,
            neon_available: true,
            gpu_name: None,
            cpu_cores: 8,
        };
        assert_eq!(neon_profile.best_backend_name(), "NEON");

        let cpu_profile = HardwareProfile {
            cuda_available: false,
            metal_available: false,
            avx512_available: false,
            neon_available: false,
            gpu_name: None,
            cpu_cores: 4,
        };
        assert_eq!(cpu_profile.best_backend_name(), "CPU (scalar)");
    }
}
