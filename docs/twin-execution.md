# Twin execution v0, region tags v0 and community demand

Status: draft design and coordinator-side implementation. **Every switch is off by default.**
There are no consensus, genesis or on-chain rule changes. Code: `crates/arc-node/src/twin.rs`
(pure rules) and `crates/arc-node/src/rpc/twin_dispatch.rs` (coordinator orchestration).

## 1. Problem

On `main` (4070114c6):

- **Validators check every job.** The six team-run validators, all on one hosting provider, assign and re-check every community job.
- **Workers sit idle.** Installed community workers wait, because the only demand is a caller's `/inference/run`. Live `/workers/scoreboard` on 2026-10-06 showed 3 eligible workers and 15 lifetime successes, with no new work since 2026-10-04.
- **"Checked by independent operators" is not true yet.** The only checkers are the team's validators.

This design adds three things:

- **Twin execution.** Each community job runs on two community workers with distinct node keys, and their outputs are compared byte for byte. Validators only arbitrate disagreements and take a small, unpredictable sample.
- **Region tags.** A measured-latency label, so twins prefer different places.
- **Rate-limited demand.** Idle workers get real jobs: public demo prompts and replays of verified answers.

## 2. The current community job flow (main @ 4070114c6)

`rpc.rs` means `crates/arc-node/src/rpc.rs`; `main.rs` means `crates/arc-node/src/main.rs`.

| Step | What happens | Where |
|---|---|---|
| Register and heartbeat | The worker signs `/community/register` every fourth 15 s tick and `/community/heartbeat` otherwise, sent to all six origins concurrently. Registry TTL is 90 s. | `main.rs:8849`, `main.rs:8898`; `rpc.rs:776`, `rpc.rs:13105`, `rpc.rs:13236` |
| Authentication | Every mutation is an Ed25519 envelope bound to path, coordinator, recovery domain, timestamp (±120 s) and nonce, with replay caches. | `rpc.rs:12855` |
| Demand | Only a caller's `POST /inference/run`. The smart router dispatches to a community worker when one is live, otherwise it runs locally. | `rpc.rs:7232`, `rpc.rs:7419` |
| Dispatch | A job id is derived from validator, boot epoch, nonce, model, input hash and max tokens. A pending record is inserted, the item is `try_send` onto a per-coordinator FIFO (capacity 256), and the dispatcher waits on a oneshot. The budget is 3 × 3.3 s per position × 1.5, plus a 30 s claim window. | `rpc.rs:6558`, `rpc.rs:6761`, `rpc.rs:6937`, `rpc.rs:6950`; `rpc.rs:1894`, `rpc.rs:1990`; `rpc.rs:6335` |
| Claim (first come) | Each worker long-polls `/community/claim_work` on all six coordinators at once (30 s). The coordinator's poll loop `try_recv`s every 25 ms, so the first idle poller wins. The job is bound to that worker. A worker that wins two coordinators at once declines the extra job. | `main.rs:8912`, `main.rs:9001`, `main.rs:9050`; `rpc.rs:13321`, `rpc.rs:13327`, `rpc.rs:15553`, `rpc.rs:15730` |
| Compute and submit | The worker runs the whole model (`try_generate`), signs an `InferenceAttestation` certificate over model, input and output hashes, and POSTs `/community/submit_work` with bounded retries. | `main.rs:9337`, `main.rs:9466`, `main.rs:9516`, `main.rs:5600` |
| Coordinator verification | Every successful submit is recomputed through the validators' shard pipeline. There are 6 layer ranges, each replicated on 3 validators. Every position runs on all 3 replicas, with an authenticated 2-of-3 quorum per range and position. The output is compared on token hash, token count and text. | `rpc.rs:16068`, `rpc.rs:13929`, `rpc.rs:14041`, `rpc.rs:13808`, `rpc.rs:13756` |
| Reward | If issuance capacity remains (1 per block, 40 per epoch of 216,000 blocks, 16 per coordinator, 8 per worker), the coordinator signs its approval and asks all five other validators. Each of them recomputes the job before signing. 5 of 6 approvals make a 0x25 transaction of 2.5 testnet ARC, retried from a crash-durable journal. | `rpc.rs:15407`, `rpc.rs:15419`, `rpc.rs:14594`, `rpc.rs:14617`, `rpc.rs:14402`, `rpc.rs:14308`, `rpc.rs:14323`, `rpc.rs:15320`; `arc-state/src/lib.rs:58-62`; `arc-types/src/transaction.rs:889-891` |
| Receipts and views | Reward receipt and job status, the activity feed, the in-memory scoreboard, and the explorer's receipt and activity panels. | `rpc.rs:16500`, `rpc.rs:16559`, `rpc.rs:16613`, `rpc.rs:8357`, `rpc.rs:1775`, `rpc.rs:8184`; `explorer/app.js:336`, `explorer/app.js:865` |
| Validator limits | 2 concurrent public inferences per coordinator, 3 concurrent shard computations with a queue of 12 per validator, 1 approval recomputation with a queue of 6, and 32 shard KV caches. | `rpc.rs:173-188` |

