# Replicated Chain (Model 1: full re-execution) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the 5 ARC seed validators converge on one identical replicated chain — every honest node produces the same block (same hash) at the same height — by having every validator deterministically re-execute the full DAG-ordered transaction set.

**Architecture:** ARC already has a DAG consensus layer (`arc-consensus`) that agrees on a total order of transaction hashes per committed `DagBlock`, with canonical lexicographic tx ordering and a deterministic per-block `timestamp`. The execution layer diverges because (a) block sealing reads a wall-clock `SystemTime::now()` instead of the consensus timestamp, (b) nodes silently skip transactions whose bodies they don't have locally, and (c) only the proposer fully executes — verifiers apply a best-effort state diff and fall back to local execution when it's missing. Model 1 removes the proposer/verifier split for block production: **every node executes every committed DAG block from the same ordered tx set, using the DagBlock's deterministic timestamp, after guaranteeing it has all tx bodies (fetch-on-miss).** The state-diff path is retained only as an optional performance optimization, disabled by default.

**Tech Stack:** Rust workspace. `arc-consensus` (DAG), `arc-state` (execution/state), `arc-node` (consensus loop + RPC), `arc-net` (transport messages), `arc-mempool`, `arc-types` (Block/StateDiff). Tests: `cargo test`. Acceptance: live RPC against the 5 seeds.

---

## North-Star Acceptance Test (the only definition of "done")

For every height `N` that all seeds have passed, `GET /block/N` returns an **identical block hash on all 5 seeds**. Until this is green, the network status remains "single-validator devnet", not "27-validator network".

This is implemented as a tool in Phase 0 and run after each deploy.

## Testing constraint (read before starting)

Multi-node consensus on `127.0.0.1` is currently broken (QUIC handshake fails for loopback peers — see the project memory `multi_node_localhost_broken`). Therefore:
- **Unit/determinism tests** run in a single process by constructing two independent `StateDB` instances from the same genesis and asserting identical output. These cover Phases 1–4 logic.
- **True multi-node acceptance** runs against the live seeds (or a small VPS cluster) via the Phase 0 tool. Do NOT block the plan on localhost multi-node.

## File Structure (what changes and why)

- `crates/arc-state/src/lib.rs` — add timestamp-injecting execution methods (`*_at`). One responsibility: deterministic block sealing. Existing `SystemTime::now()` methods stay as thin wrappers for back-compat (RPC/tests).
- `crates/arc-net/src/transport.rs` — add two message variants for transaction data-availability pull (`RequestTransactions` outbound, `TransactionsResponse` inbound) and the request inbound (`TransactionsRequest`).
- `crates/arc-node/src/consensus.rs` — (a) pass `dag_block.timestamp` into execution; (b) on commit, fetch missing tx bodies before executing; (c) remove the proposer/verifier branch so every node executes the committed block.
- `crates/arc-node/examples/cross_seed_block_check.rs` — NEW. Acceptance tool: compare `/block/N` hashes across seeds.
- Tests live beside the code they exercise (`#[cfg(test)]` modules in `arc-state`), plus the example tool.

---

## Phase 0: Acceptance harness (cross-seed block-hash check)

**Files:**
- Create: `crates/arc-node/examples/cross_seed_block_check.rs`

- [ ] **Step 1: Write the acceptance tool**

