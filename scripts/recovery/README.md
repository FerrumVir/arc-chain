# ARC recovery rehearsal and rollout

`recovery_rollout.py` is the execution boundary for one six-validator protocol-
v3 recovery. It does not choose the canonical legacy block, create or rotate
keys, sign a checkpoint, or authorize production. Those are human/offline
decisions. It makes the approved decision reproducible and refuses a partial,
mutable, clear-text, or same-data-directory rollout.

The default `run` behavior is read-only. A local mutation requires the same
SHA-256 three times: the sealed manifest sidecar, `--go-hash`, and the exact
`ARC_RECOVERY_GO="GO <hash>"` phrase. Production additionally binds the sealed
freeze-plan digest, deterministically derived capture ID, exact Drive
destination, verified archive-manifest hash, and explicit legacy-WAL policy in
the finalized manifest, CLI, and extended GO phrase.

The production archive remote is the separately reviewed ARC Google Drive
OAuth remote/root, currently `arc-drive-arc:ARC Chain Recovery v0.8`. The
legacy shared-client `arc-drive` remote cannot authorize a fleet freeze.

## Inputs

Start from `recovery-manifest.schema.json`. The built-in validator is stricter
than JSON Schema and rejects unknown fields. A draft binds:

- protocol v3 chain ID, recovery epoch, validator-set ID, selected legacy H,
  exact H block hash, exact H+1 transition hash/root, the approved checkpoint
  manifest hash, and `legacy_public_max_height`: the greatest legacy block
  number exposed before maintenance (or a higher conservative ceiling);
- local absolute paths and SHA-256 values for `arc-node`, genesis, ARCCHKPT,
  and, in production, the matching CLI/build metadata, public validator-key
  manifest, fresh legacy-height and six-root offline-stop receipts, the exact
  six-key SSH host trust anchor, reward
  probe, source snapshot/WAL, and exact Caddy executable;
- exactly six unique public validator addresses, stakes, keyfile paths, RPC
  origins, P2P addresses, and fresh data directories (or exact
  same-manifest-owned prepared state during a retry);
- timeouts, minimum observed height advance, and either a policy-only or real
  mined-receipt reward gate;
- production-only literal public-IPv4 HTTPS origins, SSH/service identity, and
  create-only release/unit paths;
- production-only archive destination, freeze/capture identities, executing
  helper/orchestrator/rollout/schema hashes, explicit legacy-WAL policy, and
  four finalization roots. A prearchive manifest has four all-zero roots; a
  final manifest may change only those roots and must project exactly to the
  archived prearchive digest;
- the pre-positioned canonical GGUF path and SHA-256 on every validator, plus
  exact per-node ranges that give every one of the 32 layers three replicas
  while loading the audited 15--17 layers on each validator.

Never put private key bytes, seed strings, passwords, tokens, or environment
secrets in the manifest. `key_file` is only a path to a separately delivered
mode-`0600` key.

For a local rehearsal, give each process a distinct loopback IP such as
`127.0.0.11` through `127.0.0.16` and use `http://<that-ip>:9090` for both its
`rpc_listen` and `rpc_url`. P2P ports must be distinct. Every runtime receives
all six origins through repeated `--community-rpc-url` flags; P2P peers are
never treated as RPC discovery.

For production, the manifest's `rpc_listen` remains a loopback value (for
example `127.0.0.1:9944`) so the local rehearsal and retired TCP-port absence
check stay explicit, but the remote validator does **not** bind that TCP
address. It receives only `--rpc-unix` and creates a mode-`0660` Unix socket
under a sealed mode-`0750` runtime directory. `rpc_url` must exactly match the
validator's HTTPS origin with its literal public IPv4 address. The manifest
must carry the reviewed public GET/POST allowlists verbatim. The rollout stages SHA-pinned
Caddy 2.11.4, validates its config, and requests a publicly trusted IP-address
certificate from Let's Encrypt's production ACME service with the `shortlived`
profile and HTTP-01 challenge. It persists ACME state under the release root
and installs a dedicated nginx filter connected only through permission-sealed
Unix-domain sockets for body/rate limits. This
direct-IP design removes the shared `nip.io`/`sslip.io` wildcard-DNS dependency.
Before any v3 process starts and again after public promotion, a fresh direct
TLS handshake must validate through the system public-CA store, pass IPv4
hostname verification, present exactly that validator IPv4 as its sole SAN,
identify the public Let's Encrypt issuer, have a total lifetime no greater than
160 hours, retain at least 48 hours of validity, and return the expected HTTPS
probe response. The exact leaf digest/times and these results are sealed in
the rollback journal. Issuance and future renewal still require public ACME
and inbound port 80. The bounded rollout proves issuance and restart reuse; it
does **not** wait days or claim that renewal was observed. Ongoing renewal
monitoring remains an operator obligation. Only the six validator
IPs may reach the signed internal approval path or the shard announce, forward,
and cleanup paths. Shard destinations are bound to these six explicit HTTPS
origins and responses are authenticated by active validator keys. Unknown paths
return 404; there is no public `:9090` listener.

The Unix-domain security boundary is exactly Ubuntu Noble nginx
`1.24.0-2ubuntu7.17`, binary SHA-256
`1f16b72bea2f44e5d04fe6cf9e3e4b0dec53a82c50c7c1533c302a8ecaeccacf`.
It runs as the dedicated no-login `arc-rpc-filter` user with no capabilities,
a strict systemd sandbox, a hash-bound config/unit/start preflight, and an
exact `auth_request` interlock on every proxying route—including reward
approval and validator shard traffic. Caddy has `admin off` and contains no
`forward_auth`, avoiding the affected Caddy 2.11.4 handler combination. The
package is deliberately **not** held. An unattended replacement makes the next
preflight/restart fail closed; before an nginx update, change the audited
package and binary pins in protected code, rerun the security tests and rollout
receipt, and never perform an ad-hoc hold/unhold or upgrade on one validator.
The distribution nginx service stays stopped and disabled, its old config is
preserved but never loaded by the rollout-owned filter.

The late-fork retirement tripwire also has no TCP origin. Its exact
`arc.recovery.legacy-late-fork-interlock-status.v2` response is read through a
dedicated Unix socket. `HEALTHY` is permitted only with
`capture-bound-retirement-tripwire-clear`: all six retired official origins
remain unreachable and every declared community monitor has a fresh coherent
observation at or below the sealed cutoff. Any retired origin that answers—even
at or below the cutoff—or any observed source above the cutoff latches
`latched-legacy-source-incident`. An unavailable or inconsistent required
community monitor yields transient maintenance with
`community-source-observation-unavailable`. The status always keeps
`global_absence_claimed=false`; this is a capture-bound retirement tripwire,
not a proof that no legacy fork exists anywhere. The generated frontend
service contract independently pins the exact protected `sourceMainCommit`,
`observedCutoffHeight`, source-set hash, boundary hash, and tool hash before it
will display any canonical/public health state.

