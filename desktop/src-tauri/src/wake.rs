//! Native timing signals for work that must not depend on WebView timers.
//!
//! The main window is usually hidden in the tray. A hidden WebView's timers
//! can be throttled (Tauri's `backgroundThrottling: "disabled"` is honoured
//! only on macOS 14 and later), and nothing in the WebView learns that the
//! computer slept. This module runs in Rust and sends the WebView an
//! `app-signal` event:
//!
//! - `tick` every [`SIGNAL_TICK`], so the update controller can run checks
//!   whose wall-clock time has come even if its own timers were held back;
//! - `wake` after the computer wakes from sleep;
//! - `focus` and `shown`, sent from `lib.rs` and `tray.rs` when the main
//!   window gains focus or the tray shows it.
//!
//! A wake is seen as a gap: a short interval tick that arrives much later
//! than it should, measured on either the wall clock or the monotonic clock.
//! On macOS and Linux the monotonic clock stops during sleep, so the wall
//! clock shows the gap; where the monotonic clock keeps counting, the late
//! tick shows it. Rust code waiting for a wake subscribes with [`subscribe`].

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use tauri::{AppHandle, Emitter};
use tokio::sync::watch;

/// The event the WebView listens to.
pub(crate) const APP_SIGNAL_EVENT: &str = "app-signal";

/// How often the detector samples the clocks.
const SAMPLE_EVERY: Duration = Duration::from_secs(15);
/// A sample this much later than expected counts as a wake.
const WAKE_GAP: Duration = Duration::from_secs(60);
/// How often the WebView gets a `tick`.
const SIGNAL_TICK: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SignalReason {
    Tick,
    Wake,
    Focus,
    Shown,
}

#[derive(Clone, Debug, serde::Serialize)]
struct AppSignal {
    reason: SignalReason,
}

/// Send `reason` to the WebView. Best effort.
pub(crate) fn emit_signal(app: &AppHandle, reason: SignalReason) {
    let _ = app.emit(APP_SIGNAL_EVENT, AppSignal { reason });
}

/// Counts wakes. The sender lives for the process.
fn wake_sender() -> &'static watch::Sender<u64> {
    static WAKES: OnceLock<watch::Sender<u64>> = OnceLock::new();
    WAKES.get_or_init(|| watch::channel(0).0)
}

/// A receiver whose `changed()` resolves at the next wake.
pub(crate) fn subscribe() -> watch::Receiver<u64> {
    wake_sender().subscribe()
}

/// Compares consecutive clock samples.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WakeDetector {
    last_monotonic: Instant,
    last_wall: SystemTime,
    period: Duration,
}

impl WakeDetector {
    pub(crate) fn new(monotonic: Instant, wall: SystemTime, period: Duration) -> Self {
        Self {
            last_monotonic: monotonic,
            last_wall: wall,
            period,
        }
    }

    /// Record a sample; `true` when the time since the last one, on either
    /// clock, exceeds the period by more than [`WAKE_GAP`]. A wall clock set
    /// backwards counts as no time.
    pub(crate) fn observe(&mut self, monotonic: Instant, wall: SystemTime) -> bool {
        let monotonic_elapsed = monotonic.saturating_duration_since(self.last_monotonic);
        let wall_elapsed = wall
            .duration_since(self.last_wall)
            .unwrap_or(Duration::ZERO);
        self.last_monotonic = monotonic;
        self.last_wall = wall;
        monotonic_elapsed.max(wall_elapsed) > self.period + WAKE_GAP
    }
}

/// Start the detector and the `tick` signal. Call once, from setup.
pub(crate) fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut samples = tokio::time::interval(SAMPLE_EVERY);
        samples.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        samples.tick().await;
        let mut detector = WakeDetector::new(Instant::now(), SystemTime::now(), SAMPLE_EVERY);
        let mut last_signal_tick = Instant::now();
        loop {
            samples.tick().await;
            let now = Instant::now();
            if detector.observe(now, SystemTime::now()) {
                tracing::info!("computer woke from sleep");
                wake_sender().send_modify(|wakes| *wakes = wakes.wrapping_add(1));
                emit_signal(&app, SignalReason::Wake);
            }
            if now.saturating_duration_since(last_signal_tick) >= SIGNAL_TICK {
                last_signal_tick = now;
                emit_signal(&app, SignalReason::Tick);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: Duration = Duration::from_secs(15);

    #[test]
    fn regular_samples_are_not_a_wake() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut detector = WakeDetector::new(start, wall, PERIOD);
        for step in 1..=10u64 {
            let late = Duration::from_secs(step * 15 + 2);
            assert!(!detector.observe(start + late, wall + late));
        }
    }

    #[test]
    fn a_wall_clock_gap_with_a_stopped_monotonic_clock_is_a_wake() {
        // macOS and Linux: the monotonic clock does not count sleep.
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut detector = WakeDetector::new(start, wall, PERIOD);
        assert!(detector.observe(
            start + Duration::from_secs(15),
            wall + Duration::from_secs(3 * 3600)
        ));
        // The next regular sample is not another wake.
        assert!(!detector.observe(
            start + Duration::from_secs(30),
            wall + Duration::from_secs(3 * 3600 + 15)
        ));
    }

    #[test]
    fn a_late_monotonic_sample_is_a_wake() {
        // Where the monotonic clock counts sleep, the tick itself is late.
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut detector = WakeDetector::new(start, wall, PERIOD);
        let slept = Duration::from_secs(40 * 60);
        assert!(detector.observe(start + slept, wall + slept));
    }

    #[test]
    fn a_wall_clock_set_backwards_is_not_a_wake() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut detector = WakeDetector::new(start, wall, PERIOD);
        assert!(!detector.observe(
            start + Duration::from_secs(15),
            wall - Duration::from_secs(3600)
        ));
    }

    #[test]
    fn signal_reasons_serialize_as_the_webview_expects() {
        let json = serde_json::to_string(&AppSignal {
            reason: SignalReason::Shown,
        })
        .unwrap();
        assert_eq!(json, r#"{"reason":"shown"}"#);
        for (reason, name) in [
            (SignalReason::Tick, "tick"),
            (SignalReason::Wake, "wake"),
            (SignalReason::Focus, "focus"),
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), name);
        }
    }
}
