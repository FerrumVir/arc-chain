# ARC Chain - Session Guide (archived pre-recovery snapshot)

> **Do not use this file as current operator guidance.** Its live-network
> assumptions, versions, counts, and commands predate the 2026-08-26 recovery
> audit. The current tree is the unreleased v0.8.0/protocol-v3 candidate; use
> [`README.md`](README.md),
> [`docs/PRODUCTION-RECOVERY-AUDIT-2026-08-26.md`](docs/PRODUCTION-RECOVERY-AUDIT-2026-08-26.md),
> and [`docs/VALIDATOR-FLEET-ROLLOUT.md`](docs/VALIDATOR-FLEET-ROLLOUT.md).
> Nothing in the archived body authorizes a public-seed mutation.

## What This Is

ARC Chain is a Layer 1 blockchain for verifiable AI inference. The core
innovation: a pure integer inference engine that produces bitwise identical
output across ARM, x86, and GPU. That enables hash-based verification of AI
computation at O(1) cost.

**Version at snapshot:** 0.7.11 (not the current `Cargo.toml` version)
**Repo:** `FerrumVir/arc-chain` (private)
**Counts:** historical; remeasure the current checkout instead of quoting them

---

## ⚠ SAFETY RULES FOR THE LIVE NETWORK

Read these before running anything that touches a seed. Several of them exist
because the failure already happened once.

1. **Never restart a live seed.** The worker scoreboard, the
   `inference_results` map, and every `sharded_runs_total` counter live in
   process memory with no WAL entry and no snapshot. A restart destroys the
   network's entire stock of recorded inference evidence — LHR's 15 real
   sharded runs included. It also cannot recover pre-2026-06-04 Tier-1 escrows,
   which stay stuck holding funds forever.

2. **Never join the public network with stake > 0.** A peer announcing stake
   ≥ 500,000 is merged into the live `ValidatorSet` and queued into the
   consensus engine. At the next epoch boundary `freeze_epoch()` absorbs it,
   its stake is normalised to the maximum observed, and it owns 1/N of the
   leader slots on *every* seed. `PeerDisconnected` explicitly refuses to
   remove an address in the frozen set, so the damage survives until every
   seed is restarted — which rule 1 forbids. **This has already happened:**
   `genesis.toml` declares 6 validators, LAX `/validators` reports 7 at full
   stake. Always pass `--stake 0` (or `--community`).

3. **Never let a node auto-shard-join the public net.** The trigger is
   `stake > 0 && --model is set && no explicit shard range`. It POSTs
   `/shards/join` to the first seed in the seeds file (NYC), which inserts the
   announcement verbatim with no stub-address check, and — because the live
   pipeline is already fully covered — assigns `[0, 8)`, off the existing
   6-range tiling. The v0.7.9 seeds lack the mitigation, so their pipeline
   assembler then aborts with `503 Pipeline gap: expected layer 6 next, got
   shard [0, 8)`. That takes out sharded inference on the healthiest seed.
   Break the trigger with any one of: `--stake 0`, no `--model`, or an
   explicit on-grid `--shard-range`.

4. **The seeds are per-node chains. Pin one seed for any balance demo.** They
   share a DAG round but not state — `/block/43000` returns a different hash on
   each, and heights span 51 K to 135 K. A faucet credit on LAX does not appear
   on AMS. Claim, read, and re-read against exactly one host for the whole
   session, and never let anyone curl a second seed for the same address.

5. **Read-only GETs are safe. POSTs are not.** `/inference/run_sharded` mints
   an unsigned attestation under the coordinator's validator identity and
   inserts it into the mempool. `/faucet/claim` moves real testnet balance.
   Neither is appropriate as a casual probe.

