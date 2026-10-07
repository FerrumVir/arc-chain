//! Offline capacity simulator: how many Kimi-capable islands and swarms form
//! on a synthetic community inventory, and how fast they would be.
//!
//! **Nothing here is a community number.** The inventory is synthetic. Its
//! geography and device mix are research-7 §4.1's planning inputs (themselves
//! labelled \[UNVERIFIED\] there), apportioned exactly (largest remainder), so
//! the counts are expected values, not samples. Every other input is listed
//! in [`assumptions`] and printed with the report. Projections are \[CALC\].
//!
//! **Simulation never serves.** Every synthetic device and link carries
//! synthetic evidence, formation runs with `allow_synthetic`, and islands
//! are qualified against a synthetic toy reference, so they reach the
//! `Simulated` state at most. The report counts them as simulated islands.

use crate::device::{
    Consent, DeviceDescriptor, Evidence, IslandFacts, LinkStats, Location, Measured, PoolKind,
    RdmaEvidence, RttSource,
};
use crate::economics::{CostAssumptions, CostProjection, project_cost};
use crate::form::{FormationOutcome, FormationPolicy, IslandPlan, RejectReason, Tier, form};
use crate::lifecycle::{GoldenSource, Island, Recovery, State, TrustedCheckpoint};
use crate::model::ModelSpec;
use crate::perf::{FixedDepth, NetAssumptions, Projection, project, project_fixed};
use crate::selftest::toy::ToyPipeline;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fmt::Write as _;

/// SplitMix64.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [0, n).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

struct Area {
    continent: &'static str,
    region: &'static str,
    zone: &'static str,
    /// Share of nodes at 100 nodes, % (research-7 §4.1).
    share_pct: u32,
    metros: &'static [(&'static str, u32)],
}

const AREAS: &[Area] = &[
    Area {
        continent: "NA",
        region: "US-East",
        zone: "US",
        share_pct: 16,
        metros: &[
            ("New York", 35),
            ("Washington", 20),
            ("Boston", 20),
            ("Philadelphia", 10),
            ("Atlanta", 15),
        ],
    },
    Area {
        continent: "NA",
        region: "US-Central",
        zone: "US",
        share_pct: 9,
        metros: &[("Chicago", 50), ("Dallas", 30), ("Denver", 20)],
    },
    Area {
        continent: "NA",
        region: "US-West",
        zone: "US",
        share_pct: 13,
        metros: &[("SF Bay", 40), ("Los Angeles", 35), ("Seattle", 25)],
    },
    Area {
        continent: "NA",
        region: "Canada",
        zone: "CA",
        share_pct: 4,
        metros: &[("Toronto", 50), ("Montreal", 25), ("Vancouver", 25)],
    },
    Area {
        continent: "NA",
        region: "Mexico",
        zone: "MX",
        share_pct: 1,
        metros: &[("Mexico City", 1)],
    },
    Area {
        continent: "EU",
        region: "EU-NW",
        zone: "GB",
        share_pct: 8,
        metros: &[("London", 60), ("Manchester", 25), ("Edinburgh", 15)],
    },
    Area {
        continent: "EU",
        region: "EU-NW",
        zone: "NL",
        share_pct: 3,
        metros: &[("Amsterdam", 1)],
    },
    Area {
        continent: "EU",
        region: "EU-NW",
        zone: "FR",
        share_pct: 4,
        metros: &[("Paris", 70), ("Lyon", 30)],
    },
    Area {
        continent: "EU",
        region: "EU-C",
        zone: "DE",
        share_pct: 7,
        metros: &[("Frankfurt", 35), ("Berlin", 35), ("Munich", 30)],
    },
    Area {
        continent: "EU",
        region: "EU-C",
        zone: "PL",
        share_pct: 2,
        metros: &[("Warsaw", 1)],
    },
    Area {
        continent: "EU",
        region: "EU-C",
        zone: "Nordics",
        share_pct: 2,
        metros: &[("Stockholm", 50), ("Copenhagen", 50)],
    },
    Area {
        continent: "EU",
        region: "EU-S",
        zone: "ES",
        share_pct: 2,
        metros: &[("Madrid", 60), ("Barcelona", 40)],
    },
    Area {
        continent: "EU",
        region: "EU-S",
        zone: "IT",
        share_pct: 2,
        metros: &[("Milan", 60), ("Rome", 40)],
    },
    Area {
        continent: "AS",
        region: "South Asia",
        zone: "IN",
        share_pct: 5,
        metros: &[("Mumbai", 40), ("Bangalore", 35), ("Delhi", 25)],
    },
    Area {
        continent: "AS",
        region: "East Asia",
        zone: "JP",
        share_pct: 3,
        metros: &[("Tokyo", 70), ("Osaka", 30)],
    },
    Area {
        continent: "AS",
        region: "East Asia",
        zone: "KR",
        share_pct: 2,
        metros: &[("Seoul", 1)],
    },
    Area {
        continent: "AS",
        region: "SE Asia",
        zone: "SG",
        share_pct: 2,
        metros: &[("Singapore", 1)],
    },
    Area {
        continent: "AS",
        region: "SE Asia",
        zone: "ID",
        share_pct: 1,
        metros: &[("Jakarta", 1)],
    },
    Area {
        continent: "AS",
        region: "SE Asia",
        zone: "PH",
        share_pct: 1,
        metros: &[("Manila", 1)],
    },
    Area {
        continent: "OC",
        region: "ANZ",
        zone: "AU",
        share_pct: 3,
        metros: &[("Sydney", 55), ("Melbourne", 45)],
    },
    Area {
        continent: "SA",
        region: "South Cone",
        zone: "BR",
        share_pct: 4,
        metros: &[("Sao Paulo", 70), ("Rio de Janeiro", 30)],
    },
    Area {
        continent: "SA",
        region: "South Cone",
        zone: "AR",
        share_pct: 1,
        metros: &[("Buenos Aires", 1)],
    },
    Area {
        continent: "SA",
        region: "Andean",
        zone: "CO",
        share_pct: 1,
        metros: &[("Bogota", 1)],
    },
    Area {
        continent: "AF",
        region: "West Africa",
        zone: "NG",
        share_pct: 2,
        metros: &[("Lagos", 1)],
    },
    Area {
        continent: "AF",
        region: "East Africa",
        zone: "KE",
        share_pct: 1,
        metros: &[("Nairobi", 1)],
    },
    Area {
        continent: "AF",
        region: "Southern Africa",
        zone: "ZA",
        share_pct: 1,
        metros: &[("Johannesburg", 60), ("Cape Town", 40)],
    },
];

/// A device class of research-7 §4.1's mix.
pub struct DeviceClass {
    pub name: &'static str,
    /// ‰ of nodes.
    pub share_permille: u32,
    pub facts: IslandFacts,
}

const fn class(
    name: &'static str,
    share_permille: u32,
    mem: u32,
    unified: bool,
    vram: Option<u32>,
    tb5: Option<bool>,
) -> DeviceClass {
    DeviceClass {
        name,
        share_permille,
        facts: IslandFacts {
            memory_class_gb: Some(mem),
            unified_memory: unified,
            gpu_vram_class_gb: vram,
            thunderbolt5: tb5,
            download_mbps_class: None,
        },
    }
}

/// research-7 §4.1 device mix. Splits inside a research class (16 vs 24 GB
/// Macs, Mac vs Strix at 128 GB, 256 vs 512 GB Ultras) are even \[ASSUMPTION\];
/// Thunderbolt 5 is assumed on 128 GB Macs and Ultras only.
pub const DEVICE_MIX: [DeviceClass; 16] = [
    class("8 GB GPU", 100, 16, false, Some(8), None),
    class("12 GB GPU", 150, 32, false, Some(12), None),
    class("16 GB GPU", 150, 32, false, Some(16), None),
    class("24 GB GPU", 200, 64, false, Some(24), None),
    class("32 GB GPU", 80, 64, false, Some(32), None),
    class("Mac 16 GB", 50, 16, true, None, Some(false)),
    class("Mac 24 GB", 50, 24, true, None, Some(false)),
    class("Mac 48 GB", 60, 48, true, None, Some(false)),
    class("Mac 64 GB", 60, 64, true, None, Some(false)),
    class("Mac 128 GB", 30, 128, true, None, Some(true)),
    class("Strix/GB10 128 GB", 30, 128, true, None, None),
    class("M3 Ultra 256 GB", 5, 256, true, None, Some(true)),
    class("M3 Ultra 512 GB", 5, 512, true, None, Some(true)),
    class("CPU-only 32 GB", 30, 32, false, None, None),
    // Zero-weight classes used only by the ordinary-node scenarios.
    class("16 GB RAM, CPU pool", 0, 16, false, None, None),
    class(
        "16 GB RAM + 8 GB GPU, CPU pool",
        0,
        16,
        false,
        Some(8),
        None,
    ),
];

/// Largest-remainder apportionment of `total` by `weights`.
fn apportion(total: usize, weights: &[f64]) -> Vec<usize> {
    let sum: f64 = weights.iter().sum();
    let exact: Vec<f64> = weights.iter().map(|w| total as f64 * w / sum).collect();
    let mut out: Vec<usize> = exact.iter().map(|e| e.floor() as usize).collect();
    let mut left = total - out.iter().sum::<usize>();
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|&a, &b| {
        let ra = exact[a] - exact[a].floor();
        let rb = exact[b] - exact[b].floor();
        rb.total_cmp(&ra).then(a.cmp(&b))
    });
    for i in order {
        if left == 0 {
            break;
        }
        out[i] += 1;
        left -= 1;
    }
    out
}

/// Home access RTT to the metro hub, research-7 §1.6 row "0–100 km"
/// \[MEASURED, RIPE Atlas\]: p25 3.7, median 5.5, p75 9.8, p90 22.1 ms. The
/// 0th (1 ms) and 100th (40 ms) points are \[ASSUMPTION\]. Linear between.
const ACCESS_QUANTILES: [(f64, f64); 6] = [
    (0.0, 1.0),
    (0.25, 3.7),
    (0.5, 5.5),
    (0.75, 9.8),
    (0.9, 22.1),
    (1.0, 40.0),
];

fn access_leg_us(u: f64) -> u32 {
    for w in ACCESS_QUANTILES.windows(2) {
        let ((q0, v0), (q1, v1)) = (w[0], w[1]);
        if u <= q1 {
            return ((v0 + (u - q0) / (q1 - q0) * (v1 - v0)) * 1000.0) as u32;
        }
    }
    40_000
}

/// Extra RTT between two homes beyond both access legs, ms, from research-7
/// §1.6 planning medians (metro ~10, zone ~22, region ~38, wide region ~56,
/// intercontinental ~192) minus two median access legs (2 × 5.5).
const CORE_MS: [(&str, i64); 5] = [
    ("metro", -1),
    ("zone", 11),
    ("region", 27),
    ("continent", 45),
    ("world", 181),
];

/// The synthetic inventory.
pub struct Inventory {
    pub devices: Vec<DeviceDescriptor>,
    pub class_of: Vec<usize>,
    pub online: Vec<bool>,
    pub access_us: Vec<u32>,
}

/// Share of nodes on multi-device LAN sites, 3 devices per site
/// (research-7 §4.1: 3% / 5% / 8% at 100 / 1,000 / 10,000 nodes).
fn lan_site_share_permille(nodes: usize) -> usize {
    match nodes {
        0..=999 => 30,
        1_000..=9_999 => 50,
        _ => 80,
    }
}

/// research-7 §4.1: larger networks tilt toward Asia (×1.25 at 1,000, ×1.55
/// at 10,000) and Africa (×1.2, ×1.6).
fn tilt(continent: &str, nodes: usize) -> f64 {
    match (continent, nodes) {
        ("AS", 1_000..=9_999) => 1.25,
        ("AS", 10_000..) => 1.55,
        ("AF", 1_000..=9_999) => 1.2,
        ("AF", 10_000..) => 1.6,
        _ => 1.0,
    }
}

