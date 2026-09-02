use arc_crypto::Hash256;
use arc_crypto::signature::{KeyPair, Signature, SignatureError};
use serde::{Deserialize, Serialize};

use crate::account::Address;

/// Protocol-v3 state-machine-owned dynamic accounts share this 120-bit
/// prefix; byte 15 identifies the exact account family.  The remaining 128
/// bits are a transcript hash.  Every externally writable v3 transaction
/// family must reject all addresses in this namespace before it mutates
/// state, so a normal account can neither pre-dust nor overwrite a future
/// replay/budget marker.
pub const V3_SYSTEM_ACCOUNT_PREFIX: [u8; 15] = *b"ARC-V3-REWARD:\0";

/// Exhaustive protocol-v3 dynamic system-account families.
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

/// Embed a full transcript digest in a type-specific 128-bit reserved
/// namespace.  Truncating only the digest suffix retains 128-bit collision
/// resistance while making ownership recognizable without enumerating
/// unbounded content-derived addresses.
pub fn v3_system_account_address(kind: V3SystemAccountKind, digest: &Hash256) -> Address {
    let mut bytes = [0u8; 32];
    bytes[..V3_SYSTEM_ACCOUNT_PREFIX.len()].copy_from_slice(&V3_SYSTEM_ACCOUNT_PREFIX);
    bytes[V3_SYSTEM_ACCOUNT_PREFIX.len()] = kind as u8;
    bytes[16..].copy_from_slice(&digest.as_ref()[..16]);
    Hash256(bytes)
}

/// Whether an address belongs to one of the explicitly allocated v3 dynamic
/// system-account namespaces.
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
    // Consume the legacy wire field to preserve the bincode layout, but never
    // trust a process-local verification-cache bit received from JSON/P2P.
    let _ = bool::deserialize(deserializer)?;
    Ok(false)
}

// ---------------------------------------------------------------------------
// Gas constants & metering
// ---------------------------------------------------------------------------

/// Gas costs for common operations (aligned with EVM for comparability).
pub mod gas_costs {
    /// Base gas cost for any transaction.
    pub const TX_BASE: u64 = 21_000;
    /// Gas per byte of transaction data.
    pub const TX_DATA_BYTE: u64 = 16;
    /// Gas for a simple transfer.
    pub const TRANSFER: u64 = 21_000;
    /// Gas for a settle transaction.
    pub const SETTLE: u64 = 25_000;
    /// Gas for a swap transaction.
    pub const SWAP: u64 = 30_000;
    /// Gas for staking operations.
    pub const STAKE: u64 = 25_000;
    /// Gas for escrow operations.
    pub const ESCROW: u64 = 35_000;
    /// Gas for contract deployment.
    pub const DEPLOY_CONTRACT: u64 = 53_000;
    /// Gas for contract call (base, plus execution).
    pub const CONTRACT_CALL: u64 = 21_000;
    /// Gas for agent registration.
    pub const REGISTER_AGENT: u64 = 30_000;
    /// Gas for multi-sig operations.
    pub const MULTI_SIG: u64 = 35_000;
    /// Gas for validator join.
    pub const JOIN_VALIDATOR: u64 = 30_000;
    /// Gas for validator leave.
    pub const LEAVE_VALIDATOR: u64 = 25_000;
    /// Gas for claiming rewards.
    pub const CLAIM_REWARDS: u64 = 25_000;
    /// Gas for a validator-signed faucet claim (slightly higher than a
    /// plain transfer because the executor performs a validator-set
    /// lookup and writes three account snapshots — signer, pool, recipient).
    pub const FAUCET_CLAIM: u64 = 25_000;
    /// Gas for updating validator stake.
    pub const UPDATE_STAKE: u64 = 25_000;
    /// Gas for governance proposal execution.
    pub const GOVERNANCE: u64 = 50_000;
    /// Gas for locking tokens in the bridge escrow.
    pub const BRIDGE_LOCK: u64 = 50_000;
    /// Gas for minting bridged tokens from another chain.
    pub const BRIDGE_MINT: u64 = 50_000;
    /// Base gas for batch settlement (before per-entry charges).
    pub const BATCH_SETTLE_BASE: u64 = 30_000;
    /// Gas per entry in a batch settlement.
    pub const BATCH_SETTLE_PER_ENTRY: u64 = 500;
    /// Maximum entries allowed in a single BatchSettle transaction.
    pub const BATCH_SETTLE_MAX_ENTRIES: usize = 10_000;
    /// Legacy flat gas constant (deprecated - use BATCH_SETTLE_BASE + PER_ENTRY).
    pub const BATCH_SETTLE: u64 = 30_000;
    /// Gas for opening a state channel.
    pub const CHANNEL_OPEN: u64 = 40_000;
    /// Gas for closing a state channel (mutual).
    pub const CHANNEL_CLOSE: u64 = 35_000;
    /// Gas for disputing a state channel.
    pub const CHANNEL_DISPUTE: u64 = 50_000;
    /// Gas for submitting a shard STARK proof.
    pub const SHARD_PROOF: u64 = 60_000;
    /// Gas for submitting an optimistic inference attestation (Tier 2).
    pub const INFERENCE_ATTESTATION: u64 = 50_000;
    /// Gas for a validator-authorized community inference reward.
    pub const COMMUNITY_INFERENCE_REWARD: u64 = 50_000;
    /// Gas for challenging an inference attestation (Tier 2).
    pub const INFERENCE_CHALLENGE: u64 = 100_000;
    /// Gas for opening a per-request inference escrow (Milestone B).
    pub const INFERENCE_ESCROW_OPEN: u64 = 50_000;
    /// Gas for releasing an inference escrow to replicas + treasury + proposer.
    pub const INFERENCE_ESCROW_RELEASE: u64 = 80_000;
    /// Gas for refunding an unreleased escrow after timeout.
    pub const INFERENCE_ESCROW_REFUND: u64 = 40_000;
    /// Gas for registering a new model on-chain (Milestone C).
    pub const MODEL_REGISTRATION: u64 = 60_000;
    /// Gas for signalling model demand (Milestone C).
    pub const MODEL_REQUEST: u64 = 50_000;
    /// Gas for claiming shard coverage (Milestone C).
    pub const SHARD_COVERAGE_CLAIM: u64 = 60_000;
    /// Gas for advertising node capacity (Milestone D).
    pub const CAPACITY_ADVERTISEMENT: u64 = 40_000;
    /// Gas for broadcasting a planner assignment (Milestone D).
    pub const SHARD_ASSIGNMENT_PROPOSAL: u64 = 80_000;
    /// Gas for opening a Tier 1 on-chain inference request. Slightly
    /// above an attestation because state writes a request escrow + a
    /// (lazy) vote bucket account.
    pub const TIER1_INFERENCE_REQUEST: u64 = 80_000;
    /// Gas for a single committee vote. Lower than a request because
    /// it only appends to the vote bucket + bumps signer nonce.
    pub const TIER1_INFERENCE_VOTE: u64 = 30_000;
    /// Gas for the finalize tx. Reads the vote bucket, aggregates,
    /// emits payouts; the actual cost scales with committee size but
    /// at K≤7 the work is bounded.
    pub const TIER1_INFERENCE_FINALIZE: u64 = 60_000;
    /// Gas for storage read.
    pub const SLOAD: u64 = 200;
    /// Gas for storage write.
    pub const SSTORE: u64 = 5_000;
    /// Gas for event emission.
    pub const LOG: u64 = 375;
    /// Default block gas limit.
    pub const BLOCK_GAS_LIMIT: u64 = 30_000_000;
}

