// Shared synthetic fixture; included only in unit tests. No model loading.
fn build_test_model(vs: usize, d: usize, nh: usize, dff: usize, nl: usize) -> CachedIntegerModel {
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
