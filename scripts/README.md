# scripts/ - runtime tooling for the ARC Chain testnet

A guide to the scripts shipped with ARC Chain. Run repository scripts from a
reviewed checkout; do not assume an old `curl | bash` example is an approved
production procedure.

## Production recovery boundary (2026-08-26)

The public v2 validator fleet has mixed versions and divergent chain state.
The v0.7.12 recovery candidate is not published or deployed. An HTTP response
from one seed therefore proves only that one process answered; it does not
prove shared-chain progress, finality, inference assignment, or payment.

All legacy scripts that can provision, upgrade, restart, or self-heal that
fleet are unconditionally retired. They exit with status 78 before reading
credentials, contacting a host/cloud API, changing a service, or deleting
state. Legacy scripts that register workers, submit faucet/transfer/inference
load to hard-coded public seeds, launch a staked public listener, or call any
HTTP response healthy are retired under the same rule. There is no environment flag or command-line override.

## Community-facing scripts

These are the visitor and community-node entry points. Their presence in the
recovery candidate does not mean that candidate has been released.

| Script | When to use |
|--------|-------------|
| [`install-community-node.sh`](install-community-node.sh) | **Joining the network headlessly.** Compatibility entry point for the root [`install.sh`](../install.sh). It installs checksummed `arc-node` + `arc-cli` assets and exact-tag seeds/genesis, a stake-0 service, and an optional verified updater. Repository tag protection remains an owner-controlled release prerequisite. It does **not** download a model; pass `--model /absolute/path.gguf` to execute compatible local inference. |
| [`arc-demo.sh`](arc-demo.sh) | **Inspecting a controlled coordinator.** Requires explicit `ARC_COORDINATOR`; prints its response/trace and distinguishes cache matches from recomputation without claiming public-fleet health. |
| [`arc-verify.sh`](arc-verify.sh) | **Comparing a past inference claim.** Requires explicit `ARC_COORDINATOR`; compares reported commitments and labels cache vs recomputation. This is not exact-artifact or payment proof by itself. |
| [`arc-bench.sh`](arc-bench.sh) | **Testing one controlled endpoint.** Requires explicit `ARC_COORDINATOR`; runs 5 (or 10) prompts and emits a dated endpoint-specific report, not a public-fleet determinism claim. |
| [`arc-pick-coordinator.sh`](arc-pick-coordinator.sh) | **Read-only dated diagnosis.** Scores the configured endpoints and prints one URL. The controlled demo scripts no longer invoke it automatically. A result is not shared-chain health. |

### Historical v2 behavior observed on 2026-08-17

This is a dated diagnostic snapshot, not current production status and not a
claim that the forked fleet is suitable for a public demonstration.

**`arc-pick-coordinator.sh`** scores every reachable seed and returns the best,
rather than short-circuiting on the first hit as it used to. Ranking is
liveness → capability tier → node version → holds attestation data → latency.
At that snapshot it resolved to LHR `104.238.171.11`, the only queried seed
with an inference history. Useful diagnostic knobs were:

```bash
ARC_PICK_VERBOSE=1 bash scripts/arc-pick-coordinator.sh        # show the scoring
ARC_PICK_BLOCK_WINDOW=300 bash scripts/arc-pick-coordinator.sh # also require a block in 300s
ARC_SEEDS="1.2.3.4:9090" bash scripts/arc-pick-coordinator.sh  # override the seed list
```

`ARC_PICK_BLOCK_WINDOW` costs that many seconds of wall time and only means
something at ≥300 s for that snapshot: the fastest queried seed was sealing
roughly one block every few minutes, so a short window reported every seed as
stalled.

**`arc-demo.sh`** step 3 asks for `force_recompute` on the re-run. A coordinator
that supports it recomputes and the script prints `✓ DETERMINISTIC`. A
coordinator that did not (every queried seed in that snapshot) answered from its
content-addressed cache in microseconds, and the script prints
`● SERVED FROM CACHE (hash match)` instead — deliberately, because a cache
lookup is not a determinism proof. Step 2's per-hop table also now says how
much of the wall time it actually covers; the trace samples prefill only, so
expect roughly half.

**`arc-verify.sh`** sweeps every seed and both `/inference/results` and
`/inference/attestations`, because results is empty on most seeds and
attestations pads its list with unrelated transactions tagged `tx_type:"Other"`
once the real rows run out. Those padding rows are filtered. It re-runs against
whichever seed actually holds the record, and reports
`VERIFIED (recomputed)` or `VERIFIED (from cache)` rather than collapsing both
into one verdict.

