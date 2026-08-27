# Tier 1 On-Chain Inference — Implementation Plan

**Status:** Draft v1 — 2026-05-16
**Goal:** Migrate all inference from off-chain coordinator orchestration to fully on-chain VRF-committee voting. Eliminates the hardcoded coordinator list, shard-registry drift, and the INT8 gibberish output problem in one architectural change.

> **Historical proposal, not current rollout state.** Do not use the endpoints,
> committee sizes, rewards, or deployment expectations below as evidence that
> Tier 1 is live. The 2026-08-26 public fleet is forked/version-skewed and the
> v0.8.0 recovery candidate is not published or deployed. See
> [`PRODUCTION-RECOVERY-AUDIT-2026-08-26.md`](PRODUCTION-RECOVERY-AUDIT-2026-08-26.md).
> The candidate desktop disables both Tier 1 request submission and paid
> escrow before signing or network I/O. VRF selection is not validator
> authorization.

---

## Context

### Why this change

Today (per `INFERENCE_FLOW.md` current-state diagram), inference goes:

```
desktop → POST /inference/run_consensus to hardcoded coordinator
       → coordinator orchestrates sharded pipeline (in-memory ShardRegistry)
       → ONE coordinator submits InferenceAttestation tx to chain
```

Three failure modes baked into this design:

1. **Pipeline gap (today's actual failure):** Coordinator's `ShardRegistry` is an in-memory `DashMap` per node (`crates/arc-inference/src/distributed.rs:51`). Retired SAO + JNB nodes are still registered as covering layers [28,30) and [30,32). Pipeline planner picks dead shards → all 5 alive coordinators return `503 Pipeline gap: expected layer 32 next, got [28, 30)`.

2. **Gibberish output:** Coordinators run as shard-holders. `arc-node/src/main.rs:667` explicitly skips the candle backend (Q4 float = coherent) for shard-holders to save 4 GB RAM. They fall back to the INT8 integer path — documented PPL ~144 = essentially random tokens (per `CLAUDE.md:51`).

3. **Hardcoded coordinator list:** Desktop has 6 IPs literally pasted in `desktop/src-tauri/src/commands.rs:370-377`. NYC is dead. Every inference burns 5-10s on the failed NYC connect before falling through to LAX. No on-chain discovery.

### Why Tier 1 fixes all three

Tier 1 means **every validator runs the same model independently**, votes on the output hash, and chain consensus aggregates. Description from `crates/arc-inference/src/candle_backend.rs:9-10`:

> *This is the Tier 1 on-chain inference path. Every validator loads the same GGUF model and produces bitwise identical output.*

Consequences:
- No coordinator role. Discovery problem disappears.
- No shard pipeline. SAO/JNB drift problem disappears.
- Each validator runs candle Q4 backend (full model, not shard-holder). Gibberish problem disappears.
- Multi-validator vote ≥ k-of-n agreement is enforced by chain state transitions, not by an HTTP fan-out a single coordinator decides to run.

### What's already in the codebase (audit findings)

| Component | Location | Status |
|---|---|---|
| VRF committee selection (`select_committee`) | `crates/arc-inference/src/committee.rs:69-101` | ✅ Implemented, not wired |
| `aggregate_votes` (5-of-7 agreement) | `crates/arc-inference/src/committee.rs:108-` | ✅ Implemented, not wired |
| VRF primitive (ECVRF-ED25519) | `crates/arc-crypto/src/vrf.rs:1-220` | ✅ Implemented |
| Candle GGUF inference engine | `crates/arc-inference/src/candle_backend.rs` | ✅ Implemented, used by non-shard-holders |
| InferenceAttestation tx + apply | `transaction.rs:221` + `arc-state/src/lib.rs:3828-3871` | ✅ Implemented (single-attester pattern) |
| InferenceChallenge tx + apply | `transaction.rs:223` + `arc-state/src/lib.rs:3872-3920` | ✅ Implemented (bond + challenge period) |
| Bond/escrow at `BLAKE3("arc-inference" \|\| tx_hash)` | `arc-state/src/lib.rs:3854` | ✅ Reusable for new flow |
| Validator main loop + block commit hook | `crates/arc-node/src/consensus.rs:914-975` | ✅ Has hook point |
| Reactive event subscription | — | ❌ Doesn't exist; must hook block-commit |
| Auto-finalize guardian (release bond after challenge period) | — | ❌ Missing today (line 3864 stores period, nobody cleans up) |

The plumbing is 70% there. What's missing: a multi-vote tx type, on-chain tally state, validator background task that runs candle on demand, and an auto-finalize hook.

---

## Approach

### Decisions taken

| Decision | Choice | Reason |
|---|---|---|
| Committee size K | **5** (min_agreement = **3**) | Matches the 5 currently-alive coordinators (LAX, AMS, LHR, NRT, SGP). No new infra required to bootstrap. Bumps to K=7 once SAO/JNB resurrected or 2 new VPS added. |
| Slashing policy | **Follow existing** — voter posts a bond like `InferenceAttestation` does today. Disagreement = bond goes to escrow, releasable to majority via the existing `InferenceChallenge` mechanic. **No new auto-slash code.** | Matches user instruction "follow existing". Keeps the change surface small. Auto-slash can come later as a separate proposal. |
| Backward compatibility | **Keep `/inference/run_consensus` running in parallel.** Add new path behind a desktop setting flag (`inference_mode: "coordinator" \| "onchain"`, default coordinator). | Allows dual-running during validation. Hard cutover only after observing 1-2 weeks of clean onchain finalizations. |
| Model size at Tier 1 | **Llama-2-7B Q4_K_M (~4 GB)** per validator | Fits commodity validator RAM. Below this we'd need Tier 2 sharded (out of scope). |
| Randomness for committee seed | `BLAKE3(committed_block_hash \|\| request_id)` of the block that committed the `InferenceRequest` tx | Unpredictable before commit, deterministic after. Avoids `output_hash`-as-seed chicken-and-egg. |
| Deployment scope | **Phase A is code-only.** Phase B (per-VPS rebuild + model upload + restart) is explicitly optional and yours to schedule. Plan does not block on it. | Matches user question "apakah bisa tanpa itu" — yes, code can land without immediate deploy. |

### New transaction types

Add 3 variants to `TxType` enum at `crates/arc-types/src/transaction.rs:177`. Next free slot is `0x22` (existing types go through `0x1c`+; leave 0x1d–0x21 for in-flight Milestone D/E work):

| Variant | Byte | Purpose |
|---|---|---|
| `InferenceRequest` | `0x22` | Submitted by user. Locks `max_reward` in escrow. Triggers committee selection. |
| `InferenceVote` | `0x23` | Submitted by each committee member after running candle. Carries `output_hash`, VRF proof of committee membership, optional `output_blob` (only the first voter attaches plaintext to save block space). |
| `InferenceFinalize` | `0x24` | System-deterministic tx injected by every validator when either ≥ K votes received or deadline lapsed. Triggers payout/refund per `aggregate_votes` result. |

Each variant gets a struct body. Sketch (final field tuning during implementation):

```rust
pub struct InferenceRequestBody {
    pub request_id: Hash256,        // BLAKE3(input || model_id || requester || nonce)
    pub model_id: Hash256,           // arc-32L-4096d-32h-32000v hash
    pub input_hash: Hash256,
    pub input_blob: Vec<u8>,         // ≤32 KB; longer = use IPFS hash variant later
    pub max_tokens: u32,
    pub tier: u8,                    // 1 for Llama-7B; reserved for future Tier 2+
    pub max_reward: u64,             // ARC locked in escrow
    pub deadline_blocks: u64,        // current_height + N
    pub committee_size: u8,          // 5 for testnet
}

pub struct InferenceVoteBody {
    pub request_id: Hash256,
    pub output_hash: Hash256,
    pub output_blob: Option<Vec<u8>>,// first voter attaches; verifies output_hash
    pub vrf_proof: Vec<u8>,          // proof voter ∈ committee for (block_hash || request_id)
    pub committee_seed: Hash256,     // = block_hash of request commit block
}

pub struct InferenceFinalizeBody {
    pub request_id: Hash256,
    // No signature: identified by tx.from = system_address()
}
```

### State transitions (in `crates/arc-state/src/lib.rs`)

Three new apply arms, each ~80-120 LOC, modelled after existing `apply_inference_attestation` at line 3828:

1. **`apply_inference_request`**
   - Debit `max_reward` from sender into escrow account `BLAKE3("arc-infreq" || request_id)`
   - Store metadata `BLAKE3(model_id || input_hash || tier || deadline || committee_size || height)` in escrow's `storage_root`
   - Use first byte of `code_hash` as status enum: `0=Open, 1=Voting, 2=Finalized, 3=Refunded`. Start at `Open`.
   - Anchor block height in escrow `nonce` field
   - Append RequestSubmitted to receipt log

2. **`apply_inference_vote`**
   - Verify request exists, status ≤ Voting
   - Recompute committee deterministically: `select_committee(committee_seed, eligible_validators, tier, K)`. Eligible = current validator set from `StateDB.validators`. Reject if voter ∉ committee.
   - Verify VRF proof (defense-in-depth; committee membership check above is the primary gate)
   - Reject duplicate vote from same address for same request
   - Append `(voter, output_hash)` to vote bucket at address `BLAKE3("arc-infvotes" || request_id)`, serialized as `Vec<(Address, Hash256)>` in `storage_root`
   - Transition status `Open → Voting` on first vote
   - If vote count reaches `K`, set status to `ReadyToFinalize` (status byte 4) so any node knows it's eligible for immediate finalize

3. **`apply_inference_finalize`**
   - Reject if status already `Finalized` or `Refunded`
   - Load votes. Run `aggregate_votes(committee, votes)` from `committee.rs:108`
   - **Consensus branch** (≥ min_agreement agree):
     - Compute `agreeing_voters` and `disagreeing_voters` lists
     - Distribute `max_reward`: 70% split evenly among agreeing voters, 20% rebate to requester (encourages reasonable max_reward), 10% to treasury (per existing `RoleRevenueConfig`)
     - Set status `Finalized`. Emit final `InferenceAttestation`-style receipt with winning `output_hash` + agreeing voters list
     - Disagreeing voters: **no slash this phase** (per "follow existing"). They simply don't earn. Their bond, if posted, returns. A future `InferenceChallenge` against them remains available via the existing 0x17 path.
   - **Disagreement branch** (< min_agreement on any hash):
     - Refund `max_reward` to requester
     - Set status `Refunded`
     - Log `DisagreementEvent` for off-chain investigation
   - **Timeout branch** (current_height > deadline, votes < min_agreement):
     - Refund `max_reward - request_fee_floor (1 ARC anti-spam)` to requester
     - Set status `Refunded`
     - Absent committee members: liveness flag in their validator record (existing `JoinValidator`/`LeaveValidator` flow already tracks uptime; no new slashing needed)

### Validator inference task (`crates/arc-node/src/inference_validator.rs` — NEW, ~350 LOC)

Spawned at boot from `consensus.rs:230` after the validator is initialized. Owns:

- `tokio::sync::broadcast::Receiver<CommittedBlock>` — fed from `consensus.rs:920` after `committed.sort_by_key`. Add a `broadcast::Sender` to `ConsensusState` (~10 LOC), publish each committed block.
- `Arc<GgufEngine>` — lazy-init on first inference; loads the Llama-7B Q4 GGUF from `~/.arc/models/`.
- `Arc<Mempool>` for submitting `InferenceVote` and `InferenceFinalize` txs.

Behavior per committed block:
1. Scan txs for `InferenceRequest`. For each:
   a. Compute committee seed = `BLAKE3(committed_block.hash || request_id)`
   b. Look up validator set from state; call `select_committee(seed, eligible, tier, K)`
   c. If `self.validator_address ∈ committee.members`: spawn detached task that runs `engine.generate(input, max_tokens)` → builds + signs `InferenceVote` → submits to mempool
2. Scan state for requests in `Voting` or `ReadyToFinalize` status:
   a. If status `ReadyToFinalize` (≥K votes): build unsigned `InferenceFinalize` tx, submit. Mempool dedupes (all nodes generate identical bytes → same tx hash).
   b. If `height >= request.deadline_blocks + request.anchor_height`: same as above but on timeout path.

The validator inference task is the **only** place that decides "I should run inference now." All other ordering (when votes counted, when payout) is decided by deterministic state transitions when txs commit.

### RPC endpoints (`crates/arc-node/src/rpc.rs`)

Two new handlers, ~100 LOC each:

- `POST /inference/onchain/submit` — accepts `{model_id, input, max_tokens, tier, max_reward, deadline_blocks}`. Builds `InferenceRequest` tx body, signs with the desktop user's identity key (passed in `signed_tx` field — desktop signs locally, RPC just relays), submits to mempool. Returns `{request_id, tx_hash}`.
- `GET /inference/onchain/result/:request_id` — reads escrow state at `BLAKE3("arc-infreq" || request_id)` + vote bucket. Returns `{status, vote_count, output_hash?, output_blob?, finalize_tx?}`. Desktop polls at 500ms cadence until status ∈ {Finalized, Refunded}.

Optional later: WebSocket subscription to skip polling. Deferred from Phase A.

### Desktop changes (`desktop/`)

Behind a feature flag, **minimal**:

- `desktop/src/lib/store.ts` — add `inferenceMode: "coordinator" | "onchain"` field, persist via existing zustand+localStorage.
- `desktop/src/screens/Settings.tsx` — toggle to switch modes (default "coordinator" during initial rollout).
- `desktop/src/screens/Inference.tsx` — in `runInferenceSmart`, branch on `inferenceMode`:
  - `coordinator`: existing flow (run_consensus → run direct fallback)
  - `onchain`: build & sign `InferenceRequest` tx locally, POST to `/inference/onchain/submit`, poll `/onchain/result/:id` until terminal status, render the winning `output_hash` decoded from `output_blob`
- `desktop/src-tauri/src/commands.rs` + `rpc_client.rs` — add `run_inference_onchain(prompt, max_tokens, max_reward)` Tauri command (~80 LOC), and `poll_onchain_result(request_id)` (~40 LOC)

No new UI components. Onchain mode reuses the existing inference result card; the only visible difference is a small "✓ on-chain consensus (K votes)" badge replacing the coordinator name.

### File-level breakdown

| File | Action | LOC | Owner phase |
|---|---|---|---|
| `crates/arc-types/src/transaction.rs` | Add 3 TxType variants, 3 body structs, serialization | +180 | Phase A.1 |
| `crates/arc-types/src/lib.rs` | Re-export new body types | +6 | Phase A.1 |
| `crates/arc-state/src/lib.rs` | 3 apply arms + helpers + status enum bytes | +400 | Phase A.2 |
| `crates/arc-inference/src/committee.rs` | `derive_committee_seed`, `eligible_validators_from_state` helper | +60 | Phase A.2 |
| `crates/arc-node/src/inference_validator.rs` | **NEW** — committee watcher + candle runner + finalize injector | +350 | Phase A.3 |
| `crates/arc-node/src/consensus.rs` | Wire `broadcast::Sender<CommittedBlock>`, spawn validator task | +60 | Phase A.3 |
| `crates/arc-node/src/rpc.rs` | 2 new handlers + route registration | +200 | Phase A.4 |
| `crates/arc-node/src/lib.rs` | Module declaration | +2 | Phase A.3 |
| `desktop/src-tauri/src/commands.rs` | 2 new Tauri commands | +120 | Phase A.5 |
| `desktop/src-tauri/src/rpc_client.rs` | 2 new RPC client methods | +80 | Phase A.5 |
| `desktop/src-tauri/src/lib.rs` | Register handlers | +2 | Phase A.5 |
| `desktop/src/lib/tauri.ts` | Typed API wrappers + mock-mode | +100 | Phase A.5 |
| `desktop/src/lib/store.ts` | Add `inferenceMode` flag | +15 | Phase A.5 |
| `desktop/src/screens/Settings.tsx` | Toggle UI | +40 | Phase A.5 |
| `desktop/src/screens/Inference.tsx` | Branch in `runInferenceSmart` + onchain polling | +120 | Phase A.5 |
| Tests in `crates/arc-node/tests/inference_onchain_single.rs` | **NEW** — single-node integration test | +300 | Phase A.6 |
| Tests in `crates/arc-state/tests/inference_state_transitions.rs` | **NEW** — apply-arm unit tests | +200 | Phase A.2 |

**Total: ~2235 LOC, 17 files touched, 3 new files. Estimated 4-5 focused dev days.**

---

## Implementation phases

### Phase A — Code only (no production deploy required)

**A.1: TxType + bodies in `arc-types`** *(half day)*
- Add 3 enum variants, body structs, serde implementations
- Unit-test serialize/deserialize round-trip
- Verify no conflicts with existing 0x22+ (currently free)

**A.2: State transitions in `arc-state`** *(1 day)*
- Implement 3 apply arms paralleling `apply_inference_attestation` pattern
- Add helpers in `committee.rs` for seed derivation and validator iteration
- Unit tests for each apply path (mocked accounts and state)

**A.3: Validator inference task** *(1 day)*
- Create `inference_validator.rs`
- Add commit broadcast channel in `consensus.rs`
- Spawn task at boot
- Wire candle engine load (reuse existing `GgufEngine`)
- Submit InferenceVote on detection; submit InferenceFinalize when conditions met

**A.4: RPC endpoints** *(half day)*
- Add `/inference/onchain/submit` and `/result/:id` handlers
- Wire into existing axum router
- Validate against existing escrow inspection helpers

**A.5: Desktop integration** *(half day)*
- Add Tauri commands, typed wrappers, mock paths
- Settings toggle
- Inference screen branching + polling loop

**A.6: Single-node integration test** *(half day)*
- One-validator test: submit request, validator votes itself, auto-finalizes, payout asserted
- Timeout test: submit, advance N+deadline blocks without vote, refund asserted
- All running in `cargo test --workspace`, no external network

### Phase A acceptance criteria

- `cargo test --workspace` passes including the 2 new integration tests
- `npm run tauri:dev` shows new Settings toggle, switching to "onchain" mode and submitting an inference produces a real on-chain `request_id` + transaction hash visible in `/health` (height increments)
- Single local node can self-vote and finalize end-to-end in <15 sec for a 32-token Llama-7B response

**Phase A is shippable on its own.** The new path lives behind a feature flag; default behavior (coordinator path) is unchanged for existing users.

### Phase B — Production deployment (your task, optional, can be deferred)

This is what unlocks Tier 1 for the live testnet. **Phase A landing does not require Phase B.**

For each of the 5 currently-alive coordinators (LAX `140.82.16.112`, AMS `136.244.109.1`, LHR `104.238.171.11`, NRT `202.182.107.41`, SGP `149.28.153.31`):

```bash
# 1. SSH in
ssh root@<IP>

# 2. Upload full Llama-2-7B Q4_K_M to ~/.arc/models/
curl -L "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf" \
  -o ~/.arc/models/llama-2-7b.gguf

# 3. Pull new arc-node binary
curl -L https://github.com/FerrumVir/arc-chain/releases/latest/download/arc-node-linux-x86_64 \
  -o ~/.arc/bin/arc-node && chmod +x ~/.arc/bin/arc-node

# 4. Edit systemd unit — REMOVE any --shard-ranges flag (this triggers full-model candle mode)
sudo systemctl edit arc-node   # set ExecStart with --model ~/.arc/models/llama-2-7b.gguf and NO --shard-ranges

# 5. Restart
sudo systemctl restart arc-node
sudo journalctl -u arc-node -f
```

Watch for the log line `Inference  : ENABLED (candle Q4 float, coherent output)` — that's the confirmation candle backend is active.

**Cost:** 20 minutes per VPS × 5 = ~2 hours total. No new VPS purchase required for K=5. To upgrade to K=7 later, also resurrect SAO + JNB or add 2 new VPS.

### Phase C — Tier 1 default (future, after 1-2 weeks dual-run)

- Flip desktop default `inferenceMode` from `coordinator` to `onchain`
- Deprecate `/inference/run_consensus` endpoint (mark in release notes, keep code one release for rollback)
- Document Tier 2 sharded path as separate model-size escalation (Llama-70B+), not the default

---

## Failure modes & their state-machine responses

| Mode | Trigger | Response |
|---|---|---|
| Validator disagreement | `aggregate_votes` returns `Disagreement` | Refund payer, set status=Refunded, emit DisagreementEvent. No slashing (follow existing). |
| Timeout (< K votes by deadline) | `current_height > anchor + deadline` AND `votes < min_agreement` | Refund `max_reward - 1 ARC anti-spam fee` to payer. No slashing. |
| Validator offline when committee-selected | Validator doesn't submit vote | Counted as absent. Their reward share goes to treasury. Existing liveness tracker handles repeat offenders. |
| Duplicate vote | Same voter, same request, second tx | Reject at apply (`storage_root` check), tx fee burned (anti-spam). |
| Non-committee vote | Stranger submits vote | Reject at apply. Tx fee burned. |
| Stuck finalize (no node injects the tx) | Determinism drift or all candidates offline | Watchdog in `inference_validator.rs` re-checks every N blocks, force-injects after `deadline + 10 blocks`. |
| Model missing on validator | `GgufEngine::generate` returns `ModelNotFound` | Validator skips this request (doesn't vote). Counted as absent. Logs error so operator notices. |
| Validator running INT8 path (no candle) | shard-holder accidentally selected | Same as above — skip, counted absent. Phase B deploy ensures this can't happen. |

---

## Test strategy

### Single-node (lands in Phase A.6)

`crates/arc-node/tests/inference_onchain_single.rs`:
- Spin up 1-validator test chain
- Submit `InferenceRequest` from the validator's own account
- Validator scans, sees self in committee (only member), runs `MockInferenceEngine` (deterministic `BLAKE3(input)` output for test stability — skips real model load)
- Submits `InferenceVote`
- Validator scans state, sees `ReadyToFinalize`, injects `InferenceFinalize`
- Assert: escrow drained, validator balance increased by 70% of `max_reward`, treasury increased by 10%, status `Finalized` in receipt log

Timeout variant:
- Same setup, but skip the vote step
- Advance `N + deadline_blocks`
- Trigger watchdog finalize
- Assert: refund minus 1 ARC fee back to requester

### Multi-node (Phase A done locally with test harness, real network in Phase B)

`crates/arc-node/tests/inference_onchain_multinode.rs`:
- 7-validator harness (existing test pattern in arc-consensus)
- 1 submits request, 5 in committee with deterministic mock engine
- Verify all 5 derive same committee membership (determinism property)
- Verify ≥3 agreement → consensus → payout distribution
- Disagreement variant: 3 nodes return different hashes → refund + status=Refunded
- Liveness variant: 2 committee members offline → 3 remaining still reach min_agreement → finalize succeeds, absent members' share to treasury

### Property tests (proptest)
- Committee selection determinism across random validator sets + seeds
- Apply ordering: `Vote` before `Request` rejected; `Finalize` before `Vote` rejected
- Status state machine: can't go Finalized → Refunded, can't double-Finalize

---

## Alternatives considered and rejected

**Alt 1: Single multi-sig `InferenceConsensus` tx with off-chain vote aggregation by a leader.**
Validators sign output_hash off-chain, a leader aggregates into one tx with K signatures.
Rejected: re-introduces an off-chain coordinator (justru yang mau dihilangkan). Leader can censor disagreeing votes invisibly. Doesn't allow partial recovery when 1-2 voters are slow.

**Alt 2: Synchronous in-block inference — every validator runs inference during block proposal, result is included as a precompile output.**
Rejected: Llama-7B forward pass is 5-30 sec on CPU. Blocks block production (must be ~1 sec). Breaks DAG round timing in `consensus.rs:244` (50ms tick). Would require redesigning consensus tempo.

**Alt 3: Keep coordinator but make ShardRegistry on-chain (lighter scope).**
Rejected: doesn't address the "user wants everything on-chain" goal. Fixes pipeline gap but leaves coordinator role + INT8 gibberish problem. Worth doing later as a Tier 2+ optimization, not the Tier 1 path.

---

## What this plan does NOT cover

- Tier 2 sharded inference for models > validator RAM (Llama-70B+). Stays out of scope; existing `/inference/run_consensus` + `ShardRegistry` keep serving that case after Phase C if ever needed.
- Auto-slash on disagreement (deferred to a future proposal — user instruction "follow existing" means no new slash code in this PR).
- GPU acceleration. Independent dimension; works whether each validator runs CPU or GPU candle. Recommended after Phase B for sub-second latency.
- Coordinator-rotation via VRF for the legacy path (was a candidate in earlier discussion). Dropped because Phase C removes the coordinator role entirely.

---

## Critical files referenced

- `crates/arc-types/src/transaction.rs` — TxType enum, all body structs
- `crates/arc-state/src/lib.rs:3828-3920` — pattern to clone for new apply arms
- `crates/arc-inference/src/committee.rs:69-101` — `select_committee` to reuse
- `crates/arc-inference/src/candle_backend.rs` — `GgufEngine` for the validator task
- `crates/arc-crypto/src/vrf.rs` — VRF prove/verify
- `crates/arc-node/src/consensus.rs:914-975` — block commit hook point
- `crates/arc-node/src/main.rs:665-687` — current candle-vs-shardholder branch (preserve, don't remove)
- `desktop/src-tauri/src/commands.rs:370-377` — hardcoded coordinator list (left in place for legacy path, not used by onchain path)

---

## Verification (end-to-end, post Phase A)

```bash
# 1. Workspace builds + tests pass
cargo test --workspace --lib

# 2. New integration tests pass
cargo test -p arc-node --test inference_onchain_single
cargo test -p arc-state --test inference_state_transitions

# 3. Local end-to-end via desktop
# Start single-validator local node
cargo run -p arc-node -- \
  --rpc 127.0.0.1:9090 --p2p-port 9091 \
  --data-dir /tmp/arc-test \
  --model ~/.arc/models/llama-2-7b.gguf \
  --validator-seed "test seed phrase here"

# Desktop:
cd desktop && npm run tauri:dev
# → Settings → Inference Mode → "onchain"
# → Inference → "Hello, what is 2+2?" → submit
# → Expect: status flips Open → Voting → Finalized within 30-60 sec
#           output_blob contains coherent Llama response
#           sidebar shows "✓ on-chain consensus (1/1 votes)" badge
```

After Phase B deploy, repeat step 3 against the live testnet — expect 3-5 votes, finalize within 1-2 minutes per request.
