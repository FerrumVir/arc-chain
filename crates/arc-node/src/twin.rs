//! Twin execution v0: pure building blocks.
//!
//! A twin-executed community job is dispatched to two community workers with
//! independent identities. The coordinator compares their output commitments
//! and asks the validators to recompute only when the twins disagree, when a
//! job is selected for a validator spot check, when a reward needs the
//! coordinator's own recomputation, or when one twin never answers.
//!
//! This module holds the parts that do not touch the network: worker
//! independence, claim-time pairing, output comparison, the rolling
//! checkpoint chain, region tags from measured round-trip times, public demo
//! prompts and their screening, rate limits, statistics and the receipt
//! commitment. Orchestration lives in `rpc::twin_dispatch`.
//!
//! Nothing here is consensus input. Twin execution is a per-coordinator,
//! off-chain policy that stays disabled unless the operator enables it. See
//! `docs/twin-execution.md`.

use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Schema of the receipt recorded for every twin-executed job.
pub const TWIN_RECEIPT_SCHEMA: &str = "arc.community.twin-receipt.v1";
/// Schema of `GET /community/twin_stats`.
pub const TWIN_STATS_SCHEMA: &str = "arc.community.twin-stats.v1";
/// Signed worker report of round-trip times to the validator origins.
pub const COMMUNITY_REGION_PATH: &str = "/community/region";
/// What a region tag is derived from. It is self-reported by the worker, so
/// it may steer pairing and coarse display, never payment or penalties.
pub const REGION_TAG_BASIS: &str = "worker_reported_rtt_to_validators";
/// The only comparison basis v0 workers support: the BLAKE3 commitment to
/// the complete generated token sequence, plus its token count and text.
pub const COMPARISON_BASIS_FINAL_OUTPUT: &str = "final_output_hash";
/// Tokens per rolling checkpoint once workers stream checkpoints
/// (research-7 section 3.6 compares running hashes every 32 tokens).
pub const CHECKPOINT_INTERVAL_TOKENS: usize = 32;
/// Research-7 section 1.7: p80 diameter of an L2 latency region.
pub const REGION_DIAMETER_MS: u32 = 60;
/// Research-7 section 1.7: p80 diameter of an L1 continent.
pub const CONTINENT_DIAMETER_MS: u32 = 150;
/// Largest round-trip time a region report may carry.
pub const MAX_REPORTED_RTT_MS: u32 = 60_000;
/// Largest number of samples in one region report.
pub const MAX_RTT_SAMPLES: usize = 16;
/// Default share of twin-matched jobs (without a reward) that validators
/// still recompute, in parts per thousand.
pub const DEFAULT_SPOT_CHECK_PER_MILLE: u16 = 50;
/// Default seconds between demand ticks on one coordinator.
pub const DEFAULT_DEMAND_INTERVAL_SECS: u64 = 120;
/// Lower bound for the demand tick, so no configuration floods workers.
pub const MIN_DEMAND_INTERVAL_SECS: u64 = 30;
/// Default share of demand ticks spent on re-executing a verified prompt.
pub const DEFAULT_REPLAY_SHARE_PER_MILLE: u16 = 250;
/// Output budget of coordinator-generated demo and replay jobs.
pub const PUMP_MAX_TOKENS: u32 = 32;
/// Output budget of caller requests marked `public_demo`.
pub const PUBLIC_DEMO_MAX_TOKENS: u32 = 64;
/// Longest prompt the public demo accepts.
pub const PUBLIC_DEMO_PROMPT_MAX_BYTES: usize = 1_000;
/// How long a queued twin leg may wait for a more diverse or less recently
/// served idle worker before any independent worker may take it.
pub const CLAIM_PREFERENCE_WINDOW_MS: u64 = 3_000;
/// A worker counts as "less recently served" only when it has waited at
/// least this much longer than the claimer.
pub const FAIRNESS_MARGIN_MS: u64 = 60_000;
/// Shown with every public demo answer.
pub const PUBLIC_DEMO_NOTICE: &str = "Public ARC testnet demo. Your prompt is sent to independent community computers that can read it and is not private. Do not enter personal data. Testnet output only; nothing here has monetary value.";
/// Shown with every twin receipt so it can never be read as more than it is.
pub const CENTRALIZATION_DISCLOSURE: &str = "The coordinator that dispatched this job and the validators that recompute results are operated by the ARC team (six validators, one hosting provider). The two workers are community nodes with distinct node keys; operator identity and network independence are not yet verified.";

const CHECKPOINT_DOMAIN: &str = "ARC community twin rolling checkpoint v1";
const RECEIPT_COMMITMENT_DOMAIN: &str = "ARC community twin receipt v1";
const BUCKET_UNITS_PER_TOKEN: u64 = 3_600_000;

/// Display labels for the six genesis validators. The addresses are the
/// public `/network/info` identities of the community RPC origins listed in
/// `install.sh`. Any other validator is labelled by its address prefix.
const KNOWN_VALIDATOR_REGIONS: [(&str, &str, &str); 6] = [
    (
        "adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f",
        "us-east",
        "north-america",
    ),
    (
        "44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780",
        "us-west",
        "north-america",
    ),
    (
        "5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21",
        "eu-west",
        "europe",
    ),
    (
        "228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd",
        "eu-west",
        "europe",
    ),
    (
        "f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f",
        "asia-east",
        "asia-pacific",
    ),
    (
        "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b",
        "asia-southeast",
        "asia-pacific",
    ),
];

/// Fixed, harmless public prompts used by coordinator-generated demand.
/// They are published in receipts verbatim. Caller prompts never are.
pub const PUBLIC_DEMO_PROMPTS: [&str; 32] = [
    "Name three primary colors.",
    "Write one sentence about the ocean.",
    "What is the capital of France?",
    "List four planets in our solar system.",
    "Give a synonym for the word happy.",
    "Translate good morning into Spanish.",
    "What do bees make?",
    "Name two instruments in an orchestra.",
    "Describe a sunrise in one sentence.",
    "What is twelve plus thirty?",
    "Name three fruits that are red.",
    "Write a short haiku about rain.",
    "What season comes after winter?",
    "Give one tip for staying hydrated.",
    "Name a mammal that lives in the sea.",
    "What shape has three sides?",
    "Suggest a name for a friendly robot.",
    "What is the opposite of cold?",
    "Name two things you can find in a library.",
    "Write one sentence about mountains.",
    "What color do you get by mixing blue and yellow?",
    "Name three vegetables.",
    "How many days are in a week?",
    "Give a short greeting for a new neighbor.",
    "Name a bird that cannot fly.",
    "What do plants need to grow?",
    "Write one sentence about the moon.",
    "Name two sports played with a ball.",
    "What is the boiling point of water in Celsius?",
    "Name three things that are made of wood.",
    "Describe a cat in five words.",
    "What is a group of wolves called?",
];

/// Operator configuration. Every switch defaults to off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TwinConfig {
    /// Dispatch each community job to two independent workers.
    pub twin_execution: bool,
    /// Generate rate-limited public demo and replay jobs for idle workers.
    pub demand_pump: bool,
    /// Plan and log every demand tick without dispatching anything, so an
    /// operator can review what the pump would do before enabling it.
    pub demand_dry_run: bool,
    /// Share of twin-matched jobs without a reward that validators still
    /// recompute, in parts per thousand.
    pub spot_check_per_mille: u16,
    /// Seconds between demand ticks on this coordinator.
    pub demand_interval_secs: u64,
}

impl Default for TwinConfig {
    fn default() -> Self {
        Self {
            twin_execution: false,
            demand_pump: false,
            demand_dry_run: false,
            spot_check_per_mille: DEFAULT_SPOT_CHECK_PER_MILLE,
            demand_interval_secs: DEFAULT_DEMAND_INTERVAL_SECS,
        }
    }
}

impl TwinConfig {
    /// Clamp operator input to the reviewed bounds.
    pub fn normalized(self) -> Self {
        Self {
            spot_check_per_mille: self.spot_check_per_mille.min(1_000),
            demand_interval_secs: self.demand_interval_secs.max(MIN_DEMAND_INTERVAL_SECS),
            ..self
        }
    }
}

/// How precisely a region tag places a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegionClass {
    /// Nearest validator within the L2 region diameter.
    Region,
    /// Nearest validator within the L1 continent diameter.
    Continent,
    /// Farther than a continent from every validator.
    Distant,
}

/// Coarse, measured-latency location label of a community worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegionTag {
    pub region: String,
    pub continent: String,
    pub class: RegionClass,
    pub basis: &'static str,
    pub measured_at_unix_ms: u64,
    /// Used for classification only and never published: a precise
    /// round-trip time narrows a home location more than a region label.
    #[serde(skip)]
    pub nearest_rtt_ms: u32,
}

impl RegionTag {
    /// The key twins should differ in: the region when the worker is within
    /// a region diameter of a validator, otherwise its continent.
    pub fn locality(&self) -> String {
        match self.class {
            RegionClass::Region => format!("region:{}", self.region),
            RegionClass::Continent | RegionClass::Distant => {
                format!("continent:{}", self.continent)
            }
        }
    }
}

/// One measured round trip from a worker to a validator origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RttSample {
    pub validator: Hash256,
    pub rtt_ms: u32,
}

/// Wire form of one sample in a signed region report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityRttSample {
    /// 0x-prefixed validator address from the origin's `/network/info`.
    pub validator: String,
    pub rtt_ms: u32,
}

/// Signed worker report of round-trip times to the validator origins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityRegionReport {
    pub worker_id: String,
    pub samples: Vec<CommunityRttSample>,
}

