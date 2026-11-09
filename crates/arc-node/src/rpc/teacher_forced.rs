//! Optional claim acceleration. Only the existing sequential verifier may
//! reject a claim; every failure here is an instruction to fall back.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

// Process-lifetime counters are emitted in structured logs, outside the frozen
// desktop response schemas. Candidate counters count validated candidates.
static COUNTERS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
fn count(index: usize, event: &'static str) {
    let total = COUNTERS[index].fetch_add(1, Ordering::Relaxed) + 1;
    tracing::info!(event, total, "community teacher-forced verification");
}

fn candidate(
    model: &arc_inference::cached_integer_model::CachedIntegerModel,
    result: &WorkResult,
) -> Option<Vec<u32>> {
    if !result.success {
        return None;
    }
    let matches = |tokens: &[u32]| {
        tokens.len() <= INFERENCE_RUN_MAX_TOKENS as usize
            && compare_community_result_with_tokens(result, tokens, &model.decode(tokens)).is_ok()
    };
    if let Some(tokens) = result
        .output_tokens
        .as_ref()
        .filter(|tokens| matches(tokens))
    {
        count(5, "candidate_wire");
        return Some(tokens.clone());
    }
    let tokens = model.encode(&result.output);
    if matches(&tokens) {
        count(6, "candidate_reencoded");
        return Some(tokens);
    }
    // Some tokenizers encode a displayed EOS piece as ordinary text. Try
    // stripping precisely that trailing piece and appending its token ID.
    for &eos in &model.config.eos_tokens {
        let piece = model.decode(&[eos]);
        if !piece.is_empty()
            && let Some(text) = result.output.strip_suffix(&piece)
        {
            let mut tokens = model.encode(text);
            tokens.push(eos);
            if matches(&tokens) {
                count(6, "candidate_reencoded");
                return Some(tokens);
            }
        }
    }
    count(7, "candidate_unavailable");
    None
}

