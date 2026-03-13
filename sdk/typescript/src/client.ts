// ─── @arc-chain/sdk — RPC Client ──────────────────────────────
// Full-featured client for the ARC Chain native REST API.
// Zero dependencies — uses the built-in Fetch API (Node 18+, all browsers).

import type {
  Address,
  Hash256,
  HealthResponse,
  InfoResponse,
  NodeInfoResponse,
  StatsResponse,
  BlockDetail,
  BlocksResponse,
  BlocksQueryOptions,
  BlockTxsResponse,
  BlockTxsQueryOptions,
  BlockProofsResponse,
  TxReceipt,
  TxProof,
  FullTransaction,
  TxSubmitResponse,
  TxSubmitBatchResponse,
  TxSubmitPayload,
  Account,
  AccountTxs,
  ValidatorsResponse,
  ContractInfo,
  ContractCallResult,
  ContractCallOptions,
  LightSnapshot,
  SyncSnapshotInfo,
  FaucetClaimResponse,
  FaucetStatus,
  FaucetHealth,
  JsonRpcRequest,
  JsonRpcResponse,
} from "./types";

// ─── Error ──────────────────────────────────────────────────

/**
 * Error thrown when an RPC request fails.
 * Carries the HTTP status code and the response body text.
 */
export class ArcRpcError extends Error {
  /** HTTP status code (e.g. 400, 404, 409, 500). */
  readonly statusCode: number;
  /** Raw response body from the node. */
  readonly body: string;

  constructor(statusCode: number, body: string) {
    super(`ARC RPC Error (${statusCode}): ${body}`);
    this.name = "ArcRpcError";
    this.statusCode = statusCode;
    this.body = body;
  }
}

// ─── Client ─────────────────────────────────────────────────

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
export class ArcClient {
  private readonly baseUrl: string;
  private readonly timeout: number;
  private readonly headers: Record<string, string>;

  /**
   * Create a new ARC Chain RPC client.
   *
   * @param rpcUrl - Base URL of the ARC Chain node (e.g. `"http://localhost:9090"`).
   * @param options - Optional configuration.
   * @param options.timeout - Request timeout in milliseconds (default: 30000).
   * @param options.headers - Additional headers to include on every request.
   */
  constructor(
    rpcUrl: string,
    options?: {
      timeout?: number;
      headers?: Record<string, string>;
    },
  ) {
    this.baseUrl = rpcUrl.replace(/\/+$/, "");
    this.timeout = options?.timeout ?? 30_000;
    this.headers = {
      "Content-Type": "application/json",
      ...options?.headers,
    };
  }

  // ─── Health & Info ──────────────────────────────────────

  /** `GET /health` — Node health check. */
  async getHealth(): Promise<HealthResponse> {
    return this._get<HealthResponse>("/health");
  }

  /** `GET /info` — Chain information including GPU status. */
  async getInfo(): Promise<InfoResponse> {
    return this._get<InfoResponse>("/info");
  }

  /** `GET /node/info` — Validator-specific node information. */
  async getNodeInfo(): Promise<NodeInfoResponse> {
    return this._get<NodeInfoResponse>("/node/info");
  }

  /** `GET /stats` — Aggregate chain statistics. */
  async getStats(): Promise<StatsResponse> {
    return this._get<StatsResponse>("/stats");
  }

  // ─── Blocks ─────────────────────────────────────────────

  /**
   * `GET /block/{height}` — Fetch a block by height.
   *
   * Returns the full block detail including header, transaction hashes,
   * and the block hash.
   *
   * @throws {ArcRpcError} 404 if block not found.
   */
  async getBlock(height: number): Promise<BlockDetail> {
    return this._get<BlockDetail>(`/block/${height}`);
  }

