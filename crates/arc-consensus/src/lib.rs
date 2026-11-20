use arc_crypto::{Hash256, KeyPair, Signature as CryptoSignature};
use arc_types::{Transaction, TxBody, strict_supermajority_threshold};
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tracing::{debug, info, warn};

pub mod beacon;
pub mod data_availability;
pub mod pending;
pub mod security;
pub mod subnet;
pub mod view_change;
pub use data_availability::*;
pub use security::*;

pub type Address = Hash256;

pub const STAKE_SPARK: u64 = 500_000;

pub const STAKE_ARC: u64 = 5_000_000;

pub const STAKE_CORE: u64 = 50_000_000;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConsensusError {
    #[error("invalid block: {0}")]
    InvalidBlock(String),

    #[error("invalid block: block {hash} has {missing} missing or wrong-round parents")]
    MissingParents { hash: Hash256, missing: usize },

    #[error(
        "invalid block: round {round} is too far ahead (current={current}); authenticated state sync required"
    )]
    RoundTooFarAhead { round: u64, current: u64 },

    #[error("insufficient parents: need >= 2f+1 references from previous round")]
    InsufficientParents,

    #[error("invalid round: block round does not match expectations")]
    InvalidRound,

    #[error("author is not a registered validator")]
    NotValidator,

    #[error("duplicate block: already exists in DAG")]
    DuplicateBlock,

    #[error("invalid signature")]
    InvalidSignature,

    #[error("cross-shard lock not found: {0}")]
    CrossShardLockNotFound(String),

    #[error("cross-shard lock already exists: {0}")]
    CrossShardLockAlreadyExists(String),

    #[error("cross-shard lock failed: {0}")]
    CrossShardLockFailed(String),

    #[error("slash error: {0}")]
    SlashError(String),

    #[error("MEV ordering violation: {0}")]
    MevOrderingViolation(String),

    #[error("cross-shard lock expired: {0}")]
    CrossShardLockExpired(String),

    #[error("history block persistence failed: {0}")]
    HistoryPersistence(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StakeTier {
    Spark,
    Arc,
    Core,
}

impl StakeTier {
    pub fn from_stake(stake: u64) -> Option<Self> {
        if stake >= STAKE_CORE {
            Some(StakeTier::Core)
        } else if stake >= STAKE_ARC {
            Some(StakeTier::Arc)
        } else if stake >= STAKE_SPARK {
            Some(StakeTier::Spark)
        } else {
            None
        }
    }

    pub fn can_produce_blocks(&self) -> bool {
        matches!(self, StakeTier::Arc | StakeTier::Core)
    }

    pub fn can_govern(&self) -> bool {
        matches!(self, StakeTier::Core)
    }
}

pub const SLASH_RATE_SPARK: u64 = 10;
pub const SLASH_RATE_ARC: u64 = 20;
pub const SLASH_RATE_CORE: u64 = 30;
pub const LIVENESS_THRESHOLD: u64 = 100;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SlashableOffense {
    DoubleSigning,
    LivenessFault,
    InvalidBlockProposal,
    EquivocationDAG,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlashRecord {
    pub offense: SlashableOffense,
    pub offender: Address,
    pub evidence: Hash256,
    pub round: u64,
    pub timestamp: u64,
    pub slash_amount: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EquivocationProof {
    pub author: Address,
    pub round: u64,
    pub block1_hash: Hash256,
    pub block2_hash: Hash256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Validator {
    pub address: Address,
    pub stake: u64,
    pub tier: StakeTier,
    pub shard_assignment: u16,
}

impl Validator {
    pub fn new(address: Address, stake: u64, shard_assignment: u16) -> Option<Self> {
        let tier = StakeTier::from_stake(stake)?;
        Some(Self {
            address,
            stake,
            tier,
            shard_assignment,
        })
    }
}

fn checked_validator_total_stake(validators: &[Validator]) -> Result<u64, ConsensusError> {
    let mut seen = HashSet::with_capacity(validators.len());
    let mut total = 0u64;
    for validator in validators {
        if StakeTier::from_stake(validator.stake) != Some(validator.tier) {
            return Err(ConsensusError::InvalidBlock(format!(
                "validator {} has invalid stake/tier metadata",
                validator.address
            )));
        }
        if !seen.insert(validator.address) {
            return Err(ConsensusError::InvalidBlock(
                "duplicate validator address in validator set".into(),
            ));
        }
        total = total.checked_add(validator.stake).ok_or_else(|| {
            ConsensusError::InvalidBlock("total validator stake exceeds u64::MAX".into())
        })?;
    }
    Ok(total)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorSet {
    pub validators: Vec<Validator>,
    pub total_stake: u64,
    pub epoch: u64,
    pub quorum: u64,
    pub slashed_total: u64,
    pub slash_history: Vec<SlashRecord>,
}

impl ValidatorSet {
    pub fn new(validators: Vec<Validator>, epoch: u64) -> Self {
        let total_stake = checked_validator_total_stake(&validators)
            .unwrap_or_else(|err| panic!("invalid validator set: {err}"));
        let quorum = strict_supermajority_threshold(total_stake);
        Self {
            validators,
            total_stake,
            epoch,
            quorum,
            slashed_total: 0,
            slash_history: Vec::new(),
        }
    }

    pub fn get_validator(&self, address: &Address) -> Option<&Validator> {
        self.validators.iter().find(|v| v.address == *address)
    }

    pub fn is_validator(&self, address: &Address) -> bool {
        self.get_validator(address).is_some()
    }

    pub fn can_produce_blocks(&self, address: &Address) -> bool {
        self.get_validator(address)
            .map(|v| v.tier.can_produce_blocks())
            .unwrap_or(false)
    }

    pub fn fault_tolerance_stake(&self) -> u64 {
        self.total_stake.saturating_sub(self.quorum)
    }

    pub fn stake_for_addresses(&self, addresses: &[Address]) -> u64 {
        let mut seen = HashSet::with_capacity(addresses.len());
        addresses
            .iter()
            .filter(|address| seen.insert(**address))
            .filter_map(|address| self.get_validator(address).map(|validator| validator.stake))
            .try_fold(0u64, u64::checked_add)
            .expect("unique validator stake cannot exceed checked total stake")
    }

    pub fn has_quorum(&self, addresses: &[Address]) -> bool {
        self.stake_for_addresses(addresses) >= self.quorum
    }

    pub fn len(&self) -> usize {
        self.validators.len()
    }

    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    pub fn block_producers(&self) -> Vec<&Validator> {
        self.validators
            .iter()
            .filter(|v| v.tier.can_produce_blocks())
            .collect()
    }

    pub fn get_validator_mut(&mut self, address: &Address) -> Option<&mut Validator> {
        self.validators.iter_mut().find(|v| v.address == *address)
    }

    pub fn slash_rate(tier: &StakeTier) -> u64 {
        match tier {
            StakeTier::Spark => SLASH_RATE_SPARK,
            StakeTier::Arc => SLASH_RATE_ARC,
            StakeTier::Core => SLASH_RATE_CORE,
        }
    }

    pub fn report_offense(
        &mut self,
        offender: Address,
        offense: SlashableOffense,
        evidence: Hash256,
        round: u64,
        timestamp: u64,
    ) -> Result<SlashRecord, ConsensusError> {
        let validator = self
            .validators
            .iter()
            .find(|v| v.address == offender)
            .ok_or_else(|| {
                ConsensusError::SlashError(format!("validator {:?} not found", offender))
            })?;
        let rate = Self::slash_rate(&validator.tier);
        let slash_amount = ((validator.stake as u128 * rate as u128) / 100) as u64;
        let new_slashed_total = self
            .slashed_total
            .checked_add(slash_amount)
            .ok_or_else(|| {
                ConsensusError::SlashError("cumulative slashed stake exceeds u64::MAX".into())
            })?;
        let record = SlashRecord {
            offense,
            offender,
            evidence,
            round,
            timestamp,
            slash_amount,
        };
        self.slashed_total = new_slashed_total;
        self.slash_history.push(record.clone());
        Ok(record)
    }

    pub fn apply_slash(&mut self, offender: &Address, slash_amount: u64) {
        let mut removed = false;
        if let Some(validator) = self.validators.iter_mut().find(|v| v.address == *offender) {
            validator.stake = validator.stake.saturating_sub(slash_amount);
            if validator.stake < STAKE_SPARK {
                warn!(
                    address = %offender,
                    remaining_stake = validator.stake,
                    "Validator stake below minimum after slashing - removing from validator set"
                );
                removed = true;
            } else {
                validator.tier = StakeTier::from_stake(validator.stake).unwrap_or(StakeTier::Spark);
                info!(
                    address = %offender,
                    new_stake = validator.stake,
                    new_tier = ?validator.tier,
                    "Validator stake reduced by slashing"
                );
            }
        }
        if removed {
            self.validators.retain(|v| v.address != *offender);
        }
        self.total_stake = checked_validator_total_stake(&self.validators)
            .expect("slashing cannot overflow or duplicate validator stake");
        self.quorum = strict_supermajority_threshold(self.total_stake);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsensusDomain {
    pub domain_hash: Hash256,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
}

impl ConsensusDomain {
    pub fn new(domain_hash: Hash256, recovery_epoch: u64, validator_set_id: u64) -> Self {
        Self {
            domain_hash,
            recovery_epoch,
            validator_set_id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DagBlock {
    pub author: Address,
    pub round: u64,
    pub parents: Vec<Hash256>,
    pub transactions: Vec<Hash256>,
    pub timestamp: u64,
    pub hash: Hash256,
    pub signature: Vec<u8>,
    pub ordering_commitment: Hash256,
}

impl DagBlock {
    pub fn compute_hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-dag-block-v1");
        hasher.update(self.author.as_ref());
        hasher.update(&self.round.to_le_bytes());
        for parent in &self.parents {
            hasher.update(parent.as_ref());
        }
        for tx in &self.transactions {
            hasher.update(tx.as_ref());
        }
        hasher.update(&self.timestamp.to_le_bytes());
        hasher.update(self.ordering_commitment.as_ref());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn compute_hash_in_domain(&self, domain: &ConsensusDomain) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-dag-block-v3");
        hasher.update(domain.domain_hash.as_ref());
        hasher.update(&domain.recovery_epoch.to_be_bytes());
        hasher.update(&domain.validator_set_id.to_be_bytes());
        hasher.update(self.author.as_ref());
        hasher.update(&self.round.to_le_bytes());
        for parent in &self.parents {
            hasher.update(parent.as_ref());
        }
        for tx in &self.transactions {
            hasher.update(tx.as_ref());
        }
        hasher.update(&self.timestamp.to_le_bytes());
        hasher.update(self.ordering_commitment.as_ref());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn state_decision_commitment(&self, domain: &ConsensusDomain) -> Hash256 {
        Self::decision_commitment(domain, &self.hash, self.round)
    }

    pub fn decision_commitment(domain: &ConsensusDomain, anchor: &Hash256, round: u64) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-dag-state-decision-v3");
        hasher.update(domain.domain_hash.as_ref());
        hasher.update(&domain.recovery_epoch.to_be_bytes());
        hasher.update(&domain.validator_set_id.to_be_bytes());
        hasher.update(anchor.as_ref());
        hasher.update(&round.to_be_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn compute_ordering_commitment(transactions: &[Hash256]) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-ordering-commitment-v1");
        for tx in transactions {
            hasher.update(tx.as_ref());
        }
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn verify_hash(&self) -> bool {
        self.hash == self.compute_hash()
    }

    pub fn verify_hash_in_domain(&self, domain: &ConsensusDomain) -> bool {
        self.hash == self.compute_hash_in_domain(domain)
    }

    pub fn verify_ordering(&self) -> bool {
        let is_sorted = self.transactions.windows(2).all(|w| w[0].0 <= w[1].0);
        if !is_sorted {
            return false;
        }
        self.ordering_commitment == Self::compute_ordering_commitment(&self.transactions)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrossShardReceipt {
    pub source_shard: u16,
    pub source_block: Hash256,
    pub tx_hash: Hash256,
    pub amount: u64,
    pub recipient: Address,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CrossShardStatus {
    Locked,
    Committed,
    Aborted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrossShardProof {
    pub tx_hash: Hash256,
    pub source_shard: u16,
    pub target_shard: u16,
    pub source_block_hash: Hash256,
    pub source_round: u64,
    pub status: CrossShardStatus,
    pub lock_hash: Hash256,
    pub inclusion_proof: Vec<u8>,
    pub locked_at_ms: u64,
    pub locked_at_round: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinalityProof {
    pub block_hash: Hash256,
    pub round: u64,
    pub height: u64,
    pub quorum_signatures: Vec<(Address, Vec<u8>)>,
    pub signing_stake: u64,
    pub total_stake: u64,
}

impl FinalityProof {
    pub fn has_sufficient_stake(&self) -> bool {
        let quorum = strict_supermajority_threshold(self.total_stake);
        self.signing_stake >= quorum
    }
}

pub const PRUNE_DEPTH: u64 = 100;

pub const DEFAULT_RETAINED_FINALITY_HEIGHTS: u64 = 8192;

const FINALITY_PRUNE_EVERY: u64 = 256;

pub const DEFAULT_RETAINED_ROUNDS: u64 = 4_096;

pub const SECURITY_HISTORY_DEPTH: u64 = 10 * PRUNE_DEPTH;

const CROSS_SHARD_LOCK_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeRole {
    Proposer,
    Verifier,
    Full,
}

pub struct ConsensusEngine {
    dag: DashMap<Hash256, DagBlock>,
    rounds: DashMap<u64, Vec<Hash256>>,
    committed: RwLock<Vec<Hash256>>,
    last_committed_round: AtomicU64,
    current_round: AtomicU64,
    validator_set: RwLock<ValidatorSet>,
    frozen_validator_set: RwLock<ValidatorSet>,
    pending_validators: RwLock<Vec<Validator>>,
    local_address: Address,
    local_keypair: Option<KeyPair>,
    testnet_mode: bool,
    validator_keys: DashMap<Address, [u8; 32]>,
    author_round_blocks: DashMap<(Address, u64), Hash256>,
    pub da_commitments: DashMap<Hash256, data_availability::DACommitment>,
    pub pending_cross_shard: DashMap<Hash256, CrossShardProof>,
    pub completed_cross_shard: DashMap<Hash256, CrossShardProof>,
    finality_proofs: DashMap<Hash256, FinalityProof>,
    round_start: RwLock<std::time::Instant>,
    force_advanced: std::sync::atomic::AtomicBool,
    withholding_detector: parking_lot::Mutex<WithholdingDetector>,
    checkpoint_registry: parking_lot::Mutex<CheckpointRegistry>,
    stake_tracker: parking_lot::Mutex<StakeTracker>,
    node_role: NodeRole,
    consensus_domain: RwLock<Option<ConsensusDomain>>,
    certificate_domain: RwLock<Option<ConsensusDomain>>,
    skip_certificates: DashMap<(u64, Address), view_change::SkipCertificate>,
    finality_certificates: DashMap<u64, view_change::FinalityCertificate>,
    highest_finalized_height: AtomicU64,
    retained_finality_heights: AtomicU64,
    retained_rounds: AtomicU64,
    recovery_bootstrap_round: RwLock<Option<u64>>,
    restart_base_round: RwLock<Option<u64>>,
    proposal_floor: AtomicU64,
    dag_bootstrapping: std::sync::atomic::AtomicBool,
    local_recovery_boundary_round: RwLock<Option<u64>>,
    local_recovery_replay_active: std::sync::atomic::AtomicBool,
}

impl ConsensusEngine {
    pub fn new_with_keypair(
        validator_set: ValidatorSet,
        local_address: Address,
        local_keypair: KeyPair,
    ) -> Self {
        info!(
            epoch = validator_set.epoch,
            validators = validator_set.len(),
            total_stake = validator_set.total_stake,
            quorum = validator_set.quorum,
            "ConsensusEngine initialized with signing keypair (strict mode)"
        );
        let frozen = validator_set.clone();
        Self {
            dag: DashMap::new(),
            rounds: DashMap::new(),
            committed: RwLock::new(Vec::new()),
            last_committed_round: AtomicU64::new(0),
            current_round: AtomicU64::new(0),
            validator_set: RwLock::new(validator_set),
            frozen_validator_set: RwLock::new(frozen),
            pending_validators: RwLock::new(Vec::new()),
            local_address,
            local_keypair: Some(local_keypair),
            validator_keys: DashMap::new(),
            author_round_blocks: DashMap::new(),
            da_commitments: DashMap::new(),
            pending_cross_shard: DashMap::new(),
            completed_cross_shard: DashMap::new(),
            finality_proofs: DashMap::new(),
            round_start: RwLock::new(std::time::Instant::now()),
            force_advanced: std::sync::atomic::AtomicBool::new(false),
            withholding_detector: parking_lot::Mutex::new(WithholdingDetector::new()),
            checkpoint_registry: parking_lot::Mutex::new(CheckpointRegistry::new()),
            stake_tracker: parking_lot::Mutex::new(StakeTracker::new()),
            node_role: NodeRole::Full,
            testnet_mode: false,
            consensus_domain: RwLock::new(None),
            certificate_domain: RwLock::new(None),
            skip_certificates: DashMap::new(),
            retained_rounds: AtomicU64::new(DEFAULT_RETAINED_ROUNDS),
            finality_certificates: DashMap::new(),
            highest_finalized_height: AtomicU64::new(0),
            retained_finality_heights: AtomicU64::new(DEFAULT_RETAINED_FINALITY_HEIGHTS),
            recovery_bootstrap_round: RwLock::new(None),
            restart_base_round: RwLock::new(None),
            proposal_floor: AtomicU64::new(0),
            dag_bootstrapping: std::sync::atomic::AtomicBool::new(false),
            local_recovery_boundary_round: RwLock::new(None),
            local_recovery_replay_active: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn new(validator_set: ValidatorSet, local_address: Address) -> Self {
        info!(
            epoch = validator_set.epoch,
            validators = validator_set.len(),
            total_stake = validator_set.total_stake,
            quorum = validator_set.quorum,
            "ConsensusEngine initialized (strict mode)"
        );
        let frozen = validator_set.clone();
        Self {
            dag: DashMap::new(),
            rounds: DashMap::new(),
            committed: RwLock::new(Vec::new()),
            last_committed_round: AtomicU64::new(0),
            current_round: AtomicU64::new(0),
            validator_set: RwLock::new(validator_set),
            frozen_validator_set: RwLock::new(frozen),
            pending_validators: RwLock::new(Vec::new()),
            local_address,
            local_keypair: None,
            validator_keys: DashMap::new(),
            author_round_blocks: DashMap::new(),
            da_commitments: DashMap::new(),
            pending_cross_shard: DashMap::new(),
            completed_cross_shard: DashMap::new(),
            finality_proofs: DashMap::new(),
            round_start: RwLock::new(std::time::Instant::now()),
            force_advanced: std::sync::atomic::AtomicBool::new(false),
            withholding_detector: parking_lot::Mutex::new(WithholdingDetector::new()),
            checkpoint_registry: parking_lot::Mutex::new(CheckpointRegistry::new()),
            stake_tracker: parking_lot::Mutex::new(StakeTracker::new()),
            node_role: NodeRole::Full,
            testnet_mode: false,
            consensus_domain: RwLock::new(None),
            certificate_domain: RwLock::new(None),
            skip_certificates: DashMap::new(),
            retained_rounds: AtomicU64::new(DEFAULT_RETAINED_ROUNDS),
            finality_certificates: DashMap::new(),
            highest_finalized_height: AtomicU64::new(0),
            retained_finality_heights: AtomicU64::new(DEFAULT_RETAINED_FINALITY_HEIGHTS),
            recovery_bootstrap_round: RwLock::new(None),
            restart_base_round: RwLock::new(None),
            proposal_floor: AtomicU64::new(0),
            dag_bootstrapping: std::sync::atomic::AtomicBool::new(false),
            local_recovery_boundary_round: RwLock::new(None),
            local_recovery_replay_active: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn new_testnet(validator_set: ValidatorSet, local_address: Address) -> Self {
        info!(
            epoch = validator_set.epoch,
            validators = validator_set.len(),
            "ConsensusEngine initialized (testnet mode)"
        );
        let mut engine = Self::new(validator_set, local_address);
        engine.testnet_mode = true;
        engine
    }

    pub fn new_testnet_with_keypair(
        validator_set: ValidatorSet,
        local_address: Address,
        local_keypair: KeyPair,
    ) -> Self {
        let mut engine = Self::new_with_keypair(validator_set, local_address, local_keypair);
        engine.testnet_mode = true;
        engine
    }

    pub fn register_validator_key(&self, address: Address, ed25519_pubkey: [u8; 32]) {
        self.validator_keys.insert(address, ed25519_pubkey);
        debug!(%address, "Registered validator public key");
    }

    pub fn install_consensus_domain(&self, domain: ConsensusDomain) -> Result<(), ConsensusError> {
        let mut active = self.consensus_domain.write();
        if let Some(existing) = active.as_ref() {
            if existing == &domain {
                return self.install_certificate_domain(domain);
            }
            return Err(ConsensusError::InvalidBlock(
                "consensus recovery domain is already bound to another epoch/set".into(),
            ));
        }
        if !self.dag.is_empty()
            || self.current_round.load(Ordering::SeqCst) != 0
            || self.last_committed_round.load(Ordering::SeqCst) != 0
        {
            return Err(ConsensusError::InvalidBlock(
                "consensus recovery domain must be installed before DAG/cursor recovery".into(),
            ));
        }
        self.install_certificate_domain(domain)?;
        *active = Some(domain);
        Ok(())
    }

    pub fn consensus_domain(&self) -> Option<ConsensusDomain> {
        *self.consensus_domain.read()
    }

    pub fn install_recovery_cursor(
        &self,
        source_consensus_round: u64,
    ) -> Result<u64, ConsensusError> {
        if self.consensus_domain.read().is_none() {
            return Err(ConsensusError::InvalidBlock(
                "recovery cursor requires an installed consensus domain".into(),
            ));
        }
        let bootstrap_round = source_consensus_round.checked_add(1).ok_or_else(|| {
            ConsensusError::InvalidBlock("recovery source consensus round overflows u64".into())
        })?;

        let mut active = self.recovery_bootstrap_round.write();
        if let Some(existing) = *active {
            if existing == bootstrap_round
                && self.current_round.load(Ordering::SeqCst) == bootstrap_round
                && self.last_committed_round.load(Ordering::SeqCst) == bootstrap_round
            {
                return Ok(bootstrap_round);
            }
            return Err(ConsensusError::InvalidBlock(
                "recovery cursor is already bound to another position".into(),
            ));
        }
        if !self.dag.is_empty()
            || self.current_round.load(Ordering::SeqCst) != 0
            || self.last_committed_round.load(Ordering::SeqCst) != 0
        {
            return Err(ConsensusError::InvalidBlock(
                "recovery cursor must be installed before DAG/local-WAL recovery".into(),
            ));
        }

        self.current_round.store(bootstrap_round, Ordering::SeqCst);
        self.last_committed_round
            .store(bootstrap_round, Ordering::SeqCst);
        *active = Some(bootstrap_round);
        self.reset_round_timer();
        Ok(bootstrap_round)
    }

    pub fn install_recovery_generation_cursor(
        &self,
        retention_floor_round: u64,
        current_round: u64,
        next_commit_round: u64,
    ) -> Result<(), ConsensusError> {
        const MAX_RETAINED_ROUND_SPAN: u64 = 4_096;
        let bootstrap = self.recovery_bootstrap_round.read().ok_or_else(|| {
            ConsensusError::InvalidBlock(
                "local recovery generation requires the signed bootstrap cursor".into(),
            )
        })?;
        if self.consensus_domain.read().is_none()
            || !self.dag.is_empty()
            || self.local_recovery_replay_active.load(Ordering::SeqCst)
            || self.local_recovery_boundary_round.read().is_some()
        {
            return Err(ConsensusError::InvalidBlock(
                "local recovery generation must be installed once on an empty domain-bound engine"
                    .into(),
            ));
        }
        if self.current_round.load(Ordering::SeqCst) != bootstrap
            || self.last_committed_round.load(Ordering::SeqCst) != bootstrap
            || retention_floor_round < bootstrap
            || retention_floor_round > next_commit_round
            || next_commit_round > current_round
            || current_round.saturating_sub(retention_floor_round) > MAX_RETAINED_ROUND_SPAN
        {
            return Err(ConsensusError::InvalidBlock(
                "local recovery generation cursors are inconsistent or outside the bounded window"
                    .into(),
            ));
        }

        *self.local_recovery_boundary_round.write() = Some(retention_floor_round);
        self.current_round.store(current_round, Ordering::SeqCst);
        self.last_committed_round
            .store(next_commit_round, Ordering::SeqCst);
        self.local_recovery_replay_active
            .store(true, Ordering::SeqCst);
        self.reset_round_timer();
        Ok(())
    }

    pub fn finish_recovery_generation_replay(&self) -> Result<(), ConsensusError> {
        if !self
            .local_recovery_replay_active
            .swap(false, Ordering::SeqCst)
        {
            return Err(ConsensusError::InvalidBlock(
                "local recovery generation replay is not active".into(),
            ));
        }
        Ok(())
    }

    pub fn is_recovery_bootstrap_round(&self, round: u64) -> bool {
        self.recovery_bootstrap_round.read().as_ref() == Some(&round)
    }

    pub fn set_restart_base_round(&self, round: u64) -> Result<(), ConsensusError> {
        if round == 0
            || !self.dag.is_empty()
            || self.recovery_bootstrap_round.read().is_some()
            || round != self.last_committed_round.load(Ordering::SeqCst)
        {
            return Err(ConsensusError::InvalidBlock(
                "a restart base round needs an empty DAG and must be this node's own commit \
                 cursor"
                    .into(),
            ));
        }
        *self.restart_base_round.write() = Some(round);
        Ok(())
    }

    pub fn set_dag_bootstrapping(&self, active: bool) {
        self.dag_bootstrapping.store(active, Ordering::SeqCst);
    }

    pub fn dag_bootstrapping(&self) -> bool {
        self.dag_bootstrapping.load(Ordering::SeqCst)
    }

    pub fn restart_base_round(&self) -> Option<u64> {
        let base = (*self.restart_base_round.read())?;
        self.is_restart_base_round(base).then_some(base)
    }

    pub fn close_restart_base_round(&self) {
        *self.restart_base_round.write() = None;
    }

    pub fn find_committed_anchor(
        &self,
        proof_hash: &Hash256,
        domain: &ConsensusDomain,
    ) -> Option<(Hash256, u64)> {
        self.dag
            .iter()
            .find(|entry| entry.value().state_decision_commitment(domain) == *proof_hash)
            .map(|entry| (entry.value().hash, entry.value().round))
    }

    pub fn check_rebase(&self, anchor_round: u64) -> Result<u64, ConsensusError> {
        let cursor = anchor_round.checked_add(1).ok_or_else(|| {
            ConsensusError::InvalidBlock("checkpoint anchor round overflows".into())
        })?;
        if self.recovery_bootstrap_round.read().is_some() {
            return Err(ConsensusError::InvalidBlock(
                "a recovery-domain engine is repositioned by its signed recovery path".into(),
            ));
        }
        if cursor <= self.last_committed_round.load(Ordering::SeqCst) {
            return Err(ConsensusError::InvalidBlock(
                "a checkpoint must move the commit cursor forward".into(),
            ));
        }
        Ok(cursor)
    }

    pub fn rebase_to(&self, anchor_round: u64) -> Result<u64, ConsensusError> {
        let cursor = self.check_rebase(anchor_round)?;
        let reached = self.current_round.load(Ordering::SeqCst);
        self.dag.clear();
        self.rounds.clear();
        self.committed.write().clear();
        self.author_round_blocks.clear();
        self.da_commitments.clear();
        self.finality_proofs.clear();
        self.proposal_floor.fetch_max(reached, Ordering::SeqCst);
        self.current_round
            .store(reached.max(cursor), Ordering::SeqCst);
        self.last_committed_round.store(cursor, Ordering::SeqCst);
        *self.restart_base_round.write() = Some(cursor);
        self.reset_round_timer();
        Ok(cursor)
    }

    fn is_restart_base_round(&self, round: u64) -> bool {
        self.restart_base_round.read().as_ref() == Some(&round)
            && self.last_committed_round.load(Ordering::SeqCst) <= round.saturating_add(1)
    }

    pub fn requires_full_round_participation(&self) -> bool {
        self.consensus_domain.read().is_some()
    }

    fn round_participation_satisfies_quorum(
        &self,
        validator_set: &ValidatorSet,
        round: u64,
        authors: &HashSet<Address>,
        stake: u64,
    ) -> bool {
        stake >= validator_set.quorum
            && (!self.requires_full_round_participation()
                || validator_set
                    .validators
                    .iter()
                    .filter(|validator| validator.stake > 0)
                    .all(|validator| {
                        authors.contains(&validator.address)
                            || self.is_excused_for_round(round, &validator.address)
                    }))
    }

    pub fn has_quorum_parents_for_round(&self, round: u64) -> bool {
        let validator_set = self.validator_set.read();
        let mut authors = HashSet::new();
        let mut stake = 0u64;
        for hash in self.blocks_in_round(round) {
            if let Some(block) = self.dag.get(&hash)
                && authors.insert(block.author)
                && let Some(validator) = validator_set.get_validator(&block.author)
            {
                stake = stake
                    .checked_add(validator.stake)
                    .expect("unique parent stake cannot exceed validator-set total");
            }
        }
        self.round_participation_satisfies_quorum(&validator_set, round, &authors, stake)
    }

    pub fn current_round(&self) -> u64 {
        self.current_round.load(Ordering::SeqCst)
    }

    pub fn last_committed_round(&self) -> u64 {
        self.last_committed_round.load(Ordering::SeqCst)
    }

    pub fn restore_round_from_local_wal(&self, round: u64, committed: u64) {
        if committed > round {
            warn!(
                round,
                committed, "Ignoring inconsistent local WAL round cursors"
            );
            return;
        }
        let current = self.current_round.load(Ordering::SeqCst);
        self.proposal_floor.fetch_max(round, Ordering::SeqCst);
        if round > current {
            self.current_round.store(round, Ordering::SeqCst);
            self.last_committed_round.store(committed, Ordering::SeqCst);
            self.reset_round_timer();
            tracing::info!(
                "Restored round cursors from local WAL: round={}, committed={}",
                round,
                committed
            );
        }
    }

    pub fn restore_next_recovery_commit_from_local_wal(&self) -> Result<DagBlock, ConsensusError> {
        if self.consensus_domain.read().is_none()
            || !self.local_recovery_replay_active.load(Ordering::SeqCst)
        {
            return Err(ConsensusError::InvalidBlock(
                "recovery commit replay requires an active bound local generation".into(),
            ));
        }
        self.try_commit_limited(1, false)
            .into_iter()
            .next()
            .ok_or_else(|| {
                ConsensusError::InvalidBlock(
                    "recovery DAG has no certified next canonical decision".into(),
                )
            })
    }

    pub fn restore_recovery_commit_from_local_wal(
        &self,
        block_hash: Hash256,
    ) -> Result<u64, ConsensusError> {
        let block = self.restore_next_recovery_commit_from_local_wal()?;
        if block.hash != block_hash {
            return Err(ConsensusError::InvalidBlock(format!(
                "recovery commit {block_hash} differs from next certified decision {}",
                block.hash
            )));
        }
        Ok(self.last_committed_round())
    }

    pub fn observe_untrusted_round_hint(&self, round: u64, committed: u64) {
        debug!(
            hinted_round = round,
            hinted_committed = committed,
            local_round = self.current_round(),
            local_committed = self.last_committed_round(),
            "Observed unauthenticated round hint; consensus cursors unchanged"
        );
    }

    pub fn frozen_validator_set(&self) -> parking_lot::RwLockReadGuard<'_, ValidatorSet> {
        self.frozen_validator_set.read()
    }

    pub fn queue_validator(&self, validator: Validator) {
        let mut pending = self.pending_validators.write();
        if !pending.iter().any(|v| v.address == validator.address) {
            pending.push(validator);
        }
    }

    pub fn freeze_epoch(&self) {
        let (validators, pending, old_epoch) = {
            let vs = self.validator_set.read();
            let pv = self.pending_validators.read();
            (vs.validators.clone(), pv.clone(), vs.epoch)
        };

        let mut all = validators;
        for pv in &pending {
            if let Some(existing) = all.iter_mut().find(|v| v.address == pv.address) {
                *existing = pv.clone();
            } else {
                all.push(pv.clone());
            }
        }

        let new_epoch = old_epoch
            .checked_add(1)
            .expect("validator epoch cannot advance beyond u64::MAX");
        let new_set = ValidatorSet::new(all, new_epoch);
        info!(
            epoch = new_epoch,
            validators = new_set.len(),
            total_stake = new_set.total_stake,
            quorum = new_set.quorum,
            "Epoch transition: frozen validator set updated"
        );

        *self.validator_set.write() = new_set.clone();
        *self.frozen_validator_set.write() = new_set;
        self.pending_validators.write().clear();
    }

    pub fn validator_set(&self) -> ValidatorSet {
        self.validator_set.read().clone()
    }

    pub fn join_validator(
        &self,
        address: Address,
        stake: u64,
        pubkey: [u8; 32],
        shard: u16,
    ) -> Result<(), ConsensusError> {
        if StakeTier::from_stake(stake).is_none() {
            return Err(ConsensusError::InvalidBlock(format!(
                "stake {} below minimum threshold {}",
                stake, STAKE_SPARK
            )));
        }
        let mut vs = self.validator_set.write();
        if vs.is_validator(&address) {
            return Err(ConsensusError::InvalidBlock(
                "address already in validator set".into(),
            ));
        }
        if vs.len() >= 100 {
            return Err(ConsensusError::InvalidBlock(
                "validator set at capacity (100)".into(),
            ));
        }
        let validator = Validator::new(address, stake, shard)
            .ok_or_else(|| ConsensusError::InvalidBlock("invalid stake".into()))?;
        let new_total_stake = vs.total_stake.checked_add(stake).ok_or_else(|| {
            ConsensusError::InvalidBlock("total validator stake exceeds u64::MAX".into())
        })?;
        vs.validators.push(validator);
        vs.total_stake = new_total_stake;
        vs.quorum = strict_supermajority_threshold(vs.total_stake);
        drop(vs);
        self.register_validator_key(address, pubkey);
        info!(%address, stake, "Validator joined the active set");
        Ok(())
    }

    pub fn leave_validator(&self, address: &Address) -> Result<u64, ConsensusError> {
        let mut vs = self.validator_set.write();
        let stake = vs
            .get_validator(address)
            .map(|v| v.stake)
            .ok_or_else(|| ConsensusError::InvalidBlock("address not in validator set".into()))?;
        vs.validators.retain(|v| v.address != *address);
        vs.total_stake = checked_validator_total_stake(&vs.validators)?;
        vs.quorum = strict_supermajority_threshold(vs.total_stake);
        self.validator_keys.remove(address);
        info!(%address, returned_stake = stake, "Validator left the active set");
        Ok(stake)
    }

    pub fn update_validator_stake(
        &self,
        address: &Address,
        new_stake: u64,
    ) -> Result<(), ConsensusError> {
        if StakeTier::from_stake(new_stake).is_none() {
            return Err(ConsensusError::InvalidBlock(format!(
                "new stake {} below minimum threshold",
                new_stake
            )));
        }
        let mut vs = self.validator_set.write();
        let idx = vs.validators.iter().position(|v| v.address == *address);
        if let Some(idx) = idx {
            let old_stake = vs.validators[idx].stake;
            let new_total_stake = vs
                .total_stake
                .checked_sub(old_stake)
                .and_then(|remaining| remaining.checked_add(new_stake))
                .ok_or_else(|| {
                    ConsensusError::InvalidBlock("total validator stake exceeds u64::MAX".into())
                })?;
            vs.validators[idx].stake = new_stake;
            vs.validators[idx].tier = StakeTier::from_stake(new_stake).unwrap_or(StakeTier::Spark);
            let new_tier = vs.validators[idx].tier;
            vs.total_stake = new_total_stake;
            vs.quorum = strict_supermajority_threshold(vs.total_stake);
            info!(%address, new_stake, new_tier = ?new_tier, "Validator stake updated");
            Ok(())
        } else {
            Err(ConsensusError::InvalidBlock(
                "address not in validator set".into(),
            ))
        }
    }

    pub fn epoch_transition(&self) -> u64 {
        let mut vs = self.validator_set.write();
        vs.epoch += 1;
        vs.total_stake = checked_validator_total_stake(&vs.validators)
            .expect("epoch transition cannot overflow or duplicate validator stake");
        vs.quorum = strict_supermajority_threshold(vs.total_stake);
        let new_epoch = vs.epoch;
        info!(
            epoch = new_epoch,
            validators = vs.len(),
            total_stake = vs.total_stake,
            "Epoch transition"
        );
        new_epoch
    }

    pub fn is_force_advanced(&self) -> bool {
        self.force_advanced.load(Ordering::SeqCst)
    }

    pub fn get_block(&self, hash: &Hash256) -> Option<DagBlock> {
        self.dag.get(hash).map(|r| r.value().clone())
    }

    pub fn blocks_in_round(&self, round: u64) -> Vec<Hash256> {
        self.rounds
            .get(&round)
            .map(|r| r.value().clone())
            .unwrap_or_default()
    }

    pub fn committed_blocks(&self) -> Vec<Hash256> {
        self.committed.read().clone()
    }

    pub fn dag_size(&self) -> usize {
        self.dag.len()
    }

    pub fn propose_block(
        &self,
        transactions: Vec<Hash256>,
        timestamp: u64,
    ) -> Result<DagBlock, ConsensusError> {
        let vs = self.validator_set.read();

        if !vs.can_produce_blocks(&self.local_address) {
            return Err(ConsensusError::NotValidator);
        }

        let round = self.current_round.load(Ordering::SeqCst);
        if self
            .author_round_blocks
            .contains_key(&(self.local_address, round))
        {
            return Err(ConsensusError::DuplicateBlock);
        }
        let floor = self.proposal_floor.load(Ordering::SeqCst);
        if floor > 0 && round <= floor {
            return Err(ConsensusError::DuplicateBlock);
        }

        let parents = if round == 0 || self.is_recovery_bootstrap_round(round) {
            Vec::new()
        } else {
            let prev_round = round - 1;
            let mut prev_hashes = self.blocks_in_round(prev_round);
            prev_hashes.sort_by_key(|hash| hash.0);

            let mut selected_parents = Vec::new();
            let mut accumulated_stake = 0u64;
            let mut seen_parent_authors = HashSet::new();

            for hash in &prev_hashes {
                if let Some(block) = self.dag.get(hash)
                    && let Some(validator) = vs.get_validator(&block.author)
                    && seen_parent_authors.insert(block.author)
                {
                    selected_parents.push(*hash);
                    accumulated_stake = accumulated_stake
                        .checked_add(validator.stake)
                        .expect("unique parent stake cannot exceed checked total stake");
                }
            }

            if !self.round_participation_satisfies_quorum(
                &vs,
                prev_round,
                &seen_parent_authors,
                accumulated_stake,
            ) {
                return Err(ConsensusError::InsufficientParents);
            }

            selected_parents
        };

        drop(vs);

        let mut transactions = transactions;
        transactions.sort_by_key(|a| a.0);

        let ordering_commitment = DagBlock::compute_ordering_commitment(&transactions);
        let mut block = DagBlock {
            author: self.local_address,
            round,
            parents,
            transactions,
            timestamp,
            hash: Hash256::ZERO,
            signature: Vec::new(),
            ordering_commitment,
        };
        block.hash = match self.consensus_domain.read().as_ref() {
            Some(domain) => block.compute_hash_in_domain(domain),
            None => block.compute_hash(),
        };

        if let Some(ref keypair) = self.local_keypair {
            match keypair.sign(&block.hash) {
                Ok(sig) => {
                    block.signature = bincode::serialize(&sig).unwrap_or_default();
                }
                Err(e) => {
                    return Err(ConsensusError::InvalidBlock(format!(
                        "failed to sign block: {e}"
                    )));
                }
            }
        }

        debug!(
            round = block.round,
            parents = block.parents.len(),
            txs = block.transactions.len(),
            hash = %block.hash,
            "Proposed DAG block"
        );

        self.insert_block_into_dag(&block);
        self.author_round_blocks
            .insert((block.author, block.round), block.hash);

        let block_data = bincode::serialize(&block.transactions).unwrap_or_default();
        let commitment = data_availability::create_da_commitment(&block_data, block.hash);
        self.store_da_commitment(commitment);

        {
            let mut wd = self.withholding_detector.lock();
            wd.report_expected(self.local_address, round);
            wd.report_received(self.local_address, round);
        }
        self.prune_security_history(round);

        Ok(block)
    }

    pub fn receive_block(&self, block: &DagBlock) -> Result<(), ConsensusError> {
        self.receive_block_inner(block, false)
    }

    fn is_authentic_committee_block(&self, block: &DagBlock) -> bool {
        if !self.validator_set.read().can_produce_blocks(&block.author) {
            return false;
        }
        let valid_hash = match self.consensus_domain.read().as_ref() {
            Some(domain) => block.verify_hash_in_domain(domain),
            None => block.verify_hash(),
        };
        if !valid_hash || block.signature.is_empty() {
            return false;
        }
        let Ok(sig) = bincode::deserialize::<CryptoSignature>(&block.signature) else {
            return false;
        };
        if sig.verify(&block.hash, &block.author).is_err() {
            return false;
        }
        if let Some(registered_key) = self.validator_keys.get(&block.author)
            && let CryptoSignature::Ed25519 { public_key, .. } = &sig
            && public_key != registered_key.value()
        {
            return false;
        }
        true
    }

    fn receive_block_inner(
        &self,
        block: &DagBlock,
        base_exception: bool,
    ) -> Result<(), ConsensusError> {
        self.receive_block_inner_with_persistence(block, base_exception, |_| Ok(()))
    }

    fn receive_block_inner_with_persistence(
        &self,
        block: &DagBlock,
        base_exception: bool,
        persist: impl FnOnce(&DagBlock) -> Result<(), ConsensusError>,
    ) -> Result<(), ConsensusError> {
        let vs = self.validator_set.read();

        if !vs.can_produce_blocks(&block.author) {
            return Err(ConsensusError::NotValidator);
        }

        if self.dag.contains_key(&block.hash) {
            return Err(ConsensusError::DuplicateBlock);
        }

        let valid_hash = match self.consensus_domain.read().as_ref() {
            Some(domain) => block.verify_hash_in_domain(domain),
            None => block.verify_hash(),
        };
        if !valid_hash {
            return Err(ConsensusError::InvalidBlock(
                "hash does not match block contents".into(),
            ));
        }

        if !block.verify_ordering() {
            return Err(ConsensusError::MevOrderingViolation(format!(
                "block {} by {} has transactions not in canonical lexicographic order \
                     or ordering commitment does not match sorted tx hashes",
                block.hash, block.author
            )));
        }

        if !block.signature.is_empty() {
            let sig: CryptoSignature = bincode::deserialize(&block.signature)
                .map_err(|_| ConsensusError::InvalidSignature)?;
            sig.verify(&block.hash, &block.author).map_err(|_| {
                warn!(
                    author = %block.author,
                    hash = %block.hash,
                    "Block signature verification failed"
                );
                ConsensusError::InvalidSignature
            })?;
            if let Some(registered_key) = self.validator_keys.get(&block.author)
                && let CryptoSignature::Ed25519 { public_key, .. } = &sig
                && public_key != registered_key.value()
            {
                warn!(
                    author = %block.author,
                    "Block signed with key that doesn't match registered key"
                );
                return Err(ConsensusError::InvalidSignature);
            }
        } else if self.local_keypair.is_some() {
            return Err(ConsensusError::InvalidSignature);
        }

        let current = self.current_round.load(Ordering::SeqCst);
        let round_gap = block.round.saturating_sub(current);
        if round_gap > 1 {
            return Err(ConsensusError::RoundTooFarAhead {
                round: block.round,
                current,
            });
        }

        if block.round == 0 || self.is_recovery_bootstrap_round(block.round) {
            if !block.parents.is_empty() {
                return Err(ConsensusError::InvalidBlock(
                    "bootstrap block must not have parents".into(),
                ));
            }
        } else if base_exception && self.is_restart_base_round(block.round) {
            if block.parents.is_empty()
                || block.parents.contains(&Hash256::ZERO)
                || block.parents.iter().copied().collect::<HashSet<_>>().len()
                    != block.parents.len()
            {
                return Err(ConsensusError::InvalidBlock(
                    "restart base round block has an empty, zero, or duplicate parent list".into(),
                ));
            }
        } else if self.local_recovery_replay_active.load(Ordering::SeqCst)
            && self.local_recovery_boundary_round.read().as_ref() == Some(&block.round)
        {
            if block.parents.is_empty()
                || block.parents.contains(&Hash256::ZERO)
                || block.parents.iter().copied().collect::<HashSet<_>>().len()
                    != block.parents.len()
            {
                return Err(ConsensusError::InvalidBlock(
                    "local recovery boundary has an empty, zero, or duplicate parent commitment"
                        .into(),
                ));
            }
        } else {
            let expected_parent_round = block.round - 1;
            let mut parent_stake = 0u64;
            let mut missing_parents = 0usize;
            let mut seen_parent_authors = HashSet::new();

            for parent_hash in &block.parents {
                match self.dag.get(parent_hash) {
                    Some(parent_block) => {
                        if parent_block.round != expected_parent_round {
                            missing_parents += 1;
                            continue;
                        }
                        if let Some(validator) = vs.get_validator(&parent_block.author)
                            && seen_parent_authors.insert(parent_block.author)
                        {
                            parent_stake = parent_stake
                                .checked_add(validator.stake)
                                .expect("unique parent stake cannot exceed checked total stake");
                        }
                    }
                    None => {
                        missing_parents += 1;
                    }
                }
            }

            if missing_parents > 0 {
                return Err(ConsensusError::MissingParents {
                    hash: block.hash,
                    missing: missing_parents,
                });
            }

            let parent_round = block.round.saturating_sub(1);
            if !self.round_participation_satisfies_quorum(
                &vs,
                parent_round,
                &seen_parent_authors,
                parent_stake,
            ) {
                return Err(ConsensusError::InsufficientParents);
            }
        }

        drop(vs);

        persist(block)?;

        let key = (block.author, block.round);
        if let Some(equivocation) = self.detect_equivocation(block) {
            let evidence =
                arc_crypto::hash_pair(&equivocation.block1_hash, &equivocation.block2_hash);
            warn!(
                author = %block.author,
                round = block.round,
                first_block = %equivocation.block1_hash,
                equivocating_block = %equivocation.block2_hash,
                "EQUIVOCATION DETECTED - validator produced two blocks in the same round"
            );

            if self.consensus_domain.read().is_some() {
                warn!(
                    offender = %block.author,
                    evidence = %evidence,
                    "Domain-bound equivocation retained as evidence; validator authority is unchanged"
                );
            } else {
                let mut vs = self.validator_set.write();
                if let Ok(record) = vs.report_offense(
                    block.author,
                    SlashableOffense::EquivocationDAG,
                    evidence,
                    block.round,
                    block.timestamp,
                ) {
                    warn!(
                        offender = %block.author,
                        slash_amount = record.slash_amount,
                        "Slash applied for DAG equivocation"
                    );
                    vs.apply_slash(&block.author, record.slash_amount);
                }
            }
        } else {
            use dashmap::mapref::entry::Entry;
            match self.author_round_blocks.entry(key) {
                Entry::Vacant(e) => {
                    e.insert(block.hash);
                }
                Entry::Occupied(e) => {
                    if *e.get() != block.hash {
                        warn!(
                            author = %block.author,
                            round = block.round,
                            existing = %e.get(),
                            new = %block.hash,
                            "Late equivocation detected via atomic check"
                        );
                    }
                }
            }
        }

        self.insert_block_into_dag(block);

        {
            let mut wd = self.withholding_detector.lock();
            wd.report_received(block.author, block.round);
        }
        let is_equivocation = self
            .author_round_blocks
            .get(&(block.author, block.round))
            .map(|existing| *existing.value() != block.hash)
            .unwrap_or(false);
        if !is_equivocation {
            let mut st = self.stake_tracker.lock();
            st.report_vote(block.author, block.round, *block.hash.as_bytes());
            let evidence = st.detect_double_voting(block.round);
            if !evidence.is_empty() {
                for dv in &evidence {
                    if self.testnet_mode {
                        debug!(
                            validator = %dv.validator,
                            round = dv.round,
                            "Double vote detected (testnet: slash disabled)"
                        );
                    } else {
                        tracing::warn!(
                            validator = %dv.validator,
                            round = dv.round,
                            "Double vote detected - slashing validator"
                        );
                    }
                }
            }
        }

        self.prune_security_history(block.round);

        debug!(
            round = block.round,
            author = %block.author,
            hash = %block.hash,
            "Received valid DAG block"
        );

        Ok(())
    }

    fn two_round_commit_support(
        &self,
        block_hash: &Hash256,
        round: u64,
        validator_set: &ValidatorSet,
    ) -> Option<u64> {
        for child_hash in self.blocks_in_round(round + 1) {
            let Some(child) = self.dag.get(&child_hash) else {
                continue;
            };
            if !child.parents.contains(block_hash) {
                continue;
            }

            let mut supporting_stake = 0u64;
            let mut seen_certifiers = HashSet::new();
            for certifier_hash in self.blocks_in_round(round + 2) {
                if let Some(certifier) = self.dag.get(&certifier_hash)
                    && certifier.parents.contains(&child_hash)
                    && let Some(validator) = validator_set.get_validator(&certifier.author)
                    && seen_certifiers.insert(certifier.author)
                {
                    supporting_stake = supporting_stake
                        .checked_add(validator.stake)
                        .expect("unique certifier stake cannot exceed checked total stake");
                }
            }
            if supporting_stake >= validator_set.quorum {
                return Some(supporting_stake);
            }
        }
        None
    }

    pub fn register_skip_certificate(
        &self,
        certificate: view_change::SkipCertificate,
    ) -> Result<u64, view_change::CertificateError> {
        let domain = self
            .certificate_domain()
            .ok_or(view_change::CertificateError::WrongValidatorSet)?;
        let signing = {
            let vs = self.frozen_validator_set.read();
            certificate.verify(&domain, &vs)?
        };
        let round = certificate.round;
        let absentee = certificate.absentee;
        self.skip_certificates
            .insert((round, absentee), certificate);
        info!(
            round,
            %absentee,
            signing_stake = signing,
            "Registered authenticated absence certificate"
        );
        Ok(signing)
    }

    fn is_excused_for_round(&self, round: u64, address: &Address) -> bool {
        self.skip_certificates.contains_key(&(round, *address))
    }

    pub fn install_certificate_domain(
        &self,
        domain: ConsensusDomain,
    ) -> Result<(), ConsensusError> {
        let mut active = self.certificate_domain.write();
        match active.as_ref() {
            Some(existing) if existing == &domain => Ok(()),
            Some(_) => Err(ConsensusError::InvalidBlock(
                "certificate domain is already bound to another chain/epoch".into(),
            )),
            None => {
                *active = Some(domain);
                Ok(())
            }
        }
    }

    pub fn certificate_domain(&self) -> Option<ConsensusDomain> {
        *self.certificate_domain.read()
    }

    pub fn local_keypair(&self) -> Option<&KeyPair> {
        self.local_keypair.as_ref()
    }

    pub fn has_skip_certificate(&self, round: u64, absentee: &Address) -> bool {
        self.skip_certificates.contains_key(&(round, *absentee))
    }

    pub fn skip_certificate(
        &self,
        round: u64,
        absentee: &Address,
    ) -> Option<view_change::SkipCertificate> {
        self.skip_certificates
            .get(&(round, *absentee))
            .map(|c| c.value().clone())
    }

    pub fn is_certified_absent(&self, round: u64, member: &Address) -> bool {
        self.has_skip_certificate(round, member)
    }

    pub fn import_history(
        &self,
        blocks: &[DagBlock],
        max_rounds: u64,
    ) -> Result<u64, ConsensusError> {
        self.import_history_with_persistence(blocks, max_rounds, |_| Ok(()))
    }

    pub fn import_history_with_persistence(
        &self,
        blocks: &[DagBlock],
        max_rounds: u64,
        mut persist: impl FnMut(&DagBlock) -> Result<(), ConsensusError>,
    ) -> Result<u64, ConsensusError> {
        if blocks.is_empty() {
            return Err(ConsensusError::InvalidBlock("empty history".into()));
        }
        let start = self.current_round.load(Ordering::SeqCst);
        let mut by_round: std::collections::BTreeMap<u64, Vec<&DagBlock>> =
            std::collections::BTreeMap::new();
        for block in blocks {
            by_round.entry(block.round).or_default().push(block);
        }
        let base = self.restart_base_round();
        if let Some(base) = base {
            by_round.retain(|round, _| *round >= base);
            if by_round.is_empty() {
                return Err(ConsensusError::InvalidBlock(
                    "history is entirely below this node's commit cursor".into(),
                ));
            }
            if let Some(base_blocks) = by_round.get(&base) {
                let vs = self.validator_set.read();
                let one_honest = vs.total_stake.saturating_sub(vs.quorum).saturating_add(1);
                let next: Vec<&DagBlock> = by_round
                    .get(&base.saturating_add(1))
                    .map(|blocks| {
                        blocks
                            .iter()
                            .copied()
                            .filter(|b| self.is_authentic_committee_block(b))
                            .collect()
                    })
                    .unwrap_or_default();
                let supported: Vec<&DagBlock> = base_blocks
                    .iter()
                    .copied()
                    .filter(|candidate| {
                        let mut authors = HashSet::new();
                        let stake: u64 = next
                            .iter()
                            .filter(|b| b.parents.contains(&candidate.hash))
                            .filter(|b| authors.insert(b.author))
                            .filter_map(|b| vs.get_validator(&b.author).map(|v| v.stake))
                            .sum();
                        stake >= one_honest
                    })
                    .collect();
                drop(vs);
                if supported.is_empty() {
                    return Err(ConsensusError::InvalidBlock(
                        "history names no base-round block its next round supports".into(),
                    ));
                }
                by_round.insert(base, supported);
            }
        }
        let first = *by_round.keys().next().expect("non-empty");
        let last = *by_round.keys().next_back().expect("non-empty");
        if first > start {
            return Err(ConsensusError::InvalidBlock(format!(
                "history starts at round {first} but this node is waiting at {start}"
            )));
        }
        if last.saturating_sub(start) > max_rounds {
            return Err(ConsensusError::InvalidBlock(format!(
                "history spans {} rounds, above the {max_rounds} bound",
                last.saturating_sub(first)
            )));
        }
        for round in first..=last {
            if !by_round.contains_key(&round) {
                return Err(ConsensusError::InvalidBlock(format!(
                    "history has a gap at round {round}"
                )));
            }
        }
        let quorum = {
            let vs = self.validator_set.read();
            vs.quorum
        };
        let mut first_thin: Option<u64> = None;
        for (round, round_blocks) in &by_round {
            let vs = self.validator_set.read();
            let mut authors = HashSet::new();
            let mut stake = 0u64;
            for block in round_blocks {
                if let Some(validator) = vs.get_validator(&block.author)
                    && authors.insert(block.author)
                {
                    stake = stake.saturating_add(validator.stake);
                }
            }
            let excused = vs.validators.iter().filter(|v| v.stake > 0).all(|v| {
                authors.contains(&v.address) || self.is_excused_for_round(*round, &v.address)
            });
            if stake < quorum && !excused {
                first_thin = Some(*round);
                if *round == first {
                    return Err(ConsensusError::InvalidBlock(format!(
                        "history round {round} carries {stake} stake, below quorum {quorum}"
                    )));
                }
                break;
            }
        }
        if let Some(thin) = first_thin {
            by_round.retain(|round, _| *round < thin);
        }
        let mut reached = start;
        let mut inserted = 0usize;
        'rounds: for (round, round_blocks) in by_round {
            for block in round_blocks {
                match self.receive_block_inner_with_persistence(
                    block,
                    base == Some(round),
                    &mut persist,
                ) {
                    Ok(()) => inserted += 1,
                    Err(ConsensusError::DuplicateBlock) => {}
                    Err(error @ ConsensusError::HistoryPersistence(_)) => return Err(error),
                    Err(error) => {
                        debug!(
                            round,
                            ?error,
                            reached,
                            "History import stopped here; keeping what was applied"
                        );
                        break 'rounds;
                    }
                }
            }
            if round >= reached {
                if self.advance_round() {
                    reached = self.current_round.load(Ordering::SeqCst);
                }
            }
        }
        if inserted == 0 && reached == start {
            return Err(ConsensusError::InvalidBlock(
                "history added nothing this node did not already have".into(),
            ));
        }
        info!(
            from = start,
            to = reached,
            blocks = blocks.len(),
            inserted,
            "Imported authenticated history"
        );
        Ok(reached)
    }

    pub fn leader_commit_support(&self, block_hash: &Hash256, round: u64) -> Option<u64> {
        let vs = self.frozen_validator_set.read();
        self.two_round_commit_support(block_hash, round, &vs)
    }

    pub fn register_finality_certificate(
        &self,
        certificate: view_change::FinalityCertificate,
    ) -> Result<u64, view_change::CertificateError> {
        let domain = self
            .certificate_domain()
            .ok_or(view_change::CertificateError::WrongValidatorSet)?;
        let signing = {
            let vs = self.frozen_validator_set.read();
            certificate.verify(&domain, &vs)?
        };
        if let Some(held) = self.finality_certificates.get(&certificate.height)
            && held.block_hash != certificate.block_hash
        {
            return Err(view_change::CertificateError::ConflictingFinality {
                height: certificate.height,
                held: held.block_hash,
                offered: certificate.block_hash,
            });
        }
        let height = certificate.height;
        self.finality_certificates.insert(height, certificate);
        let highest = self
            .highest_finalized_height
            .fetch_max(height, Ordering::AcqRel)
            .max(height);
        if height.is_multiple_of(FINALITY_PRUNE_EVERY) {
            let floor = highest.saturating_sub(self.retained_finality_heights());
            self.finality_certificates.retain(|h, _| *h >= floor);
        }
        Ok(signing)
    }

    pub fn highest_finalized_height(&self) -> Option<u64> {
        let h = self.highest_finalized_height.load(Ordering::Acquire);
        (h > 0 || self.finality_certificates.contains_key(&0)).then_some(h)
    }

    pub fn retained_finality_heights(&self) -> u64 {
        self.retained_finality_heights.load(Ordering::Acquire)
    }

    pub fn set_retained_finality_heights(&self, heights: u64) {
        self.retained_finality_heights
            .store(heights.max(FINALITY_PRUNE_EVERY), Ordering::Release);
    }

    pub fn finality_certificate_count(&self) -> usize {
        self.finality_certificates.len()
    }

    pub fn memory_gauges(&self) -> Vec<(&'static str, u64)> {
        let withholding = self.withholding_detector.lock().entry_count();
        let (stake_votes, penalties) = self.stake_tracker.lock().sizes();
        let checkpoints = self.checkpoint_registry.lock().len();
        [
            ("engine_dag_blocks", self.dag.len()),
            ("engine_rounds", self.rounds.len()),
            ("engine_committed_hashes", self.committed.read().len()),
            ("engine_author_round_blocks", self.author_round_blocks.len()),
            ("engine_da_commitments", self.da_commitments.len()),
            ("engine_pending_cross_shard", self.pending_cross_shard.len()),
            (
                "engine_completed_cross_shard",
                self.completed_cross_shard.len(),
            ),
            ("engine_finality_proofs", self.finality_proofs.len()),
            ("engine_skip_certificates", self.skip_certificates.len()),
            ("engine_excused_participation", self.skip_certificates.len()),
            (
                "engine_finality_certificates",
                self.finality_certificates.len(),
            ),
            ("engine_validator_keys", self.validator_keys.len()),
            ("engine_withholding_entries", withholding),
            ("engine_stake_votes", stake_votes),
            ("engine_penalties", penalties),
            ("engine_checkpoints", checkpoints),
        ]
        .into_iter()
        .map(|(name, len)| (name, len as u64))
        .collect()
    }

    pub fn finalized_block_at(&self, height: u64) -> Option<Hash256> {
        self.finality_certificates
            .get(&height)
            .map(|certificate| certificate.block_hash)
    }

    pub fn finality_certificate(&self, height: u64) -> Option<view_change::FinalityCertificate> {
        self.finality_certificates
            .get(&height)
            .map(|c| c.value().clone())
    }

    pub fn frozen_validator_set_hash(&self) -> Hash256 {
        view_change::validator_set_hash(&self.frozen_validator_set.read())
    }

    fn causal_history_contains(&self, from: &Hash256, target: &Hash256, floor_round: u64) -> bool {
        if from == target {
            return true;
        }
        let mut seen: HashSet<Hash256> = HashSet::new();
        let mut frontier = vec![*from];
        seen.insert(*from);
        while let Some(hash) = frontier.pop() {
            let Some(block) = self.dag.get(&hash) else {
                continue;
            };
            if block.round < floor_round {
                continue;
            }
            for parent in &block.parents {
                if parent == target {
                    return true;
                }
                if seen.insert(*parent) {
                    frontier.push(*parent);
                }
            }
        }
        false
    }

    pub fn try_commit(&self) -> Vec<DagBlock> {
        self.try_commit_limited(usize::MAX, true)
    }

    fn try_commit_limited(
        &self,
        maximum_blocks: usize,
        maintain_live_state: bool,
    ) -> Vec<DagBlock> {
        let vs = self.frozen_validator_set.read();
        if vs.epoch == 0 {
            return Vec::new();
        }
        let current = self.current_round.load(Ordering::SeqCst);
        let mut newly_committed = Vec::new();
        let domain_bound = self.consensus_domain.read().is_some();

        if current < 2 {
            return newly_committed;
        }

        let committed_set: std::collections::HashSet<Hash256> = {
            let committed = self.committed.read();
            committed.iter().copied().collect()
        };

        let frozen_vals = {
            let fvs = self.frozen_validator_set.read();
            let mut addrs: Vec<Address> = fvs.validators.iter().map(|v| v.address).collect();
            addrs.sort_by_key(|a| a.0);
            addrs
        };

        let scan_start = self.last_committed_round.load(Ordering::SeqCst);
        let scan_end = current.saturating_sub(2);

        let anchors_of = |round: u64| -> Vec<Hash256> {
            let Some(leader) = frozen_vals.get(round as usize % frozen_vals.len().max(1)) else {
                return Vec::new();
            };
            let mut found: Vec<Hash256> = self
                .blocks_in_round(round)
                .into_iter()
                .filter(|hash| {
                    self.dag
                        .get(hash)
                        .map(|block| block.author == *leader)
                        .unwrap_or(false)
                })
                .collect();
            found.sort_by_key(|hash| hash.0);
            found.dedup();
            found
        };

        let mut r = scan_start;
        while r <= scan_end && newly_committed.len() < maximum_blocks {
            if frozen_vals.is_empty() {
                break;
            }
            let anchor_candidates = anchors_of(r);
            if domain_bound && anchor_candidates.len() > 1 {
                tracing::error!(
                    round = r,
                    hashes = ?anchor_candidates,
                    "Ambiguous recovery-domain leader round; commit cursor is fenced"
                );
                break;
            }
            let certified_all: Vec<(Hash256, u64)> = anchor_candidates
                .iter()
                .filter_map(|hash| {
                    self.two_round_commit_support(hash, r, &vs)
                        .map(|stake| (*hash, stake))
                })
                .collect();
            if certified_all.len() > 1 {
                tracing::error!(
                    round = r,
                    hashes = ?anchor_candidates,
                    "Two certified blocks from one leader round; commit cursor is fenced"
                );
                break;
            }
            let certified = certified_all.first().copied();

            if let Some((hash, supporting_stake)) = certified {
                if !committed_set.contains(&hash)
                    && let Some(block) = self.dag.get(&hash)
                {
                    debug!(
                        round = r,
                        hash = %block.hash,
                        "Block committed via two-round rule"
                    );
                    debug!(
                        hash = %block.hash,
                        signing_stake = supporting_stake,
                        total_stake = vs.total_stake,
                        "Commit support observed; proof export remains disabled because D-block signatures do not sign a canonical B-finality transcript"
                    );
                    newly_committed.push(block.clone());
                }
                self.last_committed_round.store(r + 1, Ordering::SeqCst);
                r += 1;
                continue;
            }

            let mut decider = None;
            let mut ambiguous_ahead = false;
            for r2 in (r + 3)..=scan_end {
                let candidates = anchors_of(r2);
                if domain_bound && candidates.len() > 1 {
                    ambiguous_ahead = true;
                    break;
                }
                let certified_ahead: Vec<Hash256> = candidates
                    .iter()
                    .filter(|hash| self.two_round_commit_support(hash, r2, &vs).is_some())
                    .copied()
                    .collect();
                if certified_ahead.len() > 1 {
                    ambiguous_ahead = true;
                    break;
                }
                if let Some(hash) = certified_ahead.first() {
                    decider = Some((r2, *hash));
                    break;
                }
            }
            if ambiguous_ahead {
                break;
            }
            let Some((decider_round, decider_hash)) = decider else {
                break;
            };

            let in_history: Vec<Hash256> = anchor_candidates
                .iter()
                .filter(|hash| self.causal_history_contains(&decider_hash, hash, r))
                .copied()
                .collect();
            if in_history.len() > 1 {
                tracing::error!(
                    round = r,
                    decided_by = decider_round,
                    "A later anchor's history holds two blocks from one leader round; \
                     commit cursor is fenced"
                );
                break;
            }
            match in_history.first().copied() {
                Some(hash) => {
                    if !committed_set.contains(&hash)
                        && let Some(block) = self.dag.get(&hash)
                    {
                        info!(
                            round = r,
                            hash = %block.hash,
                            decided_by = decider_round,
                            "Block committed via a later anchor's causal history"
                        );
                        newly_committed.push(block.clone());
                    }
                }
                _ => {
                    debug!(
                        round = r,
                        decided_by = decider_round,
                        "Round skipped: its anchor is absent from a later committed anchor's history"
                    );
                }
            }
            self.last_committed_round.store(r + 1, Ordering::SeqCst);
            r += 1;
        }

        if !newly_committed.is_empty() {
            let mut committed = self.committed.write();
            for block in &newly_committed {
                if !committed.contains(&block.hash) {
                    committed.push(block.hash);
                }
            }
        }

        if !maintain_live_state {
            return newly_committed;
        }

        drop(vs);

        if !newly_committed.is_empty() {
            let max_round = newly_committed.iter().map(|b| b.round).max().unwrap_or(0);
            if max_round % security::CHECKPOINT_INTERVAL == 0 && max_round > 0 {
                warn!(
                    round = max_round,
                    "Checkpoint interval reached without canonical state root and validator signature quorum; no trust anchor registered"
                );
            }

            if max_round % 100 == 0 {
                let wd = self.withholding_detector.lock();
                let reports = wd.detect_withholding(100);
                for report in &reports {
                    warn!(
                        validator = %report.validator,
                        score = report.withholding_score,
                        "Withholding detected - scheduling slash"
                    );
                    let evidence_hash = arc_crypto::hash_bytes(&report.validator.0);
                    let mut vs_w = self.validator_set.write();
                    if let Ok(record) = vs_w.report_offense(
                        report.validator,
                        SlashableOffense::LivenessFault,
                        evidence_hash,
                        max_round,
                        0,
                    ) {
                        vs_w.apply_slash(&report.validator, record.slash_amount);
                    }
                }
            }
        }

        if !newly_committed.is_empty() {
            self.prune_old_rounds();
            if let Some(max_committed) = newly_committed.iter().map(|b| b.round).max() {
                self.prune_below_round(max_committed);
            }
        }

        self.expire_stale_locks();
        self.expire_stale_locks_with_timeout(30);

        newly_committed
    }

    fn prune_security_history(&self, round: u64) {
        if !round.is_multiple_of(PRUNE_DEPTH) {
            return;
        }
        let Some(before_round) = round.checked_sub(SECURITY_HISTORY_DEPTH) else {
            return;
        };
        self.withholding_detector.lock().prune(before_round);
        self.stake_tracker.lock().prune_votes(before_round);
    }

    pub fn first_missing_round(&self, from: u64, limit: u64) -> u64 {
        let end = from.saturating_add(limit);
        let mut round = from;
        while round < end {
            if self.blocks_in_round(round).is_empty() {
                return round;
            }
            round = round.saturating_add(1);
        }
        end
    }

    pub fn absent_parents(&self, block: &DagBlock) -> Option<Vec<Hash256>> {
        let expected = block.round.checked_sub(1)?;
        let mut absent = Vec::new();
        for parent in &block.parents {
            match self.dag.get(parent) {
                Some(held) if held.round != expected => return None,
                Some(_) => {}
                None => absent.push(*parent),
            }
        }
        Some(absent)
    }

    pub fn dag_block_count(&self) -> usize {
        self.dag.len()
    }

    pub fn dag_is_empty(&self) -> bool {
        self.dag.is_empty()
    }

    pub fn retained_rounds(&self) -> u64 {
        self.retained_rounds.load(Ordering::Relaxed)
    }

    pub fn set_retained_rounds(&self, rounds: u64) {
        let clamped = rounds.max(PRUNE_DEPTH);
        if clamped != rounds {
            warn!(
                requested = rounds,
                floor = PRUNE_DEPTH,
                "DAG retention raised to the reorg-safety floor"
            );
        }
        self.retained_rounds.store(clamped, Ordering::Relaxed);
    }

    fn prune_old_rounds(&self) {
        let current = self.current_round.load(Ordering::SeqCst);
        let retained = self.retained_rounds();
        if current <= retained {
            return;
        }
        let prune_below =
            (current - retained).min(self.last_committed_round.load(Ordering::SeqCst));

        let mut pruned_blocks = 0usize;
        let mut pruned_rounds = 0usize;

        let rounds_to_prune: Vec<u64> = self
            .rounds
            .iter()
            .filter(|entry| *entry.key() < prune_below)
            .map(|entry| *entry.key())
            .collect();

        for round in &rounds_to_prune {
            if let Some((_, block_hashes)) = self.rounds.remove(round) {
                for hash in &block_hashes {
                    self.dag.remove(hash);
                    pruned_blocks += 1;
                }
                pruned_rounds += 1;
            }

            let keys_to_remove: Vec<(Address, u64)> = self
                .author_round_blocks
                .iter()
                .filter(|entry| entry.key().1 == *round)
                .map(|entry| *entry.key())
                .collect();
            for key in keys_to_remove {
                self.author_round_blocks.remove(&key);
            }
        }

        if pruned_blocks > 0 {
            let mut committed = self.committed.write();
            committed.retain(|hash| self.dag.contains_key(hash));
        }

        if pruned_blocks > 0 {
            let stale_proofs: Vec<Hash256> = self
                .finality_proofs
                .iter()
                .filter(|e| !self.dag.contains_key(e.key()))
                .map(|e| *e.key())
                .collect();
            for h in &stale_proofs {
                self.finality_proofs.remove(h);
            }
            let stale_da: Vec<Hash256> = self
                .da_commitments
                .iter()
                .filter(|e| !self.dag.contains_key(e.key()))
                .map(|e| *e.key())
                .collect();
            for h in &stale_da {
                self.da_commitments.remove(h);
            }
            let stale_cs: Vec<Hash256> = self
                .completed_cross_shard
                .iter()
                .filter(|e| !self.dag.contains_key(e.key()))
                .map(|e| *e.key())
                .collect();
            for h in &stale_cs {
                self.completed_cross_shard.remove(h);
            }
        }

        if pruned_rounds > 0 {
            debug!(
                pruned_rounds,
                pruned_blocks,
                dag_size = self.dag.len(),
                finality_proofs = self.finality_proofs.len(),
                "DAG pruned old rounds"
            );
        }
    }

    pub fn advance_round(&self) -> bool {
        let current = self.current_round.load(Ordering::SeqCst);
        let vs = self.validator_set.read();

        let round_blocks = self.blocks_in_round(current);
        let mut round_stake = 0u64;
        let mut seen_authors = std::collections::HashSet::new();

        for hash in &round_blocks {
            if let Some(block) = self.dag.get(hash) {
                if seen_authors.insert(block.author)
                    && let Some(validator) = vs.get_validator(&block.author)
                {
                    round_stake = round_stake
                        .checked_add(validator.stake)
                        .expect("unique round stake cannot exceed checked total stake");
                }
            }
        }

        let certified_participation =
            self.round_participation_satisfies_quorum(&vs, current, &seen_authors, round_stake);
        if certified_participation {
            let Some(new_round) = current.checked_add(1) else {
                warn!(round = current, "Cannot advance beyond u64::MAX round");
                return false;
            };
            self.current_round.store(new_round, Ordering::SeqCst);
            self.force_advanced.store(false, Ordering::SeqCst);
            self.reset_round_timer();
            debug!(
                old_round = current,
                new_round = new_round,
                blocks = round_blocks.len(),
                stake = round_stake,
                "Advanced to new round"
            );
            true
        } else {
            debug!(
                round = current,
                stake = round_stake,
                quorum = vs.quorum,
                certified_participation,
                "Cannot advance round: insufficient certified participation"
            );
            false
        }
    }

    pub fn shard_of(address: &Address, num_shards: u16) -> u16 {
        if num_shards == 0 {
            return 0;
        }
        let bytes = address.as_bytes();
        let val = u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        (val % num_shards as u64) as u16
    }

    pub fn needs_full_consensus(tx: &Transaction, num_shards: u16) -> bool {
        if num_shards <= 1 {
            return false;
        }

        let sender_shard = Self::shard_of(&tx.from, num_shards);

        match &tx.body {
            TxBody::Transfer(body) => {
                let recipient_shard = Self::shard_of(&body.to, num_shards);
                sender_shard != recipient_shard
            }
            TxBody::Settle(body) => {
                let agent_shard = Self::shard_of(&body.agent_id, num_shards);
                sender_shard != agent_shard
            }
            TxBody::Swap(body) => {
                let counterparty_shard = Self::shard_of(&body.counterparty, num_shards);
                sender_shard != counterparty_shard
            }
            TxBody::Escrow(body) => {
                let beneficiary_shard = Self::shard_of(&body.beneficiary, num_shards);
                sender_shard != beneficiary_shard
            }
            TxBody::Stake(body) => {
                let validator_shard = Self::shard_of(&body.validator, num_shards);
                sender_shard != validator_shard
            }
            TxBody::WasmCall(body) => {
                let contract_shard = Self::shard_of(&body.contract, num_shards);
                sender_shard != contract_shard
            }
            TxBody::MultiSig(body) => body
                .signers
                .iter()
                .any(|signer| Self::shard_of(signer, num_shards) != sender_shard),
            TxBody::DeployContract(_) => false,
            TxBody::RegisterAgent(_) => false,
            TxBody::JoinValidator(_)
            | TxBody::LeaveValidator
            | TxBody::ClaimRewards
            | TxBody::UpdateStake(_) => false,
            TxBody::Governance(_) => false,
            TxBody::BridgeLock(_) | TxBody::BridgeMint(_) => false,
            TxBody::BatchSettle(_) => false,
            TxBody::ChannelOpen(body) => {
                let cp_shard = Self::shard_of(&body.counterparty, num_shards);
                sender_shard != cp_shard
            }
            TxBody::ChannelClose(_) => false,
            TxBody::ChannelDispute(_) => false,
            TxBody::ShardProof(_) => false,
            TxBody::InferenceAttestation(_) => false,
            TxBody::CommunityInferenceReward(_) => true,
            TxBody::InferenceChallenge(_) => false,
            TxBody::InferenceRegister(_) => false,
            TxBody::InferenceEscrowOpen(_) => false,
            TxBody::InferenceEscrowRelease(_) => false,
            TxBody::InferenceEscrowRefund(_) => false,
            TxBody::ModelRegistration(_) => false,
            TxBody::ModelRequest(_) => false,
            TxBody::ShardCoverageClaim(_) => false,
            TxBody::CapacityAdvertisement(_) => false,
            TxBody::ShardAssignmentProposal(_) => false,
            TxBody::FaucetClaim(_) => false,
            TxBody::InferenceRequest(_) => false,
            TxBody::InferenceVote(_) => false,
            TxBody::InferenceFinalize(_) => false,
            TxBody::NativeInferenceRequest(_)
            | TxBody::NativeInferenceFinalize(_)
            | TxBody::NativeInferenceRefund(_) => true,
        }
    }

    pub fn update_validator_set(&self, new_set: ValidatorSet) -> Result<(), ConsensusError> {
        let canonical_total = checked_validator_total_stake(&new_set.validators)?;
        let canonical_quorum = strict_supermajority_threshold(canonical_total);
        if new_set.total_stake != canonical_total || new_set.quorum != canonical_quorum {
            return Err(ConsensusError::InvalidBlock(
                "validator set has non-canonical total stake or quorum".into(),
            ));
        }
        info!(
            epoch = new_set.epoch,
            validators = new_set.len(),
            total_stake = new_set.total_stake,
            "Validator set updated"
        );
        *self.validator_set.write() = new_set;
        Ok(())
    }

    const PROPOSER_TIMEOUT_MS: u128 = 100;

    const VIEW_CHANGE_TIMEOUT_MS: u128 = 5_000;

    pub fn is_proposer_timed_out(&self) -> bool {
        self.round_start.read().elapsed().as_millis() > Self::PROPOSER_TIMEOUT_MS
    }

    pub fn needs_view_change(&self) -> bool {
        self.round_start.read().elapsed().as_millis() > Self::VIEW_CHANGE_TIMEOUT_MS
    }

    pub fn reset_round_timer(&self) {
        *self.round_start.write() = std::time::Instant::now();
    }

    pub fn force_advance_round(&self) {
        let Ok(old) =
            self.current_round
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    current.checked_add(1)
                })
        else {
            warn!(
                round = u64::MAX,
                "Cannot force a view beyond u64::MAX round"
            );
            self.reset_round_timer();
            return;
        };
        self.force_advanced.store(true, Ordering::SeqCst);
        self.reset_round_timer();
        warn!(
            old_round = old,
            new_round = old + 1,
            "View-change: advanced local round; parent quorum remains required"
        );
    }

    pub fn online_stake_fraction(&self, lookback_rounds: u64) -> f64 {
        let current = self.current_round.load(Ordering::SeqCst);
        let vs = self.validator_set.read();
        let mut online_stake = 0u64;

        for validator in &vs.validators {
            let start = current.saturating_sub(lookback_rounds);
            let mut seen = false;
            for r in start..=current {
                if let Some(blocks) = self.rounds.get(&r) {
                    for hash in blocks.value() {
                        if let Some(block) = self.dag.get(hash)
                            && block.author == validator.address
                        {
                            seen = true;
                            break;
                        }
                    }
                }
                if seen {
                    break;
                }
            }
            if seen {
                online_stake = online_stake
                    .checked_add(validator.stake)
                    .expect("online stake cannot exceed checked total stake");
            }
        }

        if vs.total_stake == 0 {
            return 0.0;
        }
        online_stake as f64 / vs.total_stake as f64
    }

    pub fn reset_dag(&self) {
        let old_round = self.current_round.swap(0, Ordering::SeqCst);
        let dag_size = self.dag.len();
        self.dag.clear();
        self.rounds.clear();
        self.committed.write().clear();
        self.author_round_blocks.clear();
        self.finality_proofs.clear();
        self.force_advanced.store(false, Ordering::SeqCst);
        *self.round_start.write() = std::time::Instant::now();
        info!(
            old_round = old_round,
            cleared_blocks = dag_size,
            "DAG reset for validator set transition"
        );
    }

    fn insert_block_into_dag(&self, block: &DagBlock) {
        self.dag.insert(block.hash, block.clone());
        self.rounds.entry(block.round).or_default().push(block.hash);
    }

    pub fn store_da_commitment(&self, commitment: data_availability::DACommitment) {
        self.da_commitments
            .insert(commitment.block_hash, commitment);
    }

    pub fn get_da_commitment(
        &self,
        block_hash: &Hash256,
    ) -> Option<data_availability::DACommitment> {
        self.da_commitments
            .get(block_hash)
            .map(|r| r.value().clone())
    }

    pub fn lock_cross_shard(
        &self,
        tx_hash: Hash256,
        source_shard: u16,
        target_shard: u16,
        source_block_hash: Hash256,
        source_round: u64,
    ) -> Result<CrossShardProof, ConsensusError> {
        if self.pending_cross_shard.contains_key(&tx_hash) {
            return Err(ConsensusError::CrossShardLockAlreadyExists(format!(
                "tx {} already locked",
                tx_hash
            )));
        }
        let lock_hash = arc_crypto::hash_bytes(
            &bincode::serialize(&(&tx_hash, source_shard, target_shard)).map_err(|e| {
                ConsensusError::CrossShardLockFailed(format!("serialize lock: {e}"))
            })?,
        );
        let inclusion_proof =
            bincode::serialize(&(&tx_hash, &source_block_hash)).unwrap_or_default();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let proof = CrossShardProof {
            tx_hash,
            source_shard,
            target_shard,
            source_block_hash,
            source_round,
            status: CrossShardStatus::Locked,
            lock_hash,
            inclusion_proof,
            locked_at_ms: now_ms,
            locked_at_round: self.current_round.load(Ordering::SeqCst),
        };
        self.pending_cross_shard.insert(tx_hash, proof.clone());
        Ok(proof)
    }

    pub fn commit_cross_shard(&self, tx_hash: Hash256) -> Result<CrossShardProof, ConsensusError> {
        let (_, mut proof) = self.pending_cross_shard.remove(&tx_hash).ok_or_else(|| {
            ConsensusError::CrossShardLockNotFound(format!("tx {} not found in pending", tx_hash))
        })?;
        proof.status = CrossShardStatus::Committed;
        self.completed_cross_shard.insert(tx_hash, proof.clone());
        Ok(proof)
    }

    pub fn abort_cross_shard(&self, tx_hash: Hash256) -> Result<CrossShardProof, ConsensusError> {
        let (_, mut proof) = self.pending_cross_shard.remove(&tx_hash).ok_or_else(|| {
            ConsensusError::CrossShardLockNotFound(format!("tx {} not found in pending", tx_hash))
        })?;
        proof.status = CrossShardStatus::Aborted;
        self.completed_cross_shard.insert(tx_hash, proof.clone());
        Ok(proof)
    }

    pub fn atomic_cross_shard_batch(
        &self,
        tx_hashes: Vec<Hash256>,
        shards: Vec<(u16, u16)>,
    ) -> Result<Vec<CrossShardProof>, ConsensusError> {
        let mut proofs = Vec::new();
        let batch_block = arc_crypto::hash_bytes(b"batch_source");
        for (i, tx_hash) in tx_hashes.iter().enumerate() {
            let (src, tgt) = shards[i];
            match self.lock_cross_shard(*tx_hash, src, tgt, batch_block, 0) {
                Ok(proof) => proofs.push(proof),
                Err(e) => {
                    for prev_proof in &proofs {
                        let _ = self.abort_cross_shard(prev_proof.tx_hash);
                    }
                    return Err(ConsensusError::CrossShardLockFailed(format!(
                        "batch failed at tx {}: {}",
                        tx_hash, e
                    )));
                }
            }
        }
        Ok(proofs)
    }

    pub fn cross_shard_stats(&self) -> (usize, usize) {
        (
            self.pending_cross_shard.len(),
            self.completed_cross_shard.len(),
        )
    }

    pub fn expire_stale_locks(&self) -> usize {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let current_round = self.current_round.load(Ordering::SeqCst);
        const MAX_LOCK_ROUNDS: u64 = 100;

        let expired: Vec<Hash256> = self
            .pending_cross_shard
            .iter()
            .filter(|entry| {
                let proof = entry.value();
                let time_expired =
                    now_ms.saturating_sub(proof.locked_at_ms) > CROSS_SHARD_LOCK_TIMEOUT_MS;
                let round_expired =
                    current_round.saturating_sub(proof.locked_at_round) > MAX_LOCK_ROUNDS;
                time_expired || round_expired
            })
            .map(|entry| *entry.key())
            .collect();

        let count = expired.len();
        for tx_hash in expired {
            if let Some((_, mut proof)) = self.pending_cross_shard.remove(&tx_hash) {
                proof.status = CrossShardStatus::Aborted;
                warn!(
                    tx = %tx_hash,
                    age_ms = now_ms.saturating_sub(proof.locked_at_ms),
                    "Cross-shard lock expired - aborting to prevent deadlock"
                );
                self.completed_cross_shard.insert(tx_hash, proof);
            }
        }

        if count > 0 {
            info!(expired = count, "Expired stale cross-shard locks");
        }
        count
    }

    pub fn generate_finality_proof(
        &self,
        block_hash: &Hash256,
        height: u64,
    ) -> Option<FinalityProof> {
        let certificate = self.finality_certificates.get(&height)?;
        let certificate = certificate.value();
        if &certificate.block_hash != block_hash {
            return None;
        }
        let vs = self.frozen_validator_set.read();
        let mut quorum_signatures = Vec::with_capacity(certificate.votes.len());
        let mut signing_stake = 0u64;
        for vote in &certificate.votes {
            let validator = vs.get_validator(&vote.voter)?;
            if vote
                .signature
                .verify(&vote.transcript(), &vote.voter)
                .is_err()
            {
                return None;
            }
            signing_stake = signing_stake.checked_add(validator.stake)?;
            quorum_signatures.push((vote.voter, bincode::serialize(&vote.signature).ok()?));
        }
        if signing_stake < vs.quorum {
            return None;
        }
        Some(FinalityProof {
            block_hash: *block_hash,
            round: height,
            height,
            quorum_signatures,
            signing_stake,
            total_stake: vs.total_stake,
        })
    }

    pub fn get_finality_proof(&self, block_hash: &Hash256) -> Option<FinalityProof> {
        self.finality_proofs
            .get(block_hash)
            .map(|r| r.value().clone())
    }

    pub fn prune_below_round(&self, committed_round: u64) -> usize {
        let retained = self.retained_rounds();
        if committed_round < retained {
            return 0;
        }
        let cutoff = committed_round - retained;

        let mut pruned_count = 0usize;

        let rounds_to_prune: Vec<u64> = self
            .rounds
            .iter()
            .filter(|entry| *entry.key() < cutoff)
            .map(|entry| *entry.key())
            .collect();

        for round in &rounds_to_prune {
            if let Some((_, block_hashes)) = self.rounds.remove(round) {
                for hash in &block_hashes {
                    self.dag.remove(hash);
                    pruned_count += 1;
                }
            }

            let keys_to_remove: Vec<(Address, u64)> = self
                .author_round_blocks
                .iter()
                .filter(|entry| entry.key().1 == *round)
                .map(|entry| *entry.key())
                .collect();
            for key in keys_to_remove {
                self.author_round_blocks.remove(&key);
            }
        }

        self.skip_certificates
            .retain(|(round, _), _| *round >= cutoff);

        if pruned_count > 0 {
            let mut committed = self.committed.write();
            committed.retain(|hash| self.dag.contains_key(hash));
            info!(
                pruned_count,
                cutoff_round = cutoff,
                dag_size = self.dag.len(),
                "DAG pruned below round"
            );
        }

        pruned_count
    }

    pub fn detect_equivocation(&self, block: &DagBlock) -> Option<EquivocationProof> {
        let key = (block.author, block.round);
        if let Some(existing_hash) = self.author_round_blocks.get(&key)
            && *existing_hash.value() != block.hash
        {
            return Some(EquivocationProof {
                author: block.author,
                round: block.round,
                block1_hash: *existing_hash.value(),
                block2_hash: block.hash,
            });
        }
        None
    }

    pub fn enforce_slash(&self, offender: &Address, amount: u64) -> bool {
        let mut vs = self.validator_set.write();
        let was_present = vs.get_validator(offender).is_some();
        if !was_present {
            warn!(
                address = %offender,
                "enforce_slash: validator not found in set"
            );
            return false;
        }

        vs.apply_slash(offender, amount);

        let still_present = vs.get_validator(offender).is_some();
        let removed = was_present && !still_present;

        if removed {
            warn!(
                address = %offender,
                slash_amount = amount,
                "Validator removed from set after slash (stake below minimum)"
            );
        } else {
            info!(
                address = %offender,
                slash_amount = amount,
                "Slash enforced on validator"
            );
        }

        removed
    }

    pub fn expire_stale_locks_with_timeout(&self, timeout_secs: u64) -> usize {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let current_round = self.current_round.load(Ordering::SeqCst);
        const MAX_LOCK_ROUNDS: u64 = 100;

        let timeout_ms = timeout_secs * 1000;

        let expired: Vec<Hash256> = self
            .pending_cross_shard
            .iter()
            .filter(|entry| {
                let proof = entry.value();
                let time_expired = now_ms.saturating_sub(proof.locked_at_ms) > timeout_ms;
                let round_expired =
                    current_round.saturating_sub(proof.locked_at_round) > MAX_LOCK_ROUNDS;
                time_expired || round_expired
            })
            .map(|entry| *entry.key())
            .collect();

        let count = expired.len();
        for tx_hash in expired {
            if let Some((_, mut proof)) = self.pending_cross_shard.remove(&tx_hash) {
                proof.status = CrossShardStatus::Aborted;
                warn!(
                    tx = %tx_hash,
                    age_ms = now_ms.saturating_sub(proof.locked_at_ms),
                    timeout_secs = timeout_secs,
                    "Cross-shard lock expired (parameterized) - aborting to prevent deadlock"
                );
                self.completed_cross_shard.insert(tx_hash, proof);
            }
        }

        if count > 0 {
            info!(
                expired = count,
                timeout_secs, "Expired stale cross-shard locks (parameterized)"
            );
        }
        count
    }

    pub fn set_node_role(&mut self, role: NodeRole) {
        self.node_role = role;
    }

    pub fn node_role(&self) -> NodeRole {
        self.node_role
    }

    pub fn submit_fraud_proof(
        &self,
        block_hash: Hash256,
        proposer: Address,
        claimed_root: Hash256,
        actual_root: Hash256,
        round: u64,
    ) -> Result<SlashRecord, ConsensusError> {
        warn!(
            proposer = %proposer,
            block = %block_hash,
            claimed = %claimed_root,
            actual = %actual_root,
            "FRAUD PROOF: proposer's state diff produced wrong root"
        );

        let evidence = arc_crypto::hash_pair(&claimed_root, &actual_root);
        let mut vs = self.validator_set.write();
        let record = vs.report_offense(
            proposer,
            SlashableOffense::InvalidBlockProposal,
            evidence,
            round,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        )?;
        vs.apply_slash(&proposer, record.slash_amount);
        Ok(record)
    }

    pub fn withholding_detector(&self) -> &parking_lot::Mutex<WithholdingDetector> {
        &self.withholding_detector
    }

    pub fn checkpoint_registry(&self) -> &parking_lot::Mutex<CheckpointRegistry> {
        &self.checkpoint_registry
    }

    pub fn stake_tracker(&self) -> &parking_lot::Mutex<StakeTracker> {
        &self.stake_tracker
    }
}

pub fn assign_shard(address: &Hash256, num_shards: u16) -> u16 {
    let bytes = address.as_bytes();
    let val = u16::from_le_bytes([bytes[0], bytes[1]]);
    val % num_shards
}

pub fn is_cross_shard(from: &Hash256, to: &Hash256, num_shards: u16) -> bool {
    if num_shards <= 1 {
        return false;
    }
    assign_shard(from, num_shards) != assign_shard(to, num_shards)
}

#[cfg(test)]
mod formal_proofs;

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn test_addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    fn test_validator_set(n: usize) -> ValidatorSet {
        let validators: Vec<Validator> = (0..n)
            .map(|i| {
                Validator::new(test_addr(i as u8), STAKE_ARC, i as u16).expect("valid validator")
            })
            .collect();
        ValidatorSet::new(validators, 1)
    }

    fn mixed_validator_set() -> ValidatorSet {
        let validators = vec![
            Validator::new(test_addr(0), STAKE_CORE, 0).unwrap(),
            Validator::new(test_addr(1), STAKE_ARC, 1).unwrap(),
            Validator::new(test_addr(2), STAKE_ARC, 0).unwrap(),
            Validator::new(test_addr(3), STAKE_SPARK, 1).unwrap(),
        ];
        ValidatorSet::new(validators, 1)
    }

    fn make_block(
        author: Address,
        round: u64,
        parents: Vec<Hash256>,
        transactions: Vec<Hash256>,
        timestamp: u64,
    ) -> DagBlock {
        let mut transactions = transactions;
        transactions.sort_by_key(|tx| tx.0);
        let ordering_commitment = DagBlock::compute_ordering_commitment(&transactions);
        let mut block = DagBlock {
            author,
            round,
            parents,
            transactions,
            timestamp,
            hash: Hash256::ZERO,
            signature: Vec::new(),
            ordering_commitment,
        };
        block.hash = block.compute_hash();
        block
    }

    fn make_block_in_domain(
        author: Address,
        round: u64,
        parents: Vec<Hash256>,
        transactions: Vec<Hash256>,
        timestamp: u64,
        domain: &ConsensusDomain,
    ) -> DagBlock {
        let mut block = make_block(author, round, parents, transactions, timestamp);
        block.hash = block.compute_hash_in_domain(domain);
        block
    }

    #[test]
    fn test_validator_set_creation_and_quorum() {
        let vs = test_validator_set(4);
        assert_eq!(vs.total_stake, 4 * STAKE_ARC);
        assert_eq!(vs.len(), 4);
        assert_eq!(vs.epoch, 1);
        assert_eq!(vs.quorum, strict_supermajority_threshold(vs.total_stake));
        assert!(vs.quorum > vs.total_stake * 2 / 3);
        assert!(vs.has_quorum(&[test_addr(0), test_addr(1), test_addr(2)]));
        assert!(!vs.has_quorum(&[test_addr(0), test_addr(1)]));
    }

    #[test]
    fn test_strict_supermajority_threshold_edges_and_overflow() {
        assert_eq!(strict_supermajority_threshold(0), 1);
        assert_eq!(strict_supermajority_threshold(1), 1);
        assert_eq!(strict_supermajority_threshold(2), 2);
        assert_eq!(strict_supermajority_threshold(3), 3);
        assert_eq!(strict_supermajority_threshold(6), 5);
        assert_eq!(
            strict_supermajority_threshold(u64::MAX),
            12_297_829_382_473_034_411
        );

        let empty = ValidatorSet::new(Vec::new(), 1);
        assert_eq!(empty.quorum, 1, "an empty set must fail closed");
        assert!(!empty.has_quorum(&[]));
        assert_eq!(empty.fault_tolerance_stake(), 0);

        let max_stake =
            ValidatorSet::new(vec![Validator::new(test_addr(0), u64::MAX, 0).unwrap()], 1);
        assert_eq!(max_stake.total_stake, u64::MAX);
        assert_eq!(max_stake.quorum, strict_supermajority_threshold(u64::MAX));
    }

    #[test]
    #[should_panic(expected = "duplicate validator address in validator set")]
    fn test_validator_set_rejects_duplicate_identities() {
        let address = test_addr(0);
        ValidatorSet::new(
            vec![
                Validator::new(address, STAKE_ARC, 0).unwrap(),
                Validator::new(address, STAKE_ARC, 1).unwrap(),
            ],
            1,
        );
    }

    #[test]
    #[should_panic(expected = "total validator stake exceeds u64::MAX")]
    fn test_validator_set_rejects_total_stake_overflow() {
        ValidatorSet::new(
            vec![
                Validator::new(test_addr(0), u64::MAX, 0).unwrap(),
                Validator::new(test_addr(1), STAKE_SPARK, 1).unwrap(),
            ],
            1,
        );
    }

    #[test]
    fn test_six_equal_validators_require_five_for_quorum() {
        let vs = test_validator_set(6);
        let four = [test_addr(0), test_addr(1), test_addr(2), test_addr(3)];
        let five = [
            test_addr(0),
            test_addr(1),
            test_addr(2),
            test_addr(3),
            test_addr(4),
        ];

        assert_eq!(vs.total_stake, 30_000_000);
        assert_eq!(vs.quorum, 20_000_001);
        assert!(!vs.has_quorum(&four), "exactly 2/3 stake is insufficient");
        assert!(vs.has_quorum(&five), "5/6 stake must reach quorum");
        assert!(
            !vs.has_quorum(&[test_addr(0); 5]),
            "repeating one validator identity must not multiply its stake"
        );
    }

    #[test]
    fn test_stake_tier_classification() {
        assert_eq!(StakeTier::from_stake(100), None);
        assert_eq!(StakeTier::from_stake(499_999), None);
        assert_eq!(StakeTier::from_stake(STAKE_SPARK), Some(StakeTier::Spark));
        assert_eq!(StakeTier::from_stake(4_999_999), Some(StakeTier::Spark));
        assert_eq!(StakeTier::from_stake(STAKE_ARC), Some(StakeTier::Arc));
        assert_eq!(StakeTier::from_stake(49_999_999), Some(StakeTier::Arc));
        assert_eq!(StakeTier::from_stake(STAKE_CORE), Some(StakeTier::Core));
        assert_eq!(StakeTier::from_stake(100_000_000), Some(StakeTier::Core));

        assert!(!StakeTier::Spark.can_produce_blocks());
        assert!(StakeTier::Arc.can_produce_blocks());
        assert!(StakeTier::Core.can_produce_blocks());

        assert!(!StakeTier::Spark.can_govern());
        assert!(!StakeTier::Arc.can_govern());
        assert!(StakeTier::Core.can_govern());
    }

    #[test]
    fn test_mixed_validator_set_permissions() {
        let vs = mixed_validator_set();
        assert_eq!(vs.total_stake, STAKE_CORE + 2 * STAKE_ARC + STAKE_SPARK);

        assert!(vs.can_produce_blocks(&test_addr(0)));
        assert!(vs.can_produce_blocks(&test_addr(1)));
        assert!(vs.can_produce_blocks(&test_addr(2)));
        assert!(!vs.can_produce_blocks(&test_addr(3)));

        assert!(!vs.can_produce_blocks(&test_addr(99)));
        assert!(!vs.is_validator(&test_addr(99)));

        let producers = vs.block_producers();
        assert_eq!(producers.len(), 3);
    }

    #[test]
    fn test_join_validator() {
        let validators = vec![Validator::new(test_addr(1), STAKE_ARC, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let result = engine.join_validator(test_addr(2), STAKE_ARC, [2u8; 32], 1);
        assert!(result.is_ok());

        let vs = engine.validator_set();
        assert_eq!(vs.len(), 2);
        assert!(vs.is_validator(&test_addr(2)));
    }

    #[test]
    fn test_join_validator_rejects_total_stake_overflow_atomically() {
        let validators = vec![Validator::new(test_addr(1), u64::MAX, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let result = engine.join_validator(test_addr(2), STAKE_SPARK, [2u8; 32], 1);
        assert!(result.is_err());

        let vs = engine.validator_set();
        assert_eq!(vs.len(), 1, "a failed join must not mutate the set");
        assert_eq!(vs.total_stake, u64::MAX);
        assert!(!vs.is_validator(&test_addr(2)));
    }

    #[test]
    fn test_join_validator_insufficient_stake() {
        let validators = vec![Validator::new(test_addr(1), STAKE_ARC, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let result = engine.join_validator(test_addr(2), 100, [2u8; 32], 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_leave_validator() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
            Validator::new(test_addr(2), STAKE_ARC, 1).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let stake = engine.leave_validator(&test_addr(2)).unwrap();
        assert_eq!(stake, STAKE_ARC);

        let vs = engine.validator_set();
        assert_eq!(vs.len(), 1);
        assert!(!vs.is_validator(&test_addr(2)));
    }

    #[test]
    fn test_update_validator_stake() {
        let validators = vec![Validator::new(test_addr(1), STAKE_ARC, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        engine
            .update_validator_stake(&test_addr(1), STAKE_CORE)
            .unwrap();

        let vs = engine.validator_set();
        let v = vs.get_validator(&test_addr(1)).unwrap();
        assert_eq!(v.stake, STAKE_CORE);
        assert_eq!(v.tier, StakeTier::Core);
    }

    #[test]
    fn test_update_validator_stake_rejects_overflow_atomically() {
        let validators = vec![
            Validator::new(test_addr(1), u64::MAX - STAKE_SPARK, 0).unwrap(),
            Validator::new(test_addr(2), STAKE_SPARK, 1).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let result = engine.update_validator_stake(&test_addr(2), STAKE_SPARK + 1);
        assert!(result.is_err());

        let vs = engine.validator_set();
        assert_eq!(vs.total_stake, u64::MAX);
        assert_eq!(
            vs.get_validator(&test_addr(2)).unwrap().stake,
            STAKE_SPARK,
            "a failed update must retain the old stake"
        );
    }

    #[test]
    fn test_epoch_transition() {
        let validators = vec![Validator::new(test_addr(1), STAKE_ARC, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let new_epoch = engine.epoch_transition();
        assert_eq!(new_epoch, 2);

        let vs = engine.validator_set();
        assert_eq!(vs.epoch, 2);
    }

    #[test]
    fn test_join_validator_duplicate() {
        let validators = vec![Validator::new(test_addr(1), STAKE_ARC, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let result = engine.join_validator(test_addr(1), STAKE_ARC, [1u8; 32], 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_propose_block_round_zero() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx1 = hash_bytes(b"tx1");
        let tx2 = hash_bytes(b"tx2");
        let block = engine
            .propose_block(vec![tx1, tx2], 1000)
            .expect("proposal should succeed");

        assert_eq!(block.author, test_addr(0));
        assert_eq!(block.round, 0);
        assert!(block.parents.is_empty());
        assert_eq!(block.transactions.len(), 2);
        assert_eq!(block.timestamp, 1000);
        assert_ne!(block.hash, Hash256::ZERO);
        assert!(block.verify_hash());

        assert_eq!(engine.dag_size(), 1);
        assert_eq!(engine.blocks_in_round(0).len(), 1);
    }

    #[test]
    fn recovery_domain_refuses_a_conflicting_certificate_binding_without_partial_install() {
        let engine = ConsensusEngine::new(test_validator_set(4), test_addr(0));
        let genesis_domain = ConsensusDomain::new(hash_bytes(b"genesis-domain"), 0, 1);
        let recovery_domain = ConsensusDomain::new(hash_bytes(b"recovery-domain"), 1, 1);
        engine.install_certificate_domain(genesis_domain).unwrap();

        assert!(engine.install_consensus_domain(recovery_domain).is_err());
        assert_eq!(engine.consensus_domain(), None);
        assert_eq!(engine.certificate_domain(), Some(genesis_domain));
    }

    #[test]
    fn recovery_domain_rejection_after_dag_activity_does_not_install_certificate_domain() {
        let engine = ConsensusEngine::new(test_validator_set(4), test_addr(0));
        engine.propose_block(Vec::new(), 1_000).unwrap();
        let domain = ConsensusDomain::new(hash_bytes(b"late-recovery-domain"), 1, 1);

        assert!(engine.install_consensus_domain(domain).is_err());
        assert_eq!(engine.consensus_domain(), None);
        assert_eq!(engine.certificate_domain(), None);
    }

    #[test]
    fn recovery_domain_binds_both_identities_and_remains_idempotent_after_activity() {
        let engine = ConsensusEngine::new(test_validator_set(4), test_addr(0));
        let domain = ConsensusDomain::new(hash_bytes(b"shared-recovery-domain"), 1, 1);
        engine.install_consensus_domain(domain).unwrap();
        engine.propose_block(Vec::new(), 1_000).unwrap();
        engine.install_consensus_domain(domain).unwrap();

        assert_eq!(engine.consensus_domain(), Some(domain));
        assert_eq!(engine.certificate_domain(), Some(domain));
    }

    #[test]
    fn recovery_domain_prevents_cross_epoch_dag_replay() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));
        let domain_a = ConsensusDomain::new(hash_bytes(b"recovery-domain-a"), 1, 7);
        let domain_b = ConsensusDomain::new(hash_bytes(b"recovery-domain-b"), 2, 8);
        engine.install_consensus_domain(domain_a).unwrap();

        let block = engine
            .propose_block(vec![hash_bytes(b"tx")], 1_000)
            .unwrap();

        assert!(block.verify_hash_in_domain(&domain_a));
        assert!(!block.verify_hash_in_domain(&domain_b));
        assert!(!block.verify_hash());
        assert!(engine.install_consensus_domain(domain_b).is_err());
    }

    #[test]
    fn state_decision_commitment_binds_domain_block_hash_and_round() {
        let domain = ConsensusDomain::new(hash_bytes(b"state-decision-domain"), 3, 9);
        let block = make_block_in_domain(
            test_addr(1),
            7,
            vec![hash_bytes(b"parent")],
            vec![hash_bytes(b"tx")],
            1_000,
            &domain,
        );
        let commitment = block.state_decision_commitment(&domain);
        assert_eq!(commitment, block.state_decision_commitment(&domain));
        assert_ne!(commitment, Hash256::ZERO);

        let other_hash_domain =
            ConsensusDomain::new(hash_bytes(b"other-state-decision-domain"), 3, 9);
        let other_epoch = ConsensusDomain::new(domain.domain_hash, 4, 9);
        let other_set = ConsensusDomain::new(domain.domain_hash, 3, 10);
        assert_ne!(
            commitment,
            block.state_decision_commitment(&other_hash_domain)
        );
        assert_ne!(commitment, block.state_decision_commitment(&other_epoch));
        assert_ne!(commitment, block.state_decision_commitment(&other_set));

        let mut other_hash = block.clone();
        other_hash.hash = hash_bytes(b"other-dag-block-hash");
        assert_ne!(commitment, other_hash.state_decision_commitment(&domain));
        let mut other_round = block.clone();
        other_round.round += 1;
        assert_ne!(commitment, other_round.state_decision_commitment(&domain));
    }

    #[test]
    fn signed_recovery_cursor_allows_exactly_one_parentless_domain_round() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));
        assert!(engine.install_recovery_cursor(100).is_err());

        let domain = ConsensusDomain::new(hash_bytes(b"cursor-domain"), 3, 9);
        engine.install_consensus_domain(domain).unwrap();
        assert_eq!(engine.install_recovery_cursor(100).unwrap(), 101);
        assert_eq!(engine.current_round(), 101);
        assert_eq!(engine.last_committed_round(), 101);
        assert!(engine.is_recovery_bootstrap_round(101));

        let own = engine.propose_block(vec![], 1_000).unwrap();
        assert!(own.parents.is_empty());
        assert_eq!(
            engine.propose_block(vec![], 1_001).unwrap_err(),
            ConsensusError::DuplicateBlock,
            "restart/retry must never sign a second block in the same round"
        );

        for author in [test_addr(1), test_addr(2), test_addr(3)] {
            let mut block = make_block(author, 101, vec![], vec![], 1_000);
            block.hash = block.compute_hash_in_domain(&domain);
            engine.receive_block(&block).unwrap();
        }
        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 102);
        let next = engine.propose_block(vec![], 2_000).unwrap();
        assert_eq!(next.parents.len(), 4);

        let mut invalid = make_block(test_addr(3), 102, vec![], vec![], 2_001);
        invalid.hash = invalid.compute_hash_in_domain(&domain);
        assert_eq!(
            engine.receive_block(&invalid).unwrap_err(),
            ConsensusError::InsufficientParents
        );
    }

    #[test]
    fn recovery_round_pauses_at_five_of_six_and_resumes_with_sixth() {
        let engine = ConsensusEngine::new(test_validator_set(6), test_addr(0));
        let domain = ConsensusDomain::new(hash_bytes(b"unanimous-recovery-round"), 3, 9);
        engine.install_consensus_domain(domain).unwrap();
        engine.install_recovery_cursor(100).unwrap();
        engine.propose_block(vec![], 1_000).unwrap();
        for author in (1..5).map(test_addr) {
            let mut block = make_block(author, 101, vec![], vec![], 1_000);
            block.hash = block.compute_hash_in_domain(&domain);
            engine.receive_block(&block).unwrap();
        }
        assert_eq!(engine.blocks_in_round(101).len(), 5);
        assert!(
            !engine.advance_round(),
            "five-of-six must pause rather than skip a future deterministic leader"
        );
        assert_eq!(engine.current_round(), 101);

        let mut sixth = make_block(test_addr(5), 101, vec![], vec![], 1_000);
        sixth.hash = sixth.compute_hash_in_domain(&domain);
        engine.receive_block(&sixth).unwrap();
        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 102);
        assert_eq!(
            engine.propose_block(vec![], 2_000).unwrap().parents.len(),
            6
        );
    }

    #[test]
    fn signed_recovery_proposal_and_receive_require_all_six_canonical_parents() {
        let keys: Vec<_> = (0..6).map(|_| KeyPair::generate_ed25519()).collect();
        let validators = keys
            .iter()
            .enumerate()
            .map(|(index, key)| Validator::new(key.address(), STAKE_ARC, index as u16).unwrap())
            .collect();
        let engine = ConsensusEngine::new_with_keypair(
            ValidatorSet::new(validators, 1),
            keys[0].address(),
            keys[0].clone(),
        );
        let domain = ConsensusDomain::new(hash_bytes(b"signed-six-parent-domain"), 3, 9);
        engine.install_consensus_domain(domain).unwrap();
        engine.install_recovery_cursor(100).unwrap();
        engine.propose_block(vec![], 1_000).unwrap();
        for (index, key) in keys.iter().enumerate().skip(1) {
            let mut block = make_block_in_domain(
                key.address(),
                101,
                Vec::new(),
                Vec::new(),
                1_000 + index as u64,
                &domain,
            );
            block.signature = bincode::serialize(&key.sign(&block.hash).unwrap()).unwrap();
            engine.receive_block(&block).unwrap();
        }
        assert!(engine.advance_round());

        let proposal = engine.propose_block(vec![], 2_000).unwrap();
        assert_eq!(proposal.parents.len(), 6);
        assert!(
            proposal
                .parents
                .windows(2)
                .all(|pair| pair[0].0 <= pair[1].0)
        );

        let mut missing_one = make_block_in_domain(
            keys[1].address(),
            102,
            proposal.parents[..5].to_vec(),
            Vec::new(),
            2_001,
            &domain,
        );
        missing_one.signature =
            bincode::serialize(&keys[1].sign(&missing_one.hash).unwrap()).unwrap();
        assert_eq!(
            engine.receive_block(&missing_one).unwrap_err(),
            ConsensusError::InsufficientParents,
            "a signed recovery block cannot omit one fixed validator parent"
        );
    }

    #[test]
    fn signed_recovery_cursor_refuses_overflow_and_rebinding() {
        let engine = ConsensusEngine::new(test_validator_set(4), test_addr(0));
        engine
            .install_consensus_domain(ConsensusDomain::new(hash_bytes(b"cursor-domain"), 3, 9))
            .unwrap();
        assert!(engine.install_recovery_cursor(u64::MAX).is_err());
        assert_eq!(engine.install_recovery_cursor(50).unwrap(), 51);
        assert!(engine.install_recovery_cursor(51).is_err());
    }

    #[test]
    fn pinned_generation_boundary_is_parent_relaxed_only_during_local_replay() {
        let engine = ConsensusEngine::new(test_validator_set(4), test_addr(0));
        let domain = ConsensusDomain::new(hash_bytes(b"generation-domain"), 4, 12);
        engine.install_consensus_domain(domain).unwrap();
        assert_eq!(engine.install_recovery_cursor(100).unwrap(), 101);
        engine
            .install_recovery_generation_cursor(109, 111, 110)
            .unwrap();
        assert_eq!(engine.current_round(), 111);
        assert_eq!(engine.last_committed_round(), 110);

        let mut boundary = Vec::new();
        for author in [test_addr(0), test_addr(1), test_addr(2), test_addr(3)] {
            let block = make_block_in_domain(
                author,
                109,
                vec![hash_bytes(format!("compacted-parent-{author}").as_bytes())],
                vec![],
                1_000,
                &domain,
            );
            engine.receive_block(&block).unwrap();
            boundary.push(block.hash);
        }
        let child = make_block_in_domain(test_addr(3), 110, boundary, vec![], 1_001, &domain);
        engine.receive_block(&child).unwrap();
        engine.finish_recovery_generation_replay().unwrap();
        assert!(engine.finish_recovery_generation_replay().is_err());

        let late_boundary = make_block_in_domain(
            test_addr(3),
            109,
            vec![hash_bytes(b"missing-after-replay")],
            vec![],
            1_002,
            &domain,
        );
        assert!(engine.receive_block(&late_boundary).is_err());
    }

    #[test]
    fn test_propose_block_spark_tier_rejected() {
        let vs = mixed_validator_set();
        let spark_addr = test_addr(3);
        let engine = ConsensusEngine::new(vs, spark_addr);

        let result = engine.propose_block(vec![], 1000);
        assert_eq!(result.unwrap_err(), ConsensusError::NotValidator);
    }

    #[test]
    fn test_receive_valid_block() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let block = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx1")], 1000);
        engine
            .receive_block(&block)
            .expect("should accept valid block");

        assert_eq!(engine.dag_size(), 1);
        let retrieved = engine.get_block(&block.hash).expect("block should exist");
        assert_eq!(retrieved.author, test_addr(1));
    }

    #[test]
    fn test_receive_duplicate_block_rejected() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let block = make_block(test_addr(1), 0, vec![], vec![], 1000);
        engine.receive_block(&block).expect("first receive ok");

        let result = engine.receive_block(&block);
        assert_eq!(result.unwrap_err(), ConsensusError::DuplicateBlock);
    }

    #[test]
    fn test_receive_block_from_non_validator_rejected() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let block = make_block(test_addr(99), 0, vec![], vec![], 1000);
        let result = engine.receive_block(&block);
        assert_eq!(result.unwrap_err(), ConsensusError::NotValidator);
    }

    #[test]
    fn test_receive_block_invalid_hash_rejected() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let mut block = make_block(test_addr(1), 0, vec![], vec![], 1000);
        block.hash = Hash256::ZERO;

        let result = engine.receive_block(&block);
        assert!(matches!(result, Err(ConsensusError::InvalidBlock(_))));
    }

    #[test]
    fn test_duplicate_parent_references_do_not_multiply_stake() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let parent = make_block(test_addr(0), 0, vec![], vec![], 1000);
        engine.receive_block(&parent).unwrap();

        let child = make_block(
            test_addr(1),
            1,
            vec![parent.hash, parent.hash, parent.hash],
            vec![],
            1001,
        );
        assert_eq!(
            engine.receive_block(&child).unwrap_err(),
            ConsensusError::InsufficientParents,
            "one validator repeated three times must remain below quorum"
        );
    }

    #[test]
    fn test_missing_or_wrong_round_parents_fail_closed() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let missing = make_block(
            test_addr(1),
            1,
            vec![hash_bytes(b"missing-parent")],
            vec![],
            1001,
        );
        assert!(matches!(
            engine.receive_block(&missing),
            Err(ConsensusError::MissingParents { missing: 1, .. })
        ));
        assert_eq!(
            engine.absent_parents(&missing),
            Some(vec![hash_bytes(b"missing-parent")])
        );

        let old_parent = make_block(test_addr(0), 0, vec![], vec![], 1002);
        engine.receive_block(&old_parent).unwrap();
        engine.restore_round_from_local_wal(1, 0);
        let wrong_round = make_block(test_addr(1), 2, vec![old_parent.hash], vec![], 1003);
        assert!(matches!(
            engine.receive_block(&wrong_round),
            Err(ConsensusError::MissingParents { .. })
        ));
        assert_eq!(engine.absent_parents(&wrong_round), None);
        assert_eq!(
            engine.current_round(),
            1,
            "invalid block must not move view"
        );
        assert_eq!(engine.last_committed_round(), 0);
    }

    #[test]
    fn test_force_advance_and_testnet_never_bypass_parent_quorum() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new_testnet(vs, test_addr(0));
        let lone_parent = make_block(test_addr(0), 0, vec![], vec![], 1000);
        engine.receive_block(&lone_parent).unwrap();

        engine.force_advance_round();
        assert!(engine.is_force_advanced());
        assert_eq!(engine.current_round(), 1);
        assert_eq!(
            engine.propose_block(vec![], 1001).unwrap_err(),
            ConsensusError::InsufficientParents
        );

        let child = make_block(test_addr(1), 1, vec![lone_parent.hash], vec![], 1002);
        assert_eq!(
            engine.receive_block(&child).unwrap_err(),
            ConsensusError::InsufficientParents
        );

        let unknown = make_block(test_addr(99), 0, vec![], vec![], 1003);
        assert_eq!(
            engine.receive_block(&unknown).unwrap_err(),
            ConsensusError::NotValidator
        );
    }

    #[test]
    fn test_receive_block_future_round_requires_authenticated_sync() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));
        let block = make_block(test_addr(1), 5, vec![], vec![], 1000);
        let result = engine.receive_block(&block);
        assert!(
            matches!(
                &result,
                Err(ConsensusError::RoundTooFarAhead {
                    round: 5,
                    current: 0
                })
            ),
            "single-peer future block must not move consensus state: {result:?}"
        );
        assert_eq!(engine.current_round(), 0);
        assert_eq!(engine.last_committed_round(), 0);
    }

    fn install_certified_leader_equivocation(
        engine: &ConsensusEngine,
        domain: &ConsensusDomain,
        reverse_arrival: bool,
    ) -> (DagBlock, DagBlock) {
        let mut validators: Vec<_> = engine
            .frozen_validator_set()
            .validators
            .iter()
            .map(|validator| validator.address)
            .collect();
        validators.sort_by_key(|address| address.0);
        assert_eq!(validators.len(), 6);
        let leader = validators[0];

        let first = make_block_in_domain(
            leader,
            0,
            Vec::new(),
            vec![hash_bytes(b"equivocation-transfer-a")],
            100,
            domain,
        );
        let second = make_block_in_domain(
            leader,
            0,
            Vec::new(),
            vec![hash_bytes(b"equivocation-transfer-b")],
            101,
            domain,
        );
        let mut round_zero = vec![first.clone(), second.clone()];
        round_zero.extend(
            validators
                .iter()
                .copied()
                .filter(|author| *author != leader)
                .enumerate()
                .map(|(index, author)| {
                    make_block_in_domain(
                        author,
                        0,
                        Vec::new(),
                        Vec::new(),
                        110 + index as u64,
                        domain,
                    )
                }),
        );
        if reverse_arrival {
            round_zero.reverse();
        }
        for block in &round_zero {
            engine.receive_block(block).unwrap();
        }
        assert!(engine.advance_round());

        let mut round_zero_parents: Vec<_> = round_zero.iter().map(|block| block.hash).collect();
        round_zero_parents.sort_by_key(|hash| hash.0);
        let mut round_one: Vec<_> = validators
            .iter()
            .copied()
            .enumerate()
            .map(|(index, author)| {
                make_block_in_domain(
                    author,
                    1,
                    round_zero_parents.clone(),
                    Vec::new(),
                    200 + index as u64,
                    domain,
                )
            })
            .collect();
        if reverse_arrival {
            round_one.reverse();
        }
        for block in &round_one {
            engine.receive_block(block).unwrap();
        }
        assert!(engine.advance_round());

        let mut round_one_parents: Vec<_> = round_one.iter().map(|block| block.hash).collect();
        round_one_parents.sort_by_key(|hash| hash.0);
        let mut round_two: Vec<_> = validators
            .iter()
            .copied()
            .enumerate()
            .map(|(index, author)| {
                make_block_in_domain(
                    author,
                    2,
                    round_one_parents.clone(),
                    Vec::new(),
                    300 + index as u64,
                    domain,
                )
            })
            .collect();
        if reverse_arrival {
            round_two.reverse();
        }
        for block in &round_two {
            engine.receive_block(block).unwrap();
        }
        (first, second)
    }

    fn validator_stakes(engine: &ConsensusEngine) -> Vec<(Address, u64)> {
        let mut stakes: Vec<_> = engine
            .validator_set()
            .validators
            .iter()
            .map(|validator| (validator.address, validator.stake))
            .collect();
        stakes.sort_by_key(|entry| entry.0.0);
        stakes
    }

    #[test]
    fn recovery_domain_certified_leader_equivocation_fences_both_arrival_orders() {
        let domain = ConsensusDomain::new(hash_bytes(b"equivocation-domain"), 1, 1);
        let validator_set = test_validator_set(6);
        let engine_forward = ConsensusEngine::new(validator_set.clone(), test_addr(0));
        let engine_reverse = ConsensusEngine::new(validator_set, test_addr(0));
        engine_forward.install_consensus_domain(domain).unwrap();
        engine_reverse.install_consensus_domain(domain).unwrap();
        let initial_forward = validator_stakes(&engine_forward);
        let initial_reverse = validator_stakes(&engine_reverse);

        let (forward_first, forward_second) =
            install_certified_leader_equivocation(&engine_forward, &domain, false);
        let (reverse_first, reverse_second) =
            install_certified_leader_equivocation(&engine_reverse, &domain, true);
        let frozen = engine_forward.frozen_validator_set();
        assert!(
            engine_forward
                .two_round_commit_support(&forward_first.hash, 0, &frozen)
                .is_some()
        );
        assert!(
            engine_forward
                .two_round_commit_support(&forward_second.hash, 0, &frozen)
                .is_some()
        );
        assert_eq!(forward_first.hash, reverse_first.hash);
        assert_eq!(forward_second.hash, reverse_second.hash);

        for engine in [&engine_forward, &engine_reverse] {
            assert!(engine.try_commit().is_empty());
            assert!(engine.try_commit().is_empty());
            assert_eq!(engine.last_committed_round(), 0);
            assert!(engine.committed_blocks().is_empty());
        }
        assert_eq!(validator_stakes(&engine_forward), initial_forward);
        assert_eq!(validator_stakes(&engine_reverse), initial_reverse);
        assert_eq!(engine_forward.validator_set().total_stake, 6 * STAKE_ARC);
        assert_eq!(engine_reverse.validator_set().total_stake, 6 * STAKE_ARC);
    }

    fn commits_over_rounds(
        n: usize,
        rounds: u64,
        link_all: bool,
        missing: Option<(usize, u64)>,
    ) -> usize {
        let vs = test_validator_set(n);
        let per = vs.total_stake / n as u64;
        let quorum_authors = vs.quorum.div_ceil(per) as usize;
        let engine = ConsensusEngine::new(vs, test_addr(0));
        let mut previous: Vec<Hash256> = Vec::new();
        let mut committed = 0usize;
        for round in 0..rounds {
            let mut current = Vec::new();
            for author in 0..n {
                if let Some((absent, from)) = missing
                    && author == absent
                    && round >= from
                {
                    continue;
                }
                let parents: Vec<Hash256> = if previous.is_empty() {
                    Vec::new()
                } else if link_all || quorum_authors >= previous.len() {
                    previous.clone()
                } else {
                    let start = (author + round as usize) % previous.len();
                    (0..quorum_authors)
                        .map(|k| previous[(start + k) % previous.len()])
                        .collect()
                };
                let block = make_block(
                    test_addr(author as u8),
                    round,
                    parents,
                    vec![],
                    1000 + round * 10 + author as u64,
                );
                engine.receive_block(&block).unwrap();
                current.push(block.hash);
            }
            if !engine.advance_round() {
                break;
            }
            committed += engine.try_commit().len();
            previous = current;
        }
        committed
    }

    fn leader_index_for_round(n: usize, round: u64) -> usize {
        let mut order: Vec<(Address, usize)> = (0..n).map(|i| (test_addr(i as u8), i)).collect();
        order.sort_by_key(|(address, _)| address.0);
        order[round as usize % n].1
    }

    #[test]
    fn commit_rule_commits_at_every_committee_size_when_every_leader_block_is_present() {
        for n in 2..=5 {
            for link_all in [true, false] {
                let committed = commits_over_rounds(n, 8, link_all, None);
                assert!(
                    committed > 0,
                    "n={n} link_all={link_all}: committed nothing in 8 rounds"
                );
            }
        }
    }

    #[test]
    fn commit_rule_passes_a_missing_anchor_once_a_later_anchor_decides_it() {
        for n in 4..=5usize {
            let absent = leader_index_for_round(n, 2);
            let committed = commits_over_rounds(n, 12, false, Some((absent, 2)));
            let healthy = commits_over_rounds(n, 12, false, None);
            assert!(
                committed > 2,
                "n={n}: the cursor stopped at the missing anchor again ({committed} commits)"
            );
            assert!(
                healthy >= committed,
                "n={n}: a complete committee ({healthy}) should not commit less than one \
                 missing a member ({committed})"
            );
        }
        for n in 2..=3usize {
            assert_eq!(
                commits_over_rounds(n, 12, false, Some((leader_index_for_round(n, 2), 2))),
                commits_over_rounds(n, 2, false, None),
                "n={n}: a committee whose quorum is the whole set must fail-stop"
            );
        }
    }

    #[test]
    fn dag_retention_is_configurable_but_never_below_the_reorg_floor() {
        let engine = ConsensusEngine::new(test_validator_set(4), test_addr(0));
        assert_eq!(engine.retained_rounds(), DEFAULT_RETAINED_ROUNDS);
        engine.set_retained_rounds(50_000);
        assert_eq!(engine.retained_rounds(), 50_000);
        engine.set_retained_rounds(1);
        assert_eq!(engine.retained_rounds(), PRUNE_DEPTH);
        engine.set_retained_rounds(0);
        assert_eq!(engine.retained_rounds(), PRUNE_DEPTH);
    }

    #[test]
    fn retention_decides_how_far_back_history_can_be_served() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));
        engine.set_retained_rounds(PRUNE_DEPTH);
        let mut previous: Vec<Hash256> = Vec::new();
        for round in 0..(PRUNE_DEPTH + 40) {
            let mut current = Vec::new();
            for author in 0..4u8 {
                let block = make_block(
                    test_addr(author),
                    round,
                    previous.clone(),
                    vec![],
                    5_000 + round * 10 + author as u64,
                );
                engine.receive_block(&block).unwrap();
                current.push(block.hash);
            }
            assert!(engine.advance_round());
            let _ = engine.try_commit();
            previous = current;
        }
        let cursor = engine.last_committed_round();
        assert!(
            cursor > PRUNE_DEPTH,
            "the chain must have committed past the window"
        );
        assert!(
            !engine.blocks_in_round(cursor - 1).is_empty(),
            "a round just below the cursor must still be retained"
        );
        assert!(
            engine.blocks_in_round(0).is_empty(),
            "round 0 should have been pruned once the cursor passed the window"
        );
    }

    #[test]
    fn a_certified_anchor_is_never_skipped_by_the_retroactive_rule() {
        let n = 4usize;
        let vs = test_validator_set(n);
        let engine = ConsensusEngine::new(vs.clone(), test_addr(0));
        let mut previous: Vec<Hash256> = Vec::new();
        let mut anchors: Vec<(u64, Hash256)> = Vec::new();
        for round in 0..10u64 {
            let mut current = Vec::new();
            for author in 0..n {
                let block = make_block(
                    test_addr(author as u8),
                    round,
                    previous.clone(),
                    vec![],
                    2_000 + round * 10 + author as u64,
                );
                engine.receive_block(&block).unwrap();
                let leader = leader_index_for_round(n, round);
                if author == leader {
                    anchors.push((round, block.hash));
                }
                current.push(block.hash);
            }
            assert!(engine.advance_round());
            previous = current;
        }
        for (round, hash) in &anchors {
            if engine.leader_commit_support(hash, *round).is_none() {
                continue;
            }
            for (later_round, later_hash) in &anchors {
                if later_round <= round {
                    continue;
                }
                assert!(
                    engine.causal_history_contains(later_hash, hash, *round),
                    "certified anchor at round {round} is missing from the history of the \
                     anchor at round {later_round}"
                );
            }
        }
    }

    #[test]
    fn test_commit_rule_two_round() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let block_b = make_block(test_addr(0), 0, vec![], vec![hash_bytes(b"tx_b")], 100);
        engine.receive_block(&block_b).unwrap();

        let b1 = make_block(test_addr(1), 0, vec![], vec![], 101);
        let b2 = make_block(test_addr(2), 0, vec![], vec![], 102);
        let b3 = make_block(test_addr(3), 0, vec![], vec![], 103);
        engine.receive_block(&b1).unwrap();
        engine.receive_block(&b2).unwrap();
        engine.receive_block(&b3).unwrap();

        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 1);

        let block_c = make_block(
            test_addr(1),
            1,
            vec![block_b.hash, b1.hash, b2.hash],
            vec![hash_bytes(b"tx_c")],
            200,
        );
        engine.receive_block(&block_c).unwrap();

        let c2 = make_block(
            test_addr(2),
            1,
            vec![block_b.hash, b1.hash, b2.hash],
            vec![],
            201,
        );
        let c3 = make_block(
            test_addr(3),
            1,
            vec![block_b.hash, b1.hash, b3.hash],
            vec![],
            202,
        );
        engine.receive_block(&c2).unwrap();
        engine.receive_block(&c3).unwrap();

        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 2);

        let committed = engine.try_commit();
        assert!(committed.is_empty());

        let d0 = make_block(
            test_addr(0),
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            300,
        );
        let d2 = make_block(
            test_addr(2),
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            301,
        );
        let d3 = make_block(
            test_addr(3),
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            302,
        );
        engine.receive_block(&d0).unwrap();
        engine.receive_block(&d2).unwrap();
        engine.receive_block(&d3).unwrap();

        let committed = engine.try_commit();
        assert!(
            committed.iter().any(|b| b.hash == block_b.hash),
            "Block B should be committed"
        );

        let committed_list = engine.committed_blocks();
        assert!(committed_list.contains(&block_b.hash));
    }

    #[test]
    fn test_commit_not_triggered_without_quorum_in_r2() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let b0 = make_block(test_addr(0), 0, vec![], vec![], 100);
        let b1 = make_block(test_addr(1), 0, vec![], vec![], 101);
        let b2 = make_block(test_addr(2), 0, vec![], vec![], 102);
        let b3 = make_block(test_addr(3), 0, vec![], vec![], 103);
        engine.receive_block(&b0).unwrap();
        engine.receive_block(&b1).unwrap();
        engine.receive_block(&b2).unwrap();
        engine.receive_block(&b3).unwrap();
        engine.advance_round();

        let c1 = make_block(
            test_addr(1),
            1,
            vec![b0.hash, b1.hash, b2.hash],
            vec![],
            200,
        );
        let c2 = make_block(
            test_addr(2),
            1,
            vec![b0.hash, b1.hash, b2.hash],
            vec![],
            201,
        );
        let c3 = make_block(
            test_addr(3),
            1,
            vec![b0.hash, b1.hash, b3.hash],
            vec![],
            202,
        );
        engine.receive_block(&c1).unwrap();
        engine.receive_block(&c2).unwrap();
        engine.receive_block(&c3).unwrap();
        engine.advance_round();

        let d0 = make_block(
            test_addr(0),
            2,
            vec![c1.hash, c2.hash, c3.hash],
            vec![],
            300,
        );
        engine.receive_block(&d0).unwrap();

        let committed = engine.try_commit();
        let b0_committed = committed.iter().any(|b| b.hash == b0.hash);
        assert!(
            !b0_committed,
            "B0 should NOT be committed with insufficient R+2 support"
        );
    }

    #[test]
    fn test_commit_scan_never_skips_delayed_leader_round() {
        let vs = test_validator_set(4);
        let mut ordered: Vec<Address> = vs.validators.iter().map(|v| v.address).collect();
        ordered.sort_by_key(|address| address.0);
        let leader = ordered[0];
        let engine = ConsensusEngine::new(vs, ordered[1]);

        for (index, author) in ordered
            .iter()
            .copied()
            .filter(|address| *address != leader)
            .enumerate()
        {
            let block = make_block(author, 0, vec![], vec![], 100 + index as u64);
            engine.receive_block(&block).unwrap();
        }
        engine.restore_round_from_local_wal(100, 0);

        assert!(engine.try_commit().is_empty());
        assert_eq!(engine.last_committed_round(), 0);

        let delayed_leader = make_block(leader, 0, vec![], vec![], 200);
        engine.receive_block(&delayed_leader).unwrap();
        assert!(engine.get_block(&delayed_leader.hash).is_some());
        assert_eq!(engine.last_committed_round(), 0);
    }

    #[test]
    fn test_advance_round_requires_quorum() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        assert!(!engine.advance_round());
        assert_eq!(engine.current_round(), 0);

        let b0 = make_block(test_addr(0), 0, vec![], vec![], 100);
        let b1 = make_block(test_addr(1), 0, vec![], vec![], 101);
        engine.receive_block(&b0).unwrap();
        engine.receive_block(&b1).unwrap();
        assert!(!engine.advance_round());
        assert_eq!(engine.current_round(), 0);

        let b2 = make_block(test_addr(2), 0, vec![], vec![], 102);
        engine.receive_block(&b2).unwrap();
        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 1);
    }

    #[test]
    fn test_advance_round_deduplicates_authors() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let b0a = make_block(test_addr(0), 0, vec![], vec![hash_bytes(b"a")], 100);
        let b0b = make_block(test_addr(0), 0, vec![], vec![hash_bytes(b"b")], 101);
        engine.receive_block(&b0a).unwrap();
        engine.receive_block(&b0b).unwrap();

        assert!(!engine.advance_round());

        let b1 = make_block(test_addr(1), 0, vec![], vec![], 102);
        let b2 = make_block(test_addr(2), 0, vec![], vec![], 103);
        engine.receive_block(&b1).unwrap();
        engine.receive_block(&b2).unwrap();
        assert!(engine.advance_round());
    }

    #[test]
    fn test_shard_assignment_deterministic() {
        let addr = test_addr(42);
        let shard1 = ConsensusEngine::shard_of(&addr, 8);
        let shard2 = ConsensusEngine::shard_of(&addr, 8);
        assert_eq!(shard1, shard2, "shard assignment must be deterministic");

        assert!(shard1 < 8);
    }

    #[test]
    fn test_shard_assignment_distributes() {
        let num_shards = 4u16;
        let mut shard_counts = vec![0u32; num_shards as usize];

        for i in 0..100u8 {
            let addr = test_addr(i);
            let shard = ConsensusEngine::shard_of(&addr, num_shards);
            assert!(shard < num_shards);
            shard_counts[shard as usize] += 1;
        }

        for (shard_id, &count) in shard_counts.iter().enumerate() {
            assert!(
                count > 0,
                "shard {} has zero assignments out of 100 addresses",
                shard_id
            );
        }
    }

    #[test]
    fn test_shard_of_single_shard() {
        let addr = test_addr(42);
        assert_eq!(ConsensusEngine::shard_of(&addr, 1), 0);
    }

    #[test]
    fn test_shard_of_zero_shards() {
        let addr = test_addr(42);
        assert_eq!(ConsensusEngine::shard_of(&addr, 0), 0);
    }

    #[test]
    fn test_same_shard_transfer_fast_path() {
        let num_shards = 4u16;
        let from_addr = test_addr(0);
        let from_shard = ConsensusEngine::shard_of(&from_addr, num_shards);

        let mut to_addr = Hash256::ZERO;
        for i in 1..=255u8 {
            let candidate = test_addr(i);
            if ConsensusEngine::shard_of(&candidate, num_shards) == from_shard {
                to_addr = candidate;
                break;
            }
        }
        assert_ne!(to_addr, Hash256::ZERO, "should find a same-shard address");

        let tx = Transaction::new_transfer(from_addr, to_addr, 1000, 0);
        assert!(
            !ConsensusEngine::needs_full_consensus(&tx, num_shards),
            "same-shard transfer should use fast path"
        );
    }

    #[test]
    fn test_cross_shard_transfer_full_consensus() {
        let num_shards = 4u16;
        let from_addr = test_addr(0);
        let from_shard = ConsensusEngine::shard_of(&from_addr, num_shards);

        let mut to_addr = Hash256::ZERO;
        for i in 1..=255u8 {
            let candidate = test_addr(i);
            if ConsensusEngine::shard_of(&candidate, num_shards) != from_shard {
                to_addr = candidate;
                break;
            }
        }
        assert_ne!(to_addr, Hash256::ZERO, "should find a cross-shard address");

        let tx = Transaction::new_transfer(from_addr, to_addr, 1000, 0);
        assert!(
            ConsensusEngine::needs_full_consensus(&tx, num_shards),
            "cross-shard transfer should need full consensus"
        );
    }

    #[test]
    fn test_single_shard_always_fast_path() {
        let from_addr = test_addr(0);
        let to_addr = test_addr(1);
        let tx = Transaction::new_transfer(from_addr, to_addr, 1000, 0);

        assert!(
            !ConsensusEngine::needs_full_consensus(&tx, 1),
            "single shard should always use fast path"
        );
    }

    #[test]
    fn test_deploy_contract_always_local() {
        let from_addr = test_addr(0);
        let tx = Transaction::new_deploy(
            from_addr,
            vec![0x00, 0x61, 0x73, 0x6d],
            vec![],
            1000,
            50,
            100_000,
            0,
        );
        assert!(
            !ConsensusEngine::needs_full_consensus(&tx, 8),
            "deploy contract is always local to sender shard"
        );
    }

    #[test]
    fn test_cross_shard_receipt_construction() {
        let receipt = CrossShardReceipt {
            source_shard: 2,
            source_block: hash_bytes(b"block_on_shard_2"),
            tx_hash: hash_bytes(b"cross_shard_tx"),
            amount: 5000,
            recipient: test_addr(42),
        };

        assert_eq!(receipt.source_shard, 2);
        assert_eq!(receipt.amount, 5000);
        assert_eq!(receipt.recipient, test_addr(42));
        assert_ne!(receipt.source_block, Hash256::ZERO);
        assert_ne!(receipt.tx_hash, Hash256::ZERO);
    }

    #[test]
    fn test_dag_block_hash_deterministic() {
        let b1 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx1")], 1000);
        let b2 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx1")], 1000);
        assert_eq!(
            b1.hash, b2.hash,
            "identical blocks must have identical hashes"
        );
    }

    #[test]
    fn test_dag_block_hash_changes_with_content() {
        let b1 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx1")], 1000);
        let b2 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx2")], 1000);
        assert_ne!(
            b1.hash, b2.hash,
            "different transactions must produce different hashes"
        );

        let b3 = make_block(test_addr(2), 0, vec![], vec![hash_bytes(b"tx1")], 1000);
        assert_ne!(
            b1.hash, b3.hash,
            "different authors must produce different hashes"
        );
    }

    #[test]
    fn test_full_three_round_lifecycle() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let r0_b0 = engine
            .propose_block(vec![hash_bytes(b"r0_tx0")], 100)
            .unwrap();
        let r0_b1 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"r0_tx1")], 101);
        let r0_b2 = make_block(test_addr(2), 0, vec![], vec![hash_bytes(b"r0_tx2")], 102);
        let r0_b3 = make_block(test_addr(3), 0, vec![], vec![hash_bytes(b"r0_tx3")], 103);
        engine.receive_block(&r0_b1).unwrap();
        engine.receive_block(&r0_b2).unwrap();
        engine.receive_block(&r0_b3).unwrap();

        assert_eq!(engine.dag_size(), 4);
        assert_eq!(engine.blocks_in_round(0).len(), 4);
        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 1);

        let all_r0 = vec![r0_b0.hash, r0_b1.hash, r0_b2.hash];
        let r1_b0 = engine
            .propose_block(vec![hash_bytes(b"r1_tx0")], 200)
            .unwrap();
        let r1_b1 = make_block(test_addr(1), 1, all_r0.clone(), vec![], 201);
        let r1_b2 = make_block(test_addr(2), 1, all_r0.clone(), vec![], 202);
        let r1_b3 = make_block(
            test_addr(3),
            1,
            vec![r0_b0.hash, r0_b1.hash, r0_b3.hash],
            vec![],
            203,
        );
        engine.receive_block(&r1_b1).unwrap();
        engine.receive_block(&r1_b2).unwrap();
        engine.receive_block(&r1_b3).unwrap();

        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 2);

        let all_r1 = vec![r1_b0.hash, r1_b1.hash, r1_b2.hash];
        let _r2_b0 = engine
            .propose_block(vec![hash_bytes(b"r2_tx0")], 300)
            .unwrap();
        let r2_b1 = make_block(test_addr(1), 2, all_r1.clone(), vec![], 301);
        let r2_b2 = make_block(test_addr(2), 2, all_r1.clone(), vec![], 302);
        let r2_b3 = make_block(
            test_addr(3),
            2,
            vec![r1_b0.hash, r1_b1.hash, r1_b3.hash],
            vec![],
            303,
        );
        engine.receive_block(&r2_b1).unwrap();
        engine.receive_block(&r2_b2).unwrap();
        engine.receive_block(&r2_b3).unwrap();

        assert_eq!(engine.dag_size(), 12);

        let committed = engine.try_commit();
        assert!(
            !committed.is_empty(),
            "should have committed at least one block from round 0"
        );

        for block in &committed {
            assert_eq!(block.round, 0);
        }

        let committed_list = engine.committed_blocks();
        assert!(!committed_list.is_empty());
    }

    #[test]
    fn test_update_validator_set() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        assert_eq!(engine.validator_set().epoch, 1);
        assert_eq!(engine.validator_set().len(), 4);

        let new_vs = ValidatorSet::new(
            (0..6)
                .map(|i| Validator::new(test_addr(i as u8), STAKE_ARC, i as u16).unwrap())
                .collect(),
            2,
        );
        engine.update_validator_set(new_vs).unwrap();

        assert_eq!(engine.validator_set().epoch, 2);
        assert_eq!(engine.validator_set().len(), 6);
    }

    #[test]
    fn test_update_validator_set_rejects_forged_cached_quorum() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));
        let before = engine.validator_set();

        let mut forged = test_validator_set(6);
        forged.total_stake = u64::MAX;
        forged.quorum = 1;
        assert!(engine.update_validator_set(forged).is_err());

        let after = engine.validator_set();
        assert_eq!(after.validators.len(), before.validators.len());
        assert_eq!(after.total_stake, before.total_stake);
        assert_eq!(after.quorum, before.quorum);
    }

    #[test]
    fn test_cross_shard_assign_shard_deterministic() {
        let addr = test_addr(42);
        let s1 = assign_shard(&addr, 8);
        let s2 = assign_shard(&addr, 8);
        let s3 = assign_shard(&addr, 8);
        assert_eq!(s1, s2);
        assert_eq!(s2, s3);
        assert!(s1 < 8, "shard must be within range");
    }

    #[test]
    fn test_cross_shard_detection() {
        let num_shards = 4u16;
        let addr_a = test_addr(0);
        let shard_a = assign_shard(&addr_a, num_shards);

        let mut same_shard_addr = None;
        let mut diff_shard_addr = None;
        for i in 1..=255u8 {
            let candidate = test_addr(i);
            let s = assign_shard(&candidate, num_shards);
            if s == shard_a && same_shard_addr.is_none() {
                same_shard_addr = Some(candidate);
            }
            if s != shard_a && diff_shard_addr.is_none() {
                diff_shard_addr = Some(candidate);
            }
            if same_shard_addr.is_some() && diff_shard_addr.is_some() {
                break;
            }
        }

        let same = same_shard_addr.expect("should find same-shard address");
        let diff = diff_shard_addr.expect("should find diff-shard address");

        assert!(
            !is_cross_shard(&addr_a, &same, num_shards),
            "same-shard pair should not be detected as cross-shard"
        );
        assert!(
            is_cross_shard(&addr_a, &diff, num_shards),
            "different-shard pair should be detected as cross-shard"
        );
        assert!(!is_cross_shard(&addr_a, &diff, 1));
    }

    #[test]
    fn test_cross_shard_lock_commit() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"cross_shard_tx_1");
        let source_block = hash_bytes(b"source_block_1");

        let proof = engine
            .lock_cross_shard(tx, 0, 1, source_block, 0)
            .expect("lock should succeed");
        assert_eq!(proof.status, CrossShardStatus::Locked);
        assert_eq!(proof.source_shard, 0);
        assert_eq!(proof.target_shard, 1);
        assert_eq!(proof.tx_hash, tx);
        assert_eq!(proof.source_block_hash, source_block);
        assert_eq!(proof.source_round, 0);
        assert_ne!(proof.lock_hash, Hash256::ZERO);
        assert!(!proof.inclusion_proof.is_empty());
        assert_eq!(engine.cross_shard_stats(), (1, 0));

        let committed = engine
            .commit_cross_shard(tx)
            .expect("commit should succeed");
        assert_eq!(committed.status, CrossShardStatus::Committed);
        assert_eq!(committed.tx_hash, tx);
        assert_eq!(engine.cross_shard_stats(), (0, 1));
        assert!(!engine.pending_cross_shard.contains_key(&tx));
        assert!(engine.completed_cross_shard.contains_key(&tx));
    }

    #[test]
    fn test_cross_shard_lock_abort() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"cross_shard_tx_abort");
        let source_block = hash_bytes(b"source_block_abort");

        let proof = engine
            .lock_cross_shard(tx, 2, 3, source_block, 5)
            .expect("lock should succeed");
        assert_eq!(proof.status, CrossShardStatus::Locked);

        let aborted = engine.abort_cross_shard(tx).expect("abort should succeed");
        assert_eq!(aborted.status, CrossShardStatus::Aborted);
        assert_eq!(aborted.source_shard, 2);
        assert_eq!(aborted.target_shard, 3);
        assert_eq!(engine.cross_shard_stats(), (0, 1));

        let err = engine.commit_cross_shard(tx);
        assert!(
            matches!(err, Err(ConsensusError::CrossShardLockNotFound(_))),
            "commit after abort should fail with LockNotFound"
        );
    }

    #[test]
    fn test_atomic_batch_all_succeed() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx1 = hash_bytes(b"batch_tx_1");
        let tx2 = hash_bytes(b"batch_tx_2");
        let tx3 = hash_bytes(b"batch_tx_3");
        let tx_hashes = vec![tx1, tx2, tx3];
        let shards = vec![(0, 1), (0, 2), (1, 3)];

        let proofs = engine
            .atomic_cross_shard_batch(tx_hashes.clone(), shards)
            .expect("batch should succeed");
        assert_eq!(proofs.len(), 3);
        for proof in &proofs {
            assert_eq!(proof.status, CrossShardStatus::Locked);
        }
        assert_eq!(engine.cross_shard_stats(), (3, 0));

        for tx in &tx_hashes {
            engine.commit_cross_shard(*tx).expect("commit should work");
        }
        assert_eq!(engine.cross_shard_stats(), (0, 3));
    }

    #[test]
    fn test_atomic_batch_partial_failure() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx1 = hash_bytes(b"batch_fail_tx_1");
        let tx2 = hash_bytes(b"batch_fail_tx_2");
        let tx3 = hash_bytes(b"batch_fail_tx_3");

        engine
            .lock_cross_shard(tx2, 0, 2, hash_bytes(b"pre_block"), 0)
            .expect("pre-lock should succeed");
        assert_eq!(engine.cross_shard_stats(), (1, 0));

        let result =
            engine.atomic_cross_shard_batch(vec![tx1, tx2, tx3], vec![(0, 1), (0, 2), (1, 3)]);
        assert!(
            result.is_err(),
            "batch should fail because tx2 is already locked"
        );

        assert!(engine.completed_cross_shard.get(&tx1).is_some());
        let tx1_proof = engine.completed_cross_shard.get(&tx1).unwrap();
        assert_eq!(tx1_proof.status, CrossShardStatus::Aborted);

        assert!(engine.pending_cross_shard.contains_key(&tx2));

        assert!(!engine.pending_cross_shard.contains_key(&tx3));
        assert!(!engine.completed_cross_shard.contains_key(&tx3));

        assert_eq!(engine.cross_shard_stats(), (1, 1));
    }

    #[test]
    fn test_ordering_commitment_deterministic() {
        let txs = vec![hash_bytes(b"tx1"), hash_bytes(b"tx2"), hash_bytes(b"tx3")];
        let c1 = DagBlock::compute_ordering_commitment(&txs);
        let c2 = DagBlock::compute_ordering_commitment(&txs);
        assert_eq!(c1, c2, "same tx order must produce same commitment");
    }

    #[test]
    fn test_ordering_commitment_order_sensitive() {
        let txs_a = vec![hash_bytes(b"tx1"), hash_bytes(b"tx2")];
        let txs_b = vec![hash_bytes(b"tx2"), hash_bytes(b"tx1")];
        let ca = DagBlock::compute_ordering_commitment(&txs_a);
        let cb = DagBlock::compute_ordering_commitment(&txs_b);
        assert_ne!(
            ca, cb,
            "different tx order must produce different commitment"
        );
    }

    #[test]
    fn test_block_verifies_correct_ordering() {
        let block = make_block(
            test_addr(0),
            0,
            vec![],
            vec![hash_bytes(b"tx1"), hash_bytes(b"tx2")],
            1000,
        );
        assert!(
            block.verify_ordering(),
            "block with correct ordering should verify"
        );
    }

    #[test]
    fn test_block_rejects_tampered_ordering() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let txs = vec![hash_bytes(b"tx1"), hash_bytes(b"tx2")];
        let mut block = make_block(test_addr(1), 0, vec![], txs, 1000);

        block.ordering_commitment =
            DagBlock::compute_ordering_commitment(&[hash_bytes(b"tx2"), hash_bytes(b"tx1")]);
        block.hash = block.compute_hash();

        let result = engine.receive_block(&block);
        assert!(
            matches!(result, Err(ConsensusError::MevOrderingViolation(_))),
            "reordered transactions should be rejected as MEV violation"
        );
    }

    #[test]
    fn test_block_rejects_unsorted_transactions() {
        let tx_a = hash_bytes(b"aaa");
        let tx_b = hash_bytes(b"bbb");

        let (first, second) = if tx_a.0 <= tx_b.0 {
            (tx_a, tx_b)
        } else {
            (tx_b, tx_a)
        };

        let unsorted = vec![second, first];
        let commitment = DagBlock::compute_ordering_commitment(&unsorted);
        let mut block = DagBlock {
            author: test_addr(0),
            round: 0,
            parents: vec![],
            transactions: unsorted,
            timestamp: 1000,
            hash: Hash256::ZERO,
            signature: Vec::new(),
            ordering_commitment: commitment,
        };
        block.hash = block.compute_hash();

        assert!(
            !block.verify_ordering(),
            "block with unsorted transactions must fail ordering verification"
        );
    }

    #[test]
    fn test_propose_block_sorts_transactions() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx_a = hash_bytes(b"alpha");
        let tx_b = hash_bytes(b"bravo");
        let tx_c = hash_bytes(b"charlie");

        let mut expected = vec![tx_a, tx_b, tx_c];
        expected.sort_by_key(|tx| tx.0);

        let reversed: Vec<Hash256> = expected.iter().rev().copied().collect();

        let block = engine.propose_block(reversed, 1000).unwrap();

        assert_eq!(
            block.transactions, expected,
            "propose_block must sort transactions into canonical lexicographic order"
        );
        assert!(
            block.verify_ordering(),
            "proposed block must pass ordering verification"
        );
    }

    #[test]
    fn test_cross_shard_lock_has_timestamp() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"timestamped_lock");
        let proof = engine
            .lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0)
            .unwrap();
        assert!(
            proof.locked_at_ms > 0,
            "lock should have a non-zero timestamp"
        );
    }

    #[test]
    fn test_expire_stale_locks_no_stale() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"fresh_lock");
        engine
            .lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0)
            .unwrap();

        let expired = engine.expire_stale_locks();
        assert_eq!(expired, 0, "fresh lock should not be expired");
        assert_eq!(engine.cross_shard_stats(), (1, 0));
    }

    #[test]
    fn test_expire_stale_locks_force_old() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"old_lock");
        engine
            .lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0)
            .unwrap();

        if let Some(mut entry) = engine.pending_cross_shard.get_mut(&tx) {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            entry.locked_at_ms = now_ms - 60_000;
        }

        let expired = engine.expire_stale_locks();
        assert_eq!(expired, 1, "stale lock should be expired");
        assert_eq!(engine.cross_shard_stats(), (0, 1));

        let proof = engine.completed_cross_shard.get(&tx).unwrap();
        assert_eq!(proof.status, CrossShardStatus::Aborted);
    }

    #[test]
    fn test_finality_proof_sufficient_stake() {
        let proof = FinalityProof {
            block_hash: hash_bytes(b"block"),
            round: 0,
            height: 1,
            quorum_signatures: vec![],
            signing_stake: 15_000_000,
            total_stake: 20_000_000,
        };
        assert!(
            proof.has_sufficient_stake(),
            "75% stake should be sufficient"
        );

        let insufficient = FinalityProof {
            signing_stake: 10_000_000,
            ..proof.clone()
        };
        assert!(
            !insufficient.has_sufficient_stake(),
            "50% stake should be insufficient"
        );

        let exact_two_thirds = FinalityProof {
            signing_stake: 20_000_000,
            total_stake: 30_000_000,
            ..proof.clone()
        };
        assert!(
            !exact_two_thirds.has_sufficient_stake(),
            "exactly 2/3 stake must not produce finality"
        );

        let five_of_six = FinalityProof {
            signing_stake: 25_000_000,
            ..exact_two_thirds
        };
        assert!(
            five_of_six.has_sufficient_stake(),
            "5/6 equal stake must produce finality"
        );

        let empty = FinalityProof {
            signing_stake: 0,
            total_stake: 0,
            ..proof
        };
        assert!(
            !empty.has_sufficient_stake(),
            "an empty validator set must not produce finality"
        );
    }

    #[test]
    fn test_finality_proof_export_fails_closed_without_canonical_signatures() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let block_b = make_block(test_addr(0), 0, vec![], vec![hash_bytes(b"tx_b")], 100);
        let b1 = make_block(test_addr(1), 0, vec![], vec![], 101);
        let b2 = make_block(test_addr(2), 0, vec![], vec![], 102);
        let b3 = make_block(test_addr(3), 0, vec![], vec![], 103);
        engine.receive_block(&block_b).unwrap();
        engine.receive_block(&b1).unwrap();
        engine.receive_block(&b2).unwrap();
        engine.receive_block(&b3).unwrap();
        engine.advance_round();

        let block_c = make_block(
            test_addr(1),
            1,
            vec![block_b.hash, b1.hash, b2.hash],
            vec![],
            200,
        );
        let c2 = make_block(
            test_addr(2),
            1,
            vec![block_b.hash, b1.hash, b2.hash],
            vec![],
            201,
        );
        let c3 = make_block(
            test_addr(3),
            1,
            vec![block_b.hash, b1.hash, b3.hash],
            vec![],
            202,
        );
        engine.receive_block(&block_c).unwrap();
        engine.receive_block(&c2).unwrap();
        engine.receive_block(&c3).unwrap();
        engine.advance_round();

        let d0 = make_block(
            test_addr(0),
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            300,
        );
        let d2 = make_block(
            test_addr(2),
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            301,
        );
        let d3 = make_block(
            test_addr(3),
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            302,
        );
        engine.receive_block(&d0).unwrap();
        engine.receive_block(&d2).unwrap();
        engine.receive_block(&d3).unwrap();

        let committed = engine.try_commit();
        assert!(!committed.is_empty());

        assert!(engine.generate_finality_proof(&block_b.hash, 1).is_none());
        assert!(engine.get_finality_proof(&block_b.hash).is_none());

        let validator = KeyPair::generate_ed25519();
        let d_block_signature = validator.sign(&d0.hash).unwrap();
        assert!(
            d_block_signature
                .verify(&block_b.hash, &validator.address())
                .is_err(),
            "a signature over D's hash must never be relabeled as B finality"
        );
    }

    #[test]
    fn test_dag_pruning_removes_old_rounds() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        for round in 0..=110u64 {
            for v in 0..4u8 {
                let parents = if round == 0 {
                    vec![]
                } else {
                    engine.blocks_in_round(round - 1)
                };
                let block =
                    make_block(test_addr(v), round, parents, vec![], round * 100 + v as u64);
                engine.dag.insert(block.hash, block.clone());
                engine.rounds.entry(round).or_default().push(block.hash);
                engine
                    .author_round_blocks
                    .insert((block.author, round), block.hash);
            }
        }

        let initial_dag_size = engine.dag_size();
        assert!(initial_dag_size > 0);

        assert_eq!(
            engine.prune_below_round(110),
            0,
            "with the default retention, 110 rounds of history are all still needed"
        );

        engine.set_retained_rounds(100);
        let pruned = engine.prune_below_round(110);
        assert!(pruned > 0, "should have pruned some blocks");

        for round in 0..10u64 {
            assert!(
                engine.blocks_in_round(round).is_empty(),
                "round {} should have been pruned",
                round
            );
        }

        assert!(
            !engine.blocks_in_round(10).is_empty(),
            "round 10 should still exist"
        );

        assert!(engine.dag_size() < initial_dag_size);
    }

    #[test]
    fn test_dag_pruning_preserves_recent() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        for round in 0..=50u64 {
            let block = make_block(
                test_addr(0),
                round,
                if round == 0 {
                    vec![]
                } else {
                    engine.blocks_in_round(round - 1)
                },
                vec![],
                round * 100,
            );
            engine.dag.insert(block.hash, block.clone());
            engine.rounds.entry(round).or_default().push(block.hash);
        }

        let initial_size = engine.dag_size();

        let pruned = engine.prune_below_round(50);
        assert_eq!(
            pruned, 0,
            "no rounds should be pruned when committed_round < PRUNE_DEPTH"
        );
        assert_eq!(engine.dag_size(), initial_size);

        for round in 0..=50u64 {
            assert!(
                !engine.blocks_in_round(round).is_empty(),
                "round {} should still exist",
                round
            );
        }
    }

    #[test]
    fn commit_prune_never_discards_rounds_at_or_above_the_commit_cursor() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        for round in 0..=2u64 {
            let parents = if round == 0 {
                Vec::new()
            } else {
                engine.blocks_in_round(round - 1)
            };
            for v in 0..4u8 {
                let block = make_block(
                    test_addr(v),
                    round,
                    parents.clone(),
                    vec![],
                    round * 10 + v as u64,
                );
                engine.receive_block(&block).expect("valid block");
            }
            if round < 2 {
                assert!(engine.advance_round());
            }
        }
        assert_eq!(engine.current_round(), 2);

        for _ in 0..=PRUNE_DEPTH {
            engine.force_advance_round();
        }
        assert!(engine.current_round() > PRUNE_DEPTH + 2);

        let committed = engine.try_commit();
        assert_eq!(committed.len(), 1, "round 0 leader must commit");
        assert_eq!(engine.last_committed_round(), 1);

        assert!(
            !engine.blocks_in_round(1).is_empty(),
            "round 1 is the commit cursor and must survive pruning"
        );
        assert!(
            !engine.blocks_in_round(2).is_empty(),
            "round 2 certifies round 1 and must survive pruning"
        );

        let round3_parents = engine.blocks_in_round(2);
        assert_eq!(round3_parents.len(), 4);
        for v in 0..4u8 {
            let block = make_block(
                test_addr(v),
                3,
                round3_parents.clone(),
                vec![],
                300 + v as u64,
            );
            engine.dag.insert(block.hash, block.clone());
            engine.rounds.entry(3).or_default().push(block.hash);
            engine
                .author_round_blocks
                .insert((block.author, 3), block.hash);
        }

        let committed = engine.try_commit();
        assert!(
            committed.iter().any(|block| block.round == 1),
            "round 1 must still be committable after pruning"
        );
    }

    #[test]
    fn security_detector_history_is_bounded_by_round_progress() {
        let vs = test_validator_set(1);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let rounds = SECURITY_HISTORY_DEPTH + 5 * PRUNE_DEPTH;
        for round in 0..=rounds {
            let parents = if round == 0 {
                Vec::new()
            } else {
                engine.blocks_in_round(round - 1)
            };
            let block = make_block(test_addr(0), round, parents, vec![], round);
            engine.receive_block(&block).expect("valid block");
            assert!(engine.advance_round());
        }

        let bound = (SECURITY_HISTORY_DEPTH + PRUNE_DEPTH) as usize;
        let retained_votes = engine.stake_tracker().lock().retained_vote_rounds();
        assert!(
            retained_votes <= bound,
            "stake tracker retained {retained_votes} vote rounds over {rounds} rounds (bound {bound})"
        );
        let retained_observations = engine.withholding_detector().lock().retained_observations();
        assert!(
            retained_observations <= bound,
            "withholding detector retained {retained_observations} observations over {rounds} rounds (bound {bound})"
        );

        assert!(
            retained_votes >= PRUNE_DEPTH as usize,
            "retention must not fall below the widest detector window"
        );
    }

    #[test]
    fn test_equivocation_detected_same_round() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let block1 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx_a")], 1000);
        engine.receive_block(&block1).unwrap();

        let block2 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx_b")], 1001);

        let proof = engine.detect_equivocation(&block2);
        assert!(
            proof.is_some(),
            "should detect equivocation for same author, same round"
        );

        let proof = proof.unwrap();
        assert_eq!(proof.author, test_addr(1));
        assert_eq!(proof.round, 0);
        assert_eq!(proof.block1_hash, block1.hash);
        assert_eq!(proof.block2_hash, block2.hash);
    }

    #[test]
    fn test_equivocation_not_triggered_different_rounds() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let block1 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx_a")], 1000);
        engine.receive_block(&block1).unwrap();

        let block2 = make_block(
            test_addr(1),
            1,
            vec![block1.hash],
            vec![hash_bytes(b"tx_b")],
            1001,
        );

        let proof = engine.detect_equivocation(&block2);
        assert!(
            proof.is_none(),
            "different rounds should not trigger equivocation"
        );
    }

    #[test]
    fn test_slash_reduces_stake() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let target = test_addr(1);
        let initial_stake = engine.validator_set().get_validator(&target).unwrap().stake;
        assert_eq!(initial_stake, STAKE_ARC);

        let removed = engine.enforce_slash(&target, 1_000_000);
        assert!(
            !removed,
            "validator should not be removed with 4M remaining"
        );

        let new_stake = engine.validator_set().get_validator(&target).unwrap().stake;
        assert_eq!(new_stake, STAKE_ARC - 1_000_000);
    }

    #[test]
    fn test_slash_removes_below_minimum() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let target = test_addr(1);
        assert!(engine.validator_set().is_validator(&target));

        let removed = engine.enforce_slash(&target, STAKE_ARC);
        assert!(
            removed,
            "validator should be removed when stake falls below STAKE_SPARK"
        );

        assert!(
            !engine.validator_set().is_validator(&target),
            "removed validator should not be in the set"
        );
        assert_eq!(
            engine.validator_set().len(),
            3,
            "validator set should have 3 remaining validators"
        );
    }

    #[test]
    fn test_cross_shard_lock_expiry() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"expiry_test_lock");
        engine
            .lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0)
            .unwrap();

        if let Some(mut entry) = engine.pending_cross_shard.get_mut(&tx) {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            entry.locked_at_ms = now_ms - 60_000;
        }

        let expired = engine.expire_stale_locks_with_timeout(30);
        assert_eq!(expired, 1, "stale lock should be expired with 30s timeout");
        assert_eq!(engine.cross_shard_stats(), (0, 1));

        let proof = engine.completed_cross_shard.get(&tx).unwrap();
        assert_eq!(proof.status, CrossShardStatus::Aborted);
    }

    #[test]
    fn test_cross_shard_lock_not_expired_if_recent() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"fresh_lock_test");
        engine
            .lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0)
            .unwrap();

        let expired = engine.expire_stale_locks_with_timeout(30);
        assert_eq!(expired, 0, "fresh lock should not be expired");
        assert_eq!(engine.cross_shard_stats(), (1, 0));
        assert!(engine.pending_cross_shard.contains_key(&tx));
    }

    #[test]
    fn test_security_modules_wired() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
            Validator::new(test_addr(2), STAKE_ARC, 1).unwrap(),
            Validator::new(test_addr(3), STAKE_ARC, 2).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let wd = engine.withholding_detector().lock();
        assert_eq!(wd.detect_withholding(100).len(), 0);
        drop(wd);

        let cr = engine.checkpoint_registry().lock();
        assert!(cr.is_empty());
        drop(cr);

        let st = engine.stake_tracker().lock();
        assert!(st.all_penalties().is_empty());
    }

    #[test]
    fn test_node_role_default() {
        let validators = vec![Validator::new(test_addr(1), STAKE_ARC, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));
        assert_eq!(engine.node_role(), NodeRole::Full);
    }

    #[test]
    fn test_node_role_setter() {
        let validators = vec![Validator::new(test_addr(1), STAKE_ARC, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let mut engine = ConsensusEngine::new(vs, test_addr(1));

        engine.set_node_role(NodeRole::Proposer);
        assert_eq!(engine.node_role(), NodeRole::Proposer);

        engine.set_node_role(NodeRole::Verifier);
        assert_eq!(engine.node_role(), NodeRole::Verifier);

        engine.set_node_role(NodeRole::Full);
        assert_eq!(engine.node_role(), NodeRole::Full);
    }

    #[test]
    fn fresh_boot_rejects_far_ahead_signed_block() {
        let local_key = KeyPair::generate_ed25519();
        let peer_key = KeyPair::generate_ed25519();
        let validators = vec![
            Validator::new(local_key.address(), STAKE_ARC, 0).unwrap(),
            Validator::new(peer_key.address(), STAKE_ARC, 1).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new_with_keypair(vs, local_key.address(), local_key);

        let mut block = make_block(
            peer_key.address(),
            u64::MAX,
            vec![],
            vec![],
            1_700_000_000_000,
        );
        block.signature = bincode::serialize(&peer_key.sign(&block.hash).unwrap()).unwrap();
        let result = engine.receive_block(&block);
        assert!(
            matches!(&result, Err(ConsensusError::RoundTooFarAhead { .. })),
            "fresh node must reject attacker-controlled round movement; got {:?}",
            result
        );
        assert_eq!(engine.current_round(), 0);
        assert_eq!(engine.last_committed_round(), 0);
    }

    #[test]
    fn established_node_rejects_block_beyond_max_jump() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
            Validator::new(test_addr(2), STAKE_ARC, 1).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));
        engine.restore_round_from_local_wal(100, 0);

        let block = make_block(test_addr(2), 10_000_000, vec![], vec![], 1_700_000_000_000);
        let result = engine.receive_block(&block);
        assert!(
            result.is_err(),
            "established node must reject too-far-ahead block; got {:?}",
            result
        );
        match result {
            Err(ConsensusError::RoundTooFarAhead { round, current }) => {
                assert!(round > current + 1, "round {round} vs current {current}");
            }
            other => panic!("expected RoundTooFarAhead, got {other:?}"),
        }
    }

    #[test]
    fn restore_round_from_local_wal_is_idempotent_when_lower() {
        let validators = vec![Validator::new(test_addr(1), STAKE_ARC, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));
        engine.restore_round_from_local_wal(500, 400);
        assert_eq!(engine.current_round(), 500);

        engine.restore_round_from_local_wal(100, 50);
        assert_eq!(
            engine.current_round(),
            500,
            "local WAL restoration must not rewind"
        );
    }

    #[test]
    fn untrusted_round_hint_never_mutates_consensus_cursors() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        engine.observe_untrusted_round_hint(u64::MAX, u64::MAX - 1);

        assert_eq!(engine.current_round(), 0);
        assert_eq!(engine.last_committed_round(), 0);
    }

    #[test]
    fn round_counter_fails_closed_at_u64_max() {
        let validators = vec![Validator::new(test_addr(1), STAKE_ARC, 0).unwrap()];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));
        engine.restore_round_from_local_wal(u64::MAX, 0);

        engine.force_advance_round();
        assert_eq!(engine.current_round(), u64::MAX);

        let block = make_block(test_addr(1), u64::MAX, vec![], vec![], 1000);
        engine.insert_block_into_dag(&block);
        assert!(!engine.advance_round());
        assert_eq!(engine.current_round(), u64::MAX);
    }

    #[test]
    fn test_fraud_proof_slashes_proposer() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
            Validator::new(test_addr(2), STAKE_ARC, 1).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let result = engine.submit_fraud_proof(
            Hash256([1; 32]),
            test_addr(2),
            Hash256([2; 32]),
            Hash256([3; 32]),
            10,
        );
        assert!(result.is_ok());

        let vs = engine.validator_set();
        let slashed = vs.get_validator(&test_addr(2));
        if let Some(v) = slashed {
            assert!(v.stake < STAKE_ARC);
        }
    }
}
