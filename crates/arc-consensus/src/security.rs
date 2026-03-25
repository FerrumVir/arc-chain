//! Security Detection Modules for ARC Chain Consensus
//!
//! This module addresses three critical consensus security concerns:
//!
//! 1. **Block Withholding Detection** (#27): Identifies validators who consistently
//!    fail to publish blocks they were expected to produce, indicating a withholding
//!    attack that can degrade network liveness.
//!
//! 2. **Long-Range Attack Prevention** (#28): Maintains a checkpoint registry of
//!    finalized chain state, rejecting any forks that diverge before the latest
//!    checkpoint. This prevents attackers from rewriting distant history.
//!
//! 3. **Nothing-at-Stake Mitigation** (#29): Detects double-voting across forks
//!    and enforces graduated slashing penalties to make equivocation economically
//!    irrational.

use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tracing::warn;

// ══════════════════════════════════════════════════════════════════════════════
// §1  Block Withholding Detection (#27)
// ══════════════════════════════════════════════════════════════════════════════

/// Report of a validator suspected of withholding blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WithholdingReport {
    /// The validator suspected of withholding.
    pub validator: Hash256,
    /// Rounds where the validator was expected to produce a block but did not.
    pub missing_rounds: Vec<u64>,
    /// Total number of rounds where the validator was expected to produce.
    pub total_expected: u64,
    /// Ratio of missing to expected (0.0 = perfect, 1.0 = never produced).
    pub withholding_score: f64,
}

/// Tracks expected vs received blocks per validator to detect withholding attacks.
///
/// A withholding attack occurs when a validator is selected to produce a block but
/// deliberately withholds it, degrading network throughput without an overt protocol
/// violation. The detector flags validators whose withholding score exceeds 0.5
/// over a configurable window (default 100 rounds).
#[derive(Debug, Default)]
pub struct WithholdingDetector {
    /// Set of (validator, round) pairs where a block was expected.
    expected: HashMap<Hash256, HashSet<u64>>,
    /// Set of (validator, round) pairs where a block was actually received.
    received: HashMap<Hash256, HashSet<u64>>,
}

impl WithholdingDetector {
    /// Create a new detector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark that a validator was expected to produce a block in the given round.
    pub fn report_expected(&mut self, validator: Hash256, round: u64) {
        self.expected
            .entry(validator)
            .or_default()
            .insert(round);
    }

    /// Mark that a validator actually published a block in the given round.
    pub fn report_received(&mut self, validator: Hash256, round: u64) {
        self.received
            .entry(validator)
            .or_default()
            .insert(round);
    }

    /// Scan the last `window` rounds for validators with high withholding scores.
    ///
    /// A withholding score > 0.5 over the window triggers a report. The score is
    /// calculated as `missing_count / expected_count` for rounds within the window.
    pub fn detect_withholding(&self, window: u64) -> Vec<WithholdingReport> {
        let mut reports = Vec::new();

        // Determine the highest round we've seen across all expectations.
        let max_round = self
            .expected
            .values()
            .flat_map(|rounds| rounds.iter().copied())
            .max()
            .unwrap_or(0);

        let window_start = max_round.saturating_sub(window);

        for (validator, expected_rounds) in &self.expected {
            let received_rounds = self.received.get(validator);

            // Only consider rounds within the window.
            let expected_in_window: Vec<u64> = expected_rounds
                .iter()
                .copied()
                .filter(|&r| r > window_start && r <= max_round)
                .collect();

            let total_expected = expected_in_window.len() as u64;
            if total_expected == 0 {
                continue;
            }

            let missing: Vec<u64> = expected_in_window
                .iter()
                .copied()
                .filter(|r| {
                    received_rounds
                        .map(|set| !set.contains(r))
                        .unwrap_or(true)
                })
                .collect();

            let withholding_score = missing.len() as f64 / total_expected as f64;

            if withholding_score > 0.5 {
                warn!(
                    validator = %validator,
                    score = withholding_score,
                    missing = missing.len(),
                    expected = total_expected,
                    "Withholding detected"
                );
                reports.push(WithholdingReport {
                    validator: *validator,
                    missing_rounds: missing,
                    total_expected,
                    withholding_score,
                });
            }
        }

        reports
    }

