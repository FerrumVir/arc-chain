// ─── @arc-chain/sdk - Type Definitions ────────────────────────
// Complete TypeScript types for the ARC Chain RPC API.
// Covers all endpoints, all 37 transaction body variants,
// block structures, account state, validators, contracts,
// light client, faucet, and ETH JSON-RPC compatibility.

import type { U64 } from "./u64.js";
export type { U64 } from "./u64.js";

// ─── Primitives ──────────────────────────────────────────────

/** 64-character hex-encoded BLAKE3 hash (no 0x prefix on the native API). */
export type Hash256 = string;

/** 64-character hex-encoded account/validator address. */
export type Address = string;

/** Hex-encoded bytes without a required prefix. */
export type HexString = string;

/** Hex-encoded bytes with the `0x` prefix used by selected RPC projections. */
export type PrefixedHexString = `0x${string}`;

// ─── Health & Info ───────────────────────────────────────────

export interface HealthResponse {
  status: string;
  version: string;
  height: number;
  peers: number;
  uptime_secs: number;
}

export interface GpuInfo {
  name: string;
  backend: string;
  available: boolean;
}

export interface InfoResponse {
  chain: string;
  version: string;
  block_height: number;
  account_count: number;
  mempool_size: number;
  gpu: GpuInfo | string;
}

export interface NodeInfoResponse {
  validator: string;
  stake: U64;
  tier: string;
  height: number;
  version: string;
  mempool_size: number;
}

export interface StatsResponse {
  chain: string;
  version: string;
  block_height: number;
  total_accounts: number;
  mempool_size: number;
  total_transactions: number;
  indexed_hashes: number;
  indexed_receipts: number;
}

// ─── Blocks ─────────────────────────────────────────────────

export interface BlockHeader {
  height: number;
  timestamp: number;
  parent_hash: Hash256;
  tx_root: Hash256;
  state_root: Hash256;
  proof_hash: Hash256;
  tx_count: number;
  producer: Address;
}

export interface BlockDetail {
  header: BlockHeader;
  tx_hashes: Hash256[];
  hash: Hash256;
}

export interface BlockSummary {
  height: number;
  hash: Hash256;
  parent_hash: Hash256;
  tx_root: Hash256;
  tx_count: number;
  timestamp: number;
  producer: Address;
}

export interface BlocksResponse {
  from: number;
  to: number;
  limit: number;
  count: number;
  blocks: BlockSummary[];
}

export interface BlockTxEntry {
  index: number;
  hash: Hash256;
  from: Address;
  nonce: U64;
  tx_type: string;
  body: TransactionBody;
}

export interface BlockTxsResponse {
  block_height: number;
  tx_count: number;
  offset: number;
  limit: number;
  returned: number;
  transactions: BlockTxEntry[];
}

// ─── Merkle Proofs ──────────────────────────────────────────

export interface MerkleProofSibling {
  hash: Hash256;
  is_left: boolean;
}

export interface MerkleProof {
  leaf: Hash256;
  index: number;
  siblings: MerkleProofSibling[];
  root: Hash256;
}

export interface BlockProofsResponse {
  block_height: number;
  block_hash: Hash256;
  tx_root: Hash256;
  proof_count: number;
  proofs: Array<{
    tx_hash: Hash256;
    leaf: Hash256;
    index: number;
    siblings: MerkleProofSibling[];
    root: Hash256;
  }>;
}

// ─── Transactions ───────────────────────────────────────────

export interface TxReceipt {
  tx_hash: Hash256;
  block_height: number;
  block_hash: Hash256;
  index: number;
  success: boolean;
  gas_used: U64;
  value_commitment: string | null;
  inclusion_proof: string | number[] | null;
  logs?: string[];
}

export interface TxProof {
  tx_hash: Hash256;
  blake3_domain: string;
  merkle_proof: MerkleProof;
  block_height: number;
  block_tx_root: Hash256;
  verified: boolean;
  pedersen_commitment: string | null;
}

export interface TxSignature {
  Ed25519?: {
    public_key: string;
    signature: string;
  };
}

export interface FullTransaction {
  tx_hash: Hash256;
  tx_type: string;
  from: Address;
  nonce: U64;
  fee: U64;
  gas_limit: U64;
  body: TransactionBody;
  signature?: TxSignature;
  block_height?: number;
  block_hash?: Hash256;
  index?: number;
  success?: boolean;
  gas_used?: U64;
}

