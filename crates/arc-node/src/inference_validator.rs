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

const TICK: Duration = Duration::from_millis(500);

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

pub struct InferenceValidatorTask {
    pub state: Arc<StateDB>,
    pub mempool: Arc<Mempool>,
    pub validator_address: Address,
    pub validator_keypair: KeyPair,
    pub engine: Option<Arc<GgufEngine>>,
    pub tokenizer: Option<Arc<CachedIntegerModel>>,
    pub model_id: Option<Hash256>,
    voted: Arc<DashMap<[u8; 32], ()>>,
    finalize_submitted: Arc<DashMap<[u8; 32], std::time::Instant>>,
    compute_permits: Arc<tokio::sync::Semaphore>,
    worker_execution: Arc<crate::native_inference::WorkerExecutionGate>,
}

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
            worker_execution: Arc::new(crate::native_inference::WorkerExecutionGate::default()),
        }
    }

    pub fn with_worker_execution_gate(
        mut self,
        gate: Arc<crate::native_inference::WorkerExecutionGate>,
    ) -> Self {
        self.worker_execution = gate;
        self
    }

    pub async fn run(self) {
        Self::run_arc_inner(Arc::new(self), None).await
    }

    pub async fn run_with_shutdown(self, shutdown: tokio::sync::watch::Receiver<bool>) {
        Self::run_arc_inner(Arc::new(self), Some(shutdown)).await
    }

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
            if snap.status == TIER1_STATUS_FINALIZED || snap.status == TIER1_STATUS_REFUNDED {
                continue;
            }

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
                if in_committee
                    && let Some(worker_permit) = self.worker_execution.try_enter()
                    && let Ok(compute_permit) = self.compute_permits.clone().try_acquire_owned()
                {
                    self.voted.insert(request_id, ());
                    let task = self.clone();
                    let task_for_cleanup = self.clone();
                    let snap_for_task = snap.clone();
                    jobs.spawn(async move {
                        if let Err(e) = task
                            .run_inference_and_vote(snap_for_task, compute_permit, worker_permit)
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

    async fn run_inference_and_vote(
        self: Arc<Self>,
        snap: Tier1RequestSnapshot,
        compute_permit: tokio::sync::OwnedSemaphorePermit,
        worker_permit: crate::native_inference::WorkerExecutionPermit,
    ) -> anyhow::Result<()> {
        let request_id = snap.request_id;
        info!(
            request_id = %hex::encode(request_id),
            input_len = snap.input_blob.len(),
            "Tier 1 inference: running for committee membership"
        );

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
        let worker_permit_for_compute = worker_permit.clone();
        let (output_hash, output_blob) =
            spawn_blocking_with_tier1_compute_permit(compute_permit, move || {
                let _worker_permit = worker_permit_for_compute;
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

        let nonce = self
            .state
            .get_account(&self.validator_address)
            .map(|a| a.nonce)
            .unwrap_or(0);
        let body = TxBody::InferenceVote(InferenceVoteBody {
            request_id,
            output_hash,
            output_blob: if snap.votes.is_empty() {
                Some(output_blob)
            } else {
                None
            },
            vrf_proof: Vec::new(),
            committee_seed: Hash256::ZERO,
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
