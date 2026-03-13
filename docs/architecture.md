# Architecture Deep Dive

ARC Chain is structured as a five-layer blockchain stack, implemented across 10 Rust crates totaling ~70,600 lines of code. Each layer is a distinct crate with clean dependency boundaries.

---

## Layer Overview

```
                    ┌──────────────────────────────────┐
                    │          Applications             │
                    │   SDKs (Python, TypeScript)       │
                    │   Block Explorer, Faucet          │
                    └──────────────┬───────────────────┘
                                   │
                    ┌──────────────┴───────────────────┐
                    │         RPC Layer (arc-node)      │
                    │   REST API + ETH JSON-RPC         │
                    │   axum + tower-http               │
                    └──────────────┬───────────────────┘
                                   │
          ┌────────────────────────┼────────────────────────┐
          │                        │                        │
┌─────────┴─────────┐  ┌──────────┴──────────┐  ┌─────────┴─────────┐
│   Consensus Layer  │  │  Execution Layer    │  │   Network Layer   │
│   (arc-consensus)  │  │  (arc-vm, arc-state)│  │   (arc-net)       │
│                    │  │                     │  │                    │
│ DAG-based consensus│  │ WASM VM runtime     │  │ QUIC transport    │
│ VRF proposer       │  │ Block-STM parallel  │  │ Reed-Solomon FEC  │
│ Staking tiers      │  │ JMT state tree      │  │ PEX discovery     │
│ Cross-shard coord  │  │ WAL persistence     │  │ Shred protocol    │
└─────────┬─────────┘  └──────────┬──────────┘  └─────────┬─────────┘
          │                        │                        │
          └────────────────────────┼────────────────────────┘
                                   │
                    ┌──────────────┴───────────────────┐
                    │     Cryptography Layer            │
                    │     (arc-crypto, arc-gpu)         │
                    │                                   │
                    │  BLAKE3 hashing, Ed25519/secp256k1│
                    │  Stwo STARK (Circle STARK, M31)   │
                    │  GPU Ed25519 (Metal + WGSL)       │
                    │  BLS threshold, Poseidon, VRF     │
                    │  Merkle trees + inclusion proofs  │
                    └──────────────────────────────────┘
```

## Crate Dependency Graph

```
arc-node (Axum RPC server, block production, consensus loop)
├── arc-state (StateDB, JMT, WAL, snapshots, Block-STM)
│   ├── arc-crypto (BLAKE3, Ed25519, Secp256k1, BLS, Poseidon, VRF, Merkle)
│   ├── arc-types (Transaction, Block, Account, TxBody, Hash256, gas costs)
│   └── arc-vm (WASM runtime, host imports, gas metering, storage I/O)
├── arc-mempool (transaction pool, priority ordering, deduplication)
├── arc-consensus (DAG consensus, VRF proposer, staking tiers, shard routing)
├── arc-net (QUIC transport, shred propagation, PEX, Reed-Solomon FEC)
└── arc-gpu (wgpu compute shaders, GPU Ed25519, Metal MSL + WGSL)

arc-bench (benchmark suite)
├── arc-node, arc-crypto, arc-state, arc-types, arc-gpu
```

---

## Consensus Layer (`arc-consensus`)

ARC Chain uses a **DAG-based consensus protocol** with **VRF (Verifiable Random Function) proposer selection**. Validators are organized into staking tiers that determine their block proposal weight.

### Staking Tiers

| Tier | Minimum Stake | Role |
|---|---|---|
| Spark | 500,000 ARC | Observer / vote only (cannot produce blocks) |
| Arc | 5,000,000 ARC | Block producer + voter |
| Core | 50,000,000 ARC | Priority producer + governance |

### VRF Proposer Selection

The `ProposerSelector` module (900+ lines) uses a VRF to determine which validator proposes each block. The VRF output is deterministic given the validator's private key and the current round, but unpredictable to other validators until revealed. This prevents front-running and MEV extraction.

The VRF is wired into `ConsensusManager` -- block proposals check `vrf_approved` before proceeding. Backward compatible: if VRF is not configured, all validators can propose.

### Cross-Shard Coordination