  /**
   * `GET /blocks` — Paginated block listing.
   *
   * @param options.from - Start height (inclusive, default 0).
   * @param options.to - End height (inclusive, default chain tip).
   * @param options.limit - Max blocks to return (default 20, server caps at 100).
   */
  async getBlocks(options?: BlocksQueryOptions): Promise<BlocksResponse> {
    const params = new URLSearchParams();
    if (options?.from !== undefined) params.set("from", String(options.from));
    if (options?.to !== undefined) params.set("to", String(options.to));
    if (options?.limit !== undefined) params.set("limit", String(options.limit));

    const qs = params.toString();
    return this._get<BlocksResponse>(`/blocks${qs ? `?${qs}` : ""}`);
  }

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
  async getBlockTxs(
    height: number,
    options?: BlockTxsQueryOptions,
  ): Promise<BlockTxsResponse> {
    const params = new URLSearchParams();
    if (options?.offset !== undefined) params.set("offset", String(options.offset));
    if (options?.limit !== undefined) params.set("limit", String(options.limit));

    const qs = params.toString();
    return this._get<BlockTxsResponse>(`/block/${height}/txs${qs ? `?${qs}` : ""}`);
  }

  /**
   * `GET /block/{height}/proofs` — All Merkle inclusion proofs for a block.
   */
  async getBlockProofs(height: number): Promise<BlockProofsResponse> {
    return this._get<BlockProofsResponse>(`/block/${height}/proofs`);
  }

  // ─── Transactions ───────────────────────────────────────

  /**
   * `GET /tx/{hash}` — Look up a transaction receipt by hash.
   *
   * Falls back to on-demand reconstruction for benchmark transactions.
   *
   * @param hash - 64-character hex transaction hash.
   * @throws {ArcRpcError} 404 if transaction not found.
   */
  async getTx(hash: Hash256): Promise<TxReceipt> {
    return this._get<TxReceipt>(`/tx/${hash}`);
  }

  /**
   * `GET /tx/{hash}/full` — Full transaction with type-specific body fields,
   * signature information, and receipt data.
   *
   * Supports all 21 transaction types.
   *
   * @param hash - 64-character hex transaction hash.
   * @throws {ArcRpcError} 404 if transaction not found.
   */
  async getTxFull(hash: Hash256): Promise<FullTransaction> {
    return this._get<FullTransaction>(`/tx/${hash}/full`);
  }

  /**
   * `GET /tx/{hash}/proof` — Merkle inclusion proof for a transaction.
   *
   * The proof can be verified client-side using BLAKE3 with the
   * `ARC-chain-tx-v1` domain separator.
   *
   * @param hash - 64-character hex transaction hash.
   * @throws {ArcRpcError} 404 if transaction not found.
   */
  async getTxProof(hash: Hash256): Promise<TxProof> {
    return this._get<TxProof>(`/tx/${hash}/proof`);
  }

  /**
   * `POST /tx/submit` — Submit a transaction to the mempool.
   *
   * @param tx - Transaction payload (from, to, amount, nonce, optional tx_type).
   * @throws {ArcRpcError} 400 if addresses are invalid hex.
   * @throws {ArcRpcError} 409 if transaction already exists (duplicate hash).
   */
  async submitTx(tx: TxSubmitPayload): Promise<TxSubmitResponse> {
    return this._post<TxSubmitResponse>("/tx/submit", tx);
  }

  /**
   * `POST /tx/submit` — Submit a fully-formed signed transaction.
   *
   * Use this when you have constructed and signed the transaction yourself.
   */
  async submitSignedTx(tx: FullTransaction): Promise<TxSubmitResponse> {
    return this._post<TxSubmitResponse>("/tx/submit", tx);
  }

  /**
   * `POST /tx/submit_batch` — Submit multiple transactions in one request.
   *
   * Each transaction is processed independently; some may be accepted
   * while others are rejected.
   */
  async submitTxBatch(
    transactions: TxSubmitPayload[],
  ): Promise<TxSubmitBatchResponse> {
    return this._post<TxSubmitBatchResponse>("/tx/submit_batch", {
      transactions,
    });
  }

  // ─── Accounts ───────────────────────────────────────────

  /**
   * `GET /account/{address}` — Fetch account state.
   *
   * @param address - 64-character hex address.
   * @throws {ArcRpcError} 400 if address is not valid hex.
   * @throws {ArcRpcError} 404 if account not found.
   */
  async getAccount(address: Address): Promise<Account> {
    return this._get<Account>(`/account/${address}`);
  }

