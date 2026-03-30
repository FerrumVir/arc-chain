//! Persistent earnings tracker — survives node restarts.
//!
//! Stores inference count and ARC earned to `<data_dir>/earnings.json`.
//! The file is atomically updated (write-to-temp + rename) on every inference
//! to prevent data loss on crash or SIGTERM.
//!
//! On startup, loads existing earnings from disk so counters resume where
//! they left off instead of resetting to zero.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

/// On-disk format for earnings state.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EarningsData {
    /// Total inference requests completed.
    pub inference_count: u64,
    /// Total ARC earned (testnet: 100 ARC per inference).
    pub inference_earned: u64,
    /// ISO-8601 timestamp of last update.
    pub last_updated: String,
}

/// Thread-safe earnings tracker with disk persistence.
///
/// Wraps the same `AtomicU64` counters used by consensus and RPC,
/// but adds load-from-disk on init and save-to-disk on each increment.
pub struct EarningsTracker {
    pub inference_count: Arc<AtomicU64>,
    pub inference_earned: Arc<AtomicU64>,
    path: PathBuf,
}

impl EarningsTracker {
    /// Create a new tracker, loading existing earnings from disk if available.
    ///
    /// `data_dir` is the node's data directory (e.g., `./arc-data` or `~/.arc-chain`).
    /// Earnings file will be `<data_dir>/earnings.json`.
    pub fn new(data_dir: &str) -> Self {
        let path = PathBuf::from(data_dir).join("earnings.json");
        let (count, earned) = Self::load_from_disk(&path);

        if count > 0 || earned > 0 {
            info!(
                count = count,
                earned = earned,
                path = %path.display(),
                "Restored earnings from disk"
            );
        }

        Self {
            inference_count: Arc::new(AtomicU64::new(count)),
            inference_earned: Arc::new(AtomicU64::new(earned)),
            path,
        }
    }

    /// Record a completed inference and persist to disk.
    ///
    /// Called from both the RPC layer (direct inference) and the consensus
    /// loop (P2P inference requests). The atomic increment is lock-free;
    /// only the disk write uses filesystem atomicity (write-tmp + rename).
    pub fn record_inference(&self, reward: u64) {
        self.inference_count.fetch_add(1, Ordering::Relaxed);
        self.inference_earned.fetch_add(reward, Ordering::Relaxed);
        self.save_to_disk();
    }

    /// Force a save of current state to disk (e.g., on graceful shutdown).
    pub fn save_to_disk(&self) {
        let data = EarningsData {
            inference_count: self.inference_count.load(Ordering::Relaxed),
            inference_earned: self.inference_earned.load(Ordering::Relaxed),
            last_updated: chrono::Utc::now().to_rfc3339(),
        };

        // Atomic write: write to .tmp, then rename. This prevents partial
        // reads if the process is killed mid-write.
        let tmp_path = self.path.with_extension("json.tmp");
        match serde_json::to_string_pretty(&data) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp_path, json.as_bytes()) {
                    warn!(error = %e, "Failed to write earnings temp file");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp_path, &self.path) {
                    warn!(error = %e, "Failed to rename earnings file");
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to serialize earnings");
            }
        }
    }

    /// Load earnings from disk, returning (count, earned). Returns (0, 0) if
    /// the file doesn't exist or is corrupt.
    fn load_from_disk(path: &PathBuf) -> (u64, u64) {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                match serde_json::from_str::<EarningsData>(&contents) {
                    Ok(data) => (data.inference_count, data.inference_earned),
                    Err(e) => {
                        warn!(error = %e, path = %path.display(), "Corrupt earnings file — starting fresh");
                        (0, 0)
                    }
                }
            }
            Err(_) => (0, 0), // File doesn't exist yet — first run
        }
    }
}
