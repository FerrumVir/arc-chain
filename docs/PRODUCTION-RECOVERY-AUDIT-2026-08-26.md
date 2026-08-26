# ARC production-recovery audit — 2026-08-26

This is a read-only evidence snapshot plus the contract of the unreleased
v0.7.12/v3 recovery candidate. It is not a deployment announcement. No seed was
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

## What v0.7.12 changes — and what remains blocked

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

Approval collection is not implemented yet. The candidate intentionally
reports the effective reward gate disabled and refuses to build a reward
transaction. It does not manufacture validator approvals from shard signatures
and does not expose a validator-signing oracle. Projected rewards remain hidden
until the selected coordinator confirms both protocol activation and approval
collection, then has enough mined receipt history to measure a rate.

The desktop's legacy paid-escrow and Tier 1 request commands are also disabled
before identity access, signing, nonce reads, or network writes. Their old
label-derived model IDs did not bind the exact artifact; opening escrow first
could lock funds before an exact-artifact coordinator rejected the request.
VRF selection and server-derived replica labels are not payment authorization.
Free/community inference remains available without opening escrow.

## Answers operators can give today

- **Is v0.7.12 available?** No. It is not published or deployed. Its pinned
  install URL becomes valid only after the complete GitHub release exists.
- **Are the seeds upgraded?** No. The audited public fleet was one v0.7.2 and
  five v0.7.9 nodes. A coordinated v3 cutover has not occurred.
- **Can a stake-zero worker receive the configured 2.5 testnet ARC?** Stake zero
  does not by itself disqualify a community worker, but no reward is available
  merely for registering or submitting `0x16`. All exact-model, assignment,
  verification, strict validator authorization, activation, treasury, and mined
  `0x25` gates above must pass. The current candidate fails closed before
  issuance because approval collection is unavailable.
- **What hardware is required?** An observer/router needs no model or GPU. The
  current full-model worker target is Llama-2-7B Q4_K_M, about 4 GB on disk.
  Use at least 8 GB RAM; 12 GB or more gives safer OS/chain headroom. More CPU
  cores can reduce latency. A GPU is optional. Hardware size is not a reward
  multiplier and cannot guarantee jobs.

## Human-controlled cutover gates

Before publishing a “working network” walkthrough, operators must choose an
approved canonical genesis or checkpoint, rotate the six validator identities
whose legacy seed material appeared in repository history, configure the full
trusted validator set, implement peer-authenticated reward-approval collection,
choose and record the activation height, execute one coordinated strict-quorum
cutover, and verify common-height block hash plus state root agreement.

Only after that should the team use
[`COMMUNITY-NODE-WALKTHROUGH.md`](COMMUNITY-NODE-WALKTHROUGH.md) to record the
2–3 minute demo. If any readiness field is false, any same-height root differs,
or `reward_tx_hash` lacks a successful mined `0x25` receipt on the selected
host, stop and show the failure honestly.