/// Gas metering state for transaction execution.
#[derive(Clone, Debug, Default)]
pub struct GasMeter {
    /// Maximum gas allowed for this transaction.
    pub limit: u64,
    /// Gas consumed so far.
    pub consumed: u64,
}

impl GasMeter {
    /// Create a new gas meter with the given limit.
    pub fn new(limit: u64) -> Self {
        Self { limit, consumed: 0 }
    }

    /// Charge gas for an operation. Returns Err if out of gas.
    pub fn charge(&mut self, amount: u64) -> Result<(), GasError> {
        let new_consumed = self
            .consumed
            .checked_add(amount)
            .ok_or(GasError::Overflow)?;
        if new_consumed > self.limit {
            self.consumed = self.limit; // Cap at limit
            return Err(GasError::OutOfGas {
                limit: self.limit,
                consumed: new_consumed,
            });
        }
        self.consumed = new_consumed;
        Ok(())
    }

    /// Remaining gas.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.consumed)
    }

    /// Whether gas has been exhausted.
    pub fn is_exhausted(&self) -> bool {
        self.consumed >= self.limit
    }
}

/// Gas-related errors.
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

// ---------------------------------------------------------------------------
// Transaction types
// ---------------------------------------------------------------------------

/// Transaction type discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum TxType {
    /// Simple value transfer between accounts.
    Transfer = 0x01,
    /// Agent-to-agent service settlement (zero fee).
    Settle = 0x02,
    /// Asset swap (atomic exchange).
    Swap = 0x03,
    /// Escrow creation or release.
    Escrow = 0x04,
    /// Stake or unstake.
    Stake = 0x05,
    /// WASM smart contract call.
    WasmCall = 0x06,
    /// Multi-signature authorization.
    MultiSig = 0x07,
    /// Deploy a WASM smart contract.
    DeployContract = 0x08,
    /// Register an agent on-chain.
    RegisterAgent = 0x09,
    /// Join the validator set.
    JoinValidator = 0x0a,
    /// Leave the validator set.
    LeaveValidator = 0x0b,
    /// Claim staking rewards.
    ClaimRewards = 0x0c,
    /// Update validator stake.
    UpdateStake = 0x0d,
    /// Execute a governance proposal on-chain.
    Governance = 0x0e,
    /// Lock tokens for cross-chain bridge transfer.
    BridgeLock = 0x0f,
    /// Mint bridged tokens from another chain.
    BridgeMint = 0x10,
    /// Batch settlement - nets bilateral balances from multiple settlements.
    BatchSettle = 0x11,
    /// Open a bilateral state channel (lock funds).
    ChannelOpen = 0x12,
    /// Close a state channel (mutual agreement, release funds).
    ChannelClose = 0x13,
    /// Dispute a state channel (submit latest signed state).
    ChannelDispute = 0x14,
    /// Submit a STARK proof for a shard block.
    ShardProof = 0x15,
    /// Optimistic inference attestation (Tier 2 - off-chain with fraud proofs).
    InferenceAttestation = 0x16,
    /// Challenge an inference attestation (Tier 2 fraud proof).
    InferenceChallenge = 0x17,
    /// Register as an inference provider (declare hardware tier + stake).
    InferenceRegister = 0x18,
    /// Milestone B: open a per-request inference escrow - payer locks
    /// max_fee ARC against a request_id, which can be released on a
    /// successful attestation or refunded after timeout.
    InferenceEscrowOpen = 0x19,
    /// Milestone B: release an opened escrow. Splits max_fee into the
    /// RoleRevenueConfig shares (40% proposer / 25% replicas / 15% observer
    /// pool / 20% treasury) and zeros the escrow.
    InferenceEscrowRelease = 0x1a,
    /// Milestone B: payer reclaims their funds after `timeout_blocks` have
    /// elapsed without a release. Only callable by the original payer
    /// (identity proved via metadata-hash match on the escrow account).
    InferenceEscrowRefund = 0x1b,
    /// Milestone C: register a new model on-chain - commits to a stable
    /// model_id (BLAKE3-derived), the layer config, quantization, and the
    /// chunk-tree root for content-addressed weight distribution.
    /// Registration costs a 1000 ARC fee (anti-spam, Milestone E).
    ModelRegistration = 0x1c,
    /// Milestone C: signal demand for a model. Pins k-replication goal,
    /// per-layer-epoch bond offered to workers, and a max wait time.
    /// Community workers poll for open requests and claim ranges.
    ModelRequest = 0x1d,
    /// Milestone C: a community worker claims coverage for a specific
    /// layer range of a specific model for the epoch, posting a bond.
    /// Bond slashes if the worker doesn't serve for the agreed epoch.
    ShardCoverageClaim = 0x1e,
    /// Milestone D: worker advertises capacity so the planner can assign
    /// them fitting ranges. RAM / VRAM / bandwidth / uptime_hint / stake.
    CapacityAdvertisement = 0x1f,
    /// Milestone D: the planner's deterministic assignment output -
    /// broadcast so any full node replaying history reaches the same
    /// node→range mapping. Community workers long-poll for their
    /// assignment by pubkey and auto-apply.
    ShardAssignmentProposal = 0x20,
    /// Validator-signed faucet claim. Authorizes a debit from the system
    /// faucet pool (`faucet_pool_address()`) to a recipient. Replaces the
    /// previous null-signed Transfer pattern, which `pipeline.rs` rejected
    /// on every non-originating seed — see `arc-state` executor for the
    /// authorization rule (signer must be an active validator).
    FaucetClaim = 0x21,
    /// Tier 1 on-chain inference request. Submitted by the user; locks
    /// `max_reward` ARC in escrow and triggers VRF committee selection.
    /// Each selected validator runs the model locally and submits an
    /// `InferenceVote`. See `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md`.
    InferenceRequest = 0x22,
    /// Tier 1 vote from a committee member: their computed `output_hash`
    /// plus a VRF proof of committee membership. Multiple votes per
    /// request; aggregation runs once `min_agreement` votes match.
    InferenceVote = 0x23,
    /// Tier 1 system-deterministic finalize tx. Any full node injects this
    /// when either `committee_size` votes received or `deadline_blocks`
    /// elapsed. Distributes payout (or refunds), zeroes the escrow, and
    /// commits the final `output_hash` to the receipt log.
    InferenceFinalize = 0x24,
    /// Validator-authorized payment for one coordinator-issued community
    /// inference job. Appended to preserve every existing wire discriminant.
    CommunityInferenceReward = 0x25,
}

/// A transaction on the ARC chain.
///
/// The `hash` is computed over all fields *except* `hash` and `signature`.
/// The `signature` is a cryptographic proof that the holder of the private key
/// corresponding to `from` authorizes this transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    /// Transaction type.
    pub tx_type: TxType,
    /// Sender address (derived from public key).
    pub from: Address,
    /// Sender nonce (replay protection).
    pub nonce: u64,
    /// Transaction body (type-specific payload).
    pub body: TxBody,
    /// Fee in ARC (can be 0 for settlements).
    pub fee: u64,
    /// Gas limit for this transaction. Zero means unlimited (backward compat).
    /// For transfers the typical cost is 21,000; for deploys 53,000, etc.
    #[serde(default)]
    pub gas_limit: u64,
    /// BLAKE3 hash of the signable content (computed on creation).
    pub hash: Hash256,
    /// Cryptographic signature. Must be valid - null signatures are rejected.
    pub signature: Signature,
    /// Whether the signature has already been verified (e.g. at mempool insertion).
    /// When true, block execution can skip re-verification for a ~2x speedup.
    #[serde(
        default,
        serialize_with = "serialize_unverified",
        deserialize_with = "deserialize_unverified"
    )]
    pub sig_verified: bool,
}

