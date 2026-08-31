//! Tier 1 on-chain inference validator task.
//!
//! Polls [`StateDB::tier1_pending_requests`] on a tick, derives the committee
//! for each open request, and:
//!   1. Submits an `InferenceVote` tx when this validator is selected and
//!      hasn't yet voted.
//!   2. Submits an `InferenceFinalize` tx when the vote count reaches the
//!      committee size OR the deadline has elapsed.
//!
//! Inference itself runs the candle Q4 backend on the validator's own
//! machine (`GgufEngine::generate`), producing bitwise-identical output
//! across hardware. The output_hash committed on-chain is BLAKE3 of the
//! concatenated little-endian token IDs.
//!
//! Spawn this task at boot from `main.rs` after the candle engine + cached
//! integer model are loaded. It owns no chain state — it reads via the
//! shared `Arc<StateDB>` and writes by inserting txs into `Arc<Mempool>`.
//!
//! See `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md` for the full design.

use std::sync::Arc;
use std::time::Duration;

use arc_crypto::{Hash256, KeyPair};
use arc_inference::cached_integer_model::CachedIntegerModel;
use arc_inference::candle_backend::GgufEngine;
use arc_mempool::Mempool;
use arc_state::{StateDB, Tier1RequestSnapshot};
use arc_state::{
    TIER1_STATUS_FINALIZED, TIER1_STATUS_OPEN, TIER1_STATUS_REFUNDED, TIER1_STATUS_VOTING,
};
use arc_types::Address;
use arc_types::transaction::{
    InferenceAttestationBody, InferenceFinalizeBody, InferenceVoteBody, Transaction, TxBody, TxType,
};
use dashmap::DashMap;
use tokio::time;
use tracing::{info, warn};

/// How often the task scans for new work. 500 ms balances reactivity against
/// state-lock contention. The chain's block tempo (~1-3 s) is the natural
/// upper bound — finer polling than that wastes cycles.
const TICK: Duration = Duration::from_millis(500);

/// Candle's model object owns one mutable KV cache and therefore serializes
/// work per loaded artifact. Admit only one Tier-1 compute at a time instead
/// of spawning an unbounded set of blocking jobs that merely wait on the
/// model lock. Public inference has its own independently bounded permits.
const TIER1_INFERENCE_COMPUTE_CONCURRENCY: usize = 1;

fn spawn_blocking_with_tier1_compute_permit<T, F>(
    permit: tokio::sync::OwnedSemaphorePermit,
    compute: F,
) -> tokio::task::JoinHandle<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _compute_permit = permit;
        compute()
    })
}

/// The validator background task.
pub struct InferenceValidatorTask {
    pub state: Arc<StateDB>,
    pub mempool: Arc<Mempool>,
    pub validator_address: Address,
    pub validator_keypair: KeyPair,
    /// Candle GGUF engine (forward pass). Optional — if a node hasn't loaded
    /// a model it simply never votes. Other committee members can still
    /// reach `min_agreement` without it.
    pub engine: Option<Arc<GgufEngine>>,
    /// Cached integer model used for tokenizer encode/decode + chat template.
    pub tokenizer: Option<Arc<CachedIntegerModel>>,
    /// Streaming BLAKE3 of every byte in the source artifact loaded into the
    /// candle engine. None means this validator must abstain from inference.
    pub model_id: Option<Hash256>,
    /// In-memory dedup so the task never submits two votes for the same
    /// request from the same validator. State-side `apply_inference_vote`
    /// also rejects duplicates; this avoids burning gas on doomed txs.
    voted: Arc<DashMap<[u8; 32], ()>>,
    /// Last finalize-submit time per request. Used to throttle retries:
    /// the apply-time eligibility check (`now >= anchor_height +
    /// deadline_blocks`) can race a finalize tx through mempool→block in a
    /// window where the height is still 1 below deadline; the tx then
    /// applies-with-error (consuming the validator's nonce) and the request
    /// stays Open forever unless we re-submit. Track the timestamp so we
    /// retry every `FINALIZE_RETRY_AFTER` seconds while the request stays
    /// non-terminal.
    finalize_submitted: Arc<DashMap<[u8; 32], std::time::Instant>>,
    compute_permits: Arc<tokio::sync::Semaphore>,
}

