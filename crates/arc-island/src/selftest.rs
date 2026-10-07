//! The qualification self-test (research-6 §6.4 "qualify", §4.2).
//!
//! An island runs the golden prompts stage by stage, exactly as it will
//! serve, and reports the hash of the residual leaving every stage plus the
//! output digest. Because the engine is exact integer arithmetic, the
//! residual after any layer unit is the same bytes whatever the partition,
//! so the reference publishes a digest for every unit boundary and a
//! mismatch names the first faulty stage with no thresholds.
//!
//! **Trust.** A reference is only trusted if it comes from [`GoldenReference::pinned`]:
//! a table compiled into this crate, keyed by the exact model checkpoint and
//! integer profile ([`ModelIdentity`]). Callers cannot construct a measured
//! reference; [`GoldenReference::synthetic`] exists for simulation and tests
//! and can never qualify serving. The table has **no Kimi K2.6 entry**,
//! because no real K2.6 run exists yet (#156 is a draft), so qualifying a
//! real Kimi island fails closed until a reviewed change pins one.
//!
//! The real executor runs the integer engine and plugs in through
//! [`GoldenExecutor`]. The toy pipeline (`toy`, only under `cfg(test)` or
//! the `simulator` feature) is an exact integer stand-in whose runs are
//! always synthetic.

use crate::device::{DeviceDescriptor, Provenance};
use crate::form::IslandPlan;
use crate::model::ModelIdentity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What one self-test run reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfTestRun {
    /// The model and profile the executor actually ran.
    pub identity: ModelIdentity,
    /// Digest of the residual leaving each stage, in stage order.
    pub stage_boundaries: Vec<String>,
    pub output_digest: String,
}

/// A golden reference: the output digest and the residual digest after every
/// layer unit (key = units completed), for one model identity. Fields are
/// private so that a measured reference can only come from the pinned table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoldenReference {
    identity: ModelIdentity,
    output_digest: String,
    boundary_after_unit: BTreeMap<usize, String>,
    provenance: Provenance,
}

/// One pinned reference.
struct PinnedGolden {
    checkpoint: &'static str,
    quant_profile: &'static str,
    output_digest: &'static str,
    boundary_after_unit: &'static [(usize, &'static str)],
}

/// The trusted references. A real one is added only by a reviewed change
/// that cites the CI run which produced it. Empty today: there is no real
/// Kimi K2.6 golden run.
const PINNED: &[PinnedGolden] = &[];

impl GoldenReference {
    /// The pinned reference for exactly this model and profile, if any.
    pub fn pinned(identity: &ModelIdentity) -> Option<Self> {
        if let Some(p) = PINNED.iter().find(|p| {
            p.checkpoint == identity.checkpoint && p.quant_profile == identity.quant_profile
        }) {
            return Some(Self {
                identity: identity.clone(),
                output_digest: p.output_digest.into(),
                boundary_after_unit: p
                    .boundary_after_unit
                    .iter()
                    .map(|(u, d)| (*u, (*d).into()))
                    .collect(),
                provenance: Provenance::Measured,
            });
        }
        #[cfg(test)]
        if let Some(g) = test_pins::pinned(identity) {
            return Some(g);
        }
        None
    }

    /// A synthetic reference (simulation and tests). It can never qualify
    /// serving.
    pub fn synthetic(
        identity: ModelIdentity,
        output_digest: String,
        boundary_after_unit: BTreeMap<usize, String>,
    ) -> Self {
        Self {
            identity,
            output_digest,
            boundary_after_unit,
            provenance: Provenance::Synthetic,
        }
    }

    pub fn identity(&self) -> &ModelIdentity {
        &self.identity
    }

    pub fn output_digest(&self) -> &str {
        &self.output_digest
    }

    pub fn provenance(&self) -> Provenance {
        self.provenance
    }
}

/// Runs the golden prompts through an island.
pub trait GoldenExecutor {
    /// Synthetic executors (the toy pipeline, simulators) can never qualify
    /// serving.
    fn provenance(&self) -> Provenance;
    fn run(&mut self, plan: &IslandPlan, devices: &[DeviceDescriptor]) -> SelfTestRun;
}

/// The self-test verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Match,
    /// The run, the reference or the plan are for different models.
    WrongModel,
    /// `first_faulty_stage` is the first stage whose output differs from the
    /// reference while its input matched; `None` if it cannot be located.
    Mismatch {
        first_faulty_stage: Option<usize>,
    },
}

