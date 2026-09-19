//! Inactive, versioned native inference contract primitives.
//!
//! This module is deliberately pure: it validates commitments and signatures
//! and returns a settlement delta for a future state adapter. It does not
//! mutate balances, activate a transaction body, persist a marker, or claim
//! crash-safe exactly-once behavior.

use arc_crypto::signature::{KeyPair, Signature, SignatureError};
use arc_crypto::{Hash256, hash_bytes};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

pub const INFERENCE_CONTRACT_VERSION: u16 = 1;
pub const MAX_COMMITTEE_MEMBERS: usize = 32;

const JOB_DOMAIN: &str = "ARC-native-inference-job-v1";
const REQUEST_ID_DOMAIN: &str = "ARC-native-inference-request-id-v1";
const COMMITTEE_DOMAIN: &str = "ARC-native-inference-validator-set-v1";
const REQUEST_SIGNATURE_DOMAIN: &str = "ARC-native-inference-request-signature-v1";
const VOTE_SIGNATURE_DOMAIN: &str = "ARC-native-inference-vote-signature-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceDomain {
    pub chain_genesis: Hash256,
    pub recovery_epoch: u64,
    pub validator_set_hash: Hash256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorMember {
    pub address: Hash256,
    pub stake: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceJob {
    pub version: u16,
    pub domain: InferenceDomain,
    pub requester: Hash256,
    pub nonce: u64,
    pub model_hash: Hash256,
    pub profile_hash: Hash256,
    pub input_hash: Hash256,
    pub generation_hash: Hash256,
    pub assignment_hash: Hash256,
    pub max_tokens: u32,
    pub max_output_bytes: u32,
    pub execution_price: u64,
    pub reserved_max_payment: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub job: InferenceJob,
    pub requester_signature: Signature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceVote {
    pub validator: Hash256,
    pub output_hash: Hash256,
    pub signature: Signature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceCertificate {
    pub output: Vec<u8>,
    pub votes: Vec<InferenceVote>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInference {
    request: InferenceRequest,
    request_id: Hash256,
    reserved_balance: u64,
}

impl PendingInference {
    pub fn request_id(&self) -> Hash256 {
        self.request_id
    }

    pub fn job(&self) -> &InferenceJob {
        &self.request.job
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementCredit {
    pub payee: Hash256,
    pub amount: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementPlan {
    Finalize {
        request_id: Hash256,
        consume_pending: Hash256,
        charged: u64,
        credits: Vec<SettlementCredit>,
        output_hash: Hash256,
    },
    Refund {
        request_id: Hash256,
        consume_pending: Hash256,
        charged: u64,
        credits: Vec<SettlementCredit>,
    },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InferenceContractError {
    #[error("unsupported inference contract version")]
    UnsupportedVersion,
    #[error("job domain does not match trusted state domain")]
    DomainMismatch,
    #[error("validator set must contain 1..={MAX_COMMITTEE_MEMBERS} sorted unique members")]
    InvalidCommittee,
    #[error("validator stake sum overflow or zero")]
    InvalidStakeTotal,
    #[error("validator set commitment does not match job domain")]
    ValidatorSetMismatch,
    #[error("invalid job bounds")]
    InvalidBounds,
    #[error("request is expired")]
    Expired,
    #[error("request signature is invalid: {0}")]
    RequestSignature(String),
    #[error("request id does not match commitment")]
    RequestIdMismatch,
    #[error("output blob is empty, oversized, or has the wrong hash")]
    InvalidOutput,
    #[error("certificate must contain a matching two-thirds-plus stake certificate")]
    InsufficientStake,
    #[error("duplicate or non-member validator vote")]
    InvalidVoteMember,
    #[error("vote signature is invalid: {0}")]
    VoteSignature(String),
    #[error("execution price exceeds reserved payment")]
    PriceExceedsReserve,
    #[error("settlement arithmetic overflow")]
    ArithmeticOverflow,
    #[error("reserved balance does not equal the committed reserve")]
    ReserveMismatch,
}

fn domain_hash(label: &str, fields: &[&[u8]]) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key(label);
    for field in fields {
        hasher.update(field);
    }
    Hash256(*hasher.finalize().as_bytes())
}

impl ValidatorMember {
    pub fn new(address: Hash256, stake: u64) -> Self {
        Self { address, stake }
    }
}

/// Validate the state-provided frozen member list and return its checked total.
/// The state adapter must source this list from the trusted validator registry.
pub fn validate_members(members: &[ValidatorMember]) -> Result<u64, InferenceContractError> {
    if members.is_empty() || members.len() > MAX_COMMITTEE_MEMBERS {
        return Err(InferenceContractError::InvalidCommittee);
    }
    let mut total = 0u64;
    for (index, member) in members.iter().enumerate() {
        if member.stake == 0 || (index > 0 && members[index - 1].address.0 >= member.address.0) {
            return Err(InferenceContractError::InvalidCommittee);
        }
        total = total
            .checked_add(member.stake)
            .ok_or(InferenceContractError::InvalidStakeTotal)?;
    }
    if total == 0 {
        return Err(InferenceContractError::InvalidStakeTotal);
    }
    Ok(total)
}

pub fn validator_set_commitment(
    members: &[ValidatorMember],
) -> Result<Hash256, InferenceContractError> {
    validate_members(members)?;
    let mut encoded = Vec::with_capacity(4 + members.len() * 40);
    encoded.extend_from_slice(&(members.len() as u32).to_le_bytes());
    for member in members {
        encoded.extend_from_slice(member.address.as_ref());
        encoded.extend_from_slice(&member.stake.to_le_bytes());
    }
    Ok(domain_hash(COMMITTEE_DOMAIN, &[&encoded]))
}

impl InferenceJob {
    /// Fixed-width, explicit little-endian encoding. No serde or map ordering
    /// is part of the wire commitment.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(3 * 32 + 8 * 32 + 4 * 2 + 8 * 2);
        encoded.extend_from_slice(&self.version.to_le_bytes());
        encoded.extend_from_slice(self.domain.chain_genesis.as_ref());
        encoded.extend_from_slice(&self.domain.recovery_epoch.to_le_bytes());
        for value in [
            self.domain.validator_set_hash,
            self.requester,
            self.model_hash,
            self.profile_hash,
            self.input_hash,
            self.generation_hash,
            self.assignment_hash,
        ] {
            encoded.extend_from_slice(value.as_ref());
        }
        encoded.extend_from_slice(&self.nonce.to_le_bytes());
        encoded.extend_from_slice(&self.max_tokens.to_le_bytes());
        encoded.extend_from_slice(&self.max_output_bytes.to_le_bytes());
        encoded.extend_from_slice(&self.execution_price.to_le_bytes());
        encoded.extend_from_slice(&self.reserved_max_payment.to_le_bytes());
        encoded.extend_from_slice(&self.expires_at.to_le_bytes());
        encoded
    }

    pub fn commitment(&self) -> Hash256 {
        domain_hash(JOB_DOMAIN, &[&self.canonical_bytes()])
    }

    pub fn request_id(&self) -> Hash256 {
        let commitment = self.commitment();
        domain_hash(REQUEST_ID_DOMAIN, &[commitment.as_ref()])
    }
}

impl InferenceRequest {
    pub fn sign(job: InferenceJob, keypair: &KeyPair) -> Result<Self, SignatureError> {
        let message = domain_hash(REQUEST_SIGNATURE_DOMAIN, &[job.commitment().as_ref()]);
        Ok(Self {
            job,
            requester_signature: keypair.sign(&message)?,
        })
    }

    pub fn validate(
        &self,
        expected_domain: &InferenceDomain,
        members: &[ValidatorMember],
        now: u64,
    ) -> Result<PendingInference, InferenceContractError> {
        validate_job(&self.job, expected_domain, members, now)?;
        self.verify_signature()?;
        Ok(PendingInference {
            request: self.clone(),
            request_id: self.job.request_id(),
            reserved_balance: self.job.reserved_max_payment,
        })
    }

    pub fn verify_signature(&self) -> Result<(), InferenceContractError> {
        let message = domain_hash(REQUEST_SIGNATURE_DOMAIN, &[self.job.commitment().as_ref()]);
        self.requester_signature
            .verify(&message, &self.job.requester)
            .map_err(|error| InferenceContractError::RequestSignature(error.to_string()))?;
        Ok(())
    }
}

pub fn validate_job(
    job: &InferenceJob,
    expected_domain: &InferenceDomain,
    members: &[ValidatorMember],
    now: u64,
) -> Result<(), InferenceContractError> {
    validate_job_structure(job, expected_domain, members)?;
    if job.expires_at <= now {
        return Err(InferenceContractError::Expired);
    }
    Ok(())
}

fn validate_job_structure(
    job: &InferenceJob,
    expected_domain: &InferenceDomain,
    members: &[ValidatorMember],
) -> Result<(), InferenceContractError> {
    if job.version != INFERENCE_CONTRACT_VERSION {
        return Err(InferenceContractError::UnsupportedVersion);
    }
    if &job.domain != expected_domain {
        return Err(InferenceContractError::DomainMismatch);
    }
    let _total = validate_members(members)?;
    if validator_set_commitment(members)? != job.domain.validator_set_hash {
        return Err(InferenceContractError::ValidatorSetMismatch);
    }
    if job.execution_price > job.reserved_max_payment {
        return Err(InferenceContractError::PriceExceedsReserve);
    }
    if job.max_tokens == 0
        || job.max_tokens > crate::transaction::TIER1_MAX_TOKENS
        || job.max_output_bytes == 0
        || job.max_output_bytes > crate::transaction::TIER1_OUTPUT_BLOB_MAX as u32
        || job.execution_price == 0
        || job.reserved_max_payment == 0
    {
        return Err(InferenceContractError::InvalidBounds);
    }
    Ok(())
}

fn vote_message(request_id: Hash256, output_hash: Hash256, output_len: usize) -> Hash256 {
    let output_len = output_len as u64;
    domain_hash(
        VOTE_SIGNATURE_DOMAIN,
        &[
            request_id.as_ref(),
            output_hash.as_ref(),
            &output_len.to_le_bytes(),
        ],
    )
}

pub fn sign_vote(
    request_id: Hash256,
    output: &[u8],
    keypair: &KeyPair,
) -> Result<InferenceVote, SignatureError> {
    let output_hash = hash_bytes(output);
    let message = vote_message(request_id, output_hash, output.len());
    Ok(InferenceVote {
        validator: keypair.address(),
        output_hash,
        signature: keypair.sign(&message)?,
    })
}

fn validate_certificate(
    pending: &PendingInference,
    certificate: &InferenceCertificate,
    members: &[ValidatorMember],
    expected_domain: &InferenceDomain,
    now: u64,
) -> Result<(Hash256, Vec<ValidatorMember>), InferenceContractError> {
    validate_job(&pending.request.job, expected_domain, members, now)?;
    pending.request.verify_signature()?;
    if pending.request_id != pending.request.job.request_id() {
        return Err(InferenceContractError::RequestIdMismatch);
    }
    if pending.reserved_balance != pending.request.job.reserved_max_payment {
        return Err(InferenceContractError::ReserveMismatch);
    }
    if certificate.output.is_empty()
        || certificate.output.len() > pending.request.job.max_output_bytes as usize
    {
        return Err(InferenceContractError::InvalidOutput);
    }
    let output_hash = hash_bytes(&certificate.output);
    let total_stake = validate_members(members)?;
    let mut matching = Vec::with_capacity(certificate.votes.len());
    let mut seen = BTreeMap::<[u8; 32], ()>::new();
    for vote in &certificate.votes {
        let member = members
            .iter()
            .find(|member| member.address == vote.validator)
            .ok_or(InferenceContractError::InvalidVoteMember)?;
        if seen.insert(vote.validator.0, ()).is_some() {
            return Err(InferenceContractError::InvalidVoteMember);
        }
        if vote.output_hash != output_hash {
            return Err(InferenceContractError::InvalidOutput);
        }
        let message = vote_message(pending.request_id, output_hash, certificate.output.len());
        if vote.signature.is_null() || vote.signature.verify(&message, &vote.validator).is_err() {
            return Err(InferenceContractError::VoteSignature(
                "signature did not verify for member key".to_string(),
            ));
        }
        matching.push(*member);
    }
    let matching_stake = matching.iter().try_fold(0u64, |sum, member| {
        sum.checked_add(member.stake)
            .ok_or(InferenceContractError::InvalidStakeTotal)
    })?;
    if matching_stake < arc_types_threshold(total_stake) {
        return Err(InferenceContractError::InsufficientStake);
    }
    matching.sort_by_key(|member| member.address.0);
    Ok((output_hash, matching))
}

fn arc_types_threshold(total_stake: u64) -> u64 {
    // Reuse the crate-wide overflow-safe strict >2/3 rule.
    crate::strict_supermajority_threshold(total_stake)
}

fn coalesced_credits(
    entries: impl IntoIterator<Item = (Hash256, u128)>,
) -> Result<Vec<SettlementCredit>, InferenceContractError> {
    let mut totals = BTreeMap::<[u8; 32], u128>::new();
    for (payee, amount) in entries {
        let total = totals.entry(payee.0).or_default();
        *total = total
            .checked_add(amount)
            .ok_or(InferenceContractError::ArithmeticOverflow)?;
    }
    totals
        .into_iter()
        .map(|(payee, amount)| {
            Ok(SettlementCredit {
                payee: Hash256(payee),
                amount: u64::try_from(amount)
                    .map_err(|_| InferenceContractError::ArithmeticOverflow)?,
            })
        })
        .collect()
}

pub fn plan_finalize(
    pending: &PendingInference,
    certificate: &InferenceCertificate,
    members: &[ValidatorMember],
    expected_domain: &InferenceDomain,
    now: u64,
) -> Result<SettlementPlan, InferenceContractError> {
    let (output_hash, matching) =
        validate_certificate(pending, certificate, members, expected_domain, now)?;
    let execution_price = pending.request.job.execution_price;
    if execution_price > pending.reserved_balance {
        return Err(InferenceContractError::PriceExceedsReserve);
    }
    let total_matching_stake = matching.iter().try_fold(0u64, |sum, member| {
        sum.checked_add(member.stake)
            .ok_or(InferenceContractError::InvalidStakeTotal)
    })?;
    let mut entries = Vec::with_capacity(matching.len() + 1);
    let mut paid = 0u64;
    for member in &matching {
        let numerator = u128::from(execution_price)
            .checked_mul(u128::from(member.stake))
            .ok_or(InferenceContractError::ArithmeticOverflow)?;
        let base = numerator / u128::from(total_matching_stake);
        paid = paid
            .checked_add(
                u64::try_from(base).map_err(|_| InferenceContractError::ArithmeticOverflow)?,
            )
            .ok_or(InferenceContractError::ArithmeticOverflow)?;
        entries.push((member.address, base));
    }
    let remainder = execution_price
        .checked_sub(paid)
        .ok_or(InferenceContractError::ArithmeticOverflow)? as usize;
    for (index, entry) in entries.iter_mut().enumerate() {
        if index < remainder {
            entry.1 = entry
                .1
                .checked_add(1)
                .ok_or(InferenceContractError::ArithmeticOverflow)?;
        }
    }
    let refund = pending
        .reserved_balance
        .checked_sub(execution_price)
        .ok_or(InferenceContractError::ArithmeticOverflow)?;
    let mut credits = entries;
    credits.push((pending.request.job.requester, u128::from(refund)));
    let credits = coalesced_credits(credits)?;
    let credited = credits.iter().try_fold(0u64, |sum, credit| {
        sum.checked_add(credit.amount)
            .ok_or(InferenceContractError::ArithmeticOverflow)
    })?;
    if credited != pending.reserved_balance {
        return Err(InferenceContractError::ArithmeticOverflow);
    }
    Ok(SettlementPlan::Finalize {
        request_id: pending.request_id,
        consume_pending: pending.request_id,
        charged: execution_price,
        credits,
        output_hash,
    })
}

pub fn plan_refund(
    pending: &PendingInference,
    expected_domain: &InferenceDomain,
    members: &[ValidatorMember],
    now: u64,
) -> Result<SettlementPlan, InferenceContractError> {
    if pending.request_id != pending.request.job.request_id() {
        return Err(InferenceContractError::RequestIdMismatch);
    }
    if pending.reserved_balance != pending.request.job.reserved_max_payment {
        return Err(InferenceContractError::ReserveMismatch);
    }
    validate_job_structure(&pending.request.job, expected_domain, members)?;
    pending.request.verify_signature()?;
    if now < pending.request.job.expires_at {
        return Err(InferenceContractError::Expired);
    }
    Ok(SettlementPlan::Refund {
        request_id: pending.request_id,
        consume_pending: pending.request_id,
        charged: 0,
        credits: vec![SettlementCredit {
            payee: pending.request.job.requester,
            amount: pending.reserved_balance,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::signature::KeyPair;

    fn members(keys: &[&KeyPair], stakes: &[u64]) -> Vec<ValidatorMember> {
        let mut members: Vec<_> = keys
            .iter()
            .zip(stakes)
            .map(|(key, stake)| ValidatorMember::new(key.address(), *stake))
            .collect();
        members.sort_by_key(|member| member.address.0);
        members
    }

    fn job(domain: InferenceDomain, requester: Hash256) -> InferenceJob {
        InferenceJob {
            version: INFERENCE_CONTRACT_VERSION,
            domain,
            requester,
            nonce: 0,
            model_hash: hash_bytes(b"model"),
            profile_hash: hash_bytes(b"profile"),
            input_hash: hash_bytes(b"input"),
            generation_hash: hash_bytes(b"generation"),
            assignment_hash: hash_bytes(b"assignment"),
            max_tokens: 32,
            max_output_bytes: 128,
            execution_price: 10,
            reserved_max_payment: 101,
            expires_at: 100,
        }
    }

    fn setup(
        count: usize,
    ) -> (
        Vec<KeyPair>,
        Vec<ValidatorMember>,
        InferenceDomain,
        InferenceJob,
    ) {
        let keys: Vec<_> = (0..count).map(|_| KeyPair::generate_ed25519()).collect();
        let refs: Vec<_> = keys.iter().collect();
        let committee = members(&refs, &vec![1; count]);
        let domain = InferenceDomain {
            chain_genesis: hash_bytes(b"genesis"),
            recovery_epoch: 7,
            validator_set_hash: validator_set_commitment(&committee).unwrap(),
        };
        let requester = KeyPair::generate_ed25519();
        let request_job = job(domain, requester.address());
        (keys, committee, domain, request_job)
    }

    #[test]
    fn commitment_binds_every_job_dimension_and_request_id() {
        let (_keys, _committee, domain, mut original) = setup(6);
        let first = original.commitment();
        original.profile_hash = hash_bytes(b"other-profile");
        assert_ne!(first, original.commitment());
        original.profile_hash = hash_bytes(b"profile");
        original.nonce = 1;
        assert_ne!(
            original.request_id(),
            job(domain, original.requester).request_id()
        );
    }

    #[test]
    fn signed_price_and_version_are_bound_and_limits_are_enforced() {
        let (_keys, committee, domain, base_job) = setup(6);
        let requester = KeyPair::generate_ed25519();
        let request = InferenceRequest::sign(
            InferenceJob {
                requester: requester.address(),
                ..base_job.clone()
            },
            &requester,
        )
        .unwrap();
        let mut changed_price = request.clone();
        changed_price.job.execution_price += 1;
        assert!(matches!(
            changed_price.validate(&domain, &committee, 1),
            Err(InferenceContractError::RequestSignature(_))
        ));
        let mut unknown_version = request.clone();
        unknown_version.job.version = INFERENCE_CONTRACT_VERSION + 1;
        assert_eq!(
            unknown_version.validate(&domain, &committee, 1),
            Err(InferenceContractError::UnsupportedVersion)
        );
        let mut oversized = request;
        oversized.job.max_output_bytes = crate::transaction::TIER1_OUTPUT_BLOB_MAX as u32 + 1;
        assert_eq!(
            oversized.validate(&domain, &committee, 1),
            Err(InferenceContractError::InvalidBounds)
        );
    }

    #[test]
    fn five_of_six_accepts_and_four_of_six_rejects() {
        let (keys, committee, domain, job) = setup(6);
        let requester = KeyPair::generate_ed25519();
        let request = InferenceRequest::sign(
            InferenceJob {
                requester: requester.address(),
                ..job
            },
            &requester,
        )
        .unwrap();
        let pending = request.validate(&domain, &committee, 1).unwrap();
        let output = b"output";
        let five: Vec<_> = keys
            .iter()
            .take(5)
            .map(|key| sign_vote(pending.request_id, output, key).unwrap())
            .collect();
        let certificate = InferenceCertificate {
            output: output.to_vec(),
            votes: five,
        };
        assert!(plan_finalize(&pending, &certificate, &committee, &domain, 1).is_ok());
        let four = InferenceCertificate {
            output: output.to_vec(),
            votes: certificate.votes[..4].to_vec(),
        };
        assert_eq!(
            plan_finalize(&pending, &four, &committee, &domain, 1),
            Err(InferenceContractError::InsufficientStake)
        );
    }

    #[test]
    fn weighted_stake_and_lexicographic_remainder_conserve_reserve() {
        let (keys, _committee, domain, base_job) = setup(3);
        let requester = KeyPair::generate_ed25519();
        let mut weighted = members(&[&keys[0], &keys[1], &keys[2]], &[5, 3, 1]);
        weighted.sort_by_key(|member| member.address.0);
        let domain = InferenceDomain {
            validator_set_hash: validator_set_commitment(&weighted).unwrap(),
            ..domain
        };
        let request = InferenceRequest::sign(
            InferenceJob {
                domain,
                requester: requester.address(),
                ..base_job
            },
            &requester,
        )
        .unwrap();
        let pending = request.validate(&domain, &weighted, 1).unwrap();
        let output = b"weighted";
        let votes = keys
            .iter()
            .map(|key| sign_vote(pending.request_id, output, key).unwrap())
            .collect();
        let plan = plan_finalize(
            &pending,
            &InferenceCertificate {
                output: output.to_vec(),
                votes,
            },
            &weighted,
            &domain,
            1,
        )
        .unwrap();
        let SettlementPlan::Finalize { credits, .. } = plan else {
            panic!()
        };
        assert_eq!(credits.iter().map(|credit| credit.amount).sum::<u64>(), 101);
        assert_eq!(credits.len(), 4);
    }

    #[test]
    fn forged_duplicate_nonmember_output_and_expiry_are_rejected() {
        let (keys, committee, domain, job) = setup(6);
        let requester = KeyPair::generate_ed25519();
        let request = InferenceRequest::sign(
            InferenceJob {
                requester: requester.address(),
                ..job
            },
            &requester,
        )
        .unwrap();
        let pending = request.validate(&domain, &committee, 1).unwrap();
        let output = b"output";
        let mut vote = sign_vote(pending.request_id, output, &keys[0]).unwrap();
        vote.signature = Signature::null();
        let forged = InferenceCertificate {
            output: output.to_vec(),
            votes: vec![vote],
        };
        assert!(matches!(
            plan_finalize(&pending, &forged, &committee, &domain, 1),
            Err(InferenceContractError::VoteSignature(_))
        ));

        let outsider = KeyPair::generate_ed25519();
        let nonmember = sign_vote(pending.request_id, output, &outsider).unwrap();
        let certificate = InferenceCertificate {
            output: output.to_vec(),
            votes: vec![nonmember],
        };
        assert_eq!(
            plan_finalize(&pending, &certificate, &committee, &domain, 1),
            Err(InferenceContractError::InvalidVoteMember)
        );

        let duplicate = sign_vote(pending.request_id, output, &keys[0]).unwrap();
        let duplicate_cert = InferenceCertificate {
            output: output.to_vec(),
            votes: vec![duplicate.clone(), duplicate],
        };
        assert_eq!(
            plan_finalize(&pending, &duplicate_cert, &committee, &domain, 1),
            Err(InferenceContractError::InvalidVoteMember)
        );

        let bad_output = InferenceCertificate {
            output: b"different".to_vec(),
            votes: vec![sign_vote(pending.request_id, output, &keys[0]).unwrap()],
        };
        assert_eq!(
            plan_finalize(&pending, &bad_output, &committee, &domain, 1),
            Err(InferenceContractError::InvalidOutput)
        );
        assert_eq!(
            plan_finalize(
                &pending,
                &InferenceCertificate {
                    output: Vec::new(),
                    votes: Vec::new(),
                },
                &committee,
                &domain,
                1,
            ),
            Err(InferenceContractError::InvalidOutput)
        );
        assert_eq!(
            plan_finalize(
                &pending,
                &InferenceCertificate {
                    output: vec![0; 129],
                    votes: Vec::new(),
                },
                &committee,
                &domain,
                1,
            ),
            Err(InferenceContractError::InvalidOutput)
        );
        assert_eq!(
            plan_finalize(&pending, &bad_output, &committee, &domain, 100),
            Err(InferenceContractError::Expired)
        );
        assert!(matches!(
            plan_refund(&pending, &domain, &committee, 99),
            Err(InferenceContractError::Expired)
        ));
        assert!(matches!(
            plan_refund(&pending, &domain, &committee, 100),
            Ok(SettlementPlan::Refund { .. })
        ));
    }
}
