//! Export complete selected-layer INT8 rows for a `tensor_row_stdio_worker`.
//!
//! Usage:
//! cargo run -p arc-inference --example tensor_row_layer_bundle_export --features candle --release -- \\
//!   --model /absolute/path/model.gguf --expected-blake3 <64-hex-digits> \\
//!   --worker-id sidecar-1 --layers 0,1,4 --output-dir /absolute/path/bundle
//!
//! The exporter loads the full GGUF into memory to verify and canonicalize it.
//! It is a bundle-construction tool, not a low-memory worker runtime.

use arc_crypto::Hash256;
use arc_inference::cached_integer_model::{
    CachedIntegerModel, CachedLayer, GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE, I8Weights,
    load_cached_model_canonical_i8_interleaved_rope,
};
use arc_inference::model_artifact::ModelArtifactCommitment;
use arc_inference::tensor_parallel::{RowAssignment, TensorKey, export_verified_model_rows};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// Keep in sync with tensor_row_stdio_worker.rs. Both the largest individual
// row file and the sum loaded from its rows/ directory must fit this bound.
const STDIO_WORKER_MAX_BYTES: usize = 1_073_741_824;
const MAX_WORKER_ID_BYTES: usize = 64;

struct PlannedFile<'a> {
    layer: usize,
    tensor: TensorKey,
    tensor_name: &'static str,
    weights: &'a I8Weights,
    serialized_bytes: usize,
}

fn expected_artifact_hash(value: &str) -> Result<Hash256, String> {
    let raw = value.strip_prefix("0x").unwrap_or(value);
    if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("expected BLAKE3 must be 64 hexadecimal digits (optional 0x prefix)".into());
    }
    let bytes = hex::decode(raw).map_err(|_| "invalid expected BLAKE3 hex".to_string())?;
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "expected BLAKE3 must decode to 32 bytes".to_string())?;
    Ok(Hash256(array))
}

fn verify_artifact_hash(actual: Hash256, expected: Hash256) -> Result<(), String> {
    if actual != expected {
        return Err(format!(
            "GGUF BLAKE3 mismatch: expected 0x{}, found 0x{}",
            hex::encode(expected.0),
            hex::encode(actual.0)
        ));
    }
    Ok(())
}

fn parse_layers(value: &str, layer_count: usize) -> Result<Vec<usize>, String> {
    if value.is_empty() {
        return Err("--layers must contain at least one layer index".into());
    }
    let mut layers = Vec::new();
    for part in value.split(',') {
        if part.is_empty() {
            return Err("--layers contains an empty index".into());
        }
        let layer = part
            .parse::<usize>()
            .map_err(|_| format!("invalid layer index {part:?}"))?;
        if layer >= layer_count {
            return Err(format!("layer {layer} is out of range 0..{layer_count}"));
        }
        if layers.contains(&layer) {
            return Err(format!("layer {layer} is listed more than once"));
        }
        layers.push(layer);
    }
    layers.sort_unstable();
    Ok(layers)
}

fn validate_worker_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_WORKER_ID_BYTES
        || value.chars().any(char::is_whitespace)
    {
        return Err("--worker-id must be 1 to 64 UTF-8 bytes without whitespace".into());
    }
    Ok(())
}

fn serialized_row_file_bytes(
    rows: usize,
    cols: usize,
    profile_bytes: usize,
    worker_bytes: usize,
) -> Result<usize, String> {
    if rows == 0 || cols == 0 || rows > 131_072 || cols > 131_072 {
        return Err(format!("unsupported row shape {rows}x{cols}"));
    }
    let payload = rows
        .checked_mul(cols)
        .and_then(|data_bytes| {
            rows.checked_mul(8)
                .and_then(|scale_bytes| data_bytes.checked_add(scale_bytes))
        })
        .ok_or_else(|| "row payload size overflow".to_string())?;
    // ARCROW01 header: magic, artifact, profile length+bytes, layer, tensor,
    // start/end, worker length+bytes, column count and row count.
    let header = 85usize
        .checked_add(profile_bytes)
        .and_then(|n| n.checked_add(worker_bytes))
        .ok_or_else(|| "row header size overflow".to_string())?;
    let serialized = header
        .checked_add(payload)
        .ok_or_else(|| "serialized row file size overflow".to_string())?;
    if serialized > STDIO_WORKER_MAX_BYTES {
        return Err(format!(
            "serialized row file is {serialized} bytes, over the 1 GiB worker file bound"
        ));
    }
    Ok(serialized)
}

