import type { Address, Hash256, HealthResponse, InfoResponse, NodeInfoResponse, StatsResponse, BlockDetail, BlocksResponse, BlocksQueryOptions, BlockTxsResponse, BlockTxsQueryOptions, BlockProofsResponse, TxReceipt, TxProof, FullTransaction, TxSubmitResponse, TxSubmitBatchResponse, TxSubmitPayload, Account, AccountTxs, ValidatorsResponse, ContractInfo, ContractCallResult, ContractCallOptions, LightSnapshot, SyncSnapshotInfo, FaucetClaimResponse, FaucetStatus, FaucetHealth } from "./types";
/**
 * Error thrown when an RPC request fails.
 * Carries the HTTP status code and the response body text.
 */
export declare class ArcRpcError extends Error {
    /** HTTP status code (e.g. 400, 404, 409, 500). */
    readonly statusCode: number;
    /** Raw response body from the node. */
    readonly body: string;
    constructor(statusCode: number, body: string);
}
/**
 * ARC Chain RPC client.
 *
 * Wraps the full ARC native REST API with typed methods.
 * Uses the built-in `fetch` — no external dependencies required.
 *
 * @example
 * ```ts
 * import { ArcClient } from "@arc-chain/sdk";
 *
 * const client = new ArcClient("http://localhost:9090");
 *
 * const health = await client.getHealth();
 * console.log(health.status); // "ok"
 *
 * const block = await client.getBlock(42);
 * console.log(block.header.tx_count);
 * ```
 */
