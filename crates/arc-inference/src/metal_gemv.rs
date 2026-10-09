//! Opt-in exact Metal GEMV for the canonical per-row INT8 projection.
//!
//! The kernel itself, its exactness proof and its self-test live in
//! [`arc_gpu::metal_exact`]. This module connects it to the engine's single
//! I8 projection entry point (`matmul_i8_view_into`, reached by whole-model
//! decode, validator re-runs and shard forwards), behind three switches that
//! are all off by default:
//!
//! 1. the cargo feature `metal-exact` (without it this module does not exist);
//! 2. the runtime switch `ARC_METAL_EXACT_GEMV=1` or [`set_metal_exact_gemv`];
//! 3. a residency scope: projections run on the GPU only inside
//!    [`MetalModel::run`], and only for matrices that [`MetalModel`] uploaded.
//!
//! Anything else (switch off, no scope, a matrix that is not resident, or any
//! refusal) leaves the call to the existing CPU kernels, which then compute
//! the value exactly as they do today. The GPU path never writes a partial
//! result.
//!
//! # Why a resident copy cannot go stale
//!
//! The device holds a copy of each matrix taken when [`MetalModel`] is built.
//! A projection uses that copy only when the call's weight and scale slices
//! start at the addresses that were copied. `MetalModel<'a>` borrows the model
//! (or the matrices) immutably for `'a`, so while it exists safe code can
//! neither modify, move nor free those vectors, and no other live allocation
//! can occupy their addresses. The scope itself is a closure call: the
//! thread-local that activates it is reset when the closure returns or
//! unwinds, so it cannot outlive the borrow (`mem::forget` cannot leak it).
//!
//! Wiring this as a worker default is a separate, reviewed step.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use arc_gpu::metal_exact::{MetalExactGemv, Refusal, ResidentMatrix, Storage};

use crate::cached_integer_model::{CachedIntegerModel, I8Weights, I8WeightsView};

static REQUESTED: AtomicBool = AtomicBool::new(false);
static ENV_INIT: OnceLock<()> = OnceLock::new();
static ENGINE: OnceLock<Result<Arc<MetalExactGemv>, String>> = OnceLock::new();

static ATTEMPTED: AtomicU64 = AtomicU64::new(0);
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static OUTSIDE_SCOPE: AtomicU64 = AtomicU64::new(0);
static NOT_RESIDENT: AtomicU64 = AtomicU64::new(0);
static REFUSED_SHAPE: AtomicU64 = AtomicU64::new(0);
static REFUSED_INNER_DIM: AtomicU64 = AtomicU64::new(0);
static REFUSED_DOMAIN: AtomicU64 = AtomicU64::new(0);
static REFUSED_SCALE: AtomicU64 = AtomicU64::new(0);
static DEVICE_ERRORS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The residency scope active on this thread, set only by
    /// [`MetalModel::run`] for the duration of its closure.
    static ACTIVE: Cell<*const Residency> = const { Cell::new(std::ptr::null()) };
}

fn env_init() {
    ENV_INIT.get_or_init(|| {
        if std::env::var("ARC_METAL_EXACT_GEMV").as_deref() == Ok("1") {
            REQUESTED.store(true, Ordering::Relaxed);
        }
    });
}

/// Turn the exact Metal GEMV on or off process-wide. Default off; an explicit
/// call overrides `ARC_METAL_EXACT_GEMV`.
pub fn set_metal_exact_gemv(enabled: bool) {
    env_init();
    REQUESTED.store(enabled, Ordering::Relaxed);
}

/// Whether the runtime switch is on. Projections still need a residency scope.
pub fn metal_exact_gemv_requested() -> bool {
    env_init();
    REQUESTED.load(Ordering::Relaxed)
}

/// The process-wide engine, created and self-tested on first use.
pub fn metal_engine() -> Result<Arc<MetalExactGemv>, String> {
    ENGINE
        .get_or_init(|| MetalExactGemv::new().map(Arc::new))
        .clone()
}

/// Counts of projections that reached the Metal hook (switch on).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct MetalCensus {
    pub attempted: u64,
    pub accepted: u64,
    /// No residency scope on the calling thread.
    pub outside_scope: u64,
    /// Inside a scope, but the matrix was not uploaded by it.
    pub not_resident: u64,
    pub refused_shape: u64,
    pub refused_inner_dim_above_i32_bound: u64,
    pub refused_activation_out_of_domain: u64,
    pub refused_scale_multiply_would_overflow: u64,
    pub device_errors: u64,
}

impl MetalCensus {
    /// Projections inside a scope that the CPU computed instead.
    pub fn in_scope_fallbacks(&self) -> u64 {
        self.not_resident
            + self.refused_shape
            + self.refused_inner_dim_above_i32_bound
            + self.refused_activation_out_of_domain
            + self.refused_scale_multiply_would_overflow
            + self.device_errors
    }

