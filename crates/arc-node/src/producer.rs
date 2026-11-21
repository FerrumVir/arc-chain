use arc_crypto::Hash256;
use arc_mempool::Mempool;
use arc_state::StateDB;
use std::sync::Arc;
use tracing::{info, warn};

const MAX_TXS_PER_BLOCK: usize = 100_000;

const BLOCK_INTERVAL_MS: u64 = 100;

pub async fn run_block_producer(state: Arc<StateDB>, mempool: Arc<Mempool>, producer: Hash256) {
    info!(
        "Block producer started (interval={}ms, max_txs={})",
        BLOCK_INTERVAL_MS, MAX_TXS_PER_BLOCK
    );

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(BLOCK_INTERVAL_MS)).await;

        let pending = mempool.len();
        if pending == 0 {
            continue;
        }

        let transactions = mempool.drain(MAX_TXS_PER_BLOCK);
        if transactions.is_empty() {
            continue;
        }

        let tx_count = transactions.len();
        let start = std::time::Instant::now();

        match state.execute_block_verified(&transactions, producer) {
            Ok((block, receipts)) => {
                let elapsed = start.elapsed();
                let success_count = receipts.iter().filter(|r| r.success).count();
                let tps = if elapsed.as_secs_f64() > 0.0 {
                    tx_count as f64 / elapsed.as_secs_f64()
                } else {
                    tx_count as f64
                };

                info!(
                    height = block.header.height,
                    txs = tx_count,
                    success = success_count,
                    elapsed_ms = elapsed.as_millis(),
                    tps = format!("{:.0}", tps),
                    root = %block.header.tx_root,
                    "Block produced"
                );
            }
            Err(e) => {
                warn!("Block production failed: {}", e);
            }
        }
    }
}