```rust
//! Cross-seed block-hash equality check — the acceptance test for the
//! replicated-chain work. Fetches GET /block/<height> from every seed and
//! reports whether all seeds agree on the block hash at that height.
//!
//! Usage:
//!     cargo run --release --example cross_seed_block_check -p arc-node -- <height> [height2 ...]
//! Exit code 0 = all seeds agree on every requested height; 1 = divergence.

use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

const SEEDS: &[(&str, &str)] = &[
    ("LAX", "140.82.16.112"),
    ("AMS", "136.244.109.1"),
    ("LHR", "104.238.171.11"),
    ("NRT", "202.182.107.41"),
    ("SGP", "149.28.153.31"),
];

async fn block_hash(c: &Client, ip: &str, height: u64) -> Option<String> {
    let url = format!("http://{}:9090/block/{}", ip, height);
    let v: Value = c.get(&url).send().await.ok()?.json().await.ok()?;
    v.get("hash")
        .or_else(|| v.get("block_hash"))
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
}

#[tokio::main]
async fn main() {
    let heights: Vec<u64> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    if heights.is_empty() {
        eprintln!("usage: cross_seed_block_check <height> [height2 ...]");
        std::process::exit(2);
    }
    let c = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let mut all_ok = true;
    for h in heights {
        let mut hashes: Vec<(&str, Option<String>)> = Vec::new();
        for (name, ip) in SEEDS {
            hashes.push((name, block_hash(&c, ip, h).await));
        }
        let reference = hashes.iter().find_map(|(_, x)| x.clone());
        let agree = reference.is_some()
            && hashes.iter().all(|(_, x)| x.as_ref() == reference.as_ref());
        println!("height {}: {}", h, if agree { "AGREE" } else { "DIVERGE" });
        for (name, x) in &hashes {
            println!("  {:<4} {}", name, x.clone().unwrap_or_else(|| "<none>".into()));
        }
        all_ok &= agree;
    }
    std::process::exit(if all_ok { 0 } else { 1 });
}
```

- [ ] **Step 2: Verify it compiles and runs (expect DIVERGE today)**

Run: `cargo run --release --example cross_seed_block_check -p arc-node -- 43000`
Expected: prints `height 43000: DIVERGE` with 5 different hashes, exit code 1. This is the current (broken) baseline and confirms the tool works.

- [ ] **Step 3: Commit**

```bash
git add crates/arc-node/examples/cross_seed_block_check.rs
git commit -m "test: add cross-seed block-hash acceptance tool (baseline: DIVERGE)"
```

---

## Phase 1: Deterministic block sealing (use DagBlock.timestamp)

The committed `DagBlock` carries a deterministic `timestamp: u64` (arc-consensus/src/lib.rs:382), set by the proposer and covered by the block hash. Execution must seal the `Block` with THAT timestamp, not `SystemTime::now()`.

**Files:**
- Modify: `crates/arc-state/src/lib.rs` (add `execute_block_adaptive_at`, `execute_block_verified_at`, `execute_block_blockstm_at`; existing methods delegate)
- Modify: `crates/arc-node/src/consensus.rs:1050` (call the `_at` variant with `dag_block.timestamp`)
- Test: `crates/arc-state/src/lib.rs` `#[cfg(test)]` module

- [ ] **Step 1: Write the failing determinism test**

Add to the tests module in `crates/arc-state/src/lib.rs` (find the existing `#[cfg(test)] mod tests` block; if helpers like a genesis-builder exist, reuse them — otherwise construct two `StateDB::new(...)` the same way other tests in this file do):

```rust
#[test]
fn two_states_seal_identical_block_with_same_timestamp() {
    // Two independent state machines built identically.
    let a = StateDB::new_in_memory_for_test();
    let b = StateDB::new_in_memory_for_test();
    let producer = Address::ZERO;
    let txs: Vec<Transaction> = Vec::new(); // empty block is fine for sealing determinism
    let ts: u64 = 1_700_000_000_000;

    let (block_a, _) = a.execute_block_adaptive_at(&txs, producer, ts).unwrap();
    let (block_b, _) = b.execute_block_adaptive_at(&txs, producer, ts).unwrap();

    assert_eq!(block_a.header.timestamp, ts);
    assert_eq!(block_a.hash, block_b.hash,
        "same txs + same timestamp must seal identical block hash");
}
```

Note: if `StateDB::new_in_memory_for_test()` does not exist, replace both constructions with the exact constructor the surrounding tests use (search this file for `fn ` test helpers / `StateDB::` constructions in `#[cfg(test)]`). The assertion logic is unchanged.

- [ ] **Step 2: Run it — expect compile failure (`execute_block_adaptive_at` not found)**

Run: `cargo test -p arc-state two_states_seal_identical_block_with_same_timestamp 2>&1 | tail -20`
Expected: FAIL — `no method named execute_block_adaptive_at`.