/// Type-specific transaction payload.
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
    /// Request to join the validator set.
    JoinValidator(JoinValidatorBody),
    /// Request to leave the validator set (unstake).
    LeaveValidator,
    /// Claim accumulated staking rewards.
    ClaimRewards,
    /// Increase or decrease validator stake.
    UpdateStake(UpdateStakeBody),
    /// Execute a governance proposal on-chain.
    Governance(GovernanceBody),
    /// Lock tokens for cross-chain bridge transfer.
    BridgeLock(BridgeLockBody),
    /// Mint bridged tokens from another chain.
    BridgeMint(BridgeMintBody),
    /// Batch settlement - multiple settlements netted into one TX.
    BatchSettle(BatchSettleBody),
    /// Open a bilateral state channel.
    ChannelOpen(ChannelOpenBody),
    /// Close a state channel.
    ChannelClose(ChannelCloseBody),
    /// Dispute a state channel.
    ChannelDispute(ChannelDisputeBody),
    /// Submit a STARK proof for a shard block.
    ShardProof(ShardProofBody),
    /// Optimistic inference attestation (Tier 2).
    InferenceAttestation(InferenceAttestationBody),
    /// Challenge an inference attestation (Tier 2 fraud proof).
    InferenceChallenge(InferenceChallengeBody),
    /// Register as an inference provider (declare hardware tier + stake).
    InferenceRegister(InferenceRegisterBody),
    /// Open a per-request inference escrow (Milestone B).
    InferenceEscrowOpen(InferenceEscrowOpenBody),
    /// Release an opened escrow into replica + treasury + proposer shares.
    InferenceEscrowRelease(InferenceEscrowReleaseBody),
    /// Refund an unreleased escrow after timeout.
    InferenceEscrowRefund(InferenceEscrowRefundBody),
    /// Register a new model on-chain (Milestone C / E anti-spam fee).
    ModelRegistration(ModelRegistrationBody),
    /// Signal demand for a model - recruits community workers (Milestone C).
    ModelRequest(ModelRequestBody),
    /// Claim coverage for a layer range of a model (Milestone C).
    ShardCoverageClaim(ShardCoverageClaimBody),
    /// Advertise node capacity for the planner (Milestone D).
    CapacityAdvertisement(CapacityAdvertisementBody),
    /// Broadcast the planner's assignment output (Milestone D).
    ShardAssignmentProposal(ShardAssignmentProposalBody),
    /// Validator-authorized faucet claim — debits the system faucet pool.
    FaucetClaim(FaucetClaimBody),
    /// Tier 1 on-chain inference request (locks max_reward, triggers VRF committee).
    InferenceRequest(InferenceRequestBody),
    /// Tier 1 committee member vote (output_hash + VRF proof of membership).
    InferenceVote(InferenceVoteBody),
    /// Tier 1 system-deterministic finalize (payout or refund, zeroes escrow).
    InferenceFinalize(InferenceFinalizeBody),
    /// Validator-authorized, replay-protected community inference payment.
    CommunityInferenceReward(CommunityInferenceRewardBody),
}

impl TxBody {
    /// Canonical envelope discriminant for this body variant.
    ///
    /// Consensus and ingress must reject a transaction whose public `tx_type`
    /// disagrees with this value; otherwise a restricted body can masquerade
    /// as a harmless transfer while state executes the body variant.
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
        }
    }
}

/// Simple value transfer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferBody {
    pub to: Address,
    pub amount: u64,
    /// Pedersen commitment to the amount (for shielded transfers).
    pub amount_commitment: Option<[u8; 32]>,
}

/// Agent-to-agent service settlement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettleBody {
    pub agent_id: Address,
    pub service_hash: Hash256,
    pub amount: u64,
    pub usage_units: u64,
    pub amount_commitment: Option<[u8; 32]>,
}

/// Atomic asset swap.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SwapBody {
    pub counterparty: Address,
    pub offer_amount: u64,
    pub receive_amount: u64,
    pub offer_asset: Hash256,
    pub receive_asset: Hash256,
}

/// Escrow creation/release.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EscrowBody {
    pub beneficiary: Address,
    pub amount: u64,
    pub conditions_hash: Hash256,
    /// true = create, false = release
    pub is_create: bool,
}

/// Stake/unstake.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StakeBody {
    pub amount: u64,
    /// true = stake, false = unstake
    pub is_stake: bool,
    pub validator: Address,
}

/// WASM smart contract call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WasmCallBody {
    pub contract: Address,
    pub function: String,
    pub calldata: Vec<u8>,
    pub value: u64,
    pub gas_limit: u64,
}

/// Multi-signature transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MultiSigBody {
    pub inner_tx: Box<TxBody>,
    pub signers: Vec<Address>,
    pub threshold: u32,
}

/// Deploy a WASM smart contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployBody {
    /// WASM binary bytecode.
    pub bytecode: Vec<u8>,
    /// ABI-encoded constructor arguments.
    pub constructor_args: Vec<u8>,
    /// Pre-paid state rent deposit (in ARC).
    pub state_rent_deposit: u64,
}

/// Register an agent on-chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegisterBody {
    /// Human-readable agent name.
    pub agent_name: String,
    /// Capability bitmap or descriptor.
    pub capabilities: Vec<u8>,
    /// Agent endpoint URL.
    pub endpoint: String,
    /// Protocol hash (identifies the agent protocol version).
    pub protocol: Hash256,
    /// Arbitrary metadata (JSON, CBOR, etc).
    pub metadata: Vec<u8>,
}

/// Request to join the validator set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JoinValidatorBody {
    /// Ed25519 public key bytes for block signing.
    pub pubkey: [u8; 32],
    /// Initial stake amount (must meet minimum tier threshold).
    pub initial_stake: u64,
}

/// Update validator stake amount.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateStakeBody {
    /// New stake amount. If lower than current, difference is returned.
    pub new_stake: u64,
}

/// Governance transaction payload - records on-chain execution of a passed proposal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GovernanceBody {
    /// The proposal ID being executed.
    pub proposal_id: u64,
    /// The governance action to perform.
    pub action: GovernanceAction,
}

/// The action to perform in a governance transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum GovernanceAction {
    /// Execute a passed proposal (records execution on-chain).
    Execute,
}

/// Lock tokens on ARC Chain for transfer to a destination chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeLockBody {
    /// Target chain identifier.
    pub destination_chain: u32,
    /// Recipient address on the destination chain.
    pub destination_address: [u8; 32],
    /// Amount of ARC to lock in escrow.
    pub amount: u64,
}

/// Mint bridged tokens on ARC Chain from a source chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeMintBody {
    /// Source chain identifier.
    pub source_chain: u32,
    /// Transaction hash on the source chain that locked the tokens.
    pub source_tx_hash: Hash256,
    /// Recipient address on ARC Chain.
    pub recipient: Address,
    /// Amount of ARC to mint.
    pub amount: u64,
    /// Merkle proof of the lock transaction on the source chain.
    pub merkle_proof: Vec<u8>,
}

/// Batch settlement - nets bilateral balances for efficiency.
///
/// Instead of N individual Settle transactions (N state reads + N writes),
/// a BatchSettle computes the net balance change per account and applies
/// them in a single TX. 1000:1 compression ratio for bilateral agent settlements.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchSettleBody {
    /// Individual settlement entries to net.
    pub entries: Vec<SettleEntry>,
}

/// A single entry within a batch settlement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettleEntry {
    /// Agent being paid.
    pub agent_id: Address,
    /// Service hash (for audit trail).
    pub service_hash: Hash256,
    /// Gross amount owed.
    pub amount: u64,
}

