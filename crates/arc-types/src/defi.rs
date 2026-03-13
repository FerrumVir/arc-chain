// Add to lib.rs: pub mod defi;

use serde::{Deserialize, Serialize};
use std::fmt;

// ─── Constants ──────────────────────────────────────────────────────────────

/// Precision for price calculations (18 decimal places).
const PRICE_PRECISION: u128 = 1_000_000_000_000_000_000; // 10^18

/// Default swap fee: 30 basis points (0.30%).
pub const DEFAULT_SWAP_FEE_BPS: u16 = 30;

/// Default collateral ratio: 150%.
pub const DEFAULT_COLLATERAL_RATIO_BPS: u16 = 15000;

/// Default liquidation ratio: 120%.
pub const DEFAULT_LIQUIDATION_RATIO_BPS: u16 = 12000;

/// Default stability fee: 2% annual.
pub const DEFAULT_STABILITY_FEE_BPS: u16 = 200;

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Errors arising from DeFi operations.
#[derive(Debug)]
pub enum DeFiError {
    /// The pool has insufficient reserves to satisfy the swap.
    InsufficientLiquidity,
    /// Output amount is below the caller's minimum (slippage protection).
    SlippageExceeded,
    /// The swap deadline (block height) has passed.
    DeadlineExpired,
    /// The provided token address does not belong to this pool.
    InvalidToken,
    /// Vault collateral is insufficient for the requested operation.
    InsufficientCollateral,
    /// Operation would bring the vault below its liquidation ratio.
    BelowLiquidationRatio,
    /// Amount must be greater than zero.
    ZeroAmount,
    /// Pool has zero liquidity.
    PoolEmpty,
}

impl fmt::Display for DeFiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeFiError::InsufficientLiquidity => write!(f, "insufficient liquidity in pool"),
            DeFiError::SlippageExceeded => write!(f, "output below minimum (slippage exceeded)"),
            DeFiError::DeadlineExpired => write!(f, "swap deadline expired"),
            DeFiError::InvalidToken => write!(f, "token not in pool"),
            DeFiError::InsufficientCollateral => write!(f, "insufficient collateral"),
            DeFiError::BelowLiquidationRatio => write!(f, "below liquidation ratio"),
            DeFiError::ZeroAmount => write!(f, "amount must be non-zero"),
            DeFiError::PoolEmpty => write!(f, "pool is empty"),
        }
    }
}

impl std::error::Error for DeFiError {}

// ─── Price source ───────────────────────────────────────────────────────────

/// Source of a price feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PriceSource {
    /// Derived from an on-chain AMM pool.
    Pool([u8; 32]),
    /// External oracle submission.
    Oracle,
    /// VRF-verified random oracle.
    Vrf,
}

/// A single price entry for a token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceEntry {
    /// Token address.
    pub token: [u8; 32],
    /// Price in USD with 18 decimal places of precision.
    pub price_usd: u128,
    /// Block height at which the price was recorded.
    pub timestamp: u64,
    /// Origin of this price data.
    pub source: PriceSource,
}

// ─── Order status ───────────────────────────────────────────────────────────

/// Status of a limit order on the order book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Expired,
}

// ─── Liquidity pool ─────────────────────────────────────────────────────────

/// Uniswap V2-style constant-product automated market maker (AMM) pool.
///
/// Invariant: `reserve_a * reserve_b = k` (after fees).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidityPool {
    /// Unique pool identifier.
    pub pool_id: [u8; 32],
    /// Token A address.
    pub token_a: [u8; 32],
    /// Token B address.
    pub token_b: [u8; 32],
    /// Reserve of token A held by the pool.
    pub reserve_a: u128,
    /// Reserve of token B held by the pool.
    pub reserve_b: u128,
    /// Total LP shares outstanding.
    pub total_lp_shares: u128,
    /// Swap fee in basis points (30 = 0.30%).
    pub fee_bps: u16,
    /// Block height at pool creation.
    pub created_at: u64,
    /// Cumulative trade volume (in token-agnostic smallest units).
    pub cumulative_volume: u128,
}

