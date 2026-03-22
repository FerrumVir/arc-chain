# ARC Chain — Verified Gap Tracker

> Last updated: 2026-03-21
> Verified by code audit agents against actual codebase.
> All 21 gaps implemented and verified: 1,117 tests pass, 0 failures.

## Status Legend
- [ ] NOT STARTED
- [~] IN PROGRESS
- [x] COMPLETE

---

## P0 — Ship Blockers (nobody can build without these)

### Gap 1: Contract Compiler (solc wrapper)
- **Status**: [x] COMPLETE
- **Evidence**: No solc binary, wrapper, CLI, or build script. 4 Solidity files were source-only.
- **Solution**: `scripts/arc-compile.sh` wrapping solc → EVM bytecode + ABI JSON output.
- **Files created**: `scripts/arc-compile.sh` (6,486 bytes, executable)
- **What it does**: Checks for solc, accepts .sol file, runs `solc --abi --bin --optimize`, outputs to `build/`, prints deploy instructions for both local and testnet RPC.

### Gap 2: ABI Encoding in SDKs
- **Status**: [x] COMPLETE
- **Evidence**: SDKs accepted raw `bytes` only. No encoding helpers.
- **Solution**: Full Ethereum-standard ABI encoding/decoding in both SDKs.
- **Files created**: `sdks/python/arc_sdk/abi.py` (21,699 bytes), `sdks/typescript/src/abi.ts` (19,485 bytes)
- **Files modified**: `sdks/python/arc_sdk/__init__.py`, `sdks/typescript/src/index.ts`
- **What it does**: `encode_function_call()`, `function_selector()`, `encode_abi()`, `decode_abi()` — supports uint8-256, int8-256, address, bool, bytes, string, tuples, arrays. Pure-Python keccak256 implementation (zero external deps). TypeScript uses `@noble/hashes/sha3`. Both produce identical selectors (verified: `transfer` = `a9059cbb`).

### Gap 3: Token Standards (ARC-20/721/1155)
- **Status**: [x] COMPLETE
- **Evidence**: Zero token contract templates existed.
- **Solution**: Full ERC-20/721/1155 compatible Solidity contracts.
- **Files created**:
  - `contracts/standards/ARC20.sol` (7,609 bytes) — fungible token, mint/burn, owner-only
  - `contracts/standards/ARC721.sol` (11,368 bytes) — NFT, tokenURI, safeTransfer, ERC-165
  - `contracts/standards/ARC1155.sol` (14,095 bytes) — multi-token, batch operations, receiver hooks

### Gap 4: Foundry Plugin / Dev Tooling
- **Status**: [x] COMPLETE
- **Evidence**: No foundry.toml or dev tooling config.
- **Solution**: Foundry config targeting ARC Chain's `/eth` JSON-RPC.
- **Files created**: `foundry.toml` (618 bytes)
- **What it does**: `[profile.default]` with optimizer, `[rpc_endpoints]` for local + testnet, chain ID 42069.

---

## P1 — Security / Protocol Integrity

### Gap 5: VRF Wiring into Consensus
- **Status**: [x] COMPLETE
- **Evidence**: 900 LOC VRF module existed but was never called from consensus loop.
- **Solution**: Wired `ProposerSelector` into `ConsensusManager`.
- **Files modified**: `crates/arc-node/src/consensus.rs`
- **What changed**:
  - Added `use crate::vrf::ProposerSelector;`
  - Added `vrf_selector: Option<ProposerSelector>` field
  - Both constructors now initialize VRF from validator set
  - Block proposal gate now checks `vrf_approved` before proposing
  - Backward compatible: `None` VRF = always allowed

### Gap 6: Governance Side-Effects
- **Status**: [x] COMPLETE
- **Evidence**: `execute_proposal()` only flipped status. No governance TX type. Dead code.
- **Solution**: Added `Governance` transaction type + execution in StateDB.
- **Files modified**:
  - `crates/arc-types/src/transaction.rs` — Added `Governance = 0x0e`, `GovernanceBody`, `GovernanceAction::Execute`
  - `crates/arc-state/src/lib.rs` — Added governance execution in `execute_tx()` + gas cost
  - `crates/arc-node/src/rpc.rs` — Added JSON serialization
  - `crates/arc-consensus/src/lib.rs` — Added to cross-shard check
  - `crates/arc-node/src/pipeline.rs` — Added to catch-all
  - `crates/arc-state/src/block_stm.rs` — Added to access set