export declare class ArcClient {
    private readonly baseUrl;
    private readonly timeout;
    private readonly headers;
    /**
     * Create a new ARC Chain RPC client.
     *
     * @param rpcUrl - Base URL of the ARC Chain node (e.g. `"http://localhost:9090"`).
     * @param options - Optional configuration.
     * @param options.timeout - Request timeout in milliseconds (default: 30000).
     * @param options.headers - Additional headers to include on every request.
     */
    constructor(rpcUrl: string, options?: {
        timeout?: number;
        headers?: Record<string, string>;
    });
    /** `GET /health` — Node health check. */
    getHealth(): Promise<HealthResponse>;
    /** `GET /info` — Chain information including GPU status. */
    getInfo(): Promise<InfoResponse>;
    /** `GET /node/info` — Validator-specific node information. */
    getNodeInfo(): Promise<NodeInfoResponse>;
    /** `GET /stats` — Aggregate chain statistics. */
    getStats(): Promise<StatsResponse>;
    /**
     * `GET /block/{height}` — Fetch a block by height.
     *
     * Returns the full block detail including header, transaction hashes,
     * and the block hash.
     *
     * @throws {ArcRpcError} 404 if block not found.
     */
    getBlock(height: number): Promise<BlockDetail>;
    /**
     * `GET /blocks` — Paginated block listing.
     *
     * @param options.from - Start height (inclusive, default 0).
     * @param options.to - End height (inclusive, default chain tip).
     * @param options.limit - Max blocks to return (default 20, server caps at 100).
     */
    getBlocks(options?: BlocksQueryOptions): Promise<BlocksResponse>;
    /**
     * `GET /block/{height}/txs` — Paginated transactions for a block.
     *
     * For benchmark blocks, transactions are reconstructed on-demand.
     *
     * @param height - Block height.
     * @param options.offset - Start index within the block (default 0).
     * @param options.limit - Max transactions to return (default 100, server caps at 1000).
     * @throws {ArcRpcError} 404 if block not found.
     */
    getBlockTxs(height: number, options?: BlockTxsQueryOptions): Promise<BlockTxsResponse>;
    /**
     * `GET /block/{height}/proofs` — All Merkle inclusion proofs for a block.
     */
    getBlockProofs(height: number): Promise<BlockProofsResponse>;
    /**
     * `GET /tx/{hash}` — Look up a transaction receipt by hash.
     *
     * Falls back to on-demand reconstruction for benchmark transactions.
     *
     * @param hash - 64-character hex transaction hash.
     * @throws {ArcRpcError} 404 if transaction not found.
     */
    getTx(hash: Hash256): Promise<TxReceipt>;
    /**
     * `GET /tx/{hash}/full` — Full transaction with type-specific body fields,
     * signature information, and receipt data.
     *
     * Supports all 21 transaction types.
     *
     * @param hash - 64-character hex transaction hash.
     * @throws {ArcRpcError} 404 if transaction not found.
     */
    getTxFull(hash: Hash256): Promise<FullTransaction>;
    /**
     * `GET /tx/{hash}/proof` — Merkle inclusion proof for a transaction.
     *
     * The proof can be verified client-side using BLAKE3 with the
     * `ARC-chain-tx-v1` domain separator.
     *
     * @param hash - 64-character hex transaction hash.
     * @throws {ArcRpcError} 404 if transaction not found.
     */
    getTxProof(hash: Hash256): Promise<TxProof>;
    /**
     * `POST /tx/submit` — Submit a transaction to the mempool.
     *
     * @param tx - Transaction payload (from, to, amount, nonce, optional tx_type).
     * @throws {ArcRpcError} 400 if addresses are invalid hex.
     * @throws {ArcRpcError} 409 if transaction already exists (duplicate hash).
     */
    submitTx(tx: TxSubmitPayload): Promise<TxSubmitResponse>;
    /**
     * `POST /tx/submit` — Submit a fully-formed signed transaction.
     *
     * Use this when you have constructed and signed the transaction yourself.
     */
    submitSignedTx(tx: FullTransaction): Promise<TxSubmitResponse>;
    /**
     * `POST /tx/submit_batch` — Submit multiple transactions in one request.
     *
     * Each transaction is processed independently; some may be accepted
     * while others are rejected.
     */
    submitTxBatch(transactions: TxSubmitPayload[]): Promise<TxSubmitBatchResponse>;
    /**
     * `GET /account/{address}` — Fetch account state.
     *
     * @param address - 64-character hex address.
     * @throws {ArcRpcError} 400 if address is not valid hex.
     * @throws {ArcRpcError} 404 if account not found.
     */
    getAccount(address: Address): Promise<Account>;
    /**
     * `GET /account/{address}/txs` — Transaction hashes involving an account.
     *
     * @param address - 64-character hex address.
     */
    getAccountTxs(address: Address): Promise<AccountTxs>;
    /**
     * Convenience: get account balance as a number.
     */
    getBalance(address: Address): Promise<number>;
    /**
     * Convenience: get account nonce.
     */
    getNonce(address: Address): Promise<number>;
    /** `GET /validators` — List all validators with stake and tier. */
    getValidators(): Promise<ValidatorsResponse>;
    /**
     * `GET /contract/{address}` — Get deployed contract information.
     *
     * @param address - 64-character hex contract address.
     * @throws {ArcRpcError} 404 if no contract at address.
     */
    getContract(address: Address): Promise<ContractInfo>;
    /**
     * `POST /contract/{address}/call` — Read-only contract call.
     *
     * Executes the function in a sandbox without modifying state.
     *
     * @param address - Contract address.
     * @param fn - Function name to invoke (e.g. `"get_count"`).
     * @param options.calldata - Hex-encoded calldata.
     * @param options.from - Caller address (optional).
     * @param options.gasLimit - Gas limit for execution.
     */
    callContract(address: Address, fn: string, options?: ContractCallOptions): Promise<ContractCallResult>;
    /** `GET /light/snapshot` — Lightweight snapshot for light client bootstrapping. */
    getLightSnapshot(): Promise<LightSnapshot>;
    /** `GET /sync/snapshot/info` — Metadata about the available state snapshot. */
    getSyncSnapshotInfo(): Promise<SyncSnapshotInfo>;
    /**
     * `GET /sync/snapshot` — Download the full state snapshot as LZ4-compressed bincode.
     *
     * Returns the raw response so you can stream or save the binary data.
     * Use `response.arrayBuffer()` or pipe `response.body` for large snapshots.
     */
    getSyncSnapshot(): Promise<Response>;
    /**
     * `POST /claim` — Request test tokens from the faucet.
     *
     * The faucet is a separate service (default port 3001).
     * If your faucet runs on a different URL, create a second `ArcClient`
     * pointed at the faucet URL.
     *
     * @param address - 64-character hex address to receive tokens.
     */
    faucetClaim(address: Address): Promise<FaucetClaimResponse>;
    /** `GET /status` — Faucet operational status. */
    faucetStatus(): Promise<FaucetStatus>;
    /** `GET /health` — Faucet health check (same path as node health, different service). */
    faucetHealth(): Promise<FaucetHealth>;
    /**
     * Send a raw ETH JSON-RPC 2.0 request.
     *
     * The ARC Chain ETH RPC is available at `/eth` on the main port (9090)
     * or at the root path on the dedicated ETH port (default 8545).
     *
     * @example
     * ```ts
     * const balance = await client.ethRpc<string>("eth_getBalance", [
     *   "0xaf1349b9f5f9a1a6a0404dea36dcc9499bcb25c9",
     *   "latest",
     * ]);
     * ```
     */
    ethRpc<T = unknown>(method: string, params?: unknown[], id?: number | string): Promise<T>;
    /**
     * `eth_chainId` — Returns the ARC Chain ID (`0x415243` = 4,281,923).
     */
    ethChainId(): Promise<string>;
    /**
     * `eth_blockNumber` — Current block height as hex.
     */
    ethBlockNumber(): Promise<string>;
    /**
     * `eth_getBalance` — Account balance in hex wei.
     */
    ethGetBalance(address: string, block?: string): Promise<string>;
    /**
     * `eth_getTransactionCount` — Account nonce as hex.
     */
    ethGetTransactionCount(address: string, block?: string): Promise<string>;
    /**
     * Poll until a transaction is included in a block.
     *
     * @param hash - Transaction hash to watch.
     * @param options.timeout - Max wait time in ms (default 60000).
     * @param options.interval - Poll interval in ms (default 500).
     * @throws {Error} if the timeout is exceeded.
     */
    waitForTx(hash: Hash256, options?: {
        timeout?: number;
        interval?: number;
    }): Promise<TxReceipt>;
    /**
     * Subscribe to new blocks via polling.
     *
     * Calls the provided callback for each new block as it appears.
     * Returns a handle to stop polling.
     *
     * @param callback - Invoked for each new block.
     * @param interval - Poll interval in ms (default 1000).
     */
    onBlock(callback: (block: BlockDetail) => void | Promise<void>, interval?: number): {
        unsubscribe: () => void;
    };
    /** @internal */
    private _get;
    /** @internal */
    private _post;
}
//# sourceMappingURL=client.d.ts.map