/// Open a bilateral state channel between two parties.
///
/// Locks funds from the opener into the channel. The counterparty can
/// accept by submitting their own ChannelOpen with the same channel_id.
/// Once both sides have locked funds, off-chain bilateral trading begins.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelOpenBody {
    /// Unique channel identifier (BLAKE3 of both parties + nonce).
    pub channel_id: Hash256,
    /// The other party in the channel.
    pub counterparty: Address,
    /// Amount to lock in the channel.
    pub deposit: u64,
    /// Timeout in blocks - if counterparty doesn't open, funds unlock.
    pub timeout_blocks: u64,
}

/// Close a state channel by mutual agreement.
///
/// Both parties sign the final balances. Funds are released according
/// to the agreed split. This is the happy path (no dispute).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelCloseBody {
    /// Channel being closed.
    pub channel_id: Hash256,
    /// Final balance for the opener.
    pub opener_balance: u64,
    /// Final balance for the counterparty.
    pub counterparty_balance: u64,
    /// Counterparty's signature over the final state.
    pub counterparty_sig: Vec<u8>,
    /// State sequence number (monotonically increasing).
    pub state_nonce: u64,
}

/// Dispute a state channel by submitting the latest signed state.
///
/// Starts a challenge period. If the other party has a newer signed state,
/// they can submit it to override. After the challenge period, the latest
/// submitted state is finalized.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelDisputeBody {
    /// Channel being disputed.
    pub channel_id: Hash256,
    /// Claimed final balance for the opener.
    pub opener_balance: u64,
    /// Claimed final balance for the counterparty.
    pub counterparty_balance: u64,
    /// Signature of the other party over this state.
    pub other_party_sig: Vec<u8>,
    /// State sequence number (higher wins).
    pub state_nonce: u64,
    /// Challenge period in blocks.
    pub challenge_period: u64,
}

/// Submit a STARK proof for a shard block.
///
/// The shard proposer generates a Stwo STARK proof of the block's
/// state transition (prev_root → post_root given transactions).
/// Other shards/validators verify the proof instead of re-executing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShardProofBody {
    /// Shard index this proof covers.
    pub shard_id: u16,
    /// Block height within the shard.
    pub block_height: u64,
    /// Block hash being proven.
    pub block_hash: Hash256,
    /// Pre-state root before the block.
    pub prev_state_root: Hash256,
    /// Post-state root after the block.
    pub post_state_root: Hash256,
    /// Number of transactions in the proven block.
    pub tx_count: u32,
    /// The serialized STARK proof data.
    pub proof_data: Vec<u8>,
}

/// Optimistic inference attestation (Tier 2).
///
/// An off-chain inference provider attests to the result of running a model
/// on given inputs.  A bond is locked as collateral; if no challenge is
/// submitted within `challenge_period` blocks the attestation is finalized
/// and the bond is returned.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceAttestationBody {
    /// Model commitment hash (Merkle root of weights).
    pub model_id: Hash256,
    /// Hash of the input data.
    pub input_hash: Hash256,
    /// Hash of the output data.
    pub output_hash: Hash256,
    /// Challenge period in blocks (default: 100).
    pub challenge_period: u64,
    /// Bond amount locked as collateral (slashed if fraud proven).
    pub bond: u64,
    /// Local-only credit hint — NEVER serialized on the wire.
    ///
    /// IMPORTANT: this field is `#[serde(skip)]` on purpose. It shipped as
    /// `#[serde(default)]` in v0.7.6, but transactions are serialized with
    /// bincode, which is NOT self-describing: adding a struct field shifts
    /// the byte layout of every DAG block carrying an attestation, so v0.7.6
    /// nodes could not deserialize v0.7.2 blocks (and vice-versa). That
    /// partitioned the validator set during a rolling upgrade on 2026-05-29.
    /// Marking the field `skip` makes the serialized form (and the tx hash)
    /// byte-identical to v0.7.2, restoring rolling-upgrade compatibility.
    ///
    /// This deprecated local hint is not used for payment. Community income
    /// is attributed explicitly by `CommunityInferenceRewardBody::worker`.
    #[serde(skip)]
    pub beneficiary: Option<Address>,
}

/// Validator-authorized payment for one completed community inference job.
///
/// The worker's signed `InferenceAttestation` proves the result to independent
/// validators. A bounded Ed25519 approval quorum authorizes this compact
/// on-chain receipt; its outer signature only authenticates the aggregator.
/// Consensus derives one-shot markers from both `job_id` and the worker
/// certificate so neither can be paid twice under a different transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommunityInferenceRewardBody {
    /// Domain separator for ARC chain `0x415243` reward-v1 signatures.
    pub chain_domain: Hash256,
    /// Coordinator-generated, globally unique job commitment.
    pub job_id: Hash256,
    /// Active validator that created the assignment. Validators authenticate
    /// this coordinator before independently recomputing and approving work.
    pub coordinator: Address,
    /// Cryptographically random coordinator boot/session epoch for ordinary
    /// jobs, or a namespaced rollout/ordinal identity for recovery probes.
    /// Binding it in every approval prevents semantic reuse; recovery probes
    /// additionally receive a consensus replay marker across coordinators.
    pub assignment_epoch: Hash256,
    /// Monotonic nonce within `assignment_epoch`.
    pub job_nonce: u64,
    /// Protocol-v3 recovery context active when validators approved this job.
    /// All three fields are zero on legacy/dev state without a recovery
    /// context and must exactly match state at execution.
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    pub transaction_domain: Hash256,
    /// Stake-zero or staked worker that completed the assigned job.
    pub worker: Address,
    /// Model and I/O commitments copied from the verified worker attestation.
    pub model_id: Hash256,
    pub input_hash: Hash256,
    pub output_hash: Hash256,
    /// Token ceiling authorized by the coordinator for this job.
    pub max_tokens: u32,
    /// Last block height at which this reward authorization is valid.
    pub expires_at_height: u64,
    /// Flat worker-signed certificate. A full `Transaction` here made the
    /// wire type recursively nestable because a reward transaction could
    /// contain another reward transaction indefinitely. Validators rebuild
    /// the one permitted InferenceAttestation shape from these bounded fields.
    pub worker_certificate: WorkerInferenceCertificate,
    /// Independent active-validator approvals over
    /// [`Self::validator_approval_commitment`]. The outer transaction
    /// signature authenticates the aggregator only; consensus does not treat
    /// it as proof that the off-chain result was independently verified.
    /// State caps this list at [`MAX_COMMUNITY_REWARD_APPROVALS`].
    pub validator_approvals: Vec<CommunityRewardValidatorApproval>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerInferenceCertificate {
    /// Hash assigned by `Transaction::sign` to the original worker
    /// InferenceAttestation.
    pub attestation_hash: Hash256,
    pub nonce: u64,
    pub challenge_period: u64,
    pub signature: Signature,
}

/// Hard wire bound for reward-v1 quorum evidence.
///
/// V1 deliberately fails closed when the active validator set exceeds this
/// bound. Each entry is exactly an address, Ed25519 public key, and 64-byte
/// signature, so a reward cannot recursively embed transactions or unbounded
/// post-quantum signature payloads.
pub const MAX_COMMUNITY_REWARD_APPROVALS: usize = 64;

/// Reward-v1 is intentionally fixed to the six-validator ARC approval
/// committee. Issuance fails closed if the active set has any other size.
pub const COMMUNITY_REWARD_VALIDATOR_SET_SIZE: usize = 6;
/// Five independently recomputing validators must approve one receipt.
pub const COMMUNITY_REWARD_APPROVALS_REQUIRED: usize = 5;
/// Community compute is stake-zero eligible. This explicit consensus
/// constant is the single code-level policy switch; raising it requires a
/// coordinated protocol release rather than an unsafe per-node flag.
pub const COMMUNITY_REWARD_MIN_WORKER_STAKE: u64 = 0;

