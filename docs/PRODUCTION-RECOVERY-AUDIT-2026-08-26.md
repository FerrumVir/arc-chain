# ARC production-recovery audit — 2026-08-26

This is a read-only evidence snapshot plus the contract of the unreleased
v0.8.0/v3 recovery candidate. It is not a deployment announcement. No seed was
restarted, upgraded, rekeyed, or otherwise mutated during this audit.

## What community reports correctly identified

- Public v0.7.10 and v0.7.11 were desktop-only releases. The desktop and CLI
  publishers had split, so those tags did not contain the headless
  `arc-node-linux-x86_64` asset that existed in v0.7.7. A GUI package is not a
  substitute for an EC2/VPS/SSH binary.
- Public v0.7.11 could manually check, verify, and install a signed update from
  Settings, but its persisted automatic-update preference was never consumed,
  so startup and periodic checks did not run.
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
- **What hardware is required?** A stake-zero router that does not execute
  inference needs no model or GPU. The current full-model worker target is
  Llama-2-7B Q4_K_M, about 4 GB on disk. Use at least 16 GB system RAM for the
  expanded integer weights plus OS/chain headroom. More CPU cores can reduce
  latency. A GPU is optional. Hardware size is not a reward multiplier and
  cannot guarantee jobs.

## Human-controlled cutover gates

Before publishing a “working network” walkthrough, operators must verify the
signed canonical checkpoint and the byte-identical, checkpoint-bound recovered
genesis already checked into the candidate. That genesis contains the complete
six-identity rotated validator set and the block 137146 activation boundary;
it is not an incomplete observer placeholder. Operators must prove each
separately delivered keyfile matches its assigned public address, configure the
six explicit HTTPS community RPC origins, execute one coordinated cutover, and
verify common-height block hash plus state root agreement. The content-addressed
`scripts/recovery/recovery_rollout.py` plan defaults to read-only, requires an
exact archive-bound GO phrase before mutation, imports only into fresh or exact
same-manifest resumable data directories, checks H/H+1 continuity and restart
convergence, and can require a successful reward receipt plus receipt-only
earnings on every validator.

The legacy archive has a separate, earlier freeze authorization because the
final checkpoint and archive roots cannot truthfully exist before the forked
fleet is stopped and indexed. `audit-writers` binds the exact controlled
systemd `MainPID`, process identity, argv, executable, data directory,
validator identity, and stake to a sealed eight-identity 40M source set.
`archive-fleet-to-drive.sh capture` executes only with
`ARC_RECOVERY_FREEZE_GO="FREEZE <freeze-plan-sha256> CAPTURE <capture-id>"`.
It persistently fences and cleanly stops the six exact controlled writers,
representing 30M of that set, without SIGKILL. Stopping them leaves at most 10M
of the sealed set—below quorum—but dynamic legacy RPC membership was poisoned
and divergent. Unknown positive-stake identities remain recorded as untrusted
external forks; the recovery evidence does **not** claim every possible
external legacy network globally halted.

After all six controlled writers are fenced, capture records the original data
directory's path/device/inode, final WAL, external snapshot identities, stop
evidence, and a complete content index. It retains the original legacy source
in place and repeatedly re-hashes it; it does not create a second full local
data-tree copy or pretend the source is OS-read-only. Changed, missing,
unexpected, cross-device, symlink, or special-file content fails closed. The
racy legacy `/sync/snapshot` RPC is not the capture boundary.

The independently preserved shared reference snapshot/WAL pair is the
canonical source. The recovery exporter—not an endpoint label or a later live
capture—must decode that pair and prove its H/full-root equals its complete WAL
block/checkpoint boundary. The audited old WAL predates the authenticated
genesis network hash, so its narrowly scoped `--allow-unbound-legacy-wal`
exception must be explicit in checkpoint export, archive sealing, and both GO
policies.

After the 5-of-6 checkpoint is signed, operators seal a prearchive rollout
whose four archive-finalization roots are all zero. Archive execution requires
exactly `GO <prearchive-rollout-sha256> FREEZE <freeze-plan-sha256> CAPTURE
<capture-id> DEST <sha256-of-exact-drive-destination> LEGACY_WAL
<BOUND|UNBOUND>`. Each stopped capture is independently classified
`valid_canonical`, `valid_noncanonical_fork`, or `preserved_unclassified`; all
six may be forks or unclassified because canonical authority comes from the
shared reference pair. Every stopped source is streamed directly to
`arc-drive:ARC Chain Recovery/captures/<capture-id>` without a full second
local copy.

Google Drive is not WORM or intrinsically immutable. Partial uploads are
resumable but not consumable; `COMPLETE.json` is only the last create-only
write in this execution, and the verifier re-downloads and hashes every object
named by `SHA256SUMS` and `ARCHIVE-MANIFEST.json`. The final production manifest
may replace only its four zero archive roots and must project byte-for-byte to
the archived premanifest. Production execution then requires both
`--archive-manifest-sha256 <verified-archive-manifest-sha256>` and exactly `GO
<final-rollout-sha256> FREEZE <freeze-plan-sha256> CAPTURE <capture-id> ARCHIVE
<verified-archive-manifest-sha256> DEST
<sha256-of-exact-drive-destination> LEGACY_WAL <BOUND|UNBOUND>`. New v3 release
and data paths must be disjoint from every preserved legacy source, which is
reverified after cutover.

Only after that should the team use
[`COMMUNITY-NODE-WALKTHROUGH.md`](COMMUNITY-NODE-WALKTHROUGH.md) to record the
2–3 minute demo. If any readiness field is false, any same-height root differs,
or `reward_tx_hash` lacks a successful mined `0x25` receipt on the selected
host, stop and show the failure honestly.
