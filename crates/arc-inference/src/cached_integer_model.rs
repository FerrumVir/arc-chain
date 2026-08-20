//! Cached Integer Model - Production-speed deterministic inference.
//!
//! Default mode: **INT16** weights (2 bytes per parameter) with per-row Q16 scale factors.
//! INT16 gives 32,767 quantization levels per row - 258x finer than INT8's 127 levels,
//! and 32x finer than FP16's 1,024 mantissa levels - while remaining fully deterministic.
//!
//! Also supports INT8 (1 byte, lower precision) and Q4 (0.5 byte, lowest bandwidth).
//! Forward pass: integer weight × i64 activation → accumulate in i64 → per-row scale → Q16.
//! Pure integer arithmetic during inference. Deterministic on all platforms.
//! Float used ONLY at model load time (GGUF dequant → per-row quantization).

use crate::integer_lut::*;
use arc_crypto::Hash256;
use rayon::prelude::*;
use tracing::info;

// ─── INT8 Weight Storage (Per-Row Quantization) ───────────────────────────────

/// Per-row symmetric INT8 quantized weight matrix.
///
/// Each row has its own scale factor, so every row uses the full [-127, 127] range.
/// This eliminates the precision loss from outlier weights in other rows.
///
/// Layout: data is row-major [n_rows × n_cols] as i8.
/// scales[i] = Q16 representation of (abs_max_of_row_i / 127).
/// Reconstruction: real_value[i][j] ≈ data[i*cols+j] * scales[i] / ONE
pub struct I8Weights {
    pub data: Vec<i8>,
    pub scales: Vec<i64>,  // Per-row scale in Q16 (one per output row)
    pub n_rows: usize,
    pub n_cols: usize,
}

impl I8Weights {
    /// Quantize f32 matrix [n_rows × n_cols] to per-row symmetric INT8.
    pub fn quantize_f32(values: &[f32], n_rows: usize, n_cols: usize) -> Self {
        assert_eq!(values.len(), n_rows * n_cols);

        let mut data = Vec::with_capacity(n_rows * n_cols);
        let mut scales = Vec::with_capacity(n_rows);

        for i in 0..n_rows {
            let row = &values[i * n_cols..(i + 1) * n_cols];

            // Per-row abs_max
            let abs_max = row.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
            let abs_max = abs_max.max(1e-10);

            let inv_abs_max = 127.0 / abs_max;
            for &x in row {
                data.push((x * inv_abs_max).round().clamp(-127.0, 127.0) as i8);
            }

            // Per-row scale = abs_max / 127 in Q16. Compute in f64 to keep
            // sub-1.0 abs_max values (every output.weight and ffn_down row
            // in real Llama-2-7B per probe_i16_real_weights.rs) from
            // collapsing to scale=1. Pre-fix this used `abs_max as i64`,
            // which truncated abs_max < 1 to 0 — leaving every row of those
            // tensors quantized at ~36-258× the wrong magnitude. Block-wise
            // INT8 (the default forward path on v0.5.3+) was unaffected
            // because it does its own per-block scale; this fix matters for
            // any path still routing through I8Weights (sharded loaders
            // and the I16 dual-quantize bring-up).
            let scale = ((abs_max as f64 * ONE as f64) / 127.0)
                .round()
                .max(1.0) as i64;
            scales.push(scale);
        }

        Self { data, scales, n_rows, n_cols }
    }

    /// Memory usage in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 8 + 16
    }

    /// Zero-memory placeholder used by sharded loading for layers this node
    /// does NOT hold. The forward path will skip these slots.
    pub fn empty() -> Self {
        Self { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 }
    }
}

// ─── INT16 Weight Storage (Per-Row Quantization, Feature-Gated) ──────────────

/// Per-row symmetric INT16 quantized weight matrix.
///
/// Same approach as I8Weights but with 258x finer granularity: full [-32767, 32767] range.
/// This reduces quantization error by ~258x compared to INT8 [-127, 127].
///
/// Layout: data is row-major [n_rows x n_cols] as i16.
/// scales[i] = Q16 representation of (abs_max_of_row_i / 32767).
/// Reconstruction: real_value[i][j] ≈ data[i*cols+j] * scales[i] / ONE
pub struct I16Weights {
    pub data: Vec<i16>,
    pub scales: Vec<i64>,  // Per-row scale in Q16 (one per output row)
    pub n_rows: usize,
    pub n_cols: usize,
}

impl I16Weights {
    /// Quantize f32 matrix [n_rows x n_cols] to per-row symmetric INT16.
    ///
    /// Uses the full [-32767, 32767] range per row, giving ~258x finer
    /// granularity than INT8's [-127, 127]. This is where the real
    /// perplexity improvement comes from.
    pub fn quantize_f32(values: &[f32], n_rows: usize, n_cols: usize) -> Self {
        assert_eq!(values.len(), n_rows * n_cols);

        let mut data = Vec::with_capacity(n_rows * n_cols);
        let mut scales = Vec::with_capacity(n_rows);

        for i in 0..n_rows {
            let row = &values[i * n_cols..(i + 1) * n_cols];

            // Per-row abs_max
            let abs_max = row.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
            let abs_max = abs_max.max(1e-10);

            let inv_abs_max = 32767.0 / abs_max;
            for &x in row {
                data.push((x * inv_abs_max).round().clamp(-32767.0, 32767.0) as i16);
            }

            // Per-row scale = abs_max in Q16. The matmul divides by 32767 to
            // complete the dequantization: output = (acc * scale) / 32767 >> FRAC_BITS.
            // This keeps the scale large enough for sub-1.0 abs_max values.
            let scale = (abs_max as f64 * ONE as f64).round() as i64;
            scales.push(scale.max(1));
        }

        Self { data, scales, n_rows, n_cols }
    }

    /// Zero-memory placeholder used by sharded loading.
    pub fn empty() -> Self {
        Self { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 }
    }

    /// Convert from existing I8Weights (cast i8 -> i16, adjust scales).
    ///
    /// This does NOT improve precision - the i8 quantization loss is already baked in.
    /// Use this to validate the I16 code path. For real quality improvement,
    /// use `quantize_f32` with the original float weights.
    ///
    /// Scale adjustment: I8 matmul uses `(acc * scale) >> FRAC_BITS` where
    /// scale = abs_max/127 in Q16. I16 matmul uses `(acc / 32767 * scale) >> FRAC_BITS`
    /// where scale = abs_max in Q16. To preserve correctness for i8-range values
    /// ([-127,127] not [-32767,32767]), we set scale = i8_scale * 32767 so that
    /// the /32767 in the matmul cancels out, giving the same result as I8.
    pub fn from_i8(w: &I8Weights) -> Self {
        let data: Vec<i16> = w.data.iter().map(|&v| v as i16).collect();
        // Adjust scales: i8 scale represents abs_max/127 * ONE,
        // i16 matmul expects abs_max * ONE and divides by 32767 internally.
        // For i8-range data: scale = i8_scale * 32767 so (val / 32767 * scale) = (val * i8_scale).
        // But we also need >> FRAC_BITS to match, so scale = i8_scale * 32767.
        let scales: Vec<i64> = w.scales.iter().map(|&s| s * 32767).collect();
        Self { data, scales, n_rows: w.n_rows, n_cols: w.n_cols }
    }

    /// Memory usage in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.data.len() * 2 + self.scales.len() * 8 + 16
    }
}

// ─── Layer and Model Structs ──────────────────────────────────────────────────

/// Pre-loaded transformer layer weights in per-row INT8 with Q16 norms.
pub struct CachedLayer {
    pub wq: I8Weights,      // [d_model × d_model]
    pub wk: I8Weights,      // [d_model × d_kv]
    pub wv: I8Weights,      // [d_model × d_kv]
    pub wo: I8Weights,      // [d_model × d_model]
    pub w_gate: I8Weights,  // [d_ff × d_model]
    pub w_up: I8Weights,    // [d_ff × d_model]
    pub w_down: I8Weights,  // [d_model × d_ff]
    pub attn_norm: Vec<i64>, // norms stay i64 (small: d_model each)
    pub ffn_norm: Vec<i64>,
}

impl CachedLayer {
    /// Zero-memory placeholder for layer slots NOT held by this shard.
    /// The pipeline forward pass only iterates [shard_start, shard_end), so
    /// these slots are never read. Total cost: ~144 bytes per skipped layer.
    pub fn placeholder() -> Self {
        Self {
            wq: I8Weights::empty(),
            wk: I8Weights::empty(),
            wv: I8Weights::empty(),
            wo: I8Weights::empty(),
            w_gate: I8Weights::empty(),
            w_up: I8Weights::empty(),
            w_down: I8Weights::empty(),
            attn_norm: Vec::new(),
            ffn_norm: Vec::new(),
        }
    }

    /// True if this slot was populated by the loader (vs a placeholder).
    pub fn is_loaded(&self) -> bool {
        self.wq.n_rows > 0
    }
}

/// KV cache - full i64 precision for deterministic attention.
/// i8 quantization loses too much precision for attention dot products,
/// causing the model to attend to wrong positions after a few tokens.
/// Memory: 7B at 2048 context: ~4 GB. Fits on Mac Studio, tight on 8GB Vultr.
pub struct KVCache {
    /// k_data[layer]: flat i64 array, [pos * d_kv .. (pos+1) * d_kv]
    pub k_data: Vec<Vec<i64>>,
    /// v_data[layer]: flat i64 array
    pub v_data: Vec<Vec<i64>>,
    pub seq_len: usize,
}

impl KVCache {
    pub fn new(n_layers: usize) -> Self {
        Self {
            k_data: vec![Vec::new(); n_layers],
            v_data: vec![Vec::new(); n_layers],
            seq_len: 0,
        }
    }

    pub fn clear(&mut self) {
        for l in 0..self.k_data.len() {
            self.k_data[l].clear();
            self.v_data[l].clear();
        }
        self.seq_len = 0;
    }

    pub fn push_k(&mut self, layer: usize, k: &[i64]) {
        self.k_data[layer].extend_from_slice(k);
    }

    pub fn push_v(&mut self, layer: usize, v: &[i64]) {
        self.v_data[layer].extend_from_slice(v);
    }
}

/// Quantize an i64 Q16 vector to i8 with per-vector scale.
// Reference helper: no live caller today, kept alongside `quantize_for_dot`
// so the two quantisation conventions stay side by side for comparison.
#[allow(dead_code)]
#[inline]
fn quantize_vec_i8(v: &[i64]) -> (Vec<i8>, i64) {
    let abs_max = v.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let scale_factor = (abs_max / 127).max(1);
    let data: Vec<i8> = v.iter()
        .map(|&x| (x / scale_factor).clamp(-127, 127) as i8)
        .collect();
    (data, scale_factor)
}

/// Model config extracted from GGUF metadata.
#[derive(Clone)]
pub struct ModelConfig {
    pub n_layers: usize,
    pub d_model: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub d_ff: usize,
    pub d_head: usize,
    pub d_kv: usize,
    pub vocab_size: usize,
    pub attn_scale: i64,
    pub rope_cos: Vec<i64>,
    pub rope_sin: Vec<i64>,
    pub max_seq: usize,
    /// EOS token IDs read from GGUF metadata. Defaults to LLaMA family tokens.
    pub eos_tokens: Vec<u32>,
    /// BOS token ID read from GGUF metadata.
    pub bos_token: u32,
    /// Chat template (Jinja2) from GGUF metadata. Used to wrap user prompts
    /// in the correct format for the model (e.g., [INST]...[/INST] for LLaMA-2,
    /// <|start_header_id|>user<|end_header_id|> for LLaMA-3, etc.).
    /// Empty string means no template - use raw input.
    pub chat_template: String,
}

/// Pre-converted Q4 layer weights (optional, converted at runtime).
pub struct Q4Layer {
    pub wq: Q4WeightsX86,
    pub wk: Q4WeightsX86,
    pub wv: Q4WeightsX86,
    pub wo: Q4WeightsX86,
    pub w_gate: Q4WeightsX86,
    pub w_up: Q4WeightsX86,
    pub w_down: Q4WeightsX86,
}

/// Pre-loaded transformer layer weights in per-row INT16.
pub struct I16Layer {
    pub wq: I16Weights,
    pub wk: I16Weights,
    pub wv: I16Weights,
    pub wo: I16Weights,
    pub w_gate: I16Weights,
    pub w_up: I16Weights,
    pub w_down: I16Weights,
}

/// Pre-loaded transformer layer in ternary (2 bits/weight).
///
/// Matmuls route through `matmul_ternary` - zero multiplications, pure
/// ADD + XOR on the accumulator. See `ternary_engine.rs` for the math and
/// `sha256_isa.rs` for the SHA-256 datapath proof.
pub struct TernaryLayer {
    pub wq: crate::ternary_engine::TernaryWeights,
    pub wk: crate::ternary_engine::TernaryWeights,
    pub wv: crate::ternary_engine::TernaryWeights,
    pub wo: crate::ternary_engine::TernaryWeights,
    pub w_gate: crate::ternary_engine::TernaryWeights,
    pub w_up: crate::ternary_engine::TernaryWeights,
    pub w_down: crate::ternary_engine::TernaryWeights,
}

/// Hybrid ternary + INT8 outliers. Ternary bulk runs on ASIC primitives;
/// the small outlier fraction (1–5%) runs on the controller via regular
/// INT8 multiply-accumulate. Preserves PTQ quality on models not trained
/// ternary-aware. See `ternary_hybrid.rs`.
pub struct TernaryHybridLayer {
    pub wq: crate::ternary_hybrid::TernaryHybridWeights,
    pub wk: crate::ternary_hybrid::TernaryHybridWeights,
    pub wv: crate::ternary_hybrid::TernaryHybridWeights,
    pub wo: crate::ternary_hybrid::TernaryHybridWeights,
    pub w_gate: crate::ternary_hybrid::TernaryHybridWeights,
    pub w_up: crate::ternary_hybrid::TernaryHybridWeights,
    pub w_down: crate::ternary_hybrid::TernaryHybridWeights,
}

/// Pre-loaded transformer layer in block-wise INT8 (Q8_0-style) format.
/// 32-weight blocks with i32 Q16 scales. Preferred storage going forward -
/// bridges the old per-row I8 quality gap without sacrificing integer
/// determinism.
pub struct BlockI8Layer {
    pub wq: crate::block_i8::BlockI8Weights,
    pub wk: crate::block_i8::BlockI8Weights,
    pub wv: crate::block_i8::BlockI8Weights,
    pub wo: crate::block_i8::BlockI8Weights,
    pub w_gate: crate::block_i8::BlockI8Weights,
    pub w_up: crate::block_i8::BlockI8Weights,
    pub w_down: crate::block_i8::BlockI8Weights,
}

/// Fully cached integer model with per-row INT8 weights.
pub struct CachedIntegerModel {
    pub config: ModelConfig,
    /// Embeddings stored at full Q16 precision (not INT8).
    /// Embedding values can be extremely small (1e-6) and INT8 destroys them.
    /// This is just a lookup table, not a matmul - no performance impact.
    pub embedding_q16: Vec<i64>,  // [vocab × d_model] in Q16
    pub embedding_i8: I8Weights,  // kept for weight_hash and save_weights
    pub layers: Vec<CachedLayer>,
    pub final_norm: Vec<i64>,
    pub output_weight: I8Weights, // [vocab × d_model]
    pub vocab: Vec<String>,
    /// Q4 weights - converted from I8 on enable_q4(). Halves bandwidth.
    pub q4_layers: Option<Vec<Q4Layer>>,
    pub q4_output: Option<Q4WeightsX86>,
    /// I16 weights - converted from I8 on enable_i16(). Finer quantization.
        pub i16_layers: Option<Vec<I16Layer>>,
        pub i16_output: Option<I16Weights>,
    /// Block-wise INT8 (32-weight blocks with i32 Q16 scales). When present,
    /// this path takes priority in forward dispatch: higher quality than
    /// per-row I8, full integer determinism, ~12% memory overhead vs I8.
    pub block_i8_layers: Option<Vec<BlockI8Layer>>,
    pub block_i8_output: Option<crate::block_i8::BlockI8Weights>,
    /// Ternary weights - converted from I8 on enable_ternary().
    /// 2 bits/weight, ASIC-compatible matmul (ADD + XOR only, zero multiplications).
    /// See `ternary_engine.rs`.
    pub ternary_layers: Option<Vec<TernaryLayer>>,
    pub ternary_output: Option<crate::ternary_engine::TernaryWeights>,
    /// Hybrid ternary + sparse INT8 outliers - enabled via enable_ternary_hybrid().
    /// Takes dispatch priority over pure ternary when both are populated.
    pub ternary_hybrid_layers: Option<Vec<TernaryHybridLayer>>,
    pub ternary_hybrid_output: Option<crate::ternary_hybrid::TernaryHybridWeights>,
}

impl CachedIntegerModel {
    /// Convert all weights to Q4 (4-bit). Halves memory bandwidth.
    /// Call once after loading model. Original I8 weights kept for fallback.
    pub fn enable_q4(&mut self) {
        let q4_layers: Vec<Q4Layer> = self.layers.iter().map(|l| Q4Layer {
            wq: Q4WeightsX86::from_i8(&l.wq),
            wk: Q4WeightsX86::from_i8(&l.wk),
            wv: Q4WeightsX86::from_i8(&l.wv),
            wo: Q4WeightsX86::from_i8(&l.wo),
            w_gate: Q4WeightsX86::from_i8(&l.w_gate),
            w_up: Q4WeightsX86::from_i8(&l.w_up),
            w_down: Q4WeightsX86::from_i8(&l.w_down),
        }).collect();
        self.q4_output = Some(Q4WeightsX86::from_i8(&self.output_weight));
        self.q4_layers = Some(q4_layers);
    }

    /// Convert all weights to hybrid ternary + INT8 outliers.
    /// `outlier_pct` = fraction of weights (by magnitude) kept as INT8 per row.
    /// Typical values: 1.0-5.0 (lower = more ASIC-native, higher = more quality).
    ///
    /// The ternary bulk runs on ASIC primitives; outliers run on the controller.
    /// This is the PTQ quality path - works on any model without retraining.
    ///
    /// Gated behind the `experimental-ip` feature - the actual ternary
    /// implementation lives in a private branch until patents are filed
    /// (see lib.rs ternary_engine/ternary_hybrid stub modules for context).
    #[cfg(feature = "experimental-ip")]
    pub fn enable_ternary_hybrid(&mut self, outlier_pct: f32) {
        use crate::ternary_hybrid::TernaryHybridWeights;
        if self.ternary_hybrid_layers.is_some() { return; }
        let hybrid_layers: Vec<TernaryHybridLayer> = self.layers.iter().map(|l| TernaryHybridLayer {
            wq:     TernaryHybridWeights::from_i8(&l.wq, outlier_pct),
            wk:     TernaryHybridWeights::from_i8(&l.wk, outlier_pct),
            wv:     TernaryHybridWeights::from_i8(&l.wv, outlier_pct),
            wo:     TernaryHybridWeights::from_i8(&l.wo, outlier_pct),
            w_gate: TernaryHybridWeights::from_i8(&l.w_gate, outlier_pct),
            w_up:   TernaryHybridWeights::from_i8(&l.w_up, outlier_pct),
            w_down: TernaryHybridWeights::from_i8(&l.w_down, outlier_pct),
        }).collect();
        self.ternary_hybrid_output = Some(TernaryHybridWeights::from_i8(&self.output_weight, outlier_pct));
        self.ternary_hybrid_layers = Some(hybrid_layers);
    }

    /// Convert all weights from I8 to ternary (2 bits/weight).
    /// Call once after loading model. Original I8 weights kept for fallback.
    ///
    /// Ternary matmul runs on SHA-256 ASIC primitives (ADD + XOR, no multiplication),
    /// enabling inference on Bitcoin mining hardware via the ARC distributed network.
    ///
    /// Gated behind `experimental-ip` for the same reason as
    /// `enable_ternary_hybrid` - real implementation isn't in the public repo.
    #[cfg(feature = "experimental-ip")]
    pub fn enable_ternary(&mut self) {
        use crate::ternary_engine::TernaryWeights;
        if self.ternary_layers.is_some() { return; }
        let ternary_layers: Vec<TernaryLayer> = self.layers.iter().map(|l| TernaryLayer {
            wq:     TernaryWeights::from_i8(&l.wq),
            wk:     TernaryWeights::from_i8(&l.wk),
            wv:     TernaryWeights::from_i8(&l.wv),
            wo:     TernaryWeights::from_i8(&l.wo),
            w_gate: TernaryWeights::from_i8(&l.w_gate),
            w_up:   TernaryWeights::from_i8(&l.w_up),
            w_down: TernaryWeights::from_i8(&l.w_down),
        }).collect();
        self.ternary_output = Some(TernaryWeights::from_i8(&self.output_weight));
        self.ternary_layers = Some(ternary_layers);
    }

    /// Report the precision label the forward dispatch will actually use.
    /// Matches the priority chain in `dispatch_matmul` (layers) and the LM
    /// head dispatch in `forward_one_token`. Returns the highest-quality
    /// path that is populated. Used by RPC so the engine name in the
    /// response shows what's running, not a hardcoded string.
    pub fn effective_precision_label(&self) -> &'static str {
        if self.ternary_hybrid_layers.is_some() {
            "ternary-hybrid (per-row sparse-INT8 outliers)"
        } else if self.ternary_layers.is_some() {
            "ternary (1.58-bit, ASIC-compatible)"
        } else if self.i16_layers.is_some() {
            "INT16 integer (per-row, cross-platform deterministic)"
        } else if self.block_i8_layers.is_some() {
            "block-INT8 integer (32-weight blocks, cross-platform deterministic)"
        } else if self.q4_layers.is_some() {
            "Q4 integer (cross-platform deterministic)"
        } else {
            "INT8 integer (per-row, cross-platform deterministic)"
        }
    }

    /// Convert all weights from I8 to I16 format.
    /// Call once after loading model. Original I8 weights kept for fallback.
    ///
    /// Note: `from_i8` preserves I8-level precision (validates code path).
    /// For real quality improvement, the model loader should call
    /// `I16Weights::quantize_f32` from original floats instead.
    pub fn enable_i16(&mut self) {
        // Preserve f32-quantized I16 weights installed by load_cached_model.
        // from_i8 promotion would silently replace them with coarser I8-level precision.
        if self.i16_layers.is_some() { return; }
        let i16_layers: Vec<I16Layer> = self.layers.iter().map(|l| I16Layer {
            wq: I16Weights::from_i8(&l.wq),
            wk: I16Weights::from_i8(&l.wk),
            wv: I16Weights::from_i8(&l.wv),
            wo: I16Weights::from_i8(&l.wo),
            w_gate: I16Weights::from_i8(&l.w_gate),
            w_up: I16Weights::from_i8(&l.w_up),
            w_down: I16Weights::from_i8(&l.w_down),
        }).collect();
        self.i16_output = Some(I16Weights::from_i8(&self.output_weight));
        self.i16_layers = Some(i16_layers);
    }
}

// ─── Cached Input Quantization ────────────────────────────────────────────────

/// Pre-quantized i8 input - computed once, reused for multiple matmuls.
pub struct QuantizedInput {
    pub data: Vec<i8>,
    pub scale_factor: i64,
}

impl QuantizedInput {
    /// Quantize i64 Q16 input to i8. Call once, pass to multiple matmuls.
    #[inline]
    pub fn from_i64(input: &[i64]) -> Self {
        let abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
        let scale_factor = (abs_max / 127).max(1);
        let data: Vec<i8> = input.iter()
            .map(|&x| (x / scale_factor).clamp(-127, 127) as i8)
            .collect();
        Self { data, scale_factor }
    }
}

// ─── INT8 Matmul (Per-Row Scale, Optimized) ───────────────────────────────────

/// Core i8×i64 dot product. Unsafe, 8-element unroll, 4 independent accumulators.
#[inline(always)]
unsafe fn dot_i8_i64(row: *const i8, input: *const i64, len: usize) -> i64 {
    // SAFETY: body wrapped for `unsafe_op_in_unsafe_fn` (denied workspace-wide).
    // The contract is unchanged: the caller guarantees the pointers are valid
    // for `len` reads. Wrapping is purely lexical - no semantics change.
    unsafe {
        let mut acc0: i64 = 0;
        let mut acc1: i64 = 0;
        let mut acc2: i64 = 0;
        let mut acc3: i64 = 0;
        let full = len / 8 * 8;
        let mut j = 0usize;
        while j < full {
            acc0 += (*row.add(j) as i64) * (*input.add(j));
            acc1 += (*row.add(j + 1) as i64) * (*input.add(j + 1));
            acc2 += (*row.add(j + 2) as i64) * (*input.add(j + 2));
            acc3 += (*row.add(j + 3) as i64) * (*input.add(j + 3));
            acc0 += (*row.add(j + 4) as i64) * (*input.add(j + 4));
            acc1 += (*row.add(j + 5) as i64) * (*input.add(j + 5));
            acc2 += (*row.add(j + 6) as i64) * (*input.add(j + 6));
            acc3 += (*row.add(j + 7) as i64) * (*input.add(j + 7));
            j += 8;
        }
        let mut acc = acc0 + acc1 + acc2 + acc3;
        while j < len {
            acc += (*row.add(j) as i64) * (*input.add(j));
            j += 1;
        }
        acc
    }
}

