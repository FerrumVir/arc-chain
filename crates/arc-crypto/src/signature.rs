//! Digital signature primitives for ARC Chain.
//!
//! Two signature schemes:
//! - **Ed25519**: Fast agent/native transactions (~4-9M batch verifications/sec)
//! - **Secp256k1**: ETH-compatible operations (MetaMask, bridge verification)
//!
//! Address derivation:
//! - Ed25519: `address = BLAKE3(public_key)[0..32]`
//! - Secp256k1: `address = BLAKE3(uncompressed_pubkey[1..65])[0..32]`

use crate::hash::{Hash256, hash_bytes};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SignatureError {
    #[error("invalid signature")]
    InvalidSignature,

    #[error("invalid public key")]
    InvalidPublicKey,

    #[error("key generation failed")]
    KeyGeneration,

    #[error("signing failed: {0}")]
    SigningFailed(String),

    #[error("batch verification failed")]
    BatchVerifyFailed,

    #[error("address mismatch: expected {expected}, got {got}")]
    AddressMismatch { expected: String, got: String },

    #[error("hash mismatch: transaction data does not match hash")]
    HashMismatch,

    #[error("invalid signature length: expected {expected}, got {got}")]
    InvalidLength { expected: usize, got: usize },
}

// ── Signature ───────────────────────────────────────────────────────────────

/// Cryptographic signature proving transaction authorization.
///
/// Ed25519 signatures carry the public key (96 bytes total).
/// Secp256k1 signatures are recoverable (65 bytes total: r‖s‖v).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signature {
    /// Ed25519 signature (fast, for native/agent transactions).
    Ed25519 {
        /// 32-byte compressed public key.
        public_key: [u8; 32],
        /// 64-byte Ed25519 signature (stored as Vec for serde compat).
        signature: Vec<u8>,
    },
    /// Secp256k1 recoverable signature (ETH-compatible).
    Secp256k1 {
        /// 65-byte recoverable signature: r[32] ‖ s[32] ‖ v[1].
        signature: Vec<u8>,
    },
}

impl Signature {
    /// Verify this signature against a message hash and expected sender address.
    ///
    /// For Ed25519: checks that `public_key` hashes to `expected_address`,
    /// then verifies the signature over `message_hash`.
    ///
    /// For Secp256k1: recovers the public key from `(signature, recovery_id)`,
    /// then checks it hashes to `expected_address`.
    pub fn verify(
        &self,
        message_hash: &Hash256,
        expected_address: &Hash256,
    ) -> Result<(), SignatureError> {
        match self {
            Signature::Ed25519 {
                public_key,
                signature,
            } => {
                // Length check
                if signature.len() != 64 {
                    return Err(SignatureError::InvalidLength {
                        expected: 64,
                        got: signature.len(),
                    });
                }

                // 1. Public key must hash to the expected address
                let derived = address_from_ed25519_pubkey(public_key);
                if derived != *expected_address {
                    return Err(SignatureError::AddressMismatch {
                        expected: expected_address.to_hex(),
                        got: derived.to_hex(),
                    });
                }

                // 2. Verify the Ed25519 signature
                let vk = ed25519_dalek::VerifyingKey::from_bytes(public_key)
                    .map_err(|_| SignatureError::InvalidPublicKey)?;
                let sig_bytes: [u8; 64] = signature
                    .as_slice()
                    .try_into()
                    .map_err(|_| SignatureError::InvalidSignature)?;
                let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
                use ed25519_dalek::Verifier;
                vk.verify(message_hash.as_bytes(), &sig)
                    .map_err(|_| SignatureError::InvalidSignature)
            }

            Signature::Secp256k1 { signature } => {
                // Length check
                if signature.len() != 65 {
                    return Err(SignatureError::InvalidLength {
                        expected: 65,
                        got: signature.len(),
                    });
                }

                // 1. Split 65-byte blob into (r‖s, v)
                let (rs, v_slice) = signature.split_at(64);
                let recovery_id = k256::ecdsa::RecoveryId::try_from(v_slice[0])
                    .map_err(|_| SignatureError::InvalidSignature)?;
                let sig = k256::ecdsa::Signature::from_slice(rs)
                    .map_err(|_| SignatureError::InvalidSignature)?;

                // 2. Recover public key from the signature + hash
                let recovered_vk = k256::ecdsa::VerifyingKey::recover_from_prehash(
                    message_hash.as_bytes(),
                    &sig,
                    recovery_id,
                )
                .map_err(|_| SignatureError::InvalidSignature)?;

                // 3. Check the recovered key hashes to the expected address
                let uncompressed = recovered_vk.to_encoded_point(false);
                let point_bytes = uncompressed.as_bytes();
                // Skip the 0x04 uncompressed prefix → 64 raw bytes
                let derived = address_from_secp256k1_pubkey(&point_bytes[1..65]);
                if derived != *expected_address {
                    return Err(SignatureError::AddressMismatch {
                        expected: expected_address.to_hex(),
                        got: derived.to_hex(),
                    });
                }

                Ok(())
            }
        }
    }

