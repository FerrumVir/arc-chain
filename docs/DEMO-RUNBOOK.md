# ARC Chain — Demo Runbook

> **Recovery notice (2026-08-26):** this live-network runbook records the old
> v0.7.2/v0.7.9 fleet and is not the v0.8.0 launch walkthrough. v0.8.0 is
> still unpublished and undeployed. Do not use the legacy v0.7.7 installer
> path below; the gated replacement is
> [`COMMUNITY-NODE-WALKTHROUGH.md`](COMMUNITY-NODE-WALKTHROUGH.md) after
> [`VALIDATOR-FLEET-ROLLOUT.md`](VALIDATOR-FLEET-ROLLOUT.md) completes.

**The run-of-show.** Everything here was checked read-only against the live
network on **2026-08-17**. This supersedes `docs/SERO-DEMO.md`, which describes
a 7-node topology that no longer exists.

Read [`ALERTS.md`](../ALERTS.md) before you start. Two active alerts shape every
decision in this document: **block production is stalled on 4 of the 6 seeds**,
and **the seeds are independent chains, not replicas**.

---

## Legend — what runs where

Every step is tagged. Do not skip these tags; several segments only exist on
one side of the line.

| Tag | Meaning |
|---|---|
| 🟢 **LIVE** | Works today against the public v0.7.9 seeds. |
| 🔵 **BRANCH** | Needs the `demo-hardening` binary. Will not work against the live seeds. |
| 🟡 **LOCAL** | Runs against your own local node, not the public network. |

The single most important consequence: **`force_recompute` and
`POST /node/threads` do not exist on the live seeds.** I verified
`GET /node/threads` returns **404** on NYC and LHR while `GET /eth` on the same
router returns 405, so that is a genuine route absence, not a method mismatch.

---

## The two pins

You need **two different hosts**, for two different reasons. Getting this
backwards is the most likely way to break the demo.

| Purpose | Host | Why |
|---|---|---|
| **Chain + wallet reads** | **NYC `149.28.32.76:9090`** | The only seed reliably sealing blocks — last block **29 seconds** old at check time. Balances, faucet, and any attestation you want mined must go here. |
| **Inference coordination** | **LHR `104.238.171.11:9090`** | The only seed with a real attestation history (15 records), and it serves hops in **180–410 ms**. This is what the repaired `arc-pick-coordinator.sh` now returns. |

```bash
export ARC_WALLET_SEED=http://149.28.32.76:9090     # NYC — blocks and balances
export ARC_COORDINATOR=http://104.238.171.11:9090   # LHR — inference
```

NYC is simultaneously the only seed producing blocks **and** the oldest binary
on the network (v0.7.2 against v0.7.9 elsewhere). That is why the two roles are
split: pin NYC for chain reads, but do not route inference through it.

### On `ARC_TIER1_RPC` — a correction

You may have been told this variable must not be set. That is backwards.

`ARC_TIER1_RPC` is the desktop's override for `wallet_host`. Left unset, the
desktop **hard-pins `WALLET_HOSTS[0]` = LAX**, and LAX is not the seed you want
for a balance demo today. Set it deliberately:

```bash
export ARC_TIER1_RPC=149.28.32.76:9090   # match ARC_WALLET_SEED above
```

It repoints balance, faucet, earnings, and attestation reads **together**, so it
is the single switch that decides which chain your wallet demo is reading. The
rule is not "never set it" — the rule is **never leave it pointing at a seed you
have not just verified**.

Ignore the code comment near it claiming the seeds "form a real multi-validator
consensus network … so reading from any one of them returns consistent state."
That comment is wrong; see `docs/TESTNET_STATE_DIVERGENCE_2026-06-03.md`.

---

## (a) PREP

### Day before

**1. Pre-place the model and the binary. 🟡**

Do not let anything download 4 GB on stage, and do not rely on the desktop's
auto-download. The desktop force-redownloads `~/.arc/bin/arc-node` from GitHub
`releases/latest` on **every** `start_node` whenever the binary's `--version`
string differs from the desktop's own version. A hand-built demo binary dropped
there is silently replaced.

