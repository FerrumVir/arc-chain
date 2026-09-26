//! One query, different pieces, assigned automatically - on ONE host.
//!
//! LABEL: LOCAL, IN-PROCESS WORKERS, SIMULATED LINKS. Every worker is a
//! distinct signing identity with its own lease, measured rate and slices,
//! but all of them run in this process against the coordinator's loaded
//! model, and their links are synthetic. This exercises the assignment path
//! end to end on real canonical-I8 weights; it is NOT a multi-machine or WAN
//! result, and it does not change S10 (the original single-query comparison
//! across machines), which stays FAIL until a real distributed run passes.
//!
//! What it does, in order:
//! 1. Each worker signs a capability lease (arc-assign S1). One worker may
//!    claim a false rate (`--dishonest-last`).
//! 2. The coordinator runs a row-projection challenge on each worker and
//!    checks the result; the measured rate replaces the claim, and a claim
//!    the measurement does not support is refused (S1).
//! 3. Links are summarised from synthetic probes, marked `simulated` (S2).
//! 4. Placement splits every projection of every layer by measured rate and
//!    link cost (S3); an assignment certificate binds it, and any validator
//!    can recompute it (S4).
//! 5. The partitioned backend runs a real generation with those slices (S5).
//!    Every forward is compared exactly with the unpartitioned model
//!    (full-model equivalence), and the verification plan's spot rows and
//!    duplicate slices are recomputed and compared (S8).
//!
//! Usage (heavy: loads the canonical 7B model; run only in a model window):
//!   cargo run --release -p arc-node --example assigned_partition_local -- \
//!     GGUF [--workers 3] [--slowdown 1,2,4] [--tokens 4] [--prompt TEXT] \
//!     [--rtt-us 200] [--bandwidth-mbps 10000] [--dishonest-last] [--churn] [--out FILE]
//!
//! `--churn` (S7): one placed worker fails partway through the query. The
//! query aborts - the backend never falls back to another participant or to
//! local rows on its own - and the coordinator re-places the query without
//! that worker under a new certificate (epoch 1), then runs it again from
//! empty KV caches. Row workers hold no KV state (the coordinator does), so
//! no cache history can mix across the reassignment.

use arc_assign::certificate::{AssignmentCertificate, policy_hash};
use arc_assign::lease::{
    CapabilityLease, ChallengeResult, LeaseBody, LeaseRequirements, OP_ROW_PROJECTION_I8,
    check_challenge, validate,
};
use arc_assign::link::{Probe, summarize};
use arc_assign::placement::{Candidate, Participant, Policy, Stage};
use arc_assign::verify::{VerificationRule, plan};
use arc_crypto::{Hash256, KeyPair, hash_bytes};
use arc_inference::cached_integer_model::{CachedIntegerModel, I8Weights, KVCache};
use arc_inference::tensor_parallel::{
    ModelRowWorker, PartitionedProjectionBackend, RowAssignment, RowProjectionRequest,
    RowProjectionResponse, RowWorker, TensorKey, TensorParallelError, hash_i64,
};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LABEL: &str =
    "LOCAL, IN-PROCESS WORKERS, SIMULATED LINKS - not a multi-machine or WAN result";
const CHALLENGE_ROWS: usize = 256;
const MIN_MEASURED_PERCENT: u64 = 50;