For sharded execution, ARC Chain implements a **cross-shard locking protocol**:

1. **Lock phase**: Acquire locks on all affected accounts across shards
2. **Execute phase**: Transactions execute within their shard
3. **Commit/Abort**: If all shards succeed, commit; otherwise abort and release locks

Transaction types are automatically routed to shards based on their access set declarations in `block_stm.rs`. For example, `ChannelOpen` is cross-shard if the counterparty resides on a different shard; `ChannelClose` and `ChannelDispute` are always local.

---

## Execution Layer

### Block-STM Parallel Execution (`arc-state`)

ARC Chain uses **Block-STM** (Software Transactional Memory) for parallel transaction execution within a block. Transactions are speculatively executed in parallel across CPU cores, and conflicts are detected and re-executed.

Each transaction type declares its **access set** -- the accounts it reads and writes:

| TX Type | Read/Write Set |
|---|---|
| Transfer | sender, recipient |
| Stake | sender, validator |
| WasmCall | sender, contract |
| Escrow | sender, beneficiary |
| BatchSettle | sender, all entry agent accounts |
| ChannelOpen | sender, counterparty |
| ShardProof | (no cross-account conflicts) |

### Jellyfish Merkle Tree (JMT)

Account state is stored in a **Jellyfish Merkle Tree** -- the same authenticated data structure used by Aptos/Diem:

- **Incremental updates**: `apply_dirty()` is O(k log n) where k = changed accounts
- **Merkle inclusion proofs**: For any account, produce a proof verifiable against the state root
- **Non-membership proofs**: Prove that an account does NOT exist (verified via empty-slot walks or different-key-at-slot proofs)
- **WAL persistence**: Write-ahead log with CRC32 checksums ensures crash recovery
- **Snapshots**: Full state snapshots (LZ4-compressed bincode) for node bootstrapping via `/sync/snapshot`

### WASM Virtual Machine (`arc-vm`)

Smart contracts compile to WASM bytecode and execute in a sandboxed `ArcVM` runtime:

- Gas metering (charge per opcode, enforced limits)
- Storage I/O (read/write contract storage slots via `StateDB`)
- Event emission (EVM-compatible event logs with indexed topics)
- Read-only calls (no state mutation, for queries via `/contract/{address}/call`)
- Module compilation and caching

### Gas Costs

Every operation has a defined gas cost. The block gas limit is **30,000,000**.

| Operation | Gas | Notes |
|---|---|---|
| Transaction base (TX_BASE) | 21,000 | Charged for every transaction |
| Data byte (TX_DATA_BYTE) | 16 | Per byte of transaction data |
| Storage read (SLOAD) | 200 | |
| Storage write (SSTORE) | 5,000 | |
| Event log (LOG) | 375 | |
| Transfer | 21,000 | |
| Settle | 25,000 | Agent settlements |
| Swap | 30,000 | Atomic asset swap |
| Stake | 25,000 | Stake/unstake |
| Escrow | 35,000 | Create/release |
| Deploy Contract | 53,000 | + bytecode storage |
| Contract Call | 21,000 | + execution gas |
| Register Agent | 30,000 | |
| Multi-Sig | 35,000 | |
| Join Validator | 30,000 | |
| Leave Validator | 25,000 | |
| Claim Rewards | 25,000 | |
| Update Stake | 25,000 | |
| Governance | 50,000 | |
| Bridge Lock | 50,000 | |
| Bridge Mint | 50,000 | |
| Batch Settle | 30,000 | Covers netting computation |
| Channel Open | 40,000 | |
| Channel Close | 35,000 | |
| Channel Dispute | 50,000 | |
| Shard Proof | 60,000 | STARK proof submission |

---

## State Layer (`arc-state`)

### Account Model

Every account on ARC Chain has:

```rust
pub struct Account {
    pub address: Address,        // 32-byte BLAKE3 hash of public key
    pub balance: u64,            // Spendable balance (smallest unit)
    pub nonce: u64,              // Replay protection counter
    pub code_hash: Hash256,      // WASM bytecode hash (zero if not a contract)
    pub storage_root: Hash256,   // Merkle root of contract storage
    pub staked_balance: u64,     // Locked stake (not spendable)
}
```

