//! EIP-1559 inference gas lane — separate fee market for inference transactions.
//!
//! Prevents inference DoS by maintaining a dedicated base fee that adjusts
//! per block based on inference transaction density:
//!
//! - Target: 5 inference TXs per block
//! - Maximum: 10 inference TXs per block
//! - Fee adjustment: ×1.125 per block above target, ÷1.125 below
//! - Per-address rate limit: 1 inference call per 10 blocks
//!
//! Combined with stake-gated access, sustained attack becomes economically
//! impossible: after 20 full blocks, fee is ~10.5× base; after 50, ~361× base.

use serde::{Deserialize, Serialize};

/// Default inference gas lane configuration.
pub const DEFAULT_TARGET_TXS: u64 = 5;
pub const DEFAULT_MAX_TXS: u64 = 10;
pub const DEFAULT_BASE_FEE: u64 = 100_000; // 100K base units
pub const DEFAULT_MIN_FEE: u64 = 10_000; // 10K floor
pub const RATE_LIMIT_BLOCKS: u64 = 10;

/// Multiplier numerator/denominator for fee adjustment.
/// 1.125 = 9/8
const FEE_ADJUST_NUM: u64 = 9;
const FEE_ADJUST_DEN: u64 = 8;

/// Inference gas lane state.
///
/// Maintained per-block in the chain state. The base fee adjusts every block
/// based on how many inference transactions were included.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceGasLane {
    /// Current inference base fee (in ARC base units).
    pub base_fee: u64,
    /// Target number of inference TXs per block.
    pub target_txs: u64,
    /// Maximum inference TXs allowed per block.
    pub max_txs: u64,
    /// Minimum base fee (floor).
    pub min_fee: u64,
    /// Number of inference TXs in the current block.
    pub current_block_count: u64,
    /// Last block height this was updated.
    pub last_update_height: u64,
}

impl Default for InferenceGasLane {
    fn default() -> Self {
        Self {
            base_fee: DEFAULT_BASE_FEE,
            target_txs: DEFAULT_TARGET_TXS,
            max_txs: DEFAULT_MAX_TXS,
            min_fee: DEFAULT_MIN_FEE,
            current_block_count: 0,
            last_update_height: 0,
        }
    }
}

impl InferenceGasLane {
    /// Create a new gas lane with custom parameters.
    pub fn new(base_fee: u64, target_txs: u64, max_txs: u64) -> Self {
        Self {
            base_fee,
            target_txs,
            max_txs,
            min_fee: DEFAULT_MIN_FEE,
            current_block_count: 0,
            last_update_height: 0,
        }
    }

    /// Check if another inference TX can be included in this block.
    pub fn can_include(&self) -> bool {
        self.current_block_count < self.max_txs
    }

    /// Get the current inference fee for a new transaction.
    pub fn current_fee(&self) -> u64 {
        self.base_fee
    }

    /// Record an inference TX in this block.
    ///
    /// Returns `false` if the block is full (hit max_txs).
    pub fn record_tx(&mut self) -> bool {
        if self.current_block_count >= self.max_txs {
            return false;
        }
        self.current_block_count += 1;
        true
    }

    /// Adjust base fee for the next block based on current block's usage.
    ///
    /// Called once per block after all transactions are processed.
    /// EIP-1559 algorithm:
    /// - If inference_txs > target: `fee = fee * 9/8` (increase 12.5%)
    /// - If inference_txs < target: `fee = fee * 8/9` (decrease 11.1%)
    /// - If inference_txs == target: no change
    pub fn end_block(&mut self, height: u64) {
        let count = self.current_block_count;

        if count > self.target_txs {
            // Above target: increase fee
            self.base_fee = self.base_fee.saturating_mul(FEE_ADJUST_NUM) / FEE_ADJUST_DEN;
        } else if count < self.target_txs {
            // Below target: decrease fee
            self.base_fee = self.base_fee.saturating_mul(FEE_ADJUST_DEN) / FEE_ADJUST_NUM;
        }
        // == target: no change

        // Enforce floor
        if self.base_fee < self.min_fee {
            self.base_fee = self.min_fee;
        }

        // Reset for next block
        self.current_block_count = 0;
        self.last_update_height = height;
    }