6. **`ARC_TIER1_RPC` decides which seed the desktop wallet reads.** Unset, the
   desktop hard-pins `WALLET_HOSTS[0]` = LAX. Set it deliberately to the seed
   you pinned under rule 4 — it repoints balance, faucet, earnings and
   attestation reads together. Do not leave it pointing at a seed you have not
   checked. (The code comment near it claiming the seeds "form a real
   multi-validator consensus network … so reading from any one of them returns
   consistent state" is **wrong**; see rule 4.)

7. **Do not call `GET /community/list` before `/workers/scoreboard`.** The
   community-list handler prunes the registry as a side effect; the scoreboard
   handler does not. Calling it first is what empties the worker list you were
   about to show.

---

## Live network (probed read-only 2026-08-17)

| Seed | Host | Version | Height | Last block | Notes |
|---|---|---|---|---|---|
| NYC | 149.28.32.76:9090 | **0.7.2** | 135,058 | **29 s ago** | only seed reliably sealing; oldest binary |
| LAX | 140.82.16.112:9090 | 0.7.9 | 123,469 | 6.6 min ago | desktop's default wallet host |
| AMS | 136.244.109.1:9090 | 0.7.9 | 92,897 | 6.3 d ago | scoreboard empty (0/0) |
| LHR | 104.238.171.11:9090 | 0.7.9 | 51,386 | 6.7 d ago | **only seed with real attestations (15)**; EWMA-poisoned |
| NRT | 202.182.107.41:9090 | 0.7.9 | 96,726 | 6.3 d ago | |
| SGP | 149.28.153.31:9090 | 0.7.9 | 97,548 | 6.3 d ago | |

Nothing on the network runs v0.7.11 — that version exists only as a desktop
bundle. Dashboard http://140.82.16.112:3200 · wallet http://140.82.16.112:3100.

Topology: 32 layers in 6 ranges, each replicated on 3 of the 6 nodes = 18
shards, full coverage, 15–17 layers (~2.9–3.3 GB) per node.

See [`ALERTS.md`](ALERTS.md) for the current alert list.

---

## Build & Test

```bash
make ci                 # the full local gate: fmt-check, lint, test, audit
make test               # cargo test --workspace --lib --locked  (the CI gate)
make test-integration   # integration + doc tests, single-threaded
make lint               # clippy --workspace --all-targets -D warnings
make fmt-check          # what CI runs
make desktop-test       # desktop typecheck + Playwright (no arc-node needed)

cargo build --release -p arc-node       # the node binary
make eval-perplexity                    # perplexity eval (needs a GGUF)
```

Run a node locally without touching the public net:

```bash
cargo run --release -p arc-node -- --rpc 127.0.0.1:9944 --stake 0 --community-mode
```

---

## Key docs

| Doc | Why you'd open it |
|---|---|
| [`docs/DEMO-RUNBOOK.md`](docs/DEMO-RUNBOOK.md) | **Run-of-show for a live demo.** Prep, safe-join, segments, do-not-show list, failure playbook. |
| [`ALERTS.md`](ALERTS.md) | What is broken on the live network right now. |
| [`docs/TESTNET_STATE_DIVERGENCE_2026-06-03.md`](docs/TESTNET_STATE_DIVERGENCE_2026-06-03.md) | Why the seeds are not one chain. The systemic issue. |
| [`docs/INFERENCE_TIER1_INVESTIGATION_2026-06-04.md`](docs/INFERENCE_TIER1_INVESTIGATION_2026-06-04.md) | Why on-chain inference never finalizes. |
| [`INFERENCE_DETERMINISM.md`](INFERENCE_DETERMINISM.md) | The honest quality numbers (PPL ~107/~155 vs FP16 5.47). |
| [`docs/SCALE_ARCHITECTURE.md`](docs/SCALE_ARCHITECTURE.md) | Most candid internal doc; §1.2 what is actually verified, §2.6 what claims are overreach. |
| [`docs/superpowers/plans/2026-06-04-replicated-chain-model-1.md`](docs/superpowers/plans/2026-06-04-replicated-chain-model-1.md) | The unstarted fix for the divergence. |
| [`scripts/README.md`](scripts/README.md) | What each script does and what it actually produces on the live net. |

---

## Architecture Notes

- **Inference crate:** `crates/arc-inference/src/cached_integer_model.rs` — INT16 weights, matmul, forward pass
- **Node RPC:** `crates/arc-node/src/rpc.rs` — the sharded coordinator, cache, attestation construction
- **GPU crate:** `crates/arc-gpu/src/` — WGSL (`transformer.wgsl`), Metal `.metal` shaders, `metal_icb.rs`
- **Fixed-point:** Q16 format (i64 with 16 fractional bits, ONE = 65536)
- **Quantization:** Per-row symmetric. INT16 = [-32767, 32767], scale = abs_max * ONE
- **Feature flags:** `candle` (GGUF loading), `metal-icb` (native Metal dispatch), `stwo-prover` (STARK proofs, needs nightly)

---

## Known Issues

**Chain / consensus**
- Seeds are independent chains; `/block/N` differs on every seed. Repair plan written, not started.
- Block production stalled on AMS/LHR/NRT/SGP for ~6 days. `/health` still reports `ok` because DAG rounds keep advancing.
- Tier-1 on-chain inference: `/inference/onchain/submit` returns HTTP 200, the signer nonce never moves, the tx never lands in a block. UI removed in v0.7.11 rather than fixed.
- Zero-stake validators are counted in `/validators` and `/health`, inflating the displayed set by 4.
- `/economics/revenue_split` derives its worked example from the local validator count, so the same fee splits three different ways depending on which seed you ask.

**Inference**
- Poisoned latency EWMA excludes the fastest node (LHR) from routing; the value is 9–11 h stale and never resamples because exclusion prevents sampling.
- `model_hash` is BLAKE3 of the shape label `arc-32L-4096d-32h-32000v`, not of the weights. Nothing binds an attestation to actual tensors.
- Attestations are submitted unsigned with `sig_verified: true` forced.
- The VRF committee is selected and reported but never polled for votes.
- The per-hop trace samples prefill only and covers ~48% of wall time; `payload_bytes` is hardcoded to 0.
- `/models` reports `fully_covered: false` by summing replica layer spans (96) instead of taking their union. `/shards` is correct.
- Shard announcements carry `socket_addr 0.0.0.0:9090` (GH #27, open since 2026-04-16).
- INT16 quality: WikiText-2 PPL ~107 (63-token) / ~155 (256-token) vs a published FP16 baseline of 5.47. Root-caused to `I8Weights::quantize_f32` truncating the per-row `abs_max`; explicitly not a one-line fix. Prompts beginning "Explain …" reliably produce newline spam.

**Economics / display**
- `/worker/earnings` is display arithmetic: `count × 2.5 ARC`, reading no on-chain balance, so it never reconciles against `/account/{addr}`. Its source map is pruned, so lifetime earnings can *decrease* between polls and reset to 0 on restart.
- `InferenceAttestation` apply only **debits** the bond and locks it; there is no release, settle, or expiry path. The reward sign is inverted, not merely unpaid.
- "Today" earnings are fabricated as 12% of lifetime, on both the node and the desktop.
- `/inference/attestations` pads its list with unrelated transactions tagged `tx_type: "Other"` once real rows run out.

**Platform**
- `gpu.available: true` on every seed while naming `llvmpipe`, a CPU software rasterizer.
- `arc-node-linux-aarch64` has never been published in any release.
- Metal shaders in `arc-gpu` are UNTESTED on hardware (`attention`, `rope`, `silu`, `residual`, `argmax`).
- `test_channel_close_releases_funds` in arc-state was documented as a known
  failure for months, but PASSES as of 2026-08-17 (all 6 arc-state channel
  tests green). If it reddens again, bisect rather than ignore.

---

## The Stwo STARK System is REAL

The Stwo Circle STARK prover (`stwo_air.rs`) is REAL, not mock. It uses
`stwo_prover_mod::prove::<SimdBackend, Blake2sMerkleChannel>()` when the
`stwo-prover` feature is enabled. The default path (no feature) uses BLAKE3 for
fast testing. Do NOT claim Stwo is mock.
