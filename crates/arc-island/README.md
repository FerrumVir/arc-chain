# arc-island: ARC-AC v0 island discovery and formation

A dormant library and an offline simulator. It decides which opted-in
community machines serve one copy of a Kimi-class model together, how they
split it, and how the group lives through churn. Nothing in the node depends
on it. It has no network code and changes no protocol behaviour.

Design sources: research-6 §2, §3 and §6 (ARC-AC v0) and research-7 §2–§4
(regional clustering). Model figures come from
`docs/protocol/kimi-k26-checkpoint.md` (PR #156).

| Module | What it does |
|---|---|
| `device` | Device descriptor built from the Proof Kit's `arc.proof-result.v1` island facts (PR #149), measured link statistics, and consent (#138's tri-state; both answers must be an explicit yes). |
| `model` | Per-unit weight, active-byte and KV accounting; `ModelSpec::kimi_k26_int4()` comes to 582.6 GB. |
| `fit` | Contiguous layer partitioning sized to each member's memory and speed (integer dynamic program: slowest stage first, then the sum of stage times), plus ring ordering (exact for up to 8 members, nearest neighbour then 2-opt above that). |
| `form` | Tiers T0 (one device), T1a (Thunderbolt 5), T1b (LAN), then T2 swarms metro → zone → region with measured p95 diameters of 20 / 35 / 60 ms. Members are added largest first until the copy fits with 10% headroom; warm spares are assigned round-robin. |
| `perf` | Projected per-answer tok/s with and without speculative decoding, the chosen draft depth, batching depth, and aggregate tok/s. Reporting only. |
| `lifecycle` | Form → qualify (golden digest) → serve. On a member loss, promote a covering spare and re-qualify, otherwise dissolve. Consent withdrawal takes effect at once. |
| `selftest` | The qualification contract: per-stage boundary digests locate the first faulty stage. `ToyPipeline` is an exact integer stand-in for the real engine. |
| `admission` | Request admission against KV budget, predicted per-stream speed, island health and prefill queue length. |
| `sim` | Synthetic inventory (research-7 §4.1 mix), an RTT model built from RIPE Atlas home access quantiles, scenario runs, churn over a 6 h lease, and the report. |

Formation, partitioning, spare choice and the lifecycle use integers only,
so a recomputation from the same inputs picks the same islands.

```sh
cargo test -p arc-island
cargo run --release -p arc-island --bin arc-island-sim -- --seed 7 --out sim-out
```

The simulator's numbers are projections from labelled assumptions over a
synthetic inventory. They are not measurements and not community counts; the
report lists every assumption.
