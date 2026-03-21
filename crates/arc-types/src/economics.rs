// Add to lib.rs: pub mod economics;

use serde::{Deserialize, Serialize};

use crate::account::Address;

// ─── ARC token constants ───────────────────────────────────────────────────

/// Total supply: 1.03 billion ARC with 9 decimal places.
pub const TOTAL_SUPPLY: u128 = 1_030_000_000_000_000_000; // 1.03B * 10^9
pub const DECIMALS: u8 = 9;

/// Blocks per year at ~400ms block time.
pub const BLOCKS_PER_YEAR: u64 = 78_840_000;

/// Maximum annual inflation rate for staking rewards (basis points). 300 = 3%.
pub const MAX_ANNUAL_INFLATION_BPS: u16 = 300;

/// No token burning. All fees are distributed to validators and treasury.
/// The 3% trading tax on the ERC-20 side funds ongoing rewards via bridge.
pub const BURN_ENABLED: bool = false;

// ─── Validator roles (distinct from staking tiers) ──────────────────────────

/// Validator role within a shard. Determines hardware requirements and revenue share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidatorRole {
    /// Executes transactions, produces blocks. Requires GPU + high-core CPU.
    /// Minimum stake: Arc tier (5M ARC).
    Proposer,
    /// Verifies state diffs from proposers. Moderate CPU.
    /// Minimum stake: Spark tier (500K ARC).
    Verifier,
    /// Light verification, proof checking. Minimal hardware.
    /// Minimum stake: Lite tier (50K ARC).
    Observer,
}

/// Revenue split configuration for all protocol revenue (fees + tax).
///
/// No tokens are burned. 100% of revenue flows to participants:
///   - Proposers: 40% (cover GPU/server costs)
///   - Verifiers: 25% (cover moderate hardware)
///   - Observers: 15% (reward home node runners)
///   - Treasury:  20% (team revenue, development, grants)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleRevenueConfig {
    /// Proposer share (basis points). Default: 4000 = 40%.
    pub proposer_share_bps: u16,
    /// Verifier share, split equally (basis points). Default: 2500 = 25%.
    pub verifier_share_bps: u16,
    /// Observer pool, pro-rata by stake (basis points). Default: 1500 = 15%.
    pub observer_pool_bps: u16,
    /// Treasury/team share (basis points). Default: 2000 = 20%.
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
    /// Split a fee amount into (proposer, per_verifier, observer_pool, treasury).
    /// `num_verifiers` is the count of active verifiers in this shard.
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
        FeeSplit { proposer, per_verifier, observer_pool, treasury }
    }
}

/// Result of splitting fees across validator roles and treasury.
#[derive(Debug, Clone)]
pub struct FeeSplit {
    /// Amount to the block proposer.
    pub proposer: u64,
    /// Amount to each individual verifier.
    pub per_verifier: u64,
    /// Amount to the observer staking pool.
    pub observer_pool: u64,
    /// Amount to the protocol treasury (team revenue).
    pub treasury: u64,
}

// ─── Validator bootstrap fund ───────────────────────────────────────────────

/// Bootstrap fund for subsidizing early validators before fee revenue is sufficient.
/// Linear vesting with optional cliff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapFund {
    /// Total ARC allocated to the fund.
    pub total_allocation: u128,
    /// Amount already distributed.
    pub distributed: u128,
    /// Block height when vesting begins.
    pub vesting_start_block: u64,
    /// Total vesting duration in blocks.
    pub vesting_duration_blocks: u64,
    /// Cliff period: no vesting before this many blocks after start.
    pub cliff_blocks: u64,
}

impl BootstrapFund {
    /// Create a 2-year bootstrap fund with 1-week cliff.
    pub fn new_two_year(total_allocation: u128, start_block: u64) -> Self {
        Self {
            total_allocation,
            distributed: 0,
            vesting_start_block: start_block,
            vesting_duration_blocks: BLOCKS_PER_YEAR * 2, // 2 years
            cliff_blocks: 1_512_000,                       // ~7 days
        }
    }

    /// Amount vested (unlocked) at a given block height.
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

    /// Amount claimable (vested minus already distributed).
    pub fn claimable(&self, current_block: u64) -> u128 {
        self.vested_amount(current_block).saturating_sub(self.distributed)
    }

