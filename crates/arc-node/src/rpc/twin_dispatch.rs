//! Coordinator orchestration for twin execution v0.
//!
//! The rules live in `crate::twin`; this module wires them into the community
//! queue, the claim and submit handlers, validator recomputation, settlement,
//! the demand pump and the read endpoints. Twin dispatch and the demand pump
//! are off unless the operator enables them, and nothing here changes a
//! consensus rule: rewards still need five independently recomputing
//! validators. See `docs/twin-execution.md`.

use super::*;
use crate::twin::{
    self, ClaimDecision, DemandKind, DemandSource, LegOutput, LegStatus, RecomputeReason,
    RecomputeStatus, TwinConfig, Verdict,
};
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;

/// How long a group waits for its sibling leg to be claimed after the first
/// leg finished, before it resolves without it.
const TWIN_SIBLING_CLAIM_WINDOW_MS: u64 = 30_000;
const TWIN_WATCHDOG_TICK_MS: u64 = 2_000;
/// Unresolved groups one coordinator tracks at once.
const TWIN_OPEN_GROUP_CAP: usize = 64;
/// Leg job ids remembered after resolution, so late legs are recognized
/// instead of being verified and settled as ordinary jobs.
const TWIN_RESOLVED_LEG_CAP: usize = 1_024;
/// Extra time a caller waits beyond the dispatch budget for resolution.
const TWIN_RESOLUTION_GRACE_SECS: u64 = 60;
/// Longest wait for this coordinator's single twin recomputation slot when
/// validators must decide (mismatch, fallback or reward).
const TWIN_RECOMPUTE_WAIT_SECS: u64 = 600;
const TWIN_SPOT_CHECKS_PER_HOUR: u32 = 6;
const TWIN_SPOT_CHECK_BURST: u32 = 2;
const PUBLIC_DEMO_PER_HOUR: u32 = 60;
const PUBLIC_DEMO_BURST: u32 = 6;
const PUMP_GROUPS_PER_HOUR: u32 = 30;
const PUMP_BURST: u32 = 2;
const WORKER_STATE_CAP: usize = COMMUNITY_WORKER_REGISTRY_MAX;
const RECENT_RECEIPTS_MAX: usize = 50;
const VALIDATOR_RECOMPUTE_METHOD: &str = "authenticated_shard_quorum_2_of_3_per_range";

/// Per-worker twin tallies since this coordinator started.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(super) struct WorkerTwinTally {
    pub(super) legs_verified: u64,
    pub(super) legs_rejected: u64,
    pub(super) legs_unverified: u64,
}

/// What `/workers/scoreboard` adds to a worker's row when twin execution or
/// the demand pump is enabled.
#[derive(Debug, Clone, Serialize)]
pub(super) struct WorkerTwinRow {
    pub(super) region: Option<twin::RegionTag>,
    pub(super) legs_verified: u64,
    pub(super) legs_rejected: u64,
    pub(super) legs_unverified: u64,
    pub(super) last_served_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupState {
    Collecting,
    Resolving,
}

struct TwinLeg {
    item: WorkItem,
    assignment_epoch: Hash256,
    job_nonce: u64,
    enqueued_at_unix_ms: u64,
    worker: Option<String>,
    status: LegStatus,
    terminal_at_unix_ms: Option<u64>,
    result: Option<WorkResult>,
    attestation: Option<arc_types::Transaction>,
}

struct TwinGroup {
    source: DemandSource,
    public_prompt: Option<(usize, &'static str)>,
    model_id: Hash256,
    input_hash: Hash256,
    max_tokens: u32,
    created_at_unix_ms: u64,
    collect_deadline_unix_ms: u64,
    legs: Vec<TwinLeg>,
    reference: Option<twin::ReplayReference>,
    outcome_tx: Option<tokio::sync::oneshot::Sender<TwinGroupOutcome>>,
    state: GroupState,
}

struct TwinInner {
    groups: HashMap<String, TwinGroup>,
    leg_to_group: HashMap<String, (String, usize)>,
    resolved_legs: VecDeque<String>,
    regions: HashMap<String, twin::RegionTag>,
    last_served: HashMap<String, u64>,
    worker_legs: HashMap<String, WorkerTwinTally>,
    receipts: twin::ReceiptStore,
    references: twin::ReferenceBook,
    counters: twin::TwinCounters,
    throughput: twin::ThroughputWindow,
    public_demo_bucket: twin::TokenBucket,
    spot_check_bucket: twin::TokenBucket,
    pump_bucket: twin::TokenBucket,
    demand_counter: u64,
}

/// Result handed to whoever dispatched the group.
pub(super) struct TwinGroupOutcome {
    pub(super) receipt: twin::TwinReceipt,
    /// The verified leg's result. `None` unless the verdict is `verified`.
    pub(super) chosen: Option<WorkResult>,
    /// Present when validators recomputed and confirmed the chosen output.
    pub(super) verification: Option<CommunityVerificationSummary>,
}

/// A twin (two legs) or replay (one leg plus a verified reference) job.
pub(super) struct TwinDispatchRequest {
    pub(super) input: String,
    pub(super) max_tokens: u32,
    pub(super) model_id_hint: Option<String>,
    pub(super) source: DemandSource,
    pub(super) public_prompt: Option<(usize, &'static str)>,
    pub(super) reference: Option<twin::ReplayReference>,
}

enum WatchdogAction {
    Wait,
    Resolve,
    Stop,
}

enum PlannedDemand {
    Twin {
        prompt_index: usize,
        prompt: &'static str,
    },
    Replay(Box<twin::ReplayReference>),
}

struct LegSnapshot {
    item: WorkItem,
    assignment_epoch: Hash256,
    job_nonce: u64,
    worker: Option<String>,
    status: LegStatus,
    result: Option<WorkResult>,
    attestation: Option<arc_types::Transaction>,
    region: Option<twin::RegionTag>,
}

struct ResolutionInput {
    group_id: String,
    source: DemandSource,
    public_prompt: Option<(usize, &'static str)>,
    model_id: Hash256,
    input_hash: Hash256,
    max_tokens: u32,
    created_at_unix_ms: u64,
    legs: Vec<LegSnapshot>,
    reference: Option<twin::ReplayReference>,
    outcome_tx: Option<tokio::sync::oneshot::Sender<TwinGroupOutcome>>,
    cancelled_jobs: Vec<String>,
    spot_check_selected: bool,
}

struct ResolutionAccounting {
    comparison: twin::ComparisonResult,
    recompute_reason: Option<RecomputeReason>,
    recompute_status: RecomputeStatus,
    recompute_ms: Option<u64>,
    verdict: Verdict,
    verified_tokens: Option<u64>,
    compute_tokens: u64,
    leg_outcomes: Vec<(String, Option<bool>)>,
    new_reference: Option<twin::ReplayReference>,
    contradicted_reference: Option<(usize, String)>,
}

/// Coordinator-local twin execution state. One instance per `NodeState`.
pub(super) struct CommunityTwinState {
    pub(super) config: TwinConfig,
    secret: [u8; 32],
    started_at_unix_ms: u64,
    inner: parking_lot::Mutex<TwinInner>,
    recompute_permits: Arc<tokio::sync::Semaphore>,
    pump_running: AtomicBool,
}

struct PumpRunningGuard<'a>(&'a AtomicBool);

impl Drop for PumpRunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn bound_map<V>(map: &mut HashMap<String, V>, cap: usize) {
    while map.len() > cap {
        let Some(key) = map.keys().next().cloned() else {
            break;
        };
        map.remove(&key);
    }
}

fn worker_facts(
    regions: &HashMap<String, twin::RegionTag>,
    last_served: &HashMap<String, u64>,
    platforms: &HashMap<String, String>,
    worker_id: &str,
) -> twin::WorkerFacts {
    twin::WorkerFacts {
        worker_id: worker_id.to_string(),
        operator: None,
        network_group: None,
        region: regions.get(worker_id).cloned(),
        platform: platforms.get(worker_id).cloned(),
        last_served_unix_ms: last_served.get(worker_id).copied(),
    }
}

/// `(different locality, different platform)`, each unknown when either
/// side lacks the fact.
fn pair_differences(
    left: &twin::WorkerFacts,
    right: &twin::WorkerFacts,
) -> (Option<bool>, Option<bool>) {
    let regions = match (&left.region, &right.region) {
        (Some(a), Some(b)) => Some(a.locality() != b.locality()),
        _ => None,
    };
    let platforms = match (&left.platform, &right.platform) {
        (Some(a), Some(b)) => Some(a != b),
        _ => None,
    };
    (regions, platforms)
}

impl CommunityTwinState {
    pub(super) fn new(config: TwinConfig) -> Self {
        let now = now_unix_ms();
        Self {
            config: config.normalized(),
            secret: random_secret(),
            started_at_unix_ms: now,
            inner: parking_lot::Mutex::new(TwinInner {
                groups: HashMap::new(),
                leg_to_group: HashMap::new(),
                resolved_legs: VecDeque::new(),
                regions: HashMap::new(),
                last_served: HashMap::new(),
                worker_legs: HashMap::new(),
                receipts: twin::ReceiptStore::default(),
                references: twin::ReferenceBook::default(),
                counters: twin::TwinCounters::default(),
                throughput: twin::ThroughputWindow::default(),
                public_demo_bucket: twin::TokenBucket::new(
                    PUBLIC_DEMO_BURST,
                    PUBLIC_DEMO_PER_HOUR,
                    now,
                ),
                spot_check_bucket: twin::TokenBucket::new(
                    TWIN_SPOT_CHECK_BURST,
                    TWIN_SPOT_CHECKS_PER_HOUR,
                    now,
                ),
                pump_bucket: twin::TokenBucket::new(PUMP_BURST, PUMP_GROUPS_PER_HOUR, now),
                demand_counter: 0,
            }),
            recompute_permits: Arc::new(tokio::sync::Semaphore::new(1)),
            pump_running: AtomicBool::new(false),
        }
    }

    fn is_twin_leg(&self, job_id: &str) -> bool {
        self.inner.lock().leg_to_group.contains_key(job_id)
    }

    fn register_group(&self, group_id: String, group: TwinGroup) -> Result<(), &'static str> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        if inner.groups.len() >= TWIN_OPEN_GROUP_CAP {
            return Err("twin execution is at its open-group capacity");
        }
        for (index, leg) in group.legs.iter().enumerate() {
            inner
                .leg_to_group
                .insert(leg.item.job_id.clone(), (group_id.clone(), index));
        }
        inner.counters.groups_started += 1;
        inner.counters.count_demand(group.source);
        inner.groups.insert(group_id, group);
        Ok(())
    }

    fn unregister_group(&self, group_id: &str) {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        if let Some(group) = inner.groups.remove(group_id) {
            for leg in &group.legs {
                inner.leg_to_group.remove(&leg.item.job_id);
            }
        }
    }

    fn related_workers(&self, job_id: &str) -> Vec<String> {
        let guard = self.inner.lock();
        let Some((group_id, leg_index)) = guard.leg_to_group.get(job_id) else {
            return Vec::new();
        };
        let Some(group) = guard.groups.get(group_id) else {
            return Vec::new();
        };
        let mut workers: Vec<String> = group
            .legs
            .iter()
            .enumerate()
            .filter(|(index, _)| index != leg_index)
            .filter_map(|(_, leg)| leg.worker.clone())
            .collect();
        if let Some(reference) = &group.reference {
            workers.extend(reference.workers.iter().cloned());
        }
        workers
    }