    /// Prune records older than `before_round` to bound memory usage.
    pub fn prune(&mut self, before_round: u64) {
        for rounds in self.expected.values_mut() {
            rounds.retain(|&r| r >= before_round);
        }
        for rounds in self.received.values_mut() {
            rounds.retain(|&r| r >= before_round);
        }
        self.expected.retain(|_, rounds| !rounds.is_empty());
        self.received.retain(|_, rounds| !rounds.is_empty());
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// §2  Long-Range Attack Prevention (#28)
// ══════════════════════════════════════════════════════════════════════════════

/// Interval (in rounds) between checkpoints.
pub const CHECKPOINT_INTERVAL: u64 = 1000;

/// A validator's signature on a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorSignature {
    /// The signing validator's address.
    pub validator: Hash256,
    /// The raw signature bytes (Ed25519).
    pub signature: Vec<u8>,
}

/// A finalized checkpoint anchoring the chain at a specific round/height.
///
/// Checkpoints are produced every `CHECKPOINT_INTERVAL` rounds and signed by a
/// quorum of validators. Any chain that diverges before the latest checkpoint
/// is rejected, preventing long-range history rewrite attacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Hash of the block at this checkpoint.
    pub block_hash: [u8; 32],
    /// Consensus round of this checkpoint.
    pub round: u64,
    /// Block height at this checkpoint.
    pub height: u64,
    /// State root hash at this checkpoint.
    pub state_root: [u8; 32],
    /// Unix timestamp when the checkpoint was created.
    pub timestamp: u64,
    /// Quorum of validator signatures attesting to this checkpoint.
    pub signatures: Vec<ValidatorSignature>,
}

/// Lightweight reference to a block, used for chain verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRef {
    /// Hash of the block.
    pub hash: [u8; 32],
    /// Round the block belongs to.
    pub round: u64,
    /// Block height.
    pub height: u64,
}

/// Registry of finalized checkpoints for long-range attack prevention.
///
/// Nodes joining the network (or syncing after a long absence) use this registry
/// to verify that the chain they are downloading is consistent with the checkpoints
/// known to the honest majority.
#[derive(Debug, Default)]
pub struct CheckpointRegistry {
    /// Checkpoints indexed by round number.
    checkpoints: HashMap<u64, Checkpoint>,
    /// The highest round for which we have a checkpoint.
    latest_round: u64,
}

impl CheckpointRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new checkpoint.
    ///
    /// The checkpoint's round must be greater than the current latest round.
    /// In production the signatures would be verified against the validator set;
    /// here we store the checkpoint and advance the latest-round pointer.
    pub fn add_checkpoint(&mut self, checkpoint: Checkpoint) -> bool {
        if checkpoint.round <= self.latest_round && self.latest_round > 0 {
            warn!(
                round = checkpoint.round,
                latest = self.latest_round,
                "Rejecting checkpoint at or before latest round"
            );
            return false;
        }
        let round = checkpoint.round;
        self.checkpoints.insert(round, checkpoint);
        self.latest_round = round;
        true
    }

    /// Return the most recent checkpoint, if any.
    pub fn latest_checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoints.get(&self.latest_round)
    }

    /// Verify that a chain of blocks is consistent with all known checkpoints.
    ///
    /// For each checkpoint whose round falls within the chain range, the chain
    /// must contain a block at that round with a matching hash. Returns `false`
    /// if any checkpoint is violated.
    pub fn verify_chain_against_checkpoints(&self, chain: &[BlockRef]) -> bool {
        if chain.is_empty() {
            return true;
        }

        // Build a quick lookup: round -> block hash.
        let round_to_hash: HashMap<u64, [u8; 32]> = chain
            .iter()
            .map(|b| (b.round, b.hash))
            .collect();

        let chain_min = chain.iter().map(|b| b.round).min().unwrap_or(0);
        let chain_max = chain.iter().map(|b| b.round).max().unwrap_or(0);

        for (round, checkpoint) in &self.checkpoints {
            if *round >= chain_min && *round <= chain_max {
                match round_to_hash.get(round) {
                    Some(hash) if *hash == checkpoint.block_hash => {
                        // Consistent — continue checking.
                    }
                    Some(_) => {
                        warn!(
                            round = round,
                            "Chain diverges from checkpoint"
                        );
                        return false;
                    }
                    None => {
                        // Chain doesn't include this round at all — suspicious but
                        // may happen with sparse block refs. We treat missing as
                        // invalid since the chain should cover checkpoint rounds.
                        warn!(
                            round = round,
                            "Chain missing block at checkpoint round"
                        );
                        return false;
                    }
                }
            }
        }

        true
    }

    /// Determine whether a proposed fork point is valid.
    ///
    /// A fork is rejected if it branches off before the latest checkpoint, since
    /// that would require rewriting finalized history.
    pub fn is_valid_fork_point(&self, round: u64) -> bool {
        if self.latest_round == 0 {
            // No checkpoints yet — any fork point is acceptable.
            return true;
        }
        // Fork at or after the checkpoint round is valid. In DAG consensus,
        // multiple blocks can exist at the same round (different validators).
        // The checkpoint finalizes the committed ordering, not a single block.
        round >= self.latest_round
    }

    /// Return the checkpoint at a specific round, if it exists.
    pub fn get_checkpoint(&self, round: u64) -> Option<&Checkpoint> {
        self.checkpoints.get(&round)
    }

    /// Total number of registered checkpoints.
    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// §3  Nothing-at-Stake Mitigation (#29)
