// Add to lib.rs: pub mod account_abstraction;

use serde::{Deserialize, Serialize};

// ─── Smart Account ──────────────────────────────────────────────────────────

/// Smart contract wallet account (ERC-4337 style).
///
/// Supports social recovery, session keys, spending limits, and
/// pluggable modules for extensible wallet behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartAccount {
    /// Account address (derived from owner + salt).
    pub address: [u8; 32],
    /// Primary signer (owner public key hash).
    pub owner: [u8; 32],
    /// Guardians for social recovery.
    pub guardians: Vec<Guardian>,
    /// Temporary signing keys (for dApps, agents).
    pub session_keys: Vec<SessionKey>,
    /// Per-token spending limits.
    pub spending_limits: Vec<SpendingLimit>,
    /// Pluggable account modules.
    pub modules: Vec<AccountModule>,
    /// Transaction nonce (replay protection).
    pub nonce: u64,
    /// Block height when the account was created.
    pub created_at: u64,
}

/// Social recovery guardian.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guardian {
    /// Guardian address.
    pub address: [u8; 32],
    /// Human-readable guardian name.
    pub name: String,
    /// Voting weight for recovery threshold.
    pub weight: u32,
    /// Block height when the guardian was added.
    pub added_at: u64,
}

/// Time-limited signing key (for dApps, agents).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionKey {
    /// Public key of the session signer.
    pub key: [u8; 32],
    /// What this session key is allowed to do.
    pub permissions: SessionPermissions,
    /// Block height from which this key is valid.
    pub valid_from: u64,
    /// Block height after which this key expires.
    pub valid_until: u64,
    /// Maximum gas this key can spend per transaction.
    pub max_gas_per_tx: u64,
    /// Whether this key has been explicitly revoked.
    pub is_revoked: bool,
}

/// Permission set for a session key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPermissions {
    /// Whether the session key can initiate transfers.
    pub can_transfer: bool,
    /// Whether the session key can call smart contracts.
    pub can_call_contracts: bool,
    /// Whitelist of contract addresses this key can call (empty = all).
    pub allowed_contracts: Vec<[u8; 32]>,
    /// Maximum value (in smallest unit) per transaction.
    pub max_value_per_tx: u64,
    /// Maximum number of transactions per day.
    pub max_txs_per_day: u32,
}

/// Per-token spending limit with daily reset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendingLimit {
    /// Token contract address (zero = native ARC token).
    pub token: [u8; 32],
    /// Maximum spend allowed per daily period.
    pub daily_limit: u64,
    /// Amount spent in the current period.
    pub spent_today: u64,
    /// Block height of the last daily reset.
    pub last_reset: u64,
}

/// Pluggable account module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AccountModule {
    /// N-of-M guardian social recovery.
    SocialRecovery { threshold: u32 },
    /// Multi-signature authorization.
    MultiSig {
        signers: Vec<[u8; 32]>,
        threshold: u32,
    },
    /// Gasless transactions via a paymaster.
    PaymasterSponsored { paymaster: [u8; 32] },
    /// Automation hook (auto-execute on conditions).
    AutomationHook { contract: [u8; 32] },
    /// Daily spending cap (enforced at module level).
    DailyLimit { amount: u64 },
}

// ─── User Operation (ERC-4337 style) ────────────────────────────────────────

/// A user operation submitted to a bundler (ERC-4337 style).
///
/// Encapsulates the intent of a smart account action including
/// gas parameters and optional paymaster sponsorship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserOperation {
    /// Smart account address sending this operation.
    pub sender: [u8; 32],
    /// Account nonce (replay protection).
    pub nonce: u64,
    /// Encoded call data (function selector + arguments).
    pub call_data: Vec<u8>,
    /// Gas limit for the main execution call.
    pub call_gas_limit: u64,
    /// Gas limit for the verification step.
    pub verification_gas_limit: u64,
    /// Gas overhead for pre-verification (bundler compensation).
    pub pre_verification_gas: u64,
    /// Maximum fee per gas unit the sender will pay.
    pub max_fee_per_gas: u64,
    /// Maximum priority fee (tip) per gas unit.
    pub max_priority_fee: u64,
    /// Optional paymaster address (who pays gas on behalf of sender).
    pub paymaster: Option<[u8; 32]>,
    /// Paymaster-specific data (e.g. sponsorship proof).
    pub paymaster_data: Vec<u8>,
    /// Signature over the user operation hash.
    pub signature: Vec<u8>,
}

// ─── Social Recovery ────────────────────────────────────────────────────────

