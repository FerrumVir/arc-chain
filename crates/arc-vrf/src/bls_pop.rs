//! BLS12-381 proof of possession for validator key registration.
//!
//! Scheme: the proof-of-possession scheme of the IETF BLS signature draft
//! (draft-irtf-cfrg-bls-signature, Section 3.3), `min_pk` variant. Public keys
//! are 48-byte compressed G1 points; signatures and proofs are 96-byte
//! compressed G2 points. Hashing to G2 is `BLS12381G2_XMD:SHA-256_SSWU_RO_`
//! (RFC 9380). Two distinct domain separation tags keep message signatures and
//! proofs of possession apart:
//!
//! - messages: `BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_`, the tag that
//!   ARC's existing `arc-crypto::bls` module and Ethereum consensus use;
//! - proofs of possession: `BLS_POP_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_`.
//!
//! `PopProve(SK) = CoreSign(SK, PK_bytes)` under the `BLS_POP_` tag, and
//! `PopVerify(PK, proof) = CoreVerify(PK, PK_bytes, proof)` under the same tag.
//! Registering a key without a valid proof of possession enables the rogue-key
//! attack on aggregated keys: an attacker publishes `pk_rogue = pk_attacker -
//! pk_victim`, after which `aggregate(pk_rogue, pk_victim) = pk_attacker` and
//! the attacker alone forges "two-party" aggregate signatures. The
//! `tests/bls_pop.rs` integration test performs that attack and shows the proof
//! of possession defeating it.
//!
//! All arithmetic is `blst` (supranational), already the BLS library of
//! `arc-crypto`. Every public constructor validates its input: public keys are
//! rejected when they are the identity or lie outside the prime-order subgroup;
//! signatures and proofs are rejected when they are the identity or lie outside
//! the subgroup. [`EnrolledBlsKey`] can only be built by verifying a proof, and
//! [`aggregate_enrolled`] accepts nothing else, so a key without a valid proof
//! cannot reach an aggregate.
//!
//! This module is not wired into consensus. `arc-crypto::bls` keeps serving the
//! existing aggregate-signature code until the Stage 0 wiring PR replaces it.

use blst::BLST_ERROR;
use blst::min_pk::{
    AggregatePublicKey, AggregateSignature as BlstAggregateSignature, PublicKey as BlstPublicKey,
    SecretKey as BlstSecretKey, Signature as BlstSignature,
};

/// Compressed G1 public key length.
pub const BLS_PUBLIC_KEY_LENGTH: usize = 48;
/// Compressed G2 signature and proof-of-possession length.
pub const BLS_SIGNATURE_LENGTH: usize = 96;
/// Secret key (scalar) length, big-endian.
pub const BLS_SECRET_KEY_LENGTH: usize = 32;
/// Minimum input keying material for [`BlsSecretKey::key_gen`]; the IETF
/// `KeyGen` requires at least 32 bytes.
pub const MIN_IKM_LENGTH: usize = 32;
/// Domain separation tag for message signatures (PoP scheme, `min_pk`).
pub const SIGNATURE_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";
/// Domain separation tag for proofs of possession (PoP scheme, `min_pk`).
pub const POP_DST: &[u8] = b"BLS_POP_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

/// Errors of the BLS proof-of-possession API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlsPopError {
    /// `key_gen` needs at least 32 bytes of input keying material.
    #[error("input keying material must be at least 32 bytes, got {0}")]
    ShortIkm(usize),
    /// Secret key bytes are zero or not below the group order.
    #[error("secret key bytes are not a valid non-zero BLS12-381 scalar")]
    InvalidSecretKey,
    /// Public key bytes do not decode to a non-identity point in the G1 prime-order subgroup.
    #[error("public key is not a valid non-identity BLS12-381 G1 subgroup point")]
    InvalidPublicKey,
    /// Signature bytes do not decode to a non-identity point in the G2 prime-order subgroup.
    #[error("signature is not a valid non-identity BLS12-381 G2 subgroup point")]
    InvalidSignature,
    /// The proof of possession does not verify for the public key.
    #[error("proof of possession does not verify for this public key")]
    InvalidProofOfPossession,
    /// Nothing to aggregate.
    #[error("cannot aggregate an empty set")]
    EmptyAggregate,
}

/// A BLS12-381 secret key (scalar). `blst` zeroizes it on drop.
#[derive(Clone)]
pub struct BlsSecretKey {
    inner: BlstSecretKey,
}

impl core::fmt::Debug for BlsSecretKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BlsSecretKey([redacted])")
    }
}