```bash
ls -la ~/.arc/bin/arc-node && ~/.arc/bin/arc-node --version
ls -la ~/.arc-models/*.gguf
```

If you are demoing the branch build, either match the desktop version string or
launch the node yourself rather than through the desktop's Start button.

**2. Confirm the recovery installer contract locally. 🔵**

```bash
bash install.sh --help
bash tests/release/run.sh
```

At the 2026-08-17 audit, v0.7.7 was the last public release with CLI binaries;
it is historical, not the repaired “latest CLI.” v0.7.10 and v0.7.11 are
desktop-only. The unreleased v0.8.0 candidate adds checksummed Linux
x86_64/arm64 and macOS arm64/x86_64 headless assets, but no install command may
be presented as live until that complete release is published.

**3. Rehearse the exact prompts you will type.** See the prompt list below.

### Hour before

**4. Verify block freshness on your wallet pin. 🟢**

This is the check that decides whether the income segment is possible at all.

```bash
curl -s $ARC_WALLET_SEED/block/latest | python3 -c '
import json,sys,time
d=json.load(sys.stdin)
age=(time.time()*1000 - d["timestamp"])/1000
print(f"height {d[\"height\"]}  last block {age:.0f}s ago")'
```

- **< 120 s** — good, the income segment can show a mined attestation.
- **> 600 s** — NYC has stalled too. Fall back to LAX, and if LAX is also
  stale, cut the "mined" half of the income segment and show mempool
  acceptance only (see the failure playbook).

Reference readings at check time: NYC 29 s, LAX 6.6 min, and AMS / LHR / NRT /
SGP **6.3–6.7 days**.

**5. Warm the shards — but warm them with a THROWAWAY prompt. 🟢**

This is the most important prep step and the easiest to get wrong.

There are two separate caches, and confusing them will delete the visual you
most want to show:

- **Shard warmth** is per node. A node that has not served recently pays a
  large first-hit penalty loading weights — a measured cold run spent
  **14,478 ms** and **16,480 ms** on two layer ranges that a warm node serves
  in ~230 ms and ~180 ms. Warming is what buys you ~2.2 s/token instead of
  ~9.6 s/token.
- **The result cache** is per `(model, prompt, max_tokens)`. Warming *the exact
  prompt you will demo* means the on-stage request is answered from cache in
  microseconds — and on the live v0.7.9 seeds a cache hit returns
  `total_ms: 0`, `total_bytes_transferred: 0`, and **`shard_trace: []`**.

**So: warm the pipeline with throwaway prompts, then demo a fresh one.** You
get warm shards *and* a real per-hop trace.

```bash
# Warm the shards. Throwaway prompts - do NOT use your demo prompts here.
for p in "What is a Merkle tree?" "What is on-chain AI?"; do
  curl -s -m 300 -X POST $ARC_COORDINATOR/inference/run_sharded \
    -H 'Content-Type: application/json' \
    -d "$(python3 -c 'import json,sys;print(json.dumps({"input":sys.argv[1],"max_tokens":8}))' "$p")" \
    | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("ms_per_token"),"ms/token")'
done
```

🔵 On the branch build this trap disappears: cache hits carry the original run's
trace and attestation through `sharded_run_meta`, and `sharded_cache_hits` is
counted separately from `sharded_runs_total`. On the branch you may warm the
real demo prompts.

**6. Confirm the coordinator picker agrees with you. 🟢**

```bash
ARC_PICK_VERBOSE=1 bash scripts/arc-pick-coordinator.sh
# expect: http://104.238.171.11:9090
```

**7. Check the worker scoreboard is not empty on the seed you will show. 🟢**

```bash
curl -s $ARC_WALLET_SEED/workers/scoreboard | head -c 200
```

Readings at check time: NYC 6/5, LAX 6/5, LHR 9/5, **AMS 0/0**. A dashboard
pointed at AMS renders no workers at all.

> **Do not call `GET /community/list` beforehand.** That handler *prunes* the
> registry as a side effect; `/workers/scoreboard` does not. Calling it is what
> empties the list you are about to display.

