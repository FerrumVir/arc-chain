// Add to lib.rs: pub mod session_keys;

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

// ─── Session Key Error ──────────────────────────────────────────────────────

/// Errors that can occur during session key operations.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum SessionKeyError {
    #[error("session key not found")]
    KeyNotFound,
    #[error("session key has expired")]
    KeyExpired,
    #[error("session key has been revoked")]
    KeyRevoked,
    #[error("session key is not authorized for this action")]
    Unauthorized,
    #[error("contract address is not in the allowed list")]
    ContractNotAllowed,
    #[error("function selector is not in the allowed list")]
    FunctionNotAllowed,
    #[error("transaction value exceeds session key limit")]
    ValueExceedsLimit,
    #[error("session key rate limit exceeded")]
    RateLimitExceeded,
    #[error("maximum number of active session keys reached")]
    TooManyActiveKeys,
    #[error("session key spending budget is exhausted")]
    KeyExhausted,
}

// ─── Types ──────────────────────────────────────────────────────────────────

/// Status of a managed session key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKeyStatus {
    /// Key is active and can be used.
    Active,
    /// Key has passed its expiration timestamp.
    Expired,
    /// Key was explicitly revoked.
    Revoked,
    /// Key has spent its total value budget.
    Exhausted,
}

impl fmt::Display for SessionKeyStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => write!(f, "Active"),
            Self::Expired => write!(f, "Expired"),
            Self::Revoked => write!(f, "Revoked"),
            Self::Exhausted => write!(f, "Exhausted"),
        }
    }
}

/// Per-hour rate limiting for a session key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    /// Maximum transactions per rolling hour.
    pub max_txs_per_hour: u32,
    /// Maximum total value per rolling hour.
    pub max_value_per_hour: u64,
}

/// Permission set controlling what a session key can do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPermissions {
    /// Whitelist of contract addresses this key can call (empty = any).
    pub allowed_contracts: Vec<[u8; 32]>,
    /// Whitelist of 4-byte function selectors (empty = any).
    pub allowed_functions: Vec<[u8; 4]>,
    /// Maximum value (in smallest denomination) per single transaction.
    pub max_value_per_tx: u64,
    /// Maximum total value the key can spend over its lifetime.
    pub max_total_value: u64,
    /// Maximum gas per transaction.
    pub max_gas_per_tx: u64,
    /// Optional per-hour rate limit.
    pub rate_limit: Option<RateLimit>,
}

/// Usage counters for a session key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUsage {
    /// Total transactions executed with this key.
    pub tx_count: u64,
    /// Total value spent (cumulative lifetime).
    pub total_value_spent: u64,
    /// Total gas spent (cumulative lifetime).
    pub total_gas_spent: u64,
    /// Timestamp of the last transaction.
    pub last_used: u64,
    /// Transactions in the current hour window.
    pub hourly_tx_count: u32,
    /// Value spent in the current hour window.
    pub hourly_value_spent: u64,
    /// Start of the current hour window.
    pub hour_start: u64,
}

impl SessionUsage {
    fn new() -> Self {
        Self {
            tx_count: 0,
            total_value_spent: 0,
            total_gas_spent: 0,
            last_used: 0,
            hourly_tx_count: 0,
            hourly_value_spent: 0,
            hour_start: 0,
        }
    }
}

/// A managed session key with permissions, usage tracking, and lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedSessionKey {
    /// Unique key identifier (derived from public key + creation params).
    pub key_id: [u8; 32],
    /// Public key of the session signer.
    pub public_key: [u8; 32],
    /// What this key is allowed to do.
    pub permissions: SessionPermissions,
    /// Timestamp when the key was created.
    pub created_at: u64,
    /// Timestamp after which the key is no longer valid.
    pub expires_at: u64,
    /// Cumulative usage counters.
    pub usage: SessionUsage,
    /// Human-readable label.
    pub label: String,
    /// Current lifecycle status.
    pub status: SessionKeyStatus,
}

// ─── Session Key Manager ────────────────────────────────────────────────────

