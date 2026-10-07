//! Placement optimizer: which nodes run which contiguous layer slice (and,
//! inside a LAN island, which expert slice) so one token's trip is as short
//! as possible.
//!
//! Follows research-7's regional-clustering model:
//!
//! 1. **Regional cells.** Complete-linkage agglomerative clustering on the
//!    measured RTT matrix, cut at `cluster_diameter_ms` (research-7 §3.5:
//!    complete linkage guarantees every pair in a cell is within the cut;
//!    §6.2 diameters are 20 / 35 / 60 ms for metro / zone / region). A
//!    forward pass never leaves its cell (§6.1 rule 3). Only if no cell can
//!    hold the model is the whole node set searched, and the result is
//!    flagged `cross_cluster`.
//! 2. **Units.** Every node is a candidate stage. For MoE models, nodes
//!    within `island_rtt_ms` of each other (a LAN or Thunderbolt island,
//!    research-7 L5) also form a candidate expert-parallel stage: each member
//!    holds the dense part of the stage's layers and a contiguous slice of
//!    the routed experts, in proportion to its memory.
//! 3. **Fewest, fastest stages.** All disjoint unit subsets up to
//!    `max_stages` are scored (after pruning to the `candidates` best units
//!    per cell). Each extra stage costs a hop, `r/2 + o + bytes·8/uplink`,
//!    so the search prefers fewer, faster, larger-memory stages by
//!    construction.
//! 4. **Regional ordering.** The ring order minimizing `Σ r/2` is exact
//!    (Held-Karp, `S ≤ 8`), then every rotation and direction is scored,
//!    because which unit sends activations (its uplink) and which holds the
//!    embedding or LM head (its memory) depend on position.
//! 5. **Layer slices.** For per-answer latency the per-token compute is a
//!    sum, so layers go to the fastest units first, up to memory. For
//!    aggregate throughput the busiest stage bounds the ring, so layers are
//!    water-filled to equalize stage time.
//!
//! The cost of a plan is [`super::cost`]'s research-7 model. Inputs are
//! measurements (RTT p50 or p95, per-layer decode time, uplink); the output
//! is a prediction and is labeled as such wherever it is reported.

use super::cost::{self, HopCost};
use std::ops::Range;

#[derive(Clone, Debug)]
pub struct NodeSpec {
    pub id: String,
    /// Display label only; membership comes from measured RTT.
    pub region: String,
    /// Memory available for weights.
    pub mem_bytes: u64,
    /// Measured decode time per layer per token at batch 1, ms.
    pub ms_per_layer: f64,
    pub uplink_mbps: f64,
}

#[derive(Clone, Debug)]
pub struct ModelShape {
    pub n_layers: u32,
    /// Bytes of one transformer layer's weights.
    pub layer_bytes: u64,
    /// Extra bytes on the first stage (embedding).
    pub first_extra_bytes: u64,
    /// Extra bytes on the last stage (final norm, LM head).
    pub last_extra_bytes: u64,
    /// Bytes per token on an activation hop, after the exact codec.
    pub boundary_bytes: f64,
    /// Routed experts per layer; 0 for a dense model.
    pub experts_per_layer: u32,
    /// Share of `layer_bytes` held in routed experts.
    pub routed_bytes_fraction: f64,
    /// Share of a layer's decode time spent on routed experts.
    pub routed_time_fraction: f64,
}

impl ModelShape {
    pub fn total_bytes(&self) -> Option<u64> {
        self.layer_bytes
            .checked_mul(self.n_layers as u64)?
            .checked_add(self.first_extra_bytes)?
            .checked_add(self.last_extra_bytes)
    }

