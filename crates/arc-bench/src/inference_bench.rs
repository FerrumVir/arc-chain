//! ARC Chain — Inference Benchmark for Paper Evidence
//!
//! Loads real neural network models, executes inference through the
//! InferenceEngine, and measures:
//! - Forward pass latency per model size
//! - Determinism verification (100 runs, bitwise identical)
//! - Attestation throughput (InferenceAttestation TX processing)
//! - On-chain recording via StateDB
//!
//! Outputs JSON to stdout for the paper benchmark suite.
//!
//! Usage:
//!   cargo run --release --bin arc-bench-inference
//!   cargo run --release --bin arc-bench-inference -- --model /tmp/arc-models/mlp-1024x6.arc

use arc_crypto::{hash_bytes, Hash256};
use arc_state::StateDB;
use arc_types::*;
use arc_vm::inference::{
    InferenceConfig, InferenceEngine, InferenceInput, InferenceParams, InferenceRequest,
};
use std::time::Instant;

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model_path = args.iter().position(|a| a == "--model").map(|i| args[i + 1].clone());

    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("ARC Chain — Inference Benchmark");
    eprintln!("═══════════════════════════════════════════════════════════");

    let mut results = serde_json::json!({
        "tier": 1,
        "benchmark": "OnChainInference",
        "models": [],
        "attestation_throughput": {},
        "determinism_verified": true,
    });

    // ── Benchmark 1: Built-in models (various sizes) ────────────────────

    let model_configs = vec![
        ("4-layer MLP (1K params)", 32, 16, 4),
        ("6-layer MLP (50K params)", 128, 64, 6),
        ("8-layer MLP (500K params)", 256, 128, 8),
        ("12-layer MLP (2M params)", 512, 256, 12),
    ];

    for (name, hidden, input_dim, depth) in &model_configs {
        let model = build_model(*hidden, *input_dim, *depth);
        let model_bytes = model.to_bytes();
        let model_id = hash_bytes(&model_bytes);
        let param_count = count_params(&model);

        // Load into engine
        let mut engine = InferenceEngine::new(InferenceConfig {
            max_loaded_models: 16,
            default_timeout_ms: 5_000,
            max_tokens: 256,
            temperature: 0.0, // deterministic
        });

        let info = arc_vm::inference::ModelInfo {
            name: name.to_string(),
            model_type: "mlp".to_string(),
            parameter_count: param_count as u64,
            quantization: "f32".to_string(),
            max_context: 256,
        };

        engine
            .load_model_with_weights(model_id.0, info, &model_bytes)
            .expect("Failed to load model");

        // Warm up
        let input = random_input(*input_dim);
        let request = make_request(model_id.0, &input);
        let _ = engine.run_inference(&request);

        // Benchmark: 100 inference calls
        let mut latencies = Vec::with_capacity(100);
        let mut first_output = None;
        let mut all_deterministic = true;

        for i in 0..100 {
            let start = Instant::now();
            let result = engine.run_inference(&request).expect("Inference failed");
            let elapsed_us = start.elapsed().as_micros() as u64;
            latencies.push(elapsed_us);

            // Check determinism
            let output_hash = hash_bytes(format!("{:?}", result.output).as_bytes());
            if i == 0 {
                first_output = Some(output_hash);
            } else if Some(output_hash) != first_output {
                all_deterministic = false;
                eprintln!("  DETERMINISM VIOLATION at iteration {i}!");
            }
        }

        let avg_us = latencies.iter().sum::<u64>() / latencies.len() as u64;
        let p50_us = {
            let mut sorted = latencies.clone();
            sorted.sort();
            sorted[sorted.len() / 2]
        };
        let p99_us = {
            let mut sorted = latencies.clone();
            sorted.sort();
            sorted[(sorted.len() as f64 * 0.99) as usize]
        };

        eprintln!(
            "  {}: {} params, avg={:.2}ms, p50={:.2}ms, p99={:.2}ms, deterministic={}",
            name,
            param_count,
            avg_us as f64 / 1000.0,
            p50_us as f64 / 1000.0,
            p99_us as f64 / 1000.0,
            all_deterministic
        );

        results["models"].as_array_mut().unwrap().push(serde_json::json!({
            "name": name,
            "params": param_count,
            "input_dim": input_dim,
            "hidden_dim": hidden,
            "depth": depth,
            "runs": 100,
            "avg_us": avg_us,
            "p50_us": p50_us,
            "p99_us": p99_us,
            "forward_ms": avg_us as f64 / 1000.0,
            "deterministic": all_deterministic,
            "model_id": hex_encode(&model_id.0[..8]),
        }));

        if !all_deterministic {
            results["determinism_verified"] = serde_json::json!(false);
        }
    }

    // ── Benchmark 2: External model file (if provided) ──────────────────

    if let Some(path) = model_path {
        eprintln!("\n  Loading external model: {}", path);
        match std::fs::read(&path) {
            Ok(data) => {
                let model_id = hash_bytes(&data);
                let mut engine = InferenceEngine::new(InferenceConfig {
                    max_loaded_models: 16,
                    default_timeout_ms: 10_000,
                    max_tokens: 256,
                    temperature: 0.0,
                });

                let info = arc_vm::inference::ModelInfo {
                    name: path.clone(),
                    model_type: "external".to_string(),
                    parameter_count: 0,
                    quantization: "unknown".to_string(),
                    max_context: 256,
                };

                match engine.load_model_with_weights(model_id.0, info, &data) {
                    Ok(_) => {
                        let request = make_text_request(model_id.0, "What is 2+2?");
                        let start = Instant::now();
                        match engine.run_inference(&request) {
                            Ok(result) => {
                                let elapsed_ms = start.elapsed().as_millis();
                                eprintln!("  External model: {}ms, output_len={}", elapsed_ms, format!("{:?}", result.output).len());
                                results["external_model"] = serde_json::json!({
                                    "path": path,
                                    "model_id": hex_encode(&model_id.0[..8]),
                                    "file_size_bytes": data.len(),
                                    "inference_ms": elapsed_ms,
                                });
                            }
                            Err(e) => eprintln!("  External model inference failed: {e}"),
                        }
                    }
                    Err(e) => eprintln!("  Failed to load external model: {e}"),
                }
            }
            Err(e) => eprintln!("  Failed to read model file: {e}"),
        }
    }

    // ── Benchmark 3: Attestation TX throughput ──────────────────────────

    eprintln!("\n  Benchmarking InferenceAttestation TX throughput...");
    {
        let state = StateDB::with_genesis(&[
            (hash_bytes(b"attester"), 10_000_000),
        ]);
        let attester = hash_bytes(b"attester");

        let mut attestation_txs = Vec::new();
        for i in 0..1000u64 {
            let model_id = hash_bytes(&i.to_le_bytes());
            let input_hash = hash_bytes(format!("input-{i}").as_bytes());
            let output_hash = hash_bytes(format!("output-{i}").as_bytes());

            let tx = Transaction {
                tx_type: TxType::InferenceAttestation,
                from: attester,
                nonce: i,
                body: TxBody::InferenceAttestation(transaction::InferenceAttestationBody {
                    model_id,
                    input_hash,
                    output_hash,
                    challenge_period: 100,
                    bond: 100,
                }),
                fee: 0,
                gas_limit: 0,
                hash: Hash256::ZERO,
                signature: arc_crypto::Signature::null(),
                sig_verified: false,
            };
            attestation_txs.push(tx);
        }

        // Recompute hashes
        for tx in &mut attestation_txs {
            tx.hash = tx.compute_hash();
        }

        let start = Instant::now();
        let (block, receipts) = state.execute_block(&attestation_txs, hash_bytes(b"producer")).unwrap();
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let success_count = receipts.iter().filter(|r| r.success).count();
        let tps = if elapsed_ms > 0 { 1000 * 1000 / elapsed_ms } else { 0 };

        eprintln!(
            "  1000 attestations: {}ms, {}/{} success, ~{} attestations/sec",
            elapsed_ms, success_count, 1000, tps
        );

        results["attestation_throughput"] = serde_json::json!({
            "count": 1000,
            "elapsed_ms": elapsed_ms,
            "success": success_count,
            "tps": tps,
            "block_height": block.header.height,
        });
    }

    // ── Output JSON ─────────────────────────────────────────────────────

    println!("{}", serde_json::to_string_pretty(&results).unwrap());
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_model(hidden: usize, input_dim: usize, depth: usize) -> arc_vm::inference::NeuralNet {
    use arc_vm::inference::{Layer, NeuralNet};

    let mut layers = Vec::new();
    let mut rng_state: u64 = 42;

    // Simple LCG for deterministic "random" weights
    let mut next_f32 = || -> f32 {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((rng_state >> 33) as f32 / u32::MAX as f32 - 0.5) * 0.1
    };

    // Input layer
    let weights: Vec<Vec<f32>> = (0..hidden)
        .map(|_| (0..input_dim).map(|_| next_f32()).collect())
        .collect();
    let bias: Vec<f32> = (0..hidden).map(|_| 0.0).collect();
    layers.push(Layer::Dense { weights, bias });
    layers.push(Layer::ReLU);

    // Hidden layers
    for _ in 0..depth - 2 {
        let weights: Vec<Vec<f32>> = (0..hidden)
            .map(|_| (0..hidden).map(|_| next_f32()).collect())
            .collect();
        let bias: Vec<f32> = (0..hidden).map(|_| 0.0).collect();
        layers.push(Layer::Dense { weights, bias });
        layers.push(Layer::ReLU);
    }

    // Output layer
    let output_dim = 10; // classification
    let weights: Vec<Vec<f32>> = (0..output_dim)
        .map(|_| (0..hidden).map(|_| next_f32()).collect())
        .collect();
    let bias: Vec<f32> = (0..output_dim).map(|_| 0.0).collect();
    layers.push(Layer::Dense { weights, bias });
    layers.push(Layer::Softmax);

    NeuralNet {
        layers,
        input_size: input_dim,
        output_size: output_dim,
    }
}