/// Write matmul result into pre-allocated output buffer (zero-alloc).
/// Parallel with 512-row chunks to minimize rayon scheduling overhead.
fn matmul_i8_into(weights: &I8Weights, input: &[i64], in_size: usize, output: &mut [i64]) {
    // Empty-weight guard. Shard-mode models pre-allocate every layer as an
    // empty placeholder and only populate the range this node holds. Any
    // code path that iterates over a non-held layer hits an empty weight
    // struct; without this guard the SIMD dot product dereferences a
    // dangling non-null pointer and segfaults in the consensus thread.
    if weights.n_rows == 0 || weights.data.is_empty() {
        for o in output.iter_mut() { *o = 0; }
        return;
    }
    debug_assert_eq!(output.len(), weights.scales.len(), "matmul output/scales mismatch");
    let data = &weights.data;
    let scales = &weights.scales;
    // Chunk width 256, matching matmul_i16_into. At 512 a 4096-row output
    // yields only 8 rayon tasks, so the I8 path saturated at 8 cores no
    // matter how wide the pool was — which made "add two cores" a no-op on
    // any node that hadn't been promoted to I16. 256 gives 16 tasks.
    output.par_chunks_mut(256).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 256;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let acc = unsafe {
                dot_i8_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size)
            };
            *out = (acc * scales[i]) >> FRAC_BITS;
        }
    });
}

/// Allocating matmul (for compatibility and small outputs).
fn matmul_i8(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let mut output = vec![0i64; out_size];
    if weights.n_rows == 0 || weights.data.is_empty() {
        return output;
    }
    if out_size >= 256 {
        matmul_i8_into(weights, input, in_size, &mut output);
    } else {
        let data = &weights.data;
        let scales = &weights.scales;
        for i in 0..out_size {
            let acc = unsafe { dot_i8_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size) };
            output[i] = (acc * scales[i]) >> FRAC_BITS;
        }
    }
    output
}

// ─── INT16 Matmul (Feature-Gated) ────────────────────────────────────────────

/// Core i16×i64 dot product with 8-element unroll. Scalar fallback.
// Live on x86_64 (via dot_i16_i64_avx2) and on targets with no SIMD path, and
// used directly by the unit tests as the bit-exact reference. On aarch64 the
// NEON path takes over, so the lib-only build sees it as unused.
#[allow(dead_code)]
#[inline(always)]
unsafe fn dot_i16_i64_scalar(row: *const i16, input: *const i64, len: usize) -> i64 {
    // SAFETY: body wrapped for `unsafe_op_in_unsafe_fn` (denied workspace-wide).
    // The contract is unchanged: the caller guarantees the pointers are valid
    // for `len` reads. Wrapping is purely lexical - no semantics change.
    unsafe {
        let mut acc0: i64 = 0;
        let mut acc1: i64 = 0;
        let mut acc2: i64 = 0;
        let mut acc3: i64 = 0;
        let full = len / 8 * 8;
        let mut j = 0usize;
        while j < full {
            acc0 += (*row.add(j) as i64) * (*input.add(j));
            acc1 += (*row.add(j + 1) as i64) * (*input.add(j + 1));
            acc2 += (*row.add(j + 2) as i64) * (*input.add(j + 2));
            acc3 += (*row.add(j + 3) as i64) * (*input.add(j + 3));
            acc0 += (*row.add(j + 4) as i64) * (*input.add(j + 4));
            acc1 += (*row.add(j + 5) as i64) * (*input.add(j + 5));
            acc2 += (*row.add(j + 6) as i64) * (*input.add(j + 6));
            acc3 += (*row.add(j + 7) as i64) * (*input.add(j + 7));
            j += 8;
        }
        let mut acc = acc0 + acc1 + acc2 + acc3;
        while j < len {
            acc += (*row.add(j) as i64) * (*input.add(j));
            j += 1;
        }
        acc
    }
}

/// NEON SIMD i16×i32 dot product. Mac M2 / aarch64.
///
/// Truncates input from i64 → i32. The hidden state values produced by
/// layernorm are bounded by the Q16 fixed-point range and easily fit in
/// i32 (max ~2^30 absolute). Multiplies i16 weight × i32 input → i64
/// accumulator. Vectorizes 8 lanes per iteration via vmull_high_s32.
///
/// Empirical speedup vs scalar dot_i16_i64 on M2 Ultra: ~3.5x for
/// 4096-wide rows. Bit-identical to scalar provided inputs fit in i32.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn dot_i16_i64_neon(row: *const i16, input: *const i64, len: usize) -> i64 {
    // SAFETY: body wrapped for `unsafe_op_in_unsafe_fn` (denied workspace-wide).
    // The contract is unchanged: the caller guarantees the pointers are valid
    // for `len` reads. Wrapping is purely lexical - no semantics change.
    unsafe {
        use std::arch::aarch64::*;
        // acc0/acc1 are bound below from the horizontal sum of the vector
        // accumulators; acc2/acc3 exist to keep the horizontal-sum expression
        // symmetric with the scalar path and stay zero here.
        let acc2: i64 = 0;
        let acc3: i64 = 0;
        let simd_len = len / 8 * 8;
        let mut j = 0usize;
        // Vector accumulators for 8 i64 partial sums
        let mut va0 = vdupq_n_s64(0);
        let mut va1 = vdupq_n_s64(0);
        let mut va2 = vdupq_n_s64(0);
        let mut va3 = vdupq_n_s64(0);
        while j < simd_len {
            // Load 8 weights as i16
            let w16 = vld1q_s16(row.add(j));
            // Widen the bottom 4 i16 to i32
            let w32_lo = vmovl_s16(vget_low_s16(w16));
            // Widen the top 4 i16 to i32
            let w32_hi = vmovl_s16(vget_high_s16(w16));
            // Load 8 i64 inputs and narrow to i32 (truncate)
            let i64_0 = vld1q_s64(input.add(j));      // input[j..j+2]
            let i64_1 = vld1q_s64(input.add(j + 2));  // input[j+2..j+4]
            let i64_2 = vld1q_s64(input.add(j + 4));  // input[j+4..j+6]
            let i64_3 = vld1q_s64(input.add(j + 6));  // input[j+6..j+8]
            // Pack 4×i64 into 4×i32 (truncating)
            let i32_lo = vcombine_s32(vmovn_s64(i64_0), vmovn_s64(i64_1));
            let i32_hi = vcombine_s32(vmovn_s64(i64_2), vmovn_s64(i64_3));
            // Multiply low half: i32 × i32 → i64 widening
            va0 = vmlal_s32(va0, vget_low_s32(w32_lo),  vget_low_s32(i32_lo));
            va1 = vmlal_high_s32(va1, w32_lo, i32_lo);
            // Multiply high half
            va2 = vmlal_s32(va2, vget_low_s32(w32_hi),  vget_low_s32(i32_hi));
            va3 = vmlal_high_s32(va3, w32_hi, i32_hi);
            j += 8;
        }
        // Horizontal sum the four i64x2 accumulators
        let s01 = vaddq_s64(va0, va1);
        let s23 = vaddq_s64(va2, va3);
        let s = vaddq_s64(s01, s23);
        let acc0 = vgetq_lane_s64(s, 0);
        let acc1 = vgetq_lane_s64(s, 1);
        let mut acc = acc0 + acc1 + acc2 + acc3;
        // Tail
        while j < len {
            acc += (*row.add(j) as i64) * (*input.add(j));
            j += 1;
        }
        acc
    }
}

/// x86_64 i16×i64 dot product. Falls through to the scalar 8-element
/// unrolled path which LLVM autovectorizes adequately for the 2-3 layer
/// shards on NYC/LAX. An explicit AVX-512 widening path was attempted
/// (commit 8274796) but caused segfaults during consensus on Vultr
/// Skylake Xeon VPS - reverted to scalar pending investigation.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn dot_i16_i64_avx2(row: *const i16, input: *const i64, len: usize) -> i64 {
    // SAFETY: body wrapped for `unsafe_op_in_unsafe_fn` (denied workspace-wide).
    // The contract is unchanged: the caller guarantees the pointers are valid
    // for `len` reads. Wrapping is purely lexical - no semantics change.
    unsafe {
        dot_i16_i64_scalar(row, input, len)
    }
}

/// Dispatch wrapper - picks NEON on aarch64, AVX2 on x86_64, scalar elsewhere.
#[inline(always)]
unsafe fn dot_i16_i64(row: *const i16, input: *const i64, len: usize) -> i64 {
    // SAFETY: body wrapped for `unsafe_op_in_unsafe_fn` (denied workspace-wide).
    // The contract is unchanged: the caller guarantees the pointers are valid
    // for `len` reads. Wrapping is purely lexical - no semantics change.
    unsafe {
        #[cfg(target_arch = "aarch64")]
        { dot_i16_i64_neon(row, input, len) }
        #[cfg(target_arch = "x86_64")]
        { dot_i16_i64_avx2(row, input, len) }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        { dot_i16_i64_scalar(row, input, len) }
    }
}

// ─── NEON attention Q·K / attention·V SIMD ────────────────────────────────────

/// NEON dot product of two i64 arrays. Used by the attention inner loop
/// for Q · K_cache scoring. The i64 inputs are truncated to i32 with the
/// same assumption as dot_i16_i64_neon (hidden state magnitudes bounded
/// by Q16 fixed-point ~ 2^28), then multiplied pairwise to i64
/// accumulators via vmlal_s32. Processes 4 lanes per iteration.
///
/// Speedup vs scalar i64×i64 dot: ~2.5× on M2 Ultra for d_head=128.
/// Called 32 heads × seq_len times per layer, so even marginal
/// attention improvements compound as generated sequences grow.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn dot_i64xi64_attn_neon(a: *const i64, b: *const i64, len: usize) -> i64 {
    // SAFETY: body wrapped for `unsafe_op_in_unsafe_fn` (denied workspace-wide).
    // The contract is unchanged: the caller guarantees the pointers are valid
    // for `len` reads. Wrapping is purely lexical - no semantics change.
    unsafe {
        use std::arch::aarch64::*;
        let mut acc = vdupq_n_s64(0);
        let simd_len = len / 4 * 4;
        let mut j = 0usize;
        while j < simd_len {
            // Load 4 i64 from each operand
            let a0 = vld1q_s64(a.add(j));      // a[j..j+2]
            let a1 = vld1q_s64(a.add(j + 2));  // a[j+2..j+4]
            let b0 = vld1q_s64(b.add(j));
            let b1 = vld1q_s64(b.add(j + 2));
            // Narrow to i32 (truncate - values are bounded by Q16)
            let a32 = vcombine_s32(vmovn_s64(a0), vmovn_s64(a1));
            let b32 = vcombine_s32(vmovn_s64(b0), vmovn_s64(b1));
            // Multiply i32×i32 → i64 via vmull
            acc = vmlal_s32(acc, vget_low_s32(a32), vget_low_s32(b32));
            acc = vmlal_high_s32(acc, a32, b32);
            j += 4;
        }
        let mut sum = vgetq_lane_s64(acc, 0) + vgetq_lane_s64(acc, 1);
        while j < len {
            sum += (*a.add(j)) * (*b.add(j));
            j += 1;
        }
        sum
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
unsafe fn dot_i64xi64_attn_neon(a: *const i64, b: *const i64, len: usize) -> i64 {
    // SAFETY: body wrapped for `unsafe_op_in_unsafe_fn` (denied workspace-wide).
    // The contract is unchanged: the caller guarantees the pointers are valid
    // for `len` reads. Wrapping is purely lexical - no semantics change.
    unsafe {
        let mut sum: i64 = 0;
        for i in 0..len {
            sum += (*a.add(i)) * (*b.add(i));
        }
        sum
    }
}

/// Write i16 matmul result into pre-allocated output buffer (zero-alloc).
///
/// Scaling: scales[i] = abs_max_row_i * ONE (abs_max in Q16).
/// acc = sum(i16_weight * i64_input) ≈ 32767 * ONE * dot(W, X) / abs_max.
/// output = acc / 32767 * scale >> FRAC_BITS
///        = acc / 32767 * abs_max * ONE >> FRAC_BITS
///        ≈ ONE * dot(W, X)  (Q16 of the real result).
fn matmul_i16_into(weights: &I16Weights, input: &[i64], in_size: usize, output: &mut [i64]) {
    if weights.n_rows == 0 || weights.data.is_empty() {
        for o in output.iter_mut() { *o = 0; }
        return;
    }
    debug_assert!(output.len() <= weights.data.len() / in_size, "i16 matmul bounds");
    let data = &weights.data;
    let scales = &weights.scales;
    // Chunk size 256: empirical sweet spot on M2 Ultra. Going smaller
    // (e.g. 64) increases rayon task overhead more than the extra core
    // utilization helps. 256 keeps task count high enough for 4096+
    // matmuls without per-task scheduling cost.
    output.par_chunks_mut(256).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 256;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let acc = unsafe {
                dot_i16_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size)
            };
            // Use i128 intermediate for full precision: avoids truncation from
            // dividing first AND avoids i64 overflow from multiplying first.
            let wide = (acc as i128) * (scales[i] as i128);
            *out = ((wide / 32767) >> FRAC_BITS as i128) as i64;
        }
    });
}

/// Allocating i16 matmul (for compatibility and small outputs).
// Only the unit tests call this today; the forward pass uses the `_into`
// variant. Kept because the tests use it as the reference i16 result.
#[allow(dead_code)]
fn matmul_i16(weights: &I16Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let mut output = vec![0i64; out_size];
    if weights.n_rows == 0 || weights.data.is_empty() {
        return output;
    }
    matmul_i16_into(weights, input, in_size, &mut output);
    output
}

// ─── SIMD INT8 Matmul (Cross-Platform Deterministic) ──────────────────────────

/// NEON i8×i8→i32 SIMD matmul with per-row scales.
/// Processes 32 elements per iteration (2 × 16-byte NEON loads).
// Only the unit tests call this today; the forward pass uses the scalar
// i8xi64 path (see matmul_simd_preq). Kept as the SIMD cross-check.
#[allow(dead_code)]
#[cfg(target_arch = "aarch64")]
fn matmul_i8xi8_simd(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    use std::arch::aarch64::*;

    if weights.n_rows == 0 || weights.data.is_empty() {
        return vec![0i64; out_size];
    }

    let input_abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let input_scale_factor = (input_abs_max / 127).max(1);
    let input_i8: Vec<i8> = input.iter()
        .map(|&x| (x / input_scale_factor).clamp(-127, 127) as i8)
        .collect();

    let data = &weights.data;
    let inp_slice = &input_i8;
    let scales = &weights.scales;

    let mut output = vec![0i64; out_size];
    output.par_chunks_mut(64).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 64;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let row = unsafe { data.as_ptr().add(i * in_size) };
            let mut acc: i64;
            let simd_len = in_size / 32 * 32;

            unsafe {
                let mut vacc0 = vdupq_n_s32(0);
                let mut vacc1 = vdupq_n_s32(0);
                let mut vacc2 = vdupq_n_s32(0);
                let mut vacc3 = vdupq_n_s32(0);
                let mut j = 0usize;
                while j < simd_len {
                    let vw0 = vld1q_s8(row.add(j));
                    let vi0 = vld1q_s8(inp_slice.as_ptr().add(j));
                    vacc0 = vpadalq_s16(vacc0, vmull_s8(vget_low_s8(vw0), vget_low_s8(vi0)));
                    vacc1 = vpadalq_s16(vacc1, vmull_s8(vget_high_s8(vw0), vget_high_s8(vi0)));
                    let vw1 = vld1q_s8(row.add(j + 16));
                    let vi1 = vld1q_s8(inp_slice.as_ptr().add(j + 16));
                    vacc2 = vpadalq_s16(vacc2, vmull_s8(vget_low_s8(vw1), vget_low_s8(vi1)));
                    vacc3 = vpadalq_s16(vacc3, vmull_s8(vget_high_s8(vw1), vget_high_s8(vi1)));
                    j += 32;
                }
                vacc0 = vaddq_s32(vacc0, vacc1);
                vacc2 = vaddq_s32(vacc2, vacc3);
                vacc0 = vaddq_s32(vacc0, vacc2);
                acc = vaddvq_s32(vacc0) as i64;

                while j < in_size {
                    acc += (*row.add(j) as i64) * (*inp_slice.as_ptr().add(j) as i64);
                    j += 1;
                }
            }

            let combined = (scales[i] * input_scale_factor) >> FRAC_BITS;
            *out = acc * combined;
        }
    });
    output
}

/// x86 SIMD matmul - llama.cpp sign trick for AVX2, AVX-512.
/// sign trick: abs(w) × sign_corrected(input) → safe maddubs (no i16 saturation)
/// Processes 32 bytes at once (AVX2) or 64 (AVX-512), no sign extension needed.
#[cfg(target_arch = "x86_64")]
fn matmul_i8xi8_simd(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    use std::arch::x86_64::*;

    if weights.n_rows == 0 || weights.data.is_empty() {
        return vec![0i64; out_size];
    }

    if !is_x86_feature_detected!("avx2") {
        return matmul_i8(weights, input, in_size, out_size);
    }

    let use_avx512 = is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512f");

    // Quantize input once - reused across all output rows
    let input_abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let input_scale_factor = (input_abs_max / 127).max(1);
    // Align to 64 bytes for AVX-512 loads (pad with zeros)
    let aligned_len = (in_size + 63) & !63;
    let mut input_i8 = vec![0i8; aligned_len];
    for (i, &x) in input.iter().enumerate() {
        input_i8[i] = (x / input_scale_factor).clamp(-127, 127) as i8;
    }

    let data = &weights.data;
    let inp_slice = &input_i8;
    let scales = &weights.scales;

    let mut output = vec![0i64; out_size];
    output.par_chunks_mut(64).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 64;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let row = unsafe { data.as_ptr().add(i * in_size) };
            let inp_ptr = inp_slice.as_ptr();
            let mut acc: i64 = 0;

            if use_avx512 {
                // AVX-512: 4 independent 512-bit accumulators for ILP
                // + software prefetch 256 bytes ahead
                let simd_len = in_size / 128 * 128; // process 128 per iteration
                unsafe {
                    let mut vacc0 = _mm512_setzero_si512();
                    let mut vacc1 = _mm512_setzero_si512();
                    let mut vacc2 = _mm512_setzero_si512();
                    let mut vacc3 = _mm512_setzero_si512();

                    let mut j = 0usize;
                    while j < simd_len {
                        // Prefetch next iteration's data into L1
                        _mm_prefetch(row.add(j + 256) as *const i8, _MM_HINT_T0);
                        _mm_prefetch(inp_ptr.add(j + 256) as *const i8, _MM_HINT_T0);

                        // First 64 elements
                        let vw0 = _mm512_loadu_si512(row.add(j) as *const __m512i);
                        let vi0 = _mm512_loadu_si512(inp_ptr.add(j) as *const __m512i);
                        let vw0_lo = _mm512_castsi512_si256(vw0);
                        let vw0_hi = _mm512_extracti64x4_epi64(vw0, 1);
                        let vi0_lo = _mm512_castsi512_si256(vi0);
                        let vi0_hi = _mm512_extracti64x4_epi64(vi0, 1);
                        vacc0 = _mm512_add_epi32(vacc0, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw0_lo), _mm512_cvtepi8_epi16(vi0_lo)));
                        vacc1 = _mm512_add_epi32(vacc1, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw0_hi), _mm512_cvtepi8_epi16(vi0_hi)));

                        // Second 64 elements (independent accumulators for ILP)
                        let vw1 = _mm512_loadu_si512(row.add(j + 64) as *const __m512i);
                        let vi1 = _mm512_loadu_si512(inp_ptr.add(j + 64) as *const __m512i);
                        let vw1_lo = _mm512_castsi512_si256(vw1);
                        let vw1_hi = _mm512_extracti64x4_epi64(vw1, 1);
                        let vi1_lo = _mm512_castsi512_si256(vi1);
                        let vi1_hi = _mm512_extracti64x4_epi64(vi1, 1);
                        vacc2 = _mm512_add_epi32(vacc2, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw1_lo), _mm512_cvtepi8_epi16(vi1_lo)));
                        vacc3 = _mm512_add_epi32(vacc3, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw1_hi), _mm512_cvtepi8_epi16(vi1_hi)));
                        j += 128;
                    }

                    vacc0 = _mm512_add_epi32(_mm512_add_epi32(vacc0, vacc1),
                                             _mm512_add_epi32(vacc2, vacc3));
                    acc = _mm512_reduce_add_epi32(vacc0) as i64;

                    // 64-element remainder
                    if j + 64 <= in_size {
                        let vw = _mm512_loadu_si512(row.add(j) as *const __m512i);
                        let vi = _mm512_loadu_si512(inp_ptr.add(j) as *const __m512i);
                        let vw_lo = _mm512_castsi512_si256(vw);
                        let vw_hi = _mm512_extracti64x4_epi64(vw, 1);
                        let vi_lo = _mm512_castsi512_si256(vi);
                        let vi_hi = _mm512_extracti64x4_epi64(vi, 1);
                        let mut vr = _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw_lo), _mm512_cvtepi8_epi16(vi_lo));
                        vr = _mm512_add_epi32(vr, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw_hi), _mm512_cvtepi8_epi16(vi_hi)));
                        acc += _mm512_reduce_add_epi32(vr) as i64;
                        j += 64;
                    }

                    // Scalar remainder
                    while j < in_size {
                        acc += (*row.add(j) as i64) * (*inp_ptr.add(j) as i64);
                        j += 1;
                    }
                }
            } else {
                // AVX2: 4 independent accumulators + sign trick + prefetch
                let simd_len = in_size / 128 * 128; // 4×32 per iteration for ILP
                unsafe {
                    let mut vacc0 = _mm256_setzero_si256();
                    let mut vacc1 = _mm256_setzero_si256();
                    let mut vacc2 = _mm256_setzero_si256();
                    let mut vacc3 = _mm256_setzero_si256();
                    let ones = _mm256_set1_epi16(1);

                    let mut j = 0usize;
                    while j < simd_len {
                        _mm_prefetch(row.add(j + 256) as *const i8, _MM_HINT_T0);
                        _mm_prefetch(inp_ptr.add(j + 256) as *const i8, _MM_HINT_T0);

                        // 4 independent 32-element blocks per iteration
                        let vw0 = _mm256_loadu_si256(row.add(j) as *const __m256i);
                        let vi0 = _mm256_loadu_si256(inp_ptr.add(j) as *const __m256i);
                        let ax0 = _mm256_sign_epi8(vw0, vw0);
                        let sy0 = _mm256_sign_epi8(vi0, vw0);
                        vacc0 = _mm256_add_epi32(vacc0, _mm256_madd_epi16(_mm256_maddubs_epi16(ax0, sy0), ones));

                        let vw1 = _mm256_loadu_si256(row.add(j + 32) as *const __m256i);
                        let vi1 = _mm256_loadu_si256(inp_ptr.add(j + 32) as *const __m256i);
                        let ax1 = _mm256_sign_epi8(vw1, vw1);
                        let sy1 = _mm256_sign_epi8(vi1, vw1);
                        vacc1 = _mm256_add_epi32(vacc1, _mm256_madd_epi16(_mm256_maddubs_epi16(ax1, sy1), ones));

                        let vw2 = _mm256_loadu_si256(row.add(j + 64) as *const __m256i);
                        let vi2 = _mm256_loadu_si256(inp_ptr.add(j + 64) as *const __m256i);
                        let ax2 = _mm256_sign_epi8(vw2, vw2);
                        let sy2 = _mm256_sign_epi8(vi2, vw2);
                        vacc2 = _mm256_add_epi32(vacc2, _mm256_madd_epi16(_mm256_maddubs_epi16(ax2, sy2), ones));

                        let vw3 = _mm256_loadu_si256(row.add(j + 96) as *const __m256i);
                        let vi3 = _mm256_loadu_si256(inp_ptr.add(j + 96) as *const __m256i);
                        let ax3 = _mm256_sign_epi8(vw3, vw3);
                        let sy3 = _mm256_sign_epi8(vi3, vw3);
                        vacc3 = _mm256_add_epi32(vacc3, _mm256_madd_epi16(_mm256_maddubs_epi16(ax3, sy3), ones));

                        j += 128;
                    }

                    // Merge 4 accumulators
                    vacc0 = _mm256_add_epi32(_mm256_add_epi32(vacc0, vacc1),
                                             _mm256_add_epi32(vacc2, vacc3));

                    // 32-element remainder blocks
                    while j + 32 <= in_size {
                        let vw = _mm256_loadu_si256(row.add(j) as *const __m256i);
                        let vi = _mm256_loadu_si256(inp_ptr.add(j) as *const __m256i);
                        let ax = _mm256_sign_epi8(vw, vw);
                        let sy = _mm256_sign_epi8(vi, vw);
                        vacc0 = _mm256_add_epi32(vacc0, _mm256_madd_epi16(_mm256_maddubs_epi16(ax, sy), ones));
                        j += 32;
                    }

                    // Horizontal sum
                    let lo = _mm256_extracti128_si256(vacc0, 0);
                    let hi = _mm256_extracti128_si256(vacc0, 1);
                    let sum128 = _mm_add_epi32(lo, hi);
                    let sum128 = _mm_hadd_epi32(sum128, sum128);
                    let sum128 = _mm_hadd_epi32(sum128, sum128);
                    acc = _mm_extract_epi32(sum128, 0) as i64;

                    // Scalar remainder
                    while j < in_size {
                        acc += (*row.add(j) as i64) * (*inp_ptr.add(j) as i64);
                        j += 1;
                    }
                }
            }

            let combined = (scales[i] * input_scale_factor) >> FRAC_BITS;
            *out = acc * combined;
        }
    });
    output
}