    /// Conservative whole-byte footprint: replicate the dense bytes and round
    /// each equal-sized expert up. Fraction multiplication uses the exact f64
    /// binary value, avoiding loss of integer precision above 2^53 bytes.
    fn member_layer_bytes(&self, experts: u32, split: bool) -> Option<u64> {
        if !split {
            return Some(self.layer_bytes);
        }
        let f = self.routed_bytes_fraction;
        let bits = f.to_bits();
        let exponent = ((bits >> 52) & 0x7ff) as u32;
        let mantissa = (bits & ((1u64 << 52) - 1)) | if exponent == 0 { 0 } else { 1u64 << 52 };
        let shift = if exponent == 0 { 1074 } else { 1075 - exponent };
        let product = self.layer_bytes as u128 * mantissa as u128;
        let routed = if shift >= 128 {
            0
        } else {
            (product >> shift) as u64
        };
        let dense = self.layer_bytes.checked_sub(routed)?;
        let expert = routed.div_ceil(self.experts_per_layer as u64);
        dense.checked_add(expert.checked_mul(experts as u64)?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Objective {
    /// Minimize one answer's time per token.
    PerAnswer,
    /// Maximize tokens per second with this many sequences in flight.
    Aggregate { in_flight: u32 },
}

#[derive(Clone, Debug)]
pub struct PlacementParams {
    pub max_stages: usize,
    pub hop_overhead_ms: f64,
    pub cluster_diameter_ms: f64,
    pub island_rtt_ms: f64,
    /// Expert dispatch + combine inside an island, per MoE layer, ms.
    pub island_exchange_ms_per_layer: f64,
    pub max_island_size: usize,
    /// Units kept per cell before subset search.
    pub candidates: usize,
    pub objective: Objective,
}

impl Default for PlacementParams {
    fn default() -> Self {
        Self {
            max_stages: 8,
            hop_overhead_ms: 1.0,
            cluster_diameter_ms: 60.0,
            island_rtt_ms: 1.0,
            island_exchange_ms_per_layer: 0.05,
            max_island_size: 4,
            candidates: 12,
            objective: Objective::PerAnswer,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemberSlice {
    pub node: usize,
    pub experts: Range<u32>,
}

#[derive(Clone, Debug)]
pub struct StagePlan {
    pub members: Vec<MemberSlice>,
    pub layers: Range<u32>,
    pub compute_ms: f64,
    /// The hop leaving this stage (absent for a single-stage plan).
    pub hop: Option<HopCost>,
}

#[derive(Clone, Debug)]
pub struct Placement {
    /// Nodes of the cell the plan was drawn from.
    pub cell: Vec<usize>,
    pub cross_cluster: bool,
    pub stages: Vec<StagePlan>,
    pub compute_ms: f64,
    pub network_ms: f64,
    pub pass_ms: f64,
    pub per_answer_tok_s: f64,
    pub aggregate_tok_s: f64,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PlacementError {
    #[error("RTT matrix is {got}×? but there are {nodes} nodes")]
    BadMatrix { got: usize, nodes: usize },
    #[error("invalid placement input: {0}")]
    InvalidInput(&'static str),
    #[error("memory arithmetic overflow")]
    MemoryOverflow,
    #[error("no set of at most {max_stages} stages can hold the model")]
    Infeasible { max_stages: usize },
}

/// Symmetric RTT lookup (the larger of the two directions; NaN = unreachable).
fn rtt(m: &[Vec<f64>], a: usize, b: usize) -> f64 {
    if a == b {
        return 0.0;
    }
    let (x, y) = (m[a][b], m[b][a]);
    if !x.is_finite() || !y.is_finite() {
        f64::INFINITY
    } else {
        x.max(y)
    }
}

/// Complete-linkage agglomerative clustering of `members`, cut at `cut` ms:
/// every pair inside a returned cluster is within `cut`.
pub fn complete_linkage(m: &[Vec<f64>], members: &[usize], cut: f64) -> Vec<Vec<usize>> {
    let mut clusters: Vec<Vec<usize>> = members.iter().map(|&i| vec![i]).collect();
    let n = clusters.len();
    let mut d = vec![vec![f64::INFINITY; n]; n];
    for i in 0..n {
        for j in 0..n {
            if i != j {
                d[i][j] = rtt(m, clusters[i][0], clusters[j][0]);
            }
        }
    }
    let mut alive = vec![true; n];
    loop {
        let mut best = (f64::INFINITY, 0, 0);
        for i in 0..n {
            if !alive[i] {
                continue;
            }
            for j in i + 1..n {
                if alive[j] && d[i][j] < best.0 {
                    best = (d[i][j], i, j);
                }
            }
        }
        let (dist, i, j) = best;
        if dist > cut || !dist.is_finite() {
            break;
        }
        let moved = std::mem::take(&mut clusters[j]);
        clusters[i].extend(moved);
        alive[j] = false;
        for k in 0..n {
            if alive[k] && k != i {
                let v = d[i][k].max(d[j][k]);
                d[i][k] = v;
                d[k][i] = v;
            }
        }
    }
    let mut out: Vec<Vec<usize>> = clusters
        .into_iter()
        .zip(alive)
        .filter(|(_, a)| *a)
        .map(|(mut c, _)| {
            c.sort_unstable();
            c
        })
        .collect();
    out.sort();
    out
}

#[derive(Clone, Debug)]
struct Unit {
    members: Vec<usize>,
    experts: Vec<Range<u32>>,
    ms_per_layer: f64,
    uplink_mbps: f64,
}

impl Unit {
    fn lead(&self) -> usize {
        self.members[0]
    }

    /// Layers this unit can hold with `extra` bytes on its lead.
    fn capacity(&self, nodes: &[NodeSpec], model: &ModelShape, extra: u64) -> u32 {
        let mut cap = model.n_layers;
        for (k, (&node, experts)) in self.members.iter().zip(&self.experts).enumerate() {
            let Some(mem) = nodes[node]
                .mem_bytes
                .checked_sub(if k == 0 { extra } else { 0 })
            else {
                return 0;
            };
            let Some(per_layer) =
                model.member_layer_bytes(experts.end - experts.start, self.members.len() > 1)
            else {
                return 0;
            };
            if let Some(layers) = mem.checked_div(per_layer) {
                cap = cap.min(layers.min(model.n_layers as u64) as u32);
            }
        }
        cap.min(model.n_layers)
    }
}

fn make_units(
    nodes: &[NodeSpec],
    m: &[Vec<f64>],
    cell: &[usize],
    model: &ModelShape,
    p: &PlacementParams,
) -> Vec<Unit> {
    let mut units: Vec<Unit> = cell
        .iter()
        .map(|&i| Unit {
            members: vec![i],
            experts: std::iter::once(0..model.experts_per_layer).collect(),
            ms_per_layer: nodes[i].ms_per_layer,
            uplink_mbps: nodes[i].uplink_mbps,
        })
        .collect();
    if model.experts_per_layer > 1 && p.max_island_size > 1 {
        for mut island in complete_linkage(m, cell, p.island_rtt_ms) {
            if island.len() < 2 {
                continue;
            }
            // Largest memory first; the lead holds the embedding/LM head.
            island.sort_by(|&a, &b| nodes[b].mem_bytes.cmp(&nodes[a].mem_bytes).then(a.cmp(&b)));
            island.truncate(p.max_island_size.min(model.experts_per_layer as usize));
            // Assign whole experts first, using integer largest-remainder memory shares.
            // Reserve one expert per member, then apportion the remainder.
            let total: u128 = island.iter().map(|&i| nodes[i].mem_bytes as u128).sum();
            if total == 0 {
                continue;
            }
            let remaining = model.experts_per_layer - island.len() as u32;
            let weights: Vec<u128> = island
                .iter()
                .map(|&i| nodes[i].mem_bytes as u128 * remaining as u128)
                .collect();
            let mut counts: Vec<u32> = weights.iter().map(|w| 1 + (w / total) as u32).collect();
            let left = model.experts_per_layer - counts.iter().sum::<u32>();
            let mut remainders: Vec<usize> = (0..island.len()).collect();
            remainders.sort_by_key(|&k| (std::cmp::Reverse(weights[k] % total), k));
            for &k in remainders.iter().take(left as usize) {
                counts[k] += 1;
            }
            let mut at = 0;
            let experts: Vec<Range<u32>> = counts
                .into_iter()
                .map(|count| {
                    let range = at..at + count;
                    at += count;
                    range
                })
                .collect();
            let ft = model.routed_time_fraction;
            let ms = island
                .iter()
                .zip(&experts)
                .map(|(&i, e)| {
                    nodes[i].ms_per_layer
                        * ((1.0 - ft)
                            + ft * (e.end - e.start) as f64 / model.experts_per_layer as f64)
                })
                .fold(0.0f64, f64::max)
                + p.island_exchange_ms_per_layer;
            units.push(Unit {
                uplink_mbps: nodes[island[0]].uplink_mbps,
                members: island,
                experts,
                ms_per_layer: ms,
            });
        }
    }
    units
}

/// Keep the `k` most useful units: the fastest half, the largest half, then
/// fill by speed.
fn prune(units: Vec<Unit>, nodes: &[NodeSpec], model: &ModelShape, k: usize) -> Vec<Unit> {
    if units.len() <= k {
        return units;
    }
    let mut by_speed: Vec<usize> = (0..units.len()).collect();
    by_speed.sort_by(|&a, &b| {
        units[a]
            .ms_per_layer
            .total_cmp(&units[b].ms_per_layer)
            .then(a.cmp(&b))
    });
    let mut by_cap: Vec<usize> = (0..units.len()).collect();
    by_cap.sort_by(|&a, &b| {
        units[b]
            .capacity(nodes, model, 0)
            .cmp(&units[a].capacity(nodes, model, 0))
            .then(a.cmp(&b))
    });
    let mut keep: Vec<usize> = Vec::with_capacity(k);
    for &i in by_speed
        .iter()
        .take(k / 2)
        .chain(by_cap.iter().take(k - k / 2))
    {
        if !keep.contains(&i) {
            keep.push(i);
        }
    }
    for &i in &by_speed {
        if keep.len() >= k {
            break;
        }
        if !keep.contains(&i) {
            keep.push(i);
        }
    }
    keep.sort_unstable();
    keep.into_iter().map(|i| units[i].clone()).collect()
}

/// Exact minimum-cost Hamiltonian cycle over `ids` (Held-Karp), edge cost
/// `w(a, b)`. Returns the order starting at `ids[0]`.
fn best_cycle(ids: &[usize], w: &dyn Fn(usize, usize) -> f64) -> Vec<usize> {
    let n = ids.len();
    if n <= 2 {
        return ids.to_vec();
    }
    let full = 1usize << n;
    let mut dp = vec![vec![f64::INFINITY; n]; full];
    let mut parent = vec![vec![usize::MAX; n]; full];
    dp[1][0] = 0.0;
    for mask in 1..full {
        if mask & 1 == 0 {
            continue;
        }
        for last in 0..n {
            let cur = dp[mask][last];
            if !cur.is_finite() || mask & (1 << last) == 0 {
                continue;
            }
            for next in 1..n {
                if mask & (1 << next) != 0 {
                    continue;
                }
                let nm = mask | (1 << next);
                let c = cur + w(ids[last], ids[next]);
                if c < dp[nm][next] {
                    dp[nm][next] = c;
                    parent[nm][next] = last;
                }
            }
        }
    }
    let mut best = (f64::INFINITY, 1);
    for last in 1..n {
        let c = dp[full - 1][last] + w(ids[last], ids[0]);
        if c < best.0 {
            best = (c, last);
        }
    }
    let mut order = Vec::with_capacity(n);
    let (mut mask, mut cur) = (full - 1, best.1);
    while cur != usize::MAX && cur != 0 {
        order.push(ids[cur]);
        let p = parent[mask][cur];
        mask &= !(1 << cur);
        cur = p;
    }
    order.push(ids[0]);
    order.reverse();
    order
}

struct Scored {
    score: (f64, f64),
    order: Vec<usize>,
    layers: Vec<u32>,
}

/// Layer counts per ordered unit, or `None` if they cannot hold the model.
fn allocate(
    units: &[&Unit],
    caps: &[u32],
    hops: &[HopCost],
    model: &ModelShape,
    obj: Objective,
) -> Option<Vec<u32>> {
    let s = units.len();
    let l = model.n_layers;
    if caps.contains(&0) || caps.iter().map(|&c| c as u64).sum::<u64>() < l as u64 || (s as u32) > l
    {
        return None;
    }
    let mut layers = vec![1u32; s];
    match obj {
        Objective::PerAnswer => {
            let mut left = l - s as u32;
            let mut by_speed: Vec<usize> = (0..s).collect();
            by_speed.sort_by(|&a, &b| {
                units[a]
                    .ms_per_layer
                    .total_cmp(&units[b].ms_per_layer)
                    .then(a.cmp(&b))
            });
            for i in by_speed {
                let add = left.min(caps[i] - 1);
                layers[i] += add;
                left -= add;
            }
            (left == 0).then_some(layers)
        }
        Objective::Aggregate { .. } => {
            let ser = |i: usize| hops.get(i).map_or(0.0, HopCost::serialization_ms);
            let count = |t: f64| -> (Vec<u32>, u64) {
                let v: Vec<u32> = (0..s)
                    .map(|i| {
                        let k = ((t - ser(i)) / units[i].ms_per_layer.max(1e-9)).floor();
                        (k.max(1.0) as u64).min(caps[i] as u64) as u32
                    })
                    .collect();
                let sum = v.iter().map(|&x| x as u64).sum();
                (v, sum)
            };
            let mut hi = (0..s)
                .map(|i| caps[i] as f64 * units[i].ms_per_layer + ser(i))
                .fold(0.0, f64::max);
            let mut lo = 0.0;
            for _ in 0..80 {
                let mid = (lo + hi) / 2.0;
                if count(mid).1 >= l as u64 {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            let (mut v, mut sum) = count(hi);
            while sum > l as u64 {
                // Trim from the stage that is busiest.
                let i = (0..s).filter(|&i| v[i] > 1).max_by(|&a, &b| {
                    (v[a] as f64 * units[a].ms_per_layer + ser(a))
                        .total_cmp(&(v[b] as f64 * units[b].ms_per_layer + ser(b)))
                })?;
                v[i] -= 1;
                sum -= 1;
            }
            (sum == l as u64).then_some(v)
        }
    }
}

fn hops_for(
    order: &[&Unit],
    m: &[Vec<f64>],
    model: &ModelShape,
    p: &PlacementParams,
) -> Vec<HopCost> {
    let s = order.len();
    if s < 2 {
        return Vec::new();
    }
    (0..s)
        .map(|i| HopCost {
            rtt_ms: rtt(m, order[i].lead(), order[(i + 1) % s].lead()),
            overhead_ms: p.hop_overhead_ms,
            uplink_mbps: Some(order[i].uplink_mbps),
            // The last hop returns a token id.
            bytes: if i + 1 == s {
                8.0
            } else {
                model.boundary_bytes
            },
        })
        .collect()
}

fn score_order(
    order_idx: &[usize],
    units: &[Unit],
    nodes: &[NodeSpec],
    m: &[Vec<f64>],
    model: &ModelShape,
    p: &PlacementParams,
) -> Option<Scored> {
    let order: Vec<&Unit> = order_idx.iter().map(|&i| &units[i]).collect();
    let s = order.len();
    let caps: Vec<u32> = (0..s)
        .map(|i| {
            let mut extra = 0;
            if i == 0 {
                extra += model.first_extra_bytes;
            }
            if i + 1 == s {
                extra += model.last_extra_bytes;
            }
            order[i].capacity(nodes, model, extra)
        })
        .collect();
    let hops = hops_for(&order, m, model, p);
    if hops.iter().any(|h| !h.rtt_ms.is_finite()) {
        return None;
    }
    let layers = allocate(&order, &caps, &hops, model, p.objective)?;
    let compute: Vec<f64> = (0..s)
        .map(|i| layers[i] as f64 * order[i].ms_per_layer)
        .collect();
    let pass = cost::pass_ms(&compute, &hops);
    if !pass.is_finite() || pass <= 0.0 || !(1000.0 / pass).is_finite() {
        return None;
    }
    let score = match p.objective {
        Objective::PerAnswer => (pass, 0.0),
        Objective::Aggregate { in_flight } => {
            (-cost::aggregate_tok_s(&compute, &hops, in_flight), pass)
        }
    };
    Some(Scored {
        score,
        order: order_idx.to_vec(),
        layers,
    })
}

/// Lexicographic "strictly better" on (primary, tie-break) scores.
fn score_lt(a: (f64, f64), b: (f64, f64)) -> bool {
    a.0 < b.0 - 1e-9 || ((a.0 - b.0).abs() <= 1e-9 && a.1 < b.1 - 1e-9)
}

fn better(a: &Scored, b: &Option<Scored>) -> bool {
    b.as_ref().is_none_or(|b| score_lt(a.score, b.score))
}

fn search_cell(
    nodes: &[NodeSpec],
    m: &[Vec<f64>],
    cell: &[usize],
    model: &ModelShape,
    p: &PlacementParams,
) -> Option<(Vec<Unit>, Scored)> {
    let units = prune(
        make_units(nodes, m, cell, model, p),
        nodes,
        model,
        p.candidates.max(1),
    );
    let caps: Vec<u32> = units.iter().map(|u| u.capacity(nodes, model, 0)).collect();
    let max_s = p.max_stages.clamp(1, 8);
    let mut best: Option<Scored> = None;
    let mut chosen: Vec<usize> = Vec::new();
    let mut used = vec![false; nodes.len()];

    #[allow(clippy::too_many_arguments)]
    fn rec(
        start: usize,
        units: &[Unit],
        caps: &[u32],
        chosen: &mut Vec<usize>,
        used: &mut [bool],
        cap_sum: u64,
        max_s: usize,
        nodes: &[NodeSpec],
        m: &[Vec<f64>],
        model: &ModelShape,
        p: &PlacementParams,
        best: &mut Option<Scored>,
    ) {
        if !chosen.is_empty() && cap_sum >= model.n_layers as u64 {
            let w = |a: usize, b: usize| rtt(m, units[a].lead(), units[b].lead()) / 2.0;
            let cycle = best_cycle(chosen, &w);
            let s = cycle.len();
            for dir in 0..2 {
                for rot in 0..s {
                    let mut order: Vec<usize> = (0..s).map(|k| cycle[(rot + k) % s]).collect();
                    if dir == 1 {
                        order.reverse();
                    }
                    if let Some(sc) = score_order(&order, units, nodes, m, model, p)
                        && better(&sc, best)
                    {
                        *best = Some(sc);
                    }
                    if s == 1 {
                        break;
                    }
                }
                if s <= 2 {
                    // Reversing a 1- or 2-cycle gives the same rotations.
                    break;
                }
            }
        }
        if chosen.len() == max_s {
            return;
        }
        for i in start..units.len() {
            if units[i].members.iter().any(|&n| used[n]) {
                continue;
            }
            for &n in &units[i].members {
                used[n] = true;
            }
            chosen.push(i);
            rec(
                i + 1,
                units,
                caps,
                chosen,
                used,
                cap_sum + caps[i] as u64,
                max_s,
                nodes,
                m,
                model,
                p,
                best,
            );
            chosen.pop();
            for &n in &units[i].members {
                used[n] = false;
            }
        }
    }

    rec(
        0,
        &units,
        &caps,
        &mut chosen,
        &mut used,
        0,
        max_s,
        nodes,
        m,
        model,
        p,
        &mut best,
    );
    best.map(|b| (units, b))
}

fn build(
    units: &[Unit],
    sc: &Scored,
    m: &[Vec<f64>],
    cell: Vec<usize>,
    cross: bool,
    model: &ModelShape,
    p: &PlacementParams,
) -> Placement {
    let order: Vec<&Unit> = sc.order.iter().map(|&i| &units[i]).collect();
    let hops = hops_for(&order, m, model, p);
    let mut at = 0u32;
    let mut stages = Vec::with_capacity(order.len());
    let mut compute = Vec::with_capacity(order.len());
    for (i, u) in order.iter().enumerate() {
        let members = u
            .members
            .iter()
            .zip(&u.experts)
            .map(|(&node, experts)| MemberSlice {
                node,
                experts: experts.clone(),
            })
            .collect();
        let c = sc.layers[i] as f64 * u.ms_per_layer;
        compute.push(c);
        stages.push(StagePlan {
            members,
            layers: at..at + sc.layers[i],
            compute_ms: c,
            hop: hops.get(i).copied(),
        });
        at += sc.layers[i];
    }
    let in_flight = match p.objective {
        Objective::Aggregate { in_flight } => in_flight,
        Objective::PerAnswer => 1,
    };
    let pass = cost::pass_ms(&compute, &hops);
    Placement {
        cell,
        cross_cluster: cross,
        compute_ms: compute.iter().sum(),
        // `+ 0.0` turns the empty sum's -0.0 into 0.0 for display.
        network_ms: hops.iter().map(HopCost::ms).sum::<f64>() + 0.0,
        pass_ms: pass,
        per_answer_tok_s: 1000.0 / pass,
        aggregate_tok_s: cost::aggregate_tok_s(&compute, &hops, in_flight),
        stages,
    }
}

fn validate_inputs(
    nodes: &[NodeSpec],
    m: &[Vec<f64>],
    model: &ModelShape,
    p: &PlacementParams,
) -> Result<(), PlacementError> {
    let nonnegative = |v: f64| v.is_finite() && v >= 0.0;
    let positive = |v: f64| v.is_finite() && v > 0.0;
    if nodes
        .iter()
        .any(|n| !positive(n.ms_per_layer) || !positive(n.uplink_mbps))
    {
        return Err(PlacementError::InvalidInput(
            "compute and uplink must be finite and positive",
        ));
    }
    // NaN and positive infinity mean unknown; negative RTT is never physical.
    if m.iter().flatten().any(|&v| v < 0.0) {
        return Err(PlacementError::InvalidInput("negative RTT"));
    }
    if model.n_layers == 0
        || model.layer_bytes == 0
        || !nonnegative(model.boundary_bytes)
        || ![model.routed_bytes_fraction, model.routed_time_fraction]
            .iter()
            .all(|&f| f.is_finite() && (0.0..=1.0).contains(&f))
        || (model.experts_per_layer == 0
            && (model.routed_bytes_fraction != 0.0 || model.routed_time_fraction != 0.0))
    {
        return Err(PlacementError::InvalidInput(
            "model shape or routed fractions",
        ));
    }
    model.total_bytes().ok_or(PlacementError::MemoryOverflow)?;
    if ![
        p.hop_overhead_ms,
        p.cluster_diameter_ms,
        p.island_rtt_ms,
        p.island_exchange_ms_per_layer,
    ]
    .iter()
    .all(|&v| nonnegative(v))
        || !(1..=8).contains(&p.max_stages)
        || p.candidates == 0
        || p.max_island_size == 0
        || matches!(p.objective, Objective::Aggregate { in_flight: 0 })
    {
        return Err(PlacementError::InvalidInput("planning parameters"));
    }
    Ok(())
}

/// Defense in depth: check the exact slices returned to the caller, without
/// fractional shares or an allowance for rounding beyond a node's memory.
fn validate_footprints(
    plan: Placement,
    nodes: &[NodeSpec],
    model: &ModelShape,
) -> Result<Placement, PlacementError> {
    for (i, stage) in plan.stages.iter().enumerate() {
        for (k, member) in stage.members.iter().enumerate() {
            let mut bytes = model
                .member_layer_bytes(
                    member.experts.end - member.experts.start,
                    stage.members.len() > 1,
                )
                .and_then(|b| b.checked_mul((stage.layers.end - stage.layers.start) as u64))
                .ok_or(PlacementError::MemoryOverflow)?;
            if k == 0 {
                if i == 0 {
                    bytes = bytes
                        .checked_add(model.first_extra_bytes)
                        .ok_or(PlacementError::MemoryOverflow)?;
                }
                if i + 1 == plan.stages.len() {
                    bytes = bytes
                        .checked_add(model.last_extra_bytes)
                        .ok_or(PlacementError::MemoryOverflow)?;
                }
            }
            if bytes > nodes[member.node].mem_bytes {
                return Err(PlacementError::InvalidInput("final member exceeds memory"));
            }
        }
    }
    Ok(plan)
}

/// Plan the best placement for `model` on `nodes` given the RTT matrix `m`
/// (ms, `m[i][j]`; NaN or infinity = unmeasured).
pub fn plan(
    nodes: &[NodeSpec],
    m: &[Vec<f64>],
    model: &ModelShape,
    p: &PlacementParams,
) -> Result<Placement, PlacementError> {
    if m.len() != nodes.len() || m.iter().any(|r| r.len() != nodes.len()) {
        return Err(PlacementError::BadMatrix {
            got: m.len(),
            nodes: nodes.len(),
        });
    }
    validate_inputs(nodes, m, model, p)?;
    let all: Vec<usize> = (0..nodes.len()).collect();
    let mut best: Option<(Vec<Unit>, Scored, Vec<usize>)> = None;
    for cell in complete_linkage(m, &all, p.cluster_diameter_ms) {
        let mem: u128 = cell.iter().map(|&i| nodes[i].mem_bytes as u128).sum();
        if mem < model.total_bytes().ok_or(PlacementError::MemoryOverflow)? as u128 {
            continue;
        }
        if let Some((units, sc)) = search_cell(nodes, m, &cell, model, p)
            && best
                .as_ref()
                .is_none_or(|(_, b, _)| score_lt(sc.score, b.score))
        {
            best = Some((units, sc, cell));
        }
    }
    if let Some((units, sc, cell)) = best {
        return validate_footprints(build(&units, &sc, m, cell, false, model, p), nodes, model);
    }
    // No single cell can hold it: search everything and say so.
    let (units, sc) = search_cell(nodes, m, &all, model, p).ok_or(PlacementError::Infeasible {
        max_stages: p.max_stages,
    })?;
    validate_footprints(build(&units, &sc, m, all, true, model, p), nodes, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    fn node(id: &str, region: &str, mem_gb: u64, ms: f64) -> NodeSpec {
        NodeSpec {
            id: id.into(),
            region: region.into(),
            mem_bytes: mem_gb * GB,
            ms_per_layer: ms,
            uplink_mbps: 100.0,
        }
    }

    fn dense(layers: u32, layer_gb: u64) -> ModelShape {
        ModelShape {
            n_layers: layers,
            layer_bytes: layer_gb * GB,
            first_extra_bytes: 0,
            last_extra_bytes: 0,
            boundary_bytes: 16_000.0,
            experts_per_layer: 0,
            routed_bytes_fraction: 0.0,
            routed_time_fraction: 0.0,
        }
    }

    fn matrix(n: usize, f: impl Fn(usize, usize) -> f64) -> Vec<Vec<f64>> {
        (0..n)
            .map(|i| (0..n).map(|j| if i == j { 0.0 } else { f(i, j) }).collect())
            .collect()
    }

    fn check_tiles(p: &Placement, model: &ModelShape, nodes: &[NodeSpec]) {
        let mut at = 0;
        for (i, s) in p.stages.iter().enumerate() {
            assert_eq!(s.layers.start, at);
            assert!(s.layers.end > s.layers.start);
            at = s.layers.end;
            // Memory per member.
            let n_layers = (s.layers.end - s.layers.start) as f64;
            for (k, mbr) in s.members.iter().enumerate() {
                let share = if s.members.len() > 1 {
                    (mbr.experts.end - mbr.experts.start) as f64 / model.experts_per_layer as f64
                } else {
                    1.0
                };
                let f = if s.members.len() > 1 {
                    model.routed_bytes_fraction
                } else {
                    0.0
                };
                let mut need = n_layers * model.layer_bytes as f64 * ((1.0 - f) + f * share);
                if k == 0 && i == 0 {
                    need += model.first_extra_bytes as f64;
                }
                if k == 0 && i + 1 == p.stages.len() {
                    need += model.last_extra_bytes as f64;
                }
                assert!(
                    need <= nodes[mbr.node].mem_bytes as f64,
                    "stage {i} member {k} over memory"
                );
            }
        }
        assert_eq!(at, model.n_layers);
    }

    fn byte_model() -> ModelShape {
        ModelShape {
            n_layers: 1,
            layer_bytes: 300,
            first_extra_bytes: 0,
            last_extra_bytes: 0,
            boundary_bytes: 24.0,
            experts_per_layer: 3,
            routed_bytes_fraction: 1.0,
            routed_time_fraction: 1.0,
        }
    }

    fn byte_nodes(mem: &[u64]) -> Vec<NodeSpec> {
        mem.iter()
            .enumerate()
            .map(|(i, &mem_bytes)| NodeSpec {
                id: i.to_string(),
                region: "lab".into(),
                mem_bytes,
                ms_per_layer: 1.0,
                uplink_mbps: 100.0,
            })
            .collect()
    }

    #[test]
    fn integer_experts_reject_200_bytes_on_150_byte_node() {
        assert!(matches!(
            plan(
                &byte_nodes(&[150, 150]),
                &matrix(2, |_, _| 0.2),
                &byte_model(),
                &PlacementParams::default()
            ),
            Err(PlacementError::Infeasible { .. })
        ));
    }

    #[test]
    fn uneven_expert_shares_drive_both_memory_and_compute() {
        for memories in [&[200, 100][..], &[100, 200][..], &[201, 101][..]] {
            let nodes = byte_nodes(memories);
            let model = byte_model();
            let result = plan(
                &nodes,
                &matrix(2, |_, _| 0.2),
                &model,
                &PlacementParams::default(),
            )
            .unwrap();
            check_tiles(&result, &model, &nodes);
            assert_eq!(result.stages[0].members[0].experts, 0..2);
            assert_eq!(result.stages[0].members[1].experts, 2..3);
            assert!((result.compute_ms - (2.0 / 3.0 + 0.05)).abs() < 1e-12);
        }
    }

    #[test]
    fn dense_replication_and_both_boundary_extras_must_fit() {
        let mut model = byte_model();
        model.layer_bytes = 400;
        model.routed_bytes_fraction = 0.75; // 100 dense + 100 per expert
        model.first_extra_bytes = 7;
        model.last_extra_bytes = 11;
        let m = matrix(2, |_, _| 0.2);
        assert!(
            plan(
                &byte_nodes(&[317, 200]),
                &m,
                &model,
                &PlacementParams::default()
            )
            .is_err()
        );
        let nodes = byte_nodes(&[318, 200]);
        let result = plan(&nodes, &m, &model, &PlacementParams::default()).unwrap();
        check_tiles(&result, &model, &nodes);
        assert!(
            plan(
                &byte_nodes(&[318, 199]),
                &m,
                &model,
                &PlacementParams::default()
            )
            .is_err()
        );
    }

    #[test]
    fn expert_bytes_round_up_and_memory_arithmetic_is_checked() {
        let mut model = byte_model();
        model.layer_bytes = 301; // 101 bytes per indivisible equal expert
        assert!(
            plan(
                &byte_nodes(&[201, 101]),
                &matrix(2, |_, _| 0.2),
                &model,
                &PlacementParams::default()
            )
            .is_err()
        );
        let nodes = byte_nodes(&[202, 101]);
        assert!(
            plan(
                &nodes,
                &matrix(2, |_, _| 0.2),
                &model,
                &PlacementParams::default()
            )
            .is_ok()
        );
        model.layer_bytes = u64::MAX;
        model.n_layers = 2;
        assert_eq!(
            plan(
                &nodes,
                &matrix(2, |_, _| 0.2),
                &model,
                &PlacementParams::default()
            )
            .unwrap_err(),
            PlacementError::MemoryOverflow
        );
        model.n_layers = 1;
        model.first_extra_bytes = 1;
        assert_eq!(model.total_bytes(), None);
        model.first_extra_bytes = 0;
        model.routed_bytes_fraction = 0.5;
        assert_eq!(
            model.member_layer_bytes(1, true),
            Some((1u64 << 63) + ((1u64 << 63) - 1).div_ceil(3))
        );
        // Aggregate node memory may exceed u64 even when the model does not.
        let nodes = byte_nodes(&[u64::MAX, u64::MAX]);
        assert!(
            plan(
                &nodes,
                &matrix(2, |_, _| 0.2),
                &byte_model(),
                &PlacementParams::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn unknown_asymmetric_rtt_never_forms_a_measured_link() {
        let nodes = byte_nodes(&[150, 150]);
        let mut model = byte_model();
        model.n_layers = 2;
        model.layer_bytes = 150;
        model.experts_per_layer = 0;
        model.routed_bytes_fraction = 0.0;
        model.routed_time_fraction = 0.0;
        for unknown in [f64::NAN, f64::INFINITY] {
            for m in [
                vec![vec![0.0, unknown], vec![1.0, 0.0]],
                vec![vec![0.0, 1.0], vec![unknown, 0.0]],
            ] {
                assert_eq!(complete_linkage(&m, &[0, 1], 60.0), vec![vec![0], vec![1]]);
                assert!(plan(&nodes, &m, &model, &PlacementParams::default()).is_err());
            }
        }
    }

    #[test]
    fn invalid_numerical_domains_are_rejected_before_search() {
        let m = matrix(1, |_, _| 0.0);
        for bad in [-1.0, 0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for speed in [true, false] {
                let mut nodes = byte_nodes(&[1000]);
                if speed {
                    nodes[0].ms_per_layer = bad;
                } else {
                    nodes[0].uplink_mbps = bad;
                }
                assert!(matches!(
                    plan(&nodes, &m, &byte_model(), &PlacementParams::default()),
                    Err(PlacementError::InvalidInput(_))
                ));
            }
        }
        for bad in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            for bytes in [true, false] {
                let mut model = byte_model();
                if bytes {
                    model.routed_bytes_fraction = bad;
                } else {
                    model.routed_time_fraction = bad;
                }
                assert!(
                    plan(
                        &byte_nodes(&[1000]),
                        &m,
                        &model,
                        &PlacementParams::default()
                    )
                    .is_err()
                );
            }
        }
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            let mut model = byte_model();
            model.boundary_bytes = bad;
            assert!(
                plan(
                    &byte_nodes(&[1000]),
                    &m,
                    &model,
                    &PlacementParams::default()
                )
                .is_err()
            );
            for field in 0..4 {
                let mut p = PlacementParams::default();
                match field {
                    0 => p.hop_overhead_ms = bad,
                    1 => p.cluster_diameter_ms = bad,
                    2 => p.island_rtt_ms = bad,
                    _ => p.island_exchange_ms_per_layer = bad,
                }
                assert!(plan(&byte_nodes(&[1000]), &m, &byte_model(), &p).is_err());
            }
        }
        assert!(
            plan(
                &byte_nodes(&[1000, 1000]),
                &matrix(2, |_, _| -1.0),
                &byte_model(),
                &PlacementParams::default()
            )
            .is_err()
        );
    }

    #[test]
    fn complete_linkage_respects_diameter() {
        // A chain 0-1-2-3 with 10 ms steps: single linkage would join all at
        // 10 ms; complete linkage at 15 ms gives pairs.
        let m = matrix(4, |i, j| 10.0 * (i as f64 - j as f64).abs());
        let c = complete_linkage(&m, &[0, 1, 2, 3], 15.0);
        assert_eq!(c, vec![vec![0, 1], vec![2, 3]]);
        for cl in &c {
            for &a in cl {
                for &b in cl {
                    assert!(rtt(&m, a, b) <= 15.0);
                }
            }
        }
    }

    #[test]
    fn picks_the_cheaper_of_one_slow_node_and_a_fast_pipeline() {
        let nodes = vec![
            node("a", "x", 100, 1.0),
            node("b", "x", 40, 0.5),
            node("c", "x", 40, 0.5),
        ];
        let m = matrix(3, |_, _| 10.0);
        let model = dense(32, 2);
        let p = plan(&nodes, &m, &model, &PlacementParams::default()).expect("plan");
        // At 10 ms RTT two fast stages (16 ms compute + 2 hops ≈ 13.3 ms)
        // beat one slow node (32 ms).
        let single: f64 = 32.0;
        let pipe = 32.0 * 0.5 + 2.0 * (5.0 + 1.0) + 16_000.0 * 8.0 / 100e3;
        assert!((p.pass_ms - single.min(pipe)).abs() < 0.2, "{}", p.pass_ms);
        check_tiles(&p, &model, &nodes);
        // With 40 ms RTT the single node wins.
        let m = matrix(3, |_, _| 40.0);
        let p = plan(&nodes, &m, &model, &PlacementParams::default()).expect("plan");
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].members[0].node, 0);
        assert!(p.stages[0].hop.is_none());
    }

    #[test]
    fn stays_inside_one_region() {
        // EU: nodes 0-3, 8 ms apart, fast. US: nodes 4-7, 12 ms apart.
        // Cross-Atlantic 75 ms. The model needs three nodes' memory.
        let mut nodes = Vec::new();
        for i in 0..4 {
            nodes.push(node(&format!("eu{i}"), "EU", 48, 1.0));
        }
        for i in 0..4 {
            nodes.push(node(&format!("us{i}"), "US", 48, 0.9));
        }
        let m = matrix(8, |i, j| match (i < 4, j < 4) {
            (true, true) => 8.0,
            (false, false) => 12.0,
            _ => 75.0,
        });
        let model = dense(30, 4); // 120 GB
        let p = plan(&nodes, &m, &model, &PlacementParams::default()).expect("plan");
        assert!(!p.cross_cluster);
        let regions: Vec<&str> = p
            .stages
            .iter()
            .flat_map(|s| s.members.iter().map(|m| nodes[m.node].region.as_str()))
            .collect();
        assert!(regions.iter().all(|r| *r == regions[0]), "{regions:?}");
        assert_eq!(p.stages.len(), 3);
        check_tiles(&p, &model, &nodes);
    }

    #[test]
    fn ring_order_is_optimal_against_brute_force() {
        // Five nodes on a line; the model needs four of them.
        let pos: [f64; 5] = [0.0, 3.0, 9.0, 10.0, 25.0];
        let speeds = [1.0, 0.7, 1.2, 0.8, 0.6];
        let nodes: Vec<NodeSpec> = (0..5)
            .map(|i| node(&format!("n{i}"), "x", 26, speeds[i]))
            .collect();
        let m = matrix(5, |i, j| (pos[i] - pos[j]).abs() * 2.0 + 1.0);
        let model = dense(40, 2);
        let params = PlacementParams {
            cluster_diameter_ms: 1e9,
            ..PlacementParams::default()
        };
        let p = plan(&nodes, &m, &model, &params).expect("plan");
        // Brute force over every ordered subset of size 1..=5.
        let units: Vec<Unit> = (0..5)
            .map(|i| Unit {
                members: vec![i],
                experts: std::iter::once(0..model.experts_per_layer).collect(),
                ms_per_layer: speeds[i],
                uplink_mbps: 100.0,
            })
            .collect();
        let mut best = f64::INFINITY;
        fn perms(items: &mut Vec<usize>, k: usize, out: &mut Vec<Vec<usize>>) {
            if k == items.len() {
                out.push(items.clone());
                return;
            }
            for i in k..items.len() {
                items.swap(k, i);
                perms(items, k + 1, out);
                items.swap(k, i);
            }
        }
        for mask in 1u32..32 {
            let mut set: Vec<usize> = (0..5).filter(|i| mask & (1 << i) != 0).collect();
            let mut all = Vec::new();
            perms(&mut set, 0, &mut all);
            for order in all {
                if let Some(sc) = score_order(&order, &units, &nodes, &m, &model, &params) {
                    best = best.min(sc.score.0);
                }
            }
        }
        assert!(
            (p.pass_ms - best).abs() < 1e-6,
            "optimizer {} vs brute force {best}",
            p.pass_ms
        );
        check_tiles(&p, &model, &nodes);
    }

    #[test]
    fn moe_island_splits_experts_contiguously() {
        // Three 256 GB machines on one LAN (0.2 ms), none can hold a 600 GB
        // MoE alone; a fourth machine is 30 ms away.
        let nodes = vec![
            node("a", "lan", 256, 0.6),
            node("b", "lan", 256, 0.6),
            node("c", "lan", 256, 0.6),
            node("far", "wan", 512, 0.5),
        ];
        let m = matrix(4, |i, j| if i < 3 && j < 3 { 0.2 } else { 30.0 });
        let model = ModelShape {
            n_layers: 60,
            layer_bytes: 10 * GB,
            first_extra_bytes: GB / 2,
            last_extra_bytes: GB / 2,
            boundary_bytes: 7168.0 * 3.0,
            experts_per_layer: 384,
            routed_bytes_fraction: 0.95,
            routed_time_fraction: 0.8,
        };
        let p = plan(&nodes, &m, &model, &PlacementParams::default()).expect("plan");
        check_tiles(&p, &model, &nodes);
        let island = p
            .stages
            .iter()
            .find(|s| s.members.len() > 1)
            .expect("an island stage");
        let mut at = 0;
        for mbr in &island.members {
            assert_eq!(mbr.experts.start, at);
            at = mbr.experts.end;
        }
        assert_eq!(at, 384);
        // The whole model fits on the island: no WAN hop at all.
        assert_eq!(p.stages.len(), 1, "{:?}", p.stages);
        assert!(p.network_ms < 1e-9);
    }

    #[test]
    fn aggregate_objective_balances_stage_time() {
        let nodes = vec![node("fast", "x", 100, 0.5), node("slow", "x", 100, 1.0)];
        let m = matrix(2, |_, _| 10.0);
        let model = dense(30, 2);
        let params = PlacementParams {
            objective: Objective::Aggregate { in_flight: 64 },
            max_stages: 2,
            ..PlacementParams::default()
        };
        let p = plan(&nodes, &m, &model, &params).expect("plan");
        if p.stages.len() == 2 {
            let fast = p
                .stages
                .iter()
                .find(|s| s.members[0].node == 0)
                .expect("fast");
            let slow = p
                .stages
                .iter()
                .find(|s| s.members[0].node == 1)
                .expect("slow");
            let (f, s) = (fast.layers.len(), slow.layers.len());
            assert!(f > s, "fast {f} slow {s}");
            assert!(
                (fast.compute_ms - slow.compute_ms).abs() <= 1.5,
                "{} vs {}",
                fast.compute_ms,
                slow.compute_ms
            );
        }
        // Per-answer on the same inputs puts everything on the fast node.
        let p1 = plan(&nodes, &m, &model, &PlacementParams::default()).expect("plan");
        assert_eq!(p1.stages.len(), 1);
        assert_eq!(p1.stages[0].members[0].node, 0);
        assert!(p.aggregate_tok_s >= p1.aggregate_tok_s * 0.99);
    }

    #[test]
    fn infeasible_and_bad_inputs() {
        let nodes = vec![node("a", "x", 1, 1.0)];
        let m = matrix(1, |_, _| 0.0);
        assert_eq!(
            plan(&nodes, &m, &dense(10, 1), &PlacementParams::default()).unwrap_err(),
            PlacementError::Infeasible { max_stages: 8 }
        );
        assert!(matches!(
            plan(&nodes, &[], &dense(10, 1), &PlacementParams::default()),
            Err(PlacementError::BadMatrix { .. })
        ));
    }
}
