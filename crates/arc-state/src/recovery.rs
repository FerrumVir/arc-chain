//! Quorum-certified, content-addressed production recovery checkpoints.
//!
//! `ARCCHKPT` is deliberately separate from the legacy peer snapshot API.
//! A peer response is not a trust root: activation requires an operator-
//! approved manifest hash, an exact configured validator set, and a strict
//! validator identity + stake supermajority. The complete package is verified
//! in memory before any active-data marker is written.

use crate::wal::{ContractStorage, Snapshot};
use crate::{StateDB, StateError, WalEntry, WalOp, read_wal_strict};
use arc_crypto::{Hash256, IncrementalMerkle, KeyPair, Signature, hash_bytes};
use arc_types::{
    Account, Address, Block, BlockHeader, EventLog, Identity, ProtocolVersion, Transaction,
    TxReceipt, strict_supermajority_threshold,
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

pub const ARCCHKPT_MAGIC: [u8; 8] = *b"ARCCHKPT";
pub const ARCCHKPT_FORMAT_VERSION: u16 = 1;
/// Format-v1 checkpoints are decoded as one in-memory object. Bound the input
/// before deserialization so an operator cannot accidentally hand an offline
/// signer or validator a sparse/hostile file that exhausts the machine.
/// A future format can raise this safely by chunking and authenticating each
/// section independently.
pub const ARCCHKPT_MAX_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;
/// Snapshot-assisted legacy recovery is intentionally bounded to the same
/// in-memory envelope as an ARCCHKPT payload. The legacy LZ4 framing carries
/// its decompressed length in the first four bytes, so reject allocation bombs
/// before asking the decoder to reserve attacker-controlled memory.
pub const LEGACY_SNAPSHOT_MAX_BYTES: usize = ARCCHKPT_MAX_PAYLOAD_BYTES;
/// Protocol-v3 recovery has one fixed six-validator trust committee.
pub const RECOVERY_VALIDATOR_SET_SIZE: usize = 6;
/// Five identities are required in addition to strict >2/3 signed stake.
pub const RECOVERY_SIGNATURES_REQUIRED: usize = 5;
pub const RECOVERY_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion {
    major: 3,
    minor: 0,
    patch: 0,
};
const ACTIVE_RECOVERY_MARKER: &str = "recovery.active";
const ACTIVE_RECOVERY_PREFIX: &str = "recovery-";
const ACTIVE_RECOVERY_SUFFIX: &str = ".arcchkpt";

/// The third prefunded legacy system account funds the one-time conversion
/// of synthetic legacy validator weights into real bonded account balances.
/// The transition debits it exactly; it never mints stake.
pub fn recovery_stake_reserve_address() -> Address {
    hash_bytes(&[2u8])
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryValidator {
    pub address: Address,
    pub public_key: [u8; 32],
    pub stake: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySignature {
    pub validator: Address,
    pub public_key: [u8; 32],
    pub signature_halves: [[u8; 32]; 2],
}

impl RecoverySignature {
    fn from_keypair(keypair: &KeyPair, signing_hash: &Hash256) -> Result<Self, RecoveryError> {
        let address = keypair.address();
        let signature = keypair
            .sign(signing_hash)
            .map_err(|error| RecoveryError::Signature(error.to_string()))?;
        let Signature::Ed25519 {
            public_key,
            signature,
        } = signature
        else {
            return Err(RecoveryError::Signature(
                "ARCCHKPT accepts Ed25519 validator signatures only".into(),
            ));
        };
        let signature: [u8; 64] = signature.try_into().map_err(|_| {
            RecoveryError::Signature("Ed25519 signature is not exactly 64 bytes".into())
        })?;
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        first.copy_from_slice(&signature[..32]);
        second.copy_from_slice(&signature[32..]);
        Ok(Self {
            validator: address,
            public_key,
            signature_halves: [first, second],
        })
    }

    fn as_signature(&self) -> Signature {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&self.signature_halves[0]);
        bytes.extend_from_slice(&self.signature_halves[1]);
        Signature::Ed25519 {
            public_key: self.public_key,
            signature: bytes,
        }
    }
}

/// Consensus domain installed after a recovery transition. Legacy state has
/// no domain, preserving every pre-recovery hash and state-root behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryContext {
    pub chain_id_hash: Hash256,
    pub genesis_hash: Hash256,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    pub protocol_version: ProtocolVersion,
}

impl RecoveryContext {
    pub fn new(
        chain_id: &str,
        genesis_hash: Hash256,
        recovery_epoch: u64,
        validator_set_id: u64,
    ) -> Self {
        Self {
            chain_id_hash: hash_bytes(chain_id.as_bytes()),
            genesis_hash,
            recovery_epoch,
            validator_set_id,
            protocol_version: RECOVERY_PROTOCOL_VERSION,
        }
    }

    /// Domain used by consensus messages and state commitments after H+1.
    pub fn domain_hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-recovery-consensus-domain-v1");
        hasher.update(self.chain_id_hash.as_ref());
        hasher.update(self.genesis_hash.as_ref());
        hasher.update(&self.recovery_epoch.to_be_bytes());
        hasher.update(&self.validator_set_id.to_be_bytes());
        hasher.update(&self.protocol_version.major.to_be_bytes());
        hasher.update(&self.protocol_version.minor.to_be_bytes());
        hasher.update(&self.protocol_version.patch.to_be_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }
}

/// Complete retained canonical state and history at source height H.
///
/// Maps are represented by sorted vectors. Import rejects non-canonical order
/// and duplicates rather than sorting attacker-controlled input before hashing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryPayload {
    pub blocks: Vec<(u64, Block)>,
    pub accounts: Vec<(Address, Account)>,
    pub storage: ContractStorage,
    pub contracts: Vec<(Address, Vec<u8>)>,
    pub receipts: Vec<(Hash256, TxReceipt)>,
    pub full_transactions: Vec<(Hash256, Transaction)>,
    pub tx_index: Vec<(Hash256, (u64, u32))>,
    pub account_txs: Vec<(Address, Vec<Hash256>)>,
    pub identities: Vec<(Address, Identity)>,
    pub event_logs: Vec<(u64, Vec<EventLog>)>,
    pub validators: Vec<(Address, u64)>,
    pub staking_pool: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryManifest {
    pub format_version: u16,
    pub chain_id: String,
    pub genesis_hash: Hash256,
    pub source_height: u64,
    pub source_block_hash: Hash256,
    pub source_state_root: Hash256,
    pub source_consensus_round: u64,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    pub protocol_version: ProtocolVersion,
    pub validators: Vec<RecoveryValidator>,
    pub community_rewards_v1_activation_height: Option<u64>,
    pub full_state_root: Hash256,
    pub payload_hash: Hash256,
    pub created_at_unix_ms: u64,
}

impl RecoveryManifest {
    pub fn recovery_context(&self) -> RecoveryContext {
        RecoveryContext::new(
            &self.chain_id,
            self.genesis_hash,
            self.recovery_epoch,
            self.validator_set_id,
        )
    }

    /// Stable address of this manifest. Signatures are stored outside the
    /// manifest so adding the fifth approval cannot change what was approved.
    pub fn content_hash(&self) -> Hash256 {
        let bytes = bincode::serialize(self).expect("ARCCHKPT manifest is serializable");
        let mut hasher = blake3::Hasher::new_derive_key("ARCCHKPT-manifest-content-v1");
        hasher.update(&ARCCHKPT_MAGIC);
        hasher.update(&bytes);
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn signing_hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARCCHKPT-validator-approval-v1");
        hasher.update(self.content_hash().as_ref());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn transition_commitment(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-recovery-transition-block-v1");
        hasher.update(self.content_hash().as_ref());
        hasher.update(self.source_block_hash.as_ref());
        hasher.update(self.full_state_root.as_ref());
        hasher.update(self.recovery_context().domain_hash().as_ref());
        Hash256(*hasher.finalize().as_bytes())
    }

    /// The only valid H+1 block for this checkpoint. It contains no ordinary
    /// transactions; the manifest hash occupies `tx_root` and the transition
    /// transcript occupies `proof_hash` without changing the legacy wire type.
    pub fn transition_block(&self) -> Result<Block, RecoveryError> {
        let height = self
            .source_height
            .checked_add(1)
            .ok_or(RecoveryError::HeightOverflow)?;
        let header = BlockHeader {
            height,
            timestamp: self.created_at_unix_ms,
            parent_hash: self.source_block_hash,
            tx_root: self.content_hash(),
            state_root: self.full_state_root,
            proof_hash: self.transition_commitment(),
            tx_count: 0,
            producer: Hash256::ZERO,
            protocol_version: self.protocol_version,
            state_diff: None,
        };
        Ok(Block::new(header, Vec::new()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArcCheckpoint {
    pub magic: [u8; 8],
    pub manifest: RecoveryManifest,
    pub payload: RecoveryPayload,
    pub signatures: Vec<RecoverySignature>,
}

#[derive(Clone, Debug)]
pub struct RecoveryExportSpec {
    pub chain_id: String,
    pub genesis_hash: Hash256,
    pub source_consensus_round: u64,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    pub validators: Vec<RecoveryValidator>,
    pub community_rewards_v1_activation_height: Option<u64>,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Debug)]
pub struct RecoveryNetworkPolicy {
    pub chain_id: String,
    pub genesis_hash: Hash256,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    pub validators: Vec<(Address, u64)>,
    pub community_rewards_v1_activation_height: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct RecoveryTrustRoot {
    pub network: RecoveryNetworkPolicy,
    pub approved_manifest_hash: Hash256,
}

#[derive(Clone, Debug)]
pub struct RecoveryImport {
    pub checkpoint_path: PathBuf,
    pub approved_manifest_hash: Hash256,
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("ARCCHKPT I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ARCCHKPT codec error: {0}")]
    Codec(String),
    #[error("invalid ARCCHKPT: {0}")]
    Invalid(String),
    #[error("ARCCHKPT signature error: {0}")]
    Signature(String),
    #[error("ARCCHKPT manifest hash {actual} is not the operator-approved hash {expected}")]
    UnapprovedManifest { expected: Hash256, actual: Hash256 },
    #[error("source height cannot advance to a recovery transition block")]
    HeightOverflow,
}

impl From<RecoveryError> for StateError {
    fn from(value: RecoveryError) -> Self {
        StateError::PersistenceError(value.to_string())
    }
}

impl ArcCheckpoint {
    pub fn export_unsigned(
        state: &StateDB,
        mut spec: RecoveryExportSpec,
    ) -> Result<Self, RecoveryError> {
        canonicalize_recovery_validators(&mut spec.validators)?;
        // Payload is the exact legacy source state. Validator replacement and
        // missing zero-balance validator accounts are deterministic H+1
        // transition effects, not edits to the source snapshot. Keeping both
        // states distinct lets offline signers recompute the legacy state root
        // as well as the post-transition v3 root.
        let payload = state.export_recovery_payload();
        payload.validate_canonical()?;

        let Some((source_height, source_block)) = payload.blocks.last() else {
            return Err(RecoveryError::Invalid(
                "source state contains no canonical anchor block".into(),
            ));
        };
        let replayed_source_state_root = payload.legacy_state_root();
        let state_source_root = state.compute_state_root();
        if replayed_source_state_root != state_source_root {
            return Err(RecoveryError::Invalid(format!(
                "exported source state root {} differs from replayed state root {}",
                replayed_source_state_root, state_source_root
            )));
        }
        // Legacy genesis used a zero state_root in block 0. Every later block
        // must bind the actual replayed state root in its header.
        if (*source_height != 0 || source_block.header.state_root != Hash256::ZERO)
            && source_block.header.state_root != replayed_source_state_root
        {
            return Err(RecoveryError::Invalid(format!(
                "source anchor state root {} differs from replayed state root {}",
                source_block.header.state_root, replayed_source_state_root
            )));
        }
        let context = RecoveryContext::new(
            &spec.chain_id,
            spec.genesis_hash,
            spec.recovery_epoch,
            spec.validator_set_id,
        );
        let full_state_root = payload.transition_consensus_state_root(
            &context,
            spec.community_rewards_v1_activation_height,
            &spec.validators,
        )?;
        let payload_hash = payload.content_hash();
        let manifest = RecoveryManifest {
            format_version: ARCCHKPT_FORMAT_VERSION,
            chain_id: spec.chain_id,
            genesis_hash: spec.genesis_hash,
            source_height: *source_height,
            source_block_hash: source_block.hash,
            source_state_root: replayed_source_state_root,
            source_consensus_round: spec.source_consensus_round,
            recovery_epoch: spec.recovery_epoch,
            validator_set_id: spec.validator_set_id,
            protocol_version: RECOVERY_PROTOCOL_VERSION,
            validators: spec.validators,
            community_rewards_v1_activation_height: spec.community_rewards_v1_activation_height,
            full_state_root,
            payload_hash,
            created_at_unix_ms: spec.created_at_unix_ms,
        };
        Ok(Self {
            magic: ARCCHKPT_MAGIC,
            manifest,
            payload,
            signatures: Vec::new(),
        })
    }

    pub fn manifest_hash(&self) -> Hash256 {
        self.manifest.content_hash()
    }

    pub fn add_signature(&mut self, keypair: &KeyPair) -> Result<(), RecoveryError> {
        let address = keypair.address();
        let validator = self
            .manifest
            .validators
            .iter()
            .find(|validator| validator.address == address)
            .ok_or_else(|| {
                RecoveryError::Signature(format!(
                    "signer {address} is not in the recovery validator set"
                ))
            })?;
        let public_key: [u8; 32] = keypair
            .public_key_bytes()
            .try_into()
            .map_err(|_| RecoveryError::Signature("validator key is not Ed25519".into()))?;
        if public_key != validator.public_key {
            return Err(RecoveryError::Signature(format!(
                "signer {address} public key differs from the manifest"
            )));
        }
        let signature = RecoverySignature::from_keypair(keypair, &self.manifest.signing_hash())?;
        if let Some(existing) = self
            .signatures
            .iter_mut()
            .find(|signature| signature.validator == address)
        {
            *existing = signature;
        } else {
            self.signatures.push(signature);
            self.signatures
                .sort_by_key(|signature| signature.validator.0);
        }
        Ok(())
    }

    /// Verify every byte and internal invariant without consulting an external
    /// trust root or requiring a completed signature quorum. This is the safe
    /// pre-signing check used by offline validators.
    pub fn verify_content(&self) -> Result<(), RecoveryError> {
        if self.magic != ARCCHKPT_MAGIC {
            return Err(RecoveryError::Invalid("bad ARCCHKPT magic".into()));
        }
        if self.manifest.format_version != ARCCHKPT_FORMAT_VERSION {
            return Err(RecoveryError::Invalid(format!(
                "unsupported format version {}",
                self.manifest.format_version
            )));
        }
        if self.manifest.protocol_version != RECOVERY_PROTOCOL_VERSION {
            return Err(RecoveryError::Invalid(format!(
                "recovery requires protocol {}, got {}",
                RECOVERY_PROTOCOL_VERSION, self.manifest.protocol_version
            )));
        }
        if self.manifest.recovery_epoch == 0 || self.manifest.validator_set_id == 0 {
            return Err(RecoveryError::Invalid(
                "recovery_epoch and validator_set_id must both be non-zero".into(),
            ));
        }
        validate_recovery_validators(&self.manifest.validators)?;
        self.payload.validate_canonical()?;
        let Some((height, anchor)) = self.payload.blocks.last() else {
            return Err(RecoveryError::Invalid(
                "checkpoint has no anchor block".into(),
            ));
        };
        if *height != self.manifest.source_height
            || anchor.header.height != self.manifest.source_height
            || anchor.hash != self.manifest.source_block_hash
        {
            return Err(RecoveryError::Invalid(
                "source height/hash does not match the retained anchor block".into(),
            ));
        }
        if (*height != 0 || anchor.header.state_root != Hash256::ZERO)
            && anchor.header.state_root != self.manifest.source_state_root
        {
            return Err(RecoveryError::Invalid(
                "source anchor header does not commit the replayed source state root".into(),
            ));
        }
        if self.payload.staking_pool != checked_total_stake(&self.payload.validators)? {
            return Err(RecoveryError::Invalid(
                "source staking pool does not equal its retained validator stake".into(),
            ));
        }
        if self.payload.content_hash() != self.manifest.payload_hash {
            return Err(RecoveryError::Invalid("payload hash mismatch".into()));
        }
        let source_root = self.payload.legacy_state_root();
        if source_root != self.manifest.source_state_root {
            return Err(RecoveryError::Invalid(format!(
                "replayed source state root mismatch: manifest {}, computed {}",
                self.manifest.source_state_root, source_root
            )));
        }
        let full_root = self.payload.transition_consensus_state_root(
            &self.manifest.recovery_context(),
            self.manifest.community_rewards_v1_activation_height,
            &self.manifest.validators,
        )?;
        if full_root != self.manifest.full_state_root {
            return Err(RecoveryError::Invalid(format!(
                "full state root mismatch: manifest {}, computed {}",
                self.manifest.full_state_root, full_root
            )));
        }
        let transition = self.manifest.transition_block()?;
        if transition.header.height != self.manifest.source_height + 1
            || transition.header.parent_hash != self.manifest.source_block_hash
            || transition.header.tx_count != 0
            || !transition.tx_hashes.is_empty()
        {
            return Err(RecoveryError::Invalid(
                "dedicated recovery transition block is malformed".into(),
            ));
        }
        Ok(())
    }

    /// Verify content, the exact operator pin, and the complete local network
    /// policy before an offline validator signs the candidate.
    pub fn verify_candidate(&self, trust: &RecoveryTrustRoot) -> Result<(), RecoveryError> {
        let actual_hash = self.manifest_hash();
        if actual_hash != trust.approved_manifest_hash {
            return Err(RecoveryError::UnapprovedManifest {
                expected: trust.approved_manifest_hash,
                actual: actual_hash,
            });
        }
        verify_network_policy(&self.manifest, &trust.network)?;
        self.verify_content()
    }

    /// Verify the candidate plus both strict identity and stake
    /// supermajorities before activation.
    pub fn verify(&self, trust: &RecoveryTrustRoot) -> Result<(), RecoveryError> {
        self.verify_candidate(trust)?;
        self.verify_signature_quorum()
    }

    fn verify_signature_quorum(&self) -> Result<(), RecoveryError> {
        let mut seen = HashSet::new();
        let validator_map: HashMap<Address, &RecoveryValidator> = self
            .manifest
            .validators
            .iter()
            .map(|validator| (validator.address, validator))
            .collect();
        if self.signatures.len() > validator_map.len() {
            return Err(RecoveryError::Signature(
                "more signatures than configured validators".into(),
            ));
        }
        let signing_hash = self.manifest.signing_hash();
        let mut signed_stake = 0u64;
        for approval in &self.signatures {
            if !seen.insert(approval.validator) {
                return Err(RecoveryError::Signature(format!(
                    "duplicate validator signature {}",
                    approval.validator
                )));
            }
            let validator = validator_map.get(&approval.validator).ok_or_else(|| {
                RecoveryError::Signature(format!("unknown recovery signer {}", approval.validator))
            })?;
            if approval.public_key != validator.public_key
                || hash_bytes(&approval.public_key) != approval.validator
            {
                return Err(RecoveryError::Signature(format!(
                    "signer {} public key does not match its configured address",
                    approval.validator
                )));
            }
            approval
                .as_signature()
                .verify(&signing_hash, &approval.validator)
                .map_err(|_| {
                    RecoveryError::Signature(format!(
                        "invalid signature from {}",
                        approval.validator
                    ))
                })?;
            signed_stake = signed_stake.checked_add(validator.stake).ok_or_else(|| {
                RecoveryError::Invalid("signed validator stake exceeds u64::MAX".into())
            })?;
        }
        if seen.len() < RECOVERY_SIGNATURES_REQUIRED {
            return Err(RecoveryError::Signature(format!(
                "insufficient signer identities: have {}, require {} of {}",
                seen.len(),
                RECOVERY_SIGNATURES_REQUIRED,
                validator_map.len()
            )));
        }
        let total_stake = checked_total_stake(&self.payload.validators)?;
        let required_stake = strict_supermajority_threshold(total_stake);
        if signed_stake < required_stake {
            return Err(RecoveryError::Signature(format!(
                "insufficient signed stake: have {signed_stake}, require {required_stake} of {total_stake}"
            )));
        }
        Ok(())
    }

    pub fn write_to(&self, path: impl AsRef<Path>) -> Result<(), RecoveryError> {
        let bytes =
            bincode::serialize(self).map_err(|error| RecoveryError::Codec(error.to_string()))?;
        let path = path.as_ref();
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(&ARCCHKPT_MAGIC)?;
        file.write_all(&(bytes.len() as u64).to_be_bytes())?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    }

    pub fn read_from(path: impl AsRef<Path>) -> Result<Self, RecoveryError> {
        let path = path.as_ref();
        let mut file = File::open(path)?;
        let metadata_len = file.metadata()?.len();
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic)?;
        if magic != ARCCHKPT_MAGIC {
            return Err(RecoveryError::Invalid("bad ARCCHKPT file magic".into()));
        }
        let mut len = [0u8; 8];
        file.read_exact(&mut len)?;
        let declared = u64::from_be_bytes(len);
        if declared > ARCCHKPT_MAX_PAYLOAD_BYTES as u64 {
            return Err(RecoveryError::Invalid(format!(
                "checkpoint payload is {declared} bytes; format-v1 safety limit is {ARCCHKPT_MAX_PAYLOAD_BYTES} bytes"
            )));
        }
        if metadata_len != declared.saturating_add(16) {
            return Err(RecoveryError::Invalid(format!(
                "file length mismatch: declared {declared} payload bytes, file is {metadata_len} bytes"
            )));
        }
        let usize_len = usize::try_from(declared)
            .map_err(|_| RecoveryError::Invalid("checkpoint is too large for this host".into()))?;
        let mut bytes = vec![0u8; usize_len];
        file.read_exact(&mut bytes)?;
        let checkpoint: Self =
            bincode::deserialize_limited_exact::<Self, ARCCHKPT_MAX_PAYLOAD_BYTES>(&bytes)
                .map_err(|error| RecoveryError::Codec(error.to_string()))?;
        if checkpoint.magic != magic {
            return Err(RecoveryError::Invalid(
                "inner and outer ARCCHKPT magic differ".into(),
            ));
        }
        Ok(checkpoint)
    }
}

impl RecoveryPayload {
    pub fn content_hash(&self) -> Hash256 {
        let bytes = bincode::serialize(self).expect("canonical recovery payload is serializable");
        let mut hasher = blake3::Hasher::new_derive_key("ARCCHKPT-payload-content-v1");
        hasher.update(&bytes);
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Recompute the legacy account-only Merkle root from the exact retained
    /// source accounts. This is intentionally independent of StateDB caches,
    /// dirty-key tracking, and the replacement validator set.
    pub fn legacy_state_root(&self) -> Hash256 {
        let mut tree = IncrementalMerkle::new();
        for (address, account) in &self.accounts {
            let bytes = bincode::serialize(account).expect("canonical account is serializable");
            tree.update(address.0, hash_bytes(&bytes));
        }
        tree.rebuild();
        tree.root()
    }

    fn transition_accounts(
        &self,
        validators: &[RecoveryValidator],
    ) -> Result<Vec<(Address, Account)>, RecoveryError> {
        let source_stake = checked_total_stake(&self.validators)?;
        if self.staking_pool != source_stake {
            return Err(RecoveryError::Invalid(
                "source staking pool does not equal its retained validator stake".into(),
            ));
        }
        let target_stake = validators.iter().try_fold(0u64, |total, validator| {
            total
                .checked_add(validator.stake)
                .ok_or_else(|| RecoveryError::Invalid("validator stake exceeds u64::MAX".into()))
        })?;
        if target_stake != source_stake {
            return Err(RecoveryError::Invalid(format!(
                "recovery validator stake {target_stake} must equal conserved source stake {source_stake}"
            )));
        }

        let mut accounts = self.accounts.clone();
        // Some legacy fleets recorded validator weights only in the validator
        // map; their account.staked_balance remained zero. Move any real legacy
        // bonds into the target positions, then fund only the synthetic
        // shortfall from an explicit prefunded system reserve. This handles
        // both legacy shapes while conserving liquid+bonded supply exactly.
        let mut real_legacy_bonds = 0u64;
        for (_, account) in &mut accounts {
            real_legacy_bonds = real_legacy_bonds
                .checked_add(account.staked_balance)
                .ok_or_else(|| {
                    RecoveryError::Invalid("legacy account stake exceeds u64::MAX".into())
                })?;
            account.staked_balance = 0;
        }
        let reserve_debit = target_stake.checked_sub(real_legacy_bonds).ok_or_else(|| {
            RecoveryError::Invalid(format!(
                "real legacy account bonds {real_legacy_bonds} exceed target stake {target_stake}"
            ))
        })?;
        let reserve_address = recovery_stake_reserve_address();
        let reserve_index = accounts
            .binary_search_by_key(&reserve_address.0, |entry| entry.0.0)
            .map_err(|_| {
                RecoveryError::Invalid(format!(
                    "recovery stake reserve {reserve_address} is absent from source state"
                ))
            })?;
        if accounts[reserve_index].1.balance < reserve_debit {
            return Err(RecoveryError::Invalid(format!(
                "recovery stake reserve has {}, needs {reserve_debit}",
                accounts[reserve_index].1.balance
            )));
        }
        accounts[reserve_index].1.balance -= reserve_debit;
        for validator in validators {
            let index = match accounts.binary_search_by_key(&validator.address.0, |entry| entry.0.0)
            {
                Ok(index) => index,
                Err(index) => {
                    accounts.insert(
                        index,
                        (validator.address, Account::new(validator.address, 0)),
                    );
                    index
                }
            };
            accounts[index].1.staked_balance = validator.stake;
        }
        Ok(accounts)
    }

    fn transition_consensus_state_root(
        &self,
        context: &RecoveryContext,
        reward_activation_height: Option<u64>,
        validators: &[RecoveryValidator],
    ) -> Result<Hash256, RecoveryError> {
        let accounts = self.transition_accounts(validators)?;
        let target_validators: Vec<_> = validators
            .iter()
            .map(|validator| (validator.address, validator.stake))
            .collect();
        let target_staking_pool = checked_total_stake(&target_validators)?;
        Ok(consensus_state_root_from_sections(
            context,
            reward_activation_height,
            &accounts,
            &self.storage,
            &self.contracts,
            &self.identities,
            &target_validators,
            target_staking_pool,
        ))
    }

    pub fn consensus_state_root(
        &self,
        context: &RecoveryContext,
        reward_activation_height: Option<u64>,
    ) -> Hash256 {
        consensus_state_root_from_sections(
            context,
            reward_activation_height,
            &self.accounts,
            &self.storage,
            &self.contracts,
            &self.identities,
            &self.validators,
            self.staking_pool,
        )
    }

    fn validate_canonical(&self) -> Result<(), RecoveryError> {
        require_sorted_unique(&self.blocks, |entry| entry.0, "blocks")?;
        require_sorted_unique(&self.accounts, |entry| entry.0.0, "accounts")?;
        require_sorted_unique(&self.storage, |entry| entry.0.0, "storage")?;
        for (_, entries) in &self.storage {
            require_sorted_unique(entries, |entry| entry.0.0, "contract storage entries")?;
        }
        require_sorted_unique(&self.contracts, |entry| entry.0.0, "contracts")?;
        require_sorted_unique(&self.receipts, |entry| entry.0.0, "receipts")?;
        require_sorted_unique(
            &self.full_transactions,
            |entry| entry.0.0,
            "full transactions",
        )?;
        require_sorted_unique(&self.tx_index, |entry| entry.0.0, "transaction index")?;
        require_sorted_unique(&self.account_txs, |entry| entry.0.0, "account history")?;
        require_sorted_unique(&self.identities, |entry| entry.0.0, "identities")?;
        require_sorted_unique(&self.event_logs, |entry| entry.0, "event logs")?;
        require_sorted_unique(&self.validators, |entry| entry.0.0, "validator state")?;

        for (index, (height, block)) in self.blocks.iter().enumerate() {
            if *height != block.header.height || block.hash != Block::compute_hash(&block.header) {
                return Err(RecoveryError::Invalid(format!(
                    "block record {height} has invalid height or hash"
                )));
            }
            if block.header.tx_count as usize != block.tx_hashes.len() {
                return Err(RecoveryError::Invalid(format!(
                    "block {height} transaction count mismatch"
                )));
            }
            if index == 0 {
                if *height != 0 || block.header.parent_hash != Hash256::ZERO {
                    return Err(RecoveryError::Invalid(
                        "retained canonical history must start at genesis".into(),
                    ));
                }
            } else {
                let (previous_height, previous) = &self.blocks[index - 1];
                if *height != previous_height.saturating_add(1)
                    || block.header.parent_hash != previous.hash
                {
                    return Err(RecoveryError::Invalid(format!(
                        "canonical history breaks before block {height}"
                    )));
                }
            }
        }
        for (address, account) in &self.accounts {
            if account.address != *address {
                return Err(RecoveryError::Invalid(format!(
                    "account key {address} differs from embedded address {}",
                    account.address
                )));
            }
        }
        for (hash, receipt) in &self.receipts {
            if receipt.tx_hash != *hash {
                return Err(RecoveryError::Invalid(format!(
                    "receipt key {hash} differs from embedded transaction hash {}",
                    receipt.tx_hash
                )));
            }
        }
        for (hash, transaction) in &self.full_transactions {
            if transaction.hash != *hash || transaction.compute_hash() != *hash {
                return Err(RecoveryError::Invalid(format!(
                    "full transaction {hash} has invalid content hash"
                )));
            }
        }
        for (address, identity) in &self.identities {
            if identity.address != *address {
                return Err(RecoveryError::Invalid(format!(
                    "identity key {address} differs from embedded address {}",
                    identity.address
                )));
            }
        }
        Ok(())
    }
}

fn verify_network_policy(
    manifest: &RecoveryManifest,
    expected: &RecoveryNetworkPolicy,
) -> Result<(), RecoveryError> {
    if manifest.chain_id != expected.chain_id
        || manifest.genesis_hash != expected.genesis_hash
        || manifest.recovery_epoch != expected.recovery_epoch
        || manifest.validator_set_id != expected.validator_set_id
        || manifest.community_rewards_v1_activation_height
            != expected.community_rewards_v1_activation_height
    {
        return Err(RecoveryError::Invalid(
            "manifest network/epoch/activation fields differ from local approved policy".into(),
        ));
    }
    let mut expected_validators = expected.validators.clone();
    expected_validators.sort_by_key(|entry| entry.0.0);
    let manifest_validators: Vec<_> = manifest
        .validators
        .iter()
        .map(|validator| (validator.address, validator.stake))
        .collect();
    if manifest_validators != expected_validators {
        return Err(RecoveryError::Invalid(
            "manifest validator identities/stakes differ from configured genesis".into(),
        ));
    }
    Ok(())
}

fn canonicalize_recovery_validators(
    validators: &mut [RecoveryValidator],
) -> Result<(), RecoveryError> {
    validators.sort_by_key(|validator| validator.address.0);
    validate_recovery_validators(validators)
}

fn validate_recovery_validators(validators: &[RecoveryValidator]) -> Result<(), RecoveryError> {
    if validators.len() != RECOVERY_VALIDATOR_SET_SIZE {
        return Err(RecoveryError::Invalid(format!(
            "protocol-v3 recovery requires exactly {RECOVERY_VALIDATOR_SET_SIZE} validators, got {}",
            validators.len()
        )));
    }
    require_sorted_unique(
        validators,
        |validator| validator.address.0,
        "manifest validators",
    )?;
    for validator in validators {
        if validator.stake == 0 {
            return Err(RecoveryError::Invalid(format!(
                "validator {} has zero stake",
                validator.address
            )));
        }
        if hash_bytes(&validator.public_key) != validator.address {
            return Err(RecoveryError::Invalid(format!(
                "validator {} public key does not derive to its address",
                validator.address
            )));
        }
    }
    Ok(())
}

fn checked_total_stake(validators: &[(Address, u64)]) -> Result<u64, RecoveryError> {
    validators.iter().try_fold(0u64, |total, (_, stake)| {
        total
            .checked_add(*stake)
            .ok_or_else(|| RecoveryError::Invalid("validator stake exceeds u64::MAX".into()))
    })
}

fn require_sorted_unique<T, K: Ord + Copy>(
    entries: &[T],
    key: impl Fn(&T) -> K,
    label: &str,
) -> Result<(), RecoveryError> {
    if entries
        .windows(2)
        .any(|window| key(&window[0]) >= key(&window[1]))
    {
        return Err(RecoveryError::Invalid(format!(
            "{label} are not strictly sorted and unique"
        )));
    }
    Ok(())
}

fn commit_section(hasher: &mut blake3::Hasher, label: &[u8], hash: &Hash256) {
    hasher.update(&(label.len() as u64).to_be_bytes());
    hasher.update(label);
    hasher.update(hash.as_ref());
}

fn commit_serialized<T: Serialize>(hasher: &mut blake3::Hasher, label: &[u8], value: &T) {
    let bytes = bincode::serialize(value).expect("consensus state section is serializable");
    let mut section = blake3::Hasher::new_derive_key("ARC-full-state-section-v1");
    section.update(&(label.len() as u64).to_be_bytes());
    section.update(label);
    section.update(&(bytes.len() as u64).to_be_bytes());
    section.update(&bytes);
    commit_section(hasher, label, &Hash256(*section.finalize().as_bytes()));
}

#[allow(clippy::too_many_arguments)]
fn consensus_state_root_from_sections(
    context: &RecoveryContext,
    reward_activation_height: Option<u64>,
    accounts: &[(Address, Account)],
    storage: &ContractStorage,
    contracts: &[(Address, Vec<u8>)],
    identities: &[(Address, Identity)],
    validators: &[(Address, u64)],
    staking_pool: u64,
) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-full-consensus-state-v1");
    commit_section(&mut hasher, b"domain", &context.domain_hash());
    commit_serialized(&mut hasher, b"accounts", &accounts);
    commit_serialized(&mut hasher, b"storage", &storage);
    commit_serialized(&mut hasher, b"contracts", &contracts);
    commit_serialized(&mut hasher, b"identities", &identities);
    commit_serialized(&mut hasher, b"validators", &validators);
    commit_serialized(&mut hasher, b"staking_pool", &staking_pool);
    commit_serialized(
        &mut hasher,
        b"community_rewards_v1_activation_height",
        &reward_activation_height,
    );
    Hash256(*hasher.finalize().as_bytes())
}

#[derive(Clone, Copy, Debug)]
struct LegacyBoundary {
    height: u64,
    state_root: Hash256,
    checkpoint_index: usize,
}

/// Read the longest checksum- and sequence-valid WAL prefix while retaining a
/// precise reason for any rejected tail. This is used only with an exact-height
/// snapshot: the caller must prove the selected block boundary matches the
/// independently captured snapshot before it may ignore `tail_error`.
fn read_legacy_wal_prefix(path: &Path) -> Result<(Vec<WalEntry>, Option<String>), StateError> {
    let file = File::open(path).map_err(|error| {
        StateError::PersistenceError(format!("failed to open legacy WAL {path:?}: {error}"))
    })?;
    let mut reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut expected_sequence = 0u64;

    loop {
        let mut length_bytes = [0u8; 4];
        let first = reader.read(&mut length_bytes[..1]).map_err(|error| {
            StateError::PersistenceError(format!("failed to read legacy WAL {path:?}: {error}"))
        })?;
        if first == 0 {
            return Ok((entries, None));
        }
        if let Err(error) = reader.read_exact(&mut length_bytes[1..]) {
            return Ok((
                entries,
                Some(format!("truncated WAL frame length: {error}")),
            ));
        }
        let length = u32::from_le_bytes(length_bytes) as usize;
        if length == 0 || length > ARCCHKPT_MAX_PAYLOAD_BYTES {
            return Ok((entries, Some(format!("invalid WAL frame length {length}"))));
        }
        let mut encoded = vec![0u8; length];
        if let Err(error) = reader.read_exact(&mut encoded) {
            return Ok((
                entries,
                Some(format!("truncated WAL frame payload: {error}")),
            ));
        }
        let entry: WalEntry = match bincode::deserialize(&encoded) {
            Ok(entry) => entry,
            Err(error) => {
                return Ok((
                    entries,
                    Some(format!("invalid WAL entry encoding: {error}")),
                ));
            }
        };
        let checksum_payload =
            bincode::serialize(&(&entry.block_height, &entry.sequence, &entry.op)).map_err(
                |error| {
                    StateError::PersistenceError(format!(
                        "failed to recompute legacy WAL checksum: {error}"
                    ))
                },
            )?;
        if entry.checksum != crc32fast::hash(&checksum_payload) {
            return Ok((
                entries,
                Some(format!(
                    "WAL checksum mismatch at sequence {}",
                    entry.sequence
                )),
            ));
        }
        if entry.sequence != expected_sequence {
            return Ok((
                entries,
                Some(format!(
                    "WAL sequence gap: expected {expected_sequence}, got {}",
                    entry.sequence
                )),
            ));
        }
        expected_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
            StateError::PersistenceError("legacy WAL sequence overflows u64".into())
        })?;
        entries.push(entry);
    }
}

fn legacy_post_root_state_mutation(op: &WalOp) -> bool {
    matches!(
        op,
        WalOp::SetAccount(..)
            | WalOp::SetStorage(..)
            | WalOp::DeleteStorage(..)
            | WalOp::SetContract(..)
            | WalOp::SetIdentity(..)
    )
}

/// Locate the latest structurally complete block boundary without trusting a
/// final WAL record merely because it calls itself a checkpoint. A later torn
/// suffix has no matching SetBlock+root and is deliberately not selected.
fn latest_complete_legacy_boundary(entries: &[WalEntry]) -> Result<LegacyBoundary, StateError> {
    let mut blocks = HashMap::<u64, usize>::new();
    let mut latest = None;
    for (index, entry) in entries.iter().enumerate() {
        match &entry.op {
            WalOp::SetBlock(height, block)
                if *height == block.header.height
                    && block.hash == Block::compute_hash(&block.header) =>
            {
                blocks.insert(*height, index);
            }
            WalOp::Checkpoint(root) => {
                let height = entry.block_height;
                let Some(&block_index) = blocks.get(&height) else {
                    continue;
                };
                let WalOp::SetBlock(_, block) = &entries[block_index].op else {
                    unreachable!("legacy block index is created only from SetBlock")
                };
                if block.header.state_root != *root
                    || entries[block_index + 1..index]
                        .iter()
                        .any(|candidate| legacy_post_root_state_mutation(&candidate.op))
                {
                    continue;
                }
                latest = Some(LegacyBoundary {
                    height,
                    state_root: *root,
                    checkpoint_index: index,
                });
            }
            _ => {}
        }
    }
    latest.ok_or_else(|| {
        StateError::PersistenceError(
            "legacy source has no complete SetBlock + Checkpoint boundary".into(),
        )
    })
}

fn validate_legacy_boundary(
    entries: &[WalEntry],
    boundary: &LegacyBoundary,
) -> Result<(), StateError> {
    let committed = &entries[..=boundary.checkpoint_index];
    let mut blocks = BTreeMap::<u64, (usize, &Block)>::new();
    let mut checkpoints = BTreeMap::<u64, (usize, Hash256)>::new();

    for (index, entry) in committed.iter().enumerate() {
        match &entry.op {
            WalOp::SetBlock(height, block) => {
                if *height != entry.block_height || *height != block.header.height {
                    return Err(StateError::PersistenceError(format!(
                        "legacy SetBlock at WAL sequence {} has inconsistent heights: tag={}, key={}, header={}",
                        entry.sequence, entry.block_height, height, block.header.height
                    )));
                }
                if block.hash != Block::compute_hash(&block.header) {
                    return Err(StateError::PersistenceError(format!(
                        "legacy block {} has an invalid header hash at WAL sequence {}",
                        height, entry.sequence
                    )));
                }
                if blocks.insert(*height, (index, block)).is_some() {
                    return Err(StateError::PersistenceError(format!(
                        "legacy WAL rewrites canonical block height {height}"
                    )));
                }
            }
            WalOp::Checkpoint(root) => match checkpoints.entry(entry.block_height) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert((index, *root));
                }
                std::collections::btree_map::Entry::Occupied(_) => {
                    return Err(StateError::PersistenceError(format!(
                        "legacy WAL has duplicate checkpoints at height {}",
                        entry.block_height
                    )));
                }
            },
            _ => {}
        }
    }

    let expected_block_count = boundary
        .height
        .checked_add(1)
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| StateError::PersistenceError("legacy height exceeds usize".into()))?;
    if blocks.len() != expected_block_count {
        return Err(StateError::PersistenceError(format!(
            "legacy WAL is missing canonical blocks through height {}: expected {}, got {}",
            boundary.height,
            expected_block_count,
            blocks.len()
        )));
    }

    let mut previous = None;
    for height in 0..=boundary.height {
        let Some(&(block_index, block)) = blocks.get(&height) else {
            return Err(StateError::PersistenceError(format!(
                "legacy WAL is missing canonical block {height}"
            )));
        };
        if let Some(parent) = previous
            && block.header.parent_hash != parent
        {
            return Err(StateError::PersistenceError(format!(
                "legacy canonical block parent mismatch at height {height}"
            )));
        }
        previous = Some(block.hash);

        let Some(&(checkpoint_index, checkpoint_root)) = checkpoints.get(&height) else {
            if height == 0 {
                continue;
            }
            return Err(StateError::PersistenceError(format!(
                "legacy canonical block {height} has no durable checkpoint"
            )));
        };
        if checkpoint_index <= block_index || checkpoint_root != block.header.state_root {
            return Err(StateError::PersistenceError(format!(
                "legacy block/checkpoint mismatch at height {height}"
            )));
        }
        if committed[block_index + 1..checkpoint_index]
            .iter()
            .any(|entry| legacy_post_root_state_mutation(&entry.op))
        {
            return Err(StateError::PersistenceError(format!(
                "legacy WAL mutates root-covered state after SetBlock and before Checkpoint at height {height}"
            )));
        }
    }

    let Some(&(final_index, final_root)) = checkpoints.get(&boundary.height) else {
        return Err(StateError::PersistenceError(
            "selected legacy boundary lost its checkpoint".into(),
        ));
    };
    if final_index != boundary.checkpoint_index || final_root != boundary.state_root {
        return Err(StateError::PersistenceError(
            "selected legacy boundary differs from validated checkpoint chain".into(),
        ));
    }
    Ok(())
}

