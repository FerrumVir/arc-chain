//! Authenticated absence certificates for the recovery domain's participation
//! requirement.
//!
//! Protocol-v3's recovery domain refuses to advance a round until EVERY fixed
//! validator has contributed a block to it. That is deliberately fail-closed,
//! and it means one member being down halts the chain. This module lets a
//! quorum attest that a named member produced no block in a round - after that
//! round already carried a quorum of stake from others - and that certificate
//! excuses the member from the requirement for that one round, leaving the
//! ordinary quorum rule that every non-recovery chain already uses.
//!
//! **This certificate has no safety role.** It cannot cause or prevent a
//! commit, it does not refuse any block, and it does not enter the commit rule.
//! The worst a false certificate can do is relax one round's participation
//! requirement to quorum, so `observed_round_stake` being self-reported costs
//! nothing that an equal amount of Byzantine stake could not already do.
//!
//! An earlier draft gave this certificate a safety role: signers permanently
//! refused to reference or count the attested block, and a quorum of refusals
//! was supposed to make committing it impossible. An independent adversarial
//! review found that unsound and unsafe in four separate ways - the refusal was
//! direct-parent only while commit support is transitive, and nothing bounded
//! how much stake could be certified absent in one round, which could take
//! block production below quorum permanently. The commit cursor now decides an
//! undecided round retroactively from a later committed anchor's causal
//! history (`ConsensusEngine::try_commit`), which is common knowledge and needs
//! no certificate at all. The review is in
//! `outputs/.../round4/c1-adversarial-consensus-review.md`.
//!
//! Committed-block finality transcripts also live here. They are genuinely
//! safety-relevant and unchanged: a validator signs at most one per height,
//! persists that decision first, and a DAG block signature is never re-labelled
//! as one.

use std::collections::{HashMap, HashSet};

use arc_crypto::{Hash256, KeyPair, Signature, hash_bytes};
use arc_types::Address;
use serde::{Deserialize, Serialize};

use crate::{ConsensusDomain, ValidatorSet};

/// Domain tag for the skip-vote transcript. A signature produced under this tag
/// can never be reinterpreted as a block signature or a finality signature.
pub const SKIP_VOTE_DOMAIN: &[u8] = b"arc.consensus.skipvote.v1";
/// Domain tag for the committed-block finality transcript.
pub const FINALITY_VOTE_DOMAIN: &[u8] = b"arc.consensus.finalityvote.v1";
/// Domain tag for the validator-set commitment carried by both certificates.
pub const VALIDATOR_SET_DOMAIN: &[u8] = b"arc.consensus.validatorset.v1";

/// How long a validator waits, after first seeing a quorum of a round's stake
/// without the leader's block, before it is willing to sign a skip.
///
/// Liveness knob only: a shorter grace never makes an unsafe skip safe, it only
/// skips more eagerly over a leader whose block was merely slow.
pub const DEFAULT_SKIP_GRACE_MS: u64 = 2_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CertificateError {
    #[error("certificate is for consensus domain {found:?}, expected {expected:?}")]
    WrongDomain {
        expected: Box<ConsensusDomain>,
        found: Box<ConsensusDomain>,
    },
    #[error("certificate is bound to a different validator set")]
    WrongValidatorSet,
    #[error("vote {index} disagrees with the certificate header")]
    InconsistentVote { index: usize },
    #[error("validator {0} is not in the frozen set")]
    UnknownVoter(Address),
    #[error("validator {0} voted more than once")]
    DuplicateVoter(Address),
    #[error("signature from {0} did not verify")]
    BadSignature(Address),
    #[error("signing stake {signing} is below quorum {quorum}")]
    BelowQuorum { signing: u64, quorum: u64 },
    #[error("certificate carries no votes")]
    Empty,
    /// Two quorum certificates for one height naming different blocks. This is
    /// a safety violation by the committee, not a local error: with quorum
    /// intersection at more than the Byzantine bound, at least one validator
    /// signed both. It is surfaced rather than resolved - overwriting one with
    /// the other would destroy the only evidence that it happened.
    #[error(
        "conflicting finality certificates at height {height}: already hold {held}, \
         offered {offered}"
    )]
    ConflictingFinality {
        height: u64,
        held: Hash256,
        offered: Hash256,
    },
}

/// Commitment over the exact frozen membership and voting power.
///
/// A certificate is meaningless against a different committee; carrying this
/// makes that explicit instead of implied by context.
pub fn validator_set_hash(set: &ValidatorSet) -> Hash256 {
    let mut members: Vec<(Address, u64)> = set
        .validators
        .iter()
        .map(|validator| (validator.address, validator.stake))
        .collect();
    members.sort_by(|a, b| a.0.0.cmp(&b.0.0));
    let mut bytes = Vec::with_capacity(VALIDATOR_SET_DOMAIN.len() + 16 + members.len() * 40);
    bytes.extend_from_slice(VALIDATOR_SET_DOMAIN);
    bytes.extend_from_slice(&set.epoch.to_le_bytes());
    bytes.extend_from_slice(&(members.len() as u64).to_le_bytes());
    for (address, stake) in &members {
        bytes.extend_from_slice(&address.0);
        bytes.extend_from_slice(&stake.to_le_bytes());
    }
    hash_bytes(&bytes)
}

fn domain_bytes(domain: &ConsensusDomain) -> [u8; 48] {
    let mut out = [0u8; 48];
    out[..32].copy_from_slice(&domain.domain_hash.0);
    out[32..40].copy_from_slice(&domain.recovery_epoch.to_le_bytes());
    out[40..48].copy_from_slice(&domain.validator_set_id.to_le_bytes());
    out
}

// ── Skip votes ───────────────────────────────────────────────────────────────

/// Why a validator attests about a member of a round.
///
/// One reason, kept as an enum so the transcript stays extensible and a future
/// meaning cannot be confused with this one.
///
/// An earlier draft carried a second reason, `NoQuorumSupport`, meant to let a
/// quorum declare that a block that EXISTS could never gather support. An
/// independent adversarial review showed it was unsound: "rounds r+1 and r+2
/// are quorum-complete" bounds how many AUTHORS are missing, not how many
/// SUPPORTERS, and the gap between the two is exactly f - so at n=7 with no
/// Byzantine node at all, six nodes could certify "no support" for a block the
/// seventh had already committed. It was removed rather than tuned, and the
/// commit cursor now decides such rounds retroactively from a later committed
/// anchor's causal history instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AbsenceReason {
    /// The member produced no block in that round, observed after the round
    /// already carried a quorum of stake from others.
    NoBlock,
}

/// One validator's attestation that a round's leader cannot be committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipVote {
    pub domain: ConsensusDomain,
    pub validator_set_hash: Hash256,
    pub round: u64,
    pub absentee: Address,
    pub voter: Address,
    /// Why this attestation was made; bound into the signature.
    pub reason: AbsenceReason,
    /// Stake of the distinct round authors the voter had seen. Recorded so an
    /// auditor can check the voter's own claim, and so a vote that claims less
    /// than quorum is rejected on its face.
    pub observed_round_stake: u64,
    pub signature: Signature,
}

/// The transcript a [`SkipVote`] signs. Separate from the struct so a verifier
/// recomputes it rather than trusting any field ordering on the wire.
pub fn skip_vote_transcript(
    domain: &ConsensusDomain,
    validator_set_hash: &Hash256,
    round: u64,
    absentee: &Address,
    voter: &Address,
    reason: AbsenceReason,
    observed_round_stake: u64,
) -> Hash256 {
    let mut bytes = Vec::with_capacity(SKIP_VOTE_DOMAIN.len() + 48 + 32 + 8 + 32 + 32 + 9);
    bytes.extend_from_slice(SKIP_VOTE_DOMAIN);
    bytes.extend_from_slice(&domain_bytes(domain));
    bytes.extend_from_slice(&validator_set_hash.0);
    bytes.extend_from_slice(&round.to_le_bytes());
    bytes.extend_from_slice(&absentee.0);
    bytes.extend_from_slice(&voter.0);
    bytes.push(match reason {
        AbsenceReason::NoBlock => 0,
    });
    bytes.extend_from_slice(&observed_round_stake.to_le_bytes());
    hash_bytes(&bytes)
}

impl SkipVote {
    pub fn transcript(&self) -> Hash256 {
        skip_vote_transcript(
            &self.domain,
            &self.validator_set_hash,
            self.round,
            &self.absentee,
            &self.voter,
            self.reason,
            self.observed_round_stake,
        )
    }

