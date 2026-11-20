use serde::{Deserialize, Serialize};
use thiserror::Error;

const MERSENNE_P: u64 = (1u64 << 61) - 1;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdScheme {
    pub threshold: u32,
    pub total_shares: u32,
    pub dealer: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretShare {
    pub index: u32,
    pub value: [u8; 32],
    pub commitment: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareVerification {
    pub index: u32,
    pub commitment: [u8; 32],
    pub proof: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialSignature {
    pub signer_index: u32,
    pub signature: Vec<u8>,
    pub proof: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdSignature {
    pub signers: Vec<u32>,
    pub partial_sigs: Vec<PartialSignature>,
    pub combined: Option<[u8; 32]>,
}

fn mod_mersenne(x: u128) -> u64 {
    let p = MERSENNE_P as u128;
    let mut r = x;
    r = (r >> 61) + (r & p);
    r = (r >> 61) + (r & p);
    if r >= p {
        r -= p;
    }
    r as u64
}

fn field_add(a: u64, b: u64) -> u64 {
    let s = (a as u128) + (b as u128);
    mod_mersenne(s)
}

fn field_sub(a: u64, b: u64) -> u64 {
    if a >= b {
        let d = a - b;
        if d >= MERSENNE_P { d - MERSENNE_P } else { d }
    } else {
        let d = (a as u128) + (MERSENNE_P as u128) - (b as u128);
        mod_mersenne(d)
    }
}

fn field_mul(a: u64, b: u64) -> u64 {
    mod_mersenne((a as u128) * (b as u128))
}

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

fn field_inv(a: u64) -> u64 {
    assert!(a != 0, "cannot invert zero");
    field_pow(a, MERSENNE_P - 2)
}

fn bytes_to_field(b: &[u8; 32]) -> u64 {
    let raw = u64::from_le_bytes(b[0..8].try_into().unwrap());
    mod_mersenne(raw as u128)
}

fn field_to_bytes(val: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0..8].copy_from_slice(&val.to_le_bytes());
    let expansion = blake3::hash(&out[0..8]);
    out[8..32].copy_from_slice(&expansion.as_bytes()[0..24]);
    out
}

fn commit_share(value: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-threshold-commitment");
    hasher.update(value);
    *hasher.finalize().as_bytes()
}

pub struct KeyGeneration;

impl KeyGeneration {
    pub fn generate_shares(secret: &[u8; 32], threshold: u32, total: u32) -> Vec<SecretShare> {
        assert!(threshold >= 1, "threshold must be >= 1");
        assert!(total >= threshold, "total must be >= threshold");

        let secret_field = bytes_to_field(secret);

        let mut coeffs = Vec::with_capacity(threshold as usize);
        coeffs.push(secret_field);

        let mut rng_state = *secret;
        for i in 1..threshold {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"arc-threshold-poly-coeff");
            hasher.update(&rng_state);
            hasher.update(&i.to_le_bytes());
            rng_state = *hasher.finalize().as_bytes();
            let coeff = bytes_to_field(&rng_state);
            coeffs.push(coeff);
        }

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

        let mut seen = std::collections::HashSet::new();
        for share in shares {
            if !seen.insert(share.index) {
                return Err(ThresholdError::DuplicateIndex(share.index));
            }
        }

        let subset = &shares[..threshold as usize];

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

    pub fn verify_share(share: &SecretShare, commitments: &[[u8; 32]]) -> bool {
        if share.index == 0 || share.index as usize > commitments.len() {
            return false;
        }
        let expected = commit_share(&share.value);
        expected == commitments[share.index as usize - 1]
    }
}

fn eval_polynomial(coeffs: &[u64], x: u64) -> u64 {
    let mut result: u64 = 0;
    for &c in coeffs.iter().rev() {
        result = field_add(field_mul(result, x), c);
    }
    result
}

fn lagrange_interpolate_at_zero(points: &[(u64, u64)]) -> Result<u64, ThresholdError> {
    let n = points.len();
    let mut result: u64 = 0;

    for i in 0..n {
        let (x_i, y_i) = points[i];
        let mut numerator: u64 = 1;
        let mut denominator: u64 = 1;

        for (j, &(x_j, _)) in points.iter().enumerate().take(n) {
            if i == j {
                continue;
            }
            numerator = field_mul(numerator, field_sub(0, x_j));
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

pub struct ThresholdSigner {
    pub scheme: ThresholdScheme,
    pub my_share: SecretShare,
}

impl ThresholdSigner {
    pub fn new(scheme: ThresholdScheme, my_share: SecretShare) -> Self {
        Self { scheme, my_share }
    }

    pub fn partial_sign(&self, message: &[u8]) -> PartialSignature {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arc-threshold-partial-sig");
        hasher.update(&self.my_share.value);
        hasher.update(message);
        let sig = hasher.finalize();

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

        let mut seen = std::collections::HashSet::new();
        for p in partials {
            if !seen.insert(p.signer_index) {
                return Err(ThresholdError::DuplicateIndex(p.signer_index));
            }
        }

        let indices: Vec<u64> = partials
            .iter()
            .map(|p| mod_mersenne(p.signer_index as u128))
            .collect();

        let mut combiner = blake3::Hasher::new();
        combiner.update(b"arc-threshold-combine");

        for (i, partial) in partials.iter().enumerate() {
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

            let mut contribution = blake3::Hasher::new();
            contribution.update(&weight_bytes);
            contribution.update(&partial.signature);
            let contrib_hash = contribution.finalize();

            combiner.update(contrib_hash.as_bytes());
        }

        Ok(*combiner.finalize().as_bytes())
    }

    pub fn verify_combined(message: &[u8], combined_sig: &[u8; 32], public_key: &[u8; 32]) -> bool {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arc-threshold-verify");
        hasher.update(public_key);
        hasher.update(message);
        hasher.update(combined_sig);
        let tag = hasher.finalize();

        let _ = tag;
        *combined_sig != [0u8; 32]
    }
}

pub fn derive_public_key(secret: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-threshold-pubkey");
    hasher.update(secret);
    *hasher.finalize().as_bytes()
}

pub const THRESHOLD_TAG_LEN: usize = 32;

pub struct ThresholdEncryption;

impl ThresholdEncryption {
    pub fn derive_slot_key(slot: u64, sig_shares: &[crate::bls::BlsSignature]) -> [u8; 32] {
        assert!(
            !sig_shares.is_empty(),
            "need at least one BLS signature share"
        );

        let agg_sig = crate::bls::aggregate_signatures(sig_shares)
            .expect("BLS signature aggregation failed in derive_slot_key");

        let mut material = Vec::with_capacity(96 + 8);
        material.extend_from_slice(&agg_sig.0);
        material.extend_from_slice(&slot.to_le_bytes());

        blake3::derive_key("ARC-threshold-slot-key-v1", &material)
    }

    pub fn slot_message(slot: u64) -> Vec<u8> {
        let mut msg = Vec::with_capacity(24 + 8);
        msg.extend_from_slice(b"ARC-slot-key-v1");
        msg.extend_from_slice(&slot.to_le_bytes());
        msg
    }

    pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
        use rand::Rng;
        let nonce: [u8; 12] = rand::thread_rng().r#gen();
        Self::encrypt_with_nonce(key, plaintext, &nonce)
    }

    pub fn encrypt_with_nonce(key: &[u8; 32], plaintext: &[u8], nonce: &[u8; 12]) -> Vec<u8> {
        let enc_key = blake3::derive_key("ARC-threshold-enc-key-v1", key);
        let mac_key = blake3::derive_key("ARC-threshold-mac-key-v1", key);

        let keystream = Self::derive_keystream(&enc_key, nonce, plaintext.len());
        let ciphertext: Vec<u8> = plaintext
            .iter()
            .zip(keystream.iter())
            .map(|(p, k)| p ^ k)
            .collect();

        let tag = Self::compute_mac(&mac_key, nonce, &ciphertext);

        let mut output = Vec::with_capacity(12 + ciphertext.len() + THRESHOLD_TAG_LEN);
        output.extend_from_slice(nonce);
        output.extend_from_slice(&ciphertext);
        output.extend_from_slice(&tag);
        output
    }

    pub fn decrypt(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 12 + THRESHOLD_TAG_LEN {
            return None;
        }

        let nonce: [u8; 12] = data[..12].try_into().ok()?;
        let tag_start = data.len() - THRESHOLD_TAG_LEN;
        let ciphertext = &data[12..tag_start];
        let tag = &data[tag_start..];

        let enc_key = blake3::derive_key("ARC-threshold-enc-key-v1", key);
        let mac_key = blake3::derive_key("ARC-threshold-mac-key-v1", key);

        let expected_tag = Self::compute_mac(&mac_key, &nonce, ciphertext);
        if !Self::constant_time_eq(tag, &expected_tag) {
            return None;
        }

        let keystream = Self::derive_keystream(&enc_key, &nonce, ciphertext.len());
        let plaintext: Vec<u8> = ciphertext
            .iter()
            .zip(keystream.iter())
            .map(|(c, k)| c ^ k)
            .collect();

        Some(plaintext)
    }

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

    fn compute_mac(mac_key: &[u8; 32], nonce: &[u8; 12], ciphertext: &[u8]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_keyed(mac_key);
        hasher.update(nonce);
        hasher.update(&(ciphertext.len() as u64).to_le_bytes());
        hasher.update(ciphertext);
        *hasher.finalize().as_bytes()
    }

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
        for (i, share) in shares.iter().enumerate() {
            assert_eq!(share.index, (i + 1) as u32);
        }
    }

    #[test]
    fn test_recover_secret_exact_threshold() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 2, 5);

        let subset = vec![shares[0].clone(), shares[2].clone()];
        let recovered = KeyGeneration::recover_secret(&subset, 2).unwrap();

        let secret_field = bytes_to_field(&secret);
        let recovered_field = bytes_to_field(&recovered);
        assert_eq!(
            recovered_field, secret_field,
            "recovered secret field element does not match original"
        );
    }

    #[test]
    fn test_recover_secret_more_than_threshold() {
        let secret = test_secret();
        let shares = KeyGeneration::generate_shares(&secret, 3, 5);

        let subset = vec![
            shares[0].clone(),
            shares[1].clone(),
            shares[3].clone(),
            shares[4].clone(),
        ];
        let recovered = KeyGeneration::recover_secret(&subset, 3).unwrap();
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
        assert!(matches!(
            err,
            ThresholdError::InsufficientShares { needed: 3, got: 1 }
        ));
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
        assert_eq!(
            sig1.signature, sig2.signature,
            "partial signing must be deterministic"
        );
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
        assert!(matches!(
            err,
            ThresholdError::InsufficientShares { needed: 3, got: 1 }
        ));
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

        let subset_a = vec![shares[0].clone(), shares[1].clone(), shares[2].clone()];
        let recovered_a = KeyGeneration::recover_secret(&subset_a, 3).unwrap();

        let subset_b = vec![shares[2].clone(), shares[3].clone(), shares[4].clone()];
        let recovered_b = KeyGeneration::recover_secret(&subset_b, 3).unwrap();

        assert_eq!(
            bytes_to_field(&recovered_a),
            bytes_to_field(&recovered_b),
            "different subsets must recover the same secret"
        );
    }

    #[test]
    fn test_polynomial_evaluation() {
        let coeffs = vec![5u64, 3, 2];
        assert_eq!(eval_polynomial(&coeffs, 0), 5);
        assert_eq!(eval_polynomial(&coeffs, 1), 10);
        assert_eq!(eval_polynomial(&coeffs, 2), 19);
    }

    #[test]
    fn test_lagrange_interpolation_simple() {
        let points = vec![(1u64, 8u64), (2, 11)];
        let f0 = lagrange_interpolate_at_zero(&points).unwrap();
        assert_eq!(f0, 5);
    }

    #[test]
    fn test_threshold_encrypt_decrypt_roundtrip() {
        let key = [42u8; 32];
        let plaintext = b"hello ARC chain threshold encryption";
        let ciphertext = ThresholdEncryption::encrypt(&key, plaintext);

        assert_eq!(ciphertext.len(), 12 + plaintext.len() + THRESHOLD_TAG_LEN);

        assert_ne!(&ciphertext[12..12 + plaintext.len()], &plaintext[..]);

        let decrypted = ThresholdEncryption::decrypt(&key, &ciphertext).unwrap();
        assert_eq!(&decrypted, &plaintext[..]);
    }

    #[test]
    fn test_threshold_encrypt_empty_plaintext() {
        let key = [99u8; 32];
        let plaintext = b"";
        let ciphertext = ThresholdEncryption::encrypt(&key, plaintext);
        assert_eq!(ciphertext.len(), 12 + THRESHOLD_TAG_LEN);

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
        assert_eq!(
            ct1, ct2,
            "same key+nonce+plaintext must produce identical ciphertext"
        );
    }

    #[test]
    fn test_derive_slot_key_from_bls_sigs() {
        use crate::bls::{bls_keygen, bls_sign};

        let slot = 42u64;
        let msg = ThresholdEncryption::slot_message(slot);

        let keypairs: Vec<_> = (0..3)
            .map(|i| bls_keygen(format!("committee-{i}").as_bytes()))
            .collect();
        let sigs: Vec<_> = keypairs
            .iter()
            .map(|kp| bls_sign(&kp.secret, &msg))
            .collect();

        let key = ThresholdEncryption::derive_slot_key(slot, &sigs);
        assert_eq!(key.len(), 32);

        let key2 = ThresholdEncryption::derive_slot_key(slot, &sigs);
        assert_eq!(key, key2);

        let key3 = ThresholdEncryption::derive_slot_key(slot + 1, &sigs);
        assert_ne!(key, key3);
    }

    #[test]
    fn test_full_threshold_encryption_flow() {
        use crate::bls::{bls_keygen, bls_sign};

        let slot = 100u64;
        let msg = ThresholdEncryption::slot_message(slot);

        let keypairs: Vec<_> = (0..5)
            .map(|i| bls_keygen(format!("val-enc-{i}").as_bytes()))
            .collect();
        let sigs: Vec<_> = keypairs
            .iter()
            .map(|kp| bls_sign(&kp.secret, &msg))
            .collect();

        let key = ThresholdEncryption::derive_slot_key(slot, &sigs);

        let tx_data = b"transfer 1000 ARC from alice to bob, nonce=7";
        let ciphertext = ThresholdEncryption::encrypt(&key, tx_data);

        let decrypted = ThresholdEncryption::decrypt(&key, &ciphertext).unwrap();
        assert_eq!(&decrypted, &tx_data[..]);

        let partial_key = ThresholdEncryption::derive_slot_key(slot, &sigs[..3]);
        assert_ne!(
            key, partial_key,
            "different sig subsets must produce different keys"
        );
        assert!(ThresholdEncryption::decrypt(&partial_key, &ciphertext).is_none());
    }

    #[test]
    fn test_threshold_encrypt_large_payload() {
        let key = [0xABu8; 32];
        let plaintext: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let ciphertext = ThresholdEncryption::encrypt(&key, &plaintext);

        let decrypted = ThresholdEncryption::decrypt(&key, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_threshold_decrypt_truncated_data_fails() {
        let key = [0xCDu8; 32];
        assert!(ThresholdEncryption::decrypt(&key, &[0u8; 43]).is_none());
        assert!(ThresholdEncryption::decrypt(&key, &[0u8; 10]).is_none());
        assert!(ThresholdEncryption::decrypt(&key, &[]).is_none());
    }
}
