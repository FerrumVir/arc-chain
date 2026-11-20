use serde::{Deserialize, Serialize};

use crate::account::Address;

pub const TOTAL_SUPPLY: u128 = 1_030_000_000_000_000_000;
pub const DECIMALS: u8 = 9;

pub const ARC_BASE_UNITS: u64 = 10u64.pow(DECIMALS as u32);

pub const INFERENCE_ATTESTATION_REWARD: u64 = 2_500_000_000;

pub fn inference_attestation_reward_arc() -> f64 {
    INFERENCE_ATTESTATION_REWARD as f64 / ARC_BASE_UNITS as f64
}

pub const BLOCKS_PER_YEAR: u64 = 78_840_000;

pub const MAX_ANNUAL_INFLATION_BPS: u16 = 300;

pub const BURN_ENABLED: bool = false;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidatorRole {
    Proposer,
    Verifier,
    Observer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleRevenueConfig {
    pub proposer_share_bps: u16,
    pub verifier_share_bps: u16,
    pub observer_pool_bps: u16,
    pub treasury_share_bps: u16,
}

impl Default for RoleRevenueConfig {
    fn default() -> Self {
        Self {
            proposer_share_bps: 4000,
            verifier_share_bps: 2500,
            observer_pool_bps: 1500,
            treasury_share_bps: 2000,
        }
    }
}

impl RoleRevenueConfig {
    pub fn split_fee(&self, total: u64, num_verifiers: u32) -> FeeSplit {
        let proposer = (total as u128 * self.proposer_share_bps as u128 / 10_000) as u64;
        let verifier_total = (total as u128 * self.verifier_share_bps as u128 / 10_000) as u64;
        let per_verifier = if num_verifiers > 0 {
            verifier_total / num_verifiers as u64
        } else {
            0
        };
        let observer_pool = (total as u128 * self.observer_pool_bps as u128 / 10_000) as u64;
        let treasury = (total as u128 * self.treasury_share_bps as u128 / 10_000) as u64;
        FeeSplit {
            proposer,
            per_verifier,
            observer_pool,
            treasury,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeeSplit {
    pub proposer: u64,
    pub per_verifier: u64,
    pub observer_pool: u64,
    pub treasury: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapFund {
    pub total_allocation: u128,
    pub distributed: u128,
    pub vesting_start_block: u64,
    pub vesting_duration_blocks: u64,
    pub cliff_blocks: u64,
}

impl BootstrapFund {
    pub fn new_two_year(total_allocation: u128, start_block: u64) -> Self {
        Self {
            total_allocation,
            distributed: 0,
            vesting_start_block: start_block,
            vesting_duration_blocks: BLOCKS_PER_YEAR * 2,
            cliff_blocks: 1_512_000,
        }
    }

    pub fn vested_amount(&self, current_block: u64) -> u128 {
        if current_block < self.vesting_start_block + self.cliff_blocks {
            return 0;
        }
        let elapsed = current_block.saturating_sub(self.vesting_start_block);
        if elapsed >= self.vesting_duration_blocks {
            return self.total_allocation;
        }
        self.total_allocation * elapsed as u128 / self.vesting_duration_blocks as u128
    }

    pub fn claimable(&self, current_block: u64) -> u128 {
        self.vested_amount(current_block)
            .saturating_sub(self.distributed)
    }

    pub fn per_block_amount(&self) -> u128 {
        if self.vesting_duration_blocks == 0 {
            return 0;
        }
        self.total_allocation / self.vesting_duration_blocks as u128
    }
}

pub const MIN_STAKE_LITE: u64 = 50_000_000_000_000;
pub const MIN_STAKE_SPARK: u64 = 500_000_000_000_000;
pub const MIN_STAKE_ARC: u64 = 5_000_000_000_000_000;
pub const MIN_STAKE_CORE: u64 = 50_000_000_000_000_000;

pub const APY_LITE: u16 = 500;
pub const APY_SPARK: u16 = 800;
pub const APY_ARC: u16 = 1500;
pub const APY_CORE: u16 = 2500;

pub const UNBONDING_LITE: u64 = 216_000;
pub const UNBONDING_SPARK: u64 = 1_512_000;
pub const UNBONDING_ARC: u64 = 3_024_000;
pub const UNBONDING_CORE: u64 = 6_480_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StakeTier {
    None,
    Lite,
    Spark,
    Arc,
    Core,
}

impl StakeTier {
    pub fn from_amount(amount: u64) -> Self {
        if amount >= MIN_STAKE_CORE {
            StakeTier::Core
        } else if amount >= MIN_STAKE_ARC {
            StakeTier::Arc
        } else if amount >= MIN_STAKE_SPARK {
            StakeTier::Spark
        } else if amount >= MIN_STAKE_LITE {
            StakeTier::Lite
        } else {
            StakeTier::None
        }
    }

    pub fn min_stake(&self) -> u64 {
        match self {
            StakeTier::None => 0,
            StakeTier::Lite => MIN_STAKE_LITE,
            StakeTier::Spark => MIN_STAKE_SPARK,
            StakeTier::Arc => MIN_STAKE_ARC,
            StakeTier::Core => MIN_STAKE_CORE,
        }
    }

    pub fn apy_bps(&self) -> u16 {
        match self {
            StakeTier::None => 0,
            StakeTier::Lite => APY_LITE,
            StakeTier::Spark => APY_SPARK,
            StakeTier::Arc => APY_ARC,
            StakeTier::Core => APY_CORE,
        }
    }

    pub fn unbonding_period(&self) -> u64 {
        match self {
            StakeTier::None => 0,
            StakeTier::Lite => UNBONDING_LITE,
            StakeTier::Spark => UNBONDING_SPARK,
            StakeTier::Arc => UNBONDING_ARC,
            StakeTier::Core => UNBONDING_CORE,
        }
    }

    pub fn can_propose(&self) -> bool {
        matches!(self, StakeTier::Arc | StakeTier::Core)
    }

    pub fn can_vote(&self) -> bool {
        matches!(self, StakeTier::Spark | StakeTier::Arc | StakeTier::Core)
    }

    pub fn can_govern(&self) -> bool {
        matches!(self, StakeTier::Core)
    }

    pub fn name(&self) -> &'static str {
        match self {
            StakeTier::None => "None",
            StakeTier::Lite => "Lite",
            StakeTier::Spark => "Spark",
            StakeTier::Arc => "Arc",
            StakeTier::Core => "Core",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StakePosition {
    pub staker: Address,
    pub amount: u64,
    pub tier: StakeTier,
    pub staked_at_height: u64,
    pub last_reward_height: u64,
    pub accumulated_rewards: u64,
    pub is_active: bool,
    pub unbonding_height: Option<u64>,
}

impl StakePosition {
    pub fn new(staker: Address, amount: u64, height: u64) -> Self {
        Self {
            staker,
            amount,
            tier: StakeTier::from_amount(amount),
            staked_at_height: height,
            last_reward_height: height,
            accumulated_rewards: 0,
            is_active: true,
            unbonding_height: None,
        }
    }

    pub fn calculate_pending_rewards(&self, current_height: u64, blocks_per_year: u64) -> u64 {
        if !self.is_active || current_height <= self.last_reward_height || blocks_per_year == 0 {
            return 0;
        }
        let elapsed = current_height - self.last_reward_height;
        let apy = self.tier.apy_bps() as u128;
        let amt = self.amount as u128;
        let reward = amt
            .checked_mul(apy)
            .and_then(|v| v.checked_mul(elapsed as u128))
            .map(|v| v / (10_000u128 * blocks_per_year as u128))
            .unwrap_or(0);
        reward as u64
    }

    pub fn claim_rewards(&mut self, current_height: u64, blocks_per_year: u64) -> u64 {
        let pending = self.calculate_pending_rewards(current_height, blocks_per_year);
        self.accumulated_rewards = self.accumulated_rewards.saturating_add(pending);
        self.last_reward_height = current_height;
        pending
    }

    pub fn begin_unbonding(&mut self, current_height: u64) {
        self.is_active = false;
        self.unbonding_height = Some(current_height);
    }

    pub fn can_withdraw(&self, current_height: u64) -> bool {
        match self.unbonding_height {
            Some(start) => {
                let period = self.tier.unbonding_period();
                current_height >= start.saturating_add(period)
            }
            None => false,
        }
    }

    pub fn increase_stake(&mut self, additional: u64) {
        self.amount = self.amount.saturating_add(additional);
        self.tier = StakeTier::from_amount(self.amount);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeConfig {
    pub base_fee: u64,
    pub min_base_fee: u64,
    pub max_base_fee: u64,
    pub target_block_utilization: f64,
    pub adjustment_speed: u64,
    pub burn_percentage: u16,
    pub proposer_percentage: u16,
    #[serde(default)]
    pub smoothed_tps: f64,
    #[serde(default = "default_circulating_supply")]
    pub circulating_supply: u128,
    #[serde(default = "default_target_burn_bps")]
    pub target_annual_burn_bps: u16,
    #[serde(default)]
    pub role_revenue: RoleRevenueConfig,
}

fn default_circulating_supply() -> u128 {
    TOTAL_SUPPLY
}
fn default_target_burn_bps() -> u16 {
    0
}

#[derive(Debug, Clone)]
pub struct FeeBreakdown {
    pub base_fee: u64,
    pub priority_fee: u64,
    pub total_fee: u64,
    pub to_proposer: u64,
    pub to_verifiers: u64,
    pub to_observer_pool: u64,
    pub to_treasury: u64,
    pub is_free_settlement: bool,
}

impl FeeConfig {
    pub fn default_config() -> Self {
        Self {
            base_fee: 1_000,
            min_base_fee: 1,
            max_base_fee: 1_000_000_000,
            target_block_utilization: 0.5,
            adjustment_speed: 8,
            burn_percentage: 0,
            proposer_percentage: 10000,
            smoothed_tps: 0.0,
            circulating_supply: TOTAL_SUPPLY,
            target_annual_burn_bps: 0,
            role_revenue: RoleRevenueConfig::default(),
        }
    }

    pub fn is_free_settlement(tx_type: &str) -> bool {
        tx_type == "Settle"
    }

    pub fn calculate_fee(&self, gas_used: u64, priority_fee: u64, tx_type: &str) -> FeeBreakdown {
        if Self::is_free_settlement(tx_type) {
            return FeeBreakdown {
                base_fee: 0,
                priority_fee: 0,
                total_fee: 0,
                to_proposer: 0,
                to_verifiers: 0,
                to_observer_pool: 0,
                to_treasury: 0,
                is_free_settlement: true,
            };
        }

        let base_component = self.base_fee.saturating_mul(gas_used);
        let priority_component = priority_fee.saturating_mul(gas_used);
        let total = base_component.saturating_add(priority_component);

        let to_proposer =
            (total as u128 * self.role_revenue.proposer_share_bps as u128 / 10_000) as u64;
        let to_verifiers =
            (total as u128 * self.role_revenue.verifier_share_bps as u128 / 10_000) as u64;
        let to_observer_pool =
            (total as u128 * self.role_revenue.observer_pool_bps as u128 / 10_000) as u64;
        let to_treasury =
            (total as u128 * self.role_revenue.treasury_share_bps as u128 / 10_000) as u64;

        FeeBreakdown {
            base_fee: base_component,
            priority_fee: priority_component,
            total_fee: total,
            to_proposer,
            to_verifiers,
            to_observer_pool,
            to_treasury,
            is_free_settlement: false,
        }
    }

    pub fn adjust_base_fee(&mut self, block_utilization: f64) {
        if self.adjustment_speed == 0 {
            return;
        }

        let delta = block_utilization - self.target_block_utilization;
        let max_change = self.base_fee / self.adjustment_speed;

        let scale = if delta >= 0.0 {
            let denom = 1.0 - self.target_block_utilization;
            if denom <= 0.0 {
                1.0
            } else {
                (delta / denom).min(1.0)
            }
        } else {
            let denom = self.target_block_utilization;
            if denom <= 0.0 {
                -1.0
            } else {
                (delta / denom).max(-1.0)
            }
        };

        let change = (max_change as f64 * scale) as i64;

        let new_fee = if change >= 0 {
            self.base_fee.saturating_add(change as u64)
        } else {
            self.base_fee.saturating_sub(change.unsigned_abs())
        };

        self.base_fee = new_fee.clamp(self.min_base_fee, self.max_base_fee);
    }

    pub fn adjust_for_tps(&mut self, block_tx_count: u64, block_time_secs: f64) {
        if block_time_secs <= 0.0 {
            return;
        }

        let instant_tps = block_tx_count as f64 / block_time_secs;
        self.smoothed_tps = self.smoothed_tps * 0.9 + instant_tps * 0.1;

        if self.smoothed_tps < 100.0 {
            return;
        }

        let target_annual_fees = self.circulating_supply * 500 / 10_000;
        let target_fees_per_block = target_annual_fees / BLOCKS_PER_YEAR as u128;

        if target_fees_per_block == 0 {
            return;
        }

        let avg_gas: u128 = 21_000;
        let txs_per_block = (self.smoothed_tps * block_time_secs) as u128;
        let actual_fees_per_block = (self.base_fee as u128)
            .saturating_mul(avg_gas)
            .saturating_mul(txs_per_block);

        if actual_fees_per_block == 0 {
            return;
        }

        let ratio = target_fees_per_block as f64 / actual_fees_per_block as f64;
        let clamped_ratio = ratio.clamp(0.5, 2.0);
        let new_fee = ((self.base_fee as f64) * clamped_ratio) as u64;

        self.base_fee = new_fee.clamp(self.min_base_fee, self.max_base_fee);
    }

    pub fn update_supply(&mut self, circulating: u128) {
        self.circulating_supply = circulating;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRentConfig {
    pub cost_per_byte_per_epoch: u64,
    pub epoch_length_blocks: u64,
    pub dust_threshold: u64,
    pub grace_epochs: u64,
    pub account_size_bytes: u64,
}

impl Default for StateRentConfig {
    fn default() -> Self {
        Self {
            cost_per_byte_per_epoch: 1,
            epoch_length_blocks: 216_000,
            dust_threshold: 1_000_000,
            grace_epochs: 30,
            account_size_bytes: 128,
        }
    }
}

impl StateRentConfig {
    pub fn rent_per_epoch(&self) -> u64 {
        self.cost_per_byte_per_epoch
            .saturating_mul(self.account_size_bytes)
    }

    pub fn is_dormant(&self, balance: u64) -> bool {
        balance < self.dust_threshold
    }

    pub fn epochs_until_archive(&self, balance: u64) -> u64 {
        let rent = self.rent_per_epoch();
        if rent == 0 {
            return u64::MAX;
        }
        let usable = balance.saturating_sub(self.dust_threshold);
        usable / rent
    }
}

#[derive(Debug, Clone)]
pub struct BlockReward {
    pub block_height: u64,
    pub proposer: Address,
    pub base_fee_revenue: u64,
    pub priority_fee_revenue: u64,
    pub proposer_reward: u64,
    pub verifier_reward: u64,
    pub observer_pool_reward: u64,
    pub treasury_reward: u64,
    pub staking_rewards_distributed: u64,
    pub tx_count: u32,
    pub free_settlement_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupplyTracker {
    pub total_supply: u128,
    pub total_staked: u128,
    pub bootstrap_locked: u128,
    pub treasury_balance: u128,
    pub total_fees_distributed: u128,
    pub total_tax_revenue: u128,
    pub current_block: u64,
}

impl Default for SupplyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SupplyTracker {
    pub fn new() -> Self {
        Self {
            total_supply: TOTAL_SUPPLY,
            total_staked: 0,
            bootstrap_locked: 0,
            treasury_balance: 0,
            total_fees_distributed: 0,
            total_tax_revenue: 0,
            current_block: 0,
        }
    }

    pub fn stake(&mut self, amount: u128) {
        self.total_staked = self.total_staked.saturating_add(amount);
    }

    pub fn unstake(&mut self, amount: u128) {
        self.total_staked = self.total_staked.saturating_sub(amount);
    }

    pub fn record_fees(&mut self, amount: u128) {
        self.total_fees_distributed = self.total_fees_distributed.saturating_add(amount);
    }

    pub fn record_tax_revenue(&mut self, amount: u128) {
        self.total_tax_revenue = self.total_tax_revenue.saturating_add(amount);
    }

    pub fn circulating_supply(&self) -> u128 {
        self.total_supply
            .saturating_sub(self.total_staked)
            .saturating_sub(self.bootstrap_locked)
            .saturating_sub(self.treasury_balance)
    }

    pub fn staking_ratio(&self) -> f64 {
        if self.total_supply == 0 {
            return 0.0;
        }
        self.total_staked as f64 / self.total_supply as f64 * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::Hash256;

    fn test_addr(n: u8) -> Address {
        let mut bytes = [0u8; 32];
        bytes[0] = n;
        Hash256(bytes)
    }

    #[test]
    fn test_stake_tier_from_amount() {
        assert_eq!(StakeTier::from_amount(0), StakeTier::None);
        assert_eq!(StakeTier::from_amount(10_000_000_000_000), StakeTier::None);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_LITE), StakeTier::Lite);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_SPARK), StakeTier::Spark);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_ARC), StakeTier::Arc);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_CORE), StakeTier::Core);
        assert_eq!(StakeTier::from_amount(200_000_000_000_000), StakeTier::Lite);
        assert_eq!(
            StakeTier::from_amount(1_000_000_000_000_000),
            StakeTier::Spark
        );
    }

    #[test]
    fn test_stake_tier_boundaries() {
        assert_eq!(StakeTier::from_amount(MIN_STAKE_LITE - 1), StakeTier::None);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_SPARK - 1), StakeTier::Lite);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_ARC - 1), StakeTier::Spark);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_CORE - 1), StakeTier::Arc);

        assert_eq!(StakeTier::from_amount(MIN_STAKE_LITE), StakeTier::Lite);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_SPARK), StakeTier::Spark);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_ARC), StakeTier::Arc);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_CORE), StakeTier::Core);
    }

    #[test]
    fn test_tier_permissions() {
        assert!(!StakeTier::None.can_propose());
        assert!(!StakeTier::None.can_vote());
        assert!(!StakeTier::None.can_govern());

        assert!(!StakeTier::Lite.can_propose());
        assert!(!StakeTier::Lite.can_vote());
        assert!(!StakeTier::Lite.can_govern());

        assert!(!StakeTier::Spark.can_propose());
        assert!(StakeTier::Spark.can_vote());
        assert!(!StakeTier::Spark.can_govern());

        assert!(StakeTier::Arc.can_propose());
        assert!(StakeTier::Arc.can_vote());
        assert!(!StakeTier::Arc.can_govern());

        assert!(StakeTier::Core.can_propose());
        assert!(StakeTier::Core.can_vote());
        assert!(StakeTier::Core.can_govern());
    }

    #[test]
    fn test_stake_rewards_calculation() {
        let blocks_per_year: u64 = 78_840_000;
        let staker = test_addr(1);

        let pos = StakePosition::new(staker, MIN_STAKE_ARC, 0);
        assert_eq!(pos.tier, StakeTier::Arc);

        let reward = pos.calculate_pending_rewards(blocks_per_year, blocks_per_year);
        let expected = (MIN_STAKE_ARC as u128 * APY_ARC as u128 / 10_000) as u64;
        assert_eq!(
            reward, expected,
            "One full year of Arc staking should yield 15% APY"
        );

        let half_year = blocks_per_year / 2;
        let half_reward = pos.calculate_pending_rewards(half_year, blocks_per_year);
        let expected_half = expected / 2;
        assert!(
            half_reward == expected_half || half_reward == expected_half + 1,
            "Half year reward {} should be close to {}",
            half_reward,
            expected_half
        );
    }

    #[test]
    fn test_claim_rewards() {
        let blocks_per_year: u64 = 78_840_000;
        let staker = test_addr(1);

        let mut pos = StakePosition::new(staker, MIN_STAKE_SPARK, 0);
        assert_eq!(pos.tier, StakeTier::Spark);

        let claimed = pos.claim_rewards(1_000_000, blocks_per_year);
        assert!(claimed > 0, "Should have earned some rewards");
        assert_eq!(pos.accumulated_rewards, claimed);
        assert_eq!(pos.last_reward_height, 1_000_000);

        let second = pos.claim_rewards(1_000_000, blocks_per_year);
        assert_eq!(second, 0, "No new blocks = no new rewards");
        assert_eq!(
            pos.accumulated_rewards, claimed,
            "Accumulated should not change"
        );

        let third = pos.claim_rewards(2_000_000, blocks_per_year);
        assert!(third > 0);
        assert_eq!(pos.accumulated_rewards, claimed + third);
    }

    #[test]
    fn test_unbonding_period() {
        let staker = test_addr(1);
        let mut pos = StakePosition::new(staker, MIN_STAKE_ARC, 100);
        assert!(pos.is_active);
        assert!(!pos.can_withdraw(1_000_000));

        pos.begin_unbonding(1_000);
        assert!(!pos.is_active);
        assert_eq!(pos.unbonding_height, Some(1_000));

        assert!(
            !pos.can_withdraw(1_000 + UNBONDING_ARC - 1),
            "Should not withdraw one block early"
        );
        assert!(
            pos.can_withdraw(1_000 + UNBONDING_ARC),
            "Should withdraw at exact end of period"
        );
        assert!(pos.can_withdraw(1_000 + UNBONDING_ARC + 1_000));
    }

    #[test]
    fn test_fee_calculation_normal_tx() {
        let config = FeeConfig::default_config();
        let gas_used = 21_000;
        let priority_fee = 500;

        let fee = config.calculate_fee(gas_used, priority_fee, "Transfer");

        assert!(!fee.is_free_settlement);
        assert_eq!(fee.base_fee, config.base_fee * gas_used);
        assert_eq!(fee.priority_fee, priority_fee * gas_used);
        assert_eq!(fee.total_fee, fee.base_fee + fee.priority_fee);

        let total_distributed =
            fee.to_proposer + fee.to_verifiers + fee.to_observer_pool + fee.to_treasury;
        assert!(
            (total_distributed as i64 - fee.total_fee as i64).abs() <= 4,
            "distributed {} should ≈ total_fee {}",
            total_distributed,
            fee.total_fee
        );

        let expected_proposer = (fee.total_fee as u128 * 4000 / 10_000) as u64;
        assert_eq!(fee.to_proposer, expected_proposer);

        let expected_treasury = (fee.total_fee as u128 * 2000 / 10_000) as u64;
        assert_eq!(fee.to_treasury, expected_treasury);
    }

    #[test]
    fn test_fee_calculation_free_settlement() {
        let config = FeeConfig::default_config();
        let fee = config.calculate_fee(50_000, 1_000, "Settle");

        assert!(fee.is_free_settlement);
        assert_eq!(fee.total_fee, 0);
        assert_eq!(fee.to_proposer, 0);
        assert_eq!(fee.to_treasury, 0);
    }

    #[test]
    fn test_base_fee_adjustment_up() {
        let mut config = FeeConfig::default_config();
        let original = config.base_fee;

        config.adjust_base_fee(0.8);
        assert!(
            config.base_fee > original,
            "Base fee should increase: {} > {}",
            config.base_fee,
            original
        );
    }

    #[test]
    fn test_base_fee_adjustment_down() {
        let mut config = FeeConfig::default_config();
        let original = config.base_fee;

        config.adjust_base_fee(0.2);
        assert!(
            config.base_fee < original,
            "Base fee should decrease: {} < {}",
            config.base_fee,
            original
        );
    }

    #[test]
    fn test_supply_tracker_staking() {
        let mut tracker = SupplyTracker::new();
        assert_eq!(tracker.circulating_supply(), TOTAL_SUPPLY);

        let stake_amount: u128 = 5_000_000_000_000_000;
        tracker.stake(stake_amount);

        assert_eq!(tracker.total_staked, stake_amount);
        assert_eq!(tracker.circulating_supply(), TOTAL_SUPPLY - stake_amount);
    }

    #[test]
    fn test_supply_tracker_staking_ratio() {
        let mut tracker = SupplyTracker::new();
        assert!(tracker.staking_ratio().abs() < f64::EPSILON);

        let ten_percent = TOTAL_SUPPLY / 10;
        tracker.stake(ten_percent);

        let ratio = tracker.staking_ratio();
        assert!(
            (ratio - 10.0).abs() < 0.01,
            "Staking ratio should be ~10.0%, got {}",
            ratio
        );
    }

    #[test]
    fn test_stake_position_increase() {
        let staker = test_addr(1);

        let mut pos = StakePosition::new(staker, MIN_STAKE_LITE, 0);
        assert_eq!(pos.tier, StakeTier::Lite);
        assert!(!pos.tier.can_vote());

        let additional = MIN_STAKE_SPARK - MIN_STAKE_LITE;
        pos.increase_stake(additional);
        assert_eq!(pos.amount, MIN_STAKE_SPARK);
        assert_eq!(pos.tier, StakeTier::Spark);
        assert!(pos.tier.can_vote());

        let to_core = MIN_STAKE_CORE - pos.amount;
        pos.increase_stake(to_core);
        assert_eq!(pos.tier, StakeTier::Core);
        assert!(pos.tier.can_propose());
        assert!(pos.tier.can_govern());
    }

    #[test]
    fn test_state_rent_defaults() {
        let config = StateRentConfig::default();
        assert_eq!(config.cost_per_byte_per_epoch, 1);
        assert_eq!(config.epoch_length_blocks, 216_000);
        assert_eq!(config.dust_threshold, 1_000_000);
        assert_eq!(config.grace_epochs, 30);
        assert_eq!(config.account_size_bytes, 128);
    }

    #[test]
    fn test_rent_per_epoch() {
        let config = StateRentConfig::default();
        assert_eq!(config.rent_per_epoch(), 128);
    }

    #[test]
    fn test_rent_per_epoch_custom() {
        let config = StateRentConfig {
            cost_per_byte_per_epoch: 10,
            account_size_bytes: 256,
            ..Default::default()
        };
        assert_eq!(config.rent_per_epoch(), 2560);
    }

    #[test]
    fn test_is_dormant() {
        let config = StateRentConfig::default();
        assert!(config.is_dormant(0));
        assert!(config.is_dormant(999_999));
        assert!(!config.is_dormant(1_000_000));
        assert!(!config.is_dormant(10_000_000));
    }

    #[test]
    fn test_epochs_until_archive() {
        let config = StateRentConfig::default();
        assert_eq!(config.epochs_until_archive(1_001_280), 10);

        assert_eq!(config.epochs_until_archive(500_000), 0);

        assert_eq!(config.epochs_until_archive(1_000_000), 0);

        let balance = 1_000_000 + 128 * 1000;
        assert_eq!(config.epochs_until_archive(balance), 1000);
    }

    #[test]
    fn test_epochs_until_archive_zero_rent() {
        let config = StateRentConfig {
            cost_per_byte_per_epoch: 0,
            ..Default::default()
        };
        assert_eq!(config.epochs_until_archive(100), u64::MAX);
    }

    #[test]
    fn test_arc_base_units_matches_decimals() {
        assert_eq!(ARC_BASE_UNITS, 10u64.pow(DECIMALS as u32));
        assert_eq!(ARC_BASE_UNITS, 1_000_000_000);
    }

    #[test]
    fn test_inference_attestation_reward_is_2_5_arc() {
        assert_eq!(INFERENCE_ATTESTATION_REWARD, 2_500_000_000);
        assert_eq!(INFERENCE_ATTESTATION_REWARD, ARC_BASE_UNITS * 5 / 2);
        assert!((inference_attestation_reward_arc() - 2.5).abs() < 1e-9);
    }
}
