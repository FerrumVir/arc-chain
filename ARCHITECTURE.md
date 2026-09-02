# ARC Chain - Historical v0.5.3 Architecture Snapshot

> **ARCHIVED — NOT AN OPERATOR RUNBOOK OR CURRENT NETWORK STATUS.** This file
> describes a v0.5.3-era design and contains historical implementation notes.
> Validate every claim against current code and [`README.md`](README.md).
> Never use legacy rolling, self-heal, restart, or reset procedures from this
> snapshot. Current validator recovery is the fully quiesced, fenced,
> manifest-bound checkpoint cutover in
> [`docs/VALIDATOR-FLEET-ROLLOUT.md`](docs/VALIDATOR-FLEET-ROLLOUT.md) and
> [`scripts/recovery/README.md`](scripts/recovery/README.md).

## Version: 0.5.3 | LOC: 99K+ | Crates: 16 | Tests: 1,211

---

## Consensus: Sender-Sharded DAG (Mysticeti-inspired)

**File:** `crates/arc-consensus/src/lib.rs` (3,600 lines)

- **Two-round commit rule**: Block B committed when referenced by 2f+1
  stake-weighted blocks two rounds later. ~200ms finality.
- **MEV protection**: Transactions sorted lexicographically with ordering
  commitment. EncryptedMempool for commit-reveal.
- **Epoch transitions**: `freeze_epoch()` creates deterministic frozen
  validator sets. All nodes freeze identically at same committed height.
- **Slashing**: DoubleSigning, LivenessFault, InvalidBlockProposal,
  EquivocationDAG. Rates: 10% (Spark), 20% (Arc), 30% (Core).

### Beacon Chain
**File:** `crates/arc-consensus/beacon.rs` (200 lines)
- Hierarchical sharding coordinator
- Each shard runs own DAG consensus
- Beacon collects state roots per epoch
- Global root = Merkle(shard_0_root, shard_1_root, ...)

### Security Modules - ALL WIRED (v0.5.3)
**File:** `crates/arc-consensus/security.rs` (300 lines)
- Withholding detection (>50% score = report) - wired since v0.4.x
- Long-range attack prevention (checkpoints every 1000 rounds) - **wired v0.5.3**
  `CheckpointRegistry` in `ConsensusManager`, auto-creates checkpoint at round % 1000
- Nothing-at-stake mitigation (double-vote detection) - **wired v0.5.3**
  `StakeTracker` in `ConsensusManager`, reports votes on every committed block,
  detects double voting, records slashing penalties with graduated schedule

---

## VRF Proposer Selection

**Files:**
- `crates/arc-crypto/src/vrf.rs` (150 lines) - Core VRF primitives
- `crates/arc-node/src/vrf.rs` (300 lines) - ProposerSelector

### What it does:
- Ed25519 + BLAKE3 VRF (RFC 9381-inspired)
- `vrf_prove(keypair, alpha)` → (proof, output)
- `vrf_verify(pubkey, alpha, proof)` → output
- Stake-weighted threshold: P(selected) = stake / total_stake
- Sortition: lowest `weighted_score()` wins

### STATUS: WIRED (v0.5.3)
VRF proposer selection is wired into the consensus loop at `consensus.rs:645`.
In DAG mode (multi-validator), all validators propose every round (DAG requires
it for quorum). In single-validator mode, VRF gates block production via
`ProposerSelector::is_proposer()`. The `vrf_approved` result feeds into
`allow_propose` which controls whether `propose_block()` is called.

---

## Inference Verification - 3 Tiers

### Tier 1: All-Execute (small models <20B)
All validators run inference independently. Majority vote determines output.

### Tier 2: VRF Committee (models 20-100B)
**File:** `crates/arc-inference/src/committee.rs` (334 lines)
- `select_committee(vrf_seed, validators, tier, k=7)` - deterministic
- `aggregate_votes(committee, votes)` - 5/7 agreement required
- `corruption_probability(f=0.1, k=7, min=5)` = 0.018%