    /// Atomically decide a claim for a twin leg and, when accepted, bind the
    /// leg to the claimer.
    fn decide_and_record_claim(
        &self,
        job_id: &str,
        worker_id: &str,
        idle_ids: &[String],
        platforms: &HashMap<String, String>,
        now: u64,
    ) -> ClaimDecision {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let Some((group_id, leg_index)) = inner.leg_to_group.get(job_id).cloned() else {
            return ClaimDecision::Accept;
        };
        // A resolved or resolving group's queued leg is accepted here so the
        // claim handler's pending lookup expires it instead of re-queueing it.
        let Some(group) = inner
            .groups
            .get(&group_id)
            .filter(|group| group.state == GroupState::Collecting)
        else {
            return ClaimDecision::Accept;
        };
        let Some(leg) = group.legs.get(leg_index) else {
            return ClaimDecision::Accept;
        };
        let facts = |id: &str| worker_facts(&inner.regions, &inner.last_served, platforms, id);
        let mut related = Vec::new();
        for (index, other) in group.legs.iter().enumerate() {
            if index != leg_index
                && let Some(other_worker) = other.worker.as_deref()
            {
                related.push(facts(other_worker));
            }
        }
        if let Some(reference) = &group.reference {
            for reference_worker in &reference.workers {
                related.push(facts(reference_worker));
            }
        }
        let claimer = facts(worker_id);
        let alternatives: Vec<twin::WorkerFacts> =
            idle_ids.iter().map(|id| facts(id.as_str())).collect();
        let decision = twin::decide_claim(&twin::ClaimContext {
            claimer: &claimer,
            related: &related,
            idle_alternatives: &alternatives,
            waited_ms: now.saturating_sub(leg.enqueued_at_unix_ms),
            preference_window_ms: twin::CLAIM_PREFERENCE_WINDOW_MS,
            prefer_least_recently_served: group.source.is_pump(),
        });
        let sibling_pair = if group.reference.is_none() && decision == ClaimDecision::Accept {
            related
                .first()
                .map(|sibling| pair_differences(&claimer, sibling))
        } else {
            None
        };
        match decision {
            ClaimDecision::Accept => {
                if let Some(group) = inner.groups.get_mut(&group_id)
                    && let Some(leg) = group.legs.get_mut(leg_index)
                {
                    leg.worker = Some(worker_id.to_string());
                    leg.status = LegStatus::Claimed;
                }
                inner.last_served.insert(worker_id.to_string(), now);
                bound_map(&mut inner.last_served, WORKER_STATE_CAP);
                if let Some((cross_region, cross_platform)) = sibling_pair {
                    match cross_region {
                        Some(true) => inner.counters.pairs_cross_region += 1,
                        Some(false) => inner.counters.pairs_same_region += 1,
                        None => inner.counters.pairs_region_unknown += 1,
                    }
                    if cross_platform == Some(true) {
                        inner.counters.pairs_cross_platform += 1;
                    }
                }
            }
            ClaimDecision::Refuse(_) => inner.counters.pairing_refused += 1,
            ClaimDecision::Defer(reason) => {
                if reason == twin::REASON_PREFER_LEAST_SERVED {
                    inner.counters.pairing_deferred_fairness += 1;
                } else {
                    inner.counters.pairing_deferred_diversity += 1;
                }
            }
        }
        decision
    }

    fn release_claim(&self, job_id: &str, worker_id: &str) {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let Some((group_id, leg_index)) = inner.leg_to_group.get(job_id).cloned() else {
            return;
        };
        if let Some(group) = inner.groups.get_mut(&group_id)
            && let Some(leg) = group.legs.get_mut(leg_index)
            && leg.status == LegStatus::Claimed
            && leg.worker.as_deref() == Some(worker_id)
        {
            leg.worker = None;
            leg.status = LegStatus::Unclaimed;
        }
    }

    /// Return a declined leg to the queue while its group still has
    /// collection time. A declining worker never started computing (it won a
    /// concurrent job elsewhere), so another independent worker can take it.
    fn reopen_declined_leg(&self, job_id: &str, now: u64) -> Option<(WorkItem, Hash256, u64)> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let (group_id, leg_index) = inner.leg_to_group.get(job_id).cloned()?;
        let group = inner.groups.get_mut(&group_id).filter(|group| {
            group.state == GroupState::Collecting && now < group.collect_deadline_unix_ms
        })?;
        let leg = group.legs.get_mut(leg_index)?;
        if leg.status != LegStatus::Claimed {
            return None;
        }
        leg.worker = None;
        leg.status = LegStatus::Unclaimed;
        leg.enqueued_at_unix_ms = now;
        let reopened = (leg.item.clone(), leg.assignment_epoch, leg.job_nonce);
        inner.counters.legs_requeued += 1;
        Some(reopened)
    }

    /// Record a leg's terminal submission. Returns the group id and whether
    /// every leg is now terminal, or `None` for a leg whose group already
    /// resolved.
    fn record_submission(
        &self,
        job_id: &str,
        status: LegStatus,
        result: WorkResult,
        attestation: Option<arc_types::Transaction>,
        now: u64,
    ) -> Option<(String, bool)> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let (group_id, leg_index) = inner.leg_to_group.get(job_id).cloned()?;
        let Some(group) = inner
            .groups
            .get_mut(&group_id)
            .filter(|group| group.state == GroupState::Collecting)
        else {
            inner.counters.late_legs += 1;
            return None;
        };
        let leg = group.legs.get_mut(leg_index)?;
        leg.worker = Some(result.worker_id.clone());
        leg.status = status;
        leg.terminal_at_unix_ms = Some(now);
        leg.result = Some(result);
        leg.attestation = attestation;
        let ready = group.legs.iter().all(|leg| leg.status.is_terminal());
        Some((group_id, ready))
    }

    fn watchdog_action(&self, group_id: &str, now: u64) -> WatchdogAction {
        let guard = self.inner.lock();
        let Some(group) = guard.groups.get(group_id) else {
            return WatchdogAction::Stop;
        };
        if group.state != GroupState::Collecting {
            return WatchdogAction::Stop;
        }
        if now >= group.collect_deadline_unix_ms
            || group.legs.iter().all(|leg| leg.status.is_terminal())
        {
            return WatchdogAction::Resolve;
        }
        let first_terminal = group
            .legs
            .iter()
            .filter_map(|leg| leg.terminal_at_unix_ms)
            .min();
        let sibling_unclaimed = group
            .legs
            .iter()
            .any(|leg| leg.status == LegStatus::Unclaimed);
        if sibling_unclaimed
            && first_terminal
                .is_some_and(|at| now.saturating_sub(at) >= TWIN_SIBLING_CLAIM_WINDOW_MS)
        {
            return WatchdogAction::Resolve;
        }
        WatchdogAction::Wait
    }

    /// Move a collecting group to resolving exactly once and snapshot it.
    fn begin_resolution(&self, group_id: &str) -> Option<ResolutionInput> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let group = inner.groups.get_mut(group_id)?;
        if group.state != GroupState::Collecting {
            return None;
        }
        group.state = GroupState::Resolving;
        let mut cancelled_jobs = Vec::new();
        for leg in &mut group.legs {
            match leg.status {
                LegStatus::Unclaimed => cancelled_jobs.push(leg.item.job_id.clone()),
                LegStatus::Claimed => leg.status = LegStatus::Abandoned,
                _ => {}
            }
        }
        let legs = group
            .legs
            .iter()
            .map(|leg| LegSnapshot {
                item: leg.item.clone(),
                assignment_epoch: leg.assignment_epoch,
                job_nonce: leg.job_nonce,
                worker: leg.worker.clone(),
                status: leg.status,
                result: leg.result.clone(),
                attestation: leg.attestation.clone(),
                region: leg
                    .worker
                    .as_ref()
                    .and_then(|worker| inner.regions.get(worker).cloned()),
            })
            .collect();
        Some(ResolutionInput {
            group_id: group_id.to_string(),
            source: group.source,
            public_prompt: group.public_prompt,
            model_id: group.model_id,
            input_hash: group.input_hash,
            max_tokens: group.max_tokens,
            created_at_unix_ms: group.created_at_unix_ms,
            legs,
            reference: group.reference.clone(),
            outcome_tx: group.outcome_tx.take(),
            cancelled_jobs,
            spot_check_selected: twin::spot_check_selected(
                &self.secret,
                group_id,
                self.config.spot_check_per_mille,
            ),
        })
    }

    fn take_spot_check_token(&self, now: u64) -> bool {
        self.inner.lock().spot_check_bucket.try_take(now)
    }

    fn finish_resolution(
        &self,
        group_id: &str,
        receipt: twin::TwinReceipt,
        accounting: ResolutionAccounting,
        now: u64,
    ) {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        match accounting.comparison {
            twin::ComparisonResult::Match => inner.counters.groups_matched += 1,
            twin::ComparisonResult::Mismatch => inner.counters.groups_mismatched += 1,
            twin::ComparisonResult::Incomplete => inner.counters.groups_incomplete += 1,
            twin::ComparisonResult::ReferenceMatch => inner.counters.replays_matched += 1,
            twin::ComparisonResult::ReferenceMismatch => inner.counters.replays_mismatched += 1,
        }
        match (accounting.recompute_reason, accounting.recompute_status) {
            (Some(_), RecomputeStatus::SkippedBusy) => inner.counters.recompute_skipped_busy += 1,
            (Some(_), RecomputeStatus::Unavailable) => inner.counters.recompute_unavailable += 1,
            (Some(RecomputeReason::SpotCheck), _) => inner.counters.recompute_spot_check += 1,
            (Some(RecomputeReason::Mismatch), _) => inner.counters.recompute_mismatch += 1,
            (Some(RecomputeReason::Fallback), _) => inner.counters.recompute_fallback += 1,
            (Some(RecomputeReason::Reward), _) => inner.counters.recompute_reward += 1,
            (None, _) => {
                if accounting.comparison.agrees() {
                    inner.counters.recomputes_avoided += 1;
                }
            }
        }
        if accounting.recompute_status == RecomputeStatus::Contradicted {
            inner.counters.recompute_contradicted += 1;
        }
        if let Some(elapsed_ms) = accounting.recompute_ms {
            inner.counters.recompute_ms_total =
                inner.counters.recompute_ms_total.saturating_add(elapsed_ms);
        }
        inner.counters.worker_compute_tokens = inner
            .counters
            .worker_compute_tokens
            .saturating_add(accounting.compute_tokens);
        if accounting.verdict == Verdict::Verified
            && let Some(tokens) = accounting.verified_tokens
        {
            inner.counters.verified_jobs += 1;
            inner.counters.verified_tokens = inner.counters.verified_tokens.saturating_add(tokens);
            inner.throughput.record(now, tokens);
        }
        for (worker, valid) in &accounting.leg_outcomes {
            let tally = inner.worker_legs.entry(worker.clone()).or_default();
            match valid {
                Some(true) => tally.legs_verified += 1,
                Some(false) => {
                    tally.legs_rejected += 1;
                    inner.counters.rejected_legs += 1;
                }
                None => tally.legs_unverified += 1,
            }
        }
        bound_map(&mut inner.worker_legs, WORKER_STATE_CAP);
        if let Some(reference) = accounting.new_reference {
            inner.references.record(reference);
        }
        if let Some((prompt_index, source_group_id)) = &accounting.contradicted_reference {
            inner.references.evict(*prompt_index, source_group_id);
            inner.counters.references_contradicted += 1;
        }
        inner.receipts.insert(receipt);
        if let Some(group) = inner.groups.remove(group_id) {
            for leg in &group.legs {
                inner.resolved_legs.push_back(leg.item.job_id.clone());
            }
        }
        while inner.resolved_legs.len() > TWIN_RESOLVED_LEG_CAP {
            let Some(job_id) = inner.resolved_legs.pop_front() else {
                break;
            };
            inner.leg_to_group.remove(&job_id);
        }
    }

    fn set_region(&self, worker_id: &str, tag: twin::RegionTag) {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        if !inner.regions.contains_key(worker_id) && inner.regions.len() >= WORKER_STATE_CAP {
            let oldest = inner
                .regions
                .iter()
                .min_by_key(|(_, tag)| tag.measured_at_unix_ms)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                inner.regions.remove(&oldest);
            }
        }
        inner.regions.insert(worker_id.to_string(), tag);
    }

    fn admit_public_demo(&self, prompt: &str, now: u64) -> Result<(), (StatusCode, String)> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        if let Some(reason) = twin::public_demo_prompt_rejection(prompt) {
            inner.counters.public_demo_rejected += 1;
            return Err((StatusCode::BAD_REQUEST, reason.to_string()));
        }
        if !inner.public_demo_bucket.try_take(now) {
            inner.counters.public_demo_rate_limited += 1;
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "the public demo is rate limited on this coordinator; retry in a minute"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn plan_tick(
        &self,
        idle_workers: &[String],
        model_id: &Hash256,
        now: u64,
    ) -> Result<PlannedDemand, &'static str> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let roll = (random_u64() % 1_000) as u16;
        let kind = twin::plan_demand(
            idle_workers.len(),
            self.config.twin_execution,
            inner.references.has_model(model_id),
            roll,
            twin::DEFAULT_REPLAY_SHARE_PER_MILLE,
        )?;
        let planned = match kind {
            DemandKind::TwinDemo => {
                let (prompt_index, prompt) = twin::public_demo_prompt(inner.demand_counter);
                PlannedDemand::Twin {
                    prompt_index,
                    prompt,
                }
            }
            DemandKind::Replay => {
                let reference = inner
                    .references
                    .pick(random_u64(), model_id)
                    .cloned()
                    .ok_or("no verified reference is available")?;
                if idle_workers
                    .iter()
                    .all(|worker| reference.workers.contains(worker))
                {
                    return Err("only the reference's own workers are idle");
                }
                PlannedDemand::Replay(Box::new(reference))
            }
        };
        if !inner.pump_bucket.try_take(now) {
            return Err("the demand pump's hourly budget is spent");
        }
        inner.demand_counter = inner.demand_counter.wrapping_add(1);
        Ok(planned)
    }

    fn receipt(&self, job_or_group_id: &str) -> Option<twin::TwinReceipt> {
        self.inner.lock().receipts.get(job_or_group_id).cloned()
    }

    fn recent_receipts(&self, limit: usize) -> Vec<twin::TwinReceipt> {
        self.inner
            .lock()
            .receipts
            .recent(limit)
            .into_iter()
            .cloned()
            .collect()
    }

    fn worker_rows(&self) -> HashMap<String, WorkerTwinRow> {
        let guard = self.inner.lock();
        let mut ids: HashSet<&String> = guard.regions.keys().collect();
        ids.extend(guard.worker_legs.keys());
        ids.extend(guard.last_served.keys());
        ids.into_iter()
            .map(|id| {
                let tally = guard.worker_legs.get(id).copied().unwrap_or_default();
                (
                    id.clone(),
                    WorkerTwinRow {
                        region: guard.regions.get(id).cloned(),
                        legs_verified: tally.legs_verified,
                        legs_rejected: tally.legs_rejected,
                        legs_unverified: tally.legs_unverified,
                        last_served_unix_ms: guard.last_served.get(id).copied(),
                    },
                )
            })
            .collect()
    }
}

