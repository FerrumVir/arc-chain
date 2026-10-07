//! Island and swarm formation (research-6 §6.2–§6.4, research-7 §3.5).
//!
//! Formation is a pure function of the device descriptors, the measured
//! links, the policy and the time:
//!
//! 1. **Screen.** A device takes part only if its owner's grant covers this
//!    owner and device and has not expired (both answers, #138), it
//!    reproduced the golden digest, it reports memory, its evidence is fresh
//!    and measured (synthetic inputs only when the policy allows them, which
//!    marks the plan synthetic), it meets the availability floor, and it can
//!    hold the largest layer unit.
//! 2. **T0.** A device that holds the whole model plus KV and headroom serves
//!    alone. Cluster only when the model does not fit (research-6 §6.1).
//! 3. **T1 islands.** Devices on one owner-declared LAN site whose measured
//!    links meet the LAN bound (p99 ≤ 0.5 ms). T1a (tensor parallel) needs
//!    measured RDMA and a collective p99 ≤ 0.2 ms on every member and spare;
//!    a Thunderbolt flag or a low RTT is not enough.
//! 4. **T2 swarms.** The rest, cell by cell: metro, then zone (country), then
//!    region, then a region with its declared neighbours, each with its own
//!    measured p95 diameter and stage cap. Labels only group the search;
//!    every pair in a swarm, spares included, is within the diameter by
//!    direct measurement (complete-linkage property).
//! 5. **Spares are required** (research-6 §6.4): 1 up to 6 stages, 2 up to
//!    22, 3 above. Every spare can hold the largest stage and meets the link
//!    rule with every member, so it can replace any stage. An island that
//!    cannot reserve its spares does not form.
//!
//! Inside a group, devices join largest-memory first (fewest machines,
//! research-6 §6.1 rule 2). The largest devices of the group become the
//! spares, which caps every stage at the smallest spare's memory; members
//! are added until the capped capacity holds the model with headroom. The
//! members are then ordered into the cheapest ring and partitioned by
//! [`crate::fit`].

use crate::device::{ComputePool, DeviceDescriptor, Freshness, LinkStats, Provenance, RttSource};
use crate::fit::{Stage, StageCapacity, partition, ring_cost_us, ring_order};
use crate::model::{ModelIdentity, ModelSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Tiers of research-6 §6.2, with T2 split by cell level (research-7 §1.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    /// The whole model on one device.
    T0Single,
    /// RDMA island: measured RDMA and collective p99 ≤ 0.2 ms on every member.
    T1aRdma,
    /// LAN island: one site, link p99 ≤ 0.5 ms.
    T1bLan,
    /// Metro swarm across homes (p95 diameter 20 ms).
    T2Metro,
    /// Zone (country) swarm (35 ms).
    T2Zone,
    /// Region swarm (60 ms).
    T2Region,
    /// A region with its neighbouring regions (75 ms).
    T2Neighbour,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Self::T0Single => "T0 single device",
            Self::T1aRdma => "T1a RDMA island",
            Self::T1bLan => "T1b LAN island",
            Self::T2Metro => "T2 metro swarm",
            Self::T2Zone => "T2 zone swarm",
            Self::T2Region => "T2 region swarm",
            Self::T2Neighbour => "T2 neighbour-region swarm",
        }
    }
}

/// How the members split the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Parallelism {
    Whole,
    /// Contiguous layer runs per member (the only option across sites).
    Pipeline,
    /// Tensor/expert parallel over measured RDMA (T1a only).
    Tensor,
}

/// Which cell label groups a swarm search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CellLevel {
    Metro,
    Zone,
    Region,
    /// A region plus the regions [`FormationPolicy::neighbours`] lists for it.
    Neighbourhood,
}

/// One swarm search level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmLevel {
    pub tier: Tier,
    pub cell: CellLevel,
    /// Every pair's measured p95 RTT must be at most this.
    pub diameter_p95_us: u32,
    pub max_stages: usize,
}

/// The link test an island was formed under; the lifecycle re-checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkRule {
    LanP99 { max_us: u32 },
    SwarmP95 { max_us: u32, qualified: bool },
}

impl LinkRule {
    /// The link exists, its evidence is fresh (and measured unless synthetic
    /// inputs are allowed), and it meets the bound.
    pub fn ok(
        self,
        l: Option<LinkStats>,
        now_ms: u64,
        fresh: &Freshness,
        allow_synthetic: bool,
    ) -> bool {
        let Some(l) = l else {
            return false;
        };
        if !l
            .evidence
            .acceptable(now_ms, fresh.link_ttl_ms, allow_synthetic)
        {
            return false;
        }
        match self {
            Self::LanP99 { max_us } => l.p99_us <= max_us,
            Self::SwarmP95 { max_us, qualified } => {
                l.p95_us <= max_us && (!qualified || l.pipeline_qualified())
            }
        }
    }
}