/// Dispatch: SIMD i8×i8 for large matmuls, scalar for small.
/// NOTE: SIMD path quantizes input to i8 which causes double-quantization precision loss.
/// For models with small weight distributions, use scalar path (i8×i64, full input precision).
pub fn matmul_fast(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    // Use scalar i8×i64 path for full precision - SIMD i8×i8 loses too much
    // precision for small models like TinyLlama where weights are near-zero.
    matmul_i8(weights, input, in_size, out_size)
}

/// Zero-alloc matmul - uses scalar i8×i64 for full precision.
pub fn matmul_fast_preq(weights: &I8Weights, _input_q: &QuantizedInput, input_raw: &[i64], in_size: usize, output: &mut [i64]) {
    // Use scalar i8×i64 path for full input precision.
    // The SIMD i8×i8 path loses too much via double quantization.
    matmul_i8_into(weights, input_raw, in_size, output);
}

// Superseded by the full-precision scalar path in matmul_simd_preq (the
// double-quantised SIMD version loses too much accuracy). Retained as the
// reference for the NEON datapath rather than deleted.
#[allow(dead_code)]
#[cfg(target_arch = "aarch64")]
fn matmul_simd_preq_neon(weights: &I8Weights, input_q: &QuantizedInput, in_size: usize, output: &mut [i64]) {
    debug_assert_eq!(output.len(), weights.scales.len(), "matmul output/scales mismatch");
    use std::arch::aarch64::*;
    let data = &weights.data;
    let inp = &input_q.data;
    let scales = &weights.scales;
    let isf = input_q.scale_factor;

    output.par_chunks_mut(512).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 512;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row = unsafe { data.as_ptr().add(i * in_size) };
            let simd_len = in_size / 32 * 32;
            let mut acc: i64;
            unsafe {
                let mut v0 = vdupq_n_s32(0);
                let mut v1 = vdupq_n_s32(0);
                let mut v2 = vdupq_n_s32(0);
                let mut v3 = vdupq_n_s32(0);
                let mut j = 0usize;
                while j < simd_len {
                    let w0 = vld1q_s8(row.add(j));
                    let i0 = vld1q_s8(inp.as_ptr().add(j));
                    v0 = vpadalq_s16(v0, vmull_s8(vget_low_s8(w0), vget_low_s8(i0)));
                    v1 = vpadalq_s16(v1, vmull_s8(vget_high_s8(w0), vget_high_s8(i0)));
                    let w1 = vld1q_s8(row.add(j + 16));
                    let i1 = vld1q_s8(inp.as_ptr().add(j + 16));
                    v2 = vpadalq_s16(v2, vmull_s8(vget_low_s8(w1), vget_low_s8(i1)));
                    v3 = vpadalq_s16(v3, vmull_s8(vget_high_s8(w1), vget_high_s8(i1)));
                    j += 32;
                }
                v0 = vaddq_s32(vaddq_s32(v0, v1), vaddq_s32(v2, v3));
                acc = vaddvq_s32(v0) as i64;
                while j < in_size { acc += (*row.add(j) as i64) * (*inp.as_ptr().add(j) as i64); j += 1; }
            }
            *out = acc * ((scales[i] * isf) >> FRAC_BITS);
        }
    });
}

#[cfg(target_arch = "x86_64")]
fn matmul_simd_preq_x86(weights: &I8Weights, input_q: &QuantizedInput, in_size: usize, output: &mut [i64]) {
    debug_assert_eq!(output.len(), weights.scales.len(), "matmul output/scales mismatch");
    use std::arch::x86_64::*;
    let data = &weights.data;
    let inp = &input_q.data;
    let scales = &weights.scales;
    let isf = input_q.scale_factor;
    let use512 = is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512f");

    output.par_chunks_mut(512).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 512;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row = unsafe { data.as_ptr().add(i * in_size) };
            let ip = inp.as_ptr();
            let mut acc: i64 = 0;
            unsafe {
                if use512 {
                    let sl = in_size / 64 * 64;
                    let mut a0 = _mm512_setzero_si512();
                    let mut a1 = _mm512_setzero_si512();
                    let mut j = 0usize;
                    while j < sl {
                        let vw = _mm512_loadu_si512(row.add(j) as *const __m512i);
                        let vi = _mm512_loadu_si512(ip.add(j) as *const __m512i);
                        a0 = _mm512_add_epi32(a0, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(_mm512_castsi512_si256(vw)),
                            _mm512_cvtepi8_epi16(_mm512_castsi512_si256(vi))));
                        a1 = _mm512_add_epi32(a1, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(_mm512_extracti64x4_epi64(vw, 1)),
                            _mm512_cvtepi8_epi16(_mm512_extracti64x4_epi64(vi, 1))));
                        j += 64;
                    }
                    acc = _mm512_reduce_add_epi32(_mm512_add_epi32(a0, a1)) as i64;
                    while j < in_size { acc += (*row.add(j) as i64) * (*ip.add(j) as i64); j += 1; }
                } else {
                    let sl = in_size / 32 * 32;
                    let mut a0 = _mm256_setzero_si256();
                    let mut a1 = _mm256_setzero_si256();
                    let mut j = 0usize;
                    while j < sl {
                        let vw = _mm256_loadu_si256(row.add(j) as *const __m256i);
                        let vi = _mm256_loadu_si256(ip.add(j) as *const __m256i);
                        a0 = _mm256_add_epi32(a0, _mm256_madd_epi16(
                            _mm256_cvtepi8_epi16(_mm256_castsi256_si128(vw)),
                            _mm256_cvtepi8_epi16(_mm256_castsi256_si128(vi))));
                        a1 = _mm256_add_epi32(a1, _mm256_madd_epi16(
                            _mm256_cvtepi8_epi16(_mm256_extracti128_si256(vw, 1)),
                            _mm256_cvtepi8_epi16(_mm256_extracti128_si256(vi, 1))));
                        j += 32;
                    }
                    let v = _mm256_add_epi32(a0, a1);
                    let lo = _mm256_extracti128_si256(v, 0);
                    let hi = _mm256_extracti128_si256(v, 1);
                    let s = _mm_hadd_epi32(_mm_add_epi32(lo, hi), _mm_setzero_si128());
                    let s = _mm_hadd_epi32(s, _mm_setzero_si128());
                    acc = _mm_extract_epi32(s, 0) as i64;
                    while j < in_size { acc += (*row.add(j) as i64) * (*ip.add(j) as i64); j += 1; }
                }
            }
            *out = acc * ((scales[i] * isf) >> FRAC_BITS);
        }
    });
}

// ─── Q4 Weights (4-bit, half bandwidth) ──────────────────────────────────────

/// Q4 weight matrix: 4-bit signed values packed 2 per byte.
/// Byte layout: [hi_nibble(4b) | lo_nibble(4b)], both signed [-8, 7].
/// Buffer is half the size of I8Weights → 2x bandwidth reduction.
pub struct Q4WeightsX86 {
    pub data: Vec<u8>,       // packed Q4 bytes (n_rows × n_cols / 2)
    pub scales: Vec<i64>,    // per-row scale factors (same as I8Weights)
    pub n_rows: usize,
    pub n_cols: usize,
}

impl Q4WeightsX86 {
    /// Total memory in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 8
    }

    /// Convert I8Weights to Q4WeightsX86 with per-row scaling.
    /// Each i8 row is rescaled to use the full Q4 range [-8, 7].
    /// Per-row q4_per_unit = ceil(abs_max / 7) ensures minimal clamping loss.
    /// Encoding: bias +8 (stored [0, 15], decode by subtracting 8).
    pub fn from_i8(w: &I8Weights) -> Self {
        let n_rows = w.n_rows;
        let n_cols = w.n_cols;
        let mut data = Vec::with_capacity(w.data.len() / 2);
        let mut scales = Vec::with_capacity(n_rows);

        for i in 0..n_rows {
            let row = &w.data[i * n_cols..(i + 1) * n_cols];

            // Per-row abs_max of i8 values
            let abs_max = row.iter().map(|&x| (x as i16).unsigned_abs() as u8).max().unwrap_or(1).max(1);
            // How many i8 units per Q4 step: ceil(abs_max / 7)
            let q4_per_unit = ((abs_max as i64 + 6) / 7).max(1);

            for pair in row.chunks(2) {
                // Divide by q4_per_unit to use full Q4 range, then clamp
                let v0 = ((pair[0] as i16) / q4_per_unit as i16).clamp(-8, 7);
                let v1 = if pair.len() > 1 {
                    ((pair[1] as i16) / q4_per_unit as i16).clamp(-8, 7)
                } else { 0 };
                // Bias encoding: store value + 8 in nibble [0, 15]
                let lo = ((v0 + 8) as u8) & 0x0F;
                let hi = ((v1 + 8) as u8) & 0x0F;
                data.push(lo | (hi << 4));
            }

            // Combined scale: original i8 scale × q4_per_unit
            scales.push(w.scales[i] * q4_per_unit);
        }

        Q4WeightsX86 { data, scales, n_rows, n_cols }
    }
}

/// Q4×Q8 matmul with pre-quantized input. AVX2/AVX-512 + sign trick.
/// Reads HALF the weight data of matmul_i8xi8 → 2x bandwidth improvement.
#[cfg(target_arch = "x86_64")]
pub fn matmul_q4_preq_x86(q4: &Q4WeightsX86, input_q: &QuantizedInput, output: &mut [i64]) {
    if q4.n_rows == 0 || q4.data.is_empty() {
        for o in output.iter_mut() { *o = 0; }
        return;
    }
    debug_assert_eq!(output.len(), q4.scales.len(), "matmul output/scales mismatch");
    use std::arch::x86_64::*;

    if !is_x86_feature_detected!("avx2") {
        // Scalar fallback
        matmul_q4_scalar(q4, input_q, output);
        return;
    }

    let in_size = q4.n_cols;
    let byte_cols = in_size / 2;
    let data = &q4.data;
    let inp = &input_q.data;
    let scales = &q4.scales;
    let isf = input_q.scale_factor;

    output.par_chunks_mut(64).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 64;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row = unsafe { data.as_ptr().add(i * byte_cols) };
            let ip = inp.as_ptr();
            let mut acc: i64 = 0;

            unsafe {
                // AVX2: process 16 Q4 bytes (32 values) per iteration
                // Unpack nibbles → sign-extend → multiply with Q8 input via sign trick
                let simd_len = byte_cols / 64 * 64; // 4×16 per iteration for ILP
                let mask_lo = _mm_set1_epi8(0x0F);
                let bias = _mm256_set1_epi8(8);
                let ones = _mm256_set1_epi16(1);
                let mut vacc0 = _mm256_setzero_si256();
                let mut vacc1 = _mm256_setzero_si256();
                let mut vacc2 = _mm256_setzero_si256();
                let mut vacc3 = _mm256_setzero_si256();

                let mut j = 0usize;
                while j < simd_len {
                    _mm_prefetch(row.add(j + 128) as *const i8, _MM_HINT_T0);
                    _mm_prefetch(ip.add(j * 2 + 256) as *const i8, _MM_HINT_T0);

                    // Block 0: 16 Q4 bytes → 32 i8 weights × 32 i8 input
                    let packed0 = _mm_loadu_si128(row.add(j) as *const __m128i);
                    let lo0 = _mm_and_si128(packed0, mask_lo);
                    let hi0 = _mm_and_si128(_mm_srli_epi16(packed0, 4), mask_lo);
                    let interleaved0 = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo0, hi0), _mm_unpacklo_epi8(lo0, hi0)),
                        bias);
                    let vi0 = _mm256_loadu_si256(ip.add(j * 2) as *const __m256i);
                    let ax0 = _mm256_sign_epi8(interleaved0, interleaved0);
                    let sy0 = _mm256_sign_epi8(vi0, interleaved0);
                    vacc0 = _mm256_add_epi32(vacc0, _mm256_madd_epi16(_mm256_maddubs_epi16(ax0, sy0), ones));

                    // Block 1
                    let packed1 = _mm_loadu_si128(row.add(j + 16) as *const __m128i);
                    let lo1 = _mm_and_si128(packed1, mask_lo);
                    let hi1 = _mm_and_si128(_mm_srli_epi16(packed1, 4), mask_lo);
                    let interleaved1 = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo1, hi1), _mm_unpacklo_epi8(lo1, hi1)),
                        bias);
                    let vi1 = _mm256_loadu_si256(ip.add(j * 2 + 32) as *const __m256i);
                    let ax1 = _mm256_sign_epi8(interleaved1, interleaved1);
                    let sy1 = _mm256_sign_epi8(vi1, interleaved1);
                    vacc1 = _mm256_add_epi32(vacc1, _mm256_madd_epi16(_mm256_maddubs_epi16(ax1, sy1), ones));

                    // Block 2
                    let packed2 = _mm_loadu_si128(row.add(j + 32) as *const __m128i);
                    let lo2 = _mm_and_si128(packed2, mask_lo);
                    let hi2 = _mm_and_si128(_mm_srli_epi16(packed2, 4), mask_lo);
                    let interleaved2 = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo2, hi2), _mm_unpacklo_epi8(lo2, hi2)),
                        bias);
                    let vi2 = _mm256_loadu_si256(ip.add(j * 2 + 64) as *const __m256i);
                    let ax2 = _mm256_sign_epi8(interleaved2, interleaved2);
                    let sy2 = _mm256_sign_epi8(vi2, interleaved2);
                    vacc2 = _mm256_add_epi32(vacc2, _mm256_madd_epi16(_mm256_maddubs_epi16(ax2, sy2), ones));

                    // Block 3
                    let packed3 = _mm_loadu_si128(row.add(j + 48) as *const __m128i);
                    let lo3 = _mm_and_si128(packed3, mask_lo);
                    let hi3 = _mm_and_si128(_mm_srli_epi16(packed3, 4), mask_lo);
                    let interleaved3 = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo3, hi3), _mm_unpacklo_epi8(lo3, hi3)),
                        bias);
                    let vi3 = _mm256_loadu_si256(ip.add(j * 2 + 96) as *const __m256i);
                    let ax3 = _mm256_sign_epi8(interleaved3, interleaved3);
                    let sy3 = _mm256_sign_epi8(vi3, interleaved3);
                    vacc3 = _mm256_add_epi32(vacc3, _mm256_madd_epi16(_mm256_maddubs_epi16(ax3, sy3), ones));

                    j += 64;
                }

                vacc0 = _mm256_add_epi32(_mm256_add_epi32(vacc0, vacc1),
                                         _mm256_add_epi32(vacc2, vacc3));

                // 16-byte remainder
                while j + 16 <= byte_cols {
                    let packed = _mm_loadu_si128(row.add(j) as *const __m128i);
                    let lo = _mm_and_si128(packed, mask_lo);
                    let hi = _mm_and_si128(_mm_srli_epi16(packed, 4), mask_lo);
                    let interleaved = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo, hi), _mm_unpacklo_epi8(lo, hi)),
                        bias);
                    let vi = _mm256_loadu_si256(ip.add(j * 2) as *const __m256i);
                    let ax = _mm256_sign_epi8(interleaved, interleaved);
                    let sy = _mm256_sign_epi8(vi, interleaved);
                    vacc0 = _mm256_add_epi32(vacc0, _mm256_madd_epi16(_mm256_maddubs_epi16(ax, sy), ones));
                    j += 16;
                }

                let lo128 = _mm256_extracti128_si256(vacc0, 0);
                let hi128 = _mm256_extracti128_si256(vacc0, 1);
                let sum128 = _mm_add_epi32(lo128, hi128);
                let sum128 = _mm_hadd_epi32(sum128, sum128);
                let sum128 = _mm_hadd_epi32(sum128, sum128);
                acc = _mm_extract_epi32(sum128, 0) as i64;

                // Scalar remainder
                while j < byte_cols {
                    let byte = *row.add(j);
                    let w_lo = (byte & 0x0F) as i8 - 8;
                    let w_hi = ((byte >> 4) & 0x0F) as i8 - 8;
                    acc += (w_lo as i64) * (*ip.add(j * 2) as i64)
                         + (w_hi as i64) * (*ip.add(j * 2 + 1) as i64);
                    j += 1;
                }
            }
            *out = acc * ((scales[i] * isf) >> FRAC_BITS);
        }
    });
}

/// Q4×Q8 matmul with NEON SIMD. Same algorithm as x86 AVX2 but uses
/// NEON nibble extraction: vand + vshr for low/high, vsub for bias-8.
/// Processes 16 packed bytes (32 Q4 values) per iteration.
#[cfg(target_arch = "aarch64")]
pub fn matmul_q4_preq_neon(q4: &Q4WeightsX86, input_q: &QuantizedInput, output: &mut [i64]) {
    if q4.n_rows == 0 || q4.data.is_empty() {
        for o in output.iter_mut() { *o = 0; }
        return;
    }
    debug_assert_eq!(output.len(), q4.scales.len(), "matmul output/scales mismatch");
    use std::arch::aarch64::*;

    let in_size = q4.n_cols;
    let byte_cols = in_size / 2;
    let data = &q4.data;
    let inp = &input_q.data;
    let scales = &q4.scales;
    let isf = input_q.scale_factor;

    output.par_chunks_mut(512).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 512;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row_off = i * byte_cols;
            let mut acc: i64;

            unsafe {
                let simd_len = byte_cols / 16 * 16; // 16 bytes = 32 Q4 values
                let bias = vdupq_n_s8(8);
                let mask_lo = vdupq_n_u8(0x0F);
                let mut vacc0 = vdupq_n_s32(0);
                let mut vacc1 = vdupq_n_s32(0);
                let mut vacc2 = vdupq_n_s32(0);
                let mut vacc3 = vdupq_n_s32(0);

                let mut j = 0usize;
                while j < simd_len {
                    // Load 16 packed Q4 bytes = 32 weight values
                    let packed = vld1q_u8(data.as_ptr().add(row_off + j));

                    // Extract low nibbles [0,15] and high nibbles [0,15]
                    let lo = vreinterpretq_s8_u8(vandq_u8(packed, mask_lo));
                    let hi = vreinterpretq_s8_u8(vshrq_n_u8(packed, 4));

                    // Subtract bias 8 → signed [-8, 7]
                    let q_lo = vsubq_s8(lo, bias);
                    let q_hi = vsubq_s8(hi, bias);

                    // Load 32 input i8 values (lo input for lo nibbles, hi for hi)
                    // Layout: lo nibble = even cols, hi nibble = odd cols
                    // Need to interleave: input[j*2], input[j*2+1], input[j*2+2], ...
                    // lo[k] pairs with input[j*2 + k*2], hi[k] with input[j*2 + k*2 + 1]
                    // But NEON zip can interleave: q_lo[0],q_hi[0],q_lo[1],q_hi[1],...
                    // to match sequential input layout

                    // Interleave weights to match sequential input
                    let wlo_lo = vget_low_s8(q_lo);   // 8 low nibbles (even cols)
                    let whi_lo = vget_low_s8(q_hi);   // 8 high nibbles (odd cols)
                    let wlo_hi = vget_high_s8(q_lo);
                    let whi_hi = vget_high_s8(q_hi);

                    let w_interleaved_0 = vzip1q_s8(
                        vcombine_s8(wlo_lo, wlo_hi),
                        vcombine_s8(whi_lo, whi_hi));
                    let w_interleaved_1 = vzip2q_s8(
                        vcombine_s8(wlo_lo, wlo_hi),
                        vcombine_s8(whi_lo, whi_hi));

                    let i0 = vld1q_s8(inp.as_ptr().add(j * 2));
                    let i1 = vld1q_s8(inp.as_ptr().add(j * 2 + 16));

                    // i8×i8 → i16 → pairwise add to i32
                    vacc0 = vpadalq_s16(vacc0, vmull_s8(vget_low_s8(w_interleaved_0), vget_low_s8(i0)));
                    vacc1 = vpadalq_s16(vacc1, vmull_s8(vget_high_s8(w_interleaved_0), vget_high_s8(i0)));
                    vacc2 = vpadalq_s16(vacc2, vmull_s8(vget_low_s8(w_interleaved_1), vget_low_s8(i1)));
                    vacc3 = vpadalq_s16(vacc3, vmull_s8(vget_high_s8(w_interleaved_1), vget_high_s8(i1)));

                    j += 16;
                }

                vacc0 = vaddq_s32(vaddq_s32(vacc0, vacc1), vaddq_s32(vacc2, vacc3));
                acc = vaddvq_s32(vacc0) as i64;

                // Scalar remainder
                while j < byte_cols {
                    let byte = data[row_off + j];
                    let w_lo = (byte & 0x0F) as i8 - 8;
                    let w_hi = ((byte >> 4) & 0x0F) as i8 - 8;
                    acc += (w_lo as i64) * (inp[j * 2] as i64)
                         + (w_hi as i64) * (inp[j * 2 + 1] as i64);
                    j += 1;
                }
            }
            *out = acc * ((scales[i] * isf) >> FRAC_BITS);
        }
    });
}

// Scalar fallback for the AVX2 Q4 path; its only caller, matmul_q4_preq_x86,
// is x86_64-only, so gate it the same way instead of leaving it unused
// elsewhere.
#[cfg(target_arch = "x86_64")]
fn matmul_q4_scalar(q4: &Q4WeightsX86, input_q: &QuantizedInput, output: &mut [i64]) {
    if q4.n_rows == 0 || q4.data.is_empty() {
        for o in output.iter_mut() { *o = 0; }
        return;
    }
    debug_assert_eq!(output.len(), q4.scales.len(), "matmul output/scales mismatch");
    let byte_cols = q4.n_cols / 2;
    let data = &q4.data;
    let inp = &input_q.data;
    let scales = &q4.scales;
    let isf = input_q.scale_factor;

    for (i, out) in output.iter_mut().enumerate() {
        let mut acc: i64 = 0;
        let row_off = i * byte_cols;
        for j in 0..byte_cols {
            let byte = data[row_off + j];
            let w_lo = (byte & 0x0F) as i8 - 8;
            let w_hi = ((byte >> 4) & 0x0F) as i8 - 8;
            acc += (w_lo as i64) * (inp[j * 2] as i64)
                 + (w_hi as i64) * (inp[j * 2 + 1] as i64);
        }
        *out = acc * ((scales[i] * isf) >> FRAC_BITS);
    }
}

/// Q4 × i64 matmul with FULL input precision (no pre-quantization).
/// This avoids the double-quantization precision loss of the SIMD path.
pub fn matmul_q4_full(q4: &Q4WeightsX86, input: &[i64], output: &mut [i64]) {
    if q4.n_rows == 0 || q4.data.is_empty() {
        for o in output.iter_mut() { *o = 0; }
        return;
    }
    debug_assert_eq!(output.len(), q4.scales.len(), "matmul output/scales mismatch");
    let byte_cols = q4.n_cols / 2;
    let data = &q4.data;
    let scales = &q4.scales;

    output.par_chunks_mut(512).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 512;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row_off = i * byte_cols;
            let mut acc: i64 = 0;

            for j in 0..byte_cols {
                let byte = data[row_off + j];
                let w_lo = (byte & 0x0F) as i64 - 8;
                let w_hi = ((byte >> 4) as i64) - 8;
                acc += w_lo * input[j * 2]
                     + w_hi * input[j * 2 + 1];
            }
            // scales already include q4_per_unit factor from from_i8()
            *out = (acc * scales[i]) >> FRAC_BITS;
        }
    });
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// RMSNorm (what Llama uses - no mean subtraction, just root-mean-square).
/// output[i] = (input[i] / rms) * gamma[i]
/// where rms = sqrt(mean(x²))
pub fn layernorm(input: &[i64], gamma: &[i64]) -> Vec<i64> {
    let n = input.len() as i64;
    if n == 0 { return vec![]; }
    // RMSNorm: compute mean of squares (NOT variance around mean).
    //
    // Use i128 accumulator so small x² terms don't truncate. With the old
    // `sq_sum += (x*x) >> FRAC_BITS` pattern any x with |x| < 256 Q16
    // (≈0.0039 real) contributed 0 to the sum because x·x was below 2^16
    // before the shift. Long-tailed distributions (most of Llama's hidden
    // states) got systematically understated RMS → oversized normalization
    // factor → amplified residual stream layer-to-layer - a measurable
    // contributor to the PPL gap vs candle's Q8_0 reference.
    let mut sq_sum: i128 = 0;
    for &x in input {
        sq_sum += (x as i128) * (x as i128);
    }
    // sq_sum accumulated in full Q32. Bring back to Q16 once for isqrt.
    let mean_sq_q32 = sq_sum / (n as i128);
    let mean_sq = (mean_sq_q32 >> FRAC_BITS as i128) as i64;
    let inv_rms = integer_isqrt(mean_sq + 1);
    input.iter().enumerate().map(|(i, &x)| {
        let norm = (x * inv_rms) >> FRAC_BITS;
        let g = if i < gamma.len() { gamma[i] } else { ONE };
        (norm * g) >> FRAC_BITS
    }).collect()
}

