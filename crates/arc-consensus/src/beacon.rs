//! Beacon chain shard coordinator for hierarchical sharding.
//!
//! The beacon chain coordinates multiple independent shards, each running its
//! own DAG consensus. It collects state roots from all shards per epoch,
//! computes a global state root, manages shard assignments for validators,
//! and handles cross-shard settlement.

use arc_crypto::{hash_pair, Hash256};
use dashmap::DashMap;
use thiserror::Error;

/// Address is a 256-bit hash derived from a public key.
pub type Address = Hash256;

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BeaconError {
    #[error("invalid shard id {0}: must be < {1}")]
    InvalidShardId(u32, u32),

    #[error("epoch mismatch: expected {expected}, got {got}")]
    EpochMismatch { expected: u64, got: u64 },

    #[error("insufficient validator signatures")]
    InsufficientSignatures,
}

// ── Configuration ───────────────────────────────────────────────────────────

/// Beacon chain configuration.
#[derive(Debug, Clone)]
pub struct BeaconConfig {
    /// Number of active shards.
    pub num_shards: u32,
    /// Blocks per epoch (shard root collection interval).
    pub epoch_length: u64,
    /// Minimum validators per shard for liveness.
    pub min_validators_per_shard: u32,
}

// ── Attestation ─────────────────────────────────────────────────────────────

/// A shard's state attestation submitted to the beacon chain.
#[derive(Debug, Clone)]
pub struct ShardAttestation {
    pub shard_id: u32,
    pub epoch: u64,
    pub state_root: Hash256,
    pub block_height: u64,
    pub tx_count: u64,
    pub proposer: Address,
    pub validator_signatures: Vec<(Address, Vec<u8>)>,
}

// ── Epoch Summary ───────────────────────────────────────────────────────────

/// Summary returned after advancing an epoch.
#[derive(Debug, Clone)]
pub struct EpochSummary {
    pub epoch: u64,
    pub shard_count: u32,
    pub total_txs: u64,
    pub global_root: Hash256,
}

// ── Beacon State ────────────────────────────────────────────────────────────

/// The beacon chain state.
pub struct BeaconState {
    pub config: BeaconConfig,
    pub current_epoch: u64,
    /// Latest attestation per shard.
    pub shard_roots: DashMap<u32, ShardAttestation>,
    /// Global state root = Merkle(shard_0_root, shard_1_root, ...).
    pub global_root: Hash256,
    /// Validator -> shard assignment.
    pub shard_assignments: DashMap<Address, u32>,
    /// Shard -> list of validators.
    pub shard_validators: DashMap<u32, Vec<Address>>,
}

impl BeaconState {
    /// Create a new beacon state from the given configuration.
    pub fn new(config: BeaconConfig) -> Self {
        let shard_validators = DashMap::new();
        for i in 0..config.num_shards {
            shard_validators.insert(i, Vec::new());
        }

        Self {
            config,
            current_epoch: 0,
            shard_roots: DashMap::new(),
            global_root: Hash256::ZERO,
            shard_assignments: DashMap::new(),
            shard_validators,
        }
    }

    /// Submit a shard attestation to the beacon chain.
    ///
    /// Validates that the shard_id is in range and the epoch matches the current epoch,
    /// then stores the attestation.
    pub fn submit_attestation(&self, att: ShardAttestation) -> Result<(), BeaconError> {
        if att.shard_id >= self.config.num_shards {
            return Err(BeaconError::InvalidShardId(
                att.shard_id,
                self.config.num_shards,
            ));
        }

        if att.epoch != self.current_epoch {
            return Err(BeaconError::EpochMismatch {
                expected: self.current_epoch,
                got: att.epoch,
            });
        }

        self.shard_roots.insert(att.shard_id, att);
        Ok(())
    }

