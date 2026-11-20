use crate::lease::{self, CapabilityLease, LeaseError, LeaseRequirements};
use crate::link::{self, LinkMeasurement, PathCounters, Probe};
use crate::placement::{Candidate, Participant};
use crate::reservation::ReservationLedger;
use crate::verify::Finding;
use crate::{Address, Hash256};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const MAX_PROBES_PER_WORKER: usize = 32;

pub const FAILURES_BEFORE_EXCLUSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offered {
    Accepted,
    Stale,
}

#[derive(Debug, Default)]
pub struct LeaseBook {
    leases: BTreeMap<[u8; 32], CapabilityLease>,
}

impl LeaseBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn offer(
        &mut self,
        offered: CapabilityLease,
        req: &LeaseRequirements<'_>,
    ) -> Result<Offered, LeaseError> {
        lease::validate(&offered, req)?;
        let key = offered.body.worker.0;
        if let Some(held) = self.leases.get(&key)
            && offered.body.nonce <= held.body.nonce
        {
            return Ok(Offered::Stale);
        }
        self.leases.insert(key, offered);
        Ok(Offered::Accepted)
    }

    pub fn retain_current(&mut self, members: &BTreeSet<[u8; 32]>, height: u64) -> Vec<Address> {
        let mut gone = Vec::new();
        self.leases.retain(|key, lease| {
            let keep = members.contains(key) && height < lease.body.expires_at_height;
            if !keep {
                gone.push(Hash256(*key));
            }
            keep
        });
        gone
    }

    pub fn get(&self, worker: &Address) -> Option<&CapabilityLease> {
        self.leases.get(&worker.0)
    }

    pub fn iter(&self) -> impl Iterator<Item = &CapabilityLease> {
        self.leases.values()
    }

    pub fn len(&self) -> usize {
        self.leases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeState {
    InFlight { deadline: u64 },
    Measured { macs_per_s: u64 },
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChallengeError {
    #[error("this worker was already challenged in epoch {0}")]
    AlreadyChallenged(u64),
    #[error("no challenge to this worker is in flight")]
    NotInFlight,
}

#[derive(Debug)]
pub struct ChallengeBook {
    epoch: u64,
    state: BTreeMap<[u8; 32], ChallengeState>,
}

impl ChallengeBook {
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch,
            state: BTreeMap::new(),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn begin_epoch(&mut self, epoch: u64) -> usize {
        if epoch == self.epoch {
            return 0;
        }
        self.epoch = epoch;
        let cleared = self.state.len();
        self.state.clear();
        cleared
    }

    pub fn issue(&mut self, worker: &Address, deadline: u64) -> Result<(), ChallengeError> {
        match self.state.entry(worker.0) {
            Entry::Occupied(_) => Err(ChallengeError::AlreadyChallenged(self.epoch)),
            Entry::Vacant(slot) => {
                slot.insert(ChallengeState::InFlight { deadline });
                Ok(())
            }
        }
    }

    pub fn answer(
        &mut self,
        worker: &Address,
        checked: Result<u64, LeaseError>,
        now: u64,
    ) -> Result<ChallengeState, ChallengeError> {
        let Some(ChallengeState::InFlight { deadline }) = self.state.get(&worker.0).copied() else {
            return Err(ChallengeError::NotInFlight);
        };
        let outcome = match checked {
            Ok(macs_per_s) if now <= deadline && macs_per_s > 0 => {
                ChallengeState::Measured { macs_per_s }
            }
            _ => ChallengeState::Refused,
        };
        self.state.insert(worker.0, outcome);
        Ok(outcome)
    }

    pub fn expire(&mut self, now: u64) -> usize {
        let mut expired = 0;
        for state in self.state.values_mut() {
            if matches!(*state, ChallengeState::InFlight { deadline } if now > deadline) {
                *state = ChallengeState::Refused;
                expired += 1;
            }
        }
        expired
    }

    pub fn measured(&self, worker: &Address) -> Option<u64> {
        match self.state.get(&worker.0) {
            Some(ChallengeState::Measured { macs_per_s }) => Some(*macs_per_s),
            _ => None,
        }
    }

    pub fn state(&self, worker: &Address) -> Option<ChallengeState> {
        self.state.get(&worker.0).copied()
    }

    pub fn retain_members(&mut self, members: &BTreeSet<[u8; 32]>) -> usize {
        let before = self.state.len();
        self.state.retain(|key, _| members.contains(key));
        before - self.state.len()
    }

    pub fn len(&self) -> usize {
        self.state.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct ProbeBook {
    simulated: bool,
    probes: BTreeMap<[u8; 32], VecDeque<(u64, Probe)>>,
    counters: BTreeMap<[u8; 32], PathCounters>,
    transport_loss: BTreeMap<[u8; 32], u32>,
}

impl ProbeBook {
    pub fn new(simulated: bool) -> Self {
        Self {
            simulated,
            ..Self::default()
        }
    }

    pub fn record_probe(&mut self, worker: &Address, at: u64, probe: Probe) {
        let ring = self.probes.entry(worker.0).or_default();
        if ring.len() == MAX_PROBES_PER_WORKER {
            ring.pop_front();
        }
        ring.push_back((at, probe));
    }

    pub fn record_counters(&mut self, worker: &Address, now: PathCounters) -> Option<u32> {
        let previous = self.counters.insert(worker.0, now);
        let loss = previous
            .as_ref()
            .and_then(|earlier| link::windowed_loss_per_mille(earlier, &now));
        match loss {
            Some(per_mille) => {
                self.transport_loss.insert(worker.0, per_mille);
            }
            None if previous.map(|p| p.generation) != Some(now.generation) => {
                self.transport_loss.remove(&worker.0);
            }
            None => {}
        }
        loss
    }

    pub fn link(&self, worker: &Address) -> Option<LinkMeasurement> {
        let ring = self.probes.get(&worker.0)?;
        let measured_at = ring.iter().map(|(at, _)| *at).max()?;
        let probes: Vec<Probe> = ring.iter().map(|(_, probe)| *probe).collect();
        let mut measurement = link::summarize(&probes, measured_at, self.simulated)?;
        if let Some(loss) = self.transport_loss.get(&worker.0) {
            measurement.failure_per_mille = measurement.failure_per_mille.max(*loss);
        }
        Some(measurement)
    }

    pub fn retain_members(&mut self, members: &BTreeSet<[u8; 32]>) -> usize {
        let before = self.probes.len();
        self.probes.retain(|key, _| members.contains(key));
        self.counters.retain(|key, _| members.contains(key));
        self.transport_loss.retain(|key, _| members.contains(key));
        before - self.probes.len()
    }

    pub fn len(&self) -> usize {
        self.probes.values().map(VecDeque::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.probes.values().all(VecDeque::is_empty)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exclusion {
    Fault {
        certificate: Hash256,
        stage: usize,
        expected_digest: Hash256,
        found_digest: Hash256,
    },
    Failures(u32),
}

#[derive(Debug)]
pub struct Exclusions {
    epoch: u64,
    excluded: BTreeMap<[u8; 32], Exclusion>,
    failures: BTreeMap<[u8; 32], u32>,
}

impl Exclusions {
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch,
            excluded: BTreeMap::new(),
            failures: BTreeMap::new(),
        }
    }

    pub fn begin_epoch(&mut self, epoch: u64) -> usize {
        if epoch == self.epoch {
            return 0;
        }
        self.epoch = epoch;
        let ended = self.excluded.len();
        self.excluded.clear();
        self.failures.clear();
        ended
    }

    pub fn fault(&mut self, certificate: Hash256, finding: &Finding) -> Option<Address> {
        let Finding::Fault {
            stage,
            participant: Participant::Worker(worker),
            expected_digest,
            found_digest,
        } = finding
        else {
            return None;
        };
        self.excluded.entry(worker.0).or_insert(Exclusion::Fault {
            certificate,
            stage: *stage,
            expected_digest: *expected_digest,
            found_digest: *found_digest,
        });
        self.failures.remove(&worker.0);
        Some(*worker)
    }

    pub fn call_failed(&mut self, worker: &Address) -> bool {
        if self.excluded.contains_key(&worker.0) {
            return false;
        }
        let count = {
            let failures = self.failures.entry(worker.0).or_insert(0);
            *failures += 1;
            *failures
        };
        if count < FAILURES_BEFORE_EXCLUSION {
            return false;
        }
        self.failures.remove(&worker.0);
        self.excluded.insert(worker.0, Exclusion::Failures(count));
        true
    }

    pub fn call_succeeded(&mut self, worker: &Address) {
        self.failures.remove(&worker.0);
    }

    pub fn forgive_failures(&mut self, worker: &Address) -> bool {
        self.failures.remove(&worker.0);
        if matches!(self.excluded.get(&worker.0), Some(Exclusion::Failures(_))) {
            self.excluded.remove(&worker.0);
            return true;
        }
        false
    }

    pub fn is_excluded(&self, worker: &Address) -> bool {
        self.excluded.contains_key(&worker.0)
    }

    pub fn get(&self, worker: &Address) -> Option<&Exclusion> {
        self.excluded.get(&worker.0)
    }

    pub fn retain_members(&mut self, members: &BTreeSet<[u8; 32]>) -> usize {
        let before = self.excluded.len();
        self.excluded.retain(|key, _| members.contains(key));
        self.failures.retain(|key, _| members.contains(key));
        before - self.excluded.len()
    }

    pub fn len(&self) -> usize {
        self.excluded.len() + self.failures.len()
    }

    pub fn is_empty(&self) -> bool {
        self.excluded.is_empty() && self.failures.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub worker: Address,
    pub transport_id: String,
    pub ram_headroom_bytes: u64,
    pub claimed_macs_per_s: Option<u64>,
    pub resident_layers: Vec<(u32, u32)>,
    pub resident_output: bool,
    pub digest: Hash256,
}

impl Offer {
    pub fn from_lease(lease: &CapabilityLease) -> Self {
        Self {
            worker: lease.body.worker,
            transport_id: lease.body.transport_id.clone(),
            ram_headroom_bytes: lease.body.ram_headroom_bytes,
            claimed_macs_per_s: Some(lease.body.claimed_macs_per_s),
            resident_layers: lease.body.resident_layers.clone(),
            resident_output: false,
            digest: lease.digest(),
        }
    }
}

pub fn candidates_from(
    offers: impl IntoIterator<Item = Offer>,
    challenges: &ChallengeBook,
    probes: &ProbeBook,
    ledger: &ReservationLedger,
    exclusions: &Exclusions,
) -> (Vec<Candidate>, Vec<Hash256>) {
    let mut out = Vec::new();
    let mut digests = Vec::new();
    for offer in offers {
        if exclusions.is_excluded(&offer.worker) {
            continue;
        }
        let Some(measured) = challenges.measured(&offer.worker) else {
            continue;
        };
        let slots = ledger.free(&offer.worker);
        if slots == 0 {
            continue;
        }
        let Some(link) = probes.link(&offer.worker) else {
            continue;
        };
        out.push(Candidate {
            worker: offer.worker,
            transport_id: offer.transport_id,
            macs_per_s: offer
                .claimed_macs_per_s
                .map_or(measured, |claimed| measured.min(claimed)),
            ram_headroom_bytes: offer.ram_headroom_bytes,
            max_concurrency: slots,
            link,
            resident_layers: offer.resident_layers,
            resident_output: offer.resident_output,
        });
        digests.push(offer.digest);
    }
    (out, digests)
}

pub fn candidates(
    req: &LeaseRequirements<'_>,
    leases: &LeaseBook,
    challenges: &ChallengeBook,
    probes: &ProbeBook,
    ledger: &ReservationLedger,
    exclusions: &Exclusions,
) -> (Vec<Candidate>, Vec<Hash256>) {
    let offers = leases
        .iter()
        .filter(|current| lease::validate(current, req).is_ok())
        .map(Offer::from_lease);
    candidates_from(offers, challenges, probes, ledger, exclusions)
}
