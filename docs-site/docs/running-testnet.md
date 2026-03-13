---
title: "Running a Testnet"
sidebar_position: 2
slug: "/running-testnet"
---
# Running a Testnet

This guide covers single-node, multi-validator, and production-like testnet configurations.

---

## Single Node

The simplest setup. Run one validator node with a genesis state containing 100 pre-funded accounts.

```bash
./target/release/arc-node
```

The node starts with:
- **ARC RPC** on `http://0.0.0.0:9090`
- **ETH JSON-RPC** on `http://0.0.0.0:8545`
- **P2P (QUIC)** on port `9091`
- **Stake**: 5,000,000 ARC (Arc tier)
- **Data directory**: `./arc-data`

### Custom Configuration

```bash
./target/release/arc-node \
  --rpc 0.0.0.0:9090 \
  --p2p-port 9091 \
  --stake 5000000 \
  --validator-seed "my-validator" \
  --data-dir ./arc-data \
  --eth-rpc-port 8545
```

### All CLI Flags

| Flag | Default | Description |
|---|---|---|
| `--rpc` | `0.0.0.0:9090` | RPC listen address |
| `--p2p-port` | `9091` | QUIC P2P listen port |
| `--stake` | `5000000` | Validator stake in ARC |
| `--data-dir` | `./arc-data` | WAL/snapshot storage directory |
| `--peers` | (none) | Bootstrap peer addresses (comma-separated `host:port`) |
| `--min-stake` | `500000` | Minimum stake required |
| `--validator-seed` | `arc-validator-0` | Deterministic validator key seed |
| `--benchmark` | `false` | Enable continuous test TX generation |
| `--bench-batch` | `500` | Transactions per benchmark batch |
| `--bench-interval` | `200` | Milliseconds between benchmark batches |
| `--bench-sender-start` | `0` | First sender index (0-49) for multi-node benchmarks |
| `--bench-sender-count` | `50` | Number of senders this node owns |
| `--bench-sign-threads` | `4` | Signing thread count in benchmark mode |
| `--bench-rayon-threads` | `6` | Rayon threads for batch verification |
| `--proposer-mode` | `false` | Enable proposer execution pipeline |
| `--eth-rpc-port` | `8545` | ETH JSON-RPC port (0 to disable) |

---

## Multi-Validator Network

### Two-Node Setup

Start two validators that peer with each other:

```bash
# Terminal 1: Node 0
./target/release/arc-node \
  --rpc 0.0.0.0:9090 \
  --p2p-port 9091 \
  --eth-rpc-port 8545 \
  --validator-seed node-0 \
  --data-dir ./arc-data-0

# Terminal 2: Node 1
./target/release/arc-node \
  --rpc 0.0.0.0:9092 \
  --p2p-port 9093 \
  --eth-rpc-port 8546 \
  --validator-seed node-1 \
  --data-dir ./arc-data-1 \
  --peers 127.0.0.1:9091
```

Node 1 connects to Node 0 via the `--peers` flag. After connecting, PEX (Peer Exchange) ensures both nodes know about each other.

### Four-Node Setup

```bash
# Node 0 (bootstrap node)
./target/release/arc-node \
  --rpc 0.0.0.0:9090 --p2p-port 9091 --eth-rpc-port 8545 \
  --validator-seed node-0 --data-dir ./arc-data-0 &

# Node 1
./target/release/arc-node \
  --rpc 0.0.0.0:9092 --p2p-port 9093 --eth-rpc-port 8546 \
  --validator-seed node-1 --data-dir ./arc-data-1 \
  --peers 127.0.0.1:9091 &

# Node 2
./target/release/arc-node \
  --rpc 0.0.0.0:9094 --p2p-port 9095 --eth-rpc-port 8547 \
  --validator-seed node-2 --data-dir ./arc-data-2 \
  --peers 127.0.0.1:9091 &

# Node 3
./target/release/arc-node \
  --rpc 0.0.0.0:9096 --p2p-port 9097 --eth-rpc-port 8548 \
  --validator-seed node-3 --data-dir ./arc-data-3 \
  --peers 127.0.0.1:9091 &
```

All nodes only need to know one bootstrap peer -- PEX propagates the full peer table within 60 seconds.

---

## Staking and Validator Registration

### Staking Tiers

| Tier | Minimum Stake | Capabilities |
|---|---|---|
| Spark | 500,000 ARC | Observer / vote only (cannot produce blocks) |
| Arc | 5,000,000 ARC | Block producer + voter |
| Core | 50,000,000 ARC | Priority producer + governance |

### Join as a Validator

To join the validator set programmatically, submit a `JoinValidator` transaction (type `0x0a`, gas 30,000):