fn read_legacy_recovery_snapshot(path: &Path) -> Result<Snapshot, StateError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        StateError::PersistenceError(format!(
            "failed to inspect legacy recovery snapshot {path:?}: {error}"
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(StateError::PersistenceError(format!(
            "legacy recovery snapshot must be a regular non-symlink file: {path:?}"
        )));
    }
    let compressed_len: usize = metadata
        .len()
        .try_into()
        .map_err(|_| StateError::PersistenceError("snapshot file size exceeds usize".into()))?;
    if !(4..=LEGACY_SNAPSHOT_MAX_BYTES).contains(&compressed_len) {
        return Err(StateError::PersistenceError(format!(
            "legacy recovery snapshot compressed size {compressed_len} is outside 4..={LEGACY_SNAPSHOT_MAX_BYTES} bytes"
        )));
    }
    let mut file = File::open(path).map_err(|error| {
        StateError::PersistenceError(format!(
            "failed to open legacy recovery snapshot {path:?}: {error}"
        ))
    })?;
    let mut compressed = Vec::with_capacity(compressed_len);
    file.read_to_end(&mut compressed).map_err(|error| {
        StateError::PersistenceError(format!(
            "failed to read legacy recovery snapshot {path:?}: {error}"
        ))
    })?;
    if compressed.len() != compressed_len {
        return Err(StateError::PersistenceError(format!(
            "legacy recovery snapshot changed size while being read: expected {compressed_len}, got {}",
            compressed.len()
        )));
    }
    let decoded_len =
        u32::from_le_bytes(compressed[..4].try_into().expect("four bytes checked")) as usize;
    if decoded_len > LEGACY_SNAPSHOT_MAX_BYTES {
        return Err(StateError::PersistenceError(format!(
            "legacy recovery snapshot requests {decoded_len} decompressed bytes, limit is {LEGACY_SNAPSHOT_MAX_BYTES}"
        )));
    }
    let decoded = lz4_flex::decompress_size_prepended(&compressed).map_err(|error| {
        StateError::PersistenceError(format!(
            "invalid legacy recovery snapshot compression at {path:?}: {error}"
        ))
    })?;
    let snapshot: Snapshot = bincode::deserialize(&decoded).map_err(|error| {
        StateError::PersistenceError(format!(
            "invalid legacy recovery snapshot payload at {path:?}: {error}"
        ))
    })?;
    validate_legacy_snapshot_shape(&snapshot)?;
    Ok(snapshot)
}

