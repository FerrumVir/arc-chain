//! RFC 9381 ECVRF, ciphersuite `ECVRF-EDWARDS25519-SHA512-TAI` (`suite_string = 0x03`).
//!
//! This module is a literal transcription of RFC 9381 Section 5 on top of
//! `curve25519-dalek` (group arithmetic, constant-time scalar multiplication,
//! point decoding) and `sha2` (SHA-512). The step names below are the RFC's:
//!
//! - key derivation: RFC 8032 Section 5.1.5 (SHA-512 of the 32-byte secret key;
//!   the clamped lower half is the scalar `x`, the upper half seeds the nonce);
//! - `ECVRF_encode_to_curve_try_and_increment` (Section 5.4.1.1) with the public
//!   key as `encode_to_curve_salt` (Section 5.5);
//! - `ECVRF_nonce_generation_RFC8032` (Section 5.4.2.2);
//! - `ECVRF_challenge_generation` (Section 5.4.3) with `cLen = 16`;
//! - `ECVRF_decode_proof` (Section 5.4.4), which rejects `s >= q`;
//! - `ECVRF_validate_key` (Section 5.4.5), always applied: this implementation
//!   supports only `validate_key = TRUE`, as the RFC permits.
//!
//! Encodings follow RFC 8032: little-endian integers and 32-byte compressed
//! points. Point decoding fails on non-canonical encodings (`y >= p`, or `x = 0`
//! with the sign bit set). `curve25519-dalek` 4.1.3 accepts a non-canonical `y`
//! in `CompressedEdwardsY::decompress`, so the private decoder here re-encodes
//! the decoded point and requires the bytes to round-trip.
//!
//! A proof is `pi = Gamma (32) || c (16) || s (32)`, 80 bytes. The output `beta`
//! is 64 bytes. The prover uses constant-time scalar multiplication for every
//! secret-dependent operation; the verifier uses variable-time multiplication,
//! which RFC 9381 Section 7.5 allows because all of its inputs are public. The
//! try-and-increment loop's running time depends on `alpha`; in ARC `alpha` is
//! public (see the `leader` module), which is the case the RFC permits.

use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::{Scalar, clamp_integer};
use curve25519_dalek::traits::{IsIdentity, VartimeMultiscalarMul};
use rand_core::CryptoRngCore;
use sha2::{Digest, Sha512};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// `suite_string` of `ECVRF-EDWARDS25519-SHA512-TAI` (RFC 9381 Section 5.5).
pub const SUITE_STRING: u8 = 0x03;
/// Length of a secret key: the 32-byte RFC 8032 secret key `SK`.
pub const SECRET_KEY_LENGTH: usize = 32;
/// Length of a compressed public key (`ptLen`).
pub const PUBLIC_KEY_LENGTH: usize = 32;
/// Length of a proof: `ptLen + cLen + qLen = 32 + 16 + 32`.
pub const PROOF_LENGTH: usize = 80;
/// Length of the VRF output `beta_string` (`hLen` of SHA-512).
pub const OUTPUT_LENGTH: usize = 64;

/// `cLen` for every RFC 9381 ciphersuite.
const C_LEN: usize = 16;
const ENCODE_TO_CURVE_DOMAIN_SEPARATOR_FRONT: u8 = 0x01;
const CHALLENGE_GENERATION_DOMAIN_SEPARATOR_FRONT: u8 = 0x02;
const PROOF_TO_HASH_DOMAIN_SEPARATOR_FRONT: u8 = 0x03;
const DOMAIN_SEPARATOR_BACK: u8 = 0x00;

/// Errors returned by the ECVRF API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VrfError {
    /// The public key bytes are not the canonical encoding of a point on edwards25519.
    #[error("public key is not a canonical edwards25519 point encoding")]
    InvalidPublicKey,
    /// The public key is a small-order point (`ECVRF_validate_key` failed).
    #[error("public key is a small-order point (ECVRF_validate_key failed)")]
    SmallOrderPublicKey,
    /// The proof bytes do not decode (`ECVRF_decode_proof` failed): wrong length,
    /// non-canonical `Gamma`, or `s >= q`.
    #[error("proof does not decode (wrong length, non-canonical Gamma, or s >= q)")]
    InvalidProof,
    /// The proof decoded but does not verify for this public key and input.
    #[error("proof does not verify for this public key and input")]
    VerificationFailed,
    /// Try-and-increment exhausted all 256 counter values (probability about 2^-256).
    #[error("encode_to_curve exhausted the try-and-increment counter")]
    EncodeToCurveFailed,
}

