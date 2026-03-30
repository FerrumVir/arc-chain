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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create an EarningsTracker using a fresh temp directory.
    fn tracker_in_tmpdir() -> (EarningsTracker, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let tracker = EarningsTracker::new(dir.path().to_str().unwrap());
        (tracker, dir)
    }

    // ── New tracker defaults ────────────────────────────────────────────

    #[test]
    fn new_tracker_starts_at_zero() {
        let (tracker, _dir) = tracker_in_tmpdir();
        assert_eq!(tracker.inference_count.load(Ordering::Relaxed), 0);
        assert_eq!(tracker.inference_earned.load(Ordering::Relaxed), 0);
        assert!(tracker.get_history().is_empty());
    }

    // ── record_inference ────────────────────────────────────────────────

    #[test]
    fn record_inference_increments_count_and_earned() {
        let (tracker, _dir) = tracker_in_tmpdir();
        tracker.record_inference(100);
        assert_eq!(tracker.inference_count.load(Ordering::Relaxed), 1);
        assert_eq!(tracker.inference_earned.load(Ordering::Relaxed), 100);

        tracker.record_inference(100);
        assert_eq!(tracker.inference_count.load(Ordering::Relaxed), 2);
        assert_eq!(tracker.inference_earned.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn record_inference_appends_history_point() {
        let (tracker, _dir) = tracker_in_tmpdir();
        tracker.record_inference(100);
        tracker.record_inference(100);
        tracker.record_inference(100);

        let history = tracker.get_history();
        assert_eq!(history.len(), 3);
        // Each point records cumulative totals
        assert_eq!(history[0].total_arc, 100);
        assert_eq!(history[0].total_inferences, 1);
        assert_eq!(history[2].total_arc, 300);
        assert_eq!(history[2].total_inferences, 3);
    }

    #[test]
    fn record_inference_with_custom_reward() {
        let (tracker, _dir) = tracker_in_tmpdir();
        tracker.record_inference(250);
        assert_eq!(tracker.inference_earned.load(Ordering::Relaxed), 250);
        tracker.record_inference(50);
        assert_eq!(tracker.inference_earned.load(Ordering::Relaxed), 300);
    }

    // ── Disk persistence ────────────────────────────────────────────────

    #[test]
    fn save_creates_earnings_json_on_disk() {
        let (tracker, dir) = tracker_in_tmpdir();
        tracker.record_inference(100);

        let earnings_path = dir.path().join("earnings.json");
        assert!(earnings_path.exists(), "earnings.json should be created after record_inference");

        let contents = fs::read_to_string(&earnings_path).unwrap();
        let data: EarningsData = serde_json::from_str(&contents).unwrap();
        assert_eq!(data.inference_count, 1);
        assert_eq!(data.inference_earned, 100);
        assert!(!data.last_updated.is_empty());
    }

    #[test]
    fn save_creates_history_json_on_disk() {
        let (tracker, dir) = tracker_in_tmpdir();
        tracker.record_inference(100);
        tracker.record_inference(100);

        let history_path = dir.path().join("earnings_history.json");
        assert!(history_path.exists(), "earnings_history.json should be created");

        let contents = fs::read_to_string(&history_path).unwrap();
        let history: EarningsHistory = serde_json::from_str(&contents).unwrap();
        assert_eq!(history.points.len(), 2);
    }

    #[test]
    fn load_restores_counters_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_str().unwrap();

        // First tracker: record 5 inferences
        {
            let t = EarningsTracker::new(dir_str);
            for _ in 0..5 {
                t.record_inference(100);
            }
            assert_eq!(t.inference_count.load(Ordering::Relaxed), 5);
            assert_eq!(t.inference_earned.load(Ordering::Relaxed), 500);
        }
        // Tracker dropped — simulates node shutdown

        // Second tracker: should restore from disk
        let t2 = EarningsTracker::new(dir_str);
        assert_eq!(t2.inference_count.load(Ordering::Relaxed), 5);
        assert_eq!(t2.inference_earned.load(Ordering::Relaxed), 500);
    }

    #[test]
    fn load_restores_history_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_str().unwrap();

        {
            let t = EarningsTracker::new(dir_str);
            t.record_inference(100);
            t.record_inference(100);
            t.record_inference(100);
        }

        let t2 = EarningsTracker::new(dir_str);
        let history = t2.get_history();
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].total_arc, 300);
    }

    #[test]
    fn load_from_empty_dir_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        // No files exist — should not panic, just return zeros
        let t = EarningsTracker::new(dir.path().to_str().unwrap());
        assert_eq!(t.inference_count.load(Ordering::Relaxed), 0);
        assert_eq!(t.inference_earned.load(Ordering::Relaxed), 0);
        assert!(t.get_history().is_empty());
    }

    #[test]
    fn load_from_corrupt_file_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        // Write garbage to earnings.json
        fs::write(dir.path().join("earnings.json"), "not valid json {{{").unwrap();
        fs::write(dir.path().join("earnings_history.json"), "also garbage").unwrap();

        let t = EarningsTracker::new(dir.path().to_str().unwrap());
        assert_eq!(t.inference_count.load(Ordering::Relaxed), 0);
        assert_eq!(t.inference_earned.load(Ordering::Relaxed), 0);
        assert!(t.get_history().is_empty());
    }

    // ── Downsampling ────────────────────────────────────────────────────

    #[test]
    fn downsample_noop_when_under_target() {
        let points: Vec<EarningsHistoryPoint> = (0..10)
            .map(|i| EarningsHistoryPoint {
                timestamp: format!("2026-01-01T00:00:{}Z", i),
                epoch_secs: 1700000000 + i * 60,
                total_arc: (i as u64 + 1) * 100,
                total_inferences: i as u64 + 1,
            })
            .collect();

        let result = EarningsTracker::downsample(&points, 20);
        assert_eq!(result.len(), 10, "should not downsample when under target");
    }

    #[test]
    fn downsample_reduces_point_count() {
        // Create 100 points spanning a long time range (1 point per second)
        let points: Vec<EarningsHistoryPoint> = (0..100)
            .map(|i| EarningsHistoryPoint {
                timestamp: format!("2026-01-01T00:00:{}Z", i),
                epoch_secs: 1700000000 + i,
                total_arc: (i as u64 + 1) * 100,
                total_inferences: i as u64 + 1,
            })
            .collect();

        let result = EarningsTracker::downsample(&points, 50);
        assert!(
            result.len() < 100,
            "should have fewer points after downsampling, got {}",
            result.len()
        );
        // Recent half (50 points) should be preserved at full resolution
        // Old half (50 points within one 5-min bucket) should collapse
    }

    #[test]
    fn downsample_preserves_recent_points() {
        // 20 points: first 10 are old (same 5-min bucket), last 10 are recent
        let points: Vec<EarningsHistoryPoint> = (0..20)
            .map(|i| EarningsHistoryPoint {
                timestamp: format!("t{}", i),
                epoch_secs: if i < 10 {
                    1700000000 + i // All within one 5-min bucket
                } else {
                    1700000000 + (i * 300) // Each in its own bucket
                },
                total_arc: (i as u64 + 1) * 100,
                total_inferences: i as u64 + 1,
            })
            .collect();

        let result = EarningsTracker::downsample(&points, 10);
        // The recent 10 points should ALL be there (indices 10..20)
        let recent_arcs: Vec<u64> = result.iter().rev().take(10).rev().map(|p| p.total_arc).collect();
        let expected_arcs: Vec<u64> = (10..20).map(|i| (i + 1) * 100).collect();
        assert_eq!(recent_arcs, expected_arcs, "recent points must be preserved exactly");
    }

    // ── History epoch_secs ordering ─────────────────────────────────────

    #[test]
    fn history_points_have_increasing_epoch() {
        let (tracker, _dir) = tracker_in_tmpdir();
        for _ in 0..5 {
            tracker.record_inference(100);
        }
        let history = tracker.get_history();
        for window in history.windows(2) {
            assert!(
                window[1].epoch_secs >= window[0].epoch_secs,
                "history must be in chronological order"
            );
        }
    }

    // ── Atomic write safety ─────────────────────────────────────────────

    #[test]
    fn no_tmp_file_left_after_save() {
        let (tracker, dir) = tracker_in_tmpdir();
        tracker.record_inference(100);

        let tmp_earnings = dir.path().join("earnings.json.tmp");
        let tmp_history = dir.path().join("earnings_history.json.tmp");
        assert!(!tmp_earnings.exists(), "temp file should be renamed away");
        assert!(!tmp_history.exists(), "temp history file should be renamed away");
    }
}
