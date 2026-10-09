//! Capacity-aware inference pricing v0: an advisory base fee and quotes.
//!
//! A coordinator keeps one base fee per served model, in ARC base units per
//! 1,000,000 billable tokens. Once per epoch the fee moves toward a target
//! utilization of *verified* community capacity, EIP-1559 style: idle
//! verified capacity lowers it, congestion raises it, and one epoch can move
//! it by at most `max_step_bps`. It never falls below a floor derived from the
//! worker floor (what each executing worker must recover, set by the operator
//! to cover electricity) and the worker/verifier/treasury split.
//!
//! Everything here is off by default and advisory. Paid inference remains
//! disabled at every public ingress, so a quote holds, charges and refunds
//! nothing, and nothing in this module touches consensus or chain state. The
//! design, the simulation and the on-chain follow-up are in
//! `docs/inference-pricing.md`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_crypto::Hash256;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use tower_http::cors::CorsLayer;

use crate::rpc::{COMMUNITY_WORKER_TTL_SECS, CommunityWorker, NodeState, WorkItem};

/// Basis points in one whole.
pub const BPS: u64 = 10_000;
/// Prices are per this many billable tokens.
pub const TOKENS_PER_MTOK: u64 = 1_000_000;

/// Read-only route: configuration, base fee, capacity and recent epochs.
pub const PRICING_PATH: &str = "/inference/pricing";
/// Read-only route: an advisory quote for one answer.
pub const QUOTE_PATH: &str = "/inference/quote";
pub const PRICING_SCHEMA: &str = "arc.inference.pricing.v0";
pub const QUOTE_SCHEMA: &str = "arc.inference.quote.v0";

pub const DEFAULT_TARGET_UTILIZATION_BPS: u32 = 5_000;
pub const DEFAULT_MAX_STEP_BPS: u32 = 1_250;
pub const DEFAULT_EPOCH_SECS: u64 = 600;
pub const DEFAULT_SAMPLE_INTERVAL_SECS: u64 = 15;
/// Nominal testnet worker floor: 0.001 ARC per 1M billable tokens for each
/// executing worker. Testnet ARC has no monetary value; the number exists to
/// exercise the mechanism, not to price anything.
pub const DEFAULT_WORKER_FLOOR_PER_MTOK: u64 = 1_000_000;
pub const DEFAULT_CEILING_MULTIPLE: u64 = 1_000;
pub const DEFAULT_TREASURY_BPS: u32 = 1_000;
/// Sized for twin execution: validators recompute a 5% spot check on three
/// replicas, about 0.15 executions per job against two paid worker runs.
pub const DEFAULT_VERIFIER_BPS: u32 = 700;
/// Twin execution pays two community workers per job.
pub const DEFAULT_WORKER_EXECUTIONS: u8 = 2;
/// Today's engine runs every prompt position as a full forward pass, so a
/// prompt token costs what an output token costs.
pub const DEFAULT_INPUT_WEIGHT_BPS: u32 = 10_000;
pub const DEFAULT_EXPECTED_OUTPUT_BPS: u32 = 5_000;
pub const DEFAULT_MIN_JOB_TOKENS: u32 = 64;

/// Busy workers plus queued jobs can exceed verified capacity; utilization is
/// reported up to 200%.
pub const MAX_UTILIZATION_BPS: u32 = 20_000;
pub const MAX_QUOTE_PROMPT_TOKENS: u32 = 32_768;
/// A quote never prices more output than a community job may request.
pub const MAX_QUOTE_OUTPUT_TOKENS: u32 = crate::rpc::INFERENCE_RUN_MAX_TOKENS;
pub const HISTORY_EPOCHS: usize = 48;
pub const MAX_PRICED_MODELS: usize = 16;

const SETTLEMENT_NOTE: &str = "none in v0: paid inference is disabled on this network, so no ARC is held, charged or refunded";
const VALUE_NOTICE: &str = "Advisory testnet quote. Testnet ARC has no monetary value. A quote is not an offer of service and not a promise of income to anyone.";
const UNIT_NOTE: &str =
    "ARC base units per 1,000,000 billable tokens (1 ARC = 1,000,000,000 base units)";
const CAPACITY_BASIS: &str = "verified = heartbeat within the worker TTL, canonical INT8 profile, this coordinator's model and at least one quorum-verified job; busy = verified but not long-polling this coordinator; utilization = (busy + queued here) / verified";
const TIP_NOTE: &str =
    "v0 adds the tip to the price; the community queue is FIFO and does not yet order jobs by tip";
const ESCROW_RULE: &str = "a paid job would hold the max and refund, at the quoted price, whatever the tokens it actually generated did not use";

/// Operator configuration. `PricingConfig::default()` is disabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PricingConfig {
    pub enabled: bool,
    pub target_utilization_bps: u32,
    pub max_step_bps: u32,
    pub epoch_secs: u64,
    pub sample_interval_secs: u64,
    /// ARC base units each executing worker must receive per 1M billable
    /// tokens. The base-fee floor is derived from it and the split.
    pub worker_floor_per_mtok: u64,
    pub ceiling_multiple: u64,
    pub treasury_bps: u32,
    pub verifier_bps: u32,
    /// Community workers paid per job: 2 under twin execution, else 1.
    pub worker_executions: u8,
    pub input_weight_bps: u32,
    pub expected_output_bps: u32,
    pub min_job_tokens: u32,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target_utilization_bps: DEFAULT_TARGET_UTILIZATION_BPS,
            max_step_bps: DEFAULT_MAX_STEP_BPS,
            epoch_secs: DEFAULT_EPOCH_SECS,
            sample_interval_secs: DEFAULT_SAMPLE_INTERVAL_SECS,
            worker_floor_per_mtok: DEFAULT_WORKER_FLOOR_PER_MTOK,
            ceiling_multiple: DEFAULT_CEILING_MULTIPLE,
            treasury_bps: DEFAULT_TREASURY_BPS,
            verifier_bps: DEFAULT_VERIFIER_BPS,
            worker_executions: DEFAULT_WORKER_EXECUTIONS,
            input_weight_bps: DEFAULT_INPUT_WEIGHT_BPS,
            expected_output_bps: DEFAULT_EXPECTED_OUTPUT_BPS,
            min_job_tokens: DEFAULT_MIN_JOB_TOKENS,
        }
    }
}

impl PricingConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1_000..=9_000).contains(&self.target_utilization_bps) {
            return Err("target utilization must be 1000-9000 basis points".to_string());
        }
        if !(1..=5_000).contains(&self.max_step_bps) {
            return Err("max step must be 1-5000 basis points".to_string());
        }
        if !(60..=86_400).contains(&self.epoch_secs) {
            return Err("epoch length must be 60-86400 seconds".to_string());
        }
        if self.sample_interval_secs == 0 || self.sample_interval_secs >= self.epoch_secs {
            return Err(
                "sample interval must be at least 1 second and shorter than the epoch".to_string(),
            );
        }
        if self.worker_floor_per_mtok == 0 {
            return Err("worker floor must be at least 1 base unit".to_string());
        }
        if !(1..=1_000_000).contains(&self.ceiling_multiple) {
            return Err("ceiling multiple must be 1-1000000".to_string());
        }
        if self.treasury_bps > 3_000 || self.verifier_bps > 3_000 {
            return Err(
                "treasury and verifier shares must each be at most 3000 basis points".to_string(),
            );
        }
        if !(1..=2).contains(&self.worker_executions) {
            return Err("worker executions per job must be 1 or 2".to_string());
        }
        if !(1..=10_000).contains(&self.input_weight_bps) {
            return Err("input weight must be 1-10000 basis points".to_string());
        }
        if !(1..=10_000).contains(&self.expected_output_bps) {
            return Err("expected output share must be 1-10000 basis points".to_string());
        }
        if !(1..=4_096).contains(&self.min_job_tokens) {
            return Err("minimum billable tokens per job must be 1-4096".to_string());
        }
        Ok(())
    }

    /// Basis points of the base fee shared by the executing workers.
    pub fn worker_pool_bps(&self) -> u64 {
        BPS.saturating_sub(u64::from(self.treasury_bps))
            .saturating_sub(u64::from(self.verifier_bps))
    }

    /// The lowest base fee at which every executing worker's share still
    /// covers `worker_floor_per_mtok`.
    pub fn base_fee_floor(&self) -> u64 {
        let pool = u128::from(self.worker_pool_bps().max(1));
        let needed = u128::from(self.worker_floor_per_mtok)
            * u128::from(self.worker_executions.max(1))
            * u128::from(BPS);
        u64::try_from(needed.div_ceil(pool)).unwrap_or(u64::MAX)
    }

    pub fn fee_params(&self) -> FeeParams {
        let floor_per_mtok = self.base_fee_floor();
        FeeParams {
            floor_per_mtok,
            ceiling_per_mtok: floor_per_mtok.saturating_mul(self.ceiling_multiple.max(1)),
            target_utilization_bps: self.target_utilization_bps,
            max_step_bps: self.max_step_bps,
        }
    }
}

