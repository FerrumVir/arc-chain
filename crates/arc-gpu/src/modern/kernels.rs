//! WGSL module assembly, compute pipelines and buffer helpers.

use std::borrow::Cow;

use wgpu::util::DeviceExt;

use super::GpuModernError;
use super::device::GpuContext;

const INT_LIB: &str = include_str!("wgsl/int.wgsl");
const EXP_LIB: &str = include_str!("wgsl/exp.wgsl");
const DOT_FALLBACK: &str = include_str!("wgsl/dot_fallback.wgsl");
const DOT_NATIVE: &str = include_str!("wgsl/dot_native.wgsl");

/// Workgroup width of the element-wise kernels (embed, rope, KV store, SiLU,
/// residual).
pub(crate) const ELEMENT_WG: u32 = 64;
/// Output rows per GEMV workgroup.
pub(crate) const GEMV_ROWS: u32 = 8;
/// Bytes of every parameter uniform (eight u32).
pub(crate) const PARAM_BYTES: usize = 32;
/// Bytes of the cursor uniform: pos0, count, two pads, 64 token ids.
pub(crate) const CURSOR_BYTES: usize = 16 + 4 * 64;

/// One compute kernel; each is its own WGSL module (library + kernel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kernel {
    Embed,
    RmsNorm,
    Split,
    Gemv,
    Rope,
    KvStore,
    Attention,
    Silu,
    Residual,
}

impl Kernel {
    const ALL: [Kernel; 9] = [
        Kernel::Embed,
        Kernel::RmsNorm,
        Kernel::Split,
        Kernel::Gemv,
        Kernel::Rope,
        Kernel::KvStore,
        Kernel::Attention,
        Kernel::Silu,
        Kernel::Residual,
    ];

    pub(crate) fn entry(self) -> &'static str {
        match self {
            Kernel::Embed => "embed",
            Kernel::RmsNorm => "rms_norm",
            Kernel::Split => "split",
            Kernel::Gemv => "gemv",
            Kernel::Rope => "rope",
            Kernel::KvStore => "kv_store",
            Kernel::Attention => "attention",
            Kernel::Silu => "gated_silu",
            Kernel::Residual => "residual",
        }
    }

    fn body(self) -> &'static str {
        match self {
            Kernel::Embed => include_str!("wgsl/embed.wgsl"),
            Kernel::RmsNorm => include_str!("wgsl/rms_norm.wgsl"),
            Kernel::Split => include_str!("wgsl/split.wgsl"),
            Kernel::Gemv => include_str!("wgsl/gemv.wgsl"),
            Kernel::Rope => include_str!("wgsl/rope.wgsl"),
            Kernel::KvStore => include_str!("wgsl/kv_store.wgsl"),
            Kernel::Attention => include_str!("wgsl/attention.wgsl"),
            Kernel::Silu => include_str!("wgsl/silu.wgsl"),
            Kernel::Residual => include_str!("wgsl/residual.wgsl"),
        }
    }

    fn needs_exp(self) -> bool {
        matches!(self, Kernel::Attention | Kernel::Silu)
    }

    fn needs_dot(self) -> bool {
        matches!(self, Kernel::Gemv)
    }
}

/// The complete WGSL source of one kernel module.
pub(crate) fn module_source(kernel: Kernel, native_dot: bool) -> String {
    let mut source = String::new();
    if native_dot && kernel.needs_dot() {
        source.push_str("requires packed_4x8_integer_dot_product;\n\n");
    }
    source.push_str(INT_LIB);
    if kernel.needs_exp() {
        source.push('\n');
        source.push_str(EXP_LIB);
    }
    if kernel.needs_dot() {
        source.push('\n');
        source.push_str(if native_dot { DOT_NATIVE } else { DOT_FALLBACK });
    }
    source.push('\n');
    source.push_str(kernel.body());
    source
}

/// Compiled pipelines for every kernel, with automatic bind-group layouts.
pub(crate) struct Pipelines {
    pipelines: Vec<wgpu::ComputePipeline>,
    /// Which exact i8 dot product the GEMV uses.
    pub(crate) dot_path: &'static str,
}

