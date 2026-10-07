# arc-island: ARC-AC v0 island discovery and formation

A dormant library and an offline simulator. It decides which opted-in
community machines serve one copy of a Kimi-class model together, how they
split it, and how the group lives through churn. Nothing in the node depends
on it. It has no network code and changes no protocol behaviour.

Design sources: research-6 §2, §3 and §6 (ARC-AC v0) and research-7 §2–§4
(regional clustering). Model figures come from
`docs/protocol/kimi-k26-checkpoint.md` (PR #156).

## Trust model (fail closed)

- **Golden reference.** Real serving needs the reference that
  `GoldenReference::pinned` returns for the plan's exact checkpoint and
  integer profile. That table is compiled in and changes only by review. It
  is **empty for Kimi K2.6** because no real K2.6 run exists yet, so a real
  Kimi island refuses to qualify. Callers cannot construct a measured
  reference.
- **Executor.** The executor must report measured provenance. The toy
  pipeline exists only under `cfg(test)` or the `simulator` feature and is
  always synthetic.
- **Inputs.** Every device and link carries evidence: measured or
  synthetic, plus a timestamp. Serving needs measured, fresh inputs,
  including measured memory and bandwidth. A synthetic formation reaches at
  most the `Simulated` state. The island re-checks its health on every
  admission.
- **Lifecycle authority.** `Island::new` always starts unqualified. Plan,
  state, generation and events have read-only accessors; diagnostic snapshots
  are serializable but cannot be deserialized into a running island. Import a
  plan into a new island and qualify it again. Real admission and health gates
  reject synthetic plans independently. With the simulator feature,
  `simulation_health_check` evaluates simulated health without opening admission.
- **Consent.** Owners answer two questions, compute and island (#138). The
  grant is bound to one owner and one device, and it expires. Withdrawal
  takes effect at the next check.
- **Spares.** Each island needs 1 / 2 / 3 warm spares for 2–6 / 7–22 / 23+
  stages. Every spare holds the largest stage and meets the link rule with
  every member.
  - When a spare leaves, admission pauses until a replacement is reserved.
  - When a member leaves, a spare is promoted. The island then restores
    in-flight state from a trusted ledger checkpoint and passes a fresh golden
    run before serving again.

## Modules

| Module | What it does |
|---|---|
| `device` | Descriptor built from the Proof Kit's `arc.proof-result.v1` island facts (#149) plus measured links. Also holds evidence, freshness, bound consent and RDMA evidence. |
| `model` | Per-unit weight, active-byte and KV accounting, plus the model identity. `ModelSpec::kimi_k26_int4()` comes to 582.6 GB. |
| `fit` | Integer DP for contiguous, speed-aware layer partitioning. Ring order is exact up to 8 members, then nearest neighbour plus 2-opt. |
| `form` | Tiers in order: T0; T1a (measured RDMA plus a collective p99 of at most 0.2 ms); T1b LAN; then T2 swarms. Swarms are searched metro → zone → region → declared neighbouring regions, at measured p95 diameters of 20 / 35 / 60 / 75 ms. Includes the required spares. |
| `perf` | Projected per-answer tok/s with and without speculation, plus batching depth and draft depth. Reporting only. |
| `lifecycle` | Qualification against the pinned golden, health checks, heartbeats, spare promotion, checkpoint recovery, replenishment, dissolution. |
| `selftest` | The golden trust root and the self-test contract. |
| `admission` | Request admission: a health check, the full spare count, KV budget, predicted speed and prefill queue. |
| `sim` (feature `simulator`) | Synthetic inventory, RTT model, scenarios, lease churn and the report. |

Formation, partitioning, spare choice and the lifecycle use integers only,
so a recomputation from the same inputs picks the same islands.

```sh
cargo test -p arc-island
cargo test -p arc-island --features simulator
cargo run --release -p arc-island --features simulator --bin arc-island-sim -- --seed 7 --out sim-out
```

The simulator's numbers are projections from labelled assumptions over a
synthetic inventory. They are not measurements and not community counts; the
report lists every assumption.

Lease accounting separates non-dissolved spare-shortfall time (including
waiting for an eligible replacement) from time after dissolution. Both shares
divide by initially formed islands × the full lease duration; the integer
millisecond totals and remaining time are in JSON. Optional-spares scenarios
have zero spare-shortfall time. Tokens/day are fully loaded ceilings excluding
both downtimes, recovery stalls, prefill and audit. No projection in the seed-7
report reaches 59 tok/s per answer.