fn validate_legacy_snapshot_shape(snapshot: &Snapshot) -> Result<(), StateError> {
    let mut accounts = HashSet::with_capacity(snapshot.accounts.len());
    for (address, account) in &snapshot.accounts {
        if *address != account.address {
            return Err(StateError::PersistenceError(format!(
                "legacy snapshot account key {address} differs from embedded address {}",
                account.address
            )));
        }
        if !accounts.insert(address.0) {
            return Err(StateError::PersistenceError(format!(
                "legacy snapshot duplicates account {address}"
            )));
        }
    }

    let mut storage_addresses = HashSet::with_capacity(snapshot.storage.len());
    for (address, entries) in &snapshot.storage {
        if !storage_addresses.insert(address.0) {
            return Err(StateError::PersistenceError(format!(
                "legacy snapshot duplicates storage address {address}"
            )));
        }
        let mut keys = HashSet::with_capacity(entries.len());
        for (key, _) in entries {
            if !keys.insert(key.0) {
                return Err(StateError::PersistenceError(format!(
                    "legacy snapshot duplicates storage key {key} for {address}"
                )));
            }
        }
    }

    let mut contracts = HashSet::with_capacity(snapshot.contracts.len());
    for (address, bytecode) in &snapshot.contracts {
        if !contracts.insert(address.0) {
            return Err(StateError::PersistenceError(format!(
                "legacy snapshot duplicates contract {address}"
            )));
        }
        let account = snapshot
            .accounts
            .iter()
            .find_map(|(candidate, account)| (candidate == address).then_some(account))
            .ok_or_else(|| {
                StateError::PersistenceError(format!(
                    "legacy snapshot contract {address} has no account"
                ))
            })?;
        let code_hash = hash_bytes(bytecode);
        if account.code_hash != code_hash {
            return Err(StateError::PersistenceError(format!(
                "legacy snapshot contract {address} bytecode hash {code_hash} differs from account commitment {}",
                account.code_hash
            )));
        }
    }
    Ok(())
}