    /// Returns a null/empty signature (for unsigned benchmark transactions).
    pub fn null() -> Self {
        Signature::Ed25519 {
            public_key: [0u8; 32],
            signature: vec![0u8; 64],
        }
    }

    /// Returns true if this is the null (unsigned) signature.
    pub fn is_null(&self) -> bool {
        match self {
            Signature::Ed25519 {
                public_key,
                signature,
            } => *public_key == [0u8; 32] && signature.iter().all(|&b| b == 0),
            _ => false,
        }
    }
}

impl Default for Signature {
    fn default() -> Self {
        Self::null()
    }
}

// ── KeyPair ─────────────────────────────────────────────────────────────────

/// Key pair for signing ARC chain transactions.
pub enum KeyPair {
    /// Ed25519 key pair — fast, native operations.
    Ed25519(ed25519_dalek::SigningKey),
    /// Secp256k1 key pair — ETH-compatible operations.
    Secp256k1(k256::ecdsa::SigningKey),
}

impl KeyPair {
    /// Generate a new random Ed25519 key pair.
    pub fn generate_ed25519() -> Self {
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        KeyPair::Ed25519(signing_key)
    }

    /// Generate a new random Secp256k1 key pair.
    pub fn generate_secp256k1() -> Self {
        let signing_key = k256::ecdsa::SigningKey::random(&mut rand::rngs::OsRng);
        KeyPair::Secp256k1(signing_key)
    }

    /// Derive the ARC chain address from this key pair.
    ///
    /// Ed25519: `BLAKE3(public_key)`
    /// Secp256k1: `BLAKE3(uncompressed_point[1..65])`
    pub fn address(&self) -> Hash256 {
        match self {
            KeyPair::Ed25519(sk) => {
                let vk = sk.verifying_key();
                address_from_ed25519_pubkey(vk.as_bytes())
            }
            KeyPair::Secp256k1(sk) => {
                let vk = sk.verifying_key();
                let uncompressed = vk.to_encoded_point(false);
                let point_bytes = uncompressed.as_bytes();
                address_from_secp256k1_pubkey(&point_bytes[1..65])
            }
        }
    }

    /// Sign a message hash, returning a `Signature`.
    pub fn sign(&self, message_hash: &Hash256) -> Result<Signature, SignatureError> {
        match self {
            KeyPair::Ed25519(sk) => {
                use ed25519_dalek::Signer;
                let sig = sk.sign(message_hash.as_bytes());
                let vk = sk.verifying_key();
                Ok(Signature::Ed25519 {
                    public_key: *vk.as_bytes(),
                    signature: sig.to_bytes().to_vec(),
                })
            }
            KeyPair::Secp256k1(sk) => {
                let (sig, recovery_id) = sk
                    .sign_prehash_recoverable(message_hash.as_bytes())
                    .map_err(|e| SignatureError::SigningFailed(e.to_string()))?;

                let mut sig_bytes = vec![0u8; 65];
                sig_bytes[..64].copy_from_slice(&sig.to_bytes());
                sig_bytes[64] = recovery_id.to_byte();

                Ok(Signature::Secp256k1 {
                    signature: sig_bytes,
                })
            }
        }
    }

