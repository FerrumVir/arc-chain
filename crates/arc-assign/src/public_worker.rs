//! Signed public-worker offers, independent of validator membership leases.
//!
//! An offer is an authenticated statement by a worker key. It does not prove
//! the claimed capacity, hardware, independence, complete job coverage, or an
//! entitlement to payment. Callers must separately measure and verify work.

use crate::{Address, Hash256};
use arc_crypto::{KeyPair, Signature};
use serde::{Deserialize, Serialize};

pub const PUBLIC_WORKER_OFFER_DOMAIN: &str = "ARC-public-worker-offer-v1";
pub const PUBLIC_WORKER_OFFER_VERSION: u16 = 1;
pub const MAX_TENSOR_RANGES: usize = 256;
pub const MAX_TENSOR_DIMENSION: u32 = 1_048_576;
pub const MAX_MEMORY_CAPACITY_BYTES: u64 = 1 << 60;
pub const MAX_CONCURRENCY: u16 = 1024;
pub const MAX_OFFER_LIFETIME_BLOCKS: u64 = 100_000;
const MAX_SIGNATURE_MATERIAL_BYTES: usize = 6_000;

/// A half-open interval of complete tensor rows. Dimensions describe the full
/// tensor; a range always covers every column for each row in `[row_start,
/// row_end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TensorRowRange {
    pub tensor_id: Hash256,
    pub tensor_rows: u32,
    pub tensor_columns: u32,
    pub row_start: u32,
    pub row_end: u32,
}

/// Worker-authored claims for one public-native execution binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicWorkerOfferBody {
    pub version: u16,
    pub chain_genesis: Hash256,
    pub recovery_epoch: u64,
    pub public_binding: Hash256,
    /// The coordinator audience for this offer. Relays cannot retarget it.
    pub coordinator: Address,
    /// Also the payee address. The protocol does not allow payout redirection.
    pub worker: Address,
    pub artifact_hash: Hash256,
    pub profile_hash: Hash256,
    pub generation_hash: Hash256,
    pub kernel_hash: Hash256,
    pub bundle_hash: Hash256,
    pub ranges: Vec<TensorRowRange>,
    /// Claimed memory capacity in bytes; not independently verified here.
    pub memory_capacity_bytes: u64,
    pub max_concurrency: u16,
    pub issued_at_height: u64,
    pub expires_at_height: u64,
    pub nonce: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicWorkerOffer {
    pub body: PublicWorkerOfferBody,
    pub signature: Signature,
}

/// Context a coordinator must supply when accepting an offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicWorkerOfferRequirements {
    pub chain_genesis: Hash256,
    pub recovery_epoch: u64,
    pub public_binding: Hash256,
    pub coordinator: Address,
    pub artifact_hash: Hash256,
    pub profile_hash: Hash256,
    pub generation_hash: Hash256,
    pub kernel_hash: Hash256,
    pub bundle_hash: Hash256,
    pub now_height: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublicWorkerOfferError {
    #[error("unsupported public-worker offer version {0}")]
    Version(u16),
    #[error("offer contains a zero context, identity, or execution commitment")]
    ZeroCommitment,
    #[error("offer ranges are empty or exceed the maximum of {MAX_TENSOR_RANGES}")]
    RangeCount,
    #[error("tensor range dimensions or row bounds are invalid")]
    InvalidRange,
    #[error("tensor ranges are not sorted, overlap, or disagree on dimensions")]
    RangeOrderOrOverlap,
    #[error("claimed memory capacity is outside the supported bounds")]
    MemoryCapacity,
    #[error("claimed concurrency is outside the supported bounds")]
    Concurrency,
    #[error("offer issue/expiry window is invalid or exceeds the maximum lifetime")]
    TimeWindow,
    #[error("offer nonce must be nonzero")]
    Nonce,
    #[error("offer is for another chain genesis")]
    WrongChain,
    #[error("offer is for another recovery epoch")]
    WrongRecoveryEpoch,
    #[error("offer is for another public binding")]
    WrongBinding,
    #[error("offer names another coordinator audience")]
    WrongAudience,
    #[error("offer execution commitments do not match the required model binding")]
    WrongExecution,
    #[error("offer was issued at height {issued}, but current height is {now}")]
    NotYetValid { issued: u64, now: u64 },
    #[error("offer expired at height {expires}, current height is {now}")]
    Expired { expires: u64, now: u64 },
    #[error("signing key does not match the worker/payee address")]
    WrongSigningKey,
    #[error("signature material exceeds the bounded offer limit")]
    SignatureTooLarge,
    #[error("offer signature is invalid")]
    Signature,
}