fn canonical_storage(mut storage: ContractStorage) -> ContractStorage {
    for (_, entries) in &mut storage {
        entries.sort_by_key(|entry| entry.0.0);
    }
    storage.sort_by_key(|entry| entry.0.0);
    storage
}

fn validate_snapshot_sections_against_wal(
    snapshot: &Snapshot,
    state: &StateDB,
) -> Result<(), StateError> {
    let wal_storage = canonical_storage(
        state
            .storage
            .iter()
            .map(|entry| {
                (
                    Hash256(*entry.key()),
                    entry
                        .value()
                        .iter()
                        .map(|value| (*value.key(), value.value().clone()))
                        .collect(),
                )
            })
            .collect(),
    );
    let snapshot_storage = canonical_storage(snapshot.storage.clone());
    if bincode::serialize(&wal_storage).ok() != bincode::serialize(&snapshot_storage).ok() {
        return Err(StateError::PersistenceError(
            "legacy snapshot contract storage differs from canonical WAL replay".into(),
        ));
    }

    let mut wal_contracts: Vec<_> = state
        .contracts
        .iter()
        .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
        .collect();
    wal_contracts.sort_by_key(|entry| entry.0.0);
    let mut snapshot_contracts = snapshot.contracts.clone();
    snapshot_contracts.sort_by_key(|entry| entry.0.0);
    if wal_contracts != snapshot_contracts {
        return Err(StateError::PersistenceError(
            "legacy snapshot contract bytecode differs from canonical WAL replay".into(),
        ));
    }
    Ok(())
}

impl StateDB {
    fn export_recovery_payload(&self) -> RecoveryPayload {
        let mut blocks: Vec<_> = self
            .blocks
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        blocks.sort_by_key(|entry| entry.0);
        let mut accounts: Vec<_> = self
            .accounts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        accounts.sort_by_key(|entry| entry.0.0);
        let mut storage: Vec<_> = self
            .storage
            .iter()
            .map(|entry| {
                let mut values: Vec<_> = entry
                    .value()
                    .iter()
                    .map(|value| (*value.key(), value.value().clone()))
                    .collect();
                values.sort_by_key(|value| value.0.0);
                (Hash256(*entry.key()), values)
            })
            .collect();
        storage.sort_by_key(|entry| entry.0.0);
        let mut contracts: Vec<_> = self
            .contracts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        contracts.sort_by_key(|entry| entry.0.0);
        let mut receipts: Vec<_> = self
            .receipts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        receipts.sort_by_key(|entry| entry.0.0);
        let mut full_transactions: Vec<_> = self
            .full_transactions
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        full_transactions.sort_by_key(|entry| entry.0.0);
        let mut tx_index: Vec<_> = self
            .tx_index
            .iter()
            .map(|entry| (Hash256(*entry.key()), *entry.value()))
            .collect();
        tx_index.sort_by_key(|entry| entry.0.0);
        let mut account_txs: Vec<_> = self
            .account_txs
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        account_txs.sort_by_key(|entry| entry.0.0);
        let mut identities: Vec<_> = self
            .identities
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        identities.sort_by_key(|entry| entry.0.0);
        let mut event_logs: Vec<_> = self
            .event_logs
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        event_logs.sort_by_key(|entry| entry.0);
        let mut validators: Vec<_> = self
            .validators
            .iter()
            .map(|entry| (Hash256(*entry.key()), *entry.value()))
            .collect();
        validators.sort_by_key(|entry| entry.0.0);
        RecoveryPayload {
            blocks,
            accounts,
            storage,
            contracts,
            receipts,
            full_transactions,
            tx_index,
            account_txs,
            identities,
            event_logs,
            validators,
            staking_pool: self.staking_pool.load(Ordering::Acquire),
        }
    }

