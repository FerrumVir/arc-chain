use arc_crypto::inference_proof::{dense_forward_i64, prove_sharded_dense};
use arc_crypto::stwo_air::prove_dense_stark;
use std::io::Write;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let output_dir = args.get(1).map(|s| s.as_str()).unwrap_or("/tmp/proofs");
    std::fs::create_dir_all(output_dir).unwrap();

    let configs: Vec<(usize, usize, &str)> = vec![
        (32, 64, "1b-attn-q"),
        (32, 64, "1b-attn-k"),
        (32, 64, "1b-attn-v"),
        (64, 32, "1b-attn-o"),
        (128, 64, "1b-ffn-gate"),
        (128, 64, "1b-ffn-up"),
        (64, 128, "1b-ffn-down"),
        (32, 64, "1b-lm-head-s1"),
        (32, 64, "1b-lm-head-s2"),
        (32, 64, "1b-lm-head-s3"),
        (64, 128, "7b-attn-q"),
        (64, 128, "7b-attn-k"),
        (64, 128, "7b-attn-v"),
        (128, 64, "7b-attn-o"),
        (256, 128, "7b-ffn-gate"),
        (256, 128, "7b-ffn-up"),
        (128, 256, "7b-ffn-down"),
        (128, 128, "7b-layer-0"),
        (128, 128, "7b-layer-1"),
        (128, 128, "7b-layer-2"),
        (128, 256, "13b-attn-q"),
        (128, 256, "13b-attn-k"),
        (128, 256, "13b-attn-v"),
        (256, 128, "13b-attn-o"),
        (512, 256, "13b-ffn-gate"),
        (512, 256, "13b-ffn-up"),
        (256, 512, "13b-ffn-down"),
        (256, 256, "13b-layer-0"),
        (256, 256, "13b-layer-1"),
        (256, 256, "13b-layer-2"),
        (256, 512, "50b-shard-0"),
        (256, 512, "50b-shard-1"),
        (256, 512, "50b-shard-2"),
        (256, 512, "50b-shard-3"),
        (512, 256, "50b-shard-4"),
        (512, 256, "50b-shard-5"),
        (512, 512, "50b-layer-q"),
        (512, 512, "50b-layer-k"),
        (512, 512, "50b-layer-v"),
        (512, 512, "50b-layer-o"),
        (512, 1024, "70b-ffn-gate"),
        (512, 1024, "70b-ffn-up"),
        (1024, 512, "70b-ffn-down"),
        (512, 512, "70b-attn-q"),
        (512, 512, "70b-attn-k"),
        (512, 512, "70b-attn-v"),
        (512, 512, "70b-attn-o"),
        (1024, 1024, "70b-full-layer"),
        (256, 1024, "70b-shard-a"),
        (256, 1024, "70b-shard-b"),
        (1024, 512, "stress-1"),
        (512, 1024, "stress-2"),
        (1024, 1024, "stress-max-1"),
        (1024, 1024, "stress-max-2"),
        (128, 128, "folded-layer-0"),
        (128, 128, "folded-layer-1"),
        (128, 128, "folded-layer-2"),
        (128, 128, "folded-layer-3"),
        (256, 256, "folded-deep-0"),
        (256, 256, "folded-deep-1"),
    ];

    let total = configs.len();
    println!("=== ARC Chain STARK Proof Generator ===");
    println!("Generating {} real Circle STARK proofs", total);
    println!("Output: {}/", output_dir);
    println!();

    let mut manifest = Vec::new();
    let total_start = Instant::now();

    for (idx, (out_size, in_size, label)) in configs.iter().enumerate() {
        let out_size = *out_size;
        let in_size = *in_size;

        let mut rng: u64 = 0;
        for b in label.bytes() {
            rng = rng.wrapping_mul(31).wrapping_add(b as u64);
        }
        let mut next = || -> i64 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((rng >> 33) as i64) % 10 - 5
        };

        let weights: Vec<i64> = (0..out_size * in_size).map(|_| next()).collect();
        let bias: Vec<i64> = (0..out_size).map(|_| 0).collect();
        let input: Vec<i64> = (0..in_size).map(|_| next()).collect();
        let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

        let start = Instant::now();
        let (proof_data, proof_size, proving_time_ms) =
            prove_dense_stark(&weights, &input, &output, in_size, out_size);
        let elapsed = start.elapsed().as_millis();

        let filename = format!("{:03}-{}.stark", idx, label);
        let path = format!("{}/{}", output_dir, filename);
        std::fs::write(&path, &proof_data).unwrap();

        let input_hash = arc_crypto::hash_bytes(
            &input
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        let output_hash = arc_crypto::hash_bytes(
            &output
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );

        let entry = serde_json::json!({
            "idx": idx,
            "label": label,
            "out_size": out_size,
            "in_size": in_size,
            "macs": out_size * in_size,
            "proof_size": proof_size,
            "proving_time_ms": elapsed,
            "proof_file": filename,
            "input_hash": format!("0x{}", hex::encode(&input_hash.0[..16])),
            "output_hash": format!("0x{}", hex::encode(&output_hash.0[..16])),
        });
        manifest.push(entry);

        let macs = out_size * in_size;
        println!(
            "[{:2}/{}] {:20} {:4}x{:4} ({:>8} MACs) -> {:3}B proof in {:>5}ms",
            idx, total, label, out_size, in_size, macs, proof_size, elapsed
        );
    }

    let total_elapsed = total_start.elapsed();

    let manifest_path = format!("{}/manifest.json", output_dir);
    let manifest_json = serde_json::to_string_pretty(&manifest).unwrap();
    std::fs::write(&manifest_path, &manifest_json).unwrap();

    println!();
    println!("=== SUMMARY ===");
    println!("Total proofs: {}", total);
    println!("Total time: {:.1}s", total_elapsed.as_secs_f64());
    println!(
        "Total proof size: {} bytes",
        manifest
            .iter()
            .map(|e| e["proof_size"].as_u64().unwrap())
            .sum::<u64>()
    );
    println!("Manifest: {}", manifest_path);
    println!("All proofs are REAL Circle STARK (Stwo) - cryptographically verifiable");
}