/// Wire-compatible namespace for rollout recovery probes.  Recovery probes
/// reuse the existing `assignment_epoch` commitment so old blocks retain the
/// exact same bincode layout, while consensus can recognize the small subset
/// that also needs a rollout/ordinal-wide replay marker across coordinators.
pub const RECOVERY_REWARD_PROBE_PREFIX: [u8; 16] = *b"ARC-RCV-PROBE1\0\0";

/// One validator's approval of a community reward's complete semantic
/// commitment. The split 64-byte signature keeps the wire representation
/// fixed-size while using serde's portable 32-byte array support.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRewardValidatorApproval {
    pub validator: Address,
    pub public_key: [u8; 32],
    pub signature_halves: [[u8; 32]; 2],
}

impl CommunityRewardValidatorApproval {
    /// Convert only a canonical 64-byte Ed25519 signature. Other ARC
    /// signature schemes are intentionally not representable in reward-v1
    /// quorum evidence.
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

    /// Reconstruct the ARC signature enum only for cryptographic verification.
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

    /// Derive the only valid job identifier for an exact assignment.
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

    /// Zero-balance state marker used for consensus-level replay protection.
    pub fn marker_address(chain_domain: &Hash256, job_id: &Hash256) -> Address {
        let mut bytes = Vec::with_capacity(23 + 64);
        bytes.extend_from_slice(b"arc-community-reward-v1");
        bytes.extend_from_slice(chain_domain.as_ref());
        bytes.extend_from_slice(job_id.as_ref());
        arc_crypto::hash_bytes(&bytes)
    }

    /// Protocol-v3 replay marker. The legacy full-hash derivation above must
    /// remain stable for historical state; v3 writes the same transcript into
    /// a transfer-inaccessible namespace so predictable jobs cannot be
    /// pre-dusted by an ordinary account.
    pub fn v3_marker_address(chain_domain: &Hash256, job_id: &Hash256) -> Address {
        v3_system_account_address(
            V3SystemAccountKind::CommunityRewardJob,
            &Self::marker_address(chain_domain, job_id),
        )
    }

    /// Independent one-shot marker for the worker-signed certificate. A
    /// validator must not be able to wrap one valid certificate in fresh job
    /// IDs and collect the flat treasury reward repeatedly.
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

    /// Protocol-v3 namespaced certificate replay marker.
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

    /// Whether this assignment carries the explicit recovery-probe namespace.
    pub fn is_recovery_probe_assignment(assignment_epoch: &Hash256) -> bool {
        assignment_epoch.as_ref()[..RECOVERY_REWARD_PROBE_PREFIX.len()]
            == RECOVERY_REWARD_PROBE_PREFIX
    }

    /// Cross-coordinator one-shot marker for a rollout-bound recovery probe.
    ///
    /// Normal community jobs deliberately share a random boot epoch and must
    /// not receive this marker.  The fixed 128-bit namespace makes accidental
    /// classification of a normal random epoch cryptographically negligible
    /// without adding a field that would break historical bincode decoding.
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

    /// Protocol-v3 namespaced rollout-probe replay marker.
    pub fn v3_recovery_probe_marker_address(
        chain_domain: &Hash256,
        assignment_epoch: &Hash256,
    ) -> Option<Address> {
        Self::recovery_probe_marker_address(chain_domain, assignment_epoch).map(|legacy| {
            v3_system_account_address(V3SystemAccountKind::RecoveryRewardProbe, &legacy)
        })
    }

    /// Common transcript independently signed by every reward approver.
    ///
    /// This binds all payout semantics, the exact worker-signed certificate,
    /// and the reward-v1 amount. It intentionally excludes
    /// `validator_approvals` and the outer transaction envelope so validators
    /// can sign the same bounded message before an aggregator packages it.
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

    /// Rebuild the only worker transaction shape accepted by a community
    /// reward. Fixed fee/gas/bond values keep the certificate compact and
    /// remove all recursive deserialization paths.
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

/// Challenge an inference attestation (Tier 2 fraud proof).
///
/// A challenger disagrees with the attested output and submits their own
/// computed output hash along with a bond.  If the challenger's output is
/// confirmed correct (via on-chain re-execution through precompile 0x0A),
/// the challenger receives both bonds; otherwise the challenger's bond is
/// slashed and the attester keeps both.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceChallengeBody {
    /// Hash of the attestation TX being challenged.
    pub attestation_hash: Hash256,
    /// The challenger's computed output hash (should differ from attested).
    pub challenger_output_hash: Hash256,
    /// Bond amount from challenger (returned if challenge succeeds).
    pub challenger_bond: u64,
}

/// Register as an inference provider.
///
/// Validators declare their hardware tier and lock a stake bond.
/// The chain maintains a registry: `DashMap<Address, InferenceTier>`.
/// VRF committee selection reads from this registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceRegisterBody {
    /// Hardware tier this validator can support (1-4).
    pub tier: u8,
    /// Stake bond to lock (proves commitment, returned on deregister).
    pub stake_bond: u64,
}

/// Milestone B: open a per-request inference escrow.
///
/// The payer (tx.from) locks `max_fee` ARC against `request_id`. The chain
/// derives a deterministic escrow account from `request_id` and stores
/// identifying metadata (model_id, max_tokens, timeout_blocks, payer) so
/// later release/refund tx bodies can be validated by re-hashing the
/// same fields and matching the stored commitment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceEscrowOpenBody {
    /// Client-chosen request identifier (must be unique per open).
    pub request_id: [u8; 32],
    /// Which model this payment covers - e.g. Llama-2-7B BLAKE3 ID.
    pub model_id: Hash256,
    /// Maximum ARC the payer is willing to pay for this request.
    pub max_fee: u64,
    /// Maximum tokens to generate (caps the work the network does).
    pub max_tokens: u32,
    /// After opened_at + timeout_blocks elapses without a release, the
    /// original payer may reclaim the escrow via InferenceEscrowRefund.
    pub timeout_blocks: u64,
}

/// Release an opened escrow against an attested inference result.
///
/// The release distributes `max_fee` according to the RoleRevenueConfig:
/// 40% to `proposer`, 25% split evenly among `replicas`, 15% to
/// `observer_pool`, 20% to `treasury`. Any rounding residue goes to
/// `treasury`.
///
/// Authorization (MVP): any signed tx may submit a release as long as
/// the provided metadata (payer, model_id, max_tokens, timeout_blocks)
/// hashes to the value stored at open time. Output-hash-gated release
/// and attestation-required release are tracked as follow-ups.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceEscrowReleaseBody {
    pub request_id: [u8; 32],
    /// Must match the original payer. Encoded in the escrow's stored
    /// metadata hash.
    pub payer: Address,
    pub model_id: Hash256,
    pub max_tokens: u32,
    pub timeout_blocks: u64,
    /// Output hash from the consensus attestation (recorded in the
    /// release receipt for audit; not gating today).
    pub output_hash: Hash256,
    /// The coordinator that served the request (receives 40% share).
    pub proposer: Address,
    /// Replicas that answered; 25% share is split evenly.
    pub replicas: Vec<Address>,
    /// Account that accumulates the observer-pool share (15%). Today the
    /// testnet uses the treasury address; decoupled for when the pool
    /// becomes a distinct account.
    pub observer_pool: Address,
    /// Treasury address - receives 20% plus any rounding residue.
    pub treasury: Address,
}

