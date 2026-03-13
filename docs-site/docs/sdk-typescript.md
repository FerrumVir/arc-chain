---
title: "TypeScript SDK"
sidebar_position: 5
slug: "/sdk-typescript"
---
# TypeScript SDK Reference

The ARC Chain TypeScript SDK provides a typed HTTP client for all RPC endpoints, Ed25519 key management, transaction building and signing, and Ethereum-standard ABI encoding/decoding. Uses the native `fetch` API and works in Node.js 18+, Deno, Bun, and browsers.

**Package:** `@arc-chain/sdk`

---

## Installation

From source (in the arc-chain repository):

```bash
cd sdks/typescript
npm install
npm run build
```

Or when published:

```bash
npm install @arc-chain/sdk
```

**Dependencies:** `@noble/hashes` (Keccak-256 for ABI selectors). Uses native `fetch` (no HTTP library needed).

## Quick Start

```typescript
import { ArcClient, KeyPair, TransactionBuilder } from "@arc-chain/sdk";

// Connect to a local node
const client = new ArcClient("http://localhost:9090");

// Check node health
const health = await client.getHealth();
console.log(health.status); // "ok"

// Generate a key pair
const kp = await KeyPair.generate();
console.log(`Address: ${kp.address()}`);

// Build, sign, and submit a transfer
const tx = TransactionBuilder.transfer(kp.address(), "0".repeat(64), 1000);
const signed = await TransactionBuilder.sign(tx, kp);
const hash = await client.submitTransaction(signed);
console.log(`Submitted: ${hash}`);
```

---

## ArcClient

### Constructor

```typescript
const client = new ArcClient("http://localhost:9090", {
  timeout: 30_000,  // Request timeout in ms (default 30s)
  headers: {
    "X-Custom-Header": "value",
  },
});
```

### Chain Info Methods

```typescript
const health = await client.getHealth();       // -> HealthInfo
const info = await client.getChainInfo();      // -> ChainInfo
const node = await client.getNodeInfo();       // -> NodeInfo
const stats = await client.getStats();         // -> ChainStats
```

### Block Methods

```typescript
// Get block by height
const block = await client.getBlock(42);       // -> Block
console.log(block.header.height, block.header.tx_count);

// Paginated block listing
const result = await client.getBlocks(0, 100, 20); // -> { blocks: BlockSummary[], ... }
for (const b of result.blocks) {
  console.log(`Block ${b.height}: ${b.tx_count} txs`);
}

// Transaction listing for a block
const txs = await client.getBlockTxs(42, 0, 100);

// Merkle proofs for all transactions in a block
const proofs = await client.getBlockProofs(42);
```

### Account Methods

```typescript
// Get account state
const account = await client.getAccount("af1349b9..."); // -> Account
console.log(account.balance, account.nonce, account.staked_balance);

// Transaction history
const txs = await client.getAccountTxs("af1349b9...");
console.log(txs.tx_hashes);
```

### Transaction Methods

```typescript
// Submit a single transaction
const hash = await client.submitTransaction(signedTx);   // -> SubmitResult

// Submit a batch
const result = await client.submitBatch([tx1, tx2]);      // -> BatchResult
console.log(`Accepted: ${result.accepted}, Rejected: ${result.rejected}`);

// Get receipt
const receipt = await client.getTransaction("a1b2c3d4..."); // -> Receipt

// Get full transaction body
const full = await client.getFullTransaction("a1b2c3d4...");

// Get Merkle inclusion proof
const proof = await client.getTxProof("a1b2c3d4...");
```

### Contract Methods

```typescript
// Get contract info
const info = await client.getContractInfo("deadbeef..."); // -> ContractInfo

// Read-only contract call
const result = await client.callContract(        // -> ContractCallResult
  "deadbeef...",  // contract address
  "get_count",    // function name
  {
    calldata: "",
    from: "af1349...",
    gasLimit: 100_000,
  }
);
console.log(result.return_data, result.gas_used);
```

### Light Client / Sync

```typescript
const snapshot = await client.getLightSnapshot();       // -> LightSnapshot
const syncInfo = await client.getSyncSnapshotInfo();   // -> SyncSnapshotInfo
```

### ETH JSON-RPC

```typescript
const result = await client.ethCall("eth_blockNumber");
console.log(result.result); // "0x606"

const balance = await client.ethCall("eth_getBalance", [
  "0xaf1349b9...",
  "latest",
]);
```

---

## KeyPair

Ed25519 key generation, signing, and verification.

```typescript
import { KeyPair } from "@arc-chain/sdk";

// Generate a new random key pair
const kp = await KeyPair.generate();

// Get the 64-hex-char address (BLAKE3 hash of public key)
const address = kp.address();  // "af1349b9..."

// Get the public key as hex
const pubkey = kp.publicKeyHex();

// Sign arbitrary bytes
const signature = await kp.sign(new Uint8Array([1, 2, 3]));

// Verify a signature
const isValid = await kp.verify(new Uint8Array([1, 2, 3]), signature);
```

---

## TransactionBuilder

Build unsigned transactions, then sign them with a `KeyPair`.

### Transfer

```typescript
const tx = TransactionBuilder.transfer(
  kp.address(),    // from
  "0".repeat(64),  // to
  1000,            // amount
);
const signed = await TransactionBuilder.sign(tx, kp);
```

### Settle (Zero Fee)

```typescript
const tx = TransactionBuilder.settle(
  kp.address(),
  "abcdef01".repeat(8),  // agent address
  "fedcba98".repeat(8),  // service hash
  500,                    // amount
  10,                     // usage units
  { nonce: 0 }
);
```

