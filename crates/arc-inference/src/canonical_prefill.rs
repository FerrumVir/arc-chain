//! Opt-in batched multi-token prefill for the canonical per-row I8 profile.
//!
//! This is a **separate** switch from [`crate::canonical_simd`], so the effect
//! of batching can be isolated from the effect of the vectorised kernel. Both
//! default to OFF. Four combinations are therefore measurable independently:
//! scalar/token-at-a-time, scalar/batched, SIMD/token-at-a-time, SIMD/batched.
//!
//! # Why batching cannot change a value
//!
//! Every output is `(dot(row_i, x_t) * scale_i) >> FRAC_BITS` — exactly the
//! expression the token-at-a-time path computes. Tokens share only the
//! read-only weights. There is no cross-token reduction, no shared scale, and
//! no batch-dependent quantisation anywhere in the canonical profile, so a
//! batch size cannot influence any token's result. (This is the property FP
//! stacks have to engineer for with batch-invariant kernels; integer per-row
//! arithmetic has it by construction.)
//!
//! Attention, RoPE, the KV cache, layer norms and the activation function stay
//! strictly per token and per position, so causality and cache ordering are
//! unchanged. Batching applies only to the projections, which are ~99% of
//! prefill arithmetic.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

static BATCHED_PREFILL: AtomicBool = AtomicBool::new(false);

/// Below this many tokens in a chunk, batching is a measured **loss**, so a
/// production caller must route to the token-at-a-time path instead.
///
/// This is not a guess. In the E1(b) run on the real canonical artifact, a
/// one-token prefill was *slower* batched: 0.604x on the SIMD path
/// (113 ms -> 186 ms) and 0.289x on the scalar path (187 ms -> 646 ms). The
/// cause is structural: a short quad is filled by repeating the first token's
/// digit planes and the duplicate lanes are discarded, so a quad of one token
/// costs up to four times the arithmetic and produces the same single result.
/// The scalar ratio matches that explanation almost exactly (187 x 4 = 748 ms
/// predicted against 646 ms observed).
///
/// Batching is therefore a capability with a profitability floor, not a
/// universal improvement, and must never be enabled unconditionally.
pub const MIN_PROFITABLE_BATCH_TOKENS: usize = 4;

/// Ceiling on the transient scratch one prefill chunk may allocate.
///
/// Chunk size is a pure performance knob: the conformance tests show every
/// chunk size produces bit-identical logits and an identical KV cache, so a
/// chunk may be split without changing any result. That makes clamping the
/// chunk to fit this budget safe, and it is what
/// `prefill_canonical_i8_batched` does — it never refuses a request that the
/// token-at-a-time path would have served, and it never relaxes a bound.
///
/// Without this, scratch scales linearly with the caller's chunk size: at the
/// `MAX_BATCH_TOKENS` ceiling of 1024 and the canonical 4096/11008 geometry, a
/// single chunk would reserve roughly 460 MB. On a memory-constrained host that
/// is the difference between serving and swapping.
pub const MAX_PREFILL_SCRATCH_BYTES: usize = 256 * 1024 * 1024;

/// Should a caller holding `n_tokens` use the batched path at all?
///
/// Capability question, not an admission question: a `false` here means "this
/// is slower batched", never "this request is rejected".
pub fn batching_is_profitable(n_tokens: usize) -> bool {
    n_tokens >= MIN_PROFITABLE_BATCH_TOKENS
}

/// Transient bytes one batched prefill chunk allocates for `n_tokens`.
///
/// Counts the ten per-chunk i64 activation buffers the prefill itself holds
/// (`6 * d_model + 2 * d_kv + 2 * d_ff` elements per token) plus the digit
/// planes the vectorised kernel splits activations into for the widest
/// projection (`LIMB_COUNT * d_ff` bytes per token, from `w_down`). It excludes
/// the model weights and the KV cache, which are not per-chunk.
pub fn prefill_chunk_scratch_bytes(
    n_tokens: usize,
    d_model: usize,
    d_kv: usize,
    d_ff: usize,
) -> usize {
    let activations = n_tokens
        .saturating_mul(
            6usize
                .saturating_mul(d_model)
                .saturating_add(2usize.saturating_mul(d_kv))
                .saturating_add(2usize.saturating_mul(d_ff)),
        )
        .saturating_mul(8);
    let limb_planes = n_tokens
        .saturating_mul(crate::canonical_simd::LIMB_COUNT)
        .saturating_mul(d_ff);
    activations.saturating_add(limb_planes)
}