/// Result of a swap computation.
#[derive(Debug, Clone)]
pub struct SwapResult {
    /// Amount of input token consumed.
    pub amount_in: u128,
    /// Amount of output token produced.
    pub amount_out: u128,
    /// Fee deducted from input (in input token units).
    pub fee_amount: u128,
    /// Price impact in basis points.
    pub price_impact_bps: u16,
    /// Reserve A after the swap.
    pub new_reserve_a: u128,
    /// Reserve B after the swap.
    pub new_reserve_b: u128,
}

/// Parameters for executing a swap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapParams {
    /// Pool to swap in.
    pub pool_id: [u8; 32],
    /// Address of the token being sold.
    pub token_in: [u8; 32],
    /// Amount of input token to sell.
    pub amount_in: u128,
    /// Minimum acceptable output (slippage protection).
    pub min_amount_out: u128,
    /// Block-height deadline after which the swap reverts.
    pub deadline: u64,
}

impl LiquidityPool {
    /// Create a new empty pool for the given token pair.
    pub fn new(token_a: [u8; 32], token_b: [u8; 32], fee_bps: u16) -> Self {
        // Deterministic pool ID from the token pair.
        let mut hasher = blake3::Hasher::new_derive_key("ARC-pool-v1");
        hasher.update(&token_a);
        hasher.update(&token_b);
        let hash = hasher.finalize();

        Self {
            pool_id: *hash.as_bytes(),
            token_a,
            token_b,
            reserve_a: 0,
            reserve_b: 0,
            total_lp_shares: 0,
            fee_bps,
            created_at: 0,
            cumulative_volume: 0,
        }
    }

    /// Calculate the output amount for a constant-product swap.
    ///
    /// Uses the standard AMM formula:
    ///   `amount_out = (reserve_out * amount_in_after_fee) / (reserve_in + amount_in_after_fee)`
    ///
    /// The fee is deducted from `amount_in` before computing the output.
    pub fn calculate_swap(
        &self,
        token_in: &[u8; 32],
        amount_in: u128,
    ) -> Result<SwapResult, DeFiError> {
        if amount_in == 0 {
            return Err(DeFiError::ZeroAmount);
        }
        if self.reserve_a == 0 || self.reserve_b == 0 {
            return Err(DeFiError::PoolEmpty);
        }

        let (reserve_in, reserve_out, is_a_to_b) = if *token_in == self.token_a {
            (self.reserve_a, self.reserve_b, true)
        } else if *token_in == self.token_b {
            (self.reserve_b, self.reserve_a, false)
        } else {
            return Err(DeFiError::InvalidToken);
        };

        // Fee deducted from input.
        let fee_amount = amount_in * self.fee_bps as u128 / 10_000;
        let amount_in_after_fee = amount_in - fee_amount;

        // Constant product: amount_out = reserve_out * amount_in_after_fee / (reserve_in + amount_in_after_fee)
        let numerator = reserve_out
            .checked_mul(amount_in_after_fee)
            .ok_or(DeFiError::InsufficientLiquidity)?;
        let denominator = reserve_in
            .checked_add(amount_in_after_fee)
            .ok_or(DeFiError::InsufficientLiquidity)?;

        if denominator == 0 {
            return Err(DeFiError::PoolEmpty);
        }

        let amount_out = numerator / denominator;
        if amount_out == 0 {
            return Err(DeFiError::InsufficientLiquidity);
        }

        // Price impact: compare effective price to spot price.
        // Spot price (token_out per token_in) = reserve_out / reserve_in
        // Effective price = amount_out / amount_in_after_fee
        // Impact = 1 - (effective / spot)  expressed in bps.
        let price_impact_bps = self.price_impact(token_in, amount_in);

        let (new_reserve_a, new_reserve_b) = if is_a_to_b {
            (
                self.reserve_a + amount_in, // full amount_in goes to pool (fee stays in pool)
                self.reserve_b - amount_out,
            )
        } else {
            (
                self.reserve_a - amount_out,
                self.reserve_b + amount_in,
            )
        };

        Ok(SwapResult {
            amount_in,
            amount_out,
            fee_amount,
            price_impact_bps,
            new_reserve_a,
            new_reserve_b,
        })
    }

