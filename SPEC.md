# ARC Chain — L1 Specification v1.0

## Architecture

```
Agents / Users
      │
      ▼
┌─ arc-net ──────────────────────────────────────────┐
│  QUIC transport, shred propagation, tx gossip      │
└────────────┬───────────────────────────────────────┘
             ▼
┌─ arc-consensus ────────────────────────────────────┐
│  DAG block proposals, validator set, finalization   │
└────────────┬───────────────────────────────────────┘
             ▼
┌─ arc-node ─────────────────────────────────────────┐
│  Sequencer: signature verify → execute → persist    │
│  RPC API, block production loop                     │
└────────────┬───────────────────────────────────────┘
         ┌───┴───┐
         ▼       ▼
┌─ arc-state ─┐ ┌─ arc-vm ──────────────────────────┐
│ DashMap RAM  │ │ Wasmer WASM, host imports, gas    │
│ WAL + snap   │ └──────────────────────────────────┘
└──────────────┘
```

Crates: `arc-crypto`, `arc-types`, `arc-state`, `arc-vm`, `arc-mempool`,
`arc-consensus` (new), `arc-net` (new), `arc-node`, `arc-gpu`, `arc-bench`

---

## 1. Cryptography (`arc-crypto`)

### 1.1 Signatures

Two signature schemes. Ed25519 for agent/native transactions (fast, lightweight).
Secp256k1 for ETH-compatible operations (MetaMask, bridge verification).

```rust
pub enum Signature {
    Ed25519 {
        public_key: [u8; 32],
        signature: [u8; 64],
    },
    Secp256k1 {
        signature: [u8; 65], // recoverable (r, s, v)
    },
}

pub enum KeyPair {
    Ed25519(ed25519_dalek::SigningKey),
    Secp256k1(k256::ecdsa::SigningKey),
}
```

**Address derivation:**
- Ed25519: `address = BLAKE3(public_key)[0..32]`
- Secp256k1: `address = BLAKE3(uncompressed_pubkey[1..65])[0..32]`

**Batch verification:**
```rust
/// Verify N ed25519 signatures in a batch.
/// Uses multi-scalar multiplication — ~2x faster than individual verify.
pub fn batch_verify_ed25519(
    messages: &[&[u8]],
    signatures: &[&[u8; 64]],
    public_keys: &[&[u8; 32]],
) -> Result<(), CryptoError>;
```

**Dependencies:** `ed25519-dalek` (with `batch` feature), `k256`

### 1.2 BLS Aggregate Signatures (Consensus)

Validators sign blocks using BLS12-381. N validator signatures aggregate into
one 48-byte signature. Verification cost is constant regardless of validator count.

```rust
pub struct BlsSignature(pub [u8; 48]);
pub struct BlsPublicKey(pub [u8; 96]);

/// Aggregate N signatures into one.
pub fn bls_aggregate(signatures: &[BlsSignature]) -> BlsSignature;

/// Verify an aggregate signature against N public keys and one message.
pub fn bls_verify_aggregate(
    aggregate: &BlsSignature,
    public_keys: &[BlsPublicKey],
    message: &[u8],
) -> bool;
```

**Dependency:** `blst` (supranational, fastest BLS implementation)

### 1.3 Existing (unchanged)

- BLAKE3 hashing (`hash_bytes`, `batch_commit_parallel`, GPU shader)
- Pedersen commitments (`commit`, `verify`, homomorphic addition)
- Merkle trees (`build_parallel`, `prove`, `verify_proof`)
- ZK aggregate proofs

---

## 2. Data Structures (`arc-types`)

### 2.1 Transaction

```rust
pub struct Transaction {
    pub tx_type: u8,
    pub from: Address,        // 32 bytes
    pub nonce: u64,
    pub body: TxBody,
    pub hash: Hash256,        // BLAKE3("ARC-chain-tx-v1", serialized_body)
    pub signature: Signature, // NEW — cryptographic proof of authorization
    pub fee: u64,             // NEW — fee in ARC (can be 0 for settlements)
    pub gas_limit: u64,       // NEW — max gas for WASM calls
}
```