**`install-community-node.sh`** in this recovery candidate is a compatibility
wrapper around the canonical root `install.sh`. The candidate installer
resolves either the latest release or one exact pin, requires every platform
asset, and verifies all downloads with that release's `SHA256SUMS`. It does not
walk backward through old tags: an incomplete release fails closed.

Public v0.7.10 and v0.7.11 were desktop-only releases. The v0.7.12 recovery
candidate and its restored headless asset matrix are not published yet, so the
commands below are release-shape examples, not a claim that GitHub's current
`latest` release can install every platform today.

```bash
# Example only after the v0.7.12 release is explicitly approved and published:
curl --fail --silent --show-error --location --output install.sh \
  https://github.com/FerrumVir/arc-chain/releases/download/v0.7.12/install.sh
bash install.sh --version 0.7.12
```

The candidate service launches with `--stake 0 --min-stake 0 --community-mode`.
Keep it that way: a community node does not enter the validator set. The
candidate release contract requires Linux x86_64 and ARM64, macOS Apple
Silicon and Intel, and Windows x86_64; the shell installer manages Linux/macOS,
while Windows service setup is manual. See
[`docs/HEADLESS_INSTALL.md`](../docs/HEADLESS_INSTALL.md).

## Retired operator scripts

These files are retained only so old runbooks and installed units fail with a
clear recovery message instead of silently operating the forked fleet.

| Retired entry point | Unsafe legacy behavior now blocked |
|---------------------|------------------------------------|
| [`arc-watchdog.sh`](arc-watchdog.sh) | SSH polling, hard process kills, and relaunch with label-derived validator identity and fixed stake. |
| [`arc-tunnel-watchdog.sh`](arc-tunnel-watchdog.sh) + [`arc-health-check.sh`](arc-health-check.sh) | A reverse SSH tunnel with host verification disabled, and a host-local probe that mislabeled reachable processes as a healthy network. |
| [`rolling-upgrade.sh`](rolling-upgrade.sh) | Remote build/copy, state deletion, service control, and rolling relaunch across hard-coded seeds. |
| [`arc-rolling-restart.sh`](arc-rolling-restart.sh) + [`arc-remote-relaunch.sh`](arc-remote-relaunch.sh) | Capture-and-replay of live validator argv followed by hard process termination. |
| [`arc-self-heal.sh`](arc-self-heal.sh) + [`install-self-heal.sh`](install-self-heal.sh) | Automatic process/service mutation based on host-local health and stale fleet assumptions. Existing installed `arc-self-heal.service` units must remain disabled during recovery. |
| [`deploy-testnet.sh`](deploy-testnet.sh) | Vultr provisioning and mutation with permissive SSH, moving downloads, deterministic validator labels, and fixed stake. |
| [`../deploy/setup-testnet.sh`](../deploy/setup-testnet.sh) + [`../deploy/monitor.sh`](../deploy/monitor.sh) + [`../deploy/teardown.sh`](../deploy/teardown.sh) | Hetzner provisioning, misleading host monitoring, and name-based server deletion. Every `deploy/Makefile` operational target is retired too. |
| [`deploy-explorer.sh`](deploy-explorer.sh) + [`setup-vps.sh`](setup-vps.sh) | Unpinned package/toolchain installation plus public service and benchmark host mutation. |
| [`arc-community-register.sh`](arc-community-register.sh) + [`tps-generator.sh`](tps-generator.sh) + [`load-test.sh`](load-test.sh) | Direct registration, faucet, and transfer writes to hard-coded public v2 seeds. |
| [`inference-benchmark.sh`](inference-benchmark.sh) + [`inference-router.sh`](inference-router.sh) + [`inference-tps-bench.sh`](inference-tps-bench.sh) | Automatic public-seed inference writes and throughput claims against a forked fleet. |
| [`monitor-testnet.sh`](monitor-testnet.sh) | Treated any HTTP response as a healthy node and aggregated incompatible seed state. |
| [`run-node.sh`](run-node.sh) | Bound RPC to every interface and launched a legacy fixed-stake identity without the approved v3 genesis/keyfile contract. |

Replacement operator tooling must consume one approved v3 manifest that pins
artifact digests, unique validator public identities, canonical genesis and
checkpoint hashes, verified host keys, and an explicit activation/rollback
plan. It must verify shared-chain progress and finality before calling a fleet
healthy. See [`../deploy/README.md`](../deploy/README.md).

