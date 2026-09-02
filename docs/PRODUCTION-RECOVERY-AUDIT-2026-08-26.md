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
rollback coverage. GUI-free node boot is blocking for Linux x86_64 on Ubuntu
22.04, 24.04, and 26.04 and for Linux ARM64 on Ubuntu 24.04 and 26.04.

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
  Llama-2-7B Q4_K_M, about 4 GB on disk. The release does not establish a minimum-RAM
  figure; prove a complete model load and inference with OS/chain headroom on
  the target host. More CPU cores can reduce latency. A GPU is optional.
  Hardware size is not a reward multiplier and cannot guarantee jobs.

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
fleet is stopped and indexed. `audit-writers` separately binds the exact
controlled validator process and systemd supervisor `MainPID`, including both
start times, cgroups, argv, executables, the data directory, validator identity,
and stake, to a sealed eight-identity 40M source set. A validator in the exact
root-session cgroup shape is recorded explicitly instead of being falsely
treated as the service `MainPID`. Preparation does not stop, reparent, or
normalize a writer. It requires the exact
`ARC_RECOVERY_PREPARE_GO="STAGE-BARRIERS <orchestrator-sha256> HELPER
<helper-sha256>"` phrase, stages four condition-only persistent barriers
behind a present allow marker, and seals the marker, writer cgroup
path/device/inode, unit source/alias/activation closure, and selected versus
process-free alternative states. Removed alternative boot links are globally
synced and their terminal enablement/PID/job state is independently rechecked
before that prepare receipt is sealed. `seal-freeze-plan` emits the canonical
`arc.recovery.freeze-plan.v5`.
`archive-fleet-to-drive.sh capture` executes only with
`ARC_RECOVERY_FREEZE_GO="FREEZE <freeze-plan-sha256> CAPTURE <capture-id>"`.
Before that authorization may consume a stop intent, the v5 plan binds the ARC
custom-OAuth Drive remote/root
`arc-drive-arc:ARC Chain Recovery v0.8`, hashes of the inspectable OAuth client
ID and account, the exact prefreeze gate, remaining dedicated-uploader budget,
and an archive-capacity reservation. The legacy shared-client `arc-drive`
remote is forbidden. Plan mode verifies those identities and capacity
read-only. Execute mode repeats the gate immediately before the first writer
signal, then immutable-creates, downloads and SHA-256 verifies, permanently
deletes, and proves absence of one unique 8 MiB canary. A durable
`arc.recovery.drive-prefreeze.v1` receipt must report both verification and
deletion.

After the Drive gate, the helper applies exact volatile lifecycle safety and
freezes the inode-checked cgroup-v2 supervisor subtree. For a detached
root-session writer it transiently freezes the audited parent scope, creates
and locally freezes an inode-bound `arc-recovery-writer` child cgroup, moves the
sole sealed writer into that child, durably seals the child identity and
membership, then thaws and releases the parent scope. The owned child—not the
root-session scope—is the writer's continuing freeze target; a systemd-owned
writer instead shares the supervisor cgroup. Before the transient parent
freeze, the high-priority scope overlay sets `DefaultDependencies=no`; the
effective `Conflicts` and `Before` sets must be empty so PID1 has no generated
shutdown stop edge. Exact source, property, reverse-edge, recursive-member, and
signal-baseline checks are repeated through barrier arm. The
transaction requires exclusive trusted-root control; a concurrent privileged
root/PID1 D-Bus adversary could directly thaw or kill any process and is not a
defensible in-host threat boundary.

Still before marker unlink, all four canonical ARC units must resolve through
their exact `Names`/`Id` and empty `Following` closure to `/dev/null` masks in
the higher-priority volatile `/run/systemd/system.control` directory. Only
after the frozen state, effective masks, and barrier arm are fsynced does the
transaction unlink `/etc/arc-recovery/legacy-start-allowed` through its parent
dirfd and fsync that parent. That unlink commits the four persistent
condition-only start barriers. A precommit reboot loses the volatile masks but
keeps the allow marker; a postcommit reboot is zero-signal reconciliation.

The same-boot controller sends only pidfd `SIGTERM` while targets remain
frozen. It never sends job-control signals or `SIGKILL`. Its two-stage v2
journal persists writer TERM progress and writer thaw intent first, opens the
exact owned-leaf cgroup inode, and writes `0` directly to `cgroup.freeze` while
the supervisor remains frozen and unsignaled. Two stable checks and a durable
writer-terminal receipt must prove the sealed identity, exact matches, and
owned-leaf membership gone before supervisor TERM or thaw intent may exist.
Only then may the controller thaw/reconcile the supervisor through its exact
inode. A retry after either thaw intent cannot refreeze that target; a shared
systemd cgroup uses a linked one-stage event chain and is thawed once. These
facts do not establish which permitted event ultimately caused process exit.

