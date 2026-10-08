//! The model shape of the MLA + MoE profile (spec §2) and its refusals.

use serde_json::{Value, json};

use crate::modern::ModernError;
use crate::modern::convert::eps_q32;
use crate::modern::tables::attention_lambda;

/// How the routed experts are stored (spec §4.1 and the §13 variant).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExpertFormat {
    /// INT8 rows with dyadic scales: `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1`.
    #[default]
    Int8Dyadic,
    /// INT4 values with BF16 scales per 32 inputs (spec §13):
    /// `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.i4g32-experts.q16.v1`.
    Int4G32,
}

impl ExpertFormat {
    /// The arithmetic profile identity of a package in this format.
    pub fn profile(self) -> &'static str {
        match self {
            ExpertFormat::Int8Dyadic => super::PROFILE,
            ExpertFormat::Int4G32 => super::PROFILE_I4G32,
        }
    }

    /// The format a profile identity names, if any.
    pub fn from_profile(profile: &str) -> Option<Self> {
        match profile {
            p if p == super::PROFILE => Some(ExpertFormat::Int8Dyadic),
            p if p == super::PROFILE_I4G32 => Some(ExpertFormat::Int4G32),
            _ => None,
        }
    }

    /// Parse `i8` or `i4g32` (CLI option).
    pub fn parse(text: &str) -> Result<Self, ModernError> {
        match text {
            "i8" => Ok(ExpertFormat::Int8Dyadic),
            "i4g32" => Ok(ExpertFormat::Int4G32),
            other => Err(ModernError::Invalid(format!(
                "unknown expert format {other} (i8 or i4g32)"
            ))),
        }
    }
}

/// Model shape and profile constants: the `model` object of spec §2.2. The
/// expert format is not part of the object; the package's profile names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MlaConfig {
    /// `model_type` of the source (`deepseek_v3` or `kimi_k2`).
    pub architecture: String,
    pub n_layers: usize,
    pub d_model: usize,
    pub n_heads: usize,
    /// Query LoRA rank; 0 when the model projects queries directly.
    pub q_lora_rank: usize,
    /// Rank of the compressed KV latent (C).
    pub kv_lora_rank: usize,
    /// Per-head query/key width without RoPE (N).
    pub qk_nope_dim: usize,
    /// Per-head (and shared-key) RoPE width (R).
    pub qk_rope_dim: usize,
    /// Per-head value width (Vh).
    pub v_head_dim: usize,
    /// Dense FFN width (F).
    pub d_ff: usize,
    /// Layers `0 .. first_k_dense` are dense; the rest are MoE.
    pub first_k_dense: usize,
    pub n_routed_experts: usize,
    pub n_experts_per_tok: usize,
    pub n_shared_experts: usize,
    /// Expert FFN width (Fm).
    pub moe_d_ff: usize,
    pub n_group: usize,
    pub topk_group: usize,
    pub norm_topk_prob: bool,
    /// `rha(routed_scaling_factor * 2^32)`.
    pub routed_scaling_q32: i64,
    pub vocab_size: usize,
    pub max_seq: usize,
    /// `rha(rms_norm_eps * 2^32)`.
    pub rms_eps_q32: i64,
    pub rope_theta: u64,
    /// `floor(2^30 / sqrt(N + R))`.
    pub attention_lambda: i64,
    /// Storage of the routed experts (named by the profile, not the object).
    pub expert_format: ExpertFormat,
}

const MODEL_KEYS: [&str; 25] = [
    "architecture",
    "attention_lambda",
    "d_ff",
    "d_model",
    "first_k_dense",
    "kv_lora_rank",
    "max_seq",
    "moe_d_ff",
    "n_experts_per_tok",
    "n_group",
    "n_heads",
    "n_layers",
    "n_routed_experts",
    "n_shared_experts",
    "norm_topk_prob",
    "q_lora_rank",
    "qk_nope_dim",
    "qk_rope_dim",
    "rms_eps_q32",
    "rope_theta",
    "routed_scaling_q32",
    "tied_embeddings",
    "topk_group",
    "v_head_dim",
    "vocab_size",
];

