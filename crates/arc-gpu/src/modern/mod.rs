//! Portable, bit-exact GPU kernels for the dyadic integer profile
//! `arc.hf-llama.i8-dyadic-row.q16.v1` (SmolLM3-3B), written in WGSL and run
//! through wgpu: Vulkan (NVIDIA, AMD, Intel, Mesa lavapipe), DX12 (Windows,
//! WARP) and Metal (Apple).
//!
//! The kernels compute exactly the integers of the CPU engine in
//! `arc_inference::modern` (spec: `docs/protocol/integer-profile-hf-llama-dyadic-v1.md`),
//! so logits, tokens and golden digests are identical. The argument, in short
//! (details in `docs/gpu-portable-kernels.md`):
//!
//! * No floating point. Wide values are u32 limbs in two's complement; only
//!   unsigned wrapping arithmetic and shifts in `[0, 31]` are used.
//! * Projections decompose each i64 activation into eight balanced base-256
//!   digits and sum i8 x i8 products in i32, which provably cannot overflow
//!   for rows of at most [`MAX_PROJECTION_INPUT`] inputs. The digit sums are
//!   recombined exactly and the dyadic epilogue runs in i128.
//! * Every reduction is an exact integer sum or maximum, so thread count,
//!   workgroup size and scheduling cannot change a value. There are no float
//!   atomics; the only atomic is an integer OR of refusal flags.
//! * Every domain refusal of the CPU engine (spec §9) sets a status bit in the
//!   same place, and the host turns it into the same refusal.
//!
//! Token selection, logits hashing and tokenisation stay on the CPU: the
//! protocol hashes every logits vector anyway, so the logits are read back.

mod device;
mod engine;
mod kernels;
mod lab;

pub use device::{AdapterReport, list_adapters};
pub use engine::{
    EngineOptions, GpuEngine, GpuEngineBuilder, MAX_BATCH, MAX_POSITIONS, MAX_PROJECTION_INPUT,
    TraceEntry,
};
pub use lab::{AttentionCase, OpLab};

/// Why a GPU operation refused or failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GpuModernError {
    /// No adapter, or none matching the requested selector.
    #[error("no usable GPU adapter: {0}")]
    NoAdapter(String),
    /// Device creation, shader compilation or another wgpu failure.
    #[error("GPU device: {0}")]
    Device(String),
    /// A model shape or adapter limit these kernels do not support.
    #[error("unsupported by the portable GPU kernels: {0}")]
    Unsupported(String),
    /// Malformed input (shapes, token ids, context length).
    #[error("invalid input: {0}")]
    Invalid(String),
    /// An intermediate left the profile's domain; the CPU engine refuses the
    /// same input.
    #[error("out of the profile's domain: {0}")]
    Domain(String),
    /// A failure while running or reading back a submitted forward pass.
    #[error("GPU execution: {0}")]
    Execution(String),
}

/// Model shape and profile constants, as read from the integer package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelShape {
    pub n_layers: usize,
    pub d_model: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub d_head: usize,
    pub d_ff: usize,
    pub vocab_size: usize,
    pub max_seq: usize,
    /// `round_half_away(rms_norm_eps * 2^32)`, at least 1.
    pub rms_eps_q32: i64,
    /// `true` where the layer applies RoPE, `false` for NoPE layers.
    pub rope_layers: Vec<bool>,
    /// `floor(2^30 / sqrt(d_head))`, the CPU engine's `attention_lambda`.
    pub attention_lambda: i64,
}

impl ModelShape {
    /// Width of the concatenated query heads.
    pub fn d_q(&self) -> usize {
        self.n_heads * self.d_head
    }

    /// Width of the concatenated KV heads.
    pub fn d_kv(&self) -> usize {
        self.n_kv_heads * self.d_head
    }
}

/// A borrowed per-row INT8 matrix with dyadic scales `mu * 2^-k`.
#[derive(Debug, Clone, Copy)]
pub struct DyadicRef<'a> {
    pub rows: usize,
    pub cols: usize,
    /// Row-major weights in `[-127, 127]`.
    pub q: &'a [i8],
    pub mu: &'a [i32],
    pub k: &'a [u8],
}

impl DyadicRef<'_> {
    fn check(&self, name: &str, rows: usize, cols: usize) -> Result<(), GpuModernError> {
        let consistent = self.rows == rows
            && self.cols == cols
            && rows.checked_mul(cols) == Some(self.q.len())
            && self.mu.len() == rows
            && self.k.len() == rows;
        if !consistent {
            return Err(GpuModernError::Invalid(format!(
                "{name}: shape {}x{} (q {}, mu {}, k {}) where {rows}x{cols} is required",
                self.rows,
                self.cols,
                self.q.len(),
                self.mu.len(),
                self.k.len()
            )));
        }
        for (row, (&mu, &k)) in self.mu.iter().zip(self.k).enumerate() {
            let zero = mu == 0 && k == 16;
            let normal = mu >= 1 << 30 && (16..=62).contains(&k);
            if !(zero || normal) {
                return Err(GpuModernError::Invalid(format!(
                    "{name}: row {row} has an invalid dyadic scale ({mu}, {k})"
                )));
            }
        }
        if self.q.contains(&i8::MIN) {
            return Err(GpuModernError::Invalid(format!(
                "{name}: weight -128 is not a profile value"
            )));
        }
        Ok(())
    }
}