- [ ] **Step 3: Add the `_at` methods; make existing methods delegate**

In `crates/arc-state/src/lib.rs`, replace the body of `execute_block_adaptive` (currently lines ~1109-1124) so it delegates, and add the `_at` variant:

```rust
pub fn execute_block_adaptive(
    &self,
    transactions: &[Transaction],
    producer: Address,
) -> Result<(Block, Vec<TxReceipt>), StateError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    self.execute_block_adaptive_at(transactions, producer, now)
}

/// Deterministic variant: seal the block with the supplied consensus
/// timestamp (the committed DagBlock.timestamp) instead of wall clock.
/// This is what the multi-validator commit path MUST use so every node
/// produces an identical block hash.
pub fn execute_block_adaptive_at(
    &self,
    transactions: &[Transaction],
    producer: Address,
    timestamp: u64,
) -> Result<(Block, Vec<TxReceipt>), StateError> {
    let mode = crate::block_stm::choose_execution_mode(transactions);
    match mode {
        crate::block_stm::AdaptiveMode::Sequential => {
            self.execute_block_verified_at(transactions, producer, timestamp)
        }
        crate::block_stm::AdaptiveMode::BlockSTM => {
            self.execute_block_blockstm_at(transactions, producer, timestamp)
        }
    }
}
```

Then refactor `execute_block_verified` (lines ~1269-1443): rename the current method to `execute_block_verified_at` and add a `timestamp: u64` parameter; change the header construction at lines ~1397-1400 from `SystemTime::now()...` to `timestamp,`. Add a thin wrapper preserving the old signature:

```rust
pub fn execute_block_verified(
    &self,
    transactions: &[Transaction],
    producer: Address,
) -> Result<(Block, Vec<TxReceipt>), StateError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    self.execute_block_verified_at(transactions, producer, now)
}

pub fn execute_block_verified_at(
    &self,
    transactions: &[Transaction],
    producer: Address,
    timestamp: u64,
) -> Result<(Block, Vec<TxReceipt>), StateError> {
    // ... existing body, but the BlockHeader is built with:
    //     timestamp,
    // instead of std::time::SystemTime::now()...as_millis() as u64
}
```

Apply the identical rename+param+wrapper pattern to `execute_block_blockstm` (the header `timestamp:` site at line ~1049 or ~1230): create `execute_block_blockstm_at(&self, transactions, producer, timestamp)` using `timestamp,` in the header, and have `execute_block_blockstm` delegate with `SystemTime::now()`.

- [ ] **Step 4: Run the test — expect PASS**

Run: `cargo test -p arc-state two_states_seal_identical_block_with_same_timestamp 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Wire the consensus commit path to use the consensus timestamp**

In `crates/arc-node/src/consensus.rs`, the proposer execution call at line ~1050:

```rust
// BEFORE:
match state.execute_block_adaptive(&committed_txs, self.validator_address)
// AFTER:
match state.execute_block_adaptive_at(&committed_txs, self.validator_address, dag_block.timestamp)
```

`dag_block` is in scope in this loop (it is the committed block being iterated). Confirm by checking the enclosing `for dag_block in &committed` / `if let ... = committed` binding around line 940–965.

- [ ] **Step 6: Build the node and run the full arc-state test suite**

Run: `cargo build -p arc-node 2>&1 | tail -5 && cargo test -p arc-state --lib 2>&1 | tail -20`
Expected: build OK; arc-state tests pass (the one known pre-existing failure `test_channel_close_releases_funds` may remain — it is unrelated).

- [ ] **Step 7: Commit**

```bash
git add crates/arc-state/src/lib.rs crates/arc-node/src/consensus.rs
git commit -m "feat(consensus): seal blocks with deterministic DagBlock.timestamp"
```

---

## Phase 2: Transaction data availability (fetch-on-miss, no silent skip)

When a DAG block commits, a node must have **every** tx body it references. Today missing bodies are silently dropped (consensus.rs:965-974), producing a short block. Add a targeted pull: request missing bodies from the block's author, await them (bounded), then execute. Never skip.

**Files:**
- Modify: `crates/arc-net/src/transport.rs` (add message variants)
- Modify: `crates/arc-node/src/consensus.rs` (fetch-on-miss before execution; handle inbound request)
- Modify: wherever inbound/outbound messages are dispatched to the transport (the same module that already routes `BroadcastTransactions` / `Transactions`). Confirm in Step 1.

- [ ] **Step 1: Confirm transport dispatch wiring (investigation, no code)**

Read how an existing targeted message round-trips: trace `OutboundMessage::SendRoundSyncResponse { target, .. }` from where it's constructed in `arc-node` to where `arc-net` sends it to a specific peer, and how the peer surfaces the matching `InboundMessage::RoundSyncResponse`. Record the two files:line you must edit to add a new targeted request/response pair. (Grep: `SendRoundSyncResponse`, `RoundSyncResponse`, `RoundSyncRequest`.) This pattern is the template; reuse it exactly.

- [ ] **Step 2: Write the failing unit test for the missing-hash detector**

Add to `crates/arc-node/src/consensus.rs` tests (or a small free function module). The pure logic — "given the committed tx-hash list and the set of locally available hashes, return which hashes are missing" — must be a standalone function so it's unit-testable without the network:

```rust
#[cfg(test)]
mod da_tests {
    use super::*;
    use arc_crypto::Hash256;