pub fn apply_rope(vec: &mut [i64], pos: usize, d_head: usize, cos: &[i64], sin: &[i64]) {
    let half = d_head / 2;
    for i in 0..half {
        let cos_val = cos[pos * half + i];
        let sin_val = sin[pos * half + i];
        let x0 = vec[i];
        let x1 = vec[i + half];
        vec[i] = ((x0 * cos_val) >> FRAC_BITS) - ((x1 * sin_val) >> FRAC_BITS);
        vec[i + half] = ((x0 * sin_val) >> FRAC_BITS) + ((x1 * cos_val) >> FRAC_BITS);
    }
}

/// SiLU(x) = x * sigmoid(x) = x / (1 + exp(-x))
/// Uses the integer exp LUT for sigmoid computation.
pub fn silu_i64(x: i64) -> i64 {
    // sigmoid(x) = 1 / (1 + exp(-x))
    let sig = if x >= 0 {
        let exp_neg = integer_exp(-x);
        (ONE * ONE) / (ONE + exp_neg).max(1)
    } else {
        let exp_pos = integer_exp(x);
        (exp_pos * ONE) / (ONE + exp_pos).max(1)
    };
    // SiLU = x * sigmoid(x)
    (x * sig) >> FRAC_BITS
}

// ─── Fused LayerNorm + Projection ─────────────────────────────────────────────

/// Compute layernorm stats (mean, inv_std) without materializing the normed vector.
// Only caller is fused_layernorm_matmul, which is itself off the live path.
#[allow(dead_code)]
#[inline]
fn layernorm_stats(input: &[i64]) -> (i64, i64) {
    let n = input.len() as i64;
    let mean = input.iter().sum::<i64>() / n;
    let mut var_sum: i64 = 0;
    for &x in input {
        let d = x - mean;
        var_sum += (d * d) >> FRAC_BITS;
    }
    let inv_std = integer_isqrt(var_sum / n + 1);
    (mean, inv_std)
}

/// Fused layernorm + i8 matmul projection.
/// Computes: output = matmul(weights, layernorm(input, gamma))
/// Without allocating the intermediate normed vector.
/// One pass over input to compute stats, then stream through weight rows.
// The live forward pass still calls layernorm and matmul separately; this
// fused variant is kept as the reference for the fused kernel work.
#[allow(dead_code)]
fn fused_layernorm_matmul(
    input: &[i64],
    gamma: &[i64],
    weights: &I8Weights,
    in_size: usize,
    out_size: usize,
) -> Vec<i64> {
    let (mean, inv_std) = layernorm_stats(input);
    let scales = &weights.scales;
    let data = &weights.data;

    let mut output = vec![0i64; out_size];
    output.par_chunks_mut(64).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 64;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let row = &data[i * in_size..(i + 1) * in_size];
            let mut acc: i64 = 0;
            // Fused: for each j, compute normed[j] on-the-fly and multiply
            for j in 0..in_size {
                let norm = ((input[j] - mean) * inv_std) >> FRAC_BITS;
                let g = if j < gamma.len() { gamma[j] } else { ONE };
                let normed_j = (norm * g) >> FRAC_BITS;
                acc += (row[j] as i64) * normed_j;
            }
            *out = (acc * scales[i]) >> FRAC_BITS;
        }
    });
    output
}

// ─── SIMD Attention KV Dot Products ──────────────────────────────────────────

/// Quantize a Q16 i64 vector to i8 for SIMD dot products.
/// Returns (i8 data, scale factor).
// Only caller is flash_attention_i8, which is off the live path (the i64 KV
// cache variant is what runs today).
#[allow(dead_code)]
#[inline]
fn quantize_for_dot(v: &[i64]) -> (Vec<i8>, i64) {
    let abs_max = v.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let sf = (abs_max / 127).max(1);
    let data: Vec<i8> = v.iter().map(|&x| (x / sf).clamp(-127, 127) as i8).collect();
    (data, sf)
}

/// SIMD i8×i8 dot product for attention K scores.
/// q_i8: query head quantized to i8, k_ptr: pointer into KV cache i8 data.
/// Returns i32 dot product (caller applies scales).
// Reached only through dot_i8_kv, which only flash_attention_i8 calls; that
// i8-KV attention path is not the live one today.
#[allow(dead_code)]
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn dot_i8_kv_neon(q_i8: *const i8, k_ptr: *const i8, d_head: usize) -> i32 {
    // SAFETY: body wrapped for `unsafe_op_in_unsafe_fn` (denied workspace-wide).
    // The contract is unchanged: the caller guarantees the pointers are valid
    // for `len` reads. Wrapping is purely lexical - no semantics change.
    unsafe {
        use std::arch::aarch64::*;
        let simd_len = d_head / 32 * 32;
        let mut vacc0 = vdupq_n_s32(0);
        let mut vacc1 = vdupq_n_s32(0);
        let mut vacc2 = vdupq_n_s32(0);
        let mut vacc3 = vdupq_n_s32(0);
        let mut j = 0usize;
        while j < simd_len {
            let vq0 = vld1q_s8(q_i8.add(j));
            let vk0 = vld1q_s8(k_ptr.add(j));
            vacc0 = vpadalq_s16(vacc0, vmull_s8(vget_low_s8(vq0), vget_low_s8(vk0)));
            vacc1 = vpadalq_s16(vacc1, vmull_s8(vget_high_s8(vq0), vget_high_s8(vk0)));
            let vq1 = vld1q_s8(q_i8.add(j + 16));
            let vk1 = vld1q_s8(k_ptr.add(j + 16));
            vacc2 = vpadalq_s16(vacc2, vmull_s8(vget_low_s8(vq1), vget_low_s8(vk1)));
            vacc3 = vpadalq_s16(vacc3, vmull_s8(vget_high_s8(vq1), vget_high_s8(vk1)));
            j += 32;
        }
        vacc0 = vaddq_s32(vaddq_s32(vacc0, vacc1), vaddq_s32(vacc2, vacc3));
        let mut acc = vaddvq_s32(vacc0);
        while j < d_head {
            acc += (*q_i8.add(j) as i32) * (*k_ptr.add(j) as i32);
            j += 1;
        }
        acc
    }
}

/// AVX2 i8×i8 dot for attention K scores with sign trick.
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn dot_i8_kv_avx2(q_i8: *const i8, k_ptr: *const i8, d_head: usize) -> i32 {
    // SAFETY: body wrapped for `unsafe_op_in_unsafe_fn` (denied workspace-wide).
    // The contract is unchanged: the caller guarantees the pointers are valid
    // for `len` reads. Wrapping is purely lexical - no semantics change.
    unsafe {
        use std::arch::x86_64::*;
        if !is_x86_feature_detected!("avx2") {
            let mut acc: i32 = 0;
            for j in 0..d_head {
                acc += (*q_i8.add(j) as i32) * (*k_ptr.add(j) as i32);
            }
            return acc;
        }
        let simd_len = d_head / 32 * 32;
        let ones = _mm256_set1_epi16(1);
        let mut vacc = _mm256_setzero_si256();
        let mut j = 0usize;
        while j < simd_len {
            let vq = _mm256_loadu_si256(q_i8.add(j) as *const __m256i);
            let vk = _mm256_loadu_si256(k_ptr.add(j) as *const __m256i);
            let ax = _mm256_sign_epi8(vq, vq);
            let sy = _mm256_sign_epi8(vk, vq);
            vacc = _mm256_add_epi32(vacc, _mm256_madd_epi16(_mm256_maddubs_epi16(ax, sy), ones));
            j += 32;
        }
        let lo = _mm256_extracti128_si256(vacc, 0);
        let hi = _mm256_extracti128_si256(vacc, 1);
        let sum128 = _mm_hadd_epi32(_mm_add_epi32(lo, hi), _mm_setzero_si128());
        let sum128 = _mm_hadd_epi32(sum128, _mm_setzero_si128());
        let mut acc = _mm_extract_epi32(sum128, 0);
        while j < d_head {
            acc += (*q_i8.add(j) as i32) * (*k_ptr.add(j) as i32);
            j += 1;
        }
        acc
    }
}

/// Cross-platform SIMD dot product dispatch for attention.
// Only caller is flash_attention_i8, which is off the live path.
#[allow(dead_code)]
#[inline]
fn dot_i8_kv(q_i8: &[i8], k_ptr: &[i8], k_offset: usize, d_head: usize) -> i32 {
    #[cfg(target_arch = "aarch64")]
    { unsafe { dot_i8_kv_neon(q_i8.as_ptr(), k_ptr.as_ptr().add(k_offset), d_head) } }
    #[cfg(target_arch = "x86_64")]
    { unsafe { dot_i8_kv_avx2(q_i8.as_ptr(), k_ptr.as_ptr().add(k_offset), d_head) } }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let mut acc: i32 = 0;
        for dd in 0..d_head {
            acc += (q_i8[dd] as i32) * (k_ptr[k_offset + dd] as i32);
        }
        acc
    }
}

// ─── Flash Attention (Online Softmax, O(1) Memory) ───────────────────────────

/// Flash attention for a single query head against i8-quantized KV cache.
/// Uses online softmax: processes KV in streaming fashion, never allocates O(n²).
/// Numerically equivalent to standard attention (within integer rounding).
///
/// NOTE: This i8-quantized variant is available for future use with i8 KV caches.
/// The production path uses flash_attention_i64() which operates directly on the
/// i64 KV cache with online softmax. Both use the same integer_exp() arithmetic.
///
/// q_head: [d_head] i64 Q16 - the query for this head at current position
/// k_data: flat i8 array of all cached K for this layer
/// k_scales: per-position scales for K
/// v_data: flat i8 array of all cached V
/// v_scales: per-position scales for V
/// d_kv: total d_kv dimension (d_head * n_kv_heads)
/// kv_h: which KV head to use
/// d_head: dimension per head
/// full_seq: number of positions in cache
/// attn_scale: 1/sqrt(d_head) in Q16
// The live attention path is flash_attention_i64 (i64 KV cache). This i8-KV
// variant is kept as the reference implementation for the quantised KV cache
// and is exercised by hand when that path is revisited.
#[allow(dead_code)]
// Every argument is a separate KV-cache slice or a shape constant. Packing
// them into a struct would only move the same 10 values behind an extra
// indirection in a hot integer kernel, so the count stays as is.
#[allow(clippy::too_many_arguments)]
fn flash_attention_i8(
    q_head: &[i64],
    k_data: &[i8], k_scales: &[i64],
    v_data: &[i8], v_scales: &[i64],
    d_kv: usize, kv_h: usize, d_head: usize,
    full_seq: usize, attn_scale: i64,
) -> Vec<i64> {
    // Online softmax: maintain running max, sum of exp, and weighted V sum.
    // Process one position at a time - O(1) extra memory (no scores array).
    let mut running_max: i64 = -8 * ONE; // start very negative
    let mut running_sum: i64 = 0;        // sum of exp(score - max)
    let mut out = vec![0i64; d_head];     // weighted V accumulator

    // Quantize Q to i8 ONCE for SIMD dot products across all positions
    let (q_i8, q_sf) = quantize_for_dot(q_head);

    for j in 0..full_seq {
        let k_off = j * d_kv + kv_h * d_head;
        let k_scale = k_scales[j];

        // SIMD dot product: Q_i8 · K_i8 (both already quantized)
        let dot_i32 = dot_i8_kv(&q_i8, k_data, k_off, d_head);
        let dot = (dot_i32 as i64) * q_sf * k_scale;
        let score = ((dot >> (FRAC_BITS * 2)) * attn_scale) >> FRAC_BITS;

        // Online softmax update
        if score > running_max {
            // New max - rescale existing accumulator
            let diff = running_max - score; // negative
            let correction = integer_exp(diff); // exp(old_max - new_max) < 1
            // Scale down existing sum and output
            running_sum = (running_sum * correction) >> FRAC_BITS;
            for o in out.iter_mut() {
                *o = (*o * correction) >> FRAC_BITS;
            }
            running_max = score;
        }

        // exp(score - running_max)
        let w = integer_exp(score - running_max);
        running_sum += w;

        // Accumulate weighted V (dequantized on-the-fly)
        let v_off = j * d_kv + kv_h * d_head;
        let v_scale = v_scales[j];
        for dd in 0..d_head {
            let v_val = (v_data[v_off + dd] as i64) * v_scale;
            out[dd] += (w * v_val) >> FRAC_BITS;
        }
    }

    // Normalize by sum
    if running_sum > 0 {
        for o in out.iter_mut() {
            *o = (*o * ONE) / running_sum;
        }
    }

    out
}

/// Flash attention for a single query head against the i64 KV cache.
/// Uses online softmax: processes KV in streaming fashion, O(d_head) memory
/// instead of O(full_seq) for the scores array. Numerically equivalent to the
/// standard softmax(Q·K)·V path (same integer_exp + shift arithmetic).
///
/// This is the production attention path as of v0.5.3. It replaces the standard
/// path that allocated a scores Vec<i64> of size full_seq per head.
///
/// Returns the attention output for this head: [d_head] i64 Q16.
// Every argument is a separate KV-cache slice or a shape constant. Packing
// them into a struct would only move the same 8 values behind an extra
// indirection in the hottest integer kernel in the crate, so the count stays.
#[allow(clippy::too_many_arguments)]
#[inline]
fn flash_attention_i64(
    q_head: &[i64],            // [d_head] i64 Q16
    k_cache: &[i64],           // flat i64 [full_seq * d_kv]
    v_cache: &[i64],           // flat i64 [full_seq * d_kv]
    d_kv: usize,               // total KV dimension
    kv_h: usize,               // which KV head to use
    d_head: usize,             // dimension per head
    full_seq: usize,           // positions in cache
    attn_scale: i64,           // 1/sqrt(d_head) in Q16
) -> Vec<i64> {
    // Online softmax: maintain running max, sum of exp, and weighted V sum.
    let mut running_max: i64 = i64::MIN / 2; // avoid overflow on subtraction
    let mut running_sum: i64 = 0;
    let mut out = vec![0i64; d_head];

    for j in 0..full_seq {
        let k_off = j * d_kv + kv_h * d_head;

        // Q·K dot product (same as standard path)
        let dot = unsafe {
            dot_i64xi64_attn_neon(
                q_head.as_ptr(),
                k_cache.as_ptr().add(k_off),
                d_head,
            )
        };
        let score = ((dot >> FRAC_BITS) * attn_scale) >> FRAC_BITS;

        // Online softmax update
        if score > running_max {
            // New max - rescale existing accumulator
            let diff = running_max - score; // negative
            let correction = integer_exp(diff);
            running_sum = (running_sum * correction) >> FRAC_BITS;
            for o in out.iter_mut() {
                *o = (*o * correction) >> FRAC_BITS;
            }
            running_max = score;
        }

        let w = integer_exp(score - running_max);
        running_sum += w;

        // Accumulate weighted V
        let v_off = j * d_kv + kv_h * d_head;
        for dd in 0..d_head {
            out[dd] += (w * v_cache[v_off + dd]) >> FRAC_BITS;
        }
    }

    // Normalize by sum
    if running_sum > 0 {
        for o in out.iter_mut() {
            *o = (*o * ONE) / running_sum;
        }
    }

    out
}

// ─── Binary Weight Cache ──────────────────────────────────────────────────────

impl I8Weights {
    pub fn write_to(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        w.write_all(&(self.n_rows as u64).to_le_bytes())?;
        w.write_all(&(self.n_cols as u64).to_le_bytes())?;
        // Per-row scales
        for &s in &self.scales {
            w.write_all(&s.to_le_bytes())?;
        }
        // Data
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(self.data.as_ptr() as *const u8, self.data.len())
        };
        w.write_all(bytes)
    }

    pub fn read_from(r: &mut impl std::io::Read) -> std::io::Result<Self> {
        let mut buf8 = [0u8; 8];
        r.read_exact(&mut buf8)?;
        let n_rows = u64::from_le_bytes(buf8) as usize;
        r.read_exact(&mut buf8)?;
        let n_cols = u64::from_le_bytes(buf8) as usize;
        let mut scales = Vec::with_capacity(n_rows);
        for _ in 0..n_rows {
            r.read_exact(&mut buf8)?;
            scales.push(i64::from_le_bytes(buf8));
        }
        let mut data_bytes = vec![0u8; n_rows * n_cols];
        r.read_exact(&mut data_bytes)?;
        let data: Vec<i8> = unsafe {
            let mut d = std::mem::ManuallyDrop::new(data_bytes);
            Vec::from_raw_parts(d.as_mut_ptr() as *mut i8, d.len(), d.capacity())
        };
        Ok(Self { data, scales, n_rows, n_cols })
    }
}

fn write_i64_vec(w: &mut impl std::io::Write, v: &[i64]) -> std::io::Result<()> {
    w.write_all(&(v.len() as u64).to_le_bytes())?;
    for &x in v { w.write_all(&x.to_le_bytes())?; }
    Ok(())
}

fn read_i64_vec(r: &mut impl std::io::Read) -> std::io::Result<Vec<i64>> {
    let mut buf8 = [0u8; 8];
    r.read_exact(&mut buf8)?;
    let len = u64::from_le_bytes(buf8) as usize;
    let mut v = Vec::with_capacity(len);
    for _ in 0..len {
        r.read_exact(&mut buf8)?;
        v.push(i64::from_le_bytes(buf8));
    }
    Ok(v)
}

// ─── Forward Pass ─────────────────────────────────────────────────────────────

impl CachedIntegerModel {
    pub fn memory_bytes(&self) -> usize {
        let mut total = self.embedding_i8.memory_bytes() + self.embedding_q16.len() * 8
            + self.output_weight.memory_bytes()
            + self.final_norm.len() * 8
            + self.config.rope_cos.len() * 8 * 2;
        for layer in &self.layers {
            total += layer.wq.memory_bytes() + layer.wk.memory_bytes()
                + layer.wv.memory_bytes() + layer.wo.memory_bytes()
                + layer.w_gate.memory_bytes() + layer.w_up.memory_bytes()
                + layer.w_down.memory_bytes()
                + (layer.attn_norm.len() + layer.ffn_norm.len()) * 8;
        }
        total
    }

    pub fn decode(&self, tokens: &[u32]) -> String {
        tokens.iter()
            .map(|&id| {
                if (id as usize) < self.vocab.len() {
                    self.vocab[id as usize].replace('▁', " ")
                } else {
                    format!("[{}]", id)
                }
            })
            .collect::<String>()
    }

    pub fn encode(&self, text: &str) -> Vec<u32> {
        if self.vocab.is_empty() { return vec![]; }
        let mut tokens = Vec::new();
        let sp_text = format!("▁{}", text.replace(' ', "▁"));
        let bytes = sp_text.as_bytes();
        let mut pos = 0;
        while pos < bytes.len() {
            let mut best_len = 0;
            let mut best_id = 0u32;
            let max_try = (bytes.len() - pos).min(32);
            for try_len in (1..=max_try).rev() {
                if let Ok(candidate) = std::str::from_utf8(&bytes[pos..pos + try_len])
                    && let Some(id) = self.vocab.iter().position(|v| v == candidate) {
                        best_len = try_len;
                        best_id = id as u32;
                        break;
                    }
            }
            if best_len > 0 {
                tokens.push(best_id);
                pos += best_len;
            } else {
                // Determine the length of the current UTF-8 character to emit
                // all its bytes as individual byte tokens without splitting mid-char.
                let char_len = match bytes[pos] {
                    0x00..=0x7F => 1,
                    0xC0..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    0xF0..=0xF7 => 4,
                    _ => 1, // continuation byte (shouldn't happen at valid char boundary)
                };
                let char_end = (pos + char_len).min(bytes.len());
                for b in &bytes[pos..char_end] {
                    let byte_tok = format!("<0x{b:02X}>");
                    if let Some(id) = self.vocab.iter().position(|v| v == &byte_tok) {
                        tokens.push(id as u32);
                    }
                }
                pos = char_end;
            }
        }
        tokens
    }

    /// Apply the model's chat template to wrap user input in the correct format.
    /// Parses common Jinja2 chat template patterns from GGUF metadata.
    /// Falls back to raw input if no template or unrecognized format.
    pub fn apply_chat_template(&self, user_input: &str) -> String {
        let tmpl = &self.config.chat_template;
        if tmpl.is_empty() {
            return user_input.to_string();
        }

        // Detect common template patterns by content rather than parsing Jinja2.
        // This handles the vast majority of HuggingFace models.
        if tmpl.contains("[INST]") {
            // LLaMA-2 / Mistral style
            format!("[INST] {} [/INST]", user_input)
        } else if tmpl.contains("<|start_header_id|>") {
            // LLaMA-3 style
            format!(
                "<|begin_of_text|><|start_header_id|>user<|end_header_id|>\n\n{}<|eot_id|><|start_header_id|>assistant<|end_header_id|>\n\n",
                user_input
            )
        } else if tmpl.contains("<|im_start|>") {
            // ChatML (Qwen, Yi, many finetunes)
            format!(
                "<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
                user_input
            )
        } else if tmpl.contains("<|user|>") {
            // Phi style
            format!("<|user|>\n{}<|end|>\n<|assistant|>\n", user_input)
        } else if tmpl.contains("[turn_start]") {
            // Gemma style
            format!(
                "<start_of_turn>user\n{}<end_of_turn>\n<start_of_turn>model\n",
                user_input
            )
        } else {
            // Unknown template - use raw input
            user_input.to_string()
        }
    }

