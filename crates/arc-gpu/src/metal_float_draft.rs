//! NON-CANONICAL: ARC's canonical per-row INT8 projection with the dot
//! product formed in f32, on Apple GPUs, for a speculative-decoding drafter.
//!
//! A drafter only proposes tokens. ARC's exact engine verifies every one, so
//! nothing computed here is ever an ARC output, and the exact paths
//! (`metal_exact`, and `metal_decoder` unless a decoder is built with
//! `MetalDecoder::new_float_draft`) never call it. The kernels
//! (`metal_float_draft.metal`) are compiled as their own library, with fast
//! math off.
//!
//! The projection reproduces the order of the CPU study
//! (`arc_inference::float_accumulation_study`, #191) operation for operation:
//! the INT8 weights and the Q16 activations converted to f32 (round to
//! nearest, ties to even), sixteen chains of fused multiply-adds over the
//! whole 16-column blocks, the chains summed in order, the remaining columns,
//! `round`, and the exact engine's requantisation `(dot * s) >> 16`. The tests
//! below compare every output with a CPU model of that order, bit for bit.
//!
//! [`RowThreads`] chooses how many threads run a row's sixteen chains (1, 4
//! or 16). It changes speed only: every choice computes the same bits.

use metal::{
    BufferRef, CompileOptions, ComputeCommandEncoderRef, ComputePipelineState, DeviceRef,
    MTLCommandBufferStatus, MTLResourceOptions, MTLSize,
};
use objc::rc::autoreleasepool;

use crate::metal_exact::{BLOCK_BYTES, MetalExactGemv, ResidentMatrix, gpu_seconds};

/// Source of the drafter's kernels.
pub const FLOAT_DRAFT_SOURCE: &str = include_str!("metal_float_draft.metal");

/// Threads per threadgroup for every kernel here.
const GROUP_THREADS: u64 = 64;
const SELF_TEST_SEED: u64 = 0x00F1_0A7D_5E1F_7E57;

/// The CPU study's dot product (`dot_f32` in
/// `arc_inference::float_accumulation_study`), rounded to an integer as
/// there: what every kernel here must equal. The weights and activations
/// are converted to f32, sixteen chains of fused multiply-adds run over the
/// whole 16-column blocks, the chains are summed in order, the remaining
/// columns are added with fused multiply-adds, and the sum is rounded.
pub fn reference_dot_f32(weights: &[i8], input: &[i64]) -> i64 {
    let mut lanes = [0f32; 16];
    let whole = weights.len().min(input.len()) / 16 * 16;
    for (w, x) in weights[..whole]
        .chunks_exact(16)
        .zip(input[..whole].chunks_exact(16))
    {
        for ((lane, &wv), &xv) in lanes.iter_mut().zip(w).zip(x) {
            *lane = f32::from(wv).mul_add(xv as f32, *lane);
        }
    }
    let mut acc = 0f32;
    for lane in lanes {
        acc += lane;
    }
    for (&wv, &xv) in weights[whole..].iter().zip(&input[whole..]) {
        acc = f32::from(wv).mul_add(xv as f32, acc);
    }
    acc.round() as i64
}

/// SplitMix64, for the self-test.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A random INT8 matrix, per-row scales and activations of up to `top`
    /// bits: the self-test's and the tests' inputs.
    fn case(&mut self, rows: usize, cols: usize, top: u32) -> (Vec<i8>, Vec<i64>, Vec<i64>) {
        let data = (0..rows * cols).map(|_| self.next_u64() as i8).collect();
        let scales = (0..rows)
            .map(|_| (self.next_u64() % (1 << 20)) as i64 - (1 << 19))
            .collect();
        let input = (0..cols)
            .map(|_| {
                let bits = 1 + self.next_u64() % u64::from(top);
                (self.next_u64() as i64) >> (64 - bits)
            })
            .collect();
        (data, scales, input)
    }
}

/// [`reference_dot_f32`] of every row, raw or requantised as the kernels'
/// `raw` flag says.
fn reference_rows(data: &[i8], scales: &[i64], input: &[i64], raw: bool) -> Vec<i64> {
    data.chunks_exact(input.len().max(1))
        .zip(scales)
        .map(|(row, &scale)| {
            let dot = reference_dot_f32(row, input);
            if raw {
                dot
            } else {
                dot.wrapping_mul(scale) >> 16
            }
        })
        .collect()
}