**Signing:** The signer computes `hash = BLAKE3("ARC-chain-tx-v1", tx_type || from || nonce || body || fee || gas_limit)` then signs `hash` with their private key.

**Verification:** Before execution, verify `signature` recovers to `from`. For ed25519, check `public_key` in signature hashes to `from`. For secp256k1, recover the public key from signature + hash, verify it hashes to `from`.

### 2.2 Transaction Types (9 total)

Existing 7 types unchanged. Two new types added:

```rust
pub enum TxBody {
    Transfer(TransferBody),         // 0x01
    Settle(SettleBody),             // 0x02
    Swap(SwapBody),                 // 0x03
    Escrow(EscrowBody),             // 0x04
    Stake(StakeBody),               // 0x05
    WasmCall(WasmCallBody),         // 0x06
    MultiSig(MultiSigBody),         // 0x07
    DeployContract(DeployBody),     // 0x08 — NEW
    RegisterAgent(RegisterBody),    // 0x09 — NEW
}

pub struct DeployBody {
    pub bytecode: Vec<u8>,          // WASM binary
    pub constructor_args: Vec<u8>,
    pub state_rent_deposit: u64,    // pre-pay for state storage
}

pub struct RegisterBody {
    pub agent_name: String,         // max 64 bytes
    pub capabilities: Vec<u8>,      // CBOR-encoded capability list
    pub endpoint: String,           // how to reach this agent (URL, DID, etc.)
    pub protocol: Hash256,          // optional — linked WASM contract address
    pub metadata: Vec<u8>,          // arbitrary metadata, max 1024 bytes
}
```

### 2.3 Block

```rust
pub struct Block {
    pub height: u64,
    pub timestamp: u64,             // unix millis
    pub parent_hash: Hash256,
    pub tx_root: Hash256,           // Merkle root of transaction hashes
    pub state_root: Hash256,        // Merkle root of all account states
    pub receipt_root: Hash256,      // NEW — Merkle root of receipts
    pub tx_count: u32,
    pub producer: Address,
    pub producer_signature: Signature,
    // Consensus fields
    pub round: u64,                 // NEW — DAG round number
    pub dag_parents: Vec<Hash256>,  // NEW — parent block hashes in DAG
    pub validator_aggregate: Option<BlsSignature>, // NEW — 2/3+ validator sigs
    pub shard_id: u16,              // NEW — which shard produced this block
}
```

### 2.4 Account

```rust
pub struct Account {
    pub address: Address,
    pub balance: u64,
    pub nonce: u64,
    pub code_hash: Hash256,         // zero for EOAs
    pub storage_root: Hash256,      // Merkle root of contract storage
    pub last_active: u64,           // NEW — block height of last tx (state rent)
    pub agent_info: Option<AgentInfo>, // NEW — if registered as agent
}

pub struct AgentInfo {
    pub name: String,
    pub capabilities: Vec<u8>,
    pub endpoint: String,
    pub protocol: Hash256,
    pub registered_at: u64,
}
```

---

## 3. State Model (`arc-state`)

### 3.1 In-Memory State (existing, enhanced)

```rust
pub struct StateDB {
    accounts: DashMap<Address, Account>,
    storage: DashMap<Address, DashMap<Hash256, Vec<u8>>>,
    blocks: DashMap<u64, Block>,
    receipts: DashMap<Hash256, TxReceipt>,
    agents: DashMap<Address, AgentInfo>,  // NEW — agent registry
    contracts: DashMap<Address, Vec<u8>>, // NEW — WASM bytecode cache
    height: RwLock<u64>,

    // Persistence
    wal: WalWriter,                       // NEW
    snapshot_trigger: AtomicU64,          // NEW — blocks since last snapshot
}
```

### 3.2 Write-Ahead Log (WAL)

Every state mutation is journaled to an append-only file BEFORE acknowledging
the transaction. Sequential writes only — never seeks, never reads during execution.

