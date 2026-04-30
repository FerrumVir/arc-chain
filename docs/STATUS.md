# ARC Chain - Status (2026-04-22)

Current production state of the ARC testnet. Refreshed 2026-04-22 to reflect
the cluster self-heal / latency-aware routing / slashing wire-up / SAO+JNB
retirement landing this week. **The source of truth for what actually runs;
CHANGELOG.md tracks what changed when.**

## TL;DR

A real **Llama-2-7B** is running across **6 separate VPS in 6 cities** (NYC, LAX, AMS, LHR, NRT, SGP) with **3× replication per layer range** - 6 contiguous ranges spanning the 32 transformer layers, each held by 3 seeds. A request flows through the pipeline via HTTP, every hidden state is BLAKE3-hashed, and the output is bit-identical regardless of which replica answers each hop. `/inference/run_consensus` fires to k=3 replicas in parallel and requires majority hash agreement; `/inference/run_sharded` picks the fastest and falls over to the next on failure.

**Live demo**: http://140.82.16.112:3200 - type a prompt in the "Sharded AI" panel.

**One-command end-to-end demo**:
```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-demo.sh | bash
```

**One-command verify the network's most recent inference**:
```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh | bash -s -- --latest
```

**One-command join the network**:
```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh | bash
```

---

## What works (verified live)