/// Largest chunk whose scratch fits [`MAX_PREFILL_SCRATCH_BYTES`], at least 1.
///
/// Returns 1 rather than 0 for a geometry so large that even a single token
/// exceeds the budget: one token is exactly what the token-at-a-time path
/// already allocates, so refusing it would be stricter than the existing path
/// rather than safer.
pub fn max_chunk_within_scratch_budget(d_model: usize, d_kv: usize, d_ff: usize) -> usize {
    let per_token = prefill_chunk_scratch_bytes(1, d_model, d_kv, d_ff).max(1);
    (MAX_PREFILL_SCRATCH_BYTES / per_token).max(1)
}

/// Why a prefill declined the batched path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefillRefusal {
    /// Model is not the complete canonical per-row I8 profile.
    NotCanonicalProfile,
    /// Empty prompt, zero chunk size, or an unusable cache/embedding shape.
    Shape,
    /// The request would exceed the model's context window.
    ContextWindowExceeded,
}

static N_CHUNKS: AtomicU64 = AtomicU64::new(0);
static N_BATCHED_PROJECTIONS: AtomicU64 = AtomicU64::new(0);
static N_TOKENS: AtomicU64 = AtomicU64::new(0);
static N_REFUSED_PROFILE: AtomicU64 = AtomicU64::new(0);
static N_REFUSED_SHAPE: AtomicU64 = AtomicU64::new(0);
static N_REFUSED_CONTEXT: AtomicU64 = AtomicU64::new(0);

/// What the batched prefill actually did, so a speedup can never be reported
/// for a run that silently fell back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrefillCensus {
    pub chunks: u64,
    pub batched_projections: u64,
    pub tokens: u64,
    pub refused_not_canonical_profile: u64,
    pub refused_shape: u64,
    pub refused_context_window: u64,
}

impl PrefillCensus {
    pub fn refused_total(&self) -> u64 {
        self.refused_not_canonical_profile + self.refused_shape + self.refused_context_window
    }
}

/// Enable or disable batched prefill process-wide. Default OFF.
///
/// `ARC_BATCHED_PREFILL=1` sets the initial state once; an explicit call
/// afterwards still wins.
pub fn set_batched_prefill_enabled(enabled: bool) {
    BATCHED_PREFILL.store(enabled, Ordering::Relaxed);
}

pub fn batched_prefill_enabled() -> bool {
    static ENV_INIT: OnceLock<()> = OnceLock::new();
    ENV_INIT.get_or_init(|| {
        if std::env::var("ARC_BATCHED_PREFILL").as_deref() == Ok("1") {
            BATCHED_PREFILL.store(true, Ordering::Relaxed);
        }
    });
    BATCHED_PREFILL.load(Ordering::Relaxed)
}

pub fn reset_prefill_census() {
    for c in [
        &N_CHUNKS,
        &N_BATCHED_PROJECTIONS,
        &N_TOKENS,
        &N_REFUSED_PROFILE,
        &N_REFUSED_SHAPE,
        &N_REFUSED_CONTEXT,
    ] {
        c.store(0, Ordering::Relaxed);
    }
}

pub fn prefill_census() -> PrefillCensus {
    PrefillCensus {
        chunks: N_CHUNKS.load(Ordering::Relaxed),
        batched_projections: N_BATCHED_PROJECTIONS.load(Ordering::Relaxed),
        tokens: N_TOKENS.load(Ordering::Relaxed),
        refused_not_canonical_profile: N_REFUSED_PROFILE.load(Ordering::Relaxed),
        refused_shape: N_REFUSED_SHAPE.load(Ordering::Relaxed),
        refused_context_window: N_REFUSED_CONTEXT.load(Ordering::Relaxed),
    }
}