/// How long to wait after a finalize submit before allowing a retry on the
/// same request, if it hasn't reached a terminal state. Long enough to give
/// the original tx time to commit and apply, short enough that a wedged
/// request unsticks within minutes rather than hours.
const FINALIZE_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

impl InferenceValidatorTask {
    pub fn new(
        state: Arc<StateDB>,
        mempool: Arc<Mempool>,
        validator_address: Address,
        validator_keypair: KeyPair,
        engine: Option<Arc<GgufEngine>>,
        tokenizer: Option<Arc<CachedIntegerModel>>,
        model_id: Option<Hash256>,
    ) -> Self {
        Self {
            state,
            mempool,
            validator_address,
            validator_keypair,
            engine,
            tokenizer,
            model_id,
            voted: Arc::new(DashMap::new()),
            finalize_submitted: Arc::new(DashMap::new()),
            compute_permits: Arc::new(tokio::sync::Semaphore::new(
                TIER1_INFERENCE_COMPUTE_CONCURRENCY,
            )),
        }
    }

    /// Run forever. Cancellation is via the parent runtime dropping the
    /// task handle. Never returns under normal operation.
    ///
    /// Convenience: takes `self` and wraps in `Arc` internally. If you
    /// already have an `Arc<InferenceValidatorTask>`, call `run_arc` instead
    /// to avoid the move.
    pub async fn run(self) {
        Self::run_arc_inner(Arc::new(self), None).await
    }

    /// Run until the node lifecycle closes admission, then wait for every
    /// already-started inference/vote job to finish before returning.
    pub async fn run_with_shutdown(self, shutdown: tokio::sync::watch::Receiver<bool>) {
        Self::run_arc_inner(Arc::new(self), Some(shutdown)).await
    }

    /// Same as `run` but operates on a pre-built `Arc<Self>`. Used by
    /// integration tests that need to keep a reference for assertions
    /// while the loop runs.
    pub async fn run_arc(me: Arc<Self>) {
        Self::run_arc_inner(me, None).await
    }

    pub async fn run_arc_with_shutdown(
        me: Arc<Self>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        Self::run_arc_inner(me, Some(shutdown)).await
    }

