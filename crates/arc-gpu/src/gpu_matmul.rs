use bytemuck::{Pod, Zeroable};
use std::sync::Arc;
use tracing::info;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct MatmulParams {
    in_size: u32,
    out_size: u32,
    scale_offset: u32,
    _pad: u32,
}

struct BufferPool {
    input_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    staging_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    #[allow(dead_code)]
    scales_buf: wgpu::Buffer,
    #[allow(dead_code)]
    max_in_size: usize,
    #[allow(dead_code)]
    max_out_size: usize,
}

pub struct GpuMatmul {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::ComputePipeline,
    msl_pipeline: Option<wgpu::ComputePipeline>,
    msl_q4_pipeline: Option<wgpu::ComputePipeline>,
    bind_group_layout: wgpu::BindGroupLayout,
    pool: BufferPool,
    pub is_metal: bool,
    pub has_msl: bool,
}

pub struct GpuWeights {
    buffer: wgpu::Buffer,
    scales_buffer: wgpu::Buffer,
    pub n_rows: usize,
    pub n_cols: usize,
    pub is_q4: bool,
}

impl GpuMatmul {
    pub fn new_wgsl_only(max_in: usize, max_out: usize) -> Result<Self, String> {
        Self::new_inner(max_in, max_out, true)
    }

    pub fn new(max_in: usize, max_out: usize) -> Result<Self, String> {
        Self::new_inner(max_in, max_out, false)
    }

    fn new_inner(max_in: usize, max_out: usize, force_wgsl: bool) -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .map_err(|_| "No GPU adapter found".to_string())?;

        let adapter_info = adapter.get_info();

        if adapter_info.name.contains("llvmpipe") || adapter_info.name.contains("SwiftShader") {
            return Err(format!("Software GPU ({}), no speedup", adapter_info.name));
        }

        let is_metal = adapter_info.backend == wgpu::Backend::Metal;
        let has_msl = adapter
            .features()
            .contains(wgpu::Features::MSL_SHADER_PASSTHROUGH);
        info!(
            "GPU matmul: {} ({:?}, metal={}, msl={})",
            adapter_info.name, adapter_info.backend, is_metal, has_msl
        );

        let required_features = if has_msl {
            wgpu::Features::MSL_SHADER_PASSTHROUGH
        } else {
            wgpu::Features::empty()
        };

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ARC Matmul GPU"),
            required_features,
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .map_err(|e| format!("GPU device: {e}"))?;

        let device: Arc<wgpu::Device> = Arc::new(device);
        let queue: Arc<wgpu::Queue> = Arc::new(queue);

