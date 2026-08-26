//! Security Detection Modules for ARC Chain Consensus
//!
//! This module addresses three critical consensus security concerns:
//!
//! 1. **Block Withholding Detection** (#27): Identifies validators who consistently
//!    fail to publish blocks they were expected to produce, indicating a withholding
//!    attack that can degrade network liveness.
//!
//! 2. **Long-Range Checkpoint Verification** (#28): Stores only checkpoints
//!    carrying real strict-supermajority certificates from an externally
//!    trusted validator set. Production currently leaves this registry empty
//!    until canonical state-root signature collection is implemented; an empty
//!    registry is not represented as long-range protection.
//!
//! 3. **Nothing-at-Stake Mitigation** (#29): Detects double-voting across forks
//!    and enforces graduated slashing penalties to make equivocation economically
//!    irrational.

use crate::{StakeTier, ValidatorSet};
use arc_crypto::{Hash256, Signature};
use arc_types::strict_supermajority_threshold;
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
        self.expected.entry(validator).or_default().insert(round);
    }

    /// Mark that a validator actually published a block in the given round.
    pub fn report_received(&mut self, validator: Hash256, round: u64) {
        self.received.entry(validator).or_default().insert(round);
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
                .filter(|r| received_rounds.map(|set| !set.contains(r)).unwrap_or(true))
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

/// Candidate interval for collecting a checkpoint certificate. Reaching this
/// round alone never registers a trust anchor.
pub const CHECKPOINT_INTERVAL: u64 = 1000;

/// A validator's signature on a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorSignature {
    /// The signing validator's address.
    pub validator: Hash256,
    /// Ed25519 verifying key whose hash must equal `validator`.
    pub public_key: [u8; 32],
    /// Fixed-size Ed25519 signature split for portable serde array support.
    pub signature_halves: [[u8; 32]; 2],
}

impl ValidatorSignature {
    /// Convert only canonical 64-byte Ed25519 signatures. Checkpoint trust does
    /// not accept recoverable or post-quantum transaction signature variants.
    pub fn from_ed25519_signature(validator: Hash256, signature: Signature) -> Option<Self> {
        let Signature::Ed25519 {
            public_key,
            signature,
        } = signature
        else {
            return None;
        };
        let bytes: [u8; 64] = signature.try_into().ok()?;
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        first.copy_from_slice(&bytes[..32]);
        second.copy_from_slice(&bytes[32..]);
        Some(Self {
            validator,
            public_key,
            signature_halves: [first, second],
        })
    }

    fn as_signature(&self) -> Signature {
        let mut signature = Vec::with_capacity(64);
        signature.extend_from_slice(&self.signature_halves[0]);
        signature.extend_from_slice(&self.signature_halves[1]);
        Signature::Ed25519 {
            public_key: self.public_key,
            signature,
        }
    }
}

/// A finalized checkpoint anchoring the chain at a specific round/height.
///
/// A checkpoint becomes trusted only after [`CheckpointRegistry::add_checkpoint`]
/// verifies its real validator certificate against an externally trusted set.
/// Merely reaching `CHECKPOINT_INTERVAL` does not create a trust anchor.
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

