//! Shared native-inference planner and persistent isolated/canonical adapters.

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
    /// How commit-time selection counts candidates (decision D20). Fixed at
    /// activation and part of the commitment.
    pub selection_rule: NativeSelectionRule,
}

/// How commit-time selection treats a candidate whose sender has already
/// used its nonce (decision D20). A chain's rule is fixed at activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NativeSelectionRule {
    /// Chains activated before D20: every examined candidate counts toward
    /// the per-block cap. Recorded in the original activation layout, so the
    /// record and its commitment are byte-identical to what older binaries
    /// wrote and read.
    #[serde(rename = "count-every-candidate-v1")]
    CountEveryCandidateV1,
    /// D20: a candidate whose sender has already used its nonce is skipped
    /// without counting. Binaries without D20 cannot decode this record and
    /// refuse to open the chain, so a network never mixes the two rules.
    #[serde(rename = "skip-used-nonces-v2")]
    SkipUsedNoncesV2,
}

impl NativeSelectionRule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CountEveryCandidateV1 => "count-every-candidate-v1",
            Self::SkipUsedNoncesV2 => "skip-used-nonces-v2",
        }
    }
}

/// The activation record layout from before D20. Selection-rule v1 chains
/// keep exactly these bytes.
#[derive(Serialize, Deserialize)]
struct LegacyAdmissionContext {
    domain: InferenceDomain,
    members: Vec<ValidatorMember>,
    allowed_executions: Vec<AllowedExecution>,
}

/// Encode an activation record: the original layout for rule v1, the
/// extended one (with the rule) for v2. One encoding per context.
pub(crate) fn encode_activation(
    context: &InferenceAdmissionContext,
) -> Result<Vec<u8>, StateError> {
    let encoded = match context.selection_rule {
        NativeSelectionRule::CountEveryCandidateV1 => bincode::serialize(&LegacyAdmissionContext {
            domain: context.domain,
            members: context.members.clone(),
            allowed_executions: context.allowed_executions.clone(),
        }),
        NativeSelectionRule::SkipUsedNoncesV2 => bincode::serialize(context),
    };
    encoded.map_err(|error| StateError::ExecutionError(error.to_string()))
}