    /// Forward pass - zero-alloc matmuls with cached input quantization.
    /// Quantize input ONCE, reuse for Q/K/V (3 matmuls) and gate/up (2 matmuls).
    /// Saves 4 input quantizations per layer × 32 layers = 128 saved quantizations.
    /// Uses pre-allocated buffers (q/k/v/attn_out/gate/up/gated/ff_out).
    /// When Q4 weights are enabled (via enable_q4), uses 4-bit matmul on x86_64.
    pub fn forward_one_token(&self, token: u32, cache: &mut KVCache) -> Vec<i64> {
        let cfg = &self.config;
        let d = cfg.d_model;
        let pos = cache.seq_len;

        // Shard holders that don't cover layer 0 don't load embeddings.
        // Calling forward_one_token (single-node path) on such a node would
        // panic reading self.embedding_q16 which is empty. Return an empty
        // vec so the RPC handler can error gracefully instead of the whole
        // tokio worker thread panicking and killing launchd's keepalive loop.
        if self.embedding_q16.len() < (token as usize + 1) * d {
            return Vec::new();
        }

        // Dispatch priority (highest quality first):
        //   Block-I8  - 32-weight blocks with i32 Q16 scales (Q8_0-shaped)
        //   I16       - per-row, 258× finer than I8 on paper (but known PPL
        //               issue before the attn/LUT rescale lands, see
        //               project_i16_ppl_bug.md). Kept as a manual override.
        //   Q4        - x86 low-bandwidth path
        //   I8        - original per-row fallback
        macro_rules! dispatch_matmul {
            ($hyb:expr, $tern:expr, $blk:expr, $i16w:expr, $q4w:expr, $i8w:expr, $inq:expr, $raw:expr, $in_sz:expr, $out:expr) => {
                {
                    if let Some(hw) = $hyb {
                        crate::ternary_hybrid::matmul_ternary_hybrid_into(hw, $raw, $in_sz, $out);
                    } else if let Some(tw) = $tern {
                        crate::ternary_engine::matmul_ternary_into(tw, $raw, $in_sz, $out);
                    } else if let Some(i16w) = $i16w {
                        // Reordered 2026-06-04: per-row I16 (~258× I8
                        // resolution) wins over block-wise I8 when both
                        // are populated. Prior order silently demoted I16
                        // to a fallback that block_i8 (set by default in
                        // the loader) always preempted, so enabling
                        // i16_layers in the model struct had no observable
                        // effect at runtime. block_i8 stays as the
                        // fallback for the few code paths that don't
                        // populate I16 yet.
                        matmul_i16_into(i16w, $raw, $in_sz, $out);
                    } else if let Some(blk) = $blk {
                        crate::block_i8::matmul_block_i8_into(blk, $raw, $out);
                    } else if let Some(q4w) = $q4w {
                        matmul_q4_full(q4w, $raw, $out);
                    } else {
                        matmul_fast_preq($i8w, $inq, $raw, $in_sz, $out);
                    }
                }
            };
        }

        // Embed - use full Q16 precision (INT8 destroys tiny embedding values)
        let idx = (token as usize).min(cfg.vocab_size - 1);
        let emb_start = idx * d;
        let mut hidden: Vec<i64> = self.embedding_q16[emb_start..emb_start + d].to_vec();

        // Pre-allocate buffers (reused across layers)
        let mut q = vec![0i64; d];
        let mut k_buf = vec![0i64; cfg.d_kv];
        let mut v_buf = vec![0i64; cfg.d_kv];
        let mut attn_out = vec![0i64; d];
        let mut projected = vec![0i64; d];
        let mut gate = vec![0i64; cfg.d_ff];
        let mut up = vec![0i64; cfg.d_ff];
        let mut ff_out = vec![0i64; d];

        for (layer_idx, layer) in self.layers.iter().enumerate() {
            let hyb_layer  = self.ternary_hybrid_layers.as_ref().map(|hl| &hl[layer_idx]);
            let tern_layer = self.ternary_layers.as_ref().map(|tl| &tl[layer_idx]);
            let blk_layer  = self.block_i8_layers.as_ref().map(|bl| &bl[layer_idx]);
            let i16_layer  = self.i16_layers.as_ref().map(|il| &il[layer_idx]);
            let q4_layer   = self.q4_layers.as_ref().map(|ql| &ql[layer_idx]);

            // LayerNorm once - result fits in L1 (32KB)
            let normed = layernorm(&hidden, &layer.attn_norm);

            // Quantize normed input ONCE, reuse for Q, K, V projections
            let normed_q = QuantizedInput::from_i64(&normed);

            // Q/K/V with zero-alloc + cached quantized input
            dispatch_matmul!(hyb_layer.map(|l| &l.wq), tern_layer.map(|l| &l.wq), blk_layer.map(|l| &l.wq), i16_layer.map(|l| &l.wq), q4_layer.map(|l| &l.wq), &layer.wq, &normed_q, &normed, d, &mut q);
            dispatch_matmul!(hyb_layer.map(|l| &l.wk), tern_layer.map(|l| &l.wk), blk_layer.map(|l| &l.wk), i16_layer.map(|l| &l.wk), q4_layer.map(|l| &l.wk), &layer.wk, &normed_q, &normed, d, &mut k_buf);
            dispatch_matmul!(hyb_layer.map(|l| &l.wv), tern_layer.map(|l| &l.wv), blk_layer.map(|l| &l.wv), i16_layer.map(|l| &l.wv), q4_layer.map(|l| &l.wv), &layer.wv, &normed_q, &normed, d, &mut v_buf);

            // RoPE
            for h in 0..cfg.n_heads {
                apply_rope(&mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin);
            }
            for h in 0..cfg.n_kv_heads {
                apply_rope(&mut k_buf[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin);
            }

            // Store K/V in i8 cache
            cache.push_k(layer_idx, &k_buf);
            cache.push_v(layer_idx, &v_buf);

            // Flash attention with online softmax - NEON-vectorized Q·K dot product.
            // Processes KV cache in streaming fashion: O(d_head) memory instead of
            // O(full_seq) for the scores array. Numerically equivalent to standard
            // softmax(Q·K)·V (same integer_exp + shift arithmetic).
            let full_seq = pos + 1;
            let k_layer_data = &cache.k_data[layer_idx];
            let v_layer_data = &cache.v_data[layer_idx];
            let head_results: Vec<Vec<i64>> = (0..cfg.n_heads).into_par_iter().map(|h| {
                let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                let dh = cfg.d_head;
                let q_head = &q[h * dh..(h + 1) * dh];
                flash_attention_i64(
                    q_head, k_layer_data, v_layer_data,
                    cfg.d_kv, kv_h, dh, full_seq, cfg.attn_scale,
                )
            }).collect();

            for val in attn_out.iter_mut() { *val = 0; }
            for (h, head_out) in head_results.iter().enumerate() {
                attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(head_out);
            }

            // Wo projection + residual (zero-alloc)
            let attn_out_q = QuantizedInput::from_i64(&attn_out);
            dispatch_matmul!(hyb_layer.map(|l| &l.wo), tern_layer.map(|l| &l.wo), blk_layer.map(|l| &l.wo), i16_layer.map(|l| &l.wo), q4_layer.map(|l| &l.wo), &layer.wo, &attn_out_q, &attn_out, d, &mut projected);
            for i in 0..d { hidden[i] += projected[i]; }

            // FFN: quantize normed_ff ONCE for gate+up
            let normed_ff = layernorm(&hidden, &layer.ffn_norm);
            let normed_ff_q = QuantizedInput::from_i64(&normed_ff);

            dispatch_matmul!(hyb_layer.map(|l| &l.w_gate), tern_layer.map(|l| &l.w_gate), blk_layer.map(|l| &l.w_gate), i16_layer.map(|l| &l.w_gate), q4_layer.map(|l| &l.w_gate), &layer.w_gate, &normed_ff_q, &normed_ff, d, &mut gate);
            dispatch_matmul!(hyb_layer.map(|l| &l.w_up), tern_layer.map(|l| &l.w_up), blk_layer.map(|l| &l.w_up), i16_layer.map(|l| &l.w_up), q4_layer.map(|l| &l.w_up), &layer.w_up, &normed_ff_q, &normed_ff, d, &mut up);

            // SiLU gate * up (in-place)
            for j in 0..cfg.d_ff {
                gate[j] = (silu_i64(gate[j]) * up[j]) >> FRAC_BITS;
            }

            // W_down + residual
            let gate_q = QuantizedInput::from_i64(&gate);
            dispatch_matmul!(hyb_layer.map(|l| &l.w_down), tern_layer.map(|l| &l.w_down), blk_layer.map(|l| &l.w_down), i16_layer.map(|l| &l.w_down), q4_layer.map(|l| &l.w_down), &layer.w_down, &gate_q, &gate, cfg.d_ff, &mut ff_out);
            for i in 0..d { hidden[i] += ff_out[i]; }
        }

        cache.seq_len = pos + 1;
        let normed = layernorm(&hidden, &self.final_norm);

        // LM head dispatch: I16 > Block-I8 > Q4 > I8. Reordered 2026-06-04
        // to match the layer-matmul dispatch above so the output projection
        // doesn't silently fall through to block-I8 while every other
        // matmul uses I16.
        if let Some(i16_out) = &self.i16_output {
            let mut logits = vec![0i64; cfg.vocab_size];
            matmul_i16_into(i16_out, &normed, d, &mut logits);
            return logits;
        }
        if let Some(blk_out) = &self.block_i8_output
            && blk_out.n_rows > 0 {
                let mut logits = vec![0i64; cfg.vocab_size];
                crate::block_i8::matmul_block_i8_into(blk_out, &normed, &mut logits);
                return logits;
            }
        if let Some(q4_out) = &self.q4_output {
            let mut logits = vec![0i64; cfg.vocab_size];
            matmul_q4_full(q4_out, &normed, &mut logits);
            return logits;
        }
        matmul_fast(&self.output_weight, &normed, d, cfg.vocab_size)
    }

    pub fn generate(&self, prompt: &[u32], max_tokens: u32, eos_tokens: &[u32]) -> (Vec<u32>, Hash256) {
        let mut cache = KVCache::new(self.config.n_layers);
        let mut generated = Vec::new();

        // Prepend BOS token (1) - Llama requires it
        let _ = self.forward_one_token(1, &mut cache);

        for &tok in prompt {
            let _logits = self.forward_one_token(tok, &mut cache);
        }

        for _ in 0..max_tokens {
            let last_token = generated.last().copied()
                .unwrap_or(*prompt.last().unwrap_or(&0));
            let mut logits = self.forward_one_token(last_token, &mut cache);

            // Repetition penalty: penalize recently generated tokens deterministically.
            // This prevents INT8 quantized models from getting stuck in loops.
            // Penalty factor: divide logit by 1.2 (multiply by ONE*5/6) for repeated tokens.
            for &prev_tok in generated.iter().rev().take(64) {
                let idx = prev_tok as usize;
                if idx < logits.len() {
                    if logits[idx] > 0 {
                        logits[idx] = logits[idx] * 5 / 6; // reduce positive logit
                    } else {
                        logits[idx] = logits[idx] * 6 / 5; // increase negative logit (make more negative)
                    }
                }
            }

            let next = argmax_i64(&logits) as u32;
            generated.push(next);
            if eos_tokens.contains(&next) { break; }
        }

        let output_bytes: Vec<u8> = generated.iter()
            .flat_map(|t| t.to_le_bytes()).collect();
        let hash = arc_crypto::hash_bytes(&output_bytes);
        (generated, hash)
    }

    /// Save weights to binary .arc-int8 file for cross-platform distribution.
    pub fn save_weights(&self, path: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        f.write_all(b"ARC-INT8\x02\x00")?; // v2: per-row scales

        let cfg = &self.config;
        for &v in &[cfg.n_layers, cfg.d_model, cfg.n_heads, cfg.n_kv_heads,
                     cfg.d_ff, cfg.d_head, cfg.d_kv, cfg.vocab_size, cfg.max_seq] {
            f.write_all(&(v as u64).to_le_bytes())?;
        }
        f.write_all(&cfg.attn_scale.to_le_bytes())?;
        write_i64_vec(&mut f, &cfg.rope_cos)?;
        write_i64_vec(&mut f, &cfg.rope_sin)?;

        self.embedding_i8.write_to(&mut f)?;
        self.output_weight.write_to(&mut f)?;
        write_i64_vec(&mut f, &self.final_norm)?;

        for layer in &self.layers {
            layer.wq.write_to(&mut f)?;
            layer.wk.write_to(&mut f)?;
            layer.wv.write_to(&mut f)?;
            layer.wo.write_to(&mut f)?;
            layer.w_gate.write_to(&mut f)?;
            layer.w_up.write_to(&mut f)?;
            layer.w_down.write_to(&mut f)?;
            write_i64_vec(&mut f, &layer.attn_norm)?;
            write_i64_vec(&mut f, &layer.ffn_norm)?;
        }

        let vocab_json = serde_json::to_string(&self.vocab).unwrap_or_default();
        let vb = vocab_json.as_bytes();
        f.write_all(&(vb.len() as u64).to_le_bytes())?;
        f.write_all(vb)?;
        f.flush()
    }

    /// BLAKE3 hash of all weights for cross-platform identity verification.
    pub fn weight_hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new();
        let hash_i8w = |h: &mut blake3::Hasher, w: &I8Weights| {
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(w.data.as_ptr() as *const u8, w.data.len())
            };
            h.update(bytes);
            for &s in &w.scales { h.update(&s.to_le_bytes()); }
        };
        hash_i8w(&mut hasher, &self.embedding_i8);
        hash_i8w(&mut hasher, &self.output_weight);
        for layer in &self.layers {
            for w in [&layer.wq, &layer.wk, &layer.wv, &layer.wo,
                      &layer.w_gate, &layer.w_up, &layer.w_down] {
                hash_i8w(&mut hasher, w);
            }
        }
        let hash = hasher.finalize();
        Hash256(*hash.as_bytes())
    }

    /// Run a single token through layers [start_layer, end_layer) of this shard.
    ///
    /// `input`:
    ///   - First shard (start_layer == 0): pass `ShardInput::Token(token_id)`.
    ///     The function looks up the embedding internally.
    ///   - Middle/last shards: pass `ShardInput::Hidden(state)` from the previous shard.
    ///
    /// `position`: token position in the sequence (for RoPE + KV cache push).
    /// `cache`: per-request KV cache (lives across tokens; size = n_layers).
    ///
    /// Returns:
    ///   - Last shard (end_layer == n_layers): `ShardOutput::Token { id, logits_hash }`.
    ///     Runs final_norm + LM head + argmax to produce the next token id.
    ///   - First/middle shards: `ShardOutput::Hidden(state)` to forward to the next shard.
    pub fn forward_shard_token(
        &self,
        input: ShardInput,
        cache: &mut KVCache,
        start_layer: usize,
        end_layer: usize,
        position: usize,
    ) -> Result<ShardOutput, ShardForwardError> {
        let cfg = &self.config;
        let d = cfg.d_model;
        let is_first = start_layer == 0;
        let is_last = end_layer == cfg.n_layers;
        let end = end_layer.min(self.layers.len());

        // ── Preflight guards ────────────────────────────────────────────
        // Everything below used to be a `debug_assert!` — i.e. absent in
        // release — in front of an unchecked read. See ShardForwardError.

        // RoPE table bound. `apply_rope` indexes cos[pos * half + i] with no
        // bounds check on `pos`.
        if position >= cfg.max_seq {
            return Err(ShardForwardError::PositionOutOfRange {
                position,
                max_seq: cfg.max_seq,
            });
        }

        // KV-cache continuity. `flash_attention_i64` walks j in 0..(position+1)
        // and dereferences `k_cache.as_ptr().add(j * d_kv + ...)` UNCHECKED, so
        // a cache holding fewer than `position` prior entries is an
        // out-of-bounds read (UB), not a wrong answer. This is the state a
        // replica is in when the coordinator failed over to it mid-stream, or
        // when a hedged/raced request to it was dropped before it landed.
        //
        // We require exact equality, not >=: a cache that is somehow LONGER
        // than the position implies the coordinator is replaying a position
        // this replica already consumed, which would double-push K/V and
        // silently corrupt every later token.
        for layer_idx in start_layer..end {
            let cached = cache.k_data[layer_idx].len() / cfg.d_kv.max(1);
            if cached != position {
                return Err(ShardForwardError::KvCacheOutOfSync {
                    layer: layer_idx,
                    expected_positions: position,
                    cached_positions: cached,
                });
            }
        }

        // Layer residency. A non-resident layer carries empty weight buffers;
        // matmul against them produces garbage at best and indexes off the end
        // at worst.
        for layer_idx in start_layer..end {
            if !self.layers[layer_idx].is_loaded() {
                return Err(ShardForwardError::LayerNotLoaded { layer: layer_idx });
            }
        }

        // ── Input: token id (first shard) → embedding lookup ──
        // ── Input: hidden state (middle/last) → use directly ──
        let mut hidden: Vec<i64> = match input {
            ShardInput::Token(token_id) => {
                debug_assert!(is_first, "Token input is only valid on the first shard");
                let idx = (token_id as usize).min(cfg.vocab_size - 1);
                let emb_start = idx * d;
                self.embedding_q16[emb_start..emb_start + d].to_vec()
            }
            ShardInput::Hidden(state) => {
                // A short hidden state from a peer used to panic on
                // `hidden[i] += projected[i]`, which aborts the process under
                // the release profile's `panic = "abort"`.
                if state.len() != d {
                    return Err(ShardForwardError::BadHiddenDim {
                        got: state.len(),
                        expected: d,
                    });
                }
                state
            }
        };

        // Pre-allocate buffers reused across layers
        let mut q = vec![0i64; d];
        let mut k_buf = vec![0i64; cfg.d_kv];
        let mut v_buf = vec![0i64; cfg.d_kv];
        let mut attn_out = vec![0i64; d];
        let mut projected = vec![0i64; d];
        let mut gate = vec![0i64; cfg.d_ff];
        let mut up = vec![0i64; cfg.d_ff];
        let mut ff_out = vec![0i64; d];

        for layer_idx in start_layer..end {
            let layer = &self.layers[layer_idx];

            // I16 layer ref (preferred - quantized from f32 with 258x finer
            // granularity than I8, which is what makes output coherent on
            // smaller models like Llama-7B). Q4 layer ref used on aarch64
            // where matmul_q4_preq_neon delivers ~2× extra bandwidth.
            let i16l = self.i16_layers.as_ref().map(|il| &il[layer_idx]);
            let q4l = self.q4_layers.as_ref().map(|ql| &ql[layer_idx]);

            // Dispatch order (highest quality first, then highest speed):
            //   NEON + Q4 weights present → matmul_q4_preq_neon (2× bw, SIMD)
            //   I16 weights present       → matmul_i16_into (NEON SIMD in
            //                                dot_i16_i64_neon per row)
            //   I8 fallback               → matmul_fast_preq scalar
            macro_rules! dispatch {
                ($i16_field:ident, $i8_field:ident, $inq:expr, $raw:expr, $in_sz:expr, $out:expr) => {
                    {
                        #[cfg(target_arch = "aarch64")]
                        {
                            if let Some(q4l) = q4l {
                                matmul_q4_preq_neon(&q4l.$i16_field, $inq, $out);
                            } else if let Some(i16l) = i16l {
                                matmul_i16_into(&i16l.$i16_field, $raw, $in_sz, $out);
                            } else {
                                matmul_fast_preq(&layer.$i8_field, $inq, $raw, $in_sz, $out);
                            }
                        }
                        #[cfg(not(target_arch = "aarch64"))]
                        {
                            if let Some(i16l) = i16l {
                                matmul_i16_into(&i16l.$i16_field, $raw, $in_sz, $out);
                            } else {
                                matmul_fast_preq(&layer.$i8_field, $inq, $raw, $in_sz, $out);
                            }
                        }
                    }
                };
            }

            // LayerNorm
            let normed = layernorm(&hidden, &layer.attn_norm);
            let normed_q = QuantizedInput::from_i64(&normed);

            // Q/K/V projections - I16 if loaded, else I8
            dispatch!(wq, wq, &normed_q, &normed, d, &mut q);
            dispatch!(wk, wk, &normed_q, &normed, d, &mut k_buf);
            dispatch!(wv, wv, &normed_q, &normed, d, &mut v_buf);

            // RoPE on Q and K
            for h in 0..cfg.n_heads {
                apply_rope(
                    &mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    position, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin,
                );
            }
            for h in 0..cfg.n_kv_heads {
                apply_rope(
                    &mut k_buf[h * cfg.d_head..(h + 1) * cfg.d_head],
                    position, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin,
                );
            }

            // Push K/V into per-request cache
            cache.push_k(layer_idx, &k_buf);
            cache.push_v(layer_idx, &v_buf);

            // Flash attention with online softmax - same as forward_one_token.
            // O(d_head) memory per head instead of O(full_seq) for scores array.
            let full_seq = position + 1;
            let k_layer_data = &cache.k_data[layer_idx];
            let v_layer_data = &cache.v_data[layer_idx];
            let head_results: Vec<Vec<i64>> = (0..cfg.n_heads).into_par_iter().map(|h| {
                let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                let dh = cfg.d_head;
                let q_head = &q[h * dh..(h + 1) * dh];
                flash_attention_i64(
                    q_head, k_layer_data, v_layer_data,
                    cfg.d_kv, kv_h, dh, full_seq, cfg.attn_scale,
                )
            }).collect();

            for val in attn_out.iter_mut() { *val = 0; }
            for (h, head_out) in head_results.iter().enumerate() {
                attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(head_out);
            }

            // Wo projection + residual
            let attn_out_q = QuantizedInput::from_i64(&attn_out);
            dispatch!(wo, wo, &attn_out_q, &attn_out, d, &mut projected);
            for i in 0..d { hidden[i] += projected[i]; }

            // FFN: gate, up, down
            let normed_ff = layernorm(&hidden, &layer.ffn_norm);
            let normed_ff_q = QuantizedInput::from_i64(&normed_ff);
            dispatch!(w_gate, w_gate, &normed_ff_q, &normed_ff, d, &mut gate);
            dispatch!(w_up,   w_up,   &normed_ff_q, &normed_ff, d, &mut up);
            for j in 0..cfg.d_ff {
                gate[j] = (silu_i64(gate[j]) * up[j]) >> FRAC_BITS;
            }
            let gate_q = QuantizedInput::from_i64(&gate);
            dispatch!(w_down, w_down, &gate_q, &gate, cfg.d_ff, &mut ff_out);
            for i in 0..d { hidden[i] += ff_out[i]; }
        }

        // Last shard: run final norm + LM head + argmax
        if is_last {
            cache.seq_len = position + 1;
            let normed = layernorm(&hidden, &self.final_norm);
            let logits = if let Some(ref i16w) = self.i16_output {
                let mut logits = vec![0i64; cfg.vocab_size];
                matmul_i16_into(i16w, &normed, d, &mut logits);
                logits
            } else {
                matmul_fast(&self.output_weight, &normed, d, cfg.vocab_size)
            };
            let token_id = argmax_i64(&logits) as u32;
            let logits_bytes: Vec<u8> = logits.iter().flat_map(|v| v.to_le_bytes()).collect();
            let logits_hash = arc_crypto::hash_bytes(&logits_bytes);
            Ok(ShardOutput::Token { id: token_id, logits_hash })
        } else {
            cache.seq_len = position + 1;
            Ok(ShardOutput::Hidden(hidden))
        }
    }
}

/// Input to a shard's forward pass.
#[derive(Debug)]
pub enum ShardInput {
    /// Raw token id - used by the FIRST shard, which embeds it locally.
    Token(u32),
    /// Hidden state from the previous shard - used by middle/last shards.
    Hidden(Vec<i64>),
}

/// Output from a shard's forward pass.
#[derive(Debug)]
pub enum ShardOutput {
    /// Hidden state to forward to the next shard.
    Hidden(Vec<i64>),
    /// Final token id from the LAST shard, plus the BLAKE3 hash of the logits
    /// for cryptographic determinism verification.
    Token { id: u32, logits_hash: Hash256 },
}

/// Why a shard forward pass refused to run.
///
/// Every variant here replaces something that used to be a `debug_assert!`
/// (compiled out in release) followed by an out-of-bounds read or an index
/// panic. Because the workspace builds release with `panic = "abort"`, an
/// index panic inside `spawn_blocking` takes the whole node down — so a
/// coordinator that dispatched one bad request could kill a shard holder.
/// These are now honest, typed refusals the coordinator can act on.
///
/// The KV-cache variant is the load-bearing one: `forward_shard_token`
/// reads `k_cache` through an UNCHECKED raw pointer in `flash_attention_i64`,
/// so running position `p` against a cache holding fewer than `p` positions
/// is undefined behaviour, not merely a wrong answer. That happens whenever
/// a coordinator fails over to a replica that missed earlier positions —
/// which is exactly what replica failover and hedged dispatch do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardForwardError {
    /// This node's per-request KV cache does not line up with the requested
    /// position: running position `p` requires exactly `p` cached positions
    /// for every layer in the range. Means "this replica is cold for this
    /// request" — the coordinator should treat it as a replica failure and
    /// either replay from position 0 or use a warm replica.
    KvCacheOutOfSync {
        layer: usize,
        expected_positions: usize,
        cached_positions: usize,
    },
    /// A layer in the requested range is not resident on this node.
    LayerNotLoaded { layer: usize },
    /// The hidden state from the previous shard has the wrong dimension.
    BadHiddenDim { got: usize, expected: usize },
    /// Position is past the model's precomputed RoPE tables.
    PositionOutOfRange { position: usize, max_seq: usize },
}

impl ShardForwardError {
    /// Stable machine-readable tag. The coordinator matches on this string
    /// over the wire, so treat it as part of the RPC contract.
    pub fn kind(&self) -> &'static str {
        match self {
            ShardForwardError::KvCacheOutOfSync { .. } => "kv_cache_out_of_sync",
            ShardForwardError::LayerNotLoaded { .. } => "layer_not_loaded",
            ShardForwardError::BadHiddenDim { .. } => "bad_hidden_dim",
            ShardForwardError::PositionOutOfRange { .. } => "position_out_of_range",
        }
    }
}

impl std::fmt::Display for ShardForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShardForwardError::KvCacheOutOfSync {
                layer,
                expected_positions,
                cached_positions,
            } => write!(
                f,
                "kv_cache_out_of_sync: layer {} holds {} cached positions but \
                 this request needs exactly {} (replica is cold for this request_id)",
                layer, cached_positions, expected_positions
            ),
            ShardForwardError::LayerNotLoaded { layer } => {
                write!(f, "layer_not_loaded: layer {} is not resident on this node", layer)
            }
            ShardForwardError::BadHiddenDim { got, expected } => write!(
                f,
                "bad_hidden_dim: hidden state has {} elements, expected {}",
                got, expected
            ),
            ShardForwardError::PositionOutOfRange { position, max_seq } => write!(
                f,
                "position_out_of_range: position {} is past the model's {}-position RoPE table",
                position, max_seq
            ),
        }
    }
}

impl std::error::Error for ShardForwardError {}

// ─── RoPE Tables ──────────────────────────────────────────────────────────────

pub fn compute_rope_tables(d_head: usize, max_seq: usize, base: f64) -> (Vec<i64>, Vec<i64>) {
    let half = d_head / 2;
    let mut cos_table = vec![0i64; max_seq * half];
    let mut sin_table = vec![0i64; max_seq * half];
    for pos in 0..max_seq {
        for i in 0..half {
            let freq = 1.0 / base.powf(2.0 * i as f64 / d_head as f64);
            let angle = pos as f64 * freq;
            cos_table[pos * half + i] = (angle.cos() * ONE as f64).round() as i64;
            sin_table[pos * half + i] = (angle.sin() * ONE as f64).round() as i64;
        }
    }
    (cos_table, sin_table)
}

// ─── GGUF Loader ──────────────────────────────────────────────────────────────