    /// Get the raw public key bytes.
    pub fn public_key_bytes(&self) -> Vec<u8> {
        match self {
            KeyPair::Ed25519(sk) => sk.verifying_key().as_bytes().to_vec(),
            KeyPair::Secp256k1(sk) => {
                let vk = sk.verifying_key();
                vk.to_encoded_point(true).as_bytes().to_vec()
            }
        }
    }
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyPair::Ed25519(_) => write!(f, "KeyPair::Ed25519(<redacted>)"),
            KeyPair::Secp256k1(_) => write!(f, "KeyPair::Secp256k1(<redacted>)"),
        }
    }
}

// ── Address derivation ──────────────────────────────────────────────────────

/// Derive an ARC address from an Ed25519 public key.
/// `address = BLAKE3(public_key)[0..32]`
#[inline]
pub fn address_from_ed25519_pubkey(public_key: &[u8; 32]) -> Hash256 {
    hash_bytes(public_key)
}

/// Derive an ARC address from a raw (uncompressed, no prefix) Secp256k1 public key.
/// `address = BLAKE3(raw_64_bytes)[0..32]`
#[inline]
pub fn address_from_secp256k1_pubkey(raw_pubkey_64: &[u8]) -> Hash256 {
    debug_assert_eq!(
        raw_pubkey_64.len(),
        64,
        "expected 64-byte raw secp256k1 pubkey"
    );
    hash_bytes(raw_pubkey_64)
}

// ── Batch verification ──────────────────────────────────────────────────────