    /// Sign a skip vote. The caller is responsible for the S1–S5 preconditions;
    /// [`SkipTracker::sign_if_permitted`] is the path that enforces them.
    pub fn sign(
        domain: ConsensusDomain,
        validator_set_hash: Hash256,
        round: u64,
        absentee: Address,
        reason: AbsenceReason,
        observed_round_stake: u64,
        keypair: &KeyPair,
    ) -> Result<Self, arc_crypto::SignatureError> {
        let voter = keypair.address();
        let transcript = skip_vote_transcript(
            &domain,
            &validator_set_hash,
            round,
            &absentee,
            &voter,
            reason,
            observed_round_stake,
        );
        let signature = keypair.sign(&transcript)?;
        Ok(Self {
            domain,
            validator_set_hash,
            round,
            absentee,
            voter,
            reason,
            observed_round_stake,
            signature,
        })
    }

    fn verify_against(&self, set: &ValidatorSet) -> Result<u64, CertificateError> {
        let validator = set
            .get_validator(&self.voter)
            .ok_or(CertificateError::UnknownVoter(self.voter))?;
        self.signature
            .verify(&self.transcript(), &self.voter)
            .map_err(|_| CertificateError::BadSignature(self.voter))?;
        Ok(validator.stake)
    }
}

/// A quorum of [`SkipVote`]s naming one absentee in one round. This is what
/// excuses that member from the round's participation requirement, and — when
/// the absentee is the round's leader — what authorises the commit cursor to
/// move past the round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipCertificate {
    pub domain: ConsensusDomain,
    pub validator_set_hash: Hash256,
    pub round: u64,
    pub absentee: Address,
    pub reason: AbsenceReason,
    pub votes: Vec<SkipVote>,
}

impl SkipCertificate {
    /// Assemble from votes that are already known to share a header. Validity is
    /// still decided by [`Self::verify`]; this never assumes the input is good.
    pub fn new(
        domain: ConsensusDomain,
        validator_set_hash: Hash256,
        round: u64,
        absentee: Address,
        reason: AbsenceReason,
        votes: Vec<SkipVote>,
    ) -> Self {
        Self {
            domain,
            validator_set_hash,
            round,
            absentee,
            reason,
            votes,
        }
    }

    /// Total stake behind this certificate, or the first reason it is invalid.
    ///
    /// Rejects: the wrong consensus domain, a different committee, a vote whose
    /// header disagrees with the certificate, an unknown voter, a repeated
    /// voter, a bad signature, a self-reported observation below quorum, and a
    /// total below quorum.
    pub fn verify(
        &self,
        expected_domain: &ConsensusDomain,
        set: &ValidatorSet,
    ) -> Result<u64, CertificateError> {
        if &self.domain != expected_domain {
            return Err(CertificateError::WrongDomain {
                expected: Box::new(*expected_domain),
                found: Box::new(self.domain),
            });
        }
        if self.validator_set_hash != validator_set_hash(set) {
            return Err(CertificateError::WrongValidatorSet);
        }
        if self.votes.is_empty() {
            return Err(CertificateError::Empty);
        }
        let mut seen = HashSet::new();
        let mut signing = 0u64;
        for (index, vote) in self.votes.iter().enumerate() {
            if vote.domain != self.domain
                || vote.validator_set_hash != self.validator_set_hash
                || vote.round != self.round
                || vote.absentee != self.absentee
                || vote.reason != self.reason
            {
                return Err(CertificateError::InconsistentVote { index });
            }
            // A voter that admits it had not yet seen a quorum of the round's
            // stake has not established S1, whatever it signed.
            if vote.observed_round_stake < set.quorum {
                return Err(CertificateError::InconsistentVote { index });
            }
            if !seen.insert(vote.voter) {
                return Err(CertificateError::DuplicateVoter(vote.voter));
            }
            let stake = vote.verify_against(set)?;
            signing = signing
                .checked_add(stake)
                .expect("unique voter stake cannot exceed the checked set total");
        }
        if signing < set.quorum {
            return Err(CertificateError::BelowQuorum {
                signing,
                quorum: set.quorum,
            });
        }
        Ok(signing)
    }
}

// ── Finality ─────────────────────────────────────────────────────────────────

/// One validator's statement that it committed and executed exactly this block.
///
/// This is NOT a DAG block signature. A DAG signature authorises its own block's
/// hash; relabelling those bytes as finality evidence would be invalid, which is
/// why finality export stayed fail-closed until this transcript existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalityVote {
    pub domain: ConsensusDomain,
    pub validator_set_hash: Hash256,
    pub height: u64,
    pub block_hash: Hash256,
    pub state_root: Hash256,
    pub tx_root: Hash256,
    pub voter: Address,
    pub signature: Signature,
}

pub fn finality_vote_transcript(
    domain: &ConsensusDomain,
    validator_set_hash: &Hash256,
    height: u64,
    block_hash: &Hash256,
    state_root: &Hash256,
    tx_root: &Hash256,
    voter: &Address,
) -> Hash256 {
    let mut bytes = Vec::with_capacity(FINALITY_VOTE_DOMAIN.len() + 48 + 32 + 8 + 32 * 4);
    bytes.extend_from_slice(FINALITY_VOTE_DOMAIN);
    bytes.extend_from_slice(&domain_bytes(domain));
    bytes.extend_from_slice(&validator_set_hash.0);
    bytes.extend_from_slice(&height.to_le_bytes());
    bytes.extend_from_slice(&block_hash.0);
    bytes.extend_from_slice(&state_root.0);
    bytes.extend_from_slice(&tx_root.0);
    bytes.extend_from_slice(&voter.0);
    hash_bytes(&bytes)
}

impl FinalityVote {
    pub fn transcript(&self) -> Hash256 {
        finality_vote_transcript(
            &self.domain,
            &self.validator_set_hash,
            self.height,
            &self.block_hash,
            &self.state_root,
            &self.tx_root,
            &self.voter,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        domain: ConsensusDomain,
        validator_set_hash: Hash256,
        height: u64,
        block_hash: Hash256,
        state_root: Hash256,
        tx_root: Hash256,
        keypair: &KeyPair,
    ) -> Result<Self, arc_crypto::SignatureError> {
        let voter = keypair.address();
        let transcript = finality_vote_transcript(
            &domain,
            &validator_set_hash,
            height,
            &block_hash,
            &state_root,
            &tx_root,
            &voter,
        );
        let signature = keypair.sign(&transcript)?;
        Ok(Self {
            domain,
            validator_set_hash,
            height,
            block_hash,
            state_root,
            tx_root,
            voter,
            signature,
        })
    }

    fn verify_against(&self, set: &ValidatorSet) -> Result<u64, CertificateError> {
        let validator = set
            .get_validator(&self.voter)
            .ok_or(CertificateError::UnknownVoter(self.voter))?;
        self.signature
            .verify(&self.transcript(), &self.voter)
            .map_err(|_| CertificateError::BadSignature(self.voter))?;
        Ok(validator.stake)
    }
}

/// A quorum of [`FinalityVote`]s over one committed block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalityCertificate {
    pub domain: ConsensusDomain,
    pub validator_set_hash: Hash256,
    pub height: u64,
    pub block_hash: Hash256,
    pub state_root: Hash256,
    pub tx_root: Hash256,
    pub votes: Vec<FinalityVote>,
}

impl FinalityCertificate {
    pub fn verify(
        &self,
        expected_domain: &ConsensusDomain,
        set: &ValidatorSet,
    ) -> Result<u64, CertificateError> {
        if &self.domain != expected_domain {
            return Err(CertificateError::WrongDomain {
                expected: Box::new(*expected_domain),
                found: Box::new(self.domain),
            });
        }
        if self.validator_set_hash != validator_set_hash(set) {
            return Err(CertificateError::WrongValidatorSet);
        }
        if self.votes.is_empty() {
            return Err(CertificateError::Empty);
        }
        let mut seen = HashSet::new();
        let mut signing = 0u64;
        for (index, vote) in self.votes.iter().enumerate() {
            if vote.domain != self.domain
                || vote.validator_set_hash != self.validator_set_hash
                || vote.height != self.height
                || vote.block_hash != self.block_hash
                || vote.state_root != self.state_root
                || vote.tx_root != self.tx_root
            {
                return Err(CertificateError::InconsistentVote { index });
            }
            if !seen.insert(vote.voter) {
                return Err(CertificateError::DuplicateVoter(vote.voter));
            }
            let stake = vote.verify_against(set)?;
            signing = signing
                .checked_add(stake)
                .expect("unique voter stake cannot exceed the checked set total");
        }
        if signing < set.quorum {
            return Err(CertificateError::BelowQuorum {
                signing,
                quorum: set.quorum,
            });
        }
        Ok(signing)
    }
}

// ── Durable anti-equivocation record ─────────────────────────────────────────