struct Args {
    gguf: String,
    workers: usize,
    slowdown: Vec<u32>,
    tokens: usize,
    prompt: String,
    rtt_us: u64,
    bandwidth_mbps: u64,
    dishonest_last: bool,
    churn: bool,
    out: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut raw = std::env::args().skip(1);
    let gguf = raw
        .next()
        .ok_or("usage: assigned_partition_local GGUF [options]")?;
    let mut args = Args {
        gguf,
        workers: 3,
        slowdown: vec![1, 2, 4],
        tokens: 4,
        prompt: "The capital of France is".into(),
        rtt_us: 200,
        bandwidth_mbps: 10_000,
        dishonest_last: false,
        churn: false,
        out: None,
    };
    while let Some(flag) = raw.next() {
        let mut value = || raw.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--workers" => {
                args.workers = value()?.parse().map_err(|e| format!("--workers: {e}"))?
            }
            "--slowdown" => {
                args.slowdown = value()?
                    .split(',')
                    .map(|f| {
                        f.trim()
                            .parse::<u32>()
                            .map_err(|e| format!("--slowdown: {e}"))
                    })
                    .collect::<Result<_, _>>()?
            }
            "--tokens" => args.tokens = value()?.parse().map_err(|e| format!("--tokens: {e}"))?,
            "--prompt" => args.prompt = value()?,
            "--rtt-us" => args.rtt_us = value()?.parse().map_err(|e| format!("--rtt-us: {e}"))?,
            "--bandwidth-mbps" => {
                args.bandwidth_mbps = value()?
                    .parse()
                    .map_err(|e| format!("--bandwidth-mbps: {e}"))?
            }
            "--dishonest-last" => args.dishonest_last = true,
            "--churn" => args.churn = true,
            "--out" => args.out = Some(value()?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if args.workers == 0 || args.workers > 8 {
        return Err("--workers must be 1..=8".into());
    }
    if args.slowdown.contains(&0) {
        return Err("--slowdown factors must be at least 1".into());
    }
    Ok(args)
}

fn file_hash(path: &str) -> Result<Hash256, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Hash256(*hasher.finalize().as_bytes()))
}

fn tensor_name(tensor: TensorKey) -> &'static str {
    match tensor {
        TensorKey::Wq => "wq",
        TensorKey::Wk => "wk",
        TensorKey::Wv => "wv",
        TensorKey::Wo => "wo",
        TensorKey::WGate => "w_gate",
        TensorKey::WUp => "w_up",
        TensorKey::WDown => "w_down",
        TensorKey::LmHead => "lm_head",
    }
}

/// Every projection the canonical forward dispatches, in forward order.
fn projections(model: &CachedIntegerModel) -> Vec<(Option<usize>, TensorKey, &I8Weights)> {
    let mut out = Vec::new();
    for (index, layer) in model.layers.iter().enumerate() {
        for (tensor, weights) in [
            (TensorKey::Wq, &layer.wq),
            (TensorKey::Wk, &layer.wk),
            (TensorKey::Wv, &layer.wv),
            (TensorKey::Wo, &layer.wo),
            (TensorKey::WGate, &layer.w_gate),
            (TensorKey::WUp, &layer.w_up),
            (TensorKey::WDown, &layer.w_down),
        ] {
            out.push((Some(index), tensor, weights));
        }
    }
    out.push((None, TensorKey::LmHead, &model.output_weight));
    out
}

/// A participant's compute: rows done, time spent, and the latest
/// (input, output) per assignment for the verification plan.
#[derive(Default)]
struct Ledger {
    rows: BTreeMap<String, u64>,
    busy: BTreeMap<String, Duration>,
    latest: BTreeMap<String, (Vec<i64>, Vec<i64>)>,
}

