use arc_crypto::Hash256;
use arc_crypto::signature::{KeyPair, Signature, SignatureError};
use serde::{Deserialize, Serialize};

use crate::account::Address;

pub const V3_SYSTEM_ACCOUNT_PREFIX: [u8; 15] = *b"ARC-V3-REWARD:\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum V3SystemAccountKind {
    CommunityRewardJob = 1,
    CommunityRewardCertificate = 2,
    RecoveryRewardProbe = 3,
    CommunityRewardBlockBudget = 4,
    CommunityRewardEpochBudget = 5,
    CommunityRewardWorkerBudget = 6,
    CommunityRewardCoordinatorBudget = 7,
    FaucetClaimMarker = 8,
}

pub fn v3_system_account_address(kind: V3SystemAccountKind, digest: &Hash256) -> Address {
    let mut bytes = [0u8; 32];
    bytes[..V3_SYSTEM_ACCOUNT_PREFIX.len()].copy_from_slice(&V3_SYSTEM_ACCOUNT_PREFIX);
    bytes[V3_SYSTEM_ACCOUNT_PREFIX.len()] = kind as u8;
    bytes[16..].copy_from_slice(&digest.as_ref()[..16]);
    Hash256(bytes)
}

pub fn is_v3_system_account(address: &Address) -> bool {
    address.as_ref()[..V3_SYSTEM_ACCOUNT_PREFIX.len()] == V3_SYSTEM_ACCOUNT_PREFIX
        && matches!(address.as_ref()[V3_SYSTEM_ACCOUNT_PREFIX.len()], 1..=8)
}

fn serialize_unverified<S>(_: &bool, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_bool(false)
}

fn deserialize_unverified<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let _ = bool::deserialize(deserializer)?;
    Ok(false)
}

pub mod gas_costs {
    pub const TX_BASE: u64 = 21_000;
    pub const TX_DATA_BYTE: u64 = 16;
    pub const TRANSFER: u64 = 21_000;
    pub const SETTLE: u64 = 25_000;
    pub const SWAP: u64 = 30_000;
    pub const STAKE: u64 = 25_000;
    pub const ESCROW: u64 = 35_000;
    pub const DEPLOY_CONTRACT: u64 = 53_000;
    pub const CONTRACT_CALL: u64 = 21_000;
    pub const REGISTER_AGENT: u64 = 30_000;
    pub const MULTI_SIG: u64 = 35_000;
    pub const JOIN_VALIDATOR: u64 = 30_000;
    pub const LEAVE_VALIDATOR: u64 = 25_000;
    pub const CLAIM_REWARDS: u64 = 25_000;
    pub const FAUCET_CLAIM: u64 = 25_000;
    pub const UPDATE_STAKE: u64 = 25_000;
    pub const GOVERNANCE: u64 = 50_000;
    pub const BRIDGE_LOCK: u64 = 50_000;
    pub const BRIDGE_MINT: u64 = 50_000;
    pub const BATCH_SETTLE_BASE: u64 = 30_000;
    pub const BATCH_SETTLE_PER_ENTRY: u64 = 500;
    pub const BATCH_SETTLE_MAX_ENTRIES: usize = 10_000;
    pub const BATCH_SETTLE: u64 = 30_000;
    pub const CHANNEL_OPEN: u64 = 40_000;
    pub const CHANNEL_CLOSE: u64 = 35_000;
    pub const CHANNEL_DISPUTE: u64 = 50_000;
    pub const SHARD_PROOF: u64 = 60_000;
    pub const INFERENCE_ATTESTATION: u64 = 50_000;
    pub const COMMUNITY_INFERENCE_REWARD: u64 = 50_000;
    pub const INFERENCE_CHALLENGE: u64 = 100_000;
    pub const INFERENCE_ESCROW_OPEN: u64 = 50_000;
    pub const INFERENCE_ESCROW_RELEASE: u64 = 80_000;
    pub const INFERENCE_ESCROW_REFUND: u64 = 40_000;
    pub const MODEL_REGISTRATION: u64 = 60_000;
    pub const MODEL_REQUEST: u64 = 50_000;
    pub const SHARD_COVERAGE_CLAIM: u64 = 60_000;
    pub const CAPACITY_ADVERTISEMENT: u64 = 40_000;
    pub const SHARD_ASSIGNMENT_PROPOSAL: u64 = 80_000;
    pub const TIER1_INFERENCE_REQUEST: u64 = 80_000;
    pub const TIER1_INFERENCE_VOTE: u64 = 30_000;
    pub const TIER1_INFERENCE_FINALIZE: u64 = 60_000;
    pub const NATIVE_INFERENCE_REQUEST: u64 = 90_000;
    pub const NATIVE_INFERENCE_FINALIZE: u64 = 90_000;
    pub const NATIVE_INFERENCE_REFUND: u64 = 45_000;
    pub const SLOAD: u64 = 200;
    pub const SSTORE: u64 = 5_000;
    pub const LOG: u64 = 375;
    pub const BLOCK_GAS_LIMIT: u64 = 30_000_000;
}

#[derive(Clone, Debug, Default)]
pub struct GasMeter {
    pub limit: u64,
    pub consumed: u64,
}

impl GasMeter {
    pub fn new(limit: u64) -> Self {
        Self { limit, consumed: 0 }
    }

    pub fn charge(&mut self, amount: u64) -> Result<(), GasError> {
        let new_consumed = self
            .consumed
            .checked_add(amount)
            .ok_or(GasError::Overflow)?;
        if new_consumed > self.limit {
            self.consumed = self.limit;
            return Err(GasError::OutOfGas {
                limit: self.limit,
                consumed: new_consumed,
            });
        }
        self.consumed = new_consumed;
        Ok(())
    }

    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.consumed)
    }

    pub fn is_exhausted(&self) -> bool {
        self.consumed >= self.limit
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GasError {
    OutOfGas { limit: u64, consumed: u64 },
    Overflow,
    BlockGasLimitExceeded { block_limit: u64, total: u64 },
}

impl std::fmt::Display for GasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GasError::OutOfGas { limit, consumed } => {
                write!(f, "out of gas: limit={}, consumed={}", limit, consumed)
            }
            GasError::Overflow => write!(f, "gas counter overflow"),
            GasError::BlockGasLimitExceeded { block_limit, total } => {
                write!(
                    f,
                    "block gas limit exceeded: limit={}, total={}",
                    block_limit, total
                )
            }
        }
    }
}

impl std::error::Error for GasError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum TxType {
    Transfer = 0x01,
    Settle = 0x02,
    Swap = 0x03,
    Escrow = 0x04,
    Stake = 0x05,
    WasmCall = 0x06,
    MultiSig = 0x07,
    DeployContract = 0x08,
    RegisterAgent = 0x09,
    JoinValidator = 0x0a,
    LeaveValidator = 0x0b,
    ClaimRewards = 0x0c,
    UpdateStake = 0x0d,
    Governance = 0x0e,
    BridgeLock = 0x0f,
    BridgeMint = 0x10,
    BatchSettle = 0x11,
    ChannelOpen = 0x12,
    ChannelClose = 0x13,
    ChannelDispute = 0x14,
    ShardProof = 0x15,
    InferenceAttestation = 0x16,
    InferenceChallenge = 0x17,
    InferenceRegister = 0x18,
    InferenceEscrowOpen = 0x19,
    InferenceEscrowRelease = 0x1a,
    InferenceEscrowRefund = 0x1b,
    ModelRegistration = 0x1c,
    ModelRequest = 0x1d,
    ShardCoverageClaim = 0x1e,
    CapacityAdvertisement = 0x1f,
    ShardAssignmentProposal = 0x20,
    FaucetClaim = 0x21,
    InferenceRequest = 0x22,
    InferenceVote = 0x23,
    InferenceFinalize = 0x24,
    CommunityInferenceReward = 0x25,
    NativeInferenceRequest = 0x26,
    NativeInferenceFinalize = 0x27,
    NativeInferenceRefund = 0x28,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    pub tx_type: TxType,
    pub from: Address,
    pub nonce: u64,
    pub body: TxBody,
    pub fee: u64,
    #[serde(default)]
    pub gas_limit: u64,
    pub hash: Hash256,
    pub signature: Signature,
    #[serde(
        default,
        serialize_with = "serialize_unverified",
        deserialize_with = "deserialize_unverified"
    )]
    pub sig_verified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TxBody {
    Transfer(TransferBody),
    Settle(SettleBody),
    Swap(SwapBody),
    Escrow(EscrowBody),
    Stake(StakeBody),
    WasmCall(WasmCallBody),
    MultiSig(MultiSigBody),
    DeployContract(DeployBody),
    RegisterAgent(RegisterBody),
    JoinValidator(JoinValidatorBody),
    LeaveValidator,
    ClaimRewards,
    UpdateStake(UpdateStakeBody),
    Governance(GovernanceBody),
    BridgeLock(BridgeLockBody),
    BridgeMint(BridgeMintBody),
    BatchSettle(BatchSettleBody),
    ChannelOpen(ChannelOpenBody),
    ChannelClose(ChannelCloseBody),
    ChannelDispute(ChannelDisputeBody),
    ShardProof(ShardProofBody),
    InferenceAttestation(InferenceAttestationBody),
    InferenceChallenge(InferenceChallengeBody),
    InferenceRegister(InferenceRegisterBody),
    InferenceEscrowOpen(InferenceEscrowOpenBody),
    InferenceEscrowRelease(InferenceEscrowReleaseBody),
    InferenceEscrowRefund(InferenceEscrowRefundBody),
    ModelRegistration(ModelRegistrationBody),
    ModelRequest(ModelRequestBody),
    ShardCoverageClaim(ShardCoverageClaimBody),
    CapacityAdvertisement(CapacityAdvertisementBody),
    ShardAssignmentProposal(ShardAssignmentProposalBody),
    FaucetClaim(FaucetClaimBody),
    InferenceRequest(InferenceRequestBody),
    InferenceVote(InferenceVoteBody),
    InferenceFinalize(InferenceFinalizeBody),
    CommunityInferenceReward(CommunityInferenceRewardBody),
    NativeInferenceRequest(NativeInferenceRequestBody),
    NativeInferenceFinalize(NativeInferenceFinalizeBody),
    NativeInferenceRefund(NativeInferenceRefundBody),
}