impl BlsSecretKey {
    /// IETF `KeyGen(IKM, key_info)`: HKDF-SHA-256 with salt `"BLS-SIG-KEYGEN-SALT-"`
    /// (re-hashed while the candidate is zero), `L = 48`, reduced modulo `r`
    /// (`blst::min_pk::SecretKey::key_gen`). With empty `key_info` this is
    /// ERC-2333 `derive_master_SK`, whose published vectors `tests/bls_pop.rs`
    /// checks. `ikm` must be at least 32 bytes of operating-system randomness.
    pub fn key_gen(ikm: &[u8], key_info: &[u8]) -> Result<Self, BlsPopError> {
        if ikm.len() < MIN_IKM_LENGTH {
            return Err(BlsPopError::ShortIkm(ikm.len()));
        }
        let inner =
            BlstSecretKey::key_gen(ikm, key_info).map_err(|_| BlsPopError::InvalidSecretKey)?;
        Ok(Self { inner })
    }

    /// Loads a 32-byte big-endian scalar; zero and values at or above `r` are rejected.
    pub fn from_bytes(bytes: &[u8; BLS_SECRET_KEY_LENGTH]) -> Result<Self, BlsPopError> {
        let inner = BlstSecretKey::from_bytes(bytes).map_err(|_| BlsPopError::InvalidSecretKey)?;
        Ok(Self { inner })
    }

    /// The 32-byte big-endian scalar.
    pub fn to_bytes(&self) -> [u8; BLS_SECRET_KEY_LENGTH] {
        self.inner.to_bytes()
    }

    /// The public key `pk = sk * G1`.
    pub fn public_key(&self) -> BlsPublicKey {
        let inner = self.inner.sk_to_pk();
        BlsPublicKey {
            inner,
            bytes: inner.compress(),
        }
    }

    /// `CoreSign` under [`SIGNATURE_DST`]. Deterministic.
    pub fn sign(&self, message: &[u8]) -> BlsSignature {
        let inner = self.inner.sign(message, SIGNATURE_DST, &[]);
        BlsSignature {
            inner,
            bytes: inner.compress(),
        }
    }

    /// `PopProve(SK)`: `CoreSign(SK, PK_bytes)` under [`POP_DST`]. Deterministic.
    pub fn prove_possession(&self) -> ProofOfPossession {
        let public_key = self.public_key();
        let inner = self.inner.sign(&public_key.bytes, POP_DST, &[]);
        ProofOfPossession(BlsSignature {
            inner,
            bytes: inner.compress(),
        })
    }
}

/// A validated BLS12-381 public key (compressed G1, 48 bytes): not the identity,
/// in the prime-order subgroup, canonically encoded.
#[derive(Clone, Copy, Debug)]
pub struct BlsPublicKey {
    inner: BlstPublicKey,
    bytes: [u8; BLS_PUBLIC_KEY_LENGTH],
}

impl PartialEq for BlsPublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for BlsPublicKey {}

impl BlsPublicKey {
    /// `KeyValidate`: decodes 48 compressed bytes and rejects the identity, points
    /// outside the prime-order subgroup, and non-canonical encodings.
    pub fn from_bytes(bytes: &[u8; BLS_PUBLIC_KEY_LENGTH]) -> Result<Self, BlsPopError> {
        let inner =
            BlstPublicKey::key_validate(bytes).map_err(|_| BlsPopError::InvalidPublicKey)?;
        if inner.compress() != *bytes {
            return Err(BlsPopError::InvalidPublicKey);
        }
        Ok(Self {
            inner,
            bytes: *bytes,
        })
    }

    /// The compressed public key.
    pub fn as_bytes(&self) -> &[u8; BLS_PUBLIC_KEY_LENGTH] {
        &self.bytes
    }

    /// The compressed public key, by value.
    pub fn to_bytes(self) -> [u8; BLS_PUBLIC_KEY_LENGTH] {
        self.bytes
    }

    /// `CoreVerify` under [`SIGNATURE_DST`]. Also verifies an aggregate signature
    /// over one message against the matching aggregate public key.
    pub fn verify(&self, message: &[u8], signature: &BlsSignature) -> bool {
        signature
            .inner
            .verify(true, message, SIGNATURE_DST, &[], &self.inner, true)
            == BLST_ERROR::BLST_SUCCESS
    }

    /// `PopVerify(PK, proof)`: `CoreVerify(PK, PK_bytes, proof)` under [`POP_DST`].
    pub fn verify_possession(&self, proof: &ProofOfPossession) -> bool {
        proof
            .0
            .inner
            .verify(true, &self.bytes, POP_DST, &[], &self.inner, true)
            == BLST_ERROR::BLST_SUCCESS
    }
}

/// A validated BLS12-381 signature (compressed G2, 96 bytes): not the identity,
/// in the prime-order subgroup, canonically encoded.
#[derive(Clone, Copy, Debug)]
pub struct BlsSignature {
    inner: BlstSignature,
    bytes: [u8; BLS_SIGNATURE_LENGTH],
}