/// An ECVRF secret key: the 32-byte RFC 8032 secret key `SK`. Zeroized on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretKey {
    bytes: [u8; SECRET_KEY_LENGTH],
}

impl core::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SecretKey([redacted])")
    }
}

/// Secret material derived from `SK` (RFC 8032 Section 5.1.5): the scalar `x`
/// and the upper 32 bytes of `SHA-512(SK)` that nonce generation hashes.
#[derive(Zeroize, ZeroizeOnDrop)]
struct ExpandedSecretKey {
    x: Scalar,
    nonce_prefix: [u8; 32],
}

impl SecretKey {
    /// Wraps 32 secret bytes as a secret key.
    pub fn from_bytes(bytes: [u8; SECRET_KEY_LENGTH]) -> Self {
        Self { bytes }
    }

    /// Generates a secret key from a cryptographically secure random number generator.
    pub fn generate<R: CryptoRngCore + ?Sized>(rng: &mut R) -> Self {
        let mut bytes = [0u8; SECRET_KEY_LENGTH];
        rng.fill_bytes(&mut bytes);
        Self { bytes }
    }

    /// Returns the 32 secret bytes.
    pub fn to_bytes(&self) -> [u8; SECRET_KEY_LENGTH] {
        self.bytes
    }

    /// Derives the public key `Y = x*B`.
    pub fn public_key(&self) -> PublicKey {
        let expanded = self.expand();
        let point = EdwardsPoint::mul_base(&expanded.x);
        PublicKey {
            point,
            bytes: point.compress().to_bytes(),
        }
    }

    /// `ECVRF_prove(SK, alpha_string)` (RFC 9381 Section 5.1).
    ///
    /// Deterministic: the same key and input always produce the same proof.
    pub fn prove(&self, alpha: &[u8]) -> Result<Proof, VrfError> {
        prove_with_suite(self, alpha, SUITE_STRING)
    }

    /// RFC 8032 Section 5.1.5: hash the secret key, clamp the lower half into
    /// the scalar `x`, keep the upper half for nonce generation.
    fn expand(&self) -> ExpandedSecretKey {
        let hashed = Sha512::digest(self.bytes);
        let mut x_bytes = [0u8; 32];
        x_bytes.copy_from_slice(&hashed[..32]);
        let mut nonce_prefix = [0u8; 32];
        nonce_prefix.copy_from_slice(&hashed[32..]);
        let x = Scalar::from_bytes_mod_order(clamp_integer(x_bytes));
        x_bytes.zeroize();
        ExpandedSecretKey { x, nonce_prefix }
    }
}

/// An ECVRF public key `Y`, validated on construction: canonical encoding and
/// not a small-order point (`ECVRF_validate_key`, RFC 9381 Section 5.4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicKey {
    point: EdwardsPoint,
    bytes: [u8; PUBLIC_KEY_LENGTH],
}

impl PublicKey {
    /// `string_to_point(PK_string)` followed by `ECVRF_validate_key(Y)`.
    pub fn from_bytes(bytes: [u8; PUBLIC_KEY_LENGTH]) -> Result<Self, VrfError> {
        let point = decode_point(&bytes).ok_or(VrfError::InvalidPublicKey)?;
        if point.is_small_order() {
            return Err(VrfError::SmallOrderPublicKey);
        }
        Ok(Self { point, bytes })
    }

    /// The compressed public key.
    pub fn as_bytes(&self) -> &[u8; PUBLIC_KEY_LENGTH] {
        &self.bytes
    }

    /// The compressed public key, by value.
    pub fn to_bytes(self) -> [u8; PUBLIC_KEY_LENGTH] {
        self.bytes
    }

    /// `ECVRF_verify(PK_string, alpha_string, pi_string)` (RFC 9381 Section 5.3)
    /// with `validate_key = TRUE` (already enforced by [`PublicKey::from_bytes`]).
    ///
    /// Returns `beta_string` on success. Every failure is
    /// [`VrfError::VerificationFailed`]; the proof's own decoding errors are
    /// reported by [`Proof::from_bytes`].
    pub fn verify(&self, alpha: &[u8], proof: &Proof) -> Result<Output, VrfError> {
        verify_with_suite(self, alpha, proof, SUITE_STRING)
    }
}

