use serde::{Deserialize, Serialize};

pub const DEFAULT_TARGET_TXS: u64 = 5;
pub const DEFAULT_MAX_TXS: u64 = 10;
pub const DEFAULT_BASE_FEE: u64 = 100_000;
pub const DEFAULT_MIN_FEE: u64 = 10_000;
pub const RATE_LIMIT_BLOCKS: u64 = 10;

const FEE_ADJUST_NUM: u64 = 9;
const FEE_ADJUST_DEN: u64 = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceGasLane {
    pub base_fee: u64,
    pub target_txs: u64,
    pub max_txs: u64,
    pub min_fee: u64,
    pub current_block_count: u64,
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

    pub fn can_include(&self) -> bool {
        self.current_block_count < self.max_txs
    }

    pub fn current_fee(&self) -> u64 {
        self.base_fee
    }

    pub fn record_tx(&mut self) -> bool {
        if self.current_block_count >= self.max_txs {
            return false;
        }
        self.current_block_count += 1;
        true
    }

    pub fn end_block(&mut self, height: u64) {
        let count = self.current_block_count;

        if count > self.target_txs {
            self.base_fee = self.base_fee.saturating_mul(FEE_ADJUST_NUM) / FEE_ADJUST_DEN;
        } else if count < self.target_txs {
            self.base_fee = self.base_fee.saturating_mul(FEE_ADJUST_DEN) / FEE_ADJUST_NUM;
        }

        if self.base_fee < self.min_fee {
            self.base_fee = self.min_fee;
        }

        self.current_block_count = 0;
        self.last_update_height = height;
    }

    pub fn fee_after_full_blocks(initial_fee: u64, n: u32) -> u64 {
        let mut fee = initial_fee;
        for _ in 0..n {
            fee = fee.saturating_mul(FEE_ADJUST_NUM) / FEE_ADJUST_DEN;
        }
        fee
    }

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

        for _ in 0..10 {
            assert!(lane.record_tx());
        }
        lane.end_block(1);

        assert!(lane.base_fee > initial);
        assert_eq!(lane.base_fee, 112_500);
    }

    #[test]
    fn test_fee_decreases_when_empty() {
        let mut lane = InferenceGasLane::default();
        let initial = lane.base_fee;

        lane.end_block(1);

        assert!(lane.base_fee < initial);
        assert_eq!(lane.base_fee, 88_888);
    }

    #[test]
    fn test_fee_stable_at_target() {
        let mut lane = InferenceGasLane::default();
        let initial = lane.base_fee;

        for _ in 0..5 {
            lane.record_tx();
        }
        lane.end_block(1);

        assert_eq!(lane.base_fee, initial);
    }

    #[test]
    fn test_fee_floor_enforced() {
        let mut lane = InferenceGasLane::new(20_000, 5, 10);

        for h in 1..=100 {
            lane.end_block(h);
        }

        assert!(lane.base_fee >= lane.min_fee);
    }

    #[test]
    fn test_max_txs_enforced() {
        let mut lane = InferenceGasLane::default();

        for _ in 0..10 {
            assert!(lane.record_tx());
        }
        assert!(!lane.record_tx());
    }

    #[test]
    fn test_fee_escalation_dos_defense() {
        let fee_20 = InferenceGasLane::fee_after_full_blocks(100_000, 20);
        assert!(
            fee_20 > 1_000_000,
            "After 20 full blocks fee should be >10x: {fee_20}"
        );

        let fee_50 = InferenceGasLane::fee_after_full_blocks(100_000, 50);
        assert!(
            fee_50 > 20_000_000,
            "After 50 full blocks fee should be >200x: {fee_50}"
        );
    }

    #[test]
    fn test_rate_limit() {
        assert!(InferenceGasLane::check_rate_limit(85, 100));

        assert!(!InferenceGasLane::check_rate_limit(95, 100));

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
