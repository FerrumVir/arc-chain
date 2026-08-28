# ARC validator fleet recovery and protocol-v3 cutover

This is an incident-recovery checklist, not authorization to modify the
public seeds. The legacy validator seed material was published in repository
history and must be treated as compromised. Removing it from the current tree
does not make those keys safe again.

The repaired node also speaks protocol v3. Protocol v2 and v3 deliberately
reject one another because the meaning of consensus and reward messages
changed. This is a coordinated quorum cutover, not a compatible rolling
upgrade.

The repository version is the **unreleased v0.8.0 recovery candidate**. Do
not treat this checklist, a green local build, or a draft tag as evidence that
v0.8.0 is published or running on any public seed.

Until every security gate below is complete:

- do not restart, upgrade, or generate keys on a public seed;
- do not use the legacy seed strings or the old deployment scripts;
- do not enable `--enable-community-rewards-v1`;
- do not market a public reward as live;
- do not send private keys, keyfiles, or seed phrases through chat, GitHub,
  CI, shell history, logs, or this repository.

Stake-zero community nodes use the same checkpoint-bound recovered network
identity as the validators while keeping local consensus and voting disabled.
The checked-in genesis now contains the complete six-validator public set and
the block 137146 reward-activation boundary. It is still an unreleased
definition, not evidence that the public cutover or reward path is live.

## 1. Freeze and inventory the current fleet

Record the following for all six validators before touching any process:

- host, operator, launch mechanism, binary version, and binary SHA-256;
- validator public address and configured stake;
- latest height, block hash, state root, and last-progress timestamp;
- authenticated peers and connected stake;
- data, WAL, snapshot, model, and environment locations;
- worker, inference, shard, reward, and receipt evidence;
- enough free bytes and inodes to retain the exact fenced legacy source,
  stream its complete archive, and create the fresh v3 data tree with explicit
  headroom.

Do not infer a service manager from a generic guide. Use the launch mechanism
actually installed on each host. At the August 26, 2026 audit, NYC reported
v0.7.2, five seeds reported v0.7.9, several seeds were stalled, and the fleet
did not share one advancing height/root. Re-query every host; the audit is a
warning, not a substitute for a fresh inventory.

Choose and document the last state that operators accept as canonical. If
operators cannot agree on one block hash and root, stop and resolve that
governance decision before building a replacement genesis or checkpoint.

The freeze authorization must not depend on the final checkpoint hash, because
that hash cannot be known until the forked fleet is stopped and its exact
source is verified. First use `archive-fleet-to-drive.sh audit-writers` to bind
each validator and its controlled systemd supervisor separately, including
both process start times, boot ID, cgroups, argv, executables, data directory,
validator identity, and stake, to the sealed eight-validator 40M legacy source
set. First run `prepare-writers` in plan mode. Execution requires the exact
`ARC_RECOVERY_PREPARE_GO="STAGE-BARRIERS <orchestrator-sha256> HELPER
<helper-sha256>"` phrase. It never stops, reparents, or normalizes a writer;
it stages four condition-only persistent barriers behind the present allow
marker, disables only process-free alternatives, globally syncs the removed
boot links, and independently rechecks their terminal enablement/PID/job state.
It records that durability proof and either the exact systemd cgroup or exact
detached root-session relationship. The canonical unit
closure includes exact `Names`, `Id`, empty `Following`, merged sources,
activation edges, and selected/alternative states.

`seal-freeze-plan` then creates a reviewed, create-only
`arc.recovery.freeze-plan.v5`. The v5 plan binds writer cgroup
path/device/inode and all preparation evidence, plus the exact
`arc-drive-arc:ARC Chain Recovery v0.8` root, ARC custom OAuth client-ID hash,
account hash, prefreeze-gate hash, remaining dedicated-uploader budget, and
capacity reservation. The legacy shared-client `arc-drive` remote cannot
authorize production. Run `capture` in plan mode first. Execution
requires exactly
`ARC_RECOVERY_FREEZE_GO="FREEZE <freeze-plan-sha256> CAPTURE <capture-id>"`;
the capture ID is deterministically derived from the freeze-plan digest.