impl Inventory {
    /// Builds `nodes` synthetic devices, each online with probability
    /// `availability_permille` (research-7 §4.1: 0.7, 0.5 sensitivity);
    /// `opt_in_permille` of owners grant both consent answers for their own
    /// device; all pass the golden self-test \[ASSUMPTION\]. All evidence
    /// is synthetic.
    pub fn synthetic(
        nodes: usize,
        seed: u64,
        opt_in_permille: u32,
        availability_permille: u16,
    ) -> Self {
        let mut rng = Rng::new(seed);
        let weights: Vec<f64> = AREAS
            .iter()
            .map(|a| f64::from(a.share_pct) * tilt(a.continent, nodes))
            .collect();
        let per_area = apportion(nodes, &weights);
        let class_counts = apportion(
            nodes,
            &DEVICE_MIX
                .iter()
                .map(|c| f64::from(c.share_permille))
                .collect::<Vec<_>>(),
        );
        let mut classes: Vec<usize> = class_counts
            .iter()
            .enumerate()
            .flat_map(|(c, &n)| std::iter::repeat_n(c, n))
            .collect();
        for i in (1..classes.len()).rev() {
            let j = rng.below(i + 1);
            classes.swap(i, j);
        }

        let mut devices = Vec::with_capacity(nodes);
        let mut metro_members: Vec<Vec<usize>> = Vec::new();
        for (area, &count) in AREAS.iter().zip(&per_area) {
            let mw: Vec<f64> = area.metros.iter().map(|m| f64::from(m.1)).collect();
            for (&(metro, _), mcount) in area.metros.iter().zip(apportion(count, &mw)) {
                let mut members = Vec::with_capacity(mcount);
                for _ in 0..mcount {
                    let i = devices.len();
                    members.push(i);
                    devices.push(DeviceDescriptor {
                        device_id: format!("dev-{i:05}"),
                        owner: format!("owner-{i:05}"),
                        consent: Consent::default(),
                        facts: DEVICE_MIX[classes[i]].facts,
                        golden_qualified: true,
                        measured: Measured::default(),
                        site: None,
                        location: Location {
                            continent: area.continent.into(),
                            region: area.region.into(),
                            zone: area.zone.into(),
                            metro: metro.into(),
                        },
                        availability_permille,
                        evidence: Evidence::synthetic(0),
                        rdma: None,
                    });
                }
                metro_members.push(members);
            }
        }

        // LAN sites: round-robin over metros, largest first, 3 devices each.
        let mut sites_left = (nodes * lan_site_share_permille(nodes) / 1000).div_ceil(3);
        let mut order: Vec<usize> = (0..metro_members.len()).collect();
        order.sort_by_key(|&m| (std::cmp::Reverse(metro_members[m].len()), m));
        let mut cursor = vec![0usize; metro_members.len()];
        let mut site_id = 0;
        while sites_left > 0 {
            let mut placed = false;
            for &m in &order {
                if sites_left == 0 {
                    break;
                }
                let members = &metro_members[m];
                if cursor[m] + 3 > members.len() {
                    continue;
                }
                let owner = format!("site-owner-{site_id:04}");
                for &i in &members[cursor[m]..cursor[m] + 3] {
                    devices[i].site = Some(format!("site-{site_id:04}"));
                    devices[i].owner = owner.clone();
                    // Thunderbolt 5 Macs on a site get a synthetic RDMA
                    // result (research-6: < 50 µs, collectives 0.1–0.2 ms).
                    if devices[i].thunderbolt5() {
                        devices[i].rdma = Some(RdmaEvidence {
                            rdma_up: true,
                            collective_p99_us: 150,
                            evidence: Evidence::synthetic(0),
                        });
                    }
                }
                cursor[m] += 3;
                site_id += 1;
                sites_left -= 1;
                placed = true;
            }
            if !placed {
                break;
            }
        }

        let mut online = Vec::with_capacity(nodes);
        let mut access_us = Vec::with_capacity(nodes);
        for d in &mut devices {
            d.consent = if rng.unit() * 1000.0 < f64::from(opt_in_permille) {
                Consent::grant(&d.owner, &d.device_id, u64::MAX)
            } else {
                Consent {
                    owner: d.owner.clone(),
                    device_id: d.device_id.clone(),
                    compute: Some(true),
                    island: None,
                    expires_at_ms: u64::MAX,
                }
            };
            online.push(rng.unit() * 1000.0 < f64::from(d.availability_permille));
            access_us.push(access_leg_us(rng.unit()));
        }
        Self {
            devices,
            class_of: classes,
            online,
            access_us,
        }
    }

    /// The devices online now, with their RTT model.
    pub fn online_snapshot(&self) -> (Vec<DeviceDescriptor>, Vec<usize>, SyntheticRtt) {
        let idx: Vec<usize> = (0..self.devices.len())
            .filter(|&i| self.online[i])
            .collect();
        let devices: Vec<DeviceDescriptor> = idx.iter().map(|&i| self.devices[i].clone()).collect();
        let rtt = SyntheticRtt {
            access_us: idx.iter().map(|&i| self.access_us[i]).collect(),
            devices: devices.clone(),
            at_ms: 0,
            fixed_regional_rtt_us: None,
        };
        (devices, idx, rtt)
    }
}

/// RTT between synthetic devices: same LAN site 0.3 ms p50 (0.04 ms when
/// both have Thunderbolt 5); otherwise both access legs plus the core for
/// their relation (`CORE_MS`). p95 = 1.25 × p50, p99 = 1.5 × p50, no loss
/// \[ASSUMPTION\].
pub struct SyntheticRtt {
    access_us: Vec<u32>,
    devices: Vec<DeviceDescriptor>,
    /// Time stamped on every link (probes repeat every 5 min).
    pub at_ms: u64,
    pub fixed_regional_rtt_us: Option<u32>,
}

impl RttSource for SyntheticRtt {
    fn link(&self, a: usize, b: usize) -> Option<LinkStats> {
        let stats = |p50: u32| LinkStats {
            p50_us: p50,
            p95_us: p50 + p50 / 4,
            p99_us: p50 + p50 / 2,
            loss_permille: 0,
            samples: 200,
            evidence: Evidence::synthetic(self.at_ms),
        };
        if a == b {
            return Some(stats(0));
        }
        let (da, db) = (&self.devices[a], &self.devices[b]);
        if let Some(us) = self.fixed_regional_rtt_us {
            return (da.location.region == db.location.region).then(|| stats(us));
        }
        if da.site.is_some() && da.site == db.site {
            return Some(stats(if da.thunderbolt5() && db.thunderbolt5() {
                40
            } else {
                300
            }));
        }
        let (la, lb) = (&da.location, &db.location);
        let relation = if la.continent != lb.continent {
            "world"
        } else if la.region != lb.region {
            "continent"
        } else if la.zone != lb.zone {
            "region"
        } else if la.metro != lb.metro {
            "zone"
        } else {
            "metro"
        };
        let core = CORE_MS.iter().find(|c| c.0 == relation).map_or(0, |c| c.1);
        let p50 =
            (i64::from(self.access_us[a]) + i64::from(self.access_us[b]) + core * 1000).max(1000);
        Some(stats(p50 as u32))
    }
}

/// Interactive or batch service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Service {
    Interactive,
    Batch,
}

/// Hardware/geography assumptions, never a discovered community inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InventoryProfile {
    /// Retained research-7 comparison, not the deployment requirement.
    ResearchMix,
    /// Every host has 16 GB RAM; research-7's dispersed geography.
    OrdinaryGlobal,
    /// Every host has 16 GB RAM, in synthetic public-internet regions of this
    /// many registered nodes. No links between regions are assumed.
    OrdinaryRegional {
        rtt_us: u32,
        nodes_per_region: usize,
    },
}

/// How a scenario chooses batch and draft depth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BatchingPolicy {
    /// Per-island search: the largest aggregate keeping per-stream speed at
    /// least `min(floor_tok_s, half the island's own single-answer rate)`.
    /// The floor follows the single-answer rate, which follows RTT, so the
    /// selected workload changes with RTT: never use these rows to compare
    /// RTTs.
    Adaptive { floor_tok_s: f64 },
    /// Depths held fixed for a controlled comparison: every swarm in the
    /// scenario runs the plain batch at `plain` and the speculative batch
    /// (and single-answer speculation) at `speculative`.
    Fixed {
        plain: FixedDepth,
        speculative: FixedDepth,
        /// How the depths were chosen.
        chosen_from: String,
    },
}

/// One simulator run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    pub inventory: InventoryProfile,
    pub batching: BatchingPolicy,
    pub costs: CostAssumptions,
    /// Reserved concurrent sequences; formation and batching share this budget.
    pub kv_sequences: u64,
    pub nodes: usize,
    pub seed: u64,
    pub opt_in_permille: u32,
    pub availability_permille: u16,
    pub service: Service,
    pub transport: String,
    pub net: NetAssumptions,
    /// The research-6 spare policy (the default); `false` only for the
    /// comparison rows that explain what the requirement costs.
    pub require_spares: bool,
}

/// Context length per sequence for KV budgets and batching.
pub const CONTEXT_POSITIONS: u64 = 4_096;
/// Concurrent sequences each island reserves KV for: 8 interactive, 32 batch.
pub fn kv_sequences(service: Service) -> u64 {
    match service {
        Service::Interactive => 8,
        Service::Batch => 32,
    }
}
/// Per-stream floor when choosing the batching depth, tok/s.
pub const BATCH_FLOOR_TOK_S: f64 = 5.0;
/// Device MTBF for churn (Salad, research-4/6: 92 h).
pub const MTBF_H: f64 = 92.0;
/// Island lease (research-6 §6.4: 6 h).
pub const LEASE_H: f64 = 6.0;
/// Stall while a promoted spare recovers and re-qualifies (research-6 §6.7
/// target: ≤ 2 s GPU, ≤ 10 s Mac); the simulator uses 10 s.
pub const PROMOTION_STALL_MS: u64 = 10_000;
/// Link a replacement spare stages its shard over (1 Gb/s).
pub const STAGING_BITS_PER_S: f64 = 1e9;
/// Opt-in share in every scenario.
pub const OPT_IN_PERMILLE: u32 = 900;

/// Region adjacency for the neighbour level \[ASSUMPTION\]: regions that share
/// a border or a short fibre corridor in the synthetic geography.
pub const NEIGHBOURS: &[(&str, &str)] = &[
    ("NA/US-East", "NA/US-Central"),
    ("NA/US-East", "NA/Canada"),
    ("NA/US-Central", "NA/US-West"),
    ("NA/US-Central", "NA/Mexico"),
    ("NA/US-West", "NA/Canada"),
    ("EU/EU-NW", "EU/EU-C"),
    ("EU/EU-NW", "EU/EU-S"),
    ("EU/EU-C", "EU/EU-S"),
    ("AS/South Asia", "AS/SE Asia"),
    ("AS/SE Asia", "AS/East Asia"),
    ("SA/South Cone", "SA/Andean"),
    ("AF/East Africa", "AF/Southern Africa"),
];

/// [`NEIGHBOURS`] as a symmetric map.
pub fn neighbour_map() -> BTreeMap<String, Vec<String>> {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for &(a, b) in NEIGHBOURS {
        m.entry(a.into()).or_default().push(b.into());
        m.entry(b.into()).or_default().push(a.into());
    }
    m
}

/// The formation policy a scenario uses: synthetic inputs allowed (so its
/// islands can never serve), the neighbour table, and the spare policy.
pub fn policy_for(s: &Scenario) -> FormationPolicy {
    let kv_positions = s.kv_sequences * CONTEXT_POSITIONS;
    let mut policy = if s.inventory != InventoryProfile::ResearchMix {
        FormationPolicy::regional(kv_positions)
    } else {
        match s.service {
            Service::Interactive => FormationPolicy::interactive(kv_positions),
            Service::Batch => FormationPolicy::batch(kv_positions),
        }
    };
    policy.allow_synthetic = true;
    policy.require_spares = s.require_spares;
    policy.neighbours = neighbour_map();
    policy
}

/// One island in the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IslandRow {
    pub tier: Tier,
    pub cell: String,
    pub members: usize,
    pub spares: usize,
    pub required_spares: usize,
    pub hops: usize,
    pub ring_p50_ms: f64,
    pub mean_hop_rtt_ms: f64,
    pub max_pair_p95_ms: f64,
    pub mix: String,
    pub spare_mix: String,
    /// SHA-256 over the member and spare device ids in stage order: equal
    /// digests mean identical placement.
    pub placement_digest: String,
    /// The lifecycle state qualification reached (always `Simulated`).
    pub state: State,
    pub projection: Projection,
    pub tokens_per_day: f64,
    pub tokens_per_day_speculative: f64,
}