Addresses are 32-byte BLAKE3 hashes. The ETH JSON-RPC layer maps 20-byte Ethereum addresses into the 32-byte space for tooling compatibility.

### Transaction Structure

```rust
pub struct Transaction {
    pub tx_type: TxType,         // Discriminant (0x01-0x15)
    pub from: Address,           // Sender address
    pub nonce: u64,              // Replay protection
    pub body: TxBody,            // Type-specific payload (enum of 21 variants)
    pub fee: u64,                // Fee in ARC (can be 0 for settlements)
    pub gas_limit: u64,          // Gas limit (0 = unlimited for backward compat)
    pub hash: Hash256,           // BLAKE3 signing hash
    pub signature: Signature,    // Ed25519, secp256k1, or ML-DSA-65
}
```

The signing hash covers `tx_type || from || nonce || body || fee || gas_limit` using BLAKE3 with domain separation key `"ARC-chain-tx-v1"`.

### Persistence

State is persisted through two mechanisms:

1. **Write-Ahead Log (WAL)** -- Every state mutation is appended to an on-disk journal before acknowledgement. Sequential writes only, CRC32 checksums.

2. **Snapshots** -- Full state dump every N blocks (LZ4-compressed bincode). Used for fast node bootstrap and crash recovery.

**Crash recovery**: Load latest snapshot, replay WAL from checkpoint, verify state root.

---

## Network Layer (`arc-net`)

### QUIC Transport

All P2P communication uses **QUIC** (via the `quinn` crate):

- Encrypted transport (TLS 1.3 built-in)
- Multiplexed streams on a single connection
- 0-RTT connection resumption
- NAT traversal friendly (UDP-based)

### Shred Protocol

Blocks are split into **shreds** for parallel dissemination:

1. Block data is chunked into fixed-size data shreds
2. **Reed-Solomon FEC** generates parity shreds: for every 2 data shreds, 1 XOR parity shred (50% redundancy)
3. Shreds are broadcast to peers independently
4. Receivers can recover any single missing data shred from its pair + parity
5. If both data shreds in a pair are lost, recovery fails (graceful degradation)

### Peer Exchange (PEX)

Nodes share their peer tables every 60 seconds via the `PeerExchange` protocol message (type `0x06`). The PEX message contains `PexPeerInfo` records with peer addresses and connection metadata. New validators can join the network by knowing just one existing peer.

---

## Cryptography Layer (`arc-crypto`, `arc-gpu`)

### Signature Schemes

ARC Chain supports three signature algorithms:

| Scheme | Key Size | Signature Size | Use Case |
|---|---|---|---|
| Ed25519 | 32 bytes | 64 bytes | Default, fastest verification |
| secp256k1 | 33 bytes | 64 bytes | Ethereum/Bitcoin compatibility |
| ML-DSA-65 | 1,952 bytes | 3,309 bytes | Post-quantum (experimental) |

### GPU-Accelerated Ed25519 (`arc-gpu`)

On Apple Silicon (and other wgpu-capable GPUs), Ed25519 verification is offloaded to GPU compute shaders:

- **Metal MSL shader** (`ed25519_verify.metal`): Native hardware u64, preferred path
- **WGSL fallback** (`ed25519_verify.wgsl`): Cross-platform WebGPU path
- **Branchless Shamir's trick**: 4-entry LUT indexed by `(a_bit + 2*b_bit)` -- zero SIMD divergence
- **Buffer pool**: Pre-allocated GPU buffers reused via `queue.write_buffer()`
- **SigVerifyCache**: `DashMap` pre-verification cache integrated into the block pipeline
- **Async dispatch**: `GpuVerifyFuture` with per-dispatch staging buffer

Performance (M4 Pro, 100K batch, release build):
- CPU parallel (Rayon): ~235K verifications/sec
- GPU (Metal MSL): ~121K verifications/sec

### Stwo STARK Prover

ARC Chain uses the **Stwo** prover (Circle STARK over the Mersenne-31 field):

