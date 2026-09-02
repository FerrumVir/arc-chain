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
import { Channel } from "../src/channel";
import { parseJsonWithBigInts } from "../src/u64";

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
// Client - getBlock
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

  it("fails closed instead of rounding an untyped unsafe integer", async () => {
    global.fetch = jest.fn().mockResolvedValue({
      status: 200,
      text: async () =>
        `{"hash":"${"ab".repeat(32)}","header":{"height":9007199254740993},"tx_hashes":[]}`,
    });
    const client = new ArcClient("http://localhost:9000");

    await expect(client.getBlock(1)).rejects.toThrow("safe-integer range");
  });
});

// ---------------------------------------------------------------------------
// Client - getAccount
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

  it("parses consensus u64 fields above 2^53 without rounding", async () => {
    global.fetch = jest.fn().mockResolvedValue({
      status: 200,
      text: async () =>
        `{"address":"${"ab".repeat(32)}","balance":9007199254740993,"nonce":9007199254740995}`,
    });

    const client = new ArcClient("http://localhost:9000");
    const result = await client.getAccount("ab".repeat(32));

    expect(result.balance).toBe(9_007_199_254_740_993n);
    expect(result.nonce).toBe(9_007_199_254_740_995n);
  });

  it("stays lossless on old runtimes without JSON.parse source context", () => {
    const nativeParse = JSON.parse;
    jest.spyOn(JSON, "parse").mockImplementation(((text, reviver) =>
      nativeParse(
        text,
        reviver === undefined
          ? undefined
          : function (this: unknown, key: string, value: unknown) {
              return (reviver as (this: unknown, key: string, value: unknown) => unknown)
                .call(this, key, value);
            },
      )) as typeof JSON.parse);

    expect(
      parseJsonWithBigInts<{ amount: bigint }>(
        '{"amount":9007199254740993}',
      ).amount,
    ).toBe(9_007_199_254_740_993n);
  });
});