    /// Add liquidity to the pool. Returns the number of LP shares minted.
    ///
    /// For the first deposit, shares = sqrt(amount_a * amount_b).
    /// For subsequent deposits, shares are proportional to the smaller ratio.
    pub fn add_liquidity(
        &mut self,
        amount_a: u128,
        amount_b: u128,
    ) -> Result<u128, DeFiError> {
        if amount_a == 0 || amount_b == 0 {
            return Err(DeFiError::ZeroAmount);
        }

        let shares = if self.total_lp_shares == 0 {
            // Initial deposit — geometric mean sets the share base.
            isqrt(amount_a * amount_b)
        } else {
            // Mint proportional to the smaller ratio to prevent manipulation.
            let share_a = amount_a * self.total_lp_shares / self.reserve_a;
            let share_b = amount_b * self.total_lp_shares / self.reserve_b;
            share_a.min(share_b)
        };

        if shares == 0 {
            return Err(DeFiError::ZeroAmount);
        }

        self.reserve_a += amount_a;
        self.reserve_b += amount_b;
        self.total_lp_shares += shares;

        Ok(shares)
    }

    /// Remove liquidity from the pool. Returns `(amount_a, amount_b)` withdrawn.
    pub fn remove_liquidity(&mut self, lp_shares: u128) -> Result<(u128, u128), DeFiError> {
        if lp_shares == 0 {
            return Err(DeFiError::ZeroAmount);
        }
        if lp_shares > self.total_lp_shares {
            return Err(DeFiError::InsufficientLiquidity);
        }
        if self.total_lp_shares == 0 {
            return Err(DeFiError::PoolEmpty);
        }

        let amount_a = self.reserve_a * lp_shares / self.total_lp_shares;
        let amount_b = self.reserve_b * lp_shares / self.total_lp_shares;

        self.reserve_a -= amount_a;
        self.reserve_b -= amount_b;
        self.total_lp_shares -= lp_shares;

        Ok((amount_a, amount_b))
    }

    /// Current spot price of token A in terms of token B (18-decimal fixed point).
    ///
    /// Returns `reserve_b * 10^18 / reserve_a`, or 0 if pool is empty.
    pub fn price_a_in_b(&self) -> u128 {
        if self.reserve_a == 0 {
            return 0;
        }
        self.reserve_b * PRICE_PRECISION / self.reserve_a
    }

    /// Estimate price impact of a trade in basis points.
    ///
    /// Price impact measures how much worse the effective price is compared
    /// to the spot price, caused by the trade moving the pool reserves.
    pub fn price_impact(&self, token_in: &[u8; 32], amount_in: u128) -> u16 {
        if self.reserve_a == 0 || self.reserve_b == 0 || amount_in == 0 {
            return 0;
        }

        let (reserve_in, reserve_out) = if *token_in == self.token_a {
            (self.reserve_a, self.reserve_b)
        } else if *token_in == self.token_b {
            (self.reserve_b, self.reserve_a)
        } else {
            return 0;
        };

        // Fee-adjusted input.
        let fee_amount = amount_in * self.fee_bps as u128 / 10_000;
        let amount_in_after_fee = amount_in - fee_amount;

        // Effective output from the constant-product formula.
        let amount_out = reserve_out * amount_in_after_fee / (reserve_in + amount_in_after_fee);
        if amount_out == 0 {
            return 10_000; // 100% impact (trade is too large)
        }

        // Spot price: reserve_out / reserve_in (how much out per 1 in).
        // Effective price: amount_out / amount_in_after_fee.
        // Impact = 1 - (effective / spot)
        //        = 1 - (amount_out * reserve_in) / (amount_in_after_fee * reserve_out)
        let effective_numerator = amount_out as u128 * reserve_in as u128;
        let effective_denominator = amount_in_after_fee as u128 * reserve_out as u128;

        if effective_denominator == 0 {
            return 10_000;
        }

        // Impact in bps: (1 - effective_numerator / effective_denominator) * 10_000
        if effective_numerator >= effective_denominator {
            return 0; // No negative impact (shouldn't happen in AMM)
        }
        let impact = 10_000 - (effective_numerator * 10_000 / effective_denominator) as u16;
        impact
    }

    /// Total value locked in USD (18-decimal fixed point).
    ///
    /// `tvl = reserve_a * price_a + reserve_b * price_b`
    /// where prices are also 18-decimal fixed point.
    pub fn total_value_locked(&self, price_a: u128, price_b: u128) -> u128 {
        let value_a = self.reserve_a.saturating_mul(price_a) / PRICE_PRECISION;
        let value_b = self.reserve_b.saturating_mul(price_b) / PRICE_PRECISION;
        value_a.saturating_add(value_b)
    }
}