/// Operator flags, flattened into the node's command line.
#[derive(clap::Args, Clone, Debug)]
pub struct PricingArgs {
    /// Coordinator: keep a capacity-aware base fee per 1M tokens for the served
    /// model and answer advisory quotes at GET /inference/quote and GET
    /// /inference/pricing. Off by default. v0 moves no ARC; paid inference
    /// stays disabled. See docs/inference-pricing.md.
    #[arg(long, default_value_t = false)]
    pub enable_inference_pricing: bool,

    /// Target utilization of verified community capacity, in basis points.
    #[arg(
        long,
        default_value_t = DEFAULT_TARGET_UTILIZATION_BPS,
        value_parser = clap::value_parser!(u32).range(1_000..=9_000)
    )]
    pub inference_pricing_target_utilization_bps: u32,

    /// Largest base-fee change per epoch, in basis points of the current fee.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_STEP_BPS,
        value_parser = clap::value_parser!(u32).range(1..=5_000)
    )]
    pub inference_pricing_max_step_bps: u32,

    /// Epoch length in seconds. The base fee changes once per epoch.
    #[arg(
        long,
        default_value_t = DEFAULT_EPOCH_SECS,
        value_parser = clap::value_parser!(u64).range(60..=86_400)
    )]
    pub inference_pricing_epoch_secs: u64,

    /// ARC base units each executing worker must receive per 1M billable
    /// tokens, set to cover its electricity. The base-fee floor follows from
    /// it and the split.
    #[arg(
        long,
        default_value_t = DEFAULT_WORKER_FLOOR_PER_MTOK,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub inference_pricing_worker_floor_per_mtok: u64,

    /// Base-fee ceiling as a multiple of the floor.
    #[arg(
        long,
        default_value_t = DEFAULT_CEILING_MULTIPLE,
        value_parser = clap::value_parser!(u64).range(1..=1_000_000)
    )]
    pub inference_pricing_ceiling_multiple: u64,

    /// Treasury share of the base fee, in basis points.
    #[arg(
        long,
        default_value_t = DEFAULT_TREASURY_BPS,
        value_parser = clap::value_parser!(u32).range(0..=3_000)
    )]
    pub inference_pricing_treasury_bps: u32,

    /// Verifier-pool share of the base fee, in basis points.
    #[arg(
        long,
        default_value_t = DEFAULT_VERIFIER_BPS,
        value_parser = clap::value_parser!(u32).range(0..=3_000)
    )]
    pub inference_pricing_verifier_bps: u32,

    /// Community workers paid per job: 2 under twin execution, 1 otherwise.
    #[arg(
        long,
        default_value_t = DEFAULT_WORKER_EXECUTIONS,
        value_parser = clap::value_parser!(u8).range(1..=2)
    )]
    pub inference_pricing_worker_executions: u8,
}

impl PricingArgs {
    pub fn to_config(&self) -> PricingConfig {
        PricingConfig {
            enabled: self.enable_inference_pricing,
            target_utilization_bps: self.inference_pricing_target_utilization_bps,
            max_step_bps: self.inference_pricing_max_step_bps,
            epoch_secs: self.inference_pricing_epoch_secs,
            worker_floor_per_mtok: self.inference_pricing_worker_floor_per_mtok,
            ceiling_multiple: self.inference_pricing_ceiling_multiple,
            treasury_bps: self.inference_pricing_treasury_bps,
            verifier_bps: self.inference_pricing_verifier_bps,
            worker_executions: self.inference_pricing_worker_executions,
            ..PricingConfig::default()
        }
    }
}

/// The bounds one epoch update works within.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FeeParams {
    pub floor_per_mtok: u64,
    pub ceiling_per_mtok: u64,
    pub target_utilization_bps: u32,
    pub max_step_bps: u32,
}

/// One epoch of the EIP-1559-style update, in integer arithmetic.
///
/// `None` means no verified capacity was observed, which holds the fee. The
/// step is proportional to `(utilization - target) / target`, capped at
/// `max_step_bps` of the current fee either way. An increase is at least one
/// base unit, so congestion always moves the fee. The result stays within
/// `[floor, ceiling]`. `scripts/inference_pricing_sim.py` implements the same
/// rule, and `base_fee_matches_python_parity_vector` keeps the two equal.
pub fn next_base_fee(current: u64, utilization_bps: Option<u32>, params: &FeeParams) -> u64 {
    let floor = params.floor_per_mtok;
    let ceiling = params.ceiling_per_mtok.max(floor);
    let current = current.clamp(floor, ceiling);
    let Some(utilization) = utilization_bps else {
        return current;
    };
    let target = u128::from(params.target_utilization_bps.max(1));
    let utilization = u128::from(utilization).min(2 * target);
    let step = u128::from(params.max_step_bps);
    let scale = target * u128::from(BPS);
    let next = match utilization.cmp(&target) {
        Ordering::Greater => {
            let delta = u128::from(current) * step * (utilization - target) / scale;
            current.saturating_add(u64::try_from(delta).unwrap_or(u64::MAX).max(1))
        }
        Ordering::Less => {
            let delta = u128::from(current) * step * (target - utilization) / scale;
            current.saturating_sub(u64::try_from(delta).unwrap_or(u64::MAX))
        }
        Ordering::Equal => current,
    };
    next.clamp(floor, ceiling)
}

/// What one sample of the coordinator's community registry shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CapacitySnapshot {
    /// Live, eligible workers with at least one quorum-verified job.
    pub verified_workers: u32,
    /// Verified workers long-polling this coordinator for work.
    pub idle_here: u32,
    /// Verified workers computing a job this coordinator assigned.
    pub assigned_here: u32,
    /// Live, eligible workers with no verified job yet. Not capacity.
    pub unverified_workers: u32,
    /// Jobs waiting in this coordinator's queue for a worker.
    pub queued_here: u32,
}

/// Running totals for the current epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EpochAccumulator {
    pub samples: u32,
    pub samples_with_capacity: u32,
    pub verified_worker_samples: u64,
    pub busy_worker_samples: u64,
    pub queued_job_samples: u64,
}

impl EpochAccumulator {
    pub fn record(&mut self, snapshot: &CapacitySnapshot) {
        self.samples = self.samples.saturating_add(1);
        let verified = u64::from(snapshot.verified_workers);
        if verified == 0 {
            return;
        }
        let idle = u64::from(snapshot.idle_here).min(verified);
        self.samples_with_capacity = self.samples_with_capacity.saturating_add(1);
        self.verified_worker_samples = self.verified_worker_samples.saturating_add(verified);
        self.busy_worker_samples = self.busy_worker_samples.saturating_add(verified - idle);
        self.queued_job_samples = self
            .queued_job_samples
            .saturating_add(u64::from(snapshot.queued_here));
    }

    /// Utilization of verified capacity so far, or `None` when no verified
    /// capacity was observed.
    pub fn utilization_bps(&self) -> Option<u32> {
        if self.verified_worker_samples == 0 {
            return None;
        }
        let demand = u128::from(self.busy_worker_samples) + u128::from(self.queued_job_samples);
        let bps = demand * u128::from(BPS) / u128::from(self.verified_worker_samples);
        Some(u32::try_from(bps.min(u128::from(MAX_UTILIZATION_BPS))).unwrap_or(MAX_UTILIZATION_BPS))
    }
}

/// One closed epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EpochRecord {
    pub epoch: u64,
    pub samples: u32,
    pub utilization_bps: Option<u32>,
    pub base_fee_before: u64,
    pub base_fee_after: u64,
}

/// Pricing state for one model.
#[derive(Clone, Debug)]
pub struct ModelPricing {
    pub base_fee_per_mtok: u64,
    pub epoch: u64,
    pub accumulator: EpochAccumulator,
    pub last_snapshot: Option<CapacitySnapshot>,
    pub history: VecDeque<EpochRecord>,
}

impl ModelPricing {
    fn new(base_fee_per_mtok: u64, epoch: u64) -> Self {
        Self {
            base_fee_per_mtok,
            epoch,
            accumulator: EpochAccumulator::default(),
            last_snapshot: None,
            history: VecDeque::new(),
        }
    }
}

