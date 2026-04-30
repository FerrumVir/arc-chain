# Changelog

All notable changes to ARC Chain are tracked here. This project follows
[semantic versioning](https://semver.org/).

## Unreleased - 2026-04-22

**Cluster self-heal, latency-aware routing, slashing hook-in, retirement of dead seeds.**

### Self-heal daemon (GH #30)
- **`scripts/arc-self-heal.sh`** - on-host bash daemon running as a systemd
  unit on each seed. Polls `http://127.0.0.1:9090/health` every 30 s and
  restarts arc-node on either (a) RPC silence ≥180 s or (b) `dag_round`
  unchanged ≥300 s while a remote peer is ≥100 rounds ahead. Captures
  every `--shard-range`, `--model`, and `ARC_PUBLIC_SOCKET` from
  `/proc/PID/cmdline` + `/proc/PID/environ` so restarts reuse the live
  argv exactly. Persists last-good snapshot to
  `/root/arc-chain/.self-heal-last-good.sh` so the daemon can relaunch
  arc-node from scratch if the process is already dead when the silent
  threshold fires.
- **`scripts/arc-self-heal.service`** - systemd unit with `KillMode=process`
  (so `systemctl restart arc-self-heal` doesn't take arc-node down as
  cgroup collateral) and NO `MemoryMax` (arc-node inherits the cgroup
  via setsid and a cap on the supervisor OOM-kills the node during the
  1 GB embedding load).
- **`scripts/install-self-heal.sh`** - idempotent per-host installer;
  refuses to install if `systemctl is-active arc-node` returns active
  (would conflict with the daemon's spawn path).
- 5 min `RESTART_DEBOUNCE` (bumped from initial 300 s proposal to 600 s)
  so a cold-boot cycle fits inside one debounce without flapping.
- `MIN_HEALTHY_PEERS=4` safety rail - drift-triggered restart refuses to
  fire if fewer than 4 remote peers are healthy, preventing a cascade
  from taking consensus below quorum.
- Deployed to all 6 seeds; two live repairs captured in-session.

### Coordinator routing (GH #29)
- **`NodeState.latency_stats`** - `DashMap<socket_addr, LatencyEWMA>`
  (α=0.2). Folded on every successful `forward_shard` hop in both
  `inference_run_sharded` (prefill worker + gen loop) and
  `inference_run_consensus` (per-replica parallel dispatch).
- **Per-range replica lists sorted by EWMA ascending** before the
  coordinator picks primary (`run_sharded`) or top-k (`run_consensus`).
  Unseen replicas keep insertion order at the tail so cold-start doesn't
  starve first-try dispatch.
- **`GET /inference/latency_stats`** - exposes the map sorted ascending
  for dashboard / diagnostics. Does not affect determinism - only which
  replicas answer, not what they answer.

### Divergence → on-chain slashing (GH #31)
- When `/inference/run_consensus` records non-empty `divergent_replicas`,
  the handler auto-submits one `InferenceCommitment` per divergent
  replica via `arc_vm::inference_verify::VerificationManager` and one
  `VerificationChallenge(ConsensusVerification)` from the coordinator.
- Response JSON gains `consensus.auto_challenges[]` with
  `commitment_id`, `challenge_id`, `divergent_replica`, `their_hash`.
- `AUTO_CHALLENGE_BOND = 100_000` is a placeholder; final value + payer
  (coordinator treasury vs honest-majority split) pending operator call.
- Divergent-replica provider identity derived as
  `hash("divergent:<node_name>")` - stable pseudo-ID until real
  validator-address reconciliation lands.

### Cosmetic (GH #33)
- **`/shards fully_covered`** - walks `BTreeSet<(start_layer, end_layer)>`
  to dedup the 18-replica list before the contiguity check. Returns
  `fully_covered=true` on a healthy 3×-replicated deployment.

### Dashboard (GH #34)
- **`dashboard/index.html`** - all 6 inference fetch sites swapped from
  `/inference/run_sharded` to `/inference/run_consensus` with `k:3`.
  The "all passing consensus" story now shows in the UI by default.

### Retirements (GH #32)
- **SAO (216.238.120.27)** and **JNB (139.84.237.49)** retired from the
  validator set: removed from `testnet-seeds.txt` (pushed to all 6 live
  seeds) and from `genesis.toml` (local only - live DAG history still
  has the 8-validator set; change takes effect on next coordinated
  genesis event). Both had been RPC-dead for weeks with unreliable
  datacenter connectivity; 6-seed × 3× replication already covers the
  inference pipeline.

## v0.5.2 - 2026-04-11

**Community inference network + SIMD performance + audit hardening.**

### Community Worker Infrastructure
- **Community gateway sidecar** (`scripts/community-gateway.py`) - Python
  HTTP server on port 3001 that runs alongside arc-node. Handles worker
  registration, heartbeat, inference job distribution via long-poll.
- **`POST /inference/community`** - submit inference to be computed by
  any available community worker. Gateway queues the request, worker
  claims via `/community/claim_work`, computes locally, submits result
  via `/community/submit_work`. All outbound-HTTPS, works behind NAT.
- **`--community-mode`** CLI flag - arc-node auto-registers with all
  seed gateways (port 3001), heartbeats every 15s, polls for inference
  jobs in background. Works with any model loaded.
- **Standalone registration script** (`scripts/arc-community-register.sh`)
  - bash loop that registers ANY version of arc-node with gateways.
  No binary rebuild needed. `curl ... | bash` one-liner.
- **UDP 443 fallback** on all 8 seeds via iptables redirect. Community
  nodes behind ISPs that block UDP 9091 can connect via 443.
- **`scripts/arc-diagnose.sh`** - 4-phase health check for community
  nodes: UDP reachability, process status, peer count, chain sync.
- Install script now checks peer count and warns if node is isolated.

### Performance (NEON SIMD on Mac M2 Ultra)
- **NEON i16 dot product** (`dot_i16_i64_neon`) - 3.7× matmul speedup.
  Vectorized 8-lane i16×i32→i64 via vmlal_s32.
- **NEON attention Q·K SIMD** (`dot_i64xi64_attn_neon`) - vectorized
  attention inner loop. Heads parallelized via `into_par_iter()`.
- **Rayon chunk-256** - empirical sweet spot for M2 Ultra core
  saturation. Combined Mac compute: **23× faster** (14.4s → 622ms
  per position).
- **Q4 NEON wiring** - opt-in via `ARC_Q4_SHARD=1`. Dispatch prefers
  Q4→I16→I8 on aarch64. Disabled by default (precision risk on 7B).
- AVX-512 i16 widening attempted but reverted (consensus segfault on
  Vultr Skylake Xeon). x86 stays on scalar fallback.

### Stability & Correctness (11 audit fixes)
- `debug_assert` bounds checks in all 8 matmul functions.
- GPU `recv().unwrap()` replaced with graceful error handling.
- 32KB prompt length limit added to `inference_run_sharded`.
- Faucet rate limit documented as testnet-only.
- Gateway default port fixed (9090 → 9944 to match binary).
- New tests: `test_q4_scale_roundtrip`, `test_i16_matmul_nonzero_output`.
- Shard registry 60s TTL + self-refresh broadcaster.
- Puller only pulls `self_shard` from each seed (prevents stale entry
  resurrection).
- No-candle on shard holders saves ~4 GB RAM on 8 GB VPS.
- `forward_one_token` panic guard for non-first shards.
- Pipelined prefill + chat template skip (~4× faster first-token).

### Infrastructure
- Shard dedup fix: pipeline walker prefers routable socket_addr.
- Updated `testnet-seeds.txt` with `:443` fallback entries (git only;
  seeds themselves use `:9091` to avoid self-dial).
- Community gateway deployed as systemd service on 6 seeds.

### Tests
- 68 tests pass (was 64 in v0.5.1). New: dot_i16 SIMD correctness,
  Q4 scale roundtrip, I16 matmul nonzero, dot_i16 tail.

## v0.5.1 - 2026-04-08

**Dashboard cache warmth indicator.** The dashboard now probes the
coordinator's `DistributedCache` on page load and tags preset prompt
buttons that are already warm with a green `⚡ INSTANT` badge. Visitors
know at a glance which clicks will return in ~100 ms (cache hit,
~200 tok/s effective) vs which will run the full 7-shard pipeline.

New RPC endpoints:
- `GET /inference/cache_stats` - `{size, capacity, total_hits,
  cache_type}`. Dashboards call this to show "N entries cached ·
  K cumulative hits" under the preset prompt row.
- `POST /inference/cache_check` - body: `{prompts: [{input, max_tokens},
  ...]}`, returns `[{input, max_tokens, cached: bool}, ...]`. Tokenizes
  server-side and re-derives the BLAKE3 cache key the exact same way
  `inference_run_sharded` does, so a check here matches what a real
  call would look up. Does NOT bump `hit_count` - a warmth probe
  should not distort LRU ordering.

`DistributedCache` gains `contains()`, `capacity()`, and `total_hits()`
helpers. `contains()` is a non-mutating probe; the existing `get()`
still bumps `hit_count` for LRU ordering.

Measured tokens/sec (live testnet, 2026-04-08):
- Cache HIT (repeat): ~200 tok/s wall (94-109 ms for 20 tokens);
  server-side serve time is 17-37 μs, the rest is HTTP roundtrip.
- Cache MISS (novel): ~0.04 tok/s (~22 sec/token through full
  7-shard pipeline).
- Speedup ratio: ~5,000× for cached results.

## v0.5.0 - 2026-04-08

**Deterministic inference cache wired into sharded handler.** The
`DistributedCache` in `crates/arc-inference/src/distributed.rs` was
fully implemented but never connected to any RPC handler. Now wired
into `inference_run_sharded`:

- **First call** with a given `(model_id, input_tokens, max_tokens)`
  triple runs the full 7-shard pipeline (~12-15 sec/token), inserts
  the result into the cache.
- **Every subsequent call** with the same triple returns in **O(1)**
  (~10-50 microseconds) with `cache.hit: true` and the same
  `output_hash`. Provably bit-identical because integer determinism
  guarantees same input → same output.
- 10,000-entry cache with LRU eviction by hit count.
- Response includes a `cache: { hit, key, size, served_in_us }` block
  so the dashboard can render a CACHE HIT badge.

This is the first user-visible 10× speedup. The dashboard's preset
prompt buttons (which always send the same prompts) now feel instant
after the first click. The 7-shard pipeline is still slow for novel
prompts - that's the next optimization (pipeline microbatching).

The cryptographic correctness of cache hits comes from the integer
engine: same model + same input ALWAYS produces the same output.
The cache isn't an approximation, it's a proof.

## v0.4.6 - 2026-04-08

**Apply unique-nonce fix to single-node `/inference/run` too.** v0.4.5 fixed
the duplicate-tx_hash issue for sharded inference but the same bug existed
in the single-node handler. Both endpoints now use the shared
`attestation_nonce: AtomicU64` so any inference call - sharded or not -
produces a unique attestation tx_hash even on repeat prompts.

## v0.4.5 - 2026-04-08

**Unique attestation tx_hash for repeat prompts.** Identical inputs to
`/inference/run_sharded` were producing identical attestation tx_hashes
(same nonce + same body), and the mempool was de-duping the second
submission. Now uses a monotonic `attestation_nonce: AtomicU64` on
NodeState that bumps on every sharded run, so each submission gets a
unique nonce → unique tx_hash → committed independently.

Also: `/tx/{hash}` now accepts both `0x`-prefixed and bare 64-hex forms.

## v0.4.4 - 2026-04-08

**Sharded runs now produce on-chain attestations.** Every successful
`/inference/run_sharded` call now submits an `InferenceAttestation`
transaction to the mempool, just like single-node `/inference/run` does.
The attestation includes `model_id`, `input_hash`, and `output_hash` so
anyone reading the chain can later verify a sharded run actually happened
and produced a specific output. Returns the attestation `tx_hash` and
`explorer_url` in the response.

## v0.4.3 - 2026-04-08

**CI fix.** Dropped `x86_64-apple-darwin` from the release workflow matrix
because GitHub-hosted `macos-13` runners were removed. Releases now ship
`arc-node-macos-arm64` + `arc-node-linux-x86_64`. Intel Mac users can build
from source.

## v0.4.2 - 2026-04-08

**Server-side sharded inference activity counters.** `NodeState` gains
`sharded_runs_total: AtomicU64` and `sharded_bytes_total: AtomicU64`. The
`/inference/run_sharded` handler increments both before responding. The
`/stats` endpoint exposes them as new fields. The dashboard hero shows them
as live "Runs Served" + "Bytes Forwarded" stats and aggregates them across
all 8 seed nodes (resilient to single-node restarts).

## v0.4.1 - 2026-04-08

**Sharded inference INT16 quality fix.** The shard loader was producing
INT8-only weights, which made the integer engine too noisy on Llama-7B -
every prompt collapsed to the same output tokens regardless of input.

Fix: each shard now loads weights as BOTH I8 (kept for fallback) AND I16,
where I16 is quantized directly from f32 with 258× finer granularity (32,767
levels per row vs 127). `forward_shard_token` dispatches to the I16 path
when available. The output_weight (LM head) gets the same treatment so the
final argmax sees better-quantized logits.

After this fix the network produces coherent answers like `"The largest
planet is Jupiter, which is more than 1,31..."` and `"The capital of France
is Paris."`.

## v0.4.0 - 2026-04-08

**Pipeline-parallel sharded inference shipped.** A model is now split across
N nodes at transformer layer boundaries. Each node holds a contiguous slice
of layers and forwards activations to the next shard via HTTP. BLAKE3
verifies every hidden state in transit. The first shard embeds tokens, the
last shard runs the LM head + argmax.

New endpoints:
- `POST /inference/run_sharded` - coordinator endpoint that walks the pipeline
- `POST /inference/forward_shard` - per-shard handler
- `GET /shards` - local shard registry
- `POST /shards/announce` - peers register their shards here

New CLI flags: `--shard-start <usize>` `--shard-end <usize>`. Together they
turn a node into a shard holder for the specified layer range. Without them
the node loads the full model normally.

New crate APIs:
- `arc_inference::cached_integer_model::load_cached_model_shard(path, start, end)`
- `arc_inference::cached_integer_model::CachedIntegerModel::forward_shard_token(input, cache, start, end, position)`
- `arc_inference::cached_integer_model::ShardInput::{Token, Hidden}` and `ShardOutput::{Hidden, Token}`

New scripts:
- `scripts/install-community-node.sh` - one-command community node installer with launchd / systemd persistent service + daily auto-update
- `scripts/arc-demo.sh` - end-to-end demo runner (discover pipeline → run inference → verify determinism → verify isolation)
- `scripts/arc-watchdog.sh` - testnet watchdog that preserves shard flags on restart
- `scripts/arc-health-check.sh` - network-wide health probe

New docs:
- `docs/HOW-SHARDING-WORKS.md` - engineer-grade deep dive
- `docs/SERO-DEMO.md` - 5-minute demo walkthrough
- `docs/ANNOUNCEMENT.md` - shareable summary

Dashboard: new "Sharded AI" hero section with live pipeline diagram, custom
prompt input with preset buttons, per-hop trace replay using actual wall_ms
timings, persisted run history, server-side activity counters, model_id
verification badge, and a copy-pasteable join command.

## v0.3.1 - 2026-04-07

P2P deadlock fix (try_send + broadcast write timeout), Quinn Connection
lifetime fix, commit-scan liveness fallback, observer-mode panic fix,
cross-device attestation reading from chain state, sero-quickstart.sh.

## v0.3.0 - 2026-03-29

Distributed inference parallel mode + consensus partition healing.

## v0.2.x and earlier

See git history.
