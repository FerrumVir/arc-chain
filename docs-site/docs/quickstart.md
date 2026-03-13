---
title: "Quickstart"
sidebar_position: 1
slug: "/quickstart"
---
# Quickstart

Get ARC Chain running locally and submit your first transaction in under 5 minutes.

---

## Prerequisites

- **Rust** (nightly recommended): [rustup.rs](https://rustup.rs/)
- **Node.js 18+** (for the block explorer)
- **Python 3.10+** (for the Python SDK)
- **solc 0.8.24+** (optional, for Solidity contract compilation)

## 1. Clone and Build

```bash
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain
cargo build --release
```

This compiles all 10 crates and produces binaries in `target/release/`:

| Binary | Purpose |
|---|---|
| `arc-node` | Full blockchain node (RPC + P2P + consensus) |
| `arc-bench` | Throughput benchmark suite |
| `arc-bench-multinode` | Real multi-node TPS benchmark |
| `arc-bench-propose-verify` | Propose-verify pipeline benchmark |

## 2. Run a Local Node

Start a single validator node with default settings:

```bash
./target/release/arc-node
```

This starts:
- **ARC RPC server** on `http://0.0.0.0:9090`
- **ETH JSON-RPC** on `http://0.0.0.0:8545` (MetaMask/Foundry compatible)
- **P2P transport** on QUIC port `9091`

The genesis state includes 100 pre-funded accounts for testing.

### Common CLI Flags

```bash
./target/release/arc-node \
  --rpc 0.0.0.0:9090 \
  --p2p-port 9091 \
  --stake 5000000 \
  --data-dir ./arc-data \
  --eth-rpc-port 8545 \
  --validator-seed my-node \
  --peers 127.0.0.1:9091
```

| Flag | Default | Description |
|---|---|---|
| `--rpc` | `0.0.0.0:9090` | RPC listen address |
| `--p2p-port` | `9091` | QUIC P2P listen port |
| `--stake` | `5000000` | Validator stake in ARC |
| `--data-dir` | `./arc-data` | WAL/snapshot directory |
| `--eth-rpc-port` | `8545` | ETH JSON-RPC port (0 to disable) |
| `--validator-seed` | `arc-validator-0` | Unique seed for validator identity |
| `--peers` | (none) | Bootstrap peers (comma-separated `host:port`) |
| `--min-stake` | `500000` | Minimum stake to run this node |
| `--benchmark` | `false` | Enable continuous test TX generation |
| `--proposer-mode` | `false` | Enable proposer execution pipeline |

## 3. Verify the Node is Running

```bash
curl http://localhost:9090/health
```

Expected response:

```json
{
  "status": "ok",
  "version": "0.1.0",
  "height": 1,
  "peers": 0,
  "uptime_secs": 5
}
```

## 4. Get Chain Info

```bash
curl http://localhost:9090/info
```

```json
{
  "chain": "ARC Chain",
  "version": "0.1.0",
  "block_height": 1,
  "account_count": 100,
  "mempool_size": 0,
  "gpu": {
    "name": "Apple M4 Pro",
    "backend": "Metal",
    "available": true
  }
}
```

## 5. Submit a Transaction

Send 1,000 ARC between two accounts via the native RPC:

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

Response:

```json
{
  "tx_hash": "a1b2c3d4...",
  "status": "pending"
}
```

### Submit via ETH JSON-RPC

ARC Chain exposes an Ethereum-compatible JSON-RPC on port 8545. Query the block number:

```bash
curl -X POST http://localhost:8545 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "method": "eth_blockNumber",
    "params": [],
    "id": 1
  }'
```

## 6. Look Up a Transaction

```bash
curl http://localhost:9090/tx/<TX_HASH>
```

Returns the transaction receipt including block height, success status, and gas used.

For the full transaction body with type-specific fields:

```bash
curl http://localhost:9090/tx/<TX_HASH>/full
```

## 7. Submit a Batch

```bash
curl -X POST http://localhost:9090/tx/submit_batch \
  -H "Content-Type: application/json" \
  -d '{
    "transactions": [
      {"from": "aa...", "to": "bb...", "amount": 100, "nonce": 0},
      {"from": "aa...", "to": "cc...", "amount": 200, "nonce": 1}
    ]
  }'
```

Response includes `accepted`, `rejected` counts and `tx_hashes`.

## 8. Run a Multi-Validator Network

Start two nodes that peer with each other:

```bash
# Node 1
./target/release/arc-node \
  --rpc 0.0.0.0:9090 \
  --p2p-port 9091 \
  --validator-seed node-0 \
  --data-dir ./arc-data-0 &

# Node 2
./target/release/arc-node \
  --rpc 0.0.0.0:9092 \
  --p2p-port 9093 \
  --eth-rpc-port 8546 \
  --validator-seed node-1 \
  --data-dir ./arc-data-1 \
  --peers 127.0.0.1:9091 &
```

Both nodes discover each other via PEX (Peer Exchange) and participate in DAG consensus.

## 9. Run the Block Explorer

```bash
cd explorer
npm install
npm run dev
```

The explorer launches at `http://localhost:5173` and connects to the local node RPC.

## 10. Run the Test Suite

```bash
# Full workspace (1,000+ tests)
cargo test --workspace

# Include STARK prover tests (+200 STARK tests)
cargo test -p arc-crypto --features stwo-prover
```

## Next Steps

- [Architecture Deep Dive](./architecture.md) -- Understand consensus, execution, state, and cryptography layers
- [RPC API Reference](./rpc-api.md) -- Complete endpoint documentation
- [Smart Contract Development](./smart-contracts.md) -- Deploy Solidity contracts
- [Python SDK](./sdk-python.md) / [TypeScript SDK](./sdk-typescript.md) -- Build applications
- [Benchmarking](./benchmarking.md) -- Measure throughput performance
- [Running a Testnet](./running-testnet.md) -- Multi-node setup with staking and monitoring
