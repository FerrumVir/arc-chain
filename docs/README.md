# ARC Chain Developer Documentation

ARC Chain is an AI-native Layer 1 blockchain purpose-built for autonomous agent economies. It provides 21 native transaction types, STARK zero-knowledge proofs (Stwo/Circle STARK over M31), GPU-accelerated Ed25519 signature verification, DAG-based consensus with VRF proposer selection, and Block-STM parallel execution -- all in a single, integrated Rust codebase.

**Codebase:** ~70,600 lines of Rust across 10 crates, with 1,028+ tests passing.

---

## Getting Started

- [**Quickstart**](./quickstart.md) -- Build, run a node, and submit your first transaction in 5 minutes

## Architecture

- [**Architecture Deep Dive**](./architecture.md) -- Consensus, execution, state, networking, and cryptography layers explained with data flow

## API Reference

- [**JSON-RPC API**](./rpc-api.md) -- Complete HTTP + Ethereum-compatible JSON-RPC endpoint reference with curl examples
- [**Python SDK**](./sdk-python.md) -- Full-featured Python client with ABI encoding, Ed25519 signing, and typed RPC methods
- [**TypeScript SDK**](./sdk-typescript.md) -- TypeScript/Node.js client with identical API surface and native `fetch`

## Smart Contracts

- [**Smart Contract Development**](./smart-contracts.md) -- Solidity token standards (ARC-20/721/1155), UUPS proxy, compilation, and deployment

## Operations

- [**Running a Testnet**](./running-testnet.md) -- Single node, multi-validator, staking, explorer, and faucet setup
- [**Benchmarking**](./benchmarking.md) -- Performance measurement: TPS benchmarks, GPU verification, STARK proving, propose-verify pipeline

---

## Crate Map

| Crate | Purpose |
|---|---|
| `arc-types` | Transaction types, gas costs, account model, block/receipt structures |
| `arc-crypto` | BLAKE3 hashing, Ed25519/secp256k1 signatures, Merkle trees, Poseidon, VRF, Stwo STARK prover |
| `arc-state` | Jellyfish Merkle Tree (JMT) state storage, WAL persistence, Block-STM parallel execution |
| `arc-mempool` | Transaction mempool with deduplication and ordering |
| `arc-consensus` | DAG-based consensus, VRF proposer selection, staking tiers, cross-shard coordination |
| `arc-net` | QUIC transport, Reed-Solomon FEC, PEX peer discovery, shred protocol |
| `arc-vm` | WASM smart contract runtime with gas metering and storage I/O |
| `arc-gpu` | GPU-accelerated Ed25519 verification (Metal MSL + WGSL fallback), batch commitment |
| `arc-node` | Node binary: RPC server, consensus loop, block pipeline, benchmark mode |
| `arc-bench` | Benchmark suite: single-node, multi-node, propose-verify, signed, soak tests |

## Transaction Types

ARC Chain defines 21 native transaction types (see [Architecture](./architecture.md) for details):

| Type | Discriminant | Gas Cost | Purpose |
|---|---|---|---|
| Transfer | `0x01` | 21,000 | Simple value transfer |
| Settle | `0x02` | 25,000 | Agent-to-agent service settlement |
| Swap | `0x03` | 30,000 | Atomic asset swap |
| Escrow | `0x04` | 35,000 | Escrow creation/release |
| Stake | `0x05` | 25,000 | Stake/unstake tokens |
| WasmCall | `0x06` | 21,000 + execution | Smart contract call |
| MultiSig | `0x07` | 35,000 | Multi-signature authorization |
| DeployContract | `0x08` | 53,000 | Deploy WASM contract |
| RegisterAgent | `0x09` | 30,000 | Register an AI agent on-chain |
| JoinValidator | `0x0a` | 30,000 | Join the validator set |
| LeaveValidator | `0x0b` | 25,000 | Leave the validator set |
| ClaimRewards | `0x0c` | 25,000 | Claim staking rewards |
| UpdateStake | `0x0d` | 25,000 | Adjust validator stake |
| Governance | `0x0e` | 50,000 | Execute governance proposal |
| BridgeLock | `0x0f` | 50,000 | Lock tokens for cross-chain bridge |
| BridgeMint | `0x10` | 50,000 | Mint bridged tokens |
| BatchSettle | `0x11` | 30,000 | Batch bilateral netting (1000:1 compression) |
| ChannelOpen | `0x12` | 40,000 | Open a state channel |
| ChannelClose | `0x13` | 35,000 | Close a state channel (mutual) |
| ChannelDispute | `0x14` | 50,000 | Dispute a state channel |
| ShardProof | `0x15` | 60,000 | Submit shard STARK proof |

## Additional Resources

- [Block Explorer](../explorer/) -- Vite + React explorer with client-side Merkle verification
- [Testnet Faucet](../faucet/) -- Request test tokens for development
- [Gap Tracker](../GAP_TRACKER.md) -- Feature implementation status and audit results
