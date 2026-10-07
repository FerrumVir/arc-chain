//! Projected speed of a planned island \[CALC\]. Reporting only: formation
//! never reads these numbers.
//!
//! Per-answer speed (research-7 §2.1):
//! `t_pass = Σ compute_s(k+1) + Σ_ring (RTT/2 + o) + Σ_{s<S} serialisation_s(k+1)
//!           + k·t_draft + fixed`, and `tok/s = τ / t_pass` with
//! `τ = (1 − α^(k+1)) / (1 − α)` for a k-token chain draft. Compute is
//! memory-bound: bytes read (experts counted by expected distinct experts,
//! research-6 §2.6) over effective bandwidth. FLOPs, prefill and queueing are
//! not modelled.
//!
//! Batching (research-6 §2.6): B sequences in G = min(S, B) micro-batches of
//! b = ⌈B/G⌉; round time `T = max(G·max_s step_s(b), Σ_s step_s(b) + ring + fixed)`;
//! per-stream `1/T`, aggregate `B/T`. A second plan also searches the draft
//! depth under batching: each sequence then sends k+1 positions per round
//! and commits τ(k) tokens (research-7 §2.7 warns that speculation often
//! stops paying at high load; the search keeps k = 0 when it does not pay).
//!
//! The speculation exactness rule (research-6 §2.7, research-7 §2.2): a
//! draft token is accepted only if it equals what the seeded integer
//! sampler emits at that position, so speculation never changes the bytes.

use crate::device::{DeviceDescriptor, RttSource};
use crate::form::{IslandPlan, Parallelism, Tier};
use crate::model::ModelSpec;
use serde::{Deserialize, Serialize};

/// Network and drafting assumptions \[ASSUMPTION unless measured\].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NetAssumptions {
    /// Per-hop software overhead across homes (research-7: o = 1 ms).
    pub wan_hop_overhead_us: u32,
    /// Per-hop overhead on a LAN (research-6: 0.3 ms streaming transport).
    pub lan_hop_overhead_us: u32,
    /// Home uplink when not measured (research-7 pessimistic: 50 Mb/s).
    pub wan_uplink_mbps: u32,
    /// LAN link speed (10 GbE).
    pub lan_uplink_mbps: u32,
    /// Fixed overhead per forward pass (research-7: about 2 ms).
    pub fixed_pass_us: u32,
    /// One tensor-parallel collective on Thunderbolt-5 RDMA (research-6:
    /// 0.1–0.2 ms).
    pub collective_us: u32,
    /// Drafting cost per draft token (research-7: 1.5 ms, EAGLE-style head).
    pub draft_us_per_token: u32,
    /// Per-token draft acceptance α (research-7 planning value for chain
    /// drafts in chat at T≈0.7: 0.7).
    pub draft_acceptance: f64,
    pub max_draft_tokens: u32,
}

impl NetAssumptions {
    /// research-7's pessimistic WAN defaults.
    pub fn pessimistic() -> Self {
        Self {
            wan_hop_overhead_us: 1_000,
            lan_hop_overhead_us: 300,
            wan_uplink_mbps: 50,
            lan_uplink_mbps: 10_000,
            fixed_pass_us: 2_000,
            collective_us: 150,
            draft_us_per_token: 1_500,
            draft_acceptance: 0.7,
            max_draft_tokens: 5,
        }
    }

    /// An optimised transport: 0.3 ms per hop and 500 Mb/s fibre uplinks.
    pub fn optimized() -> Self {
        Self {
            wan_hop_overhead_us: 300,
            wan_uplink_mbps: 500,
            ..Self::pessimistic()
        }
    }
}

/// Tokens committed per pass for a k-token chain draft.
pub fn tau(draft_tokens: u32, acceptance: f64) -> f64 {
    if draft_tokens == 0 {
        return 1.0;
    }
    if (acceptance - 1.0).abs() < f64::EPSILON {
        return f64::from(draft_tokens) + 1.0;
    }
    (1.0 - acceptance.powi(draft_tokens as i32 + 1)) / (1.0 - acceptance)
}

/// One stream's projected speed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamProjection {
    pub draft_tokens: u32,
    pub tau: f64,
    pub pass_ms: f64,
    /// Of `pass_ms`: compute, network, drafting.
    pub compute_ms: f64,
    pub network_ms: f64,
    pub tok_s: f64,
}

/// The planned batching depth.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BatchPlan {
    pub concurrent: u32,
    pub micro_batches: u32,
    /// Draft tokens per sequence per pass (0 = no speculation).
    pub draft_tokens: u32,
    pub round_ms: f64,
    pub per_stream_tok_s: f64,
    pub aggregate_tok_s: f64,
}