/// What a validator must remember across restarts so that it cannot be tricked
/// into contradicting itself. Forgetting this is exactly the equivocation the
/// protocol is built to prevent, so a record that cannot be read is a hard
/// startup failure rather than a fresh start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsensusSigningRecord {
    /// Rounds this validator has attested about, which members it named, and
    /// why. More than one member of a round can legitimately be attested, and
    /// the reason decides whether the member is also excused from the
    /// participation requirement, so both are recorded.
    pub skipped_rounds: HashMap<u64, HashMap<Address, AbsenceReason>>,
    /// Heights this validator has signed a finality transcript for.
    pub finality_votes: HashMap<u64, (Hash256, Hash256, Hash256)>,
    /// The highest DAG round whose committed block this node has DURABLY
    /// applied to its own state, or `None` if it has never applied one.
    ///
    /// `None` and `Some(0)` are different and must stay different: a node that
    /// has committed nothing is not a node that has committed round 0.
    ///
    /// **This is the LAST APPLIED round, not the next round to scan.** The
    /// engine's commit cursor means "next round to scan" - `try_commit` starts
    /// at it and stores `r + 1` after committing `r` - so restoring this value
    /// verbatim re-scanned the round just applied and appended a second
    /// canonical block for the same anchor. That off-by-one needed no crash to
    /// occur; `next_round_to_scan` is the only correct way to read it.
    #[serde(default)]
    pub last_applied_round: Option<u64>,
}

impl ConsensusSigningRecord {
    /// Forget decisions this validator can provably never be asked to make
    /// again, so the record stays bounded on a long-running node.
    ///
    /// Unbounded, this record grew by one finality vote per height and one
    /// entry per attested round for the life of the process, and it is
    /// rewritten and fsynced in full before every signature - so both its size
    /// and the cost of every vote grew without limit. A day at the fixture's
    /// block rate is ~430,000 finality entries per node.
    ///
    /// Why dropping below the floors is not the equivocation this record
    /// prevents:
    ///
    /// * **Finality votes.** A validator signs a finality transcript only for a
    ///   height it has just durably produced. Each height is produced once -
    ///   every canonical block commits to the anchor that produced it, and a
    ///   re-offered anchor is refused - and the produced height is restored
    ///   from the durable store on restart, so production resumes ABOVE it. A
    ///   height at or below the durable height is therefore never voted on
    ///   again, and if it were, the durable block at that height would itself
    ///   decide the only transcript this validator could sign. The caller
    ///   passes a floor comfortably below its durable height.
    /// * **Absence attestations.** A validator attests only about rounds at or
    ///   above its durable commit cursor (`last_applied_round`, kept in this
    ///   same file and never pruned). Rounds below the floor are decided, and
    ///   under the v2 design an absence certificate carries no safety weight in
    ///   the commit rule, so a forgotten attestation cannot yield conflicting
    ///   commits.
    ///
    /// `last_applied_round` is never pruned: it is what keeps the cursor, and
    /// with it the absence floor, from going backwards.
    pub fn prune(&mut self, finality_floor_height: u64, absence_floor_round: u64) {
        self.finality_votes
            .retain(|height, _| *height >= finality_floor_height);
        self.skipped_rounds
            .retain(|round, _| *round >= absence_floor_round);
    }

    /// The round `try_commit` should resume scanning from.
    ///
    /// The engine's cursor is the NEXT round to scan, while the record holds
    /// the LAST round applied, so this is the conversion between the two. A
    /// node that has applied nothing resumes at 0; one that applied round 0
    /// resumes at 1.
    pub fn next_round_to_scan(&self) -> u64 {
        match self.last_applied_round {
            Some(applied) => applied.saturating_add(1),
            None => 0,
        }
    }

    /// Serialise with an explicit version tag.
    ///
    /// The record is bincode-encoded, and bincode is NOT self-describing:
    /// `#[serde(default)]` cannot rescue a changed field type, and the v1 -> v2
    /// change of `durable_commit_round: u64` into `last_applied_round:
    /// Option<u64>` silently altered the byte layout. Without this tag an
    /// existing v1 file decodes as a tag byte plus trailing garbage and
    /// `load_signing_record` panics the node at startup. The tag makes the
    /// format change explicit and keeps v1 files readable.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(SIGNING_RECORD_MAGIC);
        bytes.push(SIGNING_RECORD_VERSION);
        bytes.extend_from_slice(&bincode::serialize(self).expect("signing record is serialisable"));
        bytes
    }

    /// Read either format. A v1 file is migrated; anything else is an error,
    /// never a silent fresh start.
    pub fn decode(bytes: &[u8]) -> Result<Self, SigningRecordError> {
        if let Some(rest) = bytes.strip_prefix(SIGNING_RECORD_MAGIC) {
            let (version, payload) = rest.split_first().ok_or(SigningRecordError::Truncated)?;
            if *version != SIGNING_RECORD_VERSION {
                return Err(SigningRecordError::UnknownVersion(*version));
            }
            return bincode::deserialize(payload).map_err(SigningRecordError::Malformed);
        }
        // No magic: a v1 file, written before the format was tagged.
        let legacy: LegacySigningRecordV1 =
            bincode::deserialize(bytes).map_err(SigningRecordError::Malformed)?;
        Ok(legacy.migrate())
    }
}

/// Magic + version prefix on a v2 record.
const SIGNING_RECORD_MAGIC: &[u8] = b"ARC-SIGNREC";
const SIGNING_RECORD_VERSION: u8 = 2;

/// The v1 on-disk shape. Kept only so an existing file can be migrated.
#[derive(Deserialize)]
struct LegacySigningRecordV1 {
    skipped_rounds: HashMap<u64, HashMap<Address, AbsenceReason>>,
    finality_votes: HashMap<u64, (Hash256, Hash256, Hash256)>,
    /// v1 stored the last applied round in a bare `u64`, so it could not tell
    /// "nothing applied" from "applied round 0". Both were written as 0.
    durable_commit_round: u64,
}

impl LegacySigningRecordV1 {
    fn migrate(self) -> ConsensusSigningRecord {
        ConsensusSigningRecord {
            skipped_rounds: self.skipped_rounds,
            finality_votes: self.finality_votes,
            // A v1 zero is genuinely ambiguous, so it resolves to `None` - this
            // node rescans from round 0 rather than claiming it applied
            // genesis. Adding one to that zero would assert an application that
            // may never have happened. Rescanning is safe because every
            // canonical block commits to the anchor that produced it, so an
            // already-applied anchor is recognised and not applied twice.
            last_applied_round: (self.durable_commit_round > 0)
                .then_some(self.durable_commit_round),
        }
    }
}

/// Why a durable signing record could not be read. Every variant is fatal at
/// startup: a record that cannot be understood must not become a fresh start.
#[derive(Debug)]
pub enum SigningRecordError {
    Truncated,
    UnknownVersion(u8),
    Malformed(bincode::Error),
}

impl std::fmt::Display for SigningRecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "record ends before its version tag"),
            Self::UnknownVersion(version) => write!(
                f,
                "record is version {version}, but this build understands up to \
                 {SIGNING_RECORD_VERSION}; a newer node wrote it"
            ),
            Self::Malformed(error) => write!(f, "record body is malformed: {error}"),
        }
    }
}

impl std::error::Error for SigningRecordError {}

// ── The skip state machine ───────────────────────────────────────────────────

/// Why a validator declined to sign a skip vote. Every variant is a safety or
/// sequencing rule, not a transient error, so callers log rather than retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipRefusal {
    /// S1: the round has not yet carried a quorum of stake.
    RoundBelowQuorum,
    /// S2: the attestable condition was falsified in this validator's view -
    /// the block turned up, or it reached quorum support.
    AbsenteeBlockPresent,
    /// S3: the grace period since S1 first held has not elapsed.
    WithinGrace,
    /// S4: this validator already signed a conflicting attestation.
    ConflictingSkip,
    /// The round is already behind the commit cursor; nothing to skip.
    RoundAlreadyPassed,
}

/// Per-round observation state backing conditions S1 and S3.
#[derive(Debug, Clone, Copy)]
struct RoundObservation {
    /// Stake of the distinct round authors seen so far.
    stake: u64,
    /// Sticky: the attestable condition was falsified at least once, so this
    /// validator can never attest it. For `NoBlock` that means the member's
    /// block was seen; for `NoQuorumSupport` that its block reached quorum
    /// support. Stickiness is what stops a later, narrower view from reopening
    /// an attestation the validator has already ruled out.
    condition_broken: bool,
    /// Monotonic milliseconds at which the condition first held with a quorum.
    holding_since: Option<u64>,
}

/// Tracks, per round, whether this validator may sign a skip vote — and records
/// the decision so it can never contradict itself later.
///
/// Deliberately free of clocks, I/O and networking: the caller supplies a
/// monotonic millisecond reading, so the same sequence of observations always
/// produces the same decisions in a simulation and on a real node.
#[derive(Debug)]
pub struct SkipTracker {
    domain: ConsensusDomain,
    validator_set_hash: Hash256,
    grace_ms: u64,
    /// Keyed by `(round, absentee)`. Keying by round alone would let an
    /// observation about a member that IS present poison the observation about
    /// one that is absent, since `absentee_seen` is deliberately sticky.
    observations: HashMap<(u64, Address, AbsenceReason), RoundObservation>,
    record: ConsensusSigningRecord,
    /// Leader blocks this validator has permanently refused, as
    /// `(round, absentee)`. Once present, the block is never referenced as a
    /// parent and never counted in commit support.
    refused: HashSet<(u64, Address)>,
}

