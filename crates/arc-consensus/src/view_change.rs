use std::collections::{HashMap, HashSet};

use arc_crypto::{Hash256, KeyPair, Signature, hash_bytes};
use arc_types::Address;
use serde::{Deserialize, Serialize};

use crate::{ConsensusDomain, ValidatorSet};

pub const SKIP_VOTE_DOMAIN: &[u8] = b"arc.consensus.skipvote.v1";
pub const FINALITY_VOTE_DOMAIN: &[u8] = b"arc.consensus.finalityvote.v1";
pub const VALIDATOR_SET_DOMAIN: &[u8] = b"arc.consensus.validatorset.v1";

pub const DEFAULT_SKIP_GRACE_MS: u64 = 2_000;
pub const RECENT_ABSENCE_GRACE_MS: u64 = 250;
pub const RECENT_ABSENCE_ROUNDS: u64 = 3;

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

pub fn validator_set_hash(set: &ValidatorSet) -> Hash256 {
    let mut members: Vec<(Address, u64)> = set
        .validators
        .iter()
        .map(|validator| (validator.address, validator.stake))
        .collect();
    members.sort_by_key(|member| member.0.0);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AbsenceReason {
    NoBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipVote {
    pub domain: ConsensusDomain,
    pub validator_set_hash: Hash256,
    pub round: u64,
    pub absentee: Address,
    pub voter: Address,
    pub reason: AbsenceReason,
    pub observed_round_stake: u64,
    pub signature: Signature,
}

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
        let mut transcripts = Vec::with_capacity(self.votes.len());
        let mut signatures = Vec::with_capacity(self.votes.len());
        let mut voters = Vec::with_capacity(self.votes.len());
        let mut members_stake = Vec::with_capacity(self.votes.len());

        for (index, vote) in self.votes.iter().enumerate() {
            if vote.domain != self.domain
                || vote.validator_set_hash != self.validator_set_hash
                || vote.round != self.round
                || vote.absentee != self.absentee
                || vote.reason != self.reason
            {
                return Err(CertificateError::InconsistentVote { index });
            }
            if vote.observed_round_stake < set.quorum {
                return Err(CertificateError::InconsistentVote { index });
            }
            if !seen.insert(vote.voter) {
                return Err(CertificateError::DuplicateVoter(vote.voter));
            }
            let validator = set
                .get_validator(&vote.voter)
                .ok_or(CertificateError::UnknownVoter(vote.voter))?;
            members_stake.push(validator.stake);
            transcripts.push(vote.transcript());
            signatures.push(&vote.signature);
            voters.push(vote.voter);
        }

        if !signatures.is_empty()
            && arc_crypto::batch_verify_arc_ed25519(&transcripts, &signatures, &voters).is_ok()
        {
            for stake in members_stake {
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
            return Ok(signing);
        }

        signing = 0;
        for vote in &self.votes {
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
        let mut transcripts = Vec::with_capacity(self.votes.len());
        let mut signatures = Vec::with_capacity(self.votes.len());
        let mut voters = Vec::with_capacity(self.votes.len());
        let mut members_stake = Vec::with_capacity(self.votes.len());

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
            let validator = set
                .get_validator(&vote.voter)
                .ok_or(CertificateError::UnknownVoter(vote.voter))?;
            members_stake.push(validator.stake);
            transcripts.push(vote.transcript());
            signatures.push(&vote.signature);
            voters.push(vote.voter);
        }

        if !signatures.is_empty()
            && arc_crypto::batch_verify_arc_ed25519(&transcripts, &signatures, &voters).is_ok()
        {
            for stake in members_stake {
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
            return Ok(signing);
        }

        signing = 0;
        for vote in &self.votes {
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsensusSigningRecord {
    pub skipped_rounds: HashMap<u64, HashMap<Address, AbsenceReason>>,
    pub finality_votes: HashMap<u64, (Hash256, Hash256, Hash256)>,
    #[serde(default)]
    pub last_applied_round: Option<u64>,
}

impl ConsensusSigningRecord {
    pub fn prune(&mut self, finality_floor_height: u64, absence_floor_round: u64) {
        self.finality_votes
            .retain(|height, _| *height >= finality_floor_height);
        self.skipped_rounds
            .retain(|round, _| *round >= absence_floor_round);
    }

    pub fn next_round_to_scan(&self) -> u64 {
        match self.last_applied_round {
            Some(applied) => applied.saturating_add(1),
            None => 0,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(SIGNING_RECORD_MAGIC);
        bytes.push(SIGNING_RECORD_VERSION);
        bytes.extend_from_slice(&bincode::serialize(self).expect("signing record is serialisable"));
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SigningRecordError> {
        if let Some(rest) = bytes.strip_prefix(SIGNING_RECORD_MAGIC) {
            let (version, payload) = rest.split_first().ok_or(SigningRecordError::Truncated)?;
            if *version != SIGNING_RECORD_VERSION {
                return Err(SigningRecordError::UnknownVersion(*version));
            }
            return bincode::deserialize(payload).map_err(SigningRecordError::Malformed);
        }
        let legacy: LegacySigningRecordV1 =
            bincode::deserialize(bytes).map_err(SigningRecordError::Malformed)?;
        Ok(legacy.migrate())
    }
}

const SIGNING_RECORD_MAGIC: &[u8] = b"ARC-SIGNREC";
const SIGNING_RECORD_VERSION: u8 = 2;

#[derive(Deserialize)]
struct LegacySigningRecordV1 {
    skipped_rounds: HashMap<u64, HashMap<Address, AbsenceReason>>,
    finality_votes: HashMap<u64, (Hash256, Hash256, Hash256)>,
    durable_commit_round: u64,
}

impl LegacySigningRecordV1 {
    fn migrate(self) -> ConsensusSigningRecord {
        ConsensusSigningRecord {
            skipped_rounds: self.skipped_rounds,
            finality_votes: self.finality_votes,
            last_applied_round: (self.durable_commit_round > 0)
                .then_some(self.durable_commit_round),
        }
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipRefusal {
    RoundBelowQuorum,
    AbsenteeBlockPresent,
    WithinGrace,
    ConflictingSkip,
    RoundAlreadyPassed,
}

#[derive(Debug, Clone, Copy)]
struct RoundObservation {
    stake: u64,
    condition_broken: bool,
    holding_since: Option<u64>,
}

#[derive(Debug)]
pub struct SkipTracker {
    domain: ConsensusDomain,
    validator_set_hash: Hash256,
    grace_ms: u64,
    recent_absence_grace_ms: u64,
    recent_absence: HashMap<Address, u64>,
    observations: HashMap<(u64, Address, AbsenceReason), RoundObservation>,
    record: ConsensusSigningRecord,
    refused: HashSet<(u64, Address)>,
}

impl SkipTracker {
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
        let mut recent_absence: HashMap<Address, u64> = HashMap::new();
        for (round, members) in &record.skipped_rounds {
            for (member, reason) in members {
                if *reason == AbsenceReason::NoBlock {
                    recent_absence
                        .entry(*member)
                        .and_modify(|recent| *recent = (*recent).max(*round))
                        .or_insert(*round);
                }
            }
        }
        Self {
            domain,
            validator_set_hash,
            grace_ms,
            recent_absence_grace_ms: grace_ms,
            recent_absence,
            observations: HashMap::new(),
            record,
            refused,
        }
    }

    pub fn with_recent_absence_grace(mut self, grace_ms: u64) -> Self {
        self.recent_absence_grace_ms = grace_ms.min(self.grace_ms);
        self
    }

    pub fn record(&self) -> &ConsensusSigningRecord {
        &self.record
    }

    pub fn refuses(&self, round: u64, absentee: &Address) -> bool {
        self.refused.contains(&(round, *absentee))
    }

    pub fn note_durable_commit_round(&mut self, round: u64) {
        if self
            .record
            .last_applied_round
            .is_none_or(|applied| round > applied)
        {
            self.record.last_applied_round = Some(round);
        }
    }

    pub fn note_finality_vote(&mut self, height: u64, identity: (Hash256, Hash256, Hash256)) {
        self.record.finality_votes.insert(height, identity);
    }

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
        if reason == AbsenceReason::NoBlock
            && !condition_holds
            && self.recent_absence.get(member).is_some_and(|r| round > *r)
        {
            self.recent_absence.remove(member);
        }
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
        let grace_ms = if reason == AbsenceReason::NoBlock
            && self
                .recent_absence
                .get(&absentee)
                .is_some_and(|r| round <= r.saturating_add(RECENT_ABSENCE_ROUNDS))
        {
            self.recent_absence_grace_ms
        } else {
            self.grace_ms
        };
        if now_ms.saturating_sub(since) < grace_ms {
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
        if reason == AbsenceReason::NoBlock {
            self.recent_absence
                .entry(absentee)
                .and_modify(|recent| *recent = (*recent).max(round))
                .or_insert(round);
        }
        Ok(vote)
    }

    pub fn adopt_certificate(&mut self, certificate: &SkipCertificate) {
        self.record
            .skipped_rounds
            .entry(certificate.round)
            .or_default()
            .insert(certificate.absentee, certificate.reason);
        self.refused
            .insert((certificate.round, certificate.absentee));
        if certificate.reason == AbsenceReason::NoBlock {
            self.recent_absence
                .entry(certificate.absentee)
                .and_modify(|recent| *recent = (*recent).max(certificate.round))
                .or_insert(certificate.round);
        }
    }

    pub fn prune_observations_below(&mut self, round: u64) {
        self.observations
            .retain(|(tracked, _, _), _| *tracked >= round);
        self.refused.retain(|(tracked, _)| *tracked >= round);
    }

    pub fn prune_record(&mut self, finality_floor_height: u64, absence_floor_round: u64) {
        self.record
            .prune(finality_floor_height, absence_floor_round);
    }
}

#[derive(Debug, Default)]
pub struct SkipVoteCollector {
    by_round: HashMap<(u64, Address, AbsenceReason), HashMap<Address, SkipVote>>,
}

impl SkipVoteCollector {
    pub fn len(&self) -> usize {
        self.by_round.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_round.is_empty()
    }

    pub fn new() -> Self {
        Self::default()
    }

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

#[derive(Debug, Default)]
pub struct FinalityVoteCollector {
    by_block: HashMap<(u64, Hash256), HashMap<Address, FinalityVote>>,
}

impl FinalityVoteCollector {
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

    #[test]
    fn two_quorums_always_intersect_in_more_than_the_byzantine_bound() {
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
        for n in 2..=16usize {
            let (set, _) = committee(n);
            let f = set.total_stake - set.quorum;
            assert!(
                set.total_stake - f >= set.quorum,
                "n={n}: honest stake cannot reach quorum"
            );
        }
    }

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

    fn tracker(set: &ValidatorSet) -> SkipTracker {
        SkipTracker::new(
            domain(),
            validator_set_hash(set),
            DEFAULT_SKIP_GRACE_MS,
            ConsensusSigningRecord::default(),
        )
    }

    fn assert_no_block_grace(
        tracker: &mut SkipTracker,
        set: &ValidatorSet,
        signer: &KeyPair,
        member: Address,
        round: u64,
        now_ms: u64,
        grace_ms: u64,
    ) {
        tracker.observe(
            round,
            &member,
            AbsenceReason::NoBlock,
            set.quorum,
            true,
            set.quorum,
            now_ms,
        );
        assert_eq!(
            tracker.sign_if_permitted(
                round,
                member,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                now_ms + grace_ms - 1,
                signer,
            ),
            Err(SkipRefusal::WithinGrace)
        );
        let vote = tracker
            .sign_if_permitted(
                round,
                member,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                now_ms + grace_ms,
                signer,
            )
            .expect("grace elapsed");
        assert_eq!(vote.round, round);
        assert_eq!(vote.absentee, member);
        assert!(tracker.refuses(round, &member));
        assert_eq!(
            tracker.record().skipped_rounds[&round].get(&member),
            Some(&AbsenceReason::NoBlock)
        );
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
    fn an_own_absence_signature_shortens_only_recent_absences_of_that_member() {
        let (set, keys) = committee(4);
        let mut tracker = tracker(&set).with_recent_absence_grace(RECENT_ABSENCE_GRACE_MS);
        let member = keys[0].address();
        let round = 5;
        assert_no_block_grace(
            &mut tracker,
            &set,
            &keys[1],
            member,
            round,
            1_000,
            DEFAULT_SKIP_GRACE_MS,
        );
        assert_no_block_grace(
            &mut tracker,
            &set,
            &keys[1],
            member,
            round + 1,
            4_000,
            RECENT_ABSENCE_GRACE_MS,
        );
        assert_no_block_grace(
            &mut tracker,
            &set,
            &keys[1],
            member,
            round + 1 + RECENT_ABSENCE_ROUNDS + 1,
            5_000,
            DEFAULT_SKIP_GRACE_MS,
        );
        assert_no_block_grace(
            &mut tracker,
            &set,
            &keys[1],
            keys[2].address(),
            round + 2 + RECENT_ABSENCE_ROUNDS,
            8_000,
            DEFAULT_SKIP_GRACE_MS,
        );
    }

    #[test]
    fn only_a_newer_block_restores_the_full_absence_grace() {
        let (set, keys) = committee(4);
        let member = keys[0].address();
        for block_round in [4, 5, 6] {
            let mut tracker = tracker(&set).with_recent_absence_grace(RECENT_ABSENCE_GRACE_MS);
            assert_no_block_grace(
                &mut tracker,
                &set,
                &keys[1],
                member,
                5,
                1_000,
                DEFAULT_SKIP_GRACE_MS,
            );
            tracker.observe(
                block_round,
                &member,
                AbsenceReason::NoBlock,
                0,
                false,
                set.quorum,
                4_000,
            );
            let grace_ms = if block_round > 5 {
                DEFAULT_SKIP_GRACE_MS
            } else {
                RECENT_ABSENCE_GRACE_MS
            };
            assert_no_block_grace(&mut tracker, &set, &keys[1], member, 7, 5_000, grace_ms);
        }
    }

    #[test]
    fn recent_absence_grace_is_opt_in_and_clamped_to_the_full_grace() {
        let (set, keys) = committee(4);
        let member = keys[0].address();
        for grace_ms in [None, Some(DEFAULT_SKIP_GRACE_MS + 1)] {
            let mut tracker = tracker(&set);
            if let Some(grace_ms) = grace_ms {
                tracker = tracker.with_recent_absence_grace(grace_ms);
            }
            for (round, now_ms) in [(5, 1_000), (6, 4_000)] {
                assert_no_block_grace(
                    &mut tracker,
                    &set,
                    &keys[1],
                    member,
                    round,
                    now_ms,
                    DEFAULT_SKIP_GRACE_MS,
                );
            }
        }
    }

    #[test]
    fn restarting_seeds_recent_absence_from_the_highest_recorded_round() {
        let (set, keys) = committee(4);
        let member = keys[0].address();
        let mut record = ConsensusSigningRecord::default();
        for round in [5, 1, 4] {
            record
                .skipped_rounds
                .entry(round)
                .or_default()
                .insert(member, AbsenceReason::NoBlock);
        }
        for round in [6, 5 + RECENT_ABSENCE_ROUNDS] {
            let restored = ConsensusSigningRecord::decode(&record.encode()).expect("decodes");
            let mut tracker = SkipTracker::new(
                domain(),
                validator_set_hash(&set),
                DEFAULT_SKIP_GRACE_MS,
                restored,
            )
            .with_recent_absence_grace(RECENT_ABSENCE_GRACE_MS);
            assert_no_block_grace(
                &mut tracker,
                &set,
                &keys[1],
                member,
                round,
                1_000,
                RECENT_ABSENCE_GRACE_MS,
            );
        }
    }

    #[test]
    fn older_attestations_cannot_lower_the_recent_absence_round() {
        let (set, keys) = committee(4);
        let member = keys[0].address();
        let mut tracker = tracker(&set).with_recent_absence_grace(RECENT_ABSENCE_GRACE_MS);
        assert_no_block_grace(
            &mut tracker,
            &set,
            &keys[1],
            member,
            10,
            1_000,
            DEFAULT_SKIP_GRACE_MS,
        );
        assert_no_block_grace(
            &mut tracker,
            &set,
            &keys[1],
            member,
            5,
            4_000,
            RECENT_ABSENCE_GRACE_MS,
        );
        let certificate = SkipCertificate::new(
            domain(),
            validator_set_hash(&set),
            4,
            member,
            AbsenceReason::NoBlock,
            skip_votes(&set, &keys, 4, member, 3),
        );
        certificate.verify(&domain(), &set).expect("valid");
        tracker.adopt_certificate(&certificate);
        assert_no_block_grace(
            &mut tracker,
            &set,
            &keys[1],
            member,
            11,
            5_000,
            RECENT_ABSENCE_GRACE_MS,
        );
    }

    #[test]
    fn recent_absence_window_includes_its_last_round_and_saturates() {
        let (set, keys) = committee(4);
        let member = keys[0].address();
        for (round, next_round) in [(5, 5 + RECENT_ABSENCE_ROUNDS), (u64::MAX - 1, u64::MAX)] {
            let mut tracker = tracker(&set).with_recent_absence_grace(RECENT_ABSENCE_GRACE_MS);
            assert_no_block_grace(
                &mut tracker,
                &set,
                &keys[1],
                member,
                round,
                1_000,
                DEFAULT_SKIP_GRACE_MS,
            );
            assert_no_block_grace(
                &mut tracker,
                &set,
                &keys[1],
                member,
                next_round,
                4_000,
                RECENT_ABSENCE_GRACE_MS,
            );
        }
    }

    #[test]
    fn the_applied_round_is_monotonic_and_converts_to_a_scan_cursor() {
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

        for record in [fresh.record().clone(), applied_genesis.record().clone()] {
            let round_tripped = ConsensusSigningRecord::decode(&record.encode()).expect("decodes");
            assert_eq!(round_tripped.last_applied_round, record.last_applied_round);
            assert_eq!(
                round_tripped.next_round_to_scan(),
                record.next_round_to_scan()
            );
        }
    }

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
        assert!(!tracker.record().finality_votes.contains_key(&10));
    }

    #[test]
    fn a_long_run_record_stays_bounded_when_pruned() {
        let mut tracker = SkipTracker::new(
            domain(),
            Hash256::ZERO,
            DEFAULT_SKIP_GRACE_MS,
            ConsensusSigningRecord::default(),
        );
        let margin = 256u64;
        let every = 128u64;
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
        let record = ConsensusSigningRecord {
            last_applied_round: Some(9),
            ..ConsensusSigningRecord::default()
        };
        let bytes = record.encode();
        assert!(bytes.starts_with(SIGNING_RECORD_MAGIC));
        assert_eq!(bytes[SIGNING_RECORD_MAGIC.len()], SIGNING_RECORD_VERSION);
        assert_eq!(ConsensusSigningRecord::decode(&bytes).unwrap(), record);
    }

    #[test]
    fn two_members_of_one_round_can_both_be_attested() {
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
        let mut tracker = tracker(&set).with_recent_absence_grace(RECENT_ABSENCE_GRACE_MS);
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
        assert!(!tracker.refuses(11, &keys[2].address()));
        assert!(!tracker.refuses(12, &leader));
        assert_no_block_grace(
            &mut tracker,
            &set,
            &keys[1],
            leader,
            12,
            1_000,
            RECENT_ABSENCE_GRACE_MS,
        );
    }

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
    #[error("the checkpoint's tip block is not the certified block at its height")]
    TipMismatch,
    #[error("the declared anchor does not match the decision commitment in the certified header")]
    AnchorMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotIdentity {
    pub height: u64,
    pub state_root: Hash256,
    pub digest: Hash256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointEnvelope {
    pub certificate: FinalityCertificate,
    pub snapshot: SnapshotIdentity,
    pub anchor_hash: Hash256,
    pub anchor_round: u64,
}

impl CheckpointEnvelope {
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

    pub fn verify_resume_point(
        &self,
        tip: &arc_types::Block,
        domain: &ConsensusDomain,
    ) -> Result<u64, CheckpointError> {
        if tip.hash != self.certificate.block_hash
            || tip.header.height != self.certificate.height
            || arc_types::Block::compute_hash(&tip.header) != tip.hash
        {
            return Err(CheckpointError::TipMismatch);
        }
        let commitment =
            crate::DagBlock::decision_commitment(domain, &self.anchor_hash, self.anchor_round);
        if commitment != tip.header.proof_hash {
            return Err(CheckpointError::AnchorMismatch);
        }
        self.anchor_round
            .checked_add(1)
            .ok_or(CheckpointError::AnchorMismatch)
    }

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

    fn committed_tip(height: u64, anchor: Hash256, round: u64) -> arc_types::Block {
        let mut tip = arc_types::Block::genesis();
        tip.header.height = height;
        tip.header.proof_hash = crate::DagBlock::decision_commitment(&domain(), &anchor, round);
        tip.hash = arc_types::Block::compute_hash(&tip.header);
        tip
    }

    #[test]
    fn a_checkpoint_resume_point_is_proven_from_the_certified_header() {
        let anchor = hash_bytes(b"anchor");
        let tip = committed_tip(900, anchor, 4_000);
        let mut envelope = CheckpointEnvelope {
            certificate: FinalityCertificate {
                domain: domain(),
                validator_set_hash: Hash256::ZERO,
                height: 900,
                block_hash: tip.hash,
                state_root: Hash256::ZERO,
                tx_root: Hash256::ZERO,
                votes: vec![],
            },
            snapshot: SnapshotIdentity {
                height: 900,
                state_root: Hash256::ZERO,
                digest: Hash256::ZERO,
            },
            anchor_hash: anchor,
            anchor_round: 4_000,
        };
        assert_eq!(envelope.verify_resume_point(&tip, &domain()), Ok(4_001));

        envelope.anchor_round = 3_999;
        assert_eq!(
            envelope.verify_resume_point(&tip, &domain()),
            Err(CheckpointError::AnchorMismatch)
        );
        envelope.anchor_round = 4_000;
        envelope.anchor_hash = hash_bytes(b"another anchor");
        assert_eq!(
            envelope.verify_resume_point(&tip, &domain()),
            Err(CheckpointError::AnchorMismatch)
        );
        envelope.anchor_hash = anchor;

        let other = committed_tip(900, anchor, 4_001);
        assert_eq!(
            envelope.verify_resume_point(&other, &domain()),
            Err(CheckpointError::TipMismatch)
        );
        let mut tampered = tip.clone();
        tampered.header.timestamp += 1;
        assert_eq!(
            envelope.verify_resume_point(&tampered, &domain()),
            Err(CheckpointError::TipMismatch)
        );
    }

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
            anchor_hash: Hash256::ZERO,
            anchor_round: 0,
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
