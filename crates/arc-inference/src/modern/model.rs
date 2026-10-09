//! The dyadic-profile model: configuration, forward pass and generation.
//!
//! Grouped-query attention, per-layer NoPE, a tied INT8 embedding that is also
//! the LM head, and an i32 KV cache (spec §5.8, §6). Every value is computed by
//! [`super::arith`], so the output is a pure function of the package bytes, the
//! token ids and the generation rule.

use std::time::Instant;

use rayon::prelude::*;

use super::ModernError;
use super::arith::{
    self, DyadicMatrix, HeadCache, Selection, add_residual, attention_head, embed_row, gated_silu,
    project, rms_norm, rope_split_half,
};
use super::tables::attention_lambda;

/// Model shape and the profile constants read from the package header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModernConfig {
    pub architecture: String,
    pub n_layers: usize,
    pub d_model: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub d_head: usize,
    pub d_ff: usize,
    pub vocab_size: usize,
    pub max_seq: usize,
    /// `round_half_away(rms_norm_eps * 2^32)`.
    pub rms_eps_q32: i64,
    pub rope_theta: u64,
    /// `true` where the layer applies RoPE; `false` for NoPE layers.
    pub rope_layers: Vec<bool>,
}

impl ModernConfig {
    /// Width of the concatenated query heads.
    pub fn d_q(&self) -> usize {
        self.n_heads * self.d_head
    }

    /// Width of the concatenated KV heads.
    pub fn d_kv(&self) -> usize {
        self.n_kv_heads * self.d_head
    }

    /// KV cache bytes per position (i32 keys and values, every layer).
    pub fn kv_bytes_per_position(&self) -> usize {
        self.n_layers * 2 * self.d_kv() * 4
    }

    /// Reject shapes the profile does not define.
    pub fn validate(&self) -> Result<(), ModernError> {
        let positive = [
            self.n_layers,
            self.d_model,
            self.n_heads,
            self.n_kv_heads,
            self.d_head,
            self.d_ff,
            self.vocab_size,
            self.max_seq,
        ];
        let ok = positive.iter().all(|&v| v > 0)
            && self.n_heads.is_multiple_of(self.n_kv_heads)
            && self.d_head.is_multiple_of(2)
            && self.rope_layers.len() == self.n_layers
            && self.rms_eps_q32 >= 1
            && self.vocab_size <= u32::MAX as usize
            && self.max_seq <= 1 << 20
            && self.d_model <= 1 << 20
            && self.d_ff <= 1 << 22;
        if !ok {
            return Err(ModernError::Invalid(format!(
                "unsupported model shape: {self:?}"
            )));
        }
        Ok(())
    }
}

/// One transformer layer.
#[derive(Debug, Clone)]
pub struct ModernLayer {
    pub attn_norm: Vec<i64>,
    pub wq: DyadicMatrix,
    pub wk: DyadicMatrix,
    pub wv: DyadicMatrix,
    pub wo: DyadicMatrix,
    pub ffn_norm: Vec<i64>,
    pub w_gate: DyadicMatrix,
    pub w_up: DyadicMatrix,
    pub w_down: DyadicMatrix,
}

/// A loaded dyadic-profile model.
#[derive(Debug, Clone)]
pub struct ModernModel {
    pub config: ModernConfig,
    /// Tied embedding: token lookup and LM head.
    pub embed: DyadicMatrix,
    pub final_norm: Vec<i64>,
    /// `max_seq * d_head/2` Q16 values, row-major by position.
    pub rope_cos: Vec<i32>,
    pub rope_sin: Vec<i32>,
    pub layers: Vec<ModernLayer>,
}