// ─── Entry points used by rpc.rs ────────────────────────────────────────────

/// Twin dispatch needs the operator switch and at least two live workers.
pub(super) fn twin_dispatch_ready(node: &NodeState) -> bool {
    node.community_twin.config.twin_execution && live_inference_worker_count(node) >= 2
}

pub(super) fn is_twin_leg(node: &NodeState, job_id: &str) -> bool {
    node.community_twin.is_twin_leg(job_id)
}

/// Pairing decision for a dequeued twin leg, recorded atomically when it is
/// accepted. `None` for ordinary jobs.
pub(super) fn claim_decision(
    node: &NodeState,
    job_id: &str,
    worker_id: &str,
) -> Option<ClaimDecision> {
    if !node.community_twin.is_twin_leg(job_id) {
        return None;
    }
    let idle_ids: Vec<String> = node
        .community_active_jobs
        .iter()
        .filter(|entry| entry.value().is_empty() && entry.key().as_str() != worker_id)
        .map(|entry| entry.key().clone())
        .collect();
    let related = node.community_twin.related_workers(job_id);
    let mut platforms = HashMap::new();
    for id in idle_ids
        .iter()
        .chain(related.iter())
        .map(String::as_str)
        .chain(std::iter::once(worker_id))
    {
        if let Some(entry) = node.community_workers.get(id) {
            platforms.insert(id.to_string(), entry.value().0.platform.clone());
        }
    }
    Some(node.community_twin.decide_and_record_claim(
        job_id,
        worker_id,
        &idle_ids,
        &platforms,
        now_unix_ms(),
    ))
}

/// The `no_work` reason for a refused or deferred claim.
pub(super) fn refusal_reason(decision: ClaimDecision) -> Option<&'static str> {
    match decision {
        ClaimDecision::Accept => None,
        ClaimDecision::Refuse(reason) | ClaimDecision::Defer(reason) => Some(reason),
    }
}

/// Undo an accepted twin claim whose pending assignment vanished.
pub(super) fn release_claim(node: &NodeState, job_id: &str, worker_id: &str) {
    node.community_twin.release_claim(job_id, worker_id);
}

/// Put a dequeued item back for another worker. If the queue is full the
/// pending record is dropped so no dispatcher waits on a lost item.
pub(super) fn requeue_item(node: &NodeState, item: WorkItem) {
    if let Some(tx) = node.community_work_tx.as_ref() {
        let job_id = item.job_id.clone();
        if tx.try_send(item).is_err()
            && let Some(results) = node.community_work_results.as_ref()
        {
            results.remove(&job_id);
        }
    }
}

/// Re-insert a reopened leg's pending record and put it back on the queue.
fn requeue_twin_leg(
    node: &NodeState,
    item: WorkItem,
    assignment_epoch: Hash256,
    job_nonce: u64,
) -> bool {
    let (Some(tx), Some(results)) = (
        node.community_work_tx.as_ref(),
        node.community_work_results.as_ref(),
    ) else {
        return false;
    };
    match results.entry(item.job_id.clone()) {
        Entry::Occupied(_) => return false,
        Entry::Vacant(entry) => {
            let (leg_sender, _unused_receiver) =
                tokio::sync::oneshot::channel::<CommunityDispatchOutcome>();
            entry.insert(PendingCommunityWork {
                item: item.clone(),
                assignment_epoch,
                job_nonce,
                assigned_worker: None,
                submitting: false,
                sender: leg_sender,
            });
        }
    }
    let job_id = item.job_id.clone();
    if tx.try_send(item).is_err() {
        results.remove(&job_id);
        return false;
    }
    true
}

/// Record a twin leg's authenticated submission. The comparison and any
/// validator recomputation run in a node-owned task, so the worker's HTTP
/// request is not held open and a dropped connection cannot cancel them.
pub(super) fn submit_twin_leg(
    node: &NodeState,
    reservation: PendingSubmissionReservation,
    assigned_worker: Option<String>,
    result: WorkResult,
    attestation: Option<arc_types::Transaction>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let job_id = result.job_id.clone();
    if reservation.take().is_none() {
        return Err((
            StatusCode::CONFLICT,
            format!("pending twin leg {job_id} changed before it was recorded"),
        ));
    }
    let _active_job_reservation = ActiveCommunityJobReservation {
        active_jobs: node.community_active_jobs.clone(),
        worker_id: assigned_worker,
        job_id: job_id.clone(),
    };
    if let Some(mut entry) = node.community_workers.get_mut(&result.worker_id) {
        entry.value_mut().1 = std::time::Instant::now();
    }
    if result.declined
        && let Some((item, assignment_epoch, job_nonce)) = node
            .community_twin
            .reopen_declined_leg(&job_id, now_unix_ms())
        && requeue_twin_leg(node, item, assignment_epoch, job_nonce)
    {
        return Ok(Json(json!({
            "ok": true,
            "job_id": job_id,
            "twin": {
                "status": "requeued",
            },
        })));
    }
    let status = if result.success {
        LegStatus::Submitted
    } else if result.declined {
        LegStatus::Declined
    } else {
        LegStatus::Failed
    };
    match node
        .community_twin
        .record_submission(&job_id, status, result, attestation, now_unix_ms())
    {
        Some((group_id, ready)) => {
            if ready {
                spawn_twin_resolution(node, group_id.clone());
            }
            let twin_status = if ready {
                "resolving"
            } else {
                "awaiting_sibling"
            };
            Ok(Json(json!({
                "ok": true,
                "job_id": job_id,
                "twin": {
                    "group_id": group_id,
                    "status": twin_status,
                },
            })))
        }
        None => Ok(Json(json!({
            "ok": true,
            "job_id": job_id,
            "twin": {
                "status": "late_after_resolution",
            },
        }))),
    }
}