impl PartialEq for BlsSignature {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for BlsSignature {}

impl BlsSignature {
    /// Decodes 96 compressed bytes and rejects the identity, points outside the
    /// prime-order subgroup, and non-canonical encodings.
    pub fn from_bytes(bytes: &[u8; BLS_SIGNATURE_LENGTH]) -> Result<Self, BlsPopError> {
        let inner =
            BlstSignature::sig_validate(bytes, true).map_err(|_| BlsPopError::InvalidSignature)?;
        if inner.compress() != *bytes {
            return Err(BlsPopError::InvalidSignature);
        }
        Ok(Self {
            inner,
            bytes: *bytes,
        })
    }

    /// The compressed signature.
    pub fn as_bytes(&self) -> &[u8; BLS_SIGNATURE_LENGTH] {
        &self.bytes
    }

    /// The compressed signature, by value.
    pub fn to_bytes(self) -> [u8; BLS_SIGNATURE_LENGTH] {
        self.bytes
    }
}

/// A proof of possession: a signature over the public key bytes under [`POP_DST`].
///
/// A distinct type, so a message signature cannot be passed where a proof is
/// expected or the other way round without an explicit byte-level conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProofOfPossession(BlsSignature);

impl ProofOfPossession {
    /// Decodes 96 compressed bytes with the same checks as [`BlsSignature::from_bytes`].
    pub fn from_bytes(bytes: &[u8; BLS_SIGNATURE_LENGTH]) -> Result<Self, BlsPopError> {
        BlsSignature::from_bytes(bytes).map(Self)
    }

    /// The compressed proof.
    pub fn as_bytes(&self) -> &[u8; BLS_SIGNATURE_LENGTH] {
        self.0.as_bytes()
    }

    /// The compressed proof, by value.
    pub fn to_bytes(self) -> [u8; BLS_SIGNATURE_LENGTH] {
        self.0.bytes
    }
}

/// A public key whose proof of possession has been verified.
///
/// This is the only type [`aggregate_enrolled`] accepts, so a key without a
/// valid proof can never enter an aggregate: the rogue-key defence of the
/// proof-of-possession scheme, enforced by the type system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnrolledBlsKey {
    key: BlsPublicKey,
}

impl EnrolledBlsKey {
    /// Verifies `proof` for `key` and, only on success, marks the key as enrolled.
    pub fn enroll(key: BlsPublicKey, proof: &ProofOfPossession) -> Result<Self, BlsPopError> {
        if key.verify_possession(proof) {
            Ok(Self { key })
        } else {
            Err(BlsPopError::InvalidProofOfPossession)
        }
    }

    /// The enrolled public key.
    pub fn public_key(&self) -> &BlsPublicKey {
        &self.key
    }
}

/// Sums enrolled public keys. Verify an aggregate signature over one message
/// with [`BlsPublicKey::verify`] on the result.
///
/// The sum of valid keys can still be the identity (for example a key and its
/// negation); such an aggregate is rejected like any other identity key.
pub fn aggregate_enrolled(keys: &[EnrolledBlsKey]) -> Result<BlsPublicKey, BlsPopError> {
    if keys.is_empty() {
        return Err(BlsPopError::EmptyAggregate);
    }
    let refs: Vec<&BlstPublicKey> = keys.iter().map(|key| &key.key.inner).collect();
    // Group checks already happened in `BlsPublicKey::from_bytes`.
    let aggregate =
        AggregatePublicKey::aggregate(&refs, false).map_err(|_| BlsPopError::InvalidPublicKey)?;
    let inner = aggregate.to_public_key();
    inner
        .validate()
        .map_err(|_| BlsPopError::InvalidPublicKey)?;
    Ok(BlsPublicKey {
        inner,
        bytes: inner.compress(),
    })
}

/// Sums signatures over the same message, for verification against
/// [`aggregate_enrolled`]'s result.
pub fn aggregate_signatures(signatures: &[BlsSignature]) -> Result<BlsSignature, BlsPopError> {
    if signatures.is_empty() {
        return Err(BlsPopError::EmptyAggregate);
    }
    let refs: Vec<&BlstSignature> = signatures
        .iter()
        .map(|signature| &signature.inner)
        .collect();
    // Group checks already happened in `BlsSignature::from_bytes`.
    let aggregate = BlstAggregateSignature::aggregate(&refs, false)
        .map_err(|_| BlsPopError::InvalidSignature)?;
    let inner = aggregate.to_signature();
    inner
        .validate(true)
        .map_err(|_| BlsPopError::InvalidSignature)?;
    Ok(BlsSignature {
        inner,
        bytes: inner.compress(),
    })
}
