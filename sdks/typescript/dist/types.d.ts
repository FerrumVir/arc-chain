/**
 * ARC Chain SDK — TypeScript type definitions.
 *
 * Interfaces matching the ARC Chain RPC response shapes.
 */
/** Transaction type discriminant matching the Rust TxType enum. */
export type TxType = "Transfer" | "Settle" | "Swap" | "Escrow" | "Stake" | "WasmCall" | "MultiSig" | "DeployContract" | "RegisterAgent" | "JoinValidator" | "LeaveValidator" | "ClaimRewards" | "UpdateStake";
/** Transfer body. */
export interface TransferBody {
    type: "Transfer";
    to: string;
    amount: number;
    amount_commitment?: string | null;
}
/** Contract deployment body. */
export interface DeployContractBody {
    type: "DeployContract";
    bytecode: string;
    constructor_args: string;
    state_rent_deposit: number;
}
/** WASM contract call body. */
export interface WasmCallBody {
    type: "WasmCall";
    contract: string;
    function: string;
    calldata: string;
    value: number;
    gas_limit: number;
}
/** Stake/unstake body. */
export interface StakeBody {
    type: "Stake";
    amount: number;
    is_stake: boolean;
    validator: string;
}
/** Settlement body. */
export interface SettleBody {
    type: "Settle";
    agent_id: string;
    service_hash: string;
    amount: number;
    usage_units: number;
    amount_commitment?: string | null;
}
/** Union of all transaction body types. */
export type TxBody = TransferBody | DeployContractBody | WasmCallBody | StakeBody | SettleBody;
/** Ed25519 signature payload. */
export interface Ed25519Signature {
    Ed25519: {
        public_key: string;
        signature: string;
    };
}
/** An unsigned or signed transaction. */
export interface Transaction {
    tx_type: TxType;
    from: string;
    nonce: number;
    fee: number;
    gas_limit: number;
    body: TxBody;
    hash: string;
    signature: Ed25519Signature | null;
    to?: string;
    amount?: number;
}
export interface Account {
    address: string;
    balance: number;
    nonce: number;
    code_hash?: string;
    storage_root?: string;
}
export interface BlockHeader {
    height: number;
    parent_hash: string;
    tx_root: string;
    state_root: string;
    tx_count: number;
    timestamp: number;
    producer: string;
}
export interface Block {
    hash: string;
    header: BlockHeader;
    tx_hashes: string[];
}
export interface BlockSummary {
    height: number;
    hash: string;
    parent_hash: string;
    tx_root: string;
    tx_count: number;
    timestamp: number;
    producer: string;
}
export interface EventLog {
    address: string;
    topics: string[];
    data: string;
    block_height: number;
    tx_hash: string;
    log_index: number;
}
export interface Receipt {
    tx_hash: string;
    block_height: number;
    block_hash: string;
    index: number;
    success: boolean;
    gas_used: number;
    value_commitment?: string | null;
    inclusion_proof?: string | null;
    logs: EventLog[];
}
export interface ChainInfo {
    chain: string;
    version: string;
    block_height: number;
    account_count: number;
    mempool_size: number;
    gpu?: Record<string, unknown>;
}
export interface ChainStats {
    chain: string;
    version: string;
    block_height: number;
    total_accounts: number;
    mempool_size: number;
    total_transactions: number;
    indexed_hashes: number;
    indexed_receipts: number;
}
export interface HealthInfo {
    status: string;
    version: string;
    height: number;
    peers: number;
    uptime_secs: number;
}
export interface NodeInfo {
    validator: string;
    stake: number;
    tier: string;
    height: number;
    version: string;
    mempool_size: number;
}
export interface SubmitResult {
    tx_hash: string;
    status: string;
}
export interface BatchResult {
    accepted: number;
    rejected: number;
    tx_hashes: string[];
}
export interface EthRpcResponse {
    jsonrpc: string;
    id: number;
    result?: unknown;
    error?: {
        code: number;
        message: string;
    };
}
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
    error?: string;
    logs?: string[];
    events?: Array<{
        topic: string;
        data: string;
    }>;
}
export interface LightSnapshot {
    height: number;
    state_root: string;
    account_count: number;
    total_supply: number;
    latest_block_hash: string;
}
export interface SyncSnapshotInfo {
    available: boolean;
    height: number;
    state_root: string;
    account_count: number;
}
//# sourceMappingURL=types.d.ts.map