/// A pending social recovery request.
///
/// Guardians approve the request until the threshold is met,
/// at which point the account owner can be replaced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRequest {
    /// Smart account being recovered.
    pub account: [u8; 32],
    /// Proposed new owner address.
    pub new_owner: [u8; 32],
    /// Guardian approvals collected so far.
    pub approvals: Vec<RecoveryApproval>,
    /// Number of approval weight required.
    pub threshold: u32,
    /// Block height when the recovery was initiated.
    pub initiated_at: u64,
    /// Block height after which the recovery expires.
    pub expires_at: u64,
    /// Current status of the recovery request.
    pub status: RecoveryStatus,
}

/// A single guardian approval for a recovery request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryApproval {
    /// Guardian address that approved.
    pub guardian: [u8; 32],
    /// Block height when the approval was given.
    pub approved_at: u64,
    /// Signature proving the guardian authorized this approval.
    pub signature: Vec<u8>,
}

/// Status of a social recovery request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryStatus {
    /// Awaiting guardian approvals.
    Pending,
    /// Threshold met, ready for execution.
    Approved,
    /// Owner has been replaced.
    Executed,
    /// TTL elapsed without sufficient approvals.
    Expired,
    /// Cancelled by the current owner.
    Cancelled,
}

// ─── Constants ──────────────────────────────────────────────────────────────

/// Default daily reset period in blocks (~1 day at 400ms/block).
pub const DAILY_RESET_BLOCKS: u64 = 216_000;

// ─── SmartAccount impl ─────────────────────────────────────────────────────

impl SmartAccount {
    /// Create a new smart account with the given owner.
    ///
    /// The address is derived from a BLAKE3 hash of the owner key
    /// and the nonce (used as a salt for counterfactual deployment).
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

    /// Add a guardian for social recovery.
    pub fn add_guardian(&mut self, guardian: Guardian) {
        self.guardians.push(guardian);
    }

    /// Remove a guardian by address. Returns true if removed.
    pub fn remove_guardian(&mut self, address: &[u8; 32]) -> bool {
        let before = self.guardians.len();
        self.guardians.retain(|g| &g.address != address);
        self.guardians.len() < before
    }

    /// Add a session key for delegated signing.
    pub fn add_session_key(&mut self, key: SessionKey) {
        self.session_keys.push(key);
    }

    /// Revoke a session key by its public key. Returns true if found and revoked.
    pub fn revoke_session_key(&mut self, key: &[u8; 32]) -> bool {
        for sk in &mut self.session_keys {
            if &sk.key == key {
                sk.is_revoked = true;
                return true;
            }
        }
        false
    }

    /// Check whether a session key is valid at the given block height.
    ///
    /// A key is valid if it exists, is not revoked, and the current
    /// height falls within its `[valid_from, valid_until]` range.
    pub fn is_session_key_valid(&self, key: &[u8; 32], current_height: u64) -> bool {
        self.session_keys.iter().any(|sk| {
            &sk.key == key
                && !sk.is_revoked
                && current_height >= sk.valid_from
                && current_height <= sk.valid_until
        })
    }

    /// Check whether a spend of `amount` for the given token is within limits.
    ///
    /// If no spending limit is configured for the token, the spend is allowed.
    pub fn check_spending_limit(&self, token: &[u8; 32], amount: u64) -> bool {
        for limit in &self.spending_limits {
            if &limit.token == token {
                return limit.spent_today.saturating_add(amount) <= limit.daily_limit;
            }
        }
        // No limit configured for this token — allow.
        true
    }

    /// Record a spend against the spending limit for a token.
    ///
    /// If `current_height` is beyond the reset period since `last_reset`,
    /// the daily counter is reset before recording.
    pub fn record_spend(&mut self, token: &[u8; 32], amount: u64, current_height: u64) {
        for limit in &mut self.spending_limits {
            if &limit.token == token {
                // Reset daily counter if a full period has elapsed.
                if current_height >= limit.last_reset + DAILY_RESET_BLOCKS {
                    limit.spent_today = 0;
                    limit.last_reset = current_height;
                }
                limit.spent_today = limit.spent_today.saturating_add(amount);
                return;
            }
        }
    }

    /// Check whether the account has a module of the given type.
    ///
    /// Module types: "SocialRecovery", "MultiSig", "PaymasterSponsored",
    /// "AutomationHook", "DailyLimit".
    pub fn has_module(&self, module_type: &str) -> bool {
        self.modules.iter().any(|m| match m {
            AccountModule::SocialRecovery { .. } => module_type == "SocialRecovery",
            AccountModule::MultiSig { .. } => module_type == "MultiSig",
            AccountModule::PaymasterSponsored { .. } => module_type == "PaymasterSponsored",
            AccountModule::AutomationHook { .. } => module_type == "AutomationHook",
            AccountModule::DailyLimit { .. } => module_type == "DailyLimit",
        })
    }