    /// Field-wise difference from an earlier reading.
    pub fn since(&self, earlier: &MetalCensus) -> MetalCensus {
        MetalCensus {
            attempted: self.attempted - earlier.attempted,
            accepted: self.accepted - earlier.accepted,
            outside_scope: self.outside_scope - earlier.outside_scope,
            not_resident: self.not_resident - earlier.not_resident,
            refused_shape: self.refused_shape - earlier.refused_shape,
            refused_inner_dim_above_i32_bound: self.refused_inner_dim_above_i32_bound
                - earlier.refused_inner_dim_above_i32_bound,
            refused_activation_out_of_domain: self.refused_activation_out_of_domain
                - earlier.refused_activation_out_of_domain,
            refused_scale_multiply_would_overflow: self.refused_scale_multiply_would_overflow
                - earlier.refused_scale_multiply_would_overflow,
            device_errors: self.device_errors - earlier.device_errors,
        }
    }
}

/// Current counts. Counting is always on; it costs one relaxed atomic add per
/// projection that reaches the hook, and nothing when the switch is off.
pub fn metal_census() -> MetalCensus {
    MetalCensus {
        attempted: ATTEMPTED.load(Ordering::Relaxed),
        accepted: ACCEPTED.load(Ordering::Relaxed),
        outside_scope: OUTSIDE_SCOPE.load(Ordering::Relaxed),
        not_resident: NOT_RESIDENT.load(Ordering::Relaxed),
        refused_shape: REFUSED_SHAPE.load(Ordering::Relaxed),
        refused_inner_dim_above_i32_bound: REFUSED_INNER_DIM.load(Ordering::Relaxed),
        refused_activation_out_of_domain: REFUSED_DOMAIN.load(Ordering::Relaxed),
        refused_scale_multiply_would_overflow: REFUSED_SCALE.load(Ordering::Relaxed),
        device_errors: DEVICE_ERRORS.load(Ordering::Relaxed),
    }
}

fn count(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// One uploaded matrix and the addresses of the slices it was copied from.
struct Resident {
    data: usize,
    scales: usize,
    n_rows: usize,
    n_cols: usize,
    matrix: ResidentMatrix,
}

struct Residency {
    engine: Arc<MetalExactGemv>,
    matrices: Vec<Resident>,
}

impl Residency {
    /// The resident matrix that `view` is a contiguous row range of, and the
    /// first row of that range.
    fn find(&self, view: &I8WeightsView<'_>) -> Option<(&Resident, usize)> {
        let data = view.data.as_ptr() as usize;
        let scales = view.scales.as_ptr() as usize;
        if view.n_rows.checked_mul(view.n_cols) != Some(view.data.len())
            || view.scales.len() != view.n_rows
        {
            return None;
        }
        self.matrices.iter().find_map(|resident| {
            if view.n_cols != resident.n_cols || data < resident.data {
                return None;
            }
            let offset = data - resident.data;
            if !offset.is_multiple_of(resident.n_cols) {
                return None;
            }
            let start = offset / resident.n_cols;
            let end = start.checked_add(view.n_rows)?;
            let scales_at = resident
                .scales
                .checked_add(start.checked_mul(std::mem::size_of::<i64>())?)?;
            (end <= resident.n_rows && scales == scales_at).then_some((resident, start))
        })
    }
}

/// Device-resident copies of a model's INT8 projection matrices.
///
/// Borrows the matrices immutably for `'a`; see the module documentation for
/// why that keeps every copy equal to the bytes the CPU would read.
pub struct MetalModel<'a> {
    residency: Residency,
    _weights: PhantomData<&'a I8Weights>,
}

impl<'a> MetalModel<'a> {
    /// Upload every per-row INT8 projection of `model`: seven per layer and
    /// the output projection. Placeholder (empty) shard layers are skipped.
    pub fn new(model: &'a CachedIntegerModel) -> Result<Self, String> {
        let mut matrices: Vec<&'a I8Weights> = Vec::with_capacity(model.layers.len() * 7 + 1);
        for layer in &model.layers {
            matrices.extend([
                &layer.wq,
                &layer.wk,
                &layer.wv,
                &layer.wo,
                &layer.w_gate,
                &layer.w_up,
                &layer.w_down,
            ]);
        }
        matrices.push(&model.output_weight);
        Self::from_matrices(&matrices)
    }

    /// Upload the given matrices.
    pub fn from_matrices(weights: &[&'a I8Weights]) -> Result<Self, String> {
        let engine = metal_engine()?;
        let mut matrices = Vec::with_capacity(weights.len());
        for w in weights {
            if w.n_rows == 0 || w.data.is_empty() {
                continue;
            }
            let matrix = engine.upload(&w.data, &w.scales, w.n_rows, w.n_cols, Storage::Shared)?;
            matrices.push(Resident {
                data: w.data.as_ptr() as usize,
                scales: w.scales.as_ptr() as usize,
                n_rows: w.n_rows,
                n_cols: w.n_cols,
                matrix,
            });
        }
        Ok(Self {
            residency: Residency { engine, matrices },
            _weights: PhantomData,
        })
    }

    /// Number of matrices on the device.
    pub fn resident_matrices(&self) -> usize {
        self.residency.matrices.len()
    }

    /// INT8 weight bytes on the device.
    pub fn resident_weight_bytes(&self) -> usize {
        self.residency
            .matrices
            .iter()
            .map(|resident| resident.matrix.weight_bytes())
            .sum()
    }

    /// Run `f` with this residency scope active on the current thread. While
    /// it runs, and the runtime switch is on, projections of resident matrices
    /// on this thread run on the GPU. Scopes nest; the previous one is
    /// restored when `f` returns or unwinds.
    pub fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        struct Restore(*const Residency);
        impl Drop for Restore {
            fn drop(&mut self) {
                ACTIVE.with(|active| active.set(self.0));
            }
        }
        let previous = ACTIVE.with(|active| active.replace(&self.residency));
        let _restore = Restore(previous);
        f()
    }
}

