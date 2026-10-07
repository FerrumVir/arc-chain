//! The qualification self-test (research-6 §6.4 "qualify", §4.2).
//!
//! An island runs the golden prompts stage by stage, exactly as it will
//! serve, and reports the hash of the residual leaving every stage plus the
//! output digest. The island qualifies only if the output digest equals the
//! published golden digest. Because the engine is exact integer arithmetic,
//! the residual after any layer unit is the same bytes whatever the
//! partition, so the reference publishes a digest for every unit boundary
//! and a mismatch names the first faulty stage with no thresholds.
//!
//! The real Kimi self-test runs the integer engine (#156); it plugs in
//! through [`SelfTest`]. [`ToyPipeline`] is a small exact integer pipeline
//! used by the tests and the simulator to exercise the same contract.

use crate::device::DeviceDescriptor;
use crate::form::IslandPlan;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// What one self-test run reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfTestRun {
    /// Digest of the residual leaving each stage, in stage order.
    pub stage_boundaries: Vec<String>,
    pub output_digest: String,
}

/// The published reference: the output digest and the residual digest after
/// every layer unit (key = units completed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenReference {
    pub output_digest: String,
    pub boundary_after_unit: BTreeMap<usize, String>,
}

/// Runs the golden prompts through an island.
pub trait SelfTest {
    fn run(&mut self, plan: &IslandPlan, devices: &[DeviceDescriptor]) -> SelfTestRun;
}

/// The self-test verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Match,
    /// `first_faulty_stage` is the first stage whose output differs from the
    /// reference while its input matched; `None` if it cannot be located.
    Mismatch {
        first_faulty_stage: Option<usize>,
    },
}

/// Compares a run with the reference.
pub fn judge(plan: &IslandPlan, run: &SelfTestRun, golden: &GoldenReference) -> Verdict {
    if run.output_digest == golden.output_digest && run.stage_boundaries.len() == plan.stages.len()
    {
        let all = plan
            .stages
            .iter()
            .zip(&run.stage_boundaries)
            .all(|(s, d)| golden.boundary_after_unit.get(&s.layers.end) == Some(d));
        if all {
            return Verdict::Match;
        }
    }
    let first_faulty_stage = plan
        .stages
        .iter()
        .zip(&run.stage_boundaries)
        .position(|(s, d)| golden.boundary_after_unit.get(&s.layers.end) != Some(d));
    Verdict::Mismatch { first_faulty_stage }
}

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

    /// The reference for a model of `units` layer units, from one
    /// whole-model run.
    pub fn golden(units: usize) -> GoldenReference {
        let mut x = Self::input();
        let mut boundary_after_unit = BTreeMap::new();
        for u in 0..units {
            Self::unit(u, &mut x);
            boundary_after_unit.insert(u + 1, Self::digest(&x));
        }
        GoldenReference {
            output_digest: Self::digest(&x),
            boundary_after_unit,
        }
    }
}

impl SelfTest for ToyPipeline {
    fn run(&mut self, plan: &IslandPlan, devices: &[DeviceDescriptor]) -> SelfTestRun {
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
            stage_boundaries,
            output_digest: Self::digest(&x),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fit::{StageCapacity, partition};
    use crate::form::tests::{device, mesh};
    use crate::form::{FormationPolicy, form};
    use crate::model::ModelSpec;

    #[test]
    fn every_partition_gives_the_golden_digest() {
        let model = ModelSpec::uniform("u", 24, 1_000_000_000, 0);
        let golden = ToyPipeline::golden(24);
        let devices: Vec<_> = (0..6)
            .map(|i| device(&format!("d{i}"), 64, false, None, "NYC"))
            .collect();
        let rtt = mesh(6, 5_000, &[]);
        let out = form(&devices, &rtt, &model, &FormationPolicy::batch(0));
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
            let run = ToyPipeline::default().run(&plan, &devices);
            assert_eq!(run.output_digest, golden.output_digest, "split {split:?}");
            assert_eq!(judge(&plan, &run, &golden), Verdict::Match);
        }
    }

    #[test]
    fn a_faulty_member_is_located() {
        let model = ModelSpec::uniform("u", 12, 1_000_000_000, 0);
        let golden = ToyPipeline::golden(12);
        let devices: Vec<_> = (0..3)
            .map(|i| device(&format!("d{i}"), 8, false, None, "NYC"))
            .collect();
        let rtt = mesh(3, 5_000, &[]);
        let plan = form(&devices, &rtt, &model, &FormationPolicy::batch(0)).islands[0].clone();
        assert_eq!(plan.members.len(), 3);
        let bad = devices[plan.members[1]].device_id.clone();
        let mut test = ToyPipeline {
            faulty: [bad].into_iter().collect(),
        };
        let run = test.run(&plan, &devices);
        assert_eq!(
            judge(&plan, &run, &golden),
            Verdict::Mismatch {
                first_faulty_stage: Some(1)
            }
        );
    }
}
