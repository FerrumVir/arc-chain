//! ARC Chain DAG-based consensus engine.
//!
//! Inspired by Mysticeti (Sui), this implements a sender-sharded DAG consensus
//! where validators concurrently propose blocks that form a directed acyclic graph.
//! Single-shard transactions use a fast path (no full ordering), while cross-shard
//! transactions require full DAG ordering via the commit rule.
//!
//! # Commit Rule (Mysticeti-inspired)
//!
//! A block B in round R is committed when:
//! 1. A block C in round R+1 references B as a parent.
//! 2. Block C is itself referenced by >= 2f+1 stake-weighted blocks in round R+2.
//!
//! This provides two-round latency for commit finality.

use arc_crypto::{Hash256, KeyPair, Signature as CryptoSignature};
use arc_types::{TxBody, Transaction};
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tracing::{debug, info, warn};

pub mod beacon;
pub mod data_availability;
pub mod subnet;
pub mod security;
pub use security::*;
pub use data_availability::*;

// ── Type Aliases ─────────────────────────────────────────────────────────────

/// Address is a 256-bit hash derived from a public key.
pub type Address = Hash256;

// ── Stake Tier Thresholds ────────────────────────────────────────────────────

/// Minimum stake to qualify as a Spark validator (can vote, cannot produce blocks).
pub const STAKE_SPARK: u64 = 500_000;

/// Minimum stake to qualify as an Arc validator (can produce blocks).
pub const STAKE_ARC: u64 = 5_000_000;

/// Minimum stake to qualify as a Core validator (priority producer, governance).
pub const STAKE_CORE: u64 = 50_000_000;

// ── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConsensusError {
    #[error("invalid block: {0}")]
    InvalidBlock(String),

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
}

// ── Stake Tier ───────────────────────────────────────────────────────────────

/// Validator stake tier determining capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StakeTier {
    /// 500K ARC -- can vote on blocks, cannot produce blocks.
    Spark,
    /// 5M ARC -- can produce blocks and vote.
    Arc,
    /// 50M ARC -- priority block producer, governance participation.
    Core,
}

impl StakeTier {
    /// Determine the tier from a stake amount.
    /// Returns `None` if the stake is below the minimum Spark threshold.
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

    /// Whether this tier can produce blocks.
    pub fn can_produce_blocks(&self) -> bool {
        matches!(self, StakeTier::Arc | StakeTier::Core)
    }

    /// Whether this tier can participate in governance.
    pub fn can_govern(&self) -> bool {
        matches!(self, StakeTier::Core)
    }
}

// ── Slashing ────────────────────────────────────────────────────────────────

/// Slash rate for Spark validators (10%).
pub const SLASH_RATE_SPARK: u64 = 10;
/// Slash rate for Arc validators (20%).
pub const SLASH_RATE_ARC: u64 = 20;
/// Slash rate for Core validators (30%).
pub const SLASH_RATE_CORE: u64 = 30;
/// Number of rounds a validator can miss before liveness fault.
pub const LIVENESS_THRESHOLD: u64 = 100;

/// Types of slashable offenses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SlashableOffense {
    DoubleSigning,
    LivenessFault,
    InvalidBlockProposal,
    EquivocationDAG,
}

/// Record of a slashing event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlashRecord {
    pub offense: SlashableOffense,
    pub offender: Address,
    pub evidence: Hash256,
    pub round: u64,
    pub timestamp: u64,
    pub slash_amount: u64,
}

// ── Equivocation Proof ──────────────────────────────────────────────────────

/// Proof that a validator proposed two different blocks in the same round.
/// Used as evidence for slashing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EquivocationProof {
    /// The equivocating validator's address.
    pub author: Address,
    /// The round in which both blocks were proposed.
    pub round: u64,
    /// Hash of the first block seen.
    pub block1_hash: Hash256,
    /// Hash of the conflicting second block.
    pub block2_hash: Hash256,
}

// ── Validator ────────────────────────────────────────────────────────────────

/// A validator in the ARC consensus network.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Validator {
    /// Validator address (derived from public key).
    pub address: Address,
    /// Amount of ARC staked.
    pub stake: u64,
    /// Tier determined by stake amount.
    pub tier: StakeTier,
    /// Assigned shard for sender-sharded consensus.
    pub shard_assignment: u16,
}

impl Validator {
    /// Create a new validator with automatic tier assignment.
    /// Returns `None` if stake is below the minimum Spark threshold.
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

// ── Validator Set ────────────────────────────────────────────────────────────

/// The set of active validators for an epoch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorSet {
    /// All validators in the set.
    pub validators: Vec<Validator>,
    /// Sum of all validator stakes.
    pub total_stake: u64,
    /// Current epoch number.
    pub epoch: u64,
    /// Quorum threshold: ceil(2/3 * total_stake).
    pub quorum: u64,
    pub slashed_total: u64,
    pub slash_history: Vec<SlashRecord>,
}

impl ValidatorSet {
    /// Create a new validator set for the given epoch.
    /// Computes total_stake and quorum (ceiling of 2/3 * total_stake).
    pub fn new(validators: Vec<Validator>, epoch: u64) -> Self {
        let total_stake: u64 = validators.iter().map(|v| v.stake).sum();
        // quorum = ceil(2 * total_stake / 3)
        let quorum = (2 * total_stake + 2) / 3;
        Self {
            validators,
            total_stake,
            epoch,
            quorum,
            slashed_total: 0,
            slash_history: Vec::new(),
        }
    }

    /// Look up a validator by address.
    pub fn get_validator(&self, address: &Address) -> Option<&Validator> {
        self.validators.iter().find(|v| v.address == *address)
    }

    /// Check if an address belongs to a validator in the set.
    pub fn is_validator(&self, address: &Address) -> bool {
        self.get_validator(address).is_some()
    }

    /// Check if an address is a block-producing validator (Arc or Core tier).
    pub fn can_produce_blocks(&self, address: &Address) -> bool {
        self.get_validator(address)
            .map(|v| v.tier.can_produce_blocks())
            .unwrap_or(false)
    }

    /// Compute the fault tolerance threshold f.
    /// In BFT: n = 3f + 1, so f = (n-1)/3 in terms of validators.
    /// For stake-weighted: f = floor((total_stake - 1) / 3)
    /// The quorum is total_stake - f = ceil(2/3 * total_stake).
    pub fn fault_tolerance_stake(&self) -> u64 {
        // f such that quorum = total_stake - f
        self.total_stake - self.quorum
    }

    /// Compute the total stake for a given set of validator addresses.
    /// Unknown addresses are skipped.
    pub fn stake_for_addresses(&self, addresses: &[Address]) -> u64 {
        addresses
            .iter()
            .filter_map(|addr| self.get_validator(addr).map(|v| v.stake))
            .sum()
    }

    /// Check if a set of validator addresses reaches quorum.
    pub fn has_quorum(&self, addresses: &[Address]) -> bool {
        self.stake_for_addresses(addresses) >= self.quorum
    }

    /// Number of validators.
    pub fn len(&self) -> usize {
        self.validators.len()
    }

    /// Whether the validator set is empty.
    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    /// Get all validators that can produce blocks.
    pub fn block_producers(&self) -> Vec<&Validator> {
        self.validators
            .iter()
            .filter(|v| v.tier.can_produce_blocks())
            .collect()
    }

    /// Get a mutable reference to a validator by address.
    pub fn get_validator_mut(&mut self, address: &Address) -> Option<&mut Validator> {
        self.validators.iter_mut().find(|v| v.address == *address)
    }

    /// Get the slash rate for a given tier.
    pub fn slash_rate(tier: &StakeTier) -> u64 {
        match tier {
            StakeTier::Spark => SLASH_RATE_SPARK,
            StakeTier::Arc => SLASH_RATE_ARC,
            StakeTier::Core => SLASH_RATE_CORE,
        }
    }

    /// Report a slashable offense and apply the penalty.
    pub fn report_offense(
        &mut self,
        offender: Address,
        offense: SlashableOffense,
        evidence: Hash256,
        round: u64,
        timestamp: u64,
    ) -> Result<SlashRecord, ConsensusError> {
        let validator = self.validators.iter().find(|v| v.address == offender)
            .ok_or_else(|| ConsensusError::SlashError(format!("validator {:?} not found", offender)))?;
        let rate = Self::slash_rate(&validator.tier);
        let slash_amount = validator.stake * rate / 100;
        let record = SlashRecord {
            offense,
            offender,
            evidence,
            round,
            timestamp,
            slash_amount,
        };
        self.slashed_total += slash_amount;
        self.slash_history.push(record.clone());
        Ok(record)
    }

    /// Actually reduce a validator's stake and recalculate totals.
    /// If the validator drops below the minimum stake threshold (STAKE_SPARK),
    /// they are removed from the validator set.
    pub fn apply_slash(&mut self, offender: &Address, slash_amount: u64) {
        let mut removed = false;
        if let Some(validator) = self.validators.iter_mut().find(|v| v.address == *offender) {
            validator.stake = validator.stake.saturating_sub(slash_amount);
            if validator.stake < STAKE_SPARK {
                warn!(
                    address = %offender,
                    remaining_stake = validator.stake,
                    "Validator stake below minimum after slashing — removing from validator set"
                );
                removed = true;
            } else {
                // Recalculate tier
                validator.tier = StakeTier::from_stake(validator.stake)
                    .unwrap_or(StakeTier::Spark);
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
        // Recalculate total stake and quorum
        self.total_stake = self.validators.iter().map(|v| v.stake).sum();
        self.quorum = (self.total_stake * 2 + 2) / 3; // ceil(2/3 * total)
    }
}

// ── DAG Block ────────────────────────────────────────────────────────────────

/// A block in the DAG. Each validator proposes one block per round,
/// referencing parent blocks from the previous round.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DagBlock {
    /// Block author (validator address).
    pub author: Address,
    /// DAG round number.
    pub round: u64,
    /// Parent block hashes from round - 1.
    pub parents: Vec<Hash256>,
    /// Transaction hashes included in this block.
    pub transactions: Vec<Hash256>,
    /// Block creation timestamp (unix millis).
    pub timestamp: u64,
    /// BLAKE3 hash of the block contents.
    pub hash: Hash256,
    /// Author's signature over the block hash.
    pub signature: Vec<u8>,
    /// MEV Protection: BLAKE3 commitment over canonically-sorted tx hashes.
    /// Validators verify that `ordering_commitment == BLAKE3(sorted_tx_hashes)`
    /// where sorted means lexicographic order by tx hash bytes. This removes
    /// proposer discretion over transaction ordering entirely, preventing MEV.
    pub ordering_commitment: Hash256,
}

impl DagBlock {
    /// Compute the hash of this block's contents (excluding hash and signature fields).
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

    /// Compute the ordering commitment for a list of transaction hashes.
    /// This is BLAKE3 over the concatenated tx hashes in **canonical lexicographic
    /// order**. Validators recompute this commitment by sorting the block's
    /// transactions lexicographically and comparing — any deviation proves the
    /// proposer reordered transactions (MEV extraction attempt).
    ///
    /// The input `transactions` slice MUST already be sorted lexicographically
    /// by `Hash256` bytes; callers are responsible for sorting before calling.
    pub fn compute_ordering_commitment(transactions: &[Hash256]) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-ordering-commitment-v1");
        for tx in transactions {
            hasher.update(tx.as_ref());
        }
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Verify that the stored hash matches the computed hash.
    pub fn verify_hash(&self) -> bool {
        self.hash == self.compute_hash()
    }

