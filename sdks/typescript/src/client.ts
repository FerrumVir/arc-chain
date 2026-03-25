/**
 * ARC Chain SDK — RPC client.
 *
 * Typed HTTP client for all ARC Chain RPC endpoints.
 * Uses the native `fetch` API (Node 18+, Deno, Bun, browsers).
 */

import type {
  Account,
  BatchResult,
  Block,
  BlockSummary,
  ChainInfo,
  ChainStats,
  ContractCallResult,
  ContractInfo,
  EthRpcResponse,
  HealthInfo,
  LightSnapshot,
  NodeInfo,
  Receipt,
  SubmitResult,
  SyncSnapshotInfo,
  Transaction,
} from "./types";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

export class ArcError extends Error {
  statusCode?: number;
  detail?: string;

  constructor(message: string, statusCode?: number, detail?: string) {
    super(message);
    this.name = "ArcError";
    this.statusCode = statusCode;
    this.detail = detail;
  }
}

export class ArcConnectionError extends ArcError {
  url?: string;
  cause?: Error;

  constructor(message: string, url?: string, cause?: Error) {
    super(message);
    this.name = "ArcConnectionError";
    this.url = url;
    this.cause = cause;
  }
}

export class ArcTransactionError extends ArcError {
  txHash?: string;

  constructor(message: string, txHash?: string, statusCode?: number) {
    super(message, statusCode);
    this.name = "ArcTransactionError";
    this.txHash = txHash;
  }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

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
 * const client = new ArcClient("http://localhost:9090");
 * const info = await client.getChainInfo();
 * console.log(info.block_height);
 * ```
 */
export class ArcClient {
  readonly rpcUrl: string;
  private timeout: number;
  private headers: Record<string, string>;

  constructor(rpcUrl: string, options: ArcClientOptions = {}) {
    this.rpcUrl = rpcUrl.replace(/\/+$/, "");
    this.timeout = options.timeout ?? 30_000;
    this.headers = {
      "Content-Type": "application/json",
      ...options.headers,
    };
  }

  // -- Internal helpers --

  private async _get<T = unknown>(path: string, params?: Record<string, string | number>): Promise<T> {
    let url = `${this.rpcUrl}${path}`;
    if (params) {
      const qs = new URLSearchParams();
      for (const [k, v] of Object.entries(params)) {
        if (v !== undefined && v !== null) qs.set(k, String(v));
      }
      const qsStr = qs.toString();
      if (qsStr) url += `?${qsStr}`;
    }

    let resp: Response;
    try {
      resp = await fetch(url, {
        method: "GET",
        headers: this.headers,
        signal: AbortSignal.timeout(this.timeout),
      });
    } catch (e) {
      throw new ArcConnectionError(
        `Failed to connect to ${url}`,
        url,
        e instanceof Error ? e : undefined
      );
    }

    if (resp.status === 404) {
      throw new ArcError(`Not found: ${path}`, 404);
    }
    if (resp.status === 400) {
      throw new ArcError(`Bad request: ${path}`, 400);
    }
    if (resp.status >= 400) {
      const text = await resp.text().catch(() => "");
      throw new ArcError(
        `RPC error ${resp.status}: ${path}`,
        resp.status,
        text.slice(0, 500)
      );
    }

    return resp.json() as Promise<T>;
  }

  private async _post<T = unknown>(path: string, body: unknown): Promise<T> {
    const url = `${this.rpcUrl}${path}`;

    let resp: Response;
    try {
      resp = await fetch(url, {
        method: "POST",
        headers: this.headers,
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(this.timeout),
      });
    } catch (e) {
      throw new ArcConnectionError(
        `Failed to connect to ${url}`,
        url,
        e instanceof Error ? e : undefined
      );
    }

    if (resp.status === 409) {
      throw new ArcTransactionError(
        "Transaction already exists (duplicate/conflict)",
        undefined,
        409
      );
    }
    if (resp.status >= 400) {
      const text = await resp.text().catch(() => "");
      throw new ArcError(
        `RPC error ${resp.status}: ${path}`,
        resp.status,
        text.slice(0, 500)
      );
    }

    return resp.json() as Promise<T>;
  }

  // -- Block endpoints --

  /**
   * GET /block/{height} -- Fetch a block by height.
   */
  async getBlock(height: number): Promise<Block> {
    return this._get<Block>(`/block/${height}`);
  }

  /**
   * GET /blocks -- Paginated block listing.
   */
  async getBlocks(
    fromHeight: number = 0,
    toHeight?: number,
    limit: number = 20
  ): Promise<{ from: number; to: number; count: number; blocks: BlockSummary[] }> {
    const params: Record<string, string | number> = { from: fromHeight, limit };
    if (toHeight !== undefined) params.to = toHeight;
    return this._get(`/blocks`, params);
  }

  /**
   * GET /block/{height}/txs -- Paginated transaction listing for a block.
   */
  async getBlockTxs(
    height: number,
    offset: number = 0,
    limit: number = 100
  ): Promise<Record<string, unknown>> {
    return this._get(`/block/${height}/txs`, { offset, limit });
  }

  /**
   * GET /block/{height}/proofs -- All Merkle proofs for transactions in a block.
   */
  async getBlockProofs(height: number): Promise<Record<string, unknown>> {
    return this._get(`/block/${height}/proofs`);
  }

  // -- Account endpoints --

  /**
   * GET /account/{address} -- Fetch an account by address.
   */
  async getAccount(address: string): Promise<Account> {
    return this._get<Account>(`/account/${address}`);
  }

  /**
   * GET /account/{address}/txs -- Transaction hashes involving an account.
   */
  async getAccountTxs(
    address: string
  ): Promise<{ address: string; tx_count: number; tx_hashes: string[] }> {
    return this._get(`/account/${address}/txs`);
  }

  // -- Transaction endpoints --

  /**
   * POST /tx/submit -- Submit a transaction to the mempool.
   *
   * Accepts either a raw RPC-format object or a signed Transaction from
   * TransactionBuilder.sign().
   *
   * @returns Transaction hash string.
   */
  async submitTransaction(tx: Record<string, unknown> | Transaction): Promise<string> {
    let payload: Record<string, unknown>;

    // Normalize TransactionBuilder-style tx
    if ("body" in tx && "tx_type" in tx) {
      const body = tx.body as Record<string, unknown>;
      payload = {
        from: tx.from,
        to: (body.to as string) ?? "0".repeat(64),
        amount: (body.amount as number) ?? 0,
        nonce: tx.nonce ?? 0,
        tx_type: tx.tx_type,
      };
    } else {
      payload = tx as Record<string, unknown>;
    }

    const data = await this._post<SubmitResult>("/tx/submit", payload);
    return data.tx_hash;
  }

  /**
   * GET /tx/{hash} -- Look up a transaction receipt by hash.
   */
  async getTransaction(txHash: string): Promise<Receipt> {
    return this._get<Receipt>(`/tx/${txHash}`);
  }

  /**
   * GET /tx/{hash}/full -- Full transaction body with type-specific fields.
   */
  async getFullTransaction(txHash: string): Promise<Record<string, unknown>> {
    return this._get(`/tx/${txHash}/full`);
  }

  /**
   * GET /tx/{hash}/proof -- Merkle inclusion proof for a transaction.
   */
  async getTxProof(txHash: string): Promise<Record<string, unknown>> {
    return this._get(`/tx/${txHash}/proof`);
  }

  /**
   * POST /tx/submit_batch -- Submit multiple transactions.
   */
  async submitBatch(txs: Array<Record<string, unknown>>): Promise<BatchResult> {
    const normalized = txs.map((tx) => {
      if ("body" in tx && "tx_type" in tx) {
        const body = tx.body as Record<string, unknown>;
        return {
          from: tx.from,
          to: (body.to as string) ?? "0".repeat(64),
          amount: (body.amount as number) ?? 0,
          nonce: tx.nonce ?? 0,
        };
      }
      return tx;
    });

    return this._post<BatchResult>("/tx/submit_batch", {
      transactions: normalized,
    });
  }

  // -- Chain info & stats --

  /**
   * GET /info -- Chain information.
   */
  async getChainInfo(): Promise<ChainInfo> {
    return this._get<ChainInfo>("/info");
  }

  /**
   * GET /stats -- Chain statistics.
   */
  async getStats(): Promise<ChainStats> {
    return this._get<ChainStats>("/stats");
  }

  /**
   * GET /health -- Node health status.
   */
  async getHealth(): Promise<HealthInfo> {
    return this._get<HealthInfo>("/health");
  }

  /**
   * GET /node/info -- Validator node information.
   */
  async getNodeInfo(): Promise<NodeInfo> {
    return this._get<NodeInfo>("/node/info");
  }

  // -- Contract endpoints --

  /**
   * GET /contract/{address} -- Contract bytecode info.
   */
  async getContractInfo(address: string): Promise<ContractInfo> {
    return this._get<ContractInfo>(`/contract/${address}`);
  }

  /**
   * POST /contract/{address}/call -- Read-only contract call.
   */
  async callContract(
    address: string,
    func: string,
    calldata?: string,
    fromAddr?: string,
    gasLimit: number = 1_000_000
  ): Promise<ContractCallResult> {
    const body: Record<string, unknown> = {
      function: func,
      gas_limit: gasLimit,
    };
    if (calldata) body.calldata = calldata;
    if (fromAddr) body.from = fromAddr;
    return this._post<ContractCallResult>(`/contract/${address}/call`, body);
  }

  // -- Light client & sync --

  /**
   * GET /light/snapshot -- Lightweight snapshot for light client bootstrapping.
   */
  async getLightSnapshot(): Promise<LightSnapshot> {
    return this._get<LightSnapshot>("/light/snapshot");
  }

  /**
   * GET /sync/snapshot/info -- Metadata about available state sync snapshot.
   */
  async getSyncSnapshotInfo(): Promise<SyncSnapshotInfo> {
    return this._get<SyncSnapshotInfo>("/sync/snapshot/info");
  }

  // -- ETH JSON-RPC --

  /**
   * POST /eth -- Send an ETH-compatible JSON-RPC request.
   *
   * Supports methods like eth_chainId, eth_blockNumber,
   * eth_getBalance, eth_call, eth_estimateGas, etc.
   */
  async ethCall(method: string, params: unknown[] = []): Promise<EthRpcResponse> {
    return this._post<EthRpcResponse>("/eth", {
      jsonrpc: "2.0",
      method,
      params,
      id: 1,
    });
  }
}
