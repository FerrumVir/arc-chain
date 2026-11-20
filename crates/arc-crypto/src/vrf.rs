use crate::hash::{Hash256, hash_bytes};
use crate::signature::{KeyPair, Signature, SignatureError};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VrfProof {
    pub gamma: Hash256,
    pub signature: Signature,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VrfOutput(pub Hash256);

pub fn vrf_prove(keypair: &KeyPair, alpha: &[u8]) -> Result<(VrfProof, VrfOutput), SignatureError> {
    let gamma_input = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-gamma-v1");
        h.update(&keypair.public_key_bytes());
        h.update(alpha);
        Hash256(*h.finalize().as_bytes())
    };

    let gamma_sig = keypair.sign(&gamma_input)?;

    let gamma = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-gamma-derive-v1");
        match &gamma_sig {
            Signature::Ed25519 {
                public_key,
                signature,
            } => {
                h.update(&[0u8]);
                h.update(public_key);
                h.update(signature);
            }
            Signature::Secp256k1 { signature } => {
                h.update(&[1u8]);
                h.update(signature);
            }
            Signature::MlDsa65 {
                public_key,
                signature,
            } => {
                h.update(&[2u8]);
                h.update(public_key);
                h.update(signature);
            }
            Signature::Falcon512 {
                public_key,
                signature,
            } => {
                h.update(&[3u8]);
                h.update(public_key);
                h.update(signature);
            }
        }
        Hash256(*h.finalize().as_bytes())
    };

    let alpha_hash = hash_bytes(alpha);
    let proof_msg = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-proof-v1");
        h.update(gamma.as_ref());
        h.update(alpha_hash.as_ref());
        Hash256(*h.finalize().as_bytes())
    };
    let signature = keypair.sign(&proof_msg)?;

    let output = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-output-v1");
        h.update(gamma.as_ref());
        Hash256(*h.finalize().as_bytes())
    };

    Ok((VrfProof { gamma, signature }, VrfOutput(output)))
}

pub fn vrf_verify(
    public_key_address: &Hash256,
    alpha: &[u8],
    proof: &VrfProof,
) -> Result<VrfOutput, SignatureError> {
    let alpha_hash = hash_bytes(alpha);
    let proof_msg = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-proof-v1");
        h.update(proof.gamma.as_ref());
        h.update(alpha_hash.as_ref());
        Hash256(*h.finalize().as_bytes())
    };

    proof.signature.verify(&proof_msg, public_key_address)?;

    let output = {
        let mut h = blake3::Hasher::new_derive_key("ARC-vrf-output-v1");
        h.update(proof.gamma.as_ref());
        Hash256(*h.finalize().as_bytes())
    };

    Ok(VrfOutput(output))
}

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

        assert_eq!(
            output, verified,
            "prove and verify must yield the same output"
        );
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

        let json = serde_json::to_string(&proof).expect("serialize");
        let recovered: VrfProof = serde_json::from_str(&json).expect("deserialize");

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
