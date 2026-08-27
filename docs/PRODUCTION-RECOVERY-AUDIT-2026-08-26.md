# ARC production-recovery audit — 2026-08-26

This is a read-only evidence snapshot plus the contract of the unreleased
v0.8.0/v3 recovery candidate. It is not a deployment announcement. No seed was
restarted, upgraded, rekeyed, or otherwise mutated during this audit.

## What community reports correctly identified

- Public v0.7.10 and v0.7.11 were desktop-only releases. The desktop and CLI
  publishers had split, so those tags did not contain the headless
  `arc-node-linux-x86_64` asset that existed in v0.7.7. A GUI package is not a
  substitute for an EC2/VPS/SSH binary.
- Public v0.7.11 carried updater configuration, but the desktop application did
  not invoke the update lifecycle. It therefore did not perform automatic
  checks or present an install flow.
- Community workers had no demonstrated work or payment. The Aug 26 snapshot of
  `/community/list` reported `total_work_completed: 0` across the worker list.
- The public dashboard could not prove a shared chain or successful community
  settlement. A raw inference attestation was being confused with payment.

The release-packaging regression has direct evidence in the public asset
matrix; it is not attributed to parallel inference. Parallel execution is still
a high-risk correctness boundary, so the candidate adds sequential-versus-
parallel state/root equivalence tests and deterministic-order tests. Those gates
are evidence against a regression in the candidate, not a retrospective proof
of what forked the public fleet.

## Public fleet snapshot

Observed read-only on 2026-08-26 at about 15:06 CDT:

| Seed | Reported version | State height | Latest block age |
|---|---:|---:|---:|
| NYC | 0.7.2 | ~136,969 | ~157 seconds |
| LAX | 0.7.9 | ~127,188 | ~1,050 seconds |
| AMS | 0.7.9 | ~88,452 | ~125 seconds |
| LHR | 0.7.9 | ~51,422 | ~361,993 seconds |
| NRT | 0.7.9 | ~96,770 | ~361,993 seconds |
| SGP | 0.7.9 | ~97,591 | ~361,993 seconds |

At common height 50,000, the six reachable seeds returned six different block
hashes and six different state roots. The corrected dashboard repeated a
common-height comparison at 51,422 and again found six hashes and six roots.
This is a confirmed fork, not one replicated testnet. An advancing DAG round or
`status: ok` does not prove block production, finality, or shared state.

The snapshot will age. Re-run the same-height hash/root and latest-block-age
checks before making a current claim; never silently blend seeds.

## What v0.8.0 changes — and what remains blocked

The candidate unifies one release publisher for checksummed headless and
desktop artifacts: Linux amd64/arm64, Intel and Apple Silicon macOS, Windows
x86_64 CLI, plus the desktop packages. The installer is exact-tagged, preserves
identity, verifies checksums, refuses downgrade/incomplete releases, and has
rollback coverage. Ubuntu 24 and Ubuntu 26 no-display smoke tests are blocking.

The desktop updater now performs signed-manifest checks after startup and every
24 hours when enabled. Background checks never download or install an update;
the user confirms installation. Package-manager-owned `.deb` and `.rpm` files
remain package-manager upgrades.

The inference candidate streams the exact model artifact bytes through BLAKE3.
Only a completely loaded, every-layer model advertises capacity, and work is
eligible only when the request carries that exact artifact ID. A filename,
shape, model tier, peer count, or larger machine does not establish eligibility
or promise work.

Accepted community work requires authenticated 2-of-3 recomputation for every
layer range and token. Payment is deliberately separate:

- `InferenceAttestation` (`0x16`) is a computation claim and pays nothing.
- `CommunityInferenceReward` (`0x25`) carries the signed worker certificate and
  is the only community payment transaction.
- Authorization requires unique active-validator approvals covering strict
  greater-than-two-thirds of validator identities **and** active stake. Six
  equally staked validators require five approvals.
- Genesis protocol activation, the local issuance switch, approval collection,
  treasury funding, transaction validity, block inclusion, and a successful
  receipt must all hold.
- `/worker/earnings/{address}` counts only successful mined `0x25` receipts in
  that host's retained index. Pending, failed, pruned, or raw `0x16` records are
  not earnings.