/// Compares a run with the reference for `plan`'s model.
pub fn judge(plan: &IslandPlan, run: &SelfTestRun, golden: &GoldenReference) -> Verdict {
    if run.identity != golden.identity || plan.model != golden.identity {
        return Verdict::WrongModel;
    }
    let boundary = |s: &crate::fit::Stage| golden.boundary_after_unit.get(&s.layers.end);
    if run.output_digest == golden.output_digest
        && run.stage_boundaries.len() == plan.stages.len()
        && plan
            .stages
            .iter()
            .zip(&run.stage_boundaries)
            .all(|(s, d)| boundary(s) == Some(d))
    {
        return Verdict::Match;
    }
    let first_faulty_stage = plan
        .stages
        .iter()
        .zip(&run.stage_boundaries)
        .position(|(s, d)| boundary(s) != Some(d));
    Verdict::Mismatch { first_faulty_stage }
}

/// An exact integer stand-in for the engine: tests and simulation only.
#[cfg(any(test, feature = "simulator"))]
pub mod toy {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;

    const WIDTH: usize = 16;
    const MASK: i64 = (1 << 31) - 1;

    fn splitmix(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }

    /// A small exact integer pipeline: each unit mixes a 16-lane state with
    /// per-unit constants modulo 2^31.
    #[derive(Debug, Clone, Default)]
    pub struct ToyPipeline {
        /// Devices (by id) that corrupt their stage's output, to test fault
        /// location.
        pub faulty: BTreeSet<String>,
    }

    impl ToyPipeline {
        fn input() -> [i64; WIDTH] {
            let mut x = [0i64; WIDTH];
            for (i, v) in x.iter_mut().enumerate() {
                *v = (splitmix(i as u64) as i64) & MASK;
            }
            x
        }

        fn unit(u: usize, x: &mut [i64; WIDTH]) {
            let a = (splitmix(u as u64 * 3 + 1) as i64 & 0xFFFF) | 1;
            let b = splitmix(u as u64 * 3 + 2) as i64 & 0xFFFF;
            let c = splitmix(u as u64 * 3 + 3) as i64 & MASK;
            let prev = *x;
            for i in 0..WIDTH {
                x[i] = (prev[i] * a + prev[(i + 1) % WIDTH] * b + c) & MASK;
            }
        }

        fn digest(x: &[i64; WIDTH]) -> String {
            let mut h = Sha256::new();
            for v in x {
                h.update(v.to_le_bytes());
            }
            hex::encode(h.finalize())
        }

        /// The synthetic reference for `identity` with `units` layer units,
        /// from one whole-model run.
        pub fn golden(identity: &ModelIdentity, units: usize) -> GoldenReference {
            let (output, boundaries) = Self::reference(units);
            GoldenReference::synthetic(identity.clone(), output, boundaries)
        }

        pub(crate) fn reference(units: usize) -> (String, BTreeMap<usize, String>) {
            let mut x = Self::input();
            let mut boundary_after_unit = BTreeMap::new();
            for u in 0..units {
                Self::unit(u, &mut x);
                boundary_after_unit.insert(u + 1, Self::digest(&x));
            }
            (Self::digest(&x), boundary_after_unit)
        }

        /// Runs the plan stage by stage.
        pub fn execute(&self, plan: &IslandPlan, devices: &[DeviceDescriptor]) -> SelfTestRun {
            let mut x = Self::input();
            let mut stage_boundaries = Vec::with_capacity(plan.stages.len());
            for (stage, &member) in plan.stages.iter().zip(&plan.members) {
                for u in stage.layers.clone() {
                    Self::unit(u, &mut x);
                }
                if self.faulty.contains(&devices[member].device_id) {
                    x[0] ^= 1;
                }
                stage_boundaries.push(Self::digest(&x));
            }
            SelfTestRun {
                identity: plan.model.clone(),
                stage_boundaries,
                output_digest: Self::digest(&x),
            }
        }
    }

    impl GoldenExecutor for ToyPipeline {
        fn provenance(&self) -> Provenance {
            Provenance::Synthetic
        }

        fn run(&mut self, plan: &IslandPlan, devices: &[DeviceDescriptor]) -> SelfTestRun {
            self.execute(plan, devices)
        }
    }
}

/// Test-only pins and a test-only "measured" executor, so the real serving
/// path can be exercised without a real model. Compiled only under `cfg(test)`.
#[cfg(test)]
pub(crate) mod test_pins {
    use super::toy::ToyPipeline;
    use super::*;