### The prompts

These are verified against the recorded attestation set on LHR. Every one was
read read-only from `/inference/attestations`.

#### ✅ Use these four

| Prompt | Recorded output | Output hash |
|---|---|---|
| `What is a blockchain?` | "A blockchain is a decentralized, digital ledger technology that" | `0x663882ae…` |
| `How do validators work?` | "Validators are an essential component of a blockchain network, and how" | `0xa1c77ecd…` |
| `What is a DAG in blockchain?` | "A DAG (Directed Acyclic Graph) is a directed" | `0xa2b7ccec…` |
| `What is a smart contract?` | "A smart contract is a computerized contract with the terms and conditions of" | `0x01c42ab3…` |

Backups, also verified clean: `What is cross-chain bridging?`,
`How does staking work?`, `What is a Merkle tree?`, `What is on-chain AI?`,
`What is post-quantum cryptography?`. And `The largest planet is` — the
`arc-demo.sh` default — returns " Jupiter, which is more than ".

#### ✅ Re-vetted live 2026-08-17 through the branch coordinator

The four above were read from LHR's *recorded* set. These were run fresh
through a local v0.7.11 coordinator against the live seeds (`redundancy: 2`,
`force_recompute: true`, no `[INST]` wrapper, 6 tokens), so they are verified
on the path the demo actually uses. All four hashes are distinct, which is what
the isolation check needs.

| Prompt | Output | Hash | Wall |
|---|---|---|---|
| `The largest planet is` | " Jupiter, which is more" | `0xb8bfa3d0…` | 14.3 s |
| `Water boils at` | " 100 degrees Cel" | `0xcd54aff0…` | 14.1 s |
| `The sun is a` | " star, and the stars are" | `0x6dee54b8…` | 13.2 s |
| `Bitcoin is a` | " digital and decentralized currency" | `0xc3ae1c6b…` | 14.2 s |

Two more answer **correctly** but render raw newline tokens, so they read badly
on a projector even though the model is right — keep them as spares, not
openers: `The capital of France is` → " Paris.`<0x0A><0x0A><0x0A>`The", and
`The first president of the United States was` → " George Washington.`<0x0A>`
He was".

Note the shape of a sentence-completion prompt: this is a **base** model, so
`The largest planet is` completes naturally while an instruction phrasing
invites the newline spam documented below.

The recorded runs wrap prompts as `[INST] <prompt> [/INST]`. Match that format
if you want to hit a recorded hash.

#### ⛔ Never type these

Six of the fifteen recorded prompts produce degenerate output, and all four
phrased `Explain <topic>` are among them:

| Prompt | What it actually returns |
|---|---|
| `Explain token economics` | 14 consecutive newline tokens, nothing else |
| `Explain cryptographic hashing` | 14 consecutive newline tokens, nothing else |
| `Explain zero-knowledge proofs` | 8 newlines, then the German word "Unterscheidung" |
| `Explain digital signatures` | 6 leading newlines |
| `What is deterministic AI inference?` | trails into 7 newlines |
| `How does consensus work?` | 10 leading newlines, then "ConSensus is" |

All six are recorded as `success: true, deterministic: true`. The engine is
reproducibly wrong, which is exactly the distinction to keep straight.

> **The collision.** `Explain token economics` and `Explain cryptographic
> hashing` — two completely different prompts — produce the **same output
> hash, `0xd52b2af8f39ea4e6`**. If those two ever landed in the isolation
> check, it would report that different prompts produced identical hashes. Keep
> both off the stage.

**Take no prompts from the audience.** Offer them a choice from your verified
list instead.

---

## (b) SAFE-JOIN

**Goal:** show a laptop joining the live network, live, without touching
consensus.

### The exact flags 🟢

```bash
~/.arc/bin/arc-node \
  --rpc 127.0.0.1:9944 \
  --p2p-port 9945 \
  --seeds-file ~/.arc/seeds.txt \
  --genesis ~/.arc/genesis.toml \
  --validator-seed "community-demo-$(openssl rand -hex 4)" \
  --stake 0 --min-stake 0 \
  --community-mode
```

