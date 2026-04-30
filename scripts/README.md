# scripts/ - runtime tooling for the ARC Chain testnet

A guide to the scripts shipped with ARC Chain. Most of these can be run as `bash scripts/<name>.sh` from the repo root, or piped via `curl … | bash` from a fresh machine.

## The 4 scripts you actually want

These four are the ones a visitor or community node operator runs.

| Script | When to use |
|--------|-------------|
| [`install-community-node.sh`](install-community-node.sh) | **Joining the network.** One-command installer that downloads the binary, model, seeds, and genesis, generates a unique validator seed, and installs as a persistent launchd / systemd service with daily auto-update. |
| [`arc-demo.sh`](arc-demo.sh) | **Trying the demo.** End-to-end: discover the live shard pipeline → run real inference → re-run for determinism check → run a different prompt for isolation check → print summary. Single command, no install. |
| [`arc-verify.sh`](arc-verify.sh) | **Auditing a past inference.** Takes any attestation `tx_hash` (or `--latest`) and re-derives the inference, comparing both `output_hash` and `model_hash` to the on-chain claim. The cryptographic verifier. |
| [`arc-bench.sh`](arc-bench.sh) | **Reproducing the factual benchmark.** Runs 5 (or 10 with `ARC_BENCH_FULL=1`) factual prompts through the sharded pipeline, checks each output for an expected keyword, emits a markdown report. |

## Operator scripts (testnet maintenance)

These are for the operator running the testnet, not for end users.

| Script | What it does |
|--------|-------------|
| [`arc-self-heal.sh`](arc-self-heal.sh) + [`arc-self-heal.service`](arc-self-heal.service) + [`install-self-heal.sh`](install-self-heal.sh) | **On-host self-heal daemon (GH #30).** Runs as a systemd unit on each seed. Polls localhost `/health`; restarts arc-node on RPC silence (≥180 s) or consensus drift (round unchanged ≥300 s while a remote peer is ≥100 rounds ahead). Reads `/proc/PID/cmdline` + `/proc/PID/environ` so every `--shard-range`, `--model`, and `ARC_PUBLIC_SOCKET` survives the restart. `KillMode=process` in the unit so `systemctl restart arc-self-heal` doesn't take arc-node down as cgroup collateral. Installed via `bash scripts/install-self-heal.sh <NODE_IP>`. |
| [`arc-watchdog.sh`](arc-watchdog.sh) | Legacy off-cluster watchdog (run from your laptop, SSHes to each seed). Superseded by the on-host `arc-self-heal` daemon above; kept for emergencies when SSH is all you have. |
| [`arc-health-check.sh`](arc-health-check.sh) | One-shot ping of the 6 seeds via SSH. Prints peer count + dag_round per node. Reports `STATUS: ALL HEALTHY` or `STATUS: SOME NODES DOWN`. |
| [`rolling-upgrade.sh`](rolling-upgrade.sh) | Builds the new binary on NYC, copies it to each other seed, and rolling-restarts them with health verification. Use `--skip-build` to deploy an existing NYC binary. |
| [`tps-generator.sh`](tps-generator.sh) | Pumps faucet transfers across the 6 seeds to drive visible TPS on the dashboard. Uses round-robin distribution and configurable worker count. |

## Pre-existing scripts (older)

These predate the sharded inference work and are kept for backward compatibility.

| Script | Notes |
|--------|-------|
| [`sero-quickstart.sh`](sero-quickstart.sh) | Older simpler installer. **Prefer `install-community-node.sh`** for new installs. |
| [`join-inference.sh`](join-inference.sh) | Build-from-source inference node setup. |
| [`inference-benchmark.sh`](inference-benchmark.sh) | Sequential vs parallel inference benchmark for the parallel-mode load-balancing demo. |
| [`inference-router.sh`](inference-router.sh) | Round-robin distributor for the parallel inference demo. |
| [`auto-update.sh`](auto-update.sh) | Older auto-updater. The new community installer ships its own auto-updater. |
| [`check-attestations.sh`](check-attestations.sh) | Lists recent inference attestations from the chain. |
| [`test-inference.sh`](test-inference.sh) | One-off inference smoke test. |

## Build / deploy / CI

| Script | Purpose |
|--------|---------|
| [`arc-compile.sh`](arc-compile.sh) | Cross-compile arc-node for distribution. |
| [`ci_check.sh`](ci_check.sh) | Run the same checks CI runs locally. |
| [`deploy-testnet.sh`](deploy-testnet.sh) | Bootstrap a fresh testnet on a list of fresh hosts. |
| [`deploy-explorer.sh`](deploy-explorer.sh) | Deploy the dashboard / explorer static site. |
| [`create-testnet.sh`](create-testnet.sh) | Generate testnet config + genesis from scratch. |
| [`install-node.sh`](install-node.sh) | Older systemd-based installer (predates the community installer). |
| [`install-inference-node.sh`](install-inference-node.sh) | Inference-only node installer. |
| [`join-testnet.sh`](join-testnet.sh) | Add a node to an existing testnet. |
| [`eval-perplexity.sh`](eval-perplexity.sh) | Run perplexity evaluation against WikiText. |

## Quick reference

**I want to try the demo without installing anything:**
```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-demo.sh | bash
```

**I want to verify the network's most recent inference:**
```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh | bash -s -- --latest
```

**I want to join the network as a community node:**
```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh | bash
```

**I want to reproduce the factual benchmark:**
```bash
bash scripts/arc-bench.sh
```

**I'm the operator and a node went down:**
```bash
bash scripts/arc-health-check.sh           # see who's down
# Seeds run arc-self-heal as a systemd unit - they self-recover on drift
# or RPC silence without intervention. If you ever need to install or
# re-install the daemon on a seed:
bash scripts/install-self-heal.sh <NODE_IP>
```