  /**
   * `GET /account/{address}/txs` — Transaction hashes involving an account.
   *
   * @param address - 64-character hex address.
   */
  async getAccountTxs(address: Address): Promise<AccountTxs> {
    return this._get<AccountTxs>(`/account/${address}/txs`);
  }

  /**
   * Convenience: get account balance as a number.
   */
  async getBalance(address: Address): Promise<number> {
    const account = await this.getAccount(address);
    return account.balance;
  }

  /**
   * Convenience: get account nonce.
   */
  async getNonce(address: Address): Promise<number> {
    const account = await this.getAccount(address);
    return account.nonce;
  }

  // ─── Validators ─────────────────────────────────────────

  /** `GET /validators` — List all validators with stake and tier. */
  async getValidators(): Promise<ValidatorsResponse> {
    return this._get<ValidatorsResponse>("/validators");
  }

  // ─── Contracts ──────────────────────────────────────────

  /**
   * `GET /contract/{address}` — Get deployed contract information.
   *
   * @param address - 64-character hex contract address.
   * @throws {ArcRpcError} 404 if no contract at address.
   */
  async getContract(address: Address): Promise<ContractInfo> {
    return this._get<ContractInfo>(`/contract/${address}`);
  }

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
  async callContract(
    address: Address,
    fn: string,
    options?: ContractCallOptions,
  ): Promise<ContractCallResult> {
    return this._post<ContractCallResult>(`/contract/${address}/call`, {
      function: fn,
      calldata: options?.calldata,
      from: options?.from,
      gas_limit: options?.gasLimit,
    });
  }

  // ─── Light Client & Sync ────────────────────────────────

  /** `GET /light/snapshot` — Lightweight snapshot for light client bootstrapping. */
  async getLightSnapshot(): Promise<LightSnapshot> {
    return this._get<LightSnapshot>("/light/snapshot");
  }

  /** `GET /sync/snapshot/info` — Metadata about the available state snapshot. */
  async getSyncSnapshotInfo(): Promise<SyncSnapshotInfo> {
    return this._get<SyncSnapshotInfo>("/sync/snapshot/info");
  }