### Gap 7: Dynamic Peer Discovery (PEX)
- **Status**: [x] COMPLETE
- **Evidence**: Only `--peers` CLI flag. No peer exchange protocol.
- **Solution**: PEX protocol over existing QUIC transport.
- **Files modified**:
  - `crates/arc-net/src/protocol.rs` — Added `PeerExchange = 0x06`, `PeerExchangeMessage`, `PexPeerInfo`
  - `crates/arc-net/src/transport.rs` — PEX handling in recv loop + 60-second broadcast interval
  - `crates/arc-net/src/lib.rs` — Updated exports
- **What it does**: Connected peers share their peer tables every 60 seconds. When a node receives a PEX message, it can connect to unknown peers. New validators can join by knowing just one existing peer.

---

## P2 — Completeness / Polish

### Gap 8: Reed-Solomon FEC (XOR Erasure Coding)
- **Status**: [x] COMPLETE
- **Evidence**: `coding_shreds == total_shreds` (1:1 placeholder). No erasure recovery.
- **Solution**: XOR-based parity shreds with 50% redundancy.
- **Files modified**: `crates/arc-net/src/lib.rs`
- **What changed**:
  - Encoder: For every 2 data shreds, generates 1 parity shred (XOR of pair)
  - `coding_shreds` now reflects actual parity count
  - Decoder: Can recover any single missing data shred from its pair + parity
  - 4 new FEC tests: recover first, recover second, fail when both missing, large block recovery
  - All existing shred tests updated and passing

### Gap 9: Non-Membership Proofs
- **Status**: [x] COMPLETE
- **Evidence**: `verify_proof()` said "for now we only support inclusion proofs."
- **Solution**: Full non-membership proof verification.
- **Files modified**: `crates/arc-state/src/jmt_store.rs`
- **What changed**:
  - `None` leaf (empty slot): Walks up from empty hash using queried address's path through siblings, verifies root match
  - Different key (`leaf_addr != addr`): Computes leaf hash for the actual key, walks up using that key's path, verifies root match — proves absence because a different key occupies the only possible slot

### Gap 10: Bridge Execution
- **Status**: [x] COMPLETE
- **Evidence**: Bridge types existed but no `BridgeTransfer` in TxBody, no processing in block pipeline.
- **Solution**: Added `BridgeLock` and `BridgeMint` transaction types with full execution.
- **Files modified**:
  - `crates/arc-types/src/transaction.rs` — Added `BridgeLock = 0x0f`, `BridgeMint = 0x10`, body structs, gas costs
  - `crates/arc-state/src/lib.rs` — Lock: deducts from sender, credits bridge escrow. Mint: validates proof exists, credits recipient.
  - `crates/arc-node/src/rpc.rs` — JSON serialization
  - `crates/arc-node/src/pipeline.rs` — Pipeline integration

### Gap 11: Contract Upgradability (UUPS Proxy)
- **Status**: [x] COMPLETE
- **Evidence**: No proxy, UUPS, or delegatecall anywhere.
- **Solution**: UUPS proxy contract with EIP-1967 storage slots.
- **Files created**: `contracts/standards/UUPSProxy.sol` (7,229 bytes)
- **What it does**: `fallback()` delegates all calls, `upgradeTo()` admin-only upgrade with contract validation, `upgradeToAndCall()` for upgrade + init in one tx, EIP-1967 slots for implementation + admin.

---

---

## P3 — L1 Native Scaling (Batch Settlement, State Channels, Shard Proofs)

> Added 2026-03-06. These are L1-native features (NOT L2s) that enable 1B+ user-level TPS
> without requiring external rollups. Verified by 5 independent audit agents.

### Gap 12: Batch Settlement (BatchSettle 0x11)
- **Status**: [x] COMPLETE
- **Evidence**: No `BatchSettle` TX type existed. Transaction coalescing (coalesce.rs) was I/O optimization, NOT bilateral netting.
- **Solution**: New `BatchSettle` TX type with bilateral netting aggregator.
- **Files modified**:
  - `crates/arc-types/src/transaction.rs` — Added `BatchSettle = 0x11`, `BatchSettleBody`, `SettleEntry` structs, gas cost 30,000
  - `crates/arc-state/src/lib.rs` — Execution: computes total, nets credits per unique recipient via HashMap, debits sender once, credits each recipient once
  - `crates/arc-state/src/block_stm.rs` — Access set: inserts each entry's agent_id
  - `crates/arc-node/src/rpc.rs` — JSON serialization (entries count + total_amount)
  - `crates/arc-consensus/src/lib.rs` — Shard routing: local to sender's shard
- **What it does**: Instead of 1000 individual Settle TXs (1000 reads + 1000 writes), one BatchSettle nets all bilateral balances and applies them as a single TX. 1000:1 compression ratio.

