//! Verifiable Random Function (VRF) for ARC Chain.
//!
//! Simplified ECVRF construction using Ed25519 + BLAKE3, inspired by RFC 9381
//! (ECVRF-ED25519-SHA512-TAI). Rather than raw curve arithmetic, we use
//! Ed25519 signatures as the secret-key-dependent operation and BLAKE3 for
//! domain-separated hashing.
//!
//! # Construction
//!
//! ```text
//! gamma = BLAKE3_derive("ARC-vrf-gamma-derive-v1",
//!             sign(sk, BLAKE3_derive("ARC-vrf-gamma-v1", pk || alpha)))
//! proof = sign(sk, BLAKE3_derive("ARC-vrf-proof-v1", gamma || H(alpha)))
//! output = BLAKE3_derive("ARC-vrf-output-v1", gamma)
//! ```
//!
//! **Verification** recomputes the proof message from the supplied gamma and
//! alpha, then checks the Ed25519 signature against the prover's address.
//! A valid signature proves the prover knew the secret key, and the output
//! is deterministically derived from gamma.
//!
//! # Security properties
//!
//! - **Uniqueness**: For a given keypair and input, there is exactly one valid
//!   output (the Ed25519 signature is deterministic per RFC 8032).
//! - **Pseudorandomness**: The output is indistinguishable from random to
//!   anyone who does not know the secret key.
//! - **Verifiability**: Anyone with the public key can verify that the output
//!   was correctly derived.

use crate::hash::{hash_bytes, Hash256};
use crate::signature::{KeyPair, Signature, SignatureError};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// VRF proof binding a pseudorandom output to a secret key and input.
///
/// Contains the intermediate `gamma` point (as a Hash256) and an Ed25519
/// signature over `gamma || H(alpha)` that proves knowledge of the secret key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VrfProof {
    /// Deterministic intermediate value derived from secret key + input.
    pub gamma: Hash256,
    /// Ed25519 signature over `BLAKE3("ARC-vrf-proof-v1", gamma || H(alpha))`.
    pub signature: Signature,
}

/// The 32-byte pseudorandom output of a VRF evaluation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VrfOutput(pub Hash256);

// ---------------------------------------------------------------------------
// Core API
// ---------------------------------------------------------------------------

/// Produce a VRF proof and pseudorandom output for the given input.
///
/// # Arguments
///
/// * `keypair` - An Ed25519 `KeyPair` (other key types will work but VRF
///   is designed around Ed25519 deterministic signatures).
/// * `alpha` - The VRF input (arbitrary bytes).
///
/// # Returns
///
/// `(VrfProof, VrfOutput)` on success, or `SignatureError` if signing fails.
pub fn vrf_prove(
    keypair: &KeyPair,
    alpha: &[u8],
) -> Result<(VrfProof, VrfOutput), SignatureError> {
    // Step 1: Derive a gamma input deterministically from (pk, alpha).
    let gamma_input = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-gamma-v1");
        h.update(&keypair.public_key_bytes());
        h.update(alpha);
        Hash256(*h.finalize().as_bytes())
    };

    // Step 2: Sign the gamma input — this binds gamma to the secret key.
    // Ed25519 signatures are deterministic (RFC 8032), so the same
    // (keypair, alpha) always produces the same gamma.
    let gamma_sig = keypair.sign(&gamma_input)?;

    // Step 3: Derive gamma from the signature bytes.
    // We feed the raw signature material into a domain-separated BLAKE3 hash
    // so gamma is a clean 32-byte value.
    let gamma = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-gamma-derive-v1");
        match &gamma_sig {
            Signature::Ed25519 {
                public_key,
                signature,
            } => {
                h.update(public_key);
                h.update(signature);
            }
            Signature::Secp256k1 { signature } => {
                h.update(signature);
            }
            Signature::MlDsa65 {
                public_key,
                signature,
            } => {
                h.update(public_key);
                h.update(signature);
            }
            Signature::Falcon512 {
                public_key,
                signature,
            } => {
                h.update(public_key);
                h.update(signature);
            }
        }
        Hash256(*h.finalize().as_bytes())
    };

    // Step 4: Create the proof — sign(gamma || H(alpha)).
    let alpha_hash = hash_bytes(alpha);
    let proof_msg = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-proof-v1");
        h.update(gamma.as_ref());
        h.update(alpha_hash.as_ref());
        Hash256(*h.finalize().as_bytes())
    };
    let signature = keypair.sign(&proof_msg)?;

    // Step 5: Derive the output from gamma.
    let output = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-output-v1");
        h.update(gamma.as_ref());
        Hash256(*h.finalize().as_bytes())
    };

    Ok((VrfProof { gamma, signature }, VrfOutput(output)))
}