// ─── LP position ────────────────────────────────────────────────────────────

/// A liquidity provider's position in a specific pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpPosition {
    /// Owner address.
    pub owner: [u8; 32],
    /// Pool this position belongs to.
    pub pool_id: [u8; 32],
    /// Number of LP shares held.
    pub lp_shares: u128,
    /// Amount of token A deposited.
    pub deposited_a: u128,
    /// Amount of token B deposited.
    pub deposited_b: u128,
    /// Block height at position creation.
    pub created_at: u64,
}

// ─── Stablecoin vault ───────────────────────────────────────────────────────

/// Over-collateralized stablecoin vault (MakerDAO/DAI model).
///
/// Users deposit collateral and mint arcUSD against it. The vault tracks
/// the collateral ratio and triggers liquidation when it falls below the
/// liquidation threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StablecoinVault {
    /// Unique vault identifier.
    pub vault_id: [u8; 32],
    /// Owner of the vault.
    pub owner: [u8; 32],
    /// Address of the collateral token.
    pub collateral_token: [u8; 32],
    /// Amount of collateral deposited.
    pub collateral_amount: u128,
    /// Amount of arcUSD debt minted.
    pub debt_amount: u128,
    /// Minimum collateral ratio required (bps, e.g. 15000 = 150%).
    pub collateral_ratio_bps: u16,
    /// Ratio below which the vault is liquidatable (bps, e.g. 12000 = 120%).
    pub liquidation_ratio_bps: u16,
    /// Annual stability fee (bps, e.g. 200 = 2%).
    pub stability_fee_bps: u16,
    /// Block height at vault creation.
    pub created_at: u64,
    /// Block height of last stability fee accrual.
    pub last_fee_update: u64,
}

impl StablecoinVault {
    /// Create a new empty vault with default parameters.
    pub fn new(owner: [u8; 32], collateral_token: [u8; 32]) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-vault-v1");
        hasher.update(&owner);
        hasher.update(&collateral_token);
        let hash = hasher.finalize();