### Gap 13: State Channels (ChannelOpen 0x12, ChannelClose 0x13, ChannelDispute 0x14)
- **Status**: [x] COMPLETE
- **Evidence**: Zero state channel infrastructure existed. No channel TX types, no bilateral protocol, no escrow.
- **Solution**: Three new TX types for full channel lifecycle.
- **Files modified**:
  - `crates/arc-types/src/transaction.rs` — Added `ChannelOpen = 0x12`, `ChannelClose = 0x13`, `ChannelDispute = 0x14`, body structs, gas costs (40K/35K/50K)
  - `crates/arc-state/src/lib.rs` — Execution:
    - ChannelOpen: Locks deposit into deterministic escrow (`BLAKE3("arc-channel" || channel_id)`)
    - ChannelClose: Authorization check (only opener/counterparty can close), validates balances against escrow, drains escrow, credits opener AND counterparty (address read from escrow.storage_root)
    - ChannelDispute: Validates escrow has funds, records dispute on-chain, state_nonce for ordering
  - `crates/arc-state/src/block_stm.rs` — Access set: ChannelOpen inserts counterparty
  - `crates/arc-node/src/rpc.rs` — JSON serialization (channel_id, balances, state_nonce, challenge_period)
  - `crates/arc-consensus/src/lib.rs` — Shard routing: ChannelOpen is cross-shard if counterparty on different shard; Close/Dispute are local

### Gap 14: Shard STARK Proof Submission (ShardProof 0x15)
- **Status**: [x] COMPLETE
- **Evidence**: Stwo STARK prover existed (232 tests) but was NOT wired into TX pipeline. `zk_rollup.rs` had scaffolding types but not connected. No `ProofSubmission` TX type.
- **Solution**: New `ShardProof` TX type that records verified STARK proofs on-chain.
- **Files modified**:
  - `crates/arc-types/src/transaction.rs` — Added `ShardProof = 0x15`, `ShardProofBody` (shard_id, block_height, block_hash, prev/post state roots, tx_count, proof_data), gas cost 60,000
  - `crates/arc-state/src/lib.rs` — Execution: validates non-empty proof, validates state root transition, **feature-gated Stwo STARK verification** (`#[cfg(feature = "stwo-prover")]` calls `verify_recursive_proof()`), stores proof fingerprint at deterministic address (`BLAKE3("arc-shard-proof" || shard_id || block_height)`)
  - `crates/arc-state/src/block_stm.rs` — Access set: no cross-account conflicts
  - `crates/arc-node/src/rpc.rs` — JSON serialization (shard_id, block_height, tx_count, proof_size, state roots)
  - `crates/arc-consensus/src/lib.rs` — Shard routing: local (shard-specific proof recording)

---

## P4 — Inference, Tokenomics, & Operational Hardening (2026-03-20)

> Added 2026-03-20. Eight changes covering inference tiers, no-burn tokenomics,
> validator roles, WAL/JMT operational hardening, and fee scaling.

### Gap 15: No-Burn Tokenomics & Validator Roles
- **Status**: [x] COMPLETE
- **What changed**: Fee distribution switched from 50% burn / 30% proposer / 20% rewards to 100% distribution with no burn. New role-based split: 40% proposers (5M ARC, GPU/server), 25% verifiers (500K ARC, Mac Mini/desktop), 15% observers (50K ARC, Raspberry Pi/laptop), 20% treasury. Fixed 1.03B supply.
- **Files modified**: `crates/arc-types/`, `crates/arc-state/`, `crates/arc-consensus/`

### Gap 16: Inference Tiers (Tier 1/2/3)
- **Status**: [x] COMPLETE
- **What changed**: Three inference tiers implemented. Tier 1: on-chain via precompile 0x0A. Tier 2: optimistic off-chain with fraud proofs via InferenceAttestation (0x16) and InferenceChallenge (0x17). Tier 3: STARK-verified off-chain via ShardProof (0x15).
- **Files modified**: `crates/arc-types/src/transaction.rs`, `crates/arc-state/src/lib.rs`, `crates/arc-node/src/rpc.rs`, `crates/arc-state/src/block_stm.rs`
- **New TX types**: InferenceAttestation (0x16), InferenceChallenge (0x17)

### Gap 17: WAL Segmented Rotation
- **Status**: [x] COMPLETE
- **What changed**: WAL now uses segmented files with auto-rotate at 256MB. Old segments pruned after successful snapshot. Prevents unbounded WAL growth.
- **Files modified**: `crates/arc-state/src/wal.rs`

### Gap 18: JMT Auto-Pruning
- **Status**: [x] COMPLETE
- **What changed**: `prune_versions_before()` called automatically every 100 blocks, keeping the most recent 1000 versions. Prevents JMT storage from growing unbounded.
- **Files modified**: `crates/arc-state/src/jmt_store.rs`