- **22-constraint AIR** (Algebraic Intermediate Representation)
- **Inner-circuit STARK recursion** for composable proofs
- **Shard proof submission** via `ShardProof` TX type (0x15)
- **232 additional tests** enabled with `cargo test -p arc-crypto --features stwo-prover`

### Hashing

- **BLAKE3**: Primary hash function for transactions, blocks, Merkle trees (with domain separation)
- **Poseidon**: ZK-friendly hash for STARK circuits
- **Keccak-256**: Ethereum ABI compatibility (function selectors in SDKs)

---

## Propose-Verify Pipeline

ARC Chain supports a **bifurcated execution model** for maximum throughput:

1. **Proposer node** (with `--proposer-mode`):
   - Executes all transactions in the block
   - Computes state diffs (account balance changes)
   - Broadcasts the block with state diffs attached

2. **Verifier nodes**:
   - Receive the block with state diffs
   - Re-execute to verify diffs match
   - If verified, apply state diffs directly

This enables verifiers to validate blocks significantly faster than full re-execution.

---

## Transaction Types (All 21)

### Core Operations (0x01-0x04)

| Type | Gas | Body Fields |
|---|---|---|
| **Transfer** (0x01) | 21,000 | `to`, `amount`, optional `amount_commitment` (Pedersen) |
| **Settle** (0x02) | 25,000 | `agent_id`, `service_hash`, `amount`, `usage_units` |
| **Swap** (0x03) | 30,000 | `counterparty`, `offer_amount`, `receive_amount`, `offer_asset`, `receive_asset` |
| **Escrow** (0x04) | 35,000 | `beneficiary`, `amount`, `conditions_hash`, `is_create` |

### Staking & Contracts (0x05-0x09)

| Type | Gas | Body Fields |
|---|---|---|
| **Stake** (0x05) | 25,000 | `amount`, `is_stake`, `validator` |
| **WasmCall** (0x06) | 21,000 + exec | `contract`, `function`, `calldata`, `value`, `gas_limit` |
| **MultiSig** (0x07) | 35,000 | `inner_tx`, `signers`, `threshold` |
| **DeployContract** (0x08) | 53,000 | `bytecode`, `constructor_args`, `state_rent_deposit` |
| **RegisterAgent** (0x09) | 30,000 | `agent_name`, `capabilities`, `endpoint`, `protocol`, `metadata` |

### Validator Operations (0x0a-0x0d)

| Type | Gas | Body Fields |
|---|---|---|
| **JoinValidator** (0x0a) | 30,000 | `pubkey` (Ed25519 32 bytes), `initial_stake` |
| **LeaveValidator** (0x0b) | 25,000 | (no body) |
| **ClaimRewards** (0x0c) | 25,000 | (no body) |
| **UpdateStake** (0x0d) | 25,000 | `new_stake` |

### Governance & Bridges (0x0e-0x10)

| Type | Gas | Body Fields |
|---|---|---|
| **Governance** (0x0e) | 50,000 | `proposal_id`, `action` (Execute) |
| **BridgeLock** (0x0f) | 50,000 | `destination_chain`, `destination_address`, `amount` |
| **BridgeMint** (0x10) | 50,000 | `source_chain`, `source_tx_hash`, `recipient`, `amount`, `merkle_proof` |

### L1 Scaling (0x11-0x15)

| Type | Gas | Body Fields |
|---|---|---|
| **BatchSettle** (0x11) | 30,000 | `entries[]` (each: `agent_id`, `service_hash`, `amount`) |
| **ChannelOpen** (0x12) | 40,000 | `channel_id`, `counterparty`, `deposit`, `timeout_blocks` |
| **ChannelClose** (0x13) | 35,000 | `channel_id`, `opener_balance`, `counterparty_balance`, `counterparty_sig`, `state_nonce` |
| **ChannelDispute** (0x14) | 50,000 | `channel_id`, `opener_balance`, `counterparty_balance`, `other_party_sig`, `state_nonce`, `challenge_period` |
| **ShardProof** (0x15) | 60,000 | `shard_id`, `block_height`, `block_hash`, `prev_state_root`, `post_state_root`, `tx_count`, `proof_data` |