`scripts/install-community-node.sh` generates exactly these flags in the
launchd plist and the systemd unit, so "what the installer does" and "what I am
typing" are the same thing. `--community` is the shorthand: it forces
`stake = 0` and `community_mode = true` in one flag.

### What the audience sees

The status pill goes `connecting` → `syncing`, the Network tab lists the six
seeds, and the node appears in `/community/list` on a seed within ~60 s
(registration ticks every 60 s, heartbeat every 15 s).

> **Set expectations out loud:** the local node's own **height will sit at 0**
> for a long time, possibly the whole demo. A fresh node starts at DAG round 0,
> and it will not fast-forward from one peer's claimed round or a far-ahead
> signed block. A migrated validator needs the operator-approved genesis or a
> quorum-certified checkpoint. Do not present an unauthenticated snapshot or
> round hint as consensus catch-up.

### Why this cannot disturb consensus

Say this plainly, because it is the interesting part:

A transport peer's advertised stake is never consensus authority. Only the
operator-approved genesis/checkpoint identities and stakes enter the live and
frozen `ValidatorSet` or the RPC validator authority list. Community inference
registration is a separate worker path and cannot change voting membership.

### What WOULD be dangerous — do not improvise these

1. **Changing membership outside the approved trust root.** The earlier
   transport path trusted self-declared handshake stake and polluted the frozen
   set. The recovery candidate rejects that path. Do not work around it by
   editing one node's genesis, validator list, or stake: unequal trust roots
   deliberately fail to reach quorum.

2. **Auto-shard-join on the public net.** The trigger is
   `stake > 0 && --model is set && no explicit shard range`. It POSTs
   `/shards/join` to the first seed in the seeds file (NYC), which inserts the
   announcement verbatim with **no stub-address check**. Because the live
   pipeline is already fully covered, it assigns `[0, 8)` — off the existing
   6-range tiling — and the v0.7.9 assembler then aborts with
   `503 Pipeline gap: expected layer 6 next, got shard [0, 8)`. That kills
   sharded inference on the seed you are demoing against, caused by the demo
   itself. `--stake 0` breaks the trigger; so does omitting `--model`.

3. **Restarting any seed.** See the failure playbook.

---

## (c) PARALLEL INFERENCE SEGMENT

**Goal:** show that the model genuinely does not fit on one machine, and that
the pipeline is real.

### Run it 🟢

Use a **fresh** clean prompt — not one you warmed.

```bash
bash scripts/arc-demo.sh
```

or by hand:

```bash
curl -s -m 300 -X POST $ARC_COORDINATOR/inference/run_sharded \
  -H 'Content-Type: application/json' \
  -d '{"input":"[INST] What is a blockchain? [/INST]","max_tokens":12}' \
  | python3 -m json.tool
```

### What to point at

**1. The topology, before anything runs.**

```bash
curl -s $ARC_COORDINATOR/shards | python3 -c '
import json,sys
from collections import defaultdict
d=json.load(sys.stdin); g=defaultdict(list)
for s in d["shards"]: g[(s["start_layer"],s["end_layer"])].append(s["node_name"])
for k in sorted(g): print(f"  [{k[0]},{k[1]})  " + " · ".join(sorted(g[k])))'
```

Verified output — 18 shards, 6 ranges, 3 replicas each, full 0..31 coverage:

```
  [0,6)   AMS · LAX · NYC
  [6,12)  AMS · LAX · LHR
  [12,17) AMS · LHR · NRT
  [17,22) LHR · NRT · SGP
  [22,27) NRT · NYC · SGP
  [27,32) LAX · NYC · SGP
```

Each node holds 15–17 layers, about 2.9–3.3 GB. **No node has the whole model.**
That is the claim, and `/shards` proves it in one screen.

**2. The per-hop trace.** Six hops, each a real cross-network HTTP call, each
verifying the previous hop's BLAKE3 hash before computing. Point at the
`node` column changing city by city.

