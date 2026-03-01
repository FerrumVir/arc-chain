//! Pre-signing pool for benchmark mode.
//!
//! Background threads continuously generate and sign Transfer transactions
//! using deterministic Ed25519 keypairs. The consensus loop drains signed txs
//! from a bounded channel and feeds them to `execute_block_signed_benchmark()`.

use arc_crypto::signature::{benchmark_address, benchmark_keypair};
use arc_crypto::Hash256;
use arc_types::Transaction;
use crossbeam::channel::{self, Receiver};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use tracing::info;

/// Pre-signing pool that feeds signed transactions to the consensus loop.
pub struct BenchmarkPool {
    rx: Receiver<Vec<Transaction>>,
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    handles: Vec<JoinHandle<()>>,
}

impl BenchmarkPool {
    /// Start the pre-signing pool.
    ///
    /// Spawns `num_threads` background threads that continuously generate and sign
    /// Transfer transactions. Each thread owns a partition of senders to avoid
    /// nonce conflicts.
    ///
    /// # Arguments
    /// * `sender_start` — first sender index (0-based)
    /// * `sender_count` — number of senders this node owns
    /// * `num_threads` — signing threads (default 4)
    /// * `batch_size` — txs per batch sent through channel
    pub fn start(
        sender_start: u8,
        sender_count: u8,
        num_threads: usize,
        batch_size: usize,
    ) -> Self {
        // Bounded channel — prevents OOM if execution can't keep up
        let (tx, rx) = channel::bounded::<Vec<Transaction>>(64);
        let stop = Arc::new(AtomicBool::new(false));

        // Pre-compute receiver addresses (senders 50..100 map to receivers)
        let receivers: Vec<Hash256> = (50u8..100u8)
            .map(benchmark_address)
            .collect();

        // Partition senders across threads
        let senders_per_thread = (sender_count as usize + num_threads - 1) / num_threads;

        let mut handles = Vec::with_capacity(num_threads);
        for thread_id in 0..num_threads {
            let tx = tx.clone();
            let stop = stop.clone();
            let receivers = receivers.clone();
            let start_idx = sender_start as usize + thread_id * senders_per_thread;
            let end_idx = (start_idx + senders_per_thread).min((sender_start + sender_count) as usize);

            if start_idx >= end_idx {
                continue;
            }

            let handle = std::thread::Builder::new()
                .name(format!("bench-sign-{}", thread_id))
                .spawn(move || {
                    // Each thread creates its own keypairs (deterministic, same across nodes)
                    let keypairs: Vec<_> = (start_idx..end_idx)
                        .map(|i| {
                            let sk = benchmark_keypair(i as u8);
                            let addr = benchmark_address(i as u8);
                            (sk, addr, i)
                        })
                        .collect();

                    // Track nonces per sender
                    let mut nonces: Vec<u64> = vec![0; keypairs.len()];

                    while !stop.load(Ordering::Relaxed) {
                        let mut batch = Vec::with_capacity(batch_size);

                        // Round-robin across this thread's senders
                        for _ in 0..batch_size {
                            for (kp_idx, (sk, sender, sender_global_idx)) in keypairs.iter().enumerate() {
                                if batch.len() >= batch_size {
                                    break;
                                }
                                let receiver = receivers[*sender_global_idx % receivers.len()];
                                let nonce = nonces[kp_idx];

                                let mut tx = Transaction::new_transfer(
                                    *sender,
                                    receiver,
                                    1,
                                    nonce,
                                );

                                // Sign with ed25519 keypair
                                use ed25519_dalek::Signer;
                                let sig = sk.sign(tx.hash.as_bytes());
                                let vk = sk.verifying_key();
                                tx.signature = arc_crypto::signature::Signature::Ed25519 {
                                    public_key: *vk.as_bytes(),
                                    signature: sig.to_bytes().to_vec(),
                                };

                                nonces[kp_idx] += 1;
                                batch.push(tx);
                            }
                            if batch.len() >= batch_size {
                                break;
                            }
                        }

                        if !batch.is_empty() {
                            if tx.send(batch).is_err() {
                                break; // Channel closed
                            }
                        }
                    }
                })
                .expect("spawn signing thread");

            handles.push(handle);
        }

        info!(
            threads = num_threads,
            senders = format!("{}-{}", sender_start, sender_start + sender_count - 1),
            batch_size = batch_size,
            "Benchmark signing pool started"
        );

        Self { rx, stop, handles }
    }

    /// Drain up to `max` signed transactions from the pool.
    /// Non-blocking — returns whatever is available.
    pub fn drain(&self, max: usize) -> Vec<Transaction> {
        let mut result = Vec::with_capacity(max);
        while result.len() < max {
            match self.rx.try_recv() {
                Ok(batch) => {
                    let remaining = max - result.len();
                    if batch.len() <= remaining {
                        result.extend(batch);
                    } else {
                        result.extend(batch.into_iter().take(remaining));
                    }
                }
                Err(_) => break,
            }
        }
        result
    }

    /// Stop all signing threads.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for BenchmarkPool {
    fn drop(&mut self) {
        self.stop();
    }
}
