//! Cached Integer Model — Production-speed deterministic inference.
//!
//! Loads GGUF model weights into memory ONCE at startup. Forward pass
//! uses pre-loaded i64 weights with KV cache and rayon parallelism.
//! No disk I/O during inference. Pure i64 arithmetic.
//!
//! Target: ~76ms/token for 7B on Mac Studio M2 Ultra (24 cores).

use crate::integer_lut::*;
use arc_crypto::Hash256;
use rayon::prelude::*;
use tracing::info;

/// Pre-loaded transformer layer weights in i64 Q16 fixed-point.
pub struct CachedLayer {
    pub wq: Vec<i64>,      // [d_model × d_model]
    pub wk: Vec<i64>,      // [d_model × d_kv]
    pub wv: Vec<i64>,      // [d_model × d_kv]
    pub wo: Vec<i64>,      // [d_model × d_model]
    pub w_gate: Vec<i64>,  // [d_ff × d_model]
    pub w_up: Vec<i64>,    // [d_ff × d_model]
    pub w_down: Vec<i64>,  // [d_model × d_ff]
    pub attn_norm: Vec<i64>,
    pub ffn_norm: Vec<i64>,
}

/// KV cache for autoregressive generation.
pub struct KVCache {
    /// k_cache[layer][pos * d_kv .. (pos+1) * d_kv]
    pub k: Vec<Vec<i64>>,
    /// v_cache[layer][pos * d_kv .. (pos+1) * d_kv]
    pub v: Vec<Vec<i64>>,
    pub seq_len: usize,
}

impl KVCache {
    pub fn new(n_layers: usize) -> Self {
        Self {
            k: vec![Vec::new(); n_layers],
            v: vec![Vec::new(); n_layers],
            seq_len: 0,
        }
    }

    pub fn clear(&mut self) {
        for layer in &mut self.k { layer.clear(); }
        for layer in &mut self.v { layer.clear(); }
        self.seq_len = 0;
    }
}

/// Model config extracted from GGUF metadata.
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
    /// Pre-computed RoPE cos/sin tables in Q16.
    pub rope_cos: Vec<i64>, // [max_seq × d_head/2]
    pub rope_sin: Vec<i64>,
    pub max_seq: usize,
}

/// Fully cached integer model — all weights in RAM.
pub struct CachedIntegerModel {
    pub config: ModelConfig,
    pub embedding: Vec<i64>,     // [vocab × d_model]
    pub layers: Vec<CachedLayer>,
    pub final_norm: Vec<i64>,    // [d_model]
    pub output_weight: Vec<i64>, // [vocab × d_model]
}

/// Parallel i64 matmul — each output row computed independently.
fn matmul_par(weights: &[i64], input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    (0..out_size).into_par_iter().map(|i| {
        let row = &weights[i * in_size..(i + 1) * in_size];
        let mut acc: i64 = 0;
        for j in 0..in_size {
            acc += row[j] * input[j];
        }
        acc >> FRAC_BITS
    }).collect()
}

/// Sequential i64 matmul for small dimensions (rayon overhead not worth it).
fn matmul_seq(weights: &[i64], input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let mut output = Vec::with_capacity(out_size);
    for i in 0..out_size {
        let mut acc: i64 = 0;
        let row_start = i * in_size;
        for j in 0..in_size {
            acc += weights[row_start + j] * input[j];
        }
        output.push(acc >> FRAC_BITS);
    }
    output
}

/// Choose parallel or sequential based on size.
fn matmul(weights: &[i64], input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    if out_size >= 256 {
        matmul_par(weights, input, in_size, out_size)
    } else {
        matmul_seq(weights, input, in_size, out_size)
    }
}