/// Refund an unreleased escrow after timeout.
///
/// Only the original payer can call this: `tx.from` is used as the payer
/// in the metadata rehash, and the rehash must equal the commitment
/// stored at open time. Current block height must be at least
/// `opened_at + timeout_blocks`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceEscrowRefundBody {
    pub request_id: [u8; 32],
    pub model_id: Hash256,
    pub max_tokens: u32,
    pub timeout_blocks: u64,
}

// ---------------------------------------------------------------------------
// Tier 1 on-chain inference (VRF committee voting)
//
// See `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md` for the full design.
// In short: requester submits InferenceRequest → committee selected via VRF
// using BLAKE3(commit_block_hash || request_id) → each committee member
// runs the model locally and submits InferenceVote → any node deterministically
// injects InferenceFinalize once min_agreement votes match or deadline expires.
// ---------------------------------------------------------------------------

/// Tier 1 inference request body. Locks `max_reward` ARC in escrow at
/// `BLAKE3("arc-infreq" || request_id)`. Committee selection happens at apply
/// time using the commit block hash as the VRF seed.
///
/// Prompts longer than 32 KB should use a future content-addressed variant
/// (model_id-style hash + off-chain blob fetch). For Phase A we inline the
/// prompt to keep the desktop client simple.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceRequestBody {
    /// Caller-chosen request identifier, must be unique per requester.
    /// Convention: `BLAKE3(requester || nonce || input_hash)`.
    pub request_id: [u8; 32],
    /// Model the requester wants run (must be registered via ModelRegistration).
    pub model_id: Hash256,
    /// BLAKE3 of the input bytes — committee members verify against the blob.
    pub input_hash: Hash256,
    /// The actual prompt bytes. Capped at 32 KB by the state validator.
    pub input_blob: Vec<u8>,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Hardware tier required (1 = commodity CPU, 2 = GPU, 3+ = sharded).
    /// Phase A only supports tier=1.
    pub tier: u8,
    /// Maximum ARC the requester is willing to pay for this request.
    /// Locked in escrow; distributed on Finalize (70/20/10 by default).
    pub max_reward: u64,
    /// Relative deadline in blocks. Auto-refund triggers if Finalize hasn't
    /// committed by `anchor_height + deadline_blocks`.
    pub deadline_blocks: u64,
    /// Target committee size K (e.g. 5 for testnet, 7 for default).
    /// Actual size may be smaller if eligible validator set is smaller.
    pub committee_size: u8,
}

/// Tier 1 inference vote body. One per (committee_member, request).
/// Voter must be in the committee derived from the request's commit block
/// hash — state apply rejects votes from non-members.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceVoteBody {
    /// The request this vote is for.
    pub request_id: [u8; 32],
    /// Voter's computed BLAKE3 over the generated output tokens.
    pub output_hash: Hash256,
    /// Optional plaintext output. Only the first voter attaches to save
    /// block space; subsequent voters set None. State apply verifies
    /// `BLAKE3(blob) == output_hash` when blob is present.
    pub output_blob: Option<Vec<u8>>,
    /// ECVRF proof that the voter belongs to the committee derived from
    /// (`committee_seed`, voter address). Defense-in-depth on top of the
    /// state-side committee re-derivation.
    pub vrf_proof: Vec<u8>,
    /// Block hash of the block that committed this request. The committee
    /// was derived from `BLAKE3(committee_seed || request_id)`.
    pub committee_seed: Hash256,
}

/// Tier 1 finalize body. Deterministic — any full node can submit, only
/// the first one to commit succeeds (subsequent submissions reject in
/// `apply` because status != Voting/ReadyToFinalize). State apply runs
/// `committee::aggregate_votes` and distributes the escrow.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceFinalizeBody {
    /// The request being finalized.
    pub request_id: [u8; 32],
}

/// Maximum size (in bytes) of an inline prompt in `InferenceRequestBody`.
/// Enforced by state validation. Longer prompts must wait for the future
/// content-addressed variant. 32 KB ≈ 8000 tokens of UTF-8 input which
/// comfortably exceeds Llama-2-7B's 4096-token context window.
pub const TIER1_INPUT_BLOB_MAX: usize = 32 * 1024;

/// Maximum size of the optional `output_blob` in `InferenceVoteBody`.
/// Sized for the largest realistic `max_tokens` (2048) at a worst-case
/// ~8 bytes per token UTF-8 (multi-byte CJK etc.) plus a small framing
/// margin. Voters whose output exceeds this must omit the blob (set
/// `None`) and rely on hash-only voting; the first-attached blob
/// requirement is best-effort.
pub const TIER1_OUTPUT_BLOB_MAX: usize = 16 * 1024;

/// Anti-spam fee charged from the requester even on timeout/disagreement
/// refund. Prevents free DoS of validator inference capacity.
pub const TIER1_ANTI_SPAM_FEE: u64 = 1;

/// Maximum tokens a single InferenceRequest may ask for. Bounds validator
/// work per request; without this a u32::MAX value could pin a validator
/// for hours. 2048 is roughly half a Llama-2 context window — plenty for
/// chat, summarization, and short essay generation.
pub const TIER1_MAX_TOKENS: u32 = 2048;

/// Lower bound on `deadline_blocks`. Below this a request would refund
/// before validators realistically can run inference + submit votes
/// (CPU inference of even 32 tokens takes 20-40 sec, and chain block
/// time is ~1-3 sec). 5 blocks ≈ 5-15 sec wall-clock buffer for committee
/// observability before any vote can possibly land.
pub const TIER1_MIN_DEADLINE_BLOCKS: u64 = 5;

/// Upper bound on `deadline_blocks`. Caps how long a requester's
/// `max_reward` can sit locked in escrow. 1000 blocks ≈ 16-50 minutes
/// at the current block tempo — long enough for slow GPU-less validators,
/// short enough that an abandoned request returns funds within an hour.
pub const TIER1_MAX_DEADLINE_BLOCKS: u64 = 1000;

/// Default reward split applied by `apply_inference_finalize` on a
/// successful consensus outcome. Mirrors the Milestone B
/// `RoleRevenueConfig` shape: most goes to the producers, a rebate
/// returns to the requester (encouraging tight max_reward), a slice to
/// treasury.
pub const TIER1_REWARD_SHARE_VOTERS_BPS: u64 = 7000; // 70.00%
pub const TIER1_REWARD_SHARE_REFUND_BPS: u64 = 2000; // 20.00%
pub const TIER1_REWARD_SHARE_TREASURY_BPS: u64 = 1000; // 10.00%

/// Milestone C: register a model. On accept, the chain stores the
/// registration in a deterministic account keyed by the model_id (using
/// the same "metadata-in-storage_root" trick as the inference escrow:
/// storage_root commits to (n_layers, d_model, quantization_tag,
/// chunk_tree_root) and nonce stores registered_at height).
///
/// Registration costs `registration_fee` ARC (default 1000) which goes
/// to the treasury - anti-spam floor for the open registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRegistrationBody {
    pub model_id: Hash256,
    pub metadata_hash: Hash256,
    pub chunk_tree_root: Hash256,
    pub n_layers: u32,
    pub d_model: u32,
    /// Short tag: "int16", "int8", "q4", "fp16", etc.
    pub quantization: String,
    /// Anti-spam fee. Chain floors this at
    /// `MIN_MODEL_REGISTRATION_FEE` and transfers to treasury.
    pub registration_fee: u64,
    /// Address that receives future per-model fees (royalty to the
    /// publisher). May equal `tx.from` but not required.
    pub royalty_recipient: Address,
}