/// Churn over one lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChurnReport {
    pub islands: usize,
    pub lease_duration_ms: u64,
    pub member_failures: usize,
    pub spare_failures: usize,
    pub promotions: usize,
    pub replenished: usize,
    pub dissolved: usize,
    pub survived: usize,
    /// Denominator: initially formed islands × the full lease duration.
    pub total_island_time_ms: u64,
    /// Non-dissolved island-time below the required spare count, including
    /// time with no eligible replacement (not just shard staging).
    pub spare_shortfall_time_ms: u64,
    /// Time from dissolution to lease end, disjoint from spare shortfall.
    pub dissolved_time_ms: u64,
    /// Remainder of the lease, not a measurement of serving uptime.
    pub other_time_ms: u64,
    /// spare_shortfall_time_ms / total_island_time_ms; 0 for no islands.
    pub spare_shortfall_share: f64,
    /// dissolved_time_ms / total_island_time_ms; 0 for no islands.
    pub dissolved_share: f64,
}

impl ChurnReport {
    fn account_interval(&mut self, island: &Island, duration_ms: u64) {
        self.total_island_time_ms += duration_ms;
        if island.is_dissolved() {
            self.dissolved_time_ms += duration_ms;
        } else if island.spare_shortfall() > 0 {
            self.spare_shortfall_time_ms += duration_ms;
        } else {
            self.other_time_ms += duration_ms;
        }
    }

    fn finish_accounting(&mut self) {
        if self.total_island_time_ms > 0 {
            self.spare_shortfall_share =
                self.spare_shortfall_time_ms as f64 / self.total_island_time_ms as f64;
            self.dissolved_share = self.dissolved_time_ms as f64 / self.total_island_time_ms as f64;
        }
    }
}

/// One scenario's results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioReport {
    pub scenario: Scenario,
    pub online: usize,
    pub opted_in_online: usize,
    pub eligible_online: usize,
    pub rejected: BTreeMap<String, usize>,
    pub islands_by_tier: BTreeMap<String, usize>,
    /// Islands that reached `Serving` (always 0: synthetic evidence).
    pub serving_islands: usize,
    pub simulated_islands: usize,
    pub machines_serving: usize,
    pub spares: usize,
    pub unused_eligible: usize,
    /// Memory one Kimi copy needs with headroom, GB.
    pub needed_gb: f64,
    /// The region (L2 cell) with the most usable memory among eligible
    /// online devices, and that memory in GB.
    pub largest_region: (String, f64),
    pub rows: Vec<IslandRow>,
    pub aggregate_tok_s: f64,
    pub aggregate_tok_s_speculative: f64,
    pub tokens_per_day: f64,
    pub tokens_per_day_speculative: f64,
    pub churn: ChurnReport,
    pub cost_plain: CostProjection,
    pub cost_speculative: CostProjection,
    /// Each external input is an assumption; outputs are derived projections.
    pub input_basis: BTreeMap<String, String>,
}

fn placement_digest(plan: &IslandPlan, devices: &[DeviceDescriptor]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (stage, &m) in plan.members.iter().enumerate() {
        let layers = &plan.stages[stage].layers;
        h.update(
            format!(
                "member {} {} {}\n",
                devices[m].device_id, layers.start, layers.end
            )
            .as_bytes(),
        );
    }
    for s in &plan.spares {
        h.update(format!("spare {}\n", devices[s.device].device_id).as_bytes());
    }
    hex::encode(h.finalize())
}

fn mix_of(group: &[usize], inv: &Inventory, idx: &[usize]) -> String {
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for &m in group {
        *counts.entry(inv.class_of[idx[m]]).or_default() += 1;
    }
    let mut parts: Vec<(usize, usize)> = counts.into_iter().collect();
    parts.sort_by_key(|&(c, n)| {
        (
            Reverse(
                DEVICE_MIX[c].facts.memory_class_gb.unwrap_or(0) * 1000
                    + DEVICE_MIX[c].facts.gpu_vram_class_gb.unwrap_or(0),
            ),
            n,
        )
    });
    parts
        .iter()
        .map(|&(c, n)| format!("{n}× {}", DEVICE_MIX[c].name))
        .collect::<Vec<_>>()
        .join(" + ")
}

/// Recovery in the simulator: the synthetic ledger's digest is restored.
struct LedgerRecovery;

impl Recovery for LedgerRecovery {
    fn restore(&mut self, _: &IslandPlan, _: usize, checkpoint: &TrustedCheckpoint) -> String {
        checkpoint.digest.clone()
    }
}

const FAIL: u8 = 0;
const READY: u8 = 1;

/// Plays one lease of churn through the lifecycle: exponential failures;
/// a lost member promotes a spare, recovers from the (synthetic) ledger
/// checkpoint and re-qualifies; every lost or promoted spare is replaced from
/// the unused eligible devices, warm after staging its largest stage at
/// 1 Gb/s; admission is paused while spares are short.
fn churn(
    outcome: &FormationOutcome,
    devices: &[DeviceDescriptor],
    rtt: &mut SyntheticRtt,
    model: &ModelSpec,
    policy: &FormationPolicy,
    rng: &mut Rng,
) -> ChurnReport {
    let golden = ToyPipeline::golden(&model.identity, model.layers.len());
    let checkpoint = TrustedCheckpoint {
        committed_tokens: 0,
        digest: "synthetic-ledger-checkpoint".into(),
    };
    let lease_ms = (LEASE_H * 3_600_000.0) as u64;
    let mut r = ChurnReport {
        islands: outcome.islands.len(),
        lease_duration_ms: lease_ms,
        ..ChurnReport::default()
    };
    let mut live = devices.to_vec();
    let mut pool: Vec<usize> = outcome.unused.clone();
    let draw = |rng: &mut Rng, from: u64| {
        let t = from as f64 - MTBF_H * 3_600_000.0 * (1.0 - rng.unit()).ln();
        (t < lease_ms as f64).then_some(t as u64)
    };
    for plan in &outcome.islands {
        rtt.at_ms = 0;
        let mut island = Island::new(plan.clone(), devices, policy.freshness, 0);
        let _ = island.qualify(
            0,
            &mut ToyPipeline::default(),
            devices,
            rtt,
            GoldenSource::Simulation(&golden),
        );
        let mut heap: BinaryHeap<Reverse<(u64, u8, usize)>> = BinaryHeap::new();
        for d in island.devices() {
            if let Some(t) = draw(rng, 0) {
                heap.push(Reverse((t, FAIL, d)));
            }
        }
        let mut pending: BTreeSet<usize> = BTreeSet::new();
        let mut last = 0u64;
        while let Some(Reverse((t, kind, d))) = heap.pop() {
            r.account_interval(&island, t - last);
            last = t;
            if island.is_dissolved() {
                continue;
            }
            rtt.at_ms = t;
            match kind {
                FAIL if pending.remove(&d) => {}
                FAIL if island.devices().contains(&d) => {
                    if island.plan().members.contains(&d) {
                        r.member_failures += 1;
                    } else {
                        r.spare_failures += 1;
                    }
                    let before = island.generation();
                    island.device_lost(d, t);
                    if island.generation() > before {
                        r.promotions += 1;
                        let at = t + PROMOTION_STALL_MS;
                        island.recover(at, &checkpoint, &mut LedgerRecovery);
                        let _ = island.qualify(
                            at,
                            &mut ToyPipeline::default(),
                            devices,
                            rtt,
                            GoldenSource::Simulation(&golden),
                        );
                    }
                }
                READY if pending.remove(&d) => {
                    // Probes and facts are refreshed every epoch.
                    for &i in island.plan().members.iter().chain([&d]) {
                        live[i].evidence.measured_at_ms = t;
                    }
                    if island.replenish_spare(t, d, &live, rtt).is_ok() {
                        r.replenished += 1;
                    }
                }
                _ => {}
            }
            if island.is_dissolved() {
                continue;
            }
            // Reserve replacements for any shortfall.
            let largest = island.plan().largest_stage_bytes();
            while island.spare_shortfall() > pending.len() {
                let found = pool.iter().position(|&c| {
                    devices[c].pool().is_some_and(|p| p.usable_bytes >= largest)
                        && island.plan().members.iter().all(|&m| {
                            island
                                .plan()
                                .link_rule
                                .ok(rtt.link(c, m), t, &policy.freshness, true)
                        })
                });
                let Some(k) = found else { break };
                let c = pool.remove(k);
                pending.insert(c);
                let staging = (largest as f64 * 8.0 / STAGING_BITS_PER_S * 1000.0) as u64;
                if t + staging < lease_ms {
                    heap.push(Reverse((t + staging, READY, c)));
                }
                if let Some(f) = draw(rng, t) {
                    heap.push(Reverse((f, FAIL, c)));
                }
            }
        }
        r.account_interval(&island, lease_ms - last);
        if island.is_dissolved() {
            r.dissolved += 1;
        } else {
            r.survived += 1;
        }
    }
    r.finish_accounting();
    r
}

fn reject_label(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::NoConsent => "no island consent",
        RejectReason::NotQualified => "not golden-qualified",
        RejectReason::NoMemoryFacts => "no memory facts",
        RejectReason::StaleEvidence => "stale evidence",
        RejectReason::UnmeasuredInputs => "unmeasured inputs",
        RejectReason::LowAvailability => "low availability",
        RejectReason::TooSmallForOneLayer => "too small for one layer",
    }
}

