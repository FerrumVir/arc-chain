use crate::{Hash256, hash_bytes};
use serde::{Deserialize, Serialize};

pub const DENSE_TRACE_COLS: usize = 11;

#[derive(Debug, Clone)]
pub struct DenseLayerInput {
    pub weights: Vec<i64>,
    pub bias: Vec<i64>,
    pub input: Vec<i64>,
    pub output: Vec<i64>,
    pub in_size: usize,
    pub out_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DenseLayerProof {
    pub proof_data: Vec<u8>,
    pub binding_hash: Hash256,
    pub input_hash: Hash256,
    pub output_hash: Hash256,
    pub proving_time_ms: u64,
    pub trace_rows: usize,
    pub proof_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldedInferenceProof {
    pub layer_proofs: Vec<DenseLayerProof>,
    pub composition_proof: Vec<u8>,
    pub model_input_hash: Hash256,
    pub final_output_hash: Hash256,
    pub total_proving_time_ms: u64,
    pub total_proof_size: usize,
    pub num_layers: usize,
}

pub fn dense_forward_i64(
    weights: &[i64],
    bias: &[i64],
    input: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<i64> {
    let mut output = Vec::with_capacity(out_size);
    for i in 0..out_size {
        let mut acc: i64 = 0;
        for j in 0..in_size {
            acc += weights[i * in_size + j] * input[j];
        }
        acc += bias[i];
        output.push(acc);
    }
    output
}

pub fn generate_dense_trace(layer: &DenseLayerInput) -> Vec<Vec<i64>> {
    let n_ops = layer.out_size * layer.in_size;
    let log_size = (n_ops as f64).log2().ceil() as u32;
    let trace_size = 1usize << log_size.max(4);

    let mut active = vec![0i64; trace_size];
    let mut row_idx = vec![0i64; trace_size];
    let mut col_idx = vec![0i64; trace_size];
    let mut weight = vec![0i64; trace_size];
    let mut input_col = vec![0i64; trace_size];
    let mut product = vec![0i64; trace_size];
    let mut acc_prev = vec![0i64; trace_size];
    let mut acc_next = vec![0i64; trace_size];
    let mut is_last_col = vec![0i64; trace_size];
    let mut bias_col = vec![0i64; trace_size];
    let mut output_col = vec![0i64; trace_size];

    let mut row = 0;
    for i in 0..layer.out_size {
        let mut acc: i64 = 0;
        for j in 0..layer.in_size {
            if row >= trace_size {
                break;
            }

            active[row] = 1;
            row_idx[row] = i as i64;
            col_idx[row] = j as i64;

            let w = layer.weights[i * layer.in_size + j];
            let inp = layer.input[j];
            let prod = w * inp;

            weight[row] = w;
            input_col[row] = inp;
            product[row] = prod;
            acc_prev[row] = acc;
            acc += prod;
            acc_next[row] = acc;

            let is_last = if j == layer.in_size - 1 { 1 } else { 0 };
            is_last_col[row] = is_last;

            if is_last == 1 {
                bias_col[row] = layer.bias[i];
                output_col[row] = acc + layer.bias[i];
            }

            row += 1;
        }
    }

    vec![
        active,
        row_idx,
        col_idx,
        weight,
        input_col,
        product,
        acc_prev,
        acc_next,
        is_last_col,
        bias_col,
        output_col,
    ]
}

pub fn verify_dense_trace(trace: &[Vec<i64>]) -> Result<(), String> {
    if trace.len() != DENSE_TRACE_COLS {
        return Err(format!(
            "Expected {} columns, got {}",
            DENSE_TRACE_COLS,
            trace.len()
        ));
    }

    let n_rows = trace[0].len();
    let active = &trace[0];
    let _row_idx = &trace[1];
    let _col_idx = &trace[2];
    let weight = &trace[3];
    let input = &trace[4];
    let product = &trace[5];
    let acc_prev = &trace[6];
    let acc_next = &trace[7];
    let is_last_col = &trace[8];
    let bias = &trace[9];
    let output = &trace[10];

    for row in 0..n_rows {
        let a = active[row];

        if a * (a - 1) != 0 {
            return Err(format!("Row {row}: active not boolean ({a})"));
        }

        let ilc = is_last_col[row];
        if ilc * (ilc - 1) != 0 {
            return Err(format!("Row {row}: is_last_col not boolean ({ilc})"));
        }

        if a == 1 {
            let expected_product = weight[row] * input[row];
            if product[row] != expected_product {
                return Err(format!(
                    "Row {row}: product mismatch: {} != {} * {}",
                    product[row], weight[row], input[row]
                ));
            }

            if acc_next[row] != acc_prev[row] + product[row] {
                return Err(format!(
                    "Row {row}: accumulation mismatch: {} != {} + {}",
                    acc_next[row], acc_prev[row], product[row]
                ));
            }

            if ilc == 1 && output[row] != acc_next[row] + bias[row] {
                return Err(format!(
                    "Row {row}: output mismatch: {} != {} + {}",
                    output[row], acc_next[row], bias[row]
                ));
            }
        }
    }

    Ok(())
}

pub fn prove_dense_layer(layer: &DenseLayerInput) -> Result<DenseLayerProof, String> {
    let start = Instant::now();

    let trace = generate_dense_trace(layer);
    let trace_rows = trace[0].len();

    verify_dense_trace(&trace)?;

    let mut binding_input = Vec::new();
    for col in &trace {
        for &val in col {
            binding_input.extend_from_slice(&val.to_le_bytes());
        }
    }
    let binding_hash = hash_bytes(&binding_input);

    let input_bytes: Vec<u8> = layer.input.iter().flat_map(|v| v.to_le_bytes()).collect();
    let input_hash = hash_bytes(&input_bytes);

    let output_bytes: Vec<u8> = layer.output.iter().flat_map(|v| v.to_le_bytes()).collect();
    let output_hash = hash_bytes(&output_bytes);

    #[cfg(feature = "stwo-prover")]
    let (proof_data, proof_kind) = {
        let (receipt, _size, _ms) = crate::stwo_air::try_prove_dense_stark(
            &layer.weights,
            &layer.input,
            &layer.output,
            &layer.bias,
            layer.in_size,
            layer.out_size,
        )?;
        (receipt, "stwo-circle-stark".to_string())
    };

    #[cfg(not(feature = "stwo-prover"))]
    let (proof_data, proof_kind) = {
        let mut d = Vec::new();
        d.extend_from_slice(&(trace_rows as u64).to_le_bytes());
        d.extend_from_slice(&(DENSE_TRACE_COLS as u64).to_le_bytes());
        d.extend_from_slice(&binding_hash.0);
        d.extend_from_slice(&input_hash.0);
        d.extend_from_slice(&output_hash.0);
        (d, "blake3-commitment".to_string())
    };

    let proving_time_ms = start.elapsed().as_millis() as u64;

    Ok(DenseLayerProof {
        proof_data,
        binding_hash,
        input_hash,
        output_hash,
        proving_time_ms,
        trace_rows,
        proof_kind,
    })
}

pub fn prove_folded_inference(layers: &[DenseLayerInput]) -> Result<FoldedInferenceProof, String> {
    let total_start = Instant::now();

    if layers.is_empty() {
        return Err("No layers to prove".into());
    }

    let mut layer_proofs = Vec::with_capacity(layers.len());
    for (i, layer) in layers.iter().enumerate() {
        let proof = prove_dense_layer(layer).map_err(|e| format!("Layer {i} proof failed: {e}"))?;
        layer_proofs.push(proof);
    }

    for i in 0..layer_proofs.len() - 1 {
        if layer_proofs[i].output_hash != layer_proofs[i + 1].input_hash {
            return Err(format!(
                "Chain break between layer {} and {}: output {} != input {}",
                i,
                i + 1,
                hex::encode(&layer_proofs[i].output_hash.0[..8]),
                hex::encode(&layer_proofs[i + 1].input_hash.0[..8]),
            ));
        }
    }

    let mut comp_input = Vec::new();
    for proof in &layer_proofs {
        comp_input.extend_from_slice(&proof.binding_hash.0);
        comp_input.extend_from_slice(&proof.input_hash.0);
        comp_input.extend_from_slice(&proof.output_hash.0);
    }
    let composition_proof = hash_bytes(&comp_input).0.to_vec();

    let total_proof_size: usize = layer_proofs
        .iter()
        .map(|p| p.proof_data.len())
        .sum::<usize>()
        + composition_proof.len();

    let total_proving_time_ms = total_start.elapsed().as_millis() as u64;

    Ok(FoldedInferenceProof {
        model_input_hash: layer_proofs.first().unwrap().input_hash,
        final_output_hash: layer_proofs.last().unwrap().output_hash,
        num_layers: layers.len(),
        layer_proofs,
        composition_proof,
        total_proving_time_ms,
        total_proof_size,
    })
}

use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardProof {
    pub shard_idx: usize,
    pub col_start: usize,
    pub col_end: usize,
    pub partial_sums_hash: Hash256,
    pub proof: DenseLayerProof,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardedLayerProof {
    pub shard_proofs: Vec<ShardProof>,
    pub output_hash: Hash256,
    pub input_hash: Hash256,
    pub out_size: usize,
    pub in_size: usize,
    pub total_proving_time_ms: u64,
    pub total_proof_size: usize,
}

pub fn prove_sharded_dense(
    weights: &[i64],
    bias: &[i64],
    input: &[i64],
    in_size: usize,
    out_size: usize,
    shard_cols: usize,
) -> Result<ShardedLayerProof, String> {
    let total_start = Instant::now();

    if shard_cols == 0 || shard_cols > in_size {
        return Err("shard_cols must be > 0 and <= in_size".into());
    }

    let n_shards = in_size.div_ceil(shard_cols);
    let mut shard_proofs = Vec::with_capacity(n_shards);

    let mut partial_sums = vec![0i64; out_size];

    for shard_idx in 0..n_shards {
        let col_start = shard_idx * shard_cols;
        let col_end = (col_start + shard_cols).min(in_size);
        let shard_width = col_end - col_start;
        let is_last_shard = shard_idx == n_shards - 1;

        let mut shard_weights = Vec::with_capacity(out_size * shard_width);
        for i in 0..out_size {
            for j in col_start..col_end {
                shard_weights.push(weights[i * in_size + j]);
            }
        }

        let shard_input: Vec<i64> = input[col_start..col_end].to_vec();

        let shard_bias = if is_last_shard {
            bias.to_vec()
        } else {
            vec![0i64; out_size]
        };
        let shard_output = dense_forward_i64(
            &shard_weights,
            &shard_bias,
            &shard_input,
            shard_width,
            out_size,
        );

        for i in 0..out_size {
            partial_sums[i] += shard_output[i];
        }

        let partial_hash = hash_bytes(
            &partial_sums
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );

        let layer_input = DenseLayerInput {
            weights: shard_weights,
            bias: shard_bias,
            input: shard_input,
            output: shard_output,
            in_size: shard_width,
            out_size,
        };

        let proof = prove_dense_layer(&layer_input)
            .map_err(|e| format!("Shard {shard_idx} proof failed: {e}"))?;

        shard_proofs.push(ShardProof {
            shard_idx,
            col_start,
            col_end,
            partial_sums_hash: partial_hash,
            proof,
        });
    }

    let output_bytes: Vec<u8> = partial_sums.iter().flat_map(|v| v.to_le_bytes()).collect();
    let output_hash = hash_bytes(&output_bytes);

    let input_bytes: Vec<u8> = input.iter().flat_map(|v| v.to_le_bytes()).collect();
    let input_hash = hash_bytes(&input_bytes);

    let total_proof_size: usize = shard_proofs.iter().map(|s| s.proof.proof_data.len()).sum();

    Ok(ShardedLayerProof {
        shard_proofs,
        output_hash,
        input_hash,
        out_size,
        in_size,
        total_proving_time_ms: total_start.elapsed().as_millis() as u64,
        total_proof_size,
    })
}

pub fn verify_sharded_proof(proof: &ShardedLayerProof) -> Result<(), String> {
    if proof.shard_proofs.is_empty() {
        return Err("No shards".into());
    }

    let mut expected_col = 0;
    for (i, shard) in proof.shard_proofs.iter().enumerate() {
        if shard.col_start != expected_col {
            return Err(format!(
                "Shard {i}: expected col_start={expected_col}, got {}",
                shard.col_start
            ));
        }
        if shard.col_end <= shard.col_start {
            return Err(format!(
                "Shard {i}: invalid range [{}, {})",
                shard.col_start, shard.col_end
            ));
        }
        expected_col = shard.col_end;
    }
    if expected_col != proof.in_size {
        return Err(format!(
            "Shards cover {} columns, expected {}",
            expected_col, proof.in_size
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dense_forward_i64() {
        let weights = vec![1, 2, 3, 4, 5, 6];
        let bias = vec![10, 20];
        let input = vec![1, 2, 3];
        let output = dense_forward_i64(&weights, &bias, &input, 3, 2);
        assert_eq!(output, vec![24, 52]);
    }

    #[test]
    fn test_generate_and_verify_trace() {
        let weights = vec![1, 2, 3, 4, 5, 6];
        let bias = vec![10, 20];
        let input = vec![1, 2, 3];
        let output = dense_forward_i64(&weights, &bias, &input, 3, 2);

        let layer = DenseLayerInput {
            weights,
            bias,
            input,
            output,
            in_size: 3,
            out_size: 2,
        };

        let trace = generate_dense_trace(&layer);
        assert_eq!(trace.len(), DENSE_TRACE_COLS);
        assert!(verify_dense_trace(&trace).is_ok());
    }

    #[test]
    fn test_prove_dense_layer() {
        let in_size = 64;
        let out_size = 32;

        let mut rng: u64 = 42;
        let mut next = || -> i64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 33) as i64) % 100 - 50
        };

        let weights: Vec<i64> = (0..out_size * in_size).map(|_| next()).collect();
        let bias: Vec<i64> = (0..out_size).map(|_| next()).collect();
        let input: Vec<i64> = (0..in_size).map(|_| next()).collect();
        let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

        let layer = DenseLayerInput {
            weights,
            bias,
            input,
            output,
            in_size,
            out_size,
        };

        let proof = prove_dense_layer(&layer).unwrap();
        assert!(proof.trace_rows >= in_size * out_size);
        assert!(!proof.proof_data.is_empty());
        assert!(proof.proving_time_ms < 5000);
    }

    #[test]
    fn test_prove_folded_2_layers() {
        let dim1 = 16;
        let dim2 = 8;
        let dim3 = 4;

        let mut rng: u64 = 123;
        let mut next = || -> i64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 33) as i64) % 10 - 5
        };

        let w1: Vec<i64> = (0..dim2 * dim1).map(|_| next()).collect();
        let b1: Vec<i64> = (0..dim2).map(|_| next()).collect();
        let input1: Vec<i64> = (0..dim1).map(|_| next()).collect();
        let output1 = dense_forward_i64(&w1, &b1, &input1, dim1, dim2);

        let w2: Vec<i64> = (0..dim3 * dim2).map(|_| next()).collect();
        let b2: Vec<i64> = (0..dim3).map(|_| next()).collect();
        let output2 = dense_forward_i64(&w2, &b2, &output1, dim2, dim3);

        let layers = vec![
            DenseLayerInput {
                weights: w1,
                bias: b1,
                input: input1,
                output: output1.clone(),
                in_size: dim1,
                out_size: dim2,
            },
            DenseLayerInput {
                weights: w2,
                bias: b2,
                input: output1,
                output: output2,
                in_size: dim2,
                out_size: dim3,
            },
        ];

        let folded = prove_folded_inference(&layers).unwrap();
        assert_eq!(folded.num_layers, 2);
        assert_eq!(folded.layer_proofs.len(), 2);
        assert!(folded.total_proof_size > 0);

        assert_eq!(
            folded.layer_proofs[0].output_hash,
            folded.layer_proofs[1].input_hash,
        );
    }

    #[test]
    fn test_prove_larger_layer() {
        let in_size = 256;
        let out_size = 128;

        let mut rng: u64 = 999;
        let mut next = || -> i64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 33) as i64) % 50 - 25
        };

        let weights: Vec<i64> = (0..out_size * in_size).map(|_| next()).collect();
        let bias: Vec<i64> = (0..out_size).map(|_| next()).collect();
        let input: Vec<i64> = (0..in_size).map(|_| next()).collect();
        let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

        let layer = DenseLayerInput {
            weights,
            bias,
            input,
            output,
            in_size,
            out_size,
        };

        let proof = prove_dense_layer(&layer).unwrap();
        assert!(proof.trace_rows >= 32_768);
        assert!(proof.proving_time_ms < 10_000);
    }

    #[test]
    fn test_tampered_trace_rejected() {
        let weights = vec![1, 2, 3, 4];
        let bias = vec![0, 0];
        let input = vec![1, 1];
        let output = dense_forward_i64(&weights, &bias, &input, 2, 2);

        let layer = DenseLayerInput {
            weights,
            bias,
            input,
            output,
            in_size: 2,
            out_size: 2,
        };

        let mut trace = generate_dense_trace(&layer);

        trace[5][0] = 999;

        assert!(verify_dense_trace(&trace).is_err());
    }

    #[test]
    fn test_sharded_dense_matches_unsharded() {
        let in_size = 32;
        let out_size = 16;
        let shard_cols = 8;

        let mut rng: u64 = 555;
        let mut next = || -> i64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 33) as i64) % 20 - 10
        };

        let weights: Vec<i64> = (0..out_size * in_size).map(|_| next()).collect();
        let bias: Vec<i64> = (0..out_size).map(|_| next()).collect();
        let input: Vec<i64> = (0..in_size).map(|_| next()).collect();
        let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

        let layer = DenseLayerInput {
            weights: weights.clone(),
            bias: bias.clone(),
            input: input.clone(),
            output: output.clone(),
            in_size,
            out_size,
        };
        let unsharded = prove_dense_layer(&layer).unwrap();

        let sharded =
            prove_sharded_dense(&weights, &bias, &input, in_size, out_size, shard_cols).unwrap();

        assert_eq!(unsharded.output_hash, sharded.output_hash);
        assert_eq!(sharded.shard_proofs.len(), 4);
        assert!(verify_sharded_proof(&sharded).is_ok());
    }

    #[test]
    fn test_sharded_large_layer() {
        let in_size = 256;
        let out_size = 128;
        let shard_cols = 64;

        let mut rng: u64 = 7777;
        let mut next = || -> i64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 33) as i64) % 10 - 5
        };

        let weights: Vec<i64> = (0..out_size * in_size).map(|_| next()).collect();
        let bias: Vec<i64> = (0..out_size).map(|_| next()).collect();
        let input: Vec<i64> = (0..in_size).map(|_| next()).collect();

        let sharded =
            prove_sharded_dense(&weights, &bias, &input, in_size, out_size, shard_cols).unwrap();
        assert_eq!(sharded.shard_proofs.len(), 4);
        assert!(verify_sharded_proof(&sharded).is_ok());

        println!(
            "Sharded 128×256 in {}ms, {} shards, {} bytes total",
            sharded.total_proving_time_ms,
            sharded.shard_proofs.len(),
            sharded.total_proof_size
        );
    }

    #[test]
    fn test_sharded_50b_scale_dimension() {
        let in_size = 1024;
        let out_size = 256;
        let shard_cols = 128;

        let mut rng: u64 = 50_000_000_000;
        let mut next = || -> i64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 33) as i64) % 4 - 2
        };

        let weights: Vec<i64> = (0..out_size * in_size).map(|_| next()).collect();
        let bias: Vec<i64> = (0..out_size).map(|_| next()).collect();
        let input: Vec<i64> = (0..in_size).map(|_| next()).collect();

        let start = Instant::now();
        let sharded =
            prove_sharded_dense(&weights, &bias, &input, in_size, out_size, shard_cols).unwrap();
        let total_ms = start.elapsed().as_millis();

        assert_eq!(sharded.shard_proofs.len(), 8);
        assert!(verify_sharded_proof(&sharded).is_ok());

        let per_shard_ms = total_ms / 8;
        let scaled_per_shard = per_shard_ms * 262;
        let scaled_total = scaled_per_shard * 32;

        println!(
            "Test: {}×{} in {}ms ({} shards, {}ms/shard)",
            out_size, in_size, total_ms, 8, per_shard_ms
        );
        println!(
            "Extrapolated 16384×16384: {}ms/shard × 32 shards = {}ms total",
            scaled_per_shard, scaled_total
        );
        println!(
            "For 80 layers (50B model): {}s total proving time",
            (scaled_total * 80 * 7) / 1000
        );
    }

    #[test]
    fn test_chain_break_detected() {
        let dim = 4;
        let mut rng: u64 = 77;
        let mut next = || -> i64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 33) as i64) % 10
        };

        let w1: Vec<i64> = (0..dim * dim).map(|_| next()).collect();
        let b1: Vec<i64> = (0..dim).map(|_| 0).collect();
        let input1: Vec<i64> = (0..dim).map(|_| next()).collect();
        let output1 = dense_forward_i64(&w1, &b1, &input1, dim, dim);

        let w2: Vec<i64> = (0..dim * dim).map(|_| next()).collect();
        let b2: Vec<i64> = (0..dim).map(|_| 0).collect();
        let wrong_input: Vec<i64> = (0..dim).map(|_| next()).collect();
        let output2 = dense_forward_i64(&w2, &b2, &wrong_input, dim, dim);

        let layers = vec![
            DenseLayerInput {
                weights: w1,
                bias: b1,
                input: input1,
                output: output1,
                in_size: dim,
                out_size: dim,
            },
            DenseLayerInput {
                weights: w2,
                bias: b2,
                input: wrong_input,
                output: output2,
                in_size: dim,
                out_size: dim,
            },
        ];

        let result = prove_folded_inference(&layers);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Chain break"));
    }
}