impl CommunityRegionReport {
    /// Check bounds and parse every sample. Duplicate validators keep the
    /// smallest round trip.
    pub fn parsed_samples(&self) -> Result<Vec<RttSample>, String> {
        if self.samples.is_empty() || self.samples.len() > MAX_RTT_SAMPLES {
            return Err(format!(
                "region report must carry 1-{MAX_RTT_SAMPLES} samples"
            ));
        }
        let mut best: BTreeMap<[u8; 32], u32> = BTreeMap::new();
        for sample in &self.samples {
            if sample.rtt_ms > MAX_REPORTED_RTT_MS {
                return Err(format!(
                    "region report rtt_ms must be at most {MAX_REPORTED_RTT_MS}"
                ));
            }
            let bare = sample
                .validator
                .strip_prefix("0x")
                .unwrap_or(&sample.validator);
            let validator = Hash256::from_hex(bare)
                .map_err(|_| "region report validator must be a 32-byte hex address".to_string())?;
            best.entry(validator.0)
                .and_modify(|rtt| *rtt = (*rtt).min(sample.rtt_ms))
                .or_insert(sample.rtt_ms);
        }
        Ok(best
            .into_iter()
            .map(|(validator, rtt_ms)| RttSample {
                validator: Hash256(validator),
                rtt_ms,
            })
            .collect())
    }
}

/// Display region and continent of a validator address.
pub fn validator_region(validator: &Hash256) -> (String, String) {
    let hex = validator.to_hex();
    for (address, region, continent) in KNOWN_VALIDATOR_REGIONS {
        if address == hex {
            return (region.to_string(), continent.to_string());
        }
    }
    (format!("validator-{}", &hex[..8]), "unknown".to_string())
}

/// Classify a worker by its nearest validator, using research-7's region
/// (60 ms) and continent (150 ms) diameters.
pub fn classify_region(samples: &[RttSample], measured_at_unix_ms: u64) -> Option<RegionTag> {
    let nearest = samples
        .iter()
        .filter(|sample| sample.rtt_ms <= MAX_REPORTED_RTT_MS)
        .min_by(|left, right| {
            left.rtt_ms
                .cmp(&right.rtt_ms)
                .then_with(|| left.validator.0.cmp(&right.validator.0))
        })?;
    let (region, continent) = validator_region(&nearest.validator);
    let class = if nearest.rtt_ms <= REGION_DIAMETER_MS {
        RegionClass::Region
    } else if nearest.rtt_ms <= CONTINENT_DIAMETER_MS {
        RegionClass::Continent
    } else {
        RegionClass::Distant
    };
    Some(RegionTag {
        region,
        continent,
        class,
        basis: REGION_TAG_BASIS,
        measured_at_unix_ms,
        nearest_rtt_ms: nearest.rtt_ms,
    })
}

/// What the coordinator knows about a worker when pairing twins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerFacts {
    pub worker_id: String,
    /// Declared operator or family (Tor-style). Not collected yet.
    pub operator: Option<String>,
    /// Salted client network prefix (/24 IPv4, /48 IPv6). Unavailable until
    /// the gateway forwards a network group to the node.
    pub network_group: Option<String>,
    pub region: Option<RegionTag>,
    /// Operating system and architecture from registration.
    pub platform: Option<String>,
    pub last_served_unix_ms: Option<u64>,
}

/// Why two workers may or may not twin each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Independence {
    Independent,
    SameWorker,
    SameOperator,
    SameNetwork,
}

/// Hard independence rule: distinct node keys, and distinct operator and
/// network group whenever both sides are known.
pub fn independence(left: &WorkerFacts, right: &WorkerFacts) -> Independence {
    if left.worker_id == right.worker_id {
        return Independence::SameWorker;
    }
    if let (Some(a), Some(b)) = (&left.operator, &right.operator)
        && a == b
    {
        return Independence::SameOperator;
    }
    if let (Some(a), Some(b)) = (&left.network_group, &right.network_group)
        && a == b
    {
        return Independence::SameNetwork;
    }
    Independence::Independent
}

/// Outcome of a worker asking for a twin leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimDecision {
    Accept,
    /// The claimer breaks a hard independence rule; the leg stays queued.
    Refuse(&'static str),
    /// A better-placed idle worker exists; the leg stays queued briefly.
    Defer(&'static str),
}

pub const REASON_SAME_WORKER: &str = "twin_same_worker";
pub const REASON_SAME_OPERATOR: &str = "twin_same_operator";
pub const REASON_SAME_NETWORK: &str = "twin_same_network";
pub const REASON_PREFER_DIVERSE: &str = "twin_prefers_more_diverse_worker";
pub const REASON_PREFER_LEAST_SERVED: &str = "prefer_least_recently_served";

/// Inputs to one claim decision.
pub struct ClaimContext<'a> {
    pub claimer: &'a WorkerFacts,
    /// Workers holding another leg of the group, or that produced the replay
    /// reference. The claimer must be independent of every one.
    pub related: &'a [WorkerFacts],
    /// Other idle workers currently long-polling this coordinator.
    pub idle_alternatives: &'a [WorkerFacts],
    pub waited_ms: u64,
    pub preference_window_ms: u64,
    /// Coordinator-generated demand spreads work across idle workers.
    pub prefer_least_recently_served: bool,
}

/// Points for differing from every related worker in locality and platform.
/// Unknown values never earn a point.
pub fn diversity_score(candidate: &WorkerFacts, related: &[WorkerFacts]) -> u8 {
    if related.is_empty() {
        return 0;
    }
    let mut score = 0;
    if let Some(locality) = candidate.region.as_ref().map(RegionTag::locality)
        && related.iter().all(|other| {
            other
                .region
                .as_ref()
                .is_some_and(|region| region.locality() != locality)
        })
    {
        score += 1;
    }
    if let Some(platform) = candidate.platform.as_deref()
        && related
            .iter()
            .all(|other| other.platform.as_deref().is_some_and(|p| p != platform))
    {
        score += 1;
    }
    score
}

fn independent_of_all(candidate: &WorkerFacts, related: &[WorkerFacts]) -> bool {
    related
        .iter()
        .all(|other| independence(candidate, other) == Independence::Independent)
}

/// Decide whether `claimer` may take a twin leg now.
///
/// Hard rule first: the claimer must be independent of every related
/// worker. Inside the preference window, a leg waits for an idle worker that
/// differs from the related workers in more of {region, platform}; for
/// coordinator-generated demand it also waits for a worker that has gone
/// unserved for longer. After the window any independent worker may take it.
pub fn decide_claim(ctx: &ClaimContext<'_>) -> ClaimDecision {
    for other in ctx.related {
        match independence(ctx.claimer, other) {
            Independence::Independent => {}
            Independence::SameWorker => return ClaimDecision::Refuse(REASON_SAME_WORKER),
            Independence::SameOperator => return ClaimDecision::Refuse(REASON_SAME_OPERATOR),
            Independence::SameNetwork => return ClaimDecision::Refuse(REASON_SAME_NETWORK),
        }
    }
    if ctx.waited_ms >= ctx.preference_window_ms {
        return ClaimDecision::Accept;
    }
    let claimer_score = diversity_score(ctx.claimer, ctx.related);
    let claimer_served = ctx.claimer.last_served_unix_ms.unwrap_or(0);
    let mut hungrier_alternative = false;
    for alternative in ctx.idle_alternatives {
        if alternative.worker_id == ctx.claimer.worker_id
            || !independent_of_all(alternative, ctx.related)
        {
            continue;
        }
        let score = diversity_score(alternative, ctx.related);
        if score > claimer_score {
            return ClaimDecision::Defer(REASON_PREFER_DIVERSE);
        }
        if ctx.prefer_least_recently_served
            && score == claimer_score
            && alternative
                .last_served_unix_ms
                .unwrap_or(0)
                .saturating_add(FAIRNESS_MARGIN_MS)
                < claimer_served
        {
            hungrier_alternative = true;
        }
    }
    if hungrier_alternative {
        ClaimDecision::Defer(REASON_PREFER_LEAST_SERVED)
    } else {
        ClaimDecision::Accept
    }
}

/// Where a twin-executed job came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DemandSource {
    /// A caller's `/inference/run` request.
    PublicRequest,
    /// A caller's `/inference/run` request marked `public_demo`.
    PublicDemo,
    /// A coordinator-generated job from [`PUBLIC_DEMO_PROMPTS`].
    PumpDemo,
    /// A coordinator-generated re-execution of a verified public prompt.
    PumpReplay,
}

impl DemandSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PublicRequest => "public_request",
            Self::PublicDemo => "public_demo",
            Self::PumpDemo => "pump_demo",
            Self::PumpReplay => "pump_replay",
        }
    }

    pub fn is_pump(&self) -> bool {
        matches!(self, Self::PumpDemo | Self::PumpReplay)
    }

    /// Only caller demand may draw on the protocol-capped promotional
    /// reward budget. Demand the coordinator generates itself never does.
    pub fn reward_eligible(&self) -> bool {
        matches!(self, Self::PublicRequest | Self::PublicDemo)
    }
}

/// The commitments of one successful twin leg that are compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegOutput {
    /// BLAKE3 of the generated token ids (little-endian u32).
    pub output_hash: Hash256,
    pub tokens_generated: u64,
    /// BLAKE3 of the decoded output text, so a worker that reports the right
    /// token hash with different text is caught as well.
    pub text_digest: Hash256,
}

/// Fields in which two leg outputs differ. Empty means a byte-exact match.
pub fn mismatch_fields(left: &LegOutput, right: &LegOutput) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if left.output_hash != right.output_hash {
        fields.push("output_hash");
    }
    if left.tokens_generated != right.tokens_generated {
        fields.push("tokens_generated");
    }
    if left.text_digest != right.text_digest {
        fields.push("output_text");
    }
    fields
}

