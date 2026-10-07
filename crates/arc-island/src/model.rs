//! Per-layer accounting of a model for placement: weight bytes, bytes read
//! per decoded position, and KV bytes per position.
//!
//! The unit of placement is a "layer unit": the embedding, each transformer
//! layer, and the LM head. Pipeline stages own contiguous runs of units.

use serde::{Deserialize, Serialize};

/// One placement unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerSpec {
    pub name: String,
    /// Bytes the stage must hold for this unit.
    pub weight_bytes: u64,
    /// Bytes read for every forward pass regardless of routing (attention,
    /// shared expert, router, dense MLP, LM head; one row for the embedding).
    pub fixed_active_bytes: u64,
    /// Routed experts in this unit (0 for dense units).
    pub routed_experts: u32,
    /// Routed experts selected per position.
    pub experts_per_token: u32,
    /// Bytes of one routed expert.
    pub expert_bytes: u64,
    /// KV-cache bytes per position held by the stage that runs this unit.
    pub kv_bytes_per_position: u64,
}

impl LayerSpec {
    /// Bytes read by one forward pass over `positions` positions (a batch, or
    /// the k+1 positions of a speculative verification pass). Routed experts
    /// are counted by the expected number of distinct experts under uniform
    /// routing, research-6 §2.6: `E·(1 − (1 − k/E)^n)`. Projection only.
    pub fn active_bytes(&self, positions: u32) -> f64 {
        let routed = if self.routed_experts == 0 {
            0.0
        } else {
            distinct_experts(self.routed_experts, self.experts_per_token, positions)
                * self.expert_bytes as f64
        };
        self.fixed_active_bytes as f64 + routed
    }
}

/// Expected distinct experts touched by `positions` positions, each picking
/// `top` of `experts` uniformly (research-6 §2.6; real routing is skewed,
/// which lowers this).
pub fn distinct_experts(experts: u32, top: u32, positions: u32) -> f64 {
    if experts == 0 {
        return 0.0;
    }
    let e = f64::from(experts);
    let miss = 1.0 - f64::from(top) / e;
    e * (1.0 - miss.powi(positions as i32))
}

/// The exact model and quantisation a golden reference is pinned to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelIdentity {
    /// Repository and revision of the source checkpoint.
    pub checkpoint: String,
    /// ARC integer profile identifier.
    pub quant_profile: String,
}

/// A model as placement sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub name: String,
    pub identity: ModelIdentity,
    pub layers: Vec<LayerSpec>,
    /// Bytes crossing a pipeline-stage boundary per position.
    pub boundary_bytes_per_position: u64,
    /// Sequential collectives per decoded token under tensor parallelism
    /// (two per transformer layer).
    pub collectives_per_token: u32,
    /// Whether the checkpoint ships a multi-token-prediction head. Without
    /// one, speculation needs a separate drafter.
    pub has_mtp_head: bool,
}

impl ModelSpec {
    /// Kimi K2.6 under the §13 profile: INT4 group-32 routed experts with
    /// BF16 scales, INT8 elsewhere, INT16 router rows, i32 MLA latent cache,
    /// Q16 residual (i64) at stage boundaries.
    ///
    /// Shapes are from the K2.6 `config.json` as recorded in
    /// `docs/protocol/kimi-k26-checkpoint.md` (PR #156): 61 layers (layer 0
    /// dense), hidden 7,168, 64 heads, query LoRA 1,536, KV latent 512, RoPE
    /// 64, head dims 128/128, 384 routed experts with 8 per token plus 1
    /// shared, expert width 2,048, dense width 18,432, vocabulary 163,840.
    /// The total is 582.6 GB, matching that document's 582 GB \[CALC\].
    pub fn kimi_k26_int4() -> Self {
        const HIDDEN: u64 = 7_168;
        const HEADS: u64 = 64;
        const Q_LORA: u64 = 1_536;
        const KV_LORA: u64 = 512;
        const ROPE: u64 = 64;
        const NOPE: u64 = 128;
        const V_HEAD: u64 = 128;
        const EXPERTS: u32 = 384;
        const TOP: u32 = 8;
        const EXPERT_FF: u64 = 2_048;
        const DENSE_FF: u64 = 18_432;
        const VOCAB: u64 = 163_840;
        const MOE_LAYERS: usize = 60;

        // MLA projections at INT8 (one byte per weight).
        let attention = HIDDEN * Q_LORA
            + Q_LORA * HEADS * (NOPE + ROPE)
            + HIDDEN * (KV_LORA + ROPE)
            + KV_LORA * HEADS * (NOPE + V_HEAD)
            + HEADS * V_HEAD * HIDDEN;
        let ffn_values = 3 * HIDDEN * EXPERT_FF;
        let shared_expert = ffn_values;
        let router = u64::from(EXPERTS) * HIDDEN * 2;
        // INT4 nibbles plus one BF16 scale per group of 32.
        let expert = ffn_values / 2 + ffn_values / 32 * 2;
        let dense_mlp = 3 * HIDDEN * DENSE_FF;
        let embedding = VOCAB * HIDDEN;
        // i32 latent (512) plus RoPE key (64) per position per layer.
        let kv = (KV_LORA + ROPE) * 4;

        let mut layers = Vec::with_capacity(MOE_LAYERS + 3);
        layers.push(LayerSpec {
            name: "embed".into(),
            weight_bytes: embedding,
            fixed_active_bytes: HIDDEN,
            routed_experts: 0,
            experts_per_token: 0,
            expert_bytes: 0,
            kv_bytes_per_position: 0,
        });
        layers.push(LayerSpec {
            name: "L0".into(),
            weight_bytes: attention + dense_mlp,
            fixed_active_bytes: attention + dense_mlp,
            routed_experts: 0,
            experts_per_token: 0,
            expert_bytes: 0,
            kv_bytes_per_position: kv,
        });
        let moe_fixed = attention + shared_expert + router;
        for i in 1..=MOE_LAYERS {
            layers.push(LayerSpec {
                name: format!("L{i}"),
                weight_bytes: moe_fixed + u64::from(EXPERTS) * expert,
                fixed_active_bytes: moe_fixed,
                routed_experts: EXPERTS,
                experts_per_token: TOP,
                expert_bytes: expert,
                kv_bytes_per_position: kv,
            });
        }
        layers.push(LayerSpec {
            name: "lm_head".into(),
            weight_bytes: embedding,
            fixed_active_bytes: embedding,
            routed_experts: 0,
            experts_per_token: 0,
            expert_bytes: 0,
            kv_bytes_per_position: 0,
        });
        Self {
            name: "kimi-k2.6-i4g32-experts".into(),
            identity: ModelIdentity {
                checkpoint: "moonshotai/Kimi-K2.6@7eb5002f6aadc958aed6a9177b7ed26bb94011bb".into(),
                quant_profile: "arc.hf-deepseek-v3.mla-moe.i4g32-experts.q16.v1".into(),
            },
            layers,
            boundary_bytes_per_position: HIDDEN * 8,
            collectives_per_token: 2 * 61,
            has_mtp_head: false,
        }
    }