/// Dispatch a twin (two legs) or replay (one leg) job and wait for its
/// resolution. Pre-enqueue failures are safe for a local fallback.
pub(super) async fn dispatch_twin(
    node: &NodeState,
    request: TwinDispatchRequest,
) -> Result<TwinGroupOutcome, CommunityDispatchError> {
    let _worker_execution_permit = node
        .native_request_admission
        .worker_execution_gate()
        .try_enter()
        .ok_or_else(|| CommunityDispatchError::before_enqueue("worker is quiescing for update"))?;
    let _inference_permit = node
        .public_inference_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            CommunityDispatchError::before_enqueue(
                "public inference capacity is saturated; retry after 2 seconds",
            )
        })?;
    let TwinDispatchRequest {
        input,
        max_tokens,
        model_id_hint,
        source,
        public_prompt,
        reference,
    } = request;
    if max_tokens == 0 || max_tokens > INFERENCE_RUN_MAX_TOKENS {
        return Err(CommunityDispatchError::before_enqueue(format!(
            "community max_tokens must be in 1..={INFERENCE_RUN_MAX_TOKENS}, got {max_tokens}"
        )));
    }
    let model_id_hash = model_id_hint
        .as_deref()
        .ok_or_else(|| {
            CommunityDispatchError::before_enqueue(
                "community dispatch requires an exact model artifact commitment",
            )
        })
        .and_then(|value| {
            parse_hash256_hex(value, "model_id").map_err(CommunityDispatchError::before_enqueue)
        })?;
    let model = node.inference_model.as_ref().ok_or_else(|| {
        CommunityDispatchError::before_enqueue(
            "community dispatch requires the coordinator's exact tokenizer/model",
        )
    })?;
    let (_, generation_preflight) = community_worker_generation_context(model, &input, max_tokens)
        .map_err(CommunityDispatchError::before_enqueue)?;
    let timeout_secs = community_dispatch_timeout_secs(generation_preflight.required_positions)
        .map_err(CommunityDispatchError::before_enqueue)?;
    let tx = node
        .community_work_tx
        .as_ref()
        .ok_or_else(|| CommunityDispatchError::before_enqueue("community work queue not wired"))?
        .clone();
    let results = node
        .community_work_results
        .as_ref()
        .ok_or_else(|| {
            CommunityDispatchError::before_enqueue("community work results map not wired")
        })?
        .clone();

    let model_id = format!("0x{}", model_id_hash.to_hex());
    let input_hash = arc_crypto::hash_bytes(input.as_bytes());
    let submitted_at_unix_ms = now_unix_ms();
    let submitted_at = i64::try_from(submitted_at_unix_ms).map_err(|_| {
        CommunityDispatchError::before_enqueue("community submission timestamp overflow")
    })?;
    let expires_at_unix_ms = timeout_secs
        .checked_add(COMMUNITY_LATE_SUBMIT_GRACE_SECS)
        .and_then(|seconds| seconds.checked_mul(1_000))
        .and_then(|window_ms| submitted_at_unix_ms.checked_add(window_ms))
        .ok_or_else(|| {
            CommunityDispatchError::before_enqueue("community assignment expiry overflow")
        })?;
    // Legs run in parallel, so collection gets half the reviewed budget; the
    // other half covers any validator recomputation and reward approvals.
    let collect_deadline_unix_ms =
        submitted_at_unix_ms.saturating_add(timeout_secs.saturating_mul(1_000) / 2);
    let transaction_domain = node
        .state
        .transaction_domain_hash()
        .map(|domain| format!("0x{}", domain.to_hex()));
    let leg_count = if reference.is_some() { 1 } else { 2 };
    let mut legs = Vec::with_capacity(leg_count);
    for _ in 0..leg_count {
        let job_nonce = node.attestation_nonce.fetch_add(1, Ordering::Relaxed);
        let job_id = community_job_id(
            &node.validator_address,
            &node.community_job_epoch,
            &model_id_hash,
            &input_hash,
            max_tokens,
            job_nonce,
        );
        legs.push(TwinLeg {
            item: WorkItem {
                job_id,
                input: input.clone(),
                max_tokens,
                model_id: Some(model_id.clone()),
                execution_profile:
                    arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE
                        .to_string(),
                transaction_domain: transaction_domain.clone(),
                expected_worker_id: None,
                submitted_at_unix_ms: submitted_at,
                expires_at_unix_ms,
            },
            assignment_epoch: node.community_job_epoch,
            job_nonce,
            enqueued_at_unix_ms: submitted_at_unix_ms,
            worker: None,
            status: LegStatus::Unclaimed,
            terminal_at_unix_ms: None,
            result: None,
            attestation: None,
        });
    }
    let group_id = legs[0].item.job_id.clone();
    let pending_specs: Vec<(WorkItem, Hash256, u64)> = legs
        .iter()
        .map(|leg| (leg.item.clone(), leg.assignment_epoch, leg.job_nonce))
        .collect();
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel::<TwinGroupOutcome>();
    node.community_twin
        .register_group(
            group_id.clone(),
            TwinGroup {
                source,
                public_prompt,
                model_id: model_id_hash,
                input_hash,
                max_tokens,
                created_at_unix_ms: submitted_at_unix_ms,
                collect_deadline_unix_ms,
                legs,
                reference,
                outcome_tx: Some(outcome_tx),
                state: GroupState::Collecting,
            },
        )
        .map_err(CommunityDispatchError::before_enqueue)?;

    // Dropping a reservation removes an unclaimed leg immediately and keeps
    // a claimed leg for its bounded late-submit grace, as for single jobs.
    let mut reservations = Vec::with_capacity(pending_specs.len());
    for (item, assignment_epoch, job_nonce) in &pending_specs {
        match results.entry(item.job_id.clone()) {
            Entry::Occupied(_) => {
                node.community_twin.unregister_group(&group_id);
                return Err(CommunityDispatchError::before_enqueue(
                    "community job already exists; retry its exact status",
                ));
            }
            Entry::Vacant(entry) => {
                // Twin legs report through their group, never through a
                // per-leg dispatcher channel.
                let (leg_sender, _unused_receiver) =
                    tokio::sync::oneshot::channel::<CommunityDispatchOutcome>();
                entry.insert(PendingCommunityWork {
                    item: item.clone(),
                    assignment_epoch: *assignment_epoch,
                    job_nonce: *job_nonce,
                    assigned_worker: None,
                    submitting: false,
                    sender: leg_sender,
                });
            }
        }
        reservations.push(PendingDispatchReservation {
            results: results.clone(),
            active_jobs: node.community_active_jobs.clone(),
            job_id: item.job_id.clone(),
        });
    }
    for (index, (item, _, _)) in pending_specs.into_iter().enumerate() {
        if let Err(error) = tx.try_send(item) {
            node.community_twin.unregister_group(&group_id);
            let message = format!("community queue unavailable: {error}");
            return Err(if index == 0 {
                CommunityDispatchError::before_enqueue(message)
            } else {
                CommunityDispatchError::after_enqueue(message)
            });
        }
    }
    spawn_twin_watchdog(node, group_id);

    let wait = Duration::from_secs(timeout_secs.saturating_add(TWIN_RESOLUTION_GRACE_SECS));
    let outcome = match tokio::time::timeout(wait, outcome_rx).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(_)) => Err(CommunityDispatchError::after_enqueue(
            "twin group ended without reporting an outcome",
        )),
        Err(_) => Err(CommunityDispatchError::timed_out(format!(
            "twin group did not resolve within {}s",
            wait.as_secs()
        ))),
    };
    drop(reservations);
    outcome
}

/// Screen and rate-limit a caller request marked `public_demo`.
pub(super) fn admit_public_demo(
    node: &NodeState,
    prompt: &str,
) -> Result<(), (StatusCode, String)> {
    node.community_twin.admit_public_demo(prompt, now_unix_ms())
}

/// The `/inference/run` response for a twin-executed request. Output text is
/// returned only for a verified group.
pub(super) fn twin_inference_response(
    node: &NodeState,
    input_text: &str,
    outcome: TwinGroupOutcome,
    live_workers: usize,
    dispatch_ms: u64,
    public_demo: bool,
) -> Value {
    let TwinGroupOutcome {
        receipt,
        chosen,
        verification,
    } = outcome;
    let verification_value = json!({
        "method": "twin_execution_v0",
        "verdict": receipt.verdict,
        "verified_by": receipt.verified_by,
        "comparison": receipt.comparison,
        "validator_recompute": receipt.validator_recompute,
        "validator_quorum": verification,
    });
    let twin_workers: Vec<Option<String>> = receipt
        .legs
        .iter()
        .map(|leg| leg.worker_id.clone())
        .collect();
    let mut response = match chosen {
        Some(chosen) if receipt.verdict == Verdict::Verified => {
            if let Some(model_id) = node.model_artifact_id {
                let input = node.inference_model.as_ref().map(|model| {
                    let input = if public_demo {
                        model.apply_chat_template(input_text)
                    } else {
                        input_text.to_string()
                    };
                    model.encode(&input).len() as u64
                });
                node.serving_stats.lock().record(serving_stats::Answer {
                    model: model_id.0,
                    input,
                    output: chosen.tokens_generated,
                    verified: true,
                    cached: false,
                    timing: serving_stats::AnswerTiming::default(),
                    hops: Vec::new(),
                });
            }
            retain_inference_result(
                node,
                chosen.job_id.clone(),
                json!({
                    "input_hash": receipt.input_hash,
                    "output_hash": chosen.output_hash,
                    "model": format!("community:{}", chosen.engine),
                    "model_hash": receipt.model_id,
                    "ms_per_token": chosen.ms_per_token,
                    "tokens_generated": chosen.tokens_generated,
                    "engine": chosen.engine,
                    "deterministic": chosen.engine.contains("integer"),
                    "worker_id": chosen.worker_id,
                    "observed_at_unix_ms": now_unix_ms(),
                    "verification": verification_value,
                    "settlement": receipt.settlement,
                    "twin_group_id": receipt.group_id,
                }),
            );
            json!({
                "success": true,
                "routed_via": format!("community:{}", chosen.worker_id),
                "inference": {
                    "model": "community-served",
                    "model_hash": receipt.model_id,
                    "input": input_text,
                    "input_hash": receipt.input_hash,
                    "output": chosen.output,
                    "output_hash": chosen.output_hash,
                    "tokens_generated": chosen.tokens_generated,
                    "inference_ms": chosen.total_ms,
                    "ms_per_token": chosen.ms_per_token,
                    "encode_ms": 0,
                    "deterministic": chosen.engine.contains("integer"),
                    "engine": chosen.engine,
                    "dispatch_ms": dispatch_ms,
                },
                "attestation": {
                    "status": "worker_certificate_handled_by_settlement",
                    "request_overrides_applied": false,
                    "note": "bond and challenge_period request fields apply only to the local fallback; community certificates use the protocol-fixed shape reported by settlement",
                },
                "worker": {
                    "worker_id": chosen.worker_id,
                    "twin_workers": twin_workers,
                    "live_workers_at_dispatch": live_workers,
                },
                "verification": verification_value,
                "settlement": receipt.settlement,
            })
        }
        _ => json!({
            "success": false,
            "routed_via": "community-twin",
            "error": format!(
                "twin execution ended {}; no output is returned without verification",
                receipt.verdict.as_str()
            ),
            "worker": {
                "twin_workers": twin_workers,
                "live_workers_at_dispatch": live_workers,
            },
            "verification": verification_value,
        }),
    };
    response["twin"] = serde_json::to_value(&receipt).unwrap_or(Value::Null);
    if public_demo {
        response["public_demo"] = json!({
            "public": true,
            "testnet": true,
            "notice": twin::PUBLIC_DEMO_NOTICE,
            "prompt_template": "model_chat_template",
            "max_tokens_cap": twin::PUBLIC_DEMO_MAX_TOKENS,
        });
    }
    response
}

/// Start the demand pump when the operator enabled it.
pub(super) fn spawn_community_demand_pump(node: &NodeState) {
    if !node.community_twin.config.demand_pump {
        return;
    }
    tracing::info!(
        interval_secs = node.community_twin.config.demand_interval_secs,
        twin_execution = node.community_twin.config.twin_execution,
        dry_run = node.community_twin.config.demand_dry_run,
        "community demand pump enabled: public demo and replay jobs for idle workers"
    );
    let pump_node = node.clone();
    let mut shutdown = node.runtime_shutdown.clone();
    spawn_node_runtime_task(node, async move {
        loop {
            let interval = jittered_interval(pump_node.community_twin.config.demand_interval_secs);
            tokio::select! {
                biased;
                _ = wait_for_optional_runtime_shutdown(&mut shutdown) => return,
                _ = tokio::time::sleep(interval) => {}
            }
            tokio::select! {
                biased;
                _ = wait_for_optional_runtime_shutdown(&mut shutdown) => return,
                tick = run_demand_tick(&pump_node) => {
                    if let Err(reason) = tick {
                        tracing::debug!(reason = %reason, "community demand tick skipped");
                    }
                }
            }
        }
    });
}

/// Scoreboard summary, present only when twin execution or the pump is on.
pub(super) fn scoreboard_summary(node: &NodeState) -> Option<Value> {
    let config = node.community_twin.config;
    if !config.twin_execution && !config.demand_pump {
        return None;
    }
    let now = now_unix_ms();
    let mut guard = node.community_twin.inner.lock();
    let inner = &mut *guard;
    let (jobs, tokens) = inner.throughput.summary(now);
    Some(json!({
        "twin_execution": config.twin_execution,
        "demand_pump": config.demand_pump,
        "groups_matched": inner.counters.groups_matched,
        "groups_mismatched": inner.counters.groups_mismatched,
        "twin_match_rate": inner.counters.twin_match_rate(),
        "verified_jobs_last_hour": jobs,
        "verified_tokens_last_hour": tokens,
        "verified_tokens_per_second_last_hour": tokens as f64 / 3_600.0,
        "stats": "/community/twin_stats",
    }))
}

/// Per-worker rows for the scoreboard; empty while both switches are off.
pub(super) fn worker_rows(node: &NodeState) -> HashMap<String, WorkerTwinRow> {
    let config = node.community_twin.config;
    if !config.twin_execution && !config.demand_pump {
        return HashMap::new();
    }
    node.community_twin.worker_rows()
}

impl CommunityAuthenticatedPayload for twin::CommunityRegionReport {
    fn signer_id(&self) -> &str {
        &self.worker_id
    }

    fn validate_for_auth(&self) -> Result<(), String> {
        self.parsed_samples().map(|_| ())
    }
}

/// POST /community/region: a registered worker's signed round-trip times to
/// the validator origins. Only the coarse label is ever published.
pub(super) async fn community_region_signed(
    AxumState(node): AxumState<NodeState>,
    Json(signed): Json<CommunitySignedRequest<twin::CommunityRegionReport>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let report = authenticate_community_request(&node, twin::COMMUNITY_REGION_PATH, signed)?;
    let samples = report
        .parsed_samples()
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let tag = twin::classify_region(&samples, now_unix_ms()).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "region report has no usable sample".to_string(),
        )
    })?;
    node.community_twin
        .set_region(&report.worker_id, tag.clone());
    Ok(Json(json!({
        "ok": true,
        "region": tag,
    })))
}