**3. Redundancy and hedging.** Every range has three replicas; the coordinator
keeps a rolling latency EWMA and dispatches to the fastest. Because the engine
is deterministic, *which* replica answers cannot change the output — so
failover is free. Contrast with the old sequential path, which had one holder
per range and no fallback.

### Be ready for these three questions

- **"Why is the trace only ~half the wall time?"** Because it is. The trace
  samples the **prefill** pass only. The per-token decode loop makes the same
  six hops again for every token and is not represented. On a measured run the
  six traced hops summed to 36,816 ms against a reported total of 76,737 ms —
  **48%**. The repaired `arc-demo.sh` now prints this percentage itself rather
  than letting someone discover it. Say it before they ask.
- **"Why does payload say 0 KB?"** The server emits `payload_bytes: 0` as a
  literal for every hop. The response-level `total_bytes_transferred` is the
  real figure. The repaired script prints `n/a` instead of a misleading `0.0 KB`.
- **"Is this running on GPUs?"** No. `/info` reports `gpu.available: true` while
  naming **`llvmpipe`**, which is Mesa's CPU software rasterizer. These are
  CPU-only VPS. Do not read that field aloud as evidence of GPU acceleration —
  the ~10 s/token sitting next to it makes it indefensible.

### The determinism step — read this before you run it

🟢 On the live seeds, step 3 will print **`● SERVED FROM CACHE (hash match)`**,
not `✓ DETERMINISTIC`. That is correct and intended. The old script re-POSTed
the same prompt and declared victory when the hashes matched — but the second
call is an LRU cache lookup. On a measured run, run 1 took 76,737 ms and run 2
returned in **821 microseconds** with an empty trace: a 93,468× speedup that
proves only that a cache returns what was put in it. Across LHR's entire
history, ~95% of recorded sharded runs were cache hits.

Say it in the room: *"This second number is the cache, not the pipeline. Here
is what it looks like when we actually force a recomputation."*

🔵 On the branch build, `force_recompute` makes the rerun a genuine second
pipeline walk and the script prints `✓ DETERMINISTIC` — two independent walks,
same 32 bytes. **This is the segment that most benefits from demoing the branch
binary.**

### Do not claim more than this

If asked how the output is verified, the honest answers are:

- `model_hash` is BLAKE3 of the **string** `arc-32L-4096d-32h-32000v`, not of
  any weights. A node serving entirely different weights under the same shape
  produces an attestation that verifies identically.
- The attestation transaction is submitted **unsigned**, with `sig_verified`
  forced true.
- The **VRF committee never votes.** Members are selected and recorded; no
  votes are requested or aggregated. The `corruption_probability` field
  describes machinery that did not run.

Have this answer ready, or steer away from the verification question. Do not
improvise it.

---

## (d) ADD-TWO-CORES SEGMENT

**Goal:** show the node scaling up live — Settings slider, more cores, visible
change.

> ### 🔵 BRANCH ONLY — this segment does not work against the live seeds.
> I verified `GET /node/threads` returns **404** on both NYC and LHR, while
> `GET /eth` on the same router returns 405. The route does not exist on the
> deployed binary. The node state fields (`compute_pool`, `compute_threads`)
> and the endpoint land on this branch.

### On the branch build

1. Open **Settings**, move the worker-threads slider from *n* to *n + 2*.
2. The desktop issues `POST /node/threads` with the new width. The node rebuilds
   its dedicated rayon compute pool **in place** — no restart, no reconnect, no
   lost peers.
3. On screen: the active-cores readout updates immediately, the node stays
   `live` throughout (no `connecting` flicker), and the next local inference
   shows lower `compute_ms`.

Point out what did *not* happen: no restart. Rebuilding the pool in place is the
whole point — a restart would drop peers and, on a seed, would wipe in-memory
state.

### Fallback if you must show this on a stock binary 🟡

The desktop checks `binary_supports_flag(binary, "--threads")` and falls back to
setting `RAYON_NUM_THREADS` when the flag is absent. That path **requires a node
restart** to take effect. Demo it on a **local** node only:

```bash
RAYON_NUM_THREADS=8 ~/.arc/bin/arc-node --rpc 127.0.0.1:9944 --stake 0 --community-mode
```

