# Tier 1 On-Chain Inference — User Experience

**Companion to:** [TIER1_ONCHAIN_INFERENCE_PLAN.md](./TIER1_ONCHAIN_INFERENCE_PLAN.md) (implementation plan)
**Status:** Draft v1 — 2026-05-16
**Audience:** Anyone evaluating what users will actually see when Tier 1 ships.

This document walks through the inference experience after Tier 1 lands. It complements the implementation plan by focusing on what the user perceives — UI states, timing, edge cases, and how it differs from today's coordinator-based flow.

---

## 1. Submit a prompt

User opens the desktop app and goes to the **Inference** tab. Nothing about the input form changes — same text box, same example prompts, same Submit button.

The one new element is a setting in **Settings → Inference**:

```
┌──────────────────────────────────────────────┐
│ Inference Mode                               │
│   ◯ Coordinator (legacy)                     │
│   ● On-chain (Tier 1) ← new default          │
│                                              │
│ Max reward per request: [10 ARC] ▼          │
│ Deadline (blocks):       [20]    ▼          │
└──────────────────────────────────────────────┘
```

- **Default flips to "On-chain"** once Phase C lands (after 1-2 weeks of clean dual-running).
- Coordinator mode stays available as a fallback selector — useful if Tier 1 validator pool is temporarily empty or a bug surfaces.
- `Max reward` is the user's escrow ceiling; the actual paid amount depends on consensus outcome (see §5).
- `Deadline (blocks)` controls how long the user waits before the request auto-refunds.

---

## 2. Status panel during inference

The moment Submit is pressed, the result card replaces the static "submit a prompt" placeholder with a live status panel:

```
┌─────────────────────────────────────────────────┐
│ "Explain zero-knowledge proofs in one sentence."│
│                                                 │
│ ⟳ On-chain inference in progress...             │
│                                                 │
│ Tx:  0x3d00c1f3...  [explorer ↗]                │
│ Lock: 10 ARC in escrow                          │
│                                                 │
│ Committee selected via VRF (block #34928):      │
│   ● LAX  ✓ voted   (output: 0xe598...)          │
│   ● AMS  ✓ voted   (output: 0xe598...)          │
│   ● LHR  ⟳ computing                            │
│   ● NRT  ⟳ computing                            │
│   ● SGP  ○ waiting                              │
│                                                 │
│ Agreement: 2/3 matched — need 1 more vote       │
│ Elapsed: 18s · Deadline: block #34948 (47s left)│
└─────────────────────────────────────────────────┘
```

Per-validator progress is live:

- `○ waiting` — the committee member's node hasn't yet seen the request commit
- `⟳ computing` — node saw the request, is running candle Q4 inference locally
- `✓ voted` — node submitted `InferenceVote` tx; its output hash shown
- `✗ dissent` — node voted but output hash diverges from majority
- `— absent` — deadline passing without a vote

This is a step change from today's spinner: users see **which validators are doing what**, in real time, with chain-anchored tx hashes for every state transition.

---

## 3. Final result

After consensus is reached (≥3 of 5 agree on the same output hash) or the deadline expires, the panel transitions to its terminal state:

```
┌─────────────────────────────────────────────────┐
│ Prompt: "Explain zero-knowledge proofs..."      │
│                                                 │
│ "A zero-knowledge proof is a cryptographic      │
│  method where one party can prove they know     │
│  a secret without revealing the secret itself." │
│                                                 │
│ ┌─ ✓ Verified on-chain consensus ─────────────┐│
│ │ Votes:    3/5 agreed (LAX, AMS, NRT)        ││
│ │ Dissent:  LHR (0x8a2f...) — bond forfeited  ││
│ │ Absent:   SGP — no liveness penalty         ││
│ │ Finalize: 0xfafafafa...  [explorer ↗]       ││
│ │ Reward:   7 ARC distributed                 ││
│ │ Refund:   2 ARC back to you                 ││
│ │ Treasury: 1 ARC                             ││
│ └──────────────────────────────────────────────┘│
│                                                 │
│ Model: arc-32L-4096d (Llama-2-7B Q4)            │
│ Latency: 38s (compute) + 4s (consensus)         │
│ Engine: candle Q4 float (coherent, deterministic)│
└─────────────────────────────────────────────────┘
```