/// Everything the report shows for one island.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    pub single: StreamProjection,
    /// The best draft depth (may be 0 when speculation does not pay).
    pub speculative: StreamProjection,
    /// Batching without speculation.
    pub batch: BatchPlan,
    /// Batching with the best draft depth per sequence (may equal `batch`).
    pub batch_speculative: BatchPlan,
}

struct Shape {
    tensor: bool,
    /// Bytes/s per stage.
    bw: Vec<f64>,
    /// Bits/s each stage sends at.
    uplink: Vec<f64>,
    /// Seconds per hop (RTT/2 + o), ring order, including the return hop.
    hops: Vec<f64>,
}

fn shape(
    plan: &IslandPlan,
    devices: &[DeviceDescriptor],
    rtt: &dyn RttSource,
    net: &NetAssumptions,
) -> Shape {
    let lan = matches!(plan.tier, Tier::T1aRdma | Tier::T1bLan);
    let pool = |m: usize| devices[m].pool();
    let bw = plan
        .members
        .iter()
        .map(|&m| pool(m).map_or(1.0, |p| p.bandwidth_mb_s as f64 * 1e6))
        .collect();
    let uplink = plan
        .members
        .iter()
        .map(|&m| {
            let mbps = if lan {
                net.lan_uplink_mbps
            } else {
                devices[m]
                    .measured
                    .uplink_mbps
                    .unwrap_or(net.wan_uplink_mbps)
            };
            f64::from(mbps) * 1e6
        })
        .collect();
    let o = if lan {
        net.lan_hop_overhead_us
    } else {
        net.wan_hop_overhead_us
    };
    let n = plan.members.len();
    let hops = if n < 2 {
        Vec::new()
    } else {
        (0..n)
            .map(|i| {
                let rtt_us = rtt
                    .link(plan.members[i], plan.members[(i + 1) % n])
                    .map_or(f64::INFINITY, |l| f64::from(l.p50_us));
                (rtt_us / 2.0 + f64::from(o)) * 1e-6
            })
            .collect()
    };
    Shape {
        tensor: plan.parallelism == Parallelism::Tensor,
        bw,
        uplink,
        hops,
    }
}

/// Seconds stage `s` reads for `positions` positions.
fn stage_compute(
    plan: &IslandPlan,
    model: &ModelSpec,
    sh: &Shape,
    s: usize,
    positions: u32,
) -> f64 {
    let bytes: f64 = plan.stages[s]
        .layers
        .clone()
        .map(|l| model.layers[l].active_bytes(positions))
        .sum();
    bytes / sh.bw[s]
}

/// Seconds for a tensor-parallel island: bytes over the summed bandwidth
/// plus the sequential collectives.
fn tensor_compute(model: &ModelSpec, sh: &Shape, net: &NetAssumptions, positions: u32) -> f64 {
    let bytes: f64 = model.layers.iter().map(|l| l.active_bytes(positions)).sum();
    bytes / sh.bw.iter().sum::<f64>()
        + f64::from(model.collectives_per_token) * f64::from(net.collective_us) * 1e-6
}

fn serialisation(model: &ModelSpec, sh: &Shape, s: usize, positions: u32) -> f64 {
    f64::from(positions) * model.boundary_bytes_per_position as f64 * 8.0 / sh.uplink[s]
}

/// Per-answer speed with a `k`-token draft.
pub fn single_stream(
    plan: &IslandPlan,
    devices: &[DeviceDescriptor],
    model: &ModelSpec,
    rtt: &dyn RttSource,
    net: &NetAssumptions,
    k: u32,
) -> StreamProjection {
    let sh = shape(plan, devices, rtt, net);
    let n = k + 1;
    let (compute, network) = if sh.tensor || plan.members.len() == 1 {
        let c = if sh.tensor {
            tensor_compute(model, &sh, net, n)
        } else {
            stage_compute(plan, model, &sh, 0, n)
        };
        (c, 0.0)
    } else {
        let s_count = plan.members.len();
        let compute = (0..s_count)
            .map(|s| stage_compute(plan, model, &sh, s, n))
            .sum();
        // The return hop carries token ids only: no serialisation.
        let ser: f64 = (0..s_count - 1)
            .map(|s| serialisation(model, &sh, s, n))
            .sum();
        (compute, sh.hops.iter().sum::<f64>() + ser)
    };
    let draft = f64::from(k) * f64::from(net.draft_us_per_token) * 1e-6;
    let pass = compute + network + draft + f64::from(net.fixed_pass_us) * 1e-6;
    let t = tau(k, net.draft_acceptance);
    StreamProjection {
        draft_tokens: k,
        tau: t,
        pass_ms: pass * 1e3,
        compute_ms: compute * 1e3,
        network_ms: network * 1e3,
        tok_s: t / pass,
    }
}

