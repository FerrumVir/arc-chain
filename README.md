![Rust](https://img.shields.io/badge/Rust-196K%2B_LOC-orange)
![Tests](https://img.shields.io/badge/Rust_tests-1%2C900%2B_defined-brightgreen)
![License](https://img.shields.io/badge/license-BUSL--1.1-blue)
![Inference](https://img.shields.io/badge/inference-CPU_KAT--verified-purple)
![Testnet](https://img.shields.io/badge/public_fleet-forked-red)

# ARC Chain - Trustworthy AI

**A Rust Layer 1 recovery candidate designed to make AI inference
reproducible, independently recomputed, and explicitly authorized before a
community reward can settle.**

ARC's protocol and product code are developed in this repository. Reviewed
upstream components vendored for reproducibility are identified under
`vendor/`; they are not represented as ARC-authored code.

## v0.8.0 release status and quickstart

> **Source-freeze snapshot (2026-08-31; tag-stable):** At this commit's review
> cutoff, v0.8.0 / protocol v3 was not published or deployed, and GitHub
> `latest` was still the desktop-only v0.7.11 bundle. This is a historical
> pre-tag statement, not a live status probe. An immutable v0.8.0 tag keeps
> this source-freeze record; determine current release and fleet status only
> from the exact release evidence and signed rollout receipts described below.
> The default branch may receive a reviewed post-rollout status update.

This README does not use the moving `latest` release as an install source. If
the complete
[exact v0.8.0 release](https://github.com/FerrumVir/arc-chain/releases/tag/v0.8.0)
shows every required asset, `SHA256SUMS`, and `SHA256SUMS.sig`, an
SSH/EC2/VPS operator can run:

```bash
curl -fsSLO --proto '=https' --proto-redir '=https' --tlsv1.2 https://raw.githubusercontent.com/FerrumVir/arc-chain/v0.8.0/install.sh
ARC_INSTALL_SHA256=355bbf283b028ffe16a4ebfbdc5cb5cd0e994b0874f368511c887aa735c8fd27
if command -v sha256sum >/dev/null 2>&1; then
  printf '%s  %s\n' "$ARC_INSTALL_SHA256" install.sh | sha256sum -c -
else
  printf '%s  %s\n' "$ARC_INSTALL_SHA256" install.sh | shasum -a 256 -c -
fi
bash install.sh --version 0.8.0
```

Expected `install.sh` SHA-256 for this candidate:
`355bbf283b028ffe16a4ebfbdc5cb5cd0e994b0874f368511c887aa735c8fd27`.

The unified release contract restores headless Linux amd64 and arm64, Intel
and Apple Silicon macOS, Windows CLI binaries, signed desktop-updater payloads,
normalized desktop installers, and one checksummed installer. Until the exact
release evidence exists, build and test only from a reviewed checkout; do not
treat the commands above as a production download. Publication alone never
proves fleet deployment: the public fleet also requires the
[coordinated recovery/cutover gate](docs/VALIDATOR-FLEET-ROLLOUT.md).
Unsigned artifacts, updater signatures, the owner-signed checksum manifest,
draft publication, and read-only release verification cross separate
exact-ID/digest-bound jobs; protected key windows execute only reviewed direct
signing tools and never repository programs.

### Community support answer sheet

| Community question | Evidence-backed answer at the 2026-08-31 source freeze |
|---|---|
| Can an SSH-only EC2/VPS install ARC? | Not from public v0.7.11. The complete v0.8.0 release restores real headless `arc-node` assets for Linux amd64 and arm64; the GUI packages are not server binaries. |
| Are Intel and Apple Silicon Macs supported? | The v0.8.0 contract builds separate Intel and Apple Silicon CLI and desktop assets. Treat those links as installable only when the exact immutable release exposes the complete signed asset set. |
| Does automatic update work? | Public v0.7.11 saved the desktop preference but never scheduled checks. The v0.8.0 source-freeze candidate adds signed desktop checks after startup and every 24 hours, plus a transactional daily headless updater. Desktop updates still require confirmation; `.deb` and `.rpm` remain package-manager owned. |
| When are the seeds upgraded? | At the source freeze they had not been upgraded, and there is no honest calendar promise in this repository. Publishing v0.8.0 alone does not update them. Current deployment requires signed rollout receipts proving one coordinated, archive-bound checkpoint cutover and agreement by all six above the greatest block height users saw before maintenance. |
| Can a stake-zero worker earn the configured 2.5 testnet ARC? | Stake zero is eligible, but registration and raw inference tx `0x16` pay nothing. Exact-model assignment, independent verification, five-of-six reward authorization, activation, treasury limits, and a successful mined `0x25` receipt are all required. At the source freeze, that payment path was not live on the public v2 fleet. |
| What should a worker run? | A router needs no model or GPU. The current full-worker target is Llama-2-7B Q4_K_M (about 4 GB on disk). The release has no validated minimum-RAM claim yet: prove a complete model load and inference with OS/chain headroom on the target host. GPU is optional and hardware never guarantees work. |
| Where are earnings and the block explorer? | At the source freeze, the corrected dashboard and source-pinned explorer were candidates rather than supported public services. They fail closed to maintenance mode unless live checkpoint, replica, inference, and receipt gates pass; projections fail closed to null when evidence is insufficient. |

Use the [headless/server guide](docs/HEADLESS_INSTALL.md) for the complete
platform, update, permissions, and troubleshooting contract. After the seed
cutover is proven, the
[2–3 minute community-node walkthrough](docs/COMMUNITY-NODE-WALKTHROUGH.md)
demonstrates install, assignment, inference verification, a mined reward
receipt, earnings, and the matching explorer block without overstating any
missing evidence.

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
public seeds did not run this release candidate at the 2026-08-31 source
freeze.

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

**Read-only public-fleet snapshot, 2026-08-31 around 13:19 CDT.** These are
observations, not a standing uptime promise:

| Seed | Version | State height | Peers | DAG round |
|---|---:|---:|---:|---:|
| NYC | 0.7.2 | 138,244 | 8 | 9,841,293 |
| LAX | 0.7.9 | 129,637 | 8 | 9,841,293 |
| AMS | 0.7.9 | 89,633 | 8 | 9,841,293 |
| LHR | 0.7.9 | 51,422 | 8 | 9,841,292 |
| NRT | 0.7.9 | 96,770 | 8 | 9,841,292 |
| SGP | 0.7.9 | 97,591 | 8 | 9,841,292 |

At common height 50,000, all six reachable seeds returned **six different
block hashes and six different state roots**. The dashboard independently
repeated the comparison at the highest common height, 51,422, with the same
6/6 divergence. Nearly matching DAG rounds in the newer snapshot do not repair
that block/state disagreement. Therefore the public fleet is not one replicated chain.
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

| Candidate desktop build target | Normalized v0.8.0 asset (valid after publication) |
|---|---|
| **macOS — Apple Silicon** (built/packaged on macOS 15) | [DMG](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-macos-arm64.dmg) |
| **macOS — Intel** (built/packaged on macOS 15 Intel) | [DMG](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-macos-x86_64.dmg) |
| **Windows — x86_64** (built/packaged on GitHub `windows-latest`) | [NSIS installer](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-windows-x86_64-setup.exe) · [MSI](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-windows-x86_64.msi) |
| **Linux desktop — x86_64** (built/packaged on Ubuntu 24.04) | [AppImage](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-linux-x86_64.AppImage) · [.deb](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-linux-x86_64.deb) · [.rpm](https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-linux-x86_64.rpm) |

These stable names are generated by the unified v0.8.0 release pipeline;
until that exact release is published, the links intentionally do not resolve.
The macOS package metadata declares 11.0 as its minimum, but CI currently runs
the package only on macOS 15; no older-macOS or Windows-version runtime floor is
claimed until those exact versions receive a release-blocking test.
The GUI is not a server binary: it needs a
graphical session. An EC2/VPS/SSH-only machine should use the headless installer
below. Linux ARM64 is also headless-only.

The public v0.7.11 desktop could manually check, verify, and install a signed
update from Settings, but its saved automatic-update preference never started
a scheduler. The v0.8.0 candidate now checks the signed manifest shortly after
startup and every 24 hours when that setting is enabled. Background checks do
not download or install anything; the user confirms installation. macOS,
Windows, and Linux AppImage can then update in place. `.deb` and `.rpm` remain
owned by their package managers; the app reports the available version but
cannot invoke in-app replacement for those channels.

**📖 Desktop controls:** [Getting Started with ARC Node](docs/GETTING_STARTED.md)
— release gates, identity, inference evidence, faucet, mined reward receipts,
and FAQ.

---

### Headless / server node (no GUI or display required)

The candidate headless assets target Linux x86_64/amd64 and ARM64, macOS Apple
Silicon and Intel, and Windows x86_64. The installer supports Linux and macOS;
Windows Server operators download the two `.exe` assets and `SHA256SUMS`
manually from the exact v0.8.0 release. The Linux x86_64/amd64 artifact is
built on Ubuntu 22.04 and must boot with `DISPLAY` unset on Ubuntu 22.04,
24.04, and 26.04 before publication; the ARM64 artifact has the same GUI-free
runtime gate on Ubuntu 24.04 and 26.04. The macOS binaries boot on macOS 15
Apple-Silicon and Intel runners, and the Windows x86_64 binary boots on GitHub's
`windows-latest` runner; those gates do not establish older OS-version floors.

The following recovery command is intentionally pinned and becomes valid only
after GitHub shows the complete `v0.8.0` release. The moving `latest` alias is
never used by the initial install command.

```bash
curl -fsSLO --proto '=https' --proto-redir '=https' --tlsv1.2 https://raw.githubusercontent.com/FerrumVir/arc-chain/v0.8.0/install.sh
ARC_INSTALL_SHA256=355bbf283b028ffe16a4ebfbdc5cb5cd0e994b0874f368511c887aa735c8fd27
if command -v sha256sum >/dev/null 2>&1; then
  printf '%s  %s\n' "$ARC_INSTALL_SHA256" install.sh | sha256sum -c -
else
  printf '%s  %s\n' "$ARC_INSTALL_SHA256" install.sh | shasum -a 256 -c -
fi
bash install.sh --version 0.8.0
```

Expected `install.sh` SHA-256:
`355bbf283b028ffe16a4ebfbdc5cb5cd0e994b0874f368511c887aa735c8fd27`.

The bootstrap installer comes from the owner-created protected source tag.
It resolves an exact immutable, non-draft release, requires GitHub to identify
the publisher as `github-actions[bot]`, and verifies the owner-signed
`SHA256SUMS.sig` before trusting checksums for `arc-node`, `arc-cli`, seeds,
genesis, or the retained updater. It refuses unsigned, mutable/prerelease,
missing, unknown-version, and downgrade paths. On Linux it installs a systemd
system service when run as root and a systemd user service otherwise; on macOS
it installs a LaunchAgent. It preserves a mode-`0600` private Ed25519 keyfile
across upgrades and never places secret identity material in the process
command line or environment. Managed stake-zero
nodes bind RPC to `127.0.0.1` only; `--port` changes the local port, not the
interface, so a permissive EC2 security group cannot expose RPC accidentally.
`--data-dir` must be an absolute, dedicated directory. The installer rejects
relative/traversal-shaped paths, symlinked paths, operating-system roots such
as `/etc`, `/usr`, and `/var`, and any data path that contains or overlaps the
managed program/identity tree before it runs `mkdir`, `chown`, or `chmod`.
Dedicated descendants such as `/var/lib/arc-chain/data`, `/srv/arc-data`, and
`$HOME/arc-chain-data` remain supported.

A fresh install atomically claims its dedicated install directory with an
ARC marker bound to that exact normalized path. Updates require the marker, so
the installer will not silently claim a pre-existing unmarked directory. The
only compatibility exception is an exact default `~/.arc` (or the system
default `/var/lib/arc-chain`) containing a fully recognized, correctly owned,
non-symlinked v0.7.x community-node layout beneath non-writable ancestors. It
receives a path/data/version/service-manager/supervisor/port/model-bound,
fsynced pending marker before any v0.8 replacement; custom roots and
partial/lookalike layouts remain blocked. The bridge recognizes only the real
v0.7 Linux global units (`arc-node.service` plus the optional
`arc-updater.service`/`.timer` pair), macOS labels `com.arc.inference` and
`com.arc.updater`, or an exact live `node.pid` command. Ambiguous supervisors,
changed directives, stale PIDs, symlinks, and unsafe `~/.arc` ancestors fail
before reservation or release download.
Every uninstall requires the final marker's contents, owner, permissions, and
path binding; a pending adoption marker is not an uninstall capability.
`--uninstall --purge` validates the final marker again immediately before
recursive deletion. It removes only the marked install root; a custom
`--data-dir` outside that root is deliberately preserved.

v0.8.0 writes `genesis.network-hash` into fresh persisted state and fails
closed when an existing WAL has no marker or its hash differs from the selected
genesis. Do not reuse a v0.7.11-or-earlier data directory. Back up an observer's
identity and old data for forensics, then select a fresh `--data-dir`; validators
require the approved canonical checkpoint migration. On a failed install or
update, the installer restores every managed binary, network file, runner,
config, identity file, service unit, and the prior service/timer state. That
rollback is not a migration and never rewrites the model or chain data.
For the narrowly verified default v0.7.x layout, the installer automates this:
it retains `data/`, the model, and the exact identity, archives the old
version/seeds/genesis/identity under `legacy-v0.7-preserved/`, and configures
fresh v0.8 state at `data-v0.8/`. It also retains verified custom RPC/P2P
ports and the active model. On Linux, the historical root-owned global service
is replaced with root-owned managed units whose node and checksummed updater
both execute as the original community user; the old unsigned updater and its
timer are stopped and removed before the binary changes. The one-time bridge
uses sudo, while later scheduled updates run as that user and signal only the
owned node process. A failed bridge restores the exact old binary, unit files,
and prior active/enabled state. Purge stays disabled while adoption is pending
and becomes eligible only after the complete v0.8 transaction commits.

Useful server options:

```bash
# SSH-only Ubuntu server, custom RPC/P2P ports and data volume
bash install.sh --version 0.8.0 \
  --port 19090 --p2p-port 19091 --data-dir /srv/arc-data

# Install binaries/config only; print the command but do not start anything
bash install.sh --version 0.8.0 --no-service --no-auto-update

# Serve deterministic community inference. The installer adds
# --full-integer-worker; it does not advertise this home node as a layer shard.
bash install.sh --version 0.8.0 --model /absolute/path/to/model.gguf

# Reproducible pin; an older version is rejected if a newer one is installed
bash install.sh --version 0.8.0
```

For an install that kept the scheduled updater, the manual commands are:

```bash
# Linux user service, adopted v0.7 system-user bridge, or macOS LaunchAgent
"$HOME/.arc/bin/arc-installer" --update-only --install-dir "$HOME/.arc"

# Linux system service
sudo /var/lib/arc-chain/bin/arc-installer --update-only --install-dir /var/lib/arc-chain --system-service
```

Update mode intentionally resolves the newest immutable, non-draft release,
requires the complete bundle for the installed platform, verifies it, and
refuses equality or downgrade; do not add `--version 0.8.0` when the goal is to
discover a later safe update.

Managed macOS nodes receive a 4,420-second launchd `ExitTimeOut`. On the first
upgrade from an older plist, the installer sends SIGTERM to the exact inspected
node PID and waits for its graceful drain before unloading it. The scheduled
updater remains loaded while it may be the process running that transaction,
so auto-update cannot terminate itself halfway through replacement.

Without `--model`, the node is an observer/router and will not execute local
model inference. It still joins with `--stake 0 --community-mode`; stake-zero
is the safe community posture, but rewards and work assignment are determined
by the network and are not guaranteed by the installer. See
[`docs/HEADLESS_INSTALL.md`](docs/HEADLESS_INSTALL.md) for service commands,
firewall notes, upgrade behavior, and Windows verification. The short operator
demo is [`docs/COMMUNITY-NODE-WALKTHROUGH.md`](docs/COMMUNITY-NODE-WALKTHROUGH.md).

With `--model`, the managed runner also passes `--full-integer-worker`. That
loads all deterministic integer transformer layers so a claimed result can be
independently recomputed, but deliberately emits no `ShardInfo`. Do not replace
it with `--shard-range 0:32`: a residential node would then announce an
overlapping, normally NAT-unreachable validator shard and poison the displayed
coverage map.

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

In the read-only 2026-08-28 public-v2 snapshot, `model_hash` was still a BLAKE3 of the model's
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
their evidence; it is not a claim that every item was deployed on the
version-skewed public fleet observed at the 2026-08-31 source freeze:

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
   from the explicit HTTPS validator origins and requires five of six. The
   checked-in recovered genesis binds the rotated six-validator set and block
   137146 activation boundary; issuance still fails closed until the
   coordinated rollout is live and the independent runtime switch is enabled.

9. **Post-quantum signature code paths.** Falcon-512 and ML-DSA exist alongside
   Ed25519, BLS12-381, and secp256k1 in the current source tree. This is not a
   claim that the divergent public fleet runs one coordinated release of them.

10. **DashMap lock-inversion repair in `index_account_tx`.** The source no
    longer holds one shard write lock while acquiring another. That removes one
   identified deadlock; it is not evidence that the forked public fleet
   observed at the 2026-08-31 source freeze was healthy.

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
  [6,12)          LHR · NRT · SGP
  [12,17)         NYC · LAX · LHR
  [17,22)         NYC · AMS · NRT
  [22,27)         LAX · NRT · SGP
  [27,32)         AMS · LHR · SGP

  NYC 149.28.32.76   LAX 140.82.16.112   AMS 136.244.109.1
  LHR 104.238.171.11 NRT 202.182.107.41  SGP 149.28.153.31
```

Each `→` is a validator-only HTTPS `POST /inference/forward_shard` to the next
shard. The public gateway denies that route unless the source is one of the six
sealed validator IPs. Each shard verifies the previous shard's BLAKE3 hash
before computing. The last shard runs `final_norm + LM head + argmax` and
returns the next token id. The coordinator collects tokens until `max_tokens`
or EOS.

Coordinators batch the whole prompt into one round-trip per shard (`"prefill":"batch"`) and pick the lowest-latency replica for each range. Racing several replicas at once and taking the first to finish is designed but not shipped.

Each node holds exactly 16 of the 32 layers, and every layer has exactly three
validator replicas. Verify the post-cutover map through a reviewed HTTPS
origin, for example `curl https://104.238.171.11/shards`; raw public
`:9090` is intentionally unavailable.

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
│  Block production, HTTP/JSON RPC + ETH JSON-RPC,       │
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

The current checkout contains more than 196,000 physical lines of checked-in,
non-vendored Rust across ARC's crates, agents, relayer, faucet, desktop backend,
and integration tests: 17 ARC packages, plus one narrowly
vendored `wasmer-derive` workspace member that is excluded from that line count.
More than 1,900 Rust test functions are defined in the same non-vendored tree. These
are source-tree counts, not test-pass claims; the commands below are the release
evidence. Run the complete release gate from the repository root:

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

| Crate | What it does |
|---|---|
| `arc-types` | transaction types, blocks, accounts, governance, staking, bridge, inference certificates/rewards |
| `arc-state` | state DB, Jellyfish Merkle Tree, WAL, parallel execution, GPU-resident cache |
| `arc-crypto` | Ed25519, secp256k1, BLS12-381, BLAKE3, Falcon-512, ML-DSA, VRF, Stwo STARK prover |
| `arc-olm` | on-chain language-model runtime and deterministic INT16 inference |
| `arc-vm` | Wasmer WASM + revm EVM, gas metering, precompiles, inference oracle |
| `arc-node` | block production, RPC, community work coordination, sharded inference |
| `arc-inference` | pure-integer engine, model loading, committee selection, distributed dispatch |
| `arc-consensus` | DAG consensus, 2-round finality, slashing, VRF, epoch transitions |
| `arc-gpu` | Metal/WGSL verification and GPU memory support |
| `arc-net` | QUIC transport, shred propagation, FEC, gossip, peer exchange |
| `arc-mempool` | queueing and deduplication; encrypted submission is not enabled in v0.8 |
| `arc-cli`, `arc-channel`, `arc-bench`, `arc-relayer`, `arc-agents` | CLI, payment channels, benchmarks, bridge, example agents |

The tree also contains Python and TypeScript SDKs, Solidity contracts, the
desktop app, dashboard, and a dependency-free static block explorer.

---

## What existed in the 2026-08-31 source-freeze candidate

This table describes the source-freeze tree. At that cutoff, the public testnet
was still on v0.7.2/v0.7.9. These rows can be described as deployed together
only when the coordinated rollout evidence proves it.

| | |
|---|---|
| DAG consensus, 2-round commit | implemented in source; v3 trusted-set cutover not performed |
| Self-heal daemon | scripts and service units exist; does not repair a forked trust root |
| Deterministic CPU I8/I16 inference | ✅ hardcoded ARM/x86 KAT; GPU/full-GGUF unverified |
| Sharded inference, 3× range replication, transit BLAKE3 | candidate endpoints implemented; not deployed to the public fleet at the source freeze |
| Authenticated range recomputation | candidate requires 2-of-3 for every range/token; not deployed at the source freeze |
| Latency-aware replica selection per layer range | ✅ rolling EWMA |
| Auto-shard node onboarding | ✅ `--auto-shard` flag |
| Inference computation certificate | legacy/history-only tx `0x16`; v3 rejects standalone submission and embeds/reverifies the worker certificate inside payable `0x25` |
| Community reward settlement | tx `0x25`, five-of-six active-validator identity + stake approvals; implemented and receipt-gated; recovered genesis activates at block 137146, but the candidate was not deployed at the source freeze |
| EVM (Solidity) + WASM (Rust / C / Go) both | ✅ revm 19, Wasmer 6.0 |
| 5 signature algorithms incl. 2 post-quantum | ✅ Ed25519 · Falcon-512 · BLS · ML-DSA · secp256k1 |
| BLS threshold encrypted mempool (MEV protection) | not shipped: v0.8 explicitly leaves the proposer-local, non-replicated prototype disabled |
| Zero-fee agent settlements | ✅ `Settle` (0x06) · `RegisterAgent` (0x07) |
| Wallet and dashboard UIs | public diagnostics exist; corrected candidate UI was not deployed at the source freeze |

### Built but not doing its job at the source freeze

Listed separately because the code existed and the endpoint answered, but the
thing you would assume from the name was not happening at the review cutoff:

| | |
|---|---|
| Validator slashing (equivocation, liveness) | implemented in `arc-consensus`; no slash has been triggered on the live net |
| VRF committee re-execution | committee is selected and recorded; votes are never collected |
| Legacy inference claims in historical blocks | host-dependent on the forked fleet; v3 rejects new standalone `0x16`, and a historical mined `0x16` pays nothing |
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
| Block explorer | source-pinned static candidate built; not publicly deployed at the source freeze; support requires live rollout evidence and an exact published URL |

---

## Block-level history and explorer continuity

Recovery does not call the six divergent v2 databases one chain, discard their
evidence, or restart height at zero. Before production mutation, the rollout
contract fences the six controlled writers and content-indexes every retained
source. An independently preserved reference snapshot/WAL pair must reproduce
the approved block and state root at height H. Each divergent source is kept
and labelled canonical, non-canonical, or unclassified; it is never silently
merged into balances or transaction history.

The selected checkpoint imports the canonical blocks `0..H` into every v3
validator. The recovery boundary is exactly `H+1`, whose parent hash must equal
the signed block H hash; the rollout then proves the same advancing
height/hash/state-root commitment on all six validators, including after
one-at-a-time restarts. That is a block-level continuation, not a reset or a
claim that every old fork became canonical.

The dashboard and explorer consume configuration generated from that sealed
rollout. They do not label anything canonical until the exact H/H+1 boundary,
network identity, and all six replica identities verify live. The explorer's
automatic timeline serves retained history through H and the v3 continuation
from H+1; explicit alternate-source views keep their provenance and are never
promoted into canonical search results. The checked-in public configuration is
still maintenance-only, so no public explorer URL is supported yet.

### Freeze, archive, and late-fork safety

The cutover is a sealed transaction, not a rolling best-effort upgrade. Before
any new validator key is installed, all six legacy writers are fenced and two
independent quarantine samples—at least 120 seconds apart—must agree per host
that its writer, listeners, and persisted head stayed stable. Fleet-wide head
equality is deliberately not required: the six divergent lineages are the
incident evidence being preserved. Each source is content-indexed and assigned
an explicit canonical, non-canonical-fork, or unclassified disposition.

The local archive, its inventory, the maintenance boundary, the public-height
cutoff, and the separately downloaded Google Drive completion evidence are
hash-bound into the finalized rollout. Drive is transport and redundant
storage, not a WORM trust anchor; the cryptographic manifests are what make a
replacement or partial upload detectable. The new fleet remains offline until
those archive roots and the checkpoint boundary verify together.

Recovery also seals a declared legacy-source set and the exact monitoring tool.
After cutover, every gateway publishes a short-lived maintenance-interlock
status. The dashboard and explorer require fresh healthy status from all six;
a missing, stale, or tripped replica pauses canonical, inference, balance, and
earnings claims. If any official or declared legacy source coherently serves a
block above the pre-maintenance cutoff, the monitor creates a persistent
incident and returns to maintenance. It does not auto-clear the incident or
silently make that late fork canonical; disposition remains an offline,
operator-authorized recovery action.

Rotated validator keys use a separate one-shot delivery boundary. The retained
passphrase-encrypted vault is validated without extracting members, rewrapped
to an operator-supplied CMS certificate, and restored only against the exact
protected-main commit and protected pre-tag Linux artifacts. Installation is
create-only over strict pinned SSH and requires authenticated offline-stop
evidence v2 for all six legacy writers, so a successful archive alone cannot
authorize private-key delivery.

---

## Network endpoints

The production-v3 candidate configuration uses these six explicit, literal-IPv4
TLS origins. P2P addresses are separate and are never converted into RPC URLs
at runtime. The locked rollout stages SHA-pinned Caddy 2.11.4 and requests a
publicly trusted IP-address certificate from Let's Encrypt's production ACME
service with the `shortlived` profile and HTTP-01 challenge. This removes the
shared `nip.io`/`sslip.io` wildcard-DNS dependency; certificate issuance and
renewal still fail closed on the public ACME and port-80 reachability checks.
These candidate origins are not evidence that the v3 cutover is complete: use
them only after the locked rollout has installed and verified every gateway.

| Node | Location | v3 HTTPS RPC origin |
|---|---|---|
| NYC | New York | `https://149.28.32.76` |
| LAX | Los Angeles | `https://140.82.16.112` |
| AMS | Amsterdam | `https://136.244.109.1` |
| LHR | London | `https://104.238.171.11` |
| NRT | Tokyo | `https://202.182.107.41` |
| SGP | Singapore | `https://149.28.153.31` |

Raw public `http://IP:9090` origins are legacy diagnostics, not supported v3
client or validator configuration. Non-loopback production RPC must use HTTPS.

The sealed protocol-v3 gateway exposes only the following exact public API.
Unknown paths and methods return 404/405; a handler existing in source does not
make it public.

Public GET paths carried verbatim in the sealed rollout manifest:

<!-- ARC_PUBLIC_GET_BEGIN -->
`/health`
`/info`
`/network/info`
`/stats`
`/validators`
`/block/latest`
`/blocks`
`/inference/attestations`
`/economics/rewards`
`/faucet/status`
`/community/list`
`/community/reward_policy`
`/workers/scoreboard`
`/shards`
`/models`
`/models/shards`
<!-- ARC_PUBLIC_GET_END -->

The gateway also admits these strictly shaped public GET routes:

<!-- ARC_PUBLIC_PARAMETERIZED_GET_BEGIN -->
`/block/{height}`
`/block/{height}/txs`
`/tx/{hash}`
`/tx/{hash}/full`
`/account/{address}`
`/account/{address}/txs`
`/worker/earnings/{address}`
`/community/reward_receipt/{tx_hash}`
`/community/reward_job/{job_id}`
<!-- ARC_PUBLIC_PARAMETERIZED_GET_END -->

Public POST paths carried verbatim in the sealed rollout manifest:

<!-- ARC_PUBLIC_POST_BEGIN -->
`/inference/run`
`/inference/run_consensus`
`/community/register`
`/community/heartbeat`
`/community/claim_work`
`/community/submit_work`
`/tx/submit`
`/tx/submit_signed`
`/tx/submit_batch`
`/faucet/claim`
<!-- ARC_PUBLIC_POST_END -->

`/inference/run*` has a 4,000-second upstream timeout, worker submission has a
2,700-second timeout, and the validator-only approval path has a 1,500-second
timeout. The faucet POST is only a submission; only a successful mined receipt confirms the 1 ARC credit.

`/internal/community/reward/approve`, `/shards/announce`,
`/inference/forward_shard`, and `/inference/cleanup_shard` are restricted to the
six sealed validator IPs and have no browser CORS policy. The source handlers
`/inference/run_sharded`, `/inference/results`,
`/community/reward_approval/{job_id}`, and `/eth` are intentionally not routed
by the public v3 gateway. Legacy/demo documents describing those paths are not
the production API contract. `/tx/submit` is the public flat transfer contract
and `/tx/submit_batch` is its batch form, used across the supported SDKs. The
batch contract has a hard 64-item maximum and shares the atomic 10 tx/s
per-sender admission policy with the single and generic signed routes. The node
rejects every request item that omits either the transaction signature or its
public key; publishing these routes does not restore unsigned transaction
submission.

`/worker/earnings/{address}` returns only confirmed mined `0x25` receipt rows.
The v0.8 production rollout forces archive mode on all six selectable public
validators, disables receipt/transaction/WAL pruning in that mode, requires
`archive_mode=true` from every earnings response, and restarts every validator
after the two rollout receipts are mined before re-proving both rows. Once that
cutover passes, this is complete gross reward history since the v3 recovery
boundary across every later recovery epoch—not a wallet's net balance after
transfers or spending. The exact `history_domain` field makes that all-v3
scope machine-checkable. Historical receipt rows retain their own
`recovery_epoch`, `validator_set_id`, and `transaction_domain`; the matching
top-level fields describe the current issuance/readiness context and need not
equal older rows. The desktop
keeps a valid zero separate from an unavailable or malformed RPC response and
never turns a failed read into “0 ARC.” Projection is null with an explicit
reason unless policy, confirmed history, treasury, and remaining consensus
budget all permit one. These durability claims are still candidate behavior
until the protected production cutover completes.

The public v2 seeds still exhibit two known API bugs: `/models` double-counts
replicated layer coverage, and `/worker/earnings/{addr}` reports display
arithmetic rather than mined income. The v3 candidate fixes both: coverage is a
range union, while earnings count only successful retained
`CommunityInferenceReward` receipts. A submitted reward, raw `0x16`
attestation, failed receipt, or faucet POST never increments confirmed ARC.
Forward projections are available only from an explicit active reward policy,
confirmed receipt history, a treasury that can fund another full reward, and
remaining consensus block/epoch/worker/coordinator budget; otherwise the value is null and the API returns the reason. The v0.8 reward is a protocol-capped
testnet promotional compute subsidy, not customer demand or revenue. Five-of-six
recomputation proves output agreement, not that a customer paid for the job.
Those fixes are not live until the fleet cutover completes.

See `docs/HOW-SHARDING-WORKS.md` for the wire protocol.

---

## ARC Token

ARC's Ethereum ERC-20 contract is `0x672fdba7055bddfa8fd6bd45b1455ce5eb97f499`.

Fixed supply: 1.03 B. No inflation. No burns.

When mainnet launches, ERC-20 holders migrate to native ARC via a bridge contract. On testnet, use the faucet.

---

## Disclaimer

ARC Chain is in active development. This is a testnet. Do not use real funds. Software is provided as-is, no warranty.

---

## License

BUSL-1.1. Source-available under that license; becomes Apache 2.0 on
2030-03-25.

**Free forever:**
- Any project under $10 M revenue - full production rights, no approval
- Anything built on ARC Chain at any scale (contracts, tokens, agents, L2s, rollups)
- Validators, inference providers, observers
- Research, education, personal projects, forks, experiments

**Commercial license ($50 K/yr) for $10 M+ revenue orgs that want to:**
- Fork this codebase to launch a competing L1
- Extract consensus / inference / crypto for a competing network
- Repackage the code as their own chain

ARC-specific work is offered under this repository's terms; reviewed third-party
code retains its documented provenance and licenses. Commercial licensing:
tj@arc.ai.
