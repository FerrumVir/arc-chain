// Add to lib.rs: pub mod economics;

use serde::{Deserialize, Serialize};

use crate::account::Address;

// ─── ARC token constants ───────────────────────────────────────────────────

/// Total supply: 1.03 billion ARC with 9 decimal places.
pub const TOTAL_SUPPLY: u128 = 1_030_000_000_000_000_000; // 1.03B * 10^9
pub const DECIMALS: u8 = 9;

/// Minimum stake amounts per tier (in smallest unit, 9 decimals).
pub const MIN_STAKE_LITE: u64 = 50_000_000_000_000; // 50K ARC
pub const MIN_STAKE_SPARK: u64 = 500_000_000_000_000; // 500K ARC
pub const MIN_STAKE_ARC: u64 = 5_000_000_000_000_000; // 5M ARC
pub const MIN_STAKE_CORE: u64 = 50_000_000_000_000_000; // 50M ARC

/// Annual percentage yield per tier (basis points: 100 bps = 1%).
pub const APY_LITE: u16 = 500; // 5.00%
pub const APY_SPARK: u16 = 800; // 8.00%
pub const APY_ARC: u16 = 1500; // 15.00%
pub const APY_CORE: u16 = 2500; // 25.00%

/// Unbonding periods per tier (in blocks, ~400 ms per block).
pub const UNBONDING_LITE: u64 = 216_000; // ~1 day
pub const UNBONDING_SPARK: u64 = 1_512_000; // ~7 days
pub const UNBONDING_ARC: u64 = 3_024_000; // ~14 days
pub const UNBONDING_CORE: u64 = 6_480_000; // ~30 days

// ─── Staking tier ──────────────────────────────────────────────────────────

/// Staking tier derived from the amount staked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StakeTier {
    None,
    Lite,
    Spark,
    Arc,
    Core,
}

impl StakeTier {
    /// Determine the staking tier from a raw token amount.
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

    /// Minimum stake required for this tier. Returns 0 for `None`.
    pub fn min_stake(&self) -> u64 {
        match self {
            StakeTier::None => 0,
            StakeTier::Lite => MIN_STAKE_LITE,
            StakeTier::Spark => MIN_STAKE_SPARK,
            StakeTier::Arc => MIN_STAKE_ARC,
            StakeTier::Core => MIN_STAKE_CORE,
        }
    }

    /// Annual yield in basis points (100 bps = 1%). Returns 0 for `None`.
    pub fn apy_bps(&self) -> u16 {
        match self {
            StakeTier::None => 0,
            StakeTier::Lite => APY_LITE,
            StakeTier::Spark => APY_SPARK,
            StakeTier::Arc => APY_ARC,
            StakeTier::Core => APY_CORE,
        }
    }

    /// Unbonding period in blocks. Returns 0 for `None`.
    pub fn unbonding_period(&self) -> u64 {
        match self {
            StakeTier::None => 0,
            StakeTier::Lite => UNBONDING_LITE,
            StakeTier::Spark => UNBONDING_SPARK,
            StakeTier::Arc => UNBONDING_ARC,
            StakeTier::Core => UNBONDING_CORE,
        }
    }

    /// Whether this tier can propose blocks (Arc and Core only).
    pub fn can_propose(&self) -> bool {
        matches!(self, StakeTier::Arc | StakeTier::Core)
    }

    /// Whether this tier can vote in consensus (Spark, Arc, Core).
    pub fn can_vote(&self) -> bool {
        matches!(self, StakeTier::Spark | StakeTier::Arc | StakeTier::Core)
    }

    /// Whether this tier has governance rights (Core only).
    pub fn can_govern(&self) -> bool {
        matches!(self, StakeTier::Core)
    }

    /// Human-readable tier name.
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

// ─── Staking position ──────────────────────────────────────────────────────

/// A single staking position held by an address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StakePosition {
    /// The staker's address.
    pub staker: Address,
    /// Total amount staked (smallest unit).
    pub amount: u64,
    /// Current tier (derived from amount).
    pub tier: StakeTier,
    /// Block height when the stake was created.
    pub staked_at_height: u64,
    /// Block height when rewards were last claimed.
    pub last_reward_height: u64,
    /// Rewards accumulated but not yet withdrawn.
    pub accumulated_rewards: u64,
    /// Whether the position is actively staking.
    pub is_active: bool,
    /// If unbonding, the block height when unbonding started.
    pub unbonding_height: Option<u64>,
}

