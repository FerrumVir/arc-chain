// ARC Chain Explorer Types

export type TxType =
  | "Transfer"
  | "Settle"
  | "Swap"
  | "Escrow"
  | "Stake"
  | "WasmCall"
  | "MultiSig";

export interface Transaction {
  hash: string;
  tx_type: TxType;
  from: string;
  to: string;
  amount: number;
  nonce: number;
  gas_used: number;
  byte_size: number;
  success: boolean;
  timestamp: number;
  block_height: number;
}

export interface Block {
  height: number;
  hash: string;
  parent_hash: string;
  state_root: string;
  tx_root: string;
  tx_count: number;
  timestamp: number;
  producer: string;
  transactions: Transaction[];
}

export interface Account {
  address: string;
  balance: number;
  nonce: number;
  code_hash: string | null;
  is_contract: boolean;
  tx_count: number;
}

export interface NetworkStats {
  chain_height: number;
  total_transactions: number;
  total_accounts: number;
  tps_current: number;
  tps_peak: number;
  total_staked: number;
  node_count: number;
  uptime_seconds: number;
  visa_comparison: string;
  avg_tx_size: number;
}

export interface NodeInfo {
  peer_id: string;
  version: string;
  region: string;
  stake: number;
  status: "active" | "syncing" | "offline";
  blocks_produced: number;
  uptime_pct: number;
}