/// The hook in `matmul_i8_view_into`. Returns `true` only if `output` now
/// holds the exact canonical projection; on `false` nothing was written and
/// the caller runs its CPU kernel.
pub(crate) fn try_project(
    weights: I8WeightsView<'_>,
    input: &[i64],
    in_size: usize,
    output: &mut [i64],
) -> bool {
    count(&ATTEMPTED);
    let active = ACTIVE.with(Cell::get);
    if active.is_null() {
        count(&OUTSIDE_SCOPE);
        return false;
    }
    // SAFETY: ACTIVE is non-null only while `MetalModel::run` is executing on
    // this thread, and `run` holds `&self` for the whole call, so the
    // `Residency` it points to is alive and not mutated.
    let residency = unsafe { &*active };
    if weights.n_rows == 0
        || in_size != weights.n_cols
        || input.len() != in_size
        || output.len() != weights.n_rows
    {
        count(&REFUSED_SHAPE);
        return false;
    }
    let Some((resident, start)) = residency.find(&weights) else {
        count(&NOT_RESIDENT);
        return false;
    };
    match residency.engine.project_rows(
        &resident.matrix,
        start..start + weights.n_rows,
        input,
        output,
    ) {
        Ok(_) => {
            count(&ACCEPTED);
            true
        }
        Err(refusal) => {
            count(match refusal {
                Refusal::Shape => &REFUSED_SHAPE,
                Refusal::InnerDimAboveI32Bound => &REFUSED_INNER_DIM,
                Refusal::ActivationOutOfDomain => &REFUSED_DOMAIN,
                Refusal::ScaleMultiplyWouldOverflow => &REFUSED_SCALE,
                Refusal::Device => &DEVICE_ERRORS,
            });
            false
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Turns the runtime switch on or off for one test and restores it after,
    /// even on panic. Hold `canonical_simd::kernel_switch_guard()` as well.
    pub(crate) struct SwitchGuard(bool);

    impl SwitchGuard {
        pub(crate) fn set(enabled: bool) -> Self {
            let previous = metal_exact_gemv_requested();
            set_metal_exact_gemv(enabled);
            SwitchGuard(previous)
        }
    }

    impl Drop for SwitchGuard {
        fn drop(&mut self) {
            set_metal_exact_gemv(self.0);
        }
    }

    /// SplitMix64 for test inputs.
    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// Uniform in `[-limit, limit]`.
        pub(crate) fn symmetric(&mut self, limit: i64) -> i64 {
            let span = 2 * limit as u64 + 1;
            (self.next_u64() % span) as i64 - limit
        }

        /// Raw random bytes, so -128 and 127 both occur.
        pub(crate) fn bytes(&mut self, len: usize) -> Vec<i8> {
            let mut out = Vec::with_capacity(len + 8);
            while out.len() < len {
                out.extend(self.next_u64().to_le_bytes().map(|b| b as i8));
            }
            out.truncate(len);
            out
        }
    }

    /// Random weights with per-row Q16 scales in `[1, 4000]`.
    pub(crate) fn weights(rng: &mut Rng, rows: usize, cols: usize) -> I8Weights {
        I8Weights {
            data: rng.bytes(rows * cols),
            scales: (0..rows).map(|_| 1 + rng.symmetric(3999).abs()).collect(),
            n_rows: rows,
            n_cols: cols,
        }
    }

    /// The CPU scalar reference: the production kernel with both opt-in
    /// paths off.
    pub(crate) fn cpu_scalar(weights: &I8Weights, input: &[i64]) -> Vec<i64> {
        assert!(!crate::canonical_simd::fast_canonical_kernel_enabled());
        assert!(!metal_exact_gemv_requested());
        let mut out = vec![0i64; weights.n_rows];
        crate::cached_integer_model::matmul_i8_canonical_rows(weights, input, &mut out)
            .expect("input inside the CPU domain");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Rng, SwitchGuard, cpu_scalar, weights};
    use super::*;
    use crate::cached_integer_model::{matmul_i8_canonical_row_range, matmul_i8_canonical_rows};
    use crate::canonical_simd::{self, kernel_switch_guard};
    use arc_gpu::metal_exact::{
        Epilogue, MAX_COLS, MAX_PLANES, PLANE_MAX, PLANE_MIN, Tile, reference_rows, split_planes,
    };

    const SENTINEL: i64 = 0x7E57_7E57_7E57_7E57;

    fn ints(value: &serde_json::Value) -> Vec<i64> {
        value
            .as_array()
            .expect("array")
            .iter()
            .map(|v| v.as_i64().expect("integer"))
            .collect()
    }

    #[test]
    fn gpu_digit_planes_are_the_cpu_limb_digits() {
        assert_eq!(MAX_PLANES, canonical_simd::LIMB_COUNT);
        assert_eq!(PLANE_MAX, canonical_simd::LIMB_MAX);
        assert_eq!(PLANE_MIN, canonical_simd::LIMB_MIN);
        assert_eq!(MAX_COLS, canonical_simd::MAX_COLS_FOR_I32);
        let mut rng = Rng(0xD161_7500);
        let mut cases = vec![
            vec![
                0, 1, -1, 127, 128, -128, -129, 255, 256, -256, 32_767, -32_768,
            ],
            vec![
                PLANE_MAX,
                PLANE_MIN,
                PLANE_MAX - 1,
                PLANE_MIN + 1,
                16_777_216,
            ],
            vec![PLANE_MAX + 1],
            vec![3, PLANE_MIN - 1, 5],
            vec![i64::MAX, 0],
            vec![i64::MIN],
        ];
        for round in 0..300 {
            let len = 1 + (rng.next_u64() % 70) as usize;
            let limit = [127, 32_639, 8_355_711, PLANE_MAX, PLANE_MAX + 2][round % 5];
            cases.push((0..len).map(|_| rng.symmetric(limit)).collect());
        }
        for input in cases {
            let n = input.len();
            let mut cpu = vec![0i8; MAX_PLANES * n];
            let cpu_used = canonical_simd::split_limbs(&input, &mut cpu);
            let stride = n.div_ceil(16) * 16;
            let mut gpu = vec![0x33i8; MAX_PLANES * stride];
            let gpu_used = split_planes(&input, &mut gpu, stride);
            assert_eq!(cpu_used, gpu_used, "{input:?}");
            if cpu_used.is_some() {
                for (cpu_plane, gpu_plane) in cpu.chunks_exact(n).zip(gpu.chunks_exact(stride)) {
                    assert_eq!(cpu_plane, &gpu_plane[..n], "{input:?}");
                }
            }
        }
    }

    /// Projection vectors computed by the independent Python reference of the
    /// integer profile contract (scripts/arc_conformance).
    #[test]
    fn operator_kat_projections_are_byte_identical_on_the_gpu() {
        let _guard = kernel_switch_guard();
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/integer_operator_kat.json"))
                .expect("operator KAT");
        let engine = metal_engine().expect("Metal device");
        let cases = doc["matmul_rows"].as_array().expect("matmul_rows");
        assert!(!cases.is_empty());
        for (index, case) in cases.iter().enumerate() {
            let rows = case["rows"].as_u64().expect("rows") as usize;
            let cols = case["cols"].as_u64().expect("cols") as usize;
            let data: Vec<i8> = ints(&case["weights"])
                .into_iter()
                .map(|w| w as i8)
                .collect();
            let scales = ints(&case["scales"]);
            let input = ints(&case["input"]);
            let expected = ints(&case["output"]);
            let matrix = engine
                .upload(&data, &scales, rows, cols, Storage::Shared)
                .expect("upload");
            for tile in Tile::all() {
                let mut got = vec![SENTINEL; rows];
                engine
                    .project_rows_with(
                        &matrix,
                        0..rows,
                        &input,
                        &mut got,
                        tile,
                        Epilogue::CanonicalQ16,
                    )
                    .expect("KAT inputs are in the domain");
                assert_eq!(
                    got,
                    expected,
                    "operator KAT case {index}, tile {}",
                    tile.label()
                );
            }
        }
    }

    #[test]
    fn random_and_boundary_matrices_match_the_cpu_scalar_kernel() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let tiles = Tile::all();
        let mut rng = Rng(0x5EED_6E6D_0002);
        let mut compared = 0usize;
        for cols in [1usize, 7, 15, 16, 17, 31, 32, 33, 63, 65, 255, 4096, 4097] {
            for rows in [1usize, 3, 8, 9, 33, 257] {
                let mut w = weights(&mut rng, rows, cols);
                w.data[0] = -128;
                *w.data.last_mut().expect("non-empty") = 127;
                let input: Vec<i64> = (0..cols)
                    .map(|j| match j % 5 {
                        0 => PLANE_MAX,
                        1 => PLANE_MIN,
                        2 => 0,
                        3 => -rng.symmetric(8_388_608).abs() - 1,
                        _ => rng.symmetric(8_388_608).abs() + 1,
                    })
                    .collect();
                let want = cpu_scalar(&w, &input);
                assert_eq!(
                    want,
                    reference_rows(&w.data, &w.scales, cols, &input, Epilogue::CanonicalQ16)
                );
                let matrix = engine
                    .upload(&w.data, &w.scales, rows, cols, Storage::Shared)
                    .expect("upload");
                for &tile in &tiles {
                    let mut got = vec![SENTINEL; rows];
                    engine
                        .project_rows_with(
                            &matrix,
                            0..rows,
                            &input,
                            &mut got,
                            tile,
                            Epilogue::CanonicalQ16,
                        )
                        .expect("in domain");
                    assert_eq!(got, want, "{rows}x{cols} tile {}", tile.label());
                    compared += 1;
                }
            }
        }
        eprintln!("random and boundary matrices: {compared} GPU projections byte-identical");
    }

    #[test]
    fn llama_7b_projection_shapes_match_the_cpu_scalar_kernel() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let mut rng = Rng(0x0007_B5EE_D000);
        for (rows, cols) in [
            (4096usize, 4096usize),
            (11008, 4096),
            (4096, 11008),
            (32000, 4096),
        ] {
            let w = weights(&mut rng, rows, cols);
            let matrix = engine
                .upload(&w.data, &w.scales, rows, cols, Storage::Shared)
                .expect("upload");
            // Typical Llama-2-7B activation magnitude (about 2^22.6): three
            // digit planes; and a four-plane vector.
            for limit in [6_500_000i64, 1 << 30] {
                let input: Vec<i64> = (0..cols).map(|_| rng.symmetric(limit)).collect();
                let want = cpu_scalar(&w, &input);
                for tile in [
                    Tile::DEFAULT,
                    Tile {
                        rows_per_simdgroup: 8,
                        simdgroups: 4,
                        mul16: true,
                    },
                ] {
                    let mut got = vec![SENTINEL; rows];
                    let planes = engine
                        .project_rows_with(
                            &matrix,
                            0..rows,
                            &input,
                            &mut got,
                            tile,
                            Epilogue::CanonicalQ16,
                        )
                        .expect("in domain");
                    assert_eq!(
                        got,
                        want,
                        "{rows}x{cols}, |x| <= {limit}, tile {}",
                        tile.label()
                    );
                    eprintln!(
                        "{rows}x{cols} |x|<={limit} tile {}: {planes} planes, identical",
                        tile.label()
                    );
                }
            }
        }
    }

    #[test]
    fn the_i32_bound_and_the_scale_bound_are_exact_at_their_edges() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        // Every digit of LIMB_MIN is -128; with -128 weights each digit-plane
        // sum is 16,384 * 131,071 = 2,147,467,264, the largest i32 sum allowed.
        let cols = MAX_COLS;
        let input = vec![PLANE_MIN; cols];
        let dot_bound = 128 * cols as i64 * PLANE_MIN.abs();
        let largest = i64::MAX / dot_bound;
        let accepted = I8Weights {
            data: vec![-128; 2 * cols],
            scales: vec![largest, 1],
            n_rows: 2,
            n_cols: cols,
        };
        let want = cpu_scalar(&accepted, &input);
        let matrix = engine
            .upload(&accepted.data, &accepted.scales, 2, cols, Storage::Shared)
            .expect("upload");
        let mut got = vec![SENTINEL; 2];
        engine
            .project(&matrix, &input, &mut got)
            .expect("on the boundary");
        assert_eq!(got, want);

        // One past the scale bound: the CPU's checked entry refuses, the CPU
        // limb kernel refuses, and so does the GPU, writing nothing.
        let over = I8Weights {
            scales: vec![largest + 1, 1],
            ..accepted
        };
        let mut cpu_out = vec![SENTINEL; 2];
        assert!(matmul_i8_canonical_rows(&over, &input, &mut cpu_out).is_err());
        assert!(!canonical_simd::matmul_i8_canonical_rows_fast(
            &over,
            &input,
            cols,
            &mut cpu_out
        ));
        let matrix = engine
            .upload(&over.data, &over.scales, 2, cols, Storage::Shared)
            .expect("upload");
        let mut gpu_out = vec![SENTINEL; 2];
        assert_eq!(
            engine.project(&matrix, &input, &mut gpu_out),
            Err(Refusal::ScaleMultiplyWouldOverflow)
        );
        assert_eq!(gpu_out, vec![SENTINEL; 2]);
        assert_eq!(cpu_out, vec![SENTINEL; 2]);
    }

    #[test]
    fn refusals_match_the_cpu_limb_kernel_and_write_nothing() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let cols = 64usize;
        let good = vec![7i64; cols];
        let mut bad_domain = good.clone();
        bad_domain[cols / 2] = PLANE_MAX + 1;
        let unit = I8Weights {
            data: vec![-128; 4 * cols],
            scales: vec![1; 4],
            n_rows: 4,
            n_cols: cols,
        };
        let huge = I8Weights {
            data: vec![-128; 4 * cols],
            scales: vec![i64::MAX; 4],
            n_rows: 4,
            n_cols: cols,
        };
        let min_scale = I8Weights {
            data: vec![1; 4 * cols],
            scales: vec![1, i64::MIN, 1, 1],
            n_rows: 4,
            n_cols: cols,
        };
        for (name, w, input, refusal) in [
            (
                "activation domain",
                &unit,
                &bad_domain,
                Refusal::ActivationOutOfDomain,
            ),
            (
                "scale overflow",
                &huge,
                &good,
                Refusal::ScaleMultiplyWouldOverflow,
            ),
            (
                "i64::MIN scale",
                &min_scale,
                &good,
                Refusal::ScaleMultiplyWouldOverflow,
            ),
        ] {
            let mut cpu_out = vec![SENTINEL; 4];
            assert!(
                !canonical_simd::matmul_i8_canonical_rows_fast(w, input, cols, &mut cpu_out),
                "{name}: the CPU limb kernel must refuse"
            );
            assert_eq!(cpu_out, vec![SENTINEL; 4], "{name}: the CPU refusal wrote");
            let matrix = engine
                .upload(&w.data, &w.scales, 4, cols, Storage::Shared)
                .expect("upload");
            let mut gpu_out = vec![SENTINEL; 4];
            assert_eq!(
                engine.project(&matrix, input, &mut gpu_out),
                Err(refusal),
                "{name}"
            );
            assert_eq!(
                gpu_out,
                vec![SENTINEL; 4],
                "{name}: a refusal must not write"
            );
        }
        let wide = I8Weights {
            data: vec![1; MAX_COLS + 1],
            scales: vec![1],
            n_rows: 1,
            n_cols: MAX_COLS + 1,
        };
        let input = vec![1i64; MAX_COLS + 1];
        let mut cpu_out = vec![SENTINEL];
        assert!(!canonical_simd::matmul_i8_canonical_rows_fast(
            &wide,
            &input,
            MAX_COLS + 1,
            &mut cpu_out
        ));
        assert_eq!(cpu_out, vec![SENTINEL]);
        let matrix = engine
            .upload(&wide.data, &wide.scales, 1, MAX_COLS + 1, Storage::Shared)
            .expect("upload");
        let mut gpu_out = vec![SENTINEL];
        assert_eq!(
            engine.project(&matrix, &input, &mut gpu_out),
            Err(Refusal::InnerDimAboveI32Bound)
        );
        assert_eq!(gpu_out, vec![SENTINEL]);
    }

    /// The raw-dot mode plus the CPU's own dyadic epilogue `(acc * mu) >> k`
    /// reproduces the SmolLM3 profile's projection exactly.
    #[test]
    fn raw_dots_with_the_cpu_dyadic_epilogue_match_the_modern_profile() {
        use crate::modern::arith::{DyadicMatrix, dyadic_epilogue, project};
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let mut rng = Rng(0xD7AD_1C00);
        for (rows, cols) in [(1usize, 1usize), (5, 33), (64, 2048), (300, 512)] {
            let m = DyadicMatrix {
                rows,
                cols,
                q: (0..rows * cols).map(|_| rng.symmetric(127) as i8).collect(),
                mu: (0..rows)
                    .map(|_| (1i64 << 30) as i32 + (rng.next_u64() % (1 << 30)) as i32)
                    .collect(),
                k: (0..rows)
                    .map(|_| 16 + (rng.next_u64() % 30) as u8)
                    .collect(),
            };
            m.validate("raw-dot test").expect("valid dyadic matrix");
            let input: Vec<i64> = (0..cols).map(|_| rng.symmetric(1 << 24)).collect();
            let mut want = vec![0i64; rows];
            project(&m, &input, &mut want).expect("CPU dyadic projection");
            let matrix = engine
                .upload(&m.q, &vec![1; rows], rows, cols, Storage::Shared)
                .expect("upload");
            let mut acc = vec![SENTINEL; rows];
            engine
                .project_rows_with(
                    &matrix,
                    0..rows,
                    &input,
                    &mut acc,
                    Tile::DEFAULT,
                    Epilogue::RawDot,
                )
                .expect("in domain");
            let got: Vec<i64> = acc
                .iter()
                .zip(m.mu.iter().zip(&m.k))
                .map(|(&a, (&mu, &k))| dyadic_epilogue(a, mu, k).expect("in range"))
                .collect();
            assert_eq!(got, want, "{rows}x{cols}");
        }
    }

    #[test]
    fn the_runtime_switch_is_off_by_default() {
        let _guard = kernel_switch_guard();
        if std::env::var("ARC_METAL_EXACT_GEMV").as_deref() != Ok("1") {
            assert!(!metal_exact_gemv_requested());
        }
    }

    #[test]
    fn the_hook_runs_only_inside_a_scope_and_only_for_resident_rows() {
        let _guard = kernel_switch_guard();
        let mut rng = Rng(0x0000_5C0E);
        let w = weights(&mut rng, 96, 200);
        let other = weights(&mut rng, 96, 200);
        let input: Vec<i64> = (0..200).map(|_| rng.symmetric(6_500_000)).collect();
        let want = cpu_scalar(&w, &input);
        let want_other = cpu_scalar(&other, &input);
        let metal = MetalModel::from_matrices(&[&w]).expect("upload");
        assert_eq!(metal.resident_matrices(), 1);
        assert_eq!(metal.resident_weight_bytes(), 96 * 200);

        let _switch = SwitchGuard::set(true);
        // Outside a scope: the CPU computes it.
        let before = metal_census();
        let mut out = vec![SENTINEL; 96];
        matmul_i8_canonical_rows(&w, &input, &mut out).expect("projection");
        let delta = metal_census().since(&before);
        assert_eq!(out, want);
        assert_eq!(delta.accepted, 0);
        assert!(delta.outside_scope >= 1);

        metal.run(|| {
            // Whole matrix, then a row range of it: the GPU computes both.
            let before = metal_census();
            let mut out = vec![SENTINEL; 96];
            matmul_i8_canonical_rows(&w, &input, &mut out).expect("projection");
            assert_eq!(out, want);
            let mut part = vec![SENTINEL; 37];
            matmul_i8_canonical_row_range(&w, 40, 77, &input, &mut part).expect("rows");
            assert_eq!(part, want[40..77]);
            let delta = metal_census().since(&before);
            assert_eq!(delta.accepted, 2, "{delta:?}");
            assert_eq!(delta.in_scope_fallbacks(), 0, "{delta:?}");

            // A matrix the scope did not upload: the CPU computes it.
            let before = metal_census();
            let mut out = vec![SENTINEL; 96];
            matmul_i8_canonical_rows(&other, &input, &mut out).expect("projection");
            assert_eq!(out, want_other);
            let delta = metal_census().since(&before);
            assert_eq!((delta.accepted, delta.not_resident), (0, 1), "{delta:?}");
        });

        // The scope ends with the closure.
        let before = metal_census();
        let mut out = vec![SENTINEL; 96];
        matmul_i8_canonical_rows(&w, &input, &mut out).expect("projection");
        assert_eq!(out, want);
        assert_eq!(metal_census().since(&before).accepted, 0);
    }
}