impl Pipelines {
    pub(crate) fn build(ctx: &GpuContext) -> Result<Self, GpuModernError> {
        if ctx.native_dot
            && let Ok(pipelines) = Self::build_with(ctx, true)
        {
            return Ok(pipelines);
        }
        Self::build_with(ctx, false)
    }

    fn build_with(ctx: &GpuContext, native_dot: bool) -> Result<Self, GpuModernError> {
        let mut pipelines = Vec::with_capacity(Kernel::ALL.len());
        for kernel in Kernel::ALL {
            ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let module = ctx
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(kernel.entry()),
                    source: wgpu::ShaderSource::Wgsl(Cow::Owned(module_source(kernel, native_dot))),
                });
            let pipeline = ctx
                .device
                .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(kernel.entry()),
                    layout: None,
                    module: &module,
                    entry_point: Some(kernel.entry()),
                    compilation_options: Default::default(),
                    cache: None,
                });
            if let Some(error) = pollster::block_on(ctx.device.pop_error_scope()) {
                return Err(GpuModernError::Device(format!(
                    "kernel {} failed to compile: {error}",
                    kernel.entry()
                )));
            }
            pipelines.push(pipeline);
        }
        Ok(Self {
            pipelines,
            dot_path: if native_dot {
                "dot4I8Packed"
            } else {
                "exact fallback (u32 sign extension + i32 dot)"
            },
        })
    }

    pub(crate) fn get(&self, kernel: Kernel) -> &wgpu::ComputePipeline {
        &self.pipelines[kernel as usize]
    }

    /// A bind group for `kernel` with whole buffers at the given bindings.
    pub(crate) fn bind(
        &self,
        ctx: &GpuContext,
        kernel: Kernel,
        entries: &[(u32, &wgpu::Buffer)],
    ) -> wgpu::BindGroup {
        let layout = self.get(kernel).get_bind_group_layout(0);
        let entries: Vec<wgpu::BindGroupEntry<'_>> = entries
            .iter()
            .map(|&(binding, buffer)| wgpu::BindGroupEntry {
                binding,
                resource: buffer.as_entire_binding(),
            })
            .collect();
        ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(kernel.entry()),
            layout: &layout,
            entries: &entries,
        })
    }
}

/// Workgroups needed to cover `items` with groups of `per_group`.
pub(crate) fn groups(items: usize, per_group: u32) -> u32 {
    u32::try_from(items.div_ceil(per_group as usize)).unwrap_or(u32::MAX)
}

/// Split a 1-D workgroup count into (x, y) with x within the dispatch limit;
/// kernels recover the index as x + y * nx and skip the excess.
pub(crate) fn grid_2d(count: u32, max_per_dimension: u32) -> (u32, u32) {
    if count <= max_per_dimension {
        (count.max(1), 1)
    } else {
        (max_per_dimension, count.div_ceil(max_per_dimension))
    }
}

/// A storage buffer holding `bytes` (at least 16 bytes, zero-padded).
pub(crate) fn storage_init(ctx: &GpuContext, label: &str, bytes: &[u8]) -> wgpu::Buffer {
    if bytes.len() >= 16 {
        return ctx
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytes,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            });
    }
    let mut padded = [0u8; 16];
    padded[..bytes.len()].copy_from_slice(bytes);
    ctx.device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: &padded,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        })
}

/// A zeroed read-write storage buffer that can be copied from and cleared.
pub(crate) fn storage_zeroed(ctx: &GpuContext, label: &str, bytes: u64) -> wgpu::Buffer {
    ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.max(16).next_multiple_of(16),
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_SRC
            | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// A uniform buffer holding `words`.
pub(crate) fn uniform(ctx: &GpuContext, label: &str, words: &[u32]) -> wgpu::Buffer {
    ctx.device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: bytemuck::cast_slice(words),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        })
}