/**
 * A signed transfer projection that can be normalized to `POST /tx/submit`.
 *
 * `FullTransaction` is the broad read model returned by `/tx/{hash}/full` and
 * is not itself a write contract.  Keeping this transfer-only type separate
 * prevents callers from accidentally POSTing the nested read projection to
 * the node's flat transfer adapter.
 */
export interface SignedTransferTransaction
  extends Omit<FullTransaction, "tx_type" | "body" | "signature"> {
  tx_type: "Transfer";
  body: TransferBody;
  signature: {
    Ed25519: {
      public_key: string;
      signature: string;
    };
  };
  /** Exact domain advertised by `/network/info`; null on pre-v3 chains. */
  transaction_domain: PrefixedHexString | null;
}

export interface TxSubmitResponse {
  tx_hash: Hash256;
  status: string;
}

export interface TxSubmitBatchResponse {
  accepted: number;
  rejected: number;
  tx_hashes: Hash256[];
}

// ─── Transaction Body Variants (0x01 through 0x25) ─────────

export interface TransferBody {
  type: "Transfer";
  to: Address;
  amount: U64;
  amount_commitment: string | null;
}

export interface SettleBody {
  type: "Settle";
  agent_id: Address;
  service_hash: Hash256;
  amount: U64;
  usage_units: U64;
}

export interface SwapBody {
  type: "Swap";
  counterparty: Address;
  offer_amount: U64;
  receive_amount: U64;
  offer_asset: string;
  receive_asset: string;
}

export interface EscrowBody {
  type: "Escrow";
  beneficiary: Address;
  amount: U64;
  conditions_hash: Hash256;
  is_create: boolean;
}

export interface StakeBody {
  type: "Stake";
  amount: U64;
  is_stake: boolean;
  validator: Address;
}

export interface WasmCallBody {
  type: "WasmCall";
  contract: Address;
  function: string;
  calldata: string;
  value: U64;
  gas_limit: U64;
}

export interface MultiSigBody {
  type: "MultiSig";
  signers: Address[];
  threshold: number;
}

export interface DeployContractBody {
  type: "DeployContract";
  bytecode_size: number;
  constructor_args_size: number;
  state_rent_deposit: U64;
}

export interface RegisterAgentBody {
  type: "RegisterAgent";
  agent_name: string;
  endpoint: string;
  protocol: string;
  capabilities_size: number;
}

export interface JoinValidatorBody {
  type: "JoinValidator";
  pubkey: HexString;
  initial_stake: U64;
}

export interface LeaveValidatorBody {
  type: "LeaveValidator";
}

export interface ClaimRewardsBody {
  type: "ClaimRewards";
}

export interface UpdateStakeBody {
  type: "UpdateStake";
  new_stake: U64;
}

export interface GovernanceBody {
  type: "Governance";
  proposal_id: number;
  action: string;
}

export interface BridgeLockBody {
  type: "BridgeLock";
  destination_chain: number;
  destination_address: HexString;
  amount: U64;
}

export interface BridgeMintBody {
  type: "BridgeMint";
  source_chain: number;
  source_tx_hash: Hash256;
  recipient: Address;
  amount: U64;
  merkle_proof_size: number;
}

export interface BatchSettleBody {
  type: "BatchSettle";
  entries: number;
  total_amount: U64;
}

export interface ChannelOpenBody {
  type: "ChannelOpen";
  channel_id: PrefixedHexString;
  counterparty: PrefixedHexString;
  deposit: U64;
  timeout_blocks: number;
}

export interface ChannelCloseBody {
  type: "ChannelClose";
  channel_id: PrefixedHexString;
  opener_balance: U64;
  counterparty_balance: U64;
  state_nonce: U64;
}

export interface ChannelDisputeBody {
  type: "ChannelDispute";
  channel_id: PrefixedHexString;
  opener_balance: U64;
  counterparty_balance: U64;
  state_nonce: U64;
  challenge_period: number;
}

export interface ShardProofBody {
  type: "ShardProof";
  shard_id: number;
  block_height: number;
  tx_count: number;
  proof_size: number;
  prev_state_root: PrefixedHexString;
  post_state_root: PrefixedHexString;
}