/// Benchmark for the 7B projection shapes. Run explicitly, in release:
/// `cargo test --release -p arc-inference --features metal-exact --lib
/// metal_gemv::bench -- --ignored --nocapture --test-threads=1`.
#[cfg(test)]
mod bench {
    use super::test_support::{Rng, cpu_scalar, weights};
    use super::*;
    use crate::canonical_simd::{self, kernel_switch_guard};
    use arc_gpu::metal_exact::Tile;
    use std::time::Instant;

    const READ_PROBE_BYTES: usize = 256 << 20;
    /// Llama-2-7B: (name, rows, cols, matrices per layer).
    const SHAPES: [(&str, usize, usize, usize); 4] = [
        ("q/k/v/o 4096x4096", 4096, 4096, 4),
        ("gate/up 11008x4096", 11008, 4096, 2),
        ("down 4096x11008", 4096, 11008, 1),
        ("lm_head 32000x4096", 32000, 4096, 0),
    ];

    fn median(mut samples: Vec<f64>) -> f64 {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    }

    fn time_wall(runs: usize, mut f: impl FnMut()) -> f64 {
        median(
            (0..runs)
                .map(|_| {
                    let start = Instant::now();
                    f();
                    start.elapsed().as_secs_f64()
                })
                .collect(),
        )
    }