impl SkipTracker {
    /// (observations, refused, record skipped rounds, record finality votes)
    /// held, for resource diagnostics.
    pub fn sizes(&self) -> (usize, usize, usize, usize) {
        (
            self.observations.len(),
            self.refused.len(),
            self.record.skipped_rounds.len(),
            self.record.finality_votes.len(),
        )
    }

    pub fn new(
        domain: ConsensusDomain,
        validator_set_hash: Hash256,
        grace_ms: u64,
        record: ConsensusSigningRecord,
    ) -> Self {
        let refused = record
            .skipped_rounds
            .iter()
            .flat_map(|(round, members)| members.keys().map(move |member| (*round, *member)))
            .collect();
        Self {
            domain,
            validator_set_hash,
            grace_ms,
            observations: HashMap::new(),
            record,
            refused,
        }
    }

    pub fn record(&self) -> &ConsensusSigningRecord {
        &self.record
    }

    /// True when this validator has already attested about a member in a round.
    /// Informational: nothing in the commit rule consults it.
    pub fn refuses(&self, round: u64, absentee: &Address) -> bool {
        self.refused.contains(&(round, *absentee))
    }

    /// Record that a committed block at this DAG round has been durably
    /// applied. Monotonic: a later crash cannot lower it.
    pub fn note_durable_commit_round(&mut self, round: u64) {
        if self
            .record
            .last_applied_round
            .is_none_or(|applied| round > applied)
        {
            self.record.last_applied_round = Some(round);
        }
    }

    /// Record that this validator signed a finality transcript for a height.
    /// One per height, ever: the record is what makes that survive a restart.
    pub fn note_finality_vote(&mut self, height: u64, identity: (Hash256, Hash256, Hash256)) {
        self.record.finality_votes.insert(height, identity);
    }

    /// Feed the current view of one round for one (member, reason) pair.
    ///
    /// `condition_holds` is the attestable claim as this validator currently
    /// sees it: for [`AbsenceReason::NoBlock`], that the member produced no
    /// block; for [`AbsenceReason::NoQuorumSupport`], that no child of its
    /// block reaches quorum support. `distinct_author_stake` is the stake of
    /// distinct authors in the round, which is what makes the round
    /// quorum-complete enough to judge.
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &mut self,
        round: u64,
        member: &Address,
        reason: AbsenceReason,
        distinct_author_stake: u64,
        condition_holds: bool,
        quorum: u64,
        now_ms: u64,
    ) {
        let entry = self
            .observations
            .entry((round, *member, reason))
            .or_insert(RoundObservation {
                stake: 0,
                condition_broken: false,
                holding_since: None,
            });
        entry.stake = entry.stake.max(distinct_author_stake);
        entry.condition_broken |= !condition_holds;
        if entry.stake >= quorum && !entry.condition_broken {
            entry.holding_since.get_or_insert(now_ms);
        } else {
            entry.holding_since = None;
        }
    }

    /// Decide whether to sign, and sign if permitted. On success the decision is
    /// already recorded in [`Self::record`]; the caller must persist that record
    /// **before** the returned vote leaves the process (S5).
    #[allow(clippy::too_many_arguments)]
    pub fn sign_if_permitted(
        &mut self,
        round: u64,
        absentee: Address,
        reason: AbsenceReason,
        commit_cursor: u64,
        quorum: u64,
        now_ms: u64,
        keypair: &KeyPair,
    ) -> Result<SkipVote, SkipRefusal> {
        if round < commit_cursor {
            return Err(SkipRefusal::RoundAlreadyPassed);
        }
        let observation = self
            .observations
            .get(&(round, absentee, reason))
            .copied()
            .ok_or(SkipRefusal::RoundBelowQuorum)?;
        if observation.condition_broken {
            return Err(SkipRefusal::AbsenteeBlockPresent);
        }
        if observation.stake < quorum {
            return Err(SkipRefusal::RoundBelowQuorum);
        }
        let since = observation
            .holding_since
            .ok_or(SkipRefusal::RoundBelowQuorum)?;
        if now_ms.saturating_sub(since) < self.grace_ms {
            return Err(SkipRefusal::WithinGrace);
        }
        let vote = SkipVote::sign(
            self.domain,
            self.validator_set_hash,
            round,
            absentee,
            reason,
            observation.stake,
            keypair,
        )
        .map_err(|_| SkipRefusal::ConflictingSkip)?;
        self.record
            .skipped_rounds
            .entry(round)
            .or_default()
            .insert(absentee, reason);
        self.refused.insert((round, absentee));
        Ok(vote)
    }

    /// Adopt a peer's valid certificate. The local validator inherits the same
    /// permanent refusal, so it cannot later help certify the skipped block.
    pub fn adopt_certificate(&mut self, certificate: &SkipCertificate) {
        self.record
            .skipped_rounds
            .entry(certificate.round)
            .or_default()
            .insert(certificate.absentee, certificate.reason);
        self.refused
            .insert((certificate.round, certificate.absentee));
    }

    /// Drop observations for rounds the commit cursor has passed.
    pub fn prune_observations_below(&mut self, round: u64) {
        self.observations
            .retain(|(tracked, _, _), _| *tracked >= round);
        self.refused.retain(|(tracked, _)| *tracked >= round);
    }

    /// Bound the durable record. See [`ConsensusSigningRecord::prune`] for why
    /// this does not forget any decision this validator could be asked to make
    /// again.
    pub fn prune_record(&mut self, finality_floor_height: u64, absence_floor_round: u64) {
        self.record
            .prune(finality_floor_height, absence_floor_round);
    }
}

/// Collects skip votes from peers until a quorum exists for a round.
#[derive(Debug, Default)]
pub struct SkipVoteCollector {
    by_round: HashMap<(u64, Address, AbsenceReason), HashMap<Address, SkipVote>>,
}

