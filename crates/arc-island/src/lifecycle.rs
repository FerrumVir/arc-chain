//! Island lifecycle (research-6 §6.4, §6.7), fail closed:
//!
//! ```text
//! Forming ─qualify(pinned golden, measured executor, measured fresh inputs)→ Serving
//!    └────qualify(simulation reference or synthetic anything)──────────────→ Simulated (never serves)
//! Serving ─member lost→ Recovering ─trusted checkpoint restored→ Qualifying ─fresh golden→ Serving
//!                       (no spare, failed recovery, bad links or digest mismatch → Dissolved)
//! ```
//!
//! - **Trust.** Real serving needs the reference from
//!   [`GoldenReference::pinned`] for the plan's exact model identity, an
//!   executor that reports measured provenance, and measured, fresh inputs.
//!   A missing pin (Kimi K2.6 today) refuses qualification. A simulation
//!   reference or a synthetic executor can at most reach `Simulated`.
//! - **Health.** Members and spares heartbeat every 250 ms; one silent for
//!   more than 1 s is lost. [`Island::health_check`] re-reads every member and
//!   spare (identity, consent, evidence freshness and provenance, memory)
//!   and every link; admission runs it on every request.
//! - **Spares.** The plan carries the research-6 count (1/2/3). Losing a
//!   spare (failure or the owner withdrawing consent) removes only that spare
//!   and **pauses admission** until [`Island::replenish_spare`] restores the
//!   count; the active members keep their state.
//! - **Recovery.** A lost member is replaced by a spare, then the island
//!   restores in-flight state from a trusted checkpoint (committed-token
//!   ledger; KV mirror or activation replay) through [`Recovery`], and must
//!   pass a fresh golden run before it serves again.
//! - **Re-optimisation** happens only at lease boundaries and only for a
//!   ≥ 20% predicted gain ([`should_reform`]).

use crate::device::{DeviceDescriptor, Freshness, Provenance, RttSource};
use crate::form::{IslandPlan, SparePlan};
use crate::selftest::{GoldenExecutor, GoldenReference, Verdict, judge};
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
    WrongModel,
    MemberLostWithoutSpare { stage: usize, device: usize },
    RecoveryFailed { stage: usize },
    LinkFailed { a: usize, b: usize },
    LeaseEnded,
    Requested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Planned; shards loading.
    Forming,
    /// A spare took over `stage`; waiting for checkpoint recovery.
    Recovering {
        stage: usize,
    },
    /// Ready for a golden run.
    Qualifying,
    /// Qualified against a pinned golden on measured inputs.
    Serving,
    /// Qualified on synthetic evidence. Never serves.
    Simulated,
    Dissolved(DissolveReason),
}

/// Why qualification was refused (the island stays closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualifyError {
    /// Not in `Forming` or `Qualifying`.
    NotReady,
    /// No pinned golden for this model identity (Kimi K2.6 today).
    NoPinnedGolden,
    SyntheticExecutor,
    /// The plan was formed from synthetic inputs.
    SyntheticInputs,
    /// A member, spare or link failed the input check.
    UnhealthyInputs,
}

/// Which reference qualification uses.
#[derive(Debug, Clone, Copy)]
pub enum GoldenSource<'a> {
    /// The pinned reference for the plan's model; the only path to `Serving`.
    Pinned,
    /// A reference for simulation or tests; at most `Simulated`.
    Simulation(&'a GoldenReference),
}

/// A checkpoint from the trusted committed-token ledger, never from the
/// recovering worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedCheckpoint {
    pub committed_tokens: u64,
    pub digest: String,
}

/// Restores in-flight state on a promoted stage (KV mirror or activation
/// replay) and returns the digest of the restored state.
pub trait Recovery {
    fn restore(
        &mut self,
        plan: &IslandPlan,
        stage: usize,
        checkpoint: &TrustedCheckpoint,
    ) -> String;
}