Production currently requires audited root-owned SSH/service operation because
the gateway binds ports 80/443 and the validator keys remain mode `0600`.
Existing system nginx state/listeners are recorded before it is stopped and
disabled; its configuration is preserved. Another process still holding 80 or
443 is a hard stop.

## Seal and inspect the plan

```bash
python3 scripts/recovery/recovery_rollout.py seal \
  --draft /secure/operator/arc-recovery-draft.json \
  --output /secure/operator/arc-recovery.lock.json

python3 scripts/recovery/recovery_rollout.py run \
  --manifest /secure/operator/arc-recovery.lock.json
```

`seal` verifies every artifact hash, writes canonical JSON with mode `0444`,
and creates a mode-`0444` `.sha256` sidecar. It never replaces either file.
`run` rechecks the seal and all artifact hashes, executes offline ARCCHKPT
`inspect` plus quorum `verify`, checks six fresh-or-exact-resume
data/key/host prerequisites,
and prints `PLAN ONLY`. It changes no local/remote directory, process, service,
package, proxy, certificate, or data.

If any artifact, endpoint, node, key path, stake, activation rule, timeout, or
probe changes, create and approve a new sealed manifest. Do not chmod/edit an
old one.

## Execute an approved local rehearsal

Copy the exact hash printed by `seal`:

```bash
locked_sha256='<64 lowercase hex characters>'
ARC_RECOVERY_GO="GO $locked_sha256" \
  python3 scripts/recovery/recovery_rollout.py run \
    --manifest /secure/operator/arc-recovery.lock.json \
    --execute \
    --go-hash "$locked_sha256"
```

Execution imports the quorum checkpoint into all six absent data directories,
starts all six validators without recovery flags, proves the preserved H and
exact H+1 continuation, requires advancing same-height hash/root agreement,
then cleanly restarts one validator at a time while the other five retain
strict quorum. Local processes are stopped at the end; data and logs are never
deleted.

Every local or production validator stop first closes background admission,
then waits through the node's complete 4,000-second inference window, the
300-second crash/late-submit grace for already-owned community work, and a
two-minute writer-join/WAL-fsync allowance (4,420 seconds total).
Remote stop/restart SSH watchdogs include additional systemd and transport
margin, while a start that has no prior process remains short-bounded. Rollback
allows one full validator drain before restoring the lightweight gateway and
archive units; a timeout never escalates to SIGKILL or claims a clean restart.
The production rollback journal directory must not exist before execute. It is
created mode `0700` and fsynced before the first remote mutation. Every reverse
host attempt and result is create-only; the final receipt binds exact
post-restore service states and listener ownership. An unreachable or partial
restore raises `EMERGENCY_ROLLBACK_INCOMPLETE`: preserve every data/history/
artifact/config/log byte, delete nothing, and do not begin another rollout.

## Execute the production cutover

Use a roots-only finalized, sealed `mode: "production"` manifest only after the
exact capture-scoped archive has a fully verified `COMPLETE.json` and all six
controlled legacy writers remain persistently fenced. This is not a claim that
all dynamically observed external legacy forks are globally halted. Run the
read-only plan to obtain the verified archive-manifest hash and exact extended
phrase:

```bash
python3 scripts/recovery/recovery_rollout.py run \
  --manifest /secure/operator/arc-recovery-final.lock.json \
  --reward-evidence-output /secure/operator/recovery-v3.reward-evidence.json \
  --rollback-journal /secure/operator/rollback-final-rollout

ARC_RECOVERY_GO="GO $final_rollout_sha256 FREEZE $freeze_sha256 CAPTURE $capture_id ARCHIVE $archive_manifest_sha256 DEST $destination_sha256 LEGACY_WAL $legacy_wal_policy" \
  python3 scripts/recovery/recovery_rollout.py run \
    --manifest /secure/operator/arc-recovery-final.lock.json \
    --execute \
    --go-hash "$final_rollout_sha256" \
    --archive-manifest-sha256 "$archive_manifest_sha256" \
    --reward-evidence-output /secure/operator/recovery-v3.reward-evidence.json \
    --rollback-journal /secure/operator/rollback-final-rollout
```

The orchestrator:

1. after the exact GO, re-verifies the complete Drive archive and every live,
   stopped, restart-fenced capture before the first remote mutation;
2. downloads only the small root-pinned manifest, completion marker, and fork
   inventories needed to derive the exact `valid_noncanonical_fork` set;
3. stages and re-hashes the exact binary/genesis/checkpoint/Caddy artifacts;
4. validates the checkpoint and Caddy configuration remotely;
5. imports into six fresh or exact same-manifest resumable data directories;
6. copies/reflinks each fork's pinned unsigned checkpoint and binding evidence
   into rollout-owned local storage and installs the generated unprivileged,
   loopback-only archive/filter/TLS routes automatically;
7. installs create-only filter, gateway, archive, and validator systemd units;
8. obtains publicly trusted TLS and, before any v3 start, seals an exact-IP SAN,
   system-public-trust, <=160-hour lifetime, >=48-hour remaining-validity, and
   HTTPS response proof for all six; after public promotion it repeats and
   seals the same fresh fleet proof. This bounded run does not claim renewal;
9. starts five validators in a tight quorum batch, then the sixth;
10. proves every validator/archive MainPID and argv, loopback-only listeners,
   GET-only fork routes, Pages-only CORS, HTTPS health, H/H+1 continuity, all-six
   convergence strictly above the sealed `legacy_public_max_height`, continued
   advancing convergence, the sealed 32-layer/3x HTTPS shard topology, and
   every one-at-a-time restart;
11. proves reward-policy agreement and, when selected, two sequential,
   successful mined `0x25` receipts with distinct transaction hashes, job IDs,
   block heights, and block hashes; then proves the same worker has exactly
   2.5 ARC (2,500,000,000 base units) per receipt and exactly 5 ARC gross in
   the two-receipt canary window. All six must also return null observed rate
   and null `projected_daily_arc` with the canonical `collecting data` reason:
   a projection requires at least three successful mined receipts spanning at
   least 24 hours, not the initial one or two rollout canaries.

Do not reuse the 2026-08-27 README observation as the final public ceiling.
The sealed maintenance evidence bundle retains the official-six pre-fence
public samples, every post-quarantine authenticated loopback tuple, two stable
per-host quarantine samples at least 120 seconds apart, and six pinned offline
persisted-head exports. `legacy_observed_cutoff_height` is the exact maximum of
that enumerated evidence; `legacy_public_max_height` is that cutoff plus the
explicit 128-block operational safety margin. It does not claim global legacy
fork absence. The orchestrator rechecks that all six v3 replicas are strictly
above that ceiling
after restarts and again before completion. `frontend-config` repeats the full
live H/H+1, liveness, reward-policy, and height-floor verification before it
creates a recovered config; the shared frontend keeps canonical labels paused
if even one replica is missing or at/below the floor.