/// Runs one scenario.
pub fn run(scenario: &Scenario, model: &ModelSpec) -> ScenarioReport {
    let mut inv = Inventory::synthetic(
        scenario.nodes,
        scenario.seed,
        scenario.opt_in_permille,
        scenario.availability_permille,
    );
    if scenario.inventory != InventoryProfile::ResearchMix {
        for (i, d) in inv.devices.iter_mut().enumerate() {
            // GPU presence is welcome but not required. RAM and VRAM are never
            // summed, and no unmeasured GPU speedup is credited.
            let class = if i % 4 == 0 { 15 } else { 14 };
            inv.class_of[i] = class;
            d.facts = DEVICE_MIX[class].facts;
            d.measured.pool_kind = Some(PoolKind::Cpu);
            d.site = None;
            d.rdma = None;
            if let InventoryProfile::OrdinaryRegional {
                nodes_per_region, ..
            } = scenario.inventory
            {
                let region = format!("synthetic-region-{}", i / nodes_per_region.max(1));
                d.location = Location {
                    continent: "synthetic".into(),
                    region: region.clone(),
                    zone: region.clone(),
                    metro: region,
                };
            }
        }
    }
    let (devices, idx, mut rtt) = inv.online_snapshot();
    if let InventoryProfile::OrdinaryRegional { rtt_us, .. } = scenario.inventory {
        rtt.fixed_regional_rtt_us = Some(rtt_us);
    }
    let policy = policy_for(scenario);
    let outcome = form(&devices, &rtt, model, &policy, 0);
    let golden = ToyPipeline::golden(&model.identity, model.layers.len());

    let mut rejected: BTreeMap<String, usize> = BTreeMap::new();
    for r in &outcome.rejected {
        *rejected.entry(reject_label(r.reason).into()).or_default() += 1;
    }
    let mut islands_by_tier: BTreeMap<String, usize> = BTreeMap::new();
    let mut rows = Vec::new();
    for plan in &outcome.islands {
        *islands_by_tier.entry(plan.tier.label().into()).or_default() += 1;
        let mut island = Island::new(plan.clone(), &devices, policy.freshness, 0);
        let state = island
            .qualify(
                0,
                &mut ToyPipeline::default(),
                &devices,
                &rtt,
                GoldenSource::Simulation(&golden),
            )
            .unwrap_or_else(|_| island.state().clone());
        let p = match &scenario.batching {
            BatchingPolicy::Adaptive { floor_tok_s } => project(
                plan,
                &devices,
                model,
                &rtt,
                &scenario.net,
                CONTEXT_POSITIONS,
                *floor_tok_s,
            ),
            BatchingPolicy::Fixed {
                plain, speculative, ..
            } => project_fixed(
                plan,
                &devices,
                model,
                &rtt,
                &scenario.net,
                CONTEXT_POSITIONS,
                *plain,
                *speculative,
            ),
        };
        let hops = plan.hops();
        let spares: Vec<usize> = plan.spares.iter().map(|s| s.device).collect();
        rows.push(IslandRow {
            tier: plan.tier,
            cell: plan.cell.clone(),
            members: plan.members.len(),
            spares: plan.spares.len(),
            required_spares: plan.required_spares,
            hops,
            ring_p50_ms: plan.ring_p50_us as f64 / 1000.0,
            mean_hop_rtt_ms: if hops == 0 {
                0.0
            } else {
                plan.ring_p50_us as f64 / 1000.0 / hops as f64
            },
            max_pair_p95_ms: f64::from(plan.max_pair_p95_us) / 1000.0,
            mix: mix_of(&plan.members, &inv, &idx),
            spare_mix: mix_of(&spares, &inv, &idx),
            placement_digest: placement_digest(plan, &devices),
            state,
            tokens_per_day: p.batch.aggregate_tok_s * 86_400.0,
            tokens_per_day_speculative: p.batch_speculative.aggregate_tok_s * 86_400.0,
            projection: p,
        });
    }
    let opted_in_online = devices.iter().filter(|d| d.consent.permits(d, 0)).count();
    let rejected_set: BTreeSet<usize> = outcome.rejected.iter().map(|r| r.device).collect();
    let mut region_bytes: BTreeMap<String, u64> = BTreeMap::new();
    for (i, d) in devices.iter().enumerate() {
        if let (false, Some(p)) = (rejected_set.contains(&i), d.pool()) {
            *region_bytes
                .entry(format!("{}/{}", d.location.continent, d.location.region))
                .or_default() += p.usable_bytes;
        }
    }
    let largest_region = region_bytes
        .iter()
        .max_by_key(|&(k, v)| (*v, Reverse(k.clone())))
        .map_or((String::new(), 0.0), |(k, v)| (k.clone(), *v as f64 / 1e9));
    let need = model.weight_bytes() + model.kv_bytes_per_position() * policy.kv_positions;
    let needed_gb = (need + need * policy.capacity_headroom_permille / 1000) as f64 / 1e9;
    let mut rng = Rng::new(scenario.seed ^ 0xC0FFEE);
    let sum = |f: &dyn Fn(&IslandRow) -> f64| rows.iter().map(f).sum::<f64>() + 0.0;
    let churn = churn(&outcome, &devices, &mut rtt, model, &policy, &mut rng);
    let available_ms = churn
        .other_time_ms
        .saturating_sub((churn.promotions as u64).saturating_mul(PROMOTION_STALL_MS));
    let lease_available = if churn.total_island_time_ms == 0 {
        0.0
    } else {
        available_ms as f64 / churn.total_island_time_ms as f64
    };
    let members = outcome.islands.iter().map(|p| p.members.len()).sum();
    let spares = outcome.islands.iter().map(|p| p.spares.len()).sum();
    let cost = |speculative: bool| {
        let mut tokens = 0.0;
        let mut egress = 0.0;
        for row in &rows {
            let b = if speculative {
                row.projection.batch_speculative
            } else {
                row.projection.batch
            };
            let daily = b.aggregate_tok_s * 86_400.0;
            tokens += daily;
            // A feedback token is counted per verified pass, alongside all
            // activation boundaries. No compression credit without ENG-9 data.
            let bytes = model.boundary_bytes_per_position as f64
                * row.members.saturating_sub(1) as f64
                * f64::from(b.draft_tokens + 1)
                + if row.members > 1 { 8.0 } else { 0.0 };
            egress +=
                daily * bytes / crate::perf::tau(b.draft_tokens, scenario.net.draft_acceptance);
        }
        project_cost(
            scenario.costs,
            members,
            spares,
            rows.len(),
            tokens,
            egress,
            lease_available,
        )
        .expect("finite simulator inputs")
    };
    let cost_plain = cost(false);
    let cost_speculative = cost(true);
    ScenarioReport {
        cost_plain,
        cost_speculative,
        input_basis: assumptions().into_iter().map(|(key, value, basis)|
            (key.into(), format!("ASSUMED input: {value}; provenance: {basis}"))).chain([
                ("Scenario".into(), "ASSUMED node count (130 is TJ's approximate fleet size, not a measured inventory), seed, profile, opt-in, availability, service, transport, KV and spare policy; exact values in scenario".into()),
                ("Costs".into(), "ASSUMED rates and utilisation: exact values in scenario.costs; no operator prices measured".into()),
            ]).collect(),
        online: devices.len(),
        opted_in_online,
        eligible_online: devices.len() - outcome.rejected.len(),
        rejected,
        islands_by_tier,
        serving_islands: rows.iter().filter(|r| r.state == State::Serving).count(),
        simulated_islands: rows.iter().filter(|r| r.state == State::Simulated).count(),
        machines_serving: outcome.islands.iter().map(|p| p.members.len()).sum(),
        spares: outcome.islands.iter().map(|p| p.spares.len()).sum(),
        unused_eligible: outcome.unused.len(),
        needed_gb,
        largest_region,
        aggregate_tok_s: sum(&|r| r.projection.batch.aggregate_tok_s),
        aggregate_tok_s_speculative: sum(&|r| r.projection.batch_speculative.aggregate_tok_s),
        tokens_per_day: sum(&|r| r.tokens_per_day),
        tokens_per_day_speculative: sum(&|r| r.tokens_per_day_speculative),
        churn,
        rows,
        scenario: scenario.clone(),
    }
}

/// The scenarios the report covers: for each node count, batch and
/// interactive service at a = 0.7, pessimistic and optimized transport;
/// a = 0.5 for both services; and batch without the spare requirement, for
/// comparison only. Opt-in 90% throughout.
pub fn standard_scenarios(seed: u64, node_counts: &[usize]) -> Vec<Scenario> {
    let p = || ("pessimistic", NetAssumptions::pessimistic());
    let o = || ("optimized", NetAssumptions::optimized());
    let mut out = Vec::new();
    for &nodes in node_counts {
        for (service, (transport, net), availability, require_spares) in [
            (Service::Batch, p(), 700, true),
            (Service::Batch, o(), 700, true),
            (Service::Interactive, p(), 700, true),
            (Service::Interactive, o(), 700, true),
            (Service::Batch, p(), 500, true),
            (Service::Interactive, o(), 500, true),
            (Service::Batch, p(), 700, false),
        ] {
            out.push(Scenario {
                inventory: InventoryProfile::ResearchMix,
                batching: BatchingPolicy::Adaptive {
                    floor_tok_s: BATCH_FLOOR_TOK_S,
                },
                costs: CostAssumptions::default(),
                kv_sequences: kv_sequences(service),
                nodes,
                seed,
                opt_in_permille: OPT_IN_PERMILLE,
                availability_permille: availability,
                service,
                transport: transport.into(),
                net,
                require_spares,
            });
        }
    }
    out
}

/// Default scenarios for TJ's ordinary-node regional deployment target.
/// Concentrated profiles are sensitivity cases, not a claim that today's
/// ~130 nodes share a region. The dispersed case shows that uncertainty.
pub fn regional_scenarios(seed: u64, node_counts: &[usize]) -> Vec<Scenario> {
    let mut out = Vec::new();
    for &nodes in node_counts {
        for rtt_us in [5_000, 10_000, 20_000] {
            for optimized in [false, true] {
                let mut net = if optimized {
                    NetAssumptions::optimized()
                } else {
                    NetAssumptions::pessimistic()
                };
                // Parameter sweep for ENG-8 integration, NOT an implemented
                // token-tree verifier or an asserted acceptance measurement.
                net.max_draft_tokens = 64;
                net.draft_acceptance = 0.9;
                out.push(Scenario {
                    inventory: InventoryProfile::OrdinaryRegional {
                        rtt_us,
                        nodes_per_region: 130,
                    },
                    batching: BatchingPolicy::Adaptive {
                        floor_tok_s: BATCH_FLOOR_TOK_S,
                    },
                    costs: CostAssumptions::default(),
                    kv_sequences: 128,
                    nodes,
                    seed,
                    opt_in_permille: OPT_IN_PERMILLE,
                    availability_permille: 700,
                    service: Service::Batch,
                    transport: if optimized { "500-Mbps" } else { "50-Mbps" }.into(),
                    net,
                    require_spares: true,
                });
            }
        }
        let mut sensitivity = out.last().expect("six scenarios").clone();
        sensitivity.availability_permille = 500;
        out.push(sensitivity);
        let mut dispersed = out.last().expect("scenario").clone();
        dispersed.inventory = InventoryProfile::OrdinaryGlobal;
        dispersed.availability_permille = 700;
        out.push(dispersed);
    }
    out
}

/// RTT of the reference scenario whose adaptive optimum fixes the depths.
pub const REFERENCE_RTT_US: u32 = 5_000;

/// The modal (most common) plain and speculative depth across a report's
/// swarms; ties go to the smaller concurrency, then the shallower draft.
fn modal_depths(report: &ScenarioReport) -> Option<(FixedDepth, FixedDepth)> {
    let modal = |pick: &dyn Fn(&IslandRow) -> FixedDepth| {
        let mut counts: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for row in &report.rows {
            let d = pick(row);
            *counts.entry((d.concurrent, d.draft_tokens)).or_default() += 1;
        }
        counts
            .into_iter()
            .max_by_key(|&((b, k), n)| (n, Reverse(b), Reverse(k)))
            .map(|((concurrent, draft_tokens), _)| FixedDepth {
                concurrent,
                draft_tokens,
            })
    };
    let depth = |b: crate::perf::BatchPlan| FixedDepth {
        concurrent: b.concurrent,
        draft_tokens: b.draft_tokens,
    };
    Some((
        modal(&|r| depth(r.projection.batch))?,
        modal(&|r| depth(r.projection.batch_speculative))?,
    ))
}

/// The default ordinary-node scenarios as a **controlled** comparison.
///
/// Within each matched group (same node count and uplink), the batch and
/// draft depths are fixed once: the adaptive policy's choice in the group's
/// reference scenario (RTT 5 ms, availability 0.7), taken as the modal value
/// across its swarms. Those depths are then applied unchanged to RTT 5, 10
/// and 20 ms and to the group's availability and dispersed sensitivities.
/// Inventory, placement, compute, demand, downtime and cost inputs are the
/// same within a group; only the RTT (or the named sensitivity) differs. A
/// group whose reference forms no swarm has nothing to compare; it gets
/// depth 1 / draft 0, which affects no row.
pub fn controlled_regional_scenarios(
    seed: u64,
    node_counts: &[usize],
    model: &ModelSpec,
) -> Vec<Scenario> {
    let adaptive = regional_scenarios(seed, node_counts);
    let mut depths: BTreeMap<(usize, String), BatchingPolicy> = BTreeMap::new();
    for s in &adaptive {
        let reference = matches!(
            s.inventory,
            InventoryProfile::OrdinaryRegional {
                rtt_us: REFERENCE_RTT_US,
                ..
            }
        ) && s.availability_permille == 700;
        if !reference {
            continue;
        }
        let report = run(s, model);
        let (plain, speculative, chosen_from) = match modal_depths(&report) {
            Some((p, sp)) => (
                p,
                sp,
                format!(
                    "adaptive optimum at RTT {} ms, a = 0.7, same nodes and uplink (modal over {} swarm(s)); held fixed for every RTT and sensitivity in the group",
                    REFERENCE_RTT_US / 1000,
                    report.rows.len()
                ),
            ),
            None => {
                let none = FixedDepth {
                    concurrent: 1,
                    draft_tokens: 0,
                };
                (
                    none,
                    none,
                    "no swarm forms in the reference scenario; the depth affects no row".into(),
                )
            }
        };
        depths.insert(
            (s.nodes, s.transport.clone()),
            BatchingPolicy::Fixed {
                plain,
                speculative,
                chosen_from,
            },
        );
    }
    adaptive
        .into_iter()
        .map(|mut s| {
            s.batching = depths[&(s.nodes, s.transport.clone())].clone();
            s
        })
        .collect()
}

/// The adaptive-policy RTT rows, reported separately from the controlled
/// comparison.
pub fn adaptive_rtt_scenarios(seed: u64, node_counts: &[usize]) -> Vec<Scenario> {
    regional_scenarios(seed, node_counts)
        .into_iter()
        .filter(|s| {
            matches!(s.inventory, InventoryProfile::OrdinaryRegional { .. })
                && s.availability_permille == 700
        })
        .collect()
}