impl TxBody {
    pub const fn tx_type(&self) -> TxType {
        match self {
            Self::Transfer(_) => TxType::Transfer,
            Self::Settle(_) => TxType::Settle,
            Self::Swap(_) => TxType::Swap,
            Self::Escrow(_) => TxType::Escrow,
            Self::Stake(_) => TxType::Stake,
            Self::WasmCall(_) => TxType::WasmCall,
            Self::MultiSig(_) => TxType::MultiSig,
            Self::DeployContract(_) => TxType::DeployContract,
            Self::RegisterAgent(_) => TxType::RegisterAgent,
            Self::JoinValidator(_) => TxType::JoinValidator,
            Self::LeaveValidator => TxType::LeaveValidator,
            Self::ClaimRewards => TxType::ClaimRewards,
            Self::UpdateStake(_) => TxType::UpdateStake,
            Self::Governance(_) => TxType::Governance,
            Self::BridgeLock(_) => TxType::BridgeLock,
            Self::BridgeMint(_) => TxType::BridgeMint,
            Self::BatchSettle(_) => TxType::BatchSettle,
            Self::ChannelOpen(_) => TxType::ChannelOpen,
            Self::ChannelClose(_) => TxType::ChannelClose,
            Self::ChannelDispute(_) => TxType::ChannelDispute,
            Self::ShardProof(_) => TxType::ShardProof,
            Self::InferenceAttestation(_) => TxType::InferenceAttestation,
            Self::InferenceChallenge(_) => TxType::InferenceChallenge,
            Self::InferenceRegister(_) => TxType::InferenceRegister,
            Self::InferenceEscrowOpen(_) => TxType::InferenceEscrowOpen,
            Self::InferenceEscrowRelease(_) => TxType::InferenceEscrowRelease,
            Self::InferenceEscrowRefund(_) => TxType::InferenceEscrowRefund,
            Self::ModelRegistration(_) => TxType::ModelRegistration,
            Self::ModelRequest(_) => TxType::ModelRequest,
            Self::ShardCoverageClaim(_) => TxType::ShardCoverageClaim,
            Self::CapacityAdvertisement(_) => TxType::CapacityAdvertisement,
            Self::ShardAssignmentProposal(_) => TxType::ShardAssignmentProposal,
            Self::FaucetClaim(_) => TxType::FaucetClaim,
            Self::InferenceRequest(_) => TxType::InferenceRequest,
            Self::InferenceVote(_) => TxType::InferenceVote,
            Self::InferenceFinalize(_) => TxType::InferenceFinalize,
            Self::CommunityInferenceReward(_) => TxType::CommunityInferenceReward,
            Self::NativeInferenceRequest(_) => TxType::NativeInferenceRequest,
            Self::NativeInferenceFinalize(_) => TxType::NativeInferenceFinalize,
            Self::NativeInferenceRefund(_) => TxType::NativeInferenceRefund,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferBody {
    pub to: Address,
    pub amount: u64,
    pub amount_commitment: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettleBody {
    pub agent_id: Address,
    pub service_hash: Hash256,
    pub amount: u64,
    pub usage_units: u64,
    pub amount_commitment: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SwapBody {
    pub counterparty: Address,
    pub offer_amount: u64,
    pub receive_amount: u64,
    pub offer_asset: Hash256,
    pub receive_asset: Hash256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EscrowBody {
    pub beneficiary: Address,
    pub amount: u64,
    pub conditions_hash: Hash256,
    pub is_create: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StakeBody {
    pub amount: u64,
    pub is_stake: bool,
    pub validator: Address,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WasmCallBody {
    pub contract: Address,
    pub function: String,
    pub calldata: Vec<u8>,
    pub value: u64,
    pub gas_limit: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MultiSigBody {
    pub inner_tx: Box<TxBody>,
    pub signers: Vec<Address>,
    pub threshold: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployBody {
    pub bytecode: Vec<u8>,
    pub constructor_args: Vec<u8>,
    pub state_rent_deposit: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegisterBody {
    pub agent_name: String,
    pub capabilities: Vec<u8>,
    pub endpoint: String,
    pub protocol: Hash256,
    pub metadata: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JoinValidatorBody {
    pub pubkey: [u8; 32],
    pub initial_stake: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateStakeBody {
    pub new_stake: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GovernanceBody {
    pub proposal_id: u64,
    pub action: GovernanceAction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum GovernanceAction {
    Execute,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeLockBody {
    pub destination_chain: u32,
    pub destination_address: [u8; 32],
    pub amount: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeMintBody {
    pub source_chain: u32,
    pub source_tx_hash: Hash256,
    pub recipient: Address,
    pub amount: u64,
    pub merkle_proof: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchSettleBody {
    pub entries: Vec<SettleEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettleEntry {
    pub agent_id: Address,
    pub service_hash: Hash256,
    pub amount: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelOpenBody {
    pub channel_id: Hash256,
    pub counterparty: Address,
    pub deposit: u64,
    pub timeout_blocks: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelCloseBody {
    pub channel_id: Hash256,
    pub opener_balance: u64,
    pub counterparty_balance: u64,
    pub counterparty_sig: Vec<u8>,
    pub state_nonce: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelDisputeBody {
    pub channel_id: Hash256,
    pub opener_balance: u64,
    pub counterparty_balance: u64,
    pub other_party_sig: Vec<u8>,
    pub state_nonce: u64,
    pub challenge_period: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShardProofBody {
    pub shard_id: u16,
    pub block_height: u64,
    pub block_hash: Hash256,
    pub prev_state_root: Hash256,
    pub post_state_root: Hash256,
    pub tx_count: u32,
    pub proof_data: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceAttestationBody {
    pub model_id: Hash256,
    pub input_hash: Hash256,
    pub output_hash: Hash256,
    pub challenge_period: u64,
    pub bond: u64,
    #[serde(skip)]
    pub beneficiary: Option<Address>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommunityInferenceRewardBody {
    pub chain_domain: Hash256,
    pub job_id: Hash256,
    pub coordinator: Address,
    pub assignment_epoch: Hash256,
    pub job_nonce: u64,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    pub transaction_domain: Hash256,
    pub worker: Address,
    pub model_id: Hash256,
    pub input_hash: Hash256,
    pub output_hash: Hash256,
    pub max_tokens: u32,
    pub expires_at_height: u64,
    pub worker_certificate: WorkerInferenceCertificate,
    pub validator_approvals: Vec<CommunityRewardValidatorApproval>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerInferenceCertificate {
    pub attestation_hash: Hash256,
    pub nonce: u64,
    pub challenge_period: u64,
    pub signature: Signature,
}

pub const MAX_COMMUNITY_REWARD_APPROVALS: usize = 64;

pub const COMMUNITY_REWARD_VALIDATOR_SET_SIZE: usize = 6;
pub const COMMUNITY_REWARD_APPROVALS_REQUIRED: usize = 5;
pub const COMMUNITY_REWARD_TERMINAL_RECEIPT_FIELDS: [&str; 25] = [
    "assignment_epoch",
    "block_hash",
    "block_height",
    "confirmed",
    "evidence_source",
    "included",
    "index",
    "input_hash",
    "job_id",
    "model_id",
    "output_hash",
    "receipt_url",
    "recovery_epoch",
    "reward_arc",
    "reward_base",
    "status",
    "submitted",
    "success",
    "transaction_domain",
    "tx_hash",
    "tx_type",
    "validator_approvals",
    "validator_set_commitment",
    "validator_set_id",
    "worker",
];
pub const COMMUNITY_REWARD_PENDING_RECEIPT_FIELDS: [&str; 23] = [
    "assignment_epoch",
    "block_hash",
    "block_height",
    "confirmed",
    "evidence_source",
    "included",
    "index",
    "job_id",
    "receipt_url",
    "recovery_epoch",
    "required_validator_approvals",
    "reward_arc",
    "reward_base",
    "status",
    "submitted",
    "success",
    "transaction_domain",
    "tx_hash",
    "tx_type",
    "validator_approvals",
    "validator_set_commitment",
    "validator_set_id",
    "worker",
];
pub const COMMUNITY_REWARD_PENDING_EVIDENCE: &str =
    "coordinator mempool submission only; no mined receipt";
pub const COMMUNITY_REWARD_SUCCESS_EVIDENCE: &str =
    "successful mined CommunityInferenceReward receipt";
pub const COMMUNITY_REWARD_UNSUCCESSFUL_EVIDENCE: &str = "no successful mined receipt";

pub const CANONICAL_REWARD_INFERENCE_PROFILE: &str =
    "INT8 integer (per-row, cross-platform deterministic)";
pub const COMMUNITY_REWARD_MIN_WORKER_STAKE: u64 = 0;

pub const RECOVERY_REWARD_PROBE_PREFIX: [u8; 16] = *b"ARC-RCV-PROBE1\0\0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRewardValidatorApproval {
    pub validator: Address,
    pub public_key: [u8; 32],
    pub signature_halves: [[u8; 32]; 2],
}

impl CommunityRewardValidatorApproval {
    pub fn from_ed25519_signature(validator: Address, signature: Signature) -> Option<Self> {
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

    pub fn as_signature(&self) -> Signature {
        let mut signature = Vec::with_capacity(64);
        signature.extend_from_slice(&self.signature_halves[0]);
        signature.extend_from_slice(&self.signature_halves[1]);
        Signature::Ed25519 {
            public_key: self.public_key,
            signature,
        }
    }
}

impl CommunityInferenceRewardBody {
    pub fn expected_chain_domain() -> Hash256 {
        arc_crypto::hash_bytes(b"arc-chain:0x415243:community-inference-reward:v1")
    }

    pub fn derive_job_id(
        coordinator: &Address,
        assignment_epoch: &Hash256,
        job_nonce: u64,
        model_id: &Hash256,
        input_hash: &Hash256,
        max_tokens: u32,
    ) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-community-job-v4");
        hasher.update(coordinator.as_ref());
        hasher.update(assignment_epoch.as_ref());
        hasher.update(&job_nonce.to_le_bytes());
        hasher.update(model_id.as_ref());
        hasher.update(input_hash.as_ref());
        hasher.update(&max_tokens.to_le_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn marker_address(chain_domain: &Hash256, job_id: &Hash256) -> Address {
        let mut bytes = Vec::with_capacity(23 + 64);
        bytes.extend_from_slice(b"arc-community-reward-v1");
        bytes.extend_from_slice(chain_domain.as_ref());
        bytes.extend_from_slice(job_id.as_ref());
        arc_crypto::hash_bytes(&bytes)
    }

    pub fn v3_marker_address(chain_domain: &Hash256, job_id: &Hash256) -> Address {
        v3_system_account_address(
            V3SystemAccountKind::CommunityRewardJob,
            &Self::marker_address(chain_domain, job_id),
        )
    }

    pub fn certificate_marker_address(
        chain_domain: &Hash256,
        worker: &Address,
        attestation_hash: &Hash256,
    ) -> Address {
        let mut bytes = Vec::with_capacity(34 + 96);
        bytes.extend_from_slice(b"arc-community-certificate-v1");
        bytes.extend_from_slice(chain_domain.as_ref());
        bytes.extend_from_slice(worker.as_ref());
        bytes.extend_from_slice(attestation_hash.as_ref());
        arc_crypto::hash_bytes(&bytes)
    }

    pub fn v3_certificate_marker_address(
        chain_domain: &Hash256,
        worker: &Address,
        attestation_hash: &Hash256,
    ) -> Address {
        v3_system_account_address(
            V3SystemAccountKind::CommunityRewardCertificate,
            &Self::certificate_marker_address(chain_domain, worker, attestation_hash),
        )
    }

    pub fn is_recovery_probe_assignment(assignment_epoch: &Hash256) -> bool {
        assignment_epoch.as_ref()[..RECOVERY_REWARD_PROBE_PREFIX.len()]
            == RECOVERY_REWARD_PROBE_PREFIX
    }

    pub fn recovery_probe_marker_address(
        chain_domain: &Hash256,
        assignment_epoch: &Hash256,
    ) -> Option<Address> {
        if !Self::is_recovery_probe_assignment(assignment_epoch) {
            return None;
        }
        let mut bytes = Vec::with_capacity(31 + 64);
        bytes.extend_from_slice(b"arc-recovery-reward-probe-marker-v1");
        bytes.extend_from_slice(chain_domain.as_ref());
        bytes.extend_from_slice(assignment_epoch.as_ref());
        Some(arc_crypto::hash_bytes(&bytes))
    }

    pub fn v3_recovery_probe_marker_address(
        chain_domain: &Hash256,
        assignment_epoch: &Hash256,
    ) -> Option<Address> {
        Self::recovery_probe_marker_address(chain_domain, assignment_epoch).map(|legacy| {
            v3_system_account_address(V3SystemAccountKind::RecoveryRewardProbe, &legacy)
        })
    }

    pub fn validator_approval_commitment(&self) -> Hash256 {
        let mut hasher =
            blake3::Hasher::new_derive_key("ARC-community-inference-reward-validator-approval-v1");
        hasher.update(self.chain_domain.as_ref());
        hasher.update(self.job_id.as_ref());
        hasher.update(self.coordinator.as_ref());
        hasher.update(self.assignment_epoch.as_ref());
        hasher.update(&self.job_nonce.to_le_bytes());
        hasher.update(&self.recovery_epoch.to_be_bytes());
        hasher.update(&self.validator_set_id.to_be_bytes());
        hasher.update(self.transaction_domain.as_ref());
        hasher.update(self.worker.as_ref());
        hasher.update(self.model_id.as_ref());
        hasher.update(self.input_hash.as_ref());
        hasher.update(self.output_hash.as_ref());
        hasher.update(&self.max_tokens.to_le_bytes());
        hasher.update(&self.expires_at_height.to_le_bytes());
        hasher.update(self.worker_certificate.attestation_hash.as_ref());
        hasher.update(&self.worker_certificate.nonce.to_le_bytes());
        hasher.update(&self.worker_certificate.challenge_period.to_le_bytes());
        let certificate_signature = bincode::serialize(&self.worker_certificate.signature)
            .expect("worker certificate signature is serializable");
        hasher.update(&(certificate_signature.len() as u64).to_le_bytes());
        hasher.update(&certificate_signature);
        hasher.update(&crate::economics::INFERENCE_ATTESTATION_REWARD.to_le_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn reconstruct_worker_attestation(&self) -> Transaction {
        Transaction {
            tx_type: TxType::InferenceAttestation,
            from: self.worker,
            nonce: self.worker_certificate.nonce,
            body: TxBody::InferenceAttestation(InferenceAttestationBody {
                model_id: self.model_id,
                input_hash: self.input_hash,
                output_hash: self.output_hash,
                challenge_period: self.worker_certificate.challenge_period,
                bond: 0,
                beneficiary: None,
            }),
            fee: 0,
            gas_limit: 0,
            hash: self.worker_certificate.attestation_hash,
            signature: self.worker_certificate.signature.clone(),
            sig_verified: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceChallengeBody {
    pub attestation_hash: Hash256,
    pub challenger_output_hash: Hash256,
    pub challenger_bond: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceRegisterBody {
    pub tier: u8,
    pub stake_bond: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceEscrowOpenBody {
    pub request_id: [u8; 32],
    pub model_id: Hash256,
    pub max_fee: u64,
    pub max_tokens: u32,
    pub timeout_blocks: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceEscrowReleaseBody {
    pub request_id: [u8; 32],
    pub payer: Address,
    pub model_id: Hash256,
    pub max_tokens: u32,
    pub timeout_blocks: u64,
    pub output_hash: Hash256,
    pub proposer: Address,
    pub replicas: Vec<Address>,
    pub observer_pool: Address,
    pub treasury: Address,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceEscrowRefundBody {
    pub request_id: [u8; 32],
    pub model_id: Hash256,
    pub max_tokens: u32,
    pub timeout_blocks: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceRequestBody {
    pub request_id: [u8; 32],
    pub model_id: Hash256,
    pub input_hash: Hash256,
    pub input_blob: Vec<u8>,
    pub max_tokens: u32,
    pub tier: u8,
    pub max_reward: u64,
    pub deadline_blocks: u64,
    pub committee_size: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceVoteBody {
    pub request_id: [u8; 32],
    pub output_hash: Hash256,
    pub output_blob: Option<Vec<u8>>,
    pub vrf_proof: Vec<u8>,
    pub committee_seed: Hash256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceFinalizeBody {
    pub request_id: [u8; 32],
}

pub const GGUF_LLAMA_I8_INTERLEAVED_ROPE_PROFILE_V1: &str =
    "arc.gguf-llama.i8-per-row.rope-interleaved.v1";

pub const GGUF_LLAMA_GREEDY_GENERATION_SEMANTICS_V1: &str = "arc.whole-model-generation.greedy.v1";
pub const GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1: &str = "arc.gguf-llama.spm-score-merge.v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeInferenceRequestBody {
    pub request: crate::inference_contract::InferenceRequest,
    pub input_blob: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeInferenceFinalizeBody {
    pub request_id: [u8; 32],
    pub certificate: crate::inference_contract::InferenceCertificate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeInferenceRefundBody {
    pub request_id: [u8; 32],
}

pub const TIER1_INPUT_BLOB_MAX: usize = 32 * 1024;

pub const TIER1_OUTPUT_BLOB_MAX: usize = 16 * 1024;

pub const TIER1_ANTI_SPAM_FEE: u64 = 1;

pub const TIER1_MAX_TOKENS: u32 = 2048;

pub const TIER1_MIN_DEADLINE_BLOCKS: u64 = 5;

pub const TIER1_MAX_DEADLINE_BLOCKS: u64 = 1000;

pub const TIER1_REWARD_SHARE_VOTERS_BPS: u64 = 7000;
pub const TIER1_REWARD_SHARE_REFUND_BPS: u64 = 2000;
pub const TIER1_REWARD_SHARE_TREASURY_BPS: u64 = 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRegistrationBody {
    pub model_id: Hash256,
    pub metadata_hash: Hash256,
    pub chunk_tree_root: Hash256,
    pub n_layers: u32,
    pub d_model: u32,
    pub quantization: String,
    pub registration_fee: u64,
    pub royalty_recipient: Address,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRequestBody {
    pub request_id: [u8; 32],
    pub model_id: Hash256,
    pub target_k_replication: u32,
    pub bond_per_layer_epoch: u64,
    pub max_wait_secs: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShardCoverageClaimBody {
    pub model_id: Hash256,
    pub node_pubkey: [u8; 32],
    pub ranges: Vec<(u32, u32)>,
    pub bond: u64,
    pub epoch_blocks: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapacityAdvertisementBody {
    pub node_pubkey: [u8; 32],
    pub ram_bytes: u64,
    pub vram_bytes: u64,
    pub bandwidth_mbps: u32,
    pub uptime_hint_mins: u32,
    pub stake: u64,
    pub region: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShardAssignmentProposalBody {
    pub epoch_blocks: u64,
    pub assignments: Vec<AssignmentEntry>,
    pub input_snapshot_hash: Hash256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssignmentEntry {
    pub node_pubkey: [u8; 32],
    pub model_id: Hash256,
    pub ranges: Vec<(u32, u32)>,
}

impl ModelRegistrationBody {
    pub fn registry_account(model_id: &Hash256) -> [u8; 32] {
        let mut buf = Vec::with_capacity(19 + 32);
        buf.extend_from_slice(b"arc-model-registry");
        buf.extend_from_slice(&model_id.0);
        arc_crypto::hash_bytes(&buf).0
    }

    pub fn metadata_commitment(
        n_layers: u32,
        d_model: u32,
        quantization: &str,
        chunk_tree_root: &Hash256,
        royalty_recipient: &Address,
    ) -> [u8; 32] {
        let mut buf = Vec::new();
        buf.extend_from_slice(&n_layers.to_le_bytes());
        buf.extend_from_slice(&d_model.to_le_bytes());
        buf.extend_from_slice(&(quantization.len() as u32).to_le_bytes());
        buf.extend_from_slice(quantization.as_bytes());
        buf.extend_from_slice(&chunk_tree_root.0);
        buf.extend_from_slice(&royalty_recipient.0);
        arc_crypto::hash_bytes(&buf).0
    }
}

impl ModelRequestBody {
    pub fn request_account(request_id: &[u8; 32]) -> [u8; 32] {
        let mut buf = Vec::with_capacity(18 + 32);
        buf.extend_from_slice(b"arc-model-request");
        buf.extend_from_slice(request_id);
        arc_crypto::hash_bytes(&buf).0
    }
}

impl ShardCoverageClaimBody {
    pub fn claim_account(model_id: &Hash256, node_pubkey: &[u8; 32]) -> [u8; 32] {
        let mut buf = Vec::with_capacity(20 + 32 + 32);
        buf.extend_from_slice(b"arc-shard-claim");
        buf.extend_from_slice(&model_id.0);
        buf.extend_from_slice(node_pubkey);
        arc_crypto::hash_bytes(&buf).0
    }
}

impl CapacityAdvertisementBody {
    pub fn capacity_account(node_pubkey: &[u8; 32]) -> [u8; 32] {
        let mut buf = Vec::with_capacity(20 + 32);
        buf.extend_from_slice(b"arc-node-capacity");
        buf.extend_from_slice(node_pubkey);
        arc_crypto::hash_bytes(&buf).0
    }
}

pub const MIN_MODEL_REGISTRATION_FEE: u64 = 1_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaucetClaimBody {
    pub recipient: Address,
    pub amount: u64,
}

impl FaucetClaimBody {
    pub fn marker_address(recipient: &Address) -> Address {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-faucet-recipient-marker-v1");
        hasher.update(recipient.as_ref());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn v3_marker_address(recipient: &Address) -> Address {
        v3_system_account_address(
            V3SystemAccountKind::FaucetClaimMarker,
            &Self::marker_address(recipient),
        )
    }
}

pub const FAUCET_CLAIM_MAX: u64 = crate::economics::ARC_BASE_UNITS;

pub fn faucet_pool_address() -> Address {
    arc_crypto::hash_bytes(&[0u8])
}

pub fn inference_reward_treasury_address() -> Address {
    arc_crypto::hash_bytes(&[1u8])
}

impl InferenceEscrowOpenBody {
    pub fn escrow_address(request_id: &[u8; 32]) -> [u8; 32] {
        let mut buf = Vec::with_capacity(24 + 32);
        buf.extend_from_slice(b"arc-inference-escrow");
        buf.extend_from_slice(request_id);
        arc_crypto::hash_bytes(&buf).0
    }

    pub fn metadata_commitment(
        payer: &Address,
        model_id: &Hash256,
        max_tokens: u32,
        timeout_blocks: u64,
    ) -> [u8; 32] {
        let mut buf = Vec::with_capacity(32 + 32 + 4 + 8);
        buf.extend_from_slice(&payer.0);
        buf.extend_from_slice(&model_id.0);
        buf.extend_from_slice(&max_tokens.to_le_bytes());
        buf.extend_from_slice(&timeout_blocks.to_le_bytes());
        arc_crypto::hash_bytes(&buf).0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventLog {
    pub address: Address,
    pub topics: Vec<Hash256>,
    pub data: Vec<u8>,
    pub block_height: u64,
    pub tx_hash: Hash256,
    pub log_index: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxReceipt {
    pub tx_hash: Hash256,
    pub block_height: u64,
    pub block_hash: Hash256,
    pub index: u32,
    pub success: bool,
    pub gas_used: u64,
    pub value_commitment: Option<[u8; 32]>,
    pub inclusion_proof: Option<Vec<u8>>,
    pub logs: Vec<EventLog>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactTransfer {
    pub from: Address,
    pub to: Address,
    pub amount: u64,
    pub nonce: u64,
    pub hash: Hash256,
}

pub const COMPACT_TX_SIZE: usize = 250;

impl CompactTransfer {
    pub fn new(from: Address, to: Address, amount: u64, nonce: u64) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
        hasher.update(&[TxType::Transfer as u8]);
        hasher.update(from.as_ref());
        hasher.update(&nonce.to_le_bytes());
        hasher.update(to.as_ref());
        hasher.update(&amount.to_le_bytes());
        let hash = Hash256(*hasher.finalize().as_bytes());
        Self {
            from,
            to,
            amount,
            nonce,
            hash,
        }
    }

    pub fn to_bytes(&self) -> [u8; COMPACT_TX_SIZE] {
        let mut buf = [0u8; COMPACT_TX_SIZE];
        buf[0] = TxType::Transfer as u8;
        buf[1..33].copy_from_slice(&self.from.0);
        buf[33..65].copy_from_slice(&self.to.0);
        buf[65..73].copy_from_slice(&self.amount.to_le_bytes());
        buf[73..81].copy_from_slice(&self.nonce.to_le_bytes());
        buf[81..113].copy_from_slice(&self.hash.0);
        buf
    }
}

impl Transaction {
    pub fn new_community_inference_reward(
        validator: Address,
        nonce: u64,
        body: CommunityInferenceRewardBody,
    ) -> Self {
        let mut tx = Self {
            tx_type: TxType::CommunityInferenceReward,
            from: validator,
            nonce,
            body: TxBody::CommunityInferenceReward(body),
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    pub fn new_faucet_claim(
        validator: Address,
        recipient: Address,
        amount: u64,
        nonce: u64,
    ) -> Self {
        let body = TxBody::FaucetClaim(FaucetClaimBody { recipient, amount });
        let mut tx = Self {
            tx_type: TxType::FaucetClaim,
            from: validator,
            nonce,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    pub fn new_transfer(from: Address, to: Address, amount: u64, nonce: u64) -> Self {
        let body = TxBody::Transfer(TransferBody {
            to,
            amount,
            amount_commitment: None,
        });
        let mut tx = Self {
            tx_type: TxType::Transfer,
            from,
            nonce,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    pub fn new_settle(
        from: Address,
        agent_id: Address,
        service_hash: Hash256,
        amount: u64,
        usage_units: u64,
        nonce: u64,
    ) -> Self {
        let body = TxBody::Settle(SettleBody {
            agent_id,
            service_hash,
            amount,
            usage_units,
            amount_commitment: None,
        });
        let mut tx = Self {
            tx_type: TxType::Settle,
            from,
            nonce,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    pub fn new_wasm_call(
        from: Address,
        contract: Address,
        function: String,
        calldata: Vec<u8>,
        value: u64,
        gas_limit: u64,
        nonce: u64,
    ) -> Self {
        let body = TxBody::WasmCall(WasmCallBody {
            contract,
            function,
            calldata,
            value,
            gas_limit,
        });
        let mut tx = Self {
            tx_type: TxType::WasmCall,
            from,
            nonce,
            body,
            fee: 0,
            gas_limit,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    pub fn new_deploy(
        from: Address,
        bytecode: Vec<u8>,
        constructor_args: Vec<u8>,
        state_rent_deposit: u64,
        fee: u64,
        gas_limit: u64,
        nonce: u64,
    ) -> Self {
        let body = TxBody::DeployContract(DeployBody {
            bytecode,
            constructor_args,
            state_rent_deposit,
        });
        let mut tx = Self {
            tx_type: TxType::DeployContract,
            from,
            nonce,
            body,
            fee,
            gas_limit,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_register_agent(
        from: Address,
        agent_name: String,
        capabilities: Vec<u8>,
        endpoint: String,
        protocol: Hash256,
        metadata: Vec<u8>,
        fee: u64,
        nonce: u64,
    ) -> Self {
        let body = TxBody::RegisterAgent(RegisterBody {
            agent_name,
            capabilities,
            endpoint,
            protocol,
            metadata,
        });
        let mut tx = Self {
            tx_type: TxType::RegisterAgent,
            from,
            nonce,
            body,
            fee,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    pub fn compute_hash(&self) -> Hash256 {
        let body_bytes = bincode::serialize(&self.body).expect("serializable");
        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
        hasher.update(&[self.tx_type as u8]);
        hasher.update(self.from.as_ref());
        hasher.update(&self.nonce.to_le_bytes());
        hasher.update(&body_bytes);
        hasher.update(&self.fee.to_le_bytes());
        hasher.update(&self.gas_limit.to_le_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn compute_hash_in_domain(&self, recovery_domain: &Hash256) -> Hash256 {
        let body_bytes = bincode::serialize(&self.body).expect("serializable");
        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v3");
        hasher.update(recovery_domain.as_ref());
        hasher.update(&[self.tx_type as u8]);
        hasher.update(self.from.as_ref());
        hasher.update(&self.nonce.to_le_bytes());
        hasher.update(&body_bytes);
        hasher.update(&self.fee.to_le_bytes());
        hasher.update(&self.gas_limit.to_le_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn sign(&mut self, keypair: &KeyPair) -> Result<(), SignatureError> {
        self.hash = self.compute_hash();
        self.signature = keypair.sign(&self.hash)?;
        Ok(())
    }

    pub fn sign_in_domain(
        &mut self,
        keypair: &KeyPair,
        recovery_domain: &Hash256,
    ) -> Result<(), SignatureError> {
        self.hash = self.compute_hash_in_domain(recovery_domain);
        self.signature = keypair.sign(&self.hash)?;
        self.sig_verified = false;
        Ok(())
    }

    pub fn verify_signature(&self) -> Result<(), SignatureError> {
        if self.tx_type != self.body.tx_type() {
            return Err(SignatureError::HashMismatch);
        }
        let expected = self.compute_hash();
        if expected != self.hash {
            return Err(SignatureError::HashMismatch);
        }
        self.signature.verify(&self.hash, &self.from)
    }

    pub fn verify_signature_in_domain(
        &self,
        recovery_domain: &Hash256,
    ) -> Result<(), SignatureError> {
        if self.tx_type != self.body.tx_type() {
            return Err(SignatureError::HashMismatch);
        }
        let expected = self.compute_hash_in_domain(recovery_domain);
        if self.hash != expected {
            return Err(SignatureError::HashMismatch);
        }
        self.signature.verify(&self.hash, &self.from)
    }

    pub fn is_unsigned(&self) -> bool {
        self.signature.is_null()
    }

    pub fn size(&self) -> usize {
        bincode::serialize(self).map(|b| b.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn test_addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    #[test]
    fn test_transfer() {
        let tx = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        assert_eq!(tx.tx_type, TxType::Transfer);
        assert_ne!(tx.hash, Hash256::ZERO);
        assert!(tx.is_unsigned());
    }

    #[test]
    fn unsigned_transfer_wire_format_stays_bincode_v1_compatible() {
        const LEGACY_WIRE_HEX: &str = "0000000048fc721fbbc172e0925fa27af1671de225ba927134802998b10a1568a188652b000000000000000000000000ab13bedf42e84bae0f7c62c7dd6a8ada571e8829bed6ea558217f0361b5e25d0e80300000000000000000000000000000000000000000000003296b0b8498403c1255885e25dbc016407ad8b83498b8233bd006c59a4ba892e00000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
        let tx = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        assert_eq!(
            hex::encode(bincode::serialize(&tx).unwrap()),
            LEGACY_WIRE_HEX
        );

        let historical = hex::decode(LEGACY_WIRE_HEX).unwrap();
        let decoded: Transaction = bincode::deserialize(&historical).unwrap();
        assert_eq!(bincode::serialize(&decoded).unwrap(), historical);
        assert_eq!(decoded.hash, tx.hash);
        assert_eq!(decoded.tx_type, tx.tx_type);
    }

    #[test]
    fn test_hash_deterministic() {
        let a = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        let b = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn test_hash_changes_with_nonce() {
        let a = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        let b = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 1);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn test_settle() {
        let tx = Transaction::new_settle(
            test_addr(1),
            test_addr(2),
            hash_bytes(b"api-service"),
            500,
            100,
            0,
        );
        assert_eq!(tx.tx_type, TxType::Settle);
        assert_eq!(tx.fee, 0, "settlements are always zero fee");
    }

    #[test]
    fn test_deploy_contract() {
        let tx = Transaction::new_deploy(
            test_addr(1),
            vec![0x00, 0x61, 0x73, 0x6d],
            vec![],
            1000,
            50,
            100_000,
            0,
        );
        assert_eq!(tx.tx_type, TxType::DeployContract);
        assert_eq!(tx.fee, 50);
        assert_eq!(tx.gas_limit, 100_000);
    }

    #[test]
    fn test_register_agent() {
        let tx = Transaction::new_register_agent(
            test_addr(1),
            "my-agent".to_string(),
            vec![0x01],
            "https://agent.arc.ai".to_string(),
            hash_bytes(b"arc-agent-v1"),
            vec![],
            10,
            0,
        );
        assert_eq!(tx.tx_type, TxType::RegisterAgent);
    }

    #[test]
    fn test_ed25519_sign_verify_transfer() {
        let kp = KeyPair::generate_ed25519();
        let address = kp.address();

        let mut tx = Transaction::new_transfer(address, test_addr(2), 1000, 0);
        assert!(tx.is_unsigned());

        tx.sign(&kp).expect("sign ok");
        assert!(!tx.is_unsigned());

        tx.verify_signature().expect("verify ok");
    }

    #[test]
    fn recovery_domain_prevents_cross_epoch_transaction_replay() {
        let keypair = KeyPair::generate_ed25519();
        let domain_a = hash_bytes(b"chain-A/recovery-1/set-1");
        let domain_b = hash_bytes(b"chain-A/recovery-2/set-2");
        let mut transaction = Transaction::new_transfer(keypair.address(), test_addr(2), 1_000, 0);

        transaction.sign_in_domain(&keypair, &domain_a).unwrap();

        transaction.verify_signature_in_domain(&domain_a).unwrap();
        assert!(transaction.verify_signature_in_domain(&domain_b).is_err());
        assert!(transaction.verify_signature().is_err());
    }

    #[test]
    fn test_secp256k1_sign_verify_transfer() {
        let kp = KeyPair::generate_secp256k1();
        let address = kp.address();

        let mut tx = Transaction::new_transfer(address, test_addr(2), 500, 1);
        tx.sign(&kp).expect("sign ok");
        tx.verify_signature().expect("verify ok");
    }

    #[test]
    fn test_signature_fails_after_tamper() {
        let kp = KeyPair::generate_ed25519();
        let address = kp.address();

        let mut tx = Transaction::new_transfer(address, test_addr(2), 1000, 0);
        tx.sign(&kp).expect("sign ok");

        tx.body = TxBody::Transfer(TransferBody {
            to: test_addr(2),
            amount: 9999,
            amount_commitment: None,
        });

        assert!(tx.verify_signature().is_err());
    }

    #[test]
    fn sig_verified_wire_field_is_always_reset_to_false() {
        let kp = KeyPair::generate_ed25519();
        let mut tx = Transaction::new_transfer(kp.address(), test_addr(2), 1, 0);
        tx.sign(&kp).unwrap();
        tx.sig_verified = true;

        let json = serde_json::to_value(&tx).unwrap();
        assert_eq!(json["sig_verified"], false);
        let mut malicious_json = json;
        malicious_json["sig_verified"] = serde_json::Value::Bool(true);
        let decoded_json: Transaction = serde_json::from_value(malicious_json).unwrap();
        assert!(!decoded_json.sig_verified);

        let wire = bincode::serialize(&tx).unwrap();
        let decoded_wire: Transaction = bincode::deserialize(&wire).unwrap();
        assert!(!decoded_wire.sig_verified);
        decoded_wire.verify_signature().unwrap();
    }

    #[test]
    fn signed_type_body_mismatch_is_rejected() {
        let kp = KeyPair::generate_ed25519();
        let mut tx = Transaction::new_transfer(kp.address(), test_addr(2), 1, 0);
        tx.tx_type = TxType::InferenceAttestation;
        tx.sign(&kp).unwrap();
        assert_ne!(tx.tx_type, tx.body.tx_type());
        assert!(tx.verify_signature().is_err());
    }

    #[test]
    fn test_wrong_signer_fails() {
        let kp = KeyPair::generate_ed25519();
        let wrong_kp = KeyPair::generate_ed25519();

        let mut tx = Transaction::new_transfer(kp.address(), test_addr(2), 1000, 0);
        tx.hash = tx.compute_hash();
        tx.signature = wrong_kp.sign(&tx.hash).expect("sign ok");

        assert!(tx.verify_signature().is_err());
    }

    #[test]
    fn test_unsigned_verify_fails() {
        let tx = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        assert!(tx.verify_signature().is_err());
    }

    #[test]
    fn test_fee_included_in_hash() {
        let mut a = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        a.fee = 10;
        let hash_a = a.compute_hash();

        let mut b = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        b.fee = 20;
        let hash_b = b.compute_hash();

        assert_ne!(
            hash_a, hash_b,
            "different fees must produce different hashes"
        );
    }

    #[test]
    fn test_gas_meter_basic() {
        let mut gas = GasMeter::new(100_000);
        assert_eq!(gas.remaining(), 100_000);
        assert!(!gas.is_exhausted());

        assert!(gas.charge(21_000).is_ok());
        assert_eq!(gas.consumed, 21_000);
        assert_eq!(gas.remaining(), 79_000);
    }

    #[test]
    fn test_gas_meter_out_of_gas() {
        let mut gas = GasMeter::new(10_000);
        assert!(gas.charge(10_001).is_err());
        assert!(gas.is_exhausted());
    }

    #[test]
    fn test_gas_meter_exact_limit() {
        let mut gas = GasMeter::new(21_000);
        assert!(gas.charge(21_000).is_ok());
        assert!(gas.is_exhausted());
        assert_eq!(gas.remaining(), 0);
    }

    #[test]
    fn test_gas_meter_multiple_charges() {
        let mut gas = GasMeter::new(50_000);
        assert!(gas.charge(21_000).is_ok());
        assert!(gas.charge(5_000).is_ok());
        assert!(gas.charge(5_000).is_ok());
        assert_eq!(gas.consumed, 31_000);
        assert!(gas.charge(20_000).is_err());
    }

    #[test]
    fn test_gas_costs_constants() {
        assert_eq!(gas_costs::TX_BASE, 21_000);
        const { assert!(gas_costs::DEPLOY_CONTRACT > gas_costs::TRANSFER) };
        const { assert!(gas_costs::BLOCK_GAS_LIMIT >= 30_000_000) };
    }

    fn tier1_request() -> InferenceRequestBody {
        InferenceRequestBody {
            request_id: [7u8; 32],
            model_id: hash_bytes(b"arc-32L-4096d-32h-32000v"),
            input_hash: hash_bytes(b"[INST] hello [/INST]"),
            input_blob: b"[INST] hello [/INST]".to_vec(),
            max_tokens: 32,
            tier: 1,
            max_reward: 10,
            deadline_blocks: 20,
            committee_size: 5,
        }
    }

    #[test]
    fn tier1_inference_request_roundtrip() {
        let body = TxBody::InferenceRequest(tier1_request());
        let bytes = bincode::serialize(&body).expect("serialize InferenceRequest");
        let back: TxBody = bincode::deserialize(&bytes).expect("deserialize InferenceRequest");
        match back {
            TxBody::InferenceRequest(b) => {
                assert_eq!(b.request_id, [7u8; 32]);
                assert_eq!(b.committee_size, 5);
                assert_eq!(b.max_tokens, 32);
                assert_eq!(b.max_reward, 10);
                assert_eq!(b.tier, 1);
                assert_eq!(b.input_blob.len(), b"[INST] hello [/INST]".len());
            }
            other => panic!("wrong variant after roundtrip: {:?}", other),
        }
    }

    #[test]
    fn tier1_inference_vote_roundtrip() {
        let body = TxBody::InferenceVote(InferenceVoteBody {
            request_id: [7u8; 32],
            output_hash: hash_bytes(b"hello world output"),
            output_blob: Some(b"hello world".to_vec()),
            vrf_proof: vec![0u8; 80],
            committee_seed: hash_bytes(b"block-hash-1234"),
        });
        let bytes = bincode::serialize(&body).expect("serialize InferenceVote");
        let back: TxBody = bincode::deserialize(&bytes).expect("deserialize InferenceVote");
        match back {
            TxBody::InferenceVote(b) => {
                assert_eq!(b.request_id, [7u8; 32]);
                assert_eq!(b.output_hash, hash_bytes(b"hello world output"));
                assert_eq!(b.output_blob.as_deref(), Some(&b"hello world"[..]));
                assert_eq!(b.vrf_proof.len(), 80);
            }
            other => panic!("wrong variant after roundtrip: {:?}", other),
        }
    }

    #[test]
    fn tier1_inference_finalize_roundtrip() {
        let body = TxBody::InferenceFinalize(InferenceFinalizeBody {
            request_id: [7u8; 32],
        });
        let bytes = bincode::serialize(&body).expect("serialize InferenceFinalize");
        let back: TxBody = bincode::deserialize(&bytes).expect("deserialize InferenceFinalize");
        match back {
            TxBody::InferenceFinalize(b) => assert_eq!(b.request_id, [7u8; 32]),
            other => panic!("wrong variant after roundtrip: {:?}", other),
        }
    }

    #[test]
    fn tier1_tx_type_discriminants_match_plan() {
        assert_eq!(TxType::InferenceRequest as u8, 0x22);
        assert_eq!(TxType::InferenceVote as u8, 0x23);
        assert_eq!(TxType::InferenceFinalize as u8, 0x24);
        assert_eq!(TxType::CommunityInferenceReward as u8, 0x25);
    }

    #[test]
    fn native_inference_tail_variants_roundtrip_without_legacy_renumbering() {
        use crate::inference_contract::{InferenceDomain, InferenceJob, InferenceRequest};
        let request = InferenceRequest {
            job: InferenceJob {
                version: crate::inference_contract::INFERENCE_CONTRACT_VERSION,
                domain: InferenceDomain {
                    chain_genesis: Hash256::ZERO,
                    recovery_epoch: 1,
                    validator_set_hash: Hash256::ZERO,
                },
                requester: test_addr(9),
                nonce: 4,
                model_hash: hash_bytes(b"model"),
                profile_hash: hash_bytes(b"profile"),
                input_hash: hash_bytes(b"input"),
                generation_hash: hash_bytes(b"generation"),
                assignment_hash: hash_bytes(b"assignment"),
                max_tokens: 32,
                max_output_bytes: 128,
                execution_price: 10,
                reserved_max_payment: 20,
                expires_at: 100,
            },
            requester_signature: arc_crypto::Signature::null(),
        };
        let bodies = [
            TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
                request: request.clone(),
                input_blob: b"input".to_vec(),
            }),
            TxBody::NativeInferenceFinalize(NativeInferenceFinalizeBody {
                request_id: request.job.request_id().0,
                certificate: crate::inference_contract::InferenceCertificate {
                    output: b"output".to_vec(),
                    votes: Vec::new(),
                },
            }),
            TxBody::NativeInferenceRefund(NativeInferenceRefundBody {
                request_id: request.job.request_id().0,
            }),
        ];
        for body in bodies {
            let bytes = bincode::serialize(&body).expect("serialize native body");
            let decoded: TxBody = bincode::deserialize(&bytes).expect("decode native body");
            assert_eq!(decoded.tx_type(), body.tx_type());
        }
        assert_eq!(TxType::NativeInferenceRequest as u8, 0x26);
        assert_eq!(TxType::NativeInferenceFinalize as u8, 0x27);
        assert_eq!(TxType::NativeInferenceRefund as u8, 0x28);
    }

    #[test]
    fn community_inference_reward_roundtrip_and_marker_are_stable() {
        let job_id = hash_bytes(b"job-1");
        let worker_key = KeyPair::generate_ed25519();
        let validator_key = KeyPair::generate_ed25519();
        let mut worker_attestation = Transaction {
            tx_type: TxType::InferenceAttestation,
            from: worker_key.address(),
            nonce: 0,
            body: TxBody::InferenceAttestation(InferenceAttestationBody {
                model_id: hash_bytes(b"model"),
                input_hash: hash_bytes(b"input"),
                output_hash: hash_bytes(b"output"),
                challenge_period: 100,
                bond: 0,
                beneficiary: None,
            }),
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        worker_attestation.sign(&worker_key).unwrap();
        let chain_domain = CommunityInferenceRewardBody::expected_chain_domain();
        let mut body = CommunityInferenceRewardBody {
            chain_domain,
            job_id,
            coordinator: validator_key.address(),
            assignment_epoch: hash_bytes(b"assignment-epoch"),
            job_nonce: 7,
            recovery_epoch: 3,
            validator_set_id: 11,
            transaction_domain: hash_bytes(b"recovery-domain"),
            worker: worker_key.address(),
            model_id: hash_bytes(b"model"),
            input_hash: hash_bytes(b"input"),
            output_hash: hash_bytes(b"output"),
            max_tokens: 32,
            expires_at_height: 123,
            worker_certificate: WorkerInferenceCertificate {
                attestation_hash: worker_attestation.hash,
                nonce: worker_attestation.nonce,
                challenge_period: 100,
                signature: worker_attestation.signature.clone(),
            },
            validator_approvals: Vec::new(),
        };
        let approval_commitment = body.validator_approval_commitment();
        body.validator_approvals.push(
            CommunityRewardValidatorApproval::from_ed25519_signature(
                validator_key.address(),
                validator_key.sign(&approval_commitment).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            approval_commitment,
            body.validator_approval_commitment(),
            "approval evidence itself is excluded from the common transcript"
        );
        let marker = CommunityInferenceRewardBody::marker_address(&chain_domain, &job_id);
        assert_eq!(
            marker,
            CommunityInferenceRewardBody::marker_address(&chain_domain, &job_id)
        );

        let encoded = bincode::serialize(&TxBody::CommunityInferenceReward(body.clone()))
            .expect("serialize reward");
        assert!(
            encoded.len() < 1024,
            "flat reward certificate must remain bounded; got {} bytes",
            encoded.len()
        );
        let decoded: TxBody = bincode::deserialize(&encoded).expect("deserialize reward");
        match decoded {
            TxBody::CommunityInferenceReward(got) => {
                assert_eq!(got.job_id, body.job_id);
                assert_eq!(got.worker, body.worker);
                assert_eq!(got.output_hash, body.output_hash);
                assert_eq!(got.max_tokens, 32);
                assert_eq!(got.expires_at_height, 123);
                assert_eq!(got.validator_approvals.len(), 1);
                got.validator_approvals[0]
                    .as_signature()
                    .verify(
                        &got.validator_approval_commitment(),
                        &validator_key.address(),
                    )
                    .unwrap();
                let rebuilt = got.reconstruct_worker_attestation();
                assert_eq!(rebuilt.hash, got.worker_certificate.attestation_hash);
                rebuilt.verify_signature().unwrap();
            }
            other => panic!("wrong variant after roundtrip: {:?}", other),
        }
    }

    #[test]
    fn recovery_probe_marker_is_namespace_bound_and_cross_coordinator() {
        let chain_domain = CommunityInferenceRewardBody::expected_chain_domain();
        let mut encoded = [0u8; 32];
        encoded[..RECOVERY_REWARD_PROBE_PREFIX.len()]
            .copy_from_slice(&RECOVERY_REWARD_PROBE_PREFIX);
        encoded[RECOVERY_REWARD_PROBE_PREFIX.len()..].fill(7);
        let probe_id = Hash256(encoded);
        assert!(CommunityInferenceRewardBody::is_recovery_probe_assignment(
            &probe_id
        ));
        let marker =
            CommunityInferenceRewardBody::recovery_probe_marker_address(&chain_domain, &probe_id)
                .expect("recovery namespace receives a marker");
        assert_eq!(
            Some(marker),
            CommunityInferenceRewardBody::recovery_probe_marker_address(&chain_domain, &probe_id,)
        );
        assert!(
            CommunityInferenceRewardBody::recovery_probe_marker_address(
                &chain_domain,
                &hash_bytes(b"ordinary-random-boot-epoch"),
            )
            .is_none()
        );
    }

    #[test]
    fn v3_dynamic_system_accounts_have_distinct_reserved_128_bit_namespaces() {
        let digest = hash_bytes(b"same transcript for every namespace");
        let kinds = [
            V3SystemAccountKind::CommunityRewardJob,
            V3SystemAccountKind::CommunityRewardCertificate,
            V3SystemAccountKind::RecoveryRewardProbe,
            V3SystemAccountKind::CommunityRewardBlockBudget,
            V3SystemAccountKind::CommunityRewardEpochBudget,
            V3SystemAccountKind::CommunityRewardWorkerBudget,
            V3SystemAccountKind::CommunityRewardCoordinatorBudget,
            V3SystemAccountKind::FaucetClaimMarker,
        ];
        let addresses: Vec<_> = kinds
            .iter()
            .map(|kind| v3_system_account_address(*kind, &digest))
            .collect();
        let unique: std::collections::HashSet<_> =
            addresses.iter().map(|address| address.0).collect();
        assert_eq!(unique.len(), kinds.len());
        for (address, kind) in addresses.iter().zip(kinds) {
            assert!(is_v3_system_account(address));
            assert_eq!(
                &address.as_ref()[..V3_SYSTEM_ACCOUNT_PREFIX.len()],
                &V3_SYSTEM_ACCOUNT_PREFIX
            );
            assert_eq!(address.as_ref()[V3_SYSTEM_ACCOUNT_PREFIX.len()], kind as u8);
            assert_eq!(&address.as_ref()[16..], &digest.as_ref()[..16]);
        }
        assert!(!is_v3_system_account(&digest));

        let chain_domain = CommunityInferenceRewardBody::expected_chain_domain();
        let job = hash_bytes(b"job");
        assert_ne!(
            CommunityInferenceRewardBody::marker_address(&chain_domain, &job),
            CommunityInferenceRewardBody::v3_marker_address(&chain_domain, &job),
            "legacy full-hash state keys remain distinct and unchanged"
        );
        assert!(is_v3_system_account(
            &CommunityInferenceRewardBody::v3_marker_address(&chain_domain, &job)
        ));
        assert!(is_v3_system_account(&FaucetClaimBody::v3_marker_address(
            &hash_bytes(b"recipient")
        )));
    }

    #[test]
    fn community_reward_approval_commitment_binds_every_semantic_and_certificate_field() {
        let worker_key = KeyPair::generate_ed25519();
        let mut worker_attestation = Transaction {
            tx_type: TxType::InferenceAttestation,
            from: worker_key.address(),
            nonce: 4,
            body: TxBody::InferenceAttestation(InferenceAttestationBody {
                model_id: hash_bytes(b"model"),
                input_hash: hash_bytes(b"input"),
                output_hash: hash_bytes(b"output"),
                challenge_period: 100,
                bond: 0,
                beneficiary: None,
            }),
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        worker_attestation.sign(&worker_key).unwrap();
        let body = CommunityInferenceRewardBody {
            chain_domain: CommunityInferenceRewardBody::expected_chain_domain(),
            job_id: hash_bytes(b"job"),
            coordinator: hash_bytes(b"coordinator"),
            assignment_epoch: hash_bytes(b"assignment-epoch"),
            job_nonce: 9,
            recovery_epoch: 3,
            validator_set_id: 11,
            transaction_domain: hash_bytes(b"recovery-domain"),
            worker: worker_key.address(),
            model_id: hash_bytes(b"model"),
            input_hash: hash_bytes(b"input"),
            output_hash: hash_bytes(b"output"),
            max_tokens: 32,
            expires_at_height: 123,
            worker_certificate: WorkerInferenceCertificate {
                attestation_hash: worker_attestation.hash,
                nonce: worker_attestation.nonce,
                challenge_period: 100,
                signature: worker_attestation.signature,
            },
            validator_approvals: Vec::new(),
        };
        let expected = body.validator_approval_commitment();

        macro_rules! assert_mutation_bound {
            ($label:literal, $mutation:expr) => {{
                let mut changed = body.clone();
                $mutation(&mut changed);
                assert_ne!(
                    changed.validator_approval_commitment(),
                    expected,
                    "{} was not bound by approval commitment",
                    $label
                );
            }};
        }
        assert_mutation_bound!("chain_domain", |b: &mut CommunityInferenceRewardBody| {
            b.chain_domain = hash_bytes(b"other-domain")
        });
        assert_mutation_bound!("job_id", |b: &mut CommunityInferenceRewardBody| {
            b.job_id = hash_bytes(b"other-job")
        });
        assert_mutation_bound!("coordinator", |b: &mut CommunityInferenceRewardBody| {
            b.coordinator = hash_bytes(b"other-coordinator")
        });
        assert_mutation_bound!(
            "assignment_epoch",
            |b: &mut CommunityInferenceRewardBody| {
                b.assignment_epoch = hash_bytes(b"other-epoch")
            }
        );
        assert_mutation_bound!("job_nonce", |b: &mut CommunityInferenceRewardBody| {
            b.job_nonce += 1
        });
        assert_mutation_bound!("recovery_epoch", |b: &mut CommunityInferenceRewardBody| {
            b.recovery_epoch += 1
        });
        assert_mutation_bound!(
            "validator_set_id",
            |b: &mut CommunityInferenceRewardBody| { b.validator_set_id += 1 }
        );
        assert_mutation_bound!(
            "transaction_domain",
            |b: &mut CommunityInferenceRewardBody| {
                b.transaction_domain = hash_bytes(b"other-recovery-domain")
            }
        );
        assert_mutation_bound!("worker", |b: &mut CommunityInferenceRewardBody| {
            b.worker = hash_bytes(b"other-worker")
        });
        assert_mutation_bound!("model_id", |b: &mut CommunityInferenceRewardBody| {
            b.model_id = hash_bytes(b"other-model")
        });
        assert_mutation_bound!("input_hash", |b: &mut CommunityInferenceRewardBody| {
            b.input_hash = hash_bytes(b"other-input")
        });
        assert_mutation_bound!("output_hash", |b: &mut CommunityInferenceRewardBody| {
            b.output_hash = hash_bytes(b"other-output")
        });
        assert_mutation_bound!("max_tokens", |b: &mut CommunityInferenceRewardBody| {
            b.max_tokens += 1
        });
        assert_mutation_bound!(
            "expires_at_height",
            |b: &mut CommunityInferenceRewardBody| { b.expires_at_height += 1 }
        );
        assert_mutation_bound!(
            "worker_certificate.attestation_hash",
            |b: &mut CommunityInferenceRewardBody| {
                b.worker_certificate.attestation_hash = hash_bytes(b"other-attestation")
            }
        );
        assert_mutation_bound!(
            "worker_certificate.nonce",
            |b: &mut CommunityInferenceRewardBody| { b.worker_certificate.nonce += 1 }
        );
        assert_mutation_bound!(
            "worker_certificate.challenge_period",
            |b: &mut CommunityInferenceRewardBody| { b.worker_certificate.challenge_period += 1 }
        );
        assert_mutation_bound!(
            "worker_certificate.signature",
            |b: &mut CommunityInferenceRewardBody| {
                b.worker_certificate.signature = Signature::null()
            }
        );
    }

    #[test]
    fn tier1_constants_sane() {
        const { assert!(TIER1_INPUT_BLOB_MAX > 0 && TIER1_INPUT_BLOB_MAX <= 1024 * 1024) };
        const { assert!(TIER1_OUTPUT_BLOB_MAX > 0) };
        assert!(TIER1_OUTPUT_BLOB_MAX as u32 >= TIER1_MAX_TOKENS * 8);
        assert_eq!(
            TIER1_REWARD_SHARE_VOTERS_BPS
                + TIER1_REWARD_SHARE_REFUND_BPS
                + TIER1_REWARD_SHARE_TREASURY_BPS,
            10_000
        );
    }

    #[test]
    fn tier1_deadline_bounds_sane() {
        const { assert!(TIER1_MIN_DEADLINE_BLOCKS >= 5) };
        const { assert!(TIER1_MAX_DEADLINE_BLOCKS > TIER1_MIN_DEADLINE_BLOCKS) };
        const { assert!(TIER1_MAX_DEADLINE_BLOCKS <= 100_000) };
    }

    #[test]
    fn tier1_max_tokens_bounded() {
        const { assert!(TIER1_MAX_TOKENS > 0) };
        const { assert!(TIER1_MAX_TOKENS <= 8192) };
    }

    #[test]
    fn inference_attestation_body_wire_compat_v072() {
        const V072_WIRE_BYTES: usize = 32 * 3 + 8 * 2;

        let body_none = InferenceAttestationBody {
            model_id: Hash256([1u8; 32]),
            input_hash: Hash256([2u8; 32]),
            output_hash: Hash256([3u8; 32]),
            challenge_period: 100,
            bond: 0,
            beneficiary: None,
        };
        let body_some = InferenceAttestationBody {
            model_id: Hash256([1u8; 32]),
            input_hash: Hash256([2u8; 32]),
            output_hash: Hash256([3u8; 32]),
            challenge_period: 100,
            bond: 0,
            beneficiary: Some(Hash256([9u8; 32])),
        };

        let bytes_none = bincode::serialize(&body_none).unwrap();
        let bytes_some = bincode::serialize(&body_some).unwrap();

        assert_eq!(
            bytes_none.len(),
            V072_WIRE_BYTES,
            "wire size MUST match v0.7.2 ({} bytes); change broke rolling-upgrade compat",
            V072_WIRE_BYTES
        );
        assert_eq!(
            bytes_some.len(),
            V072_WIRE_BYTES,
            "beneficiary value MUST NOT affect wire size — field must be #[serde(skip)]"
        );
        assert_eq!(
            bytes_none, bytes_some,
            "beneficiary value MUST NOT affect wire bytes"
        );

        let round: InferenceAttestationBody = bincode::deserialize(&bytes_some).unwrap();
        assert_eq!(
            round.beneficiary, None,
            "deserialize MUST default to None (skipped on wire)"
        );
    }
}