impl SkipVoteCollector {
    /// (round, member, reason) slots being collected, for diagnostics.
    pub fn len(&self) -> usize {
        self.by_round.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_round.is_empty()
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Add a vote. Returns a certificate the moment the accumulated distinct
    /// voters reach quorum stake. Rejects anything that does not verify.
    pub fn add(
        &mut self,
        vote: SkipVote,
        expected_domain: &ConsensusDomain,
        set: &ValidatorSet,
    ) -> Result<Option<SkipCertificate>, CertificateError> {
        if &vote.domain != expected_domain {
            return Err(CertificateError::WrongDomain {
                expected: Box::new(*expected_domain),
                found: Box::new(vote.domain),
            });
        }
        if vote.validator_set_hash != validator_set_hash(set) {
            return Err(CertificateError::WrongValidatorSet);
        }
        if vote.observed_round_stake < set.quorum {
            return Err(CertificateError::InconsistentVote { index: 0 });
        }
        vote.verify_against(set)?;
        let key = (vote.round, vote.absentee, vote.reason);
        let round_votes = self.by_round.entry(key).or_default();
        round_votes.insert(vote.voter, vote);
        let mut signing = 0u64;
        for voter in round_votes.keys() {
            if let Some(validator) = set.get_validator(voter) {
                signing = signing.saturating_add(validator.stake);
            }
        }
        if signing >= set.quorum {
            let votes: Vec<SkipVote> = round_votes.values().cloned().collect();
            let certificate = SkipCertificate::new(
                *expected_domain,
                validator_set_hash(set),
                key.0,
                key.1,
                key.2,
                votes,
            );
            return Ok(Some(certificate));
        }
        Ok(None)
    }

    pub fn prune_below(&mut self, round: u64) {
        self.by_round.retain(|(tracked, _, _), _| *tracked >= round);
    }
}

/// Collects finality votes until a quorum exists for one committed block.
#[derive(Debug, Default)]
pub struct FinalityVoteCollector {
    by_block: HashMap<(u64, Hash256), HashMap<Address, FinalityVote>>,
}

impl FinalityVoteCollector {
    /// (height, block) slots being collected, for diagnostics.
    pub fn len(&self) -> usize {
        self.by_block.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_block.is_empty()
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(
        &mut self,
        vote: FinalityVote,
        expected_domain: &ConsensusDomain,
        set: &ValidatorSet,
    ) -> Result<Option<FinalityCertificate>, CertificateError> {
        if &vote.domain != expected_domain {
            return Err(CertificateError::WrongDomain {
                expected: Box::new(*expected_domain),
                found: Box::new(vote.domain),
            });
        }
        if vote.validator_set_hash != validator_set_hash(set) {
            return Err(CertificateError::WrongValidatorSet);
        }
        vote.verify_against(set)?;
        let key = (vote.height, vote.block_hash);
        let header = (
            vote.state_root,
            vote.tx_root,
            vote.domain,
            vote.validator_set_hash,
        );
        let block_votes = self.by_block.entry(key).or_default();
        // A voter that changes its mind about the same block's roots is
        // equivocating; keep the first statement and ignore the rest.
        if let Some(existing) = block_votes.get(&vote.voter)
            && (existing.state_root, existing.tx_root) != (vote.state_root, vote.tx_root)
        {
            return Err(CertificateError::InconsistentVote { index: 0 });
        }
        block_votes.insert(vote.voter, vote);
        let mut signing = 0u64;
        let mut votes = Vec::with_capacity(block_votes.len());
        for (voter, stored) in block_votes.iter() {
            if (
                stored.state_root,
                stored.tx_root,
                stored.domain,
                stored.validator_set_hash,
            ) != header
            {
                continue;
            }
            if let Some(validator) = set.get_validator(voter) {
                signing = signing.saturating_add(validator.stake);
                votes.push(stored.clone());
            }
        }
        if signing >= set.quorum {
            return Ok(Some(FinalityCertificate {
                domain: *expected_domain,
                validator_set_hash: validator_set_hash(set),
                height: key.0,
                block_hash: key.1,
                state_root: header.0,
                tx_root: header.1,
                votes,
            }));
        }
        Ok(None)
    }

    pub fn prune_below(&mut self, height: u64) {
        self.by_block.retain(|(tracked, _), _| *tracked >= height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{STAKE_ARC, Validator};

    fn domain() -> ConsensusDomain {
        ConsensusDomain::new(hash_bytes(b"arc.test.domain"), 1, 1)
    }

    fn committee(n: usize) -> (ValidatorSet, Vec<KeyPair>) {
        let keys: Vec<KeyPair> = (0..n).map(|_| KeyPair::generate_ed25519()).collect();
        let validators: Vec<Validator> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| {
                Validator::new(key.address(), STAKE_ARC, i as u16).expect("valid validator")
            })
            .collect();
        (ValidatorSet::new(validators, 1), keys)
    }

    fn skip_votes(
        set: &ValidatorSet,
        keys: &[KeyPair],
        round: u64,
        leader: Address,
        count: usize,
    ) -> Vec<SkipVote> {
        let set_hash = validator_set_hash(set);
        keys.iter()
            .take(count)
            .map(|key| {
                SkipVote::sign(
                    domain(),
                    set_hash,
                    round,
                    leader,
                    AbsenceReason::NoBlock,
                    set.quorum,
                    key,
                )
                .unwrap()
            })
            .collect()
    }

    // ── quorum arithmetic the safety argument depends on ────────────────────

    #[test]
    fn two_quorums_always_intersect_in_more_than_the_byzantine_bound() {
        // S - 2f > f for every committee size the product supports. If this
        // ever fails, the skip-certificate safety argument fails with it.
        for n in 1..=32usize {
            let (set, _) = committee(n);
            let total = set.total_stake;
            let f = total - set.quorum;
            let intersection = (2 * set.quorum).saturating_sub(total);
            assert!(
                intersection > f || n == 1,
                "n={n}: quorum intersection {intersection} does not exceed f={f}"
            );
        }
    }

    #[test]
    fn honest_stake_alone_can_form_a_certificate() {
        // Liveness depends on this: with the maximum Byzantine stake absent,
        // the remaining honest stake still reaches quorum.
        for n in 2..=16usize {
            let (set, _) = committee(n);
            let f = set.total_stake - set.quorum;
            assert!(
                set.total_stake - f >= set.quorum,
                "n={n}: honest stake cannot reach quorum"
            );
        }
    }

    // ── certificate validity ────────────────────────────────────────────────

    #[test]
    fn a_quorum_of_honest_skip_votes_verifies() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let votes = skip_votes(&set, &keys, 7, leader, 3);
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            7,
            leader,
            AbsenceReason::NoBlock,
            votes,
        );
        let signing = certificate.verify(&domain(), &set).expect("valid");
        assert!(signing >= set.quorum);
    }