```rust
pub struct WalEntry {
    pub block_height: u64,
    pub sequence: u64,
    pub op: WalOp,
    pub checksum: u32,          // CRC32 for corruption detection
}

pub enum WalOp {
    SetAccount(Address, Account),
    SetStorage(Address, Hash256, Vec<u8>),
    DeleteStorage(Address, Hash256),
    SetBlock(u64, Block),
    SetReceipt(Hash256, TxReceipt),
    SetAgent(Address, AgentInfo),
    SetContract(Address, Vec<u8>),
    Checkpoint(Hash256),        // state root at this point
}
```

**WAL is write-only during execution.** The async writer batches entries and
flushes to SSD using `O_DIRECT` + `fdatasync`. Execution thread never blocks.

```rust
pub struct WalWriter {
    sender: crossbeam::channel::Sender<WalEntry>,
    // Background thread: receives entries, writes to file, fsyncs every 100ms
}

impl WalWriter {
    /// Non-blocking. Sends entry to background writer.
    pub fn append(&self, entry: WalEntry);

    /// Blocks until all pending entries are fsynced. Called at block boundaries.
    pub fn sync(&self);
}
```

### 3.3 Snapshots

Full state dump every N blocks (configurable, default 10,000). Written as a
single file: sorted accounts + storage + metadata. Used for:
- Fast node bootstrap (download snapshot + replay recent WAL)
- Crash recovery (load snapshot + replay WAL from checkpoint)

```rust
pub struct Snapshot {
    pub block_height: u64,
    pub state_root: Hash256,
    pub accounts: Vec<(Address, Account)>,  // sorted by address
    pub storage: Vec<(Address, Vec<(Hash256, Vec<u8>)>)>,
    pub agents: Vec<(Address, AgentInfo)>,
    pub contracts: Vec<(Address, Vec<u8>)>,
}
```

Format: LZ4-compressed bincode. Typical size: ~2 bytes per account field.
1M accounts ≈ 200 MB compressed snapshot.

### 3.4 Crash Recovery

1. Load latest snapshot
2. Replay WAL entries from snapshot's checkpoint forward
3. Verify final state root matches WAL's last checkpoint
4. Resume operation

### 3.5 GPU-Resident State Cache

Accounts can be cached in GPU memory for high-throughput batch compute shader
access (BlockSTM parallel execution, batch Merkle hashing). The cache uses a
dual-write architecture:

- **CPU-side DashMap mirror** — serves all individual reads at ~15M lookups/sec
- **GPU buffer (wgpu)** — 128-byte aligned `GpuAccountRepr` structs accessible
  by compute shaders at unified memory bandwidth (~2 TB/s on Apple Silicon)
- **Lazy flush** — `flush_to_gpu()` batch-writes dirty accounts once per block,
  avoiding per-account wgpu overhead during transaction execution

```rust
pub struct GpuStateCache {
    gpu_buffer: Arc<GpuAccountBuffer>,   // wgpu buffer (unified/managed/CPU-only)
    cpu_mirror: DashMap<[u8;32], CachedAccount>,  // fast individual reads
    slot_map: DashMap<[u8;32], AccessMeta>,       // GPU slot tracking
    warm: DashMap<[u8;32], CachedAccount>,        // overflow (CPU RAM)
}
```

**Memory models:**
- `UnifiedMetal` — Apple Silicon: zero-copy, CPU and GPU share physical pages
- `ManagedDiscrete` — NVIDIA/AMD: staging buffer with explicit sync
- `CpuOnly` — fallback when no GPU available

**Eviction:** LRU or LFU policy, configurable. Evicted accounts move to warm
(CPU) tier. Auto-promotion on warm hit.

**Security:** `secure_shutdown()` zeros all GPU memory before release.

### 3.6 State Rent

Accounts not touched in `RENT_EPOCH` blocks (default: 100,000 ≈ ~3 hours at
10 blocks/sec) get evicted from RAM to cold storage (SSD). Re-activation
costs a `RENT_REVIVAL_FEE` (small, covers SSD read cost).

```rust
const RENT_EPOCH: u64 = 100_000;
const RENT_REVIVAL_FEE: u64 = 100; // in smallest ARC denomination
const RENT_PER_BYTE_PER_EPOCH: u64 = 1; // contract storage cost
```

---

## 4. Execution Pipeline (`arc-node`)

### 4.1 Transaction Lifecycle