/// Manages session keys for a smart account.
///
/// Session keys enable delegated signing with fine-grained permission
/// controls, spending limits, rate limits, and expiration. This is essential
/// for dApp interactions where the primary key should not be exposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionKeyManager {
    /// The account address that owns these session keys.
    pub account: [u8; 32],
    /// Currently active (non-revoked, non-expired) session keys.
    pub active_keys: Vec<ManagedSessionKey>,
    /// Key IDs that have been explicitly revoked.
    pub revoked_keys: Vec<[u8; 32]>,
    /// Maximum number of concurrent active keys.
    pub max_active_keys: usize,
}

impl SessionKeyManager {
    // ── Helpers ──────────────────────────────────────────────────────────

    fn derive_key_id(account: &[u8; 32], public_key: &[u8; 32], label: &str) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"session-key-v1");
        hasher.update(account);
        hasher.update(public_key);
        hasher.update(label.as_bytes());
        *hasher.finalize().as_bytes()
    }

    fn find_key_mut(&mut self, key_id: &[u8; 32]) -> Result<&mut ManagedSessionKey, SessionKeyError> {
        self.active_keys
            .iter_mut()
            .find(|k| k.key_id == *key_id)
            .ok_or(SessionKeyError::KeyNotFound)
    }

    fn find_key(&self, key_id: &[u8; 32]) -> Result<&ManagedSessionKey, SessionKeyError> {
        self.active_keys
            .iter()
            .find(|k| k.key_id == *key_id)
            .ok_or(SessionKeyError::KeyNotFound)
    }

    /// Reset hourly counters if we've moved to a new hour window.
    fn maybe_reset_hourly(usage: &mut SessionUsage, now: u64) {
        if now >= usage.hour_start + 3600 {
            usage.hourly_tx_count = 0;
            usage.hourly_value_spent = 0;
            usage.hour_start = now - (now % 3600);
        }
    }

    // ── Public API ──────────────────────────────────────────────────────

    /// Create a new session key manager for the given account.
    pub fn new(account: [u8; 32], max_active_keys: usize) -> Self {
        Self {
            account,
            active_keys: Vec::new(),
            revoked_keys: Vec::new(),
            max_active_keys,
        }
    }

    /// Create a new session key with the specified permissions and duration.
    ///
    /// Returns the key ID on success.
    ///
    /// # Errors
    /// - `TooManyActiveKeys` if the active key limit has been reached.
    pub fn create_key(
        &mut self,
        public_key: [u8; 32],
        permissions: SessionPermissions,
        duration: u64,
        label: String,
    ) -> Result<[u8; 32], SessionKeyError> {
        // Count only Active keys toward the limit.
        let active_count = self
            .active_keys
            .iter()
            .filter(|k| k.status == SessionKeyStatus::Active)
            .count();
        if active_count >= self.max_active_keys {
            return Err(SessionKeyError::TooManyActiveKeys);
        }

        let key_id = Self::derive_key_id(&self.account, &public_key, &label);
        let now = 0u64; // Creation timestamp set externally or at 0 for determinism.

        let key = ManagedSessionKey {
            key_id,
            public_key,
            permissions,
            created_at: now,
            expires_at: now + duration,
            usage: SessionUsage::new(),
            label,
            status: SessionKeyStatus::Active,
        };

        self.active_keys.push(key);
        Ok(key_id)
    }

    /// Validate whether a session key is allowed to execute a given transaction.
    ///
    /// This checks: status, expiration, contract whitelist, function whitelist,
    /// per-tx value limit, total value budget, gas limit, and rate limit.
    ///
    /// Does NOT record usage — call `record_usage` after successful execution.
    pub fn validate_transaction(
        &self,
        key_id: [u8; 32],
        contract: [u8; 32],
        function_selector: [u8; 4],
        value: u64,
        gas: u64,
        now: u64,
    ) -> Result<(), SessionKeyError> {
        let key = self.find_key(&key_id)?;

        // Check status.
        match key.status {
            SessionKeyStatus::Revoked => return Err(SessionKeyError::KeyRevoked),
            SessionKeyStatus::Expired => return Err(SessionKeyError::KeyExpired),
            SessionKeyStatus::Exhausted => return Err(SessionKeyError::KeyExhausted),
            SessionKeyStatus::Active => {}
        }

        // Check expiration.
        if now >= key.expires_at {
            return Err(SessionKeyError::KeyExpired);
        }

        // Check contract whitelist.
        if !key.permissions.allowed_contracts.is_empty()
            && !key.permissions.allowed_contracts.contains(&contract)
        {
            return Err(SessionKeyError::ContractNotAllowed);
        }

        // Check function selector whitelist.
        if !key.permissions.allowed_functions.is_empty()
            && !key.permissions.allowed_functions.contains(&function_selector)
        {
            return Err(SessionKeyError::FunctionNotAllowed);
        }

        // Check per-tx value.
        if value > key.permissions.max_value_per_tx {
            return Err(SessionKeyError::ValueExceedsLimit);
        }

        // Check total value budget.
        if key.usage.total_value_spent + value > key.permissions.max_total_value {
            return Err(SessionKeyError::KeyExhausted);
        }

        // Check gas limit.
        if gas > key.permissions.max_gas_per_tx {
            return Err(SessionKeyError::ValueExceedsLimit);
        }

        // Check rate limit.
        if let Some(ref rl) = key.permissions.rate_limit {
            let mut hourly_tx = key.usage.hourly_tx_count;
            let mut hourly_val = key.usage.hourly_value_spent;

            // If we've moved to a new hour, counters reset.
            if now >= key.usage.hour_start + 3600 {
                hourly_tx = 0;
                hourly_val = 0;
            }

            if hourly_tx + 1 > rl.max_txs_per_hour {
                return Err(SessionKeyError::RateLimitExceeded);
            }
            if hourly_val + value > rl.max_value_per_hour {
                return Err(SessionKeyError::RateLimitExceeded);
            }
        }

        Ok(())
    }

    /// Record a successful transaction's usage against a session key.
    ///
    /// Updates tx count, value spent, gas spent, and hourly counters.
    /// If the total value budget is exhausted, the key status becomes `Exhausted`.
    pub fn record_usage(
        &mut self,
        key_id: [u8; 32],
        value: u64,
        gas: u64,
        now: u64,
    ) -> Result<(), SessionKeyError> {
        let key = self.find_key_mut(&key_id)?;

        if key.status != SessionKeyStatus::Active {
            return Err(SessionKeyError::KeyRevoked);
        }

        Self::maybe_reset_hourly(&mut key.usage, now);

        key.usage.tx_count += 1;
        key.usage.total_value_spent += value;
        key.usage.total_gas_spent += gas;
        key.usage.last_used = now;
        key.usage.hourly_tx_count += 1;
        key.usage.hourly_value_spent += value;

        // Mark exhausted if budget is fully spent.
        if key.usage.total_value_spent >= key.permissions.max_total_value {
            key.status = SessionKeyStatus::Exhausted;
        }

        Ok(())
    }

    /// Revoke a specific session key.
    ///
    /// # Errors
    /// - `KeyNotFound` if the key id does not exist.
    pub fn revoke_key(&mut self, key_id: [u8; 32]) -> Result<(), SessionKeyError> {
        let key = self.find_key_mut(&key_id)?;
        key.status = SessionKeyStatus::Revoked;
        self.revoked_keys.push(key_id);
        Ok(())
    }

    /// Revoke all active session keys. Returns the number of keys revoked.
    pub fn revoke_all(&mut self) -> usize {
        let mut count = 0;
        for key in &mut self.active_keys {
            if key.status == SessionKeyStatus::Active {
                key.status = SessionKeyStatus::Revoked;
                self.revoked_keys.push(key.key_id);
                count += 1;
            }
        }
        count
    }

    /// Remove expired keys from the active list. Returns the number removed.
    pub fn cleanup_expired(&mut self, now: u64) -> usize {
        let before = self.active_keys.len();
        self.active_keys.retain(|k| {
            if k.status == SessionKeyStatus::Active && now >= k.expires_at {
                // This key expired; don't retain.
                false
            } else if k.status == SessionKeyStatus::Expired {
                false
            } else {
                true
            }
        });
        before - self.active_keys.len()
    }

    /// Get all keys that are currently in `Active` status.
    pub fn get_active_keys(&self) -> Vec<&ManagedSessionKey> {
        self.active_keys
            .iter()
            .filter(|k| k.status == SessionKeyStatus::Active)
            .collect()
    }

    /// Check if a key exists and is valid at the given time.
    pub fn is_valid_key(&self, key_id: [u8; 32], now: u64) -> bool {
        match self.find_key(&key_id) {
            Ok(key) => key.status == SessionKeyStatus::Active && now < key.expires_at,
            Err(_) => false,
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(id: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = id;
        a
    }

    fn selector(id: u8) -> [u8; 4] {
        [id, 0, 0, 0]
    }

    fn default_permissions() -> SessionPermissions {
        SessionPermissions {
            allowed_contracts: vec![],
            allowed_functions: vec![],
            max_value_per_tx: 1_000_000,
            max_total_value: 10_000_000,
            max_gas_per_tx: 500_000,
            rate_limit: None,
        }
    }

    fn restricted_permissions() -> SessionPermissions {
        SessionPermissions {
            allowed_contracts: vec![addr(50), addr(51)],
            allowed_functions: vec![selector(0xAA), selector(0xBB)],
            max_value_per_tx: 1_000,
            max_total_value: 5_000,
            max_gas_per_tx: 100_000,
            rate_limit: Some(RateLimit {
                max_txs_per_hour: 5,
                max_value_per_hour: 3_000,
            }),
        }
    }

    fn default_manager() -> SessionKeyManager {
        SessionKeyManager::new(addr(1), 10)
    }

    #[test]
    fn test_create_key() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), default_permissions(), 86400, "dapp-key".into())
            .unwrap();
        assert_eq!(mgr.active_keys.len(), 1);
        assert_eq!(mgr.active_keys[0].key_id, kid);
        assert_eq!(mgr.active_keys[0].status, SessionKeyStatus::Active);
    }

    #[test]
    fn test_create_key_too_many() {
        let mut mgr = SessionKeyManager::new(addr(1), 2);
        mgr.create_key(addr(20), default_permissions(), 86400, "k1".into())
            .unwrap();
        mgr.create_key(addr(21), default_permissions(), 86400, "k2".into())
            .unwrap();
        let result = mgr.create_key(addr(22), default_permissions(), 86400, "k3".into());
        assert_eq!(result.unwrap_err(), SessionKeyError::TooManyActiveKeys);
    }

    #[test]
    fn test_validate_basic_transaction() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), default_permissions(), 86400, "k1".into())
            .unwrap();
        // Valid transaction within limits, before expiry.
        mgr.validate_transaction(kid, addr(50), selector(1), 100, 21_000, 1000)
            .unwrap();
    }

    #[test]
    fn test_validate_expired_key() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), default_permissions(), 100, "k1".into())
            .unwrap();
        // created_at = 0, expires_at = 100. now = 200 → expired.
        let result = mgr.validate_transaction(kid, addr(50), selector(1), 100, 21_000, 200);
        assert_eq!(result.unwrap_err(), SessionKeyError::KeyExpired);
    }

    #[test]
    fn test_validate_contract_not_allowed() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        // addr(99) is not in the allowed contracts list.
        let result = mgr.validate_transaction(kid, addr(99), selector(0xAA), 100, 21_000, 10);
        assert_eq!(result.unwrap_err(), SessionKeyError::ContractNotAllowed);
    }

    #[test]
    fn test_validate_function_not_allowed() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        // selector(0xFF) is not allowed.
        let result = mgr.validate_transaction(kid, addr(50), selector(0xFF), 100, 21_000, 10);
        assert_eq!(result.unwrap_err(), SessionKeyError::FunctionNotAllowed);
    }

    #[test]
    fn test_validate_value_exceeds_per_tx() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        // max_value_per_tx = 1000, sending 2000.
        let result = mgr.validate_transaction(kid, addr(50), selector(0xAA), 2_000, 21_000, 10);
        assert_eq!(result.unwrap_err(), SessionKeyError::ValueExceedsLimit);
    }

    #[test]
    fn test_validate_total_value_exhausted() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        // Record 4500 worth of usage (budget is 5000).
        mgr.record_usage(kid, 4_500, 21_000, 10).unwrap();
        // Now try to spend 600 more (4500 + 600 > 5000).
        let result = mgr.validate_transaction(kid, addr(50), selector(0xAA), 600, 21_000, 20);
        assert_eq!(result.unwrap_err(), SessionKeyError::KeyExhausted);
    }

    #[test]
    fn test_validate_rate_limit_txs() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        // Burn through 5 txs (the rate limit).
        for i in 0..5u64 {
            mgr.record_usage(kid, 100, 21_000, 10 + i).unwrap();
        }
        // 6th tx should be rate limited.
        let result = mgr.validate_transaction(kid, addr(50), selector(0xAA), 100, 21_000, 15);
        assert_eq!(result.unwrap_err(), SessionKeyError::RateLimitExceeded);
    }

    #[test]
    fn test_rate_limit_resets_after_hour() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        // 5 txs in hour starting at 0.
        for i in 0..5u64 {
            mgr.record_usage(kid, 100, 21_000, i).unwrap();
        }
        // Move to next hour — should pass validation.
        mgr.validate_transaction(kid, addr(50), selector(0xAA), 100, 21_000, 3700)
            .unwrap();
    }

    #[test]
    fn test_record_usage_and_exhaustion() {
        let mut mgr = default_manager();
        let perms = SessionPermissions {
            allowed_contracts: vec![],
            allowed_functions: vec![],
            max_value_per_tx: 1000,
            max_total_value: 2000,
            max_gas_per_tx: 500_000,
            rate_limit: None,
        };
        let kid = mgr
            .create_key(addr(20), perms, 86400, "k1".into())
            .unwrap();

        mgr.record_usage(kid, 1000, 21_000, 10).unwrap();
        assert_eq!(mgr.active_keys[0].usage.tx_count, 1);
        assert_eq!(mgr.active_keys[0].usage.total_value_spent, 1000);

        mgr.record_usage(kid, 1000, 21_000, 20).unwrap();
        // Should now be exhausted (2000 = max_total_value).
        assert_eq!(mgr.active_keys[0].status, SessionKeyStatus::Exhausted);
    }

    #[test]
    fn test_revoke_key() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), default_permissions(), 86400, "k1".into())
            .unwrap();
        mgr.revoke_key(kid).unwrap();
        assert_eq!(mgr.active_keys[0].status, SessionKeyStatus::Revoked);
        assert!(mgr.revoked_keys.contains(&kid));
    }

    #[test]
    fn test_revoke_all() {
        let mut mgr = default_manager();
        mgr.create_key(addr(20), default_permissions(), 86400, "k1".into())
            .unwrap();
        mgr.create_key(addr(21), default_permissions(), 86400, "k2".into())
            .unwrap();
        mgr.create_key(addr(22), default_permissions(), 86400, "k3".into())
            .unwrap();
        let count = mgr.revoke_all();
        assert_eq!(count, 3);
        assert_eq!(mgr.get_active_keys().len(), 0);
    }

    #[test]
    fn test_cleanup_expired() {
        let mut mgr = default_manager();
        mgr.create_key(addr(20), default_permissions(), 100, "short".into())
            .unwrap();
        mgr.create_key(addr(21), default_permissions(), 86400, "long".into())
            .unwrap();

        let removed = mgr.cleanup_expired(500);
        assert_eq!(removed, 1);
        assert_eq!(mgr.active_keys.len(), 1);
        assert_eq!(mgr.active_keys[0].label, "long");
    }

    #[test]
    fn test_get_active_keys() {
        let mut mgr = default_manager();
        let k1 = mgr
            .create_key(addr(20), default_permissions(), 86400, "k1".into())
            .unwrap();
        mgr.create_key(addr(21), default_permissions(), 86400, "k2".into())
            .unwrap();
        mgr.revoke_key(k1).unwrap();

        let active = mgr.get_active_keys();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].label, "k2");
    }

    #[test]
    fn test_is_valid_key() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), default_permissions(), 1000, "k1".into())
            .unwrap();

        assert!(mgr.is_valid_key(kid, 500));
        assert!(!mgr.is_valid_key(kid, 2000));
        assert!(!mgr.is_valid_key([0xFF; 32], 500));
    }

    #[test]
    fn test_validate_revoked_key() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), default_permissions(), 86400, "k1".into())
            .unwrap();
        mgr.revoke_key(kid).unwrap();
        let result = mgr.validate_transaction(kid, addr(50), selector(1), 100, 21_000, 10);
        assert_eq!(result.unwrap_err(), SessionKeyError::KeyRevoked);
    }

    #[test]
    fn test_gas_exceeds_limit() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), default_permissions(), 86400, "k1".into())
            .unwrap();
        // max_gas_per_tx = 500_000, request 999_999.
        let result = mgr.validate_transaction(kid, addr(50), selector(1), 100, 999_999, 10);
        assert_eq!(result.unwrap_err(), SessionKeyError::ValueExceedsLimit);
    }
}