/// Decode an activation record written by [`encode_activation`] or by a
/// binary from before D20. Both layouts decode exactly (no trailing bytes),
/// so neither can be mistaken for the other.
pub(crate) fn decode_activation(encoded: &[u8]) -> Result<InferenceAdmissionContext, StateError> {
    if let Ok(context) =
        bincode::deserialize_limited_exact::<InferenceAdmissionContext, MAX_CONTEXT_BYTES>(encoded)
    {
        if context.selection_rule == NativeSelectionRule::CountEveryCandidateV1 {
            return Err(StateError::ExecutionError(
                "invalid native activation: a selection-rule v1 context is recorded only in the \
                 original layout"
                    .into(),
            ));
        }
        return Ok(context);
    }
    let legacy =
        bincode::deserialize_limited_exact::<LegacyAdmissionContext, MAX_CONTEXT_BYTES>(encoded)
            .map_err(|e| StateError::ExecutionError(format!("invalid native activation: {e}")))?;
    Ok(InferenceAdmissionContext {
        domain: legacy.domain,
        members: legacy.members,
        allowed_executions: legacy.allowed_executions,
        selection_rule: NativeSelectionRule::CountEveryCandidateV1,
    })
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
        // Rule v1 keeps the original commitment. The v2 marker cannot be
        // mistaken for more allowlist entries: they come in 128-byte units.
        if self.selection_rule == NativeSelectionRule::SkipUsedNoncesV2 {
            bytes.extend_from_slice(b"selection-rule-v2");
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

/// How many native candidates one committed block's selection examines, in
/// hash order. See `select_native_block_transactions`.
pub const MAX_NATIVE_CANDIDATES_PER_BLOCK: usize = 64;

/// How far ahead of its sender's account nonce a native transaction may be
/// and still be kept for a later block.
pub const NATIVE_FUTURE_NONCE_WINDOW: u64 = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeInferencePendingSnapshot {
    pub request_id: Hash256,
    pub request: InferenceRequest,
    pub input_blob: Vec<u8>,
    pub admission_height: u64,
    pub context_commitment: Hash256,
}

/// Request-keyed canonical receipt view for worker/RPC consumers.
#[derive(Clone, Debug)]
pub struct NativeInferenceReceiptSnapshot {
    pub request_id: Hash256,
    pub admission_height: u64,
    pub metadata: InferenceMetadata,
    pub admission_transaction: Option<NativeInferenceTransactionLink>,
    pub terminal_transaction: Option<NativeInferenceTransactionLink>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeInferenceTransactionLink {
    pub tx_hash: Hash256,
    pub block_height: u64,
    pub block_hash: Hash256,
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

/// Whether `value` at (`address`, `key`) is committed by `account`, the
/// certified account at that address (C11 checkpoint adoption).
///
/// Native inference keeps its state in two kinds of row, both bound to a
/// field of the account the state root covers: an escrow's metadata row
/// hashes to that escrow account's `storage_root`, and the context pin row
/// equals the pin account's `storage_root`. No other row is committed by the
/// account-only root.
pub(crate) fn native_storage_row_committed(
    account: Option<&arc_types::Account>,
    address: &arc_types::Address,
    key: &Hash256,
    value: &[u8],
) -> bool {
    let Some(account) = account else {
        return false;
    };
    if *key == metadata_key() {
        return value.len() <= MAX_METADATA_BYTES && account.storage_root == hash_bytes(value);
    }
    *address == context_account() && *key == context_key() && value == account.storage_root.as_ref()
}

/// Whether (`address`, `key`) is a row a certified account commits, rather
/// than one that must simply be unchanged: a native escrow's metadata row, or
/// the context pin row. Such a row is adopted from a checkpoint only if its
/// certified account commits it; being equal to this node's own copy is not
/// enough, because this node's copy may be the stale one.
pub(crate) fn native_storage_row_is_committable(
    address: &arc_types::Address,
    key: &Hash256,
) -> bool {
    *key == metadata_key() || (*address == context_account() && *key == context_key())
}

pub(crate) fn escrow_address(request_id: Hash256) -> arc_types::Address {
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

/// Compare a genesis validator list with the committee a native binding froze.
///
/// Returns `Ok` when they are the same committee (the genesis list counts only
/// entries at or above the minimum validator stake, exactly as the live
/// registry does). Otherwise returns a message naming every difference - added,
/// removed, and changed-stake members - and the remedy.
///
/// This exists because the validator registry is re-seeded from the genesis
/// file on every ordinary start. A restart with a genesis whose committee
/// differs from the one bound at activation used to surface only later, as an
/// opaque activation failure, and the node could not start (D2). Nothing in the
/// persistent state is wrong in that case - the re-seed is in memory - so the
/// correct remedy is to restore the genesis used at activation, and the check
/// runs BEFORE the re-seed so it is refused without touching anything.
pub fn genesis_committee_matches_binding(
    genesis_validators: &[(Hash256, u64)],
    bound: &[ValidatorMember],
) -> Result<(), String> {
    let mut genesis: Vec<ValidatorMember> = genesis_validators
        .iter()
        .filter(|(_, stake)| *stake >= StateDB::MIN_VALIDATOR_STAKE)
        .map(|(address, stake)| ValidatorMember {
            address: *address,
            stake: *stake,
        })
        .collect();
    genesis.sort_by_key(|member| member.address.0);
    if genesis.as_slice() == bound {
        return Ok(());
    }
    let bound_map: std::collections::BTreeMap<[u8; 32], u64> =
        bound.iter().map(|m| (m.address.0, m.stake)).collect();
    let genesis_map: std::collections::BTreeMap<[u8; 32], u64> =
        genesis.iter().map(|m| (m.address.0, m.stake)).collect();
    let mut differences = Vec::new();
    for (address, stake) in &genesis_map {
        match bound_map.get(address) {
            None => differences.push(format!(
                "added {} (stake {stake})",
                Hash256(*address).to_hex()
            )),
            Some(bound_stake) if bound_stake != stake => differences.push(format!(
                "{} stake {bound_stake} -> {stake}",
                Hash256(*address).to_hex()
            )),
            Some(_) => {}
        }
    }
    for (address, stake) in &bound_map {
        if !genesis_map.contains_key(address) {
            differences.push(format!(
                "removed {} (stake {stake})",
                Hash256(*address).to_hex()
            ));
        }
    }
    Err(format!(
        "the genesis validator set differs from the committee named by this chain's native \
         inference binding: {}. The persistent state is unchanged. A committee change IS \
         supported - publish the new binding with a versioned update, which keeps work already \
         admitted settling under the binding that accepted it - but the genesis committee must \
         be updated to match in the same operation, or a restart re-seeds the old committee. \
         This refusal is what stops the two drifting apart. Restart with a genesis file that \
         matches the current binding.",
        differences.join("; ")
    ))
}

/// Validate the private native-inference activation boundary without mutating
/// state. Recovery-bound state and non-persistent state are never eligible.
pub fn validate_native_inference_activation(
    state: &StateDB,
    context: &InferenceAdmissionContext,
) -> Result<Hash256, StateError> {
    let commitment = validate_native_inference_binding_shape(state, context)?;
    if let Some(pin) = state.get_storage(&context_account(), &context_key()) {
        if pin.as_slice() != commitment.as_ref() {
            return Err(StateError::ExecutionError(
                "native inference pinned context mismatch on reopen".into(),
            ));
        }
        let account = state.get_account(&context_account()).ok_or_else(|| {
            StateError::ExecutionError("native inference context pin account is missing".into())
        })?;
        if account.storage_root != commitment {
            return Err(StateError::ExecutionError(
                "native inference context account root is not pinned".into(),
            ));
        }
    }
    Ok(commitment)
}

/// Everything `validate_native_inference_activation` checks EXCEPT that the
/// context is the one the account is already pinned to. A binding update
/// replaces that pin, so it validates the incoming binding against the chain
/// and the live registry without requiring it to match what is pinned now.
pub fn validate_native_inference_binding_shape(
    state: &StateDB,
    context: &InferenceAdmissionContext,
) -> Result<Hash256, StateError> {
    if !state.is_persistent() {
        return Err(StateError::ExecutionError(
            "native inference requires a persistent WAL".into(),
        ));
    }
    // Recovery-bound state is the recovered public chain. Activating there is
    // exactly what a migration record authorises, and without one the original
    // rule stands.
    if state.recovery_context().is_some() && state.native_migration().is_none() {
        return Err(StateError::ExecutionError(
            "native inference is unavailable on recovery-bound state without an operator \
             migration record"
                .into(),
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
            "native inference domain is not bound to the persistent genesis".into(),
        ));
    }
    if members_from_state(state) != context.members {
        return Err(StateError::ExecutionError(
            "native inference context does not match active validator registry".into(),
        ));
    }
    if validator_set_commitment(&context.members).map_err(contract_error)?
        != context.domain.validator_set_hash
    {
        return Err(StateError::ExecutionError(
            "native inference validator-set hash is incorrect".into(),
        ));
    }
    context.commitment()
}

// Receipt reads validate the escrow's rooted bytes; binding eligibility is
// checked separately only for pending work, since settled old bindings remain history.
fn read_native_metadata(
    state: &StateDB,
    request_id: Hash256,
) -> Result<Option<NativeInferenceReceiptSnapshot>, StateError> {
    let escrow = escrow_address(request_id);
    let Some(bytes) = state.get_storage(&escrow, &metadata_key()) else {
        return Ok(None);
    };
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(StateError::ExecutionError(
            "native inference metadata exceeds bound".into(),
        ));
    }
    let metadata =
        bincode::deserialize_limited_exact::<InferenceMetadata, MAX_METADATA_BYTES>(&bytes)
            .map_err(|error| {
                StateError::ExecutionError(format!("decode native metadata: {error}"))
            })?;
    let Some(account) = state.get_account(&escrow) else {
        return Err(StateError::ExecutionError(
            "native inference escrow account is missing".into(),
        ));
    };
    // Reading a receipt does NOT require the binding it names to still be one
    // this chain holds. Work settled under a binding that has since been
    // replaced is history, and its receipt has to stay readable for as long
    // as the chain keeps it - a configuration change must not make past
    // settlements disappear from the app, the explorer or an RPC client.
    //
    // What proves the receipt is the chain's own is the escrow account's
    // rooted `storage_root`, which commits these exact bytes. The binding a
    // request must name to be WORK rather than history is checked where that
    // matters, in `native_inference_pending_requests`.
    if metadata.request.job.request_id() != request_id
        || account.storage_root != hash_bytes(&bytes)
        || metadata.input_blob.len() > arc_types::transaction::TIER1_INPUT_BLOB_MAX
        || hash_bytes(&metadata.input_blob) != metadata.request.job.input_hash
    {
        return Err(StateError::ExecutionError(
            "native inference metadata commitment or input binding failed".into(),
        ));
    }
    let (admission_transaction, terminal_transaction) = native_transaction_links(state, request_id);
    Ok(Some(NativeInferenceReceiptSnapshot {
        request_id,
        admission_height: account.nonce,
        metadata,
        admission_transaction,
        terminal_transaction,
    }))
}

fn native_transaction_links(
    state: &StateDB,
    request_id: Hash256,
) -> (
    Option<NativeInferenceTransactionLink>,
    Option<NativeInferenceTransactionLink>,
) {
    let mut admission = None;
    let mut terminal = None;
    // Every canonical request escrow has at most one successful request and
    // terminal transaction. Use its bounded history, never a full tx scan.
    for hash in state.get_account_txs(&escrow_address(request_id).0) {
        let Some(tx) = state.full_transactions.get(&hash.0).map(|v| v.clone()) else {
            continue;
        };
        let is_request = match &tx.body {
            arc_types::TxBody::NativeInferenceRequest(body)
                if body.request.job.request_id() == request_id =>
            {
                true
            }
            arc_types::TxBody::NativeInferenceFinalize(body) if body.request_id == request_id.0 => {
                false
            }
            arc_types::TxBody::NativeInferenceRefund(body) if body.request_id == request_id.0 => {
                false
            }
            _ => continue,
        };
        let Some((height, index)) = state.get_tx_location(&hash.0) else {
            continue;
        };
        let Some(block) = state.get_block(height) else {
            continue;
        };
        let Some(receipt) = state.get_receipt(&hash.0) else {
            continue;
        };
        if !receipt.success
            || receipt.tx_hash != hash
            || receipt.block_height != height
            || receipt.index != index
            || receipt.block_hash != block.hash
            || block.tx_hashes.get(index as usize) != Some(&hash)
        {
            continue;
        }
        let link = NativeInferenceTransactionLink {
            tx_hash: hash,
            block_height: height,
            block_hash: block.hash,
        };
        if is_request {
            admission = Some(link);
        } else {
            terminal = Some(link);
        }
    }
    (admission, terminal)
}

const MAX_NATIVE_PENDING: usize = 1024;
const MAX_CONTEXT_BYTES: usize = 32 * 1024;

pub(crate) fn activation_key() -> Hash256 {
    hash_bytes(b"ARC-native-inference-activation-v1")
}

/// Whether a binding write seals its own WAL checkpoint.
///
/// `Standalone` is a write of its own - a fresh-genesis activation, or one
/// driven straight from operator config - and it checkpoints and barriers
/// itself. `InBlock` is part of a block's transition: the block appends the
/// checkpoint and takes the barrier, and a second standalone checkpoint at
/// the same height is exactly what WAL replay validation refuses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WalSeal {
    Standalone,
    InBlock,
}

/// How many superseded bindings may be retained at once. A binding is
/// retired as soon as no pending request still names it, so this bounds only
/// how many updates can be in flight over unsettled work at the same time.
pub(crate) const MAX_RETAINED_BINDINGS: usize = 8;

/// The index of retained superseded bindings: a list of their commitments.
pub(crate) fn versions_key() -> Hash256 {
    hash_bytes(b"ARC-native-inference-versions-v1")
}

/// Where one superseded binding is kept, by its own commitment. A request
/// admitted under a binding keeps naming it through settlement or refund, so
/// the binding has to outlive being replaced.
pub(crate) fn version_key(commitment: Hash256) -> Hash256 {
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(b"ARC-native-inference-version-v1");
    bytes.extend_from_slice(commitment.as_ref());
    hash_bytes(&bytes)
}

/// Where the authorising migration record is kept once it has been applied.
/// The record is an operator input to the ONE activation, but the fact that
/// activation happened under it has to outlive the process: a restarting node
/// is not handed the record again, and every validator's root must contain
/// the same evidence of why a running chain was allowed to activate.
pub(crate) fn migration_key() -> Hash256 {
    hash_bytes(b"ARC-native-inference-migration-v1")
}

/// Authorisation to activate native inference on a chain that is ALREADY
/// RUNNING, instead of only at a fresh genesis.
///
/// The fresh-genesis rule exists because activation writes state: every
/// validator must make the identical write at the identical point, or they
/// derive different roots. At genesis that is trivial. On a live chain it has
/// to be arranged, and this record is the arrangement - it names the chain,
/// the recovery epoch it was produced for, the exact height at which every
/// validator applies it, and the binding being frozen. A node that disagrees
/// about any of those refuses rather than guessing.
///
/// What it deliberately does NOT do is relax a check. Every binding
/// `validate_native_inference_activation` already enforces - persistent WAL,
/// genesis, the live validator registry, the validator-set commitment, an
/// existing pin - still applies. This only replaces "height must be zero"
/// with "height must be the one every validator agreed on".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMigrationRecord {
    /// Must equal the persistent `genesis.network-hash`, so a record written
    /// for one chain cannot activate another.
    pub chain_genesis: Hash256,
    /// The recovery epoch and validator-set id this record was produced for.
    /// A chain that has since been recovered again has a different epoch and
    /// must not accept a record aimed at the previous one.
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    /// The coordinated point. Every validator applies the activation when its
    /// height is exactly this, so all of them write the same state at the
    /// same place.
    pub activation_height: u64,
    /// The binding being frozen, restated here so the record is self-
    /// describing and a mismatched context is caught before anything is
    /// written.
    pub context_commitment: Hash256,
}

impl NativeMigrationRecord {
    /// Check the record against this chain before it is allowed to authorise
    /// anything. Returns the reason it does not apply, or `None`.
    pub fn refusal_against(&self, state: &StateDB) -> Option<String> {
        let Some(recovery) = state.recovery_context() else {
            return Some(
                "a migration record authorises activation on a recovery-bound chain, and this \
                 state carries no recovery context"
                    .into(),
            );
        };
        if recovery.genesis_hash != self.chain_genesis {
            return Some(format!(
                "migration record names genesis {} but this chain's is {}",
                self.chain_genesis, recovery.genesis_hash
            ));
        }
        if recovery.recovery_epoch != self.recovery_epoch
            || recovery.validator_set_id != self.validator_set_id
        {
            return Some(format!(
                "migration record was produced for recovery epoch {} / validator set {}, and this \
                 chain is at epoch {} / validator set {}",
                self.recovery_epoch,
                self.validator_set_id,
                recovery.recovery_epoch,
                recovery.validator_set_id
            ));
        }
        None
    }
}

impl StateDB {
    /// The operator's authorisation to activate on a running chain, if one
    /// was loaded and accepted for this chain.
    pub fn native_migration(&self) -> Option<NativeMigrationRecord> {
        self.native_migration.read().clone()
    }

    /// Accept a migration record for THIS chain. Rejected records leave no
    /// trace, so a node that was handed the wrong one behaves exactly as a
    /// node that was handed none: fresh genesis only.
    ///
    /// A record may be loaded either before its height (the normal case, the
    /// operator stages it ahead of the coordinated point) or after the
    /// activation it authorised has already been applied (every restart from
    /// then on). What it may never do is authorise a point this chain has
    /// already passed without having activated - that would be a retroactive
    /// state change, and every other validator would have a different root.
    /// Apply a binding change due at the height THIS BLOCK is sealing, as
    /// part of the block's own transition.
    ///
    /// This is the difference between a change every validator makes and one
    /// only the validators that happened to execute that block make. Called
    /// before the block's state root is computed, the activation or update
    /// lands *inside* the root this block commits - so a node that applies
    /// this block and does not make the same write computes a different root
    /// and rejects it, loudly, instead of carrying on with state nobody else
    /// has. A node whose height moved past the coordinated block by some other
    /// route inherits the change with the state it took.
    ///
    /// The bond sweep beside it works the same way and for the same reason:
    /// what a block is supposed to contain has to happen before its root is
    /// taken, not after.
    ///
    /// Takes the serial native execution lock. Block paths that already hold
    /// it call `apply_due_binding_change_locked` instead - it is not
    /// reentrant.
    pub(crate) fn apply_due_binding_change_in_block(&self) -> Result<(), StateError> {
        let _guard = self.native_inference_execution.lock();
        self.apply_due_binding_change_locked()
    }

    /// The body of the above, for a caller already holding that lock.
    pub(crate) fn apply_due_binding_change_locked(&self) -> Result<(), StateError> {
        let height = self.height();

        // A migration authorised for exactly this height.
        if let Some(record) = self.native_migration()
            && self.native_inference_context().is_none()
            && record.activation_height == height
        {
            let Some(context) = self.pending_migration_context.read().clone() else {
                return Err(StateError::ExecutionError(format!(
                    "a migration is authorised for height {height}, which this block reaches, \
                     but the binding it freezes was never supplied to this node"
                )));
            };
            self.activate_native_inference_locked(context, WalSeal::InBlock)?;
            return Ok(());
        }

        // Or a binding update coordinated for exactly this height.
        let due = {
            let pending = self.pending_binding_update.read();
            match pending.as_ref() {
                Some((at, _, _)) if *at == height => pending.clone(),
                _ => None,
            }
        };
        let Some((_, replacing, context)) = due else {
            return Ok(());
        };
        let current = self
            .native_inference_context()
            .ok_or_else(|| {
                StateError::ExecutionError(
                    "a binding update is due in this block but no binding is active".into(),
                )
            })?
            .commitment()?;
        if current == context.commitment()? {
            *self.pending_binding_update.write() = None;
            return Ok(());
        }
        if current != replacing {
            return Err(StateError::ExecutionError(format!(
                "a binding update due in this block replaces {}, but the binding in force is {}",
                replacing.to_hex(),
                current.to_hex()
            )));
        }
        self.update_native_inference_binding_locked(context, WalSeal::InBlock)?;
        *self.pending_binding_update.write() = None;
        Ok(())
    }

    /// Apply a migration whose coordinated height the chain has just reached.
    /// Called once per block from the single block funnel, so every validator
    /// makes the same write at the same point whether it produced that block
    /// or applied someone else's. A failure here is fatal on purpose: a node
    /// that cannot perform the coordinated activation must stop rather than
    /// continue on a chain it no longer agrees with. The block is already
    /// durable, so a restart replays and retries at the same height.
    pub(crate) fn apply_due_native_migration(&self) -> Result<(), StateError> {
        let Some(record) = self.native_migration() else {
            return Ok(());
        };
        if self.native_inference_context().is_some() {
            return Ok(());
        }
        // Past the coordinated height with nothing activated. A node can
        // reach here by advancing without executing the block at that height
        // - importing history, adopting a window - and if it simply carried
        // on it would be quietly deriving different state from every
        // validator that did activate. Silence is the dangerous outcome, so
        // this stops instead: the node has to be resynchronised from a peer
        // that holds the activated state.
        if self.height() > record.activation_height {
            return Err(StateError::ExecutionError(format!(
                "this chain passed the coordinated native activation height {} without \
                 activating (now at {}); it cannot agree with the validators that did, and must \
                 be resynchronised rather than restarted",
                record.activation_height,
                self.height()
            )));
        }
        if self.height() != record.activation_height {
            return Ok(());
        }
        let Some(context) = self.pending_migration_context.read().clone() else {
            return Err(StateError::ExecutionError(format!(
                "a migration is authorised for height {}, which this chain has now reached, but \
                 the binding it freezes was never supplied to this node",
                record.activation_height
            )));
        };
        self.activate_native_inference(context)?;
        Ok(())
    }

    pub fn authorize_native_migration(
        &self,
        record: NativeMigrationRecord,
        context: InferenceAdmissionContext,
    ) -> Result<(), StateError> {
        if let Some(reason) = record.refusal_against(self) {
            return Err(StateError::ExecutionError(reason));
        }
        let already_applied = self
            .get_storage(&context_account(), &context_key())
            .is_some();
        if !already_applied && record.activation_height <= self.height() {
            return Err(StateError::ExecutionError(format!(
                "migration record activates at height {}, which this chain passed without \
                 activating (now at {}); a retroactive activation would diverge from every \
                 other validator",
                record.activation_height,
                self.height()
            )));
        }
        *self.native_migration.write() = Some(record);
        *self.pending_migration_context.write() = Some(context);
        Ok(())
    }

    /// Explicitly enable the private protocol-4 candidate at fresh genesis.
    /// This durable, rooted context is never inferred from a submitted job.
    pub fn activate_native_inference(
        &self,
        context: InferenceAdmissionContext,
    ) -> Result<Hash256, StateError> {
        let _guard = self.native_inference_execution.lock();
        self.activate_native_inference_locked(context, WalSeal::Standalone)
    }

    /// The body of the above, for callers that already hold the serial native
    /// execution lock - block execution does, and that lock is not reentrant.
    fn activate_native_inference_locked(
        &self,
        context: InferenceAdmissionContext,
        seal: WalSeal,
    ) -> Result<Hash256, StateError> {
        self.require_healthy_wal()?;
        let commitment = validate_native_inference_activation(self, &context)?;
        if self.use_jmt {
            return Err(StateError::ExecutionError("private native inference activation requires the canonical default account-root backend".into()));
        }
        if let Some(existing) = self.native_inference_context() {
            if existing != context {
                return Err(StateError::ExecutionError(
                    "native inference activation cannot change".into(),
                ));
            }
            return Ok(commitment);
        }
        // Where activation is allowed to happen. At a fresh genesis the answer
        // is height 0 and nothing else. With an operator migration record it
        // is the single coordinated height every validator agreed on -
        // exactly, never "at or after", so all of them make the same write in
        // the same block and derive the same root.
        let migration = self.native_migration();
        if let Some(record) = &migration {
            if let Some(reason) = record.refusal_against(self) {
                return Err(StateError::ExecutionError(reason));
            }
            if record.context_commitment != commitment {
                return Err(StateError::ExecutionError(
                    "migration record authorises a different inference binding than the one \
                     being activated"
                        .into(),
                ));
            }
        }
        let at_height = migration.as_ref().map_or(0, |r| r.activation_height);
        if self.height() != at_height
            || self
                .get_storage(&context_account(), &context_key())
                .is_some()
        {
            return Err(StateError::ExecutionError(match &migration {
                None => "native inference activation requires unused private genesis".to_string(),
                Some(record) => format!(
                    "native inference migration activates at height {}, and this node is at {}",
                    record.activation_height,
                    self.height()
                ),
            }));
        }
        let encoded = encode_activation(&context)?;
        if encoded.len() > MAX_CONTEXT_BYTES {
            return Err(StateError::ExecutionError(
                "native context exceeds bound".into(),
            ));
        }
        let address = context_account();
        if self.get_account(&address).is_some() {
            return Err(StateError::ExecutionError(
                "native context account is occupied".into(),
            ));
        }
        let mut account = Account::new(address, 0);
        account.storage_root = commitment;
        let mut ops = vec![
            WalOp::SetAccount(address, account),
            WalOp::SetStorage(address, context_key(), commitment.0.to_vec()),
            WalOp::SetStorage(address, activation_key(), encoded),
            WalOp::SetValidatorState(
                context
                    .members
                    .iter()
                    .map(|m| (m.address, m.stake))
                    .collect(),
                self.staking_pool.load(std::sync::atomic::Ordering::Acquire),
            ),
        ];
        // A migrated chain records what authorised it, in the same rooted
        // write and the same block. Without this the binding cannot be
        // rebuilt after a restart, and nothing on-chain says why a running
        // chain was allowed to activate at all.
        if let Some(record) = &migration {
            let encoded_record = bincode::serialize(record)
                .map_err(|error| StateError::ExecutionError(error.to_string()))?;
            if encoded_record.len() > MAX_CONTEXT_BYTES {
                return Err(StateError::ExecutionError(
                    "native migration record exceeds bound".into(),
                ));
            }
            ops.push(WalOp::SetStorage(address, migration_key(), encoded_record));
        }
        let root = self.projected_root_after(&ops);
        for op in &ops {
            self.wal.append(op.clone(), at_height);
        }
        if seal == WalSeal::Standalone {
            self.wal.append(WalOp::Checkpoint(root), at_height);
            self.durable_wal_barrier()?;
        }
        {
            let _publication = self.native_inference_publication.write();
            for op in &ops {
                self.apply_wal_op(op);
            }
            *self.native_inference_context.write() = Some(context);
            debug_assert_eq!(self.compute_state_root(), root);
        }
        Ok(commitment)
    }

    /// Commitments still named by work that has not settled or refunded.
    fn bindings_named_by_pending_work(&self) -> std::collections::BTreeSet<[u8; 32]> {
        let ids: Vec<[u8; 32]> = self
            .native_inference_pending
            .iter()
            .map(|entry| *entry.key())
            .collect();
        let mut named = std::collections::BTreeSet::new();
        for id in ids {
            let Some(bytes) = self.get_storage(&escrow_address(Hash256(id)), &metadata_key())
            else {
                continue;
            };
            let Ok(metadata) =
                bincode::deserialize_limited_exact::<InferenceMetadata, MAX_METADATA_BYTES>(&bytes)
            else {
                continue;
            };
            if metadata.status == InferenceTransitionStatus::Pending {
                named.insert(metadata.context_commitment.0);
            }
        }
        named
    }

    /// The commitments of the superseded bindings this chain still holds.
    pub(crate) fn retained_binding_index(&self) -> Result<Vec<Hash256>, StateError> {
        let Some(bytes) = self.get_storage(&context_account(), &versions_key()) else {
            return Ok(Vec::new());
        };
        bincode::deserialize_limited_exact::<Vec<Hash256>, 1024>(&bytes).map_err(|error| {
            StateError::ExecutionError(format!("decode retained binding index: {error}"))
        })
    }

    /// Authorise a binding update for a coordinated height. Every validator
    /// is given the identical instruction, and each applies it in the block
    /// that reaches that height - the same rule activation follows, and for
    /// the same reason: publishing a binding writes state.
    pub fn authorize_binding_update(
        &self,
        at_height: u64,
        replacing: Hash256,
        context: InferenceAdmissionContext,
    ) -> Result<(), StateError> {
        let Some(current) = self.native_inference_context() else {
            return Err(StateError::ExecutionError(
                "there is no active inference binding to update; activate one first".into(),
            ));
        };
        let current_commitment = current.commitment()?;
        if replacing != current_commitment {
            return Err(StateError::ExecutionError(format!(
                "this update replaces binding {}, but the binding in force is {}",
                replacing.to_hex(),
                current_commitment.to_hex()
            )));
        }
        if context.domain.chain_genesis != current.domain.chain_genesis {
            return Err(StateError::ExecutionError(
                "a binding update cannot move the chain the binding is bound to".into(),
            ));
        }
        if context.selection_rule != current.selection_rule {
            return Err(StateError::ExecutionError(
                "the commit-time selection rule is fixed for the chain's life; a binding update \
                 cannot change it"
                    .into(),
            ));
        }
        if at_height <= self.height() {
            return Err(StateError::ExecutionError(format!(
                "a binding update is coordinated at height {at_height}, which this chain has \
                 already passed (now at {}); publishing it here would diverge from every other \
                 validator",
                self.height()
            )));
        }
        *self.pending_binding_update.write() = Some((at_height, replacing, context));
        Ok(())
    }

    /// Apply an authorised binding update whose coordinated height the chain
    /// has just reached. Fatal on failure, for the same reason the migration
    /// is: a node that cannot publish the binding every other validator just
    /// published must stop rather than carry on out of step.
    pub fn apply_due_binding_update(&self) -> Result<(), StateError> {
        let (due, passed) = {
            let pending = self.pending_binding_update.read();
            match pending.as_ref() {
                Some((at, _, _)) if *at == self.height() => (pending.clone(), false),
                Some((at, _, _)) if *at < self.height() => (pending.clone(), true),
                _ => (None, false),
            }
        };
        let Some((at, replacing, context)) = due else {
            return Ok(());
        };
        if passed {
            // Already published, if the binding in force is the new one -
            // this node applied it and simply has the instruction still in
            // hand. Otherwise it advanced past the coordinated height without
            // publishing, which is the same silent divergence the migration
            // guards against, and it stops here for the same reason.
            let current = self
                .native_inference_context()
                .ok_or_else(|| {
                    StateError::ExecutionError("a binding update is due but none is active".into())
                })?
                .commitment()?;
            if current == context.commitment()? {
                *self.pending_binding_update.write() = None;
                return Ok(());
            }
            return Err(StateError::ExecutionError(format!(
                "this chain passed the coordinated binding-update height {at} without publishing \
                 it (now at {}); it cannot agree with the validators that did, and must be \
                 resynchronised rather than restarted",
                self.height()
            )));
        }
        let current = self
            .native_inference_context()
            .ok_or_else(|| {
                StateError::ExecutionError(
                    "a binding update is due but no binding is active".into(),
                )
            })?
            .commitment()?;
        if current == context.commitment()? {
            // Already published, by a replay of this same block.
            *self.pending_binding_update.write() = None;
            return Ok(());
        }
        if current != replacing {
            return Err(StateError::ExecutionError(format!(
                "a binding update due at this height replaces {}, but the binding in force is {}",
                replacing.to_hex(),
                current.to_hex()
            )));
        }
        self.update_native_inference_binding(context)?;
        *self.pending_binding_update.write() = None;
        Ok(())
    }

    /// Publish a NEW binding over a running one: a changed committee, an added
    /// allowed execution, a model upgrade. The new binding governs requests
    /// admitted from here on.
    ///
    /// Work already admitted is NOT re-bound. Each request keeps naming the
    /// binding it was admitted under, and that binding is retained here until
    /// nothing names it, so everything in flight settles or refunds on the
    /// exact terms it was accepted on. That is what makes a committee or
    /// model change safe to do on a live chain instead of a reason to freeze
    /// one forever.
    pub fn update_native_inference_binding(
        &self,
        context: InferenceAdmissionContext,
    ) -> Result<Hash256, StateError> {
        let _guard = self.native_inference_execution.lock();
        self.update_native_inference_binding_locked(context, WalSeal::Standalone)
    }

    /// As above, for callers already holding the serial native execution lock.
    fn update_native_inference_binding_locked(
        &self,
        context: InferenceAdmissionContext,
        seal: WalSeal,
    ) -> Result<Hash256, StateError> {
        self.require_healthy_wal()?;
        let Some(previous) = self.native_inference_context() else {
            return Err(StateError::ExecutionError(
                "there is no active inference binding to update; activate one first".into(),
            ));
        };
        let previous_commitment = previous.commitment()?;
        if previous == context {
            return Ok(previous_commitment);
        }
        if self.use_jmt {
            return Err(StateError::ExecutionError(
                "a native inference binding update requires the canonical default account-root \
                 backend"
                    .into(),
            ));
        }
        let commitment = validate_native_inference_binding_shape(self, &context)?;
        if context.domain.chain_genesis != previous.domain.chain_genesis {
            return Err(StateError::ExecutionError(
                "a binding update cannot move the chain the binding is bound to".into(),
            ));
        }
        if context.selection_rule != previous.selection_rule {
            return Err(StateError::ExecutionError(
                "the commit-time selection rule is fixed for the chain's life; a binding update \
                 cannot change it"
                    .into(),
            ));
        }

        // Keep only the superseded bindings that pending work still names,
        // and add the one being replaced if anything names it.
        let named = self.bindings_named_by_pending_work();
        let mut keep: Vec<Hash256> = Vec::new();
        let mut discard: Vec<Hash256> = Vec::new();
        for held in self.retained_binding_index()? {
            if named.contains(&held.0) {
                keep.push(held);
            } else {
                discard.push(held);
            }
        }
        let retire_previous = named.contains(&previous_commitment.0);
        if retire_previous && !keep.contains(&previous_commitment) {
            keep.push(previous_commitment);
        }
        keep.sort_by_key(|entry| entry.0);
        if keep.len() > MAX_RETAINED_BINDINGS {
            return Err(StateError::ExecutionError(format!(
                "{} superseded bindings would still be named by pending work, over the limit of \
                 {MAX_RETAINED_BINDINGS}; let that work settle or expire before updating again",
                keep.len()
            )));
        }

        let encoded = encode_activation(&context)?;
        if encoded.len() > MAX_CONTEXT_BYTES {
            return Err(StateError::ExecutionError(
                "native context exceeds bound".into(),
            ));
        }
        let address = context_account();
        let mut account = self.get_account(&address).ok_or_else(|| {
            StateError::ExecutionError("native inference context account is missing".into())
        })?;
        account.storage_root = commitment;
        let mut ops = vec![
            WalOp::SetAccount(address, account),
            WalOp::SetStorage(address, context_key(), commitment.0.to_vec()),
            WalOp::SetStorage(address, activation_key(), encoded),
            WalOp::SetValidatorState(
                context
                    .members
                    .iter()
                    .map(|member| (member.address, member.stake))
                    .collect(),
                self.staking_pool.load(std::sync::atomic::Ordering::Acquire),
            ),
            WalOp::SetStorage(
                address,
                versions_key(),
                bincode::serialize(&keep)
                    .map_err(|error| StateError::ExecutionError(error.to_string()))?,
            ),
        ];
        if retire_previous {
            ops.push(WalOp::SetStorage(
                address,
                version_key(previous_commitment),
                encode_activation(&previous)?,
            ));
        }
        for gone in &discard {
            ops.push(WalOp::DeleteStorage(address, version_key(*gone)));
        }

        let height = self.height();
        let root = self.projected_root_after(&ops);
        for op in &ops {
            self.wal.append(op.clone(), height);
        }
        if seal == WalSeal::Standalone {
            self.wal.append(WalOp::Checkpoint(root), height);
            self.durable_wal_barrier()?;
        }
        {
            let _publication = self.native_inference_publication.write();
            for op in &ops {
                self.apply_wal_op(op);
            }
            {
                let mut retained = self.superseded_bindings.write();
                retained.retain(|held, _| keep.iter().any(|entry| entry.0 == *held));
                if retire_previous {
                    retained.insert(previous_commitment.0, previous.clone());
                }
            }
            *self.native_inference_context.write() = Some(context);
            debug_assert_eq!(self.compute_state_root(), root);
        }
        Ok(commitment)
    }

    /// The last committed context; poisoned WAL state exposes no active context.
    pub fn native_inference_context(&self) -> Option<InferenceAdmissionContext> {
        self.try_native_inference_context().ok().flatten()
    }

    pub fn try_native_inference_context(
        &self,
    ) -> Result<Option<InferenceAdmissionContext>, StateError> {
        self.require_healthy_wal()?;
        Ok(self.native_inference_context.read().clone())
    }

    /// Rebuild the native pending index from certified storage: every escrow
    /// metadata row is decoded, checked against its escrow account's
    /// `storage_root`, and indexed if still pending. Used on reopen and after
    /// a checkpoint rebase, never per poll.
    pub(crate) fn rebuild_native_inference_pending(
        &self,
        commitment: Hash256,
    ) -> Result<(), StateError> {
        self.native_inference_pending.clear();
        // Two passes on purpose: the body below cross-reads storage through
        // `get_storage` for the same escrow, which is the same DashMap shard
        // this iterator holds a guard on. Recursive shard reads are what the
        // two rebase deadlocks were, so the rows are collected first and the
        // iterator dropped before anything reads storage again.
        let rows: Vec<([u8; 32], Vec<u8>)> = self
            .storage
            .iter()
            .filter_map(|entry| {
                entry
                    .value()
                    .get(&metadata_key())
                    .map(|bytes| (*entry.key(), bytes.clone()))
            })
            .collect();
        for (address, bytes) in rows {
            let metadata =
                bincode::deserialize_limited_exact::<InferenceMetadata, MAX_METADATA_BYTES>(&bytes)
                    .map_err(|e| {
                        StateError::ExecutionError(format!("invalid native pending metadata: {e}"))
                    })?;
            // A pending request names the binding it was admitted under,
            // which after a configuration update is a retained one rather
            // than the current one. Anything naming neither is not this
            // chain's work and still stops the rebuild.
            if metadata.context_commitment != commitment
                && !self
                    .superseded_bindings
                    .read()
                    .contains_key(&metadata.context_commitment.0)
            {
                return Err(StateError::ExecutionError(
                    "native metadata context mismatch".into(),
                ));
            }
            let request_id = metadata.request.job.request_id();
            let receipt = read_native_metadata(self, request_id)?.ok_or_else(|| {
                StateError::ExecutionError("native metadata escrow mismatch".into())
            })?;
            if escrow_address(request_id).0 != address {
                return Err(StateError::ExecutionError(
                    "native metadata at wrong address".into(),
                ));
            }
            if metadata.status == InferenceTransitionStatus::Pending {
                if self.native_inference_pending.len() >= MAX_NATIVE_PENDING {
                    return Err(StateError::ExecutionError(
                        "native pending index exceeds bound".into(),
                    ));
                }
                self.native_inference_pending
                    .insert(request_id.0, receipt.admission_height);
            }
        }
        Ok(())
    }

    pub(crate) fn restore_native_inference_context(&self) -> Result<(), StateError> {
        // Before anything revalidates the binding: a chain that migrated says
        // so in its own state. Restoring the record first is what lets a
        // recovery-bound chain come back with paid inference still active,
        // and it is read from rooted storage, never from an operator input.
        if let Some(encoded) = self.get_storage(&context_account(), &migration_key()) {
            let record = bincode::deserialize_limited_exact::<
                NativeMigrationRecord,
                MAX_CONTEXT_BYTES,
            >(&encoded)
            .map_err(|error| {
                StateError::ExecutionError(format!("invalid native migration record: {error}"))
            })?;
            if let Some(reason) = record.refusal_against(self) {
                return Err(StateError::ExecutionError(format!(
                    "recorded native migration does not authorise this chain: {reason}"
                )));
            }
            *self.native_migration.write() = Some(record);
        }
        let Some(encoded) = self.get_storage(&context_account(), &activation_key()) else {
            if self
                .blocks
                .iter()
                .any(|block| block.header.protocol_version.major == 4)
            {
                return Err(StateError::ExecutionError(
                    "protocol-4 history has no rooted activation context".into(),
                ));
            }
            return Ok(());
        };
        let context = decode_activation(&encoded)?;
        let commitment = validate_native_inference_activation(self, &context)?;
        // Superseded bindings come back too, or a restart would strand every
        // request admitted under one. They are NOT revalidated against the
        // live registry - they are not the live committee any more, and that
        // is the point - but each must still be exactly the binding its own
        // commitment names, on this chain.
        {
            let mut retained = std::collections::BTreeMap::new();
            for held in self.retained_binding_index()? {
                let Some(bytes) = self.get_storage(&context_account(), &version_key(held)) else {
                    return Err(StateError::ExecutionError(format!(
                        "retained inference binding {} is indexed but missing",
                        held.to_hex()
                    )));
                };
                let superseded = decode_activation(&bytes)?;
                if superseded.commitment()? != held {
                    return Err(StateError::ExecutionError(format!(
                        "retained inference binding {} does not match its own commitment",
                        held.to_hex()
                    )));
                }
                if validator_set_commitment(&superseded.members).map_err(contract_error)?
                    != superseded.domain.validator_set_hash
                {
                    return Err(StateError::ExecutionError(format!(
                        "retained inference binding {} has an incorrect validator-set hash",
                        held.to_hex()
                    )));
                }
                if superseded.domain.chain_genesis != context.domain.chain_genesis {
                    return Err(StateError::ExecutionError(format!(
                        "retained inference binding {} belongs to another chain",
                        held.to_hex()
                    )));
                }
                retained.insert(held.0, superseded);
            }
            *self.superseded_bindings.write() = retained;
        }
        *self.native_inference_context.write() = Some(context);
        // Snapshot recovery can omit the derived index. Rebuild once, never per poll.
        self.rebuild_native_inference_pending(commitment)?;
        let mut native_txs: Vec<_> = self
            .full_transactions
            .iter()
            .filter(|tx| is_native_body(&tx.body))
            .map(|tx| tx.value().clone())
            .collect();
        native_txs.sort_by_key(|tx| {
            self.tx_index
                .get(&tx.hash.0)
                .map(|index| *index)
                .unwrap_or((u64::MAX, u32::MAX))
        });
        for tx in native_txs {
            self.index_account_tx(&tx);
        }
        Ok(())
    }

    /// The subset of `candidates` a protocol-4 block may carry, chosen the
    /// same way by every node.
    ///
    /// A protocol-4 block carries at most ONE native-inference transaction and
    /// nothing else. The multi-validator commit path used to hand every
    /// committed transaction to block execution, which refuses anything else -
    /// and it treats an execution refusal as fatal. Two native requests in one
    /// DAG block, or one ordinary transfer, would therefore have stopped every
    /// node's consensus loop at the same committed block.
    ///
    /// Selection is deterministic, so all nodes, which hold the same committed
    /// preimages and the same state, pick the same transaction: candidates are
    /// considered in hash order, and the first native transaction that passes
    /// the full block-admission check wins. Everything else is omitted - never
    /// executed, never turned into a failed receipt - and a byzantine proposer
    /// who packs a block with junk can at worst make it carry nothing.
    ///
    /// Off protocol 4 this returns every candidate unchanged.
    pub fn select_native_block_transactions(
        &self,
        candidates: &[arc_types::Transaction],
    ) -> Vec<arc_types::Transaction> {
        if self.native_inference_context().is_none() {
            return candidates.to_vec();
        }
        // A MIGRATED chain carries ordinary blocks too. When the committed
        // set holds no native work there is nothing to choose between, and
        // the block is an ordinary one: returning the empty native selection
        // here would drop every transfer the block actually carries. When it
        // does hold native work, the one-per-block rule applies exactly as
        // on a private chain, because a block is either ordinary or native
        // and admission refuses a mixture.
        let holds_native = candidates.iter().any(|tx| is_native_body(&tx.body));
        if self.native_migration().is_some() && !holds_native {
            return candidates.to_vec();
        }
        let mut ordered: Vec<&arc_types::Transaction> = candidates
            .iter()
            .filter(|tx| is_native_body(&tx.body))
            .collect();
        ordered.sort_by_key(|tx| tx.hash.0);
        // Each candidate costs a full admission check - signature included -
        // on every node. A leader block packed with junk would otherwise cost
        // all of them that much inside the commit path. The first N in hash
        // order are examined, identically everywhere; a block whose first N
        // hold nothing admissible carries nothing.
        //
        // A candidate whose nonce its sender has already used can never be
        // admitted, and it is skipped WITHOUT counting toward N. On protocol 4
        // a transaction was applied exactly when its nonce was consumed, so
        // this is the same set whether or not a node holds the receipts that
        // the commit path otherwise filters on. A node that rebased onto a
        // checkpoint holds none below it, and without this rule a leader
        // packing N already-applied transactions ahead of a live one would
        // make that node select differently from its peers.
        //
        // The rule is the chain's, fixed at activation: rule-v1 chains keep
        // counting every candidate, so an upgraded binary selects exactly as
        // the binaries it runs beside.
        let skip_used = self
            .native_inference_context()
            .is_some_and(|context| context.selection_rule == NativeSelectionRule::SkipUsedNoncesV2);
        let mut examined = 0usize;
        for tx in ordered {
            if skip_used
                && self
                    .get_account(&tx.from)
                    .is_some_and(|account| tx.nonce < account.nonce)
            {
                continue;
            }
            if examined == MAX_NATIVE_CANDIDATES_PER_BLOCK {
                break;
            }
            examined += 1;
            if self
                .validate_native_inference_block_admission(std::slice::from_ref(tx))
                .is_ok()
            {
                return vec![tx.clone()];
            }
        }
        Vec::new()
    }

    /// True for a native-inference transaction body.
    pub fn is_native_inference_transaction(tx: &arc_types::Transaction) -> bool {
        is_native_body(&tx.body)
    }

    /// True if `tx` could be carried by the next protocol-4 block on its own -
    /// i.e. it is worth keeping for a later proposal.
    ///
    /// Several validators may each reach the vote threshold and submit a
    /// finalize for the same request; once one executes, the others can never
    /// be admitted. Re-queueing every unselected native transaction would make
    /// those circulate forever - exactly the defect already fixed for receipted
    /// transfers - so only transactions that are still admissible go back.
    ///
    /// "Not admissible right now" is not the same as "never": a requester's
    /// nonce-2 request is inadmissible while its nonce-1 request is still
    /// pending, and becomes admissible once that one executes. So a
    /// transaction whose nonce is AHEAD of its sender's account is kept; one
    /// at or behind it that still fails can never succeed and is dropped.
    ///
    /// "Ahead" is bounded, or it is an invitation: a transaction failing for
    /// any reason at all, from a key with no account, was kept forever as long
    /// as its nonce was above zero, so anyone could fill every mempool with
    /// junk that each proposal re-validates. A future transaction is kept only
    /// if its sender has an account, its nonce is within
    /// `NATIVE_FUTURE_NONCE_WINDOW` of that account's, it is correctly
    /// signed, and - for a request - the sender can cover its reservation.
    pub fn native_transaction_still_admissible(&self, tx: &arc_types::Transaction) -> bool {
        if !is_native_body(&tx.body) {
            return false;
        }
        if self
            .validate_native_inference_block_admission(std::slice::from_ref(tx))
            .is_ok()
        {
            return true;
        }
        let Some(account) = self.get_account(&tx.from) else {
            return false;
        };
        if tx.nonce <= account.nonce
            || tx.nonce > account.nonce.saturating_add(NATIVE_FUTURE_NONCE_WINDOW)
        {
            return false;
        }
        if let arc_types::TxBody::NativeInferenceRequest(body) = &tx.body
            && account.balance < body.request.job.reserved_max_payment
        {
            return false;
        }
        self.verify_transaction_signature(tx).is_ok()
    }

    pub(crate) fn validate_native_inference_block_admission(
        &self,
        transactions: &[arc_types::Transaction],
    ) -> Result<(), StateError> {
        let height = self
            .height()
            .checked_add(1)
            .ok_or_else(|| StateError::ExecutionError("native block height overflow".into()))?;
        self.validate_native_inference_block_admission_at(transactions, height)
    }

    /// Lock-free canonical preflight: execution callers already hold the
    /// native execution lock. Local runtime availability is never validity.
    pub(crate) fn validate_native_inference_block_admission_at(
        &self,
        transactions: &[arc_types::Transaction],
        height: u64,
    ) -> Result<(), StateError> {
        let context = self.native_inference_context();
        let contains_native = transactions.iter().any(|tx| is_native_body(&tx.body));
        let Some(context) = context else {
            if contains_native {
                return Err(StateError::ExecutionError(
                    "native inference protocol-4 context is not activated".into(),
                ));
            }
            return Ok(());
        };
        // A chain that MIGRATED keeps the transaction families it already
        // had. The fresh-genesis rule - one native transaction per block and
        // nothing else - exists because a private protocol-4 chain has no
        // other families to run. An existing public chain does, and its
        // balances, transfers, contracts and rewards were promised to the
        // people already using it; activation must not silently end them.
        //
        // What activation genuinely does freeze is the committee, because the
        // binding names its exact members. So a migrated chain refuses the
        // registry changes that would break the binding, and nothing else. A
        // block is then either an ordinary block or a native block, never a
        // mixture, which leaves native execution exactly as it was qualified.
        if self.native_migration().is_some() && !contains_native {
            // Including a registry change. Activation used to freeze the
            // committee for the chain's life; with versioned bindings it does
            // not, so staking and validator admission survive activation.
            // New requests are refused between the registry moving and the
            // operator publishing the matching binding - fail-closed - and
            // requests already admitted settle under the binding that
            // accepted them.
            return Ok(());
        }
        if transactions.len() > 1 || transactions.iter().any(|tx| !is_native_body(&tx.body)) {
            return Err(StateError::ExecutionError(
                "private protocol-4 blocks allow at most one native inference transaction".into(),
            ));
        }
        if self.height().checked_add(1) != Some(height) {
            return Err(StateError::ExecutionError(
                "native admission requires the next canonical block height".into(),
            ));
        }
        validate_native_inference_activation(self, &context)?;
        if let Some(tx) = transactions.first() {
            self.plan_native_transaction(tx, &context, height)?;
        }
        Ok(())
    }

    /// The binding a pending request was admitted under, when that is not
    /// the current one. `None` means "use the current binding": either the
    /// request names it, or there is no such request and the planner reports
    /// that in its own words.
    fn binding_for_request(
        &self,
        request_id: Hash256,
    ) -> Result<Option<InferenceAdmissionContext>, StateError> {
        let Some(bytes) = self.get_storage(&escrow_address(request_id), &metadata_key()) else {
            return Ok(None);
        };
        if bytes.len() > MAX_METADATA_BYTES {
            return Ok(None);
        }
        let Ok(metadata) =
            bincode::deserialize_limited_exact::<InferenceMetadata, MAX_METADATA_BYTES>(&bytes)
        else {
            return Ok(None);
        };
        let current = self
            .native_inference_context()
            .and_then(|context| context.commitment().ok());
        if current == Some(metadata.context_commitment) {
            return Ok(None);
        }
        match self
            .superseded_bindings
            .read()
            .get(&metadata.context_commitment.0)
        {
            Some(context) => Ok(Some(context.clone())),
            None => Err(StateError::ExecutionError(format!(
                "request {} was admitted under inference binding {}, which this node no longer \
                 holds; it cannot be settled or refunded here",
                request_id.to_hex(),
                metadata.context_commitment.to_hex()
            ))),
        }
    }

    fn plan_native_transaction(
        &self,
        tx: &arc_types::Transaction,
        context: &InferenceAdmissionContext,
        height: u64,
    ) -> Result<InferenceTransitionRecord, StateError> {
        self.require_healthy_wal()?;
        if self.native_inference_context().as_ref() != Some(context) {
            return Err(StateError::ExecutionError(
                "native context is not activated".into(),
            ));
        }
        // From here, `context` is the CURRENT binding. Which binding governs
        // the transaction is decided just below.
        // Which binding governs THIS transaction. A new request is admitted
        // under the current one - a configuration update governs new work.
        // A settlement or refund is governed by the binding its request was
        // admitted under, which the update retained for exactly this reason,
        // so work already in flight is never stranded by a committee or
        // execution change.
        let current_commitment = context.commitment()?;
        let retained = match &tx.body {
            arc_types::TxBody::NativeInferenceFinalize(body) => {
                self.binding_for_request(Hash256(body.request_id))?
            }
            arc_types::TxBody::NativeInferenceRefund(body) => {
                self.binding_for_request(Hash256(body.request_id))?
            }
            _ => None,
        };
        let governing: &InferenceAdmissionContext = retained.as_ref().unwrap_or(context);
        self.validate_native_inference_envelope_at(tx, governing, height)?;
        if tx.gas_limit < Self::gas_cost_for_tx(tx) {
            return Err(StateError::ExecutionError(
                "native transaction has insufficient gas".into(),
            ));
        }
        let caller = self
            .get_account(&tx.from)
            .ok_or(StateError::AccountNotFound(tx.from))?;
        if caller.nonce != tx.nonce {
            return Err(StateError::InvalidNonce {
                expected: caller.nonce,
                got: tx.nonce,
            });
        }
        let next_nonce = caller
            .nonce
            .checked_add(1)
            .ok_or_else(|| StateError::ExecutionError("native caller nonce overflow".into()))?;
        let planner = InferencePlanner {
            state: self,
            context: governing,
            context_commitment: governing.commitment()?,
            current_commitment,
        };
        let request = matches!(&tx.body, arc_types::TxBody::NativeInferenceRequest(_));
        let plan = match &tx.body {
            arc_types::TxBody::NativeInferenceRequest(body) => {
                if self.native_inference_pending.len() >= MAX_NATIVE_PENDING {
                    return Err(StateError::ExecutionError(
                        "native pending index is full".into(),
                    ));
                }
                planner.plan_admit(body.request.clone(), &body.input_blob, height)?
            }
            arc_types::TxBody::NativeInferenceFinalize(body) => {
                planner.plan_finalize(Hash256(body.request_id), &body.certificate, height)?
            }
            arc_types::TxBody::NativeInferenceRefund(body) => {
                planner.plan_refund(Hash256(body.request_id), height)?
            }
            _ => {
                return Err(StateError::ExecutionError(
                    "not a native inference transaction".into(),
                ));
            }
        };
        let InferencePlan::Transition(record) = plan else {
            return Err(StateError::ExecutionError(
                "native inference request is already terminal".into(),
            ));
        };
        let mut record = *record;
        if tx.from == record.escrow || tx.from == context_account() {
            return Err(StateError::ExecutionError(
                "native caller aliases reserved state account".into(),
            ));
        }
        if !request {
            // The outer caller can also be a payee. Coalesce replacements so
            // settlement credits and the single consumed caller nonce survive.
            if let Some((_, account)) = record
                .account_updates
                .iter_mut()
                .find(|(addr, _)| *addr == tx.from)
            {
                account.nonce = next_nonce;
            } else {
                let mut account = caller;
                account.nonce = next_nonce;
                record.account_updates.push((tx.from, account));
            }
        }
        record.account_updates.sort_by_key(|(address, _)| address.0);
        planner.validate_record(&record)?;
        Ok(record)
    }

    pub(crate) fn index_native_inference_payees(
        &self,
        request_id: Hash256,
        tx: &arc_types::Transaction,
    ) {
        let Some(context) = self.native_inference_context() else {
            return;
        };
        let Ok(_commitment) = context.commitment() else {
            return;
        };
        let Ok(Some(receipt)) = read_native_metadata(self, request_id) else {
            return;
        };
        for credit in receipt.metadata.credits {
            if credit.amount == 0 || credit.payee == tx.from {
                continue;
            }
            let mut history = self.account_txs.entry(credit.payee.0).or_default();
            if history.len() >= 10_000 {
                history.drain(..1000);
            }
            history.push(tx.hash);
        }
    }

    /// Project the account-root backend without publishing any replacements.
    /// The private candidate trades an O(accounts) projection for a clear
    /// durability boundary; worker polling remains O(bounded pending jobs).
    fn projected_native_root(
        &self,
        replacements: &[(arc_types::Address, Account)],
        storage_replacements: &[(arc_types::Address, Hash256, Vec<u8>)],
    ) -> Hash256 {
        // A MIGRATED chain computes its root the recovery way, over domains a
        // bare account projection never copies - storage among them, which a
        // native settlement writes. Projecting the genesis way here would
        // commit a root no validator, including this one one line later,
        // could reproduce.
        if let Some(context) = self.recovery_context() {
            return self.compute_recovery_state_root_with(
                &context,
                replacements,
                storage_replacements,
            );
        }
        let mut projected = StateDB::new();
        projected.use_jmt = self.use_jmt;
        for entry in self.accounts.iter() {
            projected
                .accounts
                .insert(*entry.key(), entry.value().clone());
        }
        for (address, account) in replacements {
            projected.accounts.insert(address.0, account.clone());
        }
        projected.compute_state_root()
    }

    /// The state root this node will have once `ops` are applied, computed the
    /// way THIS state computes roots.
    ///
    /// `projected_native_root` above builds a bare `StateDB` holding only
    /// accounts, which is faithful at a fresh genesis and nowhere else: on a
    /// recovery-bound chain `compute_state_root` takes a different branch
    /// entirely (`compute_recovery_state_root`) and commits domains that
    /// projection never copies. Activating mid-chain with that number would
    /// have written a checkpoint root no validator could reproduce.
    ///
    /// This projects by the idiom the rebase path already uses: snapshot the
    /// real state, apply the same ops to the copy, and ask the copy. It costs
    /// one snapshot, once, at the activation height.
    fn projected_root_after(&self, ops: &[WalOp]) -> Hash256 {
        let scratch = StateDB::new();
        scratch.install_durable_snapshot(&self.export_durable_snapshot());
        *scratch.recovery_context.write() = self.recovery_context();
        for op in ops {
            scratch.apply_wal_op(op);
        }
        scratch.compute_state_root()
    }

    /// The caller holds the serial native execution lock. No financial state,
    /// height, or receipt becomes visible until the full block is durable.
    pub(crate) fn execute_native_inference_block_at(
        &self,
        transactions: &[arc_types::Transaction],
        producer: arc_types::Address,
        timestamp: u64,
        proof_hash: Hash256,
    ) -> Result<(arc_types::Block, Vec<arc_types::TxReceipt>), StateError> {
        self.require_healthy_wal()?;
        if self.active_protocol_version().major == 3 {
            self.validate_v3_block_admission(transactions)?;
        } else {
            self.validate_native_inference_block_admission(transactions)?;
        }
        let context = self
            .native_inference_context()
            .ok_or_else(|| StateError::ExecutionError("native context is unavailable".into()))?;
        let height = self
            .height()
            .checked_add(1)
            .ok_or_else(|| StateError::ExecutionError("native height overflow".into()))?;
        let record = transactions
            .first()
            .map(|tx| self.plan_native_transaction(tx, &context, height))
            .transpose()?;
        let replacements = record
            .as_ref()
            .map(|r| r.account_updates.as_slice())
            .unwrap_or(&[]);
        let storage_replacements = record
            .as_ref()
            .map(|r| r.storage_updates.as_slice())
            .unwrap_or(&[]);
        let state_root = self.projected_native_root(replacements, storage_replacements);
        let tx_hashes: Vec<_> = transactions.iter().map(|tx| tx.hash).collect();
        let tree = arc_crypto::merkle::MerkleTree::from_leaves(tx_hashes.clone());
        let parent_hash = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);
        let block = arc_types::Block::new(
            arc_types::BlockHeader {
                height,
                timestamp,
                parent_hash,
                tx_root: tree.root(),
                state_root,
                proof_hash,
                tx_count: transactions.len() as u32,
                producer,
                protocol_version: self.active_protocol_version(),
                state_diff: None,
            },
            tx_hashes,
        );
        let receipts: Vec<_> = transactions
            .iter()
            .enumerate()
            .map(|(index, tx)| arc_types::TxReceipt {
                tx_hash: tx.hash,
                block_height: height,
                block_hash: block.hash,
                index: index as u32,
                success: true,
                gas_used: Self::gas_cost_for_tx(tx),
                value_commitment: None,
                inclusion_proof: tree
                    .proof(index)
                    .and_then(|proof| bincode::serialize(&proof).ok()),
                logs: Vec::new(),
            })
            .collect();
        if let Some(record) = &record {
            self.wal
                .append(WalOp::InferenceTransition(Box::new(record.clone())), height);
        }
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;
        {
            let _publication = self.native_inference_publication.write();
            if let Some(record) = record {
                self.apply_wal_op(&WalOp::InferenceTransition(Box::new(record)));
            }
            self.apply_wal_op(&WalOp::SetBlock(height, block.clone()));
            for (tx, receipt) in transactions.iter().zip(&receipts) {
                self.receipts.insert(tx.hash.0, receipt.clone());
                self.full_transactions.insert(tx.hash.0, tx.clone());
            }
            debug_assert_eq!(self.compute_state_root(), state_root);
        }
        for tx in transactions {
            self.index_account_tx(tx);
        }
        Ok((block, receipts))
    }
}

pub(crate) fn is_native_body(body: &arc_types::TxBody) -> bool {
    matches!(
        body,
        arc_types::TxBody::NativeInferenceRequest(_)
            | arc_types::TxBody::NativeInferenceFinalize(_)
            | arc_types::TxBody::NativeInferenceRefund(_)
    )
}

impl StateDB {
    /// Validate a candidate protocol-4 native transaction at its exact block
    /// height. This is an ingress/state preflight only; it performs no debit,
    /// nonce consumption, WAL append, or model-quality claim.
    pub fn validate_native_inference_transaction_admission_at(
        &self,
        tx: &arc_types::Transaction,
        context: &InferenceAdmissionContext,
        execution_height: u64,
    ) -> Result<(), StateError> {
        let _guard = self.native_inference_execution.lock();
        self.require_healthy_wal()?;
        if self.height().checked_add(1) != Some(execution_height) {
            return Err(StateError::ExecutionError(
                "native ingress preflight requires the next canonical block height".into(),
            ));
        }
        self.plan_native_transaction(tx, context, execution_height)
            .map(|_| ())
    }

    /// The ingress preflight: `validate_native_inference_transaction_admission_at`
    /// for the next canonical height, read under the same lock. A caller that
    /// read the height itself could lose a race with block application and be
    /// refused for a height that had just moved on - a spurious rejection of
    /// a valid request whenever blocks are fast.
    pub fn validate_native_inference_transaction_admission_next(
        &self,
        tx: &arc_types::Transaction,
        context: &InferenceAdmissionContext,
    ) -> Result<(), StateError> {
        let _guard = self.native_inference_execution.lock();
        self.require_healthy_wal()?;
        let execution_height = self.height().saturating_add(1);
        self.plan_native_transaction(tx, context, execution_height)
            .map(|_| ())
    }

    fn validate_native_inference_envelope_at(
        &self,
        tx: &arc_types::Transaction,
        context: &InferenceAdmissionContext,
        execution_height: u64,
    ) -> Result<(), StateError> {
        self.require_healthy_wal()?;
        // The binding here is either the current one - for new work - or one
        // this chain retained because work admitted under it has not settled.
        // A retained binding is not revalidated against the live registry: it
        // is no longer the live committee, and it is held precisely so the
        // work it accepted can finish on its own terms. What proves it is the
        // binding it claims to be is its own commitment, which is rooted in
        // this chain's state.
        let expected_context = if self.native_inference_context().as_ref() == Some(context) {
            validate_native_inference_activation(self, context)?
        } else {
            let commitment = context.commitment()?;
            if !self.superseded_bindings.read().contains_key(&commitment.0) {
                return Err(StateError::ExecutionError(
                    "native inference context is not activated".into(),
                ));
            }
            commitment
        };
        if tx.fee != 0 {
            return Err(StateError::ExecutionError(
                "native inference transactions require fee=0; price is signed in the job".into(),
            ));
        }
        if tx.gas_limit == 0 || tx.gas_limit > arc_types::transaction::gas_costs::BLOCK_GAS_LIMIT {
            return Err(StateError::ExecutionError(
                "native inference transaction gas_limit is outside the bounded range".into(),
            ));
        }
        if tx.signature.is_null() {
            return Err(StateError::ExecutionError(
                "native inference transaction signature is required".into(),
            ));
        }
        self.verify_transaction_signature(tx)
            .map_err(|error| StateError::ExecutionError(format!("native signature: {error}")))?;
        if tx.tx_type != tx.body.tx_type() {
            return Err(StateError::ExecutionError(
                "native transaction type/body mismatch".into(),
            ));
        }
        match &tx.body {
            arc_types::TxBody::NativeInferenceRequest(body) => {
                if body.input_blob.len() > arc_types::transaction::TIER1_INPUT_BLOB_MAX
                    || hash_bytes(&body.input_blob) != body.request.job.input_hash
                {
                    return Err(StateError::ExecutionError(
                        "native request input does not match its signed hash".into(),
                    ));
                }
                if body.request.job.requester != tx.from || body.request.job.nonce != tx.nonce {
                    return Err(StateError::ExecutionError(
                        "native request outer sender/nonce does not match signed job".into(),
                    ));
                }
                if !context.allows(&body.request) {
                    return Err(StateError::ExecutionError(
                        "native request execution tuple is not in the pinned allowlist".into(),
                    ));
                }
                body.request
                    .validate(&context.domain, &context.members, execution_height)
                    .map_err(contract_error)?;
            }
            arc_types::TxBody::NativeInferenceFinalize(body) => {
                if body.certificate.output.is_empty()
                    || body.certificate.output.len() > arc_types::transaction::TIER1_OUTPUT_BLOB_MAX
                    || body.certificate.votes.is_empty()
                    || body.certificate.votes.len() > context.members.len()
                {
                    return Err(StateError::ExecutionError(
                        "native certificate exceeds bounded output/member limits".into(),
                    ));
                }
            }
            arc_types::TxBody::NativeInferenceRefund(_) => {}
            _ => {
                return Err(StateError::ExecutionError(
                    "not a native protocol-4 inference transaction".into(),
                ));
            }
        }
        // Keep the value live in this preflight so a future caller cannot
        // accidentally validate against a different context after the file
        // and registry checks above.
        let _ = expected_context;
        Ok(())
    }

    /// Return bounded pending native requests from persisted escrow metadata.
    /// This is intentionally a derived read index; canonical writes still
    /// belong to block execution and its checkpoint barrier.
    pub fn native_inference_pending_requests(
        &self,
        context_commitment: Hash256,
    ) -> Result<Vec<NativeInferencePendingSnapshot>, StateError> {
        let _guard = self.native_inference_execution.lock();
        self.require_healthy_wal()?;
        let mut requests = Vec::new();
        for entry in self.native_inference_pending.iter() {
            let request_id = Hash256(*entry.key());
            let receipt = read_native_metadata(self, request_id)?.ok_or_else(|| {
                StateError::ExecutionError("native pending index references missing receipt".into())
            })?;
            // Pending work must name a binding this chain still holds: the
            // current one, or one retained because work admitted under it has
            // not settled. Anything else is not work this node can execute.
            if receipt.metadata.context_commitment != context_commitment
                && !self
                    .superseded_bindings
                    .read()
                    .contains_key(&receipt.metadata.context_commitment.0)
            {
                return Err(StateError::ExecutionError(
                    "native pending index references an unknown inference binding".into(),
                ));
            }
            if receipt.metadata.status != InferenceTransitionStatus::Pending {
                return Err(StateError::ExecutionError(
                    "native pending index references terminal receipt".into(),
                ));
            }
            requests.push(NativeInferencePendingSnapshot {
                request_id,
                request: receipt.metadata.request,
                input_blob: receipt.metadata.input_blob,
                admission_height: receipt.admission_height,
                context_commitment,
            });
        }
        requests.sort_by_key(|request| request.request_id.0);
        Ok(requests)
    }

    /// One pending request, read directly: `None` when it is not pending.
    ///
    /// The vote path used to call `native_inference_pending_requests` and
    /// search the result - decoding every pending request's metadata, input
    /// included, for each vote it handled. With every validator re-sending
    /// each pending vote every few seconds, that is quadratic in the number of
    /// pending requests, on the consensus thread.
    pub fn native_inference_pending_request(
        &self,
        request_id: Hash256,
        context_commitment: Hash256,
    ) -> Result<Option<NativeInferencePendingSnapshot>, StateError> {
        let _guard = self.native_inference_execution.lock();
        self.require_healthy_wal()?;
        if !self.native_inference_pending.contains_key(&request_id.0) {
            return Ok(None);
        }
        let receipt = read_native_metadata(self, request_id)?.ok_or_else(|| {
            StateError::ExecutionError("native pending index references missing receipt".into())
        })?;
        if receipt.metadata.status != InferenceTransitionStatus::Pending {
            return Err(StateError::ExecutionError(
                "native pending index references terminal receipt".into(),
            ));
        }
        Ok(Some(NativeInferencePendingSnapshot {
            request_id,
            request: receipt.metadata.request,
            input_blob: receipt.metadata.input_blob,
            admission_height: receipt.admission_height,
            context_commitment,
        }))
    }

    /// Read a request-keyed native receipt after checking its escrow
    /// commitment. Terminal status is returned unchanged; no payment action
    /// occurs in this accessor. The context argument is retained for API
    /// compatibility; historical receipts remain readable after a binding change.
    pub fn native_inference_receipt(
        &self,
        request_id: Hash256,
        _context_commitment: Hash256,
    ) -> Result<Option<NativeInferenceReceiptSnapshot>, StateError> {
        let _guard = self.native_inference_execution.lock();
        self.require_healthy_wal()?;
        read_native_metadata(self, request_id)
    }
}

impl IsolatedInferenceLedger {
    pub fn new(state: StateDB, context: InferenceAdmissionContext) -> Result<Self, StateError> {
        if state.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "activated canonical native state cannot use isolated per-transition checkpoints"
                    .into(),
            ));
        }
        let context_commitment = validate_native_inference_activation(&state, &context)?;
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

    fn transition(&mut self, record: InferenceTransitionRecord) -> Result<Hash256, StateError> {
        self.ensure_usable()?;
        self.planner().validate_record(&record)?;
        let height = self.state.height();
        self.state
            .wal
            .append(WalOp::InferenceTransition(Box::new(record.clone())), height);
        if let Err(error) = self.state.durable_wal_barrier() {
            self.poisoned = true;
            return Err(error);
        }

        // The typed record is durable before this publication. A crash before
        // the checkpoint leaves it as an uncommitted WAL suffix for recovery
        // to quarantine; this adapter remains poisoned on the live process.
        self.state
            .apply_wal_op(&WalOp::InferenceTransition(Box::new(record)));
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

    fn planner(&self) -> InferencePlanner<'_> {
        // The isolated ledger holds exactly one binding and never updates it,
        // so its governing binding and its pin are the same.
        InferencePlanner {
            state: &self.state,
            context: &self.context,
            context_commitment: self.context_commitment,
            current_commitment: self.context_commitment,
        }
    }

    fn publish_plan(
        &mut self,
        plan: InferencePlan,
    ) -> Result<IsolatedTransitionResult, StateError> {
        match plan {
            InferencePlan::Transition(record) => {
                let request_id = record.request_id;
                let status = record.metadata.status;
                let state_root = self.transition(*record)?;
                Ok(IsolatedTransitionResult::Applied {
                    request_id,
                    state_root,
                    status,
                })
            }
            InferencePlan::AlreadyTerminal {
                request_id,
                status,
                output_hash,
            } => Ok(IsolatedTransitionResult::AlreadyTerminal {
                request_id,
                status,
                output_hash,
            }),
        }
    }

    pub fn admit(
        &mut self,
        request: InferenceRequest,
        input_blob: &[u8],
        now: u64,
    ) -> Result<IsolatedTransitionResult, StateError> {
        self.ensure_usable()?;
        let plan = self.planner().plan_admit(request, input_blob, now)?;
        self.publish_plan(plan)
    }

    pub fn finalize(
        &mut self,
        request_id: Hash256,
        certificate: &InferenceCertificate,
        now: u64,
    ) -> Result<IsolatedTransitionResult, StateError> {
        self.ensure_usable()?;
        let plan = self.planner().plan_finalize(request_id, certificate, now)?;
        self.publish_plan(plan)
    }

    pub fn refund(
        &mut self,
        request_id: Hash256,
        now: u64,
    ) -> Result<IsolatedTransitionResult, StateError> {
        self.ensure_usable()?;
        let plan = self.planner().plan_refund(request_id, now)?;
        self.publish_plan(plan)
    }
}