/// One transformer layer's tensors, borrowed.
#[derive(Debug, Clone, Copy)]
pub struct LayerRef<'a> {
    pub attn_norm: &'a [i64],
    pub wq: DyadicRef<'a>,
    pub wk: DyadicRef<'a>,
    pub wv: DyadicRef<'a>,
    pub wo: DyadicRef<'a>,
    pub ffn_norm: &'a [i64],
    pub w_gate: DyadicRef<'a>,
    pub w_up: DyadicRef<'a>,
    pub w_down: DyadicRef<'a>,
}

/// Bits of the GPU status word (one per CPU refusal site), mirrored from
/// `wgsl/int.wgsl`.
pub mod status {
    pub const PROJ_INPUT: u32 = 1;
    pub const PROJ_OUTPUT: u32 = 1 << 1;
    pub const RMS_SUM: u32 = 1 << 2;
    pub const RMS_MEAN: u32 = 1 << 3;
    pub const RMS_PRODUCT: u32 = 1 << 4;
    pub const RMS_OUTPUT: u32 = 1 << 5;
    pub const ROPE_OUTPUT: u32 = 1 << 6;
    pub const KV_RANGE: u32 = 1 << 7;
    pub const ATTN_PRODUCT: u32 = 1 << 8;
    pub const ATTN_SCORE: u32 = 1 << 9;
    pub const SILU_PRODUCT: u32 = 1 << 10;
    pub const SILU_OUTPUT: u32 = 1 << 11;
    pub const RESIDUAL: u32 = 1 << 12;
    pub const EMBED_TOKEN: u32 = 1 << 13;

    const MESSAGES: [(u32, &str); 14] = [
        (
            PROJ_INPUT,
            "projection input magnitude (127 * sum |x| >= 2^63)",
        ),
        (PROJ_OUTPUT, "projection output beyond 2^62"),
        (RMS_SUM, "rms_norm sum of squares beyond 2^127"),
        (RMS_MEAN, "rms_norm mean square beyond 2^92"),
        (RMS_PRODUCT, "rms_norm product beyond 2^127"),
        (RMS_OUTPUT, "rms_norm output beyond 2^62"),
        (ROPE_OUTPUT, "rope output beyond 2^62"),
        (KV_RANGE, "KV value outside i32 (|v| >= 2^31)"),
        (ATTN_PRODUCT, "attention score product beyond 2^127"),
        (ATTN_SCORE, "attention score beyond 2^62"),
        (SILU_PRODUCT, "gated SiLU product beyond 2^127"),
        (SILU_OUTPUT, "gated SiLU output beyond 2^62"),
        (RESIDUAL, "residual beyond 2^62"),
        (EMBED_TOKEN, "token id is outside the vocabulary"),
    ];

    /// The CPU engine's refusal messages for every bit set in `bits`.
    pub fn describe(bits: u32) -> String {
        let parts: Vec<&str> = MESSAGES
            .iter()
            .filter(|(bit, _)| bits & bit != 0)
            .map(|(_, message)| *message)
            .collect();
        if parts.is_empty() {
            format!("status {bits:#x}")
        } else {
            parts.join("; ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bits_are_distinct_and_described() {
        let all = (0..14).fold(0u32, |acc, i| acc | (1 << i));
        assert_eq!(status::describe(all).matches("; ").count(), 13);
        assert_eq!(
            status::describe(status::KV_RANGE),
            "KV value outside i32 (|v| >= 2^31)"
        );
        assert_eq!(status::describe(0), "status 0x0");
    }

    #[test]
    fn dyadic_ref_checks_shape_and_scales() {
        let q = [1i8, -2, 3, 4];
        let mu = [1 << 30, 0];
        let k = [40u8, 16];
        let ok = DyadicRef {
            rows: 2,
            cols: 2,
            q: &q,
            mu: &mu,
            k: &k,
        };
        assert!(ok.check("m", 2, 2).is_ok());
        assert!(ok.check("m", 1, 4).is_err());
        let bad_k = [40u8, 15];
        assert!(DyadicRef { k: &bad_k, ..ok }.check("m", 2, 2).is_err());
        let bad_q = [1i8, i8::MIN, 3, 4];
        assert!(DyadicRef { q: &bad_q, ..ok }.check("m", 2, 2).is_err());
    }
}
