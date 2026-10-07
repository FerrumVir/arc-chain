//! The device descriptor: what formation knows about one machine.
//!
//! Hardware facts come from the Proof Kit's `arc.proof-result.v1` `island`
//! object (PR #149): memory class, unified memory, GPU memory class,
//! Thunderbolt 5 and download class. The class lists below mirror
//! `crates/arc-inference/src/modern/proof.rs` on that branch; a result whose
//! values are not on these lists is refused. Link quality comes from
//! measured RTT probes ([`LinkStats`]), never from self-reported location.
//!
//! Consent follows #138: `NodeConfig.compute_consent` is `None` until the
//! owner answers, and only `Some(true)` counts. An island additionally needs
//! its own explicit answer, because island members hold model shards and
//! see activations for other people's requests. A grant is bound to one
//! owner and one device and expires; the embedding application
//! authenticates it (this library does not).
//!
//! Every device and link carries [`Evidence`]: whether it was measured or
//! synthesised, and when. Formation may run on synthetic inputs (the
//! simulator does), but an island built from them can never serve, and a
//! serving island re-checks that its inputs are measured and fresh before
//! every admission.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// `arc.proof-result.v1` (PR #149).
pub const PROOF_RESULT_SCHEMA: &str = "arc.proof-result.v1";
/// Memory classes in GB (largest class not above the measured value).
pub const MEMORY_CLASSES_GB: [u32; 17] = [
    1, 2, 4, 8, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024,
];
/// GPU memory classes in GB.
pub const VRAM_CLASSES_GB: [u32; 18] = [
    1, 2, 4, 6, 8, 10, 12, 16, 20, 24, 32, 40, 48, 64, 80, 96, 128, 192,
];
/// Download-throughput classes in Mb/s.
pub const NETWORK_CLASSES_MBPS: [u32; 11] = [1, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000];

const GB: u64 = 1_000_000_000;

/// The Proof Kit's coarse island facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IslandFacts {
    pub memory_class_gb: Option<u32>,
    pub unified_memory: bool,
    pub gpu_vram_class_gb: Option<u32>,
    pub thunderbolt5: Option<bool>,
    pub download_mbps_class: Option<u32>,
}

/// What formation takes from one Proof Kit result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofFacts {
    pub island: IslandFacts,
    /// Every backend in the result matched the published golden digest.
    pub golden_match: bool,
    pub golden_digest: String,
}

/// Why a Proof Kit result cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsError {
    WrongSchema(String),
    MissingIsland,
    Field(&'static str),
}

impl std::fmt::Display for FactsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongSchema(s) => write!(f, "schema is {s:?}, expected {PROOF_RESULT_SCHEMA}"),
            Self::MissingIsland => write!(f, "the result carries no island facts"),
            Self::Field(name) => write!(f, "island.{name} is missing or not an allowed value"),
        }
    }
}

impl std::error::Error for FactsError {}

fn class(value: &Value, classes: &[u32], name: &'static str) -> Result<Option<u32>, FactsError> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| classes.contains(n))
        .map(Some)
        .ok_or(FactsError::Field(name))
}

impl ProofFacts {
    /// Reads the island facts and the verdict from an `arc.proof-result.v1`
    /// document. Run the Proof Kit's full validator first; this checks only
    /// the fields formation uses.
    pub fn from_proof_result(result: &Value) -> Result<Self, FactsError> {
        let schema = result["schema"].as_str().unwrap_or_default();
        if schema != PROOF_RESULT_SCHEMA {
            return Err(FactsError::WrongSchema(schema.to_string()));
        }
        let island = &result["island"];
        if !island.is_object() {
            return Err(FactsError::MissingIsland);
        }
        let unified_memory = island["unified_memory"]
            .as_bool()
            .ok_or(FactsError::Field("unified_memory"))?;
        let thunderbolt5 = match &island["thunderbolt5"] {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            _ => return Err(FactsError::Field("thunderbolt5")),
        };
        let facts = IslandFacts {
            memory_class_gb: class(
                &island["memory_class_gb"],
                &MEMORY_CLASSES_GB,
                "memory_class_gb",
            )?,
            unified_memory,
            gpu_vram_class_gb: class(
                &island["gpu_vram_class_gb"],
                &VRAM_CLASSES_GB,
                "gpu_vram_class_gb",
            )?,
            thunderbolt5,
            download_mbps_class: class(
                &island["download_mbps_class"],
                &NETWORK_CLASSES_MBPS,
                "download_mbps_class",
            )?,
        };
        Ok(Self {
            island: facts,
            golden_match: result["verdict"].as_str() == Some("MATCH"),
            golden_digest: result["golden"]["digest"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        })
    }
}

/// Whether an input was measured or synthesised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Provenance {
    /// Measured on the real machine or link.
    Measured,
    /// Assumed, modelled or generated (simulation, class defaults).
    Synthetic,
}