        Self {
            vault_id: *hash.as_bytes(),
            owner,
            collateral_token,
            collateral_amount: 0,
            debt_amount: 0,
            collateral_ratio_bps: DEFAULT_COLLATERAL_RATIO_BPS,
            liquidation_ratio_bps: DEFAULT_LIQUIDATION_RATIO_BPS,
            stability_fee_bps: DEFAULT_STABILITY_FEE_BPS,
            created_at: 0,
            last_fee_update: 0,
        }
    }

    /// Deposit additional collateral into the vault.
    pub fn deposit_collateral(&mut self, amount: u128) {
        self.collateral_amount = self.collateral_amount.saturating_add(amount);
    }

    /// Withdraw collateral, ensuring the vault stays above the collateral ratio.
    ///
    /// `current_price` is the collateral token price in USD (18 decimals).
    pub fn withdraw_collateral(
        &mut self,
        amount: u128,
        current_price: u128,
    ) -> Result<(), DeFiError> {
        if amount > self.collateral_amount {
            return Err(DeFiError::InsufficientCollateral);
        }

        let new_collateral = self.collateral_amount - amount;

        // If there is outstanding debt, check that the ratio is maintained.
        if self.debt_amount > 0 {
            let ratio = Self::compute_ratio(new_collateral, self.debt_amount, current_price);
            if ratio < self.collateral_ratio_bps {
                return Err(DeFiError::BelowLiquidationRatio);
            }
        }

        self.collateral_amount = new_collateral;
        Ok(())
    }

    /// Mint arcUSD stablecoin against deposited collateral.
    ///
    /// `current_price` is the collateral token price in USD (18 decimals).
    pub fn mint_stablecoin(
        &mut self,
        amount: u128,
        current_price: u128,
    ) -> Result<(), DeFiError> {
        if amount == 0 {
            return Err(DeFiError::ZeroAmount);
        }

        let new_debt = self.debt_amount + amount;
        let ratio = Self::compute_ratio(self.collateral_amount, new_debt, current_price);

        if ratio < self.collateral_ratio_bps {
            return Err(DeFiError::InsufficientCollateral);
        }

        self.debt_amount = new_debt;
        Ok(())
    }

    /// Repay arcUSD debt, reducing the outstanding balance.
    pub fn repay_debt(&mut self, amount: u128) {
        self.debt_amount = self.debt_amount.saturating_sub(amount);
    }

    /// Current collateral ratio in basis points.
    ///
    /// `ratio = (collateral_amount * collateral_price) / debt_amount * 10_000`
    ///
    /// Returns `u16::MAX` if there is no debt.
    pub fn current_ratio(&self, collateral_price: u128) -> u16 {
        if self.debt_amount == 0 {
            return u16::MAX;
        }
        Self::compute_ratio(self.collateral_amount, self.debt_amount, collateral_price)
    }

    /// Whether this vault can be liquidated at the given price.
    pub fn is_liquidatable(&self, collateral_price: u128) -> bool {
        if self.debt_amount == 0 {
            return false;
        }
        let ratio = Self::compute_ratio(self.collateral_amount, self.debt_amount, collateral_price);
        ratio < self.liquidation_ratio_bps
    }

    /// Accrue the annual stability fee over elapsed blocks.
    ///
    /// Formula: `debt += debt * fee_bps / 10_000 * elapsed / blocks_per_year`
    pub fn accrue_stability_fee(&mut self, current_height: u64, blocks_per_year: u64) {
        if blocks_per_year == 0 || current_height <= self.last_fee_update || self.debt_amount == 0 {
            return;
        }

        let elapsed = current_height - self.last_fee_update;
        let fee = self.debt_amount as u128
            * self.stability_fee_bps as u128
            * elapsed as u128
            / (10_000u128 * blocks_per_year as u128);

        self.debt_amount = self.debt_amount.saturating_add(fee);
        self.last_fee_update = current_height;
    }

    /// Compute collateral ratio in basis points.
    ///
    /// `ratio_bps = collateral_value / debt * 10_000`
    /// where `collateral_value = collateral_amount * price / 10^18`.
    ///
    /// Uses careful ordering to avoid u128 overflow while preserving precision.
    fn compute_ratio(collateral: u128, debt: u128, price: u128) -> u16 {
        if debt == 0 {
            return u16::MAX;
        }
        // ratio_bps = collateral * price * 10_000 / (PRICE_PRECISION * debt)
        //
        // Direct computation overflows u128 when collateral ~ 10^21 and price ~ 10^18.
        // Strategy: normalize price to basis-points scale first.
        //   price_bps = price * 10_000 / PRICE_PRECISION
        // This converts price from 18-decimal USD to "price in units of 0.01%".
        // For $2.00: price_bps = 2 * 10^18 * 10_000 / 10^18 = 20_000.
        // For $1.20: price_bps = 1.2 * 10^18 * 10_000 / 10^18 = 12_000.
        //
        // Then: ratio_bps = collateral * price_bps / debt
        // This works as long as collateral * price_bps fits in u128,
        // which holds for collateral up to ~10^34 and price_bps up to ~10^5.
        let price_bps = price * 10_000 / PRICE_PRECISION;
        let ratio = collateral
            .checked_mul(price_bps)
            .unwrap_or(u128::MAX)
            / debt;
        ratio.min(u16::MAX as u128) as u16
    }
}

// ─── Limit order ────────────────────────────────────────────────────────────

/// A limit order on the order book.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitOrder {
    /// Unique order identifier.
    pub order_id: [u8; 32],
    /// Owner address.
    pub owner: [u8; 32],
    /// Pool this order targets.
    pub pool_id: [u8; 32],
    /// True for buy orders, false for sell orders.
    pub is_buy: bool,
    /// Limit price in token_b per token_a (18 decimal places).
    pub price: u128,
    /// Total order amount (in token_a for buy, token_b for sell).
    pub amount: u128,
    /// Amount already filled.
    pub filled: u128,
    /// Block height at order creation.
    pub created_at: u64,
    /// Block height after which the order expires.
    pub expires_at: u64,
    /// Current order status.
    pub status: OrderStatus,
}