/// GET /community/twin_stats
pub(super) async fn community_twin_stats(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let now = now_unix_ms();
    let config = node.community_twin.config;
    let eligible_live = live_inference_worker_count(&node);
    let mut guard = node.community_twin.inner.lock();
    let inner = &mut *guard;
    let (jobs, tokens) = inner.throughput.summary(now);
    let counters = inner.counters.clone();
    let performed = counters.recompute_spot_check
        + counters.recompute_mismatch
        + counters.recompute_fallback
        + counters.recompute_reward;
    let mut by_region: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for tag in inner.regions.values() {
        *by_region.entry(tag.region.clone()).or_default() += 1;
    }
    let body = json!({
        "schema": twin::TWIN_STATS_SCHEMA,
        "coordinator": format!("0x{}", node.validator_address.to_hex()),
        "since_unix_ms": node.community_twin.started_at_unix_ms,
        "config": {
            "twin_execution": config.twin_execution,
            "demand_pump": config.demand_pump,
            "demand_dry_run": config.demand_dry_run,
            "spot_check_per_mille": config.spot_check_per_mille,
            "spot_checks_per_hour_max": TWIN_SPOT_CHECKS_PER_HOUR,
            "demand_interval_secs": config.demand_interval_secs,
            "pump_groups_per_hour_max": PUMP_GROUPS_PER_HOUR,
            "replay_share_per_mille": twin::DEFAULT_REPLAY_SHARE_PER_MILLE,
            "pump_max_tokens": twin::PUMP_MAX_TOKENS,
            "public_demo_max_tokens": twin::PUBLIC_DEMO_MAX_TOKENS,
            "public_demo_per_hour_max": PUBLIC_DEMO_PER_HOUR,
            "checkpoint_interval_tokens": twin::CHECKPOINT_INTERVAL_TOKENS,
        },
        "counters": counters,
        "twin_match_rate": counters.twin_match_rate(),
        "throughput_last_hour": {
            "window_secs": twin::ThroughputWindow::WINDOW_MS / 1_000,
            "verified_jobs": jobs,
            "verified_tokens": tokens,
            "verified_tokens_per_second": tokens as f64 / 3_600.0,
        },
        "validator_recompute": {
            "performed": performed,
            "avoided": counters.recomputes_avoided,
            "skipped_busy": counters.recompute_skipped_busy,
            "unavailable": counters.recompute_unavailable,
            "mean_ms": (performed > 0).then(|| counters.recompute_ms_total / performed),
        },
        "workers": {
            "eligible_live": eligible_live,
            "region_tagged": inner.regions.len(),
            "by_region": by_region,
        },
        "open_groups": inner.groups.len(),
        "replay_references": inner.references.len(),
        "receipts_retained": inner.receipts.len(),
        "disclosure": twin::CENTRALIZATION_DISCLOSURE,
    });
    Json(body)
}

/// GET /community/twin/{job_id}: the receipt for a twin group or either leg.
pub(super) async fn community_twin_receipt(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(job_id): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let receipt = node.community_twin.receipt(&job_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "twin receipt is unknown or has aged out of this coordinator's bounded store"
                .to_string(),
        )
    })?;
    serde_json::to_value(receipt)
        .map(Json)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

/// GET /community/twin_receipts?limit=N: the newest receipts.
pub(super) async fn community_twin_receipts(
    AxumState(node): AxumState<NodeState>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(20)
        .min(RECENT_RECEIPTS_MAX);
    let receipts = node.community_twin.recent_receipts(limit);
    Json(json!({
        "schema": "arc.community.twin-receipts.v1",
        "count": receipts.len(),
        "receipts": receipts,
    }))
}

// ─── Resolution ─────────────────────────────────────────────────────────────

fn spawn_twin_watchdog(node: &NodeState, group_id: String) {
    let task_node = node.clone();
    let mut shutdown = node.runtime_shutdown.clone();
    spawn_node_runtime_task(node, async move {
        loop {
            tokio::select! {
                biased;
                _ = wait_for_optional_runtime_shutdown(&mut shutdown) => return,
                _ = tokio::time::sleep(Duration::from_millis(TWIN_WATCHDOG_TICK_MS)) => {}
            }
            match task_node
                .community_twin
                .watchdog_action(&group_id, now_unix_ms())
            {
                WatchdogAction::Wait => {}
                WatchdogAction::Stop => return,
                WatchdogAction::Resolve => {
                    resolve_until_shutdown(&task_node, &group_id, &mut shutdown).await;
                    return;
                }
            }
        }
    });
}

fn spawn_twin_resolution(node: &NodeState, group_id: String) {
    let task_node = node.clone();
    let mut shutdown = node.runtime_shutdown.clone();
    spawn_node_runtime_task(node, async move {
        resolve_until_shutdown(&task_node, &group_id, &mut shutdown).await;
    });
}

async fn resolve_until_shutdown(
    node: &NodeState,
    group_id: &str,
    shutdown: &mut Option<tokio::sync::watch::Receiver<bool>>,
) {
    tokio::select! {
        biased;
        _ = wait_for_optional_runtime_shutdown(shutdown) => {}
        _ = resolve_twin_group(node, group_id) => {}
    }
}

/// The comparable commitments of a leg that submitted a successful result in
/// the canonical execution profile.
fn leg_output(leg: &LegSnapshot) -> Option<LegOutput> {
    if leg.status != LegStatus::Submitted {
        return None;
    }
    let result = leg.result.as_ref()?;
    if !result.success || validate_community_reward_profile(result).is_err() {
        return None;
    }
    Some(LegOutput {
        output_hash: parse_hash256_hex(&result.output_hash, "output_hash").ok()?,
        tokens_generated: result.tokens_generated,
        text_digest: arc_crypto::hash_bytes(result.output.as_bytes()),
    })
}

fn canonical_leg_output(canonical: &CommunityCanonicalRecompute) -> LegOutput {
    LegOutput {
        output_hash: canonical.output_hash,
        tokens_generated: canonical.generated.len() as u64,
        text_digest: arc_crypto::hash_bytes(canonical.output_text.as_bytes()),
    }
}

/// The first leg that could carry a protocol reward: caller demand, a
/// comparable output, a worker certificate and remaining issuance capacity.
fn reward_candidate_leg(
    node: &NodeState,
    input: &ResolutionInput,
    outputs: &[Option<LegOutput>],
) -> Option<usize> {
    if !input.source.reward_eligible() || !community_rewards_v1_protocol_active(node) {
        return None;
    }
    input.legs.iter().enumerate().find_map(|(index, leg)| {
        let comparable = outputs.get(index).is_some_and(Option::is_some);
        let worker = leg
            .result
            .as_ref()
            .and_then(|result| parse_hash256_hex(&result.worker_id, "worker_id").ok())?;
        (comparable
            && leg.attestation.is_some()
            && community_reward_issuance_ready_for(node, worker))
        .then_some(index)
    })
}

async fn run_twin_recompute(
    node: &NodeState,
    item: &WorkItem,
    reason: RecomputeReason,
) -> Result<(CommunityCanonicalRecompute, u64), RecomputeStatus> {
    let Some(_worker_execution_permit) = node
        .native_request_admission
        .worker_execution_gate()
        .try_enter()
    else {
        return Err(RecomputeStatus::Unavailable);
    };
    let permits = node.community_twin.recompute_permits.clone();
    let _permit = if reason == RecomputeReason::SpotCheck {
        // A spot check never queues: it runs only while this validator is
        // not already recomputing for an approval or another twin group.
        if node.community_approval_permits.available_permits() == 0 {
            return Err(RecomputeStatus::SkippedBusy);
        }
        permits
            .try_acquire_owned()
            .map_err(|_| RecomputeStatus::SkippedBusy)?
    } else {
        match tokio::time::timeout(
            Duration::from_secs(TWIN_RECOMPUTE_WAIT_SECS),
            permits.acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            _ => return Err(RecomputeStatus::SkippedBusy),
        }
    };
    let started = Instant::now();
    let recomputed = recompute_community_output_with_quorum(node, item)
        .await
        .map_err(|_| RecomputeStatus::Unavailable)?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok((recomputed, elapsed_ms))
}

/// Settlement for the verified leg of caller demand. A reward is requested
/// only when validators recomputed and confirmed this exact output, because
/// every approval asserts that validator's own recomputation.
async fn twin_settlement(
    node: &NodeState,
    leg: &LegSnapshot,
    validators_confirmed: bool,
) -> Option<Value> {
    let result = leg.result.as_ref()?;
    let Some(attestation) = leg.attestation.as_ref() else {
        return Some(unsubmitted_community_reward_settlement(
            "missing_worker_attestation",
            &result.job_id,
            &result.worker_id,
            "worker did not include signed_attestation_hex",
        ));
    };
    let mut settlement = if !community_rewards_v1_protocol_active(node) {
        unsubmitted_community_reward_settlement(
            "reward_protocol_not_activated",
            &result.job_id,
            &result.worker_id,
            "result verified, but reward issuance requires both the genesis-committed activation height and the local issuance switch",
        )
    } else if !validators_confirmed {
        unsubmitted_community_reward_settlement(
            "twin_verified_without_reward_recomputation",
            &result.job_id,
            &result.worker_id,
            "verified by twin execution; a reward needs this coordinator's own validator recomputation, which runs only while reward issuance capacity remains",
        )
    } else {
        match submit_verified_community_reward(
            node,
            &leg.item,
            leg.assignment_epoch,
            leg.job_nonce,
            result,
            attestation,
        )
        .await
        {
            Ok(settlement) => settlement,
            Err(reason) => {
                let mut settlement = unsubmitted_community_reward_settlement(
                    "reward_approval_quorum_unavailable",
                    &result.job_id,
                    &result.worker_id,
                    reason,
                );
                settlement["required_validator_approvals"] =
                    Value::from(arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED);
                settlement
            }
        }
    };
    settlement["worker_attestation_hash"] =
        Value::String(format!("0x{}", attestation.hash.to_hex()));
    Some(settlement)
}

fn sign_receipt(node: &NodeState, commitment: &Hash256) -> Option<twin::ReceiptSignature> {
    let key = node.validator_keypair.as_ref()?;
    if key.address() != node.validator_address {
        return None;
    }
    match key.sign(commitment).ok()? {
        arc_crypto::Signature::Ed25519 {
            public_key,
            signature,
        } => Some(twin::ReceiptSignature {
            scheme: "ed25519",
            signer: format!("0x{}", node.validator_address.to_hex()),
            public_key: format!("0x{}", hex::encode(public_key)),
            signature: format!("0x{}", hex::encode(signature)),
        }),
        _ => None,
    }
}

