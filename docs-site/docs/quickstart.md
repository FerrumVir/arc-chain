---
title: Quickstart
sidebar_position: 2
id: quickstart
---

# Quickstart

Join the ARC Chain network in under five minutes.

## One-Click Install (recommended)

```bash
curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-node.sh | bash
```

This script will:
1. Install the Rust toolchain (if not already present)
2. Clone the `arc-chain` repository
3. Build the node in release mode
4. Generate a validator keypair
5. Start the node as a background service (systemd on Linux, launchd on macOS)

Works on Linux and macOS, Intel and ARM.

## Desktop App

The ARC Node desktop app provides a graphical interface for running a node, viewing chain status, and managing your validator identity. Downloads are available on the [GitHub releases page](https://github.com/FerrumVir/arc-chain/releases).

## Build from Source

### Prerequisites

- **Rust 1.85+** (edition 2024)
- **Node.js 22+** (only needed for the block explorer)

### Clone and build

```bash
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain
cargo build --release
```

### Run tests

```bash
cargo test --workspace --lib    # 1,054 tests
```

## Running Your First Node

Start a node with default settings:

```bash
cargo run --release -p arc-node
```

The RPC server starts at `http://localhost:9090`. Verify it is running:

```bash
curl http://localhost:9090/health
```

You should see a JSON response with the node status, peer count, and current block height.

### Check chain stats

```bash
curl http://localhost:9090/stats
```

Returns live TPS, block height, and total transaction count.

### Submit a transaction

Use the CLI to generate a keypair and submit a transfer:

```bash
# Generate a keypair
cargo run --release -p arc-cli -- keygen

# Submit a transfer (replace addresses with real ones)
cargo run --release -p arc-cli -- transfer \
    --from <sender-address> \
    --to <receiver-address> \
    --amount 1000
```

## Running the Block Explorer

```bash
cd explorer && npm install && npm run dev
```

The explorer runs at `http://localhost:3100` and connects to the local node at port 9090.

## Docker Compose

Run both the node and explorer with Docker:

```bash
docker compose up -d --build
# Node: http://localhost:9090
# Explorer: http://localhost:3100
```

## Bare Metal Deployment (Ubuntu/Debian)

```bash
git clone https://github.com/FerrumVir/arc-chain.git /opt/arc-chain
cd /opt/arc-chain && bash deploy.sh
```

This creates systemd services for `arc-node` and `arc-explorer` with automatic restart.

## Next Steps

- [Architecture](./architecture.md) -- understand how ARC Chain works under the hood
- [Testnet](./testnet.md) -- join the public testnet
- [RPC API](./rpc-api.md) -- explore all available endpoints
