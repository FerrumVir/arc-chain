//! Island lifecycle (research-6 §6.4, §6.7): form → qualify → serve →
//! (member lost → promote spare → re-qualify → serve) → dissolve.
//!
//! - **Health.** Members and spares heartbeat every 250 ms; one silent for
//!   more than 1 s is declared failed.
//! - **Consent.** An owner who withdraws consent leaves at once, exactly as if
//!   the machine failed: their device stops serving the island the moment
//!   the answer changes (#138's model).
//! - **Churn.** A failed stage is taken over by the first warm spare that
//!   covers it. The island then re-runs the self-test before it serves
//!   again; in-flight requests continue by KV mirror, activation replay or
//!   re-prefill, which determinism makes byte-identical. With no covering
//!   spare the island dissolves and its members return to the pool.
//! - **Re-optimisation** happens only at lease boundaries and only for a
//!   ≥ 20% predicted gain ([`should_reform`]).

use crate::device::DeviceDescriptor;
use crate::form::IslandPlan;
use crate::selftest::{GoldenReference, SelfTest, Verdict, judge};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const HEARTBEAT_PERIOD_MS: u64 = 250;
/// Silence after which a member is declared failed (research-6 §6.7).
pub const FAILURE_SILENCE_MS: u64 = 1_000;
/// Minimum predicted gain for re-forming a serving island, ‰ (Petals rule).
pub const REFORM_GAIN_PERMILLE: u64 = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DissolveReason {
    QualificationFailed { first_faulty_stage: Option<usize> },
    MemberLostWithoutSpare { stage: usize, device: usize },
    LeaseEnded,
    Requested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Planned; shards loading.
    Forming,
    /// Running the self-test (after forming or after a spare promotion).
    Qualifying,
    Serving,
    Dissolved(DissolveReason),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Qualified,
    MemberFailed { stage: usize, device: usize },
    ConsentWithdrawn { device: usize },
    SparePromoted { stage: usize, device: usize },
    SpareLost { device: usize },
    Dissolved(DissolveReason),
}

/// A running island.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Island {
    pub plan: IslandPlan,
    pub state: State,
    /// Bumped on every membership change.
    pub generation: u32,
    last_seen: BTreeMap<usize, u64>,
    pub events: Vec<(u64, Event)>,
}

impl Island {
    pub fn new(plan: IslandPlan, now_ms: u64) -> Self {
        let last_seen = plan
            .members
            .iter()
            .chain(plan.spares.iter().map(|s| &s.device))
            .map(|&d| (d, now_ms))
            .collect();
        Self {
            plan,
            state: State::Forming,
            generation: 0,
            last_seen,
            events: Vec::new(),
        }
    }

    pub fn is_serving(&self) -> bool {
        self.state == State::Serving
    }

    pub fn is_dissolved(&self) -> bool {
        matches!(self.state, State::Dissolved(_))
    }

    pub fn warm_spares(&self) -> usize {
        self.plan.spares.len()
    }

    fn log(&mut self, now: u64, e: Event) {
        self.events.push((now, e));
    }

    /// Runs the self-test; serves on a match, dissolves on a mismatch.
    pub fn qualify(
        &mut self,
        now_ms: u64,
        test: &mut dyn SelfTest,
        devices: &[DeviceDescriptor],
        golden: &GoldenReference,
    ) -> &State {
        if !matches!(self.state, State::Forming | State::Qualifying) {
            return &self.state;
        }
        self.state = State::Qualifying;
        let run = test.run(&self.plan, devices);
        match judge(&self.plan, &run, golden) {
            Verdict::Match => {
                self.state = State::Serving;
                self.log(now_ms, Event::Qualified);
            }
            Verdict::Mismatch { first_faulty_stage } => {
                self.dissolve(
                    now_ms,
                    DissolveReason::QualificationFailed { first_faulty_stage },
                );
            }
        }
        &self.state
    }