impl PublicWorkerOfferBody {
    /// Validate all variable-size and range inputs before encoding or hashing.
    pub fn validate(&self) -> Result<(), PublicWorkerOfferError> {
        if self.version != PUBLIC_WORKER_OFFER_VERSION {
            return Err(PublicWorkerOfferError::Version(self.version));
        }
        if [
            self.chain_genesis,
            self.public_binding,
            self.coordinator,
            self.worker,
            self.artifact_hash,
            self.profile_hash,
            self.generation_hash,
            self.kernel_hash,
            self.bundle_hash,
        ]
        .contains(&Hash256::ZERO)
        {
            return Err(PublicWorkerOfferError::ZeroCommitment);
        }
        if self.ranges.is_empty() || self.ranges.len() > MAX_TENSOR_RANGES {
            return Err(PublicWorkerOfferError::RangeCount);
        }
        let mut previous: Option<TensorRowRange> = None;
        for range in &self.ranges {
            if range.tensor_id == Hash256::ZERO
                || range.tensor_rows == 0
                || range.tensor_columns == 0
                || range.tensor_rows > MAX_TENSOR_DIMENSION
                || range.tensor_columns > MAX_TENSOR_DIMENSION
                || range.row_start >= range.row_end
                || range.row_end > range.tensor_rows
            {
                return Err(PublicWorkerOfferError::InvalidRange);
            }
            if let Some(prev) = previous {
                let ordering = range
                    .tensor_id
                    .0
                    .cmp(&prev.tensor_id.0)
                    .then_with(|| range.row_start.cmp(&prev.row_start));
                if ordering != std::cmp::Ordering::Greater {
                    return Err(PublicWorkerOfferError::RangeOrderOrOverlap);
                }
                if range.tensor_id == prev.tensor_id
                    && (range.tensor_rows != prev.tensor_rows
                        || range.tensor_columns != prev.tensor_columns
                        || range.row_start < prev.row_end)
                {
                    return Err(PublicWorkerOfferError::RangeOrderOrOverlap);
                }
            }
            previous = Some(*range);
        }
        if self.memory_capacity_bytes == 0 || self.memory_capacity_bytes > MAX_MEMORY_CAPACITY_BYTES
        {
            return Err(PublicWorkerOfferError::MemoryCapacity);
        }
        if self.max_concurrency == 0 || self.max_concurrency > MAX_CONCURRENCY {
            return Err(PublicWorkerOfferError::Concurrency);
        }
        if self.issued_at_height >= self.expires_at_height
            || self
                .expires_at_height
                .checked_sub(self.issued_at_height)
                .is_none_or(|lifetime| lifetime > MAX_OFFER_LIFETIME_BLOCKS)
        {
            return Err(PublicWorkerOfferError::TimeWindow);
        }
        if self.nonce == 0 {
            return Err(PublicWorkerOfferError::Nonce);
        }
        Ok(())
    }