    #[test]
    fn a_certificate_below_quorum_is_refused() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let votes = skip_votes(&set, &keys, 7, leader, 2);
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            7,
            leader,
            AbsenceReason::NoBlock,
            votes,
        );
        assert!(matches!(
            certificate.verify(&domain(), &set),
            Err(CertificateError::BelowQuorum { .. })
        ));
    }

    #[test]
    fn a_repeated_voter_cannot_manufacture_quorum() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let mut votes = skip_votes(&set, &keys, 7, leader, 1);
        votes.push(votes[0].clone());
        votes.push(votes[0].clone());
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            7,
            leader,
            AbsenceReason::NoBlock,
            votes,
        );
        assert!(matches!(
            certificate.verify(&domain(), &set),
            Err(CertificateError::DuplicateVoter(_))
        ));
    }

    #[test]
    fn a_certificate_from_another_consensus_domain_is_refused() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let votes = skip_votes(&set, &keys, 7, leader, 3);
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            7,
            leader,
            AbsenceReason::NoBlock,
            votes,
        );
        let other = ConsensusDomain::new(hash_bytes(b"another.chain"), 1, 1);
        assert!(matches!(
            certificate.verify(&other, &set),
            Err(CertificateError::WrongDomain { .. })
        ));
    }

    #[test]
    fn a_certificate_bound_to_another_committee_is_refused() {
        let (set, keys) = committee(4);
        let (other_set, _) = committee(4);
        let leader = keys[0].address();
        let votes = skip_votes(&set, &keys, 7, leader, 3);
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            7,
            leader,
            AbsenceReason::NoBlock,
            votes,
        );
        assert!(matches!(
            certificate.verify(&domain(), &other_set),
            Err(CertificateError::WrongValidatorSet)
        ));
    }

    #[test]
    fn a_vote_from_outside_the_committee_is_refused() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let mut votes = skip_votes(&set, &keys, 7, leader, 3);
        let outsider = KeyPair::generate_ed25519();
        votes.push(
            SkipVote::sign(
                domain(),
                validator_set_hash(&set),
                7,
                leader,
                AbsenceReason::NoBlock,
                set.quorum,
                &outsider,
            )
            .unwrap(),
        );
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            7,
            leader,
            AbsenceReason::NoBlock,
            votes,
        );
        assert!(matches!(
            certificate.verify(&domain(), &set),
            Err(CertificateError::UnknownVoter(_))
        ));
    }

    #[test]
    fn a_forged_signature_is_refused() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let mut votes = skip_votes(&set, &keys, 7, leader, 3);
        // Keep the voter identity, swap in a signature over a different round.
        let wrong = SkipVote::sign(
            domain(),
            validator_set_hash(&set),
            8,
            leader,
            AbsenceReason::NoBlock,
            set.quorum,
            &keys[2],
        )
        .unwrap();
        votes[2].signature = wrong.signature;
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            7,
            leader,
            AbsenceReason::NoBlock,
            votes,
        );
        assert!(matches!(
            certificate.verify(&domain(), &set),
            Err(CertificateError::BadSignature(_))
        ));
    }

    #[test]
    fn a_vote_for_a_different_round_cannot_be_folded_in() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let mut votes = skip_votes(&set, &keys, 7, leader, 2);
        votes.extend(skip_votes(&set, &keys[2..], 9, leader, 1));
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            7,
            leader,
            AbsenceReason::NoBlock,
            votes,
        );
        assert!(matches!(
            certificate.verify(&domain(), &set),
            Err(CertificateError::InconsistentVote { .. })
        ));
    }

    #[test]
    fn a_voter_that_admits_it_lacked_quorum_is_refused() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let set_hash = validator_set_hash(&set);
        // Signed honestly, but the voter states it had seen less than quorum,
        // so it never established S1 whatever else it claims.
        let votes: Vec<SkipVote> = keys[..3]
            .iter()
            .map(|key| {
                SkipVote::sign(
                    domain(),
                    set_hash,
                    7,
                    leader,
                    AbsenceReason::NoBlock,
                    set.quorum - 1,
                    key,
                )
                .unwrap()
            })
            .collect();
        let certificate =
            SkipCertificate::new(domain(), set_hash, 7, leader, AbsenceReason::NoBlock, votes);
        assert!(matches!(
            certificate.verify(&domain(), &set),
            Err(CertificateError::InconsistentVote { .. })
        ));
    }

    #[test]
    fn a_skip_vote_signature_is_not_a_finality_signature() {
        // Domain separation: the two transcripts must never collide, or a skip
        // vote could be replayed as finality evidence.
        let (set, keys) = committee(4);
        let set_hash = validator_set_hash(&set);
        let skip = skip_vote_transcript(
            &domain(),
            &set_hash,
            3,
            &keys[0].address(),
            &keys[1].address(),
            AbsenceReason::NoBlock,
            set.quorum,
        );
        let finality = finality_vote_transcript(
            &domain(),
            &set_hash,
            3,
            &hash_bytes(b"block"),
            &hash_bytes(b"state"),
            &hash_bytes(b"tx"),
            &keys[1].address(),
        );
        assert_ne!(skip, finality);
    }

    // ── the S1..S5 state machine ────────────────────────────────────────────

    fn tracker(set: &ValidatorSet) -> SkipTracker {
        SkipTracker::new(
            domain(),
            validator_set_hash(set),
            DEFAULT_SKIP_GRACE_MS,
            ConsensusSigningRecord::default(),
        )
    }

    #[test]
    fn s1_a_round_below_quorum_cannot_be_skipped() {
        let (set, keys) = committee(4);
        let mut tracker = tracker(&set);
        let leader = keys[0].address();
        tracker.observe(
            5,
            &leader,
            AbsenceReason::NoBlock,
            set.quorum - 1,
            true,
            set.quorum,
            0,
        );
        assert_eq!(
            tracker.sign_if_permitted(
                5,
                leader,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                10_000,
                &keys[1]
            ),
            Err(SkipRefusal::RoundBelowQuorum)
        );
    }

    #[test]
    fn s2_a_leader_block_that_was_seen_blocks_the_skip_permanently() {
        let (set, keys) = committee(4);
        let mut tracker = tracker(&set);
        let leader = keys[0].address();
        tracker.observe(
            5,
            &leader,
            AbsenceReason::NoBlock,
            set.quorum,
            false,
            set.quorum,
            0,
        );
        // A later view that omits the leader must not reopen the skip.
        tracker.observe(
            5,
            &leader,
            AbsenceReason::NoBlock,
            set.quorum,
            true,
            set.quorum,
            5_000,
        );
        assert_eq!(
            tracker.sign_if_permitted(
                5,
                leader,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                10_000,
                &keys[1]
            ),
            Err(SkipRefusal::AbsenteeBlockPresent)
        );
    }

    #[test]
    fn s3_the_grace_period_is_enforced() {
        let (set, keys) = committee(4);
        let mut tracker = tracker(&set);
        let leader = keys[0].address();
        tracker.observe(
            5,
            &leader,
            AbsenceReason::NoBlock,
            set.quorum,
            true,
            set.quorum,
            1_000,
        );
        assert_eq!(
            tracker.sign_if_permitted(
                5,
                leader,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                1_500,
                &keys[1]
            ),
            Err(SkipRefusal::WithinGrace)
        );
        let vote = tracker
            .sign_if_permitted(
                5,
                leader,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                1_000 + DEFAULT_SKIP_GRACE_MS,
                &keys[1],
            )
            .expect("grace elapsed");
        assert_eq!(vote.round, 5);
        assert_eq!(vote.absentee, leader);
    }

    #[test]
    fn the_applied_round_is_monotonic_and_converts_to_a_scan_cursor() {
        // Two separate obligations, and the second is the one that was wrong.
        //
        // 1. The record must never go backwards: a later crash cannot make the
        //    node re-decide rounds it already committed and applied.
        // 2. The record holds the LAST APPLIED round; the engine's cursor is
        //    the NEXT round to scan. Restoring the former verbatim as the
        //    latter re-scanned the round just applied and appended a second
        //    canonical block for one anchor - no crash required.
        let (set, _) = committee(4);
        let mut tracker = tracker(&set);
        assert_eq!(tracker.record().last_applied_round, None);
        assert_eq!(
            tracker.record().next_round_to_scan(),
            0,
            "a node that has applied nothing resumes at 0"
        );

        tracker.note_durable_commit_round(40);
        tracker.note_durable_commit_round(12);
        assert_eq!(
            tracker.record().last_applied_round,
            Some(40),
            "an earlier round must not lower the record"
        );
        assert_eq!(tracker.record().next_round_to_scan(), 41);

        let encoded = tracker.record().encode();
        let restored = ConsensusSigningRecord::decode(&encoded).expect("decodes");
        assert_eq!(restored.last_applied_round, Some(40));
        assert_eq!(restored.next_round_to_scan(), 41);
    }

    #[test]
    fn having_applied_round_zero_is_not_the_same_as_having_applied_nothing() {
        // The distinction a bare `u64` could not make, and the reason the field
        // is an Option: adding one to a default zero would resume a fresh node
        // at round 1 and silently skip genesis.
        let (set, _) = committee(4);
        let fresh = tracker(&set);
        assert_eq!(fresh.record().last_applied_round, None);
        assert_eq!(fresh.record().next_round_to_scan(), 0);

        let mut applied_genesis = tracker(&set);
        applied_genesis.note_durable_commit_round(0);
        assert_eq!(applied_genesis.record().last_applied_round, Some(0));
        assert_eq!(
            applied_genesis.record().next_round_to_scan(),
            1,
            "a node that applied round 0 must not be asked to apply it again"
        );

        // And the distinction survives a restart in both directions.
        for record in [fresh.record().clone(), applied_genesis.record().clone()] {
            let round_tripped = ConsensusSigningRecord::decode(&record.encode()).expect("decodes");
            assert_eq!(round_tripped.last_applied_round, record.last_applied_round);
            assert_eq!(
                round_tripped.next_round_to_scan(),
                record.next_round_to_scan()
            );
        }
    }

    /// The v1 shape, reproduced exactly so the migration is tested against
    /// real legacy bytes rather than against a description of them.
    #[derive(Serialize)]
    struct LegacyV1ForTest {
        skipped_rounds: HashMap<u64, HashMap<Address, AbsenceReason>>,
        finality_votes: HashMap<u64, (Hash256, Hash256, Hash256)>,
        durable_commit_round: u64,
    }

    fn legacy_v1_bytes(durable_commit_round: u64) -> Vec<u8> {
        bincode::serialize(&LegacyV1ForTest {
            skipped_rounds: HashMap::new(),
            finality_votes: HashMap::new(),
            durable_commit_round,
        })
        .expect("legacy record serialises")
    }

    #[test]
    fn a_v1_record_is_migrated_rather_than_panicking_the_node() {
        // Changing the field type changed the bincode layout. Reading a v1
        // file as v2 fails, and `load_signing_record` treats a failure as
        // fatal - so without migration every node holding a record would
        // refuse to start.
        let raw = legacy_v1_bytes(575);
        assert!(
            bincode::deserialize::<ConsensusSigningRecord>(&raw).is_err(),
            "if v1 bytes still parsed as v2 this migration would be unnecessary"
        );

        let migrated = ConsensusSigningRecord::decode(&raw).expect("v1 migrates");
        assert_eq!(migrated.last_applied_round, Some(575));
        assert_eq!(
            migrated.next_round_to_scan(),
            576,
            "a migrated v1 record must resume AFTER the round it applied"
        );
    }

    #[test]
    fn a_v1_zero_is_ambiguous_and_resolves_to_nothing_applied() {
        // v1 wrote 0 both for "nothing applied" and for "applied round 0".
        // Adding one to it would assert an application that may never have
        // happened, so it resolves to None and the node rescans from 0.
        let migrated = ConsensusSigningRecord::decode(&legacy_v1_bytes(0)).expect("v1 migrates");
        assert_eq!(migrated.last_applied_round, None);
        assert_eq!(migrated.next_round_to_scan(), 0);
    }

    #[test]
    fn a_record_from_a_newer_build_is_refused_not_silently_reset() {
        let mut bytes = ConsensusSigningRecord::default().encode();
        let version_at = SIGNING_RECORD_MAGIC.len();
        bytes[version_at] = SIGNING_RECORD_VERSION + 1;
        let error = ConsensusSigningRecord::decode(&bytes).expect_err("must refuse");
        assert!(
            matches!(error, SigningRecordError::UnknownVersion(v) if v == SIGNING_RECORD_VERSION + 1),
            "got {error:?}"
        );
    }

    #[test]
    fn pruning_keeps_every_decision_at_or_above_the_floors() {
        let mut record = ConsensusSigningRecord::default();
        for h in 0..1000u64 {
            record.finality_votes.insert(
                h,
                (hash_bytes(&h.to_le_bytes()), Hash256::ZERO, Hash256::ZERO),
            );
            record
                .skipped_rounds
                .entry(h)
                .or_default()
                .insert(hash_bytes(b"m"), AbsenceReason::NoBlock);
        }
        record.last_applied_round = Some(999);
        record.prune(900, 800);
        assert_eq!(record.finality_votes.len(), 100);
        assert!(record.finality_votes.keys().all(|h| *h >= 900));
        assert_eq!(record.skipped_rounds.len(), 200);
        assert!(record.skipped_rounds.keys().all(|r| *r >= 800));
        assert_eq!(
            record.last_applied_round,
            Some(999),
            "the cursor is never pruned; it is what keeps the floors from moving back"
        );
    }

    #[test]
    fn a_retained_finality_vote_still_refuses_a_conflicting_transcript() {
        // Pruning must not weaken the check for heights it keeps.
        let mut tracker = SkipTracker::new(
            domain(),
            Hash256::ZERO,
            DEFAULT_SKIP_GRACE_MS,
            ConsensusSigningRecord::default(),
        );
        let first = (hash_bytes(b"block-a"), Hash256::ZERO, Hash256::ZERO);
        for h in 0..500u64 {
            tracker.note_finality_vote(h, first);
        }
        tracker.prune_record(400, 0);
        assert_eq!(
            tracker.record().finality_votes.get(&450),
            Some(&first),
            "a vote above the floor must survive, so a second transcript for \
             that height can still be refused"
        );
        assert!(tracker.record().finality_votes.get(&10).is_none());
    }

    #[test]
    fn a_long_run_record_stays_bounded_when_pruned() {
        // The shape of a day: one vote per height, pruned periodically with a
        // fixed margin. The encoded size must stop growing.
        let mut tracker = SkipTracker::new(
            domain(),
            Hash256::ZERO,
            DEFAULT_SKIP_GRACE_MS,
            ConsensusSigningRecord::default(),
        );
        let margin = 256u64;
        let every = 128u64;
        // Sample at the SAME phase of every prune cycle - just before a prune,
        // where the record is largest. (An earlier version sampled every 5,000
        // heights, which lands at a phase that drifts through the 128-height
        // cycle, and reported bounded growth as a leak.)
        let mut sizes = Vec::new();
        let mut max_entries = 0usize;
        for h in 0..20_000u64 {
            tracker.note_finality_vote(
                h,
                (hash_bytes(&h.to_le_bytes()), Hash256::ZERO, Hash256::ZERO),
            );
            tracker.note_durable_commit_round(h);
            if h % every == 0 {
                tracker.prune_record(h.saturating_sub(margin), h.saturating_sub(margin));
            }
            max_entries = max_entries.max(tracker.record().finality_votes.len());
            if h > 2_000 && h % every == every - 1 {
                sizes.push(tracker.record().encode().len());
            }
        }
        let (lo, hi) = (*sizes.iter().min().unwrap(), *sizes.iter().max().unwrap());
        assert_eq!(
            lo, hi,
            "at the same point of every prune cycle the record must be the same size"
        );
        assert!(
            max_entries <= (margin + every) as usize,
            "{max_entries} finality entries were retained at peak; the bound is {}",
            margin + every
        );
    }

    #[test]
    fn a_v2_record_is_self_identifying() {
        let mut record = ConsensusSigningRecord::default();
        record.last_applied_round = Some(9);
        let bytes = record.encode();
        assert!(bytes.starts_with(SIGNING_RECORD_MAGIC));
        assert_eq!(bytes[SIGNING_RECORD_MAGIC.len()], SIGNING_RECORD_VERSION);
        assert_eq!(ConsensusSigningRecord::decode(&bytes).unwrap(), record);
    }

    #[test]
    fn two_members_of_one_round_can_both_be_attested() {
        // More than one member of a round can legitimately be absent, so an
        // attestation about one must not block an attestation about another.
        //
        // The earlier design made this dangerous: each attestation refused a
        // block as a parent, so enough of them in one round could take a
        // proposer's usable parent stake below quorum and stop block
        // production for good. The certificate no longer refuses anything, so
        // the only cost of several attestations in one round is that those
        // members are excused from that round's participation requirement.
        let (set, keys) = committee(4);
        let mut tracker = tracker(&set);
        let first = keys[0].address();
        let second = keys[2].address();
        tracker.observe(
            5,
            &first,
            AbsenceReason::NoBlock,
            set.quorum,
            true,
            set.quorum,
            0,
        );
        tracker.observe(
            5,
            &second,
            AbsenceReason::NoBlock,
            set.quorum,
            true,
            set.quorum,
            0,
        );
        tracker
            .sign_if_permitted(
                5,
                first,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                100_000,
                &keys[1],
            )
            .expect("first attestation");
        tracker
            .sign_if_permitted(
                5,
                second,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                100_000,
                &keys[1],
            )
            .expect("a different member of the same round");
        assert!(tracker.refuses(5, &first));
        assert!(tracker.refuses(5, &second));
        // And a member that was never attested is not refused.
        assert!(!tracker.refuses(5, &keys[3].address()));
    }

    #[test]
    fn an_attestation_records_an_excusal_that_survives_restart() {
        let (set, keys) = committee(4);
        let mut tracker = tracker(&set);
        let leader = keys[0].address();
        tracker.observe(
            5,
            &leader,
            AbsenceReason::NoBlock,
            set.quorum,
            true,
            set.quorum,
            0,
        );
        tracker
            .sign_if_permitted(
                5,
                leader,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                100_000,
                &keys[1],
            )
            .expect("skip");
        assert!(tracker.refuses(5, &leader));

        // Restart: the record is reloaded and the refusal comes back with it.
        let encoded = tracker.record().encode();
        let restored = ConsensusSigningRecord::decode(&encoded).expect("decodes");
        let reloaded = SkipTracker::new(
            domain(),
            validator_set_hash(&set),
            DEFAULT_SKIP_GRACE_MS,
            restored,
        );
        assert!(reloaded.refuses(5, &leader));
    }

    #[test]
    fn adopting_a_peer_certificate_inherits_the_refusal() {
        let (set, keys) = committee(4);
        let mut tracker = tracker(&set);
        let leader = keys[0].address();
        let votes = skip_votes(&set, &keys, 11, leader, 3);
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            11,
            leader,
            AbsenceReason::NoBlock,
            votes,
        );
        certificate.verify(&domain(), &set).expect("valid");
        tracker.adopt_certificate(&certificate);
        assert!(tracker.refuses(11, &leader));
        // The refusal is specific to the member the certificate names.
        assert!(!tracker.refuses(11, &keys[2].address()));
        assert!(!tracker.refuses(12, &leader));
    }

    // ── collectors ──────────────────────────────────────────────────────────

    #[test]
    fn the_collector_emits_a_certificate_exactly_at_quorum() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let mut collector = SkipVoteCollector::new();
        let votes = skip_votes(&set, &keys, 2, leader, 3);
        assert!(
            collector
                .add(votes[0].clone(), &domain(), &set)
                .unwrap()
                .is_none()
        );
        assert!(
            collector
                .add(votes[1].clone(), &domain(), &set)
                .unwrap()
                .is_none()
        );
        let certificate = collector
            .add(votes[2].clone(), &domain(), &set)
            .unwrap()
            .expect("quorum reached");
        certificate.verify(&domain(), &set).expect("valid");
    }

    #[test]
    fn the_collector_ignores_a_repeated_voter() {
        let (set, keys) = committee(4);
        let leader = keys[0].address();
        let mut collector = SkipVoteCollector::new();
        let votes = skip_votes(&set, &keys, 2, leader, 2);
        collector.add(votes[0].clone(), &domain(), &set).unwrap();
        for _ in 0..5 {
            assert!(
                collector
                    .add(votes[0].clone(), &domain(), &set)
                    .unwrap()
                    .is_none(),
                "one voter must never reach quorum by repeating itself"
            );
        }
        assert!(
            collector
                .add(votes[1].clone(), &domain(), &set)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn finality_votes_reach_a_certificate_and_reject_equivocation() {
        let (set, keys) = committee(4);
        let set_hash = validator_set_hash(&set);
        let block = hash_bytes(b"committed-block");
        let state = hash_bytes(b"state-root");
        let tx = hash_bytes(b"tx-root");
        let mut collector = FinalityVoteCollector::new();
        for key in keys.iter().take(2) {
            let vote = FinalityVote::sign(domain(), set_hash, 9, block, state, tx, key).unwrap();
            assert!(collector.add(vote, &domain(), &set).unwrap().is_none());
        }
        // The same voter now claims a different state root for the same block.
        let equivocation = FinalityVote::sign(
            domain(),
            set_hash,
            9,
            block,
            hash_bytes(b"other-state"),
            tx,
            &keys[0],
        )
        .unwrap();
        assert!(matches!(
            collector.add(equivocation, &domain(), &set),
            Err(CertificateError::InconsistentVote { .. })
        ));
        let vote = FinalityVote::sign(domain(), set_hash, 9, block, state, tx, &keys[2]).unwrap();
        let certificate = collector
            .add(vote, &domain(), &set)
            .unwrap()
            .expect("quorum reached");
        let signing = certificate.verify(&domain(), &set).expect("valid");
        assert!(signing >= set.quorum);
        assert_eq!(certificate.state_root, state);
    }

    #[test]
    fn a_finality_certificate_for_another_committee_is_refused() {
        let (set, keys) = committee(4);
        let (other_set, _) = committee(4);
        let set_hash = validator_set_hash(&set);
        let votes: Vec<FinalityVote> = keys[..3]
            .iter()
            .map(|key| {
                FinalityVote::sign(
                    domain(),
                    set_hash,
                    3,
                    hash_bytes(b"b"),
                    hash_bytes(b"s"),
                    hash_bytes(b"t"),
                    key,
                )
                .unwrap()
            })
            .collect();
        let certificate = FinalityCertificate {
            domain: domain(),
            validator_set_hash: set_hash,
            height: 3,
            block_hash: hash_bytes(b"b"),
            state_root: hash_bytes(b"s"),
            tx_root: hash_bytes(b"t"),
            votes,
        };
        assert!(matches!(
            certificate.verify(&domain(), &other_set),
            Err(CertificateError::WrongValidatorSet)
        ));
    }
}

// ── Authenticated checkpoints ────────────────────────────────────────────────

/// Domain tag for the checkpoint transcript.
pub const CHECKPOINT_DOMAIN: &[u8] = b"arc.consensus.checkpoint.v1";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    #[error("the finality certificate is not valid: {0}")]
    Certificate(#[from] CertificateError),
    #[error("snapshot digest {found} does not match the authorised {expected}")]
    DigestMismatch { expected: Hash256, found: Hash256 },
    #[error("snapshot declares state root {found}, certificate authorises {expected}")]
    StateRootMismatch { expected: Hash256, found: Hash256 },
    #[error("snapshot declares height {found}, certificate authorises {expected}")]
    HeightMismatch { expected: u64, found: u64 },
}

/// What a snapshot must say about itself for a checkpoint to authorise it.
///
/// Deliberately minimal and separate from any concrete snapshot format: the
/// checkpoint authorises an identity, and whatever carries the bytes must prove
/// it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotIdentity {
    pub height: u64,
    pub state_root: Hash256,
    /// BLAKE3 over the exact bytes of the snapshot payload.
    pub digest: Hash256,
}