    /// GPU seconds per pass, with enough repeats for about 0.2 s of GPU time.
    fn time_gpu(engine: &MetalExactGemv, items: &[(&ResidentMatrix, &[i64])], tile: Tile) -> f64 {
        let single = engine.time_batch(items, tile, 1).expect("timing");
        let repeats = ((0.2 / single.max(1e-6)) as usize).clamp(3, 200);
        engine.time_batch(items, tile, repeats).expect("timing")
    }

    #[test]
    #[ignore = "benchmark: run explicitly in release with --ignored --nocapture"]
    fn llama_7b_projection_benchmark() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let device = engine.report();
        let read = |storage| {
            let times = engine
                .measure_read_bandwidth(READ_PROBE_BYTES, 10, storage)
                .expect("read probe");
            READ_PROBE_BYTES as f64 / times[0] / 1e9
        };
        let read_shared = read(Storage::Shared);
        let read_private = read(Storage::Private);
        let device_gbps = read_shared.max(read_private);
        let threads = rayon::current_num_threads();

        let mut rng = Rng(0x0007_BBE4_C400);
        let mut rows_md = Vec::new();
        let mut json_rows = Vec::new();
        let mut layer_items: Vec<(I8Weights, Vec<i64>)> = Vec::new();
        let mut lm_head_seconds = 0.0;
        let mut best_tiles = Vec::new();
        for (name, rows, cols, per_layer) in SHAPES {
            let w = weights(&mut rng, rows, cols);
            let x3: Vec<i64> = (0..cols).map(|_| rng.symmetric(6_500_000)).collect();
            let x4: Vec<i64> = (0..cols).map(|_| rng.symmetric(1 << 30)).collect();
            let matrix = engine
                .upload(&w.data, &w.scales, rows, cols, Storage::Shared)
                .expect("upload");
            let bytes = (rows * cols) as f64;

            // Exactness first: the benchmarked kernel must match the CPU.
            let want = cpu_scalar(&w, &x3);
            let mut got = vec![0i64; rows];
            engine.project(&matrix, &x3, &mut got).expect("in domain");
            assert_eq!(got, want, "{name}: GPU differs from the CPU scalar kernel");

            // Tile sweep on the three-plane input.
            let mut sweep: Vec<(Tile, f64)> = Tile::all()
                .into_iter()
                .map(|tile| (tile, time_gpu(&engine, &[(&matrix, x3.as_slice())], tile)))
                .collect();
            sweep.sort_by(|a, b| a.1.total_cmp(&b.1));
            let (best, best_seconds) = sweep[0];
            let default_seconds = time_gpu(&engine, &[(&matrix, x3.as_slice())], Tile::DEFAULT);
            let four_plane_seconds = time_gpu(&engine, &[(&matrix, x4.as_slice())], best);
            best_tiles.push(best);
            let mut out = vec![0i64; rows];
            let call_seconds = time_wall(20, || {
                engine.project(&matrix, &x3, &mut out).expect("in domain");
            });
            let mut cpu_out = vec![0i64; rows];
            let cpu_scalar_seconds = time_wall(3, || {
                crate::cached_integer_model::matmul_i8_canonical_rows(&w, &x3, &mut cpu_out)
                    .expect("in domain");
            });
            let cpu_simd_seconds = time_wall(5, || {
                assert!(canonical_simd::matmul_i8_canonical_rows_fast(
                    &w,
                    &x3,
                    cols,
                    &mut cpu_out
                ));
            });
            let gbps = bytes / best_seconds / 1e9;
            rows_md.push(format!(
                "| {name} | {} | {:.1} | {gbps:.1} | {:.0}% | {:.1} | {:.1} | {:.1} | {:.0} | {:.0} |",
                best.label(),
                best_seconds * 1e6,
                100.0 * gbps / device_gbps,
                default_seconds * 1e6,
                four_plane_seconds * 1e6,
                call_seconds * 1e6,
                cpu_scalar_seconds * 1e6,
                cpu_simd_seconds * 1e6,
            ));
            json_rows.push(serde_json::json!({
                "shape": name,
                "rows": rows,
                "cols": cols,
                "best_tile": best.label(),
                "gpu_us_3_planes": best_seconds * 1e6,
                "gpu_gbps_3_planes": gbps,
                "fraction_of_measured_read_bandwidth": gbps / device_gbps,
                "gpu_us_default_tile": default_seconds * 1e6,
                "gpu_us_4_planes": four_plane_seconds * 1e6,
                "single_call_wall_us": call_seconds * 1e6,
                "cpu_scalar_us": cpu_scalar_seconds * 1e6,
                "cpu_simd_us": cpu_simd_seconds * 1e6,
                "tile_sweep_us": sweep
                    .iter()
                    .map(|(tile, seconds)| (tile.label(), seconds * 1e6))
                    .collect::<Vec<_>>(),
            }));
            if per_layer == 0 {
                lm_head_seconds = best_seconds;
            }
            for _ in 0..per_layer {
                let fresh = weights(&mut rng, rows, cols);
                layer_items.push((fresh, x3.clone()));
            }
        }