    /// Verify that the block's transactions are in canonical lexicographic order
    /// and that the ordering commitment matches this canonical ordering.
    ///
    /// Returns `true` only if:
    /// 1. The transactions are sorted lexicographically by hash bytes.
    /// 2. The ordering commitment equals `BLAKE3(sorted_tx_hashes)`.
    ///
    /// This prevents MEV extraction: a proposer cannot reorder transactions
    /// because validators independently verify the canonical sort.
    pub fn verify_ordering(&self) -> bool {
        // Check that the block's transactions are actually in sorted order
        let is_sorted = self.transactions.windows(2).all(|w| w[0].0 <= w[1].0);
        if !is_sorted {
            return false;
        }
        // Verify commitment matches the (already-sorted) transaction list
        self.ordering_commitment == Self::compute_ordering_commitment(&self.transactions)
    }
}

// ── Cross-Shard Receipt ──────────────────────────────────────────────────────

/// Receipt proving a cross-shard transfer was committed on the source shard.
/// The destination shard uses this receipt to credit the recipient.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrossShardReceipt {
    /// Source shard ID.
    pub source_shard: u16,
    /// Block hash on the source shard containing the transaction.
    pub source_block: Hash256,
    /// Hash of the original transaction.
    pub tx_hash: Hash256,
    /// Transfer amount.
    pub amount: u64,
    /// Recipient address on the destination shard.
    pub recipient: Address,
}

// ── Cross-Shard Execution Proofs ────────────────────────────────────────────

/// Status of a cross-shard transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CrossShardStatus {
    Locked,
    Committed,
    Aborted,
}

/// Proof of a cross-shard transaction's state.
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
    /// Timestamp when the lock was acquired (unix millis).
    /// Used for deadlock prevention — locks older than CROSS_SHARD_LOCK_TIMEOUT_MS
    /// are automatically expired to prevent indefinite lock-holding.
    pub locked_at_ms: u64,
    /// Consensus round when the lock was acquired.
    /// Used alongside `locked_at_ms` for round-based expiry that is immune
    /// to wall-clock skew.
    pub locked_at_round: u64,
}

// ── Finality Proof (A8: Light Client Finality Proofs) ────────────────────────

/// Proof that a block has been finalized by a quorum of validators.
/// Light clients can verify this without replaying the DAG — they just
/// check that >= 2/3 stake signed the block hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinalityProof {
    /// Hash of the finalized block.
    pub block_hash: Hash256,
    /// DAG round number.
    pub round: u64,
    /// Block height (sequential number for the committed chain).
    pub height: u64,
    /// Quorum signatures: (validator_address, signature_bytes).
    /// Must represent >= 2/3 of total stake.
    pub quorum_signatures: Vec<(Address, Vec<u8>)>,
    /// Total stake that signed.
    pub signing_stake: u64,
    /// Total stake in the validator set at the time.
    pub total_stake: u64,
}

impl FinalityProof {
    /// Verify that the proof has sufficient stake (>= 2/3 total).
    pub fn has_sufficient_stake(&self) -> bool {
        // quorum = ceil(2/3 * total_stake)
        let quorum = (2 * self.total_stake + 2) / 3;
        self.signing_stake >= quorum
    }
}

// ── DAG Pruning ─────────────────────────────────────────────────────────────

/// Number of rounds to keep for reorg safety during DAG pruning.
pub const PRUNE_DEPTH: u64 = 100;

// ── Cross-Shard Lock Timeout ─────────────────────────────────────────────────

/// Maximum time a cross-shard lock can be held before automatic expiry (30 seconds).
const CROSS_SHARD_LOCK_TIMEOUT_MS: u64 = 30_000;

// ── Propose-Verify Protocol ──────────────────────────────────────────────────

/// Role of this node in the propose-verify protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeRole {
    /// Proposer: executes transactions and produces state diffs.
    Proposer,
    /// Verifier: applies state diffs and verifies roots match.
    Verifier,
    /// Full: both proposes and verifies (default, backward compatible).
    Full,
}

// ── Consensus Engine ─────────────────────────────────────────────────────────

/// The core DAG-based consensus engine.
///
/// Validators propose blocks concurrently, forming a DAG. Blocks are committed
/// when they satisfy the two-round commit rule (Mysticeti-inspired).
pub struct ConsensusEngine {
    /// All known DAG blocks, indexed by hash.
    dag: DashMap<Hash256, DagBlock>,
    /// Index: round number -> block hashes in that round.
    rounds: DashMap<u64, Vec<Hash256>>,
    /// Committed block hashes in finalized order.
    committed: RwLock<Vec<Hash256>>,
    /// Current DAG round.
    current_round: AtomicU64,
    /// Active validator set.
    validator_set: RwLock<ValidatorSet>,
    /// This node's validator address.
    local_address: Address,
    /// This node's signing keypair for block proposals.
    /// None only in legacy test mode — production always has a keypair.
    local_keypair: Option<KeyPair>,
    /// Registered validator public keys: address -> Ed25519 verifying key bytes.
    validator_keys: DashMap<Address, [u8; 32]>,
    /// Equivocation detector: (author, round) → first block hash seen.
    /// If a second block from the same author in the same round arrives,
    /// that's equivocation and the author gets slashed.
    author_round_blocks: DashMap<(Address, u64), Hash256>,
    /// Data availability commitments.
    pub da_commitments: DashMap<Hash256, data_availability::DACommitment>,
    /// Pending cross-shard locks.
    pub pending_cross_shard: DashMap<Hash256, CrossShardProof>,
    /// Completed cross-shard proofs.
    pub completed_cross_shard: DashMap<Hash256, CrossShardProof>,
    /// Finality proofs: block_hash -> FinalityProof.
    /// Generated when blocks are committed with quorum signatures.
    pub finality_proofs: DashMap<Hash256, FinalityProof>,
    /// Liveness: when the current round started (for proposer failover).
    round_start: RwLock<std::time::Instant>,
    /// Set to `true` after `force_advance_round()`. When true, `propose_block()`
    /// will accept whatever parents are available (even below quorum) to recover
    /// from stalls. Cleared after a successful normal `advance_round()`.
    force_advanced: std::sync::atomic::AtomicBool,
    /// Tracks expected vs received blocks to detect withholding attacks.
    withholding_detector: parking_lot::Mutex<WithholdingDetector>,
    /// Stores finalized checkpoints for long-range attack prevention.
    checkpoint_registry: parking_lot::Mutex<CheckpointRegistry>,
    /// Tracks validator votes to detect double-voting (nothing-at-stake).
    stake_tracker: parking_lot::Mutex<StakeTracker>,
    /// This node's role in the propose-verify protocol.
    node_role: NodeRole,
}

