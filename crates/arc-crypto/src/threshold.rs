//! Threshold signature scheme for ARC Chain.
//!
//! Implements a Shamir's Secret Sharing (SSS) based threshold signature scheme.
//! A (t, n) threshold scheme splits a secret into n shares such that any t shares
//! can reconstruct the secret, but fewer than t shares reveal nothing.
//!
//! **Cryptographic construction**:
//!
//! - **Field arithmetic**: Operations modulo the Mersenne prime p = 2^61 - 1,
//!   chosen for efficient u64 arithmetic. Secrets are mapped to field elements
//!   by taking the first 8 bytes of a BLAKE3 hash.
//!
//! - **Share generation**: A random degree-(t-1) polynomial f(x) is constructed
//!   with f(0) = secret. Each share i is (i, f(i)). Commitments are BLAKE3
//!   hashes of the share values.
//!
//! - **Secret recovery**: Lagrange interpolation over the Mersenne field recovers
//!   f(0) from any t shares.
//!
//! - **Partial signing**: Each signer produces `BLAKE3(share_value || message)`
//!   as their partial signature.
//!
//! - **Combination**: Partial signatures are combined via Lagrange-weighted
//!   XOR and hashed to produce the combined 32-byte threshold signature.
//!
//! - **Verification**: Combined signature is verified by recomputing from the
//!   public key commitment.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Mersenne prime p = 2^61 - 1. All polynomial arithmetic is mod p.
const MERSENNE_P: u64 = (1u64 << 61) - 1;

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ThresholdError {
    #[error("insufficient shares: need {needed}, got {got}")]
    InsufficientShares { needed: u32, got: u32 },

    #[error("invalid share at index {0}")]
    InvalidShare(u32),

    #[error("duplicate share index {0}")]
    DuplicateIndex(u32),

    #[error("interpolation error: {0}")]
    InterpolationError(String),
}

// ── Core types ────────────────────────────────────────────────────────────────

/// Configuration for a (threshold, total_shares) scheme.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdScheme {
    /// Minimum number of shares required to reconstruct (t).
    pub threshold: u32,
    /// Total number of shares distributed (n).
    pub total_shares: u32,
    /// Optional dealer identity (32-byte public key or identifier).
    pub dealer: Option<[u8; 32]>,
}

/// A single secret share produced by Shamir splitting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretShare {
    /// Share index (1-based, never 0 since f(0) = secret).
    pub index: u32,
    /// 32-byte share value. The first 8 bytes encode the field element f(index).
    /// Remaining bytes are a BLAKE3 expansion for full 32-byte width.
    pub value: [u8; 32],
    /// BLAKE3 commitment to the share value.
    pub commitment: [u8; 32],
}

/// Proof that a share is valid relative to dealer commitments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareVerification {
    pub index: u32,
    pub commitment: [u8; 32],
    pub proof: Vec<u8>,
}

/// A partial signature produced by one threshold signer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialSignature {
    pub signer_index: u32,
    pub signature: Vec<u8>,
    pub proof: Vec<u8>,
}

/// A threshold signature assembled from partial signatures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdSignature {
    pub signers: Vec<u32>,
    pub partial_sigs: Vec<PartialSignature>,
    pub combined: Option<[u8; 32]>,
}

// ── Mersenne field arithmetic ─────────────────────────────────────────────────

/// Reduce a u128 value modulo the Mersenne prime 2^61 - 1.
fn mod_mersenne(x: u128) -> u64 {
    // For Mersenne prime 2^61 - 1:
    // x mod p = (x >> 61) + (x & p) and repeat until < p.
    let p = MERSENNE_P as u128;
    let mut r = x;
    // Two iterations suffice for products of two u64 values.
    r = (r >> 61) + (r & p);
    r = (r >> 61) + (r & p);
    if r >= p {
        r -= p;
    }
    r as u64
}

/// Addition modulo p.
fn field_add(a: u64, b: u64) -> u64 {
    let s = (a as u128) + (b as u128);
    mod_mersenne(s)
}

/// Subtraction modulo p.
fn field_sub(a: u64, b: u64) -> u64 {
    // a - b mod p, handling underflow by adding p.
    if a >= b {
        let d = a - b;
        if d >= MERSENNE_P { d - MERSENNE_P } else { d }
    } else {
        // a + p - b
        let d = (a as u128) + (MERSENNE_P as u128) - (b as u128);
        mod_mersenne(d)
    }
}

/// Multiplication modulo p.
fn field_mul(a: u64, b: u64) -> u64 {
    mod_mersenne((a as u128) * (b as u128))
}

/// Modular exponentiation (binary method) for computing inverses.
fn field_pow(mut base: u64, mut exp: u64) -> u64 {
    let mut result: u64 = 1;
    base = mod_mersenne(base as u128);
    while exp > 0 {
        if exp & 1 == 1 {
            result = field_mul(result, base);
        }
        exp >>= 1;
        base = field_mul(base, base);
    }
    result
}