    /// A model of `count` identical dense units (tests and toy runs).
    pub fn uniform(
        name: &str,
        count: usize,
        weight_bytes: u64,
        kv_bytes_per_position: u64,
    ) -> Self {
        let layers = (0..count)
            .map(|i| LayerSpec {
                name: format!("L{i}"),
                weight_bytes,
                fixed_active_bytes: weight_bytes,
                routed_experts: 0,
                experts_per_token: 0,
                expert_bytes: 0,
                kv_bytes_per_position,
            })
            .collect();
        Self {
            name: name.into(),
            identity: ModelIdentity {
                checkpoint: format!("test/toy-units-{count}"),
                quant_profile: "toy".into(),
            },
            layers,
            boundary_bytes_per_position: 16 * 1024,
            collectives_per_token: 2 * count as u32,
            has_mtp_head: false,
        }
    }

    /// Total weight bytes.
    pub fn weight_bytes(&self) -> u64 {
        self.layers.iter().map(|l| l.weight_bytes).sum()
    }

    /// KV bytes per position across the whole model.
    pub fn kv_bytes_per_position(&self) -> u64 {
        self.layers.iter().map(|l| l.kv_bytes_per_position).sum()
    }

    /// Largest single unit; a device smaller than this holds no stage.
    pub fn largest_unit_bytes(&self) -> u64 {
        self.layers
            .iter()
            .map(|l| l.weight_bytes)
            .max()
            .unwrap_or(0)
    }

    /// Bytes read per decoded token at batch 1.
    pub fn active_bytes_per_token(&self) -> f64 {
        self.layers.iter().map(|l| l.active_bytes(1)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_k26_totals_match_the_checkpoint_document() {
        let m = ModelSpec::kimi_k26_int4();
        assert_eq!(m.layers.len(), 63);
        // 582,647,218,176 B: within 0.15% of the 582 GB in kimi-k26-checkpoint.md §3.
        assert_eq!(m.weight_bytes(), 582_647_218_176);
        // 24.8 MB per routed expert, 9.51 GB of experts per MoE layer (§3).
        assert_eq!(m.layers[2].expert_bytes, 24_772_608);
        assert_eq!(m.layers[2].expert_bytes * 384, 9_512_681_472);
        // 140,544 B of KV per position (§3) and a 56 KiB boundary.
        assert_eq!(m.kv_bytes_per_position(), 140_544);
        assert_eq!(m.boundary_bytes_per_position, 57_344);
        // About 22.6 GB read per token at batch 1 (research-6 §2.1: 22.3 GB).
        let active = m.active_bytes_per_token() / 1e9;
        assert!((22.0..23.0).contains(&active), "{active}");
    }

    #[test]
    fn distinct_experts_follow_research_6() {
        // research-6 §2.6: 8 / 60 / 188 distinct experts at batch 1 / 8 / 32.
        assert!((distinct_experts(384, 8, 1) - 8.0).abs() < 1e-9);
        assert_eq!(distinct_experts(384, 8, 8).round() as u32, 60);
        assert_eq!(distinct_experts(384, 8, 32).round() as u32, 188);
    }
}
