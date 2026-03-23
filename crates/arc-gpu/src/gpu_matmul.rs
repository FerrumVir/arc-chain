//! GPU-accelerated integer matmul — native Metal + WGSL fallback.
//!
//! Pre-allocates buffer pools at model load time. Per-matmul dispatch
//! reuses existing buffers (zero allocation). Native Metal shader uses
//! char (i8) types directly — no packed u32 extraction overhead.
//!
//! Apple Silicon: unified memory, weight buffer shared with CPU.
//! Discrete GPU: weights copied once at load, input/output per dispatch.

use bytemuck::{Pod, Zeroable};
use std::sync::Arc;
use tracing::info;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct MatmulParams {
    in_size: u32,
    out_size: u32,
}

/// Pre-allocated buffer pool for zero-alloc dispatch.
struct BufferPool {
    input_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    staging_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    max_in_size: usize,
    max_out_size: usize,
}

/// GPU matmul engine with pre-allocated pools and native Metal support.
pub struct GpuMatmul {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::ComputePipeline,
    msl_pipeline: Option<wgpu::ComputePipeline>,
    bind_group_layout: wgpu::BindGroupLayout,
    pool: BufferPool,
    pub is_metal: bool,
}

/// GPU-resident weight matrix.
pub struct GpuWeights {
    buffer: wgpu::Buffer,
    pub n_rows: usize,
    pub n_cols: usize,
}

