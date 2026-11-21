use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static BATCHED_PREFILL: AtomicBool = AtomicBool::new(false);

pub const MIN_PROFITABLE_BATCH_TOKENS: usize = 4;

pub const MAX_PREFILL_SCRATCH_BYTES: usize = 256 * 1024 * 1024;

pub const SERVING_PREFILL_CHUNK: usize = 64;

pub fn batching_is_profitable(n_tokens: usize) -> bool {
    n_tokens >= MIN_PROFITABLE_BATCH_TOKENS
}

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
    let pair_copy = limb_planes;
    activations
        .saturating_add(limb_planes)
        .saturating_add(pair_copy)
}

pub fn max_chunk_within_scratch_budget(d_model: usize, d_kv: usize, d_ff: usize) -> usize {
    let per_token = prefill_chunk_scratch_bytes(1, d_model, d_kv, d_ff).max(1);
    (MAX_PREFILL_SCRATCH_BYTES / per_token).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefillRefusal {
    NotCanonicalProfile,
    Shape,
    ContextWindowExceeded,
}

static N_CHUNKS: AtomicU64 = AtomicU64::new(0);
static N_BATCHED_PROJECTIONS: AtomicU64 = AtomicU64::new(0);
static N_TOKENS: AtomicU64 = AtomicU64::new(0);
static N_REFUSED_PROFILE: AtomicU64 = AtomicU64::new(0);
static N_REFUSED_SHAPE: AtomicU64 = AtomicU64::new(0);
static N_REFUSED_CONTEXT: AtomicU64 = AtomicU64::new(0);

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
        for n in 0..MIN_PROFITABLE_BATCH_TOKENS {
            assert!(!batching_is_profitable(n), "{n} tokens must not batch");
        }
        for n in MIN_PROFITABLE_BATCH_TOKENS..=64 {
            assert!(batching_is_profitable(n), "{n} tokens should batch");
        }
    }

    #[test]
    fn scratch_estimate_is_linear_and_bounded() {
        let (d, dkv, dff) = (4096usize, 4096usize, 11008usize);
        let one = prefill_chunk_scratch_bytes(1, d, dkv, dff);
        assert_eq!(prefill_chunk_scratch_bytes(0, d, dkv, dff), 0);
        assert_eq!(prefill_chunk_scratch_bytes(64, d, dkv, dff), one * 64);

        assert!(
            prefill_chunk_scratch_bytes(64, d, dkv, dff) < MAX_PREFILL_SCRATCH_BYTES,
            "chunk 64 must remain within budget: {} vs {MAX_PREFILL_SCRATCH_BYTES}",
            prefill_chunk_scratch_bytes(64, d, dkv, dff)
        );

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
        assert_eq!(max_chunk_within_scratch_budget(usize::MAX / 64, 1, 1), 1);
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
        assert!(
            c.chunks >= 2 && c.tokens >= 12 && c.batched_projections >= 450,
            "{c:?}"
        );
        reset_prefill_census();
    }
}