/// Result of comparing a group's legs with each other or with a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonResult {
    Match,
    Mismatch,
    Incomplete,
    ReferenceMatch,
    ReferenceMismatch,
}

impl ComparisonResult {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Mismatch => "mismatch",
            Self::Incomplete => "incomplete",
            Self::ReferenceMatch => "reference_match",
            Self::ReferenceMismatch => "reference_mismatch",
        }
    }

    pub fn agrees(&self) -> bool {
        matches!(self, Self::Match | Self::ReferenceMatch)
    }

    pub fn disagrees(&self) -> bool {
        matches!(self, Self::Mismatch | Self::ReferenceMismatch)
    }
}

/// Compare a group. A replay group (one leg) compares against the verified
/// reference; a twin group compares its two legs.
pub fn compare_group(
    legs: &[Option<LegOutput>],
    reference: Option<&LegOutput>,
) -> (ComparisonResult, Vec<&'static str>) {
    let submitted: Vec<&LegOutput> = legs.iter().flatten().collect();
    if let Some(reference) = reference {
        return match submitted.first() {
            Some(output) => {
                let fields = mismatch_fields(output, reference);
                if fields.is_empty() {
                    (ComparisonResult::ReferenceMatch, fields)
                } else {
                    (ComparisonResult::ReferenceMismatch, fields)
                }
            }
            None => (ComparisonResult::Incomplete, Vec::new()),
        };
    }
    match submitted.as_slice() {
        [left, right] => {
            let fields = mismatch_fields(left, right);
            if fields.is_empty() {
                (ComparisonResult::Match, fields)
            } else {
                (ComparisonResult::Mismatch, fields)
            }
        }
        _ => (ComparisonResult::Incomplete, Vec::new()),
    }
}

/// Why validators recompute a twin group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecomputeReason {
    SpotCheck,
    Mismatch,
    Fallback,
    Reward,
}

/// Decide whether validators recompute. Disagreement always does. Agreement
/// does only for a reward (a validator approval asserts its own
/// recomputation) or a spot check. An incomplete caller job falls back to
/// today's single-worker verification; incomplete generated demand is
/// dropped without spending validator capacity.
pub fn recompute_reason(
    comparison: ComparisonResult,
    source: DemandSource,
    any_output: bool,
    reward_candidate: bool,
    spot_check_selected: bool,
) -> Option<RecomputeReason> {
    match comparison {
        ComparisonResult::Mismatch | ComparisonResult::ReferenceMismatch => {
            Some(RecomputeReason::Mismatch)
        }
        ComparisonResult::Match | ComparisonResult::ReferenceMatch => {
            if reward_candidate {
                Some(RecomputeReason::Reward)
            } else if spot_check_selected {
                Some(RecomputeReason::SpotCheck)
            } else {
                None
            }
        }
        ComparisonResult::Incomplete => {
            (any_output && !source.is_pump()).then_some(RecomputeReason::Fallback)
        }
    }
}

/// Deterministic, coordinator-secret spot-check selection. Workers cannot
/// predict which matched groups validators will recompute.
pub fn spot_check_selected(secret: &[u8; 32], group_id: &str, per_mille: u16) -> bool {
    if per_mille == 0 {
        return false;
    }
    if per_mille >= 1_000 {
        return true;
    }
    let mut hasher = blake3::Hasher::new_keyed(secret);
    hasher.update(group_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(bytes) % 1_000 < u64::from(per_mille)
}

/// What validators reported, when they recomputed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecomputeStatus {
    NotSelected,
    Confirmed,
    Contradicted,
    Unavailable,
    SkippedBusy,
}

impl RecomputeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotSelected => "not_selected",
            Self::Confirmed => "confirmed",
            Self::Contradicted => "contradicted",
            Self::Unavailable => "unavailable",
            Self::SkippedBusy => "skipped_busy",
        }
    }
}

/// Final verdict of a twin group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Verified,
    Rejected,
    Unverified,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Rejected => "rejected",
            Self::Unverified => "unverified",
        }
    }
}

pub const VERIFIED_BY_TWIN: &str = "twin_match";
pub const VERIFIED_BY_VALIDATORS: &str = "validator_recompute";
pub const VERIFIED_BY_REFERENCE: &str = "reference_hash";

/// Per-leg validity, verdict and the leg whose output is served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub leg_valid: Vec<Option<bool>>,
    pub verdict: Verdict,
    pub verified_by: Vec<&'static str>,
    pub chosen_leg: Option<usize>,
    pub recompute_status: RecomputeStatus,
    /// Whether the replay reference agrees with the validators' output.
    pub reference_valid: Option<bool>,
}

/// Classify a group from its comparison and, when validators recomputed,
/// their canonical output (`Ok`) or why they did not (`Err`).
pub fn classify(
    legs: &[Option<LegOutput>],
    comparison: ComparisonResult,
    reference: Option<&LegOutput>,
    recompute: Option<Result<&LegOutput, RecomputeStatus>>,
) -> Classification {
    match recompute {
        Some(Ok(canonical)) => {
            let leg_valid: Vec<Option<bool>> = legs
                .iter()
                .map(|leg| {
                    leg.as_ref()
                        .map(|output| mismatch_fields(output, canonical).is_empty())
                })
                .collect();
            let chosen_leg = leg_valid.iter().position(|valid| *valid == Some(true));
            let every_submitted_valid = leg_valid.iter().flatten().all(|valid| *valid);
            let recompute_status = if chosen_leg.is_some() && every_submitted_valid {
                RecomputeStatus::Confirmed
            } else if comparison.agrees() || chosen_leg.is_none() {
                RecomputeStatus::Contradicted
            } else {
                RecomputeStatus::Confirmed
            };
            let reference_valid =
                reference.map(|reference| mismatch_fields(reference, canonical).is_empty());
            let mut verified_by = Vec::new();
            if chosen_leg.is_some() {
                if comparison == ComparisonResult::Match && every_submitted_valid {
                    verified_by.push(VERIFIED_BY_TWIN);
                }
                if comparison == ComparisonResult::ReferenceMatch && every_submitted_valid {
                    verified_by.push(VERIFIED_BY_REFERENCE);
                }
                verified_by.push(VERIFIED_BY_VALIDATORS);
            }
            Classification {
                leg_valid,
                verdict: if chosen_leg.is_some() {
                    Verdict::Verified
                } else {
                    Verdict::Rejected
                },
                verified_by,
                chosen_leg,
                recompute_status,
                reference_valid,
            }
        }
        other => {
            let recompute_status = match other {
                Some(Err(status)) => status,
                _ => RecomputeStatus::NotSelected,
            };
            if comparison.agrees() {
                let leg_valid: Vec<Option<bool>> =
                    legs.iter().map(|leg| leg.as_ref().map(|_| true)).collect();
                let chosen_leg = leg_valid.iter().position(|valid| *valid == Some(true));
                let marker = if comparison == ComparisonResult::Match {
                    VERIFIED_BY_TWIN
                } else {
                    VERIFIED_BY_REFERENCE
                };
                Classification {
                    leg_valid,
                    verdict: Verdict::Verified,
                    verified_by: vec![marker],
                    chosen_leg,
                    recompute_status,
                    reference_valid: reference.map(|_| true),
                }
            } else {
                Classification {
                    leg_valid: vec![None; legs.len()],
                    verdict: Verdict::Unverified,
                    verified_by: Vec::new(),
                    chosen_leg: None,
                    recompute_status,
                    reference_valid: None,
                }
            }
        }
    }
}

/// Rolling checkpoint chain over a generated token sequence.
///
/// `h_0` is the job commitment; `h_{k+1} = BLAKE3-derive-key(domain,
/// h_k || chunk_len_le64 || tokens_le32)` for each chunk of `interval`
/// tokens (the last chunk may be shorter). Workers that stream will post
/// `h_k` every `interval` tokens so twins can be compared while generating.
pub fn checkpoint_chain(job_commitment: &Hash256, tokens: &[u32], interval: usize) -> Vec<Hash256> {
    let interval = interval.max(1);
    let mut previous = *job_commitment;
    let mut chain = Vec::with_capacity(tokens.len().div_ceil(interval));
    for chunk in tokens.chunks(interval) {
        let mut hasher = blake3::Hasher::new_derive_key(CHECKPOINT_DOMAIN);
        hasher.update(previous.as_ref());
        hasher.update(&(chunk.len() as u64).to_le_bytes());
        for token in chunk {
            hasher.update(&token.to_le_bytes());
        }
        previous = Hash256(*hasher.finalize().as_bytes());
        chain.push(previous);
    }
    chain
}

/// Index of the first checkpoint where two chains disagree, if any.
pub fn first_divergent_checkpoint(left: &[Hash256], right: &[Hash256]) -> Option<usize> {
    if let Some(index) = left.iter().zip(right.iter()).position(|(a, b)| a != b) {
        return Some(index);
    }
    (left.len() != right.len()).then_some(left.len().min(right.len()))
}

/// Token bucket in exact integer units (no rounding loss between refills).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBucket {
    capacity: u32,
    per_hour: u32,
    available_units: u64,
    updated_unix_ms: u64,
}

impl TokenBucket {
    /// A full bucket of `capacity` that refills `per_hour` tokens an hour.
    pub fn new(capacity: u32, per_hour: u32, now_unix_ms: u64) -> Self {
        Self {
            capacity,
            per_hour,
            available_units: u64::from(capacity).saturating_mul(BUCKET_UNITS_PER_TOKEN),
            updated_unix_ms: now_unix_ms,
        }
    }