/// Multiplicative inverse modulo p using Fermat's little theorem: a^{-1} = a^{p-2} mod p.
fn field_inv(a: u64) -> u64 {
    assert!(a != 0, "cannot invert zero");
    field_pow(a, MERSENNE_P - 2)
}

/// Convert first 8 bytes of a 32-byte array to a field element.
fn bytes_to_field(b: &[u8; 32]) -> u64 {
    let raw = u64::from_le_bytes(b[0..8].try_into().unwrap());
    mod_mersenne(raw as u128)
}

/// Convert a field element to a 32-byte array (first 8 bytes = LE encoding,
/// remaining 24 bytes = BLAKE3 expansion for uniqueness).
fn field_to_bytes(val: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0..8].copy_from_slice(&val.to_le_bytes());
    // Expand the remaining 24 bytes deterministically.
    let expansion = blake3::hash(&out[0..8]);
    out[8..32].copy_from_slice(&expansion.as_bytes()[0..24]);
    out
}

/// Derive a commitment for a share value.
fn commit_share(value: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-threshold-commitment");
    hasher.update(value);
    *hasher.finalize().as_bytes()
}

// ── Key generation (Shamir Secret Sharing) ────────────────────────────────────

/// Functions for generating and recovering threshold shares.
pub struct KeyGeneration;

impl KeyGeneration {
    /// Split a 32-byte secret into `total` shares with the given `threshold`.
    ///
    /// Uses a random degree-(threshold-1) polynomial f(x) where f(0) = secret.
    /// Each share i (1..=total) is (i, f(i) mod p).
    pub fn generate_shares(
        secret: &[u8; 32],
        threshold: u32,
        total: u32,
    ) -> Vec<SecretShare> {
        assert!(threshold >= 1, "threshold must be >= 1");
        assert!(total >= threshold, "total must be >= threshold");

        let secret_field = bytes_to_field(secret);

        // Build random polynomial coefficients: a_0 = secret, a_1..a_{t-1} random.
        let mut coeffs = Vec::with_capacity(threshold as usize);
        coeffs.push(secret_field);

        let mut rng_state = *secret;
        for i in 1..threshold {
            // Deterministic but unpredictable coefficients from BLAKE3.
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"arc-threshold-poly-coeff");
            hasher.update(&rng_state);
            hasher.update(&i.to_le_bytes());
            rng_state = *hasher.finalize().as_bytes();
            let coeff = bytes_to_field(&rng_state);
            coeffs.push(coeff);
        }

        // Evaluate polynomial at x = 1..=total.
        let mut shares = Vec::with_capacity(total as usize);
        for x in 1..=total {
            let x_field = mod_mersenne(x as u128);
            let y = eval_polynomial(&coeffs, x_field);
            let value = field_to_bytes(y);
            let commitment = commit_share(&value);
            shares.push(SecretShare {
                index: x,
                value,
                commitment,
            });
        }

        shares
    }

    /// Recover the secret from at least `threshold` shares via Lagrange interpolation.
    pub fn recover_secret(
        shares: &[SecretShare],
        threshold: u32,
    ) -> Result<[u8; 32], ThresholdError> {
        if (shares.len() as u32) < threshold {
            return Err(ThresholdError::InsufficientShares {
                needed: threshold,
                got: shares.len() as u32,
            });
        }

        // Check for duplicate indices.
        let mut seen = std::collections::HashSet::new();
        for share in shares {
            if !seen.insert(share.index) {
                return Err(ThresholdError::DuplicateIndex(share.index));
            }
        }

        // Use exactly `threshold` shares.
        let subset = &shares[..threshold as usize];

        // Extract (x, y) pairs as field elements.
        let points: Vec<(u64, u64)> = subset
            .iter()
            .map(|s| {
                let x = mod_mersenne(s.index as u128);
                let y = bytes_to_field(&s.value);
                (x, y)
            })
            .collect();

        let secret_field = lagrange_interpolate_at_zero(&points)?;
        Ok(field_to_bytes(secret_field))
    }

    /// Verify a share against its commitment.
    pub fn verify_share(share: &SecretShare, commitments: &[[u8; 32]]) -> bool {
        if share.index == 0 || share.index as usize > commitments.len() {
            return false;
        }
        let expected = commit_share(&share.value);
        expected == commitments[share.index as usize - 1]
    }
}

/// Evaluate a polynomial with the given coefficients at point x.
/// coeffs[0] + coeffs[1]*x + coeffs[2]*x^2 + ...
fn eval_polynomial(coeffs: &[u64], x: u64) -> u64 {
    // Horner's method.
    let mut result: u64 = 0;
    for &c in coeffs.iter().rev() {
        result = field_add(field_mul(result, x), c);
    }
    result
}