// ---------------------------------------------------------------------------
// Client - submitTransaction
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
      fee: 1,
      signature: "cc".repeat(64),
      public_key: "dd".repeat(32),
      transaction_domain: null,
    });

    expect(txHash).toBe(expectedHash);
    expect(global.fetch).toHaveBeenNthCalledWith(
      2,
      "http://localhost:9000/tx/submit",
      expect.objectContaining({
        body: JSON.stringify({
          from: "aa".repeat(32),
          to: "bb".repeat(32),
          amount: 100,
          nonce: 0,
          fee: 1,
          signature: "cc".repeat(64),
          public_key: "dd".repeat(32),
        }),
      }),
    );
  });

  it("serializes bigint u64 transfer fields as exact Rust JSON numbers", async () => {
    global.fetch = mockFetchResponse({
      tx_hash: "de".repeat(32),
      status: "pending",
    });
    const client = new ArcClient("http://localhost:9000");

    await client.submitTransaction({
      from: "aa".repeat(32),
      to: "bb".repeat(32),
      amount: 9_007_199_254_740_993n,
      nonce: 9_007_199_254_740_995n,
      fee: 9_007_199_254_740_997n,
      signature: "cc".repeat(64),
      public_key: "dd".repeat(32),
      transaction_domain: null,
    });

    const wire = (global.fetch as jest.Mock).mock.calls[1][1].body as string;
    expect(wire).toContain('"amount":9007199254740993');
    expect(wire).toContain('"nonce":9007199254740995');
    expect(wire).toContain('"fee":9007199254740997');
  });

  it("rejects an unsafe number before any network request", async () => {
    global.fetch = mockFetchResponse({});
    const client = new ArcClient("http://localhost:9000");

    await expect(
      client.submitTransaction({
        from: "aa".repeat(32),
        to: "bb".repeat(32),
        amount: Number.MAX_SAFE_INTEGER + 1,
        nonce: 0,
        fee: 1,
        signature: "cc".repeat(64),
        public_key: "dd".repeat(32),
        transaction_domain: null,
      }),
    ).rejects.toThrow("safe integer");
    expect(global.fetch).not.toHaveBeenCalled();
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
    const wire = JSON.parse(
      (global.fetch as jest.Mock).mock.calls[1][1].body as string,
    );
    expect(wire).toEqual({
      from: signed.from,
      to: signed.body.to,
      amount: signed.body.amount,
      nonce: signed.nonce,
      fee: signed.fee,
      tx_type: "Transfer",
      signature: signed.signature.Ed25519.signature,
      public_key: signed.signature.Ed25519.public_key,
    });
    expect(wire).not.toHaveProperty("body");
    expect(wire).not.toHaveProperty("transaction_domain");
  });

  it("throws ArcTransactionError on 409 conflict", async () => {
    global.fetch = jest
      .fn()
      .mockResolvedValueOnce({
        status: 200,
        json: async () => ({ transaction_domain: null }),
        text: async () => '{"transaction_domain":null}',
      })
      .mockResolvedValueOnce({
        status: 409,
        json: async () => ({}),
        text: async () => "{}",
      });
    const client = new ArcClient("http://localhost:9000");

    await expect(
      client.submitTransaction({
        from: "aa".repeat(32),
        to: "bb".repeat(32),
        amount: 100,
        nonce: 0,
        fee: 1,
        signature: "cc".repeat(64),
        public_key: "dd".repeat(32),
        transaction_domain: null,
      })
    ).rejects.toThrow(ArcTransactionError);
  });

  it("fails closed when the signature domain differs from the node", async () => {
    global.fetch = mockFetchResponse({
      protocol_version: "3.0.0",
      recovery_active: true,
      transaction_domain: `0x${"11".repeat(32)}`,
    });
    const client = new ArcClient("http://localhost:9000");

    await expect(
      client.submitTransaction({
        from: "aa".repeat(32),
        to: "bb".repeat(32),
        amount: 100,
        nonce: 0,
        fee: 1,
        signature: "cc".repeat(64),
        public_key: "dd".repeat(32),
        transaction_domain: `0x${"22".repeat(32)}`,
      }),
    ).rejects.toThrow("signature domain mismatch");
    expect(global.fetch).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// Client - submitBatch
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
      {
        from: "11".repeat(32),
        to: "22".repeat(32),
        amount: 10,
        nonce: 0,
        fee: 1,
        signature: "55".repeat(64),
        public_key: "66".repeat(32),
        transaction_domain: null,
      },
      {
        from: "33".repeat(32),
        to: "44".repeat(32),
        amount: 20,
        nonce: 0,
        fee: 1,
        signature: "77".repeat(64),
        public_key: "88".repeat(32),
        transaction_domain: null,
      },
    ]);

    expect(result.accepted).toBe(2);
    expect(result.tx_hashes).toHaveLength(2);
  });

  it("rejects more than the server's 64-item cap before network I/O", async () => {
    global.fetch = mockFetchResponse({});
    const client = new ArcClient("http://localhost:9000");
    const tx = {
      from: "11".repeat(32),
      to: "22".repeat(32),
      amount: 1,
      nonce: 0,
      fee: 1,
      signature: "55".repeat(64),
      public_key: "66".repeat(32),
      transaction_domain: null,
    } as const;

    await expect(client.submitBatch(Array.from({ length: 65 }, () => tx))).rejects
      .toThrow("maximum of 64 items");
    expect(global.fetch).not.toHaveBeenCalled();
  });
});

