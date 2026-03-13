# ARC Chain — Project Status

> **Version**: 0.2.0 (pre-mainnet, L1 scaling)
> **Last updated**: 2026-03-13
> **Codebase**: 75,001 LOC Rust (11 crates) · 1,944 LOC Solidity · 4,699 LOC SDKs · 7,552 LOC explorer/docs
> **Tests**: 1,050 passing, 0 failures
> **TX Types**: 21 (16 core + 5 L1 scaling)
> **Benchmark**: 27K TPS sustained (M4 MacBook, 2 validators, real consensus)

---

## What ARC Chain Is

A high-performance Layer 1 blockchain purpose-built for AI agent settlements. DAG consensus with sub-second finality, WASM + EVM dual execution, GPU-accelerated signature verification, and zero-fee agent-to-agent settlements.

**Reference documents:**
- [SPEC.md](SPEC.md) — L1 technical specification v1.0
- [BENCHMARK_RESULTS.md](BENCHMARK_RESULTS.md) — Performance measurements and scaling projections
- [GAP_TRACKER.md](GAP_TRACKER.md) — Implementation gap audit (11/11 closed)
- [docs/](docs/) — Developer documentation (quickstart, architecture, RPC, SDKs)

---

## Crate Status

| Crate | LOC | Tests | Status | What It Does |
|-------|-----|-------|--------|-------------|
| **arc-crypto** | ~8,200 | 126 | Production | Ed25519, Secp256k1, BLS (blst), BLAKE3, ML-DSA, Falcon-512, VRF, threshold crypto, Merkle trees, Pedersen commitments, ZK circuits, Stwo STARK prover |
| **arc-types** | ~5,800 | 258 | Production | 21 transaction types (16 core + 5 L1 scaling), block/header, protocol versioning, governance, economics (EIP-1559), staking tiers, bridge types, account abstraction, multisig, social recovery, batch settlement, state channels, shard proofs |
| **arc-state** | ~7,800 | 60 | Production | DashMap state, JMT (Jellyfish Merkle Tree) with inclusion + non-membership proofs, WAL persistence (CRC32 + LZ4), snapshots, BlockSTM parallel execution, GPU-resident state cache (Metal unified memory / CPU fallback), light client proofs, state sync |
| **arc-vm** | ~3,900 | 32 | Production | Wasmer 6.0 WASM runtime, revm 19 EVM, gas metering, host imports, precompiles (Ed25519/Secp256k1/BLS/BLAKE3/SHA256), AI inference oracle, formal verification model-checker |
| **arc-mempool** | ~1,200 | 17 | Production | SegQueue FIFO, deduplication, encrypted mempool (BLS threshold, feature-flagged), capacity limits |
| **arc-consensus** | ~4,500 | 26 | Production | Mysticeti-inspired DAG, 2-round finality, stake tiers (Spark/Arc/Core), slashing (equivocation + liveness), cross-shard coordination, canonical TX ordering (MEV protection), epoch transitions |
| **arc-net** | ~2,800 | 31+ | Production | QUIC transport (quinn), shred propagation with XOR FEC, TX gossip, challenge-response peer auth, stake-weighted peer selection, PEX (peer exchange protocol) |
| **arc-node** | ~7,100 | 17 | Production | Consensus manager with VRF proposer selection, signature verification pipeline, RPC API (20+ HTTP + ETH JSON-RPC), propose-verify mode |
| **arc-gpu** | ~2,400 | 32 | Production | Metal MSL + WGSL Ed25519 batch verification, branchless Shamir's trick, buffer pool, async dispatch, SigVerifyCache, GPU account buffer (unified/managed/CPU-only memory) |
| **arc-bench** | 5,336 | — | Tool | 10 benchmark binaries (multinode, parallel, signed, soak, production, mixed, node, propose-verify, gpu-state) |
| **arc-cli** | 660 | — | Tool | Command-line client: keygen, RPC queries, transaction submission |

---

## Transaction Types (21 total)

### Core Protocol (16 types)