    fn refill(&mut self, now_unix_ms: u64) {
        let elapsed = now_unix_ms.saturating_sub(self.updated_unix_ms);
        self.updated_unix_ms = self.updated_unix_ms.max(now_unix_ms);
        let cap = u64::from(self.capacity).saturating_mul(BUCKET_UNITS_PER_TOKEN);
        self.available_units = self
            .available_units
            .saturating_add(elapsed.saturating_mul(u64::from(self.per_hour)))
            .min(cap);
    }

    /// Take one token if available.
    pub fn try_take(&mut self, now_unix_ms: u64) -> bool {
        self.refill(now_unix_ms);
        if self.available_units >= BUCKET_UNITS_PER_TOKEN {
            self.available_units -= BUCKET_UNITS_PER_TOKEN;
            true
        } else {
            false
        }
    }
}

/// Which kind of job a demand tick should create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandKind {
    TwinDemo,
    Replay,
}

/// Plan one demand tick from the idle workers and verified references.
pub fn plan_demand(
    idle_workers: usize,
    twin_enabled: bool,
    reference_available: bool,
    replay_roll_per_mille: u16,
    replay_share_per_mille: u16,
) -> Result<DemandKind, &'static str> {
    if idle_workers == 0 {
        return Err("no idle eligible worker is polling this coordinator");
    }
    let replay_wanted = replay_roll_per_mille < replay_share_per_mille;
    if reference_available && (idle_workers < 2 || !twin_enabled || replay_wanted) {
        return Ok(DemandKind::Replay);
    }
    if twin_enabled && idle_workers >= 2 {
        return Ok(DemandKind::TwinDemo);
    }
    Err("twin demo jobs need two idle workers and twin execution enabled")
}

/// The fixed public prompt for a demand counter.
pub fn public_demo_prompt(counter: u64) -> (usize, &'static str) {
    let slot = (counter % PUBLIC_DEMO_PROMPTS.len() as u64) as usize;
    (slot, PUBLIC_DEMO_PROMPTS[slot])
}

/// Reject public demo prompts that look like they carry private data. This
/// is a best-effort screen; the notice is what tells people not to.
pub fn public_demo_prompt_rejection(prompt: &str) -> Option<&'static str> {
    if prompt.trim().is_empty() {
        return Some("public demo prompt is empty");
    }
    if prompt.len() > PUBLIC_DEMO_PROMPT_MAX_BYTES {
        return Some("public demo prompt exceeds 1,000 bytes");
    }
    if contains_email_address(prompt) {
        return Some("public demo prompts must not contain email addresses");
    }
    if longest_number_run(prompt) >= 9 {
        return Some(
            "public demo prompts must not contain long numbers such as phone, account or ID numbers",
        );
    }
    if contains_secret_like_token(prompt) {
        return Some("public demo prompts must not contain keys, tokens or other secrets");
    }
    None
}

fn contains_email_address(text: &str) -> bool {
    text.split_whitespace().any(|word| {
        let word = word.trim_matches(|c: char| c.is_ascii_punctuation() && c != '@');
        match word.split_once('@') {
            Some((local, domain)) => {
                !local.is_empty()
                    && !domain.contains('@')
                    && domain.contains('.')
                    && domain.split('.').all(|label| !label.is_empty())
            }
            None => false,
        }
    })
}

fn longest_number_run(text: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for character in text.chars() {
        if character.is_ascii_digit() {
            current += 1;
            longest = longest.max(current);
        } else if !matches!(character, ' ' | '-' | '.' | '(' | ')' | '+' | '/') {
            current = 0;
        }
    }
    longest
}

fn contains_secret_like_token(text: &str) -> bool {
    text.split_whitespace().any(|word| {
        let word = word.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | ',' | '.' | ';' | ':' | '(' | ')' | '[' | ']'
            )
        });
        word.len() >= 32
            && word.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'_' | b'-')
            })
            && word.bytes().any(|byte| byte.is_ascii_digit())
            && word.bytes().any(|byte| byte.is_ascii_alphabetic())
    })
}

/// A verified answer to a fixed public prompt, used to re-execute it later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReference {
    pub prompt_index: usize,
    /// The exact input the workers ran (the prompt after the chat template).
    pub input: String,
    pub max_tokens: u32,
    pub model_id: Hash256,
    pub output: LegOutput,
    pub source_group_id: String,
    pub workers: Vec<String>,
    pub recorded_at_unix_ms: u64,
}

/// The latest verified answer per fixed public prompt.
#[derive(Debug, Default)]
pub struct ReferenceBook {
    by_prompt: BTreeMap<usize, ReplayReference>,
}

impl ReferenceBook {
    pub fn record(&mut self, reference: ReplayReference) {
        self.by_prompt.insert(reference.prompt_index, reference);
    }

    /// Remove a reference that validators contradicted.
    pub fn evict(&mut self, prompt_index: usize, source_group_id: &str) {
        if self
            .by_prompt
            .get(&prompt_index)
            .is_some_and(|reference| reference.source_group_id == source_group_id)
        {
            self.by_prompt.remove(&prompt_index);
        }
    }

    /// Pick a reference for this model with a caller-supplied selector.
    pub fn pick(&self, selector: u64, model_id: &Hash256) -> Option<&ReplayReference> {
        let candidates: Vec<&ReplayReference> = self
            .by_prompt
            .values()
            .filter(|reference| reference.model_id == *model_id)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let index = (selector % candidates.len() as u64) as usize;
        candidates.get(index).copied()
    }

    pub fn has_model(&self, model_id: &Hash256) -> bool {
        self.by_prompt
            .values()
            .any(|reference| reference.model_id == *model_id)
    }

    pub fn len(&self) -> usize {
        self.by_prompt.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_prompt.is_empty()
    }
}

/// Verified jobs and tokens over the trailing hour.
#[derive(Debug, Default)]
pub struct ThroughputWindow {
    events: VecDeque<(u64, u64)>,
}

impl ThroughputWindow {
    pub const WINDOW_MS: u64 = 3_600_000;
    const MAX_EVENTS: usize = 100_000;

    pub fn record(&mut self, now_unix_ms: u64, tokens: u64) {
        self.prune(now_unix_ms);
        if self.events.len() >= Self::MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back((now_unix_ms, tokens));
    }

    fn prune(&mut self, now_unix_ms: u64) {
        let cutoff = now_unix_ms.saturating_sub(Self::WINDOW_MS);
        while self.events.front().is_some_and(|(at, _)| *at < cutoff) {
            self.events.pop_front();
        }
    }

    /// `(jobs, tokens)` in the trailing hour.
    pub fn summary(&mut self, now_unix_ms: u64) -> (u64, u64) {
        self.prune(now_unix_ms);
        let tokens: u64 = self.events.iter().map(|(_, tokens)| *tokens).sum();
        (self.events.len() as u64, tokens)
    }
}

/// Monotonic counters since the coordinator started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TwinCounters {
    pub groups_started: u64,
    pub groups_matched: u64,
    pub groups_mismatched: u64,
    pub groups_incomplete: u64,
    pub replays_matched: u64,
    pub replays_mismatched: u64,
    pub verified_jobs: u64,
    pub verified_tokens: u64,
    pub worker_compute_tokens: u64,
    pub rejected_legs: u64,
    pub late_legs: u64,
    pub legs_requeued: u64,
    pub recompute_spot_check: u64,
    pub recompute_mismatch: u64,
    pub recompute_fallback: u64,
    pub recompute_reward: u64,
    pub recompute_skipped_busy: u64,
    pub recompute_unavailable: u64,
    pub recompute_contradicted: u64,
    pub recompute_ms_total: u64,
    pub recomputes_avoided: u64,
    pub demand_public_request: u64,
    pub demand_public_demo: u64,
    pub demand_pump_demo: u64,
    pub demand_pump_replay: u64,
    pub demand_dry_run_ticks: u64,
    pub public_demo_rate_limited: u64,
    pub public_demo_rejected: u64,
    pub pairing_refused: u64,
    pub pairing_deferred_diversity: u64,
    pub pairing_deferred_fairness: u64,
    pub pairs_cross_region: u64,
    pub pairs_same_region: u64,
    pub pairs_region_unknown: u64,
    pub pairs_cross_platform: u64,
    pub references_contradicted: u64,
}

impl TwinCounters {
    pub fn count_demand(&mut self, source: DemandSource) {
        match source {
            DemandSource::PublicRequest => self.demand_public_request += 1,
            DemandSource::PublicDemo => self.demand_public_demo += 1,
            DemandSource::PumpDemo => self.demand_pump_demo += 1,
            DemandSource::PumpReplay => self.demand_pump_replay += 1,
        }
    }

    /// Share of twin comparisons that matched, if any were made.
    pub fn twin_match_rate(&self) -> Option<f64> {
        let compared = self.groups_matched + self.groups_mismatched;
        if compared == 0 {
            return None;
        }
        Some(self.groups_matched as f64 / compared as f64)
    }
}

/// Status of one leg in a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegStatus {
    Unclaimed,
    Claimed,
    Submitted,
    Failed,
    Declined,
    Abandoned,
}