/// A quorum-authenticated statement that one state root is canonical at one
/// height, and that a specific snapshot payload is the one being authorised.
///
/// This is the trust boundary for importing state a node did not compute
/// itself, and it is deliberately separate from loading a node's own validated
/// local snapshot. Its authority comes entirely from the finality certificate:
/// a quorum of the frozen committee that each independently committed and
/// executed that exact block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointEnvelope {
    pub certificate: FinalityCertificate,
    pub snapshot: SnapshotIdentity,
}

impl CheckpointEnvelope {
    /// Verify everything that can be checked without the payload: the
    /// certificate against the frozen committee, and that the snapshot identity
    /// is the one the certificate authorises.
    pub fn verify(
        &self,
        expected_domain: &ConsensusDomain,
        set: &ValidatorSet,
    ) -> Result<u64, CheckpointError> {
        let signing = self.certificate.verify(expected_domain, set)?;
        if self.snapshot.height != self.certificate.height {
            return Err(CheckpointError::HeightMismatch {
                expected: self.certificate.height,
                found: self.snapshot.height,
            });
        }
        if self.snapshot.state_root != self.certificate.state_root {
            return Err(CheckpointError::StateRootMismatch {
                expected: self.certificate.state_root,
                found: self.snapshot.state_root,
            });
        }
        Ok(signing)
    }

