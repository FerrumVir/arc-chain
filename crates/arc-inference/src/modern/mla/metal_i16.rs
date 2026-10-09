//! Opt-in exact Metal GEMV for the INT16 projection of the MLA + MoE profile.
//!
//! The kernel, its exactness proof and its self-test live in
//! [`arc_gpu::metal_exact_i16`]. This module connects it to the profile's one
//! INT16 projection entry point, [`super::precision::project_i16`] (reached by
//! every INT16 query, KV, per-head key and value, output, dense or
//! shared-expert FFN and LM-head projection of [`super::model::StageModel`]),
//! behind three switches that are all off by default:
//!
//! 1. the cargo feature `metal-exact` (without it this module does not exist);
//! 2. the runtime switch `ARC_METAL_EXACT_I16=1` or [`set_metal_exact_i16`];
//! 3. a residency scope: projections run on the GPU only inside
//!    [`MetalI16Model::run`], and only for weights that [`MetalI16Model`]
//!    uploaded.
//!
//! # What runs where
//!
//! The hook sits inside `project_i16`, after its own checks (shape, the
//! accumulator guard `sum|x| < floor(2^63 / 32767)`, the scale domain) and
//! before its row dots. The GPU computes the exact row dots, or refuses
//! without writing and leaves them to the CPU kernels. Either way the CPU then
//! applies the same epilogue, `floor(dot * mu / 2^k)` in i128 with the 2^62
//! output check (`arith::dyadic_epilogue`): Metal has no 128-bit integer type,
//! and sharing the CPU's function makes the epilogue identical by
//! construction. So a projection refuses exactly when the CPU refuses, with
//! the CPU's error: every refusal condition is checked by the same code
//! before or after the hook, and a GPU refusal only hands the dots back.
//!
//! # Why a resident copy cannot go stale
//!
//! As for the INT8 GEMV (`crate::metal_gemv`): the device holds a copy of each
//! matrix taken when [`MetalI16Model`] is built, and a projection uses it only
//! when the call's weight bytes start at an address inside a copied matrix, at
//! a whole row. `MetalI16Model<'a>` borrows the model (or the weights)
//! immutably for `'a`, so while it exists safe code can neither modify, move
//! nor free those bytes, and no other live allocation can occupy their
//! addresses. A memory-mapped package changed on disk after it was loaded is
//! already outside the profile (`INT16-CONTRACT.md`); the copy keeps the
//! bytes that were admitted. The scope is a closure call: the thread-local that
//! activates it is reset when the closure returns or unwinds.
//!
//! The scope is per thread. Projections on rayon workers, such as the heads
//! of a K2.6-sized INT16 layer (`Schedule::Pool`), are outside it and stay on
//! the CPU, counted as `outside_scope`.
//!
//! # Head batches
//!
//! With a fourth switch, `ARC_METAL_EXACT_I16_HEADS=1` or
//! [`set_metal_exact_i16_heads`] (also off by default), an INT16 layer's heads
//! leave the per-projection hook: [`try_heads`] runs every head's `wk_b`
//! projection in one dispatch, the CPU's RoPE and attention for every head,
//! and every head's `wv_b` projection in one dispatch, in one command buffer
//! per layer ([`set_metal_i16_heads_submission`] picks the submission; every
//! submission computes the same integers). The epilogues are the CPU's
//! `arith::dyadic_epilogue`, and the attention is the CPU's own code. If any
//! head would refuse anywhere, the batch declines without writing, and the
//! per-head loop computes the layer, so it returns the CPU's error.
//!
//! Wiring this as a worker default is a separate, reviewed step.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use arc_gpu::metal_exact::Storage;
use arc_gpu::metal_exact_i16::{HeadPhase, MetalExactI16, Refusal, ResidentI16, Submission};
use rayon::prelude::*;

use super::model::StageModel;
use super::precision::I16Weights;
use crate::modern::ModernError;
use crate::modern::arith::dyadic_epilogue;

static REQUESTED: AtomicBool = AtomicBool::new(false);
static HEADS_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Index into `Submission::ALL`; 0 is one command buffer per layer.
static HEADS_SUBMISSION: AtomicU8 = AtomicU8::new(0);
static ENV_INIT: OnceLock<()> = OnceLock::new();
static ENGINE: OnceLock<Result<Arc<MetalExactI16>, String>> = OnceLock::new();

static ATTEMPTED: AtomicU64 = AtomicU64::new(0);
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static OUTSIDE_SCOPE: AtomicU64 = AtomicU64::new(0);
static NOT_RESIDENT: AtomicU64 = AtomicU64::new(0);
static REFUSED_SHAPE: AtomicU64 = AtomicU64::new(0);
static REFUSED_GUARD: AtomicU64 = AtomicU64::new(0);
static REFUSED_SPLIT: AtomicU64 = AtomicU64::new(0);
static DEVICE_ERRORS: AtomicU64 = AtomicU64::new(0);
static HEADS_ATTEMPTED: AtomicU64 = AtomicU64::new(0);
static HEADS_ACCEPTED: AtomicU64 = AtomicU64::new(0);
static HEADS_OUTSIDE_SCOPE: AtomicU64 = AtomicU64::new(0);
static HEADS_DECLINED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The residency scope active on this thread, set only by
    /// [`MetalI16Model::run`] for the duration of its closure.
    static ACTIVE: Cell<*const Residency> = const { Cell::new(std::ptr::null()) };
}

fn env_init() {
    ENV_INIT.get_or_init(|| {
        if std::env::var("ARC_METAL_EXACT_I16").as_deref() == Ok("1") {
            REQUESTED.store(true, Ordering::Relaxed);
        }
        if std::env::var("ARC_METAL_EXACT_I16_HEADS").as_deref() == Ok("1") {
            HEADS_REQUESTED.store(true, Ordering::Relaxed);
        }
    });
}

/// Turn the exact INT16 Metal GEMV on or off process-wide. Default off; an
/// explicit call overrides `ARC_METAL_EXACT_I16`.
pub fn set_metal_exact_i16(enabled: bool) {
    env_init();
    REQUESTED.store(enabled, Ordering::Relaxed);
}

/// Whether the runtime switch is on. Projections still need a residency scope.
pub fn metal_exact_i16_requested() -> bool {
    env_init();
    REQUESTED.load(Ordering::Relaxed)
}

/// Turn head batches on or off process-wide. Default off; an explicit call
/// overrides `ARC_METAL_EXACT_I16_HEADS`. They also need the INT16 switch and
/// a residency scope.
pub fn set_metal_exact_i16_heads(enabled: bool) {
    env_init();
    HEADS_REQUESTED.store(enabled, Ordering::Relaxed);
}

/// Whether the head-batch switch is on.
pub fn metal_exact_i16_heads_requested() -> bool {
    env_init();
    HEADS_REQUESTED.load(Ordering::Relaxed)
}

/// How head batches are submitted (default: one command buffer per layer).
pub fn set_metal_i16_heads_submission(submission: Submission) {
    let index = Submission::ALL
        .iter()
        .position(|&s| s == submission)
        .unwrap_or(0);
    HEADS_SUBMISSION.store(index as u8, Ordering::Relaxed);
}

/// The submission head batches use.
pub fn metal_i16_heads_submission() -> Submission {
    Submission::ALL[usize::from(HEADS_SUBMISSION.load(Ordering::Relaxed)) % Submission::ALL.len()]
}

/// The process-wide engine, created and self-tested on first use.
pub fn metal_i16_engine() -> Result<Arc<MetalExactI16>, String> {
    ENGINE
        .get_or_init(|| MetalExactI16::new().map(Arc::new))
        .clone()
}

/// Counts of INT16 projections that reached the Metal hook (switch on).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct MetalI16Census {
    pub attempted: u64,
    pub accepted: u64,
    /// No residency scope on the calling thread.
    pub outside_scope: u64,
    /// Inside a scope, but the weights were not uploaded by it.
    pub not_resident: u64,
    pub refused_shape: u64,
    pub refused_accumulator_bound: u64,
    pub refused_digit_split: u64,
    pub device_errors: u64,
    /// Layers whose heads reached the head-batch hook (both switches on).
    pub heads_attempted: u64,
    /// Layers whose heads the GPU batch computed.
    pub heads_accepted: u64,
    /// No residency scope on the calling thread.
    pub heads_outside_scope: u64,
    /// Inside a scope, but the batch declined and the per-head loop ran.
    pub heads_declined: u64,
}

impl MetalI16Census {
    /// Projections inside a scope whose dots the CPU computed instead.
    pub fn in_scope_fallbacks(&self) -> u64 {
        self.not_resident
            + self.refused_shape
            + self.refused_accumulator_bound
            + self.refused_digit_split
            + self.device_errors
    }

    /// Field-wise difference from an earlier reading.
    pub fn since(&self, earlier: &MetalI16Census) -> MetalI16Census {
        MetalI16Census {
            attempted: self.attempted - earlier.attempted,
            accepted: self.accepted - earlier.accepted,
            outside_scope: self.outside_scope - earlier.outside_scope,
            not_resident: self.not_resident - earlier.not_resident,
            refused_shape: self.refused_shape - earlier.refused_shape,
            refused_accumulator_bound: self.refused_accumulator_bound
                - earlier.refused_accumulator_bound,
            refused_digit_split: self.refused_digit_split - earlier.refused_digit_split,
            device_errors: self.device_errors - earlier.device_errors,
            heads_attempted: self.heads_attempted - earlier.heads_attempted,
            heads_accepted: self.heads_accepted - earlier.heads_accepted,
            heads_outside_scope: self.heads_outside_scope - earlier.heads_outside_scope,
            heads_declined: self.heads_declined - earlier.heads_declined,
        }
    }
}

/// Current counts. Counting is always on; it costs one relaxed atomic add per
/// projection that reaches the hook, and nothing when the switch is off.
pub fn metal_i16_census() -> MetalI16Census {
    MetalI16Census {
        attempted: ATTEMPTED.load(Ordering::Relaxed),
        accepted: ACCEPTED.load(Ordering::Relaxed),
        outside_scope: OUTSIDE_SCOPE.load(Ordering::Relaxed),
        not_resident: NOT_RESIDENT.load(Ordering::Relaxed),
        refused_shape: REFUSED_SHAPE.load(Ordering::Relaxed),
        refused_accumulator_bound: REFUSED_GUARD.load(Ordering::Relaxed),
        refused_digit_split: REFUSED_SPLIT.load(Ordering::Relaxed),
        device_errors: DEVICE_ERRORS.load(Ordering::Relaxed),
        heads_attempted: HEADS_ATTEMPTED.load(Ordering::Relaxed),
        heads_accepted: HEADS_ACCEPTED.load(Ordering::Relaxed),
        heads_outside_scope: HEADS_OUTSIDE_SCOPE.load(Ordering::Relaxed),
        heads_declined: HEADS_DECLINED.load(Ordering::Relaxed),
    }
}