async fn resolve_twin_group(node: &NodeState, group_id: &str) {
    let Some(mut input) = node.community_twin.begin_resolution(group_id) else {
        return;
    };
    if let Some(results) = node.community_work_results.as_ref() {
        for job_id in &input.cancelled_jobs {
            results.remove(job_id);
        }
    }
    let outputs: Vec<Option<LegOutput>> = input.legs.iter().map(leg_output).collect();
    let reference_output = input
        .reference
        .as_ref()
        .map(|reference| reference.output.clone());
    let (comparison, mismatch_fields) = twin::compare_group(&outputs, reference_output.as_ref());
    let reward_leg = reward_candidate_leg(node, &input, &outputs);
    let any_output = outputs.iter().any(Option::is_some);
    let spot_check = input.spot_check_selected
        && comparison.agrees()
        && reward_leg.is_none()
        && node.community_twin.take_spot_check_token(now_unix_ms());
    let recompute_reason = twin::recompute_reason(
        comparison,
        input.source,
        any_output,
        reward_leg.is_some(),
        spot_check,
    );

    let mut canonical: Option<CommunityCanonicalRecompute> = None;
    let mut recompute_ms = None;
    let mut recompute_error = None;
    if let Some(reason) = recompute_reason {
        let item_index = reward_leg
            .or_else(|| outputs.iter().position(Option::is_some))
            .unwrap_or(0);
        if let Some(leg) = input.legs.get(item_index) {
            match run_twin_recompute(node, &leg.item, reason).await {
                Ok((recomputed, elapsed_ms)) => {
                    recompute_ms = Some(elapsed_ms);
                    canonical = Some(recomputed);
                }
                Err(status) => recompute_error = Some(status),
            }
        } else {
            recompute_error = Some(RecomputeStatus::Unavailable);
        }
    }
    let canonical_output = canonical.as_ref().map(canonical_leg_output);
    let recompute_outcome = match (&canonical_output, recompute_error) {
        (Some(output), _) => Some(Ok(output)),
        (None, Some(status)) => Some(Err(status)),
        (None, None) => None,
    };
    let mut classification = twin::classify(
        &outputs,
        comparison,
        reference_output.as_ref(),
        recompute_outcome,
    );
    // The reward recomputation ran because `reward_leg` qualified (certificate
    // and issuance capacity). When validators found that leg valid, serve and
    // settle it, rather than whichever valid leg comes first, which may lack a
    // certificate or issuance capacity. Valid legs carry identical outputs.
    if let Some(index) = reward_leg
        && classification.leg_valid.get(index).copied().flatten() == Some(true)
    {
        classification.chosen_leg = Some(index);
    }

    let settlement = match classification.chosen_leg {
        Some(index) if input.source.reward_eligible() => match input.legs.get(index) {
            Some(leg) => twin_settlement(node, leg, canonical.is_some()).await,
            None => None,
        },
        _ => None,
    };

    // Worker health counters, exactly as for single jobs: a verified leg is
    // a success, a leg validators proved wrong or the worker reported as
    // failed is a failure, and unverifiable work changes nothing.
    let mut leg_outcomes = Vec::new();
    for (index, leg) in input.legs.iter().enumerate() {
        let Some(worker_id) = leg.worker.as_deref() else {
            continue;
        };
        let valid = classification.leg_valid.get(index).copied().flatten();
        let invalid_submission = leg.status == LegStatus::Submitted
            && leg.result.as_ref().is_some_and(|result| result.success)
            && outputs.get(index).is_some_and(Option::is_none);
        let counted = if valid.is_some() {
            valid
        } else if leg.status == LegStatus::Failed || invalid_submission {
            Some(false)
        } else {
            None
        };
        if let Some(mut entry) = node.community_workers.get_mut(worker_id) {
            let (worker, _) = entry.value_mut();
            match (counted, leg.result.as_ref()) {
                (Some(true), Some(result)) => {
                    worker.work_completed += 1;
                    worker.success_count += 1;
                    worker.sum_total_ms_success =
                        worker.sum_total_ms_success.saturating_add(result.total_ms);
                    worker.last_total_ms = result.total_ms;
                }
                (Some(false), _) => worker.failure_count += 1,
                _ => {}
            }
        }
        if counted == Some(true)
            && let Some(result) = leg.result.as_ref()
        {
            record_latency(
                &node.latency_stats,
                &format!("worker:{worker_id}"),
                result.total_ms,
            );
        }
        if leg.status.is_terminal() || leg.status == LegStatus::Abandoned {
            leg_outcomes.push((worker_id.to_string(), counted));
        }
    }

    let now = now_unix_ms();
    let chosen_output = classification
        .chosen_leg
        .and_then(|index| outputs.get(index).cloned().flatten());
    let new_reference = match (input.source, input.public_prompt, &chosen_output) {
        (DemandSource::PumpDemo, Some((prompt_index, _)), Some(output))
            if classification.verdict == Verdict::Verified =>
        {
            Some(twin::ReplayReference {
                prompt_index,
                input: input
                    .legs
                    .first()
                    .map(|leg| leg.item.input.clone())
                    .unwrap_or_default(),
                max_tokens: input.max_tokens,
                model_id: input.model_id,
                output: canonical_output.clone().unwrap_or_else(|| output.clone()),
                source_group_id: input.group_id.clone(),
                workers: input
                    .legs
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| {
                        classification.leg_valid.get(*index).copied().flatten() == Some(true)
                    })
                    .filter_map(|(_, leg)| leg.worker.clone())
                    .collect(),
                recorded_at_unix_ms: now,
            })
        }
        _ => None,
    };
    let contradicted_reference = match (&input.reference, classification.reference_valid) {
        (Some(reference), Some(false)) => {
            Some((reference.prompt_index, reference.source_group_id.clone()))
        }
        _ => None,
    };

    let platforms: Vec<Option<String>> = input
        .legs
        .iter()
        .map(|leg| {
            leg.worker.as_deref().and_then(|worker| {
                node.community_workers
                    .get(worker)
                    .map(|entry| entry.value().0.platform.clone())
            })
        })
        .collect();
    let legs: Vec<twin::TwinLegReceipt> = input
        .legs
        .iter()
        .enumerate()
        .map(|(index, leg)| {
            let output = outputs.get(index).cloned().flatten();
            twin::TwinLegReceipt {
                leg: index,
                job_id: leg.item.job_id.clone(),
                worker_id: leg.worker.clone(),
                region: leg.region.clone(),
                platform: platforms.get(index).cloned().flatten(),
                status: leg.status,
                output_hash: output
                    .as_ref()
                    .map(|output| format!("0x{}", output.output_hash.to_hex())),
                tokens_generated: output.as_ref().map(|output| output.tokens_generated),
                ms_per_token: output
                    .as_ref()
                    .and(leg.result.as_ref())
                    .map(|result| result.ms_per_token),
                worker_attestation_hash: leg
                    .attestation
                    .as_ref()
                    .map(|attestation| format!("0x{}", attestation.hash.to_hex())),
                valid: classification.leg_valid.get(index).copied().flatten(),
            }
        })
        .collect();
    let workers: Vec<&str> = input
        .legs
        .iter()
        .filter_map(|leg| leg.worker.as_deref())
        .collect();
    let distinct_worker_keys = match &input.reference {
        Some(reference) => workers.iter().all(|worker| {
            !reference
                .workers
                .iter()
                .any(|other| other.as_str() == *worker)
        }),
        None => workers.len() == 2 && workers[0] != workers[1],
    };
    let (different_regions, different_platforms) = match (legs.first(), legs.get(1)) {
        (Some(left), Some(right)) => (
            match (&left.region, &right.region) {
                (Some(a), Some(b)) => Some(a.locality() != b.locality()),
                _ => None,
            },
            match (&left.platform, &right.platform) {
                (Some(a), Some(b)) => Some(a != b),
                _ => None,
            },
        ),
        _ => (None, None),
    };
    let verification = match (&canonical, classification.chosen_leg) {
        (Some(canonical), Some(_)) => Some(CommunityVerificationSummary::from(
            &CommunityResultVerification {
                output_hash: canonical.output_hash,
                tokens_generated: canonical.generated.len(),
                range_count: canonical.range_count,
                range_position_quorum_count: canonical.range_position_quorum_count,
            },
        )),
        _ => None,
    };
    let chosen = classification
        .chosen_leg
        .and_then(|index| input.legs.get(index))
        .and_then(|leg| leg.result.clone());
    let verified_tokens = chosen.as_ref().map(|result| result.tokens_generated);
    let compute_tokens: u64 = outputs
        .iter()
        .flatten()
        .map(|output| output.tokens_generated)
        .sum();

    let mut receipt = twin::TwinReceipt {
        schema: twin::TWIN_RECEIPT_SCHEMA,
        group_id: input.group_id.clone(),
        coordinator: format!("0x{}", node.validator_address.to_hex()),
        source: input.source,
        public_prompt: input.public_prompt.map(|(_, prompt)| prompt),
        model_id: format!("0x{}", input.model_id.to_hex()),
        execution_profile: arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE
            .to_string(),
        input_hash: format!("0x{}", input.input_hash.to_hex()),
        max_tokens: input.max_tokens,
        created_at_unix_ms: input.created_at_unix_ms,
        resolved_at_unix_ms: now,
        legs,
        comparison: twin::ComparisonReceipt {
            basis: twin::COMPARISON_BASIS_FINAL_OUTPUT,
            checkpoint_interval_tokens: twin::CHECKPOINT_INTERVAL_TOKENS,
            result: comparison,
            mismatch_fields,
            reference: input
                .reference
                .as_ref()
                .map(|reference| twin::ReferenceReceipt {
                    group_id: reference.source_group_id.clone(),
                    output_hash: format!("0x{}", reference.output.output_hash.to_hex()),
                    valid: classification.reference_valid,
                }),
        },
        independence: twin::IndependenceReceipt {
            distinct_worker_keys,
            distinct_operators: "unknown",
            distinct_network_groups: "unknown",
            different_regions,
            different_platforms,
        },
        validator_recompute: twin::RecomputeReceipt {
            reason: recompute_reason,
            status: classification.recompute_status,
            method: canonical.as_ref().map(|_| VALIDATOR_RECOMPUTE_METHOD),
            output_hash: canonical
                .as_ref()
                .map(|canonical| format!("0x{}", canonical.output_hash.to_hex())),
            duration_ms: recompute_ms,
        },
        verdict: classification.verdict,
        verified_by: classification.verified_by.clone(),
        settlement,
        disclosure: twin::CENTRALIZATION_DISCLOSURE,
        commitment: String::new(),
        coordinator_signature: None,
    };
    let commitment = receipt.compute_commitment();
    receipt.commitment = format!("0x{}", commitment.to_hex());
    receipt.coordinator_signature = sign_receipt(node, &commitment);

    let outcome_tx = input.outcome_tx.take();
    node.community_twin.finish_resolution(
        group_id,
        receipt.clone(),
        ResolutionAccounting {
            comparison,
            recompute_reason,
            recompute_status: classification.recompute_status,
            recompute_ms,
            verdict: classification.verdict,
            verified_tokens,
            compute_tokens,
            leg_outcomes,
            new_reference,
            contradicted_reference,
        },
        now,
    );
    tracing::info!(
        group_id,
        source = input.source.as_str(),
        comparison = comparison.as_str(),
        verdict = classification.verdict.as_str(),
        recompute = classification.recompute_status.as_str(),
        "community twin group resolved"
    );
    if let Some(outcome_tx) = outcome_tx {
        let _ = outcome_tx.send(TwinGroupOutcome {
            receipt,
            chosen,
            verification,
        });
    }
}

// ─── Demand ─────────────────────────────────────────────────────────────────

/// Process-local randomness from the OS CSPRNG through UUIDv4, the same
/// source the signed community request nonces use. Six of the 128 bits are
/// fixed by the UUID format; folding both halves keeps the result mixed.
fn random_u64() -> u64 {
    let value = uuid::Uuid::new_v4().as_u128();
    (value as u64) ^ ((value >> 64) as u64)
}