/// Formation parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormationPolicy {
    /// KV positions every island reserves: concurrent sequences × context.
    pub kv_positions: u64,
    /// Members are added until Σ usable ≥ (weights + KV) × (1 + this/1000).
    pub capacity_headroom_permille: u64,
    pub min_availability_permille: u16,
    /// T1a: measured collective p99 bound on every member and spare.
    pub collective_p99_max_us: u32,
    pub lan_p99_max_us: u32,
    pub max_stages_rdma: usize,
    pub max_stages_lan: usize,
    pub swarm_levels: Vec<SwarmLevel>,
    /// Swarm links must pass p99 ≤ 2 × p50 and loss ≤ 0.5%.
    pub require_link_qualification: bool,
    /// Reserve [`spares_for`] spares per cluster or do not form it.
    pub require_spares: bool,
    /// Accept synthetic inputs (simulation). Plans built from them are marked
    /// synthetic and can never serve.
    pub allow_synthetic: bool,
    pub freshness: Freshness,
    /// Region adjacency for the neighbourhood level, keyed
    /// `continent/region`.
    pub neighbours: BTreeMap<String, Vec<String>>,
}

impl FormationPolicy {
    fn base(kv_positions: u64, caps: [usize; 4]) -> Self {
        let level = |tier, cell, diameter_p95_us, max_stages| SwarmLevel {
            tier,
            cell,
            diameter_p95_us,
            max_stages,
        };
        Self {
            kv_positions,
            capacity_headroom_permille: 100,
            min_availability_permille: 0,
            collective_p99_max_us: 200,
            lan_p99_max_us: 500,
            max_stages_rdma: 4,
            max_stages_lan: 30,
            swarm_levels: vec![
                level(Tier::T2Metro, CellLevel::Metro, 20_000, caps[0]),
                level(Tier::T2Zone, CellLevel::Zone, 35_000, caps[1]),
                level(Tier::T2Region, CellLevel::Region, 60_000, caps[2]),
                level(Tier::T2Neighbour, CellLevel::Neighbourhood, 75_000, caps[3]),
            ],
            require_link_qualification: true,
            require_spares: true,
            allow_synthetic: false,
            freshness: Freshness::default(),
            neighbours: BTreeMap::new(),
        }
    }

    /// Interactive service: swarm stage caps of research-7 §6.2 `PIPE_S_MAX`
    /// (metro 4, zone 3, region 2; neighbouring regions 2).
    pub fn interactive(kv_positions: u64) -> Self {
        Self::base(kv_positions, [4, 3, 2, 2])
    }

    /// Batch service: research-6 T2-batch, up to 30 stages at every level.
    pub fn batch(kv_positions: u64) -> Self {
        Self::base(kv_positions, [30, 30, 30, 30])
    }
}

/// Warm spares per cluster (research-6 §6.4): 1 up to 6 stages, 2 up to 22,
/// 3 above. A single device (T0) is not a cluster; its failover is
/// re-prefill on another island (research-6 §6.7 step 4).
pub fn spares_for(stages: usize) -> usize {
    match stages {
        0 | 1 => 0,
        2..=6 => 1,
        7..=22 => 2,
        _ => 3,
    }
}

/// A warm spare and the stages it can replace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SparePlan {
    pub device: usize,
    pub covers: Vec<usize>,
}

/// One planned island or swarm. `members[i]` runs `stages[i]`; the ring goes
/// `members[0]` → … → `members[S-1]` → `members[0]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IslandPlan {
    pub cluster_id: String,
    pub model: ModelIdentity,
    pub tier: Tier,
    pub parallelism: Parallelism,
    /// Cell or site label.
    pub cell: String,
    pub members: Vec<usize>,
    pub stages: Vec<Stage>,
    pub spares: Vec<SparePlan>,
    /// Spares the policy requires for this many stages.
    pub required_spares: usize,
    pub link_rule: LinkRule,
    /// Synthetic if any member, spare or link was synthetic or assumed.
    pub provenance: Provenance,
    pub formed_at_ms: u64,
    pub kv_positions: u64,
    /// Σ p50 RTT around the ring, µs (0 for one member).
    pub ring_p50_us: u64,
    /// Largest measured p95 between any two members, µs.
    pub max_pair_p95_us: u32,
}

impl IslandPlan {
    /// Network hops per decoded token (the ring), 0 for one device.
    pub fn hops(&self) -> usize {
        if self.members.len() < 2 {
            0
        } else {
            self.members.len()
        }
    }

    /// Memory the largest stage needs; every spare must hold it.
    pub fn largest_stage_bytes(&self) -> u64 {
        self.stages.iter().map(Stage::need_bytes).max().unwrap_or(0)
    }
}

/// Why a device takes no part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RejectReason {
    /// No grant, a grant for another owner or device, expired, or withdrawn.
    NoConsent,
    NotQualified,
    NoMemoryFacts,
    /// Evidence older than the freshness bound, or from the future.
    StaleEvidence,
    /// Synthetic facts or class-assumed memory/bandwidth where measured
    /// inputs are required.
    UnmeasuredInputs,
    LowAvailability,
    TooSmallForOneLayer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    pub device: usize,
    pub reason: RejectReason,
}

/// The result of one formation round.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormationOutcome {
    pub islands: Vec<IslandPlan>,
    pub rejected: Vec<Rejection>,
    /// Eligible devices that ended in no island and as no spare.
    pub unused: Vec<usize>,
}