/// The checks on one matched group of the controlled RTT comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RttCheck {
    pub nodes: usize,
    pub transport: String,
    pub rtts_ms: Vec<u32>,
    pub plain: FixedDepth,
    pub speculative: FixedDepth,
    /// Every row ran the group's fixed depths.
    pub same_depths: bool,
    /// Identical swarm placement (members, stages, spares) at every RTT.
    pub same_placement: bool,
    /// Identical simulated lease availability at every RTT.
    pub same_lease: bool,
    /// Aggregate tok/s, plain and speculative, never rises with RTT.
    pub aggregate_non_increasing: bool,
    /// USD per million tokens, plain and speculative, never falls with RTT
    /// (zero output counts as unbounded cost).
    pub cost_non_decreasing: bool,
    /// No swarm forms at any RTT (the comparison is vacuous).
    pub zero_output: bool,
}

impl RttCheck {
    pub fn holds(&self) -> bool {
        self.same_depths
            && self.same_placement
            && self.same_lease
            && self.aggregate_non_increasing
            && self.cost_non_decreasing
    }
}

/// Groups the controlled regional rows (a = 0.7) by node count and uplink,
/// orders them by RTT, and checks the comparison is controlled and monotone.
pub fn controlled_rtt_checks(reports: &[ScenarioReport]) -> Vec<RttCheck> {
    let mut groups: BTreeMap<(usize, String), Vec<(u32, &ScenarioReport)>> = BTreeMap::new();
    for r in reports {
        if let (InventoryProfile::OrdinaryRegional { rtt_us, .. }, BatchingPolicy::Fixed { .. }) =
            (&r.scenario.inventory, &r.scenario.batching)
            && r.scenario.availability_permille == 700
        {
            groups
                .entry((r.scenario.nodes, r.scenario.transport.clone()))
                .or_default()
                .push((*rtt_us, r));
        }
    }
    let usd = |x: Option<f64>| x.unwrap_or(f64::INFINITY);
    groups
        .into_iter()
        .filter(|(_, rows)| rows.len() > 1)
        .map(|((nodes, transport), mut rows)| {
            rows.sort_by_key(|(rtt, _)| *rtt);
            let BatchingPolicy::Fixed {
                plain, speculative, ..
            } = rows[0].1.scenario.batching.clone()
            else {
                unreachable!("filtered to fixed rows")
            };
            let placement = |r: &ScenarioReport| {
                r.rows
                    .iter()
                    .map(|x| x.placement_digest.clone())
                    .collect::<Vec<_>>()
            };
            let same_depths = rows.iter().all(|(_, r)| {
                r.scenario.batching == rows[0].1.scenario.batching
                    && r.rows.iter().all(|x| {
                        x.projection.batch.draft_tokens == 0
                            && x.projection.batch_speculative.draft_tokens
                                == speculative.draft_tokens
                            && x.projection.speculative.draft_tokens == speculative.draft_tokens
                    })
            });
            let pairs: Vec<(&ScenarioReport, &ScenarioReport)> =
                rows.windows(2).map(|w| (w[0].1, w[1].1)).collect();
            RttCheck {
                nodes,
                transport,
                rtts_ms: rows.iter().map(|(rtt, _)| rtt / 1000).collect(),
                plain,
                speculative,
                same_depths,
                same_placement: rows
                    .iter()
                    .all(|(_, r)| placement(r) == placement(rows[0].1)),
                // The lease fraction is derived from the churn record.
                same_lease: rows.iter().all(|(_, r)| r.churn == rows[0].1.churn),
                aggregate_non_increasing: pairs.iter().all(|(a, b)| {
                    b.aggregate_tok_s <= a.aggregate_tok_s
                        && b.aggregate_tok_s_speculative <= a.aggregate_tok_s_speculative
                }),
                cost_non_decreasing: pairs.iter().all(|(a, b)| {
                    usd(b.cost_plain.usd_per_million_tokens)
                        >= usd(a.cost_plain.usd_per_million_tokens)
                        && usd(b.cost_speculative.usd_per_million_tokens)
                            >= usd(a.cost_speculative.usd_per_million_tokens)
                }),
                zero_output: rows.iter().all(|(_, r)| r.rows.is_empty()),
            }
        })
        .collect()
}

/// Every labelled input, for the report.
pub fn assumptions() -> Vec<(&'static str, String, &'static str)> {
    let p = NetAssumptions::pessimistic();
    let o = NetAssumptions::optimized();
    vec![
        ("Cost inputs", format!("{:?}; USD; all rates assumed, not operator quotes. Costs charge all reserved members/spares for a full day; output discounted by demand utilisation and equal-weight simulated lease availability. Egress includes activation boundaries for every verification position and token feedback, no compression credit. Shard staging/checkpoint traffic, separate drafter resources, unallocated hosts, taxes, rewards and profit margins excluded.", CostAssumptions::default()), "ASSUMED"),
        ("Cost downtime approximation", "Lease available fraction = max(0, other island-time minus promotions times 10 seconds) / total island-time; conservative promotion-stall subtraction may overlap shortfall time. Uses equal island-time weights, not throughput-weighted per-swarm downtime. Costs are planning estimates, not a measured TCO.".into(), "ASSUMED accounting model"),
        ("Model", "Kimi K2.6, INT4 g32 routed experts + INT8 elsewhere: 582.6 GB weights, 140,544 B KV per position, 56 KiB Q16 boundary per position, no MTP head".into(), "CALC from docs/protocol/kimi-k26-checkpoint.md (#156)"),
        ("Evidence", "every synthetic device, link and RDMA result is marked synthetic; formation allows it, so every island is a simulated island and none can serve; no Kimi golden is pinned".into(), "by construction"),
        ("Inventory geography", "Default regional-density sensitivity: consecutive groups of up to 130 registered machines in one synthetic region at p50 RTT 5, 10 or 20 ms; no inter-region links assumed. The dispersed sensitivity and legacy comparison use research-7 §4.1 shares (US/EU-skewed), Asia ×1.25/×1.55 and Africa ×1.2/×1.6 at 1,000/10,000; metros inside an area weighted by a fixed synthetic table".into(), "UNVERIFIED planning input (research-7) + ASSUMPTION"),
        ("Device mix", "Default: all nodes have 16 GB system RAM; 25% also have an 8 GB consumer GPU. Placement uses the CPU memory pool, not RAM+VRAM, and credits no GPU speedup. Legacy comparison only: 8 GB GPU 10%, 12 GB 15%, 16 GB 15%, 24 GB 20%, 32 GB 8%, Mac 16–24 GB 10%, Mac 36–64 GB 12%, 128 GB Mac/Strix 6%, 256–512 GB Ultra 1%, CPU-only 3%; even splits inside each class".into(), "UNVERIFIED planning input (research-7 §4.1)"),
        ("Availability", "each node online at formation with probability 0.7 (base) or 0.5 (sensitivity)".into(), "UNVERIFIED (research-7 §4.1)"),
        ("Consent", format!("{}% of owners grant both answers for their own device (no expiry inside the run); the rest answered the compute question only", OPT_IN_PERMILLE / 10), "ASSUMPTION; consent model from #138"),
        ("Golden qualification", "every synthetic device passes its own Proof Kit self-test".into(), "ASSUMPTION"),
        ("Usable memory", "85% of GPU memory, 80% of unified/system memory".into(), "research-6 §2.2"),
        ("Bandwidth", "llama.cpp-class effective GB/s by class: GPU 8/12/16/24/32 GB = 250/300/450/680/1,108; Mac ≤24 GB 90, 36–192 GB 300, ≥256 GB 456; CPU 60. ARC's engine is not yet measured at these rates".into(), "CALC (research-6 §2.5, research-7 §2.6), UNVERIFIED for ARC"),
        ("Spare policy", "1 / 2 / 3 warm spares for 2–6 / 7–22 / 23+ stages; every spare holds the largest stage and meets the link rule with every member; the comparison rows drop the requirement".into(), "research-6 §6.4"),
        ("LAN sites", "Default ordinary-node profiles have no LAN sites or RDMA. Legacy comparison: 3% / 5% / 8% of nodes at <1,000 / 1,000 / 10,000 on sites of 3 devices of one owner; Thunderbolt 5 Macs there carry a synthetic RDMA result (collective p99 0.15 ms)".into(), "UNVERIFIED (research-7 §4.1) + ASSUMPTION"),
        ("Home RTT", "Regional-density cases use fixed synthetic p50 5/10/20 ms, p95 1.25 times p50, p99 1.5 times p50; no packet loss. Dispersed/legacy cases: access leg to the metro hub drawn from RIPE Atlas 0–100 km quantiles (p25 3.7, p50 5.5, p75 9.8, p90 22.1 ms); pair RTT = both legs + core (metro −1, zone +11, region +27, other region of the continent +45, world +181 ms); p95 = 1.25 × p50, p99 = 1.5 × p50, no loss".into(), "MEASURED quantiles (research-7 §1.6) + CALC + ASSUMPTION for jitter"),
        ("Search levels", "Default regional policy: up to 128 stages, no T0/LAN prerequisite. Geo hints at p95 20/35/60/75 ms, then direct-RTT fallback at 60 ms. Legacy comparison alone retains interactive caps 4/3/2/2 and batch 30".into(), "research-7 §1.7, §6.2; 75 ms from the wide-region p75 (research-7 §1.6)"),
        ("Neighbour table", NEIGHBOURS.iter().map(|(a, b)| format!("{a}–{b}")).collect::<Vec<_>>().join(", "), "ASSUMPTION"),
        ("LAN RTT", "0.3 ms p50 on one site, 0.04 ms when both machines have Thunderbolt 5".into(), "ASSUMPTION (research-6 §1.2: RDMA < 50 µs)"),
        ("Pessimistic transport", format!("{} ms per hop, {} Mb/s home uplink, {} ms fixed per pass", f64::from(p.wan_hop_overhead_us) / 1000.0, p.wan_uplink_mbps, f64::from(p.fixed_pass_us) / 1000.0), "research-7 §2.1 defaults"),
        ("Optimized transport", format!("{} ms per hop, {} Mb/s uplink", f64::from(o.wan_hop_overhead_us) / 1000.0, o.wan_uplink_mbps), "ASSUMPTION (fibre homes, tuned streaming transport)"),
        ("Tensor parallel", format!("{} ms per collective on RDMA, 122 per token", f64::from(p.collective_us) / 1000.0), "research-6 §2.5"),
        ("Speculation", format!("Ordinary-node rows: alpha 0.9 ASSUMED; controlled rows use the group's fixed draft depth (shown per group, the 5 ms adaptive optimum), adaptive rows search depth 0–64; exact settings in scenario.net and scenario.batching. No token-tree execution implemented (ENG-8). Legacy comparison: chain drafts from a separate drafter (K2.6 has no MTP head), acceptance α = {}, {} ms per draft token, depth 0–{} chosen per island, separately for a single answer and under batching", p.draft_acceptance, f64::from(p.draft_us_per_token) / 1000.0, p.max_draft_tokens), "ASSUMPTION (research-7 §2.2 planning values)"),
        ("Batching", format!("Default ordinary-node rows are a controlled comparison: KV for 128 sequences × {CONTEXT_POSITIONS} positions per swarm, and within each matched group (same node count and uplink) one fixed plain batch depth and one fixed speculative batch/draft depth, applied unchanged at RTT 5, 10 and 20 ms and in the group's availability and dispersed sensitivities. The fixed depths are the adaptive optimum of the group's RTT {} ms, a = 0.7 scenario (modal across its swarms) and are printed per group. Adaptive-policy rows are reported separately: there each swarm searches batch 1, 2, 4, … 128 and draft 0–64 for the largest aggregate keeping per-stream speed ≥ min({BATCH_FLOOR_TOK_S} tok/s, half its own single-answer rate); that floor moves with RTT, so adaptive rows must not be compared across RTTs. Legacy comparison: adaptive, KV for {} (interactive) / {} (batch) sequences", REFERENCE_RTT_US / 1000, kv_sequences(Service::Interactive), kv_sequences(Service::Batch)), "research-6 §2.6 model; depth choice ASSUMED"),
        ("Compute model", "memory-bandwidth bound; distinct experts under uniform routing; FLOPs, prefill and queueing not modelled".into(), "CALC (research-6 §2.6)"),
        ("Tokens/day", "aggregate tok/s × 86,400: a fully loaded ceiling, excluding spare-shortfall and dissolved downtime, recovery stalls, prefill and audit; not a demand forecast".into(), "CALC"),
        ("Churn", format!("device MTBF {MTBF_H} h, lease {LEASE_H} h, exponential failures; a member loss promotes a spare, recovers from the ledger checkpoint and re-qualifies ({} s stall); lost spares are replaced from unused eligible devices after staging the largest stage at 1 Gb/s; spare-shortfall time counts only non-dissolved islands below policy, including time without an eligible replacement; dissolved time counts separately from dissolution to lease end", PROMOTION_STALL_MS / 1000), "research-6 §3.3 (Salad 92 h), §6.7"),
    ]
}

