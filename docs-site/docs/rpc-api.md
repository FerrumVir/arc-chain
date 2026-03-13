---
title: "JSON-RPC API Reference"
sidebar_position: 4
slug: "/rpc-api"
---
# JSON-RPC API Reference

ARC Chain exposes two API surfaces from a single node:

1. **ARC Native API** -- RESTful HTTP endpoints on the main RPC port (default `9090`)
2. **ETH JSON-RPC** -- Ethereum-compatible JSON-RPC 2.0 on both `/eth` (port 9090) and a dedicated port (default `8545`)

Base URL: `http://localhost:9090`

---

## Health and Info

### `GET /`

Returns a version string.

```bash
curl http://localhost:9090/
```

```
ARC Chain — Agent Runtime Chain — Testnet v0.1.0
```

### `GET /health`

Node health check. Returns status, version, current block height, peer count, and uptime.

```bash
curl http://localhost:9090/health
```

```json
{
  "status": "ok",
  "version": "0.1.0",
  "height": 1542,
  "peers": 3,
  "uptime_secs": 8421
}
```

### `GET /info`

Chain information including GPU status, account count, and mempool size.

```bash
curl http://localhost:9090/info
```

```json
{
  "chain": "ARC Chain",
  "version": "0.1.0",
  "block_height": 1542,
  "account_count": 150,
  "mempool_size": 12,
  "gpu": {
    "name": "Apple M4 Pro",
    "backend": "Metal",
    "available": true
  }
}
```

### `GET /node/info`

Validator-specific information including stake amount and tier.

```bash
curl http://localhost:9090/node/info
```

```json
{
  "validator": "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
  "stake": 5000000,
  "tier": "Arc",
  "height": 1542,
  "version": "0.1.0",
  "mempool_size": 12
}
```

### `GET /stats`

Aggregate chain statistics including total transactions and index sizes.

```bash
curl http://localhost:9090/stats
```

```json
{
  "chain": "ARC Chain",
  "version": "0.1.0",
  "block_height": 1542,
  "total_accounts": 150,
  "mempool_size": 12,
  "total_transactions": 48230,
  "indexed_hashes": 48230,
  "indexed_receipts": 48230
}
```

---

## Blocks

### `GET /block/{height}`

Fetch a block by height. Returns the full block including header, transaction hashes, and block hash.

**Parameters:**
- `height` (path, required): Block number (u64)

```bash
curl http://localhost:9090/block/42
```

```json
{
  "header": {
    "height": 42,
    "timestamp": 1709654321000,
    "parent_hash": "a1b2c3...",
    "tx_root": "d4e5f6...",
    "state_root": "789abc...",
    "proof_hash": "def012...",
    "tx_count": 128,
    "producer": "af1349b9..."
  },
  "tx_hashes": ["hash1...", "hash2..."],
  "hash": "blockHash..."
}
```

**Errors:** `404` if block not found.

### `GET /blocks?from=&to=&limit=`

Paginated block listing. Returns block summaries (height, hash, parent hash, tx root, tx count, timestamp, producer).

**Query Parameters:**

| Parameter | Default | Description |
|---|---|---|
| `from` | `0` | Start height (inclusive) |
| `to` | chain tip | End height (inclusive) |
| `limit` | `20` | Max blocks to return (server caps at 100) |

```bash
curl "http://localhost:9090/blocks?from=0&to=100&limit=10"
```

```json
{
  "from": 0,
  "to": 100,
  "limit": 10,
  "count": 10,
  "blocks": [
    {
      "height": 0,
      "hash": "...",
      "parent_hash": "...",
      "tx_root": "...",
      "tx_count": 100,
      "timestamp": 0,
      "producer": "..."
    }
  ]
}
```

### `GET /block/{height}/txs?offset=&limit=`

Paginated transaction listing for a block. Returns transaction hashes and metadata. For benchmark blocks, transactions are reconstructed on-demand from deterministic parameters.

**Query Parameters:**

| Parameter | Default | Description |
|---|---|---|
| `offset` | `0` | Start index within the block |
| `limit` | `100` | Max transactions to return (server caps at 1000) |