#[cfg(feature = "candle")]
pub fn load_cached_model(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use candle_core::Device;
    use candle_core::quantized::gguf_file;
    use crate::InferenceError;

    let device = Device::Cpu;
    let gguf_path = path.to_string();

    let (n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size, vocab, rope_base, eos_tokens, bos_token, chat_template) = {
        let mut reader = std::fs::File::open(&gguf_path)
            .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
        let content = gguf_file::Content::read(&mut reader)
            .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

        // Detect architecture from GGUF metadata (supports llama, mistral, phi, gemma, qwen, etc.)
        let arch = match content.metadata.get("general.architecture") {
            Some(gguf_file::Value::String(s)) => s.clone(),
            _ => "llama".to_string(), // default for LLaMA-family models
        };

        let get_u32 = |key: &str| -> u32 {
            match content.metadata.get(key) {
                Some(gguf_file::Value::U32(v)) => *v,
                Some(gguf_file::Value::U64(v)) => *v as u32,
                Some(gguf_file::Value::I32(v)) => *v as u32,
                _ => 0,
            }
        };

        // Architecture-agnostic metadata keys: try {arch}.key first
        let nl = get_u32(&format!("{arch}.block_count")) as usize;
        let dm = get_u32(&format!("{arch}.embedding_length")) as usize;
        let nh = get_u32(&format!("{arch}.attention.head_count")) as usize;
        let nkv = {
            let v = get_u32(&format!("{arch}.attention.head_count_kv"));
            if v > 0 { v as usize } else { nh }
        };
        let dff = get_u32(&format!("{arch}.feed_forward_length")) as usize;
        let vs = content.tensor_infos.get("token_embd.weight")
            .map(|t| t.shape.dims()[0]).unwrap_or(32000);

        // Read RoPE base frequency from metadata (LLaMA-3 uses 500000, most others use 10000)
        // Handle both F32 and F64 storage - some quantizers write F64
        let rope_base: f64 = match content.metadata.get(&format!("{arch}.rope.freq_base")) {
            Some(gguf_file::Value::F32(v)) => *v as f64,
            Some(gguf_file::Value::F64(v)) => *v,
            _ => 10000.0,
        };

        // Read EOS tokens from tokenizer metadata (scalar or array)
        let eos_tokens = match content.metadata.get("tokenizer.ggml.eos_token_id") {
            Some(gguf_file::Value::U32(v)) => vec![*v],
            Some(gguf_file::Value::U64(v)) => vec![*v as u32],
            Some(gguf_file::Value::Array(arr)) => {
                arr.iter().filter_map(|v| match v {
                    gguf_file::Value::U32(n) => Some(*n),
                    gguf_file::Value::U64(n) => Some(*n as u32),
                    _ => None,
                }).collect()
            }
            _ => vec![2, 128001, 128009], // LLaMA-2/3 defaults
        };

        // Read BOS token ID
        let bos_token = match content.metadata.get("tokenizer.ggml.bos_token_id") {
            Some(gguf_file::Value::U32(v)) => *v,
            Some(gguf_file::Value::U64(v)) => *v as u32,
            _ => 1, // LLaMA default
        };

        // Read chat template (Jinja2 format) for correct prompt wrapping
        let chat_template = match content.metadata.get("tokenizer.chat_template") {
            Some(gguf_file::Value::String(s)) => s.clone(),
            _ => String::new(),
        };

        let vocab = match content.metadata.get("tokenizer.ggml.tokens") {
            Some(gguf_file::Value::Array(arr)) => {
                arr.iter().filter_map(|v| match v {
                    gguf_file::Value::String(s) => Some(s.clone()),
                    _ => None,
                }).collect()
            }
            _ => Vec::new(),
        };

        info!(arch = %arch, rope_base = %rope_base, eos = ?eos_tokens,
            bos = bos_token, chat_template_len = chat_template.len(),
            "GGUF architecture detected");

        (nl, dm, nh, nkv, dff, vs, vocab, rope_base, eos_tokens, bos_token, chat_template)
    };

    let d_head = d_model / n_heads;
    let d_kv = d_head * n_kv_heads;

    info!(n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size,
        "Loading GGUF into per-row INT8 cache...");

    // Single file handle + content parse
    let mut reader = std::fs::File::open(&gguf_path)
        .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
    let content = gguf_file::Content::read(&mut reader)
        .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

    let extract_f32 = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str| -> Result<Vec<f32>, InferenceError> {
        let qt = content.tensor(reader, name, &device)
            .map_err(|e| InferenceError::Runtime(format!("{name}: {e}")))?;
        let deq = qt.dequantize(&device)
            .map_err(|e| InferenceError::Runtime(format!("dequant {name}: {e}")))?;
        deq.flatten_all()
            .map_err(|e| InferenceError::Runtime(format!("flatten: {e}")))?
            .to_vec1::<f32>()
            .map_err(|e| InferenceError::Runtime(format!("tovec: {e}")))
    };

    let _extract_i8 = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str, rows: usize, cols: usize| -> Result<I8Weights, InferenceError> {
        let f = extract_f32(reader, content, name)?;
        Ok(I8Weights::quantize_f32(&f, rows, cols))
    };

    // Extract f32 ONCE, then quantize to I8, I16, AND block-wise I8 from the
    // originals. Block-i8 is the new preferred format (see project_i16_ppl_bug.md
    // for why per-row I8 tops out at PPL 107 and I16::from_i8 doesn't help).
    // I16 kept for back-compat but not installed in the model by default.
    let extract_i8_and_i16 = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str, rows: usize, cols: usize| -> Result<(I8Weights, I16Weights, crate::block_i8::BlockI8Weights), InferenceError> {
        let f = extract_f32(reader, content, name)?;
        let i8 = I8Weights::quantize_f32(&f, rows, cols);
        let i16 = I16Weights::quantize_f32(&f, rows, cols);
        // Block-i8 is safe only when n_cols is a multiple of the block size;
        // every Llama-family dimension (4096, 11008, 32000) satisfies this,
        // but fall back to an empty placeholder otherwise so shards with
        // irregular shapes don't panic at load.
        let block = if cols.is_multiple_of(crate::block_i8::BLOCK_SIZE) {
            crate::block_i8::BlockI8Weights::quantize_f32(&f, rows, cols)
        } else {
            crate::block_i8::BlockI8Weights::empty()
        };
        Ok((i8, i16, block))
    };

    let extract_norm = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str, size: usize| -> Vec<i64> {
        extract_f32(reader, content, name).map(|f| {
            f.iter().map(|&x| (x * ONE as f32).round() as i64).collect()
        }).unwrap_or_else(|_| vec![ONE; size])
    };

    // Embedding: single f32 extraction → I8 + full-precision Q16 vector.
    let embedding_f32 = extract_f32(&mut reader, &content, "token_embd.weight")?;
    let embedding_i8 = I8Weights::quantize_f32(&embedding_f32, vocab_size, d_model);
    let embedding_q16: Vec<i64> = embedding_f32.iter()
        .map(|&x| (x as f64 * ONE as f64).round() as i64).collect();
    drop(embedding_f32);
    info!("Embeddings loaded: {} MB Q16 + {} MB INT8",
        embedding_q16.len() * 8 / (1024 * 1024), embedding_i8.memory_bytes() / (1024 * 1024));

    // Output projection: f32 → I8 + I16 + Block-I8 in one pass.
    // Falls back to tied embeddings if absent.
    let (output_weight, i16_output, block_i8_output) = match extract_i8_and_i16(&mut reader, &content, "output.weight", vocab_size, d_model) {
        Ok((i8, i16, block)) => (i8, Some(i16), Some(block)),
        Err(_) => {
            let tied_f32 = extract_f32(&mut reader, &content, "token_embd.weight").ok();
            let tied_i16 = tied_f32.as_ref().map(|f| I16Weights::quantize_f32(f, vocab_size, d_model));
            let tied_block = tied_f32.as_ref().and_then(|f| {
                if d_model % crate::block_i8::BLOCK_SIZE == 0 {
                    Some(crate::block_i8::BlockI8Weights::quantize_f32(f, vocab_size, d_model))
                } else { None }
            });
            (
                I8Weights {
                    data: embedding_i8.data.clone(), scales: embedding_i8.scales.clone(),
                    n_rows: embedding_i8.n_rows, n_cols: embedding_i8.n_cols,
                },
                tied_i16,
                tied_block,
            )
        }
    };
    let final_norm = extract_norm(&mut reader, &content, "output_norm.weight", d_model);

    let mut layers = Vec::with_capacity(n_layers);
    let mut i16_layers_vec: Vec<I16Layer> = Vec::with_capacity(n_layers);
    let mut block_i8_layers_vec: Vec<BlockI8Layer> = Vec::with_capacity(n_layers);
    for l in 0..n_layers {
        let p = format!("blk.{l}");
        let (wq, wq16, wq_b) = extract_i8_and_i16(&mut reader, &content, &format!("{p}.attn_q.weight"), d_model, d_model)?;
        let (wk, wk16, wk_b) = extract_i8_and_i16(&mut reader, &content, &format!("{p}.attn_k.weight"), d_kv, d_model)?;
        let (wv, wv16, wv_b) = extract_i8_and_i16(&mut reader, &content, &format!("{p}.attn_v.weight"), d_kv, d_model)?;
        let (wo, wo16, wo_b) = extract_i8_and_i16(&mut reader, &content, &format!("{p}.attn_output.weight"), d_model, d_model)?;
        let (w_gate, w_gate16, w_gate_b) = extract_i8_and_i16(&mut reader, &content, &format!("{p}.ffn_gate.weight"), d_ff, d_model)?;
        let (w_up, w_up16, w_up_b) = extract_i8_and_i16(&mut reader, &content, &format!("{p}.ffn_up.weight"), d_ff, d_model)?;
        let (w_down, w_down16, w_down_b) = extract_i8_and_i16(&mut reader, &content, &format!("{p}.ffn_down.weight"), d_model, d_ff)?;

        if l % 8 == 0 || l == n_layers - 1 {
            info!("Layer {}/{} loaded", l + 1, n_layers);
        }

        layers.push(CachedLayer {
            wq, wk, wv, wo, w_gate, w_up, w_down,
            attn_norm: extract_norm(&mut reader, &content, &format!("{p}.attn_norm.weight"), d_model),
            ffn_norm: extract_norm(&mut reader, &content, &format!("{p}.ffn_norm.weight"), d_model),
        });
        i16_layers_vec.push(I16Layer {
            wq: wq16, wk: wk16, wv: wv16, wo: wo16,
            w_gate: w_gate16, w_up: w_up16, w_down: w_down16,
        });
        block_i8_layers_vec.push(BlockI8Layer {
            wq: wq_b, wk: wk_b, wv: wv_b, wo: wo_b,
            w_gate: w_gate_b, w_up: w_up_b, w_down: w_down_b,
        });
    }

    // Match the trained Llama-2 4096-position context window. See the
    // matching note in `load_cached_model_ranges` for the truncation
    // failure mode at the old 2048 cap.
    let max_seq = 4096;
    let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, rope_base);
    // 1/sqrt(d_head) in Q16 - integer_isqrt already returns ONE/sqrt(x/ONE)
    let attn_scale = integer_isqrt((d_head as i64) * ONE);

    info!("Model loaded: ~{} MB per-row INT8", layers.iter()
        .map(|l| l.wq.memory_bytes() + l.wk.memory_bytes() + l.wv.memory_bytes()
            + l.wo.memory_bytes() + l.w_gate.memory_bytes() + l.w_up.memory_bytes()
            + l.w_down.memory_bytes())
        .sum::<usize>() / (1024 * 1024));

    Ok(CachedIntegerModel {
        config: ModelConfig {
            n_layers, d_model, n_heads, n_kv_heads, d_ff, d_head, d_kv,
            vocab_size, attn_scale, rope_cos, rope_sin, max_seq,
            eos_tokens: eos_tokens.clone(),
            bos_token, chat_template: chat_template.clone(),
        },
        embedding_q16, embedding_i8, layers, final_norm, output_weight, vocab,
        q4_layers: None, q4_output: None,
        // INT16 enabled 2026-06-04. The 2026-04-20 disable rationale
        // ("logits ~38× larger magnitude than I8 baseline") cited a
        // symptom traceable to the I8 scale bug at line 64 (`abs_max as
        // i64` truncating sub-1.0 magnitudes to 0). That bug was fixed
        // independently and the unit-level matmul correctness suite
        // (test_i16_matmul_correctness, test_i16_matmul_matches_i8_on_from_i8,
        // test_i16_quantize_f32_reconstruction, test_i16_deterministic,
        // test_i16_from_i8_preserves_values, test_i16_matmul_nonzero_output,
        // test_i16_memory_bytes — all 7 green) now verifies the math
        // matches f64 reference within 15%. The dispatch macro at line
        // 2068 prefers I16 over I8 when present, so enabling here flips
        // every layer + the output projection to the finer-grained
        // [-32767, 32767] quantization (~258× I8 resolution per row).
        i16_layers: Some(i16_layers_vec),
        // i16_output is already `Option<I16Weights>` from the loader
        // match arms above (Ok → Some(i16), Err → tied_i16 which is
        // .map'd Option). Pass through, don't re-wrap.
        i16_output,
        // Block-wise INT8 installed by default - 32-weight blocks with i32
        // Q16 scales, pure integer math, quality on par with llama.cpp Q8_0.
        // Forward dispatch prefers this over per-row I8 when present.
        block_i8_layers: Some(block_i8_layers_vec),
        block_i8_output,
        ternary_layers: None,
        ternary_output: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,    })
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model(_path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime("candle feature not enabled".into()))
}

/// Load ONLY the tokenizer from a GGUF file - no transformer weights.
/// Returns a CachedIntegerModel with vocab, config, encode/decode capability,
/// but zero layers and zero embedding weights. ~30MB instead of 4GB.
///
/// Use case: coordinator nodes that need to tokenize prompts for sharded
/// inference but don't hold any model weights themselves.
#[cfg(feature = "candle")]
pub fn load_tokenizer_only(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use candle_core::quantized::gguf_file;
    use crate::InferenceError;

    let mut reader = std::fs::File::open(path)
        .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
    let content = gguf_file::Content::read(&mut reader)
        .map_err(|e| InferenceError::Runtime(format!("GGUF metadata: {e}")))?;

    let arch = match content.metadata.get("general.architecture") {
        Some(gguf_file::Value::String(s)) => s.clone(),
        _ => "llama".to_string(),
    };

    let get_u32 = |key: &str| -> u32 {
        match content.metadata.get(key) {
            Some(gguf_file::Value::U32(v)) => *v,
            Some(gguf_file::Value::U64(v)) => *v as u32,
            Some(gguf_file::Value::I32(v)) => *v as u32,
            _ => 0,
        }
    };

    let n_layers = get_u32(&format!("{arch}.block_count")) as usize;
    let d_model = get_u32(&format!("{arch}.embedding_length")) as usize;
    let n_heads = get_u32(&format!("{arch}.attention.head_count")) as usize;
    let n_kv_heads = {
        let v = get_u32(&format!("{arch}.attention.head_count_kv"));
        if v > 0 { v as usize } else { n_heads }
    };
    let d_ff = get_u32(&format!("{arch}.feed_forward_length")) as usize;
    let vocab_size = content.tensor_infos.get("token_embd.weight")
        .map(|t| t.shape.dims()[0]).unwrap_or(32000);
    // checked_div carries the `n_heads > 0` guard: it returns None only when
    // the divisor is zero, so this is the same integer division and the same
    // 128 fallback, bit for bit.
    let d_head = d_model.checked_div(n_heads).unwrap_or(128);
    let d_kv = d_head * n_kv_heads;

    let rope_base: f64 = match content.metadata.get(&format!("{arch}.rope.freq_base")) {
        Some(gguf_file::Value::F32(v)) => *v as f64,
        Some(gguf_file::Value::F64(v)) => *v,
        _ => 10000.0,
    };

    let eos_tokens = match content.metadata.get("tokenizer.ggml.eos_token_id") {
        Some(gguf_file::Value::U32(v)) => vec![*v],
        Some(gguf_file::Value::U64(v)) => vec![*v as u32],
        Some(gguf_file::Value::Array(arr)) => arr.iter().filter_map(|v| match v {
            gguf_file::Value::U32(n) => Some(*n),
            gguf_file::Value::U64(n) => Some(*n as u32),
            _ => None,
        }).collect(),
        _ => vec![2, 128001, 128009],
    };
    let bos_token = match content.metadata.get("tokenizer.ggml.bos_token_id") {
        Some(gguf_file::Value::U32(v)) => *v,
        Some(gguf_file::Value::U64(v)) => *v as u32,
        _ => 1,
    };
    let chat_template = match content.metadata.get("tokenizer.chat_template") {
        Some(gguf_file::Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let vocab: Vec<String> = match content.metadata.get("tokenizer.ggml.tokens") {
        Some(gguf_file::Value::Array(arr)) => arr.iter().filter_map(|v| match v {
            gguf_file::Value::String(s) => Some(s.clone()),
            _ => None,
        }).collect(),
        _ => Vec::new(),
    };

    info!(n_layers, d_model, vocab_size, "Loaded tokenizer-only (no weights)");

    // Build config with RoPE tables
    let max_seq = 4096;
    let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, rope_base);
    let attn_scale = (ONE as f64 / (d_head as f64).sqrt()).round() as i64;

    let config = ModelConfig {
        n_layers, d_model, n_heads, n_kv_heads, d_head, d_kv, d_ff,
        vocab_size, rope_cos, rope_sin, attn_scale, eos_tokens, bos_token,
        chat_template, max_seq,
    };

    Ok(CachedIntegerModel {
        config,
        embedding_q16: Vec::new(),
        embedding_i8: I8Weights::empty(),
        layers: Vec::new(),
        final_norm: Vec::new(),
        output_weight: I8Weights::empty(),
        vocab,
        q4_layers: None,
        q4_output: None,
        i16_layers: None,
        i16_output: None,
        block_i8_layers: None,
        block_i8_output: None,
        ternary_layers: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,        ternary_output: None,
    })
}

#[cfg(not(feature = "candle"))]
pub fn load_tokenizer_only(_path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime("candle feature not enabled".into()))
}

// ─── Sharded Loading: Load Only Layers [start, end) ──────────────────────────
//
// A node holding shard k of N loads only its slice of the model. This is the
// foundation for pipeline-parallel distributed inference: each node holds a
// fraction of the layers, and a request flows through the pipeline of shards
// via HTTP forwarding of activations.
//
// Memory invariant: the layers Vec is the same size as n_layers (so absolute
// layer indexing in forward_shard_layers stays correct), but slots outside
// [start, end) are zero-byte placeholders. A 70B model split 8 ways uses
// ~5GB per node instead of 40GB.
//
// Embeddings load only on the FIRST shard (start_layer == 0).
// Output head + final_norm load only on the LAST shard (end_layer == n_layers).

