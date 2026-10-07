//! Island and swarm formation (research-6 §6.2–§6.4, research-7 §3.5).
//!
//! Formation is a pure function of the device descriptors, the measured
//! links and the policy:
//!
//! 1. **Screen.** A device takes part only if its owner opted in (both
//!    answers, #138), it reproduced the golden digest, it reports memory, it
//!    meets the availability floor, and it can hold the largest layer unit.
//! 2. **T0.** A device that holds the whole model plus KV and headroom serves
//!    alone. Cluster only when the model does not fit (research-6 §6.1).
//! 3. **T1 islands.** Devices on one owner-declared LAN site whose measured
//!    links meet the LAN bound (p99 ≤ 0.5 ms; ≤ 0.2 ms and Thunderbolt 5 on
//!    every member for T1a).
//! 4. **T2 swarms.** The rest, cell by cell: metro first, then zone (country),
//!    then region, each with its own measured p95 diameter and stage cap.
//!    Location labels only group the search; every pair in a swarm is
//!    within the diameter by direct measurement (complete-linkage property).
//! 5. **Spares.** Leftover devices become warm spares, round-robin across
//!    islands, each covering the stages whose memory it can hold.
//!
//! Inside a group, members are chosen largest-memory first (fewest machines,
//! research-6 §6.1 rule 2) until the island holds the model with headroom,
//! then ordered into the cheapest ring and partitioned by [`crate::fit`].

use crate::device::{ComputePool, DeviceDescriptor, LinkStats, RttSource};
use crate::fit::{Stage, StageCapacity, partition, ring_cost_us, ring_order};
use crate::model::ModelSpec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Tiers of research-6 §6.2, with T2 split by cell level (research-7 §1.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    /// The whole model on one device.
    T0Single,
    /// Thunderbolt 5 island: every member TB5, link p99 ≤ 0.2 ms.
    T1aThunderbolt,
    /// LAN island: one site, link p99 ≤ 0.5 ms.
    T1bLan,
    /// Metro swarm across homes (p95 diameter 20 ms).
    T2Metro,
    /// Zone (country) swarm (35 ms).
    T2Zone,
    /// Region swarm, neighbouring metros and countries (60 ms).
    T2Region,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Self::T0Single => "T0 single device",
            Self::T1aThunderbolt => "T1a Thunderbolt island",
            Self::T1bLan => "T1b LAN island",
            Self::T2Metro => "T2 metro swarm",
            Self::T2Zone => "T2 zone swarm",
            Self::T2Region => "T2 region swarm",
        }
    }
}

/// How the members split the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Parallelism {
    Whole,
    /// Contiguous layer runs per member (the only option across sites).
    Pipeline,
    /// Tensor/expert parallel over RDMA-class links (T1a only).
    Tensor,
}

/// Which cell label groups a swarm search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CellLevel {
    Metro,
    Zone,
    Region,
}

/// One swarm search level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmLevel {
    pub tier: Tier,
    pub cell: CellLevel,
    /// Every member pair's measured p95 RTT must be at most this.
    pub diameter_p95_us: u32,
    pub max_stages: usize,
}

/// Formation parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormationPolicy {
    /// KV positions every island reserves: concurrent sequences × context.
    pub kv_positions: u64,
    /// Members are added until Σ usable ≥ (weights + KV) × (1 + this/1000).
    pub capacity_headroom_permille: u64,
    pub min_availability_permille: u16,
    pub tb5_p99_max_us: u32,
    pub lan_p99_max_us: u32,
    pub max_stages_tb5: usize,
    pub max_stages_lan: usize,
    pub swarm_levels: Vec<SwarmLevel>,
    /// Swarm links must pass p99 ≤ 2 × p50 and loss ≤ 0.5%.
    pub require_link_qualification: bool,
}

impl FormationPolicy {
    fn base(kv_positions: u64, levels: [(usize, usize, usize); 1]) -> Self {
        let [(metro, zone, region)] = levels;
        Self {
            kv_positions,
            capacity_headroom_permille: 100,
            min_availability_permille: 0,
            tb5_p99_max_us: 200,
            lan_p99_max_us: 500,
            max_stages_tb5: 4,
            max_stages_lan: 30,
            swarm_levels: vec![
                SwarmLevel {
                    tier: Tier::T2Metro,
                    cell: CellLevel::Metro,
                    diameter_p95_us: 20_000,
                    max_stages: metro,
                },
                SwarmLevel {
                    tier: Tier::T2Zone,
                    cell: CellLevel::Zone,
                    diameter_p95_us: 35_000,
                    max_stages: zone,
                },
                SwarmLevel {
                    tier: Tier::T2Region,
                    cell: CellLevel::Region,
                    diameter_p95_us: 60_000,
                    max_stages: region,
                },
            ],
            require_link_qualification: true,
        }
    }

