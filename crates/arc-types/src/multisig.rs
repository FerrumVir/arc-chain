use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MultiSigError {
    #[error("address is not an owner of this wallet")]
    NotOwner,
    #[error("signer has already approved this transaction")]
    AlreadyApproved,
    #[error("approval threshold has not been met")]
    ThresholdNotMet,
    #[error("transaction not found")]
    TxNotFound,
    #[error("transaction has expired")]
    TxExpired,
    #[error("invalid threshold: must be > 0 and <= total owner weight")]
    InvalidThreshold,
    #[error("at least one owner is required")]
    TooFewOwners,
    #[error("owner already exists in the wallet")]
    OwnerAlreadyExists,
    #[error("cannot remove the last owner")]
    CannotRemoveLastOwner,
    #[error("transaction has already been executed")]
    TxAlreadyExecuted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MultiSigTxStatus {
    Pending,
    Approved,
    Executed,
    Rejected,
    Expired,
    Cancelled,
}

impl fmt::Display for MultiSigTxStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "Pending"),
            Self::Approved => write!(f, "Approved"),
            Self::Executed => write!(f, "Executed"),
            Self::Rejected => write!(f, "Rejected"),
            Self::Expired => write!(f, "Expired"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub success: bool,
    pub return_data: Vec<u8>,
    pub gas_used: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub signer: [u8; 32],
    pub weight: u32,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    pub address: [u8; 32],
    pub weight: u32,
    pub added_at: u64,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingMultiSigTx {
    pub id: [u8; 32],
    pub to: [u8; 32],
    pub value: u64,
    pub data: Vec<u8>,
    pub proposer: [u8; 32],
    pub approvals: Vec<Approval>,
    pub rejections: Vec<[u8; 32]>,
    pub created_at: u64,
    pub expires_at: u64,
    pub status: MultiSigTxStatus,
    pub execution_result: Option<ExecutionResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiSigWallet {
    pub address: [u8; 32],
    pub owners: Vec<Owner>,
    pub threshold: u32,
    pub nonce: u64,
    pub pending_txs: Vec<PendingMultiSigTx>,
}

impl MultiSigWallet {
    fn total_weight(&self) -> u32 {
        self.owners.iter().map(|o| o.weight).sum()
    }

    fn is_owner(&self, addr: &[u8; 32]) -> bool {
        self.owners.iter().any(|o| o.address == *addr)
    }

    fn owner_weight(&self, addr: &[u8; 32]) -> u32 {
        self.owners
            .iter()
            .find(|o| o.address == *addr)
            .map(|o| o.weight)
            .unwrap_or(0)
    }

    fn find_tx_mut(&mut self, tx_id: &[u8; 32]) -> Result<&mut PendingMultiSigTx, MultiSigError> {
        self.pending_txs
            .iter_mut()
            .find(|tx| tx.id == *tx_id)
            .ok_or(MultiSigError::TxNotFound)
    }

    fn find_tx(&self, tx_id: &[u8; 32]) -> Result<&PendingMultiSigTx, MultiSigError> {
        self.pending_txs
            .iter()
            .find(|tx| tx.id == *tx_id)
            .ok_or(MultiSigError::TxNotFound)
    }

    fn approval_weight(tx: &PendingMultiSigTx) -> u32 {
        tx.approvals.iter().map(|a| a.weight).sum()
    }

    fn derive_tx_id(nonce: u64, to: &[u8; 32], value: u64) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&nonce.to_le_bytes());
        hasher.update(to);
        hasher.update(&value.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    fn derive_address(owners: &[Owner], threshold: u32) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"multisig-wallet-v1");
        hasher.update(&threshold.to_le_bytes());
        for owner in owners {
            hasher.update(&owner.address);
        }
        *hasher.finalize().as_bytes()
    }

    pub fn new(owners: Vec<Owner>, threshold: u32) -> Result<Self, MultiSigError> {
        if owners.is_empty() {
            return Err(MultiSigError::TooFewOwners);
        }
        let total: u32 = owners.iter().map(|o| o.weight).sum();
        if threshold == 0 || threshold > total {
            return Err(MultiSigError::InvalidThreshold);
        }
        let address = Self::derive_address(&owners, threshold);
        Ok(Self {
            address,
            owners,
            threshold,
            nonce: 0,
            pending_txs: Vec::new(),
        })
    }

    pub fn propose_tx(
        &mut self,
        proposer: [u8; 32],
        to: [u8; 32],
        value: u64,
        data: Vec<u8>,
        expires_at: u64,
    ) -> Result<[u8; 32], MultiSigError> {
        if !self.is_owner(&proposer) {
            return Err(MultiSigError::NotOwner);
        }
        let tx_id = Self::derive_tx_id(self.nonce, &to, value);
        self.nonce += 1;

        let tx = PendingMultiSigTx {
            id: tx_id,
            to,
            value,
            data,
            proposer,
            approvals: Vec::new(),
            rejections: Vec::new(),
            created_at: 0,
            expires_at,
            status: MultiSigTxStatus::Pending,
            execution_result: None,
        };
        self.pending_txs.push(tx);
        Ok(tx_id)
    }

    pub fn approve(&mut self, tx_id: [u8; 32], signer: [u8; 32]) -> Result<bool, MultiSigError> {
        if !self.is_owner(&signer) {
            return Err(MultiSigError::NotOwner);
        }
        let weight = self.owner_weight(&signer);
        let threshold = self.threshold;

        let tx = self.find_tx_mut(&tx_id)?;

        match tx.status {
            MultiSigTxStatus::Executed => return Err(MultiSigError::TxAlreadyExecuted),
            MultiSigTxStatus::Expired | MultiSigTxStatus::Cancelled => {
                return Err(MultiSigError::TxExpired);
            }
            _ => {}
        }

        if tx.approvals.iter().any(|a| a.signer == signer) {
            return Err(MultiSigError::AlreadyApproved);
        }

        tx.approvals.push(Approval {
            signer,
            weight,
            timestamp: 0,
        });

        let total_weight = Self::approval_weight(tx);
        if total_weight >= threshold {
            tx.status = MultiSigTxStatus::Approved;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn reject(&mut self, tx_id: [u8; 32], signer: [u8; 32]) -> Result<(), MultiSigError> {
        if !self.is_owner(&signer) {
            return Err(MultiSigError::NotOwner);
        }

        let total_weight = self.total_weight();
        let threshold = self.threshold;
        let owner_weights: Vec<([u8; 32], u32)> =
            self.owners.iter().map(|o| (o.address, o.weight)).collect();

        let tx = self.find_tx_mut(&tx_id)?;

        match tx.status {
            MultiSigTxStatus::Executed => return Err(MultiSigError::TxAlreadyExecuted),
            MultiSigTxStatus::Expired | MultiSigTxStatus::Cancelled => {
                return Err(MultiSigError::TxExpired);
            }
            _ => {}
        }

        if !tx.rejections.contains(&signer) {
            tx.rejections.push(signer);
        }

        let rejected_weight: u32 = tx
            .rejections
            .iter()
            .map(|r| {
                owner_weights
                    .iter()
                    .find(|(addr, _)| addr == r)
                    .map(|(_, w)| *w)
                    .unwrap_or(0)
            })
            .sum();
        if total_weight - rejected_weight < threshold {
            tx.status = MultiSigTxStatus::Rejected;
        }

        Ok(())
    }

    pub fn is_approved(&self, tx_id: [u8; 32]) -> bool {
        match self.find_tx(&tx_id) {
            Ok(tx) => Self::approval_weight(tx) >= self.threshold,
            Err(_) => false,
        }
    }

    pub fn execute(&mut self, tx_id: [u8; 32]) -> Result<ExecutionResult, MultiSigError> {
        let threshold = self.threshold;

        let tx = self.find_tx_mut(&tx_id)?;

        if tx.status == MultiSigTxStatus::Executed {
            return Err(MultiSigError::TxAlreadyExecuted);
        }
        if Self::approval_weight(tx) < threshold {
            return Err(MultiSigError::ThresholdNotMet);
        }

        let result = ExecutionResult {
            success: true,
            return_data: Vec::new(),
            gas_used: 21_000,
        };

        tx.status = MultiSigTxStatus::Executed;
        tx.execution_result = Some(result.clone());

        Ok(result)
    }

    pub fn add_owner(&mut self, new_owner: Owner) -> Result<(), MultiSigError> {
        if self.is_owner(&new_owner.address) {
            return Err(MultiSigError::OwnerAlreadyExists);
        }
        self.owners.push(new_owner);
        Ok(())
    }

    pub fn remove_owner(&mut self, address: [u8; 32]) -> Result<(), MultiSigError> {
        if self.owners.len() <= 1 {
            return Err(MultiSigError::CannotRemoveLastOwner);
        }
        let idx = self
            .owners
            .iter()
            .position(|o| o.address == address)
            .ok_or(MultiSigError::NotOwner)?;

        let remaining_weight: u32 = self
            .owners
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx)
            .map(|(_, o)| o.weight)
            .sum();
        if remaining_weight < self.threshold {
            return Err(MultiSigError::InvalidThreshold);
        }

        self.owners.remove(idx);
        Ok(())
    }

    pub fn change_threshold(&mut self, new_threshold: u32) -> Result<(), MultiSigError> {
        if new_threshold == 0 || new_threshold > self.total_weight() {
            return Err(MultiSigError::InvalidThreshold);
        }
        self.threshold = new_threshold;
        Ok(())
    }

    pub fn cancel_expired(&mut self, now: u64) -> usize {
        let mut count = 0;
        for tx in &mut self.pending_txs {
            if tx.expires_at <= now
                && tx.status != MultiSigTxStatus::Executed
                && tx.status != MultiSigTxStatus::Expired
                && tx.status != MultiSigTxStatus::Cancelled
            {
                tx.status = MultiSigTxStatus::Expired;
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(id: u8, weight: u32) -> Owner {
        let mut addr = [0u8; 32];
        addr[0] = id;
        Owner {
            address: addr,
            weight,
            added_at: 1000,
            label: format!("owner-{}", id),
        }
    }

    fn addr(id: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = id;
        a
    }

    fn default_wallet() -> MultiSigWallet {
        let owners = vec![owner(1, 1), owner(2, 1), owner(3, 1)];
        MultiSigWallet::new(owners, 2).unwrap()
    }

    #[test]
    fn test_create_wallet() {
        let wallet = default_wallet();
        assert_eq!(wallet.owners.len(), 3);
        assert_eq!(wallet.threshold, 2);
        assert_eq!(wallet.nonce, 0);
    }

    #[test]
    fn test_create_wallet_no_owners() {
        let result = MultiSigWallet::new(vec![], 1);
        assert_eq!(result.unwrap_err(), MultiSigError::TooFewOwners);
    }

    #[test]
    fn test_create_wallet_invalid_threshold_zero() {
        let owners = vec![owner(1, 1)];
        let result = MultiSigWallet::new(owners, 0);
        assert_eq!(result.unwrap_err(), MultiSigError::InvalidThreshold);
    }

    #[test]
    fn test_create_wallet_threshold_exceeds_weight() {
        let owners = vec![owner(1, 1), owner(2, 1)];
        let result = MultiSigWallet::new(owners, 5);
        assert_eq!(result.unwrap_err(), MultiSigError::InvalidThreshold);
    }

    #[test]
    fn test_propose_tx() {
        let mut wallet = default_wallet();
        let tx_id = wallet
            .propose_tx(addr(1), addr(99), 1000, vec![], 9999)
            .unwrap();
        assert_eq!(wallet.pending_txs.len(), 1);
        assert_eq!(wallet.pending_txs[0].id, tx_id);
        assert_eq!(wallet.nonce, 1);
    }

    #[test]
    fn test_propose_tx_not_owner() {
        let mut wallet = default_wallet();
        let result = wallet.propose_tx(addr(99), addr(50), 100, vec![], 9999);
        assert_eq!(result.unwrap_err(), MultiSigError::NotOwner);
    }

    #[test]
    fn test_approve_and_threshold() {
        let mut wallet = default_wallet();
        let tx_id = wallet
            .propose_tx(addr(1), addr(99), 500, vec![], 9999)
            .unwrap();

        let met = wallet.approve(tx_id, addr(1)).unwrap();
        assert!(!met);

        let met = wallet.approve(tx_id, addr(2)).unwrap();
        assert!(met);
        assert!(wallet.is_approved(tx_id));
    }

    #[test]
    fn test_approve_already_approved() {
        let mut wallet = default_wallet();
        let tx_id = wallet
            .propose_tx(addr(1), addr(99), 500, vec![], 9999)
            .unwrap();
        wallet.approve(tx_id, addr(1)).unwrap();
        let result = wallet.approve(tx_id, addr(1));
        assert_eq!(result.unwrap_err(), MultiSigError::AlreadyApproved);
    }

    #[test]
    fn test_reject_tx() {
        let mut wallet = default_wallet();
        let tx_id = wallet
            .propose_tx(addr(1), addr(99), 500, vec![], 9999)
            .unwrap();
        wallet.reject(tx_id, addr(1)).unwrap();
        wallet.reject(tx_id, addr(2)).unwrap();
        let tx = wallet.find_tx(&tx_id).unwrap();
        assert_eq!(tx.status, MultiSigTxStatus::Rejected);
    }

    #[test]
    fn test_execute_approved_tx() {
        let mut wallet = default_wallet();
        let tx_id = wallet
            .propose_tx(addr(1), addr(99), 500, vec![], 9999)
            .unwrap();
        wallet.approve(tx_id, addr(1)).unwrap();
        wallet.approve(tx_id, addr(2)).unwrap();

        let result = wallet.execute(tx_id).unwrap();
        assert!(result.success);
        assert_eq!(result.gas_used, 21_000);

        let tx = wallet.find_tx(&tx_id).unwrap();
        assert_eq!(tx.status, MultiSigTxStatus::Executed);
    }

    #[test]
    fn test_execute_without_threshold() {
        let mut wallet = default_wallet();
        let tx_id = wallet
            .propose_tx(addr(1), addr(99), 500, vec![], 9999)
            .unwrap();
        wallet.approve(tx_id, addr(1)).unwrap();
        let result = wallet.execute(tx_id);
        assert_eq!(result.unwrap_err(), MultiSigError::ThresholdNotMet);
    }

    #[test]
    fn test_execute_already_executed() {
        let mut wallet = default_wallet();
        let tx_id = wallet
            .propose_tx(addr(1), addr(99), 500, vec![], 9999)
            .unwrap();
        wallet.approve(tx_id, addr(1)).unwrap();
        wallet.approve(tx_id, addr(2)).unwrap();
        wallet.execute(tx_id).unwrap();
        let result = wallet.execute(tx_id);
        assert_eq!(result.unwrap_err(), MultiSigError::TxAlreadyExecuted);
    }

    #[test]
    fn test_add_owner() {
        let mut wallet = default_wallet();
        wallet.add_owner(owner(4, 2)).unwrap();
        assert_eq!(wallet.owners.len(), 4);
    }

    #[test]
    fn test_add_owner_duplicate() {
        let mut wallet = default_wallet();
        let result = wallet.add_owner(owner(1, 1));
        assert_eq!(result.unwrap_err(), MultiSigError::OwnerAlreadyExists);
    }

    #[test]
    fn test_remove_owner() {
        let mut wallet = default_wallet();
        wallet.remove_owner(addr(3)).unwrap();
        assert_eq!(wallet.owners.len(), 2);
    }

    #[test]
    fn test_remove_last_owner() {
        let owners = vec![owner(1, 1)];
        let mut wallet = MultiSigWallet::new(owners, 1).unwrap();
        let result = wallet.remove_owner(addr(1));
        assert_eq!(result.unwrap_err(), MultiSigError::CannotRemoveLastOwner);
    }

    #[test]
    fn test_change_threshold() {
        let mut wallet = default_wallet();
        wallet.change_threshold(3).unwrap();
        assert_eq!(wallet.threshold, 3);
    }

    #[test]
    fn test_change_threshold_invalid() {
        let mut wallet = default_wallet();
        let result = wallet.change_threshold(0);
        assert_eq!(result.unwrap_err(), MultiSigError::InvalidThreshold);
        let result = wallet.change_threshold(100);
        assert_eq!(result.unwrap_err(), MultiSigError::InvalidThreshold);
    }

    #[test]
    fn test_cancel_expired() {
        let mut wallet = default_wallet();
        let tx1 = wallet
            .propose_tx(addr(1), addr(99), 100, vec![], 500)
            .unwrap();
        let _tx2 = wallet
            .propose_tx(addr(1), addr(99), 200, vec![], 1500)
            .unwrap();
        let tx3 = wallet
            .propose_tx(addr(1), addr(99), 300, vec![], 700)
            .unwrap();

        let cancelled = wallet.cancel_expired(1000);
        assert_eq!(cancelled, 2);

        let t1 = wallet.find_tx(&tx1).unwrap();
        assert_eq!(t1.status, MultiSigTxStatus::Expired);
        let t3 = wallet.find_tx(&tx3).unwrap();
        assert_eq!(t3.status, MultiSigTxStatus::Expired);
    }

    #[test]
    fn test_weighted_threshold() {
        let owners = vec![owner(1, 3), owner(2, 1)];
        let mut wallet = MultiSigWallet::new(owners, 3).unwrap();
        let tx_id = wallet
            .propose_tx(addr(1), addr(99), 100, vec![], 9999)
            .unwrap();

        let met = wallet.approve(tx_id, addr(1)).unwrap();
        assert!(met);
        assert!(wallet.is_approved(tx_id));
    }

    #[test]
    fn test_approve_expired_tx() {
        let mut wallet = default_wallet();
        let tx_id = wallet
            .propose_tx(addr(1), addr(99), 100, vec![], 500)
            .unwrap();
        wallet.cancel_expired(1000);
        let result = wallet.approve(tx_id, addr(1));
        assert_eq!(result.unwrap_err(), MultiSigError::TxExpired);
    }

    #[test]
    fn test_remove_owner_would_break_threshold() {
        let owners = vec![owner(1, 1), owner(2, 1), owner(3, 1)];
        let mut wallet = MultiSigWallet::new(owners, 3).unwrap();
        let result = wallet.remove_owner(addr(1));
        assert_eq!(result.unwrap_err(), MultiSigError::InvalidThreshold);
    }
}
