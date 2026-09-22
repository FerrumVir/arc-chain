//! Deterministic, cost-aware placement (S3).
//!
//! Given the per-token projection stages of a model, the coordinator's own
//! rate, and validated workers with MEASURED rates and fresh link
//! measurements, choose who computes which rows - or nobody but the
//! coordinator, when distributing would be slower. The same inputs always
//! give the same placement, whatever order they arrive in, so any validator
//! can recompute it from a certificate (S4).

use crate::Address;
use crate::link::LinkMeasurement;
use serde::{Deserialize, Serialize};

/// One projection a token needs: `rows` outputs of `cols` inputs each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub layer: Option<u32>,
    pub tensor: String,
    pub rows: u64,
    pub cols: u64,
}

/// A worker eligible for placement: its lease validated, its capacity
/// measured by challenge, its link measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub worker: Address,
    pub transport_id: String,
    /// Measured multiply-accumulates per second (never the claim).
    pub macs_per_s: u64,
    pub ram_headroom_bytes: u64,
    pub max_concurrency: u32,
    pub link: LinkMeasurement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub max_workers: usize,
    /// Links measured more than this long ago (same units as
    /// `LinkMeasurement::measured_at`) are not used.
    pub max_link_age: u64,
    pub now: u64,
    pub max_failure_per_mille: u32,
    /// Whether measurements taken under injected conditions may be used.
    pub allow_simulated_links: bool,
    /// Bytes per input element sent and per output element returned.
    pub input_element_bytes: u64,
    pub output_element_bytes: u64,
    /// Bytes a resident row costs per input column (1 for canonical I8).
    pub weight_bytes_per_element: u64,
    /// Whether the coordinator computes a share itself.
    pub include_coordinator: bool,
}

/// Who computes a slice: the coordinator itself, or a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Participant {
    Coordinator,
    Worker(Address),
}

