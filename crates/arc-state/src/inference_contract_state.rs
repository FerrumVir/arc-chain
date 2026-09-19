//! Inactive persistent adapter for the pure native-inference contract.
//!
//! The adapter owns a persistent `StateDB` and exposes only serialized
//! transitions. It is intentionally not wired to transaction or RPC paths.

use crate::wal::{InferenceMetadata, InferenceTransitionRecord, InferenceTransitionStatus, WalOp};
use crate::{StateDB, StateError};
use arc_crypto::{Hash256, hash_bytes};
use arc_types::Account;
use arc_types::inference_contract::{
    InferenceCertificate, InferenceContractError, InferenceDomain, InferenceRequest,
    PendingInference, SettlementCredit, SettlementPlan, ValidatorMember, validate_members,
    validator_set_commitment,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const CONTEXT_DOMAIN: &str = "ARC-isolated-inference-context-v1";
const CONTEXT_ACCOUNT_DOMAIN: &[u8] = b"ARC-isolated-inference-context-account-v1";
const CONTEXT_KEY_DOMAIN: &[u8] = b"ARC-isolated-inference-context-key-v1";
const METADATA_KEY_DOMAIN: &[u8] = b"ARC-isolated-inference-metadata-v1";
const MAX_METADATA_BYTES: usize = 128 * 1024;
const MAX_TRANSITION_BYTES: usize = 256 * 1024;
const MAX_STORAGE_UPDATES: usize = 3;
const MAX_ALLOWLIST_ENTRIES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InferenceAdmissionContext {
    pub domain: InferenceDomain,
    pub members: Vec<ValidatorMember>,
    pub allowed_executions: Vec<AllowedExecution>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AllowedExecution {
    pub model_hash: Hash256,
    pub profile_hash: Hash256,
    pub generation_hash: Hash256,
    pub assignment_hash: Hash256,
}

impl InferenceAdmissionContext {
    pub fn commitment(&self) -> Result<Hash256, StateError> {
        validate_members(&self.members).map_err(contract_error)?;
        if self.allowed_executions.is_empty()
            || self.allowed_executions.len() > MAX_ALLOWLIST_ENTRIES
        {
            return Err(StateError::ExecutionError(
                "isolated inference execution allowlist exceeds bound".into(),
            ));
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.domain.chain_genesis.0);
        bytes.extend_from_slice(&self.domain.recovery_epoch.to_le_bytes());
        bytes.extend_from_slice(&self.domain.validator_set_hash.0);
        bytes.extend_from_slice(&(self.members.len() as u32).to_le_bytes());
        for member in &self.members {
            bytes.extend_from_slice(&member.address.0);
            bytes.extend_from_slice(&member.stake.to_le_bytes());
        }
        let mut sorted = self.allowed_executions.clone();
        sorted.sort_by_key(|execution| {
            (
                execution.model_hash.0,
                execution.profile_hash.0,
                execution.generation_hash.0,
                execution.assignment_hash.0,
            )
        });
        sorted.dedup();
        bytes.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
        for execution in sorted {
            bytes.extend_from_slice(&execution.model_hash.0);
            bytes.extend_from_slice(&execution.profile_hash.0);
            bytes.extend_from_slice(&execution.generation_hash.0);
            bytes.extend_from_slice(&execution.assignment_hash.0);
        }
        Ok(domain_hash(CONTEXT_DOMAIN, &bytes))
    }

    fn allows(&self, request: &InferenceRequest) -> bool {
        self.allowed_executions.contains(&AllowedExecution {
            model_hash: request.job.model_hash,
            profile_hash: request.job.profile_hash,
            generation_hash: request.job.generation_hash,
            assignment_hash: request.job.assignment_hash,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IsolatedTransitionResult {
    Applied {
        request_id: Hash256,
        state_root: Hash256,
        status: InferenceTransitionStatus,
    },
    AlreadyTerminal {
        request_id: Hash256,
        status: InferenceTransitionStatus,
        output_hash: Option<Hash256>,
    },
}

pub struct IsolatedInferenceLedger {
    state: StateDB,
    context: InferenceAdmissionContext,
    context_commitment: Hash256,
    poisoned: bool,
    #[cfg(test)]
    fail_before_checkpoint: bool,
}

fn domain_hash(domain: &str, bytes: &[u8]) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key(domain);
    hasher.update(bytes);
    Hash256(*hasher.finalize().as_bytes())
}

fn contract_error(error: InferenceContractError) -> StateError {
    StateError::ExecutionError(format!("isolated inference contract: {error}"))
}

fn context_account() -> arc_types::Address {
    hash_bytes(CONTEXT_ACCOUNT_DOMAIN)
}

fn context_key() -> Hash256 {
    hash_bytes(CONTEXT_KEY_DOMAIN)
}

fn escrow_address(request_id: Hash256) -> arc_types::Address {
    domain_hash("ARC-isolated-inference-escrow-v1", request_id.as_ref())
}

fn metadata_key() -> Hash256 {
    hash_bytes(METADATA_KEY_DOMAIN)
}

fn members_from_state(state: &StateDB) -> Vec<ValidatorMember> {
    let mut members: Vec<_> = state
        .active_validators()
        .into_iter()
        .map(|(address, stake)| ValidatorMember { address, stake })
        .collect();
    members.sort_by_key(|member| member.address.0);
    members
}

impl IsolatedInferenceLedger {
    pub fn new(state: StateDB, context: InferenceAdmissionContext) -> Result<Self, StateError> {
        if !state.is_persistent() {
            return Err(StateError::ExecutionError(
                "isolated inference requires a persistent WAL".into(),
            ));
        }
        if state.recovery_context().is_some() {
            return Err(StateError::ExecutionError(
                "isolated inference adapter is unavailable on recovery-bound state".into(),
            ));
        }
        let bound = state
            .persistence_dir()
            .ok_or_else(|| StateError::ExecutionError("persistent directory is missing".into()))?
            .join("genesis.network-hash");
        let actual_genesis = std::fs::read_to_string(&bound)
            .map_err(|error| StateError::PersistenceError(error.to_string()))
            .and_then(|value| {
                Hash256::from_hex(value.trim()).map_err(|error| {
                    StateError::PersistenceError(format!("invalid genesis binding: {error}"))
                })
            })?;
        if actual_genesis != context.domain.chain_genesis {
            return Err(StateError::ExecutionError(
                "isolated inference domain is not bound to the persistent genesis".into(),
            ));
        }
        let actual_members = members_from_state(&state);
        if actual_members != context.members {
            return Err(StateError::ExecutionError(
                "isolated inference context does not match active validator registry".into(),
            ));
        }
        let set_hash = validator_set_commitment(&context.members).map_err(contract_error)?;
        if set_hash != context.domain.validator_set_hash {
            return Err(StateError::ExecutionError(
                "isolated inference context validator-set hash is incorrect".into(),
            ));
        }
        let context_commitment = context.commitment()?;
        if let Some(pin) = state.get_storage(&context_account(), &context_key()) {
            if pin.as_slice() != context_commitment.as_ref() {
                return Err(StateError::ExecutionError(
                    "isolated inference pinned context mismatch on reopen".into(),
                ));
            }
            let account = state.get_account(&context_account()).ok_or_else(|| {
                StateError::ExecutionError(
                    "isolated inference context pin account is missing".into(),
                )
            })?;
            if account.storage_root != context_commitment {
                return Err(StateError::ExecutionError(
                    "isolated inference context account root is not pinned".into(),
                ));
            }
        }
        Ok(Self {
            state,
            context,
            context_commitment,
            poisoned: false,
            #[cfg(test)]
            fail_before_checkpoint: false,
        })
    }

    #[cfg(test)]
    fn fail_before_checkpoint_for_test(&mut self) {
        self.fail_before_checkpoint = true;
    }

    pub fn state_root(&self) -> Result<Hash256, StateError> {
        self.ensure_usable()?;
        Ok(self.state.get_state_root())
    }

    pub fn account(&self, address: &arc_types::Address) -> Result<Option<Account>, StateError> {
        self.ensure_usable()?;
        Ok(self.state.get_account(address))
    }

    pub fn context_commitment(&self) -> Hash256 {
        self.context_commitment
    }

    fn ensure_usable(&self) -> Result<(), StateError> {
        if self.poisoned {
            return Err(StateError::ExecutionError(
                "isolated inference adapter is poisoned; reopen the state".into(),
            ));
        }
        self.state.wal_failure().map_or(Ok(()), |error| {
            Err(StateError::PersistenceError(error.to_string()))
        })
    }

    fn ensure_context_current(&self) -> Result<(), StateError> {
        if state_members(&self.state) != self.context.members {
            return Err(StateError::ExecutionError(
                "active validator set changed after isolated context pin".into(),
            ));
        }
        if self.state.recovery_context().is_some() {
            return Err(StateError::ExecutionError(
                "isolated inference adapter became recovery-bound".into(),
            ));
        }
        Ok(())
    }

    fn trusted_time(&self) -> Result<u64, StateError> {
        let Some(account) = self.state.get_account(&context_account()) else {
            return Ok(0);
        };
        if account.storage_root != self.context_commitment {
            return Err(StateError::ExecutionError(
                "persisted inference clock is not bound to the context account".into(),
            ));
        }
        Ok(account.nonce)
    }

    fn transition(&mut self, record: InferenceTransitionRecord) -> Result<Hash256, StateError> {
        self.ensure_usable()?;
        self.validate_record(&record)?;
        let height = self.state.height();
        self.state
            .wal
            .append(WalOp::InferenceTransition(record.clone()), height);
        if let Err(error) = self.state.durable_wal_barrier() {
            self.poisoned = true;
            return Err(error);
        }

        // The typed record is durable before this publication. A crash before
        // the checkpoint leaves it as an uncommitted WAL suffix for recovery
        // to quarantine; this adapter remains poisoned on the live process.
        self.state.apply_wal_op(&WalOp::InferenceTransition(record));
        let root = self.state.get_state_root();
        #[cfg(test)]
        if self.fail_before_checkpoint {
            self.fail_before_checkpoint = false;
            self.state
                .wal
                .inject_failure(crate::wal::WalFaultPoint::Fsync);
            if let Err(error) = self.state.durable_wal_barrier() {
                self.poisoned = true;
                return Err(error);
            }
        }
        self.state.wal.append(WalOp::Checkpoint(root), height);
        if let Err(error) = self.state.durable_wal_barrier() {
            self.poisoned = true;
            return Err(error);
        }
        Ok(root)
    }

    fn validate_record(&self, record: &InferenceTransitionRecord) -> Result<(), StateError> {
        if record.context_commitment != self.context_commitment
            || record.metadata.context_commitment != self.context_commitment
            || record.request_id != record.metadata.request.job.request_id()
            || record.metadata.request.job.domain != self.context.domain
            || record.members != self.context.members
            || record.account_updates.len() > 40
            || record.storage_updates.len() > MAX_STORAGE_UPDATES
            || record.escrow != escrow_address(record.request_id)
        {
            return Err(StateError::ExecutionError(
                "isolated inference transition metadata mismatch or exceeds bound".into(),
            ));
        }
        let encoded_record = bincode::serialize(record)
            .map_err(|error| StateError::ExecutionError(error.to_string()))?;
        if encoded_record.len() > MAX_TRANSITION_BYTES {
            return Err(StateError::ExecutionError(
                "isolated inference transition exceeds WAL bound".into(),
            ));
        }
        let encoded = bincode::serialize(&record.metadata)
            .map_err(|error| StateError::ExecutionError(error.to_string()))?;
        if encoded.len() > MAX_METADATA_BYTES
            || record.metadata.output.len() > arc_types::transaction::TIER1_OUTPUT_BLOB_MAX
        {
            return Err(StateError::ExecutionError(
                "isolated inference transition metadata/output exceeds bound".into(),
            ));
        }
        match record.metadata.status {
            InferenceTransitionStatus::Pending => {
                if !record.metadata.output.is_empty()
                    || record.metadata.output_hash.is_some()
                    || record.metadata.certificate.is_some()
                    || !record.metadata.credits.is_empty()
                {
                    return Err(StateError::ExecutionError(
                        "pending transition contains terminal settlement data".into(),
                    ));
                }
            }
            InferenceTransitionStatus::Finalized => {
                if record.metadata.output.is_empty()
                    || record.metadata.output_hash != Some(hash_bytes(&record.metadata.output))
                    || record.metadata.certificate.is_none()
                    || record
                        .metadata
                        .certificate
                        .as_ref()
                        .is_some_and(|certificate| certificate.output != record.metadata.output)
                    || record.metadata.credits.is_empty()
                {
                    return Err(StateError::ExecutionError(
                        "finalized transition has invalid receipt data".into(),
                    ));
                }
            }
            InferenceTransitionStatus::Refunded => {
                if !record.metadata.output.is_empty()
                    || record.metadata.output_hash.is_some()
                    || record.metadata.certificate.is_some()
                    || record.metadata.credits.is_empty()
                {
                    return Err(StateError::ExecutionError(
                        "refund transition has invalid receipt data".into(),
                    ));
                }
            }
        }
        if record.metadata.input_blob.len() > arc_types::transaction::TIER1_INPUT_BLOB_MAX
            || hash_bytes(&record.metadata.input_blob) != record.metadata.request.job.input_hash
        {
            return Err(StateError::ExecutionError(
                "inference input blob is oversized or does not match its signed hash".into(),
            ));
        }
        if !record.storage_updates.iter().any(|(address, key, value)| {
            *address == record.escrow && *key == metadata_key() && *value == encoded
        }) {
            return Err(StateError::ExecutionError(
                "inference transition does not persist its exact escrow receipt".into(),
            ));
        }
        let mut accounts = BTreeSet::new();
        for (address, account) in &record.account_updates {
            if !accounts.insert(address.0) || account.address != *address {
                return Err(StateError::ExecutionError(
                    "isolated inference transition has duplicate account replacement".into(),
                ));
            }
        }
        let mut storage = BTreeSet::new();
        for (address, key, value) in &record.storage_updates {
            if !storage.insert((address.0, key.0)) || value.len() > MAX_METADATA_BYTES {
                return Err(StateError::ExecutionError(
                    "isolated inference transition has duplicate or oversized storage".into(),
                ));
            }
        }
        let escrow = record
            .account_updates
            .iter()
            .find(|(address, _)| *address == record.escrow)
            .map(|(_, account)| account)
            .ok_or_else(|| StateError::ExecutionError("escrow replacement is missing".into()))?;
        if escrow.storage_root != hash_bytes(&encoded) {
            return Err(StateError::ExecutionError(
                "escrow storage root does not commit transition metadata".into(),
            ));
        }
        Ok(())
    }

    pub fn admit(
        &mut self,
        request: InferenceRequest,
        input_blob: &[u8],
        now: u64,
    ) -> Result<IsolatedTransitionResult, StateError> {
        self.ensure_usable()?;
        self.ensure_context_current()?;
        let state_height = self.state.height();
        if now < state_height {
            return Err(StateError::ExecutionError(
                "inference admission time is behind the state height".into(),
            ));
        }
        let trusted_time = self.trusted_time()?;
        if now < trusted_time {
            return Err(StateError::ExecutionError(
                "inference admission time moved backwards".into(),
            ));
        }
        if input_blob.len() > arc_types::transaction::TIER1_INPUT_BLOB_MAX
            || hash_bytes(input_blob) != request.job.input_hash
        {
            return Err(StateError::ExecutionError(
                "inference input blob is oversized or does not match its signed hash".into(),
            ));
        }
        if !self.context.allows(&request) {
            return Err(StateError::ExecutionError(
                "model/profile/generation/assignment is not in the pinned allowlist".into(),
            ));
        }
        let pending = request
            .validate(&self.context.domain, &self.context.members, now)
            .map_err(contract_error)?;
        let requester = pending.job().requester;
        let mut sender = self
            .state
            .get_account(&requester)
            .ok_or(StateError::AccountNotFound(requester))?;
        if sender.nonce != pending.job().nonce {
            return Err(StateError::InvalidNonce {
                expected: sender.nonce,
                got: pending.job().nonce,
            });
        }
        if sender.balance < pending.job().reserved_max_payment {
            return Err(StateError::InsufficientBalance {
                have: sender.balance,
                need: pending.job().reserved_max_payment,
            });
        }
        sender.balance -= pending.job().reserved_max_payment;
        sender.nonce = sender
            .nonce
            .checked_add(1)
            .ok_or_else(|| StateError::ExecutionError("request nonce overflow".into()))?;
        let request_id = pending.request_id();
        let escrow = escrow_address(request_id);
        if self.state.get_storage(&escrow, &metadata_key()).is_some()
            || self
                .state
                .get_account(&escrow)
                .is_some_and(|account| account.balance != 0 || account.code_hash != Hash256::ZERO)
        {
            return Err(StateError::ExecutionError(
                "request id already has an escrow or terminal record".into(),
            ));
        }
        let metadata = InferenceMetadata {
            context_commitment: self.context_commitment,
            request: request.clone(),
            input_blob: input_blob.to_vec(),
            status: InferenceTransitionStatus::Pending,
            output: Vec::new(),
            output_hash: None,
            certificate: None,
            credits: Vec::new(),
        };
        let encoded = bincode::serialize(&metadata)
            .map_err(|error| StateError::ExecutionError(error.to_string()))?;
        let mut escrow_account = Account::new(escrow, pending.job().reserved_max_payment);
        escrow_account.nonce = now;
        escrow_account.storage_root = hash_bytes(&encoded);
        let mut account_updates = vec![(requester, sender), (escrow, escrow_account)];
        let mut storage_updates = vec![(escrow, metadata_key(), encoded)];
        let context_account_address = context_account();
        let mut pin_account = self
            .state
            .get_account(&context_account_address)
            .unwrap_or_else(|| Account::new(context_account_address, 0));
        if pin_account.balance != 0 || pin_account.code_hash != Hash256::ZERO {
            return Err(StateError::ExecutionError(
                "context pin account is unexpectedly occupied".into(),
            ));
        }
        if self
            .state
            .get_storage(&context_account_address, &context_key())
            .is_none()
        {
            if pin_account.storage_root != Hash256::ZERO {
                return Err(StateError::ExecutionError(
                    "context pin account has an unrecognized storage root".into(),
                ));
            }
            pin_account.storage_root = self.context_commitment;
            storage_updates.push((
                context_account_address,
                context_key(),
                self.context_commitment.0.to_vec(),
            ));
        } else if pin_account.storage_root != self.context_commitment {
            return Err(StateError::ExecutionError(
                "context pin account root does not match pinned context".into(),
            ));
        }
        pin_account.nonce = now;
        account_updates.push((context_account_address, pin_account));
        account_updates.sort_by_key(|(address, _)| address.0);
        let record = InferenceTransitionRecord {
            context_commitment: self.context_commitment,
            request_id,
            escrow,
            admission_height: now,
            members: self.context.members.clone(),
            metadata,
            account_updates,
            storage_updates,
        };
        let root = self.transition(record)?;
        Ok(IsolatedTransitionResult::Applied {
            request_id,
            state_root: root,
            status: InferenceTransitionStatus::Pending,
        })
    }

    pub fn finalize(
        &mut self,
        request_id: Hash256,
        certificate: &InferenceCertificate,
        now: u64,
    ) -> Result<IsolatedTransitionResult, StateError> {
        self.finish(request_id, certificate, now)
    }

    pub fn refund(
        &mut self,
        request_id: Hash256,
        now: u64,
    ) -> Result<IsolatedTransitionResult, StateError> {
        self.ensure_usable()?;
        self.ensure_context_current()?;
        let (metadata, admission_height, escrow) = self.load_metadata(request_id)?;
        match metadata.status {
            InferenceTransitionStatus::Refunded => {
                return Ok(IsolatedTransitionResult::AlreadyTerminal {
                    request_id,
                    status: metadata.status,
                    output_hash: metadata.output_hash,
                });
            }
            InferenceTransitionStatus::Finalized => {
                return Err(StateError::ExecutionError(
                    "request already finalized; refund is not a matching terminal operation".into(),
                ));
            }
            InferenceTransitionStatus::Pending => {}
        }
        if now < admission_height || now < self.trusted_time()? {
            return Err(StateError::ExecutionError(
                "inference terminal transition moved backwards in trusted time".into(),
            ));
        }
        let pending = self.reconstruct_pending(&metadata, admission_height)?;
        let plan = arc_types::inference_contract::plan_refund(
            &pending,
            &self.context.domain,
            &self.context.members,
            now,
        )
        .map_err(contract_error)?;
        let credits = plan_credits(&plan)?;
        let terminal = InferenceMetadata {
            context_commitment: self.context_commitment,
            request: metadata.request,
            input_blob: metadata.input_blob,
            status: InferenceTransitionStatus::Refunded,
            output: Vec::new(),
            output_hash: None,
            certificate: None,
            credits,
        };
        let root = self.apply_terminal(terminal, request_id, admission_height, escrow, now)?;
        Ok(IsolatedTransitionResult::Applied {
            request_id,
            state_root: root,
            status: InferenceTransitionStatus::Refunded,
        })
    }

    fn finish(
        &mut self,
        request_id: Hash256,
        certificate: &InferenceCertificate,
        now: u64,
    ) -> Result<IsolatedTransitionResult, StateError> {
        self.ensure_usable()?;
        self.ensure_context_current()?;
        let (metadata, admission_height, escrow) = self.load_metadata(request_id)?;
        match metadata.status {
            InferenceTransitionStatus::Finalized => {
                if metadata.certificate.as_ref() == Some(certificate) {
                    return Ok(IsolatedTransitionResult::AlreadyTerminal {
                        request_id,
                        status: metadata.status,
                        output_hash: metadata.output_hash,
                    });
                }
                return Err(StateError::ExecutionError(
                    "finalize certificate does not match stored terminal output".into(),
                ));
            }
            InferenceTransitionStatus::Refunded => {
                return Err(StateError::ExecutionError(
                    "request already refunded; finalize is not a matching terminal operation"
                        .into(),
                ));
            }
            InferenceTransitionStatus::Pending => {}
        }
        if now < admission_height || now < self.trusted_time()? {
            return Err(StateError::ExecutionError(
                "inference terminal transition moved backwards in trusted time".into(),
            ));
        }
        let pending = self.reconstruct_pending(&metadata, admission_height)?;
        let plan = arc_types::inference_contract::plan_finalize(
            &pending,
            certificate,
            &self.context.members,
            &self.context.domain,
            now,
        )
        .map_err(contract_error)?;
        let credits = plan_credits(&plan)?;
        let output_hash = match &plan {
            SettlementPlan::Finalize { output_hash, .. } => *output_hash,
            SettlementPlan::Refund { .. } => unreachable!(),
        };
        let terminal = InferenceMetadata {
            context_commitment: self.context_commitment,
            request: metadata.request,
            input_blob: metadata.input_blob,
            status: InferenceTransitionStatus::Finalized,
            output: certificate.output.clone(),
            output_hash: Some(output_hash),
            certificate: Some(certificate.clone()),
            credits,
        };
        let root = self.apply_terminal(terminal, request_id, admission_height, escrow, now)?;
        Ok(IsolatedTransitionResult::Applied {
            request_id,
            state_root: root,
            status: InferenceTransitionStatus::Finalized,
        })
    }

    fn load_metadata(
        &self,
        request_id: Hash256,
    ) -> Result<(InferenceMetadata, u64, arc_types::Address), StateError> {
        let escrow = escrow_address(request_id);
        let bytes = self
            .state
            .get_storage(&escrow, &metadata_key())
            .ok_or_else(|| {
                StateError::ExecutionError("pending inference record not found".into())
            })?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(StateError::ExecutionError(
                "pending metadata exceeds bound".into(),
            ));
        }
        let metadata: InferenceMetadata = bincode::deserialize_limited_exact::<
            InferenceMetadata,
            MAX_METADATA_BYTES,
        >(&bytes)
        .map_err(|error| StateError::ExecutionError(format!("decode pending metadata: {error}")))?;
        if metadata.request.job.request_id() != request_id {
            return Err(StateError::ExecutionError(
                "pending metadata request mismatch".into(),
            ));
        }
        if metadata.context_commitment != self.context_commitment {
            return Err(StateError::ExecutionError(
                "pending metadata context commitment mismatch".into(),
            ));
        }
        if metadata.input_blob.len() > arc_types::transaction::TIER1_INPUT_BLOB_MAX
            || hash_bytes(&metadata.input_blob) != metadata.request.job.input_hash
        {
            return Err(StateError::ExecutionError(
                "persisted inference input blob does not match its signed hash".into(),
            ));
        }
        let escrow_account = self
            .state
            .get_account(&escrow)
            .ok_or_else(|| StateError::ExecutionError("pending escrow account missing".into()))?;
        if escrow_account.storage_root != hash_bytes(&bytes) {
            return Err(StateError::ExecutionError(
                "pending escrow commitment mismatch".into(),
            ));
        }
        Ok((metadata, escrow_account.nonce, escrow))
    }

    fn reconstruct_pending(
        &self,
        metadata: &InferenceMetadata,
        admission_height: u64,
    ) -> Result<PendingInference, StateError> {
        if metadata.request.job.domain != self.context.domain
            || !self.context.allows(&metadata.request)
        {
            return Err(StateError::ExecutionError(
                "persisted request no longer matches pinned context".into(),
            ));
        }
        metadata
            .request
            .validate(
                &self.context.domain,
                &self.context.members,
                admission_height,
            )
            .map_err(contract_error)
    }

    fn apply_terminal(
        &mut self,
        metadata: InferenceMetadata,
        request_id: Hash256,
        admission_height: u64,
        escrow: arc_types::Address,
        now: u64,
    ) -> Result<Hash256, StateError> {
        let encoded = bincode::serialize(&metadata)
            .map_err(|error| StateError::ExecutionError(error.to_string()))?;
        if encoded.len() > MAX_METADATA_BYTES {
            return Err(StateError::ExecutionError(
                "terminal metadata exceeds bound".into(),
            ));
        }
        let mut accounts = BTreeMap::<[u8; 32], Account>::new();
        let mut escrow_account = self
            .state
            .get_account(&escrow)
            .ok_or_else(|| StateError::ExecutionError("terminal escrow account missing".into()))?;
        if escrow_account.balance != metadata.request.job.reserved_max_payment {
            return Err(StateError::ExecutionError("escrow reserve mismatch".into()));
        }
        escrow_account.balance = 0;
        escrow_account.storage_root = hash_bytes(&encoded);
        accounts.insert(escrow.0, escrow_account);
        let total_credits = metadata.credits.iter().try_fold(0u64, |sum, credit| {
            sum.checked_add(credit.amount)
                .ok_or_else(|| StateError::ExecutionError("credit sum overflow".into()))
        })?;
        if metadata.status == InferenceTransitionStatus::Finalized {
            let expected = metadata
                .request
                .job
                .reserved_max_payment
                .checked_sub(metadata.request.job.execution_price)
                .and_then(|refund| refund.checked_add(metadata.request.job.execution_price))
                .ok_or_else(|| {
                    StateError::ExecutionError("settlement conservation overflow".into())
                })?;
            if total_credits != expected {
                return Err(StateError::ExecutionError(
                    "final settlement is not conserving reserve".into(),
                ));
            }
        } else if total_credits != metadata.request.job.reserved_max_payment {
            return Err(StateError::ExecutionError(
                "refund is not conserving reserve".into(),
            ));
        }
        for credit in &metadata.credits {
            if credit.payee == escrow {
                return Err(StateError::ExecutionError("invalid terminal credit".into()));
            }
            if credit.amount == 0 {
                continue;
            }
            if !accounts.contains_key(&credit.payee.0) {
                let account = self
                    .state
                    .get_account(&credit.payee)
                    .unwrap_or_else(|| Account::new(credit.payee, 0));
                accounts.insert(credit.payee.0, account);
            }
            let account = accounts.get_mut(&credit.payee.0).expect("inserted above");
            account.balance = account
                .balance
                .checked_add(credit.amount)
                .ok_or_else(|| StateError::ExecutionError("credit balance overflow".into()))?;
        }
        let mut account_updates = accounts
            .into_iter()
            .map(|(_, account)| (account.address, account))
            .collect::<Vec<_>>();
        let context_address = context_account();
        let mut context_state = self
            .state
            .get_account(&context_address)
            .ok_or_else(|| StateError::ExecutionError("context clock account is missing".into()))?;
        if context_state.balance != 0
            || context_state.code_hash != Hash256::ZERO
            || context_state.storage_root != self.context_commitment
            || self
                .state
                .get_storage(&context_address, &context_key())
                .is_none()
        {
            return Err(StateError::ExecutionError(
                "context clock account is not pinned".into(),
            ));
        }
        context_state.nonce = now;
        account_updates.push((context_address, context_state));
        let record = InferenceTransitionRecord {
            context_commitment: self.context_commitment,
            request_id,
            escrow,
            admission_height,
            members: self.context.members.clone(),
            metadata,
            account_updates,
            storage_updates: vec![(escrow, metadata_key(), encoded)],
        };
        self.transition(record)
    }
}

fn state_members(state: &StateDB) -> Vec<ValidatorMember> {
    members_from_state(state)
}

fn plan_credits(plan: &SettlementPlan) -> Result<Vec<SettlementCredit>, StateError> {
    match plan {
        SettlementPlan::Finalize { credits, .. } | SettlementPlan::Refund { credits, .. } => {
            Ok(credits.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::signature::KeyPair;

    struct Fixture {
        dir: std::path::PathBuf,
        genesis: Hash256,
        requester: KeyPair,
        validators: Vec<KeyPair>,
        prefunded: Vec<(arc_types::Address, u64)>,
        context: InferenceAdmissionContext,
        ledger: IsolatedInferenceLedger,
    }

    fn fixture(name: &str) -> Fixture {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("arc-inference-{name}-{unique}"));
        let genesis = hash_bytes(format!("arc-inference-genesis-{name}-{unique}").as_bytes());
        let requester = KeyPair::generate_ed25519();
        let validators: Vec<_> = (0..6).map(|_| KeyPair::generate_ed25519()).collect();
        let mut validator_members: Vec<_> = validators
            .iter()
            .map(|key| ValidatorMember::new(key.address(), StateDB::MIN_VALIDATOR_STAKE))
            .collect();
        validator_members.sort_by_key(|member| member.address.0);
        let domain = InferenceDomain {
            chain_genesis: genesis,
            recovery_epoch: 1,
            validator_set_hash: validator_set_commitment(&validator_members).unwrap(),
        };
        let marker = hash_bytes(b"fixture-marker");
        let alternate = hash_bytes(b"fixture-alternate");
        let context = InferenceAdmissionContext {
            domain,
            members: validator_members.clone(),
            allowed_executions: vec![
                AllowedExecution {
                    model_hash: marker,
                    profile_hash: marker,
                    generation_hash: marker,
                    assignment_hash: marker,
                },
                AllowedExecution {
                    model_hash: alternate,
                    profile_hash: alternate,
                    generation_hash: alternate,
                    assignment_hash: alternate,
                },
            ],
        };
        let mut prefunded = vec![(requester.address(), 1_000)];
        prefunded.extend(validator_members.iter().map(|member| (member.address, 0)));
        let state = StateDB::with_genesis_persistent(&prefunded, &dir, genesis).unwrap();
        state.seed_genesis_validators(
            &validator_members
                .iter()
                .map(|member| (member.address, member.stake))
                .collect::<Vec<_>>(),
        );
        let ledger = IsolatedInferenceLedger::new(state, context.clone()).unwrap();
        Fixture {
            dir,
            genesis,
            requester,
            validators,
            prefunded,
            context,
            ledger,
        }
    }

    fn request(fixture: &Fixture, nonce: u64, expires_at: u64) -> InferenceRequest {
        let marker = fixture.context.allowed_executions[0].model_hash;
        let input = b"input";
        InferenceRequest::sign(
            arc_types::inference_contract::InferenceJob {
                version: arc_types::inference_contract::INFERENCE_CONTRACT_VERSION,
                domain: fixture.context.domain,
                requester: fixture.requester.address(),
                nonce,
                model_hash: marker,
                profile_hash: marker,
                input_hash: hash_bytes(input),
                generation_hash: marker,
                assignment_hash: marker,
                max_tokens: 32,
                max_output_bytes: 128,
                execution_price: 10,
                reserved_max_payment: 100,
                expires_at,
            },
            &fixture.requester,
        )
        .unwrap()
    }

    fn certificate(fixture: &Fixture, request_id: Hash256) -> InferenceCertificate {
        let output = b"deterministic output".to_vec();
        let mut votes: Vec<_> = fixture
            .validators
            .iter()
            .take(5)
            .map(|key| arc_types::inference_contract::sign_vote(request_id, &output, key).unwrap())
            .collect();
        votes.sort_by_key(|vote| vote.validator.0);
        InferenceCertificate { output, votes }
    }

    #[test]
    fn admission_finalize_and_restart_preserve_one_economic_state() {
        let mut fixture = fixture("finalize-restart");
        let request = request(&fixture, 0, 100);
        let request_id = request.job.request_id();
        let admitted = fixture.ledger.admit(request.clone(), b"input", 1).unwrap();
        assert!(matches!(
            admitted,
            IsolatedTransitionResult::Applied {
                status: InferenceTransitionStatus::Pending,
                ..
            }
        ));
        assert_eq!(
            fixture
                .ledger
                .account(&request.job.requester)
                .unwrap()
                .unwrap()
                .balance,
            900
        );
        let before = fixture.ledger.state_root().unwrap();
        let cert = certificate(&fixture, request_id);
        let finalized = fixture.ledger.finalize(request_id, &cert, 2).unwrap();
        assert!(matches!(
            finalized,
            IsolatedTransitionResult::Applied {
                status: InferenceTransitionStatus::Finalized,
                ..
            }
        ));
        let after = fixture.ledger.state_root().unwrap();
        assert_ne!(before, after);
        assert_eq!(
            fixture
                .ledger
                .account(&request.job.requester)
                .unwrap()
                .unwrap()
                .balance,
            990
        );
        fixture.ledger.state.try_sync_wal().unwrap();
        let duplicate_cert = cert.clone();
        drop(fixture.ledger);

        let state =
            StateDB::with_genesis_persistent(&fixture.prefunded, &fixture.dir, fixture.genesis)
                .unwrap();
        let members = fixture.context.members.clone();
        state.seed_genesis_validators(
            &members
                .iter()
                .map(|member| (member.address, member.stake))
                .collect::<Vec<_>>(),
        );
        let mut restarted = IsolatedInferenceLedger::new(state, fixture.context.clone()).unwrap();
        assert_eq!(restarted.state_root().unwrap(), after);
        assert_eq!(
            restarted
                .account(&request.job.requester)
                .unwrap()
                .unwrap()
                .balance,
            990
        );
        assert!(matches!(
            restarted.finalize(request_id, &duplicate_cert, 3).unwrap(),
            IsolatedTransitionResult::AlreadyTerminal {
                status: InferenceTransitionStatus::Finalized,
                ..
            }
        ));
        drop(restarted);
        std::fs::remove_dir_all(fixture.dir).unwrap();
    }

    #[test]
    fn refund_and_rejections_do_not_mutate_state() {
        let mut fixture = fixture("refund-rejections");
        let request = request(&fixture, 0, 2);
        let request_id = request.job.request_id();
        let before_admission = fixture.ledger.state_root().unwrap();
        assert!(
            fixture
                .ledger
                .admit(request.clone(), b"wrong-input", 1)
                .is_err()
        );
        assert_eq!(fixture.ledger.state_root().unwrap(), before_admission);
        assert_eq!(
            fixture
                .ledger
                .account(&request.job.requester)
                .unwrap()
                .unwrap()
                .balance,
            1_000
        );
        assert!(
            fixture
                .ledger
                .admit(
                    request.clone(),
                    &vec![0u8; arc_types::transaction::TIER1_INPUT_BLOB_MAX + 1],
                    1,
                )
                .is_err()
        );
        let admitted = fixture.ledger.admit(request.clone(), b"input", 1).unwrap();
        let pending_root = fixture.ledger.state_root().unwrap();
        assert!(matches!(
            fixture.ledger.refund(request_id, 1),
            Err(StateError::ExecutionError(message)) if message.contains("expired")
        ));
        assert_eq!(fixture.ledger.state_root().unwrap(), pending_root);
        assert!(matches!(
            fixture.ledger.refund(request_id, 2).unwrap(),
            IsolatedTransitionResult::Applied {
                status: InferenceTransitionStatus::Refunded,
                ..
            }
        ));
        assert_eq!(
            fixture
                .ledger
                .account(&request.job.requester)
                .unwrap()
                .unwrap()
                .balance,
            1_000
        );
        assert!(matches!(
            fixture.ledger.refund(request_id, 3).unwrap(),
            IsolatedTransitionResult::AlreadyTerminal {
                status: InferenceTransitionStatus::Refunded,
                ..
            }
        ));

        let bad_model = {
            let mut job = request.job.clone();
            job.nonce = 1;
            job.model_hash = hash_bytes(b"unregistered-model");
            InferenceRequest::sign(job, &fixture.requester).unwrap()
        };
        let root = fixture.ledger.state_root().unwrap();
        assert!(fixture.ledger.admit(bad_model, b"input", 4).is_err());
        assert_eq!(fixture.ledger.state_root().unwrap(), root);
        let crossed = {
            let mut job = request.job.clone();
            job.nonce = 1;
            job.model_hash = fixture.context.allowed_executions[1].model_hash;
            InferenceRequest::sign(job, &fixture.requester).unwrap()
        };
        assert!(fixture.ledger.admit(crossed, b"input", 4).is_err());
        assert_eq!(fixture.ledger.state_root().unwrap(), root);
        drop(admitted);
        drop(fixture.ledger);
        std::fs::remove_dir_all(fixture.dir).unwrap();
    }

    #[test]
    fn wal_failure_poison_prevents_unacknowledged_terminal_credit() {
        let mut fixture = fixture("wal-failure");
        let request = request(&fixture, 0, 100);
        let request_id = request.job.request_id();
        fixture.ledger.admit(request.clone(), b"input", 1).unwrap();
        let pending_balance = fixture
            .ledger
            .account(&request.job.requester)
            .unwrap()
            .unwrap()
            .balance;
        fixture
            .ledger
            .state
            .wal
            .inject_failure(crate::wal::WalFaultPoint::Fsync);
        let cert = certificate(&fixture, request_id);
        assert!(fixture.ledger.finalize(request_id, &cert, 2).is_err());
        assert!(fixture.ledger.state_root().is_err());
        assert_eq!(
            fixture.ledger.account(&request.job.requester).is_err(),
            true
        );
        drop(fixture.ledger);
        let state =
            StateDB::with_genesis_persistent(&fixture.prefunded, &fixture.dir, fixture.genesis)
                .unwrap();
        state.seed_genesis_validators(
            &fixture
                .context
                .members
                .iter()
                .map(|member| (member.address, member.stake))
                .collect::<Vec<_>>(),
        );
        let restarted = IsolatedInferenceLedger::new(state, fixture.context.clone()).unwrap();
        assert_eq!(
            restarted
                .account(&request.job.requester)
                .unwrap()
                .unwrap()
                .balance,
            pending_balance
        );
        drop(restarted);
        std::fs::remove_dir_all(fixture.dir).unwrap();
    }

    #[test]
    fn checkpoint_failure_rolls_back_published_terminal_on_restart() {
        let mut fixture = fixture("checkpoint-failure");
        let request = request(&fixture, 0, 100);
        let request_id = request.job.request_id();
        fixture.ledger.admit(request.clone(), b"input", 1).unwrap();
        let pending_balance = fixture
            .ledger
            .account(&request.job.requester)
            .unwrap()
            .unwrap()
            .balance;
        fixture.ledger.fail_before_checkpoint_for_test();
        let cert = certificate(&fixture, request_id);
        assert!(fixture.ledger.finalize(request_id, &cert, 2).is_err());
        assert!(fixture.ledger.state_root().is_err());
        drop(fixture.ledger);
        let state =
            StateDB::with_genesis_persistent(&fixture.prefunded, &fixture.dir, fixture.genesis)
                .unwrap();
        state.seed_genesis_validators(
            &fixture
                .context
                .members
                .iter()
                .map(|member| (member.address, member.stake))
                .collect::<Vec<_>>(),
        );
        let restarted = IsolatedInferenceLedger::new(state, fixture.context.clone()).unwrap();
        assert_eq!(
            restarted
                .account(&request.job.requester)
                .unwrap()
                .unwrap()
                .balance,
            pending_balance
        );
        assert_eq!(
            restarted
                .account(&escrow_address(request_id))
                .unwrap()
                .unwrap()
                .balance,
            request.job.reserved_max_payment
        );
        for member in &fixture.context.members {
            assert_eq!(
                restarted.account(&member.address).unwrap().unwrap().balance,
                0
            );
        }
        assert_eq!(
            restarted.load_metadata(request_id).unwrap().0.status,
            InferenceTransitionStatus::Pending
        );
        drop(restarted);
        std::fs::remove_dir_all(fixture.dir).unwrap();
    }

    #[test]
    fn terminal_clock_rejects_backdating_and_zero_share_settlement() {
        let mut f1 = fixture("terminal-clock-zero-share");
        let mut req1 = request(&f1, 0, 100);
        req1.job.execution_price = req1.job.reserved_max_payment;
        req1 = InferenceRequest::sign(req1.job, &f1.requester).unwrap();
        let request_id = req1.job.request_id();
        f1.ledger.admit(req1.clone(), b"input", 5).unwrap();
        let cert = certificate(&f1, request_id);
        let root = f1.ledger.state_root().unwrap();
        assert!(f1.ledger.finalize(request_id, &cert, 4).is_err());
        assert_eq!(f1.ledger.state_root().unwrap(), root);
        f1.ledger.finalize(request_id, &cert, 5).unwrap();
        assert_eq!(
            f1.ledger
                .account(&req1.job.requester)
                .unwrap()
                .unwrap()
                .balance,
            900
        );
        drop(f1.ledger);
        std::fs::remove_dir_all(f1.dir).unwrap();

        let mut tiny = fixture("terminal-clock-tiny-share");
        let mut tiny_request = request(&tiny, 0, 100);
        tiny_request.job.execution_price = 1;
        tiny_request = InferenceRequest::sign(tiny_request.job, &tiny.requester).unwrap();
        let tiny_id = tiny_request.job.request_id();
        tiny.ledger
            .admit(tiny_request.clone(), b"input", 1)
            .unwrap();
        let tiny_cert = certificate(&tiny, tiny_id);
        let missing_payee = tiny_cert.votes[0].validator;
        tiny.ledger.state.accounts.remove(&missing_payee.0);
        tiny.ledger.finalize(tiny_id, &tiny_cert, 2).unwrap();
        assert!(
            tiny.ledger
                .account(&missing_payee)
                .unwrap()
                .unwrap()
                .balance
                > 0
        );
        drop(tiny.ledger);
        std::fs::remove_dir_all(tiny.dir).unwrap();

        let mut maxed = fixture("maximum-bounded-input-output");
        let input = vec![7u8; arc_types::transaction::TIER1_INPUT_BLOB_MAX];
        let mut max_request = request(&maxed, 0, 100);
        max_request.job.input_hash = hash_bytes(&input);
        max_request.job.max_output_bytes = arc_types::transaction::TIER1_OUTPUT_BLOB_MAX as u32;
        max_request = InferenceRequest::sign(max_request.job, &maxed.requester).unwrap();
        let max_id = max_request.job.request_id();
        maxed.ledger.admit(max_request.clone(), &input, 1).unwrap();
        let output = vec![9u8; arc_types::transaction::TIER1_OUTPUT_BLOB_MAX];
        let votes = maxed
            .validators
            .iter()
            .take(5)
            .map(|key| arc_types::inference_contract::sign_vote(max_id, &output, key).unwrap())
            .collect();
        maxed
            .ledger
            .finalize(max_id, &InferenceCertificate { output, votes }, 2)
            .unwrap();
        drop(maxed.ledger);
        std::fs::remove_dir_all(maxed.dir).unwrap();
    }

    #[test]
    fn persisted_clock_rejects_backdating_across_requests_and_restart() {
        let mut fixture = fixture("persisted-clock-cross-request");
        let request_a = request(&fixture, 0, 100);
        let request_b = request(&fixture, 1, 200);
        let request_a_id = request_a.job.request_id();
        let request_b_id = request_b.job.request_id();
        fixture
            .ledger
            .admit(request_a.clone(), b"input", 1)
            .unwrap();
        fixture
            .ledger
            .admit(request_b.clone(), b"input", 10)
            .unwrap();
        let cert_a = certificate(&fixture, request_a_id);
        let cert_b = certificate(&fixture, request_b_id);
        let root_before = fixture.ledger.state_root().unwrap();
        assert!(fixture.ledger.finalize(request_a_id, &cert_a, 9).is_err());
        assert_eq!(fixture.ledger.state_root().unwrap(), root_before);
        fixture.ledger.state.try_sync_wal().unwrap();
        drop(fixture.ledger);

        let state =
            StateDB::with_genesis_persistent(&fixture.prefunded, &fixture.dir, fixture.genesis)
                .unwrap();
        state.seed_genesis_validators(
            &fixture
                .context
                .members
                .iter()
                .map(|member| (member.address, member.stake))
                .collect::<Vec<_>>(),
        );
        let mut reopened = IsolatedInferenceLedger::new(state, fixture.context.clone()).unwrap();
        assert_eq!(reopened.trusted_time().unwrap(), 10);
        let reopened_root = reopened.state_root().unwrap();
        assert!(reopened.finalize(request_a_id, &cert_a, 9).is_err());
        assert_eq!(reopened.state_root().unwrap(), reopened_root);
        reopened.refund(request_a_id, 100).unwrap();
        let root_after_refund = reopened.state_root().unwrap();
        assert!(reopened.finalize(request_b_id, &cert_b, 99).is_err());
        assert_eq!(reopened.state_root().unwrap(), root_after_refund);
        drop(reopened);
        std::fs::remove_dir_all(fixture.dir).unwrap();
    }
}