Plan mode runs the Drive identity/capacity gate read-only. After the exact
authorization, execute mode repeats it immediately before the first writer
signal and must immutable-create, download and hash-verify, permanently delete,
and prove absence of one unique 8 MiB root canary. It also rechecks the ARC
OAuth client, account, and capacity and persists an
`arc.recovery.drive-prefreeze.v1` receipt. Any rclone warning or mismatch is a
hard stop.

After the Drive gate, the helper installs exact volatile lifecycle safety and
uses cgroup v2 as the only quiescence mechanism. It freezes and inode-checks
the selected supervisor cgroup first. For a detached writer it transiently
freezes the audited root-session parent, creates and locally freezes an
inode-bound `arc-recovery-writer` child, moves the sole writer into that child,
durably seals the leaf, then thaws/releases the parent. The owned leaf—not the
root-session scope—remains the disjoint writer target. The helper and ancestors
must remain outside the final targets, and recursive membership plus the frozen
signal baseline are durable. The parent-scope overlay requires effective
`DefaultDependencies=no` and empty `Conflicts`/`Before`; exact sources,
properties, and reverse dependency edges are sealed and rechecked. This assumes
exclusive trusted-root control during the transaction.
A concurrent privileged root/PID1 D-Bus actor is outside the threat model
because it could directly thaw or kill the target regardless of unit policy.

Before marker unlink, all four canonical units must resolve through exact
alias closure to effective `/dev/null` masks in the higher-priority volatile
`/run/systemd/system.control` directory. The frozen state and effective masks
are revalidated after the durable arm. Commit is the single unlink of
`/etc/arc-recovery/legacy-start-allowed` through its sealed parent dirfd plus
parent fsync, which turns the four persistent condition-only barriers
fail-closed. The controller sends only pidfd `SIGTERM` while each target is
frozen and never sends job-control signals or `SIGKILL`.

After durable TERM progress, direct inode-checked writes of `0` to
`cgroup.freeze` perform thaw. A detached writer is thawed first while the
supervisor remains frozen; two stable terminal checks are required before the
supervisor thaw. A shared systemd cgroup is thawed once. The persisted
`arc.recovery.offline-stop.v4` result records each independently
signaled target's TERM state as `none`, `indeterminate`, or `confirmed`, with a
shared supervisor linked to the writer event chain. These states prove durable
intent/send evidence, not exit causality; `exit_cause` is always `unknown`. If
the host rebooted after audit, the controller sends no signal to stale numeric
PIDs. It instead reconciles the durable systemd fence and enablement state,
requires both reviewed services and all writers absent twice, and records the
reboot-fenced path. The controlled 30M is more than one third of the sealed 40M
source set, so only after all six exact writers are absent does the sealed set
have at most 10M available and no quorum. Unknown dynamic positive-stake
identities remain untrusted external forks; never present this closed proof as
a global legacy-network halt.

After the fence is stable, `capture-offline` records source path/device/inode,
a complete regular-file content index, final WAL identity, external snapshot
identity, and stop evidence. The original legacy data directory stays in place;
it is content-sealed by repeated hashing, not mounted read-only and not copied
into a second full local tree. Changed, missing, unexpected, cross-device,
symlink, or special-file content fails closed. The legacy `/sync/snapshot`
endpoint is not trusted as a state barrier.

## 2. Verify every rotated validator identity offline

The recovered genesis already binds six rotated public Ed25519 identities.
Each operator must verify, on a trusted offline system, that the separately
delivered mode-`0600` keyfile derives the exact public address assigned to that
host. Never reuse a legacy seed or key. If any approved private key is missing
or suspect, generate a replacement offline and reseal the genesis, checkpoint,
archive premanifest, and rollout; do not silently substitute a new identity for
one in the checked-in recovery definition.

```bash
umask 077
arc keygen --scheme ed25519 --output /secure/offline/path/validator.key
chmod 600 /secure/offline/path/validator.key
```

