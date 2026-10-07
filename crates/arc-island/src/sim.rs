//! Offline capacity simulator: how many Kimi-capable islands and swarms form
//! on a synthetic community inventory, and how fast they would be.
//!
//! **Nothing here is a community number.** The inventory is synthetic. Its
//! geography and device mix are research-7 §4.1's planning inputs (themselves
//! labelled \[UNVERIFIED\] there), apportioned exactly (largest remainder), so
//! the counts are expected values, not samples. Every other input is listed
//! in [`assumptions`] and printed with the report. Projections are \[CALC\].

use crate::device::{
    Consent, DeviceDescriptor, IslandFacts, LinkStats, Location, Measured, RttSource,
};
use crate::form::{FormationOutcome, FormationPolicy, IslandPlan, RejectReason, Tier, form};
use crate::lifecycle::Island;
use crate::model::ModelSpec;
use crate::perf::{NetAssumptions, Projection, project};
use crate::selftest::ToyPipeline;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
pub const DEVICE_MIX: [DeviceClass; 14] = [
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
    /// Builds `nodes` synthetic devices. Availability 70% (research-7 §4.1);
    /// `opt_in_permille` of owners answer yes to both consent questions; all
    /// pass the golden self-test \[ASSUMPTION\].
    pub fn synthetic(nodes: usize, seed: u64, opt_in_permille: u32) -> Self {
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
                        availability_permille: 700,
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
                Consent::opted_in()
            } else {
                Consent {
                    compute: Some(true),
                    island: None,
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
}

impl RttSource for SyntheticRtt {
    fn link(&self, a: usize, b: usize) -> Option<LinkStats> {
        let stats = |p50: u32| LinkStats {
            p50_us: p50,
            p95_us: p50 + p50 / 4,
            p99_us: p50 + p50 / 2,
            loss_permille: 0,
            samples: 200,
        };
        if a == b {
            return Some(stats(0));
        }
        let (da, db) = (&self.devices[a], &self.devices[b]);
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

/// One simulator run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    pub nodes: usize,
    pub seed: u64,
    pub opt_in_permille: u32,
    pub service: Service,
    pub transport: String,
    pub net: NetAssumptions,
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
/// Stall while a promoted spare re-qualifies (research-6 §6.7 target: ≤ 2 s
/// GPU, ≤ 10 s Mac); the simulator uses 10 s.
pub const PROMOTION_STALL_MS: u64 = 10_000;

/// One island in the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IslandRow {
    pub tier: Tier,
    pub cell: String,
    pub members: usize,
    pub hops: usize,
    pub ring_p50_ms: f64,
    pub mean_hop_rtt_ms: f64,
    pub max_pair_p95_ms: f64,
    pub mix: String,
    pub spares: usize,
    pub projection: Projection,
    pub tokens_per_day: f64,
}

/// Churn over one lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChurnReport {
    pub islands: usize,
    pub member_failures: usize,
    pub spare_failures: usize,
    pub promotions: usize,
    pub dissolved: usize,
    pub survived: usize,
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
    pub tokens_per_day: f64,
    pub churn: ChurnReport,
}

fn mix(plan: &IslandPlan, inv: &Inventory, idx: &[usize]) -> String {
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for &m in &plan.members {
        *counts.entry(inv.class_of[idx[m]]).or_default() += 1;
    }
    let mut parts: Vec<(usize, usize)> = counts.into_iter().collect();
    parts.sort_by_key(|&(c, n)| {
        (
            std::cmp::Reverse(
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

fn churn(
    outcome: &FormationOutcome,
    devices: &[DeviceDescriptor],
    model: &ModelSpec,
    rng: &mut Rng,
) -> ChurnReport {
    let golden = ToyPipeline::golden(model.layers.len());
    let lease_ms = LEASE_H * 3_600_000.0;
    let mut r = ChurnReport {
        islands: outcome.islands.len(),
        ..ChurnReport::default()
    };
    for plan in &outcome.islands {
        let mut island = Island::new(plan.clone(), 0);
        island.qualify(0, &mut ToyPipeline::default(), devices, &golden);
        let spares: Vec<usize> = plan.spares.iter().map(|s| s.device).collect();
        let mut failures: Vec<(u64, usize)> = island
            .devices()
            .into_iter()
            .filter_map(|d| {
                // Exponential time to failure.
                let t = -MTBF_H * 3_600_000.0 * (1.0 - rng.unit()).ln();
                (t < lease_ms).then_some((t as u64, d))
            })
            .collect();
        failures.sort_unstable();
        for (t, d) in failures {
            if island.is_dissolved() {
                break;
            }
            let was_member = island.plan.members.contains(&d);
            if spares.contains(&d) && !was_member {
                r.spare_failures += 1;
            } else {
                r.member_failures += 1;
            }
            let before = island.generation;
            island.device_lost(d, t);
            if island.generation > before {
                r.promotions += 1;
                island.qualify(
                    t + PROMOTION_STALL_MS,
                    &mut ToyPipeline::default(),
                    devices,
                    &golden,
                );
            }
        }
        if island.is_dissolved() {
            r.dissolved += 1;
        } else {
            r.survived += 1;
        }
    }
    r
}

/// Runs one scenario.
pub fn run(scenario: &Scenario, model: &ModelSpec) -> ScenarioReport {
    let inv = Inventory::synthetic(scenario.nodes, scenario.seed, scenario.opt_in_permille);
    let (devices, idx, rtt) = inv.online_snapshot();
    let kv_positions = kv_sequences(scenario.service) * CONTEXT_POSITIONS;
    let policy = match scenario.service {
        Service::Interactive => FormationPolicy::interactive(kv_positions),
        Service::Batch => FormationPolicy::batch(kv_positions),
    };
    let outcome = form(&devices, &rtt, model, &policy);

    let mut rejected: BTreeMap<String, usize> = BTreeMap::new();
    for r in &outcome.rejected {
        let key = match r.reason {
            RejectReason::NoConsent => "no island consent",
            RejectReason::NotQualified => "not golden-qualified",
            RejectReason::NoMemoryFacts => "no memory facts",
            RejectReason::LowAvailability => "low availability",
            RejectReason::TooSmallForOneLayer => "too small for one layer",
        };
        *rejected.entry(key.into()).or_default() += 1;
    }
    let mut islands_by_tier: BTreeMap<String, usize> = BTreeMap::new();
    let mut rows = Vec::new();
    for plan in &outcome.islands {
        *islands_by_tier.entry(plan.tier.label().into()).or_default() += 1;
        let p = project(
            plan,
            &devices,
            model,
            &rtt,
            &scenario.net,
            CONTEXT_POSITIONS,
            BATCH_FLOOR_TOK_S,
        );
        let hops = plan.hops();
        rows.push(IslandRow {
            tier: plan.tier,
            cell: plan.cell.clone(),
            members: plan.members.len(),
            hops,
            ring_p50_ms: plan.ring_p50_us as f64 / 1000.0,
            mean_hop_rtt_ms: if hops == 0 {
                0.0
            } else {
                plan.ring_p50_us as f64 / 1000.0 / hops as f64
            },
            max_pair_p95_ms: f64::from(plan.max_pair_p95_us) / 1000.0,
            mix: mix(plan, &inv, &idx),
            spares: plan.spares.len(),
            tokens_per_day: p.batch.aggregate_tok_s * 86_400.0,
            projection: p,
        });
    }
    let opted_in_online = devices.iter().filter(|d| d.consent.allows_island()).count();
    let rejected_set: std::collections::BTreeSet<usize> =
        outcome.rejected.iter().map(|r| r.device).collect();
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
        .max_by_key(|&(k, v)| (*v, std::cmp::Reverse(k.clone())))
        .map_or((String::new(), 0.0), |(k, v)| (k.clone(), *v as f64 / 1e9));
    let need = model.weight_bytes() + model.kv_bytes_per_position() * kv_positions;
    let needed_gb = (need + need * policy.capacity_headroom_permille / 1000) as f64 / 1e9;
    let mut rng = Rng::new(scenario.seed ^ 0xC0FFEE);
    ScenarioReport {
        online: devices.len(),
        opted_in_online,
        eligible_online: devices.len() - outcome.rejected.len(),
        rejected,
        islands_by_tier,
        machines_serving: outcome.islands.iter().map(|p| p.members.len()).sum(),
        spares: outcome.islands.iter().map(|p| p.spares.len()).sum(),
        unused_eligible: outcome.unused.len(),
        needed_gb,
        largest_region,
        // `+ 0.0`: an empty float sum is -0.0.
        aggregate_tok_s: rows
            .iter()
            .map(|r| r.projection.batch.aggregate_tok_s)
            .sum::<f64>()
            + 0.0,
        tokens_per_day: rows.iter().map(|r| r.tokens_per_day).sum::<f64>() + 0.0,
        churn: churn(&outcome, &devices, model, &mut rng),
        rows,
        scenario: scenario.clone(),
    }
}

/// The scenarios the report covers.
pub fn standard_scenarios(seed: u64, node_counts: &[usize]) -> Vec<Scenario> {
    let mut out = Vec::new();
    for &nodes in node_counts {
        for (service, transport, net, opt_in) in [
            (
                Service::Batch,
                "pessimistic",
                NetAssumptions::pessimistic(),
                1000,
            ),
            (
                Service::Batch,
                "optimized",
                NetAssumptions::optimized(),
                1000,
            ),
            (
                Service::Interactive,
                "pessimistic",
                NetAssumptions::pessimistic(),
                1000,
            ),
            (
                Service::Interactive,
                "optimized",
                NetAssumptions::optimized(),
                1000,
            ),
            (
                Service::Batch,
                "pessimistic",
                NetAssumptions::pessimistic(),
                500,
            ),
        ] {
            out.push(Scenario {
                nodes,
                seed,
                opt_in_permille: opt_in,
                service,
                transport: transport.into(),
                net,
            });
        }
    }
    out
}

/// Every labelled input, for the report.
pub fn assumptions() -> Vec<(&'static str, String, &'static str)> {
    let p = NetAssumptions::pessimistic();
    let o = NetAssumptions::optimized();
    vec![
        ("Model", "Kimi K2.6, INT4 g32 routed experts + INT8 elsewhere: 582.6 GB weights, 140,544 B KV per position, 56 KiB Q16 boundary per position, no MTP head".into(), "CALC from docs/protocol/kimi-k26-checkpoint.md (#156)"),
        ("Inventory geography", "research-7 §4.1 shares (US/EU-skewed), Asia ×1.25/×1.55 and Africa ×1.2/×1.6 at 1,000/10,000; metros inside an area weighted by a fixed synthetic table".into(), "UNVERIFIED planning input (research-7) + ASSUMPTION"),
        ("Device mix", "8 GB GPU 10%, 12 GB 15%, 16 GB 15%, 24 GB 20%, 32 GB 8%, Mac 16–24 GB 10%, Mac 36–64 GB 12%, 128 GB Mac/Strix 6%, 256–512 GB Ultra 1%, CPU-only 3%; even splits inside each class".into(), "UNVERIFIED planning input (research-7 §4.1)"),
        ("Online share", "70% of nodes online at formation (per-node availability a = 0.7)".into(), "UNVERIFIED (research-7 §4.1)"),
        ("Consent", "100% of owners opted in to islands (upper bound); 50% sensitivity rows".into(), "ASSUMPTION; consent model from #138"),
        ("Golden qualification", "every synthetic device passes the self-test".into(), "ASSUMPTION"),
        ("Usable memory", "85% of GPU memory, 80% of unified/system memory".into(), "research-6 §2.2"),
        ("Bandwidth", "llama.cpp-class effective GB/s by class: GPU 8/12/16/24/32 GB = 250/300/450/680/1,108; Mac ≤24 GB 90, 36–192 GB 300, ≥256 GB 456; CPU 60. ARC's engine is not yet measured at these rates".into(), "CALC (research-6 §2.5, research-7 §2.6), UNVERIFIED for ARC"),
        ("LAN sites", "3% / 5% / 8% of nodes at <1,000 / 1,000 / 10,000 on sites of 3 devices of one owner".into(), "UNVERIFIED (research-7 §4.1)"),
        ("Home RTT", "access leg to the metro hub drawn from RIPE Atlas 0–100 km quantiles (p25 3.7, p50 5.5, p75 9.8, p90 22.1 ms); pair RTT = both legs + core (metro −1, zone +11, region +27, continent +45, world +181 ms); p95 = 1.25 × p50, p99 = 1.5 × p50, no loss".into(), "MEASURED quantiles (research-7 §1.6) + CALC + ASSUMPTION for jitter"),
        ("LAN RTT", "0.3 ms p50 on one site, 0.04 ms when both machines have Thunderbolt 5".into(), "ASSUMPTION (research-6 §1.2: RDMA < 50 µs)"),
        ("Pessimistic transport", format!("{} ms per hop, {} Mb/s home uplink, {} ms fixed per pass", f64::from(p.wan_hop_overhead_us) / 1000.0, p.wan_uplink_mbps, f64::from(p.fixed_pass_us) / 1000.0), "research-7 §2.1 defaults"),
        ("Optimized transport", format!("{} ms per hop, {} Mb/s uplink", f64::from(o.wan_hop_overhead_us) / 1000.0, o.wan_uplink_mbps), "ASSUMPTION (fibre homes, tuned streaming transport)"),
        ("Tensor parallel", format!("{} ms per collective on Thunderbolt-5 RDMA, 122 per token", f64::from(p.collective_us) / 1000.0), "research-6 §2.5"),
        ("Speculation", format!("chain drafts from a separate drafter (K2.6 has no MTP head), acceptance α = {}, {} ms per draft token, depth 0–{} chosen per island", p.draft_acceptance, f64::from(p.draft_us_per_token) / 1000.0, p.max_draft_tokens), "ASSUMPTION (research-7 §2.2 planning values)"),
        ("Batching", format!("KV for {} (interactive) / {} (batch) sequences × {} positions per island; depth = max aggregate keeping per-stream ≥ min({} tok/s, half the single-stream rate); speculation off when batching", kv_sequences(Service::Interactive), kv_sequences(Service::Batch), CONTEXT_POSITIONS, BATCH_FLOOR_TOK_S), "research-6 §2.6 model"),
        ("Compute model", "memory-bandwidth bound; distinct experts under uniform routing; FLOPs, prefill and queueing not modelled".into(), "CALC (research-6 §2.6)"),
        ("Tokens/day", "aggregate tok/s × 86,400: a capacity ceiling at 100% utilisation, not a demand forecast".into(), "CALC"),
        ("Churn", format!("device MTBF {MTBF_H} h, lease {LEASE_H} h, exponential failures; a member loss promotes a covering spare (re-qualify) or dissolves the island"), "research-6 §3.3 (Salad 92 h)"),
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
        "{} · {} · opt-in {}%",
        match s.service {
            Service::Batch => "batch (S ≤ 30)",
            Service::Interactive => "interactive (S ≤ 4/3/2)",
        },
        s.transport,
        s.opt_in_permille / 10
    )
}

/// The markdown report.
pub fn markdown(reports: &[ScenarioReport], seed: u64) -> String {
    let mut md = String::new();
    let _ = writeln!(
        md,
        "# Kimi K2.6 island and swarm capacity, synthetic inventory [CALC]\n"
    );
    let _ = writeln!(
        md,
        "Generated by `cargo run --release -p arc-island --bin arc-island-sim -- --seed {seed}` \
         (crate `arc-island`, ARC-AC v0). **Every number below is a projection from the labelled \
         assumptions over a synthetic inventory. None is a measurement and none is a count of real \
         community machines.** Real numbers replace these as ENG-6 measures them.\n"
    );
    let _ = writeln!(
        md,
        "## Assumptions\n\n| Input | Value | Basis |\n|---|---|---|"
    );
    for (k, v, basis) in assumptions() {
        let _ = writeln!(md, "| {k} | {v} | {basis} |");
    }

    let _ = writeln!(
        md,
        "\n## Summary\n\nPer-answer tok/s: median (min–max) over the islands formed, batch 1. \
         \"With spec\" picks the best draft depth per island. Aggregate is at the planned batching depth.\n"
    );
    let _ = writeln!(
        md,
        "| Nodes | Scenario | Online | Eligible | Largest region, GB usable (one copy needs) | Islands (T0 / T1a / T1b) | Swarms (metro / zone / region) | Machines serving | Spares | Unused eligible | Hops per token | Per-answer tok/s | With spec | Aggregate tok/s | Tokens/day | Survive 6 h lease |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
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
        let survive = if r.churn.islands == 0 {
            "–".into()
        } else {
            format!("{}/{}", r.churn.survived, r.churn.islands)
        };
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} {:.0} ({:.0}) | {} / {} / {} | {} / {} / {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            r.scenario.nodes,
            scenario_label(&r.scenario),
            r.online,
            r.eligible_online,
            r.largest_region.0,
            r.largest_region.1,
            r.needed_gb,
            t(Tier::T0Single),
            t(Tier::T1aThunderbolt),
            t(Tier::T1bLan),
            t(Tier::T2Metro),
            t(Tier::T2Zone),
            t(Tier::T2Region),
            r.machines_serving,
            r.spares,
            r.unused_eligible,
            if hops.is_empty() {
                "–".into()
            } else {
                spread(&mut hops).replace(".0", "")
            },
            spread(&mut single),
            spread(&mut spec),
            fmt_big(r.aggregate_tok_s),
            fmt_big(r.tokens_per_day),
            survive,
        );
    }

    let _ = writeln!(md, "\n## Islands and swarms by scenario\n");
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
            "Online {} · opted in {} · eligible {} · rejected: {} · churn over the lease: {} member failures, {} spare failures, {} promotions, {} dissolved.\n",
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
            r.churn.dissolved,
        );
        if r.rows.is_empty() {
            let _ = writeln!(md, "No Kimi-capable island or swarm forms.\n");
            continue;
        }
        let _ = writeln!(
            md,
            "| Tier | Cell | Members | Hops | Mean hop RTT ms | Ring RTT ms | Max pair p95 ms | Spares | tok/s | With spec (k) | Batch B | Per-stream at B | Aggregate tok/s | Tokens/day | Members |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
        );
        let mut rows: Vec<&IslandRow> = r.rows.iter().collect();
        rows.sort_by(|a, b| {
            b.projection
                .batch
                .aggregate_tok_s
                .total_cmp(&a.projection.batch.aggregate_tok_s)
        });
        const SHOWN: usize = 12;
        for x in rows.iter().take(SHOWN) {
            let p = &x.projection;
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {} | {} | {} ({}) | {} | {} | {} | {} | {} |",
                x.tier.label(),
                x.cell,
                x.members,
                x.hops,
                x.mean_hop_rtt_ms,
                x.ring_p50_ms,
                x.max_pair_p95_ms,
                x.spares,
                fmt_rate(p.single.tok_s),
                fmt_rate(p.speculative.tok_s),
                p.speculative.draft_tokens,
                p.batch.concurrent,
                fmt_rate(p.batch.per_stream_tok_s),
                fmt_rate(p.batch.aggregate_tok_s),
                fmt_big(x.tokens_per_day),
                x.mix,
            );
        }
        if rows.len() > SHOWN {
            let _ = writeln!(md, "\n{} more in the JSON report.", rows.len() - SHOWN);
        }
        let _ = writeln!(md);
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_follows_the_apportioned_mix() {
        let inv = Inventory::synthetic(1_000, 7, 1000);
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
        let sites: std::collections::BTreeSet<_> =
            inv.devices.iter().filter_map(|d| d.site.clone()).collect();
        assert_eq!(sites.len(), 17);
        let online = inv.online.iter().filter(|&&o| o).count();
        assert!((640..=760).contains(&online), "{online}");
    }

    #[test]
    fn synthetic_rtt_matches_the_planning_medians() {
        let inv = Inventory::synthetic(1_000, 3, 1000);
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
    fn small_network_scenarios_run_and_are_deterministic() {
        let model = ModelSpec::kimi_k26_int4();
        let scenarios = standard_scenarios(7, &[130]);
        let a: Vec<_> = scenarios.iter().map(|s| run(s, &model)).collect();
        let b: Vec<_> = scenarios.iter().map(|s| run(s, &model)).collect();
        assert_eq!(a, b);
        for r in &a {
            assert_eq!(r.churn.survived + r.churn.dissolved, r.churn.islands);
            for row in &r.rows {
                assert!(row.projection.single.tok_s > 0.0);
                assert!(row.projection.speculative.tok_s >= row.projection.single.tok_s);
            }
        }
        let md = markdown(&a, 7);
        assert!(md.contains("| 130 |"));
    }
}