Before the one-way quarantine-retirement journal boundary, failure restores the
exact preexecution service baseline. After that boundary, rollback is
maintenance-only: it stops/disables v3 and the distribution nginx service,
finishes removal of only the capture-owned fence, and restores the sealed Caddy
maintenance edge while retaining both legacy restart barriers. It never starts
the old validator or old nginx path. No rollback deletes imported data,
artifacts, configs, journals, fork-reader files, or the archived fleet, and it
never falls back to compromised identities. Each
prepared stage is marked with the exact rollout digest: the same manifest can
verify and resume it, while a different manifest is rejected. New release and
data paths must be disjoint and non-nested with every frozen legacy source.

### Efficient legacy archive

The legacy freeze and the final checkpoint seal are deliberately separate.
The final checkpoint hash cannot exist until the forked fleet has stopped, so
requiring that hash before capture would create a circular authorization. Seal
a small, create-only `arc.recovery.freeze-plan.v5` first. It binds the exact
remote helper, orchestrator, rollout verifier, and schema bytes plus the source
commit,
sentinel order, six exact writer/supervisor identities, and the sealed
eight-member 40M legacy validator set. Each node seals both process start
times, argv, executables, and the writer cgroup path/device/inode. It also
seals the prepare marker and all four condition-only barriers, merged unit
sources, canonical `Names`/`Id`/empty-`Following` alias closure, selected and
alternative unit states, and activation edges. A validator in the exact
`/user.slice/user-0.slice/session-N.scope` root-session shape is recorded as a
detached writer instead of being falsely treated as the systemd `MainPID`.
The v5 plan also binds the ARC Drive gate bytes, exact remote root,
hashed custom OAuth client ID, hashed account, reviewed remaining daily upload
budget, and the operator's dedicated-uploader attestation:

```bash
scripts/recovery/archive-fleet-to-drive.sh prepare-writers \
  --legacy-validator-set /secure/operator/legacy-validator-set-40m.json \
  --output /secure/operator/arc-writers.lock.json \
  --plan

ARC_RECOVERY_PREPARE_GO="STAGE-BARRIERS <orchestrator-sha256> HELPER <helper-sha256>" \
  scripts/recovery/archive-fleet-to-drive.sh prepare-writers \
    --legacy-validator-set /secure/operator/legacy-validator-set-40m.json \
    --output /secure/operator/arc-writers.lock.json \
    --execute

scripts/recovery/archive-fleet-to-drive.sh seal-freeze-plan \
  --window arc-v3-cutover-2026-08 \
  --legacy-validator-set /secure/operator/legacy-validator-set-40m.json \
  --writer-contracts /secure/operator/arc-writers.lock.json \
  --drive-remote-root 'arc-drive-arc:ARC Chain Recovery v0.8' \
  --drive-client-id-sha256 "$drive_client_id_sha256" \
  --drive-account-sha256 "$drive_account_sha256" \
  --drive-daily-upload-budget-bytes "$drive_daily_upload_budget_bytes" \
  --attest-dedicated-drive-uploader \
  --output /secure/operator/arc-freeze.lock.json

scripts/recovery/archive-fleet-to-drive.sh capture \
  --freeze-plan /secure/operator/arc-freeze.lock.json \
  --legacy-public-height-receipt /secure/operator/legacy-public-height.json \
  --legacy-public-height-receipt-sha256 "$legacy_public_height_sha256" \
  --inspector-binary /secure/operator/pretag-linux-x86_64/arc-node \
  --inspector-binary-sha256 "$inspector_binary_sha256" \
  --genesis /secure/operator/genesis.toml \
  --genesis-sha256 "$genesis_sha256" \
  --validator-public-keys /secure/operator/validator-public-keys.json \
  --validator-public-keys-sha256 "$validator_public_keys_sha256" \
  --legacy-validator-set /secure/operator/legacy-validator-set-40m.json \
  --legacy-validator-set-sha256 "$legacy_validator_set_sha256" \
  --offline-stop-evidence-output /secure/operator/arc-offline-stop-evidence.json

freeze_sha256='<freeze-plan hash printed by seal-freeze-plan>'
capture_id='<capture id printed by seal-freeze-plan>'
ARC_RECOVERY_FREEZE_GO="FREEZE $freeze_sha256 CAPTURE $capture_id" \
  scripts/recovery/archive-fleet-to-drive.sh capture \
    --freeze-plan /secure/operator/arc-freeze.lock.json \
    --legacy-public-height-receipt /secure/operator/legacy-public-height.json \
    --legacy-public-height-receipt-sha256 "$legacy_public_height_sha256" \
    --inspector-binary /secure/operator/pretag-linux-x86_64/arc-node \
    --inspector-binary-sha256 "$inspector_binary_sha256" \
    --genesis /secure/operator/genesis.toml \
    --genesis-sha256 "$genesis_sha256" \
    --validator-public-keys /secure/operator/validator-public-keys.json \
    --validator-public-keys-sha256 "$validator_public_keys_sha256" \
    --legacy-validator-set /secure/operator/legacy-validator-set-40m.json \
    --legacy-validator-set-sha256 "$legacy_validator_set_sha256" \
    --offline-stop-evidence-output /secure/operator/arc-offline-stop-evidence.json \
    --execute
```

After all six exact writers are stopped, `capture` re-runs the hash-pinned
remote `stopped-status` command with every frozen writer argument and seals a
canonical mode-0400 `arc.validator-vault.offline-stop-evidence.v2` receipt plus
sidecar. Its fixed NYC/LAX/AMS/LHR/NRT/SGP node-to-host map binds each real
`arc.recovery.offline-stop.v4` `stop.complete` root, its `stop.files.sha256`
index root, and the exact status argv/output hashes. Key installation must
independently use the same fixed host map and re-run that exact hash-pinned
status command; this receipt is never a caller-authored stopped boolean.

The preparation authorization is exact and independently hash-binds the local
orchestrator and installed remote helper. Preparation never stops, reparents,
or normalizes a writer. It stages four persistent condition-only drop-ins
behind the still-present allow marker, disables only already process-free
alternatives, globally syncs the removed boot-enablement links, independently
rechecks their inactive/PID/job state, and seals that durability receipt in the
prepare contract. It then seals either the existing systemd cgroup relationship or the
exact detached root-session relationship. The shared marker keeps this stage
fail-open and safely resumable.

The capture ID is `SHA256("ARC recovery capture v2\0" || freeze_plan_digest)`;
it is not an operator-selected label. The default `capture` is read-only.
Immediately before every remote helper invocation, the orchestrator re-hashes
the installed helper and refuses any byte mismatch. Plan mode also runs the
Drive gate read-only: it rejects the legacy remote, any ambient Drive or
selected-remote rclone override, any ambient TLS trust/key-log override, a
missing inspectable custom OAuth client, a different account/root, rclone
warnings, insufficient finite capacity, an over-budget archive reservation,
and a largest-object reservation above Drive's limit. The gate permits only
the reviewed rclone v1.75.0 and records that version in its receipt.

