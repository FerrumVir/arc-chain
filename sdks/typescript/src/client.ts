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
  SignedTransferTransaction,
  SignedTransferSubmitPayload,
  U64,
} from "./types";
import {
  assertU64,
  parseJsonWithBigInts,
  stringifyJsonWithBigInts,
  u64ToBigInt,
} from "./u64";

const MAX_TX_SUBMIT_BATCH_SIZE = 64;

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

export interface ArcClientOptions {
  timeout?: number;
  headers?: Record<string, string>;
}

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

    return parseJsonWithBigInts<T>(await resp.text());
  }

  private async _post<T = unknown>(path: string, body: unknown): Promise<T> {
    const url = `${this.rpcUrl}${path}`;
    const serializedBody = stringifyJsonWithBigInts(body);

    let resp: Response;
    try {
      resp = await fetch(url, {
        method: "POST",
        headers: this.headers,
        body: serializedBody,
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

    return parseJsonWithBigInts<T>(await resp.text());
  }

  async getBlock(height: number): Promise<Block> {
    return this._get<Block>(`/block/${height}`);
  }

  async getBlocks(
    fromHeight: number = 0,
    toHeight?: number,
    limit: number = 20
  ): Promise<{ from: number; to: number; count: number; blocks: BlockSummary[] }> {
    const params: Record<string, string | number> = { from: fromHeight, limit };
    if (toHeight !== undefined) params.to = toHeight;
    return this._get(`/blocks`, params);
  }

  async getBlockTxs(
    height: number,
    offset: number = 0,
    limit: number = 100
  ): Promise<Record<string, unknown>> {
    return this._get(`/block/${height}/txs`, { offset, limit });
  }

  async getBlockProofs(height: number): Promise<Record<string, unknown>> {
    return this._get(`/block/${height}/proofs`);
  }

  async getAccount(address: string): Promise<Account> {
    return this._get<Account>(`/account/${address}`);
  }

  async getAccountTxs(
    address: string
  ): Promise<{ address: string; tx_count: number; tx_hashes: string[] }> {
    return this._get(`/account/${address}/txs`);
  }

  async submitTransaction(
    tx: SignedTransferSubmitPayload | SignedTransferTransaction,
  ): Promise<string> {
    const payload = this._normalizeSignedTransfer(tx);
    await this._assertTransactionDomain(tx.transaction_domain);
    const data = await this._post<SubmitResult>("/tx/submit", payload);
    return data.tx_hash;
  }

  async getTransaction(txHash: string): Promise<Receipt> {
    return this._get<Receipt>(`/tx/${txHash}`);
  }

  async getFullTransaction(txHash: string): Promise<Record<string, unknown>> {
    return this._get(`/tx/${txHash}/full`);
  }

  async getTxProof(txHash: string): Promise<Record<string, unknown>> {
    return this._get(`/tx/${txHash}/proof`);
  }

  async submitBatch(
    txs: Array<SignedTransferSubmitPayload | SignedTransferTransaction>,
  ): Promise<BatchResult> {
    if (txs.length > MAX_TX_SUBMIT_BATCH_SIZE) {
      throw new RangeError(
        `transaction batch exceeds the maximum of ${MAX_TX_SUBMIT_BATCH_SIZE} items`,
      );
    }
    for (const tx of txs) this._assertSignedTransferU64Fields(tx);
    const advertised = await this.getTransactionDomain();
    const normalized = txs.map((tx) => {
      this._requireMatchingTransactionDomain(tx.transaction_domain, advertised);
      return this._normalizeSignedTransfer(tx);
    });

    return this._post<BatchResult>("/tx/submit_batch", {
      transactions: normalized,
    });
  }

  async getTransactionDomain(): Promise<string | null> {
    let value: Record<string, unknown>;
    try {
      value = await this._get<Record<string, unknown>>("/network/info");
    } catch (error) {
      if (error instanceof ArcError && error.statusCode === 404) return null;
      throw error;
    }
    const raw = value.transaction_domain;
    const protocolMajor = Number.parseInt(
      String(value.protocol_version ?? "0").split(".")[0] ?? "0",
      10,
    );
    if (raw == null) {
      if (value.recovery_active === true || protocolMajor >= 3) {
        throw new ArcError(
          "Node requires recovery-domain signatures but omitted transaction_domain",
        );
      }
      return null;
    }
    if (typeof raw !== "string" || !/^0x[0-9a-fA-F]{64}$/.test(raw)) {
      throw new ArcError("Node returned a malformed 32-byte transaction_domain");
    }
    if (/^0x0{64}$/i.test(raw)) {
      throw new ArcError("Node returned an all-zero transaction_domain");
    }
    return raw.toLowerCase();
  }

  private _normalizeSignedTransfer(
    tx: SignedTransferSubmitPayload | SignedTransferTransaction,
  ): Record<string, unknown> {
    this._assertSignedTransferU64Fields(tx);
    if ("body" in tx) {
      if (tx.tx_type !== "Transfer" || tx.body.type !== "Transfer") {
        throw new ArcTransactionError(
          "POST /tx/submit only accepts signed Transfer transactions",
        );
      }
      assertU64(tx.gas_limit, "gas_limit");
      if (u64ToBigInt(tx.gas_limit, "gas_limit") !== 0n) {
        throw new ArcTransactionError(
          "gas_limit must be zero for the flat transfer RPC",
        );
      }
      return {
        from: tx.from,
        to: tx.body.to,
        amount: tx.body.amount,
        nonce: tx.nonce,
        fee: tx.fee,
        tx_type: "Transfer",
        signature: tx.signature.Ed25519.signature,
        public_key: tx.signature.Ed25519.public_key,
      };
    }
    const { transaction_domain: _domain, ...wire } = tx;
    return wire;
  }

  private _assertSignedTransferU64Fields(
    tx: SignedTransferSubmitPayload | SignedTransferTransaction,
  ): void {
    const amount = "body" in tx ? tx.body.amount : tx.amount;
    assertU64(amount, "amount");
    assertU64(tx.nonce, "nonce");
    assertU64(tx.fee, "fee");
  }

  private async _assertTransactionDomain(signedDomain: string | null): Promise<void> {
    const advertised = await this.getTransactionDomain();
    this._requireMatchingTransactionDomain(signedDomain, advertised);
  }

  private _requireMatchingTransactionDomain(
    signedDomain: string | null,
    advertised: string | null,
  ): void {
    let normalized: string | null = null;
    if (signedDomain !== null) {
      const raw = signedDomain.replace(/^0x/i, "");
      if (!/^[0-9a-fA-F]{64}$/.test(raw) || /^0{64}$/.test(raw)) {
        throw new ArcTransactionError(
          "Transaction signature domain must be a non-zero 32-byte hex value",
        );
      }
      normalized = `0x${raw.toLowerCase()}`;
    }
    if (normalized !== advertised) {
      throw new ArcTransactionError(
        `Transaction signature domain mismatch: signed for ${normalized ?? "legacy-v1"}, ` +
          `node requires ${advertised ?? "legacy-v1"}`,
      );
    }
  }

  async getChainInfo(): Promise<ChainInfo> {
    return this._get<ChainInfo>("/info");
  }

  async getStats(): Promise<ChainStats> {
    return this._get<ChainStats>("/stats");
  }

  async getHealth(): Promise<HealthInfo> {
    return this._get<HealthInfo>("/health");
  }

  async getNodeInfo(): Promise<NodeInfo> {
    return this._get<NodeInfo>("/node/info");
  }

  async getContractInfo(address: string): Promise<ContractInfo> {
    return this._get<ContractInfo>(`/contract/${address}`);
  }

  async callContract(
    address: string,
    func: string,
    calldata?: string,
    fromAddr?: string,
    gasLimit: U64 = 1_000_000
  ): Promise<ContractCallResult> {
    assertU64(gasLimit, "gas_limit");
    const body: Record<string, unknown> = {
      function: func,
      gas_limit: gasLimit,
    };
    if (calldata) body.calldata = calldata;
    if (fromAddr) body.from = fromAddr;
    return this._post<ContractCallResult>(`/contract/${address}/call`, body);
  }

  async getLightSnapshot(): Promise<LightSnapshot> {
    return this._get<LightSnapshot>("/light/snapshot");
  }

  async getSyncSnapshotInfo(): Promise<SyncSnapshotInfo> {
    return this._get<SyncSnapshotInfo>("/sync/snapshot/info");
  }

  async ethCall(method: string, params: unknown[] = []): Promise<EthRpcResponse> {
    return this._post<EthRpcResponse>("/eth", {
      jsonrpc: "2.0",
      method,
      params,
      id: 1,
    });
  }
}