Consequences:

- **Assignment is not chosen.** It goes to whichever poller wins.
- **Every community job costs at least one full validator recomputation**, and six when rewarded.
- **The validators are the only checkers.** Verified throughput is bounded by validator recomputations, not by community capacity.

## 3. Twin execution v0

### 3.1 Dispatch

With `--community-twin-execution`, `/inference/run` uses twin dispatch whenever at least two eligible workers are live. Recovery probes never use it.

- **Two legs, same work.** The coordinator creates two legs with the same input, model, profile and token budget, and different job ids (consecutive nonces). Each leg is an ordinary pending assignment, so v0.8.10 workers need no change.
- **The group.** A group record links the legs. The group id is leg 0's job id. Twin groups never touch the reward budget unless they come from caller demand.
- **Collection deadline.** Half the reviewed dispatch budget, because the legs run in parallel. The other half covers any validator recomputation and reward approvals.

### 3.2 Pairing at claim time

When a worker dequeues a twin leg, `twin::decide_claim` runs atomically with recording the claim:

1. **Hard rule.** The claimer must be independent of every related worker: the sibling leg's worker, or for a replay the workers that produced the reference. In v0 that means a distinct node key. The rule also enforces distinct operator and distinct network group whenever both sides are known; neither is collected yet (section 11). A refused leg is put back on the queue.
2. **Diversity preference.** For up to 3 s after enqueue, a leg waits for an idle worker that differs from the related workers in more of {region, platform}.
3. **Fairness.** For generated demand only, the leg also waits for an idle worker that has gone unserved at least 60 s longer.

After the 3 s window, any independent worker may take the leg. Without region tags the diversity rule still prefers a different platform, for example an Apple Silicon Mac paired with a Linux PC, which is the cross-architecture case ARC's determinism claim is about.

### 3.3 Comparison

- **v0 compares at submit time.** It compares each leg's output commitment: BLAKE3 of the token ids, the token count and the decoded text. Any difference is a mismatch, and the fields that differ are recorded.
- **v0.1 compares while generating.** Workers stream the rolling checkpoint chain defined in `twin::checkpoint_chain`. With `h_0` = job commitment, each step is `h_{k+1} = BLAKE3-derive-key("ARC community twin rolling checkpoint v1", h_k ‖ chunk_len_le64 ‖ tokens_le32)` over 32-token chunks. The coordinator can then detect a divergence while both twins are running and locate the first divergent chunk.
- **What v0.1 needs.** An engine observer hook (section 11) and a worker update.

### 3.4 Resolution

Resolution runs in a node-owned task when every leg is terminal, when the sibling stays unclaimed for 30 s after the first leg finished, or at the collection deadline. A dropped HTTP connection cannot cancel it.