    /// Number of guardians currently registered.
    pub fn guardian_count(&self) -> usize {
        self.guardians.len()
    }

    /// Return all session keys that are active at the given block height.
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

// ─── RecoveryRequest impl ───────────────────────────────────────────────────

impl RecoveryRequest {
    /// Create a new recovery request.
    ///
    /// - `threshold`: total approval weight required.
    /// - `ttl_blocks`: number of blocks before the request expires.
    /// - `current_height`: block height at initiation.
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

    /// Add a guardian approval. Returns true if the approval was new
    /// (i.e. the guardian had not already approved).
    ///
    /// Automatically transitions status to `Approved` when the
    /// cumulative approval weight meets the threshold.
    pub fn add_approval(&mut self, approval: RecoveryApproval) -> bool {
        // Reject duplicate approvals from the same guardian.
        if self
            .approvals
            .iter()
            .any(|a| a.guardian == approval.guardian)
        {
            return false;
        }
        self.approvals.push(approval);

        // Check if threshold is now met (each approval counts as weight 1
        // in the simple case; for weighted recovery, the caller should
        // verify against Guardian weights externally).
        if self.approvals.len() as u32 >= self.threshold {
            self.status = RecoveryStatus::Approved;
        }
        true
    }

    /// Whether the recovery has been approved (threshold met).
    pub fn is_approved(&self) -> bool {
        self.status == RecoveryStatus::Approved
    }

    /// Whether the recovery has expired at the given block height.
    pub fn is_expired(&self, current_height: u64) -> bool {
        current_height > self.expires_at
    }
}

// ─── UserOperation impl ────────────────────────────────────────────────────

impl UserOperation {
    /// Total gas budget for this user operation.
    pub fn total_gas(&self) -> u64 {
        self.call_gas_limit
            .saturating_add(self.verification_gas_limit)
            .saturating_add(self.pre_verification_gas)
    }

    /// Whether this operation is sponsored by a paymaster (gasless for user).
    pub fn is_sponsored(&self) -> bool {
        self.paymaster.is_some()
    }

    /// Compute the BLAKE3 hash of this user operation.
    ///
    /// Covers all fields except `signature` (which signs over this hash).
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

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a deterministic 32-byte "address" from a single byte.
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

    // 1. Smart account creation
    #[test]
    fn test_smart_account_creation() {
        let owner = test_key(1);
        let acct = SmartAccount::new(owner, 0);

        assert_eq!(acct.owner, owner);
        assert_ne!(acct.address, [0u8; 32], "address should be derived, not zero");
        assert_eq!(acct.nonce, 0);
        assert!(acct.guardians.is_empty());
        assert!(acct.session_keys.is_empty());
        assert!(acct.spending_limits.is_empty());
        assert!(acct.modules.is_empty());
    }

    // 2. Add / remove guardian
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

        // Remove by address
        assert!(acct.remove_guardian(&test_key(10)));
        assert_eq!(acct.guardian_count(), 0);

        // Removing a non-existent guardian returns false
        assert!(!acct.remove_guardian(&test_key(99)));
    }

    // 3. Session key valid within range, invalid outside
    #[test]
    fn test_session_key_valid() {
        let mut acct = SmartAccount::new(test_key(1), 0);
        acct.add_session_key(make_session_key(20, 100, 500));

        // Before valid_from
        assert!(!acct.is_session_key_valid(&test_key(20), 50));
        // At valid_from
        assert!(acct.is_session_key_valid(&test_key(20), 100));
        // In the middle
        assert!(acct.is_session_key_valid(&test_key(20), 300));
        // At valid_until
        assert!(acct.is_session_key_valid(&test_key(20), 500));
        // After valid_until
        assert!(!acct.is_session_key_valid(&test_key(20), 501));
        // Non-existent key
        assert!(!acct.is_session_key_valid(&test_key(99), 300));
    }

    // 4. Revoked session key rejected
    #[test]
    fn test_session_key_revoked() {
        let mut acct = SmartAccount::new(test_key(1), 0);
        acct.add_session_key(make_session_key(20, 100, 500));

        // Valid before revocation
        assert!(acct.is_session_key_valid(&test_key(20), 300));

        // Revoke
        assert!(acct.revoke_session_key(&test_key(20)));

        // No longer valid
        assert!(!acct.is_session_key_valid(&test_key(20), 300));

        // Revoking a non-existent key returns false
        assert!(!acct.revoke_session_key(&test_key(99)));
    }