/// A decoded ECVRF proof `pi = Gamma || c || s` (RFC 9381 Section 5.4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proof {
    gamma: EdwardsPoint,
    gamma_bytes: [u8; 32],
    c: [u8; C_LEN],
    s: Scalar,
}

impl Proof {
    /// `ECVRF_decode_proof(pi_string)`: rejects a non-canonical `Gamma` encoding
    /// and `s >= q` (both would otherwise make proofs malleable).
    pub fn from_bytes(pi: &[u8; PROOF_LENGTH]) -> Result<Self, VrfError> {
        let mut gamma_bytes = [0u8; 32];
        gamma_bytes.copy_from_slice(&pi[..32]);
        let gamma = decode_point(&gamma_bytes).ok_or(VrfError::InvalidProof)?;
        let mut c = [0u8; C_LEN];
        c.copy_from_slice(&pi[32..48]);
        let mut s_bytes = [0u8; 32];
        s_bytes.copy_from_slice(&pi[48..]);
        let s = Option::<Scalar>::from(Scalar::from_canonical_bytes(s_bytes))
            .ok_or(VrfError::InvalidProof)?;
        Ok(Self {
            gamma,
            gamma_bytes,
            c,
            s,
        })
    }

    /// Like [`Proof::from_bytes`], for a slice; any length other than 80 is
    /// [`VrfError::InvalidProof`].
    pub fn from_slice(pi: &[u8]) -> Result<Self, VrfError> {
        let array: &[u8; PROOF_LENGTH] = pi.try_into().map_err(|_| VrfError::InvalidProof)?;
        Self::from_bytes(array)
    }

    /// `pi_string = point_to_string(Gamma) || int_to_string(c, cLen) || int_to_string(s, qLen)`.
    pub fn to_bytes(self) -> [u8; PROOF_LENGTH] {
        let mut pi = [0u8; PROOF_LENGTH];
        pi[..32].copy_from_slice(&self.gamma_bytes);
        pi[32..48].copy_from_slice(&self.c);
        pi[48..].copy_from_slice(&self.s.to_bytes());
        pi
    }

    /// `ECVRF_proof_to_hash(pi_string)` (RFC 9381 Section 5.2).
    ///
    /// Only meaningful for a proof that came out of [`SecretKey::prove`] or
    /// passed [`PublicKey::verify`]; `verify` already returns this value.
    pub fn to_hash(self) -> Output {
        proof_to_hash_with_suite(&self.gamma, SUITE_STRING)
    }
}

/// The VRF output `beta_string`: 64 pseudorandom bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output([u8; OUTPUT_LENGTH]);

impl Output {
    /// The output bytes.
    pub fn as_bytes(&self) -> &[u8; OUTPUT_LENGTH] {
        &self.0
    }

    /// The output bytes, by value.
    pub fn to_bytes(self) -> [u8; OUTPUT_LENGTH] {
        self.0
    }
}

