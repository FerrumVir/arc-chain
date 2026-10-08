//! WAN simulation for a stage link: per-hop delay, jitter and a bounded
//! uplink, layered over any [`StageSink`].
//!
//! Model, per message of `b` bytes on a hop with round-trip time `r`:
//!
//! * the sender's uplink is busy for `b · 8 / uplink` and messages queue
//!   behind each other on it (the call to `send_frame` blocks for that time,
//!   as a socket write on a saturated uplink would);
//! * the message then arrives `r / 2 + jitter` later, where jitter is uniform
//!   in `[-j, +j]` from a seeded generator (so runs are repeatable) and
//!   delivery stays first-in first-out, as on one TCP stream.
//!
//! The delay is held on a separate thread, so a slow link never stalls the
//! sender beyond the uplink time.

use super::wire::StageSink;
use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// One hop's simulated network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkProfile {
    /// Round-trip time between the two stages, ms. One-way delay is half.
    pub rtt_ms: f64,
    /// Jitter half-width, ms (uniform in `[-j, +j]` on the one-way delay).
    pub jitter_ms: f64,
    /// Sender uplink, Mbit/s. `None` = unbounded.
    pub uplink_mbps: Option<f64>,
    /// Seed for the jitter generator.
    pub seed: u64,
}

impl LinkProfile {
    pub fn ideal() -> Self {
        Self {
            rtt_ms: 0.0,
            jitter_ms: 0.0,
            uplink_mbps: None,
            seed: 0,
        }
    }

    pub fn is_ideal(&self) -> bool {
        self.rtt_ms <= 0.0 && self.jitter_ms <= 0.0 && self.uplink_mbps.is_none()
    }

    /// Time the uplink is busy sending `bytes`.
    pub fn serialization(&self, bytes: usize) -> Duration {
        match self.uplink_mbps {
            Some(mbps) if mbps > 0.0 => Duration::from_secs_f64(bytes as f64 * 8.0 / (mbps * 1e6)),
            _ => Duration::ZERO,
        }
    }
}

/// How [`sleep_until`] waits on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerMode {
    /// `thread::sleep` is accurate: sleep coarsely, spin the last 250 µs.
    SleepThenSpin,
    /// The OS coalesces timers (e.g. a macOS process at background QoS, where
    /// a 5 ms sleep can take 35 ms): yield-spin the whole wait so injected
    /// delays stay accurate.
    Spin,
}

/// The host's timer mode and the measured duration of a 1 ms sleep,
/// calibrated once per process.
pub fn timer_calibration() -> (TimerMode, Duration) {
    static CAL: std::sync::OnceLock<(TimerMode, Duration)> = std::sync::OnceLock::new();
    *CAL.get_or_init(|| {
        let mut worst = Duration::ZERO;
        for _ in 0..5 {
            let t = Instant::now();
            std::thread::sleep(Duration::from_millis(1));
            worst = worst.max(t.elapsed());
        }
        let mode = if worst > Duration::from_micros(1_800) {
            TimerMode::Spin
        } else {
            TimerMode::SleepThenSpin
        };
        (mode, worst)
    })
}

/// Wait until `deadline` with sub-millisecond accuracy (see [`TimerMode`]).
pub fn sleep_until(deadline: Instant) {
    let spin_only = timer_calibration().0 == TimerMode::Spin;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let left = deadline - now;
        if !spin_only && left > Duration::from_micros(400) {
            std::thread::sleep(left - Duration::from_micros(250));
        } else {
            std::thread::yield_now();
        }
    }
}

struct SplitMix(u64);

impl SplitMix {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A sink whose messages cross a simulated WAN hop before reaching `inner`.
pub struct ShapedSink {
    profile: LinkProfile,
    link_free: Instant,
    last_delivery: Instant,
    rng: SplitMix,
    tx: Option<mpsc::Sender<(Instant, Vec<u8>)>>,
    error: Arc<Mutex<Option<io::Error>>>,
    worker: Option<JoinHandle<()>>,
}

impl ShapedSink {
    pub fn new<S: StageSink + 'static>(mut inner: S, profile: LinkProfile) -> Self {
        let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
        let error = Arc::new(Mutex::new(None));
        let err = error.clone();
        let worker = std::thread::Builder::new()
            .name("stage-net-wan".into())
            .spawn(move || {
                for (deliver_at, frame) in rx {
                    sleep_until(deliver_at);
                    if let Err(e) = inner.send_frame(&frame) {
                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                        return;
                    }
                }
            })
            .expect("spawn WAN shaper thread");
        let now = Instant::now();
        Self {
            profile,
            link_free: now,
            last_delivery: now,
            rng: SplitMix(profile.seed),
            tx: Some(tx),
            error,
            worker: Some(worker),
        }
    }
}

impl StageSink for ShapedSink {
    fn send_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        if let Some(e) = self.error.lock().unwrap_or_else(|p| p.into_inner()).take() {
            return Err(e);
        }
        let start = Instant::now().max(self.link_free);
        self.link_free = start + self.profile.serialization(frame.len());
        // The uplink is ours until the last bit leaves.
        sleep_until(self.link_free);
        let jitter = if self.profile.jitter_ms > 0.0 {
            (self.rng.next_f64() * 2.0 - 1.0) * self.profile.jitter_ms
        } else {
            0.0
        };
        let one_way_ms = (self.profile.rtt_ms / 2.0 + jitter).max(0.0);
        let deliver_at =
            (self.link_free + Duration::from_secs_f64(one_way_ms / 1e3)).max(self.last_delivery);
        self.last_delivery = deliver_at;
        self.tx
            .as_ref()
            .expect("sender present until drop")
            .send((deliver_at, frame.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "WAN shaper stopped"))
    }
}

impl Drop for ShapedSink {
    fn drop(&mut self) {
        // Close the queue, then let in-flight messages land before the inner
        // sink (and its socket) is dropped.
        self.tx.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage_net::wire::{StageSource, mem_link};

    #[test]
    fn delay_and_uplink_are_applied_in_order() {
        let (sink, mut source) = mem_link();
        let profile = LinkProfile {
            rtt_ms: 20.0,
            jitter_ms: 2.0,
            uplink_mbps: Some(8.0),
            seed: 1,
        };
        let mut shaped = ShapedSink::new(sink, profile);
        let t0 = Instant::now();
        // 10 KB at 8 Mb/s = 10 ms of uplink each.
        for i in 0..3u8 {
            shaped.send_frame(&vec![i; 10_000]).expect("send");
        }
        let after_send = t0.elapsed();
        assert!(
            after_send >= Duration::from_millis(29),
            "uplink time not charged: {after_send:?}"
        );
        let mut buf = Vec::new();
        for i in 0..3u8 {
            assert!(source.recv_frame(&mut buf).expect("recv"));
            assert_eq!(buf[0], i, "FIFO order broken");
        }
        // Last message: 30 ms uplink + 10 ms ± 2 ms one-way.
        let total = t0.elapsed();
        assert!(total >= Duration::from_millis(37), "{total:?}");
        drop(shaped);
        assert!(!source.recv_frame(&mut buf).expect("eof"));
    }

    #[test]
    fn ideal_profile_is_free() {
        let p = LinkProfile::ideal();
        assert!(p.is_ideal());
        assert_eq!(p.serialization(1 << 20), Duration::ZERO);
    }
}
