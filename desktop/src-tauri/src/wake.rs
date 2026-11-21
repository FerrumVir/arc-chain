use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use tauri::{AppHandle, Emitter};
use tokio::sync::watch;

pub(crate) const APP_SIGNAL_EVENT: &str = "app-signal";

const SAMPLE_EVERY: Duration = Duration::from_secs(15);
const WAKE_GAP: Duration = Duration::from_secs(60);
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

pub(crate) fn emit_signal(app: &AppHandle, reason: SignalReason) {
    let _ = app.emit(APP_SIGNAL_EVENT, AppSignal { reason });
}

fn wake_sender() -> &'static watch::Sender<u64> {
    static WAKES: OnceLock<watch::Sender<u64>> = OnceLock::new();
    WAKES.get_or_init(|| watch::channel(0).0)
}

pub(crate) fn subscribe() -> watch::Receiver<u64> {
    wake_sender().subscribe()
}

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
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut detector = WakeDetector::new(start, wall, PERIOD);
        assert!(detector.observe(
            start + Duration::from_secs(15),
            wall + Duration::from_secs(3 * 3600)
        ));
        assert!(!detector.observe(
            start + Duration::from_secs(30),
            wall + Duration::from_secs(3 * 3600 + 15)
        ));
    }

    #[test]
    fn a_late_monotonic_sample_is_a_wake() {
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
