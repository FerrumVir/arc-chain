---
name: Next-session prompt — end-to-end app test of Milestones A–E
description: Drop-in first-message prompt for the next Claude Code session. Assumes PR #40 has landed or is being verified; walks TJ through a full end-to-end paid-inference flow from the signed desktop app against the 6-seed testnet.
type: reference
---

# Session prompt — end-to-end app test

Paste this into the first message of the next session:

---

```
I'm TJ. PR #40 on FerrumVir/arc-chain ships the protocol surface for
Milestones A–E. I want to run a full end-to-end test against the 6-seed
testnet from the desktop app and verify every milestone behaves per
PLAN.md.

Network state (verify first):
- 6 seeds NYC / LAX / AMS / LHR / NRT / SGP on commit bfaa2e84 (or
  later) running ./target/release/arc-node md5 240cef61cb8c9ff7cc29a787754fdf3a
  (or the next build).
- /health on every seed: expect peers>=3, dag_round advancing, height > 1011
- /shards on every seed: expect shard_count=18 (3× replication across 6 ranges)
- arc-self-heal.service: active on every seed

Read these before touching anything:
- memory/prompt_arc_next_session_A_through_E.md
- memory/feedback_never_restart_chain.md
- memory/feedback_no_manual_restarts.md
- memory/feedback_audit_before_claiming.md
- PLAN.md
- PR #40 — which covers every milestone A through E. Status at top of
  each milestone section in PLAN.md is authoritative.

## The tests I want you to run, in this order

### Test 1 — Milestone A (free coordinator fallback, already verified)

From the signed desktop app:
1. Fresh onboard (new identity, role=observer, no model).
2. Navigate to the Inference screen.
3. Type "What is the largest planet?", click "Run inference".
4. Expect: consensus banner "Served by {NYC|LAX|AMS|…} · k=3 · N/N
   unanimous" plus a real answer within ~60s × max_tokens of wall time.
5. Report: which seed served, the consensus counts, the output hash.

### Test 2 — Milestone B (paid inference — the balance-delta test)

This is the big one. The code is shipped on PR #40 but the live
balance-delta receipt hasn't closed cleanly — see the blocker below.

From the signed desktop app:
1. With the onboarded identity from Test 1, click the Wallet tab and
   note the starting balance (ARC, not a display number).
2. Back to Inference. Flip the "Pay per request" toggle on (default
   Max fee = 10000 ARC = 10 ARC — the testnet's base unit).
3. Type a short prompt, click "Pay 10000 ARC & run".
4. Expected flow in order:
   a. Desktop signs InferenceEscrowOpen tx, POSTs to /tx/submit_signed.
   b. Polls /tx/{hash} until the open commits (< 15s under normal load).
   c. Calls /inference/run_consensus with `{payer, request_id, max_fee,
      model_id, timeout_blocks}`.
   d. Coordinator pre-flight checks escrow balance >= max_fee; 402 if not.
   e. Runs inference; on success submits InferenceEscrowRelease.
   f. Response includes an `escrow` block with `release_tx_hash`.
5. Back to Wallet: balance should be down ~10000 ARC.
6. Check treasury (address hash(b"arc-treasury")), observer_pool
   (hash(b"arc-observer-pool")), and each replica (hash("replica:NYC"),
   etc.) — they should have increased by their RoleRevenueConfig share:
     40% × 10000 = 4000 ARC to proposer (the coordinator)
     25% × 10000 = 2500 ARC split evenly across honest replicas
     15% × 10000 = 1500 ARC to observer_pool
     20% × 10000 = 2000 ARC to treasury + any rounding residue

   Total credited == 10000, exactly.

**The live test I ran during PR #40's build returned:** the open tx
submitted cleanly (HTTP 200) but didn't land in a block within my
15s poll. Need to debug: is the benchmark-traffic service drowning
single-tx submissions? Is there a signature verification layer I
missed? Use `cargo run --release --example live_paid_inference
-p arc-node -- http://<seed>:9090` as the debugging harness.

Potential root causes to check:
  - systemctl stop arc-inference-traffic.service on at least one seed
    so benchmark Transfer txs aren't filling every block
  - Check /block/{height}/txs for the block where the open tx should
    have landed
  - RUST_LOG=debug on a fresh arc-node start to see mempool drains
  - If the tx submits but never lands: verify that
    arc-consensus::DagConsensus drains the mempool (mempool.drain()
    call site) and that Block-STM access-set for the new tx types
    doesn't mutex us out

### Test 3 — Milestone C (on-demand model provisioning)

Use the paid-inference identity from Test 2. Via arc-cli or a small
Rust harness:
1. POST a ModelRegistration tx for a fake "mistral-7b" model
   (model_id = hash("mistral-7b"), n_layers=32, d_model=4096,
   quantization="int16", registration_fee=1000, royalty_recipient
   = your address). Assert: your balance -1000, treasury +1000.
2. GET /models/registry — should list the newly-registered model.
3. POST a ModelRequest for that model_id, target_k_replication=3,
   bond_per_layer_epoch=500.
4. GET /models/open_requests — should list it.
5. Submit 3 ShardCoverageClaim txs from 3 different addresses, each
   claiming a different layer range. Assert: bonds locked in claim
   accounts.

### Test 4 — Milestone D (planner determinism)

Run the arc-node cargo test `test_compute_assignment_is_deterministic`
locally twice — should pass byte-identical inputs producing identical
outputs. Then on the testnet:
1. Submit a handful of CapacityAdvertisement txs from different
   addresses.
2. GET /capacity/advertisements — verify they all appear.
3. Call the planner function via a small Rust harness (reuse
   crates/arc-node/src/planner.rs::compute_assignment) with the
   on-chain snapshot and submit the resulting ShardAssignmentProposal.
4. GET /assignments/for_me?pubkey=<node_pubkey> — verify a worker
   finds their assignment.

### Test 5 — Milestone E (LRU chunk cache + warm-set persistence)

Local test only (LRU cache is per-node local state):
1. Start a community-node process, let it serve a few /chunks/get
   requests.
2. Verify ~/.arc/chunks/ grows.
3. Restart the node, verify the _index.json sidecar is loaded and
   cached_hashes() returns the prior set.
4. Fill the cache past 10 GB cap (simulated with smaller cap for
   the test), verify LRU evicts oldest chunks.

## Rules that still override everything

1. NEVER restart all 6 seeds at once. Rolling only, one at a time,
   verify peers + round advance before touching the next. Pause
   arc-self-heal.service on that seed first.
2. NEVER skip hooks on commits (no --no-verify, no --no-gpg-sign).
3. If anything takes > estimated_effort × 1.5, stop and tell me.
   Don't pretend to ship.
4. Every claim about a milestone passing must cite the tx hashes /
   balance deltas that prove it. Screenshots or JSON printouts, not
   "seems to work."

## When each test passes, report:

- The tx hashes that made it happen
- The balance deltas (pre / post / delta columns)
- Link to the on-chain receipt (/tx/{hash}/full)
- For the desktop tests: a 2-line summary of what the user saw on screen

When all 5 tests pass, close PR #40, merge to main, update PLAN.md
to mark each milestone ✅ with the live-test tx hashes as evidence,
and post a summary comment on each of #35 through #39.

Go.
```