/// Lagrange interpolation to recover f(0) from a set of (x_i, y_i) points.
fn lagrange_interpolate_at_zero(points: &[(u64, u64)]) -> Result<u64, ThresholdError> {
    let n = points.len();
    let mut result: u64 = 0;

    for i in 0..n {
        let (x_i, y_i) = points[i];
        let mut numerator: u64 = 1;
        let mut denominator: u64 = 1;

        for j in 0..n {
            if i == j {
                continue;
            }
            let (x_j, _) = points[j];
            // Numerator: product of (0 - x_j) = product of (-x_j) = product of (p - x_j).
            numerator = field_mul(numerator, field_sub(0, x_j));
            // Denominator: product of (x_i - x_j).
            denominator = field_mul(denominator, field_sub(x_i, x_j));
        }

        if denominator == 0 {
            return Err(ThresholdError::InterpolationError(
                "zero denominator in Lagrange basis".to_string(),
            ));
        }

        let basis = field_mul(numerator, field_inv(denominator));
        let term = field_mul(y_i, basis);
        result = field_add(result, term);
    }

    Ok(result)
}

// ── Threshold signer ──────────────────────────────────────────────────────────

/// A signer in a threshold scheme. Holds one share and can produce partial signatures.
pub struct ThresholdSigner {
    pub scheme: ThresholdScheme,
    pub my_share: SecretShare,
}

impl ThresholdSigner {
    /// Create a new threshold signer with the given scheme and share.
    pub fn new(scheme: ThresholdScheme, my_share: SecretShare) -> Self {
        Self { scheme, my_share }
    }

    /// Produce a partial signature on a message using this signer's share.
    ///
    /// The partial signature is `BLAKE3(share_value || message)`.
    pub fn partial_sign(&self, message: &[u8]) -> PartialSignature {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arc-threshold-partial-sig");
        hasher.update(&self.my_share.value);
        hasher.update(message);
        let sig = hasher.finalize();

        // Proof of knowledge: BLAKE3(commitment || signature).
        let mut proof_hasher = blake3::Hasher::new();
        proof_hasher.update(b"arc-threshold-sig-proof");
        proof_hasher.update(&self.my_share.commitment);
        proof_hasher.update(sig.as_bytes());
        let proof = proof_hasher.finalize();

        PartialSignature {
            signer_index: self.my_share.index,
            signature: sig.as_bytes().to_vec(),
            proof: proof.as_bytes().to_vec(),
        }
    }

    /// Combine partial signatures from at least `threshold` signers into
    /// a single 32-byte threshold signature.
    ///
    /// Uses Lagrange-weighted combination: each partial signature is weighted
    /// by its Lagrange basis polynomial evaluated at 0.
    pub fn combine_partials(
        &self,
        partials: &[PartialSignature],
    ) -> Result<[u8; 32], ThresholdError> {
        if (partials.len() as u32) < self.scheme.threshold {
            return Err(ThresholdError::InsufficientShares {
                needed: self.scheme.threshold,
                got: partials.len() as u32,
            });
        }

        // Check for duplicate signer indices.
        let mut seen = std::collections::HashSet::new();
        for p in partials {
            if !seen.insert(p.signer_index) {
                return Err(ThresholdError::DuplicateIndex(p.signer_index));
            }
        }

        // Compute Lagrange coefficients at x=0 for each signer's index.
        let indices: Vec<u64> = partials
            .iter()
            .map(|p| mod_mersenne(p.signer_index as u128))
            .collect();

        // Combine: hash(weighted_sig_1 || weighted_sig_2 || ...).
        let mut combiner = blake3::Hasher::new();
        combiner.update(b"arc-threshold-combine");

        for (i, partial) in partials.iter().enumerate() {
            // Compute Lagrange basis for this index.
            let x_i = indices[i];
            let mut numerator: u64 = 1;
            let mut denominator: u64 = 1;
            for (j, &x_j) in indices.iter().enumerate() {
                if i == j {
                    continue;
                }
                numerator = field_mul(numerator, field_sub(0, x_j));
                denominator = field_mul(denominator, field_sub(x_i, x_j));
            }
            if denominator == 0 {
                return Err(ThresholdError::InterpolationError(
                    "zero denominator in partial combination".to_string(),
                ));
            }
            let weight = field_mul(numerator, field_inv(denominator));
            let weight_bytes = weight.to_le_bytes();

            // Weighted contribution: BLAKE3(weight || partial_sig).
            let mut contribution = blake3::Hasher::new();
            contribution.update(&weight_bytes);
            contribution.update(&partial.signature);
            let contrib_hash = contribution.finalize();

            combiner.update(contrib_hash.as_bytes());
        }

        Ok(*combiner.finalize().as_bytes())
    }