    /// Fixed-width canonical transcript bytes. This intentionally does not use
    /// serde/bincode: field order, integer width, and range framing are explicit.
    pub fn canonical_transcript(&self) -> Result<Vec<u8>, PublicWorkerOfferError> {
        self.validate()?;
        let mut out = Vec::with_capacity(334 + self.ranges.len() * 48);
        put_u16(&mut out, self.version);
        put_hash(&mut out, self.chain_genesis);
        put_u64(&mut out, self.recovery_epoch);
        put_hash(&mut out, self.public_binding);
        put_hash(&mut out, self.coordinator);
        put_hash(&mut out, self.worker);
        put_hash(&mut out, self.artifact_hash);
        put_hash(&mut out, self.profile_hash);
        put_hash(&mut out, self.generation_hash);
        put_hash(&mut out, self.kernel_hash);
        put_hash(&mut out, self.bundle_hash);
        put_u16(&mut out, self.ranges.len() as u16);
        for range in &self.ranges {
            put_hash(&mut out, range.tensor_id);
            put_u32(&mut out, range.tensor_rows);
            put_u32(&mut out, range.tensor_columns);
            put_u32(&mut out, range.row_start);
            put_u32(&mut out, range.row_end);
        }
        put_u64(&mut out, self.memory_capacity_bytes);
        put_u16(&mut out, self.max_concurrency);
        put_u64(&mut out, self.issued_at_height);
        put_u64(&mut out, self.expires_at_height);
        put_u64(&mut out, self.nonce);
        Ok(out)
    }

    pub fn transcript(&self) -> Result<Hash256, PublicWorkerOfferError> {
        let bytes = self.canonical_transcript()?;
        let mut hasher = blake3::Hasher::new_derive_key(PUBLIC_WORKER_OFFER_DOMAIN);
        hasher.update(&bytes);
        Ok(Hash256(*hasher.finalize().as_bytes()))
    }
}

impl PublicWorkerOffer {
    pub fn sign(
        body: PublicWorkerOfferBody,
        key: &KeyPair,
    ) -> Result<Self, PublicWorkerOfferError> {
        body.validate()?;
        if body.worker != key.address() {
            return Err(PublicWorkerOfferError::WrongSigningKey);
        }
        let transcript = body.transcript()?;
        let signature = key
            .sign(&transcript)
            .map_err(|_| PublicWorkerOfferError::Signature)?;
        Ok(Self { body, signature })
    }

    pub fn verify(
        &self,
        requirements: &PublicWorkerOfferRequirements,
    ) -> Result<(), PublicWorkerOfferError> {
        // Check attacker-controlled signature vectors before computing any hash.
        if signature_material_len(&self.signature) > MAX_SIGNATURE_MATERIAL_BYTES {
            return Err(PublicWorkerOfferError::SignatureTooLarge);
        }
        let body = &self.body;
        body.validate()?;
        if body.chain_genesis != requirements.chain_genesis {
            return Err(PublicWorkerOfferError::WrongChain);
        }
        if body.recovery_epoch != requirements.recovery_epoch {
            return Err(PublicWorkerOfferError::WrongRecoveryEpoch);
        }
        if body.public_binding != requirements.public_binding {
            return Err(PublicWorkerOfferError::WrongBinding);
        }
        if body.coordinator != requirements.coordinator {
            return Err(PublicWorkerOfferError::WrongAudience);
        }
        if body.artifact_hash != requirements.artifact_hash
            || body.profile_hash != requirements.profile_hash
            || body.generation_hash != requirements.generation_hash
            || body.kernel_hash != requirements.kernel_hash
            || body.bundle_hash != requirements.bundle_hash
        {
            return Err(PublicWorkerOfferError::WrongExecution);
        }
        if requirements.now_height < body.issued_at_height {
            return Err(PublicWorkerOfferError::NotYetValid {
                issued: body.issued_at_height,
                now: requirements.now_height,
            });
        }
        if requirements.now_height >= body.expires_at_height {
            return Err(PublicWorkerOfferError::Expired {
                expires: body.expires_at_height,
                now: requirements.now_height,
            });
        }
        self.signature
            .verify(&body.transcript()?, &body.worker)
            .map_err(|_| PublicWorkerOfferError::Signature)
    }