```bash
curl "http://localhost:9090/block/42/txs?offset=0&limit=50"
```

```json
{
  "block_height": 42,
  "tx_count": 10000,
  "offset": 0,
  "limit": 50,
  "returned": 50,
  "transactions": [
    {
      "index": 0,
      "hash": "a1b2c3...",
      "from": "sender...",
      "nonce": 0,
      "tx_type": "Transfer",
      "body": {
        "type": "Transfer",
        "to": "recipient...",
        "amount": 100
      }
    }
  ]
}
```

**Errors:** `404` if block not found.

### `GET /block/{height}/proofs`

All Merkle inclusion proofs for transactions in a block.

```bash
curl http://localhost:9090/block/42/proofs
```

```json
{
  "block_height": 42,
  "block_hash": "...",
  "tx_root": "d4e5f6...",
  "proof_count": 128,
  "proofs": [
    {
      "tx_hash": "abc123...",
      "leaf": "...",
      "index": 0,
      "siblings": [
        {"hash": "sibling1...", "is_left": true},
        {"hash": "sibling2...", "is_left": false}
      ],
      "root": "d4e5f6..."
    }
  ]
}
```

---

## Accounts

### `GET /account/{address}`

Fetch account state by 64-character hex address. Returns balance, nonce, code hash, storage root, and staked balance.

```bash
curl http://localhost:9090/account/af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
```

```json
{
  "address": "af1349b9...",
  "balance": 999999000,
  "nonce": 42,
  "code_hash": "0000000000000000000000000000000000000000000000000000000000000000",
  "storage_root": "0000000000000000000000000000000000000000000000000000000000000000",
  "staked_balance": 0
}
```

**Errors:** `400` if address is not valid hex. `404` if account not found.

### `GET /account/{address}/txs`

Transaction hashes involving an account.

```bash
curl http://localhost:9090/account/af1349b9.../txs
```

```json
{
  "address": "af1349b9...",
  "tx_count": 42,
  "tx_hashes": ["hash1...", "hash2..."]
}
```

---

## Transactions

### `POST /tx/submit`

Submit a transaction to the mempool.

**Request Body:**

```json
{
  "from": "64-char hex sender address",
  "to": "64-char hex recipient address",
  "amount": 1000,
  "nonce": 0,
  "tx_type": "Transfer"
}
```

The `tx_type` field is optional (defaults to Transfer).

```bash
curl -X POST http://localhost:9090/tx/submit \
  -H "Content-Type: application/json" \
  -d '{
    "from": "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
    "to":   "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213",
    "amount": 1000,
    "nonce": 0
  }'
```

```json
{
  "tx_hash": "a1b2c3d4e5f6...",
  "status": "pending"
}
```

**Errors:** `400` if addresses are invalid hex. `409` if transaction already exists (duplicate hash).

### `POST /tx/submit_batch`

Submit multiple transactions in a single request. Each transaction is processed independently -- some may be accepted while others are rejected.

```bash
curl -X POST http://localhost:9090/tx/submit_batch \
  -H "Content-Type: application/json" \
  -d '{
    "transactions": [
      {"from": "af1349...", "to": "2d3ade...", "amount": 100, "nonce": 0},
      {"from": "af1349...", "to": "2d3ade...", "amount": 200, "nonce": 1}
    ]
  }'
```

```json
{
  "accepted": 2,
  "rejected": 0,
  "tx_hashes": ["hash1...", "hash2..."]
}
```

### `GET /tx/{hash}`

Look up a transaction receipt by its 64-character hex hash. Falls back to on-demand reconstruction for benchmark transactions.

```bash
curl http://localhost:9090/tx/a1b2c3d4...
```

```json
{
  "tx_hash": "a1b2c3d4...",
  "block_height": 42,
  "block_hash": "...",
  "index": 7,
  "success": true,
  "gas_used": 21000,
  "value_commitment": null,
  "inclusion_proof": null,
  "logs": []
}
```