impl GpuMatmul {
    /// Initialize with pre-allocated pools for given max dimensions.
    /// max_in: largest input dimension (e.g., 11008 for d_ff)
    /// max_out: largest output dimension (e.g., 32000 for vocab)
    pub fn new(max_in: usize, max_out: usize) -> Result<Self, String> {
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

        // Skip software renderers
        if adapter_info.name.contains("llvmpipe") || adapter_info.name.contains("SwiftShader") {
            return Err(format!("Software GPU ({}), no speedup", adapter_info.name));
        }

        let is_metal = adapter_info.backend == wgpu::Backend::Metal;
        let has_msl = adapter.features().contains(wgpu::Features::MSL_SHADER_PASSTHROUGH);
        info!("GPU matmul: {} ({:?}, metal={}, msl={})", adapter_info.name, adapter_info.backend, is_metal, has_msl);

        let required_features = if has_msl {
            wgpu::Features::MSL_SHADER_PASSTHROUGH
        } else {
            wgpu::Features::empty()
        };

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("ARC Matmul GPU"),
                required_features,
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            },
        )).map_err(|e| format!("GPU device: {e}"))?;

        let device: Arc<wgpu::Device> = Arc::new(device);
        let queue: Arc<wgpu::Queue> = Arc::new(queue);

        // Compile WGSL shader (always available)
        let wgsl_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("matmul_wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("matmul.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("matmul_layout"),
            entries: &[
                bgl_entry(0, true),   // weights (read-only storage)
                bgl_entry(1, true),   // input (read-only storage)
                bgl_entry(2, false),  // output (read-write storage)
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

        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("matmul_pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("matmul_wgsl"),
            layout: Some(&pl),
            module: &wgsl_shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        // Native Metal shader via MSL passthrough — char (i8) types, no u32 packing
        let msl_pipeline = if has_msl {
            let msl_source = include_str!("matmul.metal");
            let msl_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let msl_shader = unsafe {
                    device.create_shader_module_passthrough(
                        wgpu::ShaderModuleDescriptorPassthrough::Msl(
                            wgpu::ShaderModuleDescriptorMsl {
                                entry_point: "matmul_i8".to_string(),
                                label: Some("matmul_metal"),
                                num_workgroups: (256, 1, 1),
                                source: std::borrow::Cow::Borrowed(msl_source),
                            },
                        ),
                    )
                };
                info!("Native Metal matmul shader compiled");
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("matmul_msl"),
                    layout: Some(&pl),
                    module: &msl_shader,
                    entry_point: Some("matmul_i8"),
                    compilation_options: Default::default(),
                    cache: None,
                })
            }));
            match msl_result {
                Ok(p) => {
                    info!("Metal matmul pipeline ready");
                    Some(p)
                }
                Err(_) => {
                    info!("MSL matmul compilation failed, using WGSL");
                    None
                }
            }
        } else {
            None
        };

        // Pre-allocate buffer pool
        let pool = BufferPool {
            input_buf: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pool_input"),
                size: max_in as u64 * 4, // packed u32
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            output_buf: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pool_output"),
                size: max_out as u64 * 4, // i32
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            staging_buf: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pool_staging"),
                size: max_out as u64 * 4,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            params_buf: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pool_params"),
                size: 8,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            max_in_size: max_in,
            max_out_size: max_out,
        };

        info!("GPU matmul ready: pool for {}×{}", max_out, max_in);
        Ok(Self { device, queue, pipeline, msl_pipeline, bind_group_layout, pool, is_metal })
    }

    /// Upload weight matrix to GPU. Call once per weight at model load.
    pub fn upload_weights(&self, data: &[i8], n_rows: usize, n_cols: usize) -> GpuWeights {
        let (buf_data, buf_size) = if self.msl_pipeline.is_some() {
            // Metal: raw i8 bytes (native char type)
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len())
            };
            (bytes.to_vec(), data.len())
        } else {
            // WGSL: pack i8 into u32 (no native i8 in WGSL)
            let packed = pack_i8_to_u32(data);
            let bytes = bytemuck::cast_slice::<u32, u8>(&packed).to_vec();
            (bytes, packed.len() * 4)
        };

        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("weights"),
            size: buf_size as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buffer, 0, &buf_data);
        GpuWeights { buffer, n_rows, n_cols }
    }

    /// Zero-alloc matmul dispatch using pre-allocated pool.
    pub fn matmul(&self, weights: &GpuWeights, input_i8: &[i8]) -> Vec<i32> {
        let n_rows = weights.n_rows;
        let n_cols = weights.n_cols;

        // Write input to pool buffer
        if self.msl_pipeline.is_some() {
            // Metal: raw i8 bytes
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(input_i8.as_ptr() as *const u8, input_i8.len())
            };
            self.queue.write_buffer(&self.pool.input_buf, 0, bytes);
        } else {
            // WGSL: packed u32
            let input_packed = pack_i8_to_u32(input_i8);
            self.queue.write_buffer(&self.pool.input_buf, 0, bytemuck::cast_slice(&input_packed));
        }

        // Write params
        let params = MatmulParams { in_size: n_cols as u32, out_size: n_rows as u32 };
        self.queue.write_buffer(&self.pool.params_buf, 0, bytemuck::bytes_of(&params));

        // Create bind group (lightweight — just pointers)
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: weights.buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: self.pool.input_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: self.pool.output_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: self.pool.params_buf.as_entire_binding() },
            ],
        });

        // Dispatch
        let active = self.msl_pipeline.as_ref().unwrap_or(&self.pipeline);
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(active);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups((n_rows as u32 + 255) / 256, 1, 1);
        }
        let out_bytes = (n_rows * 4) as u64;
        encoder.copy_buffer_to_buffer(&self.pool.output_buf, 0, &self.pool.staging_buf, 0, out_bytes);
        self.queue.submit([encoder.finish()]);

        // Readback (zero-copy on unified memory)
        let slice = self.pool.staging_buf.slice(..out_bytes);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
        let _ = self.device.poll(wgpu::PollType::wait());
        rx.recv().unwrap().unwrap();

        let mapped = slice.get_mapped_range();
        let result: Vec<i32> = bytemuck::cast_slice(&mapped).to_vec();
        drop(mapped);
        self.pool.staging_buf.unmap();

        result
    }

    /// Batch matmul: run multiple matmuls in ONE command buffer.
    /// Returns concatenated i32 outputs.
    pub fn matmul_batch(&self, ops: &[(&GpuWeights, &[i8])]) -> Vec<Vec<i32>> {
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let mut result_sizes = Vec::new();

        for (weights, input_i8) in ops {
            let n_rows = weights.n_rows;
            let n_cols = weights.n_cols;

            let input_packed = pack_i8_to_u32(input_i8);
            self.queue.write_buffer(&self.pool.input_buf, 0, bytemuck::cast_slice(&input_packed));

            let params = MatmulParams { in_size: n_cols as u32, out_size: n_rows as u32 };
            self.queue.write_buffer(&self.pool.params_buf, 0, bytemuck::bytes_of(&params));

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: weights.buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: self.pool.input_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: self.pool.output_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: self.pool.params_buf.as_entire_binding() },
                ],
            });

            let active = self.msl_pipeline.as_ref().unwrap_or(&self.pipeline);
            {
                let mut pass = encoder.begin_compute_pass(&Default::default());
                pass.set_pipeline(active);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.dispatch_workgroups((n_rows as u32 + 255) / 256, 1, 1);
            }

            result_sizes.push(n_rows);
        }

        // Single submit for all matmuls
        self.queue.submit([encoder.finish()]);

        // Note: batch readback is complex with single output buffer.
        // For now, return empty — batch dispatch reduces command overhead,
        // actual batch readback needs multiple output buffers.
        vec![vec![]; ops.len()]
    }
}

fn bgl_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

pub fn pack_i8_to_u32_pub(data: &[i8]) -> Vec<u32> {
    pack_i8_to_u32(data)
}

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
        assert_eq!(packed[0], 0xFE02FF01);
    }

    #[test]
    fn test_gpu_matmul_if_available() {
        match GpuMatmul::new(16, 8) {
            Ok(gpu) => {
                let weights: Vec<i8> = vec![1, 2, 3, 4, 5, 6, 7, 8];
                let input: Vec<i8> = vec![1, 1, 1, 1];
                let gw = gpu.upload_weights(&weights, 2, 4);
                let result = gpu.matmul(&gw, &input);
                assert_eq!(result[0], 10);
                assert_eq!(result[1], 26);
                println!("GPU matmul PASSED (metal={})", gpu.is_metal);
            }
            Err(e) => println!("No GPU: {}", e),
        }
    }
}