impl LimitOrder {
    /// Create a new limit order.
    ///
    /// - `ttl_blocks`: number of blocks until expiry.
    /// - `current_height`: current block height (used for timestamps and expiry).
    pub fn new(
        owner: [u8; 32],
        pool_id: [u8; 32],
        is_buy: bool,
        price: u128,
        amount: u128,
        ttl_blocks: u64,
        current_height: u64,
    ) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-order-v1");
        hasher.update(&owner);
        hasher.update(&pool_id);
        hasher.update(&current_height.to_le_bytes());
        hasher.update(&amount.to_le_bytes());
        let hash = hasher.finalize();

        Self {
            order_id: *hash.as_bytes(),
            owner,
            pool_id,
            is_buy,
            price,
            amount,
            filled: 0,
            created_at: current_height,
            expires_at: current_height + ttl_blocks,
            status: OrderStatus::Open,
        }
    }

    /// Fill the order by `amount`. Returns the actual amount filled
    /// (capped at the remaining unfilled quantity).
    pub fn fill(&mut self, amount: u128) -> u128 {
        let remaining = self.remaining();
        let actual = amount.min(remaining);
        self.filled += actual;

        if self.filled >= self.amount {
            self.status = OrderStatus::Filled;
        } else if self.filled > 0 {
            self.status = OrderStatus::PartiallyFilled;
        }

        actual
    }

    /// Whether the order has expired at the given block height.
    pub fn is_expired(&self, current_height: u64) -> bool {
        current_height >= self.expires_at
    }

    /// Remaining unfilled amount.
    pub fn remaining(&self) -> u128 {
        self.amount.saturating_sub(self.filled)
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Integer square root (Newton's method). Used for initial LP share calculation.
fn isqrt(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn token(n: u8) -> [u8; 32] {
        let mut addr = [0u8; 32];
        addr[0] = n;
        addr
    }

    // 1. Pool creation — new pool is empty
    #[test]
    fn test_pool_creation() {
        let pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);
        assert_eq!(pool.reserve_a, 0);
        assert_eq!(pool.reserve_b, 0);
        assert_eq!(pool.total_lp_shares, 0);
        assert_eq!(pool.fee_bps, 30);
        assert_eq!(pool.token_a, token(1));
        assert_eq!(pool.token_b, token(2));
        assert_eq!(pool.cumulative_volume, 0);
        assert_ne!(pool.pool_id, [0u8; 32], "pool_id should be deterministic and non-zero");
    }

    // 2. Initial liquidity deposit sets the ratio and mints sqrt(a*b) shares
    #[test]
    fn test_add_liquidity_initial() {
        let mut pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);

        let shares = pool.add_liquidity(1_000_000, 4_000_000).unwrap();
        // sqrt(1M * 4M) = sqrt(4 * 10^12) = 2_000_000
        assert_eq!(shares, 2_000_000);
        assert_eq!(pool.reserve_a, 1_000_000);
        assert_eq!(pool.reserve_b, 4_000_000);
        assert_eq!(pool.total_lp_shares, 2_000_000);
    }

    // 3. Subsequent liquidity deposit maintains ratio
    #[test]
    fn test_add_liquidity_subsequent() {
        let mut pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);

        // Initial: 1M / 4M, shares = 2M
        pool.add_liquidity(1_000_000, 4_000_000).unwrap();
        let initial_shares = pool.total_lp_shares;

        // Add same ratio: 500K / 2M (half the initial)
        let new_shares = pool.add_liquidity(500_000, 2_000_000).unwrap();

        // Should get half the initial shares
        assert_eq!(new_shares, initial_shares / 2);
        assert_eq!(pool.reserve_a, 1_500_000);
        assert_eq!(pool.reserve_b, 6_000_000);
    }

    // 4. Swap follows constant product math (x * y = k)
    #[test]
    fn test_swap_constant_product() {
        let mut pool = LiquidityPool::new(token(1), token(2), 0); // Zero fee for clean math
        pool.add_liquidity(1_000_000, 1_000_000).unwrap();

        let result = pool.calculate_swap(&token(1), 100_000).unwrap();

        // With zero fee: out = 1M * 100K / (1M + 100K) = 100_000_000_000 / 1_100_000 ≈ 90909
        assert_eq!(result.amount_out, 90909);

        // Verify k is maintained (approximately — integer rounding).
        let k_before: u128 = 1_000_000 * 1_000_000;
        let k_after: u128 = result.new_reserve_a * result.new_reserve_b;
        // k_after should be >= k_before (fees make k grow; with 0 fee it stays equal or rounds up)
        assert!(
            k_after >= k_before - 1, // Allow rounding of 1
            "k should be preserved: before={}, after={}",
            k_before,
            k_after
        );
    }

    // 5. Fee is deducted from input before computing output
    #[test]
    fn test_swap_with_fee() {
        let mut pool_no_fee = LiquidityPool::new(token(1), token(2), 0);
        pool_no_fee.add_liquidity(1_000_000, 1_000_000).unwrap();

        let mut pool_with_fee = LiquidityPool::new(token(1), token(2), 30); // 0.30%
        pool_with_fee.add_liquidity(1_000_000, 1_000_000).unwrap();

        let result_no_fee = pool_no_fee.calculate_swap(&token(1), 100_000).unwrap();
        let result_with_fee = pool_with_fee.calculate_swap(&token(1), 100_000).unwrap();

        // Fee should reduce output
        assert!(
            result_with_fee.amount_out < result_no_fee.amount_out,
            "Fee pool output {} should be less than no-fee output {}",
            result_with_fee.amount_out,
            result_no_fee.amount_out
        );

        // Fee amount should be 0.30% of 100_000 = 300
        assert_eq!(result_with_fee.fee_amount, 300);
    }

    // 6. Swap on empty pool fails gracefully
    #[test]
    fn test_swap_insufficient_liquidity() {
        let pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);

        let result = pool.calculate_swap(&token(1), 100_000);
        assert!(result.is_err());

        // Also test invalid token
        let mut pool2 = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);
        pool2.add_liquidity(1_000_000, 1_000_000).unwrap();
        let result2 = pool2.calculate_swap(&token(99), 100_000);
        assert!(result2.is_err());
    }

    // 7. Remove liquidity returns proportional amounts
    #[test]
    fn test_remove_liquidity() {
        let mut pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);
        let shares = pool.add_liquidity(1_000_000, 4_000_000).unwrap();

        // Remove half the shares
        let (amount_a, amount_b) = pool.remove_liquidity(shares / 2).unwrap();

        assert_eq!(amount_a, 500_000);
        assert_eq!(amount_b, 2_000_000);
        assert_eq!(pool.reserve_a, 500_000);
        assert_eq!(pool.reserve_b, 2_000_000);
        assert_eq!(pool.total_lp_shares, shares / 2);

        // Remove the rest
        let remaining = pool.total_lp_shares;
        let (rest_a, rest_b) = pool.remove_liquidity(remaining).unwrap();
        assert_eq!(rest_a, 500_000);
        assert_eq!(rest_b, 2_000_000);
        assert_eq!(pool.reserve_a, 0);
        assert_eq!(pool.reserve_b, 0);
    }

    // 8. Larger trades produce higher price impact
    #[test]
    fn test_price_impact() {
        let mut pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);
        pool.add_liquidity(10_000_000, 10_000_000).unwrap();

        let small_impact = pool.price_impact(&token(1), 10_000); // 0.1% of reserves
        let large_impact = pool.price_impact(&token(1), 5_000_000); // 50% of reserves

        assert!(
            large_impact > small_impact,
            "Large trade impact {} should exceed small trade impact {}",
            large_impact,
            small_impact
        );

        // Small trade should have minimal impact
        assert!(small_impact < 100, "Small trade impact should be < 1%, got {} bps", small_impact);

        // Huge trade should have significant impact
        assert!(large_impact > 1000, "Large trade impact should be > 10%, got {} bps", large_impact);
    }

    // 9. New stablecoin vault is empty
    #[test]
    fn test_stablecoin_vault_creation() {
        let vault = StablecoinVault::new(token(1), token(10));
        assert_eq!(vault.collateral_amount, 0);
        assert_eq!(vault.debt_amount, 0);
        assert_eq!(vault.collateral_ratio_bps, DEFAULT_COLLATERAL_RATIO_BPS);
        assert_eq!(vault.liquidation_ratio_bps, DEFAULT_LIQUIDATION_RATIO_BPS);
        assert_eq!(vault.stability_fee_bps, DEFAULT_STABILITY_FEE_BPS);
        assert_ne!(vault.vault_id, [0u8; 32]);
    }

    // 10. Mint stablecoin within acceptable collateral ratio
    #[test]
    fn test_stablecoin_mint() {
        let mut vault = StablecoinVault::new(token(1), token(10));

        // Deposit 1000 units of collateral
        vault.deposit_collateral(1_000 * PRICE_PRECISION);

        // Price: $2.00 per collateral token
        let price = 2 * PRICE_PRECISION;

        // Collateral value = 1000 * $2 = $2000
        // At 150% ratio, max debt = $2000 / 1.5 = ~$1333
        // Mint $1000 (should succeed: ratio = 2000/1000 = 200%)
        vault.mint_stablecoin(1_000 * PRICE_PRECISION, price).unwrap();

        assert_eq!(vault.debt_amount, 1_000 * PRICE_PRECISION);

        // Current ratio should be 200% = 20000 bps
        let ratio = vault.current_ratio(price);
        assert_eq!(ratio, 20000);
    }

    // 11. Mint fails when it would breach collateral ratio
    #[test]
    fn test_stablecoin_mint_overcollateralized() {
        let mut vault = StablecoinVault::new(token(1), token(10));
        vault.deposit_collateral(1_000 * PRICE_PRECISION);

        let price = 2 * PRICE_PRECISION; // $2 per token

        // Collateral value = $2000, at 150% ratio max debt = ~$1333
        // Try to mint $1400 — should fail
        let result = vault.mint_stablecoin(1_400 * PRICE_PRECISION, price);
        assert!(result.is_err(), "Minting beyond ratio should fail");
    }

    // 12. Liquidation detection when price drops
    #[test]
    fn test_stablecoin_liquidation_check() {
        let mut vault = StablecoinVault::new(token(1), token(10));
        vault.deposit_collateral(1_000 * PRICE_PRECISION);

        let good_price = 2 * PRICE_PRECISION; // $2
        vault.mint_stablecoin(1_000 * PRICE_PRECISION, good_price).unwrap();

        // At $2: ratio = 200%, not liquidatable
        assert!(!vault.is_liquidatable(good_price));

        // Price drops to $1.10: ratio = 1100/1000 = 110% < 120% liquidation threshold
        let bad_price = PRICE_PRECISION * 11 / 10; // $1.10
        assert!(
            vault.is_liquidatable(bad_price),
            "Vault should be liquidatable at $1.10 (110% < 120%)"
        );

        // Price at exactly $1.20: ratio = 120% = 12000 bps, not liquidatable (not below)
        let borderline_price = PRICE_PRECISION * 12 / 10; // $1.20
        assert!(
            !vault.is_liquidatable(borderline_price),
            "Vault at exactly 120% should NOT be liquidatable"
        );
    }

    // 13. Limit order partial and full fills
    #[test]
    fn test_limit_order_fill() {
        let mut order = LimitOrder::new(
            token(1),
            token(2),
            true,  // buy order
            PRICE_PRECISION, // price = 1.0
            1_000, // total amount
            100,   // TTL: 100 blocks
            50,    // current height
        );

        assert_eq!(order.status, OrderStatus::Open);
        assert_eq!(order.remaining(), 1_000);

        // Partial fill: 400
        let filled = order.fill(400);
        assert_eq!(filled, 400);
        assert_eq!(order.filled, 400);
        assert_eq!(order.remaining(), 600);
        assert_eq!(order.status, OrderStatus::PartiallyFilled);

        // Fill more than remaining: capped at 600
        let filled2 = order.fill(999);
        assert_eq!(filled2, 600);
        assert_eq!(order.filled, 1_000);
        assert_eq!(order.remaining(), 0);
        assert_eq!(order.status, OrderStatus::Filled);
    }

    // 14. Expired orders are detected
    #[test]
    fn test_limit_order_expiry() {
        let order = LimitOrder::new(
            token(1),
            token(2),
            false, // sell order
            PRICE_PRECISION * 2, // price = 2.0
            500,
            100, // TTL: 100 blocks
            50,  // current height = 50, expires at 150
        );

        assert_eq!(order.expires_at, 150);
        assert!(!order.is_expired(100), "Should not be expired at block 100");
        assert!(!order.is_expired(149), "Should not be expired at block 149");
        assert!(order.is_expired(150), "Should be expired at block 150");
        assert!(order.is_expired(200), "Should be expired at block 200");
    }
}