Account identity does not use backend-dependent `rclone config userinfo`.
After a benign `rclone about` refreshes the OAuth grant, the gate pipes the
decrypted configuration for exactly the selected remote directly into its
hash-pinned isolated helper; it never writes that stream to disk. The helper
uses verified TLS and bounded retries/timeouts for exactly
`GET https://www.googleapis.com/drive/v3/about?fields=user(emailAddress,permissionId,me)`.
It requires `me=true`, one normalized email, and one permission ID, and emits
only their SHA-256 hashes. Access/refresh tokens, client secret, raw email, raw
permission ID, and API body are absent from argv, environment, logs, durable
temporary files, and the receipt.

After the exact `FREEZE` authorization and immediately before the first writer
signal, execute mode repeats those checks at the sealed ARC remote root. It
uploads one unique 8 MiB canary with immutable-create semantics, downloads and
SHA-256 verifies it, permanently deletes it, verifies absence, rechecks
capacity/client/account/permission identity and the rclone version, and durably
records the `arc.recovery.drive-prefreeze.v1` receipt. This second Drive API
identity query closes the canary account-switch window. The freeze does not
begin unless the canary is both verified and deleted.

Immediately after that verified canary and before any cgroup freeze, marker
unlink, or signal, the helper performs one bounded loopback GET for exactly
`/inference/results`, `/workers/scoreboard`, and
`/inference/attestations` on each of the six sealed legacy writers. It never
requests `/community/list`. Each endpoint has a 20-second deadline and an
8 MiB captured-body ceiling. HTTP 404, connection failure, timeout, and an
oversize response are preserved as observed outcomes; they are not converted
into invented model, worker, attestation, or reward data. Every captured raw
byte sequence (including a bounded oversize prefix) has an explicit byte count
and SHA-256.

Before each GET, the helper fsyncs a create-only attempt intent. A crash after
that boundary is resumed as an interrupted observation and never repeats the
GET. A completed immutable receipt is reused byte-for-byte even after the
writer is fenced. If any of the six receipts is missing, a fleet-wide
eligibility barrier requires all six exact writers to remain live and unfenced;
once any writer's stop/fence transaction begins, no missing receipt can start
or repeat network capture. All three outcomes, their UTC
timestamps, node, exact loopback origin, limits, and raw hashes are sealed in
the per-node `arc.recovery.legacy-live-observations.v1` tree. Its labels are
explicitly `diagnostic`, `noncanonical`, and `nonreward`. All six receipt trees
and indexes must be durably written and reverified before the first writer
signal; endpoint errors are evidence, while inability to durably write a
receipt stops the fleet freeze.

Execution installs exact volatile lifecycle-safety overlays, then uses cgroup
v2 freezing as the only quiescence mechanism. It inode-checks and freezes the
selected supervisor cgroup first. For a detached writer, it transiently freezes
the audited root-session parent, creates and locally freezes an inode-bound
`arc-recovery-writer` child, moves the sole writer into that child, durably
seals the child's exact identity and membership, then thaws and releases the
parent. The owned child—not the root-session scope—remains frozen alongside the
supervisor; a systemd-owned writer is already inside the supervisor cgroup.
The parent-scope overlay sets `DefaultDependencies=no` and proves the effective
`Conflicts` and `Before` sets are empty before the transient freeze. It also
seals and repeatedly rechecks reverse dependency edges and exact scope
source/property bytes. The helper and ancestors must be outside the final
targets, all recursive members must match the sealed allowlist, and the frozen
task/signal baseline is durably recorded.

This transaction protects against ordinary service lifecycle, dependency,
retry, and reboot paths on the reviewed host. It does not claim protection
from a concurrent privileged root/PID1 D-Bus adversary: such an actor can
create a new stop edge, write `cgroup.freeze`, or kill the process directly.
Production execution therefore requires exclusive trusted-root control for
the short freeze transaction.

While the marker is still present, the transaction installs `/dev/null` masks
for all four canonical units in the higher-priority volatile
`/run/systemd/system.control` directory and proves PID1 resolves each exact
canonical `Names`/`Id` with empty `Following` to that mask. It writes and
fsyncs the barrier arm only after every cgroup is frozen and every mask is
effective. The sole commit mutation is then unlinking
`/etc/arc-recovery/legacy-start-allowed` through its sealed parent dirfd and
fsyncing the parent. The persistent condition-only barriers become fail-closed
at that point. Before commit, a reboot removes the volatile masks while the
present marker permits normal recovery; after commit, marker absence is the
durable restart fence and a reboot permits only zero-signal reconciliation.

The same-boot controller sends only pidfd `SIGTERM`, and only while the exact
cgroup remains frozen. It never sends job-control signals and never issues
`SIGKILL`. Its two-stage v2 journal is writer-first: persist writer TERM
progress and the writer thaw intent, directly thaw the inode-checked owned leaf
while the supervisor remains frozen and unsignaled, then require two stable
terminal checks and a durable writer-terminal receipt before any supervisor
TERM or thaw intent may exist. Only then may it persist supervisor progress and
directly thaw/reconcile the supervisor cgroup. Each stage binds `none`,
`indeterminate`, `confirmed`, or terminal TERM evidence, and retry after a thaw
intent never refreezes that target. A shared systemd cgroup uses the linked
single-cgroup path and is thawed once. No path asks systemd to stop or thaw the
audited unit.

Every stop writes `arc.recovery.offline-stop.v4`. Each independently signaled
target has a durable pidfd-TERM state: `none` means no intent was consumed,
`indeterminate` means intent was fsynced but no sent record exists, and
`confirmed` means the pidfd send returned and its sent record was persisted.
A shared supervisor is explicitly linked to the writer's event chain. The
record always leaves `exit_cause=unknown` and states that recovery sent no
SIGKILL. If the boot ID changed after audit, reconciliation sends nothing to a
stale numeric PID; it instead verifies the persistent fence, both reviewed
services and all writers absent twice, and records the reboot-fenced outcome.

The sentinel order remains NYC, then LAX, followed by AMS, LHR, NRT, and SGP;
those first stops do not authorize a global halt claim. The six exact
controlled identities represent 30M of the sealed
40M source set; only after all six are stopped does the sealed proof leave at
most 10M unstopped stake, below quorum. Divergent dynamic RPC identities are
recorded as untrusted external forks, never folded into a false claim that the
vulnerable old network is globally halted.

After all six writers are proven PID-free, each capture binds the original
legacy data directory's path, device, inode, complete regular-file index, final
state/DAG WAL bytes, external snapshot identity, and persistent fence evidence.
The exact source remains in place and is repeatedly re-hashed; it is content-
sealed, not OS-read-only, and no second full local data tree is created. The
helper never uses SIGKILL or the racy legacy live-snapshot RPC. It rejects
changed, missing, unexpected, cross-device, symlink, or special-file content.

The sealed prearchive production manifest carries the independently preserved
exact-height source snapshot and its paired reference WAL as SHA-256-bound
artifacts. Build
the unsigned candidate from that pair using the exact recovery exporter;
successful export decodes the snapshot, recomputes its account/storage/code
root, and requires it to equal the complete WAL block/checkpoint boundary:

```bash
arc-node recovery export \
  --data-dir /secure/operator/reference-pair \
  --snapshot /secure/operator/reference-pair/state.snapshot.lz4 \
  --genesis /secure/operator/genesis.toml \
  --validator-public-keys /secure/operator/validator-public-keys.json \
  --legacy-validator-set /secure/operator/legacy-validator-set-40m.json \
  --output /secure/operator/candidate.arcchkpt \
  --source-consensus-round 9774808 \
  --created-at-unix-ms 1787857623000 \
  --recovery-epoch 1 \
  --validator-set-id 1 \
  --allow-unbound-legacy-wal
```

The last flag is necessary for the audited legacy WAL, which predates the
authenticated genesis network hash. It is never implicit: both checkpoint
creation and final archive sealing require the operator to state it, and the
binding evidence records that exception. Sign the accepted candidate offline,
then build the prearchive production manifest only from protected-main and
sealed evidence inputs. The builder derives every chain, topology, gateway,
artifact, check, destination, and zero archive-root field:

```bash
scripts/recovery/build-production-manifest.py prearchive \
  --source-main-sha "$protected_main_sha" \
  --pretag-run-id "$pretag_run_id" \
  --pretag-run-attempt "$pretag_run_attempt" \
  --pretag-artifact-input-set /secure/operator/PRETAG-ARTIFACT-INPUT-SET.json \
  --curl /usr/bin/curl \
  --curl-sha256 "$system_curl_sha256" \
  --ca-bundle /private/etc/ssl/cert.pem \
  --ca-bundle-sha256 "$system_ca_bundle_sha256" \
  --freeze-plan /secure/operator/arc-freeze.lock.json \
  --freeze-plan-sha256 "$freeze_sha256" \
  --legacy-public-height-receipt /secure/operator/legacy-public-height.json \
  --legacy-maintenance-evidence-bundle /secure/operator/arc-offline-stop-evidence.json.legacy-maintenance-evidence-bundle.json \
  --legacy-maintenance-boundary /secure/operator/arc-offline-stop-evidence.json.legacy-maintenance-boundary.json \
  --legacy-late-fork-source-set /secure/operator/arc-offline-stop-evidence.json.legacy-late-fork-source-set.json \
  --offline-stop-evidence /secure/operator/arc-offline-stop-evidence.json \
  --ssh-known-hosts /secure/operator/arc-validator-known-hosts \
  --ssh-identity /secure/operator/id_arc_recovery_ed25519 \
  --validator-vault-restore-receipt /secure/operator/vault/RESTORE-RECEIPT.json \
  --validator-key-install-receipt /secure/operator/VALIDATOR-KEY-INSTALL-RECEIPT.json \
  --validator-public-keys /secure/operator/validator-public-keys.json \
  --legacy-validator-set /secure/operator/legacy-validator-set-40m.json \
  --checkpoint /secure/operator/recovery.arcchkpt \
  --source-snapshot /secure/operator/reference-pair/state.snapshot.lz4 \
  --source-wal /secure/operator/reference-pair/state.wal \
  --caddy /secure/operator/caddy-2.11.4-linux-amd64 \
  --reward-probe "$PWD/scripts/recovery/community-reward-probe.py" \
  --stage-root /secure/operator/production-input-stage-v0.8.0 \
  --acme-email tj@arc.ai \
  --output /secure/operator/arc-recovery.prearchive.json
```

`PRETAG-ARTIFACT-INPUT-SET.json` is mode 0400 canonical JSON. It names the
exact protected-main run and, in the documented order, the artifact ID and
absolute raw Actions ZIP path for all five headless and all four desktop
groups. Those coordinates are not authorization: the builder independently
queries the public GitHub API with hash-pinned system curl and CA bytes,
requires current protected `main`, and validates every raw ZIP, inner archive,
build metadata, and payload hash. A shared set-level proof keeps the complete
initial and final check within the anonymous API limit; the final branch query
is last and every live root is sealed.

The stage root must not exist. The builder creates it once at mode 0700, copies
every semantic input through stable no-follow file descriptors, fsyncs each
copy and a canonical stage manifest, then removes all directory write bits.
Checkpoint inspection/reproduction, archive capture, and rollout use only
these single-link staged nine raw release artifacts, proof sets, binary, CLI,
checkpoint, snapshot/WAL, genesis,
validator, key restore/install receipts, Caddy, freeze/evidence, transport,
and probe bytes. Caller paths are
never executed or deployed after their initial copy.

The local offline-stop receipt is an integrity record, not authority by itself.
Before it can seal a prearchive, the builder uses the exact staged freeze,
receipt, known-hosts, and SSH identity bytes,
generates a new 256-bit challenge, and calls all six fixed IPs in parallel. It
uses absolute root-owned `/usr/bin/ssh`, an empty environment and config,
exactly six unique Ed25519 host-key pins, one explicit identity, and disables
agents, proxies, redirects, forwarding, password fallback, and hostname
canonicalization. The hash-pinned remote helper must freshly re-derive every
`offline-stop.v4` tree and return a canonical challenge-bound response for the
same source commit, freeze, capture, node/IP, legacy address/stake, helper, and
stop roots. All six responses must complete within 120 seconds and remain no
older than 300 seconds when the create-only manifest is sealed.

The local verifier also binds the exact bytes of the freeze-plan's normalized,
non-symlink `operator_python_path` before using it. On Ubuntu this is the
versioned target (for example `/usr/bin/python3.12`), never the usual
`/usr/bin/python3` symlink. It requires root ownership, non-group/world-writable
system ancestry, one stable open-file identity, and the reviewed hash. It does not depend on
GNU `stat -c`, GNU `readlink -f`, caller `PATH`, or Python-related environment
variables.

The builder refuses unavailable or partial live verification, mutable or
symlinked seals, duplicate/noncanonical JSON, stale or replayed evidence,
duplicate/wrong host keys, any pre-tag commit/run/platform mismatch, a checkpoint
that the exact selected Linux binary cannot inspect, reproduce, and
quorum-verify, any production address/stake/model/shard/host drift, and the
wrong Caddy binary. It creates the manifest and checksum sidecar once at mode
0400 and never overwrites either. Run it on a reviewed Linux x86_64 operator
host because it executes the exact retained Linux x86_64 node. Its `complete_sha256`,
`archive_manifest_sha256`, `sha256sums_sha256`, and
`prearchive_rollout_sha256` are all 64 zeroes. Plan and execute the archive
phase:

```bash
arc_sha256() {
  if [ -x /usr/bin/sha256sum ]; then /usr/bin/sha256sum "$1"
  else /usr/bin/shasum -a 256 "$1"; fi | /usr/bin/awk '{print $1}'
}
export ARC_RECOVERY_SSH_USER=root
# Copy this exact value from freeze.lock.json.operator_python_path. It must be
# normalized, versioned on Ubuntu, and itself a non-symlink regular file.
export ARC_RECOVERY_PYTHON_PATH=/usr/bin/python3.12
test -f "$ARC_RECOVERY_PYTHON_PATH" && test ! -L "$ARC_RECOVERY_PYTHON_PATH"
export ARC_RECOVERY_PYTHON_SHA256="$(arc_sha256 "$ARC_RECOVERY_PYTHON_PATH")"
export ARC_RECOVERY_SSH_KNOWN_HOSTS=/secure/operator/production-input-stage-v0.8.0/private/known_hosts
export ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256="$(arc_sha256 "$ARC_RECOVERY_SSH_KNOWN_HOSTS")"
export ARC_RECOVERY_SSH_IDENTITY=/secure/operator/production-input-stage-v0.8.0/private/id_ed25519
export ARC_RECOVERY_SSH_IDENTITY_SHA256="$(arc_sha256 "$ARC_RECOVERY_SSH_IDENTITY")"
export ARC_RECOVERY_SSH_SHA256="$(arc_sha256 /usr/bin/ssh)"
export ARC_RECOVERY_SCP_SHA256="$(arc_sha256 /usr/bin/scp)"
export ARC_RECOVERY_RCLONE_PATH=/secure/operator/tools/rclone
export ARC_RECOVERY_RCLONE_SHA256="$(arc_sha256 "$ARC_RECOVERY_RCLONE_PATH")"
export ARC_RECOVERY_RCLONE_CONFIG=/secure/operator/rclone-arc.conf
export ARC_RECOVERY_GH_PATH="$(/bin/realpath "$(command -v gh)")"
test -f "$ARC_RECOVERY_GH_PATH" && test ! -L "$ARC_RECOVERY_GH_PATH"
export ARC_RECOVERY_GH_SHA256="$(arc_sha256 "$ARC_RECOVERY_GH_PATH")"
export ARC_RECOVERY_GITHUB_LOGIN=FerrumVir
archive_work_root=/secure/operator/arc-archive-work
install -d -m 0700 "$archive_work_root"

scripts/recovery/archive-fleet-to-drive.sh seal \
  --freeze-plan /secure/operator/arc-freeze.lock.json \
  --manifest /secure/operator/arc-recovery.prearchive.json \
  --validator-public-keys /secure/operator/validator-public-keys.json \
  --validator-install-receipt /secure/operator/production-input-stage-v0.8.0/private/VALIDATOR-KEY-INSTALL-RECEIPT.json \
  --vault-restore-receipt /secure/operator/production-input-stage-v0.8.0/private/VALIDATOR-VAULT-RESTORE-RECEIPT.json \
  --finalization-intent /secure/operator/archive-finalization-intent.json \
  --work-root "$archive_work_root" \
  --allow-unbound-legacy-wal

locked_sha256='<sealed prearchive rollout-manifest sha256>'
destination='arc-drive-arc:ARC Chain Recovery v0.8/captures/'"$capture_id"
destination_sha256="$(printf %s "$destination" | /usr/bin/env -i HOME=/var/empty PATH=/usr/bin:/bin LANG=C LC_ALL=C "$ARC_RECOVERY_PYTHON_PATH" -I -c 'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')"
ARC_RECOVERY_GO="GO $locked_sha256 FREEZE $freeze_sha256 CAPTURE $capture_id DEST $destination_sha256 LEGACY_WAL UNBOUND" \
  scripts/recovery/archive-fleet-to-drive.sh seal \
    --freeze-plan /secure/operator/arc-freeze.lock.json \
    --manifest /secure/operator/arc-recovery.prearchive.json \
    --validator-public-keys /secure/operator/validator-public-keys.json \
    --validator-install-receipt /secure/operator/production-input-stage-v0.8.0/private/VALIDATOR-KEY-INSTALL-RECEIPT.json \
    --vault-restore-receipt /secure/operator/production-input-stage-v0.8.0/private/VALIDATOR-VAULT-RESTORE-RECEIPT.json \
    --finalization-intent /secure/operator/archive-finalization-intent.json \
    --work-root "$archive_work_root" \
    --allow-unbound-legacy-wal \
    --execute
```

The prearchive rollout manifest must contain the same `freeze_plan_sha256`,
`capture_id`, exact destination, helper/tool hashes, and legacy-WAL policy.
Archive sealing refuses a manifest unless all four finalization roots are
zero. `seal` rechecks the read-only rollout sidecar, every artifact hash, the
paired reference export, and the 5-of-6 signed checkpoint. On each host it runs
the read-only exporter directly against the content-indexed, persistently
fenced source without creating a second full data tree. Each final capture is
classified as `valid_canonical`,
`valid_noncanonical_fork`, or `preserved_unclassified`. It never substitutes
the sealed canonical reference snapshot for a validator's missing or divergent
snapshot. The independently reproduced shared reference snapshot/WAL pair—not
one of the later live captures—is the canonical recovery source. All six final
captures may therefore be forks or unclassified and are still retained and
uploaded under the exact capture-scoped destination
`arc-drive-arc:ARC Chain Recovery v0.8/captures/<capture-id>`. A changed or
missing
capture or a Drive object bound to a different freeze hash stops before upload
or replacement. A per-node semantic export failure is preserved as
unclassified evidence and cannot masquerade as either a fork or a canonical
source. If strict offline recovery trims an uncheckpointed WAL tail, hashes of
the accepted prefix and quarantined tail bind the exact retained source bytes.

Each streamed bundle contains the complete stopped legacy data directory,
persistent fence evidence, optional public legacy binary/genesis inputs, and
semantic export result, plus the immutable pre-freeze live-observation receipt
tree. Each capture inventory binds that tree-index hash and receipt hash.
Shared uploads include the canonical six-node
`legacy-live-observations.json` root/receipt binding, sealed source snapshot/reference
WAL, final binary/CLI/build metadata, genesis, source/public validator sets,
fresh public-height receipt, six-root offline-stop evidence, the public
six-host SSH trust anchor, signed checkpoint,
rollout manifest, capture ID, and `SHA256SUMS`. Private identities, service
environments, build caches, model weights, and Git objects outside `arc-data`
remain excluded; DAG persistence inside `arc-data` is retained in full.
The shared set also includes `github-gist-write-canary.json`, which binds the
exact GitHub account and reviewed CLI hash plus successful private
create/read-by-revision/delete capability at the execute boundary.

After all six bundle/inventory pairs have been uploaded and independently
checked, the operator builds canonical `SHA256SUMS` and
`ARCHIVE-MANIFEST.json`. The manifest binds every shared input, all six
classifications, bundle/inventory sizes and SHA-256 values, both archive helper
hashes, rollout tool/schema hashes, the independently verified canonical
reference, source commit, freeze digest, capture ID, and prearchive rollout
digest. The shared fleet observation binding is therefore named and hashed by
both the archive manifest and `SHA256SUMS`; restore verification rechecks its
six ordered receipt roots, labels, and capture/freeze identity before accepting
the archive. Those metadata files are uploaded and checked only after the bundles.
Before metadata publication, the tool creates a random secret-Gist
create/read-by-exact-revision/delete canary using the hash-pinned `gh` binary
and the exact `FerrumVir` account; its nonsecret receipt is archived. The
canonical finalization intent is then uploaded as a secret Gist and immediately
re-fetched through `GET /gists/{id}/{revision}`. `COMPLETE.json` v2 binds the
intent SHA-256, Gist id, immutable revision, Gist-file SHA-256, and archive
manifest hash; it is the final create-only
mutation in this execution. Google Drive is not WORM or intrinsically
immutable: partial destinations without COMPLETE are resumable but must never
be consumed, and every object is re-downloaded and cryptographically checked.
If the operator disk copy of the intent is lost, verification-only resume
recovers the exact intent from that immutable Gist revision and reconstructs
the same COMPLETE bytes. A changed latest Gist revision is irrelevant.
Verify a destination before use:

```bash
scripts/recovery/archive-fleet-to-drive.sh verify-complete \
  --destination 'arc-drive-arc:ARC Chain Recovery v0.8/captures/<capture-id>'
```

An absent, non-canonical, mismatched, or tampered `COMPLETE.json`, manifest, or
sidecar fails closed.

Use the emitted `FINAL-ROLLOUT-ROOTS` values only as independently verified
trust roots for separately downloaded archive evidence, then run:

```bash
scripts/recovery/build-production-manifest.py finalize \
  --prearchive /secure/operator/arc-recovery.prearchive.json \
  --complete /secure/operator/downloaded/COMPLETE.json \
  --complete-sha256 "$complete_sha256" \
  --archive-manifest /secure/operator/downloaded/ARCHIVE-MANIFEST.json \
  --archive-manifest-sidecar /secure/operator/downloaded/ARCHIVE-MANIFEST.json.sha256 \
  --archive-manifest-sha256 "$archive_manifest_sha256" \
  --sha256sums /secure/operator/downloaded/SHA256SUMS \
  --sha256sums-sha256 "$sha256sums_sha256" \
  --drive-archive-seal-prefreeze /secure/operator/downloaded/drive-archive-seal-prefreeze.json \
  --drive-archive-seal-attempt /secure/operator/downloaded/drive-archive-seal-attempt.json \
  --github-gist-write-canary /secure/operator/downloaded/github-gist-write-canary.json \
  --output /secure/operator/arc-recovery.final.json
```

The finalizer changes **only** the prearchive manifest's four zero roots and
creates a new mode-0400 manifest and sidecar without replacement. Validation
resets those four fields to zero and requires the
resulting canonical bytes to hash to `prearchive_rollout_sha256`; a changed
host, artifact, check, model, shard assignment, destination, or policy is not a
finalization. The final `recovery_rollout.py run` first verifies the exact
destination, `COMPLETE.json`, archive manifest, `SHA256SUMS`, every listed
object, and all live capture indexes. It then prints the production GO phrase
shown above. The verifier repeats those source and archive checks immediately
before mutation and again after cutover.

## Sealed production API

The public GET allowlist carried verbatim in the manifest is `/health`,
`/info`, `/network/info`, `/stats`, `/validators`, `/block/latest`, `/blocks`,
`/inference/attestations`, `/economics/rewards`, `/faucet/status`,
`/community/list`, `/community/reward_policy`, `/workers/scoreboard`, `/shards`,
`/models`, and `/models/shards`. Strict parameterized public reads cover only
blocks, transactions, accounts, worker earnings, reward receipts, and reward
jobs in the shapes documented in the repository README.

The public POST allowlist is exactly `/inference/run`,
`/inference/run_consensus`, `/community/register`, `/community/heartbeat`,
`/community/claim_work`, `/community/submit_work`, `/tx/submit`,
`/tx/submit_signed`, `/tx/submit_batch`, and `/faucet/claim`. `/tx/submit` is
the flat signed transfer contract and `/tx/submit_batch` is its batch form,
used across the supported SDKs. Batches have a hard 64-item maximum and share
the atomic 10 tx/s per-sender admission policy with single submissions. The
node still rejects every request item that omits either `signature` or
`public_key`. Inference has a 4,000-second upstream timeout, worker result
submission 2,700 seconds, and validator approval 1,500 seconds.

`/internal/community/reward/approve`, `/shards/announce`,
`/inference/forward_shard`, and `/inference/cleanup_shard` are validator-IP-only.
Source handlers `/inference/run_sharded`, `/inference/results`,
`/community/reward_approval/{job_id}`, and `/eth` are not public v3 routes.
Unknown paths fail closed. The block explorer is a source-pinned static
candidate, not a deployed public service; configured origins or a successful
frontend build do not prove an explorer deployment or fleet cutover.

## Publish immutable legacy-fork views

Google Drive remains the immutable-by-policy capture store; it is never mounted
or exposed to HTTP. Production `recovery_rollout.py run --execute` now performs
the legacy-reader deployment itself. For each sealed
`valid_noncanonical_fork`, it verifies the Drive inventory, verifies the local
binding index against that inventory, copies or filesystem-reflinks only the
indexed `candidate.arcchkpt` and `binding.json`, and writes the exact manifest,
`COMPLETE.json`, and inventory beneath
`/var/lib/arc-legacy-archive/<rollout-sha256>/<node>`. Every file is
root-owned, group-readable by the locked `arc-archive` account, and has no
write bit. A retry may reuse only an exact same-hash file and same-rollout
owner marker.

The rollout generates the archive unit, nginx filter, and Caddy route from the
sealed node identity; no environment file or hand-edited path is accepted.
After a successful rollout, these are useful independent diagnostics (replace
`<generated-unit>`, `<archive-rpc-socket>`, `<sealed-validator-origin>`, and
`<node>` with the values printed by the plan):

```bash
sudo systemctl is-active '<generated-unit>'
sudo curl --fail --silent --unix-socket '<archive-rpc-socket>' 'http://localhost/provenance' | jq -e \
  '.schema == "arc.legacy-archive.query.v1" and .read_only == true and .classification == "valid_noncanonical_fork"'
test "$(sudo curl --unix-socket '<archive-rpc-socket>' -sS -o /dev/null -w '%{http_code}' -I 'http://localhost/provenance')" = 405
test "$(sudo curl --unix-socket '<archive-rpc-socket>' -sS -o /dev/null -w '%{http_code}' -X POST 'http://localhost/provenance')" = 405
curl --fail --silent "https://<sealed-validator-origin>/legacy/<node>/provenance" | jq -e '.read_only == true'
```

The production process has no TCP listener and has no node state, WAL, P2P,
consensus, signing key, POST route, or Drive credential. The generated route
reuses the validator's sealed HTTPS origin and publishes exactly
`/legacy/<node>/*`, with a body cap, per-IP rate limits, an explicit 405 for
non-GET methods, and a fail-closed path allowlist. The rollout validates both
configs, starts the archive before its dependent TLS gateway, proves both
listeners are loopback-only, and exercises the public provenance/CORS contract
before completion. Any mismatch triggers rollback of every new service.