| Comparison | Validators recompute? | Verdict |
|---|---|---|
| Match, caller demand, reward capacity remains | Yes, reason `reward`. A validator approval asserts that validator's own recomputation, so a twin match cannot replace it. | Verified; reward requested as today |
| Match, no reward | Only for a keyed spot check: default 5%, at most 6 per hour per coordinator, and never while this validator is already recomputing. | Verified by `twin_match`, plus `validator_recompute` when sampled |
| Mismatch | Yes, once. The single recomputation classifies both legs. | Verified for the leg that matches the validators; the other leg is rejected |
| Match contradicted by validators | (spot check or reward) | Rejected: both legs agreed on a wrong output (collusion or a shared bug) |
| One leg missing, caller demand | Yes, reason `fallback`: today's single-worker verification | Verified or rejected |
| One leg missing, generated demand | No | Unverified; no validator capacity spent |
| Replay (one leg and a verified reference) | Only on a mismatch | Verified by `reference_hash`; a reference the validators contradict is evicted |
| Validators unavailable or busy | — | Unverified. No output is returned and nobody is penalized |

Spot-check selection is `BLAKE3-keyed(coordinator secret, group_id) mod 1000 < per_mille`. The secret is random per process, so workers cannot predict which matched groups get checked.

### 3.5 Settlement, accounting and penalties

- **Rewards keep today's protocol path.** They draw only on caller demand (`public_request`, `public_demo`) and go to the verified leg, only after the coordinator's own validator recomputation. Generated demand never draws on the promotional budget.
- **Worker health counters work exactly as for single jobs.** A verified leg is a success. A leg validators proved wrong, or that the worker reported as failed, is a failure. Unverifiable work changes nothing.
- **No slashing.** Community stake is 0 on testnet; a mismatch is flagged in the receipt and counters.

### 3.6 Failure handling