Narrate it as "this is the version that needs a restart; the live-reconfigure
path is what we just shipped." Do not restart a seed to demonstrate this.

---

## (e) INCOME SEGMENT

**Goal:** tell the earnings story without saying anything false. This segment
has the most ways to go wrong, so the framing matters more than the clicks.

### ⛔ RETIRED INCOME DEMO — DO NOT REHEARSE OR RECORD

The former “strong version” credited 2.5 ARC from the raw
`InferenceAttestation` (`0x16`) path on an isolated local chain. That is not the
v0.8.0 reward contract and must not be presented as evidence that community
workers are paid. In the recovery candidate:

- `0x16` is a computation claim and **never pays a reward**;
- payment is a separate `CommunityInferenceReward` transaction (`0x25`);
- the worker certificate binds the exact model artifact, assignment, output,
  worker signature, payout, and verification evidence;
- authorization requires unique active-validator approvals covering strict
  greater-than-two-thirds of both validator identities and active stake (five
  approvals for six equally staked validators);
- genesis activation, the local issuance switch, and validator-approval
  collection must all be ready; and
- earnings count only a **successful mined `0x25` receipt** retained by the
  selected host.

Approval collection is intentionally unavailable in this candidate, so the RPC
fails closed and reports the effective reward gate disabled. It neither
synthesizes validator approvals from shard signatures nor exposes a signing
oracle. Therefore there is no valid income recording to make before the
approved v3 genesis, validator key rotation, peer-authenticated approval
aggregation, and coordinated fleet cutover all complete.

After those gates are complete, use
[`COMMUNITY-NODE-WALKTHROUGH.md`](COMMUNITY-NODE-WALKTHROUGH.md). The only
defensible payment sequence is: show the readiness fields all true; show exact
artifact eligibility and assigned, quorum-verified work; show the returned
`reward_tx_hash`; prove that hash is transaction type `0x25` with a successful
receipt on the same host; then show `/worker/earnings/{address}` reconcile to
that retained receipt. Anything less is a work demo, not an income demo.

---

### The honest story, in order

**1. Faucet credit — this is a real balance change. 🟢**

```bash
curl -s $ARC_WALLET_SEED/faucet/status
# verified: {"claim_amount":10000,"rate_limit_secs":60,...}
```

**10,000 ARC** per claim, **60-second** cooldown per address. Verified live on
both NYC and LAX. The faucet path has been validator-signed since v0.7.1, which
is why it propagates when coordinator-minted attestations do not.

Claim it, then read the balance back **from the same host**:

```bash
curl -s $ARC_WALLET_SEED/account/$ADDR
```

**2. Inference claims are not payments. 🟡**

An inference produces an `InferenceAttestation` (tx `0x16`) carrying model hash,
input hash, and output hash. Even if one reaches a block successfully, its
reward is exactly zero. It is evidence of a submitted computation claim, not a
`CommunityInferenceReward` receipt. The Aug 26 public-fleet snapshot found
mixed v0.7.2/v0.7.9 binaries, divergent state on all six seeds, and no community
work recorded; do not use a public `0x16` record as settlement evidence.

**3. Public and candidate earnings endpoints mean different things. 🟡**

```bash
curl -s $ARC_WALLET_SEED/worker/earnings/$ADDR
```

On the public v2 seeds, the number is legacy **display arithmetic** based on raw
attestation count. It is not income. In the unreleased v0.8.0/v3 candidate,
the endpoint instead counts retained successful mined `0x25` reward receipts.
Be precise about the public response:

- It is **display arithmetic**, not a balance. Nothing in that handler reads or
  writes an on-chain balance, so it **will not reconcile** against
  `/account/{addr}`. Do not put the two on screen together and imply they
  should match.
- The count comes from an **in-memory transaction map that gets pruned**, so
  lifetime earnings can go **down** between two refreshes, and reset to zero
  when a node restarts.
- **"Today" is fabricated** as 12% of lifetime, on both the node and the
  desktop. A worker with one lifetime attestation shows identical Today and
  Lifetime figures.