**Errors:** `404` if transaction not found.

### `GET /tx/{hash}/full`

Full transaction body with type-specific fields, signature information, and receipt data. Supports all 21 transaction types with their complete body structures.

```bash
curl http://localhost:9090/tx/a1b2c3d4.../full
```

```json
{
  "tx_hash": "a1b2c3d4...",
  "tx_type": "Transfer",
  "from": "af1349...",
  "nonce": 0,
  "fee": 0,
  "gas_limit": 0,
  "body": {
    "type": "Transfer",
    "to": "2d3ade...",
    "amount": 1000,
    "amount_commitment": null
  },
  "signature": {
    "Ed25519": {
      "public_key": "...",
      "signature": "..."
    }
  },
  "block_height": 42,
  "block_hash": "...",
  "index": 7,
  "success": true,
  "gas_used": 21000
}
```

Body shapes vary by transaction type. Examples:

- **Settle**: `{"type": "Settle", "agent_id": "...", "service_hash": "...", "amount": 500, "usage_units": 10}`
- **DeployContract**: `{"type": "DeployContract", "bytecode_size": 4096, "constructor_args_size": 0, "state_rent_deposit": 1000}`
- **BatchSettle**: `{"type": "BatchSettle", "entries": 42, "total_amount": 50000}`
- **ChannelOpen**: `{"type": "ChannelOpen", "channel_id": "0x...", "counterparty": "0x...", "deposit": 10000, "timeout_blocks": 100}`
- **ShardProof**: `{"type": "ShardProof", "shard_id": 0, "block_height": 100, "tx_count": 500, "proof_size": 2048, "prev_state_root": "0x...", "post_state_root": "0x..."}`

### `GET /tx/{hash}/proof`

Merkle inclusion proof for a transaction. Contains the leaf hash, sibling hashes with direction, and the Merkle root. Can be verified client-side.

```bash
curl http://localhost:9090/tx/a1b2c3d4.../proof
```

```json
{
  "tx_hash": "a1b2c3d4...",
  "blake3_domain": "ARC-chain-tx-v1",
  "merkle_proof": {
    "leaf": "...",
    "index": 7,
    "siblings": [
      {"hash": "sibling1...", "is_left": true},
      {"hash": "sibling2...", "is_left": false}
    ],
    "root": "d4e5f6..."
  },
  "block_height": 42,
  "block_tx_root": "d4e5f6...",
  "verified": true,
  "pedersen_commitment": null
}
```

**Errors:** `404` if transaction not found.

---

## Contracts

### `GET /contract/{address}`

Get deployed contract information.

```bash
curl http://localhost:9090/contract/deadbeef...
```

```json
{
  "address": "deadbeef...",
  "bytecode_size": 4096,
  "code_hash": "abc123...",
  "is_wasm": true
}
```

**Errors:** `404` if no contract at address.

### `POST /contract/{address}/call`

Read-only contract call. Executes the contract function in a sandbox without modifying state. Storage writes are buffered but never flushed to StateDB.

**Request Body:**

```json
{
  "function": "get_count",
  "calldata": "hex-encoded-calldata",
  "from": "caller-address (optional)",
  "gas_limit": 1000000
}
```

```bash
curl -X POST http://localhost:9090/contract/deadbeef.../call \
  -H "Content-Type: application/json" \
  -d '{
    "function": "get_count",
    "gas_limit": 1000000
  }'
```

```json
{
  "success": true,
  "gas_used": 1234,
  "return_data": "0a000000",
  "logs": [],
  "events": [
    {"topic": "...", "data": "..."}
  ]
}
```

On compilation or execution error:

```json
{
  "success": false,
  "error": "compilation error: invalid WASM magic"
}
```

---

## Light Client and Sync

### `GET /light/snapshot`

Lightweight snapshot for light client bootstrapping. Returns current height, state root, account count, total supply, and latest block hash.

```bash
curl http://localhost:9090/light/snapshot
```

```json
{
  "height": 1542,
  "state_root": "789abc...",
  "account_count": 150,
  "total_supply": 1000000000,
  "latest_block_hash": "def012..."
}
```