/// Pure borrowed planner shared by isolated and canonical state publication.
struct InferencePlanner<'a> {
    state: &'a StateDB,
    /// The binding that governs THIS transaction: the current one for a new
    /// request, the one it was admitted under for a settlement or refund.
    context: &'a InferenceAdmissionContext,
    context_commitment: Hash256,
    /// The binding the context account is pinned to right now. The pin and
    /// the inference clock belong to the account, not to a binding version,
    /// so a request settling under a superseded binding still reads and
    /// writes the current pin.
    current_commitment: Hash256,
}

enum InferencePlan {
    Transition(Box<InferenceTransitionRecord>),
    AlreadyTerminal {
        request_id: Hash256,
        status: InferenceTransitionStatus,
        output_hash: Option<Hash256>,
    },
}

impl InferencePlanner<'_> {
    /// For NEW work only: the binding being admitted under must still be the
    /// live committee. Between a registry change and the operator publishing
    /// the matching binding, new requests are refused - fail-closed - while
    /// work already admitted goes on settling under its own binding.
    fn ensure_context_current(&self) -> Result<(), StateError> {
        if state_members(self.state) != self.context.members {
            return Err(StateError::ExecutionError(
                "active validator set changed after isolated context pin".into(),
            ));
        }
        self.ensure_binding_usable()
    }

    /// For work ALREADY admitted: its binding is pinned by its own
    /// commitment, so the live committee is not consulted. What still has to
    /// hold is that this chain is one the binding was authorised for.
    fn ensure_binding_usable(&self) -> Result<(), StateError> {
        // Recovery-bound state is legitimate only for a chain that MIGRATED,
        // and only while the record still authorises it. A later recovery to
        // a new epoch invalidates the record, and native work stops there
        // rather than planning against a repositioned state.
        if self.state.recovery_context().is_some()
            && self
                .state
                .native_migration()
                .is_none_or(|record| record.refusal_against(self.state).is_some())
        {
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
        if account.storage_root != self.current_commitment {
            return Err(StateError::ExecutionError(
                "persisted inference clock is not bound to the context account".into(),
            ));
        }
        Ok(account.nonce)
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

    fn plan_admit(
        &self,
        request: InferenceRequest,
        input_blob: &[u8],
        now: u64,
    ) -> Result<InferencePlan, StateError> {
        self.state.require_healthy_wal()?;
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
            pin_account.storage_root = self.current_commitment;
            storage_updates.push((
                context_account_address,
                context_key(),
                self.current_commitment.0.to_vec(),
            ));
        } else if pin_account.storage_root != self.current_commitment {
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
        self.validate_record(&record)?;
        Ok(InferencePlan::Transition(Box::new(record)))
    }

    fn plan_finalize(
        &self,
        request_id: Hash256,
        certificate: &InferenceCertificate,
        now: u64,
    ) -> Result<InferencePlan, StateError> {
        self.plan_finish(request_id, certificate, now)
    }

    fn plan_refund(&self, request_id: Hash256, now: u64) -> Result<InferencePlan, StateError> {
        self.state.require_healthy_wal()?;
        self.ensure_binding_usable()?;
        let (metadata, admission_height, escrow) = self.load_metadata(request_id)?;
        match metadata.status {
            InferenceTransitionStatus::Refunded => {
                return Ok(InferencePlan::AlreadyTerminal {
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
        let record = self.plan_terminal(terminal, request_id, admission_height, escrow, now)?;
        Ok(InferencePlan::Transition(Box::new(record)))
    }

    fn plan_finish(
        &self,
        request_id: Hash256,
        certificate: &InferenceCertificate,
        now: u64,
    ) -> Result<InferencePlan, StateError> {
        self.state.require_healthy_wal()?;
        self.ensure_binding_usable()?;
        let (metadata, admission_height, escrow) = self.load_metadata(request_id)?;
        match metadata.status {
            InferenceTransitionStatus::Finalized => {
                if metadata.certificate.as_ref() == Some(certificate) {
                    return Ok(InferencePlan::AlreadyTerminal {
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
        let record = self.plan_terminal(terminal, request_id, admission_height, escrow, now)?;
        Ok(InferencePlan::Transition(Box::new(record)))
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

    fn plan_terminal(
        &self,
        metadata: InferenceMetadata,
        request_id: Hash256,
        admission_height: u64,
        escrow: arc_types::Address,
        now: u64,
    ) -> Result<InferenceTransitionRecord, StateError> {
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
            accounts.entry(credit.payee.0).or_insert_with(|| {
                self.state
                    .get_account(&credit.payee)
                    .unwrap_or_else(|| Account::new(credit.payee, 0))
            });
            let account = accounts.get_mut(&credit.payee.0).expect("inserted above");
            account.balance = account
                .balance
                .checked_add(credit.amount)
                .ok_or_else(|| StateError::ExecutionError("credit balance overflow".into()))?;
        }
        let mut account_updates = accounts
            .into_values()
            .map(|account| (account.address, account))
            .collect::<Vec<_>>();
        let context_address = context_account();
        let mut context_state = self
            .state
            .get_account(&context_address)
            .ok_or_else(|| StateError::ExecutionError("context clock account is missing".into()))?;
        if context_state.balance != 0
            || context_state.code_hash != Hash256::ZERO
            || context_state.storage_root != self.current_commitment
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
        self.validate_record(&record)?;
        Ok(record)
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
            selection_rule: NativeSelectionRule::SkipUsedNoncesV2,
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
        let _admitted = fixture.ledger.admit(request.clone(), b"input", 1).unwrap();
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
        // `_admitted` holds no borrow of the ledger, so dropping it explicitly
        // does nothing except extend its lifetime to this point.
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
        assert!(fixture.ledger.account(&request.job.requester).is_err());
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
            restarted
                .planner()
                .load_metadata(request_id)
                .unwrap()
                .0
                .status,
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
        assert_eq!(reopened.planner().trusted_time().unwrap(), 10);
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
    fn outer(
        state: &StateDB,
        key: &KeyPair,
        nonce: u64,
        body: arc_types::TxBody,
    ) -> arc_types::Transaction {
        let mut tx = arc_types::Transaction::new_transfer(key.address(), key.address(), 0, nonce);
        tx.tx_type = body.tx_type();
        tx.body = body;
        tx.gas_limit = StateDB::gas_cost_for_tx(&tx);
        state.sign_transaction(&mut tx, key).unwrap();
        tx
    }

    /// Like `outer`, but for an ordinary family that pays a fee. Native
    /// transactions require fee=0; a v3 transfer requires at least 1.
    fn outer_with_fee(
        state: &StateDB,
        key: &KeyPair,
        nonce: u64,
        fee: u64,
        body: arc_types::TxBody,
    ) -> arc_types::Transaction {
        let mut tx = arc_types::Transaction::new_transfer(key.address(), key.address(), 0, nonce);
        tx.tx_type = body.tx_type();
        tx.body = body;
        tx.fee = fee;
        tx.gas_limit = StateDB::gas_cost_for_tx(&tx);
        state.sign_transaction(&mut tx, key).unwrap();
        tx
    }

    fn native_request(
        state: &StateDB,
        key: &KeyPair,
        request: InferenceRequest,
    ) -> arc_types::Transaction {
        outer(
            state,
            key,
            request.job.nonce,
            arc_types::TxBody::NativeInferenceRequest(
                arc_types::transaction::NativeInferenceRequestBody {
                    request,
                    input_blob: b"input".to_vec(),
                },
            ),
        )
    }

    fn native_finalize(
        state: &StateDB,
        key: &KeyPair,
        nonce: u64,
        request_id: Hash256,
        certificate: InferenceCertificate,
    ) -> arc_types::Transaction {
        outer(
            state,
            key,
            nonce,
            arc_types::TxBody::NativeInferenceFinalize(
                arc_types::transaction::NativeInferenceFinalizeBody {
                    request_id: request_id.0,
                    certificate,
                },
            ),
        )
    }

    fn native_refund(
        state: &StateDB,
        key: &KeyPair,
        nonce: u64,
        request_id: Hash256,
    ) -> arc_types::Transaction {
        outer(
            state,
            key,
            nonce,
            arc_types::TxBody::NativeInferenceRefund(
                arc_types::transaction::NativeInferenceRefundBody {
                    request_id: request_id.0,
                },
            ),
        )
    }

    fn assert_rejected_unchanged(state: &StateDB, txs: &[arc_types::Transaction]) {
        let height = state.height();
        let root = state.get_state_root();
        let wal = std::fs::read(state.persistence_dir().unwrap().join("state.wal")).unwrap();
        assert!(
            state
                .execute_block_adaptive_at(txs, Hash256::ZERO, 123)
                .is_err()
        );
        assert_eq!(state.height(), height);
        assert_eq!(state.get_state_root(), root);
        assert_eq!(
            std::fs::read(state.persistence_dir().unwrap().join("state.wal")).unwrap(),
            wal
        );
    }

    #[test]
    fn canonical_native_reserve_finalize_receipts_and_restart() {
        let f = fixture("canonical-finalize");
        let state = &f.ledger.state;
        let commitment = state.activate_native_inference(f.context.clone()).unwrap();
        let req = request(&f, 0, 100);
        let id = req.job.request_id();
        let tx = native_request(state, &f.requester, req);
        let (block, receipts) = state
            .execute_block_adaptive_at(std::slice::from_ref(&tx), f.validators[0].address(), 10)
            .unwrap();
        assert_eq!(block.header.protocol_version.major, 4);
        assert!(receipts[0].success);
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            900
        );
        assert_eq!(state.get_account(&f.requester.address()).unwrap().nonce, 1);
        assert_eq!(
            state
                .native_inference_pending_requests(commitment)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 100);
        // Caller is also a validator payee; its nonce and credit must coexist.
        let terminal = native_finalize(state, &f.validators[0], 0, id, certificate(&f, id));
        let (_, receipts) = state
            .execute_block_adaptive_at(
                std::slice::from_ref(&terminal),
                f.validators[0].address(),
                20,
            )
            .unwrap();
        assert!(receipts[0].success);
        assert_eq!(
            state.get_account(&f.validators[0].address()).unwrap().nonce,
            1
        );
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            990
        );
        assert_eq!(state.get_account(&f.requester.address()).unwrap().nonce, 1);
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 0);
        let paid: u64 = f
            .validators
            .iter()
            .map(|v| state.get_account(&v.address()).unwrap().balance)
            .sum();
        assert_eq!(paid, 10);
        let root = state.get_state_root();
        assert_rejected_unchanged(state, std::slice::from_ref(&terminal));
        let next_duplicate = native_finalize(state, &f.validators[0], 1, id, certificate(&f, id));
        assert_rejected_unchanged(state, &[next_duplicate]);
        let dir = f.dir.clone();
        let prefunded = f.prefunded.clone();
        let genesis = f.genesis;
        drop(f);
        let reopened = StateDB::with_genesis_persistent(&prefunded, &dir, genesis).unwrap();
        assert_eq!(reopened.active_protocol_version().major, 4);
        assert_eq!(reopened.get_state_root(), root);
        assert_eq!(reopened.height(), 2);
        assert!(reopened.receipts.get(&terminal.hash.0).unwrap().success);
        assert!(reopened.full_transactions.contains_key(&tx.hash.0));
        assert!(reopened.get_account_txs(&tx.from.0).contains(&tx.hash));
        assert!(
            reopened
                .get_account_txs(&terminal.from.0)
                .contains(&terminal.hash)
        );
        assert!(
            reopened
                .native_inference_pending_requests(commitment)
                .unwrap()
                .is_empty()
        );
        let receipt = reopened
            .native_inference_receipt(id, commitment)
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.metadata.status,
            InferenceTransitionStatus::Finalized
        );
        assert_eq!(receipt.metadata.output, b"deterministic output");
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn protocol4_commit_selection_is_one_admissible_native_transaction_in_hash_order() {
        let f = fixture("select-native");
        let state = &f.ledger.state;
        let hashes = |txs: Vec<arc_types::Transaction>| -> Vec<Hash256> {
            txs.iter().map(|tx| tx.hash).collect()
        };
        let mut transfer = arc_types::Transaction::new_transfer(
            f.requester.address(),
            f.validators[0].address(),
            1,
            0,
        );
        state.sign_transaction(&mut transfer, &f.requester).unwrap();
        // Off protocol 4, selection is the identity.
        assert_eq!(
            hashes(state.select_native_block_transactions(&[transfer.clone()])),
            vec![transfer.hash]
        );

        state.activate_native_inference(f.context.clone()).unwrap();
        let first = request(&f, 0, 100);
        let id = first.job.request_id();
        let r0 = native_request(state, &f.requester, first);
        let r1 = native_request(state, &f.requester, request(&f, 1, 100));
        // Whatever order the DAG block listed them in: the transfer can never
        // ride a protocol-4 block and nonce 1 is not admissible yet, so the
        // nonce-0 request alone is selected.
        for candidates in [
            vec![transfer.clone(), r0.clone(), r1.clone()],
            vec![r1.clone(), r0.clone(), transfer.clone()],
        ] {
            assert_eq!(
                hashes(state.select_native_block_transactions(&candidates)),
                vec![r0.hash]
            );
        }
        assert!(
            state
                .select_native_block_transactions(&[transfer.clone()])
                .is_empty(),
            "junk alone makes an empty block, not a failed one"
        );
        assert!(state.native_transaction_still_admissible(&r0));
        assert!(
            state.native_transaction_still_admissible(&r1),
            "a nonce ahead of its account is kept for a later block"
        );
        assert!(!state.native_transaction_still_admissible(&transfer));

        state
            .execute_block_adaptive_at(std::slice::from_ref(&r0), f.validators[0].address(), 10)
            .unwrap();
        assert!(
            !state.native_transaction_still_admissible(&r0),
            "an executed request can never be admitted again, so it is not re-queued"
        );

        // Two validators each reach the threshold and submit a finalize for
        // the same request. Every node must pick the same single transaction:
        // the lowest hash among the admissible ones, in any candidate order.
        let fin_a = native_finalize(state, &f.validators[0], 0, id, certificate(&f, id));
        let fin_b = native_finalize(state, &f.validators[1], 0, id, certificate(&f, id));
        let expected = [fin_a.hash, fin_b.hash, r1.hash]
            .into_iter()
            .min_by_key(|hash| hash.0)
            .unwrap();
        for candidates in [
            vec![fin_a.clone(), fin_b.clone(), r1.clone()],
            vec![r1.clone(), fin_b.clone(), fin_a.clone()],
        ] {
            assert_eq!(
                hashes(state.select_native_block_transactions(&candidates)),
                vec![expected]
            );
        }
        state
            .execute_block_adaptive_at(std::slice::from_ref(&fin_a), f.validators[0].address(), 20)
            .unwrap();
        assert!(
            !state.native_transaction_still_admissible(&fin_b),
            "the losing finalize is dead once the request settled; it must not circulate"
        );
        assert!(state.native_transaction_still_admissible(&r1));
        assert_eq!(
            hashes(state.select_native_block_transactions(&[fin_b.clone(), r1.clone()])),
            vec![r1.hash]
        );
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_a_bounded_payable_future_transaction_is_kept_for_later() {
        let f = fixture("still-admissible");
        let state = &f.ledger.state;
        state.activate_native_inference(f.context.clone()).unwrap();
        // Nonce 1 while the account is at 0: a real future transaction.
        let next = native_request(state, &f.requester, request(&f, 1, 100));
        assert!(state.native_transaction_still_admissible(&next));
        // Far ahead of the account: never kept.
        let far = native_request(
            state,
            &f.requester,
            request(&f, NATIVE_FUTURE_NONCE_WINDOW + 1, 100),
        );
        assert!(!state.native_transaction_still_admissible(&far));
        // A key with no account at all: junk, however it is signed.
        let stranger = KeyPair::generate_ed25519();
        let mut foreign = request(&f, 1, 100).job;
        foreign.requester = stranger.address();
        let foreign = InferenceRequest::sign(foreign, &stranger).unwrap();
        let junk = native_request(state, &stranger, foreign);
        assert!(!state.native_transaction_still_admissible(&junk));
        // A request its sender cannot pay for (fixture balance is 1,000).
        let mut costly = request(&f, 1, 100).job;
        costly.reserved_max_payment = 5_000;
        let costly = InferenceRequest::sign(costly, &f.requester).unwrap();
        let unpayable = native_request(state, &f.requester, costly);
        assert!(!state.native_transaction_still_admissible(&unpayable));
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selection_examines_a_bounded_number_of_candidates_per_block() {
        let f = fixture("selection-cap");
        let state = &f.ledger.state;
        state.activate_native_inference(f.context.clone()).unwrap();
        let valid = native_request(state, &f.requester, request(&f, 0, 100));
        // Junk from fresh keys whose hashes sort before the valid request.
        let mut junk = Vec::new();
        while junk.len() < MAX_NATIVE_CANDIDATES_PER_BLOCK {
            let stranger = KeyPair::generate_ed25519();
            let mut job = request(&f, 0, 100).job;
            job.requester = stranger.address();
            let signed = InferenceRequest::sign(job, &stranger).unwrap();
            let tx = native_request(state, &stranger, signed);
            if tx.hash.0 < valid.hash.0 {
                junk.push(tx);
            }
        }
        let mut candidates = junk.clone();
        candidates.push(valid.clone());
        assert!(
            state
                .select_native_block_transactions(&candidates)
                .is_empty(),
            "the first {MAX_NATIVE_CANDIDATES_PER_BLOCK} in hash order are all junk"
        );
        // One fewer junk candidate and the valid request is reached.
        candidates.remove(0);
        assert_eq!(
            state
                .select_native_block_transactions(&candidates)
                .iter()
                .map(|t| t.hash)
                .collect::<Vec<_>>(),
            vec![valid.hash]
        );
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_ingress_preflight_admits_exactly_the_next_heights_nonce() {
        let f = fixture("admission-next");
        let state = &f.ledger.state;
        let context = f.context.clone();
        state.activate_native_inference(context.clone()).unwrap();
        let r0 = native_request(state, &f.requester, request(&f, 0, 100));
        let r1 = native_request(state, &f.requester, request(&f, 1, 100));
        state
            .validate_native_inference_transaction_admission_next(&r0, &context)
            .unwrap();
        let refused = state
            .validate_native_inference_transaction_admission_next(&r1, &context)
            .unwrap_err()
            .to_string();
        assert!(!refused.is_empty(), "the refusal must carry a reason");
        state
            .execute_block_adaptive_at(std::slice::from_ref(&r0), f.validators[0].address(), 10)
            .unwrap();
        state
            .validate_native_inference_transaction_admission_next(&r1, &context)
            .unwrap();
        assert!(
            state
                .validate_native_inference_transaction_admission_next(&r0, &context)
                .is_err()
        );
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_native_refund_consumes_caller_nonce_once() {
        let f = fixture("canonical-refund");
        let state = &f.ledger.state;
        let c = state.activate_native_inference(f.context.clone()).unwrap();
        let req = request(&f, 0, 3);
        let id = req.job.request_id();
        state
            .execute_block_verified_at(
                &[native_request(state, &f.requester, req)],
                Hash256::ZERO,
                1,
            )
            .unwrap();
        let refund = native_refund(state, &f.requester, 1, id);
        assert_rejected_unchanged(state, std::slice::from_ref(&refund));
        state
            .execute_block_verified_at(&[], Hash256::ZERO, 2)
            .unwrap();
        state
            .execute_block_verified_at(std::slice::from_ref(&refund), Hash256::ZERO, 3)
            .unwrap();
        let caller = state.get_account(&f.requester.address()).unwrap();
        assert_eq!((caller.balance, caller.nonce), (1000, 2));
        assert_eq!(
            state
                .native_inference_receipt(id, c)
                .unwrap()
                .unwrap()
                .metadata
                .status,
            InferenceTransitionStatus::Refunded
        );
        let replayed_refund = native_refund(state, &f.requester, 2, id);
        let late_finalize = native_finalize(state, &f.requester, 2, id, certificate(&f, id));
        assert_rejected_unchanged(state, std::slice::from_ref(&replayed_refund));
        assert_rejected_unchanged(state, std::slice::from_ref(&late_finalize));

        // The refund survives a restart: same root, same Refunded receipt,
        // same balance and nonce, and neither replay is admitted afterwards.
        let root = state.get_state_root();
        let dir = f.dir.clone();
        let prefunded = f.prefunded.clone();
        let genesis = f.genesis;
        let requester = f.requester.address();
        drop(f);
        let reopened = StateDB::with_genesis_persistent(&prefunded, &dir, genesis).unwrap();
        assert_eq!(reopened.get_state_root(), root);
        assert_eq!(
            reopened
                .native_inference_receipt(id, c)
                .unwrap()
                .unwrap()
                .metadata
                .status,
            InferenceTransitionStatus::Refunded
        );
        let caller = reopened.get_account(&requester).unwrap();
        assert_eq!((caller.balance, caller.nonce), (1000, 2));
        assert_rejected_unchanged(&reopened, &[replayed_refund]);
        assert_rejected_unchanged(&reopened, &[late_finalize]);
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A second store with the fixture's genesis, committee and activation,
    /// still at height 0: a node that missed everything after activation.
    fn lagging_twin(f: &Fixture, suffix: &str) -> (StateDB, std::path::PathBuf) {
        let dir = f.dir.with_extension(suffix);
        let node = StateDB::with_genesis_persistent(&f.prefunded, &dir, f.genesis).unwrap();
        node.seed_genesis_validators(
            &f.context
                .members
                .iter()
                .map(|member| (member.address, member.stake))
                .collect::<Vec<_>>(),
        );
        node.activate_native_inference(f.context.clone()).unwrap();
        (node, dir)
    }

    #[test]
    fn checkpoint_adoption_takes_only_committed_native_rows() {
        // The twin saw request A admitted (Pending) and then went down. The
        // peer finalized A and admitted B. A's escrow row therefore changed,
        // B's is new, and the twin's own copy of A's row is now stale.
        let f = fixture("adopt-native");
        let peer = &f.ledger.state;
        let c = peer.activate_native_inference(f.context.clone()).unwrap();
        let (twin, twin_dir) = lagging_twin(&f, "twin");
        let (other, other_dir) = lagging_twin(&f, "other");
        let req_a = request(&f, 0, 100);
        let id_a = req_a.job.request_id();
        let admit_a = native_request(peer, &f.requester, req_a);
        for state in [peer, &twin, &other] {
            state
                .execute_block_verified_at(std::slice::from_ref(&admit_a), Hash256::ZERO, 1)
                .unwrap();
        }
        assert_eq!(
            twin.get_block(1).unwrap().hash,
            peer.get_block(1).unwrap().hash
        );
        let stale_a = twin
            .get_storage(&escrow_address(id_a), &metadata_key())
            .unwrap();
        peer.execute_block_verified_at(
            &[native_finalize(
                peer,
                &f.validators[0],
                0,
                id_a,
                certificate(&f, id_a),
            )],
            Hash256::ZERO,
            2,
        )
        .unwrap();
        let req_b = request(&f, 1, 100);
        let id_b = req_b.job.request_id();
        peer.execute_block_verified_at(
            &[native_request(peer, &f.requester, req_b)],
            Hash256::ZERO,
            3,
        )
        .unwrap();
        let payload = peer.export_durable_snapshot();
        let tip = peer.get_block(peer.height()).unwrap();

        // Honest: A's new row and B's row are committed by their certified
        // escrow accounts; the pending index is rebuilt from them.
        let adopted = twin
            .plan_checkpoint_adoption(&payload, &tip)
            .expect("adoptable");
        assert!(adopted.full_transactions.is_empty() && adopted.receipts.is_empty());
        twin.rebase_onto_checkpoint(&adopted, &tip, Hash256::ZERO, 9)
            .unwrap();
        assert_eq!(twin.get_state_root(), peer.get_state_root());
        let pending: Vec<_> = twin
            .native_inference_pending_requests(c)
            .unwrap()
            .into_iter()
            .map(|request| request.request_id)
            .collect();
        assert_eq!(pending, vec![id_b], "A finalized, B pending");
        let live = bincode::serialize(&twin.export_durable_snapshot()).unwrap();
        drop(twin);
        let reopened =
            StateDB::with_genesis_persistent(&f.prefunded, &twin_dir, f.genesis).unwrap();
        assert_eq!(
            bincode::serialize(&reopened.export_durable_snapshot()).unwrap(),
            live,
            "a live rebase equals a restart of the same WAL"
        );

        let escrow_a = escrow_address(id_a);
        let with_rows = |rows: Vec<(Hash256, Vec<u8>)>| {
            let mut forged = payload.clone();
            for (address, held) in forged.storage.iter_mut() {
                if *address == escrow_a {
                    *held = rows.clone();
                }
            }
            forged
        };
        let fresh_a = peer.get_storage(&escrow_a, &metadata_key()).unwrap();
        // The twin's own (stale) copy of A's row: equal to "ours", NOT committed.
        let stale = with_rows(vec![(metadata_key(), stale_a.clone())]);
        assert!(
            other.plan_checkpoint_adoption(&stale, &tip).is_err(),
            "stale row"
        );
        // Two versions of the row, the stale one last.
        let duplicated = with_rows(vec![(metadata_key(), fresh_a), (metadata_key(), stale_a)]);
        assert!(
            other.plan_checkpoint_adoption(&duplicated, &tip).is_err(),
            "duplicate row"
        );
        // B's committed row left out.
        let mut omitted = payload.clone();
        omitted
            .storage
            .retain(|(address, _)| *address != escrow_address(id_b));
        assert!(
            other.plan_checkpoint_adoption(&omitted, &tip).is_err(),
            "omitted row"
        );
        // A domain nothing certifies, changed.
        let mut contracts = payload.clone();
        contracts
            .contracts
            .push((hash_bytes(b"planted"), vec![0x60]));
        assert!(
            other.plan_checkpoint_adoption(&contracts, &tip).is_err(),
            "contracts"
        );
        let mut staking = payload.clone();
        staking.staking_pool += 1;
        assert!(
            other.plan_checkpoint_adoption(&staking, &tip).is_err(),
            "staking pool"
        );
        // Rows nothing commits must equal this node's: the activation record.
        let context_holder = context_account();
        let mut activation = payload.clone();
        for (address, rows) in activation.storage.iter_mut() {
            if *address == context_holder {
                for (key, value) in rows.iter_mut() {
                    if *key == activation_key() {
                        value.push(0);
                    }
                }
            }
        }
        assert!(
            other.plan_checkpoint_adoption(&activation, &tip).is_err(),
            "activation row"
        );
        // The pin row is committed by the context account: a changed value is not.
        let mut pin = payload.clone();
        for (address, rows) in pin.storage.iter_mut() {
            if *address == context_holder {
                for (key, value) in rows.iter_mut() {
                    if *key == context_key() {
                        value[0] ^= 1;
                    }
                }
            }
        }
        assert!(
            other.plan_checkpoint_adoption(&pin, &tip).is_err(),
            "pin row"
        );
        // Entries below the minimum stake depend on when a node last
        // restarted (activation records only members; a restart re-seeds the
        // genesis list), so they are not compared. A member's stake is.
        let mut below_minimum = payload.clone();
        below_minimum
            .validators
            .push((hash_bytes(b"below-minimum"), 1));
        below_minimum
            .validators
            .sort_by_key(|(address, _)| address.0);
        assert!(
            other.plan_checkpoint_adoption(&below_minimum, &tip).is_ok(),
            "below-minimum registry entries"
        );
        let mut member_stake = payload.clone();
        member_stake.validators[0].1 += 1;
        assert!(
            other.plan_checkpoint_adoption(&member_stake, &tip).is_err(),
            "member stake"
        );

        // An account filed under another key between the same neighbours.
        // The account-only root hashes contents in key order, not keys, so
        // the root still verifies - which is why the key is checked.
        let wal_before = std::fs::read(other_dir.join("state.wal")).unwrap();
        let requester = f.requester.address();
        let relabel = |accounts: &mut Vec<(arc_types::Address, arc_types::Account)>| {
            let index = accounts
                .iter()
                .position(|(address, _)| *address == requester)
                .unwrap();
            let mut moved = accounts[index].0;
            for byte in moved.0.iter_mut().rev() {
                let (next, carry) = byte.overflowing_add(1);
                *byte = next;
                if !carry {
                    break;
                }
            }
            if let Some((after, _)) = accounts.get(index + 1) {
                assert!(moved.0 < after.0, "the relabelled key keeps the order");
            }
            accounts[index].0 = moved;
        };
        let mut relabelled = payload.clone();
        relabel(&mut relabelled.accounts);
        let scratch = StateDB::new();
        scratch.install_durable_snapshot(&relabelled);
        assert_eq!(
            scratch.get_state_root(),
            tip.header.state_root,
            "the root alone does not see it"
        );
        assert!(
            other.plan_checkpoint_adoption(&relabelled, &tip).is_err(),
            "relabelled account"
        );
        let mut planned = other
            .plan_checkpoint_adoption(&payload, &tip)
            .expect("honest plan");
        relabel(&mut planned.accounts);
        assert!(
            other
                .rebase_onto_checkpoint(&planned, &tip, Hash256::ZERO, 9)
                .is_err(),
            "a relabelled account is refused again before the write"
        );
        // An escrow moved to another address with its rows: the planned
        // shape is fine, but the state no longer reproduces the certified
        // root, so the scratch install refuses it before the write.
        let mut moved = other
            .plan_checkpoint_adoption(&payload, &tip)
            .expect("honest plan");
        let elsewhere = hash_bytes(b"moved escrow");
        for (address, account) in moved.accounts.iter_mut() {
            if *address == escrow_a {
                *address = elsewhere;
                account.address = elsewhere;
            }
        }
        for (address, _) in moved.storage.iter_mut() {
            if *address == escrow_a {
                *address = elsewhere;
            }
        }
        moved.canonicalize();
        assert!(
            other
                .rebase_onto_checkpoint(&moved, &tip, Hash256::ZERO, 9)
                .is_err(),
            "moved escrow"
        );
        assert_eq!(
            std::fs::read(other_dir.join("state.wal")).unwrap(),
            wal_before,
            "nothing refused reached the WAL"
        );
        assert_eq!(other.height(), 1);

        let dir = f.dir.clone();
        drop(f);
        drop(reopened);
        drop(other);
        for path in [dir, twin_dir, other_dir] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn already_used_nonces_do_not_consume_the_candidate_cap() {
        // A node that rebased onto a checkpoint holds no receipts below it,
        // so the commit path hands it already-applied transactions its peers
        // filtered out. They must not push a live candidate out of the
        // examined window, or that node would select differently.
        let f = fixture("selection-stale");
        let state = &f.ledger.state;
        state.activate_native_inference(f.context.clone()).unwrap();
        state
            .execute_block_verified_at(
                &[native_request(state, &f.requester, request(&f, 0, 100))],
                Hash256::ZERO,
                1,
            )
            .unwrap();
        // The live candidate (nonce 1) with the largest hash of a few tries,
        // so plenty of stale ones sort before it.
        let valid = (200..208)
            .map(|expires| native_request(state, &f.requester, request(&f, 1, expires)))
            .max_by_key(|tx| tx.hash.0)
            .unwrap();
        let mut stale = Vec::new();
        let mut expires = 1_000;
        while stale.len() < MAX_NATIVE_CANDIDATES_PER_BLOCK + 1 {
            let tx = native_request(state, &f.requester, request(&f, 0, expires));
            expires += 1;
            if tx.hash.0 < valid.hash.0 {
                stale.push(tx);
            }
        }
        let mut candidates = stale;
        candidates.push(valid.clone());
        assert_eq!(
            state
                .select_native_block_transactions(&candidates)
                .iter()
                .map(|tx| tx.hash)
                .collect::<Vec<_>>(),
            vec![valid.hash],
            "stale nonces are skipped without counting toward the cap"
        );
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_rule_v1_chain_keeps_counting_used_nonces() {
        // The same candidates as above on a chain activated with rule v1:
        // every examined candidate counts, so the stale ones fill the cap and
        // nothing is selected - exactly what binaries without D20 do beside it.
        let mut f = fixture("selection-v1");
        f.context.selection_rule = NativeSelectionRule::CountEveryCandidateV1;
        let state = &f.ledger.state;
        state.activate_native_inference(f.context.clone()).unwrap();
        state
            .execute_block_verified_at(
                &[native_request(state, &f.requester, request(&f, 0, 100))],
                Hash256::ZERO,
                1,
            )
            .unwrap();
        let valid = (200..208)
            .map(|expires| native_request(state, &f.requester, request(&f, 1, expires)))
            .max_by_key(|tx| tx.hash.0)
            .unwrap();
        let mut candidates = Vec::new();
        let mut expires = 1_000;
        while candidates.len() < MAX_NATIVE_CANDIDATES_PER_BLOCK {
            let tx = native_request(state, &f.requester, request(&f, 0, expires));
            expires += 1;
            if tx.hash.0 < valid.hash.0 {
                candidates.push(tx);
            }
        }
        candidates.push(valid);
        assert!(
            state
                .select_native_block_transactions(&candidates)
                .is_empty()
        );
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_rule_v1_chain_never_adopts_a_checkpoint() {
        let mut f = fixture("adopt-v1");
        f.context.selection_rule = NativeSelectionRule::CountEveryCandidateV1;
        let peer = &f.ledger.state;
        peer.activate_native_inference(f.context.clone()).unwrap();
        let (twin, twin_dir) = lagging_twin(&f, "twin-v1");
        peer.execute_block_verified_at(
            &[native_request(peer, &f.requester, request(&f, 0, 100))],
            Hash256::ZERO,
            1,
        )
        .unwrap();
        let payload = peer.export_durable_snapshot();
        let tip = peer.get_block(1).unwrap();
        let refusal = twin.plan_checkpoint_adoption(&payload, &tip).unwrap_err();
        assert!(
            refusal.to_string().contains("selection rule v1"),
            "{refusal}"
        );
        let dir = f.dir.clone();
        drop(f);
        drop(twin);
        for path in [dir, twin_dir] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn each_selection_rule_has_one_activation_layout_and_its_own_commitment() {
        let f = fixture("activation-layouts");
        let mut v1 = f.context.clone();
        v1.selection_rule = NativeSelectionRule::CountEveryCandidateV1;
        let v2 = f.context.clone();
        assert_eq!(v2.selection_rule, NativeSelectionRule::SkipUsedNoncesV2);

        // Rule v1 is the original record, byte for byte, and its original
        // commitment: an upgraded binary reads and writes exactly what the
        // binaries beside it do.
        let legacy = bincode::serialize(&LegacyAdmissionContext {
            domain: v1.domain,
            members: v1.members.clone(),
            allowed_executions: v1.allowed_executions.clone(),
        })
        .unwrap();
        assert_eq!(encode_activation(&v1).unwrap(), legacy);
        assert_eq!(decode_activation(&legacy).unwrap(), v1);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&v1.domain.chain_genesis.0);
        bytes.extend_from_slice(&v1.domain.recovery_epoch.to_le_bytes());
        bytes.extend_from_slice(&v1.domain.validator_set_hash.0);
        bytes.extend_from_slice(&(v1.members.len() as u32).to_le_bytes());
        for member in &v1.members {
            bytes.extend_from_slice(&member.address.0);
            bytes.extend_from_slice(&member.stake.to_le_bytes());
        }
        let mut sorted = v1.allowed_executions.clone();
        sorted.sort_by_key(|e| {
            (
                e.model_hash.0,
                e.profile_hash.0,
                e.generation_hash.0,
                e.assignment_hash.0,
            )
        });
        sorted.dedup();
        bytes.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
        for e in &sorted {
            for hash in [
                e.model_hash,
                e.profile_hash,
                e.generation_hash,
                e.assignment_hash,
            ] {
                bytes.extend_from_slice(&hash.0);
            }
        }
        assert_eq!(
            v1.commitment().unwrap(),
            domain_hash(CONTEXT_DOMAIN, &bytes)
        );

        // Rule v2 is a different record and commitment. A binary without D20
        // decodes records exactly, so it refuses this one instead of running
        // the chain under the old rule.
        let extended = encode_activation(&v2).unwrap();
        assert_ne!(extended, legacy);
        assert_eq!(decode_activation(&extended).unwrap(), v2);
        assert!(
            bincode::deserialize_limited_exact::<LegacyAdmissionContext, MAX_CONTEXT_BYTES>(
                &extended
            )
            .is_err()
        );
        assert_ne!(v1.commitment().unwrap(), v2.commitment().unwrap());
        // One encoding per context: rule v1 in the extended layout is refused.
        assert!(decode_activation(&bincode::serialize(&v1).unwrap()).is_err());

        let dir = f.dir.clone();
        drop(f);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_storage_row_is_committed_only_through_its_certified_account() {
        let value = b"metadata".to_vec();
        let mut account = arc_types::Account::new(hash_bytes(b"escrow"), 0);
        account.storage_root = hash_bytes(&value);
        let escrow = hash_bytes(b"escrow");
        assert!(native_storage_row_committed(
            Some(&account),
            &escrow,
            &metadata_key(),
            &value
        ));
        assert!(!native_storage_row_committed(
            Some(&account),
            &escrow,
            &metadata_key(),
            b"other"
        ));
        assert!(!native_storage_row_committed(
            None,
            &escrow,
            &metadata_key(),
            &value
        ));
        assert!(!native_storage_row_committed(
            Some(&account),
            &escrow,
            &hash_bytes(b"k"),
            &value
        ));
        let mut pin = arc_types::Account::new(context_account(), 0);
        pin.storage_root = hash_bytes(b"commitment");
        assert!(native_storage_row_committed(
            Some(&pin),
            &context_account(),
            &context_key(),
            pin.storage_root.as_ref()
        ));
        assert!(!native_storage_row_committed(
            Some(&pin),
            &escrow,
            &context_key(),
            pin.storage_root.as_ref()
        ));
    }

    /// Advance a recovery-bound fixture to `height` with empty blocks, the way
    /// the recovered public chain advances today.
    fn recovered_chain_at(f: &Fixture, height: u64) -> &StateDB {
        let state = &f.ledger.state;
        *state.recovery_context.write() = Some(crate::recovery::RecoveryContext::new(
            "test", f.genesis, 1, 0,
        ));
        while state.height() < height {
            state
                .execute_block_adaptive_at(
                    &[],
                    f.validators[0].address(),
                    1_700_000 + state.height(),
                )
                .expect("an empty block advances a recovered chain");
        }
        state
    }

    fn migration_for(f: &Fixture, activation_height: u64) -> NativeMigrationRecord {
        NativeMigrationRecord {
            chain_genesis: f.genesis,
            recovery_epoch: 1,
            validator_set_id: 0,
            activation_height,
            context_commitment: f.context.commitment().unwrap(),
        }
    }

    /// The product requirement: paid inference activates on the EXISTING
    /// chain, preserving its history, rather than only on a fresh genesis.
    #[test]
    fn a_migration_record_activates_native_inference_on_a_running_recovered_chain() {
        let f = fixture("migration-activates");
        let state = recovered_chain_at(&f, 4);
        let balance_before = state.get_account(&f.requester.address()).unwrap().balance;

        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .expect("a record for this chain, ahead of its height, is accepted");
        // Not yet: the coordinated height has not arrived.
        assert!(state.activate_native_inference(f.context.clone()).is_err());

        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        assert_eq!(state.height(), 5);
        let commitment = state
            .activate_native_inference(f.context.clone())
            .expect("at the coordinated height the migration activates");
        assert_eq!(commitment, f.context.commitment().unwrap());
        assert_eq!(state.native_inference_context(), Some(f.context.clone()));
        // The chain it migrated is still the chain it was.
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            balance_before,
            "migration must not move balances"
        );
        assert_eq!(state.height(), 5, "migration is not a block of its own");
    }

    #[test]
    fn a_migration_record_for_another_chain_or_epoch_is_refused() {
        let f = fixture("migration-wrong-chain");
        let state = recovered_chain_at(&f, 4);

        let mut other_chain = migration_for(&f, 5);
        other_chain.chain_genesis = hash_bytes(b"a different chain");
        let error = state
            .authorize_native_migration(other_chain, f.context.clone())
            .expect_err("a record naming another genesis must not authorise this chain");
        assert!(format!("{error}").contains("genesis"), "{error}");

        let mut other_epoch = migration_for(&f, 5);
        other_epoch.recovery_epoch = 2;
        let error = state
            .authorize_native_migration(other_epoch, f.context.clone())
            .expect_err("a record from another recovery epoch must be refused");
        assert!(format!("{error}").contains("recovery epoch"), "{error}");

        let mut other_set = migration_for(&f, 5);
        other_set.validator_set_id = 7;
        assert!(
            state
                .authorize_native_migration(other_set, f.context.clone())
                .is_err()
        );

        // None of the refused records left anything behind.
        assert!(state.native_migration().is_none());
        assert!(state.activate_native_inference(f.context.clone()).is_err());
    }

    /// A record aimed at a height this chain already passed without
    /// activating would be a retroactive state change: every other validator
    /// would have a different root.
    #[test]
    fn a_migration_record_cannot_activate_retroactively() {
        let f = fixture("migration-retroactive");
        let state = recovered_chain_at(&f, 10);
        let error = state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .expect_err("a passed height must not be authorised");
        assert!(format!("{error}").contains("retroactive"), "{error}");
        assert!(state.native_migration().is_none());
    }

    #[test]
    fn a_migration_record_must_name_the_binding_being_activated() {
        let f = fixture("migration-binding");
        let state = recovered_chain_at(&f, 4);
        let mut wrong = migration_for(&f, 5);
        wrong.context_commitment = hash_bytes(b"some other binding");
        state
            .authorize_native_migration(wrong, f.context.clone())
            .unwrap();
        // Reaching the coordinated height with a record that names another
        // binding stops this node rather than activating the wrong one or
        // carrying on out of step with the validators that did activate.
        let error = state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .expect_err("the record must authorise this exact binding");
        assert!(
            format!("{error}").contains("different inference binding"),
            "{error}"
        );
        assert!(state.native_inference_context().is_none());
    }

    /// Activation happens at ONE height, not "at or after" one, and the chain
    /// performs it itself when it gets there. Leaving it to whoever happens
    /// to be starting up would activate one node and strand the rest.
    #[test]
    fn the_chain_activates_itself_at_the_coordinated_height_and_nowhere_else() {
        let f = fixture("migration-height");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 6), f.context.clone())
            .unwrap();

        // One block early: nothing has happened, and asking directly is
        // refused with the height it is waiting for.
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_098)
            .unwrap();
        assert_eq!(state.height(), 5);
        assert!(state.native_inference_context().is_none());
        let error = state
            .activate_native_inference(f.context.clone())
            .expect_err("early activation is refused");
        assert!(
            format!("{error}").contains("activates at height 6"),
            "{error}"
        );

        // The block that reaches height 6 activates the binding, without
        // anyone calling activation at all.
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        assert_eq!(state.height(), 6);
        assert_eq!(
            state.native_inference_context(),
            Some(f.context.clone()),
            "the chain activates itself at the coordinated height"
        );

        // And later blocks neither repeat it nor change it. An empty block
        // moves the height without touching the root, which is exactly how
        // the recovered chain has been advancing.
        let root = state.compute_state_root();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_100)
            .unwrap();
        assert_eq!(state.height(), 7, "the chain kept moving");
        assert_eq!(state.native_inference_context(), Some(f.context.clone()));
        assert_eq!(state.compute_state_root(), root, "and activated once");
    }

    /// The original rule is untouched where no operator authorised anything.
    #[test]
    fn without_a_migration_record_a_recovered_chain_still_refuses_activation() {
        let f = fixture("migration-absent");
        let state = recovered_chain_at(&f, 3);
        let error = state
            .activate_native_inference(f.context.clone())
            .expect_err("recovery-bound state refuses activation by default");
        assert!(
            format!("{error}").contains("recovery-bound state"),
            "{error}"
        );
    }

    /// A migrated chain has to come back after a restart. The operator's
    /// record is an input to the ONE activation; a restarting node is not
    /// given it again and rebuilds the binding from rooted storage alone.
    #[test]
    fn a_migrated_chain_rebuilds_its_binding_the_way_a_restart_does() {
        let f = fixture("migration-restart");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        state.activate_native_inference(f.context.clone()).unwrap();
        let root = state.compute_state_root();
        let balance = state.get_account(&f.requester.address()).unwrap().balance;

        // Exactly what reopening the state directory does: the in-memory
        // caches start empty and the binding is restored from storage.
        forget_cached_binding(state);
        state
            .restore_native_inference_context()
            .expect("a migrated chain must rebuild its binding from its own rooted state");
        assert_eq!(state.native_inference_context(), Some(f.context.clone()));
        assert_eq!(state.compute_state_root(), root, "restart changes no root");
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            balance
        );
    }

    /// Every validator must derive the SAME migrated state root. Two nodes
    /// of the same chain, advanced identically and migrated by the same
    /// record, are the whole point of naming a coordinated height.
    #[test]
    fn two_validators_migrating_the_same_chain_derive_the_same_root() {
        let f = fixture("migration-agreement");
        let members: Vec<_> = f
            .context
            .members
            .iter()
            .map(|member| (member.address, member.stake))
            .collect();
        let first = recovered_chain_at(&f, 4);
        first
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        first
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        let commitment = first.activate_native_inference(f.context.clone()).unwrap();

        // A second node of the same chain, from the same genesis.
        let dir = f.dir.with_extension("second");
        let second = StateDB::with_genesis_persistent(&f.prefunded, &dir, f.genesis).unwrap();
        second.seed_genesis_validators(&members);
        *second.recovery_context.write() = Some(crate::recovery::RecoveryContext::new(
            "test", f.genesis, 1, 0,
        ));
        while second.height() < 4 {
            second
                .execute_block_adaptive_at(
                    &[],
                    f.validators[0].address(),
                    1_700_000 + second.height(),
                )
                .unwrap();
        }
        second
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        second
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        let other = second.activate_native_inference(f.context.clone()).unwrap();

        assert_eq!(commitment, other, "the same binding");
        assert_eq!(
            first.compute_state_root(),
            second.compute_state_root(),
            "two validators migrating the same chain must agree"
        );
        assert_eq!(first.height(), second.height());
    }

    /// Repeating the migration must change nothing: the same record, the same
    /// binding, the same root. An operator retrying after a timeout, or a
    /// restarted node re-reading its config, must not write twice.
    #[test]
    fn repeating_a_migration_changes_nothing() {
        let f = fixture("migration-repeat");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        let commitment = state.activate_native_inference(f.context.clone()).unwrap();
        let root = state.compute_state_root();

        // Again, with the record still authorised - the idempotent path.
        assert_eq!(
            state.activate_native_inference(f.context.clone()).unwrap(),
            commitment
        );
        assert_eq!(state.compute_state_root(), root, "a repeat writes nothing");
        // And again after re-authorising the same record, as a restarted
        // operator process would.
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .expect("re-authorising an already applied migration is accepted");
        assert_eq!(
            state.activate_native_inference(f.context.clone()).unwrap(),
            commitment
        );
        assert_eq!(state.compute_state_root(), root);
    }

    /// A refused migration must leave NO trace. Fail-closed means the chain
    /// is exactly what it was, not a half-written context account.
    #[test]
    fn a_refused_migration_publishes_no_partial_state() {
        let f = fixture("migration-refused");
        let state = recovered_chain_at(&f, 4);
        let before = state.compute_state_root();
        state
            .authorize_native_migration(migration_for(&f, 9), f.context.clone())
            .unwrap();
        // Height 4, coordinated height 9: refused.
        assert!(state.activate_native_inference(f.context.clone()).is_err());
        assert!(
            state.get_account(&context_account()).is_none(),
            "no context account"
        );
        assert!(
            state
                .get_storage(&context_account(), &activation_key())
                .is_none(),
            "no activation row"
        );
        assert!(
            state
                .get_storage(&context_account(), &migration_key())
                .is_none(),
            "no migration row"
        );
        assert_eq!(state.compute_state_root(), before, "the chain is unchanged");
    }

    /// The rooted record is evidence, so it is checked like evidence. A row
    /// that decodes to another chain's authorisation, or does not decode at
    /// all, must stop the node rather than silently restore a binding.
    #[test]
    fn a_tampered_migration_row_is_refused_on_restore() {
        let f = fixture("migration-tampered");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        state.activate_native_inference(f.context.clone()).unwrap();

        // A record for some other chain.
        let mut forged = migration_for(&f, 5);
        forged.chain_genesis = hash_bytes(b"a different chain");
        state.apply_wal_op(&WalOp::SetStorage(
            context_account(),
            migration_key(),
            bincode::serialize(&forged).unwrap(),
        ));
        forget_cached_binding(state);
        let error = state
            .restore_native_inference_context()
            .expect_err("a record naming another chain must not restore this binding");
        assert!(format!("{error}").contains("genesis"), "{error}");

        // And a row that is not a record at all.
        state.apply_wal_op(&WalOp::SetStorage(
            context_account(),
            migration_key(),
            b"not a migration record".to_vec(),
        ));
        forget_cached_binding(state);
        assert!(state.restore_native_inference_context().is_err());
    }

    /// What a restart leaves behind: nothing in memory, everything in state.
    fn forget_cached_binding(state: &StateDB) {
        *state.native_inference_context.write() = None;
        *state.native_migration.write() = None;
    }

    /// Activation must not silently end the chain's existing economy. The
    /// fresh-genesis rule admits one native transaction and nothing else,
    /// which on a private chain removes nothing. On the public chain it would
    /// remove every transfer, contract call and reward the community already
    /// has. A migrated chain keeps them; only the committee is frozen.
    #[test]
    fn a_migrated_chain_still_runs_its_existing_transaction_families() {
        let f = fixture("migration-families");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        state.activate_native_inference(f.context.clone()).unwrap();

        let transfer = outer_with_fee(
            state,
            &f.requester,
            0,
            1,
            arc_types::TxBody::Transfer(arc_types::transaction::TransferBody {
                to: f.validators[0].address(),
                amount: 1,
                amount_commitment: None,
            }),
        );
        state
            .validate_native_inference_block_admission(std::slice::from_ref(&transfer))
            .expect("a transfer survives the migration");

        // A native transaction still gets a block of its own.
        let req = native_request(state, &f.requester, request(&f, 0, 100));
        state
            .validate_native_inference_block_admission(std::slice::from_ref(&req))
            .expect("a native request is still admissible");
        // But never mixed with anything else.
        assert!(
            state
                .validate_native_inference_block_admission(&[req, transfer.clone()])
                .is_err(),
            "a block is either ordinary or native, never both"
        );

        // Staking and validator admission survive activation too. The
        // committee is not frozen for the chain's life: versioned bindings
        // let it move, with new work refused until the operator publishes
        // the binding naming the new committee and work already admitted
        // settling under the one that accepted it.
        let stake = outer(
            state,
            &f.requester,
            0,
            arc_types::TxBody::Stake(arc_types::transaction::StakeBody {
                amount: 1,
                is_stake: true,
                validator: f.validators[0].address(),
            }),
        );
        state
            .validate_native_inference_block_admission(std::slice::from_ref(&stake))
            .expect("a migrated chain still admits staking");
        state
            .refuse_registry_change_under_native_binding()
            .expect("and the executor does not refuse it either");

        // And it EXECUTES, which admission alone does not show: an ordinary
        // block on a migrated chain must take the ordinary execution path,
        // not the native one that every block used to take.
        let before = state.get_account(&f.requester.address()).unwrap().balance;
        let (block, receipts) = state
            .execute_block_adaptive_at(
                std::slice::from_ref(&transfer),
                f.validators[0].address(),
                1_700_400,
            )
            .expect("an ordinary block executes on a migrated chain");
        assert!(receipts[0].success, "the transfer succeeded");
        assert_eq!(block.header.protocol_version.major, 3);
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            before - 2,
            "the transfer moved value and paid its fee"
        );

        // And the chain still reports the protocol it actually is. Reporting
        // 4 here would skip the v3 admission path that validates every one of
        // those families - the transfer above would reach a block unchecked.
        assert_eq!(
            state.active_protocol_version().major,
            3,
            "a migrated chain keeps its recovery protocol and its whole admission path"
        );
    }

    /// The private protocol-4 rule is unchanged by any of the above: without
    /// a migration record, a chain admits native transactions and nothing
    /// else, exactly as it did before this work.
    #[test]
    fn a_fresh_protocol_4_genesis_still_admits_only_native_transactions() {
        let f = fixture("protocol4-unchanged");
        let state = &f.ledger.state;
        state.activate_native_inference(f.context.clone()).unwrap();
        assert!(
            state.native_migration().is_none(),
            "this chain activated at genesis, with no migration"
        );
        let transfer = outer(
            state,
            &f.requester,
            0,
            arc_types::TxBody::Transfer(arc_types::transaction::TransferBody {
                to: f.validators[0].address(),
                amount: 1,
                amount_commitment: None,
            }),
        );
        let error = state
            .validate_native_inference_block_admission(std::slice::from_ref(&transfer))
            .expect_err("a private protocol-4 chain admits only native transactions");
        assert!(format!("{error}").contains("at most one"), "{error}");
        assert_eq!(
            state.active_protocol_version().major,
            4,
            "a fresh private genesis IS protocol 4"
        );
    }

    /// The product requirement end to end: a real paid request reserved,
    /// certified, settled and receipted on the EXISTING recovered chain, with
    /// that chain's own history and balances still underneath it. This is
    /// what "migrate the public chain" has to mean to be worth doing.
    #[test]
    fn a_paid_request_settles_on_a_migrated_chain() {
        let f = fixture("migration-settles");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        let commitment = state.activate_native_inference(f.context.clone()).unwrap();
        let history_before = state.height();

        let req = request(&f, 0, 400);
        let id = req.job.request_id();
        let tx = native_request(state, &f.requester, req);
        state.validate_v3_transaction_admission(&tx).unwrap();
        state
            .validate_v3_block_admission(std::slice::from_ref(&tx))
            .unwrap();
        let (block, receipts) = state
            .execute_block_adaptive_at(&[tx], f.validators[0].address(), 1_700_200)
            .unwrap();
        assert!(receipts[0].success, "the request is admitted and reserved");
        assert_eq!(
            block.header.protocol_version,
            state.active_protocol_version()
        );
        assert_eq!(block.header.protocol_version.major, 3);
        assert_eq!(block.header.state_root, state.compute_state_root());
        // Recovery commitments cover native storage itself, not just balances.
        let slot = state
            .get_storage(&escrow_address(id), &metadata_key())
            .unwrap();
        state
            .storage
            .get(&escrow_address(id).0)
            .unwrap()
            .insert(metadata_key(), vec![0]);
        assert_ne!(block.header.state_root, state.compute_state_root());
        state
            .storage
            .get(&escrow_address(id).0)
            .unwrap()
            .insert(metadata_key(), slot);
        assert_eq!(block.header.state_root, state.compute_state_root());
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            900
        );
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 100);
        assert_eq!(
            state
                .native_inference_pending_requests(commitment)
                .unwrap()
                .len(),
            1
        );

        let terminal = native_finalize(state, &f.validators[0], 0, id, certificate(&f, id));
        state.validate_v3_transaction_admission(&terminal).unwrap();
        state
            .validate_v3_block_admission(std::slice::from_ref(&terminal))
            .unwrap();
        let (block, receipts) = state
            .execute_block_adaptive_at(&[terminal], f.validators[0].address(), 1_700_300)
            .unwrap();
        assert!(receipts[0].success, "the certificate settles");
        assert_eq!(block.header.protocol_version.major, 3);
        assert_eq!(block.header.state_root, state.compute_state_root());
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            990,
            "the unspent reservation comes back"
        );
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 0);
        let paid: u64 = f
            .validators
            .iter()
            .map(|v| state.get_account(&v.address()).unwrap().balance)
            .sum();
        assert_eq!(paid, 10, "the workers are paid exactly the execution price");

        // Still the chain it was: recovered, continuing, history intact.
        assert!(state.recovery_context().is_some());
        assert!(state.height() > history_before);
        assert_eq!(state.native_inference_context(), Some(f.context.clone()));
    }

    /// WAL continuity across a mid-chain activation. The activation appends
    /// its own ops and checkpoint at the coordinated height, AFTER that
    /// block's own ops and checkpoint. Reopening the directory replays the
    /// whole log through the real validation - genesis prefix, block and
    /// checkpoint pairing, chain linkage - so if a second checkpoint at one
    /// height were not accepted, a migrated node could never start again.
    #[test]
    fn a_mid_chain_activation_replays_from_its_own_wal() {
        let f = fixture("migration-wal");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        assert_eq!(state.native_inference_context(), Some(f.context.clone()));
        let height = state.height();
        let balance = state.get_account(&f.requester.address()).unwrap().balance;
        let commitment = f.context.commitment().unwrap();

        let dir = f.dir.clone();
        let prefunded = f.prefunded.clone();
        let genesis = f.genesis;
        let requester = f.requester.address();
        let context = f.context.clone();
        drop(f);

        let reopened = StateDB::with_genesis_persistent(&prefunded, &dir, genesis)
            .expect("a migrated chain's own WAL must replay");
        assert_eq!(reopened.height(), height, "same tip");
        assert_eq!(
            reopened.native_inference_context(),
            Some(context),
            "the binding is rebuilt from the replayed activation"
        );
        assert_eq!(
            reopened
                .get_storage(&context_account(), &context_key())
                .as_deref(),
            Some(commitment.0.as_slice()),
            "the rooted context row survived replay"
        );
        assert!(
            reopened
                .get_storage(&context_account(), &migration_key())
                .is_some(),
            "and so did the record that authorised it"
        );
        assert_eq!(
            reopened.get_account(&requester).unwrap().balance,
            balance,
            "balances are what they were"
        );
    }

    /// A configuration update governs NEW requests. Work already admitted
    /// keeps the binding it was accepted under, all the way through
    /// settlement - so upgrading the allowed model does not strand or
    /// silently re-price anything already in flight.
    #[test]
    fn a_binding_update_governs_new_work_while_pending_work_keeps_its_own() {
        let f = fixture("binding-update-executions");
        let state = &f.ledger.state;
        let v1 = state.activate_native_inference(f.context.clone()).unwrap();

        // Admitted under v1.
        let req = request(&f, 0, 400);
        let id = req.job.request_id();
        let tx = native_request(state, &f.requester, req);
        let (_, receipts) = state
            .execute_block_adaptive_at(&[tx], f.validators[0].address(), 10)
            .unwrap();
        assert!(receipts[0].success);
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 100);

        // The operator publishes a new binding: one more allowed execution.
        let upgraded = hash_bytes(b"a newer model");
        let mut next = f.context.clone();
        next.allowed_executions.push(AllowedExecution {
            model_hash: upgraded,
            profile_hash: upgraded,
            generation_hash: upgraded,
            assignment_hash: upgraded,
        });
        let v2 = state
            .update_native_inference_binding(next.clone())
            .expect("a running binding can be updated");
        assert_ne!(v1, v2, "a new configuration is a new binding");
        assert_eq!(state.native_inference_context(), Some(next));
        assert_eq!(
            state.retained_binding_index().unwrap(),
            vec![v1],
            "v1 is retained because pending work still names it"
        );

        // The v1 request settles on v1's terms, after the update.
        let terminal = native_finalize(state, &f.validators[0], 0, id, certificate(&f, id));
        let (_, receipts) = state
            .execute_block_adaptive_at(&[terminal], f.validators[0].address(), 20)
            .unwrap();
        assert!(
            receipts[0].success,
            "pending work settles under its own binding"
        );
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 0);
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            990
        );
        let paid: u64 = f
            .validators
            .iter()
            .map(|v| state.get_account(&v.address()).unwrap().balance)
            .sum();
        assert_eq!(
            paid, 10,
            "paid exactly once, at the price it was admitted at"
        );
    }

    /// The committee can change under a live binding. Staking and validator
    /// admission are not suspended by activating paid inference: the registry
    /// moves, the operator publishes the matching binding, new requests bind
    /// the new committee, and work already admitted is still settled by the
    /// committee that accepted it.
    #[test]
    fn a_committee_change_does_not_strand_work_admitted_before_it() {
        let f = fixture("binding-update-committee");
        let state = &f.ledger.state;
        state.activate_native_inference(f.context.clone()).unwrap();

        let req = request(&f, 0, 400);
        let id = req.job.request_id();
        let tx = native_request(state, &f.requester, req);
        state
            .execute_block_adaptive_at(&[tx], f.validators[0].address(), 10)
            .unwrap();
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 100);

        // A seventh validator joins the registry.
        let joiner = KeyPair::generate_ed25519();
        let mut members = f.context.members.clone();
        members.push(ValidatorMember::new(
            joiner.address(),
            StateDB::MIN_VALIDATOR_STAKE,
        ));
        members.sort_by_key(|member| member.address.0);
        state.apply_wal_op(&WalOp::SetValidatorState(
            members
                .iter()
                .map(|member| (member.address, member.stake))
                .collect(),
            0,
        ));

        // Until the operator publishes the matching binding, NEW work is
        // refused - fail-closed - and nothing is silently accepted against a
        // committee the binding does not name.
        let early = native_request(state, &f.requester, request(&f, 1, 400));
        assert!(
            state
                .validate_native_inference_block_admission(std::slice::from_ref(&early))
                .is_err(),
            "new work waits for the binding to catch up with the registry"
        );

        let mut next = f.context.clone();
        next.domain.validator_set_hash = validator_set_commitment(&members).unwrap();
        next.members = members;
        state
            .update_native_inference_binding(next.clone())
            .expect("the operator publishes the binding for the new committee");
        assert_eq!(state.native_inference_context(), Some(next));

        // The request admitted before the change still settles, certified by
        // the committee that admitted it.
        let terminal = native_finalize(state, &f.validators[0], 0, id, certificate(&f, id));
        let (_, receipts) = state
            .execute_block_adaptive_at(&[terminal], f.validators[0].address(), 20)
            .unwrap();
        assert!(
            receipts[0].success,
            "the old committee settles the work it accepted"
        );
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 0);
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            990
        );
        // Nothing still names the old binding, so it is not retained forever.
        assert!(
            state
                .native_inference_pending_requests(
                    state
                        .native_inference_context()
                        .unwrap()
                        .commitment()
                        .unwrap()
                )
                .is_ok()
        );
    }

    /// A restart in the middle of a configuration change must not strand the
    /// work in flight. The superseded binding is rooted in state and comes
    /// back with it.
    #[test]
    fn a_retained_binding_survives_a_restart_and_is_retired_when_nothing_names_it() {
        let f = fixture("binding-retained-restart");
        let state = &f.ledger.state;
        let v1 = state.activate_native_inference(f.context.clone()).unwrap();
        let req = request(&f, 0, 400);
        let id = req.job.request_id();
        let tx = native_request(state, &f.requester, req);
        state
            .execute_block_adaptive_at(&[tx], f.validators[0].address(), 10)
            .unwrap();

        let upgraded = hash_bytes(b"a newer model");
        let mut next = f.context.clone();
        next.allowed_executions.push(AllowedExecution {
            model_hash: upgraded,
            profile_hash: upgraded,
            generation_hash: upgraded,
            assignment_hash: upgraded,
        });
        state.update_native_inference_binding(next.clone()).unwrap();
        assert_eq!(state.retained_binding_index().unwrap(), vec![v1]);

        // Restart, holding nothing in memory. The settling transaction is
        // signed before the restart, as a validator's would have been.
        let terminal = native_finalize(state, &f.validators[0], 0, id, certificate(&f, id));
        let producer = f.validators[0].address();
        let dir = f.dir.clone();
        let prefunded = f.prefunded.clone();
        let genesis = f.genesis;
        let requester = f.requester.address();
        drop(f);

        let reopened = StateDB::with_genesis_persistent(&prefunded, &dir, genesis)
            .expect("a chain mid-update must reopen");
        assert_eq!(reopened.native_inference_context(), Some(next));
        assert_eq!(
            reopened.retained_binding_index().unwrap(),
            vec![v1],
            "the superseded binding came back"
        );

        // And the work admitted under v1 still settles after the restart.
        let (_, receipts) = reopened
            .execute_block_adaptive_at(&[terminal], producer, 20)
            .unwrap();
        assert!(receipts[0].success);
        assert_eq!(reopened.get_account(&requester).unwrap().balance, 990);

        // Nothing names v1 now, so the next update retires it.
        let later = hash_bytes(b"a later model still");
        let mut third = reopened.native_inference_context().unwrap();
        third.allowed_executions.push(AllowedExecution {
            model_hash: later,
            profile_hash: later,
            generation_hash: later,
            assignment_hash: later,
        });
        reopened.update_native_inference_binding(third).unwrap();
        assert!(
            reopened.retained_binding_index().unwrap().is_empty(),
            "a binding nothing names is not kept forever"
        );
        assert!(
            reopened
                .get_storage(&context_account(), &version_key(v1))
                .is_none(),
            "and its row is gone from state"
        );
    }

    /// The sequence the public chain will actually follow: migrate, take paid
    /// work, then upgrade the binding while that work is still in flight.
    #[test]
    fn a_migrated_chain_can_upgrade_its_binding_without_stranding_work() {
        let f = fixture("migrated-then-upgraded");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        let v1 = f.context.commitment().unwrap();
        assert_eq!(state.native_inference_context(), Some(f.context.clone()));

        // Paid work admitted under the migrated binding.
        let req = request(&f, 0, 400);
        let id = req.job.request_id();
        let tx = native_request(state, &f.requester, req);
        state
            .execute_block_adaptive_at(&[tx], f.validators[0].address(), 1_700_200)
            .unwrap();
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 100);

        // A model upgrade, coordinated two blocks ahead.
        let upgraded = hash_bytes(b"a newer model for the public chain");
        let mut next = f.context.clone();
        next.allowed_executions.push(AllowedExecution {
            model_hash: upgraded,
            profile_hash: upgraded,
            generation_hash: upgraded,
            assignment_hash: upgraded,
        });
        let at = state.height() + 2;
        state
            .authorize_binding_update(at, v1, next.clone())
            .expect("a migrated chain can schedule a binding update");
        while state.height() < at {
            state
                .execute_block_adaptive_at(
                    &[],
                    f.validators[0].address(),
                    1_700_300 + state.height(),
                )
                .unwrap();
        }
        assert_eq!(
            state.native_inference_context(),
            Some(next),
            "the migrated chain published the upgrade itself, at its height"
        );
        assert_eq!(state.retained_binding_index().unwrap(), vec![v1]);

        // The work admitted before the upgrade still settles on its terms.
        let terminal = native_finalize(state, &f.validators[0], 0, id, certificate(&f, id));
        let (_, receipts) = state
            .execute_block_adaptive_at(&[terminal], f.validators[0].address(), 1_700_400)
            .unwrap();
        assert!(receipts[0].success);
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 0);
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            990
        );
        // And the chain is still the recovered public chain it was.
        assert!(state.recovery_context().is_some());
    }

    /// A block of ordinary traffic must not be routed somewhere that refuses
    /// it. BlockSTM refuses outright while a binding is active, and the
    /// adaptive chooser sends any block of a hundred or more diverse
    /// transfers there - so on a migrated chain, real public traffic would
    /// have failed as a block rather than executed.
    #[test]
    fn a_migrated_chain_never_routes_ordinary_traffic_to_a_path_that_refuses_it() {
        let f = fixture("migrated-execution-mode");
        let state = recovered_chain_at(&f, 4);

        // A hundred transfers to distinct receivers: what the chooser sends
        // to BlockSTM. These are only inspected for their shape here.
        let bulk: Vec<arc_types::Transaction> = (0..120u32)
            .map(|i| {
                let mut tx = arc_types::Transaction::new_transfer(
                    f.requester.address(),
                    hash_bytes(&i.to_le_bytes()),
                    1,
                    i as u64,
                );
                tx.tx_type = arc_types::TxType::Transfer;
                tx.body = arc_types::TxBody::Transfer(arc_types::transaction::TransferBody {
                    to: hash_bytes(&i.to_le_bytes()),
                    amount: 1,
                    amount_commitment: None,
                });
                tx
            })
            .collect();
        assert_eq!(
            state.execution_mode(&bulk),
            crate::block_stm::AdaptiveMode::BlockSTM,
            "without a binding this traffic goes to BlockSTM"
        );

        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        assert!(state.native_inference_context().is_some(), "migrated");

        assert_eq!(
            state.execution_mode(&bulk),
            crate::block_stm::AdaptiveMode::Sequential,
            "with a binding active it must take the path that accepts it"
        );
    }

    /// Block selection on a migrated chain. An ordinary block must come back
    /// whole - selecting the native subset of a block that has none would
    /// drop every transfer it carries - and a block holding native work must
    /// still be cut to one, because execution refuses a mixture and that
    /// refusal stops every node at the same block.
    #[test]
    fn selection_keeps_an_ordinary_block_whole_and_cuts_native_work_to_one() {
        let f = fixture("migrated-selection");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();
        state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        assert!(state.native_inference_context().is_some());

        let transfers: Vec<arc_types::Transaction> = (0..3u64)
            .map(|nonce| {
                outer_with_fee(
                    state,
                    &f.requester,
                    nonce,
                    1,
                    arc_types::TxBody::Transfer(arc_types::transaction::TransferBody {
                        to: f.validators[0].address(),
                        amount: 1,
                        amount_commitment: None,
                    }),
                )
            })
            .collect();
        assert_eq!(
            state.select_native_block_transactions(&transfers).len(),
            3,
            "an ordinary block on a migrated chain comes back whole"
        );

        let natives: Vec<arc_types::Transaction> = (0..2u64)
            .map(|nonce| native_request(state, &f.requester, request(&f, nonce, 400)))
            .collect();
        assert_eq!(
            state.select_native_block_transactions(&natives).len(),
            1,
            "two native requests in one block are cut to one"
        );
    }

    /// A node can reach a height without executing the block at it - it
    /// imported history, or adopted a window. If the coordinated height went
    /// by that way, the node holds different state from every validator that
    /// applied the change, and carrying on quietly is the worst thing it can
    /// do. It stops instead.
    #[test]
    fn passing_a_coordinated_height_without_applying_it_stops_the_node() {
        let f = fixture("coordination-missed");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 8), f.context.clone())
            .unwrap();

        // Advance the way a catching-up node does: the height moves without
        // that block being executed here, so no hook ran at 8.
        *state.height.write() = 12;
        let error = state
            .apply_due_native_migration()
            .expect_err("a chain past the coordinated height must not carry on");
        assert!(format!("{error}").contains("resynchronised"), "{error}");
        assert!(
            state.native_inference_context().is_none(),
            "and nothing was activated late"
        );
    }

    /// Work admitted AFTER a binding update must be readable by the worker
    /// that has to execute it. Running this across four node processes showed
    /// the worker's poll failing on every tick the moment such a request was
    /// admitted, which stops the chain serving any paid work at all.
    #[test]
    fn work_admitted_after_an_update_is_readable_by_the_worker() {
        let f = fixture("post-update-poll");
        let state = &f.ledger.state;
        state.activate_native_inference(f.context.clone()).unwrap();

        // Update with nothing pending, so the previous binding is not
        // retained - exactly the shape the processes were in.
        let upgraded = hash_bytes(b"a newer model");
        let mut next = f.context.clone();
        next.allowed_executions.push(AllowedExecution {
            model_hash: upgraded,
            profile_hash: upgraded,
            generation_hash: upgraded,
            assignment_hash: upgraded,
        });
        let v2 = state.update_native_inference_binding(next.clone()).unwrap();
        assert!(
            state.retained_binding_index().unwrap().is_empty(),
            "nothing was pending, so nothing is retained"
        );

        // Admit a request for the model the UPDATE added - the case the
        // processes were in, and the one `request()` cannot build because it
        // always uses the binding's first execution.
        let input = b"input".to_vec();
        let job = arc_types::inference_contract::InferenceJob {
            version: arc_types::inference_contract::INFERENCE_CONTRACT_VERSION,
            domain: next.domain,
            requester: f.requester.address(),
            nonce: 0,
            model_hash: upgraded,
            profile_hash: upgraded,
            input_hash: hash_bytes(&input),
            generation_hash: upgraded,
            assignment_hash: upgraded,
            max_tokens: 32,
            max_output_bytes: 128,
            execution_price: 10,
            reserved_max_payment: 100,
            expires_at: 400,
        };
        let req = InferenceRequest::sign(job, &f.requester).unwrap();
        let id = req.job.request_id();
        let tx = native_request(state, &f.requester, req);
        let (_, receipts) = state
            .execute_block_adaptive_at(&[tx], f.validators[0].address(), 10)
            .unwrap();
        assert!(receipts[0].success);

        // What the worker does every tick.
        let pending = state
            .native_inference_pending_requests(v2)
            .expect("the worker must be able to read the work it has to execute");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, id);
    }

    /// A configuration change must not make past settlements disappear. The
    /// receipt of work settled under a binding that has since been replaced
    /// stays readable - the app, the explorer and any RPC client depend on
    /// it, and the migration exists to preserve history, not to rewrite it.
    #[test]
    fn a_receipt_survives_the_binding_it_was_settled_under_being_replaced() {
        let f = fixture("receipt-across-update");
        let state = &f.ledger.state;
        let v1 = state.activate_native_inference(f.context.clone()).unwrap();

        // Settle a request fully under v1.
        let req = request(&f, 0, 400);
        let id = req.job.request_id();
        let tx = native_request(state, &f.requester, req);
        state
            .execute_block_adaptive_at(&[tx], f.validators[0].address(), 10)
            .unwrap();
        let terminal = native_finalize(state, &f.validators[0], 0, id, certificate(&f, id));
        let (_, receipts) = state
            .execute_block_adaptive_at(&[terminal], f.validators[0].address(), 20)
            .unwrap();
        assert!(receipts[0].success);
        let before = state
            .native_inference_receipt(id, v1)
            .expect("readable under its own binding")
            .expect("a receipt");

        // Replace the binding. Nothing is pending, so v1 is not retained.
        let upgraded = hash_bytes(b"a newer model");
        let mut next = f.context.clone();
        next.allowed_executions.push(AllowedExecution {
            model_hash: upgraded,
            profile_hash: upgraded,
            generation_hash: upgraded,
            assignment_hash: upgraded,
        });
        let v2 = state.update_native_inference_binding(next).unwrap();
        assert!(state.retained_binding_index().unwrap().is_empty());

        let after = state
            .native_inference_receipt(id, v2)
            .expect("a settled receipt outlives the binding it was settled under")
            .expect("a receipt");
        assert_eq!(after.metadata.status, before.metadata.status);
        assert_eq!(after.metadata.output_hash, before.metadata.output_hash);
        assert_eq!(
            after.metadata.context_commitment, v1,
            "and still says which binding settled it"
        );
    }

    /// A projected root must never be a cached one.
    ///
    /// This pins an invariant against a merge rather than against a bug. The
    /// Studio's recovery hotfix makes a node reuse the last verified state
    /// root until something that feeds the root actually changes, and this
    /// projection asks the opposite question: what root would this state have
    /// with changes that have NOT been applied. "Nothing has changed since
    /// last time" is true for those calls and completely irrelevant to them.
    /// If a cache ever leaks into the replacement path, native settlement
    /// commits the current root as if it were the projected one and every
    /// validator on a migrated chain derives something different at the first
    /// paid request.
    #[test]
    fn a_projected_recovery_root_is_never_the_unprojected_one() {
        let f = fixture("projection-not-cached");
        let state = recovered_chain_at(&f, 4);
        let context = state.recovery_context().expect("recovery-bound");

        let plain = state.compute_recovery_state_root_with(&context, &[], &[]);
        assert_eq!(
            state.compute_recovery_state_root_with(&context, &[], &[]),
            plain,
            "the same state twice is the same root - caching this is legitimate"
        );

        // An account the projection changes but the state does not.
        let address = f.requester.address();
        let mut moved = state.get_account(&address).expect("a funded account");
        moved.balance = moved.balance.checked_add(1).unwrap();
        let projected = state.compute_recovery_state_root_with(&context, &[(address, moved)], &[]);
        assert_ne!(
            projected, plain,
            "a projection with an account replacement must not return the unprojected root"
        );

        // And the same for a storage row, which native settlement also writes.
        let with_storage = state.compute_recovery_state_root_with(
            &context,
            &[],
            &[(context_account(), metadata_key(), b"projected".to_vec())],
        );
        assert_ne!(
            with_storage, plain,
            "a projection with a storage replacement must not return the unprojected root"
        );

        // The real state was never touched by any of this.
        assert_eq!(
            state.compute_recovery_state_root_with(&context, &[], &[]),
            plain
        );
    }

    /// The activation is part of the block that reaches the coordinated
    /// height, not something that happens just after it.
    ///
    /// This is what makes the change one every validator makes rather than
    /// one only the validators running a particular code path make. Because
    /// the block's own state root commits it, a node that applies that block
    /// without making the same write computes a different root and rejects
    /// it - loudly, at that block - instead of carrying on with state nobody
    /// else has.
    #[test]
    fn the_activation_is_inside_the_block_that_reaches_its_height() {
        let f = fixture("activation-in-block");
        let state = recovered_chain_at(&f, 4);
        state
            .authorize_native_migration(migration_for(&f, 5), f.context.clone())
            .unwrap();

        // Empty blocks on this chain carry nothing and move no root, which is
        // how the recovered chain has been advancing all along.
        let before = state.compute_state_root();
        assert_eq!(
            state.blocks.get(&4).expect("block 4").header.state_root,
            before
        );

        let (block, _) = state
            .execute_block_adaptive_at(&[], f.validators[0].address(), 1_700_099)
            .unwrap();
        assert_eq!(state.height(), 5);
        assert_eq!(state.native_inference_context(), Some(f.context.clone()));

        let after = state.compute_state_root();
        assert_ne!(
            after, before,
            "the activation changed state, so the root must have moved"
        );
        assert_eq!(
            block.header.state_root, after,
            "and the block that reached the coordinated height COMMITS that root - \
             a validator that did not activate here cannot produce this block"
        );
    }

    #[test]
    fn canonical_native_envelope_domain_bounds_and_mixed_block_rejections_are_atomic() {
        let f = fixture("canonical-reject");
        let state = &f.ledger.state;
        let req = request(&f, 0, 100);
        let original = native_request(state, &f.requester, req.clone());
        assert_rejected_unchanged(state, std::slice::from_ref(&original)); // default genesis
        *state.recovery_context.write() = Some(crate::recovery::RecoveryContext::new(
            "test", f.genesis, 1, 0,
        ));
        assert!(state.activate_native_inference(f.context.clone()).is_err());
        assert_rejected_unchanged(state, std::slice::from_ref(&original)); // recovered v3
        *state.recovery_context.write() = None;
        state.activate_native_inference(f.context.clone()).unwrap();
        let mut variants = Vec::new();
        for change in 0..12 {
            let mut job = req.job.clone();
            match change {
                0 => job.domain.chain_genesis = Hash256::ZERO,
                1 => job.profile_hash = Hash256::ZERO,
                2 => job.execution_price = 0,
                3 => job.execution_price = job.reserved_max_payment + 1,
                4 => job.max_tokens = 0,
                5 => job.max_output_bytes = u32::MAX,
                6 => job.expires_at = 1,
                // Cross-domain replay: each domain component alone, signed
                // afresh by the requester, is another chain's request.
                7 => job.domain.recovery_epoch += 1,
                8 => job.domain.validator_set_hash = Hash256([9; 32]),
                // Each execution-tuple component alone.
                9 => job.generation_hash = Hash256::ZERO,
                10 => job.assignment_hash = Hash256::ZERO,
                _ => job.nonce = 1,
            }
            let signed = InferenceRequest::sign(job, &f.requester).unwrap();
            variants.push(native_request(state, &f.requester, signed));
        }
        let mut tx = original.clone();
        tx.signature = arc_crypto::signature::Signature::null();
        tx.sig_verified = true;
        variants.push(tx);
        for gas in [0, 1, arc_types::transaction::gas_costs::BLOCK_GAS_LIMIT + 1] {
            let mut tx = original.clone();
            tx.gas_limit = gas;
            tx.sign(&f.requester).unwrap();
            variants.push(tx);
        }
        let mut tx = original.clone();
        tx.fee = 1;
        tx.sign(&f.requester).unwrap();
        variants.push(tx);
        let mut tx = original.clone();
        if let arc_types::TxBody::NativeInferenceRequest(ref mut body) = tx.body {
            body.input_blob.push(1);
        }
        tx.sign(&f.requester).unwrap();
        variants.push(tx);
        for tx in variants {
            assert_rejected_unchanged(state, &[tx]);
        }
        assert_rejected_unchanged(state, &[original.clone(), original.clone()]);
        assert_rejected_unchanged(
            state,
            &[arc_types::Transaction::new_transfer(
                f.requester.address(),
                Hash256::ZERO,
                1,
                0,
            )],
        );
        assert!(state.execute_tx_pub(&original).is_err());
        assert!(state.execute_block_stm(&[original], Hash256::ZERO).is_err());
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_native_bad_certificates_do_not_consume_nonce_or_reserve() {
        let f = fixture("canonical-certificate");
        let state = &f.ledger.state;
        state.activate_native_inference(f.context.clone()).unwrap();
        let req = request(&f, 0, 100);
        let id = req.job.request_id();
        state
            .execute_block_verified_at(
                &[native_request(state, &f.requester, req)],
                Hash256::ZERO,
                1,
            )
            .unwrap();
        let valid = certificate(&f, id);
        let mut cert = valid.clone();
        cert.votes.truncate(4);
        let mut bad = vec![cert];
        let mut cert = valid.clone();
        cert.votes[1] = cert.votes[0].clone();
        bad.push(cert);
        let mut cert = valid.clone();
        cert.output.push(1);
        bad.push(cert);
        let mut cert = valid.clone();
        cert.output = vec![0; 129];
        bad.push(cert);
        let mut cert = valid.clone();
        cert.votes[0] = arc_types::inference_contract::sign_vote(
            id,
            &cert.output,
            &KeyPair::generate_ed25519(),
        )
        .unwrap();
        cert.votes.sort_by_key(|v| v.validator.0);
        bad.push(cert);
        for cert in bad {
            assert_rejected_unchanged(
                state,
                &[native_finalize(state, &f.validators[0], 0, id, cert)],
            );
        }
        assert_eq!(state.get_account(&escrow_address(id)).unwrap().balance, 100);
        assert_eq!(
            state.get_account(&f.validators[0].address()).unwrap().nonce,
            0
        );
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_native_tiny_price_requester_validator_caller_aliases_conserve() {
        let f = fixture("canonical-alias");
        let state = &f.ledger.state;
        let key = &f.validators[0];
        let mut caller = state.get_account(&key.address()).unwrap();
        caller.balance = 1000;
        state
            .wal
            .append(WalOp::SetAccount(caller.address, caller.clone()), 0);
        state.apply_wal_op(&WalOp::SetAccount(caller.address, caller));
        state.activate_native_inference(f.context.clone()).unwrap();
        let mut job = request(&f, 0, 100).job;
        job.requester = key.address();
        job.execution_price = 1;
        job.reserved_max_payment = 1;
        let req = InferenceRequest::sign(job, key).unwrap();
        let id = req.job.request_id();
        state
            .execute_block_verified_at(&[native_request(state, key, req)], Hash256::ZERO, 1)
            .unwrap();
        state
            .execute_block_verified_at(
                &[native_finalize(state, key, 1, id, certificate(&f, id))],
                Hash256::ZERO,
                2,
            )
            .unwrap();
        assert_eq!(state.get_account(&key.address()).unwrap().nonce, 2);
        let total: u64 = f
            .validators
            .iter()
            .map(|v| state.get_account(&v.address()).unwrap().balance)
            .sum();
        assert_eq!(total, 1000);
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_native_unsealed_transition_is_quarantined_on_restart() {
        let f = fixture("canonical-unsealed");
        let state = &f.ledger.state;
        let c = state.activate_native_inference(f.context.clone()).unwrap();
        let req = request(&f, 0, 100);
        let id = req.job.request_id();
        let tx = native_request(state, &f.requester, req);
        let root = state.get_state_root();
        let record = state.plan_native_transaction(&tx, &f.context, 1).unwrap();
        // Legacy bincode stores enum variant 16 followed by the bare record.
        // A Box is transparent to Serde, so old WAL bytes must remain exact.
        let mut legacy_op_bytes = 16_u32.to_le_bytes().to_vec();
        legacy_op_bytes.extend(bincode::serialize(&record).unwrap());
        let boxed_op = WalOp::InferenceTransition(Box::new(record.clone()));
        assert_eq!(bincode::serialize(&boxed_op).unwrap(), legacy_op_bytes);
        let decoded: WalOp = bincode::deserialize(&legacy_op_bytes).unwrap();
        let WalOp::InferenceTransition(decoded_record) = decoded else {
            panic!("legacy inference transition decoded as another WAL operation");
        };
        assert_eq!(
            bincode::serialize(decoded_record.as_ref()).unwrap(),
            bincode::serialize(&record).unwrap()
        );

        let mut legacy_payload = 1_u64.to_le_bytes().to_vec();
        legacy_payload.extend(1_u64.to_le_bytes());
        legacy_payload.extend(&legacy_op_bytes);
        let current_payload = bincode::serialize(&(1_u64, 1_u64, &boxed_op)).unwrap();
        assert_eq!(current_payload, legacy_payload);
        let legacy_checksum = crc32fast::hash(&legacy_payload);
        assert_eq!(crc32fast::hash(&current_payload), legacy_checksum);
        let legacy_entry = crate::wal::WalEntry {
            block_height: 1,
            sequence: 1,
            op: boxed_op.clone(),
            checksum: legacy_checksum,
        };
        let mut legacy_entry_bytes = 1_u64.to_le_bytes().to_vec();
        legacy_entry_bytes.extend(1_u64.to_le_bytes());
        legacy_entry_bytes.extend(&legacy_op_bytes);
        legacy_entry_bytes.extend(legacy_checksum.to_le_bytes());
        let mut legacy_frame = (legacy_entry_bytes.len() as u32).to_le_bytes().to_vec();
        legacy_frame.extend(&legacy_entry_bytes);
        let current_entry_bytes = bincode::serialize(&legacy_entry).unwrap();
        let mut current_frame = (current_entry_bytes.len() as u32).to_le_bytes().to_vec();
        current_frame.extend(current_entry_bytes);
        assert_eq!(current_frame, legacy_frame);
        let decoded_entry: crate::wal::WalEntry =
            bincode::deserialize(&legacy_entry_bytes).unwrap();
        assert_eq!(decoded_entry.block_height, 1);
        assert_eq!(decoded_entry.sequence, 1);
        assert_eq!(decoded_entry.checksum, legacy_checksum);
        let WalOp::InferenceTransition(decoded_record) = decoded_entry.op else {
            panic!("legacy framed entry decoded as another WAL operation");
        };
        assert_eq!(
            bincode::serialize(decoded_record.as_ref()).unwrap(),
            bincode::serialize(&record).unwrap()
        );
        // Pre-box measurements were 736-byte WalOp / 760-byte WalEntry.
        #[cfg(target_pointer_width = "64")]
        {
            assert!(std::mem::size_of::<WalOp>() <= 320);
            assert!(std::mem::size_of::<crate::wal::WalEntry>() <= 344);
        }
        state
            .wal
            .append(WalOp::InferenceTransition(Box::new(record)), 1);
        state.durable_wal_barrier().unwrap(); // crash after typed record, before block checkpoint
        let dir = f.dir.clone();
        let prefunded = f.prefunded.clone();
        let genesis = f.genesis;
        drop(f);
        let reopened = StateDB::with_genesis_persistent(&prefunded, &dir, genesis).unwrap();
        assert_eq!(reopened.height(), 0);
        assert_eq!(reopened.get_state_root(), root);
        assert!(reopened.native_inference_receipt(id, c).unwrap().is_none());
        assert!(
            reopened
                .native_inference_pending_requests(c)
                .unwrap()
                .is_empty()
        );
        assert!(!reopened.receipts.contains_key(&tx.hash.0));
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_native_wal_failure_refuses_ack_and_further_blocks() {
        let f = fixture("canonical-fsync");
        let state = &f.ledger.state;
        let c = state.activate_native_inference(f.context.clone()).unwrap();
        let tx = native_request(state, &f.requester, request(&f, 0, 100));
        let prior_root = state.get_state_root();
        let plan = state.plan_native_transaction(&tx, &f.context, 1).unwrap();
        let after_root = state.projected_native_root(&plan.account_updates, &plan.storage_updates);
        state.wal.inject_failure(crate::wal::WalFaultPoint::Fsync);
        assert!(
            state
                .execute_block_verified_at(std::slice::from_ref(&tx), Hash256::ZERO, 1)
                .is_err()
        );
        assert!(
            state
                .execute_block_verified_at(&[], Hash256::ZERO, 2)
                .is_err()
        );
        assert_eq!(state.get_state_root(), prior_root);
        assert_eq!(state.height(), 0);
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            1000
        );
        assert!(state.native_inference_pending_requests(c).is_err());
        assert!(state.native_inference_context().is_none());
        let dir = f.dir.clone();
        let prefunded = f.prefunded.clone();
        let genesis = f.genesis;
        drop(f);
        let reopened = StateDB::with_genesis_persistent(&prefunded, &dir, genesis).unwrap();
        // An unsuccessful fsync can still have written a whole block. Recovery
        // may preserve that whole boundary or the prior one, never half a tx.
        match reopened.height() {
            0 => {
                assert_eq!(reopened.get_state_root(), prior_root);
                assert!(
                    reopened
                        .native_inference_pending_requests(c)
                        .unwrap()
                        .is_empty()
                );
                assert!(!reopened.receipts.contains_key(&tx.hash.0));
            }
            1 => {
                assert_eq!(reopened.get_state_root(), after_root);
                assert_eq!(
                    reopened.native_inference_pending_requests(c).unwrap().len(),
                    1
                );
                assert!(reopened.receipts.get(&tx.hash.0).unwrap().success);
            }
            h => panic!("unexpected recovered height {h}"),
        }
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_native_overflow_and_expired_certificate_reject_before_mutation() {
        let f = fixture("canonical-overflow");
        let state = &f.ledger.state;
        let payee = f.validators[0].address();
        let mut account = state.get_account(&payee).unwrap();
        account.balance = u64::MAX;
        state
            .wal
            .append(WalOp::SetAccount(payee, account.clone()), 0);
        state.apply_wal_op(&WalOp::SetAccount(payee, account));
        state.activate_native_inference(f.context.clone()).unwrap();
        let req = request(&f, 0, 3);
        let id = req.job.request_id();
        state
            .execute_block_verified_at(
                &[native_request(state, &f.requester, req)],
                Hash256::ZERO,
                1,
            )
            .unwrap();
        // Caller is a non-payee; overflow in another account cannot consume its nonce.
        assert_rejected_unchanged(
            state,
            &[native_finalize(
                state,
                &f.validators[5],
                0,
                id,
                certificate(&f, id),
            )],
        );
        assert_eq!(
            state.get_account(&f.validators[5].address()).unwrap().nonce,
            0
        );
        state
            .execute_block_verified_at(&[], Hash256::ZERO, 2)
            .unwrap();
        assert_rejected_unchanged(
            state,
            &[native_finalize(
                state,
                &f.requester,
                1,
                id,
                certificate(&f, id),
            )],
        );
        state
            .execute_block_verified_at(
                &[native_refund(state, &f.requester, 1, id)],
                Hash256::ZERO,
                3,
            )
            .unwrap();
        assert_eq!(
            state.get_account(&f.requester.address()).unwrap().balance,
            1000
        );
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_native_request_and_terminal_nonce_overflow_are_atomic() {
        let f = fixture("canonical-nonce-overflow");
        let state = &f.ledger.state;
        let mut caller = state.get_account(&f.validators[0].address()).unwrap();
        caller.nonce = u64::MAX;
        state
            .wal
            .append(WalOp::SetAccount(caller.address, caller.clone()), 0);
        state.apply_wal_op(&WalOp::SetAccount(caller.address, caller));
        state.activate_native_inference(f.context.clone()).unwrap();
        let req = request(&f, 0, 100);
        let id = req.job.request_id();
        state
            .execute_block_verified_at(
                &[native_request(state, &f.requester, req)],
                Hash256::ZERO,
                1,
            )
            .unwrap();
        assert_rejected_unchanged(
            state,
            &[native_finalize(
                state,
                &f.validators[0],
                u64::MAX,
                id,
                certificate(&f, id),
            )],
        );
        let mut job = request(&f, 0, 100).job;
        job.requester = f.validators[0].address();
        job.nonce = u64::MAX;
        let req = InferenceRequest::sign(job, &f.validators[0]).unwrap();
        assert_rejected_unchanged(state, &[native_request(state, &f.validators[0], req)]);
        let dir = f.dir.clone();
        drop(f);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_native_activation_is_pinned_bounded_and_wal_only() {
        let f = fixture("canonical-pinning");
        let state = &f.ledger.state;
        let c = state.activate_native_inference(f.context.clone()).unwrap();
        let mut changed = f.context.clone();
        changed.allowed_executions[0].profile_hash = Hash256::ZERO;
        assert!(state.activate_native_inference(changed).is_err());
        for i in 0..MAX_NATIVE_PENDING {
            state
                .native_inference_pending
                .insert(hash_bytes(&i.to_le_bytes()).0, 0);
        }
        assert_rejected_unchanged(
            state,
            &[native_request(state, &f.requester, request(&f, 0, 100))],
        );
        state.native_inference_pending.clear();
        let snapshot = state.snapshot();
        let dir = f.dir.clone();
        let prefunded = f.prefunded.clone();
        let genesis = f.genesis;
        drop(f);
        assert!(StateDB::recover(snapshot, dir.join("state.wal")).is_err());
        let reopened = StateDB::with_genesis_persistent(&prefunded, &dir, genesis).unwrap();
        assert_eq!(
            reopened
                .native_inference_context()
                .unwrap()
                .commitment()
                .unwrap(),
            c
        );
        assert!(
            reopened
                .native_inference_pending_requests(c)
                .unwrap()
                .is_empty()
        );
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