/// Batch verify N Ed25519 signatures using multi-scalar multiplication.
/// Approximately 2× faster than individual verification.
///
/// All three slices must have the same length.
pub fn batch_verify_ed25519(
    messages: &[&[u8]],
    signatures: &[ed25519_dalek::Signature],
    verifying_keys: &[ed25519_dalek::VerifyingKey],
) -> Result<(), SignatureError> {
    ed25519_dalek::verify_batch(messages, signatures, verifying_keys)
        .map_err(|_| SignatureError::BatchVerifyFailed)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Ed25519 ──

    #[test]
    fn ed25519_sign_and_verify() {
        let kp = KeyPair::generate_ed25519();
        let address = kp.address();
        let msg = hash_bytes(b"test transaction");

        let sig = kp.sign(&msg).expect("sign ok");
        sig.verify(&msg, &address).expect("verify ok");
    }

    #[test]
    fn ed25519_wrong_address_fails() {
        let kp = KeyPair::generate_ed25519();
        let wrong = hash_bytes(b"wrong address");
        let msg = hash_bytes(b"test transaction");

        let sig = kp.sign(&msg).expect("sign ok");
        assert!(sig.verify(&msg, &wrong).is_err());
    }

    #[test]
    fn ed25519_wrong_message_fails() {
        let kp = KeyPair::generate_ed25519();
        let address = kp.address();
        let msg = hash_bytes(b"test transaction");
        let wrong_msg = hash_bytes(b"different");

        let sig = kp.sign(&msg).expect("sign ok");
        assert!(sig.verify(&wrong_msg, &address).is_err());
    }

    #[test]
    fn ed25519_address_is_deterministic() {
        let kp = KeyPair::generate_ed25519();
        assert_eq!(kp.address(), kp.address());
    }

    // ── Secp256k1 ──

    #[test]
    fn secp256k1_sign_and_verify() {
        let kp = KeyPair::generate_secp256k1();
        let address = kp.address();
        let msg = hash_bytes(b"test transaction");

        let sig = kp.sign(&msg).expect("sign ok");
        sig.verify(&msg, &address).expect("verify ok");
    }

    #[test]
    fn secp256k1_wrong_address_fails() {
        let kp = KeyPair::generate_secp256k1();
        let wrong = hash_bytes(b"wrong address");
        let msg = hash_bytes(b"test transaction");

        let sig = kp.sign(&msg).expect("sign ok");
        assert!(sig.verify(&msg, &wrong).is_err());
    }

    #[test]
    fn secp256k1_wrong_message_fails() {
        let kp = KeyPair::generate_secp256k1();
        let address = kp.address();
        let msg = hash_bytes(b"msg1");
        let wrong = hash_bytes(b"msg2");

        let sig = kp.sign(&msg).expect("sign ok");
        assert!(sig.verify(&wrong, &address).is_err());
    }

    // ── Batch verification ──

    #[test]
    fn batch_verify_10_ed25519() {
        let keys: Vec<_> = (0..10)
            .map(|_| ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng))
            .collect();

        let messages: Vec<Vec<u8>> = (0..10)
            .map(|i| format!("message {}", i).into_bytes())
            .collect();

        let signatures: Vec<_> = keys
            .iter()
            .zip(messages.iter())
            .map(|(sk, msg)| {
                use ed25519_dalek::Signer;
                sk.sign(msg)
            })
            .collect();

        let verifying_keys: Vec<_> = keys.iter().map(|sk| sk.verifying_key()).collect();
        let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();

        batch_verify_ed25519(&msg_refs, &signatures, &verifying_keys).expect("batch verify ok");
    }

    #[test]
    fn batch_verify_fails_on_bad_signature() {
        let keys: Vec<_> = (0..5)
            .map(|_| ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng))
            .collect();

        let messages: Vec<Vec<u8>> = (0..5)
            .map(|i| format!("message {}", i).into_bytes())
            .collect();

        let mut signatures: Vec<_> = keys
            .iter()
            .zip(messages.iter())
            .map(|(sk, msg)| {
                use ed25519_dalek::Signer;
                sk.sign(msg)
            })
            .collect();

        // Corrupt one signature by signing a different message
        {
            use ed25519_dalek::Signer;
            signatures[2] = keys[2].sign(b"tampered");
        }

        let verifying_keys: Vec<_> = keys.iter().map(|sk| sk.verifying_key()).collect();
        let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();

        assert!(batch_verify_ed25519(&msg_refs, &signatures, &verifying_keys).is_err());
    }

    // ── Null / Default ──

    #[test]
    fn null_signature() {
        let sig = Signature::null();
        assert!(sig.is_null());
        assert_eq!(sig, Signature::default());
    }

    #[test]
    fn non_null_signature() {
        let kp = KeyPair::generate_ed25519();
        let sig = kp.sign(&hash_bytes(b"x")).unwrap();
        assert!(!sig.is_null());
    }

    // ── Cross-scheme ──

    #[test]
    fn ed25519_and_secp256k1_addresses_differ() {
        let ed = KeyPair::generate_ed25519();
        let secp = KeyPair::generate_secp256k1();
        // Different schemes → different addresses (collision probability ~2^-256)
        assert_ne!(ed.address(), secp.address());
    }

    #[test]
    fn signature_serialization_roundtrip() {
        let kp = KeyPair::generate_ed25519();
        let sig = kp.sign(&hash_bytes(b"test")).unwrap();
        let json = serde_json::to_string(&sig).expect("serialize");
        let recovered: Signature = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(sig, recovered);
    }

    #[test]
    fn secp256k1_signature_serialization_roundtrip() {
        let kp = KeyPair::generate_secp256k1();
        let sig = kp.sign(&hash_bytes(b"test")).unwrap();
        let json = serde_json::to_string(&sig).expect("serialize");
        let recovered: Signature = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(sig, recovered);
    }
}