```
1. Receive tx (via RPC or gossip)
2. Validate: check format, fee >= minimum, gas_limit <= max
3. Verify signature (batch ed25519 or secp256k1 recover)
4. Insert into mempool (deduplicate by hash)
5. Sequencer: drain mempool, order by (shard, sender, nonce)
6. Execute: parallel sender-sharded execution
7. Produce block: Merkle roots, sign, broadcast
8. Persist: WAL append (async, non-blocking)
9. Consensus: DAG proposal, collect validator votes
10. Finalize: 2/3+ validators signed → block is final
```

### 4.2 Execution Modes

The pipeline supports three execution modes:

| Mode | Description | Use Case |
|------|-------------|----------|
| `Sequential` | Single-threaded state execution | Debugging, correctness verification |
| `BlockStm` | Optimistic parallel execution (sender-sharded) | Production default |
| `GpuResident` | BlockSTM + GPU-resident state cache | Maximum throughput with GPU acceleration |

In `GpuResident` mode, the pipeline prefetches block accounts into the GPU cache,
runs BlockSTM execution with the GPU-backed state, then flushes dirty state back
to the GPU buffer for compute shader access.

### 4.3 Signature Verification Pipeline

Signatures are verified in a dedicated thread pool BEFORE execution, pipelined
with the previous block's execution.

```
Block N execution ──────────────────► Block N persist
                    (parallel)
Block N+1 sig verify ──────────────► Block N+1 ready for execution
```

```rust
/// Verify all signatures in a batch of transactions.
/// Returns indices of invalid transactions (to be rejected).
pub fn verify_batch(txs: &[Transaction]) -> Vec<usize> {
    // Partition by signature type
    let (ed25519_txs, secp_txs) = partition_by_sig_type(txs);

    // Batch verify ed25519 (multi-scalar multiplication, ~2x single)
    let ed_invalid = batch_verify_ed25519(&ed25519_txs);

    // Parallel verify secp256k1 (no batch mode, but parallelizable)
    let secp_invalid: Vec<usize> = secp_txs.par_iter()
        .filter(|tx| !verify_secp256k1(tx))
        .map(|tx| tx.index)
        .collect();

    [ed_invalid, secp_invalid].concat()
}
```

### 4.3 WASM Execution (wired into state)

For `WasmCall` and `DeployContract` transactions:

```rust
impl StateDB {
    fn execute_wasm_call(&self, tx: &Transaction, body: &WasmCallBody) -> TxReceipt {
        // 1. Load contract bytecode from cache (RAM)
        let bytecode = self.contracts.get(&body.contract)
            .ok_or(ExecError::ContractNotFound)?;

        // 2. Create VM instance with host imports
        let vm = ArcVM::new();
        let module = vm.compile(&bytecode)?;

        // 3. Set up host environment (state reads/writes)
        let env = HostEnv {
            caller: tx.from,
            value: body.value,
            gas_limit: tx.gas_limit,
            state: self,  // read/write access to state
        };

        // 4. Execute
        let result = vm.execute_with_env(&module, &body.function, &body.calldata, env)?;

        // 5. Apply storage mutations from host env
        env.commit_storage_changes(self);

        TxReceipt { success: true, gas_used: result.gas_used, .. }
    }
}
```

**Host imports** (added to arc-vm):

```rust
// State access
fn storage_get(key_ptr: u32, key_len: u32, val_ptr: u32) -> u32;
fn storage_set(key_ptr: u32, key_len: u32, val_ptr: u32, val_len: u32);
fn storage_delete(key_ptr: u32, key_len: u32);

// Account access
fn balance_of(addr_ptr: u32) -> u64;
fn transfer(to_ptr: u32, amount: u64) -> u32; // 0 = success

// Context
fn caller(out_ptr: u32);          // write caller address to memory
fn self_address(out_ptr: u32);    // write contract address to memory
fn block_height() -> u64;
fn block_timestamp() -> u64;
fn tx_value() -> u64;             // ARC sent with this call

// Gas
fn gas_remaining() -> u64;

// Logging
fn emit_event(topic_ptr: u32, topic_len: u32, data_ptr: u32, data_len: u32);
```