    /// Compute the global state root as a Merkle root of all shard state roots.
    ///
    /// Shards without an attestation contribute `Hash256::ZERO`.
    /// Uses BLAKE3-based hashing via `arc_crypto::hash_pair`.
    pub fn compute_global_root(&self) -> Hash256 {
        let num_shards = self.config.num_shards;
        if num_shards == 0 {
            return Hash256::ZERO;
        }

        // Collect shard roots in order.
        let mut leaves: Vec<Hash256> = Vec::with_capacity(num_shards as usize);
        for i in 0..num_shards {
            let root = self
                .shard_roots
                .get(&i)
                .map(|att| att.state_root)
                .unwrap_or(Hash256::ZERO);
            leaves.push(root);
        }

        // Pad to next power of two for a balanced Merkle tree.
        let mut size = leaves.len().next_power_of_two();
        leaves.resize(size, Hash256::ZERO);

        // Iteratively compute Merkle root.
        while size > 1 {
            let mut next = Vec::with_capacity(size / 2);
            for pair in leaves.chunks(2) {
                next.push(hash_pair(&pair[0], &pair[1]));
            }
            leaves = next;
            size /= 2;
        }

        leaves[0]
    }

    /// Advance to the next epoch.
    ///
    /// Computes the global root, increments the epoch counter, clears shard
    /// attestations, and returns a summary of the completed epoch.
    pub fn advance_epoch(&mut self) -> EpochSummary {
        let global_root = self.compute_global_root();
        self.global_root = global_root;

        let shard_count = self.shard_roots.len() as u32;
        let total_txs: u64 = self
            .shard_roots
            .iter()
            .map(|entry| entry.value().tx_count)
            .sum();

        let epoch = self.current_epoch;
        self.current_epoch += 1;
        self.shard_roots.clear();

        EpochSummary {
            epoch,
            shard_count,
            total_txs,
            global_root,
        }
    }

    /// Assign a validator to a shard, picking the shard with the fewest validators.
    ///
    /// Returns the assigned shard_id.
    pub fn assign_validator(&self, validator: Address, _stake: u64) -> u32 {
        // Find the shard with the fewest validators.
        let mut min_shard = 0u32;
        let mut min_count = u64::MAX;

        for i in 0..self.config.num_shards {
            let count = self
                .shard_validators
                .get(&i)
                .map(|v| v.len() as u64)
                .unwrap_or(0);
            if count < min_count {
                min_count = count;
                min_shard = i;
            }
        }

        // Record the assignment.
        self.shard_assignments.insert(validator, min_shard);
        self.shard_validators
            .entry(min_shard)
            .or_default()
            .push(validator);

        min_shard
    }