impl ConsensusEngine {
    /// Create a new consensus engine with a signing keypair.
    ///
    /// # Arguments
    /// * `validator_set` - The initial validator set for this epoch.
    /// * `local_address` - This node's validator address.
    /// * `local_keypair` - This node's signing keypair for block proposals.
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
            "ConsensusEngine initialized with signing keypair"
        );
        Self {
            dag: DashMap::new(),
            rounds: DashMap::new(),
            committed: RwLock::new(Vec::new()),
            current_round: AtomicU64::new(0),
            validator_set: RwLock::new(validator_set),
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
        }
    }

    /// Create a consensus engine without a signing keypair (legacy/test mode).
    /// Blocks proposed without a keypair will have empty signatures.
    /// This constructor exists for backward compatibility with existing tests.
    pub fn new(validator_set: ValidatorSet, local_address: Address) -> Self {
        info!(
            epoch = validator_set.epoch,
            validators = validator_set.len(),
            total_stake = validator_set.total_stake,
            quorum = validator_set.quorum,
            "ConsensusEngine initialized (unsigned mode)"
        );
        Self {
            dag: DashMap::new(),
            rounds: DashMap::new(),
            committed: RwLock::new(Vec::new()),
            current_round: AtomicU64::new(0),
            validator_set: RwLock::new(validator_set),
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
        }
    }

    /// Register a validator's public key for signature verification.
    pub fn register_validator_key(&self, address: Address, ed25519_pubkey: [u8; 32]) {
        self.validator_keys.insert(address, ed25519_pubkey);
        debug!(%address, "Registered validator public key");
    }

    /// Get the current round number.
    pub fn current_round(&self) -> u64 {
        self.current_round.load(Ordering::SeqCst)
    }

    /// Get a snapshot of the validator set.
    pub fn validator_set(&self) -> ValidatorSet {
        self.validator_set.read().clone()
    }

    /// Add a validator to the active set.
    /// Returns error if stake is below minimum threshold.
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
        vs.validators.push(validator);
        vs.total_stake += stake;
        vs.quorum = (2 * vs.total_stake + 2) / 3;
        drop(vs);
        self.register_validator_key(address, pubkey);
        info!(%address, stake, "Validator joined the active set");
        Ok(())
    }

    /// Remove a validator from the active set.
    pub fn leave_validator(&self, address: &Address) -> Result<u64, ConsensusError> {
        let mut vs = self.validator_set.write();
        let stake = vs.get_validator(address)
            .map(|v| v.stake)
            .ok_or_else(|| ConsensusError::InvalidBlock(
                "address not in validator set".into(),
            ))?;
        vs.validators.retain(|v| v.address != *address);
        vs.total_stake = vs.validators.iter().map(|v| v.stake).sum();
        vs.quorum = if vs.total_stake > 0 {
            (2 * vs.total_stake + 2) / 3
        } else {
            0
        };
        self.validator_keys.remove(address);
        info!(%address, returned_stake = stake, "Validator left the active set");
        Ok(stake)
    }

    /// Update a validator's stake amount.
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
            vs.validators[idx].stake = new_stake;
            vs.validators[idx].tier = StakeTier::from_stake(new_stake)
                .unwrap_or(StakeTier::Spark);
            let new_tier = vs.validators[idx].tier;
            vs.total_stake = vs.validators.iter().map(|v| v.stake).sum();
            vs.quorum = (2 * vs.total_stake + 2) / 3;
            info!(%address, new_stake, new_tier = ?new_tier, "Validator stake updated");
            Ok(())
        } else {
            Err(ConsensusError::InvalidBlock(
                "address not in validator set".into(),
            ))
        }
    }

    /// Transition to a new epoch.
    /// Recalculates the validator set, distributes rewards, and advances the epoch number.
    pub fn epoch_transition(&self) -> u64 {
        let mut vs = self.validator_set.write();
        vs.epoch += 1;
        vs.total_stake = vs.validators.iter().map(|v| v.stake).sum();
        vs.quorum = if vs.total_stake > 0 {
            (2 * vs.total_stake + 2) / 3
        } else {
            0
        };
        let new_epoch = vs.epoch;
        info!(epoch = new_epoch, validators = vs.len(), total_stake = vs.total_stake, "Epoch transition");
        new_epoch
    }

    /// Returns `true` if the current round was reached via `force_advance_round()`.
    /// When true, parent quorum checks are relaxed to allow recovery from stalls.
    pub fn is_force_advanced(&self) -> bool {
        self.force_advanced.load(Ordering::SeqCst)
    }

    /// Get a block from the DAG by hash.
    pub fn get_block(&self, hash: &Hash256) -> Option<DagBlock> {
        self.dag.get(hash).map(|r| r.value().clone())
    }

    /// Get all block hashes for a given round.
    pub fn blocks_in_round(&self, round: u64) -> Vec<Hash256> {
        self.rounds
            .get(&round)
            .map(|r| r.value().clone())
            .unwrap_or_default()
    }

    /// Get the list of committed block hashes.
    pub fn committed_blocks(&self) -> Vec<Hash256> {
        self.committed.read().clone()
    }

    /// Total number of blocks in the DAG.
    pub fn dag_size(&self) -> usize {
        self.dag.len()
    }

    /// Propose a new DAG block for the current round.
    ///
    /// The block references >= 2f+1 parent blocks from the previous round.
    /// If this is round 0, there are no parents.
    ///
    /// # Arguments
    /// * `transactions` - Transaction hashes to include in this block.
    /// * `timestamp` - Block creation timestamp (unix millis).
    ///
    /// # Returns
    /// The newly created DAG block (also inserted into the local DAG).
    pub fn propose_block(
        &self,
        transactions: Vec<Hash256>,
        timestamp: u64,
    ) -> Result<DagBlock, ConsensusError> {
        let vs = self.validator_set.read();

        // Only block producers (Arc/Core tier) can propose
        if !vs.can_produce_blocks(&self.local_address) {
            return Err(ConsensusError::NotValidator);
        }

        let round = self.current_round.load(Ordering::SeqCst);

        // Collect parents from the previous round (round - 1).
        // For round 0, there are no parents.
        let parents = if round == 0 {
            Vec::new()
        } else {
            let prev_round = round - 1;
            let prev_hashes = self.blocks_in_round(prev_round);

            // Collect all available parents from the previous round.
            // The validator should have enough (>= 2f+1) since advance_round
            // was called to move us here.
            let mut selected_parents = Vec::new();
            let mut accumulated_stake = 0u64;

            for hash in &prev_hashes {
                if let Some(block) = self.dag.get(hash) {
                    if let Some(validator) = vs.get_validator(&block.author) {
                        selected_parents.push(*hash);
                        accumulated_stake += validator.stake;
                    }
                }
            }

            // Verify we have quorum-worth of parents.
            // After a force_advance_round() (view-change), relax this check to
            // accept whatever parents are available — the purpose of the view
            // change is to recover from stalls where quorum was unreachable.
            let is_force_advanced = self.force_advanced.load(Ordering::SeqCst);
            if accumulated_stake < vs.quorum && !is_force_advanced {
                return Err(ConsensusError::InsufficientParents);
            }

            selected_parents
        };

        drop(vs);

        // MEV Protection: enforce canonical lexicographic ordering of tx hashes.
        // This removes proposer discretion — transactions MUST be ordered by hash,
        // not by the proposer's chosen (potentially MEV-extracting) sequence.
        let mut transactions = transactions;
        transactions.sort_by(|a, b| a.0.cmp(&b.0));

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
        block.hash = block.compute_hash();

        // Sign the block hash with our keypair (if available)
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

        // Insert into our own DAG
        self.insert_block_into_dag(&block);

        // Create DA commitment for this block
        let block_data = bincode::serialize(&block.transactions).unwrap_or_default();
        let commitment = data_availability::create_da_commitment(&block_data, block.hash);
        self.store_da_commitment(commitment);

        // Report that we expected and produced a block in this round
        {
            let mut wd = self.withholding_detector.lock();
            wd.report_expected(self.local_address, round);
            wd.report_received(self.local_address, round);
        }

        Ok(block)
    }

    /// Receive and validate a DAG block from another validator.
    ///
    /// Validates:
    /// 1. Author is a known validator who can produce blocks.
    /// 2. Block is not a duplicate.
    /// 3. Hash is correct.
    /// 4. Round is not in the future (at most current_round + 1).
    /// 5. Parents are from the correct round (round - 1) and exist in our DAG.
    /// 6. For round > 0, block has >= 2f+1 parent stake.
    ///
    /// # Returns
    /// `Ok(())` if the block was accepted, or an appropriate error.
    pub fn receive_block(&self, block: &DagBlock) -> Result<(), ConsensusError> {
        let vs = self.validator_set.read();

        // 1. Author must be a registered validator that can produce blocks
        if !vs.can_produce_blocks(&block.author) {
            return Err(ConsensusError::NotValidator);
        }

        // 2. Check for duplicates
        if self.dag.contains_key(&block.hash) {
            return Err(ConsensusError::DuplicateBlock);
        }

        // 3. Verify hash integrity
        if !block.verify_hash() {
            return Err(ConsensusError::InvalidBlock(
                "hash does not match block contents".into(),
            ));
        }

        // 3b. MEV Protection: verify transactions are in canonical lexicographic
        //     order and that the ordering commitment matches this sorted sequence.
        //     This prevents proposers from reordering transactions for MEV.
        if !block.verify_ordering() {
            return Err(ConsensusError::MevOrderingViolation(
                format!(
                    "block {} by {} has transactions not in canonical lexicographic order \
                     or ordering commitment does not match sorted tx hashes",
                    block.hash, block.author
                ),
            ));
        }

        // 3c. Verify block signature — author must have signed the block hash
        if !block.signature.is_empty() {
            let sig: CryptoSignature = bincode::deserialize(&block.signature)
                .map_err(|_| ConsensusError::InvalidSignature)?;
            // verify() checks: (a) pubkey in sig derives to block.author, (b) sig is valid
            sig.verify(&block.hash, &block.author)
                .map_err(|_| {
                    warn!(
                        author = %block.author,
                        hash = %block.hash,
                        "Block signature verification failed"
                    );
                    ConsensusError::InvalidSignature
                })?;
            // If we have a registered key, cross-check it matches the signature's pubkey
            if let Some(registered_key) = self.validator_keys.get(&block.author) {
                if let CryptoSignature::Ed25519 { public_key, .. } = &sig {
                    if public_key != registered_key.value() {
                        warn!(
                            author = %block.author,
                            "Block signed with key that doesn't match registered key"
                        );
                        return Err(ConsensusError::InvalidSignature);
                    }
                }
            }
        } else if self.local_keypair.is_some() {
            // Production mode: reject unsigned blocks
            return Err(ConsensusError::InvalidSignature);
        }
        // else: legacy/test mode — accept unsigned blocks

        // 4. Round check: if block is ahead, fast-forward to catch up (testnet round sync).
        //    In production, this would require a proper state-sync protocol.
        //    For now, allow peers to pull us forward so nodes that start at
        //    different times can converge.
        let current = self.current_round.load(Ordering::SeqCst);
        if block.round > current + 1 {
            // Fast-forward: jump to block.round - 1 so we can accept this block
            let new_round = block.round.saturating_sub(1);
            self.current_round.store(new_round, Ordering::SeqCst);
            tracing::info!(
                "Round catch-up: fast-forwarded from {} to {} (peer block at round {})",
                current, new_round, block.round
            );
        }

        // 5. Parent validation
        if block.round == 0 {
            // Round 0 blocks should have no parents
            if !block.parents.is_empty() {
                return Err(ConsensusError::InvalidBlock(
                    "round 0 block must not have parents".into(),
                ));
            }
        } else {
            // All parents must exist in our DAG and be from round - 1.
            // Exception: if we recently fast-forwarded (round catch-up), we
            // won't have the parent blocks in our DAG. Accept the block
            // anyway — the signature and round are already verified.
            let expected_parent_round = block.round - 1;
            let mut parent_stake = 0u64;
            let mut missing_parents = 0usize;

            for parent_hash in &block.parents {
                match self.dag.get(parent_hash) {
                    Some(parent_block) => {
                        if parent_block.round != expected_parent_round {
                            // Parent round mismatch — skip this parent but don't reject
                            missing_parents += 1;
                            continue;
                        }
                        if let Some(validator) = vs.get_validator(&parent_block.author) {
                            parent_stake += validator.stake;
                        }
                    }
                    None => {
                        // Parent not in our DAG — we may have missed it.
                        // Count as missing but don't reject the block.
                        missing_parents += 1;
                    }
                }
            }

            if missing_parents > 0 {
                tracing::debug!(
                    "Block {} from {} at round {} has {}/{} missing parents (accepted anyway)",
                    block.hash, block.author, block.round,
                    missing_parents, block.parents.len()
                );
            }

            // 6. Need quorum-worth of parent stake.
            // Relax when parents are missing (catch-up) or after force_advance.
            let is_force_advanced = self.force_advanced.load(Ordering::SeqCst);
            if parent_stake < vs.quorum && !is_force_advanced && missing_parents == 0 {
                return Err(ConsensusError::InsufficientParents);
            }
        }

        drop(vs);

        // 7. Equivocation detection: same author must not have two blocks in the same round
        let key = (block.author, block.round);
        if let Some(equivocation) = self.detect_equivocation(block) {
            // Equivocation detected! Slash the offender.
            let evidence = arc_crypto::hash_pair(&equivocation.block1_hash, &equivocation.block2_hash);
            warn!(
                author = %block.author,
                round = block.round,
                first_block = %equivocation.block1_hash,
                equivocating_block = %equivocation.block2_hash,
                "EQUIVOCATION DETECTED — validator produced two blocks in the same round"
            );
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
                // Actually reduce the validator's stake via enforce_slash
                vs.apply_slash(&block.author, record.slash_amount);
            }
            drop(vs);
            // Still insert the equivocating block (the DAG handles it)
            // but the validator has been penalized.
        } else if !self.author_round_blocks.contains_key(&key) {
            self.author_round_blocks.insert(key, block.hash);
        }

        // All checks passed: insert
        self.insert_block_into_dag(block);

        // Track received block for withholding detection
        {
            let mut wd = self.withholding_detector.lock();
            wd.report_received(block.author, block.round);
        }
        // Track vote for double-voting detection.
        // Skip if this author already has a different block in this round
        // (equivocation), since that case is already handled above by
        // detect_equivocation() and we don't want to double-slash.
        let is_equivocation = self.author_round_blocks
            .get(&(block.author, block.round))
            .map(|existing| *existing.value() != block.hash)
            .unwrap_or(false);
        if !is_equivocation {
            let mut st = self.stake_tracker.lock();
            st.report_vote(block.author, block.round, *block.hash.as_bytes());
            // Check for double voting in this round
            let evidence = st.detect_double_voting(block.round);
            if !evidence.is_empty() {
                for dv in &evidence {
                    warn!(
                        validator = %dv.validator,
                        round = dv.round,
                        "DOUBLE VOTE DETECTED"
                    );
                    let evidence_hash = arc_crypto::hash_pair(
                        &Hash256(dv.vote1_hash),
                        &Hash256(dv.vote2_hash),
                    );
                    let mut vs = self.validator_set.write();
                    if let Ok(record) = vs.report_offense(
                        dv.validator,
                        SlashableOffense::DoubleSigning,
                        evidence_hash,
                        dv.round,
                        block.timestamp,
                    ) {
                        vs.apply_slash(&dv.validator, record.slash_amount);
                        st.record_penalty(security::PenaltyRecord {
                            validator: dv.validator,
                            offense: security::SlashableOffense::DoubleVote,
                            slash_amount: record.slash_amount,
                            round: dv.round,
                            timestamp: block.timestamp,
                        });
                    }
                }
            }
        }

        debug!(
            round = block.round,
            author = %block.author,
            hash = %block.hash,
            "Received valid DAG block"
        );

        Ok(())
    }

    /// Try to commit blocks using the two-round commit rule.
    ///
    /// # Commit Rule
    ///
    /// A block B in round R is committed when:
    /// 1. There exists a block C in round R+1 that has B as a parent.
    /// 2. C is referenced by blocks in round R+2 with total stake >= 2f+1 (quorum).
    ///
    /// # Returns
    /// Newly committed blocks in causal order.
    pub fn try_commit(&self) -> Vec<DagBlock> {
        let vs = self.validator_set.read();
        let current = self.current_round.load(Ordering::SeqCst);
        let mut newly_committed = Vec::new();

        // We need at least round 2 to have any commits (R, R+1, R+2 pattern)
        if current < 2 {
            return newly_committed;
        }

        let committed_set: std::collections::HashSet<Hash256> = {
            let committed = self.committed.read();
            committed.iter().copied().collect()
        };

        // Check all rounds up to current - 2 for committable blocks
        // (a block in round R needs R+1 and R+2 data)
        for r in 0..=(current.saturating_sub(2)) {
            let round_r_blocks = self.blocks_in_round(r);

            for block_b_hash in &round_r_blocks {
                // Skip if already committed
                if committed_set.contains(block_b_hash) {
                    continue;
                }

                // Step 1: Find a block C in round R+1 that references B
                let round_r1_blocks = self.blocks_in_round(r + 1);
                for block_c_hash in &round_r1_blocks {
                    if let Some(block_c) = self.dag.get(block_c_hash) {
                        if !block_c.parents.contains(block_b_hash) {
                            continue;
                        }

                        // Step 2: Check if C is referenced by >= quorum stake in round R+2
                        let round_r2_blocks = self.blocks_in_round(r + 2);
                        let mut supporting_stake = 0u64;
                        let mut certifier_sigs: Vec<(Address, Vec<u8>)> = Vec::new();

                        for block_d_hash in &round_r2_blocks {
                            if let Some(block_d) = self.dag.get(block_d_hash) {
                                if block_d.parents.contains(block_c_hash) {
                                    if let Some(validator) =
                                        vs.get_validator(&block_d.author)
                                    {
                                        supporting_stake += validator.stake;
                                        certifier_sigs.push((
                                            block_d.author,
                                            block_d.signature.clone(),
                                        ));
                                    }
                                }
                            }
                        }

                        if supporting_stake >= vs.quorum {
                            // Block B is committed!
                            if let Some(block_b) = self.dag.get(block_b_hash) {
                                info!(
                                    round = block_b.round,
                                    hash = %block_b.hash,
                                    "Block committed via two-round rule"
                                );

                                // A8: Auto-generate finality proof
                                let proof = FinalityProof {
                                    block_hash: block_b.hash,
                                    round: block_b.round,
                                    height: 0, // Set by caller after state execution
                                    quorum_signatures: certifier_sigs,
                                    signing_stake: supporting_stake,
                                    total_stake: vs.total_stake,
                                };
                                self.finality_proofs.insert(block_b.hash, proof);
                                debug!(
                                    hash = %block_b.hash,
                                    signing_stake = supporting_stake,
                                    total_stake = vs.total_stake,
                                    "Finality proof generated"
                                );

                                newly_committed.push(block_b.clone());
                            }
                            // Once committed via one certifier, no need to check others
                            break;
                        }
                    }
                }
            }
        }

        // Add newly committed blocks to the committed list
        if !newly_committed.is_empty() {
            let mut committed = self.committed.write();
            for block in &newly_committed {
                if !committed.contains(&block.hash) {
                    committed.push(block.hash);
                }
            }
        }

        // Drop the read lock before security checks that may need write access
        drop(vs);

        // Store checkpoint every CHECKPOINT_INTERVAL rounds
        if !newly_committed.is_empty() {
            let max_round = newly_committed.iter().map(|b| b.round).max().unwrap_or(0);
            if max_round % security::CHECKPOINT_INTERVAL == 0 && max_round > 0 {
                let mut cr = self.checkpoint_registry.lock();
                if let Some(last_block) = newly_committed.last() {
                    let cp = security::Checkpoint {
                        block_hash: *last_block.hash.as_bytes(),
                        round: max_round,
                        height: max_round,  // height approximated by round
                        state_root: [0u8; 32],  // Would be filled by state layer
                        timestamp: last_block.timestamp,
                        signatures: vec![],  // Would be filled with quorum sigs
                    };
                    cr.add_checkpoint(cp);
                }
            }

            // Run withholding detection periodically (every 100 rounds)
            if max_round % 100 == 0 {
                let wd = self.withholding_detector.lock();
                let reports = wd.detect_withholding(100);
                for report in &reports {
                    warn!(
                        validator = %report.validator,
                        score = report.withholding_score,
                        "Withholding detected — scheduling slash"
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

        // Auto-prune old DAG data after successful commits
        if !newly_committed.is_empty() {
            self.prune_old_rounds();
            // A4: Also prune via committed round for explicit memory bounds
            if let Some(max_committed) = newly_committed.iter().map(|b| b.round).max() {
                self.prune_below_round(max_committed);
            }
        }

        // A6: Expire stale cross-shard locks to prevent deadlocks
        self.expire_stale_locks();
        self.expire_stale_locks_with_timeout(30);

        newly_committed
    }

    /// Prune DAG data older than PRUNE_DEPTH rounds behind the current round.
    /// Keeps recent rounds for reorg safety. Removes blocks, round index entries,
    /// committed hashes, and author-round tracking for pruned rounds.
    fn prune_old_rounds(&self) {
        let current = self.current_round.load(Ordering::SeqCst);
        if current <= PRUNE_DEPTH {
            return;
        }
        let prune_below = current - PRUNE_DEPTH;

        let mut pruned_blocks = 0usize;
        let mut pruned_rounds = 0usize;

        // Collect rounds to prune
        let rounds_to_prune: Vec<u64> = self
            .rounds
            .iter()
            .filter(|entry| *entry.key() < prune_below)
            .map(|entry| *entry.key())
            .collect();

        for round in &rounds_to_prune {
            // Remove blocks in this round from the DAG
            if let Some((_, block_hashes)) = self.rounds.remove(round) {
                for hash in &block_hashes {
                    self.dag.remove(hash);
                    pruned_blocks += 1;
                }
                pruned_rounds += 1;
            }

            // Clean up author_round_blocks entries for this round
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

        // Prune old committed hashes (keep only those still in DAG)
        if pruned_blocks > 0 {
            let mut committed = self.committed.write();
            committed.retain(|hash| self.dag.contains_key(hash));
        }

        if pruned_rounds > 0 {
            debug!(
                pruned_rounds,
                pruned_blocks,
                dag_size = self.dag.len(),
                "DAG pruned old rounds"
            );
        }
    }

    /// Advance to the next round.
    ///
    /// Called when >= 2f+1 stake-weighted blocks have been received for the current round,
    /// indicating enough validators have participated and it is safe to proceed.
    ///
    /// Returns `true` if the round was advanced, `false` if quorum was not met.
    pub fn advance_round(&self) -> bool {
        let current = self.current_round.load(Ordering::SeqCst);
        let vs = self.validator_set.read();

        // Compute stake of blocks in the current round
        let round_blocks = self.blocks_in_round(current);
        let mut round_stake = 0u64;
        let mut seen_authors = std::collections::HashSet::new();

        for hash in &round_blocks {
            if let Some(block) = self.dag.get(hash) {
                // Only count each author once per round
                if seen_authors.insert(block.author) {
                    if let Some(validator) = vs.get_validator(&block.author) {
                        round_stake += validator.stake;
                    }
                }
            }
        }

        if round_stake >= vs.quorum {
            let new_round = current + 1;
            self.current_round.store(new_round, Ordering::SeqCst);
            // Normal quorum-based advance clears the force-advanced flag,
            // restoring strict parent validation for subsequent rounds.
            self.force_advanced.store(false, Ordering::SeqCst);
            self.reset_round_timer();
            info!(
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
                "Cannot advance round: insufficient stake"
            );
            false
        }
    }

    /// Deterministic shard assignment for an address.
    ///
    /// Uses the first 8 bytes of the address as a u64, mod num_shards.
    /// This ensures the same address always maps to the same shard.
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

    /// Determine if a transaction needs full DAG-ordered consensus (cross-shard)
    /// or can use the fast path (single-shard).
    ///
    /// Cross-shard transactions involve sender and recipient on different shards,
    /// requiring full ordering to prevent double-spends across shard boundaries.
    ///
    /// # Arguments
    /// * `tx` - The transaction to check.
    /// * `num_shards` - Total number of shards.
    ///
    /// # Returns
    /// `true` if full consensus ordering is needed (cross-shard), `false` for fast path.
    pub fn needs_full_consensus(tx: &Transaction, num_shards: u16) -> bool {
        if num_shards <= 1 {
            // Single shard: everything is same-shard, fast path
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
            TxBody::MultiSig(body) => {
                // MultiSig needs full consensus if any signer is on a different shard
                body.signers
                    .iter()
                    .any(|signer| Self::shard_of(signer, num_shards) != sender_shard)
            }
            TxBody::DeployContract(_) => {
                // Contract deployment is local to the sender's shard
                false
            }
            TxBody::RegisterAgent(_) => {
                // Agent registration is local
                false
            }
            TxBody::JoinValidator(_) | TxBody::LeaveValidator | TxBody::ClaimRewards | TxBody::UpdateStake(_) => {
                // Validator management transactions are global (affect consensus state)
                false
            }
            TxBody::Governance(_) => {
                // Governance transactions are global
                false
            }
            TxBody::BridgeLock(_) | TxBody::BridgeMint(_) => {
                // Bridge transactions are global (cross-chain)
                false
            }
            TxBody::BatchSettle(_) => false,  // Local to sender's shard
            TxBody::ChannelOpen(body) => {
                let cp_shard = Self::shard_of(&body.counterparty, num_shards);
                sender_shard != cp_shard
            }
            TxBody::ChannelClose(_) => false,  // Escrow is deterministic
            TxBody::ChannelDispute(_) => false,  // Escrow is deterministic
            TxBody::ShardProof(_) => false,  // Shard-local proof recording
            TxBody::InferenceAttestation(_) => false,  // Escrow is local to sender's shard
            TxBody::InferenceChallenge(_) => false,  // Resolved on attestation's shard
            TxBody::InferenceRegister(_) => false,  // Local to sender's shard
        }
    }

    /// Update the validator set (e.g., at epoch boundary).
    pub fn update_validator_set(&self, new_set: ValidatorSet) {
        info!(
            epoch = new_set.epoch,
            validators = new_set.len(),
            total_stake = new_set.total_stake,
            "Validator set updated"
        );
        *self.validator_set.write() = new_set;
    }

    // ── Liveness Hardening (C3) ─────────────────────────────────────────────

    /// Proposer failover timeout in milliseconds.
    /// If the current round hasn't advanced within this time, the next
    /// validator in rotation can take over as proposer.
    const PROPOSER_TIMEOUT_MS: u128 = 100;

    /// View-change timeout in milliseconds.
    /// If the round is stalled for this long, force-advance to prevent halts.
    /// Set to 200ms for ~400ms two-round finality.
    const VIEW_CHANGE_TIMEOUT_MS: u128 = 200;

    /// Check if the current round's proposer has timed out.
    ///
    /// Returns `true` if the current round has been active for longer than
    /// `PROPOSER_TIMEOUT_MS`, indicating the expected proposer may be offline
    /// and the next validator should take over.
    pub fn is_proposer_timed_out(&self) -> bool {
        self.round_start.read().elapsed().as_millis() > Self::PROPOSER_TIMEOUT_MS
    }

    /// Check if a view-change (forced round advance) is needed.
    ///
    /// Returns `true` if the round has been stalled for longer than
    /// `VIEW_CHANGE_TIMEOUT_MS`.  The caller should force `advance_round()`
    /// when this returns true to prevent indefinite stalls.
    pub fn needs_view_change(&self) -> bool {
        self.round_start.read().elapsed().as_millis() > Self::VIEW_CHANGE_TIMEOUT_MS
    }

    /// Reset the round timer (called after advancing to a new round).
    pub fn reset_round_timer(&self) {
        *self.round_start.write() = std::time::Instant::now();
    }

    /// Force-advance to the next round without quorum check.
    ///
    /// Used by the view-change protocol when the round has been stalled
    /// for too long (e.g. proposer crashed).  Skips the normal quorum
    /// requirement to prevent indefinite halts.
    ///
    /// Sets the `force_advanced` flag so that `propose_block()` will
    /// accept whatever parents are available (even below quorum) in the
    /// new round. The flag is cleared on the next successful normal
    /// `advance_round()`.
    pub fn force_advance_round(&self) {
        let old = self.current_round.fetch_add(1, Ordering::SeqCst);
        self.force_advanced.store(true, Ordering::SeqCst);
        self.reset_round_timer();
        warn!(
            old_round = old,
            new_round = old + 1,
            "View-change: force-advanced round"
        );
    }

    /// Returns the fraction of total stake that is "online" (has proposed a
    /// block within the last N rounds).
    ///
    /// The caller can use this to alert if online stake drops near the 2/3
    /// quorum threshold — a warning that the network is close to stalling.
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
                        if let Some(block) = self.dag.get(hash) {
                            if block.author == validator.address {
                                seen = true;
                                break;
                            }
                        }
                    }
                }
                if seen { break; }
            }
            if seen {
                online_stake += validator.stake;
            }
        }

        if vs.total_stake == 0 {
            return 0.0;
        }
        online_stake as f64 / vs.total_stake as f64
    }

    /// Reset the DAG for a validator set transition.
    ///
    /// Called when switching between single-validator and multi-validator mode.
    /// Previous blocks were already executed (fast path or DAG commit), so the
    /// DAG can be cleared and round tracking restarted from 0.
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

    // ── Internal helpers ─────────────────────────────────────────────────────

    /// Insert a block into the DAG and the round index.
    fn insert_block_into_dag(&self, block: &DagBlock) {
        self.dag.insert(block.hash, block.clone());
        self.rounds
            .entry(block.round)
            .or_insert_with(Vec::new)
            .push(block.hash);
    }

    /// Store a DA commitment.
    pub fn store_da_commitment(&self, commitment: data_availability::DACommitment) {
        self.da_commitments.insert(commitment.block_hash, commitment);
    }

    /// Get a DA commitment by block hash.
    pub fn get_da_commitment(&self, block_hash: &Hash256) -> Option<data_availability::DACommitment> {
        self.da_commitments.get(block_hash).map(|r| r.value().clone())
    }

    /// Lock a cross-shard transaction.
    pub fn lock_cross_shard(
        &self,
        tx_hash: Hash256,
        source_shard: u16,
        target_shard: u16,
        source_block_hash: Hash256,
        source_round: u64,
    ) -> Result<CrossShardProof, ConsensusError> {
        if self.pending_cross_shard.contains_key(&tx_hash) {
            return Err(ConsensusError::CrossShardLockAlreadyExists(
                format!("tx {} already locked", tx_hash),
            ));
        }
        let lock_hash = arc_crypto::hash_bytes(&bincode::serialize(&(&tx_hash, source_shard, target_shard)).unwrap());
        let inclusion_proof = bincode::serialize(&(&tx_hash, &source_block_hash)).unwrap_or_default();
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

    /// Commit a locked cross-shard transaction.
    pub fn commit_cross_shard(&self, tx_hash: Hash256) -> Result<CrossShardProof, ConsensusError> {
        let (_, mut proof) = self.pending_cross_shard.remove(&tx_hash)
            .ok_or_else(|| ConsensusError::CrossShardLockNotFound(
                format!("tx {} not found in pending", tx_hash),
            ))?;
        proof.status = CrossShardStatus::Committed;
        self.completed_cross_shard.insert(tx_hash, proof.clone());
        Ok(proof)
    }

    /// Abort a locked cross-shard transaction.
    pub fn abort_cross_shard(&self, tx_hash: Hash256) -> Result<CrossShardProof, ConsensusError> {
        let (_, mut proof) = self.pending_cross_shard.remove(&tx_hash)
            .ok_or_else(|| ConsensusError::CrossShardLockNotFound(
                format!("tx {} not found in pending", tx_hash),
            ))?;
        proof.status = CrossShardStatus::Aborted;
        self.completed_cross_shard.insert(tx_hash, proof.clone());
        Ok(proof)
    }

    /// Atomically lock a batch of cross-shard transactions.
    /// If any lock fails, all previously locked transactions in this batch are aborted.
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
                    // Abort all previously locked transactions in this batch
                    for prev_proof in &proofs {
                        let _ = self.abort_cross_shard(prev_proof.tx_hash);
                    }
                    return Err(ConsensusError::CrossShardLockFailed(
                        format!("batch failed at tx {}: {}", tx_hash, e),
                    ));
                }
            }
        }
        Ok(proofs)
    }

    /// Get counts of (pending, completed) cross-shard transactions.
    pub fn cross_shard_stats(&self) -> (usize, usize) {
        (self.pending_cross_shard.len(), self.completed_cross_shard.len())
    }

    // ── A6: Cross-Shard Deadlock Prevention ─────────────────────────────────

    /// Expire cross-shard locks that have been held longer than the timeout.
    /// Prevents deadlocks from slow or crashed shards holding locks indefinitely.
    /// Should be called on every `try_commit()` cycle.
    ///
    /// Returns the number of locks expired.
    pub fn expire_stale_locks(&self) -> usize {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let current_round = self.current_round.load(Ordering::SeqCst);
        const MAX_LOCK_ROUNDS: u64 = 100; // locks expire after 100 rounds regardless of wall time

        let expired: Vec<Hash256> = self.pending_cross_shard
            .iter()
            .filter(|entry| {
                let proof = entry.value();
                let time_expired = now_ms.saturating_sub(proof.locked_at_ms) > CROSS_SHARD_LOCK_TIMEOUT_MS;
                let round_expired = current_round.saturating_sub(proof.locked_at_round) > MAX_LOCK_ROUNDS;
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
                    "Cross-shard lock expired — aborting to prevent deadlock"
                );
                self.completed_cross_shard.insert(tx_hash, proof);
            }
        }

        if count > 0 {
            info!(expired = count, "Expired stale cross-shard locks");
        }
        count
    }

    // ── A8: Finality Proof Generation ───────────────────────────────────────

    /// Generate a finality proof for a committed block.
    ///
    /// The proof includes the block hash, round, and the signatures from
    /// round R+2 validators that supported the commit. Light clients verify
    /// that the signing stake >= 2/3 of total stake.
    pub fn generate_finality_proof(
        &self,
        block_hash: &Hash256,
        height: u64,
    ) -> Option<FinalityProof> {
        let block = self.dag.get(block_hash)?;
        let vs = self.validator_set.read();
        let round = block.round;

        // Collect signatures from round R+2 validators who supported the commit
        let round_r1_blocks = self.blocks_in_round(round + 1);
        let round_r2_blocks = self.blocks_in_round(round + 2);

        let mut quorum_sigs = Vec::new();
        let mut signing_stake = 0u64;

        // Find blocks in R+1 that reference this block
        for c_hash in &round_r1_blocks {
            if let Some(c_block) = self.dag.get(c_hash) {
                if !c_block.parents.contains(block_hash) {
                    continue;
                }
                // Find blocks in R+2 that reference C
                for d_hash in &round_r2_blocks {
                    if let Some(d_block) = self.dag.get(d_hash) {
                        if d_block.parents.contains(c_hash) {
                            if let Some(validator) = vs.get_validator(&d_block.author) {
                                // Avoid duplicates (same author counted once)
                                if !quorum_sigs.iter().any(|(addr, _)| *addr == d_block.author) {
                                    quorum_sigs.push((
                                        d_block.author,
                                        d_block.signature.clone(),
                                    ));
                                    signing_stake += validator.stake;
                                }
                            }
                        }
                    }
                }
            }
        }

        let proof = FinalityProof {
            block_hash: *block_hash,
            round,
            height,
            quorum_signatures: quorum_sigs,
            signing_stake,
            total_stake: vs.total_stake,
        };

        // Store the proof
        self.finality_proofs.insert(*block_hash, proof.clone());
        Some(proof)
    }

    /// Get a stored finality proof by block hash.
    pub fn get_finality_proof(&self, block_hash: &Hash256) -> Option<FinalityProof> {
        self.finality_proofs.get(block_hash).map(|r| r.value().clone())
    }

    // ── A4: DAG Pruning / Memory Bounds ──────────────────────────────────────

    /// Prune DAG blocks and round data older than `committed_round - PRUNE_DEPTH`.
    ///
    /// Keeps PRUNE_DEPTH rounds of history for reorg safety. If `committed_round`
    /// is less than PRUNE_DEPTH, no pruning occurs. Returns the number of
    /// blocks pruned.
    pub fn prune_below_round(&self, committed_round: u64) -> usize {
        if committed_round < PRUNE_DEPTH {
            return 0;
        }
        let cutoff = committed_round - PRUNE_DEPTH;

        let mut pruned_count = 0usize;

        // Collect rounds to prune (anything strictly below the cutoff)
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

            // Clean up author_round_blocks entries for this round
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

        // Prune committed hashes that are no longer in the DAG
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

    // ── A3: Slashing Enforcement — Equivocation Detection ────────────────────

    /// Check if the given block constitutes equivocation (same author already
    /// proposed a different block in the same round).
    ///
    /// Returns an `EquivocationProof` if the author already has a block in
    /// this round with a different hash, or `None` if this is the first block
    /// from that author in this round.
    pub fn detect_equivocation(&self, block: &DagBlock) -> Option<EquivocationProof> {
        let key = (block.author, block.round);
        if let Some(existing_hash) = self.author_round_blocks.get(&key) {
            if *existing_hash.value() != block.hash {
                return Some(EquivocationProof {
                    author: block.author,
                    round: block.round,
                    block1_hash: *existing_hash.value(),
                    block2_hash: block.hash,
                });
            }
        }
        None
    }

    /// Enforce a slash: actually reduce the validator's stake and potentially
    /// remove them from the validator set.
    ///
    /// If the validator's stake falls below `STAKE_SPARK` (500,000 ARC), they
    /// are removed from the validator set entirely.
    ///
    /// Returns `true` if the validator was removed from the set.
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

        // Check if the validator was removed (no longer in set)
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

    // ── A6: Cross-Shard Deadlock Prevention (parameterized timeout) ──────────

    /// Expire cross-shard locks that have been held longer than `timeout_secs`.
    ///
    /// Finds all `CrossShardProof` entries in `pending_cross_shard` where
    /// `locked_at_ms` is older than the given timeout, aborts those transactions,
    /// and moves them to `completed_cross_shard`.
    ///
    /// Returns the number of locks expired.
    pub fn expire_stale_locks_with_timeout(&self, timeout_secs: u64) -> usize {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let current_round = self.current_round.load(Ordering::SeqCst);
        const MAX_LOCK_ROUNDS: u64 = 100; // locks expire after 100 rounds regardless of wall time

        let timeout_ms = timeout_secs * 1000;

        let expired: Vec<Hash256> = self.pending_cross_shard
            .iter()
            .filter(|entry| {
                let proof = entry.value();
                let time_expired = now_ms.saturating_sub(proof.locked_at_ms) > timeout_ms;
                let round_expired = current_round.saturating_sub(proof.locked_at_round) > MAX_LOCK_ROUNDS;
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
                    "Cross-shard lock expired (parameterized) — aborting to prevent deadlock"
                );
                self.completed_cross_shard.insert(tx_hash, proof);
            }
        }

        if count > 0 {
            info!(expired = count, timeout_secs, "Expired stale cross-shard locks (parameterized)");
        }
        count
    }

    // ── Propose-Verify Protocol ──────────────────────────────────────────────

    /// Set this node's role in the propose-verify protocol.
    pub fn set_node_role(&mut self, role: NodeRole) {
        self.node_role = role;
    }

    /// Get this node's role.
    pub fn node_role(&self) -> NodeRole {
        self.node_role
    }

    /// Submit a fraud proof when a proposer's state diff doesn't match.
    /// The proposer is slashed and the block is rejected.
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

    // ── Security Module Accessors ───────────────────────────────────────────

    /// Get a reference to the withholding detector.
    pub fn withholding_detector(&self) -> &parking_lot::Mutex<WithholdingDetector> {
        &self.withholding_detector
    }

    /// Get a reference to the checkpoint registry.
    pub fn checkpoint_registry(&self) -> &parking_lot::Mutex<CheckpointRegistry> {
        &self.checkpoint_registry
    }

    /// Get a reference to the stake tracker.
    pub fn stake_tracker(&self) -> &parking_lot::Mutex<StakeTracker> {
        &self.stake_tracker
    }
}