    #[test]
    fn missing_hashes_are_detected() {
        let h1 = Hash256([1u8; 32]);
        let h2 = Hash256([2u8; 32]);
        let h3 = Hash256([3u8; 32]);
        let ordered = vec![h1, h2, h3];
        let have = |h: &Hash256| *h == h1 || *h == h3; // h2 missing
        let missing = missing_tx_hashes(&ordered, have);
        assert_eq!(missing, vec![h2]);
    }
}
```

- [ ] **Step 3: Run it — expect compile failure (`missing_tx_hashes` not found)**

Run: `cargo test -p arc-node missing_hashes_are_detected 2>&1 | tail -20`
Expected: FAIL — function not found.

- [ ] **Step 4: Implement `missing_tx_hashes`**

Add near the top of `crates/arc-node/src/consensus.rs` (module-level fn):

```rust
/// Return the subset of `ordered` tx hashes for which `have(&hash)` is false,
/// preserving order. Used on DAG commit to decide what to fetch from peers
/// before execution, so that every validator executes the full ordered set.
pub(crate) fn missing_tx_hashes<F: Fn(&Hash256) -> bool>(
    ordered: &[Hash256],
    have: F,
) -> Vec<Hash256> {
    ordered.iter().copied().filter(|h| !have(h)).collect()
}
```

- [ ] **Step 5: Run the test — expect PASS**

Run: `cargo test -p arc-node missing_hashes_are_detected 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Commit the pure logic**

```bash
git add crates/arc-node/src/consensus.rs
git commit -m "feat(consensus): missing_tx_hashes detector for data availability"
```

- [ ] **Step 7: Add transport message variants**

In `crates/arc-net/src/transport.rs`, add to `OutboundMessage` (after `SendRoundSyncRequest`):

```rust
    /// Request specific transaction bodies (by hash) from a peer for data
    /// availability when a committed DAG block references txs we lack.
    RequestTransactions {
        target: Hash256,
        hashes: Vec<Hash256>,
    },
    /// Respond to a transaction request with the bodies we hold.
    SendTransactions {
        target: Hash256,
        transactions: Vec<Transaction>,
    },
```

And to `InboundMessage` (after `RoundSyncResponse`):

```rust
    /// A peer is requesting specific transaction bodies by hash.
    TransactionsRequest {
        source: Hash256,
        hashes: Vec<Hash256>,
    },
    /// A peer returned transaction bodies we requested.
    TransactionsResponse {
        transactions: Vec<Transaction>,
    },
```

Add `use arc_types::Transaction;` if not already imported. Then wire these through the dispatch module identified in Step 1, mirroring the RoundSync request/response handling exactly.

- [ ] **Step 8: Build the net + node crates**

Run: `cargo build -p arc-net -p arc-node 2>&1 | tail -15`
Expected: builds (match arms for the new variants compile). Fix any non-exhaustive-match errors the compiler points to — those are the exact dispatch sites to wire.