The unreleased candidate now implements authenticated approval collection from
six explicit HTTPS community RPC origins. It does not derive RPC authority from
P2P peers, accept clear-text remote origins by default, manufacture approvals
from shard signatures, or expose a validator-signing oracle. Five distinct
validators must independently revalidate and sign the exact reward commitment,
and those approvals must also cover strict greater-than-two-thirds active
stake. A failure leaves the mempool, success counters, and earnings unchanged.
This code is not yet published or deployed, so it is not evidence that the
audited public fleet can issue rewards today. Projected rewards remain hidden
until the selected live coordinator reports issuance ready and has enough
successful mined receipt history to measure a rate.

The desktop's legacy paid-escrow and Tier 1 request commands are also disabled
before identity access, signing, nonce reads, or network writes. Their old
label-derived model IDs did not bind the exact artifact; opening escrow first
could lock funds before an exact-artifact coordinator rejected the request.
VRF selection and server-derived replica labels are not payment authorization.
Free/community inference remains available without opening escrow.

## Answers operators can give today

- **Is v0.8.0 available?** No. It is not published or deployed. Its pinned
  install URL becomes valid only after the complete GitHub release exists.
- **Are the seeds upgraded?** No. The audited public fleet was one v0.7.2 and
  five v0.7.9 nodes. A coordinated v3 cutover has not occurred.
- **Can a stake-zero worker receive the configured 2.5 testnet ARC?** Stake zero
  does not by itself disqualify a community worker, but no reward is available
  merely for registering or submitting `0x16`. All exact-model, assignment,
  verification, strict validator authorization, activation, treasury, and mined
  `0x25` gates above must pass. The public fleet cannot currently do so because
  this candidate and its coordinated v3 trust root are not deployed.
- **What hardware is required?** An observer/router needs no model or GPU. The
  current full-model worker target is Llama-2-7B Q4_K_M, about 4 GB on disk.
  Use at least 8 GB RAM; 12 GB or more gives safer OS/chain headroom. More CPU
  cores can reduce latency. A GPU is optional. Hardware size is not a reward
  multiplier and cannot guarantee jobs.

## Human-controlled cutover gates

Before publishing a “working network” walkthrough, operators must choose an
approved canonical genesis or checkpoint, rotate the six validator identities
whose legacy seed material appeared in repository history, configure the full
trusted validator set and six explicit HTTPS community RPC origins, choose and
record the activation height, execute one coordinated strict-quorum cutover,
and verify common-height block hash plus state root agreement. The content-
addressed `scripts/recovery/recovery_rollout.py` plan defaults to read-only,
requires an exact GO hash before mutation, imports only into fresh data
directories, checks H/H+1 continuity and restart convergence, and can require a
successful reward receipt plus receipt-only earnings on every validator.

The legacy archive now has a separate, earlier freeze authorization because a
final checkpoint hash cannot truthfully exist before the forked fleet stops.
`archive-fleet-to-drive.sh capture` requires an immutable freeze-plan sidecar
and exact `FREEZE <hash>` phrase. It snapshots and cleanly stops NYC and LAX to
drop the six-equal-stake fleet below its five-validator quorum, captures the
remaining four live RPCs while finality is halted, then stops them and records
all six final WALs. Every capture contains the exact LZ4 `/sync/snapshot`,
bracketing metadata, public endpoint evidence, and a complete tamper-evident
file index; no private identity, service environment, model/build cache, Git
object, or bulky DAG trace is uploaded.

After operators choose a source, the recovery exporter—not an endpoint label—
must decode that snapshot and prove its H/full-root equals the latest complete
WAL block/checkpoint boundary. The audited old WAL predates the authenticated
genesis network hash, so its narrowly scoped `--allow-unbound-legacy-wal`
exception must be explicit in both export and archive-seal evidence. Only after
the 5-of-6 checkpoint and production rollout are sealed can the independent
exact `GO <rollout-manifest-sha256>` phase bind all unchanged captures. It
requires at least one H/hash/root match to the selected checkpoint but uploads
all six bundles with honest `canonical_match` labels, preserving fork history
instead of rewriting it to imply the testnet never diverged.

Only after that should the team use
[`COMMUNITY-NODE-WALKTHROUGH.md`](COMMUNITY-NODE-WALKTHROUGH.md) to record the
2–3 minute demo. If any readiness field is false, any same-height root differs,
or `reward_tx_hash` lacks a successful mined `0x25` receipt on the selected
host, stop and show the failure honestly.
