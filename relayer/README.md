# ARC Chain Bridge Relayer

A service that relays bridge transactions between Ethereum and ARC Chain.

## Overview

The relayer watches for bridge events on both chains and submits corresponding
transactions to complete cross-chain transfers:

- **ETH to ARC**: Monitors `Lock` events on ArcBridge.sol, waits for 12 block
  confirmations, then submits BridgeMint (0x10) transactions on ARC Chain.
- **ARC to ETH**: Monitors BridgeLock (0x0f) transactions on ARC Chain, generates
  Merkle proofs from ARC Chain state, then calls `unlock()` on ArcBridge.sol.

## Configuration

Create a `relayer.toml` file:

```toml
eth_rpc_url = "https://mainnet.infura.io/v3/YOUR_KEY"
arc_rpc_url = "http://localhost:9090"
bridge_contract = "0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499"
relayer_private_key = "0x<64-hex-chars>"
eth_private_key = "0x<64-hex-chars>"
confirmations = 12
poll_interval_secs = 15
db_path = "relayer.db"
```

## Build

```bash
cargo build -p arc-relayer --release
```

## Run

```bash
RUST_LOG=arc_relayer=info ./target/release/arc-relayer --config relayer.toml
```

## Architecture

| File               | Purpose                                      |
|--------------------|----------------------------------------------|
| `src/main.rs`      | Entry point and main relay loop               |
| `src/config.rs`    | Configuration loading and validation          |
| `src/eth_watcher.rs` | Ethereum event polling and log parsing      |
| `src/arc_submitter.rs` | ARC Chain TX submission and ETH unlocking |
