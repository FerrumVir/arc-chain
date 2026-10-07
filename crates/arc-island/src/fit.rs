//! Capacity fitting, heterogeneity-aware partitioning and ring order.
//!
//! Partitioning gives each stage a contiguous run of layer units sized to its
//! memory and speed (research-6 §6.4 "DP / water-filling", Halda/Parallax
//! style). The objective is lexicographic: first the slowest stage's time
//! (pipeline throughput), then the sum of stage times (single-stream
//! latency). Everything is integer so the choice is reproducible.

use crate::device::RttSource;
use crate::model::ModelSpec;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// What one ordered stage offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageCapacity {
    pub usable_bytes: u64,
    pub bandwidth_mb_s: u64,
}

/// One stage of a partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub layers: Range<usize>,
    pub weight_bytes: u64,
    pub kv_bytes: u64,
    /// Memory-bound time for one position at batch 1, nanoseconds.
    pub time_ns: u64,
}

impl Stage {
    pub fn need_bytes(&self) -> u64 {
        self.weight_bytes + self.kv_bytes
    }
}

/// Integer bytes read at batch 1 for one unit: fixed plus the selected
/// experts.
fn active_b1(model: &ModelSpec, i: usize) -> u64 {
    let l = &model.layers[i];
    l.fixed_active_bytes + u64::from(l.experts_per_token) * l.expert_bytes
}

struct Prefix {
    weight: Vec<u64>,
    kv: Vec<u64>,
    active: Vec<u64>,
}

impl Prefix {
    fn new(model: &ModelSpec, kv_positions: u64) -> Self {
        let n = model.layers.len();
        let mut p = Self {
            weight: vec![0; n + 1],
            kv: vec![0; n + 1],
            active: vec![0; n + 1],
        };
        for (i, l) in model.layers.iter().enumerate() {
            p.weight[i + 1] = p.weight[i] + l.weight_bytes;
            p.kv[i + 1] = p.kv[i] + l.kv_bytes_per_position * kv_positions;
            p.active[i + 1] = p.active[i] + active_b1(model, i);
        }
        p
    }

    fn stage(&self, r: Range<usize>, bandwidth_mb_s: u64) -> Stage {
        let active = self.active[r.end] - self.active[r.start];
        Stage {
            weight_bytes: self.weight[r.end] - self.weight[r.start],
            kv_bytes: self.kv[r.end] - self.kv[r.start],
            // bytes / (MB/s) = bytes × 1,000 / bw ns.
            time_ns: active.saturating_mul(1000) / bandwidth_mb_s.max(1),
            layers: r,
        }
    }
}

/// Assigns contiguous, non-empty layer runs to `caps` in order so that every
/// stage holds its weights plus KV for `kv_positions` positions. Returns
/// `None` when no such assignment exists.
pub fn partition(
    model: &ModelSpec,
    caps: &[StageCapacity],
    kv_positions: u64,
) -> Option<Vec<Stage>> {
    let n = model.layers.len();
    let s = caps.len();
    if s == 0 || s > n {
        return None;
    }
    let p = Prefix::new(model, kv_positions);
    let total_need = p.weight[n] + p.kv[n];
    if caps.iter().map(|c| c.usable_bytes).sum::<u64>() < total_need {
        return None;
    }
    // best[t][j]: stages 0..t cover units 0..j; value (max, sum) and the
    // start of stage t-1.
    type Cell = Option<((u64, u64), usize)>;
    let mut best: Vec<Vec<Cell>> = vec![vec![None; n + 1]; s + 1];
    best[0][0] = Some(((0, 0), 0));
    for t in 0..s {
        let cap = caps[t];
        // Stage t ends at j; it must leave at least one unit per later stage.
        for j in (t + 1)..=(n - (s - t - 1)) {
            let mut cell: Cell = None;
            for (i, prev) in best[t].iter().enumerate().take(j).skip(t) {
                let Some(((pmax, psum), _)) = *prev else {
                    continue;
                };
                let need = p.weight[j] - p.weight[i] + p.kv[j] - p.kv[i];
                if need > cap.usable_bytes {
                    continue;
                }
                let time =
                    (p.active[j] - p.active[i]).saturating_mul(1000) / cap.bandwidth_mb_s.max(1);
                let score = (pmax.max(time), psum + time);
                if cell.is_none_or(|(best_score, _)| score < best_score) {
                    cell = Some((score, i));
                }
            }
            best[t + 1][j] = cell;
        }
    }
    best[s][n]?;
    let mut stages = Vec::with_capacity(s);
    let mut end = n;
    for t in (1..=s).rev() {
        let (_, start) = best[t][end]?;
        stages.push(p.stage(start..end, caps[t - 1].bandwidth_mb_s));
        end = start;
    }
    stages.reverse();
    Some(stages)
}

