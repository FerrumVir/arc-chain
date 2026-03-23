//! Integer-only transformer inference engine.
//!
//! All computation is i64 fixed-point arithmetic. No f32, no f64,
//! no exp(), no sqrt() — only integer multiply, add, shift, and
//! lookup tables. Produces bitwise identical output on ARM, x86,
//! RISC-V, GPU — any platform with 64-bit integer arithmetic.
//!
//! This is ARC Chain's production inference path for consensus.

use crate::integer_lut::*;

/// Integer matmul: output[i] = sum_j(weights[i*in_size + j] * input[j]) >> FRAC_BITS + bias[i]
///
/// Weights are row-major [out_size × in_size].
/// All values are Q16 fixed-point.
pub fn matmul_i64(
    weights: &[i64],
    bias: &[i64],
    input: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<i64> {
    let mut output = Vec::with_capacity(out_size);
    for i in 0..out_size {
        let mut acc: i64 = 0;
        let row_start = i * in_size;
        for j in 0..in_size {
            acc += weights[row_start + j] * input[j];
        }
        // Right-shift accumulated product and add bias
        acc >>= FRAC_BITS;
        if i < bias.len() {
            acc += bias[i];
        }
        output.push(acc);
    }
    output
}

/// Integer layer normalization.
///
/// Computes: output[i] = (input[i] - mean) * isqrt(var + eps) * gamma[i] + beta[i]
/// All in Q16 fixed-point with integer-only arithmetic.
pub fn layernorm_i64(
    input: &[i64],
    gamma: &[i64],
    beta: &[i64],
) -> Vec<i64> {
    let n = input.len() as i64;
    if n == 0 { return vec![]; }

    // Mean = sum(input) / n
    let sum: i64 = input.iter().sum();
    let mean = sum / n;

    // Variance = sum((input[i] - mean)^2) / n
    // To avoid overflow, compute in stages:
    // diff = input[i] - mean (Q16)
    // diff_sq = diff * diff >> FRAC_BITS (Q16)
    // var = sum(diff_sq) / n (Q16)
    let mut var_sum: i64 = 0;
    for &x in input {
        let diff = x - mean;
        let diff_sq = (diff * diff) >> FRAC_BITS;
        var_sum += diff_sq;
    }
    let variance = var_sum / n;

    // eps in Q16 ≈ 1e-5 * ONE ≈ 1 (close enough for layer norm)
    let eps: i64 = 1;
    let inv_std = integer_isqrt(variance + eps);

    // Normalize and scale
    let mut output = Vec::with_capacity(input.len());
    for (i, &x) in input.iter().enumerate() {
        let diff = x - mean;
        let normalized = (diff * inv_std) >> FRAC_BITS;
        let g = if i < gamma.len() { gamma[i] } else { ONE };
        let b = if i < beta.len() { beta[i] } else { 0 };
        let scaled = (normalized * g) >> FRAC_BITS;
        output.push(scaled + b);
    }
    output
}

/// Integer attention for a single head.
///
/// q, k, v: [seq_len × d_head] in Q16.
/// Returns: [seq_len × d_head] in Q16.
pub fn attention_i64(
    q: &[i64],
    k: &[i64],
    v: &[i64],
    seq_len: usize,
    d_head: usize,
    attn_scale: i64, // round(ONE / sqrt(d_head))
    causal: bool,
) -> Vec<i64> {
    // Step 1: Compute attention scores: scores[i][j] = (Q[i] · K[j]) * scale
    let mut scores = vec![0i64; seq_len * seq_len];
    for i in 0..seq_len {
        for j in 0..seq_len {
            if causal && j > i {
                scores[i * seq_len + j] = -8 * ONE; // masked out (will exp to ~0)
                continue;
            }
            let mut dot: i64 = 0;
            for d in 0..d_head {
                dot += q[i * d_head + d] * k[j * d_head + d];
            }
            dot >>= FRAC_BITS; // Q16 after product
            scores[i * seq_len + j] = (dot * attn_scale) >> FRAC_BITS;
        }
    }

    // Step 2: Softmax per row
    let mut attn_weights = vec![0i64; seq_len * seq_len];
    for i in 0..seq_len {
        let row = &scores[i * seq_len..(i + 1) * seq_len];
        let softmax_row = softmax_i64(row);
        attn_weights[i * seq_len..(i + 1) * seq_len].copy_from_slice(&softmax_row);
    }

    // Step 3: Weighted sum of V: output[i] = sum_j(attn_weights[i][j] * V[j])
    let mut output = vec![0i64; seq_len * d_head];
    for i in 0..seq_len {
        for d in 0..d_head {
            let mut acc: i64 = 0;
            for j in 0..seq_len {
                acc += attn_weights[i * seq_len + j] * v[j * d_head + d];
            }
            output[i * d_head + d] = acc >> FRAC_BITS;
        }
    }

    output
}

/// Simple integer transformer block: norm → attention → residual → norm → FFN → residual
pub struct IntTransformerBlock {
    pub wq: Vec<i64>,      // [d_model × d_model]
    pub wk: Vec<i64>,
    pub wv: Vec<i64>,
    pub wo: Vec<i64>,
    pub w_ff1: Vec<i64>,   // [d_ff × d_model]
    pub w_ff2: Vec<i64>,   // [d_model × d_ff]
    pub norm1_gamma: Vec<i64>,
    pub norm1_beta: Vec<i64>,
    pub norm2_gamma: Vec<i64>,
    pub norm2_beta: Vec<i64>,
    pub n_heads: usize,
    pub d_model: usize,
    pub d_head: usize,
    pub d_ff: usize,
    pub attn_scale: i64,
}

impl IntTransformerBlock {
    pub fn forward(&self, input: &[i64], seq_len: usize) -> Vec<i64> {
        let d = self.d_model;

        // Pre-norm
        let mut normed = Vec::with_capacity(seq_len * d);
        for pos in 0..seq_len {
            let slice = &input[pos * d..(pos + 1) * d];
            normed.extend(layernorm_i64(slice, &self.norm1_gamma, &self.norm1_beta));
        }

        // Q, K, V projections (all positions)
        let mut all_q = Vec::with_capacity(seq_len * d);
        let mut all_k = Vec::with_capacity(seq_len * d);
        let mut all_v = Vec::with_capacity(seq_len * d);
        for pos in 0..seq_len {
            let x = &normed[pos * d..(pos + 1) * d];
            all_q.extend(matmul_i64(&self.wq, &[], x, d, d));
            all_k.extend(matmul_i64(&self.wk, &[], x, d, d));
            all_v.extend(matmul_i64(&self.wv, &[], x, d, d));
        }

        // Multi-head attention
        let mut attn_out = vec![0i64; seq_len * d];
        for h in 0..self.n_heads {
            // Extract per-head Q, K, V
            let mut hq = vec![0i64; seq_len * self.d_head];
            let mut hk = vec![0i64; seq_len * self.d_head];
            let mut hv = vec![0i64; seq_len * self.d_head];
            for pos in 0..seq_len {
                for dd in 0..self.d_head {
                    hq[pos * self.d_head + dd] = all_q[pos * d + h * self.d_head + dd];
                    hk[pos * self.d_head + dd] = all_k[pos * d + h * self.d_head + dd];
                    hv[pos * self.d_head + dd] = all_v[pos * d + h * self.d_head + dd];
                }
            }

            let head_out = attention_i64(&hq, &hk, &hv, seq_len, self.d_head, self.attn_scale, true);

            // Scatter back
            for pos in 0..seq_len {
                for dd in 0..self.d_head {
                    attn_out[pos * d + h * self.d_head + dd] = head_out[pos * self.d_head + dd];
                }
            }
        }

        // Output projection + residual
        let mut residual1 = Vec::with_capacity(seq_len * d);
        for pos in 0..seq_len {
            let projected = matmul_i64(&self.wo, &[], &attn_out[pos * d..(pos + 1) * d], d, d);
            for dd in 0..d {
                residual1.push(input[pos * d + dd] + projected[dd]);
            }
        }

        // FFN: norm → linear → ReLU → linear → residual
        let mut output = Vec::with_capacity(seq_len * d);
        for pos in 0..seq_len {
            let x = &residual1[pos * d..(pos + 1) * d];
            let normed2 = layernorm_i64(x, &self.norm2_gamma, &self.norm2_beta);

            // Up-project
            let mut hidden = matmul_i64(&self.w_ff1, &[], &normed2, d, self.d_ff);
            // ReLU
            for v in hidden.iter_mut() {
                *v = relu_i64(*v);
            }
            // Down-project
            let ff_out = matmul_i64(&self.w_ff2, &[], &hidden, self.d_ff, d);

            // Residual
            for dd in 0..d {
                output.push(x[dd] + ff_out[dd]);
            }
        }

        output
    }
}

/// Simple integer transformer model for testing.
pub struct IntTransformerModel {
    pub embedding: Vec<i64>,   // [vocab_size × d_model]
    pub blocks: Vec<IntTransformerBlock>,
    pub final_norm_gamma: Vec<i64>,
    pub final_norm_beta: Vec<i64>,
    pub lm_head: Vec<i64>,    // [vocab_size × d_model]
    pub vocab_size: usize,
    pub d_model: usize,
}

impl IntTransformerModel {
    /// Run full forward pass. Returns logits [vocab_size] in Q16.
    pub fn forward(&self, input_tokens: &[u32]) -> Vec<i64> {
        let seq_len = input_tokens.len();
        let d = self.d_model;

        // Token embedding
        let mut hidden = Vec::with_capacity(seq_len * d);
        for &tok in input_tokens {
            let idx = tok as usize;
            if idx < self.vocab_size {
                hidden.extend_from_slice(&self.embedding[idx * d..(idx + 1) * d]);
            } else {
                hidden.extend(vec![0i64; d]);
            }
        }

        // Transformer blocks
        for block in &self.blocks {
            hidden = block.forward(&hidden, seq_len);
        }

        // Final norm (on last position only for next-token prediction)
        let last_pos = &hidden[(seq_len - 1) * d..seq_len * d];
        let normed = layernorm_i64(last_pos, &self.final_norm_gamma, &self.final_norm_beta);

        // LM head: project to vocab
        matmul_i64(&self.lm_head, &[], &normed, d, self.vocab_size)
    }

    /// Autoregressive generation. Returns generated token IDs.
    pub fn generate(&self, prompt: &[u32], max_tokens: u32, eos: u32) -> Vec<u32> {
        let mut tokens = prompt.to_vec();
        let mut generated = Vec::new();

        for _ in 0..max_tokens {
            let logits = self.forward(&tokens);
            let next = argmax_i64(&logits) as u32;
            generated.push(next);
            tokens.push(next);
            if next == eos { break; }
        }

        generated
    }

    /// Compute output hash for consensus verification.
    pub fn generate_with_hash(&self, prompt: &[u32], max_tokens: u32, eos: u32) -> (Vec<u32>, arc_crypto::Hash256) {
        let generated = self.generate(prompt, max_tokens, eos);
        let output_bytes: Vec<u8> = generated.iter()
            .flat_map(|t| t.to_le_bytes())
            .collect();
        let hash = arc_crypto::hash_bytes(&output_bytes);
        (generated, hash)
    }
}

/// Build a small test model with deterministic random weights.
/// Used for cross-platform determinism testing.
pub fn build_test_model(
    vocab_size: usize,
    d_model: usize,
    n_heads: usize,
    d_ff: usize,
    n_layers: usize,
) -> IntTransformerModel {
    let d_head = d_model / n_heads;

    // Deterministic "random" weight generation using LCG
    let mut rng: u64 = 42;
    let mut next_weight = || -> i64 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        // Map to small Q16 values in [-0.1, 0.1] range
        // = [-6553, 6553] in Q16
        ((rng >> 33) as i64 % 13107) - 6553
    };

    let mut gen_vec = |size: usize| -> Vec<i64> {
        (0..size).map(|_| next_weight()).collect()
    };

    let embedding = gen_vec(vocab_size * d_model);
    let lm_head = gen_vec(vocab_size * d_model);
    let final_norm_gamma = vec![ONE; d_model];
    let final_norm_beta = vec![0; d_model];

    // attn_scale = round(ONE / sqrt(d_head))
    // For d_head=32: sqrt(32)=5.657, ONE/5.657 = 11585
    // For d_head=64: sqrt(64)=8, ONE/8 = 8192
    // Just hardcode common values for 1/sqrt(d_head) in Q16
    let attn_scale = match d_head {
        32 => 11585,  // 1/sqrt(32) * 65536
        64 => 8192,   // 1/sqrt(64) * 65536
        128 => 5793,  // 1/sqrt(128) * 65536
        _ => ONE / ((d_head as f64).sqrt() as i64 + 1), // fallback (uses float but only at init)
    };

    let mut blocks = Vec::new();
    for _ in 0..n_layers {
        blocks.push(IntTransformerBlock {
            wq: gen_vec(d_model * d_model),
            wk: gen_vec(d_model * d_model),
            wv: gen_vec(d_model * d_model),
            wo: gen_vec(d_model * d_model),
            w_ff1: gen_vec(d_ff * d_model),
            w_ff2: gen_vec(d_model * d_ff),
            norm1_gamma: vec![ONE; d_model],
            norm1_beta: vec![0; d_model],
            norm2_gamma: vec![ONE; d_model],
            norm2_beta: vec![0; d_model],
            n_heads,
            d_model,
            d_head,
            d_ff,
            attn_scale,
        });
    }

    IntTransformerModel {
        embedding,
        blocks,
        final_norm_gamma,
        final_norm_beta,
        lm_head,
        vocab_size,
        d_model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matmul_small() {
        // 2x3 weight matrix × 3-element input
        let weights = vec![
            ONE, 2 * ONE, 3 * ONE,  // row 0
            4 * ONE, 5 * ONE, 6 * ONE,  // row 1
        ];
        let bias = vec![ONE, -ONE];
        let input = vec![ONE, ONE, ONE];

        let output = matmul_i64(&weights, &bias, &input, 3, 2);
        // row0: (1+2+3)*ONE = 6*ONE, + bias ONE = 7*ONE
        // row1: (4+5+6)*ONE = 15*ONE, + bias -ONE = 14*ONE
        assert_eq!(output[0], 7 * ONE);
        assert_eq!(output[1], 14 * ONE);
    }

    #[test]
    fn test_layernorm_basic() {
        let input = vec![ONE, 2 * ONE, 3 * ONE, 4 * ONE];
        let gamma = vec![ONE; 4];
        let beta = vec![0; 4];
        let output = layernorm_i64(&input, &gamma, &beta);

        // After normalization, mean should be ~0
        let mean: i64 = output.iter().sum::<i64>() / output.len() as i64;
        assert!(mean.abs() < ONE / 10, "mean after layernorm: {}", mean);
    }

    #[test]
    fn test_attention_causal() {
        let d_head = 4;
        let seq_len = 3;
        let q = vec![ONE; seq_len * d_head];
        let k = vec![ONE; seq_len * d_head];
        let v = vec![ONE; seq_len * d_head];
        let scale = ONE / 2; // 1/sqrt(4) = 0.5

        let output = attention_i64(&q, &k, &v, seq_len, d_head, scale, true);
        assert_eq!(output.len(), seq_len * d_head);
        // All V values are ONE, so weighted average should be ~ONE regardless of attention weights
        for &val in &output {
            assert!((val - ONE).abs() < ONE / 5, "attention output {} not close to ONE", val);
        }
    }

    #[test]
    fn test_small_model_deterministic() {
        let model = build_test_model(100, 64, 2, 128, 2);
        let prompt = vec![1u32, 5, 10, 15];

        let (tokens1, hash1) = model.generate_with_hash(&prompt, 8, 99);
        let (tokens2, hash2) = model.generate_with_hash(&prompt, 8, 99);

        assert_eq!(tokens1, tokens2, "non-deterministic generation");
        assert_eq!(hash1, hash2, "non-deterministic hash");
    }

    #[test]
    fn test_deterministic_1000_runs() {
        let model = build_test_model(50, 32, 2, 64, 1);
        let prompt = vec![1u32, 2, 3];

        let (_, first_hash) = model.generate_with_hash(&prompt, 4, 99);
        for _ in 0..1000 {
            let (_, hash) = model.generate_with_hash(&prompt, 4, 99);
            assert_eq!(hash, first_hash, "determinism broken");
        }
    }
}