### STATUS: WIRED (v0.5.3)
After `inference_run_sharded()` produces output, `select_committee()` is called
with the output hash as VRF seed. Committee info (members, min_agreement,
corruption probability) is included in the inference response. In the current
deterministic integer engine, all honest committee members produce identical
output, so committee consensus is guaranteed for non-malicious validators.

### Tier 3: STARK-Proven (single validator + proof)
**File:** `crates/arc-crypto/src/stwo_air.rs` (1,400 lines)
- Real Stwo Circle STARK prover (NOT mock)
- `prove_dense_stark()` - proves one transformer layer
- `prove_block()` - proves full block with weights + activations + state
- `prove_recursive()` - recursive proof composition
- Field: M31 (2^31 - 1), Blake2s Merkle commitments
- Feature-gated: `--features stwo-prover`

### Attestation System - WIRED (v0.5.3)
**File:** `crates/arc-vm/src/inference_verify.rs` (150 lines)
- `InferenceCommitment` - provider posts result_hash + bond
- `VerificationChallenge` - challenger posts bond + deadline
- Challenge types: ReExecution, SpotCheck, StatisticalAudit, ConsensusVerification
- Resolution: winner takes loser's bond
- **Wired:** `VerificationManager` is instantiated in `NodeState`. Commitments are
  auto-submitted after every `inference_run_sharded()` call. Endpoints:
  `POST /inference/commit`, `POST /inference/challenge`, `GET /inference/verification_status`

---

## Distributed Inference Engine

**File:** `crates/arc-inference/src/distributed.rs` (400+ lines)

### ShardRegistry (MULTI-MODEL from day 1)
```rust
pub struct ShardRegistry {
    models: DashMap<Hash256, Vec<ShardAssignment>>,    // per-model shards
    node_shards: DashMap<Hash256, Vec<(Hash256, ShardAssignment)>>,
}
```
- `register_shard(model_id, shard)` - per-model
- `get_pipeline(model_id)` - ordered shard list for one model
- `is_model_fully_covered(model_id, total_layers)` - completeness check
- `fully_covered_models()` - which models have full pipelines

### STATUS: WIRED (v0.5.3)
The multi-model `ShardRegistry` is now instantiated in `NodeState` as
`multi_model_registry`. On every `/shards/announce`, shards are registered
in BOTH the flat registry (backward compat) and the multi-model registry.
Endpoints: `GET /models` (list all models), `GET /models/shards?model_id=0x...`
(per-model pipeline info).

### Auto-Sharding - WIRED (v0.5.3)
- `compute_shard_plan(nodes, n_layers)` - distributes layers proportional to RAM
- GPU bonus: nodes with GPU get 1.5x weight
- `compute_expert_shard_plan()` - MoE expert distribution
- **Wired:** `POST /shards/auto_plan` computes optimal plan from live node
  capabilities and registers the assignments in the multi-model registry.

### DistributedCache (already multi-model)
```rust
pub fn cache_key(model_id: &Hash256, input_tokens: &[u32]) -> Hash256
pub struct CacheEntry { model_id: Hash256, output_tokens: Vec<u32>, ... }
```

---

## Integer Inference Engine

**File:** `crates/arc-inference/src/cached_integer_model.rs` (3,400 lines)

### Precision Hierarchy
1. **I16** (default) - 32,767 quantization levels, loaded from f32
2. **I8** - 127 levels, fallback
3. **Q4** - 16 levels, 2x bandwidth reduction, opt-in via ARC_Q4_SHARD=1

### SIMD Acceleration (shipped v0.5.2)
- `dot_i16_i64_neon()` - NEON i16 matmul (3.7x on M2 Ultra)
- `dot_i64xi64_attn_neon()` - NEON attention Q*K dot product
- `matmul_simd_preq_neon()` - NEON i8xi8 (EXISTS, not wired into shard dispatch)
- `matmul_q4_preq_neon()` - NEON Q4 (wired, opt-in)
- AVX-512 path reverted (consensus segfault on Vultr Xeon)

