---
title: Testnet
sidebar_position: 6
id: testnet
---

# Joining the Testnet

ARC Chain's testnet allows you to run a validator node, deploy agents, and test transactions in a real multi-node environment.

## Quick Join (One-Click)

```bash
curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-node.sh | bash
```

This installs dependencies, builds the node, generates a keypair, and starts the node pointing at the testnet seed nodes.

## Desktop App

The ARC Node desktop app provides a graphical interface for joining the testnet. Download it from the [GitHub releases page](https://github.com/FerrumVir/arc-chain/releases) and follow the setup wizard.

## Manual Join (CLI)

```bash
# Clone and build
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain && cargo build --release

# Start with testnet seeds
cargo run --release -p arc-node -- \
    --seeds-file testnet/seeds.txt \
    --validator-seed "$(openssl rand -hex 16)"
```

## Hardware Requirements

| Role | Hardware | Min Stake |
|------|----------|-----------|
| **Observer** | Raspberry Pi 4 / any laptop | 50,000 ARC |
| **Verifier** | Mac Mini / desktop (4+ cores, 8 GB RAM) | 500,000 ARC |
| **Proposer** | GPU server (16+ cores, 32 GB RAM) | 5,000,000 ARC |

The node runs on Linux (x86_64, ARM64) and macOS (Intel, Apple Silicon).

## Create Your Own Testnet

For local development or testing, you can spin up a private multi-validator network:

```bash
# Generate a 4-validator testnet config
bash scripts/create-testnet.sh 4

# Start the seed node
cargo run --release -p arc-node -- --config testnet/validator-0.toml

# On other machines (or terminals), start additional validators
cargo run --release -p arc-node -- --config testnet/validator-1.toml --peers SEED_IP:9091
cargo run --release -p arc-node -- --config testnet/validator-2.toml --peers SEED_IP:9091
cargo run --release -p arc-node -- --config testnet/validator-3.toml --peers SEED_IP:9091
```

## Testnet Parameters

| Parameter | Value |
|-----------|-------|
| Chain ID | `0x415243` ("ARC") |
| Block time | ~400ms target |
| Consensus | DAG (Mysticeti-inspired), 2-round finality |
| Min stake (Observer) | 50,000 ARC |
| Min stake (Verifier) | 500,000 ARC |
| Min stake (Proposer) | 5,000,000 ARC |

## Faucet

The testnet faucet distributes test ARC tokens for development. It is available at the `/faucet` endpoint on any testnet node:

```bash
curl -X POST http://localhost:9090/faucet \
  -H "Content-Type: application/json" \
  -d '{"address": "your-address-here"}'
```

You can also run the standalone faucet server from the `faucet/` directory:

```bash
cargo run -p arc-faucet
```

## Monitoring

Monitor your testnet nodes in real time:

```bash
bash scripts/monitor-testnet.sh localhost:9090 localhost:9190 localhost:9290 localhost:9390
```

Or query individual nodes:

```bash
# Health check
curl http://localhost:9090/health

# Live stats (TPS, height, peers)
curl http://localhost:9090/stats

# Validator set
curl http://localhost:9090/validators
```

## Running the Explorer

Start the block explorer pointed at your testnet:

```bash
cd explorer && npm install && npm run dev
# Explorer at http://localhost:3100
```

## Testnet Milestones

| Day | Target | Verification |
|-----|--------|-------------|
| 1 | 4+ nodes connected, producing blocks | `curl localhost:9090/health` shows peers > 0 |
| 2 | Sustained 10K+ TPS across network | `curl localhost:9090/stats` shows tps > 10000 |
| 3 | AI agent deployed and running inference | Run sentiment-agent against testnet RPC |
| 7 | No crashes, no forks, no stalls for 7 days | Monitor dashboard shows continuous operation |
| 14 | 10+ community nodes joined | `/validators` endpoint shows 10+ validators |
| 30 | Bridge tested (ETH testnet to ARC testnet) | Bridge relayer processes test locks |
