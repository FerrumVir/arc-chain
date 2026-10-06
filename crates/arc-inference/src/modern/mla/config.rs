//! The model shape of the MLA + MoE profile (spec §2) and its refusals.

use serde_json::{Value, json};

use crate::modern::ModernError;
use crate::modern::convert::eps_q32;
use crate::modern::tables::attention_lambda;

/// Model shape and profile constants: the `model` object of spec §2.2.
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
            && self.max_seq <= 1 << 20;
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
        };
        config.validate()?;
        Ok(config)
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

/// Parse and check a Hugging Face `config.json` (spec §2.1).
pub fn parse_hf_config(bytes: &[u8], max_seq: usize) -> Result<HfMlaConfig, ModernError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| ModernError::Invalid(format!("config.json: {e}")))?;
    let bad = |what: &str| ModernError::Invalid(format!("config.json: {what}"));
    let size = |key: &str| -> Result<usize, ModernError> {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| bad(&format!("{key} must be a non-negative integer")))
    };
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    let architecture = text("model_type").ok_or_else(|| bad("model_type"))?;
    if !matches!(architecture, "deepseek_v3" | "kimi_k2") {
        return Err(bad(&format!("model_type {architecture} is not supported")));
    }
    if text("hidden_act") != Some("silu") {
        return Err(bad("hidden_act must be silu"));
    }
    if text("scoring_func") != Some("sigmoid") {
        return Err(bad("scoring_func must be sigmoid"));
    }
    if text("topk_method") != Some("noaux_tc") {
        return Err(bad("topk_method must be noaux_tc"));
    }
    let checks = [
        ("attention_bias", Value::Bool(false)),
        ("moe_layer_freq", Value::from(1)),
        ("num_nextn_predict_layers", Value::from(0)),
        ("rope_scaling", Value::Null),
        ("tie_word_embeddings", Value::Bool(false)),
        ("rope_interleave", Value::Bool(true)),
    ];
    for (key, allowed) in &checks {
        if !absent_or(&value, key, allowed) {
            return Err(bad(&format!("{key} must be absent or {allowed}")));
        }
    }
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
