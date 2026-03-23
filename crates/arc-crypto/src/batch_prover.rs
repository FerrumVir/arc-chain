//! Batch proving pipeline for ARC Chain.
//!
//! Queues proving tasks, executes them in priority order, and collects results
//! with timing statistics. Proofs are BLAKE3-based mock proofs suitable for
//! integration testing and pipeline validation. For real STARK proofs, use
//! the `stwo-prover` feature which routes through `stwo_air.rs`.
//!
//! ## Pipeline flow
//!
//! ```text
//! submit(task) ──> priority queue ──> prove_next() ──> ProveResult
//!                                 ──> prove_batch(n) ──> Vec<ProveResult>
//!                                                        │
//!                                                        v
//!                                                    drain_completed()
//! ```
//!
//! ## Mock proof construction
//!
//! ```text
//! proof = BLAKE3("arc-batch-proof" || circuit_id || public_inputs || private_inputs)
//! verification = BLAKE3("arc-batch-verify" || circuit_id || public_inputs) == proof_tag
//! ```

use serde::{Deserialize, Serialize};
use std::time::Instant;

// ── Core types ────────────────────────────────────────────────────────────────

/// Configuration for the batch prover.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchConfig {
    /// Maximum number of tasks in a single batch.
    pub max_batch_size: usize,
    /// Maximum number of proofs that can be generated in parallel.
    pub max_parallel_proofs: usize,
    /// Timeout per proof in milliseconds.
    pub timeout_ms: u64,
    /// Proof system identifier (e.g., "stark", "plonk", "groth16").
    pub proof_type: String,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 256,
            max_parallel_proofs: 4,
            timeout_ms: 30_000,
            proof_type: "stark-mock".to_string(),
        }
    }
}

/// A task submitted to the batch prover.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProveTask {
    /// Unique task identifier.
    pub id: [u8; 32],
    /// Identifier of the circuit to prove.
    pub circuit_id: [u8; 32],
    /// Public inputs to the circuit.
    pub public_inputs: Vec<u64>,
    /// Private (witness) inputs to the circuit.
    pub private_inputs: Vec<u64>,
    /// Priority (0 = lowest, 255 = highest). Higher priority tasks are proved first.
    pub priority: u8,
    /// Submission timestamp (unix millis).
    pub submitted_at: u64,
}

/// Result of proving a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProveResult {
    /// The task that was proved.
    pub task_id: [u8; 32],
    /// The generated proof bytes.
    pub proof: Vec<u8>,
    /// Public output values (derived from the proof).
    pub public_outputs: Vec<u64>,
    /// Time spent generating the proof in milliseconds.
    pub prove_time_ms: u64,
    /// Time spent verifying the proof in milliseconds.
    pub verify_time_ms: u64,
    /// Outcome status.
    pub status: ProveStatus,
}

/// Status of a prove operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProveStatus {
    Success,
    Failed(String),
    Timeout,
    Cancelled,
}

/// Cumulative statistics for a batch prover instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProverStats {
    pub total_proved: u64,
    pub total_failed: u64,
    pub avg_prove_time_ms: u64,
    pub peak_prove_time_ms: u64,
    pub total_verified: u64,
}

// ── Mock proof helpers ────────────────────────────────────────────────────────

/// Generate a mock STARK proof from a task.
///
/// proof = BLAKE3("arc-batch-proof" || circuit_id || public_inputs || private_inputs)
fn generate_mock_proof(task: &ProveTask) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-batch-proof");
    hasher.update(&task.circuit_id);
    for &v in &task.public_inputs {
        hasher.update(&v.to_le_bytes());
    }
    for &v in &task.private_inputs {
        hasher.update(&v.to_le_bytes());
    }
    hasher.finalize().as_bytes().to_vec()
}

