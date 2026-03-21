---
title: RPC API
sidebar_position: 4
id: rpc-api
---

# RPC API Reference

ARC Chain exposes 20+ HTTP endpoints and ETH JSON-RPC compatibility. The default RPC server runs on port 9090.

## HTTP Endpoints

### Health and Status

#### `GET /health`

Returns node health information.

```bash
curl http://localhost:9090/health
```

```json
{
  "status": "ok",
  "version": "0.3.0",
  "block_height": 12345,
  "peer_count": 4,
  "consensus": "active"
}
```

#### `GET /stats`

Returns live chain statistics.

```bash
curl http://localhost:9090/stats
```

```json
{
  "tps": 27000,
  "block_height": 12345,
  "total_transactions": 5000000,
  "validator_count": 4,
  "uptime_seconds": 86400
}
```

### Blocks

#### `GET /block/latest`

Returns the most recently finalized block.

```bash
curl http://localhost:9090/block/latest
```

#### `GET /block/{height}`

Returns a block at a specific height.

```bash
curl http://localhost:9090/block/100
```

#### `GET /blocks?from=&to=&limit=`

Returns a paginated list of blocks.

```bash
# Get blocks 100 through 110
curl "http://localhost:9090/blocks?from=100&to=110"

# Get the latest 20 blocks
curl "http://localhost:9090/blocks?limit=20"
```

### Accounts

#### `GET /account/{address}`

Returns the account state including balance, nonce, and contract data.

```bash
curl http://localhost:9090/account/abc123...def
```

```json
{
  "address": "abc123...def",
  "balance": 1000000,
  "nonce": 42,
  "is_contract": false,
  "storage_root": "0x..."
}
```

#### `GET /account/{address}/txs`

Returns the transaction history for an account.

```bash
curl http://localhost:9090/account/abc123...def/txs
```

### Transactions

#### `POST /tx/submit`

Submits a signed transaction.

```bash
curl -X POST http://localhost:9090/tx/submit \
  -H "Content-Type: application/json" \
  -d '{
    "tx_type": "Transfer",
    "from": "abc123...def",
    "to": "fed321...cba",
    "amount": 1000,
    "nonce": 42,
    "signature": "..."
  }'
```

```json
{
  "tx_hash": "0x...",
  "status": "accepted"
}
```

#### `POST /tx/submit_batch`

Submits multiple signed transactions in a single request.

```bash
curl -X POST http://localhost:9090/tx/submit_batch \
  -H "Content-Type: application/json" \
  -d '{"transactions": [...]}'
```

#### `GET /tx/{hash}`

Returns a transaction and its receipt.

```bash
curl http://localhost:9090/tx/0xabcdef...
```

```json
{
  "hash": "0xabcdef...",
  "tx_type": "Transfer",
  "from": "abc123...def",
  "to": "fed321...cba",
  "amount": 1000,
  "block_height": 12345,
  "status": "success",
  "gas_used": 21000
}
```

#### `GET /tx/{hash}/proof`

Returns a Merkle inclusion proof for a transaction.

```bash
curl http://localhost:9090/tx/0xabcdef.../proof
```

### Validators and Agents

#### `GET /validators`

Returns the current validator set with stake amounts and roles.

```bash
curl http://localhost:9090/validators
```

```json
{
  "validators": [
    {
      "address": "abc123...def",
      "stake": 5000000,
      "role": "Proposer",
      "tier": "Arc"
    }
  ]
}
```

#### `GET /agents`

Returns all registered AI agents.

```bash
curl http://localhost:9090/agents
```

```json
{
  "agents": [
    {
      "address": "abc123...def",
      "name": "sentiment-agent",
      "model_id": "0x...",
      "capabilities": "inference"
    }
  ]
}
```

## ETH JSON-RPC Compatibility

ARC Chain supports a subset of the Ethereum JSON-RPC specification for compatibility with existing tools (MetaMask, ethers.js, web3.py).

All ETH JSON-RPC calls use `POST /` with a JSON-RPC 2.0 body.

### `eth_blockNumber`

Returns the current block height.

```bash
curl -X POST http://localhost:9090/ \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": "0x3039"
}
```

### `eth_getBalance`

Returns the balance of an account.

```bash
curl -X POST http://localhost:9090/ \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_getBalance","params":["0xabc...","latest"],"id":1}'
```

### `eth_call`

Executes a read-only contract call without creating a transaction.

```bash
curl -X POST http://localhost:9090/ \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc":"2.0",
    "method":"eth_call",
    "params":[{"to":"0xcontract...","data":"0x..."},"latest"],
    "id":1
  }'
```

### `eth_estimateGas`

Estimates the gas required for a transaction.

```bash
curl -X POST http://localhost:9090/ \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc":"2.0",
    "method":"eth_estimateGas",
    "params":[{"from":"0x...","to":"0x...","value":"0x3e8"}],
    "id":1
  }'
```

### `eth_getLogs`

Returns logs matching a filter.

```bash
curl -X POST http://localhost:9090/ \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc":"2.0",
    "method":"eth_getLogs",
    "params":[{"fromBlock":"0x1","toBlock":"latest","address":"0x..."}],
    "id":1
  }'
```

## Endpoint Summary

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Node health |
| GET | `/stats` | Live TPS, height, total transactions |
| GET | `/block/latest` | Latest block |
| GET | `/block/{height}` | Block by height |
| GET | `/blocks?from=&to=&limit=` | Paginated block list |
| GET | `/account/{address}` | Account state |
| GET | `/account/{address}/txs` | Transaction history |
| POST | `/tx/submit` | Submit signed transaction |
| POST | `/tx/submit_batch` | Batch submission |
| GET | `/tx/{hash}` | Transaction + receipt |
| GET | `/tx/{hash}/proof` | Merkle inclusion proof |
| GET | `/validators` | Current validator set |
| GET | `/agents` | Registered AI agents |
| POST | `/` | ETH JSON-RPC (eth_blockNumber, eth_getBalance, eth_call, eth_estimateGas, eth_getLogs) |
