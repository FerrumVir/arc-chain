use crate::account::Address;
use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentityLevel {
    Anonymous,
    Basic,
    Verified,
    Institutional,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub address: Address,
    pub level: IdentityLevel,
    pub attestor: Address,
    pub proof_hash: Hash256,
    pub country_code: [u8; 2],
    pub attested_at: u64,
    pub expires_at: u64,
}

impl Identity {
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at > 0 && now > self.expires_at
    }

    pub fn is_sanctioned_country(&self) -> bool {
        let sanctioned = [*b"KP", *b"IR", *b"SY", *b"CU", *b"RU"];
        sanctioned.contains(&self.country_code)
    }
}