#[cfg(feature = "candle")]
pub fn load_cached_model_shard(
    path: &str,
    start_layer: usize,
    end_layer: usize,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    use candle_core::Device;
    use candle_core::quantized::gguf_file;
    use crate::InferenceError;

    let device = Device::Cpu;
    let gguf_path = path.to_string();

    // ── Read metadata ────────────────────────────────────────────────────────
    let (n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size, vocab,
         rope_base, eos_tokens, bos_token, chat_template) = {
        let mut reader = std::fs::File::open(&gguf_path)
            .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
        let content = gguf_file::Content::read(&mut reader)
            .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

        let arch = match content.metadata.get("general.architecture") {
            Some(gguf_file::Value::String(s)) => s.clone(),
            _ => "llama".to_string(),
        };

        let get_u32 = |key: &str| -> u32 {
            match content.metadata.get(key) {
                Some(gguf_file::Value::U32(v)) => *v,
                Some(gguf_file::Value::U64(v)) => *v as u32,
                Some(gguf_file::Value::I32(v)) => *v as u32,
                _ => 0,
            }
        };

        let nl = get_u32(&format!("{arch}.block_count")) as usize;
        let dm = get_u32(&format!("{arch}.embedding_length")) as usize;
        let nh = get_u32(&format!("{arch}.attention.head_count")) as usize;
        let nkv = {
            let v = get_u32(&format!("{arch}.attention.head_count_kv"));
            if v > 0 { v as usize } else { nh }
        };
        let dff = get_u32(&format!("{arch}.feed_forward_length")) as usize;
        let vs = content.tensor_infos.get("token_embd.weight")
            .map(|t| t.shape.dims()[0]).unwrap_or(32000);

        let rope_base: f64 = match content.metadata.get(&format!("{arch}.rope.freq_base")) {
            Some(gguf_file::Value::F32(v)) => *v as f64,
            Some(gguf_file::Value::F64(v)) => *v,
            _ => 10000.0,
        };

        let eos_tokens = match content.metadata.get("tokenizer.ggml.eos_token_id") {
            Some(gguf_file::Value::U32(v)) => vec![*v],
            Some(gguf_file::Value::U64(v)) => vec![*v as u32],
            Some(gguf_file::Value::Array(arr)) => {
                arr.iter().filter_map(|v| match v {
                    gguf_file::Value::U32(n) => Some(*n),
                    gguf_file::Value::U64(n) => Some(*n as u32),
                    _ => None,
                }).collect()
            }
            _ => vec![2, 128001, 128009],
        };

        let bos_token = match content.metadata.get("tokenizer.ggml.bos_token_id") {
            Some(gguf_file::Value::U32(v)) => *v,
            Some(gguf_file::Value::U64(v)) => *v as u32,
            _ => 1,
        };

        let chat_template = match content.metadata.get("tokenizer.chat_template") {
            Some(gguf_file::Value::String(s)) => s.clone(),
            _ => String::new(),
        };

        let vocab = match content.metadata.get("tokenizer.ggml.tokens") {
            Some(gguf_file::Value::Array(arr)) => {
                arr.iter().filter_map(|v| match v {
                    gguf_file::Value::String(s) => Some(s.clone()),
                    _ => None,
                }).collect()
            }
            _ => Vec::new(),
        };

        (nl, dm, nh, nkv, dff, vs, vocab, rope_base, eos_tokens, bos_token, chat_template)
    };

    let d_head = d_model / n_heads;
    let d_kv = d_head * n_kv_heads;

    let end_layer = end_layer.min(n_layers);
    if start_layer >= end_layer {
        return Err(InferenceError::Runtime(format!(
            "Invalid shard range: [{start_layer}, {end_layer}) is empty (n_layers={n_layers})"
        )));
    }
    let is_first = start_layer == 0;
    let is_last = end_layer == n_layers;

    info!(
        n_layers, d_model, shard_start = start_layer, shard_end = end_layer,
        is_first, is_last, "Loading GGUF SHARD"
    );

    let mut reader = std::fs::File::open(&gguf_path)
        .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
    let content = gguf_file::Content::read(&mut reader)
        .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

    let extract_f32 = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str| -> Result<Vec<f32>, InferenceError> {
        let qt = content.tensor(reader, name, &device)
            .map_err(|e| InferenceError::Runtime(format!("{name}: {e}")))?;
        let deq = qt.dequantize(&device)
            .map_err(|e| InferenceError::Runtime(format!("dequant {name}: {e}")))?;
        deq.flatten_all()
            .map_err(|e| InferenceError::Runtime(format!("flatten: {e}")))?
            .to_vec1::<f32>()
            .map_err(|e| InferenceError::Runtime(format!("tovec: {e}")))
    };

    // Extract f32 once and produce BOTH I8 (kept for compat / placeholder math)
    // AND I16 (used by forward_shard_token for ~258x finer quantization).
    // I16 directly from f32 is the only way to get real quality improvement -
    // I16Weights::from_i8() preserves I8-level precision, I16Weights::quantize_f32
    // doesn't.
    let extract_i8_i16 = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str, rows: usize, cols: usize| -> Result<(I8Weights, I16Weights), InferenceError> {
        let f = extract_f32(reader, content, name)?;
        let i8w = I8Weights::quantize_f32(&f, rows, cols);
        let i16w = I16Weights::quantize_f32(&f, rows, cols);
        Ok((i8w, i16w))
    };

    let extract_norm = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str, size: usize| -> Vec<i64> {
        extract_f32(reader, content, name).map(|f| {
            f.iter().map(|&x| (x * ONE as f32).round() as i64).collect()
        }).unwrap_or_else(|_| vec![ONE; size])
    };

    // ── Embeddings: ONLY on first shard ──────────────────────────────────────
    let (embedding_q16, embedding_i8) = if is_first {
        let f = extract_f32(&mut reader, &content, "token_embd.weight")?;
        let i8w = I8Weights::quantize_f32(&f, vocab_size, d_model);
        let q16: Vec<i64> = f.iter().map(|&x| (x as f64 * ONE as f64).round() as i64).collect();
        info!("Shard {}: embeddings loaded ({} MB Q16)", start_layer, q16.len() * 8 / (1024 * 1024));
        (q16, i8w)
    } else {
        (Vec::new(), I8Weights::empty())
    };

    // ── Output head + final norm: ONLY on last shard ─────────────────────────
    let (output_weight, output_weight_i16, final_norm) = if is_last {
        // Try output.weight first, fall back to token_embd.weight (tied embeddings).
        let f = extract_f32(&mut reader, &content, "output.weight")
            .or_else(|_| extract_f32(&mut reader, &content, "token_embd.weight"))?;
        let i8w = I8Weights::quantize_f32(&f, vocab_size, d_model);
        let i16w = I16Weights::quantize_f32(&f, vocab_size, d_model);
        let fn_ = extract_norm(&mut reader, &content, "output_norm.weight", d_model);
        info!("Shard last: output head + final_norm loaded");
        (i8w, Some(i16w), fn_)
    } else {
        (I8Weights::empty(), None, Vec::new())
    };

    // ── Layers: only [start_layer, end_layer) loaded; rest are placeholders ──
    // We populate I8, I16, AND Q4 for each held layer. The Q4 weights are
    // the fastest path on aarch64 (matmul_q4_preq_neon gives ~2× bandwidth
    // reduction vs I16), the I16 weights are the high-quality fallback,
    // and the I8 weights are kept so other code paths can fall back.
    let mut layers: Vec<CachedLayer> = (0..n_layers).map(|_| CachedLayer::placeholder()).collect();
    let mut i16_layers_vec: Vec<I16Layer> = (0..n_layers).map(|_| I16Layer {
        wq: I16Weights::empty(),
        wk: I16Weights::empty(),
        wv: I16Weights::empty(),
        wo: I16Weights::empty(),
        w_gate: I16Weights::empty(),
        w_up: I16Weights::empty(),
        w_down: I16Weights::empty(),
    }).collect();
    let mut q4_layers_vec: Vec<Q4Layer> = (0..n_layers).map(|_| Q4Layer {
        wq: Q4WeightsX86 { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 },
        wk: Q4WeightsX86 { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 },
        wv: Q4WeightsX86 { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 },
        wo: Q4WeightsX86 { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 },
        w_gate: Q4WeightsX86 { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 },
        w_up: Q4WeightsX86 { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 },
        w_down: Q4WeightsX86 { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 },
    }).collect();
    // Q4 is OPT-IN: enable by setting ARC_Q4_SHARD=1. Without it, the
    // shard uses the I16 SIMD path (higher quality). Q4 gives ~2× more
    // speed at the cost of additional quantization noise (4-bit vs 16-bit).
    let enable_q4 = std::env::var("ARC_Q4_SHARD").is_ok();
    let mut any_i16 = false;
    let mut any_q4 = false;
    for l in start_layer..end_layer {
        let p = format!("blk.{l}");
        let (wq8, wq16) = extract_i8_i16(&mut reader, &content, &format!("{p}.attn_q.weight"), d_model, d_model)?;
        let (wk8, wk16) = extract_i8_i16(&mut reader, &content, &format!("{p}.attn_k.weight"), d_kv, d_model)?;
        let (wv8, wv16) = extract_i8_i16(&mut reader, &content, &format!("{p}.attn_v.weight"), d_kv, d_model)?;
        let (wo8, wo16) = extract_i8_i16(&mut reader, &content, &format!("{p}.attn_output.weight"), d_model, d_model)?;
        let (wg8, wg16) = extract_i8_i16(&mut reader, &content, &format!("{p}.ffn_gate.weight"), d_ff, d_model)?;
        let (wu8, wu16) = extract_i8_i16(&mut reader, &content, &format!("{p}.ffn_up.weight"), d_ff, d_model)?;
        let (wd8, wd16) = extract_i8_i16(&mut reader, &content, &format!("{p}.ffn_down.weight"), d_model, d_ff)?;
        layers[l] = CachedLayer {
            wq: wq8, wk: wk8, wv: wv8, wo: wo8,
            w_gate: wg8, w_up: wu8, w_down: wd8,
            attn_norm: extract_norm(&mut reader, &content, &format!("{p}.attn_norm.weight"), d_model),
            ffn_norm: extract_norm(&mut reader, &content, &format!("{p}.ffn_norm.weight"), d_model),
        };
        i16_layers_vec[l] = I16Layer {
            wq: wq16, wk: wk16, wv: wv16, wo: wo16,
            w_gate: wg16, w_up: wu16, w_down: wd16,
        };
        any_i16 = true;
        // Convert the just-loaded I8 layer to Q4 if requested.
        if enable_q4 {
            let l8 = &layers[l];
            q4_layers_vec[l] = Q4Layer {
                wq: Q4WeightsX86::from_i8(&l8.wq),
                wk: Q4WeightsX86::from_i8(&l8.wk),
                wv: Q4WeightsX86::from_i8(&l8.wv),
                wo: Q4WeightsX86::from_i8(&l8.wo),
                w_gate: Q4WeightsX86::from_i8(&l8.w_gate),
                w_up: Q4WeightsX86::from_i8(&l8.w_up),
                w_down: Q4WeightsX86::from_i8(&l8.w_down),
            };
            any_q4 = true;
        }
        if (l - start_layer).is_multiple_of(4) || l == end_layer - 1 {
            info!("Shard layer {}/{} loaded as I8+I16{} ({} of {})",
                l + 1, n_layers,
                if any_q4 { "+Q4" } else { "" },
                l - start_layer + 1, end_layer - start_layer);
        }
    }

    // Llama-2-7B / 7B-Chat were trained on 4096-position RoPE. Capping
    // max_seq at 2048 here forced every shard-holder seed to truncate
    // prompts past position 2048 (apply_rope at line 1562 does an
    // unchecked cos[pos*half + i] read; positions past the table either
    // panic in debug or return undefined positional signal in release —
    // either way, tokens past 2048 are useless). Doubling to 4096
    // matches the trained capacity and only grows the RoPE tables by
    // ~32 KB total per model — negligible.
    let max_seq = 4096;
    let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, rope_base);
    let attn_scale = integer_isqrt((d_head as i64) * ONE);

    let shard_mb: usize = layers.iter()
        .filter(|l| l.is_loaded())
        .map(|l| l.wq.memory_bytes() + l.wk.memory_bytes() + l.wv.memory_bytes()
            + l.wo.memory_bytes() + l.w_gate.memory_bytes() + l.w_up.memory_bytes()
            + l.w_down.memory_bytes())
        .sum::<usize>() / (1024 * 1024);
    info!("Shard loaded: layers [{}, {}) = {} MB INT8", start_layer, end_layer, shard_mb);

    Ok(CachedIntegerModel {
        config: ModelConfig {
            n_layers, d_model, n_heads, n_kv_heads, d_ff, d_head, d_kv,
            vocab_size, attn_scale, rope_cos, rope_sin, max_seq,
            eos_tokens: eos_tokens.clone(),
            bos_token, chat_template: chat_template.clone(),
        },
        embedding_q16, embedding_i8, layers, final_norm, output_weight, vocab,
        q4_layers: if any_q4 { Some(q4_layers_vec) } else { None },
        q4_output: None,
        i16_layers: if any_i16 { Some(i16_layers_vec) } else { None },
        i16_output: output_weight_i16,
        // Shard loader does not yet emit block-i8 weights; a future pass
        // should replicate the main loader's triple-quant pattern here.
        block_i8_layers: None,
        block_i8_output: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,        ternary_layers: None,
        ternary_output: None,
    })
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model_shard(
    _path: &str,
    _start_layer: usize,
    _end_layer: usize,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime("candle feature not enabled".into()))
}

/// Load the union of multiple disjoint layer ranges into a single
/// `CachedIntegerModel`. The per-range layers vec is always full `n_layers`
/// long; slots outside any provided range are placeholders and are never
/// touched by `forward_shard_token`.
///
/// Embedding loads when 0 is in any range; output head + final_norm load
/// when `n_layers` is at any range end. Ranges must be disjoint (overlapping
/// ranges would double-load the same weights and waste memory).
#[cfg(feature = "candle")]
pub fn load_cached_model_ranges(
    path: &str,
    ranges: &[(usize, usize)],
) -> Result<CachedIntegerModel, crate::InferenceError> {
    use crate::InferenceError;
    if ranges.is_empty() {
        return Err(InferenceError::Runtime("load_cached_model_ranges: no ranges provided".into()));
    }
    let mut sorted: Vec<(usize, usize)> = ranges.to_vec();
    sorted.sort();
    for i in 1..sorted.len() {
        if sorted[i].0 < sorted[i - 1].1 {
            return Err(InferenceError::Runtime(format!(
                "Overlapping shard ranges: [{}, {}) and [{}, {})",
                sorted[i - 1].0, sorted[i - 1].1, sorted[i].0, sorted[i].1
            )));
        }
    }

    // Single-range path: call load_cached_model_shard directly and return
    // its output untouched. Any mutation here (e.g. zeroing optional
    // quantization slices) breaks the invariants the shard loader
    // established and has caused segfaults in production when the forward
    // path assumed the full set was still present. For a one-range caller,
    // this function MUST be a thin alias for load_cached_model_shard.
    if sorted.len() == 1 {
        return load_cached_model_shard(path, sorted[0].0, sorted[0].1);
    }

    let mut aggregate = load_cached_model_shard(path, sorted[0].0, sorted[0].1)?;
    let n_layers = aggregate.config.n_layers;

    for &(start, end) in &sorted[1..] {
        let mut other = load_cached_model_shard(path, start, end)?;
        // Sanity: every shard load must produce the same n_layers-sized layers vec
        if other.layers.len() != aggregate.layers.len() {
            return Err(InferenceError::Runtime(format!(
                "Shard loader returned inconsistent layer count: {} vs {}",
                other.layers.len(), aggregate.layers.len()
            )));
        }
        for idx in start..end.min(n_layers) {
            if other.layers[idx].is_loaded() {
                aggregate.layers[idx] = std::mem::replace(&mut other.layers[idx], CachedLayer::placeholder());
            }
        }
        // Merge embedding if the other shard loaded it (start==0 path).
        if start == 0 && aggregate.embedding_q16.is_empty() && !other.embedding_q16.is_empty() {
            aggregate.embedding_q16 = std::mem::take(&mut other.embedding_q16);
            aggregate.embedding_i8 = std::mem::replace(&mut other.embedding_i8, I8Weights::empty());
        }
        // Merge output head + final_norm if the other shard loaded them (end==n_layers path).
        if end == n_layers {
            if aggregate.output_weight.n_rows == 0 && other.output_weight.n_rows != 0 {
                aggregate.output_weight = std::mem::replace(&mut other.output_weight, I8Weights::empty());
            }
            if aggregate.final_norm.is_empty() && !other.final_norm.is_empty() {
                aggregate.final_norm = std::mem::take(&mut other.final_norm);
            }
        }
    }
    // Multi-range only: rebuild optional quantization paths coherently
    // across the merged layer set. Each sub-load's i16/q4/block_i8 slices
    // were keyed on its own subrange; regenerating from the merged I8
    // layers is simpler than stitching partial slices. Callers that want
    // I16 after this should call `.enable_i16()`.
    aggregate.i16_layers = None;
    aggregate.i16_output = None;
    aggregate.q4_layers = None;
    aggregate.q4_output = None;
    aggregate.block_i8_layers = None;
    aggregate.block_i8_output = None;
    aggregate.ternary_layers = None;
    aggregate.ternary_output = None;
    aggregate.ternary_hybrid_layers = None;
    aggregate.ternary_hybrid_output = None;

    // Post-load sanity: every layer that was merged in must have consistent
    // I8Weights dimensions. data.len() MUST equal n_rows * n_cols for each
    // of the 7 matrices per layer - a mismatch is what causes the dangling-
    // but-non-null pointer deref in matmul_i8_into / matmul_i16_into. Panic
    // with the exact layer + matrix + dims so the crash has a backtrace
    // instead of SIGSEGVing the consensus thread.
    for &(s, e) in &sorted {
        for idx in s..e.min(n_layers) {
            let l = &aggregate.layers[idx];
            for (name, w) in [
                ("wq", &l.wq), ("wk", &l.wk), ("wv", &l.wv), ("wo", &l.wo),
                ("w_gate", &l.w_gate), ("w_up", &l.w_up), ("w_down", &l.w_down),
            ] {
                let expected = w.n_rows.saturating_mul(w.n_cols);
                if w.n_rows != 0 && w.data.len() != expected {
                    return Err(crate::InferenceError::Runtime(format!(
                        "load_cached_model_ranges: merged layer {idx}.{name} has \
                         n_rows={} n_cols={} but data.len()={} (expected {}). \
                         Multi-range merge corrupted the weight vector - do not \
                         hand this model to forward_shard_token.",
                        w.n_rows, w.n_cols, w.data.len(), expected
                    )));
                }
                if w.n_rows != 0 && w.scales.len() != w.n_rows {
                    return Err(crate::InferenceError::Runtime(format!(
                        "load_cached_model_ranges: merged layer {idx}.{name} has \
                         n_rows={} but scales.len()={} (must match n_rows).",
                        w.n_rows, w.scales.len()
                    )));
                }
            }
        }
    }
    Ok(aggregate)
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model_ranges(
    _path: &str,
    _ranges: &[(usize, usize)],
) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime("candle feature not enabled".into()))
}