fn fmt_rate(x: f64) -> String {
    if x >= 100.0 {
        format!("{x:.0}")
    } else {
        format!("{x:.1}")
    }
}

fn fmt_big(x: f64) -> String {
    if x < 0.5 {
        "0".into()
    } else if x >= 1e9 {
        format!("{:.2} B", x / 1e9)
    } else if x >= 1e6 {
        format!("{:.1} M", x / 1e6)
    } else if x >= 1e5 {
        format!("{:.0} k", x / 1e3)
    } else if x >= 1e3 {
        format!("{:.1} k", x / 1e3)
    } else {
        format!("{x:.0}")
    }
}

fn spread(values: &mut [f64]) -> String {
    if values.is_empty() {
        return "–".into();
    }
    values.sort_by(f64::total_cmp);
    let med = values[values.len() / 2];
    format!(
        "{} ({}–{})",
        fmt_rate(med),
        fmt_rate(values[0]),
        fmt_rate(values[values.len() - 1])
    )
}

fn scenario_label(s: &Scenario) -> String {
    format!(
        "{} · {} · {} · a = {} · opt-in {}%{}",
        match s.inventory {
            InventoryProfile::ResearchMix => "legacy research mix".into(),
            InventoryProfile::OrdinaryGlobal => "16 GB, dispersed".into(),
            InventoryProfile::OrdinaryRegional {
                rtt_us,
                nodes_per_region,
            } => format!("16 GB, {nodes_per_region}/region, RTT {} ms", rtt_us / 1000),
        },
        match s.service {
            Service::Batch => "batch",
            Service::Interactive => "interactive",
        },
        s.transport,
        f64::from(s.availability_permille) / 1000.0,
        s.opt_in_permille / 10,
        if s.require_spares {
            ""
        } else {
            " · **spares not required (comparison)**"
        }
    ) + &match &s.batching {
        BatchingPolicy::Adaptive { .. } => " · adaptive batching".to_string(),
        BatchingPolicy::Fixed {
            plain, speculative, ..
        } => format!(
            " · fixed B {} / spec B {} k {}",
            plain.concurrent, speculative.concurrent, speculative.draft_tokens
        ),
    }
}

fn usd_cell(x: Option<f64>) -> String {
    x.map_or_else(|| "undefined (zero output)".into(), |v| format!("{v:.2}"))
}

/// The controlled RTT comparison and, separately, the adaptive-policy rows.
fn controlled_section(md: &mut String, reports: &[ScenarioReport]) {
    let checks = controlled_rtt_checks(reports);
    if !checks.is_empty() {
        controlled_tables(md, reports, &checks);
    }
    adaptive_table(md, reports);
}

fn controlled_tables(md: &mut String, reports: &[ScenarioReport], checks: &[RttCheck]) {
    let _ = writeln!(
        md,
        "#### Controlled RTT comparison (default)\n\nWithin each group (same node count and uplink) every row has the same inventory, placement, \
         compute, demand, downtime and cost inputs **and the same batch and draft depth**; only the RTT differs. \
         The fixed depths are the adaptive optimum of the group's RTT {} ms, a = 0.7 scenario (modal across its \
         swarms), applied unchanged at every RTT. Plain batches never speculate. 130-node rows are conditional \
         placement (130 registered nodes in one region), not today's unmeasured fleet.\n\n\
         Single-answer rates use B = 1; per-answer rates at fixed batch use the stated plain/speculative B and k, \
         and these fixed-batch rates underlie aggregate output, daily tokens and cost per million tokens. \
         Both rate columns show the median (min–max) across formed swarms.\n",
        REFERENCE_RTT_US / 1000
    );
    let _ = writeln!(
        md,
        "| Nodes | Uplink | RTT ms | Fixed plain B | Fixed spec B / k | Swarms | Lease available | Single-answer tok/s (B = 1), plain / spec (k) | Per-answer tok/s at fixed batch, plain / spec | Aggregate tok/s, plain / spec | Tokens/day ceiling, spec | Projected tokens/day, spec | USD / million tokens, plain / spec |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for c in checks {
        let mut rows: Vec<(u32, &ScenarioReport)> = reports
            .iter()
            .filter_map(|r| match (&r.scenario.inventory, &r.scenario.batching) {
                (
                    InventoryProfile::OrdinaryRegional { rtt_us, .. },
                    BatchingPolicy::Fixed { .. },
                ) if r.scenario.nodes == c.nodes
                    && r.scenario.transport == c.transport
                    && r.scenario.availability_permille == 700 =>
                {
                    Some((*rtt_us, r))
                }
                _ => None,
            })
            .collect();
        rows.sort_by_key(|(rtt, _)| *rtt);
        for (rtt, r) in rows {
            let mut single: Vec<f64> = r.rows.iter().map(|x| x.projection.single.tok_s).collect();
            let mut spec: Vec<f64> = r
                .rows
                .iter()
                .map(|x| x.projection.speculative.tok_s)
                .collect();
            let mut batch: Vec<f64> = r
                .rows
                .iter()
                .map(|x| x.projection.batch.per_stream_tok_s)
                .collect();
            let mut batch_spec: Vec<f64> = r
                .rows
                .iter()
                .map(|x| x.projection.batch_speculative.per_stream_tok_s)
                .collect();
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {} / {} | {} | {} | {} / {} ({}) | {} / {} | {} / {} | {} | {} | {} / {} |",
                c.nodes,
                c.transport,
                rtt / 1000,
                c.plain.concurrent,
                c.speculative.concurrent,
                c.speculative.draft_tokens,
                r.rows.len(),
                if r.rows.is_empty() {
                    "–".into()
                } else {
                    format!("{:.4}", r.cost_plain.lease_available_fraction)
                },
                spread(&mut single),
                spread(&mut spec),
                c.speculative.draft_tokens,
                spread(&mut batch),
                spread(&mut batch_spec),
                fmt_big(r.aggregate_tok_s),
                fmt_big(r.aggregate_tok_s_speculative),
                fmt_big(r.tokens_per_day_speculative),
                fmt_big(r.cost_speculative.projected_tokens_per_day),
                usd_cell(r.cost_plain.usd_per_million_tokens),
                usd_cell(r.cost_speculative.usd_per_million_tokens),
            );
        }
    }
    let mark = |ok: bool| if ok { "yes" } else { "**NO**" };
    let _ = writeln!(
        md,
        "\nChecks per group, computed from the rows above (the simulator exits with an error if any fails):\n\n| Nodes | Uplink | RTTs ms | Same depths | Same placement | Same lease | Aggregate never rises with RTT | USD/M never falls with RTT | Zero output |\n|---|---|---|---|---|---|---|---|---|"
    );
    for c in checks {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            c.nodes,
            c.transport,
            c.rtts_ms
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" / "),
            mark(c.same_depths),
            mark(c.same_placement),
            mark(c.same_lease),
            mark(c.aggregate_non_increasing),
            mark(c.cost_non_decreasing),
            if c.zero_output {
                "yes (comparison vacuous)"
            } else {
                "no"
            },
        );
    }
    let _ = writeln!(md);
}

fn adaptive_table(md: &mut String, reports: &[ScenarioReport]) {
    let mut adaptive: Vec<(usize, String, u32, &ScenarioReport)> = reports
        .iter()
        .filter_map(|r| match (&r.scenario.batching, &r.scenario.inventory) {
            (
                BatchingPolicy::Adaptive { .. },
                InventoryProfile::OrdinaryRegional { rtt_us, .. },
            ) => Some((r.scenario.nodes, r.scenario.transport.clone(), *rtt_us, r)),
            _ => None,
        })
        .collect();
    adaptive.sort_by(|a, b| (a.0, &a.1, a.2).cmp(&(b.0, &b.1, b.2)));
    if adaptive.is_empty() {
        return;
    }
    let _ = writeln!(
        md,
        "#### Adaptive batching policy (reported separately; not a controlled comparison)\n\nHere each swarm searches its own batch and draft depth, keeping per-stream speed at least min({BATCH_FLOOR_TOK_S} tok/s, half its own single-answer rate). A higher RTT lowers the single-answer rate, which lowers that floor and admits more concurrent streams, so the workload changes with RTT. These rows show what the search would pick; they must not be read as the effect of latency on throughput or cost.\n"
    );
    let _ = writeln!(
        md,
        "| Nodes | Uplink | RTT ms | Selected plain B (modal) | Selected spec B / k (modal) | Swarms | Aggregate tok/s, plain / spec | USD / million tokens, plain / spec |\n|---|---|---|---|---|---|---|---|"
    );
    for (_, _, rtt_us, r) in adaptive {
        let (p, sp) = modal_depths(r).map_or(("–".into(), "–".into()), |(p, sp)| {
            (
                p.concurrent.to_string(),
                format!("{} / {}", sp.concurrent, sp.draft_tokens),
            )
        });
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} / {} | {} / {} |",
            r.scenario.nodes,
            r.scenario.transport,
            rtt_us / 1000,
            p,
            sp,
            r.rows.len(),
            fmt_big(r.aggregate_tok_s),
            fmt_big(r.aggregate_tok_s_speculative),
            usd_cell(r.cost_plain.usd_per_million_tokens),
            usd_cell(r.cost_speculative.usd_per_million_tokens),
        );
    }
    let _ = writeln!(md);
}

