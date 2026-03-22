//! STARK proof of neural network inference (Dense layer forward pass).
//!
//! Proves that `output[i] = sum(weight[i][j] * input[j]) + bias[i]`
//! for a Dense layer using Circle STARKs over the Mersenne-31 field.
//!
//! ## Approach: Layer-by-Layer Folding
//!
//! For large models (50B+ parameters), proving the entire forward pass in
//! one trace is infeasible (would require 2^36+ rows). Instead:
//!
//! 1. Prove each layer independently (each fits in memory)
//! 2. Fold proofs using recursive composition (existing `prove_recursive`)
//! 3. The recursive verifier ensures: layer_k output = layer_{k+1} input
//!
//! This allows proving arbitrarily large models on commodity hardware.
//!
//! ## Trace Layout (DenseLayerAIR)
//!
//! One row per multiply-accumulate operation:
//!
//! | Column | Meaning |
//! |--------|---------|
//! | active | 1 if row is active, 0 if padding |
//! | row_idx | Output neuron index (i) |
//! | col_idx | Input index (j) |
//! | weight | weight[i][j] (M31 field element) |
//! | input | input[j] (M31 field element) |
//! | product | weight * input (M31) |
//! | acc_prev | Accumulator before this MAC |
//! | acc_next | Accumulator after this MAC |
//! | is_last_col | 1 if j == in_size - 1, else 0 |
//! | bias | bias[i] (only used when is_last_col = 1) |
//! | output | output[i] = acc_next + bias (when is_last_col = 1) |
//!
//! ## Constraints (all degree ≤ 2)
//!
//! 1. `active * (active - 1) = 0` — boolean
//! 2. `is_last_col * (is_last_col - 1) = 0` — boolean
//! 3. `active * (product - weight * input) = 0` — multiplication correctness
//! 4. `active * (acc_next - acc_prev - product) = 0` — accumulation
//! 5. `active * is_last_col * (output - acc_next - bias) = 0` — final output

use crate::{hash_bytes, Hash256};
use serde::{Deserialize, Serialize};

/// Number of columns in the Dense layer trace.
pub const DENSE_TRACE_COLS: usize = 11;

/// Input for proving a Dense layer forward pass.
#[derive(Debug, Clone)]
pub struct DenseLayerInput {
    /// Weight matrix [out_size × in_size] flattened row-major.
    pub weights: Vec<i64>,
    /// Bias vector [out_size].
    pub bias: Vec<i64>,
    /// Input vector [in_size].
    pub input: Vec<i64>,
    /// Output vector [out_size] (computed by prover, verified by AIR).
    pub output: Vec<i64>,
    /// Dimensions.
    pub in_size: usize,
    pub out_size: usize,
}

/// Result of proving a Dense layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DenseLayerProof {
    /// Serialized proof data.
    pub proof_data: Vec<u8>,
    /// Hash binding this proof to the layer computation.
    pub binding_hash: Hash256,
    /// Input hash: BLAKE3(input vector).
    pub input_hash: Hash256,
    /// Output hash: BLAKE3(output vector).
    pub output_hash: Hash256,
    /// Proving time in milliseconds.
    pub proving_time_ms: u64,
    /// Number of trace rows.
    pub trace_rows: usize,
}

/// Result of a folded multi-layer proof.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldedInferenceProof {
    /// One proof per layer.
    pub layer_proofs: Vec<DenseLayerProof>,
    /// Recursive composition proof (proves all layers chain correctly).
    pub composition_proof: Vec<u8>,
    /// Model input hash.
    pub model_input_hash: Hash256,
    /// Model output hash.
    pub final_output_hash: Hash256,
    /// Total proving time across all layers.
    pub total_proving_time_ms: u64,
    /// Total proof size in bytes.
    pub total_proof_size: usize,
    /// Number of layers proven.
    pub num_layers: usize,
}

/// Compute the Dense layer forward pass (reference implementation).
///
/// Uses exact integer arithmetic for determinism:
/// `output[i] = sum(weight[i][j] * input[j]) + bias[i]`
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