/// A worker identity computing one assignment, slowed by `slowdown` to stand
/// in for a slower device. It records what it computed.
struct AccountedWorker {
    participant: String,
    slowdown: u32,
    inner: ModelRowWorker,
    ledger: Arc<Mutex<Ledger>>,
    /// Calls this participant answers before it leaves (churn), shared by
    /// all of its slices. `None`: it never leaves.
    leaves_after: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl RowWorker for AccountedWorker {
    fn project(
        &self,
        request: RowProjectionRequest,
    ) -> Result<RowProjectionResponse, TensorParallelError> {
        if let Some(remaining) = &self.leaves_after {
            let left = remaining
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_err();
            if left {
                return Err(TensorParallelError::Worker(format!(
                    "{} left mid-query (simulated churn)",
                    self.participant
                )));
            }
        }
        let input = request.input.clone();
        let start = Instant::now();
        let response = self.inner.project(request)?;
        let computed = start.elapsed();
        if self.slowdown > 1 {
            std::thread::sleep(computed * (self.slowdown - 1));
        }
        let mut ledger = self.ledger.lock();
        *ledger.rows.entry(self.participant.clone()).or_default() += response.values.len() as u64;
        *ledger.busy.entry(self.participant.clone()).or_default() += start.elapsed();
        ledger.latest.insert(
            self.inner.worker_id.clone(),
            (input, response.values.clone()),
        );
        Ok(response)
    }
}

fn challenge_input(cols: usize) -> Vec<i64> {
    // Deterministic Q16 activations in [-2, 2).
    (0..cols)
        .map(|i| {
            let h = hash_bytes(&(i as u64).to_le_bytes());
            let raw = u32::from_le_bytes([h.0[0], h.0[1], h.0[2], h.0[3]]);
            (raw as i64 % (4 << 16)) - (2 << 16)
        })
        .collect()
}

fn participant_label(participant: &Participant) -> String {
    match participant {
        Participant::Coordinator => "coordinator".into(),
        Participant::Worker(address) => format!("worker-{}", &address.to_hex()[..12]),
    }
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    eprintln!("{LABEL}");
    let artifact = file_hash(&args.gguf)?;
    let model = Arc::new(
        arc_inference::cached_integer_model::load_cached_model_canonical_i8_interleaved_rope(
            &args.gguf,
        )
        .map_err(|e| e.to_string())?,
    );
    let profile = model
        .canonical_execution_profile()
        .ok_or("model is not the complete canonical I8 profile")?
        .to_string();
    let ledger = Arc::new(Mutex::new(Ledger::default()));

    // 1. Identities and signed leases.
    let keys: Vec<KeyPair> = (0..args.workers)
        .map(|_| KeyPair::generate_ed25519())
        .collect();
    let slowdown = |index: usize| *args.slowdown.get(index).unwrap_or(&1);
    let members: BTreeSet<[u8; 32]> = keys.iter().map(|k| k.address().0).collect();
    let challenge_layer = &model.layers[0].w_gate;
    let cols = challenge_layer.n_cols;
    let input = challenge_input(cols);
    let challenge_assignment = |worker_id: String| RowAssignment {
        artifact_id: artifact,
        execution_profile: profile.clone(),
        layer: Some(0),
        tensor: TensorKey::WGate,
        row_start: 0,
        row_end: CHALLENGE_ROWS,
        worker_id,
    };
    // The coordinator's own rate and the expected challenge answer.
    let reference = ModelRowWorker {
        worker_id: "coordinator-challenge".into(),
        assignment: challenge_assignment("coordinator-challenge".into()),
        model: model.clone(),
    };
    let run_challenge =
        |worker: &dyn RowWorker, worker_id: &str| -> Result<(Vec<i64>, Duration), String> {
            let request = RowProjectionRequest {
                call_id: hash_bytes(worker_id.as_bytes()),
                input_hash: hash_i64(&input),
                assignment: challenge_assignment(worker_id.to_string()),
                input: input.clone(),
            };
            let start = Instant::now();
            let response = worker.project(request).map_err(|e| e.to_string())?;
            Ok((response.values, start.elapsed()))
        };
    let (expected, coordinator_elapsed) = run_challenge(&reference, "coordinator-challenge")?;
    let macs = (CHALLENGE_ROWS * cols) as u64;
    let coordinator_rate = ChallengeResult {
        macs,
        elapsed_us: coordinator_elapsed.as_micros().max(1) as u64,
        correct: true,
    }
    .measured_macs_per_s();

    let mut candidates = Vec::new();
    let mut lease_digests = Vec::new();
    let mut lease_records = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        let worker_id = format!("challenge-{index}");
        let worker = AccountedWorker {
            participant: format!("challenge-{index}"),
            slowdown: slowdown(index),
            inner: ModelRowWorker {
                worker_id: worker_id.clone(),
                assignment: challenge_assignment(worker_id.clone()),
                model: model.clone(),
            },
            ledger: Arc::new(Mutex::new(Ledger::default())),
            leaves_after: None,
        };
        let (values, elapsed) = run_challenge(&worker, &worker_id)?;
        let measured = ChallengeResult {
            macs,
            elapsed_us: elapsed.as_micros().max(1) as u64,
            correct: values == expected,
        };
        // An honest claim is what the worker believes it does: its measured
        // rate. The dishonest one claims ten times that.
        let claimed = if args.dishonest_last && index + 1 == keys.len() {
            measured.measured_macs_per_s().saturating_mul(10)
        } else {
            measured.measured_macs_per_s()
        };
        let body = LeaseBody {
            version: 1,
            worker: key.address(),
            transport_id: format!("in-process:{index}"),
            artifact_id: artifact,
            execution_profile: profile.clone(),
            backend: "cpu-integer-in-process".into(),
            kernel_set: hash_bytes(b"scalar-canonical-rows"),
            operators: BTreeSet::from([OP_ROW_PROJECTION_I8.to_string()]),
            ram_headroom_bytes: 64 << 30,
            claimed_macs_per_s: claimed,
            max_concurrency: 1,
            warm_rows: Vec::new(),
            resident_layers: Vec::new(),
            issued_at_height: 0,
            expires_at_height: 1_000,
            nonce: index as u64,
        };
        let lease = CapabilityLease::sign(body, key).map_err(|e| e.to_string())?;
        let validity = validate(
            &lease,
            &LeaseRequirements {
                members: &members,
                artifact_id: artifact,
                execution_profile: &profile,
                operator: OP_ROW_PROJECTION_I8,
                height: 1,
            },
        );
        let checked = validity.map_err(|e| e.to_string()).and_then(|()| {
            check_challenge(&lease, &measured, MIN_MEASURED_PERCENT).map_err(|e| e.to_string())
        });
        lease_records.push(json!({
            "worker": key.address().to_hex(),
            "slowdown": slowdown(index),
            "claimed_macs_per_s": lease.body.claimed_macs_per_s,
            "measured_macs_per_s": measured.measured_macs_per_s(),
            "challenge_correct": measured.correct,
            "admitted": checked.is_ok(),
            "refusal": checked.as_ref().err(),
        }));
        let Ok(rate) = checked else { continue };
        let probes: Vec<Probe> = (0..8)
            .map(|_| Probe {
                rtt_us: args.rtt_us,
                bytes: 1 << 20,
                transfer_us: ((1u64 << 20) * 8 * 1_000_000)
                    / (args.bandwidth_mbps.max(1) * 1_000_000),
                failed: false,
            })
            .collect();
        let link = summarize(&probes, 1, true).ok_or("no link samples")?;
        lease_digests.push(lease.digest());
        candidates.push(Candidate {
            worker: key.address(),
            transport_id: lease.body.transport_id.clone(),
            macs_per_s: rate,
            ram_headroom_bytes: lease.body.ram_headroom_bytes,
            max_concurrency: lease.body.max_concurrency,
            link,
            resident_layers: lease.body.resident_layers.clone(),
        });
    }

