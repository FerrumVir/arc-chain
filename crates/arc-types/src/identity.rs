use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};
use crate::account::Address;

/// Identity attestation level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentityLevel {
    /// No verification.
    Anonymous,
    /// Email verified.
    Basic,
    /// KYC/AML verified by an approved attestor.
    Verified,
    /// Institutional-grade verification (regulated entity).
    Institutional,
}

/// On-chain identity record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    /// Account address this identity is bound to.
    pub address: Address,
    /// Verification level.
    pub level: IdentityLevel,
    /// Attestor address (the entity that verified this identity).
    pub attestor: Address,
    /// Hash of the off-chain identity proof (stored externally).
    pub proof_hash: Hash256,
    /// Country code (ISO 3166-1 alpha-2, e.g. "US").
    pub country_code: [u8; 2],
    /// Timestamp of attestation (unix millis).
    pub attested_at: u64,
    /// Expiry timestamp (unix millis, 0 = never).
    pub expires_at: u64,
}

impl Identity {
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at > 0 && now > self.expires_at
    }

    pub fn is_sanctioned_country(&self) -> bool {
        // OFAC sanctioned country codes
        let sanctioned = [
            *b"KP", *b"IR", *b"SY", *b"CU", *b"RU",
        ];
        sanctioned.contains(&self.country_code)
    }
}
