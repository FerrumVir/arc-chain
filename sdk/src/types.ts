export type Hash256 = string;

export type Address = string;

export enum TxType {
  Transfer = "Transfer",
  Settle = "Settle",
  Swap = "Swap",
  Escrow = "Escrow",
  Stake = "Stake",
  WasmCall = "WasmCall",
  MultiSig = "MultiSig",
}

export interface UnsignedTransaction {
  tx_type: TxType;
  from: Address;
  to: Address;
  amount: bigint;
  nonce: number;
  data?: Uint8Array;
  gas_limit?: number;
}

export interface SignedTransaction extends UnsignedTransaction {
  hash: Hash256;
  signature: string;
}

export interface TxReceipt {
  tx_hash: Hash256;
  block_height: number;
  success: boolean;
  gas_used: number;
  error?: string;
}

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

export interface Block extends BlockHeader {
  transactions: SignedTransaction[];
}

export interface AccountState {
  address: Address;
  balance: bigint;
  nonce: number;
  code_hash: Hash256 | null;
  storage_root: Hash256;
}

export interface NodeInfo {
  version: string;
  chain_height: number;
  peer_count: number;
  syncing: boolean;
  node_id: string;
}

export interface MerkleProof {
  leaf: Hash256;
  root: Hash256;
  path: Hash256[];
  indices: number[];
}

export interface AgentConfig {
  address: Address;
  maxGas?: number;
  maxSpend?: bigint;
  allowedTypes?: TxType[];
  autoRetry?: boolean;
  maxRetries?: number;
}

export interface EscrowParams {
  amount: bigint;
  beneficiary: Address;
  timeout: number;
  conditionHash?: Hash256;
}

export interface BatchResult {
  submitted: number;
  receipts: TxReceipt[];
  failed: { index: number; error: string }[];
}
