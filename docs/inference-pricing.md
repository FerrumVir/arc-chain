# Capacity-aware inference pricing v0

Status: draft design and a coordinator-side implementation. **Off by default** (`--enable-inference-pricing`).
v0 quotes are advisory. Paid inference stays disabled at every public ingress, so no ARC is held, charged
or refunded, and there are no consensus, genesis or on-chain rule changes.

- Code: `crates/arc-node/src/inference_pricing.rs`
- Simulation: `scripts/inference_pricing_sim.py` (standard library only; runs in well under a second)

`rpc.rs` means `crates/arc-node/src/rpc.rs`. Line numbers refer to `main` at 4070114c6.

## 1. Summary

**Goal.** Price an AI token in ARC the way gas prices block space. The price per token should fall as
verified network capacity grows and rise only when that capacity is congested.

**How v0 does it.**

- One **base fee per model**, in ARC base units per 1,000,000 billable tokens.
- Once per **epoch** (default 10 minutes) the fee moves toward a **target utilization** of verified
  community capacity (default 50%). Idle verified capacity lowers it, congestion raises it, and one epoch
  moves it by at most 12.5%.
- It never falls below a **floor** that pays every worker the job relies on at least the operator's
  **worker floor**, which is meant to cover electricity. It never rises above a ceiling.
- Before running, a caller can ask for **"≈ X ARC for this answer (max Y ARC)"**. A paid job would hold Y
  in escrow and refund whatever the answer did not use.

**What the numbers say** (section 5):

- **A congestion price removes congestion premiums. It cannot make tokens cheaper than they cost.**
  Lasting price drops come from a lower cost per token: faster kernels, batching, cheaper verification,
  and smaller models of equal quality.