/// Why a replacement spare was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplenishError {
    Dissolved,
    NotNeeded,
    AlreadyInIsland,
    /// Consent, evidence, qualification or memory (it must hold the largest
    /// stage).
    Ineligible,
    LinkFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Qualified,
    SimulatedQualified,
    QualificationRefused(QualifyError),
    MemberFailed { stage: usize, device: usize },
    ConsentWithdrawn { device: usize },
    HealthCheckFailed { device: usize },
    SparePromoted { stage: usize, device: usize },
    Recovered { stage: usize },
    SpareLost { device: usize },
    SpareReplenished { device: usize },
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
    /// Device ids at the time each index joined, to catch a reused index.
    ids: BTreeMap<usize, String>,
    freshness: Freshness,
    pub events: Vec<(u64, Event)>,
}

impl Island {
    pub fn new(
        plan: IslandPlan,
        devices: &[DeviceDescriptor],
        freshness: Freshness,
        now_ms: u64,
    ) -> Self {
        let everyone: Vec<usize> = plan
            .members
            .iter()
            .chain(plan.spares.iter().map(|s| &s.device))
            .copied()
            .collect();
        Self {
            last_seen: everyone.iter().map(|&d| (d, now_ms)).collect(),
            ids: everyone
                .iter()
                .map(|&d| (d, devices[d].device_id.clone()))
                .collect(),
            plan,
            state: State::Forming,
            generation: 0,
            freshness,
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

    /// Spares missing against the policy count.
    pub fn spare_shortfall(&self) -> usize {
        self.plan
            .required_spares
            .saturating_sub(self.plan.spares.len())
    }

    /// Serving with the full spare count.
    pub fn admission_open(&self) -> bool {
        self.is_serving() && self.spare_shortfall() == 0
    }

    fn log(&mut self, now: u64, e: Event) {
        self.events.push((now, e));
    }

    fn allow_synthetic(&self) -> bool {
        self.plan.provenance == Provenance::Synthetic
    }

    /// Whether device `i` may still hold `need_bytes` in this island.
    fn device_ok(
        &self,
        i: usize,
        need_bytes: u64,
        devices: &[DeviceDescriptor],
        now_ms: u64,
    ) -> bool {
        let Some(d) = devices.get(i) else {
            return false;
        };
        self.ids.get(&i).is_none_or(|id| *id == d.device_id)
            && d.consent.permits(d, now_ms)
            && d.golden_qualified
            && d.inputs_acceptable(now_ms, &self.freshness, self.allow_synthetic())
            && d.pool().is_some_and(|p| p.usable_bytes >= need_bytes)
    }

    fn link_ok(&self, a: usize, b: usize, rtt: &dyn RttSource, now_ms: u64) -> bool {
        self.plan.link_rule.ok(
            rtt.link(a, b),
            now_ms,
            &self.freshness,
            self.allow_synthetic(),
        )
    }

    /// Re-reads every member, spare and link. A failing spare is dropped
    /// (admission pauses); a failing member is replaced by a spare
    /// (recovery) or the island dissolves; a failing member-to-member link
    /// dissolves it. Returns whether admission is open afterwards.
    pub fn health_check(
        &mut self,
        now_ms: u64,
        devices: &[DeviceDescriptor],
        rtt: &dyn RttSource,
    ) -> bool {
        if self.is_dissolved() {
            return false;
        }
        let largest = self.plan.largest_stage_bytes();
        let spares: Vec<usize> = self.plan.spares.iter().map(|s| s.device).collect();
        for sp in spares {
            let links = self
                .plan
                .members
                .iter()
                .all(|&m| self.link_ok(sp, m, rtt, now_ms));
            if !self.device_ok(sp, largest, devices, now_ms) || !links {
                self.log(now_ms, Event::HealthCheckFailed { device: sp });
                self.device_lost(sp, now_ms);
            }
        }
        for stage in 0..self.plan.members.len() {
            let m = self.plan.members[stage];
            if !self.device_ok(m, self.plan.stages[stage].need_bytes(), devices, now_ms) {
                self.log(now_ms, Event::HealthCheckFailed { device: m });
                self.device_lost(m, now_ms);
                if self.is_dissolved() {
                    return false;
                }
            }
        }
        let members = self.plan.members.clone();
        for (k, &a) in members.iter().enumerate() {
            for &b in &members[k + 1..] {
                if !self.link_ok(a, b, rtt, now_ms) {
                    self.dissolve(now_ms, DissolveReason::LinkFailed { a, b });
                    return false;
                }
            }
        }
        self.admission_open()
    }

    /// Runs the golden self-test. Only [`GoldenSource::Pinned`] with a
    /// measured executor, a measured plan and healthy inputs can reach
    /// `Serving`; a simulation reference reaches at most `Simulated`. A
    /// mismatch dissolves the island.
    pub fn qualify(
        &mut self,
        now_ms: u64,
        executor: &mut dyn GoldenExecutor,
        devices: &[DeviceDescriptor],
        rtt: &dyn RttSource,
        source: GoldenSource<'_>,
    ) -> Result<State, QualifyError> {
        if !matches!(self.state, State::Forming | State::Qualifying) {
            return Err(QualifyError::NotReady);
        }
        let refuse = |island: &mut Self, e: QualifyError| {
            island.log(now_ms, Event::QualificationRefused(e));
            Err(e)
        };
        let golden = match source {
            GoldenSource::Pinned => {
                if self.plan.provenance == Provenance::Synthetic {
                    return refuse(self, QualifyError::SyntheticInputs);
                }
                if executor.provenance() != Provenance::Measured {
                    return refuse(self, QualifyError::SyntheticExecutor);
                }
                let Some(golden) = GoldenReference::pinned(&self.plan.model) else {
                    return refuse(self, QualifyError::NoPinnedGolden);
                };
                self.health_check(now_ms, devices, rtt);
                if self.state != State::Forming && self.state != State::Qualifying {
                    return refuse(self, QualifyError::UnhealthyInputs);
                }
                golden
            }
            GoldenSource::Simulation(g) => g.clone(),
        };
        let real = matches!(source, GoldenSource::Pinned)
            && golden.provenance() == Provenance::Measured
            && executor.provenance() == Provenance::Measured;
        self.state = State::Qualifying;
        let run = executor.run(&self.plan, devices);
        match judge(&self.plan, &run, &golden) {
            Verdict::Match if real => {
                self.state = State::Serving;
                self.log(now_ms, Event::Qualified);
            }
            Verdict::Match => {
                self.state = State::Simulated;
                self.log(now_ms, Event::SimulatedQualified);
            }
            Verdict::WrongModel => self.dissolve(now_ms, DissolveReason::WrongModel),
            Verdict::Mismatch { first_faulty_stage } => self.dissolve(
                now_ms,
                DissolveReason::QualificationFailed { first_faulty_stage },
            ),
        }
        Ok(self.state.clone())
    }

    /// Records a heartbeat from a member or spare.
    pub fn heartbeat(&mut self, device: usize, now_ms: u64) {
        if let Some(t) = self.last_seen.get_mut(&device) {
            *t = (*t).max(now_ms);
        }
    }

    /// Declares silent members and spares lost and handles them.
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

    /// The owner withdrew consent: the device leaves now. A spare leaving
    /// pauses admission; a member leaving triggers recovery.
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
        self.ids.remove(&device);
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
                self.state = State::Recovering { stage };
            }
            None => self.dissolve(
                now_ms,
                DissolveReason::MemberLostWithoutSpare { stage, device },
            ),
        }
    }

    /// Restores the promoted stage from a trusted checkpoint. On success the
    /// island must pass a fresh golden run; any mismatch dissolves it.
    pub fn recover(
        &mut self,
        now_ms: u64,
        checkpoint: &TrustedCheckpoint,
        recovery: &mut dyn Recovery,
    ) -> bool {
        let State::Recovering { stage } = self.state else {
            return false;
        };
        let restored = recovery.restore(&self.plan, stage, checkpoint);
        if checkpoint.digest.is_empty() || restored != checkpoint.digest {
            self.dissolve(now_ms, DissolveReason::RecoveryFailed { stage });
            return false;
        }
        self.log(now_ms, Event::Recovered { stage });
        self.state = State::Qualifying;
        true
    }

    /// Reserves a replacement spare: consent, evidence and qualification
    /// current, able to hold the largest stage, and meeting the link rule
    /// with every member. Admission reopens when the count is restored.
    pub fn replenish_spare(
        &mut self,
        now_ms: u64,
        candidate: usize,
        devices: &[DeviceDescriptor],
        rtt: &dyn RttSource,
    ) -> Result<(), ReplenishError> {
        if self.is_dissolved() {
            return Err(ReplenishError::Dissolved);
        }
        if self.spare_shortfall() == 0 {
            return Err(ReplenishError::NotNeeded);
        }
        if self.last_seen.contains_key(&candidate) {
            return Err(ReplenishError::AlreadyInIsland);
        }
        if !self.device_ok(candidate, self.plan.largest_stage_bytes(), devices, now_ms) {
            return Err(ReplenishError::Ineligible);
        }
        if !self
            .plan
            .members
            .iter()
            .all(|&m| self.link_ok(candidate, m, rtt, now_ms))
        {
            return Err(ReplenishError::LinkFailed);
        }
        self.plan.spares.push(SparePlan {
            device: candidate,
            covers: (0..self.plan.stages.len()).collect(),
        });
        self.last_seen.insert(candidate, now_ms);
        self.ids
            .insert(candidate, devices[candidate].device_id.clone());
        self.log(now_ms, Event::SpareReplenished { device: candidate });
        Ok(())
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
pub(crate) mod tests {
    use super::*;
    use crate::device::{Evidence, RttMatrix};
    use crate::form::tests::{device, mesh};
    use crate::form::{FormationPolicy, form};
    use crate::model::ModelSpec;
    use crate::selftest::test_pins::MeasuredToy;
    use crate::selftest::toy::ToyPipeline;

    /// Restores by returning the trusted digest (or a wrong one).
    pub struct LedgerRecovery {
        pub honest: bool,
    }

    impl Recovery for LedgerRecovery {
        fn restore(&mut self, _: &IslandPlan, _: usize, checkpoint: &TrustedCheckpoint) -> String {
            if self.honest {
                checkpoint.digest.clone()
            } else {
                "tampered".into()
            }
        }
    }

    pub fn checkpoint() -> TrustedCheckpoint {
        TrustedCheckpoint {
            committed_tokens: 42,
            digest: "ab".repeat(32),
        }
    }

    /// A 12-unit toy model on 8 GB machines: 3 members plus 1 spare, and
    /// `extra` machines left over for replenishment. Measured inputs.
    pub fn setup(extra: usize) -> (Vec<DeviceDescriptor>, RttMatrix, Island) {
        let model = ModelSpec::uniform("u", 12, 1_000_000_000, 0);
        let n = 4 + extra;
        let devices: Vec<_> = (0..n)
            .map(|i| device(&format!("d{i}"), 8, false, None, "NYC"))
            .collect();
        let rtt = mesh(n, 5_000, &[]);
        let out = form(&devices, &rtt, &model, &FormationPolicy::batch(0), 0);
        let plan = out.islands[0].clone();
        assert_eq!((plan.members.len(), plan.spares.len()), (3, 1));
        let island = Island::new(plan, &devices, Freshness::default(), 0);
        (devices, rtt, island)
    }

    fn serving(extra: usize) -> (Vec<DeviceDescriptor>, RttMatrix, Island) {
        let (devices, rtt, mut island) = setup(extra);
        let state = island.qualify(
            10,
            &mut MeasuredToy::default(),
            &devices,
            &rtt,
            GoldenSource::Pinned,
        );
        assert_eq!(state, Ok(State::Serving));
        (devices, rtt, island)
    }

    #[test]
    fn pinned_golden_measured_executor_and_measured_inputs_serve() {
        let (_, _, island) = serving(0);
        assert!(island.admission_open());
        assert_eq!(island.warm_spares(), 1);
    }

    #[test]
    fn kimi_without_a_pinned_golden_fails_closed() {
        let model = ModelSpec::kimi_k26_int4();
        let devices: Vec<_> = (0..3)
            .map(|i| device(&format!("u{i}"), 512, false, None, "NYC"))
            .collect();
        let rtt = mesh(3, 10_000, &[]);
        let plan =
            form(&devices, &rtt, &model, &FormationPolicy::batch(8 * 4096), 0).islands[0].clone();
        assert_eq!(plan.provenance, Provenance::Measured);
        let mut island = Island::new(plan, &devices, Freshness::default(), 0);
        let r = island.qualify(
            1,
            &mut MeasuredToy::default(),
            &devices,
            &rtt,
            GoldenSource::Pinned,
        );
        assert_eq!(r, Err(QualifyError::NoPinnedGolden));
        assert!(!island.is_serving());
        // A caller-supplied digest cannot stand in for the pin: the toy
        // reference for Kimi's identity is synthetic and reaches Simulated.
        let fake = ToyPipeline::golden(&model.identity, model.layers.len());
        let r = island.qualify(
            2,
            &mut MeasuredToy::default(),
            &devices,
            &rtt,
            GoldenSource::Simulation(&fake),
        );
        assert_eq!(r, Ok(State::Simulated));
        assert!(!island.admission_open());
    }

    #[test]
    fn synthetic_executor_or_inputs_never_reach_serving() {
        let (devices, rtt, mut island) = setup(0);
        let r = island.qualify(
            1,
            &mut ToyPipeline::default(),
            &devices,
            &rtt,
            GoldenSource::Pinned,
        );
        assert_eq!(r, Err(QualifyError::SyntheticExecutor));
        let golden = GoldenReference::pinned(&island.plan.model).unwrap();
        let r = island.qualify(
            2,
            &mut ToyPipeline::default(),
            &devices,
            &rtt,
            GoldenSource::Simulation(&golden),
        );
        assert_eq!(
            r,
            Ok(State::Simulated),
            "a pinned reference through the simulation path still never serves"
        );
        // A plan formed from synthetic inputs.
        let model = ModelSpec::uniform("u", 12, 1_000_000_000, 0);
        let synth: Vec<_> = (0..4)
            .map(|i| {
                let mut d = device(&format!("s{i}"), 8, false, None, "NYC");
                d.evidence = Evidence::synthetic(0);
                d
            })
            .collect();
        let mut policy = FormationPolicy::batch(0);
        policy.allow_synthetic = true;
        let plan = form(&synth, &rtt, &model, &policy, 0).islands[0].clone();
        let mut island = Island::new(plan, &synth, Freshness::default(), 0);
        let r = island.qualify(
            1,
            &mut MeasuredToy::default(),
            &synth,
            &rtt,
            GoldenSource::Pinned,
        );
        assert_eq!(r, Err(QualifyError::SyntheticInputs));
    }

    #[test]
    fn a_wrong_digest_or_model_dissolves_the_island() {
        let (devices, rtt, mut island) = setup(0);
        let bad = devices[island.plan.members[2]].device_id.clone();
        let mut exec = MeasuredToy(ToyPipeline {
            faulty: [bad].into_iter().collect(),
        });
        let _ = island.qualify(10, &mut exec, &devices, &rtt, GoldenSource::Pinned);
        assert_eq!(
            island.state,
            State::Dissolved(DissolveReason::QualificationFailed {
                first_faulty_stage: Some(2)
            })
        );
        let (devices, rtt, mut island) = setup(0);
        let other = ToyPipeline::golden(&ModelSpec::kimi_k26_int4().identity, 12);
        let _ = island.qualify(
            10,
            &mut ToyPipeline::default(),
            &devices,
            &rtt,
            GoldenSource::Simulation(&other),
        );
        assert_eq!(island.state, State::Dissolved(DissolveReason::WrongModel));
    }

    #[test]
    fn stale_or_withdrawn_inputs_refuse_qualification() {
        let (mut devices, rtt, mut island) = setup(0);
        let m = island.plan.members[0];
        devices[m].consent.withdraw();
        let spare = island.plan.spares[0].device;
        let r = island.qualify(
            10,
            &mut MeasuredToy::default(),
            &devices,
            &rtt,
            GoldenSource::Pinned,
        );
        assert_eq!(r, Err(QualifyError::UnhealthyInputs));
        assert_eq!(island.plan.members[0], spare, "the spare took the stage");
        assert_eq!(island.state, State::Recovering { stage: 0 });
    }

    #[test]
    fn member_loss_promotes_recovers_from_checkpoint_and_requalifies() {
        let (devices, rtt, mut island) = serving(1);
        let spare = island.plan.spares[0].device;
        let victim = island.plan.members[1];
        for t in (250..=1_500).step_by(250) {
            for d in island.devices() {
                if d != victim {
                    island.heartbeat(d, t);
                }
            }
            island.tick(t);
        }
        assert_eq!(island.plan.members[1], spare);
        assert_eq!(island.state, State::Recovering { stage: 1 });
        assert!(!island.admission_open());
        assert!(island.events.iter().any(|(_, e)| *e
            == Event::MemberFailed {
                stage: 1,
                device: victim
            }));
        // Golden qualification is refused until recovery succeeds.
        assert_eq!(
            island.qualify(
                1_550,
                &mut MeasuredToy::default(),
                &devices,
                &rtt,
                GoldenSource::Pinned
            ),
            Err(QualifyError::NotReady)
        );
        assert!(island.recover(1_600, &checkpoint(), &mut LedgerRecovery { honest: true }));
        assert_eq!(island.state, State::Qualifying);
        let r = island.qualify(
            1_700,
            &mut MeasuredToy::default(),
            &devices,
            &rtt,
            GoldenSource::Pinned,
        );
        assert_eq!(r, Ok(State::Serving));
        // Serving, but one spare short: admission is paused.
        assert_eq!(island.spare_shortfall(), 1);
        assert!(!island.admission_open());
        let fresh = devices.len() - 1;
        assert_eq!(island.replenish_spare(1_800, fresh, &devices, &rtt), Ok(()));
        assert!(island.admission_open());
        assert_eq!(island.generation, 1);
    }

    #[test]
    fn tampered_or_empty_checkpoints_dissolve() {
        let (_, _, mut island) = serving(0);
        let m = island.plan.members[0];
        island.device_lost(m, 100);
        assert!(!island.recover(200, &checkpoint(), &mut LedgerRecovery { honest: false }));
        assert_eq!(
            island.state,
            State::Dissolved(DissolveReason::RecoveryFailed { stage: 0 })
        );
        let (_, _, mut island) = serving(0);
        island.device_lost(island.plan.members[0], 100);
        let empty = TrustedCheckpoint {
            committed_tokens: 0,
            digest: String::new(),
        };
        assert!(!island.recover(200, &empty, &mut LedgerRecovery { honest: true }));
    }

    #[test]
    fn spare_withdrawal_pauses_admission_without_dissolving_members() {
        let (devices, rtt, mut island) = serving(1);
        let spare = island.plan.spares[0].device;
        island.withdraw_consent(spare, 20);
        assert!(island.is_serving(), "healthy members keep serving state");
        assert_eq!(island.warm_spares(), 0);
        assert!(!island.admission_open());
        assert!(!island.devices().contains(&spare));
        let fresh = devices.len() - 1;
        assert_eq!(island.replenish_spare(30, fresh, &devices, &rtt), Ok(()));
        assert!(island.admission_open());
        assert_eq!(
            island.replenish_spare(40, fresh, &devices, &rtt),
            Err(ReplenishError::NotNeeded)
        );
        // Losing a member with no spare left dissolves.
        let (_, _, mut lonely) = serving(0);
        lonely.withdraw_consent(lonely.plan.spares[0].device, 5);
        let member = lonely.plan.members[0];
        lonely.withdraw_consent(member, 6);
        assert!(lonely.is_dissolved());
        assert!(
            lonely
                .events
                .contains(&(6, Event::ConsentWithdrawn { device: member }))
        );
    }

    #[test]
    fn replenishment_checks_the_candidate() {
        let (mut devices, mut rtt, mut island) = serving(1);
        island.withdraw_consent(island.plan.spares[0].device, 1);
        let c = devices.len() - 1;
        let member = island.plan.members[0];
        assert_eq!(
            island.replenish_spare(2, member, &devices, &rtt),
            Err(ReplenishError::AlreadyInIsland)
        );
        devices[c].consent.expires_at_ms = 1;
        assert_eq!(
            island.replenish_spare(2, c, &devices, &rtt),
            Err(ReplenishError::Ineligible)
        );
        devices[c].consent.expires_at_ms = u64::MAX;
        devices[c].evidence = Evidence::synthetic(0);
        assert_eq!(
            island.replenish_spare(2, c, &devices, &rtt),
            Err(ReplenishError::Ineligible)
        );
        devices[c].evidence = Evidence::measured(0);
        devices[c].measured.usable_memory_bytes = Some(1_000_000_000);
        assert_eq!(
            island.replenish_spare(2, c, &devices, &rtt),
            Err(ReplenishError::Ineligible),
            "too small for the largest stage"
        );
        devices[c].measured.usable_memory_bytes = Some(6_400_000_000);
        rtt.insert(c, member, crate::form::tests::link(40_000));
        assert_eq!(
            island.replenish_spare(2, c, &devices, &rtt),
            Err(ReplenishError::LinkFailed)
        );
        rtt.insert(c, member, crate::form::tests::link(5_000));
        assert_eq!(island.replenish_spare(2, c, &devices, &rtt), Ok(()));
    }

    #[test]
    fn health_check_catches_stale_inputs_and_bad_links() {
        let (mut devices, mut rtt, mut island) = serving(0);
        assert!(island.health_check(100, &devices, &rtt));
        // A stale spare is dropped and admission pauses.
        let spare = island.plan.spares[0].device;
        devices[spare].evidence = Evidence::measured(0);
        let later = Freshness::default().device_ttl_ms + 1;
        for (i, d) in devices.iter_mut().enumerate() {
            if i != spare {
                d.evidence = Evidence::measured(later);
            }
        }
        let fresh_links = mesh(devices.len(), 5_000, &[]);
        let mut relinked = RttMatrix::new();
        for a in 0..devices.len() {
            for b in (a + 1)..devices.len() {
                let mut l = fresh_links.link(a, b).unwrap();
                l.evidence = Evidence::measured(later);
                relinked.insert(a, b, l);
            }
        }
        rtt = relinked;
        assert!(!island.health_check(later, &devices, &rtt));
        assert!(island.is_serving());
        assert_eq!(island.warm_spares(), 0);
        // A degraded link between members dissolves.
        let (a, b) = (island.plan.members[0], island.plan.members[1]);
        let mut slow = crate::form::tests::link(40_000);
        slow.evidence = Evidence::measured(later);
        rtt.insert(a, b, slow);
        assert!(!island.health_check(later, &devices, &rtt));
        assert_eq!(
            island.state,
            State::Dissolved(DissolveReason::LinkFailed {
                a: a.min(b),
                b: a.max(b)
            })
        );
    }

    #[test]
    fn heartbeats_within_the_window_keep_members() {
        let (_, _, mut island) = serving(0);
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
    fn reform_needs_twenty_percent_and_once_per_lease() {
        assert!(!should_reform(10_000, 11_999, 0));
        assert!(should_reform(10_000, 12_000, 0));
        assert!(!should_reform(10_000, 20_000, 1));
    }
}
