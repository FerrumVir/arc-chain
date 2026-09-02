/**
 * ARC Chain SDK - TypeScript type definitions.
 *
 * Interfaces matching the ARC Chain RPC response shapes.
 */
import type { U64 } from "./u64";
export type { U64 } from "./u64";
/** 32-byte hex-encoded hash (BLAKE3 digest, no `0x` prefix). */
export type Hash256 = string;
/** 32-byte hex-encoded account address (BLAKE3 of pubkey). */
export type Address = string;
/** Transaction type discriminant matching the Rust TxType enum. */
export type TxType = "Transfer" | "Settle" | "Swap" | "Escrow" | "Stake" | "WasmCall" | "MultiSig" | "DeployContract" | "RegisterAgent" | "JoinValidator" | "LeaveValidator" | "ClaimRewards" | "UpdateStake";
/** Transfer body. */
export interface TransferBody {
    type: "Transfer";
    to: string;
    amount: U64;
    amount_commitment?: string | null;
}
/** Contract deployment body. */
export interface DeployContractBody {
    type: "DeployContract";
    bytecode: string;
    constructor_args: string;
    state_rent_deposit: U64;
}
/** WASM contract call body. */
export interface WasmCallBody {
    type: "WasmCall";
    contract: string;
    function: string;
    calldata: string;
    value: U64;
    gas_limit: U64;
}
/** Stake/unstake body. */
export interface StakeBody {
    type: "Stake";
    amount: U64;
    is_stake: boolean;
    validator: string;
}
/** Settlement body. */
export interface SettleBody {
    type: "Settle";
    agent_id: string;
    service_hash: string;
    amount: U64;
    usage_units: U64;
    amount_commitment?: string | null;
}
/** Channel open body. */
export interface ChannelOpenBody {
    type: "ChannelOpen";
    channel_id: Hash256;
    counterparty: Address;
    deposit: number;
    timeout_blocks: number;
}
/** Channel close body. */
export interface ChannelCloseBody {
    type: "ChannelClose";
    channel_id: Hash256;
    opener_balance: number;
    counterparty_balance: number;
    counterparty_sig: number[];
    state_nonce: number;
}
/** Channel dispute body. */
export interface ChannelDisputeBody {
    type: "ChannelDispute";
    channel_id: Hash256;
    opener_balance: number;
    counterparty_balance: number;
    other_party_sig: number[];
    state_nonce: number;
    challenge_period: number;
}
/** Union of all transaction body types. */
export type TxBody = TransferBody | DeployContractBody | WasmCallBody | StakeBody | SettleBody | ChannelOpenBody | ChannelCloseBody | ChannelDisputeBody;
/** Alias for TxBody used by older code. */
export type TransactionBody = TxBody;
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
    nonce: U64;
    fee: U64;
    gas_limit: U64;
    body: TxBody;
    hash: string;
    signature: Ed25519Signature | null;
    /** Exact domain used for the signing hash; null on pre-v3 chains. */
    transaction_domain: string | null;
    to?: string;
    amount?: U64;
}
/** Transfer builder output before signing. */
export interface TransferTransaction extends Omit<Transaction, "tx_type" | "body"> {
    tx_type: "Transfer";
    body: TransferBody;
}
/** Transfer builder output after signing. */
export interface SignedTransferTransaction extends Omit<TransferTransaction, "signature"> {
    signature: Ed25519Signature;
}
/** Exact flat write contract accepted by `POST /tx/submit`. */
export interface SignedTransferSubmitPayload {
    from: Address;
    to: Address;
    amount: U64;
    nonce: U64;
    fee: U64;
    tx_type?: "Transfer";
    signature: string;
    public_key: string;
    /** Checked against `/network/info` and omitted from the HTTP body. */
    transaction_domain: string | null;
}
export interface Account {
    address: string;
    balance: U64;
    nonce: U64;
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
    gas_used: U64;
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
    stake: U64;
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
    gas_used?: U64;
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
    total_supply: U64;
    latest_block_hash: string;
}
export interface SyncSnapshotInfo {
    available: boolean;
    height: number;
    state_root: string;
    account_count: number;
}
//# sourceMappingURL=types.d.ts.map