/// A host-readable staging buffer.
pub(crate) fn staging(ctx: &GpuContext, label: &str, bytes: u64) -> wgpu::Buffer {
    ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.max(16).next_multiple_of(16),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Map the first `bytes[i]` bytes of each staging buffer, after the queue has
/// finished all submitted work, and copy them out.
pub(crate) fn read_staging(
    ctx: &GpuContext,
    buffers: &[(&wgpu::Buffer, u64)],
) -> Result<Vec<Vec<u8>>, GpuModernError> {
    let mut receivers = Vec::with_capacity(buffers.len());
    for (buffer, bytes) in buffers {
        let (sender, receiver) = std::sync::mpsc::channel();
        buffer
            .slice(0..*bytes)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        receivers.push(receiver);
    }
    ctx.device
        .poll(wgpu::PollType::wait())
        .map_err(|e| GpuModernError::Execution(format!("device poll: {e}")))?;
    let mut out = Vec::with_capacity(buffers.len());
    for ((buffer, bytes), receiver) in buffers.iter().zip(receivers) {
        receiver
            .recv()
            .map_err(|_| GpuModernError::Execution("buffer map callback dropped".into()))?
            .map_err(|e| GpuModernError::Execution(format!("buffer map: {e}")))?;
        let data = buffer.slice(0..*bytes).get_mapped_range().to_vec();
        buffer.unmap();
        out.push(data);
    }
    ctx.check_errors()?;
    Ok(out)
}

/// Little-endian i64 values from bytes.
pub(crate) fn i64s(bytes: &[u8]) -> Vec<i64> {
    bytes
        .chunks_exact(8)
        .map(|chunk| {
            let mut word = [0u8; 8];
            word.copy_from_slice(chunk);
            i64::from_le_bytes(word)
        })
        .collect()
}

/// Little-endian bytes of i64 values (the GPU's vec2<u32> layout).
pub(crate) fn i64_bytes(values: &[i64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Little-endian bytes of i32 values.
pub(crate) fn i32_bytes(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// The cursor uniform: first position, token count and token ids.
pub(crate) fn cursor_words(pos0: usize, tokens: &[u32]) -> [u32; CURSOR_BYTES / 4] {
    let mut words = [0u32; CURSOR_BYTES / 4];
    words[0] = u32::try_from(pos0).unwrap_or(u32::MAX);
    words[1] = u32::try_from(tokens.len()).unwrap_or(u32::MAX);
    for (slot, &token) in words[4..].iter_mut().zip(tokens) {
        *slot = token;
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modules_bind_the_library_once_and_pick_the_dot_product() {
        for kernel in Kernel::ALL {
            let source = module_source(kernel, false);
            assert_eq!(source.matches("var<storage, read_write> status").count(), 1);
            assert!(source.contains(&format!("fn {}(", kernel.entry())));
            assert_eq!(source.contains("exp_table"), kernel.needs_exp());
            assert!(!source.contains("dot4I8Packed"));
            assert!(!source.contains("f32") && !source.contains("f16"));
        }
        let native = module_source(Kernel::Gemv, true);
        assert!(native.starts_with("requires packed_4x8_integer_dot_product;"));
        assert!(native.contains("dot4I8Packed(a, b)"));
        assert!(!module_source(Kernel::Rope, true).contains("requires"));
    }

    #[test]
    fn grids_cover_every_group() {
        assert_eq!(grid_2d(0, 65_535), (1, 1));
        assert_eq!(grid_2d(65_535, 65_535), (65_535, 1));
        assert_eq!(grid_2d(65_536, 65_535), (65_535, 2));
        assert_eq!(groups(128_256, GEMV_ROWS), 16_032);
        assert_eq!(groups(11_008, ELEMENT_WG), 172);
    }

    #[test]
    fn cursor_and_byte_helpers_are_little_endian() {
        let words = cursor_words(7, &[3, 128_011]);
        assert_eq!(&words[..6], &[7, 2, 0, 0, 3, 128_011]);
        assert_eq!(
            i64s(&i64_bytes(&[-1, 1 << 40, i64::MIN])),
            [-1, 1 << 40, i64::MIN]
        );
        assert_eq!(i32_bytes(&[-2]), (-2i32).to_le_bytes());
    }
}