    /// Interactive service: swarm stage caps of research-7 §6.2
    /// `PIPE_S_MAX` (metro 4, zone 3, region 2).
    pub fn interactive(kv_positions: u64) -> Self {
        Self::base(kv_positions, [(4, 3, 2)])
    }

    /// Batch service: research-6 T2-batch, up to 30 stages at every level.
    pub fn batch(kv_positions: u64) -> Self {
        Self::base(kv_positions, [(30, 30, 30)])
    }
}

/// Warm spares per island (research-6 §6.4): 1 up to 6 stages, 2 up to 22,
/// 3 above.
pub fn spares_for(stages: usize) -> usize {
    match stages {
        0..=6 => 1,
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
    pub tier: Tier,
    pub parallelism: Parallelism,
    /// Cell or site label.
    pub cell: String,
    pub members: Vec<usize>,
    pub stages: Vec<Stage>,
    pub spares: Vec<SparePlan>,
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
}

/// Why a device takes no part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    NoConsent,
    NotQualified,
    NoMemoryFacts,
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
) -> Result<ComputePool, RejectReason> {
    if !d.consent.allows_island() {
        return Err(RejectReason::NoConsent);
    }
    if !d.golden_qualified {
        return Err(RejectReason::NotQualified);
    }
    let pool = d.pool().ok_or(RejectReason::NoMemoryFacts)?;
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

struct Ctx<'a> {
    devices: &'a [DeviceDescriptor],
    pools: BTreeMap<usize, ComputePool>,
    rtt: &'a dyn RttSource,
    model: &'a ModelSpec,
    policy: &'a FormationPolicy,
    target: u64,
}

/// A link test between two devices.
#[derive(Clone, Copy)]
enum LinkRule {
    LanP99(u32),
    SwarmP95 { max_us: u32, qualified: bool },
}

impl LinkRule {
    fn ok(self, l: Option<LinkStats>) -> bool {
        match (self, l) {
            (_, None) => false,
            (Self::LanP99(max), Some(l)) => l.p99_us <= max,
            (Self::SwarmP95 { max_us, qualified }, Some(l)) => {
                l.p95_us <= max_us && (!qualified || l.pipeline_qualified())
            }
        }
    }
}

impl Ctx<'_> {
    fn usable(&self, i: usize) -> u64 {
        self.pools[&i].usable_bytes
    }

    fn caps(&self, order: &[usize]) -> Vec<StageCapacity> {
        order
            .iter()
            .map(|i| StageCapacity {
                usable_bytes: self.pools[i].usable_bytes,
                bandwidth_mb_s: self.pools[i].bandwidth_mb_s,
            })
            .collect()
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

    /// Picks members from `pool` (sorted) for one island: largest first,
    /// every pair passing `rule`, until the island holds the model with
    /// headroom and a partition exists.
    fn pick(
        &self,
        pool: &[usize],
        rule: LinkRule,
        max_stages: usize,
    ) -> Option<(Vec<usize>, Vec<Stage>)> {
        if pool.iter().map(|&i| self.usable(i)).sum::<u64>() < self.target {
            return None;
        }
        for (s, &seed) in pool.iter().enumerate() {
            // Later seeds are smaller; stop when even the best case cannot fit.
            let best_case: u64 = pool[s..]
                .iter()
                .take(max_stages)
                .map(|&i| self.usable(i))
                .sum();
            if best_case < self.target {
                return None;
            }
            let mut members = vec![seed];
            let mut cap = self.usable(seed);
            for &c in &pool[s + 1..] {
                if members.len() >= max_stages {
                    break;
                }
                if !members.iter().all(|&m| rule.ok(self.rtt.link(c, m))) {
                    continue;
                }
                members.push(c);
                cap += self.usable(c);
                if cap < self.target {
                    continue;
                }
                let order = ring_order(&members, self.rtt);
                if let Some(stages) =
                    partition(self.model, &self.caps(&order), self.policy.kv_positions)
                {
                    return Some((order, stages));
                }
            }
            if members.len() == 1
                && cap >= self.target
                && let Some(stages) =
                    partition(self.model, &self.caps(&members), self.policy.kv_positions)
            {
                return Some((members, stages));
            }
        }
        None
    }

    fn plan(
        &self,
        tier: Tier,
        parallelism: Parallelism,
        cell: String,
        members: Vec<usize>,
        stages: Vec<Stage>,
    ) -> IslandPlan {
        let mut h = Sha256::new();
        h.update(b"arc-island/cluster-id/v0\n");
        h.update(self.model.name.as_bytes());
        h.update(format!("\n{tier:?}\n").as_bytes());
        for (m, s) in members.iter().zip(&stages) {
            h.update(
                format!(
                    "{} {} {}\n",
                    self.devices[*m].device_id, s.layers.start, s.layers.end
                )
                .as_bytes(),
            );
        }
        IslandPlan {
            cluster_id: hex::encode(h.finalize()),
            tier,
            parallelism,
            cell,
            ring_p50_us: ring_cost_us(&members, self.rtt),
            max_pair_p95_us: if members.len() < 2 {
                0
            } else {
                self.max_pair_p95(&members)
            },
            members,
            stages,
            spares: Vec::new(),
            kv_positions: self.policy.kv_positions,
        }
    }
}

