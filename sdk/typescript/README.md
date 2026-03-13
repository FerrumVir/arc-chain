# @arc-chain/sdk

TypeScript SDK for the **ARC Chain** RPC API -- the agent-native L1 blockchain.

Zero dependencies. Uses the built-in Fetch API (Node 18+, all modern browsers).

## Installation

```bash
npm install @arc-chain/sdk
```

## Quick Start

```typescript
import { ArcClient } from "@arc-chain/sdk";

const client = new ArcClient("http://localhost:9090");

// Check node health
const health = await client.getHealth();
console.log(health.status); // "ok"
console.log(health.height); // 1542

// Get chain stats
const stats = await client.getStats();
console.log(stats.total_transactions); // 48230
```

## API Reference

### Constructor

```typescript
const client = new ArcClient(rpcUrl: string, options?: {
  timeout?: number;              // Request timeout in ms (default: 30000)
  headers?: Record<string, string>; // Custom headers
});
```

### Health & Info

```typescript
client.getHealth()    // GET /health    -> HealthResponse
client.getInfo()      // GET /info      -> InfoResponse
client.getNodeInfo()  // GET /node/info -> NodeInfoResponse
client.getStats()     // GET /stats     -> StatsResponse
```

### Blocks

```typescript
// Fetch a single block by height
const block = await client.getBlock(42);
console.log(block.header.tx_count);
console.log(block.tx_hashes);

// Paginated block listing
const blocks = await client.getBlocks({ from: 0, to: 100, limit: 10 });
blocks.blocks.forEach(b => console.log(b.height, b.tx_count));

// Paginated transactions within a block
const txs = await client.getBlockTxs(42, { offset: 0, limit: 50 });
txs.transactions.forEach(tx => console.log(tx.hash, tx.tx_type));

// Merkle proofs for all transactions in a block
const proofs = await client.getBlockProofs(42);
```

### Transactions

```typescript
// Look up a transaction receipt
const receipt = await client.getTx("a1b2c3d4...");
console.log(receipt.success, receipt.gas_used);

// Full transaction with type-specific body
const full = await client.getTxFull("a1b2c3d4...");
console.log(full.tx_type);  // "Transfer", "BatchSettle", "ShardProof", etc.
console.log(full.body);     // Typed body matching tx_type

// Merkle inclusion proof
const proof = await client.getTxProof("a1b2c3d4...");
console.log(proof.verified);

// Submit a transaction
const result = await client.submitTx({
  from: "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
  to:   "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213",
  amount: 1000,
  nonce: 0,
});
console.log(result.tx_hash, result.status);

// Submit a batch
const batch = await client.submitTxBatch([
  { from: "af1349...", to: "2d3ade...", amount: 100, nonce: 0 },
  { from: "af1349...", to: "2d3ade...", amount: 200, nonce: 1 },
]);
console.log(batch.accepted, batch.rejected);

// Wait for confirmation
const confirmed = await client.waitForTx("a1b2c3d4...", {
  timeout: 60_000,
  interval: 500,
});
```

### Accounts

```typescript
const account = await client.getAccount("af1349b9...");
console.log(account.balance, account.nonce, account.staked_balance);

const txs = await client.getAccountTxs("af1349b9...");
console.log(txs.tx_count, txs.tx_hashes);

// Convenience methods
const balance = await client.getBalance("af1349b9...");
const nonce = await client.getNonce("af1349b9...");
```

### Validators

```typescript
const validators = await client.getValidators();
validators.validators.forEach(v =>
  console.log(v.address, v.stake, v.tier)
);
console.log(validators.total_stake);
```

### Contracts

```typescript
// Get contract info
const contract = await client.getContract("deadbeef...");
console.log(contract.bytecode_size, contract.is_wasm);

// Read-only contract call
const result = await client.callContract("deadbeef...", "get_count", {
  gasLimit: 1_000_000,
});
console.log(result.success, result.return_data);
```

### Light Client & Sync