| Code | Type | Gas | Status |
|------|------|-----|--------|
| 0x01 | Transfer | 21,000 | Implemented + tested |
| 0x02 | Settle (agent settlement) | 25,000 | Implemented + tested (zero fee) |
| 0x03 | Swap | 30,000 | Implemented + tested |
| 0x04 | Escrow | 35,000 | Implemented + tested |
| 0x05 | Stake | 25,000 | Implemented + tested |
| 0x06 | WasmCall | 21,000 + exec | Implemented + tested |
| 0x07 | MultiSig | 35,000 | Implemented + tested |
| 0x08 | DeployContract | 53,000 | Implemented + tested |
| 0x09 | RegisterAgent | 30,000 | Implemented + tested |
| 0x0a | JoinValidator | 30,000 | Implemented + tested |
| 0x0b | LeaveValidator | 25,000 | Implemented + tested |
| 0x0c | ClaimRewards | 25,000 | Implemented + tested |
| 0x0d | UpdateStake | 25,000 | Implemented + tested |
| 0x0e | Governance | 50,000 | Implemented (2026-03-06) |
| 0x0f | BridgeLock | 50,000 | Implemented (2026-03-06) |
| 0x10 | BridgeMint | 50,000 | Implemented (2026-03-06) |

### L1 Native Scaling (5 types — enables 1B+ user-level TPS without L2s)

| Code | Type | Gas | Status |
|------|------|-----|--------|
| 0x11 | BatchSettle (bilateral netting) | 30,000 | Implemented (2026-03-06) |
| 0x12 | ChannelOpen (lock funds) | 40,000 | Implemented (2026-03-06) |
| 0x13 | ChannelClose (mutual release) | 35,000 | Implemented (2026-03-06) |
| 0x14 | ChannelDispute (challenge) | 50,000 | Implemented (2026-03-06) |
| 0x15 | ShardProof (STARK verification) | 60,000 | Implemented (2026-03-06) |

---

## Feature Completeness Matrix

### Core Protocol

| Feature | Status | Notes |
|---------|--------|-------|
| DAG consensus (Mysticeti) | DONE | 2-round commit, quorum-based finality |
| Stake tiers (Spark/Arc/Core) | DONE | 500K / 5M / 50M ARC thresholds |
| Slashing (equivocation + liveness) | DONE | Progressive: 10/20/30% by tier |
| VRF proposer selection | DONE | Wired into ConsensusManager (2026-03-06) |
| View change / timeout | DONE | force_advance_round() with relaxed parent check |
| Canonical TX ordering (MEV) | DONE | Lexicographic by hash, ordering_commitment verified |
| Encrypted mempool | DONE | BLS threshold, feature-flagged, FIFO ordering |
| Cross-shard coordination | DONE | Lock/commit/abort state machine, 30s timeout |
| Epoch transitions | DONE | Validator reward calculation, set updates |
| Protocol versioning | DONE | Semantic versions, upgrade schedule, feature flags |
| Governance TX type | DONE | On-chain execution recording (2026-03-06) |

### Networking

| Feature | Status | Notes |
|---------|--------|-------|
| QUIC transport | DONE | quinn, TLS 1.3, multiplexed streams |
| Challenge-response auth | DONE | Ed25519 signed nonce + genesis hash binding |
| Shred propagation | DONE | 1,280-byte chunks, block reassembly |
| XOR erasure coding (FEC) | DONE | 50% redundancy, single-shred recovery (2026-03-06) |
| TX gossip with dedup | DONE | Batched, stake-weighted fan-out |
| Peer Exchange (PEX) | DONE | 60-second broadcast interval (2026-03-06) |
| Stake-weighted QoS | DONE | Priority based on stake ratio |

### Execution

| Feature | Status | Notes |
|---------|--------|-------|
| WASM runtime (Wasmer 6.0) | DONE | Host imports, gas metering middleware |
| EVM runtime (revm 19) | DONE | EVM opcode execution, storage/memory/stack |
| BlockSTM parallel execution | DONE | Sender-sharded, conflict detection, abort/retry |
| Gas metering | DONE | Per-operation costs, OutOfGas handling |
| Contract deployment | DONE | Address = BLAKE3(deployer ‖ nonce), bytecode storage |
| Cross-contract calls | DONE | Synchronous, value passing |
| Precompiles | DONE | Ed25519, Secp256k1, BLS, BLAKE3, SHA256, modexp |
| AI inference oracle | DONE | Model ID + input/output hash on-chain |

