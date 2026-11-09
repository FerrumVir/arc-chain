//! Coordinator-only scheduling. No worker timing or wire fields are trusted here.
use super::*;
use tokio::sync::mpsc::{Receiver, error::TryRecvError};
use tokio::time::Instant as TokioInstant;

const HOLD: Duration = Duration::from_millis(1_500);

pub(super) struct PendingRouting {
    pub enqueued_at: TokioInstant,
    pub prompt_len: usize,
    pub excluded_worker: Option<String>,
}

impl Default for PendingRouting {
    fn default() -> Self {
        Self {
            enqueued_at: TokioInstant::now(),
            prompt_len: 0,
            excluded_worker: None,
        }
    }
}

struct HeldWork {
    item: WorkItem,
    chosen: Option<String>,
    explore: bool,
}

#[derive(Default)]
pub(super) struct RoutingState {
    /// Seconds per position, using the same alpha as the displayed latency EWMA.
    seconds_per_position: HashMap<String, f64>,
    assigned: HashSet<String>,
    /// None invalidates reservations opened before a decline. Only a new
    /// admitted long-poll turns this back on; the active map remains authoritative.
    claims: HashMap<String, Option<Hash256>>,
    held: Vec<HeldWork>,
    decisions: u64,
}

pub(super) fn open_claim(node: &NodeState, worker: &str, model: Hash256) {
    node.community_routing
        .lock()
        .claims
        .insert(worker.into(), Some(model));
}

pub(super) fn assigned(node: &NodeState, worker: &str) {
    node.community_routing.lock().assigned.insert(worker.into());
}

fn record(state: &mut RoutingState, worker: &str, sample: f64) {
    state.assigned.insert(worker.into());
    state
        .seconds_per_position
        .entry(worker.into())
        .and_modify(|old| *old = LATENCY_ALPHA * sample + (1.0 - LATENCY_ALPHA) * *old)
        .or_insert(sample);
}

pub(super) fn record_slow(node: &NodeState, worker: &str, budget_secs: u64, positions: usize) {
    record(
        &mut node.community_routing.lock(),
        worker,
        budget_secs as f64 / positions.max(1) as f64,
    );
}

pub(super) fn record_timeout(
    node: &NodeState,
    results: &dashmap::DashMap<String, PendingCommunityWork>,
    job: &str,
    budget_secs: u64,
    positions: usize,
) {
    let worker = results.get(job).and_then(|pending| {
        pending
            .first_submit_at
            .is_none()
            .then(|| pending.assigned_worker.clone())
            .flatten()
    });
    if let Some(worker) = worker {
        record_slow(node, &worker, budget_secs, positions);
    }
}

pub(super) fn record_accepted(
    node: &NodeState,
    pending: &PendingCommunityWork,
    result: &WorkResult,
    submit_arrived: TokioInstant,
) {
    if result.declined {
        let mut state = node.community_routing.lock();
        state.assigned.insert(result.worker_id.clone());
        state.claims.insert(result.worker_id.clone(), None);
        return;
    }
    if !result.success {
        return;
    }
    let Some(start) = pending.assigned_at else {
        return;
    };
    let Some(model) = node.inference_model.as_ref() else {
        return;
    };
    let Ok(tokens) = u32::try_from(result.tokens_generated) else {
        return;
    };
    let Ok(preflight) = model.preflight_generation(pending.routing.prompt_len, tokens) else {
        return;
    };
    // Retrying a transient verification failure must not charge the worker for
    // the coordinator's previous verification attempt or retry delay.
    let end = pending.first_submit_at.unwrap_or(submit_arrived);
    record(
        &mut node.community_routing.lock(),
        &result.worker_id,
        end.saturating_duration_since(start).as_secs_f64()
            / preflight.required_positions.max(1) as f64,
    );
}

fn waiting(
    state: &RoutingState,
    node: &NodeState,
    model: Option<&str>,
    excluded: Option<&str>,
) -> Vec<String> {
    let model = model.and_then(|m| parse_hash256_hex(m, "model_id").ok());
    // Do not nest registry locks inside active-job shard locks: capacity
    // readers visit these maps in the opposite order.
    let candidates: Vec<String> = node
        .community_active_jobs
        .iter()
        .filter(|slot| slot.value().is_empty())
        .map(|slot| slot.key().clone())
        .collect();
    candidates
        .into_iter()
        .filter(|id| {
            if excluded == Some(id.as_str())
                || state
                    .claims
                    .get(id)
                    .is_some_and(|claimed| *claimed != model)
            {
                return false;
            }
            let Some(registered) = node.community_workers.get(id) else {
                return false;
            };
            let worker = &registered.0;
            model.is_some()
                && worker
                    .model_id
                    .as_deref()
                    .and_then(|m| parse_hash256_hex(m, "model_id").ok())
                    == model
                && worker.capabilities.iter().any(|c| c == "inference")
                && worker.execution_profile.as_deref()
                    == Some(arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE)
        })
        .collect()
}