  /**
   * `GET /sync/snapshot` — Download the full state snapshot as LZ4-compressed bincode.
   *
   * Returns the raw response so you can stream or save the binary data.
   * Use `response.arrayBuffer()` or pipe `response.body` for large snapshots.
   */
  async getSyncSnapshot(): Promise<Response> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout * 10); // longer timeout for snapshots

    try {
      const res = await fetch(`${this.baseUrl}/sync/snapshot`, {
        method: "GET",
        signal: controller.signal,
      });

      if (!res.ok) {
        throw new ArcRpcError(res.status, await res.text());
      }

      return res;
    } finally {
      clearTimeout(timer);
    }
  }

  // ─── Faucet ─────────────────────────────────────────────

  /**
   * `POST /claim` — Request test tokens from the faucet.
   *
   * The faucet is a separate service (default port 3001).
   * If your faucet runs on a different URL, create a second `ArcClient`
   * pointed at the faucet URL.
   *
   * @param address - 64-character hex address to receive tokens.
   */
  async faucetClaim(address: Address): Promise<FaucetClaimResponse> {
    return this._post<FaucetClaimResponse>("/claim", { address });
  }

  /** `GET /status` — Faucet operational status. */
  async faucetStatus(): Promise<FaucetStatus> {
    return this._get<FaucetStatus>("/status");
  }

  /** `GET /health` — Faucet health check (same path as node health, different service). */
  async faucetHealth(): Promise<FaucetHealth> {
    return this._get<FaucetHealth>("/health");
  }

  // ─── ETH JSON-RPC ───────────────────────────────────────

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
  async ethRpc<T = unknown>(
    method: string,
    params: unknown[] = [],
    id: number | string = 1,
  ): Promise<T> {
    const request: JsonRpcRequest = {
      jsonrpc: "2.0",
      method,
      params,
      id,
    };

    const response = await this._post<JsonRpcResponse<T>>("/eth", request);

    if (response.error) {
      throw new ArcRpcError(
        response.error.code,
        response.error.message,
      );
    }

    return response.result as T;
  }

  /**
   * `eth_chainId` — Returns the ARC Chain ID (`0x415243` = 4,281,923).
   */
  async ethChainId(): Promise<string> {
    return this.ethRpc<string>("eth_chainId");
  }

  /**
   * `eth_blockNumber` — Current block height as hex.
   */
  async ethBlockNumber(): Promise<string> {
    return this.ethRpc<string>("eth_blockNumber");
  }

  /**
   * `eth_getBalance` — Account balance in hex wei.
   */
  async ethGetBalance(address: string, block: string = "latest"): Promise<string> {
    return this.ethRpc<string>("eth_getBalance", [address, block]);
  }

  /**
   * `eth_getTransactionCount` — Account nonce as hex.
   */
  async ethGetTransactionCount(
    address: string,
    block: string = "latest",
  ): Promise<string> {
    return this.ethRpc<string>("eth_getTransactionCount", [address, block]);
  }

  // ─── Polling Utilities ──────────────────────────────────

  /**
   * Poll until a transaction is included in a block.
   *
   * @param hash - Transaction hash to watch.
   * @param options.timeout - Max wait time in ms (default 60000).
   * @param options.interval - Poll interval in ms (default 500).
   * @throws {Error} if the timeout is exceeded.
   */
  async waitForTx(
    hash: Hash256,
    options?: { timeout?: number; interval?: number },
  ): Promise<TxReceipt> {
    const timeout = options?.timeout ?? 60_000;
    const interval = options?.interval ?? 500;
    const deadline = Date.now() + timeout;

    while (Date.now() < deadline) {
      try {
        return await this.getTx(hash);
      } catch (err) {
        if (err instanceof ArcRpcError && err.statusCode === 404) {
          await sleep(interval);
          continue;
        }
        throw err;
      }
    }

    throw new Error(
      `Transaction ${hash} not confirmed within ${timeout}ms`,
    );
  }

  /**
   * Subscribe to new blocks via polling.
   *
   * Calls the provided callback for each new block as it appears.
   * Returns a handle to stop polling.
   *
   * @param callback - Invoked for each new block.
   * @param interval - Poll interval in ms (default 1000).
   */
  onBlock(
    callback: (block: BlockDetail) => void | Promise<void>,
    interval: number = 1000,
  ): { unsubscribe: () => void } {
    let active = true;
    let lastHeight = -1;

    const poll = async (): Promise<void> => {
      while (active) {
        try {
          const info = await this.getInfo();
          const tip = info.block_height;

          if (lastHeight === -1) {
            lastHeight = tip - 1;
          }

          for (let h = lastHeight + 1; h <= tip && active; h++) {
            const block = await this.getBlock(h);
            await callback(block);
            lastHeight = h;
          }
        } catch {
          // Swallow errors and retry on next poll
        }
        if (active) {
          await sleep(interval);
        }
      }
    };

    poll();

    return {
      unsubscribe: () => {
        active = false;
      },
    };
  }

  // ─── Internal HTTP ──────────────────────────────────────

  /** @internal */
  private async _get<T>(path: string): Promise<T> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout);

    try {
      const res = await fetch(`${this.baseUrl}${path}`, {
        method: "GET",
        headers: this.headers,
        signal: controller.signal,
      });

      if (!res.ok) {
        throw new ArcRpcError(res.status, await res.text());
      }

      return (await res.json()) as T;
    } finally {
      clearTimeout(timer);
    }
  }

  /** @internal */
  private async _post<T>(path: string, body: unknown): Promise<T> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout);

    try {
      const res = await fetch(`${this.baseUrl}${path}`, {
        method: "POST",
        headers: this.headers,
        body: JSON.stringify(body),
        signal: controller.signal,
      });

      if (!res.ok) {
        throw new ArcRpcError(res.status, await res.text());
      }

      return (await res.json()) as T;
    } finally {
      clearTimeout(timer);
    }
  }
}

// ─── Helpers ────────────────────────────────────────────────

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
