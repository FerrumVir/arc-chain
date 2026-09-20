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