impl ModernModel {
    /// Check every tensor's shape against the configuration.
    pub fn validate(&self) -> Result<(), ModernError> {
        let c = &self.config;
        c.validate()?;
        let shape = |m: &DyadicMatrix, rows: usize, cols: usize, name: &str| {
            if m.rows == rows && m.cols == cols {
                m.validate(name)
            } else {
                Err(ModernError::Invalid(format!(
                    "{name}: shape {}x{} where {rows}x{cols} is required",
                    m.rows, m.cols
                )))
            }
        };
        shape(&self.embed, c.vocab_size, c.d_model, "embed")?;
        let half = c.d_head / 2;
        if self.final_norm.len() != c.d_model
            || self.rope_cos.len() != c.max_seq * half
            || self.rope_sin.len() != c.max_seq * half
            || self.layers.len() != c.n_layers
        {
            return Err(ModernError::Invalid("model tensor sizes".into()));
        }
        for (l, layer) in self.layers.iter().enumerate() {
            let name = |m: &str| format!("layers.{l}.{m}");
            shape(&layer.wq, c.d_q(), c.d_model, &name("wq"))?;
            shape(&layer.wk, c.d_kv(), c.d_model, &name("wk"))?;
            shape(&layer.wv, c.d_kv(), c.d_model, &name("wv"))?;
            shape(&layer.wo, c.d_model, c.d_q(), &name("wo"))?;
            shape(&layer.w_gate, c.d_ff, c.d_model, &name("w_gate"))?;
            shape(&layer.w_up, c.d_ff, c.d_model, &name("w_up"))?;
            shape(&layer.w_down, c.d_model, c.d_ff, &name("w_down"))?;
            if layer.attn_norm.len() != c.d_model || layer.ffn_norm.len() != c.d_model {
                return Err(ModernError::Invalid(format!("layers.{l}: norm sizes")));
            }
        }
        Ok(())
    }

    /// INT8 weight count (every matrix including the tied embedding once).
    pub fn weight_count(&self) -> usize {
        let mut total = self.embed.q.len();
        for layer in &self.layers {
            for m in [
                &layer.wq,
                &layer.wk,
                &layer.wv,
                &layer.wo,
                &layer.w_gate,
                &layer.w_up,
                &layer.w_down,
            ] {
                total += m.q.len();
            }
        }
        total
    }

    /// A fresh KV cache for this model.
    pub fn new_cache(&self) -> KvCache {
        KvCache {
            keys: vec![Vec::new(); self.config.n_layers],
            values: vec![Vec::new(); self.config.n_layers],
            positions: 0,
        }
    }

    /// One token through the model at the cache's next position (spec §5.8).
    ///
    /// On error the cache may hold a partial position and must be discarded.
    pub fn forward(&self, token: u32, cache: &mut KvCache) -> Result<Vec<i64>, ModernError> {
        let c = &self.config;
        let position = cache.positions;
        if position >= c.max_seq {
            return Err(ModernError::Domain(format!(
                "position {position} is outside the {}-position context",
                c.max_seq
            )));
        }
        if cache.keys.len() != c.n_layers || cache.values.len() != c.n_layers {
            return Err(ModernError::Invalid(
                "KV cache belongs to another model".into(),
            ));
        }
        let mut hidden = embed_row(&self.embed, token as usize)?;
        let half = c.d_head / 2;
        let cos = &self.rope_cos[position * half..(position + 1) * half];
        let sin = &self.rope_sin[position * half..(position + 1) * half];
        let lambda = attention_lambda(c.d_head);
        let mut q = vec![0i64; c.d_q()];
        let mut k = vec![0i64; c.d_kv()];
        let mut v = vec![0i64; c.d_kv()];
        let mut attended = vec![0i64; c.d_q()];
        let mut projected = vec![0i64; c.d_model];
        let mut gate = vec![0i64; c.d_ff];
        let mut up = vec![0i64; c.d_ff];
        for (l, layer) in self.layers.iter().enumerate() {
            let normed = rms_norm(&hidden, &layer.attn_norm, c.rms_eps_q32)?;
            project(&layer.wq, &normed, &mut q)?;
            project(&layer.wk, &normed, &mut k)?;
            project(&layer.wv, &normed, &mut v)?;
            if c.rope_layers[l] {
                for head in q.chunks_exact_mut(c.d_head) {
                    rope_split_half(head, cos, sin)?;
                }
                for head in k.chunks_exact_mut(c.d_head) {
                    rope_split_half(head, cos, sin)?;
                }
            }
            cache.push(l, &k, &v)?;
            let keys: &[i32] = &cache.keys[l];
            let values: &[i32] = &cache.values[l];
            let group = c.n_heads / c.n_kv_heads;
            attended
                .par_chunks_mut(c.d_head)
                .zip(q.par_chunks(c.d_head))
                .enumerate()
                .try_for_each(|(head, (out, q_head))| {
                    let view = HeadCache {
                        keys,
                        values,
                        positions: position + 1,
                        stride: c.d_kv(),
                        offset: (head / group) * c.d_head,
                    };
                    attention_head(q_head, view, lambda, out)
                })?;
            project(&layer.wo, &attended, &mut projected)?;
            add_residual(&mut hidden, &projected)?;
            let normed = rms_norm(&hidden, &layer.ffn_norm, c.rms_eps_q32)?;
            project(&layer.w_gate, &normed, &mut gate)?;
            project(&layer.w_up, &normed, &mut up)?;
            for (g, &u) in gate.iter_mut().zip(&up) {
                *g = gated_silu(*g, u)?;
            }
            project(&layer.w_down, &gate, &mut projected)?;
            add_residual(&mut hidden, &projected)?;
        }
        cache.positions = position + 1;
        let normed = rms_norm(&hidden, &self.final_norm, c.rms_eps_q32)?;
        let mut logits = vec![0i64; c.vocab_size];
        project(&self.embed, &normed, &mut logits)?;
        Ok(logits)
    }