impl LegStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unclaimed => "unclaimed",
            Self::Claimed => "claimed",
            Self::Submitted => "submitted",
            Self::Failed => "failed",
            Self::Declined => "declined",
            Self::Abandoned => "abandoned",
        }
    }

    /// The leg can no longer produce a result.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Submitted | Self::Failed | Self::Declined)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TwinLegReceipt {
    pub leg: usize,
    pub job_id: String,
    pub worker_id: Option<String>,
    pub region: Option<RegionTag>,
    pub platform: Option<String>,
    pub status: LegStatus,
    pub output_hash: Option<String>,
    pub tokens_generated: Option<u64>,
    pub ms_per_token: Option<u64>,
    pub worker_attestation_hash: Option<String>,
    pub valid: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReferenceReceipt {
    pub group_id: String,
    pub output_hash: String,
    pub valid: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComparisonReceipt {
    pub basis: &'static str,
    pub checkpoint_interval_tokens: usize,
    pub result: ComparisonResult,
    pub mismatch_fields: Vec<&'static str>,
    pub reference: Option<ReferenceReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndependenceReceipt {
    pub distinct_worker_keys: bool,
    /// "yes", "no" or "unknown".
    pub distinct_operators: &'static str,
    pub distinct_network_groups: &'static str,
    pub different_regions: Option<bool>,
    pub different_platforms: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecomputeReceipt {
    pub reason: Option<RecomputeReason>,
    pub status: RecomputeStatus,
    pub method: Option<&'static str>,
    pub output_hash: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReceiptSignature {
    pub scheme: &'static str,
    pub signer: String,
    pub public_key: String,
    pub signature: String,
}

/// The public record of one twin-executed job.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TwinReceipt {
    pub schema: &'static str,
    pub group_id: String,
    pub coordinator: String,
    pub source: DemandSource,
    pub public_prompt: Option<&'static str>,
    pub model_id: String,
    pub execution_profile: String,
    pub input_hash: String,
    pub max_tokens: u32,
    pub created_at_unix_ms: u64,
    pub resolved_at_unix_ms: u64,
    pub legs: Vec<TwinLegReceipt>,
    pub comparison: ComparisonReceipt,
    pub independence: IndependenceReceipt,
    pub validator_recompute: RecomputeReceipt,
    pub verdict: Verdict,
    pub verified_by: Vec<&'static str>,
    pub settlement: Option<serde_json::Value>,
    pub disclosure: &'static str,
    pub commitment: String,
    pub coordinator_signature: Option<ReceiptSignature>,
}

fn put_str(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

impl TwinReceipt {
    /// Language-neutral commitment the coordinator signs. Every string is
    /// length-prefixed (u64 big-endian) UTF-8; absent values are empty
    /// strings, an absent token count is `u64::MAX`.
    pub fn compute_commitment(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key(RECEIPT_COMMITMENT_DOMAIN);
        hasher.update(&[1u8]);
        put_str(&mut hasher, &self.group_id);
        put_str(&mut hasher, &self.coordinator);
        put_str(&mut hasher, self.source.as_str());
        put_str(&mut hasher, &self.model_id);
        put_str(&mut hasher, &self.input_hash);
        hasher.update(&u64::from(self.max_tokens).to_be_bytes());
        hasher.update(&(self.legs.len() as u64).to_be_bytes());
        for leg in &self.legs {
            put_str(&mut hasher, &leg.job_id);
            put_str(&mut hasher, leg.worker_id.as_deref().unwrap_or(""));
            put_str(&mut hasher, leg.status.as_str());
            put_str(&mut hasher, leg.output_hash.as_deref().unwrap_or(""));
            hasher.update(&leg.tokens_generated.unwrap_or(u64::MAX).to_be_bytes());
            put_str(
                &mut hasher,
                match leg.valid {
                    Some(true) => "true",
                    Some(false) => "false",
                    None => "null",
                },
            );
        }
        put_str(&mut hasher, self.comparison.result.as_str());
        put_str(&mut hasher, self.validator_recompute.status.as_str());
        put_str(&mut hasher, self.verdict.as_str());
        hasher.update(&self.resolved_at_unix_ms.to_be_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }
}

/// Bounded store of recent receipts, addressable by group or leg job id.
#[derive(Debug, Default)]
pub struct ReceiptStore {
    order: VecDeque<String>,
    receipts: HashMap<String, TwinReceipt>,
    aliases: HashMap<String, String>,
}

impl ReceiptStore {
    pub const CAPACITY: usize = 512;

    /// Lower-case job id without a 0x prefix.
    pub fn normalize_id(id: &str) -> String {
        id.trim()
            .strip_prefix("0x")
            .unwrap_or(id.trim())
            .to_ascii_lowercase()
    }

    pub fn insert(&mut self, receipt: TwinReceipt) {
        let group_id = Self::normalize_id(&receipt.group_id);
        for leg in &receipt.legs {
            self.aliases
                .insert(Self::normalize_id(&leg.job_id), group_id.clone());
        }
        if let Some(existing) = self.receipts.get_mut(&group_id) {
            *existing = receipt;
            return;
        }
        while self.order.len() >= Self::CAPACITY {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.receipts.remove(&oldest) {
                for leg in &evicted.legs {
                    self.aliases.remove(&Self::normalize_id(&leg.job_id));
                }
            }
        }
        self.order.push_back(group_id.clone());
        self.receipts.insert(group_id, receipt);
    }

    pub fn get(&self, job_or_group_id: &str) -> Option<&TwinReceipt> {
        let key = Self::normalize_id(job_or_group_id);
        let group_id = self.aliases.get(&key).cloned().unwrap_or(key);
        self.receipts.get(&group_id)
    }

    /// Newest first.
    pub fn recent(&self, limit: usize) -> Vec<&TwinReceipt> {
        self.order
            .iter()
            .rev()
            .filter_map(|group_id| self.receipts.get(group_id))
            .take(limit)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(label: &str) -> Hash256 {
        arc_crypto::hash_bytes(label.as_bytes())
    }

    fn output(label: &str, tokens: u64) -> LegOutput {
        LegOutput {
            output_hash: hash(label),
            tokens_generated: tokens,
            text_digest: hash(&format!("text-{label}")),
        }
    }

    fn facts(worker_id: &str) -> WorkerFacts {
        WorkerFacts {
            worker_id: worker_id.to_string(),
            ..WorkerFacts::default()
        }
    }

    fn tag(region: &str, continent: &str, class: RegionClass) -> RegionTag {
        RegionTag {
            region: region.to_string(),
            continent: continent.to_string(),
            class,
            basis: REGION_TAG_BASIS,
            measured_at_unix_ms: 1,
            nearest_rtt_ms: 10,
        }
    }

    fn known_validator(index: usize) -> Hash256 {
        Hash256::from_hex(KNOWN_VALIDATOR_REGIONS[index].0).unwrap()
    }

    #[test]
    fn config_defaults_are_off_and_normalization_clamps() {
        let config = TwinConfig::default();
        assert!(!config.twin_execution);
        assert!(!config.demand_pump);
        let clamped = TwinConfig {
            twin_execution: true,
            demand_pump: true,
            demand_dry_run: true,
            spot_check_per_mille: 5_000,
            demand_interval_secs: 1,
        }
        .normalized();
        assert_eq!(clamped.spot_check_per_mille, 1_000);
        assert_eq!(clamped.demand_interval_secs, MIN_DEMAND_INTERVAL_SECS);
        assert!(clamped.twin_execution && clamped.demand_pump && clamped.demand_dry_run);
    }

    #[test]
    fn region_classification_uses_research_7_diameters() {
        let amsterdam = known_validator(2);
        let tokyo = known_validator(4);
        let near = classify_region(
            &[
                RttSample {
                    validator: amsterdam,
                    rtt_ms: 18,
                },
                RttSample {
                    validator: tokyo,
                    rtt_ms: 240,
                },
            ],
            7,
        )
        .unwrap();
        assert_eq!(near.region, "eu-west");
        assert_eq!(near.continent, "europe");
        assert_eq!(near.class, RegionClass::Region);
        assert_eq!(near.locality(), "region:eu-west");
        assert_eq!(near.measured_at_unix_ms, 7);

        let edge = classify_region(
            &[RttSample {
                validator: amsterdam,
                rtt_ms: REGION_DIAMETER_MS,
            }],
            0,
        )
        .unwrap();
        assert_eq!(edge.class, RegionClass::Region);
        let continental = classify_region(
            &[RttSample {
                validator: amsterdam,
                rtt_ms: REGION_DIAMETER_MS + 1,
            }],
            0,
        )
        .unwrap();
        assert_eq!(continental.class, RegionClass::Continent);
        assert_eq!(continental.locality(), "continent:europe");
        let distant = classify_region(
            &[RttSample {
                validator: tokyo,
                rtt_ms: CONTINENT_DIAMETER_MS + 1,
            }],
            0,
        )
        .unwrap();
        assert_eq!(distant.class, RegionClass::Distant);
        assert_eq!(distant.continent, "asia-pacific");
        assert!(classify_region(&[], 0).is_none());
    }

    #[test]
    fn unknown_validators_are_labelled_by_address_and_rtt_is_never_published() {
        let unknown = hash("some other validator");
        let tagged = classify_region(
            &[RttSample {
                validator: unknown,
                rtt_ms: 12,
            }],
            3,
        )
        .unwrap();
        assert_eq!(
            tagged.region,
            format!("validator-{}", &unknown.to_hex()[..8])
        );
        assert_eq!(tagged.continent, "unknown");
        let published = serde_json::to_value(&tagged).unwrap();
        assert!(published.get("nearest_rtt_ms").is_none());
        assert_eq!(published["class"], "region");
        assert_eq!(published["basis"], REGION_TAG_BASIS);
    }

    #[test]
    fn region_reports_are_bounded_and_keep_the_fastest_duplicate() {
        let validator = format!("0x{}", KNOWN_VALIDATOR_REGIONS[0].0);
        let report = CommunityRegionReport {
            worker_id: "0xabc".to_string(),
            samples: vec![
                CommunityRttSample {
                    validator: validator.clone(),
                    rtt_ms: 40,
                },
                CommunityRttSample {
                    validator: validator.clone(),
                    rtt_ms: 25,
                },
            ],
        };
        let parsed = report.parsed_samples().unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].rtt_ms, 25);

        let empty = CommunityRegionReport {
            worker_id: "0xabc".to_string(),
            samples: Vec::new(),
        };
        assert!(empty.parsed_samples().is_err());
        let too_slow = CommunityRegionReport {
            worker_id: "0xabc".to_string(),
            samples: vec![CommunityRttSample {
                validator: validator.clone(),
                rtt_ms: MAX_REPORTED_RTT_MS + 1,
            }],
        };
        assert!(too_slow.parsed_samples().is_err());
        let malformed = CommunityRegionReport {
            worker_id: "0xabc".to_string(),
            samples: vec![CommunityRttSample {
                validator: "0x1234".to_string(),
                rtt_ms: 5,
            }],
        };
        assert!(malformed.parsed_samples().is_err());
        let too_many = CommunityRegionReport {
            worker_id: "0xabc".to_string(),
            samples: (0..=MAX_RTT_SAMPLES)
                .map(|_| CommunityRttSample {
                    validator: validator.clone(),
                    rtt_ms: 5,
                })
                .collect(),
        };
        assert!(too_many.parsed_samples().is_err());
        let wire = serde_json::json!({
            "worker_id": "0xabc",
            "samples": [{"validator": validator, "rtt_ms": 5, "extra": 1}],
        });
        assert!(serde_json::from_value::<CommunityRegionReport>(wire).is_err());
    }

    #[test]
    fn independence_requires_distinct_keys_operators_and_networks() {
        let a = facts("0xa");
        let b = facts("0xb");
        assert_eq!(independence(&a, &b), Independence::Independent);
        assert_eq!(independence(&a, &a.clone()), Independence::SameWorker);
        let mut a_operator = a.clone();
        let mut b_operator = b.clone();
        a_operator.operator = Some("family-1".to_string());
        b_operator.operator = Some("family-1".to_string());
        assert_eq!(
            independence(&a_operator, &b_operator),
            Independence::SameOperator
        );
        b_operator.operator = Some("family-2".to_string());
        assert_eq!(
            independence(&a_operator, &b_operator),
            Independence::Independent
        );
        let mut a_network = a.clone();
        let mut b_network = b;
        a_network.network_group = Some("net-1".to_string());
        b_network.network_group = Some("net-1".to_string());
        assert_eq!(
            independence(&a_network, &b_network),
            Independence::SameNetwork
        );
        // Unknown on one side never blocks a pair.
        a_network.network_group = None;
        assert_eq!(
            independence(&a_network, &b_network),
            Independence::Independent
        );
    }

    #[test]
    fn claims_refuse_the_same_worker_and_prefer_diverse_idle_workers_inside_the_window() {
        let mut sibling = facts("0xa");
        sibling.platform = Some("macos-aarch64".to_string());
        sibling.region = Some(tag("eu-west", "europe", RegionClass::Region));
        let related = [sibling.clone()];

        let same = ClaimContext {
            claimer: &sibling,
            related: &related,
            idle_alternatives: &[],
            waited_ms: 0,
            preference_window_ms: CLAIM_PREFERENCE_WINDOW_MS,
            prefer_least_recently_served: false,
        };
        assert_eq!(
            decide_claim(&same),
            ClaimDecision::Refuse(REASON_SAME_WORKER)
        );

        let mut similar = facts("0xb");
        similar.platform = Some("macos-aarch64".to_string());
        similar.region = Some(tag("eu-west", "europe", RegionClass::Region));
        let mut diverse = facts("0xc");
        diverse.platform = Some("linux-x86_64".to_string());
        diverse.region = Some(tag("us-east", "north-america", RegionClass::Region));
        let alternatives = [diverse.clone()];
        let inside = ClaimContext {
            claimer: &similar,
            related: &related,
            idle_alternatives: &alternatives,
            waited_ms: 100,
            preference_window_ms: CLAIM_PREFERENCE_WINDOW_MS,
            prefer_least_recently_served: false,
        };
        assert_eq!(
            decide_claim(&inside),
            ClaimDecision::Defer(REASON_PREFER_DIVERSE)
        );
        assert_eq!(diversity_score(&diverse, &related), 2);
        assert_eq!(diversity_score(&similar, &related), 0);

        let after = ClaimContext {
            waited_ms: CLAIM_PREFERENCE_WINDOW_MS,
            ..inside
        };
        assert_eq!(decide_claim(&after), ClaimDecision::Accept);

        let diverse_claim = ClaimContext {
            claimer: &diverse,
            related: &related,
            idle_alternatives: &[],
            waited_ms: 0,
            preference_window_ms: CLAIM_PREFERENCE_WINDOW_MS,
            prefer_least_recently_served: false,
        };
        assert_eq!(decide_claim(&diverse_claim), ClaimDecision::Accept);
    }

    #[test]
    fn unknown_regions_never_earn_diversity_and_dependent_alternatives_do_not_defer() {
        let sibling = facts("0xa");
        let related = [sibling.clone()];
        let mut claimer = facts("0xb");
        claimer.region = Some(tag("us-east", "north-america", RegionClass::Region));
        assert_eq!(diversity_score(&claimer, &related), 0);
        // The only better-placed idle worker is the sibling itself.
        let mut sibling_alternative = sibling.clone();
        sibling_alternative.platform = Some("linux-x86_64".to_string());
        let alternatives = [sibling_alternative];
        let context = ClaimContext {
            claimer: &claimer,
            related: &related,
            idle_alternatives: &alternatives,
            waited_ms: 0,
            preference_window_ms: CLAIM_PREFERENCE_WINDOW_MS,
            prefer_least_recently_served: true,
        };
        assert_eq!(decide_claim(&context), ClaimDecision::Accept);
    }

    #[test]
    fn generated_demand_prefers_the_least_recently_served_worker() {
        let mut recent = facts("0xa");
        recent.last_served_unix_ms = Some(10_000_000);
        let mut hungry = facts("0xb");
        hungry.last_served_unix_ms = Some(10_000_000 - FAIRNESS_MARGIN_MS - 1);
        let alternatives = [hungry.clone()];
        let pump = ClaimContext {
            claimer: &recent,
            related: &[],
            idle_alternatives: &alternatives,
            waited_ms: 0,
            preference_window_ms: CLAIM_PREFERENCE_WINDOW_MS,
            prefer_least_recently_served: true,
        };
        assert_eq!(
            decide_claim(&pump),
            ClaimDecision::Defer(REASON_PREFER_LEAST_SERVED)
        );
        let caller = ClaimContext {
            prefer_least_recently_served: false,
            ..pump
        };
        assert_eq!(decide_claim(&caller), ClaimDecision::Accept);
        // Inside the margin the claimer keeps the job.
        let mut close = facts("0xc");
        close.last_served_unix_ms = Some(10_000_000 - FAIRNESS_MARGIN_MS + 1);
        let close_alternatives = [close];
        let near_tie = ClaimContext {
            idle_alternatives: &close_alternatives,
            ..pump
        };
        assert_eq!(decide_claim(&near_tie), ClaimDecision::Accept);
    }

    #[test]
    fn comparison_reports_every_differing_field() {
        let a = output("same", 4);
        assert!(mismatch_fields(&a, &a.clone()).is_empty());
        let mut b = a.clone();
        b.text_digest = hash("other text");
        assert_eq!(mismatch_fields(&a, &b), vec!["output_text"]);
        let c = output("different", 5);
        assert_eq!(
            mismatch_fields(&a, &c),
            vec!["output_hash", "tokens_generated", "output_text"]
        );

        let (result, fields) = compare_group(&[Some(a.clone()), Some(a.clone())], None);
        assert_eq!(result, ComparisonResult::Match);
        assert!(fields.is_empty());
        let (result, fields) = compare_group(&[Some(a.clone()), Some(c.clone())], None);
        assert_eq!(result, ComparisonResult::Mismatch);
        assert_eq!(fields.len(), 3);
        let (result, _) = compare_group(&[Some(a.clone()), None], None);
        assert_eq!(result, ComparisonResult::Incomplete);
        let (result, _) = compare_group(&[Some(a.clone())], Some(&a));
        assert_eq!(result, ComparisonResult::ReferenceMatch);
        let (result, _) = compare_group(&[Some(c)], Some(&a));
        assert_eq!(result, ComparisonResult::ReferenceMismatch);
        let (result, _) = compare_group(&[None], Some(&a));
        assert_eq!(result, ComparisonResult::Incomplete);
    }

    #[test]
    fn validators_recompute_only_when_needed() {
        use ComparisonResult as C;
        use DemandSource as S;
        assert_eq!(
            recompute_reason(C::Mismatch, S::PumpDemo, true, false, false),
            Some(RecomputeReason::Mismatch)
        );
        assert_eq!(
            recompute_reason(C::ReferenceMismatch, S::PumpReplay, true, false, false),
            Some(RecomputeReason::Mismatch)
        );
        assert_eq!(
            recompute_reason(C::Match, S::PublicRequest, true, true, false),
            Some(RecomputeReason::Reward)
        );
        assert_eq!(
            recompute_reason(C::Match, S::PumpDemo, true, false, true),
            Some(RecomputeReason::SpotCheck)
        );
        assert_eq!(
            recompute_reason(C::Match, S::PublicRequest, true, false, false),
            None
        );
        assert_eq!(
            recompute_reason(C::Incomplete, S::PublicRequest, true, false, false),
            Some(RecomputeReason::Fallback)
        );
        assert_eq!(
            recompute_reason(C::Incomplete, S::PublicRequest, false, false, false),
            None
        );
        assert_eq!(
            recompute_reason(C::Incomplete, S::PumpDemo, true, false, false),
            None
        );
    }

    #[test]
    fn spot_check_selection_is_keyed_deterministic_and_proportional() {
        let secret = [7u8; 32];
        assert!(!spot_check_selected(&secret, "group", 0));
        assert!(spot_check_selected(&secret, "group", 1_000));
        let first = spot_check_selected(&secret, "group-1", 500);
        assert_eq!(first, spot_check_selected(&secret, "group-1", 500));
        let selected = (0..2_000)
            .filter(|index| spot_check_selected(&secret, &format!("group-{index}"), 100))
            .count();
        assert!(
            (120..=280).contains(&selected),
            "selected {selected} of 2000"
        );
        let other_secret = [8u8; 32];
        let differs = (0..64).any(|index| {
            let id = format!("group-{index}");
            spot_check_selected(&secret, &id, 500) != spot_check_selected(&other_secret, &id, 500)
        });
        assert!(differs);
    }

    #[test]
    fn classification_without_validators_trusts_only_agreement() {
        let a = output("a", 3);
        let legs = [Some(a.clone()), Some(a.clone())];
        let matched = classify(&legs, ComparisonResult::Match, None, None);
        assert_eq!(matched.verdict, Verdict::Verified);
        assert_eq!(matched.verified_by, vec![VERIFIED_BY_TWIN]);
        assert_eq!(matched.leg_valid, vec![Some(true), Some(true)]);
        assert_eq!(matched.chosen_leg, Some(0));
        assert_eq!(matched.recompute_status, RecomputeStatus::NotSelected);

        let b = output("b", 3);
        let mismatched = classify(
            &[Some(a.clone()), Some(b)],
            ComparisonResult::Mismatch,
            None,
            Some(Err(RecomputeStatus::Unavailable)),
        );
        assert_eq!(mismatched.verdict, Verdict::Unverified);
        assert_eq!(mismatched.leg_valid, vec![None, None]);
        assert_eq!(mismatched.chosen_leg, None);
        assert_eq!(mismatched.recompute_status, RecomputeStatus::Unavailable);

        let replay = classify(
            &[Some(a.clone())],
            ComparisonResult::ReferenceMatch,
            Some(&a),
            None,
        );
        assert_eq!(replay.verdict, Verdict::Verified);
        assert_eq!(replay.verified_by, vec![VERIFIED_BY_REFERENCE]);
        assert_eq!(replay.reference_valid, Some(true));

        let incomplete = classify(&[Some(a), None], ComparisonResult::Incomplete, None, None);
        assert_eq!(incomplete.verdict, Verdict::Unverified);
    }

    #[test]
    fn classification_with_validators_resolves_mismatch_and_catches_colluding_twins() {
        let honest = output("honest", 4);
        let wrong = output("wrong", 4);

        let resolved = classify(
            &[Some(wrong.clone()), Some(honest.clone())],
            ComparisonResult::Mismatch,
            None,
            Some(Ok(&honest)),
        );
        assert_eq!(resolved.verdict, Verdict::Verified);
        assert_eq!(resolved.leg_valid, vec![Some(false), Some(true)]);
        assert_eq!(resolved.chosen_leg, Some(1));
        assert_eq!(resolved.recompute_status, RecomputeStatus::Confirmed);
        assert_eq!(resolved.verified_by, vec![VERIFIED_BY_VALIDATORS]);

        let colluding = classify(
            &[Some(wrong.clone()), Some(wrong.clone())],
            ComparisonResult::Match,
            None,
            Some(Ok(&honest)),
        );
        assert_eq!(colluding.verdict, Verdict::Rejected);
        assert_eq!(colluding.leg_valid, vec![Some(false), Some(false)]);
        assert_eq!(colluding.recompute_status, RecomputeStatus::Contradicted);
        assert!(colluding.verified_by.is_empty());

        let confirmed = classify(
            &[Some(honest.clone()), Some(honest.clone())],
            ComparisonResult::Match,
            None,
            Some(Ok(&honest)),
        );
        assert_eq!(confirmed.verdict, Verdict::Verified);
        assert_eq!(
            confirmed.verified_by,
            vec![VERIFIED_BY_TWIN, VERIFIED_BY_VALIDATORS]
        );
        assert_eq!(confirmed.recompute_status, RecomputeStatus::Confirmed);

        let stale_reference = classify(
            &[Some(honest.clone())],
            ComparisonResult::ReferenceMismatch,
            Some(&wrong),
            Some(Ok(&honest)),
        );
        assert_eq!(stale_reference.verdict, Verdict::Verified);
        assert_eq!(stale_reference.reference_valid, Some(false));
        assert_eq!(stale_reference.leg_valid, vec![Some(true)]);

        let fallback = classify(
            &[Some(honest.clone()), None],
            ComparisonResult::Incomplete,
            None,
            Some(Ok(&honest)),
        );
        assert_eq!(fallback.verdict, Verdict::Verified);
        assert_eq!(fallback.leg_valid, vec![Some(true), None]);
        assert_eq!(fallback.verified_by, vec![VERIFIED_BY_VALIDATORS]);
    }

    #[test]
    fn checkpoint_chain_is_deterministic_and_locates_the_first_divergence() {
        let job = hash("job");
        let tokens: Vec<u32> = (0..70).collect();
        let chain = checkpoint_chain(&job, &tokens, CHECKPOINT_INTERVAL_TOKENS);
        assert_eq!(chain.len(), 3);
        assert_eq!(
            chain,
            checkpoint_chain(&job, &tokens, CHECKPOINT_INTERVAL_TOKENS)
        );
        assert_ne!(
            chain,
            checkpoint_chain(&hash("other job"), &tokens, CHECKPOINT_INTERVAL_TOKENS)
        );
        let mut altered = tokens.clone();
        altered[40] = 999;
        let altered_chain = checkpoint_chain(&job, &altered, CHECKPOINT_INTERVAL_TOKENS);
        assert_eq!(chain[0], altered_chain[0]);
        assert_eq!(first_divergent_checkpoint(&chain, &altered_chain), Some(1));
        assert_eq!(first_divergent_checkpoint(&chain, &chain), None);
        let truncated = checkpoint_chain(&job, &tokens[..64], CHECKPOINT_INTERVAL_TOKENS);
        assert_eq!(first_divergent_checkpoint(&chain, &truncated), Some(2));
        assert!(checkpoint_chain(&job, &[], CHECKPOINT_INTERVAL_TOKENS).is_empty());
        assert_eq!(checkpoint_chain(&job, &tokens, 0).len(), tokens.len());
    }

    #[test]
    fn token_buckets_refill_exactly_and_cap_at_capacity() {
        let mut bucket = TokenBucket::new(2, 60, 0);
        assert!(bucket.try_take(0));
        assert!(bucket.try_take(0));
        assert!(!bucket.try_take(0));
        // 60 per hour is one per minute, with no rounding loss across
        // frequent small refills.
        for step in 1..60 {
            assert!(!bucket.try_take(step * 1_000), "step {step}");
        }
        assert!(bucket.try_take(60_000));
        assert!(!bucket.try_take(60_000));
        // A long idle period refills to capacity, not beyond.
        assert!(bucket.try_take(10 * 3_600_000));
        assert!(bucket.try_take(10 * 3_600_000));
        assert!(!bucket.try_take(10 * 3_600_000));
        let mut never = TokenBucket::new(0, 0, 0);
        assert!(!never.try_take(u64::MAX));
    }

    #[test]
    fn demand_planning_needs_idle_workers_and_prefers_replay_when_alone() {
        assert!(plan_demand(0, true, true, 0, 250).is_err());
        assert_eq!(plan_demand(1, true, true, 999, 250), Ok(DemandKind::Replay));
        assert!(plan_demand(1, true, false, 999, 250).is_err());
        assert_eq!(plan_demand(2, true, true, 100, 250), Ok(DemandKind::Replay));
        assert_eq!(
            plan_demand(2, true, true, 600, 250),
            Ok(DemandKind::TwinDemo)
        );
        assert_eq!(
            plan_demand(2, true, false, 100, 250),
            Ok(DemandKind::TwinDemo)
        );
        assert_eq!(
            plan_demand(5, false, true, 999, 250),
            Ok(DemandKind::Replay)
        );
        assert!(plan_demand(5, false, false, 999, 250).is_err());
    }

    #[test]
    fn public_prompts_are_fixed_harmless_and_cycle() {
        let (first_slot, first) = public_demo_prompt(0);
        assert_eq!(first_slot, 0);
        assert_eq!(first, PUBLIC_DEMO_PROMPTS[0]);
        let (wrapped_slot, wrapped) = public_demo_prompt(PUBLIC_DEMO_PROMPTS.len() as u64 + 3);
        assert_eq!(wrapped_slot, 3);
        assert_eq!(wrapped, PUBLIC_DEMO_PROMPTS[3]);
        for prompt in PUBLIC_DEMO_PROMPTS {
            assert_eq!(public_demo_prompt_rejection(prompt), None, "{prompt}");
            assert!(prompt.len() < 80);
        }
    }

    #[test]
    fn public_demo_screen_rejects_private_data_shapes() {
        assert_eq!(public_demo_prompt_rejection("What is 12 plus 30?"), None);
        assert_eq!(
            public_demo_prompt_rejection("Tell me about the year 2024 and 2025."),
            None
        );
        assert!(public_demo_prompt_rejection("   ").is_some());
        assert!(
            public_demo_prompt_rejection(&"a".repeat(PUBLIC_DEMO_PROMPT_MAX_BYTES + 1)).is_some()
        );
        assert!(public_demo_prompt_rejection("mail me at jane.doe@example.com please").is_some());
        assert!(public_demo_prompt_rejection("(write to x@y.org)").is_some());
        assert_eq!(public_demo_prompt_rejection("follow @arc on social"), None);
        assert!(public_demo_prompt_rejection("call +1 (555) 123-4567").is_some());
        assert!(public_demo_prompt_rejection("my card is 4111 1111 1111 1111").is_some());
        // Built at runtime so no secret scanner mistakes the fixture for a key.
        let secret_like = format!("token {}", "a1b2c3d4".repeat(5));
        assert!(public_demo_prompt_rejection(&secret_like).is_some());
        assert_eq!(
            public_demo_prompt_rejection("supercalifragilisticexpialidocious words"),
            None
        );
    }

    #[test]
    fn reference_book_picks_by_model_and_evicts_only_the_contradicted_source() {
        let model = hash("model");
        let mut book = ReferenceBook::default();
        assert!(book.is_empty());
        assert!(book.pick(0, &model).is_none());
        for index in 0..3 {
            book.record(ReplayReference {
                prompt_index: index,
                input: format!("prompt {index}"),
                max_tokens: PUMP_MAX_TOKENS,
                model_id: model,
                output: output(&format!("answer {index}"), 4),
                source_group_id: format!("group-{index}"),
                workers: vec!["0xa".to_string(), "0xb".to_string()],
                recorded_at_unix_ms: 1,
            });
        }
        assert_eq!(book.len(), 3);
        assert!(book.has_model(&model));
        assert!(!book.has_model(&hash("other model")));
        assert!(book.pick(0, &hash("other model")).is_none());
        assert_eq!(book.pick(4, &model).unwrap().prompt_index, 1);
        book.evict(1, "not-the-source");
        assert_eq!(book.len(), 3);
        book.evict(1, "group-1");
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn throughput_window_counts_only_the_trailing_hour() {
        let mut window = ThroughputWindow::default();
        window.record(1_000, 10);
        window.record(2_000, 20);
        assert_eq!(window.summary(2_000), (2, 30));
        assert_eq!(window.summary(1_000 + ThroughputWindow::WINDOW_MS), (2, 30));
        assert_eq!(window.summary(1_001 + ThroughputWindow::WINDOW_MS), (1, 20));
        assert_eq!(window.summary(10 * ThroughputWindow::WINDOW_MS), (0, 0));
    }

    #[test]
    fn counters_report_match_rate_only_after_comparisons() {
        let mut counters = TwinCounters::default();
        assert_eq!(counters.twin_match_rate(), None);
        counters.groups_matched = 3;
        counters.groups_mismatched = 1;
        assert!(
            counters
                .twin_match_rate()
                .is_some_and(|rate| (rate - 0.75).abs() < 1e-9)
        );
        counters.count_demand(DemandSource::PumpReplay);
        counters.count_demand(DemandSource::PublicDemo);
        assert_eq!(counters.demand_pump_replay, 1);
        assert_eq!(counters.demand_public_demo, 1);
    }

    #[test]
    fn wire_names_match_their_commitment_strings() {
        for source in [
            DemandSource::PublicRequest,
            DemandSource::PublicDemo,
            DemandSource::PumpDemo,
            DemandSource::PumpReplay,
        ] {
            assert_eq!(serde_json::to_value(source).unwrap(), source.as_str());
        }
        for status in [
            LegStatus::Unclaimed,
            LegStatus::Claimed,
            LegStatus::Submitted,
            LegStatus::Failed,
            LegStatus::Declined,
            LegStatus::Abandoned,
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), status.as_str());
        }
        for result in [
            ComparisonResult::Match,
            ComparisonResult::Mismatch,
            ComparisonResult::Incomplete,
            ComparisonResult::ReferenceMatch,
            ComparisonResult::ReferenceMismatch,
        ] {
            assert_eq!(serde_json::to_value(result).unwrap(), result.as_str());
        }
        for status in [
            RecomputeStatus::NotSelected,
            RecomputeStatus::Confirmed,
            RecomputeStatus::Contradicted,
            RecomputeStatus::Unavailable,
            RecomputeStatus::SkippedBusy,
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), status.as_str());
        }
        for verdict in [Verdict::Verified, Verdict::Rejected, Verdict::Unverified] {
            assert_eq!(serde_json::to_value(verdict).unwrap(), verdict.as_str());
        }
        assert!(LegStatus::Declined.is_terminal());
        assert!(!LegStatus::Claimed.is_terminal());
        assert!(!LegStatus::Abandoned.is_terminal());
    }

    fn sample_receipt() -> TwinReceipt {
        TwinReceipt {
            schema: TWIN_RECEIPT_SCHEMA,
            group_id: "aa".repeat(32),
            coordinator: format!("0x{}", "11".repeat(32)),
            source: DemandSource::PumpDemo,
            public_prompt: Some(PUBLIC_DEMO_PROMPTS[0]),
            model_id: format!("0x{}", "22".repeat(32)),
            execution_profile: "INT8".to_string(),
            input_hash: format!("0x{}", "33".repeat(32)),
            max_tokens: PUMP_MAX_TOKENS,
            created_at_unix_ms: 10,
            resolved_at_unix_ms: 20,
            legs: vec![
                TwinLegReceipt {
                    leg: 0,
                    job_id: "aa".repeat(32),
                    worker_id: Some("0xa".to_string()),
                    region: None,
                    platform: Some("linux-x86_64".to_string()),
                    status: LegStatus::Submitted,
                    output_hash: Some(format!("0x{}", "44".repeat(32))),
                    tokens_generated: Some(4),
                    ms_per_token: Some(700),
                    worker_attestation_hash: None,
                    valid: Some(true),
                },
                TwinLegReceipt {
                    leg: 1,
                    job_id: "bb".repeat(32),
                    worker_id: Some("0xb".to_string()),
                    region: None,
                    platform: Some("macos-aarch64".to_string()),
                    status: LegStatus::Submitted,
                    output_hash: Some(format!("0x{}", "44".repeat(32))),
                    tokens_generated: Some(4),
                    ms_per_token: Some(900),
                    worker_attestation_hash: None,
                    valid: Some(true),
                },
            ],
            comparison: ComparisonReceipt {
                basis: COMPARISON_BASIS_FINAL_OUTPUT,
                checkpoint_interval_tokens: CHECKPOINT_INTERVAL_TOKENS,
                result: ComparisonResult::Match,
                mismatch_fields: Vec::new(),
                reference: None,
            },
            independence: IndependenceReceipt {
                distinct_worker_keys: true,
                distinct_operators: "unknown",
                distinct_network_groups: "unknown",
                different_regions: None,
                different_platforms: Some(true),
            },
            validator_recompute: RecomputeReceipt {
                reason: None,
                status: RecomputeStatus::NotSelected,
                method: None,
                output_hash: None,
                duration_ms: None,
            },
            verdict: Verdict::Verified,
            verified_by: vec![VERIFIED_BY_TWIN],
            settlement: None,
            disclosure: CENTRALIZATION_DISCLOSURE,
            commitment: String::new(),
            coordinator_signature: None,
        }
    }

    #[test]
    fn receipt_commitment_binds_workers_hashes_and_verdict_but_not_presentation() {
        let receipt = sample_receipt();
        let commitment = receipt.compute_commitment();
        assert_eq!(commitment, sample_receipt().compute_commitment());

        let mut presentation = sample_receipt();
        presentation.legs[0].ms_per_token = Some(1);
        presentation.disclosure = "";
        assert_eq!(presentation.compute_commitment(), commitment);

        let mut swapped_worker = sample_receipt();
        swapped_worker.legs[1].worker_id = Some("0xc".to_string());
        assert_ne!(swapped_worker.compute_commitment(), commitment);
        let mut swapped_hash = sample_receipt();
        swapped_hash.legs[1].output_hash = Some(format!("0x{}", "55".repeat(32)));
        assert_ne!(swapped_hash.compute_commitment(), commitment);
        let mut flipped = sample_receipt();
        flipped.verdict = Verdict::Rejected;
        assert_ne!(flipped.compute_commitment(), commitment);
        let mut recomputed = sample_receipt();
        recomputed.validator_recompute.status = RecomputeStatus::Confirmed;
        assert_ne!(recomputed.compute_commitment(), commitment);

        let wire = serde_json::to_value(&receipt).unwrap();
        assert_eq!(wire["schema"], TWIN_RECEIPT_SCHEMA);
        assert_eq!(wire["comparison"]["result"], "match");
        assert_eq!(wire["legs"][1]["worker_id"], "0xb");
        assert_eq!(wire["verdict"], "verified");
        assert_eq!(wire["validator_recompute"]["status"], "not_selected");
    }

    #[test]
    fn receipt_store_is_bounded_and_addressable_by_any_leg() {
        let mut store = ReceiptStore::default();
        let receipt = sample_receipt();
        store.insert(receipt.clone());
        assert_eq!(store.len(), 1);
        assert!(store.get(&receipt.group_id).is_some());
        assert!(store.get(&format!("0x{}", "BB".repeat(32))).is_some());
        assert!(store.get("unknown").is_none());

        let mut updated = sample_receipt();
        updated.verdict = Verdict::Rejected;
        store.insert(updated);
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get(&receipt.group_id).unwrap().verdict,
            Verdict::Rejected
        );

        for index in 0..ReceiptStore::CAPACITY + 5 {
            let mut next = sample_receipt();
            next.group_id = format!("{index:064x}");
            next.legs[0].job_id = next.group_id.clone();
            next.legs[1].job_id = format!("{:064x}", index + 1_000_000);
            store.insert(next);
        }
        assert_eq!(store.len(), ReceiptStore::CAPACITY);
        assert!(store.get(&receipt.group_id).is_none());
        let recent = store.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(
            recent[0].group_id,
            format!("{:064x}", ReceiptStore::CAPACITY + 4)
        );
    }
}
