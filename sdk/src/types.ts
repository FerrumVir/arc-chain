// ─── ARC Chain Core Types ─────────────────────────────────────
// Agent Runtime Chain — TypeScript SDK

/** 32-byte hex string (0x-prefixed, 66 chars) */
export type Hash256 = string;

/** 20-byte hex string (0x-prefixed, 42 chars) */
export type Address = string;

/** Transaction types supported by ARC Chain */
export enum TxType {
  Transfer = "Transfer",
  Settle = "Settle",
  Swap = "Swap",
  Escrow = "Escrow",
  Stake = "Stake",
  WasmCall = "WasmCall",
  MultiSig = "MultiSig",
}

/** Raw transaction before signing */
export interface UnsignedTransaction {
  tx_type: TxType;
  from: Address;
  to: Address;
  amount: bigint;
  nonce: number;
  data?: Uint8Array;
  gas_limit?: number;
}

/** Signed transaction ready for submission */
export interface SignedTransaction extends UnsignedTransaction {
  hash: Hash256;
  signature: string;
}

/** Transaction receipt returned after execution */
export interface TxReceipt {
  tx_hash: Hash256;
  block_height: number;
  success: boolean;
  gas_used: number;
  error?: string;
}

/** Block header */
export interface BlockHeader {
  height: number;
  hash: Hash256;
  parent_hash: Hash256;
  state_root: Hash256;
  tx_root: Hash256;
  timestamp: number;
  producer: Address;
  tx_count: number;
}

/** Full block with transactions */
export interface Block extends BlockHeader {
  transactions: SignedTransaction[];
}

/** Account state */
export interface AccountState {
  address: Address;
  balance: bigint;
  nonce: number;
  code_hash: Hash256 | null;
  storage_root: Hash256;
}

/** Node info from /info endpoint */
export interface NodeInfo {
  version: string;
  chain_height: number;
  peer_count: number;
  syncing: boolean;
  node_id: string;
}

/** Merkle inclusion proof */
export interface MerkleProof {
  leaf: Hash256;
  root: Hash256;
  path: Hash256[];
  indices: number[];
}

/** Agent configuration for autonomous operation */
export interface AgentConfig {
  /** Agent wallet address */
  address: Address;
  /** Maximum gas per transaction */
  maxGas?: number;
  /** Maximum spend per transaction (in smallest unit) */
  maxSpend?: bigint;
  /** Allowed transaction types */
  allowedTypes?: TxType[];
  /** Auto-retry failed transactions */
  autoRetry?: boolean;
  /** Maximum retries */
  maxRetries?: number;
}

/** Escrow parameters */
export interface EscrowParams {
  /** Amount to escrow */
  amount: bigint;
  /** Beneficiary address */
  beneficiary: Address;
  /** Timeout in seconds */
  timeout: number;
  /** Condition hash (for release) */
  conditionHash?: Hash256;
}

/** Batch submission result */
export interface BatchResult {
  submitted: number;
  receipts: TxReceipt[];
  failed: { index: number; error: string }[];
}
