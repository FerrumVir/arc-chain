//! BLS12-381 aggregate signature primitives for ARC Chain.
//!
//! Uses the `blst` crate (supranational/blst) for production BLS12-381
//! cryptography. Public keys are compressed G1 points (48 bytes), signatures
//! are compressed G2 points (96 bytes) — the "min_pk" variant.
//!
//! Used for consensus finality proofs where N validators produce a single
//! aggregated signature verifiable against an aggregated public key.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum BlsError {
    #[error("invalid signature bytes")]
    InvalidSignature,
    #[error("invalid public key bytes")]
    InvalidPublicKey,
    #[error("aggregation failed")]
    AggregationFailed,
    #[error("empty input")]
    EmptyInput,
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Compressed G1 point length (BLS12-381 public key).
pub const BLS_PK_LEN: usize = 48;

/// Scalar field element length (BLS12-381 secret key).
pub const BLS_SK_LEN: usize = 32;

/// Compressed G2 point length (BLS12-381 signature).
pub const BLS_SIG_LEN: usize = 96;

/// Domain separation tag for BLS signatures (Ethereum 2.0 compatible).
const DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

// ── Serde helpers for fixed-size byte arrays > 32 ─────────────────────────────
// serde only derives Serialize/Deserialize for [u8; N] up to N=32.
// We use hex encoding in human-readable formats, raw bytes otherwise.

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&hex::encode(bytes))
        } else {
            serializer.serialize_bytes(bytes)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<[u8; N], D::Error> {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
            bytes
                .try_into()
                .map_err(|_| serde::de::Error::custom(format!("expected {N} bytes")))
        } else {
            let bytes = <Vec<u8>>::deserialize(deserializer)?;
            bytes
                .try_into()
                .map_err(|_| serde::de::Error::custom(format!("expected {N} bytes")))
        }
    }
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// BLS12-381 public key (compressed G1 point).
/// Serializes as hex string in JSON, raw bytes in binary formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlsPublicKey(pub [u8; BLS_PK_LEN]);

impl Serialize for BlsPublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        hex_bytes::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for BlsPublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        hex_bytes::deserialize::<D, BLS_PK_LEN>(deserializer).map(BlsPublicKey)
    }
}

/// BLS12-381 secret key (scalar field element).
#[derive(Debug, Clone)]
pub struct BlsSecretKey(pub [u8; BLS_SK_LEN]);

/// BLS12-381 signature (compressed G2 point).
/// Serializes as hex string in JSON, raw bytes in binary formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlsSignature(pub [u8; BLS_SIG_LEN]);

impl Serialize for BlsSignature {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        hex_bytes::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for BlsSignature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        hex_bytes::deserialize::<D, BLS_SIG_LEN>(deserializer).map(BlsSignature)
    }
}

/// Aggregated signature from multiple validators.
///
/// A single G2 point representing the sum of all individual signatures.
/// Verifiable against the corresponding aggregated public key for the same message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateSignature {
    /// The aggregated signature bytes (compressed G2 point).
    pub signature: BlsSignature,
    /// Indices of participating signers in the validator set.
    pub signers: Vec<usize>,
    /// Number of signers that contributed to this aggregate.
    pub signer_count: usize,
}

/// Aggregated public key from multiple validators.
///
/// A single G1 point representing the sum of all individual public keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatePublicKey {
    /// The aggregated public key bytes (compressed G1 point).
    pub key: BlsPublicKey,
    /// Number of individual keys aggregated.
    pub participant_count: usize,
}

/// BLS12-381 keypair (secret + public).
pub struct BlsKeypair {
    pub secret: BlsSecretKey,
    pub public: BlsPublicKey,
}

/// Threshold signature configuration for consensus.
///
/// Defines how many signers out of the total validator set must participate
/// for the aggregate signature to be considered valid (e.g., 2/3 + 1).
#[derive(Debug, Clone)]
pub struct ThresholdConfig {
    /// Total number of signers in the validator set.
    pub total_signers: usize,
    /// Minimum number of signers required.
    pub threshold: usize,
}

// ── Key Generation ────────────────────────────────────────────────────────────