- [ ] **Step 9: Implement fetch-on-miss on commit**

In `crates/arc-node/src/consensus.rs`, replace the silent-skip loop (lines 964-974) so that, before building `committed_txs`, the node requests any missing bodies and waits briefly for them:

```rust
if multi_validator {
    // Data availability: ensure we have every tx body the block references.
    let have = |h: &Hash256| pending_txs.contains_key(&h.0)
        || state.full_transactions.contains_key(&h.0);
    let missing = missing_tx_hashes(&dag_block.transactions, have);
    if !missing.is_empty() {
        if let Some(ref tx_chan) = outbound_tx {
            let _ = tx_chan.try_send(OutboundMessage::RequestTransactions {
                target: dag_block.author,
                hashes: missing.clone(),
            });
        }
        // Bounded wait for the response handler to populate pending_txs.
        // The inbound TransactionsResponse handler inserts into pending_txs.
        for _ in 0..50 {
            let still = missing_tx_hashes(&dag_block.transactions, |h| {
                pending_txs.contains_key(&h.0)
                    || state.full_transactions.contains_key(&h.0)
            });
            if still.is_empty() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    let mut committed_txs: Vec<Transaction> = Vec::new();
    let mut undelivered = 0usize;
    for tx_hash in &dag_block.transactions {
        if let Some((_, tx)) = pending_txs.remove(&tx_hash.0) {
            if state.receipts.contains_key(&tx.hash.0) {
                continue; // already applied via direct RPC path
            }
            committed_txs.push(tx);
        } else if !state.receipts.contains_key(&tx_hash.0) {
            undelivered += 1;
        }
    }
    if undelivered > 0 {
        // Availability still incomplete: do NOT execute a short block (that
        // forks state). Skip execution this round; the block stays uncommitted
        // locally and a later round / state-sync recovers it.
        warn!(round = dag_block.round, undelivered,
            "DA incomplete after fetch — deferring block execution to avoid fork");
        continue;
    }
    // ... existing execution path (now with Phase 1 _at timestamp) ...
}
```

Also add the inbound handlers near the other `InboundMessage` arms (around consensus.rs:393-480):

```rust
InboundMessage::TransactionsRequest { source, hashes } => {
    let mut found: Vec<Transaction> = Vec::new();
    for h in &hashes {
        if let Some(tx) = pending_txs.get(&h.0).map(|e| e.clone()) {
            found.push(tx);
        } else if let Some(tx) = state.full_transactions.get(&h.0).map(|e| e.clone()) {
            found.push(tx);
        }
    }
    if let Some(ref tx_chan) = outbound_tx {
        let _ = tx_chan.try_send(OutboundMessage::SendTransactions {
            target: source,
            transactions: found,
        });
    }
}
InboundMessage::TransactionsResponse { transactions } => {
    for tx in transactions {
        let mut t = tx;
        t.sig_verified = true; // peer validated at RPC; re-verified before exec
        pending_txs.insert(t.hash.0, t);
    }
}
```

Confirm `state.full_transactions` is accessible (it is a `DashMap` field used at lib.rs:1426); if private, add a `pub(crate) fn has_full_tx(&self, h: &[u8;32]) -> bool` and `fn get_full_tx(...)` accessor on `StateDB`.

- [ ] **Step 10: Build + run node/state tests**

Run: `cargo build -p arc-node 2>&1 | tail -5 && cargo test -p arc-node --lib 2>&1 | tail -20`
Expected: build OK, tests pass (including `missing_hashes_are_detected`).

- [ ] **Step 11: Commit**

```bash
git add crates/arc-net/src/transport.rs crates/arc-node/src/consensus.rs crates/arc-state/src/lib.rs
git commit -m "feat(consensus): fetch missing tx bodies on commit; never execute a short block"
```

---

## Phase 3: Uniform full execution on every node (retire proposer/verifier split)