### 4.4 Gas Metering

Use Wasmer's **metering middleware** for automatic instruction-level gas counting.
No reliance on contracts calling `use_gas()` themselves.

```rust
use wasmer_middlewares::metering::{get_remaining_points, MeteringPoints, set_remaining_points};

fn execute_with_gas(module: &Module, gas_limit: u64) -> ExecutionResult {
    set_remaining_points(&instance, gas_limit);
    let result = instance.call(function, args);
    let gas_used = gas_limit - get_remaining_points(&instance);
    // ...
}
```

### 4.5 Fee Model

```
base_fee = 1 ARC (minimum for any transaction)
gas_price = 0.001 ARC per gas unit (for WASM calls)
total_fee = base_fee + (gas_used * gas_price)
```

Fee distribution per block:
- 50% burned (deflationary pressure)
- 30% to block producer
- 20% to staking rewards pool

Settlement transactions (`Settle` type) have ZERO base fee — agents settle
for free. This is the core value proposition. Fee revenue comes from transfers,
WASM calls, and contract deployments.

---

## 5. Consensus (`arc-consensus`)

### 5.1 Overview

DAG-based consensus inspired by Mysticeti (Sui). Multiple validators propose
blocks in parallel. Blocks reference parents in the DAG. Commit rule: a block
is final when it is referenced by 2f+1 validators in a later round.

### 5.2 Validator Set

```rust
pub struct ValidatorSet {
    pub validators: Vec<Validator>,
    pub total_stake: u64,
    pub epoch: u64,             // changes when stake changes
    pub quorum: u64,            // 2/3 of total_stake
}

pub struct Validator {
    pub address: Address,
    pub bls_public_key: BlsPublicKey,
    pub ed25519_public_key: [u8; 32],
    pub stake: u64,             // ARC staked on ETH (bridged proof)
    pub tier: StakeTier,
    pub shard_assignment: u16,
}

pub enum StakeTier {
    Spark,  // 500K ARC — can vote, cannot produce blocks
    Arc,    // 5M ARC — can produce blocks
    Core,   // 50M ARC — priority producer, governance
}
```

Only `Arc` and `Core` tier validators produce blocks. `Spark` tier participates
in voting/attestation only.

### 5.3 DAG Structure

```rust
pub struct DagBlock {
    pub author: Address,
    pub round: u64,
    pub parents: Vec<Hash256>,  // blocks from round-1 that this references
    pub transactions: Vec<Hash256>, // transaction hashes included
    pub timestamp: u64,
    pub signature: BlsSignature,
}
```

**Round progression:**
1. Each validator proposes ONE block per round
2. Block references ≥2f+1 blocks from previous round (its parents)
3. A round completes when ≥2f+1 validators have proposed

**Commit rule (Mysticeti-style):**
A block B in round R is committed when:
- A block C in round R+1 references B
- Block C is itself referenced by ≥2f+1 blocks in round R+2

This gives 3-round latency to commit. At ~150ms per round = ~450ms finality.

### 5.4 Fast Path

Transactions where `from` is owned by a single address (most settlements)
skip full consensus ordering. The assigned shard validator includes them
directly. Only transactions that touch MULTIPLE accounts across shards
need full DAG ordering.

```rust
fn needs_full_consensus(tx: &Transaction) -> bool {
    match &tx.body {
        // Transfers/settles between two accounts — only if cross-shard
        TxBody::Transfer(b) => shard_of(tx.from) != shard_of(b.to),
        TxBody::Settle(b) => shard_of(tx.from) != shard_of(b.agent_id),
        // WASM calls always need ordering (may touch any state)
        TxBody::WasmCall(_) => true,
        // Everything else — check if cross-shard
        _ => false, // default to fast path
    }
}
```

### 5.5 Shard Assignment

Addresses map to shards via the first N bits of the address:

```rust
fn shard_of(address: &Address, num_shards: u16) -> u16 {
    let prefix = u16::from_be_bytes([address[0], address[1]]);
    prefix % num_shards
}
```

Each shard has a consensus group of validators assigned to it. Validators
are assigned to shards proportional to their stake.

### 5.6 Cross-Shard Transactions

