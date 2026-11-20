use arc_state::{SnapshotManifest, StateDB, StateSnapshot};
use reqwest::Client;
use std::sync::Arc;
use tracing::{debug, info, warn};

const MAX_PARALLEL_CHUNKS: usize = 8;

const CHUNK_TIMEOUT_SECS: u64 = 30;

const MAX_RETRIES: u32 = 3;

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

pub struct StateSyncManager {
    client: Client,
}

impl Default for StateSyncManager {
    fn default() -> Self {
        Self::new()
    }
}

impl StateSyncManager {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(CHUNK_TIMEOUT_SECS))
            .build()
            .expect("failed to build HTTP client");
        Self { client }
    }

    pub async fn fetch_manifest(&self, peer_rpc: &str) -> Result<SnapshotManifest, SyncError> {
        let url = format!("http://{}/sync/manifest", peer_rpc);
        info!("Fetching snapshot manifest from {}", url);

        let resp =
            self.client
                .get(&url)
                .send()
                .await
                .map_err(|e| SyncError::ManifestFetchFailed {
                    url: url.clone(),
                    source: e,
                })?;

        let manifest: SnapshotManifest = resp
            .json()
            .await
            .map_err(|e| SyncError::ManifestFetchFailed { url, source: e })?;

        info!(
            "Manifest received: height={}, accounts={}, chunks={}",
            manifest.version, manifest.total_accounts, manifest.total_chunks
        );
        Ok(manifest)
    }

    async fn fetch_chunk(
        &self,
        peer_rpc: &str,
        chunk_index: u32,
    ) -> Result<StateSnapshot, SyncError> {
        let url = format!("http://{}/sync/chunk/{}", peer_rpc, chunk_index);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| SyncError::ChunkFetchFailed {
                url: url.clone(),
                index: chunk_index,
                source: e,
            })?;

        let chunk: StateSnapshot = resp.json().await.map_err(|e| SyncError::ChunkFetchFailed {
            url,
            index: chunk_index,
            source: e,
        })?;

        Ok(chunk)
    }

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

    pub async fn sync_from_peer(
        &self,
        peer_rpc: &str,
        state: &Arc<StateDB>,
    ) -> Result<u64, SyncError> {
        if state.is_persistent() {
            return Err(SyncError::StateError(
                arc_state::StateError::UnauthenticatedPersistentStateSync,
            ));
        }
        let manifest = self.fetch_manifest(peer_rpc).await?;
        let total_chunks = manifest.total_chunks;
        let target_height = manifest.version;

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

        let mut progress = StateDB::begin_sync(manifest);

        let mut chunk_index = 0u32;
        while chunk_index < total_chunks {
            let batch_end = (chunk_index + MAX_PARALLEL_CHUNKS as u32).min(total_chunks);
            let batch_indices: Vec<u32> = (chunk_index..batch_end).collect();

            let mut handles = Vec::new();
            for &idx in &batch_indices {
                let client = self.client.clone();
                let peer = peer_rpc.to_string();
                handles.push(tokio::spawn(async move {
                    let mgr = StateSyncManager { client };
                    (idx, mgr.fetch_chunk_with_retry(&peer, idx).await)
                }));
            }

            for handle in handles {
                let (idx, result) = handle.await.expect("chunk download task panicked");
                let chunk = result?;

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

        state.finalize_sync(&progress)?;

        info!(
            "State sync complete: height={}, accounts={}",
            target_height, progress.total_accounts_imported
        );

        Ok(target_height)
    }

    pub async fn fetch_dag_state(&self, peer_rpc: &str) -> Result<(u64, u64), SyncError> {
        let url = format!("http://{}/sync/dag_state", peer_rpc);
        info!("Fetching DAG state from {}", url);

        let resp = self
            .client
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

        let state: DagState = resp
            .json()
            .await
            .map_err(|e| SyncError::ManifestFetchFailed { url, source: e })?;

        info!(
            "Peer DAG state: round={}, committed={}",
            state.current_round, state.last_committed_round
        );
        Ok((state.current_round, state.last_committed_round))
    }

    pub async fn full_sync_from_peer(
        &self,
        peer_rpc: &str,
        state: &Arc<StateDB>,
        engine: &arc_consensus::ConsensusEngine,
    ) -> Result<u64, SyncError> {
        let height = self.sync_from_peer(peer_rpc, state).await?;

        match self.fetch_dag_state(peer_rpc).await {
            Ok((round, committed)) => {
                engine.observe_untrusted_round_hint(round, committed);
                info!(
                    "Observed peer DAG hint: round {} (committed {}); local consensus cursors unchanged",
                    round, committed
                );
            }
            Err(e) => {
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

    #[tokio::test]
    async fn persistent_sync_rejects_before_contacting_a_peer() {
        let directory = std::env::temp_dir().join(format!(
            "arc-state-sync-persistent-reject-{}",
            uuid::Uuid::new_v4()
        ));
        let state = Arc::new(
            StateDB::with_genesis_persistent(
                &[],
                &directory,
                arc_crypto::hash_bytes(b"persistent-sync-reject"),
            )
            .unwrap(),
        );
        let error = StateSyncManager::new()
            .sync_from_peer("this-peer-must-never-be-resolved.invalid:1", &state)
            .await
            .expect_err("persistent legacy snapshot sync must fail closed");
        assert!(matches!(
            error,
            SyncError::StateError(arc_state::StateError::UnauthenticatedPersistentStateSync)
        ));
        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