        let wgsl_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("matmul_wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("matmul.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("matmul_layout"),
            entries: &[
                bgl_entry(0, true),
                bgl_entry(1, true),
                bgl_entry(2, false),
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
                bgl_entry(4, true),
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

        let msl_pipeline = if has_msl && !force_wgsl {
            let msl_source = include_str!("matmul.metal");
            eprintln!("[GPU] MSL passthrough DETECTED, compiling matmul_i8...");
            let msl_shader = unsafe {
                device.create_shader_module_passthrough(
                    wgpu::ShaderModuleDescriptorPassthrough::Msl(wgpu::ShaderModuleDescriptorMsl {
                        entry_point: "matmul_i8".to_string(),
                        label: Some("matmul_metal"),
                        num_workgroups: (128, 1, 1),
                        source: std::borrow::Cow::Borrowed(msl_source),
                    }),
                )
            };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("matmul_msl"),
                    layout: Some(&pl),
                    module: &msl_shader,
                    entry_point: Some("matmul_i8"),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })) {
                Ok(p) => {
                    info!("Native Metal matmul pipeline READY (char4, simd_sum, no u32 packing)");
                    Some(p)
                }
                Err(e) => {
                    eprintln!(
                        "[GPU] MSL pipeline creation failed, using WGSL fallback: {:?}",
                        e
                    );
                    None
                }
            }
        } else {
            info!("MSL passthrough not available, using WGSL");
            None
        };

        let msl_q4_pipeline = if has_msl && !force_wgsl {
            let fused_src = include_str!("fused_kernels.metal");
            let q4_shader = unsafe {
                device.create_shader_module_passthrough(
                    wgpu::ShaderModuleDescriptorPassthrough::Msl(wgpu::ShaderModuleDescriptorMsl {
                        entry_point: "matmul_i4".to_string(),
                        label: Some("matmul_q4_metal"),
                        num_workgroups: (128, 1, 1),
                        source: std::borrow::Cow::Borrowed(fused_src),
                    }),
                )
            };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("matmul_q4_msl"),
                    layout: Some(&pl),
                    module: &q4_shader,
                    entry_point: Some("matmul_i4"),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })) {
                Ok(p) => {
                    info!("Native Metal Q4 matmul pipeline READY");
                    Some(p)
                }
                Err(_) => {
                    info!("MSL Q4 matmul failed");
                    None
                }
            }
        } else {
            None
        };

        let pool = BufferPool {
            input_buf: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pool_input"),
                size: max_in as u64 * 4,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            output_buf: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pool_output"),
                size: max_out as u64 * 4,
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
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            scales_buf: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pool_scales"),
                size: max_out as u64 * 4,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            max_in_size: max_in,
            max_out_size: max_out,
        };

        info!(
            "GPU matmul ready: pool for {}×{}, MSL={}, Q4={}",
            max_out,
            max_in,
            msl_pipeline.is_some(),
            msl_q4_pipeline.is_some()
        );
        Ok(Self {
            device,
            queue,
            pipeline,
            msl_pipeline,
            msl_q4_pipeline,
            bind_group_layout,
            pool,
            is_metal,
            has_msl,
        })
    }

    pub fn upload_weights(
        &self,
        data: &[i8],
        n_rows: usize,
        n_cols: usize,
        scales: Option<&[i32]>,
    ) -> GpuWeights {
        let (buf_data, buf_size) = if self.msl_pipeline.is_some() {
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len()) };
            (bytes.to_vec(), data.len())
        } else {
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

        let default_scales: Vec<i32> = vec![256; n_rows];
        let scale_data = scales.unwrap_or(&default_scales);
        let scales_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("weight_scales"),
            size: (scale_data.len() * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue
            .write_buffer(&scales_buffer, 0, bytemuck::cast_slice(scale_data));

        GpuWeights {
            buffer,
            scales_buffer,
            n_rows,
            n_cols,
            is_q4: false,
        }
    }

    pub fn upload_weights_q4(
        &self,
        data: &[i8],
        n_rows: usize,
        n_cols: usize,
        scales: Option<&[i32]>,
    ) -> GpuWeights {
        let q4_data = pack_i8_to_q4(data);

        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("weights_q4"),
            size: q4_data.len() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buffer, 0, &q4_data);

        let default_scales: Vec<i32> = vec![256; n_rows];
        let scale_data = scales.unwrap_or(&default_scales);
        let scales_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("weight_scales_q4"),
            size: (scale_data.len() * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue
            .write_buffer(&scales_buffer, 0, bytemuck::cast_slice(scale_data));

        GpuWeights {
            buffer,
            scales_buffer,
            n_rows,
            n_cols,
            is_q4: true,
        }
    }

    pub fn matmul(&self, weights: &GpuWeights, input_i8: &[i8]) -> Vec<i32> {
        let n_rows = weights.n_rows;
        let n_cols = weights.n_cols;

        if self.msl_pipeline.is_some() {
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(input_i8.as_ptr() as *const u8, input_i8.len())
            };
            self.queue.write_buffer(&self.pool.input_buf, 0, bytes);
        } else {
            let input_packed = pack_i8_to_u32(input_i8);
            self.queue
                .write_buffer(&self.pool.input_buf, 0, bytemuck::cast_slice(&input_packed));
        }

        let params = MatmulParams {
            in_size: n_cols as u32,
            out_size: n_rows as u32,
            scale_offset: 0,
            _pad: 0,
        };
        self.queue
            .write_buffer(&self.pool.params_buf, 0, bytemuck::bytes_of(&params));

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: weights.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.pool.input_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.pool.output_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.pool.params_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: weights.scales_buffer.as_entire_binding(),
                },
            ],
        });

        let active = if weights.is_q4 {
            self.msl_q4_pipeline.as_ref().unwrap_or(&self.pipeline)
        } else {
            self.msl_pipeline.as_ref().unwrap_or(&self.pipeline)
        };
        let use_metal_dispatch = weights.is_q4 && self.msl_q4_pipeline.is_some()
            || !weights.is_q4 && self.msl_pipeline.is_some();
        let wg_count = if use_metal_dispatch {
            (n_rows as u32).div_ceil(4)
        } else {
            (n_rows as u32).div_ceil(256)
        };

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(active);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(wg_count, 1, 1);
        }
        let out_bytes = (n_rows * 4) as u64;
        encoder.copy_buffer_to_buffer(
            &self.pool.output_buf,
            0,
            &self.pool.staging_buf,
            0,
            out_bytes,
        );
        self.queue.submit([encoder.finish()]);

        let slice = self.pool.staging_buf.slice(..out_bytes);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait());
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("GPU map error: {:?}", e);
                return vec![];
            }
            Err(_) => {
                eprintln!("GPU map channel closed");
                return vec![];
            }
        }

        let mapped = slice.get_mapped_range();
        let result: Vec<i32> = bytemuck::cast_slice(&mapped).to_vec();
        drop(mapped);
        self.pool.staging_buf.unmap();

        result
    }

    pub fn matmul_batch(&self, ops: &[(&GpuWeights, &[i8])]) -> Vec<Vec<i32>> {
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let mut result_sizes = Vec::new();

        for (weights, input_i8) in ops {
            let n_rows = weights.n_rows;
            let n_cols = weights.n_cols;

            if self.msl_pipeline.is_some() {
                let bytes: &[u8] = unsafe {
                    std::slice::from_raw_parts(input_i8.as_ptr() as *const u8, input_i8.len())
                };
                self.queue.write_buffer(&self.pool.input_buf, 0, bytes);
            } else {
                let input_packed = pack_i8_to_u32(input_i8);
                self.queue.write_buffer(
                    &self.pool.input_buf,
                    0,
                    bytemuck::cast_slice(&input_packed),
                );
            }

            let params = MatmulParams {
                in_size: n_cols as u32,
                out_size: n_rows as u32,
                scale_offset: 0,
                _pad: 0,
            };
            self.queue
                .write_buffer(&self.pool.params_buf, 0, bytemuck::bytes_of(&params));

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: weights.buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.pool.input_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.pool.output_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: self.pool.params_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: weights.scales_buffer.as_entire_binding(),
                    },
                ],
            });

            let active = self.msl_pipeline.as_ref().unwrap_or(&self.pipeline);
            let wg_count = if self.msl_pipeline.is_some() {
                (n_rows as u32).div_ceil(4)
            } else {
                (n_rows as u32).div_ceil(256)
            };
            {
                let mut pass = encoder.begin_compute_pass(&Default::default());
                pass.set_pipeline(active);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.dispatch_workgroups(wg_count, 1, 1);
            }

            result_sizes.push(n_rows);
        }

        self.queue.submit([encoder.finish()]);
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