### Deploy Contract

```typescript
import { readFileSync } from "fs";

const bytecode = readFileSync("counter.wasm");
const tx = TransactionBuilder.deployContract(kp.address(), bytecode, {
  gasLimit: 2_000_000,
  fee: 50,
  nonce: 1,
});
const signed = await TransactionBuilder.sign(tx, kp);
```

### Call Contract

```typescript
const tx = TransactionBuilder.callContract(
  kp.address(),
  "deadbeef".repeat(8),
  "increment",
  new Uint8Array(),
  { gasLimit: 500_000, fee: 1, nonce: 2 }
);
```

---

## ABI Encoding/Decoding

The `abi` module provides Ethereum-standard ABI encoding and decoding. Uses `@noble/hashes/sha3` for Keccak-256.

### Function Selectors

```typescript
import { functionSelector } from "@arc-chain/sdk";

const sel = functionSelector("transfer(address,uint256)");
// sel is Uint8Array [0xa9, 0x05, 0x9c, 0xbb]
```

### Encode a Function Call

```typescript
import { encodeFunctionCall } from "@arc-chain/sdk";

const calldata = encodeFunctionCall(
  "transfer(address,uint256)",
  "0xdead000000000000000000000000000000000000",
  1000n,
);
// calldata.slice(0, 4) == selector
// calldata.slice(4) == ABI-encoded arguments
```

Note: Use `BigInt` (e.g., `1000n`) for uint/int values to avoid precision loss.

### Encode Raw ABI Data

```typescript
import { encodeAbi } from "@arc-chain/sdk";

const encoded = encodeAbi(
  ["address", "uint256", "bool"],
  ["0xdead000000000000000000000000000000000000", 1000n, true],
);
```

### Decode ABI Data

```typescript
import { decodeAbi } from "@arc-chain/sdk";

const values = decodeAbi(
  ["uint256", "bool"],
  encodedBytes,
);
console.log(values); // [1000n, true]
```

### Decode Function Input (calldata)

```typescript
import { decodeFunctionInput } from "@arc-chain/sdk";

const [name, args] = decodeFunctionInput(
  "transfer(address,uint256)",
  calldata,
);
console.log(name);  // "transfer"
console.log(args);  // ["0xdead...", 1000n]
```

### Supported ABI Types

| Type | TypeScript Representation |
|---|---|
| `uint8` through `uint256` | `bigint` |
| `int8` through `int256` | `bigint` (two's complement) |
| `address` | `string` (hex, with or without `0x`) |
| `bool` | `boolean` |
| `bytes` | `Uint8Array` (dynamic) |
| `bytes1` through `bytes32` | `Uint8Array` (fixed) |
| `string` | `string` (UTF-8) |
| `T[]` | `any[]` (dynamic array) |
| `T[N]` | `any[]` (fixed array) |
| `(T1,T2,...)` | `any[]` (tuple) |

### Keccak-256

```typescript
import { keccak256 } from "@arc-chain/sdk";

const digest = keccak256(new TextEncoder().encode("hello"));
// Returns Uint8Array (32 bytes)
```

---

## Error Handling

```typescript
import {
  ArcError,
  ArcConnectionError,
  ArcTransactionError,
} from "@arc-chain/sdk";

// Base error
try {
  await client.getAccount("nonexistent");
} catch (e) {
  if (e instanceof ArcError) {
    console.log(`Status: ${e.statusCode}, Detail: ${e.detail}`);
  }
}

// Connection errors
try {
  const bad = new ArcClient("http://unreachable:9090", { timeout: 5000 });
  await bad.getHealth();
} catch (e) {
  if (e instanceof ArcConnectionError) {
    console.log(`Connection failed: ${e.url}`);
  }
}

// Transaction errors
try {
  await client.submitTransaction(duplicateTx);
} catch (e) {
  if (e instanceof ArcTransactionError) {
    console.log(`TX rejected: ${e.txHash}`);
  }
}
```

---

## TypeScript Type Exports

All response types are exported for use in your application:

```typescript
import type {
  // Transaction types
  TxType, TxBody, TransferBody, DeployContractBody,
  WasmCallBody, StakeBody, SettleBody,
  Ed25519Signature, Transaction,

  // Chain types
  Account, Block, BlockHeader, BlockSummary,
  EventLog, Receipt, ChainInfo, ChainStats,
  HealthInfo, NodeInfo,

  // Submission results
  SubmitResult, BatchResult,

  // Contract types
  ContractInfo, ContractCallResult,

  // ETH compatibility
  EthRpcResponse,

  // Light client
  LightSnapshot, SyncSnapshotInfo,
} from "@arc-chain/sdk";
```

---

## React Integration Example

```tsx
import { useState, useEffect } from "react";
import { ArcClient } from "@arc-chain/sdk";
import type { ChainStats } from "@arc-chain/sdk";

const client = new ArcClient("http://localhost:9090");

function ChainDashboard() {
  const [stats, setStats] = useState<ChainStats | null>(null);

  useEffect(() => {
    const poll = async () => {
      try {
        setStats(await client.getStats());
      } catch (err) {
        console.error("Failed to fetch stats:", err);
      }
    };
    poll();
    const interval = setInterval(poll, 5000);
    return () => clearInterval(interval);
  }, []);

  if (!stats) return <div>Loading...</div>;

  return (
    <div>
      <h1>ARC Chain</h1>
      <p>Height: {stats.block_height}</p>
      <p>Transactions: {stats.total_transactions.toLocaleString()}</p>
      <p>Accounts: {stats.total_accounts.toLocaleString()}</p>
    </div>
  );
}
```