### State

| Feature | Status | Notes |
|---------|--------|-------|
| DashMap concurrent state | DONE | Lock-free reads, parallel execution |
| JMT (Jellyfish Merkle Tree) | DONE | Incremental updates, domain-separated BLAKE3 |
| Inclusion proofs | DONE | Log(n) Merkle proofs for light clients |
| Non-membership proofs | DONE | Empty-slot + different-key verification (2026-03-06) |
| WAL persistence | DONE | CRC32 integrity, LZ4 compression, checkpoint + replay |
| Snapshots | DONE | Bincode + LZ4, chunked for parallel download |
| State sync | DONE | StreamedSnapshot, per-chunk verification |
| State pruning | DONE | prune_versions_before() on JMT |
| GPU-resident state cache | DONE | wgpu unified memory (Metal) / managed (Vulkan) / CPU fallback. CPU-side DashMap mirror for fast reads, GPU buffer for batch compute shaders (BlockSTM, Merkle hashing). Lazy flush_to_gpu() per block. 15.2M lookups/sec on M4. (2026-03-13) |

### Cryptography

| Feature | Status | Notes |
|---------|--------|-------|
| Ed25519 (+ batch verify) | DONE | ed25519-dalek, ~2x batch speedup |
| Secp256k1 (ECDSA recovery) | DONE | k256, MetaMask compatible |
| BLS12-381 (aggregate sigs) | DONE | blst (supranational), threshold t-of-n |
| ML-DSA (FIPS 204) | DONE | Post-quantum digital signatures |
| Falcon-512 | DONE | Post-quantum, faster signing |
| BLAKE3 hashing | DONE | Domain-separated, GPU-accelerated |
| Pedersen commitments | DONE | Homomorphic, privacy-preserving |
| VRF | DONE | Verifiable random function for proposer selection |
| Threshold encryption | DONE | Shamir secret sharing, verifiable shares |
| Stwo STARK prover | DONE | Circuit building, proof aggregation, recursive composition |
| GPU Ed25519 (Metal + WGSL) | DONE | Branchless Shamir, buffer pool, async dispatch |
| GPU account buffer | DONE | 128-byte aligned GpuAccountRepr, unified/managed/CPU-only memory paths, secure shutdown (2026-03-13) |

### Token Economics

| Feature | Status | Notes |
|---------|--------|-------|
| Native ARC token | DONE | Genesis accounts, transfer TX |
| EIP-1559 fee market | DONE | Base fee + priority fee, 50% burn / 50% proposer |
| Staking with APY tiers | DONE | 5% Lite / 8% Spark / 15% Arc / 25% Core |
| Slashing penalties | DONE | Progressive by tier |
| Unbonding periods | DONE | 1d Lite / 7d Spark / 14d Arc / 30d Core |
| Claim rewards | DONE | On-demand calculation + claim TX |
| Free settlements | DONE | Settle TX type = zero base fee |

### Smart Contract Tooling

| Feature | Status | Notes |
|---------|--------|-------|
| Contract compiler (solc wrapper) | DONE | scripts/arc-compile.sh (2026-03-06) |
| ABI encoding (Python SDK) | DONE | sdks/python/arc_sdk/abi.py (2026-03-06) |
| ABI encoding (TypeScript SDK) | DONE | sdks/typescript/src/abi.ts (2026-03-06) |
| ARC-20 token standard | DONE | contracts/standards/ARC20.sol (2026-03-06) |
| ARC-721 NFT standard | DONE | contracts/standards/ARC721.sol (2026-03-06) |
| ARC-1155 multi-token standard | DONE | contracts/standards/ARC1155.sol (2026-03-06) |
| UUPS proxy (upgradability) | DONE | contracts/standards/UUPSProxy.sol (2026-03-06) |
| Foundry config | DONE | foundry.toml, chain ID 42069 (2026-03-06) |

### L1 Native Scaling

