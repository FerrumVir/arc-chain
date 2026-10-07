//! Research-7's pipeline latency model (§2.1), shared by the placement
//! optimizer and the benchmark's predicted-vs-measured column.
//!
//! Without speculation, one token's trip through `S` stages costs
//!
//! ```text
//! t_pass = Σ_s compute_s + Σ_hops (r_h / 2 + o + bytes_h · 8 / uplink_h)
//! ```
//!
//! where hop `h` leaves stage `h` (the last hop returns token ids to the
//! first stage). Per-answer speed is `1000 / t_pass` tok/s. With `m`
//! sequences in flight, aggregate speed is capped by both the ring
//! (`m · 1000 / t_pass`) and the slowest stage (`1000 / max_s busy_s`, where
//! a stage is busy for its compute plus the time its uplink carries its
//! output).

/// One network hop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HopCost {
    pub rtt_ms: f64,
    /// Per-message software overhead, ms (research-7 default 1 ms WAN; 0.3 ms
    /// for an optimized transport).
    pub overhead_ms: f64,
    /// Sender uplink, Mbit/s (`None` = unbounded).
    pub uplink_mbps: Option<f64>,
    /// Bytes this hop carries per token.
    pub bytes: f64,
}

impl HopCost {
    pub fn serialization_ms(&self) -> f64 {
        match self.uplink_mbps {
            Some(m) if m > 0.0 => self.bytes * 8.0 / (m * 1e3),
            _ => 0.0,
        }
    }

    pub fn ms(&self) -> f64 {
        self.rtt_ms / 2.0 + self.overhead_ms + self.serialization_ms()
    }
}

/// Predicted trip time for one token, ms.
pub fn pass_ms(compute_ms: &[f64], hops: &[HopCost]) -> f64 {
    compute_ms.iter().sum::<f64>() + hops.iter().map(HopCost::ms).sum::<f64>()
}

/// Predicted aggregate tok/s with `in_flight` sequences, each stage busy for
/// its compute plus its outgoing uplink time.
pub fn aggregate_tok_s(compute_ms: &[f64], hops: &[HopCost], in_flight: u32) -> f64 {
    let pass = pass_ms(compute_ms, hops);
    let busiest = compute_ms
        .iter()
        .enumerate()
        .map(|(i, c)| c + hops.get(i).map_or(0.0, HopCost::serialization_ms))
        .fold(0.0f64, f64::max);
    let ring = in_flight.max(1) as f64 * 1000.0 / pass.max(1e-9);
    let stage = 1000.0 / busiest.max(1e-9);
    ring.min(stage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(rtt: f64) -> HopCost {
        HopCost {
            rtt_ms: rtt,
            overhead_ms: 1.0,
            uplink_mbps: Some(50.0),
            // research-7's "16 KiB per position" is evaluated as 16,000 bytes
            // in its tables (S=4, r=20 → 51.7 ms only matches that).
            bytes: 16_000.0,
        }
    }

    #[test]
    fn reproduces_research7_network_table() {
        // research-7 §2.3, S=2, k=0: r=10 → 14.6 ms, r=40 → 44.6 ms. The
        // return hop carries token ids only (no serialization).
        let mut back = hop(10.0);
        back.bytes = 0.0;
        let t = pass_ms(&[], &[hop(10.0), back]);
        assert!((t - 14.6).abs() < 0.05, "{t}");
        let mut back = hop(40.0);
        back.bytes = 0.0;
        let t = pass_ms(&[], &[hop(40.0), back]);
        assert!((t - 44.6).abs() < 0.05, "{t}");
        // S=4, k=0, r=20 → 51.7 ms.
        let mut hops = vec![hop(20.0); 4];
        hops[3].bytes = 0.0;
        let t = pass_ms(&[], &hops);
        assert!((t - 51.7).abs() < 0.05, "{t}");
    }

    #[test]
    fn aggregate_is_capped_by_ring_and_by_slowest_stage() {
        let hops = vec![hop(20.0), hop(20.0)];
        let one = aggregate_tok_s(&[10.0, 10.0], &hops, 1);
        assert!((one - 1000.0 / pass_ms(&[10.0, 10.0], &hops)).abs() < 1e-6);
        let many = aggregate_tok_s(&[10.0, 30.0], &hops, 1000);
        let busiest = 30.0 + hops[1].serialization_ms();
        assert!((many - 1000.0 / busiest).abs() < 1e-6);
    }
}