/// Compute the verification tag for a proof.
///
/// tag = BLAKE3("arc-batch-verify" || circuit_id || public_inputs || proof)
fn compute_verify_tag(circuit_id: &[u8; 32], public_inputs: &[u64], proof: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-batch-verify");
    hasher.update(circuit_id);
    for &v in public_inputs {
        hasher.update(&v.to_le_bytes());
    }
    hasher.update(proof);
    *hasher.finalize().as_bytes()
}

/// Verify a mock proof.
pub fn verify_mock_proof(
    circuit_id: &[u8; 32],
    public_inputs: &[u64],
    proof: &[u8],
    expected_tag: &[u8; 32],
) -> bool {
    let tag = compute_verify_tag(circuit_id, public_inputs, proof);
    tag == *expected_tag
}

// ── Batch Prover ──────────────────────────────────────────────────────────────

/// The batch prover: queues tasks, generates proofs, and tracks statistics.
pub struct BatchProver {
    pub config: BatchConfig,
    queue: Vec<ProveTask>,
    completed: Vec<ProveResult>,
    stats: ProverStats,
}

impl BatchProver {
    /// Create a new batch prover with the given configuration.
    pub fn new(config: BatchConfig) -> Self {
        Self {
            config,
            queue: Vec::new(),
            completed: Vec::new(),
            stats: ProverStats::default(),
        }
    }

    /// Submit a task to the proving queue. Returns the task ID.
    pub fn submit(&mut self, task: ProveTask) -> [u8; 32] {
        let id = task.id;
        self.queue.push(task);
        id
    }

    /// Prove the highest-priority pending task.
    ///
    /// Returns `None` if the queue is empty.
    pub fn prove_next(&mut self) -> Option<ProveResult> {
        if self.queue.is_empty() {
            return None;
        }

        // Find highest priority task (stable: first submitted among equal priority).
        let best_idx = self
            .queue
            .iter()
            .enumerate()
            .max_by_key(|(_, t)| t.priority)
            .map(|(i, _)| i)
            .unwrap();

        let task = self.queue.remove(best_idx);
        let result = self.prove_task(&task);
        self.completed.push(result.clone());
        Some(result)
    }

    /// Prove up to `max` tasks from the queue in priority order.
    pub fn prove_batch(&mut self, max: usize) -> Vec<ProveResult> {
        let count = max.min(self.queue.len());
        let mut results = Vec::with_capacity(count);

        for _ in 0..count {
            if let Some(result) = self.prove_next() {
                results.push(result);
            } else {
                break;
            }
        }

        results
    }

    /// Drain all completed results from the prover.
    pub fn drain_completed(&mut self) -> Vec<ProveResult> {
        std::mem::take(&mut self.completed)
    }

    /// Number of tasks waiting in the queue.
    pub fn pending_count(&self) -> usize {
        self.queue.len()
    }

    /// Reference to cumulative prover statistics.
    pub fn stats(&self) -> &ProverStats {
        &self.stats
    }

