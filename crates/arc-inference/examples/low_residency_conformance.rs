//! Offline numerical conformance; run through scripts/qualification/run_low_residency_conformance.py.
//! Modes: inspect, reference, coordinator. The runner waits for reference exit
//! before exporting or starting row daemons. This is not a readiness/quality gate.

#[cfg(unix)]
#[path = "support/row_partition.rs"]
mod row_partition;

#[cfg(unix)]
mod unix {
    use super::row_partition::RowPartition;
    use arc_crypto::Hash256;
    use arc_inference::cached_integer_model::{
        GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE as PROFILE, KVCache, ModelConfig,
        load_cached_model_canonical_i8_interleaved_rope, select_next_token_with_repetition_penalty,
    };
    use arc_inference::low_residency::{CanonicalRowSource, load_low_residency_model};
    use arc_inference::tensor_parallel::{
        MAX_CANONICAL_ROW_FILE_BYTES, MAX_ROW_FRAME_BYTES, MAX_ROW_WORKERS_PER_STAGE, PlannedSlice,
        ProjectionPlan, RowAssignment, RowEvent, RowEventSink, RowProjectionRequest,
        RowProjectionResponse, RowWorker, SliceOwner, TensorKey, TensorParallelError,
        VerifiedPartitionBackend, decode_row_response, encode_row_request, hash_i64,
        validate_exact_coverage,
    };
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn hash_file(path: &Path) -> Result<Hash256, String> {
        let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut hash = blake3::Hasher::new();
        let mut buffer = vec![0; 65536];
        loop {
            let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        Ok(Hash256(*hash.finalize().as_bytes()))
    }

    fn max_rss_bytes() -> Option<u64> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // SAFETY: correctly sized, initialized output buffer, not retained.
            let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
            if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
                return None;
            }
            #[cfg(target_os = "linux")]
            return Some((usage.ru_maxrss as u64).saturating_mul(1024));
            #[cfg(target_os = "macos")]
            return Some(usage.ru_maxrss as u64);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        None
    }

    fn graph(config: &ModelConfig) -> Value {
        json!({"layers": config.n_layers, "width": config.d_model, "kv_width": config.d_kv,
            "ff_width": config.d_ff, "vocab": config.vocab_size, "bos": config.bos_token,
            "eos": config.eos_tokens, "max_seq": config.max_seq})
    }

    fn kv_hash(cache: &KVCache) -> String {
        let mut hash = blake3::Hasher::new();
        hash.update(b"ARC-low-residency-conformance-kv-v1");
        hash.update(&(cache.seq_len as u64).to_le_bytes());
        for layers in [&cache.k_data, &cache.v_data] {
            hash.update(&(layers.len() as u64).to_le_bytes());
            for layer in layers {
                hash.update(&(layer.len() as u64).to_le_bytes());
                for value in layer {
                    hash.update(&value.to_le_bytes());
                }
            }
        }
        hash.finalize().to_hex().to_string()
    }

    #[derive(Debug, Serialize, PartialEq, Eq)]
    struct Position {
        position: usize,
        input_token: u32,
        logit_count: usize,
        logits_blake3_le_i64: String,
        kv_blake3: String,
    }
    #[derive(Debug, Serialize)]
    struct Trace {
        output_token_ids: Vec<u32>,
        output_hash: String,
        positions: Vec<Position>,
        forward_ms: Vec<f64>,
    }

    // Explicit generation-v2 replay, including its final non-EOS forward.
    // Reference uses the existing resident forward, not the shared backend
    // forward under test. Digests are captured before repetition penalties.
    fn trace(
        config: &ModelConfig,
        prompt: &[u32],
        max_tokens: u32,
        mut forward: impl FnMut(u32, &mut KVCache) -> Result<Vec<i64>, String>,
    ) -> Result<Trace, String> {
        if prompt.is_empty()
            || prompt[0] == config.bos_token
            || prompt.iter().any(|&id| id as usize >= config.vocab_size)
            || 1 + prompt.len() + max_tokens as usize > config.max_seq
        {
            return Err("invalid prompt IDs or context budget".into());
        }
        let mut cache = KVCache::new(config.n_layers);
        let mut positions = Vec::new();
        let mut forward_ms = Vec::new();
        let mut recorded = |token: u32| -> Result<Vec<i64>, String> {
            let position = cache.seq_len;
            let started = Instant::now();
            let logits = forward(token, &mut cache)?;
            forward_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            if logits.len() != config.vocab_size || cache.seq_len != position + 1 {
                return Err("forward returned wrong logits/cache shape".into());
            }
            positions.push(Position {
                position,
                input_token: token,
                logit_count: logits.len(),
                logits_blake3_le_i64: hash_i64(&logits).to_hex(),
                kv_blake3: kv_hash(&cache),
            });
            Ok(logits)
        };
        let mut logits = recorded(config.bos_token)?;
        for &token in prompt {
            logits = recorded(token)?;
        }
        let mut output_token_ids = Vec::new();
        for _ in 0..max_tokens {
            let token = select_next_token_with_repetition_penalty(&mut logits, &output_token_ids);
            output_token_ids.push(token);
            if config.eos_tokens.contains(&token) {
                break;
            }
            logits = recorded(token)?;
        }
        let bytes: Vec<u8> = output_token_ids
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();
        Ok(Trace {
            output_token_ids,
            output_hash: arc_crypto::hash_bytes(&bytes).to_hex(),
            positions,
            forward_ms,
        })
    }

    #[derive(Serialize)]
    struct ExportBundle {
        worker_id: String,
        layers: Vec<usize>,
        include_output: bool,
        serialized_row_bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        row_partition: Option<String>,
    }
    // The first row of each owner is a separate, locally duplicated slice.
    // The strict executor's slice cap therefore bounds proof partitions to 16.
    fn proof_partition_count(value: Option<&str>) -> Result<Option<usize>, String> {
        value
            .map(|value| {
                let count = value
                    .parse::<usize>()
                    .map_err(|_| "invalid row partition count")?;
                if !(2..=MAX_ROW_WORKERS_PER_STAGE / 2).contains(&count) {
                    return Err(format!(
                        "offline proof row partitions must be 2..={}",
                        MAX_ROW_WORKERS_PER_STAGE / 2
                    ));
                }
                Ok(count)
            })
            .transpose()
    }

    fn inspect(
        model: &impl CanonicalRowSource,
        partitions: Option<usize>,
    ) -> Result<Value, String> {
        let tensors = [
            TensorKey::Wq,
            TensorKey::Wk,
            TensorKey::Wv,
            TensorKey::Wo,
            TensorKey::WGate,
            TensorKey::WUp,
            TensorKey::WDown,
        ];
        let bytes =
            |layer, tensor, id: &str, partition: Option<RowPartition>| -> Result<usize, String> {
                let (rows, cols) = model
                    .projection_shape(layer, tensor)
                    .ok_or("missing stage")?;
                let (start, end) = partition
                    .unwrap_or(RowPartition { rank: 0, count: 1 })
                    .range(rows)?;
                (end - start)
                    .checked_mul(cols + 8)
                    .and_then(|n| n.checked_add(85 + PROFILE.len() + id.len()))
                    .ok_or_else(|| "row size overflow".into())
            };
        if let Some(count) = partitions {
            // Preflight EVERY bundle before the runner can start any export.
            let mut bundles = Vec::new();
            for rank in 0..count {
                let id = format!("proof-{rank:03}");
                let partition = Some(RowPartition::parse(&format!("{rank}/{count}"))?);
                let mut total = bytes(None, TensorKey::LmHead, &id, partition)?;
                for layer in 0..model.config().n_layers {
                    for tensor in tensors {
                        total = total
                            .checked_add(bytes(Some(layer), tensor, &id, partition)?)
                            .ok_or("bundle size overflow")?;
                    }
                }
                if total > MAX_CANONICAL_ROW_FILE_BYTES {
                    return Err(
                        "partial-row bundle exceeds 1 GiB; increase --row-partitions".into(),
                    );
                }
                bundles.push(ExportBundle {
                    worker_id: id,
                    layers: (0..model.config().n_layers).collect(),
                    include_output: true,
                    serialized_row_bytes: total,
                    row_partition: Some(format!("{rank}/{count}")),
                });
            }
            return Ok(json!({"graph": graph(model.config()), "bundles": bundles,
                "layout": "partial-rows", "row_partitions": count,
                "total_row_file_bytes": bundles.iter().map(|b| b.serialized_row_bytes).sum::<usize>(),
                "kv_bytes_per_position": model.config().n_layers * model.config().d_kv * 16}));
        }
        let mut bundles = Vec::new();
        let mut current = ExportBundle {
            worker_id: "proof-000".into(),
            layers: Vec::new(),
            include_output: false,
            serialized_row_bytes: 0,
            row_partition: None,
        };
        for layer in 0..model.config().n_layers {
            let size = tensors.iter().try_fold(0usize, |sum, &t| {
                sum.checked_add(bytes(Some(layer), t, &current.worker_id, None)?)
                    .ok_or_else(|| "layer size overflow".to_string())
            })?;
            if size > MAX_CANONICAL_ROW_FILE_BYTES {
                return Err("one layer exceeds row bundle bound".into());
            }
            if current.serialized_row_bytes + size > MAX_CANONICAL_ROW_FILE_BYTES {
                bundles.push(current);
                current = ExportBundle {
                    worker_id: format!("proof-{:03}", bundles.len()),
                    layers: Vec::new(),
                    include_output: false,
                    serialized_row_bytes: 0,
                    row_partition: None,
                };
            }
            current.layers.push(layer);
            current.serialized_row_bytes += size;
        }
        bundles.push(current);
        let mut placed = false;
        for bundle in &mut bundles {
            let output = bytes(None, TensorKey::LmHead, &bundle.worker_id, None)?;
            if bundle.serialized_row_bytes + output <= MAX_CANONICAL_ROW_FILE_BYTES {
                bundle.include_output = true;
                bundle.serialized_row_bytes += output;
                placed = true;
                break;
            }
        }
        if !placed {
            return Err(
                "no layer bundle has output-head capacity; choose smaller layer groups".into(),
            );
        }
        Ok(
            json!({"graph": graph(model.config()), "bundles": bundles, "layout": "layers",
            "total_row_file_bytes": bundles.iter().map(|b| b.serialized_row_bytes).sum::<usize>(),
            "kv_bytes_per_position": model.config().n_layers * model.config().d_kv * 16}),
        )
    }

    // Local offline adapter: actual daemon processes, identical bounded row
    // frames. The production pinned-SSH adapter is not replaced or modified.
    struct SocketWorker(Mutex<Option<UnixStream>>);
    impl RowWorker for SocketWorker {
        fn project(
            &self,
            request: RowProjectionRequest,
        ) -> Result<RowProjectionResponse, TensorParallelError> {
            let mut slot = self
                .0
                .lock()
                .map_err(|_| TensorParallelError::Worker("socket poisoned".into()))?;
            let result = (|| {
                let stream = slot
                    .as_mut()
                    .ok_or_else(|| TensorParallelError::Worker("socket closed".into()))?;
                let bytes = encode_row_request(&request)?;
                let io = |e: std::io::Error| TensorParallelError::Worker(e.to_string());
                stream
                    .write_all(&(bytes.len() as u32).to_le_bytes())
                    .map_err(io)?;
                stream.write_all(&bytes).map_err(io)?;
                let mut length = [0; 4];
                stream.read_exact(&mut length).map_err(io)?;
                let count = u32::from_le_bytes(length) as usize;
                if count == 0 || count > MAX_ROW_FRAME_BYTES {
                    return Err(TensorParallelError::Bounds);
                }
                let mut answer = vec![0; count];
                stream.read_exact(&mut answer).map_err(io)?;
                decode_row_response(&answer)
            })();
            if result.is_err() {
                *slot = None;
            }
            result
        }
        fn is_open(&self) -> bool {
            self.0.lock().is_ok_and(|s| s.is_some())
        }
    }
    #[derive(Default)]
    struct Events(Mutex<(u64, u64)>);
    impl RowEventSink for Events {
        fn record(&self, event: RowEvent) {
            let mut counts = self.0.lock().expect("events lock");
            match event {
                RowEvent::Answered { .. } => counts.0 += 1,
                _ => counts.1 += 1,
            }
        }
        fn is_excluded(&self, _: &str) -> bool {
            false
        }
    }
    #[derive(Clone, Deserialize)]
    struct Worker {
        worker_id: String,
        socket: String,
        assignments: Vec<RowAssignment>,
    }
    type Plans = BTreeMap<(Option<usize>, TensorKey), ProjectionPlan>;
    type Workers = BTreeMap<String, Arc<dyn RowWorker>>;
    // Validate the complete manifest union before opening any socket. The
    // profile/identity/range gates are identical for whole and partial rows.
    fn plans_from_entries(
        entries: &[Worker],
        model: &impl CanonicalRowSource,
        artifact: Hash256,
        partitions: Option<usize>,
    ) -> Result<Plans, String> {
        if entries.is_empty() || entries.len() > 64 {
            return Err("worker count outside 1..64".into());
        }
        if partitions.is_some_and(|count| entries.len() != count) {
            return Err("partial layout requires exactly the declared worker count".into());
        }
        let mut ids = BTreeSet::new();
        let mut sockets = BTreeSet::new();
        let mut grouped: BTreeMap<_, Vec<RowAssignment>> = BTreeMap::new();
        for entry in entries {
            if entry.worker_id.is_empty()
                || entry.worker_id.len() > 64
                || entry.worker_id.chars().any(char::is_whitespace)
                || !ids.insert(&entry.worker_id)
                || !sockets.insert(&entry.socket)
                || entry.assignments.is_empty()
            {
                return Err(
                    "empty, duplicate or invalid worker identity/socket/assignments".into(),
                );
            }
            let mut stages = BTreeSet::new();
            for assignment in &entry.assignments {
                let key = (assignment.layer, assignment.tensor);
                if assignment.worker_id != entry.worker_id || !stages.insert(key) {
                    return Err("assignment has wrong worker or duplicate stage owner".into());
                }
                grouped.entry(key).or_default().push(assignment.clone());
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
        let keys = (0..model.config().n_layers)
            .flat_map(|layer| tensors.into_iter().map(move |tensor| (Some(layer), tensor)))
            .chain(std::iter::once((None, TensorKey::LmHead)));
        let mut plans = BTreeMap::new();
        for key in keys {
            let (rows, _) = model
                .projection_shape(key.0, key.1)
                .ok_or("missing model projection")?;
            let assignments = grouped.remove(&key).ok_or("missing manifest projection")?;
            let ordered =
                validate_exact_coverage(&assignments, artifact, PROFILE, key.0, key.1, rows)
                    .map_err(|error| error.to_string())?;
            if partitions.is_some_and(|count| ordered.len() != count) {
                return Err("projection lacks distinct partial-row owners".into());
            }
            let mut slices = Vec::new();
            for (rank, assignment) in ordered.iter().enumerate() {
                if let Some(count) = partitions {
                    let expected = RowPartition { rank, count }.range(rows)?;
                    if (assignment.row_start, assignment.row_end) != expected {
                        return Err(
                            "manifest ranges differ from deterministic partial-row layout".into(),
                        );
                    }
                }
                let owner = SliceOwner::Remote(assignment.worker_id.clone());
                // Bounded independent numerical verification of EVERY owner;
                // every primary row, including the checked one, stays remote.
                slices.push(PlannedSlice {
                    owner: owner.clone(),
                    row_start: assignment.row_start,
                    row_end: assignment.row_start + 1,
                    duplicate_on: Some(SliceOwner::Local),
                });
                if assignment.row_end > assignment.row_start + 1 {
                    slices.push(PlannedSlice {
                        owner,
                        row_start: assignment.row_start + 1,
                        row_end: assignment.row_end,
                        duplicate_on: None,
                    });
                }
            }
            if slices.len() > MAX_ROW_WORKERS_PER_STAGE {
                return Err("verified slice count exceeds executor bound".into());
            }
            plans.insert(
                key,
                ProjectionPlan {
                    slices,
                    spot_rows: vec![0, rows / 2, rows - 1],
                },
            );
        }
        if !grouped.is_empty() {
            return Err("unknown manifest projection".into());
        }
        Ok(plans)
    }

    fn connect(
        path: &Path,
        model: &impl CanonicalRowSource,
        artifact: Hash256,
        partitions: Option<usize>,
    ) -> Result<(Plans, Workers), String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        if file.metadata().map_err(|e| e.to_string())?.len() > 1024 * 1024 {
            return Err("workers config exceeds 1 MiB".into());
        }
        let entries: Vec<Worker> = serde_json::from_reader(file).map_err(|e| e.to_string())?;
        let plans = plans_from_entries(&entries, model, artifact, partitions)?;
        let mut workers: Workers = BTreeMap::new();
        for entry in entries {
            let stream = UnixStream::connect(&entry.socket).map_err(|e| e.to_string())?;
            stream
                .set_read_timeout(Some(Duration::from_secs(180)))
                .map_err(|e| e.to_string())?;
            stream
                .set_write_timeout(Some(Duration::from_secs(180)))
                .map_err(|e| e.to_string())?;
            workers.insert(
                entry.worker_id,
                Arc::new(SocketWorker(Mutex::new(Some(stream)))),
            );
        }
        Ok((plans, workers))
    }

    pub fn main() -> Result<(), String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mode = args
            .first()
            .ok_or("required mode: inspect, reference, coordinator")?;
        let mut values = BTreeMap::new();
        for pair in args[1..].chunks(2) {
            if pair.len() != 2
                || ![
                    "--model",
                    "--artifact",
                    "--prompt-ids",
                    "--max-tokens",
                    "--warmups",
                    "--workers",
                    "--row-partitions",
                    "--output",
                ]
                .contains(&pair[0].as_str())
                || values.insert(pair[0].as_str(), pair[1].as_str()).is_some()
            {
                return Err("unknown, duplicate or valueless argument".into());
            }
        }
        let get = |key| {
            values
                .get(key)
                .copied()
                .ok_or_else(|| format!("required {key}"))
        };
        let path = Path::new(get("--model")?);
        let artifact = Hash256::from_hex(get("--artifact")?.trim_start_matches("0x"))
            .map_err(|e| e.to_string())?;
        let output = Path::new(get("--output")?);
        if output.exists() {
            return Err("output already exists".into());
        }
        let prompt: Vec<u32> = get("--prompt-ids")?
            .split(',')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| "invalid prompt IDs")?;
        let max_tokens: u32 = get("--max-tokens")?
            .parse()
            .map_err(|_| "invalid max tokens")?;
        let warmups: usize = values
            .get("--warmups")
            .unwrap_or(&"0")
            .parse()
            .map_err(|_| "invalid warmups")?;
        if prompt.is_empty() || prompt.len() > 16 || !(1..=4).contains(&max_tokens) || warmups > 1 {
            return Err("bounds: 1..16 prompt IDs, 1..4 generated tokens, 0..1 warmups".into());
        }
        let partitions = proof_partition_count(values.get("--row-partitions").copied())?;
        let started = Instant::now();
        let mut result = if mode == "inspect" {
            let model = load_low_residency_model(path, artifact).map_err(|e| e.to_string())?;
            let mut report = inspect(&model, partitions)?;
            report["prepared_state_bytes"] = json!(model.resident_state_bytes());
            report
        } else if mode == "reference" {
            if hash_file(path)? != artifact {
                return Err("reference artifact mismatch before load".into());
            }
            let model = load_cached_model_canonical_i8_interleaved_rope(
                path.to_str().ok_or("non-UTF8 model path")?,
            )
            .map_err(|e| e.to_string())?;
            if model.canonical_execution_profile() != Some(PROFILE)
                || !model.has_all_transformer_layers()
            {
                return Err("reference is not complete canonical interleaved I8".into());
            }
            let _admission = model
                .preflight_generation(prompt.len(), max_tokens)
                .map_err(|e| e.to_string())?;
            let mut discarded = Vec::new();
            for _ in 0..warmups {
                discarded.push(trace(
                    &model.config,
                    &prompt,
                    max_tokens,
                    |token, cache| Ok(model.forward_one_token(token, cache)),
                )?);
            }
            let measured = trace(&model.config, &prompt, max_tokens, |token, cache| {
                Ok(model.forward_one_token(token, cache))
            })?;
            if hash_file(path)? != artifact {
                return Err("reference artifact changed during run".into());
            }
            json!({"graph": graph(&model.config), "prepared_model_payload_bytes": model.memory_bytes(),
                "warmup_runs": discarded, "measured": measured})
        } else if mode == "coordinator" {
            let model = load_low_residency_model(path, artifact).map_err(|e| e.to_string())?;
            let (plans, workers) =
                connect(Path::new(get("--workers")?), &model, artifact, partitions)?;
            let events = Events::default();
            let backend =
                VerifiedPartitionBackend::new_strict(&model, artifact, plans, workers, &events)
                    .map_err(|e| e.to_string())?;
            let run = || {
                trace(model.config(), &prompt, max_tokens, |token, cache| {
                    let mut call = blake3::Hasher::new();
                    call.update(b"ARC-low-residency-conformance-call-v1");
                    call.update(&(cache.seq_len as u64).to_le_bytes());
                    model
                        .forward_one_token_with_backend(
                            token,
                            cache,
                            Hash256(*call.finalize().as_bytes()),
                            &backend,
                        )
                        .map_err(|e| e.to_string())
                })
            };
            let mut discarded = Vec::new();
            for _ in 0..warmups {
                discarded.push(run()?);
            }
            let measured = run()?;
            let counts = events.0.lock().map_err(|_| "events lock")?;
            if counts.1 != 0 {
                return Err("strict backend reported non-answer event".into());
            }
            json!({"graph": graph(model.config()), "prepared_state_bytes": model.resident_state_bytes(),
                "row_partitions": partitions, "row_answers": counts.0, "non_answer_events": counts.1, "local_primary_rows": 0,
                "checks": "one local duplicate row per primary owner and first/middle/last spot rows per projection",
                "warmup_runs": discarded, "measured": measured})
        } else {
            return Err("unknown mode".into());
        };
        result["schema"] = json!("arc.low-residency-conformance.v1");
        result["mode"] = json!(mode);
        result["artifact_blake3"] = json!(artifact.to_hex());
        result["artifact_bytes"] = json!(path.metadata().map_err(|e| e.to_string())?.len());
        result["profile"] = json!(PROFILE);
        result["prompt_token_ids"] = json!(prompt);
        result["max_tokens"] = json!(max_tokens);
        result["generation_semantics"] =
            json!("generation-v2/BOS-once/repetition-penalty/EOS-included");
        result["warmup_count"] = json!(warmups);
        result["binary_blake3"] =
            json!(hash_file(&std::env::current_exe().map_err(|e| e.to_string())?)?.to_hex());
        result["fast_kernel_enabled"] =
            json!(arc_inference::canonical_simd::fast_canonical_kernel_enabled());
        result["simd_available"] = json!(arc_inference::canonical_simd::dotprod_available());
        result["elapsed_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
        result["max_rss_bytes_process_only"] = json!(max_rss_bytes());
        result["scope"] = json!(
            "offline numerical comparison; excludes node admission, SSH, distributed performance, quality and production readiness"
        );
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .map_err(|e| e.to_string())?;
        serde_json::to_writer_pretty(&mut file, &result).map_err(|e| e.to_string())?;
        file.write_all(b"\n").map_err(|e| e.to_string())?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use arc_inference::cached_integer_model::{
            ArithmeticProfile, I8Weights, matmul_i8_canonical_row_range,
        };
        use arc_inference::tensor_parallel::{ProjectionBackend, RowShard, project_from_shards};

        struct TinySource {
            config: ModelConfig,
            weights: BTreeMap<(Option<usize>, TensorKey), I8Weights>,
        }
        impl TinySource {
            fn new() -> Self {
                let config = fixture_config();
                let keys = [
                    TensorKey::Wq,
                    TensorKey::Wk,
                    TensorKey::Wv,
                    TensorKey::Wo,
                    TensorKey::WGate,
                    TensorKey::WUp,
                    TensorKey::WDown,
                ]
                .into_iter()
                .map(|tensor| (Some(0), tensor))
                .chain(std::iter::once((None, TensorKey::LmHead)));
                let weights = keys
                    .map(|key| {
                        // Uneven intervals, positive/negative inputs and canonical
                        // integer arithmetic; these are tiny projection fixtures.
                        let rows = if key.0.is_none() { 5 } else { 7 };
                        let values = (0..rows * 2)
                            .map(|i| (i as f32 - 5.0) / 7.0)
                            .collect::<Vec<_>>();
                        (key, I8Weights::quantize_f32(&values, rows, 2))
                    })
                    .collect();
                Self { config, weights }
            }
        }
        impl CanonicalRowSource for TinySource {
            fn config(&self) -> &ModelConfig {
                &self.config
            }
            fn canonical_execution_profile(&self) -> Option<&'static str> {
                Some(PROFILE)
            }
            fn projection_shape(
                &self,
                layer: Option<usize>,
                tensor: TensorKey,
            ) -> Option<(usize, usize)> {
                self.weights
                    .get(&(layer, tensor))
                    .map(|w| (w.n_rows, w.n_cols))
            }
            fn projection_rows(
                &self,
                layer: Option<usize>,
                tensor: TensorKey,
                start: usize,
                end: usize,
                input: &[i64],
            ) -> Result<Vec<i64>, TensorParallelError> {
                let weights = self
                    .weights
                    .get(&(layer, tensor))
                    .ok_or(TensorParallelError::WrongIdentity)?;
                if start >= end || end > weights.n_rows {
                    return Err(TensorParallelError::WrongShape);
                }
                let mut values = vec![0; end - start];
                matmul_i8_canonical_row_range(weights, start, end, input, &mut values)
                    .map_err(|_| TensorParallelError::WrongShape)?;
                Ok(values)
            }
        }
        fn entries(source: &TinySource, count: usize) -> Vec<Worker> {
            (0..count)
                .map(|rank| {
                    let id = format!("proof-{rank:03}");
                    Worker {
                        worker_id: id.clone(),
                        socket: format!("/unused/{id}.sock"),
                        assignments: source
                            .weights
                            .iter()
                            .map(|(&(layer, tensor), weights)| {
                                let (row_start, row_end) =
                                    RowPartition { rank, count }.range(weights.n_rows).unwrap();
                                RowAssignment {
                                    artifact_id: Hash256([7; 32]),
                                    execution_profile: PROFILE.into(),
                                    layer,
                                    tensor,
                                    row_start,
                                    row_end,
                                    worker_id: id.clone(),
                                }
                            })
                            .collect(),
                    }
                })
                .collect()
        }

        #[test]
        fn partial_manifests_refuse_gaps_overlap_missing_head_and_wrong_identity() {
            let source = TinySource::new();
            for case in 0..9 {
                let mut workers = entries(&source, 2);
                match case {
                    0 => workers[1].assignments[0].row_start += 1,
                    1 => workers[1].assignments[0].row_start -= 1,
                    2 => workers[0]
                        .assignments
                        .retain(|a| a.tensor != TensorKey::LmHead),
                    3 => workers[0].assignments[0].worker_id = "wrong".into(),
                    4 => workers[0].assignments[0].artifact_id = Hash256::ZERO,
                    5 => workers[0].assignments[0].execution_profile = "wrong".into(),
                    6 => workers[1].socket = workers[0].socket.clone(),
                    7 => workers[1].worker_id = workers[0].worker_id.clone(),
                    8 => {
                        let duplicate = workers[0].assignments[0].clone();
                        workers[0].assignments.push(duplicate);
                    }
                    _ => unreachable!(),
                }
                assert!(
                    plans_from_entries(&workers, &source, Hash256([7; 32]), Some(2)).is_err(),
                    "case {case}"
                );
            }
            // Whole-layer input cannot be reported as a two-owner proof.
            assert!(
                plans_from_entries(&entries(&source, 1), &source, Hash256([7; 32]), Some(2))
                    .is_err()
            );
            assert!(
                plans_from_entries(&entries(&source, 1), &source, Hash256([7; 32]), None).is_ok()
            );
        }

        struct HeldWorker {
            id: String,
            shards: Vec<RowShard>,
            corrupt: bool,
        }
        impl RowWorker for HeldWorker {
            fn project(
                &self,
                request: RowProjectionRequest,
            ) -> Result<RowProjectionResponse, TensorParallelError> {
                if request.assignment.worker_id != self.id {
                    return Err(TensorParallelError::WrongIdentity);
                }
                let mut values = project_from_shards(&self.shards, &request)?;
                if self.corrupt {
                    values[0] += 1;
                }
                Ok(RowProjectionResponse {
                    call_id: request.call_id,
                    input_hash: request.input_hash,
                    assignment: request.assignment,
                    values,
                })
            }
        }

        #[test]
        fn distinct_disjoint_workers_match_canonical_rows_and_faults_refuse() {
            let source = TinySource::new();
            let entries = entries(&source, 2);
            for corrupt in [false, true] {
                let plans =
                    plans_from_entries(&entries, &source, Hash256([7; 32]), Some(2)).unwrap();
                for plan in plans.values() {
                    assert_eq!(
                        plan.slices
                            .iter()
                            .filter(|s| s.duplicate_on == Some(SliceOwner::Local))
                            .count(),
                        2
                    );
                }
                let workers: Workers = entries
                    .iter()
                    .map(|entry| {
                        let shards = entry
                            .assignments
                            .iter()
                            .map(|a| RowShard {
                                artifact: a.artifact_id,
                                profile: a.execution_profile.clone(),
                                layer: a.layer,
                                tensor: a.tensor,
                                row_start: a.row_start,
                                row_end: a.row_end,
                                weights: source.weights[&(a.layer, a.tensor)]
                                    .copy_rows(a.row_start, a.row_end)
                                    .unwrap(),
                            })
                            .collect();
                        (
                            entry.worker_id.clone(),
                            Arc::new(HeldWorker {
                                id: entry.worker_id.clone(),
                                shards,
                                corrupt,
                            }) as Arc<dyn RowWorker>,
                        )
                    })
                    .collect();
                let events = Events::default();
                let backend = VerifiedPartitionBackend::new_strict(
                    &source,
                    Hash256([7; 32]),
                    plans,
                    workers,
                    &events,
                )
                .unwrap();
                for (&(layer, tensor), weights) in &source.weights {
                    let input = [65536, -32768];
                    let actual = backend.project_rows(
                        Hash256([9; 32]),
                        layer,
                        tensor,
                        &input,
                        weights.n_rows,
                    );
                    if corrupt {
                        assert!(
                            actual.is_err(),
                            "faulty primary must not become local success"
                        );
                    } else {
                        assert_eq!(
                            actual.unwrap(),
                            source
                                .projection_rows(layer, tensor, 0, weights.n_rows, &input)
                                .unwrap()
                        );
                    }
                }
            }
        }

        #[test]
        fn inspector_partial_layout_includes_every_head_and_respects_slice_bound() {
            let source = TinySource::new();
            let plan = inspect(&source, Some(3)).unwrap();
            assert_eq!(plan["bundles"].as_array().unwrap().len(), 3);
            for (rank, bundle) in plan["bundles"].as_array().unwrap().iter().enumerate() {
                assert_eq!(bundle["include_output"], true);
                assert_eq!(bundle["row_partition"], format!("{rank}/3"));
            }
            assert!(
                inspect(&source, Some(6)).is_err(),
                "LMHead has only five rows"
            );
            for invalid in ["0", "1", "17", "33", "oops"] {
                assert!(proof_partition_count(Some(invalid)).is_err());
            }
            assert_eq!(proof_partition_count(Some("16")).unwrap(), Some(16));
            assert!(
                inspect(&source, None).unwrap()["bundles"][0]
                    .get("row_partition")
                    .is_none()
            );
            // Metadata-only capacity preflight: no allocation of these
            // advertised columns and no export can begin on this plan.
            let mut oversized = TinySource::new();
            for weights in oversized.weights.values_mut() {
                weights.n_cols = MAX_CANONICAL_ROW_FILE_BYTES;
            }
            assert!(inspect(&oversized, Some(3)).unwrap_err().contains("1 GiB"));
        }

        fn fixture_config() -> ModelConfig {
            ModelConfig {
                n_layers: 1,
                d_model: 2,
                n_heads: 1,
                n_kv_heads: 1,
                d_ff: 2,
                d_head: 2,
                d_kv: 2,
                vocab_size: 5,
                attn_scale: 65536,
                rope_cos: vec![],
                rope_sin: vec![],
                max_seq: 8,
                eos_tokens: vec![2],
                bos_token: 1,
                chat_template: String::new(),
                arithmetic_profile: ArithmeticProfile::GgufInterleavedRowsV1,
            }
        }

        #[test]
        fn trace_uses_bos_once_repetition_penalty_and_eos_without_extra_forward() {
            let config = fixture_config();
            let forward = |token: u32, cache: &mut KVCache| {
                cache.push_k(0, &[i64::from(token), 0]);
                cache.push_v(0, &[0, i64::from(token)]);
                cache.seq_len += 1;
                Ok(vec![0, 0, 110, 120, 0])
            };
            let result = trace(&config, &[4], 2, forward).unwrap();
            assert_eq!(result.output_token_ids, vec![3, 2]);
            assert_eq!(
                result
                    .positions
                    .iter()
                    .map(|p| p.input_token)
                    .collect::<Vec<_>>(),
                vec![1, 4, 3]
            );
            assert!(
                result
                    .positions
                    .windows(2)
                    .all(|p| p[0].logits_blake3_le_i64 == p[1].logits_blake3_le_i64)
            );
            assert_ne!(result.positions[0].kv_blake3, result.positions[1].kv_blake3);
            let expected: Vec<u8> = [3u32, 2].into_iter().flat_map(u32::to_le_bytes).collect();
            assert_eq!(
                result.output_hash,
                arc_crypto::hash_bytes(&expected).to_hex()
            );
            // Generation-v2 forwards the final selected non-EOS token even
            // after consuming its one-token output budget.
            let one = trace(&config, &[4], 1, forward).unwrap();
            assert_eq!(one.output_token_ids, vec![3]);
            assert_eq!(one.positions.len(), 3);
        }

        #[test]
        fn trace_refuses_prompt_or_context_before_forward_and_bad_forward_shape() {
            let config = fixture_config();
            for prompt in [vec![], vec![1], vec![5], vec![4; 8]] {
                assert!(
                    trace(&config, &prompt, 2, |_, _| panic!(
                        "invalid input forwarded"
                    ))
                    .is_err()
                );
            }
            assert!(
                trace(&config, &[4], 2, |_, cache| {
                    cache.seq_len += 1;
                    Ok(vec![0])
                })
                .is_err()
            );
            assert!(trace(&config, &[4], 2, |_, _| Ok(vec![0; 5])).is_err());
        }
    }
}

#[cfg(unix)]
fn main() -> Result<(), String> {
    unix::main()
}
#[cfg(not(unix))]
fn main() -> Result<(), String> {
    Err("offline row conformance requires Unix sockets".into())
}
