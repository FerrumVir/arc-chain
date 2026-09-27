//! Link measurements (S2): what a coordinator-worker path costs.

use serde::{Deserialize, Serialize};

/// One probe exchange: round trip, bytes moved, whether it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub rtt_us: u64,
    pub bytes: u64,
    pub transfer_us: u64,
    pub failed: bool,
}

/// Summary of a link, from probes. Placement uses the pessimistic figures
/// (p95 RTT, worst observed bandwidth), because a stage waits for its
/// slowest participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkMeasurement {
    pub samples: u32,
    pub rtt_median_us: u64,
    pub rtt_p95_us: u64,
    pub jitter_us: u64,
    /// Bytes per second, from the slowest bulk probe.
    pub bandwidth_bps: u64,
    /// Failures per thousand probes.
    pub failure_per_mille: u32,
    /// Height (or a monotonic epoch) the measurement was taken at.
    pub measured_at: u64,
    /// True when conditions were injected (tests, emulated networks); such a
    /// measurement is evidence about the emulator, never about a real path.
    pub simulated: bool,
}

pub fn summarize(probes: &[Probe], measured_at: u64, simulated: bool) -> Option<LinkMeasurement> {
    let ok: Vec<&Probe> = probes.iter().filter(|p| !p.failed).collect();
    if ok.is_empty() {
        return None;
    }
    let mut rtts: Vec<u64> = ok.iter().map(|p| p.rtt_us).collect();
    rtts.sort_unstable();
    let median = rtts[rtts.len() / 2];
    let p95 = rtts[((rtts.len() * 95).div_ceil(100))
        .saturating_sub(1)
        .min(rtts.len() - 1)];
    let jitter = rtts[rtts.len() - 1] - rtts[0];
    let bandwidth = ok
        .iter()
        .filter(|p| p.bytes > 0 && p.transfer_us > 0)
        .map(|p| ((p.bytes as u128 * 1_000_000) / p.transfer_us as u128) as u64)
        .min()
        .unwrap_or(0);
    let failed = probes.len() - ok.len();
    Some(LinkMeasurement {
        samples: probes.len() as u32,
        rtt_median_us: median,
        rtt_p95_us: p95,
        jitter_us: jitter,
        bandwidth_bps: bandwidth,
        failure_per_mille: ((failed * 1000) / probes.len()) as u32,
        measured_at,
        simulated,
    })
}

/// Cumulative path counters from one transport connection (quinn's
/// `Connection::stats().path`), tagged with that connection's generation so
/// that a reconnect is never read as a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathCounters {
    pub generation: u64,
    pub sent_packets: u64,
    pub lost_packets: u64,
}

/// Loss per mille over the window between two snapshots of the SAME
/// connection. quinn's counters are cumulative for the life of a connection,
/// so only a delta says anything about the recent path. `None` when the
/// snapshots come from different connections (a reconnect starts a new
/// window), when a counter went backwards, or when nothing was sent in
/// between. Packets sent before the window can be declared lost inside it,
/// so the figure is capped at 1000.
pub fn windowed_loss_per_mille(earlier: &PathCounters, later: &PathCounters) -> Option<u32> {
    if earlier.generation != later.generation {
        return None;
    }
    let sent = later.sent_packets.checked_sub(earlier.sent_packets)?;
    let lost = later.lost_packets.checked_sub(earlier.lost_packets)?;
    if sent == 0 {
        return None;
    }
    let per_mille = (u128::from(lost) * 1000 / u128::from(sent)).min(1000);
    u32::try_from(per_mille).ok()
}