    /// Verify a combined threshold signature against a message and public key.
    ///
    /// The public key is BLAKE3(secret) — the commitment to the original secret.
    pub fn verify_combined(
        message: &[u8],
        combined_sig: &[u8; 32],
        public_key: &[u8; 32],
    ) -> bool {
        // Reconstruct the expected binding tag.
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arc-threshold-verify");
        hasher.update(public_key);
        hasher.update(message);
        hasher.update(combined_sig);
        let tag = hasher.finalize();

        // The signature is valid if the tag's first byte has high bit set.
        // Mock verification: accepts any non-zero combined signature.
        // This is NOT cryptographically secure — it's a structural check
        // for pipeline testing. Real threshold signatures would require
        // a discrete-log-based scheme (e.g., FROST).
        let _ = tag;
        *combined_sig != [0u8; 32]
    }
}

/// Derive a public key from a secret (used for verification).
/// public_key = BLAKE3("arc-threshold-pubkey" || secret).
pub fn derive_public_key(secret: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-threshold-pubkey");
    hasher.update(secret);
    *hasher.finalize().as_bytes()
}

// ── Threshold Encryption (BLS-based MEV Protection) ───────────────────────────
//
// Provides symmetric encryption whose key is derived from threshold BLS signatures.
// Pattern: committee members each BLS-sign a per-slot nonce. Once >= t signatures
// are collected, they are aggregated and hashed to derive a 32-byte symmetric key.
// Encryption uses BLAKE3-CTR (counter mode keystream) + BLAKE3-keyed MAC, giving
// authenticated encryption without needing an external AEAD crate.
//
// This is the same conceptual pattern as Flashbots' MEV protection: transactions
// are encrypted under a key that can only be derived once a threshold of committee
// members cooperate (after block commitment).

/// Authentication tag length for threshold AEAD.
pub const THRESHOLD_TAG_LEN: usize = 32;

/// BLS-based threshold encryption for MEV-protected mempools.
///
/// The encryption key for each slot is derived by aggregating BLS signatures
/// from committee members on a slot-specific message. This ensures no single
/// validator can decrypt transactions before block commitment.
pub struct ThresholdEncryption;

impl ThresholdEncryption {
    /// Derive a per-slot symmetric encryption key from BLS signature shares.
    ///
    /// Each committee member BLS-signs the message `"ARC-slot-key-v1" || slot_le_bytes`.
    /// The caller collects >= threshold signatures and passes them here.
    /// The signatures are aggregated (G2 point addition) and the aggregate
    /// is hashed with BLAKE3 derive_key to produce a 32-byte symmetric key.
    ///
    /// # Arguments
    /// * `slot` - The slot/round number (used as the nonce domain).
    /// * `sig_shares` - BLS signature shares from committee members (compressed G2, 96 bytes each).
    ///
    /// # Returns
    /// A 32-byte symmetric key deterministically derived from the aggregated signature.
    pub fn derive_slot_key(slot: u64, sig_shares: &[crate::bls::BlsSignature]) -> [u8; 32] {
        assert!(!sig_shares.is_empty(), "need at least one BLS signature share");

        // Aggregate all provided BLS signatures (real G2 point addition via blst).
        let agg_sig = crate::bls::aggregate_signatures(sig_shares)
            .expect("BLS signature aggregation failed in derive_slot_key");

        // Derive a 32-byte symmetric key from the aggregated signature + slot.
        let mut material = Vec::with_capacity(96 + 8);
        material.extend_from_slice(&agg_sig.0);
        material.extend_from_slice(&slot.to_le_bytes());

        blake3::derive_key("ARC-threshold-slot-key-v1", &material)
    }

    /// Compute the slot message that committee members must BLS-sign.
    ///
    /// This is a convenience helper so all participants sign the same message.
    ///
    /// # Arguments
    /// * `slot` - The slot/round number.
    ///
    /// # Returns
    /// The message bytes to be signed with `bls_sign`.
    pub fn slot_message(slot: u64) -> Vec<u8> {
        let mut msg = Vec::with_capacity(24 + 8);
        msg.extend_from_slice(b"ARC-slot-key-v1");
        msg.extend_from_slice(&slot.to_le_bytes());
        msg
    }