    /// Per-block distribution amount for active validators.
    pub fn per_block_amount(&self) -> u128 {
        if self.vesting_duration_blocks == 0 {
            return 0;
        }
        self.total_allocation / self.vesting_duration_blocks as u128
    }
}

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
///
/// At high TPS, the base fee auto-scales to maintain a target annual burn rate
/// of ~0.25% of circulating supply, preventing fee-driven supply exhaustion.
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
    /// Smoothed TPS estimate (exponential moving average, updated each block).
    #[serde(default)]
    pub smoothed_tps: f64,
    /// Circulating supply for burn-rate targeting (updated each block).
    #[serde(default = "default_circulating_supply")]
    pub circulating_supply: u128,
    /// Target annual burn rate (basis points). Default: 25 = 0.25%.
    #[serde(default = "default_target_burn_bps")]
    pub target_annual_burn_bps: u16,
    /// Revenue split by validator role.
    #[serde(default)]
    pub role_revenue: RoleRevenueConfig,
}

fn default_circulating_supply() -> u128 { TOTAL_SUPPLY }
fn default_target_burn_bps() -> u16 { 0 }

/// Breakdown of fees for a single transaction.
/// No tokens are burned — 100% distributed to validators + treasury.
#[derive(Debug, Clone)]
pub struct FeeBreakdown {
    /// Base fee component.
    pub base_fee: u64,
    /// Priority fee (tip) component.
    pub priority_fee: u64,
    /// Total fee paid by the sender.
    pub total_fee: u64,
    /// Amount to the block proposer (40%).
    pub to_proposer: u64,
    /// Amount to shard verifiers (25%, split equally).
    pub to_verifiers: u64,
    /// Amount to the observer pool (15%, pro-rata by stake).
    pub to_observer_pool: u64,
    /// Amount to protocol treasury / team (20%).
    pub to_treasury: u64,
    /// Whether this transaction is a free settlement.
    pub is_free_settlement: bool,
}

impl FeeConfig {
    /// Sensible default configuration for ARC Chain mainnet.
    /// No burn — 100% of fees distributed to validators + treasury.
    pub fn default_config() -> Self {
        Self {
            base_fee: 1_000,
            min_base_fee: 1,
            max_base_fee: 1_000_000_000,
            target_block_utilization: 0.5,
            adjustment_speed: 8,
            burn_percentage: 0,       // No burn
            proposer_percentage: 10000, // 100% to validators (split by role_revenue)
            smoothed_tps: 0.0,
            circulating_supply: TOTAL_SUPPLY,
            target_annual_burn_bps: 0, // No burn target
            role_revenue: RoleRevenueConfig::default(),
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

        // No burn — 100% of fees distributed by role.
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

    /// Update smoothed TPS and auto-scale base fee to keep fees sustainable.
    ///
    /// At high TPS, fees auto-decrease so total fee extraction stays reasonable
    /// relative to circulating supply (~5% of supply/year in total fees).
    /// At low TPS, fees stay at defaults to prevent spam.
    pub fn adjust_for_tps(&mut self, block_tx_count: u64, block_time_secs: f64) {
        if block_time_secs <= 0.0 {
            return;
        }

        // Update smoothed TPS (exponential moving average, alpha = 0.1).
        let instant_tps = block_tx_count as f64 / block_time_secs;
        self.smoothed_tps = self.smoothed_tps * 0.9 + instant_tps * 0.1;

        if self.smoothed_tps < 100.0 {
            return; // Don't adjust at very low TPS
        }

        // Target: total annual fee extraction ≈ 5% of circulating supply.
        // This funds validators + treasury without inflating or burning.
        let target_annual_fees = self.circulating_supply * 500 / 10_000; // 5%
        let target_fees_per_block = target_annual_fees / BLOCKS_PER_YEAR as u128;

        if target_fees_per_block == 0 {
            return;
        }

        // Estimated actual fees per block at current base_fee:
        let avg_gas: u128 = 21_000;
        let txs_per_block = (self.smoothed_tps * block_time_secs) as u128;
        let actual_fees_per_block = (self.base_fee as u128)
            .saturating_mul(avg_gas)
            .saturating_mul(txs_per_block);

        if actual_fees_per_block == 0 {
            return;
        }

        // Adjust proportionally, clamped to ±50% per block.
        let ratio = target_fees_per_block as f64 / actual_fees_per_block as f64;
        let clamped_ratio = ratio.clamp(0.5, 2.0);
        let new_fee = ((self.base_fee as f64) * clamped_ratio) as u64;

        self.base_fee = new_fee.clamp(self.min_base_fee, self.max_base_fee);
    }

    /// Update circulating supply tracker.
    pub fn update_supply(&mut self, circulating: u128) {
        self.circulating_supply = circulating;
    }
}

// ─── State rent ─────────────────────────────────────────────────────────────

/// Configuration for state rent — charges accounts for on-chain storage to
/// prevent unbounded state growth at high TPS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRentConfig {
    /// Cost per byte per epoch (in smallest ARC unit, i.e. nanoARC).
    pub cost_per_byte_per_epoch: u64,
    /// Epoch length in blocks.
    pub epoch_length_blocks: u64,
    /// Minimum balance to keep an account alive (below this = dormant).
    pub dust_threshold: u64,
    /// Grace period in epochs before dormant accounts are archived.
    pub grace_epochs: u64,
    /// Bytes per standard account (for rent calculation).
    pub account_size_bytes: u64,
}

impl Default for StateRentConfig {
    fn default() -> Self {
        Self {
            cost_per_byte_per_epoch: 1,        // 1 nanoARC per byte per epoch
            epoch_length_blocks: 216_000,      // ~1 day at 400ms blocks
            dust_threshold: 1_000_000,         // 0.001 ARC
            grace_epochs: 30,                  // ~30 days
            account_size_bytes: 128,           // bytes per standard account
        }
    }
}

impl StateRentConfig {
    /// Cost per account per epoch: `cost_per_byte_per_epoch * account_size_bytes`.
    pub fn rent_per_epoch(&self) -> u64 {
        self.cost_per_byte_per_epoch.saturating_mul(self.account_size_bytes)
    }

