use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartAccount {
    pub address: [u8; 32],
    pub owner: [u8; 32],
    pub guardians: Vec<Guardian>,
    pub session_keys: Vec<SessionKey>,
    pub spending_limits: Vec<SpendingLimit>,
    pub modules: Vec<AccountModule>,
    pub nonce: u64,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guardian {
    pub address: [u8; 32],
    pub name: String,
    pub weight: u32,
    pub added_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionKey {
    pub key: [u8; 32],
    pub permissions: SessionPermissions,
    pub valid_from: u64,
    pub valid_until: u64,
    pub max_gas_per_tx: u64,
    pub is_revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPermissions {
    pub can_transfer: bool,
    pub can_call_contracts: bool,
    pub allowed_contracts: Vec<[u8; 32]>,
    pub max_value_per_tx: u64,
    pub max_txs_per_day: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendingLimit {
    pub token: [u8; 32],
    pub daily_limit: u64,
    pub spent_today: u64,
    pub last_reset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AccountModule {
    SocialRecovery {
        threshold: u32,
    },
    MultiSig {
        signers: Vec<[u8; 32]>,
        threshold: u32,
    },
    PaymasterSponsored {
        paymaster: [u8; 32],
    },
    AutomationHook {
        contract: [u8; 32],
    },
    DailyLimit {
        amount: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserOperation {
    pub sender: [u8; 32],
    pub nonce: u64,
    pub call_data: Vec<u8>,
    pub call_gas_limit: u64,
    pub verification_gas_limit: u64,
    pub pre_verification_gas: u64,
    pub max_fee_per_gas: u64,
    pub max_priority_fee: u64,
    pub paymaster: Option<[u8; 32]>,
    pub paymaster_data: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRequest {
    pub account: [u8; 32],
    pub new_owner: [u8; 32],
    pub approvals: Vec<RecoveryApproval>,
    pub threshold: u32,
    pub initiated_at: u64,
    pub expires_at: u64,
    pub status: RecoveryStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryApproval {
    pub guardian: [u8; 32],
    pub approved_at: u64,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryStatus {
    Pending,
    Approved,
    Executed,
    Expired,
    Cancelled,
}

pub const DAILY_RESET_BLOCKS: u64 = 216_000;

impl SmartAccount {
    pub fn new(owner: [u8; 32], nonce: u64) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-smart-account-v1");
        hasher.update(&owner);
        hasher.update(&nonce.to_le_bytes());
        let address = *hasher.finalize().as_bytes();

        Self {
            address,
            owner,
            guardians: Vec::new(),
            session_keys: Vec::new(),
            spending_limits: Vec::new(),
            modules: Vec::new(),
            nonce,
            created_at: 0,
        }
    }

    pub fn add_guardian(&mut self, guardian: Guardian) {
        self.guardians.push(guardian);
    }

    pub fn remove_guardian(&mut self, address: &[u8; 32]) -> bool {
        let before = self.guardians.len();
        self.guardians.retain(|g| &g.address != address);
        self.guardians.len() < before
    }

    pub fn add_session_key(&mut self, key: SessionKey) {
        self.session_keys.push(key);
    }

    pub fn revoke_session_key(&mut self, key: &[u8; 32]) -> bool {
        for sk in &mut self.session_keys {
            if &sk.key == key {
                sk.is_revoked = true;
                return true;
            }
        }
        false
    }

    pub fn is_session_key_valid(&self, key: &[u8; 32], current_height: u64) -> bool {
        self.session_keys.iter().any(|sk| {
            &sk.key == key
                && !sk.is_revoked
                && current_height >= sk.valid_from
                && current_height <= sk.valid_until
        })
    }

    pub fn check_spending_limit(&self, token: &[u8; 32], amount: u64) -> bool {
        for limit in &self.spending_limits {
            if &limit.token == token {
                return limit.spent_today.saturating_add(amount) <= limit.daily_limit;
            }
        }
        true
    }

    pub fn record_spend(&mut self, token: &[u8; 32], amount: u64, current_height: u64) {
        for limit in &mut self.spending_limits {
            if &limit.token == token {
                if current_height >= limit.last_reset + DAILY_RESET_BLOCKS {
                    limit.spent_today = 0;
                    limit.last_reset = current_height;
                }
                limit.spent_today = limit.spent_today.saturating_add(amount);
                return;
            }
        }
    }

    pub fn has_module(&self, module_type: &str) -> bool {
        self.modules.iter().any(|m| match m {
            AccountModule::SocialRecovery { .. } => module_type == "SocialRecovery",
            AccountModule::MultiSig { .. } => module_type == "MultiSig",
            AccountModule::PaymasterSponsored { .. } => module_type == "PaymasterSponsored",
            AccountModule::AutomationHook { .. } => module_type == "AutomationHook",
            AccountModule::DailyLimit { .. } => module_type == "DailyLimit",
        })
    }

    pub fn guardian_count(&self) -> usize {
        self.guardians.len()
    }

    pub fn active_session_keys(&self, current_height: u64) -> Vec<&SessionKey> {
        self.session_keys
            .iter()
            .filter(|sk| {
                !sk.is_revoked
                    && current_height >= sk.valid_from
                    && current_height <= sk.valid_until
            })
            .collect()
    }
}

impl RecoveryRequest {
    pub fn new(
        account: [u8; 32],
        new_owner: [u8; 32],
        threshold: u32,
        ttl_blocks: u64,
        current_height: u64,
    ) -> Self {
        Self {
            account,
            new_owner,
            approvals: Vec::new(),
            threshold,
            initiated_at: current_height,
            expires_at: current_height.saturating_add(ttl_blocks),
            status: RecoveryStatus::Pending,
        }
    }

    pub fn add_approval(&mut self, approval: RecoveryApproval) -> bool {
        if self
            .approvals
            .iter()
            .any(|a| a.guardian == approval.guardian)
        {
            return false;
        }
        self.approvals.push(approval);

        if self.approvals.len() as u32 >= self.threshold {
            self.status = RecoveryStatus::Approved;
        }
        true
    }

    pub fn is_approved(&self) -> bool {
        self.status == RecoveryStatus::Approved
    }

    pub fn is_expired(&self, current_height: u64) -> bool {
        current_height > self.expires_at
    }
}

impl UserOperation {
    pub fn total_gas(&self) -> u64 {
        self.call_gas_limit
            .saturating_add(self.verification_gas_limit)
            .saturating_add(self.pre_verification_gas)
    }

    pub fn is_sponsored(&self) -> bool {
        self.paymaster.is_some()
    }

    pub fn hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-userop-v1");
        hasher.update(&self.sender);
        hasher.update(&self.nonce.to_le_bytes());
        hasher.update(&self.call_data);
        hasher.update(&self.call_gas_limit.to_le_bytes());
        hasher.update(&self.verification_gas_limit.to_le_bytes());
        hasher.update(&self.pre_verification_gas.to_le_bytes());
        hasher.update(&self.max_fee_per_gas.to_le_bytes());
        hasher.update(&self.max_priority_fee.to_le_bytes());
        if let Some(ref pm) = self.paymaster {
            hasher.update(pm);
        }
        hasher.update(&self.paymaster_data);
        *hasher.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key(n: u8) -> [u8; 32] {
        let mut k = [0u8; 32];
        k[0] = n;
        k
    }

    fn make_session_key(n: u8, from: u64, until: u64) -> SessionKey {
        SessionKey {
            key: test_key(n),
            permissions: SessionPermissions {
                can_transfer: true,
                can_call_contracts: false,
                allowed_contracts: vec![],
                max_value_per_tx: 1_000_000,
                max_txs_per_day: 100,
            },
            valid_from: from,
            valid_until: until,
            max_gas_per_tx: 50_000,
            is_revoked: false,
        }
    }

    #[test]
    fn test_smart_account_creation() {
        let owner = test_key(1);
        let acct = SmartAccount::new(owner, 0);

        assert_eq!(acct.owner, owner);
        assert_ne!(
            acct.address, [0u8; 32],
            "address should be derived, not zero"
        );
        assert_eq!(acct.nonce, 0);
        assert!(acct.guardians.is_empty());
        assert!(acct.session_keys.is_empty());
        assert!(acct.spending_limits.is_empty());
        assert!(acct.modules.is_empty());
    }

    #[test]
    fn test_add_remove_guardian() {
        let mut acct = SmartAccount::new(test_key(1), 0);

        let guardian = Guardian {
            address: test_key(10),
            name: "Alice".to_string(),
            weight: 1,
            added_at: 100,
        };
        acct.add_guardian(guardian);
        assert_eq!(acct.guardian_count(), 1);

        assert!(acct.remove_guardian(&test_key(10)));
        assert_eq!(acct.guardian_count(), 0);

        assert!(!acct.remove_guardian(&test_key(99)));
    }

    #[test]
    fn test_session_key_valid() {
        let mut acct = SmartAccount::new(test_key(1), 0);
        acct.add_session_key(make_session_key(20, 100, 500));

        assert!(!acct.is_session_key_valid(&test_key(20), 50));
        assert!(acct.is_session_key_valid(&test_key(20), 100));
        assert!(acct.is_session_key_valid(&test_key(20), 300));
        assert!(acct.is_session_key_valid(&test_key(20), 500));
        assert!(!acct.is_session_key_valid(&test_key(20), 501));
        assert!(!acct.is_session_key_valid(&test_key(99), 300));
    }

    #[test]
    fn test_session_key_revoked() {
        let mut acct = SmartAccount::new(test_key(1), 0);
        acct.add_session_key(make_session_key(20, 100, 500));

        assert!(acct.is_session_key_valid(&test_key(20), 300));

        assert!(acct.revoke_session_key(&test_key(20)));

        assert!(!acct.is_session_key_valid(&test_key(20), 300));

        assert!(!acct.revoke_session_key(&test_key(99)));
    }

    #[test]
    fn test_spending_limit_check() {
        let mut acct = SmartAccount::new(test_key(1), 0);
        let native_token = [0u8; 32];

        acct.spending_limits.push(SpendingLimit {
            token: native_token,
            daily_limit: 1_000_000,
            spent_today: 0,
            last_reset: 0,
        });

        assert!(acct.check_spending_limit(&native_token, 500_000));
        assert!(acct.check_spending_limit(&native_token, 1_000_000));
        assert!(!acct.check_spending_limit(&native_token, 1_000_001));

        assert!(acct.check_spending_limit(&test_key(42), 999_999_999));
    }

    #[test]
    fn test_spending_limit_daily_reset() {
        let mut acct = SmartAccount::new(test_key(1), 0);
        let native_token = [0u8; 32];

        acct.spending_limits.push(SpendingLimit {
            token: native_token,
            daily_limit: 1_000_000,
            spent_today: 0,
            last_reset: 0,
        });

        acct.record_spend(&native_token, 800_000, 100);
        assert!(!acct.check_spending_limit(&native_token, 300_000));

        acct.record_spend(&native_token, 200_000, DAILY_RESET_BLOCKS + 1);

        assert!(acct.check_spending_limit(&native_token, 800_000));
        assert!(!acct.check_spending_limit(&native_token, 800_001));
    }

    #[test]
    fn test_recovery_request() {
        let account = test_key(1);
        let new_owner = test_key(2);
        let mut req = RecoveryRequest::new(account, new_owner, 2, 1000, 100);

        assert_eq!(req.status, RecoveryStatus::Pending);
        assert!(!req.is_approved());
        assert_eq!(req.initiated_at, 100);
        assert_eq!(req.expires_at, 1100);

        let approval1 = RecoveryApproval {
            guardian: test_key(10),
            approved_at: 150,
            signature: vec![0xAA; 64],
        };
        assert!(req.add_approval(approval1));
        assert!(!req.is_approved());

        let dup = RecoveryApproval {
            guardian: test_key(10),
            approved_at: 160,
            signature: vec![0xBB; 64],
        };
        assert!(!req.add_approval(dup));

        let approval2 = RecoveryApproval {
            guardian: test_key(11),
            approved_at: 200,
            signature: vec![0xCC; 64],
        };
        assert!(req.add_approval(approval2));
        assert!(req.is_approved());
        assert_eq!(req.status, RecoveryStatus::Approved);
    }

    #[test]
    fn test_recovery_expiration() {
        let req = RecoveryRequest::new(test_key(1), test_key(2), 2, 1000, 100);

        assert!(!req.is_expired(100));
        assert!(!req.is_expired(1100));
        assert!(req.is_expired(1101));
    }

    #[test]
    fn test_user_operation_hash() {
        let op = UserOperation {
            sender: test_key(1),
            nonce: 42,
            call_data: vec![0x01, 0x02, 0x03],
            call_gas_limit: 100_000,
            verification_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            max_fee_per_gas: 1_000,
            max_priority_fee: 100,
            paymaster: None,
            paymaster_data: vec![],
            signature: vec![],
        };

        let h1 = op.hash();
        let h2 = op.hash();
        assert_eq!(h1, h2, "hash must be deterministic");
        assert_ne!(h1, [0u8; 32], "hash must not be zero");

        let mut op2 = op.clone();
        op2.nonce = 43;
        assert_ne!(
            op.hash(),
            op2.hash(),
            "different nonce must produce different hash"
        );
    }

    #[test]
    fn test_user_operation_sponsored() {
        let mut op = UserOperation {
            sender: test_key(1),
            nonce: 0,
            call_data: vec![],
            call_gas_limit: 100_000,
            verification_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            max_fee_per_gas: 1_000,
            max_priority_fee: 100,
            paymaster: None,
            paymaster_data: vec![],
            signature: vec![],
        };

        assert!(!op.is_sponsored());
        assert_eq!(op.total_gas(), 171_000);

        op.paymaster = Some(test_key(50));
        assert!(op.is_sponsored());
    }

    #[test]
    fn test_account_modules() {
        let mut acct = SmartAccount::new(test_key(1), 0);

        assert!(!acct.has_module("SocialRecovery"));
        assert!(!acct.has_module("MultiSig"));
        assert!(!acct.has_module("PaymasterSponsored"));
        assert!(!acct.has_module("AutomationHook"));
        assert!(!acct.has_module("DailyLimit"));

        acct.modules
            .push(AccountModule::SocialRecovery { threshold: 3 });
        acct.modules.push(AccountModule::PaymasterSponsored {
            paymaster: test_key(50),
        });

        assert!(acct.has_module("SocialRecovery"));
        assert!(!acct.has_module("MultiSig"));
        assert!(acct.has_module("PaymasterSponsored"));
        assert!(!acct.has_module("AutomationHook"));
        assert!(!acct.has_module("DailyLimit"));

        acct.modules.push(AccountModule::MultiSig {
            signers: vec![test_key(2), test_key(3)],
            threshold: 2,
        });
        assert!(acct.has_module("MultiSig"));
    }

    #[test]
    fn test_active_session_keys() {
        let mut acct = SmartAccount::new(test_key(1), 0);

        acct.add_session_key(make_session_key(10, 100, 500));
        acct.add_session_key(make_session_key(11, 200, 600));
        acct.add_session_key(make_session_key(12, 800, 1000));

        assert_eq!(acct.active_session_keys(50).len(), 0);

        let active = acct.active_session_keys(150);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key, test_key(10));

        assert_eq!(acct.active_session_keys(300).len(), 2);

        let active = acct.active_session_keys(550);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key, test_key(11));

        acct.revoke_session_key(&test_key(11));
        assert_eq!(acct.active_session_keys(550).len(), 0);

        let active = acct.active_session_keys(900);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key, test_key(12));
    }
}