    /// Deterministically map an account address to a shard.
    ///
    /// Uses the first 4 bytes of the address interpreted as a big-endian u32,
    /// modulo the number of shards.
    pub fn get_shard_for_account(&self, address: &Address) -> u32 {
        let bytes = address.as_bytes();
        let val = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        val % self.config.num_shards
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn test_config() -> BeaconConfig {
        BeaconConfig {
            num_shards: 4,
            epoch_length: 100,
            min_validators_per_shard: 2,
        }
    }

    fn make_address(seed: u8) -> Address {
        hash_bytes(&[seed])
    }

    fn make_attestation(shard_id: u32, epoch: u64, tx_count: u64) -> ShardAttestation {
        ShardAttestation {
            shard_id,
            epoch,
            state_root: hash_bytes(&[shard_id as u8, epoch as u8]),
            block_height: epoch * 100 + shard_id as u64,
            tx_count,
            proposer: make_address(shard_id as u8),
            validator_signatures: vec![],
        }
    }

    #[test]
    fn test_submit_attestation_valid() {
        let state = BeaconState::new(test_config());

        let att = make_attestation(0, 0, 500);
        assert!(state.submit_attestation(att).is_ok());
        assert!(state.shard_roots.contains_key(&0));
    }

    #[test]
    fn test_submit_attestation_invalid_shard() {
        let state = BeaconState::new(test_config());

        let att = make_attestation(99, 0, 100);
        let err = state.submit_attestation(att).unwrap_err();
        assert_eq!(err, BeaconError::InvalidShardId(99, 4));
    }

    #[test]
    fn test_submit_attestation_wrong_epoch() {
        let state = BeaconState::new(test_config());

        let att = make_attestation(0, 5, 100);
        let err = state.submit_attestation(att).unwrap_err();
        assert_eq!(
            err,
            BeaconError::EpochMismatch {
                expected: 0,
                got: 5
            }
        );
    }

    #[test]
    fn test_global_root_deterministic() {
        let state = BeaconState::new(test_config());

        // Submit attestations for all 4 shards.
        for i in 0..4 {
            state
                .submit_attestation(make_attestation(i, 0, 100))
                .unwrap();
        }

        let root1 = state.compute_global_root();
        let root2 = state.compute_global_root();
        assert_eq!(root1, root2, "global root must be deterministic");
        assert_ne!(root1, Hash256::ZERO, "root should not be zero with attestations");
    }

    #[test]
    fn test_global_root_changes_with_different_attestations() {
        let state = BeaconState::new(test_config());

        // Only shard 0.
        state
            .submit_attestation(make_attestation(0, 0, 100))
            .unwrap();
        let root_partial = state.compute_global_root();

        // Now add shard 1.
        state
            .submit_attestation(make_attestation(1, 0, 200))
            .unwrap();
        let root_more = state.compute_global_root();

        assert_ne!(
            root_partial, root_more,
            "different attestation sets must produce different roots"
        );
    }

    #[test]
    fn test_validator_assignment_load_balancing() {
        let state = BeaconState::new(test_config());

        // Assign 8 validators; should spread evenly across 4 shards.
        let mut shard_counts = [0u32; 4];
        for i in 0..8u8 {
            let addr = make_address(i);
            let shard = state.assign_validator(addr, 1_000_000);
            shard_counts[shard as usize] += 1;
        }

        // Each shard should have exactly 2 validators.
        for (i, &count) in shard_counts.iter().enumerate() {
            assert_eq!(
                count, 2,
                "shard {} should have 2 validators, got {}",
                i, count
            );
        }
    }

    #[test]
    fn test_account_to_shard_deterministic() {
        let state = BeaconState::new(test_config());

        let addr = make_address(42);
        let shard1 = state.get_shard_for_account(&addr);
        let shard2 = state.get_shard_for_account(&addr);
        assert_eq!(shard1, shard2, "shard mapping must be deterministic");
        assert!(shard1 < 4, "shard id must be in range");
    }

    #[test]
    fn test_account_to_shard_range() {
        let config = BeaconConfig {
            num_shards: 16,
            epoch_length: 100,
            min_validators_per_shard: 1,
        };
        let state = BeaconState::new(config);

        for i in 0..=255u8 {
            let addr = make_address(i);
            let shard = state.get_shard_for_account(&addr);
            assert!(shard < 16, "shard {} out of range for address seed {}", shard, i);
        }
    }

    #[test]
    fn test_epoch_advancement() {
        let mut state = BeaconState::new(test_config());
        assert_eq!(state.current_epoch, 0);

        // Submit attestations with known tx counts.
        state
            .submit_attestation(make_attestation(0, 0, 1000))
            .unwrap();
        state
            .submit_attestation(make_attestation(1, 0, 2000))
            .unwrap();
        state
            .submit_attestation(make_attestation(2, 0, 3000))
            .unwrap();

        let summary = state.advance_epoch();
        assert_eq!(summary.epoch, 0);
        assert_eq!(summary.shard_count, 3);
        assert_eq!(summary.total_txs, 6000);
        assert_ne!(summary.global_root, Hash256::ZERO);
        assert_eq!(state.current_epoch, 1);

        // Shard roots should be cleared after epoch advance.
        assert!(state.shard_roots.is_empty());
    }

    #[test]
    fn test_epoch_advancement_updates_global_root() {
        let mut state = BeaconState::new(test_config());
        assert_eq!(state.global_root, Hash256::ZERO);

        state
            .submit_attestation(make_attestation(0, 0, 500))
            .unwrap();
        let _summary = state.advance_epoch();

        assert_ne!(state.global_root, Hash256::ZERO);
    }

    #[test]
    fn test_multiple_epochs() {
        let mut state = BeaconState::new(test_config());

        // Epoch 0
        state
            .submit_attestation(make_attestation(0, 0, 100))
            .unwrap();
        let s0 = state.advance_epoch();
        assert_eq!(s0.epoch, 0);

        // Epoch 1 — attestations must use epoch 1 now.
        state
            .submit_attestation(make_attestation(0, 1, 200))
            .unwrap();
        state
            .submit_attestation(make_attestation(1, 1, 300))
            .unwrap();
        let s1 = state.advance_epoch();
        assert_eq!(s1.epoch, 1);
        assert_eq!(s1.total_txs, 500);
        assert_eq!(state.current_epoch, 2);
    }
}