        // One decoder layer: seven distinct matrices, one command buffer.
        let resident: Vec<(ResidentMatrix, &[i64])> = layer_items
            .iter()
            .map(|(w, x)| {
                let m = engine
                    .upload(&w.data, &w.scales, w.n_rows, w.n_cols, Storage::Shared)
                    .expect("upload");
                (m, x.as_slice())
            })
            .collect();
        let items: Vec<(&ResidentMatrix, &[i64])> = resident.iter().map(|(m, x)| (m, *x)).collect();
        let layer_bytes: usize = resident.iter().map(|(m, _)| m.weight_bytes()).sum();
        let layer_tile = best_tiles[0];
        let layer_seconds = time_gpu(&engine, &items, layer_tile);
        let forward_seconds = 32.0 * layer_seconds + lm_head_seconds;
        let forward_bytes = 32.0 * layer_bytes as f64 + 32000.0 * 4096.0;

        let mut md = String::new();
        md.push_str("### Exact Metal GEMV, Llama-2-7B projection shapes\n\n");
        md.push_str(
            "Virtualized-runner measurement: GitHub-hosted macOS VM with a paravirtual \
             Metal GPU. Not Apple GPU hardware numbers.\n\n",
        );
        md.push_str(&format!(
            "Device `{}`; max buffer {} bytes; measured read bandwidth {read_shared:.1} GB/s \
             (shared) and {read_private:.1} GB/s (private), 256 MiB, best of 10. CPU columns \
             use {threads} rayon threads.\n\n",
            device.name, device.max_buffer_length,
        ));
        md.push_str(
            "| Shape | Best tile | GPU µs | GB/s | of measured BW | GPU µs, default tile \
             | GPU µs, 4 planes | One call, wall µs | CPU scalar µs | CPU SIMD µs |\n",
        );
        md.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
        for row in &rows_md {
            md.push_str(row);
            md.push('\n');
        }
        md.push_str(&format!(
            "\nOne decoder layer (7 projections, {layer_bytes} weight bytes, tile {}): \
             {:.1} µs of GPU time, {:.1} GB/s. Derived projection-only forward (32 layers + \
             LM head, encoded back to back): {:.2} ms, i.e. at most {:.1} tok/s from \
             projections alone on this device.\n",
            layer_tile.label(),
            layer_seconds * 1e6,
            layer_bytes as f64 / layer_seconds / 1e9,
            forward_seconds * 1e3,
            1.0 / forward_seconds,
        ));
        println!("{md}");
        let json = serde_json::json!({
            "label": "virtualized-runner measurement",
            "device": device,
            "read_gbps_shared": read_shared,
            "read_gbps_private": read_private,
            "rayon_threads": threads,
            "shapes": json_rows,
            "layer": {
                "tile": layer_tile.label(),
                "weight_bytes": layer_bytes,
                "gpu_us": layer_seconds * 1e6,
                "gbps": layer_bytes as f64 / layer_seconds / 1e9,
            },
            "derived_forward": {
                "projection_ms": forward_seconds * 1e3,
                "weight_bytes": forward_bytes,
                "tok_s_ceiling_projections_only": 1.0 / forward_seconds,
            },
        });
        println!("METAL_EXACT_GEMV_BENCH {json}");
        if let Ok(path) = std::env::var("ARC_METAL_BENCH_MD") {
            std::fs::write(path, md).expect("write the benchmark summary");
        }
    }
}
