//! State Sync - chunked snapshot download for fast node catch-up.
//!
//! New nodes that are behind the network can download a recent state snapshot
//! from peers instead of replaying every block from genesis. The protocol:
//!
//! 1. **Manifest**: The joining node fetches `GET /sync/manifest` from a peer
//!    to learn the snapshot height, chunk count, and state root.
//! 2. **Chunks**: The node downloads chunks in parallel via `GET /sync/chunk/{index}`.
//!    Each chunk contains ~1000 accounts and a BLAKE3 integrity proof.
//! 3. **Verify**: After all chunks are imported, the node recomputes the state
//!    root and verifies it matches the manifest.
//! 4. **Resume**: The node then joins consensus and catches up on blocks
//!    produced since the snapshot height.

use arc_state::{SnapshotManifest, StateDB, StateSnapshot, SyncProgress};
use reqwest::Client;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Maximum number of parallel chunk downloads.
const MAX_PARALLEL_CHUNKS: usize = 8;

/// Timeout for a single chunk download (seconds).
const CHUNK_TIMEOUT_SECS: u64 = 30;

/// Maximum number of retry attempts per chunk.
const MAX_RETRIES: u32 = 3;

/// Error types for the state sync protocol.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("failed to fetch manifest from {url}: {source}")]
    ManifestFetchFailed { url: String, source: reqwest::Error },

    #[error("failed to fetch chunk {index} from {url}: {source}")]
    ChunkFetchFailed {
        url: String,
        index: u32,
        source: reqwest::Error,
    },

    #[error("chunk verification failed for chunk {index}")]
    ChunkVerificationFailed { index: u32 },

    #[error("state root mismatch after sync: expected {expected}, got {computed}")]
    StateRootMismatch { expected: String, computed: String },

    #[error("sync incomplete: {received}/{total} chunks")]
    SyncIncomplete { received: u32, total: u32 },

    #[error("state import error: {0}")]
    StateError(#[from] arc_state::StateError),

    #[error("no peers available for sync")]
    NoPeers,
}

/// Manages the state sync process over HTTP (RPC endpoints).
pub struct StateSyncManager {
    client: Client,
}

