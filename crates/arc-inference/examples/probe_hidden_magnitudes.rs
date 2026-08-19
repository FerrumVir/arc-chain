// Traces hidden-state magnitudes through every layer of the integer forward,
// dumping max/mean/p99 per stage. The first layer where magnitudes diverge
// meaningfully from the expected Q16 range (real values ~[-5, +5]) is the
// bug site.
//
// Reconstructs the forward manually using public primitives so we don't have
// to fork forward_one_token. Uses the same dispatch order as production
// (block-i8 > i8 fallback).

use arc_inference::block_i8;
use arc_inference::cached_integer_model::{
    load_cached_model, KVCache, apply_rope, layernorm, matmul_fast, silu_i64,
};
use arc_inference::integer_lut::{integer_exp, FRAC_BITS, ONE};

const MODEL_PATH: &str = "/Users/tjdunham/.arc-models/llama-2-7b.gguf";

fn stats(label: &str, v: &[i64]) {
    let abs: Vec<i64> = v.iter().map(|x| x.abs()).collect();
    let max = *abs.iter().max().unwrap_or(&0);
    let mean: i64 = if abs.is_empty() { 0 } else { abs.iter().sum::<i64>() / abs.len() as i64 };
    let mut sorted = abs.clone();
    sorted.sort();
    let p50 = if sorted.is_empty() { 0 } else { sorted[sorted.len() / 2] };
    let p99 = if sorted.is_empty() { 0 } else { sorted[sorted.len() * 99 / 100] };
    println!(
        "  {:40} max={:>15} mean={:>12} p50={:>10} p99={:>12}  (real: max={:>9.3} mean={:>7.3})",
        label,
        max, mean, p50, p99,
        max as f64 / ONE as f64,
        mean as f64 / ONE as f64,
    );
}