impl MlaConfig {
    /// Whether layer `layer` is an MoE layer (spec §2).
    pub fn is_moe(&self, layer: usize) -> bool {
        layer >= self.first_k_dense
    }

    /// Per-head query width `N + R`.
    pub fn d_qk(&self) -> usize {
        self.qk_nope_dim + self.qk_rope_dim
    }

    /// Width of the concatenated queries `H (N + R)`.
    pub fn d_q(&self) -> usize {
        self.n_heads * self.d_qk()
    }

    /// Width of the `kv_a` projection `C + R`.
    pub fn d_kv_a(&self) -> usize {
        self.kv_lora_rank + self.qk_rope_dim
    }

    /// Width of the concatenated head outputs `H Vh`.
    pub fn d_attn_out(&self) -> usize {
        self.n_heads * self.v_head_dim
    }

    /// Width of the shared-expert FFN `Es Fm`.
    pub fn shared_d_ff(&self) -> usize {
        self.n_shared_experts * self.moe_d_ff
    }

    /// Cached i32 values per position and layer (`C + R`).
    pub fn kv_values_per_position(&self) -> usize {
        self.kv_lora_rank + self.qk_rope_dim
    }

    /// KV cache bytes per position for the whole model (i32 entries).
    pub fn kv_bytes_per_position(&self) -> usize {
        self.n_layers * self.kv_values_per_position() * 4
    }

    /// Reject shapes and constants the profile does not define (spec §2.1).
    pub fn validate(&self) -> Result<(), ModernError> {
        let positive = [
            self.n_layers,
            self.d_model,
            self.n_heads,
            self.kv_lora_rank,
            self.qk_nope_dim,
            self.qk_rope_dim,
            self.v_head_dim,
            self.d_ff,
            self.n_routed_experts,
            self.n_experts_per_tok,
            self.n_shared_experts,
            self.moe_d_ff,
            self.n_group,
            self.topk_group,
            self.vocab_size,
            self.max_seq,
        ];
        let group_size = self.n_routed_experts.checked_div(self.n_group).unwrap_or(0);
        let ok = !self.architecture.is_empty()
            && positive.iter().all(|&v| v > 0)
            && self.qk_rope_dim.is_multiple_of(2)
            && self.first_k_dense <= self.n_layers
            && self.n_experts_per_tok <= self.n_routed_experts
            && self.n_routed_experts.is_multiple_of(self.n_group.max(1))
            && self.topk_group <= self.n_group
            && (self.n_group == 1 || group_size >= 2)
            && self.n_experts_per_tok <= self.topk_group * group_size
            && (1..(1i64 << 40)).contains(&self.routed_scaling_q32)
            && self.rms_eps_q32 >= 1
            && (2..=(1u64 << 53)).contains(&self.rope_theta)
            && self.attention_lambda == attention_lambda(self.d_qk())
            && self.n_layers <= 1024
            && self.d_model <= 1 << 20
            && self.n_heads <= 4096
            && self.q_lora_rank <= 1 << 20
            && self.kv_lora_rank <= 1 << 16
            && self.qk_nope_dim <= 1 << 12
            && self.qk_rope_dim <= 1 << 12
            && self.v_head_dim <= 1 << 12
            && self.d_ff <= 1 << 22
            && self.moe_d_ff <= 1 << 22
            && self.n_routed_experts <= 1 << 16
            && self.n_shared_experts <= 1 << 8
            && self.vocab_size <= u32::MAX as usize
            && self.max_seq <= 1 << 20
            && (self.expert_format == ExpertFormat::Int8Dyadic
                || (self.d_model.is_multiple_of(32) && self.moe_d_ff.is_multiple_of(32)));
        if !ok {
            return Err(ModernError::Invalid(format!(
                "unsupported MLA + MoE model shape: {self:?}"
            )));
        }
        Ok(())
    }

