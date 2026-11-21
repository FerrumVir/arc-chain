import {
  TxType,
} from "./types";
import type {
  Address,
  Hash256,
  TxReceipt,
  AgentConfig,
  EscrowParams,
  SignedTransaction,
  BatchResult,
} from "./types";
import { ArcClient } from "./client";
import { ArcWallet } from "./wallet";

export class ArcAgent {
  readonly client: ArcClient;
  readonly wallet: ArcWallet;
  readonly config: Required<AgentConfig>;

  private _totalSpent: bigint = 0n;
  private _txCount: number = 0;
  private _initialized: boolean = false;

  constructor(
    client: ArcClient,
    wallet: ArcWallet,
    config?: Partial<AgentConfig>
  ) {
    this.client = client;
    this.wallet = wallet;
    this.config = {
      address: wallet.address,
      maxGas: config?.maxGas ?? 1_000_000,
      maxSpend: config?.maxSpend ?? BigInt("1000000000000000000"),
      allowedTypes: config?.allowedTypes ?? Object.values(TxType) as TxType[],
      autoRetry: config?.autoRetry ?? true,
      maxRetries: config?.maxRetries ?? 3,
    };
  }

  static create(rpcUrl: string, config?: Partial<AgentConfig>): ArcAgent {
    const client = new ArcClient({ rpcUrl });
    const wallet = ArcWallet.create();
    return new ArcAgent(client, wallet, config);
  }

  static fromKey(
    rpcUrl: string,
    privateKey: string,
    config?: Partial<AgentConfig>
  ): ArcAgent {
    const client = new ArcClient({ rpcUrl });
    const wallet = ArcWallet.fromPrivateKey(privateKey);
    return new ArcAgent(client, wallet, config);
  }

  async init(): Promise<void> {
    await this.wallet.syncNonce(this.client);
    this._initialized = true;
  }

  get address(): Address {
    return this.wallet.address;
  }

  get totalSpent(): bigint {
    return this._totalSpent;
  }

  get txCount(): number {
    return this._txCount;
  }

  async getBalance(): Promise<bigint> {
    return this.client.getBalance(this.wallet.address);
  }

  async transfer(to: Address, amount: bigint): Promise<TxReceipt> {
    this.assertAllowed("Transfer" as TxType);
    this.assertSpendLimit(amount);

    const tx = this.wallet.transfer(to, amount);
    const receipt = await this.submitWithRetry(tx);

    if (receipt.success) {
      this._totalSpent += amount;
    }

    return receipt;
  }

  async settle(
    to: Address,
    amount: bigint,
    data?: Uint8Array
  ): Promise<TxReceipt> {
    this.assertAllowed("Settle" as TxType);
    this.assertSpendLimit(amount);

    const tx = this.wallet.settle(to, amount, data);
    const receipt = await this.submitWithRetry(tx);

    if (receipt.success) {
      this._totalSpent += amount;
    }

    return receipt;
  }

  async swap(to: Address, amount: bigint): Promise<TxReceipt> {
    this.assertAllowed("Swap" as TxType);
    this.assertSpendLimit(amount);

    const tx = this.wallet.swap(to, amount);
    return this.submitWithRetry(tx);
  }

  async stake(amount: bigint): Promise<TxReceipt> {
    this.assertAllowed("Stake" as TxType);
    this.assertSpendLimit(amount);

    const tx = this.wallet.stake(amount);
    return this.submitWithRetry(tx);
  }

  async callContract(
    contract: Address,
    data: Uint8Array,
    gasLimit?: number
  ): Promise<TxReceipt> {
    this.assertAllowed("WasmCall" as TxType);

    const tx = this.wallet.callContract(
      contract,
      data,
      gasLimit ?? this.config.maxGas
    );
    return this.submitWithRetry(tx);
  }

  async submitBatch(txs: SignedTransaction[]): Promise<BatchResult> {
    return this.client.submitBatch(txs);
  }

  async createEscrow(params: EscrowParams): Promise<TxReceipt> {
    this.assertAllowed("Escrow" as TxType);
    this.assertSpendLimit(params.amount);

    const escrowData = new TextEncoder().encode(
      JSON.stringify({
        beneficiary: params.beneficiary,
        timeout: params.timeout,
        condition_hash: params.conditionHash,
      })
    );

    const tx = this.wallet.sign({
      tx_type: "Escrow" as TxType,
      from: this.wallet.address,
      to: params.beneficiary,
      amount: params.amount,
      nonce: this.wallet.nonce,
      data: escrowData,
    });

    return this.submitWithRetry(tx);
  }

  async getChainHeight(): Promise<number> {
    const info = await this.client.info();
    return info.chain_height;
  }

  onBlock(
    callback: (block: { height: number; tx_count: number }) => void,
    pollInterval?: number
  ): { unsubscribe: () => void } {
    return this.client.onBlock(
      (block) =>
        callback({
          height: block.height,
          tx_count: block.tx_count,
        }),
      pollInterval
    );
  }

  async verifyTx(txHash: Hash256): Promise<boolean> {
    try {
      const proof = await this.client.getMerkleProof(txHash);
      return this.client.verifyMerkleProof(proof);
    } catch {
      return false;
    }
  }

  private assertAllowed(txType: TxType): void {
    if (!this.config.allowedTypes.includes(txType)) {
      throw new AgentError(
        `Transaction type ${txType} not allowed for this agent`
      );
    }
  }

  private assertSpendLimit(amount: bigint): void {
    if (this._totalSpent + amount > this.config.maxSpend) {
      throw new AgentError(
        `Spend limit exceeded: ${this._totalSpent + amount} > ${this.config.maxSpend}`
      );
    }
  }

  private async submitWithRetry(tx: SignedTransaction): Promise<TxReceipt> {
    let lastError: Error | null = null;
    const maxAttempts = this.config.autoRetry ? this.config.maxRetries : 1;

    for (let attempt = 0; attempt < maxAttempts; attempt++) {
      try {
        const receipt = await this.client.submitTx(tx);
        this._txCount++;
        return receipt;
      } catch (err) {
        lastError = err as Error;
        if (attempt < maxAttempts - 1) {
          await new Promise((r) => setTimeout(r, 100 * 2 ** attempt));
        }
      }
    }

    throw lastError ?? new AgentError("Transaction submission failed");
  }
}

export class AgentError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "AgentError";
  }
}