For each validator, verify that the keyfile is owned by the service account
and has mode `0600`. Transfer it using the team's approved secret-delivery
channel directly to that validator. The node must receive only a keyfile path,
for example:

```bash
arc-node --validator-key-file /run/secrets/arc-validator.key ...
```

Collect only these non-secret values in the rollout manifest:

- new public validator address;
- intended stake;
- operator and host;
- SHA-256 of the candidate binary;
- approved canonical genesis/checkpoint identifier.

No production staked validator may start from a seed string, environment
seed, CLI seed, incomplete validator set, or a genesis entry that does not
match both its public key and intended stake.

## 3. Approve a new trust root

Because the old validator keys are compromised, a rotation transaction signed
only by the old validator set is not a sufficient trust anchor. The current
candidate records the out-of-band recovery decision as a specifically
identified canonical state checkpoint under the rotated set and carries the
matching complete genesis in all release locations. Operators must approve the
exact signed artifacts and hashes out of band. Rejecting any part of that
decision requires a newly reviewed recovery manifest; it is not permission to
fall back to the old trust root or improvise a fresh chain on one host.

The manifest must bind chain ID, protocol version, canonical height/hash/root
(if preserving state), all six new public addresses and stakes, binary/tag
checksum, genesis checksum, and a future
`community_rewards_v1_activation_height` (or explicitly state that rewards
remain disabled). Have the human operators approve the same manifest out of
band. Do not store private key material in it.

A fresh or migrated validator must start from that approved genesis or an
authenticated checkpoint carrying the required validator quorum. A single
peer's round-sync response, heartbeat, state snapshot, or far-ahead signed DAG
block is diagnostic data, not authority to advance round or commit cursors.
Until quorum-certified checkpoint sync exists end to end, a node that lacks
the approved local history must stop and require operator recovery instead of
fast-forwarding from a peer.

Absence is the fail-closed disabled state; do not encode “disabled” as height
zero. The release contract permits an explicit bounded activation only in a
complete validator genesis. The checked-in checkpoint-bound recovery genesis
is complete and explicitly schedules activation at block 137146. The node also
requires the independent `--enable-community-rewards-v1` switch, so neither
the schedule nor the switch can enable issuance by itself.

The canonical, deployment, and desktop genesis copies are now byte-identical
copies of that recovered definition with `validator_set_complete = true`.
Before release, verify their pinned checksum and every signed recovery artifact
against the reviewed recovery commit; do not reconstruct or edit the definition
on a validator host.
Every validator public address must also appear exactly once in the shared
`[[accounts]]` list with an explicit `balance` (zero is allowed). Runtime
startup and release validation both reject a complete validator genesis when
an address is missing from accounts or duplicated. A node's local keyfile must
only prove that it matches this shared definition; local identity must never
insert an account or otherwise mutate genesis state at startup.
The schedule is included in the authenticated semantic genesis hash; nodes
with different activation rules are different networks. The checked-in
definition remains an unreleased recovery artifact until the coordinated
cutover proves the sealed checkpoint and the six new validator keyfiles.

## 4. Build and prove the release candidate

From a clean checkout with Node.js 24 LTS and Actionlint installed, require the
single aggregate gate to pass:

```bash
./scripts/ci_check.sh --full
```

This includes release/install contracts, a releasable-worktree secret scan,
workflow and shell lint, all Rust targets/tests, the deterministic desktop
gate, stable Tauri tests, and a clean packed-install smoke of the supported
TypeScript SDK. The cross-OS CI run and inference known-answer-vector workflow
must also be green on the exact candidate commit; a same-process determinism
test on one laptop is not a substitute for ARM/x86 agreement.

Workflow text does not prove that repository settings are enabled. Before a
release owner runs the tag workflow, the owner must verify all of these controls
in GitHub's settings:

- protect `main` with a no-bypass PR/check/review ruleset. Protect **all** tag
  names with two `~ALL` tag rulesets: owner-only creation, plus no-bypass
  update, deletion, and non-fast-forward prevention. Enable immutable releases;
- restrict Actions to an owner-reviewed allowlist and require full commit-SHA
  pinning;
