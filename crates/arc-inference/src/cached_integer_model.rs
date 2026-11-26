use crate::integer_lut::*;
use arc_crypto::Hash256;
use rayon::prelude::*;
use tracing::info;

pub struct I8Weights {
    pub data: Vec<i8>,
    pub scales: Vec<i64>,
    pub n_rows: usize,
    pub n_cols: usize,
}

impl I8Weights {
    pub fn quantize_f32(values: &[f32], n_rows: usize, n_cols: usize) -> Self {
        assert_eq!(values.len(), n_rows * n_cols);

        let mut data = Vec::with_capacity(n_rows * n_cols);
        let mut scales = Vec::with_capacity(n_rows);

        for i in 0..n_rows {
            let row = &values[i * n_cols..(i + 1) * n_cols];

            let abs_max = row.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
            let abs_max = abs_max.max(1e-10);

            let inv_abs_max = 127.0 / abs_max;
            for &x in row {
                data.push((x * inv_abs_max).round().clamp(-127.0, 127.0) as i8);
            }

            let scale = ((abs_max as f64 * ONE as f64) / 127.0).round().max(1.0) as i64;
            scales.push(scale);
        }

        Self {
            data,
            scales,
            n_rows,
            n_cols,
        }
    }

    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 8 + 16
    }

    pub fn empty() -> Self {
        Self {
            data: Vec::new(),
            scales: Vec::new(),
            n_rows: 0,
            n_cols: 0,
        }
    }

    pub fn copy_rows(&self, start: usize, end: usize) -> Result<Self, String> {
        if start >= end
            || end > self.n_rows
            || self.n_cols == 0
            || self.data.len() != self.n_rows.saturating_mul(self.n_cols)
            || self.scales.len() != self.n_rows
        {
            return Err(format!(
                "invalid I8 row range [{start}, {end}) for {}x{} matrix",
                self.n_rows, self.n_cols
            ));
        }
        Ok(Self {
            data: self.data[start * self.n_cols..end * self.n_cols].to_vec(),
            scales: self.scales[start..end].to_vec(),
            n_rows: end - start,
            n_cols: self.n_cols,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct I8WeightsView<'a> {
    pub(crate) data: &'a [i8],
    pub(crate) scales: &'a [i64],
    pub(crate) n_rows: usize,
    pub(crate) n_cols: usize,
}

impl<'a> From<&'a I8Weights> for I8WeightsView<'a> {
    fn from(weights: &'a I8Weights) -> Self {
        Self {
            data: &weights.data,
            scales: &weights.scales,
            n_rows: weights.n_rows,
            n_cols: weights.n_cols,
        }
    }
}

pub struct I16Weights {
    pub data: Vec<i16>,
    pub scales: Vec<i64>,
    pub n_rows: usize,
    pub n_cols: usize,
}

impl I16Weights {
    pub fn quantize_f32(values: &[f32], n_rows: usize, n_cols: usize) -> Self {
        assert_eq!(values.len(), n_rows * n_cols);

        let mut data = Vec::with_capacity(n_rows * n_cols);
        let mut scales = Vec::with_capacity(n_rows);

        for i in 0..n_rows {
            let row = &values[i * n_cols..(i + 1) * n_cols];

            let abs_max = row.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
            let abs_max = abs_max.max(1e-10);

            let inv_abs_max = 32767.0 / abs_max;
            for &x in row {
                data.push((x * inv_abs_max).round().clamp(-32767.0, 32767.0) as i16);
            }

            let scale = (abs_max as f64 * ONE as f64).round() as i64;
            scales.push(scale.max(1));
        }

        Self {
            data,
            scales,
            n_rows,
            n_cols,
        }
    }

    pub fn empty() -> Self {
        Self {
            data: Vec::new(),
            scales: Vec::new(),
            n_rows: 0,
            n_cols: 0,
        }
    }

    pub fn from_i8(w: &I8Weights) -> Self {
        let data: Vec<i16> = w.data.iter().map(|&v| v as i16).collect();
        let scales: Vec<i64> = w.scales.iter().map(|&s| s * 32767).collect();
        Self {
            data,
            scales,
            n_rows: w.n_rows,
            n_cols: w.n_cols,
        }
    }

    pub fn memory_bytes(&self) -> usize {
        self.data.len() * 2 + self.scales.len() * 8 + 16
    }
}

pub struct CachedLayer {
    pub wq: I8Weights,
    pub wk: I8Weights,
    pub wv: I8Weights,
    pub wo: I8Weights,
    pub w_gate: I8Weights,
    pub w_up: I8Weights,
    pub w_down: I8Weights,
    pub attn_norm: Vec<i64>,
    pub ffn_norm: Vec<i64>,
}

impl CachedLayer {
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

    pub fn is_loaded(&self) -> bool {
        let matrix_is_complete = |weights: &I8Weights| {
            weights.n_rows > 0
                && weights.n_cols > 0
                && weights.data.len() == weights.n_rows.saturating_mul(weights.n_cols)
                && weights.scales.len() == weights.n_rows
        };
        [
            &self.wq,
            &self.wk,
            &self.wv,
            &self.wo,
            &self.w_gate,
            &self.w_up,
            &self.w_down,
        ]
        .into_iter()
        .all(matrix_is_complete)
            && !self.attn_norm.is_empty()
            && !self.ffn_norm.is_empty()
    }
}

pub struct KVCache {
    pub k_data: Vec<Vec<i64>>,
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

#[allow(dead_code)]
#[inline]
fn quantize_vec_i8(v: &[i64]) -> (Vec<i8>, i64) {
    let abs_max = v.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let scale_factor = (abs_max / 127).max(1);
    let data: Vec<i8> = v
        .iter()
        .map(|&x| (x / scale_factor).clamp(-127, 127) as i8)
        .collect();
    (data, scale_factor)
}

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
    pub eos_tokens: Vec<u32>,
    pub bos_token: u32,
    pub chat_template: String,
    pub arithmetic_profile: ArithmeticProfile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithmeticProfile {
    LegacySplitHalfV0,
    GgufInterleavedRowsV1,
}

pub struct Q4Layer {
    pub wq: Q4WeightsX86,
    pub wk: Q4WeightsX86,
    pub wv: Q4WeightsX86,
    pub wo: Q4WeightsX86,
    pub w_gate: Q4WeightsX86,
    pub w_up: Q4WeightsX86,
    pub w_down: Q4WeightsX86,
}

pub struct I16Layer {
    pub wq: I16Weights,
    pub wk: I16Weights,
    pub wv: I16Weights,
    pub wo: I16Weights,
    pub w_gate: I16Weights,
    pub w_up: I16Weights,
    pub w_down: I16Weights,
}

pub struct TernaryLayer {
    pub wq: crate::ternary_engine::TernaryWeights,
    pub wk: crate::ternary_engine::TernaryWeights,
    pub wv: crate::ternary_engine::TernaryWeights,
    pub wo: crate::ternary_engine::TernaryWeights,
    pub w_gate: crate::ternary_engine::TernaryWeights,
    pub w_up: crate::ternary_engine::TernaryWeights,
    pub w_down: crate::ternary_engine::TernaryWeights,
}

pub struct TernaryHybridLayer {
    pub wq: crate::ternary_hybrid::TernaryHybridWeights,
    pub wk: crate::ternary_hybrid::TernaryHybridWeights,
    pub wv: crate::ternary_hybrid::TernaryHybridWeights,
    pub wo: crate::ternary_hybrid::TernaryHybridWeights,
    pub w_gate: crate::ternary_hybrid::TernaryHybridWeights,
    pub w_up: crate::ternary_hybrid::TernaryHybridWeights,
    pub w_down: crate::ternary_hybrid::TernaryHybridWeights,
}

pub struct BlockI8Layer {
    pub wq: crate::block_i8::BlockI8Weights,
    pub wk: crate::block_i8::BlockI8Weights,
    pub wv: crate::block_i8::BlockI8Weights,
    pub wo: crate::block_i8::BlockI8Weights,
    pub w_gate: crate::block_i8::BlockI8Weights,
    pub w_up: crate::block_i8::BlockI8Weights,
    pub w_down: crate::block_i8::BlockI8Weights,
}

pub struct CachedIntegerModel {
    pub config: ModelConfig,
    pub embedding_q16: Vec<i64>,
    pub embedding_i8: I8Weights,
    pub layers: Vec<CachedLayer>,
    pub final_norm: Vec<i64>,
    pub output_weight: I8Weights,
    pub vocab: Vec<String>,
    pub q4_layers: Option<Vec<Q4Layer>>,
    pub q4_output: Option<Q4WeightsX86>,
    pub i16_layers: Option<Vec<I16Layer>>,
    pub i16_output: Option<I16Weights>,
    pub block_i8_layers: Option<Vec<BlockI8Layer>>,
    pub block_i8_output: Option<crate::block_i8::BlockI8Weights>,
    pub ternary_layers: Option<Vec<TernaryLayer>>,
    pub ternary_output: Option<crate::ternary_engine::TernaryWeights>,
    pub ternary_hybrid_layers: Option<Vec<TernaryHybridLayer>>,
    pub ternary_hybrid_output: Option<crate::ternary_hybrid::TernaryHybridWeights>,
}

pub const CANONICAL_REWARD_INFERENCE_PROFILE: &str =
    arc_types::transaction::CANONICAL_REWARD_INFERENCE_PROFILE;
pub const GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE: &str =
    arc_types::transaction::GGUF_LLAMA_I8_INTERLEAVED_ROPE_PROFILE_V1;
pub const GGUF_LLAMA_GREEDY_GENERATION_SEMANTICS_V1: &str =
    arc_types::transaction::GGUF_LLAMA_GREEDY_GENERATION_SEMANTICS_V1;
pub const I16_INFERENCE_PROFILE: &str = "INT16 integer (per-row, cross-platform deterministic)";
pub const BLOCK_I8_INFERENCE_PROFILE: &str =
    "block-INT8 integer (32-weight blocks, cross-platform deterministic)";
pub const Q4_INFERENCE_PROFILE: &str = "Q4 integer (cross-platform deterministic)";
pub const TERNARY_INFERENCE_PROFILE: &str = "ternary (1.58-bit, ASIC-compatible)";
pub const TERNARY_HYBRID_INFERENCE_PROFILE: &str = "ternary-hybrid (per-row sparse-INT8 outliers)";

pub fn is_supported_inference_profile(profile: &str) -> bool {
    matches!(
        profile,
        CANONICAL_REWARD_INFERENCE_PROFILE
            | GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE
            | I16_INFERENCE_PROFILE
            | BLOCK_I8_INFERENCE_PROFILE
            | Q4_INFERENCE_PROFILE
            | TERNARY_INFERENCE_PROFILE
            | TERNARY_HYBRID_INFERENCE_PROFILE
    )
}

pub const GENERATION_INTERNAL_BOS_POSITIONS: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a successful preflight must gate untrusted generation"]
pub struct GenerationPreflight {
    pub prompt_tokens: usize,
    pub max_tokens: u32,
    pub required_positions: usize,
    pub max_seq: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationError {
    PositionCountOverflow {
        prompt_tokens: usize,
        max_tokens: u32,
    },
    ContextWindowExceeded {
        prompt_tokens: usize,
        max_tokens: u32,
        required_positions: usize,
        max_seq: usize,
    },
}

impl GenerationError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::PositionCountOverflow { .. } => "position_count_overflow",
            Self::ContextWindowExceeded { .. } => "context_window_exceeded",
        }
    }
}

impl std::fmt::Display for GenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PositionCountOverflow {
                prompt_tokens,
                max_tokens,
            } => write!(
                f,
                "position_count_overflow: 1 internal BOS + {prompt_tokens} prompt tokens + \
                 {max_tokens} generated tokens exceeds the platform position counter"
            ),
            Self::ContextWindowExceeded {
                prompt_tokens,
                max_tokens,
                required_positions,
                max_seq,
            } => write!(
                f,
                "context_window_exceeded: 1 internal BOS + {prompt_tokens} prompt tokens + \
                 {max_tokens} generated tokens requires {required_positions} positions, but \
                 the model supports at most {max_seq}"
            ),
        }
    }
}

impl std::error::Error for GenerationError {}

#[derive(Debug, PartialEq, Eq)]
pub enum BackendGenerationError {
    Generation(GenerationError),
    Projection(crate::tensor_parallel::TensorParallelError),
}

impl std::fmt::Display for BackendGenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Generation(error) => write!(f, "{error}"),
            Self::Projection(error) => write!(f, "row projection failed: {error}"),
        }
    }
}

impl std::error::Error for BackendGenerationError {}

impl From<GenerationError> for BackendGenerationError {
    fn from(error: GenerationError) -> Self {
        Self::Generation(error)
    }
}

impl From<crate::tensor_parallel::TensorParallelError> for BackendGenerationError {
    fn from(error: crate::tensor_parallel::TensorParallelError) -> Self {
        Self::Projection(error)
    }
}

fn backend_call_id(request: &Hash256, position: usize) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-backend-generation-call-v1");
    hasher.update(&request.0);
    hasher.update(&(position as u64).to_le_bytes());
    Hash256(*hasher.finalize().as_bytes())
}

impl CachedIntegerModel {
    pub fn arithmetic_profile(&self) -> &'static str {
        match self.config.arithmetic_profile {
            ArithmeticProfile::LegacySplitHalfV0 => CANONICAL_REWARD_INFERENCE_PROFILE,
            ArithmeticProfile::GgufInterleavedRowsV1 => GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE,
        }
    }

    pub fn canonical_execution_profile(&self) -> Option<&'static str> {
        self.has_canonical_i8_profile()
            .then(|| self.arithmetic_profile())
    }

    pub fn canonicalize_gguf_interleaved_rope_rows(&mut self) -> Result<(), crate::InferenceError> {
        if self.config.arithmetic_profile != ArithmeticProfile::LegacySplitHalfV0 {
            return Err(crate::InferenceError::Runtime(
                "GGUF interleaved-RoPE canonicalization was requested twice".into(),
            ));
        }
        if !self.has_canonical_i8_profile() {
            return Err(crate::InferenceError::Runtime(
                "GGUF interleaved-RoPE canonicalization requires the canonical per-row INT8 profile".into(),
            ));
        }
        for (layer_index, layer) in self.layers.iter_mut().enumerate() {
            if !layer.is_loaded() {
                return Err(crate::InferenceError::Runtime(format!(
                    "GGUF interleaved-RoPE canonicalization requires complete layers; layer {layer_index} is absent"
                )));
            }
            permute_interleaved_rows_to_split_half(
                &mut layer.wq,
                self.config.n_heads,
                self.config.d_head,
                "wq",
            )?;
            permute_interleaved_rows_to_split_half(
                &mut layer.wk,
                self.config.n_kv_heads,
                self.config.d_head,
                "wk",
            )?;
        }
        self.config.arithmetic_profile = ArithmeticProfile::GgufInterleavedRowsV1;
        Ok(())
    }

    pub const LLAMA2_CHAT_SYSTEM_PROMPT: &'static str = "You are a helpful, respectful and honest assistant. Always answer as helpfully as possible, while being safe. Your answers should not contain harmful, unethical, racist, sexist, toxic, dangerous, or illegal content. Please ensure your responses are socially unbiased and positive in nature. If a question does not make any sense, or is not factually coherent, explain why instead of answering something not correct. If you don't know the answer to a question, please don't share false information.";

    pub fn apply_llama2_chat_template(user_input: &str) -> String {
        format!(
            "[INST] <<SYS>>\n{}\n<</SYS>>\n{} [/INST]",
            Self::LLAMA2_CHAT_SYSTEM_PROMPT,
            user_input
        )
    }

    pub fn preflight_generation(
        &self,
        prompt_tokens: usize,
        max_tokens: u32,
    ) -> Result<GenerationPreflight, GenerationError> {
        preflight_generation_config(&self.config, prompt_tokens, max_tokens)
    }

    pub fn has_all_transformer_layers(&self) -> bool {
        self.config.n_layers > 0
            && self.layers.len() == self.config.n_layers
            && self.layers.iter().all(CachedLayer::is_loaded)
    }

    pub fn enable_q4(&mut self) {
        let q4_layers: Vec<Q4Layer> = self
            .layers
            .iter()
            .map(|l| Q4Layer {
                wq: Q4WeightsX86::from_i8(&l.wq),
                wk: Q4WeightsX86::from_i8(&l.wk),
                wv: Q4WeightsX86::from_i8(&l.wv),
                wo: Q4WeightsX86::from_i8(&l.wo),
                w_gate: Q4WeightsX86::from_i8(&l.w_gate),
                w_up: Q4WeightsX86::from_i8(&l.w_up),
                w_down: Q4WeightsX86::from_i8(&l.w_down),
            })
            .collect();
        self.q4_output = Some(Q4WeightsX86::from_i8(&self.output_weight));
        self.q4_layers = Some(q4_layers);
    }

    #[cfg(feature = "experimental-ip")]
    pub fn enable_ternary_hybrid(&mut self, outlier_pct: f32) {
        use crate::ternary_hybrid::TernaryHybridWeights;
        if self.ternary_hybrid_layers.is_some() {
            return;
        }
        let hybrid_layers: Vec<TernaryHybridLayer> = self
            .layers
            .iter()
            .map(|l| TernaryHybridLayer {
                wq: TernaryHybridWeights::from_i8(&l.wq, outlier_pct),
                wk: TernaryHybridWeights::from_i8(&l.wk, outlier_pct),
                wv: TernaryHybridWeights::from_i8(&l.wv, outlier_pct),
                wo: TernaryHybridWeights::from_i8(&l.wo, outlier_pct),
                w_gate: TernaryHybridWeights::from_i8(&l.w_gate, outlier_pct),
                w_up: TernaryHybridWeights::from_i8(&l.w_up, outlier_pct),
                w_down: TernaryHybridWeights::from_i8(&l.w_down, outlier_pct),
            })
            .collect();
        self.ternary_hybrid_output = Some(TernaryHybridWeights::from_i8(
            &self.output_weight,
            outlier_pct,
        ));
        self.ternary_hybrid_layers = Some(hybrid_layers);
    }

    #[cfg(feature = "experimental-ip")]
    pub fn enable_ternary(&mut self) {
        use crate::ternary_engine::TernaryWeights;
        if self.ternary_layers.is_some() {
            return;
        }
        let ternary_layers: Vec<TernaryLayer> = self
            .layers
            .iter()
            .map(|l| TernaryLayer {
                wq: TernaryWeights::from_i8(&l.wq),
                wk: TernaryWeights::from_i8(&l.wk),
                wv: TernaryWeights::from_i8(&l.wv),
                wo: TernaryWeights::from_i8(&l.wo),
                w_gate: TernaryWeights::from_i8(&l.w_gate),
                w_up: TernaryWeights::from_i8(&l.w_up),
                w_down: TernaryWeights::from_i8(&l.w_down),
            })
            .collect();
        self.ternary_output = Some(TernaryWeights::from_i8(&self.output_weight));
        self.ternary_layers = Some(ternary_layers);
    }

    pub fn effective_precision_label(&self) -> &'static str {
        if self.ternary_hybrid_layers.is_some() {
            TERNARY_HYBRID_INFERENCE_PROFILE
        } else if self.ternary_layers.is_some() {
            TERNARY_INFERENCE_PROFILE
        } else if self.i16_layers.is_some() {
            I16_INFERENCE_PROFILE
        } else if self.block_i8_layers.is_some() {
            BLOCK_I8_INFERENCE_PROFILE
        } else if self.q4_layers.is_some() {
            Q4_INFERENCE_PROFILE
        } else {
            self.arithmetic_profile()
        }
    }

    pub fn has_canonical_i8_profile(&self) -> bool {
        self.q4_layers.is_none()
            && self.q4_output.is_none()
            && self.i16_layers.is_none()
            && self.i16_output.is_none()
            && self.block_i8_layers.is_none()
            && self.block_i8_output.is_none()
            && self.ternary_layers.is_none()
            && self.ternary_output.is_none()
            && self.ternary_hybrid_layers.is_none()
            && self.ternary_hybrid_output.is_none()
    }

    pub fn enforce_canonical_i8_profile(&mut self) {
        self.q4_layers = None;
        self.q4_output = None;
        self.i16_layers = None;
        self.i16_output = None;
        self.block_i8_layers = None;
        self.block_i8_output = None;
        self.ternary_layers = None;
        self.ternary_output = None;
        self.ternary_hybrid_layers = None;
        self.ternary_hybrid_output = None;
        debug_assert!(self.has_canonical_i8_profile());
    }

    pub fn enable_i16(&mut self) {
        if self.i16_layers.is_some() {
            return;
        }
        let i16_layers: Vec<I16Layer> = self
            .layers
            .iter()
            .map(|l| I16Layer {
                wq: I16Weights::from_i8(&l.wq),
                wk: I16Weights::from_i8(&l.wk),
                wv: I16Weights::from_i8(&l.wv),
                wo: I16Weights::from_i8(&l.wo),
                w_gate: I16Weights::from_i8(&l.w_gate),
                w_up: I16Weights::from_i8(&l.w_up),
                w_down: I16Weights::from_i8(&l.w_down),
            })
            .collect();
        self.i16_output = Some(I16Weights::from_i8(&self.output_weight));
        self.i16_layers = Some(i16_layers);
    }
}

pub struct QuantizedInput {
    pub data: Vec<i8>,
    pub scale_factor: i64,
}

impl QuantizedInput {
    #[inline]
    pub fn from_i64(input: &[i64]) -> Self {
        let abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
        let scale_factor = (abs_max / 127).max(1);
        let data: Vec<i8> = input
            .iter()
            .map(|&x| (x / scale_factor).clamp(-127, 127) as i8)
            .collect();
        Self { data, scale_factor }
    }
}

#[inline(always)]
unsafe fn dot_i8_i64(row: *const i8, input: *const i64, len: usize) -> i64 {
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

pub(crate) fn matmul_i8_into(
    weights: &I8Weights,
    input: &[i64],
    in_size: usize,
    output: &mut [i64],
) {
    matmul_i8_view_into(I8WeightsView::from(weights), input, in_size, output);
}

fn matmul_i8_view_into(
    weights: I8WeightsView<'_>,
    input: &[i64],
    in_size: usize,
    output: &mut [i64],
) {
    if weights.n_rows == 0 || weights.data.is_empty() {
        for o in output.iter_mut() {
            *o = 0;
        }
        return;
    }
    debug_assert_eq!(
        output.len(),
        weights.scales.len(),
        "matmul output/scales mismatch"
    );
    if crate::canonical_simd::fast_canonical_kernel_enabled()
        && crate::canonical_simd::matmul_i8_canonical_rows_fast_view(
            weights, input, in_size, output,
        )
    {
        return;
    }
    let data = weights.data;
    let scales = weights.scales;
    output
        .par_chunks_mut(256)
        .enumerate()
        .for_each(|(chunk_idx, chunk)| {
            let start = chunk_idx * 256;
            for (local_i, out) in chunk.iter_mut().enumerate() {
                let i = start + local_i;
                let acc =
                    unsafe { dot_i8_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size) };
                *out = (acc * scales[i]) >> FRAC_BITS;
            }
        });
}

#[inline]
unsafe fn dot_i8_i64_x4(row: *const i8, inputs: &[*const i64; 4], len: usize) -> [i64; 4] {
    unsafe {
        let mut acc = [0i64; 4];
        for j in 0..len {
            let w = *row.add(j) as i64;
            for q in 0..4 {
                acc[q] += w * *inputs[q].add(j);
            }
        }
        acc
    }
}

fn batched_shape_is_valid(
    weights: &I8Weights,
    inputs: &[i64],
    n_tokens: usize,
    in_size: usize,
    output: &[i64],
) -> bool {
    let (Some(in_len), Some(out_len), Some(weight_len)) = (
        n_tokens.checked_mul(in_size),
        n_tokens.checked_mul(weights.n_rows),
        weights.n_rows.checked_mul(in_size),
    ) else {
        return false;
    };
    n_tokens != 0
        && in_size != 0
        && weights.n_rows != 0
        && weights.n_cols == in_size
        && weights.data.len() == weight_len
        && weights.scales.len() == weights.n_rows
        && inputs.len() == in_len
        && output.len() == out_len
}

pub(crate) fn matmul_i8_into_batched(
    weights: &I8Weights,
    inputs: &[i64],
    n_tokens: usize,
    in_size: usize,
    output: &mut [i64],
) {
    if weights.n_rows == 0 || weights.data.is_empty() {
        for o in output.iter_mut() {
            *o = 0;
        }
        return;
    }
    assert!(
        batched_shape_is_valid(weights, inputs, n_tokens, in_size, output),
        "matmul_i8_into_batched: invalid geometry - n_tokens={n_tokens} in_size={in_size} \
         n_rows={} n_cols={} data={} scales={} inputs={} output={}",
        weights.n_rows,
        weights.n_cols,
        weights.data.len(),
        weights.scales.len(),
        inputs.len(),
        output.len()
    );
    if crate::canonical_simd::fast_canonical_kernel_enabled()
        && crate::canonical_simd::matmul_i8_batched_fast(weights, inputs, n_tokens, in_size, output)
    {
        return;
    }
    let data = &weights.data;
    let scales = &weights.scales;
    let n_rows = weights.n_rows;
    if n_tokens < 4 {
        for (input, out) in inputs
            .chunks_exact(in_size)
            .zip(output.chunks_exact_mut(weights.n_rows))
        {
            matmul_i8_into(weights, input, in_size, out);
        }
        return;
    }
    let row_block = (131_072 / in_size.max(1)).clamp(1, n_rows);
    let n_blocks = n_rows.div_ceil(row_block);
    let out = BatchOutPtr(output.as_mut_ptr());
    (0..n_blocks).into_par_iter().for_each(|b| {
        let r0 = b * row_block;
        let r1 = (r0 + row_block).min(n_rows);
        let mut t = 0usize;
        while t < n_tokens {
            let quad = (n_tokens - t).min(4);
            if quad < 4 {
                for (offset, &sc) in scales[r0..r1].iter().enumerate() {
                    let i = r0 + offset;
                    for q in 0..quad {
                        let acc = unsafe {
                            dot_i8_i64(
                                data.as_ptr().add(i * in_size),
                                inputs.as_ptr().add((t + q) * in_size),
                                in_size,
                            )
                        };
                        unsafe { *out.get().add((t + q) * n_rows + i) = (acc * sc) >> FRAC_BITS };
                    }
                }
                t += quad;
                continue;
            }
            let src = |q: usize| {
                let tok = t + q.min(quad - 1);
                unsafe { inputs.as_ptr().add(tok * in_size) }
            };
            let ins = [src(0), src(1), src(2), src(3)];
            for (offset, &sc) in scales[r0..r1].iter().enumerate() {
                let i = r0 + offset;
                let acc = unsafe { dot_i8_i64_x4(data.as_ptr().add(i * in_size), &ins, in_size) };
                for (q, a) in acc.iter().enumerate().take(quad) {
                    unsafe { *out.get().add((t + q) * n_rows + i) = (*a * sc) >> FRAC_BITS };
                }
            }
            t += quad;
        }
    });
}

const SILU_CHUNK: usize = 2048;

#[derive(Clone, Copy)]
struct BatchOutPtr(*mut i64);
unsafe impl Send for BatchOutPtr {}
unsafe impl Sync for BatchOutPtr {}
impl BatchOutPtr {
    #[inline]
    fn get(self) -> *mut i64 {
        self.0
    }
}

pub fn matmul_i8_canonical_rows(
    weights: &I8Weights,
    input: &[i64],
    output: &mut [i64],
) -> Result<(), String> {
    matmul_i8_canonical_row_range(weights, 0, weights.n_rows, input, output)
}

pub fn matmul_i8_canonical_row_range(
    weights: &I8Weights,
    start: usize,
    end: usize,
    input: &[i64],
    output: &mut [i64],
) -> Result<(), String> {
    let expected_data_len = weights
        .n_rows
        .checked_mul(weights.n_cols)
        .ok_or_else(|| "canonical I8 matrix geometry overflows".to_string())?;
    if weights.n_rows == 0
        || weights.n_cols == 0
        || weights.data.len() != expected_data_len
        || weights.scales.len() != weights.n_rows
        || start >= end
        || end > weights.n_rows
    {
        return Err("invalid canonical I8 row range".into());
    }
    let rows = end - start;
    if input.len() != weights.n_cols || output.len() != rows {
        return Err(format!(
            "canonical I8 row projection shape mismatch: input {}, output {}, weights {}x{} range [{start}, {end})",
            input.len(),
            output.len(),
            weights.n_rows,
            weights.n_cols
        ));
    }
    let input_abs_sum = input
        .iter()
        .try_fold(0i64, |sum, value| {
            sum.checked_add(value.checked_abs().ok_or(())?).ok_or(())
        })
        .map_err(|_| "canonical I8 input magnitude overflows accumulator".to_string())?;
    let dot_bound = input_abs_sum
        .checked_mul(128)
        .ok_or_else(|| "canonical I8 dot bound overflows".to_string())?;
    let row_start = start
        .checked_mul(weights.n_cols)
        .ok_or_else(|| "canonical I8 row offset overflows".to_string())?;
    let row_end = end
        .checked_mul(weights.n_cols)
        .ok_or_else(|| "canonical I8 row offset overflows".to_string())?;
    let view = I8WeightsView {
        data: &weights.data[row_start..row_end],
        scales: &weights.scales[start..end],
        n_rows: rows,
        n_cols: weights.n_cols,
    };
    if view.scales.iter().any(|scale| {
        scale
            .checked_abs()
            .and_then(|s| dot_bound.checked_mul(s))
            .is_none()
    }) {
        return Err("canonical I8 scale multiply would overflow".into());
    }
    matmul_i8_view_into(view, input, view.n_cols, output);
    Ok(())
}

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
            let acc =
                unsafe { dot_i8_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size) };
            output[i] = (acc * scales[i]) >> FRAC_BITS;
        }
    }
    output
}

