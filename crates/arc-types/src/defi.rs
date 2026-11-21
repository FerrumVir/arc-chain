use serde::{Deserialize, Serialize};
use std::fmt;

const PRICE_PRECISION: u128 = 1_000_000_000_000_000_000;

pub const DEFAULT_SWAP_FEE_BPS: u16 = 30;

pub const DEFAULT_COLLATERAL_RATIO_BPS: u16 = 15000;

pub const DEFAULT_LIQUIDATION_RATIO_BPS: u16 = 12000;

pub const DEFAULT_STABILITY_FEE_BPS: u16 = 200;

#[derive(Debug)]
pub enum DeFiError {
    InsufficientLiquidity,
    SlippageExceeded,
    DeadlineExpired,
    InvalidToken,
    InsufficientCollateral,
    BelowLiquidationRatio,
    ZeroAmount,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PriceSource {
    Pool([u8; 32]),
    Oracle,
    Vrf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceEntry {
    pub token: [u8; 32],
    pub price_usd: u128,
    pub timestamp: u64,
    pub source: PriceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidityPool {
    pub pool_id: [u8; 32],
    pub token_a: [u8; 32],
    pub token_b: [u8; 32],
    pub reserve_a: u128,
    pub reserve_b: u128,
    pub total_lp_shares: u128,
    pub fee_bps: u16,
    pub created_at: u64,
    pub cumulative_volume: u128,
}

#[derive(Debug, Clone)]
pub struct SwapResult {
    pub amount_in: u128,
    pub amount_out: u128,
    pub fee_amount: u128,
    pub price_impact_bps: u16,
    pub new_reserve_a: u128,
    pub new_reserve_b: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapParams {
    pub pool_id: [u8; 32],
    pub token_in: [u8; 32],
    pub amount_in: u128,
    pub min_amount_out: u128,
    pub deadline: u64,
}

impl LiquidityPool {
    pub fn new(token_a: [u8; 32], token_b: [u8; 32], fee_bps: u16) -> Self {
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

        let fee_amount = amount_in * self.fee_bps as u128 / 10_000;
        let amount_in_after_fee = amount_in - fee_amount;

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

        let price_impact_bps = self.price_impact(token_in, amount_in);

        let (new_reserve_a, new_reserve_b) = if is_a_to_b {
            (self.reserve_a + amount_in, self.reserve_b - amount_out)
        } else {
            (self.reserve_a - amount_out, self.reserve_b + amount_in)
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

    pub fn add_liquidity(&mut self, amount_a: u128, amount_b: u128) -> Result<u128, DeFiError> {
        if amount_a == 0 || amount_b == 0 {
            return Err(DeFiError::ZeroAmount);
        }

        let shares = if self.total_lp_shares == 0 {
            isqrt(amount_a * amount_b)
        } else {
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

    pub fn price_a_in_b(&self) -> u128 {
        if self.reserve_a == 0 {
            return 0;
        }
        self.reserve_b * PRICE_PRECISION / self.reserve_a
    }

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

        let fee_amount = amount_in * self.fee_bps as u128 / 10_000;
        let amount_in_after_fee = amount_in - fee_amount;

        let amount_out = reserve_out * amount_in_after_fee / (reserve_in + amount_in_after_fee);
        if amount_out == 0 {
            return 10_000;
        }

        let effective_numerator = amount_out * reserve_in;
        let effective_denominator = amount_in_after_fee * reserve_out;

        if effective_denominator == 0 {
            return 10_000;
        }

        if effective_numerator >= effective_denominator {
            return 0;
        }

        10_000 - (effective_numerator * 10_000 / effective_denominator) as u16
    }

    pub fn total_value_locked(&self, price_a: u128, price_b: u128) -> u128 {
        let value_a = self.reserve_a.saturating_mul(price_a) / PRICE_PRECISION;
        let value_b = self.reserve_b.saturating_mul(price_b) / PRICE_PRECISION;
        value_a.saturating_add(value_b)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LpPosition {
    pub owner: [u8; 32],
    pub pool_id: [u8; 32],
    pub lp_shares: u128,
    pub deposited_a: u128,
    pub deposited_b: u128,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StablecoinVault {
    pub vault_id: [u8; 32],
    pub owner: [u8; 32],
    pub collateral_token: [u8; 32],
    pub collateral_amount: u128,
    pub debt_amount: u128,
    pub collateral_ratio_bps: u16,
    pub liquidation_ratio_bps: u16,
    pub stability_fee_bps: u16,
    pub created_at: u64,
    pub last_fee_update: u64,
}

impl StablecoinVault {
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

    pub fn deposit_collateral(&mut self, amount: u128) {
        self.collateral_amount = self.collateral_amount.saturating_add(amount);
    }

    pub fn withdraw_collateral(
        &mut self,
        amount: u128,
        current_price: u128,
    ) -> Result<(), DeFiError> {
        if amount > self.collateral_amount {
            return Err(DeFiError::InsufficientCollateral);
        }

        let new_collateral = self.collateral_amount - amount;

        if self.debt_amount > 0 {
            let ratio = Self::compute_ratio(new_collateral, self.debt_amount, current_price);
            if ratio < self.collateral_ratio_bps {
                return Err(DeFiError::BelowLiquidationRatio);
            }
        }

        self.collateral_amount = new_collateral;
        Ok(())
    }

    pub fn mint_stablecoin(&mut self, amount: u128, current_price: u128) -> Result<(), DeFiError> {
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

    pub fn repay_debt(&mut self, amount: u128) {
        self.debt_amount = self.debt_amount.saturating_sub(amount);
    }

    pub fn current_ratio(&self, collateral_price: u128) -> u16 {
        if self.debt_amount == 0 {
            return u16::MAX;
        }
        Self::compute_ratio(self.collateral_amount, self.debt_amount, collateral_price)
    }

    pub fn is_liquidatable(&self, collateral_price: u128) -> bool {
        if self.debt_amount == 0 {
            return false;
        }
        let ratio = Self::compute_ratio(self.collateral_amount, self.debt_amount, collateral_price);
        ratio < self.liquidation_ratio_bps
    }

    pub fn accrue_stability_fee(&mut self, current_height: u64, blocks_per_year: u64) {
        if blocks_per_year == 0 || current_height <= self.last_fee_update || self.debt_amount == 0 {
            return;
        }

        let elapsed = current_height - self.last_fee_update;
        let fee = self.debt_amount * self.stability_fee_bps as u128 * elapsed as u128
            / (10_000u128 * blocks_per_year as u128);

        self.debt_amount = self.debt_amount.saturating_add(fee);
        self.last_fee_update = current_height;
    }

    fn compute_ratio(collateral: u128, debt: u128, price: u128) -> u16 {
        if debt == 0 {
            return u16::MAX;
        }
        let price_bps = price * 10_000 / PRICE_PRECISION;
        let ratio = collateral.saturating_mul(price_bps) / debt;
        ratio.min(u16::MAX as u128) as u16
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitOrder {
    pub order_id: [u8; 32],
    pub owner: [u8; 32],
    pub pool_id: [u8; 32],
    pub is_buy: bool,
    pub price: u128,
    pub amount: u128,
    pub filled: u128,
    pub created_at: u64,
    pub expires_at: u64,
    pub status: OrderStatus,
}

impl LimitOrder {
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

    pub fn is_expired(&self, current_height: u64) -> bool {
        current_height >= self.expires_at
    }

    pub fn remaining(&self) -> u128 {
        self.amount.saturating_sub(self.filled)
    }
}

fn isqrt(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(n: u8) -> [u8; 32] {
        let mut addr = [0u8; 32];
        addr[0] = n;
        addr
    }

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
        assert_ne!(
            pool.pool_id, [0u8; 32],
            "pool_id should be deterministic and non-zero"
        );
    }

    #[test]
    fn test_add_liquidity_initial() {
        let mut pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);

        let shares = pool.add_liquidity(1_000_000, 4_000_000).unwrap();
        assert_eq!(shares, 2_000_000);
        assert_eq!(pool.reserve_a, 1_000_000);
        assert_eq!(pool.reserve_b, 4_000_000);
        assert_eq!(pool.total_lp_shares, 2_000_000);
    }

    #[test]
    fn test_add_liquidity_subsequent() {
        let mut pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);

        pool.add_liquidity(1_000_000, 4_000_000).unwrap();
        let initial_shares = pool.total_lp_shares;

        let new_shares = pool.add_liquidity(500_000, 2_000_000).unwrap();

        assert_eq!(new_shares, initial_shares / 2);
        assert_eq!(pool.reserve_a, 1_500_000);
        assert_eq!(pool.reserve_b, 6_000_000);
    }

    #[test]
    fn test_swap_constant_product() {
        let mut pool = LiquidityPool::new(token(1), token(2), 0);
        pool.add_liquidity(1_000_000, 1_000_000).unwrap();

        let result = pool.calculate_swap(&token(1), 100_000).unwrap();

        assert_eq!(result.amount_out, 90909);

        let k_before: u128 = 1_000_000 * 1_000_000;
        let k_after: u128 = result.new_reserve_a * result.new_reserve_b;
        assert!(
            k_after >= k_before - 1,
            "k should be preserved: before={}, after={}",
            k_before,
            k_after
        );
    }

    #[test]
    fn test_swap_with_fee() {
        let mut pool_no_fee = LiquidityPool::new(token(1), token(2), 0);
        pool_no_fee.add_liquidity(1_000_000, 1_000_000).unwrap();

        let mut pool_with_fee = LiquidityPool::new(token(1), token(2), 30);
        pool_with_fee.add_liquidity(1_000_000, 1_000_000).unwrap();

        let result_no_fee = pool_no_fee.calculate_swap(&token(1), 100_000).unwrap();
        let result_with_fee = pool_with_fee.calculate_swap(&token(1), 100_000).unwrap();

        assert!(
            result_with_fee.amount_out < result_no_fee.amount_out,
            "Fee pool output {} should be less than no-fee output {}",
            result_with_fee.amount_out,
            result_no_fee.amount_out
        );

        assert_eq!(result_with_fee.fee_amount, 300);
    }

    #[test]
    fn test_swap_insufficient_liquidity() {
        let pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);

        let result = pool.calculate_swap(&token(1), 100_000);
        assert!(result.is_err());

        let mut pool2 = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);
        pool2.add_liquidity(1_000_000, 1_000_000).unwrap();
        let result2 = pool2.calculate_swap(&token(99), 100_000);
        assert!(result2.is_err());
    }

    #[test]
    fn test_remove_liquidity() {
        let mut pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);
        let shares = pool.add_liquidity(1_000_000, 4_000_000).unwrap();

        let (amount_a, amount_b) = pool.remove_liquidity(shares / 2).unwrap();

        assert_eq!(amount_a, 500_000);
        assert_eq!(amount_b, 2_000_000);
        assert_eq!(pool.reserve_a, 500_000);
        assert_eq!(pool.reserve_b, 2_000_000);
        assert_eq!(pool.total_lp_shares, shares / 2);

        let remaining = pool.total_lp_shares;
        let (rest_a, rest_b) = pool.remove_liquidity(remaining).unwrap();
        assert_eq!(rest_a, 500_000);
        assert_eq!(rest_b, 2_000_000);
        assert_eq!(pool.reserve_a, 0);
        assert_eq!(pool.reserve_b, 0);
    }

    #[test]
    fn test_price_impact() {
        let mut pool = LiquidityPool::new(token(1), token(2), DEFAULT_SWAP_FEE_BPS);
        pool.add_liquidity(10_000_000, 10_000_000).unwrap();

        let small_impact = pool.price_impact(&token(1), 10_000);
        let large_impact = pool.price_impact(&token(1), 5_000_000);

        assert!(
            large_impact > small_impact,
            "Large trade impact {} should exceed small trade impact {}",
            large_impact,
            small_impact
        );

        assert!(
            small_impact < 100,
            "Small trade impact should be < 1%, got {} bps",
            small_impact
        );

        assert!(
            large_impact > 1000,
            "Large trade impact should be > 10%, got {} bps",
            large_impact
        );
    }

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

    #[test]
    fn test_stablecoin_mint() {
        let mut vault = StablecoinVault::new(token(1), token(10));

        vault.deposit_collateral(1_000 * PRICE_PRECISION);

        let price = 2 * PRICE_PRECISION;

        vault
            .mint_stablecoin(1_000 * PRICE_PRECISION, price)
            .unwrap();

        assert_eq!(vault.debt_amount, 1_000 * PRICE_PRECISION);

        let ratio = vault.current_ratio(price);
        assert_eq!(ratio, 20000);
    }

    #[test]
    fn test_stablecoin_mint_overcollateralized() {
        let mut vault = StablecoinVault::new(token(1), token(10));
        vault.deposit_collateral(1_000 * PRICE_PRECISION);

        let price = 2 * PRICE_PRECISION;

        let result = vault.mint_stablecoin(1_400 * PRICE_PRECISION, price);
        assert!(result.is_err(), "Minting beyond ratio should fail");
    }

    #[test]
    fn test_stablecoin_liquidation_check() {
        let mut vault = StablecoinVault::new(token(1), token(10));
        vault.deposit_collateral(1_000 * PRICE_PRECISION);

        let good_price = 2 * PRICE_PRECISION;
        vault
            .mint_stablecoin(1_000 * PRICE_PRECISION, good_price)
            .unwrap();

        assert!(!vault.is_liquidatable(good_price));

        let bad_price = PRICE_PRECISION * 11 / 10;
        assert!(
            vault.is_liquidatable(bad_price),
            "Vault should be liquidatable at $1.10 (110% < 120%)"
        );

        let borderline_price = PRICE_PRECISION * 12 / 10;
        assert!(
            !vault.is_liquidatable(borderline_price),
            "Vault at exactly 120% should NOT be liquidatable"
        );
    }

    #[test]
    fn test_limit_order_fill() {
        let mut order = LimitOrder::new(token(1), token(2), true, PRICE_PRECISION, 1_000, 100, 50);

        assert_eq!(order.status, OrderStatus::Open);
        assert_eq!(order.remaining(), 1_000);

        let filled = order.fill(400);
        assert_eq!(filled, 400);
        assert_eq!(order.filled, 400);
        assert_eq!(order.remaining(), 600);
        assert_eq!(order.status, OrderStatus::PartiallyFilled);

        let filled2 = order.fill(999);
        assert_eq!(filled2, 600);
        assert_eq!(order.filled, 1_000);
        assert_eq!(order.remaining(), 0);
        assert_eq!(order.status, OrderStatus::Filled);
    }

    #[test]
    fn test_limit_order_expiry() {
        let order = LimitOrder::new(token(1), token(2), false, PRICE_PRECISION * 2, 500, 100, 50);

        assert_eq!(order.expires_at, 150);
        assert!(!order.is_expired(100), "Should not be expired at block 100");
        assert!(!order.is_expired(149), "Should not be expired at block 149");
        assert!(order.is_expired(150), "Should be expired at block 150");
        assert!(order.is_expired(200), "Should be expired at block 200");
    }
}