    /// Generation `arc.hf-chat.no-bos.*.le-u32.v1` (spec §6.2).
    pub fn generate(
        &self,
        request: &GenerationRequest<'_>,
    ) -> Result<GenerationOutput, ModernError> {
        let c = &self.config;
        if request.prompt.is_empty() || request.max_tokens == 0 {
            return Err(ModernError::Invalid(
                "generation needs a non-empty prompt and max_tokens >= 1".into(),
            ));
        }
        if request.prompt.len() + request.max_tokens > c.max_seq {
            return Err(ModernError::Domain(format!(
                "{} prompt tokens + {} generated tokens exceed the {}-position context",
                request.prompt.len(),
                request.max_tokens,
                c.max_seq
            )));
        }
        if let Some(&bad) = request.prompt.iter().find(|&&t| t as usize >= c.vocab_size) {
            return Err(ModernError::Domain(format!(
                "prompt token {bad} is outside the vocabulary"
            )));
        }
        let mut cache = self.new_cache();
        let mut hashes = Vec::with_capacity(request.prompt.len() + request.max_tokens);
        let prefill_start = Instant::now();
        let mut logits = Vec::new();
        for &token in request.prompt {
            logits = self.forward(token, &mut cache)?;
            hashes.push(arith::logits_hash(&logits));
        }
        let prefill_seconds = prefill_start.elapsed().as_secs_f64();
        let decode_start = Instant::now();
        let mut tokens: Vec<u32> = Vec::new();
        loop {
            let next = arith::select(&logits, &tokens, request.selection)?;
            tokens.push(next);
            if request.eos.contains(&next) || tokens.len() == request.max_tokens {
                break;
            }
            logits = self.forward(next, &mut cache)?;
            hashes.push(arith::logits_hash(&logits));
        }
        let decode_seconds = decode_start.elapsed().as_secs_f64();
        Ok(GenerationOutput {
            output_hash: arith::tokens_hash(&tokens),
            logits_digest: arith::logits_digest(&hashes),
            decode_forwards: tokens.len() - 1,
            tokens,
            logits_hashes: hashes,
            prefill_seconds,
            decode_seconds,
        })
    }
}

/// i32 KV cache: post-RoPE keys and raw values per layer.
#[derive(Debug, Clone)]
pub struct KvCache {
    keys: Vec<Vec<i32>>,
    values: Vec<Vec<i32>>,
    positions: usize,
}

impl KvCache {
    /// Number of cached positions.
    pub fn positions(&self) -> usize {
        self.positions
    }

