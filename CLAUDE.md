# CLAUDE.md

Checked against 8949910b (v0.8.20) on 17 Nov 2026

**Do not use this file as current operator guidance.** It is a short pointer
for coding agents. Read [AGENTS.md](AGENTS.md) first. It is the guide to this
repository: layout, checks, invariants and which docs are current.

@AGENTS.md

The previous version of this file, a pre-recovery snapshot, is in git history
at [8949910b](https://github.com/FerrumVir/arc-chain/blob/8949910bdfd079bdef18d6a41b62f6838acf60b7/CLAUDE.md),
for history only. Code comments that cite "CLAUDE.md rule N" (for example
`desktop/src-tauri/src/commands.rs:1698`) mean that snapshot's numbered rules,
not this file.

## Safety rules that still hold

These rules come from that snapshot and are still true in the code. Each one
is tagged with its snapshot rule number. Line numbers are at 8949910b.

- **Keep `--auto-shard-join` off unless you mean it** (snapshot rule 3). It is
  off by default (`crates/arc-node/src/main.rs:591-602`) and runs only for a
  staked node with a model and no explicit range (`main.rs:7552-7558`). The
  planner never registers a node. It answers `"registered": false`, and work
  is routed to a range only after a signed `/shards/announce`
  (`crates/arc-node/src/rpc.rs:19425-19430`, `19463`). Prefer an explicit
  on-grid `--shard-range`.
- **Run community workers with stake 0** (snapshot rule 2). `--stake`
  defaults to 0 (`main.rs:75-84`). `--full-integer-worker` refuses any stake,
  shard range or auto-join (`main.rs:5989-6025`). Do not use
  `--shard-range 0:N` to force integer loading on a home worker: it also
  announces a validator shard (`main.rs:441-444`).
- **Treat POSTs as real actions, not probes** (snapshot rule 5).
  `/faucet/claim` submits a transfer to the mempool (`rpc.rs:4535`).
  `/inference/run_sharded` runs the validator pipeline and signs an
  attestation with the node's key (`rpc.rs:13064`, `12464-12466`).
- **`GET /community/list` is not read-only** (snapshot rule 7). It deletes
  expired registry entries (`rpc.rs:14536-14538`). A deleted worker's counters
  start from zero when it registers again (`rpc.rs:14422-14443`).
  `/workers/scoreboard` hides expired workers without deleting them
  (`rpc.rs:9402-9404`).
- **A restart clears in-memory evidence** (snapshot rule 1). The worker
  registry, recent inference results (512 entries, one hour) and
  `sharded_runs_total` live only in process memory (`rpc.rs:173-177`, `511`,
  `579`, `604`), as do twin tallies
  (`crates/arc-node/src/rpc/twin_dispatch.rs:41`).

The snapshot's rules about live networks are not repeated here. This file
does not describe what any network runs.

## Before you open a PR

- Run the checks in [AGENTS.md](AGENTS.md#build-test-and-lint) that cover
  your change. The heavy ones are marked there.
- Run the secret scan with `--worktree` before you commit:
  `bash tests/release/current_tree_secret_scan.sh --worktree`.
- After any doc edit, run `bash tests/release/documentation_contract_test.sh`.
  It requires the bold sentence at the top of this file
  (`tests/release/documentation_contract_test.sh:400`).
- If you change behaviour that AGENTS.md or this file describes, update them
  in the same PR.