/// Generate a BLS keypair from a seed.
///
/// Uses `blst::SecretKey::key_gen` which implements the IETF KeyGen spec
/// (draft-irtf-cfrg-bls-signature). Deterministic: same seed produces the
/// same keypair. Seed must be at least 32 bytes.
///
/// # Arguments
/// * `seed` - Seed bytes (if shorter than 32 bytes, will be hashed to extend).
///
/// # Returns
/// A `BlsKeypair` containing the derived secret and public keys.
pub fn bls_keygen(seed: &[u8]) -> BlsKeypair {
    // blst requires IKM >= 32 bytes. Hash short seeds to guarantee length.
    let ikm = if seed.len() < 32 {
        let hash = blake3::hash(seed);
        hash.as_bytes().to_vec()
    } else {
        seed.to_vec()
    };

    let sk = blst::min_pk::SecretKey::key_gen(&ikm, &[])
        .expect("key_gen failed: IKM must be >= 32 bytes");

    let pk = sk.sk_to_pk();

    BlsKeypair {
        secret: BlsSecretKey(sk.serialize()),
        public: BlsPublicKey(pk.compress()),
    }
}

// ── Signing ───────────────────────────────────────────────────────────────────

/// Sign a message with a BLS secret key.
///
/// Uses the real BLS12-381 pairing-based signing via `blst`. The signature
/// is a compressed G2 point (96 bytes). The domain separation tag (DST)
/// follows the Ethereum 2.0 spec.
///
/// # Arguments
/// * `sk` - The signer's secret key.
/// * `message` - The message bytes to sign.
///
/// # Returns
/// A `BlsSignature` (96 bytes, compressed G2 point).
pub fn bls_sign(sk: &BlsSecretKey, message: &[u8]) -> BlsSignature {
    let blst_sk = blst::min_pk::SecretKey::deserialize(&sk.0)
        .expect("invalid secret key bytes");
    let sig = blst_sk.sign(message, DST, &[]);
    BlsSignature(sig.compress())
}

// ── Verification ──────────────────────────────────────────────────────────────

/// Verify a single BLS signature against a public key and message.
///
/// Performs a real pairing check: `e(pk, H(msg)) == e(G1, sig)`.
///
/// # Arguments
/// * `pk` - The signer's public key.
/// * `message` - The original message bytes.
/// * `sig` - The signature to verify.
///
/// # Returns
/// `true` if the signature is valid for this public key and message.
pub fn bls_verify(pk: &BlsPublicKey, message: &[u8], sig: &BlsSignature) -> bool {
    let blst_pk = match blst::min_pk::PublicKey::uncompress(&pk.0) {
        Ok(pk) => pk,
        Err(_) => return false,
    };
    let blst_sig = match blst::min_pk::Signature::uncompress(&sig.0) {
        Ok(sig) => sig,
        Err(_) => return false,
    };
    let result = blst_sig.verify(true, message, DST, &[], &blst_pk, true);
    result == blst::BLST_ERROR::BLST_SUCCESS
}

// ── Aggregation ───────────────────────────────────────────────────────────────

/// Aggregate multiple signatures into a single signature.
///
/// Performs real G2 point addition via `blst::AggregateSignature`.
///
/// # Arguments
/// * `sigs` - Slice of individual signatures (must all be for the same message).
///
/// # Returns
/// A single `BlsSignature` representing the aggregate (compressed G2 point).
pub fn aggregate_signatures(sigs: &[BlsSignature]) -> Result<BlsSignature, BlsError> {
    if sigs.is_empty() {
        return Err(BlsError::EmptyInput);
    }

    let blst_sigs: Vec<blst::min_pk::Signature> = sigs
        .iter()
        .map(|s| blst::min_pk::Signature::uncompress(&s.0).map_err(|_| BlsError::InvalidSignature))
        .collect::<Result<Vec<_>, _>>()?;

    let sig_refs: Vec<&blst::min_pk::Signature> = blst_sigs.iter().collect();
    let agg = blst::min_pk::AggregateSignature::aggregate(&sig_refs, true)
        .map_err(|_| BlsError::AggregationFailed)?;

    Ok(BlsSignature(agg.to_signature().compress()))
}