/// Per-model base fees. The fee starts at the floor and rises only when
/// verified capacity is congested.
#[derive(Clone, Debug)]
pub struct PricingEngine {
    config: PricingConfig,
    params: FeeParams,
    models: BTreeMap<[u8; 32], ModelPricing>,
}

impl PricingEngine {
    pub fn new(config: PricingConfig) -> Self {
        Self {
            config,
            params: config.fee_params(),
            models: BTreeMap::new(),
        }
    }

    pub fn config(&self) -> PricingConfig {
        self.config
    }

    pub fn params(&self) -> FeeParams {
        self.params
    }

    /// Epochs are aligned to Unix time, so coordinators with the same
    /// configuration share boundaries.
    pub fn epoch_at(&self, unix_secs: u64) -> u64 {
        unix_secs / self.config.epoch_secs.max(1)
    }

    pub fn model(&self, model_id: Hash256) -> Option<&ModelPricing> {
        self.models.get(&model_id.0)
    }

    pub fn models(&self) -> impl Iterator<Item = (Hash256, &ModelPricing)> {
        self.models.iter().map(|(id, model)| (Hash256(*id), model))
    }

    /// Bring a model to the epoch containing `unix_secs`, closing the
    /// current epoch first if it has ended. Epochs skipped without samples
    /// hold the fee. Returns `None` only when the model table is full.
    pub fn advance(&mut self, model_id: Hash256, unix_secs: u64) -> Option<&mut ModelPricing> {
        let epoch = self.epoch_at(unix_secs);
        let params = self.params;
        if !self.models.contains_key(&model_id.0) && self.models.len() >= MAX_PRICED_MODELS {
            return None;
        }
        let model = self
            .models
            .entry(model_id.0)
            .or_insert_with(|| ModelPricing::new(params.floor_per_mtok, epoch));
        if epoch > model.epoch {
            let utilization_bps = model.accumulator.utilization_bps();
            let before = model.base_fee_per_mtok;
            let after = next_base_fee(before, utilization_bps, &params);
            model.history.push_back(EpochRecord {
                epoch: model.epoch,
                samples: model.accumulator.samples,
                utilization_bps,
                base_fee_before: before,
                base_fee_after: after,
            });
            while model.history.len() > HISTORY_EPOCHS {
                model.history.pop_front();
            }
            model.base_fee_per_mtok = after;
            model.epoch = epoch;
            model.accumulator = EpochAccumulator::default();
        }
        Some(model)
    }

    /// Add one capacity sample. Returns `false` when the model table is full.
    pub fn record_sample(
        &mut self,
        model_id: Hash256,
        snapshot: CapacitySnapshot,
        unix_secs: u64,
    ) -> bool {
        match self.advance(model_id, unix_secs) {
            Some(model) => {
                model.accumulator.record(&snapshot);
                model.last_snapshot = Some(snapshot);
                true
            }
            None => false,
        }
    }
}

/// What a caller asks to have priced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuoteRequest {
    pub prompt_tokens: u32,
    pub max_output_tokens: u32,
    pub expected_output_tokens: Option<u32>,
    pub tip_per_mtok: u64,
}

/// How one charge would be divided, in base units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Split {
    pub workers: u8,
    pub per_worker: u64,
    pub verifier_pool: u64,
    pub treasury: u64,
}

impl Split {
    pub fn total(&self) -> u64 {
        self.per_worker
            .saturating_mul(u64::from(self.workers))
            .saturating_add(self.verifier_pool)
            .saturating_add(self.treasury)
    }
}

/// An advisory price for one answer. `max` is what a paid job would hold in
/// escrow; `estimate` assumes the expected output length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Quote {
    pub base_fee_per_mtok: u64,
    pub tip_per_mtok: u64,
    pub price_per_mtok: u64,
    pub prompt_tokens: u32,
    pub max_output_tokens: u32,
    pub expected_output_tokens: u32,
    pub billable_tokens_expected: u64,
    pub billable_tokens_max: u64,
    pub estimate: u64,
    pub max: u64,
    pub split_at_max: Split,
}

/// Prompt tokens weighted by `input_weight_bps`, plus output tokens, and at
/// least `min_job_tokens`.
pub fn billable_tokens(config: &PricingConfig, prompt_tokens: u32, output_tokens: u32) -> u64 {
    let weighted_prompt =
        (u64::from(prompt_tokens) * u64::from(config.input_weight_bps)).div_ceil(BPS);
    weighted_prompt
        .saturating_add(u64::from(output_tokens))
        .max(u64::from(config.min_job_tokens))
}

/// Base units owed for `billable_tokens` at `price_per_mtok`, rounded up so a
/// hold never under-collects.
pub fn charge_for(billable_tokens: u64, price_per_mtok: u64) -> u64 {
    let amount = (u128::from(billable_tokens) * u128::from(price_per_mtok))
        .div_ceil(u128::from(TOKENS_PER_MTOK));
    u64::try_from(amount).unwrap_or(u64::MAX)
}

fn apply_bps(amount: u64, bps: u32) -> u64 {
    u64::try_from(u128::from(amount) * u128::from(bps) / u128::from(BPS)).unwrap_or(u64::MAX)
}

/// Divide a charge: the tip goes entirely to the executing workers; the
/// base-fee part pays treasury and verifier pool their basis points and the
/// workers the rest, in equal shares. Rounding residue goes to the treasury,
/// so the parts always add up to `charge`.
pub fn split_charge(
    config: &PricingConfig,
    charge: u64,
    billable_tokens: u64,
    tip_per_mtok: u64,
) -> Split {
    let tip_part = u64::try_from(
        u128::from(billable_tokens) * u128::from(tip_per_mtok) / u128::from(TOKENS_PER_MTOK),
    )
    .unwrap_or(u64::MAX)
    .min(charge);
    let base_part = charge - tip_part;
    let treasury = apply_bps(base_part, config.treasury_bps);
    let verifier_pool = apply_bps(base_part, config.verifier_bps);
    let worker_pool = base_part
        .saturating_sub(treasury)
        .saturating_sub(verifier_pool)
        .saturating_add(tip_part);
    let workers = config.worker_executions.max(1);
    let per_worker = worker_pool / u64::from(workers);
    let residue = worker_pool - per_worker * u64::from(workers);
    Split {
        workers,
        per_worker,
        verifier_pool,
        treasury: treasury.saturating_add(residue),
    }
}

fn default_expected_output(config: &PricingConfig, max_output_tokens: u32) -> u32 {
    let expected =
        (u64::from(max_output_tokens) * u64::from(config.expected_output_bps)).div_ceil(BPS);
    u32::try_from(expected)
        .unwrap_or(max_output_tokens)
        .clamp(1, max_output_tokens.max(1))
}

/// Price one answer at `base_fee_per_mtok` plus the caller's tip.
pub fn build_quote(
    config: &PricingConfig,
    base_fee_per_mtok: u64,
    request: &QuoteRequest,
) -> Result<Quote, String> {
    if !(1..=MAX_QUOTE_OUTPUT_TOKENS).contains(&request.max_output_tokens) {
        return Err(format!("max_tokens must be 1-{MAX_QUOTE_OUTPUT_TOKENS}"));
    }
    if request.prompt_tokens > MAX_QUOTE_PROMPT_TOKENS {
        return Err(format!(
            "prompt_tokens must be at most {MAX_QUOTE_PROMPT_TOKENS}"
        ));
    }
    let expected_output_tokens = match request.expected_output_tokens {
        Some(expected) if expected > request.max_output_tokens => {
            return Err("expected_output_tokens cannot exceed max_tokens".to_string());
        }
        Some(expected) => expected,
        None => default_expected_output(config, request.max_output_tokens),
    };
    let price_per_mtok = base_fee_per_mtok.saturating_add(request.tip_per_mtok);
    let billable_tokens_max =
        billable_tokens(config, request.prompt_tokens, request.max_output_tokens);
    let billable_tokens_expected =
        billable_tokens(config, request.prompt_tokens, expected_output_tokens);
    let max = charge_for(billable_tokens_max, price_per_mtok);
    Ok(Quote {
        base_fee_per_mtok,
        tip_per_mtok: request.tip_per_mtok,
        price_per_mtok,
        prompt_tokens: request.prompt_tokens,
        max_output_tokens: request.max_output_tokens,
        expected_output_tokens,
        billable_tokens_expected,
        billable_tokens_max,
        estimate: charge_for(billable_tokens_expected, price_per_mtok),
        max,
        split_at_max: split_charge(config, max, billable_tokens_max, request.tip_per_mtok),
    })
}