- create a protected `release` environment, restrict its deployment tags, add
  required reviewers, move `TAURI_SIGNING_PRIVATE_KEY` and the separate
  `ARC_RELEASE_MANIFEST_PRIVATE_KEY` into that environment, and remove all
  repository-level copies. The retained v0.7-compatible Tauri key encoding has
  no passphrase, so both signing jobs explicitly set
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` to the empty string. Tauri otherwise
  attempts an interactive prompt and fails closed on a headless runner. Run the
  protected-main signing preflight and require both key canaries to verify
  before creating the non-movable `v0.8.0` tag. Freeze merges from that
  preflight through immutable publication: the tag validator requires a
  successful preflight on the exact current `main` SHA, and the publisher
  rejects any `main` movement while the build matrix is running;
- after the independent PR approval and merge are complete, reduce every
  non-owner collaborator below `write` before tagging and keep release creation
  owner-only. Contributions continue through forked pull requests; grant `write`
  only for a bounded independent-review window, then remove it again before any
  tag. Existing unreleased historical tags mean a write collaborator could
  otherwise publish junk as GitHub's `Latest` release and deny service to the
  updater even though payload signatures still prevent code execution;
- before tagging, retain the existing v0.7-compatible updater key and create a
  recovery copy of both current signing keys. If the only private copies are
  the write-only GitHub environment secrets, store a random 32+-character
  backup passphrase in macOS Keychain and the protected `release` environment,
  then manually dispatch `release-signing-backup.yml` with the exact protected
  `main` SHA and confirmation `BACKUP_EXISTING_RELEASE_KEYS`. That one-shot
  workflow runs `scripts/release/backup-signing-keys.sh`,
  immediately restores and byte-compares both keys, uploads ciphertext only,
  and retains the temporary Actions artifact for one day. Before dispatch,
  disable administrator bypass and self-review on the `release` environment
  and require a distinct trusted reviewer. The artifact name binds protected
  `main`, run/attempt, and its ciphertext SHA-256. Download it into a
  FileVault-protected directory, set it mode 0600, and restore-test it from a
  clean checkout of that exact commit. Disable inherited shell tracing before
  reading Keychain so the passphrase cannot enter a trace:

  ```bash
  (
    set +x
    chmod 600 -- "$CIPHERTEXT"
    export ARC_SIGNING_BACKUP_PASSPHRASE
    ARC_SIGNING_BACKUP_PASSPHRASE="$(security find-generic-password \
      -a 'FerrumVir ARC release' \
      -s 'com.arc-chain.release-signing-backup.current' -w)"
    trap 'unset ARC_SIGNING_BACKUP_PASSPHRASE' EXIT HUP INT TERM
    scripts/release/verify-signing-key-backup.sh \
      "$CIPHERTEXT" "$EXPECTED_MAIN_SHA" "$EXPECTED_CIPHERTEXT_SHA256"
  )
  ```

  The verifier binds its checkout and trust inputs to the expected protected
  `main` commit, verifies the ciphertext hash before decryption, prepares all
  locked build tools before plaintext exists, and signs both manifest and
  updater canaries without exporting either private key. Copy only the
  ciphertext to ARC
  Drive and a second independent recovery medium, re-download and hash-match
  both copies, then delete the Actions artifact, delete only the temporary
  passphrase environment secret, and remove the one-shot workflow through a
  protected PR. Keep the passphrase outside every ciphertext provider. Never
  upload either private key in plaintext and never rotate the updater key
  before v0.8.0: every v0.7 client must still be able to verify the v0.8.0
  bridge release;
- configure Apple Developer ID signing/notarization and Windows Authenticode
  signing before claiming OS-signed installers. Until then, release notes must
  plainly label macOS and Windows packages unsigned; the Tauri updater payload
  signature is not Apple or Windows platform signing.

After its one tag-resolution checkout, the release workflow pins every
downstream job to the commit SHA validated from `v0.8.0`, re-checks the remote
tag immediately before creation, and refuses to replace an existing release.
Its publisher is blocked on the full quality harness, Cargo
and npm dependency policy, and the five-platform inference known-answer matrix.
That one new release must contain the CLI/headless and desktop artifacts,
installer, updater manifest/signature, owner-signed `SHA256SUMS` plus
`SHA256SUMS.sig`, seeds, and genesis from the same commit and version. The
signed manifest header binds repository, tag, and commit. The publication gate
cryptographically verifies its signature and all
four updater payloads against the public key embedded in that exact commit.
Test Linux x86_64 in clean Ubuntu 24.04 and 26.04 containers with `DISPLAY`
unset. Test Intel macOS with the headless x86_64 artifact. Confirm that
update/install tests preserve node identity and roll back the entire failed
replacement.

Run the release against an isolated six-validator v3 network loaded from the
approved public manifest. Require:

- all six validators agree on genesis/checkpoint, height, block hash, and root;
- an old v2 peer is rejected and cannot affect v3 quorum;
- replacing a connection cannot let a stale disconnect remove the new one;
- deterministic sequential and parallel execution produce identical ordered
  receipts and state roots;
- peer state hints cannot mutate state and a failed diff is atomic;
- community assignment, decline, timeout, verification, replay, and restart
  tests pass;
- a worker result is recomputed and supported by two distinct active-validator
  signatures for every model range before it is accepted.

Keep reward issuance disabled throughout this rehearsal.

Use `scripts/recovery/recovery_rollout.py` for both the isolated rehearsal and
the production cutover. Its manifest is canonical JSON, create-only, mode
`0444`, and protected by a SHA-256 sidecar. A local rehearsal is
plan/preflight-only unless both `--execute --go-hash
<locked-manifest-sha256>` and the exact
`ARC_RECOVERY_GO="GO <locked-manifest-sha256>"` value are present. Production
has the longer archive-bound authorization in section 5. The harness imports
the quorum-verified checkpoint into six fresh—or exact same-manifest resumable—
data directories, proves the selected legacy block H and v3 transition H+1,
requires advancing same-height hash/root convergence, restarts one validator
at a time, and checks the configured reward policy. Receipt mode additionally
requires the exact successful mined `0x25` receipt and receipt-backed worker
earnings on all six.

## 5. Execute the coordinated v3 cutover

The sealed legacy source set contains eight 5M-stake identities (40M total),
including the six controlled 30M writers. Stopping all six leaves at most 10M
of that sealed set, below its strict greater-than-two-thirds quorum. This is a
closed proof about the sealed source identities, not host count and not a
global halt claim about dynamically admitted legacy forks. The recovered v3
set contains six rotated validators totalling 40M; the orchestrator starts five
in a tight batch before the sixth. Since v2 and v3 are mutually incompatible,
do not interpret the maintenance boundary as permission to mix protocols or
lower quorum.

1. Announce a maintenance window and stop ordinary submissions.
2. Execute the separately sealed freeze plan with the exact `FREEZE
   <freeze-plan-sha256> CAPTURE <capture-id>` authorization. Before any stop,
   require the sealed ARC OAuth remote/root preflight receipt and the successful
   8 MiB write/read/hash/delete canary receipt. Persist and verify the systemd
   start fence, prior enablement evidence, and durable stop intent.
3. For NYC then LAX, freeze the exact cgroup-v2 supervisor subtree. For a
   detached writer, transiently freeze its audited root-session parent, move
   the sole writer into a newly created, locally frozen, inode-bound
   `arc-recovery-writer` child, durably seal that leaf, then thaw/release the
   parent. Require all four high-priority volatile control masks and the durable
   barrier arm before unlinking and fsyncing the allow marker. Send only pidfd
   `SIGTERM` while each target remains frozen. Follow the two-stage v2 chain:
   persist writer TERM/thaw intent, directly thaw the owned leaf, prove the
   writer terminal twice and seal that receipt while the supervisor remains
   frozen and unsignaled, then persist supervisor TERM/thaw intent and directly
   thaw/reconcile its cgroup. Retry after either thaw intent must never refreeze
   that target. Do not issue `SIGKILL`, claim an
   exact exit cause, or claim those sentinels halted a global legacy network.
   Repeat for AMS, LHR, NRT, and SGP. Verify all six exact writer PIDs are gone,
   their persistent condition-only barriers remain stable, and each
   `arc.recovery.offline-stop.v4` journal reconciles its TERM state and any
   zero-signal reboot path. The controlled
   30M has now been removed from the sealed 40M source set, leaving at most 10M
   of that set available; record external dynamic identities as untrusted
   forks rather than claiming a global legacy halt.
4. Build and verify all six capture evidence trees and complete content indexes
   against the original fenced data directories. Preserve every source in
   place; do not discard a fork because it is not ultimately selected and do
   not create a second full local data-tree copy.
5. Use `arc-node recovery export --data-dir <reference-pair> --snapshot
   <reference-pair/state.snapshot.lz4> --legacy-validator-set
   <legacy-validator-set-40m.json> ...` to reproduce the candidate from the
   independently preserved shared reference pair. Successful export—not
   endpoint metadata or a later validator capture—must prove that the decoded
   snapshot H/root equals its complete WAL block/checkpoint boundary.
   The audited legacy WAL needs the explicit `--allow-unbound-legacy-wal`
   exception because it predates the genesis network hash; record that fact.
6. Sign the accepted candidate offline with the required 5-of-6 recovery
   quorum and seal the **prearchive** production manifest. Its
   `complete_sha256`, `archive_manifest_sha256`, `sha256sums_sha256`, and
   `prearchive_rollout_sha256` fields must all be 64 zeroes.
7. Run `archive-fleet-to-drive.sh seal` in plan mode, then execute it only with
   the exact `ARC_RECOVERY_GO="GO <prearchive-rollout-sha256> FREEZE
   <freeze-plan-sha256> CAPTURE <capture-id> DEST
   <sha256-of-exact-drive-destination> LEGACY_WAL <BOUND|UNBOUND>"`. It
   re-exports each stopped WAL only against that capture's own on-disk
   snapshot. A derivable pair is classified as `valid_canonical` or
   `valid_noncanonical_fork`; a missing, ambiguous, torn, or otherwise
   non-derivable pair is `preserved_unclassified`. The independently verified
   shared reference snapshot/WAL pair is the canonical recovery source and is
   never substituted into a validator capture. All six captures may be forks
   or unclassified; no live capture is required to match the canonical
   checkpoint.
