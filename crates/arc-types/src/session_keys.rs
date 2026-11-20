use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKeyStatus {
    Active,
    Expired,
    Revoked,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    pub max_txs_per_hour: u32,
    pub max_value_per_hour: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPermissions {
    pub allowed_contracts: Vec<[u8; 32]>,
    pub allowed_functions: Vec<[u8; 4]>,
    pub max_value_per_tx: u64,
    pub max_total_value: u64,
    pub max_gas_per_tx: u64,
    pub rate_limit: Option<RateLimit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUsage {
    pub tx_count: u64,
    pub total_value_spent: u64,
    pub total_gas_spent: u64,
    pub last_used: u64,
    pub hourly_tx_count: u32,
    pub hourly_value_spent: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedSessionKey {
    pub key_id: [u8; 32],
    pub public_key: [u8; 32],
    pub permissions: SessionPermissions,
    pub created_at: u64,
    pub expires_at: u64,
    pub usage: SessionUsage,
    pub label: String,
    pub status: SessionKeyStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionKeyManager {
    pub account: [u8; 32],
    pub active_keys: Vec<ManagedSessionKey>,
    pub revoked_keys: Vec<[u8; 32]>,
    pub max_active_keys: usize,
}

impl SessionKeyManager {
    fn derive_key_id(account: &[u8; 32], public_key: &[u8; 32], label: &str) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"session-key-v1");
        hasher.update(account);
        hasher.update(public_key);
        hasher.update(label.as_bytes());
        *hasher.finalize().as_bytes()
    }

    fn find_key_mut(
        &mut self,
        key_id: &[u8; 32],
    ) -> Result<&mut ManagedSessionKey, SessionKeyError> {
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

    fn maybe_reset_hourly(usage: &mut SessionUsage, now: u64) {
        if now >= usage.hour_start + 3600 {
            usage.hourly_tx_count = 0;
            usage.hourly_value_spent = 0;
            usage.hour_start = now - (now % 3600);
        }
    }

    pub fn new(account: [u8; 32], max_active_keys: usize) -> Self {
        Self {
            account,
            active_keys: Vec::new(),
            revoked_keys: Vec::new(),
            max_active_keys,
        }
    }

    pub fn create_key(
        &mut self,
        public_key: [u8; 32],
        permissions: SessionPermissions,
        duration: u64,
        label: String,
    ) -> Result<[u8; 32], SessionKeyError> {
        let active_count = self
            .active_keys
            .iter()
            .filter(|k| k.status == SessionKeyStatus::Active)
            .count();
        if active_count >= self.max_active_keys {
            return Err(SessionKeyError::TooManyActiveKeys);
        }

        let key_id = Self::derive_key_id(&self.account, &public_key, &label);
        let now = 0u64;

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

        match key.status {
            SessionKeyStatus::Revoked => return Err(SessionKeyError::KeyRevoked),
            SessionKeyStatus::Expired => return Err(SessionKeyError::KeyExpired),
            SessionKeyStatus::Exhausted => return Err(SessionKeyError::KeyExhausted),
            SessionKeyStatus::Active => {}
        }

        if now >= key.expires_at {
            return Err(SessionKeyError::KeyExpired);
        }

        if !key.permissions.allowed_contracts.is_empty()
            && !key.permissions.allowed_contracts.contains(&contract)
        {
            return Err(SessionKeyError::ContractNotAllowed);
        }

        if !key.permissions.allowed_functions.is_empty()
            && !key
                .permissions
                .allowed_functions
                .contains(&function_selector)
        {
            return Err(SessionKeyError::FunctionNotAllowed);
        }

        if value > key.permissions.max_value_per_tx {
            return Err(SessionKeyError::ValueExceedsLimit);
        }

        if key.usage.total_value_spent + value > key.permissions.max_total_value {
            return Err(SessionKeyError::KeyExhausted);
        }

        if gas > key.permissions.max_gas_per_tx {
            return Err(SessionKeyError::ValueExceedsLimit);
        }

        if let Some(ref rl) = key.permissions.rate_limit {
            let mut hourly_tx = key.usage.hourly_tx_count;
            let mut hourly_val = key.usage.hourly_value_spent;

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

        if key.usage.total_value_spent >= key.permissions.max_total_value {
            key.status = SessionKeyStatus::Exhausted;
        }

        Ok(())
    }

    pub fn revoke_key(&mut self, key_id: [u8; 32]) -> Result<(), SessionKeyError> {
        let key = self.find_key_mut(&key_id)?;
        key.status = SessionKeyStatus::Revoked;
        self.revoked_keys.push(key_id);
        Ok(())
    }

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

    pub fn cleanup_expired(&mut self, now: u64) -> usize {
        let before = self.active_keys.len();
        self.active_keys.retain(|k| {
            if k.status == SessionKeyStatus::Active && now >= k.expires_at {
                false
            } else {
                k.status != SessionKeyStatus::Expired
            }
        });
        before - self.active_keys.len()
    }

    pub fn get_active_keys(&self) -> Vec<&ManagedSessionKey> {
        self.active_keys
            .iter()
            .filter(|k| k.status == SessionKeyStatus::Active)
            .collect()
    }

    pub fn is_valid_key(&self, key_id: [u8; 32], now: u64) -> bool {
        match self.find_key(&key_id) {
            Ok(key) => key.status == SessionKeyStatus::Active && now < key.expires_at,
            Err(_) => false,
        }
    }
}

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
        mgr.validate_transaction(kid, addr(50), selector(1), 100, 21_000, 1000)
            .unwrap();
    }

    #[test]
    fn test_validate_expired_key() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), default_permissions(), 100, "k1".into())
            .unwrap();
        let result = mgr.validate_transaction(kid, addr(50), selector(1), 100, 21_000, 200);
        assert_eq!(result.unwrap_err(), SessionKeyError::KeyExpired);
    }

    #[test]
    fn test_validate_contract_not_allowed() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        let result = mgr.validate_transaction(kid, addr(99), selector(0xAA), 100, 21_000, 10);
        assert_eq!(result.unwrap_err(), SessionKeyError::ContractNotAllowed);
    }

    #[test]
    fn test_validate_function_not_allowed() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        let result = mgr.validate_transaction(kid, addr(50), selector(0xFF), 100, 21_000, 10);
        assert_eq!(result.unwrap_err(), SessionKeyError::FunctionNotAllowed);
    }

    #[test]
    fn test_validate_value_exceeds_per_tx() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        let result = mgr.validate_transaction(kid, addr(50), selector(0xAA), 2_000, 21_000, 10);
        assert_eq!(result.unwrap_err(), SessionKeyError::ValueExceedsLimit);
    }

    #[test]
    fn test_validate_total_value_exhausted() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        mgr.record_usage(kid, 4_500, 21_000, 10).unwrap();
        let result = mgr.validate_transaction(kid, addr(50), selector(0xAA), 600, 21_000, 20);
        assert_eq!(result.unwrap_err(), SessionKeyError::KeyExhausted);
    }

    #[test]
    fn test_validate_rate_limit_txs() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        for i in 0..5u64 {
            mgr.record_usage(kid, 100, 21_000, 10 + i).unwrap();
        }
        let result = mgr.validate_transaction(kid, addr(50), selector(0xAA), 100, 21_000, 15);
        assert_eq!(result.unwrap_err(), SessionKeyError::RateLimitExceeded);
    }

    #[test]
    fn test_rate_limit_resets_after_hour() {
        let mut mgr = default_manager();
        let kid = mgr
            .create_key(addr(20), restricted_permissions(), 86400, "k1".into())
            .unwrap();
        for i in 0..5u64 {
            mgr.record_usage(kid, 100, 21_000, i).unwrap();
        }
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
        let kid = mgr.create_key(addr(20), perms, 86400, "k1".into()).unwrap();

        mgr.record_usage(kid, 1000, 21_000, 10).unwrap();
        assert_eq!(mgr.active_keys[0].usage.tx_count, 1);
        assert_eq!(mgr.active_keys[0].usage.total_value_spent, 1000);

        mgr.record_usage(kid, 1000, 21_000, 20).unwrap();
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
        let result = mgr.validate_transaction(kid, addr(50), selector(1), 100, 999_999, 10);
        assert_eq!(result.unwrap_err(), SessionKeyError::ValueExceedsLimit);
    }
}
