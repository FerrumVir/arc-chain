![Rust](https://img.shields.io/badge/Rust-121K%2B_LOC-orange)
![Tests](https://img.shields.io/badge/tests-1%2C363_defined-brightgreen)
![License](https://img.shields.io/badge/license-BUSL--1.1-blue)
![Inference](https://img.shields.io/badge/inference-CPU_KAT--verified-purple)
![Testnet](https://img.shields.io/badge/public_fleet-forked-red)

# ARC Chain - Trustworthy AI

**A Rust Layer 1 recovery candidate designed to make AI inference
reproducible, independently recomputed, and explicitly authorized before a
community reward can settle.**

**Not a fork. Not a copy. Every line is original.**

## v0.8.0 release status and quickstart

**v0.8.0 / protocol v3 is not published or deployed yet.** The moving GitHub
`latest` release is still the desktop-only v0.7.11 bundle, so this README does
not use it as an install source. After the complete
[exact v0.8.0 release](https://github.com/FerrumVir/arc-chain/releases/tag/v0.8.0)
shows every required asset and `SHA256SUMS`, an SSH/EC2/VPS operator can run:

```bash
curl -fsSLO https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/install.sh
bash install.sh --version 0.8.0
```

The unified release contract restores headless Linux amd64 and arm64, Intel
and Apple Silicon macOS, Windows CLI binaries, signed desktop-updater payloads,
normalized desktop installers, and one checksummed installer. Until the exact
release exists, build and test only from a reviewed checkout; do not treat the
commands above as a production download. The public fleet still requires the
[coordinated recovery/cutover gate](docs/VALIDATOR-FLEET-ROLLOUT.md).

📄 Paper: *On the Foundations of Trustworthy Artificial Intelligence*

---

## The claim

Every AI response from a cloud provider is a claim you can't check. You don't know which model ran. You don't know whether the output was truncated, cached, routed, or silently modified. You trust the logo.

ARC's release candidate makes inference reproducible enough for validators to
recompute and compare a 32-byte commitment. The production CPU engine uses
integer arithmetic, and a hardcoded synthetic-model KAT now proves its I8/I16
whole-model and three-way-shard paths on ARM and x86. That test does **not** yet
cover GPU backends or a production 7B GGUF. The new community path rejects a
worker result unless the coordinator obtains a 2-of-3 authenticated quorum for
every range and token position; automatic slashing is not wired. The current
public seeds do not run this release candidate yet.

The design goal is AI that can pass consensus. The current candidate proves a
bounded CPU path and fails closed when exact-model, recomputation, validator
authorization, or chain-readiness evidence is missing. It is not yet a claim of
trustless inference at arbitrary scale or on every hardware backend.

---

## Why this doesn't exist anywhere else

Five things, in one runtime. Every row is checkable, and the commands are in
[`docs/RECEIPTS.md`](docs/RECEIPTS.md).

| | Everywhere else | Here |
|---|---|---|
| **Verifying an AI answer** | Trust the API, or pay orders of magnitude more to prove it in zero-knowledge | Re-run it and compare one 32-byte hash — the cost of a single forward pass |
| **Same answer on different chips** | Floating-point execution can drift | Blocking CPU I8/I16 KAT on ARM and x86; GPU and full-GGUF vectors still required |
| **Inference verification before community reward** | Trust a worker or external oracle | Coordinator recomputes through authenticated 2-of-3 range quorums; mismatches are rejected, not auto-slashed |
| **Post-quantum signatures** | Common chains still center classical signatures | Current source includes ML-DSA-65 and Falcon-512 transaction verification; public-fleet deployment is not claimed |
| **Post-quantum verify inside a contract** | Common contract runtimes do not expose Falcon verification | Current source defines `falcon512_verify` precompile `0x08`; public-fleet deployment is not claimed |

**I don't know of another chain that has all five.** If you know one, open an
issue and I'll put it in this table myself.

And a result that surprised me: **the post-quantum signature verifies faster
than the classical one it replaces.** Falcon-512 at 20.9 µs against Ed25519's
30.1 µs, through the same code path the mempool uses. Everyone assumes
quantum-safe means slow and heavy. On Apple silicon it's the opposite.

```bash
cargo run --release -p arc-crypto --example pq_bench
```

### On the zero-knowledge side, to be straight with you

The Circle STARK prover here is **StarkWare's Stwo** — the best prover in the
world, and I use it on purpose. What's mine is the circuit built on top of it:
an AIR that proves a Llama-2-7B dense layer and actually *binds* the result.
A full 4096 × 4096 attention projection — 16.7 million multiply-accumulates,
a 2²⁴-row trace — proves as a single STARK in 30 seconds on a desktop.

The interesting part isn't that it proves. It's that it refuses:

```bash
cargo run --release --example soundness_check --features stwo-prover
```

Four forged outputs, all rejected. My first version of that circuit had four
constraints, two of which did nothing, and it would have signed off on a fake
answer. I found it, fixed it, and left the test in so nobody has to take my
word for it.

**Lagrange's DeepProve is ahead of me on zkML and it isn't close** — they prove
full LLM inference in production. I went the other direction: make the
computation reproducible so you don't need the expensive proof. Different
trade, not a better prover.

---

## Public fleet snapshot

**Read-only public-fleet snapshot, 2026-08-26 around 15:06 CDT.** These are
observations, not a standing uptime promise:

| Seed | Version | State height at snapshot | Latest-block observation |
|---|---:|---:|---|
| NYC | 0.7.2 | ~136,969 | ~157 seconds old |
| LAX | 0.7.9 | ~127,188 | ~1,050 seconds old |
| AMS | 0.7.9 | ~88,452 | ~125 seconds old |
| LHR | 0.7.9 | ~51,422 | ~4.2 days old |
| NRT | 0.7.9 | ~96,770 | ~4.2 days old |
| SGP | 0.7.9 | ~97,591 | ~4.2 days old |

At common height 50,000, all six reachable seeds returned **six different
block hashes and six different state roots**. The dashboard independently
repeated the comparison at the then-highest common height, 51,422, with the
same 6/6 divergence. Therefore the public fleet is not one replicated chain.
Stop reward issuance, pin one source for diagnosis, and choose an approved
canonical recovery state before any rollout. An advancing DAG round or
`status: ok` does not override this result.

The same snapshot found community `total_work_completed: 0` across the worker
list. No public inference job, validator-authorized `0x25` reward, or mined
community payment was demonstrated. A raw `InferenceAttestation` (`0x16`) is a
computation claim and pays nothing.

Version skew is real too: NYC runs v0.7.2, the other five run v0.7.9, and
**nothing on the network runs v0.7.11** — that version exists only as a desktop
bundle. See [`ALERTS.md`](ALERTS.md) for the current alert list.

The concise evidence record is
[`docs/PRODUCTION-RECOVERY-AUDIT-2026-08-26.md`](docs/PRODUCTION-RECOVERY-AUDIT-2026-08-26.md).

There is also a trust-root incident: legacy production validator seed material
was published in repository history. Those six identities must be replaced;
deleting the strings from the current tree does not make them safe. The v3
candidate requires mode-`0600` Ed25519 keyfiles and a complete public-address
genesis, and intentionally refuses staked production startup until operators
approve a new genesis/checkpoint and coordinated quorum cutover. Rewards remain
off during that migration.

---

## Test it yourself

### Desktop GUI (requires a screen)

| Supported desktop | Normalized v0.8.0 asset (valid after publication) |
|---|---|
| **macOS 11+ — Apple Silicon** | [DMG](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-macos-arm64.dmg) |
| **macOS 11+ — Intel** | [DMG](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-macos-x86_64.dmg) |
| **Windows 10/11 — x86_64** | [Installer](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-windows-x86_64-setup.exe) |
| **Linux desktop — x86_64** | [AppImage](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-linux-x86_64.AppImage) · [.deb](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-linux-x86_64.deb) · [.rpm](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-linux-x86_64.rpm) |

These stable names are generated by the unified v0.8.0 release pipeline;
until that exact release is published, the links intentionally do not resolve.
The GUI is not a server binary: it needs a
graphical session. An EC2/VPS/SSH-only machine should use the headless installer
below. Linux ARM64 is also headless-only.

The public v0.7.11 desktop bundled updater configuration but did not invoke the
update lifecycle, so it must not be described as auto-updating. The v0.8.0
candidate now checks the signed manifest shortly after startup and every 24
hours when the setting is enabled. Background checks do not download or install
anything; the user confirms installation. macOS, Windows, and Linux AppImage
can then update in place. `.deb` and `.rpm` remain owned by their package
managers and must be upgraded by installing the new package.

**📖 Desktop controls:** [Getting Started with ARC Node](docs/GETTING_STARTED.md)
— release gates, identity, inference evidence, faucet, mined reward receipts,
and FAQ.

---

### Headless / server node (no GUI or display required)

The supported headless assets are Linux x86_64/amd64 and ARM64, macOS Apple
Silicon and Intel, and Windows x86_64. The installer supports Linux and macOS;
Windows Server operators download the two `.exe` assets and `SHA256SUMS`
manually from the exact v0.8.0 release.

The following recovery command is intentionally pinned and becomes valid only
after GitHub shows the complete `v0.8.0` release. The moving `latest` alias is
never used by the initial install command.

```bash
curl -fsSLO https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/install.sh
bash install.sh --version 0.8.0
```

The installer uses an exact release tag after resolution, verifies `arc-node`,
`arc-cli`, seeds, and genesis against that release's `SHA256SUMS`, and refuses
missing assets, unknown versions, and downgrades. On Linux it installs a systemd
system service when run as root and a systemd user service otherwise; on macOS
it installs a LaunchAgent. It preserves the private node identity across
upgrades and never places it in the process command line. Managed stake-zero
nodes bind RPC to `127.0.0.1` only; `--port` changes the local port, not the
interface, so a permissive EC2 security group cannot expose RPC accidentally.

v0.8.0 writes `genesis.network-hash` into fresh persisted state and fails
closed when an existing WAL has no marker or its hash differs from the selected
genesis. Do not reuse a v0.7.11-or-earlier data directory. Back up an observer's
identity and old data for forensics, then select a fresh `--data-dir`; validators
require the approved canonical checkpoint migration. On a failed install or
update, the installer restores every managed binary, network file, runner,
config, identity file, service unit, and the prior service/timer state. That
rollback is not a migration and never rewrites the model or chain data.

Useful server options:

```bash
# SSH-only Ubuntu server, custom RPC/P2P ports and data volume
bash install.sh --version 0.8.0 \
  --port 19090 --p2p-port 19091 --data-dir /srv/arc-data

# Install binaries/config only; print the command but do not start anything
bash install.sh --version 0.8.0 --no-service --no-auto-update

# Serve local inference (a model is optional and never passed as an empty arg)
bash install.sh --version 0.8.0 --model /absolute/path/to/model.gguf

# Reproducible pin; an older version is rejected if a newer one is installed
bash install.sh --version 0.8.0
```

For an install that kept the scheduled updater, the manual commands are:

```bash
# Linux user service or macOS LaunchAgent
"$HOME/.arc/bin/arc-installer" --update-only --install-dir "$HOME/.arc"

# Linux system service
sudo /var/lib/arc-chain/bin/arc-installer --update-only --install-dir /var/lib/arc-chain --system-service
```

Update mode intentionally resolves the newest complete release, verifies it,
and refuses equality or downgrade; do not add `--version 0.8.0` when the goal
is to discover a later safe update.

Without `--model`, the node is an observer/router and will not execute local
model inference. It still joins with `--stake 0 --community-mode`; stake-zero
is the safe community posture, but rewards and work assignment are determined
by the network and are not guaranteed by the installer. See
[`docs/HEADLESS_INSTALL.md`](docs/HEADLESS_INSTALL.md) for service commands,
firewall notes, upgrade behavior, and Windows verification. The short operator
demo is [`docs/COMMUNITY-NODE-WALKTHROUGH.md`](docs/COMMUNITY-NODE-WALKTHROUGH.md).

**📖 Desktop walkthrough:** [Getting Started with ARC Node](docs/GETTING_STARTED.md).

---

### Command-line network demos

**Inspect a public inference response, its trace, and the evidence the selected
coordinator actually returns:**

```bash
ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-demo.sh
```

Run this only against a controlled local or reviewed recovery-candidate
coordinator. Automatic public-fleet discovery is disabled because the public v2
seeds have mixed versions and divergent state. The script inspects the selected
pipeline, dispatches a prompt, prints its trace, and asks for recomputation.

A word on that re-run, because it is the point of the demo. The coordinator
caches by (model, prompt, max_tokens), so simply POSTing the same prompt twice
is answered out of cache and proves nothing. The script sends
`force_recompute`: a supporting candidate reports `✓ DETERMINISTIC` after two
pipeline walks; a cache response is labeled `● SERVED FROM CACHE (hash match)`.

**Attempt to recompute a past public inference claim on your own machine:**

```bash
ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-verify.sh --latest
```

The historical script sweeps seeds for an inference record, replays its prompt,
and compares reported commitments. Its `VERIFIED` label means only that those
reported hashes matched. A cache response is not recomputation, and the public
v2 model ID below does not bind the weight bytes, so this is not exact-artifact
proof.

On today's public v2 seeds, `model_hash` is still a BLAKE3 of the model's
shape label (`arc-32L-4096d-32h-32000v`), not of the weight bytes. It proves
the same declared shape, not the same tensors. The unpublished v0.8.0/v3
candidate instead streams the complete `--model` artifact through BLAKE3 and
uses that exact byte commitment for shard routing, worker eligibility, caches,
attestations, and verification. Do not read that candidate behavior as already
deployed on the public fleet.

---

## The improvements that made this real

The core thesis - "inference that passes consensus" - only works if the
arithmetic is perfectly reproducible. The list below separates mechanisms from
their evidence; it is not a claim that every item is deployed on today's
version-skewed public fleet:

1. **Integer transformer path.** The candidate's production CPU I8/I16 path
   uses fixed-point kernels for its covered transformer operations. The
   blocking KAT is the evidence boundary; it does not establish that every
   optional backend or production model is float-free.

2. **Cross-architecture CPU KAT.** A committed synthetic model now produces the
   same reviewed token, logits, KV-cache, hidden-state, and output hashes on
   Apple arm64 and x86_64. The blocking workflow covers Linux, Windows, Apple
   Silicon, and Intel macOS. This is not yet a GPU or full-Llama-2-7B proof; see
   [`INFERENCE_DETERMINISM.md`](INFERENCE_DETERMINISM.md).

3. **Constant-size commitment comparison.** Comparing two BLAKE3 commitments is
   constant size, but obtaining independent evidence still requires another
   forward pass. A matching hash is useful only when the verifier really
   recomputed the exact artifact and input.

4. **Sharded inference with transit integrity.** The historical public layout
   splits 32 layers into six ranges with three replicas each. Each request binds
   the received hidden-state bytes to a BLAKE3 digest, which detects accidental
   or in-transit modification. A malicious shard can hash its own wrong output;
   authenticated independent recomputation—not the transit hash—is what rejects
   a bad result in the candidate.

5. **Pipelined prefill across shards.** Prompt prefill runs one task per shard joined by channels, so the node holding layers 6–12 works on position *p* while the node holding 0–6 is already on *p+1*. The per-token decode loop that follows is necessarily sequential — each token depends on the previous token's logits — so a long prompt pipelines well and a long generation does not.

6. **Latency-aware replica selection.** Each layer range has 3 replicas; the coordinator keeps a rolling EWMA of per-hop latency and dispatches to the fastest. Because the engine is deterministic, the output is identical whichever replica answers, so this is a free speed knob. (Racing the top-K in parallel rather than picking one is designed but not shipped - see the roadmap.)

7. **Deterministic result cache, content-addressed.** Integer-only means identical inputs produce identical outputs, so results are addressable by (model, prompt, length) and a repeat serves in microseconds. Worth being precise about what that does and does not show: a cache hit is not evidence that the pipeline recomputed. `force_recompute` exists to get a real second walk.

8. **Legacy VRF committee metadata.** The source can select and record a
   deterministic committee for the older inference gas lane, but that path
   does not collect votes or auto-slash. Treat a `committee` field as metadata,
   not verification. Community reward `0x25` uses a separate strict active-set
   authorization contract. The v3 candidate collects independent approvals
   from the explicit HTTPS validator origins and requires five of six; the
   checked-in observer genesis keeps issuance disabled until the coordinated
   rollout supplies an approved activation and validator set.

9. **Post-quantum signature code paths.** Falcon-512 and ML-DSA exist alongside
   Ed25519, BLS12-381, and secp256k1 in the current source tree. This is not a
   claim that the divergent public fleet runs one coordinated release of them.

10. **DashMap lock-inversion repair in `index_account_tx`.** The source no
    longer holds one shard write lock while acquiring another. That removes one
    identified deadlock; it is not evidence that today's forked public fleet is
    healthy.

---

## Measured performance

The values below are historical lab or public-path observations, not the
v0.8.0 release gate and not an earnings promise. Re-run the linked harness on
the exact commit, model artifact, backend, and hardware before quoting one.
The blocking candidate evidence currently covers CPU ARM/x86 KATs; it does not
validate GPU determinism or a production 7B GGUF.

**Read this row first, because it is the one people get wrong.** There are two
very different latency stories here and the millisecond numbers below are the
*local single-node* one.

| Where | Latency | What it is |
|---|---|---|
| **One node, whole model in memory, M2 Ultra** | **76–139 ms/token** | the numbers in the table below |
| **Sharded across 6 public v2 seeds (historical snapshot)** | **~2–10 s/token** | dated observation; not current recovery-candidate evidence |

In that historical public-v2 snapshot, a 16-token response took roughly **1–3
minutes**, not milliseconds. Those measurements do not establish current fleet
health or candidate performance and should not be used as a live demo promise.

All numbers below on Apple M2 Ultra (24 cores, 64 GB) unless noted, single node.

| Metric | Value | Conditions |
|---|---|---|
| Historical integer GPU path | 76 ms/token | single-node lab measurement; outside current determinism gate |
| Historical integer CPU path | 139 ms/token | single-node lab measurement; rerun required on candidate |
| Standard float (Candle Q4) | 175 ms/token | Not deterministic |
| Single-node peak TPS | 183,000 | CPU verify + sequential exec |
| Multi-node sustained TPS | 33,230 | 2 validators, real QUIC, real DAG |
| Peak TPS | 350,000 | 1-second burst window |
| Commit rate | 100% | 500 K / 500 K transactions |
| State lookups | 22.3 M/sec | DashMap baseline |
| GPU Ed25519 verify | 379,000 / sec | Metal compute shader (13.68× CPU) |
| Ed25519 signing | 82,800 / sec | Single-core |
| DAG finality | ~24 ms | 2-round commit rule |

The historical lab run measured the integer path faster than its float control.
That result is workload- and backend-specific and does not establish universal
GPU speed or cross-GPU bit identity.

---

## How the sharding works

```
                  Llama-2-7B - 32 transformer layers, 6 seed nodes,
                    3× replication per layer range

  token id  →  [0,6)  →  [6,12)  →  [12,17)  →  [17,22)  →  [22,27)  →  [27,32)  →  token id
                EMBED                                                      LM HEAD

  range           replicas (any one answers, failover to the next)
  ─────           ────────────────────────────────────────────────
  [0,6)           AMS · LAX · NYC
  [6,12)          AMS · LAX · LHR
  [12,17)         AMS · LHR · NRT
  [17,22)         LHR · NRT · SGP
  [22,27)         NRT · NYC · SGP
  [27,32)         LAX · NYC · SGP

  NYC 149.28.32.76   LAX 140.82.16.112   AMS 136.244.109.1
  LHR 104.238.171.11 NRT 202.182.107.41  SGP 149.28.153.31   (port 9090)
```

Each `→` is a `POST /inference/forward_shard` to the next shard. Each shard verifies the previous shard's BLAKE3 hash before computing. The last shard runs `final_norm + LM head + argmax` and returns the next token id. The coordinator collects tokens until `max_tokens` or EOS.

Coordinators batch the whole prompt into one round-trip per shard (`"prefill":"batch"`) and pick the lowest-latency replica for each range. Racing several replicas at once and taking the first to finish is designed but not shipped.

Each node holds 15–17 of the 32 layers, about 2.9–3.3 GB. Verify the live map yourself: `curl http://104.238.171.11:9090/shards`.

---

## Architecture

```
Users / AI Agents
       │
       ▼
┌─ arc-net ────────────────────────────────────────────────┐
│  QUIC transport (quinn 0.11), TLS 1.3, shred propagation, │
│  XOR FEC, TX gossip, peer exchange (PEX)                  │
└──────────────────────┬───────────────────────────────────┘
                       ▼
┌─ arc-consensus ──────────────────────────────────────┐
│  DAG block proposals (Mysticeti-inspired),            │
│  stake-weighted 2-round finality, VRF proposer select │
└──────────────────────┬───────────────────────────────┘
                       ▼
┌─ arc-node ───────────────────────────────────────────┐
│  Block production, 34-endpoint RPC + ETH JSON-RPC,    │
│  sharded inference coordinator, consensus manager     │
└──────┬────────────────────────┬──────────────────────┘
       ▼                        ▼
┌─ arc-state ──────────┐ ┌─ arc-vm ──────────────────┐
│  DashMap + JMT        │ │  Wasmer 6.0 WASM runtime   │
│  GPU-resident cache   │ │  revm 19 EVM interpreter    │
│  BlockSTM parallel    │ │  Gas metering, precompiles  │
│  WAL persistence      │ └─────────────────────────────┘
└───────────────────────┘
       │
┌─ arc-inference ──────────────┐ ┌─ arc-olm ────────────────┐
│  Pure-integer INT8/INT16     │ │  On-chain LM runtime,     │
│  transformer engine,         │ │  INT16 deterministic      │
│  committee selection,        │ │  inference                │
│  distributed dispatch        │ └───────────────────────────┘
└──────────────────────────────┘
       │
┌─ arc-gpu ──────────────────┐
│  Metal/WGSL Ed25519 batch   │
│  GPU state cache (wgpu)     │
│  Unified memory             │
└─────────────────────────────┘
```

---

## Codebase

~121,900 lines of Rust across 16 workspace members (14 under `crates/`, plus
`agents/` and `relayer/`). More than 1,300 Rust tests are defined. Run the
complete release gate from the repository root:

```bash
./scripts/ci_check.sh             # full release/security/integration/UI suite
./scripts/ci_check.sh --quick     # shorter edit loop
```

The full command covers release and installer contracts, a releasable-worktree
secret scan, ShellCheck, workflow syntax, rustfmt, Clippy, every workspace
target, unit/integration/doc tests, multi-node scenarios, dashboard/explorer
contracts (including reproducible compiled dashboard CSS), deterministic
desktop TypeScript/Playwright/Tauri tests, and a clean build plus packed-install
smoke of the supported TypeScript SDK. CI scans the
exact checked-out commit. Node.js 24 LTS (see `.node-version`) is required locally.
Failures retain complete logs under `target/ci-check/`.
The local shell harness targets macOS/Linux POSIX hosts; Windows-specific SDK,
desktop, and packaging behavior is enforced by the blocking Windows CI legs.

| Crate | LOC | What it does |
|---|---|---|
| `arc-types` | 14,490 | 24 transaction types, blocks, accounts, governance, staking, bridge, inference attestation/challenge |
| `arc-state` | 13,203 | DashMap state DB, Jellyfish Merkle Tree, WAL, BlockSTM parallel execution, GPU-resident cache |
| `arc-crypto` | 11,680 | Ed25519, secp256k1, BLS12-381, BLAKE3, Falcon-512, ML-DSA, VRF, Stwo STARK prover |
| `arc-olm` | 9,760 | On-chain language model runtime, INT16 deterministic inference |
| `arc-vm` | 8,439 | Wasmer WASM + revm EVM, gas metering, 11 precompiles, AI inference oracle |
| `arc-node` | 8,424 | Block production, 34-endpoint RPC, sharded inference coordinator |
| `arc-inference` | 8,343 | Pure-integer engine, committee selection, distributed dispatch |
| `arc-consensus` | 7,971 | DAG consensus, 2-round finality, slashing, VRF, epoch transitions |
| `arc-gpu` | 5,250 | Metal MSL + WGSL Ed25519 batch verify (379 K / sec), GPU memory |
| `arc-net` | 2,355 | QUIC transport, shred propagation, FEC, gossip, peer exchange |
| `arc-mempool` | 876 | Lock-free queue, deduplication, BLS threshold encrypted mempool |
| `arc-cli`, `arc-channel`, `arc-bench`, `arc-relayer`, `arc-agents` | misc | CLI, payment channels, benchmarks, bridge, example agents |

Plus: Python SDK (2,688 LOC), TypeScript SDK (2,011 LOC), Solidity contracts
(1,944 LOC), and a dependency-free static block explorer.

---

## What exists in the current release candidate

This table describes the current source tree. The public testnet is still on
v0.7.2/v0.7.9 and must pass the coordinated rollout gate before these rows can
be described as deployed together.

| | |
|---|---|
| DAG consensus, 2-round commit | implemented in source; v3 trusted-set cutover not performed |
| Self-heal daemon | scripts and service units exist; does not repair a forked trust root |
| Deterministic CPU I8/I16 inference | ✅ hardcoded ARM/x86 KAT; GPU/full-GGUF unverified |
| Sharded inference, 3× range replication, transit BLAKE3 | candidate endpoints implemented; not deployed to the public fleet |
| Authenticated range recomputation | candidate requires 2-of-3 for every range/token; not deployed |
| Latency-aware replica selection per layer range | ✅ rolling EWMA |
| Auto-shard node onboarding | ✅ `--auto-shard` flag |
| Inference computation claims | tx `0x16`; never a payment |
| Community reward settlement | tx `0x25`, five-of-six active-validator identity + stake approvals; implemented and receipt-gated, but disabled by the checked-in observer genesis and not deployed |
| EVM (Solidity) + WASM (Rust / C / Go) both | ✅ revm 19, Wasmer 6.0 |
| 5 signature algorithms incl. 2 post-quantum | ✅ Ed25519 · Falcon-512 · BLS · ML-DSA · secp256k1 |
| BLS threshold encrypted mempool (MEV protection) | ✅ commit-reveal |
| Zero-fee agent settlements | ✅ `Settle` (0x06) · `RegisterAgent` (0x07) |
| Wallet and dashboard UIs | public diagnostics exist; corrected candidate UI not yet deployed |

### Built but not yet doing its job

Listed separately because the code exists and the endpoint answers, but the
thing you would assume from the name is not happening yet:

| | |
|---|---|
| Validator slashing (equivocation, liveness) | implemented in `arc-consensus`; no slash has been triggered on the live net |
| VRF committee re-execution | committee is selected and recorded; votes are never collected |
| Public inference claims reaching blocks | host-dependent on the forked fleet; a mined `0x16` still pays nothing |
| Exact model identity | public v2 `model_hash` is shape-derived; the unpublished v0.8.0/v3 candidate commits to every byte of the source model artifact |

### Roadmap — designed, not shipped

Previous versions of this README listed these as live. They are not; the
endpoints return 404 on the current binary.

| | |
|---|---|
| Content-addressed model chunks (no GGUF download) | `/chunks/get/{hash}` — planned |
| Heterogeneous hardware scheduler, race-top-K | `/inference/plan` — planned |
| Peer-to-peer weight distribution | planned |
| Replicated chain across the seeds (one shared state) | v3 repair candidate built; public cutover blocked on validator key rotation and an approved genesis/checkpoint |
| Block explorer | source-pinned static candidate built; not yet publicly deployed |

---

## Network endpoints

Production v3 configuration uses these six explicit TLS origins. P2P addresses
are separate and are never converted into RPC URLs at runtime. The origins are
not evidence that the v3 cutover is complete: use them only after the locked
rollout has installed and verified the corresponding gateways.

| Node | Location | v3 HTTPS RPC origin |
|---|---|---|
| NYC | New York | `https://149-28-32-76.nip.io` |
| LAX | Los Angeles | `https://140-82-16-112.nip.io` |
| AMS | Amsterdam | `https://136-244-109-1.nip.io` |
| LHR | London | `https://104-238-171-11.nip.io` |
| NRT | Tokyo | `https://202-182-107-41.nip.io` |
| SGP | Singapore | `https://149-28-153-31.nip.io` |

Raw public `http://IP:9090` origins are legacy diagnostics, not supported v3
client or validator configuration. Non-loopback production RPC must use HTTPS.

Key endpoints:

| Path | Purpose |
|---|---|
| `/health`, `/stats`, `/info` | node + chain health |
| `/block/latest`, `/block/{n}` | blocks (per-seed — see the state note above) |
| `/inference/run`, `/inference/run_sharded` | single-node + sharded inference |
| `/inference/attestations` | raw inference claim records (`0x16`; not payment) |
| `/inference/results` | node-local inference results (in-memory, lost on restart) |
| `/tx/submit`, `/tx/{hash}` | transactions |
| `/validators`, `/shards` | network state |
| `/account/{addr}` | balances |
| `/faucet/claim`, `/faucet/status`, `/tx/{hash}` | faucet submission is pending; only a successful mined receipt confirms the 1 ARC credit |
| `/community/reward_policy` | active policy, recovery epoch, validator set, exact reward, and issuance readiness |
| `/community/reward_approval/{job_id}` | local approval/submission status bound to the recovery epoch and validator set |
| `/community/reward_job/{job_id}`, `/community/reward_receipt/{tx_hash}` | pending, mined-success, or mined-failed `0x25` evidence |
| `/worker/earnings/{addr}` | confirmed mined `0x25` receipt rows; projection is null with an explicit reason unless policy, history, and treasury evidence permit one |
| `/workers/scoreboard` | registered community workers |
| `/eth` | Ethereum JSON-RPC (MetaMask compatible) |

The public v2 seeds still exhibit two known API bugs: `/models` double-counts
replicated layer coverage, and `/worker/earnings/{addr}` reports display
arithmetic rather than mined income. The v3 candidate fixes both: coverage is a
range union, while earnings count only successful retained
`CommunityInferenceReward` receipts. A submitted reward, raw `0x16`
attestation, failed receipt, or faucet POST never increments confirmed ARC.
Forward projections are available only from an explicit active reward policy,
confirmed receipt history, and a treasury that can fund another full reward;
otherwise the value is null and the API returns the reason. Those fixes are
not live until the fleet cutover completes.

See `docs/HOW-SHARDING-WORKS.md` for the wire protocol.

---

## ARC Token

ARC exists today as ERC-20 on Ethereum: `0x672fdba7055bddfa8fd6bd45b1455ce5eb97f499`.

Fixed supply: 1.03 B. No inflation. No burns.

When mainnet launches, ERC-20 holders migrate to native ARC via a bridge contract. On testnet, use the faucet.

---

## Disclaimer

ARC Chain is in active development. This is a testnet. Do not use real funds. Software is provided as-is, no warranty.

---

## License

BUSL-1.1. Source-available today. Becomes Apache 2.0 on 2030-03-25.

**Free forever:**
- Any project under $10 M revenue - full production rights, no approval
- Anything built on ARC Chain at any scale (contracts, tokens, agents, L2s, rollups)
- Validators, inference providers, observers
- Research, education, personal projects, forks, experiments

**Commercial license ($50 K/yr) for $10 M+ revenue orgs that want to:**
- Fork this codebase to launch a competing L1
- Extract consensus / inference / crypto for a competing network
- Repackage the code as their own chain

Built solo from scratch, every line. I want it used. I don't want it taken. Commercial license: tj@arc.ai.
