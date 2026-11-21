#[cfg(feature = "benchmark-tools")]
use arc_crypto::Hash256;
#[cfg(feature = "benchmark-tools")]
use arc_crypto::signature::{benchmark_address, benchmark_keypair};
use arc_types::Transaction;
#[cfg(feature = "benchmark-tools")]
use crossbeam::channel::{self, Receiver};
#[cfg(feature = "benchmark-tools")]
use std::sync::Arc;
#[cfg(feature = "benchmark-tools")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "benchmark-tools")]
use std::thread::JoinHandle;
#[cfg(feature = "benchmark-tools")]
use tracing::info;

#[cfg(feature = "benchmark-tools")]
pub struct BenchmarkPool {
    rx: Receiver<Vec<Transaction>>,
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    handles: Vec<JoinHandle<()>>,
}

#[cfg(not(feature = "benchmark-tools"))]
pub struct BenchmarkPool {
    _private: (),
}

#[cfg(feature = "benchmark-tools")]
impl BenchmarkPool {
    pub fn start(
        sender_start: u8,
        sender_count: u8,
        num_threads: usize,
        batch_size: usize,
    ) -> Self {
        let (tx, rx) = channel::bounded::<Vec<Transaction>>(64);
        let stop = Arc::new(AtomicBool::new(false));

        let receivers: Vec<Hash256> = (50u8..100u8).map(benchmark_address).collect();

        let senders_per_thread = (sender_count as usize).div_ceil(num_threads);

        let mut handles = Vec::with_capacity(num_threads);
        for thread_id in 0..num_threads {
            let tx = tx.clone();
            let stop = stop.clone();
            let receivers = receivers.clone();
            let start_idx = sender_start as usize + thread_id * senders_per_thread;
            let end_idx =
                (start_idx + senders_per_thread).min((sender_start + sender_count) as usize);

            if start_idx >= end_idx {
                continue;
            }

            let handle = std::thread::Builder::new()
                .name(format!("bench-sign-{}", thread_id))
                .spawn(move || {
                    let keypairs: Vec<_> = (start_idx..end_idx)
                        .map(|i| {
                            let sk = benchmark_keypair(i as u8);
                            let addr = benchmark_address(i as u8);
                            (sk, addr, i)
                        })
                        .collect();

                    let mut nonces: Vec<u64> = vec![0; keypairs.len()];

                    while !stop.load(Ordering::Relaxed) {
                        let mut batch = Vec::with_capacity(batch_size);

                        for _ in 0..batch_size {
                            for (kp_idx, (sk, sender, sender_global_idx)) in
                                keypairs.iter().enumerate()
                            {
                                if batch.len() >= batch_size {
                                    break;
                                }
                                let receiver = receivers[*sender_global_idx % receivers.len()];
                                let nonce = nonces[kp_idx];

                                let mut tx = Transaction::new_transfer(*sender, receiver, 1, nonce);

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

                        if !batch.is_empty() && tx.send(batch).is_err() {
                            break;
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

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "benchmark-tools"))]
impl BenchmarkPool {
    pub fn drain(&self, _max: usize) -> Vec<Transaction> {
        Vec::new()
    }
}

#[cfg(feature = "benchmark-tools")]
impl Drop for BenchmarkPool {
    fn drop(&mut self) {
        self.stop();
    }
}