// ══════════════════════════════════════════════════════════════════════════════

/// Evidence that a validator voted for two different blocks in the same round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoubleVoteEvidence {
    /// The offending validator.
    pub validator: Hash256,
    /// The round in which the double vote occurred.
    pub round: u64,
    /// Hash of the first block voted for.
    pub vote1_hash: [u8; 32],
    /// Hash of the second (conflicting) block voted for.
    pub vote2_hash: [u8; 32],
}

/// Categories of slashable validator offenses with graduated penalties.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlashableOffense {
    /// Voting for two different blocks in the same round (100% slash).
    DoubleVote,
    /// Withholding a block the validator was expected to produce (10% slash).
    WithholdingBlock,
    /// Producing an invalid block (50% slash).
    InvalidBlock,
    /// Sending contradictory messages to different peers (100% slash).
    Equivocation,
}

impl SlashableOffense {
    /// Human-readable label for logging and reporting.
    pub fn label(&self) -> &'static str {
        match self {
            Self::DoubleVote => "double_vote",
            Self::WithholdingBlock => "withholding_block",
            Self::InvalidBlock => "invalid_block",
            Self::Equivocation => "equivocation",
        }
    }
}

/// A recorded penalty applied to a validator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PenaltyRecord {
    /// The penalized validator.
    pub validator: Hash256,
    /// The offense category.
    pub offense: SlashableOffense,
    /// Amount of stake slashed.
    pub slash_amount: u64,
    /// Round in which the offense was detected.
    pub round: u64,
    /// Unix timestamp of the penalty.
    pub timestamp: u64,
}

/// Calculate the slash amount for a given offense and stake.
///
/// Graduated slashing schedule:
/// - `DoubleVote`: 100% of stake (most severe — direct safety violation)
/// - `Equivocation`: 100% of stake (equivalent severity to double vote)
/// - `InvalidBlock`: 50% of stake (attempted protocol violation)
/// - `WithholdingBlock`: 10% of stake (liveness degradation)
pub fn calculate_slash_amount(offense: &SlashableOffense, stake: u64) -> u64 {
    match offense {
        SlashableOffense::DoubleVote => stake,                  // 100%
        SlashableOffense::Equivocation => stake,                // 100%
        SlashableOffense::InvalidBlock => stake / 2,            // 50%
        SlashableOffense::WithholdingBlock => stake / 10,       // 10%
    }
}

/// Monitors validator voting behavior across forks to detect nothing-at-stake
/// attacks (double voting).
///
/// In proof-of-stake systems, validators have no natural cost to voting on
/// multiple forks simultaneously. This tracker records all votes and flags
/// any validator that votes for different blocks in the same round, producing
/// cryptographic evidence that can be submitted for slashing.
#[derive(Debug, Default)]
pub struct StakeTracker {
    /// Map of (validator, round) -> set of block hashes voted for.
    votes: HashMap<(Hash256, u64), HashSet<[u8; 32]>>,
    /// Accumulated penalty records.
    penalties: Vec<PenaltyRecord>,
}

impl StakeTracker {
    /// Create a new tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a validator's vote for a specific block in a given round.
    pub fn report_vote(&mut self, validator: Hash256, round: u64, block_hash: [u8; 32]) {
        self.votes
            .entry((validator, round))
            .or_default()
            .insert(block_hash);
    }