fn count(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// One uploaded matrix and the address of the bytes it was copied from.
struct Resident {
    data: usize,
    n_rows: usize,
    n_cols: usize,
    matrix: ResidentI16,
}

struct Residency {
    engine: Arc<MetalExactI16>,
    matrices: Vec<Resident>,
}

impl Residency {
    /// The resident matrix that `weights` (`rows` x `cols`, little endian) is
    /// a contiguous row range of, and the first row of that range.
    fn find(&self, weights: &[u8], rows: usize, cols: usize) -> Option<(&Resident, usize)> {
        let data = weights.as_ptr() as usize;
        let row_bytes = cols.checked_mul(2)?;
        if rows.checked_mul(row_bytes) != Some(weights.len()) {
            return None;
        }
        self.matrices.iter().find_map(|resident| {
            if cols != resident.n_cols || data < resident.data {
                return None;
            }
            let offset = data - resident.data;
            if !offset.is_multiple_of(row_bytes) {
                return None;
            }
            let start = offset / row_bytes;
            let end = start.checked_add(rows)?;
            (end <= resident.n_rows).then_some((resident, start))
        })
    }
}

/// Device-resident copies of a stage's INT16 projection matrices.
///
/// Borrows the weights immutably for `'a`; see the module documentation for
/// why that keeps every copy equal to the bytes the CPU would read.
pub struct MetalI16Model<'a> {
    residency: Residency,
    _weights: PhantomData<&'a [u8]>,
}

impl<'a> MetalI16Model<'a> {
    /// Upload every INT16 matrix that `model` projects (each stack, such as
    /// the per-head key and value matrices, as one matrix). INT8 matrices and
    /// the embedding are not uploaded.
    pub fn new(model: &'a StageModel) -> Result<Self, String> {
        Self::from_weights(&model.int16_projection_matrices())
    }