fn count_params(model: &arc_vm::inference::NeuralNet) -> usize {
    use arc_vm::inference::Layer;
    model.layers.iter().map(|l| match l {
        Layer::Dense { weights, bias } => {
            weights.len() * weights.first().map(|r| r.len()).unwrap_or(0) + bias.len()
        }
        Layer::LayerNorm { gamma, beta, .. } => gamma.len() + beta.len(),
        Layer::Embedding { table } => table.len() * table.first().map(|r| r.len()).unwrap_or(0),
        _ => 0,
    }).sum()
}

fn random_input(dim: usize) -> Vec<f32> {
    let mut rng_state: u64 = 123;
    (0..dim).map(|_| {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (rng_state >> 33) as f32 / u32::MAX as f32
    }).collect()
}

fn make_request(model_id: [u8; 32], input: &[f32]) -> InferenceRequest {
    InferenceRequest {
        model_id,
        input: InferenceInput::Embedding(input.to_vec()),
        params: InferenceParams {
            max_tokens: 10,
            temperature: 0.0,
            top_p: 1.0,
            stop_sequences: vec![],
        },
    }
}

fn make_text_request(model_id: [u8; 32], text: &str) -> InferenceRequest {
    InferenceRequest {
        model_id,
        input: InferenceInput::Text(text.to_string()),
        params: InferenceParams {
            max_tokens: 50,
            temperature: 0.0,
            top_p: 1.0,
            stop_sequences: vec![],
        },
    }
}