/// Provenance plus the time of measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub provenance: Provenance,
    pub measured_at_ms: u64,
}

impl Evidence {
    pub fn measured(at_ms: u64) -> Self {
        Self {
            provenance: Provenance::Measured,
            measured_at_ms: at_ms,
        }
    }

    pub fn synthetic(at_ms: u64) -> Self {
        Self {
            provenance: Provenance::Synthetic,
            measured_at_ms: at_ms,
        }
    }

    /// Not from the future and no older than `ttl_ms`.
    pub fn fresh(&self, now_ms: u64, ttl_ms: u64) -> bool {
        now_ms >= self.measured_at_ms && now_ms - self.measured_at_ms <= ttl_ms
    }

    /// Fresh, and measured unless synthetic inputs are allowed.
    pub fn acceptable(&self, now_ms: u64, ttl_ms: u64, allow_synthetic: bool) -> bool {
        self.fresh(now_ms, ttl_ms) && (allow_synthetic || self.provenance == Provenance::Measured)
    }
}

/// How old evidence may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Freshness {
    /// Device facts, measured memory and bandwidth, consent re-read.
    pub device_ttl_ms: u64,
    /// Link probes.
    pub link_ttl_ms: u64,
}

impl Default for Freshness {
    /// Devices: one epoch (1 h, research-7 §6.2). Links: three probe periods
    /// (15 min; research-7 probes every 5 min).
    fn default() -> Self {
        Self {
            device_ttl_ms: 3_600_000,
            link_ttl_ms: 900_000,
        }
    }
}

/// The owner's answers (#138), bound to one owner and device, with an
/// expiry. `None` means never asked, which is a no.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consent {
    pub owner: String,
    pub device_id: String,
    /// #138's `compute_consent`: "Let ARC run inference jobs on this computer".
    pub compute: Option<bool>,
    /// The island question: hold a model shard and serve with other machines.
    pub island: Option<bool>,
    /// The grant lapses at this time; the owner must answer again.
    pub expires_at_ms: u64,
}

impl Consent {
    /// A yes to both questions for `owner`'s `device_id` until `expires_at_ms`.
    pub fn grant(owner: &str, device_id: &str, expires_at_ms: u64) -> Self {
        Self {
            owner: owner.into(),
            device_id: device_id.into(),
            compute: Some(true),
            island: Some(true),
            expires_at_ms,
        }
    }

    /// The owner withdrew the island answer.
    pub fn withdraw(&mut self) {
        self.island = Some(false);
    }

    /// Both answers are an explicit yes, the grant names this device and its
    /// owner, and it has not expired.
    pub fn permits(&self, device: &DeviceDescriptor, now_ms: u64) -> bool {
        self.owner == device.owner
            && self.device_id == device.device_id
            && self.compute == Some(true)
            && self.island == Some(true)
            && now_ms < self.expires_at_ms
    }
}

/// A measured RDMA and collective microbenchmark (research-6 §6.3: an
/// all-reduce of 28 KiB × 122 iterations). T1a needs it on every member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdmaEvidence {
    pub rdma_up: bool,
    pub collective_p99_us: u32,
    pub evidence: Evidence,
}

/// Coarse location labels, used to name cells and to order the search
/// (metro first). Membership is decided by measured RTT, never by labels.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Location {
    pub continent: String,
    pub region: String,
    pub zone: String,
    pub metro: String,
}

/// Values a qualification run measured; they replace the class-based
/// assumptions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measured {
    pub usable_memory_bytes: Option<u64>,
    pub bandwidth_mb_s: Option<u64>,
    pub uplink_mbps: Option<u32>,
}

