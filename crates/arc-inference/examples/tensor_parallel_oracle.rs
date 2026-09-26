//! Private two-machine proof harness.  It exports only the first eighth of
//! every canonical tensor to one pinned-SSH sidecar, keeps the remaining rows
//! local, and compares two complete token positions against the normal local
//! canonical-I8 oracle.  It never contacts a validator or public RPC listener.

use arc_crypto::Hash256;
use arc_inference::cached_integer_model::{CachedIntegerModel, I8Weights, KVCache};
use arc_inference::tensor_parallel::{
    ModelRowWorker, PartitionedProjectionBackend, RowAssignment, RowWorker, SshStdioConfig,
    SshStdioRowWorker, TensorKey, export_verified_model_rows,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn file_hash(path: &str) -> Result<Hash256, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = blake3::Hasher::new();
    let mut b = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut b).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(Hash256(*h.finalize().as_bytes()))
}

struct ExportContext<'a> {
    model: &'a Arc<CachedIntegerModel>,
    artifact: Hash256,
    profile: &'a str,
    remote_dir: &'a str,
}

fn add_tensor(
    context: &ExportContext<'_>,
    layer: Option<usize>,
    tensor: TensorKey,
    weights: &I8Weights,
    assignments: &mut Vec<RowAssignment>,
    workers: &mut BTreeMap<String, Arc<dyn RowWorker>>,
) -> Result<(), String> {
    if weights.n_rows < 8 || !weights.n_rows.is_multiple_of(8) {
        return Err(format!(
            "{tensor:?} rows {} cannot be divided into eighths",
            weights.n_rows
        ));
    }
    let part = weights.n_rows / 8;
    for i in 0..8 {
        let start = i * part;
        let end = start + part;
        let worker_id = if i == 0 {
            "ams".to_string()
        } else {
            format!("local-{layer:?}-{tensor:?}-{i}")
        };
        let a = RowAssignment {
            artifact_id: context.artifact,
            execution_profile: context.profile.into(),
            layer,
            tensor,
            row_start: start,
            row_end: end,
            worker_id: worker_id.clone(),
        };
        if i == 0 {
            export_verified_model_rows(
                format!("{}/{layer:?}-{tensor:?}-{start}.arcrow", context.remote_dir),
                context.model,
                context.artifact,
                &a,
            )
            .map_err(|e| e.to_string())?;
        } else {
            workers.insert(
                worker_id.clone(),
                Arc::new(ModelRowWorker {
                    worker_id,
                    assignment: a.clone(),
                    model: context.model.clone(),
                }),
            );
        }
        assignments.push(a);
    }
    Ok(())
}
fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if !(args.len() == 7 || args.len() == 8 || (args.len() == 4 && args[2] == "--export-only")) {
        return Err("usage: tensor_parallel_oracle GGUF SSH_TARGET KNOWN_HOSTS REMOTE_WORKER LOCAL_EXPORT_DIR REMOTE_ROW_DIR [short text] | GGUF --export-only EXPORT_DIR".into());
    }
    let export_dir = if args.len() == 4 { &args[3] } else { &args[5] };
    let artifact = file_hash(&args[1])?;
    let model = Arc::new(
        arc_inference::cached_integer_model::load_cached_model_canonical_i8_interleaved_rope(
            &args[1],
        )
        .map_err(|e| e.to_string())?,
    );
    let profile = model
        .canonical_execution_profile()
        .ok_or("model is not complete canonical I8")?
        .to_string();
    std::fs::create_dir_all(export_dir).map_err(|e| e.to_string())?;
    let mut assignments = Vec::new();
    let mut workers: BTreeMap<String, Arc<dyn RowWorker>> = BTreeMap::new();
    let context = ExportContext {
        model: &model,
        artifact,
        profile: &profile,
        remote_dir: export_dir,
    };
    for (i, l) in model.layers.iter().enumerate() {
        for (k, w) in [
            (TensorKey::Wq, &l.wq),
            (TensorKey::Wk, &l.wk),
            (TensorKey::Wv, &l.wv),
            (TensorKey::Wo, &l.wo),
            (TensorKey::WGate, &l.w_gate),
            (TensorKey::WUp, &l.w_up),
            (TensorKey::WDown, &l.w_down),
        ] {
            add_tensor(&context, Some(i), k, w, &mut assignments, &mut workers)?;
        }
    }
    add_tensor(
        &context,
        None,
        TensorKey::LmHead,
        &model.output_weight,
        &mut assignments,
        &mut workers,
    )?;
    if args.len() == 4 {
        println!(
            "export_only=true files={} bytes={}",
            assignments.iter().filter(|a| a.worker_id == "ams").count(),
            std::fs::read_dir(export_dir)
                .map_err(|e| e.to_string())?
                .try_fold(0u64, |n, e| Ok::<_, std::io::Error>(
                    n + e?.metadata()?.len()
                ))
                .map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let remote_rows: usize = assignments
        .iter()
        .filter(|a| a.worker_id == "ams")
        .map(RowAssignment::rows)
        .sum();
    let total_rows: usize = assignments.iter().map(RowAssignment::rows).sum();
    let export_bytes = std::fs::read_dir(export_dir)
        .map_err(|e| e.to_string())?
        .try_fold(0u64, |n, e| {
            Ok::<_, std::io::Error>(n + e?.metadata()?.len())
        })
        .map_err(|e| e.to_string())?;
    workers.insert(
        "ams".into(),
        Arc::new(
            SshStdioRowWorker::connect(
                "ams".into(),
                SshStdioConfig {
                    ssh_program: "ssh".into(),
                    target: args[2].clone(),
                    known_hosts: args[3].clone().into(),
                    remote_command: vec![args[4].clone(), args[6].clone()],
                    timeout: Duration::from_secs(90),
                },
            )
            .map_err(|e| e.to_string())?,
        ),
    );
    let backend =
        PartitionedProjectionBackend::new(artifact, profile.clone(), assignments, workers);
    let text = args.get(7).map(String::as_str).unwrap_or("Hi");
    let mut prompt_ids = vec![model.config.bos_token];
    prompt_ids.extend(model.encode(text));
    let mut local = KVCache::new(model.config.n_layers);
    let mut remote = KVCache::new(model.config.n_layers);
    let validation_start = Instant::now();
    let mut logits = Vec::new();
    let mut calls = 0usize;
    let mut local_total = Duration::ZERO;
    let mut partitioned_total = Duration::ZERO;
    let mut partitioned_ttft = Duration::ZERO;
    for token in &prompt_ids {
        let local_start = Instant::now();
        let expected = model.forward_one_token(*token, &mut local);
        let local_elapsed = local_start.elapsed();
        let partitioned_start = Instant::now();
        let got = model
            .forward_one_token_canonical_i8_with_backend(
                *token,
                &mut remote,
                Hash256([calls as u8; 32]),
                &backend,
            )
            .map_err(|e| e.to_string())?;
        let partitioned_elapsed = partitioned_start.elapsed();
        if got != expected {
            return Err(format!("oracle mismatch at forward {calls}"));
        }
        local_total += local_elapsed;
        partitioned_total += partitioned_elapsed;
        partitioned_ttft += partitioned_elapsed;
        println!(
            "{}",
            json!({"type":"forward","index":calls,"phase":"prefill","input_token":token,"exact_logits":true,"local_oracle_ms":local_elapsed.as_millis(),"partitioned_ms":partitioned_elapsed.as_millis()})
        );
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        logits = expected;
        calls += 1;
    }
    let mut output = Vec::new();
    for generated_index in 0..2 {
        let next = arc_inference::integer_lut::argmax_i64(&logits) as u32;
        output.push(next);
        if generated_index + 1 < 2 {
            let local_start = Instant::now();
            let expected = model.forward_one_token(next, &mut local);
            let local_elapsed = local_start.elapsed();
            let partitioned_start = Instant::now();
            let got = model
                .forward_one_token_canonical_i8_with_backend(
                    next,
                    &mut remote,
                    Hash256([calls as u8; 32]),
                    &backend,
                )
                .map_err(|e| e.to_string())?;
            let partitioned_elapsed = partitioned_start.elapsed();
            if got != expected {
                return Err(format!("oracle mismatch at forward {calls}"));
            }
            local_total += local_elapsed;
            partitioned_total += partitioned_elapsed;
            println!(
                "{}",
                json!({"type":"forward","index":calls,"phase":"decode","input_token":next,"exact_logits":true,"local_oracle_ms":local_elapsed.as_millis(),"partitioned_ms":partitioned_elapsed.as_millis()})
            );
            std::io::stdout().flush().map_err(|e| e.to_string())?;
            logits = expected;
            calls += 1;
        }
    }
    let validation_wall = validation_start.elapsed();
    let speedup = local_total.as_secs_f64() / partitioned_total.as_secs_f64();
    println!(
        "{}",
        json!({"type":"final","scope":"legacy_text_tokenizer_diagnostic","short_text":text,"prompt_token_ids":prompt_ids,"output_token_ids":output,"exact_all_logits":true,"artifact_id":format!("0x{}",artifact.to_hex()),"execution_profile":profile,"remote_rows":remote_rows,"total_rows":total_rows,"export_bytes":export_bytes,"calls":calls,"local_oracle_total_ms":local_total.as_millis(),"partitioned_total_ms":partitioned_total.as_millis(),"partitioned_ttft_ms":partitioned_ttft.as_millis(),"total_validation_wall_ms":validation_wall.as_millis(),"speedup_local_over_partitioned":speedup,"oracle_excluded_from_partitioned_execution":true,"oracle_included_in_validation_wall":true})
    );
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    Ok(())
}
