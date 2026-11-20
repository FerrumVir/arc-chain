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
  SignedTransferTransaction,
  TxSubmitResponse,
  TxSubmitBatchResponse,
  TxSubmitPayload,
  TransactionDomainInfo,
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
  U64,
} from "./types.js";
import {
  assertU64,
  parseJsonWithBigInts,
  stringifyJsonWithBigInts,
  u64ToBigInt,
} from "./u64.js";

const MAX_TX_SUBMIT_BATCH_SIZE = 64;

export class ArcRpcError extends Error {
  readonly statusCode: number;
  readonly body: string;

  constructor(statusCode: number, body: string) {
    super(`ARC RPC Error (${statusCode}): ${body}`);
    this.name = "ArcRpcError";
    this.statusCode = statusCode;
    this.body = body;
  }
}

export class ArcClient {
  private readonly baseUrl: string;
  private readonly timeout: number;
  private readonly headers: Record<string, string>;

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

  async getHealth(): Promise<HealthResponse> {
    return this._get<HealthResponse>("/health");
  }

  async getInfo(): Promise<InfoResponse> {
    return this._get<InfoResponse>("/info");
  }

  async getNodeInfo(): Promise<NodeInfoResponse> {
    return this._get<NodeInfoResponse>("/node/info");
  }

  async getStats(): Promise<StatsResponse> {
    return this._get<StatsResponse>("/stats");
  }

  async getBlock(height: number): Promise<BlockDetail> {
    return this._get<BlockDetail>(`/block/${height}`);
  }

  async getBlocks(options?: BlocksQueryOptions): Promise<BlocksResponse> {
    const params = new URLSearchParams();
    if (options?.from !== undefined) params.set("from", String(options.from));
    if (options?.to !== undefined) params.set("to", String(options.to));
    if (options?.limit !== undefined) params.set("limit", String(options.limit));

    const qs = params.toString();
    return this._get<BlocksResponse>(`/blocks${qs ? `?${qs}` : ""}`);
  }

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

  async getBlockProofs(height: number): Promise<BlockProofsResponse> {
    return this._get<BlockProofsResponse>(`/block/${height}/proofs`);
  }

  async getTx(hash: Hash256): Promise<TxReceipt> {
    return this._get<TxReceipt>(`/tx/${hash}`);
  }

  async getTxFull(hash: Hash256): Promise<FullTransaction> {
    return this._get<FullTransaction>(`/tx/${hash}/full`);
  }

  async getTxProof(hash: Hash256): Promise<TxProof> {
    return this._get<TxProof>(`/tx/${hash}/proof`);
  }

  async submitTx(tx: TxSubmitPayload): Promise<TxSubmitResponse> {
    assertSignedTransferU64Fields(tx);
    await this._assertTransactionDomain(tx.transaction_domain);
    const { transaction_domain: _domain, ...wire } = tx;
    return this._post<TxSubmitResponse>("/tx/submit", wire);
  }

  async submitSignedTx(
    tx: SignedTransferTransaction,
  ): Promise<TxSubmitResponse> {
    assertU64(tx.gas_limit, "gas_limit");
    if (u64ToBigInt(tx.gas_limit, "gas_limit") !== 0n) {
      throw new RangeError("gas_limit must be zero for the flat transfer RPC");
    }
    const signature = tx.signature.Ed25519;
    return this.submitTx({
      from: tx.from,
      to: tx.body.to,
      amount: tx.body.amount,
      nonce: tx.nonce,
      fee: tx.fee,
      tx_type: "Transfer",
      signature: signature.signature,
      public_key: signature.public_key,
      transaction_domain: tx.transaction_domain,
    });
  }

  async submitTxBatch(
    transactions: TxSubmitPayload[],
  ): Promise<TxSubmitBatchResponse> {
    if (transactions.length > MAX_TX_SUBMIT_BATCH_SIZE) {
      throw new RangeError(
        `transaction batch exceeds the maximum of ${MAX_TX_SUBMIT_BATCH_SIZE} items`,
      );
    }
    for (const tx of transactions) assertSignedTransferU64Fields(tx);
    const domain = await this.getTransactionDomain();
    for (const tx of transactions) {
      this._requireMatchingTransactionDomain(tx.transaction_domain, domain);
    }
    return this._post<TxSubmitBatchResponse>("/tx/submit_batch", {
      transactions: transactions.map(
        ({ transaction_domain: _domain, ...wire }) => wire,
      ),
    });
  }

  async getTransactionDomain(): Promise<TransactionDomainInfo["transaction_domain"]> {
    let value: Record<string, unknown>;
    try {
      value = await this._get<Record<string, unknown>>("/network/info");
    } catch (error) {
      if (error instanceof ArcRpcError && error.statusCode === 404) return null;
      throw error;
    }

    const raw = value.transaction_domain;
    const recoveryActive = value.recovery_active === true;
    const protocolMajor = Number.parseInt(
      String(value.protocol_version ?? "0").split(".")[0] ?? "0",
      10,
    );
    if (raw == null) {
      if (recoveryActive || protocolMajor >= 3) {
        throw new Error(
          "node requires recovery-domain signatures but omitted transaction_domain",
        );
      }
      return null;
    }
    if (typeof raw !== "string" || !/^0x[0-9a-fA-F]{64}$/.test(raw)) {
      throw new Error("node returned a malformed 32-byte transaction_domain");
    }
    if (/^0x0{64}$/i.test(raw)) {
      throw new Error("node returned an all-zero transaction_domain");
    }
    return raw.toLowerCase() as TransactionDomainInfo["transaction_domain"];
  }