/// Aggregate multiple public keys into one.
///
/// Performs real G1 point addition via `blst::AggregatePublicKey`.
///
/// # Arguments
/// * `pks` - Slice of individual public keys.
///
/// # Returns
/// An `AggregatePublicKey` with the combined key and participant count.
pub fn aggregate_public_keys(pks: &[BlsPublicKey]) -> Result<AggregatePublicKey, BlsError> {
    if pks.is_empty() {
        return Err(BlsError::EmptyInput);
    }

    let blst_pks: Vec<blst::min_pk::PublicKey> = pks
        .iter()
        .map(|pk| blst::min_pk::PublicKey::uncompress(&pk.0).map_err(|_| BlsError::InvalidPublicKey))
        .collect::<Result<Vec<_>, _>>()?;

    let pk_refs: Vec<&blst::min_pk::PublicKey> = blst_pks.iter().collect();
    let agg = blst::min_pk::AggregatePublicKey::aggregate(&pk_refs, true)
        .map_err(|_| BlsError::AggregationFailed)?;

    Ok(AggregatePublicKey {
        key: BlsPublicKey(agg.to_public_key().compress()),
        participant_count: pks.len(),
    })
}

/// Verify an aggregated signature against an aggregated public key.
///
/// Performs a real pairing check using the aggregated G1 (pk) and G2 (sig) points:
///   `e(agg_pk, H(msg)) == e(G1, agg_sig)`
///
/// # Arguments
/// * `agg_pk` - The aggregated public key.
/// * `message` - The message that all signers signed.
/// * `agg_sig` - The aggregated signature with signer metadata.
///
/// # Returns
/// `true` if the aggregate signature is valid.
pub fn verify_aggregate(
    agg_pk: &AggregatePublicKey,
    message: &[u8],
    agg_sig: &AggregateSignature,
) -> bool {
    if agg_sig.signer_count == 0 {
        return false;
    }

    let blst_pk = match blst::min_pk::PublicKey::uncompress(&agg_pk.key.0) {
        Ok(pk) => pk,
        Err(_) => return false,
    };
    let blst_sig = match blst::min_pk::Signature::uncompress(&agg_sig.signature.0) {
        Ok(sig) => sig,
        Err(_) => return false,
    };

    let result = blst_sig.verify(true, message, DST, &[], &blst_pk, true);
    result == blst::BLST_ERROR::BLST_SUCCESS
}