    // 3-4. Placement over every projection, bound by a certificate.
    let stages: Vec<Stage> = projections(&model)
        .iter()
        .map(|(layer, tensor, weights)| Stage {
            layer: layer.map(|l| l as u32),
            tensor: tensor_name(*tensor).into(),
            rows: weights.n_rows as u64,
            cols: weights.n_cols as u64,
        })
        .collect();
    let policy = Policy {
        max_workers: args.workers,
        max_link_age: 1_000,
        now: 1,
        max_failure_per_mille: 50,
        allow_simulated_links: true,
        input_element_bytes: 8,
        output_element_bytes: 8,
        weight_bytes_per_element: 1,
        include_coordinator: true,
    };
    let verification = VerificationRule {
        duplicate_per_mille: 100,
        spot_rows_per_stage: 2,
    };
    let authorised = policy_hash(&policy, &verification);
    let request_id = hash_bytes(args.prompt.as_bytes());
    let certify = |epoch: u64, candidates: Vec<Candidate>, digests: Vec<Hash256>| {
        let certificate = AssignmentCertificate::issue(
            request_id,
            artifact,
            &profile,
            epoch,
            stages.clone(),
            coordinator_rate,
            candidates,
            digests,
            policy.clone(),
            verification,
        )
        .map_err(|e| e.to_string())?;
        certificate
            .verify(&authorised)
            .map_err(|e| format!("the certificate does not recompute: {e}"))?;
        Ok::<_, String>(certificate)
    };
    let key_index: BTreeMap<[u8; 32], usize> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.address().0, i))
        .collect();
    let factor_of = |participant: &Participant| match participant {
        Participant::Coordinator => 1,
        Participant::Worker(address) => {
            key_index.get(&address.0).map(|i| slowdown(*i)).unwrap_or(1)
        }
    };
    let mut prompt = vec![model.config.bos_token];
    prompt.extend(model.encode(&args.prompt));
    let context = QueryContext {
        model: &model,
        artifact,
        profile: &profile,
        ledger: ledger.clone(),
        prompt: &prompt,
        tokens: args.tokens,
    };

    let first = certify(0, candidates.clone(), lease_digests.clone())?;
    let mut churn = serde_json::Value::Null;
    let (certificate, outcome) = if args.churn {
        let leaving = *first
            .placement
            .workers
            .first()
            .ok_or("--churn needs at least one placed worker")?;
        // Leave after roughly a quarter of the first forward's projections.
        let leaves_after = (first.placement.stages.len() / 4).max(1);
        let aborted = context.run(&first, &factor_of, Some((leaving, leaves_after)));
        let error = match aborted {
            Err(error) => error,
            Ok(_) => return Err("the query completed although a worker left".into()),
        };
        let remaining: Vec<(Candidate, Hash256)> = candidates
            .iter()
            .cloned()
            .zip(lease_digests.iter().copied())
            .filter(|(candidate, _)| candidate.worker != leaving)
            .collect();
        let second = certify(
            1,
            remaining.iter().map(|(c, _)| c.clone()).collect(),
            remaining.iter().map(|(_, d)| *d).collect(),
        )?;
        if second.placement.workers.contains(&leaving) {
            return Err("the re-placement still names the worker that left".into());
        }
        churn = json!({
            "left": leaving.to_hex(),
            "left_after_calls": leaves_after,
            "first_query": "aborted, no fallback",
            "error": error,
            "first_certificate": first.hash().to_hex(),
            "reassigned_workers": second.placement.workers.iter().map(|w| w.to_hex()).collect::<Vec<_>>(),
        });
        let outcome = context.run(&second, &factor_of, None)?;
        (second, outcome)
    } else {
        let outcome = context.run(&first, &factor_of, None)?;
        (first, outcome)
    };

    let placement = &certificate.placement;
    let ledger = ledger.lock();
    let participants: Vec<_> = ledger
        .rows
        .iter()
        .map(|(name, rows)| {
            json!({
                "participant": name,
                "rows_computed": rows,
                "busy_ms": ledger.busy.get(name).map(|d| d.as_millis()).unwrap_or(0),
            })
        })
        .collect();
    let summary = json!({
        "label": LABEL,
        "s10": "unchanged: FAIL (this run is local and in-process)",
        "artifact_blake3": artifact.to_hex(),
        "profile": profile,
        "leases": lease_records,
        "coordinator_macs_per_s": coordinator_rate,
        "certificate_epoch": certificate.epoch,
        "certificate_hash": certificate.hash().to_hex(),
        "certificate_recomputes": true,
        "policy_hash": authorised.to_hex(),
        "placement_workers": placement.workers.iter().map(|w| w.to_hex()).collect::<Vec<_>>(),
        "predicted_token_us": placement.predicted_token_us,
        "coordinator_only_token_us": placement.coordinator_only_token_us,
        "stages": placement.stages.len(),
        "participants": participants,
        "churn": churn,
        "forwards": outcome.forwards,
        "exact_logits_every_forward": true,
        "generated_tokens": outcome.output,
        "spot_rows_checked": outcome.spot_checked,
        "duplicate_slices_checked": outcome.duplicate_checked,
        "local_ms": outcome.local.as_millis(),
        "partitioned_ms": outcome.partitioned.as_millis(),
    });
    println!("{summary}");
    if let Some(path) = args.out {
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&summary).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// What one query needs, whichever certificate places it.
struct QueryContext<'a> {
    model: &'a Arc<CachedIntegerModel>,
    artifact: Hash256,
    profile: &'a str,
    ledger: Arc<Mutex<Ledger>>,
    prompt: &'a [u32],
    tokens: usize,
}

