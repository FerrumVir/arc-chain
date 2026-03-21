//! ARC Chain Bridge Relayer
//!
//! A service that watches for bridge events on both Ethereum and ARC Chain,
//! generating Merkle proofs and submitting cross-chain transactions to
//! complete bridge transfers.
//!
//! ## Directions
//!
//! **ETH -> ARC**: Watches ArcBridge.sol `Lock` events, waits for confirmations,
//! then submits BridgeMint (0x10) TXs on ARC Chain.
//!
//! **ARC -> ETH**: Watches ARC Chain for BridgeLock (0x0f) TXs, generates state
//! proofs, then calls `unlock()` on ArcBridge.sol.

mod arc_submitter;
mod config;
mod eth_watcher;

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::time::{self, Duration};
use tracing::{error, info, warn};

use arc_submitter::ArcSubmitter;
use config::RelayerConfig;
use eth_watcher::EthWatcher;

/// ARC Chain Bridge Relayer — connects Ethereum and ARC Chain.
#[derive(Parser, Debug)]
#[command(name = "arc-relayer", about = "Bridge relayer for ARC Chain")]
struct Cli {
    /// Path to the configuration file (TOML).
    #[arg(short, long, default_value = "relayer.toml")]
    config: PathBuf,
}

/// In-memory set of processed event keys for deduplication.
/// Key format: "{direction}:{nonce}" e.g. "eth2arc:42"
struct ProcessedEvents {
    seen: HashSet<String>,
}

impl ProcessedEvents {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
        }
    }

    /// Returns true if this event has already been processed.
    fn is_processed(&self, direction: &str, nonce: u64) -> bool {
        self.seen.contains(&format!("{}:{}", direction, nonce))
    }

    /// Mark an event as processed.
    fn mark_processed(&mut self, direction: &str, nonce: u64) {
        self.seen.insert(format!("{}:{}", direction, nonce));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing/logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "arc_relayer=info".into()),
        )
        .init();

    let cli = Cli::parse();

    info!("ARC Chain Bridge Relayer starting");
    info!(config_path = %cli.config.display(), "loading configuration");

    let config = RelayerConfig::from_file(&cli.config)
        .context("failed to load config")?;
    config.validate()?;

    info!(
        eth_rpc = %config.eth_rpc_url,
        arc_rpc = %config.arc_rpc_url,
        bridge_contract = %config.bridge_contract,
        confirmations = config.confirmations,
        poll_interval = config.poll_interval_secs,
        "relayer configured"
    );

    // Initialize watchers and submitters.
    let mut eth_watcher = EthWatcher::new(&config);
    let mut arc_submitter = ArcSubmitter::new(&config)?;
    let mut processed = ProcessedEvents::new();

    let poll_interval = Duration::from_secs(config.poll_interval_secs);

    info!("entering main relay loop");

    loop {
        // --- ETH -> ARC direction ---
        match eth_watcher.poll_lock_events().await {
            Ok(events) => {
                for event in events {
                    if processed.is_processed("eth2arc", event.nonce) {
                        info!(nonce = event.nonce, "skipping already-processed ETH lock");
                        continue;
                    }

                    info!(
                        nonce = event.nonce,
                        amount = event.amount,
                        sender = hex::encode(event.sender),
                        block = event.block_number,
                        "processing ETH Lock event"
                    );

                    match arc_submitter.submit_bridge_mint(&event).await {
                        Ok(transfer_id) => {
                            info!(
                                transfer_id = hex::encode(transfer_id),
                                nonce = event.nonce,
                                "BridgeMint submitted successfully"
                            );
                            processed.mark_processed("eth2arc", event.nonce);
                        }
                        Err(e) => {
                            error!(
                                nonce = event.nonce,
                                error = %e,
                                "failed to submit BridgeMint, will retry"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to poll ETH Lock events");
            }
        }

        // --- ARC -> ETH direction ---
        match arc_submitter.poll_bridge_locks().await {
            Ok(events) => {
                for event in events {
                    if processed.is_processed("arc2eth", event.nonce) {
                        info!(nonce = event.nonce, "skipping already-processed ARC lock");
                        continue;
                    }

                    info!(
                        nonce = event.nonce,
                        amount = event.amount,
                        sender = hex::encode(event.sender),
                        recipient = hex::encode(event.eth_recipient),
                        block = event.block_height,
                        "processing ARC BridgeLock"
                    );

                    // For ARC -> ETH, we need the state root from the ARC Chain
                    // block that included this BridgeLock. In production the
                    // relayer would first commit the state root to the Ethereum
                    // contract, then call unlock(). Here we construct a
                    // placeholder proof.
                    let state_root = [0u8; 32]; // Would come from arc_getBlock
                    let merkle_proof: Vec<[u8; 32]> = vec![]; // Would come from arc_getProof

                    match arc_submitter
                        .submit_eth_unlock(
                            &config.eth_rpc_url,
                            &config.bridge_contract,
                            &event,
                            state_root,
                            merkle_proof,
                        )
                        .await
                    {
                        Ok(tx_hash) => {
                            info!(
                                tx_hash = hex::encode(tx_hash),
                                nonce = event.nonce,
                                "unlock TX submitted to Ethereum"
                            );
                            processed.mark_processed("arc2eth", event.nonce);
                        }
                        Err(e) => {
                            error!(
                                nonce = event.nonce,
                                error = %e,
                                "failed to submit ETH unlock, will retry"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to poll ARC BridgeLock transactions");
            }
        }

        time::sleep(poll_interval).await;
    }
}