The durable result is `arc.recovery.offline-stop.v4`. Independently signaled
targets record pidfd-TERM state `none`, `indeterminate`, or `confirmed`; a
shared supervisor points to the writer event chain. `confirmed` proves the
pidfd send returned, not that SIGTERM caused exit, so `exit_cause` remains
`unknown`. A post-audit reboot sends no signal to stale numeric PIDs: it
re-verifies the persistent fence and enablement, proves both services and all
writers absent twice, and records reboot reconciliation. Only after all six
exact writers are absent does the closed sealed-set proof leave at most 10M of
40M available—below quorum. Dynamic legacy RPC membership was poisoned and
divergent; unknown positive-stake identities remain untrusted external forks,
and the evidence does **not** claim every possible external legacy network
globally halted.

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
`arc-drive-arc:ARC Chain Recovery v0.8/captures/<capture-id>` without a full
second local copy.

The supported seal path is `build-production-manifest.py prearchive`, never a
hand-edited production manifest. It consumes the exact
protected-main Linux x86_64 pre-tag artifacts and build run, sealed freeze and
legacy public-height receipt proven fresh at the sealed pre-quarantine
boundary, canonical six-root
`arc.validator-vault.offline-stop-evidence.v2` receipt, source artifacts,
signed checkpoint, exact Caddy 2.11.4 binary, and reward probe. Capture derives
that offline-stop receipt from fresh hash-pinned remote stopped-status calls;
each fixed node/host row binds the real offline-stop.v4 `stop.complete` and
index roots plus the exact status argv/output hashes.

The live capture path retains the receipt's 300-second wall-clock gate and,
before writing the first quarantine boundary or mutating a host, atomically
requires `receipt.completed_at <= authenticated fleet started_at <=
authenticated fleet completed_at <= first_quarantine_started_at` with a total
receipt-to-boundary interval of `0..=300` seconds. Once the fleet is stopped,
the builder deliberately uses no current-time freshness check: resampling the
six retired origins would be impossible without violating the stop fence. It
authenticates the exact receipt/freeze/capture/hash bindings and the complete
sealed chain through all-controlled-stopped and maintenance-boundary creation,
then repeats the same semantic check after an exact byte-and-SHA re-read just
before create-only prearchive publication.

A committed local first-boundary timestamp is not, by itself, proof that a
remote quarantine began. On resume, capture challenges all six exact hosts for
their capture-bound quarantine or stopped status before it may reuse that
timestamp. Zero authenticated remote mutations hard-fails with instructions to
resample the still-live origins and use a new offline-evidence output; only an
already-started remote transaction may resume from the historical boundary.

Before any semantic check, the builder creates a new operator-private stage,
copies every binary and recovery input through stable no-follow descriptors,
fsyncs a canonical inventory, and seals the directories without write bits.
All later checkpoint inspection/reproduction, archive copying, and deployment
refer only to these single-link staged paths, closing the caller-path
hash/open/stage substitution window.

The sealed local receipt cannot authorize prearchive or key delivery on its
own: mode 0400 and a checksum prove only local integrity. The builder requires
an exact mode-0400 six-IP known-hosts anchor with six unique Ed25519 keys and an
explicit mode-0400 SSH identity. It privately stages the already validated
bytes, creates a new 256-bit challenge, and uses absolute root-owned
`/usr/bin/ssh` with a fixed empty environment/config, pinned keys, no agent,
proxy, redirect, forwarding, or password fallback. All six fixed hosts must
return fresh canonical hash-pinned-helper responses binding the source commit,
freeze/capture, node/IP, legacy validator address/stake, challenge, and newly
re-derived stop roots. The parallel attempt is bounded to 120 seconds and the
result must be at most 300 seconds old at create-only prearchive sealing. The
public known-hosts anchor and exact verification receipt are manifest/archive
bound; the private SSH identity is never published or archived.

The verifier's local interpreter is itself selected only from the protected
`/usr/bin/python3` entry point and byte-hash pinned. The check accepts macOS's
legitimate signed-system Python hard links and Linux's protected
same-directory versioned symlink, validates root-owned non-writable ancestry
with `lstat`/opened-file identity, and has no GNU `stat -c` or `readlink -f`
dependency. Caller `PATH`, loaders, proxies, agents, and Python environment are
not inherited.

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

Final production sealing uses `build-production-manifest.py finalize` with
separately downloaded canonical completion evidence and independently verified
roots. It changes exactly the four archive-finalization roots, proves the
zero-root projection is the exact prearchive digest, and creates a new
mode-0400 manifest and checksum sidecar without overwrite.

The prearchive and finalized rollout manifests also seal
`chain.legacy_public_max_height`, sampled as the greatest block number exposed
by any legacy public source immediately before maintenance (or set to a higher
conservative ceiling). Roots-only archive finalization cannot change it. The
rollout must put all six v3 validators on one commitment strictly above that
floor, continue advancing, survive every one-at-a-time restart, and recheck the
floor before completion. Recovered frontend configuration is create-only and
is not emitted until the same H/H+1, reward, liveness, and all-replica height
gates pass; any replica at or below the floor keeps canonical labels paused.

Only after that should the team use
[`COMMUNITY-NODE-WALKTHROUGH.md`](COMMUNITY-NODE-WALKTHROUGH.md) to record the
2–3 minute demo. If any readiness field is false, any same-height root differs,
or `reward_tx_hash` lacks a successful mined `0x25` receipt on the selected
host, stop and show the failure honestly.