- The Aug 26 read-only snapshot returned community `total_work_completed: 0`;
  no public reward receipt was demonstrated.
- Numbers are **per-seed**, because chain state is per-seed.

The defensible framing: *“The public endpoint is legacy accounting and does not
prove payment. The candidate has a separate 0x25 reward transaction and mined-
receipt index, but issuance is deliberately disabled until validator approval
collection and the coordinated v3 cutover are ready.”* Anything stronger is
not supported by current evidence.

### ⛔ DO-NOT-SHOW LIST

Each of these exposes a known defect in under a minute.

1. **Cross-seed balance comparisons.** The seeds are separate chains. A faucet
   credit on one does not appear on another. Never curl a second seed for the
   same address.
2. **`/block/N` on two seeds.** `/block/43000` returns a **different hash on
   every seed**; heights span 51 K to 135 K. This is the fastest way to expose
   the divergence.
3. **Tier-1 on-chain submit.** It returns HTTP 200 and then nothing happens —
   the signer nonce never moves and the tx never lands in a block. The UI was
   removed in v0.7.11 rather than fixed. There is no version of this that demos
   well.
4. **The dashboard's dead tiles.** The deployed `:3200` page still references
   retired hosts and pre-April copy. The repo copy is fixed; **the live one has
   not been redeployed.** Do not open its page source, and do not paste the link
   into anything that renders a preview card — the OG description still says
   "7 separate VPS in 7 cities."
5. **`/models`.** Reports `fully_covered: false` with `covered_layers: 96`
   against `total_layers: 32`, because it sums replica spans instead of taking
   their union. Show **`/shards`** instead, which computes it correctly and says
   `true`.
6. **The raw GPU field.** `gpu.available: true` naming `llvmpipe`.
7. **Unvetted prompts.** See the forbidden list above.
8. **`/inference/attestations` on any seed but LHR.** Once real rows run out the
   handler pads the list with unrelated transactions tagged `tx_type: "Other"`.
   On LAX today, **50 of 50 rows** are padding — no output hash, no model, no
   trace. The repaired `arc-verify.sh` filters these; the raw endpoint does not.
9. **Clicking through an `explorer_url`.** It points at a transaction that was
   never mined.
10. **`/economics/revenue_split` on two seeds.** The same 10,000-unit fee splits
    three different ways (`per_verifier` 227 / 277 / 192) because the worked
    example derives from each seed's local validator count.
11. **`/validators` counts.** 12 on NYC, 11 on LAX, 14 on the others — and four
    of the fourteen have stake 0 yet are still counted.

---

## (f) FAILURE PLAYBOOK

### A hop stalls or the inference hangs

**Do not restart anything.** First, work out whether it is a slow hop or a dead
one:

```bash
curl -s -m 5 $ARC_COORDINATOR/health
```

**Blame-shift order — move the coordinator, never the network:**

1. **LHR** `104.238.171.11` — default; fastest hops.
2. **AMS** `136.244.109.1` — holds three ranges `[0,6) [6,12) [12,17)`, so it
   serves half the pipeline locally.
3. **NYC** `149.28.32.76` — last resort for inference. It is v0.7.2, so
   cross-version hops are the least-tested path on the network.

```bash
export ARC_COORDINATOR=http://136.244.109.1:9090
```

Then re-run. Switching coordinator is a one-line change the audience never sees
as a failure.

**If it is slow rather than stuck, it may be the poisoned router.** Every seed
rates LHR at **37–44 seconds per hop** from a sample 9–11 hours stale, while LHR
actually serves in 180–410 ms. Because replicas sort ascending by that EWMA, LHR
sorts last, is never dispatched, never resampled, and the stale value never
decays. The visible symptom is the pipeline routing onto cold replicas and
taking 15+ seconds on ranges LHR would serve in 200 ms. Nothing you can fix from
the stage — just know that "the network is slow" may be this.

**Restart the LOCAL node only, never a seed.** Restarting a seed destroys, with
no WAL entry and no snapshot:

- the worker scoreboard (all counts, latencies, registrations),
- the in-memory `inference_results` map — **LHR's 15 recorded inferences are the
  network's entire stock of genuine sharded output**,
