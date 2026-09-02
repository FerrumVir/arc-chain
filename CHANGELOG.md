# Changelog

All notable changes to ARC Chain are tracked here. This project follows
[semantic versioning](https://semver.org/).

> **Backfill note (2026-08-17).** Entries for v0.7.1 through v0.7.11 were
> reconstructed from `git log` in a single pass; this file had stopped at
> v0.7.0. They are terser than the hand-written entries below.
>
> **Release-topology caveat.** The version on the seeds, the version with a
> git tag, and the version with a downloadable binary are three different
> things right now:
> - **v0.7.10 and v0.7.11** were shipped **desktop-only** via
>   `release-desktop.yml`, so they carry Tauri bundles and **no `arc-node` CLI
>   asset**. v0.7.7 is the newest release with a CLI binary.
> - **v0.7.8 and v0.7.9 have no release and no tag.** The five non-NYC seeds
>   run a binary built out-of-band from branch `fix/v078-attestation-wire-compat`,
>   which was merged to main only on 2026-06-16 (f6bee03).
> - **Nothing on the live network runs v0.7.11.**

## v0.8.0 - Release-preparation snapshot (2026-08-31)

> **Tag-stable lifecycle note:** At this review cutoff, v0.8.0 was an
> unreleased recovery candidate and was not deployed or published. An
> immutable v0.8.0 tag intentionally retains that historical fact. Determine
> current publication and fleet status from the exact immutable release API
> evidence and the signed coordinated-rollout receipts, not from this changelog
> snapshot.

- Restores one checksummed release graph for headless Linux amd64/arm64,
  Intel/Apple-Silicon macOS, Windows CLI, and signed desktop bundles. The
  installer resolves one exact version, works without a display, and refuses
  downgrade or unverified replacement. Workspace, Tauri, desktop npm, and lock
  metadata are all pinned to v0.8.0 before a matching tag can publish.
- Splits unsigned builds, updater-payload signing, release-manifest signing,
  draft publication, and read-only server verification across exact-ID,
  digest-bound handoffs. Fresh protected signers run no repository program and
  expose keys only to per-boundary absolute executable allowlists; the Node
  signer runtime is exact-patch and byte bound. Publication never deletes a
  release after its publish PATCH is attempted and boundedly polls GitHub's
  eventually consistent immutable state before sealing evidence.
- Adds the typed `arc health` command and makes the installer use it for
  readiness, accepting only explicit JSON `ok` or `degraded` states. Install
  and upgrade now share a complete rollback transaction across binaries,
  configuration, identity, service definitions, and prior service state.
- Adds a blocking release/security harness: Rust format/check/Clippy/test
  coverage, GUI-free node boot gates for Linux x86_64 on Ubuntu
  22.04/24.04/26.04 and ARM64 on Ubuntu 24.04/26.04, deterministic desktop E2E,
  Tauri tests on every released desktop architecture, SDK packed-consumer
  tests, workflow/ShellCheck contracts, and staged plus working-copy secret
  scans.
- Replaces same-host determinism claims with hardcoded production-engine KATs
  covering CPU I8/I16, 1-versus-4 threads, whole-versus-three-way sharding,
  logits/KV/hidden-state hashes, and autoregressive output across ARM and x86.
- Makes sequential and BlockSTM execution use the same canonical ordering and
  state semantics; consensus timestamps and DAG attachments are verified and
  peer hints can no longer mutate authoritative state.
- Binds every persistent database to its authenticated genesis network hash,
  rejects unmarked legacy WAL reuse, requires production validators to exist
  in the canonical genesis accounts, binds native RPC to loopback by default,
  and leaves Ethereum RPC disabled unless an operator explicitly enables it.
- Replaces the abandoned bincode 1.x implementation with a bounded internal
  v1-wire-compatible facade, and narrowly patches Wasmer's derive crate to
  remove `proc-macro-error2` while preserving upstream provenance and valid
  generated code. Remaining upstream advisories stay blocking in the release
  gate; none are ignored.
- Introduces authenticated v3 community registration/heartbeat/claim/submit,
  exact model-ID routing, one-job worker capacity, bounded payloads/timeouts,
  independent 2-of-3 range verification, and five-of-six active-validator,
  replay-protected 2.5 ARC reward transactions. Stable policy, approval, job,
  receipt, and earnings endpoints bind evidence to the recovery epoch,
  validator set, transaction domain, exact model/input/output, and mined
  `0x25` receipt. Pending or rejected rewards are never reported as earned;
  projections fail closed unless policy, receipt history, and treasury support
  them. Stake-zero worker eligibility is explicit policy, not an installer
  promise.
- Separates RPC from P2P discovery and configures all six reviewed literal-IPv4
  HTTPS origins explicitly. The locked SHA-pinned Caddy 2.11.4 gateway requests
  publicly trusted Let's Encrypt IP certificates with the `shortlived` profile
  over HTTP-01, removing the shared `nip.io`/`sslip.io` wildcard-DNS dependency.
  Remote plaintext, credentials, URL paths, query strings, fragments, wildcard
  listeners, and port zero are rejected outside the deliberate local/dev escape
  hatch.
- Moves wallet transfer signing into Rust so the seed never crosses IPC,
  parses and formats ARC with exactly nine decimal base-unit precision, and
  treats sends and 1 ARC faucet claims as pending until the chain returns a
  successful mined receipt.
- Authenticates P2P session transcripts with TLS-exporter-bound Ed25519
  identities, enforces pinned certificates in strict mode, caps frames and
  decoders, and binds claimed identities to signed payloads.
- Hardens validator identity and rollout: validator key files are mandatory,
  legacy exposed keys are rejected, genesis/release contracts fail closed, and
  the six-node v3 cutover is explicitly coordinated rather than auto-deployed.
- Adds an archive-bound recovery transaction around that cutover: a sealed
  freeze plan, two independently captured quarantine samples at least 120
  seconds apart, per-node stopped-writer/listener evidence, content-indexed
  legacy bundles, and separately verified Google Drive completion roots must
  all cross-bind before validator mutation. Every divergent legacy lineage is
  retained with an explicit canonical, non-canonical-fork, or unclassified
  disposition; recovery never rewrites those forks into one invented history.
- Adds a boundary/tool/source-set-bound late-fork interlock. All six recovered
  gateways must publish a fresh healthy status. A coherent legacy observation
  above the sealed public-height cutoff creates a persistent incident and
  forces the dashboard and explorer back to maintenance; it never auto-clears
  or promotes the observation into canonical history.
- Adds one-shot validator-vault rewrap and restore/install tooling. The
  passphrase-encrypted source vault is metadata-validated and re-encrypted to
  an operator-supplied CMS certificate without publishing plaintext. Restore
  is exact-main/pre-tag/profile bound, and remote key installation is
  create-only over pinned SSH only after authenticated offline-stop evidence
  v2 proves the legacy writers remain fenced.
- Makes community reward activation an authenticated genesis schedule plus an
  independent local issuance switch. An absent activation height disables tx
  `0x25`, and the issuance flag cannot override that absence. The checked-in,
  checkpoint-bound recovery genesis schedules activation at block `137146`;
  rollout readiness and the independent local switch still keep issuance
  fail-closed until the approved coordinated cutover.
- Reworks dashboard and explorer rendering for safe DOM insertion, honest
  liveness/retained-history semantics, coordinator-specific compatible worker
  capacity, visible inference/quorum/settlement evidence, actual on-chain
  balances, and a fail-closed six-replica maintenance interlock. Production
  dashboard CSS is compiled locally instead of executing the Tailwind
  development CDN.

At the 2026-08-31 source freeze, this candidate was not deployed or published,
and the public fleet remained split across old v0.7.2/v0.7.9 binaries. A valid
current deployment claim requires operators to have rotated compromised keys,
chosen a clean-genesis or checkpoint recovery policy, and completed the signed
rollout gate.

## v0.7.11 - 2026-06-15 (desktop-only)

- Removed the On-chain Tier 1 radio and the tier1 mutation, polling, and
  status panel from the desktop UI (`Inference.tsx` -348 lines,
  `Settings.tsx` -55). The feature was withdrawn rather than fixed: a
  caller-signed `InferenceRequest` is accepted with HTTP 200 by
  `/inference/onchain/submit` but never lands in a block, so escrow never
  opens and the committee never votes. See
  `docs/INFERENCE_TIER1_INVESTIGATION_2026-06-04.md`. (57ff20d)
- Desktop default `max_tokens` halved 32 → 16, noted as "~3 min vs ~6 min per
  request on the public testnet" — i.e. ~11 s/token. (d60632a)

## v0.7.10 - 2026-06-11 (desktop-only)

- Aligned the desktop with live chain state. Deliberately released without a
  tag push so `release.yml` would **not** publish `arc-node` binaries from a
  main that lacked the wire-compat fixes the seeds were running. This is the
  change that broke `install-community-node.sh`, which resolved
  `releases/latest` and then 404'd on a CLI asset that was no longer there.
  (281bdd0)
- Hid the on-chain inference submit path (tier-1 + paid escrow) in the desktop
  and coerced persisted `"onchain"` mode back to `"coordinator"`. (306ebb7)
- Documented the tier-1 inclusion failure. (fe1ed8d)
- Wrote the replicated-chain (Model 1, full re-execution) implementation plan.
  Not started as of 2026-08-17. (99cf84f)
- Investigated and documented that the testnet seeds are **independent
  chains**, not one network. (95a9686)

## v0.7.8 / v0.7.9 - 2026-06-01 → 2026-06-04 (no tag, no release)

The binary the five non-NYC seeds actually run. Built from
`fix/v078-attestation-wire-compat`; merged to main 2026-06-16 (f6bee03).

- `InferenceAttestation` made wire-compatible with v0.7.2 validators — the
  reason NYC (still v0.7.2) can interoperate at all. Wire size pinned by a
  unit test. (24150b3, 5279838)
- `--community`: one-flag setup with auto-model-discovery, and auto-download
  of a sha-pinned Llama-2-7B GGUF. (dcab105, 397698e)
- Validator auto-shard via `/shards/join`, plus a canonical `model_id`
  function. (ff46e53) — note this is the path that can wedge a fully-covered
  public pipeline; see the safety rules in `CLAUDE.md`.
- Scalable model distribution: multi-source fetch, LFS pre-check, resume.
  (601194c)
- Sharded inference unblocked; smart-router timeout cut. (30b3113)
- INT16 enabled on shard-holders — `enable_i16()` now called after load so the
  I16 dispatch path is actually taken. (0d9d6a4, 613a232)
- `tier1_pending` restored across restarts; partial-local requests routed to
  the sharded path. (0d9d6a4)
- Context window doubled: RoPE 2048 → 4096, sharded output cap 256 → 1024.
  (96b87fe)
- `/inference/onchain/submit` accepts a caller-signed tx and reports
  diagnostics; inference-validator retries finalize on a stuck request. Neither
  made the transaction land in a block. (34e1fd0, 631e5b0)
- `rolling-upgrade.sh` pauses `arc-self-heal` per node and re-arms it after the
  health gate. (9dc7618)

## v0.7.7 - 2026-05-29

- Desktop Tier 1 routes to the public testnet seeds; the alpha VPS is retired.
  (37df67b)
- **Last release to ship `arc-node` CLI binaries** (linux-x86_64,
  macos-arm64, macos-x86_64, windows-x86_64.exe).

## v0.7.6 - 2026-05-29

Ten commits of wallet-host churn, which is worth reading as one story: the
desktop wallet was pointed at alpha, then at 5 seeds, then at LAX, then back to
localhost, then at the live testnet. It settled on pinning a single seed —
`docs/TESTNET_STATE_DIVERGENCE_2026-06-03.md` later explained why aggregation
across seeds could never work. (800d828, 6226342, b05bed5, 8e151de, bf1e14c,
4876820, f040d23, 671cf03)

- "Option C": beneficiary credit for tier-1 `InferenceAttestation`. The
  `beneficiary` wire field was subsequently marked `#[serde(skip)]` after it
  partitioned the validator set on 2026-05-29. (9d75170)
- arc-state: speculative `success=false` routed to unresolved — a BlockSTM fix
  for tier 1. (4cff9ea)

## v0.7.5 - 2026-05-28

- arc-node posts an `InferenceAttestation` after every Tier 1 vote. (28e18dc)
- Desktop wallet (balance / faucet / earnings) routed to alpha rather than
  127.0.0.1. (671cf03)

## v0.7.4 - 2026-05-28

- Desktop actually invokes the Tauri auto-update and relaunches after install —
  previously the update was downloaded but never applied. (b3014b9)
- README download links bumped v0.6.0 → v0.7.3. (3218d8f)

## v0.7.3 - 2026-05-26

- Tier 1 on-chain inference gains load balancing and error handling. (9cbbaab)
- Desktop tier1 pinned to the alpha solo node (v0.7.2) while the
  multi-validator inclusion bug stayed open. (295ee6f)

## v0.7.2 - 2026-05-20

- **Tier 1 on-chain inference, Phase A.** (73eaee9)
- Inference timeout raised 3 s → 120 s, and a false `503 Pipeline gap` fixed.
  The 3-second timeout had made sharded inference unusable. (4acbcbb)
- Connecting UX: syncing banner + onboarding progress. (0510fa4)
- Alpha VPS hardening and UX polish. (34a393f, 9308d9d)
- At the 2026-08-31 source freeze, this was the version **NYC still ran**.

## v0.7.1 - 2026-05-12

- **Validator-signed `FaucetClaim` TxType**, gated behind `FAUCET_V2_ENABLED`
  and then defaulted on. This is why faucet credits propagate while
  coordinator-minted attestations do not. (e0ad962, 548d3c3)
- Signer-nonce check dropped from the `FaucetClaim` executor. (564468c)
- `StateDB.validators` seeded from genesis at startup, and
  `seed_genesis_validators` clears the set first. (4771d35, 40bab32)

## v0.7.0 - "Just be a node, and earn for real"

The community-worker system actually works for the first time. Pre-v0.7
the seed coordinators advertised a work queue that was never wired up:
NodeState's `community_work_tx`, `_queue`, and `_results` were
declared but always set to `None`, so every `/community/claim_work`
poll returned 503 "work queue not initialized." Workers polled,
heartbeated, and ran the literal definition of doing nothing — which
matches every "12 peers connected, 0 attestations, 0 earnings forever"
report from v0.6 users.

### Architecture
- `rpc::serve()` now wires a real bounded mpsc (256 slots) for
  community work. `claim_work` long-poll, `submit_work` round-trip,
  and the seed-side dispatcher all share that channel.
- `WorkItem` and `WorkResult` redesigned around whole-prompt routing
  (`job_id`, `input`, `max_tokens` → `output`, `output_hash`,
  `tokens_generated`). The half-built layer-shard community-work
  shape is gone; layer-shard remains a seed-to-seed primitive
  (`forward_shard`).
- `/inference/run` is now a smart router: prefers a community worker
  when any are online, falls back to the seed's local model otherwise.
  EWMA latency is recorded per worker for future scoring (task 4).
- Workers sign `InferenceAttestation` (tx 0x16) with their validator
  key and submit alongside results; the seed verifies and inserts
  to mempool. Earnings are now actual on-chain credits.
- `GET /worker/earnings/:address` reads chain state directly. The
  desktop's pre-v0.7 client-side synthesis from
  `/inference/results` (which conflated "this seed's local cache"
  with "this address's network earnings") is gone.

### Desktop
- Lite-mode banner rewritten as honest "Client mode": tells users
  they're connected as a client, will not earn ARC until peers > 0,
  and offers a "Reset peer state & rebootstrap" button that wipes
  `<data_dir>/known_peers.json` and restarts. The most common cause
  of "I had peers, then I restarted, now stuck" is a stale dial cache,
  and this fixes it in one click.
- New Tauri command: `reset_peer_state`.

### Removed
- **`scripts/community-gateway.py`** — Python sidecar from v0.5.2
  on port 3001. All functionality is now in arc-node on port 9090.
  The legacy port 3001 fallback in the worker loop and registration
  script is kept temporarily for the rolling-upgrade window; remove
  in v0.7.x once all seeds run v0.7.0+.

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
- **Community gateway sidecar (REMOVED IN v0.7.0)** (`scripts/community-gateway.py`) - Python
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