pub(crate) fn record_chunk(tokens: usize, projections: usize) {
    N_CHUNKS.fetch_add(1, Ordering::Relaxed);
    N_TOKENS.fetch_add(tokens as u64, Ordering::Relaxed);
    N_BATCHED_PROJECTIONS.fetch_add(projections as u64, Ordering::Relaxed);
}

pub(crate) fn record_prefill_refusal(reason: PrefillRefusal) {
    match reason {
        PrefillRefusal::NotCanonicalProfile => &N_REFUSED_PROFILE,
        PrefillRefusal::Shape => &N_REFUSED_SHAPE,
        PrefillRefusal::ContextWindowExceeded => &N_REFUSED_CONTEXT,
    }
    .fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batched_prefill_is_off_unless_requested() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        if std::env::var("ARC_BATCHED_PREFILL").as_deref() == Ok("1") {
            assert!(batched_prefill_enabled());
        } else {
            assert!(!batched_prefill_enabled());
        }
    }

    #[test]
    fn batching_profitability_floor_matches_the_measured_loss() {
        // Measured on the real artifact: a one-token batch ran 0.604x (SIMD)
        // and 0.289x (scalar). Anything under a full quad repeats lanes.
        for n in 0..MIN_PROFITABLE_BATCH_TOKENS {
            assert!(!batching_is_profitable(n), "{n} tokens must not batch");
        }
        for n in MIN_PROFITABLE_BATCH_TOKENS..=64 {
            assert!(batching_is_profitable(n), "{n} tokens should batch");
        }
    }

    #[test]
    fn scratch_estimate_is_linear_and_bounded() {
        // Canonical geometry: 4096 d_model, 4096 d_kv, 11008 d_ff.
        let (d, dkv, dff) = (4096usize, 4096usize, 11008usize);
        let one = prefill_chunk_scratch_bytes(1, d, dkv, dff);
        assert_eq!(prefill_chunk_scratch_bytes(0, d, dkv, dff), 0);
        assert_eq!(prefill_chunk_scratch_bytes(64, d, dkv, dff), one * 64);

        // The chunk the E1(b) run used must stay comfortably inside the budget,
        // or the measured configuration would silently stop being reachable.
        assert!(
            prefill_chunk_scratch_bytes(64, d, dkv, dff) < MAX_PREFILL_SCRATCH_BYTES,
            "chunk 64 must remain within budget: {} vs {MAX_PREFILL_SCRATCH_BYTES}",
            prefill_chunk_scratch_bytes(64, d, dkv, dff)
        );

        // The clamp has to actually bite at the MAX_BATCH_TOKENS ceiling,
        // otherwise it is decorative.
        let cap = max_chunk_within_scratch_budget(d, dkv, dff);
        assert!(cap >= 64, "budget must admit the measured chunk, got {cap}");
        assert!(
            cap < crate::canonical_simd::MAX_BATCH_TOKENS,
            "budget must bind before the 1024-token ceiling, got {cap}"
        );
        assert!(
            prefill_chunk_scratch_bytes(cap, d, dkv, dff) <= MAX_PREFILL_SCRATCH_BYTES,
            "the admitted cap must itself fit the budget"
        );
    }

    #[test]
    fn scratch_budget_never_returns_a_zero_chunk() {
        // A geometry so wide that one token exceeds the budget must still admit
        // one token: that is exactly what the token-at-a-time path allocates,
        // so refusing would be stricter than the path this replaces.
        assert_eq!(max_chunk_within_scratch_budget(usize::MAX / 64, 1, 1), 1);
        // And a small model must NOT be throttled by this budget at all: the
        // MAX_BATCH_TOKENS ceiling should be what binds there, not memory.
        assert!(
            max_chunk_within_scratch_budget(64, 64, 128) > crate::canonical_simd::MAX_BATCH_TOKENS,
            "a tiny geometry must not be scratch-bound"
        );
    }

    #[test]
    fn census_counts_are_additive() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        reset_prefill_census();
        record_chunk(8, 225);
        record_chunk(4, 225);
        let c = prefill_census();
        assert!(c.chunks >= 2 && c.tokens >= 12 && c.batched_projections >= 450, "{c:?}");
        reset_prefill_census();
    }
}
