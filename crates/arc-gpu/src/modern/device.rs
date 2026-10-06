//! Adapter discovery, selection and device creation.

use std::sync::{Arc, Mutex};

use super::GpuModernError;

/// The GPU a run used, as reported in every result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AdapterReport {
    /// Position in wgpu's adapter enumeration (what `--gpu-adapter N` selects).
    pub index: usize,
    pub name: String,
    pub vendor_id: u32,
    pub vendor: String,
    pub device_id: u32,
    pub device_type: String,
    pub backend: String,
    pub driver: String,
    pub driver_info: String,
    /// Software rasterizers (lavapipe/llvmpipe, WARP, SwiftShader): their
    /// results prove exactness but their timings say nothing about GPU speed.
    pub software: bool,
}

fn vendor_name(id: u32, name: &str) -> &'static str {
    match id {
        0x10DE => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x8086 => "Intel",
        0x106B => "Apple",
        0x5143 => "Qualcomm",
        0x13B5 => "ARM",
        0x1414 => "Microsoft",
        0x1AE0 => "Google",
        0x10005 => "Mesa",
        _ if name.contains("Apple") => "Apple",
        _ => "unknown",
    }
}

fn is_software(info: &wgpu::AdapterInfo) -> bool {
    let name = info.name.to_lowercase();
    info.device_type == wgpu::DeviceType::Cpu
        || [
            "llvmpipe",
            "lavapipe",
            "swiftshader",
            "basic render",
            "warp",
        ]
        .iter()
        .any(|needle| name.contains(needle))
}

fn report(index: usize, info: &wgpu::AdapterInfo) -> AdapterReport {
    AdapterReport {
        index,
        name: info.name.clone(),
        vendor_id: info.vendor,
        vendor: vendor_name(info.vendor, &info.name).to_string(),
        device_id: info.device,
        device_type: format!("{:?}", info.device_type),
        backend: format!("{:?}", info.backend),
        driver: info.driver.clone(),
        driver_info: info.driver_info.clone(),
        software: is_software(info),
    }
}

/// Vulkan, Metal and DX12 by default; `WGPU_BACKEND` (e.g. `vulkan`, `dx12`,
/// `metal`, `gl`) overrides.
fn backends() -> wgpu::Backends {
    wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY)
}

fn instance() -> wgpu::Instance {
    wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: backends(),
        ..Default::default()
    })
}

/// Every adapter wgpu can see, in enumeration order.
pub fn list_adapters() -> Vec<AdapterReport> {
    instance()
        .enumerate_adapters(backends())
        .iter()
        .enumerate()
        .map(|(index, adapter)| report(index, &adapter.get_info()))
        .collect()
}

fn type_rank(device_type: wgpu::DeviceType) -> u8 {
    match device_type {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::Cpu => 4,
    }
}

/// The adapter a selector names: an index, or a case-insensitive substring of
/// the adapter name. Without one, the best hardware GPU, software last.
fn pick(infos: &[wgpu::AdapterInfo], selector: Option<&str>) -> Result<usize, GpuModernError> {
    if infos.is_empty() {
        return Err(GpuModernError::NoAdapter(
            "wgpu found no Vulkan, Metal or DX12 adapter".into(),
        ));
    }
    if let Some(selector) = selector.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(index) = selector.parse::<usize>() {
            return if index < infos.len() {
                Ok(index)
            } else {
                Err(GpuModernError::NoAdapter(format!(
                    "adapter index {index} requested, {} available",
                    infos.len()
                )))
            };
        }
        let needle = selector.to_lowercase();
        return infos
            .iter()
            .position(|info| info.name.to_lowercase().contains(&needle))
            .ok_or_else(|| {
                GpuModernError::NoAdapter(format!("no adapter name contains {selector:?}"))
            });
    }
    Ok((0..infos.len())
        .min_by_key(|&i| {
            (
                type_rank(infos[i].device_type),
                u8::from(is_software(&infos[i])),
            )
        })
        .unwrap_or(0))
}