8. Stream each complete content-indexed stopped source directly into its
   bundle at the exact capture-scoped destination
   `arc-drive-arc:ARC Chain Recovery v0.8/captures/<capture-id>`. Google Drive
   is not WORM. `COMPLETE.json` is merely the last create-only write in this
   execution; partial uploads are resumable but unusable, and every object
   named by `SHA256SUMS` and `ARCHIVE-MANIFEST.json` must be re-downloaded and
   hashed.
9. Create the final rollout manifest by changing only the four archive roots
   from step 6 to the verified `COMPLETE.json`, archive manifest, checksum, and
   prearchive-manifest digests. Its canonical projection with those four fields
   reset to zero must hash exactly to the archived prearchive digest.
10. Install the exact checksummed candidate and approved genesis/checkpoint on
   every host; install the host's new keyfile separately. The new release and
   data paths must be disjoint and non-nested with the preserved legacy source.
11. Run the finalized production plan, then execute only with
   `--go-hash <final-rollout-sha256> --archive-manifest-sha256
   <verified-archive-manifest-sha256>` and the exact
   `ARC_RECOVERY_GO="GO <final-rollout-sha256> FREEZE
   <freeze-plan-sha256> CAPTURE <capture-id> ARCHIVE
   <verified-archive-manifest-sha256> DEST
   <sha256-of-exact-drive-destination> LEGACY_WAL <BOUND|UNBOUND>"`.