/// Milestone C: request coverage for a model. Chain records demand; a
/// separate ShardCoverageClaim (also below) is used by workers to
/// fulfill. The request provides an economic signal; actual routing /
/// assignment is the planner's job (Milestone D).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRequestBody {
    pub request_id: [u8; 32],
    pub model_id: Hash256,
    pub target_k_replication: u32,
    pub bond_per_layer_epoch: u64,
    pub max_wait_secs: u32,
}

/// Milestone C: a worker claims coverage for a specific model+range
/// for `epoch_blocks` blocks. Their `bond` locks while the claim is
/// active; slashes if they don't serve.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShardCoverageClaimBody {
    pub model_id: Hash256,
    pub node_pubkey: [u8; 32],
    pub ranges: Vec<(u32, u32)>,
    pub bond: u64,
    pub epoch_blocks: u64,
}

/// Milestone D: node advertises its capacity. The planner uses this plus
/// open ModelRequests and current shard_registry state to compute a
/// deterministic assignment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapacityAdvertisementBody {
    pub node_pubkey: [u8; 32],
    pub ram_bytes: u64,
    pub vram_bytes: u64,
    pub bandwidth_mbps: u32,
    pub uptime_hint_mins: u32,
    pub stake: u64,
    /// Optional geographic hint so the planner can spread replicas.
    /// Simple ISO-3166-1 alpha-2 country code or "UNK" when unknown.
    pub region: String,
}

/// Milestone D: the planner's output. A single assignment tx contains
/// `(node_pubkey, model_id, Vec<range>)` entries - one entry per node
/// that gets assigned at least one range. Workers long-poll
/// `/assignments/for_me` keyed by their pubkey.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShardAssignmentProposalBody {
    pub epoch_blocks: u64,
    pub assignments: Vec<AssignmentEntry>,
    /// BLAKE3 hash of the planner's full input snapshot (registry +
    /// requests + capacity set) so multiple nodes recompute the same
    /// output deterministically. Stored verbatim on-chain for replay.
    pub input_snapshot_hash: Hash256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssignmentEntry {
    pub node_pubkey: [u8; 32],
    pub model_id: Hash256,
    pub ranges: Vec<(u32, u32)>,
}

/// Shared helpers for Milestones C–E: deterministic account addresses
/// and metadata commitments. Same pattern as Milestone B's escrow -
/// avoids a new DashMap by packing data into an account's storage_root.
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

/// Minimum registration fee. Prevents a spammer from cluttering the
/// open model registry with 10_000 fake models for free. The fee flows
/// to the treasury (no burn - fixed total supply is a hard ARC rule).
pub const MIN_MODEL_REGISTRATION_FEE: u64 = 1_000;

/// Validator-authorized faucet claim. Validator signs the tx; executor
/// requires `tx.from` to be an active validator (deterministic on every
/// seed) and debits the system faucet pool instead of the signer's own
/// balance. This is the cross-seed-propagation-safe replacement for the
/// previous null-signed Transfer pattern.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaucetClaimBody {
    /// Address to credit.
    pub recipient: Address,
    /// Amount in ARC. Chain enforces `<= FAUCET_CLAIM_MAX`.
    pub amount: u64,
}

impl FaucetClaimBody {
    /// Exactly-once marker shared by every validator for one recipient. This
    /// closes the cross-node replay path where six validators could each sign
    /// a different transaction hash for the same faucet address.
    pub fn marker_address(recipient: &Address) -> Address {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-faucet-recipient-marker-v1");
        hasher.update(recipient.as_ref());
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Protocol-v3 exactly-once marker protected from arbitrary transfer
    /// writes. The original derivation remains the legacy/v2 state key.
    pub fn v3_marker_address(recipient: &Address) -> Address {
        v3_system_account_address(
            V3SystemAccountKind::FaucetClaimMarker,
            &Self::marker_address(recipient),
        )
    }
}

/// Per-claim cap enforced by the executor (anti-drain): exactly 1 ARC in
/// nine-decimal base units. Matches the RPC-layer default; raising one without
/// the other will reject transactions.
pub const FAUCET_CLAIM_MAX: u64 = crate::economics::ARC_BASE_UNITS;

/// System faucet pool address. Same on every seed because it's derived
/// from `blake3::hash(&[0u8])` and prefunded in genesis.toml.
pub fn faucet_pool_address() -> Address {
    arc_crypto::hash_bytes(&[0u8])
}

/// Dedicated finite treasury for validator-approved community inference
/// rewards. It is the second prefunded system account (`blake3(&[1u8])`) and
/// is deliberately distinct from the public faucet, so onboarding claims can
/// neither consume worker rewards nor inflate their projected runway.
pub fn inference_reward_treasury_address() -> Address {
    arc_crypto::hash_bytes(&[1u8])
}

/// Milestone B helpers - shared between arc-state and arc-node so both
/// sides agree on the escrow-account derivation and metadata layout.
impl InferenceEscrowOpenBody {
    /// Deterministic escrow account address for this request_id.
    pub fn escrow_address(request_id: &[u8; 32]) -> [u8; 32] {
        let mut buf = Vec::with_capacity(24 + 32);
        buf.extend_from_slice(b"arc-inference-escrow");
        buf.extend_from_slice(request_id);
        arc_crypto::hash_bytes(&buf).0
    }

    /// Metadata commitment (32 bytes) stored in the escrow account's
    /// storage_root slot. Allows release/refund callers to prove they
    /// know the original (payer, model_id, max_tokens, timeout_blocks)
    /// without the chain needing a separate DashMap of records.
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

/// EVM event log emitted during contract execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventLog {
    /// Contract address that emitted the event.
    pub address: Address,
    /// Indexed event topics (topic[0] = event signature hash).
    pub topics: Vec<Hash256>,
    /// Non-indexed event data.
    pub data: Vec<u8>,
    /// Block height.
    pub block_height: u64,
    /// Transaction hash.
    pub tx_hash: Hash256,
    /// Log index within the block.
    pub log_index: u32,
}

/// Transaction receipt (result of execution).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxReceipt {
    pub tx_hash: Hash256,
    pub block_height: u64,
    pub block_hash: Hash256,
    pub index: u32,
    pub success: bool,
    pub gas_used: u64,
    /// Pedersen commitment for privacy proof.
    pub value_commitment: Option<[u8; 32]>,
    /// Merkle proof of inclusion in the block.
    pub inclusion_proof: Option<Vec<u8>>,
    /// Event logs emitted during execution.
    pub logs: Vec<EventLog>,
}

/// Compact transfer transaction - optimized for throughput benchmarks.
/// Fixed-size 250-byte layout: less memory bandwidth = more TPS.
///
/// Layout:
///   tx_type:   1 byte
///   from:     32 bytes
///   to:       32 bytes
///   amount:    8 bytes
///   nonce:     8 bytes
///   hash:     32 bytes
///   padding: 137 bytes  (total = 250 bytes)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactTransfer {
    pub from: Address,
    pub to: Address,
    pub amount: u64,
    pub nonce: u64,
    pub hash: Hash256,
}

/// Target size for compact transfers (bytes).
pub const COMPACT_TX_SIZE: usize = 250;

impl CompactTransfer {
    /// Create a compact transfer and compute its hash.
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

    /// Serialize into a fixed-size 250-byte buffer.
    /// This is the hot-path representation for hashing throughput.
    pub fn to_bytes(&self) -> [u8; COMPACT_TX_SIZE] {
        let mut buf = [0u8; COMPACT_TX_SIZE];
        buf[0] = TxType::Transfer as u8;
        buf[1..33].copy_from_slice(&self.from.0);
        buf[33..65].copy_from_slice(&self.to.0);
        buf[65..73].copy_from_slice(&self.amount.to_le_bytes());
        buf[73..81].copy_from_slice(&self.nonce.to_le_bytes());
        buf[81..113].copy_from_slice(&self.hash.0);
        // bytes 113..250 are zero padding
        buf
    }
}