- **Today ARC costs far more per token than a centralized API.** Today's verified worker runs the INT8
  CPU engine at a measured 1.4 tokens/s, and twin execution (PR #139) runs every job twice. That puts the
  break-even floor at about **$3.50 per 1M tokens** in electricity (USD-equivalent). The cheapest
  Llama-3.1-8B API costs about **$0.03**, so ARC is roughly **117 times** more expensive.
- **If every lever works, the floor could fall to about $0.005 to $0.012 per 1M tokens**, below today's
  API prices. None of those levers is proven yet. Even then the floor covers electricity only, not
  hardware, bandwidth or anyone's margin.
- **Kimi-class models** (projection, section 5.5). $0.60 or less per 1M output tokens, with island owners
  and verifiers paid, is reachable in two cases:
  - on batched islands whose owners already have the hardware, at $0.27-0.47;
  - on dense GPU rigs that pay their hardware back, at 58% utilization or more over 5 years.

  That is 76-94% below the $2.45-4.60 API range, but at only 7-24 tok/s per user.
- **Testnet ARC has no monetary value.** Nothing here promises income to anyone.

## 2. What exists today

| Piece | Where | Wired in? |
|---|---|---|
| EIP-1559 "inference gas lane" `InferenceGasLane`: a per-block base fee for inference *transactions*. Target 5 and maximum 10 transactions per block, ×9/8 or ×8/9 per block, a minimum fee, and a per-address rate limit. | `crates/arc-inference/src/gas.rs:1-140` | **No.** Nothing outside `gas.rs` calls it. It counts transactions, not tokens or capacity. Its default base fee is 100,000 base units, which is 0.0001 ARC. |
| EIP-1559 `FeeConfig`: `adjust_base_fee`, and `adjust_for_tps` with a `smoothed_tps` moving average | `crates/arc-types/src/economics.rs:392-621` | **No.** Nothing outside `economics.rs` calls it. |
| `RoleRevenueConfig` 40/25/15/20 split (proposer, verifiers, observer pool, treasury) | `economics.rs:72-122` | **Display only.** `/economics/revenue_split` (`rpc.rs:17093`) and a nominal 1,000-unit example in the sharded-run response (`rpc.rs:11915`). |
| Flat community reward, `INFERENCE_ATTESTATION_REWARD` = 2.5 ARC | `economics.rs:20-37`; caps in `crates/arc-state/src/lib.rs:58-62` | **Yes.** This is the only live payment. It is a promotional subsidy capped at 1 per block, 40 per 216,000-block epoch, 16 per coordinator and 8 per worker. `/community/reward_policy` reports it as a "protocol-capped testnet promotional compute subsidy" with `reward_is_customer_demand: false` (`rpc.rs:16613-16673`). |
| Milestone B escrow (`InferenceEscrowOpen`, `Release`, `Refund`; `max_fee`) | `crates/arc-types/src/transaction.rs:302-313, 1214-1283`; state apply `arc-state/src/lib.rs:7537-7634` | **Disabled at ingress** (`rpc.rs:3119-3141`). A release pays out the *whole* `max_fee` 40/25/15/20 (`arc-state/src/lib.rs:7584-7591`). There is no per-token charge and no refund of unused budget. |
| Tier 1 `InferenceRequest` (`max_reward`, 70/20/10) | `transaction.rs:1295-1327, 1436-1443` | **Disabled at ingress** (`rpc.rs:3119-3141`). |
| Protocol-4 native job: a signed `execution_price` no greater than the escrowed `reserved_max_payment`, with the difference refunded | `crates/arc-types/src/inference_contract.rs:36-52, 298-306, 423-486` | **Inactive candidate** (module doc, lines 1-6). Its shape is quote-then-run, but the price is flat per job (not per token) and the payees are validators, pro rata by stake. |
| Legacy paid fields on `/inference/run` | `rpc.rs:12114-12132` | Rejected on sight. |
| A quote or pricing endpoint | router, `rpc.rs:2173-2380` | **None.** |
| Capacity signals: worker registry (90 s TTL), quorum-verified successes, claim reservations, work queue | `rpc.rs:555, 608, 730-777, 6366-6387, 13482-13503, 16067-16126, 16216-16224` | Used for dispatch and the scoreboard, not for pricing. |

**Reading.** The EIP-1559 inference gas lane listed as built exists as library code with unit tests, but
nothing calls it, so "the fee lane isn't wired in" is correct. It also prices the wrong thing for this
goal: block slots for inference transactions, not tokens or verified capacity. This PR leaves `gas.rs`
and `FeeConfig` untouched. Section 8 proposes reconciling or retiring them.

## 3. Design

### 3.1 Units and billable tokens

- Amounts are in ARC base units: 1 ARC = 10^9 base units (`economics.rs:13-18`). Prices are in base
  units per 1,000,000 billable tokens.
- **Billable tokens** = ceil(prompt tokens × input weight) + output tokens, with a minimum of 64 per job.
- The input weight defaults to 1.0. Today's engine runs every prompt position as a full forward pass,
  which `rpc.rs:6323-6334` budgets like a generated token. Lower the weight once batched prefill
  (`crates/arc-inference/src/canonical_prefill.rs`, opt-in today) is in production.

### 3.2 Verified capacity and utilization

Every 15 seconds the coordinator takes a read-only snapshot of its community state.

| Symbol | Meaning | Source |
|---|---|---|
| V | **Verified workers**: heartbeat within the 90 s TTL, the canonical INT8 profile, this coordinator's model, and at least one quorum-verified job. `success_count` only increases after the validators' recomputation agrees (`rpc.rs:16067-16126`, then `16216-16224`). | `community_workers` |
| I | Verified workers **long-polling this coordinator** right now (an empty reservation) | `community_active_jobs` (`rpc.rs:608`, `13482-13503`) |
| Q | Jobs **waiting in this coordinator's queue** (FIFO, capacity 256) | `community_work_tx` (`rpc.rs:1894`) |

Over an epoch, **utilization = Σ(V − I + Q) / ΣV**, capped at 200%. Workers without a verified job are
reported but never count as capacity.

**Why "not long-polling here" counts as busy.** Each idle worker long-polls all six coordinators at
once. Once it wins a job from any of them, it opens no new polls until the job is done
(`main.rs:8985-9060`). So V − I counts it as busy wherever its job came from, and every coordinator sees
nearly the same network-wide utilization without any new messages between them.

The proxy has known errors in both directions:

- **Too low.** After winning a job, the worker leaves its other polls open, for up to about 30 s (the
  claim timeout), and declines anything they return. Until each poll ends, the worker still reads as
  idle at that coordinator, so jobs shorter than about 30 s on other coordinators are undercounted.
- **Too high.** A worker reads as busy:
  - for a few milliseconds between two polls;
  - for up to 90 s after it goes offline;
  - whenever its polls to this coordinator fail.

A busy flag on heartbeats (section 9, #138) would remove both errors.

**No verified capacity, no information.** If an epoch saw no verified worker at all, its utilization is
undefined, and the fee holds. A coordinator with no capacity does not ramp its price toward the ceiling.

### 3.3 The epoch update

With fee `f`, target `T` and observed utilization `u` (capped at 2T), and step limit `s` (default 1,250
basis points):

```text
u > T:   f' = f + max(1, floor(f × s × (u − T) / (T × 10,000)))
u < T:   f' = f −        floor(f × s × (T − u) / (T × 10,000))
u = T, or no verified capacity:   f' = f
then     f' = clamp(f', floor, ceiling)
```

- The rule is integer arithmetic throughout (`next_base_fee`). The Python simulation implements the same
  rule, and the unit test `base_fee_matches_python_parity_vector` keeps the two in step.
- Epochs are aligned to Unix time. Coordinators with the same configuration therefore share boundaries.
- The fee starts at the floor, the resting price, and rises only under congestion.

The unit tests check that:

- one epoch never moves the fee by more than the step;
- idle capacity walks the fee down to the floor and holds it there;
- sustained congestion climbs to the ceiling and stops;
- price-sensitive demand converges to the target from below, without overshoot;
- growing verified capacity lowers the fee every epoch until the floor stops it;
- epochs without verified capacity hold the fee.

### 3.4 Floor, ceiling and tips

- **Worker floor `W`** (operator input): the ARC each executing worker must receive per 1M billable tokens.
  It is meant to cover electricity.
- **Base-fee floor** = ceil(W × executions per job ÷ workers' share).
  - Twin execution (2 executions, 83% to workers): floor = **2.41 W**.
  - Sampled audits (1 execution, 87-89% to workers): floor = **1.12-1.15 W**.
  - So moving from twin execution to sampled audits roughly halves the floor automatically.
- **No price oracle in v0.** The operator converts an electricity estimate into ARC by hand. The
  simulation gives USD-equivalent figures for that. On testnet the default worker floor is a nominal
  0.001 ARC per 1M tokens per execution.
- **Ceiling** = floor × 1,000 by default. At 12.5% per epoch it takes about 59 epochs of uninterrupted
  congestion to reach it, which is about 10 hours at 10-minute epochs.
- **Tip** (optional, per 1M tokens): added to the price and paid entirely to the executing workers. v0's
  community queue is first-in, first-out and does not order jobs by tip. Every quote says so.

### 3.5 Quote before run, escrow and refund

`GET /inference/quote?prompt_tokens=N&max_tokens=M[&expected_output_tokens=E][&tip_per_mtok=T][&model_id=0x…]`

- **Price lock.** The price is the current base fee plus the tip, locked until the end of the epoch
  (`valid_until_unix_ms`).
- **max** = ceil(billable(prompt, max_tokens) × price ÷ 10^6). This is the escrow hold.
- **estimate** uses E output tokens: the caller's figure, or 50% of `max_tokens` by default.
- **Paid jobs (not in v0).** A paid job would charge billable(prompt, tokens actually generated) at the
  quoted price, never more than `max`, and refund the rest. `settle()` implements and tests that rule,
  but v0 moves nothing.
- **`serviceable_now` is false** in three cases, each with a stated reason:
  - no verified worker is online;
  - fewer verified workers are online than the job needs (2 under twin execution);
  - prompt plus `max_tokens` exceed today's community dispatch budget. At the reviewed 3.3 s per
    position, the coordinator sends a job to community workers only if it fits in about 260 positions
    (`rpc.rs:6314-6348`). The check calls that same function.
- **Output cap.** Quotes never price more than 256 output tokens (`INFERENCE_RUN_MAX_TOKENS`,
  `rpc.rs:145`).

Example, with the defaults and the nominal testnet floor: a 50-token prompt with `max_tokens=200` quotes
**"≈ 0.000000362 ARC for this answer (max 0.000000603 ARC)"**. The split at the max is 250 base units to
each of the two workers, 42 to the verifier pool and 61 to the treasury.

### 3.6 Split: workers, verifier pool, treasury

The defaults are sized for twin execution (PR #139): two community workers compute every job, and
validators recompute a 5% spot check, mismatches and fallbacks.

| Share of the base fee | Twin execution (default) | Sampled audits at scale | Why |
|---|---|---|---|
| Workers | 83% (41.5% to each twin) | 87-89% | Each full execution of the job is paid the same |
| Verifier pool | 7% | 1-3% | Twin: 5% × 3 replicas = 0.15 executions per job, against 2 paid. Audits: audit rate 1-5% × replay cost 0.4-0.8 of the original = 0.4-4% |
| Treasury | 10% | 10% | Also takes all rounding residue, as the Milestone B release does |
| Tip | 100% to workers | 100% to workers | Like an EIP-1559 priority fee |

Settlement rules for when payment is wired (implemented as pure functions in v0 where noted):

- **Match.** Each twin gets one execution share (`split_charge`). The verifier pool accrues to pay for spot
  checks.
- **Mismatch.** The leg the validators confirm gets its share. The rejected leg gets nothing; its share
  goes to the verifier pool, which paid for the recomputation.
- **One leg missing, validators fall back.** The worker gets one execution share. The other share goes to
  the verifier pool, because the validators ran the job in full.
- **Unverified** (validators unavailable): no output is returned, nothing is charged, and the full hold
  is refunded.

The existing `RoleRevenueConfig` split is not used, for two reasons:

- It pays block-production roles (proposer, verifiers, observers), not the community workers who
  computed the answer.
- Milestone B gives the "replicas" only 25%. A price meant to cover worker electricity has to pay the
  workers most of it.

### 3.7 Endpoints

Both routes are read-only GETs, mounted only when the flag is on. With the flag off they return 404, as
any unknown path does. Every body carries `advisory: true`, `testnet_units_have_no_value: true` and
`settlement: "none in v0 …"`.

- `GET /inference/quote` (`arc.inference.quote.v0`) returns:
  - the display line;
  - the price (base fee, tip, floor, ceiling);
  - the token arithmetic;
  - `estimate` and `max` in base units and in ARC;
  - the escrow rule;
  - the split at the max;
  - the capacity snapshot, with last epoch's utilization and the target;
  - the dispatch-budget check.
- `GET /inference/pricing` (`arc.inference.pricing.v0`) returns:
  - the configuration and derived floor and ceiling;
  - the capacity snapshot and how it is defined;
  - for each model: the base fee, the current epoch's running utilization, the last snapshot, and the
    last 48 closed epochs (utilization, fee before and after).

Errors:

- 400 for missing, malformed or out-of-range parameters;
- 404 for a model this coordinator does not serve;
- 503 when the coordinator has no model loaded.

### 3.8 Operator flags

| Flag | Default | Range | Role |
|---|---|---|---|
| `--enable-inference-pricing` | off | | Mount the two routes and start the sampler |
| `--inference-pricing-target-utilization-bps` | 5000 | 1000-9000 | Target utilization of verified capacity |
| `--inference-pricing-max-step-bps` | 1250 | 1-5000 | Largest change per epoch |
| `--inference-pricing-epoch-secs` | 600 | 60-86400 | Epoch length |
| `--inference-pricing-worker-floor-per-mtok` | 1000000 | ≥ 1 | Base units each executing worker must receive per 1M tokens |
| `--inference-pricing-ceiling-multiple` | 1000 | 1-1000000 | Ceiling as a multiple of the floor |
| `--inference-pricing-treasury-bps` | 1000 | 0-3000 | Treasury share |
| `--inference-pricing-verifier-bps` | 700 | 0-3000 | Verifier-pool share |
| `--inference-pricing-worker-executions` | 2 | 1-2 | Workers paid per job: 2 with twin execution, 1 without |

Clap rejects out-of-range values at startup. `mount()` re-validates and stays off on any invalid
configuration.

## 4. What makes tokens cheaper over time

Each lever applied on its own, from the simulation. Prices are USD-equivalent per 1M processed tokens, at
the floor (electricity only, US residential power).

| Step | Lever | Electricity per execution | Paid executions per job | Floor | Change | vs cheapest API ($0.030) | Status |
|---:|---|---:|---:|---:|---:|---:|---|
| 0 | Today: INT8 CPU engine, single stream, twin execution | $1.45 | 2 | $3.50 | | 117× | 1.4 tok/s measured |
| 1 | Faster CPU kernels | $0.282 | 2 | $0.680 | −81% | 23× | Target: 7.2 tok/s was reached once on an M2 Ultra |
| 2 | Batched GPUs and Max-class Macs | $0.010 | 2 | $0.025 | −96% | 0.84× | Needs a GPU determinism proof |
| 3 | Verification: twin to sampled audits, p = 5% | $0.010 | 1 | $0.012 | −52% | 0.40× | Needs the audit path |
| 4 | Verification: p = 1% | $0.010 | 1 | $0.012 | −2% | 0.39× | After probation and reputation |
| 5 | Cheaper model of equal quality | $0.0042 | 1 | $0.0047 | −60% | 0.16× | Quality parity must be measured |
| | Capacity growth | | | unchanged | removes the premium above the floor (section 5.3) | | The mechanism in this PR |

- **Capacity growth** is the only lever the pricing mechanism provides. It takes the price back down to
  the floor after congestion. It does not move the floor.
- **Kernels.** The engine already contains bit-exact SIMD INT8 kernels and batched prefill, both opt-in
  (`crates/arc-inference/src/canonical_simd.rs`, `canonical_prefill.rs`).
  - The 7.2 tok/s figure is the historical 139 ms/token single-node CPU run (`README.md:540-552`), not a
    production measurement.
- **Batching** is the largest lever. It is worth about 15× on an RTX 4090: about 150 tok/s single-stream
  against about 3,000 tok/s batched. It depends on deterministic GPU kernels, which nobody has published
  across vendors.
- **Verification** moves from two full executions plus spot checks to one execution plus a 1-5% audit.
  That roughly halves the floor.
- **Models.** A modern 3B model that matches Llama-2-7B-Chat moves about 0.4 times the weight bytes per
  token. PR #137 adds SmolLM3-3B as a candidate. Parity on ARC's integer engine must be measured first.

## 5. Simulation

`python3 scripts/inference_pricing_sim.py` prints every table below. It uses a fixed random seed and only
the standard library.

### 5.1 Assumptions

Sources are listed in section 12. Values marked [ASSUMPTION] were not measured.

| Input | Value | Source |
|---|---|---|
| Electricity | US residential, 18.31 ¢/kWh (July 2026) | EIA |
| Today's worker | 1.4 tok/s, at 40 W [ASSUMPTION] | A live receipt: 16 tokens at 716 ms/token on a community Apple Silicon MacBook Pro |
| Faster CPU kernels | 7.2 tok/s, at 40 W [ASSUMPTION] | `README.md:552` |
| Batched fleet [ASSUMPTION: GPU determinism proven] | 30% RTX 4090-class nodes at 3,000 tok/s and 550 W; 70% M4 Max-class at 248 tok/s and 80 W | Batched benchmarks; the M4 Max figure is an estimate |
| Twin execution | 2 paid executions; validators recompute 5% of jobs on 3 replicas | PR #139 |
| Sampled audits | Replay costs 0.6 of the original [ASSUMPTION] | A deterministic replay is one prefill pass. Our estimate is 0.4-0.8 of the original GPU time for chat and prompt-heavy traffic; SYNTHETIC-2 measured 1/25 on decode-heavy traces. |
| Cheaper model [ASSUMPTION] | 0.4× the bytes moved per token | |
| Demand at the floor price [ASSUMPTION] | 30% average utilization, a daily cycle of ±30-50% | One live consumer-Mac network reports 16% utilization. 30% is a planning assumption. |
| Demand bursts [ASSUMPTION] | 3 nodes: a daily 2 h demo burst at 4×. 100 nodes: 1 h at 2×. | |
| Price elasticity of demand [ASSUMPTION] | 0.5 | |
| Controller | Target 50%, step 12.5%, 10-minute epochs | Defaults |
| API benchmark | Blended per 1M processed tokens, chat mix (500 in, 500 out) | OpenRouter endpoints, read 2026-10-04 |

All prices are **USD-equivalent**. v0 has no ARC/USD reference, and testnet ARC has no value. A USD
figure here is what the floor would have to equal for an honest worker to recover its electricity. It is
not a forecast of anyone's income.

### 5.2 Price per 1M tokens against node count

| Nodes | Stage and assumptions | Floor | Base fee, load-weighted mean (p95) × floor | User price | vs cheapest API | Each worker's income per 1,000-token job | Its electricity per job |
|---:|---|---:|---:|---:|---:|---:|---:|
| 3 | Today: CPU single stream, twin execution | $3.50 | 1.42 (2.89) | **$4.96** | **165×** | $0.0021 | $0.0015 |
| 100 | Community beta: faster CPU kernels [TARGET], twin execution | $0.680 | 1.06 (1.34) | **$0.722** | **24×** | $0.0003 | $0.0003 |
| 1,000 | Batched GPU/Max fleet [ASSUMPTION], audits at p = 5% | $0.012 | 1.00 (1.00) | **$0.012** | **0.40×** | $0.0000105 | $0.0000105 |
| 100,000 | The same fleet, audits at p = 1%, cheaper model [ASSUMPTION] | $0.0047 | 1.00 (1.00) | **$0.0047** | **0.16×** | $0.0000042 | $0.0000042 |

API benchmarks per 1M processed tokens:

| Benchmark | Chat mix (500 in, 500 out) | Prompt-heavy mix (6,000 in, 400 out) |
|---|---:|---:|
| Llama-3.1-8B, DeepInfra (cheapest) | $0.030 | $0.021 |
| Llama-3.1-8B, Groq | $0.065 | $0.052 |
| Llama-3.1-8B, Together | $0.140 | $0.140 |
| H100 rented at $2.20/h, serving 8B at 15,000 tok/s (a cost, not a price) | $0.041 | $0.041 |

**Reading:**

- **3 nodes.** A short demo burst saturates the network. The fee climbs at up to 12.5% per epoch and
  peaks at 4.1× the floor. Even so, 7% of the time demand still exceeds capacity: per-epoch pricing does
  not clear a sudden burst, and queueing and admission limits have to absorb it. The congestion premium
  is also the only income above electricity: $0.0021 against $0.0015 per job.
- **1,000 nodes and up**, at 30% average utilization: daily peaks stay under the target, so the fee rests
  at the floor and each worker recovers its electricity and nothing more.
- **The comparison flatters ARC.** The API column uses Llama-3.1-8B, which is a stronger model than
  ARC's Llama-2-7B-Chat. An equal-quality comparison would favor the API further.
- **Prompt-heavy traffic** is cheaper on APIs, which charge less for input. ARC only matches that once
  batched prefill lets the input weight drop.

### 5.3 Capacity growth: the premium falls back to the floor

Demand is fixed at 80% of verified capacity at the floor price, with elasticity 0.5, so the fee first
settles where utilization is 50%. At epoch 0 verified capacity doubles.

| Epoch | Verified capacity | Utilization | Base fee × floor |
|---:|---:|---:|---:|
| before | 1× | 50% | 2.56 |
| 0 | 2× | 25% | 2.56 |
| 6 | 2× | 30% | 1.80 |
| 12 | 2× | 34% | 1.36 |
| 24 | 2× | 40% | 1.00 |
| 60 | 2× | 40% | 1.00 |

Within four hours of 10-minute epochs, the extra capacity has removed the whole premium.

### 5.4 Sensitivity: epoch length and target utilization

| Demand | Epoch | Target | Base fee, load-weighted mean (p95, max) × floor | Time above target | Time in backlog |
|---|---|---:|---:|---:|---:|
| 3 nodes (bursty) | 10 min | 50% | 1.42 (2.89, 4.11) | 9% | 7% |
| 3 nodes (bursty) | 10 min | 70% | 1.31 (2.35, 3.29) | 8% | 8% |
| 3 nodes (bursty) | 1 h | 50% | 1.06 (1.23, 1.27) | 9% | 8% |
| 3 nodes (bursty) | 13.8 h (the reward epoch) | 50% | 1.00 (1.02, 1.02) | 10% | 8% |
| 100 nodes | 10 min | 50% | 1.06 (1.34, 1.63) | 4% | 0% |
| 100 nodes | 1 h | 50% | 1.01 (1.09, 1.11) | 4% | 0% |
| 100 nodes | 13.8 h (the reward epoch) | 50% | 1.00 (1.00, 1.00) | 4% | 0% |

- **10-minute epochs** react within the hour.
- **1-hour epochs** smooth the price but respond late.
- **The 13.8-hour reward epoch** (216,000 blocks) never reacts to a daily peak, so it cannot act as a
  congestion price at all.
- **A 70% target** raises the price less often, but leaves less headroom for bursts.

### 5.5 [PROJECTION] Kimi-class island tier

Everything in this subsection is a **projection** from published measurements and arithmetic. None of it
has been measured on ARC.

**The setting.**

- Kimi K2 and K2.6 have about 1T parameters, or about 582 GB with INT4 experts. That does not fit one
  consumer device, so the model would run on an **island**: machines that one operator links over
  Thunderbolt or a LAN.
- Islands are checked by **sampled stage audits**. A verifier re-runs one pipeline stage from its
  committed input, at 5% of jobs, with a replay costing about 0.6 of the original. That is 3% of compute.
- The split is therefore 87% to the island owner, 3% to the verifier pool and 10% to the treasury.

**Fair price** is the price per 1M output tokens that pays the island owner its electricity (and, where
stated, its hardware back) with the verifier pool and treasury on top. It is the floor rule of
section 3.4 with one paid execution.

**Verifiers are paid at about cost.** At the fair price the verifier pool is 3.4% of the owner's cost,
against audits that use 3% of compute. That leaves about 15% headroom, provided a verifier's cost per
unit of compute matches the owner's.

**Inputs.**

| Island | Source |
|---|---|
| 2× Mac Studio M3 Ultra 512 GB over Thunderbolt 5: 23 tok/s on one stream, about 70 tok/s across 8 streams, 560 W, $11,699 each | A model calibrated to Kimi K2 at about 30 tok/s on 4 Mac Studios (exo, Dec 2025) and DeepSeek-R1 671B at 20.2 tok/s on one M3 Ultra |
| 22× RTX 5090 pipeline on one LAN: 576, 1,514 or 2,139 tok/s at 16, 64 or 300 streams (36, 23.7 or 7.1 tok/s per stream) | The same batching model. Power of 10 kW and about $55,000 of hardware, including hosts and switch, are assumptions. |
| Prices | US residential power at 18.31 ¢/kWh. A low-cost case at 7.6 ¢/kWh (the China and India average). Hardware paid back straight-line over 5 or 3 years. |
| API benchmark | Kimi K2.6 output on OpenRouter providers: $2.45 to $4.60 per 1M output tokens (read 4-6 Oct 2026; the lowest listing seen on 2026-10-05 was $2.40) |
| Goal | $0.60 or less per 1M output tokens, owner payouts included (✓ marks it) |

**[PROJECTION] Hardware already owned: electricity only, US residential power**

| Island | Per-stream tok/s | Aggregate tok/s | Power | Hardware | Owner electricity, $/1M out | Fair price, $/1M out | vs API |
|---|---:|---:|---:|---:|---:|---:|---|
| 2x Mac Studio M3 Ultra 512 GB, 1 stream | 23 | 23 | 560 W | $23,398 | $1.24 | $1.42 | 42%-69% below |
| 2x Mac Studio M3 Ultra 512 GB, 8 streams | 8 | 70 | 560 W | $23,398 | $0.407 | $0.467 ✓ | 81%-90% below |
| 22x RTX 5090 LAN rig, 16 streams | 36 | 576 | 10,000 W | $55,000 | $0.883 | $1.01 | 59%-78% below |
| 22x RTX 5090 LAN rig, 64 streams | 23.7 | 1,514 | 10,000 W | $55,000 | $0.336 | $0.386 ✓ | 84%-92% below |
| 22x RTX 5090 LAN rig, 300 streams (batch tier) | 7.1 | 2,139 | 10,000 W | $55,000 | $0.238 | $0.273 ✓ | 89%-94% below |

**[PROJECTION] Hardware paid back: fair price by utilization**

| Island | Power, $/kWh | Payback | 16% busy | 30% busy | 60% busy | 90% busy |
|---|---:|---|---:|---:|---:|---:|
| 2x Mac Studio M3 Ultra 512 GB, 1 stream | 0.183 | 5 years | $47.77 (above every API price) | $26.14 (above every API price) | $13.78 (above every API price) | $9.66 (above every API price) |
| 2x Mac Studio M3 Ultra 512 GB, 1 stream | 0.183 | 3 years | $78.67 (above every API price) | $42.62 (above every API price) | $22.02 (above every API price) | $15.16 (above every API price) |
| 2x Mac Studio M3 Ultra 512 GB, 8 streams | 0.183 | 5 years | $15.70 (above every API price) | $8.59 (above every API price) | $4.53 (up to 2% below) | $3.17 (up to 31% below) |
| 2x Mac Studio M3 Ultra 512 GB, 8 streams | 0.183 | 3 years | $25.85 (above every API price) | $14.00 (above every API price) | $7.24 (above every API price) | $4.98 (above every API price) |
| 22x RTX 5090 LAN rig, 16 streams | 0.183 | 5 years | $5.36 (above every API price) | $3.33 (up to 28% below) | $2.17 (11%-53% below) | $1.79 (27%-61% below) |
| 22x RTX 5090 LAN rig, 16 streams | 0.183 | 3 years | $8.26 (above every API price) | $4.88 (above every API price) | $2.95 (up to 36% below) | $2.30 (6%-50% below) |
| 22x RTX 5090 LAN rig, 64 streams | 0.183 | 5 years | $2.04 (17%-56% below) | $1.27 (48%-72% below) | $0.827 (66%-82% below) | $0.680 (72%-85% below) |
| 22x RTX 5090 LAN rig, 64 streams | 0.183 | 3 years | $3.14 (up to 32% below) | $1.86 (24%-60% below) | $1.12 (54%-76% below) | $0.876 (64%-81% below) |
| 22x RTX 5090 LAN rig, 300 streams (batch tier) | 0.183 | 5 years | $1.44 (41%-69% below) | $0.898 (63%-80% below) | $0.586 ✓ (76%-87% below) | $0.481 ✓ (80%-90% below) |
| 22x RTX 5090 LAN rig, 300 streams (batch tier) | 0.183 | 3 years | $2.23 (9%-52% below) | $1.31 (46%-71% below) | $0.794 (68%-83% below) | $0.620 (75%-87% below) |
| 22x RTX 5090 LAN rig, 64 streams | 0.076 | 5 years | $1.82 (26%-61% below) | $1.04 (57%-77% below) | $0.602 (75%-87% below) | $0.455 ✓ (81%-90% below) |
| 22x RTX 5090 LAN rig, 64 streams | 0.076 | 3 years | $2.92 (up to 37% below) | $1.63 (33%-65% below) | $0.896 (63%-81% below) | $0.651 (73%-86% below) |
| 22x RTX 5090 LAN rig, 300 streams (batch tier) | 0.076 | 5 years | $1.28 (48%-72% below) | $0.738 (70%-84% below) | $0.426 ✓ (83%-91% below) | $0.322 ✓ (87%-93% below) |
| 22x RTX 5090 LAN rig, 300 streams (batch tier) | 0.076 | 3 years | $2.07 (16%-55% below) | $1.15 (53%-75% below) | $0.634 (74%-86% below) | $0.461 ✓ (81%-90% below) |

**[PROJECTION] Utilization an island needs for $0.60 per 1M output tokens**

| Island | Power, $/kWh | Hardware already owned | 5-year payback | 3-year payback |
|---|---:|---|---|---|
| 2x Mac Studio M3 Ultra 512 GB, 1 stream | 0.183 | never: electricity alone is too high | never: electricity alone is too high | never: electricity alone is too high |
| 2x Mac Studio M3 Ultra 512 GB, 8 streams | 0.183 | any | not reachable (needs 1838.0%) | not reachable (needs 3063.4%) |
| 22x RTX 5090 LAN rig, 16 streams | 0.183 | never: electricity alone is too high | never: electricity alone is too high | never: electricity alone is too high |
| 22x RTX 5090 LAN rig, 64 streams | 0.183 | any | not reachable (needs 123.7%) | not reachable (needs 206.2%) |
| 22x RTX 5090 LAN rig, 300 streams (batch tier) | 0.183 | any | at least 58% | at least 96% |
| 22x RTX 5090 LAN rig, 64 streams | 0.076 | any | at least 61% | not reachable (needs 100.4%) |
| 22x RTX 5090 LAN rig, 300 streams (batch tier) | 0.076 | any | at least 39% | at least 65% |

**Reading** (all projections):

- **With hardware the owners already have, batched islands meet the goal.**
  - 2× M3 Ultra at 8 streams: $0.47.
  - 22× RTX 5090 at 64 streams: $0.39.
  - The 300-stream batch tier: $0.27.
  - That is 81-94% below the API range. The owner then recovers electricity only.
- **With hardware paid back at US residential power, only the GPU rig's batch tier gets there**, at about
  7 tok/s per user: 58% utilization or more with a 5-year payback.
  - At 64 streams, about 24 tok/s per user, the best case is $0.68 at 90% busy.
  - Mac islands never fall below about $3.17 once their hardware is paid back.
- **Cheaper power changes the picture.** At 7.6 ¢/kWh, the 64-stream rig reaches the goal at 61%
  utilization or more with a 5-year payback.
- **Price and speed pull against each other.** No configuration that reaches $0.60 gives one user more
  than about 24 tok/s, against about 59 tok/s from Chutes, a production provider, today. Reaching both
  goals needs faster kernels, speculative decoding (about 1.3-1.4× on a LAN pipeline), or both.
- **Most paid tokens are input.** On a GPU rig, prefill costs the owner about $0.24 per 1M input tokens
  at 30% use with a 3-year payback (about $0.28 as a fair price). That is below the $0.465-1.09 API
  input range. Mac islands prefill slowly: about 24 s for a 6,000-token prompt.
- **Utilization drives the payback columns.** One consumer-Mac network reports 16%. At 16% the hardware
  part of every payback cell is 1.9 times what it is at 30%.

**How the mechanism uses this.**

- A coordinator serving the Kimi tier sets its own worker floor: the owner cost per 1M output tokens it
  guarantees.
- It sets `input_weight_bps` to the prefill-to-decode cost ratio, about 1,500 basis points on a GPU rig.
- The base fee then rests at that floor and rises only when the island tier's verified capacity is
  congested.

## 6. Guardrails

**No promise of profit.**

- The floor covers an operator's electricity estimate and nothing more.
- Workers are not guaranteed jobs, and the price can sit at the floor indefinitely.
- Interfaces must not say "earn", "income" or "yield" about testnet units. Quotes state that a quote is
  not a promise of income to anyone.

**Testnet units have no real-world value.** Every response says so, and every response says no ARC
moves. The capped 2.5 testnet ARC promotional reward is separate and unchanged.

**Spam and abuse.**

- The base fee never falls below the floor, and every job is billed for at least 64 tokens.
- Output is capped at 256 tokens, prompts at 32,768 tokens, and the community dispatch budget still
  applies.
- Per-account limits arrive with settlement, enforced at admission:
  - one open job per payer key;
  - a bounded number of queued jobs per payer;
  - a rate limit;
  - escrow funded before work starts.
- The quote endpoints themselves are read-only GETs and change no chain or community state. Each request
  scans at most the 1,024-entry worker registry. Rate limiting belongs at the gateway, because the node
  never sees client IPs.

**Manipulation.**

- **Workers withholding capacity** (they stop polling) push utilization, and so the fee, up. The fee
  rises by at most 12.5% per epoch, never past the ceiling, and the withholders forgo every job meanwhile.
- **Fake idle workers** can only push the fee down to the floor, never below it. A worker counts as
  capacity only after a quorum-verified job.
- **The coordinator** is team-run, and nothing stops it misreporting its own utilization. A binding price
  must take utilization from on-chain evidence (section 8).

**Coordinator and validator load.**

- One sampler task scans at most 1,024 registry entries every 15 s and holds the pricing lock for
  microseconds.
- There are two GET routes. There are no chain writes, no validator recomputations and no new network
  messages.
- State is bounded: at most 16 models × 48 epochs of history.

## 7. Limits of v0

- **Advisory only.** Nothing is charged, held or refunded.
- **Each coordinator prices on its own.** The six prices should be close, because they read the same
  workers, but they will not be identical.
- **Utilization is a proxy.** The biases are listed in section 3.2.
- **Capacity counts workers, not tokens per second.** A fast and a slow worker count the same.
- **The expected output length** defaults to 50% of `max_tokens`. A paid job's charge would use the real
  count.
- **There is no ARC/USD reference.** The operator sets the worker floor in ARC by hand.
- **State is in memory.** A restart resets the fee to the floor, which is also the resting price.

## 8. Follow-up: binding prices on chain (consensus changes, not in v0)

1. **Version 2 of the protocol-4 job.**
   - Add `price_per_mtok` and `input_weight_bps` to `InferenceJob`, and keep `reserved_max_payment` as the
     hold.
   - `plan_finalize` (`inference_contract.rs:423-486`) charges min(hold, billable(tokens in the
     certificate) × price). The certificate already carries the complete bounded output, so every
     validator counts the same tokens.
   - Credit workers, the verifier pool and the treasury by the rules in section 3.6, instead of
     validators by stake.
   - Refund the rest, and bump `INFERENCE_CONTRACT_VERSION`.
2. **An on-chain base fee.**
   - Keep per-model base-fee state, updated once per epoch by a deterministic state transition from
     on-chain evidence: verified-work receipts and capacity advertisements (`CapacityAdvertisement`,
     0x1f).
   - Every validator then computes the same fee, and a job's price must be at least the base fee when it
     is included.
3. **Settlement at scale.**
   - Per-account escrow, off-chain cumulative vouchers, and one payout root per epoch.
   - Published designs of this kind need 10 to 1,000 transactions per second, not one transfer per
     request.
4. **The old fee code.** Reconcile or remove `InferenceGasLane` (`gas.rs`) and the unused `FeeConfig`
   paths, so only one inference fee rule exists.
5. **A USD reference** for the floor, if one is ever wanted. Out of scope here.

## 9. Hooks into open pull requests (described, not implemented)

| PR | Hook |
|---|---|
| #139 twin execution | Set the executions per job from `--community-twin-execution` (2 when on, 1 when off) instead of a separate flag. Replace the worker-count capacity with `twin_stats`' verified tokens per second. Settle per twin receipt with the rules in section 3.6. |
| #138 compute readiness and job counters | A busy flag on worker heartbeats would give an exact network-wide busy count in place of the "not long-polling here" proxy. The Jobs card can show the quote line, labelled advisory testnet. |
| #134 privacy | Pricing publishes counts only, never worker ids or names. No change needed. |
| #133 per-block fee settlement | It settles transfer fees and does not interact with this PR. A future on-chain inference fee could settle through the same per-block accumulation. |

## 10. Decisions needed

| Parameter | Default | Trade-off | Recommendation |
|---|---|---|---|
| Target utilization | 50% | Higher: the price rises later, and queues and latency grow at peaks. Lower: more headroom, and premiums sooner. | 50% while workers are few and bursty. Revisit at 1,000 or more verified workers. |
| Worker floor, which sets the price floor | Nominal: 0.001 testnet ARC per 1M tokens per execution | Electricity per execution ÷ an ARC reference price × (1 + margin). With a 0% margin, workers recover electricity only. | Keep it nominal on testnet. Before any real-value use, decide the margin and get a counsel review. |
| Split | Workers 83% (41.5% each twin), verifier pool 7%, treasury 10% | A larger treasury share raises the floor for everyone | Use these with twin execution. Move to 87-89% / 1-3% / 10% when audits replace twins. |
| Epoch length | 10 minutes | Shorter: tracks peaks, and quotes stay valid for less time. Longer: smoother, but it misses peaks. The 13.8 h reward epoch never reacts. | 10 minutes |
| Step and ceiling | 12.5% per epoch, 1,000× the floor | A larger step reacts faster and swings more | Keep the EIP-1559 step |
| What a Kimi-tier owner payout covers (section 5.5, projection) | Not set | Electricity only (the owner already has the hardware): a $0.27-0.47 fair price meets the $0.60 goal, at 7-24 tok/s per user. Hardware payback too: $0.60 is met only on dense GPU rigs at 58% utilization or more (US power, 5 years), or 61% at 7.6 ¢/kWh. | Decide this first. It, not the mechanism, decides whether $0.60 is reachable. |

## 11. Rollout

1. **Merge with the flag off.**
   - With the flag off, nothing changes: no routes are mounted and no sampler runs.
   - The two routes need adding to the gateway's public GET allowlist (`DEFAULT_PUBLIC_GET_PATHS`,
     `scripts/recovery/recovery_rollout.py:146-163`) in the validator release that turns the flag on. This
     PR does not change that sealed list.
2. **Enable the flag on one coordinator for 7 days.**
   - Read `/inference/pricing` history: utilization, fee and samples per epoch.
   - Compare the utilization proxy with `twin_stats` once #139 is live.
3. **Show the quote line** in the demo and the desktop app, labelled "advisory · testnet".
4. **Binding prices** come only after section 8, an external review, and counsel's sign-off on how
   testnet units are described.

**Stop rules.** Turn the flag off if:

- the fee sits at the ceiling for 6 or more epochs;
- the proxy disagrees with `twin_stats` by more than 20 percentage points;
- quotes are mistaken for income claims.

## 12. Sources

- US residential electricity, 18.31 ¢/kWh (July 2026): EIA, Electric Power Monthly, table 5.6.A.
- API prices (read 2026-10-04): OpenRouter endpoints API, `openrouter.ai/api/v1/models/{model}/endpoints`.
  Llama-3.1-8B: DeepInfra $0.02 / $0.04, Groq $0.05 / $0.08, Together $0.14 / $0.14 per 1M input / output
  tokens.
- H100 cost floor: $2.20/h rental, serving 8B at about 15,000 tok/s (TensorRT-LLM FP8), which is $0.041
  per 1M tokens.
- Consumer throughput:
  - llama.cpp discussions #15013 (CUDA) and #4167 (Apple silicon);
  - vLLM batched benchmarks of RTX GPUs (CloudRift, 2025-10);
  - the M4 Max batched figure is an estimate from llama.cpp batched-bench ratios.
- Verification cost:
  - the replay of a deterministic job is one prefill pass, which we estimate at about 0.4-0.8 of the
    original GPU time depending on the traffic mix (an estimate, not a measurement);
  - TOPLOC (arXiv 2505.07291) and Prime Intellect's SYNTHETIC-2 report about 1% overhead and "25×
    cheaper than re-doing the original inference".
- Settlement designs: Filecoin payment channels (cumulative vouchers), and the x402 `upto` and
  `batch-settlement` schemes.
- Today's speed: a live on-chain receipt of 16 tokens at 716 ms/token, from a community Apple Silicon
  MacBook Pro (`/inference/attestations` on the Singapore validator, block 5,406,627, read 2026-10-05).
- Kimi-class islands (section 5.5, projections):
  - Kimi K2 Thinking at about 30 tok/s on 4 Mac Studio M3 Ultras over Thunderbolt 5 RDMA (exo 1.0,
    jeffgeerling.com, Dec 2025);
  - DeepSeek-R1 671B Q4 at 20.21 tok/s on one M3 Ultra 512 GB at 290 W (geerlingguy/beowulf-ai-cluster,
    issue 17);
  - Kimi K2.6 prices: OpenRouter endpoints API (output $2.40-4.60 per 1M on 2026-10-05; input
    $0.465-1.09);
  - Mac Studio prices, 22-GPU rig aggregate throughput and rig power are calculated or assumed, as
    labelled in the tables.