/// 244 random bits for the per-process spot-check key.
fn random_secret() -> [u8; 32] {
    let mut secret = [0u8; 32];
    secret[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    secret[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    secret
}

fn jittered_interval(base_secs: u64) -> Duration {
    let base_ms = base_secs.saturating_mul(1_000);
    let jitter = base_ms / 5;
    let offset = random_u64() % jitter.saturating_mul(2).saturating_add(1);
    Duration::from_millis(base_ms.saturating_sub(jitter).saturating_add(offset))
}

/// Idle eligible workers currently long-polling this coordinator.
fn idle_eligible_workers(node: &NodeState) -> Vec<String> {
    let idle: Vec<String> = node
        .community_active_jobs
        .iter()
        .filter(|entry| entry.value().is_empty())
        .map(|entry| entry.key().clone())
        .collect();
    idle.into_iter()
        .filter(|worker_id| exact_live_inference_worker(node, worker_id))
        .collect()
}

async fn run_demand_tick(node: &NodeState) -> Result<(), &'static str> {
    let state = &node.community_twin;
    // Generated demand never competes with callers: it starts only while
    // every public inference slot is free and holds at most one of them.
    if node.public_inference_permits.available_permits() < PUBLIC_INFERENCE_CONCURRENCY {
        return Err("caller inference is in flight");
    }
    let Some(model) = node.inference_model.as_ref() else {
        return Err("coordinator has no tokenizer");
    };
    let Some(model_id) = node.model_artifact_id else {
        return Err("coordinator has no exact model artifact identity");
    };
    if state.pump_running.swap(true, Ordering::AcqRel) {
        return Err("the previous demand job is still running");
    }
    let _running = PumpRunningGuard(&state.pump_running);
    let idle = idle_eligible_workers(node);
    let planned = state.plan_tick(&idle, &model_id, now_unix_ms())?;
    if state.config.demand_dry_run {
        let (kind, prompt_index) = match &planned {
            PlannedDemand::Twin { prompt_index, .. } => ("twin_demo", *prompt_index),
            PlannedDemand::Replay(reference) => ("replay", reference.prompt_index),
        };
        state.inner.lock().counters.demand_dry_run_ticks += 1;
        tracing::info!(
            kind,
            prompt_index,
            idle_workers = idle.len(),
            "community demand pump dry run: would dispatch this job"
        );
        return Ok(());
    }
    let model_id_hint = Some(format!("0x{}", model_id.to_hex()));
    let request = match planned {
        PlannedDemand::Twin {
            prompt_index,
            prompt,
        } => TwinDispatchRequest {
            input: model.apply_chat_template(prompt),
            max_tokens: twin::PUMP_MAX_TOKENS,
            model_id_hint,
            source: DemandSource::PumpDemo,
            public_prompt: Some((prompt_index, prompt)),
            reference: None,
        },
        PlannedDemand::Replay(reference) => TwinDispatchRequest {
            input: reference.input.clone(),
            max_tokens: reference.max_tokens,
            model_id_hint,
            source: DemandSource::PumpReplay,
            public_prompt: twin::PUBLIC_DEMO_PROMPTS
                .get(reference.prompt_index)
                .map(|prompt| (reference.prompt_index, *prompt)),
            reference: Some(*reference),
        },
    };
    dispatch_twin(node, request)
        .await
        .map(|_| ())
        .map_err(|_| "the demand job did not complete")
}

#[cfg(test)]
mod tests {
    use super::super::tests::{canonical_profile, fake_node_with_workers, test_model_id, worker};
    use super::*;

    fn twin_node(worker_ids: &[&str], config: TwinConfig) -> NodeState {
        let now = std::time::Instant::now();
        let mut node = fake_node_with_workers(
            worker_ids
                .iter()
                .map(|id| (worker(id, &["inference"]), now))
                .collect(),
        );
        node.community_twin = Arc::new(CommunityTwinState::new(config));
        node
    }

    fn enabled() -> TwinConfig {
        TwinConfig {
            twin_execution: true,
            demand_pump: false,
            demand_dry_run: false,
            spot_check_per_mille: 0,
            demand_interval_secs: twin::DEFAULT_DEMAND_INTERVAL_SECS,
        }
    }

    fn spawn_dispatch(
        node: &NodeState,
        source: DemandSource,
    ) -> tokio::task::JoinHandle<Result<TwinGroupOutcome, CommunityDispatchError>> {
        let node = node.clone();
        tokio::spawn(async move {
            dispatch_twin(
                &node,
                TwinDispatchRequest {
                    input: "x x".to_string(),
                    max_tokens: 4,
                    model_id_hint: Some(test_model_id()),
                    source,
                    public_prompt: Some((0, twin::PUBLIC_DEMO_PROMPTS[0])),
                    reference: None,
                },
            )
            .await
        })
    }

    async fn wait_for_pending(node: &NodeState, count: usize) {
        for _ in 0..200 {
            if node.community_work_results.as_ref().unwrap().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("twin legs were not enqueued");
    }

    async fn claim(node: &NodeState, worker_id: &str) -> Value {
        let Json(response) = community_claim_work(
            AxumState(node.clone()),
            Json(ClaimWorkRequest {
                worker_id: worker_id.to_string(),
                capabilities: vec!["inference".to_string()],
                model_id: test_model_id(),
                execution_profile: canonical_profile(),
            }),
        )
        .await
        .expect("claim is accepted");
        response
    }

    fn result(job_id: &str, worker_id: &str, output: &str) -> WorkResult {
        WorkResult {
            job_id: job_id.to_string(),
            worker_id: worker_id.to_string(),
            success: true,
            declined: false,
            output: output.to_string(),
            output_hash: format!("0x{}", arc_crypto::hash_bytes(output.as_bytes()).to_hex()),
            tokens_generated: 2,
            total_ms: 40,
            ms_per_token: 20,
            engine: canonical_profile(),
            error: None,
            signed_attestation_hex: None,
        }
    }

    /// A failed (`declined = false`) or declined (`declined = true`) result.
    fn failed(job_id: &str, worker_id: &str, declined: bool) -> WorkResult {
        WorkResult {
            job_id: job_id.to_string(),
            worker_id: worker_id.to_string(),
            success: false,
            declined,
            output: String::new(),
            output_hash: String::new(),
            tokens_generated: 0,
            total_ms: 0,
            ms_per_token: 0,
            engine: String::new(),
            error: (!declined).then(|| "local inference task failed".to_string()),
            signed_attestation_hex: None,
        }
    }

    async fn submit(node: &NodeState, result: WorkResult) -> Value {
        let Json(response) = community_submit_work(AxumState(node.clone()), Json(result))
            .await
            .expect("twin leg submit is accepted");
        response
    }

    fn job_of(claimed: &Value) -> String {
        assert_eq!(claimed["status"], "work", "{claimed}");
        claimed["job_id"].as_str().unwrap().to_string()
    }

    async fn outcome_of(
        dispatcher: tokio::task::JoinHandle<Result<TwinGroupOutcome, CommunityDispatchError>>,
    ) -> TwinGroupOutcome {
        match tokio::time::timeout(Duration::from_secs(10), dispatcher)
            .await
            .expect("twin group resolves")
            .expect("dispatcher task completes")
        {
            Ok(outcome) => outcome,
            Err(error) => panic!("twin dispatch failed: {error}"),
        }
    }

    #[tokio::test]
    async fn twin_match_verifies_without_validator_recompute_and_refuses_self_twinning() {
        let node = twin_node(&["w1", "w2"], enabled());
        assert!(twin_dispatch_ready(&node));
        let dispatcher = spawn_dispatch(&node, DemandSource::PublicRequest);
        wait_for_pending(&node, 2).await;

        let first = job_of(&claim(&node, "w1").await);
        let awaiting = submit(&node, result(&first, "w1", "four")).await;
        assert_eq!(awaiting["twin"]["status"], "awaiting_sibling");

        // The worker that ran the first leg may not take its twin.
        let refused = claim(&node, "w1").await;
        assert_eq!(refused["status"], "no_work");
        assert_eq!(refused["reason"], twin::REASON_SAME_WORKER);

        let second = job_of(&claim(&node, "w2").await);
        assert_ne!(first, second);
        let resolving = submit(&node, result(&second, "w2", "four")).await;
        assert_eq!(resolving["twin"]["status"], "resolving");

        let outcome = outcome_of(dispatcher).await;
        let receipt = &outcome.receipt;
        assert_eq!(receipt.verdict, Verdict::Verified);
        assert_eq!(receipt.verified_by, vec![twin::VERIFIED_BY_TWIN]);
        assert_eq!(receipt.comparison.result, twin::ComparisonResult::Match);
        assert_eq!(
            receipt.validator_recompute.status,
            RecomputeStatus::NotSelected
        );
        assert_eq!(receipt.legs.len(), 2);
        assert_eq!(receipt.legs[0].worker_id.as_deref(), Some("w1"));
        assert_eq!(receipt.legs[1].worker_id.as_deref(), Some("w2"));
        assert!(receipt.independence.distinct_worker_keys);
        assert_eq!(
            receipt.commitment,
            format!("0x{}", receipt.compute_commitment().to_hex())
        );
        assert!(receipt.coordinator_signature.is_none());
        assert_eq!(outcome.chosen.as_ref().unwrap().output, "four");
        assert!(outcome.verification.is_none());

        let stats = node.community_twin.inner.lock().counters.clone();
        assert_eq!(stats.groups_matched, 1);
        assert_eq!(stats.recomputes_avoided, 1);
        assert_eq!(stats.pairing_refused, 1);
        assert_eq!(stats.verified_jobs, 1);
        assert_eq!(stats.verified_tokens, 2);
        assert_eq!(stats.worker_compute_tokens, 4);
        for id in ["w1", "w2"] {
            let entry = node.community_workers.get(id).unwrap();
            assert_eq!(entry.value().0.success_count, 1, "{id}");
        }
        assert!(node.community_twin.receipt(&second).is_some());
        assert!(node.community_work_results.as_ref().unwrap().is_empty());
        assert!(node.community_active_jobs.is_empty());

        let response = twin_inference_response(&node, "x x", outcome, 2, 5, true);
        assert_eq!(response["success"], true);
        assert_eq!(response["inference"]["output"], "four");
        assert_eq!(response["verification"]["method"], "twin_execution_v0");
        assert_eq!(response["twin"]["schema"], twin::TWIN_RECEIPT_SCHEMA);
        assert_eq!(response["public_demo"]["public"], true);
    }

    #[tokio::test]
    async fn twin_mismatch_without_validators_returns_no_output_and_no_penalty() {
        let node = twin_node(&["w1", "w2"], enabled());
        let dispatcher = spawn_dispatch(&node, DemandSource::PumpDemo);
        wait_for_pending(&node, 2).await;
        let first = job_of(&claim(&node, "w1").await);
        let second = job_of(&claim(&node, "w2").await);
        submit(&node, result(&first, "w1", "four")).await;
        submit(&node, result(&second, "w2", "five")).await;

        let outcome = outcome_of(dispatcher).await;
        assert_eq!(outcome.receipt.verdict, Verdict::Unverified);
        assert_eq!(
            outcome.receipt.comparison.result,
            twin::ComparisonResult::Mismatch
        );
        assert_eq!(
            outcome.receipt.validator_recompute.reason,
            Some(RecomputeReason::Mismatch)
        );
        assert_eq!(
            outcome.receipt.validator_recompute.status,
            RecomputeStatus::Unavailable
        );
        assert!(outcome.chosen.is_none());
        let counters = node.community_twin.inner.lock().counters.clone();
        assert_eq!(counters.groups_mismatched, 1);
        assert_eq!(counters.recompute_unavailable, 1);
        assert_eq!(counters.verified_jobs, 0);
        for id in ["w1", "w2"] {
            let entry = node.community_workers.get(id).unwrap();
            assert_eq!(entry.value().0.success_count, 0, "{id}");
            assert_eq!(entry.value().0.failure_count, 0, "{id}");
        }
        // Generated demand never records a reference it could not verify.
        assert!(node.community_twin.inner.lock().references.is_empty());
        let response = twin_inference_response(&node, "x x", outcome, 2, 5, false);
        assert_eq!(response["success"], false);
        assert!(response.get("inference").is_none());
        assert!(response.get("public_demo").is_none());
    }

    #[tokio::test]
    async fn incomplete_generated_demand_spends_no_validator_capacity() {
        let node = twin_node(&["w1", "w2"], enabled());
        let dispatcher = spawn_dispatch(&node, DemandSource::PumpDemo);
        wait_for_pending(&node, 2).await;
        let first = job_of(&claim(&node, "w1").await);
        let second = job_of(&claim(&node, "w2").await);
        submit(&node, result(&first, "w1", "four")).await;
        submit(&node, failed(&second, "w2", false)).await;

        let outcome = outcome_of(dispatcher).await;
        assert_eq!(
            outcome.receipt.comparison.result,
            twin::ComparisonResult::Incomplete
        );
        assert_eq!(outcome.receipt.validator_recompute.reason, None);
        assert_eq!(outcome.receipt.verdict, Verdict::Unverified);
        assert_eq!(outcome.receipt.legs[1].status, LegStatus::Failed);
        // A worker-reported failure counts against that worker, as today.
        assert_eq!(
            node.community_workers
                .get("w2")
                .unwrap()
                .value()
                .0
                .failure_count,
            1
        );
        let counters = node.community_twin.inner.lock().counters.clone();
        assert_eq!(counters.groups_incomplete, 1);
        assert_eq!(counters.recompute_fallback, 0);
        assert_eq!(counters.recompute_unavailable, 0);
    }

    #[tokio::test]
    async fn declined_twin_leg_is_requeued_for_another_independent_worker() {
        let node = twin_node(&["w1", "w2", "w3"], enabled());
        let dispatcher = spawn_dispatch(&node, DemandSource::PumpDemo);
        wait_for_pending(&node, 2).await;
        let first = job_of(&claim(&node, "w1").await);
        let second = job_of(&claim(&node, "w2").await);
        // w2 won a concurrent job elsewhere and declines without computing.
        let requeued = submit(&node, failed(&second, "w2", true)).await;
        assert_eq!(requeued["twin"]["status"], "requeued");
        let again = job_of(&claim(&node, "w3").await);
        assert_eq!(again, second);
        submit(&node, result(&first, "w1", "green")).await;
        submit(&node, result(&second, "w3", "green")).await;

        let outcome = outcome_of(dispatcher).await;
        assert_eq!(outcome.receipt.verdict, Verdict::Verified);
        assert_eq!(outcome.receipt.legs[1].worker_id.as_deref(), Some("w3"));
        let counters = node.community_twin.inner.lock().counters.clone();
        assert_eq!(counters.legs_requeued, 1);
        assert_eq!(counters.groups_matched, 1);
        // Declining is not a failure.
        assert_eq!(
            node.community_workers
                .get("w2")
                .unwrap()
                .value()
                .0
                .failure_count,
            0
        );
    }

    #[tokio::test]
    async fn verified_demo_prompts_become_replay_references() {
        let node = twin_node(&["w1", "w2"], enabled());
        let dispatcher = spawn_dispatch(&node, DemandSource::PumpDemo);
        wait_for_pending(&node, 2).await;
        let first = job_of(&claim(&node, "w1").await);
        let second = job_of(&claim(&node, "w2").await);
        submit(&node, result(&first, "w1", "red")).await;
        submit(&node, result(&second, "w2", "red")).await;
        let outcome = outcome_of(dispatcher).await;
        assert_eq!(outcome.receipt.verdict, Verdict::Verified);
        assert_eq!(
            outcome.receipt.public_prompt,
            Some(twin::PUBLIC_DEMO_PROMPTS[0])
        );
        let inner = node.community_twin.inner.lock();
        assert_eq!(inner.references.len(), 1);
        let model = parse_hash256_hex(&test_model_id(), "model").unwrap();
        let reference = inner.references.pick(0, &model).unwrap();
        assert_eq!(reference.workers, vec!["w1".to_string(), "w2".to_string()]);
        assert_eq!(reference.input, "x x");
        assert_eq!(inner.counters.demand_pump_demo, 1);
    }

    #[tokio::test]
    async fn late_leg_after_resolution_is_recorded_not_reverified() {
        let node = twin_node(&["w1", "w2"], enabled());
        let dispatcher = spawn_dispatch(&node, DemandSource::PublicRequest);
        wait_for_pending(&node, 2).await;
        let first = job_of(&claim(&node, "w1").await);
        let second = job_of(&claim(&node, "w2").await);
        submit(&node, result(&first, "w1", "four")).await;

        // Resolve while the sibling is still computing: it is abandoned and
        // the caller job falls back to validator recomputation, which this
        // fixture cannot reach.
        resolve_twin_group(&node, &first).await;
        let outcome = outcome_of(dispatcher).await;
        assert_eq!(outcome.receipt.legs[1].status, LegStatus::Abandoned);
        assert_eq!(
            outcome.receipt.validator_recompute.reason,
            Some(RecomputeReason::Fallback)
        );
        assert_eq!(outcome.receipt.verdict, Verdict::Unverified);

        let late = submit(&node, result(&second, "w2", "four")).await;
        assert_eq!(late["twin"]["status"], "late_after_resolution");
        let counters = node.community_twin.inner.lock().counters.clone();
        assert_eq!(counters.late_legs, 1);
        assert_eq!(counters.recompute_fallback, 0);
        assert_eq!(counters.recompute_unavailable, 1);
        assert!(node.community_active_jobs.is_empty());
    }

    #[tokio::test]
    async fn twin_dispatch_requires_the_switch_and_two_live_workers() {
        let off = twin_node(&["w1", "w2"], TwinConfig::default());
        assert!(!twin_dispatch_ready(&off));
        let lonely = twin_node(&["w1"], enabled());
        assert!(!twin_dispatch_ready(&lonely));
        assert!(scoreboard_summary(&off).is_none());
        assert!(worker_rows(&off).is_empty());
        assert!(scoreboard_summary(&lonely).is_some());
    }

    #[tokio::test]
    async fn region_reports_are_authenticated_and_only_the_label_is_published() {
        let node = twin_node(&[], enabled());
        let keypair = arc_crypto::KeyPair::generate_ed25519();
        let worker_id = format!("0x{}", keypair.address().to_hex());
        node.community_workers.insert(
            worker_id.clone(),
            (
                worker(&worker_id, &["inference"]),
                std::time::Instant::now(),
            ),
        );
        let report = twin::CommunityRegionReport {
            worker_id: worker_id.clone(),
            samples: vec![twin::CommunityRttSample {
                validator: "0x5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21"
                    .to_string(),
                rtt_ms: 21,
            }],
        };
        let signed = super::super::sign_community_request(
            twin::COMMUNITY_REGION_PATH,
            report.clone(),
            &keypair,
            Hash256::ZERO,
            None,
        )
        .unwrap();
        let Json(response) = community_region_signed(AxumState(node.clone()), Json(signed))
            .await
            .expect("signed region report is accepted");
        assert_eq!(response["region"]["region"], "eu-west");
        assert_eq!(response["region"]["class"], "region");
        assert!(response["region"].get("nearest_rtt_ms").is_none());
        let rows = worker_rows(&node);
        assert_eq!(
            rows.get(&worker_id)
                .and_then(|row| row.region.as_ref())
                .map(|tag| tag.continent.as_str()),
            Some("europe")
        );

        // An unregistered signer is refused before anything is stored.
        let stranger = arc_crypto::KeyPair::generate_ed25519();
        let mut foreign = report;
        foreign.worker_id = format!("0x{}", stranger.address().to_hex());
        let signed = super::super::sign_community_request(
            twin::COMMUNITY_REGION_PATH,
            foreign,
            &stranger,
            Hash256::ZERO,
            None,
        )
        .unwrap();
        let error = community_region_signed(AxumState(node.clone()), Json(signed))
            .await
            .expect_err("unregistered workers cannot tag a region");
        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(node.community_twin.inner.lock().regions.len(), 1);
    }

    #[tokio::test]
    async fn public_demo_admission_screens_prompts_and_rate_limits() {
        let node = twin_node(&["w1", "w2"], enabled());
        let rejected = admit_public_demo(&node, "email me at someone@example.com")
            .expect_err("private data is refused");
        assert_eq!(rejected.0, StatusCode::BAD_REQUEST);
        for _ in 0..PUBLIC_DEMO_BURST {
            admit_public_demo(&node, "Name three primary colors.").expect("within the burst");
        }
        let limited =
            admit_public_demo(&node, "Name three primary colors.").expect_err("burst exhausted");
        assert_eq!(limited.0, StatusCode::TOO_MANY_REQUESTS);
        let counters = node.community_twin.inner.lock().counters.clone();
        assert_eq!(counters.public_demo_rejected, 1);
        assert_eq!(counters.public_demo_rate_limited, 1);
    }

    #[tokio::test]
    async fn stats_and_receipt_endpoints_expose_the_match_rate() {
        let node = twin_node(&["w1", "w2"], enabled());
        let dispatcher = spawn_dispatch(&node, DemandSource::PumpDemo);
        wait_for_pending(&node, 2).await;
        let first = job_of(&claim(&node, "w1").await);
        let second = job_of(&claim(&node, "w2").await);
        submit(&node, result(&first, "w1", "blue")).await;
        submit(&node, result(&second, "w2", "blue")).await;
        outcome_of(dispatcher).await;

        let Json(stats) = community_twin_stats(AxumState(node.clone())).await;
        assert_eq!(stats["schema"], twin::TWIN_STATS_SCHEMA);
        assert!(
            stats["twin_match_rate"]
                .as_f64()
                .is_some_and(|rate| (rate - 1.0).abs() < 1e-9)
        );
        assert_eq!(stats["throughput_last_hour"]["verified_jobs"], 1);
        assert_eq!(stats["validator_recompute"]["avoided"], 1);
        assert_eq!(stats["config"]["twin_execution"], true);

        let Json(receipt) =
            community_twin_receipt(AxumState(node.clone()), axum::extract::Path(second))
                .await
                .expect("receipt is addressable by either leg");
        assert_eq!(receipt["group_id"], first);
        let Json(recent) =
            community_twin_receipts(AxumState(node.clone()), Query(HashMap::new())).await;
        assert_eq!(recent["count"], 1);
        let missing = community_twin_receipt(
            AxumState(node.clone()),
            axum::extract::Path("00".repeat(32)),
        )
        .await
        .expect_err("unknown receipts are 404");
        assert_eq!(missing.0, StatusCode::NOT_FOUND);

        let summary = scoreboard_summary(&node).unwrap();
        assert_eq!(summary["groups_matched"], 1);
        assert_eq!(summary["verified_tokens_last_hour"], 2);
    }

    #[tokio::test]
    async fn demand_pump_dry_run_plans_without_dispatching_and_yields_to_callers() {
        let node = twin_node(
            &["w1", "w2"],
            TwinConfig {
                demand_pump: true,
                demand_dry_run: true,
                ..enabled()
            },
        );
        assert_eq!(
            run_demand_tick(&node).await.unwrap_err(),
            "no idle eligible worker is polling this coordinator"
        );
        for id in ["w1", "w2"] {
            node.community_active_jobs
                .insert(id.to_string(), String::new());
        }
        run_demand_tick(&node)
            .await
            .expect("a dry-run tick plans a job");
        assert!(node.community_work_results.as_ref().unwrap().is_empty());
        let counters = node.community_twin.inner.lock().counters.clone();
        assert_eq!(counters.demand_dry_run_ticks, 1);
        assert_eq!(counters.groups_started, 0);
        assert!(!node.community_twin.pump_running.load(Ordering::Acquire));

        let _caller = node
            .public_inference_permits
            .clone()
            .try_acquire_owned()
            .unwrap();
        assert_eq!(
            run_demand_tick(&node).await.unwrap_err(),
            "caller inference is in flight"
        );
    }

    #[test]
    fn jittered_intervals_stay_within_twenty_percent() {
        for _ in 0..64 {
            let interval = jittered_interval(100);
            assert!(interval >= Duration::from_secs(80), "{interval:?}");
            assert!(interval <= Duration::from_secs(120), "{interval:?}");
        }
        assert_eq!(jittered_interval(0), Duration::ZERO);
    }

    #[test]
    fn bounded_maps_never_exceed_their_cap() {
        let mut map: HashMap<String, u64> =
            (0..10).map(|index| (index.to_string(), index)).collect();
        bound_map(&mut map, 4);
        assert_eq!(map.len(), 4);
        bound_map(&mut map, 10);
        assert_eq!(map.len(), 4);
    }
}