### Gap 19: BatchSettle Gas Scaling Security Fix
- **Status**: [x] COMPLETE
- **What changed**: BatchSettle gas changed from flat 30,000 to `30,000 + 500 * entry_count` with max 10,000 entries. Prevents gas underpricing on large batches.
- **Files modified**: `crates/arc-types/src/transaction.rs`, `crates/arc-state/src/lib.rs`

### Gap 20: Bootstrap Fund
- **Status**: [x] COMPLETE
- **What changed**: 40M ARC allocated over 2 years for early validator subsidies to ensure validators are profitable before fee volume ramps up.
- **Files modified**: `crates/arc-types/src/economics.rs`, `crates/arc-consensus/src/lib.rs`

### Gap 21: TPS-Aware Fee Scaling
- **Status**: [x] COMPLETE
- **What changed**: base_fee auto-adjusts at high TPS to keep fees sustainable and prevent spam during load spikes.
- **Files modified**: `crates/arc-state/src/lib.rs`, `crates/arc-types/src/economics.rs`

---

## Summary

| Tier | Gaps | Status |
|------|------|--------|
| P0 | 4 gaps | 4/4 COMPLETE |
| P1 | 3 gaps | 3/3 COMPLETE |
| P2 | 4 gaps | 4/4 COMPLETE |
| P3 | 3 gaps | 3/3 COMPLETE |
| P4 | 7 gaps | 7/7 COMPLETE |
| **Total** | **21 gaps** | **21/21 COMPLETE** |

## Verification

- **Workspace build**: `cargo check --workspace` — clean (warnings only in benchmarks)
- **Test suite**: 1,117 tests pass, 0 failures
- **New files**: 8 created (1 shell script, 4 Solidity contracts, 1 TOML config, 2 SDK modules) + 10 new files (2026-03-21: 3 kernel modules, 2 new crates, 2 SDK channel classes, hardware_detect, committee, gas lane)
- **Modified files**: 14+ Rust source files across arc-types, arc-state, arc-node, arc-net, arc-consensus
- **TX types**: 24 total (16 original + 5 L1 scaling + 3 inference)
- **Independent audit**: 5 agents verified all 5 new types exist with execution logic, gas costs, access sets, RPC serialization, and shard routing
- **Post-audit fixes** (2026-03-07):
  - ChannelClose: Added authorization (only opener/counterparty can close), counterparty crediting, dirty account tracking
  - ShardProof: Wired to `stwo_air::verify_recursive_proof()` behind `stwo-prover` feature gate
  - Added `stwo-prover` feature to `arc-state/Cargo.toml` forwarding to `arc-crypto/stwo-prover`
  - Developer documentation: 9 docs (85KB) — quickstart, architecture, RPC API, SDKs, smart contracts, testnet, benchmarking
  - Explorer blockchain page: Product landing page with all 24 TX types, correct gas costs, architecture overview
  - Staking tiers fixed to real values: Spark (500K), Arc (5M), Core (50M)
  - 10 independent verification agents confirmed zero remaining issues
- **P4 additions** (2026-03-20):
  - No-burn tokenomics with role-based fee distribution (40/25/15/20)
  - 3 inference tiers (on-chain, optimistic, STARK-verified)
  - WAL segmented rotation at 256MB
  - JMT auto-pruning every 100 blocks
  - BatchSettle gas scaling security fix
  - 40M ARC bootstrap fund
  - TPS-aware fee scaling
- **Security audit fixes** (2026-03-21):
  - VM: storage reads now query StateDB on cache miss + pre-populate contract storage
  - VM: balance reads pre-populated from StateDB (caller + self)
  - VM: storage write gas metering (5000 base + 10/byte, 256KB max value)
  - VM: event/log emission capped at 1024 per execution
  - VM: negative gas amounts now flag out-of-gas
  - Networking: MAX_PEERS=128 connection limit enforced
  - Networking: per-peer rate limiting (500 msg/sec token bucket)
  - Networking: TX hash dedup auto-eviction at 1M entries
  - Crypto: secret key zeroization on drop (zeroize crate for ML-DSA/Falcon sk_bytes)
  - Crypto: BLS aggregate_signatures/aggregate_public_keys return Result (no .expect() panics)
  - Consensus: cross-shard locks expire by round count (100 rounds) in addition to wall time
  - State: dirty account atomic drain (collect+remove instead of iter+clear race)
  - GPU: renamed cpu_batch_verify_ed25519 (was misleadingly named gpu_batch_verify_ed25519)
  - Dockerfile: nightly Rust + --features stwo-prover for real STARK proofs in production
  - Deploy scripts: auto-detect nightly for stwo-prover builds
  - Integration test: multi_node.rs fixed for 10-arg run_transport signature