/// The draft depth in `0..=max_draft_tokens` with the highest per-answer
/// speed (ties go to the shallower draft).
pub fn best_speculation(
    plan: &IslandPlan,
    devices: &[DeviceDescriptor],
    model: &ModelSpec,
    rtt: &dyn RttSource,
    net: &NetAssumptions,
) -> StreamProjection {
    let mut best = single_stream(plan, devices, model, rtt, net, 0);
    for k in 1..=net.max_draft_tokens {
        let p = single_stream(plan, devices, model, rtt, net, k);
        if p.tok_s > best.tok_s {
            best = p;
        }
    }
    best
}

/// Round time for `concurrent` sequences.
fn batch_round(
    plan: &IslandPlan,
    model: &ModelSpec,
    sh: &Shape,
    net: &NetAssumptions,
    concurrent: u32,
    k: u32,
) -> (u32, f64) {
    let fixed = f64::from(net.fixed_pass_us) * 1e-6;
    let draft = f64::from(k) * f64::from(net.draft_us_per_token) * 1e-6;
    let positions = |seqs: u32| seqs * (k + 1);
    if sh.tensor {
        return (
            1,
            tensor_compute(model, sh, net, positions(concurrent)) + draft + fixed,
        );
    }
    let s_count = plan.members.len();
    if s_count == 1 {
        return (
            1,
            stage_compute(plan, model, sh, 0, positions(concurrent)) + draft + fixed,
        );
    }
    let g = (s_count as u32).min(concurrent);
    let b = concurrent.div_ceil(g);
    let steps: Vec<f64> = (0..s_count)
        .map(|s| {
            let ser = if s + 1 < s_count {
                serialisation(model, sh, s, positions(b))
            } else {
                0.0
            };
            stage_compute(plan, model, sh, s, positions(b)) + ser
        })
        .collect();
    let slowest = steps.iter().copied().fold(0.0, f64::max);
    let circuit = steps.iter().sum::<f64>() + sh.hops.iter().sum::<f64>() + draft + fixed;
    (g, (f64::from(g) * slowest).max(circuit))
}

/// Picks the batching depth (and, when `max_draft_tokens > 0`, the draft
/// depth): the largest aggregate over B ∈ {1, 2, 4, …} up to the island's KV
/// budget (`kv_positions / context_positions` sequences) and k ∈
/// 0..=`max_draft_tokens`, keeping per-stream speed at least
/// `min(floor_tok_s, single-stream / 2)`. Each sequence commits τ(k) tokens
/// per round.
#[allow(clippy::too_many_arguments)]
pub fn plan_batching(
    plan: &IslandPlan,
    devices: &[DeviceDescriptor],
    model: &ModelSpec,
    rtt: &dyn RttSource,
    net: &NetAssumptions,
    context_positions: u64,
    floor_tok_s: f64,
    max_draft_tokens: u32,
) -> BatchPlan {
    let sh = shape(plan, devices, rtt, net);
    let max_b = (plan.kv_positions / context_positions.max(1)).clamp(1, u64::from(u32::MAX)) as u32;
    let at = |b: u32, k: u32| {
        let (g, round) = batch_round(plan, model, &sh, net, b, k);
        let t = tau(k, net.draft_acceptance);
        BatchPlan {
            concurrent: b,
            micro_batches: g,
            draft_tokens: k,
            round_ms: round * 1e3,
            per_stream_tok_s: t / round,
            aggregate_tok_s: f64::from(b) * t / round,
        }
    };
    let one = at(1, 0);
    let floor = floor_tok_s.min(one.per_stream_tok_s / 2.0);
    let mut best = one;
    let mut b = 1u32;
    while b <= max_b {
        for k in 0..=max_draft_tokens {
            let p = at(b, k);
            if p.per_stream_tok_s >= floor && p.aggregate_tok_s > best.aggregate_tok_s {
                best = p;
            }
        }
        let Some(next) = b.checked_mul(2) else { break };
        b = next;
    }
    best
}