    pub fn recovery_context(&self) -> Option<RecoveryContext> {
        self.recovery_context.read().clone()
    }

    pub fn recovery_manifest_hash(&self) -> Option<Hash256> {
        *self.recovery_manifest_hash.read()
    }

    pub fn active_protocol_version(&self) -> ProtocolVersion {
        self.recovery_context()
            .map(|context| context.protocol_version)
            .unwrap_or(ProtocolVersion::GENESIS)
    }

    pub fn transaction_domain_hash(&self) -> Option<Hash256> {
        self.recovery_context().map(|context| context.domain_hash())
    }

    pub fn verify_transaction_signature(
        &self,
        transaction: &Transaction,
    ) -> Result<(), arc_crypto::SignatureError> {
        match self.transaction_domain_hash() {
            Some(domain) => transaction.verify_signature_in_domain(&domain),
            None => transaction.verify_signature(),
        }
    }

    pub fn sign_transaction(
        &self,
        transaction: &mut Transaction,
        keypair: &KeyPair,
    ) -> Result<(), arc_crypto::SignatureError> {
        match self.transaction_domain_hash() {
            Some(domain) => transaction.sign_in_domain(keypair, &domain),
            None => transaction.sign(keypair),
        }
    }

    pub(crate) fn compute_recovery_state_root(&self, context: &RecoveryContext) -> Hash256 {
        // Do not materialize retained blocks, receipts, transaction bodies, or
        // logs here. They are content-addressed in ARCCHKPT, but are historical
        // data rather than live consensus state. Re-hashing history on every
        // block would make production O(chain length).
        let mut accounts: Vec<_> = self
            .accounts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        accounts.sort_by_key(|entry| entry.0.0);
        let mut storage: Vec<_> = self
            .storage
            .iter()
            .map(|entry| {
                let mut values: Vec<_> = entry
                    .value()
                    .iter()
                    .map(|value| (*value.key(), value.value().clone()))
                    .collect();
                values.sort_by_key(|value| value.0.0);
                (Hash256(*entry.key()), values)
            })
            .collect();
        storage.sort_by_key(|entry| entry.0.0);
        let mut contracts: Vec<_> = self
            .contracts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        contracts.sort_by_key(|entry| entry.0.0);
        let mut identities: Vec<_> = self
            .identities
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        identities.sort_by_key(|entry| entry.0.0);
        let mut validators: Vec<_> = self
            .validators
            .iter()
            .map(|entry| (Hash256(*entry.key()), *entry.value()))
            .collect();
        validators.sort_by_key(|entry| entry.0.0);
        consensus_state_root_from_sections(
            context,
            self.community_rewards_v1_activation_height(),
            &accounts,
            &storage,
            &contracts,
            &identities,
            &validators,
            self.staking_pool.load(Ordering::Acquire),
        )
    }

    /// Strict, read-only loader for the legacy WAL used to build an unsigned
    /// recovery candidate. It never creates a binding, marker, WAL, or
    /// checkpoint file and requires a complete block-boundary checkpoint.
    pub fn load_legacy_recovery_source(
        wal_dir: impl AsRef<Path>,
        expected_genesis_hash: Hash256,
        allow_unbound_legacy_wal: bool,
    ) -> Result<Self, StateError> {
        Self::load_legacy_recovery_source_inner(
            wal_dir.as_ref(),
            expected_genesis_hash,
            allow_unbound_legacy_wal,
            None,
        )
    }

    /// Load a legacy recovery source and bind its canonical block boundary to
    /// a same-height live state snapshot.
    ///
    /// Old ARC nodes did not persist every in-memory state section in every
    /// execution path. A WAL remains authoritative for block/history ordering,
    /// while the snapshot supplies exactly accounts/storage/contracts. The
    /// snapshot is never trusted by metadata: its account root is recomputed
    /// and must equal both the final complete WAL checkpoint and block header.
    pub fn load_legacy_recovery_source_with_snapshot(
        wal_dir: impl AsRef<Path>,
        expected_genesis_hash: Hash256,
        allow_unbound_legacy_wal: bool,
        snapshot_path: impl AsRef<Path>,
    ) -> Result<Self, StateError> {
        Self::load_legacy_recovery_source_inner(
            wal_dir.as_ref(),
            expected_genesis_hash,
            allow_unbound_legacy_wal,
            Some(snapshot_path.as_ref()),
        )
    }

    fn load_legacy_recovery_source_inner(
        wal_dir: &Path,
        expected_genesis_hash: Hash256,
        allow_unbound_legacy_wal: bool,
        snapshot_path: Option<&Path>,
    ) -> Result<Self, StateError> {
        if wal_dir.join(ACTIVE_RECOVERY_MARKER).exists() {
            return Err(StateError::PersistenceError(
                "source data directory already has an active recovery; chained recovery export is not supported by this format revision"
                    .into(),
            ));
        }
        let wal_path = wal_dir.join("state.wal");
        if !wal_path.is_file() {
            return Err(StateError::PersistenceError(format!(
                "legacy recovery source has no state WAL at {wal_path:?}"
            )));
        }

        let binding_path = wal_dir.join("genesis.network-hash");
        match fs::read_to_string(&binding_path) {
            Ok(value) => {
                let actual = Hash256::from_hex(value.trim()).map_err(|_| {
                    StateError::PersistenceError(format!(
                        "invalid genesis binding in {binding_path:?}"
                    ))
                })?;
                if actual != expected_genesis_hash {
                    return Err(StateError::PersistenceError(format!(
                        "legacy source is bound to genesis {actual}, expected {expected_genesis_hash}"
                    )));
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && allow_unbound_legacy_wal => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StateError::PersistenceError(format!(
                    "legacy source has no authenticated genesis binding at {binding_path:?}; rerun only after independent chain-tip verification with --allow-unbound-legacy-wal"
                )));
            }
            Err(error) => {
                return Err(StateError::PersistenceError(format!(
                    "failed to read genesis binding {binding_path:?}: {error}"
                )));
            }
        }

        let snapshot = snapshot_path
            .map(read_legacy_recovery_snapshot)
            .transpose()?;
        let (entries, rejected_tail) = if snapshot.is_some() {
            read_legacy_wal_prefix(&wal_path)?
        } else {
            (
                read_wal_strict(&wal_path).map_err(|error| {
                    StateError::PersistenceError(format!(
                        "legacy recovery source WAL is incomplete or corrupt: {error}"
                    ))
                })?,
                None,
            )
        };
        if entries.is_empty() {
            return Err(StateError::PersistenceError(
                "legacy recovery source WAL is empty".into(),
            ));
        }

        let boundary = latest_complete_legacy_boundary(&entries)?;
        if let Some(snapshot) = snapshot.as_ref()
            && (snapshot.block_height != boundary.height
                || snapshot.state_root != boundary.state_root)
        {
            return Err(StateError::PersistenceError(format!(
                "legacy snapshot/WAL boundary mismatch: snapshot height/root {}/{}, latest complete WAL height/root {}/{}",
                snapshot.block_height, snapshot.state_root, boundary.height, boundary.state_root
            )));
        }

        validate_legacy_boundary(&entries, &boundary)?;
        let committed_entries = &entries[..=boundary.checkpoint_index];
        let state = StateDB::new();
        for entry in committed_entries {
            state.apply_wal_op(&entry.op);
        }
        state.rebuild_transaction_indexes();
        if state.get_block(0).is_none() {
            return Err(StateError::PersistenceError(
                "legacy source does not retain genesis block 0".into(),
            ));
        }

        if let Some(snapshot) = snapshot.as_ref() {
            validate_snapshot_sections_against_wal(snapshot, &state)?;
            state.import_snapshot(snapshot, boundary.state_root)?;
            // import_snapshot changes only the three snapshot-covered state
            // sections and height. WAL-derived blocks/history/receipts remain.
            state.rebuild_transaction_indexes();
        } else {
            let actual_root = state.compute_state_root();
            if actual_root != boundary.state_root {
                return Err(StateError::PersistenceError(format!(
                    "legacy source checkpoint root mismatch: WAL {}, replayed {}; provide an exact-height --snapshot capture so missing legacy state can be root-verified without weakening the checkpoint",
                    boundary.state_root, actual_root
                )));
            }
        }

        if state.height() != boundary.height || state.get_state_root() != boundary.state_root {
            return Err(StateError::PersistenceError(format!(
                "legacy canonical boundary changed after replay: expected height/root {}/{}, got {}/{}",
                boundary.height,
                boundary.state_root,
                state.height(),
                state.get_state_root()
            )));
        }
        if boundary.checkpoint_index + 1 < entries.len() {
            tracing::warn!(
                ignored_entries = entries.len() - boundary.checkpoint_index - 1,
                first_ignored_sequence = entries[boundary.checkpoint_index + 1].sequence,
                committed_height = boundary.height,
                "Ignored WAL suffix after the latest fully committed legacy block boundary"
            );
        }
        if let Some(reason) = rejected_tail {
            tracing::warn!(
                committed_height = boundary.height,
                rejected_tail = %reason,
                "Quarantined invalid WAL tail after snapshot-bound legacy block boundary"
            );
        }
        Ok(state)
    }

    fn install_verified_checkpoint(&self, checkpoint: &ArcCheckpoint) -> Result<(), RecoveryError> {
        let payload = &checkpoint.payload;
        let transitioned_accounts = payload.transition_accounts(&checkpoint.manifest.validators)?;
        let transitioned_staking_pool =
            checkpoint
                .manifest
                .validators
                .iter()
                .try_fold(0u64, |total, validator| {
                    total.checked_add(validator.stake).ok_or_else(|| {
                        RecoveryError::Invalid("validator stake exceeds u64::MAX".into())
                    })
                })?;
        self.accounts.clear();
        self.storage.clear();
        self.blocks.clear();
        self.receipts.clear();
        self.tx_index.clear();
        self.account_txs.clear();
        self.contracts.clear();
        self.identities.clear();
        self.full_transactions.clear();
        self.event_logs.clear();
        self.validators.clear();

        for (address, account) in &transitioned_accounts {
            self.accounts.insert(address.0, account.clone());
            self.dirty_accounts.insert(address.0);
        }
        for (address, entries) in &payload.storage {
            let values = DashMap::new();
            for (key, value) in entries {
                values.insert(*key, value.clone());
            }
            self.storage.insert(address.0, values);
        }
        for (address, bytes) in &payload.contracts {
            self.contracts.insert(address.0, bytes.clone());
        }
        for (hash, receipt) in &payload.receipts {
            self.receipts.insert(hash.0, receipt.clone());
        }
        for (hash, transaction) in &payload.full_transactions {
            self.full_transactions.insert(hash.0, transaction.clone());
        }
        for (hash, location) in &payload.tx_index {
            self.tx_index.insert(hash.0, *location);
        }
        for (address, transactions) in &payload.account_txs {
            self.account_txs.insert(address.0, transactions.clone());
        }
        for (address, identity) in &payload.identities {
            self.identities.insert(address.0, identity.clone());
        }
        for (height, logs) in &payload.event_logs {
            self.event_logs.insert(*height, logs.clone());
        }
        for validator in &checkpoint.manifest.validators {
            self.validators.insert(validator.address.0, validator.stake);
        }
        self.staking_pool
            .store(transitioned_staking_pool, Ordering::Release);
        self.community_rewards_v1_activation_height.store(
            checkpoint
                .manifest
                .community_rewards_v1_activation_height
                .unwrap_or(u64::MAX),
            Ordering::Release,
        );
        *self.recovery_context.write() = Some(checkpoint.manifest.recovery_context());
        *self.recovery_manifest_hash.write() = Some(checkpoint.manifest_hash());

        for (height, block) in &payload.blocks {
            self.blocks.insert(*height, block.clone());
        }
        let transition = checkpoint.manifest.transition_block()?;
        self.blocks
            .insert(transition.header.height, transition.clone());
        *self.height.write() = transition.header.height;

        let computed = self.compute_recovery_state_root(&checkpoint.manifest.recovery_context());
        if computed != checkpoint.manifest.full_state_root {
            return Err(RecoveryError::Invalid(format!(
                "staged state root changed during import: expected {}, got {}",
                checkpoint.manifest.full_state_root, computed
            )));
        }
        Ok(())
    }

