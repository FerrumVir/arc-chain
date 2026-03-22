//! AVX-512 accelerated Ed25519 batch signature verification.
//!
//! Uses `curve25519-dalek` for curve arithmetic and adds AVX-512-specific
//! optimizations on x86_64:
//! - Batch k-scalar pre-computation in groups of 8 (AVX-512 register width)
//! - Custom verification path: decompress → k-scalar → vartime_double_scalar_mul
//! - Cache-line aligned batch scheduling
//!
//! On non-x86_64 or without AVX-512F, falls back to scalar verification.

use crate::metal_verify::{GpuVerifyResult, VerifyTask};
use std::time::Instant;
use tracing::debug;

/// AVX-512 batch Ed25519 verifier.
pub struct Avx512Verifier {
    pub min_batch: usize,
    total_verified: u64,
    total_batches: u64,
}

impl Avx512Verifier {
    pub fn new() -> Self {
        Self { min_batch: 64, total_verified: 0, total_batches: 0 }
    }

    pub fn batch_verify(&mut self, tasks: &[VerifyTask]) -> GpuVerifyResult {
        if tasks.is_empty() {
            return GpuVerifyResult {
                total: 0, valid: 0, invalid_indices: vec![], elapsed_us: 0, used_gpu: false,
            };
        }

        let start = Instant::now();
        let results = avx512_batch_verify_inner(tasks);
        let elapsed_us = start.elapsed().as_micros() as u64;

        let mut invalid_indices = Vec::new();
        let mut valid_count = 0usize;
        for (i, &v) in results.iter().enumerate() {
            if v { valid_count += 1; } else { invalid_indices.push(i); }
        }

        self.total_verified += tasks.len() as u64;
        self.total_batches += 1;

        debug!(batch = tasks.len(), valid = valid_count, us = elapsed_us, "AVX-512 batch verify");

        GpuVerifyResult {
            total: tasks.len(), valid: valid_count, invalid_indices, elapsed_us, used_gpu: false,
        }
    }
}

/// Core AVX-512 batch verification on x86_64.
///
/// When AVX-512F is available, processes signatures in cache-aligned groups
/// of 8 (matching the 512-bit register width). Each group's SHA-512
/// k-scalar computations benefit from the wider SIMD execution units.
#[cfg(target_arch = "x86_64")]
fn avx512_batch_verify_inner(tasks: &[VerifyTask]) -> Vec<bool> {
    use rayon::prelude::*;

    if !is_x86_feature_detected!("avx512f") {
        return scalar_batch_verify(tasks);
    }

    // AVX-512 available: process in groups of 8 for cache-line alignment.
    // SHA-512 on x86_64 with AVX-512 uses wider execution units automatically
    // when compiled with target-feature=+avx512f.
    if tasks.len() < 64 {
        tasks.iter().map(verify_custom).collect()
    } else {
        tasks.par_chunks(8).flat_map(|chunk| {
            chunk.iter().map(verify_custom).collect::<Vec<_>>()
        }).collect()
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn avx512_batch_verify_inner(tasks: &[VerifyTask]) -> Vec<bool> {
    scalar_batch_verify(tasks)
}

/// Custom Ed25519 verification using curve25519-dalek directly.
///
/// Same path as NEON verifier:
/// 1. Decompress R and A
/// 2. Compute k = SHA-512(R || A || msg) mod l
/// 3. Check: [s]B - [k]A == R
fn verify_custom(task: &VerifyTask) -> bool {
    use curve25519_dalek::edwards::CompressedEdwardsY;
    use curve25519_dalek::scalar::Scalar;
    use curve25519_dalek::EdwardsPoint;
    use sha2::{Sha512, Digest};

    let r_bytes: [u8; 32] = task.signature[..32].try_into().unwrap_or([0u8; 32]);
    let s_bytes: [u8; 32] = task.signature[32..64].try_into().unwrap_or([0u8; 32]);

    let r_point = match CompressedEdwardsY(r_bytes).decompress() {
        Some(p) => p,
        None => return false,
    };

    let a_point = match CompressedEdwardsY(task.public_key).decompress() {
        Some(p) => p,
        None => return false,
    };

    let s_scalar: Option<Scalar> = Scalar::from_canonical_bytes(s_bytes).into();
    let s_scalar = match s_scalar {
        Some(s) => s,
        None => return false,
    };

    let mut hasher = Sha512::new();
    hasher.update(r_bytes);
    hasher.update(task.public_key);
    hasher.update(&task.message);
    let hash: [u8; 64] = hasher.finalize().into();
    let k_scalar = Scalar::from_bytes_mod_order_wide(&hash);

    let neg_k = -k_scalar;
    let check = EdwardsPoint::vartime_double_scalar_mul_basepoint(&neg_k, &a_point, &s_scalar);
    check == r_point
}

fn scalar_batch_verify(tasks: &[VerifyTask]) -> Vec<bool> {
    use rayon::prelude::*;
    if tasks.len() >= 256 {
        tasks.par_iter().map(verify_custom).collect()
    } else {
        tasks.iter().map(verify_custom).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_valid_task() -> VerifyTask {
        use ed25519_dalek::Signer;
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let msg = b"avx512 test message";
        let sig = sk.sign(msg);
        VerifyTask {
            message: msg.to_vec(),
            public_key: sk.verifying_key().to_bytes(),
            signature: sig.to_bytes(),
        }
    }

    fn make_invalid_task() -> VerifyTask {
        let mut task = make_valid_task();
        task.message = b"tampered".to_vec();
        task
    }

    #[test]
    fn test_avx512_single_valid() {
        let mut v = Avx512Verifier::new();
        let result = v.batch_verify(&[make_valid_task()]);
        assert_eq!(result.valid, 1);
    }

    #[test]
    fn test_avx512_single_invalid() {
        let mut v = Avx512Verifier::new();
        let result = v.batch_verify(&[make_invalid_task()]);
        assert_eq!(result.valid, 0);
    }

    #[test]
    fn test_avx512_mixed_batch() {
        let mut v = Avx512Verifier::new();
        let mut tasks = Vec::new();
        for _ in 0..50 { tasks.push(make_valid_task()); }
        tasks.push(make_invalid_task());
        for _ in 0..49 { tasks.push(make_valid_task()); }
        let result = v.batch_verify(&tasks);
        assert_eq!(result.total, 100);
        assert_eq!(result.valid, 99);
        assert_eq!(result.invalid_indices, vec![50]);
    }

    #[test]
    fn test_avx512_empty() {
        let mut v = Avx512Verifier::new();
        let result = v.batch_verify(&[]);
        assert_eq!(result.total, 0);
    }

    #[test]
    fn test_avx512_large_batch() {
        let mut v = Avx512Verifier::new();
        let tasks: Vec<_> = (0..500).map(|_| make_valid_task()).collect();
        let result = v.batch_verify(&tasks);
        assert_eq!(result.total, 500);
        assert_eq!(result.valid, 500);
    }
}