    /// Records a heartbeat from a member or spare.
    pub fn heartbeat(&mut self, device: usize, now_ms: u64) {
        if let Some(t) = self.last_seen.get_mut(&device) {
            *t = (*t).max(now_ms);
        }
    }

    /// Declares silent members and spares failed and handles them.
    pub fn tick(&mut self, now_ms: u64) {
        if self.is_dissolved() {
            return;
        }
        let silent: Vec<usize> = self
            .last_seen
            .iter()
            .filter(|&(_, &t)| now_ms.saturating_sub(t) > FAILURE_SILENCE_MS)
            .map(|(&d, _)| d)
            .collect();
        for d in silent {
            self.device_lost(d, now_ms);
            if self.is_dissolved() {
                return;
            }
        }
    }

    /// The owner withdrew consent: the device leaves now.
    pub fn withdraw_consent(&mut self, device: usize, now_ms: u64) {
        if !self.last_seen.contains_key(&device) || self.is_dissolved() {
            return;
        }
        self.log(now_ms, Event::ConsentWithdrawn { device });
        self.device_lost(device, now_ms);
    }

    /// A member or spare is gone (failed, departed or withdrawn).
    pub fn device_lost(&mut self, device: usize, now_ms: u64) {
        if self.is_dissolved() || self.last_seen.remove(&device).is_none() {
            return;
        }
        if let Some(k) = self.plan.spares.iter().position(|s| s.device == device) {
            self.plan.spares.remove(k);
            self.log(now_ms, Event::SpareLost { device });
            return;
        }
        let Some(stage) = self.plan.members.iter().position(|&m| m == device) else {
            return;
        };
        self.log(now_ms, Event::MemberFailed { stage, device });
        match self
            .plan
            .spares
            .iter()
            .position(|s| s.covers.contains(&stage))
        {
            Some(k) => {
                let spare = self.plan.spares.remove(k);
                self.plan.members[stage] = spare.device;
                self.generation += 1;
                self.log(
                    now_ms,
                    Event::SparePromoted {
                        stage,
                        device: spare.device,
                    },
                );
                // Re-qualify before serving again.
                self.state = State::Qualifying;
            }
            None => self.dissolve(
                now_ms,
                DissolveReason::MemberLostWithoutSpare { stage, device },
            ),
        }
    }

    pub fn dissolve(&mut self, now_ms: u64, reason: DissolveReason) {
        if self.is_dissolved() {
            return;
        }
        self.state = State::Dissolved(reason.clone());
        self.log(now_ms, Event::Dissolved(reason));
    }

    /// Devices this island still holds (members and spares).
    pub fn devices(&self) -> Vec<usize> {
        self.last_seen.keys().copied().collect()
    }
}