impl StakePosition {
    /// Create a new active staking position at the given block height.
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

    /// Calculate pending rewards since the last claim.
    ///
    /// Formula: `amount * apy_bps / 10_000 * elapsed_blocks / blocks_per_year`
    ///
    /// Uses u128 intermediaries to avoid overflow on large stakes.
    pub fn calculate_pending_rewards(&self, current_height: u64, blocks_per_year: u64) -> u64 {
        if !self.is_active || current_height <= self.last_reward_height || blocks_per_year == 0 {
            return 0;
        }
        let elapsed = current_height - self.last_reward_height;
        let apy = self.tier.apy_bps() as u128;
        let amt = self.amount as u128;
        // reward = amount * (apy / 10_000) * (elapsed / blocks_per_year)
        let reward = amt
            .checked_mul(apy)
            .and_then(|v| v.checked_mul(elapsed as u128))
            .map(|v| v / (10_000u128 * blocks_per_year as u128))
            .unwrap_or(0);
        reward as u64
    }

    /// Claim pending rewards: accumulate them and reset the reward height.
    /// Returns the amount of newly claimed rewards.
    pub fn claim_rewards(&mut self, current_height: u64, blocks_per_year: u64) -> u64 {
        let pending = self.calculate_pending_rewards(current_height, blocks_per_year);
        self.accumulated_rewards = self.accumulated_rewards.saturating_add(pending);
        self.last_reward_height = current_height;
        pending
    }

    /// Begin the unbonding process at the given block height.
    pub fn begin_unbonding(&mut self, current_height: u64) {
        self.is_active = false;
        self.unbonding_height = Some(current_height);
    }

    /// Whether the unbonding period has elapsed and funds can be withdrawn.
    pub fn can_withdraw(&self, current_height: u64) -> bool {
        match self.unbonding_height {
            Some(start) => {
                let period = self.tier.unbonding_period();
                current_height >= start.saturating_add(period)
            }
            None => false,
        }
    }

    /// Increase the stake and potentially upgrade the tier.
    pub fn increase_stake(&mut self, additional: u64) {
        self.amount = self.amount.saturating_add(additional);
        self.tier = StakeTier::from_amount(self.amount);
    }
}

// ─── Fee model (EIP-1559 style) ────────────────────────────────────────────

/// Configuration for the EIP-1559 style dynamic fee market.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeConfig {
    /// Current base fee per gas unit (smallest ARC unit).
    pub base_fee: u64,
    /// Minimum base fee floor.
    pub min_base_fee: u64,
    /// Maximum base fee ceiling.
    pub max_base_fee: u64,
    /// Target block utilization ratio (0.0 .. 1.0). Typically 0.5.
    pub target_block_utilization: f64,
    /// How fast the base fee adjusts (denominator). Higher = slower.
    pub adjustment_speed: u64,
    /// Percentage of base fee burned (basis points). 5000 = 50%.
    pub burn_percentage: u16,
    /// Percentage of base fee to block proposer (basis points). 5000 = 50%.
    pub proposer_percentage: u16,
}

/// Breakdown of fees for a single transaction.
#[derive(Debug, Clone)]
pub struct FeeBreakdown {
    /// Base fee component.
    pub base_fee: u64,
    /// Priority fee (tip) component.
    pub priority_fee: u64,
    /// Total fee paid by the sender.
    pub total_fee: u64,
    /// Amount of the total fee that is burned.
    pub burned: u64,
    /// Amount of the total fee that goes to the block proposer.
    pub to_proposer: u64,
    /// Whether this transaction is a free settlement.
    pub is_free_settlement: bool,
}

impl FeeConfig {
    /// Sensible default configuration for ARC Chain mainnet.
    pub fn default_config() -> Self {
        Self {
            base_fee: 1_000, // 0.000001 ARC per gas unit
            min_base_fee: 100,
            max_base_fee: 1_000_000_000, // 1 ARC per gas unit
            target_block_utilization: 0.5,
            adjustment_speed: 8, // 1/8 max change per block
            burn_percentage: 5000, // 50%
            proposer_percentage: 5000, // 50%
        }
    }