/// Integer layer normalization.
fn layernorm(input: &[i64], gamma: &[i64]) -> Vec<i64> {
    let n = input.len() as i64;
    if n == 0 { return vec![]; }

    let mean = input.iter().sum::<i64>() / n;
    let mut var_sum: i64 = 0;
    for &x in input {
        let d = x - mean;
        var_sum += (d * d) >> FRAC_BITS;
    }
    let variance = var_sum / n;
    let inv_std = integer_isqrt(variance + 1);

    input.iter().enumerate().map(|(i, &x)| {
        let norm = ((x - mean) * inv_std) >> FRAC_BITS;
        let g = if i < gamma.len() { gamma[i] } else { ONE };
        (norm * g) >> FRAC_BITS
    }).collect()
}

/// Apply RoPE (rotary position embeddings) to Q or K vector.
fn apply_rope(vec: &mut [i64], pos: usize, d_head: usize, cos: &[i64], sin: &[i64]) {
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

/// SiLU approximation: x * sigmoid(x) ≈ x * (x > 0 ? 1 : 0.25)
/// Simple but effective for integer arithmetic.
fn silu_i64(x: i64) -> i64 {
    if x > 0 { x } else { x >> 2 }
}

impl CachedIntegerModel {
    /// Run forward pass for a SINGLE new token (uses KV cache for previous tokens).
    /// Returns logits [vocab_size] in Q16.
    pub fn forward_one_token(&self, token: u32, cache: &mut KVCache) -> Vec<i64> {
        let cfg = &self.config;
        let d = cfg.d_model;
        let pos = cache.seq_len;

        // Embed token
        let idx = (token as usize).min(cfg.vocab_size - 1);
        let mut hidden: Vec<i64> = self.embedding[idx * d..(idx + 1) * d].to_vec();

        // Process each layer
        for (layer_idx, layer) in self.layers.iter().enumerate() {
            // Pre-norm
            let normed = layernorm(&hidden, &layer.attn_norm);

            // Q, K, V projections (only for this one token)
            let mut q = matmul(&layer.wq, &normed, d, d);
            let mut k = matmul(&layer.wk, &normed, d, cfg.d_kv);
            let mut v = matmul(&layer.wv, &normed, d, cfg.d_kv);

            // Apply RoPE to Q and K (per-head)
            for h in 0..cfg.n_heads {
                apply_rope(
                    &mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin,
                );
            }
            for h in 0..cfg.n_kv_heads {
                apply_rope(
                    &mut k[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin,
                );
            }

            // Append K, V to cache
            cache.k[layer_idx].extend_from_slice(&k);
            cache.v[layer_idx].extend_from_slice(&v);

            // Multi-head attention with KV cache
            let full_seq = pos + 1;
            let mut attn_out = vec![0i64; d];

            // Parallel across heads
            let head_results: Vec<Vec<i64>> = (0..cfg.n_heads).into_par_iter().map(|h| {
                let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                let dh = cfg.d_head;

                // Q for this head (current position only)
                let q_head = &q[h * dh..(h + 1) * dh];

                // Compute attention scores against all cached K
                let mut scores = Vec::with_capacity(full_seq);
                for j in 0..full_seq {
                    let k_head = &cache.k[layer_idx][j * cfg.d_kv + kv_h * dh..j * cfg.d_kv + (kv_h + 1) * dh];
                    let mut dot: i64 = 0;
                    for dd in 0..dh {
                        dot += q_head[dd] * k_head[dd];
                    }
                    scores.push((dot >> FRAC_BITS) * cfg.attn_scale >> FRAC_BITS);
                }

                // Softmax
                let attn_weights = softmax_i64(&scores);

                // Weighted sum of V
                let mut out = vec![0i64; dh];
                for j in 0..full_seq {
                    let v_head = &cache.v[layer_idx][j * cfg.d_kv + kv_h * dh..j * cfg.d_kv + (kv_h + 1) * dh];
                    for dd in 0..dh {
                        out[dd] += (attn_weights[j] * v_head[dd]) >> FRAC_BITS;
                    }
                }
                out
            }).collect();

            // Gather head results
            for (h, head_out) in head_results.iter().enumerate() {
                attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(head_out);
            }

            // Output projection + residual
            let projected = matmul(&layer.wo, &attn_out, d, d);
            for i in 0..d {
                hidden[i] += projected[i];
            }

            // FFN: pre-norm → gate/up → SiLU → down → residual
            let normed_ff = layernorm(&hidden, &layer.ffn_norm);
            let gate = matmul(&layer.w_gate, &normed_ff, d, cfg.d_ff);
            let up = matmul(&layer.w_up, &normed_ff, d, cfg.d_ff);

            // SiLU gate * up
            let gated: Vec<i64> = gate.iter().zip(up.iter())
                .map(|(&g, &u)| (silu_i64(g) * u) >> FRAC_BITS)
                .collect();

            let ff_out = matmul(&layer.w_down, &gated, cfg.d_ff, d);
            for i in 0..d {
                hidden[i] += ff_out[i];
            }
        }

        cache.seq_len = pos + 1;

        // Final norm + LM head
        let normed = layernorm(&hidden, &self.final_norm);
        matmul(&self.output_weight, &normed, d, cfg.vocab_size)
    }

    /// Generate tokens autoregressively with KV cache.
    pub fn generate(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> (Vec<u32>, Hash256) {
        let mut cache = KVCache::new(self.config.n_layers);
        let mut generated = Vec::new();

        // Process prompt tokens (build up KV cache)
        for &tok in prompt {
            let _logits = self.forward_one_token(tok, &mut cache);
        }

        // Generate new tokens
        for _ in 0..max_tokens {
            let last_token = generated.last().copied()
                .unwrap_or(*prompt.last().unwrap_or(&0));
            let logits = self.forward_one_token(last_token, &mut cache);
            let next = argmax_i64(&logits) as u32;
            generated.push(next);
            if eos_tokens.contains(&next) { break; }
        }

        let output_bytes: Vec<u8> = generated.iter()
            .flat_map(|t| t.to_le_bytes())
            .collect();
        let hash = arc_crypto::hash_bytes(&output_bytes);
        (generated, hash)
    }
}

/// Pre-compute RoPE cos/sin tables as i64 Q16.
pub fn compute_rope_tables(d_head: usize, max_seq: usize, base: f64) -> (Vec<i64>, Vec<i64>) {
    let half = d_head / 2;
    let mut cos_table = vec![0i64; max_seq * half];
    let mut sin_table = vec![0i64; max_seq * half];

    for pos in 0..max_seq {
        for i in 0..half {
            let freq = 1.0 / base.powf(2.0 * i as f64 / d_head as f64);
            let angle = pos as f64 * freq;
            // Convert to Q16 — this uses f64 but only at init, never during forward pass
            cos_table[pos * half + i] = (angle.cos() * ONE as f64).round() as i64;
            sin_table[pos * half + i] = (angle.sin() * ONE as f64).round() as i64;
        }
    }

    (cos_table, sin_table)
}

/// Load a GGUF model into a CachedIntegerModel.
/// This is the ONE-TIME startup cost. After this, inference is pure i64 from RAM.
#[cfg(feature = "candle")]
pub fn load_cached_model(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use candle_core::Device;
    use candle_core::quantized::gguf_file;
    use crate::InferenceError;

    let device = Device::Cpu;
    let gguf_path = path.to_string();

    // Read metadata from GGUF
    let (n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size) = {
        let mut reader = std::fs::File::open(&gguf_path)
            .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
        let content = gguf_file::Content::read(&mut reader)
            .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

        let get_u32 = |key: &str| -> u32 {
            match content.metadata.get(key) {
                Some(gguf_file::Value::U32(v)) => *v,
                Some(gguf_file::Value::U64(v)) => *v as u32,
                Some(gguf_file::Value::I32(v)) => *v as u32,
                _ => 0,
            }
        };

        let nl = get_u32("llama.block_count") as usize;
        let dm = get_u32("llama.embedding_length") as usize;
        let nh = get_u32("llama.attention.head_count") as usize;
        let nkv = { let v = get_u32("llama.attention.head_count_kv"); if v > 0 { v as usize } else { nh } };
        let dff = get_u32("llama.feed_forward_length") as usize;
        let vs = content.tensor_infos.get("token_embd.weight")
            .map(|t| t.shape.dims()[0] as usize)
            .unwrap_or(32000);
        (nl, dm, nh, nkv, dff, vs)
    };

    let d_head = d_model / n_heads;
    let d_kv = d_head * n_kv_heads;

    info!(n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size, "Loading GGUF into integer cache...");

    // Helper: extract tensor as i64 Q16 (opens a fresh file handle each time)
    let extract_tensor = |name: &str| -> Result<Vec<i64>, InferenceError> {
        let mut rdr = std::fs::File::open(&gguf_path)
            .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
        let cnt = gguf_file::Content::read(&mut rdr)
            .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;
        let qt = cnt.tensor(&mut rdr, name, &device)
            .map_err(|e| InferenceError::Runtime(format!("{name}: {e}")))?;
        let deq = qt.dequantize(&device)
            .map_err(|e| InferenceError::Runtime(format!("dequant {name}: {e}")))?;
        let flat = deq.flatten_all()
            .map_err(|e| InferenceError::Runtime(format!("flatten: {e}")))?
            .to_vec1::<f32>()
            .map_err(|e| InferenceError::Runtime(format!("tovec: {e}")))?;
        Ok(flat.iter().map(|&x| (x * ONE as f32).round() as i64).collect())
    };

    let extract_or_ones = |name: &str, size: usize| -> Vec<i64> {
        extract_tensor(name).unwrap_or_else(|_| vec![ONE; size])
    };

    // Load embeddings
    let embedding = extract_tensor("token_embd.weight")?;
    info!("Embeddings loaded ({} values)", embedding.len());

    // Load output head
    let output_weight = extract_tensor("output.weight").unwrap_or_else(|_| embedding.clone());
    let final_norm = extract_or_ones("output_norm.weight", d_model);

    // Load all layers
    let mut layers = Vec::with_capacity(n_layers);
    for l in 0..n_layers {
        let p = format!("blk.{l}");
        info!("Loading layer {}/{}", l + 1, n_layers);
        layers.push(CachedLayer {
            wq: extract_tensor(&format!("{p}.attn_q.weight"))?,
            wk: extract_tensor(&format!("{p}.attn_k.weight"))?,
            wv: extract_tensor(&format!("{p}.attn_v.weight"))?,
            wo: extract_tensor(&format!("{p}.attn_output.weight"))?,
            w_gate: extract_tensor(&format!("{p}.ffn_gate.weight"))?,
            w_up: extract_tensor(&format!("{p}.ffn_up.weight"))?,
            w_down: extract_tensor(&format!("{p}.ffn_down.weight"))?,
            attn_norm: extract_or_ones(&format!("{p}.attn_norm.weight"), d_model),
            ffn_norm: extract_or_ones(&format!("{p}.ffn_norm.weight"), d_model),
        });
    }

    // Compute RoPE tables
    let max_seq = 2048;
    let rope_base = 10000.0;
    let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, rope_base);

    let attn_scale = {
        let isqrt = integer_isqrt((d_head as i64) * ONE);
        (ONE * ONE) / isqrt.max(1)
    };

    info!("Model loaded into integer cache. Ready for inference.");

    Ok(CachedIntegerModel {
        config: ModelConfig {
            n_layers, d_model, n_heads, n_kv_heads, d_ff, d_head, d_kv,
            vocab_size, attn_scale, rope_cos, rope_sin, max_seq,
        },
        embedding,
        layers,
        final_norm,
        output_weight,
    })
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model(_path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime("candle feature not enabled".into()))
}
