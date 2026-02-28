use anyhow::Result;
use arc_crypto::{hash_bytes, Hash256};
use arc_mempool::Mempool;
use arc_node::rpc;
use arc_state::StateDB;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("arc=info".parse()?))
        .init();

    tracing::info!("╔═══════════════════════════════════════╗");
    tracing::info!("║   ARC Chain — Agent Runtime Chain     ║");
    tracing::info!("║   Testnet Node v0.1.0                 ║");
    tracing::info!("╚═══════════════════════════════════════╝");

    // Genesis accounts — prefunded for testing
    let genesis_accounts: Vec<(Hash256, u64)> = (0..100u8)
        .map(|i| (hash_bytes(&[i]), 1_000_000_000_000))
        .collect();

    let state = Arc::new(StateDB::with_genesis(&genesis_accounts));
    let mempool = Arc::new(Mempool::new(10_000_000));
    let producer = hash_bytes(&[0]); // Genesis validator

    // Start block production in background
    let state_clone = state.clone();
    let mempool_clone = mempool.clone();
    tokio::spawn(async move {
        arc_node::producer::run_block_producer(state_clone, mempool_clone, producer).await;
    });

    // Start RPC server
    let addr = "0.0.0.0:9090";
    tracing::info!("RPC server listening on {}", addr);
    rpc::serve(addr, state, mempool).await?;

    Ok(())
}