impl Checkpoint {
    /// Canonical, domain-separated checkpoint transcript. Signatures are
    /// excluded so every validator signs the same bounded message.
    pub fn signing_hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-consensus-finalized-checkpoint-v1");
        hasher.update(&self.block_hash);
        hasher.update(&self.round.to_le_bytes());
        hasher.update(&self.height.to_le_bytes());
        hasher.update(&self.state_root);
        hasher.update(&self.timestamp.to_le_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Verify this checkpoint only against an externally trusted active
    /// validator set. Checkpoint-provided identities never declare stake.
    pub fn verify_against_trusted_validator_set(
        &self,
        trusted: &ValidatorSet,
    ) -> Result<(), CheckpointError> {
        let (active_stakes, total_stake) = trusted_validator_stakes(trusted)?;
        if self.signatures.is_empty() {
            return Err(CheckpointError::EmptySignatures);
        }
        if self.signatures.len() > active_stakes.len() {
            return Err(CheckpointError::TooManySignatures {
                have: self.signatures.len(),
                maximum: active_stakes.len(),
            });
        }

        let active_identity_count = u64::try_from(active_stakes.len()).map_err(|_| {
            CheckpointError::InvalidTrustedValidatorSet(
                "active validator count exceeds u64::MAX".to_string(),
            )
        })?;
        let required_identities = strict_supermajority_threshold(active_identity_count);
        let required_identities = usize::try_from(required_identities).map_err(|_| {
            CheckpointError::InvalidTrustedValidatorSet(
                "identity threshold exceeds usize::MAX".to_string(),
            )
        })?;
        let signing_hash = self.signing_hash();
        let mut seen = HashSet::with_capacity(self.signatures.len());
        let mut approved_stake = 0u64;

        for approval in &self.signatures {
            if !seen.insert(approval.validator) {
                return Err(CheckpointError::DuplicateSigner(approval.validator));
            }
            let Some(stake) = active_stakes.get(&approval.validator) else {
                return Err(CheckpointError::UnknownSigner(approval.validator));
            };
            approval
                .as_signature()
                .verify(&signing_hash, &approval.validator)
                .map_err(|_| CheckpointError::InvalidSignature(approval.validator))?;
            approved_stake = approved_stake.checked_add(*stake).ok_or_else(|| {
                CheckpointError::InvalidTrustedValidatorSet(
                    "approved validator stake exceeds u64::MAX".to_string(),
                )
            })?;
        }

        if seen.len() < required_identities {
            return Err(CheckpointError::InsufficientIdentities {
                have: seen.len(),
                need: required_identities,
            });
        }
        let required_stake = strict_supermajority_threshold(total_stake);
        if approved_stake < required_stake {
            return Err(CheckpointError::InsufficientStake {
                have: approved_stake,
                need: required_stake,
            });
        }
        Ok(())
    }
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

/// Why an untrusted checkpoint failed the trust boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    #[error("trusted validator set is empty")]
    EmptyTrustedValidatorSet,
    #[error("invalid trusted validator set: {0}")]
    InvalidTrustedValidatorSet(String),
    #[error("checkpoint round {round} is not newer than trusted round {latest}")]
    StaleRound { round: u64, latest: u64 },
    #[error("checkpoint round zero is reserved for the uninitialized registry")]
    ZeroRound,
    #[error("checkpoint has no validator signatures")]
    EmptySignatures,
    #[error("checkpoint has {have} signatures but trusted set has only {maximum} validators")]
    TooManySignatures { have: usize, maximum: usize },
    #[error("duplicate checkpoint signer {0}")]
    DuplicateSigner(Hash256),
    #[error("checkpoint signer {0} is not in the trusted active validator set")]
    UnknownSigner(Hash256),
    #[error("invalid Ed25519 checkpoint signature from {0}")]
    InvalidSignature(Hash256),
    #[error("checkpoint has {have} distinct validator identities; requires {need}")]
    InsufficientIdentities { have: usize, need: usize },
    #[error("checkpoint has {have} signed stake; requires {need}")]
    InsufficientStake { have: u64, need: u64 },
}

fn trusted_validator_stakes(
    trusted: &ValidatorSet,
) -> Result<(HashMap<Hash256, u64>, u64), CheckpointError> {
    if trusted.validators.is_empty() {
        return Err(CheckpointError::EmptyTrustedValidatorSet);
    }
    let mut stakes = HashMap::with_capacity(trusted.validators.len());
    let mut total = 0u64;
    for validator in &trusted.validators {
        if StakeTier::from_stake(validator.stake) != Some(validator.tier) {
            return Err(CheckpointError::InvalidTrustedValidatorSet(format!(
                "validator {} has inactive stake or inconsistent tier",
                validator.address
            )));
        }
        if stakes.insert(validator.address, validator.stake).is_some() {
            return Err(CheckpointError::InvalidTrustedValidatorSet(format!(
                "duplicate validator {}",
                validator.address
            )));
        }
        total = total.checked_add(validator.stake).ok_or_else(|| {
            CheckpointError::InvalidTrustedValidatorSet(
                "total validator stake exceeds u64::MAX".to_string(),
            )
        })?;
    }
    if trusted.total_stake != total {
        return Err(CheckpointError::InvalidTrustedValidatorSet(format!(
            "cached total stake {} does not match recomputed {}",
            trusted.total_stake, total
        )));
    }
    let expected_quorum = strict_supermajority_threshold(total);
    if trusted.quorum != expected_quorum {
        return Err(CheckpointError::InvalidTrustedValidatorSet(format!(
            "cached quorum {} does not match strict threshold {}",
            trusted.quorum, expected_quorum
        )));
    }
    Ok((stakes, total))
}

/// Registry of authenticated finalized checkpoints.
///
/// Nodes joining the network (or syncing after a long absence) use this registry
/// to verify that the chain they are downloading is consistent with the checkpoints
/// known to the honest majority. An empty registry provides no long-range trust
/// and its verification helpers deliberately fail closed.
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

