# Working in arc-chain

Checked against 8949910b (v0.8.20) on 17 Nov 2026

This is the guide for anyone who changes this repository, person or coding
agent. Line numbers are at commit 8949910b. They move with every change, so
search for the symbol and re-check the line before you quote it.

## Read this first

1. **Read the code, not the prose docs.** Most prose docs describe older
   versions. [Docs status](#docs-status) lists the ones that are current.
2. **Cite `path:line` at a named tag or commit** for every claim about
   behaviour, and re-check the line at that ref before you quote it.
3. **Keep the [invariants](#invariants-you-must-not-break).** A change that can
   alter one output bit, hash, signature or quorum is a protocol change, not a
   refactor.
4. **Run the checks that cover your change** before you open a PR.
   [Build, test and lint](#build-test-and-lint) marks the heavy ones.
5. **Never commit secrets:** keys, tokens, wallet phrases or `.env` files. CI's
   secret scan fails on any it finds (`.github/workflows/ci.yml:88-97`). Run
   it with `--worktree` before you commit; by default it scans only `HEAD`.
6. **Update this file in the same PR** when you change behaviour it describes.
7. **Write no code comments.** Make the code say it with a name, a type or a
   smaller function. Put the reason for a change in the commit message or PR.
   `python3 scripts/ci/no_comments.py` fails CI on any comment. It allows only
   tool directives, license lines, clap `///` help text, PowerShell
   comment-based help at the top of a script, and a Python module docstring
   that the module reads through `__doc__`. It skips `vendor/`, `docs/`,
   `papers/`, data such as genesis, seed and non-code fixture files, six
   SHA-256-pinned files, and minified or source-map output.
8. **Turn corrections into checks.** When a review finds a mistake that can
   happen again, add a test, lint rule or script that fails on it. A sentence
   in this file is the last resort, not the fix.

## What ARC is

ARC is a chain whose staked validators order transactions with a
Mysticeti-inspired DAG consensus (`crates/arc-consensus/src/lib.rs:1-14`) and
run AI inference on an integer-only engine that gives bit-identical output on
every platform (`crates/arc-inference/src/integer_engine.rs:1-8`). Validators
hold model layer ranges and check results, while stake-0 community workers,
including the desktop app, run whole inference jobs
(`crates/arc-node/src/main.rs:435-446`).

## Repo map

The Rust workspace has 20 members (`Cargo.toml:4-24`). `desktop/src-tauri` and
`vendor/stwo` are excluded (`Cargo.toml:3`); the desktop builds from its own
manifest.

| Path | Contents |
|---|---|
| `crates/arc-node` | The node binary: CLI and startup (`src/main.rs`), HTTP RPC (`src/rpc.rs`, `src/rpc/`), consensus driver (`src/consensus.rs`), twin execution rules (`src/twin.rs`) |
| `crates/arc-consensus` | DAG consensus engine, validator sets and quorum (`src/lib.rs`), checkpoints (`src/security.rs`) |
| `crates/arc-state` | Accounts, WAL, snapshots, light client, recovery |
| `crates/arc-types` | Transactions, blocks and shared protocol constants |
| `crates/arc-inference` | Integer inference engine, canonical kernels, golden vectors, model loading |
| `crates/arc-crypto` | Signatures, hashing, VRF, threshold crypto, STARK proofs (Stwo only with the `stwo-prover` feature) |
| `crates/arc-bincode` | The bincode-v1 wire-format facade that hashes and signatures depend on |
| `crates/arc-net`, `crates/arc-mempool` | QUIC transport and peer protocol; the transaction pool |
| `crates/arc-vm` | WASM and EVM execution, precompiles |
| `crates/arc-gpu` | GPU and SIMD kernels (WGSL, Metal, CUDA) |
| `crates/arc-assign` | Work assignment: leases, placement, assignment certificates |
| `crates/arc-channel`, `crates/arc-cli` | Off-chain payment channels; the command-line client |
| `crates/arc-bench` | Benchmarks; some binaries need `--features benchmark-tools` |
| `crates/arc-legacy-bridge` | One-time launcher that moves a v0.7 install onto a pinned v0.8 stake-0 node |
| `crates/arc-olm` | Deterministic reasoning by typed DAG search |
| `agents/`, `relayer/` | Example agents; the Ethereum bridge relayer |
| `vendor/` | Vendored and patched dependencies (`stwo`, `wasmer-compiler`, `wasmer-derive`, `shared-buffer`, `third_party/glib-0.18.5`) |
| `desktop/` | Tauri desktop app (TypeScript UI; Rust side in `desktop/src-tauri`) |
| `shared/` | Code both node and desktop use (`keep_awake.rs`) and front-end network config |
| `dashboard/`, `explorer/`, `wallet/` | Static web front ends |
| `sdk/`, `sdks/` | TypeScript and Python client SDKs |
| `contracts/`, `test/` | Solidity contracts and their Foundry tests (`foundry.toml`) |
| `scripts/` | Release, recovery, CI and helper scripts; `scripts/ci/` holds what CI calls |
| `tests/` | `release/` contract tests, `integration/`, `legacy-bridge/` |
| `deploy/` | systemd unit, cloud-init, nginx config and setup scripts |
| `docs/` | Prose docs, mostly historical; see [Docs status](#docs-status) |
| `papers/`, `faucet/`, `release/` | Whitepaper sources; the faucet service; the release signer allow-list |
| `genesis.toml`, `config.toml`, `testnet/` | Genesis, a sample node config (read only with `--config`), seed lists |

## Finding your way in the big files

Do not read these files whole. Search them, then read the whole function you
need and its callers: `git grep -n -e 'fn name' 8949910b -- path`.

| File | Lines | `mod tests` at |
|---|---|---|
| `crates/arc-node/src/rpc.rs` | 34,312 | 20321 (and 34044) |
| `scripts/recovery/archive-node.sh` | 22,367 | |
| `crates/arc-state/src/lib.rs` | 17,423 | 11187 |
| `scripts/recovery/recovery_rollout.py` | 15,494 | in `scripts/recovery/test_recovery_rollout.py` |
| `crates/arc-node/src/main.rs` | 13,976 | 10349 |
| `crates/arc-inference/src/cached_integer_model.rs` | 10,137 | 7077 |
| `desktop/src-tauri/src/node_manager.rs` | 8,833 | 5457 |
| `crates/arc-node/src/consensus.rs` | 8,172 | 5437 |
| `desktop/src-tauri/src/commands.rs` | 8,022 | several, the first at 76 |
| `crates/arc-state/src/recovery.rs` | 7,139 | 3970 |
| `crates/arc-consensus/src/lib.rs` | 6,831 | 4043 |
| `crates/arc-node/src/native_inference.rs` | 6,503 | 3540 |

- **`rpc.rs`.** Routes are registered in `serve` (`rpc.rs:2367`), lines
  2683-2916. `NodeState` is at 431 and is built by `build_node_state` (1869).
  List the later sections with
  `git grep -n -e '^// ─── ' 8949910b -- crates/arc-node/src/rpc.rs`. Health,
  chain, block, transaction, validator, faucet, state-sync and ETH JSON-RPC
  handlers sit between 3109 and 6785 under `// ----` banners. Inference starts
  at 6847, the validator pipeline at 9845 and 11019, the community registry and
  dispatch at 13962 and 14563, shard join at 19301. The ETH JSON-RPC port has
  its own router in `serve_eth` (`rpc.rs:3052`).
- **`rpc/` submodules** (`rpc.rs:25-28`): `community_routing.rs` picks the
  worker for a single job, `teacher_forced.rs` is the fast verification path,
  `twin_dispatch.rs` runs twin execution and the demand pump. Tests for the
  first two are in `community_routing_tests.rs` and `teacher_forced_tests.rs`,
  included into `rpc.rs`'s `mod tests` (`rpc.rs:20322-20323`);
  `twin_dispatch.rs` keeps its tests inline (`twin_dispatch.rs:2160`).
- **`main.rs`.** `struct Cli` (`main.rs:57`) holds every flag with its doc
  comment. `run_arc_node` (6942) is the startup path. Offline operator
  subcommands are in `OperatorCommand` (633), run by `run_operator_command`
  (4691).

## Build, test and lint

The workspace uses nightly-2026-03-16 (`rust-toolchain.toml:12-14`). The
desktop uses stable (`desktop/src-tauri/rust-toolchain.toml:2`), but only when
run from `desktop/src-tauri` or with `RUSTUP_TOOLCHAIN=stable`, as CI sets
(`.github/workflows/ci.yml:483-486`). Pass `--locked`, as CI does. Format with
`scripts/rustfmt-workspace.sh`, not `cargo fmt --all`, which would rewrite
vendored code (`scripts/rustfmt-workspace.sh:4-8`).

**Start light.** Run the tests of the crate you changed, one command at a
time: `cargo test -p <crate> --lib --locked`. Kernels or the engine: add
`cargo test -p arc-inference --lib --locked golden`. Wire types:
`cargo test -p arc-types -p arc-bincode --lib --locked`. Consensus:
`cargo test -p arc-consensus --lib --locked`. Docs only:
`bash tests/release/documentation_contract_test.sh`. Run the workspace rows
below only when CI needs them, and `./scripts/ci_check.sh --quick` before
`make ci`.

Line numbers in this table are in `.github/workflows/ci.yml` unless another
file is named. **Heavy** means a whole-workspace build or a long suite: run one
at a time, and only the ones your change needs.

| Check | Command | Line | Heavy |
|---|---|---|---|
| Format | `./scripts/rustfmt-workspace.sh --check` | 184 | |
| Lint | `cargo clippy --workspace --all-targets --locked -- -D warnings` | 202 | yes |
| Compile | `cargo check --workspace --all-targets --locked` | 380 | yes |
| Unit tests | `cargo test --workspace --lib --locked` | 383 | yes |
| Node binary tests | `cargo test -p arc-node --bin arc-node --locked -- --test-threads=1` | 402 | yes |
| `candle` feature | `cargo test -p arc-inference -p arc-node --lib --features candle --locked` (clippy at 223) | 225 | yes |
| `benchmark-tools` feature | clippy, check and test with `--features benchmark-tools` | 246-250 | yes |
| Integration and doc tests | `cargo test --workspace --test '*' --locked -- --test-threads=1`, then `cargo test --workspace --doc --locked` | 440-442 | yes |
| Golden vectors | `ARC_GOLDEN_REQUIRE_VECTORISED=1 cargo test -p arc-inference --lib --locked golden -- --nocapture` | `golden-vectors.yml:50`, `122` | yes |
| Release contracts | `bash tests/release/run.sh` | 64 | |
| Doc contracts | `bash tests/release/documentation_contract_test.sh` (CI runs it from `desktop/`) | 468 | |
| No code comments | `python3 scripts/ci/no_comments.py`; its tests are in `scripts/ci/test_no_comments.py` | `no-comments` job | |
| Secret scan | `bash tests/release/current_tree_secret_scan.sh --worktree` before you commit. CI runs it bare, which scans `HEAD` only. It downloads a pinned Gitleaks (`tests/release/current_tree_secret_scan.sh:17-24`, `58-74`) | 97 | |
| Shell | `bash -n` on every `*.sh` under `scripts`, `deploy`, `tests/release`, `tests/integration`; `shellcheck -S warning` on those plus `install.sh` | 749-792 | |
| Supply chain | `bash scripts/ci/run-cargo-deny.sh <manifest>` for each manifest listed | 733-736 | |
| Solidity | `forge fmt --check`, `forge test`, `forge build --sizes` | 813-823 | |
| Desktop | in `desktop/`: `npm ci`, `npx tsc --noEmit`, `npx playwright test --config playwright.gate.config.ts` | 466-469, 710 | yes |
| Desktop Rust | `RUSTUP_TOOLCHAIN=stable cargo test --manifest-path desktop/src-tauri/Cargo.toml --all-targets --locked` (or `cargo +stable ...`, `Makefile:82`) | 486, 671 | yes |
| Script suites | `python3 -m pip install "blake3>=1.0" "numpy>=1.26"`, `bash scripts/ci/run-script-suites.sh`, `node explorer/test-localdev-native.mjs`, then in `desktop/`: `npm ci && npm run test:unit` (`make test-scripts`, `Makefile:90-94`) | 963-972 | |
| Front ends and SDKs | `npm test` in `dashboard/`, `node explorer/test-contract.mjs`, SDK jobs | 151-165, 833-930 | |

`make ci` runs `./scripts/ci_check.sh --full` (`Makefile:97-98`), the local
gate for most Linux CI jobs. It does not run the Solidity (`forge`),
cargo-deny or script-suite jobs (`scripts/ci_check.sh:219-255`); run those
rows above when your change touches them. It is the heaviest command here.
`--quick` drops the secret scan and the integration, doc, desktop and SDK
checks, but still runs the workspace compile check, three clippy passes and
the unit tests (`scripts/ci_check.sh:219-242`), so it is heavy too.
`make test` also runs the `candle` unit tests
(`Makefile:58-60`). `make bench` fails as written, because its binary needs
`--features benchmark-tools` (`Makefile:133-134`;
`crates/arc-bench/Cargo.toml:40-43`).

### Running things safely

- A node started without `--peers`, `--seeds-file`, `--config`,
  `--shard-hosts` or a community RPC URL has no peers and nothing to register
  with (`crates/arc-node/src/config.rs:322-331`, `512-519`;
  `crates/arc-node/src/main.rs:7101-7107`, `7309-7321`). A stake-0 node is in
  community mode and registers with every configured community RPC URL
  (`main.rs:9139-9140`), including any in `ARC_COMMUNITY_RPC_URLS`
  (`main.rs:128-139`). Pass `--no-community` to be sure (`main.rs:568-580`).
- Any stake-0 node started without `--model` (stake 0 is the default) looks
  for the pinned model, about 4 GB, in a few standard paths and loads it if
  found (`main.rs:4807-4818`, `7058-7059`). Pass an explicit small test model,
  or run where none exists.
- `--community` also downloads that model when none is on disk
  (`main.rs:7063-7065`).
- Do not run `make join`, `make inference`, `make inference-node`,
  `make stats` or `make health`. They join or query a public network
  (`Makefile:109-120`, `137-142`).
- POST endpoints do real work. `/faucet/claim` submits a transfer to the
  mempool (`rpc.rs:4332`, `4535`). `/inference/run_sharded` runs the validator
  pipeline and signs an attestation with the node's validator key
  (`rpc.rs:12771`, `13064`, `12464-12466`). Even `GET /community/list` writes:
  it deletes expired registry entries (`rpc.rs:14536-14538`).

## Invariants you must not break

- **Integer-only inference.** The consensus inference path uses i64
  fixed-point arithmetic and no floats
  (`crates/arc-inference/src/integer_engine.rs:1-8`). Clippy warns on float
  equality for this reason (`Cargo.toml:176`).
- **Fast paths compute the same value.** The SIMD kernel returns the scalar
  kernel's exact value or refuses the input
  (`crates/arc-inference/src/canonical_simd.rs:1-8`). Batched prefill cannot
  change a value (`crates/arc-inference/src/canonical_prefill.rs:9-22`).
- **Golden vectors are fixed answers.** Expected tokens and BLAKE3 digests are
  committed in `crates/arc-inference/tests/fixtures/integer_inference_kat.json`
  and never recomputed from a second run
  (`crates/arc-inference/src/golden_vectors.rs:1-7`). CI checks them on five
  runner types (`.github/workflows/golden-vectors.yml:41-45`).
- **Wire format.** Hashes, signatures, WAL records, snapshots and peer messages
  commit to bincode-v1 bytes (`crates/arc-bincode/src/lib.rs:1-8`). The
  `bincode` dependency key points at that facade (`Cargo.toml:64-67`).
  Wire-compatibility tests pin byte layouts, for example
  `inference_attestation_body_wire_compat_v072`
  (`crates/arc-types/src/transaction.rs:2814`).
- **Quorum.** Quorum is the strict supermajority
  `total - (total - 1) / 3` (`crates/arc-types/src/lib.rs:26-38`), used by
  validator sets (`crates/arc-consensus/src/lib.rs:273`, `322-325`) and
  light-client finality (`crates/arc-state/src/light_client.rs:70-80`).
- **Membership.** Voting membership and power come only from the approved
  genesis or checkpoint. A peer's advertised stake never changes them
  (`crates/arc-node/src/consensus.rs:2486-2489`; `main.rs:75-84`).
  Checkpoints verify only against an externally trusted validator set
  (`crates/arc-consensus/src/security.rs:247-311`).
- **Commit rule.** A block commits under the two-round rule
  (`crates/arc-consensus/src/lib.rs:8-14`, `2144`). Only the round leader's
  block becomes canonical (`crates/arc-node/src/consensus.rs:1046-1048`).
- **Community results.** A result is accepted only when 2 distinct validator
  signatures agree on every layer range (`rpc.rs:6870-6876`, `15302-15336`).
  A reward needs 5 approvals from a fixed 6-validator committee, and issuance
  fails closed for any other set size
  (`crates/arc-types/src/transaction.rs:887-891`). Twin execution changes no
  consensus rule (`crates/arc-node/src/rpc/twin_dispatch.rs:5-8`).
- **Crypto code.** Do not rewrite crypto hot paths to satisfy style lints; a
  one-bit difference would split consensus
  (`crates/arc-crypto/src/lib.rs:1-4`). The allowed-lints list is deliberate
  (`Cargo.toml:182-205`).
- **Test-only features.** `benchmark-tools` exposes predictable signing keys
  and must never reach a default or release build
  (`crates/arc-crypto/Cargo.toml:10-12`; `crates/arc-node/Cargo.toml:20`).
  Default feature sets are empty (`crates/arc-node/Cargo.toml:15`).
- **Fail closed.** `POST /inference/onchain/submit` always returns 503 until
  authorization is bound end to end (`rpc.rs:2794-2798`, `7844-7855`).

## How work and sharding flow

The full account is [docs/HOW-SHARDING-WORKS.md](docs/HOW-SHARDING-WORKS.md).

`crates/arc-node/src/community_sharding.rs` supplies a bounded community
layer-pipeline coordinator with expiring leases, fixed partial-layer tiles,
session ownership, and idle-worker reassignment. Ready transitions rebalance
idle workers toward uncovered or underrepresented tiles without reducing the
current complete-pipeline floor; rejoining identities enter the current fairness
sequence. Bounded capacity-aware reassignment can first move an idle replacement
to free a stronger worker for a missing range. An exhaustive small-state check
compares convergence with feasible pipeline capacity and protects busy workers,
ready capacity, and lease acknowledgement fences. Its outputs require independent
verification and cannot authorize rewards. Its integer-model tests exercise
different partial models, including a 100-identity fairness simulation. This
simulation is not evidence of 100 connected operators. The module alone does not connect desktop workers
or replace the existing whole-job runtime described below.

`--community-shard-coordinator` enables signed lease, heartbeat, claim, submit,
and release routes under `/community/shards/`. Workers must register the
`shard-inference-v1` capability, an exact model commitment, and the canonical
execution profile. Whole-model and shard capabilities cannot be combined.
Requests bind the worker identity, coordinator, network domain, timestamp, and
nonce. Shard mutations use a separate bounded replay cache with a 30-second
clock window; ordinary-worker and validator authentication keep their existing
limits. Expiry uses a queue and per-identity counts. The coordinator requires a
loaded canonical model; it is off by default.
The status route reports verified dispatch available only with the enabled
coordinator, canonical model, and an independent validator verification plan.
`POST /inference/run_community_sharded` owns and cancels its shard session, checks
its exact result digest, coverage, token limits and EOS, then uses the existing
validator quorum verification before returning output. Public shard admission requires
an idle complete pipeline. Requests may overlap two additional verification slots,
while the configured overall cap remains 24 by default and at most 64.
Independent verification alone holds the shared public compute permits; queued
verification and shutdown stay bounded. Rewards
remain disabled for these multi-worker results.
Structured phase timing records shard execution, verification permit wait, and
quorum verification under the public coordinator boot nonce and session sequence.
The events contain no prompt, output, tokens, or keys. They record returned awaits;
an interrupted future can leave an incomplete trace, which must not be counted as
zero-duration work or a successful request.
Coordinator abort events include the same HTTP status returned to the caller. A
captured-event regression fixes the allowed field set and rejects private failure
details or model data in timing records.
This transport alone does not activate desktop nodes.
`--community-shard-worker` integrates the pinned partial worker into a managed
stake-zero node while preserving local RPC and the shutdown durability barrier.
It excludes whole-model and validator roles, requires explicit model and network
pins and a memory budget, and defaults to one compute thread. Existing local
recovery domains must match the supplied pin. Stop closes worker admission before
the lifecycle waits for drained tasks and acknowledges the WAL receipt.
Integrated workers retry exhausted transient connection, lease-reacquisition
and registration-recovery budgets with bounded, cancellable backoff while
retaining lease fences. After a lost release, an exact remembered prior
assignment is released again rather than rebound or accepted as new work.
Regressed assignments and fenced coordinator boots remain invalid. Integrated
partial workers acquire the shared update-drain permit before claiming work
and hold it through execution and submission retries. A closed gate suppresses
new claims while heartbeats and cancellation continue. Authentication, model and
protocol errors still stop the worker; the local RPC remaining up is not proof
that the worker is serving.
Stake-zero TCP `--full-integer-worker` and `--community-shard-worker` launches
with an explicit canonical GGUF model (excluding validators, chain
participation, shard holders, coordinators, native inference runtime,
`--tokenizer-only`, `--enable-i16`, `.arc-int8`, and `--rpc-unix`) bind their
loading RPC listener after the pre-bind barriers (signals, CLI/role checks,
data-dir lock, desktop shutdown watchers, identity, fast model file and
bounded GGUF header checks (including unverified header-only
`CanonicalShardMetadata::from_path` geometry and admission validation for
partial workers before binding, with mandatory full-file BLAKE3 verification
retained afterward), and `StateDB`/WAL replay, `--enable-community-rewards-v1` activation-height
pre-validation, and recovery-domain verification) and before blocking canonical
GGUF hashing and INT8 weight preparation (`crates/arc-node/src/worker_startup.rs`).
While model preparation runs, `GET /health` returns HTTP `200` with
`status: "loading"`, `startup_phase: "loading_model"`, a non-ready
`degraded_reason`, and `Connection: close`, while `/inference/readiness`,
`/community/worker/status`, non-`GET` methods on those routes, and all other
routes fail closed with HTTP `503`, `Retry-After: 5`, and `Connection: close`.
Preparation honors shutdown cancellation at chunk, header, tensor and layer
boundaries (`from_path_cancellable`, `verify_unchanged_cancellable`,
`CanonicalShardMetadata::from_committed_artifact_cancellable`,
`load_cached_model_canonical_i8_cancellable`), revalidates regular model paths
before full-worker open, and always joins the blocking task before bounded
supervised listener stop, watcher join, WAL sync, receipt acknowledgement, and
data-dir lock release; only cancellation with `shutdown_requested == true` and
a cleanly drained listener (no remaining live connections/handlers or stop
errors) exits cleanly and acknowledges a desktop shutdown receipt, while fatal
early-startup aborts retain the original error with cleanup context. On fatal
post-bind RPC errors without a clean shutdown request (including pre-handoff
`handoff_router` errors and unexpected server task returns or panics), the RPC
server signals its owned lifecycle shutdown sender to drain all RPC-owned
runtime tasks and waits for the inference compute barrier without aborting
active compute (while all fallible RPC state validations and settlement replay
run before spawning tasks), and the node then broadcasts shutdown, drains any
ETH RPC task, owned watcher/worker tasks, and consensus writers first, and runs
the final WAL durability sync while preserving the fatal error and never
acknowledging a clean desktop shutdown receipt. On success, the full RPC router replaces the loading router atomically on the same
bound TCP listener without a close/rebind gap, and outbound worker registration
or job claims begin only after preparation completes. Validators, non-eligible
roles, and `--rpc-unix` listeners keep the existing post-initialization
`rpc::serve` bind order unchanged.
Early, ordinary, and ETH TCP RPC listeners share a platform-aware binding helper:
Unix permits reuse after closing a socket; Windows never enables address reuse
that would allow a second process to bind an occupied port.
The offline `community-shard-worker --check-header-only` command returns bounded
header geometry and memory admission with `artifact_verified: false` and the
expected model pin. It neither hashes the complete artifact nor loads keys,
weights, or network state. It cannot authorize inference; the launched worker
still verifies the complete artifact before registration or execution. The
existing `--check-only` command retains full artifact verification. Both check
modes omit the worker key requirement and are mutually exclusive.
An ordinary cleanly stopped desktop node can prepare an unattended update only
with launch-on-start disabled. The idle path takes a lifecycle fence, rechecks
the data lock and never signals detached or legacy nodes. Contention remains
retryable; pending recovery and manual-stop intent are preserved. Successful
preparation cancels startup retries, while Busy leaves them intact. The existing
fenced durability-repair path retains its separate recovery behavior.
All desktop update schedules use the `UpdateDeadline` wrapper for wall-clock
deadlines and callback identity. Native ticks recover suspended startup,
periodic, failed-check, install and shutdown-reconciliation waits exactly once.
A manual reconciliation wait still recovers with automatic checks disabled.
Disabling, disposing or replacing a schedule invalidates its queued callbacks.
Focused regressions and a reference-model check exercise stale delivery,
same-clock replacement and callbacks scheduling successors without a browser
or node. A source check prevents controller timers bypassing the wrapper.
The desktop derives partial-worker mode from existing compute consent and typed
release pins, preserving managed identity and update rollback. The release pins
Singapore's verified coordinator identity, recovered domain and canonical model.
Eligible desktop workers use partial shards; explicit compute opt-outs and
unresolved shutdown recovery retain observer mode. An absent pin preserves the
whole-worker path for rollback tests.
Settings reads preserve saved compute choices and model selection. Memory limits
apply to the launch configuration without inventing a saved compute opt-out.
Desktop lifecycle and private-directory namespace ownership explicitly unlock
when released, so an inherited duplicate descriptor cannot retain an aborted
update's ownership. A duplicate closing later cannot release the next owner's
independently acquired lock.
Launches serialize the node guard before reading current saved settings, and a
newer lifecycle action cancels a pending launch. Settings writers release the
store guard before waiting for the node. Interrupted shutdown retains the child,
shutdown control and lifecycle ownership until a later stop completes. A ready,
consenting installation remains on the bounded reconciliation schedule so a
running observer can resume compute after recovery or model restoration. These
checks preserve a manually stopped node and use an already verified cached model
without requiring download conditions.
The rollout manifest's optional `gateway.community_sharding` boolean adds the
exact shard routes and coordinator flag only when enabled. Disabled rendering
preserves the sealed gateway bytes. Metadata and public requests are capped at
16 KiB, step submissions at 2 MiB, with a separate worker rate limit. The
validator IP restrictions and interlock remain required for every upstream.

`community_shard_executor.rs` executes only the assigned canonical partial
model with per-session KV limits, exact-retry caching, cancellation retention,
and lease/boot fences. It rejects full-model residency and non-owned weights.
Hidden activations are limited to ±2^30 in Q16 at coordinator and executor
boundaries; values outside that envelope are refused before arithmetic. The
coordinator follows the canonical legacy generation sequence, including BOS,
prompt prefill, repeating the final prompt token for first decode, and retaining
the EOS output token. The outbound `community_shard_worker` runtime registers a
signed shard identity, maintains leases during blocking model loads and execution,
and retries identical submissions using fresh request nonces. Shutdown drains
blocking work before release; reassignment drops the previous partial model before
loading the next. TLS coordinator identity and transaction domain are pinned by
the caller. `arc-node community-shard-worker` runs without chain state or inbound
listeners. It requires coordinator, recovered-domain and full-artifact identity
pins, a private worker key, and an explicit RAM budget. `--check-only` inspects
admission without keys, network registration or tensor loading. Run mode uses one
compute thread by default and drains work on shutdown. Its conservative admission
requires capacity for all shard roles because role-aware placement is not yet
implemented. The integrated desktop path described above selects this runtime for eligible
workers. Network tests use small canonical fixtures; they do not establish
deployed capacity or operator availability.

`load_cached_model_shard_canonical_i8` reads only its assigned layer weights
from a GGUF, without preparing optional quantizations. It validates a strict
partial range before reading tensor weights. Its GGUF tests execute two partial
models across prefill and decoding and compare tokens and logits hashes with
the full canonical engine. It does not define downloadable partial artifacts.
Validator range loading uses the same canonical-only preparation without transient
optional quantizations. Clamped terminal ranges retain their output head; golden
checks cover full, split and disjoint ranges against the canonical engine.

`canonical_shard_admission` inspects one bounded, captured GGUF header before
canonical config allocation. It checks geometry and estimates partial-model
residency, first-shard embeddings, last-shard output weights, temporary loading
buffers, metadata heap storage, and reserved KV bytes. Admission rejects a range
that exceeds the caller's explicit RAM budget. This estimate is conservative
payload accounting, not a guarantee of process RSS; artifact identity still
requires the full-file commitment. Worker timing and reconnect limits are bounded,
and active heartbeats run even under a continuous stream of work. A network test
exercises 100 signed worker identities using tiny canonical models; it does not
prove 100 independent live operators or production-model speed.

`arc-node recovery qualify-peer-state` opens an already copied,
disposable `peer-recovery-candidate-*` directory. Its hash-pinned manifest binds
the target signing record, genesis, state WAL, recovery domain, and raw finality
certificate. It replays persistent state and signed DAG records without opening
listeners, loading models, signing, or activating a validator. The command may
write inside the disposable candidate; it does not establish capture atomicity
or completeness of the operator's supplied prior signing high-water marks.
`recovery inspect-peer-capture` inspects a disposable copied DAG generation and
preserved target signing record to locate candidate certificate heights before
full WAL replay. It does not verify the state WAL, DAG signatures, finality, or
complete prior signing history; `qualify-peer-state` remains required.
Qualification requires both restored proposal and commit cursors above the
target's supplied prior signing bound, preventing resumed skip voting on old
rounds even when the proposal cursor is newer.
`scripts/recovery/wal_precopy.py` streams a bounded append-only WAL prefix into
a create-only candidate. Its receipt is explicitly unqualified: capture of the
remaining state, signed finality checks and full peer qualification are still
required. It neither stops a service nor replaces any live state.

`scripts/recovery/peer_capture.py` captures a bounded WAL tail and recovery files
under a short cgroup-v2 freeze with an independent parent-death thaw watchdog.
Its target publisher rejects stale destinations, verifies the whole pre-copy
before and after appending, and retains the advisory lock through durable
publication. Capture and publication receipts remain unqualified; finality and
both signing-cursor bounds must pass the offline qualifier before activation.
Recovery startup retains its verified replayed state and binds the WAL writer
to the exact opened file bytes and accepted sequence before returning it. It
rejects missing or changed WALs and preserves tail quarantine and native-context
restoration; it does not bypass replay, checkpoint roots, or signed DAG checks.

Normal `POST /inference/run` prefers an eligible complete community partial
pipeline before whole-worker dispatch. It preserves forced-local, public-demo,
and recovery-probe paths and forwards a selected partial pipeline's error
without a second dispatch. Readiness includes this capacity through the existing
v1 sharded flag, preserving its nine wire fields. Partial results retain their
verification and participant evidence without inventing rewards or settlement.
Desktop direct inference first checks the compiled partial coordinator through a
bounded, read-only, uncached readiness endpoint that binds coordinator, domain,
model and prompt capabilities. Its dedicated compute-only POST cannot change
wallet or receipt origins; explicit custom origins retain their existing path.
Sharding-enabled gateway generation admits this exact GET route with the same
interlock, method and resource bounds as shard status. Preflight failures may continue the existing route selection. Once a POST is
attempted, errors are terminal and cannot trigger another inference dispatch.

The optional `partial_shard` worker status reports loading, lease expiry,
reconnects, and partial steps separately from whole jobs. Expired ready or
computing leases clear their displayed range; partial steps do not count as
whole answers or rewards. Legacy whole-worker payloads omit these fields.
The desktop rejects unknown or contradictory worker payloads, displays partial
steps separately, and clears expired assignments. The Linux candidate workflow's
`desktop-ui` mode builds the frontend, checks state transitions and captures the
ready, computing, failed and stopped views in a single browser worker.

`scripts/qualification/run-recovery-peer-tests.py` runs the exact mandatory
recovery and sharding tests on a single build worker and rejects missing cases
or ignored tests. Use the `recovery-peer` mode of the Linux candidate workflow
when local resource limits prohibit compilation.

### Whole-model compatibility path

- Whole-model public jobs retain the two-permit compute limit described at
  `8949910b` (`rpc.rs:179`). The partial pipeline above has separate admission.
- A whole-model worker job is not split. One stake-0 worker runs every layer
  (`main.rs:435-446`, `8461-8467`, `9189-9206`); the job carries no layer
  range (`rpc.rs:14626-14664`).
- Twin execution, off by default, runs the same job on two workers and
  compares them (`main.rs:524-529`; `crates/arc-node/src/twin.rs:520-544`).
  It duplicates work; it does not divide it.
- Since v0.8.19 a single job is held for up to 1.5 seconds for the fastest
  measured worker among those waiting on that coordinator; after that any
  eligible waiting worker may take it. Every 4th decision tries an untried
  worker, if one is waiting
  (`crates/arc-node/src/rpc/community_routing.rs:186-205`, `239`, `280`).
  Public-request and public-demo twin legs use this same speed preference,
  among workers accepted by the independent-worker and diversity checks.
  A held leg reselects if its chosen worker becomes a sibling or stops waiting.
  Verified twin legs teach the router using coordinator-observed elapsed time
  per model position; unverified output and worker-reported timing do not.
  Caller twins dispatch immediately when no eligible waiter has a measured speed.
  An ineligible caller leaves held twin work available to other workers, and a
  declined twin clears its old availability claim until the next claim request.
  Background twin jobs retain their existing fairness and reserve rules.
- Validators hold layer ranges and verify results; each range needs 2
  agreeing signatures (`rpc.rs:6870-6876`). Prefill overlaps across hops,
  while decode passes every hop once per token (`rpc.rs:11989-11998`).
- `--auto-shard-join` is off by default and only plans. At v0.8.20 the
  planner first fills the largest uncovered gap, which need not be on the
  six-range grid (with no holders it is the whole model). When every layer is
  covered, it replicates an on-grid range or returns 409 (`main.rs:591-602`;
  `rpc.rs:19342-19355`, `19360-19374`, `19417-19423`).
  Join-plan coverage uses interval boundaries from the same fresh registry
  snapshot as placement; a caller's layer count never sizes a coverage bitmap.
- v0.8.20 desktops share by default on eligible machines. A new sharer gets a
  core cap of a quarter of the logical cores (at least one) unless a count is
  stored; an install already sharing keeps its cores
  (`desktop/src-tauri/src/commands.rs:981-1028`). They take whole jobs.
- More workers add capacity only up to each coordinator's limit. One job gets
  faster only when routing finds a faster worker.
- `/shards/auto_plan` returns a proposed layout without registering it. Its
  node addresses must be authenticated validator identities from signed shard
  announcements; malformed identities fail closed. A model hash is never a
  node address (`crates/arc-node/src/rpc.rs`, `compute_auto_shard_plan`).

The manual community-shard qualification workflows use temporary GitHub-hosted
workers and never count them toward an operator target. Artifact downloads resolve
the GitHub CLI from the caller's PATH before launching that exact executable with
the minimal child environment, so user-local CLI installations work without
inheriting proxy or debug settings. The scaling qualification
selects either the pinned Singapore or Amsterdam coordinator; all worker, transport,
readiness and cleanup checks use that same identity. Both selections require a quiet
coordinator with zero existing partial workers and sessions. Existing Singapore
artifacts retain their original identity and default selection.
The scaling qualification
binds twelve distinct machine boots and measures six sequential requests on six
workers, six requests at concurrency two on twelve workers, then six sequential
requests after the added workers leave. It records client timing, exact participants,
and server-reported verification; it does not prove unbounded scaling or independent
client verification. Phase artifacts bind workflow, attempt and cohort digest.
Cleanup proves each owned worker released its lease and deleted its temporary key.
The controller never retries an inference POST. These workflows require an exact
completed Linux candidate and a quiet coordinator; do not overlap them with a
coordinator restart or count synthetic fixtures as measured performance.
The bounded phase-evidence parser reads the production text formatter inside
journal JSON `MESSAGE` fields and joins only supplied, independently validated
response session keys. It preserves missing and aborted phases, rejects duplicate
events and malformed target records, and does not infer server concurrency or
request success. Its formatter fixtures are synthetic until a deployed capture
has been checked; the parser's regression tests run in the script-suite CI gate.
The optional overlap experiment keeps twelve temporary machines for all three
arms and offers concurrency two, four, then two, with six, twelve, then six
requests. Its cohort, phase, supervisor and summary schemas are distinct from
worker-count scaling. Every machine starts with the cohort and stops on the
verified completion marker. Client HTTP intervals remain separate from server
phase timings; neither proves server admission concurrency. Phase reports first
snapshot and validate the bounded controller files, then correlate supplied
journal records without publishing raw logs or treating them as authenticated
request-success evidence.

## Releases and versions

The checked-in candidate is 0.8.28; the published release remains
0.8.27. v0.8.27 ships under `desktop-update-existing-validators-v1`, while
validators remain on v0.8.26. The headless installer defaults `--worker-mode`
to `auto`. With a model it qualifies the pinned community partial-shard mode
through the node's offline `community-shard-worker --check-only` command. The
qualification requires the desktop's nominal 16 GB floor, available-memory
headroom, six admitted layers and a bounded working-set budget. Memory credit
comes only from a matching managed worker that the transaction will stop.
Ineligible `auto` hosts fall back to whole-model mode and reassess on later
updates; explicit `full` persists; `shard` refuses failed qualification before
stopping the old service; no model remains observer-only. The separate
`worker-policy.conf` records configured and resolved modes, model, policy
revision and the running installer's digest. Same-version `--update-only`
reconciles missing or stale policy and can promote automatic whole-model
fallbacks to `shard`, while unchanged policy is a no-op and downgrades remain
refused. Older scheduled installers replace `arc-installer` with the signed new
installer first and reconcile policy on the next scheduled cycle; `--no-service`
and `--no-auto-update` installs have no `arc-installer` and must rerun a
verified `install.sh` manually. The policy participates in service rollback.
Root-owned staging streams bytes to the target user for user-owned installs,
while that user's transaction snapshots stay in a separate private directory;
the release staging directory remains private to the installer. Signed release
verification, legacy migration fences and data preservation remain required.
Installer regressions run in the remote release-contract gate for this release.

The macOS bundle sealer reads the release version from the repository Tauri
configuration and rejects mismatched app identity or version before modifying
packages. Archive and DMG verification use the same captured version.

- The version is in `Cargo.toml:28`, `desktop/package.json:4`,
  `desktop/src-tauri/tauri.conf.json:4` and `desktop/src-tauri/Cargo.toml:3`
  (0.8.20 at 8949910b).
- List tags by date with `git for-each-ref --sort=-creatordate refs/tags`. An
  older pre-release tag sorts above v0.8.x under version sort.
- `CHANGELOG.md` is newest first. Its v0.8.28 entry is a release-preparation
  snapshot dated 2026-11-28 and makes no publication or fleet-activation claim.
- `.github/workflows/release.yml` runs on a `v*.*.*` tag push or a manual
  dispatch (lines 8-12). The current procedures are
  `scripts/release/EXISTING-CHAIN-UPDATE.md` and `scripts/recovery/README.md`.
- The doc contract test pins the published version at 0.8.27 and the candidate
  at 0.8.28, pins the exact published installer digest, requires the
  `desktop-update-existing-validators-v1` release profile, and keeps the actual
  validator baseline at v0.8.26. Active install links change only after
  immutable publication is verified; historical version observations remain
  dated. The contract checks the current-status heading link, enforces headless
  `--worker-mode` (`auto`, `full`, `shard`), same-version `worker-policy.conf`
  reconciliation, and explicit `--worker-mode full` in the whole-job reward
  walkthrough, fails if current docs claim `--model` is always whole-model, and
  keeps partial-shard rewards disabled. Each active install guide must state
  that this update profile refuses legacy v0.7 migration; a separately
  qualified cutover release with authentic cutover and retirement evidence is
  required.
- JavaScript audit gates use `scripts/release/npm-audit-gate.py` and the
  dated `npm-audit-exceptions.json` policy; see the dependency audit policy in
  `scripts/release/EXISTING-CHAIN-UPDATE.md`. Every root advisory must match
  an unexpired exception or block at the caller's existing severity threshold.
- A PR can stay open after its change ships through an integration commit.
  Check the code at the tag, not the PR state.
- What changed since a tag: `git log --oneline v0.8.20..origin/main` and
  `git diff --stat v0.8.19 v0.8.20 -- <path>`.
- When a doc last changed: `git log -1 --format=%cs <ref> -- <doc>`.

## Docs status

| Status | Docs (last changed) |
|---|---|
| Current, checked against v0.8.20 | `AGENTS.md`, `CLAUDE.md`, `docs/HOW-SHARDING-WORKS.md` |
| Recent (Oct-Nov 2026), not re-checked line by line against v0.8.20 | `docs/LEGACY-BRIDGE.md` (2026-11-04), `docs/determinism-proof.md` (2026-11-02, a dated measurement record), `docs/self-hosted-runner.md` (2026-10-06), `scripts/release/EXISTING-CHAIN-UPDATE.md` (2026-11-14), `scripts/recovery/README.md` (2026-11-13) |
| Partly current: check details against the code | `README.md` (2026-11-02), `CHANGELOG.md` (2026-11-13), `INFERENCE_DETERMINISM.md` (2026-10-11), `desktop/README.md` (2026-11-13), `docs/twin-execution.md` (2026-11-04), `docs/GETTING_STARTED.md` (2026-11-04), `docs/HEADLESS_INSTALL.md` (2026-11-02), `docs/COMMUNITY-NODE-WALKTHROUGH.md` (2026-10-26), `docs/MACOS-PRETAG-COMMUNITY-CANARY.md` (2026-09-08), `agents/README.md` (2026-03-21) |
| Historical, not current guidance | `ARCHITECTURE.md`, `ALERTS.md`, `EVOLUTION-LOG.md`, `PLAN.md`, `docs/STATUS.md`, `docs/SESSION_HANDOFF.md`, `docs/SCALE_ARCHITECTURE.md`, `docs/DEMO-RUNBOOK.md`, `docs/VALIDATOR-FLEET-ROLLOUT.md`, `docs/PRODUCTION-RECOVERY-AUDIT-2026-08-26.md`, the dated investigations in `docs/`, `docs/TIER1_ONCHAIN_INFERENCE_PLAN.md`, `docs/TIER1_INFERENCE_UX.md` |

Any doc not listed, including the folders under `docs/`, has not been checked
against v0.8.20. Treat it as historical. Some historical docs still carry
sentences the doc contract test requires, so do not move or delete one
without updating `tests/release/documentation_contract_test.sh`.

Code comments were removed after 8949910b, and CI keeps them out. Citations
in this file that point at a comment, such as `integer_engine.rs:1-8`, are
read at that commit: `git show 8949910b:<path>`. Some of those comments were
stale, so check them against the code. Comments there that cite "CLAUDE.md
rule N" (for example `desktop/src-tauri/src/commands.rs:1698`) mean the
numbered rules in the
[CLAUDE.md snapshot at 8949910b](https://github.com/FerrumVir/arc-chain/blob/8949910bdfd079bdef18d6a41b62f6838acf60b7/CLAUDE.md).
The current `CLAUDE.md` tags each kept rule with its snapshot number. Clap
help text stays, and the `--shard-range` help example (`main.rs:468-470`) is
not on the six-range grid.

## Keeping this file current

- When you change a flag, default, invariant or CI command named here, edit
  the matching line in the same PR.
- When you re-check this file against a new commit, update the "Checked
  against" line and every line number that moved. Use `git grep -n` at that
  commit; do not estimate.
- When a historical doc is rewritten to match the code, move it to the
  current row and remove its banner.
- Keep this file under about 350 lines; put long explanations in `docs/`.
- After any doc edit, run `bash tests/release/documentation_contract_test.sh`.
  It pins exact sentences in several docs, including `CLAUDE.md`
  (`CLAUDE_GUIDE` in that test).