12. Confirm public address, keyfile source, protocol v3, genesis/checkpoint,
   binary checksum, connected authenticated stake, and advancing chain on
   every host.
13. Seal the greatest block number exposed by any legacy public source before
   maintenance as `chain.legacy_public_max_height` (a higher conservative
   ceiling is allowed). Require all six to converge on the same height/hash/root
   strictly above that number and continue advancing for the full observation
   window. Recheck the floor after restarts and immediately before generating
   the recovered frontend config; one missing or lagging replica keeps the app
   and explorer in maintenance rather than showing a lower public block number.

Each production validator RPC must bind loopback. Configure these six explicit
origins on every validator, each as its own repeated `--community-rpc-url`
argument; P2P peers are not RPC discovery:

```text
https://149-28-32-76.nip.io
https://140-82-16-112.nip.io
https://136-244-109-1.nip.io
https://104-238-171-11.nip.io
https://202-182-107-41.nip.io
https://149-28-153-31.nip.io
```

`/community/reward_policy.configured_community_rpc_origins` reports the
configured origin **count**, so the sealed production value is `6`; it does
not return the URL array. The locked rollout installs a SHA-pinned Caddy TLS gateway for
an exact IP-derived `nip.io` hostname (`sslip.io` is the resealed-manifest
fallback), a loopback request/rate-limit filter, strict body limits, security
headers, an exact GitHub Pages CORS origin, and a reviewed path allowlist.
Public preflight terminates at Caddy; internal validator routes never receive
browser CORS. Unknown paths fail closed. Raw public
`:9090` endpoints and clear-text remote community origins are not acceptable
frontend or validator configuration.