export interface InferenceAttestationBody {
  type: "InferenceAttestation";
  model_id: PrefixedHexString;
  input_hash: PrefixedHexString;
  output_hash: PrefixedHexString;
  challenge_period: number;
  bond: number;
}

export interface InferenceChallengeBody {
  type: "InferenceChallenge";
  attestation_hash: PrefixedHexString;
  challenger_output_hash: PrefixedHexString;
  challenger_bond: number;
}

export interface InferenceRegisterBody {
  type: "InferenceRegister";
  tier: number;
  stake_bond: number;
}

export interface InferenceEscrowOpenBody {
  type: "InferenceEscrowOpen";
  request_id: PrefixedHexString;
  model_id: PrefixedHexString;
  max_fee: U64;
  max_tokens: number;
  timeout_blocks: number;
}

export interface InferenceEscrowReleaseBody {
  type: "InferenceEscrowRelease";
  request_id: PrefixedHexString;
  payer: PrefixedHexString;
  model_id: PrefixedHexString;
  max_tokens: number;
  timeout_blocks: number;
  output_hash: PrefixedHexString;
  proposer: PrefixedHexString;
  replicas: PrefixedHexString[];
  observer_pool: PrefixedHexString;
  treasury: PrefixedHexString;
}

export interface InferenceEscrowRefundBody {
  type: "InferenceEscrowRefund";
  request_id: PrefixedHexString;
  model_id: PrefixedHexString;
  max_tokens: number;
  timeout_blocks: number;
}

export interface ModelRegistrationBody {
  type: "ModelRegistration";
  model_id: PrefixedHexString;
  metadata_hash: PrefixedHexString;
  chunk_tree_root: PrefixedHexString;
  n_layers: number;
  d_model: number;
  quantization: string;
  registration_fee: U64;
  royalty_recipient: PrefixedHexString;
}

export interface ModelRequestBody {
  type: "ModelRequest";
  request_id: PrefixedHexString;
  model_id: PrefixedHexString;
  target_k_replication: number;
  bond_per_layer_epoch: number;
  max_wait_secs: number;
}

/** Inclusive start/end layer range serialized by the RPC as a JSON tuple. */
export type LayerRange = [number, number];

export interface ShardCoverageClaimBody {
  type: "ShardCoverageClaim";
  model_id: PrefixedHexString;
  node_pubkey: PrefixedHexString;
  ranges: LayerRange[];
  bond: number;
  epoch_blocks: number;
}

export interface CapacityAdvertisementBody {
  type: "CapacityAdvertisement";
  node_pubkey: PrefixedHexString;
  ram_bytes: number;
  vram_bytes: number;
  bandwidth_mbps: number;
  uptime_hint_mins: number;
  stake: U64;
  region: string;
}

export interface ShardAssignmentEntry {
  node_pubkey: PrefixedHexString;
  model_id: PrefixedHexString;
  ranges: LayerRange[];
}

export interface ShardAssignmentProposalBody {
  type: "ShardAssignmentProposal";
  epoch_blocks: number;
  input_snapshot_hash: PrefixedHexString;
  assignments: ShardAssignmentEntry[];
}

export interface FaucetClaimBody {
  type: "FaucetClaim";
  recipient: Address;
  amount: U64;
}

export interface InferenceRequestBody {
  type: "InferenceRequest";
  request_id: PrefixedHexString;
  model_id: Hash256;
  input_hash: Hash256;
  max_tokens: number;
  tier: number;
  max_reward: U64;
  deadline_blocks: number;
  committee_size: number;
}

export interface InferenceVoteBody {
  type: "InferenceVote";
  request_id: PrefixedHexString;
  output_hash: Hash256;
  output_blob_attached: boolean;
}

export interface InferenceFinalizeBody {
  type: "InferenceFinalize";
  request_id: PrefixedHexString;
}

export interface CommunityInferenceRewardBody {
  type: "CommunityInferenceReward";
  chain_domain: Hash256;
  job_id: Hash256;
  worker: Address;
  model_id: Hash256;
  input_hash: Hash256;
  output_hash: Hash256;
  max_tokens: number;
  expires_at_height: number;
  worker_attestation_hash: Hash256;
}