    /// Verify and register a new trusted checkpoint.
    ///
    /// The checkpoint must be newer than the current anchor and carry real,
    /// unique Ed25519 signatures from strict supermajorities of both identities
    /// and active stake in the supplied trusted validator set.
    pub fn add_checkpoint(
        &mut self,
        checkpoint: Checkpoint,
        trusted: &ValidatorSet,
    ) -> Result<(), CheckpointError> {
        if checkpoint.round == 0 {
            return Err(CheckpointError::ZeroRound);
        }
        if checkpoint.round <= self.latest_round && self.latest_round > 0 {
            warn!(
                round = checkpoint.round,
                latest = self.latest_round,
                "Rejecting checkpoint at or before latest round"
            );
            return Err(CheckpointError::StaleRound {
                round: checkpoint.round,
                latest: self.latest_round,
            });
        }
        checkpoint.verify_against_trusted_validator_set(trusted)?;
        let round = checkpoint.round;
        self.checkpoints.insert(round, checkpoint);
        self.latest_round = round;
        Ok(())
    }

    /// Return the most recent checkpoint, if any.
    pub fn latest_checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoints.get(&self.latest_round)
    }

    /// Verify that a chain of blocks is consistent with all known checkpoints.
    ///
    /// When a trusted checkpoint exists, the supplied chain must explicitly
    /// contain the latest anchor with matching round, height, and hash. A chain
    /// that begins after the checkpoint without carrying the anchor proves
    /// nothing about its history and therefore fails closed.
    pub fn verify_chain_against_checkpoints(&self, chain: &[BlockRef]) -> bool {
        let Some(latest) = self.latest_checkpoint() else {
            warn!("Cannot verify a chain without a trusted checkpoint anchor");
            return false;
        };
        if chain.is_empty() {
            warn!(
                round = latest.round,
                "Cannot verify an empty chain against a trusted checkpoint"
            );
            return false;
        }

        let mut by_round: HashMap<u64, &BlockRef> = HashMap::with_capacity(chain.len());
        for block in chain {
            if let Some(previous) = by_round.insert(block.round, block)
                && (previous.hash != block.hash || previous.height != block.height)
            {
                warn!(
                    round = block.round,
                    "Chain has conflicting block references"
                );
                return false;
            }
        }

        for (round, checkpoint) in &self.checkpoints {
            match by_round.get(round) {
                Some(block)
                    if block.hash == checkpoint.block_hash && block.height == checkpoint.height => {
                }
                Some(_) => {
                    warn!(round = round, "Chain diverges from checkpoint");
                    return false;
                }
                None => {
                    warn!(round = round, "Chain omits trusted checkpoint anchor");
                    return false;
                }
            }
        }

        true
    }

    /// Determine whether a proposed fork point is valid.
    ///
    /// A fork is rejected unless it branches strictly after the latest
    /// checkpoint. This helper has no block-hash argument, so equality cannot
    /// prove descent from the exact trusted anchor.
    pub fn is_valid_fork_point(&self, round: u64) -> bool {
        if self.latest_round == 0 {
            // No authenticated anchor means this API cannot vouch for a fork.
            return false;
        }
        // Without a block hash, a fork advertised at the checkpoint's own
        // round cannot prove it descends from the exact anchor. Only a fork
        // strictly after the trusted checkpoint is safe here.
        round > self.latest_round
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
/// - `DoubleVote`: 100% of stake (most severe - direct safety violation)
/// - `Equivocation`: 100% of stake (equivalent severity to double vote)
/// - `InvalidBlock`: 50% of stake (attempted protocol violation)
/// - `WithholdingBlock`: 10% of stake (liveness degradation)
pub fn calculate_slash_amount(offense: &SlashableOffense, stake: u64) -> u64 {
    match offense {
        SlashableOffense::DoubleVote => stake,            // 100%
        SlashableOffense::Equivocation => stake,          // 100%
        SlashableOffense::InvalidBlock => stake / 2,      // 50%
        SlashableOffense::WithholdingBlock => stake / 10, // 10%
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
                let (vote1, vote2) = match (iter.next(), iter.next()) {
                    (Some(&v1), Some(&v2)) => (v1, v2),
                    _ => continue,
                };

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
    use crate::{STAKE_ARC, Validator};
    use arc_crypto::{KeyPair, hash_bytes};

    /// Deterministic test address from a single byte.
    fn test_addr(n: u8) -> Hash256 {
        hash_bytes(&[n])
    }

    /// Deterministic 32-byte block hash from a single byte.
    fn test_block_hash(n: u8) -> [u8; 32] {
        *hash_bytes(&[n]).as_bytes()
    }

    /// Create an unsigned test checkpoint at the given round.
    fn make_checkpoint(round: u64, block_hash: [u8; 32]) -> Checkpoint {
        Checkpoint {
            block_hash,
            round,
            height: round, // For simplicity, height == round in tests.
            state_root: [0u8; 32],
            timestamp: 1_700_000_000 + round,
            signatures: Vec::new(),
        }
    }

    fn validator_fixture(stakes: &[u64]) -> (Vec<KeyPair>, ValidatorSet) {
        let keys: Vec<_> = stakes.iter().map(|_| KeyPair::generate_ed25519()).collect();
        let validators = keys
            .iter()
            .zip(stakes)
            .enumerate()
            .map(|(index, (key, stake))| {
                Validator::new(key.address(), *stake, index as u16).expect("active validator")
            })
            .collect();
        (keys, ValidatorSet::new(validators, 1))
    }

    fn sign_checkpoint(checkpoint: &mut Checkpoint, signers: &[&KeyPair]) {
        let signing_hash = checkpoint.signing_hash();
        checkpoint.signatures = signers
            .iter()
            .map(|signer| {
                ValidatorSignature::from_ed25519_signature(
                    signer.address(),
                    signer.sign(&signing_hash).expect("checkpoint signature"),
                )
                .expect("Ed25519 fixture")
            })
            .collect();
    }

    fn signed_checkpoint(round: u64, block_hash: [u8; 32], keys: &[KeyPair]) -> Checkpoint {
        let mut checkpoint = make_checkpoint(round, block_hash);
        sign_checkpoint(&mut checkpoint, &[&keys[0], &keys[1], &keys[2]]);
        checkpoint
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

        // Window of 50 covers rounds 51-100 - all present, no withholding.
        let reports = detector.detect_withholding(50);
        assert!(
            reports.is_empty(),
            "Recent window should show no withholding"
        );

        // Window of 100 covers everything - 50 missing out of 100 = 0.5, not > 0.5.
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
    fn checkpoint_add_and_retrieve_requires_real_supermajority_signatures() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut registry = CheckpointRegistry::new();
        let cp = signed_checkpoint(1000, test_block_hash(1), &keys);
        registry
            .add_checkpoint(cp.clone(), &trusted)
            .expect("3 of 4 valid equal-stake validators");
        assert_eq!(registry.latest_checkpoint(), Some(&cp));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn checkpoint_rejects_old_round() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut registry = CheckpointRegistry::new();
        let cp1 = signed_checkpoint(2000, test_block_hash(1), &keys);
        let cp_old = signed_checkpoint(1000, test_block_hash(2), &keys);

        assert!(
            registry.add_checkpoint(cp1, &trusted).is_ok(),
            "new checkpoint should register"
        );
        assert!(matches!(
            registry.add_checkpoint(cp_old, &trusted),
            Err(CheckpointError::StaleRound { .. })
        ));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn checkpoint_rejects_empty_signatures() {
        let (_, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut registry = CheckpointRegistry::new();
        let error = registry
            .add_checkpoint(make_checkpoint(1000, test_block_hash(1)), &trusted)
            .expect_err("unsigned checkpoint must never become trusted");
        assert_eq!(error, CheckpointError::EmptySignatures);
        assert!(registry.is_empty());
    }

    #[test]
    fn checkpoint_registry_without_trusted_anchor_fails_closed() {
        let mut registry = CheckpointRegistry::new();
        assert!(!registry.verify_chain_against_checkpoints(&[BlockRef {
            hash: test_block_hash(1),
            round: 1,
            height: 1,
        }]));
        assert!(!registry.is_valid_fork_point(1));

        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let checkpoint = signed_checkpoint(0, test_block_hash(1), &keys);
        assert_eq!(
            registry.add_checkpoint(checkpoint, &trusted),
            Err(CheckpointError::ZeroRound)
        );
    }

    #[test]
    fn checkpoint_rejects_fake_ed25519_signature() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut checkpoint = signed_checkpoint(1000, test_block_hash(1), &keys);
        checkpoint.signatures[1].signature_halves[0][0] ^= 0x80;
        let mut registry = CheckpointRegistry::new();
        assert!(matches!(
            registry.add_checkpoint(checkpoint, &trusted),
            Err(CheckpointError::InvalidSignature(_))
        ));
    }

    #[test]
    fn checkpoint_rejects_duplicate_signer_identity() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut checkpoint = signed_checkpoint(1000, test_block_hash(1), &keys);
        checkpoint.signatures[2] = checkpoint.signatures[1].clone();
        let mut registry = CheckpointRegistry::new();
        assert!(matches!(
            registry.add_checkpoint(checkpoint, &trusted),
            Err(CheckpointError::DuplicateSigner(_))
        ));
    }

    #[test]
    fn checkpoint_rejects_unknown_signer_even_with_valid_signature() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let outsider = KeyPair::generate_ed25519();
        let mut checkpoint = make_checkpoint(1000, test_block_hash(1));
        sign_checkpoint(&mut checkpoint, &[&keys[0], &keys[1], &outsider]);
        let mut registry = CheckpointRegistry::new();
        assert_eq!(
            registry.add_checkpoint(checkpoint, &trusted),
            Err(CheckpointError::UnknownSigner(outsider.address()))
        );
    }

    #[test]
    fn checkpoint_rejects_exactly_two_thirds_identities() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 6]);
        let mut checkpoint = make_checkpoint(1000, test_block_hash(1));
        sign_checkpoint(&mut checkpoint, &[&keys[0], &keys[1], &keys[2], &keys[3]]);
        let mut registry = CheckpointRegistry::new();
        assert_eq!(
            registry.add_checkpoint(checkpoint, &trusted),
            Err(CheckpointError::InsufficientIdentities { have: 4, need: 5 })
        );
    }

    #[test]
    fn checkpoint_rejects_exactly_two_thirds_active_stake() {
        let unit = STAKE_ARC;
        let (keys, trusted) = validator_fixture(&[unit, unit, 4 * unit, 3 * unit]);
        let mut checkpoint = make_checkpoint(1000, test_block_hash(1));
        sign_checkpoint(&mut checkpoint, &[&keys[0], &keys[1], &keys[2]]);
        let mut registry = CheckpointRegistry::new();
        assert_eq!(
            registry.add_checkpoint(checkpoint, &trusted),
            Err(CheckpointError::InsufficientStake {
                have: 6 * unit,
                need: 6 * unit + 1,
            })
        );
    }

    #[test]
    fn checkpoint_rejects_forged_trusted_set_stake_cache() {
        let (keys, mut trusted) = validator_fixture(&[STAKE_ARC; 4]);
        trusted.total_stake = 1;
        trusted.quorum = 1;
        let checkpoint = signed_checkpoint(1000, test_block_hash(1), &keys);
        let mut registry = CheckpointRegistry::new();
        assert!(matches!(
            registry.add_checkpoint(checkpoint, &trusted),
            Err(CheckpointError::InvalidTrustedValidatorSet(_))
        ));
    }

    #[test]
    fn checkpoint_signatures_bind_every_checkpoint_field() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let checkpoint = signed_checkpoint(1000, test_block_hash(1), &keys);

        let mut mutations = Vec::new();
        let mut changed = checkpoint.clone();
        changed.block_hash = test_block_hash(2);
        mutations.push(changed);
        let mut changed = checkpoint.clone();
        changed.round += 1;
        mutations.push(changed);
        let mut changed = checkpoint.clone();
        changed.height += 1;
        mutations.push(changed);
        let mut changed = checkpoint.clone();
        changed.state_root[0] ^= 1;
        mutations.push(changed);
        let mut changed = checkpoint;
        changed.timestamp += 1;
        mutations.push(changed);

        for changed in mutations {
            assert!(matches!(
                changed.verify_against_trusted_validator_set(&trusted),
                Err(CheckpointError::InvalidSignature(_))
            ));
        }
    }

    #[test]
    fn checkpoint_verify_chain_consistent() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut registry = CheckpointRegistry::new();
        let bh = test_block_hash(42);
        registry
            .add_checkpoint(signed_checkpoint(1000, bh, &keys), &trusted)
            .unwrap();

        let chain = vec![
            BlockRef {
                hash: test_block_hash(0),
                round: 500,
                height: 500,
            },
            BlockRef {
                hash: bh,
                round: 1000,
                height: 1000,
            },
            BlockRef {
                hash: test_block_hash(1),
                round: 1500,
                height: 1500,
            },
        ];

        assert!(registry.verify_chain_against_checkpoints(&chain));
    }

    #[test]
    fn checkpoint_verify_chain_divergent() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut registry = CheckpointRegistry::new();
        registry
            .add_checkpoint(
                signed_checkpoint(1000, test_block_hash(42), &keys),
                &trusted,
            )
            .unwrap();

        // Chain has a different hash at round 1000.
        let chain = vec![BlockRef {
            hash: test_block_hash(99),
            round: 1000,
            height: 1000,
        }];

        assert!(!registry.verify_chain_against_checkpoints(&chain));
    }

    #[test]
    fn checkpoint_verify_chain_missing_round() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut registry = CheckpointRegistry::new();
        registry
            .add_checkpoint(
                signed_checkpoint(1000, test_block_hash(42), &keys),
                &trusted,
            )
            .unwrap();

        // Chain spans the checkpoint round but has no block at round 1000.
        let chain = vec![
            BlockRef {
                hash: test_block_hash(0),
                round: 999,
                height: 999,
            },
            BlockRef {
                hash: test_block_hash(1),
                round: 1001,
                height: 1001,
            },
        ];

        assert!(!registry.verify_chain_against_checkpoints(&chain));
    }

    #[test]
    fn checkpoint_fork_point_validity() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut registry = CheckpointRegistry::new();

        // No authenticated checkpoint means this API cannot vouch for a fork.
        assert!(!registry.is_valid_fork_point(0));
        assert!(!registry.is_valid_fork_point(500));

        registry
            .add_checkpoint(signed_checkpoint(1000, test_block_hash(1), &keys), &trusted)
            .unwrap();

        // Fork before checkpoint is rejected.
        assert!(!registry.is_valid_fork_point(999));
        assert!(!registry.is_valid_fork_point(0));

        // The checkpoint round itself lacks a hash argument, so only a fork
        // strictly after it can be accepted by this helper.
        assert!(!registry.is_valid_fork_point(1000));
        assert!(registry.is_valid_fork_point(1500));
    }

    #[test]
    fn checkpoint_empty_or_post_anchor_chain_fails_closed() {
        let (keys, trusted) = validator_fixture(&[STAKE_ARC; 4]);
        let mut registry = CheckpointRegistry::new();
        registry
            .add_checkpoint(signed_checkpoint(1000, test_block_hash(1), &keys), &trusted)
            .unwrap();
        assert!(!registry.verify_chain_against_checkpoints(&[]));
        assert!(!registry.verify_chain_against_checkpoints(&[BlockRef {
            hash: test_block_hash(2),
            round: 1001,
            height: 1001,
        }]));
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

        assert_eq!(
            calculate_slash_amount(&SlashableOffense::DoubleVote, stake),
            1_000_000
        );
        assert_eq!(
            calculate_slash_amount(&SlashableOffense::Equivocation, stake),
            1_000_000
        );
        assert_eq!(
            calculate_slash_amount(&SlashableOffense::InvalidBlock, stake),
            500_000
        );
        assert_eq!(
            calculate_slash_amount(&SlashableOffense::WithholdingBlock, stake),
            100_000
        );
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
        assert_eq!(
            SlashableOffense::WithholdingBlock.label(),
            "withholding_block"
        );
        assert_eq!(SlashableOffense::InvalidBlock.label(), "invalid_block");
        assert_eq!(SlashableOffense::Equivocation.label(), "equivocation");
    }

    #[test]
    fn stake_slash_zero_stake() {
        // Edge case: slashing zero stake should produce zero penalty.
        assert_eq!(calculate_slash_amount(&SlashableOffense::DoubleVote, 0), 0);
        assert_eq!(
            calculate_slash_amount(&SlashableOffense::InvalidBlock, 0),
            0
        );
        assert_eq!(
            calculate_slash_amount(&SlashableOffense::WithholdingBlock, 0),
            0
        );
    }
}
