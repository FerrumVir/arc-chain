import type { Address, Hash256, HealthResponse, InfoResponse, NodeInfoResponse, StatsResponse, BlockDetail, BlocksResponse, BlocksQueryOptions, BlockTxsResponse, BlockTxsQueryOptions, BlockProofsResponse, TxReceipt, TxProof, FullTransaction, SignedTransferTransaction, TxSubmitResponse, TxSubmitBatchResponse, TxSubmitPayload, TransactionDomainInfo, Account, AccountTxs, ValidatorsResponse, ContractInfo, ContractCallResult, ContractCallOptions, LightSnapshot, SyncSnapshotInfo, FaucetClaimResponse, FaucetStatus, FaucetHealth, U64 } from "./types.js";
export declare class ArcRpcError extends Error {
    readonly statusCode: number;
    readonly body: string;
    constructor(statusCode: number, body: string);
}
export declare class ArcClient {
    private readonly baseUrl;
    private readonly timeout;
    private readonly headers;
    constructor(rpcUrl: string, options?: {
        timeout?: number;
        headers?: Record<string, string>;
    });
    getHealth(): Promise<HealthResponse>;
    getInfo(): Promise<InfoResponse>;
    getNodeInfo(): Promise<NodeInfoResponse>;
    getStats(): Promise<StatsResponse>;
    getBlock(height: number): Promise<BlockDetail>;
    getBlocks(options?: BlocksQueryOptions): Promise<BlocksResponse>;
    getBlockTxs(height: number, options?: BlockTxsQueryOptions): Promise<BlockTxsResponse>;
    getBlockProofs(height: number): Promise<BlockProofsResponse>;
    getTx(hash: Hash256): Promise<TxReceipt>;
    getTxFull(hash: Hash256): Promise<FullTransaction>;
    getTxProof(hash: Hash256): Promise<TxProof>;
    submitTx(tx: TxSubmitPayload): Promise<TxSubmitResponse>;
    submitSignedTx(tx: SignedTransferTransaction): Promise<TxSubmitResponse>;
    submitTxBatch(transactions: TxSubmitPayload[]): Promise<TxSubmitBatchResponse>;
    getTransactionDomain(): Promise<TransactionDomainInfo["transaction_domain"]>;
    private _assertTransactionDomain;
    private _requireMatchingTransactionDomain;
    getAccount(address: Address): Promise<Account>;
    getAccountTxs(address: Address): Promise<AccountTxs>;
    getBalance(address: Address): Promise<U64>;
    getNonce(address: Address): Promise<U64>;
    getValidators(): Promise<ValidatorsResponse>;
    getContract(address: Address): Promise<ContractInfo>;
    callContract(address: Address, fn: string, options?: ContractCallOptions): Promise<ContractCallResult>;
    getLightSnapshot(): Promise<LightSnapshot>;
    getSyncSnapshotInfo(): Promise<SyncSnapshotInfo>;
    getSyncSnapshot(): Promise<Response>;
    faucetClaim(address: Address): Promise<FaucetClaimResponse>;
    faucetStatus(): Promise<FaucetStatus>;
    faucetHealth(): Promise<FaucetHealth>;
    ethRpc<T = unknown>(method: string, params?: unknown[], id?: number | string): Promise<T>;
    ethChainId(): Promise<string>;
    ethBlockNumber(): Promise<string>;
    ethGetBalance(address: string, block?: string): Promise<string>;
    ethGetTransactionCount(address: string, block?: string): Promise<string>;
    waitForTx(hash: Hash256, options?: {
        timeout?: number;
        interval?: number;
    }): Promise<TxReceipt>;
    onBlock(callback: (block: BlockDetail) => void | Promise<void>, interval?: number): {
        unsubscribe: () => void;
    };
    private _get;
    private _post;
}
//# sourceMappingURL=client.d.ts.map