use crate::placement::{Participant, Placement};
use crate::{Hash256, digest};
use serde::{Deserialize, Serialize};

pub const PLAN_DOMAIN: &str = "ARC-assign-verification-plan-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRule {
    pub duplicate_per_mille: u32,
    pub spot_rows_per_stage: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Duplicate {
    pub stage: usize,
    pub slice: usize,
    pub checker: Participant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationPlan {
    pub duplicates: Vec<Duplicate>,
    pub spot_rows: Vec<(usize, u64)>,
}

struct Stream {
    seed: Hash256,
    counter: u64,
}

impl Stream {
    fn next(&mut self) -> u64 {
        let h = digest(PLAN_DOMAIN, &(self.seed, self.counter));
        self.counter += 1;
        u64::from_le_bytes(h.0[..8].try_into().expect("8 bytes"))
    }
}

pub fn plan(seed: Hash256, placement: &Placement, rule: &VerificationRule) -> VerificationPlan {
    let mut stream = Stream { seed, counter: 0 };
    let mut duplicates = Vec::new();
    let mut spot_rows = Vec::new();
    for plan in &placement.stages {
        for (index, slice) in plan.slices.iter().enumerate() {
            if slice.participant == Participant::Coordinator {
                continue;
            }
            if stream.next() % 1000 < rule.duplicate_per_mille as u64 {
                let checker = plan
                    .slices
                    .iter()
                    .map(|s| s.participant.clone())
                    .find(|p| *p != slice.participant && *p != Participant::Coordinator)
                    .unwrap_or(Participant::Coordinator);
                duplicates.push(Duplicate {
                    stage: plan.stage,
                    slice: index,
                    checker,
                });
            }
        }
        let rows: u64 = plan.slices.last().map(|s| s.row_end).unwrap_or(0);
        if rows > 0 {
            for _ in 0..rule.spot_rows_per_stage {
                spot_rows.push((plan.stage, stream.next() % rows));
            }
        }
    }
    VerificationPlan {
        duplicates,
        spot_rows,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Finding {
    Agrees,
    Fault {
        stage: usize,
        participant: Participant,
        expected_digest: Hash256,
        found_digest: Hash256,
    },
}

pub fn compare(
    stage: usize,
    participant: Participant,
    expected_digest: Hash256,
    found_digest: Hash256,
) -> Finding {
    if expected_digest == found_digest {
        Finding::Agrees
    } else {
        Finding::Fault {
            stage,
            participant,
            expected_digest,
            found_digest,
        }
    }
}
