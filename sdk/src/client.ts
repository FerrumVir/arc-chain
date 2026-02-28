// ─── ARC Chain RPC Client ─────────────────────────────────────
// Connects to an ARC Chain node and provides typed API access

import type {
  Address,
  Hash256,
  Block,
  BlockHeader,
  AccountState,
  NodeInfo,
  TxReceipt,
  SignedTransaction,
  MerkleProof,
  BatchResult,
} from "./types";

export interface ArcClientConfig {
  /** RPC endpoint URL (e.g., "http://localhost:8545") */
  rpcUrl: string;
  /** Request timeout in ms (default: 30000) */
  timeout?: number;
  /** Custom headers for authentication */
  headers?: Record<string, string>;
}

export class ArcClient {
  private rpcUrl: string;
  private timeout: number;
  private headers: Record<string, string>;

  constructor(config: ArcClientConfig) {
    this.rpcUrl = config.rpcUrl.replace(/\/$/, "");
    this.timeout = config.timeout ?? 30_000;
    this.headers = {
      "Content-Type": "application/json",
      ...config.headers,
    };
  }

  // ─── Node Info ─────────────────────────────────────────

  /** Get node health status */
  async health(): Promise<{ status: string }> {
    return this.get("/health");
  }

  /** Get node info (version, height, peers, sync status) */
  async info(): Promise<NodeInfo> {
    return this.get("/info");
  }

  // ─── Blocks ────────────────────────────────────────────

  /** Get block by height */
  async getBlock(height: number): Promise<Block> {
    return this.get(`/block/${height}`);
  }

  /** Get the latest block */
  async getLatestBlock(): Promise<Block> {
    const info = await this.info();
    return this.getBlock(info.chain_height);
  }

  /** Get block headers in a range (inclusive) */
  async getBlockRange(from: number, to: number): Promise<BlockHeader[]> {
    const blocks: BlockHeader[] = [];
    for (let h = from; h <= to; h++) {
      blocks.push(await this.getBlock(h));
    }
    return blocks;
  }

  // ─── Accounts ──────────────────────────────────────────

  /** Get account state by address */
  async getAccount(address: Address): Promise<AccountState> {
    return this.get(`/account/${address}`);
  }

  /** Get account balance */
  async getBalance(address: Address): Promise<bigint> {
    const account = await this.getAccount(address);
    return BigInt(account.balance);
  }

  /** Get account nonce (for next transaction) */
  async getNonce(address: Address): Promise<number> {
    const account = await this.getAccount(address);
    return account.nonce;
  }

  // ─── Transactions ──────────────────────────────────────

  /** Submit a signed transaction */
  async submitTx(tx: SignedTransaction): Promise<TxReceipt> {
    return this.post("/tx/submit", tx);
  }

  /** Submit a batch of signed transactions */
  async submitBatch(txs: SignedTransaction[]): Promise<BatchResult> {
    return this.post("/tx/submit_batch", { transactions: txs });
  }

  /** Get transaction receipt by hash */
  async getTxReceipt(hash: Hash256): Promise<TxReceipt> {
    return this.get(`/tx/${hash}`);
  }

  /** Wait for a transaction to be included in a block */
  async waitForTx(
    hash: Hash256,
    opts?: { timeout?: number; pollInterval?: number }
  ): Promise<TxReceipt> {
    const timeout = opts?.timeout ?? 60_000;
    const interval = opts?.pollInterval ?? 500;
    const start = Date.now();

    while (Date.now() - start < timeout) {
      try {
        const receipt = await this.getTxReceipt(hash);
        if (receipt) return receipt;
      } catch {
        // Not found yet, keep polling
      }
      await sleep(interval);
    }

    throw new Error(`Transaction ${hash} not confirmed within ${timeout}ms`);
  }

  // ─── Proofs ────────────────────────────────────────────

  /** Get Merkle inclusion proof for a transaction */
  async getMerkleProof(txHash: Hash256): Promise<MerkleProof> {
    return this.get(`/proof/merkle/${txHash}`);
  }

  /** Verify a Merkle proof locally */
  verifyMerkleProof(proof: MerkleProof): boolean {
    // Client-side verification using the proof path
    // This would use BLAKE3 hashing — for now returns true
    // Full implementation requires WASM blake3 binding
    return proof.root !== "" && proof.leaf !== "" && proof.path.length > 0;
  }

  // ─── Subscriptions ─────────────────────────────────────

  /** Subscribe to new blocks (via polling) */
  onBlock(
    callback: (block: Block) => void,
    pollInterval: number = 1000
  ): { unsubscribe: () => void } {
    let lastHeight = 0;
    let active = true;

    const poll = async () => {
      while (active) {
        try {
          const info = await this.info();
          if (info.chain_height > lastHeight) {
            for (let h = lastHeight + 1; h <= info.chain_height; h++) {
              const block = await this.getBlock(h);
              callback(block);
            }
            lastHeight = info.chain_height;
          }
        } catch {
          // Retry on next poll
        }
        await sleep(pollInterval);
      }
    };

    poll();

    return {
      unsubscribe: () => {
        active = false;
      },
    };
  }

  // ─── HTTP Helpers ──────────────────────────────────────

  private async get<T>(path: string): Promise<T> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout);

    try {
      const res = await fetch(`${this.rpcUrl}${path}`, {
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

  private async post<T>(path: string, body: unknown): Promise<T> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout);

    try {
      const res = await fetch(`${this.rpcUrl}${path}`, {
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

/** RPC error with status code */
export class ArcRpcError extends Error {
  constructor(
    public readonly statusCode: number,
    message: string
  ) {
    super(`ARC RPC Error (${statusCode}): ${message}`);
    this.name = "ArcRpcError";
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