    /// Internal: prove a single task and update stats.
    fn prove_task(&mut self, task: &ProveTask) -> ProveResult {
        let start = Instant::now();

        // Generate the mock proof.
        let proof = generate_mock_proof(task);
        let prove_elapsed = start.elapsed().as_millis() as u64;

        // Compute a verification tag and verify.
        let verify_start = Instant::now();
        let verify_tag = compute_verify_tag(&task.circuit_id, &task.public_inputs, &proof);
        let verified = verify_mock_proof(&task.circuit_id, &task.public_inputs, &proof, &verify_tag);
        let verify_elapsed = verify_start.elapsed().as_millis() as u64;

        let status = if verified {
            ProveStatus::Success
        } else {
            ProveStatus::Failed("verification mismatch".to_string())
        };

        // Update stats.
        match &status {
            ProveStatus::Success => {
                self.stats.total_proved += 1;
                self.stats.total_verified += 1;
            }
            ProveStatus::Failed(_) => {
                self.stats.total_failed += 1;
            }
            _ => {}
        }

        // Update timing stats.
        if prove_elapsed > self.stats.peak_prove_time_ms {
            self.stats.peak_prove_time_ms = prove_elapsed;
        }
        let total_proofs = self.stats.total_proved + self.stats.total_failed;
        if total_proofs > 0 {
            // Rolling average.
            self.stats.avg_prove_time_ms = ((self.stats.avg_prove_time_ms
                * (total_proofs - 1))
                + prove_elapsed)
                / total_proofs;
        }

        // Public outputs: the first 32 bytes of the proof re-hashed.
        let mut output_hasher = blake3::Hasher::new();
        output_hasher.update(b"arc-batch-output");
        output_hasher.update(&proof);
        let output_hash = output_hasher.finalize();
        let public_outputs: Vec<u64> = output_hash
            .as_bytes()
            .chunks(8)
            .map(|chunk| {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(chunk);
                u64::from_le_bytes(bytes)
            })
            .collect();

        ProveResult {
            task_id: task.id,
            proof,
            public_outputs,
            prove_time_ms: prove_elapsed,
            verify_time_ms: verify_elapsed,
            status,
        }
    }
}