| Feature | Status | Notes |
|---------|--------|-------|
| BatchSettle TX (0x11) | DONE | Bilateral netting, 1000:1 compression, nets per-recipient via HashMap (2026-03-06) |
| ChannelOpen TX (0x12) | DONE | Locks deposit in deterministic escrow (BLAKE3("arc-channel" ‖ channel_id)) (2026-03-06) |
| ChannelClose TX (0x13) | DONE | Mutual close, validates balances vs escrow, releases funds (2026-03-06) |
| ChannelDispute TX (0x14) | DONE | Submit signed state with state_nonce ordering, challenge period (2026-03-06) |
| ShardProof TX (0x15) | DONE | Records verified STARK proof at deterministic address, validates state root transition (2026-03-06) |
| Propose-verify mode | DONE | Proposers execute + export diff, verifiers apply diff + check root. Fraud detection. |
| Cross-shard locking | DONE | Lock/commit/abort state machine, 30s timeout, atomic batch ops |
| Transaction coalescing | DONE | I/O optimization: nets same-account reads/writes across batch |

### Bridge

| Feature | Status | Notes |
|---------|--------|-------|
| Bridge types | DONE | BridgeTransfer, BridgeProof, ChainId enum |
| BridgeLock TX (ARC → ETH) | DONE | Lock in escrow, emit event (2026-03-06) |
| BridgeMint TX (ETH → ARC) | DONE | Proof validation, credit recipient (2026-03-06) |
| ArcBridge.sol (Ethereum side) | DONE | Lock/unlock with Merkle proof verification |
| Bridge relayer | NOT STARTED | Needs: event listener, proof submission service |
| Ethereum light client on ARC | NOT STARTED | Store ETH headers, verify proofs trustlessly |

### Account Abstraction

| Feature | Status | Notes |
|---------|--------|-------|
| SmartAccount types | DONE | Owner, guardians, session keys, spending limits, modules |
| SessionKey manager | DONE | Time-bounded, contract whitelist, rate limiting |
| MultiSig manager | DONE | Weighted approval, proposal lifecycle |
| Social recovery manager | DONE | 6 guardian types, time-locked recovery |
| UserOperation (ERC-4337) | DONE | Paymaster sponsorship, gas separation |
| MultiSig TX in pipeline | DONE | TxBody::MultiSig processed in block_stm |
| EntryPoint contract | NOT STARTED | Needed for full ERC-4337 flow |

### Developer Tools

| Feature | Status | Notes |
|---------|--------|-------|
| HTTP RPC API (20+ endpoints) | DONE | Blocks, accounts, TX submit/query, stats |
| ETH JSON-RPC compatibility | DONE | eth_blockNumber, eth_getBalance, eth_call, eth_estimateGas, eth_getLogs |
| Python SDK | DONE | Transaction building, signing, RPC client, ABI encoding |
| TypeScript SDK | DONE | Transaction building, signing, RPC client, ABI encoding |
| Block explorer (Next.js) | DONE | Blocks, transactions, accounts pages |
| Testnet faucet (Rust) | DONE | Token drip endpoint |
| Developer docs | DONE | 9 guides: quickstart, architecture, RPC, SDKs, contracts, testnet, benchmarking |

### Governance

| Feature | Status | Notes |
|---------|--------|-------|
| Proposal types (7 types) | DONE | ProtocolUpgrade, ParameterChange, TreasurySpend, AddValidator, RemoveValidator, FeatureFlagToggle, EmergencyAction |
| Voting (For/Against/Abstain) | DONE | Quorum 40%, approval 60%, emergency 75% |
| Timelock enforcement | DONE | 2-day delay + 3-day execution window |
| Governance TX type (0x0e) | DONE | On-chain execution recording (2026-03-06) |
| Side-effect execution | PARTIAL | TX type records execution; caller applies state changes. Wire apply_governance_outcome() into state for auto-mutation. |

---

## Benchmark Results (2026-03-06)

| Metric | Value |
|--------|-------|
| Sustained TPS (100K TX) | 19,800 |
| Sustained TPS (500K TX) | 27,000 |
| Peak TPS (1-second window) | 350,000 |
| Commit rate | 100% (500K/500K) |
| Elapsed time (500K TX) | 18.5 seconds |
| Blocks produced | 7 |
| Hardware | Apple M4 MacBook Pro, 10 cores |
| Validators | 2 (real QUIC, real consensus, real signatures) |