```python
from arc_sdk import ArcClient, KeyPair, TransactionBuilder

client = ArcClient("http://localhost:9090")
kp = KeyPair.generate()

# JoinValidator requires pubkey and initial_stake
tx = {
    "from": kp.address(),
    "tx_type": "JoinValidator",
    "nonce": 0,
    "body": {
        "pubkey": kp.public_key_hex(),
        "initial_stake": 5_000_000,
    }
}
```

### Update Stake

```python
# UpdateStake (type 0x0d, gas 25,000) -- adjust validator stake
tx = {
    "from": kp.address(),
    "tx_type": "UpdateStake",
    "nonce": 1,
    "body": {"new_stake": 10_000_000}
}
```

### Leave Validator Set

```python
# LeaveValidator (type 0x0b, gas 25,000) -- exit validator set
tx = {
    "from": kp.address(),
    "tx_type": "LeaveValidator",
    "nonce": 2,
}
```

### Claim Rewards

```python
# ClaimRewards (type 0x0c, gas 25,000)
tx = {
    "from": kp.address(),
    "tx_type": "ClaimRewards",
    "nonce": 3,
}
```

---

## Monitoring

### Health Endpoint

```bash
# Check each node's health
for port in 9090 9092 9094 9096; do
  echo "Node on port $port:"
  curl -s http://localhost:$port/health | python3 -m json.tool
  echo
done
```

### Stats Endpoint

```bash
curl http://localhost:9090/stats
```

Response includes block height, total accounts, mempool size, total transactions, and index sizes.

### Node Info

```bash
curl http://localhost:9090/node/info
```

Shows validator address, stake amount, tier, and mempool size.

### Comparing Node Heights

A healthy multi-node network should have all nodes at approximately the same block height:

```bash
for port in 9090 9092 9094 9096; do
  height=$(curl -s http://localhost:$port/health | python3 -c "import sys,json;print(json.load(sys.stdin)['height'])")
  echo "Port $port: height $height"
done
```

---

## Benchmark Mode

Enable benchmark mode to generate continuous test transactions:

```bash
./target/release/arc-node --benchmark \
  --bench-batch 500 \
  --bench-interval 200
```

This generates 500 transfer transactions every 200ms, signed with Ed25519, and submits them to the local mempool.

### Multi-Node Benchmark

For multi-node benchmarks, partition senders across nodes to avoid nonce conflicts:

```bash
# Node 0: senders 0-24
./target/release/arc-node --benchmark \
  --bench-sender-start 0 --bench-sender-count 25 \
  --validator-seed node-0 --data-dir ./arc-data-0 &

# Node 1: senders 25-49
./target/release/arc-node --benchmark \
  --bench-sender-start 25 --bench-sender-count 25 \
  --validator-seed node-1 --data-dir ./arc-data-1 \
  --peers 127.0.0.1:9091 &
```

### Dedicated Multi-Node Benchmark Binary

For precise TPS measurement, use the dedicated `arc-bench-multinode` binary:

```bash
cargo run --release --bin arc-bench-multinode -- \
  --txs 100000 \
  --batch 1000 \
  --nodes 2 \
  --senders-per-node 50 \
  --warmup-blocks 3 \
  --timeout-secs 300
```

This starts N real nodes in-process connected via QUIC, injects pre-signed transactions, and reports committed TPS.

---

## Block Explorer

Start the Vite + React block explorer:

```bash
cd explorer
npm install
npm run dev
```

The explorer runs at `http://localhost:5173` and connects to `http://localhost:9090` by default. Features:

- Block listing with pagination
- Transaction detail view
- Account balance lookup
- Client-side Merkle proof verification
- Faucet page (links to the faucet service)

---

## Faucet

Start the testnet faucet for distributing test tokens:

```bash
cd faucet
npm install
npm run dev
```

The faucet runs at `http://localhost:5174`. Request tokens by entering a 64-character hex address.

Programmatic access:

```bash
curl http://localhost:9090/faucet/<your-64-hex-address>
```

---

## State Sync (New Node Bootstrap)

New nodes can bootstrap without replaying from genesis by downloading a state snapshot:

```bash
# Check snapshot availability
curl http://localhost:9090/sync/snapshot/info

# Download snapshot (LZ4-compressed bincode)
curl -o snapshot.lz4 http://localhost:9090/sync/snapshot
```

The snapshot contains the full state tree (all accounts, balances, nonces, contract storage) at a specific block height.

## Light Client

Light clients can bootstrap with minimal data:

```bash
curl http://localhost:9090/light/snapshot
```

Returns the current height, state root, account count, total supply, and latest block hash -- enough to start verifying Merkle proofs without downloading full state.