    fn push(&mut self, layer: usize, k: &[i64], v: &[i64]) -> Result<(), ModernError> {
        let narrow = |x: &i64| {
            i32::try_from(*x)
                .map_err(|_| ModernError::Domain("KV value outside i32 (|v| >= 2^31)".into()))
        };
        let k32 = k.iter().map(narrow).collect::<Result<Vec<i32>, _>>()?;
        let v32 = v.iter().map(narrow).collect::<Result<Vec<i32>, _>>()?;
        self.keys[layer].extend_from_slice(&k32);
        self.values[layer].extend_from_slice(&v32);
        Ok(())
    }

    /// BLAKE3 over every cached key then value, layer by layer, as LE i32.
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for (keys, values) in self.keys.iter().zip(&self.values) {
            for x in keys.iter().chain(values) {
                hasher.update(&x.to_le_bytes());
            }
        }
        *hasher.finalize().as_bytes()
    }
}

/// A generation request in token ids.
#[derive(Debug, Clone)]
pub struct GenerationRequest<'a> {
    pub prompt: &'a [u32],
    pub max_tokens: usize,
    pub eos: &'a [u32],
    pub selection: Selection,
}

/// Tokens, digests and timings of one generation.
#[derive(Debug, Clone)]
pub struct GenerationOutput {
    pub tokens: Vec<u32>,
    pub output_hash: [u8; 32],
    /// One hash per forward call, in order.
    pub logits_hashes: Vec<[u8; 32]>,
    pub logits_digest: [u8; 32],
    pub prefill_seconds: f64,
    pub decode_seconds: f64,
    /// Forward calls made after the prompt.
    pub decode_forwards: usize,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Deterministic pseudo-random dyadic matrix with realistic scales.
    pub(crate) fn lcg_matrix(seed: &mut u64, rows: usize, cols: usize) -> DyadicMatrix {
        let mut next = || {
            *seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *seed >> 33
        };
        let q = (0..rows * cols)
            .map(|_| ((next() % 255) as i64 - 127) as i8)
            .collect();
        let mu = (0..rows)
            .map(|_| ((1u64 << 30) + next() % (1 << 30)) as i32)
            .collect();
        let k = (0..rows).map(|_| 40 + (next() % 3) as u8).collect();
        DyadicMatrix {
            rows,
            cols,
            q,
            mu,
            k,
        }
    }

    pub(crate) fn tiny_model() -> ModernModel {
        let config = ModernConfig {
            architecture: "smollm3".into(),
            n_layers: 2,
            d_model: 16,
            n_heads: 4,
            n_kv_heads: 2,
            d_head: 4,
            d_ff: 24,
            vocab_size: 40,
            max_seq: 32,
            rms_eps_q32: 4295,
            rope_theta: 5_000_000,
            rope_layers: vec![true, false],
        };
        let mut seed = 0x5eed_u64;
        let (cos, sin) = super::super::tables::rope_tables(config.rope_theta, 4, 32).unwrap();
        let mut layers = Vec::new();
        for _ in 0..config.n_layers {
            layers.push(ModernLayer {
                attn_norm: vec![arith::ONE; 16],
                wq: lcg_matrix(&mut seed, 16, 16),
                wk: lcg_matrix(&mut seed, 8, 16),
                wv: lcg_matrix(&mut seed, 8, 16),
                wo: lcg_matrix(&mut seed, 16, 16),
                ffn_norm: vec![arith::ONE + 1000; 16],
                w_gate: lcg_matrix(&mut seed, 24, 16),
                w_up: lcg_matrix(&mut seed, 24, 16),
                w_down: lcg_matrix(&mut seed, 16, 24),
            });
        }
        let model = ModernModel {
            embed: lcg_matrix(&mut seed, 40, 16),
            final_norm: vec![arith::ONE; 16],
            rope_cos: cos,
            rope_sin: sin,
            layers,
            config,
        };
        model.validate().unwrap();
        model
    }