    /// Returns `true` if the balance is below the dust threshold (dormant).
    pub fn is_dormant(&self, balance: u64) -> bool {
        balance < self.dust_threshold
    }

    /// How many full epochs of rent the given balance can cover before the
    /// account would be considered dormant.
    ///
    /// Returns `u64::MAX` if rent per epoch is zero (rent disabled).
    pub fn epochs_until_archive(&self, balance: u64) -> u64 {
        let rent = self.rent_per_epoch();
        if rent == 0 {
            return u64::MAX;
        }
        // After deducting rent each epoch, the account becomes dormant once
        // balance drops below dust_threshold. Usable balance for rent is
        // everything above the threshold (if already below, 0 epochs left).
        let usable = balance.saturating_sub(self.dust_threshold);
        usable / rent
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
    /// Total reward paid to the proposer (40% of fees).
    pub proposer_reward: u64,
    /// Total reward paid to verifiers (25% of fees).
    pub verifier_reward: u64,
    /// Amount added to observer pool (15% of fees).
    pub observer_pool_reward: u64,
    /// Amount sent to treasury (20% of fees).
    pub treasury_reward: u64,
    /// Total staking rewards distributed in this block.
    pub staking_rewards_distributed: u64,
    /// Number of transactions in the block.
    pub tx_count: u32,
    /// Number of free settlement transactions.
    pub free_settlement_count: u32,
}

// ─── Supply tracker ────────────────────────────────────────────────────────

/// Tracks the ARC token supply. Fixed supply — no burn, no inflation.
/// Staking rewards and validator payments come from fees + tax revenue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupplyTracker {
    /// The fixed total supply (never changes).
    pub total_supply: u128,
    /// Tokens currently locked in staking.
    pub total_staked: u128,
    /// Tokens locked in bootstrap fund (vesting).
    pub bootstrap_locked: u128,
    /// Tokens locked in treasury.
    pub treasury_balance: u128,
    /// Cumulative fee revenue distributed to validators.
    pub total_fees_distributed: u128,
    /// Cumulative tax revenue bridged from ETH and distributed.
    pub total_tax_revenue: u128,
    /// Current block height.
    pub current_block: u64,
}

impl SupplyTracker {
    /// Initialize the tracker at genesis.
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