  private async _assertTransactionDomain(
    signedDomain: TransactionDomainInfo["transaction_domain"],
  ): Promise<void> {
    const advertised = await this.getTransactionDomain();
    this._requireMatchingTransactionDomain(signedDomain, advertised);
  }

  private _requireMatchingTransactionDomain(
    signedDomain: TransactionDomainInfo["transaction_domain"],
    advertised: TransactionDomainInfo["transaction_domain"],
  ): void {
    let normalized: TransactionDomainInfo["transaction_domain"] = null;
    if (signedDomain !== null) {
      if (!/^0x[0-9a-fA-F]{64}$/.test(signedDomain) || /^0x0{64}$/i.test(signedDomain)) {
        throw new Error(
          "transaction signature domain must be a non-zero 32-byte 0x-prefixed hex value",
        );
      }
      normalized = signedDomain.toLowerCase() as TransactionDomainInfo["transaction_domain"];
    }
    if (normalized !== advertised) {
      throw new Error(
        `transaction signature domain mismatch: signed for ${normalized ?? "legacy-v1"}, ` +
          `node requires ${advertised ?? "legacy-v1"}`,
      );
    }
  }

  async getAccount(address: Address): Promise<Account> {
    return this._get<Account>(`/account/${address}`);
  }

  async getAccountTxs(address: Address): Promise<AccountTxs> {
    return this._get<AccountTxs>(`/account/${address}/txs`);
  }

  async getBalance(address: Address): Promise<U64> {
    const account = await this.getAccount(address);
    return account.balance;
  }

  async getNonce(address: Address): Promise<U64> {
    const account = await this.getAccount(address);
    return account.nonce;
  }

  async getValidators(): Promise<ValidatorsResponse> {
    return this._get<ValidatorsResponse>("/validators");
  }

  async getContract(address: Address): Promise<ContractInfo> {
    return this._get<ContractInfo>(`/contract/${address}`);
  }

  async callContract(
    address: Address,
    fn: string,
    options?: ContractCallOptions,
  ): Promise<ContractCallResult> {
    if (options?.gasLimit !== undefined) {
      assertU64(options.gasLimit, "gas_limit");
    }
    return this._post<ContractCallResult>(`/contract/${address}/call`, {
      function: fn,
      calldata: options?.calldata,
      from: options?.from,
      gas_limit: options?.gasLimit,
    });
  }

  async getLightSnapshot(): Promise<LightSnapshot> {
    return this._get<LightSnapshot>("/light/snapshot");
  }

  async getSyncSnapshotInfo(): Promise<SyncSnapshotInfo> {
    return this._get<SyncSnapshotInfo>("/sync/snapshot/info");
  }

  async getSyncSnapshot(): Promise<Response> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout * 10);

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

  async faucetClaim(address: Address): Promise<FaucetClaimResponse> {
    return this._post<FaucetClaimResponse>("/claim", { address });
  }

  async faucetStatus(): Promise<FaucetStatus> {
    return this._get<FaucetStatus>("/status");
  }

  async faucetHealth(): Promise<FaucetHealth> {
    return this._get<FaucetHealth>("/health");
  }

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

  async ethChainId(): Promise<string> {
    return this.ethRpc<string>("eth_chainId");
  }

  async ethBlockNumber(): Promise<string> {
    return this.ethRpc<string>("eth_blockNumber");
  }

  async ethGetBalance(address: string, block: string = "latest"): Promise<string> {
    return this.ethRpc<string>("eth_getBalance", [address, block]);
  }

  async ethGetTransactionCount(
    address: string,
    block: string = "latest",
  ): Promise<string> {
    return this.ethRpc<string>("eth_getTransactionCount", [address, block]);
  }

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

      return parseJsonWithBigInts<T>(await res.text());
    } finally {
      clearTimeout(timer);
    }
  }

  private async _post<T>(path: string, body: unknown): Promise<T> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout);
    const serializedBody = stringifyJsonWithBigInts(body);

    try {
      const res = await fetch(`${this.baseUrl}${path}`, {
        method: "POST",
        headers: this.headers,
        body: serializedBody,
        signal: controller.signal,
      });

      if (!res.ok) {
        throw new ArcRpcError(res.status, await res.text());
      }

      return parseJsonWithBigInts<T>(await res.text());
    } finally {
      clearTimeout(timer);
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function assertSignedTransferU64Fields(
  tx: Pick<TxSubmitPayload, "amount" | "nonce" | "fee">,
): void {
  assertU64(tx.amount, "amount");
  assertU64(tx.nonce, "nonce");
  assertU64(tx.fee, "fee");
}