/// Create a task ID from a seed string (convenience for tests).
pub fn task_id_from_seed(seed: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-task-id");
    hasher.update(seed.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Create a circuit ID from a name (convenience for tests).
pub fn circuit_id_from_name(name: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-circuit-id");
    hasher.update(name.as_bytes());
    *hasher.finalize().as_bytes()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(seed: &str, priority: u8) -> ProveTask {
        ProveTask {
            id: task_id_from_seed(seed),
            circuit_id: circuit_id_from_name("test-circuit"),
            public_inputs: vec![1, 2, 3],
            private_inputs: vec![10, 20],
            priority,
            submitted_at: 1000,
        }
    }

    #[test]
    fn test_new_prover() {
        let prover = BatchProver::new(BatchConfig::default());
        assert_eq!(prover.pending_count(), 0);
        assert_eq!(prover.stats().total_proved, 0);
        assert_eq!(prover.stats().total_failed, 0);
    }

    #[test]
    fn test_submit_task() {
        let mut prover = BatchProver::new(BatchConfig::default());
        let id = prover.submit(make_task("task-1", 5));
        assert_eq!(prover.pending_count(), 1);
        assert_eq!(id, task_id_from_seed("task-1"));
    }

    #[test]
    fn test_prove_next_empty() {
        let mut prover = BatchProver::new(BatchConfig::default());
        assert!(prover.prove_next().is_none());
    }

    #[test]
    fn test_prove_next_single() {
        let mut prover = BatchProver::new(BatchConfig::default());
        prover.submit(make_task("task-1", 5));

        let result = prover.prove_next().unwrap();
        assert_eq!(result.task_id, task_id_from_seed("task-1"));
        assert_eq!(result.status, ProveStatus::Success);
        assert!(!result.proof.is_empty());
        assert_eq!(prover.pending_count(), 0);
    }

    #[test]
    fn test_prove_next_priority_order() {
        let mut prover = BatchProver::new(BatchConfig::default());
        prover.submit(make_task("low", 1));
        prover.submit(make_task("high", 10));
        prover.submit(make_task("medium", 5));

        // Highest priority first.
        let r1 = prover.prove_next().unwrap();
        assert_eq!(r1.task_id, task_id_from_seed("high"));

        let r2 = prover.prove_next().unwrap();
        assert_eq!(r2.task_id, task_id_from_seed("medium"));

        let r3 = prover.prove_next().unwrap();
        assert_eq!(r3.task_id, task_id_from_seed("low"));
    }

    #[test]
    fn test_prove_batch() {
        let mut prover = BatchProver::new(BatchConfig::default());
        for i in 0..5 {
            prover.submit(make_task(&format!("task-{i}"), i as u8));
        }

        let results = prover.prove_batch(3);
        assert_eq!(results.len(), 3);
        assert_eq!(prover.pending_count(), 2);

        // All should succeed.
        for r in &results {
            assert_eq!(r.status, ProveStatus::Success);
        }
    }

    #[test]
    fn test_prove_batch_more_than_available() {
        let mut prover = BatchProver::new(BatchConfig::default());
        prover.submit(make_task("only-one", 5));

        let results = prover.prove_batch(100);
        assert_eq!(results.len(), 1);
        assert_eq!(prover.pending_count(), 0);
    }

    #[test]
    fn test_drain_completed() {
        let mut prover = BatchProver::new(BatchConfig::default());
        prover.submit(make_task("t1", 5));
        prover.submit(make_task("t2", 5));

        prover.prove_batch(2);
        let completed = prover.drain_completed();
        assert_eq!(completed.len(), 2);

        // Drain again should be empty.
        let again = prover.drain_completed();
        assert!(again.is_empty());
    }

    #[test]
    fn test_stats_updated() {
        let mut prover = BatchProver::new(BatchConfig::default());
        prover.submit(make_task("t1", 5));
        prover.submit(make_task("t2", 5));
        prover.submit(make_task("t3", 5));

        prover.prove_batch(3);

        let stats = prover.stats();
        assert_eq!(stats.total_proved, 3);
        assert_eq!(stats.total_failed, 0);
        assert_eq!(stats.total_verified, 3);
    }

    #[test]
    fn test_mock_proof_deterministic() {
        let task = make_task("deterministic", 5);
        let proof1 = generate_mock_proof(&task);
        let proof2 = generate_mock_proof(&task);
        assert_eq!(proof1, proof2, "mock proofs must be deterministic");
    }

    #[test]
    fn test_mock_proof_different_inputs() {
        let mut t1 = make_task("same-seed", 5);
        let mut t2 = make_task("same-seed", 5);
        t1.public_inputs = vec![1, 2, 3];
        t2.public_inputs = vec![4, 5, 6];

        let proof1 = generate_mock_proof(&t1);
        let proof2 = generate_mock_proof(&t2);
        assert_ne!(proof1, proof2, "different inputs must produce different proofs");
    }

    #[test]
    fn test_verify_mock_proof_roundtrip() {
        let task = make_task("verify-roundtrip", 5);
        let proof = generate_mock_proof(&task);
        let tag = compute_verify_tag(&task.circuit_id, &task.public_inputs, &proof);

        assert!(verify_mock_proof(
            &task.circuit_id,
            &task.public_inputs,
            &proof,
            &tag
        ));
    }

    #[test]
    fn test_verify_mock_proof_tampered() {
        let task = make_task("tamper-test", 5);
        let mut proof = generate_mock_proof(&task);
        let tag = compute_verify_tag(&task.circuit_id, &task.public_inputs, &proof);

        // Tamper with the proof.
        proof[0] ^= 0xFF;
        assert!(!verify_mock_proof(
            &task.circuit_id,
            &task.public_inputs,
            &proof,
            &tag
        ));
    }

    #[test]
    fn test_public_outputs_present() {
        let mut prover = BatchProver::new(BatchConfig::default());
        prover.submit(make_task("outputs", 5));

        let result = prover.prove_next().unwrap();
        // Public outputs should be 4 u64 values (32 bytes / 8).
        assert_eq!(result.public_outputs.len(), 4);
    }

    #[test]
    fn test_task_id_from_seed() {
        let id1 = task_id_from_seed("hello");
        let id2 = task_id_from_seed("hello");
        let id3 = task_id_from_seed("world");

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_batch_config_default() {
        let config = BatchConfig::default();
        assert_eq!(config.max_batch_size, 256);
        assert_eq!(config.max_parallel_proofs, 4);
        assert_eq!(config.timeout_ms, 30_000);
        assert_eq!(config.proof_type, "stark-mock");
    }
}