    /// Open normal state or activate/reopen an approved ARCCHKPT base.
    /// Existing legacy WAL data is never overwritten by an import.
    pub fn with_genesis_persistent_recovery(
        prefunded: &[(Address, u64)],
        wal_dir: impl AsRef<Path>,
        network: RecoveryNetworkPolicy,
        import: Option<RecoveryImport>,
    ) -> Result<Self, StateError> {
        let wal_dir = wal_dir.as_ref();
        fs::create_dir_all(wal_dir).map_err(|error| {
            StateError::PersistenceError(format!("failed to create {wal_dir:?}: {error}"))
        })?;
        let marker_path = wal_dir.join(ACTIVE_RECOVERY_MARKER);
        let existing_marker = read_active_marker(&marker_path)?;

        let approved_hash = match (existing_marker, import.as_ref()) {
            (Some(active), Some(requested)) if active != requested.approved_manifest_hash => {
                return Err(StateError::PersistenceError(format!(
                    "data directory is already bound to recovery manifest {active}; refusing requested {}",
                    requested.approved_manifest_hash
                )));
            }
            (Some(active), _) => Some(active),
            (None, Some(requested)) => Some(requested.approved_manifest_hash),
            (None, None) => None,
        };

        let Some(approved_hash) = approved_hash else {
            reject_orphaned_recovery_files(wal_dir)?;
            return Self::with_genesis_persistent(prefunded, wal_dir, network.genesis_hash);
        };

        let checkpoint_path = if let Some(requested) = import.as_ref() {
            requested.checkpoint_path.clone()
        } else {
            checkpoint_store_path(wal_dir, approved_hash)
        };
        let checkpoint = ArcCheckpoint::read_from(&checkpoint_path)?;
        let trust = RecoveryTrustRoot {
            network: network.clone(),
            approved_manifest_hash: approved_hash,
        };
        checkpoint.verify(&trust)?;

        // Verify the complete logical import in an isolated, non-persistent
        // StateDB before creating a marker or WAL in the active directory.
        let staged = StateDB::new();
        staged.install_verified_checkpoint(&checkpoint)?;
        if staged.height() != checkpoint.manifest.source_height + 1 {
            return Err(StateError::PersistenceError(
                "staged checkpoint did not activate exactly at H+1".into(),
            ));
        }

        let wal_path = wal_dir.join("state.wal");
        if existing_marker.is_none() {
            if wal_path.exists() {
                return Err(StateError::PersistenceError(format!(
                    "refusing ARCCHKPT activation over existing WAL {wal_path:?}; archive it and use a fresh data directory"
                )));
            }
            let stored_path = checkpoint_store_path(wal_dir, approved_hash);
            if !stored_path.exists() {
                write_checkpoint_atomically(&checkpoint, &stored_path)?;
            }
            write_marker_atomically(&marker_path, approved_hash)?;
        }
        Self::verify_or_create_genesis_binding(wal_dir, wal_path.exists(), network.genesis_hash)?;

        let recovered_entries = if wal_path.exists() {
            read_wal_strict(&wal_path).map_err(|error| {
                StateError::PersistenceError(format!(
                    "recovery WAL is not fully valid; refusing partial replay: {error}"
                ))
            })?
        } else {
            Vec::new()
        };
        let state = StateDB::with_persistence(&wal_path)?;
        state.install_verified_checkpoint(&checkpoint)?;
        let transition_height = checkpoint.manifest.source_height + 1;
        for entry in &recovered_entries {
            if entry.block_height <= transition_height {
                return Err(StateError::PersistenceError(format!(
                    "post-recovery WAL entry {} targets height {} at/before transition {}",
                    entry.sequence, entry.block_height, transition_height
                )));
            }
            state.apply_wal_op(&entry.op);
        }
        state.rebuild_transaction_indexes();
        state.verify_recovery_restart(&recovered_entries, &checkpoint)?;
        Ok(state)
    }

    fn rebuild_transaction_indexes(&self) {
        self.tx_index.clear();
        self.account_txs.clear();
        let mut transactions: Vec<_> = self
            .full_transactions
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        transactions.sort_by_key(|transaction| {
            self.receipts
                .get(&transaction.hash.0)
                .map(|receipt| (receipt.block_height, receipt.index))
                .unwrap_or((u64::MAX, u32::MAX))
        });
        for transaction in transactions {
            if let Some(receipt) = self.receipts.get(&transaction.hash.0) {
                self.tx_index
                    .insert(transaction.hash.0, (receipt.block_height, receipt.index));
            }
            self.index_account_tx(&transaction);
        }
    }

    fn verify_recovery_restart(
        &self,
        entries: &[crate::WalEntry],
        checkpoint: &ArcCheckpoint,
    ) -> Result<(), StateError> {
        let transition_height = checkpoint.manifest.source_height + 1;
        let mut last_checkpoint = None;
        let mut last_block_height = transition_height;
        for entry in entries {
            match &entry.op {
                WalOp::Checkpoint(root) => last_checkpoint = Some((entry.block_height, *root)),
                WalOp::SetBlock(height, _) => last_block_height = last_block_height.max(*height),
                _ => {}
            }
        }
        if !entries.is_empty() {
            let Some((checkpoint_height, expected_root)) = last_checkpoint else {
                return Err(StateError::PersistenceError(
                    "post-recovery WAL has no complete block checkpoint".into(),
                ));
            };
            if checkpoint_height != last_block_height || self.height() != last_block_height {
                return Err(StateError::PersistenceError(format!(
                    "post-recovery WAL is not block-atomic: checkpoint={checkpoint_height}, block={last_block_height}, state={}",
                    self.height()
                )));
            }
            let actual = self.compute_state_root();
            if actual != expected_root {
                return Err(StateError::PersistenceError(format!(
                    "post-recovery WAL state root mismatch: checkpoint {expected_root}, replayed {actual}"
                )));
            }
        }
        let mut previous = self
            .get_block(transition_height)
            .ok_or_else(|| StateError::PersistenceError("transition block missing".into()))?;
        for height in (transition_height + 1)..=self.height() {
            let block = self.get_block(height).ok_or_else(|| {
                StateError::PersistenceError(format!(
                    "canonical block {height} missing after replay"
                ))
            })?;
            if block.header.parent_hash != previous.hash
                || block.hash != Block::compute_hash(&block.header)
            {
                return Err(StateError::PersistenceError(format!(
                    "canonical block linkage/hash invalid at height {height}"
                )));
            }
            previous = block;
        }
        Ok(())
    }
}

fn checkpoint_store_path(wal_dir: &Path, hash: Hash256) -> PathBuf {
    wal_dir.join(format!(
        "{ACTIVE_RECOVERY_PREFIX}{}{ACTIVE_RECOVERY_SUFFIX}",
        hash.to_hex()
    ))
}

fn read_active_marker(path: &Path) -> Result<Option<Hash256>, StateError> {
    match fs::read_to_string(path) {
        Ok(value) => Hash256::from_hex(value.trim())
            .map(Some)
            .map_err(|_| StateError::PersistenceError(format!("invalid recovery marker {path:?}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StateError::PersistenceError(format!(
            "failed to read recovery marker {path:?}: {error}"
        ))),
    }
}

fn reject_orphaned_recovery_files(wal_dir: &Path) -> Result<(), StateError> {
    let has_orphan = fs::read_dir(wal_dir)
        .map_err(|error| StateError::PersistenceError(error.to_string()))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .any(|name| {
            name.starts_with(ACTIVE_RECOVERY_PREFIX) && name.ends_with(ACTIVE_RECOVERY_SUFFIX)
        });
    if has_orphan {
        return Err(StateError::PersistenceError(
            "ARCCHKPT file exists without recovery.active; refusing ambiguous startup".into(),
        ));
    }
    Ok(())
}

fn write_checkpoint_atomically(
    checkpoint: &ArcCheckpoint,
    destination: &Path,
) -> Result<(), RecoveryError> {
    let temporary = destination.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    checkpoint.write_to(&temporary)?;
    fs::rename(&temporary, destination)?;
    sync_parent(destination)?;
    Ok(())
}