    /// Check for double voting in a specific round.
    ///
    /// Returns evidence for every validator that voted for more than one distinct
    /// block hash in the given round.
    pub fn detect_double_voting(&self, round: u64) -> Vec<DoubleVoteEvidence> {
        let mut evidence = Vec::new();

        for ((validator, r), hashes) in &self.votes {
            if *r != round {
                continue;
            }
            if hashes.len() >= 2 {
                // Take the first two distinct hashes as evidence.
                let mut iter = hashes.iter();
                let vote1 = *iter.next().unwrap();
                let vote2 = *iter.next().unwrap();

                warn!(
                    validator = %validator,
                    round = round,
                    "Double vote detected"
                );

                evidence.push(DoubleVoteEvidence {
                    validator: *validator,
                    round,
                    vote1_hash: vote1,
                    vote2_hash: vote2,
                });
            }
        }

        evidence
    }

    /// Record a penalty against a validator.
    pub fn record_penalty(&mut self, record: PenaltyRecord) {
        self.penalties.push(record);
    }

    /// Return all penalties for a given validator.
    pub fn penalties_for(&self, validator: &Hash256) -> Vec<&PenaltyRecord> {
        self.penalties
            .iter()
            .filter(|p| p.validator == *validator)
            .collect()
    }

    /// Total amount slashed from a validator across all penalties.
    pub fn total_slashed(&self, validator: &Hash256) -> u64 {
        self.penalties
            .iter()
            .filter(|p| p.validator == *validator)
            .map(|p| p.slash_amount)
            .sum()
    }

    /// All recorded penalties.
    pub fn all_penalties(&self) -> &[PenaltyRecord] {
        &self.penalties
    }