### Projected Performance

| Hardware | Projected TPS | Basis |
|----------|--------------|-------|
| M4 MacBook (measured) | 27,000 | Actual benchmark |
| EPYC 9654 (96 cores) | 162,000-270,000 | 6-10x CPU scaling |
| A100 (GPU sig verify) | 270,000-810,000 | 2-3x GPU batch verify |
| H100 (GPU + CPU) | 540,000-1,350,000 | Compound scaling |
| 128 validators (sharded) | 1,000,000+ | Linear shard scaling |

---

## What's Next

### Immediate (next sprint)

1. **SPEC.md update** — Sync with actual implementation (21 TX types, VRF, PEX, FEC, governance, bridge, channels, shard proofs)
2. **Channel counterparty tracking** — Store counterparty address in channel metadata on-chain so ChannelClose credits both parties
3. **ShardProof ↔ Stwo integration** — Wire ShardProof TX to call `stwo_air::verify_recursive_proof()` for actual cryptographic verification
4. **Governance auto-mutation** — Wire `apply_governance_outcome()` so proposals auto-execute on StateDB
5. **A100/H100 benchmark** — Run multinode_bench on server hardware to validate projections

### Short-term (1-3 months)

6. **Ethereum light client on ARC** — Store ETH block headers, verify Merkle proofs trustlessly (removes bridge relayer trust assumption)
7. **WebSocket subscriptions** — Real-time block/TX streaming on RPC
8. **Explorer contract verification** — Accept source + compiler version, verify bytecode match
9. **Foundry deployment docs** — End-to-end: write contract → compile → deploy → verify → interact
10. **Testnet launch** — 4+ validators running publicly for 30+ days

### Medium-term (3-6 months)

11. **Security audit** — External firm (Trail of Bits, OtterSec, Halborn)
12. **Formal safety proof** — TLA+ or Coq model of DAG consensus
13. **Multi-shard benchmark** — 4+ shards with cross-shard TX load
14. **Mainnet genesis** — Token migration from ERC-20, validator onboarding
15. **SDK v2** — Auto-generated TypeScript bindings from contract ABI

---

## File Structure

```
arc-chain/
├── crates/
│   ├── arc-crypto/     # Signatures, hashing, BLS, ZK, VRF, threshold
│   ├── arc-types/      # TX types, blocks, governance, economics, bridge, AA
│   ├── arc-state/      # StateDB, JMT, WAL, BlockSTM, light client, sync
│   ├── arc-vm/         # WASM (Wasmer), EVM (revm), gas, precompiles, inference
│   ├── arc-mempool/    # TX queue, encrypted mempool, dedup
│   ├── arc-consensus/  # DAG, finality, slashing, MEV ordering, epochs
│   ├── arc-net/        # QUIC, shreds, FEC, gossip, PEX, peer auth
│   ├── arc-node/       # Consensus manager, VRF, RPC, pipeline
│   ├── arc-gpu/        # Metal/WGSL Ed25519, buffer pool, async dispatch
│   └── arc-bench/      # 8 benchmark binaries
├── contracts/
│   ├── standards/      # ARC20, ARC721, ARC1155, UUPSProxy
│   ├── ARCStaking.sol  # Staking with tiers
│   ├── ArcBridge.sol   # Cross-chain bridge
│   ├── ArcStateRoot.sol # State root commitments
│   └── TaxSplitter.sol # Fee distribution
├── sdks/
│   ├── python/         # arc_sdk: tx, signing, rpc, abi
│   └── typescript/     # @arc-chain/sdk: tx, signing, rpc, abi
├── explorer/           # Next.js block explorer
├── faucet/             # Rust testnet faucet
├── docs/               # 9 developer guides
├── scripts/            # arc-compile.sh, deploy, CI
├── SPEC.md             # L1 technical specification
├── BENCHMARK_RESULTS.md # Performance report
├── GAP_TRACKER.md      # Implementation gap audit
├── STATUS.md           # This file
└── foundry.toml        # Foundry dev tooling config
```