### `GET /sync/snapshot/info`

Metadata about the available state snapshot for new-node synchronization.

```bash
curl http://localhost:9090/sync/snapshot/info
```

```json
{
  "available": true,
  "height": 1542,
  "state_root": "789abc...",
  "account_count": 150
}
```

### `GET /sync/snapshot`

Download the full state snapshot as LZ4-compressed bincode. New nodes use this to bootstrap without replaying from genesis.

```bash
curl -o snapshot.lz4 http://localhost:9090/sync/snapshot
```

Returns `Content-Type: application/octet-stream` with `Content-Disposition: attachment; filename="snapshot.lz4"`.

---

## ETH JSON-RPC Compatibility

### `POST /eth`

Ethereum-compatible JSON-RPC 2.0 endpoint. Also available on the dedicated ETH RPC port (default 8545) at the root path `/`.

**Chain ID:** `0x415243` (4,281,923 -- "ARC" in ASCII)

### Supported Methods

| Method | Description |
|---|---|
| `eth_chainId` | Returns `0x415243` |
| `eth_blockNumber` | Current block height (hex) |
| `net_version` | Network ID (string) |
| `web3_clientVersion` | Returns `ARC/v0.1.0` |
| `eth_gasPrice` | Returns `0x0` (zero-fee chain) |
| `net_listening` | Returns `true` |
| `net_peerCount` | Connected peer count (hex) |
| `eth_syncing` | Returns `false` (always synced) |
| `eth_mining` | Returns `false` |
| `eth_hashrate` | Returns `0x0` |
| `eth_accounts` | Returns `[]` |
| `eth_getBalance` | Account balance (hex wei) |
| `eth_getTransactionCount` | Account nonce (hex) |
| `eth_getCode` | Contract bytecode |
| `eth_getStorageAt` | Storage slot value |
| `eth_getBlockByNumber` | Block by height (supports `latest`, `earliest`, `pending`, hex) |
| `eth_getBlockByHash` | Block by hash (placeholder) |
| `eth_getTransactionByHash` | Transaction by hash |
| `eth_getTransactionReceipt` | Transaction receipt with logs |
| `eth_call` | Read-only contract call |
| `eth_estimateGas` | Gas estimation |
| `eth_sendRawTransaction` | Submit raw transaction |
| `eth_getLogs` | Event log query |
| `eth_getBlockTransactionCountByNumber` | TX count in a block |

### Example: Get Balance

```bash
curl -X POST http://localhost:8545 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "method": "eth_getBalance",
    "params": ["0xaf1349b9f5f9a1a6a0404dea36dcc9499bcb25c9", "latest"],
    "id": 1
  }'
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": "0x3b9aca00"
}
```

### Example: Get Block

```bash
curl -X POST http://localhost:8545 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "method": "eth_getBlockByNumber",
    "params": ["latest", false],
    "id": 1
  }'
```

### Foundry / Hardhat Configuration

ARC Chain's ETH RPC is compatible with Foundry and Hardhat. See [Smart Contracts](./smart-contracts.md) for the `foundry.toml` configuration.

---

## Error Responses

### HTTP Errors

| Code | Meaning |
|---|---|
| 200 | Success |
| 400 | Bad request (invalid hex address, malformed parameters) |
| 404 | Not found (block, transaction, account, or contract does not exist) |
| 409 | Conflict (duplicate transaction in mempool) |
| 500 | Internal server error |

### ETH JSON-RPC Errors

ETH errors follow the JSON-RPC 2.0 error format:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32602,
    "message": "Missing address parameter"
  }
}
```

| Code | Meaning |
|---|---|
| -32601 | Method not found |
| -32602 | Invalid params |

---

## Body Size Limit

The RPC server accepts request bodies up to **256 MB** (configured via `DefaultBodyLimit`). This accommodates large contract deployments and batch submissions.

## CORS

All endpoints have permissive CORS enabled (`CorsLayer::permissive()`), allowing browser-based applications to interact with the RPC directly.