fn write_marker_atomically(path: &Path, hash: Hash256) -> Result<(), RecoveryError> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    writeln!(file, "{}", hash.to_hex())?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_parent(path)?;
    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), RecoveryError> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let serial = NEXT_DIR.fetch_add(1, AtomicOrdering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "arc-recovery-{label}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn validators() -> (Vec<KeyPair>, Vec<RecoveryValidator>) {
        let keys: Vec<_> = (0..6).map(|_| KeyPair::generate_ed25519()).collect();
        let validators = keys
            .iter()
            .map(|key| RecoveryValidator {
                address: key.address(),
                public_key: key.public_key_bytes().try_into().unwrap(),
                stake: 5_000_000,
            })
            .collect();
        (keys, validators)
    }

    fn bond_source_stake(state: &StateDB, validators: &[RecoveryValidator]) {
        let mut total = 0u64;
        for validator in validators {
            let mut account = state
                .get_account(&validator.address)
                .unwrap_or_else(|| Account::new(validator.address, 0));
            account.staked_balance = validator.stake;
            state.accounts.insert(validator.address.0, account);
            state.dirty_accounts.insert(validator.address.0);
            state
                .validators
                .insert(validator.address.0, validator.stake);
            total = total.checked_add(validator.stake).unwrap();
        }
        state.staking_pool.store(total, Ordering::Release);
        let reserve = recovery_stake_reserve_address();
        state
            .accounts
            .insert(reserve.0, Account::new(reserve, total.saturating_mul(2)));
        state.dirty_accounts.insert(reserve.0);
    }

    fn checkpoint() -> (ArcCheckpoint, Vec<KeyPair>, RecoveryNetworkPolicy) {
        let (keys, validators) = validators();
        let state =
            StateDB::with_genesis(&[(hash_bytes(b"alice"), 10_000), (hash_bytes(b"bob"), 20_000)]);
        bond_source_stake(&state, &validators);
        let genesis_hash = hash_bytes(b"approved-v3-genesis");
        let mut checkpoint = ArcCheckpoint::export_unsigned(
            &state,
            RecoveryExportSpec {
                chain_id: "0x415243".into(),
                genesis_hash,
                source_consensus_round: 9_000_000,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators: validators.clone(),
                community_rewards_v1_activation_height: Some(100),
                created_at_unix_ms: 1_787_777_000_000,
            },
        )
        .unwrap();
        for key in keys.iter().take(5) {
            checkpoint.add_signature(key).unwrap();
        }
        let policy = RecoveryNetworkPolicy {
            chain_id: "0x415243".into(),
            genesis_hash,
            recovery_epoch: 1,
            validator_set_id: 1,
            validators: validators
                .iter()
                .map(|validator| (validator.address, validator.stake))
                .collect(),
            community_rewards_v1_activation_height: Some(100),
        };
        (checkpoint, keys, policy)
    }

    #[test]
    fn manifest_is_content_addressed_and_five_of_six_signed() {
        let (checkpoint, _, policy) = checkpoint();
        let hash = checkpoint.manifest_hash();
        checkpoint
            .verify(&RecoveryTrustRoot {
                network: policy,
                approved_manifest_hash: hash,
            })
            .unwrap();
        assert_eq!(checkpoint.manifest_hash(), hash);
        assert_eq!(checkpoint.signatures.len(), 5);
        let transitioned = checkpoint
            .payload
            .transition_accounts(&checkpoint.manifest.validators)
            .unwrap();
        for validator in &checkpoint.manifest.validators {
            let account = transitioned
                .binary_search_by_key(&validator.address.0, |entry| entry.0.0)
                .expect("every replacement validator has an explicit account");
            assert_eq!(transitioned[account].1.staked_balance, validator.stake);
        }
    }

    #[test]
    fn validator_rotation_moves_bonded_stake_without_minting_supply() {
        let (_, source_validators) = validators();
        let (_, target_validators) = validators();
        let state = StateDB::with_genesis(&[(hash_bytes(b"holder"), 42_000)]);
        bond_source_stake(&state, &source_validators);
        let source_supply: u128 = state
            .export_recovery_payload()
            .accounts
            .iter()
            .map(|(_, account)| u128::from(account.balance) + u128::from(account.staked_balance))
            .sum();

        let checkpoint = ArcCheckpoint::export_unsigned(
            &state,
            RecoveryExportSpec {
                chain_id: "0x415243".into(),
                genesis_hash: hash_bytes(b"rotation-genesis"),
                source_consensus_round: 1,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators: target_validators.clone(),
                community_rewards_v1_activation_height: None,
                created_at_unix_ms: 1,
            },
        )
        .unwrap();
        let transitioned = checkpoint
            .payload
            .transition_accounts(&target_validators)
            .unwrap();
        let target_supply: u128 = transitioned
            .iter()
            .map(|(_, account)| u128::from(account.balance) + u128::from(account.staked_balance))
            .sum();
        assert_eq!(source_supply, target_supply);
        for validator in &source_validators {
            let index = transitioned
                .binary_search_by_key(&validator.address.0, |entry| entry.0.0)
                .unwrap();
            assert_eq!(transitioned[index].1.staked_balance, 0);
        }
        for validator in &target_validators {
            let index = transitioned
                .binary_search_by_key(&validator.address.0, |entry| entry.0.0)
                .unwrap();
            assert_eq!(transitioned[index].1.staked_balance, validator.stake);
        }
    }

    #[test]
    fn synthetic_legacy_validator_weights_are_bonded_from_system_reserve() {
        let (_, source_validators) = validators();
        let (_, target_validators) = validators();
        let total: u64 = source_validators
            .iter()
            .map(|validator| validator.stake)
            .sum();
        let reserve = recovery_stake_reserve_address();
        let state = StateDB::with_genesis(&[(reserve, total * 2), (hash_bytes(b"holder"), 42)]);
        for validator in &source_validators {
            state
                .validators
                .insert(validator.address.0, validator.stake);
        }
        state.staking_pool.store(total, Ordering::Release);
        let source_supply: u128 = state
            .export_recovery_payload()
            .accounts
            .iter()
            .map(|(_, account)| u128::from(account.balance) + u128::from(account.staked_balance))
            .sum();

        let checkpoint = ArcCheckpoint::export_unsigned(
            &state,
            RecoveryExportSpec {
                chain_id: "0x415243".into(),
                genesis_hash: hash_bytes(b"synthetic-stake-genesis"),
                source_consensus_round: 1,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators: target_validators.clone(),
                community_rewards_v1_activation_height: None,
                created_at_unix_ms: 1,
            },
        )
        .unwrap();
        let transitioned = checkpoint
            .payload
            .transition_accounts(&target_validators)
            .unwrap();
        let target_supply: u128 = transitioned
            .iter()
            .map(|(_, account)| u128::from(account.balance) + u128::from(account.staked_balance))
            .sum();
        assert_eq!(source_supply, target_supply);
        let reserve_index = transitioned
            .binary_search_by_key(&reserve.0, |entry| entry.0.0)
            .unwrap();
        assert_eq!(transitioned[reserve_index].1.balance, total);
        for validator in &target_validators {
            let index = transitioned
                .binary_search_by_key(&validator.address.0, |entry| entry.0.0)
                .unwrap();
            assert_eq!(transitioned[index].1.staked_balance, validator.stake);
        }
    }

    #[test]
    fn validator_rotation_rejects_any_change_to_total_bonded_stake() {
        let (_, source_validators) = validators();
        let (_, mut target_validators) = validators();
        target_validators[0].stake += 1;
        let state = StateDB::with_genesis(&[(hash_bytes(b"holder"), 42_000)]);
        bond_source_stake(&state, &source_validators);
        let error = ArcCheckpoint::export_unsigned(
            &state,
            RecoveryExportSpec {
                chain_id: "0x415243".into(),
                genesis_hash: hash_bytes(b"rotation-genesis"),
                source_consensus_round: 1,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators: target_validators,
                community_rewards_v1_activation_height: None,
                created_at_unix_ms: 1,
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must equal conserved source stake")
        );
    }

    #[test]
    fn verifier_rejects_payload_whose_legacy_root_differs_from_source() {
        let (mut checkpoint, _, _) = checkpoint();
        checkpoint.payload.accounts[0].1.balance += 1;
        checkpoint.manifest.payload_hash = checkpoint.payload.content_hash();
        let error = checkpoint.verify_content().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("replayed source state root mismatch")
        );
    }

    #[test]
    fn oversized_checkpoint_is_rejected_before_payload_allocation() {
        let dir = temp_dir("oversized");
        let path = dir.join("oversized.arcchkpt");
        let mut file = File::create(&path).unwrap();
        file.write_all(&ARCCHKPT_MAGIC).unwrap();
        file.write_all(&(ARCCHKPT_MAX_PAYLOAD_BYTES as u64 + 1).to_be_bytes())
            .unwrap();
        file.sync_all().unwrap();
        drop(file);

        let error = ArcCheckpoint::read_from(&path).unwrap_err();
        assert!(error.to_string().contains("format-v1 safety limit"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exporter_rejects_anchor_root_that_differs_from_replayed_state() {
        let state = StateDB::with_genesis(&[(hash_bytes(b"source-account"), 100)]);
        let mut anchor = state.get_block(0).unwrap();
        anchor.header.state_root = hash_bytes(b"forged-source-root");
        anchor.hash = Block::compute_hash(&anchor.header);
        state.blocks.insert(0, anchor);
        let (_, validators) = validators();
        bond_source_stake(&state, &validators);
        let error = ArcCheckpoint::export_unsigned(
            &state,
            RecoveryExportSpec {
                chain_id: "0x415243".into(),
                genesis_hash: hash_bytes(b"approved-v3-genesis"),
                source_consensus_round: 1,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators,
                community_rewards_v1_activation_height: None,
                created_at_unix_ms: 1,
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("differs from replayed state root")
        );
    }

    #[test]
    fn four_of_six_is_exactly_two_thirds_and_rejected() {
        let (mut checkpoint, _, policy) = checkpoint();
        checkpoint.signatures.pop();
        let error = checkpoint
            .verify(&RecoveryTrustRoot {
                network: policy,
                approved_manifest_hash: checkpoint.manifest_hash(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("insufficient signer identities"));
    }

    #[test]
    fn recovery_rejects_any_validator_set_other_than_six() {
        let (_, mut recovery_validators) = validators();
        recovery_validators.pop();
        let state = StateDB::with_genesis(&[(hash_bytes(b"holder"), 42)]);
        bond_source_stake(&state, &recovery_validators);

        let error = ArcCheckpoint::export_unsigned(
            &state,
            RecoveryExportSpec {
                chain_id: "0x415243".into(),
                genesis_hash: hash_bytes(b"wrong-validator-count-genesis"),
                source_consensus_round: 1,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators: recovery_validators,
                community_rewards_v1_activation_height: None,
                created_at_unix_ms: 1,
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("requires exactly 6 validators"),
            "{error}"
        );
    }

    #[test]
    fn every_payload_byte_is_bound_before_activation() {
        let (mut checkpoint, _, policy) = checkpoint();
        checkpoint.payload.accounts[0].1.balance += 1;
        let error = checkpoint
            .verify(&RecoveryTrustRoot {
                network: policy,
                approved_manifest_hash: checkpoint.manifest_hash(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("payload hash mismatch"));
    }

    #[test]
    fn full_state_root_commits_every_consensus_section_and_domain() {
        let (checkpoint, _, _) = checkpoint();
        let context = checkpoint.manifest.recovery_context();
        let activation = checkpoint.manifest.community_rewards_v1_activation_height;
        let baseline = checkpoint
            .payload
            .consensus_state_root(&context, activation);

        let mut account = checkpoint.payload.clone();
        account.accounts[0].1.balance += 1;
        let mut storage = checkpoint.payload.clone();
        storage
            .storage
            .push((hash_bytes(b"contract-storage"), Vec::new()));
        storage.storage.sort_by_key(|entry| entry.0.0);
        let mut contracts = checkpoint.payload.clone();
        contracts
            .contracts
            .push((hash_bytes(b"contract-code"), vec![1, 2, 3]));
        contracts.contracts.sort_by_key(|entry| entry.0.0);
        let mut identities = checkpoint.payload.clone();
        let identity_address = hash_bytes(b"identity");
        identities.identities.push((
            identity_address,
            Identity {
                address: identity_address,
                level: arc_types::IdentityLevel::Verified,
                attestor: hash_bytes(b"attestor"),
                proof_hash: hash_bytes(b"proof"),
                country_code: *b"US",
                attested_at: 1,
                expires_at: 0,
            },
        ));
        identities.identities.sort_by_key(|entry| entry.0.0);
        let mut validators = checkpoint.payload.clone();
        validators.validators[0].1 += 1;
        let mut staking_pool = checkpoint.payload.clone();
        staking_pool.staking_pool += 1;

        for changed in [
            account,
            storage,
            contracts,
            identities,
            validators,
            staking_pool,
        ] {
            assert_ne!(changed.consensus_state_root(&context, activation), baseline);
        }
        assert_ne!(
            checkpoint
                .payload
                .consensus_state_root(&context, activation.map(|height| height + 1)),
            baseline
        );
        let different_domain = RecoveryContext::new(
            &checkpoint.manifest.chain_id,
            checkpoint.manifest.genesis_hash,
            context.recovery_epoch + 1,
            context.validator_set_id + 1,
        );
        assert_ne!(
            checkpoint
                .payload
                .consensus_state_root(&different_domain, activation),
            baseline
        );
    }

    #[test]
    fn transition_is_dedicated_h_plus_one_block() {
        let (checkpoint, _, _) = checkpoint();
        let transition = checkpoint.manifest.transition_block().unwrap();
        assert_eq!(
            transition.header.height,
            checkpoint.manifest.source_height + 1
        );
        assert_eq!(
            transition.header.parent_hash,
            checkpoint.manifest.source_block_hash
        );
        assert_eq!(transition.header.tx_count, 0);
        assert!(transition.tx_hashes.is_empty());
        assert_eq!(transition.header.tx_root, checkpoint.manifest_hash());
        assert_eq!(
            transition.header.proof_hash,
            checkpoint.manifest.transition_commitment()
        );
        assert_eq!(
            transition.header.protocol_version,
            RECOVERY_PROTOCOL_VERSION
        );
    }

    #[test]
    fn import_is_verified_then_restart_reverifies_same_checkpoint() {
        let (checkpoint, _, policy) = checkpoint();
        let source_dir = temp_dir("source");
        let active_dir = temp_dir("active");
        let source = source_dir.join("candidate.arcchkpt");
        checkpoint.write_to(&source).unwrap();
        let approved = checkpoint.manifest_hash();

        let state = StateDB::with_genesis_persistent_recovery(
            &[],
            &active_dir,
            policy.clone(),
            Some(RecoveryImport {
                checkpoint_path: source,
                approved_manifest_hash: approved,
            }),
        )
        .unwrap();
        assert_eq!(state.height(), checkpoint.manifest.source_height + 1);
        assert_eq!(state.recovery_manifest_hash(), Some(approved));
        assert_eq!(state.get_state_root(), checkpoint.manifest.full_state_root);
        drop(state);

        let restarted =
            StateDB::with_genesis_persistent_recovery(&[], &active_dir, policy, None).unwrap();
        assert_eq!(restarted.recovery_manifest_hash(), Some(approved));
        assert_eq!(
            restarted
                .get_block(checkpoint.manifest.source_height + 1)
                .unwrap()
                .header
                .parent_hash,
            checkpoint.manifest.source_block_hash
        );

        fs::remove_dir_all(source_dir).unwrap();
        fs::remove_dir_all(active_dir).unwrap();
    }

    #[test]
    fn restart_fails_closed_on_torn_post_recovery_wal() {
        let (checkpoint, _, policy) = checkpoint();
        let source_dir = temp_dir("torn-source");
        let active_dir = temp_dir("torn-active");
        let source = source_dir.join("candidate.arcchkpt");
        checkpoint.write_to(&source).unwrap();
        let approved = checkpoint.manifest_hash();
        let state = StateDB::with_genesis_persistent_recovery(
            &[],
            &active_dir,
            policy.clone(),
            Some(RecoveryImport {
                checkpoint_path: source,
                approved_manifest_hash: approved,
            }),
        )
        .unwrap();
        drop(state);
        OpenOptions::new()
            .append(true)
            .open(active_dir.join("state.wal"))
            .unwrap()
            .write_all(&64u32.to_le_bytes())
            .unwrap();

        let error = StateDB::with_genesis_persistent_recovery(&[], &active_dir, policy, None)
            .err()
            .expect("torn recovery WAL must fail startup");
        assert!(
            error
                .to_string()
                .contains("refusing partial replay: truncated WAL frame payload")
        );
        fs::remove_dir_all(source_dir).unwrap();
        fs::remove_dir_all(active_dir).unwrap();
    }

    #[test]
    fn adaptive_h_plus_two_block_has_correct_wal_height_and_survives_restart() {
        let sender = KeyPair::generate_ed25519();
        let recipient = hash_bytes(b"post-recovery-recipient");
        let source = StateDB::with_genesis(&[(sender.address(), 10_000), (recipient, 0)]);
        let (validator_keys, validators) = validators();
        bond_source_stake(&source, &validators);
        let genesis_hash = hash_bytes(b"post-recovery-genesis");
        let mut checkpoint = ArcCheckpoint::export_unsigned(
            &source,
            RecoveryExportSpec {
                chain_id: "0x415243".into(),
                genesis_hash,
                source_consensus_round: 100,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators: validators.clone(),
                community_rewards_v1_activation_height: None,
                created_at_unix_ms: 1_787_777_000_000,
            },
        )
        .unwrap();
        for key in validator_keys.iter().take(5) {
            checkpoint.add_signature(key).unwrap();
        }
        let policy = RecoveryNetworkPolicy {
            chain_id: "0x415243".into(),
            genesis_hash,
            recovery_epoch: 1,
            validator_set_id: 1,
            validators: validators
                .iter()
                .map(|validator| (validator.address, validator.stake))
                .collect(),
            community_rewards_v1_activation_height: None,
        };
        let source_dir = temp_dir("post-block-source");
        let active_dir = temp_dir("post-block-active");
        let package = source_dir.join("candidate.arcchkpt");
        checkpoint.write_to(&package).unwrap();
        let approved = checkpoint.manifest_hash();
        let state = StateDB::with_genesis_persistent_recovery(
            &[],
            &active_dir,
            policy.clone(),
            Some(RecoveryImport {
                checkpoint_path: package,
                approved_manifest_hash: approved,
            }),
        )
        .unwrap();
        let mut transaction = Transaction::new_transfer(sender.address(), recipient, 250, 0);
        state.sign_transaction(&mut transaction, &sender).unwrap();
        let transaction_hash = transaction.hash;
        let (block, receipts) = state
            .execute_block_adaptive_at(
                &[transaction],
                validator_keys[0].address(),
                1_787_777_001_000,
            )
            .unwrap();
        assert!(receipts[0].success);
        assert_eq!(
            block.header.height,
            checkpoint.manifest.source_height + 2,
            "the first ordinary transaction block must be H+2 after the dedicated H+1 transition"
        );
        assert_eq!(block.header.protocol_version, RECOVERY_PROTOCOL_VERSION);
        let root = block.header.state_root;
        let height = block.header.height;
        drop(state);

        let restarted =
            StateDB::with_genesis_persistent_recovery(&[], &active_dir, policy, None).unwrap();
        assert_eq!(restarted.height(), height);
        assert_eq!(restarted.get_state_root(), root);
        assert!(restarted.get_transaction(&transaction_hash.0).is_some());
        let receipt = restarted.get_receipt(&transaction_hash.0).unwrap();
        assert!(receipt.success);
        assert_eq!(receipt.block_height, height);
        assert_eq!(restarted.get_account(&sender.address()).unwrap().nonce, 1);
        assert_eq!(restarted.get_account(&recipient).unwrap().balance, 250);

        fs::remove_dir_all(source_dir).unwrap();
        fs::remove_dir_all(active_dir).unwrap();
    }

    fn legacy_snapshot_fixture(
        label: &str,
    ) -> (PathBuf, PathBuf, Snapshot, Address, Address, Hash256) {
        let data_dir = temp_dir(label);
        let snapshot_path = data_dir.with_extension("snapshot.lz4");
        let sender = hash_bytes(format!("{label}-sender").as_bytes());
        let recipient = hash_bytes(format!("{label}-recipient").as_bytes());
        let reference = StateDB::with_genesis(&[(sender, 1_000), (recipient, 0)]);
        let transaction = Transaction::new_transfer(sender, recipient, 125, 0);
        let (block, receipts) = reference.execute_block(&[transaction], sender).unwrap();
        assert!(receipts[0].success);
        let snapshot = reference.export_snapshot();
        assert_eq!(snapshot.block_height, 1);
        assert_eq!(snapshot.state_root, block.header.state_root);
        snapshot.write_to(&snapshot_path).unwrap();

        // Recreate the complete block/history boundary but deliberately omit
        // the recipient account from the WAL. This models the real legacy
        // failure mode: the block root is canonical, while WAL-only replay is
        // missing live state that the exact-height snapshot still commits.
        let writer = crate::WalWriter::new(data_dir.join("state.wal")).unwrap();
        writer.append(WalOp::SetAccount(sender, Account::new(sender, 1_000)), 0);
        writer.append(WalOp::SetBlock(0, Block::genesis()), 0);
        writer.append(
            WalOp::SetAccount(sender, reference.get_account(&sender).unwrap()),
            1,
        );
        writer.append(WalOp::SetBlock(1, block), 1);
        writer.append(WalOp::Checkpoint(snapshot.state_root), 1);
        writer.sync().unwrap();
        drop(writer);

        (
            data_dir,
            snapshot_path,
            snapshot.clone(),
            sender,
            recipient,
            snapshot.state_root,
        )
    }

    #[test]
    fn snapshot_assisted_legacy_loader_recovers_only_root_bound_live_sections() {
        let (data_dir, snapshot_path, _, sender, recipient, root) =
            legacy_snapshot_fixture("snapshot-assisted");
        let error = StateDB::load_legacy_recovery_source(&data_dir, Hash256::ZERO, true)
            .err()
            .expect("WAL-only replay with missing state must fail");
        assert!(
            error
                .to_string()
                .contains("provide an exact-height --snapshot")
        );

        let loaded = StateDB::load_legacy_recovery_source_with_snapshot(
            &data_dir,
            Hash256::ZERO,
            true,
            &snapshot_path,
        )
        .unwrap();
        assert_eq!(loaded.height(), 1);
        assert_eq!(loaded.get_state_root(), root);
        assert_eq!(loaded.get_account(&sender).unwrap().balance, 875);
        assert_eq!(loaded.get_account(&recipient).unwrap().balance, 125);

        fs::remove_dir_all(data_dir).unwrap();
        fs::remove_file(snapshot_path).unwrap();
    }

    #[test]
    fn snapshot_assisted_loader_discards_validly_framed_and_torn_uncommitted_suffix() {
        let (data_dir, snapshot_path, _, _, _, root) = legacy_snapshot_fixture("snapshot-trailing");
        let attacker = hash_bytes(b"snapshot-trailing-attacker");
        let writer = crate::WalWriter::new(data_dir.join("state.wal")).unwrap();
        writer.append(
            WalOp::SetAccount(attacker, Account::new(attacker, u64::MAX)),
            2,
        );
        writer.sync().unwrap();
        drop(writer);
        let mut wal = OpenOptions::new()
            .append(true)
            .open(data_dir.join("state.wal"))
            .unwrap();
        wal.write_all(&64u32.to_le_bytes()).unwrap();
        wal.write_all(b"physically-torn-tail").unwrap();
        wal.sync_all().unwrap();
        drop(wal);

        let loaded = StateDB::load_legacy_recovery_source_with_snapshot(
            &data_dir,
            Hash256::ZERO,
            true,
            &snapshot_path,
        )
        .unwrap();
        assert_eq!(loaded.height(), 1);
        assert_eq!(loaded.get_state_root(), root);
        assert!(loaded.get_account(&attacker).is_none());

        fs::remove_dir_all(data_dir).unwrap();
        fs::remove_file(snapshot_path).unwrap();
    }

    #[test]
    fn snapshot_assisted_loader_rejects_complete_looking_forged_higher_boundary() {
        let (data_dir, snapshot_path, snapshot, _, _, _) =
            legacy_snapshot_fixture("snapshot-forged");
        let header = BlockHeader {
            height: 2,
            timestamp: 2,
            parent_hash: hash_bytes(b"forged-parent"),
            tx_root: Hash256::ZERO,
            state_root: snapshot.state_root,
            proof_hash: Hash256::ZERO,
            tx_count: 0,
            producer: hash_bytes(b"forged-producer"),
            protocol_version: ProtocolVersion::new(0, 1, 0),
            state_diff: None,
        };
        let forged = Block::new(header, Vec::new());
        let writer = crate::WalWriter::new(data_dir.join("state.wal")).unwrap();
        writer.append(WalOp::SetBlock(2, forged), 2);
        writer.append(WalOp::Checkpoint(snapshot.state_root), 2);
        writer.sync().unwrap();
        drop(writer);

        let error = StateDB::load_legacy_recovery_source_with_snapshot(
            &data_dir,
            Hash256::ZERO,
            true,
            &snapshot_path,
        )
        .err()
        .expect("a complete-looking suffix cannot roll the snapshot boundary forward");
        assert!(error.to_string().contains("snapshot/WAL boundary mismatch"));

        fs::remove_dir_all(data_dir).unwrap();
        fs::remove_file(snapshot_path).unwrap();
    }

    #[test]
    fn snapshot_assisted_loader_rejects_uncommitted_storage_and_allocation_bombs() {
        let (data_dir, snapshot_path, mut snapshot, _, _, _) =
            legacy_snapshot_fixture("snapshot-storage");
        snapshot.storage.push((
            hash_bytes(b"uncommitted-contract"),
            vec![(hash_bytes(b"key"), b"value".to_vec())],
        ));
        snapshot.write_to(&snapshot_path).unwrap();
        let error = StateDB::load_legacy_recovery_source_with_snapshot(
            &data_dir,
            Hash256::ZERO,
            true,
            &snapshot_path,
        )
        .err()
        .expect("snapshot-only storage is not committed by the WAL");
        assert!(error.to_string().contains("storage differs"));

        let bomb = data_dir.with_extension("snapshot-bomb.lz4");
        let requested = (LEGACY_SNAPSHOT_MAX_BYTES as u32).saturating_add(1);
        fs::write(&bomb, requested.to_le_bytes()).unwrap();
        let error = read_legacy_recovery_snapshot(&bomb).unwrap_err();
        assert!(error.to_string().contains("decompressed bytes"));

        fs::remove_dir_all(data_dir).unwrap();
        fs::remove_file(snapshot_path).unwrap();
        fs::remove_file(bomb).unwrap();
    }

    #[test]
    fn legacy_export_loader_is_read_only_and_requires_explicit_unbound_override() {
        let source_dir = temp_dir("legacy-export-source");
        let genesis_hash = hash_bytes(b"legacy-export-genesis");
        let state = StateDB::with_genesis_persistent(
            &[(hash_bytes(b"legacy-account"), 123)],
            &source_dir,
            genesis_hash,
        )
        .unwrap();
        let (block, _) = state
            .execute_block_verified_at(&[], hash_bytes(b"legacy-producer"), 1)
            .unwrap();
        let root = block.header.state_root;
        drop(state);

        let loaded =
            StateDB::load_legacy_recovery_source(&source_dir, genesis_hash, false).unwrap();
        assert_eq!(loaded.height(), 1);
        assert_eq!(loaded.get_state_root(), root);
        assert!(!source_dir.join(ACTIVE_RECOVERY_MARKER).exists());

        fs::remove_file(source_dir.join("genesis.network-hash")).unwrap();
        let error = StateDB::load_legacy_recovery_source(&source_dir, genesis_hash, false)
            .err()
            .expect("unbound legacy source must fail closed");
        assert!(
            error
                .to_string()
                .contains("no authenticated genesis binding")
        );
        StateDB::load_legacy_recovery_source(&source_dir, genesis_hash, true).unwrap();

        fs::remove_dir_all(source_dir).unwrap();
    }

    #[test]
    fn existing_legacy_wal_blocks_checkpoint_activation() {
        let (checkpoint, _, policy) = checkpoint();
        let source_dir = temp_dir("legacy-source");
        let active_dir = temp_dir("legacy-active");
        let source = source_dir.join("candidate.arcchkpt");
        checkpoint.write_to(&source).unwrap();
        fs::write(active_dir.join("state.wal"), []).unwrap();
        let result = StateDB::with_genesis_persistent_recovery(
            &[],
            &active_dir,
            policy,
            Some(RecoveryImport {
                checkpoint_path: source,
                approved_manifest_hash: checkpoint.manifest_hash(),
            }),
        );
        let error = match result {
            Ok(_) => panic!("legacy WAL activation unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("refusing ARCCHKPT activation over existing WAL")
        );
        assert!(!active_dir.join(ACTIVE_RECOVERY_MARKER).exists());

        fs::remove_dir_all(source_dir).unwrap();
        fs::remove_dir_all(active_dir).unwrap();
    }
}