#[allow(dead_code)]
#[inline(always)]
unsafe fn dot_i16_i64_scalar(row: *const i16, input: *const i64, len: usize) -> i64 {
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

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn dot_i16_i64_neon(row: *const i16, input: *const i64, len: usize) -> i64 {
    unsafe {
        use std::arch::aarch64::*;
        let acc2: i64 = 0;
        let acc3: i64 = 0;
        let simd_len = len / 8 * 8;
        let mut j = 0usize;
        let mut va0 = vdupq_n_s64(0);
        let mut va1 = vdupq_n_s64(0);
        let mut va2 = vdupq_n_s64(0);
        let mut va3 = vdupq_n_s64(0);
        while j < simd_len {
            let w16 = vld1q_s16(row.add(j));
            let w32_lo = vmovl_s16(vget_low_s16(w16));
            let w32_hi = vmovl_s16(vget_high_s16(w16));
            let i64_0 = vld1q_s64(input.add(j));
            let i64_1 = vld1q_s64(input.add(j + 2));
            let i64_2 = vld1q_s64(input.add(j + 4));
            let i64_3 = vld1q_s64(input.add(j + 6));
            let i32_lo = vcombine_s32(vmovn_s64(i64_0), vmovn_s64(i64_1));
            let i32_hi = vcombine_s32(vmovn_s64(i64_2), vmovn_s64(i64_3));
            va0 = vmlal_s32(va0, vget_low_s32(w32_lo), vget_low_s32(i32_lo));
            va1 = vmlal_high_s32(va1, w32_lo, i32_lo);
            va2 = vmlal_s32(va2, vget_low_s32(w32_hi), vget_low_s32(i32_hi));
            va3 = vmlal_high_s32(va3, w32_hi, i32_hi);
            j += 8;
        }
        let s01 = vaddq_s64(va0, va1);
        let s23 = vaddq_s64(va2, va3);
        let s = vaddq_s64(s01, s23);
        let acc0 = vgetq_lane_s64(s, 0);
        let acc1 = vgetq_lane_s64(s, 1);
        let mut acc = acc0 + acc1 + acc2 + acc3;
        while j < len {
            acc += (*row.add(j) as i64) * (*input.add(j));
            j += 1;
        }
        acc
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn dot_i16_i64_avx2(row: *const i16, input: *const i64, len: usize) -> i64 {
    unsafe { dot_i16_i64_scalar(row, input, len) }
}

#[inline(always)]
unsafe fn dot_i16_i64(row: *const i16, input: *const i64, len: usize) -> i64 {
    unsafe {
        #[cfg(target_arch = "aarch64")]
        {
            dot_i16_i64_neon(row, input, len)
        }
        #[cfg(target_arch = "x86_64")]
        {
            dot_i16_i64_avx2(row, input, len)
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            dot_i16_i64_scalar(row, input, len)
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn dot_i64xi64_attn_neon(a: *const i64, b: *const i64, len: usize) -> i64 {
    unsafe {
        use std::arch::aarch64::*;
        let a_values = std::slice::from_raw_parts(a, len);
        let b_values = std::slice::from_raw_parts(b, len);
        let outside_i32 = a_values.iter().chain(b_values).fold(0u64, |acc, &x| {
            acc | ((x.wrapping_add(1 << 31) as u64) >> 32)
        });
        if outside_i32 != 0 {
            return dot_i64xi64_exact(a_values, b_values);
        }
        let mut acc = vdupq_n_s64(0);
        let simd_len = len / 4 * 4;
        let mut j = 0usize;
        while j < simd_len {
            let a0 = vld1q_s64(a.add(j));
            let a1 = vld1q_s64(a.add(j + 2));
            let b0 = vld1q_s64(b.add(j));
            let b1 = vld1q_s64(b.add(j + 2));
            let a32 = vcombine_s32(vmovn_s64(a0), vmovn_s64(a1));
            let b32 = vcombine_s32(vmovn_s64(b0), vmovn_s64(b1));
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

#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn dot_i64xi64_exact(a: &[i64], b: &[i64]) -> i64 {
    let mut sum: i64 = 0;
    for (x, y) in a.iter().zip(b) {
        sum += x * y;
    }
    sum
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
unsafe fn dot_i64xi64_attn_neon(a: *const i64, b: *const i64, len: usize) -> i64 {
    unsafe {
        let mut sum: i64 = 0;
        for i in 0..len {
            sum += (*a.add(i)) * (*b.add(i));
        }
        sum
    }
}

fn matmul_i16_into(weights: &I16Weights, input: &[i64], in_size: usize, output: &mut [i64]) {
    if weights.n_rows == 0 || weights.data.is_empty() {
        for o in output.iter_mut() {
            *o = 0;
        }
        return;
    }
    debug_assert!(
        output.len() <= weights.data.len() / in_size,
        "i16 matmul bounds"
    );
    let data = &weights.data;
    let scales = &weights.scales;
    output
        .par_chunks_mut(256)
        .enumerate()
        .for_each(|(chunk_idx, chunk)| {
            let start = chunk_idx * 256;
            for (local_i, out) in chunk.iter_mut().enumerate() {
                let i = start + local_i;
                let acc =
                    unsafe { dot_i16_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size) };
                let wide = (acc as i128) * (scales[i] as i128);
                *out = ((wide / 32767) >> FRAC_BITS as i128) as i64;
            }
        });
}

#[allow(dead_code)]
fn matmul_i16(weights: &I16Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let mut output = vec![0i64; out_size];
    if weights.n_rows == 0 || weights.data.is_empty() {
        return output;
    }
    matmul_i16_into(weights, input, in_size, &mut output);
    output
}

#[allow(dead_code)]
#[cfg(target_arch = "aarch64")]
fn matmul_i8xi8_simd(
    weights: &I8Weights,
    input: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<i64> {
    use std::arch::aarch64::*;

    if weights.n_rows == 0 || weights.data.is_empty() {
        return vec![0i64; out_size];
    }

    let input_abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let input_scale_factor = (input_abs_max / 127).max(1);
    let input_i8: Vec<i8> = input
        .iter()
        .map(|&x| (x / input_scale_factor).clamp(-127, 127) as i8)
        .collect();

    let data = &weights.data;
    let inp_slice = &input_i8;
    let scales = &weights.scales;

    let mut output = vec![0i64; out_size];
    output
        .par_chunks_mut(64)
        .enumerate()
        .for_each(|(chunk_idx, chunk)| {
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

#[cfg_attr(not(test), allow(dead_code))]
#[cfg(target_arch = "x86_64")]
fn matmul_i8xi8_simd(
    weights: &I8Weights,
    input: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<i64> {
    use std::arch::x86_64::*;

    if weights.n_rows == 0 || weights.data.is_empty() {
        return vec![0i64; out_size];
    }

    if !is_x86_feature_detected!("avx2") {
        return matmul_i8(weights, input, in_size, out_size);
    }

    let use_avx512 = is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512f");

    let input_abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let input_scale_factor = (input_abs_max / 127).max(1);
    let aligned_len = (in_size + 63) & !63;
    let mut input_i8 = vec![0i8; aligned_len];
    for (i, &x) in input.iter().enumerate() {
        input_i8[i] = (x / input_scale_factor).clamp(-127, 127) as i8;
    }

    let data = &weights.data;
    let inp_slice = &input_i8;
    let scales = &weights.scales;

    let mut output = vec![0i64; out_size];
    output
        .par_chunks_mut(64)
        .enumerate()
        .for_each(|(chunk_idx, chunk)| {
            let start = chunk_idx * 64;
            for (local_i, out) in chunk.iter_mut().enumerate() {
                let i = start + local_i;
                let row = unsafe { data.as_ptr().add(i * in_size) };
                let inp_ptr = inp_slice.as_ptr();
                let mut acc: i64;

                if use_avx512 {
                    let simd_len = in_size / 128 * 128;
                    unsafe {
                        let mut vacc0 = _mm512_setzero_si512();
                        let mut vacc1 = _mm512_setzero_si512();
                        let mut vacc2 = _mm512_setzero_si512();
                        let mut vacc3 = _mm512_setzero_si512();

                        let mut j = 0usize;
                        while j < simd_len {
                            _mm_prefetch(row.add(j + 256), _MM_HINT_T0);
                            _mm_prefetch(inp_ptr.add(j + 256), _MM_HINT_T0);

                            let vw0 = _mm512_loadu_si512(row.add(j) as *const __m512i);
                            let vi0 = _mm512_loadu_si512(inp_ptr.add(j) as *const __m512i);
                            let vw0_lo = _mm512_castsi512_si256(vw0);
                            let vw0_hi = _mm512_extracti64x4_epi64(vw0, 1);
                            let vi0_lo = _mm512_castsi512_si256(vi0);
                            let vi0_hi = _mm512_extracti64x4_epi64(vi0, 1);
                            vacc0 = _mm512_add_epi32(
                                vacc0,
                                _mm512_madd_epi16(
                                    _mm512_cvtepi8_epi16(vw0_lo),
                                    _mm512_cvtepi8_epi16(vi0_lo),
                                ),
                            );
                            vacc1 = _mm512_add_epi32(
                                vacc1,
                                _mm512_madd_epi16(
                                    _mm512_cvtepi8_epi16(vw0_hi),
                                    _mm512_cvtepi8_epi16(vi0_hi),
                                ),
                            );

                            let vw1 = _mm512_loadu_si512(row.add(j + 64) as *const __m512i);
                            let vi1 = _mm512_loadu_si512(inp_ptr.add(j + 64) as *const __m512i);
                            let vw1_lo = _mm512_castsi512_si256(vw1);
                            let vw1_hi = _mm512_extracti64x4_epi64(vw1, 1);
                            let vi1_lo = _mm512_castsi512_si256(vi1);
                            let vi1_hi = _mm512_extracti64x4_epi64(vi1, 1);
                            vacc2 = _mm512_add_epi32(
                                vacc2,
                                _mm512_madd_epi16(
                                    _mm512_cvtepi8_epi16(vw1_lo),
                                    _mm512_cvtepi8_epi16(vi1_lo),
                                ),
                            );
                            vacc3 = _mm512_add_epi32(
                                vacc3,
                                _mm512_madd_epi16(
                                    _mm512_cvtepi8_epi16(vw1_hi),
                                    _mm512_cvtepi8_epi16(vi1_hi),
                                ),
                            );
                            j += 128;
                        }

                        vacc0 = _mm512_add_epi32(
                            _mm512_add_epi32(vacc0, vacc1),
                            _mm512_add_epi32(vacc2, vacc3),
                        );
                        acc = _mm512_reduce_add_epi32(vacc0) as i64;

                        if j + 64 <= in_size {
                            let vw = _mm512_loadu_si512(row.add(j) as *const __m512i);
                            let vi = _mm512_loadu_si512(inp_ptr.add(j) as *const __m512i);
                            let vw_lo = _mm512_castsi512_si256(vw);
                            let vw_hi = _mm512_extracti64x4_epi64(vw, 1);
                            let vi_lo = _mm512_castsi512_si256(vi);
                            let vi_hi = _mm512_extracti64x4_epi64(vi, 1);
                            let mut vr = _mm512_madd_epi16(
                                _mm512_cvtepi8_epi16(vw_lo),
                                _mm512_cvtepi8_epi16(vi_lo),
                            );
                            vr = _mm512_add_epi32(
                                vr,
                                _mm512_madd_epi16(
                                    _mm512_cvtepi8_epi16(vw_hi),
                                    _mm512_cvtepi8_epi16(vi_hi),
                                ),
                            );
                            acc += _mm512_reduce_add_epi32(vr) as i64;
                            j += 64;
                        }

                        while j < in_size {
                            acc += (*row.add(j) as i64) * (*inp_ptr.add(j) as i64);
                            j += 1;
                        }
                    }
                } else {
                    let simd_len = in_size / 128 * 128;
                    unsafe {
                        let mut vacc0 = _mm256_setzero_si256();
                        let mut vacc1 = _mm256_setzero_si256();
                        let mut vacc2 = _mm256_setzero_si256();
                        let mut vacc3 = _mm256_setzero_si256();
                        let ones = _mm256_set1_epi16(1);

                        let mut j = 0usize;
                        while j < simd_len {
                            _mm_prefetch(row.add(j + 256), _MM_HINT_T0);
                            _mm_prefetch(inp_ptr.add(j + 256), _MM_HINT_T0);

                            let vw0 = _mm256_loadu_si256(row.add(j) as *const __m256i);
                            let vi0 = _mm256_loadu_si256(inp_ptr.add(j) as *const __m256i);
                            let ax0 = _mm256_sign_epi8(vw0, vw0);
                            let sy0 = _mm256_sign_epi8(vi0, vw0);
                            vacc0 = _mm256_add_epi32(
                                vacc0,
                                _mm256_madd_epi16(_mm256_maddubs_epi16(ax0, sy0), ones),
                            );

                            let vw1 = _mm256_loadu_si256(row.add(j + 32) as *const __m256i);
                            let vi1 = _mm256_loadu_si256(inp_ptr.add(j + 32) as *const __m256i);
                            let ax1 = _mm256_sign_epi8(vw1, vw1);
                            let sy1 = _mm256_sign_epi8(vi1, vw1);
                            vacc1 = _mm256_add_epi32(
                                vacc1,
                                _mm256_madd_epi16(_mm256_maddubs_epi16(ax1, sy1), ones),
                            );

                            let vw2 = _mm256_loadu_si256(row.add(j + 64) as *const __m256i);
                            let vi2 = _mm256_loadu_si256(inp_ptr.add(j + 64) as *const __m256i);
                            let ax2 = _mm256_sign_epi8(vw2, vw2);
                            let sy2 = _mm256_sign_epi8(vi2, vw2);
                            vacc2 = _mm256_add_epi32(
                                vacc2,
                                _mm256_madd_epi16(_mm256_maddubs_epi16(ax2, sy2), ones),
                            );

                            let vw3 = _mm256_loadu_si256(row.add(j + 96) as *const __m256i);
                            let vi3 = _mm256_loadu_si256(inp_ptr.add(j + 96) as *const __m256i);
                            let ax3 = _mm256_sign_epi8(vw3, vw3);
                            let sy3 = _mm256_sign_epi8(vi3, vw3);
                            vacc3 = _mm256_add_epi32(
                                vacc3,
                                _mm256_madd_epi16(_mm256_maddubs_epi16(ax3, sy3), ones),
                            );

                            j += 128;
                        }

                        vacc0 = _mm256_add_epi32(
                            _mm256_add_epi32(vacc0, vacc1),
                            _mm256_add_epi32(vacc2, vacc3),
                        );

                        while j + 32 <= in_size {
                            let vw = _mm256_loadu_si256(row.add(j) as *const __m256i);
                            let vi = _mm256_loadu_si256(inp_ptr.add(j) as *const __m256i);
                            let ax = _mm256_sign_epi8(vw, vw);
                            let sy = _mm256_sign_epi8(vi, vw);
                            vacc0 = _mm256_add_epi32(
                                vacc0,
                                _mm256_madd_epi16(_mm256_maddubs_epi16(ax, sy), ones),
                            );
                            j += 32;
                        }

                        let lo = _mm256_extracti128_si256(vacc0, 0);
                        let hi = _mm256_extracti128_si256(vacc0, 1);
                        let sum128 = _mm_add_epi32(lo, hi);
                        let sum128 = _mm_hadd_epi32(sum128, sum128);
                        let sum128 = _mm_hadd_epi32(sum128, sum128);
                        acc = _mm_extract_epi32(sum128, 0) as i64;

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

pub fn matmul_fast(
    weights: &I8Weights,
    input: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<i64> {
    matmul_i8(weights, input, in_size, out_size)
}

pub fn matmul_fast_preq(
    weights: &I8Weights,
    _input_q: &QuantizedInput,
    input_raw: &[i64],
    in_size: usize,
    output: &mut [i64],
) {
    matmul_i8_into(weights, input_raw, in_size, output);
}

#[allow(dead_code)]
#[cfg(target_arch = "aarch64")]
fn matmul_simd_preq_neon(
    weights: &I8Weights,
    input_q: &QuantizedInput,
    in_size: usize,
    output: &mut [i64],
) {
    debug_assert_eq!(
        output.len(),
        weights.scales.len(),
        "matmul output/scales mismatch"
    );
    use std::arch::aarch64::*;
    let data = &weights.data;
    let inp = &input_q.data;
    let scales = &weights.scales;
    let isf = input_q.scale_factor;

    output
        .par_chunks_mut(512)
        .enumerate()
        .for_each(|(ci, chunk)| {
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
                    while j < in_size {
                        acc += (*row.add(j) as i64) * (*inp.as_ptr().add(j) as i64);
                        j += 1;
                    }
                }
                *out = acc * ((scales[i] * isf) >> FRAC_BITS);
            }
        });
}

#[allow(dead_code)]
#[cfg(target_arch = "x86_64")]
fn matmul_simd_preq_x86(
    weights: &I8Weights,
    input_q: &QuantizedInput,
    in_size: usize,
    output: &mut [i64],
) {
    debug_assert_eq!(
        output.len(),
        weights.scales.len(),
        "matmul output/scales mismatch"
    );
    use std::arch::x86_64::*;
    let data = &weights.data;
    let inp = &input_q.data;
    let scales = &weights.scales;
    let isf = input_q.scale_factor;
    let use512 = is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512f");

    output
        .par_chunks_mut(512)
        .enumerate()
        .for_each(|(ci, chunk)| {
            let base = ci * 512;
            for (li, out) in chunk.iter_mut().enumerate() {
                let i = base + li;
                let row = unsafe { data.as_ptr().add(i * in_size) };
                let ip = inp.as_ptr();
                let mut acc: i64;
                unsafe {
                    if use512 {
                        let sl = in_size / 64 * 64;
                        let mut a0 = _mm512_setzero_si512();
                        let mut a1 = _mm512_setzero_si512();
                        let mut j = 0usize;
                        while j < sl {
                            let vw = _mm512_loadu_si512(row.add(j) as *const __m512i);
                            let vi = _mm512_loadu_si512(ip.add(j) as *const __m512i);
                            a0 = _mm512_add_epi32(
                                a0,
                                _mm512_madd_epi16(
                                    _mm512_cvtepi8_epi16(_mm512_castsi512_si256(vw)),
                                    _mm512_cvtepi8_epi16(_mm512_castsi512_si256(vi)),
                                ),
                            );
                            a1 = _mm512_add_epi32(
                                a1,
                                _mm512_madd_epi16(
                                    _mm512_cvtepi8_epi16(_mm512_extracti64x4_epi64(vw, 1)),
                                    _mm512_cvtepi8_epi16(_mm512_extracti64x4_epi64(vi, 1)),
                                ),
                            );
                            j += 64;
                        }
                        acc = _mm512_reduce_add_epi32(_mm512_add_epi32(a0, a1)) as i64;
                        while j < in_size {
                            acc += (*row.add(j) as i64) * (*ip.add(j) as i64);
                            j += 1;
                        }
                    } else {
                        let sl = in_size / 32 * 32;
                        let mut a0 = _mm256_setzero_si256();
                        let mut a1 = _mm256_setzero_si256();
                        let mut j = 0usize;
                        while j < sl {
                            let vw = _mm256_loadu_si256(row.add(j) as *const __m256i);
                            let vi = _mm256_loadu_si256(ip.add(j) as *const __m256i);
                            a0 = _mm256_add_epi32(
                                a0,
                                _mm256_madd_epi16(
                                    _mm256_cvtepi8_epi16(_mm256_castsi256_si128(vw)),
                                    _mm256_cvtepi8_epi16(_mm256_castsi256_si128(vi)),
                                ),
                            );
                            a1 = _mm256_add_epi32(
                                a1,
                                _mm256_madd_epi16(
                                    _mm256_cvtepi8_epi16(_mm256_extracti128_si256(vw, 1)),
                                    _mm256_cvtepi8_epi16(_mm256_extracti128_si256(vi, 1)),
                                ),
                            );
                            j += 32;
                        }
                        let v = _mm256_add_epi32(a0, a1);
                        let lo = _mm256_extracti128_si256(v, 0);
                        let hi = _mm256_extracti128_si256(v, 1);
                        let s = _mm_hadd_epi32(_mm_add_epi32(lo, hi), _mm_setzero_si128());
                        let s = _mm_hadd_epi32(s, _mm_setzero_si128());
                        acc = _mm_extract_epi32(s, 0) as i64;
                        while j < in_size {
                            acc += (*row.add(j) as i64) * (*ip.add(j) as i64);
                            j += 1;
                        }
                    }
                }
                *out = acc * ((scales[i] * isf) >> FRAC_BITS);
            }
        });
}

pub struct Q4WeightsX86 {
    pub data: Vec<u8>,
    pub scales: Vec<i64>,
    pub n_rows: usize,
    pub n_cols: usize,
}

impl Q4WeightsX86 {
    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 8
    }

    pub fn from_i8(w: &I8Weights) -> Self {
        let n_rows = w.n_rows;
        let n_cols = w.n_cols;
        let mut data = Vec::with_capacity(w.data.len() / 2);
        let mut scales = Vec::with_capacity(n_rows);

        for i in 0..n_rows {
            let row = &w.data[i * n_cols..(i + 1) * n_cols];

            let abs_max = row
                .iter()
                .map(|&x| (x as i16).unsigned_abs() as u8)
                .max()
                .unwrap_or(1)
                .max(1);
            let q4_per_unit = ((abs_max as i64 + 6) / 7).max(1);

            for pair in row.chunks(2) {
                let v0 = ((pair[0] as i16) / q4_per_unit as i16).clamp(-8, 7);
                let v1 = if pair.len() > 1 {
                    ((pair[1] as i16) / q4_per_unit as i16).clamp(-8, 7)
                } else {
                    0
                };
                let lo = ((v0 + 8) as u8) & 0x0F;
                let hi = ((v1 + 8) as u8) & 0x0F;
                data.push(lo | (hi << 4));
            }

            scales.push(w.scales[i] * q4_per_unit);
        }

        Q4WeightsX86 {
            data,
            scales,
            n_rows,
            n_cols,
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub fn matmul_q4_preq_x86(q4: &Q4WeightsX86, input_q: &QuantizedInput, output: &mut [i64]) {
    if q4.n_rows == 0 || q4.data.is_empty() {
        for o in output.iter_mut() {
            *o = 0;
        }
        return;
    }
    debug_assert_eq!(
        output.len(),
        q4.scales.len(),
        "matmul output/scales mismatch"
    );
    use std::arch::x86_64::*;

    if !is_x86_feature_detected!("avx2") {
        matmul_q4_scalar(q4, input_q, output);
        return;
    }

    let in_size = q4.n_cols;
    let byte_cols = in_size / 2;
    let data = &q4.data;
    let inp = &input_q.data;
    let scales = &q4.scales;
    let isf = input_q.scale_factor;

    output
        .par_chunks_mut(64)
        .enumerate()
        .for_each(|(ci, chunk)| {
            let base = ci * 64;
            for (li, out) in chunk.iter_mut().enumerate() {
                let i = base + li;
                let row = unsafe { data.as_ptr().add(i * byte_cols) };
                let ip = inp.as_ptr();
                let mut acc: i64;

                unsafe {
                    let simd_len = byte_cols / 64 * 64;
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
                        _mm_prefetch(ip.add(j * 2 + 256), _MM_HINT_T0);

                        let packed0 = _mm_loadu_si128(row.add(j) as *const __m128i);
                        let lo0 = _mm_and_si128(packed0, mask_lo);
                        let hi0 = _mm_and_si128(_mm_srli_epi16(packed0, 4), mask_lo);
                        let interleaved0 = _mm256_sub_epi8(
                            _mm256_set_m128i(
                                _mm_unpackhi_epi8(lo0, hi0),
                                _mm_unpacklo_epi8(lo0, hi0),
                            ),
                            bias,
                        );
                        let vi0 = _mm256_loadu_si256(ip.add(j * 2) as *const __m256i);
                        let ax0 = _mm256_sign_epi8(interleaved0, interleaved0);
                        let sy0 = _mm256_sign_epi8(vi0, interleaved0);
                        vacc0 = _mm256_add_epi32(
                            vacc0,
                            _mm256_madd_epi16(_mm256_maddubs_epi16(ax0, sy0), ones),
                        );

                        let packed1 = _mm_loadu_si128(row.add(j + 16) as *const __m128i);
                        let lo1 = _mm_and_si128(packed1, mask_lo);
                        let hi1 = _mm_and_si128(_mm_srli_epi16(packed1, 4), mask_lo);
                        let interleaved1 = _mm256_sub_epi8(
                            _mm256_set_m128i(
                                _mm_unpackhi_epi8(lo1, hi1),
                                _mm_unpacklo_epi8(lo1, hi1),
                            ),
                            bias,
                        );
                        let vi1 = _mm256_loadu_si256(ip.add(j * 2 + 32) as *const __m256i);
                        let ax1 = _mm256_sign_epi8(interleaved1, interleaved1);
                        let sy1 = _mm256_sign_epi8(vi1, interleaved1);
                        vacc1 = _mm256_add_epi32(
                            vacc1,
                            _mm256_madd_epi16(_mm256_maddubs_epi16(ax1, sy1), ones),
                        );

                        let packed2 = _mm_loadu_si128(row.add(j + 32) as *const __m128i);
                        let lo2 = _mm_and_si128(packed2, mask_lo);
                        let hi2 = _mm_and_si128(_mm_srli_epi16(packed2, 4), mask_lo);
                        let interleaved2 = _mm256_sub_epi8(
                            _mm256_set_m128i(
                                _mm_unpackhi_epi8(lo2, hi2),
                                _mm_unpacklo_epi8(lo2, hi2),
                            ),
                            bias,
                        );
                        let vi2 = _mm256_loadu_si256(ip.add(j * 2 + 64) as *const __m256i);
                        let ax2 = _mm256_sign_epi8(interleaved2, interleaved2);
                        let sy2 = _mm256_sign_epi8(vi2, interleaved2);
                        vacc2 = _mm256_add_epi32(
                            vacc2,
                            _mm256_madd_epi16(_mm256_maddubs_epi16(ax2, sy2), ones),
                        );

                        let packed3 = _mm_loadu_si128(row.add(j + 48) as *const __m128i);
                        let lo3 = _mm_and_si128(packed3, mask_lo);
                        let hi3 = _mm_and_si128(_mm_srli_epi16(packed3, 4), mask_lo);
                        let interleaved3 = _mm256_sub_epi8(
                            _mm256_set_m128i(
                                _mm_unpackhi_epi8(lo3, hi3),
                                _mm_unpacklo_epi8(lo3, hi3),
                            ),
                            bias,
                        );
                        let vi3 = _mm256_loadu_si256(ip.add(j * 2 + 96) as *const __m256i);
                        let ax3 = _mm256_sign_epi8(interleaved3, interleaved3);
                        let sy3 = _mm256_sign_epi8(vi3, interleaved3);
                        vacc3 = _mm256_add_epi32(
                            vacc3,
                            _mm256_madd_epi16(_mm256_maddubs_epi16(ax3, sy3), ones),
                        );

                        j += 64;
                    }

                    vacc0 = _mm256_add_epi32(
                        _mm256_add_epi32(vacc0, vacc1),
                        _mm256_add_epi32(vacc2, vacc3),
                    );

                    while j + 16 <= byte_cols {
                        let packed = _mm_loadu_si128(row.add(j) as *const __m128i);
                        let lo = _mm_and_si128(packed, mask_lo);
                        let hi = _mm_and_si128(_mm_srli_epi16(packed, 4), mask_lo);
                        let interleaved = _mm256_sub_epi8(
                            _mm256_set_m128i(_mm_unpackhi_epi8(lo, hi), _mm_unpacklo_epi8(lo, hi)),
                            bias,
                        );
                        let vi = _mm256_loadu_si256(ip.add(j * 2) as *const __m256i);
                        let ax = _mm256_sign_epi8(interleaved, interleaved);
                        let sy = _mm256_sign_epi8(vi, interleaved);
                        vacc0 = _mm256_add_epi32(
                            vacc0,
                            _mm256_madd_epi16(_mm256_maddubs_epi16(ax, sy), ones),
                        );
                        j += 16;
                    }

                    let lo128 = _mm256_extracti128_si256(vacc0, 0);
                    let hi128 = _mm256_extracti128_si256(vacc0, 1);
                    let sum128 = _mm_add_epi32(lo128, hi128);
                    let sum128 = _mm_hadd_epi32(sum128, sum128);
                    let sum128 = _mm_hadd_epi32(sum128, sum128);
                    acc = _mm_extract_epi32(sum128, 0) as i64;

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

#[cfg(target_arch = "aarch64")]
pub fn matmul_q4_preq_neon(q4: &Q4WeightsX86, input_q: &QuantizedInput, output: &mut [i64]) {
    if q4.n_rows == 0 || q4.data.is_empty() {
        for o in output.iter_mut() {
            *o = 0;
        }
        return;
    }
    debug_assert_eq!(
        output.len(),
        q4.scales.len(),
        "matmul output/scales mismatch"
    );
    use std::arch::aarch64::*;

    let in_size = q4.n_cols;
    let byte_cols = in_size / 2;
    let data = &q4.data;
    let inp = &input_q.data;
    let scales = &q4.scales;
    let isf = input_q.scale_factor;

    output
        .par_chunks_mut(512)
        .enumerate()
        .for_each(|(ci, chunk)| {
            let base = ci * 512;
            for (li, out) in chunk.iter_mut().enumerate() {
                let i = base + li;
                let row_off = i * byte_cols;
                let mut acc: i64;

                unsafe {
                    let simd_len = byte_cols / 16 * 16;
                    let bias = vdupq_n_s8(8);
                    let mask_lo = vdupq_n_u8(0x0F);
                    let mut vacc0 = vdupq_n_s32(0);
                    let mut vacc1 = vdupq_n_s32(0);
                    let mut vacc2 = vdupq_n_s32(0);
                    let mut vacc3 = vdupq_n_s32(0);

                    let mut j = 0usize;
                    while j < simd_len {
                        let packed = vld1q_u8(data.as_ptr().add(row_off + j));

                        let lo = vreinterpretq_s8_u8(vandq_u8(packed, mask_lo));
                        let hi = vreinterpretq_s8_u8(vshrq_n_u8(packed, 4));

                        let q_lo = vsubq_s8(lo, bias);
                        let q_hi = vsubq_s8(hi, bias);

                        let wlo_lo = vget_low_s8(q_lo);
                        let whi_lo = vget_low_s8(q_hi);
                        let wlo_hi = vget_high_s8(q_lo);
                        let whi_hi = vget_high_s8(q_hi);

                        let w_interleaved_0 =
                            vzip1q_s8(vcombine_s8(wlo_lo, wlo_hi), vcombine_s8(whi_lo, whi_hi));
                        let w_interleaved_1 =
                            vzip2q_s8(vcombine_s8(wlo_lo, wlo_hi), vcombine_s8(whi_lo, whi_hi));

                        let i0 = vld1q_s8(inp.as_ptr().add(j * 2));
                        let i1 = vld1q_s8(inp.as_ptr().add(j * 2 + 16));

                        vacc0 = vpadalq_s16(
                            vacc0,
                            vmull_s8(vget_low_s8(w_interleaved_0), vget_low_s8(i0)),
                        );
                        vacc1 = vpadalq_s16(
                            vacc1,
                            vmull_s8(vget_high_s8(w_interleaved_0), vget_high_s8(i0)),
                        );
                        vacc2 = vpadalq_s16(
                            vacc2,
                            vmull_s8(vget_low_s8(w_interleaved_1), vget_low_s8(i1)),
                        );
                        vacc3 = vpadalq_s16(
                            vacc3,
                            vmull_s8(vget_high_s8(w_interleaved_1), vget_high_s8(i1)),
                        );

                        j += 16;
                    }

                    vacc0 = vaddq_s32(vaddq_s32(vacc0, vacc1), vaddq_s32(vacc2, vacc3));
                    acc = vaddvq_s32(vacc0) as i64;

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

#[cfg(target_arch = "x86_64")]
fn matmul_q4_scalar(q4: &Q4WeightsX86, input_q: &QuantizedInput, output: &mut [i64]) {
    if q4.n_rows == 0 || q4.data.is_empty() {
        for o in output.iter_mut() {
            *o = 0;
        }
        return;
    }
    debug_assert_eq!(
        output.len(),
        q4.scales.len(),
        "matmul output/scales mismatch"
    );
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
            acc += (w_lo as i64) * (inp[j * 2] as i64) + (w_hi as i64) * (inp[j * 2 + 1] as i64);
        }
        *out = acc * ((scales[i] * isf) >> FRAC_BITS);
    }
}

pub fn matmul_q4_full(q4: &Q4WeightsX86, input: &[i64], output: &mut [i64]) {
    if q4.n_rows == 0 || q4.data.is_empty() {
        for o in output.iter_mut() {
            *o = 0;
        }
        return;
    }
    debug_assert_eq!(
        output.len(),
        q4.scales.len(),
        "matmul output/scales mismatch"
    );
    let byte_cols = q4.n_cols / 2;
    let data = &q4.data;
    let scales = &q4.scales;

    output
        .par_chunks_mut(512)
        .enumerate()
        .for_each(|(ci, chunk)| {
            let base = ci * 512;
            for (li, out) in chunk.iter_mut().enumerate() {
                let i = base + li;
                let row_off = i * byte_cols;
                let mut acc: i64 = 0;

                for j in 0..byte_cols {
                    let byte = data[row_off + j];
                    let w_lo = (byte & 0x0F) as i64 - 8;
                    let w_hi = ((byte >> 4) as i64) - 8;
                    acc += w_lo * input[j * 2] + w_hi * input[j * 2 + 1];
                }
                *out = (acc * scales[i]) >> FRAC_BITS;
            }
        });
}

pub fn layernorm(input: &[i64], gamma: &[i64]) -> Vec<i64> {
    let n = input.len() as i64;
    if n == 0 {
        return vec![];
    }
    let mut sq_sum: i128 = 0;
    for &x in input {
        sq_sum += (x as i128) * (x as i128);
    }
    let mean_sq_q32 = sq_sum / (n as i128);
    let mean_sq = (mean_sq_q32 >> FRAC_BITS as i128) as i64;
    let inv_rms = integer_isqrt(mean_sq + 1);
    input
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let norm = (x * inv_rms) >> FRAC_BITS;
            let g = if i < gamma.len() { gamma[i] } else { ONE };
            (norm * g) >> FRAC_BITS
        })
        .collect()
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

pub fn apply_rope_interleaved(
    vec: &mut [i64],
    pos: usize,
    d_head: usize,
    cos: &[i64],
    sin: &[i64],
) {
    let half = d_head / 2;
    for i in 0..half {
        let cos_val = cos[pos * half + i];
        let sin_val = sin[pos * half + i];
        let even = 2 * i;
        let odd = even + 1;
        let x0 = vec[even];
        let x1 = vec[odd];
        vec[even] = ((x0 * cos_val) >> FRAC_BITS) - ((x1 * sin_val) >> FRAC_BITS);
        vec[odd] = ((x0 * sin_val) >> FRAC_BITS) + ((x1 * cos_val) >> FRAC_BITS);
    }
}

fn permute_interleaved_rows_to_split_half(
    weights: &mut I8Weights,
    n_heads: usize,
    d_head: usize,
    matrix_name: &str,
) -> Result<(), crate::InferenceError> {
    if d_head == 0 || !d_head.is_multiple_of(2) || n_heads == 0 {
        return Err(crate::InferenceError::Runtime(format!(
            "{matrix_name}: RoPE row permutation requires a nonzero even head dimension and head count"
        )));
    }
    let expected_rows = n_heads.checked_mul(d_head).ok_or_else(|| {
        crate::InferenceError::Runtime(format!("{matrix_name}: RoPE row count overflow"))
    })?;
    if weights.n_rows != expected_rows
        || weights.scales.len() != expected_rows
        || weights.data.len() != expected_rows.saturating_mul(weights.n_cols)
    {
        return Err(crate::InferenceError::Runtime(format!(
            "{matrix_name}: expected {expected_rows} complete output rows for {n_heads} heads of width {d_head}, got rows={}, cols={}, scales={}, data={}",
            weights.n_rows,
            weights.n_cols,
            weights.scales.len(),
            weights.data.len(),
        )));
    }

    let source_data = weights.data.clone();
    let source_scales = weights.scales.clone();
    let half = d_head / 2;
    for head in 0..n_heads {
        let base = head * d_head;
        for pair in 0..half {
            for (destination, source) in [
                (base + pair, base + 2 * pair),
                (base + half + pair, base + 2 * pair + 1),
            ] {
                let dst = destination * weights.n_cols;
                let src = source * weights.n_cols;
                weights.data[dst..dst + weights.n_cols]
                    .copy_from_slice(&source_data[src..src + weights.n_cols]);
                weights.scales[destination] = source_scales[source];
            }
        }
    }
    Ok(())
}

pub struct InterleavedRopeQk {
    layers: Vec<Option<(I8Weights, I8Weights)>>,
}

impl InterleavedRopeQk {
    pub fn from_resident_layers(model: &CachedIntegerModel) -> Result<Self, crate::InferenceError> {
        if model.config.arithmetic_profile != ArithmeticProfile::LegacySplitHalfV0 {
            return Err(crate::InferenceError::Runtime(
                "an interleaved-RoPE Q/K copy is built from the legacy split-half model".into(),
            ));
        }
        if !model.has_canonical_i8_profile() {
            return Err(crate::InferenceError::Runtime(
                "an interleaved-RoPE Q/K copy requires the canonical per-row INT8 profile".into(),
            ));
        }
        let copy = |weights: &I8Weights| I8Weights {
            data: weights.data.clone(),
            scales: weights.scales.clone(),
            n_rows: weights.n_rows,
            n_cols: weights.n_cols,
        };
        let mut layers = Vec::with_capacity(model.layers.len());
        for layer in &model.layers {
            if !layer.is_loaded() {
                layers.push(None);
                continue;
            }
            let mut wq = copy(&layer.wq);
            let mut wk = copy(&layer.wk);
            permute_interleaved_rows_to_split_half(
                &mut wq,
                model.config.n_heads,
                model.config.d_head,
                "wq",
            )?;
            permute_interleaved_rows_to_split_half(
                &mut wk,
                model.config.n_kv_heads,
                model.config.d_head,
                "wk",
            )?;
            layers.push(Some((wq, wk)));
        }
        if layers.iter().all(Option::is_none) {
            return Err(crate::InferenceError::Runtime(
                "an interleaved-RoPE Q/K copy needs at least one resident layer".into(),
            ));
        }
        Ok(Self { layers })
    }

    pub fn covers(&self, start: usize, end: usize) -> bool {
        start < end
            && end <= self.layers.len()
            && self.layers[start..end].iter().all(Option::is_some)
    }

    pub fn layer_count(&self) -> usize {
        self.layers.iter().filter(|layer| layer.is_some()).count()
    }

    pub fn memory_bytes(&self) -> usize {
        self.layers
            .iter()
            .flatten()
            .map(|(wq, wk)| wq.memory_bytes() + wk.memory_bytes())
            .sum()
    }

    pub fn weight_bytes(&self) -> usize {
        self.layers
            .iter()
            .flatten()
            .map(|(wq, wk)| {
                wq.data.len()
                    + wk.data.len()
                    + (wq.scales.len() + wk.scales.len()) * std::mem::size_of::<i64>()
            })
            .sum()
    }

    fn layer(&self, layer: usize) -> Option<(&I8Weights, &I8Weights)> {
        self.layers
            .get(layer)
            .and_then(Option::as_ref)
            .map(|(wq, wk)| (wq, wk))
    }
}

pub fn silu_i64(x: i64) -> i64 {
    let sig = if x >= 0 {
        let exp_neg = integer_exp(-x);
        (ONE * ONE) / (ONE + exp_neg).max(1)
    } else {
        let exp_pos = integer_exp(x);
        (exp_pos * ONE) / (ONE + exp_pos).max(1)
    };
    (x * sig) >> FRAC_BITS
}

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
    output
        .par_chunks_mut(64)
        .enumerate()
        .for_each(|(chunk_idx, chunk)| {
            let start = chunk_idx * 64;
            for (local_i, out) in chunk.iter_mut().enumerate() {
                let i = start + local_i;
                let row = &data[i * in_size..(i + 1) * in_size];
                let mut acc: i64 = 0;
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

#[allow(dead_code)]
#[inline]
fn quantize_for_dot(v: &[i64]) -> (Vec<i8>, i64) {
    let abs_max = v.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let sf = (abs_max / 127).max(1);
    let data: Vec<i8> = v.iter().map(|&x| (x / sf).clamp(-127, 127) as i8).collect();
    (data, sf)
}

#[allow(dead_code)]
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn dot_i8_kv_neon(q_i8: *const i8, k_ptr: *const i8, d_head: usize) -> i32 {
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

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn dot_i8_kv_avx2(q_i8: *const i8, k_ptr: *const i8, d_head: usize) -> i32 {
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

#[allow(dead_code)]
#[inline]
fn dot_i8_kv(q_i8: &[i8], k_ptr: &[i8], k_offset: usize, d_head: usize) -> i32 {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe { dot_i8_kv_neon(q_i8.as_ptr(), k_ptr.as_ptr().add(k_offset), d_head) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        unsafe { dot_i8_kv_avx2(q_i8.as_ptr(), k_ptr.as_ptr().add(k_offset), d_head) }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let mut acc: i32 = 0;
        for dd in 0..d_head {
            acc += (q_i8[dd] as i32) * (k_ptr[k_offset + dd] as i32);
        }
        acc
    }
}

#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
fn flash_attention_i8(
    q_head: &[i64],
    k_data: &[i8],
    k_scales: &[i64],
    v_data: &[i8],
    v_scales: &[i64],
    d_kv: usize,
    kv_h: usize,
    d_head: usize,
    full_seq: usize,
    attn_scale: i64,
) -> Vec<i64> {
    let mut running_max: i64 = -8 * ONE;
    let mut running_sum: i64 = 0;
    let mut out = vec![0i64; d_head];

    let (q_i8, q_sf) = quantize_for_dot(q_head);

    for j in 0..full_seq {
        let k_off = j * d_kv + kv_h * d_head;
        let k_scale = k_scales[j];

        let dot_i32 = dot_i8_kv(&q_i8, k_data, k_off, d_head);
        let dot = (dot_i32 as i64) * q_sf * k_scale;
        let score = ((dot >> (FRAC_BITS * 2)) * attn_scale) >> FRAC_BITS;

        if score > running_max {
            let diff = running_max - score;
            let correction = integer_exp(diff);
            running_sum = (running_sum * correction) >> FRAC_BITS;
            for o in out.iter_mut() {
                *o = (*o * correction) >> FRAC_BITS;
            }
            running_max = score;
        }

        let w = integer_exp(score - running_max);
        running_sum += w;

        let v_off = j * d_kv + kv_h * d_head;
        let v_scale = v_scales[j];
        for dd in 0..d_head {
            let v_val = (v_data[v_off + dd] as i64) * v_scale;
            out[dd] += (w * v_val) >> FRAC_BITS;
        }
    }

    if running_sum > 0 {
        for o in out.iter_mut() {
            *o = (*o * ONE) / running_sum;
        }
    }

    out
}

#[allow(clippy::too_many_arguments)]
#[inline]
pub(crate) fn flash_attention_i64(
    q_head: &[i64],
    k_cache: &[i64],
    v_cache: &[i64],
    d_kv: usize,
    kv_h: usize,
    d_head: usize,
    full_seq: usize,
    attn_scale: i64,
) -> Vec<i64> {
    let mut running_max: i64 = i64::MIN / 2;
    let mut running_sum: i64 = 0;
    let mut out = vec![0i64; d_head];

    for j in 0..full_seq {
        let k_off = j * d_kv + kv_h * d_head;

        let dot =
            unsafe { dot_i64xi64_attn_neon(q_head.as_ptr(), k_cache.as_ptr().add(k_off), d_head) };
        let score = ((dot >> FRAC_BITS) * attn_scale) >> FRAC_BITS;

        if score > running_max {
            let diff = running_max - score;
            let correction = integer_exp(diff);
            running_sum = (running_sum * correction) >> FRAC_BITS;
            for o in out.iter_mut() {
                *o = (*o * correction) >> FRAC_BITS;
            }
            running_max = score;
        }

        let w = integer_exp(score - running_max);
        running_sum += w;

        let v_off = j * d_kv + kv_h * d_head;
        for dd in 0..d_head {
            out[dd] += (w * v_cache[v_off + dd]) >> FRAC_BITS;
        }
    }

    if running_sum > 0 {
        for o in out.iter_mut() {
            *o = (*o * ONE) / running_sum;
        }
    }

    out
}

impl I8Weights {
    pub fn write_to(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        w.write_all(&(self.n_rows as u64).to_le_bytes())?;
        w.write_all(&(self.n_cols as u64).to_le_bytes())?;
        for &s in &self.scales {
            w.write_all(&s.to_le_bytes())?;
        }
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(self.data.as_ptr() as *const u8, self.data.len()) };
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
        Ok(Self {
            data,
            scales,
            n_rows,
            n_cols,
        })
    }
}

fn write_i64_vec(w: &mut impl std::io::Write, v: &[i64]) -> std::io::Result<()> {
    w.write_all(&(v.len() as u64).to_le_bytes())?;
    for &x in v {
        w.write_all(&x.to_le_bytes())?;
    }
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

impl CachedIntegerModel {
    pub fn memory_bytes(&self) -> usize {
        let mut total = self.embedding_i8.memory_bytes()
            + self.embedding_q16.len() * 8
            + self.output_weight.memory_bytes()
            + self.final_norm.len() * 8
            + self.config.rope_cos.len() * 8 * 2;
        for layer in &self.layers {
            total += layer.wq.memory_bytes()
                + layer.wk.memory_bytes()
                + layer.wv.memory_bytes()
                + layer.wo.memory_bytes()
                + layer.w_gate.memory_bytes()
                + layer.w_up.memory_bytes()
                + layer.w_down.memory_bytes()
                + (layer.attn_norm.len() + layer.ffn_norm.len()) * 8;
        }
        total
    }

    pub fn decode(&self, tokens: &[u32]) -> String {
        tokens
            .iter()
            .map(|&id| {
                if (id as usize) < self.vocab.len() {
                    self.vocab[id as usize].replace('▁', " ")
                } else {
                    format!("[{}]", id)
                }
            })
            .collect::<String>()
    }

    pub fn decode_v2(&self, tokens: &[u32]) -> String {
        let mut text = String::new();
        let mut fallback = Vec::new();
        let flush = |text: &mut String, fallback: &mut Vec<u8>| {
            if !fallback.is_empty() {
                text.push_str(&String::from_utf8_lossy(fallback));
                fallback.clear();
            }
        };
        for &id in tokens {
            let Some(piece) = self.vocab.get(id as usize) else {
                flush(&mut text, &mut fallback);
                text.push_str(&format!("[{id}]"));
                continue;
            };
            if let Some(hex) = piece
                .strip_prefix("<0x")
                .and_then(|value| value.strip_suffix('>'))
                && hex.len() == 2
                && let Ok(byte) = u8::from_str_radix(hex, 16)
            {
                fallback.push(byte);
                continue;
            }
            flush(&mut text, &mut fallback);
            text.push_str(&piece.replace('▁', " "));
        }
        flush(&mut text, &mut fallback);
        text
    }

    pub fn decode_v2_content(&self, tokens: &[u32]) -> String {
        let mut text = String::new();
        let mut fallback = Vec::new();
        let flush = |text: &mut String, fallback: &mut Vec<u8>| {
            if !fallback.is_empty() {
                text.push_str(&String::from_utf8_lossy(fallback));
                fallback.clear();
            }
        };
        for &id in tokens {
            if self.config.eos_tokens.contains(&id) {
                break;
            }
            if id == self.config.bos_token {
                continue;
            }
            let Some(piece) = self.vocab.get(id as usize) else {
                flush(&mut text, &mut fallback);
                text.push_str(&format!("[{id}]"));
                continue;
            };
            if matches!(piece.as_str(), "<s>" | "</s>") {
                continue;
            }
            if let Some(hex) = piece
                .strip_prefix("<0x")
                .and_then(|value| value.strip_suffix('>'))
                && hex.len() == 2
                && let Ok(byte) = u8::from_str_radix(hex, 16)
            {
                fallback.push(byte);
                continue;
            }
            flush(&mut text, &mut fallback);
            text.push_str(&piece.replace('▁', " "));
        }
        flush(&mut text, &mut fallback);
        text
    }

    pub fn encode(&self, text: &str) -> Vec<u32> {
        if self.vocab.is_empty() {
            return vec![];
        }
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
                    && let Some(id) = self.vocab.iter().position(|v| v == candidate)
                {
                    best_len = try_len;
                    best_id = id as u32;
                    break;
                }
            }
            if best_len > 0 {
                tokens.push(best_id);
                pos += best_len;
            } else {
                let char_len = match bytes[pos] {
                    0x00..=0x7F => 1,
                    0xC0..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    0xF0..=0xF7 => 4,
                    _ => 1,
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

    pub fn apply_chat_template(&self, user_input: &str) -> String {
        let tmpl = &self.config.chat_template;
        if tmpl.is_empty() {
            return user_input.to_string();
        }

        if tmpl.contains("[INST]") {
            format!("[INST] {} [/INST]", user_input)
        } else if tmpl.contains("<|start_header_id|>") {
            format!(
                "<|begin_of_text|><|start_header_id|>user<|end_header_id|>\n\n{}<|eot_id|><|start_header_id|>assistant<|end_header_id|>\n\n",
                user_input
            )
        } else if tmpl.contains("<|im_start|>") {
            format!(
                "<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
                user_input
            )
        } else if tmpl.contains("<|user|>") {
            format!("<|user|>\n{}<|end|>\n<|assistant|>\n", user_input)
        } else if tmpl.contains("[turn_start]") {
            format!(
                "<start_of_turn>user\n{}<end_of_turn>\n<start_of_turn>model\n",
                user_input
            )
        } else {
            user_input.to_string()
        }
    }

    pub fn forward_one_token(&self, token: u32, cache: &mut KVCache) -> Vec<i64> {
        self.forward_one_token_observed(token, cache, |_, _, _| {})
    }

    pub fn forward_one_token_observed<F>(
        &self,
        token: u32,
        cache: &mut KVCache,
        mut on_layer: F,
    ) -> Vec<i64>
    where
        F: FnMut(usize, usize, &[i64]),
    {
        let cfg = &self.config;
        let d = cfg.d_model;
        let pos = cache.seq_len;

        if self.embedding_q16.len() < (token as usize + 1) * d {
            return Vec::new();
        }

        macro_rules! dispatch_matmul {
            ($hyb:expr, $tern:expr, $blk:expr, $i16w:expr, $q4w:expr, $i8w:expr, $inq:expr, $raw:expr, $in_sz:expr, $out:expr) => {{
                if let Some(hw) = $hyb {
                    crate::ternary_hybrid::matmul_ternary_hybrid_into(hw, $raw, $in_sz, $out);
                } else if let Some(tw) = $tern {
                    crate::ternary_engine::matmul_ternary_into(tw, $raw, $in_sz, $out);
                } else if let Some(i16w) = $i16w {
                    matmul_i16_into(i16w, $raw, $in_sz, $out);
                } else if let Some(blk) = $blk {
                    crate::block_i8::matmul_block_i8_into(blk, $raw, $out);
                } else if let Some(q4w) = $q4w {
                    matmul_q4_full(q4w, $raw, $out);
                } else {
                    matmul_fast_preq($i8w, $inq, $raw, $in_sz, $out);
                }
            }};
        }

        let idx = (token as usize).min(cfg.vocab_size - 1);
        let emb_start = idx * d;
        let mut hidden: Vec<i64> = self.embedding_q16[emb_start..emb_start + d].to_vec();

        let mut q = vec![0i64; d];
        let mut k_buf = vec![0i64; cfg.d_kv];
        let mut v_buf = vec![0i64; cfg.d_kv];
        let mut attn_out = vec![0i64; d];
        let mut projected = vec![0i64; d];
        let mut gate = vec![0i64; cfg.d_ff];
        let mut up = vec![0i64; cfg.d_ff];
        let mut ff_out = vec![0i64; d];

        for (layer_idx, layer) in self.layers.iter().enumerate() {
            let hyb_layer = self.ternary_hybrid_layers.as_ref().map(|hl| &hl[layer_idx]);
            let tern_layer = self.ternary_layers.as_ref().map(|tl| &tl[layer_idx]);
            let blk_layer = self.block_i8_layers.as_ref().map(|bl| &bl[layer_idx]);
            let i16_layer = self.i16_layers.as_ref().map(|il| &il[layer_idx]);
            let q4_layer = self.q4_layers.as_ref().map(|ql| &ql[layer_idx]);

            let normed = layernorm(&hidden, &layer.attn_norm);

            let normed_q = QuantizedInput::from_i64(&normed);

            dispatch_matmul!(
                hyb_layer.map(|l| &l.wq),
                tern_layer.map(|l| &l.wq),
                blk_layer.map(|l| &l.wq),
                i16_layer.map(|l| &l.wq),
                q4_layer.map(|l| &l.wq),
                &layer.wq,
                &normed_q,
                &normed,
                d,
                &mut q
            );
            dispatch_matmul!(
                hyb_layer.map(|l| &l.wk),
                tern_layer.map(|l| &l.wk),
                blk_layer.map(|l| &l.wk),
                i16_layer.map(|l| &l.wk),
                q4_layer.map(|l| &l.wk),
                &layer.wk,
                &normed_q,
                &normed,
                d,
                &mut k_buf
            );
            dispatch_matmul!(
                hyb_layer.map(|l| &l.wv),
                tern_layer.map(|l| &l.wv),
                blk_layer.map(|l| &l.wv),
                i16_layer.map(|l| &l.wv),
                q4_layer.map(|l| &l.wv),
                &layer.wv,
                &normed_q,
                &normed,
                d,
                &mut v_buf
            );

            for h in 0..cfg.n_heads {
                apply_rope(
                    &mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos,
                    cfg.d_head,
                    &cfg.rope_cos,
                    &cfg.rope_sin,
                );
            }
            for h in 0..cfg.n_kv_heads {
                apply_rope(
                    &mut k_buf[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos,
                    cfg.d_head,
                    &cfg.rope_cos,
                    &cfg.rope_sin,
                );
            }

            cache.push_k(layer_idx, &k_buf);
            cache.push_v(layer_idx, &v_buf);

            let full_seq = pos + 1;
            let k_layer_data = &cache.k_data[layer_idx];
            let v_layer_data = &cache.v_data[layer_idx];
            let head_results: Vec<Vec<i64>> = (0..cfg.n_heads)
                .into_par_iter()
                .map(|h| {
                    let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                    let dh = cfg.d_head;
                    let q_head = &q[h * dh..(h + 1) * dh];
                    flash_attention_i64(
                        q_head,
                        k_layer_data,
                        v_layer_data,
                        cfg.d_kv,
                        kv_h,
                        dh,
                        full_seq,
                        cfg.attn_scale,
                    )
                })
                .collect();

            for val in attn_out.iter_mut() {
                *val = 0;
            }
            for (h, head_out) in head_results.iter().enumerate() {
                attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(head_out);
            }

            let attn_out_q = QuantizedInput::from_i64(&attn_out);
            dispatch_matmul!(
                hyb_layer.map(|l| &l.wo),
                tern_layer.map(|l| &l.wo),
                blk_layer.map(|l| &l.wo),
                i16_layer.map(|l| &l.wo),
                q4_layer.map(|l| &l.wo),
                &layer.wo,
                &attn_out_q,
                &attn_out,
                d,
                &mut projected
            );
            for i in 0..d {
                hidden[i] += projected[i];
            }

            let normed_ff = layernorm(&hidden, &layer.ffn_norm);
            let normed_ff_q = QuantizedInput::from_i64(&normed_ff);

            dispatch_matmul!(
                hyb_layer.map(|l| &l.w_gate),
                tern_layer.map(|l| &l.w_gate),
                blk_layer.map(|l| &l.w_gate),
                i16_layer.map(|l| &l.w_gate),
                q4_layer.map(|l| &l.w_gate),
                &layer.w_gate,
                &normed_ff_q,
                &normed_ff,
                d,
                &mut gate
            );
            dispatch_matmul!(
                hyb_layer.map(|l| &l.w_up),
                tern_layer.map(|l| &l.w_up),
                blk_layer.map(|l| &l.w_up),
                i16_layer.map(|l| &l.w_up),
                q4_layer.map(|l| &l.w_up),
                &layer.w_up,
                &normed_ff_q,
                &normed_ff,
                d,
                &mut up
            );

            for j in 0..cfg.d_ff {
                gate[j] = (silu_i64(gate[j]) * up[j]) >> FRAC_BITS;
            }

            let gate_q = QuantizedInput::from_i64(&gate);
            dispatch_matmul!(
                hyb_layer.map(|l| &l.w_down),
                tern_layer.map(|l| &l.w_down),
                blk_layer.map(|l| &l.w_down),
                i16_layer.map(|l| &l.w_down),
                q4_layer.map(|l| &l.w_down),
                &layer.w_down,
                &gate_q,
                &gate,
                cfg.d_ff,
                &mut ff_out
            );
            for i in 0..d {
                hidden[i] += ff_out[i];
            }
            on_layer(pos, layer_idx, &hidden);
        }

        cache.seq_len = pos + 1;
        let normed = layernorm(&hidden, &self.final_norm);

        if let Some(i16_out) = &self.i16_output {
            let mut logits = vec![0i64; cfg.vocab_size];
            matmul_i16_into(i16_out, &normed, d, &mut logits);
            return logits;
        }
        if let Some(blk_out) = &self.block_i8_output
            && blk_out.n_rows > 0
        {
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

    pub fn prefill_canonical_i8_batched(
        &self,
        tokens: &[u32],
        cache: &mut KVCache,
        chunk_size: usize,
        all_positions: bool,
    ) -> Option<Vec<Vec<i64>>> {
        self.prefill_canonical_i8_batched_observed(
            tokens,
            cache,
            chunk_size,
            all_positions,
            |_, _, _| {},
        )
    }

    pub fn prefill_canonical_i8_batched_observed<F>(
        &self,
        tokens: &[u32],
        cache: &mut KVCache,
        chunk_size: usize,
        all_positions: bool,
        mut on_layer: F,
    ) -> Option<Vec<Vec<i64>>>
    where
        F: FnMut(usize, usize, &[i64]),
    {
        use crate::canonical_prefill::{PrefillRefusal, record_chunk, record_prefill_refusal};
        let cfg = &self.config;
        let (d, dkv, dff, dh) = (cfg.d_model, cfg.d_kv, cfg.d_ff, cfg.d_head);
        if !self.has_canonical_i8_profile() || !self.has_all_transformer_layers() {
            record_prefill_refusal(PrefillRefusal::NotCanonicalProfile);
            return None;
        }
        if tokens.is_empty()
            || chunk_size == 0
            || d == 0
            || dh == 0
            || cfg.n_heads == 0
            || cfg.n_kv_heads == 0
            || cfg.vocab_size == 0
            || cache.k_data.len() != cfg.n_layers
            || cache.v_data.len() != cfg.n_layers
            || self.layers.len() != cfg.n_layers
        {
            record_prefill_refusal(PrefillRefusal::Shape);
            return None;
        }
        if tokens
            .iter()
            .any(|t| self.embedding_q16.len() < (*t as usize + 1).saturating_mul(d))
        {
            record_prefill_refusal(PrefillRefusal::Shape);
            return None;
        }
        let base = cache.seq_len;
        if base
            .checked_add(tokens.len())
            .is_none_or(|end| end > cfg.max_seq)
        {
            record_prefill_refusal(PrefillRefusal::ContextWindowExceeded);
            return None;
        }
        let chunk_size = chunk_size
            .min(crate::canonical_simd::MAX_BATCH_TOKENS)
            .min(crate::canonical_prefill::max_chunk_within_scratch_budget(
                d, dkv, dff,
            ))
            .max(1);

        let total = tokens.len();
        let mut out: Vec<Vec<i64>> = Vec::new();
        let mut done = 0usize;
        while done < total {
            let t_n = chunk_size.min(total - done);
            let chunk = &tokens[done..done + t_n];

            let mut hidden = vec![0i64; t_n * d];
            for (ti, &tok) in chunk.iter().enumerate() {
                let idx = (tok as usize).min(cfg.vocab_size - 1);
                hidden[ti * d..(ti + 1) * d]
                    .copy_from_slice(&self.embedding_q16[idx * d..(idx + 1) * d]);
            }
            self.forward_layers_batched(
                &mut hidden,
                t_n,
                base + done,
                0..cfg.n_layers,
                cache,
                &mut on_layer,
            );
            cache.seq_len = base + done + t_n;

            let want: Vec<usize> = if all_positions {
                (0..t_n).collect()
            } else if done + t_n == total {
                vec![t_n - 1]
            } else {
                Vec::new()
            };
            if !want.is_empty() {
                let cnt = want.len();
                let mut fin = vec![0i64; cnt * d];
                for (o, &ti) in want.iter().enumerate() {
                    fin[o * d..(o + 1) * d].copy_from_slice(&layernorm(
                        &hidden[ti * d..(ti + 1) * d],
                        &self.final_norm,
                    ));
                }
                let mut logits = vec![0i64; cnt * cfg.vocab_size];
                matmul_i8_into_batched(&self.output_weight, &fin, cnt, d, &mut logits);
                for o in 0..cnt {
                    out.push(logits[o * cfg.vocab_size..(o + 1) * cfg.vocab_size].to_vec());
                }
            }
            record_chunk(t_n, 7 * cfg.n_layers + usize::from(!want.is_empty()));
            done += t_n;
        }
        Some(out)
    }

    fn forward_layers_batched<F>(
        &self,
        hidden: &mut [i64],
        t_n: usize,
        first_position: usize,
        layers: std::ops::Range<usize>,
        cache: &mut KVCache,
        on_layer: &mut F,
    ) where
        F: FnMut(usize, usize, &[i64]),
    {
        let spans = [SeqSpan {
            first_row: 0,
            rows: t_n,
            first_position,
        }];
        let mut caches = [cache];
        self.forward_layers_multi(
            hidden,
            &spans,
            layers,
            &mut caches,
            &mut |_: usize, position: usize, layer: usize, row: &[i64]| {
                on_layer(position, layer, row)
            },
        );
    }

    fn forward_layers_multi<F>(
        &self,
        hidden: &mut [i64],
        spans: &[SeqSpan],
        layers: std::ops::Range<usize>,
        caches: &mut [&mut KVCache],
        on_layer: &mut F,
    ) where
        F: FnMut(usize, usize, usize, &[i64]),
    {
        let cfg = &self.config;
        let (d, dkv, dff, dh) = (cfg.d_model, cfg.d_kv, cfg.d_ff, cfg.d_head);
        let row_sequence: Vec<usize> = spans
            .iter()
            .enumerate()
            .flat_map(|(sequence, span)| std::iter::repeat_n(sequence, span.rows))
            .collect();
        let row_position: Vec<usize> = spans
            .iter()
            .flat_map(|span| span.first_position..span.first_position + span.rows)
            .collect();
        let t_n = row_position.len();
        let mut normed = vec![0i64; t_n * d];
        let mut q = vec![0i64; t_n * d];
        let mut k = vec![0i64; t_n * dkv];
        let mut v = vec![0i64; t_n * dkv];
        let mut attn = vec![0i64; t_n * d];
        let mut proj = vec![0i64; t_n * d];
        let mut gate = vec![0i64; t_n * dff];
        let mut up = vec![0i64; t_n * dff];
        let mut ffo = vec![0i64; t_n * d];

        for li in layers {
            let layer = &self.layers[li];
            normed
                .par_chunks_mut(d)
                .zip(hidden.par_chunks(d))
                .for_each(|(out, row)| out.copy_from_slice(&layernorm(row, &layer.attn_norm)));
            matmul_i8_into_batched(&layer.wq, &normed, t_n, d, &mut q);
            matmul_i8_into_batched(&layer.wk, &normed, t_n, d, &mut k);
            matmul_i8_into_batched(&layer.wv, &normed, t_n, d, &mut v);

            q.par_chunks_mut(d)
                .zip(k.par_chunks_mut(dkv))
                .zip(row_position.par_iter())
                .for_each(|((q_row, k_row), &pos)| {
                    for h in 0..cfg.n_heads {
                        apply_rope(
                            &mut q_row[h * dh..(h + 1) * dh],
                            pos,
                            dh,
                            &cfg.rope_cos,
                            &cfg.rope_sin,
                        );
                    }
                    for h in 0..cfg.n_kv_heads {
                        apply_rope(
                            &mut k_row[h * dh..(h + 1) * dh],
                            pos,
                            dh,
                            &cfg.rope_cos,
                            &cfg.rope_sin,
                        );
                    }
                });
            for (span, cache) in spans.iter().zip(caches.iter_mut()) {
                let rows = span.first_row..span.first_row + span.rows;
                for (k_row, v_row) in k[rows.start * dkv..rows.end * dkv]
                    .chunks_exact(dkv)
                    .zip(v[rows.start * dkv..rows.end * dkv].chunks_exact(dkv))
                {
                    cache.push_k(li, k_row);
                    cache.push_v(li, v_row);
                }
            }

            {
                let kv: Vec<(&[i64], &[i64])> = caches
                    .iter()
                    .map(|cache| (cache.k_data[li].as_slice(), cache.v_data[li].as_slice()))
                    .collect();
                let heads = cfg.n_heads;
                let results: Vec<Vec<i64>> = (0..t_n * heads)
                    .into_par_iter()
                    .map(|x| {
                        let ti = x / heads;
                        let h = x % heads;
                        let (kd, vd) = kv[row_sequence[ti]];
                        let pos = row_position[ti];
                        let kv_h = h * cfg.n_kv_heads / heads;
                        flash_attention_i64(
                            &q[ti * d + h * dh..ti * d + (h + 1) * dh],
                            kd,
                            vd,
                            dkv,
                            kv_h,
                            dh,
                            pos + 1,
                            cfg.attn_scale,
                        )
                    })
                    .collect();
                for (x, r) in results.iter().enumerate() {
                    let ti = x / heads;
                    let h = x % heads;
                    attn[ti * d + h * dh..ti * d + (h + 1) * dh].copy_from_slice(r);
                }
            }

            matmul_i8_into_batched(&layer.wo, &attn, t_n, d, &mut proj);
            hidden
                .par_iter_mut()
                .zip(proj.par_iter())
                .for_each(|(h, p)| *h += p);
            normed
                .par_chunks_mut(d)
                .zip(hidden.par_chunks(d))
                .for_each(|(out, row)| out.copy_from_slice(&layernorm(row, &layer.ffn_norm)));
            matmul_i8_into_batched(&layer.w_gate, &normed, t_n, d, &mut gate);
            matmul_i8_into_batched(&layer.w_up, &normed, t_n, d, &mut up);
            gate.par_chunks_mut(SILU_CHUNK)
                .zip(up.par_chunks(SILU_CHUNK))
                .for_each(|(gate, up)| {
                    for (g, &u) in gate.iter_mut().zip(up) {
                        *g = (silu_i64(*g) * u) >> FRAC_BITS;
                    }
                });
            matmul_i8_into_batched(&layer.w_down, &gate, t_n, dff, &mut ffo);
            hidden
                .par_iter_mut()
                .zip(ffo.par_iter())
                .for_each(|(h, f)| *h += f);
            for (ti, row) in hidden.chunks_exact(d).enumerate() {
                on_layer(row_sequence[ti], row_position[ti], li, row);
            }
        }
    }

    pub fn forward_one_token_canonical_i8_with_backend(
        &self,
        token: u32,
        cache: &mut KVCache,
        call_id: Hash256,
        backend: &impl crate::tensor_parallel::ProjectionBackend,
    ) -> Result<Vec<i64>, crate::tensor_parallel::TensorParallelError> {
        if !self.has_all_transformer_layers() {
            return Err(crate::tensor_parallel::TensorParallelError::WrongIdentity);
        }
        forward_canonical_with_backend(self, token, cache, call_id, backend)
    }

    pub fn try_generate(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> Result<(Vec<u32>, Hash256), GenerationError> {
        let _admission = self.preflight_generation(prompt.len(), max_tokens)?;
        Ok(self.generate_preflighted(prompt, max_tokens, eos_tokens))
    }

    pub fn generate(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> (Vec<u32>, Hash256) {
        self.try_generate(prompt, max_tokens, eos_tokens)
            .expect("trusted generation request must fit the model context window")
    }

    pub fn try_generate_v2(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> Result<(Vec<u32>, Hash256), GenerationError> {
        let _admission = self.preflight_generation(prompt.len(), max_tokens)?;
        Ok(self.generate_preflighted_v2(prompt, max_tokens, eos_tokens))
    }

    pub fn generate_v2(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> (Vec<u32>, Hash256) {
        self.try_generate_v2(prompt, max_tokens, eos_tokens)
            .expect("trusted v2 generation request must fit the model context window")
    }

    pub fn try_generate_v2_greedy(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> Result<(Vec<u32>, Hash256), GenerationError> {
        let _admission = self.preflight_generation(prompt.len(), max_tokens)?;
        Ok(self.generate_preflighted_v2_with_sampling(prompt, max_tokens, eos_tokens, false))
    }

    pub fn try_generate_v2_with_backend(
        &self,
        request: Hash256,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
        backend: &impl crate::tensor_parallel::ProjectionBackend,
    ) -> Result<(Vec<u32>, Hash256), BackendGenerationError> {
        if !self.has_all_transformer_layers() {
            return Err(crate::tensor_parallel::TensorParallelError::WrongIdentity.into());
        }
        generate_canonical_with_backend(self, request, prompt, max_tokens, eos_tokens, backend)
    }

    fn prefill_prompt_into_cache(&self, prompt: &[u32], cache: &mut KVCache) -> Option<Vec<i64>> {
        if prompt.is_empty() {
            return None;
        }
        if crate::canonical_prefill::batched_prefill_enabled()
            && crate::canonical_prefill::batching_is_profitable(prompt.len())
        {
            let cfg = &self.config;
            let chunk = crate::canonical_prefill::SERVING_PREFILL_CHUNK.min(
                crate::canonical_prefill::max_chunk_within_scratch_budget(
                    cfg.d_model,
                    cfg.d_kv,
                    cfg.d_ff,
                ),
            );
            if let Some(mut out) = self.prefill_canonical_i8_batched(prompt, cache, chunk, false)
                && let Some(last) = out.pop()
            {
                return Some(last);
            }
        }
        let mut logits = Vec::new();
        for &tok in prompt {
            logits = self.forward_one_token(tok, cache);
        }
        Some(logits)
    }

    fn generate_preflighted(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> (Vec<u32>, Hash256) {
        let mut cache = KVCache::new(self.config.n_layers);
        let mut generated = Vec::new();

        let _ = self.forward_one_token(self.config.bos_token, &mut cache);

        let _ = self.prefill_prompt_into_cache(prompt, &mut cache);

        for _ in 0..max_tokens {
            let last_token = generated
                .last()
                .copied()
                .unwrap_or(*prompt.last().unwrap_or(&0));
            let mut logits = self.forward_one_token(last_token, &mut cache);
            let next = select_next_token_with_repetition_penalty(&mut logits, &generated);
            generated.push(next);
            if eos_tokens.contains(&next) {
                break;
            }
        }

        let output_bytes: Vec<u8> = generated.iter().flat_map(|t| t.to_le_bytes()).collect();
        let hash = arc_crypto::hash_bytes(&output_bytes);
        (generated, hash)
    }

    fn generate_preflighted_v2(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> (Vec<u32>, Hash256) {
        self.generate_preflighted_v2_with_sampling(prompt, max_tokens, eos_tokens, true)
    }

    fn generate_preflighted_v2_with_sampling(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
        repetition_penalty: bool,
    ) -> (Vec<u32>, Hash256) {
        let mut cache = KVCache::new(self.config.n_layers);
        let mut logits = self.forward_one_token(self.config.bos_token, &mut cache);
        if let Some(last) = self.prefill_prompt_into_cache(prompt, &mut cache) {
            logits = last;
        }

        let mut generated = Vec::new();
        for _ in 0..max_tokens {
            let next = if repetition_penalty {
                select_next_token_with_repetition_penalty(&mut logits, &generated)
            } else {
                argmax_i64(&logits) as u32
            };
            generated.push(next);
            if eos_tokens.contains(&next) {
                break;
            }
            logits = self.forward_one_token(next, &mut cache);
        }

        let output_bytes: Vec<u8> = generated.iter().flat_map(|t| t.to_le_bytes()).collect();
        let hash = arc_crypto::hash_bytes(&output_bytes);
        (generated, hash)
    }

    pub fn save_weights(&self, path: &str) -> std::io::Result<()> {
        use std::io::Write;
        if self.config.arithmetic_profile != ArithmeticProfile::LegacySplitHalfV0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ARC-INT8 v2 cache has no arithmetic-profile tag; refusing to serialize GGUF interleaved-RoPE rows as a legacy cache",
            ));
        }
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        f.write_all(b"ARC-INT8\x02\x00")?;

        let cfg = &self.config;
        for &v in &[
            cfg.n_layers,
            cfg.d_model,
            cfg.n_heads,
            cfg.n_kv_heads,
            cfg.d_ff,
            cfg.d_head,
            cfg.d_kv,
            cfg.vocab_size,
            cfg.max_seq,
        ] {
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

    pub fn weight_hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new();
        let hash_i8w = |h: &mut blake3::Hasher, w: &I8Weights| {
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(w.data.as_ptr() as *const u8, w.data.len()) };
            h.update(bytes);
            for &s in &w.scales {
                h.update(&s.to_le_bytes());
            }
        };
        hash_i8w(&mut hasher, &self.embedding_i8);
        hash_i8w(&mut hasher, &self.output_weight);
        for layer in &self.layers {
            for w in [
                &layer.wq,
                &layer.wk,
                &layer.wv,
                &layer.wo,
                &layer.w_gate,
                &layer.w_up,
                &layer.w_down,
            ] {
                hash_i8w(&mut hasher, w);
            }
        }
        let hash = hasher.finalize();
        Hash256(*hash.as_bytes())
    }

    pub fn forward_shard_token(
        &self,
        input: ShardInput,
        cache: &mut KVCache,
        start_layer: usize,
        end_layer: usize,
        position: usize,
    ) -> Result<ShardOutput, ShardForwardError> {
        self.forward_shard_token_with_history(input, cache, start_layer, end_layer, position, &[])
    }

    pub fn forward_shard_token_with_history(
        &self,
        input: ShardInput,
        cache: &mut KVCache,
        start_layer: usize,
        end_layer: usize,
        position: usize,
        generated_tokens: &[u32],
    ) -> Result<ShardOutput, ShardForwardError> {
        self.forward_shard_token_with_qk(
            input,
            cache,
            start_layer,
            end_layer,
            position,
            generated_tokens,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn forward_shard_token_with_history_interleaved_rope(
        &self,
        input: ShardInput,
        cache: &mut KVCache,
        start_layer: usize,
        end_layer: usize,
        position: usize,
        generated_tokens: &[u32],
        interleaved_qk: &InterleavedRopeQk,
    ) -> Result<ShardOutput, ShardForwardError> {
        self.forward_shard_token_with_qk(
            input,
            cache,
            start_layer,
            end_layer,
            position,
            generated_tokens,
            Some(interleaved_qk),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn forward_shard_token_with_qk(
        &self,
        input: ShardInput,
        cache: &mut KVCache,
        start_layer: usize,
        end_layer: usize,
        position: usize,
        generated_tokens: &[u32],
        interleaved_qk: Option<&InterleavedRopeQk>,
    ) -> Result<ShardOutput, ShardForwardError> {
        let cfg = &self.config;
        let d = cfg.d_model;
        let is_first = start_layer == 0;
        let is_last = end_layer == cfg.n_layers;
        let end = end_layer.min(self.layers.len());

        if position >= cfg.max_seq {
            return Err(ShardForwardError::PositionOutOfRange {
                position,
                max_seq: cfg.max_seq,
            });
        }

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

        for layer_idx in start_layer..end {
            if !self.layers[layer_idx].is_loaded() {
                return Err(ShardForwardError::LayerNotLoaded { layer: layer_idx });
            }
        }

        if let Some(copy) = interleaved_qk {
            if !self.has_canonical_i8_profile()
                || self.config.arithmetic_profile != ArithmeticProfile::LegacySplitHalfV0
            {
                return Err(ShardForwardError::InterleavedRopeUnavailable { layer: start_layer });
            }
            for layer_idx in start_layer..end {
                if copy.layer(layer_idx).is_none() {
                    return Err(ShardForwardError::InterleavedRopeUnavailable { layer: layer_idx });
                }
            }
        }

        let mut hidden: Vec<i64> = match input {
            ShardInput::Token(token_id) => {
                debug_assert!(is_first, "Token input is only valid on the first shard");
                let idx = (token_id as usize).min(cfg.vocab_size - 1);
                let emb_start = idx * d;
                self.embedding_q16[emb_start..emb_start + d].to_vec()
            }
            ShardInput::Hidden(state) => {
                if state.len() != d {
                    return Err(ShardForwardError::BadHiddenDim {
                        got: state.len(),
                        expected: d,
                    });
                }
                state
            }
        };

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

            let i16l = self.i16_layers.as_ref().map(|il| &il[layer_idx]);
            #[cfg(target_arch = "aarch64")]
            let q4l = self.q4_layers.as_ref().map(|ql| &ql[layer_idx]);

            macro_rules! dispatch {
                ($i16_field:ident, $i8_field:ident, $inq:expr, $raw:expr, $in_sz:expr, $out:expr) => {{
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
                }};
            }

            let normed = layernorm(&hidden, &layer.attn_norm);
            let normed_q = QuantizedInput::from_i64(&normed);

            match interleaved_qk.and_then(|copy| copy.layer(layer_idx)) {
                Some((wq, wk)) => {
                    matmul_fast_preq(wq, &normed_q, &normed, d, &mut q);
                    matmul_fast_preq(wk, &normed_q, &normed, d, &mut k_buf);
                }
                None => {
                    dispatch!(wq, wq, &normed_q, &normed, d, &mut q);
                    dispatch!(wk, wk, &normed_q, &normed, d, &mut k_buf);
                }
            }
            dispatch!(wv, wv, &normed_q, &normed, d, &mut v_buf);

            for h in 0..cfg.n_heads {
                apply_rope(
                    &mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    position,
                    cfg.d_head,
                    &cfg.rope_cos,
                    &cfg.rope_sin,
                );
            }
            for h in 0..cfg.n_kv_heads {
                apply_rope(
                    &mut k_buf[h * cfg.d_head..(h + 1) * cfg.d_head],
                    position,
                    cfg.d_head,
                    &cfg.rope_cos,
                    &cfg.rope_sin,
                );
            }

            cache.push_k(layer_idx, &k_buf);
            cache.push_v(layer_idx, &v_buf);

            let full_seq = position + 1;
            let k_layer_data = &cache.k_data[layer_idx];
            let v_layer_data = &cache.v_data[layer_idx];
            let head_results: Vec<Vec<i64>> = (0..cfg.n_heads)
                .into_par_iter()
                .map(|h| {
                    let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                    let dh = cfg.d_head;
                    let q_head = &q[h * dh..(h + 1) * dh];
                    flash_attention_i64(
                        q_head,
                        k_layer_data,
                        v_layer_data,
                        cfg.d_kv,
                        kv_h,
                        dh,
                        full_seq,
                        cfg.attn_scale,
                    )
                })
                .collect();

            for val in attn_out.iter_mut() {
                *val = 0;
            }
            for (h, head_out) in head_results.iter().enumerate() {
                attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(head_out);
            }

            let attn_out_q = QuantizedInput::from_i64(&attn_out);
            dispatch!(wo, wo, &attn_out_q, &attn_out, d, &mut projected);
            for i in 0..d {
                hidden[i] += projected[i];
            }

            let normed_ff = layernorm(&hidden, &layer.ffn_norm);
            let normed_ff_q = QuantizedInput::from_i64(&normed_ff);
            dispatch!(w_gate, w_gate, &normed_ff_q, &normed_ff, d, &mut gate);
            dispatch!(w_up, w_up, &normed_ff_q, &normed_ff, d, &mut up);
            for j in 0..cfg.d_ff {
                gate[j] = (silu_i64(gate[j]) * up[j]) >> FRAC_BITS;
            }
            let gate_q = QuantizedInput::from_i64(&gate);
            dispatch!(w_down, w_down, &gate_q, &gate, cfg.d_ff, &mut ff_out);
            for i in 0..d {
                hidden[i] += ff_out[i];
            }
        }

        if is_last {
            cache.seq_len = position + 1;
            let normed = layernorm(&hidden, &self.final_norm);
            let mut logits = if let Some(ref i16w) = self.i16_output {
                let mut logits = vec![0i64; cfg.vocab_size];
                matmul_i16_into(i16w, &normed, d, &mut logits);
                logits
            } else {
                matmul_fast(&self.output_weight, &normed, d, cfg.vocab_size)
            };
            let token_id = select_next_token_with_repetition_penalty(&mut logits, generated_tokens);
            let logits_bytes: Vec<u8> = logits.iter().flat_map(|v| v.to_le_bytes()).collect();
            let logits_hash = arc_crypto::hash_bytes(&logits_bytes);
            Ok(ShardOutput::Token {
                id: token_id,
                logits_hash,
            })
        } else {
            cache.seq_len = position + 1;
            Ok(ShardOutput::Hidden(hidden))
        }
    }
}

pub fn select_next_token_with_repetition_penalty(
    logits: &mut [i64],
    generated_tokens: &[u32],
) -> u32 {
    for &previous_token in generated_tokens.iter().rev().take(64) {
        let index = previous_token as usize;
        if let Some(logit) = logits.get_mut(index) {
            if *logit > 0 {
                *logit = *logit * 5 / 6;
            } else {
                *logit = *logit * 6 / 5;
            }
        }
    }
    argmax_i64(logits) as u32
}

#[derive(Debug)]
pub enum ShardInput {
    Token(u32),
    Hidden(Vec<i64>),
}

#[derive(Debug)]
pub enum ShardOutput {
    Hidden(Vec<i64>),
    Token { id: u32, logits_hash: Hash256 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardForwardError {
    KvCacheOutOfSync {
        layer: usize,
        expected_positions: usize,
        cached_positions: usize,
    },
    LayerNotLoaded {
        layer: usize,
    },
    BadHiddenDim {
        got: usize,
        expected: usize,
    },
    PositionOutOfRange {
        position: usize,
        max_seq: usize,
    },
    InterleavedRopeUnavailable {
        layer: usize,
    },
}

impl ShardForwardError {
    pub fn kind(&self) -> &'static str {
        match self {
            ShardForwardError::KvCacheOutOfSync { .. } => "kv_cache_out_of_sync",
            ShardForwardError::LayerNotLoaded { .. } => "layer_not_loaded",
            ShardForwardError::BadHiddenDim { .. } => "bad_hidden_dim",
            ShardForwardError::PositionOutOfRange { .. } => "position_out_of_range",
            ShardForwardError::InterleavedRopeUnavailable { .. } => "interleaved_rope_unavailable",
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
                write!(
                    f,
                    "layer_not_loaded: layer {} is not resident on this node",
                    layer
                )
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
            ShardForwardError::InterleavedRopeUnavailable { layer } => write!(
                f,
                "interleaved_rope_unavailable: no interleaved-RoPE Q/K rows are resident for layer {}",
                layer
            ),
        }
    }
}

impl std::error::Error for ShardForwardError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardRowsInput {
    Tokens(Vec<u32>),
    Hidden(Vec<Vec<i64>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardRowsOutput {
    Hidden(Vec<Vec<i64>>),
    Logits(Vec<Vec<i64>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardRowsError {
    Shard(ShardForwardError),
    NoRows,
    BadLayerRange {
        start_layer: usize,
        end_layer: usize,
        n_layers: usize,
    },
    WrongInput {
        start_layer: usize,
    },
    TokenNotEmbedded {
        token: u32,
    },
    NotCanonicalProfile,
    BadShape,
    HeadNotLoaded,
    TooManyRows {
        rows: usize,
        max_rows: usize,
    },
    RollbackGrows {
        keep: usize,
        cached_positions: usize,
    },
    RaggedCache {
        layer: usize,
        keys: usize,
        values: usize,
        expected: usize,
    },
    CacheLayersDisagree {
        k_layers: usize,
        v_layers: usize,
    },
}

impl ShardRowsError {
    pub fn kind(&self) -> &'static str {
        match self {
            ShardRowsError::Shard(error) => error.kind(),
            ShardRowsError::NoRows => "no_rows",
            ShardRowsError::BadLayerRange { .. } => "bad_layer_range",
            ShardRowsError::WrongInput { .. } => "wrong_input",
            ShardRowsError::TokenNotEmbedded { .. } => "token_not_embedded",
            ShardRowsError::NotCanonicalProfile => "not_canonical_profile",
            ShardRowsError::BadShape => "bad_shape",
            ShardRowsError::HeadNotLoaded => "head_not_loaded",
            ShardRowsError::TooManyRows { .. } => "too_many_rows",
            ShardRowsError::RollbackGrows { .. } => "rollback_grows",
            ShardRowsError::RaggedCache { .. } => "ragged_cache",
            ShardRowsError::CacheLayersDisagree { .. } => "cache_layers_disagree",
        }
    }
}

impl From<ShardForwardError> for ShardRowsError {
    fn from(error: ShardForwardError) -> Self {
        ShardRowsError::Shard(error)
    }
}

impl std::fmt::Display for ShardRowsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShardRowsError::Shard(error) => write!(f, "{error}"),
            ShardRowsError::NoRows => write!(f, "no_rows: a multi-row call needs at least one row"),
            ShardRowsError::BadLayerRange {
                start_layer,
                end_layer,
                n_layers,
            } => write!(
                f,
                "bad_layer_range: [{start_layer}, {end_layer}) is not a stage of a \
                 {n_layers}-layer model and its cache"
            ),
            ShardRowsError::WrongInput { start_layer } => write!(
                f,
                "wrong_input: the stage starting at layer {start_layer} takes {}",
                if *start_layer == 0 {
                    "token ids"
                } else {
                    "hidden rows"
                }
            ),
            ShardRowsError::TokenNotEmbedded { token } => {
                write!(f, "token_not_embedded: no embedding row for token {token}")
            }
            ShardRowsError::NotCanonicalProfile => write!(
                f,
                "not_canonical_profile: multi-row stage calls run the canonical per-row I8 profile only"
            ),
            ShardRowsError::BadShape => write!(f, "bad_shape: a model dimension is zero"),
            ShardRowsError::HeadNotLoaded => write!(
                f,
                "head_not_loaded: the last stage does not hold the final norm and output head"
            ),
            ShardRowsError::RollbackGrows {
                keep,
                cached_positions,
            } => write!(
                f,
                "rollback_grows: cannot keep {keep} positions of a cache holding {cached_positions}"
            ),
            ShardRowsError::TooManyRows { rows, max_rows } => write!(
                f,
                "too_many_rows: {rows} rows in one call, but this stage takes at most {max_rows}"
            ),
            ShardRowsError::RaggedCache {
                layer,
                keys,
                values,
                expected,
            } => write!(
                f,
                "ragged_cache: layer {layer} holds {keys} K and {values} V values where its \
                 position count needs {expected} of each"
            ),
            ShardRowsError::CacheLayersDisagree { k_layers, v_layers } => write!(
                f,
                "cache_layers_disagree: the cache has {k_layers} K layers and {v_layers} V layers"
            ),
        }
    }
}

impl std::error::Error for ShardRowsError {}

pub struct SeqRows<'a> {
    pub cache: &'a mut KVCache,
    pub position: usize,
    pub input: ShardRowsInput,
}

impl SeqRows<'_> {
    pub fn rows(&self) -> usize {
        match &self.input {
            ShardRowsInput::Tokens(tokens) => tokens.len(),
            ShardRowsInput::Hidden(states) => states.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiRowsError {
    Batch(ShardRowsError),
    Sequence { index: usize, error: ShardRowsError },
}

impl MultiRowsError {
    pub fn into_rows_error(self) -> ShardRowsError {
        match self {
            MultiRowsError::Batch(error) | MultiRowsError::Sequence { error, .. } => error,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            MultiRowsError::Batch(error) | MultiRowsError::Sequence { error, .. } => error.kind(),
        }
    }
}

impl std::fmt::Display for MultiRowsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MultiRowsError::Batch(error) => write!(f, "{error}"),
            MultiRowsError::Sequence { index, error } => write!(f, "sequence {index}: {error}"),
        }
    }
}

impl std::error::Error for MultiRowsError {}

#[derive(Clone, Copy, Debug)]
struct SeqSpan {
    first_row: usize,
    rows: usize,
    first_position: usize,
}

impl CachedIntegerModel {
    pub fn max_shard_rows(&self, end_layer: usize) -> usize {
        let cfg = &self.config;
        let scratch = crate::canonical_prefill::prefill_chunk_scratch_bytes(
            1,
            cfg.d_model,
            cfg.d_kv,
            cfg.d_ff,
        );
        let returned = if end_layer == cfg.n_layers {
            cfg.d_model
                .saturating_add(cfg.vocab_size.saturating_mul(2))
                .saturating_mul(8)
        } else {
            cfg.d_model.saturating_mul(8)
        };
        let per_row = scratch.saturating_add(returned).max(1);
        (crate::canonical_prefill::MAX_PREFILL_SCRATCH_BYTES / per_row)
            .clamp(1, crate::canonical_simd::MAX_BATCH_TOKENS)
    }

    pub fn forward_shard_rows(
        &self,
        input: ShardRowsInput,
        cache: &mut KVCache,
        start_layer: usize,
        end_layer: usize,
        position: usize,
    ) -> Result<ShardRowsOutput, ShardRowsError> {
        let mut batch = [SeqRows {
            cache,
            position,
            input,
        }];
        let mut outputs = self
            .forward_rows_multi(&mut batch, start_layer, end_layer)
            .map_err(MultiRowsError::into_rows_error)?;
        outputs.pop().ok_or(ShardRowsError::NoRows)
    }

    pub fn forward_rows_multi(
        &self,
        batch: &mut [SeqRows<'_>],
        start_layer: usize,
        end_layer: usize,
    ) -> Result<Vec<ShardRowsOutput>, MultiRowsError> {
        let cfg = &self.config;
        let d = cfg.d_model;
        if batch.is_empty() {
            return Err(MultiRowsError::Batch(ShardRowsError::NoRows));
        }
        if let Some(index) = batch.iter().position(|seq| seq.rows() == 0) {
            return Err(MultiRowsError::Sequence {
                index,
                error: ShardRowsError::NoRows,
            });
        }
        if !self.has_canonical_i8_profile() {
            return Err(MultiRowsError::Batch(ShardRowsError::NotCanonicalProfile));
        }
        if d == 0
            || cfg.d_head == 0
            || cfg.n_heads == 0
            || cfg.n_kv_heads == 0
            || cfg.d_kv == 0
            || cfg.vocab_size == 0
        {
            return Err(MultiRowsError::Batch(ShardRowsError::BadShape));
        }
        let bad_range = ShardRowsError::BadLayerRange {
            start_layer,
            end_layer,
            n_layers: cfg.n_layers,
        };
        if start_layer >= end_layer || end_layer > cfg.n_layers || end_layer > self.layers.len() {
            return Err(MultiRowsError::Batch(bad_range));
        }
        if let Some(index) = batch.iter().position(|seq| {
            end_layer > seq.cache.k_data.len() || end_layer > seq.cache.v_data.len()
        }) {
            return Err(MultiRowsError::Sequence {
                index,
                error: bad_range,
            });
        }
        let rows: usize = batch.iter().map(SeqRows::rows).sum();
        let max_rows = self.max_shard_rows(end_layer);
        if rows > max_rows {
            return Err(MultiRowsError::Batch(ShardRowsError::TooManyRows {
                rows,
                max_rows,
            }));
        }
        for (index, seq) in batch.iter().enumerate() {
            self.check_rows_fit(
                &*seq.cache,
                seq.position,
                seq.rows(),
                start_layer,
                end_layer,
            )
            .map_err(|error| MultiRowsError::Sequence { index, error })?;
        }
        for layer in start_layer..end_layer {
            if !self.layers[layer].is_loaded() {
                return Err(MultiRowsError::Batch(
                    ShardForwardError::LayerNotLoaded { layer }.into(),
                ));
            }
        }
        let terminal = end_layer == cfg.n_layers;
        if terminal
            && (self.final_norm.len() != d
                || self.output_weight.n_rows != cfg.vocab_size
                || self.output_weight.n_cols != d
                || self.output_weight.scales.len() != cfg.vocab_size
                || self.output_weight.data.len() != cfg.vocab_size * d)
        {
            return Err(MultiRowsError::Batch(ShardRowsError::HeadNotLoaded));
        }

        let mut hidden = Vec::with_capacity(rows * d);
        let mut spans = Vec::with_capacity(batch.len());
        for (index, seq) in batch.iter().enumerate() {
            spans.push(SeqSpan {
                first_row: hidden.len() / d,
                rows: seq.rows(),
                first_position: seq.position,
            });
            self.append_input_rows(&seq.input, start_layer, &mut hidden)
                .map_err(|error| MultiRowsError::Sequence { index, error })?;
        }

        let mut caches: Vec<&mut KVCache> = batch.iter_mut().map(|seq| &mut *seq.cache).collect();
        let mut ignore = |_: usize, _: usize, _: usize, _: &[i64]| {};
        self.forward_layers_multi(
            &mut hidden,
            &spans,
            start_layer..end_layer,
            &mut caches,
            &mut ignore,
        );
        for (span, cache) in spans.iter().zip(caches.iter_mut()) {
            cache.seq_len = span.first_position + span.rows;
        }

        if !terminal {
            return Ok(spans
                .iter()
                .map(|span| {
                    ShardRowsOutput::Hidden(
                        hidden[span.first_row * d..(span.first_row + span.rows) * d]
                            .chunks_exact(d)
                            .map(<[i64]>::to_vec)
                            .collect(),
                    )
                })
                .collect());
        }
        let mut normed = vec![0i64; rows * d];
        normed
            .par_chunks_mut(d)
            .zip(hidden.par_chunks(d))
            .for_each(|(out, row)| out.copy_from_slice(&layernorm(row, &self.final_norm)));
        let vocab = cfg.vocab_size;
        let mut logits = vec![0i64; rows * vocab];
        matmul_i8_into_batched(&self.output_weight, &normed, rows, d, &mut logits);
        Ok(spans
            .iter()
            .map(|span| {
                ShardRowsOutput::Logits(
                    logits[span.first_row * vocab..(span.first_row + span.rows) * vocab]
                        .chunks_exact(vocab)
                        .map(<[i64]>::to_vec)
                        .collect(),
                )
            })
            .collect())
    }

    fn check_rows_fit(
        &self,
        cache: &KVCache,
        position: usize,
        rows: usize,
        start_layer: usize,
        end_layer: usize,
    ) -> Result<(), ShardRowsError> {
        let cfg = &self.config;
        let last = position.saturating_add(rows - 1);
        if last >= cfg.max_seq {
            return Err(ShardForwardError::PositionOutOfRange {
                position: last,
                max_seq: cfg.max_seq,
            }
            .into());
        }
        let expected = position.saturating_mul(cfg.d_kv);
        for layer in start_layer..end_layer {
            let (keys, values) = (cache.k_data[layer].len(), cache.v_data[layer].len());
            if keys == expected && values == expected {
                continue;
            }
            if keys == values && keys.is_multiple_of(cfg.d_kv) {
                return Err(ShardForwardError::KvCacheOutOfSync {
                    layer,
                    expected_positions: position,
                    cached_positions: keys / cfg.d_kv,
                }
                .into());
            }
            return Err(ShardRowsError::RaggedCache {
                layer,
                keys,
                values,
                expected,
            });
        }
        Ok(())
    }

    fn append_input_rows(
        &self,
        input: &ShardRowsInput,
        start_layer: usize,
        hidden: &mut Vec<i64>,
    ) -> Result<(), ShardRowsError> {
        let cfg = &self.config;
        let d = cfg.d_model;
        match input {
            ShardRowsInput::Tokens(tokens) => {
                if start_layer != 0 {
                    return Err(ShardRowsError::WrongInput { start_layer });
                }
                for &token in tokens {
                    let idx = (token as usize).min(cfg.vocab_size - 1);
                    let row = self
                        .embedding_q16
                        .get(idx * d..(idx + 1) * d)
                        .ok_or(ShardRowsError::TokenNotEmbedded { token })?;
                    hidden.extend_from_slice(row);
                }
            }
            ShardRowsInput::Hidden(states) => {
                if start_layer == 0 {
                    return Err(ShardRowsError::WrongInput { start_layer });
                }
                for state in states {
                    if state.len() != d {
                        return Err(ShardForwardError::BadHiddenDim {
                            got: state.len(),
                            expected: d,
                        }
                        .into());
                    }
                    hidden.extend_from_slice(state);
                }
            }
        }
        Ok(())
    }

    pub fn rollback_rows(&self, cache: &mut KVCache, keep: usize) -> Result<(), ShardRowsError> {
        let held = cache.seq_len;
        if keep > held {
            return Err(ShardRowsError::RollbackGrows {
                keep,
                cached_positions: held,
            });
        }
        let width = self.config.d_kv;
        let expected = held.saturating_mul(width);
        if cache.k_data.len() != cache.v_data.len() {
            return Err(ShardRowsError::CacheLayersDisagree {
                k_layers: cache.k_data.len(),
                v_layers: cache.v_data.len(),
            });
        }
        for (layer, (keys, values)) in cache.k_data.iter().zip(&cache.v_data).enumerate() {
            let (keys, values) = (keys.len(), values.len());
            if (keys, values) != (0, 0) && (keys, values) != (expected, expected) {
                return Err(ShardRowsError::RaggedCache {
                    layer,
                    keys,
                    values,
                    expected,
                });
            }
        }
        for rows in cache.k_data.iter_mut().chain(cache.v_data.iter_mut()) {
            rows.truncate(keep * width);
        }
        cache.seq_len = keep;
        Ok(())
    }
}

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

#[cfg(feature = "candle")]
pub fn load_cached_model(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use crate::InferenceError;
    use candle_core::Device;
    use candle_core::quantized::gguf_file;

    let device = Device::Cpu;
    let gguf_path = path.to_string();

    let (
        n_layers,
        d_model,
        n_heads,
        n_kv_heads,
        d_ff,
        vocab_size,
        vocab,
        rope_base,
        eos_tokens,
        bos_token,
        chat_template,
    ) = {
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
        let vs = content
            .tensor_infos
            .get("token_embd.weight")
            .map(|t| t.shape.dims()[0])
            .unwrap_or(32000);

        let rope_base: f64 = match content.metadata.get(&format!("{arch}.rope.freq_base")) {
            Some(gguf_file::Value::F32(v)) => *v as f64,
            Some(gguf_file::Value::F64(v)) => *v,
            _ => 10000.0,
        };

        let eos_tokens = match content.metadata.get("tokenizer.ggml.eos_token_id") {
            Some(gguf_file::Value::U32(v)) => vec![*v],
            Some(gguf_file::Value::U64(v)) => vec![*v as u32],
            Some(gguf_file::Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| match v {
                    gguf_file::Value::U32(n) => Some(*n),
                    gguf_file::Value::U64(n) => Some(*n as u32),
                    _ => None,
                })
                .collect(),
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
            Some(gguf_file::Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| match v {
                    gguf_file::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };

        info!(arch = %arch, rope_base = %rope_base, eos = ?eos_tokens,
            bos = bos_token, chat_template_len = chat_template.len(),
            "GGUF architecture detected");

        (
            nl,
            dm,
            nh,
            nkv,
            dff,
            vs,
            vocab,
            rope_base,
            eos_tokens,
            bos_token,
            chat_template,
        )
    };

    let d_head = d_model / n_heads;
    let d_kv = d_head * n_kv_heads;

    info!(
        n_layers,
        d_model, n_heads, n_kv_heads, d_ff, vocab_size, "Loading GGUF into per-row INT8 cache..."
    );

    let mut reader = std::fs::File::open(&gguf_path)
        .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
    let content = gguf_file::Content::read(&mut reader)
        .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

    let extract_f32 = |reader: &mut std::fs::File,
                       content: &gguf_file::Content,
                       name: &str|
     -> Result<Vec<f32>, InferenceError> {
        let qt = content
            .tensor(reader, name, &device)
            .map_err(|e| InferenceError::Runtime(format!("{name}: {e}")))?;
        let deq = qt
            .dequantize(&device)
            .map_err(|e| InferenceError::Runtime(format!("dequant {name}: {e}")))?;
        deq.flatten_all()
            .map_err(|e| InferenceError::Runtime(format!("flatten: {e}")))?
            .to_vec1::<f32>()
            .map_err(|e| InferenceError::Runtime(format!("tovec: {e}")))
    };

    let _extract_i8 = |reader: &mut std::fs::File,
                       content: &gguf_file::Content,
                       name: &str,
                       rows: usize,
                       cols: usize|
     -> Result<I8Weights, InferenceError> {
        let f = extract_f32(reader, content, name)?;
        Ok(I8Weights::quantize_f32(&f, rows, cols))
    };

    let extract_i8_and_i16 = |reader: &mut std::fs::File,
                              content: &gguf_file::Content,
                              name: &str,
                              rows: usize,
                              cols: usize|
     -> Result<
        (I8Weights, I16Weights, crate::block_i8::BlockI8Weights),
        InferenceError,
    > {
        let f = extract_f32(reader, content, name)?;
        let i8 = I8Weights::quantize_f32(&f, rows, cols);
        let i16 = I16Weights::quantize_f32(&f, rows, cols);
        let block = if cols.is_multiple_of(crate::block_i8::BLOCK_SIZE) {
            crate::block_i8::BlockI8Weights::quantize_f32(&f, rows, cols)
        } else {
            crate::block_i8::BlockI8Weights::empty()
        };
        Ok((i8, i16, block))
    };

    let extract_norm = |reader: &mut std::fs::File,
                        content: &gguf_file::Content,
                        name: &str,
                        size: usize|
     -> Result<Vec<i64>, InferenceError> {
        let values = extract_f32(reader, content, name)?;
        if values.len() != size {
            return Err(InferenceError::Runtime(format!(
                "{name}: expected {size} norm weights, found {}",
                values.len()
            )));
        }
        Ok(values
            .iter()
            .map(|&x| (x * ONE as f32).round() as i64)
            .collect())
    };

    let embedding_f32 = extract_f32(&mut reader, &content, "token_embd.weight")?;
    let embedding_i8 = I8Weights::quantize_f32(&embedding_f32, vocab_size, d_model);
    let embedding_q16: Vec<i64> = embedding_f32
        .iter()
        .map(|&x| (x as f64 * ONE as f64).round() as i64)
        .collect();
    drop(embedding_f32);
    info!(
        "Embeddings loaded: {} MB Q16 + {} MB INT8",
        embedding_q16.len() * 8 / (1024 * 1024),
        embedding_i8.memory_bytes() / (1024 * 1024)
    );

    let (output_weight, i16_output, block_i8_output) =
        match extract_i8_and_i16(&mut reader, &content, "output.weight", vocab_size, d_model) {
            Ok((i8, i16, block)) => (i8, Some(i16), Some(block)),
            Err(_) => {
                let tied_f32 = extract_f32(&mut reader, &content, "token_embd.weight").ok();
                let tied_i16 = tied_f32
                    .as_ref()
                    .map(|f| I16Weights::quantize_f32(f, vocab_size, d_model));
                let tied_block = tied_f32.as_ref().and_then(|f| {
                    if d_model % crate::block_i8::BLOCK_SIZE == 0 {
                        Some(crate::block_i8::BlockI8Weights::quantize_f32(
                            f, vocab_size, d_model,
                        ))
                    } else {
                        None
                    }
                });
                (
                    I8Weights {
                        data: embedding_i8.data.clone(),
                        scales: embedding_i8.scales.clone(),
                        n_rows: embedding_i8.n_rows,
                        n_cols: embedding_i8.n_cols,
                    },
                    tied_i16,
                    tied_block,
                )
            }
        };
    let final_norm = extract_norm(&mut reader, &content, "output_norm.weight", d_model)?;

    let mut layers = Vec::with_capacity(n_layers);
    let mut i16_layers_vec: Vec<I16Layer> = Vec::with_capacity(n_layers);
    let mut block_i8_layers_vec: Vec<BlockI8Layer> = Vec::with_capacity(n_layers);
    for l in 0..n_layers {
        let p = format!("blk.{l}");
        let (wq, wq16, wq_b) = extract_i8_and_i16(
            &mut reader,
            &content,
            &format!("{p}.attn_q.weight"),
            d_model,
            d_model,
        )?;
        let (wk, wk16, wk_b) = extract_i8_and_i16(
            &mut reader,
            &content,
            &format!("{p}.attn_k.weight"),
            d_kv,
            d_model,
        )?;
        let (wv, wv16, wv_b) = extract_i8_and_i16(
            &mut reader,
            &content,
            &format!("{p}.attn_v.weight"),
            d_kv,
            d_model,
        )?;
        let (wo, wo16, wo_b) = extract_i8_and_i16(
            &mut reader,
            &content,
            &format!("{p}.attn_output.weight"),
            d_model,
            d_model,
        )?;
        let (w_gate, w_gate16, w_gate_b) = extract_i8_and_i16(
            &mut reader,
            &content,
            &format!("{p}.ffn_gate.weight"),
            d_ff,
            d_model,
        )?;
        let (w_up, w_up16, w_up_b) = extract_i8_and_i16(
            &mut reader,
            &content,
            &format!("{p}.ffn_up.weight"),
            d_ff,
            d_model,
        )?;
        let (w_down, w_down16, w_down_b) = extract_i8_and_i16(
            &mut reader,
            &content,
            &format!("{p}.ffn_down.weight"),
            d_model,
            d_ff,
        )?;

        if l % 8 == 0 || l == n_layers - 1 {
            info!("Layer {}/{} loaded", l + 1, n_layers);
        }

        layers.push(CachedLayer {
            wq,
            wk,
            wv,
            wo,
            w_gate,
            w_up,
            w_down,
            attn_norm: extract_norm(
                &mut reader,
                &content,
                &format!("{p}.attn_norm.weight"),
                d_model,
            )?,
            ffn_norm: extract_norm(
                &mut reader,
                &content,
                &format!("{p}.ffn_norm.weight"),
                d_model,
            )?,
        });
        i16_layers_vec.push(I16Layer {
            wq: wq16,
            wk: wk16,
            wv: wv16,
            wo: wo16,
            w_gate: w_gate16,
            w_up: w_up16,
            w_down: w_down16,
        });
        block_i8_layers_vec.push(BlockI8Layer {
            wq: wq_b,
            wk: wk_b,
            wv: wv_b,
            wo: wo_b,
            w_gate: w_gate_b,
            w_up: w_up_b,
            w_down: w_down_b,
        });
    }

    let max_seq = 4096;
    let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, rope_base);
    let attn_scale = integer_isqrt((d_head as i64) * ONE);

    info!(
        "Model loaded: ~{} MB per-row INT8",
        layers
            .iter()
            .map(|l| l.wq.memory_bytes()
                + l.wk.memory_bytes()
                + l.wv.memory_bytes()
                + l.wo.memory_bytes()
                + l.w_gate.memory_bytes()
                + l.w_up.memory_bytes()
                + l.w_down.memory_bytes())
            .sum::<usize>()
            / (1024 * 1024)
    );

    Ok(CachedIntegerModel {
        config: ModelConfig {
            n_layers,
            d_model,
            n_heads,
            n_kv_heads,
            d_ff,
            d_head,
            d_kv,
            vocab_size,
            attn_scale,
            rope_cos,
            rope_sin,
            max_seq,
            eos_tokens: eos_tokens.clone(),
            bos_token,
            chat_template: chat_template.clone(),
            arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
        },
        embedding_q16,
        embedding_i8,
        layers,
        final_norm,
        output_weight,
        vocab,
        q4_layers: None,
        q4_output: None,
        i16_layers: Some(i16_layers_vec),
        i16_output,
        block_i8_layers: Some(block_i8_layers_vec),
        block_i8_output,
        ternary_layers: None,
        ternary_output: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,
    })
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model(_path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime(
        "candle feature not enabled".into(),
    ))
}

#[cfg(feature = "candle")]
pub fn load_tokenizer_only(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use crate::InferenceError;
    use candle_core::quantized::gguf_file;

    let mut reader =
        std::fs::File::open(path).map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
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
    let vocab_size = content
        .tensor_infos
        .get("token_embd.weight")
        .map(|t| t.shape.dims()[0])
        .unwrap_or(32000);
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
        Some(gguf_file::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| match v {
                gguf_file::Value::U32(n) => Some(*n),
                gguf_file::Value::U64(n) => Some(*n as u32),
                _ => None,
            })
            .collect(),
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
        Some(gguf_file::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| match v {
                gguf_file::Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };

    info!(
        n_layers,
        d_model, vocab_size, "Loaded tokenizer-only (no weights)"
    );

    let max_seq = 4096;
    let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, rope_base);
    let attn_scale = (ONE as f64 / (d_head as f64).sqrt()).round() as i64;

    let config = ModelConfig {
        n_layers,
        d_model,
        n_heads,
        n_kv_heads,
        d_head,
        d_kv,
        d_ff,
        vocab_size,
        rope_cos,
        rope_sin,
        attn_scale,
        eos_tokens,
        bos_token,
        chat_template,
        arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
        max_seq,
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
        ternary_hybrid_output: None,
        ternary_output: None,
    })
}

#[cfg(not(feature = "candle"))]
pub fn load_tokenizer_only(_path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime(
        "candle feature not enabled".into(),
    ))
}

#[cfg(feature = "candle")]
pub fn load_cached_model_shard(
    path: &str,
    start_layer: usize,
    end_layer: usize,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    load_cached_model_shard_profile(path, start_layer, end_layer, true)
}

#[cfg(feature = "candle")]
pub fn load_cached_model_canonical_i8(
    path: &str,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    let model = load_cached_model_shard_profile(path, 0, usize::MAX, false)?;
    debug_assert!(model.has_all_transformer_layers());
    debug_assert!(model.has_canonical_i8_profile());
    Ok(model)
}

#[cfg(feature = "candle")]
pub fn load_cached_model_shard_canonical_i8(
    path: &str,
    start_layer: usize,
    end_layer: usize,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    let model = load_cached_model_shard_profile_with_mode(
        path,
        start_layer,
        end_layer,
        ShardRangeMode::StrictPartial,
        false,
    )?;
    debug_assert!(!model.has_all_transformer_layers());
    debug_assert!(model.has_canonical_i8_profile());
    Ok(model)
}

#[cfg(feature = "candle")]
pub fn load_cached_model_canonical_i8_interleaved_rope(
    path: &str,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    validate_llama_gguf_for_interleaved_rope(path)?;
    let mut model = load_cached_model_canonical_i8(path)?;
    if model.config.d_model != model.config.n_heads.saturating_mul(model.config.d_head)
        || model.config.d_kv != model.config.n_kv_heads.saturating_mul(model.config.d_head)
        || !model.config.d_head.is_multiple_of(2)
    {
        return Err(crate::InferenceError::Runtime(format!(
            "interleaved-RoPE Llama profile requires d_model=n_heads*d_head, d_kv=n_kv_heads*d_head, and even d_head; got d_model={}, n_heads={}, n_kv_heads={}, d_head={}, d_kv={}",
            model.config.d_model,
            model.config.n_heads,
            model.config.n_kv_heads,
            model.config.d_head,
            model.config.d_kv,
        )));
    }
    model.canonicalize_gguf_interleaved_rope_rows()?;
    debug_assert_eq!(
        model.arithmetic_profile(),
        GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE
    );
    Ok(model)
}

#[cfg(feature = "candle")]
fn validate_llama_gguf_for_interleaved_rope(path: &str) -> Result<(), crate::InferenceError> {
    use candle_core::quantized::gguf_file;
    let mut reader = std::fs::File::open(path)
        .map_err(|error| crate::InferenceError::Runtime(format!("open GGUF: {error}")))?;
    let content = gguf_file::Content::read(&mut reader)
        .map_err(|error| crate::InferenceError::Runtime(format!("read GGUF: {error}")))?;
    match content.metadata.get("general.architecture") {
        Some(gguf_file::Value::String(architecture)) if architecture == "llama" => Ok(()),
        Some(gguf_file::Value::String(architecture)) => {
            Err(crate::InferenceError::Runtime(format!(
                "GGUF interleaved-RoPE profile is pinned to Llama architecture, not {architecture}"
            )))
        }
        _ => Err(crate::InferenceError::Runtime(
            "GGUF interleaved-RoPE profile requires general.architecture=llama".into(),
        )),
    }
}

#[cfg(feature = "candle")]
pub(crate) fn config_from_gguf(
    content: &candle_core::quantized::gguf_file::Content,
) -> Result<(ModelConfig, Vec<String>), crate::InferenceError> {
    use candle_core::quantized::gguf_file;
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
    let vs = content
        .tensor_infos
        .get("token_embd.weight")
        .and_then(|t| t.shape.dims().first().copied())
        .unwrap_or(32000);

    let rope_base: f64 = match content.metadata.get(&format!("{arch}.rope.freq_base")) {
        Some(gguf_file::Value::F32(v)) => *v as f64,
        Some(gguf_file::Value::F64(v)) => *v,
        _ => 10000.0,
    };

    let eos_tokens = match content.metadata.get("tokenizer.ggml.eos_token_id") {
        Some(gguf_file::Value::U32(v)) => vec![*v],
        Some(gguf_file::Value::U64(v)) => vec![*v as u32],
        Some(gguf_file::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| match v {
                gguf_file::Value::U32(n) => Some(*n),
                gguf_file::Value::U64(n) => Some(*n as u32),
                _ => None,
            })
            .collect(),
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
        Some(gguf_file::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| match v {
                gguf_file::Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if nl == 0
        || dm == 0
        || nh == 0
        || nkv == 0
        || dff == 0
        || vs == 0
        || !dm.is_multiple_of(nh)
        || !nh.is_multiple_of(nkv)
    {
        return Err(crate::InferenceError::Runtime(
            "invalid GGUF model dimensions".into(),
        ));
    }
    let d_head = dm / nh;
    let d_kv = d_head
        .checked_mul(nkv)
        .ok_or_else(|| crate::InferenceError::Runtime("GGUF KV dimension overflow".into()))?;
    let max_seq = 4096;
    let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, rope_base);
    Ok((
        ModelConfig {
            n_layers: nl,
            d_model: dm,
            n_heads: nh,
            n_kv_heads: nkv,
            d_ff: dff,
            d_head,
            d_kv,
            vocab_size: vs,
            attn_scale: integer_isqrt((d_head as i64) * ONE),
            rope_cos,
            rope_sin,
            max_seq,
            eos_tokens,
            bos_token,
            chat_template,
            arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
        },
        vocab,
    ))
}

#[cfg(feature = "candle")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShardRangeMode {
    Clamped,
    StrictPartial,
}

#[cfg(feature = "candle")]
fn validate_canonical_partial_shard_range(
    start_layer: usize,
    end_layer: usize,
    n_layers: usize,
) -> Result<(), crate::InferenceError> {
    if start_layer >= end_layer {
        return Err(crate::InferenceError::Runtime(format!(
            "Invalid canonical shard range: [{start_layer}, {end_layer}) is empty or reversed (n_layers={n_layers})"
        )));
    }
    if end_layer > n_layers {
        return Err(crate::InferenceError::Runtime(format!(
            "Invalid canonical shard range: [{start_layer}, {end_layer}) is out of bounds (n_layers={n_layers})"
        )));
    }
    let owned_layers = end_layer - start_layer;
    if owned_layers == n_layers {
        return Err(crate::InferenceError::Runtime(format!(
            "Invalid canonical shard range: [{start_layer}, {end_layer}) covers full model (n_layers={n_layers})"
        )));
    }
    let max_owned_layers = n_layers.div_ceil(2);
    if owned_layers > max_owned_layers {
        return Err(crate::InferenceError::Runtime(format!(
            "Invalid canonical shard range: [{start_layer}, {end_layer}) owns {owned_layers} layers, exceeding ceil(n_layers/2)={max_owned_layers} (n_layers={n_layers})"
        )));
    }
    Ok(())
}

#[cfg(feature = "candle")]
fn load_cached_model_shard_profile(
    path: &str,
    start_layer: usize,
    end_layer: usize,
    include_optional_quantizations: bool,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    load_cached_model_shard_profile_with_mode(
        path,
        start_layer,
        end_layer,
        ShardRangeMode::Clamped,
        include_optional_quantizations,
    )
}

#[cfg(feature = "candle")]
fn load_cached_model_shard_profile_with_mode(
    path: &str,
    start_layer: usize,
    end_layer: usize,
    range_mode: ShardRangeMode,
    include_optional_quantizations: bool,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    use crate::InferenceError;
    use candle_core::Device;
    use candle_core::quantized::gguf_file;

    let device = Device::Cpu;
    let mut reader =
        std::fs::File::open(path).map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
    let content = gguf_file::Content::read(&mut reader)
        .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;
    let (config, vocab) = config_from_gguf(&content)?;
    let (n_layers, d_model, d_ff, d_kv, vocab_size) = (
        config.n_layers,
        config.d_model,
        config.d_ff,
        config.d_kv,
        config.vocab_size,
    );

    let end_layer = match range_mode {
        ShardRangeMode::Clamped => {
            let end_layer = end_layer.min(n_layers);
            if start_layer >= end_layer {
                return Err(InferenceError::Runtime(format!(
                    "Invalid shard range: [{start_layer}, {end_layer}) is empty (n_layers={n_layers})"
                )));
            }
            end_layer
        }
        ShardRangeMode::StrictPartial => {
            validate_canonical_partial_shard_range(start_layer, end_layer, n_layers)?;
            end_layer
        }
    };
    let is_first = start_layer == 0;
    let is_last = end_layer == n_layers;

    info!(
        n_layers,
        d_model,
        shard_start = start_layer,
        shard_end = end_layer,
        is_first,
        is_last,
        "Loading GGUF SHARD"
    );

    let extract_f32 = |reader: &mut std::fs::File,
                       content: &gguf_file::Content,
                       name: &str|
     -> Result<Vec<f32>, InferenceError> {
        let qt = content
            .tensor(reader, name, &device)
            .map_err(|e| InferenceError::Runtime(format!("{name}: {e}")))?;
        let deq = qt
            .dequantize(&device)
            .map_err(|e| InferenceError::Runtime(format!("dequant {name}: {e}")))?;
        deq.flatten_all()
            .map_err(|e| InferenceError::Runtime(format!("flatten: {e}")))?
            .to_vec1::<f32>()
            .map_err(|e| InferenceError::Runtime(format!("tovec: {e}")))
    };

    let extract_i8_i16 = |reader: &mut std::fs::File,
                          content: &gguf_file::Content,
                          name: &str,
                          rows: usize,
                          cols: usize|
     -> Result<(I8Weights, I16Weights), InferenceError> {
        let f = extract_f32(reader, content, name)?;
        let i8w = I8Weights::quantize_f32(&f, rows, cols);
        let i16w = if include_optional_quantizations {
            I16Weights::quantize_f32(&f, rows, cols)
        } else {
            I16Weights::empty()
        };
        Ok((i8w, i16w))
    };

    let extract_norm = |reader: &mut std::fs::File,
                        content: &gguf_file::Content,
                        name: &str,
                        size: usize|
     -> Result<Vec<i64>, InferenceError> {
        let values = extract_f32(reader, content, name)?;
        if values.len() != size {
            return Err(InferenceError::Runtime(format!(
                "{name}: expected {size} norm weights, found {}",
                values.len()
            )));
        }
        Ok(values
            .iter()
            .map(|&x| (x * ONE as f32).round() as i64)
            .collect())
    };

    let (embedding_q16, embedding_i8) = if is_first {
        let f = extract_f32(&mut reader, &content, "token_embd.weight")?;
        let i8w = I8Weights::quantize_f32(&f, vocab_size, d_model);
        let q16: Vec<i64> = f
            .iter()
            .map(|&x| (x as f64 * ONE as f64).round() as i64)
            .collect();
        info!(
            "Shard {}: embeddings loaded ({} MB Q16)",
            start_layer,
            q16.len() * 8 / (1024 * 1024)
        );
        (q16, i8w)
    } else {
        (Vec::new(), I8Weights::empty())
    };

    let (output_weight, output_weight_i16, final_norm) = if is_last {
        let f = extract_f32(&mut reader, &content, "output.weight")
            .or_else(|_| extract_f32(&mut reader, &content, "token_embd.weight"))?;
        let i8w = I8Weights::quantize_f32(&f, vocab_size, d_model);
        let i16w = include_optional_quantizations
            .then(|| I16Weights::quantize_f32(&f, vocab_size, d_model));
        let fn_ = extract_norm(&mut reader, &content, "output_norm.weight", d_model)?;
        info!("Shard last: output head + final_norm loaded");
        (i8w, i16w, fn_)
    } else {
        (I8Weights::empty(), None, Vec::new())
    };

    let mut layers: Vec<CachedLayer> = (0..n_layers).map(|_| CachedLayer::placeholder()).collect();
    let mut i16_layers_vec: Vec<I16Layer> = (0..n_layers)
        .map(|_| I16Layer {
            wq: I16Weights::empty(),
            wk: I16Weights::empty(),
            wv: I16Weights::empty(),
            wo: I16Weights::empty(),
            w_gate: I16Weights::empty(),
            w_up: I16Weights::empty(),
            w_down: I16Weights::empty(),
        })
        .collect();
    let mut q4_layers_vec: Vec<Q4Layer> = (0..n_layers)
        .map(|_| Q4Layer {
            wq: Q4WeightsX86 {
                data: Vec::new(),
                scales: Vec::new(),
                n_rows: 0,
                n_cols: 0,
            },
            wk: Q4WeightsX86 {
                data: Vec::new(),
                scales: Vec::new(),
                n_rows: 0,
                n_cols: 0,
            },
            wv: Q4WeightsX86 {
                data: Vec::new(),
                scales: Vec::new(),
                n_rows: 0,
                n_cols: 0,
            },
            wo: Q4WeightsX86 {
                data: Vec::new(),
                scales: Vec::new(),
                n_rows: 0,
                n_cols: 0,
            },
            w_gate: Q4WeightsX86 {
                data: Vec::new(),
                scales: Vec::new(),
                n_rows: 0,
                n_cols: 0,
            },
            w_up: Q4WeightsX86 {
                data: Vec::new(),
                scales: Vec::new(),
                n_rows: 0,
                n_cols: 0,
            },
            w_down: Q4WeightsX86 {
                data: Vec::new(),
                scales: Vec::new(),
                n_rows: 0,
                n_cols: 0,
            },
        })
        .collect();
    let enable_q4 = include_optional_quantizations && std::env::var("ARC_Q4_SHARD").is_ok();
    let mut any_i16 = false;
    let mut any_q4 = false;
    for l in start_layer..end_layer {
        let p = format!("blk.{l}");
        let (wq8, wq16) = extract_i8_i16(
            &mut reader,
            &content,
            &format!("{p}.attn_q.weight"),
            d_model,
            d_model,
        )?;
        let (wk8, wk16) = extract_i8_i16(
            &mut reader,
            &content,
            &format!("{p}.attn_k.weight"),
            d_kv,
            d_model,
        )?;
        let (wv8, wv16) = extract_i8_i16(
            &mut reader,
            &content,
            &format!("{p}.attn_v.weight"),
            d_kv,
            d_model,
        )?;
        let (wo8, wo16) = extract_i8_i16(
            &mut reader,
            &content,
            &format!("{p}.attn_output.weight"),
            d_model,
            d_model,
        )?;
        let (wg8, wg16) = extract_i8_i16(
            &mut reader,
            &content,
            &format!("{p}.ffn_gate.weight"),
            d_ff,
            d_model,
        )?;
        let (wu8, wu16) = extract_i8_i16(
            &mut reader,
            &content,
            &format!("{p}.ffn_up.weight"),
            d_ff,
            d_model,
        )?;
        let (wd8, wd16) = extract_i8_i16(
            &mut reader,
            &content,
            &format!("{p}.ffn_down.weight"),
            d_model,
            d_ff,
        )?;
        layers[l] = CachedLayer {
            wq: wq8,
            wk: wk8,
            wv: wv8,
            wo: wo8,
            w_gate: wg8,
            w_up: wu8,
            w_down: wd8,
            attn_norm: extract_norm(
                &mut reader,
                &content,
                &format!("{p}.attn_norm.weight"),
                d_model,
            )?,
            ffn_norm: extract_norm(
                &mut reader,
                &content,
                &format!("{p}.ffn_norm.weight"),
                d_model,
            )?,
        };
        i16_layers_vec[l] = I16Layer {
            wq: wq16,
            wk: wk16,
            wv: wv16,
            wo: wo16,
            w_gate: wg16,
            w_up: wu16,
            w_down: wd16,
        };
        any_i16 = include_optional_quantizations;
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
            info!(
                "Shard layer {}/{} loaded as I8{}{} ({} of {})",
                l + 1,
                n_layers,
                if any_i16 { "+I16" } else { "" },
                if any_q4 { "+Q4" } else { "" },
                l - start_layer + 1,
                end_layer - start_layer
            );
        }
    }

    let shard_mb: usize = layers
        .iter()
        .filter(|l| l.is_loaded())
        .map(|l| {
            l.wq.memory_bytes()
                + l.wk.memory_bytes()
                + l.wv.memory_bytes()
                + l.wo.memory_bytes()
                + l.w_gate.memory_bytes()
                + l.w_up.memory_bytes()
                + l.w_down.memory_bytes()
        })
        .sum::<usize>()
        / (1024 * 1024);
    info!(
        "Shard loaded: layers [{}, {}) = {} MB INT8",
        start_layer, end_layer, shard_mb
    );

    Ok(CachedIntegerModel {
        config,
        embedding_q16,
        embedding_i8,
        layers,
        final_norm,
        output_weight,
        vocab,
        q4_layers: if any_q4 { Some(q4_layers_vec) } else { None },
        q4_output: None,
        i16_layers: if any_i16 { Some(i16_layers_vec) } else { None },
        i16_output: output_weight_i16,
        block_i8_layers: None,
        block_i8_output: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,
        ternary_layers: None,
        ternary_output: None,
    })
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model_shard(
    _path: &str,
    _start_layer: usize,
    _end_layer: usize,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime(
        "candle feature not enabled".into(),
    ))
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model_canonical_i8(
    _path: &str,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime(
        "candle feature not enabled".into(),
    ))
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model_shard_canonical_i8(
    _path: &str,
    _start_layer: usize,
    _end_layer: usize,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime(
        "candle feature not enabled".into(),
    ))
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model_canonical_i8_interleaved_rope(
    _path: &str,
) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime(
        "candle feature not enabled".into(),
    ))
}

#[cfg(feature = "candle")]
pub fn load_cached_model_ranges(
    path: &str,
    ranges: &[(usize, usize)],
) -> Result<CachedIntegerModel, crate::InferenceError> {
    use crate::InferenceError;
    if ranges.is_empty() {
        return Err(InferenceError::Runtime(
            "load_cached_model_ranges: no ranges provided".into(),
        ));
    }
    let mut sorted: Vec<(usize, usize)> = ranges.to_vec();
    sorted.sort();
    for i in 1..sorted.len() {
        if sorted[i].0 < sorted[i - 1].1 {
            return Err(InferenceError::Runtime(format!(
                "Overlapping shard ranges: [{}, {}) and [{}, {})",
                sorted[i - 1].0,
                sorted[i - 1].1,
                sorted[i].0,
                sorted[i].1
            )));
        }
    }

    if sorted.len() == 1 {
        return load_cached_model_shard(path, sorted[0].0, sorted[0].1);
    }

    let mut aggregate = load_cached_model_shard(path, sorted[0].0, sorted[0].1)?;
    let n_layers = aggregate.config.n_layers;
    aggregate.enforce_canonical_i8_profile();

    for &(start, end) in &sorted[1..] {
        let mut other = load_cached_model_shard(path, start, end)?;
        other.enforce_canonical_i8_profile();
        if other.layers.len() != aggregate.layers.len() {
            return Err(InferenceError::Runtime(format!(
                "Shard loader returned inconsistent layer count: {} vs {}",
                other.layers.len(),
                aggregate.layers.len()
            )));
        }
        for idx in start..end.min(n_layers) {
            if other.layers[idx].is_loaded() {
                aggregate.layers[idx] =
                    std::mem::replace(&mut other.layers[idx], CachedLayer::placeholder());
            }
        }
        if start == 0 && aggregate.embedding_q16.is_empty() && !other.embedding_q16.is_empty() {
            aggregate.embedding_q16 = std::mem::take(&mut other.embedding_q16);
            aggregate.embedding_i8 = std::mem::replace(&mut other.embedding_i8, I8Weights::empty());
        }
        if end == n_layers {
            if aggregate.output_weight.n_rows == 0 && other.output_weight.n_rows != 0 {
                aggregate.output_weight =
                    std::mem::replace(&mut other.output_weight, I8Weights::empty());
            }
            if aggregate.final_norm.is_empty() && !other.final_norm.is_empty() {
                aggregate.final_norm = std::mem::take(&mut other.final_norm);
            }
        }
    }
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

    for &(s, e) in &sorted {
        for idx in s..e.min(n_layers) {
            let l = &aggregate.layers[idx];
            for (name, w) in [
                ("wq", &l.wq),
                ("wk", &l.wk),
                ("wv", &l.wv),
                ("wo", &l.wo),
                ("w_gate", &l.w_gate),
                ("w_up", &l.w_up),
                ("w_down", &l.w_down),
            ] {
                let expected = w.n_rows.saturating_mul(w.n_cols);
                if w.n_rows != 0 && w.data.len() != expected {
                    return Err(crate::InferenceError::Runtime(format!(
                        "load_cached_model_ranges: merged layer {idx}.{name} has \
                         n_rows={} n_cols={} but data.len()={} (expected {}). \
                         Multi-range merge corrupted the weight vector - do not \
                         hand this model to forward_shard_token.",
                        w.n_rows,
                        w.n_cols,
                        w.data.len(),
                        expected
                    )));
                }
                if w.n_rows != 0 && w.scales.len() != w.n_rows {
                    return Err(crate::InferenceError::Runtime(format!(
                        "load_cached_model_ranges: merged layer {idx}.{name} has \
                         n_rows={} but scales.len()={} (must match n_rows).",
                        w.n_rows,
                        w.scales.len()
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
    Err(crate::InferenceError::Runtime(
        "candle feature not enabled".into(),
    ))
}

pub fn load_cached_model_binary(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use crate::InferenceError;
    use std::io::Read;

    let mut f = std::io::BufReader::new(
        std::fs::File::open(path).map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?,
    );

    let mut magic = [0u8; 10];
    f.read_exact(&mut magic)
        .map_err(|e| InferenceError::Runtime(format!("Magic: {e}")))?;
    if &magic[..8] != b"ARC-INT8" {
        return Err(InferenceError::Runtime("Not an ARC-INT8 file".into()));
    }

    let read_u64 = |f: &mut std::io::BufReader<std::fs::File>| -> Result<u64, InferenceError> {
        let mut b = [0u8; 8];
        f.read_exact(&mut b)
            .map_err(|e| InferenceError::Runtime(format!("Read: {e}")))?;
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
    f.read_exact(&mut buf8)
        .map_err(|e| InferenceError::Runtime(format!("Scale: {e}")))?;
    let attn_scale = i64::from_le_bytes(buf8);

    let rope_cos =
        read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Cos: {e}")))?;
    let rope_sin =
        read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Sin: {e}")))?;

    let embedding_i8 =
        I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("Emb: {e}")))?;
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
    let output_weight =
        I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("Out: {e}")))?;
    let checked_norm = |name: String, values: Vec<i64>| -> Result<Vec<i64>, InferenceError> {
        if values.len() == d_model {
            Ok(values)
        } else {
            Err(InferenceError::Runtime(format!(
                "{name}: expected {d_model} norm weights, found {}",
                values.len()
            )))
        }
    };
    let final_norm = checked_norm(
        "final norm".into(),
        read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Norm: {e}")))?,
    )?;

    let mut layers = Vec::with_capacity(n_layers);
    for l in 0..n_layers {
        layers.push(CachedLayer {
            wq: I8Weights::read_from(&mut f)
                .map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            wk: I8Weights::read_from(&mut f)
                .map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            wv: I8Weights::read_from(&mut f)
                .map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            wo: I8Weights::read_from(&mut f)
                .map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            w_gate: I8Weights::read_from(&mut f)
                .map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            w_up: I8Weights::read_from(&mut f)
                .map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            w_down: I8Weights::read_from(&mut f)
                .map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            attn_norm: checked_norm(
                format!("L{l} attention norm"),
                read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            )?,
            ffn_norm: checked_norm(
                format!("L{l} FFN norm"),
                read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            )?,
        });
    }

    let vocab_len = read_u64(&mut f)? as usize;
    let mut vocab_bytes = vec![0u8; vocab_len];
    f.read_exact(&mut vocab_bytes)
        .map_err(|e| InferenceError::Runtime(format!("Vocab: {e}")))?;
    let vocab: Vec<String> = serde_json::from_slice(&vocab_bytes).unwrap_or_default();

    info!(
        "Binary model loaded: {} layers, d={}, vocab={}",
        n_layers, d_model, vocab_size
    );

    let eos_tokens = vec![2u32, 128001, 128009];

    Ok(CachedIntegerModel {
        config: ModelConfig {
            n_layers,
            d_model,
            n_heads,
            n_kv_heads,
            d_ff,
            d_head,
            d_kv,
            vocab_size,
            attn_scale,
            rope_cos,
            rope_sin,
            max_seq,
            eos_tokens,
            bos_token: 1,
            chat_template: String::new(),
            arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
        },
        embedding_q16,
        embedding_i8,
        layers,
        final_norm,
        output_weight,
        vocab,
        q4_layers: None,
        q4_output: None,
        i16_layers: None,
        i16_output: None,
        block_i8_layers: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,
        block_i8_output: None,
        ternary_layers: None,
        ternary_output: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tensor_parallel::{
        LocalRowWorker, PartitionedProjectionBackend, RowAssignment, RowWorker, TensorKey,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[test]
    fn interleaved_rope_matches_candle_rope_i_pairing() {
        let half = ONE / 2;
        let mut actual = vec![ONE, 2 * ONE, 3 * ONE, 4 * ONE];
        apply_rope_interleaved(&mut actual, 0, 4, &[half, half], &[half, half]);
        assert_eq!(
            actual,
            vec![-(ONE / 2), 3 * ONE / 2, -(ONE / 2), 7 * ONE / 2]
        );
    }

    #[test]
    fn shared_repetition_penalty_changes_a_repeated_raw_argmax() {
        let raw_logits = vec![0, 120, 110];
        assert_eq!(argmax_i64(&raw_logits), 1);

        let mut worker_logits = raw_logits.clone();
        let mut distributed_verifier_logits = raw_logits;
        let worker = select_next_token_with_repetition_penalty(&mut worker_logits, &[1]);
        let distributed_verifier =
            select_next_token_with_repetition_penalty(&mut distributed_verifier_logits, &[1]);

        assert_eq!(worker, 2);
        assert_eq!(distributed_verifier, worker);
        assert_eq!(worker_logits, distributed_verifier_logits);
    }

    fn token_at_a_time(
        model: &CachedIntegerModel,
        tokens: &[u32],
        cache: &mut KVCache,
    ) -> Vec<Vec<i64>> {
        tokens
            .iter()
            .map(|t| model.forward_one_token(*t, cache))
            .collect()
    }

    fn clone_cache(c: &KVCache) -> KVCache {
        KVCache {
            k_data: c.k_data.clone(),
            v_data: c.v_data.clone(),
            seq_len: c.seq_len,
        }
    }

    fn assert_same_cache(a: &KVCache, b: &KVCache, case: &str) {
        assert_eq!(a.seq_len, b.seq_len, "seq_len differs ({case})");
        assert_eq!(
            a.k_data.len(),
            b.k_data.len(),
            "layer count differs ({case})"
        );
        for l in 0..a.k_data.len() {
            assert_eq!(
                a.k_data[l], b.k_data[l],
                "K cache differs at layer {l} ({case})"
            );
            assert_eq!(
                a.v_data[l], b.v_data[l],
                "V cache differs at layer {l} ({case})"
            );
        }
    }

    fn run_batched_conformance(simd: bool) {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let prev = crate::canonical_simd::fast_canonical_kernel_enabled();
        crate::canonical_simd::set_fast_canonical_kernel(simd);
        let model = build_test_model(64, 32, 4, 64, 3);
        assert!(model.has_canonical_i8_profile());
        let prompts: Vec<Vec<u32>> = vec![
            vec![1],
            vec![1, 2],
            vec![1, 2, 3],
            vec![5, 9, 13, 21, 34, 55, 2],
            (0..8u32).collect(),
            (0..9u32).collect(),
            (0..15u32).collect(),
            (0..16u32).collect(),
            (0..17u32).collect(),
            (0..33u32).collect(),
        ];
        for prompt in &prompts {
            let mut ref_cache = KVCache::new(model.config.n_layers);
            let reference = token_at_a_time(&model, prompt, &mut ref_cache);
            for &chunk in &[1usize, 2, 3, 4, 5, 7, 8, 16, 64] {
                let mut cache = KVCache::new(model.config.n_layers);
                let got = model
                    .prefill_canonical_i8_batched(prompt, &mut cache, chunk, true)
                    .unwrap_or_else(|| panic!("refused len={} chunk={chunk}", prompt.len()));
                let case = format!("simd={simd} len={} chunk={chunk}", prompt.len());
                assert_eq!(got.len(), reference.len(), "position count ({case})");
                for (p, (a, b)) in reference.iter().zip(got.iter()).enumerate() {
                    assert_eq!(a, b, "logits differ at position {p} ({case})");
                }
                assert_same_cache(&ref_cache, &cache, &case);

                let mut ref_cont = clone_cache(&ref_cache);
                let mut got_cont = clone_cache(&cache);
                for step in 0..3u32 {
                    let a = model.forward_one_token(7 + step, &mut ref_cont);
                    let b = model.forward_one_token(7 + step, &mut got_cont);
                    assert_eq!(a, b, "continuation logits differ at step {step} ({case})");
                }
                assert_same_cache(&ref_cont, &got_cont, &format!("{case} after decode"));
            }
        }
        crate::canonical_simd::set_fast_canonical_kernel(prev);
    }

    #[test]
    fn batched_prefill_matches_token_at_a_time_scalar() {
        run_batched_conformance(false);
    }

    #[test]
    fn batched_prefill_matches_token_at_a_time_simd() {
        if !crate::canonical_simd::dotprod_available() {
            return;
        }
        run_batched_conformance(true);
    }

    #[test]
    fn batched_prefill_resumes_from_a_nonempty_cache() {
        let model = build_test_model(64, 32, 4, 64, 3);
        let head: Vec<u32> = vec![1, 4, 9];
        let tail: Vec<u32> = vec![16, 25, 36, 49, 64 - 1];
        let mut ref_cache = KVCache::new(model.config.n_layers);
        let _ = token_at_a_time(&model, &head, &mut ref_cache);
        let reference = token_at_a_time(&model, &tail, &mut ref_cache);

        for &chunk in &[1usize, 2, 3, 8] {
            let mut cache = KVCache::new(model.config.n_layers);
            let _ = token_at_a_time(&model, &head, &mut cache);
            assert_eq!(cache.seq_len, head.len());
            let got = model
                .prefill_canonical_i8_batched(&tail, &mut cache, chunk, true)
                .expect("must accept a resumed prefill");
            for (p, (a, b)) in reference.iter().zip(got.iter()).enumerate() {
                assert_eq!(
                    a, b,
                    "resumed prefill differs at position {p}, chunk={chunk}"
                );
            }
            assert_same_cache(&ref_cache, &cache, &format!("resumed chunk={chunk}"));
        }
    }

    #[test]
    fn batched_prefill_last_position_only_matches_final_logits() {
        let model = build_test_model(64, 32, 4, 64, 3);
        let prompt: Vec<u32> = (0..11u32).collect();
        let mut ref_cache = KVCache::new(model.config.n_layers);
        let reference = token_at_a_time(&model, &prompt, &mut ref_cache);
        for &chunk in &[1usize, 3, 4, 16] {
            let mut cache = KVCache::new(model.config.n_layers);
            let got = model
                .prefill_canonical_i8_batched(&prompt, &mut cache, chunk, false)
                .expect("must accept");
            assert_eq!(got.len(), 1, "production shape returns one logit vector");
            assert_eq!(got[0], *reference.last().unwrap(), "chunk={chunk}");
            assert_same_cache(&ref_cache, &cache, &format!("last-only chunk={chunk}"));
        }
    }

    #[test]
    fn batched_prefill_refuses_without_touching_the_cache() {
        use crate::canonical_prefill::{prefill_census, reset_prefill_census};
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let model = build_test_model(64, 32, 4, 64, 3);
        let max_seq = model.config.max_seq;
        reset_prefill_census();

        let mut cache = KVCache::new(model.config.n_layers);
        assert!(
            model
                .prefill_canonical_i8_batched(&[], &mut cache, 8, true)
                .is_none()
        );
        assert_eq!(cache.seq_len, 0);
        assert!(cache.k_data.iter().all(|k| k.is_empty()));

        assert!(
            model
                .prefill_canonical_i8_batched(&[1, 2], &mut cache, 0, true)
                .is_none()
        );
        assert_eq!(cache.seq_len, 0);
        assert!(cache.k_data.iter().all(|k| k.is_empty()));

        let too_long: Vec<u32> = vec![1; max_seq + 1];
        assert!(
            model
                .prefill_canonical_i8_batched(&too_long, &mut cache, 8, true)
                .is_none()
        );
        assert_eq!(cache.seq_len, 0);
        assert!(cache.k_data.iter().all(|k| k.is_empty()));

        let exact: Vec<u32> = vec![1; 4];
        assert!(
            model
                .prefill_canonical_i8_batched(&exact, &mut cache, 4, false)
                .is_some()
        );

        let c = prefill_census();
        assert!(c.refused_shape >= 2, "{c:?}");
        assert!(c.refused_context_window >= 1, "{c:?}");
        reset_prefill_census();
    }

    fn w1x1() -> I8Weights {
        I8Weights::quantize_f32(&[0.5f32], 1, 1)
    }

    #[test]
    fn batched_shape_predicate_accepts_the_shapes_prefill_actually_uses() {
        let w = I8Weights::quantize_f32(&[0.1f32; 8 * 4], 8, 4);
        let inputs = vec![0i64; 3 * 4];
        let output = vec![0i64; 3 * 8];
        assert!(batched_shape_is_valid(&w, &inputs, 3, 4, &output));
    }

    #[test]
    fn batched_shape_predicate_rejects_every_undersized_buffer() {
        let w = I8Weights::quantize_f32(&[0.1f32; 8 * 4], 8, 4);
        let good_in = vec![0i64; 3 * 4];
        let good_out = vec![0i64; 3 * 8];

        assert!(!batched_shape_is_valid(&w, &good_in, 3, 4, &good_out[..23]));
        assert!(!batched_shape_is_valid(&w1x1(), &[1i64], 1, 1, &[]));
        assert!(!batched_shape_is_valid(&w, &good_in[..11], 3, 4, &good_out));
        let short = I8Weights {
            data: w.data[..w.data.len() - 1].to_vec(),
            scales: w.scales.clone(),
            n_rows: w.n_rows,
            n_cols: w.n_cols,
        };
        assert!(!batched_shape_is_valid(&short, &good_in, 3, 4, &good_out));
        let scales = I8Weights {
            data: w.data.clone(),
            scales: w.scales[..w.scales.len() - 1].to_vec(),
            n_rows: w.n_rows,
            n_cols: w.n_cols,
        };
        assert!(!batched_shape_is_valid(&scales, &good_in, 3, 4, &good_out));
        assert!(!batched_shape_is_valid(&w, &[0i64; 3 * 5], 3, 5, &good_out));
        assert!(!batched_shape_is_valid(&w, &good_in, 0, 4, &good_out));
        assert!(!batched_shape_is_valid(&w, &good_in, 3, 0, &good_out));
    }

    #[test]
    fn batched_shape_predicate_rejects_overflowing_dimensions() {
        let w = w1x1();
        let huge = usize::MAX / 2 + 1;
        assert!(!batched_shape_is_valid(&w, &[1i64], huge, 4, &[0i64]));
        assert!(!batched_shape_is_valid(&w, &[1i64], 4, huge, &[0i64]));
        let wide = I8Weights {
            data: vec![0i8; 4],
            scales: vec![1i64; 2],
            n_rows: huge,
            n_cols: 2,
        };
        assert!(!batched_shape_is_valid(&wide, &[1i64, 2], 1, 2, &[0i64]));
    }

    #[test]
    #[should_panic(expected = "invalid geometry")]
    fn batched_matmul_panics_on_the_empty_output_slice() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let prev = crate::canonical_simd::fast_canonical_kernel_enabled();
        crate::canonical_simd::set_fast_canonical_kernel(false);
        let w = w1x1();
        let mut out: [i64; 0] = [];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            matmul_i8_into_batched(&w, &[1i64], 1, 1, &mut out);
        }));
        crate::canonical_simd::set_fast_canonical_kernel(prev);
        match result {
            Err(error) => std::panic::resume_unwind(error),
            Ok(()) => panic!("expected a panic but the call returned"),
        }
    }

    #[test]
    #[should_panic(expected = "invalid geometry")]
    fn batched_matmul_panics_on_undersized_input() {
        let w = I8Weights::quantize_f32(&[0.1f32; 4 * 4], 4, 4);
        let mut out = vec![0i64; 2 * 4];
        matmul_i8_into_batched(&w, &[1i64; 4], 2, 4, &mut out);
    }

    #[test]
    #[should_panic(expected = "invalid geometry")]
    fn simd_refusal_does_not_hand_an_invalid_shape_to_unsafe_scalar_code() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let prev = crate::canonical_simd::fast_canonical_kernel_enabled();
        crate::canonical_simd::set_fast_canonical_kernel(true);
        let w = w1x1();
        let mut out: [i64; 0] = [];
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            matmul_i8_into_batched(&w, &[1i64], 1, 1, &mut out);
        }));
        crate::canonical_simd::set_fast_canonical_kernel(prev);
        match r {
            Err(e) => std::panic::resume_unwind(e),
            Ok(()) => panic!("expected a panic but the call returned"),
        }
    }

    #[test]
    fn batched_matmul_still_computes_the_documented_result() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let prev = crate::canonical_simd::fast_canonical_kernel_enabled();
        crate::canonical_simd::set_fast_canonical_kernel(false);
        let w = I8Weights::quantize_f32(&[0.05f32; 6 * 3], 6, 3);
        let inputs: Vec<i64> = (0..2 * 3).map(|i| (i as i64 + 1) * 1000).collect();
        let mut batched = vec![0i64; 2 * 6];
        matmul_i8_into_batched(&w, &inputs, 2, 3, &mut batched);
        for t in 0..2 {
            let mut one = vec![0i64; 6];
            matmul_i8_into(&w, &inputs[t * 3..(t + 1) * 3], 3, &mut one);
            assert_eq!(&batched[t * 6..(t + 1) * 6], &one[..], "token {t}");
        }
        crate::canonical_simd::set_fast_canonical_kernel(prev);
    }

    #[test]
    fn serving_output_is_identical_with_batched_prefill_on_and_off() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let model = build_test_model(64, 32, 4, 64, 3);
        let prompt: Vec<u32> = vec![1, 5, 9, 13, 21, 34, 55, 2, 7, 11];
        let eos = [63u32];
        let prev_batched = crate::canonical_prefill::batched_prefill_enabled();
        let prev_simd = crate::canonical_simd::fast_canonical_kernel_enabled();

        for &simd in &[false, true] {
            if simd && !crate::canonical_simd::dotprod_available() {
                continue;
            }
            crate::canonical_simd::set_fast_canonical_kernel(simd);

            crate::canonical_prefill::set_batched_prefill_enabled(false);
            crate::canonical_prefill::reset_prefill_census();
            let (off_tokens, off_hash) = model.generate_v2(&prompt, 6, &eos);
            let off_census = crate::canonical_prefill::prefill_census();
            assert_eq!(
                off_census.chunks, 0,
                "batched prefill ran while disabled (simd={simd}): {off_census:?}"
            );

            crate::canonical_prefill::set_batched_prefill_enabled(true);
            crate::canonical_prefill::reset_prefill_census();
            let (on_tokens, on_hash) = model.generate_v2(&prompt, 6, &eos);
            let on_census = crate::canonical_prefill::prefill_census();

            assert_eq!(off_tokens, on_tokens, "served tokens differ (simd={simd})");
            assert_eq!(
                off_hash, on_hash,
                "served output hash differs (simd={simd})"
            );
            assert!(
                on_census.chunks > 0 && on_census.tokens as usize >= prompt.len(),
                "the serving path did not actually use batched prefill (simd={simd}): {on_census:?}"
            );
            assert_eq!(
                on_census.refused_total(),
                0,
                "batched prefill refused on the serving path (simd={simd}): {on_census:?}"
            );
        }
        crate::canonical_simd::set_fast_canonical_kernel(prev_simd);
        crate::canonical_prefill::set_batched_prefill_enabled(prev_batched);
        crate::canonical_prefill::reset_prefill_census();
    }

    #[test]
    fn serving_path_refuses_to_batch_below_the_profitability_floor() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let model = build_test_model(64, 32, 4, 64, 3);
        let prev = crate::canonical_prefill::batched_prefill_enabled();
        crate::canonical_prefill::set_batched_prefill_enabled(true);

        for len in 1..crate::canonical_prefill::MIN_PROFITABLE_BATCH_TOKENS {
            let prompt: Vec<u32> = (1..=len as u32).collect();
            crate::canonical_prefill::reset_prefill_census();
            let _ = model.generate_v2(&prompt, 3, &[63u32]);
            let c = crate::canonical_prefill::prefill_census();
            assert_eq!(
                c.chunks, 0,
                "a {len}-token prompt is below the floor and must not batch: {c:?}"
            );
        }

        let prompt: Vec<u32> =
            (1..=crate::canonical_prefill::MIN_PROFITABLE_BATCH_TOKENS as u32).collect();
        crate::canonical_prefill::reset_prefill_census();
        let _ = model.generate_v2(&prompt, 3, &[63u32]);
        assert!(
            crate::canonical_prefill::prefill_census().chunks > 0,
            "batching must engage at exactly the profitability floor"
        );

        crate::canonical_prefill::set_batched_prefill_enabled(prev);
        crate::canonical_prefill::reset_prefill_census();
    }

    fn legacy_prefill_trace(
        model: &CachedIntegerModel,
        prompt: &[u32],
        max_tokens: u32,
        eos: &[u32],
        use_helper: bool,
    ) -> ((Vec<u32>, Hash256), KVCache) {
        let mut cache = KVCache::new(model.config.n_layers);
        model.forward_one_token(model.config.bos_token, &mut cache);
        if use_helper {
            model.prefill_prompt_into_cache(prompt, &mut cache);
        } else {
            for &token in prompt {
                model.forward_one_token(token, &mut cache);
            }
        }
        let mut tokens = Vec::new();
        for _ in 0..max_tokens {
            let last = tokens
                .last()
                .copied()
                .unwrap_or(*prompt.last().unwrap_or(&0));
            let mut logits = model.forward_one_token(last, &mut cache);
            let next = select_next_token_with_repetition_penalty(&mut logits, &tokens);
            tokens.push(next);
            if eos.contains(&next) {
                break;
            }
        }
        let bytes: Vec<u8> = tokens.iter().flat_map(|t| t.to_le_bytes()).collect();
        ((tokens, arc_crypto::hash_bytes(&bytes)), cache)
    }

    struct RestorePrefillSwitches(bool, bool);

    impl RestorePrefillSwitches {
        fn capture() -> Self {
            Self(
                crate::canonical_prefill::batched_prefill_enabled(),
                crate::canonical_simd::fast_canonical_kernel_enabled(),
            )
        }
    }

    impl Drop for RestorePrefillSwitches {
        fn drop(&mut self) {
            crate::canonical_prefill::set_batched_prefill_enabled(self.0);
            crate::canonical_simd::set_fast_canonical_kernel(self.1);
            crate::canonical_prefill::reset_prefill_census();
        }
    }

    fn check_legacy_prefill_case(
        model: &CachedIntegerModel,
        prompt: &[u32],
        max_tokens: u32,
        eos: &[u32],
    ) {
        use crate::canonical_prefill as prefill;
        let (reference, reference_cache) =
            legacy_prefill_trace(model, prompt, max_tokens, eos, false);
        for enabled in [false, true] {
            prefill::set_batched_prefill_enabled(enabled);
            prefill::reset_prefill_census();
            let actual = model.try_generate(prompt, max_tokens, eos).unwrap();
            assert_eq!(
                actual,
                reference,
                "v1 tokens/hash: len={} max={max_tokens} eos={eos:?} enabled={enabled}",
                prompt.len()
            );
            let census = prefill::prefill_census();
            if enabled
                && prompt.len() >= prefill::MIN_PROFITABLE_BATCH_TOKENS
                && model.has_canonical_i8_profile()
            {
                let chunk =
                    prefill::SERVING_PREFILL_CHUNK.min(prefill::max_chunk_within_scratch_budget(
                        model.config.d_model,
                        model.config.d_kv,
                        model.config.d_ff,
                    ));
                assert_eq!(census.chunks as usize, prompt.len().div_ceil(chunk));
                assert_eq!(census.tokens as usize, prompt.len());
                assert_eq!(census.refused_total(), 0);
            } else {
                assert_eq!(census.chunks, 0);
            }
            let (traced, cache) = legacy_prefill_trace(model, prompt, max_tokens, eos, true);
            assert_eq!(traced, actual);
            assert_same_cache(
                &reference_cache,
                &cache,
                "legacy generation, BOS + prompt + decode",
            );
            assert_eq!(cache.seq_len, 1 + prompt.len() + actual.0.len());
        }
    }

    #[test]
    fn legacy_batched_prefill_preserves_tokens_hash_and_kv() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let _restore = RestorePrefillSwitches::capture();
        let legacy = build_test_model(64, 32, 4, 64, 3);
        let mut interleaved = build_test_model(32, 16, 2, 32, 2);
        interleaved
            .canonicalize_gguf_interleaved_rope_rows()
            .unwrap();
        let mut fallback = build_test_model(32, 16, 2, 32, 2);
        fallback.enable_i16();
        assert!(!fallback.has_canonical_i8_profile());
        for simd in [false, true] {
            crate::canonical_simd::set_fast_canonical_kernel(simd);
            let mut checked_mid_generation_eos = false;
            for model in [&legacy, &interleaved, &fallback] {
                for len in [0, 1, 3, 4, 5, 63, 64, 65, 130] {
                    let prompt: Vec<u32> = (0..len)
                        .map(|i| (i * 7 + 3) as u32 % model.config.vocab_size as u32)
                        .collect();
                    for max_tokens in [1, 8] {
                        check_legacy_prefill_case(model, &prompt, max_tokens, &[]);
                    }
                    let ((tokens, _), _) = legacy_prefill_trace(model, &prompt, 8, &[], false);
                    if let Some((index, token)) = tokens
                        .iter()
                        .enumerate()
                        .skip(1)
                        .take(6)
                        .find(|(index, token)| !tokens[..*index].contains(token))
                    {
                        check_legacy_prefill_case(model, &prompt, 8, &[*token]);
                        assert_eq!(
                            model.try_generate(&prompt, 8, &[*token]).unwrap().0.len(),
                            index + 1
                        );
                        checked_mid_generation_eos = true;
                    }
                }
            }
            assert!(checked_mid_generation_eos);
        }
    }

    #[test]
    fn legacy_batched_prefill_clamps_scratch_and_preserves_kv() {
        use crate::canonical_prefill as prefill;
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let _restore = RestorePrefillSwitches::capture();
        let model = build_test_model(8, 4, 1, 180_000, 1);
        let cfg = &model.config;
        let chunk = prefill::max_chunk_within_scratch_budget(cfg.d_model, cfg.d_kv, cfg.d_ff);
        assert!(chunk < prefill::SERVING_PREFILL_CHUNK);
        assert!(
            prefill::prefill_chunk_scratch_bytes(
                prefill::SERVING_PREFILL_CHUNK,
                cfg.d_model,
                cfg.d_kv,
                cfg.d_ff
            ) > prefill::MAX_PREFILL_SCRATCH_BYTES
        );
        let prompt: Vec<u32> = (0..65).map(|i| (i % 8) as u32).collect();
        check_legacy_prefill_case(&model, &prompt, 1, &[]);
    }

    fn build_test_model(
        vs: usize,
        d: usize,
        nh: usize,
        dff: usize,
        nl: usize,
    ) -> CachedIntegerModel {
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
                arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
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
            i16_output: None,
            ternary_hybrid_layers: None,
            ternary_hybrid_output: None,
            block_i8_layers: None,
            block_i8_output: None,
            ternary_layers: None,
            ternary_output: None,
        }
    }

    #[test]
    fn full_worker_requires_every_configured_transformer_layer() {
        let full = build_test_model(32, 16, 2, 32, 3);
        assert!(full.has_all_transformer_layers());

        let mut partial = build_test_model(32, 16, 2, 32, 3);
        partial.layers[1] = CachedLayer::placeholder();
        assert!(!partial.has_all_transformer_layers());

        let mut incomplete_layer = build_test_model(32, 16, 2, 32, 1);
        incomplete_layer.layers[0].w_down = I8Weights::empty();
        assert!(!incomplete_layer.has_all_transformer_layers());

        let mut tokenizer_only = build_test_model(32, 16, 2, 32, 1);
        tokenizer_only.layers[0] = CachedLayer::placeholder();
        assert!(!tokenizer_only.has_all_transformer_layers());

        let mut inconsistent = build_test_model(32, 16, 2, 32, 1);
        inconsistent.config.n_layers = 2;
        assert!(!inconsistent.has_all_transformer_layers());
    }

    #[test]
    fn generation_preflight_and_execution_accept_exact_context_boundary() {
        let mut model = build_test_model(32, 16, 2, 32, 1);
        model.config.max_seq = 6;
        let prompt = [3, 4];

        let admission = model
            .preflight_generation(prompt.len(), 3)
            .expect("1 BOS + 2 prompt + 3 generated exactly fills six positions");
        assert_eq!(admission.prompt_tokens, 2);
        assert_eq!(admission.max_tokens, 3);
        assert_eq!(admission.required_positions, 6);
        assert_eq!(admission.max_seq, 6);

        let (generated, _) = model
            .try_generate(&prompt, 3, &[])
            .expect("the exact context boundary must execute without indexing past RoPE");
        assert_eq!(generated.len(), 3);
    }

    #[test]
    fn generation_one_position_over_returns_error_before_compute() {
        let mut model = build_test_model(32, 16, 2, 32, 1);
        model.config.max_seq = 6;
        let prompt = [3, 4];

        model.embedding_q16.clear();
        let err = model
            .try_generate(&prompt, 4, &[])
            .expect_err("1 BOS + 2 prompt + 4 generated is one over six");

        assert_eq!(err.kind(), "context_window_exceeded");
        assert_eq!(
            err,
            GenerationError::ContextWindowExceeded {
                prompt_tokens: 2,
                max_tokens: 4,
                required_positions: 7,
                max_seq: 6,
            }
        );
    }

    #[test]
    fn q_and_k_row_permutation_matches_interleaved_rope_oracle_for_multihead_gqa() {
        let mut q = I8Weights {
            data: (0i8..8).flat_map(|row| [row, -row]).collect(),
            scales: (0i64..8).collect(),
            n_rows: 8,
            n_cols: 2,
        };
        let mut k = I8Weights {
            data: (20i8..24).flat_map(|row| [row, -row]).collect(),
            scales: (20i64..24).collect(),
            n_rows: 4,
            n_cols: 2,
        };
        permute_interleaved_rows_to_split_half(&mut q, 2, 4, "q").unwrap();
        permute_interleaved_rows_to_split_half(&mut k, 1, 4, "k").unwrap();
        assert_eq!(q.scales, vec![0, 2, 1, 3, 4, 6, 5, 7]);
        assert_eq!(k.scales, vec![20, 22, 21, 23]);
        assert_eq!(
            q.data,
            vec![0, 0, 2, -2, 1, -1, 3, -3, 4, -4, 6, -6, 5, -5, 7, -7]
        );

        let (cos, sin) = compute_rope_tables(4, 2, 10_000.0);
        let original = vec![11 * ONE, 13 * ONE, 17 * ONE, 19 * ONE];
        let mut reference = original.clone();
        apply_rope_interleaved(&mut reference, 1, 4, &cos, &sin);

        let mut split = vec![original[0], original[2], original[1], original[3]];
        apply_rope(&mut split, 1, 4, &cos, &sin);
        let restored = vec![split[0], split[2], split[1], split[3]];
        assert_eq!(restored, reference);

        let original_k = vec![23 * ONE, 29 * ONE, 31 * ONE, 37 * ONE];
        let mut reference_k = original_k.clone();
        apply_rope_interleaved(&mut reference_k, 1, 4, &cos, &sin);
        let mut split_k = vec![original_k[0], original_k[2], original_k[1], original_k[3]];
        apply_rope(&mut split_k, 1, 4, &cos, &sin);
        let dot = |left: &[i64], right: &[i64]| -> i128 {
            left.iter()
                .zip(right)
                .map(|(a, b)| *a as i128 * *b as i128)
                .sum()
        };
        assert_eq!(dot(&reference, &reference_k), dot(&split, &split_k));
    }

    fn shard_output_fingerprint(output: ShardOutput) -> (Vec<i64>, Option<(u32, Hash256)>) {
        match output {
            ShardOutput::Hidden(hidden) => (hidden, None),
            ShardOutput::Token { id, logits_hash } => (Vec::new(), Some((id, logits_hash))),
        }
    }

    #[test]
    fn interleaved_rope_qk_copy_runs_ranges_exactly_like_the_corrected_whole_model() {
        let legacy = build_test_model(20, 32, 2, 64, 4);
        let mut corrected = build_test_model(20, 32, 2, 64, 4);
        corrected.canonicalize_gguf_interleaved_rope_rows().unwrap();
        let copy = InterleavedRopeQk::from_resident_layers(&legacy).unwrap();
        assert_eq!(copy.layer_count(), 4);
        assert!(copy.covers(0, 4));

        let n_layers = legacy.config.n_layers;
        let split = 2;
        let mut corrected_cache = KVCache::new(n_layers);
        let mut copy_cache = KVCache::new(n_layers);
        let mut legacy_cache = KVCache::new(n_layers);
        let mut profiles_differ = false;
        for (position, token) in [1u32, 5, 7, 3, 9].into_iter().enumerate() {
            let whole = corrected
                .forward_shard_token(
                    ShardInput::Token(token),
                    &mut corrected_cache,
                    0,
                    n_layers,
                    position,
                )
                .unwrap();
            let first = legacy
                .forward_shard_token_with_history_interleaved_rope(
                    ShardInput::Token(token),
                    &mut copy_cache,
                    0,
                    split,
                    position,
                    &[],
                    &copy,
                )
                .unwrap();
            let ShardOutput::Hidden(hidden) = first else {
                panic!("a nonterminal range returns a hidden state");
            };
            let ranged = legacy
                .forward_shard_token_with_history_interleaved_rope(
                    ShardInput::Hidden(hidden),
                    &mut copy_cache,
                    split,
                    n_layers,
                    position,
                    &[],
                    &copy,
                )
                .unwrap();
            let whole = shard_output_fingerprint(whole);
            assert_eq!(
                whole,
                shard_output_fingerprint(ranged),
                "position {position}"
            );
            let legacy_output = legacy
                .forward_shard_token(
                    ShardInput::Token(token),
                    &mut legacy_cache,
                    0,
                    n_layers,
                    position,
                )
                .unwrap();
            profiles_differ |= shard_output_fingerprint(legacy_output) != whole;
        }
        assert!(
            profiles_differ,
            "this model must tell the two profiles apart"
        );
    }

    #[test]
    fn interleaved_rope_qk_copy_covers_and_accounts_for_resident_layers_only() {
        let mut model = build_test_model(20, 32, 2, 64, 4);
        model.layers[0] = CachedLayer::placeholder();
        let copy = InterleavedRopeQk::from_resident_layers(&model).unwrap();
        assert_eq!(copy.layer_count(), 3);
        assert!(!copy.covers(0, 4));
        assert!(copy.covers(1, 4));
        assert!(!copy.covers(2, 2));
        let resident = &model.layers[1..];
        assert_eq!(
            copy.memory_bytes(),
            resident
                .iter()
                .map(|layer| layer.wq.memory_bytes() + layer.wk.memory_bytes())
                .sum::<usize>()
        );
        assert_eq!(
            copy.weight_bytes(),
            resident
                .iter()
                .map(|layer| {
                    layer.wq.data.len()
                        + layer.wk.data.len()
                        + 8 * (layer.wq.scales.len() + layer.wk.scales.len())
                })
                .sum::<usize>()
        );

        let mut cache = KVCache::new(model.config.n_layers);
        let error = model
            .forward_shard_token_with_history_interleaved_rope(
                ShardInput::Hidden(vec![0; model.config.d_model]),
                &mut cache,
                1,
                4,
                0,
                &[],
                &InterleavedRopeQk {
                    layers: vec![None, None, None, None],
                },
            )
            .unwrap_err();
        assert_eq!(
            error,
            ShardForwardError::InterleavedRopeUnavailable { layer: 1 }
        );
        assert_eq!(error.kind(), "interleaved_rope_unavailable");
        assert!(cache.k_data.iter().all(Vec::is_empty));
    }

    #[test]
    fn precision_label_names_the_resident_rope_profile_and_the_copy_needs_legacy_i8() {
        let mut model = build_test_model(16, 8, 2, 16, 1);
        assert_eq!(
            model.effective_precision_label(),
            CANONICAL_REWARD_INFERENCE_PROFILE
        );
        assert!(InterleavedRopeQk::from_resident_layers(&model).is_ok());
        model.canonicalize_gguf_interleaved_rope_rows().unwrap();
        assert_eq!(
            model.effective_precision_label(),
            GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE
        );
        assert!(InterleavedRopeQk::from_resident_layers(&model).is_err());
        let mut promoted = build_test_model(16, 8, 2, 16, 1);
        promoted.enable_i16();
        assert!(InterleavedRopeQk::from_resident_layers(&promoted).is_err());
    }

    #[test]
    fn interleaved_rope_profile_is_versioned_and_legacy_cache_export_is_refused() {
        let mut legacy = build_test_model(16, 8, 2, 16, 1);
        assert_eq!(
            legacy.arithmetic_profile(),
            CANONICAL_REWARD_INFERENCE_PROFILE
        );
        legacy.canonicalize_gguf_interleaved_rope_rows().unwrap();
        assert_eq!(
            legacy.arithmetic_profile(),
            GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE
        );
        assert!(legacy.has_canonical_i8_profile());
        let error = legacy
            .save_weights("/definitely-not-written/arc-interleaved-v1.arc-int8")
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("arithmetic-profile tag"));
    }

    #[test]
    fn a_binary_cache_with_a_short_or_empty_norm_is_refused() {
        let path = std::env::temp_dir().join(format!(
            "arc-d3-binary-norm-{}.arc-int8",
            std::process::id()
        ));
        let path = path.to_str().unwrap();
        let model = build_test_model(16, 32, 2, 64, 2);
        model.save_weights(path).unwrap();
        assert!(
            load_cached_model_binary(path).is_ok(),
            "an intact cache loads"
        );

        let mut short = build_test_model(16, 32, 2, 64, 2);
        short.layers[1].ffn_norm.truncate(1);
        short.save_weights(path).unwrap();
        let error = load_cached_model_binary(path)
            .err()
            .expect("a short norm is refused");
        assert!(error.to_string().contains("L1 FFN norm"), "{error}");

        let mut empty = build_test_model(16, 32, 2, 64, 2);
        empty.final_norm.clear();
        empty.save_weights(path).unwrap();
        let error = load_cached_model_binary(path)
            .err()
            .expect("an empty final norm is refused");
        assert!(error.to_string().contains("final norm"), "{error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generation_preflight_rejects_position_count_overflow() {
        let model = build_test_model(32, 16, 2, 32, 1);
        let err = model
            .preflight_generation(usize::MAX, 0)
            .expect_err("the internal BOS addition must use checked arithmetic");

        assert_eq!(err.kind(), "position_count_overflow");
        assert_eq!(
            err,
            GenerationError::PositionCountOverflow {
                prompt_tokens: usize::MAX,
                max_tokens: 0,
            }
        );
    }

    #[test]
    fn generation_v2_matches_explicit_single_bos_prompt_logit_oracle() {
        let model = build_test_model(32, 16, 2, 32, 1);
        let prompt = [3u32, 4u32];
        let eos = [u32::MAX];

        let mut cache = KVCache::new(model.config.n_layers);
        let mut logits = model.forward_one_token(model.config.bos_token, &mut cache);
        for &token in &prompt {
            logits = model.forward_one_token(token, &mut cache);
        }
        let mut expected = Vec::new();
        for _ in 0..3 {
            let next = select_next_token_with_repetition_penalty(&mut logits, &expected);
            expected.push(next);
            if eos.contains(&next) {
                break;
            }
            logits = model.forward_one_token(next, &mut cache);
        }

        let (actual, _) = model.generate_v2(&prompt, 3, &eos);
        assert_eq!(
            actual, expected,
            "v2 must reuse final prompt logits exactly"
        );

        let (empty, _) = model.generate_v2(&[], 1, &eos);
        assert_eq!(
            empty.len(),
            1,
            "empty prompt still uses the single BOS logits"
        );
        let (zero, _) = model.generate_v2(&prompt, 0, &eos);
        assert!(zero.is_empty(), "zero token budget must produce no output");
    }

    #[test]
    fn generation_v2_greedy_matches_raw_argmax_single_bos_oracle() {
        let model = build_test_model(32, 16, 2, 32, 1);
        let prompt = [3u32, 4u32];
        let eos = [u32::MAX];
        let mut cache = KVCache::new(model.config.n_layers);
        let mut logits = model.forward_one_token(model.config.bos_token, &mut cache);
        for &token in &prompt {
            logits = model.forward_one_token(token, &mut cache);
        }
        let mut expected = Vec::new();
        for _ in 0..3 {
            let next = argmax_i64(&logits) as u32;
            expected.push(next);
            logits = model.forward_one_token(next, &mut cache);
        }
        let (actual, _) = model
            .try_generate_v2_greedy(&prompt, 3, &eos)
            .expect("small test prompt fits context");
        assert_eq!(actual, expected);
    }

    #[test]
    fn v2_decoder_reconstructs_byte_fallback_utf8() {
        let mut model = build_test_model(32, 16, 2, 32, 1);
        model.vocab[13] = "<0x0A>".into();
        assert_eq!(model.decode_v2(&[13, 13]), "\n\n");
    }

    #[test]
    fn v2_content_decoder_stops_before_eos_and_suppresses_bos() {
        let mut model = build_test_model(32, 16, 2, 32, 1);
        model.vocab[1] = "<s>".into();
        model.vocab[2] = "</s>".into();
        model.vocab[3] = "▁Hello".into();
        model.vocab[4] = "<0x21>".into();
        model.config.eos_tokens = vec![2];
        assert_eq!(model.decode_v2(&[3, 4, 2]), " Hello!</s>");
        assert_eq!(model.decode_v2_content(&[1, 3, 4, 2, 3]), " Hello!");
    }

    #[test]
    fn canonical_community_profile_matches_validator_i8_and_detects_i16_drift() {
        let mut worker = build_test_model(32, 16, 2, 32, 2);
        worker.enable_i16();
        assert!(!worker.has_canonical_i8_profile());
        worker.enforce_canonical_i8_profile();
        assert!(worker.has_canonical_i8_profile());
        assert_eq!(
            worker.effective_precision_label(),
            CANONICAL_REWARD_INFERENCE_PROFILE
        );

        let validator = build_test_model(32, 16, 2, 32, 2);
        assert!(validator.has_canonical_i8_profile());
        let prompt = [worker.config.bos_token, 3, 4];
        let (worker_tokens, worker_hash) = worker.generate(&prompt, 2, &worker.config.eos_tokens);
        let (validator_tokens, validator_hash) =
            validator.generate(&prompt, 2, &validator.config.eos_tokens);
        assert_eq!(worker_tokens, validator_tokens);
        assert_eq!(worker_hash, validator_hash);

        let mut drifted = build_test_model(32, 16, 2, 32, 2);
        drifted.enable_i16();
        assert_eq!(
            drifted.effective_precision_label(),
            "INT16 integer (per-row, cross-platform deterministic)"
        );
        assert!(!drifted.has_canonical_i8_profile());
    }

    #[test]
    fn test_per_row_quantize_precision() {
        let mut values = vec![0.01f32; 8];
        values.extend(vec![10.0, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01]);
        let q = I8Weights::quantize_f32(&values, 2, 8);

        assert_eq!(q.data[0], 127, "Row 0 should use full range");
        assert_eq!(q.data[8], 127, "Row 1 outlier should be 127");
        assert!(q.scales[0] < q.scales[1], "Row 0 should have smaller scale");
    }

    fn projection_keys(model: &CachedIntegerModel) -> Vec<(Option<usize>, TensorKey)> {
        let mut keys = Vec::new();
        for layer in 0..model.layers.len() {
            for tensor in [
                TensorKey::Wq,
                TensorKey::Wk,
                TensorKey::Wv,
                TensorKey::Wo,
                TensorKey::WGate,
                TensorKey::WUp,
                TensorKey::WDown,
            ] {
                keys.push((Some(layer), tensor));
            }
        }
        keys.push((None, TensorKey::LmHead));
        keys
    }

    fn halved_row_backend(
        model: &CachedIntegerModel,
        artifact: Hash256,
    ) -> PartitionedProjectionBackend {
        let profile = model.effective_precision_label().to_string();
        let mut assignments = Vec::new();
        let mut workers: BTreeMap<String, Arc<dyn RowWorker>> = BTreeMap::new();
        for (layer, tensor) in projection_keys(model) {
            let weights =
                crate::tensor_parallel::model_projection_weights(model, layer, tensor).unwrap();
            let split = weights.n_rows / 2;
            for (start, end) in [(0, split), (split, weights.n_rows)] {
                let worker_id = format!("{layer:?}-{tensor:?}-{start}");
                let assignment = RowAssignment {
                    artifact_id: artifact,
                    execution_profile: profile.clone(),
                    layer,
                    tensor,
                    row_start: start,
                    row_end: end,
                    worker_id: worker_id.clone(),
                };
                workers.insert(
                    worker_id.clone(),
                    Arc::new(LocalRowWorker {
                        worker_id,
                        assignment: assignment.clone(),
                        weights: weights.copy_rows(start, end).unwrap(),
                    }),
                );
                assignments.push(assignment);
            }
        }
        PartitionedProjectionBackend::new(artifact, profile, assignments, workers)
    }

    #[test]
    fn generation_on_a_row_backend_equals_local_generation_token_for_token() {
        let model = build_test_model(16, 32, 2, 64, 3);
        let backend = halved_row_backend(&model, Hash256([77; 32]));
        let request = Hash256([5; 32]);
        let prompt = [1u32, 2, 3];
        let local = model.try_generate_v2(&prompt, 4, &[99]).unwrap();
        let rows = model
            .try_generate_v2_with_backend(request, &prompt, 4, &[99], &backend)
            .unwrap();
        assert_eq!(rows, local);
        let (open_ended, _) = model.try_generate_v2(&prompt, 6, &[]).unwrap();
        let eos = [open_ended[1]];
        let local = model.try_generate_v2(&prompt, 6, &eos).unwrap();
        assert!(local.0.len() <= 2, "EOS stopped the local generation");
        let rows = model
            .try_generate_v2_with_backend(request, &prompt, 6, &eos, &backend)
            .unwrap();
        assert_eq!(rows, local);
        for (prompt, max_tokens) in [(&[][..], 3), (&prompt[..], 0)] {
            let local = model.try_generate_v2(prompt, max_tokens, &[99]).unwrap();
            let rows = model
                .try_generate_v2_with_backend(request, prompt, max_tokens, &[99], &backend)
                .unwrap();
            assert_eq!(rows, local);
        }
        let too_long = vec![1u32; model.config.max_seq];
        assert!(matches!(
            model.try_generate_v2_with_backend(request, &too_long, 4, &[99], &backend),
            Err(BackendGenerationError::Generation(_))
        ));
        let empty = PartitionedProjectionBackend::new(
            Hash256([77; 32]),
            model.effective_precision_label().to_string(),
            Vec::new(),
            BTreeMap::new(),
        );
        assert!(matches!(
            model.try_generate_v2_with_backend(request, &prompt, 4, &[99], &empty),
            Err(BackendGenerationError::Projection(_))
        ));
    }

    struct WholeModelWorker {
        model: Arc<CachedIntegerModel>,
        fail: bool,
        lie: bool,
        closed: bool,
        fail_after: Option<usize>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl WholeModelWorker {
        fn new(model: &Arc<CachedIntegerModel>, fail: bool, lie: bool) -> Self {
            Self {
                model: model.clone(),
                fail,
                lie,
                closed: false,
                fail_after: None,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl RowWorker for WholeModelWorker {
        fn is_open(&self) -> bool {
            !self.closed
        }

        fn project(
            &self,
            request: crate::tensor_parallel::RowProjectionRequest,
        ) -> Result<
            crate::tensor_parallel::RowProjectionResponse,
            crate::tensor_parallel::TensorParallelError,
        > {
            use crate::tensor_parallel::TensorParallelError;
            let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail || self.fail_after.is_some_and(|after| index >= after) {
                return Err(TensorParallelError::Worker("unreachable".into()));
            }
            let a = &request.assignment;
            let weights =
                crate::tensor_parallel::model_projection_weights(&self.model, a.layer, a.tensor)
                    .ok_or(TensorParallelError::WrongIdentity)?;
            let shard = weights
                .copy_rows(a.row_start, a.row_end)
                .map_err(|_| TensorParallelError::WrongShape)?;
            let mut values = vec![0; shard.n_rows];
            matmul_i8_canonical_rows(&shard, &request.input, &mut values)
                .map_err(|_| TensorParallelError::WrongShape)?;
            if self.lie {
                values[0] += 1;
            }
            Ok(crate::tensor_parallel::RowProjectionResponse {
                call_id: request.call_id,
                input_hash: request.input_hash,
                assignment: request.assignment,
                values,
            })
        }
    }

    #[derive(Default)]
    struct Events {
        seen: std::sync::Mutex<Vec<crate::tensor_parallel::RowEvent>>,
        excluded: std::collections::BTreeSet<String>,
    }

    impl crate::tensor_parallel::RowEventSink for Events {
        fn record(&self, event: crate::tensor_parallel::RowEvent) {
            self.seen.lock().unwrap().push(event);
        }
        fn is_excluded(&self, worker: &str) -> bool {
            self.excluded.contains(worker)
        }
    }

    fn half_remote_plans(
        model: &CachedIntegerModel,
        duplicate_on: Option<crate::tensor_parallel::SliceOwner>,
        spot_first_remote_row: bool,
    ) -> BTreeMap<(Option<usize>, TensorKey), crate::tensor_parallel::ProjectionPlan> {
        use crate::tensor_parallel::{PlannedSlice, ProjectionPlan, SliceOwner};
        let mut plans = BTreeMap::new();
        for (layer, tensor) in projection_keys(model) {
            let rows = crate::tensor_parallel::model_projection_weights(model, layer, tensor)
                .unwrap()
                .n_rows;
            let split = rows / 2;
            plans.insert(
                (layer, tensor),
                ProjectionPlan {
                    slices: vec![
                        PlannedSlice {
                            owner: SliceOwner::Local,
                            row_start: 0,
                            row_end: split,
                            duplicate_on: None,
                        },
                        PlannedSlice {
                            owner: SliceOwner::Remote("w".into()),
                            row_start: split,
                            row_end: rows,
                            duplicate_on: duplicate_on.clone(),
                        },
                    ],
                    spot_rows: if spot_first_remote_row {
                        vec![split]
                    } else {
                        Vec::new()
                    },
                },
            );
        }
        plans
    }

    fn run_verified(
        model: &Arc<CachedIntegerModel>,
        worker: WholeModelWorker,
        plans: BTreeMap<(Option<usize>, TensorKey), crate::tensor_parallel::ProjectionPlan>,
        excluded: &[&str],
    ) -> (
        (Vec<u32>, Hash256),
        Vec<crate::tensor_parallel::RowEvent>,
        usize,
    ) {
        let events = Events {
            excluded: excluded.iter().map(|w| w.to_string()).collect(),
            ..Default::default()
        };
        let worker = Arc::new(worker);
        let mut workers: BTreeMap<String, Arc<dyn RowWorker>> = BTreeMap::new();
        workers.insert("w".into(), worker.clone());
        let backend = crate::tensor_parallel::VerifiedPartitionBackend::new(
            model,
            Hash256([77; 32]),
            plans,
            workers,
            &events,
        )
        .unwrap();
        let output = model
            .try_generate_v2_with_backend(Hash256([5; 32]), &[1, 2, 3], 4, &[99], &backend)
            .unwrap();
        drop(backend);
        let calls = worker.calls.load(std::sync::atomic::Ordering::SeqCst);
        (output, events.seen.into_inner().unwrap(), calls)
    }

    fn all_remote_plans(
        model: &CachedIntegerModel,
    ) -> BTreeMap<(Option<usize>, TensorKey), crate::tensor_parallel::ProjectionPlan> {
        let mut plans =
            half_remote_plans(model, Some(crate::tensor_parallel::SliceOwner::Local), true);
        for plan in plans.values_mut() {
            plan.slices.remove(0);
            plan.slices[0].row_start = 0;
            plan.spot_rows = vec![0];
        }
        plans
    }

    #[test]
    fn strict_backend_checks_numerical_and_response_identity_failures_without_fallback() {
        use crate::tensor_parallel::{
            RowEvent, RowProjectionRequest, RowProjectionResponse, TensorParallelError,
            VerifiedPartitionBackend,
        };
        struct Altered {
            inner: WholeModelWorker,
            mode: u8,
        }
        impl RowWorker for Altered {
            fn project(
                &self,
                request: RowProjectionRequest,
            ) -> Result<RowProjectionResponse, TensorParallelError> {
                if self.mode == 1 {
                    return Err(TensorParallelError::Worker("timeout".into()));
                }
                let mut response = self.inner.project(request)?;
                match self.mode {
                    2 => response.call_id = Hash256([0; 32]),
                    3 => response.input_hash = Hash256([0; 32]),
                    4 => response.assignment.artifact_id = Hash256([0; 32]),
                    5 => response.assignment.execution_profile = "other".into(),
                    6 => response.assignment.layer = Some(99),
                    7 => response.assignment.tensor = TensorKey::LmHead,
                    8 => response.assignment.worker_id = "other".into(),
                    9 => {
                        response.values.pop();
                    }
                    10 => response.values[0] += 1,
                    _ => (),
                }
                Ok(response)
            }
        }
        let model = Arc::new(build_test_model(16, 32, 2, 64, 2));
        for mode in 0..=10 {
            let events = Events::default();
            let worker = Arc::new(Altered {
                inner: WholeModelWorker::new(&model, false, false),
                mode,
            });
            let workers: BTreeMap<String, Arc<dyn RowWorker>> =
                [("w".into(), worker as Arc<dyn RowWorker>)]
                    .into_iter()
                    .collect();
            let backend = VerifiedPartitionBackend::new_strict(
                model.as_ref(),
                Hash256([77; 32]),
                all_remote_plans(&model),
                workers,
                &events,
            )
            .unwrap();
            let result =
                model.try_generate_v2_with_backend(Hash256([5; 32]), &[3, 4], 2, &[99], &backend);
            if mode == 0 {
                assert_eq!(
                    result.unwrap(),
                    model.try_generate_v2(&[3, 4], 2, &[99]).unwrap()
                );
            } else {
                assert!(result.is_err(), "mode {mode} accepted");
            }
            let events = events.seen.lock().unwrap();
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, RowEvent::Fallback { .. })),
                "strict mode silently fell back"
            );
            if mode == 10 {
                assert!(events.iter().any(|e| matches!(e, RowEvent::Fault { .. })));
            }
        }
    }

    #[test]
    fn strict_backend_refuses_missing_coverage_before_any_worker_call() {
        use crate::tensor_parallel::{SliceOwner, VerifiedPartitionBackend};
        let model = Arc::new(build_test_model(16, 32, 2, 64, 2));
        let worker = Arc::new(WholeModelWorker::new(&model, false, false));
        let workers: BTreeMap<String, Arc<dyn RowWorker>> =
            [("w".into(), worker.clone() as Arc<dyn RowWorker>)]
                .into_iter()
                .collect();
        let events = Events::default();
        for mode in 0..6 {
            let mut plans = all_remote_plans(&model);
            if mode == 0 {
                plans.remove(&(None, TensorKey::LmHead));
            } else {
                let stage = plans.get_mut(&(Some(0), TensorKey::Wq)).unwrap();
                match mode {
                    1 => stage.slices[0].row_start = 1,
                    2 => stage.slices[0].row_end += 1,
                    3 => stage.slices[0].owner = SliceOwner::Local,
                    4 => stage.spot_rows.clear(),
                    5 => stage.slices[0].duplicate_on = Some(SliceOwner::Remote("missing".into())),
                    _ => unreachable!(),
                }
            }
            assert!(
                VerifiedPartitionBackend::new_strict(
                    model.as_ref(),
                    Hash256([77; 32]),
                    plans,
                    workers.clone(),
                    &events
                )
                .is_err()
            );
        }
        assert_eq!(worker.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn strict_backend_refuses_worker_loss_mid_generation_without_partial_output() {
        use crate::tensor_parallel::{RowEvent, VerifiedPartitionBackend};
        let model = Arc::new(build_test_model(16, 32, 2, 64, 2));
        let mut worker = WholeModelWorker::new(&model, false, false);
        worker.fail_after = Some(20);
        let worker = Arc::new(worker);
        let workers: BTreeMap<String, Arc<dyn RowWorker>> =
            [("w".into(), worker.clone() as Arc<dyn RowWorker>)]
                .into_iter()
                .collect();
        let events = Events::default();
        let backend = VerifiedPartitionBackend::new_strict(
            model.as_ref(),
            Hash256([77; 32]),
            all_remote_plans(&model),
            workers,
            &events,
        )
        .unwrap();
        assert!(
            model
                .try_generate_v2_with_backend(Hash256([5; 32]), &[3, 4], 2, &[99], &backend)
                .is_err()
        );
        assert!(worker.calls.load(std::sync::atomic::Ordering::SeqCst) > 20);
        assert!(
            !events
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, RowEvent::Fallback { .. }))
        );
    }

    #[test]
    fn a_verified_partition_backend_keeps_the_output_exact_and_reports_every_fallback_and_fault() {
        use crate::tensor_parallel::{RowEvent, SliceOwner};
        let model = Arc::new(build_test_model(16, 32, 2, 64, 3));
        let local = model.try_generate_v2(&[1, 2, 3], 4, &[99]).unwrap();
        let is_fault = |event: &RowEvent| matches!(event, RowEvent::Fault { .. });
        let is_fallback = |event: &RowEvent| matches!(event, RowEvent::Fallback { .. });

        let (output, events, calls) = run_verified(
            &model,
            WholeModelWorker::new(&model, false, false),
            half_remote_plans(&model, Some(SliceOwner::Local), true),
            &[],
        );
        assert_eq!(output, local);
        assert!(calls > 0);
        assert!(
            events
                .iter()
                .all(|e| matches!(e, RowEvent::Answered { .. }))
        );

        let (output, events, calls) = run_verified(
            &model,
            WholeModelWorker::new(&model, true, false),
            half_remote_plans(&model, None, false),
            &[],
        );
        assert_eq!(output, local);
        assert_eq!(events.iter().filter(|e| is_fallback(e)).count(), calls);
        assert!(!events.iter().any(is_fault));

        let dropout = WholeModelWorker {
            fail_after: Some(5),
            ..WholeModelWorker::new(&model, false, false)
        };
        let (output, events, calls) =
            run_verified(&model, dropout, half_remote_plans(&model, None, false), &[]);
        assert_eq!(output, local);
        assert!(calls > 5, "the query outlived the worker");
        let answered = events
            .iter()
            .filter(|e| matches!(e, RowEvent::Answered { .. }))
            .count();
        assert_eq!(answered, 5);
        assert_eq!(events.iter().filter(|e| is_fallback(e)).count(), calls - 5);
        assert!(!events.iter().any(is_fault));

        for (duplicate, spot) in [(Some(SliceOwner::Local), false), (None, true)] {
            let (output, events, _) = run_verified(
                &model,
                WholeModelWorker::new(&model, false, true),
                half_remote_plans(&model, duplicate, spot),
                &[],
            );
            assert_eq!(output, local);
            assert!(events.iter().any(is_fault));
        }

        let events = Events::default();
        let mut workers: BTreeMap<String, Arc<dyn RowWorker>> = BTreeMap::new();
        workers.insert(
            "w".into(),
            Arc::new(WholeModelWorker::new(&model, false, true)),
        );
        let backend = crate::tensor_parallel::VerifiedPartitionBackend::new(
            &model,
            Hash256([77; 32]),
            half_remote_plans(&model, None, false),
            workers,
            &events,
        )
        .unwrap();
        let mut local_cache = KVCache::new(model.config.n_layers);
        let mut lied_cache = KVCache::new(model.config.n_layers);
        let local_logits = model.forward_one_token(model.config.bos_token, &mut local_cache);
        let lied_logits = model
            .forward_one_token_canonical_i8_with_backend(
                model.config.bos_token,
                &mut lied_cache,
                Hash256([1; 32]),
                &backend,
            )
            .unwrap();
        assert_ne!(lied_logits, local_logits);
        drop(backend);

        let (output, events, calls) = run_verified(
            &model,
            WholeModelWorker::new(&model, false, true),
            half_remote_plans(&model, None, false),
            &["w"],
        );
        assert_eq!(output, local);
        assert_eq!(calls, 0);
        assert!(!events.is_empty());
        assert!(events.iter().all(|e| matches!(e, RowEvent::Skipped { .. })));

        let mut closed = WholeModelWorker::new(&model, false, false);
        closed.closed = true;
        let (output, events, calls) =
            run_verified(&model, closed, half_remote_plans(&model, None, false), &[]);
        assert_eq!(output, local);
        assert_eq!(calls, 0);
        assert!(!events.is_empty());
        assert!(events.iter().all(|e| matches!(e, RowEvent::Skipped { .. })));
    }

    #[test]
    fn the_row_frame_server_answers_any_assignment_with_the_models_own_rows() {
        use crate::tensor_parallel::{
            RowProjectionRequest, decode_row_response, encode_row_request, hash_i64,
            model_projection_weights, serve_row_frames,
        };
        let model = build_test_model(16, 32, 2, 64, 3);
        let artifact = Hash256([77; 32]);
        let profile = model.canonical_execution_profile().unwrap().to_string();
        let input: Vec<i64> = (0..model.config.d_model as i64)
            .map(|i| (i - 7) << 12)
            .collect();
        let request = RowProjectionRequest {
            call_id: Hash256([9; 32]),
            input_hash: hash_i64(&input),
            assignment: RowAssignment {
                artifact_id: artifact,
                execution_profile: profile.clone(),
                layer: Some(1),
                tensor: TensorKey::Wv,
                row_start: 1,
                row_end: 3,
                worker_id: "machine-2".into(),
            },
            input: input.clone(),
        };
        let frame = encode_row_request(&request).unwrap();
        let mut stream = (frame.len() as u32).to_le_bytes().to_vec();
        stream.extend_from_slice(&frame);
        let mut answer: Vec<u8> = Vec::new();
        serve_row_frames(&model, artifact, &mut stream.as_slice(), &mut answer).unwrap();
        let length = u32::from_le_bytes(answer[..4].try_into().unwrap()) as usize;
        assert_eq!(answer.len(), 4 + length, "one response for one request");
        let response = decode_row_response(&answer[4..]).unwrap();
        assert_eq!(response.assignment, request.assignment);
        let weights = model_projection_weights(&model, Some(1), TensorKey::Wv).unwrap();
        let shard = weights.copy_rows(1, 3).unwrap();
        let mut expected = vec![0; 2];
        matmul_i8_canonical_rows(&shard, &input, &mut expected).unwrap();
        assert_eq!(response.values, expected);

        let mut other = request.clone();
        other.assignment.artifact_id = Hash256([1; 32]);
        let frame = encode_row_request(&other).unwrap();
        let mut stream = (frame.len() as u32).to_le_bytes().to_vec();
        stream.extend_from_slice(&frame);
        let mut sink: Vec<u8> = Vec::new();
        assert!(serve_row_frames(&model, artifact, &mut stream.as_slice(), &mut sink).is_err());
        let mut empty: &[u8] = &[];
        assert!(serve_row_frames(&model, artifact, &mut empty, &mut sink).is_ok());
    }

    #[test]
    fn borrowed_canonical_row_ranges_match_copied_shards_with_both_dispatch_settings() {
        let _guard = crate::canonical_simd::kernel_switch_guard();
        struct RestoreFastKernel(bool);
        impl Drop for RestoreFastKernel {
            fn drop(&mut self) {
                crate::canonical_simd::set_fast_canonical_kernel(self.0);
            }
        }
        let restore = RestoreFastKernel(crate::canonical_simd::fast_canonical_kernel_enabled());
        let weights = I8Weights::quantize_f32(
            &[
                1.0, -2.0, 0.5, 4.0, -3.0, 2.0, 8.0, 1.0, 0.25, -0.5, 2.0, -4.0,
            ],
            4,
            3,
        );
        let input = [65_536, -32_768, 16_384];

        for enabled in [false, true] {
            crate::canonical_simd::set_fast_canonical_kernel(enabled);
            for (start, end) in [(0, 1), (1, 3), (3, 4), (0, 4)] {
                let copied = weights.copy_rows(start, end).unwrap();
                let mut expected = vec![0; end - start];
                matmul_i8_canonical_rows(&copied, &input, &mut expected).unwrap();
                let mut borrowed = vec![i64::MIN; end - start];
                matmul_i8_canonical_row_range(&weights, start, end, &input, &mut borrowed).unwrap();
                assert_eq!(borrowed, expected, "range [{start}, {end}), fast={enabled}");
            }
        }

        let mut untouched = vec![123; 5];
        assert!(
            matmul_i8_canonical_row_range(&weights, 1, 1, &input, &mut untouched[..0]).is_err()
        );
        assert_eq!(untouched, vec![123; 5]);
        assert!(
            matmul_i8_canonical_row_range(&weights, 3, 2, &input, &mut untouched[..0]).is_err()
        );
        assert_eq!(untouched, vec![123; 5]);
        assert!(matmul_i8_canonical_row_range(&weights, 0, 5, &input, &mut untouched).is_err());
        assert_eq!(untouched, vec![123; 5]);
        assert!(
            matmul_i8_canonical_row_range(&weights, 1, 3, &input[..2], &mut untouched[..2])
                .is_err()
        );
        assert_eq!(untouched, vec![123; 5]);
        let mut bad_geometry = I8Weights::quantize_f32(&[1.0], 1, 1);
        bad_geometry.n_rows = usize::MAX;
        assert!(
            matmul_i8_canonical_row_range(&bad_geometry, 0, 1, &[1], &mut untouched[..1]).is_err()
        );
        assert_eq!(untouched, vec![123; 5]);

        let mut overflow = I8Weights::quantize_f32(&[1.0, 2.0], 2, 1);
        overflow.scales[1] = i64::MAX;
        let mut one_row = [0];
        matmul_i8_canonical_row_range(&overflow, 0, 1, &[1], &mut one_row).unwrap();
        overflow.scales[0] = i64::MAX;
        one_row[0] = 456;
        assert!(matmul_i8_canonical_row_range(&overflow, 0, 1, &[1], &mut one_row).is_err());
        assert_eq!(one_row, [456]);
        drop(restore);
    }

    #[test]
    fn tensor_row_backend_matches_canonical_whole_token_across_positions() {
        fn add_rows(
            assignments: &mut Vec<RowAssignment>,
            workers: &mut BTreeMap<String, Arc<dyn RowWorker>>,
            artifact: Hash256,
            profile: &str,
            layer: Option<usize>,
            tensor: TensorKey,
            weights: &I8Weights,
        ) {
            let split = weights.n_rows / 2;
            for (start, end) in [(0, split), (split, weights.n_rows)] {
                let worker_id = format!("{layer:?}-{tensor:?}-{start}");
                let assignment = RowAssignment {
                    artifact_id: artifact,
                    execution_profile: profile.into(),
                    layer,
                    tensor,
                    row_start: start,
                    row_end: end,
                    worker_id: worker_id.clone(),
                };
                let shard = weights.copy_rows(start, end).unwrap();
                workers.insert(
                    worker_id.clone(),
                    Arc::new(LocalRowWorker {
                        worker_id,
                        assignment: assignment.clone(),
                        weights: shard,
                    }),
                );
                assignments.push(assignment);
            }
        }
        let model = build_test_model(16, 32, 2, 64, 3);
        assert!(model.has_canonical_i8_profile());
        let artifact = Hash256([77; 32]);
        let profile = model.effective_precision_label().to_string();
        let mut assignments = Vec::new();
        let mut workers: BTreeMap<String, Arc<dyn RowWorker>> = BTreeMap::new();
        for (i, layer) in model.layers.iter().enumerate() {
            add_rows(
                &mut assignments,
                &mut workers,
                artifact,
                &profile,
                Some(i),
                TensorKey::Wq,
                &layer.wq,
            );
            add_rows(
                &mut assignments,
                &mut workers,
                artifact,
                &profile,
                Some(i),
                TensorKey::Wk,
                &layer.wk,
            );
            add_rows(
                &mut assignments,
                &mut workers,
                artifact,
                &profile,
                Some(i),
                TensorKey::Wv,
                &layer.wv,
            );
            add_rows(
                &mut assignments,
                &mut workers,
                artifact,
                &profile,
                Some(i),
                TensorKey::Wo,
                &layer.wo,
            );
            add_rows(
                &mut assignments,
                &mut workers,
                artifact,
                &profile,
                Some(i),
                TensorKey::WGate,
                &layer.w_gate,
            );
            add_rows(
                &mut assignments,
                &mut workers,
                artifact,
                &profile,
                Some(i),
                TensorKey::WUp,
                &layer.w_up,
            );
            add_rows(
                &mut assignments,
                &mut workers,
                artifact,
                &profile,
                Some(i),
                TensorKey::WDown,
                &layer.w_down,
            );
        }
        add_rows(
            &mut assignments,
            &mut workers,
            artifact,
            &profile,
            None,
            TensorKey::LmHead,
            &model.output_weight,
        );
        let backend = PartitionedProjectionBackend::new(artifact, profile, assignments, workers);
        let mut local_cache = KVCache::new(model.config.n_layers);
        let mut rows_cache = KVCache::new(model.config.n_layers);
        for token in [1u32, 2u32] {
            let local = model.forward_one_token(token, &mut local_cache);
            let rows = model
                .forward_one_token_canonical_i8_with_backend(
                    token,
                    &mut rows_cache,
                    Hash256([token as u8; 32]),
                    &backend,
                )
                .unwrap();
            assert_eq!(rows, local, "row backend diverged at token {token}");
        }
    }

    #[test]
    fn test_i8_matmul_per_row() {
        let weights = I8Weights::quantize_f32(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 2, 3);
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

    #[cfg(feature = "experimental-ip")]
    #[test]
    fn test_forward_one_token_with_ternary() {
        let mut model = build_test_model(100, 64, 2, 128, 2);

        model.enable_ternary();
        assert!(
            model.ternary_layers.is_some(),
            "ternary layers must be populated after enable_ternary"
        );

        let token: u32 = 5;
        let mut cache = super::KVCache::new(model.config.n_layers);
        let logits1 = model.forward_one_token(token, &mut cache);
        assert_eq!(
            logits1.len(),
            model.config.vocab_size,
            "logits must have vocab_size entries"
        );
        assert!(
            logits1.iter().any(|&v| v != 0),
            "ternary forward must produce non-zero logits"
        );

        let mut cache2 = super::KVCache::new(model.config.n_layers);
        let logits2 = model.forward_one_token(token, &mut cache2);
        assert_eq!(
            logits1, logits2,
            "ternary forward_one_token must be deterministic"
        );
    }

    #[cfg(feature = "experimental-ip")]
    #[test]
    fn test_ternary_memory_reduction_full_model() {
        let mut model = build_test_model(100, 64, 2, 128, 2);
        let before_bytes: usize = model
            .layers
            .iter()
            .map(|l| {
                l.wq.memory_bytes()
                    + l.wk.memory_bytes()
                    + l.wv.memory_bytes()
                    + l.wo.memory_bytes()
                    + l.w_gate.memory_bytes()
                    + l.w_up.memory_bytes()
                    + l.w_down.memory_bytes()
            })
            .sum();

        model.enable_ternary();
        let after_bytes: usize = model
            .ternary_layers
            .as_ref()
            .unwrap()
            .iter()
            .map(|l| {
                l.wq.memory_bytes()
                    + l.wk.memory_bytes()
                    + l.wv.memory_bytes()
                    + l.wo.memory_bytes()
                    + l.w_gate.memory_bytes()
                    + l.w_up.memory_bytes()
                    + l.w_down.memory_bytes()
            })
            .sum();

        assert!(
            after_bytes * 3 < before_bytes,
            "ternary ({} B) should be at least 3x smaller than I8 ({} B)",
            after_bytes,
            before_bytes,
        );
    }

    #[test]
    fn test_forward_shard_token_full_equals_split() {
        let model = build_test_model(20, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;
        let token: u32 = 5;

        let mut cache_a = KVCache::new(n_layers);
        let result_a = model
            .forward_shard_token(ShardInput::Token(token), &mut cache_a, 0, n_layers, 0)
            .expect("whole-model shard at position 0 is always in sync");
        let token_a = match result_a {
            ShardOutput::Token { id, .. } => id,
            _ => panic!("Whole-model shard should produce a token, not a hidden state"),
        };

        let k = 2;
        let mut cache_b = KVCache::new(n_layers);
        let mid = model
            .forward_shard_token(ShardInput::Token(token), &mut cache_b, 0, k, 0)
            .expect("first shard at position 0 is always in sync");
        let hidden = match mid {
            ShardOutput::Hidden(h) => h,
            _ => panic!("First shard should produce a hidden state, not a token"),
        };
        let result_b = model
            .forward_shard_token(ShardInput::Hidden(hidden), &mut cache_b, k, n_layers, 0)
            .expect("last shard at position 0 is always in sync");
        let token_b = match result_b {
            ShardOutput::Token { id, .. } => id,
            _ => panic!("Last shard should produce a token"),
        };

        assert_eq!(
            token_a, token_b,
            "Shard split at K={} produced different output token (A={}, B={})",
            k, token_a, token_b
        );
    }

    #[test]
    fn test_forward_shard_token_three_way_split() {
        let model = build_test_model(20, 32, 2, 64, 6);
        let n_layers = model.config.n_layers;
        let token: u32 = 7;

        let mut cache_a = KVCache::new(n_layers);
        let result_a = model
            .forward_shard_token(ShardInput::Token(token), &mut cache_a, 0, n_layers, 0)
            .unwrap();
        let token_a = match result_a {
            ShardOutput::Token { id, .. } => id,
            _ => panic!("expected Token"),
        };

        let mut cache_b = KVCache::new(n_layers);
        let h1 = match model
            .forward_shard_token(ShardInput::Token(token), &mut cache_b, 0, 2, 0)
            .unwrap()
        {
            ShardOutput::Hidden(h) => h,
            _ => panic!("expected Hidden"),
        };
        let h2 = match model
            .forward_shard_token(ShardInput::Hidden(h1), &mut cache_b, 2, 4, 0)
            .unwrap()
        {
            ShardOutput::Hidden(h) => h,
            _ => panic!("expected Hidden"),
        };
        let token_b = match model
            .forward_shard_token(ShardInput::Hidden(h2), &mut cache_b, 4, 6, 0)
            .unwrap()
        {
            ShardOutput::Token { id, .. } => id,
            _ => panic!("expected Token"),
        };

        assert_eq!(
            token_a, token_b,
            "3-way shard split produced different token (A={}, B={})",
            token_a, token_b
        );
    }

    #[test]
    fn test_forward_shard_token_multi_position() {
        let model = build_test_model(15, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;
        let prompt: Vec<u32> = vec![1, 5, 9, 13];
        let k = 2;

        let mut cache_a = KVCache::new(n_layers);
        let mut tokens_a = Vec::new();
        for (pos, &tok) in prompt.iter().enumerate() {
            let res = model
                .forward_shard_token(ShardInput::Token(tok), &mut cache_a, 0, n_layers, pos)
                .unwrap();
            match res {
                ShardOutput::Token { id, .. } => tokens_a.push(id),
                _ => panic!("Expected Token at pos {}", pos),
            }
        }

        let mut cache_b = KVCache::new(n_layers);
        let mut tokens_b = Vec::new();
        for (pos, &tok) in prompt.iter().enumerate() {
            let mid = match model
                .forward_shard_token(ShardInput::Token(tok), &mut cache_b, 0, k, pos)
                .unwrap()
            {
                ShardOutput::Hidden(h) => h,
                _ => panic!("First shard at pos {} should produce Hidden", pos),
            };
            let res = model
                .forward_shard_token(ShardInput::Hidden(mid), &mut cache_b, k, n_layers, pos)
                .unwrap();
            match res {
                ShardOutput::Token { id, .. } => tokens_b.push(id),
                _ => panic!("Last shard at pos {} should produce Token", pos),
            }
        }

        assert_eq!(
            tokens_a, tokens_b,
            "Multi-position shard split diverged. A={:?} B={:?}",
            tokens_a, tokens_b
        );
    }

    #[test]
    fn cold_replica_mid_stream_is_rejected_not_undefined() {
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
        let model = build_test_model(15, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;
        let mut cache = KVCache::new(n_layers);

        model
            .forward_shard_token(ShardInput::Token(1), &mut cache, 0, n_layers, 0)
            .expect("position 0 on a fresh cache");
        model
            .forward_shard_token(ShardInput::Token(2), &mut cache, 0, n_layers, 1)
            .expect("position 1 follows position 0");

        let err = model
            .forward_shard_token(ShardInput::Token(2), &mut cache, 0, n_layers, 1)
            .expect_err("replayed position must be refused");
        assert_eq!(err.kind(), "kv_cache_out_of_sync");
    }

    #[test]
    fn warm_replica_in_sequence_is_accepted() {
        let model = build_test_model(15, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;
        let mut cache = KVCache::new(n_layers);
        for pos in 0..6 {
            model
                .forward_shard_token(
                    ShardInput::Token(pos as u32 + 1),
                    &mut cache,
                    0,
                    n_layers,
                    pos,
                )
                .unwrap_or_else(|e| panic!("in-sequence position {pos} rejected: {e}"));
        }
    }

    #[test]
    fn short_hidden_state_from_peer_is_rejected_not_panic() {
        let model = build_test_model(15, 32, 2, 64, 4);
        let n_layers = model.config.n_layers;
        let mut cache = KVCache::new(n_layers);
        let err = model
            .forward_shard_token(
                ShardInput::Hidden(vec![0i64; 3]),
                &mut cache,
                2,
                n_layers,
                0,
            )
            .expect_err("truncated hidden state must be refused");
        assert_eq!(err.kind(), "bad_hidden_dim");
    }

    #[test]
    fn position_past_rope_table_is_rejected_not_panic() {
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
            &(0..1024 * 512)
                .map(|i| (i as f32 % 200.0 - 100.0) / 100.0)
                .collect::<Vec<_>>(),
            1024,
            512,
        );
        let input: Vec<i64> = (0..512).map(|i| (i as i64 - 256) * ONE / 256).collect();

        let scalar = matmul_i8(&weights, &input, 512, 1024);
        let simd = matmul_i8xi8_simd(&weights, &input, 512, 1024);

        for i in 0..1024 {
            let diff = (scalar[i] - simd[i]).abs();
            let tolerance = scalar[i].abs().max(ONE) / 5;
            assert!(
                diff < tolerance,
                "Row {}: scalar={}, simd={}, diff={}",
                i,
                scalar[i],
                simd[i],
                diff
            );
        }
    }

    #[test]
    fn test_dot_i16_simd_matches_scalar() {
        let len = 4096usize;
        let row: Vec<i16> = (0..len)
            .map(|i| ((i as i32 * 31) % 65535 - 32768) as i16)
            .collect();
        let input: Vec<i64> = (0..len)
            .map(|i| ((i as i64 * 12345) % (1 << 28)) - (1 << 27))
            .collect();

        let scalar = unsafe { dot_i16_i64_scalar(row.as_ptr(), input.as_ptr(), len) };
        let dispatched = unsafe { dot_i16_i64(row.as_ptr(), input.as_ptr(), len) };

        assert_eq!(
            scalar, dispatched,
            "SIMD i16 dot product diverged from scalar: scalar={} simd={}",
            scalar, dispatched
        );
    }

    #[test]
    fn test_dot_i16_simd_tail() {
        let len = 4099usize;
        let row: Vec<i16> = (0..len)
            .map(|i| ((i as i32 * 17) % 65535 - 32768) as i16)
            .collect();
        let input: Vec<i64> = (0..len)
            .map(|i| ((i as i64 * 9876) % (1 << 28)) - (1 << 27))
            .collect();

        let scalar = unsafe { dot_i16_i64_scalar(row.as_ptr(), input.as_ptr(), len) };
        let dispatched = unsafe { dot_i16_i64(row.as_ptr(), input.as_ptr(), len) };

        assert_eq!(
            scalar, dispatched,
            "SIMD i16 dot tail diverged: len={} scalar={} simd={}",
            len, scalar, dispatched
        );
    }

    #[test]
    #[ignore]
    fn bench_dot_i16_simd_vs_scalar() {
        let len = 4096usize;
        let row: Vec<i16> = (0..len)
            .map(|i| ((i as i32 * 31) % 65535 - 32768) as i16)
            .collect();
        let input: Vec<i64> = (0..len)
            .map(|i| ((i as i64 * 12345) % (1 << 28)) - (1 << 27))
            .collect();
        let iters = 100_000usize;

        let t0 = std::time::Instant::now();
        let mut sum_s: i64 = 0;
        for _ in 0..iters {
            let acc = unsafe { dot_i16_i64_scalar(row.as_ptr(), input.as_ptr(), len) };
            sum_s = sum_s.wrapping_add(acc);
        }
        let scalar_ns = t0.elapsed().as_nanos() / iters as u128;

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

    #[test]
    fn test_i8_scale_roundtrip_small_abs_max() {
        for &abs_max in &[0.1_f32, 0.5, 0.9, 1.2, 2.3] {
            let row = vec![abs_max, -abs_max, 0.0, abs_max / 2.0];
            let w = I8Weights::quantize_f32(&row, 1, 4);
            let scale = w.scales[0] as f64;
            let peak = (w.data[0] as f64) * scale / ONE as f64;
            let rel_err = (peak - abs_max as f64).abs() / abs_max as f64;
            assert!(
                rel_err < 0.05,
                "abs_max={} peak={} rel_err={:.3} scale={}",
                abs_max,
                peak,
                rel_err,
                w.scales[0]
            );
        }
    }

    #[test]
    fn test_q4_scale_roundtrip() {
        let weights = I8Weights::quantize_f32(
            &(0..512 * 256)
                .map(|i| (i as f32 % 200.0 - 100.0) / 100.0)
                .collect::<Vec<_>>(),
            512,
            256,
        );
        let q4 = Q4WeightsX86::from_i8(&weights);
        assert_eq!(q4.n_rows, 512);
        assert_eq!(q4.n_cols, 256);
        assert_eq!(q4.scales.len(), 512);
        for (i, (&q4s, &i8s)) in q4.scales.iter().zip(weights.scales.iter()).enumerate() {
            let ratio = q4s as f64 / i8s.max(1) as f64;
            assert!(
                (0.5..=20.0).contains(&ratio),
                "Q4 scale ratio out of range at row {}: {}",
                i,
                ratio
            );
        }
    }
}

#[cfg(test)]
mod int16_tests {
    use super::*;

    #[test]
    fn test_i16_quantize_f32_reconstruction() {
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
                let i8_recon = (i8w.data[i * n + j] as f64) * (i8w.scales[i] as f64) / (ONE as f64);
                let i16_recon = (i16w.data[i * n + j] as f64) * (i16w.scales[i] as f64)
                    / 32767.0
                    / (ONE as f64);
                i8_err += (orig - i8_recon).abs();
                i16_err += (orig - i16_recon).abs();
            }
        }
        assert!(
            i16_err < i8_err,
            "I16 error {i16_err} should be less than I8 error {i8_err}"
        );
    }

    #[test]
    fn test_i16_matmul_correctness() {
        let rows = 4;
        let cols = 8;
        let weights_f32: Vec<f32> = (0..rows * cols)
            .map(|i| ((i * 13 + 5) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let input_f64: Vec<f64> = (0..cols)
            .map(|i| ((i * 7 + 1) % 50) as f64 / 25.0 - 1.0)
            .collect();
        let input_q16: Vec<i64> = input_f64.iter().map(|&x| (x * ONE as f64) as i64).collect();

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

        assert_eq!(i16w.n_rows, i8w.n_rows);
        assert_eq!(i16w.n_cols, i8w.n_cols);
        for i in 0..i8w.data.len() {
            assert_eq!(i16w.data[i], i8w.data[i] as i16);
        }
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
            let tolerance = (i8_out[i].abs() / 100).max(2);
            assert!(
                diff <= tolerance,
                "Row {i}: i8={}, i16={}, diff={} > tolerance={} - from_i8 should produce close results",
                i8_out[i],
                i16_out[i],
                diff,
                tolerance
            );
        }
    }

    #[test]
    fn test_i16_memory_bytes() {
        let weights_f32: Vec<f32> = vec![0.0; 16];
        let w = I16Weights::quantize_f32(&weights_f32, 4, 4);
        assert_eq!(w.memory_bytes(), 16 * 2 + 4 * 8 + 16);
    }

    #[test]
    fn test_enable_i16_on_model() {
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
                    arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
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
                ternary_hybrid_output: None,
                i16_output: None,
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
        assert_eq!(i16_layers[0].wq.n_rows, model.layers[0].wq.n_rows);
        assert_eq!(i16_layers[0].wq.n_cols, model.layers[0].wq.n_cols);
    }

    #[test]
    fn test_i16_matmul_nonzero_output() {
        let weights = I16Weights::quantize_f32(
            &(0..128 * 64)
                .map(|i| ((i % 200) as f32 - 100.0) / 100.0)
                .collect::<Vec<_>>(),
            128,
            64,
        );
        let input: Vec<i64> = (0..64).map(|i| (i as i64 - 32) * ONE / 32).collect();
        let mut output = vec![0i64; 128];
        matmul_i16_into(&weights, &input, 64, &mut output);
        let nonzero = output.iter().filter(|&&x| x != 0).count();
        assert!(
            nonzero > 100,
            "Expected mostly nonzero outputs, got {}/128",
            nonzero
        );
    }
}

pub(crate) fn preflight_generation_config(
    config: &ModelConfig,
    prompt_tokens: usize,
    max_tokens: u32,
) -> Result<GenerationPreflight, GenerationError> {
    let generated_positions =
        usize::try_from(max_tokens).map_err(|_| GenerationError::PositionCountOverflow {
            prompt_tokens,
            max_tokens,
        })?;
    let required_positions = GENERATION_INTERNAL_BOS_POSITIONS
        .checked_add(prompt_tokens)
        .and_then(|positions| positions.checked_add(generated_positions))
        .ok_or(GenerationError::PositionCountOverflow {
            prompt_tokens,
            max_tokens,
        })?;

    if required_positions > config.max_seq {
        return Err(GenerationError::ContextWindowExceeded {
            prompt_tokens,
            max_tokens,
            required_positions,
            max_seq: config.max_seq,
        });
    }

    Ok(GenerationPreflight {
        prompt_tokens,
        max_tokens,
        required_positions,
        max_seq: config.max_seq,
    })
}
pub(crate) fn forward_canonical_with_backend(
    source: &impl crate::low_residency::CanonicalForwardSource,
    token: u32,
    cache: &mut KVCache,
    call_id: Hash256,
    backend: &impl crate::tensor_parallel::ProjectionBackend,
) -> Result<Vec<i64>, crate::tensor_parallel::TensorParallelError> {
    use crate::tensor_parallel::{TensorKey, TensorParallelError};
    if source.canonical_execution_profile().is_none() {
        return Err(TensorParallelError::WrongIdentity);
    }
    let cfg = source.config();
    let d = cfg.d_model;
    let pos = cache.seq_len;
    if pos >= cfg.max_seq || token as usize >= cfg.vocab_size {
        return Err(TensorParallelError::WrongShape);
    }
    let mut hidden = source.embedding(token)?;
    if hidden.len() != d {
        return Err(TensorParallelError::WrongShape);
    }
    for layer_idx in 0..cfg.n_layers {
        let (attn_norm, ffn_norm) = source.norms(layer_idx)?;
        let normed = layernorm(&hidden, attn_norm);
        let mut qkv = backend.project_group(
            call_id,
            Some(layer_idx),
            &[
                (TensorKey::Wq, d),
                (TensorKey::Wk, cfg.d_kv),
                (TensorKey::Wv, cfg.d_kv),
            ],
            &normed,
        )?;
        if qkv.len() != 3 {
            return Err(TensorParallelError::WrongShape);
        }
        let mut q = qkv.remove(0);
        let mut k_buf = qkv.remove(0);
        let v_buf = qkv.remove(0);
        if q.len() != d || k_buf.len() != cfg.d_kv || v_buf.len() != cfg.d_kv {
            return Err(TensorParallelError::WrongShape);
        }
        for h in 0..cfg.n_heads {
            apply_rope(
                &mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                pos,
                cfg.d_head,
                &cfg.rope_cos,
                &cfg.rope_sin,
            );
        }
        for h in 0..cfg.n_kv_heads {
            apply_rope(
                &mut k_buf[h * cfg.d_head..(h + 1) * cfg.d_head],
                pos,
                cfg.d_head,
                &cfg.rope_cos,
                &cfg.rope_sin,
            );
        }
        cache.push_k(layer_idx, &k_buf);
        cache.push_v(layer_idx, &v_buf);
        let full_seq = pos + 1;
        let heads: Vec<Vec<i64>> = (0..cfg.n_heads)
            .into_par_iter()
            .map(|h| {
                let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                flash_attention_i64(
                    &q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    &cache.k_data[layer_idx],
                    &cache.v_data[layer_idx],
                    cfg.d_kv,
                    kv_h,
                    cfg.d_head,
                    full_seq,
                    cfg.attn_scale,
                )
            })
            .collect();
        let mut attn_out = vec![0; d];
        for (h, value) in heads.iter().enumerate() {
            attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(value);
        }
        let projected =
            backend.project_rows(call_id, Some(layer_idx), TensorKey::Wo, &attn_out, d)?;
        if projected.len() != d {
            return Err(TensorParallelError::WrongShape);
        }
        for (out, projection) in hidden.iter_mut().zip(projected) {
            *out += projection;
        }
        let normed_ff = layernorm(&hidden, ffn_norm);
        let mut gate_up = backend.project_group(
            call_id,
            Some(layer_idx),
            &[(TensorKey::WGate, cfg.d_ff), (TensorKey::WUp, cfg.d_ff)],
            &normed_ff,
        )?;
        if gate_up.len() != 2 {
            return Err(TensorParallelError::WrongShape);
        }
        let mut gate = gate_up.remove(0);
        let up = gate_up.remove(0);
        if gate.len() != cfg.d_ff || up.len() != cfg.d_ff {
            return Err(TensorParallelError::WrongShape);
        }
        for (g, u) in gate.iter_mut().zip(up) {
            *g = (silu_i64(*g) * u) >> FRAC_BITS;
        }
        let ff_out = backend.project_rows(call_id, Some(layer_idx), TensorKey::WDown, &gate, d)?;
        if ff_out.len() != d {
            return Err(TensorParallelError::WrongShape);
        }
        for (out, projection) in hidden.iter_mut().zip(ff_out) {
            *out += projection;
        }
    }
    cache.seq_len = pos + 1;
    let normed = layernorm(&hidden, source.final_norm());
    let logits = backend.project_rows(call_id, None, TensorKey::LmHead, &normed, cfg.vocab_size)?;
    if logits.len() != cfg.vocab_size {
        return Err(TensorParallelError::WrongShape);
    }
    Ok(logits)
}
pub(crate) fn generate_canonical_with_backend(
    source: &impl crate::low_residency::CanonicalForwardSource,
    request: Hash256,
    prompt: &[u32],
    max_tokens: u32,
    eos_tokens: &[u32],
    backend: &impl crate::tensor_parallel::ProjectionBackend,
) -> Result<(Vec<u32>, Hash256), BackendGenerationError> {
    let _admission = preflight_generation_config(source.config(), prompt.len(), max_tokens)?;
    let forward = |token: u32, cache: &mut KVCache| {
        let call_id = backend_call_id(&request, cache.seq_len);
        forward_canonical_with_backend(source, token, cache, call_id, backend)
    };
    let mut cache = KVCache::new(source.config().n_layers);
    let mut logits = forward(source.config().bos_token, &mut cache)?;
    for &token in prompt {
        logits = forward(token, &mut cache)?;
    }
    let mut generated = Vec::new();
    for _ in 0..max_tokens {
        let next = select_next_token_with_repetition_penalty(&mut logits, &generated);
        generated.push(next);
        if eos_tokens.contains(&next) {
            break;
        }
        logits = forward(next, &mut cache)?;
    }
    let output_bytes: Vec<u8> = generated.iter().flat_map(|t| t.to_le_bytes()).collect();
    Ok((generated, arc_crypto::hash_bytes(&output_bytes)))
}