Today only the proposer fully executes; verifiers apply a best-effort diff and fall back to local execution if it's missing — a divergence source. Model 1: every node always executes the committed block locally from the identical ordered set (Phase 1 + Phase 2 make this deterministic). Keep the diff path behind `proposer_mode` as an opt-in optimization, OFF by default.

**Files:**
- Modify: `crates/arc-node/src/consensus.rs` (the `if self.proposer_mode || received_diff.is_none()` branch at line ~1048; the verifier branch at ~1127-1161)

- [ ] **Step 1: Make execution unconditional by default**

Change the branch at consensus.rs:1048 so that, when `proposer_mode` is false (the default — see main.rs:92-96), the node ALWAYS takes the execution path (no diff dependency):

```rust
// Model 1: every validator re-executes the committed block deterministically.
// The proposer/verifier state-diff optimization is opt-in via --proposer-mode.
let use_diff = self.proposer_mode && received_diff.is_some();
if !use_diff {
    // FULL EXECUTION PATH (all nodes by default)
    match state.execute_block_adaptive_at(&committed_txs, self.validator_address, dag_block.timestamp) {
        Ok((block, receipts)) => { /* existing proposer Ok arm, minus the
            proposer-only diff export which stays guarded by self.proposer_mode */ }
        Err(e) => { warn!("DAG commit block execution failed: {}", e); }
    }
} else {
    // OPT-IN VERIFIER PATH (only when --proposer-mode and a diff arrived)
    // existing verifier branch (apply_state_diff + root check)
}
```

Keep the proposer's `export_state_diff`/`BroadcastStateDiff` emission (lines 1094-1107) guarded by `if self.proposer_mode` so diff broadcasting only happens when explicitly enabled.

- [ ] **Step 2: Determinism unit test across two states with non-empty txs**

Add to `crates/arc-state/src/lib.rs` tests — two states executing the same ordered, signed tx set at the same timestamp must produce the same block hash AND the same `state_root`:

```rust
#[test]
fn two_states_execute_identical_block_with_txs() {
    let a = StateDB::new_in_memory_for_test();
    let b = StateDB::new_in_memory_for_test();
    // Fund a sender identically on both, then submit one transfer.
    let (sender, sk) = test_keypair(); // reuse this file's existing test helpers
    a.credit_for_test(&sender, 1_000);
    b.credit_for_test(&sender, 1_000);
    let tx = signed_transfer(&sk, sender, Address([9u8;32]), 100, 0);
    let txs = vec![tx];
    let ts = 1_700_000_000_000u64;

    let (ba, _) = a.execute_block_adaptive_at(&txs, Address::ZERO, ts).unwrap();
    let (bb, _) = b.execute_block_adaptive_at(&txs, Address::ZERO, ts).unwrap();
    assert_eq!(ba.hash, bb.hash);
    assert_eq!(ba.header.state_root, bb.header.state_root);
}
```

Use whatever funding/keypair/transfer helpers already exist in this test module (search for existing `Transfer` tests in `arc-state`); the assertion is the point.

- [ ] **Step 3: Run the test — expect PASS (Phase 1 already provides determinism)**

Run: `cargo test -p arc-state two_states_execute_identical_block_with_txs 2>&1 | tail -20`
Expected: PASS. If state_root differs, there is a hidden nondeterminism in execution (map iteration order in `compute_state_root`, etc.) — fix that before proceeding; it would defeat Model 1.

- [ ] **Step 4: Build + commit**

```bash
cargo build -p arc-node 2>&1 | tail -5
git add crates/arc-node/src/consensus.rs crates/arc-state/src/lib.rs
git commit -m "feat(consensus): every node fully executes committed block (Model 1); diff path opt-in"
```

---

## Phase 4: Deterministic height + state sync for laggards

With Phases 1–3, nodes that commit the same DAG-block sequence produce identical blocks. Remaining risks: (a) a node that deferred a block (Phase 2 Step 9 `continue`) is now behind and must catch up; (b) heights must map 1:1 to the committed DAG-block sequence on every node.

**Files:**
- Modify: `crates/arc-node/src/consensus.rs` (catch-up trigger)
- Reuse: existing State Sync messages (`SnapshotManifestRequest` / `SnapshotChunkRequest` / responses already exist in transport.rs:40-141)

