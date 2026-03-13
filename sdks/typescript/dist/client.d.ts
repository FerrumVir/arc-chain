/**
 * ARC Chain SDK — RPC client.
 *
 * Typed HTTP client for all ARC Chain RPC endpoints.
 * Uses the native `fetch` API (Node 18+, Deno, Bun, browsers).
 */
import type { Account, BatchResult, Block, BlockSummary, ChainInfo, ChainStats, ContractCallResult, ContractInfo, EthRpcResponse, HealthInfo, LightSnapshot, NodeInfo, Receipt, SyncSnapshotInfo, Transaction } from "./types";
export declare class ArcError extends Error {
    statusCode?: number;
    detail?: string;
    constructor(message: string, statusCode?: number, detail?: string);
}
export declare class ArcConnectionError extends ArcError {
    url?: string;
    cause?: Error;
    constructor(message: string, url?: string, cause?: Error);
}
export declare class ArcTransactionError extends ArcError {
    txHash?: string;
    constructor(message: string, txHash?: string, statusCode?: number);
}
export interface ArcClientOptions {
    /** Request timeout in milliseconds (default 30000). */
    timeout?: number;
    /** Extra HTTP headers to include in every request. */
    headers?: Record<string, string>;
}
/**
 * HTTP client for the ARC Chain RPC API.
 *
 * Usage:
 * ```ts
 * const client = new ArcClient("http://localhost:9000");
 * const info = await client.getChainInfo();
 * console.log(info.block_height);
 * ```
 */
export declare class ArcClient {
    readonly rpcUrl: string;
    private timeout;
    private headers;
    constructor(rpcUrl: string, options?: ArcClientOptions);
    private _get;
    private _post;
    /**
     * GET /block/{height} -- Fetch a block by height.
     */
    getBlock(height: number): Promise<Block>;
    /**
     * GET /blocks -- Paginated block listing.
     */
    getBlocks(fromHeight?: number, toHeight?: number, limit?: number): Promise<{
        from: number;
        to: number;
        count: number;
        blocks: BlockSummary[];
    }>;
    /**
     * GET /block/{height}/txs -- Paginated transaction listing for a block.
     */
    getBlockTxs(height: number, offset?: number, limit?: number): Promise<Record<string, unknown>>;
    /**
     * GET /block/{height}/proofs -- All Merkle proofs for transactions in a block.
     */
    getBlockProofs(height: number): Promise<Record<string, unknown>>;
    /**
     * GET /account/{address} -- Fetch an account by address.
     */
    getAccount(address: string): Promise<Account>;
    /**
     * GET /account/{address}/txs -- Transaction hashes involving an account.
     */
    getAccountTxs(address: string): Promise<{
        address: string;
        tx_count: number;
        tx_hashes: string[];
    }>;
    /**
     * POST /tx/submit -- Submit a transaction to the mempool.
     *
     * Accepts either a raw RPC-format object or a signed Transaction from
     * TransactionBuilder.sign().
     *
     * @returns Transaction hash string.
     */
    submitTransaction(tx: Record<string, unknown> | Transaction): Promise<string>;
    /**
     * GET /tx/{hash} -- Look up a transaction receipt by hash.
     */
    getTransaction(txHash: string): Promise<Receipt>;
    /**
     * GET /tx/{hash}/full -- Full transaction body with type-specific fields.
     */
    getFullTransaction(txHash: string): Promise<Record<string, unknown>>;
    /**
     * GET /tx/{hash}/proof -- Merkle inclusion proof for a transaction.
     */
    getTxProof(txHash: string): Promise<Record<string, unknown>>;
    /**
     * POST /tx/submit_batch -- Submit multiple transactions.
     */
    submitBatch(txs: Array<Record<string, unknown>>): Promise<BatchResult>;
    /**
     * GET /info -- Chain information.
     */
    getChainInfo(): Promise<ChainInfo>;
    /**
     * GET /stats -- Chain statistics.
     */
    getStats(): Promise<ChainStats>;
    /**
     * GET /health -- Node health status.
     */
    getHealth(): Promise<HealthInfo>;
    /**
     * GET /node/info -- Validator node information.
     */
    getNodeInfo(): Promise<NodeInfo>;
    /**
     * GET /contract/{address} -- Contract bytecode info.
     */
    getContractInfo(address: string): Promise<ContractInfo>;
    /**
     * POST /contract/{address}/call -- Read-only contract call.
     */
    callContract(address: string, func: string, calldata?: string, fromAddr?: string, gasLimit?: number): Promise<ContractCallResult>;
    /**
     * GET /light/snapshot -- Lightweight snapshot for light client bootstrapping.
     */
    getLightSnapshot(): Promise<LightSnapshot>;
    /**
     * GET /sync/snapshot/info -- Metadata about available state sync snapshot.
     */
    getSyncSnapshotInfo(): Promise<SyncSnapshotInfo>;
    /**
     * POST /eth -- Send an ETH-compatible JSON-RPC request.
     *
     * Supports methods like eth_chainId, eth_blockNumber,
     * eth_getBalance, eth_call, eth_estimateGas, etc.
     */
    ethCall(method: string, params?: unknown[]): Promise<EthRpcResponse>;
}
//# sourceMappingURL=client.d.ts.map