/// Single stream, best speculation and batching for one island.
#[allow(clippy::too_many_arguments)]
pub fn project(
    plan: &IslandPlan,
    devices: &[DeviceDescriptor],
    model: &ModelSpec,
    rtt: &dyn RttSource,
    net: &NetAssumptions,
    context_positions: u64,
    floor_tok_s: f64,
) -> Projection {
    Projection {
        single: single_stream(plan, devices, model, rtt, net, 0),
        speculative: best_speculation(plan, devices, model, rtt, net),
        batch: plan_batching(
            plan,
            devices,
            model,
            rtt,
            net,
            context_positions,
            floor_tok_s,
            0,
        ),
        batch_speculative: plan_batching(
            plan,
            devices,
            model,
            rtt,
            net,
            context_positions,
            floor_tok_s,
            net.max_draft_tokens,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::tests::{device, mesh, with_rdma};
    use crate::form::{FormationPolicy, form};

    const KV: u64 = 8 * 4096;

    #[test]
    fn tau_matches_research_7() {
        assert!((tau(1, 0.85) - 1.85).abs() < 1e-9);
        assert!((tau(3, 0.7) - 2.533).abs() < 1e-3);
        assert!((tau(5, 0.8) - 3.689).abs() < 1e-3);
        assert!((tau(64, 1.0) - 65.0).abs() < 1e-9);
        assert!((tau(64, 0.9) - (0..=64).map(|i| 0.9f64.powi(i)).sum::<f64>()).abs() < 1e-9);
    }

    /// Two 512 GB Macs serving plus a third as the required spare. `rdma`
    /// adds measured RDMA evidence (T1a, tensor parallel).
    fn two_ultras(
        rtt_us: u32,
        rdma: bool,
        site: Option<&str>,
    ) -> (Vec<DeviceDescriptor>, crate::device::RttMatrix, IslandPlan) {
        let devices: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|id| {
                let d = device(id, 512, rdma, site, "NYC");
                if rdma { with_rdma(d) } else { d }
            })
            .collect();
        let rtt = mesh(3, rtt_us, &[]);
        let out = form(
            &devices,
            &rtt,
            &ModelSpec::kimi_k26_int4(),
            &FormationPolicy::interactive(KV),
            0,
        );
        let plan = out.islands[0].clone();
        assert_eq!(plan.members.len(), 2);
        (devices, rtt, plan)
    }

    #[test]
    fn calibrates_against_research_6_section_2_5() {
        let model = ModelSpec::kimi_k26_int4();
        let net = NetAssumptions::pessimistic();
        // research-6 §2.5: 2× M3 Ultra TB5 TP2 ≈ 43 ms (23.4 tok/s).
        let (d, rtt, plan) = two_ultras(40, true, Some("home"));
        assert_eq!(plan.parallelism, Parallelism::Tensor);
        let tp = single_stream(&plan, &d, &model, &rtt, &net, 0);
        assert!((20.0..27.0).contains(&tp.tok_s), "{tp:?}");
        // research-6 §2.5: 2× M3 Ultra in two homes, RTT 15 ms ≈ 64 ms
        // (15.5 tok/s). This model adds the 56 KiB Q16 boundary at 50 Mb/s.
        let (d, rtt, plan) = two_ultras(15_000, false, None);
        assert_eq!(plan.tier, Tier::T2Metro);
        let pp = single_stream(&plan, &d, &model, &rtt, &net, 0);
        assert!((10.0..16.0).contains(&pp.tok_s), "{pp:?}");
        assert!(pp.network_ms > 15.0);
    }

    #[test]
    fn speculation_pays_where_hops_dominate() {
        let model = ModelSpec::kimi_k26_int4();
        let net = NetAssumptions::optimized();
        let (d, rtt, plan) = two_ultras(15_000, false, None);
        let base = single_stream(&plan, &d, &model, &rtt, &net, 0);
        let best = best_speculation(&plan, &d, &model, &rtt, &net);
        assert!(best.draft_tokens > 0);
        assert!(best.tok_s > base.tok_s * 1.1, "{base:?} {best:?}");
    }

    #[test]
    fn batching_raises_aggregate_within_the_floor() {
        let model = ModelSpec::kimi_k26_int4();
        let net = NetAssumptions::pessimistic();
        let (d, rtt, plan) = two_ultras(15_000, false, None);
        let one = single_stream(&plan, &d, &model, &rtt, &net, 0);
        let b = plan_batching(&plan, &d, &model, &rtt, &net, 4096, 5.0, 0);
        assert_eq!(b.draft_tokens, 0);
        let spec = plan_batching(
            &plan,
            &d,
            &model,
            &rtt,
            &net,
            4096,
            5.0,
            net.max_draft_tokens,
        );
        assert!(spec.aggregate_tok_s >= b.aggregate_tok_s);
        assert!(spec.per_stream_tok_s >= 5.0_f64.min(one.tok_s / 2.0));
        assert!(b.concurrent > 1 && b.concurrent <= 8, "{b:?}");
        assert!(b.aggregate_tok_s > one.tok_s);
        assert!(b.per_stream_tok_s >= 5.0_f64.min(one.tok_s / 2.0));
    }
}
