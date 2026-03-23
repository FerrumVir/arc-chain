//! GPU-accelerated integer matmul via wgpu compute shaders.
//!
//! Uploads INT8 weights to GPU at model load time. Per-token: upload i8 input,
//! dispatch matmul, read back i32 accumulators. Per-row scale applied on CPU.
//!
//! On Apple Silicon: unified memory, zero-copy weight access.
//! On discrete GPU: weights copied once at load, input/output per dispatch.

use bytemuck::{Pod, Zeroable};
use std::sync::Arc;
use tracing::info;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct MatmulParams {
    in_size: u32,
    out_size: u32,
}

/// GPU matmul engine — holds compiled pipeline and weight buffers.
pub struct GpuMatmul {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
}

/// GPU-resident weight matrix (uploaded once at model load).
pub struct GpuWeights {
    buffer: wgpu::Buffer,
    pub n_rows: usize,
    pub n_cols: usize,
}

impl GpuMatmul {
    /// Initialize GPU matmul engine. Call once at startup.
    pub fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        })).map_err(|_| "No GPU adapter found".to_string())?;

        let adapter_info = adapter.get_info();
        info!("GPU matmul: {} ({:?})", adapter_info.name, adapter_info.backend);

        // Skip software renderers
        if adapter_info.name.contains("llvmpipe") || adapter_info.name.contains("SwiftShader") {
            return Err("Software GPU — no speedup, using CPU".into());
        }

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("ARC Matmul GPU"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            },
        )).map_err(|e| format!("GPU device error: {e}"))?;

        let device: Arc<wgpu::Device> = Arc::new(device);
        let queue: Arc<wgpu::Queue> = Arc::new(queue);

        // Compile shader
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("matmul_i8"),
            source: wgpu::ShaderSource::Wgsl(include_str!("matmul.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("matmul_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("matmul_pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("matmul_pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        info!("GPU matmul pipeline compiled");
        Ok(Self { device, queue, pipeline, bind_group_layout })
    }

    /// Upload weight matrix to GPU. Call once per weight at model load.
    /// Packs i8 data into u32 arrays for WGSL compatibility.
    pub fn upload_weights(&self, data: &[i8], n_rows: usize, n_cols: usize) -> GpuWeights {
        // Pack i8 into u32 (4 values per u32)
        let packed = pack_i8_to_u32(data);

        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("weights"),
            size: (packed.len() * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buffer, 0, bytemuck::cast_slice(&packed));

        GpuWeights { buffer, n_rows, n_cols }
    }

    /// Run matmul on GPU: output[i] = sum_j(weights[i][j] * input[j]).
    /// Returns raw i32 accumulators (caller applies per-row scale).
    pub fn matmul(&self, weights: &GpuWeights, input_i8: &[i8]) -> Vec<i32> {
        let n_rows = weights.n_rows;
        let n_cols = weights.n_cols;

        // Pack input
        let input_packed = pack_i8_to_u32(input_i8);

        // Create input buffer
        let input_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("input"),
            size: (input_packed.len() * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&input_buf, 0, bytemuck::cast_slice(&input_packed));

        // Create output buffer
        let output_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("output"),
            size: (n_rows * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        // Staging buffer for readback
        let staging_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging"),
            size: (n_rows * 4) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Params
        let params = MatmulParams {
            in_size: n_cols as u32,
            out_size: n_rows as u32,
        };
        let params_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: 8,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&params_buf, 0, bytemuck::bytes_of(&params));

        // Bind group
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("matmul_bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: weights.buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: input_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: output_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: params_buf.as_entire_binding() },
            ],
        });

        // Dispatch
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("matmul_enc"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("matmul_pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            let workgroups = (n_rows as u32 + 255) / 256;
            pass.dispatch_workgroups(workgroups, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&output_buf, 0, &staging_buf, 0, (n_rows * 4) as u64);
        self.queue.submit([encoder.finish()]);

        // Readback
        let slice = staging_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
        let _ = self.device.poll(wgpu::PollType::wait());
        rx.recv().unwrap().unwrap();

        let mapped = slice.get_mapped_range();
        let result: Vec<i32> = bytemuck::cast_slice(&mapped).to_vec();
        drop(mapped);
        staging_buf.unmap();

        result
    }
}

/// Pack i8 slice into u32 array (4 i8 per u32, little-endian).
fn pack_i8_to_u32(data: &[i8]) -> Vec<u32> {
    let padded_len = (data.len() + 3) / 4 * 4;
    let mut packed = Vec::with_capacity(padded_len / 4);
    for chunk in data.chunks(4) {
        let mut val = 0u32;
        for (k, &byte) in chunk.iter().enumerate() {
            val |= ((byte as u8) as u32) << (k * 8);
        }
        packed.push(val);
    }
    packed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pack_i8() {
        let data: Vec<i8> = vec![1, -1, 2, -2];
        let packed = pack_i8_to_u32(&data);
        assert_eq!(packed.len(), 1);
        // 1, 255(-1), 2, 254(-2) in little-endian
        assert_eq!(packed[0], 0xFE02FF01);
    }

    #[test]
    fn test_gpu_matmul_if_available() {
        match GpuMatmul::new() {
            Ok(gpu) => {
                // Simple 2x4 matrix × 4-element vector
                let weights: Vec<i8> = vec![1, 2, 3, 4, 5, 6, 7, 8];
                let input: Vec<i8> = vec![1, 1, 1, 1];
                let gw = gpu.upload_weights(&weights, 2, 4);
                let result = gpu.matmul(&gw, &input);
                // Row 0: 1+2+3+4=10, Row 1: 5+6+7+8=26
                assert_eq!(result[0], 10);
                assert_eq!(result[1], 26);
                println!("GPU matmul test PASSED");
            }
            Err(e) => {
                println!("No GPU available: {}, skipping test", e);
            }
        }
    }
}