/// One machine as formation sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceDescriptor {
    pub device_id: String,
    /// The owner (or declared family); one owner is one identity for
    /// diversity rules.
    pub owner: String,
    pub consent: Consent,
    pub facts: IslandFacts,
    /// The device reproduced the golden digest on the engine it would serve
    /// with.
    pub golden_qualified: bool,
    pub measured: Measured,
    /// Owner-declared LAN site; machines on one site may form a T1 island.
    pub site: Option<String>,
    pub location: Location,
    /// Share of time online, from history (‰).
    pub availability_permille: u16,
    /// Provenance of the facts and `measured` values.
    pub evidence: Evidence,
    /// Measured RDMA/collective result inside the device's LAN site.
    pub rdma: Option<RdmaEvidence>,
}

/// Where a device's model memory lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PoolKind {
    /// Discrete GPU memory.
    Gpu,
    /// Unified memory (Apple silicon, Strix Halo, GB10).
    Unified,
    /// System RAM on the CPU.
    Cpu,
}

/// The memory and bandwidth one stage can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputePool {
    pub kind: PoolKind,
    pub usable_bytes: u64,
    pub bandwidth_mb_s: u64,
    /// True when the figure is the class assumption, not a measurement.
    pub memory_assumed: bool,
    pub bandwidth_assumed: bool,
}

/// Usable share of memory for weights and KV (research-6 §2.2): 85% of GPU
/// memory, 80% of unified or system memory.
pub const USABLE_PERMILLE_GPU: u64 = 850;
pub const USABLE_PERMILLE_UNIFIED: u64 = 800;

/// Effective decode bandwidth by class, MB/s \[ASSUMPTION\]. llama.cpp-class
/// figures from research-7 §2.6 (calibrated as research-6 §2.5: RTX 4090
/// 711 GB/s, RTX 5090 1,108 GB/s, M3 Ultra 456 GB/s). ARC's engine has not
/// yet been measured at these rates; a measured value replaces them.
pub fn assumed_bandwidth_mb_s(kind: PoolKind, class_gb: u32) -> u64 {
    match kind {
        PoolKind::Gpu => match class_gb {
            0..=8 => 250_000,
            9..=12 => 300_000,
            13..=16 => 450_000,
            17..=24 => 680_000,
            _ => 1_108_000,
        },
        PoolKind::Unified => match class_gb {
            0..=24 => 90_000,
            25..=192 => 300_000,
            _ => 456_000,
        },
        PoolKind::Cpu => 60_000,
    }
}

impl DeviceDescriptor {
    /// The memory pool a stage would use: GPU memory on a discrete GPU,
    /// otherwise unified or system memory. `None` when the facts carry no
    /// memory class.
    pub fn pool(&self) -> Option<ComputePool> {
        let f = &self.facts;
        let (kind, class_gb, permille) = match (f.unified_memory, f.gpu_vram_class_gb) {
            (false, Some(vram)) => (PoolKind::Gpu, vram, USABLE_PERMILLE_GPU),
            (true, _) => (
                PoolKind::Unified,
                f.memory_class_gb?,
                USABLE_PERMILLE_UNIFIED,
            ),
            (false, None) => (PoolKind::Cpu, f.memory_class_gb?, USABLE_PERMILLE_UNIFIED),
        };
        let assumed_bytes = u64::from(class_gb) * GB * permille / 1000;
        Some(ComputePool {
            kind,
            usable_bytes: self.measured.usable_memory_bytes.unwrap_or(assumed_bytes),
            bandwidth_mb_s: self
                .measured
                .bandwidth_mb_s
                .unwrap_or_else(|| assumed_bandwidth_mb_s(kind, class_gb)),
            memory_assumed: self.measured.usable_memory_bytes.is_none(),
            bandwidth_assumed: self.measured.bandwidth_mb_s.is_none(),
        })
    }

    /// Thunderbolt 5 present (reported only on Macs).
    pub fn thunderbolt5(&self) -> bool {
        self.facts.thunderbolt5 == Some(true)
    }

    /// Inputs good enough for formation at `now_ms`: fresh, and either
    /// measured (memory and bandwidth included) or synthetic inputs allowed.
    pub fn inputs_acceptable(&self, now_ms: u64, fresh: &Freshness, allow_synthetic: bool) -> bool {
        if !self
            .evidence
            .acceptable(now_ms, fresh.device_ttl_ms, allow_synthetic)
        {
            return false;
        }
        allow_synthetic
            || self
                .pool()
                .is_some_and(|p| !p.memory_assumed && !p.bandwidth_assumed)
    }