    /// Record tokens moving into staking.
    pub fn stake(&mut self, amount: u128) {
        self.total_staked = self.total_staked.saturating_add(amount);
    }

    /// Record tokens leaving staking.
    pub fn unstake(&mut self, amount: u128) {
        self.total_staked = self.total_staked.saturating_sub(amount);
    }

    /// Record fee revenue distributed to validators.
    pub fn record_fees(&mut self, amount: u128) {
        self.total_fees_distributed = self.total_fees_distributed.saturating_add(amount);
    }

    /// Record tax revenue bridged from ETH.
    pub fn record_tax_revenue(&mut self, amount: u128) {
        self.total_tax_revenue = self.total_tax_revenue.saturating_add(amount);
    }

    /// Circulating supply = total - staked - bootstrap_locked - treasury.
    pub fn circulating_supply(&self) -> u128 {
        self.total_supply
            .saturating_sub(self.total_staked)
            .saturating_sub(self.bootstrap_locked)
            .saturating_sub(self.treasury_balance)
    }

    /// Staking ratio as percentage.
    pub fn staking_ratio(&self) -> f64 {
        if self.total_supply == 0 { return 0.0; }
        self.total_staked as f64 / self.total_supply as f64 * 100.0
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

    // 7. Normal transaction fee split — no burn, 100% to validators + treasury
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

        // No burn — all fees distributed
        let total_distributed = fee.to_proposer + fee.to_verifiers + fee.to_observer_pool + fee.to_treasury;
        // Allow ±4 for integer rounding across 4 splits
        assert!(
            (total_distributed as i64 - fee.total_fee as i64).abs() <= 4,
            "distributed {} should ≈ total_fee {}", total_distributed, fee.total_fee
        );

        // Proposer gets 40%
        let expected_proposer = (fee.total_fee as u128 * 4000 / 10_000) as u64;
        assert_eq!(fee.to_proposer, expected_proposer);

        // Treasury gets 20%
        let expected_treasury = (fee.total_fee as u128 * 2000 / 10_000) as u64;
        assert_eq!(fee.to_treasury, expected_treasury);
    }

    // 8. Settlement transactions are free
    #[test]
    fn test_fee_calculation_free_settlement() {
        let config = FeeConfig::default_config();
        let fee = config.calculate_fee(50_000, 1_000, "Settle");

        assert!(fee.is_free_settlement);
        assert_eq!(fee.total_fee, 0);
        assert_eq!(fee.to_proposer, 0);
        assert_eq!(fee.to_treasury, 0);
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

    // 11. Supply tracker — staking reduces circulating supply
    #[test]
    fn test_supply_tracker_staking() {
        let mut tracker = SupplyTracker::new();
        assert_eq!(tracker.circulating_supply(), TOTAL_SUPPLY);

        let stake_amount: u128 = 5_000_000_000_000_000; // 5M ARC
        tracker.stake(stake_amount);

        assert_eq!(tracker.total_staked, stake_amount);
        assert_eq!(tracker.circulating_supply(), TOTAL_SUPPLY - stake_amount);
    }

    // 12. Staking ratio
    #[test]
    fn test_supply_tracker_staking_ratio() {
        let mut tracker = SupplyTracker::new();
        assert_eq!(tracker.staking_ratio(), 0.0);

        // Stake 10% of total supply
        let ten_percent = TOTAL_SUPPLY / 10;
        tracker.stake(ten_percent);

        let ratio = tracker.staking_ratio();
        assert!(
            (ratio - 10.0).abs() < 0.01,
            "Staking ratio should be ~10.0%, got {}",
            ratio
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

    // ── State rent tests ──────────────────────────────────────────────────

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
        // 1 nanoARC/byte * 128 bytes = 128 nanoARC per epoch
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
        // rent_per_epoch = 128
        // dust_threshold = 1_000_000
        // balance = 1_000_000 + 128*10 = 1_001_280 → usable = 1280 → 10 epochs
        assert_eq!(config.epochs_until_archive(1_001_280), 10);

        // Already below dust threshold → 0 epochs
        assert_eq!(config.epochs_until_archive(500_000), 0);

        // Exactly at threshold → usable = 0 → 0 epochs
        assert_eq!(config.epochs_until_archive(1_000_000), 0);

        // Large balance → many epochs
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
}