// ---------------------------------------------------------------------------
// Client - getChainInfo / getStats
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
// Client - ethCall
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

  it("matches RFC 8032 Ed25519 test vector 1", async () => {
    const kp = await KeyPair.fromPrivateKeyHex(
      "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60"
    );
    expect(kp.publicKeyHex()).toBe(
      "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
    );

    const signature = await kp.sign(new Uint8Array());
    expect(Buffer.from(signature).toString("hex")).toBe(
      "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155" +
        "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
    );
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

  it("matches the Rust v1 and v3 transfer hash contract", () => {
    const from = "11".repeat(32);
    const to = "22".repeat(32);
    const v1 = TransactionBuilder.transfer(from, to, 7, 1, 4);
    const v3 = TransactionBuilder.transfer(
      from,
      to,
      7,
      1,
      4,
      `0x${"33".repeat(32)}`,
    );

    expect(v1.hash).toBe(
      "267d4e0c25020d50ae17ce254a28f9556cc086814304902a499916993cb8f05b",
    );
    expect(v3.hash).toBe(
      "5accadc1f889e29e95d2fdac38b3b0db2f76e727c8593bdddb6598c04172f522",
    );
  });

  it("matches Rust on both sides of the 2^53 boundary", () => {
    const from = "11".repeat(32);
    const to = "22".repeat(32);
    const maxSafe = TransactionBuilder.transfer(
      from,
      to,
      Number.MAX_SAFE_INTEGER,
      1,
      4,
    );
    const exactBigint = TransactionBuilder.transfer(
      from,
      to,
      9_007_199_254_740_993n,
      1n,
      4n,
    );

    expect(maxSafe.hash).toBe(
      "89243800e36a725c72c633a4955e86938e0a94b9321e2a1349f3d24ccd165e35",
    );
    expect(exactBigint.hash).toBe(
      "caef4f5305f968dd0b5b8f23a12d85644ba1b80328a0907e764aa59723e8146c",
    );

    const allTransferFields = TransactionBuilder.transfer(
      from,
      to,
      9_007_199_254_740_993n,
      9_007_199_254_740_997n,
      9_007_199_254_740_995n,
    );
    expect(allTransferFields.hash).toBe(
      "6218ebf83c6e14e18216c33cdfaa70ad189ffbb8cb78ffb35a187efef9e51261",
    );

    const deploy = TransactionBuilder.deployContract(
      from,
      new Uint8Array([0, 97, 115, 109]),
      9_007_199_254_740_999n,
      9_007_199_254_740_997n,
      9_007_199_254_740_995n,
      new Uint8Array([1, 2]),
      9_007_199_254_740_993n,
    );
    expect(deploy.hash).toBe(
      "3f3e08f8525892e8564bb488bb7867637444af9c926b950a7ef3b9f4d02253da",
    );
  });

  it.each([
    ["amount", () => TransactionBuilder.transfer("aa".repeat(32), "bb".repeat(32), Number.MAX_SAFE_INTEGER + 1)],
    ["amount", () => TransactionBuilder.transfer("aa".repeat(32), "bb".repeat(32), (1n << 64n))],
    ["amount", () => TransactionBuilder.transfer("aa".repeat(32), "bb".repeat(32), "100" as any)],
    ["fee", () => TransactionBuilder.transfer("aa".repeat(32), "bb".repeat(32), 1, 1.5)],
    ["nonce", () => TransactionBuilder.transfer("aa".repeat(32), "bb".repeat(32), 1, 1, -1)],
    ["gas_limit", () => TransactionBuilder.deployContract("aa".repeat(32), new Uint8Array([1]), Number.MAX_SAFE_INTEGER + 1)],
  ])("rejects an inexact or invalid %s", (_field, build) => {
    expect(build).toThrow(/safe integer|non-negative|u64 range|number or bigint/);
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

  it("recomputes the canonical hash before signing", async () => {
    const kp = await KeyPair.generate();
    const tx = TransactionBuilder.transfer(kp.address(), "bb".repeat(32), 1000);
    const originalHash = tx.hash;
    tx.nonce = 7;

    const signed = await TransactionBuilder.sign(tx, kp);

    expect(signed.hash).not.toBe(originalHash);
    const { hexToBytes } = await import("@noble/hashes/utils");
    expect(
      await kp.verify(
        hexToBytes(signed.hash),
        hexToBytes(signed.signature.Ed25519.signature),
      ),
    ).toBe(true);
  });
});

describe("Channel u64 safety", () => {
  it("rejects unsafe deposits, payments, and nonce increments", () => {
    expect(
      () => new Channel("aa".repeat(32), "opener", Number.MAX_SAFE_INTEGER + 1),
    ).toThrow("safe integer");

    const channel = new Channel("aa".repeat(32), "opener", 100);
    channel.confirmOpen();
    expect(() => channel.pay(Number.MAX_SAFE_INTEGER + 1)).toThrow("safe integer");

    channel.nonce = Number.MAX_SAFE_INTEGER;
    expect(() => channel.proposeState(100, 0)).toThrow("safe integer");
  });
});