impl AsRef<[u8]> for Output {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// RFC 9381 Section 5.1, parameterised by `suite_string` so tests can show that
/// a proof made under another suite identifier never verifies under this one.
fn prove_with_suite(sk: &SecretKey, alpha: &[u8], suite: u8) -> Result<Proof, VrfError> {
    let expanded = sk.expand();
    // Step 1: Y = x*B.
    let y = EdwardsPoint::mul_base(&expanded.x);
    let y_bytes = y.compress().to_bytes();
    // Steps 2 and 3: H = ECVRF_encode_to_curve(PK_string, alpha_string).
    let (h, _ctr) = encode_to_curve_tai(suite, &y_bytes, alpha)?;
    let h_bytes = h.compress().to_bytes();
    // Step 4: Gamma = x*H (constant time).
    let gamma = h * expanded.x;
    let gamma_bytes = gamma.compress().to_bytes();
    // Step 5: k = ECVRF_nonce_generation(SK, h_string).
    let (mut k_string, mut k) = nonce_generation(&expanded.nonce_prefix, &h_bytes);
    // Step 6: c = ECVRF_challenge_generation(Y, H, Gamma, k*B, k*H).
    let u_bytes = EdwardsPoint::mul_base(&k).compress().to_bytes();
    let v_bytes = (h * k).compress().to_bytes();
    let c = challenge_generation(
        suite,
        [&y_bytes, &h_bytes, &gamma_bytes, &u_bytes, &v_bytes],
    );
    // Step 7: s = (k + c*x) mod q.
    let s = k + challenge_to_scalar(&c) * expanded.x;
    k.zeroize();
    k_string.zeroize();
    // Step 8: pi_string = point_to_string(Gamma) || int_to_string(c, cLen) || int_to_string(s, qLen).
    Ok(Proof {
        gamma,
        gamma_bytes,
        c,
        s,
    })
}

/// RFC 9381 Section 5.3, steps 4 to 11 (steps 1 to 3 ran in [`PublicKey::from_bytes`]
/// and steps 4 to 6 in [`Proof::from_bytes`]).
fn verify_with_suite(
    pk: &PublicKey,
    alpha: &[u8],
    proof: &Proof,
    suite: u8,
) -> Result<Output, VrfError> {
    // Step 7: H = ECVRF_encode_to_curve(PK_string, alpha_string).
    let (h, _ctr) = encode_to_curve_tai(suite, &pk.bytes, alpha)?;
    let h_bytes = h.compress().to_bytes();
    let c = challenge_to_scalar(&proof.c);
    let neg_c = -c;
    // Step 8: U = s*B - c*Y. Variable time: every input is public (RFC 9381 Section 7.5).
    let u_bytes = EdwardsPoint::vartime_double_scalar_mul_basepoint(&neg_c, &pk.point, &proof.s)
        .compress()
        .to_bytes();
    // Step 9: V = s*H - c*Gamma.
    let v_bytes = EdwardsPoint::vartime_multiscalar_mul([proof.s, neg_c], [h, proof.gamma])
        .compress()
        .to_bytes();
    // Step 10: c' = ECVRF_challenge_generation(Y, H, Gamma, U, V).
    let c_prime = challenge_generation(
        suite,
        [&pk.bytes, &h_bytes, &proof.gamma_bytes, &u_bytes, &v_bytes],
    );
    // Step 11: c == c' decides. Both values are derived from public data.
    if c_prime != proof.c {
        return Err(VrfError::VerificationFailed);
    }
    Ok(proof_to_hash_with_suite(&proof.gamma, suite))
}

/// RFC 9381 Section 5.4.1.1 with the Section 5.5 options for edwards25519:
/// `interpret_hash_value_as_a_point(s) = string_to_point(s[0]...s[31])`, cofactor 8.
///
/// Returns the point and the counter value that succeeded (the test vectors list it).
fn encode_to_curve_tai(
    suite: u8,
    salt: &[u8; 32],
    alpha: &[u8],
) -> Result<(EdwardsPoint, u8), VrfError> {
    for ctr in 0..=u8::MAX {
        let mut hasher = Sha512::new();
        hasher.update([suite, ENCODE_TO_CURVE_DOMAIN_SEPARATOR_FRONT]);
        hasher.update(salt);
        hasher.update(alpha);
        hasher.update([ctr, DOMAIN_SEPARATOR_BACK]);
        let hash = hasher.finalize();
        let mut candidate = [0u8; 32];
        candidate.copy_from_slice(&hash[..32]);
        if let Some(point) = decode_point(&candidate) {
            let h = point.mul_by_cofactor();
            if !h.is_identity() {
                return Ok((h, ctr));
            }
        }
    }
    Err(VrfError::EncodeToCurveFailed)
}

/// RFC 9381 Section 5.4.2.2: `k_string = Hash(hashed_sk_string[32..64] || h_string)`,
/// `k = string_to_int(k_string) mod q`. Returns both, as the test vectors list both.
fn nonce_generation(nonce_prefix: &[u8; 32], h_string: &[u8; 32]) -> ([u8; 64], Scalar) {
    let mut hasher = Sha512::new();
    hasher.update(nonce_prefix);
    hasher.update(h_string);
    let mut k_string = [0u8; 64];
    k_string.copy_from_slice(&hasher.finalize());
    let k = Scalar::from_bytes_mod_order_wide(&k_string);
    (k_string, k)
}

/// RFC 9381 Section 5.4.3: the first `cLen` bytes of
/// `Hash(suite_string || 0x02 || P1 || P2 || P3 || P4 || P5 || 0x00)`.
fn challenge_generation(suite: u8, points: [&[u8; 32]; 5]) -> [u8; C_LEN] {
    let mut hasher = Sha512::new();
    hasher.update([suite, CHALLENGE_GENERATION_DOMAIN_SEPARATOR_FRONT]);
    for point in points {
        hasher.update(point);
    }
    hasher.update([DOMAIN_SEPARATOR_BACK]);
    let hash = hasher.finalize();
    let mut c = [0u8; C_LEN];
    c.copy_from_slice(&hash[..C_LEN]);
    c
}

/// `string_to_int(c_string)` as a scalar; `c < 2^128 < q`, so this is exact.
fn challenge_to_scalar(c: &[u8; C_LEN]) -> Scalar {
    let mut bytes = [0u8; 32];
    bytes[..C_LEN].copy_from_slice(c);
    Scalar::from_bytes_mod_order(bytes)
}

/// RFC 9381 Section 5.2, step 6:
/// `beta_string = Hash(suite_string || 0x03 || point_to_string(cofactor * Gamma) || 0x00)`.
fn proof_to_hash_with_suite(gamma: &EdwardsPoint, suite: u8) -> Output {
    let mut hasher = Sha512::new();
    hasher.update([suite, PROOF_TO_HASH_DOMAIN_SEPARATOR_FRONT]);
    hasher.update(gamma.mul_by_cofactor().compress().as_bytes());
    hasher.update([DOMAIN_SEPARATOR_BACK]);
    let mut beta = [0u8; OUTPUT_LENGTH];
    beta.copy_from_slice(&hasher.finalize());
    Output(beta)
}

/// `string_to_point` per RFC 8032 Section 5.1.3, which rejects non-canonical
/// encodings (`y >= p`, or `x = 0` with the sign bit set). `curve25519-dalek`
/// 4.1.3 decodes a non-canonical `y` without complaint, so the decoded point is
/// re-encoded and must reproduce the input bytes exactly.
fn decode_point(bytes: &[u8; 32]) -> Option<EdwardsPoint> {
    let point = CompressedEdwardsY(*bytes).decompress()?;
    if point.compress().as_bytes() == bytes {
        Some(point)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// RFC 9381 Appendix B.3, Examples 16 to 18, as committed data.
    const RFC_VECTORS: &str =
        include_str!("../tests/vectors/rfc9381_b3_ecvrf_edwards25519_sha512_tai.json");

    fn hex_field(vector: &Value, key: &str) -> Vec<u8> {
        let text = vector[key]
            .as_str()
            .unwrap_or_else(|| panic!("missing field {key}"));
        hex::decode(text).unwrap_or_else(|_| panic!("field {key} is not hex"))
    }

    fn hex32(vector: &Value, key: &str) -> [u8; 32] {
        hex_field(vector, key)
            .try_into()
            .unwrap_or_else(|_| panic!("field {key} is not 32 bytes"))
    }

    #[test]
    fn rfc9381_appendix_b3_vectors_are_reproduced_byte_exactly() {
        let document: Value = serde_json::from_str(RFC_VECTORS).expect("vector file parses");
        assert_eq!(document["suite_string"].as_str(), Some("03"));
        let vectors = document["vectors"].as_array().expect("vectors array");
        assert_eq!(
            vectors.len(),
            3,
            "RFC 9381 B.3 lists Examples 16, 17 and 18"
        );

        for vector in vectors {
            let example = vector["example"].as_u64().expect("example number");
            let sk = SecretKey::from_bytes(hex32(vector, "sk"));
            let alpha = hex_field(vector, "alpha");

            // RFC 8032 Section 5.1.5 key derivation: PK and the clamped scalar x.
            let pk = sk.public_key();
            assert_eq!(pk.to_bytes(), hex32(vector, "pk"), "example {example}: PK");
            let hashed = Sha512::digest(sk.to_bytes());
            let mut x_clamped = [0u8; 32];
            x_clamped.copy_from_slice(&hashed[..32]);
            assert_eq!(
                clamp_integer(x_clamped),
                hex32(vector, "x"),
                "example {example}: x"
            );

            // Section 5.4.1.1 try-and-increment: H and the counter that succeeded.
            let (h, ctr) = encode_to_curve_tai(SUITE_STRING, pk.as_bytes(), &alpha).unwrap();
            let h_bytes = h.compress().to_bytes();
            assert_eq!(h_bytes, hex32(vector, "h"), "example {example}: H");
            assert_eq!(
                u64::from(ctr),
                vector["ctr"].as_u64().unwrap(),
                "example {example}: ctr"
            );

            // Section 5.4.2.2 nonce: k_string (64 bytes) and k = k_string mod q.
            let expanded = sk.expand();
            let (k_string, k) = nonce_generation(&expanded.nonce_prefix, &h_bytes);
            assert_eq!(
                k_string.to_vec(),
                hex_field(vector, "k_string"),
                "example {example}: k_string"
            );
            assert_eq!(k.to_bytes(), hex32(vector, "k"), "example {example}: k");

            // The challenge inputs U = k*B and V = k*H.
            let u = EdwardsPoint::mul_base(&k).compress().to_bytes();
            let v = (h * k).compress().to_bytes();
            assert_eq!(u, hex32(vector, "u"), "example {example}: U");
            assert_eq!(v, hex32(vector, "v"), "example {example}: V");

            // The proof and the output, through the public API.
            let proof = sk.prove(&alpha).unwrap();
            assert_eq!(
                proof.to_bytes().to_vec(),
                hex_field(vector, "pi"),
                "example {example}: pi"
            );
            let beta = pk.verify(&alpha, &proof).unwrap();
            assert_eq!(
                beta.to_bytes().to_vec(),
                hex_field(vector, "beta"),
                "example {example}: beta"
            );
            assert_eq!(proof.to_hash(), beta, "example {example}: proof_to_hash");

            // Round trip through the wire encoding.
            let decoded = Proof::from_bytes(&proof.to_bytes()).unwrap();
            assert_eq!(decoded, proof);
            assert_eq!(pk.verify(&alpha, &decoded).unwrap(), beta);
            let pk_decoded = PublicKey::from_bytes(pk.to_bytes()).unwrap();
            assert_eq!(pk_decoded, pk);
        }
    }

    #[test]
    fn cross_ciphersuite_confusion_is_rejected() {
        // 0x04 is the suite_string of ECVRF-EDWARDS25519-SHA512-ELL2. Running the
        // same arithmetic under that identifier stands in for "a proof made by a
        // different ciphersuite": it must not verify here, and its output must
        // differ from the TAI output, because every hash is domain-separated by
        // suite_string.
        const FOREIGN_SUITE: u8 = 0x04;
        let sk = SecretKey::from_bytes([7u8; SECRET_KEY_LENGTH]);
        let pk = sk.public_key();
        let alpha = b"cross-suite confusion";

        let foreign = prove_with_suite(&sk, alpha, FOREIGN_SUITE).unwrap();
        assert_eq!(
            pk.verify(alpha, &foreign),
            Err(VrfError::VerificationFailed)
        );

        let native = sk.prove(alpha).unwrap();
        assert_eq!(
            verify_with_suite(&pk, alpha, &native, FOREIGN_SUITE),
            Err(VrfError::VerificationFailed)
        );

        let native_beta = pk.verify(alpha, &native).unwrap();
        let foreign_beta = verify_with_suite(&pk, alpha, &foreign, FOREIGN_SUITE).unwrap();
        assert_ne!(native_beta, foreign_beta);
        assert_ne!(native.to_bytes(), foreign.to_bytes());
    }

    #[test]
    fn decode_point_rejects_non_canonical_encodings() {
        // The identity, canonically encoded (y = 1, sign 0), decodes.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(decode_point(&identity).is_some());
        // y = 1 + p (non-canonical encoding of the identity) is rejected.
        let mut identity_plus_p = [0xffu8; 32];
        identity_plus_p[0] = 0xee;
        identity_plus_p[31] = 0x7f;
        assert!(decode_point(&identity_plus_p).is_none());
        // x = 0 with the sign bit set is rejected (RFC 8032 Section 5.1.3).
        let mut identity_negative_zero = identity;
        identity_negative_zero[31] |= 0x80;
        assert!(decode_point(&identity_negative_zero).is_none());
        // y = p itself (non-canonical zero) is rejected.
        let mut p = [0xffu8; 32];
        p[0] = 0xed;
        p[31] = 0x7f;
        assert!(decode_point(&p).is_none());
    }
}