    #[test]
    fn forward_is_deterministic_across_thread_counts_and_kernels() {
        let model = tiny_model();
        let run = |threads: usize| {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let mut cache = model.new_cache();
                let mut hashes = Vec::new();
                for token in [3u32, 17, 5, 39, 0] {
                    hashes.push(arith::logits_hash(
                        &model.forward(token, &mut cache).unwrap(),
                    ));
                }
                (hashes, cache.digest())
            })
        };
        // Scalar explicitly: the vectorised kernel is the x86-64 default, so
        // relying on the default would compare it with itself.
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let previous = crate::canonical_simd::fast_canonical_kernel_enabled();
        crate::canonical_simd::set_fast_canonical_kernel(false);
        let one = run(1);
        assert_eq!(one, run(3));
        crate::canonical_simd::set_fast_canonical_kernel(true);
        let simd = run(2);
        crate::canonical_simd::set_fast_canonical_kernel(previous);
        assert_eq!(one, simd);
    }

    #[test]
    fn generation_follows_the_no_bos_semantics() {
        let model = tiny_model();
        let prompt = [1u32, 2, 3];
        let request = GenerationRequest {
            prompt: &prompt,
            max_tokens: 6,
            eos: &[],
            selection: Selection::Rp64Argmax,
        };
        let out = model.generate(&request).unwrap();
        assert_eq!(out.tokens.len(), 6);
        // P + len(out) - 1 forward calls, the last token never forwarded.
        assert_eq!(out.logits_hashes.len(), 3 + 6 - 1);
        // Replaying prompt + tokens[..-1] reproduces every logits hash.
        let mut cache = model.new_cache();
        let mut replay = Vec::new();
        for &t in prompt.iter().chain(&out.tokens[..5]) {
            replay.push(arith::logits_hash(&model.forward(t, &mut cache).unwrap()));
        }
        assert_eq!(replay, out.logits_hashes);
        // Stopping on the first generated token as EOS forwards nothing more.
        let eos = [out.tokens[0]];
        let stopped = model
            .generate(&GenerationRequest {
                eos: &eos,
                ..request.clone()
            })
            .unwrap();
        assert_eq!(stopped.tokens, vec![out.tokens[0]]);
        assert_eq!(stopped.logits_hashes.len(), 3);
        assert_eq!(stopped.decode_forwards, 0);
    }

    #[test]
    fn generation_refuses_requests_outside_the_context() {
        let model = tiny_model();
        let long = vec![1u32; 30];
        let request = GenerationRequest {
            prompt: &long,
            max_tokens: 3,
            eos: &[],
            selection: Selection::Argmax,
        };
        assert!(matches!(
            model.generate(&request),
            Err(ModernError::Domain(_))
        ));
        let bad = [40u32];
        assert!(
            model
                .generate(&GenerationRequest {
                    prompt: &bad,
                    max_tokens: 1,
                    ..request
                })
                .is_err()
        );
    }

    #[test]
    fn nope_layers_make_attention_order_free() {
        let mut model = tiny_model();
        model.layers.truncate(1);
        model.config.n_layers = 1;
        let last_logits = |model: &ModernModel, tokens: &[u32]| {
            let mut cache = model.new_cache();
            let mut logits = Vec::new();
            for &t in tokens {
                logits = model.forward(t, &mut cache).unwrap();
            }
            logits
        };
        // One NoPE layer: keys and values depend only on their token and the
        // sums are exact, so the order of the context cannot change the
        // logits of the last position.
        model.config.rope_layers = vec![false];
        assert_eq!(
            last_logits(&model, &[3, 9, 7]),
            last_logits(&model, &[9, 3, 7])
        );
        // With RoPE the keys are rotated by position, so the order matters.
        model.config.rope_layers = vec![true];
        assert_ne!(
            last_logits(&model, &[3, 9, 7]),
            last_logits(&model, &[9, 3, 7])
        );
        // At position 0 the rotation is the identity.
        let first_rope = last_logits(&model, &[5]);
        model.config.rope_layers = vec![false];
        assert_eq!(first_rope, last_logits(&model, &[5]));
    }
}