    /// Upload the given matrices: admitted weights, rows, columns.
    pub fn from_weights(weights: &[(I16Weights<'a>, usize, usize)]) -> Result<Self, String> {
        let engine = metal_i16_engine()?;
        let mut matrices = Vec::with_capacity(weights.len());
        for &(w, n_rows, n_cols) in weights {
            let bytes = w.as_bytes();
            let matrix = engine.upload(bytes, n_rows, n_cols, Storage::Shared)?;
            matrices.push(Resident {
                data: bytes.as_ptr() as usize,
                n_rows,
                n_cols,
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

    /// INT16 weight bytes on the device.
    pub fn resident_weight_bytes(&self) -> usize {
        self.residency
            .matrices
            .iter()
            .map(|resident| resident.matrix.weight_bytes())
            .sum()
    }

    /// Run `f` with this residency scope active on the current thread. While
    /// it runs, and the runtime switch is on, INT16 projections of resident
    /// weights on this thread compute their dots on the GPU. Scopes nest; the
    /// previous one is restored when `f` returns or unwinds.
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

/// The hook in `project_i16`, called after its shape, guard and scale checks.
/// Returns `true` only if `dots` now holds the exact row dots of `q` (`rows` x
/// `cols`, little endian) against `x`; on `false` nothing was written and the
/// caller computes them on the CPU.
pub(crate) fn try_dots(q: &[u8], rows: usize, cols: usize, x: &[i64], dots: &mut [i64]) -> bool {
    count(&ATTEMPTED);
    let active = ACTIVE.with(Cell::get);
    if active.is_null() {
        count(&OUTSIDE_SCOPE);
        return false;
    }
    // SAFETY: ACTIVE is non-null only while `MetalI16Model::run` is executing
    // on this thread, and `run` holds `&self` for the whole call, so the
    // `Residency` it points to is alive and not mutated.
    let residency = unsafe { &*active };
    if rows == 0 || x.len() != cols || dots.len() != rows {
        count(&REFUSED_SHAPE);
        return false;
    }
    let Some((resident, start)) = residency.find(q, rows, cols) else {
        count(&NOT_RESIDENT);
        return false;
    };
    match residency
        .engine
        .dot_rows(&resident.matrix, start..start + rows, x, dots)
    {
        Ok(_) => {
            count(&ACCEPTED);
            true
        }
        Err(refusal) => {
            count(match refusal {
                Refusal::Shape => &REFUSED_SHAPE,
                Refusal::AccumulatorBound => &REFUSED_GUARD,
                Refusal::DigitSplit => &REFUSED_SPLIT,
                Refusal::Device => &DEVICE_ERRORS,
            });
            false
        }
    }
}

/// One per-head INT16 stack of an MLA layer (`wk_b` or `wv_b`): `heads`
/// matrices of `rows` x `cols`, one after another, with their row scales.
#[derive(Clone, Copy)]
pub(crate) struct HeadStack<'a> {
    pub(crate) weights: I16Weights<'a>,
    pub(crate) heads: usize,
    pub(crate) rows: usize,
    pub(crate) cols: usize,
    pub(crate) mu: &'a [i32],
    pub(crate) k: &'a [u8],
}

impl HeadStack<'_> {
    /// The shapes `project_i16` checks for each head, and its scale domain
    /// on every row.
    fn admissible(&self) -> bool {
        let rows = self.heads.checked_mul(self.rows);
        let bytes = rows
            .and_then(|rows| rows.checked_mul(self.cols))
            .and_then(|weights| weights.checked_mul(2));
        self.heads > 0
            && self.rows > 0
            && self.cols > 0
            && bytes == Some(self.weights.as_bytes().len())
            && rows == Some(self.mu.len())
            && rows == Some(self.k.len())
            && self
                .mu
                .iter()
                .zip(self.k)
                .all(|(&m, &s)| (m == 0 && s == 16) || (m >= 1 << 30 && (16..=62).contains(&s)))
    }
}

/// The dyadic epilogue of `dots` with row scales `mu` and `k`, or `None` if
/// one output is beyond 2^62.
fn epilogue_rows(dots: &[i64], mu: &[i32], k: &[u8]) -> Option<Vec<i64>> {
    dots.iter()
        .zip(mu.iter().zip(k))
        .map(|(&dot, (&m, &s))| dyadic_epilogue(dot, m, s).ok())
        .collect()
}

/// The heads of one MLA layer as a GPU batch: every head's `key` (`wk_b`)
/// projection of its `queries` vector, then `attend(head, qa)` on the CPU
/// (RoPE and absorbed attention, giving the head's latent `u`), then every
/// head's `value` (`wv_b`) projection of `u`, into `out` (heads x
/// `value.rows`, one head after another).
///
/// Returns `true` only if `out` now holds exactly what the per-head loop of
/// `layer_forward` computes: the GPU computes exact dots, the epilogues are
/// `arith::dyadic_epilogue` and the attention is `attend` itself. If any head
/// would refuse at any step (shape, guard, scale domain, epilogue, RoPE,
/// attention), or the GPU refuses, nothing is written and the caller runs the
/// per-head loop, which then returns the CPU's error.
pub(crate) fn try_heads<A>(
    key: HeadStack<'_>,
    queries: &[i64],
    value: HeadStack<'_>,
    attend: A,
    out: &mut [i64],
) -> bool
where
    A: Fn(usize, &[i64]) -> Result<Vec<i64>, ModernError> + Sync,
{
    if !(metal_exact_i16_requested() && metal_exact_i16_heads_requested()) {
        return false;
    }
    count(&HEADS_ATTEMPTED);
    let active = ACTIVE.with(Cell::get);
    if active.is_null() {
        count(&HEADS_OUTSIDE_SCOPE);
        return false;
    }
    // SAFETY: ACTIVE is non-null only while `MetalI16Model::run` is executing
    // on this thread, and `run` holds `&self` for the whole call, so the
    // `Residency` it points to is alive and not mutated.
    let residency = unsafe { &*active };
    let decline = || {
        count(&HEADS_DECLINED);
        false
    };
    let heads = key.heads;
    if !key.admissible()
        || !value.admissible()
        || value.heads != heads
        || value.cols != key.rows
        || heads.checked_mul(key.cols) != Some(queries.len())
        || heads.checked_mul(value.rows) != Some(out.len())
    {
        return decline();
    }
    let (Some((key_matrix, key_start)), Some((value_matrix, value_start))) = (
        residency.find(key.weights.as_bytes(), heads * key.rows, key.cols),
        residency.find(value.weights.as_bytes(), heads * value.rows, value.cols),
    ) else {
        return decline();
    };
    let a = HeadPhase {
        matrix: &key_matrix.matrix,
        first_row: key_start,
        heads,
        rows: key.rows,
    };
    let b = HeadPhase {
        matrix: &value_matrix.matrix,
        first_row: value_start,
        heads,
        rows: value.rows,
    };
    // The CPU step between the phases: every head's key epilogue and its
    // attention, in parallel over heads (each head reads only shared inputs
    // and its own dots). Any refusal declines the whole batch.
    let between = |dots: &[i64]| -> Option<Vec<i64>> {
        let latents: Vec<Option<Vec<i64>>> = dots
            .par_chunks(key.rows)
            .enumerate()
            .map(|(head, head_dots)| {
                let rows = head * key.rows..(head + 1) * key.rows;
                let qa = epilogue_rows(head_dots, &key.mu[rows.clone()], &key.k[rows])?;
                let u = attend(head, &qa).ok()?;
                (u.len() == value.cols).then_some(u)
            })
            .collect();
        let mut inputs = Vec::with_capacity(heads * value.cols);
        for u in latents {
            inputs.extend(u?);
        }
        Some(inputs)
    };
    let engine = &residency.engine;
    let mut dots_a = vec![0i64; heads * key.rows];
    let mut dots_b = vec![0i64; heads * value.rows];
    if engine
        .dot_heads_two_phase(
            &a,
            queries,
            &b,
            between,
            &mut dots_a,
            &mut dots_b,
            engine.tile(),
            metal_i16_heads_submission(),
        )
        .is_err()
    {
        return decline();
    }
    let Some(result) = epilogue_rows(&dots_b, value.mu, value.k) else {
        return decline();
    };
    out.copy_from_slice(&result);
    count(&HEADS_ACCEPTED);
    true
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::super::model::for_each_head;
    use super::super::ops::{LatentCache, mla_attend, rope_interleaved};
    use super::super::precision::tests::{Case, Outcome};
    use super::super::precision::{Schedule, project_i16};
    use super::*;

    /// Turns the runtime switch on or off for one test and restores it after,
    /// even on panic. Hold `canonical_simd::kernel_switch_guard()` as well.
    pub(crate) struct SwitchGuard(bool);

    impl SwitchGuard {
        pub(crate) fn set(enabled: bool) -> Self {
            let previous = metal_exact_i16_requested();
            set_metal_exact_i16(enabled);
            SwitchGuard(previous)
        }
    }

    impl Drop for SwitchGuard {
        fn drop(&mut self) {
            set_metal_exact_i16(self.0);
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

        /// A valid scale mantissa, in `[2^30, 2^31)`.
        pub(crate) fn mu(&mut self) -> i32 {
            ((1u64 << 30) + self.next_u64() % (1 << 30)) as i32
        }
    }

    /// Little-endian bytes of `values`.
    pub(crate) fn le_bytes(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Random weights in `[-32767, 32767]`, with both extremes present.
    pub(crate) fn weights(rng: &mut Rng, len: usize) -> Vec<i16> {
        let mut values: Vec<i16> = (0..len).map(|_| rng.symmetric(32_767) as i16).collect();
        values[0] = 32_767;
        values[len - 1] = -32_767;
        values
    }

    /// `case` through the GPU hook: `project_i16` with the switch on, on the
    /// calling thread, inside a scope that uploaded `case.q` (as `rows` x
    /// `cols`, or as one row when that shape does not fit the bytes; the
    /// projection then refuses its shape before the hook). Returns the
    /// outcome and what the hook counted. Hold `kernel_switch_guard`.
    pub(crate) fn hooked(case: &Case<'_>) -> (Outcome, MetalI16Census) {
        let admitted = I16Weights::new(case.q).expect("admitted weights");
        let shape = if case.rows.checked_mul(case.cols).map(|n| 2 * n) == Some(case.q.len()) {
            (case.rows, case.cols)
        } else {
            (1, case.q.len() / 2)
        };
        let metal = MetalI16Model::from_weights(&[(admitted, shape.0, shape.1)]).expect("upload");
        let _switch = SwitchGuard::set(true);
        let before = metal_i16_census();
        let outcome = metal.run(|| case.current(Some(Schedule::Serial)));
        (outcome, metal_i16_census().since(&before))
    }

    /// Turns head batches on or off, with a submission, for one test and
    /// restores both after, even on panic. Hold `kernel_switch_guard`.
    pub(crate) struct HeadsGuard(bool, Submission);

    impl HeadsGuard {
        pub(crate) fn set(enabled: bool, submission: Submission) -> Self {
            let previous = HeadsGuard(
                metal_exact_i16_heads_requested(),
                metal_i16_heads_submission(),
            );
            set_metal_exact_i16_heads(enabled);
            set_metal_i16_heads_submission(submission);
            previous
        }
    }

    impl Drop for HeadsGuard {
        fn drop(&mut self) {
            set_metal_exact_i16_heads(self.0);
            set_metal_i16_heads_submission(self.1);
        }
    }

    /// One MLA layer's heads with random INT16 `wk_b` and `wv_b` stacks, a
    /// random latent cache and RoPE tables: the inputs of the per-head loop
    /// in `layer_forward`.
    #[derive(Clone)]
    pub(crate) struct LayerHeads {
        pub(crate) n_heads: usize,
        pub(crate) rank: usize,
        pub(crate) nope: usize,
        pub(crate) rope_dim: usize,
        pub(crate) v_dim: usize,
        pub(crate) positions: usize,
        pub(crate) wk: Vec<u8>,
        pub(crate) wv: Vec<u8>,
        pub(crate) mu_k: Vec<i32>,
        pub(crate) k_k: Vec<u8>,
        pub(crate) mu_v: Vec<i32>,
        pub(crate) k_v: Vec<u8>,
        /// `n_heads` queries of `nope + rope_dim` values.
        pub(crate) q: Vec<i64>,
        pub(crate) latent: Vec<i32>,
        pub(crate) rope_keys: Vec<i32>,
        pub(crate) cos: Vec<i32>,
        pub(crate) sin: Vec<i32>,
        pub(crate) lambda: i64,
    }

    impl LayerHeads {
        /// Random heads: Q16 queries up to 1, latents up to `magnitude`, RoPE
        /// keys up to 1/4, and K2.6's attention scale for the head width.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn new(
            rng: &mut Rng,
            n_heads: usize,
            rank: usize,
            nope: usize,
            rope_dim: usize,
            v_dim: usize,
            positions: usize,
            magnitude: i64,
        ) -> Self {
            let dqk = nope + rope_dim;
            LayerHeads {
                n_heads,
                rank,
                nope,
                rope_dim,
                v_dim,
                positions,
                wk: le_bytes(&weights(rng, n_heads * rank * nope)),
                wv: le_bytes(&weights(rng, n_heads * v_dim * rank)),
                mu_k: (0..n_heads * rank).map(|_| rng.mu()).collect(),
                k_k: (0..n_heads * rank).map(|r| 40 + (r % 23) as u8).collect(),
                mu_v: (0..n_heads * v_dim).map(|_| rng.mu()).collect(),
                k_v: (0..n_heads * v_dim).map(|r| 40 + (r % 23) as u8).collect(),
                q: (0..n_heads * dqk).map(|_| rng.symmetric(1 << 16)).collect(),
                latent: (0..positions * rank)
                    .map(|_| rng.symmetric(magnitude) as i32)
                    .collect(),
                rope_keys: (0..positions * rope_dim)
                    .map(|_| rng.symmetric(1 << 14) as i32)
                    .collect(),
                cos: (0..rope_dim / 2)
                    .map(|_| rng.symmetric(1 << 16) as i32)
                    .collect(),
                sin: (0..rope_dim / 2)
                    .map(|_| rng.symmetric(1 << 16) as i32)
                    .collect(),
                lambda: crate::modern::tables::attention_lambda(dqk),
            }
        }

        pub(crate) fn dqk(&self) -> usize {
            self.nope + self.rope_dim
        }

        pub(crate) fn view(&self) -> LatentCache<'_> {
            LatentCache {
                latent: &self.latent,
                rope_keys: &self.rope_keys,
                positions: self.positions,
                rank: self.rank,
                rope_dim: self.rope_dim,
            }
        }

        /// The `wk_b` and `wv_b` stacks.
        pub(crate) fn stacks(&self) -> (HeadStack<'_>, HeadStack<'_>) {
            (
                HeadStack {
                    weights: I16Weights::new(&self.wk).expect("admitted"),
                    heads: self.n_heads,
                    rows: self.rank,
                    cols: self.nope,
                    mu: &self.mu_k,
                    k: &self.k_k,
                },
                HeadStack {
                    weights: I16Weights::new(&self.wv).expect("admitted"),
                    heads: self.n_heads,
                    rows: self.v_dim,
                    cols: self.rank,
                    mu: &self.mu_v,
                    k: &self.k_v,
                },
            )
        }

        /// Every head's non-RoPE query, one after another.
        pub(crate) fn queries(&self) -> Vec<i64> {
            self.q
                .chunks(self.dqk())
                .flat_map(|head| head.iter().take(self.nope).copied())
                .collect()
        }

        /// RoPE and absorbed attention of head `j`, as `layer_forward` runs
        /// them between the two projections.
        pub(crate) fn attend(&self, j: usize, qa: &[i64]) -> Result<Vec<i64>, ModernError> {
            let base = j * self.dqk();
            let mut qp = self.q[base + self.nope..base + self.dqk()].to_vec();
            rope_interleaved(&mut qp, &self.cos, &self.sin)?;
            let mut u = vec![0i64; self.rank];
            mla_attend(qa, &qp, self.view(), self.lambda, &mut u)?;
            Ok(u)
        }

        /// Head `j`'s `wk_b` projection, as `layer_forward` computes it.
        pub(crate) fn key(&self, key: &HeadStack<'_>, j: usize) -> Result<Vec<i64>, ModernError> {
            let rows = j * self.rank..(j + 1) * self.rank;
            let base = j * self.dqk();
            let mut qa = vec![0i64; self.rank];
            project_i16(
                key.weights.rows(rows.clone(), self.nope),
                self.rank,
                self.nope,
                &self.mu_k[rows.clone()],
                &self.k_k[rows],
                &self.q[base..base + self.nope],
                &mut qa,
            )?;
            Ok(qa)
        }

        /// The per-head loop of `layer_forward`, on the CPU, on `schedule`.
        pub(crate) fn cpu(&self, schedule: Schedule) -> Result<Vec<i64>, String> {
            let (key, value) = self.stacks();
            self.cpu_with(&key, &value, schedule)
        }

        /// [`Self::cpu`] with stacks admitted once (as the stage loader admits
        /// them), so a timed call does not rescan the weights.
        pub(crate) fn cpu_with(
            &self,
            key: &HeadStack<'_>,
            value: &HeadStack<'_>,
            schedule: Schedule,
        ) -> Result<Vec<i64>, String> {
            let mut out = vec![0i64; self.n_heads * self.v_dim];
            for_each_head(&mut out, self.v_dim, schedule, |j, block| {
                let qa = self.key(key, j)?;
                let u = self.attend(j, &qa)?;
                let rows = j * self.v_dim..(j + 1) * self.v_dim;
                project_i16(
                    value.weights.rows(rows.clone(), self.rank),
                    self.v_dim,
                    self.rank,
                    &self.mu_v[rows.clone()],
                    &self.k_v[rows],
                    &u,
                    block,
                )
            })
            .map_err(|e| e.to_string())?;
            Ok(out)
        }

        /// Every head's latent `u` (the second projection's input), on the
        /// CPU.
        pub(crate) fn latents(&self) -> Vec<i64> {
            let (key, _) = self.stacks();
            (0..self.n_heads)
                .flat_map(|j| {
                    let qa = self.key(&key, j).expect("in domain");
                    self.attend(j, &qa).expect("in domain")
                })
                .collect()
        }

        /// The heads as one GPU batch: `try_heads` with both switches on and
        /// `submission`, inside a scope that uploaded both stacks. Returns
        /// whether it batched, the output (sentinels where nothing was
        /// written) and what the hooks counted. Hold `kernel_switch_guard`.
        pub(crate) fn gpu(&self, submission: Submission) -> (bool, Vec<i64>, MetalI16Census) {
            let (key, value) = self.stacks();
            let metal = MetalI16Model::from_weights(&[
                (key.weights, key.heads * key.rows, key.cols),
                (value.weights, value.heads * value.rows, value.cols),
            ])
            .expect("upload");
            let _switch = SwitchGuard::set(true);
            let _heads = HeadsGuard::set(true, submission);
            let queries = self.queries();
            let mut out = vec![0x7E57_7E57_7E57_7E57i64; self.n_heads * self.v_dim];
            let before = metal_i16_census();
            let batched =
                metal.run(|| try_heads(key, &queries, value, |j, qa| self.attend(j, qa), &mut out));
            (batched, out, metal_i16_census().since(&before))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::precision::tests::{Case, Outcome};
    use super::super::precision::{Bits, Schedule, project_i16, quantize_matrix};
    use super::test_support::{
        HeadsGuard, LayerHeads, Rng, SwitchGuard, hooked, le_bytes, weights,
    };
    use super::*;
    use crate::canonical_simd::{
        self, LIMB_COUNT, LIMB_MAX, LIMB_MIN, kernel_switch_guard, set_fast_canonical_kernel,
    };
    use crate::modern::arith::dyadic_epilogue;
    use arc_gpu::metal_exact::Tile;
    use arc_gpu::metal_exact_i16::{
        ACCUMULATOR_GUARD, DEFAULT_TILE, MAX_PLANES, PLANE_MAX, PLANE_MIN, split_planes,
    };

    const SENTINEL: i64 = 0x7E57_7E57_7E57_7E57;
    /// The largest `sum |x|` the guard admits.
    const GUARD_MAX: i64 = (ACCUMULATOR_GUARD - 1) as i64;

    /// The CPU outcome of `case`: the legacy oracle, after checking that the
    /// current kernel (scalar and SIMD limbs, serial) agrees with it.
    fn cpu_outcome(case: &Case<'_>, what: &str) -> Outcome {
        set_fast_canonical_kernel(false);
        let want = case.legacy();
        for fast in [false, true] {
            set_fast_canonical_kernel(fast);
            assert_eq!(
                case.current(Some(Schedule::Serial)),
                want,
                "{what}: CPU, fast {fast}"
            );
        }
        set_fast_canonical_kernel(false);
        want
    }

    /// Dots from the engine with `tile`, finished with the CPU's epilogue:
    /// the hook's path with an explicit tile.
    fn engine_outcome(
        matrix: &ResidentI16,
        case: &Case<'_>,
        tile: Tile,
    ) -> Result<Vec<i64>, String> {
        let engine = metal_i16_engine().expect("Metal device");
        let mut dots = vec![SENTINEL; case.rows];
        engine
            .dot_rows_with(matrix, 0..case.rows, case.x, &mut dots, tile)
            .map_err(|refusal| format!("refused: {refusal}"))?;
        dots.iter()
            .zip(case.mu.iter().zip(case.k))
            .map(|(&dot, (&mu, &k))| dyadic_epilogue(dot, mu, k).map_err(|e| e.to_string()))
            .collect()
    }

    /// `case` on the GPU through the hook, and with every tile in `tiles`
    /// directly; each must equal `want`. In-domain cases must be accepted.
    fn assert_gpu_matches(case: &Case<'_>, want: &Outcome, tiles: &[Tile], what: &str) {
        let (got, delta) = hooked(case);
        assert_eq!(got, *want, "{what}: hook");
        assert_eq!(delta.in_scope_fallbacks(), 0, "{what}: {delta:?}");
        // Inputs past the guard (or the shape) are refused by `project_i16`
        // before the hook; every other input reaches the GPU.
        let reaches = ACCUMULATOR_GUARD
            > case
                .x
                .iter()
                .map(|v| u128::from(v.unsigned_abs()))
                .sum::<u128>();
        assert_eq!(delta.accepted, u64::from(reaches), "{what}: {delta:?}");
        if tiles.is_empty() || !reaches {
            return;
        }
        let engine = metal_i16_engine().expect("Metal device");
        let matrix = engine
            .upload(case.q, case.rows, case.cols, Storage::Shared)
            .expect("upload");
        for &tile in tiles {
            assert_eq!(
                engine_outcome(&matrix, case, tile),
                *want,
                "{what}: tile {}",
                tile.label()
            );
        }
    }

    /// Weights at every base-128 limb boundary, and both extremes.
    const EDGE_WEIGHTS: [i16; 19] = [
        32767, -32767, 0, 1, -1, 127, -127, 128, -128, 255, -256, 16383, -16383, 16384, -16384,
        16511, -16511, 32640, -32640,
    ];

    #[test]
    fn metal_i16_digit_planes_extend_the_cpu_limb_digits() {
        assert_eq!(MAX_PLANES, 7);
        assert_eq!(PLANE_MAX, 35_887_507_618_889_599);
        assert_eq!(PLANE_MIN, -36_170_086_419_038_336);
        let mut rng = Rng(0xD161_7516);
        let mut cases = vec![
            vec![
                0, 1, -1, 127, 128, -128, -129, 255, 256, -256, 32_767, -32_768,
            ],
            vec![LIMB_MAX, LIMB_MIN, LIMB_MAX - 1, LIMB_MIN + 1, 16_777_216],
            vec![LIMB_MAX + 1],
            vec![3, LIMB_MIN - 1, 5],
            vec![GUARD_MAX, -GUARD_MAX, PLANE_MAX, PLANE_MIN],
            vec![i64::MAX, 0],
            vec![i64::MIN],
        ];
        for round in 0..300 {
            let len = 1 + (rng.next_u64() % 70) as usize;
            let limit = [127, 32_639, 8_355_711, LIMB_MAX, 1 << 40, GUARD_MAX][round % 6];
            cases.push((0..len).map(|_| rng.symmetric(limit)).collect());
        }
        for input in cases {
            let n = input.len();
            let mut cpu = vec![0i8; LIMB_COUNT * n];
            let cpu_used = canonical_simd::split_limbs(&input, &mut cpu);
            let stride = n.div_ceil(8) * 8;
            let mut gpu = vec![0x33i8; MAX_PLANES * stride];
            let gpu_used = split_planes(&input, &mut gpu, stride);
            let in_seven = input.iter().all(|x| (PLANE_MIN..=PLANE_MAX).contains(x));
            assert_eq!(gpu_used.is_some(), in_seven, "{input:?}");
            if let Some(used) = cpu_used {
                // Inside the CPU's four digits: the same digits, then zeros.
                assert_eq!(gpu_used, Some(used), "{input:?}");
                let (low, high) = gpu.split_at(LIMB_COUNT * stride);
                for (cpu_plane, gpu_plane) in cpu.chunks_exact(n).zip(low.chunks_exact(stride)) {
                    assert_eq!(cpu_plane, &gpu_plane[..n], "{input:?}");
                }
                assert!(high.iter().all(|&c| c == 0), "{input:?}");
            }
            if gpu_used.is_some() {
                for (j, &x) in input.iter().enumerate() {
                    let rebuilt = (0..MAX_PLANES)
                        .rev()
                        .fold(0i64, |acc, d| acc * 256 + i64::from(gpu[d * stride + j]));
                    assert_eq!(rebuilt, x);
                }
            }
        }
    }

    /// #174's random and edge matrices (limb boundaries and +-32767, row-chunk
    /// edges 63/64/65, column-block edges 2047/2048/2049, zero rows, scales at
    /// k = 40 and 62 and mu = 2^30 and 2^31 - 1) against the legacy oracle
    /// and the current CPU kernel, through the hook and on every tile, for
    /// ordinary inputs, the CPU's four-digit edges and one past them, an input
    /// that needs all seven planes, the heaviest input the guard admits and
    /// the first one it refuses.
    #[test]
    fn metal_i16_matches_the_cpu_and_legacy_kernels_on_random_and_edge_matrices() {
        let _guard = kernel_switch_guard();
        let tiles = Tile::all();
        let mut rng = Rng(0x1560_F0F0_2DCA_0016);
        let shapes: [(usize, usize, &[usize]); 13] = [
            (1, 1, &[0, 1, 2]),
            (1, 3, &[0, 1, 2]),
            (3, 7, &[0, 1, 2]),
            (63, 5, &[0, 1, 2]),
            (64, 9, &[0, 1, 2]),
            (65, 2, &[0, 1, 2]),
            (130, 17, &[0, 1, 2]),
            (2, 2047, &[0, 1, 2]),
            (3, 2048, &[0, 1, 2]),
            (2, 2049, &[0, 1, 2]),
            (5, 4100, &[0, 1]),
            (1000, 300, &[0]),
            (70, 64, &[2]),
        ];
        let mut compared = 0usize;
        for (rows, cols, patterns) in shapes {
            for &pattern in patterns {
                let mut values: Vec<i16> = (0..rows * cols)
                    .map(|i| match pattern {
                        0 => rng.symmetric(32_767) as i16,
                        1 => EDGE_WEIGHTS[(i * 7 + i / cols) % EDGE_WEIGHTS.len()],
                        _ if (i / cols).is_multiple_of(2) => 32_767,
                        _ => -32_767,
                    })
                    .collect();
                let mut mu: Vec<i32> = (0..rows)
                    .map(|r| match r % 5 {
                        0 => i32::MAX,
                        1 => 1 << 30,
                        _ => rng.mu(),
                    })
                    .collect();
                let mut k: Vec<u8> = (0..rows)
                    .map(|r| match r % 3 {
                        0 => 62,
                        1 => 40,
                        _ => 40 + (rng.next_u64() % 23) as u8,
                    })
                    .collect();
                if pattern == 0 && rows > 2 {
                    values[cols..2 * cols].fill(0);
                    (mu[1], k[1]) = (0, 16);
                }
                let q = le_bytes(&values);
                // Row 0's signs, so every product of the heaviest input is positive.
                let heaviest: Vec<i64> = (0..cols)
                    .map(|j| {
                        let share = GUARD_MAX / cols as i64;
                        let magnitude = share + if j == 0 { GUARD_MAX % cols as i64 } else { 0 };
                        if values[j] < 0 { -magnitude } else { magnitude }
                    })
                    .collect();
                let mut refused = heaviest.clone();
                refused[0] += if refused[0] < 0 { -1 } else { 1 };
                let typical: Vec<i64> = (0..cols).map(|_| rng.symmetric(1 << 20)).collect();
                let edges: Vec<i64> = (0..cols)
                    .map(|j| match j % 4 {
                        0 => LIMB_MAX,
                        1 => LIMB_MIN,
                        2 => 0,
                        _ => -1,
                    })
                    .collect();
                let mut outside = typical.clone();
                outside[cols / 2] = LIMB_MAX + 1;
                let mut seven: Vec<i64> = (0..cols).map(|j| [1, -1][j % 2]).collect();
                seven[0] = -(GUARD_MAX - (cols as i64 - 1));
                for (name, x) in [
                    ("typical", &typical),
                    ("digit edges", &edges),
                    ("past four digits", &outside),
                    ("seven planes", &seven),
                    ("heaviest", &heaviest),
                    ("refused", &refused),
                ] {
                    let case = Case {
                        q: &q,
                        rows,
                        cols,
                        mu: &mu,
                        k: &k,
                        x,
                    };
                    let what = format!("{rows}x{cols} pattern {pattern} {name}");
                    let want = cpu_outcome(&case, &what);
                    assert_eq!(want.is_ok(), name != "refused", "{what}");
                    assert_gpu_matches(&case, &want, &tiles, &what);
                    compared += 1;
                }
            }
        }
        eprintln!(
            "{compared} INT16 cases byte-identical on the GPU (hook and {} tiles)",
            tiles.len()
        );
    }

    /// Every refusal of `project_i16` after admission (#174's cases, plus the
    /// accumulator guard and shapes) gives the CPU's error through the hook,
    /// and the engine itself refuses the guard and shapes without writing.
    #[test]
    fn metal_i16_refusals_match_the_cpu() {
        let _guard = kernel_switch_guard();
        let q = le_bytes(&[32767, -32767, 5, -5, 1, 0]);
        let x = [1i64 << 40, -(1 << 40)];
        let normal = [1 << 30; 3];
        for (mu, k, what) in [
            (
                [1 << 30, (1 << 30) - 1, 1 << 30],
                [40, 40, 40],
                "mu below 2^30",
            ),
            ([1 << 30, 0, 1 << 30], [40, 17, 40], "zero mu with k 17"),
            (normal, [40, 63, 40], "k 63"),
            (normal, [15, 40, 40], "k 15"),
            ([i32::MAX; 3], [16, 16, 16], "output beyond 2^62"),
        ] {
            let case = Case {
                q: &q,
                rows: 3,
                cols: 2,
                mu: &mu,
                k: &k,
                x: &x,
            };
            let want = cpu_outcome(&case, what);
            assert!(want.is_err(), "{what}");
            let (got, delta) = hooked(&case);
            assert_eq!(got, want, "{what}");
            // Only the epilogue's refusal comes after the hook.
            let after_hook = what == "output beyond 2^62";
            assert_eq!(delta.accepted, u64::from(after_hook), "{what}: {delta:?}");
        }
        let (mu, k) = ([1 << 30; 4], [40; 4]);
        let heavy = [GUARD_MAX, 1];
        for (rows, cols, x) in [
            (3, 2, &[1i64, 2, 3][..]),
            (4, 2, &[1, 2][..]),
            (2, 3, &[1, 2, 3][..]),
            (3, 2, &heavy[..]),
        ] {
            let case = Case {
                q: &q,
                rows,
                cols,
                mu: &mu[..rows],
                k: &k[..rows],
                x,
            };
            let what = format!("shape {rows}x{cols}, input {x:?}");
            let want = cpu_outcome(&case, &what);
            let (got, delta) = hooked(&case);
            assert_eq!(got, want, "{what}");
            assert_eq!(delta.accepted, u64::from(want.is_ok()), "{what}: {delta:?}");
        }

        let engine = metal_i16_engine().expect("Metal device");
        let matrix = engine.upload(&q, 3, 2, Storage::Shared).expect("upload");
        let mut out = [SENTINEL; 3];
        for (rows, input, refusal) in [
            (0..3, &[GUARD_MAX, 1][..], Refusal::AccumulatorBound),
            (0..3, &[i64::MIN, 0][..], Refusal::AccumulatorBound),
            (0..3, &[1, 2, 3][..], Refusal::Shape),
            (2..4, &[1, 2][..], Refusal::Shape),
            (1..1, &[1, 2][..], Refusal::Shape),
        ] {
            let len = rows.len();
            assert_eq!(
                engine.dot_rows(&matrix, rows, input, &mut out[..len]),
                Err(refusal),
                "{input:?}"
            );
            assert_eq!(out, [SENTINEL; 3], "a refusal must not write");
        }
        // -32768 never reaches the device: the engine's upload refuses it, as
        // I16Weights::new does.
        let minimum = le_bytes(&[3, i16::MIN, 5, 7]);
        assert!(I16Weights::new(&minimum).is_err());
        assert!(engine.upload(&minimum, 2, 2, Storage::Shared).is_err());
        assert!(engine.upload(&q[..5], 1, 2, Storage::Shared).is_err());
    }

    /// Matrices converted from BF16 by `quantize_matrix`, with row maxima at
    /// both edges of the INT16 admission window (2^-17 and just below 2^30,
    /// the BF16 bits 0x3700 and 0x4e7f, which give k = 62 and k = 16), their
    /// neighbours, 1.0, and zero rows, through the hook and on every tile.
    #[test]
    fn metal_i16_window_edge_matrices_match_the_cpu() {
        let _guard = kernel_switch_guard();
        let tiles = Tile::all();
        let mut rng = Rng(0x3700_4E7F);
        let maxima: [u16; 6] = [0x3700, 0x3701, 0x4e7e, 0x4e7f, 0x3f80, 0];
        for cols in [1usize, 9, 64, 300] {
            let rows = maxima.len() * 3;
            let mut bits = Vec::with_capacity(rows * cols);
            for r in 0..rows {
                let maximum = maxima[r % maxima.len()];
                let hit = (rng.next_u64() % cols as u64) as usize;
                for j in 0..cols {
                    let magnitude = if j == hit {
                        maximum
                    } else {
                        (rng.next_u64() % (u64::from(maximum) + 1)) as u16
                    };
                    let sign = if rng.next_u64().is_multiple_of(2) {
                        0
                    } else {
                        0x8000
                    };
                    bits.push(magnitude | sign);
                }
            }
            let m = quantize_matrix(&bits, rows, cols, Bits::Int16).expect("inside the window");
            assert!(m.k.contains(&62) && m.k.contains(&16), "{:?}", m.k);
            for (name, x) in [
                (
                    "typical",
                    (0..cols)
                        .map(|_| rng.symmetric(1 << 17))
                        .collect::<Vec<_>>(),
                ),
                (
                    "large",
                    (0..cols)
                        .map(|_| rng.symmetric(1 << 40))
                        .collect::<Vec<_>>(),
                ),
                ("heaviest", {
                    let mut x = vec![-1i64; cols];
                    x[0] = GUARD_MAX - (cols as i64 - 1);
                    x
                }),
            ] {
                let case = Case {
                    q: &m.q,
                    rows,
                    cols,
                    mu: &m.mu,
                    k: &m.k,
                    x: &x,
                };
                let what = format!("window edges, {rows}x{cols}, {name}");
                let want = cpu_outcome(&case, &what);
                assert_gpu_matches(&case, &want, &tiles, &what);
            }
        }
    }

    /// Pinned K2.6 shapes (`docs/protocol/reference/kimi-k26/config.json`)
    /// that fit in the runner's memory: a 16,384-row slice of the 163,840 x
    /// 7,168 LM head, the attention projections, the per-head key and value
    /// slices, the shared expert and the dense FFN of layer 0. Each against
    /// the legacy oracle and `project_i16` (scalar and SIMD) for Q16
    /// activations of magnitude up to 2 (three digit planes), up to 2^14 (four)
    /// and the heaviest input the guard admits (seven planes on the first
    /// column).
    #[test]
    fn metal_i16_k26_shapes_match_project_i16() {
        let _guard = kernel_switch_guard();
        let mut rng = Rng(0x0D15_EA5E_0000_0026);
        let tiles = [
            DEFAULT_TILE,
            Tile {
                rows_per_simdgroup: 8,
                simdgroups: 4,
                mul16: true,
            },
        ];
        for (name, rows, cols) in K26_SHAPES {
            let values = weights(&mut rng, rows * cols);
            let q = le_bytes(&values);
            drop(values);
            let mu: Vec<i32> = (0..rows).map(|_| rng.mu()).collect();
            let k = vec![46u8; rows];
            let small: Vec<i64> = (0..cols).map(|_| rng.symmetric(1 << 17)).collect();
            let large: Vec<i64> = (0..cols).map(|_| rng.symmetric(1 << 30)).collect();
            let mut heaviest = vec![1i64; cols];
            heaviest[0] = GUARD_MAX - (cols as i64 - 1);
            for (input, x) in [
                ("|x| <= 2^17", &small),
                ("|x| <= 2^30", &large),
                ("heaviest", &heaviest),
            ] {
                let case = Case {
                    q: &q,
                    rows,
                    cols,
                    mu: &mu,
                    k: &k,
                    x,
                };
                let what = format!("{name} {rows}x{cols}, {input}");
                let want = cpu_outcome(&case, &what);
                // With k = 46 every output of these inputs is inside 2^62.
                assert!(want.is_ok(), "{what}");
                assert_gpu_matches(&case, &want, &tiles, &what);
                eprintln!("{what}: identical on the GPU");
            }
        }
    }

    #[test]
    fn metal_i16_switch_is_off_by_default() {
        let _guard = kernel_switch_guard();
        if std::env::var("ARC_METAL_EXACT_I16").as_deref() != Ok("1") {
            assert!(!metal_exact_i16_requested());
        }
    }

    #[test]
    fn metal_i16_hook_runs_only_inside_a_scope_and_only_for_resident_rows() {
        let _guard = kernel_switch_guard();
        let mut rng = Rng(0x0000_5C0E_0016);
        let (rows, cols) = (96usize, 200usize);
        let w = le_bytes(&weights(&mut rng, rows * cols));
        let other = le_bytes(&weights(&mut rng, rows * cols));
        let mu: Vec<i32> = (0..rows).map(|_| rng.mu()).collect();
        let k = vec![40u8; rows];
        let x: Vec<i64> = (0..cols).map(|_| rng.symmetric(6_500_000)).collect();
        let w16 = I16Weights::new(&w).expect("admitted");
        let other16 = I16Weights::new(&other).expect("admitted");
        let project = |weights: I16Weights<'_>, rows: usize, mu: &[i32], k: &[u8]| {
            let mut out = vec![SENTINEL; rows];
            project_i16(weights, rows, cols, mu, k, &x, &mut out).expect("projection");
            out
        };
        let want = project(w16, rows, &mu, &k);
        let want_other = project(other16, rows, &mu, &k);
        let metal = MetalI16Model::from_weights(&[(w16, rows, cols)]).expect("upload");
        assert_eq!(metal.resident_matrices(), 1);
        assert_eq!(metal.resident_weight_bytes(), rows * cols * 2);

        let _switch = SwitchGuard::set(true);
        // Outside a scope: the CPU computes it.
        let before = metal_i16_census();
        assert_eq!(project(w16, rows, &mu, &k), want);
        let delta = metal_i16_census().since(&before);
        assert_eq!(delta.accepted, 0);
        assert!(delta.outside_scope >= 1);

        metal.run(|| {
            // The whole matrix, then a row range of it: the GPU computes both.
            let before = metal_i16_census();
            assert_eq!(project(w16, rows, &mu, &k), want);
            let part = project(w16.rows(40..77, cols), 37, &mu[40..77], &k[40..77]);
            assert_eq!(part, want[40..77]);
            let delta = metal_i16_census().since(&before);
            assert_eq!(delta.accepted, 2, "{delta:?}");
            assert_eq!(delta.in_scope_fallbacks(), 0, "{delta:?}");

            // Weights the scope did not upload: the CPU computes them.
            let before = metal_i16_census();
            assert_eq!(project(other16, rows, &mu, &k), want_other);
            let delta = metal_i16_census().since(&before);
            assert_eq!((delta.accepted, delta.not_resident), (0, 1), "{delta:?}");
        });

        // The scope ends with the closure.
        let before = metal_i16_census();
        assert_eq!(project(w16, rows, &mu, &k), want);
        assert_eq!(metal_i16_census().since(&before).accepted, 0);
    }

    /// A layer's heads as one GPU batch equal the per-head CPU loop of
    /// `layer_forward` (serial, and on the pool as #174 runs them) at K2.6's
    /// dims (64 heads, `wk_b` 512 x 128, `wv_b` 128 x 512) and at the tiny
    /// fixtures' dims, for every submission, with latents that need two or
    /// four digit planes. Nothing goes through the per-projection hook.
    #[test]
    fn metal_i16_head_batches_match_the_per_head_cpu_loop() {
        let _guard = kernel_switch_guard();
        let engine = metal_i16_engine().expect("Metal device");
        eprintln!(
            "one command buffer per layer on this device: {} ({:?})",
            engine.one_command_buffer(),
            engine.one_command_buffer_error()
        );
        let mut rng = Rng(0x4EAD_0016);
        for (n_heads, rank, nope, rope_dim, v_dim, positions) in [
            (64usize, 512usize, 128usize, 64usize, 128usize, 5usize),
            (4, 16, 8, 4, 8, 3),
            (4, 16, 128, 64, 8, 1),
        ] {
            for magnitude in [1i64 << 14, 1 << 28] {
                let layer = LayerHeads::new(
                    &mut rng, n_heads, rank, nope, rope_dim, v_dim, positions, magnitude,
                );
                let what = format!(
                    "{n_heads} heads of {rank}x{nope} and {v_dim}x{rank}, |latent| <= {magnitude}"
                );
                let want = layer.cpu(Schedule::Serial).expect("in domain");
                assert_eq!(layer.cpu(Schedule::Pool), Ok(want.clone()), "{what}: pool");
                for submission in Submission::ALL {
                    let (batched, got, delta) = layer.gpu(submission);
                    assert!(batched, "{what}: {submission:?} declined: {delta:?}");
                    assert_eq!(got, want, "{what}: {submission:?}");
                    assert_eq!(
                        (delta.heads_accepted, delta.heads_declined, delta.accepted),
                        (1, 0, 0),
                        "{what}: {submission:?}: {delta:?}"
                    );
                }
                eprintln!("{what}: identical in every submission");
            }
        }
    }

    /// Whatever the per-head loop refuses, the batch declines without
    /// writing, in every submission, and the loop then returns the CPU's
    /// error: an attention cache with no positions, RoPE tables of the wrong
    /// length, a query past the accumulator guard, a key output beyond 2^62
    /// (between the phases) and a value output beyond 2^62 (after them).
    #[test]
    fn metal_i16_head_batches_decline_whatever_the_cpu_refuses() {
        let _guard = kernel_switch_guard();
        let mut rng = Rng(0xDEC1_1E16);
        let base = LayerHeads::new(&mut rng, 4, 16, 8, 4, 8, 3, 1 << 14);
        let (dqk, nope, rank, v_dim) = (base.dqk(), base.nope, base.rank, base.v_dim);
        let mut cases = Vec::new();
        let mut layer = base.clone();
        layer.positions = 0;
        cases.push(("an attention cache with no positions", layer));
        let mut layer = base.clone();
        layer.cos.pop();
        cases.push(("RoPE tables of the wrong length", layer));
        let mut layer = base.clone();
        layer.q[2 * dqk] = GUARD_MAX + 1;
        cases.push(("a query past the accumulator guard", layer));
        // Head 1's key rows at k = 16 and the largest mu, against a query
        // whose products with row 0 are all positive.
        let mut layer = base.clone();
        for row in rank..2 * rank {
            (layer.mu_k[row], layer.k_k[row]) = (i32::MAX, 16);
        }
        for j in 0..nope {
            let w = i16::from_le_bytes([
                layer.wk[2 * (rank * nope + j)],
                layer.wk[2 * (rank * nope + j) + 1],
            ]);
            layer.q[dqk + j] = if w < 0 { -(1 << 40) } else { 1 << 40 };
        }
        cases.push(("a key output beyond 2^62", layer));
        // Head 0's first value row of +32767 at k = 16 and the largest mu,
        // against latents of 2^30.
        let mut layer = base.clone();
        layer.latent.fill(1 << 30);
        for byte in layer.wv[..2 * rank].chunks_exact_mut(2) {
            byte.copy_from_slice(&32_767i16.to_le_bytes());
        }
        for row in 0..v_dim {
            (layer.mu_v[row], layer.k_v[row]) = (i32::MAX, 16);
        }
        cases.push(("a value output beyond 2^62", layer));
        for (name, layer) in cases {
            let cpu = layer.cpu(Schedule::Serial);
            assert!(cpu.is_err(), "{name}: the CPU must refuse, got {cpu:?}");
            for submission in Submission::ALL {
                let (batched, got, delta) = layer.gpu(submission);
                assert!(!batched, "{name}: {submission:?}");
                assert!(
                    got.iter().all(|&v| v == SENTINEL),
                    "{name}: {submission:?}: a declined batch must not write"
                );
                assert_eq!(
                    (delta.heads_accepted, delta.heads_declined),
                    (0, 1),
                    "{name}: {submission:?}: {delta:?}"
                );
            }
            eprintln!("{name}: declined; the CPU loop returns {cpu:?}");
        }
    }

    /// The one-command-buffer handoff (phase A, a shared-event handoff to the
    /// CPU step, phase B) passed its start-up test on this device and passes
    /// it again on a fresh queue.
    #[test]
    fn metal_i16_one_command_buffer_handoff_is_exact_on_this_device() {
        let _guard = kernel_switch_guard();
        let engine = metal_i16_engine().expect("Metal device");
        let report = engine.self_test_one_buffer();
        eprintln!("one-command-buffer handoff self-test on a fresh queue: {report:?}");
        assert!(
            engine.one_command_buffer(),
            "the shared-event handoff failed its start-up test: {:?}",
            engine.one_command_buffer_error()
        );
        let report = report.expect("handoff self-test");
        assert!(report.compared > 0 && report.refused > 0, "{report:?}");
    }

    /// Head batches need the INT16 switch, the heads switch and a scope;
    /// without any of them the per-head loop runs, counted as such.
    #[test]
    fn metal_i16_head_batches_need_both_switches_and_a_scope() {
        let _guard = kernel_switch_guard();
        if std::env::var("ARC_METAL_EXACT_I16_HEADS").as_deref() != Ok("1") {
            assert!(!metal_exact_i16_heads_requested());
        }
        let mut rng = Rng(0x5C0E_4EAD);
        let layer = LayerHeads::new(&mut rng, 4, 16, 8, 4, 8, 2, 1 << 14);
        let (key, value) = layer.stacks();
        let queries = layer.queries();
        let mut out = vec![SENTINEL; 4 * 8];
        let attend = |j: usize, qa: &[i64]| layer.attend(j, qa);
        // Switches on, but no scope on this thread.
        let switch = SwitchGuard::set(true);
        let heads = HeadsGuard::set(true, Submission::OneCommandBuffer);
        let before = metal_i16_census();
        assert!(!try_heads(key, &queries, value, attend, &mut out));
        let delta = metal_i16_census().since(&before);
        assert_eq!(
            (delta.heads_attempted, delta.heads_outside_scope),
            (1, 1),
            "{delta:?}"
        );
        // Inside a scope, but the stacks were not uploaded by it.
        let other = LayerHeads::new(&mut rng, 4, 16, 8, 4, 8, 2, 1 << 14);
        let (other_key, other_value) = other.stacks();
        let metal = MetalI16Model::from_weights(&[
            (other_key.weights, 4 * 16, 8),
            (other_value.weights, 4 * 8, 16),
        ])
        .expect("upload");
        let before = metal_i16_census();
        assert!(!metal.run(|| try_heads(key, &queries, value, attend, &mut out)));
        let delta = metal_i16_census().since(&before);
        assert_eq!(
            (delta.heads_accepted, delta.heads_declined),
            (0, 1),
            "{delta:?}"
        );
        // The heads switch off: no attempt at all.
        drop(heads);
        let _heads = HeadsGuard::set(false, Submission::OneCommandBuffer);
        let before = metal_i16_census();
        assert!(!metal.run(|| try_heads(key, &queries, value, attend, &mut out)));
        assert_eq!(metal_i16_census().since(&before).heads_attempted, 0);
        drop(switch);
        assert!(out.iter().all(|&v| v == SENTINEL));
    }

    /// The pinned K2.6 shapes the tests and the benchmark use: (name, rows,
    /// columns).
    pub(super) const K26_SHAPES: [(&str, usize, usize); 11] = [
        ("lm_head slice", 16_384, 7_168),
        ("attention wo", 7_168, 8_192),
        ("query LoRA A", 1_536, 7_168),
        ("query LoRA B", 12_288, 1_536),
        ("KV LoRA A", 576, 7_168),
        ("wk_b per head", 512, 128),
        ("wv_b per head", 128, 512),
        ("shared expert gate/up", 2_048, 7_168),
        ("shared expert down", 7_168, 2_048),
        ("dense FFN gate/up (layer 0)", 18_432, 7_168),
        ("dense FFN down (layer 0)", 7_168, 18_432),
    ];
}

/// Benchmark for pinned K2.6 INT16 projection shapes. Run explicitly, in
/// release: `cargo test --release -p arc-inference --features metal-exact
/// --lib modern::mla::metal_i16::bench -- --ignored --nocapture
/// --test-threads=1`.
#[cfg(test)]
mod bench {
    use super::super::precision::{Schedule, project_i16};
    use super::test_support::{HeadsGuard, LayerHeads, Rng, SwitchGuard, le_bytes, weights};
    use super::tests::K26_SHAPES;
    use super::*;
    use crate::canonical_simd::{self, kernel_switch_guard, set_fast_canonical_kernel};
    use arc_gpu::metal_exact::Tile;
    use arc_gpu::metal_exact_i16::DEFAULT_TILE;
    use std::time::Instant;

    const READ_PROBE_BYTES: usize = 256 << 20;

    fn median(mut samples: Vec<f64>) -> f64 {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    }

    fn time_wall(runs: usize, mut f: impl FnMut()) -> f64 {
        f();
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
    fn time_gpu(engine: &MetalExactI16, items: &[(&ResidentI16, &[i64])], tile: Tile) -> f64 {
        let single = engine.time_batch(items, tile, 1).expect("timing");
        let repeats = ((0.2 / single.max(1e-6)) as usize).clamp(3, 200);
        engine.time_batch(items, tile, repeats).expect("timing")
    }

    /// One shape's measurements.
    struct Measured {
        name: &'static str,
        rows: usize,
        cols: usize,
        best: Tile,
        best_seconds: f64,
        default_seconds: f64,
        five_plane_seconds: f64,
        call_seconds: f64,
        cpu_scalar_seconds: f64,
        cpu_simd_seconds: Option<f64>,
        sweep: Vec<(Tile, f64)>,
    }

    #[test]
    #[ignore = "benchmark: run explicitly in release with --ignored --nocapture"]
    fn k26_int16_projection_benchmark() {
        let _guard = kernel_switch_guard();
        let engine = metal_i16_engine().expect("Metal device");
        let device = engine.report();
        // The INT8 engine's read probe (#176): the same kernel and buffer size
        // that its benchmark reports, so the two fractions are comparable.
        // The VM's bandwidth varies, so it is probed before and after the
        // shapes and the best reading is the device figure.
        let probe = crate::metal_gemv::metal_engine().expect("Metal device");
        let read = |storage| {
            let times = probe
                .measure_read_bandwidth(READ_PROBE_BYTES, 10, storage)
                .expect("read probe");
            READ_PROBE_BYTES as f64 / times[0] / 1e9
        };
        let mut reads = vec![read(Storage::Shared), read(Storage::Private)];
        let threads = rayon::current_num_threads();
        let simd = canonical_simd::dotprod_available();

        let mut rng = Rng(0x0D15_EA5E_BE4C_0016);
        let mut measured = Vec::new();
        for (name, rows, cols) in K26_SHAPES {
            let q = le_bytes(&weights(&mut rng, rows * cols));
            let admitted = I16Weights::new(&q).expect("admitted");
            let mu: Vec<i32> = (0..rows).map(|_| rng.mu()).collect();
            let k = vec![46u8; rows];
            // Q16 activations of magnitude up to 2: three digit planes.
            let x3: Vec<i64> = (0..cols).map(|_| rng.symmetric(1 << 17)).collect();
            let x5: Vec<i64> = (0..cols).map(|_| rng.symmetric(1 << 33)).collect();
            let matrix = engine
                .upload(&q, rows, cols, Storage::Shared)
                .expect("upload");

            // Exactness first: the benchmarked path must match the CPU.
            let cpu = |out: &mut Vec<i64>| {
                project_i16(admitted, rows, cols, &mu, &k, &x3, out).expect("in domain");
            };
            let mut want = vec![0i64; rows];
            cpu(&mut want);
            let metal = MetalI16Model::from_weights(&[(admitted, rows, cols)]).expect("upload");
            let switch = SwitchGuard::set(true);
            let mut got = vec![0i64; rows];
            metal.run(|| cpu(&mut got));
            assert_eq!(got, want, "{name}: GPU differs from project_i16");
            let call_seconds = time_wall(20, || metal.run(|| cpu(&mut got)));
            drop(switch);

            let mut sweep: Vec<(Tile, f64)> = Tile::all()
                .into_iter()
                .map(|tile| (tile, time_gpu(&engine, &[(&matrix, x3.as_slice())], tile)))
                .collect();
            sweep.sort_by(|a, b| a.1.total_cmp(&b.1));
            let (best, best_seconds) = sweep[0];
            let default_seconds = time_gpu(&engine, &[(&matrix, x3.as_slice())], DEFAULT_TILE);
            let five_plane_seconds = time_gpu(&engine, &[(&matrix, x5.as_slice())], best);
            let mut out = vec![0i64; rows];
            set_fast_canonical_kernel(false);
            let cpu_scalar_seconds = time_wall(3, || cpu(&mut out));
            let cpu_simd_seconds = if simd {
                set_fast_canonical_kernel(true);
                let seconds = time_wall(3, || cpu(&mut out));
                set_fast_canonical_kernel(false);
                Some(seconds)
            } else {
                None
            };
            measured.push(Measured {
                name,
                rows,
                cols,
                best,
                best_seconds,
                default_seconds,
                five_plane_seconds,
                call_seconds,
                cpu_scalar_seconds,
                cpu_simd_seconds,
                sweep,
            });
        }

        // One K2.6 layer's 64 heads (wk_b 512x128 and wv_b 128x512 each) as
        // 128 projections encoded in one command buffer, against one call per
        // head.
        let (n_heads, rank, nope) = (64usize, 512usize, 128usize);
        let wk = le_bytes(&weights(&mut rng, n_heads * rank * nope));
        let wv = le_bytes(&weights(&mut rng, n_heads * nope * rank));
        let wk_matrix = engine
            .upload(&wk, n_heads * rank, nope, Storage::Shared)
            .expect("upload");
        let wv_matrix = engine
            .upload(&wv, n_heads * nope, rank, Storage::Shared)
            .expect("upload");
        let heads_k: Vec<ResidentI16> = (0..n_heads)
            .map(|j| {
                let size = rank * nope * 2;
                engine
                    .upload(&wk[j * size..(j + 1) * size], rank, nope, Storage::Shared)
                    .expect("upload")
            })
            .collect();
        let heads_v: Vec<ResidentI16> = (0..n_heads)
            .map(|j| {
                let size = nope * rank * 2;
                engine
                    .upload(&wv[j * size..(j + 1) * size], nope, rank, Storage::Shared)
                    .expect("upload")
            })
            .collect();
        let q_nope: Vec<i64> = (0..nope).map(|_| rng.symmetric(1 << 17)).collect();
        let u: Vec<i64> = (0..rank).map(|_| rng.symmetric(1 << 17)).collect();
        let mut items: Vec<(&ResidentI16, &[i64])> = Vec::with_capacity(2 * n_heads);
        for (k_head, v_head) in heads_k.iter().zip(&heads_v) {
            items.push((k_head, q_nope.as_slice()));
            items.push((v_head, u.as_slice()));
        }
        let heads_bytes = 2 * n_heads * rank * nope * 2;
        let heads_seconds = time_gpu(&engine, &items, DEFAULT_TILE);
        let mut dots_k = vec![0i64; rank];
        let mut dots_v = vec![0i64; nope];
        let per_call_seconds = time_wall(3, || {
            for j in 0..n_heads {
                engine
                    .dot_rows(&wk_matrix, j * rank..(j + 1) * rank, &q_nope, &mut dots_k)
                    .expect("in domain");
                engine
                    .dot_rows(&wv_matrix, j * nope..(j + 1) * nope, &u, &mut dots_v)
                    .expect("in domain");
            }
        });

        reads.push(read(Storage::Shared));
        reads.push(read(Storage::Private));
        let device_gbps = reads.iter().copied().fold(0.0, f64::max);
        let mut md = String::new();
        md.push_str("### Exact Metal INT16 GEMV, Kimi K2.6 projection shapes\n\n");
        md.push_str(
            "Virtualized-runner measurement: GitHub-hosted macOS VM with a paravirtual \
             Metal GPU. Not Apple GPU hardware numbers.\n\n",
        );
        md.push_str(&format!(
            "Device `{}`; max buffer {} bytes; simdgroup width {}. Read bandwidth, 256 MiB, best \
             of 10 (the #176 probe), shared and private storage, before and after the shapes: \
             {}; the fractions below are of the best, {device_gbps:.1} GB/s. CPU columns: \
             `project_i16` on {threads} rayon threads, scalar and SIMD limbs{}. Build: this \
             workflow's release profile (LTO off, 16 codegen units).\n\n",
            device.name,
            device.max_buffer_length,
            device.thread_execution_width,
            reads
                .iter()
                .map(|gbps| format!("{gbps:.1}"))
                .collect::<Vec<_>>()
                .join(", "),
            if simd { "" } else { " (SIMD unavailable)" },
        ));
        md.push_str(
            "| Shape | Rows x cols | Best tile | GPU µs | GB/s | of measured BW | GPU µs, default \
             tile | GPU µs, 5 planes | One call, wall µs | CPU scalar µs | CPU SIMD µs |\n",
        );
        md.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
        let mut json_rows = Vec::new();
        for m in &measured {
            let gbps = (m.rows * m.cols * 2) as f64 / m.best_seconds / 1e9;
            let simd_us = m
                .cpu_simd_seconds
                .map_or("n/a".to_string(), |s| format!("{:.0}", s * 1e6));
            md.push_str(&format!(
                "| {} | {}x{} | {} | {:.1} | {gbps:.1} | {:.0}% | {:.1} | {:.1} | {:.1} | {:.0} | \
                 {simd_us} |\n",
                m.name,
                m.rows,
                m.cols,
                m.best.label(),
                m.best_seconds * 1e6,
                100.0 * gbps / device_gbps,
                m.default_seconds * 1e6,
                m.five_plane_seconds * 1e6,
                m.call_seconds * 1e6,
                m.cpu_scalar_seconds * 1e6,
            ));
            json_rows.push(serde_json::json!({
                "shape": m.name,
                "rows": m.rows,
                "cols": m.cols,
                "weight_bytes": m.rows * m.cols * 2,
                "best_tile": m.best.label(),
                "gpu_us_3_planes": m.best_seconds * 1e6,
                "gpu_gbps_3_planes": gbps,
                "fraction_of_measured_read_bandwidth": gbps / device_gbps,
                "gpu_us_default_tile": m.default_seconds * 1e6,
                "gpu_us_5_planes": m.five_plane_seconds * 1e6,
                "single_call_wall_us": m.call_seconds * 1e6,
                "cpu_scalar_us": m.cpu_scalar_seconds * 1e6,
                "cpu_simd_us": m.cpu_simd_seconds.map(|s| s * 1e6),
                "tile_sweep_us": m
                    .sweep
                    .iter()
                    .map(|(tile, seconds)| (tile.label(), seconds * 1e6))
                    .collect::<Vec<_>>(),
            }));
        }
        md.push_str(&format!(
            "\nGPU µs: one projection's GPU time (command-buffer timestamps, the same projection \
             encoded back to back), three digit planes (|x| <= 2^17). GB/s counts INT16 weight \
             bytes only; a matrix of a few MiB can stay partly in the GPU's cache across those \
             repeats, which a 256 MiB probe cannot, so its fraction can exceed 100%. One call, \
             wall µs: `project_i16` through the opt-in hook (digit split, dispatch, wait, \
             read-back and the CPU epilogue). Default tile {}.\n\n\
             One K2.6 layer's 64 heads (128 projections, {heads_bytes} weight bytes): {:.1} µs \
             of GPU time in one command buffer ({:.1} GB/s), against {:.1} µs as 128 separate \
             calls.\n",
            DEFAULT_TILE.label(),
            heads_seconds * 1e6,
            heads_bytes as f64 / heads_seconds / 1e9,
            per_call_seconds * 1e6,
        ));
        println!("{md}");
        let json = serde_json::json!({
            "label": "virtualized-runner measurement",
            "device": device,
            "read_gbps": reads,
            "device_gbps": device_gbps,
            "rayon_threads": threads,
            "default_tile": DEFAULT_TILE.label(),
            "shapes": json_rows,
            "layer_heads": {
                "projections": 2 * n_heads,
                "weight_bytes": heads_bytes,
                "gpu_us_one_command_buffer": heads_seconds * 1e6,
                "wall_us_separate_calls": per_call_seconds * 1e6,
            },
        });
        println!("METAL_EXACT_I16_BENCH {json}");
        append_summary(&md);
    }

    /// Append `md` to the file named by `ARC_METAL_BENCH_MD`, if any (each
    /// benchmark adds its own table to the CI summary).
    fn append_summary(md: &str) {
        use std::io::Write;
        if let Ok(path) = std::env::var("ARC_METAL_BENCH_MD") {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("open the benchmark summary");
            file.write_all(md.as_bytes())
                .expect("write the benchmark summary");
        }
    }

    /// Timed turns per path in the layer-heads benchmark.
    const ROUNDS: usize = 11;
    /// CPU serial, CPU pool, and the three GPU submissions.
    const PATHS: usize = 5;

    /// One K2.6 layer's heads (64 heads, `wk_b` 512 x 128, `wv_b` 128 x
    /// 512) end to end: both projections and the RoPE and attention between
    /// them, as the CPU loop of `layer_forward` runs them (heads one after
    /// another, and in parallel as #174 runs them) against the GPU batch in
    /// each submission, at three cache lengths. Every timed output equals the
    /// serial CPU loop. Virtualized-runner numbers.
    #[test]
    #[ignore = "benchmark: run explicitly in release with --ignored --nocapture"]
    fn k26_layer_heads_benchmark() {
        let _guard = kernel_switch_guard();
        let engine = metal_i16_engine().expect("Metal device");
        let threads = rayon::current_num_threads();
        let mut rng = Rng(0x0D15_EA5E_4EAD_0064);
        let (n_heads, rank, nope, rope_dim, v_dim) =
            (64usize, 512usize, 128usize, 64usize, 128usize);
        let mut md = String::new();
        md.push_str("\n### Exact Metal INT16 GEMV: one K2.6 layer's 64 heads as one batch\n\n");
        md.push_str(
            "Virtualized-runner measurement: GitHub-hosted macOS VM with a paravirtual \
             Metal GPU. Not Apple GPU hardware numbers.\n\n",
        );
        md.push_str(&format!(
            "Each cell is the median (and minimum) wall time in µs, over {ROUNDS} turns taken \
             in a rotating order, of one layer's heads: 64 `wk_b` (512 x 128) \
             and 64 `wv_b` (128 x 512) projections with the RoPE and absorbed attention \
             between them, over a cache of the given length. CPU columns run the per-head \
             loop of `layer_forward` on {threads} rayon threads. GPU columns run `try_heads`; \
             one command buffer per layer is in use on this device: {}. The last column is \
             the GPU time of the 128 projections alone (two dispatches, back to back).\n\n",
            engine.one_command_buffer(),
        ));
        md.push_str(
            "| Cache positions | CPU, heads one after another | CPU, heads in parallel (#174) \
             | GPU, one command buffer per layer | GPU, one per phase | GPU, one per head and \
             phase (#177's hook) | GPU time of the projections |\n",
        );
        md.push_str("|---|---|---|---|---|---|---|\n");
        let mut json_rows = Vec::new();
        for positions in [1usize, 64, 512] {
            let layer = LayerHeads::new(
                &mut rng,
                n_heads,
                rank,
                nope,
                rope_dim,
                v_dim,
                positions,
                1 << 14,
            );
            // The stacks are admitted (and, for the GPU, resident) before
            // anything is timed.
            let (key, value) = layer.stacks();
            let queries = layer.queries();
            let want = layer
                .cpu_with(&key, &value, Schedule::Serial)
                .expect("in domain");
            let metal = MetalI16Model::from_weights(&[
                (key.weights, n_heads * rank, nope),
                (value.weights, n_heads * v_dim, rank),
            ])
            .expect("upload");
            let switch = SwitchGuard::set(true);
            let heads_guard = HeadsGuard::set(true, Submission::OneCommandBuffer);
            let mut out = vec![0i64; n_heads * v_dim];
            let gpu_run = |submission: Submission, out: &mut Vec<i64>| -> bool {
                set_metal_i16_heads_submission(submission);
                metal.run(|| try_heads(key, &queries, value, |j, qa| layer.attend(j, qa), out))
            };
            for submission in Submission::ALL {
                assert!(
                    gpu_run(submission, &mut out) && out == want,
                    "{positions} positions, {submission:?}"
                );
            }
            // The five paths take turns, in a rotating order, so that drift
            // on the VM (clocks, other tenants) cannot favour one of them.
            let mut samples = vec![Vec::with_capacity(ROUNDS); PATHS];
            for round in 0..ROUNDS {
                for turn in 0..PATHS {
                    let path = (turn + round) % PATHS;
                    let start = Instant::now();
                    match path {
                        0 => {
                            std::hint::black_box(
                                layer
                                    .cpu_with(&key, &value, Schedule::Serial)
                                    .expect("in domain"),
                            );
                        }
                        1 => {
                            std::hint::black_box(
                                layer
                                    .cpu_with(&key, &value, Schedule::Pool)
                                    .expect("in domain"),
                            );
                        }
                        gpu => assert!(gpu_run(Submission::ALL[gpu - 2], &mut out)),
                    }
                    samples[path].push(start.elapsed().as_secs_f64());
                }
            }
            drop(heads_guard);
            drop(switch);
            let stats: Vec<(f64, f64)> = samples
                .into_iter()
                .map(|times| {
                    let low = times.iter().copied().fold(f64::INFINITY, f64::min);
                    (median(times), low)
                })
                .collect();
            // GPU time of the two dispatches alone, given every vector.
            let key_matrix = engine
                .upload(
                    key.weights.as_bytes(),
                    n_heads * rank,
                    nope,
                    Storage::Shared,
                )
                .expect("upload");
            let value_matrix = engine
                .upload(
                    value.weights.as_bytes(),
                    n_heads * v_dim,
                    rank,
                    Storage::Shared,
                )
                .expect("upload");
            let a = HeadPhase {
                matrix: &key_matrix,
                first_row: 0,
                heads: n_heads,
                rows: rank,
            };
            let b = HeadPhase {
                matrix: &value_matrix,
                first_row: 0,
                heads: n_heads,
                rows: v_dim,
            };
            let latents = layer.latents();
            let phases = [(&a, queries.as_slice()), (&b, latents.as_slice())];
            let single = engine
                .time_heads(&phases, engine.tile(), 1)
                .expect("timing");
            let repeats = ((0.2 / single.max(1e-6)) as usize).clamp(3, 200);
            let projections = engine
                .time_heads(&phases, engine.tile(), repeats)
                .expect("timing");
            let cell =
                |(median, low): (f64, f64)| format!("{:.0} ({:.0})", median * 1e6, low * 1e6);
            md.push_str(&format!(
                "| {positions} | {} | {} | {} | {} | {} | {:.0} |\n",
                cell(stats[0]),
                cell(stats[1]),
                cell(stats[2]),
                cell(stats[3]),
                cell(stats[4]),
                projections * 1e6,
            ));
            let us = |(median, low): (f64, f64)| serde_json::json!({"median": median * 1e6, "min": low * 1e6});
            json_rows.push(serde_json::json!({
                "positions": positions,
                "cpu_serial_us": us(stats[0]),
                "cpu_parallel_heads_us": us(stats[1]),
                "gpu_one_command_buffer_us": us(stats[2]),
                "gpu_per_phase_us": us(stats[3]),
                "gpu_per_head_us": us(stats[4]),
                "gpu_projections_only_us": projections * 1e6,
            }));
        }
        md.push_str(
            "\nThe stacks are resident before anything is timed. Every GPU column computes the \
             same integers as the CPU loop (checked before timing); the submissions differ \
             only in host round trips: 1 commit and completion per layer with a shared-event \
             handoff, 2 per layer, or 128 per layer.\n",
        );
        println!("{md}");
        let json = serde_json::json!({
            "label": "virtualized-runner measurement",
            "rayon_threads": threads,
            "one_command_buffer": engine.one_command_buffer(),
            "default_tile": DEFAULT_TILE.label(),
            "rows": json_rows,
        });
        println!("METAL_EXACT_I16_HEADS_BENCH {json}");
        append_summary(&md);
    }
}