pub(super) fn has_other_waiter(node: &NodeState, model: &str, declined: &str) -> bool {
    !waiting(
        &node.community_routing.lock(),
        node,
        Some(model),
        Some(declined),
    )
    .is_empty()
}

pub(super) fn retry_fits(elapsed: Duration, budget_secs: u64) -> bool {
    elapsed <= Duration::from_secs(30)
        && elapsed.saturating_add(Duration::from_secs(budget_secs))
            <= Duration::from_secs(MAX_COMMUNITY_DISPATCH_TIMEOUT_SECS)
}

fn choose(state: &RoutingState, workers: &mut [String], explore: bool) -> Option<String> {
    workers.sort_by(|a, b| {
        let rank = |id: &String| match state.seconds_per_position.get(id) {
            Some(speed) => (0, *speed),
            None if !state.assigned.contains(id) => (1, 0.0),
            None => (2, 0.0),
        };
        let a_rank = rank(a);
        let b_rank = rank(b);
        a_rank
            .0
            .cmp(&b_rank.0)
            .then_with(|| a_rank.1.total_cmp(&b_rank.1))
            .then_with(|| a.cmp(b))
    });
    if explore && let Some(new) = workers.iter().find(|id| !state.assigned.contains(*id)) {
        return Some(new.clone());
    }
    workers.first().cloned()
}

/// Called with the queue locked, without any await until an item is either
/// back in shared held state or returned to the synchronous claim/bind path.
pub(super) fn poll(
    node: &NodeState,
    worker: &str,
    queue: &mut Receiver<WorkItem>,
) -> Result<Option<WorkItem>, TryRecvError> {
    let mut state = node.community_routing.lock();
    // Temporarily take the list only inside this synchronous critical section.
    // No cancelled future can leave an item in a handler-local variable.
    let held = std::mem::take(&mut state.held);
    let mut selected = None;
    for mut work in held {
        if selected.is_none() && may_take(&mut state, node, worker, &mut work) {
            selected = Some(work.item);
        } else {
            state.held.push(work);
        }
    }
    if selected.is_some() {
        return Ok(selected);
    }
    match queue.try_recv() {
        Ok(item) => {
            // Targeted probes and twin legs retain their original claim path.
            if item.expected_worker_id.is_some() || twin_dispatch::is_twin_leg(node, &item.job_id) {
                return Ok(Some(item));
            }
            state.decisions = state.decisions.wrapping_add(1);
            let mut work = HeldWork {
                item,
                chosen: None,
                explore: state.decisions.is_multiple_of(4),
            };
            if may_take(&mut state, node, worker, &mut work) {
                Ok(Some(work.item))
            } else {
                state.held.push(work);
                Ok(None)
            }
        }
        Err(TryRecvError::Disconnected) if !state.held.is_empty() => Ok(None),
        Err(error) => Err(error),
    }
}

fn may_take(state: &mut RoutingState, node: &NodeState, worker: &str, work: &mut HeldWork) -> bool {
    let Some(pending) = node
        .community_work_results
        .as_ref()
        .and_then(|m| m.get(&work.item.job_id))
    else {
        return true; // Existing claim path reports job_expired.
    };
    if work.item.expires_at_unix_ms <= now_unix_ms() {
        return true;
    }
    let elapsed = pending.routing.enqueued_at.elapsed();
    let excluded_worker = pending.routing.excluded_worker.clone();
    drop(pending);
    let mut workers = waiting(
        state,
        node,
        work.item.model_id.as_deref(),
        excluded_worker.as_deref(),
    );
    let eligible = workers.iter().any(|id| id == worker);
    if work.chosen.as_ref().is_none_or(|id| !workers.contains(id)) {
        work.chosen = choose(state, &mut workers, work.explore);
        tracing::debug!(job_id = %work.item.job_id, chosen_worker = ?work.chosen,
            others_waiting = ?workers.iter().filter(|id| Some(*id) != work.chosen.as_ref()).collect::<Vec<_>>(),
            hold_ms = HOLD.saturating_sub(elapsed).as_millis(), "community routing decision");
    }
    let take = eligible && (elapsed >= HOLD || work.chosen.as_deref() == Some(worker));
    if take {
        tracing::debug!(job_id = %work.item.job_id, chosen_worker = worker, others_waiting = ?workers,
            hold_ms = elapsed.as_millis(), "community routing claim");
    }
    take
}

#[cfg(test)]
pub(super) fn sample(node: &NodeState, worker: &str) -> Option<f64> {
    node.community_routing
        .lock()
        .seconds_per_position
        .get(worker)
        .copied()
}