fn checked_aggregate_row_bytes(sizes: impl IntoIterator<Item = usize>) -> Result<usize, String> {
    let mut total = 0usize;
    for size in sizes {
        total = total
            .checked_add(size)
            .ok_or_else(|| "aggregate row bundle size overflow".to_string())?;
        if total > STDIO_WORKER_MAX_BYTES {
            return Err(format!(
                "aggregate serialized row bundle is {total} bytes, over the 1 GiB stdio worker bound"
            ));
        }
    }
    Ok(total)
}

fn layer_projections(layer: &CachedLayer) -> [(&'static str, TensorKey, &I8Weights); 7] {
    [
        ("wq", TensorKey::Wq, &layer.wq),
        ("wk", TensorKey::Wk, &layer.wk),
        ("wv", TensorKey::Wv, &layer.wv),
        ("wo", TensorKey::Wo, &layer.wo),
        ("w_gate", TensorKey::WGate, &layer.w_gate),
        ("w_up", TensorKey::WUp, &layer.w_up),
        ("w_down", TensorKey::WDown, &layer.w_down),
    ]
}

fn plan_files<'a>(
    model: &'a CachedIntegerModel,
    layers: &[usize],
    profile: &str,
    worker_id: &str,
) -> Result<Vec<PlannedFile<'a>>, String> {
    let mut files = Vec::with_capacity(layers.len() * 7);
    for &layer_idx in layers {
        let layer = model
            .layers
            .get(layer_idx)
            .ok_or_else(|| format!("layer {layer_idx} is out of range"))?;
        for (tensor_name, tensor, weights) in layer_projections(layer) {
            if weights.data.len() != weights.n_rows.saturating_mul(weights.n_cols)
                || weights.scales.len() != weights.n_rows
            {
                return Err(format!(
                    "layer {layer_idx} {tensor_name} has inconsistent I8 row storage"
                ));
            }
            let serialized_bytes = serialized_row_file_bytes(
                weights.n_rows,
                weights.n_cols,
                profile.len(),
                worker_id.len(),
            )?;
            if serialized_bytes > STDIO_WORKER_MAX_BYTES {
                return Err(format!(
                    "layer {layer_idx} {tensor_name} serializes to {serialized_bytes} bytes, over the 1 GiB worker file bound"
                ));
            }
            files.push(PlannedFile {
                layer: layer_idx,
                tensor,
                tensor_name,
                weights,
                serialized_bytes,
            });
        }
    }
    checked_aggregate_row_bytes(files.iter().map(|file| file.serialized_bytes))?;
    Ok(files)
}

fn merged_layer_ranges(layers: &[usize]) -> Result<Vec<[u32; 2]>, String> {
    let mut ranges: Vec<[u32; 2]> = Vec::new();
    for &layer in layers {
        let start = u32::try_from(layer).map_err(|_| "layer index exceeds u32".to_string())?;
        let end = start
            .checked_add(1)
            .ok_or_else(|| "layer range overflow".to_string())?;
        if let Some(last) = ranges.last_mut() {
            if last[1] == start {
                last[1] = end;
                continue;
            }
        }
        ranges.push([start, end]);
    }
    Ok(ranges)
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn check_output_target(output: &Path) -> Result<(PathBuf, String), String> {
    if !output.is_absolute() {
        return Err("--output-dir must be an absolute path".into());
    }
    if path_exists(output) {
        return Err(format!(
            "output directory already exists: {}",
            output.display()
        ));
    }
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| "--output-dir must end in a normal UTF-8 directory name".to_string())?
        .to_string();
    let parent = output
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| "--output-dir parent must already exist".to_string())?;
    let parent = fs::canonicalize(parent).map_err(|error| error.to_string())?;
    Ok((parent.join(&name), name))
}