/// Limits the kernels rely on (all within the WebGPU defaults).
fn check_limits(limits: &wgpu::Limits) -> Result<(), GpuModernError> {
    let needed = [
        (
            "max_storage_buffers_per_shader_stage",
            limits.max_storage_buffers_per_shader_stage,
            8,
        ),
        (
            "max_uniform_buffers_per_shader_stage",
            limits.max_uniform_buffers_per_shader_stage,
            2,
        ),
        (
            "max_compute_invocations_per_workgroup",
            limits.max_compute_invocations_per_workgroup,
            256,
        ),
        (
            "max_compute_workgroup_size_x",
            limits.max_compute_workgroup_size_x,
            256,
        ),
        (
            "max_compute_workgroup_storage_size",
            limits.max_compute_workgroup_storage_size,
            8192,
        ),
        (
            "max_compute_workgroups_per_dimension",
            limits.max_compute_workgroups_per_dimension,
            1024,
        ),
    ];
    for (name, have, want) in needed {
        if have < want {
            return Err(GpuModernError::Unsupported(format!(
                "adapter limit {name} = {have}; the kernels need {want}"
            )));
        }
    }
    Ok(())
}

/// A device and queue with everything a run reports about them.
pub(crate) struct GpuContext {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) report: AdapterReport,
    pub(crate) limits: wgpu::Limits,
    /// Whether the WGSL compiler implements `packed_4x8_integer_dot_product`.
    pub(crate) native_dot: bool,
    errors: Arc<Mutex<Vec<String>>>,
}

impl GpuContext {
    pub(crate) fn new(selector: Option<&str>) -> Result<Self, GpuModernError> {
        let instance = instance();
        let adapters = instance.enumerate_adapters(backends());
        let infos: Vec<wgpu::AdapterInfo> = adapters.iter().map(wgpu::Adapter::get_info).collect();
        let index = pick(&infos, selector)?;
        let adapter = &adapters[index];
        let report = report(index, &infos[index]);
        let limits = adapter.limits();
        check_limits(&limits)?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("arc-modern-gpu"),
            required_features: wgpu::Features::empty(),
            required_limits: limits.clone(),
            ..Default::default()
        }))
        .map_err(|e| GpuModernError::Device(format!("{}: {e}", report.name)))?;
        // Record asynchronous errors instead of letting wgpu panic; every
        // forward pass checks them before trusting its read-back.
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&errors);
        device.on_uncaptured_error(Box::new(move |error| {
            if let Ok(mut list) = sink.lock() {
                list.push(error.to_string());
            }
        }));
        let native_dot = std::env::var("ARC_GPU_DOT").as_deref() != Ok("fallback")
            && instance
                .wgsl_language_features()
                .contains(wgpu::WgslLanguageFeatures::Packed4x8IntegerDotProduct);
        Ok(Self {
            device,
            queue,
            report,
            limits,
            native_dot,
            errors,
        })
    }

    /// Fail if wgpu reported an asynchronous error since the last check.
    pub(crate) fn check_errors(&self) -> Result<(), GpuModernError> {
        let mut list = self
            .errors
            .lock()
            .map_err(|_| GpuModernError::Execution("error sink poisoned".into()))?;
        if list.is_empty() {
            return Ok(());
        }
        let joined = list.join("; ");
        list.clear();
        Err(GpuModernError::Execution(joined))
    }

    /// Largest buffer one binding may cover on this device.
    pub(crate) fn max_binding_bytes(&self) -> u64 {
        u64::from(self.limits.max_storage_buffer_binding_size).min(self.limits.max_buffer_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, device_type: wgpu::DeviceType) -> wgpu::AdapterInfo {
        wgpu::AdapterInfo {
            name: name.into(),
            vendor: 0,
            device: 0,
            device_type,
            driver: String::new(),
            driver_info: String::new(),
            backend: wgpu::Backend::Vulkan,
        }
    }

    #[test]
    fn selection_prefers_hardware_and_honours_selectors() {
        let infos = [
            info("llvmpipe (LLVM 19.1.1, 256 bits)", wgpu::DeviceType::Cpu),
            info("Intel(R) UHD Graphics", wgpu::DeviceType::IntegratedGpu),
            info("NVIDIA GeForce RTX 4090", wgpu::DeviceType::DiscreteGpu),
        ];
        assert_eq!(pick(&infos, None).unwrap(), 2);
        assert_eq!(pick(&infos, Some("0")).unwrap(), 0);
        assert_eq!(pick(&infos, Some("LLVMPIPE")).unwrap(), 0);
        assert_eq!(pick(&infos, Some(" uhd ")).unwrap(), 1);
        assert!(pick(&infos, Some("7")).is_err());
        assert!(pick(&infos, Some("radeon")).is_err());
        assert!(pick(&[], None).is_err());
        assert!(is_software(&infos[0]));
        assert!(!is_software(&infos[2]));
        assert!(is_software(&info(
            "Microsoft Basic Render Driver",
            wgpu::DeviceType::IntegratedGpu
        )));
    }
}