/// The screening rules of step 1.
pub fn screen(
    d: &DeviceDescriptor,
    model: &ModelSpec,
    policy: &FormationPolicy,
    now_ms: u64,
) -> Result<ComputePool, RejectReason> {
    if !d.consent.permits(d, now_ms) {
        return Err(RejectReason::NoConsent);
    }
    if !d.golden_qualified {
        return Err(RejectReason::NotQualified);
    }
    let pool = d.pool().ok_or(RejectReason::NoMemoryFacts)?;
    if !d.evidence.fresh(now_ms, policy.freshness.device_ttl_ms) {
        return Err(RejectReason::StaleEvidence);
    }
    if !d.inputs_acceptable(now_ms, &policy.freshness, policy.allow_synthetic) {
        return Err(RejectReason::UnmeasuredInputs);
    }
    if d.availability_permille < policy.min_availability_permille {
        return Err(RejectReason::LowAvailability);
    }
    let unit = model
        .layers
        .iter()
        .map(|l| l.weight_bytes + l.kv_bytes_per_position * policy.kv_positions)
        .max()
        .unwrap_or(0);
    if pool.usable_bytes < unit {
        return Err(RejectReason::TooSmallForOneLayer);
    }
    Ok(pool)
}

/// Whether every member and spare carries measured RDMA evidence good enough
/// for T1a.
pub fn rdma_qualified(
    devices: &[DeviceDescriptor],
    group: &[usize],
    policy: &FormationPolicy,
    now_ms: u64,
) -> bool {
    group.iter().all(|&i| {
        devices[i].rdma.is_some_and(|r| {
            r.rdma_up
                && r.collective_p99_us <= policy.collective_p99_max_us
                && r.evidence.acceptable(
                    now_ms,
                    policy.freshness.device_ttl_ms,
                    policy.allow_synthetic,
                )
        })
    })
}

struct Ctx<'a> {
    devices: &'a [DeviceDescriptor],
    pools: BTreeMap<usize, ComputePool>,
    rtt: &'a dyn RttSource,
    model: &'a ModelSpec,
    policy: &'a FormationPolicy,
    now_ms: u64,
    target: u64,
}

/// A chosen group.
struct Pick {
    members: Vec<usize>,
    stages: Vec<Stage>,
    spares: Vec<usize>,
}