impl StateSyncManager {
    /// Create a new sync manager with default settings.
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(CHUNK_TIMEOUT_SECS))
            .build()
            .expect("failed to build HTTP client");
        Self { client }
    }

    /// Fetch the snapshot manifest from a peer's RPC endpoint.
    pub async fn fetch_manifest(&self, peer_rpc: &str) -> Result<SnapshotManifest, SyncError> {
        let url = format!("http://{}/sync/manifest", peer_rpc);
        info!("Fetching snapshot manifest from {}", url);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| SyncError::ManifestFetchFailed {
                url: url.clone(),
                source: e,
            })?;

        let manifest: SnapshotManifest =
            resp.json()
                .await
                .map_err(|e| SyncError::ManifestFetchFailed { url, source: e })?;

        info!(
            "Manifest received: height={}, accounts={}, chunks={}",
            manifest.version, manifest.total_accounts, manifest.total_chunks
        );
        Ok(manifest)
    }

    /// Fetch a single snapshot chunk from a peer.
    async fn fetch_chunk(
        &self,
        peer_rpc: &str,
        chunk_index: u32,
    ) -> Result<StateSnapshot, SyncError> {
        let url = format!("http://{}/sync/chunk/{}", peer_rpc, chunk_index);

        let resp =
            self.client
                .get(&url)
                .send()
                .await
                .map_err(|e| SyncError::ChunkFetchFailed {
                    url: url.clone(),
                    index: chunk_index,
                    source: e,
                })?;

        let chunk: StateSnapshot =
            resp.json()
                .await
                .map_err(|e| SyncError::ChunkFetchFailed {
                    url,
                    index: chunk_index,
                    source: e,
                })?;

        Ok(chunk)
    }

    /// Fetch a chunk with retry logic.
    async fn fetch_chunk_with_retry(
        &self,
        peer_rpc: &str,
        chunk_index: u32,
    ) -> Result<StateSnapshot, SyncError> {
        let mut last_err = None;
        for attempt in 1..=MAX_RETRIES {
            match self.fetch_chunk(peer_rpc, chunk_index).await {
                Ok(chunk) => return Ok(chunk),
                Err(e) => {
                    warn!(
                        "Chunk {} download attempt {}/{} failed: {}",
                        chunk_index, attempt, MAX_RETRIES, e
                    );
                    last_err = Some(e);
                    if attempt < MAX_RETRIES {
                        tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64))
                            .await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or(SyncError::ChunkVerificationFailed { index: chunk_index }))
    }

    /// Run the full state sync protocol:
    /// 1. Fetch manifest from peer
    /// 2. Download all chunks in parallel batches
    /// 3. Import each chunk into the state database
    /// 4. Verify the final state root
    pub async fn sync_from_peer(
        &self,
        peer_rpc: &str,
        state: &Arc<StateDB>,
    ) -> Result<u64, SyncError> {
        // 1. Fetch manifest
        let manifest = self.fetch_manifest(peer_rpc).await?;
        let total_chunks = manifest.total_chunks;
        let target_height = manifest.version;

        // Skip if we're already at or past this height
        let our_height = state.height();
        if our_height >= target_height {
            info!(
                "Already at height {} (peer has {}), skipping sync",
                our_height, target_height
            );
            return Ok(our_height);
        }

        info!(
            "Starting state sync: {} -> {} ({} chunks, {} accounts)",
            our_height, target_height, total_chunks, manifest.total_accounts
        );

        // 2. Initialize progress tracker
        let mut progress = StateDB::begin_sync(manifest);

        // 3. Download and import chunks in parallel batches
        let mut chunk_index = 0u32;
        while chunk_index < total_chunks {
            let batch_end = (chunk_index + MAX_PARALLEL_CHUNKS as u32).min(total_chunks);
            let batch_indices: Vec<u32> = (chunk_index..batch_end).collect();

            // Spawn parallel downloads
            let mut handles = Vec::new();
            for &idx in &batch_indices {
                let client = self.client.clone();
                let peer = peer_rpc.to_string();
                handles.push(tokio::spawn(async move {
                    let mgr = StateSyncManager { client };
                    (idx, mgr.fetch_chunk_with_retry(&peer, idx).await)
                }));
            }

            // Collect results and import
            for handle in handles {
                let (idx, result) = handle.await.expect("chunk download task panicked");
                let chunk = result?;

                // Import chunk (verifies BLAKE3 proof internally)
                let imported = state
                    .import_snapshot_chunk(&chunk)
                    .map_err(|_| SyncError::ChunkVerificationFailed { index: idx })?;

                StateDB::record_chunk(&mut progress, &chunk, imported)?;

                debug!(
                    "Imported chunk {}/{}: {} accounts ({} total)",
                    idx + 1,
                    total_chunks,
                    imported,
                    progress.total_accounts_imported
                );
            }

            chunk_index = batch_end;
        }

        // 4. Finalize - verify state root
        state.finalize_sync(&progress)?;

        info!(
            "State sync complete: height={}, accounts={}",
            target_height, progress.total_accounts_imported
        );

        Ok(target_height)
    }

    /// Fetch a peer's current DAG round state for consensus catch-up.
    /// Returns (current_round, last_committed_round) so we can initialize
    /// our consensus engine at the right round instead of starting from 0.
    pub async fn fetch_dag_state(&self, peer_rpc: &str) -> Result<(u64, u64), SyncError> {
        let url = format!("http://{}/sync/dag_state", peer_rpc);
        info!("Fetching DAG state from {}", url);

        let resp = self.client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| SyncError::ManifestFetchFailed {
                url: url.clone(),
                source: e,
            })?;

        #[derive(serde::Deserialize)]
        struct DagState {
            current_round: u64,
            last_committed_round: u64,
        }

        let state: DagState = resp.json().await
            .map_err(|e| SyncError::ManifestFetchFailed { url, source: e })?;

        info!(
            "Peer DAG state: round={}, committed={}",
            state.current_round, state.last_committed_round
        );
        Ok((state.current_round, state.last_committed_round))
    }

    /// Full sync: fetch state snapshot + DAG round from a peer.
    /// This is the complete catch-up protocol for late-joining nodes.
    pub async fn full_sync_from_peer(
        &self,
        peer_rpc: &str,
        state: &Arc<StateDB>,
        engine: &arc_consensus::ConsensusEngine,
    ) -> Result<u64, SyncError> {
        // 1. Sync account state
        let height = self.sync_from_peer(peer_rpc, state).await?;

        // 2. Sync DAG round state so consensus starts at the right round
        match self.fetch_dag_state(peer_rpc).await {
            Ok((round, committed)) => {
                engine.set_initial_round(round, committed);
                info!("DAG state synced: starting at round {} (committed {})", round, committed);
            }
            Err(e) => {
                // Non-fatal: old peers may not have this endpoint yet.
                // Node will catch up via round fast-forward on first block.
                warn!("Could not sync DAG state (peer may be old version): {}", e);
            }
        }

        Ok(height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_manager_creation() {
        let _mgr = StateSyncManager::new();
    }
}