    /// Returns true if the given transaction type string represents a free settlement.
    pub fn is_free_settlement(tx_type: &str) -> bool {
        tx_type == "Settle"
    }

    /// Calculate the fee breakdown for a transaction.
    ///
    /// - `gas_used`: gas consumed by the transaction
    /// - `priority_fee`: tip per gas unit chosen by the sender
    /// - `tx_type`: string name of the transaction type (e.g. "Transfer", "Settle")
    pub fn calculate_fee(&self, gas_used: u64, priority_fee: u64, tx_type: &str) -> FeeBreakdown {
        if Self::is_free_settlement(tx_type) {
            return FeeBreakdown {
                base_fee: 0,
                priority_fee: 0,
                total_fee: 0,
                burned: 0,
                to_proposer: 0,
                is_free_settlement: true,
            };
        }

        let base_component = self.base_fee.saturating_mul(gas_used);
        let priority_component = priority_fee.saturating_mul(gas_used);
        let total = base_component.saturating_add(priority_component);

        // Base fee is split: burn_percentage burned, proposer_percentage to proposer.
        let burned = (base_component as u128 * self.burn_percentage as u128 / 10_000) as u64;
        let base_to_proposer =
            (base_component as u128 * self.proposer_percentage as u128 / 10_000) as u64;

        // Priority fee goes entirely to the proposer.
        let to_proposer = base_to_proposer.saturating_add(priority_component);

        FeeBreakdown {
            base_fee: base_component,
            priority_fee: priority_component,
            total_fee: total,
            burned,
            to_proposer,
            is_free_settlement: false,
        }
    }

    /// Adjust the base fee after a block based on its utilization.
    ///
    /// If `block_utilization > target`, fee goes up.
    /// If `block_utilization < target`, fee goes down.
    /// The change magnitude is proportional to the deviation, capped at `1/adjustment_speed`.
    pub fn adjust_base_fee(&mut self, block_utilization: f64) {
        if self.adjustment_speed == 0 {
            return;
        }

        let delta = block_utilization - self.target_block_utilization;
        // Max change per block is base_fee / adjustment_speed.
        let max_change = self.base_fee / self.adjustment_speed;

        // Scale the change by how far we are from target (normalized to 0..1 range).
        // delta is in [-target, 1-target] range; we scale to [-1, 1] relative to target.
        let scale = if delta >= 0.0 {
            // Above target: scale by delta / (1 - target)
            let denom = 1.0 - self.target_block_utilization;
            if denom <= 0.0 {
                1.0
            } else {
                (delta / denom).min(1.0)
            }
        } else {
            // Below target: scale by delta / target
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
}

// ─── Block reward ──────────────────────────────────────────────────────────

/// Summary of economic activity in a single block.
#[derive(Debug, Clone)]
pub struct BlockReward {
    /// Block height.
    pub block_height: u64,
    /// Address of the block proposer.
    pub proposer: Address,
    /// Total base fee revenue collected.
    pub base_fee_revenue: u64,
    /// Total priority fee revenue collected.
    pub priority_fee_revenue: u64,
    /// Total tokens burned from base fees.
    pub burned: u64,
    /// Total reward paid to the proposer (base share + tips).
    pub proposer_reward: u64,
    /// Total staking rewards distributed in this block.
    pub staking_rewards_distributed: u64,
    /// Number of transactions in the block.
    pub tx_count: u32,
    /// Number of free settlement transactions.
    pub free_settlement_count: u32,
}

// ─── Supply tracker ────────────────────────────────────────────────────────

/// Tracks the evolving ARC token supply (deflationary mechanics).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupplyTracker {
    /// The genesis supply.
    pub initial_supply: u128,
    /// Cumulative tokens burned.
    pub total_burned: u128,
    /// Tokens currently locked in staking.
    pub total_staked: u128,
    /// Circulating supply = initial - burned.
    pub circulating_supply: u128,
    /// Cumulative staking rewards paid out.
    pub total_rewards_paid: u128,
    /// Current block height.
    pub current_block: u64,
}

impl SupplyTracker {
    /// Initialize the tracker at genesis.
    pub fn new() -> Self {
        Self {
            initial_supply: TOTAL_SUPPLY,
            total_burned: 0,
            total_staked: 0,
            circulating_supply: TOTAL_SUPPLY,
            total_rewards_paid: 0,
            current_block: 0,
        }
    }