What's new versus today's terminal panel:

- **Output is coherent.** Candle Q4 produces real Llama-2 quality, not the INT8 PPL-144 gibberish that the current shard-holder coordinator path emits.
- **Vote breakdown is on-chain and clickable.** Every agreement/dissent/absence is a verifiable tx, not a coordinator's word.
- **Reward distribution is visible.** User sees exactly where their `max_reward` went (paying agreeing validators, refunded to them, or to treasury). No black box.
- **Finalize tx links to explorer** so anyone can audit the full vote tally.

---

## 4. Timeline (concrete seconds, CPU validators)

| Time | What happens on-chain | What the UI shows |
|---|---|---|
| 0s | User clicks Submit | Spinner: "Building transaction…" |
| 1s | Desktop signs and submits `InferenceRequest` tx to local mempool | Tx hash visible: "Submitted, waiting for inclusion" |
| 1-3s | Block commits the request, `max_reward` locked in escrow, committee selected via VRF from current validator set | "Locked 10 ARC. Committee: LAX, AMS, LHR, NRT, SGP" |
| 3-30s | All 5 validators independently run candle Q4 forward pass on their local Llama-2-7B GGUF (parallel) | Per-validator badges flip `○ waiting → ⟳ computing → ✓ voted` as each finishes |
| 30-35s | ≥3 validators agree on `output_hash`. Some node deterministically injects `InferenceFinalize` tx | "Consensus reached, finalizing…" |
| 35-38s | Finalize tx commits, payout distributed, escrow cleared | Status flips to **Finalized**, full result panel renders |

Total: **~35-40 seconds** for a 32-token response with CPU-only validators. With GPU offload (`--gpu-layers 32`): **~5-10 seconds**.

For comparison, today's coordinator flow:

| Time | What happens |
|---|---|
| 0-10s | Desktop tries NYC `:9090` (dead) → timeout |
| 10-11s | Falls to LAX → `503 Pipeline gap: expected layer 32 next, got [28, 30)` |
| 11-12s | AMS → `503 Pipeline gap` |
| 12-13s | LHR → `503 Pipeline gap` |
| 13-14s | NRT → `503 Pipeline gap` |
| 14-15s | SGP → `503 Pipeline gap` |
| 15s | Falls back to local node — either no result (model not loaded) or INT8 gibberish (PPL ~144) |

**Today's flow takes 15-60 seconds and routinely returns either an error or gibberish.** Tier 1 takes a comparable amount of time but returns a coherent, multi-validator-attested result.

---

## 5. Edge cases the user actually encounters

### Timeout (deadline passed before consensus)

Some committee members were slow or offline. The deadline block height is reached without `min_agreement` votes.

```
┌─────────────────────────────────────────────────┐
│ ⚠ Timeout — request not finalized in 20 blocks  │
│                                                 │
│ Votes received: 2/5                             │
│ Refunded: 9 ARC (10 - 1 ARC anti-spam fee)      │
│                                                 │
│ [Retry] [Switch to coordinator mode]            │
└─────────────────────────────────────────────────┘
```

The 1 ARC fee is the chain's defense against free-flood requests; everything else returns to the user's account.

### Disagreement (no majority on any output)

All 5 validators voted, but the output hashes diverged enough that no value reached `min_agreement = 3`. Possible causes: a buggy validator, a tampered model file on one node, or a transient bit-flip.

```
┌─────────────────────────────────────────────────┐
│ ✗ Disagreement — no majority output             │
│                                                 │
│ Votes:                                          │
│   0xe598... LAX, AMS  (2)                       │
│   0x8a2f... LHR        (1)                      │
│   0x712c... NRT        (1)                      │
│   0x4d33... SGP        (1)                      │
│                                                 │
│ Refunded: 9 ARC                                 │
│ Dispute logged on-chain for investigation:      │
│   [explorer ↗]                                  │
└─────────────────────────────────────────────────┘
```

User loses 1 ARC anti-spam fee. The chain logs a `DisagreementEvent` that operators can use to flag misbehaving validators. No automatic slashing in this phase (per the "follow existing" decision).