impl Transaction {
    /// Construct a validator-authorized payment for a completed community
    /// inference job. The caller must sign with an active validator key.
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

    /// Construct a validator-authorized faucet claim (unsigned — caller
    /// must `tx.sign(&validator_keypair)` before submitting). The executor
    /// will reject the tx unless `validator` is an active validator at
    /// commit time.
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

    /// Create a new transfer transaction (unsigned, zero fee).
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

    /// Create a new settlement transaction (unsigned, zero fee - settlements are always free).
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

    /// Create a new WASM contract call transaction (unsigned).
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

    /// Create a new contract deployment transaction (unsigned).
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

    /// Create a new agent registration transaction (unsigned).
    // These eight parameters are the five wire fields of `RegisterBody` plus the
    // three envelope fields (from/fee/nonce) every `new_*` constructor here takes.
    // Bundling them into a params struct would change a public API of the shared
    // type crate that every downstream SDK builds against, for no behaviour gain.
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

    /// Compute the BLAKE3 signing hash.
    ///
    /// Covers: `tx_type || from || nonce || body || fee || gas_limit`
    /// Does NOT include the hash or signature fields.
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

    /// Compute the protocol-v3 transaction hash for an exact recovery domain.
    /// The same transaction fields signed for another chain, recovery epoch,
    /// or validator-set ID produce a different hash and cannot be replayed.
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

    /// Sign this transaction in place.
    ///
    /// 1. Recomputes the hash from the current fields.
    /// 2. Signs the hash with the given key pair.
    /// 3. Sets both `hash` and `signature` on `self`.
    pub fn sign(&mut self, keypair: &KeyPair) -> Result<(), SignatureError> {
        self.hash = self.compute_hash();
        self.signature = keypair.sign(&self.hash)?;
        Ok(())
    }

    /// Sign for one protocol-v3 recovery domain. This must be used by clients
    /// after the H+1 transition; legacy [`Self::sign`] remains unchanged.
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

    /// Verify this transaction's signature.
    ///
    /// 1. Recomputes the expected hash from fields.
    /// 2. Checks `self.hash` matches.
    /// 3. Verifies the signature against the hash and `self.from`.
    ///
    /// Null signatures (benchmark mode) always fail verification.
    pub fn verify_signature(&self) -> Result<(), SignatureError> {
        if self.tx_type != self.body.tx_type() {
            return Err(SignatureError::HashMismatch);
        }
        // Integrity: recompute hash and compare
        let expected = self.compute_hash();
        if expected != self.hash {
            return Err(SignatureError::HashMismatch);
        }
        // Authorization: verify signature matches `from`
        self.signature.verify(&self.hash, &self.from)
    }

    /// Verify content, signer, and signature in one exact recovery domain.
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

    /// Returns true if this transaction is unsigned (null signature).
    pub fn is_unsigned(&self) -> bool {
        self.signature.is_null()
    }

    /// Serialized size in bytes (approximate).
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

    // ── Basic construction ──

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
            vec![0x00, 0x61, 0x73, 0x6d], // WASM magic
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

    // ── Signing & verification ──

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

        // Tamper with the amount
        tx.body = TxBody::Transfer(TransferBody {
            to: test_addr(2),
            amount: 9999,
            amount_commitment: None,
        });

        // Verification must fail (hash mismatch)
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

        // Transaction says it's from kp, but we sign with wrong_kp
        let mut tx = Transaction::new_transfer(kp.address(), test_addr(2), 1000, 0);
        tx.hash = tx.compute_hash();
        tx.signature = wrong_kp.sign(&tx.hash).expect("sign ok");

        // Verification must fail (address mismatch)
        assert!(tx.verify_signature().is_err());
    }

    #[test]
    fn test_unsigned_verify_fails() {
        let tx = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        // Null signature fails verification (key is all zeros → address mismatch)
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

    // ── Gas metering ──

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
        assert!(gas.charge(20_000).is_err()); // Would exceed limit
    }

    #[test]
    fn test_gas_costs_constants() {
        assert_eq!(gas_costs::TX_BASE, 21_000);
        const { assert!(gas_costs::DEPLOY_CONTRACT > gas_costs::TRANSFER) };
        const { assert!(gas_costs::BLOCK_GAS_LIMIT >= 30_000_000) };
    }

    // ── Tier 1 on-chain inference tx round-trip ──

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
        // The plan (TIER1_ONCHAIN_INFERENCE_PLAN.md) reserves 0x22-0x24.
        // Lock these down: a future renumber would change the wire format
        // and silently break older clients.
        assert_eq!(TxType::InferenceRequest as u8, 0x22);
        assert_eq!(TxType::InferenceVote as u8, 0x23);
        assert_eq!(TxType::InferenceFinalize as u8, 0x24);
        assert_eq!(TxType::CommunityInferenceReward as u8, 0x25);
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
        // Output blob ceiling should comfortably hold the max-token output:
        // TIER1_MAX_TOKENS × ~8 bytes/token UTF-8 worst case = 16 KB.
        assert!(TIER1_OUTPUT_BLOB_MAX as u32 >= TIER1_MAX_TOKENS * 8);
        // Three shares must sum to 100%.
        assert_eq!(
            TIER1_REWARD_SHARE_VOTERS_BPS
                + TIER1_REWARD_SHARE_REFUND_BPS
                + TIER1_REWARD_SHARE_TREASURY_BPS,
            10_000
        );
    }

    #[test]
    fn tier1_deadline_bounds_sane() {
        // Must be a real range, and min must leave validators time to
        // observe + run inference + submit a vote tx (single block is
        // not enough even on the fastest hardware).
        const { assert!(TIER1_MIN_DEADLINE_BLOCKS >= 5) };
        const { assert!(TIER1_MAX_DEADLINE_BLOCKS > TIER1_MIN_DEADLINE_BLOCKS) };
        const { assert!(TIER1_MAX_DEADLINE_BLOCKS <= 100_000) };
    }

    #[test]
    fn tier1_max_tokens_bounded() {
        // A single request must not be allowed to consume hours of
        // validator compute. 2048 caps each request at ~5-15 minutes
        // of CPU inference, manageable as a worst case.
        const { assert!(TIER1_MAX_TOKENS > 0) };
        const { assert!(TIER1_MAX_TOKENS <= 8192) };
    }

    /// Pin the wire size of InferenceAttestationBody to the v0.7.2 layout.
    /// v0.7.6 added a `beneficiary: Option<Address>` (#[serde(default)])
    /// which silently shifted the bincode byte stream and partitioned the
    /// chain on 2026-05-29. v0.7.8 marks it `#[serde(skip)]` so the bytes
    /// (and tx hash) are byte-identical to v0.7.2 regardless of the field
    /// value. This test FAILS if anyone removes the skip or adds a new
    /// wire field — at which point a coordinated activation is required,
    /// not a rolling upgrade.
    #[test]
    fn inference_attestation_body_wire_compat_v072() {
        // v0.7.2 layout: 3 Hash256 (96 B) + 2 u64 (16 B) = 112 B.
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

        // Round-trip restores beneficiary to None (skip default).
        let round: InferenceAttestationBody = bincode::deserialize(&bytes_some).unwrap();
        assert_eq!(
            round.beneficiary, None,
            "deserialize MUST default to None (skipped on wire)"
        );
    }
}