If five prepared v3 validators cannot establish the approved chain, stop all
new processes and preserve logs/data for diagnosis. Do not fall back to the
compromised identities. Recovery means correcting the v3 configuration or
approving a new manifest, not quietly restoring the old trust root.

## 6. Reward activation is a separate decision

The coordinator independently recomputes community output and requires two
distinct active-validator shard signatures per range. Separately, state
execution requires an explicit reward approval from at least
`floor(2N/3) + 1` distinct active validator identities and strictly more than
two thirds of active stake. Approval evidence is capped at 64 entries and is
bound to the complete reward commitment.

The unreleased candidate now collects approvals from the six explicitly
configured HTTPS community RPC origins. Each remote validator authenticates
the coordinator request, independently revalidates the complete job/result and
reward commitment, and signs only its own approval. The coordinator accepts
five distinct approvals only when they also cover strict greater-than-two-
thirds active stake; a dead sixth origin cannot delay an already valid quorum.
Failure is atomic: no mempool submission, worker-success increment, or earned
balance is reported without the approval quorum.

This implementation is not evidence of deployment. Leave
`--enable-community-rewards-v1` off until the exact candidate has passed the
six-validator harness, the team has documented treasury limits and monitoring,
and operators approve a bounded testnet receipt canary in the locked rollout
manifest.

For an approved canary, verify in order:

1. the worker claimed the exact coordinator-created job;
2. the model, input, output text/hash, token count, and ceiling all match the
   independent recomputation;
3. every range has two valid signatures from distinct active validators;
4. the reward is only *submitted* before block inclusion;
5. the successful receipt appears on every validator;
6. treasury and worker balances change by exactly 2.5 ARC;
7. replaying the job, certificate, or transaction pays nothing;
8. `/worker/earnings/:address` counts only the successful mined receipt.

Only then expose ordinary community work. Public coordinator origins must be
the locked HTTPS gateways described above; signed proof of possession does not
replace TLS, body/rate limits, or a fail-closed route allowlist.

## Automatic stop conditions

Stop the cutover or disable new issuance on any of these signals:

- private validator material appears in a repo, command line, log, CI job, or
  support channel;
- a node reports an unapproved validator address, genesis/checkpoint, binary,
  protocol, stake, or chain ID;
- fewer than five equal-stake v3 validators authenticate;
- heights stop advancing or block hashes, receipts, or roots diverge;
- a stale connection event reduces live quorum;
- a state hint changes local state without successful local re-execution;
- community work is accepted without complete independent verification;
- a reward is paid without the exact assignment or is paid more than once;
- counters or projected earnings increase before a successful mined receipt.

Disabling the reward flag prevents new local issuance; it cannot reverse a
mined transaction. A chain rollback or checkpoint change is a separate,
explicit operator/governance decision.