### Validator absent

One or two committee members never submit a vote. Their share rolls into treasury; the remaining members can still reach consensus if ≥ `min_agreement` agree.

```
Votes: 3/5 agreed (LAX, AMS, NRT)
Absent: LHR, SGP — share to treasury (no slash)
Result: ✓ Finalized
```

This is the normal degraded path. As long as 3 of 5 are up, the user gets a real answer.

### Switching back to coordinator mode mid-session

The Settings toggle is hot — flipping from "On-chain" back to "Coordinator (legacy)" takes effect on the next Submit. No restart needed. Useful if:

- The validator pool has fewer than `min_agreement` healthy nodes
- Tier 1 latency is unacceptable for a particular workload
- A bug surfaces during the dual-run period

---

## 6. What does NOT change

For continuity with existing user expectations:

- **Identity and wallet:** same flow. Recovery phrase generated once during onboarding, used to sign all txs.
- **ARC balance from faucet:** unchanged. Same `faucet_claim` command, same balance display.
- **Tab navigation:** Dashboard / Wallet / Inference / Earnings / Network / Logs / Settings. Same.
- **Prompt input UX:** text area + max-tokens slider + example prompts. Same.
- **Attestation concept:** still there — every inference produces a tx anchored on-chain. The difference is that now there are **5 vote txs + 1 finalize tx** instead of one coordinator-submitted attestation.
- **Faucet, balance, address copy, explorer link buttons:** all preserved.

---

## 7. What this experience requires from the user

### Required (no extra setup beyond current flow)

- An identity with at least `max_reward` ARC in their balance (escrow lock). Faucet can top up.
- Network connectivity to one local arc-node (their own or one of the testnet seeds). The arc-node handles all the chain-level mechanics.

### NOT required from the user

- Running their own validator
- Loading a model locally (they only need to load a model if they want to *be* a validator and earn rewards)
- Knowing which coordinator IPs are alive
- Configuring shard ranges
- Anything about Q4 vs INT8 vs INT16 quantization

The entire chain side — validator discovery, committee selection, candle invocation, vote aggregation, payout — is invisible to the requester. They submit a prompt and either get a coherent answer with a verifiable receipt, or a refund.

---

## 8. What this experience requires from validators (for completeness)

Not user-facing, but worth recording for operators:

- arc-node binary built with the Tier 1 code from `TIER1_ONCHAIN_INFERENCE_PLAN.md` Phase A
- Full Llama-2-7B Q4_K_M GGUF (~4 GB) at `~/.arc/models/llama-2-7b.gguf`
- Started with `--model` flag and **without** `--shard-ranges` (so candle backend activates instead of the INT8 integer engine)
- ≥ 8 GB system RAM (4 GB for model, headroom for chain state + OS)
- Stable network — being absent during your committee assignment forfeits that request's reward share

When a validator is alive and configured this way, they will:
- Be eligible for VRF committee selection on every `InferenceRequest`
- Auto-run inference and submit `InferenceVote` when picked
- Earn 70% of `max_reward / committee_size` per successful vote (≈ 1.4 ARC per inference at default settings, assuming ~3 agreeing voters of 5)

### 8.1 GPU is optional, not required

**Tier 1 runs on CPU-only validators.** GPU is a latency optimization, not a correctness or participation requirement.

The five currently-alive coordinators (LAX, AMS, LHR, NRT, SGP) are all CPU-only commodity VPS. They can become full Tier 1 validators without any hardware upgrade — only a model upload and a restart without the `--shard-ranges` flag.

| Resource | CPU validator (existing) | GPU validator (upgrade) |
|---|---|---|
| Latency, 32-token response | 30-40 seconds | 5-10 seconds |
| Output bytes | Identical | Identical |
| Coherent output | Yes | Yes |
| Eligible for committee | Yes | Yes |
| Reward per inference | Same | Same |
| Marginal hardware cost | $0 | +$50-200/mo per node |

**Determinism across CPU and GPU is preserved** because candle's quantized Llama path uses INT4 integer accumulation, which is exact across all hardware (see `crates/arc-inference/src/candle_backend.rs:7`):

> *Deterministic: INT4 accumulation is exact across all hardware.*

