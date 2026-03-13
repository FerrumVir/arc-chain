/**
 * Unit tests for the ARC Chain TypeScript SDK.
 *
 * Uses Jest with mocked fetch so no running node is needed.
 */

import {
  ArcClient,
  ArcError,
  ArcConnectionError,
  ArcTransactionError,
} from "../src/client";
import { KeyPair } from "../src/crypto";
import { TransactionBuilder } from "../src/transaction";

// ---------------------------------------------------------------------------
// Mock fetch helper
// ---------------------------------------------------------------------------

function mockFetchResponse(data: unknown, status: number = 200): jest.Mock {
  return jest.fn().mockResolvedValue({
    status,
    json: async () => data,
    text: async () => JSON.stringify(data),
  });
}

function mockFetchReject(error: Error): jest.Mock {
  return jest.fn().mockRejectedValue(error);
}

beforeEach(() => {
  jest.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Client — getBlock
// ---------------------------------------------------------------------------

describe("ArcClient.getBlock", () => {
  it("returns block data on success", async () => {
    const blockData = {
      hash: "ab".repeat(32),
      header: {
        height: 42,
        parent_hash: "cd".repeat(32),
        tx_root: "ef".repeat(32),
        state_root: "01".repeat(32),
        tx_count: 5,
        timestamp: 1700000000,
        producer: "aa".repeat(32),
      },
      tx_hashes: ["ff".repeat(32)],
    };

    global.fetch = mockFetchResponse(blockData);
    const client = new ArcClient("http://localhost:9000");
    const result = await client.getBlock(42);

    expect(result.header.height).toBe(42);
    expect(result.header.tx_count).toBe(5);
    expect(result.tx_hashes).toHaveLength(1);
    expect(global.fetch).toHaveBeenCalledTimes(1);
  });

  it("throws ArcError on 404", async () => {
    global.fetch = mockFetchResponse({}, 404);
    const client = new ArcClient("http://localhost:9000");

    await expect(client.getBlock(999999)).rejects.toThrow(ArcError);
    await expect(client.getBlock(999999)).rejects.toMatchObject({
      statusCode: 404,
    });
  });

  it("throws ArcConnectionError on network failure", async () => {
    global.fetch = mockFetchReject(new Error("ECONNREFUSED"));
    const client = new ArcClient("http://localhost:9000");

    await expect(client.getBlock(1)).rejects.toThrow(ArcConnectionError);
  });
});

// ---------------------------------------------------------------------------
// Client — getAccount
// ---------------------------------------------------------------------------

describe("ArcClient.getAccount", () => {
  it("returns account data on success", async () => {
    const addr = "ab".repeat(32);
    const accountData = {
      address: addr,
      balance: 1000000,
      nonce: 5,
    };

    global.fetch = mockFetchResponse(accountData);
    const client = new ArcClient("http://localhost:9000");
    const result = await client.getAccount(addr);

    expect(result.balance).toBe(1000000);
    expect(result.nonce).toBe(5);
  });
});

// ---------------------------------------------------------------------------
// Client — submitTransaction
// ---------------------------------------------------------------------------

describe("ArcClient.submitTransaction", () => {
  it("submits a raw transaction and returns hash", async () => {
    const expectedHash = "de".repeat(32);
    global.fetch = mockFetchResponse({
      tx_hash: expectedHash,
      status: "pending",
    });

    const client = new ArcClient("http://localhost:9000");
    const txHash = await client.submitTransaction({
      from: "aa".repeat(32),
      to: "bb".repeat(32),
      amount: 100,
      nonce: 0,
    });

    expect(txHash).toBe(expectedHash);
  });

  it("submits a TransactionBuilder tx and returns hash", async () => {
    const expectedHash = "de".repeat(32);
    global.fetch = mockFetchResponse({
      tx_hash: expectedHash,
      status: "pending",
    });

    const kp = await KeyPair.generate();
    const tx = TransactionBuilder.transfer(
      kp.address(),
      "bb".repeat(32),
      100
    );
    const signed = await TransactionBuilder.sign(tx, kp);

    const client = new ArcClient("http://localhost:9000");
    const txHash = await client.submitTransaction(signed);

    expect(txHash).toBe(expectedHash);
  });

  it("throws ArcTransactionError on 409 conflict", async () => {
    global.fetch = mockFetchResponse({}, 409);
    const client = new ArcClient("http://localhost:9000");

    await expect(
      client.submitTransaction({
        from: "aa".repeat(32),
        to: "bb".repeat(32),
        amount: 100,
        nonce: 0,
      })
    ).rejects.toThrow(ArcTransactionError);
  });
});

// ---------------------------------------------------------------------------
// Client — submitBatch
// ---------------------------------------------------------------------------

describe("ArcClient.submitBatch", () => {
  it("submits a batch and returns results", async () => {
    global.fetch = mockFetchResponse({
      accepted: 2,
      rejected: 0,
      tx_hashes: ["aa".repeat(32), "bb".repeat(32)],
    });

    const client = new ArcClient("http://localhost:9000");
    const result = await client.submitBatch([
      { from: "11".repeat(32), to: "22".repeat(32), amount: 10, nonce: 0 },
      { from: "33".repeat(32), to: "44".repeat(32), amount: 20, nonce: 0 },
    ]);

    expect(result.accepted).toBe(2);
    expect(result.tx_hashes).toHaveLength(2);
  });
});

// ---------------------------------------------------------------------------
// Client — getChainInfo / getStats
// ---------------------------------------------------------------------------

describe("ArcClient.getChainInfo", () => {
  it("returns chain info", async () => {
    global.fetch = mockFetchResponse({
      chain: "ARC Chain",
      version: "0.1.0",
      block_height: 100,
      account_count: 50,
      mempool_size: 3,
      gpu: { name: "Apple M2" },
    });

    const client = new ArcClient("http://localhost:9000");
    const info = await client.getChainInfo();

    expect(info.chain).toBe("ARC Chain");
    expect(info.block_height).toBe(100);
  });
});

describe("ArcClient.getStats", () => {
  it("returns chain stats", async () => {
    global.fetch = mockFetchResponse({
      chain: "ARC Chain",
      version: "0.1.0",
      block_height: 100,
      total_accounts: 50,
      mempool_size: 3,
      total_transactions: 1000,
      indexed_hashes: 500,
      indexed_receipts: 500,
    });

    const client = new ArcClient("http://localhost:9000");
    const stats = await client.getStats();

    expect(stats.total_transactions).toBe(1000);
    expect(stats.block_height).toBe(100);
  });
});

// ---------------------------------------------------------------------------
// Client — ethCall
// ---------------------------------------------------------------------------

describe("ArcClient.ethCall", () => {
  it("sends eth_chainId and returns result", async () => {
    global.fetch = mockFetchResponse({
      jsonrpc: "2.0",
      id: 1,
      result: "0x415243",
    });

    const client = new ArcClient("http://localhost:9000");
    const result = await client.ethCall("eth_chainId");

    expect(result.result).toBe("0x415243");
  });

  it("sends eth_blockNumber and returns hex height", async () => {
    global.fetch = mockFetchResponse({
      jsonrpc: "2.0",
      id: 1,
      result: "0x64",
    });

    const client = new ArcClient("http://localhost:9000");
    const result = await client.ethCall("eth_blockNumber");

    expect(result.result).toBe("0x64");
  });
});

// ---------------------------------------------------------------------------
// KeyPair
// ---------------------------------------------------------------------------

describe("KeyPair", () => {
  it("generates a random key pair", async () => {
    const kp = await KeyPair.generate();
    expect(kp.address()).toHaveLength(64);
    expect(kp.publicKeyHex()).toHaveLength(64);
    expect(kp.publicKeyBytes()).toHaveLength(32);
  });

  it("two random keys differ", async () => {
    const kp1 = await KeyPair.generate();
    const kp2 = await KeyPair.generate();
    expect(kp1.address()).not.toBe(kp2.address());
    expect(kp1.publicKeyHex()).not.toBe(kp2.publicKeyHex());
  });

  it("fromSeed is deterministic", async () => {
    const seed = new Uint8Array(32);
    for (let i = 0; i < 32; i++) seed[i] = i;

    const kp1 = await KeyPair.fromSeed(seed);
    const kp2 = await KeyPair.fromSeed(seed);
    expect(kp1.address()).toBe(kp2.address());
    expect(kp1.publicKeyHex()).toBe(kp2.publicKeyHex());
  });

  it("fromSeed rejects wrong length", async () => {
    const shortSeed = new Uint8Array(16);
    await expect(KeyPair.fromSeed(shortSeed)).rejects.toThrow(
      "Seed must be exactly 32 bytes"
    );
  });

  it("fromPrivateKeyHex works", async () => {
    const seed = new Uint8Array(32);
    for (let i = 0; i < 32; i++) seed[i] = i;
    const hex = Array.from(seed)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");

    const kp1 = await KeyPair.fromSeed(seed);
    const kp2 = await KeyPair.fromPrivateKeyHex(hex);
    expect(kp1.address()).toBe(kp2.address());
  });

  it("sign and verify round-trip", async () => {
    const kp = await KeyPair.generate();
    const msg = new TextEncoder().encode("hello ARC chain");
    const sig = await kp.sign(msg);

    expect(sig).toHaveLength(64);
    expect(await kp.verify(msg, sig)).toBe(true);
  });

  it("wrong message fails verification", async () => {
    const kp = await KeyPair.generate();
    const sig = await kp.sign(new TextEncoder().encode("message A"));
    expect(
      await kp.verify(new TextEncoder().encode("message B"), sig)
    ).toBe(false);
  });

  it("wrong key fails verification", async () => {
    const kp1 = await KeyPair.generate();
    const kp2 = await KeyPair.generate();
    const sig = await kp1.sign(new TextEncoder().encode("test"));
    expect(await kp2.verify(new TextEncoder().encode("test"), sig)).toBe(false);
  });

  it("verifyWithPublicKey works", async () => {
    const kp = await KeyPair.generate();
    const msg = new TextEncoder().encode("verify me");
    const sig = await kp.sign(msg);
    expect(
      await KeyPair.verifyWithPublicKey(kp.publicKeyBytes(), msg, sig)
    ).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// TransactionBuilder
// ---------------------------------------------------------------------------

describe("TransactionBuilder", () => {
  it("builds a valid transfer transaction", () => {
    const from = "aa".repeat(32);
    const to = "bb".repeat(32);
    const tx = TransactionBuilder.transfer(from, to, 1000, 5, 1);

    expect(tx.tx_type).toBe("Transfer");
    expect(tx.from).toBe(from);
    expect(tx.to).toBe(to);
    expect(tx.amount).toBe(1000);
    expect(tx.fee).toBe(5);
    expect(tx.nonce).toBe(1);
    expect(tx.hash).toHaveLength(64);
    expect(tx.signature).toBeNull();
  });

  it("transfer hash is deterministic", () => {
    const from = "aa".repeat(32);
    const to = "bb".repeat(32);
    const tx1 = TransactionBuilder.transfer(from, to, 1000);
    const tx2 = TransactionBuilder.transfer(from, to, 1000);
    expect(tx1.hash).toBe(tx2.hash);
  });

  it("transfer hash changes with nonce", () => {
    const from = "aa".repeat(32);
    const to = "bb".repeat(32);
    const tx1 = TransactionBuilder.transfer(from, to, 1000, 1, 0);
    const tx2 = TransactionBuilder.transfer(from, to, 1000, 1, 1);
    expect(tx1.hash).not.toBe(tx2.hash);
  });

  it("rejects invalid addresses", () => {
    expect(() =>
      TransactionBuilder.transfer("short", "bb".repeat(32), 1000)
    ).toThrow("fromAddr must be 64 hex characters");
  });

  it("rejects zero amount", () => {
    expect(() =>
      TransactionBuilder.transfer("aa".repeat(32), "bb".repeat(32), 0)
    ).toThrow("Amount must be positive");
  });

  it("builds a deploy contract transaction", () => {
    const from = "aa".repeat(32);
    const code = new Uint8Array([0x00, 0x61, 0x73, 0x6d]);
    const tx = TransactionBuilder.deployContract(from, code, 100000);

    expect(tx.tx_type).toBe("DeployContract");
    expect(tx.gas_limit).toBe(100000);
    expect(tx.hash).toHaveLength(64);
  });

  it("builds a call contract transaction", () => {
    const from = "aa".repeat(32);
    const contract = "cc".repeat(32);
    const tx = TransactionBuilder.callContract(
      from,
      contract,
      new Uint8Array([0x01, 0x02]),
      50,
      1_000_000,
      "transfer"
    );

    expect(tx.tx_type).toBe("WasmCall");
    expect((tx.body as any).contract).toBe(contract);
    expect((tx.body as any).function).toBe("transfer");
  });

  it("builds a stake transaction", () => {
    const from = "aa".repeat(32);
    const tx = TransactionBuilder.stake(from, 10000);

    expect(tx.tx_type).toBe("Stake");
    expect((tx.body as any).is_stake).toBe(true);
    expect((tx.body as any).amount).toBe(10000);
  });

  it("builds an unstake transaction", () => {
    const from = "aa".repeat(32);
    const tx = TransactionBuilder.stake(from, 5000, false);

    expect((tx.body as any).is_stake).toBe(false);
  });

  it("builds a settle transaction with zero fee", () => {
    const from = "aa".repeat(32);
    const agent = "bb".repeat(32);
    const service = "cc".repeat(32);
    const tx = TransactionBuilder.settle(from, agent, service, 500, 100);

    expect(tx.tx_type).toBe("Settle");
    expect(tx.fee).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// Transaction signing
// ---------------------------------------------------------------------------

describe("TransactionBuilder.sign", () => {
  it("signs a transfer and attaches Ed25519 signature", async () => {
    const kp = await KeyPair.generate();
    const tx = TransactionBuilder.transfer(
      kp.address(),
      "bb".repeat(32),
      1000
    );
    const signed = await TransactionBuilder.sign(tx, kp);

    expect(signed.signature).not.toBeNull();
    expect(signed.signature!.Ed25519.public_key).toBe(kp.publicKeyHex());

    // Verify the signature
    const { hexToBytes } = await import("@noble/hashes/utils");
    const sigBytes = hexToBytes(signed.signature!.Ed25519.signature);
    const hashBytes = hexToBytes(signed.hash);
    expect(await kp.verify(hashBytes, sigBytes)).toBe(true);
  });

  it("rejects signing with wrong key", async () => {
    const kp1 = await KeyPair.generate();
    const kp2 = await KeyPair.generate();
    const tx = TransactionBuilder.transfer(
      kp1.address(),
      "bb".repeat(32),
      1000
    );

    await expect(TransactionBuilder.sign(tx, kp2)).rejects.toThrow(
      "does not match tx sender"
    );
  });

  it("does not mutate the original transaction", async () => {
    const kp = await KeyPair.generate();
    const tx = TransactionBuilder.transfer(
      kp.address(),
      "bb".repeat(32),
      1000
    );
    const originalSig = tx.signature;
    await TransactionBuilder.sign(tx, kp);
    expect(tx.signature).toEqual(originalSig);
  });
});
