//! Prepare <=1 GiB row bundles without loading the full model. Existing
//! tensor_row_stdio_worker consumes the identical ARCROW01 files. Source
//! reads retain the pinned artifact/profile and check verified chunk hashes.
//!
//! --model GGUF --artifact HASH --worker-id ID --layers 0,1,2,3
//! --output-dir NEW_ABSOLUTE_DIRECTORY [--include-output] [--row-partition RANK/COUNT]
//! Partial bundles are for the offline proof; production layer residency
//! configuration cannot describe them yet. Ranks are zero-based.
//!
//! Peak preparation is bounded by two copies of the largest selected matrix
//! plus norms/metadata/verified-read scratch. Worker startup retains <=1 GiB
//! total rows and can transiently use one additional file-sized conversion
//! buffer. Neither preparation nor serving first constructs a full model.

use arc_crypto::Hash256;
use arc_inference::low_residency::{CanonicalRowSource, load_low_residency_model};
use arc_inference::tensor_parallel::{MAX_CANONICAL_ROW_FILE_BYTES, RowAssignment, TensorKey};
use std::path::PathBuf;

#[path = "support/row_partition.rs"]
mod row_partition;
use row_partition::RowPartition;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args)
}

fn run(args: &[String]) -> Result<(), String> {
    let mut values = std::collections::BTreeMap::new();
    let mut include_output = false;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if flag == "--include-output" {
            if include_output {
                return Err("duplicate --include-output".into());
            }
            include_output = true;
            i += 1;
            continue;
        }
        if ![
            "--model",
            "--artifact",
            "--worker-id",
            "--layers",
            "--output-dir",
            "--row-partition",
        ]
        .contains(&flag)
        {
            return Err(format!("unknown flag {flag}"));
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("missing {flag} value"))?;
        if values.insert(flag, value.as_str()).is_some() {
            return Err(format!("duplicate {flag}"));
        }
        i += 2;
    }
    let get = |flag| {
        values
            .get(flag)
            .copied()
            .ok_or_else(|| format!("required {flag}"))
    };
    let artifact = Hash256::from_hex(get("--artifact")?.trim_start_matches("0x"))
        .map_err(|e| e.to_string())?;
    let partition = values
        .get("--row-partition")
        .map(|value| RowPartition::parse(value))
        .transpose()?;
    let id = get("--worker-id")?;
    if id.is_empty() || id.len() > 64 || id.chars().any(char::is_whitespace) {
        return Err("invalid worker id".into());
    }
    let destination = PathBuf::from(get("--output-dir")?);
    if !destination.is_absolute() || destination.exists() {
        return Err("output must be a new absolute directory".into());
    }
    let model = load_low_residency_model(std::path::Path::new(get("--model")?), artifact)
        .map_err(|e| e.to_string())?;
    let mut layers = std::collections::BTreeSet::new();
    for layer in get("--layers")?.split(',') {
        let layer = layer.parse::<usize>().map_err(|e| e.to_string())?;
        if layer >= model.config().n_layers || !layers.insert(layer) {
            return Err("invalid or duplicate layer".into());
        }
    }
    let tensors = [
        TensorKey::Wq,
        TensorKey::Wk,
        TensorKey::Wv,
        TensorKey::Wo,
        TensorKey::WGate,
        TensorKey::WUp,
        TensorKey::WDown,
    ];
    let mut keys: Vec<_> = layers
        .iter()
        .flat_map(|&l| tensors.into_iter().map(move |t| (Some(l), t)))
        .collect();
    if include_output {
        keys.push((None, TensorKey::LmHead));
    }
    let profile = model
        .canonical_execution_profile()
        .ok_or("canonical profile missing")?;
    let mut planned = Vec::new();
    let mut total = 0usize;
    let mut largest = 0usize;
    for (layer, tensor) in keys {
        let (rows, cols) = model
            .projection_shape(layer, tensor)
            .ok_or("projection missing")?;
        let (row_start, row_end) = partition
            .unwrap_or(RowPartition { rank: 0, count: 1 })
            .range(rows)?;
        let bytes = (row_end - row_start)
            .checked_mul(cols + 8)
            .and_then(|n| n.checked_add(85 + profile.len() + id.len()))
            .ok_or("size overflow")?;
        total = total.checked_add(bytes).ok_or("size overflow")?;
        if total > MAX_CANONICAL_ROW_FILE_BYTES {
            return Err("selected rows exceed the worker's 1 GiB aggregate startup bound; select fewer layers or more row partitions".into());
        }
        largest = largest.max(bytes);
        let assignment = RowAssignment {
            artifact_id: artifact,
            execution_profile: profile.into(),
            layer,
            tensor,
            row_start,
            row_end,
            worker_id: id.into(),
        };
        planned.push((assignment, bytes));
    }
    let parent = destination.parent().ok_or("output has no parent")?;
    let staging = parent.join(format!(
        ".arc-row-staging-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    ));
    std::fs::create_dir(&staging).map_err(|e| e.to_string())?;
    let result = (|| -> Result<(), String> {
        let row_dir = staging.join("rows");
        std::fs::create_dir(&row_dir).map_err(|e| e.to_string())?;
        let mut files = Vec::new();
        for (assignment, size) in &planned {
            let name = format!(
                "{}-{:?}.arcrow",
                assignment
                    .layer
                    .map(|l| format!("layer-{l:05}"))
                    .unwrap_or_else(|| "output".into()),
                assignment.tensor
            );
            let path = row_dir.join(&name);
            model
                .export_rows(&path, assignment)
                .map_err(|e| e.to_string())?;
            if path.metadata().map_err(|e| e.to_string())?.len() != *size as u64 {
                return Err("exported file size disagrees with preflight".into());
            }
            // Hash in bounded chunks, never read a whole row file for its digest.
            let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            let mut hasher = blake3::Hasher::new();
            let mut buffer = vec![0; 65536];
            loop {
                let n = std::io::Read::read(&mut file, &mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
            files.push(serde_json::json!({"file": name, "assignment": assignment, "bytes": size, "blake3": hasher.finalize().to_hex().to_string()}));
        }
        let ranges: Vec<_> = layers.iter().map(|&l| [l, l + 1]).collect();
        let mut manifest = serde_json::json!({
            "format": "arc.tensor-row-low-residency-bundle.v1", "artifact_blake3": artifact.to_hex(),
            "execution_profile": profile, "worker_id": id, "serialized_row_bytes": total,
            "largest_row_file_bytes": largest,
            "preparation_matrix_peak_upper_bound_bytes": largest.saturating_mul(2),
            "worker_startup_payload_peak_upper_bound_bytes": total.saturating_add(largest),
            "files": files,
            "capacity_note": "Payload bounds exclude process/allocator/OS/node memory. Measure aggregate host peak and latency before activation."
        });
        if let Some(partition) = partition {
            // Never advertise partial rows as complete production layers.
            manifest["format"] = serde_json::json!("arc.tensor-row-offline-partition-bundle.v1");
            manifest["row_partition"] =
                serde_json::json!({"rank": partition.rank, "count": partition.count});
            manifest["scope"] = serde_json::json!(
                "offline partial-row conformance only; not production cohort configuration"
            );
        } else {
            manifest["resident_layers"] = serde_json::json!(ranges);
            manifest["resident_output"] = serde_json::json!(include_output);
        }
        std::fs::write(
            staging.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if destination.exists() {
            return Err("output directory appeared before publication".into());
        }
        std::fs::rename(&staging, &destination).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result?;
    eprintln!(
        "exported {total} row-file bytes to {}",
        destination.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_inference::cached_integer_model::load_cached_model_canonical_i8_interleaved_rope;
    use arc_inference::tensor_parallel::export_verified_model_rows;
    use candle_core::{
        Device, Tensor,
        quantized::{GgmlDType, QTensor, gguf_file},
    };

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "arc-partial-export-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // A generated, sub-megabyte GGUF with GQA and Q/K permutation. No model
    // download or production artifact is involved in these exporter tests.
    fn tiny_gguf(path: &std::path::Path) {
        use gguf_file::Value;
        let metadata = [
            ("general.architecture", Value::String("llama".into())),
            ("llama.block_count", Value::U32(1)),
            ("llama.embedding_length", Value::U32(32)),
            ("llama.attention.head_count", Value::U32(4)),
            ("llama.attention.head_count_kv", Value::U32(2)),
            ("llama.feed_forward_length", Value::U32(64)),
            ("tokenizer.ggml.bos_token_id", Value::U32(1)),
            ("tokenizer.ggml.eos_token_id", Value::U32(2)),
        ];
        let mut tensors = Vec::new();
        let mut add = |name: &str, shape: Vec<usize>| {
            let values = (0..shape.iter().product::<usize>())
                .map(|i| {
                    if shape.len() == 1 {
                        1.0
                    } else {
                        ((i * 13 % 97) as f32 - 48.0) / 64.0
                    }
                })
                .collect::<Vec<_>>();
            let tensor = Tensor::from_vec(values, shape.as_slice(), &Device::Cpu).unwrap();
            tensors.push((
                name.to_string(),
                QTensor::quantize(&tensor, GgmlDType::F32).unwrap(),
            ));
        };
        add("token_embd.weight", vec![17, 32]);
        add("output.weight", vec![17, 32]);
        add("output_norm.weight", vec![32]);
        for (name, rows, cols) in [
            ("attn_q", 32, 32),
            ("attn_k", 16, 32),
            ("attn_v", 16, 32),
            ("attn_output", 32, 32),
            ("ffn_gate", 64, 32),
            ("ffn_up", 64, 32),
            ("ffn_down", 32, 64),
        ] {
            add(&format!("blk.0.{name}.weight"), vec![rows, cols]);
        }
        add("blk.0.attn_norm.weight", vec![32]);
        add("blk.0.ffn_norm.weight", vec![32]);
        let mut file = std::fs::File::create(path).unwrap();
        let metadata = metadata.iter().map(|(k, v)| (*k, v)).collect::<Vec<_>>();
        let tensors = tensors
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect::<Vec<_>>();
        gguf_file::write(&mut file, &metadata, &tensors).unwrap();
    }

    #[test]
    fn partial_exports_equal_resident_canonical_files_and_refuse_before_writes() {
        let directory = TestDirectory::new();
        let model_path = directory.path().join("tiny.gguf");
        tiny_gguf(&model_path);
        let artifact = arc_crypto::hash_bytes(&std::fs::read(&model_path).unwrap());
        let full =
            load_cached_model_canonical_i8_interleaved_rope(model_path.to_str().unwrap()).unwrap();
        let arguments = |output: &std::path::Path, partition: Option<String>| {
            let mut args = vec![
                "--model".into(),
                model_path.to_str().unwrap().into(),
                "--artifact".into(),
                artifact.to_hex(),
                "--worker-id".into(),
                "test-worker".into(),
                "--layers".into(),
                "0".into(),
                "--include-output".into(),
                "--output-dir".into(),
                output.to_str().unwrap().into(),
            ];
            if let Some(partition) = partition {
                args.extend(["--row-partition".into(), partition]);
            }
            args
        };
        for rank in 0..3 {
            let output = directory.path().join(format!("rank-{rank}"));
            run(&arguments(&output, Some(format!("{rank}/3")))).unwrap();
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(output.join("manifest.json")).unwrap())
                    .unwrap();
            assert_eq!(
                manifest["format"],
                "arc.tensor-row-offline-partition-bundle.v1"
            );
            assert!(manifest.get("resident_layers").is_none());
            let files = manifest["files"].as_array().unwrap();
            assert_eq!(files.len(), 8);
            let mut head = false;
            for file in files {
                let assignment: RowAssignment =
                    serde_json::from_value(file["assignment"].clone()).unwrap();
                head |= assignment.tensor == TensorKey::LmHead;
                let rows = full
                    .projection_shape(assignment.layer, assignment.tensor)
                    .unwrap()
                    .0;
                assert_eq!(
                    (assignment.row_start, assignment.row_end),
                    RowPartition { rank, count: 3 }.range(rows).unwrap()
                );
                let expected = directory.path().join("resident.arcrow");
                export_verified_model_rows(&expected, &full, artifact, &assignment).unwrap();
                assert_eq!(
                    std::fs::read(output.join("rows").join(file["file"].as_str().unwrap()))
                        .unwrap(),
                    std::fs::read(expected).unwrap()
                );
            }
            assert!(head);
        }
        for invalid in ["0/0", "2/2", "0/33", "0/17"] {
            let output = directory.path().join("refused");
            assert!(
                run(&arguments(&output, Some(invalid.into()))).is_err(),
                "{invalid}"
            );
            assert!(!output.exists());
            assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".arc-row-staging")
            }));
        }
        let output = directory.path().join("whole");
        run(&arguments(&output, None)).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["format"], "arc.tensor-row-low-residency-bundle.v1");
        assert_eq!(manifest["resident_output"], true);
        assert!(manifest.get("row_partition").is_none());
    }
}