    /// Effective provenance: synthetic if the facts are synthetic or memory
    /// or bandwidth is a class assumption.
    pub fn provenance(&self) -> Provenance {
        let assumed = self
            .pool()
            .is_none_or(|p| p.memory_assumed || p.bandwidth_assumed);
        if self.evidence.provenance == Provenance::Synthetic || assumed {
            Provenance::Synthetic
        } else {
            Provenance::Measured
        }
    }
}

/// Summary of direct probes on one link (research-6 §6.3, research-7 §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkStats {
    pub p50_us: u32,
    pub p95_us: u32,
    pub p99_us: u32,
    /// Lost samples per thousand.
    pub loss_permille: u16,
    pub samples: u32,
    pub evidence: Evidence,
}

impl LinkStats {
    /// Summarises raw probe samples in microseconds taken at `at_ms`; `None`
    /// is a lost probe. Percentiles are nearest-rank on the received samples.
    pub fn from_samples(samples: &[Option<u32>], at_ms: u64) -> Option<Self> {
        let mut got: Vec<u32> = samples.iter().flatten().copied().collect();
        if got.is_empty() {
            return None;
        }
        got.sort_unstable();
        let rank = |p: usize| got[((p * got.len()).div_ceil(100)).clamp(1, got.len()) - 1];
        let lost = samples.len() - got.len();
        Some(Self {
            p50_us: rank(50),
            p95_us: rank(95),
            p99_us: rank(99),
            loss_permille: (lost * 1000 / samples.len()) as u16,
            samples: samples.len() as u32,
            evidence: Evidence::measured(at_ms),
        })
    }

    /// Pipeline qualification (research-7 §6.4 step 4): p99 ≤ 2 × p50 and
    /// loss ≤ 0.5%.
    pub fn pipeline_qualified(&self) -> bool {
        u64::from(self.p99_us) <= 2 * u64::from(self.p50_us) && self.loss_permille <= 5
    }
}

/// Measured links between devices, by index into the device list.
pub trait RttSource {
    fn link(&self, a: usize, b: usize) -> Option<LinkStats>;
}

/// Links stored explicitly (symmetric).
#[derive(Debug, Clone, Default)]
pub struct RttMatrix {
    links: BTreeMap<(usize, usize), LinkStats>,
}

impl RttMatrix {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, a: usize, b: usize, stats: LinkStats) {
        self.links.insert((a.min(b), a.max(b)), stats);
    }
}

