use arc_types::{Account, Transaction, TxBody};
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct TxAccessSet {
    pub accounts: HashSet<[u8; 32]>,
}

fn validator_set_access_key() -> [u8; 32] {
    arc_crypto::hash_bytes(b"arc-block-stm-validator-set-v1").0
}

pub fn tx_access_set(tx: &Transaction) -> TxAccessSet {
    let mut accounts = HashSet::new();
    accounts.insert(tx.from.0);

    match &tx.body {
        TxBody::Transfer(body) => {
            accounts.insert(body.to.0);
            accounts.insert(crate::v3_fee_treasury_address().0);
        }
        TxBody::Settle(body) => {
            accounts.insert(body.agent_id.0);
        }
        TxBody::Swap(body) => {
            accounts.insert(body.counterparty.0);
        }
        TxBody::Stake(body) => {
            accounts.insert(body.validator.0);
            accounts.insert(validator_set_access_key());
        }
        TxBody::WasmCall(body) => {
            accounts.insert(body.contract.0);
        }
        TxBody::Escrow(body) => {
            accounts.insert(body.beneficiary.0);
        }
        TxBody::DeployContract(_) | TxBody::RegisterAgent(_) | TxBody::MultiSig(_) => {}
        TxBody::JoinValidator(_) | TxBody::LeaveValidator | TxBody::UpdateStake(_) => {
            accounts.insert(validator_set_access_key());
        }
        TxBody::ClaimRewards => {}
        TxBody::Governance(_) => {}
        TxBody::BridgeLock(_) | TxBody::BridgeMint(_) => {}
        TxBody::BatchSettle(body) => {
            for entry in &body.entries {
                accounts.insert(entry.agent_id.0);
            }
        }
        TxBody::ChannelOpen(body) => {
            accounts.insert(body.counterparty.0);
        }
        TxBody::ChannelClose(_) | TxBody::ChannelDispute(_) => {}
        TxBody::ShardProof(_) => {}
        TxBody::InferenceAttestation(_) => {
            let escrow_addr =
                arc_crypto::hash_bytes(&[b"arc-inference", tx.hash.as_ref()].concat());
            accounts.insert(escrow_addr.0);
        }
        TxBody::CommunityInferenceReward(body) => {
            accounts.insert(arc_types::transaction::inference_reward_treasury_address().0);
            accounts.insert(validator_set_access_key());
            accounts.insert(body.worker.0);
            accounts.insert(
                arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                    &body.chain_domain,
                    &body.job_id,
                )
                .0,
            );
            accounts.insert(
                arc_types::transaction::CommunityInferenceRewardBody::v3_marker_address(
                    &body.chain_domain,
                    &body.job_id,
                )
                .0,
            );
            accounts.insert(
                arc_types::transaction::CommunityInferenceRewardBody::certificate_marker_address(
                    &body.chain_domain,
                    &body.worker,
                    &body.worker_certificate.attestation_hash,
                )
                .0,
            );
            accounts.insert(
                arc_types::transaction::CommunityInferenceRewardBody::v3_certificate_marker_address(
                    &body.chain_domain,
                    &body.worker,
                    &body.worker_certificate.attestation_hash,
                )
                .0,
            );
            if let Some(marker) =
                arc_types::transaction::CommunityInferenceRewardBody::recovery_probe_marker_address(
                    &body.chain_domain,
                    &body.assignment_epoch,
                )
            {
                accounts.insert(marker.0);
            }
            if let Some(marker) = arc_types::transaction::CommunityInferenceRewardBody::v3_recovery_probe_marker_address(
                &body.chain_domain,
                &body.assignment_epoch,
            ) {
                accounts.insert(marker.0);
            }
        }
        TxBody::InferenceChallenge(body) => {
            let escrow_addr = arc_crypto::hash_bytes(
                &[b"arc-inference", body.attestation_hash.as_ref()].concat(),
            );
            accounts.insert(escrow_addr.0);
        }
        TxBody::InferenceRegister(_) => {}
        TxBody::InferenceEscrowOpen(body) => {
            accounts.insert(
                arc_types::transaction::InferenceEscrowOpenBody::escrow_address(&body.request_id),
            );
        }
        TxBody::InferenceEscrowRelease(body) => {
            accounts.insert(
                arc_types::transaction::InferenceEscrowOpenBody::escrow_address(&body.request_id),
            );
            accounts.insert(body.proposer.0);
            for r in &body.replicas {
                accounts.insert(r.0);
            }
            accounts.insert(body.observer_pool.0);
            accounts.insert(body.treasury.0);
        }
        TxBody::InferenceEscrowRefund(body) => {
            accounts.insert(
                arc_types::transaction::InferenceEscrowOpenBody::escrow_address(&body.request_id),
            );
        }
        TxBody::ModelRegistration(body) => {
            accounts.insert(
                arc_types::transaction::ModelRegistrationBody::registry_account(&body.model_id),
            );
        }
        TxBody::ModelRequest(body) => {
            accounts.insert(arc_types::transaction::ModelRequestBody::request_account(
                &body.request_id,
            ));
        }
        TxBody::ShardCoverageClaim(body) => {
            accounts.insert(
                arc_types::transaction::ShardCoverageClaimBody::claim_account(
                    &body.model_id,
                    &body.node_pubkey,
                ),
            );
        }
        TxBody::CapacityAdvertisement(body) => {
            accounts.insert(
                arc_types::transaction::CapacityAdvertisementBody::capacity_account(
                    &body.node_pubkey,
                ),
            );
        }
        TxBody::ShardAssignmentProposal(_) => {}
        TxBody::FaucetClaim(body) => {
            accounts.insert(body.recipient.0);
            accounts.insert(arc_types::transaction::faucet_pool_address().0);
            accounts
                .insert(arc_types::transaction::FaucetClaimBody::marker_address(&body.recipient).0);
            accounts.insert(
                arc_types::transaction::FaucetClaimBody::v3_marker_address(&body.recipient).0,
            );
            accounts.insert(validator_set_access_key());
        }
        TxBody::InferenceRequest(body) => {
            let escrow_addr =
                arc_crypto::hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
            accounts.insert(escrow_addr.0);
        }
        TxBody::InferenceVote(body) => {
            let escrow_addr =
                arc_crypto::hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
            accounts.insert(escrow_addr.0);
        }
        TxBody::InferenceFinalize(body) => {
            let escrow_addr =
                arc_crypto::hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
            accounts.insert(escrow_addr.0);
            accounts.insert(arc_types::transaction::faucet_pool_address().0);
        }
        TxBody::NativeInferenceRequest(body) => {
            accounts.insert(validator_set_access_key());
            accounts.insert(
                crate::inference_contract_state::escrow_address(body.request.job.request_id()).0,
            );
        }
        TxBody::NativeInferenceFinalize(body) => {
            accounts.insert(validator_set_access_key());
            accounts.insert(
                crate::inference_contract_state::escrow_address(arc_crypto::Hash256(
                    body.request_id,
                ))
                .0,
            );
        }
        TxBody::NativeInferenceRefund(body) => {
            accounts.insert(validator_set_access_key());
            accounts.insert(
                crate::inference_contract_state::escrow_address(arc_crypto::Hash256(
                    body.request_id,
                ))
                .0,
            );
        }
    }

    TxAccessSet { accounts }
}

