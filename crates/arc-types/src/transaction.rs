use arc_crypto::Hash256;
use arc_crypto::signature::{Signature, KeyPair, SignatureError};
use serde::{Deserialize, Serialize};

use crate::account::Address;

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
    #[serde(default)]
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
        Self { from, to, amount, nonce, hash }
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

    /// Verify this transaction's signature.
    ///
    /// 1. Recomputes the expected hash from fields.
    /// 2. Checks `self.hash` matches.
    /// 3. Verifies the signature against the hash and `self.from`.
    ///
    /// Null signatures (benchmark mode) always fail verification.
    pub fn verify_signature(&self) -> Result<(), SignatureError> {
        // Integrity: recompute hash and compare
        let expected = self.compute_hash();
        if expected != self.hash {
            return Err(SignatureError::HashMismatch);
        }
        // Authorization: verify signature matches `from`
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

        assert_ne!(hash_a, hash_b, "different fees must produce different hashes");
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
        assert!(gas_costs::DEPLOY_CONTRACT > gas_costs::TRANSFER);
        assert!(gas_costs::BLOCK_GAS_LIMIT >= 30_000_000);
    }
}
