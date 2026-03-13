//! Digital signature primitives for ARC Chain.
//!
//! Four signature schemes:
//! - **Ed25519**: Fast agent/native transactions (~4-9M batch verifications/sec)
//! - **Secp256k1**: ETH-compatible operations (MetaMask, bridge verification)
//! - **ML-DSA-65**: Post-quantum signatures (NIST FIPS 204, lattice-based)
//! - **Falcon-512**: Post-quantum signatures (NIST round-3, hash-based lattice)
//!
//! Address derivation:
//! - Ed25519: `address = BLAKE3(public_key)[0..32]`
//! - Secp256k1: `address = BLAKE3(uncompressed_pubkey[1..65])[0..32]`
//! - ML-DSA-65: `address = BLAKE3(public_key)[0..32]`
//! - Falcon-512: `address = BLAKE3(public_key)[0..32]`

use crate::hash::{Hash256, hash_bytes};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── ML-DSA-65 constants (FIPS 204) ──────────────────────────────────────────

/// ML-DSA-65 public key length in bytes.
pub const ML_DSA_PK_LEN: usize = fips204::ml_dsa_65::PK_LEN;
/// ML-DSA-65 secret key length in bytes.
pub const ML_DSA_SK_LEN: usize = fips204::ml_dsa_65::SK_LEN;
/// ML-DSA-65 signature length in bytes.
pub const ML_DSA_SIG_LEN: usize = fips204::ml_dsa_65::SIG_LEN;

// ── Falcon-512 constants ────────────────────────────────────────────────────