pub(super) fn valid_claim_shape(claimed: &[u32], max_tokens: u32, eos: &[u32]) -> bool {
    !claimed.is_empty()
        && claimed.len() <= max_tokens as usize
        && !claimed[..claimed.len() - 1]
            .iter()
            .any(|token| eos.contains(token))
        && (eos.contains(claimed.last().unwrap()) || claimed.len() == max_tokens as usize)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_teacher_forced_pipeline(
    node: &NodeState,
    model: &Arc<arc_inference::cached_integer_model::CachedIntegerModel>,
    pipeline: &[PipelineHop],
    request_id: &str,
    all_tokens: &[u32],
    claimed: &[u32],
    max_tokens: u32,
    strategies: &[HopStrategy],
    deadline: tokio::time::Instant,
) -> Result<PipelineRun, String> {
    preflight_sharded_generation(model, all_tokens.len(), max_tokens)?;
    if !valid_claim_shape(claimed, max_tokens, &model.config.eos_tokens) {
        return Err("inconsistent token claim".into());
    }
    if strategies.len() != pipeline.len() {
        return Err("invalid hop strategies".into());
    }
    let execution_profile = pipeline_execution_profile(pipeline)?;
    let model_id = format!(
        "0x{}",
        node.model_artifact_id
            .ok_or("model commitment unavailable")?
            .to_hex()
    );
    let hops = pipeline.len();
    let first_output = all_tokens.len() - 1;
    let positions = all_tokens.len() + claimed.len() - 1;
    let claimed = Arc::new(claimed.to_vec());
    let mut txs = Vec::new();
    let mut rxs = Vec::new();
    for _ in 0..=hops {
        let (tx, rx) = tokio::sync::mpsc::channel::<PrefillFlow>(PREFILL_CHANNEL_DEPTH);
        txs.push(tx);
        rxs.push(Some(rx));
    }
    let mut workers = tokio::task::JoinSet::new();
    for (i, ((start_layer, end_layer), replicas)) in pipeline.iter().enumerate() {
        let (start_layer, end_layer) = (*start_layer, *end_layer);
        let strategy = strategies[i];
        let mut replicas = replicas.clone();
        let node = node.clone();
        let request_id = request_id.to_string();
        let model_id = model_id.clone();
        let execution_profile = execution_profile.clone();
        let expected_hidden_len = model.config.d_model;
        let claimed = claimed.clone();
        let mut rx = rxs[i].take().unwrap();
        let tx = txs[i + 1].clone();
        workers.spawn(async move {
            let mut stats = HopStats::default();
            let mut votes = Vec::new();
            let mut bytes = 0;
            while let Some(item) = rx.recv().await {
                let k = item.position.saturating_sub(first_output);
                let req = ForwardShardRequest {
                    request_id: request_id.clone(),
                    model_id: model_id.clone(),
                    execution_profile: execution_profile.clone(),
                    token: item.token,
                    hidden: item.hidden,
                    hidden_hash: item.hidden_hash,
                    position: item.position,
                    start_layer,
                    end_layer,
                    expected_hidden_len,
                    expect_terminal: i == hops - 1,
                    generated_tokens: if i == hops - 1 {
                        claimed[k.saturating_sub(64)..k].to_vec()
                    } else {
                        Vec::new()
                    },
                    last_token: false,
                };
                let out = pipeline_hop(&node, &mut replicas, strategy, &req, true).await?;
                // Check at the terminal worker, before accepting more positions.
                if i == hops - 1
                    && item.position >= first_output
                    && out.token_id != Some(claimed[k])
                {
                    return Err(format!("terminal mismatch at output {k}"));
                }
                bytes += out.req_bytes + out.resp_bytes;
                stats.fold(&out);
                if let Some(vote) = out.vote {
                    votes.push(vote);
                }
                tx.send(PrefillFlow {
                    position: item.position,
                    token: None,
                    hidden: out.hidden,
                    hidden_hash: out.hidden_hash,
                    terminal_token: out.token_id,
                })
                .await
                .map_err(|_| "teacher-forced downstream closed".to_string())?;
            }
            Ok::<_, String>((i, stats, votes, bytes))
        });
    }
    let input = txs.remove(0);
    drop(txs);
    let tokens: Vec<u32> = all_tokens
        .iter()
        .chain(claimed[..claimed.len() - 1].iter())
        .copied()
        .collect();
    let mut feeder = tokio::task::JoinSet::new();
    feeder.spawn(async move {
        for (position, token) in tokens.into_iter().enumerate() {
            if input
                .send(PrefillFlow {
                    position,
                    token: Some(token),
                    hidden: None,
                    hidden_hash: None,
                    terminal_token: None,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut tail = rxs[hops].take().unwrap();
    let mut seen = 0;
    let mut tail_open = true;
    let mut run = PipelineRun {
        generated: claimed.as_ref().clone(),
        hop_stats: vec![HopStats::default(); hops],
        total_bytes: 0,
        votes: Vec::new(),
    };
    let result = async {
        while !workers.is_empty() || tail_open {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => return Err("fast-pass deadline".to_string()),
                joined = workers.join_next(), if !workers.is_empty() => {
                    let (i, stats, mut votes, bytes) = joined.unwrap().map_err(|e| e.to_string())??;
                    run.hop_stats[i] = stats;
                    run.votes.append(&mut votes);
                    run.total_bytes += bytes;
                }
                flow = tail.recv(), if tail_open => {
                    if let Some(flow) = flow {
                        if flow.position != seen { return Err("non-contiguous teacher-forced output".into()); }
                        seen += 1;
                    } else { tail_open = false; }
                }
            }
        }
        if seen != positions { return Err("incomplete teacher-forced pipeline".into()); }
        Ok(())
    }.await;
    // On refusal, mismatch or timeout, stop issuance and observe every task's
    // exit before the caller cleans caches or starts a sequential fallback.
    feeder.shutdown().await;
    workers.shutdown().await;
    result?;
    Ok(run)
}

pub(super) async fn try_teacher_forced_community_output(
    node: &NodeState,
    item: &WorkItem,
    result: &WorkResult,
) -> Option<CommunityCanonicalRecompute> {
    let model = node.inference_model.as_ref()?;
    let claimed = candidate(model, result)?;
    let attempt = async {
        if !valid_claim_shape(&claimed, item.max_tokens, &model.config.eos_tokens) {
            return Err("inconsistent token claim".to_string());
        }
        let required = node
            .community_verify_profiles
            .verified(&item.execution_profile)
            .ok_or("unsupported profile")?;
        let (_, model_id) = exact_model_identity(node)?;
        if parse_hash256_hex(
            item.model_id.as_deref().ok_or("missing model identity")?,
            "assigned model_id",
        )? != model_id
        {
            return Err("assigned model mismatch".into());
        }
        let plan =
            plan_community_verification(node, required, true).map_err(|(_, detail)| detail)?;
        let (all_tokens, _) =
            community_worker_generation_context(model, &item.input, item.max_tokens)?;
        let request_id = format!(
            "0x{}",
            arc_crypto::hash_bytes(
                format!(
                    "community-verify-tf-v1-{}-{}",
                    item.job_id,
                    node.validator_address.to_hex()
                )
                .as_bytes()
            )
            .to_hex()
        );
        let strategies: Vec<_> = plan
            .replicas_per_range
            .iter()
            .map(|&fanout| HopStrategy::Fanout {
                fanout,
                needed: COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED,
                allow_degraded_quorum: false,
            })
            .collect();
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(
                COMMUNITY_FAST_VERIFY_TIMEOUT_SECS - CLEANUP_TIMEOUT_SECS,
            );
        let run = run_teacher_forced_pipeline(
            node,
            model,
            &plan.pipeline,
            &request_id,
            &all_tokens,
            &claimed,
            item.max_tokens,
            &strategies,
            deadline,
        )
        .await;
        cleanup_shards(node, &plan.pipeline, &request_id).await;
        let run = run?;
        let count = validate_community_range_position_quorums(
            &plan.pipeline,
            &plan.replicas_per_range,
            &run.hop_stats,
            &run.votes,
            &node.dag_validators.read(),
        )?;
        let output_text = model.decode(&claimed);
        let output_hash = compare_community_result_with_tokens(result, &claimed, &output_text)?;
        Ok(CommunityCanonicalRecompute {
            generated: claimed,
            output_text,
            output_hash,
            range_count: plan.pipeline.len(),
            range_position_quorum_count: count,
            execution_profile: required,
            range_replicas: plan
                .pipeline
                .iter()
                .map(|(range, _)| *range)
                .zip(plan.replicas_per_range)
                .collect(),
        })
    }
    .await;
    match attempt {
        Ok(verified) => {
            count(0, "hit");
            Some(verified)
        }
        Err(error) => {
            if error.contains("429") {
                count(2, "fallback_429");
            } else if error.contains("mismatch") {
                count(1, "fallback_mismatch");
            } else if error.contains("deadline") {
                count(3, "fallback_deadline");
            } else {
                count(4, "fallback_other");
            }
            tracing::debug!(%error, "teacher-forced pass falling back to sequential verification");
            None
        }
    }
}
