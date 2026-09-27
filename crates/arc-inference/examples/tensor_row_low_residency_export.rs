//! Prepare <=1 GiB row bundles without loading the full model. Existing
//! tensor_row_stdio_worker consumes the identical ARCROW01 files. Source
//! reads retain the pinned artifact/profile and check verified chunk hashes.
//!
//! --model GGUF --artifact HASH --worker-id ID --layers 0,1,2,3
//! --output-dir NEW_ABSOLUTE_DIRECTORY [--include-output]
//!
//! Peak preparation is bounded by two copies of the largest selected matrix
//! plus norms/metadata/verified-read scratch. Worker startup retains <=1 GiB
//! total rows and can transiently use one additional file-sized conversion
//! buffer. Neither preparation nor serving first constructs a full model.

use arc_crypto::Hash256;
use arc_inference::low_residency::{CanonicalRowSource, load_low_residency_model};
use arc_inference::tensor_parallel::{MAX_CANONICAL_ROW_FILE_BYTES, RowAssignment, TensorKey};
use std::path::PathBuf;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        let bytes = rows
            .checked_mul(cols + 8)
            .and_then(|n| n.checked_add(85 + profile.len() + id.len()))
            .ok_or("size overflow")?;
        total = total.checked_add(bytes).ok_or("size overflow")?;
        if total > MAX_CANONICAL_ROW_FILE_BYTES {
            return Err("selected rows exceed the stdio worker's 1 GiB aggregate startup bound; select fewer layers".into());
        }
        largest = largest.max(bytes);
        let assignment = RowAssignment {
            artifact_id: artifact,
            execution_profile: profile.into(),
            layer,
            tensor,
            row_start: 0,
            row_end: rows,
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
        let manifest = serde_json::json!({
            "format": "arc.tensor-row-low-residency-bundle.v1", "artifact_blake3": artifact.to_hex(),
            "execution_profile": profile, "worker_id": id, "resident_layers": ranges,
            "resident_output": include_output, "serialized_row_bytes": total,
            "largest_row_file_bytes": largest,
            "preparation_matrix_peak_upper_bound_bytes": largest.saturating_mul(2),
            "worker_startup_payload_peak_upper_bound_bytes": total.saturating_add(largest),
            "files": files,
            "capacity_note": "Payload bounds exclude process/allocator/OS/node memory. Measure aggregate host peak and latency before activation."
        });
        std::fs::write(
            staging.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
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