/// Falcon-512 public key length in bytes.
pub const FALCON_PK_LEN: usize = 897;
/// Falcon-512 secret key length in bytes.
pub const FALCON_SK_LEN: usize = 1281;
/// Falcon-512 maximum signature length in bytes.
/// Falcon signatures are variable-length (up to 752 bytes).
pub const FALCON_SIG_MAX_LEN: usize = 752;

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
/// ML-DSA-65 signatures carry the public key (1952 + 3309 = 5261 bytes total).
/// Falcon-512 signatures carry the public key (897 + up to 752 = up to 1649 bytes total).
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
    /// ML-DSA-65 post-quantum signature (NIST FIPS 204).
    MlDsa65 {
        /// 1952-byte ML-DSA-65 public key.
        public_key: Vec<u8>,
        /// 3309-byte ML-DSA-65 signature.
        signature: Vec<u8>,
    },
    /// Falcon-512 post-quantum signature (NTRU lattice-based).
    Falcon512 {
        /// 897-byte Falcon-512 public key.
        public_key: Vec<u8>,
        /// Variable-length Falcon-512 signature (up to 752 bytes).
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
    ///
    /// For ML-DSA-65: checks that `public_key` hashes to `expected_address`,
    /// then verifies the lattice-based signature over `message_hash`.
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

            Signature::MlDsa65 {
                public_key,
                signature,
            } => {
                use fips204::traits::{SerDes, Verifier};

                // Length checks
                if signature.len() != ML_DSA_SIG_LEN {
                    return Err(SignatureError::InvalidLength {
                        expected: ML_DSA_SIG_LEN,
                        got: signature.len(),
                    });
                }
                if public_key.len() != ML_DSA_PK_LEN {
                    return Err(SignatureError::InvalidPublicKey);
                }

                // 1. Public key must hash to the expected address
                let derived = address_from_ml_dsa_pubkey(public_key);
                if derived != *expected_address {
                    return Err(SignatureError::AddressMismatch {
                        expected: expected_address.to_hex(),
                        got: derived.to_hex(),
                    });
                }

                // 2. Reconstruct public key and verify signature
                let pk_arr: [u8; ML_DSA_PK_LEN] = public_key
                    .as_slice()
                    .try_into()
                    .map_err(|_| SignatureError::InvalidPublicKey)?;
                let pk = fips204::ml_dsa_65::PublicKey::try_from_bytes(pk_arr)
                    .map_err(|_| SignatureError::InvalidPublicKey)?;
                let sig_arr: [u8; ML_DSA_SIG_LEN] = signature
                    .as_slice()
                    .try_into()
                    .map_err(|_| SignatureError::InvalidSignature)?;

                if pk.verify(message_hash.as_bytes(), &sig_arr, &[]) {
                    Ok(())
                } else {
                    Err(SignatureError::InvalidSignature)
                }
            }

            Signature::Falcon512 {
                public_key,
                signature,
            } => {
                use pqcrypto_traits::sign::{
                    PublicKey as PqPublicKey,
                    DetachedSignature as PqDetachedSignature,
                };

                // Length checks
                if signature.len() > FALCON_SIG_MAX_LEN {
                    return Err(SignatureError::InvalidLength {
                        expected: FALCON_SIG_MAX_LEN,
                        got: signature.len(),
                    });
                }
                if public_key.len() != FALCON_PK_LEN {
                    return Err(SignatureError::InvalidPublicKey);
                }

                // 1. Public key must hash to the expected address
                let derived = address_from_falcon_pubkey(public_key);
                if derived != *expected_address {
                    return Err(SignatureError::AddressMismatch {
                        expected: expected_address.to_hex(),
                        got: derived.to_hex(),
                    });
                }

                // 2. Reconstruct public key and detached signature, then verify
                let pk = pqcrypto_falcon::falcon512::PublicKey::from_bytes(public_key)
                    .map_err(|_| SignatureError::InvalidPublicKey)?;
                let sig = pqcrypto_falcon::falcon512::DetachedSignature::from_bytes(signature)
                    .map_err(|_| SignatureError::InvalidSignature)?;

                pqcrypto_falcon::falcon512::verify_detached_signature(&sig, message_hash.as_bytes(), &pk)
                    .map_err(|_| SignatureError::InvalidSignature)
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
#[derive(Clone)]
pub enum KeyPair {
    /// Ed25519 key pair — fast, native operations.
    Ed25519(ed25519_dalek::SigningKey),
    /// Secp256k1 key pair — ETH-compatible operations.
    Secp256k1(k256::ecdsa::SigningKey),
    /// ML-DSA-65 key pair — quantum-resistant operations (NIST FIPS 204).
    /// Stored as serialized bytes because fips204 types don't implement Debug/Serialize.
    MlDsa65 {
        /// 4032-byte serialized private key.
        sk_bytes: Vec<u8>,
        /// 1952-byte serialized public key.
        pk_bytes: Vec<u8>,
    },
    /// Falcon-512 key pair — post-quantum NTRU lattice signatures.
    /// Stored as serialized bytes (pqcrypto types don't implement Debug/Serialize by default).
    Falcon512 {
        /// 1281-byte serialized secret key.
        sk_bytes: Vec<u8>,
        /// 897-byte serialized public key.
        pk_bytes: Vec<u8>,
    },
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

    /// Generate a new random ML-DSA-65 key pair (post-quantum).
    pub fn generate_ml_dsa() -> Self {
        use fips204::traits::SerDes;
        let (pk, sk) = fips204::ml_dsa_65::try_keygen()
            .expect("ML-DSA-65 keygen failed (RNG error)");
        KeyPair::MlDsa65 {
            sk_bytes: sk.into_bytes().to_vec(),
            pk_bytes: pk.into_bytes().to_vec(),
        }
    }

    /// Generate a new random Falcon-512 key pair (post-quantum).
    pub fn generate_falcon512() -> Self {
        use pqcrypto_traits::sign::{PublicKey as PqPk, SecretKey as PqSk};
        let (pk, sk) = pqcrypto_falcon::falcon512::keypair();
        KeyPair::Falcon512 {
            sk_bytes: sk.as_bytes().to_vec(),
            pk_bytes: pk.as_bytes().to_vec(),
        }
    }

    /// Derive the ARC chain address from this key pair.
    ///
    /// Ed25519: `BLAKE3(public_key)`
    /// Secp256k1: `BLAKE3(uncompressed_point[1..65])`
    /// ML-DSA-65: `BLAKE3(public_key)`
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
            KeyPair::MlDsa65 { pk_bytes, .. } => {
                address_from_ml_dsa_pubkey(pk_bytes)
            }
            KeyPair::Falcon512 { pk_bytes, .. } => {
                address_from_falcon_pubkey(pk_bytes)
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
            KeyPair::MlDsa65 { sk_bytes, pk_bytes } => {
                use fips204::traits::{SerDes, Signer};

                let sk_arr: [u8; ML_DSA_SK_LEN] = sk_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SignatureError::SigningFailed(
                        "invalid ML-DSA secret key length".into(),
                    ))?;
                let sk = fips204::ml_dsa_65::PrivateKey::try_from_bytes(sk_arr)
                    .map_err(|e| SignatureError::SigningFailed(e.to_string()))?;
                let sig = sk
                    .try_sign(message_hash.as_bytes(), &[])
                    .map_err(|e| SignatureError::SigningFailed(e.to_string()))?;

                Ok(Signature::MlDsa65 {
                    public_key: pk_bytes.clone(),
                    signature: sig.to_vec(),
                })
            }
            KeyPair::Falcon512 { sk_bytes, pk_bytes } => {
                use pqcrypto_traits::sign::{
                    SecretKey as PqSk,
                    DetachedSignature as PqDetachedSig,
                };

                let sk = pqcrypto_falcon::falcon512::SecretKey::from_bytes(sk_bytes)
                    .map_err(|e| SignatureError::SigningFailed(format!(
                        "invalid Falcon-512 secret key: {}", e
                    )))?;
                let sig = pqcrypto_falcon::falcon512::detached_sign(
                    message_hash.as_bytes(),
                    &sk,
                );

                Ok(Signature::Falcon512 {
                    public_key: pk_bytes.clone(),
                    signature: sig.as_bytes().to_vec(),
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
            KeyPair::MlDsa65 { pk_bytes, .. } => pk_bytes.clone(),
            KeyPair::Falcon512 { pk_bytes, .. } => pk_bytes.clone(),
        }
    }
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyPair::Ed25519(_) => write!(f, "KeyPair::Ed25519(<redacted>)"),
            KeyPair::Secp256k1(_) => write!(f, "KeyPair::Secp256k1(<redacted>)"),
            KeyPair::MlDsa65 { .. } => write!(f, "KeyPair::MlDsa65(<redacted>)"),
            KeyPair::Falcon512 { .. } => write!(f, "KeyPair::Falcon512(<redacted>)"),
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

/// Derive an ARC address from an ML-DSA-65 public key.
/// `address = BLAKE3(public_key)[0..32]`
#[inline]
pub fn address_from_ml_dsa_pubkey(public_key: &[u8]) -> Hash256 {
    debug_assert_eq!(
        public_key.len(),
        ML_DSA_PK_LEN,
        "expected {}-byte ML-DSA-65 pubkey",
        ML_DSA_PK_LEN
    );
    hash_bytes(public_key)
}

/// Derive an ARC address from a Falcon-512 public key.
/// `address = BLAKE3(public_key)[0..32]`
#[inline]
pub fn address_from_falcon_pubkey(public_key: &[u8]) -> Hash256 {
    debug_assert_eq!(
        public_key.len(),
        FALCON_PK_LEN,
        "expected {}-byte Falcon-512 pubkey",
        FALCON_PK_LEN
    );
    hash_bytes(public_key)
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

/// Batch verify N ML-DSA-65 signatures using parallel individual verification.
///
/// ML-DSA doesn't have native batch verification like Ed25519, but the NTT-based
/// verification is CPU-bound and parallelizes well across cores with Rayon.
///
/// All three slices must have the same length.
pub fn batch_verify_ml_dsa(
    messages: &[&[u8]],
    signatures: &[[u8; ML_DSA_SIG_LEN]],
    public_keys: &[[u8; ML_DSA_PK_LEN]],
) -> Result<(), SignatureError> {
    use fips204::traits::{SerDes, Verifier};
    use rayon::prelude::*;

    let all_valid = messages
        .par_iter()
        .zip(signatures.par_iter())
        .zip(public_keys.par_iter())
        .all(|((msg, sig), pk_bytes)| {
            let pk = match fips204::ml_dsa_65::PublicKey::try_from_bytes(*pk_bytes) {
                Ok(pk) => pk,
                Err(_) => return false,
            };
            pk.verify(msg, sig, &[])
        });

    if all_valid {
        Ok(())
    } else {
        Err(SignatureError::BatchVerifyFailed)
    }
}

/// Batch verify N Falcon-512 signatures using parallel individual verification.
///
/// Falcon doesn't have native batch verification, but each verification is
/// CPU-bound and parallelizes well across cores with Rayon.
///
/// All three slices must have the same length.
/// Signatures are variable-length, so they are passed as `Vec<u8>` slices.
pub fn batch_verify_falcon512(
    messages: &[&[u8]],
    signatures: &[&[u8]],
    public_keys: &[&[u8]],
) -> Result<(), SignatureError> {
    use pqcrypto_traits::sign::{
        PublicKey as PqPublicKey,
        DetachedSignature as PqDetachedSignature,
    };
    use rayon::prelude::*;

    let all_valid = messages
        .par_iter()
        .zip(signatures.par_iter())
        .zip(public_keys.par_iter())
        .all(|((msg, sig_bytes), pk_bytes)| {
            let pk = match pqcrypto_falcon::falcon512::PublicKey::from_bytes(pk_bytes) {
                Ok(pk) => pk,
                Err(_) => return false,
            };
            let sig = match pqcrypto_falcon::falcon512::DetachedSignature::from_bytes(sig_bytes) {
                Ok(sig) => sig,
                Err(_) => return false,
            };
            pqcrypto_falcon::falcon512::verify_detached_signature(&sig, msg, &pk).is_ok()
        });

    if all_valid {
        Ok(())
    } else {
        Err(SignatureError::BatchVerifyFailed)
    }
}

// ── Benchmark keypair derivation ─────────────────────────────────────────────

/// Derive a deterministic Ed25519 keypair from a benchmark index.
/// All nodes derive the same keypairs → same genesis → compatible P2P.
pub fn benchmark_keypair(index: u8) -> ed25519_dalek::SigningKey {
    let seed = blake3::derive_key("ARC-chain-benchmark-keypair-v1", &[index]);
    ed25519_dalek::SigningKey::from_bytes(&seed)
}

/// ARC address for benchmark keypair at the given index.
/// `address = BLAKE3(ed25519_public_key)`
pub fn benchmark_address(index: u8) -> Hash256 {
    address_from_ed25519_pubkey(benchmark_keypair(index).verifying_key().as_bytes())
}

// ── Falcon-512 standalone helpers ────────────────────────────────────────────

/// Generate a new Falcon-512 key pair, returning raw bytes.
/// Public key: 897 bytes, Secret key: 1281 bytes.
pub fn falcon_keygen() -> (Vec<u8>, Vec<u8>) {
    use pqcrypto_traits::sign::{PublicKey as PqPk, SecretKey as PqSk};
    let (pk, sk) = pqcrypto_falcon::falcon512::keypair();
    (pk.as_bytes().to_vec(), sk.as_bytes().to_vec())
}

/// Sign a message with a Falcon-512 secret key, returning the detached signature bytes.
pub fn falcon_sign(secret_key: &[u8], message: &[u8]) -> Result<Vec<u8>, SignatureError> {
    use pqcrypto_traits::sign::{SecretKey as PqSk, DetachedSignature as PqDetachedSig};
    let sk = pqcrypto_falcon::falcon512::SecretKey::from_bytes(secret_key)
        .map_err(|e| SignatureError::SigningFailed(format!("invalid Falcon secret key: {}", e)))?;
    let sig = pqcrypto_falcon::falcon512::detached_sign(message, &sk);
    Ok(sig.as_bytes().to_vec())
}

/// Verify a Falcon-512 detached signature against a message and public key.
pub fn falcon_verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    use pqcrypto_traits::sign::{
        PublicKey as PqPublicKey,
        DetachedSignature as PqDetachedSignature,
    };
    let pk = match pqcrypto_falcon::falcon512::PublicKey::from_bytes(public_key) {
        Ok(pk) => pk,
        Err(_) => return false,
    };
    let sig = match pqcrypto_falcon::falcon512::DetachedSignature::from_bytes(signature) {
        Ok(sig) => sig,
        Err(_) => return false,
    };
    pqcrypto_falcon::falcon512::verify_detached_signature(&sig, message, &pk).is_ok()
}

/// Batch verify N Falcon-512 signatures, returning per-element results.
///
/// Unlike `batch_verify_falcon512` which returns a single pass/fail,
/// this returns a `Vec<bool>` indicating which individual signatures are valid.
pub fn falcon_batch_verify(
    keys: &[&[u8]],
    messages: &[&[u8]],
    signatures: &[&[u8]],
) -> Vec<bool> {
    use rayon::prelude::*;
    use pqcrypto_traits::sign::{
        PublicKey as PqPublicKey,
        DetachedSignature as PqDetachedSignature,
    };

    keys.par_iter()
        .zip(messages.par_iter())
        .zip(signatures.par_iter())
        .map(|((pk_bytes, msg), sig_bytes)| {
            let pk = match pqcrypto_falcon::falcon512::PublicKey::from_bytes(pk_bytes) {
                Ok(pk) => pk,
                Err(_) => return false,
            };
            let sig = match pqcrypto_falcon::falcon512::DetachedSignature::from_bytes(sig_bytes) {
                Ok(sig) => sig,
                Err(_) => return false,
            };
            pqcrypto_falcon::falcon512::verify_detached_signature(&sig, msg, &pk).is_ok()
        })
        .collect()
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

    // ── ML-DSA-65 (Post-Quantum) ──

    #[test]
    fn ml_dsa_sign_and_verify() {
        let kp = KeyPair::generate_ml_dsa();
        let address = kp.address();
        let msg = hash_bytes(b"quantum-proof transaction");

        let sig = kp.sign(&msg).expect("sign ok");
        sig.verify(&msg, &address).expect("verify ok");
    }

    #[test]
    fn ml_dsa_wrong_address_fails() {
        let kp = KeyPair::generate_ml_dsa();
        let wrong = hash_bytes(b"wrong address");
        let msg = hash_bytes(b"quantum-proof transaction");

        let sig = kp.sign(&msg).expect("sign ok");
        assert!(sig.verify(&msg, &wrong).is_err());
    }

    #[test]
    fn ml_dsa_wrong_message_fails() {
        let kp = KeyPair::generate_ml_dsa();
        let address = kp.address();
        let msg = hash_bytes(b"msg1");
        let wrong = hash_bytes(b"msg2");

        let sig = kp.sign(&msg).expect("sign ok");
        assert!(sig.verify(&wrong, &address).is_err());
    }

    #[test]
    fn ml_dsa_address_is_deterministic() {
        let kp = KeyPair::generate_ml_dsa();
        assert_eq!(kp.address(), kp.address());
    }

    #[test]
    fn ml_dsa_public_key_bytes_correct_length() {
        let kp = KeyPair::generate_ml_dsa();
        assert_eq!(kp.public_key_bytes().len(), ML_DSA_PK_LEN);
    }

    #[test]
    fn ml_dsa_signature_correct_length() {
        let kp = KeyPair::generate_ml_dsa();
        let msg = hash_bytes(b"test");
        let sig = kp.sign(&msg).expect("sign ok");
        match sig {
            Signature::MlDsa65 { signature, public_key } => {
                assert_eq!(signature.len(), ML_DSA_SIG_LEN);
                assert_eq!(public_key.len(), ML_DSA_PK_LEN);
            }
            _ => panic!("expected MlDsa65 signature"),
        }
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

    #[test]
    fn batch_verify_ml_dsa_5_signatures() {
        let count = 5;
        let mut messages: Vec<Vec<u8>> = Vec::with_capacity(count);
        let mut signatures: Vec<[u8; ML_DSA_SIG_LEN]> = Vec::with_capacity(count);
        let mut public_keys: Vec<[u8; ML_DSA_PK_LEN]> = Vec::with_capacity(count);

        for i in 0..count {
            let kp = KeyPair::generate_ml_dsa();
            let msg = format!("quantum message {}", i).into_bytes();
            let msg_hash = hash_bytes(&msg);
            let sig = kp.sign(&msg_hash).expect("sign ok");

            match sig {
                Signature::MlDsa65 {
                    public_key,
                    signature,
                } => {
                    messages.push(msg_hash.as_bytes().to_vec());
                    signatures.push(signature.as_slice().try_into().unwrap());
                    public_keys.push(public_key.as_slice().try_into().unwrap());
                }
                _ => panic!("expected MlDsa65"),
            }
        }

        let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();
        batch_verify_ml_dsa(&msg_refs, &signatures, &public_keys).expect("batch verify ok");
    }

    #[test]
    fn batch_verify_ml_dsa_fails_on_bad_signature() {
        let count = 3;
        let mut messages: Vec<Vec<u8>> = Vec::with_capacity(count);
        let mut signatures: Vec<[u8; ML_DSA_SIG_LEN]> = Vec::with_capacity(count);
        let mut public_keys: Vec<[u8; ML_DSA_PK_LEN]> = Vec::with_capacity(count);

        for i in 0..count {
            let kp = KeyPair::generate_ml_dsa();
            let msg = format!("quantum message {}", i).into_bytes();
            let msg_hash = hash_bytes(&msg);
            let sig = kp.sign(&msg_hash).expect("sign ok");

            match sig {
                Signature::MlDsa65 {
                    public_key,
                    signature,
                } => {
                    messages.push(msg_hash.as_bytes().to_vec());
                    signatures.push(signature.as_slice().try_into().unwrap());
                    public_keys.push(public_key.as_slice().try_into().unwrap());
                }
                _ => panic!("expected MlDsa65"),
            }
        }

        // Corrupt: use a different message for verification of entry 1
        messages[1] = hash_bytes(b"tampered").as_bytes().to_vec();

        let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();
        assert!(batch_verify_ml_dsa(&msg_refs, &signatures, &public_keys).is_err());
    }

    // ── Falcon-512 (Post-Quantum) ──

    #[test]
    fn falcon512_sign_and_verify() {
        let kp = KeyPair::generate_falcon512();
        let address = kp.address();
        let msg = hash_bytes(b"falcon quantum-proof transaction");

        let sig = kp.sign(&msg).expect("sign ok");
        sig.verify(&msg, &address).expect("verify ok");
    }

    #[test]
    fn falcon512_wrong_address_fails() {
        let kp = KeyPair::generate_falcon512();
        let wrong = hash_bytes(b"wrong address");
        let msg = hash_bytes(b"falcon quantum-proof transaction");

        let sig = kp.sign(&msg).expect("sign ok");
        assert!(sig.verify(&msg, &wrong).is_err());
    }

    #[test]
    fn falcon512_wrong_message_fails() {
        let kp = KeyPair::generate_falcon512();
        let address = kp.address();
        let msg = hash_bytes(b"msg1");
        let wrong = hash_bytes(b"msg2");

        let sig = kp.sign(&msg).expect("sign ok");
        assert!(sig.verify(&wrong, &address).is_err());
    }

    #[test]
    fn falcon512_address_is_deterministic() {
        let kp = KeyPair::generate_falcon512();
        assert_eq!(kp.address(), kp.address());
    }

    #[test]
    fn falcon512_public_key_bytes_correct_length() {
        let kp = KeyPair::generate_falcon512();
        assert_eq!(kp.public_key_bytes().len(), FALCON_PK_LEN);
    }

    #[test]
    fn falcon512_signature_correct_length() {
        let kp = KeyPair::generate_falcon512();
        let msg = hash_bytes(b"test");
        let sig = kp.sign(&msg).expect("sign ok");
        match sig {
            Signature::Falcon512 { signature, public_key } => {
                assert!(signature.len() <= FALCON_SIG_MAX_LEN);
                assert!(signature.len() > 0);
                assert_eq!(public_key.len(), FALCON_PK_LEN);
            }
            _ => panic!("expected Falcon512 signature"),
        }
    }

    #[test]
    fn falcon512_signature_serialization_roundtrip() {
        let kp = KeyPair::generate_falcon512();
        let sig = kp.sign(&hash_bytes(b"test")).unwrap();
        let json = serde_json::to_string(&sig).expect("serialize");
        let recovered: Signature = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(sig, recovered);
    }

    #[test]
    fn falcon512_is_not_null() {
        let kp = KeyPair::generate_falcon512();
        let sig = kp.sign(&hash_bytes(b"x")).unwrap();
        assert!(!sig.is_null());
    }

    // ── Falcon-512 standalone helpers ──

    #[test]
    fn falcon_keygen_produces_valid_keys() {
        let (pk, sk) = falcon_keygen();
        assert_eq!(pk.len(), FALCON_PK_LEN);
        assert_eq!(sk.len(), FALCON_SK_LEN);
    }

    #[test]
    fn falcon_sign_and_verify_standalone() {
        let (pk, sk) = falcon_keygen();
        let msg = b"standalone falcon test";
        let sig = falcon_sign(&sk, msg).expect("sign ok");
        assert!(falcon_verify(&pk, msg, &sig));
    }

    #[test]
    fn falcon_verify_rejects_wrong_message() {
        let (pk, sk) = falcon_keygen();
        let sig = falcon_sign(&sk, b"correct").expect("sign ok");
        assert!(!falcon_verify(&pk, b"wrong", &sig));
    }

    #[test]
    fn falcon_batch_verify_all_valid() {
        let count = 5;
        let mut pks = Vec::with_capacity(count);
        let mut msgs: Vec<Vec<u8>> = Vec::with_capacity(count);
        let mut sigs = Vec::with_capacity(count);

        for i in 0..count {
            let (pk, sk) = falcon_keygen();
            let msg = format!("falcon batch message {}", i).into_bytes();
            let sig = falcon_sign(&sk, &msg).expect("sign ok");
            pks.push(pk);
            msgs.push(msg);
            sigs.push(sig);
        }

        let pk_refs: Vec<&[u8]> = pks.iter().map(|p| p.as_slice()).collect();
        let msg_refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
        let sig_refs: Vec<&[u8]> = sigs.iter().map(|s| s.as_slice()).collect();

        let results = falcon_batch_verify(&pk_refs, &msg_refs, &sig_refs);
        assert!(results.iter().all(|&v| v));
    }

    #[test]
    fn falcon_batch_verify_detects_bad_signature() {
        let count = 3;
        let mut pks = Vec::with_capacity(count);
        let mut msgs: Vec<Vec<u8>> = Vec::with_capacity(count);
        let mut sigs = Vec::with_capacity(count);

        for i in 0..count {
            let (pk, sk) = falcon_keygen();
            let msg = format!("falcon batch message {}", i).into_bytes();
            let sig = falcon_sign(&sk, &msg).expect("sign ok");
            pks.push(pk);
            msgs.push(msg);
            sigs.push(sig);
        }

        // Corrupt message at index 1
        msgs[1] = b"tampered".to_vec();

        let pk_refs: Vec<&[u8]> = pks.iter().map(|p| p.as_slice()).collect();
        let msg_refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
        let sig_refs: Vec<&[u8]> = sigs.iter().map(|s| s.as_slice()).collect();

        let results = falcon_batch_verify(&pk_refs, &msg_refs, &sig_refs);
        assert!(results[0], "first should be valid");
        assert!(!results[1], "second should be invalid (tampered)");
        assert!(results[2], "third should be valid");
    }

    // ── Batch verification (Falcon-512 aggregate) ──

    #[test]
    fn batch_verify_falcon512_5_signatures() {
        let count = 5;
        let mut messages: Vec<Vec<u8>> = Vec::with_capacity(count);
        let mut signatures: Vec<Vec<u8>> = Vec::with_capacity(count);
        let mut public_keys: Vec<Vec<u8>> = Vec::with_capacity(count);

        for i in 0..count {
            let kp = KeyPair::generate_falcon512();
            let msg = format!("falcon quantum message {}", i).into_bytes();
            let msg_hash = hash_bytes(&msg);
            let sig = kp.sign(&msg_hash).expect("sign ok");

            match sig {
                Signature::Falcon512 { public_key, signature } => {
                    messages.push(msg_hash.as_bytes().to_vec());
                    signatures.push(signature);
                    public_keys.push(public_key);
                }
                _ => panic!("expected Falcon512"),
            }
        }

        let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();
        let sig_refs: Vec<&[u8]> = signatures.iter().map(|s| s.as_slice()).collect();
        let pk_refs: Vec<&[u8]> = public_keys.iter().map(|p| p.as_slice()).collect();

        batch_verify_falcon512(&msg_refs, &sig_refs, &pk_refs).expect("batch verify ok");
    }

    #[test]
    fn batch_verify_falcon512_fails_on_bad_signature() {
        let count = 3;
        let mut messages: Vec<Vec<u8>> = Vec::with_capacity(count);
        let mut signatures: Vec<Vec<u8>> = Vec::with_capacity(count);
        let mut public_keys: Vec<Vec<u8>> = Vec::with_capacity(count);

        for i in 0..count {
            let kp = KeyPair::generate_falcon512();
            let msg = format!("falcon quantum message {}", i).into_bytes();
            let msg_hash = hash_bytes(&msg);
            let sig = kp.sign(&msg_hash).expect("sign ok");

            match sig {
                Signature::Falcon512 { public_key, signature } => {
                    messages.push(msg_hash.as_bytes().to_vec());
                    signatures.push(signature);
                    public_keys.push(public_key);
                }
                _ => panic!("expected Falcon512"),
            }
        }

        // Corrupt: tamper with a message
        messages[1] = hash_bytes(b"tampered").as_bytes().to_vec();

        let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();
        let sig_refs: Vec<&[u8]> = signatures.iter().map(|s| s.as_slice()).collect();
        let pk_refs: Vec<&[u8]> = public_keys.iter().map(|p| p.as_slice()).collect();

        assert!(batch_verify_falcon512(&msg_refs, &sig_refs, &pk_refs).is_err());
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

    #[test]
    fn ml_dsa_signature_is_not_null() {
        let kp = KeyPair::generate_ml_dsa();
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
    fn all_four_schemes_produce_different_addresses() {
        let ed = KeyPair::generate_ed25519();
        let secp = KeyPair::generate_secp256k1();
        let ml = KeyPair::generate_ml_dsa();
        let falcon = KeyPair::generate_falcon512();
        assert_ne!(ed.address(), secp.address());
        assert_ne!(ed.address(), ml.address());
        assert_ne!(ed.address(), falcon.address());
        assert_ne!(secp.address(), ml.address());
        assert_ne!(secp.address(), falcon.address());
        assert_ne!(ml.address(), falcon.address());
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

    #[test]
    fn ml_dsa_signature_serialization_roundtrip() {
        let kp = KeyPair::generate_ml_dsa();
        let sig = kp.sign(&hash_bytes(b"test")).unwrap();
        let json = serde_json::to_string(&sig).expect("serialize");
        let recovered: Signature = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(sig, recovered);
    }
}