| Failure | Handling |
|---|---|
| No second independent worker online | Twin dispatch is skipped and the job runs as today, with single worker and validator verification |
| Queue full when enqueueing | First leg: safe local fallback, as today. Second leg: the caller gets a timeout and the orphaned leg settles as an ordinary job |
| Sibling never claimed | Resolve 30 s after the first leg finishes. The unclaimed leg's pending record is removed, so a late dequeue expires it |
| Sibling claimed but late | Marked `abandoned`. A later submit is recognized (up to 1,024 resolved legs are remembered) and recorded as `late_after_resolution`, never re-verified or settled as an ordinary job |
| Worker declines (it won a concurrent job on another coordinator and never started) | The leg goes back on the queue for another independent worker while collection time remains (`legs_requeued`); after that it is terminal |
| Worker reports a failure | The leg is terminal and counts as that worker's failure, as today; the group resolves on what it has |
| Coordinator restarts | Open groups and receipts are in memory and are lost, like the scoreboard. Rewards in flight keep today's crash-durable settlement journal |
| Validator recomputation unavailable | Unverified; no output, no penalty (same rule as today's `Unavailable`) |
| Shutdown | Watchdogs, resolutions and the pump stop on the lifecycle signal |

### 3.7 What twin execution catches, and what it does not

- **One honest twin is enough.** A wrong answer is caught whenever the other twin is honest, because the engine is deterministic.
- **Colluding twins.** Two colluding workers can agree on a wrong answer. Suppose an attacker controls a share `f` of idle workers. The chance both legs land on the attacker is about `f²`, and each such group is spot-checked with probability `s`. At `f` = 0.1 and `s` = 5%, about 1% of groups are attacker pairs, and an always-cheating pair is caught after about 20 of them.
- **Sybils.** v0 cannot tell one person with two node keys from two people (section 8).

## 4. Receipt schema: `arc.community.twin-receipt.v1`

Every resolved group stores a receipt. Each coordinator keeps the newest 512, addressable by group id or either leg's job id. Receipts are returned inline by `/inference/run` for twin-executed requests and served by `GET /community/twin/{job_id}` and `GET /community/twin_receipts?limit=N`.

```json
{
  "schema": "arc.community.twin-receipt.v1",
  "group_id": "<leg 0 job id, 64 hex>",
  "coordinator": "0x<validator address>",
  "source": "public_request | public_demo | pump_demo | pump_replay",
  "public_prompt": "Name three primary colors. (fixed public prompts only; caller prompts are never published)",
  "model_id": "0x<model artifact id>",
  "execution_profile": "INT8 integer (per-row, cross-platform deterministic)",
  "input_hash": "0x<BLAKE3 of the exact input the workers ran>",
  "max_tokens": 32,
  "created_at_unix_ms": 0,
  "resolved_at_unix_ms": 0,
  "legs": [
    {
      "leg": 0,
      "job_id": "<64 hex>",
      "worker_id": "0x<worker node address> | null",
      "region": { "region": "eu-west", "continent": "europe", "class": "region", "basis": "worker_reported_rtt_to_validators", "measured_at_unix_ms": 0 },
      "platform": "macos-aarch64",
      "status": "unclaimed | claimed | submitted | failed | declined | abandoned",
      "output_hash": "0x<BLAKE3 of token ids> | null",
      "tokens_generated": 32,
      "ms_per_token": 716,
      "worker_attestation_hash": "0x<worker-signed certificate hash> | null",
      "valid": true
    }
  ],
  "comparison": {
    "basis": "final_output_hash",
    "checkpoint_interval_tokens": 32,
    "result": "match | mismatch | incomplete | reference_match | reference_mismatch",
    "mismatch_fields": ["output_hash", "tokens_generated", "output_text"],
    "reference": { "group_id": "...", "output_hash": "0x...", "valid": true }
  },
  "independence": {
    "distinct_worker_keys": true,
    "distinct_operators": "unknown",
    "distinct_network_groups": "unknown",
    "different_regions": true,
    "different_platforms": true
  },
  "validator_recompute": {
    "reason": "spot_check | mismatch | fallback | reward | null",
    "status": "not_selected | confirmed | contradicted | unavailable | skipped_busy",
    "method": "authenticated_shard_quorum_2_of_3_per_range | null",
    "output_hash": "0x... | null",
    "duration_ms": 0
  },
  "verdict": "verified | rejected | unverified",
  "verified_by": ["twin_match", "validator_recompute", "reference_hash"],
  "settlement": { "...": "today's 0x25 settlement object, caller demand only" },
  "disclosure": "The coordinator ... and the validators ... are operated by the ARC team ...",
  "commitment": "0x<receipt commitment>",
  "coordinator_signature": { "scheme": "ed25519", "signer": "0x...", "public_key": "0x...", "signature": "0x..." }
}
```

- **`valid`** is `true` or `false` once twin agreement or validators establish it, and `null` otherwise.
- **`region`** is a coarse label only. The precise round trip is never published.
- **`public_prompt`** carries the fixed public demo prompt verbatim. Caller prompts appear only as `input_hash`.

**Commitment and signature.** `commitment = BLAKE3-derive-key("ARC community twin receipt v1", transcript)`. The transcript is:

1. the byte `0x01`;
2. `group_id`, `coordinator`, `source`, `model_id` and `input_hash` as length-prefixed strings;
3. `max_tokens` as a u64;
4. the leg count as a u64;
5. per leg: `job_id`, `worker_id`, `status`, `output_hash`, `tokens_generated` (as a u64, `u64::MAX` when absent) and `valid` (`"true"`, `"false"` or `"null"`);
6. `comparison.result`, `validator_recompute.status` and `verdict`;
7. `resolved_at_unix_ms` as a u64.

Strings are length-prefixed with a u64 in big-endian order and encoded as UTF-8; absent values are empty strings; all integers are big-endian.

The coordinator signs the 32-byte commitment with its validator Ed25519 key. To check a receipt:

1. Rebuild the transcript from the JSON.
2. Check that `public_key` hashes to `signer` (ARC address derivation).
3. Verify the signature.

The commitment binds who computed what, the comparison and the verdict. It does not bind presentation fields such as `ms_per_token` or the disclosure text. Reference: `TwinReceipt::compute_commitment`.

## 5. Region tags v0

- **Measurement.** A worker started with `--community-region-probe` (opt-in) does this every ten minutes for each configured community origin:
  - reads `/network/info` (this also opens the connection);
  - times three `GET /health` calls and keeps the best;
  - POSTs a signed `{worker_id, samples: [{validator, rtt_ms}]}` to `POST /community/region` on every origin. Coordinators without the endpoint answer 404, which is ignored.
- **Classification (research-7 §1.7).** The nearest validator by round trip names the region:

  | Round trip to nearest validator | Class | Twins compare on |
  |---|---|---|
  | ≤ 60 ms (L2 region diameter) | `region` | region |
  | ≤ 150 ms (L1 continent diameter) | `continent` | continent |
  | > 150 ms | `distant` | continent |

  The six genesis validators carry display labels: us-east, us-west, eu-west (two), asia-east and asia-southeast. Any other validator is labelled by its address prefix.
- **Privacy.**
  - No IP address is collected or exposed. The client IP never reaches the node anyway, because the production nginx filter strips it.
  - Only the coarse label is published. The raw round trip stays inside the coordinator.
  - Tags exist only for workers that opted in.
- **Limits.** Tags are self-reported, so they steer pairing preference and coarse display only, never pay or penalties. A worker can make itself look farther away but not closer to a validator it actually measured. Without verification by third-party probes (research-7 §6.4), treat tags as hints.

## 6. Demand

### 6.1 Demand pump (`--community-demand-pump`, off by default)

- **Ticks.** Each tick (default 120 s ±20%, minimum 30 s) looks at the idle eligible workers currently long-polling this coordinator.
- **What it dispatches.** One job per tick:
  - a twin demo job: one of 32 fixed, harmless public prompts, in rotation, through the model's chat template, with 32 output tokens; or
  - a replay (section 6.2): 25% of ticks, or whenever only one idle worker is available.
- **It never competes with callers.** It starts only while every public inference slot is free and holds at most one of them.
- **Hard limits.** At most one pump job in flight per coordinator and at most 30 groups per hour per coordinator. It never draws on the reward budget.
- **Fairness.** The least-recently-served preference (section 3.2).
- **`--community-demand-dry-run`.** Plans and logs every tick ("would dispatch …") and counts `demand_dry_run_ticks`, without dispatching. Run it before enabling the pump, per the automation dry-run rule.

### 6.2 Verification workload (replays)

Every verified pump demo answer becomes a replay reference: the exact input, the canonical output commitment and the workers that produced it.

- **What a replay is.** A one-leg job that re-executes a past verified prompt on a different worker.
- **How it is checked.** Against the known hash, with no validator cost unless it mismatches.
- **What it is for.** It is the known-answer canary research-9 asks for (E4). It gives single idle workers verified work.
- **Contradicted references.** A reference the validators contradict is evicted and counted.
- **Privacy.** Caller prompts never become references, so no private text is redistributed.

### 6.3 Public demo requests

`POST /inference/run` with `"public_demo": true` (existing allowlisted path, so no gateway change):

- **Twin-only.** Always twin-executed. It answers 503 when fewer than two eligible workers are online, and refuses `force_local` and recovery probes.
- **Screened.** Empty or oversized prompts (over 1,000 bytes), email addresses, numbers of 9 or more digits and key-like tokens are rejected with 400. This screen is best effort.
- **Rate-limited.** 60 per hour per coordinator, burst 6; over the limit returns 429. Capped at 64 output tokens. Run through the chat template.
- **Labelled.** The response carries `public_demo: {public: true, testnet: true, notice}`. The notice says the prompt goes to community computers that can read it and must not contain personal data.

### 6.4 Visibility

- **`/workers/scoreboard`** (already public) gains a `twin` block and a per-worker `twin` row (region, verified, rejected and unverified legs, last served) whenever either switch is on. With both off the output is byte-identical to `main`. The block carries:
  - groups matched and mismatched;
  - the twin-match rate;
  - verified jobs and tokens over the last hour;
  - verified tokens per second.
- **`GET /community/twin_stats`** (`arc.community.twin-stats.v1`) carries:
  - configuration and caps;
  - every counter, including pairing refusals and deferrals and cross-region and cross-platform pairs;
  - recomputations performed, avoided, skipped and unavailable, with their mean duration;
  - throughput over the last hour;
  - counts of region-tagged workers;
  - the disclosure.
- **`GET /community/twin/{job_id}`** and **`GET /community/twin_receipts`** serve the receipts in section 4.

## 7. Validator load estimate

Inputs (main @ 4070114c6 unless stated):

| Input | Value | Basis |
|---|---|---|
| Validator RAM | 8 GB | sprint brief; VPS specs otherwise unconfirmed |
| Layers per validator | 15–17 of 32 (about 2.9–3.3 GB of weights); 3 replicas per range | `scripts/recovery/recovery_rollout.py` constants; archived `CLAUDE.md` topology |
| Work per recomputation position | every range on all 3 replicas, so each validator computes about half the model (3 of 6 ranges) | `rpc.rs:14041` (fanout 3, fixed quorum since v0.8.9) |
| Hop latency per range | 180–410 ms, measured on the live fleet | `rpc.rs:850` |
| Positions per job (P) | 1 + prompt + 1 + output; pump job about 50–60; caller maximum 258 + prompt | `rpc.rs:13720` |
| KV cache | about 2 MiB per position for the full model, so about 1 MiB per position per validator | research-9 [CALC] |

Derived per recomputation [CALC]:

- **Wall time** is about P × 6 hops × 0.18–0.41 s, so 1–2.3 min for a pump job and 5–11 min at the maximum.
- **CPU per validator** is at most about P × 0.55 s, so 30 s or less for a pump job.
- **KV per validator** is about P MiB, so about 55 MiB for a pump job and 258 MiB at the maximum.

Today every community job costs at least 1 recomputation, and 6 when rewarded. For example, 60 jobs an hour would need 60 recomputations an hour: about 30 CPU-minutes per validator per hour (about half a core) and 1–2.3 recomputations in flight continuously.

Twin v0 at the recommended settings: twin on, pump on two coordinators at 120 s, 25% replays, 5% spot checks.

| Source of validator work | Rate | Recomputations per hour |
|---|---|---|
| Generated demand | at most 60 groups per hour (45 twin, 15 replay) | spot checks 5% × 45 ≈ 2.3 (hard cap 12) |
| Mismatches | 1 each; about 0 if cross-platform determinism holds | bounded by 1 in flight per coordinator |
| Rewarded caller jobs | protocol cap of 40 per 13.8 h network-wide, about 2.9 per hour | at most 17.4 (unchanged from today) |
| Unrewarded caller jobs | as demanded | 5% spot checks instead of 100% |

Result:

- **CPU.** The pump at these settings adds about 2.3 × 30 s ≈ 70 s of CPU per validator per hour (about 2% of a core) plus mismatches. The same demand on today's design would cost about 25 times more validator recomputation.
- **Memory.** Twin code holds at most one recomputation per coordinator (six network-wide), which bounds added KV at about 0.33 GB per validator for pump-sized jobs and 1.5 GB in a worst-case storm of maximum-length caller mismatches.
- **Spot checks never queue.** They are skipped while the local validator is recomputing an approval.
- **Pre-existing ceiling, not introduced here.** The 32-cache KV cap times 258 MiB is about 8.3 GB in the worst case.

Measure, do not assume: `GET /community/twin_stats` reports recomputations performed and their mean duration, which is the E5 measurement research-9 asks for.

## 8. What is still centralized

- **The coordinator** is a team-run validator. It creates the jobs, chooses the twin pairs (within the rules above), compares outputs, signs receipts and holds the spot-check secret.
- **The validators.** Six, team-run, on one hosting provider. They recompute samples, mismatches and fallbacks and approve rewards. Settlement needs 5 of 6.
- **The gateway and router** are team-run.
- **Independence is key-level only.** One person can run two nodes. There is no operator registry and no network-group check (the client IP never reaches the node). Receipts state `distinct_operators: "unknown"` and `distinct_network_groups: "unknown"`.
- **Region tags are self-reported.**
- **Receipts and statistics are in memory** and reset when a coordinator restarts.

## 9. Rollout plan

### 9.1 What ships where

| Change | Ships via | Needed for |
|---|---|---|
| Twin dispatch, pairing, comparison, resolution, receipts, stats, pump, public demo, `/community/region` endpoint | **Validator release** (new `arc-node` on all six) plus operator flags in the validator units | everything coordinator-side |
| Gateway allowlist: POST `/community/region`; GET `/community/twin_stats`, `/community/twin_receipts`, `/community/twin/{job_id}` | **Validator release** (`recovery_rollout.py` sealed lists and manifest schema; not changed in this PR) | public access to the new endpoints. `/workers/scoreboard` and `/inference/run` work without it |
| Worker region probe (`--community-region-probe`) | **Desktop updater** (new `arc-node` binary), with a desktop consent toggle (EX2) | region tags |
| Twin legs on workers | **Nothing.** v0.8.10 workers already run twin legs as ordinary assignments | — |
| Consensus, genesis, chain state | **Nothing** | — |

### 9.2 Steps, each gated on the previous

1. Merge after review. Ship a validator release with all switches off and the gateway allowlist extended. Check that `/workers/scoreboard` is byte-identical with the switches off.
2. Enable `--community-twin-execution` on one coordinator. Watch receipts and `twin_stats` for 7 days: twin-match rate, mismatches by platform pair, recomputation durations, memory.
3. Enable `--community-demand-pump --community-demand-dry-run` on that coordinator for 24 h. Review the logged plan.
4. Enable the pump for real on one coordinator, then two.
5. Enable twin execution on all six coordinators.
6. Ship the desktop release with the region-probe consent toggle (EX2).
7. Only after 7 clean days: turn on public demo traffic from the demo page.

### 9.3 Stop rules

Pause the pump, and twin execution if needed, when any of these happens:

- a twin match is contradicted by validators;
- the mismatch rate rises above 1% for any platform pair;
- a validator's memory headroom drops below 1 GB;
- spot checks are skipped as busy for more than 25% of selections.

## 10. What can truthfully be said once it ships

Say this only after steps 1–2 above, with numbers read from `twin_stats` and receipts on the day.

| Claim | Status |
|---|---|
| "Each community answer is computed twice, by two different community computers, and the two results are compared byte for byte. The receipt shows both computers and both output hashes." | True on coordinators with twin execution enabled, for groups with a `match` comparison |
| "Validators re-run only a random sample and any disagreement, instead of every answer." | True |
| "X% of answers matched across twins in the last hour" and "N matches were between different chips, such as Apple Silicon and x86 Linux" | True when read from `twin_stats` and receipts |
| "Idle nodes now get real work: public demo prompts and re-checks of past answers." | True when the pump is enabled; labelled public and testnet |
| "Checked by independent operators" | **Not yet.** Say "checked by a second community node with a different key". Operator identity and network independence are unverified |
| "Truly decentralized" | **Not yet.** Coordinators and validators are team-run on one hosting provider |
| "Cheaters lose their stake" | **No.** Stake is 0 and there is no slashing. A wrong answer is caught, earns nothing and counts against the worker |

## 11. Hooks and follow-ups

- **EX2 (node and desktop).**
  - Wire a consent toggle to `--community-region-probe`.
  - If an operator or family declaration is added at registration, feed it to `twin::WorkerFacts::operator`. The hard rule already enforces distinct operators.
  - Receipts use worker ids, never display names.
- **EX3 (engine).** Add an observer variant of `try_generate` that yields each sampled token with identical arithmetic. Workers can then stream `twin::checkpoint_chain` every 32 tokens (v0.1, needs a worker update and a new signed checkpoint endpoint).
- **Gateway.**
  - Allowlist the four paths in section 9.1.
  - Optionally forward a salted /24 (IPv4) or /48 (IPv6) network group to the node, to enable `WorkerFacts::network_group` (different ISP).
- **Follow-ups.**
  - Persist receipts and counters (research-9 E9).
  - Add verification of region claims by third-party probes (research-7 §6.4).
  - Select pairs with a public beacon instead of a coordinator choice (research-7 §3.6).
  - Add a third community execution as the tiebreaker once validators should no longer be the arbiter.

## 12. Operator reference

| Flag | Default | Role |
|---|---|---|
| `--community-twin-execution` | off | coordinator: twin dispatch |
| `--community-twin-spot-check-per-mille N` | 50 | coordinator: share of matched groups recomputed; hard cap 6 per hour |
| `--community-demand-pump` | off | coordinator: generated demand. Requires `--community-twin-execution`: replays only reuse answers that twins verified |
| `--community-demand-dry-run` | off | coordinator: plan and log only |
| `--community-demand-interval-secs N` | 120 | coordinator: tick (minimum 30) |
| `--community-region-probe` | off | worker: opt-in region report |
| `--community-release-on-verification` | off | coordinator: answer the caller once 2-of-3 verification passes; reward approvals follow on the settlement retry path (section 13) |

| Endpoint | Kind | Gateway |
|---|---|---|
| `POST /inference/run` with `"public_demo": true` | public demo | already allowlisted |
| `GET /workers/scoreboard` (`twin` block) | stats | already allowlisted |
| `POST /community/region` | signed worker report | needs allowlisting |
| `GET /community/twin_stats` | stats | needs allowlisting |
| `GET /community/twin_receipts?limit=N` | receipts | needs allowlisting |
| `GET /community/twin/{job_id}` | receipt | needs allowlisting |

## 13. Release on verification (`--community-release-on-verification`, off by default)

A coordinator switch for single-worker community jobs. It changes no consensus, genesis or on-chain rule.

**What changes.** By default the waiting `/inference/run` caller is answered only after three steps: the worker's run, the validators' authenticated 2-of-3 recomputation, and the reward approvals (five of six validators, each recomputing the job, before the 0x25 transaction enters the mempool). With the switch on, the caller is answered as soon as the 2-of-3 recomputation has passed and the settlement is written to the crash-durable journal. The approvals then run on the existing settlement retry loop (`schedule_verified_settlement_retry`). That is the loop that already retries a failed first attempt and replays the journal after a restart. Its first attempt starts one second after the answer.

**What does not change.**
- No answer is released before its 2-of-3 recomputation has matched the worker's output.
- Settlement is the same code: the same five-of-six approvals, issuance budget, 0x25 transaction, backoff and expiry.
- A settlement that fails after the release is recorded (`last_error`) and retried as before. It never changes or withdraws the answer already sent.
- Each job has one journal entry and one retry task. A repeated worker submit is an idempotent replay, and the chain's job marker still refuses a second payment.
- Twin execution and sealed recovery probes keep the default ordering. Recovery probes are rollout tooling that reads the settlement evidence from the same response.

**Latency effect.**
- MEASURED, single sample (8 Oct 2026, one free 16-token desktop prompt on v0.8.11; timings from validator hop-sample timestamps and the mined receipt): 313 s from the click to the mined reward. That was worker compute 112 s, the validators' 2-of-3 recomputation about 60 s, and approvals by five of six validators about 135 s, all before the answer was released.
- With the switch on, the about 135 s of approvals leave the request path: about 3 minutes to the answer for that prompt. This is INFERRED from the same sample, not measured with the switch on.
- The margin to the coordinator's dispatch deadline grows by the same amount (INFERRED). The worker and the public inference permit are also freed at the release instead of after the approvals.

**Response contract.** A released answer is the normal success response with two differences:
- `settlement` is the existing `verified_pending_approval` object (`submitted: false`, `tx_hash: null`, `retry_running: true`).
- A new `answer_release` object is added:

```json
"answer_release": {
  "schema": "arc.community.answer-release.v1",
  "mode": "on_verification",
  "verification": "passed",
  "settlement": "pending",
  "settlement_status_url": "/community/reward_job/0x<job_id>"
}
```

`settlement_status_url` is a public gateway route. It reports `verified_pending_approval`, then `pending_mined_receipt`, then the mined receipt. The worker's `/community/submit_work` response carries the same two fields. With the switch off, `answer_release` never appears and both responses are unchanged. The desktop already treats `verified_pending_approval` as not yet submitted: it shows the answer and pins no reward-receipt route for it.

**Risk model.** The caller's answer has passed the same validator check as before, so only the timing of the answer and of settlement changes, not what is verified or paid. What remains:
- A caller can hold an answer whose reward later fails to settle, for example when the issuance budget runs out or approvers stay unreachable until the reward expires. The worker would go unpaid in that case today as well; the difference is that the caller already has the answer.
- More verified jobs can wait for approvals at once, because workers and permits are freed earlier. The approvers' bounded queues and the retry backoff handle this; a busy approver's HTTP 429 is retried.
- Turning it off is a restart without the flag. Settlements already journaled keep retrying after the restart.