    /// Pins the toy reference for `test/toy-units-N` / `toy` identities.
    pub fn pinned(identity: &ModelIdentity) -> Option<GoldenReference> {
        let units: usize = identity
            .checkpoint
            .strip_prefix("test/toy-units-")?
            .parse()
            .ok()?;
        if identity.quant_profile != "toy" {
            return None;
        }
        let (output_digest, boundary_after_unit) = ToyPipeline::reference(units);
        Some(GoldenReference {
            identity: identity.clone(),
            output_digest,
            boundary_after_unit,
            provenance: Provenance::Measured,
        })
    }

    /// The toy pipeline standing in for a real, measured executor.
    #[derive(Debug, Clone, Default)]
    pub struct MeasuredToy(pub ToyPipeline);

    impl GoldenExecutor for MeasuredToy {
        fn provenance(&self) -> Provenance {
            Provenance::Measured
        }

        fn run(&mut self, plan: &IslandPlan, devices: &[DeviceDescriptor]) -> SelfTestRun {
            self.0.execute(plan, devices)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::toy::ToyPipeline;
    use super::*;
    use crate::fit::{StageCapacity, partition};
    use crate::form::tests::{device, mesh};
    use crate::form::{FormationPolicy, form};
    use crate::model::ModelSpec;

    #[test]
    fn every_partition_gives_the_golden_digest() {
        let model = ModelSpec::uniform("u", 24, 1_000_000_000, 0);
        let golden = GoldenReference::pinned(&model.identity).unwrap();
        let devices: Vec<_> = (0..8)
            .map(|i| device(&format!("d{i}"), 64, false, None, "NYC"))
            .collect();
        let rtt = mesh(8, 5_000, &[]);
        let out = form(&devices, &rtt, &model, &FormationPolicy::batch(0), 0);
        let mut plan = out.islands[0].clone();
        for split in [vec![10u64, 10, 10], vec![2, 30, 3], vec![24], vec![1; 24]] {
            let caps: Vec<_> = split
                .iter()
                .map(|gb| StageCapacity {
                    usable_bytes: gb * 1_000_000_000,
                    bandwidth_mb_s: 1000,
                })
                .collect();
            plan.stages = partition(&model, &caps, 0).unwrap();
            plan.members = (0..plan.stages.len()).map(|i| i % devices.len()).collect();
            let run = ToyPipeline::default().execute(&plan, &devices);
            assert_eq!(run.output_digest, golden.output_digest(), "split {split:?}");
            assert_eq!(judge(&plan, &run, &golden), Verdict::Match);
        }
    }

    #[test]
    fn a_faulty_member_is_located() {
        let model = ModelSpec::uniform("u", 12, 1_000_000_000, 0);
        let golden = GoldenReference::pinned(&model.identity).unwrap();
        let devices: Vec<_> = (0..4)
            .map(|i| device(&format!("d{i}"), 8, false, None, "NYC"))
            .collect();
        let rtt = mesh(4, 5_000, &[]);
        let plan = form(&devices, &rtt, &model, &FormationPolicy::batch(0), 0).islands[0].clone();
        assert_eq!(plan.members.len(), 3);
        let bad = devices[plan.members[1]].device_id.clone();
        let toy = ToyPipeline {
            faulty: [bad].into_iter().collect(),
        };
        let run = toy.execute(&plan, &devices);
        assert_eq!(
            judge(&plan, &run, &golden),
            Verdict::Mismatch {
                first_faulty_stage: Some(1)
            }
        );
    }

    #[test]
    fn kimi_has_no_pinned_golden_and_identities_must_match() {
        let kimi = ModelSpec::kimi_k26_int4();
        assert!(
            GoldenReference::pinned(&kimi.identity).is_none(),
            "no real K2.6 run exists"
        );
        // A profile change is a different model.
        let mut other = ModelSpec::uniform("u", 12, 1_000_000_000, 0).identity;
        assert!(GoldenReference::pinned(&other).is_some());
        other.quant_profile = "toy-v2".into();
        assert!(GoldenReference::pinned(&other).is_none());
        // A reference for another model never matches.
        let model = ModelSpec::uniform("u", 12, 1_000_000_000, 0);
        let devices: Vec<_> = (0..4)
            .map(|i| device(&format!("d{i}"), 8, false, None, "NYC"))
            .collect();
        let plan = form(
            &devices,
            &mesh(4, 5_000, &[]),
            &model,
            &FormationPolicy::batch(0),
            0,
        )
        .islands[0]
            .clone();
        let run = ToyPipeline::default().execute(&plan, &devices);
        let wrong = ToyPipeline::golden(&kimi.identity, 12);
        assert_eq!(judge(&plan, &run, &wrong), Verdict::WrongModel);
        assert_eq!(
            ToyPipeline::golden(&model.identity, 12).provenance(),
            Provenance::Synthetic
        );
    }
}
