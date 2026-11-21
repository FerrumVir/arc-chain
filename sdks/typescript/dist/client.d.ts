import type { Account, BatchResult, Block, BlockSummary, ChainInfo, ChainStats, ContractCallResult, ContractInfo, EthRpcResponse, HealthInfo, LightSnapshot, NodeInfo, Receipt, SyncSnapshotInfo, SignedTransferTransaction, SignedTransferSubmitPayload, U64 } from "./types";
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
    timeout?: number;
    headers?: Record<string, string>;
}
export declare class ArcClient {
    readonly rpcUrl: string;
    private timeout;
    private headers;
    constructor(rpcUrl: string, options?: ArcClientOptions);
    private _get;
    private _post;
    getBlock(height: number): Promise<Block>;
    getBlocks(fromHeight?: number, toHeight?: number, limit?: number): Promise<{
        from: number;
        to: number;
        count: number;
        blocks: BlockSummary[];
    }>;
    getBlockTxs(height: number, offset?: number, limit?: number): Promise<Record<string, unknown>>;
    getBlockProofs(height: number): Promise<Record<string, unknown>>;
    getAccount(address: string): Promise<Account>;
    getAccountTxs(address: string): Promise<{
        address: string;
        tx_count: number;
        tx_hashes: string[];
    }>;
    submitTransaction(tx: SignedTransferSubmitPayload | SignedTransferTransaction): Promise<string>;
    getTransaction(txHash: string): Promise<Receipt>;
    getFullTransaction(txHash: string): Promise<Record<string, unknown>>;
    getTxProof(txHash: string): Promise<Record<string, unknown>>;
    submitBatch(txs: Array<SignedTransferSubmitPayload | SignedTransferTransaction>): Promise<BatchResult>;
    getTransactionDomain(): Promise<string | null>;
    private _normalizeSignedTransfer;
    private _assertSignedTransferU64Fields;
    private _assertTransactionDomain;
    private _requireMatchingTransactionDomain;
    getChainInfo(): Promise<ChainInfo>;
    getStats(): Promise<ChainStats>;
    getHealth(): Promise<HealthInfo>;
    getNodeInfo(): Promise<NodeInfo>;
    getContractInfo(address: string): Promise<ContractInfo>;
    callContract(address: string, func: string, calldata?: string, fromAddr?: string, gasLimit?: U64): Promise<ContractCallResult>;
    getLightSnapshot(): Promise<LightSnapshot>;
    getSyncSnapshotInfo(): Promise<SyncSnapshotInfo>;
    ethCall(method: string, params?: unknown[]): Promise<EthRpcResponse>;
}
//# sourceMappingURL=client.d.ts.map