fn cell_key(d: &DeviceDescriptor, level: CellLevel) -> String {
    let l = &d.location;
    match level {
        CellLevel::Metro => format!("{}/{}/{}/{}", l.continent, l.region, l.zone, l.metro),
        CellLevel::Zone => format!("{}/{}/{}", l.continent, l.region, l.zone),
        CellLevel::Region => format!("{}/{}", l.continent, l.region),
    }
}

/// Runs one formation round. See the module documentation for the steps.
pub fn form(
    devices: &[DeviceDescriptor],
    rtt: &dyn RttSource,
    model: &ModelSpec,
    policy: &FormationPolicy,
) -> FormationOutcome {
    let mut out = FormationOutcome::default();
    let mut pools = BTreeMap::new();
    for (i, d) in devices.iter().enumerate() {
        match screen(d, model, policy) {
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
        target: need + need * policy.capacity_headroom_permille / 1000,
    };
    let mut free: Vec<usize> = ctx.pools.keys().copied().collect();
    ctx.sort(&mut free);

    // T0: whole model on one device.
    free.retain(|&i| {
        if ctx.usable(i) < ctx.target {
            return true;
        }
        match partition(model, &ctx.caps(&[i]), policy.kv_positions) {
            Some(stages) => {
                let cell = devices[i]
                    .site
                    .clone()
                    .unwrap_or_else(|| cell_key(&devices[i], CellLevel::Metro));
                out.islands.push(ctx.plan(
                    Tier::T0Single,
                    Parallelism::Whole,
                    cell,
                    vec![i],
                    stages,
                ));
                false
            }
            None => true,
        }
    });

    // T1: one LAN site.
    let mut sites: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for &i in &free {
        if let Some(site) = &devices[i].site {
            sites.entry(site.clone()).or_default().push(i);
        }
    }
    for (site, mut pool) in sites {
        ctx.sort(&mut pool);
        while let Some((members, stages)) = ctx.pick(
            &pool,
            LinkRule::LanP99(policy.lan_p99_max_us),
            policy.max_stages_lan,
        ) {
            let rdma = members.len() <= policy.max_stages_tb5
                && members.iter().all(|&m| devices[m].thunderbolt5())
                && members.iter().enumerate().all(|(k, &a)| {
                    members[k + 1..]
                        .iter()
                        .all(|&b| LinkRule::LanP99(policy.tb5_p99_max_us).ok(rtt.link(a, b)))
                });
            let (tier, par) = if rdma {
                (Tier::T1aThunderbolt, Parallelism::Tensor)
            } else {
                (Tier::T1bLan, Parallelism::Pipeline)
            };
            pool.retain(|i| !members.contains(i));
            free.retain(|i| !members.contains(i));
            out.islands
                .push(ctx.plan(tier, par, site.clone(), members, stages));
        }
    }

    // T2: swarms, metro → zone → region.
    for level in &policy.swarm_levels {
        let mut cells: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for &i in &free {
            cells
                .entry(cell_key(&devices[i], level.cell))
                .or_default()
                .push(i);
        }
        let rule = LinkRule::SwarmP95 {
            max_us: level.diameter_p95_us,
            qualified: policy.require_link_qualification,
        };
        for (cell, mut pool) in cells {
            ctx.sort(&mut pool);
            while let Some((members, stages)) = ctx.pick(&pool, rule, level.max_stages) {
                pool.retain(|i| !members.contains(i));
                free.retain(|i| !members.contains(i));
                out.islands.push(ctx.plan(
                    level.tier,
                    Parallelism::Pipeline,
                    cell.clone(),
                    members,
                    stages,
                ));
            }
        }
    }

    assign_spares(&ctx, &mut out.islands, &mut free);
    out.unused = free;
    out
}

/// Round-robin spare assignment from the leftover devices.
fn assign_spares(ctx: &Ctx<'_>, islands: &mut [IslandPlan], free: &mut Vec<usize>) {
    let policy = ctx.policy;
    let max_round = islands
        .iter()
        .map(|p| spares_for(p.members.len()))
        .max()
        .unwrap_or(0);
    for round in 0..max_round {
        for plan in islands.iter_mut() {
            if round >= spares_for(plan.members.len()) || plan.tier == Tier::T0Single {
                continue;
            }
            let rule = match plan.tier {
                Tier::T1aThunderbolt | Tier::T1bLan => LinkRule::LanP99(policy.lan_p99_max_us),
                tier => {
                    let level = policy.swarm_levels.iter().find(|l| l.tier == tier);
                    LinkRule::SwarmP95 {
                        max_us: level.map_or(0, |l| l.diameter_p95_us),
                        qualified: policy.require_link_qualification,
                    }
                }
            };
            let anchor = &ctx.devices[plan.members[0]];
            let mut best: Option<(usize, usize, u64)> = None;
            for (k, &c) in free.iter().enumerate() {
                let d = &ctx.devices[c];
                // Labels prune; measured links decide.
                let same_place = match plan.tier {
                    Tier::T1aThunderbolt | Tier::T1bLan => {
                        d.site.is_some() && d.site == anchor.site
                    }
                    _ => {
                        d.location.continent == anchor.location.continent
                            && d.location.region == anchor.location.region
                    }
                };
                if !same_place || !plan.members.iter().all(|&m| rule.ok(ctx.rtt.link(c, m))) {
                    continue;
                }
                let usable = ctx.usable(c);
                let covers = plan
                    .stages
                    .iter()
                    .filter(|s| s.need_bytes() <= usable)
                    .count();
                if covers == 0 {
                    continue;
                }
                if best.is_none_or(|(_, bc, bu)| (covers, usable) > (bc, bu)) {
                    best = Some((k, covers, usable));
                }
            }
            if let Some((k, _, usable)) = best {
                let device = free.remove(k);
                let covers = plan
                    .stages
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.need_bytes() <= usable)
                    .map(|(i, _)| i)
                    .collect();
                plan.spares.push(SparePlan { device, covers });
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::device::{Consent, IslandFacts, Location, Measured, RttMatrix};

    pub fn device(
        id: &str,
        mem_gb: u32,
        tb5: bool,
        site: Option<&str>,
        metro: &str,
    ) -> DeviceDescriptor {
        DeviceDescriptor {
            device_id: id.into(),
            owner: format!("owner-{id}"),
            consent: Consent::opted_in(),
            facts: IslandFacts {
                memory_class_gb: Some(mem_gb),
                unified_memory: true,
                gpu_vram_class_gb: None,
                thunderbolt5: Some(tb5),
                download_mbps_class: Some(1000),
            },
            golden_qualified: true,
            measured: Measured::default(),
            site: site.map(str::to_string),
            location: Location {
                continent: "NA".into(),
                region: "US-East".into(),
                zone: "US".into(),
                metro: metro.into(),
            },
            availability_permille: 950,
        }
    }

    pub fn link(us: u32) -> LinkStats {
        LinkStats {
            p50_us: us,
            p95_us: us + us / 4,
            p99_us: us + us / 2,
            loss_permille: 0,
            samples: 200,
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

    #[test]
    fn two_512_gb_macs_on_thunderbolt_form_a_t1a_island() {
        let devices = vec![
            device("a", 512, true, Some("home"), "NYC"),
            device("b", 512, true, Some("home"), "NYC"),
            device("c", 512, true, Some("home"), "NYC"),
        ];
        let rtt = mesh(3, 50, &[]);
        let out = form(
            &devices,
            &rtt,
            &ModelSpec::kimi_k26_int4(),
            &FormationPolicy::interactive(KV),
        );
        assert_eq!(out.islands.len(), 1);
        let isl = &out.islands[0];
        assert_eq!(isl.tier, Tier::T1aThunderbolt);
        assert_eq!(isl.parallelism, Parallelism::Tensor);
        assert_eq!(isl.members.len(), 2, "fewest machines");
        // The third machine is the warm spare and covers both stages.
        assert_eq!(
            isl.spares,
            vec![SparePlan {
                device: 2,
                covers: vec![0, 1]
            }]
        );
        assert!(out.unused.is_empty());
    }

    #[test]
    fn a_slow_lan_makes_a_t1b_island() {
        let devices = vec![
            device("a", 512, true, Some("home"), "NYC"),
            device("b", 512, false, Some("home"), "NYC"),
        ];
        let rtt = mesh(2, 300, &[]);
        let out = form(
            &devices,
            &rtt,
            &ModelSpec::kimi_k26_int4(),
            &FormationPolicy::interactive(KV),
        );
        assert_eq!(out.islands[0].tier, Tier::T1bLan);
        assert_eq!(out.islands[0].parallelism, Parallelism::Pipeline);
        assert!(out.islands[0].spares.is_empty(), "no spare available");
    }

    #[test]
    fn a_machine_without_consent_never_joins() {
        let mut devices = vec![
            device("a", 512, true, Some("home"), "NYC"),
            device("b", 512, true, Some("home"), "NYC"),
        ];
        devices[1].consent.island = None;
        let rtt = mesh(2, 50, &[]);
        let out = form(
            &devices,
            &rtt,
            &ModelSpec::kimi_k26_int4(),
            &FormationPolicy::interactive(KV),
        );
        assert!(out.islands.is_empty());
        assert_eq!(
            out.rejected,
            vec![Rejection {
                device: 1,
                reason: RejectReason::NoConsent
            }]
        );
        devices[1].consent = Consent {
            compute: Some(false),
            island: Some(true),
        };
        let out = form(
            &devices,
            &rtt,
            &ModelSpec::kimi_k26_int4(),
            &FormationPolicy::interactive(KV),
        );
        assert!(out.islands.is_empty());
    }

    #[test]
    fn unqualified_and_small_devices_are_screened_out() {
        let mut devices = vec![
            device("a", 512, false, None, "NYC"),
            device("b", 8, false, None, "NYC"),
        ];
        devices[0].golden_qualified = false;
        let out = form(
            &devices,
            &mesh(2, 10_000, &[]),
            &ModelSpec::kimi_k26_int4(),
            &FormationPolicy::batch(KV),
        );
        assert_eq!(
            out.rejected,
            vec![
                Rejection {
                    device: 0,
                    reason: RejectReason::NotQualified
                },
                Rejection {
                    device: 1,
                    reason: RejectReason::TooSmallForOneLayer
                },
            ]
        );
    }

    #[test]
    fn metro_swarm_prefers_the_largest_machines_and_respects_the_diameter() {
        // Six 128 GB Macs and two 512 GB Macs in one metro; the second big
        // Mac is 30 ms from everyone, outside the 20 ms metro diameter.
        let mut devices: Vec<_> = (0..6)
            .map(|i| device(&format!("m{i}"), 128, false, None, "NYC"))
            .collect();
        devices.push(device("big0", 512, false, None, "NYC"));
        devices.push(device("big1", 512, false, None, "NYC"));
        let far: Vec<(usize, usize, u32)> = (0..7).map(|i| (i, 7, 30_000)).collect();
        let rtt = mesh(8, 10_000, &far);
        let out = form(
            &devices,
            &rtt,
            &ModelSpec::kimi_k26_int4(),
            &FormationPolicy::batch(KV),
        );
        let metro: Vec<_> = out
            .islands
            .iter()
            .filter(|p| p.tier == Tier::T2Metro)
            .collect();
        assert_eq!(metro.len(), 1);
        let swarm = metro[0];
        // big0 (410 GB) + two 128 GB Macs (102 GB each) = 614 < 641 needed;
        // three are needed: 410 + 3 × 102.4 = 717 GB.
        assert_eq!(swarm.members[0], 6, "the largest machine is the ingress");
        assert_eq!(swarm.members.len(), 4);
        assert!(!swarm.members.contains(&7));
        assert!(swarm.max_pair_p95_us <= 20_000);
        assert_eq!(swarm.hops(), 4);
        // The far machine is outside the metro and zone diameters but within
        // the region's 60 ms, so it anchors a region swarm with the rest.
        let region: Vec<_> = out
            .islands
            .iter()
            .filter(|p| p.tier == Tier::T2Region)
            .collect();
        assert_eq!(region.len(), 1);
        assert_eq!(region[0].members[0], 7);
        assert!((20_001..=60_000).contains(&region[0].max_pair_p95_us));
    }

    #[test]
    fn interactive_stage_caps_limit_swarms() {
        // Kimi on 128 GB Macs needs 7 stages: allowed for batch, not for
        // interactive (metro S ≤ 4).
        let devices: Vec<_> = (0..8)
            .map(|i| device(&format!("m{i}"), 128, false, None, "NYC"))
            .collect();
        let rtt = mesh(8, 10_000, &[]);
        let model = ModelSpec::kimi_k26_int4();
        let batch = form(&devices, &rtt, &model, &FormationPolicy::batch(KV));
        assert_eq!(batch.islands.len(), 1);
        assert_eq!(batch.islands[0].members.len(), 7);
        assert_eq!(
            batch.islands[0].spares.len(),
            1,
            "S = 7 wants 2 spares; one is left"
        );
        let interactive = form(&devices, &rtt, &model, &FormationPolicy::interactive(KV));
        assert!(interactive.islands.is_empty());
    }

    #[test]
    fn zone_level_picks_up_what_metros_cannot_form() {
        // Four 256 GB Macs, two per metro; each metro alone cannot hold Kimi
        // (2 × 205 = 410 GB) but the zone (35 ms) can.
        let devices = vec![
            device("a", 256, false, None, "NYC"),
            device("b", 256, false, None, "NYC"),
            device("c", 256, false, None, "BOS"),
            device("d", 256, false, None, "BOS"),
        ];
        let cross = [
            (0, 2, 25_000),
            (0, 3, 25_000),
            (1, 2, 25_000),
            (1, 3, 25_000),
        ];
        let rtt = mesh(4, 8_000, &cross);
        let out = form(
            &devices,
            &rtt,
            &ModelSpec::kimi_k26_int4(),
            &FormationPolicy::batch(KV),
        );
        assert_eq!(out.islands.len(), 1);
        assert_eq!(out.islands[0].tier, Tier::T2Zone);
        assert_eq!(out.islands[0].members.len(), 4);
    }

    #[test]
    fn formation_is_deterministic() {
        let devices: Vec<_> = (0..12)
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
        let rtt = mesh(12, 9_000, &[(0, 5, 12_000), (3, 9, 15_000)]);
        let model = ModelSpec::kimi_k26_int4();
        let a = form(&devices, &rtt, &model, &FormationPolicy::batch(KV));
        let b = form(&devices, &rtt, &model, &FormationPolicy::batch(KV));
        assert_eq!(a, b);
        assert!(!a.islands.is_empty());
        assert_eq!(a.islands[0].cluster_id.len(), 64);
    }

    #[test]
    fn lossy_links_fail_swarm_qualification() {
        // Four 256 GB Macs hold Kimi; three do not.
        let devices: Vec<_> = (0..4)
            .map(|i| device(&format!("m{i}"), 256, false, None, "NYC"))
            .collect();
        let model = ModelSpec::kimi_k26_int4();
        let mut rtt = mesh(4, 10_000, &[]);
        assert_eq!(
            form(&devices, &rtt, &model, &FormationPolicy::batch(KV))
                .islands
                .len(),
            1
        );
        let mut lossy = link(10_000);
        lossy.loss_permille = 20;
        for b in 1..4 {
            rtt.insert(0, b, lossy);
        }
        assert!(
            form(&devices, &rtt, &model, &FormationPolicy::batch(KV))
                .islands
                .is_empty()
        );
    }
}
