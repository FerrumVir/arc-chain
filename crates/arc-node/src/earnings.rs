//! Persistent earnings tracker — survives node restarts.
//!
//! Stores inference count and ARC earned to `<data_dir>/earnings.json`.
//! The file is atomically updated (write-to-temp + rename) on every inference
//! to prevent data loss on crash or SIGTERM.
//!
//! On startup, loads existing earnings from disk so counters resume where
//! they left off instead of resetting to zero.
//!
//! Also maintains an earnings history log (`<data_dir>/earnings_history.json`)
//! with timestamped data points for charting earnings over time.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// Maximum history points stored on disk. Older points are downsampled
/// (merged into 5-minute buckets) to keep the file bounded.
const MAX_HISTORY_POINTS: usize = 1000;

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

/// A single point in the earnings history timeline.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EarningsHistoryPoint {
    /// ISO-8601 timestamp when this data point was recorded.
    pub timestamp: String,
    /// Unix epoch seconds (for easy sorting/charting).
    pub epoch_secs: i64,
    /// Cumulative ARC earned at this point.
    pub total_arc: u64,
    /// Cumulative inference count at this point.
    pub total_inferences: u64,
}

/// On-disk format for earnings history.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EarningsHistory {
    pub points: Vec<EarningsHistoryPoint>,
}

/// Thread-safe earnings tracker with disk persistence.
///
/// Wraps the same `AtomicU64` counters used by consensus and RPC,
/// but adds load-from-disk on init and save-to-disk on each increment.
pub struct EarningsTracker {
    pub inference_count: Arc<AtomicU64>,
    pub inference_earned: Arc<AtomicU64>,
    path: PathBuf,
    history_path: PathBuf,
    /// In-memory earnings history, persisted on each inference.
    history: Mutex<Vec<EarningsHistoryPoint>>,
}

impl EarningsTracker {
    /// Create a new tracker, loading existing earnings from disk if available.
    ///
    /// `data_dir` is the node's data directory (e.g., `./arc-data` or `~/.arc-chain`).
    /// Earnings file will be `<data_dir>/earnings.json`.
    /// History file will be `<data_dir>/earnings_history.json`.
    pub fn new(data_dir: &str) -> Self {
        let path = PathBuf::from(data_dir).join("earnings.json");
        let history_path = PathBuf::from(data_dir).join("earnings_history.json");
        let (count, earned) = Self::load_from_disk(&path);
        let history = Self::load_history_from_disk(&history_path);

        if count > 0 || earned > 0 {
            info!(
                count = count,
                earned = earned,
                history_points = history.len(),
                path = %path.display(),
                "Restored earnings from disk"
            );
        }

        Self {
            inference_count: Arc::new(AtomicU64::new(count)),
            inference_earned: Arc::new(AtomicU64::new(earned)),
            path,
            history_path,
            history: Mutex::new(history),
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

        // Append history data point
        let now = chrono::Utc::now();
        let point = EarningsHistoryPoint {
            timestamp: now.to_rfc3339(),
            epoch_secs: now.timestamp(),
            total_arc: self.inference_earned.load(Ordering::Relaxed),
            total_inferences: self.inference_count.load(Ordering::Relaxed),
        };
        if let Ok(mut history) = self.history.lock() {
            history.push(point);
            // Downsample if we exceed the limit
            if history.len() > MAX_HISTORY_POINTS {
                *history = Self::downsample(&history, MAX_HISTORY_POINTS / 2);
            }
        }

        self.save_to_disk();
        self.save_history_to_disk();
    }

    /// Get a clone of the current earnings history for API responses.
    pub fn get_history(&self) -> Vec<EarningsHistoryPoint> {
        self.history.lock().unwrap_or_else(|e| e.into_inner()).clone()
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

    /// Save earnings history to disk (atomic write).
    fn save_history_to_disk(&self) {
        let points = self.history.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let history = EarningsHistory { points };
        let tmp_path = self.history_path.with_extension("json.tmp");
        match serde_json::to_string(&history) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp_path, json.as_bytes()) {
                    warn!(error = %e, "Failed to write earnings history temp file");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp_path, &self.history_path) {
                    warn!(error = %e, "Failed to rename earnings history file");
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to serialize earnings history");
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

    /// Load earnings history from disk. Returns empty vec if file missing or corrupt.
    fn load_history_from_disk(path: &PathBuf) -> Vec<EarningsHistoryPoint> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                match serde_json::from_str::<EarningsHistory>(&contents) {
                    Ok(data) => data.points,
                    Err(e) => {
                        warn!(error = %e, path = %path.display(), "Corrupt earnings history — starting fresh");
                        Vec::new()
                    }
                }
            }
            Err(_) => Vec::new(),
        }
    }

    /// Downsample history points by merging into time buckets.
    /// Keeps the most recent points at full resolution and compresses older ones.
    fn downsample(points: &[EarningsHistoryPoint], target: usize) -> Vec<EarningsHistoryPoint> {
        if points.len() <= target {
            return points.to_vec();
        }
        // Keep the newest half at full resolution, downsample the older half
        let split = points.len() / 2;
        let old = &points[..split];
        let recent = &points[split..];

        // Merge old points into 5-minute buckets (300 seconds)
        let bucket_size = 300i64;
        let mut buckets: Vec<EarningsHistoryPoint> = Vec::new();
        for p in old {
            let bucket_epoch = (p.epoch_secs / bucket_size) * bucket_size;
            if let Some(last) = buckets.last() {
                let last_bucket = (last.epoch_secs / bucket_size) * bucket_size;
                if last_bucket == bucket_epoch {
                    // Same bucket — keep the later point (higher totals)
                    *buckets.last_mut().unwrap() = p.clone();
                    continue;
                }
            }
            buckets.push(p.clone());
        }

        // Combine downsampled old + full-resolution recent
        buckets.extend_from_slice(recent);
        buckets
    }
}
