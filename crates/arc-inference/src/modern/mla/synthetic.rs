//! Synthetic MLA + MoE stage packages: random but valid weights of any
//! DeepSeek-V3/Kimi-K2-shaped configuration, for tests and benchmarks that
//! must not depend on downloaded checkpoints.
//!
//! Every tensor's content depends only on its name (and the router mode), so
//! packages of different layer ranges of one configuration hold the same
//! values: a stage `[a, b)` cut from one layout is the same stage in any other
//! layout. The tiny configurations are the ones the profile's golden digests
//! pin; their bytes must never change.

use std::path::Path;

use super::config::{ExpertFormat, MlaConfig};
use super::package::{self, StageSpec, StageWriter, header_json, layout};
use crate::modern::ModernError;
use crate::modern::arith::ONE;

/// Deterministic generator for synthetic packages.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

/// How a synthetic package's routers are filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Router {
    /// Random router rows and biases.
    Random,
    /// Expert `2j + 1` gets expert `2j`'s router row, shift and bias, so
    /// every token's keys tie in pairs and the top-k cut falls in a tie.
    Paired,
    /// Zero router rows and biases: every key ties for every token.
    Flat,
}

impl Router {
    pub fn parse(name: &str) -> Result<Self, ModernError> {
        match name {
            "random" => Ok(Router::Random),
            "paired" => Ok(Router::Paired),
            "flat" => Ok(Router::Flat),
            other => Err(ModernError::Invalid(format!("unknown router mode {other}"))),
        }
    }
}

/// The tiny configuration the profile's golden digests pin: query LoRA and
/// group routing when `lora`, a dense first layer and shared experts.
pub fn tiny_config(lora: bool, format: ExpertFormat) -> MlaConfig {
    MlaConfig {
        architecture: "deepseek_v3".into(),
        n_layers: 4,
        d_model: 32,
        n_heads: 4,
        q_lora_rank: if lora { 24 } else { 0 },
        kv_lora_rank: 16,
        qk_nope_dim: 8,
        qk_rope_dim: 4,
        v_head_dim: 8,
        d_ff: 48,
        first_k_dense: 1,
        n_routed_experts: 8,
        n_experts_per_tok: 3,
        n_shared_experts: 2,
        moe_d_ff: 32,
        n_group: if lora { 4 } else { 1 },
        topk_group: if lora { 2 } else { 1 },
        norm_topk_prob: true,
        routed_scaling_q32: 10_505_490_006,
        vocab_size: 50,
        max_seq: 24,
        rms_eps_q32: 42_950,
        rope_theta: 50_000,
        attention_lambda: crate::modern::tables::attention_lambda(12),
        expert_format: format,
    }
}

/// Named synthetic shapes.
///
/// * `tiny`, `tiny-lora`: the golden-digest configurations (4 layers, d 32).
/// * `small`: 8 layers, d 256, 16 experts top-4, query LoRA, group routing.
///   Big enough that a stage does measurable work, small enough for CI.
/// * `kimi-mini`: Kimi K2's proportions scaled down: 8 layers (1 dense),
///   d 512, 64 heads' ratio kept at 8 heads, 64 experts top-8 in 8 groups of
///   which 4 are used, 1 shared expert, INT4 group-32 experts.
pub fn shape(name: &str) -> Result<MlaConfig, ModernError> {
    let base = |lora: bool| tiny_config(lora, ExpertFormat::Int8Dyadic);
    let c = match name {
        "tiny" => base(false),
        // Forty independently placeable stages; synthetic arithmetic only.
        "regional-40" => MlaConfig {
            n_layers: 40,
            max_seq: 64,
            ..base(true)
        },
        "tiny-lora" => base(true),
        "tiny-i4" => tiny_config(true, ExpertFormat::Int4G32),
        "small" => MlaConfig {
            n_layers: 8,
            d_model: 256,
            n_heads: 8,
            q_lora_rank: 128,
            kv_lora_rank: 64,
            qk_nope_dim: 32,
            qk_rope_dim: 16,
            v_head_dim: 32,
            d_ff: 512,
            first_k_dense: 1,
            n_routed_experts: 16,
            n_experts_per_tok: 4,
            n_shared_experts: 1,
            moe_d_ff: 128,
            n_group: 4,
            topk_group: 2,
            vocab_size: 512,
            max_seq: 256,
            attention_lambda: crate::modern::tables::attention_lambda(48),
            ..base(true)
        },
        "kimi-mini" => MlaConfig {
            n_layers: 8,
            d_model: 512,
            n_heads: 8,
            q_lora_rank: 192,
            kv_lora_rank: 64,
            qk_nope_dim: 64,
            qk_rope_dim: 32,
            v_head_dim: 64,
            d_ff: 1024,
            first_k_dense: 1,
            n_routed_experts: 64,
            n_experts_per_tok: 8,
            n_shared_experts: 1,
            moe_d_ff: 128,
            n_group: 8,
            topk_group: 4,
            vocab_size: 1024,
            max_seq: 256,
            attention_lambda: crate::modern::tables::attention_lambda(96),
            expert_format: ExpertFormat::Int4G32,
            ..base(true)
        },
        other => {
            return Err(ModernError::Invalid(format!(
                "unknown synthetic shape {other} (tiny, tiny-lora, tiny-i4, small, kimi-mini)"
            )));
        }
    };
    c.validate()?;
    Ok(c)
}