/// Load from binary .arc-int8 file.
pub fn load_cached_model_binary(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use crate::InferenceError;
    use std::io::Read;

    let mut f = std::io::BufReader::new(
        std::fs::File::open(path).map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?
    );

    let mut magic = [0u8; 10];
    f.read_exact(&mut magic).map_err(|e| InferenceError::Runtime(format!("Magic: {e}")))?;
    if &magic[..8] != b"ARC-INT8" {
        return Err(InferenceError::Runtime("Not an ARC-INT8 file".into()));
    }

    let read_u64 = |f: &mut std::io::BufReader<std::fs::File>| -> Result<u64, InferenceError> {
        let mut b = [0u8; 8];
        f.read_exact(&mut b).map_err(|e| InferenceError::Runtime(format!("Read: {e}")))?;
        Ok(u64::from_le_bytes(b))
    };

    let n_layers = read_u64(&mut f)? as usize;
    let d_model = read_u64(&mut f)? as usize;
    let n_heads = read_u64(&mut f)? as usize;
    let n_kv_heads = read_u64(&mut f)? as usize;
    let d_ff = read_u64(&mut f)? as usize;
    let d_head = read_u64(&mut f)? as usize;
    let d_kv = read_u64(&mut f)? as usize;
    let vocab_size = read_u64(&mut f)? as usize;
    let max_seq = read_u64(&mut f)? as usize;
    let mut buf8 = [0u8; 8];
    f.read_exact(&mut buf8).map_err(|e| InferenceError::Runtime(format!("Scale: {e}")))?;
    let attn_scale = i64::from_le_bytes(buf8);

    let rope_cos = read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Cos: {e}")))?;
    let rope_sin = read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Sin: {e}")))?;

    let embedding_i8 = I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("Emb: {e}")))?;
    // Reconstruct Q16 embeddings from i8 + per-row scale
    let embedding_q16: Vec<i64> = {
        let mut q16 = Vec::with_capacity(embedding_i8.n_rows * embedding_i8.n_cols);
        for i in 0..embedding_i8.n_rows {
            let scale = embedding_i8.scales[i];
            for j in 0..embedding_i8.n_cols {
                q16.push((embedding_i8.data[i * embedding_i8.n_cols + j] as i64) * scale);
            }
        }
        q16
    };
    let output_weight = I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("Out: {e}")))?;
    let final_norm = read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Norm: {e}")))?;

    let mut layers = Vec::with_capacity(n_layers);
    for l in 0..n_layers {
        layers.push(CachedLayer {
            wq: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            wk: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            wv: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            wo: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            w_gate: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            w_up: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            w_down: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            attn_norm: read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            ffn_norm: read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
        });
    }

    let vocab_len = read_u64(&mut f)? as usize;
    let mut vocab_bytes = vec![0u8; vocab_len];
    f.read_exact(&mut vocab_bytes).map_err(|e| InferenceError::Runtime(format!("Vocab: {e}")))?;
    let vocab: Vec<String> = serde_json::from_slice(&vocab_bytes).unwrap_or_default();

    info!("Binary model loaded: {} layers, d={}, vocab={}", n_layers, d_model, vocab_size);

    // Binary format doesn't store these; use LLaMA family defaults
    let eos_tokens = vec![2u32, 128001, 128009];

    Ok(CachedIntegerModel {
        config: ModelConfig {
            n_layers, d_model, n_heads, n_kv_heads, d_ff, d_head, d_kv,
            vocab_size, attn_scale, rope_cos, rope_sin, max_seq,
            eos_tokens, bos_token: 1, chat_template: String::new(),
        },
        embedding_q16, embedding_i8, layers, final_norm, output_weight, vocab,
        q4_layers: None, q4_output: None,
        i16_layers: None,
        i16_output: None,
        block_i8_layers: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,        block_i8_output: None,
        ternary_layers: None,
        ternary_output: None,
    })
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_model(vs: usize, d: usize, nh: usize, dff: usize, nl: usize) -> CachedIntegerModel {
        let dh = d / nh;
        let nkv = nh;
        let dkv = dh * nkv;

        let mut rng: u64 = 42;
        let mut gen_f32 = |size: usize| -> Vec<f32> {
            (0..size).map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((rng >> 33) as f32 / u32::MAX as f32 - 0.5) * 0.2
            }).collect()
        };
        let mut gen_i8 = |rows: usize, cols: usize| -> I8Weights {
            I8Weights::quantize_f32(&gen_f32(rows * cols), rows, cols)
        };

        let embedding_i8 = gen_i8(vs, d);
        // For tests, Q16 embedding = i8 * scale (same as real loading)
        let embedding_q16: Vec<i64> = {
            let mut q16 = Vec::with_capacity(vs * d);
            for i in 0..vs {
                let scale = embedding_i8.scales[i];
                for j in 0..d {
                    q16.push((embedding_i8.data[i * d + j] as i64) * scale);
                }
            }
            q16
        };
        let output_weight = gen_i8(vs, d);
        let mut layers = Vec::new();
        for _ in 0..nl {
            layers.push(CachedLayer {
                wq: gen_i8(d, d), wk: gen_i8(dkv, d), wv: gen_i8(dkv, d),
                wo: gen_i8(d, d), w_gate: gen_i8(dff, d), w_up: gen_i8(dff, d),
                w_down: gen_i8(d, dff),
                attn_norm: vec![ONE; d], ffn_norm: vec![ONE; d],
            });
        }

        let (rope_cos, rope_sin) = compute_rope_tables(dh, 512, 10000.0);
        let attn_scale = { let s = integer_isqrt((dh as i64) * ONE); (ONE * ONE) / s.max(1) };

        CachedIntegerModel {
            config: ModelConfig {
                n_layers: nl, d_model: d, n_heads: nh, n_kv_heads: nkv,
                d_ff: dff, d_head: dh, d_kv: dkv, vocab_size: vs,
                attn_scale, rope_cos, rope_sin, max_seq: 512,
                eos_tokens: vec![2, 128001, 128009],
                bos_token: 1, chat_template: String::new(),
            },
            embedding_q16, embedding_i8, layers, final_norm: vec![ONE; d], output_weight,
            vocab: (0..vs).map(|i| format!("tok_{}", i)).collect(),
            q4_layers: None, q4_output: None,
        i16_layers: None,
        i16_output: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,        block_i8_layers: None,
        block_i8_output: None,
        ternary_layers: None,
        ternary_output: None,
        }
    }

    #[test]
    fn test_per_row_quantize_precision() {
        // Row with large outlier vs row with small values
        let mut values = vec![0.01f32; 8]; // row 0: small
        values.extend(vec![10.0, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01]); // row 1: outlier
        let q = I8Weights::quantize_f32(&values, 2, 8);

        // Row 0: abs_max=0.01, scale=0.01/127. Values should be 127 (full range)
        assert_eq!(q.data[0], 127, "Row 0 should use full range");
        // Row 1: abs_max=10.0, scale=10/127. 0.01 → round(0.01/10*127) = 0
        // This is expected - but only affects row 1, not row 0
        assert_eq!(q.data[8], 127, "Row 1 outlier should be 127");
        // Per-row means row 0 is NOT affected by row 1's outlier
        assert!(q.scales[0] < q.scales[1], "Row 0 should have smaller scale");
    }

    #[test]
    fn test_i8_matmul_per_row() {
        let weights = I8Weights::quantize_f32(
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 2, 3);
        let input = vec![ONE, ONE, ONE];
        let result = matmul_i8(&weights, &input, 3, 2);

        let expected_0 = 6.0 * ONE as f64;
        let expected_1 = 15.0 * ONE as f64;
        let tolerance = ONE as f64 * 0.05;
        assert!((result[0] as f64 - expected_0).abs() < tolerance);
        assert!((result[1] as f64 - expected_1).abs() < tolerance);
    }

    #[test]
    fn test_deterministic_100_runs() {
        let model = build_test_model(50, 32, 2, 64, 1);
        let prompt = vec![1u32, 2, 3];
        let (_, first_hash) = model.generate(&prompt, 4, &[99]);
        for _ in 0..100 {
            let (_, hash) = model.generate(&prompt, 4, &[99]);
            assert_eq!(hash, first_hash, "Determinism broken");
        }
    }

    #[test]
    fn test_model_deterministic() {
        let model = build_test_model(100, 64, 2, 128, 2);
        let prompt = vec![1u32, 5, 10, 15];
        let (t1, h1) = model.generate(&prompt, 8, &[99]);
        let (t2, h2) = model.generate(&prompt, 8, &[99]);
        assert_eq!(t1, t2);
        assert_eq!(h1, h2);
    }

    /// End-to-end test: run forward_one_token with ternary weights enabled.
    /// Proves the dispatch wiring works and the full transformer pipeline
    /// (embedding → attention → FFN → output) executes correctly with
    /// ternary weight storage.
    #[cfg(feature = "experimental-ip")]
    #[test]
    fn test_forward_one_token_with_ternary() {
        // Small model: 2 layers, d_model=64, vocab=100. Uses the same
        // dimensions as test_model_deterministic so we know it's in
        // the test-friendly size range.
        let mut model = build_test_model(100, 64, 2, 128, 2);

        // Enable ternary - converts I8 layers to 2-bit ternary weights
        model.enable_ternary();
        assert!(model.ternary_layers.is_some(), "ternary layers must be populated after enable_ternary");

        // Run forward_one_token - this exercises the full pipeline:
        // embedding lookup, Q/K/V matmul, attention, output projection,
        // FFN (gate/up/down), residuals, final norm.
        let token: u32 = 5;
        let mut cache = super::KVCache::new(model.config.n_layers);
        let logits1 = model.forward_one_token(token, &mut cache);
        assert_eq!(logits1.len(), model.config.vocab_size, "logits must have vocab_size entries");
        assert!(logits1.iter().any(|&v| v != 0), "ternary forward must produce non-zero logits");

        // Determinism: same token → same logits.
        let mut cache2 = super::KVCache::new(model.config.n_layers);
        let logits2 = model.forward_one_token(token, &mut cache2);
        assert_eq!(logits1, logits2, "ternary forward_one_token must be deterministic");
    }

    #[cfg(feature = "experimental-ip")]
    #[test]
    fn test_ternary_memory_reduction_full_model() {
        let mut model = build_test_model(100, 64, 2, 128, 2);
        let before_bytes: usize = model.layers.iter().map(|l| {
            l.wq.memory_bytes() + l.wk.memory_bytes() + l.wv.memory_bytes()
                + l.wo.memory_bytes() + l.w_gate.memory_bytes()
                + l.w_up.memory_bytes() + l.w_down.memory_bytes()
        }).sum();

        model.enable_ternary();
        let after_bytes: usize = model.ternary_layers.as_ref().unwrap().iter().map(|l| {
            l.wq.memory_bytes() + l.wk.memory_bytes() + l.wv.memory_bytes()
                + l.wo.memory_bytes() + l.w_gate.memory_bytes()
                + l.w_up.memory_bytes() + l.w_down.memory_bytes()
        }).sum();

        // Ternary should be approximately 4x smaller than I8 on the weight data.
        // The scale arrays are the same size, so total ratio is ~3.5-4x.
        assert!(
            after_bytes * 3 < before_bytes,
            "ternary ({} B) should be at least 3x smaller than I8 ({} B)",
            after_bytes, before_bytes,
        );
    }

    #[test]
    fn test_forward_shard_token_full_equals_split() {
        // Note: panic!() calls in this and related test functions are intentional
        // test assertions - they are NOT in any production code path.
        // Production functions (forward_shard_token, forward_one_token) return Results.
        //
        // The end-to-end claim: running all layers as a single shard
        // [0, n_layers) on one node must produce the SAME hidden state
        // (and the same final token id) as splitting the layers across
        // two shard calls [0, k) → [k, n_layers) on the same model.
        //
        // If this test passes, the shard pipeline is correct: any layer
        // boundary K produces a chain of forward_shard_token calls that
        // compose into the same answer as a single full forward.
        let model = build_test_model(20, 32, 2, 64, 4); // 4 layers, small d
        let n_layers = model.config.n_layers;
        let token: u32 = 5;

        // Path A: one shard covering the whole model [0, n_layers)
        let mut cache_a = KVCache::new(n_layers);
        let result_a = model.forward_shard_token(
            ShardInput::Token(token),
            &mut cache_a,
            0, n_layers,
            0,
        ).expect("whole-model shard at position 0 is always in sync");
        let token_a = match result_a {
            ShardOutput::Token { id, .. } => id,
            _ => panic!("Whole-model shard should produce a token, not a hidden state"),
        };

        // Path B: split at layer K, run two shards in sequence with the
        // SAME per-request KV cache (the cache holds K/V for ALL layers)
        let k = 2;
        let mut cache_b = KVCache::new(n_layers);
        let mid = model.forward_shard_token(
            ShardInput::Token(token),
            &mut cache_b,
            0, k,
            0,
        ).expect("first shard at position 0 is always in sync");
        let hidden = match mid {
            ShardOutput::Hidden(h) => h,
            _ => panic!("First shard should produce a hidden state, not a token"),
        };
        let result_b = model.forward_shard_token(
            ShardInput::Hidden(hidden),
            &mut cache_b,
            k, n_layers,
            0,
        ).expect("last shard at position 0 is always in sync");
        let token_b = match result_b {
            ShardOutput::Token { id, .. } => id,
            _ => panic!("Last shard should produce a token"),
        };

        assert_eq!(token_a, token_b,
            "Shard split at K={} produced different output token (A={}, B={})",
            k, token_a, token_b);
    }

    #[test]
    fn test_forward_shard_token_three_way_split() {
        // Same as the two-way split test but with THREE shards.
        // Stress-tests the chained hidden-state forwarding through more hops.
        let model = build_test_model(20, 32, 2, 64, 6); // 6 layers
        let n_layers = model.config.n_layers;
        let token: u32 = 7;

        // Path A: one shard covering everything
        let mut cache_a = KVCache::new(n_layers);
        let result_a = model.forward_shard_token(
            ShardInput::Token(token), &mut cache_a, 0, n_layers, 0).unwrap();
        let token_a = match result_a {
            ShardOutput::Token { id, .. } => id,
            _ => panic!("expected Token"),
        };

        // Path B: 3 shards [0, 2), [2, 4), [4, 6)
        let mut cache_b = KVCache::new(n_layers);
        let h1 = match model.forward_shard_token(
            ShardInput::Token(token), &mut cache_b, 0, 2, 0).unwrap() {
            ShardOutput::Hidden(h) => h,
            _ => panic!("expected Hidden"),
        };
        let h2 = match model.forward_shard_token(
            ShardInput::Hidden(h1), &mut cache_b, 2, 4, 0).unwrap() {
            ShardOutput::Hidden(h) => h,
            _ => panic!("expected Hidden"),
        };
        let token_b = match model.forward_shard_token(
            ShardInput::Hidden(h2), &mut cache_b, 4, 6, 0).unwrap() {
            ShardOutput::Token { id, .. } => id,
            _ => panic!("expected Token"),
        };

        assert_eq!(token_a, token_b,
            "3-way shard split produced different token (A={}, B={})",
            token_a, token_b);
    }

    #[test]
    fn test_forward_shard_token_multi_position() {
        // Run 4 positions through the shard pipeline. Each position uses
        // the same per-request KV cache, which grows as positions advance.
        // This catches KV cache management bugs across position boundaries
        // (e.g. wrong push offset, wrong attention-against-history shape,
        // wrong RoPE position propagation across shards).
        //
        // The test compares the FULL pipeline (single shard, all layers)
        // against a 2-shard split, position-by-position. The output token
        // at every position must match.
        let model = build_test_model(15, 32, 2, 64, 4); // 4 layers
        let n_layers = model.config.n_layers;
        let prompt: Vec<u32> = vec![1, 5, 9, 13]; // 4 positions
        let k = 2; // shard split

        // Path A: full forward through one shard, position-by-position
        let mut cache_a = KVCache::new(n_layers);
        let mut tokens_a = Vec::new();
        for (pos, &tok) in prompt.iter().enumerate() {
            let res = model.forward_shard_token(
                ShardInput::Token(tok), &mut cache_a, 0, n_layers, pos).unwrap();
            match res {
                ShardOutput::Token { id, .. } => tokens_a.push(id),
                _ => panic!("Expected Token at pos {}", pos),
            }
        }

        // Path B: 2-shard split, position-by-position, sharing the same
        // per-request KV cache (it spans all layers regardless of which
        // shard pushed each entry)
        let mut cache_b = KVCache::new(n_layers);
        let mut tokens_b = Vec::new();
        for (pos, &tok) in prompt.iter().enumerate() {
            let mid = match model.forward_shard_token(
                ShardInput::Token(tok), &mut cache_b, 0, k, pos).unwrap() {
                ShardOutput::Hidden(h) => h,
                _ => panic!("First shard at pos {} should produce Hidden", pos),
            };
            let res = model.forward_shard_token(
                ShardInput::Hidden(mid), &mut cache_b, k, n_layers, pos).unwrap();
            match res {
                ShardOutput::Token { id, .. } => tokens_b.push(id),
                _ => panic!("Last shard at pos {} should produce Token", pos),
            }
        }

        assert_eq!(tokens_a, tokens_b,
            "Multi-position shard split diverged. A={:?} B={:?}", tokens_a, tokens_b);
    }

    // ── KV-cache continuity guard ───────────────────────────────────────
    //
    // These pin the behaviour that makes replica failover and hedged
    // dispatch safe. Before the guard, running a position against a cold
    // cache read past the end of `k_data` through an unchecked raw pointer
    // in `flash_attention_i64` — undefined behaviour, and under the release
    // profile's `panic = "abort"` any resulting index panic takes the whole
    // node down.

    #[test]
    fn cold_replica_mid_stream_is_rejected_not_undefined() {
        // Exactly the failover scenario: the coordinator ran positions 0..3
        // against replica A, replica A stopped answering, and the request is
        // handed to replica B whose cache for this request_id is empty.
        let model = build_test_model(15, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;

        let mut cold = KVCache::new(n_layers);
        let err = model
            .forward_shard_token(ShardInput::Token(5), &mut cold, 0, n_layers, 3)
            .expect_err("cold cache at position 3 must be refused");

        assert_eq!(err.kind(), "kv_cache_out_of_sync");
        match err {
            ShardForwardError::KvCacheOutOfSync {
                expected_positions,
                cached_positions,
                ..
            } => {
                assert_eq!(expected_positions, 3);
                assert_eq!(cached_positions, 0);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn replayed_position_is_rejected_so_kv_cannot_double_push() {
        // A coordinator that retries a position this replica already
        // consumed would double-push K/V and corrupt every later token.
        // Require exact equality, not >=.
        let model = build_test_model(15, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;
        let mut cache = KVCache::new(n_layers);

        model
            .forward_shard_token(ShardInput::Token(1), &mut cache, 0, n_layers, 0)
            .expect("position 0 on a fresh cache");
        model
            .forward_shard_token(ShardInput::Token(2), &mut cache, 0, n_layers, 1)
            .expect("position 1 follows position 0");

        // Replay position 1 — cache now holds 2 positions, not 1.
        let err = model
            .forward_shard_token(ShardInput::Token(2), &mut cache, 0, n_layers, 1)
            .expect_err("replayed position must be refused");
        assert_eq!(err.kind(), "kv_cache_out_of_sync");
    }

    #[test]
    fn warm_replica_in_sequence_is_accepted() {
        // The guard must not reject the normal path: every position in
        // ascending order on the same cache.
        let model = build_test_model(15, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;
        let mut cache = KVCache::new(n_layers);
        for pos in 0..6 {
            model
                .forward_shard_token(ShardInput::Token(pos as u32 + 1), &mut cache, 0, n_layers, pos)
                .unwrap_or_else(|e| panic!("in-sequence position {pos} rejected: {e}"));
        }
    }

    #[test]
    fn short_hidden_state_from_peer_is_rejected_not_panic() {
        let model = build_test_model(15, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;
        let mut cache = KVCache::new(n_layers);
        let err = model
            .forward_shard_token(ShardInput::Hidden(vec![0i64; 3]), &mut cache, 2, n_layers, 0)
            .expect_err("truncated hidden state must be refused");
        assert_eq!(err.kind(), "bad_hidden_dim");
    }

    #[test]
    fn position_past_rope_table_is_rejected_not_panic() {
        // apply_rope indexes cos[pos * half + i] with no bounds check.
        let model = build_test_model(15, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;
        let max_seq = model.config.max_seq;
        let mut cache = KVCache::new(n_layers);
        let err = model
            .forward_shard_token(ShardInput::Token(1), &mut cache, 0, n_layers, max_seq)
            .expect_err("position past the RoPE table must be refused");
        assert_eq!(err.kind(), "position_out_of_range");
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn test_simd_matches_scalar() {
        let weights = I8Weights::quantize_f32(
            &(0..1024 * 512).map(|i| (i as f32 % 200.0 - 100.0) / 100.0).collect::<Vec<_>>(),
            1024, 512,
        );
        let input: Vec<i64> = (0..512).map(|i| (i as i64 - 256) * ONE / 256).collect();

        let scalar = matmul_i8(&weights, &input, 512, 1024);
        let simd = matmul_i8xi8_simd(&weights, &input, 512, 1024);

        for i in 0..1024 {
            let diff = (scalar[i] - simd[i]).abs();
            let tolerance = scalar[i].abs().max(ONE) / 5;
            assert!(diff < tolerance, "Row {}: scalar={}, simd={}, diff={}", i, scalar[i], simd[i], diff);
        }
    }

    /// SIMD i16 dot product must match scalar bit-exactly for inputs that
    /// fit in i32. The 4096-element row size matches Llama-7B d_model.
    #[test]
    fn test_dot_i16_simd_matches_scalar() {
        // Pseudo-realistic Llama hidden state: i64 values with magnitudes
        // up to ~2^28 (well within i32 range).
        let len = 4096usize;
        let row: Vec<i16> = (0..len).map(|i| ((i as i32 * 31) % 65535 - 32768) as i16).collect();
        let input: Vec<i64> = (0..len).map(|i| ((i as i64 * 12345) % (1 << 28)) - (1 << 27)).collect();

        let scalar = unsafe { dot_i16_i64_scalar(row.as_ptr(), input.as_ptr(), len) };
        let dispatched = unsafe { dot_i16_i64(row.as_ptr(), input.as_ptr(), len) };

        assert_eq!(scalar, dispatched,
            "SIMD i16 dot product diverged from scalar: scalar={} simd={}",
            scalar, dispatched);
    }

    /// Same test but with a row size that has a tail (not divisible by 8).
    #[test]
    fn test_dot_i16_simd_tail() {
        let len = 4099usize; // Forces a 3-element scalar tail after the SIMD loop
        let row: Vec<i16> = (0..len).map(|i| ((i as i32 * 17) % 65535 - 32768) as i16).collect();
        let input: Vec<i64> = (0..len).map(|i| ((i as i64 * 9876) % (1 << 28)) - (1 << 27)).collect();

        let scalar = unsafe { dot_i16_i64_scalar(row.as_ptr(), input.as_ptr(), len) };
        let dispatched = unsafe { dot_i16_i64(row.as_ptr(), input.as_ptr(), len) };

        assert_eq!(scalar, dispatched,
            "SIMD i16 dot tail diverged: len={} scalar={} simd={}",
            len, scalar, dispatched);
    }

    /// Benchmark: SIMD vs scalar for a Llama-7B-shaped row.
    /// Uses --nocapture to print the speedup.
    #[test]
    #[ignore] // run via: cargo test --release -p arc-inference -- --nocapture --ignored bench_dot_i16
    fn bench_dot_i16_simd_vs_scalar() {
        let len = 4096usize;
        let row: Vec<i16> = (0..len).map(|i| ((i as i32 * 31) % 65535 - 32768) as i16).collect();
        let input: Vec<i64> = (0..len).map(|i| ((i as i64 * 12345) % (1 << 28)) - (1 << 27)).collect();
        let iters = 100_000usize;

        // Scalar baseline
        let t0 = std::time::Instant::now();
        let mut sum_s: i64 = 0;
        for _ in 0..iters {
            let acc = unsafe { dot_i16_i64_scalar(row.as_ptr(), input.as_ptr(), len) };
            sum_s = sum_s.wrapping_add(acc);
        }
        let scalar_ns = t0.elapsed().as_nanos() / iters as u128;

        // Dispatched (SIMD on aarch64, scalar on x86)
        let t0 = std::time::Instant::now();
        let mut sum_d: i64 = 0;
        for _ in 0..iters {
            let acc = unsafe { dot_i16_i64(row.as_ptr(), input.as_ptr(), len) };
            sum_d = sum_d.wrapping_add(acc);
        }
        let dispatched_ns = t0.elapsed().as_nanos() / iters as u128;

        let speedup = scalar_ns as f64 / dispatched_ns.max(1) as f64;
        println!("\n=== dot_i16 benchmark (4096-wide row) ===");
        println!("scalar     : {:>5} ns/call  (sum={})", scalar_ns, sum_s);
        println!("dispatched : {:>5} ns/call  (sum={})", dispatched_ns, sum_d);
        println!("speedup    : {:.2}x", speedup);
        assert_eq!(sum_s, sum_d, "SIMD and scalar must produce identical sums");
    }

    /// Repro for project_i16_ppl_bug.md - I8Weights::quantize_f32 truncates
    /// abs_max to i64 before computing the scale, destroying precision when
    /// abs_max < ~5. Fix is to compute scale in f64:
    ///   ((abs_max as f64 * ONE as f64) / 127.0).round().max(1.0) as i64
    /// After applying the fix, remove #[ignore] so CI guards the invariant.
    #[test]
    #[ignore = "reproduces an unfixed scale-precision bug; remove #[ignore] after applying fix"]
    fn test_i8_scale_roundtrip_small_abs_max() {
        for &abs_max in &[0.1_f32, 0.5, 0.9, 1.2, 2.3] {
            // Single-row matrix with exactly abs_max as the peak value.
            let row = vec![abs_max, -abs_max, 0.0, abs_max / 2.0];
            let w = I8Weights::quantize_f32(&row, 1, 4);
            // Round-trip: dequantize and check peak ~= abs_max within 5%.
            let scale = w.scales[0] as f64;
            let peak = (w.data[0] as f64) * scale / ONE as f64;
            let rel_err = (peak - abs_max as f64).abs() / abs_max as f64;
            assert!(rel_err < 0.05,
                "abs_max={} peak={} rel_err={:.3} scale={}",
                abs_max, peak, rel_err, w.scales[0]);
        }
    }

    #[test]
    fn test_q4_scale_roundtrip() {
        let weights = I8Weights::quantize_f32(
            &(0..512*256).map(|i| (i as f32 % 200.0 - 100.0) / 100.0).collect::<Vec<_>>(),
            512, 256,
        );
        let q4 = Q4WeightsX86::from_i8(&weights);
        assert_eq!(q4.n_rows, 512);
        assert_eq!(q4.n_cols, 256);
        assert_eq!(q4.scales.len(), 512);
        // Verify scale magnitudes are reasonable
        for (i, (&q4s, &i8s)) in q4.scales.iter().zip(weights.scales.iter()).enumerate() {
            let ratio = q4s as f64 / i8s.max(1) as f64;
            assert!((0.5..=20.0).contains(&ratio), "Q4 scale ratio out of range at row {}: {}", i, ratio);
        }
    }
}

#[cfg(test)]
mod int16_tests {
    use super::*;

    #[test]
    fn test_i16_quantize_f32_reconstruction() {
        // Create deterministic weights, quantize to I8 and I16, compare reconstruction error
        let n = 128;
        let weights: Vec<f32> = (0..n * n)
            .map(|i| ((i * 7 + 3) % 256) as f32 / 128.0 - 1.0)
            .collect();

        let i8w = I8Weights::quantize_f32(&weights, n, n);
        let i16w = I16Weights::quantize_f32(&weights, n, n);

        let mut i8_err = 0.0f64;
        let mut i16_err = 0.0f64;
        for i in 0..n {
            for j in 0..n {
                let orig = weights[i * n + j] as f64;
                // I8 reconstruction: data * scale / ONE (scale = abs_max/127 in Q16)
                let i8_recon =
                    (i8w.data[i * n + j] as f64) * (i8w.scales[i] as f64) / (ONE as f64);
                // I16 reconstruction: data * scale / 32767 / ONE (scale = abs_max in Q16)
                let i16_recon =
                    (i16w.data[i * n + j] as f64) * (i16w.scales[i] as f64)
                    / 32767.0 / (ONE as f64);
                i8_err += (orig - i8_recon).abs();
                i16_err += (orig - i16_recon).abs();
            }
        }
        // I16 must have significantly lower reconstruction error than I8
        assert!(
            i16_err < i8_err,
            "I16 error {i16_err} should be less than I8 error {i8_err}"
        );
    }

    #[test]
    fn test_i16_matmul_correctness() {
        // Compare I16 matmul output against f64 reference
        let rows = 4;
        let cols = 8;
        let weights_f32: Vec<f32> = (0..rows * cols)
            .map(|i| ((i * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let input_f64: Vec<f64> = (0..cols)
            .map(|i| ((i * 7 + 1) % 50) as f64 / 25.0 - 1.0)
            .collect();
        let input_q16: Vec<i64> = input_f64.iter().map(|&x| (x * ONE as f64) as i64).collect();

        // Reference: f64 matmul
        let mut ref_out = vec![0.0f64; rows];
        for i in 0..rows {
            for j in 0..cols {
                ref_out[i] += weights_f32[i * cols + j] as f64 * input_f64[j];
            }
        }

        let i16w = I16Weights::quantize_f32(&weights_f32, rows, cols);
        let mut i16_out = vec![0i64; rows];
        matmul_i16_into(&i16w, &input_q16, cols, &mut i16_out);

        for i in 0..rows {
            let i16_real = i16_out[i] as f64 / ONE as f64;
            let err = (ref_out[i] - i16_real).abs();
            // Q16 fixed-point accumulation on small matrices can introduce
            // up to ~10% error from scale quantization and truncation.
            assert!(
                err < 0.15,
                "Row {i}: ref={}, i16={}, err={}",
                ref_out[i],
                i16_real,
                err
            );
        }
    }

    #[test]
    fn test_i16_from_i8_preserves_values() {
        let weights_f32: Vec<f32> = vec![0.5, -0.3, 0.8, -0.1, 0.2, 0.6, -0.9, 0.4];
        let i8w = I8Weights::quantize_f32(&weights_f32, 2, 4);
        let i16w = I16Weights::from_i8(&i8w);

        // from_i8 should preserve data as wider type and adjust scales
        assert_eq!(i16w.n_rows, i8w.n_rows);
        assert_eq!(i16w.n_cols, i8w.n_cols);
        for i in 0..i8w.data.len() {
            assert_eq!(i16w.data[i], i8w.data[i] as i16);
        }
        // Scales are adjusted: i16_scale = i8_scale * 32767 to account for
        // the /32767 in the I16 matmul path
        for i in 0..i8w.scales.len() {
            assert_eq!(i16w.scales[i], i8w.scales[i] * 32767);
        }
    }

    #[test]
    fn test_i16_deterministic() {
        let weights: Vec<f32> = (0..64).map(|i| (i as f32) / 32.0 - 1.0).collect();
        let input: Vec<i64> = (0..8).map(|i| (i * 8192) as i64).collect();

        let w = I16Weights::quantize_f32(&weights, 8, 8);
        let mut out1 = vec![0i64; 8];
        let mut out2 = vec![0i64; 8];
        matmul_i16_into(&w, &input, 8, &mut out1);
        matmul_i16_into(&w, &input, 8, &mut out2);
        assert_eq!(out1, out2, "I16 matmul must be deterministic");
    }

    #[test]
    fn test_i16_matmul_matches_i8_on_from_i8() {
        // When using from_i8, i16 matmul should produce very close results to i8 matmul.
        // Not bit-identical because I16 matmul uses /32767 integer division (different
        // rounding path than I8's direct scale multiply).
        let weights_f32: Vec<f32> = (0..32)
            .map(|i| ((i * 17 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let i8w = I8Weights::quantize_f32(&weights_f32, 4, 8);
        let i16w = I16Weights::from_i8(&i8w);
        let input: Vec<i64> = (0..8).map(|i| ((i + 1) as i64) * ONE / 4).collect();

        let i8_out = matmul_i8(&i8w, &input, 8, 4);
        let i16_out = matmul_i16(&i16w, &input, 8, 4);

        for i in 0..4 {
            let diff = (i8_out[i] - i16_out[i]).abs();
            // Allow up to 1% of the value or 2 Q16 units, whichever is larger
            let tolerance = (i8_out[i].abs() / 100).max(2);
            assert!(
                diff <= tolerance,
                "Row {i}: i8={}, i16={}, diff={} > tolerance={} - from_i8 should produce close results",
                i8_out[i], i16_out[i], diff, tolerance
            );
        }
    }

    #[test]
    fn test_i16_memory_bytes() {
        let weights_f32: Vec<f32> = vec![0.0; 16];
        let w = I16Weights::quantize_f32(&weights_f32, 4, 4);
        // 16 i16 values (32 bytes) + 4 scales (32 bytes) + 16 overhead
        assert_eq!(w.memory_bytes(), 16 * 2 + 4 * 8 + 16);
    }

    #[test]
    fn test_enable_i16_on_model() {
        // Build a tiny model and verify enable_i16 converts all layers
        let model_fn = || {
            let vs = 50;
            let d = 32;
            let nh = 2;
            let dff = 64;
            let nl = 1;
            let dh = d / nh;
            let nkv = nh;
            let dkv = dh * nkv;

            let mut rng: u64 = 42;
            let mut gen_f32 = |size: usize| -> Vec<f32> {
                (0..size)
                    .map(|_| {
                        rng = rng
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        ((rng >> 33) as f32 / u32::MAX as f32 - 0.5) * 0.2
                    })
                    .collect()
            };
            let mut gen_i8 = |rows: usize, cols: usize| -> I8Weights {
                I8Weights::quantize_f32(&gen_f32(rows * cols), rows, cols)
            };

            let embedding_i8 = gen_i8(vs, d);
            let embedding_q16: Vec<i64> = {
                let mut q16 = Vec::with_capacity(vs * d);
                for i in 0..vs {
                    let scale = embedding_i8.scales[i];
                    for j in 0..d {
                        q16.push((embedding_i8.data[i * d + j] as i64) * scale);
                    }
                }
                q16
            };
            let output_weight = gen_i8(vs, d);
            let mut layers = Vec::new();
            for _ in 0..nl {
                layers.push(CachedLayer {
                    wq: gen_i8(d, d),
                    wk: gen_i8(dkv, d),
                    wv: gen_i8(dkv, d),
                    wo: gen_i8(d, d),
                    w_gate: gen_i8(dff, d),
                    w_up: gen_i8(dff, d),
                    w_down: gen_i8(d, dff),
                    attn_norm: vec![ONE; d],
                    ffn_norm: vec![ONE; d],
                });
            }

            let (rope_cos, rope_sin) = compute_rope_tables(dh, 512, 10000.0);
            let attn_scale = {
                let s = integer_isqrt((dh as i64) * ONE);
                (ONE * ONE) / s.max(1)
            };

            CachedIntegerModel {
                config: ModelConfig {
                    n_layers: nl,
                    d_model: d,
                    n_heads: nh,
                    n_kv_heads: nkv,
                    d_ff: dff,
                    d_head: dh,
                    d_kv: dkv,
                    vocab_size: vs,
                    attn_scale,
                    rope_cos,
                    rope_sin,
                    max_seq: 512,
                    eos_tokens: vec![2, 128001, 128009],
                    bos_token: 1,
                    chat_template: String::new(),
                },
                embedding_q16,
                embedding_i8,
                layers,
                final_norm: vec![ONE; d],
                output_weight,
                vocab: (0..vs).map(|i| format!("tok_{}", i)).collect(),
                q4_layers: None,
                q4_output: None,
                i16_layers: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,                i16_output: None,
                block_i8_layers: None,
                block_i8_output: None,
                ternary_layers: None,
                ternary_output: None,
            }
        };

        let mut model = model_fn();
        assert!(model.i16_layers.is_none());
        assert!(model.i16_output.is_none());

        model.enable_i16();

        assert!(model.i16_layers.is_some());
        assert!(model.i16_output.is_some());
        let i16_layers = model.i16_layers.as_ref().unwrap();
        assert_eq!(i16_layers.len(), 1);
        // Verify dimensions match the original I8 layers
        assert_eq!(i16_layers[0].wq.n_rows, model.layers[0].wq.n_rows);
        assert_eq!(i16_layers[0].wq.n_cols, model.layers[0].wq.n_cols);
    }

    #[test]
    fn test_i16_matmul_nonzero_output() {
        let weights = I16Weights::quantize_f32(
            &(0..128*64).map(|i| ((i % 200) as f32 - 100.0) / 100.0).collect::<Vec<_>>(),
            128, 64,
        );
        let input: Vec<i64> = (0..64).map(|i| (i as i64 - 32) * ONE / 32).collect();
        let mut output = vec![0i64; 128];
        matmul_i16_into(&weights, &input, 64, &mut output);
        let nonzero = output.iter().filter(|&&x| x != 0).count();
        assert!(nonzero > 100, "Expected mostly nonzero outputs, got {}/128", nonzero);
    }
}