- every `sharded_runs_total` counter,
- `/worker/earnings`, which scans a map that is empty after restart,
- and pre-2026-06-04 Tier-1 escrows, which cannot be recovered and stay stuck
  holding funds forever.

### The attestation doesn't mine inside the demo window

Expected on four of six seeds, and possible even on NYC. Handle it in the open:

```bash
# 1. Show it was accepted - this part is real
curl -s -X POST ... /inference/run_sharded | python3 -c '
import json,sys;d=json.load(sys.stdin)
a=d.get("attestation",{});print(a.get("tx_hash"),a.get("status"))'
# -> "submitted_to_mempool"

# 2. Show it has not been mined - do not hide this
curl -s "$ARC_WALLET_SEED/inference/attestations?limit=5" | python3 -c '
import json,sys
for r in json.load(sys.stdin)["attestations"][:5]:
    print(r.get("tx_hash","")[:18], "block:", r.get("block_height"))'
# -> block: None
```

**The line to use:** *"The attestation is constructed, signed into the mempool,
and addressable by hash. It hasn't been included in a block yet — four of our
six seeds stopped sealing about six days ago and we're mid-repair. This is a
liveness problem in block production, not a problem with the inference or the
attestation."*

That is accurate and it is a better answer than a dead explorer link. **Do not
click the `explorer_url`** — it points at a transaction that does not exist in
any block.

If you need a mined attestation, submit it to **NYC**, the only seed reliably
sealing, and check `/block/latest` freshness first (prep step 4).

### The worker list renders empty

You are pointed at AMS (`count_total: 0`). Switch to NYC, LAX, or LHR. And
confirm nothing called `/community/list` first — that handler prunes the
registry as a side effect.

### The node restarts and sits at 0 peers 🔵 BRANCH

**Measured 2026-08-17.** Stopping a node and immediately restarting it **on the
same P2P port** produced `peers: 0` and stayed there for minutes, while
`dag_round` kept advancing (so the node looks alive and the failure is easy to
miss). A node started at the same moment on a *fresh* P2P port reached 7 peers
in seconds, and moving the restarted node to a fresh port took it to 3 peers in
19 seconds. The seeds appear to hold the old `(ip, port)` entry for a while and
ignore the re-handshake.

This matters because it is exactly what the Dashboard's **Restart** button
does. If you restart mid-demo and the peer count sticks at zero:

1. Don't keep clicking Restart — it will reuse the same port every time.
2. Change the P2P port in Settings (any unused value) and start again, or
3. Wait it out rather than restarting, if the node is otherwise serving.

Prefer **not restarting during the demo at all**. Nothing in the run-of-show
requires it, and a healthy node that has already peered will stay peered.

### The desktop replaces your binary

The desktop force-redownloads `~/.arc/bin/arc-node` from `releases/latest` on
every `start_node` when the version string does not match its own. If
`releases/latest` is not tagged to match, this becomes an **infinite redownload
loop on every start**. Launch the node from the command line instead of the
Start button for the rest of the demo.

---

## One-screen cheat sheet

```bash
export ARC_WALLET_SEED=http://149.28.32.76:9090     # NYC  - blocks, balances
export ARC_COORDINATOR=http://104.238.171.11:9090   # LHR  - inference
export ARC_TIER1_RPC=149.28.32.76:9090              # desktop wallet pin

# hour-before checks
curl -s $ARC_WALLET_SEED/block/latest | head -c 120     # want < 120s old
curl -s $ARC_WALLET_SEED/workers/scoreboard | head -c 120
bash scripts/arc-pick-coordinator.sh                    # want LHR
# warm shards with THROWAWAY prompts, never the demo ones (live seeds)

# safe join
--stake 0 --min-stake 0 --community-mode

# blame-shift order for a stalled hop
LHR 104.238.171.11  →  AMS 136.244.109.1  →  NYC 149.28.32.76
```

**Never:** restart a seed · join with stake > 0 · auto-shard-join the public
net · compare balances across seeds · type an `Explain …` prompt.