    async fn run_arc_inner(
        me: Arc<Self>,
        mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    ) {
        info!(
            validator = %me.validator_address.to_hex(),
            has_engine = me.engine.is_some(),
            has_tokenizer = me.tokenizer.is_some(),
            "Tier 1 inference validator task running"
        );
        let mut ticker = time::interval(TICK);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        let mut jobs = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = async {
                    let Some(receiver) = shutdown.as_mut() else {
                        std::future::pending::<()>().await;
                        return;
                    };
                    loop {
                        if *receiver.borrow_and_update() {
                            return;
                        }
                        if receiver.changed().await.is_err() {
                            return;
                        }
                    }
                } => break,
                _ = ticker.tick() => me.tick(&mut jobs).await,
                result = jobs.join_next(), if !jobs.is_empty() => {
                    if let Some(Err(error)) = result {
                        warn!(%error, "Tier 1 inference child task failed");
                    }
                }
            }
        }
        while let Some(result) = jobs.join_next().await {
            if let Err(error) = result {
                warn!(%error, "Tier 1 inference child task failed during shutdown");
            }
        }
        info!("Tier 1 inference validator stopped at the lifecycle barrier");
    }

    /// One pass over the pending request set. Idempotent.
    async fn tick(self: &Arc<Self>, jobs: &mut tokio::task::JoinSet<()>) {
        let pending = self.state.tier1_pending_requests();
        if pending.is_empty() {
            return;
        }
        let now = self.state.height();
        for (request_id, _anchor_height) in pending {
            let snap = match self.state.tier1_request_snapshot(&request_id) {
                Some(s) => s,
                None => continue,
            };
            // Skip terminal states. The pending index should already be
            // pruned but double-check (we may race with apply).
            if snap.status == TIER1_STATUS_FINALIZED || snap.status == TIER1_STATUS_REFUNDED {
                continue;
            }

            // Vote path. Skip if we already voted on this request.
            let already_voted = snap
                .votes
                .iter()
                .any(|(v, _)| v.0 == self.validator_address.0);
            if !already_voted && !self.voted.contains_key(&request_id) {
                let committee = self.state.tier1_committee_for(
                    &request_id,
                    snap.anchor_height,
                    snap.committee_size,
                );
                let in_committee = committee.iter().any(|a| a.0 == self.validator_address.0);
                if in_committee {
                    // A busy model is retried on the next tick. Do not create
                    // one waiter/task per pending request: the engine itself
                    // is serialized, so queued blocking jobs add no throughput
                    // and can exhaust runtime memory under adversarial demand.
                    if let Ok(compute_permit) = self.compute_permits.clone().try_acquire_owned() {
                        // Mark optimistically; clear on submission failure.
                        self.voted.insert(request_id, ());
                        let task = self.clone();
                        let task_for_cleanup = self.clone();
                        let snap_for_task = snap.clone();
                        jobs.spawn(async move {
                            if let Err(e) = task
                                .run_inference_and_vote(snap_for_task, compute_permit)
                                .await
                            {
                                warn!(
                                    request_id = %hex::encode(request_id),
                                    error = %e,
                                    "Tier 1 vote submission failed"
                                );
                                task_for_cleanup.voted.remove(&request_id);
                            }
                        });
                    }
                }
            }

            // Finalize path. Two trigger conditions:
            //   (a) Vote count reached committee_size  → ready
            //   (b) Height has passed deadline         → timeout
            let votes_done = snap.votes.len() >= snap.committee_size as usize;
            let deadline_reached = now >= snap.anchor_height.saturating_add(snap.deadline_blocks);
            let retry_ok = self
                .finalize_submitted
                .get(&request_id)
                .map(|e| e.value().elapsed() >= FINALIZE_RETRY_AFTER)
                .unwrap_or(true);
            let can_finalize = (snap.status == TIER1_STATUS_VOTING
                || snap.status == TIER1_STATUS_OPEN)
                && (votes_done || deadline_reached)
                && retry_ok;
            if can_finalize {
                self.finalize_submitted
                    .insert(request_id, std::time::Instant::now());
                if let Err(e) = self.submit_finalize(&request_id) {
                    warn!(
                        request_id = %hex::encode(request_id),
                        error = %e,
                        "Tier 1 finalize submission failed"
                    );
                    self.finalize_submitted.remove(&request_id);
                }
            }
        }
    }

    /// Run candle inference on a request's input blob, then submit an
    /// `InferenceVote` tx.
    async fn run_inference_and_vote(
        self: Arc<Self>,
        snap: Tier1RequestSnapshot,
        compute_permit: tokio::sync::OwnedSemaphorePermit,
    ) -> anyhow::Result<()> {
        let request_id = snap.request_id;
        info!(
            request_id = %hex::encode(request_id),
            input_len = snap.input_blob.len(),
            "Tier 1 inference: running for committee membership"
        );

        // candle.generate() is CPU-bound (5-15 sec on TinyLlama CPU). Running
        // it directly inside an async tokio task blocks the worker thread,
        // and a handful of concurrent inferences starves the whole runtime —
        // /health timeouts, RPC frozen. Move it onto the blocking pool so
        // tokio workers stay free for I/O.
        let engine = self.engine.clone();
        let tokenizer = self.tokenizer.clone();
        let model_id = self
            .model_id
            .ok_or_else(|| anyhow::anyhow!("exact loaded model artifact commitment unavailable"))?;
        let (requested_model_id, requested_max_tokens, expected_input_hash) = self
            .request_parameters(&request_id)
            .ok_or_else(|| anyhow::anyhow!("request parameters unavailable from chain state"))?;
        if model_id != requested_model_id {
            anyhow::bail!(
                "request model 0x{} does not match loaded artifact 0x{}",
                requested_model_id.to_hex(),
                model_id.to_hex()
            );
        }
        if arc_crypto::hash_bytes(&snap.input_blob) != expected_input_hash {
            anyhow::bail!("request input blob does not match the canonical input commitment");
        }
        let generation_tokens =
            Self::prepare_generation_tokens(&tokenizer, &snap.input_blob, requested_max_tokens)?;
        let (output_hash, output_blob) =
            spawn_blocking_with_tier1_compute_permit(compute_permit, move || {
                Self::compute_output_blocking(
                    &engine,
                    model_id,
                    requested_model_id,
                    &generation_tokens,
                    requested_max_tokens,
                )
            })
            .await
            .map_err(|e| anyhow::anyhow!("inference task join: {}", e))??;

        // Submit InferenceVote tx
        let nonce = self
            .state
            .get_account(&self.validator_address)
            .map(|a| a.nonce)
            .unwrap_or(0);
        let body = TxBody::InferenceVote(InferenceVoteBody {
            request_id,
            output_hash,
            output_blob: if snap.votes.is_empty() {
                // First voter attaches the plaintext; subsequent voters omit
                // to save block space (the apply-time hash check rejects
                // malformed blobs anyway).
                Some(output_blob)
            } else {
                None
            },
            vrf_proof: Vec::new(),         // VRF proof not enforced in Phase A.
            committee_seed: Hash256::ZERO, // advisory only; apply re-derives.
        });
        let mut tx = Transaction {
            tx_type: TxType::InferenceVote,
            from: self.validator_address,
            nonce,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        self.state
            .sign_transaction(&mut tx, &self.validator_keypair)
            .map_err(|error| anyhow::anyhow!("sign inference vote: {error}"))?;
        tx.sig_verified = true;
        self.mempool
            .insert(tx)
            .map_err(|e| anyhow::anyhow!("mempool insert vote: {:?}", e))?;
        info!(
            request_id = %hex::encode(request_id),
            "Tier 1 vote submitted"
        );

        // Also post a raw InferenceAttestation as challengeable evidence for
        // the same work. Raw 0x16 attestations never transfer treasury funds
        // and `/worker/earnings` counts only successfully mined threshold-
        // authorized CommunityInferenceReward (0x25) receipts. Best-effort: a
        // failure here logs but does not block the Tier 1 vote.
        let input_hash = arc_crypto::hash_bytes(&snap.input_blob);
        let mut att_tx = Transaction {
            tx_type: TxType::InferenceAttestation,
            from: self.validator_address,
            nonce: nonce + 1,
            body: TxBody::InferenceAttestation(InferenceAttestationBody {
                model_id,
                input_hash,
                output_hash,
                challenge_period: 100,
                bond: 0,
                beneficiary: None,
            }),
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        self.state
            .sign_transaction(&mut att_tx, &self.validator_keypair)
            .map_err(|error| anyhow::anyhow!("sign inference attestation: {error}"))?;
        att_tx.sig_verified = true;
        if let Err(e) = self.mempool.insert(att_tx) {
            warn!(
                request_id = %hex::encode(request_id),
                "Tier 1 attestation mempool insert failed: {:?}", e
            );
        } else {
            info!(
                request_id = %hex::encode(request_id),
                "Tier 1 raw attestation submitted (evidence only; no ARC reward)"
            );
        }
        Ok(())
    }

    /// Compute the model output for a request. Synchronous — call this
    /// from `tokio::task::spawn_blocking` only. The candle path is CPU-bound.
    ///
    /// Missing or mismatched model state is an abstention, never a synthetic
    /// vote. A stub output would falsely claim execution of an unavailable
    /// artifact and could influence consensus.
    fn compute_output_blocking(
        engine: &Option<Arc<GgufEngine>>,
        loaded_model_id: Hash256,
        requested_model_id: Hash256,
        tokens: &[u32],
        max_tokens: u32,
    ) -> anyhow::Result<(Hash256, Vec<u8>)> {
        if loaded_model_id != requested_model_id {
            anyhow::bail!("loaded model artifact does not match the requested model");
        }
        let engine = engine
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("candle engine unavailable for requested artifact"))?;
        let result = engine
            .generate(&loaded_model_id, tokens, max_tokens)
            .map_err(|e| anyhow::anyhow!("candle generate: {:?}", e))?;
        // Engine already provides bitwise-deterministic output bytes and the
        // BLAKE3 hash over them.
        Ok((result.output_hash, result.output))
    }

    fn prepare_generation_tokens(
        tokenizer: &Option<Arc<CachedIntegerModel>>,
        input_blob: &[u8],
        max_tokens: u32,
    ) -> anyhow::Result<Vec<u32>> {
        if input_blob.len() > arc_types::transaction::TIER1_INPUT_BLOB_MAX {
            anyhow::bail!(
                "tier1 input exceeds the chain-enforced {}-byte maximum",
                arc_types::transaction::TIER1_INPUT_BLOB_MAX
            );
        }
        if max_tokens == 0 || max_tokens > arc_types::transaction::TIER1_MAX_TOKENS {
            anyhow::bail!(
                "tier1 max_tokens {max_tokens} outside [1, {}]",
                arc_types::transaction::TIER1_MAX_TOKENS
            );
        }
        let tokenizer = tokenizer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("tokenizer unavailable for requested artifact"))?;
        let text = String::from_utf8_lossy(input_blob);
        let templated = tokenizer.apply_chat_template(&text);
        let encoded = tokenizer.encode(&templated);
        Self::preflight_generation_context(encoded.len(), max_tokens, tokenizer.config.max_seq)?;
        let mut tokens = Vec::with_capacity(encoded.len() + 1);
        tokens.push(tokenizer.config.bos_token);
        tokens.extend(encoded);
        Ok(tokens)
    }

    fn preflight_generation_context(
        encoded_prompt_tokens: usize,
        max_tokens: u32,
        context_window: usize,
    ) -> anyhow::Result<usize> {
        let input_tokens = encoded_prompt_tokens
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Tier-1 BOS position count overflow"))?;
        arc_inference::candle_backend::GgufEngine::preflight_generation_for_context(
            input_tokens,
            max_tokens,
            context_window,
        )
        .map_err(|error| anyhow::anyhow!("invalid Tier-1 generation context: {error}"))
    }

    /// Recover the model commitment from the canonical request transaction.
    /// If pruning or incomplete state makes it unavailable, voting fails
    /// closed instead of guessing from model dimensions or a display name.
    fn request_parameters(&self, request_id: &[u8; 32]) -> Option<(Hash256, u32, Hash256)> {
        self.state.full_transactions.iter().find_map(|entry| {
            let TxBody::InferenceRequest(body) = &entry.value().body else {
                return None;
            };
            (body.request_id == *request_id).then_some((
                body.model_id,
                body.max_tokens,
                body.input_hash,
            ))
        })
    }

    /// Submit an `InferenceFinalize` tx. Any validator can submit; the
    /// first to commit wins, the rest reject with a no-op error.
    fn submit_finalize(&self, request_id: &[u8; 32]) -> anyhow::Result<()> {
        let nonce = self
            .state
            .get_account(&self.validator_address)
            .map(|a| a.nonce)
            .unwrap_or(0);
        let body = TxBody::InferenceFinalize(InferenceFinalizeBody {
            request_id: *request_id,
        });
        let mut tx = Transaction {
            tx_type: TxType::InferenceFinalize,
            from: self.validator_address,
            nonce,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        self.state
            .sign_transaction(&mut tx, &self.validator_keypair)
            .map_err(|error| anyhow::anyhow!("sign inference finalize: {error}"))?;
        tx.sig_verified = true;
        self.mempool
            .insert(tx)
            .map_err(|e| anyhow::anyhow!("mempool insert finalize: {:?}", e))?;
        info!(
            request_id = %hex::encode(request_id),
            "Tier 1 finalize submitted"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier1_context_preflight_counts_bos_and_full_requested_output() {
        assert_eq!(
            InferenceValidatorTask::preflight_generation_context(3, 3, 6).unwrap(),
            6
        );
        let error = InferenceValidatorTask::preflight_generation_context(4, 3, 6)
            .expect_err("one tokenizer-expanded position past context must fail");
        assert!(error.to_string().contains("context_window_exceeded"));
        assert!(InferenceValidatorTask::preflight_generation_context(1, 0, 6).is_err());
    }

    #[test]
    fn tier1_compute_admission_has_no_waiter_queue() {
        let permits = Arc::new(tokio::sync::Semaphore::new(
            TIER1_INFERENCE_COMPUTE_CONCURRENCY,
        ));
        let held = permits.clone().try_acquire_owned().unwrap();
        assert!(
            permits.clone().try_acquire_owned().is_err(),
            "a busy model must be retried by the polling loop, not queued"
        );
        drop(held);
        assert!(permits.try_acquire_owned().is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_tier1_waiter_cannot_release_blocking_compute_permit() {
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(0);

        let waiter = tokio::spawn(async move {
            spawn_blocking_with_tier1_compute_permit(permit, move || {
                let _ = started_tx.send(());
                finish_rx.recv().unwrap();
            })
            .await
            .unwrap();
        });
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(
            permits.clone().try_acquire_owned().is_err(),
            "cancelling the async waiter must not advertise capacity while blocking compute runs"
        );

        finish_tx.send(()).unwrap();
        let reacquired = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(permit) = permits.clone().try_acquire_owned() {
                    break permit;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocking compute did not release its permit after completion");
        drop(reacquired);
    }
}