    /// Compute the fee after N consecutive full blocks (for DoS cost estimation).
    pub fn fee_after_full_blocks(initial_fee: u64, n: u32) -> u64 {
        let mut fee = initial_fee;
        for _ in 0..n {
            fee = fee.saturating_mul(FEE_ADJUST_NUM) / FEE_ADJUST_DEN;
        }
        fee
    }

    /// Check per-address rate limit.
    ///
    /// Returns `true` if the address is allowed to submit an inference TX
    /// at the given block height.
    pub fn check_rate_limit(last_inference_height: u64, current_height: u64) -> bool {
        current_height >= last_inference_height + RATE_LIMIT_BLOCKS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let lane = InferenceGasLane::default();
        assert_eq!(lane.base_fee, 100_000);
        assert_eq!(lane.target_txs, 5);
        assert_eq!(lane.max_txs, 10);
    }

    #[test]
    fn test_fee_increases_when_full() {
        let mut lane = InferenceGasLane::default();
        let initial = lane.base_fee;

        // Fill block above target
        for _ in 0..10 {
            assert!(lane.record_tx());
        }
        lane.end_block(1);

        // Fee should increase
        assert!(lane.base_fee > initial);
        // Exact: 100_000 * 9/8 = 112_500
        assert_eq!(lane.base_fee, 112_500);
    }

    #[test]
    fn test_fee_decreases_when_empty() {
        let mut lane = InferenceGasLane::default();
        let initial = lane.base_fee;

        // Empty block (0 inference TXs)
        lane.end_block(1);

        // Fee should decrease
        assert!(lane.base_fee < initial);
        // Exact: 100_000 * 8/9 = 88_888
        assert_eq!(lane.base_fee, 88_888);
    }

    #[test]
    fn test_fee_stable_at_target() {
        let mut lane = InferenceGasLane::default();
        let initial = lane.base_fee;

        // Exactly target TXs
        for _ in 0..5 {
            lane.record_tx();
        }
        lane.end_block(1);

        // Fee unchanged
        assert_eq!(lane.base_fee, initial);
    }

    #[test]
    fn test_fee_floor_enforced() {
        let mut lane = InferenceGasLane::new(20_000, 5, 10);

        // Many empty blocks to drive fee down
        for h in 1..=100 {
            lane.end_block(h);
        }

        // Should never go below min_fee
        assert!(lane.base_fee >= lane.min_fee);
    }

    #[test]
    fn test_max_txs_enforced() {
        let mut lane = InferenceGasLane::default();

        for _ in 0..10 {
            assert!(lane.record_tx());
        }
        // 11th should fail
        assert!(!lane.record_tx());
    }

    #[test]
    fn test_fee_escalation_dos_defense() {
        // After 20 full blocks: fee ~10.5x base
        let fee_20 = InferenceGasLane::fee_after_full_blocks(100_000, 20);
        assert!(
            fee_20 > 1_000_000,
            "After 20 full blocks fee should be >10x: {fee_20}"
        );

        // After 50 full blocks: fee ~361x base
        let fee_50 = InferenceGasLane::fee_after_full_blocks(100_000, 50);
        assert!(
            fee_50 > 20_000_000,
            "After 50 full blocks fee should be >200x: {fee_50}"
        );
    }

    #[test]
    fn test_rate_limit() {
        // At height 100, last inference was at height 85 → allowed (diff = 15 >= 10)
        assert!(InferenceGasLane::check_rate_limit(85, 100));

        // At height 100, last inference was at height 95 → blocked (diff = 5 < 10)
        assert!(!InferenceGasLane::check_rate_limit(95, 100));

        // At height 100, last inference was at height 90 → allowed (diff = 10 >= 10)
        assert!(InferenceGasLane::check_rate_limit(90, 100));
    }

    #[test]
    fn test_block_count_resets() {
        let mut lane = InferenceGasLane::default();
        lane.record_tx();
        lane.record_tx();
        assert_eq!(lane.current_block_count, 2);

        lane.end_block(1);
        assert_eq!(lane.current_block_count, 0);
    }
}