impl RttSource for RttMatrix {
    fn link(&self, a: usize, b: usize) -> Option<LinkStats> {
        if a == b {
            return Some(LinkStats {
                p50_us: 0,
                p95_us: 0,
                p99_us: 0,
                loss_permille: 0,
                samples: 0,
                evidence: Evidence::measured(0),
            });
        }
        self.links.get(&(a.min(b), a.max(b))).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(island: Value) -> Value {
        json!({
            "schema": "arc.proof-result.v1",
            "verdict": "MATCH",
            "golden": {"digest": "3e43f342"},
            "island": island,
        })
    }

    #[test]
    fn reads_proof_kit_island_facts() {
        let facts = ProofFacts::from_proof_result(&result(json!({
            "memory_class_gb": 512,
            "unified_memory": true,
            "gpu_vram_class_gb": null,
            "thunderbolt5": true,
            "download_mbps_class": 1000,
        })))
        .unwrap();
        assert!(facts.golden_match);
        assert_eq!(facts.island.memory_class_gb, Some(512));
        assert_eq!(facts.island.thunderbolt5, Some(true));
    }

    #[test]
    fn refuses_values_off_the_class_lists() {
        let bad = result(json!({
            "memory_class_gb": 500,
            "unified_memory": true,
            "gpu_vram_class_gb": null,
            "thunderbolt5": null,
            "download_mbps_class": null,
        }));
        assert_eq!(
            ProofFacts::from_proof_result(&bad),
            Err(FactsError::Field("memory_class_gb"))
        );
        let mut wrong = bad.clone();
        wrong["schema"] = json!("arc.proof-result.v0");
        assert!(matches!(
            ProofFacts::from_proof_result(&wrong),
            Err(FactsError::WrongSchema(_))
        ));
    }

    fn gpu_device() -> DeviceDescriptor {
        DeviceDescriptor {
            device_id: "a".into(),
            owner: "o".into(),
            consent: Consent::grant("o", "a", 10_000),
            facts: IslandFacts {
                memory_class_gb: Some(64),
                unified_memory: false,
                gpu_vram_class_gb: Some(24),
                thunderbolt5: None,
                download_mbps_class: None,
            },
            golden_qualified: true,
            measured: Measured::default(),
            site: None,
            location: Location::default(),
            availability_permille: 1000,
            evidence: Evidence::measured(0),
            rdma: None,
        }
    }

    #[test]
    fn consent_needs_two_yes_answers_bound_to_owner_and_device_until_expiry() {
        let d = gpu_device();
        assert!(d.consent.permits(&d, 0));
        assert!(!d.consent.permits(&d, 10_000), "expired at 10 s");
        assert!(!Consent::default().permits(&d, 0), "never asked");
        let mut c = Consent::grant("o", "a", 10_000);
        c.island = None;
        assert!(!c.permits(&d, 0));
        let mut c = Consent::grant("o", "a", 10_000);
        c.compute = Some(false);
        assert!(!c.permits(&d, 0));
        assert!(
            !Consent::grant("someone-else", "a", 10_000).permits(&d, 0),
            "other owner"
        );
        assert!(
            !Consent::grant("o", "b", 10_000).permits(&d, 0),
            "other device"
        );
        let mut c = Consent::grant("o", "a", 10_000);
        c.withdraw();
        assert!(!c.permits(&d, 0));
    }

    #[test]
    fn provenance_and_freshness_gate_inputs() {
        let mut d = gpu_device();
        let fresh = Freshness::default();
        // Class-assumed memory and bandwidth are synthetic even with
        // measured facts.
        assert_eq!(d.provenance(), Provenance::Synthetic);
        assert!(!d.inputs_acceptable(0, &fresh, false));
        assert!(d.inputs_acceptable(0, &fresh, true));
        d.measured.usable_memory_bytes = Some(20_000_000_000);
        d.measured.bandwidth_mb_s = Some(650_000);
        assert_eq!(d.provenance(), Provenance::Measured);
        assert!(d.inputs_acceptable(fresh.device_ttl_ms, &fresh, false));
        assert!(
            !d.inputs_acceptable(fresh.device_ttl_ms + 1, &fresh, false),
            "stale"
        );
        d.evidence = Evidence::synthetic(0);
        assert_eq!(d.provenance(), Provenance::Synthetic);
        assert!(!d.inputs_acceptable(0, &fresh, false));
        d.evidence = Evidence::measured(5_000);
        assert!(
            !d.inputs_acceptable(4_000, &fresh, false),
            "from the future"
        );
    }

    #[test]
    fn pools_follow_the_usable_fractions() {
        let mut d = gpu_device();
        let gpu = d.pool().unwrap();
        assert_eq!(gpu.kind, PoolKind::Gpu);
        assert_eq!(gpu.usable_bytes, 20_400_000_000);
        assert!(gpu.bandwidth_assumed);
        d.measured.bandwidth_mb_s = Some(700_000);
        assert_eq!(d.pool().unwrap().bandwidth_mb_s, 700_000);
        d.facts.unified_memory = true;
        d.facts.gpu_vram_class_gb = None;
        assert_eq!(d.pool().unwrap().usable_bytes, 51_200_000_000);
    }

    #[test]
    fn link_stats_from_samples() {
        let mut samples: Vec<Option<u32>> = (1..=200).map(Some).collect();
        samples[0] = None;
        let s = LinkStats::from_samples(&samples, 0).unwrap();
        assert_eq!(s.p50_us, 101);
        assert_eq!(s.p99_us, 199);
        assert_eq!(s.loss_permille, 5);
        assert!(s.pipeline_qualified());
        let mut tail: Vec<Option<u32>> = vec![Some(10_000); 190];
        tail.extend([Some(25_000); 10]);
        let s = LinkStats::from_samples(&tail, 0).unwrap();
        assert_eq!((s.p50_us, s.p95_us, s.p99_us), (10_000, 10_000, 25_000));
        assert!(!s.pipeline_qualified(), "p99 > 2 × p50 fails");
        samples[1] = None;
        assert!(
            !LinkStats::from_samples(&samples, 0)
                .unwrap()
                .pipeline_qualified(),
            "1% loss fails"
        );
        assert!(LinkStats::from_samples(&[None, None], 0).is_none());
    }
}