/// The markdown report.
pub fn markdown(reports: &[ScenarioReport], seed: u64) -> String {
    let mut md = String::new();
    let _ = writeln!(
        md,
        "# Kimi K2.6 regional swarm capacity, synthetic inventory [CALC]\n"
    );
    let _ = writeln!(
        md,
        "Generated by `cargo run --release -p arc-island --features simulator --bin arc-island-sim -- --seed {seed}` \
         (crate `arc-island`, ARC-AC v0). **Every number below is a projection from the labelled \
         assumptions over a synthetic inventory. None is a measurement and none is a count of real \
         community machines.** All inputs are synthetic, so every island here is a *simulated* island: \
         the lifecycle stops it at `Simulated`, and none can serve. Real numbers replace these as ENG-6 \
         measures them.\n"
    );
    let _ = writeln!(
        md,
        "## Assumptions\n\n| Input | Value | Basis |\n|---|---|---|"
    );
    for (k, v, basis) in assumptions() {
        let _ = writeln!(
            md,
            "| {k} | {v} | ASSUMED for this simulation; source: {basis} |"
        );
    }

    let _ = writeln!(
        md,
        "\n## Summary\n\nPer-answer tok/s: median (min–max) over the islands formed, one answer at a time. \
         \"With spec\" is single-answer speculation: at the fixed draft depth in controlled rows, at the best \
         depth per island in adaptive and legacy rows. Aggregates use the scenario's batching policy, shown in \
         its label: fixed depths (controlled rows) or the adaptive search (adaptive rows). Daily figures are fully loaded ceilings excluding both spare-shortfall and dissolved downtime, as well as recovery stalls, prefill and audit.\n\nBoth downtime shares use initially formed islands × the full 6 h lease as denominator. The categories are mutually exclusive; no islands means no denominator (shown as –). Other time is not measured serving uptime.\n"
    );
    let _ = writeln!(
        md,
        "### Ordinary-node capacity and cost [ASSUMED inputs → CALC outputs]\n\nThe ~130 node count comes from TJ; no measured current inventory or RTT matrix was supplied. Regional-density rows are conditional recruitment/placement scenarios, not a claim that today’s nodes are colocated. No large-memory host is required. Each swarm still contains complete layers/all experts; expert-group placement in the runtime remains ENG-6 work. Deep draft and batching columns are performance-model sweeps for ENG-8/ENG-1, not implementations or measured gains.\n"
    );
    controlled_section(&mut md, reports);
    let _ = writeln!(
        md,
        "| Nodes | Scenario | Concurrent swarms | Stages (range) | Per-answer at speculative batch (range) | Aggregate speculative tok/s | Tokens/day ceiling | Projected tokens/day after demand + lease discount | Allocated cost USD/day | USD / million tokens, plain / spec |\n|---|---|---|---|---|---|---|---|---|---|"
    );
    for r in reports {
        let mut stages: Vec<f64> = r.rows.iter().map(|x| x.members as f64).collect();
        let mut loaded: Vec<f64> = r
            .rows
            .iter()
            .map(|x| x.projection.batch_speculative.per_stream_tok_s)
            .collect();
        let price = |x: Option<f64>| {
            x.map_or_else(|| "undefined (zero output)".into(), |v| format!("{v:.3}"))
        };
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {:.2} | {} / {} |",
            r.scenario.nodes,
            scenario_label(&r.scenario),
            r.rows.len(),
            spread(&mut stages),
            spread(&mut loaded),
            fmt_big(r.aggregate_tok_s_speculative),
            fmt_big(r.tokens_per_day_speculative),
            fmt_big(r.cost_speculative.projected_tokens_per_day),
            r.cost_speculative.total_usd_per_day,
            price(r.cost_plain.usd_per_million_tokens),
            price(r.cost_speculative.usd_per_million_tokens)
        );
    }
    let _ = writeln!(
        md,
        "\nCost denominators discount the ceilings for assumed demand and simulated lease downtime; full-day costs still charge spares and downtime. They are not prices offered by ARC. The existing ceiling columns below exclude downtime; they must not be read as delivered volume.\n"
    );
    let best = reports
        .iter()
        .flat_map(|r| &r.rows)
        .map(|r| {
            r.projection
                .speculative
                .tok_s
                .max(r.projection.single.tok_s)
        })
        .fold(0.0, f64::max);
    let _ = writeln!(
        md,
        "Best per-answer projection: {best:.1} tok/s. {}\n",
        if best < 59.0 {
            "No projection in this assumption set reaches 59 tok/s per answer; this is not a measured limit of regional swarms. ENG-6, ENG-8, ENG-9 and ENG-1 measurements must calibrate the model."
        } else {
            "These are synthetic projections, not measured serving performance."
        }
    );
    let _ = writeln!(
        md,
        "| Nodes | Scenario | Online | Eligible | Largest region, GB (one copy needs) | Islands T0 / T1a / T1b | Swarms metro / zone / region / neighbour | Members + spares (required) | Unused eligible | Hops per token | Per-answer tok/s | With spec | Aggregate tok/s, plain / spec | Tokens/day ceiling, plain / spec (excludes both downtimes) | Survive 6 h lease | Spare-shortfall / total lease | Dissolved / total lease | Serving / simulated |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for r in reports {
        let t = |tier: Tier| r.islands_by_tier.get(tier.label()).copied().unwrap_or(0);
        let mut single: Vec<f64> = r.rows.iter().map(|x| x.projection.single.tok_s).collect();
        let mut spec: Vec<f64> = r
            .rows
            .iter()
            .map(|x| x.projection.speculative.tok_s)
            .collect();
        let mut hops: Vec<f64> = r.rows.iter().map(|x| x.hops as f64).collect();
        let required: usize = r.rows.iter().map(|x| x.required_spares).sum();
        let (survive, shortfall, dissolved) = if r.churn.islands == 0 {
            ("–".into(), "–".into(), "–".into())
        } else {
            (
                format!("{}/{}", r.churn.survived, r.churn.islands),
                format!("{:.1}%", r.churn.spare_shortfall_share * 100.0),
                format!("{:.1}%", r.churn.dissolved_share * 100.0),
            )
        };
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} {:.0} ({:.0}) | {} / {} / {} | {} / {} / {} / {} | {} + {} ({}) | {} | {} | {} | {} | {} / {} | {} / {} | {} | {} | {} | {} / {} |",
            r.scenario.nodes,
            scenario_label(&r.scenario),
            r.online,
            r.eligible_online,
            r.largest_region.0,
            r.largest_region.1,
            r.needed_gb,
            t(Tier::T0Single),
            t(Tier::T1aRdma),
            t(Tier::T1bLan),
            t(Tier::T2Metro),
            t(Tier::T2Zone),
            t(Tier::T2Region),
            t(Tier::T2Neighbour),
            r.machines_serving,
            r.spares,
            required,
            r.unused_eligible,
            if hops.is_empty() {
                "–".into()
            } else {
                spread(&mut hops).replace(".0", "")
            },
            spread(&mut single),
            spread(&mut spec),
            fmt_big(r.aggregate_tok_s),
            fmt_big(r.aggregate_tok_s_speculative),
            fmt_big(r.tokens_per_day),
            fmt_big(r.tokens_per_day_speculative),
            survive,
            shortfall,
            dissolved,
            r.serving_islands,
            r.simulated_islands,
        );
    }

    let _ = writeln!(
        md,
        "\n## What the spare requirement changes\n\nSame inventory (batch, pessimistic transport, a = 0.7, opt-in 90%), with and without the research-6 spare policy:\n"
    );
    let _ = writeln!(
        md,
        "| Nodes | Spares | Swarms | Members | Spares held / required | Unused eligible | Aggregate tok/s | Survive 6 h lease | Spare-shortfall / total lease | Dissolved / total lease |\n|---|---|---|---|---|---|---|---|---|---|"
    );
    for r in reports.iter().filter(|r| {
        r.scenario.service == Service::Batch
            && r.scenario.transport == "pessimistic"
            && r.scenario.availability_permille == 700
    }) {
        let required: usize = r.rows.iter().map(|x| x.required_spares).sum();
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} / {} | {} | {} | {}/{} | {} | {} |",
            r.scenario.nodes,
            if r.scenario.require_spares {
                "required"
            } else {
                "not required"
            },
            r.rows.len(),
            r.machines_serving,
            r.spares,
            required,
            r.unused_eligible,
            fmt_big(r.aggregate_tok_s),
            r.churn.survived,
            r.churn.islands,
            if r.churn.islands == 0 {
                "–".into()
            } else {
                format!("{:.1}%", r.churn.spare_shortfall_share * 100.0)
            },
            if r.churn.islands == 0 {
                "–".into()
            } else {
                format!("{:.1}%", r.churn.dissolved_share * 100.0)
            },
        );
    }
    let _ = writeln!(
        md,
        "\nEvery required spare must hold the island's largest stage, so the largest machines in a cell become \
         the spares and every stage is capped at the smallest spare's memory. A swarm therefore needs more \
         members than the same swarm without spares; ordinary 16 GB machines can fill every role, and fewer swarms form from the \
         same inventory. In exchange, an island survives member loss by promotion and checkpoint recovery \
         when a suitable spare remains. Spare-shortfall pauses include waiting for an eligible replacement as well as shard staging. Once dissolved, all remaining lease time counts only as dissolved time; optional-spares rows have zero spare-shortfall pause.\n"
    );

    let _ = writeln!(md, "## Every island and swarm, by scenario\n");
    for r in reports {
        let _ = writeln!(
            md,
            "### {} nodes · {}\n",
            r.scenario.nodes,
            scenario_label(&r.scenario)
        );
        let rejected: Vec<String> = r
            .rejected
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        let _ = writeln!(
            md,
            "Online {} · opted in {} · eligible {} · rejected: {} · churn over the lease: {} member failures, {} spare failures, {} promotions (each with checkpoint recovery and re-qualification), {} spares replenished, {} dissolved.\n",
            r.online,
            r.opted_in_online,
            r.eligible_online,
            if rejected.is_empty() {
                "none".into()
            } else {
                rejected.join(", ")
            },
            r.churn.member_failures,
            r.churn.spare_failures,
            r.churn.promotions,
            r.churn.replenished,
            r.churn.dissolved,
        );
        if r.rows.is_empty() {
            let _ = writeln!(md, "No Kimi-capable island or swarm forms.\n");
            continue;
        }
        let _ = writeln!(
            md,
            "| Tier | Cell | Members | Spares | Hops | Mean hop RTT ms | Ring RTT ms | Max pair p95 ms | Per-answer tok/s | With spec (k) | Batch B / k | Per-stream at B | Aggregate tok/s | Spec batch B / k | Spec aggregate tok/s | Tokens/day ceiling, plain / spec (excludes both downtimes) | Members | Spare machines |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
        );
        let mut rows: Vec<&IslandRow> = r.rows.iter().collect();
        rows.sort_by(|a, b| {
            b.projection
                .batch
                .aggregate_tok_s
                .total_cmp(&a.projection.batch.aggregate_tok_s)
        });
        for x in rows {
            let p = &x.projection;
            let _ = writeln!(
                md,
                "| {} | {} | {} | {}/{} | {} | {:.1} | {:.1} | {:.1} | {} | {} ({}) | {} / {} | {} | {} | {} / {} | {} | {} / {} | {} | {} |",
                x.tier.label(),
                x.cell,
                x.members,
                x.spares,
                x.required_spares,
                x.hops,
                x.mean_hop_rtt_ms,
                x.ring_p50_ms,
                x.max_pair_p95_ms,
                fmt_rate(p.single.tok_s),
                fmt_rate(p.speculative.tok_s),
                p.speculative.draft_tokens,
                p.batch.concurrent,
                p.batch.draft_tokens,
                fmt_rate(p.batch.per_stream_tok_s),
                fmt_rate(p.batch.aggregate_tok_s),
                p.batch_speculative.concurrent,
                p.batch_speculative.draft_tokens,
                fmt_rate(p.batch_speculative.aggregate_tok_s),
                fmt_big(x.tokens_per_day),
                fmt_big(x.tokens_per_day_speculative),
                x.mix,
                if x.spare_mix.is_empty() {
                    "–"
                } else {
                    &x.spare_mix
                },
            );
        }
        let _ = writeln!(md);
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_130_node_report_has_capacity_cost_and_no_real_serving() {
        let model = ModelSpec::kimi_k26_int4();
        let scenarios = regional_scenarios(7, &[130]);
        assert_eq!(scenarios.len(), 8);
        let mut reports = Vec::new();
        for s in &scenarios {
            let a = run(s, &model);
            assert_eq!(a, run(s, &model));
            assert_eq!(a.serving_islands, 0);
            for row in &a.rows {
                assert!(row.members >= 40);
                assert!(row.mix.contains("16 GB RAM"));
                assert!(!row.mix.contains("Ultra"));
                assert_eq!(row.required_spares, 3);
                assert!(row.projection.batch_speculative.concurrent <= 128);
                assert!(row.projection.batch_speculative.draft_tokens <= 64);
            }
            assert!(a.input_basis.values().all(|s| s.starts_with("ASSUMED")));
            assert!(a.cost_speculative.projected_tokens_per_day <= a.tokens_per_day_speculative);
            if !a.rows.is_empty() {
                assert!(a.cost_speculative.total_usd_per_day > 0.0);
                assert!(a.cost_speculative.usd_per_million_tokens.unwrap() > 0.0);
            } else {
                assert_eq!(a.cost_speculative.usd_per_million_tokens, None);
            }
            reports.push(a);
        }
        assert!(
            reports[0].simulated_islands > 0,
            "a concentrated region can hold Kimi on 16 GB nodes"
        );
        assert_eq!(
            reports.last().unwrap().simulated_islands,
            0,
            "dispersed sensitivity is not a colocated fleet"
        );
        let md = markdown(&reports, 7);
        assert!(md.contains("USD / million tokens"));
        assert!(md.contains("Per-answer at speculative batch"));
        assert!(md.contains("not a measured limit of regional swarms"));
        serde_json::to_string(&reports).unwrap();
    }

    #[test]
    fn controlled_rtt_comparison_holds_depths_placement_and_monotonicity() {
        let model = ModelSpec::kimi_k26_int4();
        let scenarios = controlled_regional_scenarios(7, &[100, 130], &model);
        // Every row in a matched group carries the group's fixed depths.
        for s in &scenarios {
            let BatchingPolicy::Fixed { chosen_from, .. } = &s.batching else {
                panic!("default rows are controlled: {s:?}");
            };
            assert!(!chosen_from.is_empty());
            let twin = scenarios
                .iter()
                .find(|o| o.nodes == s.nodes && o.transport == s.transport)
                .unwrap();
            assert_eq!(twin.batching, s.batching);
        }
        let reports: Vec<_> = scenarios.iter().map(|s| run(s, &model)).collect();
        let checks = controlled_rtt_checks(&reports);
        // 100 and 130 nodes × 50 and 500 Mb/s.
        assert_eq!(checks.len(), 4);
        for c in &checks {
            assert_eq!(c.rtts_ms, vec![5, 10, 20]);
            assert!(c.holds(), "{c:?}");
            assert_eq!(c.plain.draft_tokens, 0);
        }
        assert!(checks.iter().all(|c| !c.zero_output));
        // Zero output: the same matched groups with too few opted-in nodes
        // form no swarm at any RTT; cost per token stays undefined, and the
        // vacuous comparison still holds.
        let sparse: Vec<_> = scenarios
            .iter()
            .filter(|s| s.nodes == 130)
            .map(|s| {
                let mut s = s.clone();
                s.opt_in_permille = 400;
                run(&s, &model)
            })
            .collect();
        let zero = controlled_rtt_checks(&sparse);
        assert_eq!(zero.len(), 2);
        for c in &zero {
            assert!(c.zero_output && c.holds(), "{c:?}");
        }
        for r in &sparse {
            assert!(r.rows.is_empty());
            assert_eq!(r.cost_plain.usd_per_million_tokens, None);
            assert_eq!(r.cost_speculative.usd_per_million_tokens, None);
        }
        // At 130 nodes the rates strictly fall from 5 to 20 ms.
        for transport in ["50-Mbps", "500-Mbps"] {
            let at = |rtt: u32| {
                reports
                    .iter()
                    .find(|r| {
                        r.scenario.nodes == 130
                            && r.scenario.transport == transport
                            && r.scenario.availability_permille == 700
                            && matches!(r.scenario.inventory, InventoryProfile::OrdinaryRegional { rtt_us, .. } if rtt_us == rtt)
                    })
                    .unwrap()
            };
            let (fast, slow) = (at(5_000), at(20_000));
            assert!(!fast.rows.is_empty());
            assert!(slow.aggregate_tok_s < fast.aggregate_tok_s, "{transport}");
            assert!(
                slow.aggregate_tok_s_speculative < fast.aggregate_tok_s_speculative,
                "{transport}"
            );
            assert!(
                slow.cost_plain.usd_per_million_tokens.unwrap()
                    > fast.cost_plain.usd_per_million_tokens.unwrap(),
                "{transport}"
            );
            assert!(
                slow.cost_speculative.usd_per_million_tokens.unwrap()
                    > fast.cost_speculative.usd_per_million_tokens.unwrap(),
                "{transport}"
            );
        }
        let md = markdown(&reports, 7);
        assert!(md.contains("Controlled RTT comparison (default)"));
        assert!(!md.contains("**NO**"));
        assert!(md.contains("Single-answer tok/s (B = 1), plain / spec (k)"));
        assert!(md.contains("Per-answer tok/s at fixed batch, plain / spec"));
        // Distinct sentinels catch accidental reuse of B=1 values for the
        // fixed-batch column without changing any performance-model logic.
        let mut display = reports.clone();
        for row in &mut display[0].rows {
            row.projection.single.tok_s = 12.3;
            row.projection.speculative.tok_s = 23.4;
            row.projection.batch.per_stream_tok_s = 3.4;
            row.projection.batch_speculative.per_stream_tok_s = 4.5;
        }
        let rendered = markdown(&display, 7);
        assert!(
            rendered.contains(
                "12.3 (12.3–12.3) / 23.4 (23.4–23.4) (1) | 3.4 (3.4–3.4) / 4.5 (4.5–4.5)"
            )
        );
    }

    #[test]
    fn rtt_checks_flag_an_uncontrolled_or_inverted_comparison() {
        let model = ModelSpec::kimi_k26_int4();
        let scenarios = controlled_regional_scenarios(7, &[130], &model);
        let mut reports: Vec<_> = scenarios
            .iter()
            .filter(|s| {
                s.transport == "50-Mbps"
                    && matches!(s.inventory, InventoryProfile::OrdinaryRegional { .. })
                    && s.availability_permille == 700
            })
            .map(|s| run(s, &model))
            .collect();
        assert_eq!(reports.len(), 3);
        assert!(controlled_rtt_checks(&reports)[0].holds());
        // Swapping the 5 ms and 20 ms results is an inversion the check catches.
        let fast = reports[0].clone();
        let slow = reports[2].clone();
        reports[0].aggregate_tok_s = slow.aggregate_tok_s;
        reports[2].aggregate_tok_s = fast.aggregate_tok_s;
        let c = &controlled_rtt_checks(&reports)[0];
        assert!(!c.aggregate_non_increasing && !c.holds());
        // A row run at a different depth is not a controlled comparison.
        let mut changed = scenarios
            .iter()
            .find(|s| {
                s.transport == "50-Mbps"
                    && matches!(
                        s.inventory,
                        InventoryProfile::OrdinaryRegional { rtt_us: 20_000, .. }
                    )
                    && s.availability_permille == 700
            })
            .unwrap()
            .clone();
        if let BatchingPolicy::Fixed { speculative, .. } = &mut changed.batching {
            speculative.concurrent += 1;
        }
        reports[2] = run(&changed, &model);
        reports[0] = fast;
        let c = &controlled_rtt_checks(&reports)[0];
        assert!(!c.same_depths && !c.holds());
    }

    #[test]
    fn inventory_follows_the_apportioned_mix() {
        let inv = Inventory::synthetic(1_000, 7, 900, 700);
        assert_eq!(inv.devices.len(), 1_000);
        let ultras = inv
            .class_of
            .iter()
            .filter(|&&c| DEVICE_MIX[c].name.starts_with("M3 Ultra"))
            .count();
        assert_eq!(ultras, 10);
        let us_east = inv
            .devices
            .iter()
            .filter(|d| d.location.region == "US-East")
            .count();
        assert!((120..=160).contains(&us_east), "{us_east}");
        let sites: BTreeSet<_> = inv.devices.iter().filter_map(|d| d.site.clone()).collect();
        assert_eq!(sites.len(), 17);
        let online = inv.online.iter().filter(|&&o| o).count();
        assert!((640..=760).contains(&online), "{online}");
        let half = Inventory::synthetic(1_000, 7, 900, 500);
        let online = half.online.iter().filter(|&&o| o).count();
        assert!((440..=560).contains(&online), "{online}");
        // Consent is bound to each device's own owner and id.
        let opted = inv
            .devices
            .iter()
            .filter(|d| d.consent.permits(d, 0))
            .count();
        assert!((860..=940).contains(&opted), "{opted}");
        assert!(
            inv.devices
                .iter()
                .all(|d| d.evidence.provenance == crate::device::Provenance::Synthetic)
        );
    }

    #[test]
    fn synthetic_rtt_matches_the_planning_medians() {
        let inv = Inventory::synthetic(1_000, 3, 900, 700);
        let (devices, _, rtt) = inv.online_snapshot();
        let mut metro = Vec::new();
        for a in 0..devices.len() {
            for b in (a + 1)..devices.len() {
                if devices[a].site.is_none()
                    && devices[b].site.is_none()
                    && devices[a].location.metro == devices[b].location.metro
                {
                    metro.push(rtt.link(a, b).unwrap().p50_us);
                }
            }
        }
        metro.sort_unstable();
        let median = metro[metro.len() / 2];
        // research-7 §1.6: metro home-to-home median about 10 ms.
        assert!((9_000..=13_000).contains(&median), "{median}");
    }

    #[test]
    fn apportionment_is_exact() {
        assert_eq!(apportion(10, &[1.0, 1.0, 1.0]), vec![4, 3, 3]);
        assert_eq!(apportion(100, &[16.0, 84.0]).iter().sum::<usize>(), 100);
    }

    #[test]
    fn scenarios_run_deterministically_and_never_serve() {
        let model = ModelSpec::kimi_k26_int4();
        let scenarios = standard_scenarios(7, &[130, 1_000]);
        let a: Vec<_> = scenarios.iter().map(|s| run(s, &model)).collect();
        let b: Vec<_> = scenarios.iter().map(|s| run(s, &model)).collect();
        assert_eq!(a, b);
        let mut formed = 0;
        for r in &a {
            assert_eq!(r.serving_islands, 0, "synthetic inputs must never serve");
            assert_eq!(r.simulated_islands, r.rows.len());
            assert_eq!(r.churn.survived + r.churn.dissolved, r.churn.islands);
            let c = &r.churn;
            assert_eq!(
                c.total_island_time_ms,
                c.islands as u64 * (LEASE_H * 3_600_000.0) as u64
            );
            assert_eq!(
                c.spare_shortfall_time_ms + c.dissolved_time_ms + c.other_time_ms,
                c.total_island_time_ms
            );
            assert!(c.spare_shortfall_share + c.dissolved_share <= 1.0);
            if !r.scenario.require_spares {
                assert_eq!(c.spare_shortfall_time_ms, 0);
                assert_eq!(c.spare_shortfall_share.to_bits(), 0.0f64.to_bits());
            }
            for row in &r.rows {
                formed += 1;
                if r.scenario.require_spares {
                    assert_eq!(row.spares, row.required_spares, "{row:?}");
                    assert!(row.required_spares >= 1);
                }
                assert!(row.projection.single.tok_s > 0.0);
                assert!(row.projection.speculative.tok_s >= row.projection.single.tok_s);
                assert!(
                    row.projection.batch_speculative.aggregate_tok_s
                        >= row.projection.batch.aggregate_tok_s
                );
            }
        }
        assert!(formed > 0, "1,000 nodes form at least one swarm");
        let md = markdown(&a, 7);
        assert!(md.contains("| 130 |"));
        assert!(md.contains("What the spare requirement changes"));
        assert!(md.contains("Spare-shortfall / total lease"));
        assert!(md.contains("Dissolved / total lease"));
        assert!(md.contains("excludes both downtimes"));
        let mut previous_columns = None;
        for line in md.lines() {
            if line.starts_with('|') {
                let columns = line.matches('|').count();
                if let Some(previous) = previous_columns {
                    assert_eq!(columns, previous, "{line}");
                }
                previous_columns = Some(columns);
            } else {
                previous_columns = None;
            }
        }
    }

    #[test]
    fn lease_intervals_are_mutually_exclusive_even_when_dissolved_with_shortfall() {
        let (_, _, mut island) = crate::lifecycle::tests::setup(0);
        let mut report = ChurnReport::default();
        report.account_interval(&island, 100);
        island.device_lost(island.plan().spares[0].device, 100);
        report.account_interval(&island, 200);
        island.dissolve(300, crate::lifecycle::DissolveReason::Requested);
        assert!(island.spare_shortfall() > 0);
        report.account_interval(&island, 700);
        report.finish_accounting();
        assert_eq!(report.total_island_time_ms, 1000);
        assert_eq!(report.other_time_ms, 100);
        assert_eq!(report.spare_shortfall_time_ms, 200);
        assert_eq!(report.dissolved_time_ms, 700);
        assert!((report.spare_shortfall_share - 0.2).abs() < f64::EPSILON);
        assert!((report.dissolved_share - 0.7).abs() < f64::EPSILON);
        let mut empty = ChurnReport::default();
        empty.finish_accounting();
        assert_eq!(empty.spare_shortfall_share.to_bits(), 0.0f64.to_bits());
        assert_eq!(empty.dissolved_share.to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn reviewed_optional_spare_scenarios_have_only_dissolved_downtime() {
        for scenario in standard_scenarios(7, &[1_000, 10_000])
            .into_iter()
            .filter(|s| !s.require_spares)
        {
            let report = run(&scenario, &ModelSpec::kimi_k26_int4());
            let c = report.churn;
            assert!(c.islands > 0 && c.dissolved > 0);
            assert_eq!(c.spare_shortfall_time_ms, 0);
            assert_eq!(c.spare_shortfall_share.to_bits(), 0.0f64.to_bits());
            assert!(c.dissolved_time_ms > 0);
            assert_eq!(
                c.total_island_time_ms,
                c.islands as u64 * c.lease_duration_ms
            );
            assert_eq!(
                c.dissolved_time_ms + c.other_time_ms,
                c.total_island_time_ms
            );
        }
    }
}