/// Settle a finished job against its quote: charge the tokens actually
/// generated at the quoted price, never more than the held max, and refund
/// the rest. Returns `(charge, refund)`.
pub fn settle(config: &PricingConfig, quote: &Quote, generated_output_tokens: u32) -> (u64, u64) {
    let generated = generated_output_tokens.min(quote.max_output_tokens);
    let charge = charge_for(
        billable_tokens(config, quote.prompt_tokens, generated),
        quote.price_per_mtok,
    )
    .min(quote.max);
    (charge, quote.max - charge)
}

/// Whole ARC from base units, without trailing zeros.
pub fn format_arc(base_units: u64) -> String {
    let unit = arc_types::economics::ARC_BASE_UNITS;
    let whole = base_units / unit;
    let fraction = base_units % unit;
    if fraction == 0 {
        return whole.to_string();
    }
    let digits = format!("{fraction:09}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

/// The line a user sees before running a job.
pub fn quote_display(quote: &Quote) -> String {
    let estimate = format_arc(quote.estimate);
    let max = format_arc(quote.max);
    format!("≈ {estimate} ARC for this answer (max {max} ARC)")
}

/// Read-only handles on the coordinator's community state. It holds no
/// `StateDB`, so a sampler can never keep chain state alive.
#[derive(Clone)]
pub struct CapacitySource {
    workers: Arc<dashmap::DashMap<String, (CommunityWorker, Instant)>>,
    active_jobs: Arc<dashmap::DashMap<String, String>>,
    work_queue: Option<Arc<tokio::sync::mpsc::Sender<WorkItem>>>,
    model_id: Option<Hash256>,
}

impl CapacitySource {
    pub fn from_node(node: &NodeState) -> Self {
        Self {
            workers: node.community_workers.clone(),
            active_jobs: node.community_active_jobs.clone(),
            work_queue: node.community_work_tx.clone(),
            model_id: node.inference_model.as_ref().and(node.model_artifact_id),
        }
    }

    /// The model this coordinator serves, if it has one loaded.
    pub fn model_id(&self) -> Option<Hash256> {
        self.model_id
    }

    pub fn snapshot(&self, now: Instant) -> CapacitySnapshot {
        let mut snapshot = CapacitySnapshot::default();
        let Some(model_id) = self.model_id else {
            return snapshot;
        };
        let ttl = Duration::from_secs(COMMUNITY_WORKER_TTL_SECS);
        // Collect first, then read reservations, so no registry guard is held
        // while another map is locked.
        let mut verified = Vec::new();
        for entry in self.workers.iter() {
            let (worker, refreshed_at) = entry.value();
            if now.saturating_duration_since(*refreshed_at) > ttl
                || !worker_is_eligible(worker, model_id)
            {
                continue;
            }
            if worker.success_count == 0 {
                snapshot.unverified_workers = snapshot.unverified_workers.saturating_add(1);
            } else {
                verified.push(worker.worker_id.clone());
            }
        }
        for worker_id in &verified {
            snapshot.verified_workers = snapshot.verified_workers.saturating_add(1);
            match self.active_jobs.get(worker_id) {
                Some(job) if job.value().is_empty() => {
                    snapshot.idle_here = snapshot.idle_here.saturating_add(1);
                }
                Some(_) => {
                    snapshot.assigned_here = snapshot.assigned_here.saturating_add(1);
                }
                None => {}
            }
        }
        if let Some(queue) = &self.work_queue {
            let queued = queue.max_capacity().saturating_sub(queue.capacity());
            snapshot.queued_here = u32::try_from(queued).unwrap_or(u32::MAX);
        }
        snapshot
    }
}

fn worker_is_eligible(worker: &CommunityWorker, model_id: Hash256) -> bool {
    worker
        .capabilities
        .iter()
        .any(|capability| capability == "inference")
        && worker.execution_profile.as_deref()
            == Some(arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE)
        && worker.model_id.as_deref().and_then(parse_model_id) == Some(model_id)
}

fn parse_model_id(value: &str) -> Option<Hash256> {
    let trimmed = value.trim();
    let bare = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    Hash256::from_hex(bare).ok()
}

fn hex_id(model_id: Hash256) -> String {
    format!("0x{}", model_id.to_hex())
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn config_json(config: &PricingConfig, params: &FeeParams) -> Value {
    json!({
        "target_utilization_bps": config.target_utilization_bps,
        "max_step_bps": config.max_step_bps,
        "epoch_secs": config.epoch_secs,
        "sample_interval_secs": config.sample_interval_secs,
        "worker_floor_per_mtok": config.worker_floor_per_mtok,
        "base_fee_floor_per_mtok": params.floor_per_mtok,
        "base_fee_ceiling_per_mtok": params.ceiling_per_mtok,
        "split_bps": {
            "workers": config.worker_pool_bps(),
            "verifier_pool": config.verifier_bps,
            "treasury": config.treasury_bps,
        },
        "worker_executions_per_job": config.worker_executions,
        "input_weight_bps": config.input_weight_bps,
        "expected_output_bps": config.expected_output_bps,
        "min_job_tokens": config.min_job_tokens,
    })
}

fn model_json(
    model_id: Hash256,
    model: &ModelPricing,
    params: &FeeParams,
    epoch_secs: u64,
) -> Value {
    let epoch_ends_unix_ms = model
        .epoch
        .saturating_add(1)
        .saturating_mul(epoch_secs)
        .saturating_mul(1_000);
    json!({
        "model_id": hex_id(model_id),
        "base_fee_per_mtok": model.base_fee_per_mtok,
        "base_fee_arc_per_mtok": format_arc(model.base_fee_per_mtok),
        "floor_per_mtok": params.floor_per_mtok,
        "ceiling_per_mtok": params.ceiling_per_mtok,
        "epoch": model.epoch,
        "epoch_ends_unix_ms": epoch_ends_unix_ms,
        "current_epoch": {
            "samples": model.accumulator.samples,
            "samples_with_verified_capacity": model.accumulator.samples_with_capacity,
            "utilization_bps_so_far": model.accumulator.utilization_bps(),
        },
        "last_snapshot": model.last_snapshot,
        "history": model.history,
    })
}

/// Body of `GET /inference/pricing`.
pub fn pricing_status(
    engine: &Mutex<PricingEngine>,
    source: &CapacitySource,
    now: Instant,
    unix_secs: u64,
) -> Value {
    let capacity = source.snapshot(now);
    let served_model = source.model_id();
    let mut engine = engine.lock();
    if let Some(model_id) = served_model {
        // Close an epoch the sampler has not reached yet, so every reader
        // sees the fee the sampler would set.
        engine.advance(model_id, unix_secs);
    }
    let config = engine.config();
    let params = engine.params();
    let models: Vec<Value> = engine
        .models()
        .map(|(model_id, model)| model_json(model_id, model, &params, config.epoch_secs))
        .collect();
    json!({
        "schema": PRICING_SCHEMA,
        "advisory": true,
        "settlement": SETTLEMENT_NOTE,
        "testnet_units_have_no_value": true,
        "notice": VALUE_NOTICE,
        "unit": UNIT_NOTE,
        "served_model_id": served_model.map(hex_id),
        "epoch_now": engine.epoch_at(unix_secs),
        "config": config_json(&config, &params),
        "capacity_now": capacity,
        "capacity_basis": CAPACITY_BASIS,
        "models": models,
    })
}

fn error_response(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message })))
}

fn parse_param<T: std::str::FromStr>(
    params: &HashMap<String, String>,
    key: &str,
) -> Result<Option<T>, String> {
    params
        .get(key)
        .map(|raw| {
            raw.trim()
                .parse::<T>()
                .map_err(|_| format!("{key} must be a non-negative integer"))
        })
        .transpose()
}

fn parse_quote_request(params: &HashMap<String, String>) -> Result<QuoteRequest, String> {
    Ok(QuoteRequest {
        prompt_tokens: parse_param(params, "prompt_tokens")?.ok_or("prompt_tokens is required")?,
        max_output_tokens: parse_param(params, "max_tokens")?.ok_or("max_tokens is required")?,
        expected_output_tokens: parse_param(params, "expected_output_tokens")?,
        tip_per_mtok: parse_param(params, "tip_per_mtok")?.unwrap_or(0),
    })
}