When a transaction spans two shards (e.g., transfer from shard A to shard B):

1. Shard A validates + executes the debit (locks sender funds)
2. Shard A produces a **cross-shard receipt** with Merkle proof
3. Shard B receives the receipt, verifies the proof, executes the credit
4. Total latency: ~2 rounds extra (~300ms)

```rust
pub struct CrossShardReceipt {
    pub source_shard: u16,
    pub source_block: Hash256,
    pub tx_hash: Hash256,
    pub debit_proof: MerkleProof,   // proves debit was executed in source
    pub amount: u64,
    pub recipient: Address,
}
```

---

## 6. Networking (`arc-net`)

### 6.1 Transport: QUIC

All validator-to-validator communication uses QUIC (RFC 9000).

- Multiplexed streams without head-of-line blocking
- Built-in TLS 1.3 encryption
- Connection migration (validator IP changes don't drop connections)
- Per-stream flow control

**Dependency:** `quinn` (Rust QUIC implementation)

### 6.2 Block Propagation: Shreds

Blocks are split into shreds with Reed-Solomon erasure coding. Propagated
through a tree structure (Turbine-inspired).

```rust
pub struct Shred {
    pub block_hash: Hash256,
    pub block_height: u64,
    pub shard_id: u16,
    pub shred_index: u16,       // index within the block
    pub total_shreds: u16,      // total data shreds
    pub coding_shreds: u16,     // total coding (parity) shreds
    pub data: Vec<u8>,          // max 1280 bytes (fits in one UDP/QUIC packet)
    pub signature: Signature,   // producer's signature
}
```

**Erasure coding:** 32 data shreds + 32 coding shreds per block.
Any 32 of 64 shreds can reconstruct the full block (50% loss tolerance).

**Propagation tree:**
- Root: block producer
- Fan-out: 200 (each node forwards to 200 peers)
- Depth: log_200(N) — for 10,000 validators, depth = 2
- Latency: ~100-200ms global propagation

### 6.3 Transaction Gossip

Unconfirmed transactions propagate via gossip. Each node maintains a
bloom filter of known transaction hashes to avoid re-sending.

```rust
pub struct TxGossipMessage {
    pub transactions: Vec<Transaction>,  // batched, max 100 per message
    pub sender: Address,
}
```

### 6.4 Validator Discovery

Validators discover each other through a seed node list + gossip protocol.

```rust
pub struct PeerInfo {
    pub address: Address,
    pub quic_addr: SocketAddr,
    pub stake: u64,
    pub shard: u16,
    pub last_seen: u64,
}
```

### 6.5 Stake-Weighted QoS

During congestion, traffic from higher-staked validators gets priority.
Prevents DDoS by rate-limiting unstaked connections.

```
Priority = stake_amount / total_stake
Bandwidth allocation = Priority * total_bandwidth
```

---

## 7. Token Economics

### 7.1 ARC on Ethereum (existing)

- ERC-20 at `0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499`
- 1.03B total supply, 2% buy/sell tax
- Upgradeable proxy with modular tax system

### 7.2 Staking Contract (Solidity, new)

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IARCStaking {
    enum Tier { None, Spark, Arc, Core }

    struct StakeInfo {
        uint256 amount;
        uint256 stakedAt;
        uint256 lastClaimed;
        Tier tier;
        uint256 pendingUnstake;
        uint256 unstakeRequestedAt;
    }

    // Tier thresholds
    function SPARK_MIN() external pure returns (uint256); // 500_000e18
    function ARC_MIN() external pure returns (uint256);   // 5_000_000e18
    function CORE_MIN() external pure returns (uint256);  // 50_000_000e18

    // APY (basis points)
    function SPARK_APY() external pure returns (uint256);  // 800 = 8%
    function ARC_APY() external pure returns (uint256);    // 1500 = 15%
    function CORE_APY() external pure returns (uint256);   // 2500 = 25%

    function UNSTAKE_COOLDOWN() external pure returns (uint256); // 7 days

    function stake(uint256 amount) external;
    function requestUnstake() external;
    function withdraw() external;
    function claimRewards() external;
    function pendingRewards(address staker) external view returns (uint256);
    function getStakeInfo(address staker) external view returns (StakeInfo memory);

    // TPS rewards (called by coordinator/oracle)
    function reportTPS(address[] calldata nodes, uint256[] calldata tps) external;

    // Fund rewards pool
    function fundRewards(uint256 amount) external;

    event Staked(address indexed staker, uint256 amount, Tier tier);
    event UnstakeRequested(address indexed staker, uint256 amount);
    event Withdrawn(address indexed staker, uint256 amount);
    event RewardsClaimed(address indexed staker, uint256 amount);
}
```

### 7.3 Tax Revenue Split (new TaxModule)

Deploy a new tax module that splits revenue:

```solidity
interface ITaxSplitter {
    // Split: 50% staking rewards, 30% treasury, 20% liquidity
    function STAKING_BPS() external pure returns (uint256);  // 5000
    function TREASURY_BPS() external pure returns (uint256); // 3000
    function LIQUIDITY_BPS() external pure returns (uint256); // 2000
}
```

### 7.4 ARC Chain Native Fees

On the L1 itself:
- Settlement transactions: **FREE** (zero fee — the killer feature)
- Transfers: 1 ARC base fee
- WASM calls: 1 ARC + gas
- Contract deploy: 100 ARC + state rent deposit
- Agent registration: 10 ARC

Fee distribution: 50% burned, 30% block producer, 20% rewards.

### 7.5 Block Rewards

Each block producer receives:
```
block_reward = base_reward + sum(tx_fees) * 0.30
base_reward = 10 ARC per block (during bootstrap phase, first 2 years)
```

Bootstrap rewards come from a genesis allocation (separate from circulating supply).
After 2 years, block rewards transition to fee-only (deflationary).

---

## 8. Bridge

### 8.1 ETH → ARC Chain

1. User calls `ArcBridge.lock(amount)` on Ethereum
2. Bridge contract locks $ARC tokens
3. Bridge relayer observes the lock event
4. Relayer submits a mint transaction on ARC Chain with Merkle proof of the lock
5. ARC Chain verifies the Ethereum Merkle proof and credits native ARC balance

### 8.2 ARC Chain → ETH

1. User submits a burn transaction on ARC Chain
2. Block containing the burn is finalized (450ms)
3. State root of that block is posted to ETH (by the sequencer/relayer)
4. User calls `ArcBridge.unlock(amount, proof)` on Ethereum
5. Bridge contract verifies Merkle proof against posted state root
6. $ARC tokens are unlocked to user

### 8.3 State Root Commitments

The sequencer posts ARC Chain state roots to Ethereum periodically
(every N blocks or every M seconds). These commitments anchor ARC Chain
security to Ethereum — anyone can verify ARC state against ETH.

```solidity
interface IArcStateRoot {
    function commitStateRoot(
        uint256 blockHeight,
        bytes32 stateRoot,
        bytes32 txRoot,
        bytes signature        // sequencer BLS signature
    ) external;

    function verifyProof(
        uint256 blockHeight,
        bytes32 leaf,
        bytes32[] calldata proof,
        uint256 index
    ) external view returns (bool);

    function latestBlockHeight() external view returns (uint256);
    function stateRootAt(uint256 height) external view returns (bytes32);
}
```

---

## 9. Agent Protocol Layer

### 9.1 Agent Registration

Agents register via `RegisterAgent` transaction. Registration creates an
on-chain identity with discoverable capabilities.

### 9.2 Settlement Protocol

Agent settlements use the existing `Settle` transaction type:

```
POST /v1/settle (REST API — wrapper for Settle tx)
{
    "from": "agent_address",
    "to": "agent_address",
    "service_hash": "0x...",   // BLAKE3 hash of service description
    "amount": 1000,
    "usage_units": 42,
    "signature": "0x..."
}
```

**Zero fee.** Agents settle for free. This is the core value proposition.

### 9.3 Protocol Factory

Deploy WASM contracts that define interaction protocols:

```rust
// Example: SimpleAuction protocol
#[no_mangle]
pub fn bid(amount: u64) {
    let caller = arc::caller();
    let current_high = arc::storage_get(b"high_bid");
    assert!(amount > current_high);
    arc::storage_set(b"high_bid", &amount.to_le_bytes());
    arc::storage_set(b"high_bidder", &caller);
    arc::emit_event(b"new_bid", &[caller, amount]);
}

#[no_mangle]
pub fn settle() {
    let seller = arc::storage_get(b"seller");
    let winner = arc::storage_get(b"high_bidder");
    let amount = arc::storage_get(b"high_bid");
    arc::transfer(&seller, amount);
    arc::emit_event(b"settled", &[winner, amount]);
}
```

---

## 10. RPC API (`arc-node`)

### 10.1 Full Node API

Existing endpoints (updated):

```
GET  /health
GET  /info                          — chain info, validator status
GET  /block/{height}                — block by height
GET  /blocks?from=&to=&limit=       — paginated blocks
GET  /account/{address}             — account state
GET  /account/{address}/txs         — tx history
POST /tx/submit                     — submit ANY transaction type (signed)
POST /tx/submit_batch               — batch submission
GET  /tx/{hash}                     — receipt
GET  /tx/{hash}/proof               — Merkle inclusion proof
GET  /stats                         — chain statistics
```

New endpoints:

```
GET  /validators                    — current validator set
GET  /validators/{address}          — validator info
GET  /shard/{id}                    — shard info
GET  /agents                        — registered agents (paginated)
GET  /agents/{address}              — agent info
GET  /agents/search?capability=     — search by capability
POST /v1/settle                     — agent settlement (convenience wrapper)
GET  /bridge/status                 — bridge health, pending transfers
WS   /ws/blocks                     — stream new blocks
WS   /ws/txs                        — stream new transactions
```

---

## 11. Implementation Plan

All components build in parallel. Interfaces defined above are the contracts.

### Stream 1: Cryptography + Signatures
- Add ed25519-dalek (batch feature) + k256 to arc-crypto
- Implement Signature enum, KeyPair, sign/verify
- Add signature field to Transaction
- Wire signature verification into execution pipeline
- Add batch verify benchmark
- **Deliverable: re-benchmark TPS with signatures**

### Stream 2: Persistence
- Implement WalWriter (background thread, crossbeam channel)
- Implement WalEntry serialization (bincode + CRC32)
- Implement Snapshot write/read (LZ4-compressed bincode)
- Implement crash recovery (snapshot + WAL replay)
- Wire into StateDB (append after execute, sync at block boundary)
- **Deliverable: re-benchmark TPS with persistence**

### Stream 3: WASM VM Integration
- Add host imports (storage_get/set, balance_of, transfer, caller, etc.)
- Add Wasmer metering middleware for gas
- Wire WasmCall into execute_tx
- Wire DeployContract into execute_tx
- Create simple test contract (counter, token)
- **Deliverable: deploy + call WASM contract end-to-end**

### Stream 4: Consensus
- Define DagBlock, ValidatorSet structures
- Implement round-based DAG proposal
- Implement commit rule (2f+1 references across 3 rounds)
- Implement validator set management (from staking proofs)
- Implement fast path for owned-address transactions
- Wire into arc-node block production
- **Deliverable: multi-node consensus with measured overhead**

### Stream 5: Networking
- QUIC transport using quinn
- Shred encoding/decoding with Reed-Solomon
- Tree propagation protocol
- Transaction gossip with bloom dedup
- Validator discovery
- **Deliverable: blocks propagate across 4+ nodes**

### Stream 6: Solidity Contracts
- ARCStaking.sol (tiers, APY, cooldown, TPS rewards)
- TaxSplitter.sol (50/30/20 revenue split)
- ArcBridge.sol (lock/unlock with Merkle proof verification)
- ArcStateRoot.sol (state root commitments)
- Tests in Foundry
- **Deliverable: deploy to ETH testnet, verify staking flow**

### Stream 7: Agent SDK + API
- Settlement REST API wrapper
- Python SDK (pip install arc-chain)
- TypeScript SDK (npm install @arc-chain/sdk)
- Agent registration flow
- Example: two agents settling a code review payment
- **Deliverable: agent settles via SDK in 3 lines of code**
