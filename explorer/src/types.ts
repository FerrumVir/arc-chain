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
  total_transactions: number;
  indexed_hashes: number;
  indexed_receipts: number;
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

// ─── Full Transaction ────────────────────────────────────────────

export interface FullTransaction {
  tx_hash: string;
  tx_type: string;
  from: string;
  nonce: number;
  fee: number;
  gas_limit: number;
  body: TransactionBody;
  block_height?: number;
  block_hash?: string;
  index?: number;
  success?: boolean;
  gas_used?: number;
}

export type TransactionBody =
  | { type: 'Transfer'; to: string; amount: number; amount_commitment: string | null }
  | { type: 'Settle'; agent_id: string; service_hash: string; amount: number; usage_units: number }
  | { type: 'Swap'; counterparty: string; offer_amount: number; receive_amount: number; offer_asset: string; receive_asset: string }
  | { type: 'Escrow'; beneficiary: string; amount: number; conditions_hash: string; is_create: boolean }
  | { type: 'Stake'; amount: number; is_stake: boolean; validator: string }
  | { type: 'WasmCall'; contract: string; function: string; calldata: string; value: number; gas_limit: number }
  | { type: 'MultiSig'; signers: string[]; threshold: number }
  | { type: 'DeployContract'; bytecode_size: number; constructor_args_size: number; state_rent_deposit: number }
  | { type: 'RegisterAgent'; agent_name: string; endpoint: string; protocol: string; capabilities_size: number };

// ─── Contract ───────────────────────────────────────────────────

export interface ContractInfo {
  address: string;
  bytecode_size: number;
  code_hash: string;
  is_wasm: boolean;
}

export interface ContractCallResult {
  success: boolean;
  gas_used?: number;
  return_data?: string;
  logs?: string[];
  events?: Array<{ topic: string; data: string }>;
  error?: string;
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