/// How many threads run one row's sixteen chains. Every choice computes the
/// same bits; this changes speed only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RowThreads {
    /// One thread per row: all sixteen chains in registers, 16-byte loads.
    One,
    /// Four threads per row, four chains each, 4-byte loads.
    Four,
    /// Sixteen threads per row, one chain each, byte loads.
    Sixteen,
}

impl RowThreads {
    pub const ALL: [RowThreads; 3] = [RowThreads::One, RowThreads::Four, RowThreads::Sixteen];
    /// What a float-draft decoder uses unless told otherwise.
    pub const DEFAULT: RowThreads = RowThreads::Four;

    /// Threads per matrix row.
    pub fn threads(self) -> u64 {
        match self {
            RowThreads::One => 1,
            RowThreads::Four => 4,
            RowThreads::Sixteen => 16,
        }
    }

    fn kernel(self) -> &'static str {
        match self {
            RowThreads::One => "draft_gemv_t1",
            RowThreads::Four => "draft_gemv_t4",
            RowThreads::Sixteen => "draft_gemv_t16",
        }
    }
}

/// Mirrors `DraftGemvParams` in the kernel source.
#[repr(C)]
#[derive(Clone, Copy)]
struct GemvParams {
    rows: u32,
    cols: u32,
    blocks: u32,
    /// 0: requantised; 1: `round(dot)` itself (tests).
    mode: u32,
}

/// Mirrors `DraftConvertParams` in the kernel source.
#[repr(C)]
#[derive(Clone, Copy)]
struct ConvertParams {
    n: u32,
    pad: [u32; 3],
}

fn set_params<T: Copy>(encoder: &ComputeCommandEncoderRef, index: u64, value: &T) {
    encoder.set_bytes(
        index,
        std::mem::size_of::<T>() as u64,
        std::ptr::from_ref(value).cast(),
    );
}

/// The drafter's compiled kernels.
pub struct FloatDraftKernels {
    to_f32: ComputePipelineState,
    gemv: Vec<(RowThreads, ComputePipelineState)>,
}

impl FloatDraftKernels {
    /// Compile the kernels for `engine`'s device, with fast math off, and run
    /// [`Self::self_test`].
    pub fn new(engine: &MetalExactGemv) -> Result<Self, String> {
        let kernels = Self::build(&engine.device)?;
        kernels.self_test(engine)?;
        Ok(kernels)
    }

    fn build(device: &DeviceRef) -> Result<Self, String> {
        autoreleasepool(|| {
            let options = CompileOptions::new();
            options.set_fast_math_enabled(false);
            let library = device
                .new_library_with_source(FLOAT_DRAFT_SOURCE, &options)
                .map_err(|e| format!("float drafter kernels failed to compile: {e}"))?;
            let make = |name: &str| -> Result<ComputePipelineState, String> {
                let function = library
                    .get_function(name, None)
                    .map_err(|e| format!("{name}: {e}"))?;
                device
                    .new_compute_pipeline_state_with_function(&function)
                    .map_err(|e| format!("{name}: {e}"))
            };
            let to_f32 = make("draft_to_f32")?;
            let mut gemv = Vec::with_capacity(RowThreads::ALL.len());
            for threads in RowThreads::ALL {
                gemv.push((threads, make(threads.kernel())?));
            }
            for pipeline in std::iter::once(&to_f32).chain(gemv.iter().map(|(_, p)| p)) {
                if pipeline.max_total_threads_per_threadgroup() < GROUP_THREADS {
                    return Err(format!(
                        "a float drafter kernel cannot run {GROUP_THREADS} threads per group"
                    ));
                }
            }
            // A row's 4 or 16 threads must share a simdgroup for the shuffles.
            for (threads, pipeline) in &gemv {
                if *threads != RowThreads::One
                    && !pipeline.thread_execution_width().is_multiple_of(16)
                {
                    return Err(
                        "the float drafter kernels need simdgroups of a multiple of 16 lanes"
                            .to_string(),
                    );
                }
            }
            Ok(Self { to_f32, gemv })
        })
    }