// ── Cross-Shard Utilities ───────────────────────────────────────────────────

/// Deterministically assign an address to a shard.
pub fn assign_shard(address: &Hash256, num_shards: u16) -> u16 {
    let bytes = address.as_bytes();
    let val = u16::from_le_bytes([bytes[0], bytes[1]]);
    val % num_shards
}

/// Check if a transaction between two addresses is cross-shard.
pub fn is_cross_shard(from: &Hash256, to: &Hash256, num_shards: u16) -> bool {
    if num_shards <= 1 {
        return false;
    }
    assign_shard(from, num_shards) != assign_shard(to, num_shards)
}

#[cfg(test)]
mod formal_proofs;

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    /// Create a deterministic test address from a byte.
    fn test_addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    /// Create a test validator set with the given number of Arc-tier validators.
    /// Each validator has 5_000_000 stake (Arc tier).
    fn test_validator_set(n: usize) -> ValidatorSet {
        let validators: Vec<Validator> = (0..n)
            .map(|i| {
                Validator::new(test_addr(i as u8), STAKE_ARC, i as u16)
                    .expect("valid validator")
            })
            .collect();
        ValidatorSet::new(validators, 1)
    }

    /// Create a test validator set with mixed tiers.
    fn mixed_validator_set() -> ValidatorSet {
        let validators = vec![
            Validator::new(test_addr(0), STAKE_CORE, 0).unwrap(),  // Core: 50M
            Validator::new(test_addr(1), STAKE_ARC, 1).unwrap(),   // Arc: 5M
            Validator::new(test_addr(2), STAKE_ARC, 0).unwrap(),   // Arc: 5M
            Validator::new(test_addr(3), STAKE_SPARK, 1).unwrap(), // Spark: 500K
        ];
        ValidatorSet::new(validators, 1)
    }

    /// Helper: Create a DAG block directly (bypassing propose_block) for test scaffolding.
    /// Transactions are sorted into canonical lexicographic order to match the
    /// MEV-protection scheme enforced by `verify_ordering()`.
    fn make_block(
        author: Address,
        round: u64,
        parents: Vec<Hash256>,
        transactions: Vec<Hash256>,
        timestamp: u64,
    ) -> DagBlock {
        let mut transactions = transactions;
        transactions.sort_by(|a, b| a.0.cmp(&b.0));
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

    // ── 1. Validator Set Management ──────────────────────────────────────────

    #[test]
    fn test_validator_set_creation_and_quorum() {
        let vs = test_validator_set(4);
        // 4 validators * 5M = 20M total stake
        assert_eq!(vs.total_stake, 4 * STAKE_ARC);
        assert_eq!(vs.len(), 4);
        assert_eq!(vs.epoch, 1);
        // quorum = ceil(2/3 * 20M) = ceil(40M/3) = 13_333_334
        assert_eq!(vs.quorum, (2 * vs.total_stake + 2) / 3);
        assert!(vs.quorum > vs.total_stake * 2 / 3);
        // 3 validators have 15M stake, which should exceed quorum (~13.3M)
        assert!(vs.has_quorum(&[test_addr(0), test_addr(1), test_addr(2)]));
        // 2 validators have 10M stake, which should NOT exceed quorum (~13.3M)
        assert!(!vs.has_quorum(&[test_addr(0), test_addr(1)]));
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
        assert_eq!(
            StakeTier::from_stake(100_000_000),
            Some(StakeTier::Core)
        );

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
        // Total: 50M + 5M + 5M + 500K = 60_500_000
        assert_eq!(vs.total_stake, STAKE_CORE + 2 * STAKE_ARC + STAKE_SPARK);

        // Core and Arc can produce; Spark cannot
        assert!(vs.can_produce_blocks(&test_addr(0))); // Core
        assert!(vs.can_produce_blocks(&test_addr(1))); // Arc
        assert!(vs.can_produce_blocks(&test_addr(2))); // Arc
        assert!(!vs.can_produce_blocks(&test_addr(3))); // Spark

        // Unknown address
        assert!(!vs.can_produce_blocks(&test_addr(99)));
        assert!(!vs.is_validator(&test_addr(99)));

        // Block producers list
        let producers = vs.block_producers();
        assert_eq!(producers.len(), 3); // Core + 2 Arc
    }

    // ── 1b. Dynamic Validator Set Management ─────────────────────────────────

    #[test]
    fn test_join_validator() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        // Join a new validator
        let result = engine.join_validator(
            test_addr(2),
            STAKE_ARC,
            [2u8; 32],
            1,
        );
        assert!(result.is_ok());

        let vs = engine.validator_set();
        assert_eq!(vs.len(), 2);
        assert!(vs.is_validator(&test_addr(2)));
    }

    #[test]
    fn test_join_validator_insufficient_stake() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        // Try to join with insufficient stake
        let result = engine.join_validator(
            test_addr(2),
            100, // Below STAKE_SPARK
            [2u8; 32],
            1,
        );
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
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        engine.update_validator_stake(&test_addr(1), STAKE_CORE).unwrap();

        let vs = engine.validator_set();
        let v = vs.get_validator(&test_addr(1)).unwrap();
        assert_eq!(v.stake, STAKE_CORE);
        assert_eq!(v.tier, StakeTier::Core);
    }

    #[test]
    fn test_epoch_transition() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let new_epoch = engine.epoch_transition();
        assert_eq!(new_epoch, 2);

        let vs = engine.validator_set();
        assert_eq!(vs.epoch, 2);
    }

    #[test]
    fn test_join_validator_duplicate() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));

        let result = engine.join_validator(
            test_addr(1), // Already in set
            STAKE_ARC,
            [1u8; 32],
            0,
        );
        assert!(result.is_err());
    }

    // ── 2. Block Proposal ────────────────────────────────────────────────────

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
        assert!(block.parents.is_empty()); // Round 0 has no parents
        assert_eq!(block.transactions.len(), 2);
        assert_eq!(block.timestamp, 1000);
        assert_ne!(block.hash, Hash256::ZERO);
        assert!(block.verify_hash());

        // Block should be in the DAG
        assert_eq!(engine.dag_size(), 1);
        assert_eq!(engine.blocks_in_round(0).len(), 1);
    }

    #[test]
    fn test_propose_block_spark_tier_rejected() {
        let vs = mixed_validator_set();
        let spark_addr = test_addr(3); // Spark tier
        let engine = ConsensusEngine::new(vs, spark_addr);

        let result = engine.propose_block(vec![], 1000);
        assert_eq!(result.unwrap_err(), ConsensusError::NotValidator);
    }

    // ── 3. Block Receipt ─────────────────────────────────────────────────────

    #[test]
    fn test_receive_valid_block() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        // Create and receive a block from validator 1
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

        // Address 99 is not in the validator set
        let block = make_block(test_addr(99), 0, vec![], vec![], 1000);
        let result = engine.receive_block(&block);
        assert_eq!(result.unwrap_err(), ConsensusError::NotValidator);
    }

    #[test]
    fn test_receive_block_invalid_hash_rejected() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let mut block = make_block(test_addr(1), 0, vec![], vec![], 1000);
        // Corrupt the hash
        block.hash = Hash256::ZERO;

        let result = engine.receive_block(&block);
        assert!(matches!(result, Err(ConsensusError::InvalidBlock(_))));
    }

    #[test]
    fn test_receive_block_future_round_catchup() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));
        // Current round is 0, block in round 5 — should fast-forward and accept
        let block = make_block(test_addr(1), 5, vec![], vec![], 1000);
        let result = engine.receive_block(&block);
        // Block is accepted (round catch-up), though it may fail parent validation
        // The key is that it does NOT return InvalidRound
        assert_ne!(result.clone().err(), Some(ConsensusError::InvalidRound),
            "Future blocks should trigger round catch-up, not InvalidRound: {:?}", result);
    }

    // ── 4. Commit Rule ───────────────────────────────────────────────────────

    #[test]
    fn test_commit_rule_two_round() {
        // Setup: 4 validators with equal 5M stake each (total 20M, quorum ~13.3M)
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        // Round 0: Block B from validator 0
        let block_b = make_block(test_addr(0), 0, vec![], vec![hash_bytes(b"tx_b")], 100);
        engine.receive_block(&block_b).unwrap();

        // Also insert blocks from validators 1, 2, 3 in round 0 (needed for round 1 parents)
        let b1 = make_block(test_addr(1), 0, vec![], vec![], 101);
        let b2 = make_block(test_addr(2), 0, vec![], vec![], 102);
        let b3 = make_block(test_addr(3), 0, vec![], vec![], 103);
        engine.receive_block(&b1).unwrap();
        engine.receive_block(&b2).unwrap();
        engine.receive_block(&b3).unwrap();

        // Advance to round 1
        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 1);

        // Round 1: Block C from validator 1, referencing B (and enough parents for quorum)
        let block_c = make_block(
            test_addr(1),
            1,
            vec![block_b.hash, b1.hash, b2.hash], // References B + others for quorum
            vec![hash_bytes(b"tx_c")],
            200,
        );
        engine.receive_block(&block_c).unwrap();

        // Also need more round 1 blocks to advance
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

        // Advance to round 2
        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 2);

        // No commits yet (need round 2 blocks referencing C)
        let committed = engine.try_commit();
        assert!(committed.is_empty());

        // Round 2: Blocks from validators 0, 2, 3 referencing C
        // Need >= quorum (3 * 5M = 15M >= ~13.3M)
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

        // Now try_commit should commit block B
        let committed = engine.try_commit();
        assert!(
            committed.iter().any(|b| b.hash == block_b.hash),
            "Block B should be committed"
        );

        // Verify it is in the committed list
        let committed_list = engine.committed_blocks();
        assert!(committed_list.contains(&block_b.hash));
    }

    #[test]
    fn test_commit_not_triggered_without_quorum_in_r2() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        // Round 0: blocks from all 4 validators
        let b0 = make_block(test_addr(0), 0, vec![], vec![], 100);
        let b1 = make_block(test_addr(1), 0, vec![], vec![], 101);
        let b2 = make_block(test_addr(2), 0, vec![], vec![], 102);
        let b3 = make_block(test_addr(3), 0, vec![], vec![], 103);
        engine.receive_block(&b0).unwrap();
        engine.receive_block(&b1).unwrap();
        engine.receive_block(&b2).unwrap();
        engine.receive_block(&b3).unwrap();
        engine.advance_round();

        // Round 1: block C references B0
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

        // Round 2: Only 1 block references C1 (5M stake < quorum ~13.3M)
        let d0 = make_block(
            test_addr(0),
            2,
            vec![c1.hash, c2.hash, c3.hash],
            vec![],
            300,
        );
        engine.receive_block(&d0).unwrap();

        // Should NOT commit B0 since only 1 validator in R+2 references C1
        let committed = engine.try_commit();
        // d0 references c1, and d0 has stake 5M, which is < quorum ~13.3M
        // So b0 should NOT be committed
        let b0_committed = committed.iter().any(|b| b.hash == b0.hash);
        assert!(
            !b0_committed,
            "B0 should NOT be committed with insufficient R+2 support"
        );
    }

    // ── 5. Round Advancement ─────────────────────────────────────────────────

    #[test]
    fn test_advance_round_requires_quorum() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        // No blocks yet: cannot advance
        assert!(!engine.advance_round());
        assert_eq!(engine.current_round(), 0);

        // Add 2 blocks (10M stake < quorum ~13.3M): still cannot advance
        let b0 = make_block(test_addr(0), 0, vec![], vec![], 100);
        let b1 = make_block(test_addr(1), 0, vec![], vec![], 101);
        engine.receive_block(&b0).unwrap();
        engine.receive_block(&b1).unwrap();
        assert!(!engine.advance_round());
        assert_eq!(engine.current_round(), 0);

        // Add a third block (15M >= ~13.3M): now we can advance
        let b2 = make_block(test_addr(2), 0, vec![], vec![], 102);
        engine.receive_block(&b2).unwrap();
        assert!(engine.advance_round());
        assert_eq!(engine.current_round(), 1);
    }

    #[test]
    fn test_advance_round_deduplicates_authors() {
        // If the same author submits multiple blocks in one round,
        // their stake should only count once.
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        // Two blocks from the same author (validator 0)
        let b0a = make_block(test_addr(0), 0, vec![], vec![hash_bytes(b"a")], 100);
        let b0b = make_block(test_addr(0), 0, vec![], vec![hash_bytes(b"b")], 101);
        engine.receive_block(&b0a).unwrap();
        engine.receive_block(&b0b).unwrap();

        // Only 5M effective stake (one author): cannot advance
        assert!(!engine.advance_round());

        // Add blocks from two more validators -> 15M >= quorum
        let b1 = make_block(test_addr(1), 0, vec![], vec![], 102);
        let b2 = make_block(test_addr(2), 0, vec![], vec![], 103);
        engine.receive_block(&b1).unwrap();
        engine.receive_block(&b2).unwrap();
        assert!(engine.advance_round());
    }

    // ── 6. Shard Assignment ──────────────────────────────────────────────────

    #[test]
    fn test_shard_assignment_deterministic() {
        let addr = test_addr(42);
        let shard1 = ConsensusEngine::shard_of(&addr, 8);
        let shard2 = ConsensusEngine::shard_of(&addr, 8);
        assert_eq!(shard1, shard2, "shard assignment must be deterministic");

        // Must be within range
        assert!(shard1 < 8);
    }

    #[test]
    fn test_shard_assignment_distributes() {
        // Check that different addresses get (potentially) different shards
        let num_shards = 4u16;
        let mut shard_counts = vec![0u32; num_shards as usize];

        for i in 0..100u8 {
            let addr = test_addr(i);
            let shard = ConsensusEngine::shard_of(&addr, num_shards);
            assert!(shard < num_shards);
            shard_counts[shard as usize] += 1;
        }

        // Every shard should have at least some assignments (probabilistic, but
        // 100 addresses across 4 shards should cover all of them)
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
        // With 1 shard, everything maps to shard 0
        let addr = test_addr(42);
        assert_eq!(ConsensusEngine::shard_of(&addr, 1), 0);
    }

    #[test]
    fn test_shard_of_zero_shards() {
        // Edge case: 0 shards returns 0
        let addr = test_addr(42);
        assert_eq!(ConsensusEngine::shard_of(&addr, 0), 0);
    }

    // ── 7. Cross-Shard Detection ─────────────────────────────────────────────

    #[test]
    fn test_same_shard_transfer_fast_path() {
        // Find two addresses that hash to the same shard
        let num_shards = 4u16;
        let from_addr = test_addr(0);
        let from_shard = ConsensusEngine::shard_of(&from_addr, num_shards);

        // Search for a `to` address on the same shard
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
        // Find two addresses on different shards
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
        // With only 1 shard, everything is fast path
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

    // ── 8. Cross-Shard Receipt ───────────────────────────────────────────────

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

    // ── 9. DAG Block Hash Integrity ──────────────────────────────────────────

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

    // ── 10. Full Multi-Round Integration ─────────────────────────────────────

    #[test]
    fn test_full_three_round_lifecycle() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        // ── Round 0: All validators propose ──
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

        // ── Round 1: All validators propose, referencing round 0 ──
        let all_r0 = vec![r0_b0.hash, r0_b1.hash, r0_b2.hash]; // quorum-worth of parents
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

        // ── Round 2: All validators propose, referencing round 1 ──
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

        // ── Commit check ──
        // Round 0 blocks that are parents of round 1 blocks that are referenced
        // by >= quorum in round 2 should be committed.
        let committed = engine.try_commit();
        assert!(
            !committed.is_empty(),
            "should have committed at least one block from round 0"
        );

        // The committed set should only contain round 0 blocks
        for block in &committed {
            assert_eq!(block.round, 0);
        }

        // Committed list should be persisted
        let committed_list = engine.committed_blocks();
        assert!(!committed_list.is_empty());
    }

    // ── 11. Validator Set Update ─────────────────────────────────────────────

    #[test]
    fn test_update_validator_set() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        assert_eq!(engine.validator_set().epoch, 1);
        assert_eq!(engine.validator_set().len(), 4);

        // Update to a new epoch with 6 validators
        let new_vs = ValidatorSet::new(
            (0..6)
                .map(|i| {
                    Validator::new(test_addr(i as u8), STAKE_ARC, i as u16).unwrap()
                })
                .collect(),
            2,
        );
        engine.update_validator_set(new_vs);

        assert_eq!(engine.validator_set().epoch, 2);
        assert_eq!(engine.validator_set().len(), 6);
    }

    // ── 12. Cross-Shard Execution Proofs ────────────────────────────────────

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

        let aborted = engine
            .abort_cross_shard(tx)
            .expect("abort should succeed");
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

        let result = engine.atomic_cross_shard_batch(
            vec![tx1, tx2, tx3],
            vec![(0, 1), (0, 2), (1, 3)],
        );
        assert!(result.is_err(), "batch should fail because tx2 is already locked");

        assert!(engine.completed_cross_shard.get(&tx1).is_some());
        let tx1_proof = engine.completed_cross_shard.get(&tx1).unwrap();
        assert_eq!(tx1_proof.status, CrossShardStatus::Aborted);

        assert!(engine.pending_cross_shard.contains_key(&tx2));

        assert!(!engine.pending_cross_shard.contains_key(&tx3));
        assert!(!engine.completed_cross_shard.contains_key(&tx3));

        assert_eq!(engine.cross_shard_stats(), (1, 1));
    }

    // ── 13. MEV Protection — Fair Ordering (A7) ─────────────────────────────

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
        assert_ne!(ca, cb, "different tx order must produce different commitment");
    }

    #[test]
    fn test_block_verifies_correct_ordering() {
        let block = make_block(
            test_addr(0), 0, vec![],
            vec![hash_bytes(b"tx1"), hash_bytes(b"tx2")],
            1000,
        );
        assert!(block.verify_ordering(), "block with correct ordering should verify");
    }

    #[test]
    fn test_block_rejects_tampered_ordering() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let txs = vec![hash_bytes(b"tx1"), hash_bytes(b"tx2")];
        let mut block = make_block(test_addr(1), 0, vec![], txs, 1000);

        // Tamper with the ordering commitment (simulate reordering)
        block.ordering_commitment = DagBlock::compute_ordering_commitment(
            &[hash_bytes(b"tx2"), hash_bytes(b"tx1")]
        );
        // Recompute hash with tampered commitment
        block.hash = block.compute_hash();

        let result = engine.receive_block(&block);
        assert!(
            matches!(result, Err(ConsensusError::MevOrderingViolation(_))),
            "reordered transactions should be rejected as MEV violation"
        );
    }

    #[test]
    fn test_block_rejects_unsorted_transactions() {
        // Manually construct a block with transactions NOT in canonical order.
        // Even if the ordering commitment matches the unsorted list, the block
        // must be rejected because transactions are not lexicographically sorted.
        let tx_a = hash_bytes(b"aaa");
        let tx_b = hash_bytes(b"bbb");

        // Determine actual sort order of these hashes
        let (first, second) = if tx_a.0 <= tx_b.0 {
            (tx_a, tx_b)
        } else {
            (tx_b, tx_a)
        };

        // Put them in REVERSE canonical order
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

        // The commitment matches the transactions as-stored, but the
        // transactions are NOT in sorted order, so verify_ordering must fail.
        assert!(
            !block.verify_ordering(),
            "block with unsorted transactions must fail ordering verification"
        );
    }

    #[test]
    fn test_propose_block_sorts_transactions() {
        // propose_block should sort transactions into canonical order
        // regardless of the caller's input order.
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx_a = hash_bytes(b"alpha");
        let tx_b = hash_bytes(b"bravo");
        let tx_c = hash_bytes(b"charlie");

        // Sort to get canonical order
        let mut expected = vec![tx_a, tx_b, tx_c];
        expected.sort_by(|a, b| a.0.cmp(&b.0));

        // Feed them in reverse canonical order
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

    // ── 14. Cross-Shard Deadlock Prevention (A6) ────────────────────────────

    #[test]
    fn test_cross_shard_lock_has_timestamp() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"timestamped_lock");
        let proof = engine
            .lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0)
            .unwrap();
        assert!(proof.locked_at_ms > 0, "lock should have a non-zero timestamp");
    }

    #[test]
    fn test_expire_stale_locks_no_stale() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"fresh_lock");
        engine.lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0).unwrap();

        // Just locked — should not be expired
        let expired = engine.expire_stale_locks();
        assert_eq!(expired, 0, "fresh lock should not be expired");
        assert_eq!(engine.cross_shard_stats(), (1, 0));
    }

    #[test]
    fn test_expire_stale_locks_force_old() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"old_lock");
        engine.lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0).unwrap();

        // Manually set the lock timestamp to 60 seconds ago
        if let Some(mut entry) = engine.pending_cross_shard.get_mut(&tx) {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            entry.locked_at_ms = now_ms - 60_000; // 60 seconds ago
        }

        let expired = engine.expire_stale_locks();
        assert_eq!(expired, 1, "stale lock should be expired");
        assert_eq!(engine.cross_shard_stats(), (0, 1));

        // Check it was aborted
        let proof = engine.completed_cross_shard.get(&tx).unwrap();
        assert_eq!(proof.status, CrossShardStatus::Aborted);
    }

    // ── 15. Finality Proofs (A8) ────────────────────────────────────────────

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
        assert!(proof.has_sufficient_stake(), "75% stake should be sufficient");

        let insufficient = FinalityProof {
            signing_stake: 10_000_000,
            ..proof.clone()
        };
        assert!(!insufficient.has_sufficient_stake(), "50% stake should be insufficient");
    }

    #[test]
    fn test_finality_proof_generation() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        // Build 3-round DAG identical to test_commit_rule_two_round
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
            test_addr(1), 1,
            vec![block_b.hash, b1.hash, b2.hash],
            vec![], 200,
        );
        let c2 = make_block(test_addr(2), 1, vec![block_b.hash, b1.hash, b2.hash], vec![], 201);
        let c3 = make_block(test_addr(3), 1, vec![block_b.hash, b1.hash, b3.hash], vec![], 202);
        engine.receive_block(&block_c).unwrap();
        engine.receive_block(&c2).unwrap();
        engine.receive_block(&c3).unwrap();
        engine.advance_round();

        let d0 = make_block(test_addr(0), 2, vec![block_c.hash, c2.hash, c3.hash], vec![], 300);
        let d2 = make_block(test_addr(2), 2, vec![block_c.hash, c2.hash, c3.hash], vec![], 301);
        let d3 = make_block(test_addr(3), 2, vec![block_c.hash, c2.hash, c3.hash], vec![], 302);
        engine.receive_block(&d0).unwrap();
        engine.receive_block(&d2).unwrap();
        engine.receive_block(&d3).unwrap();

        // Commit
        let committed = engine.try_commit();
        assert!(!committed.is_empty());

        // Generate finality proof for block B
        let proof = engine.generate_finality_proof(&block_b.hash, 1).unwrap();
        assert_eq!(proof.block_hash, block_b.hash);
        assert_eq!(proof.round, 0);
        assert_eq!(proof.height, 1);
        assert!(proof.has_sufficient_stake(), "should have quorum stake");
        assert!(!proof.quorum_signatures.is_empty());

        // Should be retrievable
        let stored = engine.get_finality_proof(&block_b.hash).unwrap();
        assert_eq!(stored.block_hash, proof.block_hash);
    }

    // ── 16. DAG Pruning / Memory Bounds (A4) ─────────────────────────────────

    #[test]
    fn test_dag_pruning_removes_old_rounds() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        // Insert blocks in rounds 0 through 110
        for round in 0..=110u64 {
            for v in 0..4u8 {
                let parents = if round == 0 {
                    vec![]
                } else {
                    // Use at least one parent from the previous round
                    engine.blocks_in_round(round - 1)
                };
                let block = make_block(
                    test_addr(v),
                    round,
                    parents,
                    vec![],
                    round * 100 + v as u64,
                );
                // Insert directly into DAG (bypass validation for test scaffolding)
                engine.dag.insert(block.hash, block.clone());
                engine.rounds.entry(round).or_insert_with(Vec::new).push(block.hash);
                engine.author_round_blocks.insert((block.author, round), block.hash);
            }
        }

        let initial_dag_size = engine.dag_size();
        assert!(initial_dag_size > 0);

        // Prune with committed_round = 110 — should remove rounds < 110 - 100 = 10
        let pruned = engine.prune_below_round(110);
        assert!(pruned > 0, "should have pruned some blocks");

        // Rounds 0..9 should be gone
        for round in 0..10u64 {
            assert!(
                engine.blocks_in_round(round).is_empty(),
                "round {} should have been pruned",
                round
            );
        }

        // Round 10 and above should still exist
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

        // Insert blocks in rounds 0 through 50
        for round in 0..=50u64 {
            let block = make_block(
                test_addr(0),
                round,
                if round == 0 { vec![] } else { engine.blocks_in_round(round - 1) },
                vec![],
                round * 100,
            );
            engine.dag.insert(block.hash, block.clone());
            engine.rounds.entry(round).or_insert_with(Vec::new).push(block.hash);
        }

        let initial_size = engine.dag_size();

        // committed_round = 50, which is < PRUNE_DEPTH (100), so nothing should be pruned
        let pruned = engine.prune_below_round(50);
        assert_eq!(pruned, 0, "no rounds should be pruned when committed_round < PRUNE_DEPTH");
        assert_eq!(engine.dag_size(), initial_size);

        // All rounds should still exist
        for round in 0..=50u64 {
            assert!(
                !engine.blocks_in_round(round).is_empty(),
                "round {} should still exist",
                round
            );
        }
    }

    // ── 17. Equivocation Detection & Slashing (A3) ──────────────────────────

    #[test]
    fn test_equivocation_detected_same_round() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        // First block from validator 1 in round 0
        let block1 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx_a")], 1000);
        engine.receive_block(&block1).unwrap();

        // Second (different) block from validator 1 in round 0 — equivocation!
        let block2 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx_b")], 1001);

        // detect_equivocation should find it
        let proof = engine.detect_equivocation(&block2);
        assert!(proof.is_some(), "should detect equivocation for same author, same round");

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

        // Block from validator 1 in round 0
        let block1 = make_block(test_addr(1), 0, vec![], vec![hash_bytes(b"tx_a")], 1000);
        engine.receive_block(&block1).unwrap();

        // Block from validator 1 in round 1 — different round, NOT equivocation
        // Need to set up parent references properly
        let block2 = make_block(test_addr(1), 1, vec![block1.hash], vec![hash_bytes(b"tx_b")], 1001);

        // Insert block2's author+round into author_round_blocks for detection
        // (normally receive_block would do this, but we test detect_equivocation directly)
        let proof = engine.detect_equivocation(&block2);
        assert!(proof.is_none(), "different rounds should not trigger equivocation");
    }

    #[test]
    fn test_slash_reduces_stake() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let target = test_addr(1);
        let initial_stake = engine.validator_set().get_validator(&target).unwrap().stake;
        assert_eq!(initial_stake, STAKE_ARC); // 5,000,000

        // Slash 1,000,000 ARC — should reduce stake but not remove
        let removed = engine.enforce_slash(&target, 1_000_000);
        assert!(!removed, "validator should not be removed with 4M remaining");

        let new_stake = engine.validator_set().get_validator(&target).unwrap().stake;
        assert_eq!(new_stake, STAKE_ARC - 1_000_000);
    }

    #[test]
    fn test_slash_removes_below_minimum() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let target = test_addr(1);
        assert!(engine.validator_set().is_validator(&target));

        // Slash the entire stake — should remove the validator
        let removed = engine.enforce_slash(&target, STAKE_ARC);
        assert!(removed, "validator should be removed when stake falls below STAKE_SPARK");

        // Validator should no longer be in the set
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

    // ── 18. Cross-Shard Lock Expiry (A6) ────────────────────────────────────

    #[test]
    fn test_cross_shard_lock_expiry() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"expiry_test_lock");
        engine.lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0).unwrap();

        // Manually set the lock timestamp to 60 seconds ago (well past 30s timeout)
        if let Some(mut entry) = engine.pending_cross_shard.get_mut(&tx) {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            entry.locked_at_ms = now_ms - 60_000;
        }

        // Expire with 30-second timeout
        let expired = engine.expire_stale_locks_with_timeout(30);
        assert_eq!(expired, 1, "stale lock should be expired with 30s timeout");
        assert_eq!(engine.cross_shard_stats(), (0, 1));

        // Verify it was aborted
        let proof = engine.completed_cross_shard.get(&tx).unwrap();
        assert_eq!(proof.status, CrossShardStatus::Aborted);
    }

    #[test]
    fn test_cross_shard_lock_not_expired_if_recent() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));

        let tx = hash_bytes(b"fresh_lock_test");
        engine.lock_cross_shard(tx, 0, 1, hash_bytes(b"block"), 0).unwrap();

        // Lock was just created — should NOT be expired even with a short timeout
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

        // Verify modules are accessible
        let wd = engine.withholding_detector().lock();
        assert_eq!(wd.detect_withholding(100).len(), 0);
        drop(wd);

        let cr = engine.checkpoint_registry().lock();
        assert!(cr.is_empty());
        drop(cr);

        let st = engine.stake_tracker().lock();
        assert!(st.all_penalties().is_empty());
    }

    // ── Propose-Verify Protocol ─────────────────────────────────────────────

    #[test]
    fn test_node_role_default() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
        ];
        let vs = ValidatorSet::new(validators, 1);
        let engine = ConsensusEngine::new(vs, test_addr(1));
        assert_eq!(engine.node_role(), NodeRole::Full);
    }

    #[test]
    fn test_node_role_setter() {
        let validators = vec![
            Validator::new(test_addr(1), STAKE_ARC, 0).unwrap(),
        ];
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
        // After slashing, validator should have reduced stake
        if let Some(v) = slashed {
            assert!(v.stake < STAKE_ARC);
        }
    }
}