/// Write a synthetic package for `stage` of `c` to `path`.
pub fn write_package(
    c: &MlaConfig,
    stage: StageSpec,
    router: Router,
    path: &Path,
) -> Result<(), ModernError> {
    let entries = layout(c, stage);
    let header = header_json(
        c,
        &serde_json::json!({"repo": "arc-test/tiny-mla", "revision": "0", "files": []}),
        stage,
        &entries,
    );
    let mut w = StageWriter::create(path, &header, entries.clone())?;
    let (cos, sin) = crate::modern::tables::rope_tables(c.rope_theta, c.qk_rope_dim, c.max_seq)?;
    for e in &entries {
        let seed = blake3::hash(e.name.as_bytes());
        let mut rng = Lcg(u64::from_le_bytes(
            seed.as_bytes()[..8].try_into().expect("8 bytes"),
        ));
        let count = e.shape.iter().product::<usize>();
        let bytes: Vec<u8> = if e.name == "rope.cos" {
            crate::modern::package::i32_bytes(&cos)
        } else if e.name == "rope.sin" {
            crate::modern::package::i32_bytes(&sin)
        } else if e.name.ends_with(".q4") {
            // Any nibble is a valid INT4 value.
            (0..count).map(|_| rng.next_u64() as u8).collect()
        } else if e.name.ends_with(".s") {
            // Positive BF16 group scales near 2^-11.
            let values: Vec<u16> = (0..count)
                .map(|_| (((114 + rng.next_u64() % 3) << 7) | (rng.next_u64() % 128)) as u16)
                .collect();
            package::u16_bytes(&values)
        } else if e.name.ends_with(".q") && e.dtype == package::Dtype::I8 {
            (0..count)
                .map(|_| ((rng.next_u64() % 255) as i64 - 127) as i8 as u8)
                .collect()
        } else if e.name.ends_with(".mu") {
            let values: Vec<i32> = (0..count)
                .map(|_| ((1u64 << 30) + rng.next_u64() % (1 << 30)) as i32)
                .collect();
            crate::modern::package::i32_bytes(&values)
        } else if e.name.ends_with(".k") && !e.name.contains("router") {
            // Scales mu * 2^-k around 2^-8: projections of +-127 weights
            // keep activations of order one.
            (0..count)
                .map(|_| 38 + (rng.next_u64() % 2) as u8)
                .collect()
        } else if e.name.ends_with("router.q") {
            let values: Vec<i16> = (0..count)
                .map(|_| ((rng.next_u64() % 65_535) as i64 - 32_767) as i16)
                .collect();
            package::i16_bytes(&values)
        } else if e.name.ends_with("router.k") {
            // Router weights q * 2^-k of order 0.1.
            (0..count)
                .map(|_| 17 + (rng.next_u64() % 3) as u8)
                .collect()
        } else if e.name.ends_with("router_bias") {
            let values: Vec<i64> = (0..count)
                .map(|_| (rng.next_u64() % (1 << 30)) as i64 - (1 << 29))
                .collect();
            crate::modern::package::i64_bytes(&values)
        } else {
            // Norm gains in [0.5, 1.5).
            let values: Vec<i64> = (0..count)
                .map(|_| ONE / 2 + (rng.next_u64() % ONE as u64) as i64)
                .collect();
            crate::modern::package::i64_bytes(&values)
        };
        let routing = ["router.q", "router.k", "router_bias"]
            .iter()
            .any(|suffix| e.name.ends_with(suffix));
        let bytes = match router {
            Router::Paired if routing => {
                let row = bytes.len() / c.n_routed_experts;
                let mut paired = bytes;
                for expert in (1..c.n_routed_experts).step_by(2) {
                    paired.copy_within((expert - 1) * row..expert * row, expert * row);
                }
                paired
            }
            Router::Flat if routing && !e.name.ends_with("router.k") => vec![0; bytes.len()],
            _ => bytes,
        };
        w.write_tensor(&e.name, &bytes)?;
    }
    w.finish()?;
    Ok(())
}

/// [`write_package`] into memory (through a uniquely named temporary file).
pub fn package_bytes(
    c: &MlaConfig,
    stage: StageSpec,
    router: Router,
) -> Result<Vec<u8>, ModernError> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "arc-mla-synthetic-{}-{unique}.arcspkg",
        std::process::id()
    ));
    let written = write_package(c, stage, router, &path);
    let bytes = written.and_then(|()| {
        std::fs::read(&path).map_err(|e| ModernError::io(&path.display().to_string(), e))
    });
    let _ = std::fs::remove_file(&path);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_shape_is_a_valid_configuration() {
        for name in ["tiny", "tiny-lora", "tiny-i4", "small", "kimi-mini"] {
            let c = shape(name).unwrap();
            assert_eq!(
                c.attention_lambda,
                crate::modern::tables::attention_lambda(c.d_qk()),
                "{name}"
            );
        }
        assert!(shape("kimi-k2").is_err());
    }
}