The recovered frontend generator does not accept arbitrary archive URLs. It
fully verifies the sealed Drive destination, fetches the hash-pinned canonical
archive manifest, COMPLETE marker, and small fork inventories, derives the
complete fork-node set from the six sealed classifications, and derives each base URL as
`<sealed-validator-rpc-url>/legacy/<node>`, and then requires an exact live
provenance match through checkpoint payload/binding pins. Receipt mode also
requires the rollout-bound two-receipt artifact. Immediately before writing,
the generator also exercises each derived public archive route: provenance GET
must be exact, HEAD and POST must return 405, the exact Pages origin must receive
its CORS grant plus `Vary: Origin`, and an attacker origin must receive no CORS
grant:

```bash
python3 scripts/recovery/recovery_rollout.py frontend-config \
  --manifest /secure/operator/arc-recovery.final.lock.json \
  --reward-evidence /secure/operator/recovery-v3.reward-evidence.json \
  --output /secure/operator/arc-network.recovered.json
shasum -a 256 -c /secure/operator/arc-network.recovered.json.sha256
```

An air-gapped review may instead supply both `--archive-manifest` and
`--archive-complete`; they must be canonical mode-read-only files matching the
same finalized roots. Supplying only one fails closed. The default has no
manual local-file handoff and still never mounts or serves Drive.

The output and sidecar are create-only mode `0444`. Publish the exact bytes in
a dedicated Git commit replacing `shared/frontend/arc-network.json` only after
the dashboard and explorer contract tests pass. Verify the deployed Pages file
hash equals the generated hash and re-fetch every advertised provenance URL.
Rollback is `git revert <that-config-commit>` followed by the same Pages deploy
and hash check; this restores the tracked maintenance config without changing
canonical blocks or deleting any archive. A fork view is always explicitly
selected and provenance-verified; it is never canonical or a reward source.

## Reward gates

`checks.reward.mode: "policy"` verifies all six `/community/reward_policy`
responses, including the exact protocol/issuance state, active set size six,
required approvals five, six explicit RPC origins, stake-zero eligibility,
epoch, set, domain, validator-set commitment, and amount agreement.

`mode: "receipt"` additionally needs exactly one of:

- `receipts`, an array of exactly two `{tx_hash, job_id, worker}` objects with
  distinct transaction hashes and job IDs for the same worker; or
- `probe_argv` whose absolute executable is bound by `probe_sha256` and emits
  exactly `{"tx_hash":"0x...","job_id":"0x...","worker":"0x..."}` on
  each call. The rollout invokes it with `--probe-ordinal` plus the exact
  rollout/ordinal-derived `--recovery-probe-id`, proves receipt 1 mined, then
  invokes ordinal 2. It never submits both at once.

For the production GO gate, use the repository probe rather than policy-only
mode. It first requires an issuance-ready validator that sees an eligible
full-model worker, submits one real one-token `/inference/run`, and refuses to
emit evidence unless the response proves community routing, the canonical
per-row INT8 execution profile, authenticated 2-of-3 verification for every
range/position, five validator approvals, and a pending `0x25` transaction:

```bash
probe=/absolute/path/to/scripts/recovery/community-reward-probe.py
probe_sha256=$(shasum -a 256 "$probe" | awk '{print $1}')
```

Bind those values into the draft before sealing:

```json
{
  "mode": "receipt",
  "expect_protocol_active": true,
  "expect_issuance_ready": true,
  "probe_argv": ["/absolute/path/to/scripts/recovery/community-reward-probe.py"],
  "probe_sha256": "<exact 64-character hash above>",
  "expected_reward_base": 2500000000
}
```

The stake-zero worker must already be running and registered with all six
sealed HTTPS origins. `/workers/scoreboard` and `/community/list` are public
read-only dashboard/probe endpoints; shard forwarding and reward approval stay
validator-IP-only.

The probe receives only these non-secret environment values:
`ARC_RECOVERY_RPC_URLS`, `ARC_RECOVERY_ROLLOUT_MANIFEST_SHA256`, and
`ARC_RECOVERY_CHECKPOINT_MANIFEST_HASH`. It deterministically seals one of the
six coordinators for the rollout and never fails over to another coordinator.
The namespaced probe identity is committed as the signed assignment epoch and
has a consensus replay marker, so a retry rediscovers the same job/transaction
across client or coordinator restarts and cannot pay through another
coordinator. The reserved checksum file is a canonical, fsynced 0/1/2 progress
journal until final evidence promotion; crashes after either receipt or during
the earnings/projection-state check therefore re-prove GET-only state without
issuing another reward.
A pending or failed transaction never passes. Both jobs use a real one-token request, but the first must reach
`mined_success` on all six before the second is submitted. The receipts must
land at two distinct heights (different hashes at one height are a fork, not
two blocks), each carry at least five approvals, and reconcile on every
`/worker/earnings/{worker}` response to exactly 2,500,000,000 base units / 2.5
ARC apiece and exactly 5 ARC gross. Exactly two immediate receipts are not a
rate sample: `attestations_per_day_observed` and `projected_daily_arc` must both
remain null, and both unavailable reasons must exactly say
`collecting data: a projection needs at least 3 successful mined reward receipts spanning at least 24 hours, not the initial one or two rollout canaries`. A numeric rate or
forecast at this boundary fails closed. Counts, local observations, configured
rates, and pending submissions are not earnings. Frontend publication may
proceed with this honest null; it must not wait for or manufacture a forecast.

Before any receipt-mode plan or execution, choose the create-only output that
will carry the two proven identities. The execute command printed by plan mode
preserves this argument:

```bash
--reward-evidence-output /secure/operator/recovery-v3.reward-evidence.json
```

The rollout writes that file and its `.sha256` sidecar mode `0444` only after
both receipts and the six-node exact-gross/null-projection contract pass. Its JSON includes
`schema: arc.recovery.reward-evidence.v1` and the exact rollout SHA-256. It is
never reconstructed from chat output and is never overwritten.

A later read-only audit can use externally captured evidence:

```bash
python3 scripts/recovery/recovery_rollout.py verify \
  --manifest /secure/operator/arc-recovery.lock.json \
  --reward-evidence /secure/operator/recovery-v3.reward-evidence.json
```

## Tests

```bash
python3 -m py_compile \
  scripts/recovery/recovery_rollout.py \
  scripts/recovery/test_recovery_rollout.py
python3 scripts/recovery/test_recovery_rollout.py
python3 scripts/recovery/test_community_reward_probe.py
```

The tests cover manifest strictness, six-validator and restart-quorum rules,
roots-only prearchive finalization, partial-capture and rollout resume,
content-verified create-only archive behavior, both extended GO authorizations,
sealed-source stake proof, exact checkpoint/model/shard commitments, full
remote-object verification, source-path separation, explicit HTTPS origins,
loopback gateway policy, same-height fork rejection, restart command
construction, cgroup-v2 freeze/TERM/thaw ordering, durable signal ambiguity
without SIGKILL, the four-unit condition barrier and volatile control-mask
commit, detached-writer terminal proof, the complete
six-validator restart/height-advance sequence, hash-pinned reward probes, and
successful-receipt-only earnings.