/// Sum of p50 RTTs around the ring, including the return hop from the last
/// stage to the first, in microseconds. A missing link costs `u64::MAX`.
pub fn ring_cost_us(order: &[usize], rtt: &dyn RttSource) -> u64 {
    if order.len() < 2 {
        return 0;
    }
    let mut total = 0u64;
    for (i, &a) in order.iter().enumerate() {
        let b = order[(i + 1) % order.len()];
        match rtt.link(a, b) {
            Some(l) => total += u64::from(l.p50_us),
            None => return u64::MAX,
        }
    }
    total
}

/// Orders `members` into the ring with the lowest [`ring_cost_us`], keeping
/// `members[0]` (the ingress) first. Exact for up to 8 members
/// (research-6 §6.4); above that, nearest neighbour then 2-opt.
pub fn ring_order(members: &[usize], rtt: &dyn RttSource) -> Vec<usize> {
    if members.len() <= 3 {
        return members.to_vec();
    }
    if members.len() <= 8 {
        let mut rest = members[1..].to_vec();
        let mut best = members.to_vec();
        let mut best_cost = ring_cost_us(&best, rtt);
        permute(&mut rest, 0, &mut |perm| {
            let mut order = Vec::with_capacity(members.len());
            order.push(members[0]);
            order.extend_from_slice(perm);
            let cost = ring_cost_us(&order, rtt);
            if cost < best_cost {
                best_cost = cost;
                best = order;
            }
        });
        return best;
    }
    let p50 = |a: usize, b: usize| rtt.link(a, b).map_or(u64::MAX, |l| u64::from(l.p50_us));
    let mut order = vec![members[0]];
    let mut left: Vec<usize> = members[1..].to_vec();
    while !left.is_empty() {
        let last = *order.last().expect("non-empty");
        let (k, _) = left
            .iter()
            .enumerate()
            .min_by_key(|&(_, &x)| (p50(last, x), x))
            .expect("non-empty");
        order.push(left.remove(k));
    }
    // 2-opt over positions 1..n, keeping the ingress fixed.
    let n = order.len();
    let mut improved = true;
    while improved {
        improved = false;
        for i in 1..n - 1 {
            for j in (i + 1)..n {
                let a = order[i - 1];
                let b = order[i];
                let c = order[j];
                let d = order[(j + 1) % n];
                let before = p50(a, b).saturating_add(p50(c, d));
                let after = p50(a, c).saturating_add(p50(b, d));
                if after < before {
                    order[i..=j].reverse();
                    improved = true;
                }
            }
        }
    }
    order
}