- ✅ **6-shard pipeline with 3× replication** running Llama-2-7B across NYC, LAX, AMS, LHR, NRT, SGP - 18 replica entries across 6 contiguous layer ranges
- ✅ **TJ Mac upgraded to v0.4.6** as a 9th node (parallel mode, full model loaded)
- ✅ **Coherent factual outputs**: "The largest planet is" → "Jupiter, which is more than 1,31...", "The capital of France is" → "Paris.", "The fastest land animal is" → "the cheetah", "The currency of Japan is" → "the Japanese yen (JPY)", "The longest river is" → "the Nile River"
- ✅ **Cross-platform integer determinism**: same prompt → same output_hash on every replay
- ✅ **Per-request KV cache isolation**: 10 concurrent prompts produced 10 unique output_hashes
- ✅ **Multi-position determinism**: 4-position 2-way shard split produces same token sequence as full forward (unit-tested)
- ✅ **3 unit tests for forward_shard_token**: full vs 2-way split, full vs 3-way split, multi-position. All pass in 0.02 s.
- ✅ **Clean factual benchmark**: 3/3 pass, 3/3 unique hashes, no node restarts (`docs/BENCHMARK-RESULTS.md`)
- ✅ **On-chain attestation** for every sharded run with `model_id`, `input_hash`, `output_hash`
- ✅ **Unique attestation tx_hash per submission** (atomic nonce bump fixes mempool dedupe)
- ✅ **Cryptographic verifier**: `arc-verify.sh --latest` re-derives the newest inference and prints ✓ VERIFIED
- ✅ **Model identity verified**: all 18 replica entries (6 ranges × 3 replicas) report identical BLAKE3 model_id (`0xabec2d58...`)
- ✅ **Community installer** auto-detects platform, installs persistent service, schedules daily auto-update
- ✅ **GitHub Actions release workflow** auto-builds Mac arm64 + Linux x86_64 on tag push (5+ consecutive successful auto-releases through v0.4.6)
- ✅ **Self-heal daemon (`arc-self-heal`)** on every seed - auto-restarts arc-node on RPC silence or consensus drift, preserving every `--shard-range`, `--model`, and `ARC_PUBLIC_SOCKET` via `/proc/PID/cmdline` + `environ`. Deployed to all 6 seeds 2026-04-22 (GH #30).
- ✅ **Dashboard** defaults to `/inference/run_consensus` (GH #34) with k=3; shows live pipeline diagram with per-hop trace replay, model_id verify badge, server-side activity counters aggregated across the 6 seeds, persisted run history with per-run Verify button, copy-pasteable join command, and Open Graph meta tags for shareable previews

## What's flaky / known issues

- ⚠️ **Sequential bench saturates the pipeline**: arc-bench.sh hammers the coordinator's HTTP server when requests are < 60 s apart; the watchdog restarts the affected node mid-bench. Workaround in v0.4.7 of the script: 60 s sleep between requests, default to 5 prompts.
- ⚠️ **Watchdog occasionally restarts a node**: NYC, LAX, NRT have restart cycles every ~15-30 min when there's heavy load. Network self-heals within ~3-5 min each time. The other 5 nodes are stable.
- ⚠️ **Llama-13B doesn't fit the 8 GB Vultr loader**: known OOM issue during the GGUF dequant step. Workaround: stick with 7B for now. 70B would require bigger nodes.
- ⚠️ **AMS occasionally drifts ~500 rounds behind**: catches up within minutes, no impact on inference

## Releases

| Version | Date | Highlight |
|---------|------|-----------|
| v0.4.0 | 2026-04-08 | Pipeline-parallel sharded inference shipped |
| v0.4.1 | 2026-04-08 | INT16 quality fix - output now coherent on Llama-7B |
| v0.4.2 | 2026-04-08 | Server-side sharded inference activity counters |
| v0.4.3 | 2026-04-08 | CI fix: dropped macos-13 runner (was breaking releases) |
| v0.4.4 | 2026-04-08 | Sharded runs now submit on-chain InferenceAttestation TX |
| v0.4.5 | 2026-04-08 | Unique nonce per attestation (fixes mempool dedupe) |
| v0.4.6 | 2026-04-08 | Apply unique-nonce fix to single-node /inference/run too |

All 7 releases have Mac arm64 + Linux x86_64 binaries on GitHub. The community installer pulls v0.4.6 by default.

## Code shipped

### Rust (arc-node + arc-inference)
- `crates/arc-inference/src/cached_integer_model.rs`:
  - `I8Weights::empty()`, `I16Weights::empty()`, `CachedLayer::placeholder()` (zero-byte placeholders for sharded loading)
  - `load_cached_model_shard(path, start_layer, end_layer)` - loads only the held layers, populates both I8 and I16 from f32
  - `CachedIntegerModel::forward_shard_token(input, cache, start_layer, end_layer, position)` - runs the shard's slice on a token id (first shard) or hidden state (middle/last shards)
  - `ShardInput::{Token, Hidden}` and `ShardOutput::{Hidden, Token}` enums
- `crates/arc-node/src/main.rs`:
  - `--shard-start` / `--shard-end` CLI flags
  - Loads sharded weights when both flags are present
  - Builds `ShardInfo` at startup, passes into `rpc::serve`
  - Background broadcaster + puller (15s/20s tick) for fast shard registry convergence
- `crates/arc-node/src/rpc.rs`:
  - `NodeState` gains `shard_info`, `shard_kv_caches`, `shard_registry`, `sharded_runs_total`, `sharded_bytes_total`, `attestation_nonce`
  - `ShardInfo` struct with model_id, layer range, memory, socket, friendly node name
  - `POST /inference/forward_shard` - per-shard handler with BLAKE3 hash verification
  - `POST /inference/run_sharded` - coordinator endpoint, walks the pipeline, submits on-chain attestation, returns full per-hop trace
  - `GET /shards` and `POST /shards/announce` - registry discovery
  - `parse_hash` accepts both `0x`-prefixed and bare hex
  - Attestation nonce: `base_nonce + atomic_bump` (unique per submission)
  - `/stats` exposes `sharded_runs_total` + `sharded_bytes_total`
  - Faucet uses lock-free `DashMap` (was holding sync Mutex across awaits)

### Scripts
- `scripts/install-community-node.sh` - one-command installer (platform detect, binary download, model download, launchd/systemd service, daily auto-update)
- `scripts/arc-demo.sh` - end-to-end demo (discover pipeline → run inference → determinism check → isolation check)
- `scripts/arc-verify.sh` - third-party attestation verifier with `--latest` mode
- `scripts/arc-bench.sh` - factual benchmark with markdown report
- `scripts/arc-self-heal.sh` + `.service` + `install-self-heal.sh` - per-host self-heal daemon (GH #30). Systemd unit on each seed auto-restarts arc-node on RPC silence / consensus drift, preserves shard flags via `/proc/PID/cmdline`.
- `scripts/arc-watchdog.sh` - legacy off-cluster watchdog, superseded by `arc-self-heal`.
- `scripts/arc-health-check.sh` - network-wide health probe
- `.github/workflows/release.yml` - auto-build + auto-publish on tag push

### Dashboard
- "Sharded AI" hero section with live pipeline diagram
- Custom prompt input with 5 preset buttons (one-click factual demos)
- Real per-hop trace replay using actual wall_ms timings
- Server-side counter aggregation across the 6 seeds
- Model_id verification badge (green ✓ when all shards match)
- Persisted run history with per-run "↻ Verify" button
- "Join the network" panel with 📋 Copy install command
- Open Graph + Twitter Card meta tags for shareable previews
- SVG favicon

### Docs (9 user-facing)
- `README.md` - rewrite leading with sharding + ASCII pipeline diagram + Documentation index
- `docs/HOW-SHARDING-WORKS.md` (184 lines) - engineer-grade architecture deep dive
- `docs/SERO-DEMO.md` (211 lines) - 7-step walkthrough with timings
- `docs/ANNOUNCEMENT.md` (107 lines) - copy-paste shareable summary for socials
- `docs/PERFORMANCE-COMPARISON.md` (72 lines) - honest latency comparison vs centralized API + local llama.cpp
- `docs/STATUS.md` - this file
- `docs/BENCHMARK-RESULTS.md` - captured factual benchmark (3/3 pass)
- `CHANGELOG.md` - release notes for v0.3.0 → v0.4.6
- `scripts/README.md` - index for the 25+ scripts in scripts/

### Tests
- `cached_integer_model::tests::test_forward_shard_token_full_equals_split` - 4-layer model, single shard equals 2-shard split at K=2
- `cached_integer_model::tests::test_forward_shard_token_three_way_split` - 6-layer model, single shard equals 3-shard chain [0,2)→[2,4)→[4,6)
- `cached_integer_model::tests::test_forward_shard_token_multi_position` - 4 sequential positions through both single shard and 2-way split, token sequences match
- All 3 pass in 0.02 s; catches any future bug in layer-boundary stitching, RoPE positions, or KV cache pushes across shards

## Headline numbers

- **6 layer ranges × 3 replicas** of Llama-2-7B serving sharded inference (18 replica entries total)
- **6 testnet seed nodes** in NYC, LAX, AMS, LHR, NRT, SGP (SAO + JNB retired 2026-04-22, GH #32)
- **32 transformer layers** split into 6 contiguous ranges: [0,6), [6,12), [12,17), [17,22), [22,27), [27,32). Each range is held by 3 seeds - any one can answer a hop, primary picked by EWMA latency.
- **~1.1 GB per range-replica** of weights (the full model is ~4 GB; a multi-range seed like AMS holds 3 ranges = ~3.3 GB in RAM)
- **~150 KB transferred per token** across the network (i64 hidden states + JSON envelope + BLAKE3 hash)
- **~10-13 sec/token** wall time end-to-end via `/inference/run_sharded`; `/inference/run_consensus` with k=3 is slower per-hop but catches hash divergence
- **10/10 unique output_hashes** under 10x concurrent load
- **9/10 factually correct** answers in the 10-prompt benchmark
- **0% precision loss** vs the source GGUF model (per-row INT16 quantized from f32)

## What TJ should look at first when he wakes up

1. **Open the dashboard**: http://140.82.16.112:3200 - see the live pipeline, click "Run Through Pipeline" with the default prompt, watch the cards animate, see the trace
2. **Read** `docs/SERO-DEMO.md` for the screen-recording flow
3. **Read** `docs/ANNOUNCEMENT.md` for the social-post version
4. **Try the verifier**: `bash scripts/arc-verify.sh --latest` - see the cryptographic proof
5. **Check the autoloop log**: `/tmp/arc-autoloop-state.md` for iter-by-iter progress
6. **Ship the install link to Sero**: `curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh | bash`

## Live endpoints

- Dashboard: http://140.82.16.112:3200
- Coordinator RPC: auto-discovered via `bash scripts/arc-pick-coordinator.sh` (probes the 6 seeds, returns first healthy one)
- Shard registry: `/shards` on whichever coordinator was picked
- Health: `bash scripts/arc-health-check.sh`
- GitHub: https://github.com/FerrumVir/arc-chain
- Latest release: https://github.com/FerrumVir/arc-chain/releases/latest