impl Ctx<'_> {
    fn usable(&self, i: usize) -> u64 {
        self.pools[&i].usable_bytes
    }

    fn link_ok(&self, rule: LinkRule, a: usize, b: usize) -> bool {
        rule.ok(
            self.rtt.link(a, b),
            self.now_ms,
            &self.policy.freshness,
            self.policy.allow_synthetic,
        )
    }

    /// Sort key: largest usable memory, then fastest, then index.
    fn sort(&self, pool: &mut [usize]) {
        pool.sort_by_key(|&i| {
            let p = &self.pools[&i];
            (
                std::cmp::Reverse(p.usable_bytes),
                std::cmp::Reverse(p.bandwidth_mb_s),
                i,
            )
        });
    }

    fn max_pair_p95(&self, members: &[usize]) -> u32 {
        let mut worst = 0;
        for (k, &a) in members.iter().enumerate() {
            for &b in &members[k + 1..] {
                worst = worst.max(self.rtt.link(a, b).map_or(u32::MAX, |l| l.p95_us));
            }
        }
        worst
    }

    /// Splits a clique (sorted largest first) into spares and members: the
    /// `k` largest are the spares, with `k = spares_for(members)`, and every
    /// stage is capped at the smallest spare's memory.
    fn split(&self, clique: &[usize], max_stages: usize) -> Option<Pick> {
        let ks: &[usize] = if self.policy.require_spares {
            &[1, 2, 3]
        } else {
            &[0]
        };
        for &k in ks {
            if clique.len() <= k {
                continue;
            }
            let members = &clique[k..];
            if members.len() > max_stages
                || (self.policy.require_spares && spares_for(members.len()) != k)
            {
                continue;
            }
            let cap = clique[..k]
                .iter()
                .map(|&i| self.usable(i))
                .min()
                .unwrap_or(u64::MAX);
            let capped: u64 = members.iter().map(|&i| self.usable(i).min(cap)).sum();
            if capped < self.target {
                continue;
            }
            let order = ring_order(members, self.rtt);
            let caps: Vec<StageCapacity> = order
                .iter()
                .map(|i| StageCapacity {
                    usable_bytes: self.pools[i].usable_bytes.min(cap),
                    bandwidth_mb_s: self.pools[i].bandwidth_mb_s,
                })
                .collect();
            if let Some(stages) = partition(self.model, &caps, self.policy.kv_positions) {
                return Some(Pick {
                    members: order,
                    stages,
                    spares: clique[..k].to_vec(),
                });
            }
        }
        None
    }

    /// Picks one island from `pool` (sorted): grows a clique largest first,
    /// every pair passing `rule`, until it splits into members that hold the
    /// model with headroom plus the spares the policy requires.
    fn pick(&self, pool: &[usize], rule: LinkRule, max_stages: usize) -> Option<Pick> {
        let limit = max_stages + if self.policy.require_spares { 3 } else { 0 };
        if pool.iter().map(|&i| self.usable(i)).sum::<u64>() < self.target {
            return None;
        }
        for (s, &seed) in pool.iter().enumerate() {
            // Later seeds are smaller; stop when even the best case cannot fit.
            let best_case: u64 = pool[s..].iter().take(limit).map(|&i| self.usable(i)).sum();
            if best_case < self.target {
                return None;
            }
            let mut clique = vec![seed];
            if let Some(p) = self.split(&clique, max_stages) {
                return Some(p);
            }
            for &c in &pool[s + 1..] {
                if clique.len() >= limit {
                    break;
                }
                if !clique.iter().all(|&m| self.link_ok(rule, c, m)) {
                    continue;
                }
                clique.push(c);
                if let Some(p) = self.split(&clique, max_stages) {
                    return Some(p);
                }
            }
        }
        None
    }

    fn plan(
        &self,
        tier: Tier,
        parallelism: Parallelism,
        cell: String,
        rule: LinkRule,
        pick: Pick,
    ) -> IslandPlan {
        let Pick {
            members,
            stages,
            spares,
        } = pick;
        let mut h = Sha256::new();
        h.update(b"arc-island/cluster-id/v1\n");
        h.update(
            format!(
                "{}\n{}\n",
                self.model.identity.checkpoint, self.model.identity.quant_profile
            )
            .as_bytes(),
        );
        h.update(format!("{tier:?}\n").as_bytes());
        for (m, s) in members.iter().zip(&stages) {
            h.update(
                format!(
                    "{} {} {}\n",
                    self.devices[*m].device_id, s.layers.start, s.layers.end
                )
                .as_bytes(),
            );
        }
        for &sp in &spares {
            h.update(format!("spare {}\n", self.devices[sp].device_id).as_bytes());
        }
        let everyone: Vec<usize> = members.iter().chain(&spares).copied().collect();
        let synthetic_link = everyone.iter().enumerate().any(|(k, &a)| {
            everyone[k + 1..].iter().any(|&b| {
                self.rtt
                    .link(a, b)
                    .is_none_or(|l| l.evidence.provenance == Provenance::Synthetic)
            })
        });
        let synthetic_device = everyone
            .iter()
            .any(|&i| self.devices[i].provenance() == Provenance::Synthetic);
        let provenance = if synthetic_link || synthetic_device {
            Provenance::Synthetic
        } else {
            Provenance::Measured
        };
        let required_spares = if self.policy.require_spares {
            spares_for(members.len())
        } else {
            0
        };
        IslandPlan {
            cluster_id: hex::encode(h.finalize()),
            model: self.model.identity.clone(),
            tier,
            parallelism,
            cell,
            ring_p50_us: ring_cost_us(&members, self.rtt),
            max_pair_p95_us: if members.len() < 2 {
                0
            } else {
                self.max_pair_p95(&members)
            },
            spares: spares
                .into_iter()
                .map(|device| SparePlan {
                    device,
                    covers: (0..stages.len()).collect(),
                })
                .collect(),
            required_spares,
            link_rule: rule,
            provenance,
            formed_at_ms: self.now_ms,
            members,
            stages,
            kv_positions: self.policy.kv_positions,
        }
    }
}

fn region_key(d: &DeviceDescriptor) -> String {
    format!("{}/{}", d.location.continent, d.location.region)
}

fn cell_key(d: &DeviceDescriptor, level: CellLevel) -> String {
    let l = &d.location;
    match level {
        CellLevel::Metro => format!("{}/{}/{}/{}", l.continent, l.region, l.zone, l.metro),
        CellLevel::Zone => format!("{}/{}/{}", l.continent, l.region, l.zone),
        CellLevel::Region | CellLevel::Neighbourhood => region_key(d),
    }
}