### Unused Optimizations
- `flash_attention_i64()` - online softmax with i64 KV cache. **WIRED (v0.5.3)** into
  `forward_one_token()`, `forward_shard_token()`, and `forward_shard_layers()`.
  Replaces O(full_seq) scores array with O(d_head) streaming accumulation.
- `flash_attention_i8()` - online softmax variant for i8-quantized KV. Available for future use.
- `quantize_for_dot()` + `dot_i8_kv_neon()` - KV cache quantization. Available for i8 cache path.

### Forward Paths
- `forward_one_token()` - full model, single node
- `forward_shard_token()` - layer range, for pipeline sharding
- Both produce bit-identical output across ARM, x86

---

## GPU Engine

**File:** `crates/arc-gpu/src/gpu_forward.rs` (800 lines)

- `GpuForward` - wgpu-based transformer engine
- All kernels: matmul, layernorm, RoPE, attention, SiLU, residual, argmax
- Metal + WGSL backends
- `forward_one_token(model, token, pos) -> u32`
- Embedding lookup **FIXED (v0.5.3):** token embeddings are now stored CPU-side as
  i32 (dequantized from i8*scale) and uploaded to the GPU hidden_buf per-token.
  Previously wrote zeros.
- **BENCHMARK ONLY** - not in the default inference path (integer engine is production).
  To use: `cargo run --example bench_gpu_forward --features candle --release`
- 5 Metal shaders (attention.metal, rope.metal, silu.metal, residual.metal, argmax.metal) - UNTESTED

---

## Token Economics

**File:** `crates/arc-types/src/economics.rs` (200 lines)

| Tier | Min Stake | APY | Unbonding | Slashing |
|------|-----------|-----|-----------|----------|
| Lite | 50K | 5% | 1 day | - |
| Spark | 500K | 8% | 7 days | 10% |
| Arc | 5M | 15% | 14 days | 20% |
| Core | 50M | 25% | 30 days | 30% |

- Fixed supply: 1.03 billion ARC, 9 decimals
- NO inflation, NO burn
- Revenue: 40% proposers, 25% verifiers, 15% observers, 20% treasury - **WIRED (v0.5.3)**
  `RoleRevenueConfig` instantiated in `NodeState.revenue_config`. Fee splits computed
  on every inference run and included in the response. `GET /economics/revenue_split`
  endpoint exposes the config and example splits.
- Bootstrap fund: 2-year linear vesting, 1-week cliff

---

## Community Worker System (v0.7.0+)

The Python gateway sidecar from v0.5.2–v0.6.x is gone. Worker
registration, work dispatch, and result collection are all built
into arc-node itself on port 9090.

### Coordinator (built into arc-node)
**File:** `crates/arc-node/src/rpc.rs`
- `/community/register` + `/community/heartbeat` + `/community/list`
- `/community/claim_work` — long-poll (30s) for whole-prompt inference jobs
- `/community/submit_work` — accept a worker-signed computation result. A raw
  `InferenceAttestation` (`0x16`) is historical evidence only and pays nothing.
  v3 payment requires a successfully mined, independently verified,
  five-of-six-authorized `CommunityInferenceReward` (`0x25`) receipt.
- `/inference/run` — smart router: prefers a community worker when
  one is online, falls back to the seed's local model
- `/worker/earnings/:address` — confirmed mined `0x25` reward receipts only;
  raw `0x16` events are not earnings

### Node Client (--community-mode)
**File:** `crates/arc-node/src/main.rs`
- Auto-register with every seed it peers with
- Long-poll `/community/claim_work` on the supported coordinator endpoint.
  Historical port-3001 compatibility is not permission for mixed-version
  operation.