/// Re-form a serving island only for a ≥ 20% predicted gain and at most once
/// per lease (research-6 §6.7 rule 5). Inputs are tokens/s × 1000.
pub fn should_reform(
    current_milli_tok_s: u64,
    candidate_milli_tok_s: u64,
    reforms_this_lease: u32,
) -> bool {
    reforms_this_lease == 0
        && candidate_milli_tok_s * 1000 >= current_milli_tok_s * (1000 + REFORM_GAIN_PERMILLE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::tests::{device, mesh};
    use crate::form::{FormationPolicy, SparePlan, form};
    use crate::model::ModelSpec;
    use crate::selftest::ToyPipeline;

    /// Three 16 GB-class devices plus spares for a 12-unit toy model.
    fn setup(n: usize) -> (Vec<DeviceDescriptor>, Island, GoldenReference) {
        let model = ModelSpec::uniform("u", 12, 1_000_000_000, 0);
        let devices: Vec<_> = (0..n)
            .map(|i| device(&format!("d{i}"), 8, false, None, "NYC"))
            .collect();
        let rtt = mesh(n, 5_000, &[]);
        let plan = form(&devices, &rtt, &model, &FormationPolicy::batch(0)).islands[0].clone();
        (devices, Island::new(plan, 0), ToyPipeline::golden(12))
    }

    #[test]
    fn forms_qualifies_and_serves() {
        let (devices, mut island, golden) = setup(4);
        assert_eq!(island.state, State::Forming);
        island.qualify(10, &mut ToyPipeline::default(), &devices, &golden);
        assert!(island.is_serving());
        assert_eq!(island.warm_spares(), 1);
    }

    #[test]
    fn a_wrong_digest_dissolves_the_island() {
        let (devices, mut island, golden) = setup(3);
        let bad = devices[island.plan.members[2]].device_id.clone();
        let mut test = ToyPipeline {
            faulty: [bad].into_iter().collect(),
        };
        island.qualify(10, &mut test, &devices, &golden);
        assert_eq!(
            island.state,
            State::Dissolved(DissolveReason::QualificationFailed {
                first_faulty_stage: Some(2)
            })
        );
    }

    #[test]
    fn silent_member_is_replaced_by_the_spare_and_requalified() {
        let (devices, mut island, golden) = setup(4);
        island.qualify(0, &mut ToyPipeline::default(), &devices, &golden);
        let spare = island.plan.spares[0].device;
        let victim = island.plan.members[1];
        // Everyone but the victim keeps beating.
        for t in (250..=1_500).step_by(250) {
            for d in island.devices() {
                if d != victim {
                    island.heartbeat(d, t);
                }
            }
            island.tick(t);
        }
        assert_eq!(island.plan.members[1], spare);
        assert_eq!(island.state, State::Qualifying);
        assert_eq!(island.generation, 1);
        assert!(island.events.iter().any(|(_, e)| *e
            == Event::MemberFailed {
                stage: 1,
                device: victim
            }));
        island.qualify(1_600, &mut ToyPipeline::default(), &devices, &golden);
        assert!(island.is_serving());
        assert_eq!(island.warm_spares(), 0);
        // A second loss with no spare left dissolves the island.
        let next = island.plan.members[0];
        island.device_lost(next, 2_000);
        assert_eq!(
            island.state,
            State::Dissolved(DissolveReason::MemberLostWithoutSpare {
                stage: 0,
                device: next
            })
        );
    }

    #[test]
    fn heartbeats_within_the_window_keep_members() {
        let (devices, mut island, golden) = setup(4);
        island.qualify(0, &mut ToyPipeline::default(), &devices, &golden);
        island.tick(1_000);
        assert!(
            island.is_serving(),
            "exactly 1 s of silence is not a failure"
        );
        for d in island.devices() {
            island.heartbeat(d, 900);
        }
        island.tick(1_800);
        assert!(island.is_serving());
    }

    #[test]
    fn consent_withdrawal_removes_the_device_at_once() {
        let (devices, mut island, golden) = setup(4);
        island.qualify(0, &mut ToyPipeline::default(), &devices, &golden);
        let spare = island.plan.spares[0].device;
        island.withdraw_consent(spare, 5);
        assert_eq!(island.warm_spares(), 0);
        assert!(!island.devices().contains(&spare));
        assert!(island.is_serving());
        let member = island.plan.members[0];
        island.withdraw_consent(member, 6);
        assert!(island.is_dissolved());
        assert!(
            island
                .events
                .contains(&(6, Event::ConsentWithdrawn { device: member }))
        );
    }

    #[test]
    fn spare_only_promotes_into_stages_it_covers() {
        let (devices, mut island, golden) = setup(4);
        island.qualify(0, &mut ToyPipeline::default(), &devices, &golden);
        let spare = island.plan.spares[0].device;
        island.plan.spares[0] = SparePlan {
            device: spare,
            covers: vec![0],
        };
        let m2 = island.plan.members[2];
        island.device_lost(m2, 10);
        assert!(island.is_dissolved());
    }

    #[test]
    fn reform_needs_twenty_percent_and_once_per_lease() {
        assert!(!should_reform(10_000, 11_999, 0));
        assert!(should_reform(10_000, 12_000, 0));
        assert!(!should_reform(10_000, 20_000, 1));
    }
}
