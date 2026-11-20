use crate::metal_verify::{GpuVerifyResult, VerifyTask};
use std::time::Instant;
use tracing::debug;

pub struct NeonVerifier {
    pub min_batch: usize,
    total_verified: u64,
    total_batches: u64,
    avg_throughput: f64,
}

impl Default for NeonVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl NeonVerifier {
    pub fn new() -> Self {
        Self {
            min_batch: 32,
            total_verified: 0,
            total_batches: 0,
            avg_throughput: 0.0,
        }
    }

    pub fn batch_verify(&mut self, tasks: &[VerifyTask]) -> GpuVerifyResult {
        if tasks.is_empty() {
            return GpuVerifyResult {
                total: 0,
                valid: 0,
                invalid_indices: vec![],
                elapsed_us: 0,
                used_gpu: false,
            };
        }

        let start = Instant::now();
        let results = neon_batch_verify_inner(tasks);
        let elapsed_us = start.elapsed().as_micros() as u64;

        let mut invalid_indices = Vec::new();
        let mut valid_count = 0usize;
        for (i, &v) in results.iter().enumerate() {
            if v {
                valid_count += 1;
            } else {
                invalid_indices.push(i);
            }
        }

        self.total_verified += tasks.len() as u64;
        self.total_batches += 1;
        if elapsed_us > 0 {
            let tp = (tasks.len() as f64) / (elapsed_us as f64 / 1_000_000.0);
            self.avg_throughput = if self.total_batches == 1 {
                tp
            } else {
                self.avg_throughput * 0.7 + tp * 0.3
            };
        }

        debug!(
            batch = tasks.len(),
            valid = valid_count,
            us = elapsed_us,
            "NEON batch verify"
        );

        GpuVerifyResult {
            total: tasks.len(),
            valid: valid_count,
            invalid_indices,
            elapsed_us,
            used_gpu: false,
        }
    }

    pub fn throughput(&self) -> f64 {
        self.avg_throughput
    }
}

#[cfg(target_arch = "aarch64")]
fn neon_batch_verify_inner(tasks: &[VerifyTask]) -> Vec<bool> {
    use rayon::prelude::*;

    if tasks.len() < 64 {
        tasks.iter().map(verify_custom).collect()
    } else {
        tasks
            .par_chunks(2)
            .flat_map(|pair| {
                let results: Vec<bool> = pair.iter().map(verify_custom).collect();
                results
            })
            .collect()
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn neon_batch_verify_inner(tasks: &[VerifyTask]) -> Vec<bool> {
    use rayon::prelude::*;
    if tasks.len() >= 256 {
        tasks.par_iter().map(verify_custom).collect()
    } else {
        tasks.iter().map(verify_custom).collect()
    }
}

fn verify_custom(task: &VerifyTask) -> bool {
    use curve25519_dalek::EdwardsPoint;
    use curve25519_dalek::edwards::CompressedEdwardsY;
    use curve25519_dalek::scalar::Scalar;
    use sha2::{Digest, Sha512};

    let r_bytes: [u8; 32] = task.signature[..32].try_into().unwrap_or([0u8; 32]);
    let s_bytes: [u8; 32] = task.signature[32..64].try_into().unwrap_or([0u8; 32]);

    let r_compressed = CompressedEdwardsY(r_bytes);
    let r_point = match r_compressed.decompress() {
        Some(p) => p,
        None => return false,
    };

    let a_compressed = CompressedEdwardsY(task.public_key);
    let a_point = match a_compressed.decompress() {
        Some(p) => p,
        None => return false,
    };

    let s_scalar = match Scalar::from_canonical_bytes(s_bytes).into() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_valid_task() -> VerifyTask {
        use ed25519_dalek::Signer;
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let msg = b"neon test message";
        let sig = sk.sign(msg);
        VerifyTask {
            message: msg.to_vec(),
            public_key: sk.verifying_key().to_bytes(),
            signature: sig.to_bytes(),
        }
    }

    fn make_invalid_task() -> VerifyTask {
        let mut task = make_valid_task();
        task.message = b"wrong message".to_vec();
        task
    }

    #[test]
    fn test_neon_single_valid() {
        let mut v = NeonVerifier::new();
        let result = v.batch_verify(&[make_valid_task()]);
        assert_eq!(result.valid, 1);
    }

    #[test]
    fn test_neon_single_invalid() {
        let mut v = NeonVerifier::new();
        let result = v.batch_verify(&[make_invalid_task()]);
        assert_eq!(result.valid, 0);
        assert_eq!(result.invalid_indices, vec![0]);
    }

    #[test]
    fn test_neon_mixed_batch() {
        let mut v = NeonVerifier::new();
        let mut tasks: Vec<_> = (0..99).map(|_| make_valid_task()).collect();
        tasks.insert(42, make_invalid_task());
        let result = v.batch_verify(&tasks);
        assert_eq!(result.total, 100);
        assert_eq!(result.valid, 99);
        assert_eq!(result.invalid_indices, vec![42]);
    }

    #[test]
    fn test_neon_empty() {
        let mut v = NeonVerifier::new();
        let result = v.batch_verify(&[]);
        assert_eq!(result.total, 0);
    }

    #[test]
    fn test_neon_large_batch() {
        let mut v = NeonVerifier::new();
        let tasks: Vec<_> = (0..1000).map(|_| make_valid_task()).collect();
        let result = v.batch_verify(&tasks);
        assert_eq!(result.total, 1000);
        assert_eq!(result.valid, 1000);
        assert!(v.throughput() > 0.0);
    }

    #[test]
    fn test_neon_malformed_key() {
        let mut v = NeonVerifier::new();
        let task = VerifyTask {
            message: b"hello".to_vec(),
            public_key: [0xFFu8; 32],
            signature: [0u8; 64],
        };
        let result = v.batch_verify(&[task]);
        assert_eq!(result.valid, 0);
    }

    #[test]
    fn test_custom_verify_matches_dalek() {
        use ed25519_dalek::Signer;
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let vk = sk.verifying_key();

        for i in 0..100 {
            let msg = format!("message number {}", i);
            let sig = sk.sign(msg.as_bytes());
            let task = VerifyTask {
                message: msg.into_bytes(),
                public_key: vk.to_bytes(),
                signature: sig.to_bytes(),
            };
            assert!(verify_custom(&task), "Custom verify failed for msg {i}");
            assert!(vk.verify_strict(task.message.as_slice(), &sig).is_ok());
        }
    }
}