/// Body of `GET /inference/quote`.
pub fn pricing_quote(
    engine: &Mutex<PricingEngine>,
    source: &CapacitySource,
    params: &HashMap<String, String>,
    now: Instant,
    unix_secs: u64,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let request = parse_quote_request(params)
        .map_err(|message| error_response(StatusCode::BAD_REQUEST, &message))?;
    let Some(model_id) = source.model_id() else {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "this coordinator has no model loaded, so it prices nothing",
        ));
    };
    if let Some(requested) = params.get("model_id") {
        let requested = parse_model_id(requested).ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "model_id must be a 32-byte hex hash",
            )
        })?;
        if requested != model_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "this coordinator does not price that model",
            ));
        }
    }
    let capacity = source.snapshot(now);
    let (config, fee_params, base_fee, epoch, last_utilization) = {
        let mut engine = engine.lock();
        let config = engine.config();
        let fee_params = engine.params();
        let model = engine.advance(model_id, unix_secs).ok_or_else(|| {
            error_response(StatusCode::SERVICE_UNAVAILABLE, "the pricing table is full")
        })?;
        let last_utilization = model
            .history
            .back()
            .and_then(|record| record.utilization_bps);
        (
            config,
            fee_params,
            model.base_fee_per_mtok,
            model.epoch,
            last_utilization,
        )
    };
    let quote = build_quote(&config, base_fee, &request)
        .map_err(|message| error_response(StatusCode::BAD_REQUEST, &message))?;
    // One internal BOS position plus every prompt and output position, checked
    // against the same dispatch budget the coordinator enforces before enqueue.
    let positions =
        usize::try_from(u64::from(quote.prompt_tokens) + u64::from(quote.max_output_tokens) + 1)
            .unwrap_or(usize::MAX);
    let fits_dispatch_budget = crate::rpc::community_dispatch_timeout_secs(positions).is_ok();
    let enough_workers = capacity.verified_workers >= u32::from(config.worker_executions);
    let unserviceable_reason = if capacity.verified_workers == 0 {
        Some("no verified community worker is online")
    } else if !enough_workers {
        Some("fewer verified community workers are online than one job needs")
    } else if !fits_dispatch_budget {
        Some(
            "prompt_tokens plus max_tokens exceed the community dispatch budget, so the coordinator would not send this job to community workers",
        )
    } else {
        None
    };
    let serviceable = unserviceable_reason.is_none();
    let expected_output_basis = if request.expected_output_tokens.is_some() {
        "caller supplied"
    } else {
        "configured share of max_tokens"
    };
    let valid_until_unix_ms = epoch
        .saturating_add(1)
        .saturating_mul(config.epoch_secs)
        .saturating_mul(1_000);
    Ok(json!({
        "schema": QUOTE_SCHEMA,
        "advisory": true,
        "settlement": SETTLEMENT_NOTE,
        "testnet_units_have_no_value": true,
        "notice": VALUE_NOTICE,
        "display": quote_display(&quote),
        "model_id": hex_id(model_id),
        "epoch": epoch,
        "epoch_secs": config.epoch_secs,
        "valid_until_unix_ms": valid_until_unix_ms,
        "serviceable_now": serviceable,
        "unserviceable_reason": unserviceable_reason,
        "community_dispatch": {
            "positions": positions,
            "fits_budget": fits_dispatch_budget,
        },
        "price": {
            "unit": UNIT_NOTE,
            "base_fee_per_mtok": quote.base_fee_per_mtok,
            "tip_per_mtok": quote.tip_per_mtok,
            "price_per_mtok": quote.price_per_mtok,
            "floor_per_mtok": fee_params.floor_per_mtok,
            "ceiling_per_mtok": fee_params.ceiling_per_mtok,
            "tip_note": TIP_NOTE,
        },
        "tokens": {
            "prompt_tokens": quote.prompt_tokens,
            "input_weight_bps": config.input_weight_bps,
            "max_output_tokens": quote.max_output_tokens,
            "expected_output_tokens": quote.expected_output_tokens,
            "expected_output_basis": expected_output_basis,
            "min_job_tokens": config.min_job_tokens,
            "billable_expected": quote.billable_tokens_expected,
            "billable_max": quote.billable_tokens_max,
        },
        "estimate_base_units": quote.estimate,
        "estimate_arc": format_arc(quote.estimate),
        "max_base_units": quote.max,
        "max_arc": format_arc(quote.max),
        "escrow": {
            "hold_base_units": quote.max,
            "rule": ESCROW_RULE,
        },
        "split_at_max": {
            "workers": quote.split_at_max.workers,
            "per_worker": quote.split_at_max.per_worker,
            "verifier_pool": quote.split_at_max.verifier_pool,
            "treasury": quote.split_at_max.treasury,
            "bps": {
                "workers": config.worker_pool_bps(),
                "verifier_pool": config.verifier_bps,
                "treasury": config.treasury_bps,
            },
        },
        "capacity": {
            "now": capacity,
            "basis": CAPACITY_BASIS,
            "utilization_bps_last_epoch": last_utilization,
            "target_utilization_bps": config.target_utilization_bps,
        },
    }))
}

#[derive(Clone)]
struct ApiState {
    engine: Arc<Mutex<PricingEngine>>,
    source: CapacitySource,
}

async fn pricing_status_route(State(api): State<ApiState>) -> Json<Value> {
    Json(pricing_status(
        &api.engine,
        &api.source,
        Instant::now(),
        unix_now_secs(),
    ))
}

async fn pricing_quote_route(
    State(api): State<ApiState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    pricing_quote(
        &api.engine,
        &api.source,
        &params,
        Instant::now(),
        unix_now_secs(),
    )
    .map(Json)
}

fn router(engine: Arc<Mutex<PricingEngine>>, source: CapacitySource) -> Router {
    Router::new()
        .route(PRICING_PATH, get(pricing_status_route))
        .route(QUOTE_PATH, get(pricing_quote_route))
        .layer(CorsLayer::permissive())
        .with_state(ApiState { engine, source })
}

/// Samples capacity every `sample_interval_secs` until the node shuts down.
pub struct PricingSampler {
    engine: Arc<Mutex<PricingEngine>>,
    source: CapacitySource,
    interval: Duration,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
}

impl PricingSampler {
    pub async fn run(self) {
        let Self {
            engine,
            source,
            interval,
            mut shutdown,
        } = self;
        let Some(model_id) = source.model_id() else {
            return;
        };
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut shutdown) => return,
                _ = ticker.tick() => {}
            }
            let snapshot = source.snapshot(Instant::now());
            engine
                .lock()
                .record_sample(model_id, snapshot, unix_now_secs());
        }
    }
}

async fn wait_for_shutdown(shutdown: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    let Some(receiver) = shutdown.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    if *receiver.borrow_and_update() {
        return;
    }
    while receiver.changed().await.is_ok() {
        if *receiver.borrow_and_update() {
            return;
        }
    }
}

/// Routes and sampler for an enabled configuration.
pub struct PricingMount {
    pub router: Router,
    pub sampler: PricingSampler,
}