    /// Prune vote records for rounds before `before_round`.
    pub fn prune_votes(&mut self, before_round: u64) {
        self.votes.retain(|&(_, r), _| r >= before_round);
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    /// Deterministic test address from a single byte.
    fn test_addr(n: u8) -> Hash256 {
        hash_bytes(&[n])
    }

    /// Deterministic 32-byte block hash from a single byte.
    fn test_block_hash(n: u8) -> [u8; 32] {
        *hash_bytes(&[n]).as_bytes()
    }

    /// Create a test checkpoint at the given round.
    fn make_checkpoint(round: u64, block_hash: [u8; 32]) -> Checkpoint {
        Checkpoint {
            block_hash,
            round,
            height: round, // For simplicity, height == round in tests.
            state_root: [0u8; 32],
            timestamp: 1_700_000_000 + round,
            signatures: vec![ValidatorSignature {
                validator: test_addr(0),
                signature: vec![0u8; 64],
            }],
        }
    }

    // ── Withholding Detection Tests ─────────────────────────────────────────

    #[test]
    fn withholding_no_reports_when_all_blocks_received() {
        let mut detector = WithholdingDetector::new();
        let v = test_addr(1);

        for round in 1..=100 {
            detector.report_expected(v, round);
            detector.report_received(v, round);
        }

        let reports = detector.detect_withholding(100);
        assert!(reports.is_empty(), "No withholding should be detected");
    }

    #[test]
    fn withholding_detects_missing_blocks() {
        let mut detector = WithholdingDetector::new();
        let v = test_addr(2);

        // Validator is expected for 100 rounds but only delivers 30.
        for round in 1..=100 {
            detector.report_expected(v, round);
            if round <= 30 {
                detector.report_received(v, round);
            }
        }

        let reports = detector.detect_withholding(100);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].validator, v);
        assert_eq!(reports[0].total_expected, 100);
        assert_eq!(reports[0].missing_rounds.len(), 70);
        assert!(reports[0].withholding_score > 0.5);
    }

    #[test]
    fn withholding_respects_window() {
        let mut detector = WithholdingDetector::new();
        let v = test_addr(3);

        // Rounds 1-50: all missing. Rounds 51-100: all present.
        for round in 1..=100 {
            detector.report_expected(v, round);
            if round > 50 {
                detector.report_received(v, round);
            }
        }

        // Window of 50 covers rounds 51-100 — all present, no withholding.
        let reports = detector.detect_withholding(50);
        assert!(reports.is_empty(), "Recent window should show no withholding");

        // Window of 100 covers everything — 50 missing out of 100 = 0.5, not > 0.5.
        let reports_full = detector.detect_withholding(100);
        assert!(
            reports_full.is_empty(),
            "Score of exactly 0.5 should not trigger (need > 0.5)"
        );
    }

    #[test]
    fn withholding_borderline_just_above_threshold() {
        let mut detector = WithholdingDetector::new();
        let v = test_addr(4);

        // 100 expected, 49 received => 51 missing => score = 0.51 > 0.5.
        for round in 1..=100 {
            detector.report_expected(v, round);
            if round <= 49 {
                detector.report_received(v, round);
            }
        }

        let reports = detector.detect_withholding(100);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].withholding_score > 0.5);
    }

    #[test]
    fn withholding_multiple_validators() {
        let mut detector = WithholdingDetector::new();
        let good = test_addr(10);
        let bad = test_addr(11);

        for round in 1..=100 {
            detector.report_expected(good, round);
            detector.report_received(good, round);
            detector.report_expected(bad, round);
            // bad never delivers
        }

        let reports = detector.detect_withholding(100);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].validator, bad);
        assert!((reports[0].withholding_score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn withholding_prune_removes_old_data() {
        let mut detector = WithholdingDetector::new();
        let v = test_addr(5);

        for round in 1..=50 {
            detector.report_expected(v, round);
        }
        assert!(!detector.expected.is_empty());

        detector.prune(51);
        assert!(detector.expected.is_empty());
    }

    // ── Checkpoint Registry Tests ───────────────────────────────────────────

    #[test]
    fn checkpoint_add_and_retrieve() {
        let mut registry = CheckpointRegistry::new();
        let cp = make_checkpoint(1000, test_block_hash(1));
        assert!(registry.add_checkpoint(cp.clone()));
        assert_eq!(registry.latest_checkpoint(), Some(&cp));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn checkpoint_rejects_old_round() {
        let mut registry = CheckpointRegistry::new();
        let cp1 = make_checkpoint(2000, test_block_hash(1));
        let cp_old = make_checkpoint(1000, test_block_hash(2));

        assert!(registry.add_checkpoint(cp1));
        assert!(!registry.add_checkpoint(cp_old), "Should reject older checkpoint");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn checkpoint_verify_chain_consistent() {
        let mut registry = CheckpointRegistry::new();
        let bh = test_block_hash(42);
        registry.add_checkpoint(make_checkpoint(1000, bh));

        let chain = vec![
            BlockRef { hash: test_block_hash(0), round: 500, height: 500 },
            BlockRef { hash: bh, round: 1000, height: 1000 },
            BlockRef { hash: test_block_hash(1), round: 1500, height: 1500 },
        ];

        assert!(registry.verify_chain_against_checkpoints(&chain));
    }

    #[test]
    fn checkpoint_verify_chain_divergent() {
        let mut registry = CheckpointRegistry::new();
        registry.add_checkpoint(make_checkpoint(1000, test_block_hash(42)));

        // Chain has a different hash at round 1000.
        let chain = vec![
            BlockRef { hash: test_block_hash(99), round: 1000, height: 1000 },
        ];

        assert!(!registry.verify_chain_against_checkpoints(&chain));
    }

    #[test]
    fn checkpoint_verify_chain_missing_round() {
        let mut registry = CheckpointRegistry::new();
        registry.add_checkpoint(make_checkpoint(1000, test_block_hash(42)));

        // Chain spans the checkpoint round but has no block at round 1000.
        let chain = vec![
            BlockRef { hash: test_block_hash(0), round: 999, height: 999 },
            BlockRef { hash: test_block_hash(1), round: 1001, height: 1001 },
        ];

        assert!(!registry.verify_chain_against_checkpoints(&chain));
    }

    #[test]
    fn checkpoint_fork_point_validity() {
        let mut registry = CheckpointRegistry::new();

        // No checkpoints — any fork point is fine.
        assert!(registry.is_valid_fork_point(0));
        assert!(registry.is_valid_fork_point(500));

        registry.add_checkpoint(make_checkpoint(1000, test_block_hash(1)));

        // Fork before checkpoint is rejected.
        assert!(!registry.is_valid_fork_point(999));
        assert!(!registry.is_valid_fork_point(0));

        // Fork at or after checkpoint is accepted.
        assert!(registry.is_valid_fork_point(1000));
        assert!(registry.is_valid_fork_point(1500));
    }

    #[test]
    fn checkpoint_empty_chain_is_valid() {
        let mut registry = CheckpointRegistry::new();
        registry.add_checkpoint(make_checkpoint(1000, test_block_hash(1)));
        assert!(registry.verify_chain_against_checkpoints(&[]));
    }

    // ── Nothing-at-Stake / Slashing Tests ───────────────────────────────────

    #[test]
    fn stake_no_double_vote_with_single_vote() {
        let mut tracker = StakeTracker::new();
        let v = test_addr(1);
        tracker.report_vote(v, 10, test_block_hash(1));

        let evidence = tracker.detect_double_voting(10);
        assert!(evidence.is_empty());
    }

    #[test]
    fn stake_detects_double_vote() {
        let mut tracker = StakeTracker::new();
        let v = test_addr(1);
        tracker.report_vote(v, 10, test_block_hash(1));
        tracker.report_vote(v, 10, test_block_hash(2));

        let evidence = tracker.detect_double_voting(10);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].validator, v);
        assert_eq!(evidence[0].round, 10);
        assert_ne!(evidence[0].vote1_hash, evidence[0].vote2_hash);
    }

    #[test]
    fn stake_double_vote_only_in_queried_round() {
        let mut tracker = StakeTracker::new();
        let v = test_addr(1);
        // Double vote in round 10.
        tracker.report_vote(v, 10, test_block_hash(1));
        tracker.report_vote(v, 10, test_block_hash(2));
        // Single vote in round 11.
        tracker.report_vote(v, 11, test_block_hash(3));

        assert_eq!(tracker.detect_double_voting(10).len(), 1);
        assert!(tracker.detect_double_voting(11).is_empty());
    }

    #[test]
    fn stake_slash_amounts_graduated() {
        let stake = 1_000_000u64;

        assert_eq!(calculate_slash_amount(&SlashableOffense::DoubleVote, stake), 1_000_000);
        assert_eq!(calculate_slash_amount(&SlashableOffense::Equivocation, stake), 1_000_000);
        assert_eq!(calculate_slash_amount(&SlashableOffense::InvalidBlock, stake), 500_000);
        assert_eq!(calculate_slash_amount(&SlashableOffense::WithholdingBlock, stake), 100_000);
    }

    #[test]
    fn stake_penalty_recording_and_totals() {
        let mut tracker = StakeTracker::new();
        let v = test_addr(1);

        tracker.record_penalty(PenaltyRecord {
            validator: v,
            offense: SlashableOffense::WithholdingBlock,
            slash_amount: 100_000,
            round: 50,
            timestamp: 1_700_000_050,
        });
        tracker.record_penalty(PenaltyRecord {
            validator: v,
            offense: SlashableOffense::InvalidBlock,
            slash_amount: 500_000,
            round: 75,
            timestamp: 1_700_000_075,
        });

        assert_eq!(tracker.penalties_for(&v).len(), 2);
        assert_eq!(tracker.total_slashed(&v), 600_000);

        // Other validator has no penalties.
        let v2 = test_addr(2);
        assert_eq!(tracker.penalties_for(&v2).len(), 0);
        assert_eq!(tracker.total_slashed(&v2), 0);
    }

    #[test]
    fn stake_prune_votes_removes_old_rounds() {
        let mut tracker = StakeTracker::new();
        let v = test_addr(1);
        tracker.report_vote(v, 5, test_block_hash(1));
        tracker.report_vote(v, 5, test_block_hash(2));
        tracker.report_vote(v, 15, test_block_hash(3));

        tracker.prune_votes(10);

        // Round 5 should be gone.
        assert!(tracker.detect_double_voting(5).is_empty());
        // Round 15 should still be there.
        assert!(tracker.detect_double_voting(15).is_empty()); // Single vote, no double.
    }

    #[test]
    fn stake_offense_labels() {
        assert_eq!(SlashableOffense::DoubleVote.label(), "double_vote");
        assert_eq!(SlashableOffense::WithholdingBlock.label(), "withholding_block");
        assert_eq!(SlashableOffense::InvalidBlock.label(), "invalid_block");
        assert_eq!(SlashableOffense::Equivocation.label(), "equivocation");
    }

    #[test]
    fn stake_slash_zero_stake() {
        // Edge case: slashing zero stake should produce zero penalty.
        assert_eq!(calculate_slash_amount(&SlashableOffense::DoubleVote, 0), 0);
        assert_eq!(calculate_slash_amount(&SlashableOffense::InvalidBlock, 0), 0);
        assert_eq!(calculate_slash_amount(&SlashableOffense::WithholdingBlock, 0), 0);
    }
}