    /// Stable identifier of this exact signed offer.
    pub fn digest(&self) -> Result<Hash256, PublicWorkerOfferError> {
        if signature_material_len(&self.signature) > MAX_SIGNATURE_MATERIAL_BYTES {
            return Err(PublicWorkerOfferError::SignatureTooLarge);
        }
        let transcript = self.body.transcript()?;
        let mut hasher = blake3::Hasher::new_derive_key("ARC-public-worker-offer-id-v1");
        hasher.update(transcript.as_bytes());
        hash_signature(&mut hasher, &self.signature);
        Ok(Hash256(*hasher.finalize().as_bytes()))
    }
}

fn put_hash(out: &mut Vec<u8>, hash: Hash256) {
    out.extend_from_slice(hash.as_bytes());
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn signature_material_len(signature: &Signature) -> usize {
    match signature {
        Signature::Ed25519 { signature, .. } => signature.len(),
        Signature::Secp256k1 { signature } => signature.len(),
        Signature::MlDsa65 {
            public_key,
            signature,
        } => public_key.len().saturating_add(signature.len()),
        Signature::Falcon512 {
            public_key,
            signature,
        } => public_key.len().saturating_add(signature.len()),
    }
}

fn hash_signature(hasher: &mut blake3::Hasher, signature: &Signature) {
    match signature {
        Signature::Ed25519 {
            public_key,
            signature,
        } => {
            hasher.update(&[0]);
            hasher.update(public_key);
            put_len_and_bytes(hasher, signature);
        }
        Signature::Secp256k1 { signature } => {
            hasher.update(&[1]);
            put_len_and_bytes(hasher, signature);
        }
        Signature::MlDsa65 {
            public_key,
            signature,
        } => {
            hasher.update(&[2]);
            put_len_and_bytes(hasher, public_key);
            put_len_and_bytes(hasher, signature);
        }
        Signature::Falcon512 {
            public_key,
            signature,
        } => {
            hasher.update(&[3]);
            put_len_and_bytes(hasher, public_key);
            put_len_and_bytes(hasher, signature);
        }
    }
}

fn put_len_and_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u32).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(byte: u8) -> Hash256 {
        Hash256([byte; 32])
    }

    fn body(worker: Address) -> PublicWorkerOfferBody {
        PublicWorkerOfferBody {
            version: PUBLIC_WORKER_OFFER_VERSION,
            chain_genesis: h(1),
            recovery_epoch: 7,
            public_binding: h(2),
            coordinator: h(3),
            worker,
            artifact_hash: h(4),
            profile_hash: h(5),
            generation_hash: h(6),
            kernel_hash: h(7),
            bundle_hash: h(8),
            ranges: vec![TensorRowRange {
                tensor_id: h(9),
                tensor_rows: 16,
                tensor_columns: 8,
                row_start: 0,
                row_end: 4,
            }],
            memory_capacity_bytes: 1 << 30,
            max_concurrency: 2,
            issued_at_height: 100,
            expires_at_height: 200,
            nonce: 1,
        }
    }

    fn requirements() -> PublicWorkerOfferRequirements {
        PublicWorkerOfferRequirements {
            chain_genesis: h(1),
            recovery_epoch: 7,
            public_binding: h(2),
            coordinator: h(3),
            artifact_hash: h(4),
            profile_hash: h(5),
            generation_hash: h(6),
            kernel_hash: h(7),
            bundle_hash: h(8),
            now_height: 150,
        }
    }

    #[test]
    fn canonical_transcript_has_a_golden_hash() {
        let body = body(h(10));
        assert_eq!(
            body.transcript().unwrap(),
            Hash256([
                0xbc, 0x24, 0x56, 0x88, 0x0e, 0xfc, 0xdf, 0x6f, 0x0b, 0x42, 0x9f, 0x0b, 0x6f, 0x74,
                0x72, 0xc0, 0x98, 0x38, 0xc5, 0xd2, 0x6f, 0x8b, 0xa4, 0x8f, 0x6b, 0x6d, 0x8d, 0x54,
                0xcb, 0x47, 0xb9, 0xe2,
            ])
        );
    }

    #[test]
    fn signed_offer_verifies_for_exact_context_and_same_payee() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        offer.verify(&requirements()).unwrap();
        assert_eq!(offer.body.worker, key.address());
    }

    #[test]
    fn signature_and_signed_fields_cannot_be_tampered() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        let mut changed = offer.clone();
        changed.body.memory_capacity_bytes += 1;
        assert_eq!(
            changed.verify(&requirements()),
            Err(PublicWorkerOfferError::Signature)
        );

        let mut changed_sig = offer;
        if let Signature::Ed25519 { signature, .. } = &mut changed_sig.signature {
            signature[0] ^= 1;
        }
        assert_eq!(
            changed_sig.verify(&requirements()),
            Err(PublicWorkerOfferError::Signature)
        );
    }

    #[test]
    fn rejects_wrong_chain_and_coordinator_audience() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        let mut req = requirements();
        req.chain_genesis = h(20);
        assert_eq!(offer.verify(&req), Err(PublicWorkerOfferError::WrongChain));
        req = requirements();
        req.coordinator = h(21);
        assert_eq!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::WrongAudience)
        );
    }

    #[test]
    fn rejects_wrong_recovery_binding_and_execution_context() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        let mut req = requirements();
        req.recovery_epoch += 1;
        assert_eq!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::WrongRecoveryEpoch)
        );
        req = requirements();
        req.public_binding = h(22);
        assert_eq!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::WrongBinding)
        );
        req = requirements();
        req.artifact_hash = h(23);
        assert_eq!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::WrongExecution)
        );
    }

    #[test]
    fn rejects_not_yet_valid_and_expired_offers() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        let mut req = requirements();
        req.now_height = 99;
        assert!(matches!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::NotYetValid { .. })
        ));
        req.now_height = 200;
        assert!(matches!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::Expired { .. })
        ));
    }

    #[test]
    fn rejects_unsupported_version_bad_shape_unsorted_and_overlapping_ranges() {
        let mut b = body(h(10));
        b.version = 2;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::Version(2)));
        b = body(h(10));
        b.ranges[0].row_end = 17;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::InvalidRange));
        b = body(h(10));
        let mut overlap = b.ranges[0];
        overlap.row_start = 3;
        overlap.row_end = 6;
        b.ranges.push(overlap);
        assert_eq!(
            b.validate(),
            Err(PublicWorkerOfferError::RangeOrderOrOverlap)
        );
        b = body(h(10));
        b.ranges[0].tensor_columns = 0;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::InvalidRange));
    }

    #[test]
    fn rejects_wrong_signer_and_oversized_range_list_before_transcript() {
        let key = KeyPair::generate_ed25519();
        assert_eq!(
            PublicWorkerOffer::sign(body(h(99)), &key),
            Err(PublicWorkerOfferError::WrongSigningKey)
        );
        let mut b = body(h(10));
        b.ranges.resize(MAX_TENSOR_RANGES + 1, b.ranges[0]);
        assert_eq!(
            b.canonical_transcript(),
            Err(PublicWorkerOfferError::RangeCount)
        );
    }

    #[test]
    fn rejects_zero_capacity_concurrency_and_excessive_lifetime() {
        let mut b = body(h(10));
        b.memory_capacity_bytes = 0;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::MemoryCapacity));
        b = body(h(10));
        b.max_concurrency = 0;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::Concurrency));
        b = body(h(10));
        b.expires_at_height = b.issued_at_height + MAX_OFFER_LIFETIME_BLOCKS + 1;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::TimeWindow));
    }
}