    /// The `model` object of the header and manifest (spec §2.2).
    pub fn to_json(&self) -> Value {
        json!({
            "architecture": self.architecture,
            "n_layers": self.n_layers,
            "d_model": self.d_model,
            "n_heads": self.n_heads,
            "q_lora_rank": self.q_lora_rank,
            "kv_lora_rank": self.kv_lora_rank,
            "qk_nope_dim": self.qk_nope_dim,
            "qk_rope_dim": self.qk_rope_dim,
            "v_head_dim": self.v_head_dim,
            "d_ff": self.d_ff,
            "first_k_dense": self.first_k_dense,
            "n_routed_experts": self.n_routed_experts,
            "n_experts_per_tok": self.n_experts_per_tok,
            "n_shared_experts": self.n_shared_experts,
            "moe_d_ff": self.moe_d_ff,
            "n_group": self.n_group,
            "topk_group": self.topk_group,
            "norm_topk_prob": self.norm_topk_prob,
            "routed_scaling_q32": self.routed_scaling_q32,
            "vocab_size": self.vocab_size,
            "max_seq": self.max_seq,
            "rms_eps_q32": self.rms_eps_q32,
            "rope_theta": self.rope_theta,
            "attention_lambda": self.attention_lambda,
            "tied_embeddings": false,
        })
    }

    /// Parse the `model` object back; every field must be present and no
    /// other field may appear.
    pub fn from_json(model: &Value) -> Result<Self, ModernError> {
        let bad = |what: &str| ModernError::Invalid(format!("package model.{what}"));
        let object = model.as_object().ok_or_else(|| bad("(not an object)"))?;
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        if keys != MODEL_KEYS {
            return Err(bad(&format!("fields {keys:?}")));
        }
        let size = |key: &str| -> Result<usize, ModernError> {
            model
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(|| bad(key))
        };
        let int = |key: &str| -> Result<i64, ModernError> {
            model
                .get(key)
                .and_then(Value::as_i64)
                .ok_or_else(|| bad(key))
        };
        if model.get("tied_embeddings").and_then(Value::as_bool) != Some(false) {
            return Err(bad("tied_embeddings"));
        }
        let config = Self {
            architecture: model
                .get("architecture")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("architecture"))?
                .to_string(),
            n_layers: size("n_layers")?,
            d_model: size("d_model")?,
            n_heads: size("n_heads")?,
            q_lora_rank: size("q_lora_rank")?,
            kv_lora_rank: size("kv_lora_rank")?,
            qk_nope_dim: size("qk_nope_dim")?,
            qk_rope_dim: size("qk_rope_dim")?,
            v_head_dim: size("v_head_dim")?,
            d_ff: size("d_ff")?,
            first_k_dense: size("first_k_dense")?,
            n_routed_experts: size("n_routed_experts")?,
            n_experts_per_tok: size("n_experts_per_tok")?,
            n_shared_experts: size("n_shared_experts")?,
            moe_d_ff: size("moe_d_ff")?,
            n_group: size("n_group")?,
            topk_group: size("topk_group")?,
            norm_topk_prob: model
                .get("norm_topk_prob")
                .and_then(Value::as_bool)
                .ok_or_else(|| bad("norm_topk_prob"))?,
            routed_scaling_q32: int("routed_scaling_q32")?,
            vocab_size: size("vocab_size")?,
            max_seq: size("max_seq")?,
            rms_eps_q32: int("rms_eps_q32")?,
            rope_theta: model
                .get("rope_theta")
                .and_then(Value::as_u64)
                .ok_or_else(|| bad("rope_theta"))?,
            attention_lambda: int("attention_lambda")?,
            expert_format: ExpertFormat::Int8Dyadic,
        };
        config.validate()?;
        Ok(config)
    }

    /// The arithmetic profile identity of a package of this model.
    pub fn profile(&self) -> &'static str {
        self.expert_format.profile()
    }
}

/// A parsed `config.json`: the model shape and the EOS ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfMlaConfig {
    pub config: MlaConfig,
    pub eos: Vec<u32>,
}

