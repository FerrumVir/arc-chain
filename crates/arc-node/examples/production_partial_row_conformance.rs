//! Offline integration gate for the actual private production RowCohort.
//! No activation, qualification flag, chain clock, payment, or public worker.

use arc_crypto::Hash256;
use arc_inference::cached_integer_model::GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE as PROFILE;
use arc_inference::low_residency::{CanonicalRowSource, load_low_residency_model};
use arc_node::row_cohort::{CohortRecord, CohortView, RowCohort, RowCohortConfig};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

// An offline, constant observation supplied to the real placement API. It is
// never a claimed chain height and does not age a simulated online history.
const OFFLINE_HEIGHT: u64 = 1;
const MAX_METADATA_BYTES: u64 = 1 << 20;

fn bounded_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_METADATA_BYTES {
        return Err("metadata must be a regular file of 1 byte to 1 MiB".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err("metadata exceeds bound".into());
    }
    Ok(bytes)
}

fn hash_file(path: &Path) -> Result<Hash256, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(Hash256(*hasher.finalize().as_bytes()))
}

fn pin_config(path: &Path) -> Result<Value, String> {
    let mut config: RowCohortConfig =
        serde_json::from_slice(&bounded_bytes(path)?).map_err(|e| e.to_string())?;
    let partial = config
        .partial_rows
        .as_mut()
        .ok_or("partial_rows is required")?;
    if partial.manifests.is_empty() || partial.manifests.len() > 32 {
        return Err("manifest count is outside 1..=32".into());
    }
    for pin in &mut partial.manifests {
        pin.blake3 = Hash256(*blake3::hash(&bounded_bytes(&pin.path)?).as_bytes());
    }
    config.validate()?;
    serde_json::to_value(config).map_err(|e| e.to_string())
}

fn require_view(view: &CohortView, owners: &BTreeSet<String>) -> Result<(), String> {
    if !view.low_residency
        || !view.partial_row_residency
        || view.local_fallback_enabled
        || view.placement_timing_prediction_available
        || view.coordinator_macs_per_s != 0
        || view.machines.len() != owners.len()
        || view
            .machines
            .iter()
            .map(|m| m.id.clone())
            .collect::<BTreeSet<_>>()
            != *owners
        || view.machines.iter().any(|m| {
            !m.connected
                || m.excluded
                || m.measured_macs_per_s.is_none_or(|rate| rate == 0)
                || m.free_slots == 0
        })
        || view.reservations != 0
    {
        return Err(
            "production cohort lacks complete measured remote ownership or released reservations"
                .into(),
        );
    }
    Ok(())
}

fn require_record(
    record: &CohortRecord,
    request: Hash256,
    owners: &BTreeSet<String>,
) -> Result<(), String> {
    let certificate = record
        .certificate
        .as_deref()
        .ok_or("production run has no certificate")?;
    let certificate = Hash256::from_hex(certificate).map_err(|e| e.to_string())?;
    if certificate == Hash256::ZERO
        || record.request_id != request.to_hex()
        || record.machines.len() != owners.len()
        || record.machines.iter().cloned().collect::<BTreeSet<_>>() != *owners
        || record.answered == 0
        || record.fallbacks != 0
        || record.skipped != 0
        || record.faults != 0
        || !record
            .outcome
            .starts_with("partitioned fixed resident rows;")
    {
        return Err(
            "production run did not certify and execute all remote owners without fallback/faults"
                .into(),
        );
    }
    Ok(())
}