    /// Burn tokens, reducing the circulating supply permanently.
    pub fn burn(&mut self, amount: u128) {
        self.total_burned = self.total_burned.saturating_add(amount);
        self.circulating_supply = self
            .initial_supply
            .saturating_sub(self.total_burned);
    }

    /// Record tokens moving into staking.
    pub fn stake(&mut self, amount: u128) {
        self.total_staked = self.total_staked.saturating_add(amount);
    }

    /// Record tokens leaving staking.
    pub fn unstake(&mut self, amount: u128) {
        self.total_staked = self.total_staked.saturating_sub(amount);
    }

    /// Record staking rewards paid out (inflationary pressure counter).
    pub fn pay_reward(&mut self, amount: u128) {
        self.total_rewards_paid = self.total_rewards_paid.saturating_add(amount);
    }

    /// Deflation rate as a percentage of the initial supply that has been burned.
    pub fn deflation_rate(&self) -> f64 {
        if self.initial_supply == 0 {
            return 0.0;
        }
        self.total_burned as f64 / self.initial_supply as f64 * 100.0
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::Hash256;

    fn test_addr(n: u8) -> Address {
        let mut bytes = [0u8; 32];
        bytes[0] = n;
        Hash256(bytes)
    }

    // 1. Correct tier classification
    #[test]
    fn test_stake_tier_from_amount() {
        assert_eq!(StakeTier::from_amount(0), StakeTier::None);
        assert_eq!(StakeTier::from_amount(10_000_000_000_000), StakeTier::None); // 10K — below Lite
        assert_eq!(StakeTier::from_amount(MIN_STAKE_LITE), StakeTier::Lite);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_SPARK), StakeTier::Spark);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_ARC), StakeTier::Arc);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_CORE), StakeTier::Core);
        // In between: 200K should be Lite (below Spark threshold)
        assert_eq!(
            StakeTier::from_amount(200_000_000_000_000),
            StakeTier::Lite
        );
        // 1M should be Spark
        assert_eq!(
            StakeTier::from_amount(1_000_000_000_000_000),
            StakeTier::Spark
        );
    }

    // 2. Exact boundary amounts
    #[test]
    fn test_stake_tier_boundaries() {
        // One unit below each threshold → lower tier
        assert_eq!(StakeTier::from_amount(MIN_STAKE_LITE - 1), StakeTier::None);
        assert_eq!(
            StakeTier::from_amount(MIN_STAKE_SPARK - 1),
            StakeTier::Lite
        );
        assert_eq!(
            StakeTier::from_amount(MIN_STAKE_ARC - 1),
            StakeTier::Spark
        );
        assert_eq!(
            StakeTier::from_amount(MIN_STAKE_CORE - 1),
            StakeTier::Arc
        );

        // Exactly at threshold → that tier
        assert_eq!(StakeTier::from_amount(MIN_STAKE_LITE), StakeTier::Lite);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_SPARK), StakeTier::Spark);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_ARC), StakeTier::Arc);
        assert_eq!(StakeTier::from_amount(MIN_STAKE_CORE), StakeTier::Core);
    }

    // 3. Tier permissions
    #[test]
    fn test_tier_permissions() {
        // None: no permissions
        assert!(!StakeTier::None.can_propose());
        assert!(!StakeTier::None.can_vote());
        assert!(!StakeTier::None.can_govern());

        // Lite: observer only
        assert!(!StakeTier::Lite.can_propose());
        assert!(!StakeTier::Lite.can_vote());
        assert!(!StakeTier::Lite.can_govern());

        // Spark: can vote
        assert!(!StakeTier::Spark.can_propose());
        assert!(StakeTier::Spark.can_vote());
        assert!(!StakeTier::Spark.can_govern());

        // Arc: can vote + propose
        assert!(StakeTier::Arc.can_propose());
        assert!(StakeTier::Arc.can_vote());
        assert!(!StakeTier::Arc.can_govern());

        // Core: all permissions
        assert!(StakeTier::Core.can_propose());
        assert!(StakeTier::Core.can_vote());
        assert!(StakeTier::Core.can_govern());
    }

    // 4. Correct APY math
    #[test]
    fn test_stake_rewards_calculation() {
        let blocks_per_year: u64 = 78_840_000; // ~400ms blocks
        let staker = test_addr(1);

        // Stake 5M ARC (Arc tier, 15% APY) for exactly one year of blocks.
        let pos = StakePosition::new(staker, MIN_STAKE_ARC, 0);
        assert_eq!(pos.tier, StakeTier::Arc);

        let reward = pos.calculate_pending_rewards(blocks_per_year, blocks_per_year);
        // Expected: 5M * 15% = 750K ARC = 750_000_000_000_000 smallest units
        let expected = (MIN_STAKE_ARC as u128 * APY_ARC as u128 / 10_000) as u64;
        assert_eq!(
            reward, expected,
            "One full year of Arc staking should yield 15% APY"
        );

        // Half a year should yield half
        let half_year = blocks_per_year / 2;
        let half_reward = pos.calculate_pending_rewards(half_year, blocks_per_year);
        // Integer division means half_reward may differ by 1 from expected/2
        let expected_half = expected / 2;
        assert!(
            half_reward == expected_half || half_reward == expected_half + 1,
            "Half year reward {} should be close to {}",
            half_reward,
            expected_half
        );
    }

    // 5. Rewards accumulate and reset
    #[test]
    fn test_claim_rewards() {
        let blocks_per_year: u64 = 78_840_000;
        let staker = test_addr(1);

        let mut pos = StakePosition::new(staker, MIN_STAKE_SPARK, 0);
        assert_eq!(pos.tier, StakeTier::Spark);

        // Accumulate for 1M blocks
        let claimed = pos.claim_rewards(1_000_000, blocks_per_year);
        assert!(claimed > 0, "Should have earned some rewards");
        assert_eq!(pos.accumulated_rewards, claimed);
        assert_eq!(pos.last_reward_height, 1_000_000);

        // Claim again at same height → 0 new rewards
        let second = pos.claim_rewards(1_000_000, blocks_per_year);
        assert_eq!(second, 0, "No new blocks = no new rewards");
        assert_eq!(
            pos.accumulated_rewards, claimed,
            "Accumulated should not change"
        );

        // Claim at a later height → more rewards
        let third = pos.claim_rewards(2_000_000, blocks_per_year);
        assert!(third > 0);
        assert_eq!(pos.accumulated_rewards, claimed + third);
    }

    // 6. Unbonding period enforcement
    #[test]
    fn test_unbonding_period() {
        let staker = test_addr(1);
        let mut pos = StakePosition::new(staker, MIN_STAKE_ARC, 100);
        assert!(pos.is_active);
        assert!(!pos.can_withdraw(1_000_000));

        // Begin unbonding at block 1000
        pos.begin_unbonding(1_000);
        assert!(!pos.is_active);
        assert_eq!(pos.unbonding_height, Some(1_000));

        // Arc tier unbonding = 3_024_000 blocks
        // Cannot withdraw before period elapses
        assert!(
            !pos.can_withdraw(1_000 + UNBONDING_ARC - 1),
            "Should not withdraw one block early"
        );
        // Can withdraw at exactly the end
        assert!(
            pos.can_withdraw(1_000 + UNBONDING_ARC),
            "Should withdraw at exact end of period"
        );
        // Can withdraw after
        assert!(pos.can_withdraw(1_000 + UNBONDING_ARC + 1_000));
    }

    // 7. Normal transaction fee split
    #[test]
    fn test_fee_calculation_normal_tx() {
        let config = FeeConfig::default_config();
        let gas_used = 21_000;
        let priority_fee = 500; // per gas unit

        let fee = config.calculate_fee(gas_used, priority_fee, "Transfer");

        assert!(!fee.is_free_settlement);
        assert_eq!(fee.base_fee, config.base_fee * gas_used);
        assert_eq!(fee.priority_fee, priority_fee * gas_used);
        assert_eq!(fee.total_fee, fee.base_fee + fee.priority_fee);

        // 50% of base fee burned
        let expected_burned = fee.base_fee / 2;
        assert_eq!(fee.burned, expected_burned);

        // Proposer gets 50% of base fee + 100% of priority
        let expected_to_proposer = fee.base_fee / 2 + fee.priority_fee;
        assert_eq!(fee.to_proposer, expected_to_proposer);

        // Burned + proposer should account for entire fee
        assert_eq!(fee.burned + fee.to_proposer, fee.total_fee);
    }

    // 8. Settlement transactions are free
    #[test]
    fn test_fee_calculation_free_settlement() {
        let config = FeeConfig::default_config();
        let fee = config.calculate_fee(50_000, 1_000, "Settle");

        assert!(fee.is_free_settlement);
        assert_eq!(fee.total_fee, 0);
        assert_eq!(fee.base_fee, 0);
        assert_eq!(fee.priority_fee, 0);
        assert_eq!(fee.burned, 0);
        assert_eq!(fee.to_proposer, 0);
    }

    // 9. Base fee increases when block is more than half full
    #[test]
    fn test_base_fee_adjustment_up() {
        let mut config = FeeConfig::default_config();
        let original = config.base_fee;

        // 80% utilization (above 50% target) → fee should increase
        config.adjust_base_fee(0.8);
        assert!(
            config.base_fee > original,
            "Base fee should increase: {} > {}",
            config.base_fee,
            original
        );
    }

    // 10. Base fee decreases when block is less than half full
    #[test]
    fn test_base_fee_adjustment_down() {
        let mut config = FeeConfig::default_config();
        let original = config.base_fee;

        // 20% utilization (below 50% target) → fee should decrease
        config.adjust_base_fee(0.2);
        assert!(
            config.base_fee < original,
            "Base fee should decrease: {} < {}",
            config.base_fee,
            original
        );
    }

    // 11. Supply tracker burn reduces circulating supply
    #[test]
    fn test_supply_tracker_burn() {
        let mut tracker = SupplyTracker::new();
        assert_eq!(tracker.circulating_supply, TOTAL_SUPPLY);

        let burn_amount: u128 = 1_000_000_000_000_000; // 1M ARC
        tracker.burn(burn_amount);

        assert_eq!(tracker.total_burned, burn_amount);
        assert_eq!(tracker.circulating_supply, TOTAL_SUPPLY - burn_amount);
    }

    // 12. Deflation rate calculated correctly
    #[test]
    fn test_supply_tracker_deflation() {
        let mut tracker = SupplyTracker::new();
        assert_eq!(tracker.deflation_rate(), 0.0);

        // Burn 1% of total supply
        let one_percent = TOTAL_SUPPLY / 100;
        tracker.burn(one_percent);

        let rate = tracker.deflation_rate();
        // Should be very close to 1.0%
        assert!(
            (rate - 1.0).abs() < 0.01,
            "Deflation rate should be ~1.0%, got {}",
            rate
        );
    }

    // 13. Stake increase triggers tier upgrade
    #[test]
    fn test_stake_position_increase() {
        let staker = test_addr(1);

        // Start at Lite tier
        let mut pos = StakePosition::new(staker, MIN_STAKE_LITE, 0);
        assert_eq!(pos.tier, StakeTier::Lite);
        assert!(!pos.tier.can_vote());

        // Add enough to reach Spark tier
        let additional = MIN_STAKE_SPARK - MIN_STAKE_LITE;
        pos.increase_stake(additional);
        assert_eq!(pos.amount, MIN_STAKE_SPARK);
        assert_eq!(pos.tier, StakeTier::Spark);
        assert!(pos.tier.can_vote());

        // Push all the way to Core
        let to_core = MIN_STAKE_CORE - pos.amount;
        pos.increase_stake(to_core);
        assert_eq!(pos.tier, StakeTier::Core);
        assert!(pos.tier.can_propose());
        assert!(pos.tier.can_govern());
    }
}