/// `None` (mount nothing, sample nothing) unless pricing is enabled and its
/// configuration is valid.
pub fn mount(
    node: &NodeState,
    config: PricingConfig,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> Option<PricingMount> {
    if !config.enabled {
        return None;
    }
    if let Err(error) = config.validate() {
        tracing::error!(%error, "inference pricing stays off: invalid configuration");
        return None;
    }
    let params = config.fee_params();
    tracing::info!(
        target_utilization_bps = config.target_utilization_bps,
        epoch_secs = config.epoch_secs,
        floor_per_mtok = params.floor_per_mtok,
        "advisory inference pricing enabled at {QUOTE_PATH} and {PRICING_PATH}; no ARC moves"
    );
    let source = CapacitySource::from_node(node);
    let engine = Arc::new(Mutex::new(PricingEngine::new(config)));
    Some(PricingMount {
        router: router(engine.clone(), source.clone()),
        sampler: PricingSampler {
            engine,
            source,
            interval: Duration::from_secs(config.sample_interval_secs),
            shutdown,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use tower::ServiceExt;

    fn params() -> FeeParams {
        FeeParams {
            floor_per_mtok: 1_000,
            ceiling_per_mtok: 1_000_000,
            target_utilization_bps: 5_000,
            max_step_bps: 1_250,
        }
    }

    fn enabled() -> PricingConfig {
        PricingConfig {
            enabled: true,
            ..PricingConfig::default()
        }
    }

    fn snapshot(verified: u32, idle: u32, queued: u32) -> CapacitySnapshot {
        CapacitySnapshot {
            verified_workers: verified,
            idle_here: idle,
            queued_here: queued,
            ..CapacitySnapshot::default()
        }
    }

    fn request(prompt_tokens: u32, max_output_tokens: u32) -> QuoteRequest {
        QuoteRequest {
            prompt_tokens,
            max_output_tokens,
            expected_output_tokens: None,
            tip_per_mtok: 0,
        }
    }

    fn worker(id: &str, model_id: Hash256, success_count: u64) -> CommunityWorker {
        serde_json::from_value(json!({
            "worker_id": id,
            "name": "node-test",
            "capabilities": ["inference"],
            "model": null,
            "model_id": hex_id(model_id),
            "execution_profile": arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE,
            "platform": "test",
            "registered_at": 0,
            "work_completed": success_count,
            "success_count": success_count,
        }))
        .unwrap()
    }

    fn work_item(index: usize) -> WorkItem {
        serde_json::from_value(json!({
            "job_id": format!("job-{index}"),
            "input": "hello",
            "max_tokens": 8,
            "execution_profile": arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE,
            "submitted_at_unix_ms": 0,
            "expires_at_unix_ms": 0,
        }))
        .unwrap()
    }

    /// The returned receiver must stay alive while the queue is inspected.
    fn source_with(
        workers: Vec<(CommunityWorker, Instant)>,
        reservations: &[(&str, &str)],
        queued: usize,
        model_id: Option<Hash256>,
    ) -> (CapacitySource, tokio::sync::mpsc::Receiver<WorkItem>) {
        let registry = Arc::new(dashmap::DashMap::new());
        for (worker, seen) in workers {
            registry.insert(worker.worker_id.clone(), (worker, seen));
        }
        let active_jobs = Arc::new(dashmap::DashMap::new());
        for (worker_id, job_id) in reservations {
            active_jobs.insert((*worker_id).to_string(), (*job_id).to_string());
        }
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        for index in 0..queued {
            sender.try_send(work_item(index)).unwrap();
        }
        let source = CapacitySource {
            workers: registry,
            active_jobs,
            work_queue: Some(Arc::new(sender)),
            model_id,
        };
        (source, receiver)
    }

    fn get_request(uri: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    async fn json_body(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn defaults_are_off_valid_and_derive_the_floor_from_the_worker_floor() {
        let config = PricingConfig::default();
        assert!(!config.enabled);
        config.validate().unwrap();
        assert_eq!(config.worker_pool_bps(), 8_300);
        // ceil(1,000,000 x 2 executions x 10,000 / 8,300)
        assert_eq!(config.base_fee_floor(), 2_409_639);
        assert_eq!(config.fee_params().ceiling_per_mtok, 2_409_639_000);
    }

    #[test]
    fn every_paid_execution_recovers_the_worker_floor() {
        for worker_executions in 1..=2u8 {
            for treasury_bps in [0, 1_000, 3_000] {
                for verifier_bps in [0, 100, 700, 3_000] {
                    for worker_floor_per_mtok in [1, 7, 999, 1_000_000, 123_456_789] {
                        let config = PricingConfig {
                            worker_executions,
                            treasury_bps,
                            verifier_bps,
                            worker_floor_per_mtok,
                            ..PricingConfig::default()
                        };
                        config.validate().unwrap();
                        let share = u128::from(config.base_fee_floor())
                            * u128::from(config.worker_pool_bps())
                            / u128::from(BPS)
                            / u128::from(worker_executions);
                        assert!(
                            share >= u128::from(worker_floor_per_mtok),
                            "{config:?} pays each execution {share}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn each_epoch_moves_the_fee_by_at_most_the_max_step() {
        let params = params();
        for start in [1_000u64, 1_001, 5_000, 77_777, 999_999, 1_000_000] {
            for utilization in (0..=MAX_UTILIZATION_BPS).step_by(125) {
                let next = next_base_fee(start, Some(utilization), &params);
                let bound = start * u64::from(params.max_step_bps) / BPS + 1;
                assert!(
                    next.abs_diff(start) <= bound,
                    "{start} -> {next} at {utilization}"
                );
                assert!((params.floor_per_mtok..=params.ceiling_per_mtok).contains(&next));
                match utilization.cmp(&params.target_utilization_bps) {
                    Ordering::Greater => assert!(next >= start),
                    Ordering::Less => assert!(next <= start),
                    Ordering::Equal => assert_eq!(next, start),
                }
            }
        }
    }

    #[test]
    fn idle_capacity_walks_the_fee_down_to_the_floor_and_holds_it() {
        let params = params();
        let mut fee = params.ceiling_per_mtok;
        let mut epochs = 0;
        while fee > params.floor_per_mtok {
            let next = next_base_fee(fee, Some(0), &params);
            assert!(next < fee);
            fee = next;
            epochs += 1;
            assert!(epochs < 200, "the fee never reached the floor");
        }
        assert_eq!(fee, params.floor_per_mtok);
        for _ in 0..10 {
            fee = next_base_fee(fee, Some(0), &params);
            assert_eq!(fee, params.floor_per_mtok);
        }
    }

    #[test]
    fn sustained_congestion_climbs_to_the_ceiling_and_stops() {
        let params = params();
        let mut fee = params.floor_per_mtok;
        for _ in 0..200 {
            fee = next_base_fee(fee, Some(MAX_UTILIZATION_BPS), &params);
            assert!(fee <= params.ceiling_per_mtok);
        }
        assert_eq!(fee, params.ceiling_per_mtok);
    }

    #[test]
    fn no_verified_capacity_or_on_target_utilization_holds_the_fee() {
        let params = params();
        assert_eq!(next_base_fee(4_321, None, &params), 4_321);
        assert_eq!(next_base_fee(4_321, Some(5_000), &params), 4_321);
        // A stored fee outside the bounds is pulled back inside them.
        assert_eq!(next_base_fee(1, None, &params), params.floor_per_mtok);
        assert_eq!(
            next_base_fee(u64::MAX, None, &params),
            params.ceiling_per_mtok
        );
    }

    #[test]
    fn price_responsive_demand_converges_to_the_target_without_overshoot() {
        let params = FeeParams {
            floor_per_mtok: 1_000_000,
            ceiling_per_mtok: 1_000_000_000,
            target_utilization_bps: 5_000,
            max_step_bps: 1_250,
        };
        // Utilization halves when the fee doubles, so 50% is reached at 4,000,000.
        let demand: u128 = 5_000 * 4_000_000;
        let mut fee = params.floor_per_mtok;
        for _ in 0..200 {
            let utilization = u32::try_from(demand / u128::from(fee)).unwrap();
            let next = next_base_fee(fee, Some(utilization), &params);
            assert!(next >= fee, "the fee approaches equilibrium from below");
            fee = next;
        }
        assert!(fee.abs_diff(4_000_000) <= 40_000, "settled at {fee}");
    }

    #[test]
    fn growing_verified_capacity_lowers_the_fee_to_the_floor() {
        let params = params();
        let busy_worker_equivalents: u64 = 40;
        let utilization_with = |verified_workers: u64| {
            u32::try_from(busy_worker_equivalents * BPS / verified_workers).unwrap()
        };
        let mut fee = params.floor_per_mtok;
        // 50 verified workers: 80% busy, so the fee climbs.
        for _ in 0..20 {
            fee = next_base_fee(fee, Some(utilization_with(50)), &params);
        }
        assert!(fee > params.floor_per_mtok);
        // Verified capacity grows to 200 workers: 20% busy, so the fee falls
        // every epoch until the floor stops it.
        let mut epochs = 0;
        loop {
            let next = next_base_fee(fee, Some(utilization_with(200)), &params);
            epochs += 1;
            assert!(epochs < 100, "the fee never reached the floor");
            if next == params.floor_per_mtok {
                break;
            }
            assert!(next < fee);
            fee = next;
        }
    }

    #[test]
    fn base_fee_matches_python_parity_vector() {
        // scripts/inference_pricing_sim.py prints this vector.
        let params = FeeParams {
            floor_per_mtok: 600_000,
            ceiling_per_mtok: 2_000_000,
            target_utilization_bps: 5_000,
            max_step_bps: 1_250,
        };
        let utilization = [
            Some(0),
            Some(2_500),
            Some(5_000),
            Some(7_500),
            Some(10_000),
            Some(20_000),
            Some(9_000),
            Some(4_999),
            Some(5_001),
            None,
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        ];
        let expected = [
            875_000, 820_313, 820_313, 871_582, 980_529, 1_103_095, 1_213_404, 1_213_374,
            1_213_404, 1_213_404, 1_061_729, 929_013, 812_887, 711_277, 622_368, 600_000,
        ];
        let mut fee = 1_000_000;
        for (step, (observed, want)) in utilization.iter().zip(expected).enumerate() {
            fee = next_base_fee(fee, *observed, &params);
            assert_eq!(fee, want, "step {step}");
        }
    }

    #[test]
    fn utilization_counts_busy_workers_and_queued_jobs_against_verified_capacity() {
        let mut accumulator = EpochAccumulator::default();
        accumulator.record(&snapshot(4, 1, 0));
        accumulator.record(&snapshot(4, 0, 4));
        accumulator.record(&snapshot(0, 0, 9));
        accumulator.record(&snapshot(2, 5, 0));
        assert_eq!(accumulator.samples, 4);
        assert_eq!(accumulator.samples_with_capacity, 3);
        // (3 + 4 + 4 + 0) busy or queued over (4 + 4 + 2) verified.
        assert_eq!(accumulator.utilization_bps(), Some(11_000));
        let mut flooded = EpochAccumulator::default();
        flooded.record(&snapshot(1, 0, 1_000));
        assert_eq!(flooded.utilization_bps(), Some(MAX_UTILIZATION_BPS));
        assert_eq!(EpochAccumulator::default().utilization_bps(), None);
    }

    #[test]
    fn a_busy_epoch_raises_the_fee_once_at_the_boundary() {
        let mut engine = PricingEngine::new(enabled());
        let model = Hash256([9; 32]);
        let floor = engine.params().floor_per_mtok;
        let epoch_secs = engine.config().epoch_secs;
        let start = 1_000 * epoch_secs;
        for second in (0..epoch_secs).step_by(15) {
            assert!(engine.record_sample(model, snapshot(4, 0, 0), start + second));
        }
        assert_eq!(engine.model(model).unwrap().base_fee_per_mtok, floor);
        // Every verified worker busy is twice the 50% target: one full 12.5% step.
        assert!(engine.record_sample(model, snapshot(4, 4, 0), start + epoch_secs));
        let state = engine.model(model).unwrap();
        assert_eq!(state.base_fee_per_mtok, floor + floor * 1_250 / 10_000);
        assert_eq!(state.history.len(), 1);
        assert_eq!(
            state.history[0],
            EpochRecord {
                epoch: 1_000,
                samples: 40,
                utilization_bps: Some(10_000),
                base_fee_before: floor,
                base_fee_after: floor + floor * 1_250 / 10_000,
            }
        );
        // A fully idle epoch walks it back down, never below the floor.
        assert!(engine.record_sample(model, snapshot(4, 4, 0), start + 2 * epoch_secs));
        assert_eq!(engine.model(model).unwrap().base_fee_per_mtok, floor);
    }

    #[test]
    fn epochs_without_verified_capacity_hold_and_skipped_epochs_add_one_record() {
        let mut engine = PricingEngine::new(enabled());
        let model = Hash256([3; 32]);
        let floor = engine.params().floor_per_mtok;
        let epoch_secs = engine.config().epoch_secs;
        assert!(engine.record_sample(model, snapshot(0, 0, 5), 0));
        assert!(engine.record_sample(model, snapshot(0, 0, 5), 10 * epoch_secs));
        let state = engine.model(model).unwrap();
        assert_eq!(state.base_fee_per_mtok, floor);
        assert_eq!(state.epoch, 10);
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].utilization_bps, None);
    }

    #[test]
    fn history_and_model_table_stay_bounded() {
        let mut engine = PricingEngine::new(enabled());
        let epoch_secs = engine.config().epoch_secs;
        let model = Hash256([1; 32]);
        let epochs = u64::try_from(HISTORY_EPOCHS).unwrap() + 20;
        for epoch in 0..epochs {
            engine.record_sample(model, snapshot(2, 1, 0), epoch * epoch_secs);
        }
        assert_eq!(engine.model(model).unwrap().history.len(), HISTORY_EPOCHS);

        let mut engine = PricingEngine::new(enabled());
        let mut accepted = 0;
        for index in 0..(MAX_PRICED_MODELS + 4) {
            let model = Hash256([u8::try_from(index).unwrap(); 32]);
            if engine.record_sample(model, snapshot(1, 1, 0), 0) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, MAX_PRICED_MODELS);
        assert_eq!(engine.models().count(), MAX_PRICED_MODELS);
    }

    #[test]
    fn quote_holds_the_max_and_refunds_what_the_answer_did_not_use() {
        let config = PricingConfig::default();
        let base_fee = config.base_fee_floor();
        let quote = build_quote(&config, base_fee, &request(120, 256)).unwrap();
        assert_eq!(quote.expected_output_tokens, 128);
        assert_eq!(quote.billable_tokens_max, 376);
        assert_eq!(quote.billable_tokens_expected, 248);
        assert_eq!(quote.max, 907);
        assert_eq!(quote.estimate, 598);
        assert_eq!(
            quote.split_at_max,
            Split {
                workers: 2,
                per_worker: 377,
                verifier_pool: 63,
                treasury: 90,
            }
        );
        let mut previous_charge = 0;
        for generated in 0..=256 {
            let (charge, refund) = settle(&config, &quote, generated);
            assert_eq!(charge + refund, quote.max);
            assert!(charge >= previous_charge);
            previous_charge = charge;
        }
        assert_eq!(settle(&config, &quote, 256), (quote.max, 0));
        // Nobody is paid for more than the budget the job was given.
        assert_eq!(settle(&config, &quote, 10_000), (quote.max, 0));
    }

    #[test]
    fn small_jobs_pay_the_minimum_and_prompt_weight_applies() {
        let config = PricingConfig::default();
        assert_eq!(
            billable_tokens(&config, 1, 1),
            u64::from(DEFAULT_MIN_JOB_TOKENS)
        );
        let half_weight = PricingConfig {
            input_weight_bps: 5_000,
            ..PricingConfig::default()
        };
        assert_eq!(billable_tokens(&half_weight, 1_001, 100), 501 + 100);
    }

    #[test]
    fn splits_conserve_every_base_unit() {
        for worker_executions in 1..=2u8 {
            for (treasury_bps, verifier_bps) in [(1_000, 700), (0, 0), (3_000, 3_000), (1_000, 100)]
            {
                let config = PricingConfig {
                    worker_executions,
                    treasury_bps,
                    verifier_bps,
                    ..PricingConfig::default()
                };
                for tokens in [1u64, 64, 999, 4_096, 34_816] {
                    for tip in [0u64, 1, 500_000, 7_000_000_000] {
                        let charge = charge_for(tokens, config.base_fee_floor() + tip);
                        let split = split_charge(&config, charge, tokens, tip);
                        assert_eq!(split.total(), charge);
                        assert_eq!(split.workers, worker_executions);
                        let fair = u128::from(charge) * u128::from(config.worker_pool_bps())
                            / u128::from(BPS)
                            / u128::from(worker_executions);
                        assert!(u128::from(split.per_worker) >= fair);
                    }
                }
            }
        }
    }

    #[test]
    fn quote_rejects_out_of_range_requests_and_saturates_huge_prices() {
        let config = PricingConfig::default();
        let fee = config.base_fee_floor();
        let ok = request(10, 10);
        assert!(build_quote(&config, fee, &ok).is_ok());
        for bad in [
            QuoteRequest {
                max_output_tokens: 0,
                ..ok
            },
            QuoteRequest {
                max_output_tokens: MAX_QUOTE_OUTPUT_TOKENS + 1,
                ..ok
            },
            QuoteRequest {
                prompt_tokens: MAX_QUOTE_PROMPT_TOKENS + 1,
                ..ok
            },
            QuoteRequest {
                expected_output_tokens: Some(11),
                ..ok
            },
        ] {
            assert!(build_quote(&config, fee, &bad).is_err(), "{bad:?}");
        }
        let huge = QuoteRequest {
            prompt_tokens: MAX_QUOTE_PROMPT_TOKENS,
            max_output_tokens: MAX_QUOTE_OUTPUT_TOKENS,
            expected_output_tokens: None,
            tip_per_mtok: u64::MAX,
        };
        let quote = build_quote(&config, u64::MAX, &huge).unwrap();
        assert_eq!(quote.price_per_mtok, u64::MAX);
        assert_eq!(quote.split_at_max.total(), quote.max);
    }

    #[test]
    fn arc_amounts_render_without_trailing_zeros() {
        assert_eq!(format_arc(0), "0");
        assert_eq!(format_arc(1), "0.000000001");
        assert_eq!(format_arc(2_500_000_000), "2.5");
        assert_eq!(format_arc(1_000_000_000), "1");
        assert_eq!(format_arc(2_409_639), "0.002409639");
        let quote = build_quote(&PricingConfig::default(), 2_409_639, &request(120, 256)).unwrap();
        assert_eq!(
            quote_display(&quote),
            "≈ 0.000000598 ARC for this answer (max 0.000000907 ARC)"
        );
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        let valid = PricingConfig::default();
        let invalid = [
            PricingConfig {
                target_utilization_bps: 999,
                ..valid
            },
            PricingConfig {
                target_utilization_bps: 9_001,
                ..valid
            },
            PricingConfig {
                max_step_bps: 0,
                ..valid
            },
            PricingConfig {
                max_step_bps: 5_001,
                ..valid
            },
            PricingConfig {
                epoch_secs: 59,
                ..valid
            },
            PricingConfig {
                sample_interval_secs: 0,
                ..valid
            },
            PricingConfig {
                sample_interval_secs: DEFAULT_EPOCH_SECS,
                ..valid
            },
            PricingConfig {
                worker_floor_per_mtok: 0,
                ..valid
            },
            PricingConfig {
                ceiling_multiple: 0,
                ..valid
            },
            PricingConfig {
                treasury_bps: 3_001,
                ..valid
            },
            PricingConfig {
                verifier_bps: 3_001,
                ..valid
            },
            PricingConfig {
                worker_executions: 0,
                ..valid
            },
            PricingConfig {
                worker_executions: 3,
                ..valid
            },
            PricingConfig {
                input_weight_bps: 0,
                ..valid
            },
            PricingConfig {
                expected_output_bps: 10_001,
                ..valid
            },
            PricingConfig {
                min_job_tokens: 0,
                ..valid
            },
        ];
        for config in invalid {
            assert!(config.validate().is_err(), "{config:?}");
        }
    }

    #[test]
    fn snapshot_counts_only_live_eligible_verified_workers() {
        let model = Hash256([7; 32]);
        let now = Instant::now();
        let stale = now - Duration::from_secs(COMMUNITY_WORKER_TTL_SECS + 30);
        let mut wrong_profile = worker("wrong-profile", model, 5);
        wrong_profile.execution_profile = Some("fp16".to_string());
        let mut not_inference = worker("not-inference", model, 5);
        not_inference.capabilities = vec!["storage".to_string()];
        let (source, _queue) = source_with(
            vec![
                (worker("idle", model, 3), now),
                (worker("assigned", model, 3), now),
                (worker("busy-elsewhere", model, 3), now),
                (worker("new", model, 0), now),
                (worker("stale", model, 3), stale),
                (worker("other-model", Hash256([8; 32]), 3), now),
                (wrong_profile, now),
                (not_inference, now),
            ],
            &[("idle", ""), ("assigned", "job-1"), ("new", "")],
            2,
            Some(model),
        );
        let observed = source.snapshot(now);
        assert_eq!(
            observed,
            CapacitySnapshot {
                verified_workers: 3,
                idle_here: 1,
                assigned_here: 1,
                unverified_workers: 1,
                queued_here: 2,
            }
        );
        let mut accumulator = EpochAccumulator::default();
        accumulator.record(&observed);
        // (2 busy + 2 queued) / 3 verified.
        assert_eq!(accumulator.utilization_bps(), Some(13_333));

        let (no_model, _queue) = source_with(Vec::new(), &[], 0, None);
        assert_eq!(no_model.snapshot(now), CapacitySnapshot::default());
    }

    #[tokio::test]
    async fn routes_serve_status_and_advisory_quotes() {
        let model = Hash256([7; 32]);
        let now = Instant::now();
        let (source, _queue) = source_with(
            vec![(worker("a", model, 1), now), (worker("b", model, 1), now)],
            &[("a", ""), ("b", "")],
            0,
            Some(model),
        );
        let app = router(Arc::new(Mutex::new(PricingEngine::new(enabled()))), source);

        let response = app
            .clone()
            .oneshot(get_request(
                "/inference/quote?prompt_tokens=50&max_tokens=200",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let quote = json_body(response).await;
        assert_eq!(quote["schema"], QUOTE_SCHEMA);
        assert_eq!(quote["advisory"], Value::Bool(true));
        assert_eq!(quote["testnet_units_have_no_value"], Value::Bool(true));
        assert_eq!(quote["serviceable_now"], Value::Bool(true));
        assert_eq!(quote["community_dispatch"]["positions"], 251);
        assert_eq!(quote["max_base_units"], 603);
        assert_eq!(quote["estimate_base_units"], 362);
        assert_eq!(
            quote["display"],
            "≈ 0.000000362 ARC for this answer (max 0.000000603 ARC)"
        );
        assert_eq!(
            quote["split_at_max"]["per_worker"].as_u64().unwrap() * 2
                + quote["split_at_max"]["verifier_pool"].as_u64().unwrap()
                + quote["split_at_max"]["treasury"].as_u64().unwrap(),
            603
        );

        // Priced, but too long for today's community dispatch budget.
        let response = app
            .clone()
            .oneshot(get_request(
                "/inference/quote?prompt_tokens=120&max_tokens=256",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let long = json_body(response).await;
        assert_eq!(long["max_base_units"], 907);
        assert_eq!(long["serviceable_now"], Value::Bool(false));
        assert_eq!(
            long["community_dispatch"]["fits_budget"],
            Value::Bool(false)
        );

        let response = app
            .clone()
            .oneshot(get_request(PRICING_PATH))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let status = json_body(response).await;
        assert_eq!(status["schema"], PRICING_SCHEMA);
        assert_eq!(status["capacity_now"]["verified_workers"], 2);
        assert_eq!(status["models"][0]["base_fee_per_mtok"], 2_409_639);

        for bad in [
            "/inference/quote",
            "/inference/quote?prompt_tokens=1",
            "/inference/quote?prompt_tokens=x&max_tokens=5",
            "/inference/quote?prompt_tokens=1&max_tokens=0",
            "/inference/quote?prompt_tokens=1&max_tokens=5&model_id=nothex",
        ] {
            let response = app.clone().oneshot(get_request(bad)).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
        let other_model = format!(
            "/inference/quote?prompt_tokens=1&max_tokens=5&model_id={}",
            hex_id(Hash256([8; 32]))
        );
        let response = app
            .clone()
            .oneshot(get_request(&other_model))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn mount_is_off_by_default_and_fails_closed_on_bad_config() {
        let node = crate::rpc::build_node_state(
            Arc::new(arc_state::StateDB::new()),
            Arc::new(arc_mempool::Mempool::new(16)),
            Hash256::ZERO,
            None,
            0,
            Instant::now(),
            Arc::new(AtomicU32::new(0)),
            None,
            None,
            None,
            None,
        );
        assert!(mount(&node, PricingConfig::default(), None).is_none());
        let invalid = PricingConfig {
            epoch_secs: 1,
            ..enabled()
        };
        assert!(mount(&node, invalid, None).is_none());

        let mounted = mount(&node, enabled(), None).expect("enabled pricing mounts");
        // No model is loaded, so this coordinator prices nothing and says so.
        let response = mounted
            .router
            .clone()
            .oneshot(get_request("/inference/quote?prompt_tokens=1&max_tokens=1"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let response = mounted
            .router
            .oneshot(get_request(PRICING_PATH))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["served_model_id"], Value::Null);
        // Without a model the sampler has nothing to sample and returns.
        tokio::time::timeout(Duration::from_secs(5), mounted.sampler.run())
            .await
            .expect("the sampler returns at once without a model");
    }

    #[tokio::test]
    async fn sampler_records_samples_and_stops_on_shutdown() {
        let model = Hash256([7; 32]);
        let (source, _queue) = source_with(
            vec![(worker("a", model, 1), Instant::now())],
            &[("a", "job-9")],
            0,
            Some(model),
        );
        let engine = Arc::new(Mutex::new(PricingEngine::new(enabled())));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let sampler = PricingSampler {
            engine: engine.clone(),
            source,
            interval: Duration::from_millis(10),
            shutdown: Some(shutdown_rx),
        };
        let task = tokio::spawn(sampler.run());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let samples = engine
                    .lock()
                    .model(model)
                    .map_or(0, |state| state.accumulator.samples);
                if samples >= 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the sampler records samples");
        let last = engine
            .lock()
            .model(model)
            .and_then(|state| state.last_snapshot);
        assert_eq!(
            last,
            Some(CapacitySnapshot {
                verified_workers: 1,
                assigned_here: 1,
                ..CapacitySnapshot::default()
            })
        );
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the sampler stops on shutdown")
            .unwrap();
    }
}