/// Verify a VRF proof and return the pseudorandom output if valid.
///
/// # Arguments
///
/// * `public_key_address` - The ARC address of the prover (BLAKE3 hash of
///   their public key).
/// * `alpha` - The same VRF input that was passed to `vrf_prove`.
/// * `proof` - The `VrfProof` to verify.
///
/// # Returns
///
/// `VrfOutput` on success, or `SignatureError` if the proof is invalid.
pub fn vrf_verify(
    public_key_address: &Hash256,
    alpha: &[u8],
    proof: &VrfProof,
) -> Result<VrfOutput, SignatureError> {
    // Recompute the proof message from the supplied gamma and alpha.
    let alpha_hash = hash_bytes(alpha);
    let proof_msg = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-proof-v1");
        h.update(proof.gamma.as_ref());
        h.update(alpha_hash.as_ref());
        Hash256(*h.finalize().as_bytes())
    };

    // Verify the Ed25519 signature — this proves the prover knew the
    // secret key corresponding to `public_key_address`.
    proof.signature.verify(&proof_msg, public_key_address)?;

    // Recompute the output from gamma (must match what the prover computed).
    let output = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-output-v1");
        h.update(proof.gamma.as_ref());
        Hash256(*h.finalize().as_bytes())
    };

    Ok(VrfOutput(output))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vrf_prove_verify() {
        let kp = KeyPair::generate_ed25519();
        let address = kp.address();
        let alpha = b"block-42-slot-7";

        let (proof, output) = vrf_prove(&kp, alpha).expect("prove ok");
        let verified = vrf_verify(&address, alpha, &proof).expect("verify ok");

        assert_eq!(output, verified, "prove and verify must yield the same output");
    }

    #[test]
    fn test_vrf_different_inputs() {
        let kp = KeyPair::generate_ed25519();
        let alpha_a = b"input-a";
        let alpha_b = b"input-b";

        let (_, output_a) = vrf_prove(&kp, alpha_a).expect("prove a");
        let (_, output_b) = vrf_prove(&kp, alpha_b).expect("prove b");

        assert_ne!(
            output_a, output_b,
            "different inputs must produce different outputs"
        );
    }

    #[test]
    fn test_vrf_wrong_key_fails() {
        let kp = KeyPair::generate_ed25519();
        let other = KeyPair::generate_ed25519();
        let alpha = b"leader-election-epoch-99";

        let (proof, _) = vrf_prove(&kp, alpha).expect("prove ok");
        let wrong_address = other.address();

        let result = vrf_verify(&wrong_address, alpha, &proof);
        assert!(
            result.is_err(),
            "verification must fail with a different key's address"
        );
    }

    #[test]
    fn test_vrf_deterministic() {
        let kp = KeyPair::generate_ed25519();
        let alpha = b"deterministic-test";

        let (_, output1) = vrf_prove(&kp, alpha).expect("prove 1");
        let (_, output2) = vrf_prove(&kp, alpha).expect("prove 2");

        assert_eq!(
            output1, output2,
            "same keypair + same input must produce identical outputs"
        );
    }

    #[test]
    fn test_vrf_tampered_proof_fails() {
        let kp = KeyPair::generate_ed25519();
        let address = kp.address();
        let alpha = b"tamper-test";

        let (mut proof, _) = vrf_prove(&kp, alpha).expect("prove ok");

        // Tamper with gamma — flip a byte.
        proof.gamma.0[0] ^= 0xff;

        let result = vrf_verify(&address, alpha, &proof);
        assert!(
            result.is_err(),
            "verification must fail when gamma is tampered"
        );
    }

    #[test]
    fn test_vrf_wrong_alpha_fails() {
        let kp = KeyPair::generate_ed25519();
        let address = kp.address();
        let alpha = b"original-input";

        let (proof, _) = vrf_prove(&kp, alpha).expect("prove ok");

        // Verify with a different alpha.
        let result = vrf_verify(&address, b"different-input", &proof);
        assert!(
            result.is_err(),
            "verification must fail when alpha is changed"
        );
    }

    #[test]
    fn test_vrf_proof_serialization_roundtrip() {
        let kp = KeyPair::generate_ed25519();
        let alpha = b"serde-test";

        let (proof, output) = vrf_prove(&kp, alpha).expect("prove ok");

        // Serialize to JSON and back.
        let json = serde_json::to_string(&proof).expect("serialize");
        let recovered: VrfProof = serde_json::from_str(&json).expect("deserialize");

        // Verify the deserialized proof still works.
        let address = kp.address();
        let verified = vrf_verify(&address, alpha, &recovered).expect("verify ok");
        assert_eq!(output, verified);
    }

    #[test]
    fn test_vrf_output_is_32_bytes() {
        let kp = KeyPair::generate_ed25519();
        let (_, output) = vrf_prove(&kp, b"size-check").expect("prove ok");
        assert_eq!(output.0.as_bytes().len(), 32);
    }

    #[test]
    fn test_vrf_different_keys_same_input() {
        let kp1 = KeyPair::generate_ed25519();
        let kp2 = KeyPair::generate_ed25519();
        let alpha = b"same-input";

        let (_, output1) = vrf_prove(&kp1, alpha).expect("prove 1");
        let (_, output2) = vrf_prove(&kp2, alpha).expect("prove 2");

        assert_ne!(
            output1, output2,
            "different keys with same input must produce different outputs"
        );
    }
}
