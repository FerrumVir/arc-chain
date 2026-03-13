/** 64-character hex-encoded BLAKE3 hash (no 0x prefix on the native API). */
export type Hash256 = string;
/** 64-character hex-encoded account/validator address. */
export type Address = string;
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
    nonce: number;
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
export interface TxReceipt {
    tx_hash: Hash256;
    block_height: number;
    block_hash: Hash256;
    index: number;
    success: boolean;
    gas_used: number;
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
    nonce: number;
    fee: number;
    gas_limit: number;
    body: TransactionBody;
    signature?: TxSignature;
    block_height?: number;
    block_hash?: Hash256;
    index?: number;
    success?: boolean;
    gas_used?: number;
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
export interface TransferBody {
    type: "Transfer";
    to: Address;
    amount: number;
    amount_commitment: string | null;
}
export interface SettleBody {
    type: "Settle";
    agent_id: Address;
    service_hash: Hash256;
    amount: number;
    usage_units: number;
}
export interface SwapBody {
    type: "Swap";
    counterparty: Address;
    offer_amount: number;
    receive_amount: number;
    offer_asset: string;
    receive_asset: string;
}
export interface EscrowBody {
    type: "Escrow";
    beneficiary: Address;
    amount: number;
    conditions_hash: Hash256;
    is_create: boolean;
}
export interface StakeBody {
    type: "Stake";
    amount: number;
    is_stake: boolean;
    validator: Address;
}
export interface WasmCallBody {
    type: "WasmCall";
    contract: Address;
    function: string;
    calldata: string;
    value: number;
    gas_limit: number;
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
    state_rent_deposit: number;
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
    pubkey: number[];
    initial_stake: number;
}
export interface LeaveValidatorBody {
    type: "LeaveValidator";
}
export interface ClaimRewardsBody {
    type: "ClaimRewards";
}
export interface UpdateStakeBody {
    type: "UpdateStake";
    new_stake: number;
}
export interface GovernanceBody {
    type: "Governance";
    proposal_id: number;
    action: string;
}
export interface BridgeLockBody {
    type: "BridgeLock";
    destination_chain: number;
    destination_address: number[];
    amount: number;
}
export interface BridgeMintBody {
    type: "BridgeMint";
    source_chain: number;
    source_tx_hash: Hash256;
    recipient: Address;
    amount: number;
    merkle_proof: number[];
}
export interface BatchSettleBody {
    type: "BatchSettle";
    entries: Array<{
        agent_id: Address;
        service_hash: Hash256;
        amount: number;
    }>;
}
export interface ChannelOpenBody {
    type: "ChannelOpen";
    channel_id: Hash256;
    counterparty: Address;
    deposit: number;
    timeout_blocks: number;
}
export interface ChannelCloseBody {
    type: "ChannelClose";
    channel_id: Hash256;
    opener_balance: number;
    counterparty_balance: number;
    counterparty_sig: number[];
    state_nonce: number;
}
export interface ChannelDisputeBody {
    type: "ChannelDispute";
    channel_id: Hash256;
    opener_balance: number;
    counterparty_balance: number;
    other_party_sig: number[];
    state_nonce: number;
    challenge_period: number;
}
export interface ShardProofBody {
    type: "ShardProof";
    shard_id: number;
    block_height: number;
    block_hash: Hash256;
    prev_state_root: Hash256;
    post_state_root: Hash256;
    tx_count: number;
    proof_data: number[];
}
/** Discriminated union of all 21 ARC Chain transaction body types. */
export type TransactionBody = TransferBody | SettleBody | SwapBody | EscrowBody | StakeBody | WasmCallBody | MultiSigBody | DeployContractBody | RegisterAgentBody | JoinValidatorBody | LeaveValidatorBody | ClaimRewardsBody | UpdateStakeBody | GovernanceBody | BridgeLockBody | BridgeMintBody | BatchSettleBody | ChannelOpenBody | ChannelCloseBody | ChannelDisputeBody | ShardProofBody;
/** String literal union of all transaction type discriminators. */
export type TransactionType = TransactionBody["type"];
export interface Account {
    address: Address;
    balance: number;
    nonce: number;
    code_hash: Hash256;
    storage_root: Hash256;
    staked_balance: number;
}
export interface AccountTxs {
    address: Address;
    tx_count: number;
    tx_hashes: Hash256[];
}
export interface ValidatorInfo {
    address: Address;
    stake: number;
    tier: string;
}
export interface ValidatorsResponse {
    validators: ValidatorInfo[];
    total_stake: number;
    count: number;
}
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
    gas_used?: number;
    return_data?: string;
    logs?: string[];
    events?: ContractEvent[];
    error?: string;
}
export interface LightSnapshot {
    height: number;
    state_root: Hash256;
    account_count: number;
    total_supply: number;
    latest_block_hash: Hash256;
}
export interface SyncSnapshotInfo {
    available: boolean;
    height: number;
    state_root: Hash256;
    account_count: number;
}
export interface FaucetClaimResponse {
    tx_hash: Hash256;
    amount: number;
    message: string;
}
export interface FaucetStatus {
    address: Address;
    node_url: string;
    claims_today: number;
    claim_amount: number;
    rate_limit_secs: number;
}
export interface FaucetHealth {
    status: string;
    faucet_address: Address;
}
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
    gasLimit?: number;
}
export interface TxSubmitPayload {
    from: Address;
    to: Address;
    amount: number;
    nonce: number;
    tx_type?: string;
}
//# sourceMappingURL=types.d.ts.map