    /// Verify the envelope AND that these bytes are the payload it authorises.
    ///
    /// The caller must not touch the payload before this returns `Ok`.
    pub fn verify_payload(
        &self,
        payload: &[u8],
        expected_domain: &ConsensusDomain,
        set: &ValidatorSet,
    ) -> Result<u64, CheckpointError> {
        let signing = self.verify(expected_domain, set)?;
        let digest = hash_bytes(payload);
        if digest != self.snapshot.digest {
            return Err(CheckpointError::DigestMismatch {
                expected: self.snapshot.digest,
                found: digest,
            });
        }
        Ok(signing)
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use crate::{STAKE_ARC, Validator};

    fn domain() -> ConsensusDomain {
        ConsensusDomain::new(hash_bytes(b"arc.checkpoint.test"), 1, 1)
    }

    fn committee(n: usize) -> (ValidatorSet, Vec<KeyPair>) {
        let keys: Vec<KeyPair> = (0..n)
            .map(|i| {
                KeyPair::from_ed25519_secret_bytes(
                    &hash_bytes(format!("checkpoint-{i}").as_bytes()).0,
                )
            })
            .collect();
        let validators: Vec<Validator> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| Validator::new(key.address(), STAKE_ARC, i as u16).unwrap())
            .collect();
        (ValidatorSet::new(validators, 1), keys)
    }

    fn envelope(
        set: &ValidatorSet,
        keys: &[KeyPair],
        signers: usize,
        payload: &[u8],
        state_root: Hash256,
    ) -> CheckpointEnvelope {
        let set_hash = validator_set_hash(set);
        let block = hash_bytes(b"checkpoint-block");
        let tx = hash_bytes(b"checkpoint-tx-root");
        let votes: Vec<FinalityVote> = keys[..signers]
            .iter()
            .map(|key| {
                FinalityVote::sign(domain(), set_hash, 900, block, state_root, tx, key).unwrap()
            })
            .collect();
        CheckpointEnvelope {
            certificate: FinalityCertificate {
                domain: domain(),
                validator_set_hash: set_hash,
                height: 900,
                block_hash: block,
                state_root,
                tx_root: tx,
                votes,
            },
            snapshot: SnapshotIdentity {
                height: 900,
                state_root,
                digest: hash_bytes(payload),
            },
        }
    }

    #[test]
    fn a_quorum_signed_checkpoint_over_the_exact_payload_verifies() {
        let (set, keys) = committee(4);
        let payload = b"the state at height 900";
        let root = hash_bytes(b"state-900");
        let checkpoint = envelope(&set, &keys, 3, payload, root);
        let signing = checkpoint
            .verify_payload(payload, &domain(), &set)
            .expect("valid checkpoint");
        assert!(signing >= set.quorum);
    }

    #[test]
    fn a_checkpoint_below_quorum_is_refused() {
        let (set, keys) = committee(4);
        let payload = b"state";
        let checkpoint = envelope(&set, &keys, 2, payload, hash_bytes(b"r"));
        assert!(matches!(
            checkpoint.verify_payload(payload, &domain(), &set),
            Err(CheckpointError::Certificate(
                CertificateError::BelowQuorum { .. }
            ))
        ));
    }

    #[test]
    fn a_checkpoint_from_another_committee_is_refused() {
        let (set, keys) = committee(4);
        let (other, _) = committee(5);
        let payload = b"state";
        let checkpoint = envelope(&set, &keys, 3, payload, hash_bytes(b"r"));
        assert!(matches!(
            checkpoint.verify_payload(payload, &domain(), &other),
            Err(CheckpointError::Certificate(
                CertificateError::WrongValidatorSet
            ))
        ));
    }

    #[test]
    fn a_checkpoint_for_another_chain_is_refused() {
        let (set, keys) = committee(4);
        let payload = b"state";
        let checkpoint = envelope(&set, &keys, 3, payload, hash_bytes(b"r"));
        let elsewhere = ConsensusDomain::new(hash_bytes(b"another.chain"), 1, 1);
        assert!(matches!(
            checkpoint.verify_payload(payload, &elsewhere, &set),
            Err(CheckpointError::Certificate(
                CertificateError::WrongDomain { .. }
            ))
        ));
    }

    #[test]
    fn a_payload_that_is_not_the_authorised_one_is_refused() {
        let (set, keys) = committee(4);
        let payload = b"the state at height 900";
        let checkpoint = envelope(&set, &keys, 3, payload, hash_bytes(b"r"));
        assert!(matches!(
            checkpoint.verify_payload(b"a different state", &domain(), &set),
            Err(CheckpointError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn a_snapshot_claiming_a_root_the_committee_did_not_sign_is_refused() {
        let (set, keys) = committee(4);
        let payload = b"state";
        let mut checkpoint = envelope(&set, &keys, 3, payload, hash_bytes(b"r"));
        checkpoint.snapshot.state_root = hash_bytes(b"a root nobody signed");
        assert!(matches!(
            checkpoint.verify_payload(payload, &domain(), &set),
            Err(CheckpointError::StateRootMismatch { .. })
        ));
    }

    #[test]
    fn a_snapshot_claiming_another_height_is_refused() {
        let (set, keys) = committee(4);
        let payload = b"state";
        let mut checkpoint = envelope(&set, &keys, 3, payload, hash_bytes(b"r"));
        checkpoint.snapshot.height = 901;
        assert!(matches!(
            checkpoint.verify_payload(payload, &domain(), &set),
            Err(CheckpointError::HeightMismatch { .. })
        ));
    }

    #[test]
    fn an_unsigned_checkpoint_is_refused() {
        let (set, keys) = committee(4);
        let payload = b"state";
        let mut checkpoint = envelope(&set, &keys, 3, payload, hash_bytes(b"r"));
        checkpoint.certificate.votes.clear();
        assert!(matches!(
            checkpoint.verify_payload(payload, &domain(), &set),
            Err(CheckpointError::Certificate(CertificateError::Empty))
        ));
    }
}