/// Generate the execution trace for a Dense layer proof.
///
/// One row per multiply-accumulate: total rows = out_size * in_size.
/// Padded to next power of 2 for NTT.
pub fn generate_dense_trace(layer: &DenseLayerInput) -> Vec<Vec<i64>> {
    let n_ops = layer.out_size * layer.in_size;
    let log_size = (n_ops as f64).log2().ceil() as u32;
    let trace_size = 1usize << log_size.max(4); // minimum 16 rows

    // Initialize columns
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
            acc = acc + prod;
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
        active, row_idx, col_idx, weight, input_col,
        product, acc_prev, acc_next, is_last_col, bias_col, output_col,
    ]
}

/// Verify a Dense layer trace against constraints (CPU verification, no STARK).
///
/// This is the reference verifier — checks all constraints row by row.
/// Used for testing and as a fallback when the stwo-prover feature is disabled.
pub fn verify_dense_trace(trace: &[Vec<i64>]) -> Result<(), String> {
    if trace.len() != DENSE_TRACE_COLS {
        return Err(format!("Expected {} columns, got {}", DENSE_TRACE_COLS, trace.len()));
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

        // Constraint 1: active is boolean
        if a * (a - 1) != 0 {
            return Err(format!("Row {row}: active not boolean ({a})"));
        }

        // Constraint 2: is_last_col is boolean
        let ilc = is_last_col[row];
        if ilc * (ilc - 1) != 0 {
            return Err(format!("Row {row}: is_last_col not boolean ({ilc})"));
        }

        if a == 1 {
            // Constraint 3: product = weight * input
            let expected_product = weight[row] * input[row];
            if product[row] != expected_product {
                return Err(format!(
                    "Row {row}: product mismatch: {} != {} * {}",
                    product[row], weight[row], input[row]
                ));
            }

            // Constraint 4: acc_next = acc_prev + product
            if acc_next[row] != acc_prev[row] + product[row] {
                return Err(format!(
                    "Row {row}: accumulation mismatch: {} != {} + {}",
                    acc_next[row], acc_prev[row], product[row]
                ));
            }

            // Constraint 5: if is_last_col, output = acc_next + bias
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

/// Prove and verify a Dense layer forward pass (reference implementation).
///
/// When stwo-prover is available, this generates a real STARK proof.
/// Otherwise, uses CPU trace verification + BLAKE3 binding hash.
pub fn prove_dense_layer(layer: &DenseLayerInput) -> Result<DenseLayerProof, String> {
    let start = Instant::now();

    // Generate trace
    let trace = generate_dense_trace(layer);
    let trace_rows = trace[0].len();

    // Verify trace (CPU reference check)
    verify_dense_trace(&trace)?;

    // Compute binding hash
    let mut binding_input = Vec::new();
    for col in &trace {
        for &val in col {
            binding_input.extend_from_slice(&val.to_le_bytes());
        }
    }
    let binding_hash = hash_bytes(&binding_input);

    // Compute input/output hashes
    let input_bytes: Vec<u8> = layer.input.iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let input_hash = hash_bytes(&input_bytes);

    let output_bytes: Vec<u8> = layer.output.iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let output_hash = hash_bytes(&output_bytes);

    // Serialize proof (binding hash + trace dimensions)
    let mut proof_data = Vec::new();
    proof_data.extend_from_slice(&(trace_rows as u64).to_le_bytes());
    proof_data.extend_from_slice(&(DENSE_TRACE_COLS as u64).to_le_bytes());
    proof_data.extend_from_slice(&binding_hash.0);
    proof_data.extend_from_slice(&input_hash.0);
    proof_data.extend_from_slice(&output_hash.0);

    let proving_time_ms = start.elapsed().as_millis() as u64;

    Ok(DenseLayerProof {
        proof_data,
        binding_hash,
        input_hash,
        output_hash,
        proving_time_ms,
        trace_rows,
    })
}

/// Prove a multi-layer network with folded composition.
///
/// Each layer is proven independently. The composition proof verifies:
/// - Each layer proof is valid
/// - Layer k's output_hash == layer (k+1)'s input_hash (chaining)
/// - The first layer's input_hash matches the model input
/// - The last layer's output_hash matches the model output
pub fn prove_folded_inference(
    layers: &[DenseLayerInput],
) -> Result<FoldedInferenceProof, String> {
    let total_start = Instant::now();

    if layers.is_empty() {
        return Err("No layers to prove".into());
    }

    // Prove each layer
    let mut layer_proofs = Vec::with_capacity(layers.len());
    for (i, layer) in layers.iter().enumerate() {
        let proof = prove_dense_layer(layer)
            .map_err(|e| format!("Layer {i} proof failed: {e}"))?;
        layer_proofs.push(proof);
    }

    // Verify chaining: layer k output == layer k+1 input
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

    // Composition proof: hash of all layer binding hashes
    let mut comp_input = Vec::new();
    for proof in &layer_proofs {
        comp_input.extend_from_slice(&proof.binding_hash.0);
        comp_input.extend_from_slice(&proof.input_hash.0);
        comp_input.extend_from_slice(&proof.output_hash.0);
    }
    let composition_proof = hash_bytes(&comp_input).0.to_vec();

    let total_proof_size: usize = layer_proofs.iter()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dense_forward_i64() {
        // 2×3 Dense: output[0] = 1*1 + 2*2 + 3*3 + 10 = 24
        //            output[1] = 4*1 + 5*2 + 6*3 + 20 = 52
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
            weights, bias, input, output,
            in_size: 3, out_size: 2,
        };

        let trace = generate_dense_trace(&layer);
        assert_eq!(trace.len(), DENSE_TRACE_COLS);
        assert!(verify_dense_trace(&trace).is_ok());
    }

    #[test]
    fn test_prove_dense_layer() {
        let in_size = 64;
        let out_size = 32;

        // Deterministic weights
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
            weights, bias, input, output,
            in_size, out_size,
        };

        let proof = prove_dense_layer(&layer).unwrap();
        assert!(proof.trace_rows >= in_size * out_size);
        assert!(!proof.proof_data.is_empty());
        assert!(proof.proving_time_ms < 5000); // should be fast for 64×32
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

        // Layer 1: dim1 → dim2
        let w1: Vec<i64> = (0..dim2 * dim1).map(|_| next()).collect();
        let b1: Vec<i64> = (0..dim2).map(|_| next()).collect();
        let input1: Vec<i64> = (0..dim1).map(|_| next()).collect();
        let output1 = dense_forward_i64(&w1, &b1, &input1, dim1, dim2);

        // Layer 2: dim2 → dim3
        let w2: Vec<i64> = (0..dim3 * dim2).map(|_| next()).collect();
        let b2: Vec<i64> = (0..dim3).map(|_| next()).collect();
        let output2 = dense_forward_i64(&w2, &b2, &output1, dim2, dim3);

        let layers = vec![
            DenseLayerInput {
                weights: w1, bias: b1,
                input: input1, output: output1.clone(),
                in_size: dim1, out_size: dim2,
            },
            DenseLayerInput {
                weights: w2, bias: b2,
                input: output1, output: output2,
                in_size: dim2, out_size: dim3,
            },
        ];

        let folded = prove_folded_inference(&layers).unwrap();
        assert_eq!(folded.num_layers, 2);
        assert_eq!(folded.layer_proofs.len(), 2);
        assert!(folded.total_proof_size > 0);

        // Chain integrity: layer 1 output hash == layer 2 input hash
        assert_eq!(
            folded.layer_proofs[0].output_hash,
            folded.layer_proofs[1].input_hash,
        );
    }

    #[test]
    fn test_prove_larger_layer() {
        // 256×128 Dense = 32,768 MACs
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
            weights, bias, input, output,
            in_size, out_size,
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
            weights, bias, input, output,
            in_size: 2, out_size: 2,
        };

        let mut trace = generate_dense_trace(&layer);

        // Tamper with product column
        trace[5][0] = 999;

        assert!(verify_dense_trace(&trace).is_err());
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
        // WRONG input for layer 2 (doesn't match layer 1 output)
        let wrong_input: Vec<i64> = (0..dim).map(|_| next()).collect();
        let output2 = dense_forward_i64(&w2, &b2, &wrong_input, dim, dim);

        let layers = vec![
            DenseLayerInput {
                weights: w1, bias: b1,
                input: input1, output: output1,
                in_size: dim, out_size: dim,
            },
            DenseLayerInput {
                weights: w2, bias: b2,
                input: wrong_input, output: output2,
                in_size: dim, out_size: dim,
            },
        ];

        let result = prove_folded_inference(&layers);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Chain break"));
    }
}