This means a CPU validator and a GPU validator in the same committee will produce **bitwise-identical `output_hash` values**. They will always agree. The only difference is how long each one takes to vote.

#### When to add GPU

GPU upgrade becomes worth the cost in three scenarios:

1. **User-facing latency target < 10s.** Real-time chatbot UX needs sub-10s response. Upgrading even one or two committee members to GPU pulls the median wait down (since `min_agreement` votes can finalize early — the slow CPU voters' votes still count if they arrive before the deadline, but the request is no longer blocked on them).
2. **Models larger than Llama-2-7B.** Llama-2-13B and beyond stay marginal on CPU. Llama-70B is effectively unusable without GPU. These fall under future Tier 2+ scope.
3. **High concurrent request load.** GPU batching handles 10x more inferences per second than CPU. Once the testnet sees real traffic, throughput becomes the bottleneck before latency does.

#### Recommended rollout

For the MVP Phase B deployment described in `TIER1_ONCHAIN_INFERENCE_PLAN.md`, **do not pre-provision GPU validators**. Convert the existing 5 CPU coordinators first, validate that consensus reaches and output is coherent, then revisit GPU economics once real usage data is in hand.

If you do choose to add GPU later, one or two GPU validators alongside three CPU validators is enough to drag down the consensus median, because the deadline-based finalize fires as soon as `min_agreement` votes match — fast voters don't have to wait for slow ones.

---

## 9. Comparison table

| Aspect | Today (coordinator) | After Tier 1 |
|---|---|---|
| Who runs inference | 1 coordinator orchestrating a sharded pipeline | 5 validators independently, in parallel |
| Trust assumption | Trust 1 coordinator; challenge fallback exists but rarely used | Byzantine-tolerant: ≥3 of 5 honest |
| Output quality | INT8 path on shard-holders → gibberish (PPL ~144) | Candle Q4 float → coherent Llama-2 quality |
| Verifiability | Single attestation tx | 5 vote txs + 1 finalize tx; full tally on-chain |
| Payment | All `max_fee` goes to one coordinator's role split | Distributed 70 / 20 / 10 (agreeing voters / refund / treasury) |
| Discovery | Hardcoded 6 IPs in desktop binary | Read from chain validator set |
| Coordinator role | Required, hardcoded list, single point of failure | **No coordinator role exists** |
| Latency | 15-60s (often errors before completing) | 30-40s CPU, 5-10s GPU |
| Failure modes | Pipeline gap, dead coordinator, INT8 gibberish | Timeout (refund), disagreement (refund + audit), absent member (degraded but still works) |
| User-visible status | Spinner | Per-validator live progress |

---

## 10. Open questions surfaced by the UX walkthrough

These are decisions that don't block the implementation plan but are worth thinking about before Phase C cutover:

1. **Should the dissent badge show a slash amount in this phase?** Plan says "follow existing" → no auto-slash. Then the dissent row should say "bond locked, awaiting challenge" instead of "bond forfeited". UI text needs adjusting.

2. **Streaming partial output during the `⟳ computing` phase.** Right now the user waits 30-35s with no text. Future: the first voter could attach the partial output blob progressively. Out of Phase A scope but a clear next iteration.

3. **What's the right default `max_reward` value?** 10 ARC is a placeholder. Should be tuned once we have payout-distribution data from real validators. Probably wants to be auto-calculated from recent attestation history.

4. **Mobile / non-Tauri clients.** Plan assumes desktop. The `/inference/onchain/submit` endpoint works for any client that can sign and submit a tx, so a future mobile or web client gets Tier 1 for free.

5. **Backpressure on the validator side.** What if the same 5-validator committee gets selected 10 times back-to-back (VRF clustering)? Plan doesn't address rate limiting per validator. Add to Phase A.3 if observed in load testing.

---

## See also

- **[TIER1_ONCHAIN_INFERENCE_PLAN.md](./TIER1_ONCHAIN_INFERENCE_PLAN.md)** — implementation plan: tx types, state transitions, file-level changes, test strategy, phased rollout
- **[INFERENCE_FLOW.md](./INFERENCE_FLOW.md)** — current-state architecture: coordinator-orchestrated sharded pipeline, problems we're replacing