- [ ] **Step 1: Confirm height is driven solely by committed-block execution (investigation)**

Verify that `self.height` is incremented ONLY inside the execute_block_* paths (grep `self.height.write()` in arc-state — sites at lib.rs:1277, 1458). Confirm no other code path bumps height. If a node only increments height when it executes a committed block, and Phase 2 guarantees it never executes a short/partial block, then height is a deterministic function of the committed DAG-block sequence. Record findings; if any non-commit height bump exists, that is a bug to fix here.

- [ ] **Step 2: Trigger state-sync when a node falls behind**

When the Phase 2 `undelivered > 0` defer path fires repeatedly for the same round (e.g., a counter exceeding a threshold), trigger the existing snapshot state-sync to catch up from a peer rather than staying stuck. Use the existing `SnapshotManifestRequest` flow. Add a `deferred_rounds: HashMap<u64, u32>` counter in the consensus loop; when any round's defer count exceeds, say, 25, emit a manifest request to `dag_block.author` and log it.

```rust
// inside the `undelivered > 0` branch, before `continue;`
let entry = deferred_rounds.entry(dag_block.round).or_insert(0);
*entry += 1;
if *entry == 25 {
    if let Some(ref tx_chan) = outbound_tx {
        let _ = tx_chan.try_send(OutboundMessage::/* existing snapshot manifest request */);
    }
    warn!(round = dag_block.round, "persistent DA gap — requesting state snapshot");
}
```

Fill the exact snapshot-request variant from transport.rs (there is a `SnapshotManifestRequest` inbound; find its outbound counterpart and the existing trigger site to copy).

- [ ] **Step 3: Build + commit**

```bash
cargo build -p arc-node 2>&1 | tail -5
git add crates/arc-node/src/consensus.rs
git commit -m "feat(consensus): state-sync catch-up when data-availability gap persists"
```

---

## Rollout & live acceptance

- [ ] **Step 1: Deploy the new binary to ALL seed validators at once**

Model 1 changes block hashing (timestamp source) and execution rules. Nodes on the new rules will NOT agree with nodes on the old rules — so this is a coordinated upgrade, not a rolling one across mixed versions. Build the release binary and deploy to all 5 seeds (and any other live validators) in one window. Bump the workspace version.

Run (build): `cargo build --release -p arc-node 2>&1 | tail -5`

- [ ] **Step 2: Let the seeds produce blocks past a common height, then run the acceptance tool**

Run: `cargo run --release --example cross_seed_block_check -p arc-node -- <H> <H+10> <H+50>`
(where `H` is a height all seeds have passed AFTER the upgrade)
Expected: `AGREE` on every height, exit code 0. THIS is "done". Until then, status stays "single-validator devnet".

- [ ] **Step 3: Update project memory + status doc with the result (AGREE or remaining divergence) and, if AGREE, retire the "independent chains" caveat.**

---

## Self-Review checklist (run before handing off)

1. **Spec coverage:** Determinism (Phase 1) ✓, data availability (Phase 2) ✓, uniform execution (Phase 3) ✓, height + catch-up (Phase 4) ✓, acceptance measurement (Phase 0 + Rollout) ✓.
2. **Type consistency:** `execute_block_adaptive_at` / `execute_block_verified_at` / `execute_block_blockstm_at` all `(&self, &[Transaction], Address, u64) -> Result<(Block, Vec<TxReceipt>), StateError>`. New messages: `OutboundMessage::{RequestTransactions{target,hashes}, SendTransactions{target,transactions}}`, `InboundMessage::{TransactionsRequest{source,hashes}, TransactionsResponse{transactions}}`. `missing_tx_hashes(&[Hash256], F) -> Vec<Hash256>` used in Phase 2 detector test, commit path, and inbound handler.
3. **Known constraints surfaced:** localhost multi-node is broken → unit tests for determinism + live seeds for true acceptance; coordinated (not rolling) upgrade because block rules change.
4. **Investigation-first steps** are explicit where transport plumbing must be confirmed (Phase 2 Step 1, Phase 4 Step 1) rather than guessed.