pub fn partition_batches(transactions: &[Transaction]) -> Vec<Vec<usize>> {
    if transactions.is_empty() {
        return vec![];
    }

    let access_sets: Vec<TxAccessSet> = transactions.iter().map(tx_access_set).collect();

    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut last_batch_by_account: HashMap<[u8; 32], usize> = HashMap::new();

    for (i, access) in access_sets.iter().enumerate() {
        let batch_index = access
            .accounts
            .iter()
            .filter_map(|account| last_batch_by_account.get(account))
            .map(|last_batch| last_batch + 1)
            .max()
            .unwrap_or(0);

        if batches.len() <= batch_index {
            batches.resize_with(batch_index + 1, Vec::new);
        }
        batches[batch_index].push(i);

        for account in &access.accounts {
            last_batch_by_account.insert(*account, batch_index);
        }
    }

    batches
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;
    use arc_types::{
        Address, TxType,
        transaction::{
            CommunityInferenceRewardBody, InferenceAttestationBody, InferenceChallengeBody,
            UpdateStakeBody, WorkerInferenceCertificate,
        },
    };

    fn addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    fn make_transfer(from: Address, to: Address, nonce: u64) -> Transaction {
        Transaction::new_transfer(from, to, 100, nonce)
    }

    fn make_attestation(from: Address, nonce: u64, tag: u8) -> Transaction {
        let mut tx = Transaction {
            tx_type: TxType::InferenceAttestation,
            from,
            nonce,
            body: TxBody::InferenceAttestation(InferenceAttestationBody {
                model_id: hash_bytes(b"model"),
                input_hash: hash_bytes(&[tag, 1]),
                output_hash: hash_bytes(&[tag, 2]),
                challenge_period: 100,
                bond: 1,
                beneficiary: None,
            }),
            fee: 0,
            gas_limit: 0,
            hash: arc_crypto::Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    fn make_challenge(from: Address, attestation_hash: arc_crypto::Hash256) -> Transaction {
        let mut tx = Transaction {
            tx_type: TxType::InferenceChallenge,
            from,
            nonce: 0,
            body: TxBody::InferenceChallenge(InferenceChallengeBody {
                attestation_hash,
                challenger_output_hash: hash_bytes(b"different-output"),
                challenger_bond: 1,
            }),
            fee: 0,
            gas_limit: 0,
            hash: arc_crypto::Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    fn make_community_reward(validator: Address, worker: Address, tag: u8) -> Transaction {
        let worker_attestation = make_attestation(worker, 0, tag);
        Transaction::new_community_inference_reward(
            validator,
            u64::from(tag),
            CommunityInferenceRewardBody {
                chain_domain: CommunityInferenceRewardBody::expected_chain_domain(),
                job_id: hash_bytes(&[tag, 3]),
                coordinator: validator,
                assignment_epoch: hash_bytes(b"block-stm-assignment-epoch"),
                job_nonce: u64::from(tag),
                recovery_epoch: 0,
                validator_set_id: 0,
                transaction_domain: arc_crypto::Hash256::ZERO,
                worker,
                model_id: hash_bytes(b"model"),
                input_hash: hash_bytes(&[tag, 1]),
                output_hash: hash_bytes(&[tag, 2]),
                max_tokens: 8,
                expires_at_height: 100,
                worker_certificate: WorkerInferenceCertificate {
                    attestation_hash: worker_attestation.hash,
                    nonce: worker_attestation.nonce,
                    challenge_period: 100,
                    signature: worker_attestation.signature,
                },
                validator_approvals: Vec::new(),
            },
        )
    }

    #[test]
    fn transfers_serialize_on_shared_v3_fee_treasury() {
        let txs = vec![
            make_transfer(addr(1), addr(2), 0),
            make_transfer(addr(3), addr(4), 0),
        ];
        let batches = partition_batches(&txs);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn test_conflicting_transfers_separate_batches() {
        let txs = vec![
            make_transfer(addr(1), addr(2), 0),
            make_transfer(addr(3), addr(2), 0),
        ];
        let batches = partition_batches(&txs);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_same_sender_separate_batches() {
        let txs = vec![
            make_transfer(addr(1), addr(2), 0),
            make_transfer(addr(1), addr(3), 1),
        ];
        let batches = partition_batches(&txs);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_empty_transactions() {
        let batches = partition_batches(&[]);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_mixed_conflict_pattern() {
        let txs = vec![
            make_transfer(addr(1), addr(2), 0),
            make_transfer(addr(3), addr(4), 0),
            make_transfer(addr(5), addr(2), 0),
            make_transfer(addr(3), addr(6), 1),
        ];
        let batches = partition_batches(&txs);
        assert_eq!(batches.len(), 4);
        assert!(batches.iter().all(|batch| batch.len() == 1));
    }

    #[test]
    fn community_rewards_conflict_on_shared_treasury() {
        let first = make_community_reward(addr(9), addr(10), 1);
        let second = make_community_reward(addr(9), addr(11), 2);
        let treasury = arc_types::transaction::inference_reward_treasury_address().0;

        assert!(tx_access_set(&first).accounts.contains(&treasury));
        assert!(tx_access_set(&second).accounts.contains(&treasury));
        assert_eq!(
            partition_batches(&[first, second]).len(),
            2,
            "shared treasury debits must execute in transaction order"
        );
    }

    #[test]
    fn community_rewards_conflict_with_validator_set_mutations() {
        let reward = make_community_reward(addr(9), addr(10), 1);
        let mut update_stake = Transaction {
            tx_type: TxType::UpdateStake,
            from: addr(50),
            nonce: 0,
            body: TxBody::UpdateStake(UpdateStakeBody { new_stake: 123_456 }),
            fee: 0,
            gas_limit: 0,
            hash: arc_crypto::Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        update_stake.hash = update_stake.compute_hash();

        let validator_set = validator_set_access_key();
        assert!(tx_access_set(&reward).accounts.contains(&validator_set));
        assert!(
            tx_access_set(&update_stake)
                .accounts
                .contains(&validator_set)
        );
        assert_eq!(
            partition_batches(&[update_stake, reward]).len(),
            2,
            "reward quorum must observe the preceding canonical validator-set mutation"
        );
    }

    #[test]
    fn raw_attestations_do_not_claim_treasury_access() {
        let first = make_attestation(addr(10), 0, 1);
        let second = make_attestation(addr(11), 0, 2);
        let treasury = arc_types::transaction::inference_reward_treasury_address().0;

        assert!(!tx_access_set(&first).accounts.contains(&treasury));
        assert!(!tx_access_set(&second).accounts.contains(&treasury));
        assert_eq!(partition_batches(&[first, second]).len(), 1);
    }

    #[test]
    fn challenges_for_same_attestation_conflict_on_escrow() {
        let attestation_hash = hash_bytes(b"attestation");
        let first = make_challenge(addr(20), attestation_hash);
        let second = make_challenge(addr(21), attestation_hash);
        let escrow = hash_bytes(&[b"arc-inference", attestation_hash.as_ref()].concat()).0;

        assert!(tx_access_set(&first).accounts.contains(&escrow));
        assert!(tx_access_set(&second).accounts.contains(&escrow));
        assert_eq!(partition_batches(&[first, second]).len(), 2);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveMode {
    Sequential,
    BlockSTM,
}

pub fn choose_execution_mode(transactions: &[Transaction]) -> AdaptiveMode {
    if transactions.len() < 100 {
        return AdaptiveMode::Sequential;
    }

    let mut has_contracts = false;
    let mut has_batch_settle = false;
    let mut unique_receivers = HashSet::new();

    for tx in transactions {
        match &tx.body {
            TxBody::WasmCall(_) | TxBody::DeployContract(_) => {
                has_contracts = true;
            }
            TxBody::BatchSettle(body) if body.entries.len() > 10 => {
                has_batch_settle = true;
            }
            TxBody::Transfer(body) => {
                unique_receivers.insert(body.to.0);
            }
            TxBody::Settle(body) => {
                unique_receivers.insert(body.agent_id.0);
            }
            _ => {}
        }
    }

    if has_contracts {
        return AdaptiveMode::BlockSTM;
    }

    if has_batch_settle {
        return AdaptiveMode::BlockSTM;
    }

    let diversity_ratio = if transactions.is_empty() {
        0.0
    } else {
        unique_receivers.len() as f64 / transactions.len() as f64
    };

    if diversity_ratio > 0.5 {
        AdaptiveMode::BlockSTM
    } else {
        AdaptiveMode::Sequential
    }
}

#[derive(Debug)]
pub struct MVHashMap {
    data: DashMap<[u8; 32], Vec<(usize, u64, u64)>>,
}

impl Default for MVHashMap {
    fn default() -> Self {
        Self::new()
    }
}

impl MVHashMap {
    pub fn new() -> Self {
        Self {
            data: DashMap::new(),
        }
    }

    pub fn write(&self, account: [u8; 32], tx_index: usize, balance: u64, nonce: u64) {
        let mut entry = self.data.entry(account).or_default();
        entry.retain(|(idx, _, _)| *idx != tx_index);
        entry.push((tx_index, balance, nonce));
        entry.sort_by_key(|(idx, _, _)| *idx);
    }

    pub fn read(&self, account: &[u8; 32], tx_index: usize) -> Option<(u64, u64, usize)> {
        self.data.get(account).and_then(|versions| {
            versions
                .iter()
                .rev()
                .find(|(idx, _, _)| *idx < tx_index)
                .map(|(idx, bal, nonce)| (*bal, *nonce, *idx))
        })
    }

    pub fn clear(&self) {
        self.data.clear();
    }
}

#[derive(Debug, Clone, Default)]
pub struct SpeculativeTxAccessSet {
    pub reads: HashMap<[u8; 32], (u64, u64, usize)>,
    pub writes: HashMap<[u8; 32], (u64, u64)>,
}

#[derive(Debug, Clone)]
pub struct SpeculativeResult {
    pub tx_index: usize,
    pub success: bool,
    pub access_set: SpeculativeTxAccessSet,
    pub gas_used: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
    Pending,
    Executed,
    Validated,
    NeedsReExecution,
}

pub struct SpeculativeScheduler {
    num_txs: usize,
    statuses: Vec<parking_lot::Mutex<TxStatus>>,
    results: Vec<parking_lot::Mutex<Option<SpeculativeResult>>>,
    mv_hashmap: MVHashMap,
    max_rounds: usize,
}

impl SpeculativeScheduler {
    pub fn new(num_txs: usize) -> Self {
        let statuses: Vec<_> = (0..num_txs)
            .map(|_| parking_lot::Mutex::new(TxStatus::Pending))
            .collect();
        let results: Vec<_> = (0..num_txs)
            .map(|_| parking_lot::Mutex::new(None))
            .collect();

        Self {
            num_txs,
            statuses,
            results,
            mv_hashmap: MVHashMap::new(),
            max_rounds: 3,
        }
    }

    pub fn record_result(&self, result: SpeculativeResult) {
        let idx = result.tx_index;

        for (account, (balance, nonce)) in &result.access_set.writes {
            self.mv_hashmap.write(*account, idx, *balance, *nonce);
        }

        *self.results[idx].lock() = Some(result);
        *self.statuses[idx].lock() = TxStatus::Executed;
    }

    pub fn validate(&self) -> Vec<usize> {
        let mut needs_reexec = Vec::new();

        for idx in 0..self.num_txs {
            let status = *self.statuses[idx].lock();
            if status != TxStatus::Executed {
                continue;
            }

            let result = self.results[idx].lock();
            if let Some(ref res) = *result {
                let mut valid = true;

                for (account, (read_bal, read_nonce, read_from_idx)) in &res.access_set.reads {
                    if let Some((latest_bal, latest_nonce, writer_idx)) =
                        self.mv_hashmap.read(account, idx)
                    {
                        if writer_idx != *read_from_idx
                            || latest_bal != *read_bal
                            || latest_nonce != *read_nonce
                        {
                            valid = false;
                            break;
                        }
                    }
                }

                if valid {
                    drop(result);
                    *self.statuses[idx].lock() = TxStatus::Validated;
                } else {
                    drop(result);
                    *self.statuses[idx].lock() = TxStatus::NeedsReExecution;
                    needs_reexec.push(idx);
                }
            }
        }

        needs_reexec
    }

    pub fn pending_indices(&self) -> Vec<usize> {
        (0..self.num_txs)
            .filter(|idx| {
                let s = *self.statuses[*idx].lock();
                s == TxStatus::Pending || s == TxStatus::NeedsReExecution
            })
            .collect()
    }

    pub fn finalize(&self) -> (Vec<SpeculativeResult>, Vec<usize>) {
        let mut validated = Vec::new();
        let mut unresolved = Vec::new();

        for idx in 0..self.num_txs {
            let status = *self.statuses[idx].lock();
            match status {
                TxStatus::Validated => {
                    if let Some(result) = self.results[idx].lock().clone() {
                        if result.success {
                            validated.push(result);
                        } else {
                            unresolved.push(idx);
                        }
                    } else {
                        unresolved.push(idx);
                    }
                }
                _ => {
                    unresolved.push(idx);
                }
            }
        }

        (validated, unresolved)
    }

    pub fn num_txs(&self) -> usize {
        self.num_txs
    }

    pub fn max_rounds(&self) -> usize {
        self.max_rounds
    }

    pub fn mv_hashmap(&self) -> &MVHashMap {
        &self.mv_hashmap
    }
}

fn speculative_execute_tx(
    tx_index: usize,
    tx: &Transaction,
    base_accounts: &DashMap<[u8; 32], Account>,
    mv_hashmap: &MVHashMap,
) -> SpeculativeResult {
    let mut access_set = SpeculativeTxAccessSet::default();
    let mut success = true;
    let gas_used = 21_000u64;

    match &tx.body {
        TxBody::Transfer(body) => {
            let sender = tx.from.0;
            let receiver = body.to.0;

            let (sender_bal, sender_nonce, read_from) =
                if let Some((bal, nonce, from_idx)) = mv_hashmap.read(&sender, tx_index) {
                    (bal, nonce, from_idx)
                } else if let Some(acc) = base_accounts.get(&sender) {
                    (acc.balance, acc.nonce, usize::MAX)
                } else {
                    success = false;
                    (0, 0, usize::MAX)
                };
            access_set
                .reads
                .insert(sender, (sender_bal, sender_nonce, read_from));

            let (recv_bal, recv_nonce, recv_from) =
                if let Some((bal, nonce, from_idx)) = mv_hashmap.read(&receiver, tx_index) {
                    (bal, nonce, from_idx)
                } else if let Some(acc) = base_accounts.get(&receiver) {
                    (acc.balance, acc.nonce, usize::MAX)
                } else {
                    (0, 0, usize::MAX)
                };
            if receiver != sender {
                access_set
                    .reads
                    .insert(receiver, (recv_bal, recv_nonce, recv_from));
            }

            if success && sender_bal >= body.amount && sender_nonce == tx.nonce {
                access_set
                    .writes
                    .insert(sender, (sender_bal - body.amount, sender_nonce + 1));
                if receiver == sender {
                    let entry = access_set.writes.get_mut(&sender).unwrap();
                    entry.0 += body.amount;
                } else {
                    access_set
                        .writes
                        .insert(receiver, (recv_bal + body.amount, recv_nonce));
                }
            } else {
                success = false;
            }
        }
        TxBody::Settle(body) => {
            let sender = tx.from.0;
            let receiver = body.agent_id.0;

            let (sender_bal, sender_nonce, read_from) =
                if let Some((bal, nonce, from_idx)) = mv_hashmap.read(&sender, tx_index) {
                    (bal, nonce, from_idx)
                } else if let Some(acc) = base_accounts.get(&sender) {
                    (acc.balance, acc.nonce, usize::MAX)
                } else {
                    success = false;
                    (0, 0, usize::MAX)
                };
            access_set
                .reads
                .insert(sender, (sender_bal, sender_nonce, read_from));

            let (recv_bal, recv_nonce, recv_from) =
                if let Some((bal, nonce, from_idx)) = mv_hashmap.read(&receiver, tx_index) {
                    (bal, nonce, from_idx)
                } else if let Some(acc) = base_accounts.get(&receiver) {
                    (acc.balance, acc.nonce, usize::MAX)
                } else {
                    (0, 0, usize::MAX)
                };
            if receiver != sender {
                access_set
                    .reads
                    .insert(receiver, (recv_bal, recv_nonce, recv_from));
            }

            if success && sender_bal >= body.amount && sender_nonce == tx.nonce {
                access_set
                    .writes
                    .insert(sender, (sender_bal - body.amount, sender_nonce + 1));
                if receiver == sender {
                    let entry = access_set.writes.get_mut(&sender).unwrap();
                    entry.0 += body.amount;
                } else {
                    access_set
                        .writes
                        .insert(receiver, (recv_bal + body.amount, recv_nonce));
                }
            } else {
                success = false;
            }
        }
        _ => {
            return SpeculativeResult {
                tx_index,
                success: false,
                access_set,
                gas_used: 0,
            };
        }
    }

    SpeculativeResult {
        tx_index,
        success,
        access_set,
        gas_used,
    }
}

pub fn execute_speculative(
    transactions: &[Transaction],
    accounts: &DashMap<[u8; 32], Account>,
) -> (Vec<SpeculativeResult>, Vec<usize>) {
    if transactions.is_empty() {
        return (vec![], vec![]);
    }

    let scheduler = SpeculativeScheduler::new(transactions.len());

    let results: Vec<SpeculativeResult> = transactions
        .iter()
        .enumerate()
        .map(|(idx, tx)| speculative_execute_tx(idx, tx, accounts, scheduler.mv_hashmap()))
        .collect();

    for result in results {
        scheduler.record_result(result);
    }

    for _round in 0..scheduler.max_rounds() {
        let conflicts = scheduler.validate();
        if conflicts.is_empty() {
            break;
        }

        for idx in &conflicts {
            let result =
                speculative_execute_tx(*idx, &transactions[*idx], accounts, scheduler.mv_hashmap());
            scheduler.record_result(result);
        }
    }

    scheduler.validate();
    scheduler.finalize()
}

#[cfg(test)]
mod speculative_tests {
    use super::*;
    use arc_crypto::hash_bytes;
    use arc_types::Address;

    fn addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    #[test]
    fn test_mvhashmap_write_read() {
        let mv = MVHashMap::new();
        let a = [1u8; 32];

        mv.write(a, 0, 1000, 0);
        mv.write(a, 2, 900, 1);

        let result = mv.read(&a, 1);
        assert_eq!(result, Some((1000, 0, 0)));

        let result = mv.read(&a, 3);
        assert_eq!(result, Some((900, 1, 2)));

        let result = mv.read(&a, 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_mvhashmap_overwrite() {
        let mv = MVHashMap::new();
        let a = [1u8; 32];

        mv.write(a, 0, 1000, 0);
        mv.write(a, 0, 900, 1);

        let result = mv.read(&a, 1);
        assert_eq!(result, Some((900, 1, 0)));
    }

    #[test]
    fn test_mvhashmap_clear() {
        let mv = MVHashMap::new();
        let a = [1u8; 32];

        mv.write(a, 0, 1000, 0);
        assert!(mv.read(&a, 1).is_some());

        mv.clear();
        assert!(mv.read(&a, 1).is_none());
    }

    #[test]
    fn test_speculative_scheduler_no_conflicts() {
        let scheduler = SpeculativeScheduler::new(3);

        for i in 0..3 {
            let mut access_set = SpeculativeTxAccessSet::default();
            let mut a = [0u8; 32];
            a[0] = i as u8;
            access_set.reads.insert(a, (1000, 0, usize::MAX));
            access_set.writes.insert(a, (900, 1));

            scheduler.record_result(SpeculativeResult {
                tx_index: i,
                success: true,
                access_set,
                gas_used: 21000,
            });
        }

        let conflicts = scheduler.validate();
        assert!(conflicts.is_empty());

        let (validated, unresolved) = scheduler.finalize();
        assert_eq!(validated.len(), 3);
        assert!(unresolved.is_empty());
    }

    #[test]
    fn test_speculative_scheduler_with_conflict() {
        let scheduler = SpeculativeScheduler::new(2);
        let shared_addr = [1u8; 32];

        let mut access0 = SpeculativeTxAccessSet::default();
        access0.reads.insert(shared_addr, (1000, 0, usize::MAX));
        access0.writes.insert(shared_addr, (900, 1));
        scheduler.record_result(SpeculativeResult {
            tx_index: 0,
            success: true,
            access_set: access0,
            gas_used: 21000,
        });

        let mut access1 = SpeculativeTxAccessSet::default();
        access1.reads.insert(shared_addr, (1000, 0, usize::MAX));
        access1.writes.insert(shared_addr, (800, 1));
        scheduler.record_result(SpeculativeResult {
            tx_index: 1,
            success: true,
            access_set: access1,
            gas_used: 21000,
        });

        let conflicts = scheduler.validate();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0], 1);
    }

    #[test]
    fn test_speculative_execute_disjoint_transfers() {
        let accounts: DashMap<[u8; 32], Account> = DashMap::new();
        let sender1 = addr(1);
        let sender2 = addr(2);
        let recv1 = addr(3);
        let recv2 = addr(4);

        accounts.insert(sender1.0, Account::new(sender1, 10_000));
        accounts.insert(sender2.0, Account::new(sender2, 10_000));
        accounts.insert(recv1.0, Account::new(recv1, 0));
        accounts.insert(recv2.0, Account::new(recv2, 0));

        let txs = vec![
            Transaction::new_transfer(sender1, recv1, 100, 0),
            Transaction::new_transfer(sender2, recv2, 200, 0),
        ];

        let (results, unresolved) = execute_speculative(&txs, &accounts);
        assert_eq!(results.len(), 2);
        assert!(unresolved.is_empty());
        assert!(results.iter().all(|r| r.success));
    }

    #[test]
    fn test_speculative_execute_shared_receiver() {
        let accounts: DashMap<[u8; 32], Account> = DashMap::new();
        let sender1 = addr(1);
        let sender2 = addr(2);
        let receiver = addr(3);

        accounts.insert(sender1.0, Account::new(sender1, 10_000));
        accounts.insert(sender2.0, Account::new(sender2, 10_000));
        accounts.insert(receiver.0, Account::new(receiver, 0));

        let txs = vec![
            Transaction::new_transfer(sender1, receiver, 100, 0),
            Transaction::new_transfer(sender2, receiver, 200, 0),
        ];

        let (results, unresolved) = execute_speculative(&txs, &accounts);
        let total = results.len() + unresolved.len();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_speculative_execute_insufficient_balance() {
        let accounts: DashMap<[u8; 32], Account> = DashMap::new();
        let sender = addr(1);
        let receiver = addr(2);

        accounts.insert(sender.0, Account::new(sender, 50));
        accounts.insert(receiver.0, Account::new(receiver, 0));

        let txs = vec![Transaction::new_transfer(sender, receiver, 100, 0)];

        let (results, unresolved) = execute_speculative(&txs, &accounts);
        assert_eq!(results.len() + unresolved.len(), 1);
        if !results.is_empty() {
            assert!(!results[0].success);
        }
    }

    #[test]
    fn test_speculative_routes_unsupported_tx_to_unresolved() {
        use arc_types::transaction::{InferenceRequestBody, TxBody, TxType};

        let accounts: DashMap<[u8; 32], Account> = DashMap::new();
        let sender = addr(1);
        accounts.insert(sender.0, Account::new(sender, 1_000_000));

        let mut tx = Transaction {
            tx_type: TxType::InferenceRequest,
            from: sender,
            nonce: 0,
            body: TxBody::InferenceRequest(InferenceRequestBody {
                request_id: [7u8; 32],
                model_id: hash_bytes(b"arc-32L-test"),
                input_hash: hash_bytes(b"hello"),
                input_blob: b"hello".to_vec(),
                max_tokens: 4,
                tier: 1,
                max_reward: 10,
                deadline_blocks: 20,
                committee_size: 1,
            }),
            fee: 0,
            gas_limit: 0,
            hash: arc_crypto::Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();

        let (validated, unresolved) = execute_speculative(&[tx], &accounts);
        assert!(
            validated.is_empty(),
            "InferenceRequest must NOT be returned as a validated speculative \
             result — it would silently fail the receipt instead of falling \
             through to sequential apply (the tier 1 'no such request' bug)"
        );
        assert_eq!(
            unresolved,
            vec![0],
            "InferenceRequest must be routed to `unresolved` so the caller's \
             sequential fallback can actually apply it"
        );
    }

    #[test]
    fn test_speculative_execute_empty() {
        let accounts: DashMap<[u8; 32], Account> = DashMap::new();
        let (results, unresolved) = execute_speculative(&[], &accounts);
        assert!(results.is_empty());
        assert!(unresolved.is_empty());
    }

    #[test]
    fn test_tx_status_transitions() {
        let scheduler = SpeculativeScheduler::new(1);

        assert_eq!(scheduler.pending_indices(), vec![0]);

        let mut access_set = SpeculativeTxAccessSet::default();
        let a = [1u8; 32];
        access_set.reads.insert(a, (1000, 0, usize::MAX));
        access_set.writes.insert(a, (900, 1));

        scheduler.record_result(SpeculativeResult {
            tx_index: 0,
            success: true,
            access_set,
            gas_used: 21000,
        });

        assert!(scheduler.pending_indices().is_empty());

        let conflicts = scheduler.validate();
        assert!(conflicts.is_empty());

        let (validated, unresolved) = scheduler.finalize();
        assert_eq!(validated.len(), 1);
        assert!(unresolved.is_empty());
    }
}