```typescript
const snapshot = await client.getLightSnapshot();
console.log(snapshot.height, snapshot.state_root);

const syncInfo = await client.getSyncSnapshotInfo();
if (syncInfo.available) {
  const response = await client.getSyncSnapshot();
  const data = await response.arrayBuffer();
  // Save LZ4-compressed state snapshot
}
```

### Faucet

The faucet runs as a separate service (default port 3001).
Create a dedicated client or point your main client at the faucet URL:

```typescript
const faucet = new ArcClient("http://localhost:3001");

const status = await faucet.faucetStatus();
console.log(status.claim_amount); // 1000

const claim = await faucet.faucetClaim(
  "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
);
console.log(claim.tx_hash, claim.amount, claim.message);
```

### ETH JSON-RPC Compatibility

ARC Chain exposes an Ethereum-compatible JSON-RPC on `/eth` (port 9090) and port 8545.

```typescript
// Raw JSON-RPC call
const chainId = await client.ethChainId();
console.log(chainId); // "0x415243"

const blockNum = await client.ethBlockNumber();

const balance = await client.ethGetBalance(
  "0xaf1349b9f5f9a1a6a0404dea36dcc9499bcb25c9"
);

// Any method via ethRpc
const txCount = await client.ethRpc<string>(
  "eth_getBlockTransactionCountByNumber",
  ["latest"]
);
```

### Block Subscriptions

```typescript
const sub = client.onBlock(async (block) => {
  console.log(`Block #${block.header.height}: ${block.header.tx_count} txs`);
}, 1000); // poll every 1s

// Stop polling
sub.unsubscribe();
```

## Utilities

```typescript
import {
  isValidAddress,
  isValidTxHash,
  formatHash,
  formatArc,
  hexToBytes,
  bytesToHex,
  stripHexPrefix,
  addHexPrefix,
} from "@arc-chain/sdk";

// Validate addresses and tx hashes
isValidAddress("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"); // true
isValidTxHash("a1b2c3d4...64chars..."); // true

// Truncate hashes for display
formatHash("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
// "af1349b9...e41f3262"

// Format raw balance to ARC
formatArc(1_000_000); // "1.000000"

// Hex encoding
const bytes = hexToBytes("deadbeef");
const hex = bytesToHex(new Uint8Array([0xde, 0xad, 0xbe, 0xef])); // "deadbeef"
```

## Error Handling

All RPC methods throw `ArcRpcError` on HTTP errors:

```typescript
import { ArcClient, ArcRpcError } from "@arc-chain/sdk";

try {
  const block = await client.getBlock(999_999_999);
} catch (err) {
  if (err instanceof ArcRpcError) {
    console.error(err.statusCode); // 404
    console.error(err.body);       // Error message from node
  }
}
```

## Transaction Types

ARC Chain supports 21 transaction types. The `TransactionBody` type is a discriminated union:

| Type | Description |
|------|-------------|
| `Transfer` | Token transfer |
| `Settle` | Agent-to-agent settlement |
| `Swap` | Atomic token swap |
| `Escrow` | Conditional escrow |
| `Stake` | Stake/unstake tokens |
| `WasmCall` | WASM contract invocation |
| `MultiSig` | Multi-signature setup |
| `DeployContract` | Contract deployment |
| `RegisterAgent` | Agent registration |
| `JoinValidator` | Validator onboarding |
| `LeaveValidator` | Validator exit |
| `ClaimRewards` | Claim staking rewards |
| `UpdateStake` | Modify stake amount |
| `Governance` | Governance vote/proposal |
| `BridgeLock` | Cross-chain bridge lock |
| `BridgeMint` | Cross-chain bridge mint |
| `BatchSettle` | Batch agent settlements (L1 scaling) |
| `ChannelOpen` | Open state channel (L1 scaling) |
| `ChannelClose` | Close state channel (L1 scaling) |
| `ChannelDispute` | Dispute state channel (L1 scaling) |
| `ShardProof` | Shard STARK proof (L1 scaling) |

## Requirements

- Node.js >= 18 (for native `fetch`)
- TypeScript >= 5.0 (for development)

## License

MIT