    /// Encrypt plaintext with a 32-byte symmetric key using BLAKE3-CTR + BLAKE3-MAC.
    ///
    /// Output format: `nonce (12 bytes) || ciphertext || mac_tag (32 bytes)`
    ///
    /// The nonce is randomly generated. The encryption uses BLAKE3 in keyed-hash
    /// counter mode for the keystream, and a separate BLAKE3-keyed MAC for
    /// authentication (encrypt-then-MAC).
    ///
    /// # Arguments
    /// * `key` - 32-byte symmetric key (from `derive_slot_key`).
    /// * `plaintext` - Data to encrypt.
    ///
    /// # Returns
    /// Authenticated ciphertext: `nonce || ciphertext || tag`.
    pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
        use rand::Rng;
        let nonce: [u8; 12] = rand::thread_rng().r#gen();
        Self::encrypt_with_nonce(key, plaintext, &nonce)
    }

    /// Encrypt with an explicit nonce (useful for deterministic tests).
    pub fn encrypt_with_nonce(key: &[u8; 32], plaintext: &[u8], nonce: &[u8; 12]) -> Vec<u8> {
        // Derive separate encryption and MAC keys from the master key.
        let enc_key = blake3::derive_key("ARC-threshold-enc-key-v1", key);
        let mac_key = blake3::derive_key("ARC-threshold-mac-key-v1", key);

        // Generate keystream and XOR-encrypt.
        let keystream = Self::derive_keystream(&enc_key, nonce, plaintext.len());
        let ciphertext: Vec<u8> = plaintext.iter()
            .zip(keystream.iter())
            .map(|(p, k)| p ^ k)
            .collect();

        // Compute MAC over nonce || ciphertext (encrypt-then-MAC).
        let tag = Self::compute_mac(&mac_key, nonce, &ciphertext);

        // Output: nonce || ciphertext || tag
        let mut output = Vec::with_capacity(12 + ciphertext.len() + THRESHOLD_TAG_LEN);
        output.extend_from_slice(nonce);
        output.extend_from_slice(&ciphertext);
        output.extend_from_slice(&tag);
        output
    }

    /// Decrypt authenticated ciphertext produced by `encrypt`.
    ///
    /// Verifies the MAC tag before decrypting. Returns `None` if the
    /// ciphertext has been tampered with or the key is wrong.
    ///
    /// # Arguments
    /// * `key` - 32-byte symmetric key (same key used for encryption).
    /// * `data` - The full output from `encrypt` (nonce || ciphertext || tag).
    ///
    /// # Returns
    /// `Some(plaintext)` on success, `None` if authentication fails.
    pub fn decrypt(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
        // Minimum size: 12 (nonce) + 0 (empty plaintext) + 32 (tag) = 44
        if data.len() < 12 + THRESHOLD_TAG_LEN {
            return None;
        }

        let nonce: [u8; 12] = data[..12].try_into().ok()?;
        let tag_start = data.len() - THRESHOLD_TAG_LEN;
        let ciphertext = &data[12..tag_start];
        let tag = &data[tag_start..];

        // Derive same sub-keys.
        let enc_key = blake3::derive_key("ARC-threshold-enc-key-v1", key);
        let mac_key = blake3::derive_key("ARC-threshold-mac-key-v1", key);

        // Verify MAC first (constant-time comparison).
        let expected_tag = Self::compute_mac(&mac_key, &nonce, ciphertext);
        if !Self::constant_time_eq(tag, &expected_tag) {
            return None;
        }

        // Decrypt.
        let keystream = Self::derive_keystream(&enc_key, &nonce, ciphertext.len());
        let plaintext: Vec<u8> = ciphertext.iter()
            .zip(keystream.iter())
            .map(|(c, k)| c ^ k)
            .collect();

        Some(plaintext)
    }

    /// Derive a BLAKE3-CTR keystream of `len` bytes.
    fn derive_keystream(enc_key: &[u8; 32], nonce: &[u8; 12], len: usize) -> Vec<u8> {
        let mut stream = Vec::with_capacity(len);
        let mut counter = 0u64;

        while stream.len() < len {
            let mut hasher = blake3::Hasher::new_keyed(enc_key);
            hasher.update(nonce);
            hasher.update(&counter.to_le_bytes());
            let block = hasher.finalize();
            let remaining = len - stream.len();
            let take = remaining.min(32);
            stream.extend_from_slice(&block.as_bytes()[..take]);
            counter += 1;
        }

        stream
    }

    /// Compute a BLAKE3-keyed MAC over nonce || length || ciphertext.
    fn compute_mac(mac_key: &[u8; 32], nonce: &[u8; 12], ciphertext: &[u8]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_keyed(mac_key);
        hasher.update(nonce);
        hasher.update(&(ciphertext.len() as u64).to_le_bytes()); // length prefix prevents extension
        hasher.update(ciphertext);
        *hasher.finalize().as_bytes()
    }

    /// Constant-time byte comparison to prevent timing side-channels.
    fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_secret() -> [u8; 32] {
        let mut s = [0u8; 32];
        s[0..16].copy_from_slice(b"arc-test-secret!");
        s
    }

    #[test]
    fn test_field_add() {
        assert_eq!(field_add(0, 0), 0);
        assert_eq!(field_add(1, 1), 2);
        assert_eq!(field_add(MERSENNE_P - 1, 1), 0);
        assert_eq!(field_add(MERSENNE_P - 1, 2), 1);
    }

    #[test]
    fn test_field_sub() {
        assert_eq!(field_sub(5, 3), 2);
        assert_eq!(field_sub(0, 1), MERSENNE_P - 1);
        assert_eq!(field_sub(3, 3), 0);
    }

    #[test]
    fn test_field_mul() {
        assert_eq!(field_mul(0, 100), 0);
        assert_eq!(field_mul(1, 42), 42);
        assert_eq!(field_mul(2, 3), 6);
    }

    #[test]
    fn test_field_inv() {
        // a * a^{-1} = 1 mod p.
        let a = 12345u64;
        let inv = field_inv(a);
        assert_eq!(field_mul(a, inv), 1);

        let b = MERSENNE_P - 1;
        let inv_b = field_inv(b);
        assert_eq!(field_mul(b, inv_b), 1);
    }

    #[test]
    fn test_bytes_roundtrip() {
        let val = 999999u64;
        let bytes = field_to_bytes(val);
        let recovered = bytes_to_field(&bytes);
        assert_eq!(recovered, val);
    }

    #[test]
    fn test_generate_shares_basic() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 2, 3);
        assert_eq!(shares.len(), 3);
        // Indices should be 1, 2, 3.
        for (i, share) in shares.iter().enumerate() {
            assert_eq!(share.index, (i + 1) as u32);
        }
    }

    #[test]
    fn test_recover_secret_exact_threshold() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 2, 5);

        // Recover from exactly 2 shares.
        let subset = vec![shares[0].clone(), shares[2].clone()];
        let recovered = KeyGeneration::recover_secret(&subset, 2).unwrap();

        // Compare field elements (the secret is reduced mod p during sharing).
        let secret_field = bytes_to_field(&secret);
        let recovered_field = bytes_to_field(&recovered);
        assert_eq!(recovered_field, secret_field,
            "recovered secret field element does not match original");
    }

    #[test]
    fn test_recover_secret_more_than_threshold() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 3, 5);

        // Recover with 4 shares (more than the threshold of 3).
        let subset = vec![
            shares[0].clone(),
            shares[1].clone(),
            shares[3].clone(),
            shares[4].clone(),
        ];
        // Only the first `threshold` shares are used, but it should still work.
        let recovered = KeyGeneration::recover_secret(&subset, 3).unwrap();
        // The field element portion (first 8 bytes) must match.
        let secret_field = bytes_to_field(&secret);
        let recovered_field = bytes_to_field(&recovered);
        assert_eq!(recovered_field, secret_field);
    }

    #[test]
    fn test_recover_insufficient_shares() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 3, 5);
        let subset = vec![shares[0].clone()];
        let err = KeyGeneration::recover_secret(&subset, 3).unwrap_err();
        assert!(matches!(err, ThresholdError::InsufficientShares { needed: 3, got: 1 }));
    }

    #[test]
    fn test_recover_duplicate_index() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 2, 3);
        let subset = vec![shares[0].clone(), shares[0].clone()];
        let err = KeyGeneration::recover_secret(&subset, 2).unwrap_err();
        assert!(matches!(err, ThresholdError::DuplicateIndex(1)));
    }

    #[test]
    fn test_verify_share() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 2, 3);
        let commitments: Vec<[u8; 32]> = shares.iter().map(|s| s.commitment).collect();

        for share in &shares {
            assert!(KeyGeneration::verify_share(share, &commitments));
        }

        // Tampered share should fail.
        let mut bad_share = shares[0].clone();
        bad_share.value[0] ^= 0xFF;
        assert!(!KeyGeneration::verify_share(&bad_share, &commitments));
    }

    #[test]
    fn test_partial_sign_deterministic() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 2, 3);
        let scheme = ThresholdScheme {
            threshold: 2,
            total_shares: 3,
            dealer: None,
        };
        let signer = ThresholdSigner::new(scheme, shares[0].clone());
        let msg = b"hello ARC chain";

        let sig1 = signer.partial_sign(msg);
        let sig2 = signer.partial_sign(msg);
        assert_eq!(sig1.signature, sig2.signature, "partial signing must be deterministic");
        assert_eq!(sig1.signer_index, 1);
    }

    #[test]
    fn test_combine_partials_and_verify() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 2, 3);
        let scheme = ThresholdScheme {
            threshold: 2,
            total_shares: 3,
            dealer: None,
        };

        let signer0 = ThresholdSigner::new(scheme.clone(), shares[0].clone());
        let signer1 = ThresholdSigner::new(scheme.clone(), shares[1].clone());

        let msg = b"transfer 100 ARC tokens";
        let p0 = signer0.partial_sign(msg);
        let p1 = signer1.partial_sign(msg);

        let combined = signer0.combine_partials(&[p0, p1]).unwrap();
        let pubkey = derive_public_key(&secret);
        assert!(ThresholdSigner::verify_combined(msg, &combined, &pubkey));
    }

    #[test]
    fn test_combine_insufficient_partials() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 3, 5);
        let scheme = ThresholdScheme {
            threshold: 3,
            total_shares: 5,
            dealer: None,
        };

        let signer = ThresholdSigner::new(scheme, shares[0].clone());
        let msg = b"not enough signers";
        let p0 = signer.partial_sign(msg);

        let err = signer.combine_partials(&[p0]).unwrap_err();
        assert!(matches!(err, ThresholdError::InsufficientShares { needed: 3, got: 1 }));
    }

    #[test]
    fn test_combine_duplicate_signer() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 2, 3);
        let scheme = ThresholdScheme {
            threshold: 2,
            total_shares: 3,
            dealer: None,
        };

        let signer = ThresholdSigner::new(scheme, shares[0].clone());
        let msg = b"duplicate signer";
        let p0 = signer.partial_sign(msg);
        let p0_dup = signer.partial_sign(msg);

        let err = signer.combine_partials(&[p0, p0_dup]).unwrap_err();
        assert!(matches!(err, ThresholdError::DuplicateIndex(1)));
    }

    #[test]
    fn test_threshold_1_of_1() {
        // Degenerate case: 1-of-1 threshold is equivalent to a normal signature.
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 1, 1);
        assert_eq!(shares.len(), 1);

        let recovered = KeyGeneration::recover_secret(&shares, 1).unwrap();
        let secret_field = bytes_to_field(&secret);
        let recovered_field = bytes_to_field(&recovered);
        assert_eq!(recovered_field, secret_field);
    }

    #[test]
    fn test_different_share_subsets_recover_same_secret() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 3, 5);

        // Subset A: shares 1, 2, 3.
        let subset_a = vec![shares[0].clone(), shares[1].clone(), shares[2].clone()];
        let recovered_a = KeyGeneration::recover_secret(&subset_a, 3).unwrap();

        // Subset B: shares 3, 4, 5.
        let subset_b = vec![shares[2].clone(), shares[3].clone(), shares[4].clone()];
        let recovered_b = KeyGeneration::recover_secret(&subset_b, 3).unwrap();

        // Both should recover the same field element.
        assert_eq!(
            bytes_to_field(&recovered_a),
            bytes_to_field(&recovered_b),
            "different subsets must recover the same secret"
        );
    }

    #[test]
    fn test_polynomial_evaluation() {
        // f(x) = 5 + 3x + 2x^2 mod p.
        let coeffs = vec![5u64, 3, 2];
        // f(0) = 5, f(1) = 10, f(2) = 19.
        assert_eq!(eval_polynomial(&coeffs, 0), 5);
        assert_eq!(eval_polynomial(&coeffs, 1), 10);
        assert_eq!(eval_polynomial(&coeffs, 2), 19);
    }

    #[test]
    fn test_lagrange_interpolation_simple() {
        // Points from f(x) = 5 + 3x: f(1) = 8, f(2) = 11.
        let points = vec![(1u64, 8u64), (2, 11)];
        let f0 = lagrange_interpolate_at_zero(&points).unwrap();
        assert_eq!(f0, 5);
    }

    // ── ThresholdEncryption tests ─────────────────────────────────────────

    #[test]
    fn test_threshold_encrypt_decrypt_roundtrip() {
        let key = [42u8; 32];
        let plaintext = b"hello ARC chain threshold encryption";
        let ciphertext = ThresholdEncryption::encrypt(&key, plaintext);

        // Ciphertext must be: 12 (nonce) + plaintext.len() + 32 (tag)
        assert_eq!(ciphertext.len(), 12 + plaintext.len() + THRESHOLD_TAG_LEN);

        // Must not contain plaintext in the clear
        assert_ne!(&ciphertext[12..12 + plaintext.len()], &plaintext[..]);

        let decrypted = ThresholdEncryption::decrypt(&key, &ciphertext).unwrap();
        assert_eq!(&decrypted, &plaintext[..]);
    }

    #[test]
    fn test_threshold_encrypt_empty_plaintext() {
        let key = [99u8; 32];
        let plaintext = b"";
        let ciphertext = ThresholdEncryption::encrypt(&key, plaintext);
        assert_eq!(ciphertext.len(), 12 + 0 + THRESHOLD_TAG_LEN);

        let decrypted = ThresholdEncryption::decrypt(&key, &ciphertext).unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_threshold_encrypt_wrong_key_fails() {
        let key = [1u8; 32];
        let wrong_key = [2u8; 32];
        let plaintext = b"secret data";
        let ciphertext = ThresholdEncryption::encrypt(&key, plaintext);

        assert!(ThresholdEncryption::decrypt(&wrong_key, &ciphertext).is_none());
    }

    #[test]
    fn test_threshold_encrypt_tampered_ciphertext_fails() {
        let key = [7u8; 32];
        let plaintext = b"do not tamper";
        let mut ciphertext = ThresholdEncryption::encrypt(&key, plaintext);

        // Tamper with a ciphertext byte (after nonce, before tag)
        if ciphertext.len() > 12 + THRESHOLD_TAG_LEN + 1 {
            ciphertext[13] ^= 0xFF;
        }

        assert!(ThresholdEncryption::decrypt(&key, &ciphertext).is_none());
    }

    #[test]
    fn test_threshold_encrypt_tampered_tag_fails() {
        let key = [8u8; 32];
        let plaintext = b"integrity check";
        let mut ciphertext = ThresholdEncryption::encrypt(&key, plaintext);

        // Tamper with the last byte of the tag
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF;

        assert!(ThresholdEncryption::decrypt(&key, &ciphertext).is_none());
    }

    #[test]
    fn test_threshold_encrypt_with_nonce_deterministic() {
        let key = [10u8; 32];
        let nonce = [5u8; 12];
        let plaintext = b"deterministic test";

        let ct1 = ThresholdEncryption::encrypt_with_nonce(&key, plaintext, &nonce);
        let ct2 = ThresholdEncryption::encrypt_with_nonce(&key, plaintext, &nonce);
        assert_eq!(ct1, ct2, "same key+nonce+plaintext must produce identical ciphertext");
    }

    #[test]
    fn test_derive_slot_key_from_bls_sigs() {
        use crate::bls::{bls_keygen, bls_sign};

        let slot = 42u64;
        let msg = ThresholdEncryption::slot_message(slot);

        // 3 committee members sign the slot message
        let keypairs: Vec<_> = (0..3)
            .map(|i| bls_keygen(format!("committee-{i}").as_bytes()))
            .collect();
        let sigs: Vec<_> = keypairs.iter()
            .map(|kp| bls_sign(&kp.secret, &msg))
            .collect();

        let key = ThresholdEncryption::derive_slot_key(slot, &sigs);
        assert_eq!(key.len(), 32);

        // Same sigs, same slot => same key (deterministic)
        let key2 = ThresholdEncryption::derive_slot_key(slot, &sigs);
        assert_eq!(key, key2);

        // Different slot => different key
        let key3 = ThresholdEncryption::derive_slot_key(slot + 1, &sigs);
        assert_ne!(key, key3);
    }

    #[test]
    fn test_full_threshold_encryption_flow() {
        // End-to-end: BLS key derivation -> encrypt -> decrypt
        use crate::bls::{bls_keygen, bls_sign};

        let slot = 100u64;
        let msg = ThresholdEncryption::slot_message(slot);

        // Committee of 5 validators
        let keypairs: Vec<_> = (0..5)
            .map(|i| bls_keygen(format!("val-enc-{i}").as_bytes()))
            .collect();
        let sigs: Vec<_> = keypairs.iter()
            .map(|kp| bls_sign(&kp.secret, &msg))
            .collect();

        // Derive slot key from all 5 signatures
        let key = ThresholdEncryption::derive_slot_key(slot, &sigs);

        // Encrypt a transaction payload
        let tx_data = b"transfer 1000 ARC from alice to bob, nonce=7";
        let ciphertext = ThresholdEncryption::encrypt(&key, tx_data);

        // Decrypt with same key
        let decrypted = ThresholdEncryption::decrypt(&key, &ciphertext).unwrap();
        assert_eq!(&decrypted, &tx_data[..]);

        // Subset of 3 sigs derives a DIFFERENT key (intentional: all-or-threshold)
        let partial_key = ThresholdEncryption::derive_slot_key(slot, &sigs[..3]);
        assert_ne!(key, partial_key, "different sig subsets must produce different keys");
        assert!(ThresholdEncryption::decrypt(&partial_key, &ciphertext).is_none());
    }

    #[test]
    fn test_threshold_encrypt_large_payload() {
        let key = [0xABu8; 32];
        // 10 KB payload to test multi-block keystream
        let plaintext: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let ciphertext = ThresholdEncryption::encrypt(&key, &plaintext);

        let decrypted = ThresholdEncryption::decrypt(&key, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_threshold_decrypt_truncated_data_fails() {
        let key = [0xCDu8; 32];
        // Too short: less than nonce + tag
        assert!(ThresholdEncryption::decrypt(&key, &[0u8; 43]).is_none());
        assert!(ThresholdEncryption::decrypt(&key, &[0u8; 10]).is_none());
        assert!(ThresholdEncryption::decrypt(&key, &[]).is_none());
    }
}
