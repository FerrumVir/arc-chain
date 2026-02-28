use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use rand::rngs::OsRng;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

/// Second generator point H for Pedersen commitments.
/// Derived deterministically so nobody knows the discrete log relationship to G.
fn generator_h() -> RistrettoPoint {
    let mut hasher = Sha512::new();
    hasher.update(b"ARC-chain-pedersen-generator-H-v1");
    RistrettoPoint::from_hash(hasher)
}

/// A Pedersen commitment: C = v*G + r*H
/// Hides the value v with randomness r. Homomorphic: C(a) + C(b) = C(a+b).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PedersenCommitment {
    /// Compressed Ristretto point (32 bytes).
    pub point: [u8; 32],
    /// The blinding factor (secret, kept by committer).
    #[serde(skip_serializing)]
    pub blinding: [u8; 32],
}

/// A proof that a Pedersen commitment contains a specific value.
/// The committer reveals (value, blinding) and the verifier checks C == v*G + r*H.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PedersenProof {
    pub commitment: [u8; 32],
    pub value: u64,
    pub blinding: [u8; 32],
}

/// Commit to a value using a Pedersen commitment.
/// Returns the commitment and stores the blinding factor.
pub fn commit_value(value: u64) -> PedersenCommitment {
    let h = generator_h();
    let v = Scalar::from(value);
    let r = Scalar::random(&mut OsRng);
    let point = v * RISTRETTO_BASEPOINT_POINT + r * h;
    let compressed = point.compress();

    PedersenCommitment {
        point: compressed.to_bytes(),
        blinding: r.to_bytes(),
    }
}

/// Commit with a known blinding factor (for deterministic testing).
pub fn commit_value_with_blinding(value: u64, blinding: &Scalar) -> PedersenCommitment {
    let h = generator_h();
    let v = Scalar::from(value);
    let point = v * RISTRETTO_BASEPOINT_POINT + *blinding * h;
    let compressed = point.compress();

    PedersenCommitment {
        point: compressed.to_bytes(),
        blinding: blinding.to_bytes(),
    }
}

/// Verify that a commitment matches a revealed (value, blinding) pair.
pub fn verify_commitment(proof: &PedersenProof) -> bool {
    let h = generator_h();
    let v = Scalar::from(proof.value);
    let r = Scalar::from_bytes_mod_order(proof.blinding);
    let expected = v * RISTRETTO_BASEPOINT_POINT + r * h;
    let committed = match CompressedRistretto::from_slice(&proof.commitment) {
        Ok(c) => c,
        Err(_) => return false,
    };
    match committed.decompress() {
        Some(point) => point == expected,
        None => false,
    }
}

/// Batch-generate Pedersen commitments in parallel.
pub fn batch_commit(values: &[u64]) -> Vec<PedersenCommitment> {
    values.par_iter().map(|v| commit_value(*v)).collect()
}

/// Batch-verify Pedersen proofs in parallel.
pub fn batch_verify(proofs: &[PedersenProof]) -> Vec<bool> {
    proofs.par_iter().map(|p| verify_commitment(p)).collect()
}

/// Homomorphic addition of two commitments.
/// commit(a) + commit(b) = commit(a + b) with combined blinding factors.
pub fn add_commitments(a: &PedersenCommitment, b: &PedersenCommitment) -> PedersenCommitment {
    let pa = CompressedRistretto::from_slice(&a.point)
        .expect("valid point")
        .decompress()
        .expect("decompressible");
    let pb = CompressedRistretto::from_slice(&b.point)
        .expect("valid point")
        .decompress()
        .expect("decompressible");
    let sum = pa + pb;

    // Blinding factors also add
    let ra = Scalar::from_bytes_mod_order(a.blinding);
    let rb = Scalar::from_bytes_mod_order(b.blinding);
    let r_sum = ra + rb;

    PedersenCommitment {
        point: sum.compress().to_bytes(),
        blinding: r_sum.to_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commit_verify() {
        let c = commit_value(42);
        let proof = PedersenProof {
            commitment: c.point,
            value: 42,
            blinding: c.blinding,
        };
        assert!(verify_commitment(&proof));
    }

    #[test]
    fn test_wrong_value_fails() {
        let c = commit_value(42);
        let proof = PedersenProof {
            commitment: c.point,
            value: 43, // wrong
            blinding: c.blinding,
        };
        assert!(!verify_commitment(&proof));
    }

    #[test]
    fn test_homomorphic_addition() {
        let a_val = 100u64;
        let b_val = 250u64;

        let blinding_a = Scalar::random(&mut OsRng);
        let blinding_b = Scalar::random(&mut OsRng);

        let ca = commit_value_with_blinding(a_val, &blinding_a);
        let cb = commit_value_with_blinding(b_val, &blinding_b);
        let c_sum = add_commitments(&ca, &cb);

        // Verify the sum commitment
        let proof = PedersenProof {
            commitment: c_sum.point,
            value: a_val + b_val,
            blinding: c_sum.blinding,
        };
        assert!(verify_commitment(&proof));
    }

    #[test]
    fn test_batch_commit() {
        let values: Vec<u64> = (0..10_000).collect();
        let commits = batch_commit(&values);
        assert_eq!(commits.len(), 10_000);
        // Verify a random sample
        let proof = PedersenProof {
            commitment: commits[42].point,
            value: 42,
            blinding: commits[42].blinding,
        };
        assert!(verify_commitment(&proof));
    }
}
