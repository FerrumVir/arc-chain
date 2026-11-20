use arc_crypto::Hash256;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ChannelError {
    #[error("channel not in expected state: expected {expected}, got {actual}")]
    WrongState { expected: String, actual: String },

    #[error("conservation violated: {opener_bal} + {counterparty_bal} != {total}")]
    ConservationViolation {
        opener_bal: u64,
        counterparty_bal: u64,
        total: u64,
    },

    #[error("nonce must increase: got {got}, expected > {current}")]
    NonceNotIncreasing { current: u64, got: u64 },

    #[error("insufficient balance: have {have}, need {need}")]
    InsufficientBalance { have: u64, need: u64 },

    #[error("invalid signature from counterparty")]
    InvalidSignature,

    #[error("not authorized: {0}")]
    NotAuthorized(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelState {
    Opening,
    Open,
    Closing,
    Disputed,
    Closed,
}

impl std::fmt::Display for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Opening => write!(f, "Opening"),
            Self::Open => write!(f, "Open"),
            Self::Closing => write!(f, "Closing"),
            Self::Disputed => write!(f, "Disputed"),
            Self::Closed => write!(f, "Closed"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateCommitment {
    pub channel_id: Hash256,
    pub nonce: u64,
    pub opener_balance: u64,
    pub counterparty_balance: u64,
    pub proposer_sig: Vec<u8>,
    pub acceptor_sig: Option<Vec<u8>>,
}

impl StateCommitment {
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(32 + 8 + 8 + 8 + 22);
        payload.extend_from_slice(b"arc-channel-state-v1");
        payload.extend_from_slice(&self.channel_id.0);
        payload.extend_from_slice(&self.nonce.to_le_bytes());
        payload.extend_from_slice(&self.opener_balance.to_le_bytes());
        payload.extend_from_slice(&self.counterparty_balance.to_le_bytes());
        let hash = blake3::hash(&payload);
        hash.as_bytes().to_vec()
    }

    pub fn is_fully_signed(&self) -> bool {
        self.acceptor_sig.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Opener,
    Counterparty,
}

pub struct ChannelStateMachine {
    pub channel_id: Hash256,
    pub role: Role,
    pub state: ChannelState,
    pub total_deposit: u64,
    pub opener_balance: u64,
    pub counterparty_balance: u64,
    pub nonce: u64,
    signing_key: SigningKey,
    counterparty_vk: VerifyingKey,
    pub history: Vec<StateCommitment>,
}

impl ChannelStateMachine {
    pub fn new(
        channel_id: Hash256,
        role: Role,
        total_deposit: u64,
        signing_key: SigningKey,
        counterparty_vk: VerifyingKey,
    ) -> Self {
        let (opener_balance, counterparty_balance) = match role {
            Role::Opener => (total_deposit, 0),
            Role::Counterparty => (0, total_deposit),
        };

        Self {
            channel_id,
            role,
            state: ChannelState::Opening,
            total_deposit,
            opener_balance,
            counterparty_balance,
            nonce: 0,
            signing_key,
            counterparty_vk,
            history: Vec::new(),
        }
    }

    pub fn confirm_open(&mut self) -> Result<(), ChannelError> {
        if self.state != ChannelState::Opening {
            return Err(ChannelError::WrongState {
                expected: "Opening".into(),
                actual: self.state.to_string(),
            });
        }
        self.state = ChannelState::Open;
        Ok(())
    }

    pub fn pay(&mut self, amount: u64) -> Result<StateCommitment, ChannelError> {
        if self.state != ChannelState::Open {
            return Err(ChannelError::WrongState {
                expected: "Open".into(),
                actual: self.state.to_string(),
            });
        }

        let (new_opener, new_counter) = match self.role {
            Role::Opener => {
                if self.opener_balance < amount {
                    return Err(ChannelError::InsufficientBalance {
                        have: self.opener_balance,
                        need: amount,
                    });
                }
                (
                    self.opener_balance - amount,
                    self.counterparty_balance + amount,
                )
            }
            Role::Counterparty => {
                if self.counterparty_balance < amount {
                    return Err(ChannelError::InsufficientBalance {
                        have: self.counterparty_balance,
                        need: amount,
                    });
                }
                (
                    self.opener_balance + amount,
                    self.counterparty_balance - amount,
                )
            }
        };

        self.propose_state(new_opener, new_counter)
    }

    pub fn propose_state(
        &mut self,
        opener_balance: u64,
        counterparty_balance: u64,
    ) -> Result<StateCommitment, ChannelError> {
        if self.state != ChannelState::Open {
            return Err(ChannelError::WrongState {
                expected: "Open".into(),
                actual: self.state.to_string(),
            });
        }

        if opener_balance + counterparty_balance != self.total_deposit {
            return Err(ChannelError::ConservationViolation {
                opener_bal: opener_balance,
                counterparty_bal: counterparty_balance,
                total: self.total_deposit,
            });
        }

        let new_nonce = self.nonce + 1;

        let mut commitment = StateCommitment {
            channel_id: self.channel_id,
            nonce: new_nonce,
            opener_balance,
            counterparty_balance,
            proposer_sig: Vec::new(),
            acceptor_sig: None,
        };

        let payload = commitment.signing_payload();
        let sig = self.signing_key.sign(&payload);
        commitment.proposer_sig = sig.to_bytes().to_vec();

        Ok(commitment)
    }

    pub fn receive_state(
        &mut self,
        mut commitment: StateCommitment,
    ) -> Result<StateCommitment, ChannelError> {
        if self.state != ChannelState::Open {
            return Err(ChannelError::WrongState {
                expected: "Open".into(),
                actual: self.state.to_string(),
            });
        }

        if commitment.channel_id != self.channel_id {
            return Err(ChannelError::NotAuthorized("channel_id mismatch".into()));
        }

        if commitment.nonce <= self.nonce {
            return Err(ChannelError::NonceNotIncreasing {
                current: self.nonce,
                got: commitment.nonce,
            });
        }

        if commitment.opener_balance + commitment.counterparty_balance != self.total_deposit {
            return Err(ChannelError::ConservationViolation {
                opener_bal: commitment.opener_balance,
                counterparty_bal: commitment.counterparty_balance,
                total: self.total_deposit,
            });
        }

        let payload = commitment.signing_payload();
        if commitment.proposer_sig.len() != 64 {
            return Err(ChannelError::InvalidSignature);
        }
        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(&commitment.proposer_sig);
        let sig = Signature::from_bytes(&sig_bytes);

        self.counterparty_vk
            .verify_strict(&payload, &sig)
            .map_err(|_| ChannelError::InvalidSignature)?;

        let my_sig = self.signing_key.sign(&payload);
        commitment.acceptor_sig = Some(my_sig.to_bytes().to_vec());

        self.nonce = commitment.nonce;
        self.opener_balance = commitment.opener_balance;
        self.counterparty_balance = commitment.counterparty_balance;
        self.history.push(commitment.clone());

        Ok(commitment)
    }

    pub fn finalize_state(&mut self, commitment: &StateCommitment) -> Result<(), ChannelError> {
        if !commitment.is_fully_signed() {
            return Err(ChannelError::InvalidSignature);
        }

        let payload = commitment.signing_payload();
        let acceptor_sig_bytes = commitment.acceptor_sig.as_ref().unwrap();
        if acceptor_sig_bytes.len() != 64 {
            return Err(ChannelError::InvalidSignature);
        }
        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(acceptor_sig_bytes);
        let sig = Signature::from_bytes(&sig_bytes);

        self.counterparty_vk
            .verify_strict(&payload, &sig)
            .map_err(|_| ChannelError::InvalidSignature)?;

        self.nonce = commitment.nonce;
        self.opener_balance = commitment.opener_balance;
        self.counterparty_balance = commitment.counterparty_balance;
        self.history.push(commitment.clone());

        Ok(())
    }

    pub fn close(&mut self) -> Result<StateCommitment, ChannelError> {
        if self.state != ChannelState::Open {
            return Err(ChannelError::WrongState {
                expected: "Open".into(),
                actual: self.state.to_string(),
            });
        }

        self.state = ChannelState::Closing;

        let mut commitment = StateCommitment {
            channel_id: self.channel_id,
            nonce: self.nonce,
            opener_balance: self.opener_balance,
            counterparty_balance: self.counterparty_balance,
            proposer_sig: Vec::new(),
            acceptor_sig: None,
        };

        let payload = commitment.signing_payload();
        let sig = self.signing_key.sign(&payload);
        commitment.proposer_sig = sig.to_bytes().to_vec();

        Ok(commitment)
    }

    pub fn dispute(&self) -> Result<StateCommitment, ChannelError> {
        let latest = self
            .history
            .iter()
            .rev()
            .find(|c| c.is_fully_signed())
            .ok_or_else(|| ChannelError::WrongState {
                expected: "state with both signatures".into(),
                actual: "no fully-signed states".into(),
            })?;

        Ok(latest.clone())
    }

    pub fn confirm_closed(&mut self) {
        self.state = ChannelState::Closed;
    }

    pub fn my_balance(&self) -> u64 {
        match self.role {
            Role::Opener => self.opener_balance,
            Role::Counterparty => self.counterparty_balance,
        }
    }

    pub fn their_balance(&self) -> u64 {
        match self.role {
            Role::Opener => self.counterparty_balance,
            Role::Counterparty => self.opener_balance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn setup_channel() -> (ChannelStateMachine, ChannelStateMachine) {
        let opener_sk = SigningKey::generate(&mut rand::thread_rng());
        let counter_sk = SigningKey::generate(&mut rand::thread_rng());
        let opener_vk = opener_sk.verifying_key();
        let counter_vk = counter_sk.verifying_key();
        let channel_id = hash_bytes(b"test-channel-1");
        let deposit = 1_000_000;

        let opener =
            ChannelStateMachine::new(channel_id, Role::Opener, deposit, opener_sk, counter_vk);
        let counter = ChannelStateMachine::new(
            channel_id,
            Role::Counterparty,
            deposit,
            counter_sk,
            opener_vk,
        );

        (opener, counter)
    }

    #[test]
    fn test_channel_lifecycle() {
        let (mut opener, mut counter) = setup_channel();

        opener.confirm_open().unwrap();
        counter.confirm_open().unwrap();
        assert_eq!(opener.state, ChannelState::Open);

        let commitment = opener.pay(100).unwrap();
        assert_eq!(commitment.opener_balance, 999_900);
        assert_eq!(commitment.counterparty_balance, 100);

        let signed = counter.receive_state(commitment).unwrap();
        assert!(signed.is_fully_signed());
        assert_eq!(counter.opener_balance, 999_900);
        assert_eq!(counter.counterparty_balance, 100);

        opener.finalize_state(&signed).unwrap();
        assert_eq!(opener.opener_balance, 999_900);
        assert_eq!(opener.nonce, 1);
    }

    #[test]
    fn test_multiple_payments() {
        let (mut opener, mut counter) = setup_channel();
        opener.confirm_open().unwrap();
        counter.confirm_open().unwrap();

        for i in 1..=100 {
            let commitment = opener.pay(10).unwrap();
            let signed = counter.receive_state(commitment).unwrap();
            opener.finalize_state(&signed).unwrap();

            assert_eq!(opener.nonce, i);
            assert_eq!(opener.opener_balance, 1_000_000 - i * 10);
            assert_eq!(opener.counterparty_balance, i * 10);
        }

        assert_eq!(opener.my_balance(), 999_000);
        assert_eq!(counter.my_balance(), 1_000);
    }

    #[test]
    fn test_bidirectional_payments() {
        let (mut opener, mut counter) = setup_channel();
        opener.confirm_open().unwrap();
        counter.confirm_open().unwrap();

        let c1 = opener.pay(500).unwrap();
        let s1 = counter.receive_state(c1).unwrap();
        opener.finalize_state(&s1).unwrap();

        let c2 = counter.pay(200).unwrap();
        let s2 = opener.receive_state(c2).unwrap();
        counter.finalize_state(&s2).unwrap();

        assert_eq!(opener.opener_balance, 999_700);
        assert_eq!(opener.counterparty_balance, 300);
    }

    #[test]
    fn test_conservation_enforced() {
        let (mut opener, _) = setup_channel();
        opener.confirm_open().unwrap();

        let result = opener.propose_state(1_000_001, 0);
        assert!(result.is_err());
        match result.unwrap_err() {
            ChannelError::ConservationViolation { .. } => {}
            e => panic!("Expected ConservationViolation, got: {e}"),
        }
    }

    #[test]
    fn test_nonce_must_increase() {
        let (mut opener, mut counter) = setup_channel();
        opener.confirm_open().unwrap();
        counter.confirm_open().unwrap();

        let c1 = opener.pay(100).unwrap();
        let s1 = counter.receive_state(c1).unwrap();
        opener.finalize_state(&s1).unwrap();

        let mut replay = s1.clone();
        replay.acceptor_sig = None;
        let result = counter.receive_state(replay);
        assert!(result.is_err());
    }

    #[test]
    fn test_insufficient_balance() {
        let (mut opener, _) = setup_channel();
        opener.confirm_open().unwrap();

        let result = opener.pay(1_000_001);
        assert!(result.is_err());
        match result.unwrap_err() {
            ChannelError::InsufficientBalance { .. } => {}
            e => panic!("Expected InsufficientBalance, got: {e}"),
        }
    }

    #[test]
    fn test_dispute_returns_latest_state() {
        let (mut opener, mut counter) = setup_channel();
        opener.confirm_open().unwrap();
        counter.confirm_open().unwrap();

        for _ in 0..5 {
            let c = opener.pay(100).unwrap();
            let s = counter.receive_state(c).unwrap();
            opener.finalize_state(&s).unwrap();
        }

        let dispute = opener.dispute().unwrap();
        assert_eq!(dispute.nonce, 5);
        assert_eq!(dispute.opener_balance, 999_500);
    }

    #[test]
    fn test_close() {
        let (mut opener, _) = setup_channel();
        opener.confirm_open().unwrap();

        let close_commitment = opener.close().unwrap();
        assert_eq!(opener.state, ChannelState::Closing);
        assert_eq!(close_commitment.opener_balance, 1_000_000);
        assert_eq!(close_commitment.counterparty_balance, 0);
    }

    #[test]
    fn test_wrong_state_errors() {
        let (mut opener, _) = setup_channel();

        assert!(opener.pay(100).is_err());

        assert!(opener.close().is_err());

        opener.confirm_open().unwrap();
        opener.close().unwrap();

        assert!(opener.pay(100).is_err());
    }

    #[test]
    fn test_invalid_signature_rejected() {
        let (mut opener, mut counter) = setup_channel();
        opener.confirm_open().unwrap();
        counter.confirm_open().unwrap();

        let mut commitment = opener.pay(100).unwrap();
        commitment.proposer_sig[0] ^= 0xFF;

        let result = counter.receive_state(commitment);
        assert!(result.is_err());
        match result.unwrap_err() {
            ChannelError::InvalidSignature => {}
            e => panic!("Expected InvalidSignature, got: {e}"),
        }
    }
}