fn main() {
    println!("Loading...");
    let model = load_cached_model(MODEL_PATH).expect("load");
    let cfg = &model.config;
    let d = cfg.d_model;
    println!("Loaded. d_model={} n_layers={} d_ff={}\n", d, cfg.n_layers, cfg.d_ff);

    let block_layers = model.block_i8_layers.as_ref().expect("need block_i8_layers");
    println!("Using block-i8 for Q/K/V/O/gate/up/down.\n");

    // Token: BOS (1), then a known content token.
    let mut cache = KVCache::new(cfg.n_layers);
    for (ti, token) in [1u32, 5000u32].iter().enumerate() {
        let pos = cache.seq_len;
        println!("=========================================================");
        println!("TOKEN {} (id={}), pos={}", ti, token, pos);
        println!("=========================================================");

        let idx = (*token as usize).min(cfg.vocab_size - 1);
        let emb_start = idx * d;
        let mut hidden: Vec<i64> = model.embedding_q16[emb_start..emb_start + d].to_vec();
        stats("emb (after lookup)", &hidden);

        for (li, layer) in model.layers.iter().enumerate() {
            let blk = &block_layers[li];

            // Attention norm
            let normed = layernorm(&hidden, &layer.attn_norm);
            if li == 0 || li == cfg.n_layers - 1 || li % 8 == 0 {
                stats(&format!("L{:02} attn_norm out", li), &normed);
            }

            // Q/K/V
            let mut q = vec![0i64; d];
            let mut k_buf = vec![0i64; cfg.d_kv];
            let mut v_buf = vec![0i64; cfg.d_kv];
            block_i8::matmul_block_i8_into(&blk.wq, &normed, &mut q);
            block_i8::matmul_block_i8_into(&blk.wk, &normed, &mut k_buf);
            block_i8::matmul_block_i8_into(&blk.wv, &normed, &mut v_buf);
            if li <= 2 || li == cfg.n_layers - 1 {
                stats(&format!("L{:02} attn_norm out", li), &normed);
                stats(&format!("L{:02} Q (post-matmul, pre-RoPE)", li), &q);
                stats(&format!("L{:02} K (post-matmul, pre-RoPE)", li), &k_buf);
                stats(&format!("L{:02} V (post-matmul)", li), &v_buf);
            }

            // RoPE
            for h in 0..cfg.n_heads {
                apply_rope(&mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin);
            }
            for h in 0..cfg.n_kv_heads {
                apply_rope(&mut k_buf[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin);
            }
            if li == 0 || li == cfg.n_layers - 1 {
                stats(&format!("L{:02} Q (post-RoPE)", li), &q);
                stats(&format!("L{:02} K (post-RoPE)", li), &k_buf);
            }

            // Push K/V into cache
            cache.push_k(li, &k_buf);
            cache.push_v(li, &v_buf);

            // Attention: simple serial scan over full_seq
            let full_seq = pos + 1;
            let mut attn_out = vec![0i64; d];
            for h in 0..cfg.n_heads {
                let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                let dh = cfg.d_head;
                let q_head = &q[h * dh..(h + 1) * dh];
                let mut out_head = vec![0i64; dh];
                let mut running_max = i64::MIN / 2;
                let mut running_sum: i64 = 0;

                let k_data = &cache.k_data[li];
                let v_data = &cache.v_data[li];

                for j in 0..full_seq {
                    let k_off = j * cfg.d_kv + kv_h * dh;
                    let mut dot: i64 = 0;
                    for dd in 0..dh {
                        dot += q_head[dd] * k_data[k_off + dd];
                    }
                    let score = ((dot >> FRAC_BITS) * cfg.attn_scale) >> FRAC_BITS;

                    if score > running_max {
                        let diff = running_max - score;
                        let correction = integer_exp(diff);
                        running_sum = (running_sum * correction) >> FRAC_BITS;
                        for dd in 0..dh {
                            out_head[dd] = (out_head[dd] * correction) >> FRAC_BITS;
                        }
                        running_max = score;
                    }
                    let w = integer_exp(score - running_max);
                    running_sum += w;

                    let v_off = j * cfg.d_kv + kv_h * dh;
                    for dd in 0..dh {
                        out_head[dd] += (w * v_data[v_off + dd]) >> FRAC_BITS;
                    }
                }

                if running_sum > 0 {
                    for dd in 0..dh {
                        out_head[dd] = (out_head[dd] * ONE) / running_sum;
                    }
                }

                if li == 0 && h == 0 {
                    stats(&format!("L{:02} attn running_max/sum→ head0", li), &[running_max, running_sum]);
                    stats(&format!("L{:02} attn head0 out", li), &out_head);
                }

                attn_out[h * dh..(h + 1) * dh].copy_from_slice(&out_head);
            }

            // Wo projection + residual
            let mut projected = vec![0i64; d];
            block_i8::matmul_block_i8_into(&blk.wo, &attn_out, &mut projected);
            if li <= 2 || li == cfg.n_layers - 1 {
                stats(&format!("L{:02} Wo out", li), &projected);
            }
            for i in 0..d { hidden[i] += projected[i]; }
            if li <= 2 {
                stats(&format!("L{:02} hidden after Wo residual", li), &hidden);
            }

            // FFN norm + gate/up/down
            let normed_ff = layernorm(&hidden, &layer.ffn_norm);
            let mut gate = vec![0i64; cfg.d_ff];
            let mut up = vec![0i64; cfg.d_ff];
            block_i8::matmul_block_i8_into(&blk.w_gate, &normed_ff, &mut gate);
            block_i8::matmul_block_i8_into(&blk.w_up, &normed_ff, &mut up);
            if li <= 2 || li == cfg.n_layers - 1 {
                stats(&format!("L{:02} ffn_norm out", li), &normed_ff);
                stats(&format!("L{:02} gate (pre-SiLU)", li), &gate);
                stats(&format!("L{:02} up", li), &up);
            }

            for j in 0..cfg.d_ff {
                gate[j] = (silu_i64(gate[j]) * up[j]) >> FRAC_BITS;
            }
            if li <= 2 || li == cfg.n_layers - 1 {
                stats(&format!("L{:02} SiLU(gate)*up", li), &gate);
            }

            let mut ff_out = vec![0i64; d];
            block_i8::matmul_block_i8_into(&blk.w_down, &gate, &mut ff_out);
            if li <= 2 || li == cfg.n_layers - 1 {
                stats(&format!("L{:02} w_down out", li), &ff_out);
            }

            for i in 0..d { hidden[i] += ff_out[i]; }

            // Print every layer's end-of-layer hidden max so we can see exactly
            // where the residual stream diverges from the expected bounded range.
            stats(&format!("L{:02} hidden (end of layer)", li), &hidden);
        }
        cache.seq_len = pos + 1;

        // Final norm + LM head
        let normed = layernorm(&hidden, &model.final_norm);
        stats("FINAL layernorm out", &normed);

        let logits = if let Some(blk_out) = &model.block_i8_output {
            let mut logits = vec![0i64; cfg.vocab_size];
            block_i8::matmul_block_i8_into(blk_out, &normed, &mut logits);
            logits
        } else {
            matmul_fast(&model.output_weight, &normed, d, cfg.vocab_size)
        };
        stats("LOGITS", &logits);

        let amax = logits.iter().enumerate().max_by_key(|(_, v)| **v).unwrap();
        println!("  argmax = {} (logit = {}, real = {:.3})\n",
            amax.0, amax.1, *amax.1 as f64 / ONE as f64);
    }
}