struct StagingDir {
    path: PathBuf,
    published: bool,
}
impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn create_staging_dir(parent: &Path, name: &str) -> Result<StagingDir, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    for attempt in 0..100u32 {
        let path = parent.join(format!(
            ".{name}.staging.{}.{}.{attempt}",
            std::process::id(),
            stamp
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                return Ok(StagingDir {
                    path,
                    published: false,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create staging directory: {error}")),
        }
    }
    Err("could not reserve a unique staging directory".into())
}

fn hash_file(path: &Path) -> Result<(String, u64), String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 1024 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| "file size overflow".to_string())?;
    }
    Ok((hex::encode(hasher.finalize().as_bytes()), size))
}

fn usage(program: &str) {
    eprintln!(
        "usage: {program} --model GGUF --expected-blake3 HEX --worker-id ID --layers 0,1,4 --output-dir ABSOLUTE_DIR"
    );
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        usage(&args[0]);
        return Ok(());
    }
    let mut model_path = None;
    let mut expected_hash = None;
    let mut worker_id = None;
    let mut layer_spec = None;
    let mut output_arg = None;
    let mut i = 1;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let value = args
            .get(i)
            .ok_or_else(|| format!("missing value for {flag}"))?
            .clone();
        i += 1;
        let slot = match flag {
            "--model" => &mut model_path,
            "--expected-blake3" => &mut expected_hash,
            "--worker-id" => &mut worker_id,
            "--layers" => &mut layer_spec,
            "--output-dir" => &mut output_arg,
            _ => return Err(format!("unknown option {flag:?}")),
        };
        if slot.replace(value).is_some() {
            return Err(format!("option {flag} was supplied more than once"));
        }
    }
    let model_path = model_path.ok_or("--model is required")?;
    let expected_hash = expected_hash.ok_or("--expected-blake3 is required")?;
    let expected_hash = expected_artifact_hash(&expected_hash)?;
    let worker_id = worker_id.ok_or("--worker-id is required")?;
    validate_worker_id(&worker_id)?;
    let layer_spec = layer_spec.ok_or("--layers is required")?;
    let output_arg = output_arg.ok_or("--output-dir is required")?;
    let requested_output = PathBuf::from(output_arg);
    let (output, output_name) = check_output_target(&requested_output)?;

    // Validate exact source bytes before loading or writing anything.
    let artifact =
        ModelArtifactCommitment::from_path(&model_path).map_err(|error| error.to_string())?;
    verify_artifact_hash(artifact.model_id(), expected_hash)?;
    let model = load_cached_model_canonical_i8_interleaved_rope(&model_path)
        .map_err(|error| format!("canonical interleaved-RoPE model load failed: {error}"))?;
    artifact
        .verify_unchanged()
        .map_err(|error| error.to_string())?;
    if model.canonical_execution_profile() != Some(GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE)
        || !model.has_all_transformer_layers()
    {
        return Err("loaded model is not complete canonical interleaved-RoPE INT8".into());
    }
    let layers = parse_layers(&layer_spec, model.layers.len())?;
    let profile = model
        .canonical_execution_profile()
        .ok_or("canonical execution profile missing")?;
    let files = plan_files(&model, &layers, profile, &worker_id)?;
    let total_row_bytes = files.iter().try_fold(0usize, |sum, file| {
        sum.checked_add(file.serialized_bytes)
            .ok_or_else(|| "aggregate row bundle size overflow".to_string())
    })?;
    if total_row_bytes > STDIO_WORKER_MAX_BYTES {
        return Err(format!(
            "aggregate serialized row bundle is {total_row_bytes} bytes, over the 1 GiB stdio worker bound"
        ));
    }
    let layer_ranges = merged_layer_ranges(&layers)?;

    // Stage beside the destination, keeping publication on the same filesystem.
    let mut staging = create_staging_dir(
        output.parent().expect("normalized output parent"),
        &output_name,
    )?;
    let rows_dir = staging.path.join("rows");
    fs::create_dir(&rows_dir).map_err(|error| error.to_string())?;
    let mut manifest_files = Vec::with_capacity(files.len());
    let mut written_row_bytes = 0u64;
    for file in &files {
        let filename = format!("layer-{:05}-{}.arcrow", file.layer, file.tensor_name);
        let path = rows_dir.join(&filename);
        let assignment = RowAssignment {
            artifact_id: artifact.model_id(),
            execution_profile: profile.to_string(),
            layer: Some(file.layer),
            tensor: file.tensor,
            row_start: 0,
            row_end: file.weights.n_rows,
            worker_id: worker_id.clone(),
        };
        export_verified_model_rows(&path, &model, artifact.model_id(), &assignment)
            .map_err(|error| format!("export {filename}: {error}"))?;
        let (hash, bytes) = hash_file(&path)?;
        if bytes != file.serialized_bytes as u64 {
            return Err(format!(
                "export size mismatch for {filename}: planned {}, wrote {bytes}",
                file.serialized_bytes
            ));
        }
        written_row_bytes = written_row_bytes
            .checked_add(bytes)
            .ok_or_else(|| "written row bytes overflow".to_string())?;
        manifest_files.push(json!({
            "file": filename,
            "layer": file.layer,
            "tensor": file.tensor_name,
            "row_start": 0,
            "row_end": file.weights.n_rows,
            "rows": file.weights.n_rows,
            "cols": file.weights.n_cols,
            "bytes": bytes,
            "blake3": format!("0x{hash}"),
        }));
    }
    if written_row_bytes as usize != total_row_bytes {
        return Err("aggregate exported bytes differ from preflight".into());
    }
    let row_directory = output.join("rows");
    let config_fragment = json!({
        "id": worker_id,
        "ssh_program": "ssh",
        "target": "REPLACE_WITH_USER_AT_WORKER_HOST",
        "known_hosts": "/absolute/path/to/pinned_known_hosts",
        "remote_command": ["/absolute/path/to/tensor_row_stdio_worker", row_directory],
        "timeout_ms": 600000,
        "startup_timeout_ms": 600000,
        "ram_headroom_bytes": 0,
        "max_concurrency": 1,
        "resident_layers": layer_ranges
    });
    let manifest: Value = json!({
        "format": "arc.tensor-row-layer-bundle.v1",
        "artifact_blake3": format!("0x{}", hex::encode(artifact.model_id().0)),
        "artifact_size_bytes": artifact.size_bytes(),
        "execution_profile": profile,
        "worker_id": worker_id,
        "selected_layers": layers,
        "projection_tensors_per_layer": ["wq", "wk", "wv", "wo", "w_gate", "w_up", "w_down"],
        "row_file_count": manifest_files.len(),
        "serialized_row_bytes": written_row_bytes,
        "stdio_worker_max_serialized_bytes": STDIO_WORKER_MAX_BYTES,
        "files": manifest_files,
        "row_cohort_worker_config_fragment": config_fragment,
        "operator_note": "Replace transport placeholders and set measured ram_headroom_bytes before activation. The worker_id is a placement label echoed from the request, not an authentication boundary.",
        "exporter_requires_full_model_residency": true
    });
    let manifest_path = staging.path.join("manifest.json");
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    fs::write(&manifest_path, manifest_bytes).map_err(|error| error.to_string())?;

    if path_exists(&output) {
        return Err(format!(
            "output directory appeared before publish: {}",
            output.display()
        ));
    }
    fs::rename(&staging.path, &output)
        .map_err(|error| format!("atomic publish failed: {error}"))?;
    staging.published = true;
    println!(
        "{}",
        serde_json::to_string_pretty(&manifest).unwrap_or_default()
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("layer bundle export failed: {error}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toy_weights(rows: usize, cols: usize) -> I8Weights {
        I8Weights {
            data: vec![7; rows * cols],
            scales: vec![1 << 16; rows],
            n_rows: rows,
            n_cols: cols,
        }
    }

    fn toy_layer() -> CachedLayer {
        CachedLayer {
            wq: toy_weights(2, 3),
            wk: toy_weights(3, 2),
            wv: toy_weights(3, 2),
            wo: toy_weights(2, 3),
            w_gate: toy_weights(4, 3),
            w_up: toy_weights(4, 3),
            w_down: toy_weights(2, 4),
            attn_norm: vec![],
            ffn_norm: vec![],
        }
    }

    #[test]
    fn selected_layer_plan_covers_all_seven_full_projection_rows() {
        let layer = toy_layer();
        let projections = layer_projections(&layer);
        assert_eq!(projections.len(), 7);
        assert_eq!(
            projections.map(|(name, _, _)| name),
            ["wq", "wk", "wv", "wo", "w_gate", "w_up", "w_down"]
        );
        for (_, _, weights) in projections {
            assert_eq!(weights.data.len(), weights.n_rows * weights.n_cols);
            assert_eq!(weights.scales.len(), weights.n_rows);
        }
    }

    #[test]
    fn layer_selection_rejects_empty_duplicate_and_out_of_range_indices() {
        assert!(parse_layers("", 4).is_err());
        assert!(parse_layers("0,", 4).is_err());
        assert!(parse_layers("0,2,2", 4).is_err());
        assert!(parse_layers("4", 4).is_err());
        assert_eq!(parse_layers("2,0", 4).unwrap(), vec![0, 2]);
    }

    #[test]
    fn preflight_enforces_worker_dimension_and_aggregate_bounds() {
        assert!(serialized_row_file_bytes(0, 1, 64, 8).is_err());
        assert!(serialized_row_file_bytes(131_073, 1, 64, 8).is_err());
        assert!(serialized_row_file_bytes(1, 131_073, 64, 8).is_err());
        let exact = serialized_row_file_bytes(4, 8, 12, 3).unwrap();
        assert_eq!(exact, 85 + 12 + 3 + 4 * 8 + 4 * 8);
        assert!(exact <= STDIO_WORKER_MAX_BYTES);
        assert!(serialized_row_file_bytes(131_072, 131_072, 64, 64).is_err());

        let boundary_parts = [STDIO_WORKER_MAX_BYTES - 128, 128];
        assert!(
            boundary_parts
                .iter()
                .all(|size| *size <= STDIO_WORKER_MAX_BYTES)
        );
        assert_eq!(
            checked_aggregate_row_bytes(boundary_parts).unwrap(),
            STDIO_WORKER_MAX_BYTES
        );

        let individually_valid_but_too_large = [STDIO_WORKER_MAX_BYTES / 2 + 1; 2];
        assert!(
            individually_valid_but_too_large
                .iter()
                .all(|size| *size <= STDIO_WORKER_MAX_BYTES)
        );
        assert!(checked_aggregate_row_bytes(individually_valid_but_too_large).is_err());
        assert!(checked_aggregate_row_bytes([usize::MAX, 1]).is_err());
    }

    #[test]
    fn artifact_commitment_rejects_a_mismatch() {
        assert!(verify_artifact_hash(Hash256([1; 32]), Hash256([1; 32])).is_ok());
        assert!(verify_artifact_hash(Hash256([1; 32]), Hash256([2; 32])).is_err());
        assert!(expected_artifact_hash(&"ab".repeat(32)).is_ok());
        assert!(expected_artifact_hash("not-a-hash").is_err());
    }

    #[test]
    fn output_layer_ranges_merge_adjacent_layers() {
        assert_eq!(
            merged_layer_ranges(&[0, 1, 4, 5, 7]).unwrap(),
            vec![[0, 2], [4, 6], [7, 8]]
        );
    }

    #[test]
    fn file_hash_reports_exact_bytes_and_blake3() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "arc-layer-bundle-hash-{}-{stamp}",
            std::process::id()
        ));
        let bytes = b"ARCROW01-test-payload";
        fs::write(&path, bytes).unwrap();
        let (hash, size) = hash_file(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(hash, blake3::hash(bytes).to_hex().to_string());
    }
}