struct Outcome {
    forwards: usize,
    output: Vec<u32>,
    spot_checked: usize,
    duplicate_checked: usize,
    local: Duration,
    partitioned: Duration,
}

impl QueryContext<'_> {
    /// Run the query from empty KV caches on the backend `certificate`
    /// places, requiring exact logits at every forward and executing its
    /// verification plan. `leaves` makes one worker leave after that many
    /// projection calls.
    fn run(
        &self,
        certificate: &AssignmentCertificate,
        factor_of: &dyn Fn(&Participant) -> u32,
        leaves: Option<(Hash256, usize)>,
    ) -> Result<Outcome, String> {
        let model = self.model;
        let placement = &certificate.placement;
        let checks = plan(certificate.hash(), placement, &certificate.verification);
        let countdown =
            leaves.map(|(_, calls)| Arc::new(std::sync::atomic::AtomicUsize::new(calls)));
        let mut assignments = Vec::new();
        let mut workers: BTreeMap<String, Arc<dyn RowWorker>> = BTreeMap::new();
        let mut slice_ids: Vec<Vec<String>> = Vec::new();
        for (stage_index, stage_plan) in placement.stages.iter().enumerate() {
            let (layer, tensor, _) = projections(model)[stage_plan.stage];
            let mut ids = Vec::new();
            for slice in &stage_plan.slices {
                let participant = participant_label(&slice.participant);
                let worker_id = format!("e{}-{participant}-s{stage_index}", certificate.epoch);
                let assignment = RowAssignment {
                    artifact_id: self.artifact,
                    execution_profile: self.profile.to_string(),
                    layer,
                    tensor,
                    row_start: slice.row_start as usize,
                    row_end: slice.row_end as usize,
                    worker_id: worker_id.clone(),
                };
                let leaves_after = match (&slice.participant, leaves) {
                    (Participant::Worker(address), Some((leaving, _))) if *address == leaving => {
                        countdown.clone()
                    }
                    _ => None,
                };
                workers.insert(
                    worker_id.clone(),
                    Arc::new(AccountedWorker {
                        participant,
                        slowdown: factor_of(&slice.participant),
                        inner: ModelRowWorker {
                            worker_id: worker_id.clone(),
                            assignment: assignment.clone(),
                            model: model.clone(),
                        },
                        ledger: self.ledger.clone(),
                        leaves_after,
                    }),
                );
                assignments.push(assignment);
                ids.push(worker_id);
            }
            slice_ids.push(ids);
        }
        let backend = PartitionedProjectionBackend::new(
            self.artifact,
            self.profile.to_string(),
            assignments.clone(),
            workers,
        );
        {
            // Report only this query: an aborted one's work is in `churn`.
            let mut ledger = self.ledger.lock();
            ledger.latest.clear();
            ledger.rows.clear();
            ledger.busy.clear();
        }

        let mut local_cache = KVCache::new(model.config.n_layers);
        let mut partitioned_cache = KVCache::new(model.config.n_layers);
        let mut outcome = Outcome {
            forwards: 0,
            output: Vec::new(),
            spot_checked: 0,
            duplicate_checked: 0,
            local: Duration::ZERO,
            partitioned: Duration::ZERO,
        };
        let mut feed: Vec<u32> = self.prompt.to_vec();
        while !feed.is_empty() {
            let token = feed.remove(0);
            let start = Instant::now();
            let expected = model.forward_one_token(token, &mut local_cache);
            outcome.local += start.elapsed();
            let start = Instant::now();
            let got = model
                .forward_one_token_canonical_i8_with_backend(
                    token,
                    &mut partitioned_cache,
                    hash_bytes(&(outcome.forwards as u64).to_le_bytes()),
                    &backend,
                )
                .map_err(|e| format!("forward {} aborted: {e}", outcome.forwards))?;
            outcome.partitioned += start.elapsed();
            if got != expected {
                return Err(format!(
                    "partitioned logits differ from the model at forward {}",
                    outcome.forwards
                ));
            }
            let latest = self.ledger.lock().latest.clone();
            for (stage, row) in &checks.spot_rows {
                let stage_plan = &placement.stages[*stage];
                let Some((slice_index, slice)) = stage_plan
                    .slices
                    .iter()
                    .enumerate()
                    .find(|(_, s)| s.row_start <= *row && *row < s.row_end)
                else {
                    continue;
                };
                let Some((input, values)) = latest.get(&slice_ids[*stage][slice_index]) else {
                    continue;
                };
                let (layer, tensor, _) = projections(model)[stage_plan.stage];
                let single = RowAssignment {
                    artifact_id: self.artifact,
                    execution_profile: self.profile.to_string(),
                    layer,
                    tensor,
                    row_start: *row as usize,
                    row_end: *row as usize + 1,
                    worker_id: "spot-check".into(),
                };
                let checker = ModelRowWorker {
                    worker_id: "spot-check".into(),
                    assignment: single.clone(),
                    model: model.clone(),
                };
                let recomputed = checker
                    .project(RowProjectionRequest {
                        call_id: hash_bytes(b"spot"),
                        input_hash: hash_i64(input),
                        assignment: single,
                        input: input.clone(),
                    })
                    .map_err(|e| e.to_string())?;
                if recomputed.values[0] != values[(*row - slice.row_start) as usize] {
                    return Err(format!("spot check failed at stage {stage} row {row}"));
                }
                outcome.spot_checked += 1;
            }
            for duplicate in &checks.duplicates {
                let id = &slice_ids[duplicate.stage][duplicate.slice];
                let Some((input, values)) = latest.get(id) else {
                    continue;
                };
                let assignment = assignments
                    .iter()
                    .find(|a| &a.worker_id == id)
                    .cloned()
                    .ok_or("duplicate names an unknown slice")?;
                let checker = ModelRowWorker {
                    worker_id: assignment.worker_id.clone(),
                    assignment: assignment.clone(),
                    model: model.clone(),
                };
                let recomputed = checker
                    .project(RowProjectionRequest {
                        call_id: hash_bytes(b"duplicate"),
                        input_hash: hash_i64(input),
                        assignment,
                        input: input.clone(),
                    })
                    .map_err(|e| e.to_string())?;
                if &recomputed.values != values {
                    return Err(format!("duplicate slice {id} differs"));
                }
                outcome.duplicate_checked += 1;
            }
            outcome.forwards += 1;
            if feed.is_empty() && outcome.output.len() < self.tokens {
                let next = arc_inference::integer_lut::argmax_i64(&expected) as u32;
                outcome.output.push(next);
                if outcome.output.len() < self.tokens {
                    feed.push(next);
                }
            }
        }
        Ok(outcome)
    }
}