- Compute locally via `model.generate()`
- Sign and submit each result; the worker is credited only when the authorized
  `CommunityInferenceReward` (`0x25`) transaction is successfully mined

---

## Transaction Types (24 total)

**File:** `crates/arc-types/src/lib.rs`

Key types for inference:
- `InferenceAttestation` - on-chain record of inference result
- `InferenceChallenge` - challenge a result with bond
- `InferenceRegister` - register as inference provider
- `InferenceCommitment` - commit result hash + bond

---

## Signature Schemes (5)

**File:** `crates/arc-crypto/src/`
- Ed25519 (primary)
- secp256k1 (ETH compat)
- BLS (aggregate signatures)
- Falcon-512 (post-quantum)
- ML-DSA (post-quantum, NIST)

---

## VM Runtime

**File:** `crates/arc-vm/src/`
- EVM (revm) - Solidity/ERC-20 compatible
- WASM (wasmer) - general compute
- Precompiles for inference operations

---

## Scripts

| Script | Purpose |
|--------|---------|
| `install-community-node.sh` | One-command node install (NO model download) |
| `arc-community-register.sh` | Legacy registration shim — only useful for v0.6.x and older nodes; v0.7.0+ self-registers |
| `arc-diagnose.sh` | 4-phase health check for stuck nodes |
| `arc-demo.sh` | End-to-end sharded inference demo |
| `arc-verify.sh` | Third-party inference verifier |
| `arc-bench.sh` | Reproducible factual benchmark |
| `arc-self-heal.sh` + `.service` + `install-self-heal.sh` | **Retired:** exits before service or process mutation. Installed legacy units remain disabled during recovery. |
| `arc-watchdog.sh` | **Retired:** exits before SSH, process, or service mutation. |
| `arc-health-check.sh` | **Retired:** a reachable process was not proof of shared-chain health. |
| Legacy rolling deployment scripts | **Retired:** v2/v3 mixed operation is rejected; use only the coordinated recovery runbook. |

---

## Current safety corrections to the historical snapshot

1. **Treat this as history, not authority.** At v0.5.3 the document reported
   the following infrastructure as wired into the runtime:
   VRF proposer selection, VRF inference committees, multi-model ShardRegistry,
   auto-sharding (compute_shard_plan), verification manager (commit-challenge),
   revenue config (fee splits), checkpoint registry, double-vote tracker,
   withholding detector, flash attention (online softmax), GPU embedding fix.
   The STARK prover is real but feature-gated (`--features stwo-prover`).
   That historical completeness claim must not substitute for current code,
   release-gate, or live receipt verification.

2. **Never perform a rolling public-validator upgrade.** v2 and v3 reject each
   other. Fully quiesce and fence every old validator, preserve and verify the
   canonical history through height H, then activate fresh v3 data paths using
   the sealed checkpoint and manifest gates. Rollback never restarts v2.

3. **NEVER download models in the install script.** Nodes join for
   consensus + TPS immediately. Models are optional for inference.

4. **The integer engine is the production path.** Candle is fallback.
   GPU (arc-gpu) embedding is FIXED but GPU path is benchmark-only (not default).

5. **Both registries coexist.** The flat `DashMap<String, ShardInfo>` in rpc.rs
   handles backward-compatible shard gossip. The multi-model `ShardRegistry`
   from distributed.rs is ALSO populated on every `/shards/announce`. Use
   `/models` and `/models/shards` for multi-model queries.

6. **Test on x86 VPS before deploying.** Mac (aarch64) works but
   Vultr x86 has different memory/timing behavior. The dd0bef8 binary
   segfaults on x86 when receiving real consensus traffic - this is
   likely caused by the DashMap type change in NodeState (community_workers
   field changes memory layout, exposing a pre-existing off-by-one in
   the consensus thread). Use the stable binary (dc662fa) for x86 seeds.