    /// Compare every kernel, raw and requantised, with
    /// [`reference_dot_f32`] on random matrices with every tail length class
    /// and activations of up to 40 bits. A device that differs in one bit is
    /// not used.
    pub fn self_test(&self, engine: &MetalExactGemv) -> Result<(), String> {
        let mut rng = SplitMix64(SELF_TEST_SEED);
        for (rows, cols, top) in [
            (19usize, 7usize, 40u32),
            (33, 64, 36),
            (21, 88, 31),
            (17, 300, 33),
        ] {
            let (data, scales, input) = rng.case(rows, cols, top);
            let matrix = engine.upload(
                &data,
                &scales,
                rows,
                cols,
                crate::metal_exact::Storage::Shared,
            )?;
            for raw in [true, false] {
                let want = reference_rows(&data, &scales, &input, raw);
                for threads in RowThreads::ALL {
                    if self.project(engine, &matrix, &input, threads, raw)? != want {
                        return Err(format!(
                            "float drafter self-test: {threads:?} (raw {raw}) differs from the CPU order on {rows}x{cols}"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// `y[i] = x[i] as f32` for `i < n` (round to nearest, ties to even).
    pub fn encode_to_f32(
        &self,
        encoder: &ComputeCommandEncoderRef,
        x: &BufferRef,
        y: &BufferRef,
        n: usize,
    ) {
        encoder.set_compute_pipeline_state(&self.to_f32);
        encoder.set_buffer(0, Some(x), 0);
        encoder.set_buffer(1, Some(y), 0);
        set_params(
            encoder,
            2,
            &ConvertParams {
                n: n as u32,
                pad: [0; 3],
            },
        );
        encoder.dispatch_thread_groups(
            MTLSize::new((n as u64).div_ceil(GROUP_THREADS), 1, 1),
            MTLSize::new(GROUP_THREADS, 1, 1),
        );
    }

    /// Every row of `matrix` against the f32 activations `xf` (at least
    /// `n_cols` values): requantised, or `round(dot)` itself if `raw`.
    pub fn encode_gemv(
        &self,
        encoder: &ComputeCommandEncoderRef,
        matrix: &ResidentMatrix,
        xf: &BufferRef,
        out: &BufferRef,
        threads: RowThreads,
        raw: bool,
    ) {
        let pipeline = self
            .gemv
            .iter()
            .find(|(t, _)| *t == threads)
            .map(|(_, pipeline)| pipeline)
            .expect("a pipeline for every RowThreads");
        encoder.set_compute_pipeline_state(pipeline);
        encoder.set_buffer(0, Some(&matrix.weights), 0);
        encoder.set_buffer(1, Some(xf), 0);
        encoder.set_buffer(2, Some(&matrix.scales), 0);
        encoder.set_buffer(3, Some(out), 0);
        set_params(
            encoder,
            4,
            &GemvParams {
                rows: matrix.n_rows as u32,
                cols: matrix.n_cols as u32,
                blocks: (matrix.stride / BLOCK_BYTES) as u32,
                mode: u32::from(raw),
            },
        );
        let total = matrix.n_rows as u64 * threads.threads();
        encoder.dispatch_thread_groups(
            MTLSize::new(total.div_ceil(GROUP_THREADS), 1, 1),
            MTLSize::new(GROUP_THREADS, 1, 1),
        );
    }

    /// `input as f32`, on the GPU. For tests.
    pub fn convert(&self, engine: &MetalExactGemv, input: &[i64]) -> Result<Vec<f32>, String> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        autoreleasepool(|| {
            let device = &engine.device;
            let x = device.new_buffer_with_data(
                input.as_ptr().cast(),
                std::mem::size_of_val(input) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let y = device.new_buffer(
                (input.len() * 4) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let commands = engine.queue.new_command_buffer();
            let encoder = commands.new_compute_command_encoder();
            self.encode_to_f32(encoder, &x, &y, input.len());
            encoder.end_encoding();
            commands.commit();
            commands.wait_until_completed();
            if commands.status() != MTLCommandBufferStatus::Completed {
                return Err("the conversion did not complete".to_string());
            }
            // SAFETY: a shared buffer of `input.len()` f32 values; the command
            // buffer that wrote it has completed.
            Ok(unsafe {
                std::slice::from_raw_parts(y.contents().cast::<f32>(), input.len()).to_vec()
            })
        })
    }

    /// One projection of `input` in its own command buffer: the conversion,
    /// then the kernel for `threads`; requantised, or `round(dot)` if `raw`.
    pub fn project(
        &self,
        engine: &MetalExactGemv,
        matrix: &ResidentMatrix,
        input: &[i64],
        threads: RowThreads,
        raw: bool,
    ) -> Result<Vec<i64>, String> {
        if input.len() != matrix.n_cols {
            return Err(format!(
                "{} activations for {} columns",
                input.len(),
                matrix.n_cols
            ));
        }
        autoreleasepool(|| {
            let device = &engine.device;
            let x = device.new_buffer_with_data(
                input.as_ptr().cast(),
                std::mem::size_of_val(input) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let xf = device.new_buffer(
                (input.len() * 4) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let out = device.new_buffer(
                (matrix.n_rows * 8) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let commands = engine.queue.new_command_buffer();
            let encoder = commands.new_compute_command_encoder();
            self.encode_to_f32(encoder, &x, &xf, input.len());
            self.encode_gemv(encoder, matrix, &xf, &out, threads, raw);
            encoder.end_encoding();
            commands.commit();
            commands.wait_until_completed();
            if commands.status() != MTLCommandBufferStatus::Completed {
                return Err("the projection did not complete".to_string());
            }
            // SAFETY: a shared buffer of `n_rows` i64 values; the command
            // buffer that wrote it has completed.
            Ok(unsafe {
                std::slice::from_raw_parts(out.contents().cast::<i64>(), matrix.n_rows).to_vec()
            })
        })
    }

    /// GPU seconds for one pass over `items`, each a whole-matrix projection,
    /// averaged over `repeats` passes in one command buffer: the method of
    /// `MetalExactGemv::time_batch`. Each input is converted to f32 once,
    /// before the timed command buffer.
    pub fn time_batch(
        &self,
        engine: &MetalExactGemv,
        items: &[(&ResidentMatrix, &[i64])],
        threads: RowThreads,
        repeats: usize,
    ) -> Result<f64, String> {
        if items.is_empty() || repeats == 0 {
            return Err("nothing to time".to_string());
        }
        if items
            .iter()
            .any(|(matrix, input)| input.len() != matrix.n_cols)
        {
            return Err("an input does not match its matrix".to_string());
        }
        autoreleasepool(|| {
            let device = &engine.device;
            let mut prepared = Vec::with_capacity(items.len());
            let commands = engine.queue.new_command_buffer();
            let encoder = commands.new_compute_command_encoder();
            for &(matrix, input) in items {
                let x = device.new_buffer_with_data(
                    input.as_ptr().cast(),
                    std::mem::size_of_val(input) as u64,
                    MTLResourceOptions::StorageModeShared,
                );
                let xf = device.new_buffer(
                    (input.len() * 4) as u64,
                    MTLResourceOptions::StorageModeShared,
                );
                let out = device.new_buffer(
                    (matrix.n_rows * 8) as u64,
                    MTLResourceOptions::StorageModeShared,
                );
                self.encode_to_f32(encoder, &x, &xf, input.len());
                prepared.push((matrix, xf, out));
            }
            encoder.end_encoding();
            commands.commit();
            commands.wait_until_completed();
            if commands.status() != MTLCommandBufferStatus::Completed {
                return Err("the conversions did not complete".to_string());
            }
            let commands = engine.queue.new_command_buffer();
            let encoder = commands.new_compute_command_encoder();
            for _ in 0..repeats {
                for (matrix, xf, out) in &prepared {
                    self.encode_gemv(encoder, matrix, xf, out, threads, false);
                }
            }
            encoder.end_encoding();
            commands.commit();
            commands.wait_until_completed();
            if commands.status() != MTLCommandBufferStatus::Completed {
                return Err("the timed projections did not complete".to_string());
            }
            Ok(gpu_seconds(commands) / repeats as f64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metal_exact::Storage;

    /// Remove `//` comments.
    fn code_of(source: &str) -> String {
        source
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn float_draft_source_has_no_half_precision_and_no_division() {
        let code = code_of(FLOAT_DRAFT_SOURCE);
        let words: Vec<&str> = code
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .collect();
        for banned in [
            "half", "half2", "half3", "half4", "bfloat", "double", "fast", "precise",
        ] {
            assert!(
                !words.contains(&banned),
                "the float drafter kernels must not use {banned}"
            );
        }
        assert!(!code.contains('/'), "no integer division (see #150)");
        assert!(!code.contains('%'), "no modulo (see #150)");
    }

    #[test]
    fn conversion_rounds_like_rust() {
        let engine = MetalExactGemv::new().expect("Metal device");
        let kernels = FloatDraftKernels::new(&engine).expect("kernels and their self-test");
        let mut values = vec![
            0,
            1,
            -1,
            i64::MAX,
            i64::MIN,
            -i64::MAX,
            i64::MAX - 1,
            i64::MIN + 1,
        ];
        for bit in [24u32, 25, 26, 31, 32, 33, 40, 53, 54, 62] {
            for delta in -3i64..=7 {
                let v = (1i64 << bit).wrapping_add(delta);
                values.push(v);
                values.push(v.wrapping_neg());
            }
        }
        // Exact halfway cases, with even and odd kept parts, and their
        // neighbours.
        for shift in 1u32..=39 {
            for kept in [0x80_0000i64, 0x80_0001, 0xFF_FFFF, 0xAB_CDEF, 0xAB_CDEE] {
                let v = (kept << shift) | (1i64 << (shift - 1));
                values.extend([v, -v, v + 1, v - 1, -(v + 1), -(v - 1)]);
            }
        }
        let mut rng = SplitMix64(0x00F1_0A7D_0000_0001);
        for _ in 0..200_000 {
            let shift = rng.next_u64() % 64;
            values.push((rng.next_u64() >> shift) as i64);
        }
        let got = kernels.convert(&engine, &values).expect("conversion");
        let wrong: Vec<(i64, u32, u32)> = values
            .iter()
            .zip(&got)
            .filter(|&(&v, &g)| g.to_bits() != (v as f32).to_bits())
            .map(|(&v, &g)| (v, g.to_bits(), (v as f32).to_bits()))
            .take(5)
            .collect();
        assert!(
            wrong.is_empty(),
            "GPU conversions differ from `as f32`: {wrong:?}"
        );
    }

    #[test]
    fn every_kernel_matches_the_study_order_bit_for_bit() {
        let engine = MetalExactGemv::new().expect("Metal device");
        let kernels = FloatDraftKernels::new(&engine).expect("kernels and their self-test");
        let mut rng = SplitMix64(0x00F1_0A7D_0000_0002);
        // (rows, columns, largest activation bit): every tail length class,
        // and the Llama-2-7B widths.
        let shapes = [
            (37usize, 1usize, 30u32),
            (37, 15, 30),
            (40, 16, 38),
            (41, 17, 40),
            (33, 31, 20),
            (64, 64, 41),
            (70, 88, 36),
            (65, 200, 33),
            (130, 4104, 31),
            (24, 4096, 30),
            (24, 11008, 30),
        ];
        let mut rounding_visible = 0usize;
        for (rows, cols, top) in shapes {
            let (data, scales, input) = rng.case(rows, cols, top);
            let want_raw = reference_rows(&data, &scales, &input, true);
            let want = reference_rows(&data, &scales, &input, false);
            rounding_visible += data
                .chunks_exact(cols)
                .zip(&want_raw)
                .filter(|&(row, &dot)| {
                    let exact: i128 = row
                        .iter()
                        .zip(&input)
                        .map(|(&w, &x)| i128::from(w) * i128::from(x))
                        .sum();
                    exact != i128::from(dot)
                })
                .count();
            let matrix = engine
                .upload(&data, &scales, rows, cols, Storage::Shared)
                .expect("upload");
            for threads in RowThreads::ALL {
                for raw in [true, false] {
                    let got = kernels
                        .project(&engine, &matrix, &input, threads, raw)
                        .expect("projection");
                    let expected = if raw { &want_raw } else { &want };
                    assert_eq!(&got, expected, "{rows}x{cols}, {threads:?}, raw {raw}");
                }
            }
        }
        // The comparison is sensitive: f32 rounding changed many dots.
        assert!(
            rounding_visible > 100,
            "only {rounding_visible} rows rounded"
        );
        eprintln!("rows where f32 accumulation changed the dot: {rounding_visible}");
    }
}