    // 5. Spending limit check: within limit OK, over limit rejected
    #[test]
    fn test_spending_limit_check() {
        let mut acct = SmartAccount::new(test_key(1), 0);
        let native_token = [0u8; 32]; // zero = native ARC

        acct.spending_limits.push(SpendingLimit {
            token: native_token,
            daily_limit: 1_000_000,
            spent_today: 0,
            last_reset: 0,
        });

        // Within limit
        assert!(acct.check_spending_limit(&native_token, 500_000));
        // Exactly at limit
        assert!(acct.check_spending_limit(&native_token, 1_000_000));
        // Over limit
        assert!(!acct.check_spending_limit(&native_token, 1_000_001));

        // Unknown token (no limit configured) — should be allowed
        assert!(acct.check_spending_limit(&test_key(42), 999_999_999));
    }

    // 6. Spending limit daily reset
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

        // Spend 800K at block 100
        acct.record_spend(&native_token, 800_000, 100);
        assert!(!acct.check_spending_limit(&native_token, 300_000)); // 800K + 300K > 1M

        // Advance past daily reset period (216_000 blocks)
        acct.record_spend(&native_token, 200_000, DAILY_RESET_BLOCKS + 1);

        // After reset, spent_today was zeroed then 200K added
        assert!(acct.check_spending_limit(&native_token, 800_000)); // 200K + 800K = 1M, OK
        assert!(!acct.check_spending_limit(&native_token, 800_001)); // 200K + 800_001 > 1M
    }

    // 7. Recovery request: create, add approvals, check approval status
    #[test]
    fn test_recovery_request() {
        let account = test_key(1);
        let new_owner = test_key(2);
        let mut req = RecoveryRequest::new(account, new_owner, 2, 1000, 100);

        assert_eq!(req.status, RecoveryStatus::Pending);
        assert!(!req.is_approved());
        assert_eq!(req.initiated_at, 100);
        assert_eq!(req.expires_at, 1100);

        // First approval
        let approval1 = RecoveryApproval {
            guardian: test_key(10),
            approved_at: 150,
            signature: vec![0xAA; 64],
        };
        assert!(req.add_approval(approval1));
        assert!(!req.is_approved()); // Still pending (need 2)

        // Duplicate approval from same guardian — rejected
        let dup = RecoveryApproval {
            guardian: test_key(10),
            approved_at: 160,
            signature: vec![0xBB; 64],
        };
        assert!(!req.add_approval(dup));

        // Second (different) guardian — threshold met
        let approval2 = RecoveryApproval {
            guardian: test_key(11),
            approved_at: 200,
            signature: vec![0xCC; 64],
        };
        assert!(req.add_approval(approval2));
        assert!(req.is_approved());
        assert_eq!(req.status, RecoveryStatus::Approved);
    }

    // 8. Recovery expiration
    #[test]
    fn test_recovery_expiration() {
        let req = RecoveryRequest::new(test_key(1), test_key(2), 2, 1000, 100);

        // Not expired at initiation
        assert!(!req.is_expired(100));
        // Not expired just before expiry
        assert!(!req.is_expired(1100));
        // Expired one block after
        assert!(req.is_expired(1101));
    }

    // 9. User operation hash is deterministic
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

        // Changing any field should change the hash
        let mut op2 = op.clone();
        op2.nonce = 43;
        assert_ne!(op.hash(), op2.hash(), "different nonce must produce different hash");
    }

    // 10. User operation paymaster detection
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
        assert_eq!(op.total_gas(), 171_000); // 100K + 50K + 21K

        op.paymaster = Some(test_key(50));
        assert!(op.is_sponsored());
    }

    // 11. Account modules: has_module checks
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
        acct.modules
            .push(AccountModule::PaymasterSponsored { paymaster: test_key(50) });

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

    // 12. Active session keys filtered by current height
    #[test]
    fn test_active_session_keys() {
        let mut acct = SmartAccount::new(test_key(1), 0);

        acct.add_session_key(make_session_key(10, 100, 500));  // active 100..500
        acct.add_session_key(make_session_key(11, 200, 600));  // active 200..600
        acct.add_session_key(make_session_key(12, 800, 1000)); // active 800..1000

        // At block 50: none active
        assert_eq!(acct.active_session_keys(50).len(), 0);

        // At block 150: only key 10
        let active = acct.active_session_keys(150);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key, test_key(10));

        // At block 300: keys 10 and 11
        assert_eq!(acct.active_session_keys(300).len(), 2);

        // At block 550: only key 11 (key 10 expired at 500)
        let active = acct.active_session_keys(550);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key, test_key(11));

        // Revoke key 11 — now none active at 550
        acct.revoke_session_key(&test_key(11));
        assert_eq!(acct.active_session_keys(550).len(), 0);

        // At block 900: only key 12
        let active = acct.active_session_keys(900);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key, test_key(12));
    }
}