fn absent_or(value: &Value, key: &str, allowed: &Value) -> bool {
    match value.get(key) {
        None | Some(Value::Null) => true,
        Some(v) => v == allowed,
    }
}

/// The architecture families the engine recognises in a Hugging Face config
/// (spec §2.1, §11). Only [`Architecture::DeepseekV3Mla`] has an integer
/// profile; the others are recognised so that a refusal names everything that
/// is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    /// DeepSeek-V3 and Kimi K2.x: MLA in every layer, sigmoid-routed MoE with
    /// shared experts.
    DeepseekV3Mla,
    /// Kimi Linear and Kimi K3: Kimi Delta Attention mixed with gated NoPE
    /// MLA, latent MoE and attention residuals.
    KimiLinear,
}

impl Architecture {
    /// The family of a text model's `model_type`.
    pub fn from_model_type(model_type: &str) -> Option<Self> {
        match model_type {
            "deepseek_v3" | "kimi_k2" => Some(Self::DeepseekV3Mla),
            "kimi_linear" => Some(Self::KimiLinear),
            _ => None,
        }
    }
}

/// The text model's configuration: the file itself, or its `text_config`
/// object when the checkpoint is multimodal (Kimi K2.5, K2.6 and K3 wrap the
/// language model this way).
fn text_config(value: &Value) -> (&Value, bool) {
    match value.get("text_config") {
        Some(text @ Value::Object(_)) => (text, true),
        _ => (value, false),
    }
}

