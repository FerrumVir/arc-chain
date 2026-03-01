// ─── Health & Info ────────────────────────────────────────────────

export interface HealthResponse {
  status: string;
  version: string;
  height: number;
  peers: number;
  uptime_secs: number;
}

export interface InfoResponse {
  chain: string;
  version: string;
  block_height: number;
  account_count: number;
  mempool_size: number;
  gpu: string | { available: boolean; name: string; backend: string };
}

export interface NodeInfoResponse {
  validator: string;
  stake: number;
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
  total_receipts: number;
}

// ─── Blocks ──────────────────────────────────────────────────────

export interface BlockSummary {
  height: number;
  hash: string;
  parent_hash: string;
  tx_root: string;
  tx_count: number;
  timestamp: number;
  producer: string;
}

export interface BlocksResponse {
  from: number;
  to: number;
  limit: number;
  count: number;
  blocks: BlockSummary[];
}

export interface BlockHeader {
  height: number;
  timestamp: number;
  parent_hash: string;
  tx_root: string;
  state_root: string;
  proof_hash: string;
  tx_count: number;
  producer: string;
}

export interface BlockDetail {
  header: BlockHeader;
  tx_hashes: string[];
  hash: string;
}

// ─── Transactions ────────────────────────────────────────────────

export interface TxReceipt {
  tx_hash: string;
  block_height: number;
  block_hash: string;
  index: number;
  success: boolean;
  gas_used: number;
  value_commitment: string | null;
  inclusion_proof: string | number[] | null;
}

export interface TxProof {
  tx_hash: string;
  block_height: number;
  merkle_root: string;
  proof_nodes: string[];
  index: number;
  verified: boolean;
}

// ─── Accounts ────────────────────────────────────────────────────

export interface AccountInfo {
  balance: number;
  nonce: number;
  address?: string;
  [key: string]: unknown;
}

export interface AccountTxsResponse {
  address: string;
  tx_count: number;
  tx_hashes: string[];
}