fn permute(items: &mut Vec<usize>, k: usize, visit: &mut dyn FnMut(&[usize])) {
    if k == items.len() {
        visit(items);
        return;
    }
    for i in k..items.len() {
        items.swap(k, i);
        permute(items, k + 1, visit);
        items.swap(k, i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{LinkStats, RttMatrix};

    const GB: u64 = 1_000_000_000;

    fn cap(gb: u64, bw: u64) -> StageCapacity {
        StageCapacity {
            usable_bytes: gb * GB,
            bandwidth_mb_s: bw,
        }
    }

    #[test]
    fn kimi_fits_two_512_gb_macs_and_not_one() {
        let m = ModelSpec::kimi_k26_int4();
        let kv = 8 * 4096;
        assert!(partition(&m, &[cap(410, 456_000)], kv).is_none());
        let stages = partition(&m, &[cap(410, 456_000), cap(410, 456_000)], kv).unwrap();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].layers.start, 0);
        assert_eq!(stages[1].layers.end, m.layers.len());
        // Balanced: the halves differ by at most one MoE layer.
        let diff = stages[0].time_ns.abs_diff(stages[1].time_ns);
        assert!(diff <= 1_000_000, "{stages:?}");
        for (s, c) in stages.iter().zip([410, 410]) {
            assert!(s.need_bytes() <= c * GB);
        }
    }

    #[test]
    fn faster_stages_take_more_layers() {
        let m = ModelSpec::uniform("u", 12, GB, 0);
        let stages = partition(&m, &[cap(100, 1000), cap(100, 3000)], 0).unwrap();
        assert_eq!(stages[0].layers, 0..3);
        assert_eq!(stages[1].layers, 3..12);
        assert_eq!(stages[0].time_ns, stages[1].time_ns);
    }

    #[test]
    fn memory_limits_override_speed() {
        let m = ModelSpec::uniform("u", 12, GB, 0);
        // The fast stage can hold only 4 units.
        let stages = partition(&m, &[cap(100, 1000), cap(4, 9000)], 0).unwrap();
        assert_eq!(stages[1].layers.len(), 4);
        // Every stage gets at least one unit; too many stages fails.
        assert!(partition(&m, &vec![cap(100, 1000); 13], 0).is_none());
    }

    #[test]
    fn kv_budget_counts_against_memory() {
        let m = ModelSpec::uniform("u", 4, GB, 1_000);
        assert!(partition(&m, &[cap(4, 1000)], 0).is_some());
        // 4 units × 1,000 B × 1M positions = 4 GB more.
        assert!(partition(&m, &[cap(4, 1000)], 1_000_000).is_none());
        assert!(partition(&m, &[cap(4, 1000), cap(4, 1000)], 1_000_000).is_some());
    }

    #[test]
    fn layer_granularity_is_respected() {
        // Total capacity suffices but no stage can hold a unit.
        let m = ModelSpec::uniform("u", 2, 10 * GB, 0);
        assert!(partition(&m, &[cap(9, 1000), cap(9, 1000), cap(9, 1000)], 0).is_none());
    }

    fn line_rtt(n: usize) -> RttMatrix {
        // Devices on a line: RTT = 1 ms × distance.
        let mut m = RttMatrix::new();
        for a in 0..n {
            for b in (a + 1)..n {
                let us = (b - a) as u32 * 1000;
                m.insert(
                    a,
                    b,
                    LinkStats {
                        p50_us: us,
                        p95_us: us,
                        p99_us: us,
                        loss_permille: 0,
                        samples: 10,
                    },
                );
            }
        }
        m
    }

    #[test]
    fn ring_order_is_exact_for_small_rings() {
        let rtt = line_rtt(6);
        let order = ring_order(&[0, 5, 2, 4, 1, 3], &rtt);
        assert_eq!(order[0], 0);
        // The optimal tour of points on a line costs 2 × span.
        assert_eq!(ring_cost_us(&order, &rtt), 10_000);
    }

    #[test]
    fn ring_order_heuristic_handles_large_rings() {
        let rtt = line_rtt(20);
        let members: Vec<usize> = (0..20).rev().collect();
        let order = ring_order(&members, &rtt);
        assert_eq!(order[0], 19);
        assert_eq!(order.len(), 20);
        assert_eq!(ring_cost_us(&order, &rtt), 38_000);
    }
}