/// Runs one formation round at `now_ms`. See the module documentation.
pub fn form(
    devices: &[DeviceDescriptor],
    rtt: &dyn RttSource,
    model: &ModelSpec,
    policy: &FormationPolicy,
    now_ms: u64,
) -> FormationOutcome {
    let mut out = FormationOutcome::default();
    let mut pools = BTreeMap::new();
    for (i, d) in devices.iter().enumerate() {
        match screen(d, model, policy, now_ms) {
            Ok(p) => {
                pools.insert(i, p);
            }
            Err(reason) => out.rejected.push(Rejection { device: i, reason }),
        }
    }
    let need = model.weight_bytes() + model.kv_bytes_per_position() * policy.kv_positions;
    let ctx = Ctx {
        devices,
        pools,
        rtt,
        model,
        policy,
        now_ms,
        target: need + need * policy.capacity_headroom_permille / 1000,
    };
    let mut free: Vec<usize> = ctx.pools.keys().copied().collect();
    ctx.sort(&mut free);
    let take = |free: &mut Vec<usize>, pick: &Pick| {
        free.retain(|i| !pick.members.contains(i) && !pick.spares.contains(i));
    };

    // T0: whole model on one device.
    let lan_rule = LinkRule::LanP99 {
        max_us: policy.lan_p99_max_us,
    };
    for i in free.clone() {
        if ctx.usable(i) < ctx.target {
            continue;
        }
        let caps = [StageCapacity {
            usable_bytes: ctx.usable(i),
            bandwidth_mb_s: ctx.pools[&i].bandwidth_mb_s,
        }];
        if let Some(stages) = partition(model, &caps, policy.kv_positions) {
            let pick = Pick {
                members: vec![i],
                stages,
                spares: Vec::new(),
            };
            take(&mut free, &pick);
            let cell = devices[i]
                .site
                .clone()
                .unwrap_or_else(|| cell_key(&devices[i], CellLevel::Metro));
            out.islands
                .push(ctx.plan(Tier::T0Single, Parallelism::Whole, cell, lan_rule, pick));
        }
    }

    // T1: one LAN site.
    let mut sites: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for &i in &free {
        if let Some(site) = &devices[i].site {
            sites.entry(site.clone()).or_default().push(i);
        }
    }
    for (site, mut pool) in sites {
        ctx.sort(&mut pool);
        while let Some(pick) = ctx.pick(&pool, lan_rule, policy.max_stages_lan) {
            let group: Vec<usize> = pick.members.iter().chain(&pick.spares).copied().collect();
            let (tier, par) = if pick.members.len() <= policy.max_stages_rdma
                && rdma_qualified(devices, &group, policy, now_ms)
            {
                (Tier::T1aRdma, Parallelism::Tensor)
            } else {
                (Tier::T1bLan, Parallelism::Pipeline)
            };
            pool.retain(|i| !group.contains(i));
            take(&mut free, &pick);
            out.islands
                .push(ctx.plan(tier, par, site.clone(), lan_rule, pick));
        }
    }

    // T2: swarms, metro → zone → region → neighbouring regions.
    for level in &policy.swarm_levels {
        let rule = LinkRule::SwarmP95 {
            max_us: level.diameter_p95_us,
            qualified: policy.require_link_qualification,
        };
        let groups: Vec<(String, BTreeSet<String>)> = if level.cell == CellLevel::Neighbourhood {
            let regions: BTreeSet<String> = free.iter().map(|&i| region_key(&devices[i])).collect();
            regions
                .into_iter()
                .filter_map(|r| {
                    let ns = policy.neighbours.get(&r)?;
                    let mut keys: BTreeSet<String> = ns.iter().cloned().collect();
                    keys.insert(r.clone());
                    let label = keys.iter().cloned().collect::<Vec<_>>().join("+");
                    Some((label, keys))
                })
                .collect()
        } else {
            let cells: BTreeSet<String> = free
                .iter()
                .map(|&i| cell_key(&devices[i], level.cell))
                .collect();
            cells
                .into_iter()
                .map(|c| (c.clone(), [c].into_iter().collect()))
                .collect()
        };
        for (label, keys) in groups {
            let mut pool: Vec<usize> = free
                .iter()
                .copied()
                .filter(|&i| keys.contains(&cell_key(&devices[i], level.cell)))
                .collect();
            ctx.sort(&mut pool);
            while let Some(pick) = ctx.pick(&pool, rule, level.max_stages) {
                pool.retain(|i| !pick.members.contains(i) && !pick.spares.contains(i));
                take(&mut free, &pick);
                out.islands.push(ctx.plan(
                    level.tier,
                    Parallelism::Pipeline,
                    label.clone(),
                    rule,
                    pick,
                ));
            }
        }
    }

    out.unused = free;
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::device::{
        Consent, Evidence, IslandFacts, Location, Measured, RdmaEvidence, RttMatrix,
        assumed_bandwidth_mb_s,
    };
    use crate::device::{PoolKind, USABLE_PERMILLE_UNIFIED};

    /// A unified-memory machine with measured memory and bandwidth (the
    /// class values), measured at t = 0, consent granted forever.
    pub fn device(
        id: &str,
        mem_gb: u32,
        tb5: bool,
        site: Option<&str>,
        metro: &str,
    ) -> DeviceDescriptor {
        let owner = format!("owner-{id}");
        DeviceDescriptor {
            device_id: id.into(),
            consent: Consent::grant(&owner, id, u64::MAX),
            owner,
            facts: IslandFacts {
                memory_class_gb: Some(mem_gb),
                unified_memory: true,
                gpu_vram_class_gb: None,
                thunderbolt5: Some(tb5),
                download_mbps_class: Some(1000),
            },
            golden_qualified: true,
            measured: Measured {
                usable_memory_bytes: Some(
                    u64::from(mem_gb) * 1_000_000_000 * USABLE_PERMILLE_UNIFIED / 1000,
                ),
                bandwidth_mb_s: Some(assumed_bandwidth_mb_s(PoolKind::Unified, mem_gb)),
                uplink_mbps: None,
            },
            site: site.map(str::to_string),
            location: Location {
                continent: "NA".into(),
                region: "US-East".into(),
                zone: "US".into(),
                metro: metro.into(),
            },
            availability_permille: 950,
            evidence: Evidence::measured(0),
            rdma: None,
        }
    }

    pub fn with_rdma(mut d: DeviceDescriptor) -> DeviceDescriptor {
        d.rdma = Some(RdmaEvidence {
            rdma_up: true,
            collective_p99_us: 150,
            evidence: Evidence::measured(0),
        });
        d
    }

    pub fn link(us: u32) -> LinkStats {
        LinkStats {
            p50_us: us,
            p95_us: us + us / 4,
            p99_us: us + us / 2,
            loss_permille: 0,
            samples: 200,
            evidence: Evidence::measured(0),
        }
    }

    /// Every pair at `us`, except pairs in `overrides`.
    pub fn mesh(n: usize, us: u32, overrides: &[(usize, usize, u32)]) -> RttMatrix {
        let mut m = RttMatrix::new();
        for a in 0..n {
            for b in (a + 1)..n {
                m.insert(a, b, link(us));
            }
        }
        for &(a, b, us) in overrides {
            m.insert(a, b, link(us));
        }
        m
    }

    const KV: u64 = 8 * 4096;

    fn kimi() -> ModelSpec {
        ModelSpec::kimi_k26_int4()
    }

    /// Every island holds its required spares, and each spare can hold the
    /// largest stage and meets the link rule with every member.
    fn assert_spare_policy(
        out: &FormationOutcome,
        devices: &[DeviceDescriptor],
        rtt: &dyn RttSource,
    ) {
        for p in &out.islands {
            assert_eq!(p.required_spares, spares_for(p.members.len()));
            assert_eq!(p.spares.len(), p.required_spares, "{p:?}");
            for s in &p.spares {
                assert!(devices[s.device].pool().unwrap().usable_bytes >= p.largest_stage_bytes());
                assert_eq!(s.covers, (0..p.stages.len()).collect::<Vec<_>>());
                for &m in &p.members {
                    assert!(p.link_rule.ok(
                        rtt.link(s.device, m),
                        p.formed_at_ms,
                        &Freshness::default(),
                        false
                    ));
                }
            }
        }
    }

    #[test]
    fn t1a_needs_measured_rdma_evidence_not_thunderbolt_or_rtt() {
        let devices: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|id| with_rdma(device(id, 512, true, Some("home"), "NYC")))
            .collect();
        let rtt = mesh(3, 50, &[]);
        let out = form(
            &devices,
            &rtt,
            &kimi(),
            &FormationPolicy::interactive(KV),
            0,
        );
        assert_eq!(out.islands.len(), 1);
        let isl = &out.islands[0];
        assert_eq!(isl.tier, Tier::T1aRdma);
        assert_eq!(isl.parallelism, Parallelism::Tensor);
        assert_eq!(isl.members.len(), 2, "fewest machines");
        assert_eq!(isl.spares.len(), 1);
        assert_eq!(isl.provenance, Provenance::Measured);
        assert_spare_policy(&out, &devices, &rtt);

        // Thunderbolt 5 and a 50 µs RTT without RDMA evidence: T1b.
        let plain: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|id| device(id, 512, true, Some("home"), "NYC"))
            .collect();
        let out = form(&plain, &rtt, &kimi(), &FormationPolicy::interactive(KV), 0);
        assert_eq!(out.islands[0].tier, Tier::T1bLan);
        // RDMA down or a slow collective on one machine: T1b.
        let mut slow = devices.clone();
        slow[2].rdma.as_mut().unwrap().collective_p99_us = 400;
        assert_eq!(
            form(&slow, &rtt, &kimi(), &FormationPolicy::interactive(KV), 0).islands[0].tier,
            Tier::T1bLan
        );
        let mut down = devices.clone();
        down[1].rdma.as_mut().unwrap().rdma_up = false;
        assert_eq!(
            form(&down, &rtt, &kimi(), &FormationPolicy::interactive(KV), 0).islands[0].tier,
            Tier::T1bLan
        );
        // Synthetic RDMA evidence does not count for a measured formation.
        let mut synth = devices.clone();
        synth[0].rdma.as_mut().unwrap().evidence = Evidence::synthetic(0);
        assert_eq!(
            form(&synth, &rtt, &kimi(), &FormationPolicy::interactive(KV), 0).islands[0].tier,
            Tier::T1bLan
        );
    }

    #[test]
    fn no_island_forms_without_its_required_spare() {
        let devices = vec![
            device("a", 512, false, Some("home"), "NYC"),
            device("b", 512, false, Some("home"), "NYC"),
        ];
        let rtt = mesh(2, 300, &[]);
        let out = form(
            &devices,
            &rtt,
            &kimi(),
            &FormationPolicy::interactive(KV),
            0,
        );
        assert!(
            out.islands.is_empty(),
            "two machines hold Kimi but leave no spare"
        );
        let mut optional = FormationPolicy::interactive(KV);
        optional.require_spares = false;
        let out = form(&devices, &rtt, &kimi(), &optional, 0);
        assert_eq!(out.islands[0].tier, Tier::T1bLan);
        assert!(out.islands[0].spares.is_empty());
    }

    #[test]
    fn consent_must_be_bound_unexpired_and_unwithdrawn() {
        let make = || {
            vec![
                device("a", 512, false, None, "NYC"),
                device("b", 512, false, None, "NYC"),
                device("c", 512, false, None, "NYC"),
            ]
        };
        let rtt = mesh(3, 10_000, &[]);
        assert_eq!(
            form(&make(), &rtt, &kimi(), &FormationPolicy::batch(KV), 0)
                .islands
                .len(),
            1
        );
        let cases: Vec<fn(&mut DeviceDescriptor)> = vec![
            |d| d.consent.island = None,
            |d| d.consent.compute = Some(false),
            |d| d.consent.withdraw(),
            |d| d.consent.expires_at_ms = 5,
            |d| d.consent.owner = "someone-else".into(),
            |d| d.consent.device_id = "a".into(),
        ];
        for change in cases {
            let mut devices = make();
            change(&mut devices[1]);
            let out = form(&devices, &rtt, &kimi(), &FormationPolicy::batch(KV), 10);
            assert!(out.islands.is_empty());
            assert_eq!(
                out.rejected,
                vec![Rejection {
                    device: 1,
                    reason: RejectReason::NoConsent
                }]
            );
        }
    }

    #[test]
    fn unqualified_small_stale_and_unmeasured_devices_are_screened_out() {
        let mut devices = vec![
            device("a", 512, false, None, "NYC"),
            device("b", 8, false, None, "NYC"),
            device("c", 512, false, None, "NYC"),
            device("d", 512, false, None, "NYC"),
            device("e", 512, false, None, "NYC"),
        ];
        devices[0].golden_qualified = false;
        devices[2].evidence = Evidence::measured(0);
        devices[3].evidence = Evidence::synthetic(0);
        devices[4].measured.bandwidth_mb_s = None;
        let policy = FormationPolicy::batch(KV);
        let late = Freshness::default().device_ttl_ms + 1;
        devices[0].evidence = Evidence::measured(late);
        devices[1].evidence = Evidence::measured(late);
        devices[3].evidence = Evidence::synthetic(late);
        devices[4].evidence = Evidence::measured(late);
        let out = form(&devices, &mesh(5, 10_000, &[]), &kimi(), &policy, late);
        let reasons: Vec<_> = out.rejected.iter().map(|r| (r.device, r.reason)).collect();
        assert_eq!(
            reasons,
            vec![
                (0, RejectReason::NotQualified),
                (1, RejectReason::TooSmallForOneLayer),
                (2, RejectReason::StaleEvidence),
                (3, RejectReason::UnmeasuredInputs),
                (4, RejectReason::UnmeasuredInputs),
            ]
        );
        // The simulator allows synthetic inputs; its plans are synthetic.
        let mut sim = policy.clone();
        sim.allow_synthetic = true;
        let synthetic: Vec<_> = (0..3)
            .map(|i| {
                let mut d = device(&format!("s{i}"), 512, false, None, "NYC");
                d.evidence = Evidence::synthetic(0);
                d
            })
            .collect();
        let out = form(&synthetic, &mesh(3, 10_000, &[]), &kimi(), &sim, 0);
        assert_eq!(out.islands[0].provenance, Provenance::Synthetic);
        assert!(
            form(&synthetic, &mesh(3, 10_000, &[]), &kimi(), &policy, 0)
                .islands
                .is_empty()
        );
    }

    #[test]
    fn stale_links_block_formation() {
        let devices: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|id| device(id, 512, false, None, "NYC"))
            .collect();
        let rtt = mesh(3, 10_000, &[]);
        let policy = FormationPolicy::batch(KV);
        assert_eq!(
            form(
                &devices,
                &rtt,
                &kimi(),
                &policy,
                policy.freshness.link_ttl_ms
            )
            .islands
            .len(),
            1
        );
        assert!(
            form(
                &devices,
                &rtt,
                &kimi(),
                &policy,
                policy.freshness.link_ttl_ms + 1
            )
            .islands
            .is_empty()
        );
    }

    #[test]
    fn metro_swarm_uses_fewest_machines_and_a_spare_that_holds_the_largest_stage() {
        let mut devices: Vec<_> = (0..3)
            .map(|i| device(&format!("big{i}"), 512, false, None, "NYC"))
            .collect();
        devices.extend((0..4).map(|i| device(&format!("m{i}"), 128, false, None, "NYC")));
        let rtt = mesh(7, 10_000, &[]);
        let out = form(&devices, &rtt, &kimi(), &FormationPolicy::batch(KV), 0);
        assert_eq!(out.islands.len(), 1);
        let swarm = &out.islands[0];
        assert_eq!(swarm.tier, Tier::T2Metro);
        assert_eq!(swarm.members, vec![1, 2]);
        assert_eq!(swarm.spares[0].device, 0);
        assert_eq!(swarm.hops(), 2);
        assert_spare_policy(&out, &devices, &rtt);
        // Four 128 GB machines (410 GB) cannot hold another copy.
        assert_eq!(out.unused, vec![3, 4, 5, 6]);
    }

    #[test]
    fn the_diameter_pushes_a_far_machine_to_the_region_level() {
        let devices: Vec<_> = (0..3)
            .map(|i| device(&format!("big{i}"), 512, false, None, "NYC"))
            .collect();
        let rtt = mesh(3, 10_000, &[(0, 2, 30_000), (1, 2, 30_000)]);
        let out = form(&devices, &rtt, &kimi(), &FormationPolicy::batch(KV), 0);
        assert_eq!(out.islands.len(), 1);
        assert_eq!(out.islands[0].tier, Tier::T2Region);
        assert!((20_001..=60_000).contains(&out.islands[0].max_pair_p95_us));
    }

    #[test]
    fn neighbouring_regions_are_searched_last_and_only_when_declared() {
        let mut devices: Vec<_> = (0..3)
            .map(|i| device(&format!("big{i}"), 512, false, None, "NYC"))
            .collect();
        devices[2].location.region = "US-Central".into();
        devices[2].location.metro = "Chicago".into();
        let rtt = mesh(3, 10_000, &[(0, 2, 50_000), (1, 2, 50_000)]);
        let mut policy = FormationPolicy::batch(KV);
        assert!(form(&devices, &rtt, &kimi(), &policy, 0).islands.is_empty());
        policy
            .neighbours
            .insert("NA/US-East".into(), vec!["NA/US-Central".into()]);
        let out = form(&devices, &rtt, &kimi(), &policy, 0);
        assert_eq!(out.islands.len(), 1);
        assert_eq!(out.islands[0].tier, Tier::T2Neighbour);
        assert_eq!(out.islands[0].cell, "NA/US-Central+NA/US-East");
        // Still bounded by the measured diameter.
        let far = mesh(3, 10_000, &[(0, 2, 70_000), (1, 2, 70_000)]);
        assert!(form(&devices, &far, &kimi(), &policy, 0).islands.is_empty());
    }

    #[test]
    fn interactive_stage_caps_limit_swarms() {
        // Kimi on 128 GB Macs needs 7 members, hence 2 spares: allowed for
        // batch, not for interactive (metro S ≤ 4).
        let devices: Vec<_> = (0..9)
            .map(|i| device(&format!("m{i}"), 128, false, None, "NYC"))
            .collect();
        let rtt = mesh(9, 10_000, &[]);
        let batch = form(&devices, &rtt, &kimi(), &FormationPolicy::batch(KV), 0);
        assert_eq!(batch.islands.len(), 1);
        assert_eq!(batch.islands[0].members.len(), 7);
        assert_eq!(batch.islands[0].spares.len(), 2);
        assert_spare_policy(&batch, &devices, &rtt);
        assert!(
            form(&devices[..8], &rtt, &kimi(), &FormationPolicy::batch(KV), 0)
                .islands
                .is_empty(),
            "one spare short"
        );
        assert!(
            form(
                &devices,
                &rtt,
                &kimi(),
                &FormationPolicy::interactive(KV),
                0
            )
            .islands
            .is_empty()
        );
    }

    #[test]
    fn zone_level_picks_up_what_metros_cannot_form() {
        // Five 256 GB Macs, three in one metro and two in another: no metro
        // holds Kimi plus a spare, the zone (35 ms) does.
        let devices = vec![
            device("a", 256, false, None, "NYC"),
            device("b", 256, false, None, "NYC"),
            device("c", 256, false, None, "NYC"),
            device("d", 256, false, None, "BOS"),
            device("e", 256, false, None, "BOS"),
        ];
        let cross: Vec<_> = (0..3)
            .flat_map(|a| (3..5).map(move |b| (a, b, 25_000)))
            .collect();
        let rtt = mesh(5, 8_000, &cross);
        let out = form(&devices, &rtt, &kimi(), &FormationPolicy::batch(KV), 0);
        assert_eq!(out.islands.len(), 1);
        assert_eq!(out.islands[0].tier, Tier::T2Zone);
        assert_eq!(out.islands[0].members.len(), 4);
        assert_spare_policy(&out, &devices, &rtt);
    }

    #[test]
    fn formation_is_deterministic() {
        let devices: Vec<_> = (0..14)
            .map(|i| {
                device(
                    &format!("m{i}"),
                    if i % 3 == 0 { 256 } else { 128 },
                    false,
                    None,
                    "NYC",
                )
            })
            .collect();
        let rtt = mesh(14, 9_000, &[(0, 5, 12_000), (3, 9, 15_000)]);
        let a = form(&devices, &rtt, &kimi(), &FormationPolicy::batch(KV), 0);
        let b = form(&devices, &rtt, &kimi(), &FormationPolicy::batch(KV), 0);
        assert_eq!(a, b);
        assert!(!a.islands.is_empty());
        assert_eq!(a.islands[0].cluster_id.len(), 64);
        assert_spare_policy(&a, &devices, &rtt);
    }

    #[test]
    fn lossy_links_fail_swarm_qualification() {
        let devices: Vec<_> = (0..5)
            .map(|i| device(&format!("m{i}"), 256, false, None, "NYC"))
            .collect();
        let mut rtt = mesh(5, 10_000, &[]);
        assert_eq!(
            form(&devices, &rtt, &kimi(), &FormationPolicy::batch(KV), 0)
                .islands
                .len(),
            1
        );
        let mut lossy = link(10_000);
        lossy.loss_permille = 20;
        for b in 1..5 {
            rtt.insert(0, b, lossy);
        }
        assert!(
            form(&devices, &rtt, &kimi(), &FormationPolicy::batch(KV), 0)
                .islands
                .is_empty()
        );
    }
}
