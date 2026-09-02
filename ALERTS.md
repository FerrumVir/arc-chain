# ARC Chain — Alert Snapshot (2026-08-17)

**Last checked:** 2026-08-17 (read-only probes of all six seeds)

> **Historical evidence, not current status.** Re-probe every configured origin
> and verify signed rollout receipts before describing the network today. Do
> not carry these v2 observations forward after a v3 cutover.

Four alerts were active at that check. This file previously read "No active
alerts. All clear." while every item below was already true.

---

## ALERT 1 — CRITICAL — Block production stopped on 4 of 6 seeds ~6 days ago

`/block/latest` timestamps, measured 2026-08-17:

| Seed | Host | Version | Height | Last block sealed |
|---|---|---|---|---|
| NYC | 149.28.32.76 | **0.7.2** | 135,058 | **29 seconds ago** |
| LAX | 140.82.16.112 | 0.7.9 | 123,469 | 6.6 minutes ago |
| AMS | 136.244.109.1 | 0.7.9 | 92,897 | **152.3 hours (6.3 d)** |
| LHR | 104.238.171.11 | 0.7.9 | 51,386 | **161.0 hours (6.7 d)** |
| NRT | 202.182.107.41 | 0.7.9 | 96,726 | **152.3 hours (6.3 d)** |
| SGP | 149.28.153.31 | 0.7.9 | 97,548 | **152.3 hours (6.3 d)** |

AMS, NRT and SGP stopped within **620 milliseconds of each other**
(timestamps 1786395665977 / 1786395666596 / 1786395665990). That is one
upstream event, not three independent failures.

DAG rounds keep advancing on all six at ~0.2/s, uniformly, which is why every
node still answers `{"status":"ok","syncing":false}`. Round progress and block
commit are separate paths; nothing in `/health` surfaces the stall.

**Consequences right now:**
- An inference attestation submitted today is accepted into the mempool and
  never mined. It reads `block_height: null` and the `explorer_url` in the
  response points at a transaction that does not exist in any block.
- `/worker/earnings` returns `total_attestations: 0` for every address.
- Any demo step that follows a transaction to the chain dead-ends.

**Mitigation for a demo:** pin NYC (`149.28.32.76`) — it is the only seed
reliably sealing. Note that NYC is also the *oldest* binary on the network.

**Do not** restart the stalled seeds to "fix" this. A restart wipes the worker
scoreboard, the in-memory `inference_results` map (LHR's 15 recorded inferences
are the network's entire stock of genuine sharded output), and every
`sharded_runs_total` counter — all of which live in process memory with no WAL
entry or snapshot.

---

## ALERT 2 — HIGH — Poisoned latency EWMA is steering traffic away from the fastest node

Every one of the six seeds independently rates LHR at **37,276–44,259 ms per
hop**, with sample ages of 33,886–39,044 seconds (9–11 hours stale). LHR's own
recorded traces show it serving hops in **180–410 ms** — two orders of
magnitude faster than its rating.

The failure is self-reinforcing: replicas are sorted ascending by EWMA, so LHR
sorts last in every layer range it holds, is never dispatched, never gets a
fresh sample, and the stale value never decays.

**Measured cost:** LHR holds `[6,12)` and `[12,17)`. A sanctioned run routed
around it and paid **14,478 ms** and **16,480 ms** on cold AMS replicas for
exactly those two ranges — ranges LHR serves in ~230 ms and ~180 ms.

**Mitigation:** none available read-only. Be aware that a "slow network" during
a demo may be this, not genuine congestion.

---

## ALERT 3 — HIGH — NYC is two minor versions behind the rest of the network

NYC reports `version 0.7.2`; the other five report `0.7.9`. No v0.7.8 or v0.7.9
release exists — the five newer seeds run a binary built out-of-band from
branch `fix/v078-attestation-wire-compat`.

The v0.7.9 branch exists specifically to carry **InferenceAttestation
wire-compatibility with v0.7.2 validators**, which makes NYC precisely the peer
that needed the compatibility shim, and cross-version hops the least-tested
path on the network.

This collides directly with ALERT 1: NYC is simultaneously the only seed
sealing blocks and the oldest binary. Pinning it for chain reads is right;
routing inference through it is not.

`arc-pick-coordinator.sh` used to return NYC on every invocation. It now ranks
by version and attestation data and returns LHR.

---

## ALERT 4 — MEDIUM — Worker scoreboard is split-brain across seeds

`/workers/scoreboard`, all read within the same minute on 2026-08-17:

| Seed | count_total | count_visible |
|---|---|---|
| NYC | 6 | 5 |
| LAX | 6 | 5 |
| LHR | 9 | 5 |
| **AMS** | **0** | **0** |

Worker registrations are node-local, not replicated — the same worker carries a
different `registered_at` on different seeds. A dashboard pointed at AMS renders
an empty worker list.

Two aggravating details:

- The `:3001` community gateway is **not** a source for this data. `GET
  http://136.244.109.1:3001/workers/scoreboard` returns **404**. Read the
  scoreboard from port **9090** on a seed with `count_visible > 0`.
- Calling `GET /community/list` **prunes** the registry as a side effect, while
  `/workers/scoreboard` does not. Calling `/community/list` first is what
  empties the scoreboard you are about to display. Do not call it before a
  demo.

---

## Standing issues (not new, tracked elsewhere)

- **Seeds are independent chains.** `/block/43000` returns a different hash on
  every seed; heights span 51 K–135 K. See
  `docs/TESTNET_STATE_DIVERGENCE_2026-06-03.md`. Repair plan
  `docs/superpowers/plans/2026-06-04-replicated-chain-model-1.md` is unstarted.
- **Tier-1 on-chain inference never lands in a block.** See
  `docs/INFERENCE_TIER1_INVESTIGATION_2026-06-04.md`. UI removed in v0.7.11.
- **`/models` reports false coverage** (`covered_layers: 96` vs
  `total_layers: 32`) by summing replica spans instead of taking their union.
  `/shards` is correct.
- **Shard registry advertises `socket_addr 0.0.0.0:9090`** (GH #27, open since
  2026-04-16).
- **`/inference/attestations` pads its list** with unrelated transactions tagged
  `tx_type: "Other"` once real rows run out — 50 of 50 rows on LAX today.
- **`gpu.available: true` names `llvmpipe`**, a CPU software rasterizer. There
  is no GPU on these VPS.
- **The community installer was dead** until 2026-08-17: it resolved
  `releases/latest` (v0.7.11, desktop-only, no CLI asset) and 404'd. Fixed in
  `scripts/install-community-node.sh`.