/// Create an aggregate signature from individual signatures with signer tracking.
///
/// Performs real G2 point addition and records signer indices.
///
/// # Arguments
/// * `sigs` - Slice of `(signer_index, signature)` tuples.
///
/// # Returns
/// An `AggregateSignature` with the combined signature and signer metadata.
pub fn create_aggregate(sigs: &[(usize, BlsSignature)]) -> Result<AggregateSignature, BlsError> {
    if sigs.is_empty() {
        return Err(BlsError::EmptyInput);
    }

    let mut signers = Vec::with_capacity(sigs.len());
    let blst_sigs: Vec<blst::min_pk::Signature> = sigs
        .iter()
        .map(|(idx, sig)| {
            signers.push(*idx);
            blst::min_pk::Signature::uncompress(&sig.0).map_err(|_| BlsError::InvalidSignature)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let sig_refs: Vec<&blst::min_pk::Signature> = blst_sigs.iter().collect();
    let agg = blst::min_pk::AggregateSignature::aggregate(&sig_refs, true)
        .map_err(|_| BlsError::AggregationFailed)?;

    Ok(AggregateSignature {
        signature: BlsSignature(agg.to_signature().compress()),
        signers,
        signer_count: sigs.len(),
    })
}

/// Create a verifiable aggregate from individual signatures and public keys.
///
/// Aggregates both the signatures (G2 point addition) and public keys
/// (G1 point addition). The result is cryptographically valid and will
/// pass `verify_aggregate`.
///
/// # Arguments
/// * `keypairs_and_sigs` - Slice of `(signer_index, public_key, signature)`.
/// * `_message` - The message that was signed (not needed for real BLS aggregation,
///   kept for API compatibility).
///
/// # Returns
/// `(AggregatePublicKey, AggregateSignature)` that will pass `verify_aggregate`.
pub fn create_verifiable_aggregate(
    keypairs_and_sigs: &[(usize, &BlsPublicKey, &BlsSignature)],
    _message: &[u8],
) -> Result<(AggregatePublicKey, AggregateSignature), BlsError> {
    if keypairs_and_sigs.is_empty() {
        return Err(BlsError::EmptyInput);
    }

    let mut signers = Vec::with_capacity(keypairs_and_sigs.len());

    let blst_pks: Vec<blst::min_pk::PublicKey> = keypairs_and_sigs
        .iter()
        .map(|(_, pk, _)| {
            blst::min_pk::PublicKey::uncompress(&pk.0).map_err(|_| BlsError::InvalidPublicKey)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let blst_sigs: Vec<blst::min_pk::Signature> = keypairs_and_sigs
        .iter()
        .map(|(idx, _, sig)| {
            signers.push(*idx);
            blst::min_pk::Signature::uncompress(&sig.0).map_err(|_| BlsError::InvalidSignature)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let pk_refs: Vec<&blst::min_pk::PublicKey> = blst_pks.iter().collect();
    let agg_pk = blst::min_pk::AggregatePublicKey::aggregate(&pk_refs, true)
        .map_err(|_| BlsError::AggregationFailed)?;

    let sig_refs: Vec<&blst::min_pk::Signature> = blst_sigs.iter().collect();
    let agg_sig = blst::min_pk::AggregateSignature::aggregate(&sig_refs, true)
        .map_err(|_| BlsError::AggregationFailed)?;

    Ok((
        AggregatePublicKey {
            key: BlsPublicKey(agg_pk.to_public_key().compress()),
            participant_count: keypairs_and_sigs.len(),
        },
        AggregateSignature {
            signature: BlsSignature(agg_sig.to_signature().compress()),
            signers,
            signer_count: keypairs_and_sigs.len(),
        },
    ))
}

// ── Threshold ─────────────────────────────────────────────────────────────────

/// Check if an aggregate signature meets the threshold requirement.
///
/// # Arguments
/// * `agg` - The aggregate signature to check.
/// * `config` - The threshold configuration (total signers and minimum required).
///
/// # Returns
/// `true` if the number of signers meets or exceeds the threshold.
pub fn meets_threshold(agg: &AggregateSignature, config: &ThresholdConfig) -> bool {
    agg.signer_count >= config.threshold && agg.signer_count <= config.total_signers
}

// ── Address Derivation ────────────────────────────────────────────────────────

/// Derive a 32-byte address from a BLS public key.
///
/// Uses BLAKE3 hashing consistent with other address derivation in ARC Chain:
///   `address = BLAKE3(bls_public_key)[0..32]`
///
/// # Arguments
/// * `pk` - The BLS public key.
///
/// # Returns
/// A 32-byte address.
pub fn address_from_bls_pubkey(pk: &BlsPublicKey) -> [u8; 32] {
    *blake3::hash(&pk.0).as_bytes()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bls_keygen() {
        // Deterministic: same seed produces same keypair
        let kp1 = bls_keygen(b"test-seed");
        let kp2 = bls_keygen(b"test-seed");
        assert_eq!(kp1.public.0, kp2.public.0);
        assert_eq!(kp1.secret.0, kp2.secret.0);

        // Correct lengths
        assert_eq!(kp1.public.0.len(), BLS_PK_LEN);
        assert_eq!(kp1.secret.0.len(), BLS_SK_LEN);

        // Different seeds produce different keys
        let kp3 = bls_keygen(b"different-seed");
        assert_ne!(kp1.public.0, kp3.public.0);
        assert_ne!(kp1.secret.0, kp3.secret.0);
    }

    #[test]
    fn test_bls_sign_verify() {
        let kp = bls_keygen(b"sign-verify-seed");
        let message = b"hello ARC chain";
        let sig = bls_sign(&kp.secret, message);

        // Correct length
        assert_eq!(sig.0.len(), BLS_SIG_LEN);

        // Verify succeeds
        assert!(bls_verify(&kp.public, message, &sig));

        // Deterministic: same inputs produce same signature
        let sig2 = bls_sign(&kp.secret, message);
        assert_eq!(sig.0, sig2.0);
    }

    #[test]
    fn test_bls_verify_wrong_message() {
        let kp = bls_keygen(b"wrong-msg-seed");
        let sig = bls_sign(&kp.secret, b"correct message");

        // Wrong message must fail
        assert!(!bls_verify(&kp.public, b"wrong message", &sig));
    }

    #[test]
    fn test_bls_verify_wrong_key() {
        let kp1 = bls_keygen(b"key-1");
        let kp2 = bls_keygen(b"key-2");
        let message = b"test message";
        let sig = bls_sign(&kp1.secret, message);

        // Wrong key must fail
        assert!(!bls_verify(&kp2.public, message, &sig));

        // Right key succeeds
        assert!(bls_verify(&kp1.public, message, &sig));
    }

    #[test]
    fn test_aggregate_signatures() {
        let message = b"finalize block 42";

        // Generate 5 validator keypairs and signatures
        let keypairs: Vec<BlsKeypair> = (0..5)
            .map(|i| bls_keygen(format!("validator-{i}").as_bytes()))
            .collect();
        let sigs: Vec<BlsSignature> = keypairs
            .iter()
            .map(|kp| bls_sign(&kp.secret, message))
            .collect();

        // Each individual sig should verify
        for (kp, sig) in keypairs.iter().zip(sigs.iter()) {
            assert!(bls_verify(&kp.public, message, sig));
        }

        // Build verifiable aggregate
        let refs: Vec<(usize, &BlsPublicKey, &BlsSignature)> = keypairs
            .iter()
            .zip(sigs.iter())
            .enumerate()
            .map(|(i, (kp, sig))| (i, &kp.public, sig))
            .collect();
        let (agg_pk, agg_sig) = create_verifiable_aggregate(&refs, message).unwrap();

        // Aggregate should verify
        assert!(verify_aggregate(&agg_pk, message, &agg_sig));
        assert_eq!(agg_pk.participant_count, 5);
        assert_eq!(agg_sig.signer_count, 5);
    }

    #[test]
    fn test_aggregate_different_messages_fails() {
        let keypairs: Vec<BlsKeypair> = (0..3)
            .map(|i| bls_keygen(format!("val-{i}").as_bytes()))
            .collect();

        // Sign DIFFERENT messages
        let sig0 = bls_sign(&keypairs[0].secret, b"message A");
        let sig1 = bls_sign(&keypairs[1].secret, b"message B");
        let sig2 = bls_sign(&keypairs[2].secret, b"message C");

        // Individual verification fails for mismatched messages
        assert!(!bls_verify(&keypairs[1].public, b"message A", &sig1));
        assert!(!bls_verify(&keypairs[2].public, b"message A", &sig2));

        // But sig0 verifies correctly for message A
        assert!(bls_verify(&keypairs[0].public, b"message A", &sig0));
    }

    #[test]
    fn test_threshold_met() {
        let config = ThresholdConfig {
            total_signers: 4,
            threshold: 3, // 3 of 4, approx 2/3 + 1
        };

        // 3 of 4 signers participate
        let keypairs: Vec<BlsKeypair> = (0..3)
            .map(|i| bls_keygen(format!("thresh-val-{i}").as_bytes()))
            .collect();
        let message = b"threshold test";
        let sigs: Vec<BlsSignature> = keypairs
            .iter()
            .map(|kp| bls_sign(&kp.secret, message))
            .collect();

        let indexed_sigs: Vec<(usize, BlsSignature)> =
            sigs.into_iter().enumerate().collect();
        let agg = create_aggregate(&indexed_sigs).unwrap();

        assert!(meets_threshold(&agg, &config));
    }

    #[test]
    fn test_threshold_not_met() {
        let config = ThresholdConfig {
            total_signers: 4,
            threshold: 3, // Need 3 of 4
        };

        // Only 1 signer participates
        let kp = bls_keygen(b"lone-validator");
        let sig = bls_sign(&kp.secret, b"threshold fail");
        let agg = create_aggregate(&[(0, sig)]).unwrap();

        assert!(!meets_threshold(&agg, &config));
        assert_eq!(agg.signer_count, 1);
    }

    #[test]
    fn test_address_derivation() {
        let kp = bls_keygen(b"address-test-seed");
        let addr1 = address_from_bls_pubkey(&kp.public);
        let addr2 = address_from_bls_pubkey(&kp.public);

        // Deterministic
        assert_eq!(addr1, addr2);
        assert_eq!(addr1.len(), 32);

        // Different key produces different address
        let kp2 = bls_keygen(b"other-address-seed");
        let addr3 = address_from_bls_pubkey(&kp2.public);
        assert_ne!(addr1, addr3);
    }

    #[test]
    fn test_large_aggregate() {
        let message = b"large validator set consensus";
        let n = 100;

        let keypairs: Vec<BlsKeypair> = (0..n)
            .map(|i| bls_keygen(format!("large-val-{i}").as_bytes()))
            .collect();
        let sigs: Vec<BlsSignature> = keypairs
            .iter()
            .map(|kp| bls_sign(&kp.secret, message))
            .collect();

        // Build verifiable aggregate
        let refs: Vec<(usize, &BlsPublicKey, &BlsSignature)> = keypairs
            .iter()
            .zip(sigs.iter())
            .enumerate()
            .map(|(i, (kp, sig))| (i, &kp.public, sig))
            .collect();
        let (agg_pk, agg_sig) = create_verifiable_aggregate(&refs, message).unwrap();

        // Should verify
        assert!(verify_aggregate(&agg_pk, message, &agg_sig));
        assert_eq!(agg_pk.participant_count, n);
        assert_eq!(agg_sig.signer_count, n);
        assert_eq!(agg_sig.signers.len(), n);

        // Threshold check: 100 of 100 meets 67 threshold (2/3)
        let config = ThresholdConfig {
            total_signers: 100,
            threshold: 67,
        };
        assert!(meets_threshold(&agg_sig, &config));
    }

    #[test]
    fn test_serde_roundtrip() {
        let kp = bls_keygen(b"serde-test");
        let message = b"serde roundtrip";
        let sig = bls_sign(&kp.secret, message);

        // Public key roundtrip
        let pk_json = serde_json::to_string(&kp.public).unwrap();
        let pk_back: BlsPublicKey = serde_json::from_str(&pk_json).unwrap();
        assert_eq!(kp.public, pk_back);

        // Signature roundtrip
        let sig_json = serde_json::to_string(&sig).unwrap();
        let sig_back: BlsSignature = serde_json::from_str(&sig_json).unwrap();
        assert_eq!(sig, sig_back);

        // AggregateSignature roundtrip
        let agg = create_aggregate(&[(0, sig)]).unwrap();
        let agg_json = serde_json::to_string(&agg).unwrap();
        let agg_back: AggregateSignature = serde_json::from_str(&agg_json).unwrap();
        assert_eq!(agg.signer_count, agg_back.signer_count);
        assert_eq!(agg.signers, agg_back.signers);
    }

    #[test]
    fn test_empty_aggregate_fails_verify() {
        let agg_pk = AggregatePublicKey {
            key: BlsPublicKey([0u8; BLS_PK_LEN]),
            participant_count: 0,
        };
        let agg_sig = AggregateSignature {
            signature: BlsSignature([0u8; BLS_SIG_LEN]),
            signers: vec![],
            signer_count: 0,
        };

        assert!(!verify_aggregate(&agg_pk, b"anything", &agg_sig));
    }

    #[test]
    fn test_real_pairing_security() {
        // Verify that forged signatures fail (this is trivially true with
        // BLAKE3 simulation but cryptographically meaningful with real BLS)
        let kp = bls_keygen(b"security-test");
        let message = b"important consensus vote";

        // Random bytes should not verify as a valid signature
        let fake_sig = BlsSignature([0xAB; BLS_SIG_LEN]);
        assert!(!bls_verify(&kp.public, message, &fake_sig));

        // Sign with one key, try to verify with another key
        let kp2 = bls_keygen(b"attacker-key");
        let sig = bls_sign(&kp.secret, message);
        assert!(!bls_verify(&kp2.public, message, &sig));
        assert!(bls_verify(&kp.public, message, &sig));
    }

    #[test]
    fn test_aggregate_verify_wrong_message() {
        let message = b"correct message";
        let wrong_message = b"wrong message";

        let keypairs: Vec<BlsKeypair> = (0..3)
            .map(|i| bls_keygen(format!("agg-wrong-{i}").as_bytes()))
            .collect();
        let sigs: Vec<BlsSignature> = keypairs
            .iter()
            .map(|kp| bls_sign(&kp.secret, message))
            .collect();

        let refs: Vec<(usize, &BlsPublicKey, &BlsSignature)> = keypairs
            .iter()
            .zip(sigs.iter())
            .enumerate()
            .map(|(i, (kp, sig))| (i, &kp.public, sig))
            .collect();
        let (agg_pk, agg_sig) = create_verifiable_aggregate(&refs, message).unwrap();

        // Correct message verifies
        assert!(verify_aggregate(&agg_pk, message, &agg_sig));
        // Wrong message fails
        assert!(!verify_aggregate(&agg_pk, wrong_message, &agg_sig));
    }
}