/** Discriminated union of all 37 ARC Chain transaction body projections. */
export type TransactionBody =
  | TransferBody
  | SettleBody
  | SwapBody
  | EscrowBody
  | StakeBody
  | WasmCallBody
  | MultiSigBody
  | DeployContractBody
  | RegisterAgentBody
  | JoinValidatorBody
  | LeaveValidatorBody
  | ClaimRewardsBody
  | UpdateStakeBody
  | GovernanceBody
  | BridgeLockBody
  | BridgeMintBody
  | BatchSettleBody
  | ChannelOpenBody
  | ChannelCloseBody
  | ChannelDisputeBody
  | ShardProofBody
  | InferenceAttestationBody
  | InferenceChallengeBody
  | InferenceRegisterBody
  | InferenceEscrowOpenBody
  | InferenceEscrowReleaseBody
  | InferenceEscrowRefundBody
  | ModelRegistrationBody
  | ModelRequestBody
  | ShardCoverageClaimBody
  | CapacityAdvertisementBody
  | ShardAssignmentProposalBody
  | FaucetClaimBody
  | InferenceRequestBody
  | InferenceVoteBody
  | InferenceFinalizeBody
  | CommunityInferenceRewardBody;

/** String literal union of all transaction type discriminators. */
export type TransactionType = TransactionBody["type"];

// ─── Accounts ───────────────────────────────────────────────

export interface Account {
  address: Address;
  balance: U64;
  nonce: U64;
  code_hash: Hash256;
  storage_root: Hash256;
  staked_balance: U64;
}

export interface AccountTxs {
  address: Address;
  tx_count: number;
  tx_hashes: Hash256[];
}

// ─── Validators ─────────────────────────────────────────────

export interface ValidatorInfo {
  address: Address;
  stake: U64;
  tier: string;
}

export interface ValidatorsResponse {
  validators: ValidatorInfo[];
  total_stake: U64;
  count: number;
}

// ─── Contracts ──────────────────────────────────────────────

export interface ContractInfo {
  address: Address;
  bytecode_size: number;
  code_hash: Hash256;
  is_wasm: boolean;
}

export interface ContractEvent {
  topic: string;
  data: string;
}

export interface ContractCallResult {
  success: boolean;
  gas_used?: U64;
  return_data?: string;
  logs?: string[];
  events?: ContractEvent[];
  error?: string;
}

// ─── Light Client ───────────────────────────────────────────

export interface LightSnapshot {
  height: number;
  state_root: Hash256;
  account_count: number;
  total_supply: U64;
  latest_block_hash: Hash256;
}

export interface SyncSnapshotInfo {
  available: boolean;
  height: number;
  state_root: Hash256;
  account_count: number;
}

// ─── Faucet ─────────────────────────────────────────────────

export interface FaucetClaimResponse {
  tx_hash: Hash256;
  amount: U64;
  message: string;
}

export interface FaucetStatus {
  address: Address;
  node_url: string;
  claims_today: number;
  claim_amount: U64;
  rate_limit_secs: number;
}

export interface FaucetHealth {
  status: string;
  faucet_address: Address;
}

// ─── ETH JSON-RPC ───────────────────────────────────────────

export interface JsonRpcRequest {
  jsonrpc: "2.0";
  method: string;
  params: unknown[];
  id: number | string;
}

export interface JsonRpcResponse<T = unknown> {
  jsonrpc: "2.0";
  id: number | string;
  result?: T;
  error?: JsonRpcError;
}

export interface JsonRpcError {
  code: number;
  message: string;
  data?: unknown;
}

// ─── Client Options ─────────────────────────────────────────

export interface BlocksQueryOptions {
  from?: number;
  to?: number;
  limit?: number;
}

export interface BlockTxsQueryOptions {
  offset?: number;
  limit?: number;
}

export interface ContractCallOptions {
  calldata?: string;
  from?: Address;
  gasLimit?: U64;
}

export interface TxSubmitPayload {
  from: Address;
  to: Address;
  amount: U64;
  nonce: U64;
  fee: U64;
  tx_type?: "Transfer";
  signature: string;
  public_key: string;
  /** Exact domain used by the signer; checked locally and never sent. */
  transaction_domain: PrefixedHexString | null;
}

/** Minimal recovery metadata required to bind transaction signatures. */
export interface TransactionDomainInfo {
  protocol_version?: string | null;
  recovery_active?: boolean;
  transaction_domain: PrefixedHexString | null;
}
