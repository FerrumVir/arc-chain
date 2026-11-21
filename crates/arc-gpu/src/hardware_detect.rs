use tracing::info;

#[derive(Debug, Clone)]
pub struct HardwareProfile {
    pub cuda_available: bool,
    pub metal_available: bool,
    pub avx512_available: bool,
    pub neon_available: bool,
    pub gpu_name: Option<String>,
    pub cpu_cores: usize,
}

impl HardwareProfile {
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

pub fn detect() -> HardwareProfile {
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let (gpu_name, gpu_backend) = probe_gpu_info();

    let metal_available = is_metal(&gpu_backend);
    let cuda_available = is_cuda_capable(&gpu_name, &gpu_backend);

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

fn is_metal(backend: &str) -> bool {
    backend == "Metal"
}

fn is_cuda_capable(gpu_name: &Option<String>, _backend: &str) -> bool {
    if let Some(name) = gpu_name {
        let lower = name.to_lowercase();
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

#[cfg(target_arch = "x86_64")]
fn detect_avx512() -> bool {
    is_x86_feature_detected!("avx512f")
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_avx512() -> bool {
    false
}

#[cfg(target_arch = "aarch64")]
fn detect_neon() -> bool {
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
        assert!(profile.cpu_cores >= 1);
        assert!(!profile.best_backend_name().is_empty());
    }

    #[test]
    fn test_platform_specific_detection() {
        let profile = detect();

        if cfg!(target_arch = "aarch64") {
            assert!(profile.neon_available, "NEON must be available on aarch64");
        }

        if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            assert!(
                profile.metal_available,
                "Metal should be available on Apple Silicon macOS"
            );
        }

        if cfg!(not(target_arch = "x86_64")) {
            assert!(
                !profile.avx512_available,
                "AVX-512 cannot be available on non-x86_64"
            );
        }
    }

    #[test]
    fn test_best_backend_priority() {
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