/// Every feature of a Hugging Face `config.json` that the integer engine does
/// not implement; empty when the configuration is supported (spec §2.1, §11).
pub fn unsupported_features(value: &Value) -> Vec<String> {
    let (text, wrapped) = text_config(value);
    let str_of = |key: &str| text.get(key).and_then(Value::as_str);
    let mut missing = Vec::new();
    if wrapped {
        missing.push(
            "a multimodal checkpoint: tensors under the language_model. prefix and a vision tower"
                .to_string(),
        );
    }
    if let Some(q) = text
        .get("quantization_config")
        .or_else(|| value.get("quantization_config"))
        .filter(|q| !q.is_null())
    {
        let method = q.get("quant_method").and_then(Value::as_str).unwrap_or("?");
        let format = q.get("format").and_then(Value::as_str).unwrap_or("?");
        missing.push(format!(
            "pre-quantized weights ({method}, {format}); the converter reads BF16"
        ));
    }
    if let Some(scaling) = text.get("rope_scaling").filter(|r| !r.is_null()) {
        let kind = scaling
            .get("type")
            .or_else(|| scaling.get("rope_type"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        missing.push(format!("rope_scaling {kind}"));
    }
    let model_type = str_of("model_type").unwrap_or("(none)");
    match Architecture::from_model_type(model_type) {
        None => missing.push(format!("model_type {model_type}")),
        Some(Architecture::DeepseekV3Mla) => {
            for (key, want) in [
                ("hidden_act", "silu"),
                ("scoring_func", "sigmoid"),
                ("topk_method", "noaux_tc"),
            ] {
                if str_of(key) != Some(want) {
                    missing.push(format!("{key} {}", str_of(key).unwrap_or("(none)")));
                }
            }
            let checks = [
                ("attention_bias", Value::Bool(false)),
                ("moe_layer_freq", Value::from(1)),
                ("num_nextn_predict_layers", Value::from(0)),
                ("tie_word_embeddings", Value::Bool(false)),
                ("rope_interleave", Value::Bool(true)),
            ];
            for (key, allowed) in &checks {
                if !absent_or(text, key, allowed) {
                    missing.push(format!("{key} other than {allowed}"));
                }
            }
        }
        Some(Architecture::KimiLinear) => {
            let linear = text.get("linear_attn_config");
            let count = |key: &str| {
                linear
                    .and_then(|l| l.get(key))
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            };
            let layers = text
                .get("num_hidden_layers")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            missing.push(format!(
                "Kimi Delta Attention in {} of {layers} layers (gated delta rule, short convolutions, recurrent state)",
                count("kda_layers")
            ));
            let flag = |key: &str| text.get(key).and_then(Value::as_bool) == Some(true);
            if flag("mla_use_nope") || flag("mla_use_output_gate") {
                missing.push(format!(
                    "MLA without RoPE and with a sigmoid output gate in {} layers",
                    count("full_attn_layers")
                ));
            }
            if let Some(act) = str_of("hidden_act").filter(|&a| a != "silu") {
                missing.push(format!("the {act} activation"));
            }
            if let Some(width) = text
                .get("routed_expert_hidden_size")
                .and_then(Value::as_u64)
            {
                missing.push(format!(
                    "latent MoE: routed experts on a {width}-wide projection of the hidden state"
                ));
            }
            if let Some(block) = text.get("attn_res_block_size").and_then(Value::as_u64) {
                missing.push(format!(
                    "attention residuals over blocks of {block} layers (the residual stream carries a block stack)"
                ));
            }
        }
    }
    missing
}

/// Parse and check a Hugging Face `config.json` (spec §2.1).
pub fn parse_hf_config(bytes: &[u8], max_seq: usize) -> Result<HfMlaConfig, ModernError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| ModernError::Invalid(format!("config.json: {e}")))?;
    let bad = |what: &str| ModernError::Invalid(format!("config.json: {what}"));
    let missing = unsupported_features(&value);
    if !missing.is_empty() {
        return Err(bad(&format!("not supported: {}", missing.join("; "))));
    }
    let size = |key: &str| -> Result<usize, ModernError> {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| bad(&format!("{key} must be a non-negative integer")))
    };
    let architecture = value
        .get("model_type")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("model_type"))?;
    let n_heads = size("num_attention_heads")?;
    if let Some(kv) = value.get("num_key_value_heads")
        && kv.as_u64() != Some(n_heads as u64)
    {
        return Err(bad("num_key_value_heads must equal num_attention_heads"));
    }
    let q_lora_rank = match value.get("q_lora_rank") {
        None | Some(Value::Null) => 0,
        Some(_) => {
            let rank = size("q_lora_rank")?;
            if rank == 0 {
                return Err(bad("q_lora_rank must be null or positive"));
            }
            rank
        }
    };
    let theta = value
        .get("rope_theta")
        .and_then(Value::as_f64)
        .ok_or_else(|| bad("rope_theta"))?;
    if !(2.0..=9_007_199_254_740_992.0).contains(&theta) || theta.fract() != 0.0 {
        return Err(bad("rope_theta must be an integer in [2, 2^53]"));
    }
    let eps = value
        .get("rms_norm_eps")
        .and_then(Value::as_f64)
        .ok_or_else(|| bad("rms_norm_eps"))?;
    let scale = value
        .get("routed_scaling_factor")
        .and_then(Value::as_f64)
        .ok_or_else(|| bad("routed_scaling_factor"))?;
    // Multiplying a double by 2^32 is exact; `round` rounds half away from zero.
    let scaled = (scale * 4_294_967_296.0).round();
    if !(1.0..1_099_511_627_776.0).contains(&scaled) {
        return Err(bad("routed_scaling_factor is outside [2^-32, 256)"));
    }
    let norm_topk_prob = value
        .get("norm_topk_prob")
        .and_then(Value::as_bool)
        .ok_or_else(|| bad("norm_topk_prob must be a boolean"))?;
    if let Some(limit) = value.get("max_position_embeddings").and_then(Value::as_u64)
        && max_seq as u64 > limit
    {
        return Err(bad("max_seq exceeds max_position_embeddings"));
    }
    let eos = match value.get("eos_token_id") {
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|v| v.as_u64().and_then(|id| u32::try_from(id).ok()))
            .collect::<Option<Vec<u32>>>()
            .ok_or_else(|| bad("eos_token_id"))?,
        Some(v) => vec![
            v.as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .ok_or_else(|| bad("eos_token_id"))?,
        ],
        None => return Err(bad("eos_token_id")),
    };
    let qk_nope_dim = size("qk_nope_head_dim")?;
    let qk_rope_dim = size("qk_rope_head_dim")?;
    let config = MlaConfig {
        architecture: architecture.to_string(),
        n_layers: size("num_hidden_layers")?,
        d_model: size("hidden_size")?,
        n_heads,
        q_lora_rank,
        kv_lora_rank: size("kv_lora_rank")?,
        qk_nope_dim,
        qk_rope_dim,
        v_head_dim: size("v_head_dim")?,
        d_ff: size("intermediate_size")?,
        first_k_dense: size("first_k_dense_replace")?,
        n_routed_experts: size("n_routed_experts")?,
        n_experts_per_tok: size("num_experts_per_tok")?,
        n_shared_experts: size("n_shared_experts")?,
        moe_d_ff: size("moe_intermediate_size")?,
        n_group: size("n_group")?,
        topk_group: size("topk_group")?,
        norm_topk_prob,
        routed_scaling_q32: scaled as i64,
        vocab_size: size("vocab_size")?,
        max_seq,
        rms_eps_q32: eps_q32(eps)?,
        rope_theta: theta as u64,
        attention_lambda: attention_lambda(qk_nope_dim + qk_rope_dim),
        expert_format: ExpertFormat::Int8Dyadic,
    };
    config.validate()?;
    Ok(HfMlaConfig { config, eos })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Moonlight-16B-A3B-Instruct's `config.json`, as published at revision
    /// 4e735b07a89f73647dfab71ab91b840f362ede5b (key order and values kept).
    pub(crate) const MOONLIGHT_CONFIG: &str = r#"{
      "architectures": ["DeepseekV3ForCausalLM"], "attention_bias": false,
      "attention_dropout": 0.0,
      "auto_map": {"AutoConfig": "configuration_deepseek.DeepseekV3Config",
        "AutoModel": "modeling_deepseek.DeepseekV3Model",
        "AutoModelForCausalLM": "modeling_deepseek.DeepseekV3ForCausalLM"},
      "aux_loss_alpha": 0.001, "bos_token_id": 163584, "eos_token_id": 163586,
      "ep_size": 1, "first_k_dense_replace": 1, "hidden_act": "silu",
      "hidden_size": 2048, "initializer_range": 0.02, "intermediate_size": 11264,
      "kv_lora_rank": 512, "max_position_embeddings": 8192, "model_type": "deepseek_v3",
      "moe_intermediate_size": 1408, "moe_layer_freq": 1, "n_group": 1,
      "n_routed_experts": 64, "n_shared_experts": 2, "norm_topk_prob": true,
      "num_attention_heads": 16, "num_experts_per_tok": 6, "num_hidden_layers": 27,
      "num_key_value_heads": 16, "num_nextn_predict_layers": 0, "pretraining_tp": 1,
      "q_lora_rank": null, "qk_nope_head_dim": 128, "qk_rope_head_dim": 64,
      "rms_norm_eps": 1e-05, "rope_theta": 50000.0, "routed_scaling_factor": 2.446,
      "scoring_func": "sigmoid", "seq_aux": true, "tie_word_embeddings": false,
      "topk_group": 1, "topk_method": "noaux_tc", "torch_dtype": "bfloat16",
      "transformers_version": "4.46.3", "use_cache": true, "v_head_dim": 128,
      "vocab_size": 163840}"#;

    #[test]
    fn moonlight_config_is_parsed_exactly() {
        let hf = parse_hf_config(MOONLIGHT_CONFIG.as_bytes(), 4096).unwrap();
        assert_eq!(hf.eos, vec![163_586]);
        let c = &hf.config;
        assert_eq!(
            (
                c.n_layers,
                c.d_model,
                c.n_heads,
                c.q_lora_rank,
                c.kv_lora_rank
            ),
            (27, 2048, 16, 0, 512)
        );
        assert_eq!((c.qk_nope_dim, c.qk_rope_dim, c.v_head_dim), (128, 64, 128));
        assert_eq!((c.d_ff, c.first_k_dense, c.moe_d_ff), (11_264, 1, 1408));
        assert_eq!(
            (c.n_routed_experts, c.n_experts_per_tok, c.n_shared_experts),
            (64, 6, 2)
        );
        assert_eq!((c.n_group, c.topk_group, c.norm_topk_prob), (1, 1, true));
        assert_eq!(c.routed_scaling_q32, 10_505_490_006);
        assert_eq!(c.rms_eps_q32, 42_950);
        assert_eq!(c.rope_theta, 50_000);
        assert_eq!(c.attention_lambda, 77_490_641);
        assert_eq!(c.kv_bytes_per_position(), 62_208);
        assert_eq!(MlaConfig::from_json(&c.to_json()).unwrap(), *c);
        assert!(!c.is_moe(0) && c.is_moe(1) && c.is_moe(26));
    }

    #[test]
    fn unsupported_configurations_are_refused() {
        for (from, to) in [
            ("\"rope_theta\": 50000.0", "\"rope_theta\": 50000.5"),
            (
                "\"scoring_func\": \"sigmoid\"",
                "\"scoring_func\": \"softmax\"",
            ),
            (
                "\"topk_method\": \"noaux_tc\"",
                "\"topk_method\": \"greedy\"",
            ),
            (
                "\"tie_word_embeddings\": false",
                "\"tie_word_embeddings\": true",
            ),
            (
                "\"num_nextn_predict_layers\": 0",
                "\"num_nextn_predict_layers\": 1",
            ),
            ("\"num_key_value_heads\": 16", "\"num_key_value_heads\": 8"),
            ("\"n_group\": 1", "\"n_group\": 3"),
            (
                "\"model_type\": \"deepseek_v3\"",
                "\"model_type\": \"llama\"",
            ),
            (
                "\"q_lora_rank\": null",
                "\"q_lora_rank\": null, \"rope_scaling\": {\"type\": \"yarn\"}",
            ),
        ] {
            let changed = MOONLIGHT_CONFIG.replace(from, to);
            assert_ne!(changed, MOONLIGHT_CONFIG, "{from}");
            assert!(
                parse_hf_config(changed.as_bytes(), 4096).is_err(),
                "{to} was accepted"
            );
        }
        // A context beyond max_position_embeddings is refused.
        assert!(parse_hf_config(MOONLIGHT_CONFIG.as_bytes(), 8193).is_err());
        // Query LoRA (Kimi K2's 1536) is accepted.
        let lora = MOONLIGHT_CONFIG.replace("\"q_lora_rank\": null", "\"q_lora_rank\": 1536");
        assert_eq!(
            parse_hf_config(lora.as_bytes(), 4096)
                .unwrap()
                .config
                .q_lora_rank,
            1536
        );
    }

    /// Keys of Kimi-K2.6's `config.json` (revision
    /// 7eb5002f6aadc958aed6a9177b7ed26bb94011bb) that decide support; the
    /// quantisation block is cut to its method and format.
    const KIMI_K26_KEYS: &str = r#"{"architectures": ["KimiK25ForConditionalGeneration"], "model_type": "kimi_k25", "text_config": {"model_type": "kimi_k2", "hidden_act": "silu", "scoring_func": "sigmoid", "topk_method": "noaux_tc", "attention_bias": false, "moe_layer_freq": 1, "num_nextn_predict_layers": 0, "tie_word_embeddings": false, "rope_scaling": {"beta_fast": 32.0, "beta_slow": 1.0, "factor": 64.0, "mscale": 1.0, "mscale_all_dim": 1.0, "original_max_position_embeddings": 4096, "type": "yarn"}, "num_hidden_layers": 61, "quantization_config": {"format": "pack-quantized", "quant_method": "compressed-tensors"}}}"#;

    /// Keys of Kimi-K3's `config.json` (revision
    /// f831ab66814297da540d832a5235f8e904f29d06), cut the same way.
    const KIMI_K3_KEYS: &str = r#"{"architectures": ["KimiK3ForConditionalGeneration"], "model_type": "kimi_k3", "text_config": {"model_type": "kimi_linear", "hidden_act": "situ", "num_hidden_layers": 93, "linear_attn_config": {"full_attn_layers": [4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 64, 68, 72, 76, 80, 84, 88, 92, 93], "gate_lower_bound": -5.0, "head_dim": 128, "kda_layers": [1, 2, 3, 5, 6, 7, 9, 10, 11, 13, 14, 15, 17, 18, 19, 21, 22, 23, 25, 26, 27, 29, 30, 31, 33, 34, 35, 37, 38, 39, 41, 42, 43, 45, 46, 47, 49, 50, 51, 53, 54, 55, 57, 58, 59, 61, 62, 63, 65, 66, 67, 69, 70, 71, 73, 74, 75, 77, 78, 79, 81, 82, 83, 85, 86, 87, 89, 90, 91], "num_heads": 96, "short_conv_kernel_size": 4, "use_full_rank_gate": true}, "mla_use_nope": true, "mla_use_output_gate": true, "routed_expert_hidden_size": 3584, "attn_res_block_size": 12, "quantization_config": {"format": "mxfp4-pack-quantized", "quant_method": "compressed-tensors"}}}"#;

    #[test]
    fn kimi_k26_and_k3_refusals_list_every_missing_feature() {
        let k26: Value = serde_json::from_str(KIMI_K26_KEYS).unwrap();
        assert_eq!(
            unsupported_features(&k26),
            [
                "a multimodal checkpoint: tensors under the language_model. prefix and a vision tower",
                "pre-quantized weights (compressed-tensors, pack-quantized); the converter reads BF16",
                "rope_scaling yarn",
            ]
        );
        let k3: Value = serde_json::from_str(KIMI_K3_KEYS).unwrap();
        assert_eq!(
            unsupported_features(&k3),
            [
                "a multimodal checkpoint: tensors under the language_model. prefix and a vision tower",
                "pre-quantized weights (compressed-tensors, mxfp4-pack-quantized); the converter reads BF16",
                "Kimi Delta Attention in 69 of 93 layers (gated delta rule, short convolutions, recurrent state)",
                "MLA without RoPE and with a sigmoid output gate in 24 layers",
                "the situ activation",
                "latent MoE: routed experts on a 3584-wide projection of the hidden state",
                "attention residuals over blocks of 12 layers (the residual stream carries a block stack)",
            ]
        );
        let err = parse_hf_config(KIMI_K3_KEYS.as_bytes(), 4096).unwrap_err();
        assert!(err.to_string().contains("Kimi Delta Attention"), "{err}");
        assert_eq!(
            Architecture::from_model_type("kimi_k2"),
            Some(Architecture::DeepseekV3Mla)
        );
        assert_eq!(
            Architecture::from_model_type("kimi_linear"),
            Some(Architecture::KimiLinear)
        );
        // Moonlight itself has nothing missing.
        let moonlight: Value = serde_json::from_str(MOONLIGHT_CONFIG).unwrap();
        assert!(unsupported_features(&moonlight).is_empty());
    }

    #[test]
    fn model_objects_with_extra_or_missing_fields_are_refused() {
        let hf = parse_hf_config(MOONLIGHT_CONFIG.as_bytes(), 4096).unwrap();
        let mut extra = hf.config.to_json();
        extra["unexpected"] = Value::from(1);
        assert!(MlaConfig::from_json(&extra).is_err());
        let mut missing = hf.config.to_json();
        missing.as_object_mut().unwrap().remove("n_group");
        assert!(MlaConfig::from_json(&missing).is_err());
        let mut lambda = hf.config.to_json();
        lambda["attention_lambda"] = Value::from(77_490_640);
        assert!(MlaConfig::from_json(&lambda).is_err());
    }
}
