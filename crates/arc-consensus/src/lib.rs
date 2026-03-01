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

use arc_crypto::Hash256;
use arc_types::{TxBody, Transaction};
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tracing::{debug, info};

pub mod data_availability;
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
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Verify that the stored hash matches the computed hash.
    pub fn verify_hash(&self) -> bool {
        self.hash == self.compute_hash()
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
    /// Data availability commitments.
    pub da_commitments: DashMap<Hash256, data_availability::DACommitment>,
    /// Pending cross-shard locks.
    pub pending_cross_shard: DashMap<Hash256, CrossShardProof>,
    /// Completed cross-shard proofs.
    pub completed_cross_shard: DashMap<Hash256, CrossShardProof>,
}

impl ConsensusEngine {
    /// Create a new consensus engine.
    ///
    /// # Arguments
    /// * `validator_set` - The initial validator set for this epoch.
    /// * `local_address` - This node's validator address.
    pub fn new(validator_set: ValidatorSet, local_address: Address) -> Self {
        info!(
            epoch = validator_set.epoch,
            validators = validator_set.len(),
            total_stake = validator_set.total_stake,
            quorum = validator_set.quorum,
            "ConsensusEngine initialized"
        );
        Self {
            dag: DashMap::new(),
            rounds: DashMap::new(),
            committed: RwLock::new(Vec::new()),
            current_round: AtomicU64::new(0),
            validator_set: RwLock::new(validator_set),
            local_address,
            da_commitments: DashMap::new(),
            pending_cross_shard: DashMap::new(),
            completed_cross_shard: DashMap::new(),
        }
    }

    /// Get the current round number.
    pub fn current_round(&self) -> u64 {
        self.current_round.load(Ordering::SeqCst)
    }

    /// Get a snapshot of the validator set.
    pub fn validator_set(&self) -> ValidatorSet {
        self.validator_set.read().clone()
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

            // Verify we have quorum-worth of parents
            if accumulated_stake < vs.quorum {
                return Err(ConsensusError::InsufficientParents);
            }

            selected_parents
        };

        drop(vs);

        // Build the block
        let mut block = DagBlock {
            author: self.local_address,
            round,
            parents,
            transactions,
            timestamp,
            hash: Hash256::ZERO,
            signature: Vec::new(), // Signature would come from the validator's key
        };
        block.hash = block.compute_hash();

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

        // 4. Round check: block cannot be too far in the future
        let current = self.current_round.load(Ordering::SeqCst);
        if block.round > current + 1 {
            return Err(ConsensusError::InvalidRound);
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
            // All parents must exist in our DAG and be from round - 1
            let expected_parent_round = block.round - 1;
            let mut parent_stake = 0u64;

            for parent_hash in &block.parents {
                match self.dag.get(parent_hash) {
                    Some(parent_block) => {
                        if parent_block.round != expected_parent_round {
                            return Err(ConsensusError::InvalidBlock(format!(
                                "parent {} is from round {}, expected round {}",
                                parent_hash, parent_block.round, expected_parent_round
                            )));
                        }
                        if let Some(validator) = vs.get_validator(&parent_block.author) {
                            parent_stake += validator.stake;
                        }
                    }
                    None => {
                        return Err(ConsensusError::InvalidBlock(format!(
                            "unknown parent block {}",
                            parent_hash
                        )));
                    }
                }
            }

            // 6. Need quorum-worth of parent stake
            if parent_stake < vs.quorum {
                return Err(ConsensusError::InsufficientParents);
            }
        }

        drop(vs);

        // All checks passed: insert
        self.insert_block_into_dag(block);

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

                        for block_d_hash in &round_r2_blocks {
                            if let Some(block_d) = self.dag.get(block_d_hash) {
                                if block_d.parents.contains(block_c_hash) {
                                    if let Some(validator) =
                                        vs.get_validator(&block_d.author)
                                    {
                                        supporting_stake += validator.stake;
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

        newly_committed
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
        let proof = CrossShardProof {
            tx_hash,
            source_shard,
            target_shard,
            source_block_hash,
            source_round,
            status: CrossShardStatus::Locked,
            lock_hash,
            inclusion_proof,
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
    fn make_block(
        author: Address,
        round: u64,
        parents: Vec<Hash256>,
        transactions: Vec<Hash256>,
        timestamp: u64,
    ) -> DagBlock {
        let mut block = DagBlock {
            author,
            round,
            parents,
            transactions,
            timestamp,
            hash: Hash256::ZERO,
            signature: Vec::new(),
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
    fn test_receive_block_future_round_rejected() {
        let vs = test_validator_set(4);
        let engine = ConsensusEngine::new(vs, test_addr(0));
        // Current round is 0, block in round 5 should be rejected (> current + 1)
        let block = make_block(test_addr(1), 5, vec![], vec![], 1000);
        let result = engine.receive_block(&block);
        assert_eq!(result.unwrap_err(), ConsensusError::InvalidRound);
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
}