pub fn pack_i8_to_q4(data: &[i8]) -> Vec<u8> {
    let mut packed = Vec::with_capacity(data.len().div_ceil(2));
    for pair in data.chunks(2) {
        let lo = ((pair[0].clamp(-8, 7) + 8) as u8) & 0x0F;
        let hi = if pair.len() > 1 {
            ((pair[1].clamp(-8, 7) + 8) as u8) & 0x0F
        } else {
            8
        };
        packed.push(lo | (hi << 4));
    }
    packed
}

fn pack_i8_to_u32(data: &[i8]) -> Vec<u32> {
    let padded_len = data.len().div_ceil(4) * 4;
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
                let gw = gpu.upload_weights(&weights, 2, 4, None);
                let result = gpu.matmul(&gw, &input);
                assert_eq!(result[0], 10);
                assert_eq!(result[1], 26);
                println!(
                    "GPU matmul PASSED (metal={}, msl={})",
                    gpu.is_metal, gpu.has_msl
                );
            }
            Err(e) => println!("No GPU: {}", e),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_msl_buffer_mapping() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .unwrap();

        let has_msl = adapter
            .features()
            .contains(wgpu::Features::MSL_SHADER_PASSTHROUGH);
        if !has_msl {
            println!("SKIP: no MSL passthrough");
            return;
        }

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("diag"),
            required_features: wgpu::Features::MSL_SHADER_PASSTHROUGH,
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .unwrap();

        let diag_src = include_str!("matmul_diag.metal");

        {
            let shader = unsafe {
                device.create_shader_module_passthrough(
                    wgpu::ShaderModuleDescriptorPassthrough::Msl(wgpu::ShaderModuleDescriptorMsl {
                        entry_point: "diag_write".to_string(),
                        label: Some("diag_write"),
                        num_workgroups: (1, 1, 1),
                        source: std::borrow::Cow::Borrowed(diag_src),
                    }),
                )
            };
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[bgl_entry(0, false)],
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[&bgl],
                push_constant_ranges: &[],
            });
            let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: None,
                layout: Some(&pl),
                module: &shader,
                entry_point: Some("diag_write"),
                compilation_options: Default::default(),
                cache: None,
            });

            let out_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: 8,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let staging = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: 8,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: out_buf.as_entire_binding(),
                }],
            });

            let mut enc = device.create_command_encoder(&Default::default());
            {
                let mut p = enc.begin_compute_pass(&Default::default());
                p.set_pipeline(&pipeline);
                p.set_bind_group(0, &bg, &[]);
                p.dispatch_workgroups(1, 1, 1);
            }
            enc.copy_buffer_to_buffer(&out_buf, 0, &staging, 0, 8);
            queue.submit([enc.finish()]);

            let slice = staging.slice(..8);
            let (tx, rx) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
            let _ = device.poll(wgpu::PollType::wait());
            match rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    eprintln!("GPU map error: {:?}", e);
                    return;
                }
                Err(_) => {
                    eprintln!("GPU map channel closed");
                    return;
                }
            }
            let data: Vec<i32> = bytemuck::cast_slice(&slice.get_mapped_range()).to_vec();
            println!(
                "DIAG1 diag_write: output[0]={}, output[1]={} (expect 12345, 67890)",
                data[0], data[1]
            );
            assert_eq!(data[0], 12345, "diag_write: buffer(0) mapping failed");
            assert_eq!(data[1], 67890, "diag_write: buffer(0) mapping failed");
        }

        {
            let shader = unsafe {
                device.create_shader_module_passthrough(
                    wgpu::ShaderModuleDescriptorPassthrough::Msl(wgpu::ShaderModuleDescriptorMsl {
                        entry_point: "diag_5buf".to_string(),
                        label: Some("diag_5buf"),
                        num_workgroups: (1, 1, 1),
                        source: std::borrow::Cow::Borrowed(diag_src),
                    }),
                )
            };

            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[
                    bgl_entry(0, true),
                    bgl_entry(1, true),
                    bgl_entry(2, false),
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
                    bgl_entry(4, true),
                ],
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[&bgl],
                push_constant_ranges: &[],
            });
            let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: None,
                layout: Some(&pl),
                module: &shader,
                entry_point: Some("diag_5buf"),
                compilation_options: Default::default(),
                cache: None,
            });

            let mk_buf = |data: &[i32], usage: wgpu::BufferUsages| -> wgpu::Buffer {
                let buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: (data.len() * 4).max(16) as u64,
                    usage: usage | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                });
                queue.write_buffer(&buf, 0, bytemuck::cast_slice(data));
                buf
            };

            let buf0 = mk_buf(&[111], wgpu::BufferUsages::STORAGE);
            let buf1 = mk_buf(&[222], wgpu::BufferUsages::STORAGE);
            let buf2 = mk_buf(&[0; 5], wgpu::BufferUsages::STORAGE);
            let buf3 = mk_buf(&[333, 0, 0, 0], wgpu::BufferUsages::UNIFORM);
            let buf4 = mk_buf(&[444], wgpu::BufferUsages::STORAGE);

            let staging = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: 20,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buf0.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: buf1.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: buf2.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: buf3.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: buf4.as_entire_binding(),
                    },
                ],
            });

            let mut enc = device.create_command_encoder(&Default::default());
            {
                let mut p = enc.begin_compute_pass(&Default::default());
                p.set_pipeline(&pipeline);
                p.set_bind_group(0, &bg, &[]);
                p.dispatch_workgroups(1, 1, 1);
            }
            enc.copy_buffer_to_buffer(&buf2, 0, &staging, 0, 20);
            queue.submit([enc.finish()]);

            let slice = staging.slice(..20);
            let (tx, rx) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
            let _ = device.poll(wgpu::PollType::wait());
            match rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    eprintln!("GPU map error: {:?}", e);
                    return;
                }
                Err(_) => {
                    eprintln!("GPU map channel closed");
                    return;
                }
            }
            let data: Vec<i32> = bytemuck::cast_slice(&slice.get_mapped_range()).to_vec();
            println!(
                "DIAG2 diag_5buf: [{}, {}, {}, {}, {}]",
                data[0], data[1], data[2], data[3], data[4]
            );
            println!(
                "  Expected: [111 (buf0), 222 (buf1), 333 (buf3/params), 444 (buf4/scales), 77777 (sentinel)]"
            );

            if data[4] != 77777 {
                println!("  SENTINEL MISMATCH: output buffer(2) may not map to binding(2)!");
                for (i, &v) in data[..5].iter().enumerate() {
                    if v == 77777 {
                        println!("  Found sentinel at output[{}]", i);
                    }
                    if v == 111 {
                        println!("  Found buf0 marker at output[{}]", i);
                    }
                    if v == 222 {
                        println!("  Found buf1 marker at output[{}]", i);
                    }
                    if v == 333 {
                        println!("  Found buf3 marker at output[{}]", i);
                    }
                    if v == 444 {
                        println!("  Found buf4 marker at output[{}]", i);
                    }
                }
            }
        }
    }

    #[test]
    fn test_gpu_matmul_with_scales() {
        match GpuMatmul::new(16, 8) {
            Ok(gpu) => {
                let weights: Vec<i8> = vec![1, 2, 3, 4, 5, 6, 7, 8];
                let input: Vec<i8> = vec![1, 1, 1, 1];
                let scales: Vec<i32> = vec![512, 768];
                let gw = gpu.upload_weights(&weights, 2, 4, Some(&scales));
                let result = gpu.matmul(&gw, &input);
                assert_eq!(result[0], 20);
                assert_eq!(result[1], 78);
                println!("GPU matmul with scales PASSED");
            }
            Err(e) => println!("No GPU: {}", e),
        }
    }

    #[test]
    fn test_q4_pack() {
        let data: Vec<i8> = vec![1, -2, 3, -4, 7, -8];
        let packed = pack_i8_to_q4(&data);
        assert_eq!(packed.len(), 3);
        assert_eq!(packed[0], 0x09 | (0x06 << 4));
        assert_eq!(packed[1], 0x0B | (0x04 << 4));
        assert_eq!(packed[2], 0x0F);
        for (i, &orig) in data.iter().enumerate() {
            let byte = packed[i / 2];
            let nibble = if i % 2 == 0 { byte & 0x0F } else { byte >> 4 };
            assert_eq!(nibble as i8 - 8, orig);
        }
    }

    #[test]
    fn test_gpu_matmul_q4() {
        match GpuMatmul::new(16, 8) {
            Ok(gpu) => {
                if gpu.msl_q4_pipeline.is_none() {
                    println!("SKIP: no Q4 Metal pipeline");
                    return;
                }
                let weights: Vec<i8> = vec![1, 2, 3, 4, 5, 6, 7, -8];
                let input: Vec<i8> = vec![1, 1, 1, 1];
                let gw = gpu.upload_weights_q4(&weights, 2, 4, None);
                let result = gpu.matmul(&gw, &input);
                println!(
                    "Q4 matmul result: [{}, {}] (expect [10, 10])",
                    result[0], result[1]
                );
                assert_eq!(result[0], 10);
                assert_eq!(result[1], 10);
                println!("GPU Q4 matmul PASSED");
            }
            Err(e) => println!("No GPU: {}", e),
        }
    }

    #[test]
    #[ignore = "hardware benchmark; run explicitly with --ignored --test-threads=1"]
    fn bench_matmul_4096() {
        let n = 4096usize;

        match GpuMatmul::new(n, n) {
            Ok(gpu) => {
                assert!(
                    gpu.msl_pipeline.is_some(),
                    "provisioned macOS GPU benchmark requires a compiled MSL pipeline"
                );
                let mut weights = vec![0i8; n * n];
                let mut input = vec![0i8; n];
                for (i, w) in weights.iter_mut().enumerate() {
                    *w = ((i % 5) as i8) - 2;
                }
                for (i, x) in input.iter_mut().enumerate() {
                    *x = ((i % 3) as i8) - 1;
                }

                let gw = gpu.upload_weights(&weights, n, n, None);

                let _ = gpu.matmul(&gw, &input);
                let _ = gpu.matmul(&gw, &input);

                let iters = 20;
                let start = std::time::Instant::now();
                for _ in 0..iters {
                    let _ = gpu.matmul(&gw, &input);
                }
                let elapsed = start.elapsed();
                let ms_per = elapsed.as_secs_f64() * 1000.0 / iters as f64;

                println!(
                    "GPU matmul {}×{}: {:.2} ms/call ({} iters, MSL={})",
                    n,
                    n,
                    ms_per,
                    iters,
                    gpu.msl_pipeline.is_some()
                );

                let result = gpu.matmul(&gw, &input);
                let nonzero = result.iter().filter(|&&x| x != 0).count();
                println!("  Non-zero outputs: {}/{}", nonzero, result.len());
                assert!(nonzero > 0, "All outputs zero - shader not computing");

                println!("\n  Forcing WGSL for comparison...");
                unsafe {
                    std::env::set_var("ARC_FORCE_WGSL_BENCH", "1");
                }
            }
            Err(e) => panic!("provisioned GPU benchmark could not create its device: {e}"),
        }

        if std::env::var("ARC_FORCE_WGSL_BENCH").is_ok() {
            let gpu_wgsl = GpuMatmul::new_wgsl_only(n, n).unwrap();
            let mut weights = vec![0i8; n * n];
            let mut input = vec![0i8; n];
            for (i, w) in weights.iter_mut().enumerate() {
                *w = ((i % 5) as i8) - 2;
            }
            for (i, x) in input.iter_mut().enumerate() {
                *x = ((i % 3) as i8) - 1;
            }
            let gw_wgsl = gpu_wgsl.upload_weights(&weights, n, n, None);
            let _ = gpu_wgsl.matmul(&gw_wgsl, &input);
            let _ = gpu_wgsl.matmul(&gw_wgsl, &input);
            let iters = 20;
            let start = std::time::Instant::now();
            for _ in 0..iters {
                let _ = gpu_wgsl.matmul(&gw_wgsl, &input);
            }
            let elapsed = start.elapsed();
            let ms_per = elapsed.as_secs_f64() * 1000.0 / iters as f64;
            println!(
                "  WGSL matmul {}×{}: {:.2} ms/call ({} iters)",
                n, n, ms_per, iters
            );

            let gpu_metal = GpuMatmul::new(n, n).unwrap();
            if gpu_metal.msl_pipeline.is_some() {
                let gw_metal = gpu_metal.upload_weights(&weights, n, n, None);
                let metal_out = gpu_metal.matmul(&gw_metal, &input);
                let wgsl_out = gpu_wgsl.matmul(&gw_wgsl, &input);
                let mut mismatches = 0;
                for i in 0..n {
                    if metal_out[i] != wgsl_out[i] {
                        mismatches += 1;
                    }
                }
                println!(
                    "  Metal vs WGSL correctness: {}/{} match ({} mismatches)",
                    n - mismatches,
                    n,
                    mismatches
                );
                assert_eq!(mismatches, 0, "Metal and WGSL produce different outputs!");
            }
            unsafe {
                std::env::remove_var("ARC_FORCE_WGSL_BENCH");
            }
        }
    }
}
