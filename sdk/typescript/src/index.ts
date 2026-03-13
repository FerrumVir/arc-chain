// ─── @arc-chain/sdk ───────────────────────────────────────────
// TypeScript SDK for the ARC Chain RPC API.
//
// Usage:
//   import { ArcClient } from "@arc-chain/sdk";
//
//   const client = new ArcClient("http://localhost:9090");
//   const health = await client.getHealth();
//   const block  = await client.getBlock(42);
//   const tx     = await client.getTxFull(block.tx_hashes[0]);

// ─── Client ─────────────────────────────────────────────────

export { ArcClient, ArcRpcError } from "./client";

// ─── Utilities ──────────────────────────────────────────────

export {
  isValidAddress,
  isValidTxHash,
  formatHash,
  formatArc,
  hexToBytes,
  bytesToHex,
  stripHexPrefix,
  addHexPrefix,
} from "./utils";

// ─── Types ──────────────────────────────────────────────────

export type {
  // Primitives
  Hash256,
  Address,

  // Health & Info
  HealthResponse,
  GpuInfo,
  InfoResponse,
  NodeInfoResponse,
  StatsResponse,

  // Blocks
  BlockHeader,
  BlockDetail,
  BlockSummary,
  BlocksResponse,
  BlockTxEntry,
  BlockTxsResponse,

  // Merkle Proofs
  MerkleProofSibling,
  MerkleProof,
  BlockProofsResponse,

  // Transactions
  TxReceipt,
  TxProof,
  TxSignature,
  FullTransaction,
  TxSubmitResponse,
  TxSubmitBatchResponse,

  // Transaction Body Variants (all 21)
  TransactionBody,
  TransactionType,
  TransferBody,
  SettleBody,
  SwapBody,
  EscrowBody,
  StakeBody,
  WasmCallBody,
  MultiSigBody,
  DeployContractBody,
  RegisterAgentBody,
  JoinValidatorBody,
  LeaveValidatorBody,
  ClaimRewardsBody,
  UpdateStakeBody,
  GovernanceBody,
  BridgeLockBody,
  BridgeMintBody,
  BatchSettleBody,
  ChannelOpenBody,
  ChannelCloseBody,
  ChannelDisputeBody,
  ShardProofBody,

  // Accounts
  Account,
  AccountTxs,

  // Validators
  ValidatorInfo,
  ValidatorsResponse,

  // Contracts
  ContractInfo,
  ContractEvent,
  ContractCallResult,

  // Light Client
  LightSnapshot,
  SyncSnapshotInfo,

  // Faucet
  FaucetClaimResponse,
  FaucetStatus,
  FaucetHealth,

  // ETH JSON-RPC
  JsonRpcRequest,
  JsonRpcResponse,
  JsonRpcError,

  // Client Options
  BlocksQueryOptions,
  BlockTxsQueryOptions,
  ContractCallOptions,
  TxSubmitPayload,
} from "./types";