impl Participant {
    /// Canonical order: the coordinator first, then workers by address.
    fn order_key(&self) -> (u8, [u8; 32]) {
        match self {
            Participant::Coordinator => (0, [0; 32]),
            Participant::Worker(address) => (1, address.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slice {
    pub participant: Participant,
    pub row_start: u64,
    pub row_end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagePlan {
    pub stage: usize,
    pub slices: Vec<Slice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    /// Workers used, in address order (the coordinator is implicit).
    pub workers: Vec<Address>,
    pub stages: Vec<StagePlan>,
    /// Predicted per-token time of this placement, and of the coordinator
    /// alone - so the decision to distribute is visible in the record.
    pub predicted_token_us: u64,
    pub coordinator_only_token_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("no stage to place")]
    NoStages,
    #[error("the coordinator is excluded and no worker set can hold every row")]
    Infeasible,
    #[error("coordinator rate must be positive when it computes a share")]
    NoCoordinatorRate,
}

fn usable(c: &Candidate, p: &Policy) -> bool {
    c.macs_per_s > 0
        && c.link.bandwidth_bps > 0
        && c.max_concurrency > 0
        && c.link.failure_per_mille <= p.max_failure_per_mille
        && p.now.saturating_sub(c.link.measured_at) <= p.max_link_age
        && (p.allow_simulated_links || !c.link.simulated)
}

/// Split `rows` in proportion to `weights` (parts per million), exactly:
/// floor shares, then the remainder one row each in participant order.
fn split_rows(rows: u64, weights: &[u64]) -> Vec<u64> {
    let total: u128 = weights.iter().map(|w| *w as u128).sum();
    if total == 0 {
        return vec![0; weights.len()];
    }
    let mut out: Vec<u64> = weights
        .iter()
        .map(|w| ((rows as u128 * *w as u128) / total) as u64)
        .collect();
    let mut assigned: u64 = out.iter().sum();
    let mut i = 0usize;
    while assigned < rows {
        if weights[i % weights.len()] > 0 {
            out[i % weights.len()] += 1;
            assigned += 1;
        }
        i += 1;
    }
    out
}

/// Per-token time of one participant set with the given row counts per stage.
fn predict(
    stages: &[Stage],
    rates: &[u64],
    links: &[Option<&LinkMeasurement>],
    rows_per_stage: &[Vec<u64>],
    p: &Policy,
) -> u64 {
    let mut total: u128 = 0;
    for (si, stage) in stages.iter().enumerate() {
        let mut slowest: u128 = 0;
        for (pi, rows) in rows_per_stage[si].iter().enumerate() {
            if *rows == 0 {
                continue;
            }
            let compute_us =
                (*rows as u128 * stage.cols as u128 * 1_000_000) / rates[pi].max(1) as u128;
            let comm_us = match links[pi] {
                None => 0,
                Some(link) => {
                    let bytes = stage.cols as u128 * p.input_element_bytes as u128
                        + *rows as u128 * p.output_element_bytes as u128;
                    link.rtt_p95_us as u128
                        + (bytes * 1_000_000) / link.bandwidth_bps.max(1) as u128
                }
            };
            slowest = slowest.max(compute_us + comm_us);
        }
        total += slowest;
    }
    total.min(u64::MAX as u128) as u64
}

/// Choose a placement. See the module documentation.
pub fn place(
    stages: &[Stage],
    coordinator_macs_per_s: u64,
    candidates: &[Candidate],
    policy: &Policy,
) -> Result<Placement, PlacementError> {
    if stages.is_empty() {
        return Err(PlacementError::NoStages);
    }
    if policy.include_coordinator && coordinator_macs_per_s == 0 {
        return Err(PlacementError::NoCoordinatorRate);
    }
    // Deterministic candidate order: fastest measured first, address breaks
    // ties. Input order never matters.
    let mut pool: Vec<&Candidate> = candidates.iter().filter(|c| usable(c, policy)).collect();
    pool.sort_by(|a, b| {
        b.macs_per_s
            .cmp(&a.macs_per_s)
            .then(a.worker.0.cmp(&b.worker.0))
    });
    // One entry per validator identity - its fastest offer - however many
    // leases or transports it presented.
    let mut seen = std::collections::BTreeSet::new();
    pool.retain(|c| seen.insert(c.worker.0));

    let total_weight_bytes: u128 = stages
        .iter()
        .map(|s| s.rows as u128 * s.cols as u128 * policy.weight_bytes_per_element as u128)
        .sum();
    let coordinator_only = if policy.include_coordinator {
        let rows: Vec<Vec<u64>> = stages.iter().map(|s| vec![s.rows]).collect();
        predict(stages, &[coordinator_macs_per_s], &[None], &rows, policy)
    } else {
        u64::MAX
    };

    let mut best: Option<(u64, usize, Vec<Vec<u64>>)> = None;
    let max_k = policy.max_workers.min(pool.len());
    for k in 0..=max_k {
        if k == 0 && !policy.include_coordinator {
            continue;
        }
        // Participants: coordinator (if included) then the k fastest workers.
        let chosen = &pool[..k];
        let mut rates: Vec<u64> = Vec::new();
        let mut links: Vec<Option<&LinkMeasurement>> = Vec::new();
        let mut caps: Vec<u128> = Vec::new(); // max share of all rows, in ppm
        if policy.include_coordinator {
            rates.push(coordinator_macs_per_s);
            links.push(None);
            caps.push(1_000_000);
        }
        for c in chosen {
            rates.push(c.macs_per_s);
            links.push(Some(&c.link));
            // No weights to hold: memory caps nothing. Otherwise the share
            // of all rows this machine's headroom can hold, in ppm.
            let cap = (c.ram_headroom_bytes as u128 * 1_000_000)
                .checked_div(total_weight_bytes)
                .map_or(1_000_000, |share| share.min(1_000_000));
            caps.push(cap);
        }
        // Weights proportional to rate, each capped by what fits in memory;
        // excess redistributed to uncapped participants (bounded passes).
        let rate_total: u128 = rates.iter().map(|r| *r as u128).sum();
        let mut weights: Vec<u128> = rates
            .iter()
            .map(|r| (*r as u128 * 1_000_000) / rate_total.max(1))
            .collect();
        for _ in 0..rates.len() {
            let mut excess: u128 = 0;
            let mut free_rate: u128 = 0;
            for i in 0..weights.len() {
                if weights[i] > caps[i] {
                    excess += weights[i] - caps[i];
                    weights[i] = caps[i];
                } else if weights[i] < caps[i] {
                    free_rate += rates[i] as u128;
                }
            }
            if excess == 0 || free_rate == 0 {
                break;
            }
            for i in 0..weights.len() {
                if weights[i] < caps[i] {
                    weights[i] += (excess * rates[i] as u128) / free_rate;
                }
            }
        }
        let placed: u128 = weights.iter().sum();
        if placed + (weights.len() as u128) < 1_000_000 {
            // Cannot hold every row with this set.
            continue;
        }
        let weights_u64: Vec<u64> = weights.iter().map(|w| *w as u64).collect();
        let rows_per_stage: Vec<Vec<u64>> = stages
            .iter()
            .map(|s| split_rows(s.rows, &weights_u64))
            .collect();
        let t = predict(stages, &rates, &links, &rows_per_stage, policy);
        let better = match &best {
            None => true,
            Some((bt, bk, _)) => t < *bt || (t == *bt && k < *bk),
        };
        if better {
            best = Some((t, k, rows_per_stage));
        }
    }
    let Some((predicted, k, rows_per_stage)) = best else {
        return Err(PlacementError::Infeasible);
    };

    let mut participants: Vec<Participant> = Vec::new();
    if policy.include_coordinator {
        participants.push(Participant::Coordinator);
    }
    for c in &pool[..k] {
        participants.push(Participant::Worker(c.worker));
    }
    // Slices in a canonical participant order: coordinator, then workers by
    // address - independent of how fast each was.
    let mut order: Vec<usize> = (0..participants.len()).collect();
    order.sort_by_key(|i| participants[*i].order_key());
    let stage_plans = stages
        .iter()
        .enumerate()
        .map(|(si, _)| {
            let mut cursor = 0u64;
            let mut slices = Vec::new();
            for pi in &order {
                let rows = rows_per_stage[si][*pi];
                if rows == 0 {
                    continue;
                }
                slices.push(Slice {
                    participant: participants[*pi].clone(),
                    row_start: cursor,
                    row_end: cursor + rows,
                });
                cursor += rows;
            }
            StagePlan { stage: si, slices }
        })
        .collect();
    let mut workers: Vec<Address> = pool[..k].iter().map(|c| c.worker).collect();
    workers.sort_by_key(|w| w.0);
    Ok(Placement {
        workers,
        stages: stage_plans,
        predicted_token_us: predicted,
        coordinator_only_token_us: coordinator_only,
    })
}

/// Every stage covered exactly once, in order: the invariant a certificate
/// and the executor both check before any row is computed.
pub fn covers_exactly(placement: &Placement, stages: &[Stage]) -> bool {
    placement.stages.len() == stages.len()
        && placement.stages.iter().zip(stages).all(|(plan, stage)| {
            let mut cursor = 0u64;
            for s in &plan.slices {
                if s.row_start != cursor || s.row_end <= s.row_start {
                    return false;
                }
                cursor = s.row_end;
            }
            cursor == stage.rows
        })
}