The old local launchers [`testnet.sh`](testnet.sh),
[`create-testnet.sh`](create-testnet.sh), and [`run_cluster.sh`](run_cluster.sh)
are retired too. They generated fields rejected by the v3 genesis/keyfile
contract or launched seed-derived staked identities without the explicit test
authorization and approved genesis now required. The supported local harness is
the Rust multi-node test suite; no replacement shell launcher is approved yet.

## Pre-existing scripts (older)

These predate the sharded inference work and are kept for backward compatibility.

| Script | Notes |
|--------|-------|
| [`sero-quickstart.sh`](sero-quickstart.sh) | Deprecated model-argument wrapper around the canonical root installer. |
| [`join-inference.sh`](join-inference.sh) | Safe compatibility wrapper around root `install.sh`; requires an explicit local model and remains stake-zero. |
| [`inference-benchmark.sh`](inference-benchmark.sh) + [`inference-router.sh`](inference-router.sh) + [`inference-tps-bench.sh`](inference-tps-bench.sh) | **Retired:** exit before contacting the forked public fleet. Use repository tests or an explicitly controlled local endpoint instead. |
| [`auto-update.sh`](auto-update.sh) | Supported local stake-zero exception: accepts only an absolute local install directory and invokes the already-installed, checksum-verifying `arc-installer --update-only`. It has no SSH, raw download, service, or fleet path of its own. |
| [`check-attestations.sh`](check-attestations.sh) | Read-only view of one node's retained raw inference attestations. A raw `0x16` record is not payment. |
| [`test-inference.sh`](test-inference.sh) | Loopback-only inference smoke test; non-local endpoints fail before `curl`. It does not claim a raw attestation is earnings. |

## Build / deploy / CI

| Script | Purpose |
|--------|---------|
| [`arc-compile.sh`](arc-compile.sh) | Cross-compile arc-node for distribution. |
| [`ci_check.sh`](ci_check.sh) | One-command release, releasable-worktree secret, shell, workflow, Rust, dashboard/explorer, deterministic desktop, and packed-SDK quality gate. `--full` is the default; use `--quick` while iterating. Requires Node.js 24 LTS and Actionlint; full logs land under `target/ci-check/`. |
| [`deploy-testnet.sh`](deploy-testnet.sh) | **Retired:** exits before any Vultr, SSH, service, or host mutation. |
| [`deploy-explorer.sh`](deploy-explorer.sh) | **Retired:** exits before package, service, proxy, node, or explorer mutation. |
| [`setup-vps.sh`](setup-vps.sh) | **Retired:** exits before installing a toolchain, cloning code, or running host benchmarks. |
| [`create-testnet.sh`](create-testnet.sh), [`testnet.sh`](testnet.sh), [`run_cluster.sh`](run_cluster.sh) | **Retired:** exit before generating incompatible keys/config or launching seed-derived staked nodes. Use the Rust multi-node tests. |
| [`install-node.sh`](install-node.sh) | Safe local-checkout compatibility wrapper around root `install.sh`; mutable remote fallback is refused. |
| [`install-inference-node.sh`](install-inference-node.sh) | Safe stake-zero system-service wrapper requiring an explicit local model. |
| [`join-testnet.sh`](join-testnet.sh) | Safe compatibility wrapper around the checksummed stake-zero installer; not a validator quick-join. |
| [`arc-community-register.sh`](arc-community-register.sh), [`tps-generator.sh`](tps-generator.sh), [`load-test.sh`](load-test.sh), [`monitor-testnet.sh`](monitor-testnet.sh), [`run-node.sh`](run-node.sh) | **Retired:** no public mutation, false-health aggregation, or unsafe node launch remains reachable. |
| [`eval-perplexity.sh`](eval-perplexity.sh) | Run perplexity evaluation against WikiText. |

## Quick reference

**I want to try the demo without installing anything:**
```bash
ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-demo.sh
```

**I want to verify the network's most recent inference:**
```bash
ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-verify.sh --latest
```

**I want to join the network as a community node:**
```bash
# Only after v0.7.12 is explicitly approved and published:
curl --fail --silent --show-error --location --output install.sh \
  https://github.com/FerrumVir/arc-chain/releases/download/v0.7.12/install.sh
bash install.sh --version 0.7.12
```

**I want to reproduce the factual benchmark:**
```bash
ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-bench.sh
```

**I'm the operator and a node went down:** do not run a legacy restart,
self-heal, upgrade, deploy, or teardown command. Capture read-only evidence and
use the approved recovery runbook; the repository does not yet contain the v3
manifest-based operator tool.