fn run(values: &BTreeMap<&str, &str>) -> Result<Value, String> {
    let get = |key| {
        values
            .get(key)
            .copied()
            .ok_or_else(|| format!("required {key}"))
    };
    let artifact = Hash256::from_hex(get("--artifact")?).map_err(|e| e.to_string())?;
    let prompt: Vec<u32> = get("--prompt-ids")?
        .split(',')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| "invalid prompt IDs")?;
    let max_tokens: u32 = get("--max-tokens")?
        .parse()
        .map_err(|_| "invalid token limit")?;
    let warmups: usize = values
        .get("--warmups")
        .unwrap_or(&"0")
        .parse()
        .map_err(|_| "invalid warmups")?;
    let timeout: u64 = values
        .get("--readiness-timeout-seconds")
        .unwrap_or(&"180")
        .parse()
        .map_err(|_| "invalid readiness timeout")?;
    if prompt.is_empty()
        || prompt.len() > 16
        || !(1..=4).contains(&max_tokens)
        || warmups > 1
        || !(1..=300).contains(&timeout)
    {
        return Err("prompt/token/warmup/readiness bounds".into());
    }
    let config = RowCohortConfig::load(Path::new(get("--cohort")?))?;
    if config.partial_rows.is_none() {
        return Err("explicit partial_rows required".into());
    }
    let owners = config
        .workers
        .iter()
        .map(|w| w.id.clone())
        .collect::<BTreeSet<_>>();
    let model = Arc::new(
        load_low_residency_model(Path::new(get("--model")?), artifact)
            .map_err(|e| e.to_string())?,
    );
    let graph = model.config().clone();
    if model.canonical_execution_profile() != Some(PROFILE)
        || prompt[0] == graph.bos_token
        || prompt
            .iter()
            .any(|&token| token as usize >= graph.vocab_size)
        || 1 + prompt.len() + max_tokens as usize > graph.max_seq
    {
        return Err("model profile, prompt or context bounds".into());
    }
    let resident_bytes = model.resident_state_bytes();
    let started = Instant::now();
    let validator = arc_crypto::hash_bytes(b"ARC-offline-production-cohort-validator-v1");
    let cohort =
        RowCohort::connect_low_residency(config, validator, model, artifact, OFFLINE_HEIGHT)?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while !cohort.ready_for_requests(OFFLINE_HEIGHT) {
        if Instant::now() >= deadline {
            return Err(format!(
                "production cohort readiness timed out: {:?}",
                cohort.view()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let ready = cohort.view();
    require_view(&ready, &owners)?;
    let readiness_ms = started.elapsed().as_millis();
    let mut runs = Vec::new();
    for index in 0..=warmups {
        let mut identity = b"ARC-offline-production-cohort-request-v1".to_vec();
        identity.extend_from_slice(artifact.as_bytes());
        identity.extend_from_slice(&(index as u64).to_le_bytes());
        let request = arc_crypto::hash_bytes(&identity);
        let started = Instant::now();
        let (tokens, output_hash) = cohort
            .generate(
                request,
                cohort.assignment_hash(),
                OFFLINE_HEIGHT + 120,
                OFFLINE_HEIGHT,
                &prompt,
                max_tokens,
                &graph.eos_tokens,
            )
            .map_err(|e| e.to_string())?;
        let view = cohort.view();
        require_view(&view, &owners)?;
        let record = view
            .recent
            .last()
            .ok_or("missing production cohort record")?;
        require_record(record, request, &owners)?;
        runs.push(
            json!({"output_token_ids": tokens, "output_hash": output_hash.to_hex(),
            "elapsed_ms": started.elapsed().as_millis(), "cohort_record": record}),
        );
    }
    let measured = runs.pop().ok_or("missing measured run")?;
    Ok(json!({
        "schema": "arc.production-partial-cohort-conformance.v1", "mode": "production-cohort",
        "scope": "offline production cohort integration over loopback SSH; no activation, payment, quality or WAN claim",
        "qualification_flags_set": false, "chain_clock_used": false, "offline_observation_height": OFFLINE_HEIGHT,
        "artifact_blake3": artifact.to_hex(), "artifact_bytes": std::fs::metadata(get("--model")?).map_err(|e| e.to_string())?.len(),
        "profile": PROFILE, "prompt_token_ids": prompt, "max_tokens": max_tokens, "warmup_count": warmups,
        "generation_semantics": "generation-v2/BOS-once/repetition-penalty/EOS-included",
        "graph": {"bos":graph.bos_token,"eos":graph.eos_tokens,"vocab":graph.vocab_size,"max_seq":graph.max_seq},
        "binary_blake3": hash_file(&std::env::current_exe().map_err(|e| e.to_string())?)?.to_hex(),
        "assignment_hash": cohort.assignment_hash().to_hex(), "prepared_state_bytes": resident_bytes,
        "readiness_ms": readiness_ms, "ready_view": ready, "final_view": cohort.view(),
        "warmup_runs": runs, "measured": measured,
        "fast_kernel_requested": std::env::var("ARC_FAST_CANONICAL_KERNEL").as_deref() == Ok("1"),
        "fast_kernel_enabled": arc_inference::canonical_simd::fast_canonical_kernel_enabled(),
        "simd_available": arc_inference::canonical_simd::dotprod_available(),
        "simd_projection_census": if std::env::var("ARC_QUALIFICATION_SIMD_CENSUS").as_deref() == Ok("1") {
            json!(arc_inference::canonical_simd::projection_census())
        } else { Value::Null },
    }))
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().ok_or("required mode pin-config or run")?;
    let mut values = BTreeMap::new();
    for pair in args[1..].chunks(2) {
        if pair.len() != 2
            || ![
                "--model",
                "--artifact",
                "--cohort",
                "--prompt-ids",
                "--max-tokens",
                "--warmups",
                "--output",
                "--readiness-timeout-seconds",
            ]
            .contains(&pair[0].as_str())
            || values.insert(pair[0].as_str(), pair[1].as_str()).is_some()
        {
            return Err("unknown, duplicate or valueless argument".into());
        }
    }
    let output = Path::new(values.get("--output").ok_or("required --output")?);
    if output.exists() {
        return Err("output already exists".into());
    }
    if std::env::var("ARC_QUALIFICATION_SIMD_CENSUS").as_deref() == Ok("1") {
        arc_inference::canonical_simd::reset_projection_census();
        arc_inference::canonical_simd::set_projection_census_enabled(true);
    }
    let value = match mode.as_str() {
        "pin-config" => pin_config(Path::new(
            values.get("--cohort").ok_or("required --cohort")?,
        ))?,
        "run" => run(&values)?,
        _ => return Err("unknown mode".into()),
    };
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_node::row_cohort::MachineView;

    fn owners() -> BTreeSet<String> {
        ["a".into(), "b".into()].into()
    }
    fn view() -> CohortView {
        CohortView {
            low_residency: true,
            partial_row_residency: true,
            local_fallback_enabled: false,
            placement_timing_prediction_available: false,
            coordinator_macs_per_s: 0,
            machines: owners()
                .into_iter()
                .map(|id| MachineView {
                    id,
                    connected: true,
                    measured_macs_per_s: Some(10),
                    excluded: false,
                    free_slots: 1,
                })
                .collect(),
            epoch: 0,
            challenges: 2,
            probes: 2,
            reservations: 0,
            exclusions: 0,
            recent: vec![],
        }
    }
    fn record() -> CohortRecord {
        CohortRecord {
            request_id: Hash256([1; 32]).to_hex(),
            height: OFFLINE_HEIGHT,
            certificate: Some(Hash256([2; 32]).to_hex()),
            machines: owners().into_iter().collect(),
            predicted_token_us: 0,
            coordinator_only_token_us: 0,
            answered: 2,
            fallbacks: 0,
            skipped: 0,
            faults: 0,
            elapsed_ms: 1,
            outcome: "partitioned fixed resident rows; canonical checks".into(),
        }
    }
    #[test]
    fn missing_measurements_fallback_reservations_and_incomplete_records_cannot_pass() {
        require_view(&view(), &owners()).unwrap();
        require_record(&record(), Hash256([1; 32]), &owners()).unwrap();
        for change in 0..6 {
            let mut value = view();
            match change {
                0 => {
                    value.machines.pop();
                }
                1 => value.machines[0].measured_macs_per_s = None,
                2 => value.machines[0].connected = false,
                3 => value.machines[0].excluded = true,
                4 => value.reservations = 1,
                _ => value.local_fallback_enabled = true,
            }
            assert!(require_view(&value, &owners()).is_err());
        }
        for change in 0..6 {
            let mut value = record();
            match change {
                0 => value.certificate = None,
                1 => {
                    value.machines.pop();
                }
                2 => value.answered = 0,
                3 => value.fallbacks = 1,
                4 => value.skipped = 1,
                _ => value.faults = 1,
            }
            assert!(require_record(&value, Hash256([1; 32]), &owners()).is_err());
        }
    }
    #[test]
    fn metadata_hashing_is_bounded_and_derivation_does_not_need_a_model() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("manifest.json");
        let bytes = b"unqualified fixture metadata";
        std::fs::write(&manifest, bytes).unwrap();
        let draft = json!({"max_workers":1,"duplicate_per_mille":50,"spot_rows_per_stage":2,
            "workers":[{"id":"a","ssh_program":"/usr/bin/ssh","target":"fixture.invalid",
                "known_hosts":"/etc/arc/fixture-hosts","remote_command":["fixture"],
                "timeout_ms":1000,"ram_headroom_bytes":1000,"max_concurrency":1}],
            "partial_rows":{"format":"arc.private-row-residency.v1","manifests":[
                {"worker_id":"a","path":manifest,"blake3":Hash256::ZERO.to_hex()}]}});
        let path = directory.path().join("draft.json");
        std::fs::write(&path, serde_json::to_vec(&draft).unwrap()).unwrap();
        let pinned = pin_config(&path).unwrap();
        assert_eq!(
            pinned["partial_rows"]["manifests"][0]["blake3"],
            blake3::hash(bytes).to_hex().to_string()
        );
        assert!(pinned.get("reference_generation_qualified").is_none());
        std::fs::write(&manifest, vec![0; MAX_METADATA_BYTES as usize + 1]).unwrap();
        assert!(pin_config(&path).is_err());
        std::fs::remove_file(&manifest).unwrap();
        assert!(pin_config(&path).is_err());
    }
}
