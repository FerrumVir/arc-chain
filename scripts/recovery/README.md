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
monitoring remains an operator obligation. ACME Renewal Information (ARI)
remains authoritative when the CA supplies it; the generated Caddy config's
`renewal_window_ratio 0.5` is only the fallback schedule, corresponding to
roughly 80 hours remaining on a 160-hour leaf. Only the six validator
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
disabled; its configuration is preserved. Do not assume both ports start
empty: the reviewed LAX baseline has distribution nginx active and enabled on
public port 80 with no port-443 listener. The rollout must stop/disable that
exact baseline before Caddy and rollback must restore it exactly. Any
unaccounted process still holding 80 or 443 is a hard stop.

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
and prints `PLAN ONLY`. It makes no persistent recovery-managed change to a
local or remote directory, file, process/service state, package, proxy,
certificate, or chain data. Production probes stream directly over the pinned
SSH channel and do not install a remote rollout helper before the exact GO
authorization. Each post-archive, root-pinned metadata fetch uses an exact
private mode-`0600` rclone-config copy and an isolated disposable `HOME`.
OAuth refresh and cache writes remain inside that root, which is removed on
success or failure, and the original operator config is re-proved afterward.
Normal SSH and service audit logs may record that read access.

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
recovery-state read-only plan to obtain the verified archive-manifest hash and
exact extended phrase:

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
   block heights, and block hashes. Before the first canary, it fsyncs the
   six-node-agreed complete all-v3 earnings baseline for every worker the
   sealed coordinator could select. The final gate retains every baseline row,
   adds exactly the two 2.5 ARC (2,500,000,000 base-unit) canaries, and requires
   lifetime gross to increase by exactly 5 ARC. A zero-row baseline therefore
   ends at exactly two receipts / 5 ARC and must expose the canonical null
   collecting-data state. A nonempty baseline keeps its historical rows and
   uses the full receipt/timestamp window: a numeric rate is valid only with at
   least three receipts spanning 24 hours, and a numeric projection must equal
   that observed rate times 2.5 ARC; otherwise a concrete unavailable reason is
   required. Every production
   validator is launched with protected `--archive` history retention, every
   earnings response must report `archive_mode=true`, and the tool then
   restarts all six one at a time and re-proves both exact receipts before it
   can complete.

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
The production blocks in this section are one ordered procedure: materialize
and re-prove the protected pre-tag release, restore the six-key vault, create
the verified legacy validator-set copy, prepare/freeze/capture, install the six
keys, export/sign/checkpoint, build the prearchive, and only then archive. Do
not run a later block before an earlier one has completed. Run the blocks in
one reviewed root shell so the verified path/hash variables persist; after a
shell restart, re-establish them from the sealed files and rerun every
preceding read-only verification instead of copying values from history.

Materialize the protected pre-tag inputs on the reviewed Ubuntu x86_64 operator
enclave first. The nine `actions.zip` files are raw GitHub Actions responses,
not inner release archives. The PATH is intentional: the enclave's reviewed
`gh` is not on Ubuntu's default PATH, and the verification helper invokes
`gh` by its bare name. Before entering the shell, dispatch
`.github/workflows/validator-vault-rewrap.yml` from that same protected-main
SHA with source-ciphertext SHA-256
`bdb2dd477fe10e06e63123d6080f321fce4a251479a5af8a59ae2b47814ed7e9`,
restore-certificate SHA-256
`6707f8b1dbc1f2d37d9a873a7e3d2c870d2b46db36f15a6df5293547680bfd43`,
and confirmation `REWRAP ARC VALIDATOR VAULT <protected-main-sha>`. After the
release-environment approval and successful completion, record its exact run
ID and attempt. The procedure proves that workflow/run/commit tuple and the
live one-day artifact through the API before it downloads anything. The
workflow emits the receipt with canonical `jq -cS`; the procedure validates
both that exact byte encoding and every field, then copies those authenticated
artifact bytes unchanged into the create-only restore input. No semantic field
or replacement receipt is accepted from the operator.

<!-- BEGIN EXECUTABLE PRODUCTION RECOVERY PROCEDURE -->

```bash
set -Eeuo pipefail
test "$(uname -s)" = Linux
test "$(uname -m)" = x86_64
test "$(id -u)" = 0
umask 077
export PATH=/secure/operator/tools:/usr/bin:/bin

# Keep the reviewed Ubuntu package/runtime bytes stable for the complete
# recovery window. These are runtime-only masks: a reboot clears them, after
# which every hash check below must be repeated before work resumes.
if /usr/bin/pgrep -a -f '(^|/)(apt|apt-get|dpkg|unattended-upgrade)( |$)'; then
  echo 'package mutation is already running; preserve state and stop' >&2
  exit 1
fi
operator_package_units=(
  apt-daily.service apt-daily-upgrade.service
  apt-daily.timer apt-daily-upgrade.timer
  unattended-upgrades.service
)
/usr/bin/systemctl mask --runtime --now "${operator_package_units[@]}"
/usr/bin/systemctl reset-failed apt-daily.timer apt-daily-upgrade.timer
for unit in "${operator_package_units[@]}"; do
  test "$(/usr/bin/systemctl show --value --property=LoadState "$unit")" = masked
  test "$(/usr/bin/systemctl show --value --property=ActiveState "$unit")" = inactive
  test "$(/usr/bin/systemctl show --value --property=UnitFileState "$unit")" = masked-runtime
done
test "$(/usr/bin/dpkg-query -W python3.12-minimal | /usr/bin/cut -f 2)" = 3.12.3-1ubuntu0.15
test "$(/usr/bin/dpkg-query -W openssh-client | /usr/bin/cut -f 2)" = 1:9.6p1-3ubuntu13.18
test "$(/usr/bin/dpkg-query -W curl | /usr/bin/cut -f 2)" = 8.5.0-2ubuntu10.13
test "$(/usr/bin/dpkg-query -W ca-certificates | /usr/bin/cut -f 2)" = 20260601~24.04.1
test "$(/usr/bin/dpkg-query -W util-linux | /usr/bin/cut -f 2)" = 2.39.3-9ubuntu6.6
protected_main_sha='<exact 40-character protected-main SHA after merge>'
pretag_run_id='<exact successful release-signing-preflight run ID>'
pretag_run_attempt='<exact successful run attempt>'
vault_rewrap_run_id='<exact successful validator-vault-rewrap run ID>'
vault_rewrap_run_attempt='<exact successful rewrap run attempt>'
[[ "$protected_main_sha" =~ ^[0-9a-f]{40}$ ]]
[[ "$pretag_run_id" =~ ^[1-9][0-9]*$ ]]
[[ "$pretag_run_attempt" =~ ^[1-9][0-9]*$ ]]
[[ "$vault_rewrap_run_id" =~ ^[1-9][0-9]*$ ]]
[[ "$vault_rewrap_run_attempt" =~ ^[1-9][0-9]*$ ]]

arc_sha256() {
  /usr/bin/sha256sum -- "$1" | /usr/bin/awk '{print $1}'
}
export ARC_RECOVERY_SSH_USER=root
export ARC_RECOVERY_PYTHON_PATH=/usr/bin/python3.12
export ARC_RECOVERY_PYTHON_SHA256=1643dacd9feaedc58f3cc581e4d22577dfe25c09b10282936186ccf0f2e61118
test -f "$ARC_RECOVERY_PYTHON_PATH" && test ! -L "$ARC_RECOVERY_PYTHON_PATH"
export ARC_RECOVERY_SSH_KNOWN_HOSTS=/secure/operator/arc-validator-known-hosts
export ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256=97c826f7e1a3940f6d18095ccdb0eaeebb5d66ec16fe60b9c5c47690e707485d
export ARC_RECOVERY_SSH_IDENTITY=/secure/operator/arc-validator-maintenance-ed25519
export ARC_RECOVERY_SSH_IDENTITY_SHA256=9a7b57700dc7acf0faeca152fc341f237704e81965b5a9656fe8ccee4931444a
# Capture and fresh stopped-status proofs deliberately use the current
# package-verified /usr/bin transport. Validator-vault installation retains
# the separately reviewed immutable copies under /secure/operator/tools; do
# not reuse one hash for these now-distinct byte identities.
export ARC_RECOVERY_SSH_SHA256=3b0701113d8982d71c8cc74e5a1949f03c6f71da804cf4f3507315afbf07042c
export ARC_RECOVERY_SCP_SHA256=27421348ac188f7381634ce1d521fe9fe774c75cab0d0d2086a052c9bac2da4b
export ARC_RESTORE_SSH_SHA256=47adf415134df7eff017e9557634696ba6b2a09f5a3bb1436d91d99b8a1cd5a6
export ARC_RESTORE_SCP_SHA256=92608e03bd81bf6cd96697ce3379fdf6a4c9bdba6a699f16bcc80cf0f49ce144
export ARC_RECOVERY_RCLONE_PATH=/secure/operator/tools/rclone
export ARC_RECOVERY_RCLONE_SHA256=f3f9aff817f9766029e50adf9a7963c169e475b8f10c7927823568a0d9443db7
export ARC_RECOVERY_RCLONE_CONFIG=/secure/operator/rclone-arc.conf
export ARC_RECOVERY_GH_PATH=/secure/operator/tools/gh
export ARC_RECOVERY_GH_SHA256=c1be595a7357120e28886922c050fed34ad347c36adf37370ad91d4972a416d5
export ARC_RECOVERY_GITHUB_LOGIN=FerrumVir

arc_install_or_reuse_exact() {
  local completed_path="$1"
  local canonical_path="$2"
  "$ARC_RECOVERY_PYTHON_PATH" -I - "$completed_path" "$canonical_path" <<'PY'
import hashlib
import os
import pathlib
import stat
import sys

completed = pathlib.Path(sys.argv[1])
canonical = pathlib.Path(sys.argv[2])
if (
    not completed.is_absolute()
    or not canonical.is_absolute()
    or completed == canonical
    or completed.name in {"", ".", ".."}
    or canonical.name in {"", ".", ".."}
):
    raise SystemExit("completed and canonical outputs must be distinct absolute files")

directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
completed_parent = os.open(completed.parent, directory_flags)
canonical_parent = os.open(canonical.parent, directory_flags)

def require_parent(descriptor, label):
    details = os.fstat(descriptor)
    if (
        not stat.S_ISDIR(details.st_mode)
        or (details.st_uid, details.st_gid) != (0, 0)
        or stat.S_IMODE(details.st_mode) & 0o022
    ):
        raise SystemExit(f"{label} parent is not protected root storage")

def open_locked(parent, name, label):
    try:
        descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=parent)
    except OSError as error:
        raise SystemExit(f"cannot open {label} without following links: {error}") from None
    details = os.fstat(descriptor)
    if (
        not stat.S_ISREG(details.st_mode)
        or (details.st_uid, details.st_gid, stat.S_IMODE(details.st_mode))
        != (0, 0, 0o400)
        or details.st_nlink < 1
        or details.st_size < 1
    ):
        os.close(descriptor)
        raise SystemExit(f"{label} is not a nonempty root-owned mode-0400 regular file")
    return descriptor

def identity(details):
    return (
        details.st_dev,
        details.st_ino,
        details.st_mode,
        details.st_uid,
        details.st_gid,
        details.st_nlink,
        details.st_size,
        details.st_mtime_ns,
        details.st_ctime_ns,
    )

def digest(descriptor, label):
    before = os.fstat(descriptor)
    os.lseek(descriptor, 0, os.SEEK_SET)
    value = hashlib.sha256()
    while True:
        chunk = os.read(descriptor, 1024 * 1024)
        if not chunk:
            break
        value.update(chunk)
    if identity(before) != identity(os.fstat(descriptor)):
        raise SystemExit(f"{label} changed while it was hashed")
    return value.hexdigest(), before.st_size

try:
    require_parent(completed_parent, "completed-output")
    require_parent(canonical_parent, "canonical-output")
    completed_fd = open_locked(completed_parent, completed.name, "completed output")
    os.fsync(completed_fd)
    os.fsync(completed_parent)
    created = False
    try:
        try:
            canonical_fd = open_locked(canonical_parent, canonical.name, "canonical output")
        except SystemExit:
            try:
                os.link(
                    completed.name,
                    canonical.name,
                    src_dir_fd=completed_parent,
                    dst_dir_fd=canonical_parent,
                    follow_symlinks=False,
                )
                os.fsync(canonical_parent)
                created = True
            except FileExistsError:
                pass
            canonical_fd = open_locked(canonical_parent, canonical.name, "canonical output")
        try:
            completed_digest, completed_size = digest(completed_fd, "completed output")
            canonical_digest, canonical_size = digest(canonical_fd, "canonical output")
            if (completed_size, completed_digest) != (canonical_size, canonical_digest):
                raise SystemExit(
                    "canonical output exists with different bytes; preserve both and stop"
                )
        finally:
            os.close(canonical_fd)
    finally:
        os.close(completed_fd)
finally:
    os.close(completed_parent)
    os.close(canonical_parent)
print("created" if created else "reused-exact")
PY
}

for protected_file in \
  "$ARC_RECOVERY_PYTHON_PATH" \
  /usr/bin/ssh /usr/bin/scp \
  /secure/operator/tools/ssh /secure/operator/tools/scp \
  "$ARC_RECOVERY_RCLONE_PATH" "$ARC_RECOVERY_GH_PATH"
do
  test -f "$protected_file" && test ! -L "$protected_file"
done
printf '%s  %s\n' "$ARC_RECOVERY_PYTHON_SHA256" "$ARC_RECOVERY_PYTHON_PATH" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SSH_SHA256" /usr/bin/ssh \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SCP_SHA256" /usr/bin/scp \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RESTORE_SSH_SHA256" /secure/operator/tools/ssh \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RESTORE_SCP_SHA256" /secure/operator/tools/scp \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_RCLONE_SHA256" "$ARC_RECOVERY_RCLONE_PATH" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_GH_SHA256" "$ARC_RECOVERY_GH_PATH" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256" \
  "$ARC_RECOVERY_SSH_KNOWN_HOSTS" | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SSH_IDENTITY_SHA256" \
  "$ARC_RECOVERY_SSH_IDENTITY" | /usr/bin/sha256sum --check --strict
test "$(/usr/bin/stat --format='%a:%h' "$ARC_RECOVERY_SSH_KNOWN_HOSTS")" = 400:1
test "$(/usr/bin/stat --format='%a:%h' "$ARC_RECOVERY_SSH_IDENTITY")" = 400:1
test -f "$ARC_RECOVERY_RCLONE_CONFIG" && test ! -L "$ARC_RECOVERY_RCLONE_CONFIG"
test "$(/usr/bin/stat --format='%a:%h' "$ARC_RECOVERY_RCLONE_CONFIG")" = 600:1
rclone_config_sha256="$(arc_sha256 "$ARC_RECOVERY_RCLONE_CONFIG")"
[[ "$rclone_config_sha256" =~ ^[0-9a-f]{64}$ ]]
test -f /secure/operator/restore.cert.pem \
  && test ! -L /secure/operator/restore.cert.pem
test -f /secure/operator/restore.key.pem \
  && test ! -L /secure/operator/restore.key.pem
test "$(/usr/bin/stat --format='%a:%h' /secure/operator/restore.cert.pem)" = 600:1
test "$(/usr/bin/stat --format='%a:%h' /secure/operator/restore.key.pem)" = 600:1
printf '%s  %s\n' \
  6707f8b1dbc1f2d37d9a873a7e3d2c870d2b46db36f15a6df5293547680bfd43 \
  /secure/operator/restore.cert.pem | /usr/bin/sha256sum --check --strict

GH_TOKEN="$(
  "$ARC_RECOVERY_GH_PATH" auth token --hostname github.com \
    --user "$ARC_RECOVERY_GITHUB_LOGIN"
)"
test -n "$GH_TOKEN"
export GH_TOKEN
test "$("$ARC_RECOVERY_GH_PATH" api /user --jq .login)" = \
  "$ARC_RECOVERY_GITHUB_LOGIN"
test "$("$ARC_RECOVERY_GH_PATH" api repos/FerrumVir/arc-chain/branches/main \
  --jq .commit.sha)" = "$protected_main_sha"

git_home="$(/usr/bin/mktemp -d /secure/operator/git-home.XXXXXXXX)"
operator_checkout="$(
  /usr/bin/mktemp -d "/secure/operator/arc-chain.$protected_main_sha.XXXXXXXX"
)"
arc_git() {
  /usr/bin/env -i HOME="$git_home" PATH=/usr/bin:/bin LANG=C LC_ALL=C \
    GIT_CONFIG_NOSYSTEM=1 /usr/bin/git "$@"
}
arc_git clone --no-tags --filter=blob:none --no-checkout --single-branch \
  --branch main -- https://github.com/FerrumVir/arc-chain.git "$operator_checkout"
test "$(arc_git -C "$operator_checkout" remote get-url origin)" = \
  https://github.com/FerrumVir/arc-chain.git
test "$(arc_git -C "$operator_checkout" rev-parse refs/remotes/origin/main)" = \
  "$protected_main_sha"
arc_git -C "$operator_checkout" checkout --detach "$protected_main_sha"
test "$(arc_git -C "$operator_checkout" rev-parse HEAD)" = "$protected_main_sha"
arc_git -C "$operator_checkout" diff-index --quiet HEAD --
test -z "$(arc_git -C "$operator_checkout" status --porcelain=v1 --untracked-files=all)"
cd "$operator_checkout"
test "$PWD" = "$operator_checkout"

rewrap_download_root="$(
  /usr/bin/mktemp -d "/secure/operator/vault-rewrap.$vault_rewrap_run_id.XXXXXXXX"
)"
rewrap_run_json="$rewrap_download_root/RUN.json"
rewrap_artifacts_json="$rewrap_download_root/ARTIFACTS.json"
rewrap_raw_zip="$rewrap_download_root/actions.zip"
(
  set -o noclobber
  gh api "repos/FerrumVir/arc-chain/actions/runs/$vault_rewrap_run_id" \
    > "$rewrap_run_json"
)
jq -e \
  --arg commit "$protected_main_sha" \
  --argjson run_id "$vault_rewrap_run_id" \
  --argjson run_attempt "$vault_rewrap_run_attempt" '
    .id == $run_id
    and .run_attempt == $run_attempt
    and .event == "workflow_dispatch"
    and .status == "completed"
    and .conclusion == "success"
    and .head_branch == "main"
    and .head_sha == $commit
    and .path == ".github/workflows/validator-vault-rewrap.yml"
    and .repository.full_name == "FerrumVir/arc-chain"
    and .head_repository.full_name == "FerrumVir/arc-chain"
  ' "$rewrap_run_json" >/dev/null
(
  set -o noclobber
  gh api --paginate \
    "repos/FerrumVir/arc-chain/actions/runs/$vault_rewrap_run_id/artifacts?per_page=100" \
    --jq '.artifacts[]' | jq -s '{artifacts: .}' > "$rewrap_artifacts_json"
)
rewrap_artifact_prefix="arc-validator-vault-rewrap-$protected_main_sha-6707f8b1dbc1f2d37d9a873a7e3d2c870d2b46db36f15a6df5293547680bfd43-"
rewrap_artifact_json="$(
  jq -cer \
    --arg prefix "$rewrap_artifact_prefix" \
    --arg commit "$protected_main_sha" \
    --argjson run_id "$vault_rewrap_run_id" '
      [.artifacts[]
       | select(
           .expired == false
           and (.name | test("^" + $prefix + "[0-9a-f]{64}$"))
           and (.digest | test("^sha256:[0-9a-f]{64}$"))
           and .workflow_run.id == $run_id
           and .workflow_run.head_branch == "main"
           and .workflow_run.head_sha == $commit
         )]
      | if length == 1 then .[0]
        else error("expected exactly one live exact-run rewrap artifact") end
    ' "$rewrap_artifacts_json"
)"
rewrap_artifact_id="$(jq -er '.id' <<<"$rewrap_artifact_json")"
rewrap_artifact_name="$(jq -er '.name' <<<"$rewrap_artifact_json")"
rewrap_artifact_digest="$(jq -er '.digest | sub("^sha256:"; "")' \
  <<<"$rewrap_artifact_json")"
rewrap_artifact_size="$(jq -er '.size_in_bytes' <<<"$rewrap_artifact_json")"
[[ "$rewrap_artifact_id" =~ ^[1-9][0-9]*$ ]]
[[ "$rewrap_artifact_digest" =~ ^[0-9a-f]{64}$ ]]
[[ "$rewrap_artifact_size" =~ ^[1-9][0-9]*$ ]]
test "$rewrap_artifact_size" -le 4194304
(
  set -o noclobber
  gh api -H 'Accept: application/vnd.github+json' \
    "repos/FerrumVir/arc-chain/actions/artifacts/$rewrap_artifact_id/zip" \
    | /usr/bin/head --bytes="$((rewrap_artifact_size + 1))" > "$rewrap_raw_zip"
)
chmod 0400 "$rewrap_run_json" "$rewrap_artifacts_json" "$rewrap_raw_zip"
printf '%s  %s\n' "$rewrap_artifact_digest" "$rewrap_raw_zip" \
  | /usr/bin/sha256sum --check --strict
test "$(/usr/bin/stat --format='%s' "$rewrap_raw_zip")" = \
  "$rewrap_artifact_size"

cms_path=/secure/operator/arc-validator-keys-v0.8.0.tar.cms
rewrap_receipt=/secure/operator/REWRAP-RECEIPT.json
test ! -e "$cms_path"
test ! -e "$rewrap_receipt"
cms_sha256="$(
  /usr/bin/python3.12 -I - \
    "$rewrap_raw_zip" "$rewrap_download_root" "$rewrap_artifact_name" \
    "$protected_main_sha" "$cms_path" "$rewrap_receipt" <<'PY'
import hashlib
import json
import os
import pathlib
import stat
import sys
import zipfile

zip_path, root_raw, artifact_name, commit, cms_output_raw, receipt_output_raw = sys.argv[1:]
root = pathlib.Path(root_raw)
cms_output = pathlib.Path(cms_output_raw)
receipt_output = pathlib.Path(receipt_output_raw)
expected_names = {
    "arc-validator-keys-v0.8.0.tar.cms": 2 * 1024 * 1024,
    "REWRAP-RECEIPT.json": 64 * 1024,
    "SHA256SUMS": 1024,
}
expected_cert = "6707f8b1dbc1f2d37d9a873a7e3d2c870d2b46db36f15a6df5293547680bfd43"
expected_source = "bdb2dd477fe10e06e63123d6080f321fce4a251479a5af8a59ae2b47814ed7e9"
expected_cms = artifact_name.rsplit("-", 1)[-1]
if len(expected_cms) != 64 or any(c not in "0123456789abcdef" for c in expected_cms):
    raise SystemExit("rewrap artifact name does not end in one lowercase SHA-256")

payloads = {}
with zipfile.ZipFile(zip_path, "r") as archive:
    infos = archive.infolist()
    if len(infos) != len(expected_names) or {item.filename for item in infos} != set(expected_names):
        raise SystemExit("rewrap Actions ZIP does not contain the exact three flat files")
    if len({item.filename.casefold() for item in infos}) != len(infos):
        raise SystemExit("rewrap Actions ZIP contains a case-folding collision")
    for item in infos:
        kind = stat.S_IFMT(item.external_attr >> 16)
        if (
            item.filename != pathlib.PurePosixPath(item.filename).name
            or item.is_dir()
            or item.flag_bits & 1
            or kind not in (0, stat.S_IFREG)
            or not 0 < item.file_size <= expected_names[item.filename]
        ):
            raise SystemExit("rewrap Actions ZIP member violates the bounded regular-file contract")
        payload = archive.read(item)
        if len(payload) != item.file_size:
            raise SystemExit("rewrap Actions ZIP member changed size while read")
        payloads[item.filename] = payload

cms = payloads["arc-validator-keys-v0.8.0.tar.cms"]
cms_sha = hashlib.sha256(cms).hexdigest()
if cms_sha != expected_cms:
    raise SystemExit("rewrap CMS bytes differ from the artifact-name digest")
if payloads["SHA256SUMS"] != (
    f"{cms_sha}  arc-validator-keys-v0.8.0.tar.cms\n".encode("ascii")
):
    raise SystemExit("rewrap SHA256SUMS does not bind exactly the CMS member")
try:
    receipt = json.loads(payloads["REWRAP-RECEIPT.json"])
except (UnicodeDecodeError, json.JSONDecodeError) as error:
    raise SystemExit("rewrap receipt is not valid UTF-8 JSON") from error
expected_receipt = {
    "schema": "arc.validator-vault-rewrap.v1",
    "source_commit": commit,
    "source_ciphertext_sha256": expected_source,
    "restore_cert_sha256": expected_cert,
    "cms_sha256": cms_sha,
    "key_transport": "RSA-OAEP-SHA256",
    "content_encryption": "AES-256-GCM",
}
if receipt != expected_receipt:
    raise SystemExit("rewrap receipt differs from the exact authorized tuple")
canonical_receipt = (
    json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n"
).encode("utf-8")
if payloads["REWRAP-RECEIPT.json"] != canonical_receipt:
    raise SystemExit("rewrap receipt bytes are not the canonical restore encoding")

def create(path, payload):
    parent_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        descriptor = os.open(
            path.name,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
            0o600,
            dir_fd=parent_fd,
        )
        try:
            offset = 0
            while offset < len(payload):
                offset += os.write(descriptor, payload[offset:])
            os.fsync(descriptor)
            os.fchmod(descriptor, 0o600)
        finally:
            os.close(descriptor)
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)

create(cms_output, cms)
create(receipt_output, payloads["REWRAP-RECEIPT.json"])
print(cms_sha)
PY
)"
[[ "$cms_sha256" =~ ^[0-9a-f]{64}$ ]]
printf '%s  %s\n' "$cms_sha256" "$cms_path" \
  | /usr/bin/sha256sum --check --strict
test "$(/usr/bin/stat --format='%a:%h' "$cms_path")" = 600:1
test "$(/usr/bin/stat --format='%a:%h' "$rewrap_receipt")" = 600:1

pretag_api_json=/secure/operator/PRETAG-ARTIFACTS-API.json
pretag_selection_json=/secure/operator/PRETAG-SELECTION.json
pretag_raw_root=/secure/pretag/raw-v0.8.0
pretag_input_set=/secure/operator/PRETAG-ARTIFACT-INPUT-SET.json
test ! -e "$pretag_api_json"
test ! -e "$pretag_selection_json"
test ! -e "$pretag_raw_root"
test ! -e "$pretag_input_set"
install -d -m 0700 "$pretag_raw_root"

(
  set -o noclobber
  gh api --paginate \
    "repos/FerrumVir/arc-chain/actions/runs/$pretag_run_id/artifacts?per_page=100" \
    --jq '.artifacts[]' | jq -s '{artifacts: .}' > "$pretag_api_json"
)
(
  set -o noclobber
  "$ARC_RECOVERY_PYTHON_PATH" -I scripts/release/select-pretag-artifacts.py \
    --api-json "$pretag_api_json" \
    --repository FerrumVir/arc-chain \
    --commit "$protected_main_sha" \
    --run-id "$pretag_run_id" \
    --run-attempt "$pretag_run_attempt" > "$pretag_selection_json"
)
chmod 0400 "$pretag_api_json" "$pretag_selection_json"
pretag_artifacts_json="$(jq -cS '.artifacts' "$pretag_selection_json")"
pretag_linux_x86_64_artifact_id="$(
  jq -er '.artifacts["linux-x86_64"].headless.id' "$pretag_selection_json"
)"
[[ "$pretag_linux_x86_64_artifact_id" =~ ^[1-9][0-9]*$ ]]
scripts/release/verify-pretag-run-and-artifacts.sh \
  FerrumVir/arc-chain \
  "$protected_main_sha" \
  "$pretag_run_id" \
  "$pretag_run_attempt" \
  "$pretag_artifacts_json"

while IFS=$'\t' read -r kind platform artifact_id artifact_digest artifact_size; do
  group_root="$pretag_raw_root/$kind-$platform"
  raw_zip="$group_root/actions.zip"
  [[ "$artifact_id" =~ ^[1-9][0-9]*$ ]]
  [[ "$artifact_digest" =~ ^sha256:[0-9a-f]{64}$ ]]
  [[ "$artifact_size" =~ ^[1-9][0-9]*$ ]]
  test "$artifact_size" -le 4294967296
  install -d -m 0700 "$group_root"
  (
    set -o noclobber
    gh api -H 'Accept: application/vnd.github+json' \
      "repos/FerrumVir/arc-chain/actions/artifacts/$artifact_id/zip" \
      | /usr/bin/head --bytes="$((artifact_size + 1))" > "$raw_zip"
  )
  chmod 0400 "$raw_zip"
  printf '%s  %s\n' "${artifact_digest#sha256:}" "$raw_zip" \
    | /usr/bin/sha256sum --check --strict
  test "$(/usr/bin/stat --format='%s' "$raw_zip")" = "$artifact_size"
done < <(
  jq -r '
    .artifacts as $a
    | [["headless","linux-x86_64"],
       ["headless","linux-arm64"],
       ["headless","macos-arm64"],
       ["headless","macos-x86_64"],
       ["headless","windows-x86_64"],
       ["desktop","linux-x86_64"],
       ["desktop","macos-arm64"],
       ["desktop","macos-x86_64"],
       ["desktop","windows-x86_64"]][]
    | .[0] as $kind | .[1] as $platform
    | [$kind, $platform, ($a[$platform][$kind].id | tostring),
       $a[$platform][$kind].digest,
       ($a[$platform][$kind].size_in_bytes | tostring)]
    | @tsv
  ' "$pretag_selection_json"
)

(
  set -o noclobber
  jq -cnS \
    --slurpfile selected "$pretag_selection_json" \
    --arg repository FerrumVir/arc-chain \
    --arg commit "$protected_main_sha" \
    --argjson run_id "$pretag_run_id" \
    --argjson run_attempt "$pretag_run_attempt" \
    --arg raw_root "$pretag_raw_root" '
      $selected[0] as $s
      | [["headless","linux-x86_64"],
         ["headless","linux-arm64"],
         ["headless","macos-arm64"],
         ["headless","macos-x86_64"],
         ["headless","windows-x86_64"],
         ["desktop","linux-x86_64"],
         ["desktop","macos-arm64"],
         ["desktop","macos-x86_64"],
         ["desktop","windows-x86_64"]] as $groups
      | {
          schema: "arc.recovery.pretag-artifact-input-set.v1",
          repository: $repository,
          commit: $commit,
          run_id: $run_id,
          run_attempt: $run_attempt,
          artifacts: ($groups | map(
            .[0] as $kind | .[1] as $platform
            | {
                kind: $kind,
                platform: $platform,
                artifact_id: $s.artifacts[$platform][$kind].id,
                raw_actions_zip:
                  ($raw_root + "/" + $kind + "-" + $platform + "/actions.zip")
              }
          ))
        }
    ' > "$pretag_input_set"
)
chmod 0400 "$pretag_input_set"
unset GH_TOKEN

pretag_materialized=/secure/operator/pretag-materialized-v0.8.0
test ! -e "$pretag_materialized"
"$ARC_RECOVERY_PYTHON_PATH" -I scripts/release/materialize-pretag-artifacts.py \
  --downloads-root "$pretag_raw_root/headless-linux-x86_64" \
  --output-dir "$pretag_materialized" \
  --repository FerrumVir/arc-chain \
  --commit "$protected_main_sha" \
  --run-id "$pretag_run_id" \
  --run-attempt "$pretag_run_attempt" \
  --version 0.8.0 \
  --selection-json "$pretag_artifacts_json" \
  --only headless:linux-x86_64 \
  --retain-build-metadata
arc_node_linux=/secure/operator/pretag-materialized-v0.8.0/headless-linux-x86_64/arc-node-linux-x86_64
operator_genesis=/secure/operator/pretag-materialized-v0.8.0/headless-linux-x86_64/genesis.toml
pretag_build_metadata=/secure/operator/pretag-materialized-v0.8.0/headless-linux-x86_64/BUILD-METADATA.json
test -x "$arc_node_linux" && test ! -L "$arc_node_linux"
test -f "$operator_genesis" && test ! -L "$operator_genesis"
arc_node_linux_sha256="$(jq -er '.files["arc-node-linux-x86_64"]' "$pretag_build_metadata")"
genesis_sha256="$(jq -er '.files["genesis.toml"]' "$pretag_build_metadata")"
printf '%s  %s\n' "$arc_node_linux_sha256" "$arc_node_linux" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$genesis_sha256" "$operator_genesis" \
  | /usr/bin/sha256sum --check --strict
```

Restore the vault with the exact Ubuntu proof/runtime bytes provisioned in the
enclave. Any digest mismatch stops before CMS decryption.

```bash
ARC_PROOF_CURL=/usr/bin/curl
ARC_PROOF_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
ARC_PROOF_CURL_SHA256=74b4ce8f74b377f18ef1b3df7279c26cb3cd14c49e39ab1498575b209dc3f70f
ARC_PROOF_CA_BUNDLE_SHA256=ecd9dc38bc3efb7dbd6431f57e29d2f8d6a0f0d211e1464b3fef2cbfe266fcd2
printf '%s  %s\n' "$ARC_PROOF_CURL_SHA256" "$ARC_PROOF_CURL" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_PROOF_CA_BUNDLE_SHA256" "$ARC_PROOF_CA_BUNDLE" \
  | /usr/bin/sha256sum --check --strict

"$ARC_RECOVERY_PYTHON_PATH" -I scripts/release/restore-validator-vault.py restore \
  --cms "$cms_path" \
  --expected-cms-sha256 "$cms_sha256" \
  --rewrap-receipt "$rewrap_receipt" \
  --source-main-sha "$protected_main_sha" \
  --raw-actions-zip "$pretag_raw_root/headless-linux-x86_64/actions.zip" \
  --pretag-run-id "$pretag_run_id" \
  --pretag-run-attempt "$pretag_run_attempt" \
  --pretag-artifact-id "$pretag_linux_x86_64_artifact_id" \
  --curl "$ARC_PROOF_CURL" \
  --curl-sha256 "$ARC_PROOF_CURL_SHA256" \
  --ca-bundle "$ARC_PROOF_CA_BUNDLE" \
  --ca-bundle-sha256 "$ARC_PROOF_CA_BUNDLE_SHA256" \
  --restore-certificate /secure/operator/restore.cert.pem \
  --restore-private-key /secure/operator/restore.key.pem \
  --openssl /secure/operator/tools/openssl-3.0.13 \
  --openssl-sha256 724acbe911513d13f52bae0b8969b20336cd8618fc67898a6bf7847bf1a270ad \
  --openssl-libssl /secure/operator/tools/libssl.so.3 \
  --openssl-libssl-sha256 0c0f298a5b4b44526d20a07d126a55bf44b85eaab053b2b0118e5d806d28ea13 \
  --openssl-libcrypto /secure/operator/tools/libcrypto.so.3 \
  --openssl-libcrypto-sha256 d6fc1bc9de29c55fc905f77edba1ccc7c7a50b32bd2bb9086b0d0b00104eafc4 \
  --output-dir /secure/operator/arc-v0.8-validator-restore

validator_public_keys=/secure/operator/arc-v0.8-validator-restore/validator-public-keys.json
test -f "$validator_public_keys" && test ! -L "$validator_public_keys"
validator_public_keys_sha256="$(/usr/bin/sha256sum "$validator_public_keys" | /usr/bin/awk '{print $1}')"
```

Create the legacy-set operator input only from the exact protected checkout.
Both tracked blobs and their tracked checksum must verify before an
`O_EXCL|O_NOFOLLOW` copy is created.

```bash
test "$(arc_git rev-parse HEAD)" = "$protected_main_sha"
legacy_source="$PWD/scripts/recovery/legacy-validator-set-40m.json"
legacy_source_sidecar="$legacy_source.sha256"
test "$(arc_git hash-object "$legacy_source")" = \
  "$(arc_git rev-parse "$protected_main_sha:scripts/recovery/legacy-validator-set-40m.json")"
test "$(arc_git hash-object "$legacy_source_sidecar")" = \
  "$(arc_git rev-parse "$protected_main_sha:scripts/recovery/legacy-validator-set-40m.json.sha256")"
(cd scripts/recovery && /usr/bin/sha256sum --check --strict legacy-validator-set-40m.json.sha256)
legacy_validator_set=/secure/operator/legacy-validator-set-40m.json
/usr/bin/python3.12 -I - "$legacy_source" "$legacy_validator_set" <<'PY'
import os, stat, sys
source, destination = sys.argv[1:]
source_fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW)
try:
    metadata = os.fstat(source_fd)
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit("legacy validator source is not a regular file")
    destination_fd = os.open(
        destination,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
        0o400,
    )
    try:
        while True:
            chunk = os.read(source_fd, 65536)
            if not chunk:
                break
            view = memoryview(chunk)
            while view:
                written = os.write(destination_fd, view)
                view = view[written:]
        os.fsync(destination_fd)
    finally:
        os.close(destination_fd)
finally:
    os.close(source_fd)
PY
test -f "$legacy_validator_set" && test ! -L "$legacy_validator_set"
test "$(/usr/bin/stat --format='%a' "$legacy_validator_set")" = 400
legacy_validator_set_sha256="$(/usr/bin/sha256sum "$legacy_validator_set" | /usr/bin/awk '{print $1}')"
test "$legacy_validator_set_sha256" = 1615413b0cad59eedc8f9aa8ce41427e866f4b868f5b2148be48a1d722d7a3db
```

The v5 plan also binds the ARC Drive gate bytes, exact remote root,
hashed custom OAuth client ID, hashed account, reviewed remaining daily upload
budget, and the operator's dedicated-uploader attestation. The 700000000000-byte
(700 GB decimal) reservation leaves a 50000000000-byte (50 GB) safety margin
below Google's 750000000000-byte (750 GB decimal) per-24-hour upload cap in the
[official Drive API limits](https://developers.google.com/workspace/drive/api/guides/limits).
It is valid only after the operator has checked the current quota window and
confirmed that no other process, host, scheduled job, or human will upload
through this Google account until ARC finishes. The typed phrase is the
operational attestation for those two facts; the flag alone is not evidence
that they were reviewed. Only now prepare, freeze, and capture the six legacy
writers:

```bash
drive_client_id_sha256=73c7bd17ff0e6e52331a5adf7574e492f137ef52f9b288908413901f33c723b1
drive_account_sha256=29a77804fd021a47d43afaf1c51c2a877c66ff56699e1d3173be6d57536b8e3b
drive_daily_upload_budget_bytes=700000000000
drive_quota_attestation_phrase='I ATTEST 700000000000 BYTES REMAIN AND ARC IS THE ONLY DRIVE UPLOAD WRITER THIS QUOTA WINDOW'
printf 'Type exactly: %s\n> ' "$drive_quota_attestation_phrase" >/dev/tty
IFS= read -r drive_quota_attestation </dev/tty
test "$drive_quota_attestation" = "$drive_quota_attestation_phrase"
unset drive_quota_attestation

# Re-prove every canonical pre-capture transport byte immediately before the
# first archive orchestrator process. The mutable OAuth config is not assigned
# a stale reviewed hash; its selected client/account are proven by the gate.
printf '%s  %s\n' "$ARC_RECOVERY_PYTHON_SHA256" "$ARC_RECOVERY_PYTHON_PATH" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256" \
  "$ARC_RECOVERY_SSH_KNOWN_HOSTS" | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SSH_IDENTITY_SHA256" \
  "$ARC_RECOVERY_SSH_IDENTITY" | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SSH_SHA256" /usr/bin/ssh \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SCP_SHA256" /usr/bin/scp \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_RCLONE_SHA256" "$ARC_RECOVERY_RCLONE_PATH" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_GH_SHA256" "$ARC_RECOVERY_GH_PATH" \
  | /usr/bin/sha256sum --check --strict
test "$(arc_sha256 "$ARC_RECOVERY_RCLONE_CONFIG")" = "$rclone_config_sha256"

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

# Copy these exact values from the successful seal-freeze-plan output. Give this
# attempt a unique create-only receipt path, but do not sample it here. The
# execute phase samples only after the slow inspector, Drive, and live-
# observation prerequisites, immediately before the authenticated cross-proof.
freeze_sha256='<freeze-plan hash printed by seal-freeze-plan>'
capture_id='<capture id printed by seal-freeze-plan>'
[[ "$freeze_sha256" =~ ^[0-9a-f]{64}$ ]]
[[ "$capture_id" =~ ^[0-9a-f]{64}$ ]]
legacy_height_attempt_nonce="$(
  "$ARC_RECOVERY_PYTHON_PATH" -I -c 'import secrets; print(secrets.token_hex(16))'
)"
[[ "$legacy_height_attempt_nonce" =~ ^[0-9a-f]{32}$ ]]
legacy_public_height_receipt="/secure/operator/legacy-public-height.${capture_id}.${legacy_height_attempt_nonce}.json"
offline_stop_output=/secure/operator/arc-offline-stop-evidence.json
test ! -e "$legacy_public_height_receipt" && test ! -L "$legacy_public_height_receipt"

scripts/recovery/archive-fleet-to-drive.sh capture \
  --freeze-plan /secure/operator/arc-freeze.lock.json \
  --sample-legacy-public-height-output "$legacy_public_height_receipt" \
  --inspector-binary "$arc_node_linux" \
  --inspector-binary-sha256 "$arc_node_linux_sha256" \
  --genesis "$operator_genesis" \
  --genesis-sha256 "$genesis_sha256" \
  --validator-public-keys "$validator_public_keys" \
  --validator-public-keys-sha256 "$validator_public_keys_sha256" \
  --legacy-validator-set "$legacy_validator_set" \
  --legacy-validator-set-sha256 "$legacy_validator_set_sha256" \
  --offline-stop-evidence-output "$offline_stop_output"

ARC_RECOVERY_FREEZE_GO="FREEZE $freeze_sha256 CAPTURE $capture_id" \
  scripts/recovery/archive-fleet-to-drive.sh capture \
    --freeze-plan /secure/operator/arc-freeze.lock.json \
    --sample-legacy-public-height-output "$legacy_public_height_receipt" \
    --inspector-binary "$arc_node_linux" \
    --inspector-binary-sha256 "$arc_node_linux_sha256" \
    --genesis "$operator_genesis" \
    --genesis-sha256 "$genesis_sha256" \
    --validator-public-keys "$validator_public_keys" \
    --validator-public-keys-sha256 "$validator_public_keys_sha256" \
    --legacy-validator-set "$legacy_validator_set" \
    --legacy-validator-set-sha256 "$legacy_validator_set_sha256" \
    --offline-stop-evidence-output "$offline_stop_output" \
    --execute

legacy_public_height_sha256="$(arc_sha256 "$legacy_public_height_receipt")"
[[ "$legacy_public_height_sha256" =~ ^[0-9a-f]{64}$ ]]
printf 'legacy public-height receipt path=%s sha256=%s capture=%s\n' \
  "$legacy_public_height_receipt" "$legacy_public_height_sha256" "$capture_id"
```

If execution exits after creating the late receipt but before sealing the
authenticated cross-proof, leave every byte in place. After proving there is
no selection, mutation dispatch, quarantine ledger, or first boundary and all
six exact writers remain live and unfenced, choose a new receipt nonce. Keep
the existing offline-stop namespace only when no authenticated-cross `.partial`
exists; if that create-only partial exists, preserve it and choose a new
offline-stop namespace too. A completed cross-proof must instead resume with
its exact original receipt path and hash; it must never be resampled.

Install the restored keys only after that successful capture. All three
maintenance artifacts and sidecars are required by the current parser; their
digests are derived from the just-created bytes, not copied from an earlier
attempt.

```bash
legacy_maintenance_evidence_bundle="$offline_stop_output.legacy-maintenance-evidence-bundle.json"
legacy_maintenance_evidence_bundle_sidecar="$legacy_maintenance_evidence_bundle.sha256"
legacy_maintenance_evidence_bundle_sha256="$(/usr/bin/sha256sum "$legacy_maintenance_evidence_bundle" | /usr/bin/awk '{print $1}')"
legacy_maintenance_boundary="$offline_stop_output.legacy-maintenance-boundary.json"
legacy_maintenance_boundary_sidecar="$legacy_maintenance_boundary.sha256"
legacy_maintenance_boundary_sha256="$(/usr/bin/sha256sum "$legacy_maintenance_boundary" | /usr/bin/awk '{print $1}')"
offline_stop_evidence="$offline_stop_output"
offline_stop_evidence_sidecar="$offline_stop_evidence.sha256"
offline_stop_evidence_sha256="$(/usr/bin/sha256sum "$offline_stop_evidence" | /usr/bin/awk '{print $1}')"
known_hosts="$ARC_RECOVERY_SSH_KNOWN_HOSTS"
known_hosts_sha256="$ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256"
ssh_identity="$ARC_RECOVERY_SSH_IDENTITY"
ssh_identity_sha256="$ARC_RECOVERY_SSH_IDENTITY_SHA256"
printf '%s  %s\n' "$known_hosts_sha256" "$known_hosts" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ssh_identity_sha256" "$ssh_identity" \
  | /usr/bin/sha256sum --check --strict

"$ARC_RECOVERY_PYTHON_PATH" -I scripts/release/restore-validator-vault.py install \
  --restore-receipt /secure/operator/arc-v0.8-validator-restore/RESTORE-RECEIPT.json \
  --source-main-sha "$protected_main_sha" \
  --raw-actions-zip "$pretag_raw_root/headless-linux-x86_64/actions.zip" \
  --pretag-run-id "$pretag_run_id" \
  --pretag-run-attempt "$pretag_run_attempt" \
  --pretag-artifact-id "$pretag_linux_x86_64_artifact_id" \
  --curl "$ARC_PROOF_CURL" \
  --curl-sha256 "$ARC_PROOF_CURL_SHA256" \
  --ca-bundle "$ARC_PROOF_CA_BUNDLE" \
  --ca-bundle-sha256 "$ARC_PROOF_CA_BUNDLE_SHA256" \
  --freeze-plan /secure/operator/arc-freeze.lock.json \
  --freeze-plan-sidecar /secure/operator/arc-freeze.lock.json.sha256 \
  --freeze-plan-sha256 "$freeze_sha256" \
  --legacy-maintenance-evidence-bundle "$legacy_maintenance_evidence_bundle" \
  --legacy-maintenance-evidence-bundle-sidecar "$legacy_maintenance_evidence_bundle_sidecar" \
  --legacy-maintenance-evidence-bundle-sha256 "$legacy_maintenance_evidence_bundle_sha256" \
  --legacy-maintenance-boundary "$legacy_maintenance_boundary" \
  --legacy-maintenance-boundary-sidecar "$legacy_maintenance_boundary_sidecar" \
  --legacy-maintenance-boundary-sha256 "$legacy_maintenance_boundary_sha256" \
  --offline-stop-evidence "$offline_stop_evidence" \
  --offline-stop-evidence-sidecar "$offline_stop_evidence_sidecar" \
  --offline-stop-evidence-sha256 "$offline_stop_evidence_sha256" \
  --known-hosts "$known_hosts" \
  --known-hosts-sha256 "$known_hosts_sha256" \
  --ssh-identity "$ssh_identity" \
  --ssh-identity-sha256 "$ssh_identity_sha256" \
  --ssh /secure/operator/tools/ssh \
  --ssh-sha256 "$ARC_RESTORE_SSH_SHA256" \
  --scp /secure/operator/tools/scp \
  --scp-sha256 "$ARC_RESTORE_SCP_SHA256" \
  --receipt-output /secure/operator/VALIDATOR-KEY-INSTALL-RECEIPT.json
```

Every archive command that initializes Python, SSH, or Drive runs as the leader
of a dedicated, invocation-scoped process group and installs its cleanup
handler before argument parsing or configuration. It creates a private
mode-0700 dispatcher gate beneath the caller's `TMPDIR` (beneath the validated
`--work-root` for `seal`) and exports only that gate's private runtime child as
the phase `TMPDIR`. The Python HOME, transport root, and every other invocation
scratch directory are therefore physically contained by the gate. The phase
copies `known_hosts` and `id_ed25519` at mode 0400 and, for a
Drive command, copies the reviewed rclone executable at mode 0500 plus a
disposable mode-0600 rclone config copy. All `ssh`, `scp`, and `rclone` calls
then use only those copies in a clean environment. A token refresh can change
the disposable config copy, but the source SSH identity and rclone config
remain byte-for-byte unchanged. The invocation removes all of these roots on
a normal success, plan return, or fail-closed error, including an error partway
through configuration. Nested `prepare-writers` -> `audit-writers` execution
owns separate roots, so the nested cleanup cannot remove its parent's active
transport. The dispatcher forwards a parent-targeted `SIGHUP`, `SIGINT`, or
`SIGTERM` exactly once to the entire phase group, waits until the phase is
reaped, drains any surviving same-group descendants, and then independently
removes and verifies absence of the whole gate before returning 129, 130, or
143 respectively. The phase gets a bounded opportunity to finish its EXIT
cleanup; the supervisor-owned final gate sweep is authoritative and also
removes bytes a signal-ignoring descendant tries to recreate after that
cleanup starts. Before the sweep, the separately grouped guardian drains the
phase group. The guardian leads its own process group, which is what makes its
membership queries trustworthy; it retries bounded TERM and then exact-PID KILL
of every member except the sentinel until that group is verifiably empty, and
only then publishes its completion receipt. The sentinel exists to anchor the
phase PGID so it can never be reused while members are being killed by exact
PID, and it sweeps the gate on that receipt alone, verifying absence. The
sentinel must never query phase-group membership itself: it runs inside that
group, so its own `ps` child is counted as a member and the sweep would never
fire. An unsignaled internal cleanup failure returns 125; a run that already
received HUP, INT, or TERM preserves its required 129, 130, or 143 status while
reporting that containment is continuing.
The same guardian takes over if the dispatcher is lost to `SIGKILL`; it first
gives the cleanup-owning phase leader a TERM path, then kills a TERM-ignoring
foreground descendant so the deferred cleanup or final gate sweep can finish.
Direct `SIGKILL` of both the phase and its guardian, or loss of the operator
host/kernel, cannot run an EXIT handler or final sweep; securely remove any
private mode-0700 dispatcher-gate orphan before retrying after either event.

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
hash-binds the unredacted custom client ID from that same in-memory stream,
then uses verified TLS and bounded retries/timeouts for exactly
`GET https://www.googleapis.com/drive/v3/about?fields=user(emailAddress,permissionId,me)`.
It requires `me=true`, one normalized email, and one permission ID, and emits
only the client/account/permission SHA-256 hashes. Access/refresh tokens,
client secret, raw client ID, raw email, raw
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

The remote quarantine itself is a crash-safe sequence of immutable mixed-state
rounds, not a global all-six latch. Each round authenticates the current exact
partition of already-fenced nodes and still-live targets, freshly samples and
cross-proves only those live targets, and binds fresh capture-bound status for
every already-fenced node inside the same bracket. Its authorization expires
exactly 300 seconds after the public sample completes. Immediately before each
target's nft apply, the helper proves that round hash and deadline and writes a
create-only node-applied receipt. A crash may leave any positive subset fenced;
the immutable result records that subset and a later round freshly authorizes
only the remainder. Zero-progress attempts stay outside the ledger and may be
resampled; positive rounds are never rewritten, and no node may cross twice.
The completed generation ledger has at most six rounds, covers all six nodes,
and derives the legacy cutoff as the maximum public height across every round.
This permits honest mixed-state resume without pretending that one local
commit proves one remote mutation.

Before a live target may cross its first restart-effective systemd dependency
or nft boundary, the round captures an exact source pair with immutable role
`preauthorization-boundary`. Production data directories and their siblings
are allowed to contain no snapshot at all; that is the expected no-existing-
snapshot case, not a reason to defer capture until after stop. The helper first
proves that the loopback `/sync/snapshot` listener socket belongs to the sealed
writer PID, boot ID, start ticks, executable, argv, and cgroup. Each request is
recorded in a create-only attempt directory. The pinned inspector then copies
and hashes exactly the WAL prefix selected by that snapshot, permits only an
append-only source suffix during the copy, and selects the pair only after
strict offline replay reproduces the head and every authenticated/public
ancestry bound. A failed request, a snapshot with no exact complete WAL
boundary, a changed prefix, or a non-append-only WAL causes a retained failed
attempt and a new request. A crash after the complete attempt receipt is
fsynced but before `selected.json` reuses that exact receipt without another
snapshot request; a complete selector is also revalidated byte-for-byte.
Authorization heights may never exceed the selected capture head.

For a node that remains active behind the full-host quarantine, two stable
post-quarantine samples precede a second exact capture with immutable role
`post-quarantine-final-export`. Its listener/writer identity must be unchanged,
its head must equal the stable quarantined tuple and cover every authenticated
bound, and public plus inter-validator ingress must still be denied. The final
capture receipt hash, role, and head are bound into the stop intent, stop
receipt, persisted-head evidence, and export selection; normal active export
never selects the earlier pair. If a target becomes persistently stopped
before any complete final receipt exists, only its exact tagged stopped
transition may select the `preauthorization-boundary` pair. The archive still
preserves the complete final WAL and labels every later complete suffix
`archived_noncanonical_post_capture_suffix`; it does not silently advance the
selected snapshot boundary or call that suffix uncommitted.

The first restart-effective write is always the selected live supervisor's
dependency drop-in, followed by the remaining reviewed activation sources,
dispatcher/unit, daemon reload, enablement, sync, and the persistent barrier
receipt. Before that first dependency, natural writer absence remains eligible
for a fresh sample or ordinary supervisor restart. After it, a same-boot
persistently-stopped terminal is accepted only after authorization expiry, two
stable samples proving the writer, selected supervisor, alternatives, and
pending jobs absent, and exact fail-closed dependency effectiveness. It records
cause `unknown` with no signal; it is not rewritten as a reboot. Fleet evidence
is a tagged union: active nodes retain their full fenced status and final
source role, while stopped nodes bind their exact transition, fresh current
status, and persisted-head roots. An all-stopped generation has an explicit
empty active stability set, never a fabricated empty-input stability claim.

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

After all six writers are proven PID-free, each offline capture binds the
original legacy data directory's path, device, inode, complete regular-file
index, complete final state/DAG WAL bytes, the already sealed selected live
snapshot/fixed-prefix pair, and persistent fence evidence. The exact source
remains in place and is repeatedly re-hashed; it is content-sealed, not
OS-read-only, and no second full local data tree is created. No snapshot is
requested after stop. The earlier live `/sync/snapshot` response was never
trusted by itself: only the writer-owned, fixed-prefix, strictly replayed pair
is eligible. The helper never uses SIGKILL and rejects changed, missing,
unexpected, cross-device, symlink, or special-file content.

Pin the exact Ubuntu curl and CA bytes used for the builder's anonymous GitHub
proof, then acquire the reviewed stock Caddy binary. Keep the download
directory until final archive verification completes.

```bash
system_curl=/usr/bin/curl
system_ca_bundle=/etc/ssl/certs/ca-certificates.crt
system_curl_sha256=74b4ce8f74b377f18ef1b3df7279c26cb3cd14c49e39ab1498575b209dc3f70f
system_ca_bundle_sha256=ecd9dc38bc3efb7dbd6431f57e29d2f8d6a0f0d211e1464b3fef2cbfe266fcd2
printf '%s  %s\n' "$system_curl_sha256" "$system_curl" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$system_ca_bundle_sha256" "$system_ca_bundle" \
  | /usr/bin/sha256sum --check --strict

caddy_download_root="$(mktemp -d /secure/operator/caddy-v2.11.4.XXXXXXXX)"
caddy_archive="$caddy_download_root/caddy_2.11.4_linux_amd64.tar.gz"
caddy_binary=/secure/operator/caddy-2.11.4-linux-amd64
test ! -e "$caddy_binary"
/usr/bin/env -i HOME="$caddy_download_root" PATH=/usr/bin:/bin LANG=C LC_ALL=C \
  "$system_curl" -q --silent --show-error --fail --location --max-redirs 3 \
  --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --cacert "$system_ca_bundle" --config /dev/null --proxy '' --noproxy '*' \
  --connect-timeout 10 --max-time 180 --max-filesize 17238873 \
  --output "$caddy_archive" \
  'https://github.com/caddyserver/caddy/releases/download/v2.11.4/caddy_2.11.4_linux_amd64.tar.gz'
printf '%s  %s\n' \
  527fbf917c39189a1e3b31d34fa955601680b2d5c8055d2a87b8b9588dec7bb9 \
  "$caddy_archive" | /usr/bin/sha256sum --check --strict
test "$(/usr/bin/stat --format='%s' "$caddy_archive")" = 17238873
test "$(/usr/bin/tar -tzf "$caddy_archive")" = "$(printf 'LICENSE\nREADME.md\ncaddy')"
/usr/bin/tar --extract --gzip --file "$caddy_archive" \
  --directory "$caddy_download_root" --no-same-owner --no-same-permissions -- caddy
test -f "$caddy_download_root/caddy" && test ! -L "$caddy_download_root/caddy"
chmod 0500 "$caddy_download_root/caddy"
/bin/ln "$caddy_download_root/caddy" "$caddy_binary"
printf '%s  %s\n' \
  b7105518e3ed1c0761f232e44fc09345535533c9cb0abf0e12809416c7ac64d9 \
  "$caddy_binary" | /usr/bin/sha256sum --check --strict
test "$("$caddy_binary" version | /usr/bin/awk '{print $1}')" = v2.11.4
```


The sealed prearchive production manifest carries the independently preserved
block-height-137145 source snapshot and its paired reference WAL as
SHA-256-bound artifacts. Source consensus round `9774808` is distinct recovery
checkpoint metadata; it is not the block height. Before export, prove the exact
root-owned pair, its two metadata records, and the complete four-row
`SHA256SUMS` mapping. Then build the unsigned candidate with the exact recovery
exporter; successful export decodes the snapshot, recomputes its
account/storage/code root, and requires it to equal the complete WAL
block/checkpoint boundary:

```bash
reference_pair=/secure/operator/reference-pair
reference_source_consensus_round=9774808
reference_block_height=137145
"$ARC_RECOVERY_PYTHON_PATH" -I - \
  "$reference_pair" "$reference_block_height" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import stat
import sys

root = pathlib.Path(sys.argv[1])
expected_height = int(sys.argv[2])
expected_files = {
    "state.snapshot.lz4": (
        1_160_246,
        "ecb4e39d45e6711cffcd78183851587e4deb37ad63163f541ef6c1f821a4ce47",
    ),
    "state.wal": (
        83_385_625,
        "3820e112af1684567f0336abe73ae9aafc4228d0e02a5fccb1ff32f64dfed44c",
    ),
    "latest.json": (
        687,
        "0c9bcafd99375de7e3167c271350279c4d267dd9cf91de37aa830a2b817f80af",
    ),
    "snapshot-info.json": (
        138,
        "98f327fb9c4405cd0f6e7c31052d571a024738df5bf6987ad78d9b1ba5856b49",
    ),
}

def reject_duplicates(pairs):
    value = {}
    for key, child in pairs:
        if key in value:
            raise SystemExit(f"reference metadata duplicates key {key!r}")
        value[key] = child
    return value

def identity(value):
    return (
        value.st_dev,
        value.st_ino,
        value.st_mode,
        value.st_uid,
        value.st_gid,
        value.st_nlink,
        value.st_size,
        value.st_mtime_ns,
        value.st_ctime_ns,
    )

def read_locked(root_fd, name, size, digest=None):
    descriptor = os.open(
        name,
        os.O_RDONLY | os.O_NOFOLLOW | getattr(os, "O_CLOEXEC", 0),
        dir_fd=root_fd,
    )
    try:
        before = os.fstat(descriptor)
        if (
            not stat.S_ISREG(before.st_mode)
            or (before.st_uid, before.st_gid, stat.S_IMODE(before.st_mode))
            != (0, 0, 0o400)
            or before.st_nlink != 1
            or before.st_size != size
        ):
            raise SystemExit(f"reference-pair identity differs for {name}")
        payload = bytearray()
        while len(payload) <= size:
            chunk = os.read(descriptor, min(1024 * 1024, size + 1 - len(payload)))
            if not chunk:
                break
            payload.extend(chunk)
        if len(payload) != size or identity(before) != identity(os.fstat(descriptor)):
            raise SystemExit(f"reference-pair file changed while read: {name}")
        if digest is not None and hashlib.sha256(payload).hexdigest() != digest:
            raise SystemExit(f"reference-pair SHA-256 differs for {name}")
        return bytes(payload)
    finally:
        os.close(descriptor)

root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
try:
    root_metadata = os.fstat(root_fd)
    if (
        not stat.S_ISDIR(root_metadata.st_mode)
        or root_metadata.st_uid != 0
        or root_metadata.st_gid != 0
        or stat.S_IMODE(root_metadata.st_mode) & 0o022
    ):
        raise SystemExit("reference-pair directory is not protected root storage")
    payloads = {
        name: read_locked(root_fd, name, size, digest)
        for name, (size, digest) in expected_files.items()
    }
    sums = read_locked(root_fd, "SHA256SUMS", 324)
finally:
    os.close(root_fd)

if not sums.endswith(b"\n") or b"\r" in sums:
    raise SystemExit("reference-pair SHA256SUMS is not canonical LF text")
rows = {}
for line in sums.decode("ascii").splitlines():
    match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._-]+)", line)
    if match is None or match.group(2) in rows:
        raise SystemExit("reference-pair SHA256SUMS has malformed or duplicate rows")
    rows[match.group(2)] = match.group(1)
if rows != {name: digest for name, (_size, digest) in expected_files.items()}:
    raise SystemExit("reference-pair SHA256SUMS differs from the reviewed four-file map")

latest = json.loads(payloads["latest.json"], object_pairs_hook=reject_duplicates)
snapshot = json.loads(
    payloads["snapshot-info.json"], object_pairs_hook=reject_duplicates
)
header = latest.get("header") if isinstance(latest, dict) else None
expected_state_root = "d300a2bb8dbe7f6da9596b550f31efd36eb842a1861e294c25740a19c8e3bc6d"
if (
    set(latest) != {"header", "tx_hashes", "hash"}
    or not isinstance(header, dict)
    or header.get("height") != expected_height
    or header.get("state_root") != expected_state_root
    or latest.get("hash")
    != "8fac459a8de0164b28e30d3f67adf6aefe01054912a3d1ae5c53765e59935a90"
):
    raise SystemExit("reference latest metadata differs at the recovery boundary")
if snapshot != {
    "account_count": 78_025,
    "available": True,
    "height": expected_height,
    "state_root": "0x" + expected_state_root,
}:
    raise SystemExit("reference snapshot metadata differs at the recovery boundary")
PY

candidate_checkpoint=/secure/operator/candidate.arcchkpt
candidate_attempt_root="$(
  /usr/bin/mktemp -d /secure/operator/candidate-export.XXXXXXXX
)"
candidate_attempt="$candidate_attempt_root/candidate.arcchkpt"
test ! -e "$candidate_attempt"
"$arc_node_linux" recovery export \
  --data-dir "$reference_pair" \
  --snapshot "$reference_pair/state.snapshot.lz4" \
  --genesis "$operator_genesis" \
  --validator-public-keys "$validator_public_keys" \
  --legacy-validator-set "$legacy_validator_set" \
  --output "$candidate_attempt" \
  --source-consensus-round "$reference_source_consensus_round" \
  --created-at-unix-ms 1787857623000 \
  --recovery-epoch 1 \
  --validator-set-id 1 \
  --allow-unbound-legacy-wal
chmod 0400 "$candidate_attempt"
arc_install_or_reuse_exact "$candidate_attempt" "$candidate_checkpoint"
test -f "$candidate_checkpoint" && test ! -L "$candidate_checkpoint"
test "$(/usr/bin/stat --format='%a' "$candidate_checkpoint")" = 400
candidate_checkpoint_sha256="$(arc_sha256 "$candidate_checkpoint")"
[[ "$candidate_checkpoint_sha256" =~ ^[0-9a-f]{64}$ ]]
```

The last flag is necessary for the audited legacy WAL, which predates the
authenticated genesis network hash. It is never implicit: both checkpoint
creation and final archive sealing require the operator to state it, and the
binding evidence records that exception.

Export always targets a fresh attempt directory first. The canonical
`candidate.arcchkpt` is installed with a create-new hard link only after the
export is complete and durable. A retry re-exports from the pinned pair and may
reuse the canonical path only when a no-follow, stable hash proves byte-for-byte
equality; a different or malformed existing file stops without replacement.
Keep the attempt directory as the resume/audit copy.

`recovery sign` is the supported 5-of-6 collection primitive. The feasible
signing topology is the one reviewed, root-only operator enclave that already
contains all six restored vault keyfiles. After its required artifact proofs,
capture, and remote key installation are complete, isolate that enclave from
the network for the signing window. The wrapper below verifies the reviewed
Ubuntu `unshare` bytes, creates a new network namespace for every ARC signer
subprocess, proves `/proc/net/dev` exposes only loopback, proves there is no
IPv4 route or non-loopback IPv6 route, closes every inherited descriptor above
standard input/output/error, and only then `execve`s the hash-pinned ARC binary
with a four-variable clean environment. `GH_TOKEN` is removed before
any checkpoint inspection or signature. Five distinct reviewed keyfiles sign
in sequence; the sixth remains an unused recovery member. Every invocation
rechecks the exact Linux binary against the protected build-metadata hash,
retains earlier records, adds only that key's signature, and creates a new
checkpoint path. The final `recovery verify` authenticates both the five
identities and strict-stake supermajority.

```bash
set -Eeuo pipefail
umask 077
unset GH_TOKEN
signing_unshare=/usr/bin/unshare
signing_unshare_sha256=a23c8863860669003dc4660039fe642f5795c8c2195898ebc5d01afa1ac3d11c
test -f "$signing_unshare" && test ! -L "$signing_unshare"
printf '%s  %s\n' "$signing_unshare_sha256" "$signing_unshare" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_PYTHON_SHA256" "$ARC_RECOVERY_PYTHON_PATH" \
  | /usr/bin/sha256sum --check --strict
signing_home="$(/usr/bin/mktemp -d /secure/operator/offline-signing-home.XXXXXXXX)"
signing_attempt_root="$(
  /usr/bin/mktemp -d /secure/operator/checkpoint-signing.XXXXXXXX
)"
offline_signer_python='import errno, os, pathlib, sys
interfaces = {
    line.split(":", 1)[0].strip()
    for line in pathlib.Path("/proc/net/dev").read_text(encoding="ascii").splitlines()[2:]
    if line.strip()
}
ipv4_routes = pathlib.Path("/proc/net/route").read_text(encoding="ascii").splitlines()[1:]
ipv6_routes = pathlib.Path("/proc/net/ipv6_route").read_text(encoding="ascii").splitlines()
if interfaces != {"lo"} or any(line.strip() for line in ipv4_routes):
    raise SystemExit("offline signer has a non-loopback interface or IPv4 route")
if any(line.split()[-1] != "lo" for line in ipv6_routes if line.split()):
    raise SystemExit("offline signer has a non-loopback IPv6 route")
argv = sys.argv[1:]
if not argv or not os.path.isabs(argv[0]):
    raise SystemExit("offline signer executable is not absolute")
with os.scandir("/proc/self/fd") as entries:
    inherited = [int(entry.name) for entry in entries if int(entry.name) > 2]
for descriptor in inherited:
    try:
        os.close(descriptor)
    except OSError as error:
        if error.errno != errno.EBADF:
            raise
os.execve(argv[0], argv, {"HOME": os.environ["HOME"], "PATH": "/usr/bin:/bin", "LANG": "C", "LC_ALL": "C"})'
offline_signer() {
  printf '%s  %s\n' "$signing_unshare_sha256" "$signing_unshare" \
    | /usr/bin/sha256sum --check --strict
  "$signing_unshare" --net -- /usr/bin/env -i \
    HOME="$signing_home" PATH=/usr/bin:/bin LANG=C LC_ALL=C \
    "$ARC_RECOVERY_PYTHON_PATH" -I -c "$offline_signer_python" "$@" </dev/null
}
signing_binary="$arc_node_linux"
printf '%s  %s\n' "$arc_node_linux_sha256" "$signing_binary" \
  | /usr/bin/sha256sum --check --strict
signing_keys=(
  /secure/operator/arc-v0.8-validator-restore/keys/NYC.validator-key.json
  /secure/operator/arc-v0.8-validator-restore/keys/LAX.validator-key.json
  /secure/operator/arc-v0.8-validator-restore/keys/AMS.validator-key.json
  /secure/operator/arc-v0.8-validator-restore/keys/LHR.validator-key.json
  /secure/operator/arc-v0.8-validator-restore/keys/NRT.validator-key.json
)
test "${#signing_keys[@]}" = 5
for signing_key in "${signing_keys[@]}"; do
  test -f "$signing_key" && test ! -L "$signing_key"
  test "$(/usr/bin/stat --format='%a' "$signing_key")" = 600
done
incoming_checkpoint="$candidate_checkpoint"
checkpoint_manifest_hash="$(
  offline_signer "$signing_binary" recovery inspect \
    --checkpoint "$incoming_checkpoint" \
    | "$ARC_RECOVERY_PYTHON_PATH" -I -c 'import json,sys; print(json.load(sys.stdin)["manifest_hash"])'
)"
[[ "$checkpoint_manifest_hash" =~ ^0x[0-9a-f]{64}$ ]]
checkpoint_approval_phrase="APPROVE CHECKPOINT $checkpoint_manifest_hash"
printf 'After all six operators compare it out of band, type exactly: %s\n> ' \
  "$checkpoint_approval_phrase" >/dev/tty
IFS= read -r checkpoint_approval </dev/tty
test "$checkpoint_approval" = "$checkpoint_approval_phrase"
unset checkpoint_approval
for index in "${!signing_keys[@]}"; do
  signing_key="${signing_keys[$index]}"
  outgoing_checkpoint="$signing_attempt_root/candidate.signed-$((index + 1)).arcchkpt"
  test ! -e "$outgoing_checkpoint"
  printf '%s  %s\n' "$arc_node_linux_sha256" "$signing_binary" \
    | /usr/bin/sha256sum --check --strict
  offline_signer "$signing_binary" recovery sign \
    --checkpoint "$incoming_checkpoint" \
    --genesis "$operator_genesis" \
    --approved-manifest-hash "$checkpoint_manifest_hash" \
    --validator-key-file "$signing_key" \
    --output "$outgoing_checkpoint" \
    --recovery-epoch 1 \
    --validator-set-id 1
  chmod 0400 "$outgoing_checkpoint"
  incoming_checkpoint="$outgoing_checkpoint"
done
recovery_checkpoint=/secure/operator/recovery.arcchkpt
arc_install_or_reuse_exact "$incoming_checkpoint" "$recovery_checkpoint"
test -f "$recovery_checkpoint" && test ! -L "$recovery_checkpoint"
test "$(/usr/bin/stat --format='%a' "$recovery_checkpoint")" = 400
```

After the fifth distinct signer, the completed signing attempt is installed at
the create-only `/secure/operator/recovery.arcchkpt` path. A retry may reuse
that path only if a fresh deterministic five-key signing pass is byte-identical;
otherwise both copies are preserved and the procedure stops. Verify the
canonical file against both the five-identity and strict-stake supermajorities
with the same protected-main binary. This final verification, not a filename or
signature count copied from a signer, is the acceptance boundary:

```bash
[[ "$checkpoint_manifest_hash" =~ ^0x[0-9a-f]{64}$ ]]
offline_signer "$arc_node_linux" recovery verify \
  --checkpoint "$recovery_checkpoint" \
  --genesis "$operator_genesis" \
  --approved-manifest-hash "$checkpoint_manifest_hash" \
  --recovery-epoch 1 \
  --validator-set-id 1
test "$(arc_sha256 "$recovery_checkpoint")" = \
  "$(arc_sha256 "$incoming_checkpoint")"
```

Build the prearchive production manifest only from protected-main and sealed
evidence inputs. The builder derives every chain, topology, gateway, artifact,
check, destination, and zero archive-root field:

```bash
prearchive_output=/secure/operator/arc-recovery.prearchive.json
prearchive_sidecar="$prearchive_output.sha256"
production_stage_root=/secure/operator/production-input-stage-v0.8.0
prearchive_existing=0
for prearchive_path in \
  "$prearchive_output" "$prearchive_sidecar" "$production_stage_root"
do
  if [ -e "$prearchive_path" ] || [ -L "$prearchive_path" ]; then
    prearchive_existing=$((prearchive_existing + 1))
  fi
done
if [ "$prearchive_existing" = 0 ]; then
  prearchive_result="$(
    "$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/build-production-manifest.py prearchive \
      --source-main-sha "$protected_main_sha" \
      --pretag-run-id "$pretag_run_id" \
      --pretag-run-attempt "$pretag_run_attempt" \
      --pretag-artifact-input-set "$pretag_input_set" \
      --curl "$system_curl" \
      --curl-sha256 "$system_curl_sha256" \
      --ca-bundle "$system_ca_bundle" \
      --ca-bundle-sha256 "$system_ca_bundle_sha256" \
      --freeze-plan /secure/operator/arc-freeze.lock.json \
      --freeze-plan-sha256 "$freeze_sha256" \
      --legacy-public-height-receipt "$legacy_public_height_receipt" \
      --legacy-maintenance-evidence-bundle "$legacy_maintenance_evidence_bundle" \
      --legacy-maintenance-boundary "$legacy_maintenance_boundary" \
      --legacy-late-fork-source-set "$offline_stop_output.legacy-late-fork-source-set.json" \
      --offline-stop-evidence "$offline_stop_evidence" \
      --ssh-known-hosts "$known_hosts" \
      --ssh-identity "$ssh_identity" \
      --validator-vault-restore-receipt /secure/operator/arc-v0.8-validator-restore/RESTORE-RECEIPT.json \
      --validator-key-install-receipt /secure/operator/VALIDATOR-KEY-INSTALL-RECEIPT.json \
      --validator-public-keys "$validator_public_keys" \
      --legacy-validator-set "$legacy_validator_set" \
      --checkpoint "$recovery_checkpoint" \
      --source-snapshot "$reference_pair/state.snapshot.lz4" \
      --source-wal "$reference_pair/state.wal" \
      --caddy "$caddy_binary" \
      --reward-probe "$PWD/scripts/recovery/community-reward-probe.py" \
      --stage-root "$production_stage_root" \
      --acme-email tj@arc.ai \
      --output "$prearchive_output"
  )"
  locked_sha256="$(
    printf '%s' "$prearchive_result" | /usr/bin/python3.12 -I -c '
import json, sys
value = json.load(sys.stdin)
if set(value) != {"schema", "phase", "rollout_sha256", "output"}:
    raise SystemExit("prearchive builder output fields differ")
if value["schema"] != "arc.recovery.production-manifest-build.v1" or value["phase"] != "prearchive":
    raise SystemExit("prearchive builder output identity differs")
if value["output"] != "/secure/operator/arc-recovery.prearchive.json":
    raise SystemExit("prearchive builder output path differs")
print(value["rollout_sha256"])
'
  )"
elif [ "$prearchive_existing" = 3 ]; then
  test -f "$prearchive_output" && test ! -L "$prearchive_output"
  test -f "$prearchive_sidecar" && test ! -L "$prearchive_sidecar"
  test -d "$production_stage_root" && test ! -L "$production_stage_root"
  locked_sha256="$(
    "$ARC_RECOVERY_PYTHON_PATH" -I - \
      "$prearchive_output" "$PWD/scripts/recovery" "$protected_main_sha" <<'PY'
import importlib.util
import pathlib
import sys

manifest_path = pathlib.Path(sys.argv[1])
script_dir = pathlib.Path(sys.argv[2])
expected_commit = sys.argv[3]
sys.path.insert(0, str(script_dir))
spec = importlib.util.spec_from_file_location(
    "arc_production_manifest_builder",
    script_dir / "build-production-manifest.py",
)
if spec is None or spec.loader is None:
    raise SystemExit("cannot load the protected production-manifest validator")
builder = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = builder
spec.loader.exec_module(builder)
value, _payload, digest = builder.load_private_rollout(manifest_path)
provenance = value.get("provenance")
if not isinstance(provenance, dict) or provenance.get("source_main_commit") != expected_commit:
    raise SystemExit("existing prearchive belongs to another protected-main commit")
with builder.stable_artifact_identity_window(value):
    pass
print(digest)
PY
  )"
else
  printf '%s\n' \
    'partial prearchive output/stage set exists; preserve it under a unique forensic path and stop' >&2
  exit 1
fi
[[ "$locked_sha256" =~ ^[0-9a-f]{64}$ ]]
printf '%s  %s\n' "$locked_sha256" "${prearchive_output##*/}" \
  | /usr/bin/cmp --silent - "$prearchive_sidecar"
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

On the first attempt, none of the prearchive manifest, checksum sidecar, or
stage root may exist. The builder creates the stage once at mode 0700, copies
every semantic input through stable no-follow file descriptors, fsyncs each
copy and a canonical stage manifest, then removes all directory write bits. A
complete retry tuple is reusable only after the current protected-main builder
validates the canonical manifest/sidecar and reopens, hashes, and identity-checks
the full read-only stage inventory. A one- or two-member partial tuple stops.
Preserve every partial member by moving it with `mv --no-clobber` into a fresh
root-owned `/secure/operator/incomplete-prearchive.XXXXXXXX` forensic directory,
record its hashes, and then rerun all preceding read-only verification before a
new attempt; never delete or overwrite it.
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
# The builder has now sealed copies of the two private SSH inputs. Switch only
# those paths to the stage; every executable and its reviewed hash stays fixed.
export ARC_RECOVERY_SSH_KNOWN_HOSTS=/secure/operator/production-input-stage-v0.8.0/private/known_hosts
export ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256=97c826f7e1a3940f6d18095ccdb0eaeebb5d66ec16fe60b9c5c47690e707485d
export ARC_RECOVERY_SSH_IDENTITY=/secure/operator/production-input-stage-v0.8.0/private/id_ed25519
export ARC_RECOVERY_SSH_IDENTITY_SHA256=9a7b57700dc7acf0faeca152fc341f237704e81965b5a9656fe8ccee4931444a
test "$(/usr/bin/stat --format='%a:%h' "$ARC_RECOVERY_SSH_KNOWN_HOSTS")" = 400:1
test "$(/usr/bin/stat --format='%a:%h' "$ARC_RECOVERY_SSH_IDENTITY")" = 400:1
printf '%s  %s\n' "$ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256" \
  "$ARC_RECOVERY_SSH_KNOWN_HOSTS" | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SSH_IDENTITY_SHA256" \
  "$ARC_RECOVERY_SSH_IDENTITY" | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_PYTHON_SHA256" "$ARC_RECOVERY_PYTHON_PATH" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SSH_SHA256" /usr/bin/ssh \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_SCP_SHA256" /usr/bin/scp \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_RECOVERY_RCLONE_SHA256" "$ARC_RECOVERY_RCLONE_PATH" \
  | /usr/bin/sha256sum --check --strict
test "$(arc_sha256 "$ARC_RECOVERY_RCLONE_CONFIG")" = "$rclone_config_sha256"
printf '%s  %s\n' "$ARC_RECOVERY_GH_SHA256" "$ARC_RECOVERY_GH_PATH" \
  | /usr/bin/sha256sum --check --strict
archive_work_root="$(
  /usr/bin/mktemp -d "/secure/operator/arc-archive-work.$capture_id.XXXXXXXX"
)"

scripts/recovery/archive-fleet-to-drive.sh seal \
  --freeze-plan /secure/operator/arc-freeze.lock.json \
  --manifest /secure/operator/arc-recovery.prearchive.json \
  --validator-public-keys /secure/operator/production-input-stage-v0.8.0/validator-public-keys.json \
  --validator-install-receipt /secure/operator/production-input-stage-v0.8.0/private/VALIDATOR-KEY-INSTALL-RECEIPT.json \
  --vault-restore-receipt /secure/operator/production-input-stage-v0.8.0/private/VALIDATOR-VAULT-RESTORE-RECEIPT.json \
  --finalization-intent /secure/operator/archive-finalization-intent.json \
  --work-root "$archive_work_root" \
  --allow-unbound-legacy-wal

destination='arc-drive-arc:ARC Chain Recovery v0.8/captures/'"$capture_id"
destination_sha256="$(printf %s "$destination" | /usr/bin/env -i HOME=/var/empty PATH=/usr/bin:/bin LANG=C LC_ALL=C "$ARC_RECOVERY_PYTHON_PATH" -I -c 'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')"
ARC_RECOVERY_GO="GO $locked_sha256 FREEZE $freeze_sha256 CAPTURE $capture_id DEST $destination_sha256 LEGACY_WAL UNBOUND" \
  scripts/recovery/archive-fleet-to-drive.sh seal \
    --freeze-plan /secure/operator/arc-freeze.lock.json \
    --manifest /secure/operator/arc-recovery.prearchive.json \
    --validator-public-keys /secure/operator/production-input-stage-v0.8.0/validator-public-keys.json \
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
destination="arc-drive-arc:ARC Chain Recovery v0.8/captures/$capture_id"
scripts/recovery/archive-fleet-to-drive.sh verify-complete \
  --destination "$destination"
```

An absent, non-canonical, mismatched, or tampered `COMPLETE.json`, manifest, or
sidecar fails closed. `verify-complete` deliberately destroys its temporary
downloads when it exits, so copy the four values from its single
`FINAL-ROLLOUT-ROOTS` line and then materialize the finalizer inputs through the
same hash-pinned rclone/config identities. Opening both on file descriptors
keeps the exact reviewed inodes pinned across all seven downloads; the fresh
mode-0700 directory has a capture-scoped `mktemp` suffix, so every retry gets a
new path, and shell `noclobber` makes every local evidence file create-only.
Each read is capped at one byte beyond the finalizer's 16 MiB JSON
limit, so an oversized mutable remote object fails instead of consuming the
operator volume without bound.

```bash
set -Eeuo pipefail
complete_sha256='<FINAL-ROLLOUT-ROOTS complete_sha256>'
archive_manifest_sha256='<FINAL-ROLLOUT-ROOTS archive_manifest_sha256>'
sha256sums_sha256='<FINAL-ROLLOUT-ROOTS sha256sums_sha256>'
prearchive_rollout_sha256='<FINAL-ROLLOUT-ROOTS prearchive_rollout_sha256>'
for root in "$complete_sha256" "$archive_manifest_sha256" \
  "$sha256sums_sha256" "$prearchive_rollout_sha256"; do
  printf '%s\n' "$root" | /usr/bin/grep -Eq '^[0-9a-f]{64}$'
done
test "$prearchive_rollout_sha256" = "$locked_sha256"
test -f "$ARC_RECOVERY_RCLONE_PATH" && test ! -L "$ARC_RECOVERY_RCLONE_PATH"
test -f "$ARC_RECOVERY_RCLONE_CONFIG" && test ! -L "$ARC_RECOVERY_RCLONE_CONFIG"
rclone_config_sha256="$(arc_sha256 "$ARC_RECOVERY_RCLONE_CONFIG")"
exec 8<"$ARC_RECOVERY_RCLONE_CONFIG"
exec 9<"$ARC_RECOVERY_RCLONE_PATH"
test "$(arc_sha256 /proc/self/fd/8)" = "$rclone_config_sha256"
test "$(arc_sha256 /proc/self/fd/9)" = "$ARC_RECOVERY_RCLONE_SHA256"

download_root="$(
  /usr/bin/mktemp -d "/secure/operator/downloaded.$capture_id.XXXXXXXX"
)"
test -d "$download_root" && test ! -L "$download_root"
test "$(/usr/bin/stat --format='%a:%h' "$download_root")" = 700:1
install -d -m 0700 "$download_root/home"
arc_pinned_rclone() {
  /usr/bin/env -i \
    HOME="$download_root/home" PATH=/usr/bin:/bin LANG=C LC_ALL=C \
    /proc/self/fd/9 --config /proc/self/fd/8 "$@"
}
for name in \
  COMPLETE.json \
  ARCHIVE-MANIFEST.json \
  ARCHIVE-MANIFEST.json.sha256 \
  SHA256SUMS \
  drive-archive-seal-prefreeze.json \
  drive-archive-seal-attempt.json \
  github-gist-write-canary.json
do
  (
    set -o noclobber
    arc_pinned_rclone cat "$destination/$name" --count=16777217 \
      > "$download_root/$name"
  )
  chmod 0400 "$download_root/$name"
done
test "$(arc_sha256 "$download_root/COMPLETE.json")" = "$complete_sha256"
test "$(arc_sha256 "$download_root/ARCHIVE-MANIFEST.json")" = \
  "$archive_manifest_sha256"
test "$(arc_sha256 "$download_root/SHA256SUMS")" = "$sha256sums_sha256"
printf '%s  %s\n' "$archive_manifest_sha256" ARCHIVE-MANIFEST.json \
  | /usr/bin/cmp --silent - "$download_root/ARCHIVE-MANIFEST.json.sha256"

# Recheck the complete remote object set against the same four independent
# roots after the separate downloads. The finalizer below then verifies that
# the three archived boundary receipts match ARCHIVE-MANIFEST/SHA256SUMS.
scripts/recovery/archive-fleet-to-drive.sh verify-complete \
  --destination "$destination" \
  --expected-complete-sha256 "$complete_sha256" \
  --expected-archive-manifest-sha256 "$archive_manifest_sha256" \
  --expected-sha256sums-sha256 "$sha256sums_sha256" \
  --expected-prearchive-rollout-sha256 "$prearchive_rollout_sha256"
exec 8<&-
exec 9<&-
```

Use the emitted `FINAL-ROLLOUT-ROOTS` values only as independently verified
trust roots for the separately downloaded archive evidence in that exact
`$download_root`, then finalize, derive the final hash/policy from authenticated
output, and perform the production plan and execute:

```bash
final_manifest=/secure/operator/arc-recovery-final.lock.json
final_manifest_sidecar="$final_manifest.sha256"
finalizer_attempt_root="$(
  /usr/bin/mktemp -d /secure/operator/finalizer.XXXXXXXX
)"
finalizer_attempt="$finalizer_attempt_root/${final_manifest##*/}"
finalizer_attempt_sidecar="$finalizer_attempt.sha256"
test ! -e "$finalizer_attempt"
test ! -e "$finalizer_attempt_sidecar"
finalizer_result="$(
  "$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/build-production-manifest.py finalize \
    --prearchive "$prearchive_output" \
    --complete "$download_root/COMPLETE.json" \
    --complete-sha256 "$complete_sha256" \
    --archive-manifest "$download_root/ARCHIVE-MANIFEST.json" \
    --archive-manifest-sidecar "$download_root/ARCHIVE-MANIFEST.json.sha256" \
    --archive-manifest-sha256 "$archive_manifest_sha256" \
    --sha256sums "$download_root/SHA256SUMS" \
    --sha256sums-sha256 "$sha256sums_sha256" \
    --drive-archive-seal-prefreeze "$download_root/drive-archive-seal-prefreeze.json" \
    --drive-archive-seal-attempt "$download_root/drive-archive-seal-attempt.json" \
    --github-gist-write-canary "$download_root/github-gist-write-canary.json" \
    --output "$finalizer_attempt"
)"
final_rollout_sha256="$(
  printf '%s' "$finalizer_result" | /usr/bin/python3.12 -I -c '
import json, sys
value = json.load(sys.stdin)
if set(value) != {"schema", "phase", "rollout_sha256", "output"}:
    raise SystemExit("finalizer output fields differ")
if value["schema"] != "arc.recovery.production-manifest-build.v1" or value["phase"] != "final":
    raise SystemExit("finalizer output identity differs")
if value["output"] != sys.argv[1]:
    raise SystemExit("finalizer output path differs")
print(value["rollout_sha256"])
' "$finalizer_attempt"
)"
[[ "$final_rollout_sha256" =~ ^[0-9a-f]{64}$ ]]
printf '%s  %s\n' "$final_rollout_sha256" "${finalizer_attempt##*/}" \
  | /usr/bin/cmp --silent - "$finalizer_attempt_sidecar"
# Install the checksum first and the manifest last. A killed attempt can leave
# only a harmless sidecar; every retry recreates and compares both exact files.
arc_install_or_reuse_exact "$finalizer_attempt_sidecar" "$final_manifest_sidecar"
arc_install_or_reuse_exact "$finalizer_attempt" "$final_manifest"
printf '%s  %s\n' "$final_rollout_sha256" "${final_manifest##*/}" \
  | /usr/bin/cmp --silent - "$final_manifest_sidecar"
test "$(arc_sha256 "$final_manifest")" = "$final_rollout_sha256"
legacy_wal_policy="$(
  /usr/bin/python3.12 -I - "$final_manifest" <<'PY'
import json, pathlib, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_bytes())
archive = value.get("archive")
if not isinstance(archive, dict) or not isinstance(archive.get("allow_unbound_legacy_wal"), bool):
    raise SystemExit("sealed final manifest lacks a boolean legacy-WAL policy")
print("UNBOUND" if archive["allow_unbound_legacy_wal"] else "BOUND")
PY
)"
case "$legacy_wal_policy" in BOUND|UNBOUND) ;; *) exit 1 ;; esac

reward_evidence=/secure/operator/recovery-v3.reward-evidence.json
rollback_journal=/secure/operator/rollback-final-rollout
test ! -e "$reward_evidence"
test ! -e "$reward_evidence.sha256"
test ! -e "$rollback_journal"
"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/recovery_rollout.py run \
  --manifest "$final_manifest" \
  --reward-evidence-output "$reward_evidence" \
  --rollback-journal "$rollback_journal"

ARC_RECOVERY_GO="GO $final_rollout_sha256 FREEZE $freeze_sha256 CAPTURE $capture_id ARCHIVE $archive_manifest_sha256 DEST $destination_sha256 LEGACY_WAL $legacy_wal_policy" \
  "$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/recovery_rollout.py run \
    --manifest "$final_manifest" \
    --execute \
    --go-hash "$final_rollout_sha256" \
    --archive-manifest-sha256 "$archive_manifest_sha256" \
    --reward-evidence-output "$reward_evidence" \
    --rollback-journal "$rollback_journal"
test -f "$reward_evidence" && test ! -L "$reward_evidence"
test -f "$reward_evidence.sha256" && test ! -L "$reward_evidence.sha256"
(cd /secure/operator && \
  /usr/bin/sha256sum --check --strict recovery-v3.reward-evidence.json.sha256)
```

The finalizer changes **only** the prearchive manifest's four zero roots. Every
run writes a fresh mode-0400 attempt manifest and sidecar. The sidecar is
create-installed first and the canonical manifest last; an existing canonical
pair is accepted only when both fresh outputs are byte-identical, so a retry
cannot silently reuse stale archive roots or replace history. Validation resets
those four fields to zero and requires the
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
`/community/reward_policy`, `/workers/scoreboard`, `/shards`,
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
frontend_config=/secure/operator/arc-network.recovered.json
"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/recovery_rollout.py frontend-config \
  --manifest "$final_manifest" \
  --reward-evidence "$reward_evidence" \
  --output "$frontend_config"
(cd /secure/operator && \
  /usr/bin/sha256sum --check --strict arc-network.recovered.json.sha256)
```

## Create the compact release handoff and publish

Do this only after the finalized manifest, maintenance boundary, quorum-signed
checkpoint, six-validator restart proof, and recovered frontend proof above are
complete. Remain in the same clean, detached, exact-`$protected_main_sha`
checkout. The private handoff directory must contain exactly the four files
shown below, on protected local operator storage, with no write bits. Neither
the multi-gigabyte checkpoint nor the private finalized rollout manifest is
committed or uploaded. The helper derives three bounded public JSON files,
creates a commit whose sole parent is the exact protected-main commit, and
publishes its content-addressed
`refs/arc-recovery-handoffs/<protected-main-sha>` ref. The helper runs both
derivation/verification programs under isolated `python -I` with a five-variable
non-secret environment. Only its hardened Git publisher receives the bounded
token through a create-only askpass file, and the credential-free `origin` URL
must exactly identify this repository. A retry freshly re-derives the commit,
accepts only the identical local or remote ref, and never moves a mismatch.
After any push error it re-probes the remote: the exact ref is success, proven
absence is safely retryable, and an unavailable or different result stops with
the local ref preserved. Never put a token in the remote URL.

```bash
set -Eeuo pipefail
test "$PWD" = "$operator_checkout"
unset GH_TOKEN GITHUB_TOKEN
test "$(arc_git -C "$operator_checkout" rev-parse HEAD)" = "$protected_main_sha"
arc_git -C "$operator_checkout" diff-index --quiet HEAD --
test -z "$(arc_git -C "$operator_checkout" status --porcelain=v1 --untracked-files=all)"
test "$(arc_git -C "$operator_checkout" remote get-url --all origin)" = \
  https://github.com/FerrumVir/arc-chain.git
test "$(arc_git -C "$operator_checkout" remote get-url --push --all origin)" = \
  https://github.com/FerrumVir/arc-chain.git

full_handoff_dir="$(
  /usr/bin/mktemp -d /secure/operator/release-handoff-v0.8.0.XXXXXXXX
)"
release_control_root="$(
  /usr/bin/mktemp -d /secure/operator/release-control-v0.8.0.XXXXXXXX
)"
test "$(/usr/bin/stat --format='%a:%h' "$full_handoff_dir")" = 700:1
test "$(/usr/bin/stat --format='%a:%h' "$release_control_root")" = 700:1
write_once_or_compare() {
  local destination="$1" candidate
  case "$destination" in "$release_control_root"/*) ;; *) return 1 ;; esac
  candidate="$(/usr/bin/mktemp "$release_control_root/.receipt.XXXXXXXX")"
  /usr/bin/cat > "$candidate"
  /usr/bin/chmod 0400 "$candidate"
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    test -f "$destination" && test ! -L "$destination"
    test "$(/usr/bin/stat --format='%a:%h' "$destination")" = 400:1
    /usr/bin/cmp -s "$candidate" "$destination"
  else
    /usr/bin/ln -- "$candidate" "$destination"
    /usr/bin/sync -f "$destination"
    /usr/bin/sync -f "$release_control_root"
  fi
  /usr/bin/rm -f -- "$candidate"
}
/usr/bin/install -m 0400 "$final_manifest" \
  "$full_handoff_dir/arc-recovery-final.lock.json"
/usr/bin/install -m 0400 "$final_manifest_sidecar" \
  "$full_handoff_dir/arc-recovery-final.lock.json.sha256"
/usr/bin/install -m 0400 "$legacy_maintenance_boundary" \
  "$full_handoff_dir/legacy-maintenance-boundary.json"
/usr/bin/install -m 0400 "$recovery_checkpoint" \
  "$full_handoff_dir/recovery.arcchkpt"

GH_TOKEN="$(
  "$ARC_RECOVERY_GH_PATH" auth token --hostname github.com \
    --user "$ARC_RECOVERY_GITHUB_LOGIN"
)"
test -n "$GH_TOKEN"
test "$(GH_TOKEN="$GH_TOKEN" "$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/branches/main --jq .commit.sha)" = "$protected_main_sha"
handoff_receipt="$(
  GH_TOKEN="$GH_TOKEN" "$ARC_RECOVERY_PYTHON_PATH" -I \
    scripts/release/create-cutover-handoff-commit.py \
    --repository-root "$operator_checkout" \
    --full-handoff-dir "$full_handoff_dir" \
    --verifier-binary "$arc_node_linux" \
    --inspector-binary "$arc_node_linux" \
    --genesis "$operator_genesis" \
    --main-commit "$protected_main_sha" \
    --tag v0.8.0 \
    --repository FerrumVir/arc-chain \
    --push-remote origin
)"
handoff_commit_sha="$(printf '%s' "$handoff_receipt" | /usr/bin/jq -er '.handoff_commit_sha')"
handoff_ref="refs/arc-recovery-handoffs/$protected_main_sha"
[[ "$handoff_commit_sha" =~ ^[0-9a-f]{40}$ ]]
printf '%s' "$handoff_receipt" | /usr/bin/jq -e \
  --arg commit "$handoff_commit_sha" --arg ref "$handoff_ref" \
  '.handoff_commit_sha == $commit and .handoff_ref == $ref
   and (.local_ref_state == "created" or .local_ref_state == "reused")
   and .pushed_remote == "origin"
   and (.remote_ref_state == "created" or .remote_ref_state == "reused")' >/dev/null
test "$(arc_git -C "$operator_checkout" rev-list --parents -n 1 \
  "$handoff_commit_sha")" = "$handoff_commit_sha $protected_main_sha"
test -z "$(arc_git -C "$operator_checkout" status --porcelain=v1 --untracked-files=all)"

handoff_workflow_id="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/actions/workflows/recovery-release-handoff.yml --jq .id)"
[[ "$handoff_workflow_id" =~ ^[1-9][0-9]*$ ]]
handoff_run_candidates() {
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/actions/workflows/recovery-release-handoff.yml/runs?event=workflow_dispatch&branch=main&per_page=100' \
    --jq '.workflow_runs[]' \
  | /usr/bin/jq -cs --arg sha "$protected_main_sha" \
      --argjson workflow_id "$handoff_workflow_id" '
      [.[] | select(.workflow_id == $workflow_id and .head_sha == $sha
        and .head_branch == "main"
        and .path == ".github/workflows/recovery-release-handoff.yml"
        and .event == "workflow_dispatch")]'
}
# This is the pinned-path form of: gh workflow run recovery-release-handoff.yml
handoff_runs="$(handoff_run_candidates)"
handoff_run_count="$(printf '%s' "$handoff_runs" | /usr/bin/jq -er length)"
case "$handoff_run_count" in
  0)
    GH_TOKEN="$GH_TOKEN" "$ARC_RECOVERY_GH_PATH" workflow run recovery-release-handoff.yml \
      --repo FerrumVir/arc-chain \
      --ref main \
      -f handoff_commit_sha="$handoff_commit_sha"
    for _ in {1..30}; do
      /usr/bin/sleep 2
      handoff_runs="$(handoff_run_candidates)"
      handoff_run_count="$(printf '%s' "$handoff_runs" | /usr/bin/jq -er length)"
      [ "$handoff_run_count" -eq 0 ] || break
    done
    test "$handoff_run_count" -eq 1
    ;;
  1)
    printf 'reusing the one exact existing compact-handoff run\n' >&2
    ;;
  *)
    printf 'compact-handoff run selection is ambiguous; preserve and stop\n' >&2
    exit 1
    ;;
esac
handoff_run_id="$(printf '%s' "$handoff_runs" | /usr/bin/jq -er '.[0].id')"
handoff_run_attempt="$(printf '%s' "$handoff_runs" | /usr/bin/jq -er '.[0].run_attempt')"
[[ "$handoff_run_id" =~ ^[1-9][0-9]*$ ]]
[[ "$handoff_run_attempt" =~ ^[1-9][0-9]*$ ]]
/usr/bin/jq -cnS --argjson id "$handoff_run_id" \
  --argjson attempt "$handoff_run_attempt" --arg sha "$protected_main_sha" \
  --argjson workflow_id "$handoff_workflow_id" \
  '{schema:"arc.release-handoff-run-selection.v1",repository:"FerrumVir/arc-chain",
    workflow_id:$workflow_id,
    workflow_path:".github/workflows/recovery-release-handoff.yml",
    event:"workflow_dispatch",head_branch:"main",head_sha:$sha,
    run_id:$id,run_attempt:$attempt}' \
  | write_once_or_compare "$release_control_root/HANDOFF-RUN-SELECTION.json"
printf 'Approve only handoff run %s attempt %s, then wait here.\n' \
  "$handoff_run_id" "$handoff_run_attempt"
"$ARC_RECOVERY_GH_PATH" run watch "$handoff_run_id" \
  --repo FerrumVir/arc-chain --interval 10 --exit-status
```

Approve that one exact `release`-environment deployment, wait for the
`recovery-release-handoff.yml` run selected above to succeed. Then independently
resolve its one live artifact. The API object must
name the exact main commit, successful producer run, numeric immutable ID, and
GitHub server digest; values copied only from a log line are not release
inputs.

```bash
"$ARC_RECOVERY_GH_PATH" api \
  "repos/FerrumVir/arc-chain/actions/runs/$handoff_run_id" \
  | /usr/bin/jq -e --arg sha "$protected_main_sha" --argjson id "$handoff_run_id" \
      --argjson attempt "$handoff_run_attempt" --argjson workflow_id "$handoff_workflow_id" \
      '.id == $id and .workflow_id == $workflow_id
       and .head_repository.full_name == "FerrumVir/arc-chain"
       and .head_sha == $sha and .head_branch == "main"
       and .path == ".github/workflows/recovery-release-handoff.yml"
       and .event == "workflow_dispatch" and .run_attempt == $attempt
       and .status == "completed" and .conclusion == "success"' >/dev/null
handoff_artifact_json="$(
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    "repos/FerrumVir/arc-chain/actions/runs/$handoff_run_id/artifacts?per_page=100" \
    --jq '.artifacts[]' \
  | /usr/bin/jq -cs --arg name "arc-recovery-release-handoff-$protected_main_sha" \
      --arg sha "$protected_main_sha" --argjson run_id "$handoff_run_id" \
      '[.[] | select(.name == $name and .expired == false
          and .workflow_run.id == $run_id and .workflow_run.head_sha == $sha
          and (.digest | test("^sha256:[0-9a-f]{64}$"))
          and (.size_in_bytes > 0 and .size_in_bytes <= 33554432))]
       | if length == 1 then .[0]
         else error("expected exactly one live compact handoff artifact") end'
)"
handoff_artifact_id="$(printf '%s' "$handoff_artifact_json" | /usr/bin/jq -er '.id')"
handoff_artifact_digest="$(printf '%s' "$handoff_artifact_json" | /usr/bin/jq -er '.digest')"
[[ "$handoff_artifact_id" =~ ^[1-9][0-9]*$ ]]
[[ "$handoff_artifact_digest" =~ ^sha256:[0-9a-f]{64}$ ]]
```

Immediately before tag creation, re-prove the owner session and immutable-release
setting, require the exact direct-collaborator set `FerrumVir` plus
`arisarcmarket`, and use one bounded owner API call to reduce only
`arisarcmarket` to `pull`. Then re-query the complete set to prove that the sole
non-owner retains no `write`, `maintain`, or `admin`. An unexpected collaborator
fails closed instead of being silently mutated. Also prove unchanged protected `main` and the
remote absence of both the tag and its release. If any check fails, do not
create the tag. Create the lightweight protected tag with one isolated,
authenticated Git push to the exact credential-free HTTPS URL. The push clears
ambient credential helpers and HTTP headers, disables hooks and all protocols
except HTTPS, uses only the pinned `gh auth git-credential` helper, and carries
an empty expected-value lease so it can create but never move the tag. Re-read
the remote ref after every push result instead of treating the Git exit status
as the final proof.

```bash
test "$("$ARC_RECOVERY_GH_PATH" api /user --jq .login)" = FerrumVir
test "$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/immutable-releases --jq .enabled)" = true
test "$("$ARC_RECOVERY_GH_PATH" api repos/FerrumVir/arc-chain/branches/main \
  --jq .commit.sha)" = "$protected_main_sha"
direct_collaborators_before="$(
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/collaborators?affiliation=direct&per_page=100' \
    --jq '.[]' | /usr/bin/jq -cs 'sort_by(.login)'
)"
printf '%s' "$direct_collaborators_before" | /usr/bin/jq -e '
  length == 2 and .[0].login == "FerrumVir" and .[0].permissions.admin == true
  and .[1].login == "arisarcmarket"
  and (.[1].role_name == "write" or .[1].role_name == "read")' >/dev/null
"$ARC_RECOVERY_GH_PATH" api --method PUT \
  repos/FerrumVir/arc-chain/collaborators/arisarcmarket \
  -f permission=pull >/dev/null
direct_collaborators_after="$(
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/collaborators?affiliation=direct&per_page=100' \
    --jq '.[]' | /usr/bin/jq -cs 'sort_by(.login)'
)"
printf '%s' "$direct_collaborators_after" | /usr/bin/jq -e '
  length == 2 and .[0].login == "FerrumVir" and .[0].permissions.admin == true
  and .[1].login == "arisarcmarket" and .[1].role_name == "read"
  and .[1].permissions.pull == true and .[1].permissions.push == false
  and .[1].permissions.maintain == false and .[1].permissions.admin == false' >/dev/null
pending_writer_invitations="$(
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/invitations?per_page=100' --jq '.[]' \
  | /usr/bin/jq -cs \
      '[.[] | select(.permissions == "write" or .permissions == "maintain"
                     or .permissions == "admin")] | length'
)"
test "$pending_writer_invitations" = 0
remote_tag_before="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/git/matching-refs/tags/v0.8.0 --jq .)"
remote_tag_state="$(printf '%s' "$remote_tag_before" | /usr/bin/jq -er \
  --arg sha "$protected_main_sha" '
    [.[] | select(.ref == "refs/tags/v0.8.0")] as $matches
    | if ($matches | length) == 0 then "absent"
    elif ($matches | length) == 1
         and $matches[0].object.type == "commit"
         and $matches[0].object.sha == $sha then "exact"
    else "mismatch" end')"
case "$remote_tag_state" in
  absent|exact) ;;
  mismatch)
    printf 'v0.8.0 exists at a different remote identity; preserve and stop\n' >&2
    exit 1
    ;;
  *) exit 1 ;;
esac
remote_release_count="$(
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/releases?per_page=100' --jq '.[]' \
  | /usr/bin/jq -cs '[.[] | select(.tag_name == "v0.8.0")] | length'
)"
test "$remote_release_count" = 0
creation_ruleset="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/rulesets/21690216)"
printf '%s' "$creation_ruleset" | /usr/bin/jq -e '
  .id == 21690216 and .name == "Restrict all ARC tag creation"
  and .target == "tag" and .enforcement == "active"
  and .conditions == {"ref_name":{"exclude":[],"include":["~ALL"]}}
  and .rules == [{"type":"creation"}]
  and .bypass_actors == [{"actor_id":111036403,"actor_type":"User",
                          "bypass_mode":"always"}]' >/dev/null
mutation_ruleset="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/rulesets/21667203)"
printf '%s' "$mutation_ruleset" | /usr/bin/jq -e '
  .id == 21667203 and .name == "Protect all ARC tags from mutation"
  and .target == "tag" and .enforcement == "active"
  and .conditions == {"ref_name":{"exclude":[],"include":["~ALL"]}}
  and .rules == [{"type":"update"},{"type":"deletion"},
                 {"type":"non_fast_forward"}]
  and .bypass_actors == []' >/dev/null
tag_ref_push_status=0
if [ "$remote_tag_state" = absent ]; then
  tag_push_attempt_root="$(
    /usr/bin/mktemp -d "$release_control_root/tag-push.XXXXXXXX"
  )"
  test "$(/usr/bin/stat --format='%a:%h' "$tag_push_attempt_root")" = 700:1
  tag_push_stdout="$tag_push_attempt_root/STDOUT.txt"
  tag_push_stderr="$tag_push_attempt_root/STDERR.txt"
  ( umask 077
    set -o noclobber
    /usr/bin/env -i \
      HOME="$git_home" PATH=/usr/bin:/bin LANG=C LC_ALL=C TZ=UTC \
      GH_TOKEN="$GH_TOKEN" GH_PROMPT_DISABLED=1 \
      GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
      GIT_TERMINAL_PROMPT=0 \
      /usr/bin/git -C "$operator_checkout" \
        -c core.hooksPath=/dev/null \
        -c credential.helper= \
        -c "credential.https://github.com.helper=!$ARC_RECOVERY_GH_PATH auth git-credential" \
        -c http.extraHeader= \
        -c http.https://github.com/.extraHeader= \
        -c http.sslVerify=true \
        -c protocol.allow=never \
        -c protocol.https.allow=always \
        push --porcelain --atomic --no-verify \
        --force-with-lease=refs/tags/v0.8.0: \
        -- https://github.com/FerrumVir/arc-chain.git \
        "$protected_main_sha:refs/tags/v0.8.0" \
        >"$tag_push_stdout" 2>"$tag_push_stderr"
  ) || tag_ref_push_status=$?
fi
remote_tag_after="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/git/matching-refs/tags/v0.8.0 --jq .)"
remote_tag_after_state="$(printf '%s' "$remote_tag_after" | /usr/bin/jq -er \
  --arg sha "$protected_main_sha" '
    [.[] | select(.ref == "refs/tags/v0.8.0")] as $matches
    | if ($matches | length) == 0 then "absent"
    elif ($matches | length) == 1
         and $matches[0].object.type == "commit"
         and $matches[0].object.sha == $sha then "exact"
    else "mismatch" end')"
case "$remote_tag_after_state" in
  exact)
    if [ "$tag_ref_push_status" -ne 0 ]; then
      printf 'tag push returned %s, but the mandatory post-query proved the exact tag\n' \
        "$tag_ref_push_status" >&2
    fi
    ;;
  absent)
    printf 'tag push did not create a remote ref; absence is proven and a fresh retry is safe\n' >&2
    exit 1
    ;;
  mismatch)
    printf 'post-create v0.8.0 remote identity differs; preserve and stop\n' >&2
    exit 1
    ;;
  *) exit 1 ;;
esac
```

Tag creation automatically starts exactly one `push` form of `release.yml`;
that run is expected to fail in its initial validation job because a tag-push event
cannot carry the protected handoff artifact ID and digest. Select it by
workflow ID, path, event, branch, and exact SHA rather than copying a run ID
from the UI. An exact existing selection is resumable; zero or multiple runs
after the bounded discovery poll stop without moving or re-pushing the tag.

```bash
release_workflow_id="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/actions/workflows/release.yml --jq .id)"
[[ "$release_workflow_id" =~ ^[1-9][0-9]*$ ]]
tag_push_run_candidates() {
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/actions/workflows/release.yml/runs?event=push&branch=v0.8.0&per_page=100' \
    --jq '.workflow_runs[]' \
  | /usr/bin/jq -cs --arg sha "$protected_main_sha" \
      --argjson workflow_id "$release_workflow_id" '
      [.[] | select(.workflow_id == $workflow_id and .head_sha == $sha
        and .head_branch == "v0.8.0"
        and .path == ".github/workflows/release.yml" and .event == "push")]'
}
tag_push_runs='[]'
for _ in {1..30}; do
  tag_push_runs="$(tag_push_run_candidates)"
  tag_push_run_count="$(printf '%s' "$tag_push_runs" | /usr/bin/jq -er length)"
  [ "$tag_push_run_count" -eq 0 ] || break
  /usr/bin/sleep 2
done
test "$tag_push_run_count" -eq 1
tag_push_run_id="$(printf '%s' "$tag_push_runs" | /usr/bin/jq -er '.[0].id')"
tag_push_run_attempt="$(printf '%s' "$tag_push_runs" | /usr/bin/jq -er '.[0].run_attempt')"
[[ "$tag_push_run_id" =~ ^[1-9][0-9]*$ ]]
[[ "$tag_push_run_attempt" =~ ^[1-9][0-9]*$ ]]
/usr/bin/jq -cnS --argjson id "$tag_push_run_id" \
  --argjson attempt "$tag_push_run_attempt" --arg sha "$protected_main_sha" \
  --argjson workflow_id "$release_workflow_id" \
  '{schema:"arc.release-tag-push-run-selection.v1",repository:"FerrumVir/arc-chain",
    workflow_id:$workflow_id,workflow_path:".github/workflows/release.yml",
    event:"push",head_branch:"v0.8.0",head_sha:$sha,
    run_id:$id,run_attempt:$attempt}' \
  | write_once_or_compare "$release_control_root/TAG-PUSH-RUN-SELECTION.json"
"$ARC_RECOVERY_GH_PATH" run watch "$tag_push_run_id" \
  --repo FerrumVir/arc-chain --interval 10
"$ARC_RECOVERY_GH_PATH" api \
  "repos/FerrumVir/arc-chain/actions/runs/$tag_push_run_id" \
  | /usr/bin/jq -e --arg sha "$protected_main_sha" --argjson id "$tag_push_run_id" \
      --argjson attempt "$tag_push_run_attempt" --argjson workflow_id "$release_workflow_id" \
      '.id == $id and .run_attempt == $attempt and .workflow_id == $workflow_id
       and .head_repository.full_name == "FerrumVir/arc-chain"
       and .head_sha == $sha and .head_branch == "v0.8.0"
       and .path == ".github/workflows/release.yml" and .event == "push"
       and .status == "completed" and .conclusion == "failure"' >/dev/null
"$ARC_RECOVERY_GH_PATH" api \
  "repos/FerrumVir/arc-chain/actions/runs/$tag_push_run_id/jobs?filter=all&per_page=100" \
  | /usr/bin/jq -e '
      ([.jobs[] | select(.name == "Validate release tag and pin commit"
        and .conclusion == "failure")] | length) == 1
      and ([.jobs[] | select(
        (.name == "Create and upload one isolated release draft"
         or .name == "Publish only the independently verified draft")
        and .conclusion != "skipped")] | length) == 0' >/dev/null
tag_push_failure_log="$(
  "$ARC_RECOVERY_GH_PATH" run view "$tag_push_run_id" \
    --repo FerrumVir/arc-chain --log-failed
)"
printf '%s\n' "$tag_push_failure_log" | /usr/bin/grep -Fq \
  'Release requires workflow_dispatch with a positive cutover_handoff_artifact_id.'
```

Only after that automatic run has stopped and passed the negative proof above,
select or create exactly one manual run of the same workflow definition on
`main`. This block is resumable: an already-dispatched exact-SHA run is reused,
and an existing release prevents a second dispatch. More than one matching run,
or a release without its matching run, is ambiguous and stops. The create-only
selection receipt records the run ID and attempt before any environment
approval. Approve deployments only when GitHub shows that exact run ID and
attempt. Do not move, delete, recreate, or re-push the tag.

```bash
release_workflow_id="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/actions/workflows/release.yml --jq .id)"
[[ "$release_workflow_id" =~ ^[1-9][0-9]*$ ]]
release_run_candidates() {
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/actions/workflows/release.yml/runs?event=workflow_dispatch&branch=main&per_page=100' \
    --jq '.workflow_runs[]' \
  | /usr/bin/jq -cs --arg sha "$protected_main_sha" \
      --argjson workflow_id "$release_workflow_id" '
      [.[] | select(.workflow_id == $workflow_id and .head_sha == $sha
        and .head_branch == "main"
        and .path == ".github/workflows/release.yml"
        and .event == "workflow_dispatch")]'
}
manual_release_runs="$(release_run_candidates)"
manual_release_run_count="$(printf '%s' "$manual_release_runs" | /usr/bin/jq -er length)"
existing_release_count="$(
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/releases?per_page=100' --jq '.[]' \
  | /usr/bin/jq -cs '[.[] | select(.tag_name == "v0.8.0")] | length'
)"
case "$manual_release_run_count:$existing_release_count" in
  0:0)
    "$ARC_RECOVERY_GH_PATH" workflow run release.yml \
      --repo FerrumVir/arc-chain \
      --ref main \
      -f tag=v0.8.0 \
      -f cutover_handoff_artifact_id="$handoff_artifact_id" \
      -f cutover_handoff_artifact_digest="$handoff_artifact_digest"
    for _ in {1..30}; do
      /usr/bin/sleep 2
      manual_release_runs="$(release_run_candidates)"
      manual_release_run_count="$(printf '%s' "$manual_release_runs" | /usr/bin/jq -er length)"
      [ "$manual_release_run_count" -eq 0 ] || break
    done
    test "$manual_release_run_count" -eq 1
    ;;
  1:0|1:1)
    printf 'reusing the one exact existing manual release run\n' >&2
    ;;
  0:1)
    printf 'v0.8.0 exists without its exact manual release run; preserve and stop\n' >&2
    exit 1
    ;;
  *)
    printf 'manual release run/release selection is ambiguous; preserve and stop\n' >&2
    exit 1
    ;;
esac
release_run_id="$(printf '%s' "$manual_release_runs" | /usr/bin/jq -er '.[0].id')"
release_run_attempt="$(printf '%s' "$manual_release_runs" | /usr/bin/jq -er '.[0].run_attempt')"
[[ "$release_run_id" =~ ^[1-9][0-9]*$ ]]
[[ "$release_run_attempt" =~ ^[1-9][0-9]*$ ]]
/usr/bin/jq -cnS --argjson id "$release_run_id" \
  --argjson attempt "$release_run_attempt" --arg sha "$protected_main_sha" \
  --argjson workflow_id "$release_workflow_id" \
  '{schema:"arc.release-run-selection.v1",repository:"FerrumVir/arc-chain",
    workflow_id:$workflow_id,workflow_path:".github/workflows/release.yml",
    event:"workflow_dispatch",head_branch:"main",head_sha:$sha,
    run_id:$id,run_attempt:$attempt}' \
  | write_once_or_compare "$release_control_root/RELEASE-RUN-SELECTION.json"
printf 'Approve only release run %s attempt %s, then wait here.\n' \
  "$release_run_id" "$release_run_attempt"
"$ARC_RECOVERY_GH_PATH" run watch "$release_run_id" \
  --repo FerrumVir/arc-chain --interval 10 --exit-status
```

After the watch returns success, re-read the run rather than trusting terminal
output. Require the exact workflow/path/event/branch/SHA/attempt, every required
matrix and publication job, and the final read-only immutable-release verifier.
Then query the live release by tag and prove its lifecycle, target, publisher,
and complete 32-asset name/size/digest/uploader contract. These projections are
stored create-only so a resumed audit must reproduce the same terminal facts.

```bash
release_run_final="$release_control_root/RELEASE-RUN-FINAL.json"
release_jobs_final="$release_control_root/RELEASE-JOBS-FINAL.json"
release_api_final="$release_control_root/RELEASE-API-FINAL.json"
release_run_live="$("$ARC_RECOVERY_GH_PATH" api \
  "repos/FerrumVir/arc-chain/actions/runs/$release_run_id")"
printf '%s' "$release_run_live" | /usr/bin/jq -e \
  --argjson id "$release_run_id" --argjson attempt "$release_run_attempt" \
  --argjson workflow_id "$release_workflow_id" --arg sha "$protected_main_sha" '
  .id == $id and .run_attempt == $attempt and .workflow_id == $workflow_id
  and .head_repository.full_name == "FerrumVir/arc-chain"
  and .head_sha == $sha and .head_branch == "main"
  and .path == ".github/workflows/release.yml"
  and .event == "workflow_dispatch"
  and .status == "completed" and .conclusion == "success"' >/dev/null
printf '%s' "$release_run_live" | /usr/bin/jq -cS \
  '{id,run_attempt,workflow_id,path,event,head_branch,head_sha,status,conclusion}' \
  | write_once_or_compare "$release_run_final"

required_release_jobs='[
  "Validate release tag and pin commit",
  "Full quality gate on validated release commit",
  "Cargo dependency policy (workspace)",
  "Cargo dependency policy (desktop)",
  "Cargo dependency policy (updater-verifier)",
  "Golden vectors (linux-x86_64)",
  "Golden vectors (linux-arm64)",
  "Golden vectors (macos-arm64)",
  "Golden vectors (macos-x86_64)",
  "Golden vectors (windows-x86_64)",
  "Verify exact pre-tag headless linux-x86_64",
  "Verify exact pre-tag headless linux-arm64",
  "Verify exact pre-tag headless macos-arm64",
  "Verify exact pre-tag headless macos-x86_64",
  "Verify exact pre-tag headless windows-x86_64",
  "Ubuntu server smoke (linux-x86_64)",
  "Ubuntu server smoke (linux-arm64)",
  "Verify exact pre-tag desktop macos-arm64",
  "Verify exact pre-tag desktop macos-x86_64",
  "Verify exact pre-tag desktop windows-x86_64",
  "Verify exact pre-tag desktop linux-x86_64",
  "Assemble the exact unsigned release manifest",
  "Sign only the verified release manifest",
  "Create and upload one isolated release draft",
  "Verify GitHub draft bytes without publication authority",
  "Publish only the independently verified draft",
  "Verify the immutable GitHub release without publication authority"
]'
release_jobs_live="$("$ARC_RECOVERY_GH_PATH" api --paginate \
  "repos/FerrumVir/arc-chain/actions/runs/$release_run_id/jobs?filter=all&per_page=100" \
  --jq '.jobs[]' | /usr/bin/jq -cs 'sort_by(.name)')"
printf '%s' "$release_jobs_live" | /usr/bin/jq -e \
  --argjson required "$required_release_jobs" '
  ([.[] | select(.status == "completed" and .conclusion == "success") | .name]
    | sort) == ($required | sort)
  and ([.[] | select(.name == "Delete a draft rejected by the unprivileged verifier"
    and .status == "completed" and .conclusion == "skipped")] | length) == 1
  and length == (($required | length) + 1)' >/dev/null
printf '%s' "$release_jobs_live" | /usr/bin/jq -cS \
  '[.[] | {id,name,status,conclusion}] | sort_by(.name)' \
  | write_once_or_compare "$release_jobs_final"

expected_release_assets='[
  "arc-node-linux-x86_64","arc-cli-linux-x86_64",
  "arc-node-linux-arm64","arc-cli-linux-arm64",
  "arc-node-macos-arm64","arc-cli-macos-arm64",
  "arc-node-macos-x86_64","arc-cli-macos-x86_64",
  "arc-node-windows-x86_64.exe","arc-cli-windows-x86_64.exe",
  "arc-desktop-macos-arm64.app.tar.gz","arc-desktop-macos-arm64.app.tar.gz.sig",
  "arc-desktop-macos-arm64.dmg","arc-desktop-macos-x86_64.app.tar.gz",
  "arc-desktop-macos-x86_64.app.tar.gz.sig","arc-desktop-macos-x86_64.dmg",
  "arc-desktop-windows-x86_64-setup.exe","arc-desktop-windows-x86_64-setup.exe.sig",
  "arc-desktop-windows-x86_64.msi","arc-desktop-linux-x86_64.AppImage",
  "arc-desktop-linux-x86_64.AppImage.sig","arc-desktop-linux-x86_64.deb",
  "arc-desktop-linux-x86_64.rpm","install.sh","testnet-seeds.txt","genesis.toml",
  "arc-legacy-maintenance-boundary.json","arc-recovery-checkpoint-descriptor.json",
  "arc-cutover-policy.json","latest.json","SHA256SUMS","SHA256SUMS.sig"
]'
release_api_live="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/releases/tags/v0.8.0)"
printf '%s' "$release_api_live" | /usr/bin/jq -e \
  --arg sha "$protected_main_sha" --argjson expected "$expected_release_assets" '
  .tag_name == "v0.8.0" and .target_commitish == $sha
  and .draft == false and .prerelease == false and .immutable == true
  and .author.login == "github-actions[bot]"
  and ([.assets[].name] | sort) == ($expected | sort)
  and ([.assets[].name] | unique | length) == 32
  and ([.assets[].size] | add) <= 12884901888
  and all(.assets[];
    .state == "uploaded" and (.digest | test("^sha256:[0-9a-f]{64}$"))
    and .uploader.login == "github-actions[bot]" and .size > 0
    and .size <= (if .name == "arc-recovery-checkpoint-descriptor.json" then 1048576
                  elif (.name | endswith(".sig") or endswith(".json")) then 4194304
                  else 2147483648 end))' >/dev/null
release_id="$(printf '%s' "$release_api_live" | /usr/bin/jq -er .id)"
[[ "$release_id" =~ ^[1-9][0-9]*$ ]]
printf '%s' "$release_api_live" | /usr/bin/jq -cS \
  '{id,tag_name,target_commitish,draft,prerelease,immutable,author:.author.login,
    assets:(.assets | map({id,name,size,digest,state,uploader:.uploader.login})
      | sort_by(.name))}' \
  | write_once_or_compare "$release_api_final"
unset GH_TOKEN
```

A rejected draft
may be deleted before publication only after re-proving its exact release ID,
tag, target commit, `draft: true`, and `immutable: false` state. Once the
publication PATCH has been attempted, the workflow deliberately retains the
release on every error. If `immutable: true` is not observed within the bounded
poll, stop and manually verify that exact release ID and the repository audit
trail. Never delete the ambiguous or published object and never rerun the tag
or release workflow.

Keep the runtime package-mutation masks in place through the immutable tag,
six-validator restart proof, public frontend hash proof, and installer/update
canaries. Do not unmask anything at this point; the executable post-release
gates below come first.

An air-gapped review may instead supply both `--archive-manifest` and
`--archive-complete`; they must be canonical mode-read-only files matching the
same finalized roots. Supplying only one fails closed. The default has no
manual local-file handoff and still never mounts or serves Drive.

### Publish the recovered frontend through one reviewed PR

The output and sidecar are create-only mode `0444`. Derive a deterministic
single-parent commit that changes only `shared/frontend/arc-network.json`,
publish its dedicated branch with a create-only lease, and reuse only an exact
existing branch on resume. Never push directly to `main`. The pull request must
retain the exact head and pass every required check. Prefer an exact-head
approval from `arisarcmarket`. If that reviewer is unavailable, the owner may
temporarily change only the main ruleset's approval count from one to zero and
last-push approval from true to false. That exception is allowed only from the
sealed exact policy snapshot, is recorded, and is guarded by an EXIT/signal
restoration trap; it never disables checks, thread resolution, linear history,
or any tag rule. A leftover exact exception is restored before a retry, while
any third policy state stops without mutation. Merge by squash, as required by
the live linear-history rule. The resulting `main` commit must have the release
source as its sole parent and exactly the reviewed frontend tree and bytes.
Finally re-prove the protected tag and the canonical ruleset snapshot hash.

```bash
test "$(arc_sha256 "$frontend_config")" = \
  "$(/usr/bin/awk '{print $1}' "$frontend_config.sha256")"
test "$("$ARC_RECOVERY_GH_PATH" api repos/FerrumVir/arc-chain/branches/main \
  --jq .commit.sha)" = "$protected_main_sha"
test "$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/git/ref/tags/v0.8.0 --jq .object.sha)" = \
  "$protected_main_sha"

repository_ruleset_snapshot() {
  local ruleset_ids id
  ruleset_ids="$("$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/rulesets?includes_parents=true&per_page=100' \
    --jq '.[].id' | /usr/bin/sort -n)"
  while IFS= read -r id; do
    [[ "$id" =~ ^[1-9][0-9]*$ ]]
    "$ARC_RECOVERY_GH_PATH" api "repos/FerrumVir/arc-chain/rulesets/$id" \
      | /usr/bin/jq -cS \
          '{id,name,target,source,source_type,enforcement,bypass_actors,conditions,rules}'
  done <<< "$ruleset_ids" | /usr/bin/jq -csS 'sort_by(.id)'
}
rulesets_before_frontend="$(repository_ruleset_snapshot)"
test "$(printf '%s' "$rulesets_before_frontend" | /usr/bin/jq -er length)" -gt 0

frontend_branch='arc-recovery/frontend-v0.8.0'
frontend_index="$(/usr/bin/mktemp "$release_control_root/frontend-index.XXXXXXXX")"
/usr/bin/rm -f -- "$frontend_index"
arc_index_git() {
  /usr/bin/env -i HOME="$git_home" PATH=/usr/bin:/bin LANG=C LC_ALL=C \
    GIT_CONFIG_NOSYSTEM=1 GIT_INDEX_FILE="$frontend_index" \
    /usr/bin/git -C "$operator_checkout" "$@"
}
arc_index_git read-tree "$protected_main_sha^{tree}"
frontend_blob_sha="$(arc_git -C "$operator_checkout" hash-object -w -- "$frontend_config")"
[[ "$frontend_blob_sha" =~ ^[0-9a-f]{40}$ ]]
arc_index_git update-index --add --cacheinfo 100644 "$frontend_blob_sha" \
  shared/frontend/arc-network.json
frontend_tree_sha="$(arc_index_git write-tree)"
frontend_parent_date="$(arc_git -C "$operator_checkout" show -s --format=%cI \
  "$protected_main_sha")"
frontend_commit_sha="$(
  /usr/bin/printf '%s\n' 'Publish verified ARC v0.8 recovery frontend' \
  | /usr/bin/env -i HOME="$git_home" PATH=/usr/bin:/bin LANG=C LC_ALL=C TZ=UTC \
      GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=FerrumVir \
      GIT_AUTHOR_EMAIL=111036403+FerrumVir@users.noreply.github.com \
      GIT_AUTHOR_DATE="$frontend_parent_date" GIT_COMMITTER_NAME=FerrumVir \
      GIT_COMMITTER_EMAIL=111036403+FerrumVir@users.noreply.github.com \
      GIT_COMMITTER_DATE="$frontend_parent_date" \
      /usr/bin/git -C "$operator_checkout" -c commit.gpgSign=false \
        commit-tree "$frontend_tree_sha" -p "$protected_main_sha"
)"
/usr/bin/rm -f -- "$frontend_index"
[[ "$frontend_commit_sha" =~ ^[0-9a-f]{40}$ ]]
test "$(arc_git -C "$operator_checkout" rev-list --parents -n 1 \
  "$frontend_commit_sha")" = "$frontend_commit_sha $protected_main_sha"
test "$(arc_git -C "$operator_checkout" diff-tree --no-commit-id --name-only -r \
  "$frontend_commit_sha")" = shared/frontend/arc-network.json
arc_git -C "$operator_checkout" show \
  "$frontend_commit_sha:shared/frontend/arc-network.json" \
  | /usr/bin/cmp -s - "$frontend_config"

frontend_remote_refs="$("$ARC_RECOVERY_GH_PATH" api \
  'repos/FerrumVir/arc-chain/git/matching-refs/heads/arc-recovery/frontend/v0.8.0' \
  --jq .)"
frontend_remote_state="$(printf '%s' "$frontend_remote_refs" | /usr/bin/jq -er \
  --arg sha "$frontend_commit_sha" --arg ref "refs/heads/$frontend_branch" '
  [.[] | select(.ref == $ref)] as $matches
  | if ($matches | length) == 0 then "absent"
    elif ($matches | length) == 1 and $matches[0].object.type == "commit"
         and $matches[0].object.sha == $sha then "exact"
    else "mismatch" end')"
case "$frontend_remote_state" in
  absent)
    frontend_git_token="$("$ARC_RECOVERY_GH_PATH" auth token \
      --hostname github.com --user "$ARC_RECOVERY_GITHUB_LOGIN")"
    test -n "$frontend_git_token"
    frontend_push_attempt_root="$(
      /usr/bin/mktemp -d "$release_control_root/frontend-push.XXXXXXXX"
    )"
    test "$(/usr/bin/stat --format='%a:%h' "$frontend_push_attempt_root")" = 700:1
    frontend_push_status=0
    ( umask 077
      set -o noclobber
      /usr/bin/env -i HOME="$git_home" PATH=/usr/bin:/bin LANG=C LC_ALL=C TZ=UTC \
        GH_TOKEN="$frontend_git_token" GH_PROMPT_DISABLED=1 \
        GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
        /usr/bin/git -C "$operator_checkout" -c core.hooksPath=/dev/null \
          -c credential.helper= \
          -c "credential.https://github.com.helper=!$ARC_RECOVERY_GH_PATH auth git-credential" \
          -c http.extraHeader= -c http.https://github.com/.extraHeader= \
          -c http.sslVerify=true \
          -c protocol.allow=never -c protocol.https.allow=always \
          push --porcelain --atomic --no-verify \
          --force-with-lease="refs/heads/$frontend_branch:" \
          -- https://github.com/FerrumVir/arc-chain.git \
          "$frontend_commit_sha:refs/heads/$frontend_branch" \
          >"$frontend_push_attempt_root/STDOUT.txt" \
          2>"$frontend_push_attempt_root/STDERR.txt"
    ) || frontend_push_status=$?
    unset frontend_git_token
    ;;
  exact) frontend_push_status=0 ;;
  *) printf 'frontend branch exists at another identity; preserve and stop\n' >&2; exit 1 ;;
esac
frontend_remote_after="$("$ARC_RECOVERY_GH_PATH" api \
  'repos/FerrumVir/arc-chain/git/matching-refs/heads/arc-recovery/frontend/v0.8.0' \
  --jq .)"
printf '%s' "$frontend_remote_after" | /usr/bin/jq -e \
  --arg sha "$frontend_commit_sha" --arg ref "refs/heads/$frontend_branch" '
  [.[] | select(.ref == $ref and .object.type == "commit" and .object.sha == $sha)]
  | length == 1' >/dev/null
if [ "$frontend_push_status" -ne 0 ]; then
  printf 'branch push returned %s, but the post-query proved the exact branch\n' \
    "$frontend_push_status" >&2
fi

frontend_pr_title='Publish verified ARC v0.8 recovery frontend'
frontend_pr_body='Publishes only the create-only, rollout-derived recovered frontend configuration after immutable v0.8.0 verification.'
frontend_prs="$("$ARC_RECOVERY_GH_PATH" api --method GET \
  repos/FerrumVir/arc-chain/pulls -f state=all \
  -f head="FerrumVir:$frontend_branch" -f base=main \
  | /usr/bin/jq -c --arg sha "$frontend_commit_sha" \
      '[.[] | select(.head.sha == $sha and .head.ref == "arc-recovery/frontend-v0.8.0"
        and .base.ref == "main")]')"
frontend_pr_count="$(printf '%s' "$frontend_prs" | /usr/bin/jq -er length)"
case "$frontend_pr_count" in
  0)
    test "$("$ARC_RECOVERY_GH_PATH" api repos/FerrumVir/arc-chain/branches/main \
      --jq .commit.sha)" = "$protected_main_sha"
    "$ARC_RECOVERY_GH_PATH" api --method POST repos/FerrumVir/arc-chain/pulls \
      -f title="$frontend_pr_title" -f head="$frontend_branch" -f base=main \
      -f body="$frontend_pr_body" -F draft=false >/dev/null
    ;;
  1) ;;
  *) printf 'frontend pull-request selection is ambiguous; preserve and stop\n' >&2; exit 1 ;;
esac
frontend_prs="$("$ARC_RECOVERY_GH_PATH" api --method GET \
  repos/FerrumVir/arc-chain/pulls -f state=all \
  -f head="FerrumVir:$frontend_branch" -f base=main \
  | /usr/bin/jq -c --arg sha "$frontend_commit_sha" \
      '[.[] | select(.head.sha == $sha and .head.ref == "arc-recovery/frontend-v0.8.0"
        and .base.ref == "main")]')"
test "$(printf '%s' "$frontend_prs" | /usr/bin/jq -er length)" -eq 1
frontend_pr_number="$(printf '%s' "$frontend_prs" | /usr/bin/jq -er '.[0].number')"
[[ "$frontend_pr_number" =~ ^[1-9][0-9]*$ ]]
frontend_main_ruleset_id=21689753
frontend_main_ruleset_policy() {
  "$ARC_RECOVERY_GH_PATH" api \
    "repos/FerrumVir/arc-chain/rulesets/$frontend_main_ruleset_id" \
    | /usr/bin/jq -cS \
        '{id,name,target,source,source_type,enforcement,bypass_actors,conditions,rules}'
}
frontend_main_ruleset_put() {
  printf '%s' "$1" | /usr/bin/jq -cS \
    '{name,target,enforcement,bypass_actors,conditions,rules}' \
    | "$ARC_RECOVERY_GH_PATH" api --method PUT \
        "repos/FerrumVir/arc-chain/rulesets/$frontend_main_ruleset_id" \
        --input - >/dev/null
}
frontend_ruleset_baseline_receipt="$release_control_root/FRONTEND-MAIN-RULESET-BASELINE.json"
if [ ! -e "$frontend_ruleset_baseline_receipt" ]; then
  frontend_main_ruleset_policy | write_once_or_compare \
    "$frontend_ruleset_baseline_receipt"
fi
test -f "$frontend_ruleset_baseline_receipt" \
  && test ! -L "$frontend_ruleset_baseline_receipt"
test "$(/usr/bin/stat --format='%a:%h' "$frontend_ruleset_baseline_receipt")" = 400:1
frontend_ruleset_baseline="$(/usr/bin/jq -cS . "$frontend_ruleset_baseline_receipt")"
printf '%s' "$frontend_ruleset_baseline" | /usr/bin/jq -e \
  --argjson id "$frontend_main_ruleset_id" '
  .id == $id and .name == "Protect ARC main release path"
  and .target == "branch" and .source == "FerrumVir/arc-chain"
  and .source_type == "Repository" and .enforcement == "active"
  and .bypass_actors == []
  and .conditions == {"ref_name":{"exclude":[],"include":["refs/heads/main"]}}
  and ([.rules[] | select(.type == "required_linear_history")] | length) == 1
  and ([.rules[] | select(.type == "pull_request"
    and .parameters.required_approving_review_count == 1
    and .parameters.require_last_push_approval == true
    and .parameters.dismiss_stale_reviews_on_push == true
    and .parameters.require_extra_approval_for_unattributed_changes == true
    and .parameters.required_review_thread_resolution == true
    and (.parameters.allowed_merge_methods | sort) == ["rebase","squash"])]
    | length) == 1
  and ([.rules[] | select(.type == "required_status_checks"
    and .parameters.strict_required_status_checks_policy == true
    and (.parameters.required_status_checks | length) > 0)] | length) == 1' >/dev/null
frontend_ruleset_exception="$(printf '%s' "$frontend_ruleset_baseline" \
  | /usr/bin/jq -cS '
      .rules |= map(if .type == "pull_request" then
        .parameters.required_approving_review_count = 0
        | .parameters.require_last_push_approval = false
      else . end)')"
frontend_ruleset_current="$(frontend_main_ruleset_policy)"
case "$frontend_ruleset_current" in
  "$frontend_ruleset_baseline") ;;
  "$frontend_ruleset_exception")
    printf 'restoring an exact interrupted frontend ruleset exception before retry\n' >&2
    frontend_main_ruleset_put "$frontend_ruleset_baseline"
    test "$(frontend_main_ruleset_policy)" = "$frontend_ruleset_baseline"
    ;;
  *)
    printf 'main ruleset differs from both sealed baseline and exact exception; preserve and stop\n' >&2
    exit 1
    ;;
esac
test "$(printf '%s' "$rulesets_before_frontend" | /usr/bin/jq -cS \
  --argjson id "$frontend_main_ruleset_id" '.[] | select(.id == $id)')" = \
  "$frontend_ruleset_baseline"
frontend_ruleset_baseline_sha="$(arc_sha256 "$frontend_ruleset_baseline_receipt")"
[[ "$frontend_ruleset_baseline_sha" =~ ^[0-9a-f]{64}$ ]]
frontend_ruleset_exception_active=false
restore_frontend_main_ruleset() {
  local current
  current="$(frontend_main_ruleset_policy)" || return 1
  case "$current" in
    "$frontend_ruleset_baseline") ;;
    "$frontend_ruleset_exception")
      frontend_main_ruleset_put "$frontend_ruleset_baseline" || return 1
      test "$(frontend_main_ruleset_policy)" = "$frontend_ruleset_baseline" || return 1
      ;;
    *)
      printf 'main ruleset changed concurrently; refusing to overwrite it during restore\n' >&2
      return 1
      ;;
  esac
  frontend_ruleset_exception_active=false
}
frontend_restore_on_exit() {
  local status=$?
  trap - EXIT HUP INT TERM
  if [ "$frontend_ruleset_exception_active" = true ]; then
    restore_frontend_main_ruleset || status=1
  fi
  exit "$status"
}
trap frontend_restore_on_exit EXIT
trap 'exit 130' HUP INT TERM

frontend_pr_state="$(printf '%s' "$frontend_prs" | /usr/bin/jq -er '.[0].state')"
frontend_review_authorization="$release_control_root/FRONTEND-REVIEW-AUTHORIZATION.json"
case "$frontend_pr_state" in
  open)
    "$ARC_RECOVERY_GH_PATH" pr checks "$frontend_pr_number" \
      --repo FerrumVir/arc-chain --required --watch --interval 10
    frontend_pr_view="$("$ARC_RECOVERY_GH_PATH" pr view "$frontend_pr_number" \
      --repo FerrumVir/arc-chain --json state,headRefOid,baseRefOid,reviewDecision)"
    printf '%s' "$frontend_pr_view" | /usr/bin/jq -e \
      --arg head "$frontend_commit_sha" --arg base "$protected_main_sha" '
      .state == "OPEN" and .headRefOid == $head and .baseRefOid == $base
      and .reviewDecision != "CHANGES_REQUESTED"' >/dev/null
    frontend_aris_approval_count="$(
      "$ARC_RECOVERY_GH_PATH" api --paginate \
        "repos/FerrumVir/arc-chain/pulls/$frontend_pr_number/reviews?per_page=100" \
        --jq '.[]' \
      | /usr/bin/jq -cs --arg head "$frontend_commit_sha" '
          [.[] | select(.user.login == "arisarcmarket" and .state == "APPROVED"
            and .commit_id == $head)] | length'
    )"
    if [ ! -e "$frontend_review_authorization" ]; then
      if [ "$(printf '%s' "$frontend_pr_view" | /usr/bin/jq -r .reviewDecision)" = APPROVED ] \
          && [ "$frontend_aris_approval_count" -ge 1 ]; then
        frontend_review_mode=arisarcmarket-approval
      else
        frontend_review_mode=temporary-ruleset-exception
      fi
      /usr/bin/jq -cnS --argjson pr "$frontend_pr_number" \
        --arg head "$frontend_commit_sha" --arg base "$protected_main_sha" \
        --arg mode "$frontend_review_mode" \
        --arg baseline_sha "$frontend_ruleset_baseline_sha" \
        '{schema:"arc.frontend-review-authorization.v1",pull_request:$pr,
          head_commit:$head,base_commit:$base,mode:$mode,
          reviewer:(if $mode == "arisarcmarket-approval" then "arisarcmarket" else null end),
          main_ruleset_id:21689753,main_ruleset_baseline_sha256:$baseline_sha}' \
        | write_once_or_compare "$frontend_review_authorization"
    fi
    frontend_review_mode="$(/usr/bin/jq -er \
      --argjson pr "$frontend_pr_number" --arg head "$frontend_commit_sha" \
      --arg base "$protected_main_sha" --arg baseline_sha "$frontend_ruleset_baseline_sha" '
      select(.schema == "arc.frontend-review-authorization.v1"
        and .pull_request == $pr and .head_commit == $head and .base_commit == $base
        and .main_ruleset_id == 21689753
        and .main_ruleset_baseline_sha256 == $baseline_sha
        and (.mode == "arisarcmarket-approval"
          or .mode == "temporary-ruleset-exception")) | .mode' \
      "$frontend_review_authorization")"
    case "$frontend_review_mode" in
      arisarcmarket-approval)
        test "$(printf '%s' "$frontend_pr_view" | /usr/bin/jq -r .reviewDecision)" = APPROVED
        test "$frontend_aris_approval_count" -ge 1
        ;;
      temporary-ruleset-exception)
        frontend_ruleset_exception_active=true
        frontend_main_ruleset_put "$frontend_ruleset_exception"
        test "$(frontend_main_ruleset_policy)" = "$frontend_ruleset_exception"
        ;;
      *) exit 1 ;;
    esac
    test "$("$ARC_RECOVERY_GH_PATH" api repos/FerrumVir/arc-chain/branches/main \
      --jq .commit.sha)" = "$protected_main_sha"
    "$ARC_RECOVERY_GH_PATH" api --method PUT \
      "repos/FerrumVir/arc-chain/pulls/$frontend_pr_number/merge" \
      -f commit_title="$frontend_pr_title" -f merge_method=squash \
      -f sha="$frontend_commit_sha" \
      | /usr/bin/jq -e '.merged == true and (.sha | test("^[0-9a-f]{40}$"))' >/dev/null
    restore_frontend_main_ruleset
    ;;
  closed)
    printf '%s' "$frontend_prs" | /usr/bin/jq -e '.[0].merged_at != null' >/dev/null
    test -f "$frontend_review_authorization" && test ! -L "$frontend_review_authorization"
    frontend_review_mode="$(/usr/bin/jq -er \
      --argjson pr "$frontend_pr_number" --arg head "$frontend_commit_sha" \
      --arg base "$protected_main_sha" --arg baseline_sha "$frontend_ruleset_baseline_sha" '
      select(.schema == "arc.frontend-review-authorization.v1"
        and .pull_request == $pr and .head_commit == $head and .base_commit == $base
        and .main_ruleset_id == 21689753
        and .main_ruleset_baseline_sha256 == $baseline_sha
        and (.mode == "arisarcmarket-approval"
          or .mode == "temporary-ruleset-exception")) | .mode' \
      "$frontend_review_authorization")"
    ;;
  *) exit 1 ;;
esac
restore_frontend_main_ruleset
trap - EXIT HUP INT TERM
frontend_pr_live="$("$ARC_RECOVERY_GH_PATH" api \
  "repos/FerrumVir/arc-chain/pulls/$frontend_pr_number")"
printf '%s' "$frontend_pr_live" | /usr/bin/jq -e --arg head "$frontend_commit_sha" '
  .state == "closed" and .merged == true and .head.sha == $head
  and .base.ref == "main" and (.merge_commit_sha | test("^[0-9a-f]{40}$"))' >/dev/null
frontend_main_sha="$(printf '%s' "$frontend_pr_live" | /usr/bin/jq -er .merge_commit_sha)"
arc_git -C "$operator_checkout" fetch --no-tags origin "$frontend_main_sha"
test "$(arc_git -C "$operator_checkout" rev-list --parents -n 1 \
  "$frontend_main_sha")" = "$frontend_main_sha $protected_main_sha"
test "$(arc_git -C "$operator_checkout" rev-parse "$frontend_main_sha^{tree}")" = \
  "$frontend_tree_sha"
test "$(arc_git -C "$operator_checkout" diff --name-only \
  "$protected_main_sha" "$frontend_main_sha")" = shared/frontend/arc-network.json
arc_git -C "$operator_checkout" show \
  "$frontend_main_sha:shared/frontend/arc-network.json" \
  | /usr/bin/cmp -s - "$frontend_config"
test "$("$ARC_RECOVERY_GH_PATH" api repos/FerrumVir/arc-chain/branches/main \
  --jq .commit.sha)" = "$frontend_main_sha"
test "$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/git/ref/tags/v0.8.0 --jq .object.sha)" = \
  "$protected_main_sha"
rulesets_after_frontend="$(repository_ruleset_snapshot)"
test "$rulesets_after_frontend" = "$rulesets_before_frontend"
test "$(frontend_main_ruleset_policy)" = "$frontend_ruleset_baseline"
frontend_ruleset_exception_used=false
if [ "$frontend_review_mode" = temporary-ruleset-exception ]; then
  frontend_ruleset_exception_used=true
fi
/usr/bin/jq -cnS --arg branch "$frontend_branch" --arg commit "$frontend_commit_sha" \
  --argjson pr "$frontend_pr_number" --arg merge "$frontend_main_sha" \
  --arg source "$protected_main_sha" --arg config_sha "$(arc_sha256 "$frontend_config")" \
  --arg review_mode "$frontend_review_mode" \
  --argjson ruleset_exception_used "$frontend_ruleset_exception_used" \
  --arg ruleset_baseline_sha "$frontend_ruleset_baseline_sha" \
  '{schema:"arc.frontend-merge-receipt.v1",branch:$branch,source_commit:$source,
    frontend_commit:$commit,pull_request:$pr,merge_commit:$merge,
    merge_method:"squash",review_authorization:$review_mode,
    temporary_ruleset_exception_used:$ruleset_exception_used,
    main_ruleset_baseline_sha256:$ruleset_baseline_sha,
    config_sha256:$config_sha,changed_paths:["shared/frontend/arc-network.json"]}' \
  | write_once_or_compare "$release_control_root/FRONTEND-MERGE.json"
```

Rollback, if a later Pages or live gate fails, is a new reviewed PR that reverts
only this config change and then repeats the same Pages run and deployed-byte
proof. It restores maintenance without moving `v0.8.0`, rolling back or
renumbering canonical blocks, or deleting any archive. A fork view is always
explicitly selected and provenance-verified; it is never canonical or a reward
source.

### Prove the exact Pages deployment and every advertised archive provenance

The merge uniquely triggers `deploy-explorer.yml`. Select its `push` run by
workflow/path/event/main/SHA, never by the most recent run. Reuse one exact run
on resume, wait for terminal success, require both named jobs, then bind one
successful `github-pages` deployment to that merge. The CDN check polls only
for convergence and accepts only the exact merged config bytes and
`deployed-commit.txt`; it also verifies those bytes against the deployed site
manifest. Every advertised legacy-fork provenance URL is fetched again and
compared field-for-field with its immutable config commitments.

```bash
post_release_attempt_root="$(
  /usr/bin/mktemp -d "$release_control_root/post-release.XXXXXXXX"
)"
test "$(/usr/bin/stat --format='%a:%h' "$post_release_attempt_root")" = 700:1
pages_workflow_id="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/actions/workflows/deploy-explorer.yml --jq .id)"
[[ "$pages_workflow_id" =~ ^[1-9][0-9]*$ ]]
pages_run_candidates() {
  "$ARC_RECOVERY_GH_PATH" api --paginate \
    'repos/FerrumVir/arc-chain/actions/workflows/deploy-explorer.yml/runs?event=push&branch=main&per_page=100' \
    --jq '.workflow_runs[]' \
  | /usr/bin/jq -cs --arg sha "$frontend_main_sha" \
      --argjson workflow_id "$pages_workflow_id" '
      [.[] | select(.workflow_id == $workflow_id and .head_sha == $sha
        and .head_branch == "main"
        and .path == ".github/workflows/deploy-explorer.yml"
        and .event == "push")]'
}
pages_runs='[]'
for _ in {1..30}; do
  pages_runs="$(pages_run_candidates)"
  pages_run_count="$(printf '%s' "$pages_runs" | /usr/bin/jq -er length)"
  [ "$pages_run_count" -eq 0 ] || break
  /usr/bin/sleep 2
done
test "$pages_run_count" -eq 1
pages_run_id="$(printf '%s' "$pages_runs" | /usr/bin/jq -er '.[0].id')"
pages_run_attempt="$(printf '%s' "$pages_runs" | /usr/bin/jq -er '.[0].run_attempt')"
[[ "$pages_run_id" =~ ^[1-9][0-9]*$ ]]
[[ "$pages_run_attempt" =~ ^[1-9][0-9]*$ ]]
/usr/bin/jq -cnS --argjson id "$pages_run_id" --argjson attempt "$pages_run_attempt" \
  --arg sha "$frontend_main_sha" --argjson workflow_id "$pages_workflow_id" \
  '{schema:"arc.pages-run-selection.v1",repository:"FerrumVir/arc-chain",
    workflow_id:$workflow_id,workflow_path:".github/workflows/deploy-explorer.yml",
    event:"push",head_branch:"main",head_sha:$sha,run_id:$id,run_attempt:$attempt}' \
  | write_once_or_compare "$release_control_root/PAGES-RUN-SELECTION.json"
"$ARC_RECOVERY_GH_PATH" run watch "$pages_run_id" \
  --repo FerrumVir/arc-chain --interval 10 --exit-status
pages_run_live="$("$ARC_RECOVERY_GH_PATH" api \
  "repos/FerrumVir/arc-chain/actions/runs/$pages_run_id")"
printf '%s' "$pages_run_live" | /usr/bin/jq -e \
  --argjson id "$pages_run_id" --argjson attempt "$pages_run_attempt" \
  --argjson workflow_id "$pages_workflow_id" --arg sha "$frontend_main_sha" '
  .id == $id and .run_attempt == $attempt and .workflow_id == $workflow_id
  and .head_repository.full_name == "FerrumVir/arc-chain"
  and .head_sha == $sha and .head_branch == "main"
  and .path == ".github/workflows/deploy-explorer.yml" and .event == "push"
  and .status == "completed" and .conclusion == "success"' >/dev/null
pages_jobs_live="$("$ARC_RECOVERY_GH_PATH" api --paginate \
  "repos/FerrumVir/arc-chain/actions/runs/$pages_run_id/jobs?filter=all&per_page=100" \
  --jq '.jobs[]' | /usr/bin/jq -cs 'sort_by(.name)')"
printf '%s' "$pages_jobs_live" | /usr/bin/jq -e '
  length == 2
  and ([.[] | select(.name == "Verify and assemble public console"
    and .status == "completed" and .conclusion == "success")] | length) == 1
  and ([.[] | select(.name == "Publish GitHub Pages"
    and .status == "completed" and .conclusion == "success")] | length) == 1' >/dev/null
printf '%s' "$pages_run_live" > "$post_release_attempt_root/pages-run.json"
printf '%s' "$pages_jobs_live" > "$post_release_attempt_root/pages-jobs.json"

pages_api_live="$("$ARC_RECOVERY_GH_PATH" api repos/FerrumVir/arc-chain/pages)"
printf '%s' "$pages_api_live" | /usr/bin/jq -e '
  .build_type == "workflow"
  and (.html_url | test("^https://[A-Za-z0-9.-]+(?:/[A-Za-z0-9._~/-]*)?/$"))' >/dev/null
pages_url="$(printf '%s' "$pages_api_live" | /usr/bin/jq -er '.html_url | rtrimstr("/")')"
pages_deployments="$("$ARC_RECOVERY_GH_PATH" api --method GET \
  repos/FerrumVir/arc-chain/deployments -f sha="$frontend_main_sha" \
  -f environment=github-pages -f per_page=100 \
  | /usr/bin/jq -c --arg sha "$frontend_main_sha" '
      [.[] | select(.sha == $sha and .ref == "main"
        and .environment == "github-pages" and .task == "deploy")]')"
test "$(printf '%s' "$pages_deployments" | /usr/bin/jq -er length)" -eq 1
pages_deployment_id="$(printf '%s' "$pages_deployments" | /usr/bin/jq -er '.[0].id')"
[[ "$pages_deployment_id" =~ ^[1-9][0-9]*$ ]]
pages_deployment_statuses="$("$ARC_RECOVERY_GH_PATH" api \
  "repos/FerrumVir/arc-chain/deployments/$pages_deployment_id/statuses?per_page=100")"
printf '%s' "$pages_deployment_statuses" | /usr/bin/jq -e \
  --arg url "$pages_url" '
  [.[] | select(.state == "success" and .environment == "github-pages"
    and ((.environment_url | rtrimstr("/")) == $url))] | length == 1' >/dev/null
printf '%s' "$pages_api_live" > "$post_release_attempt_root/pages-api.json"
printf '%s' "$pages_deployments" > "$post_release_attempt_root/pages-deployments.json"
printf '%s' "$pages_deployment_statuses" > "$post_release_attempt_root/pages-statuses.json"

deployed_config="$post_release_attempt_root/arc-network.json"
deployed_commit="$post_release_attempt_root/deployed-commit.txt"
deployed_sums="$post_release_attempt_root/SHA256SUMS"
pages_bytes_match=false
for _ in {1..12}; do
  /usr/bin/curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' --tlsv1.2 --max-time 30 \
    --max-filesize 4194304 \
    "$pages_url/shared/frontend/arc-network.json?commit=$frontend_main_sha" \
    -o "$deployed_config.tmp"
  /usr/bin/curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' --tlsv1.2 --max-time 30 \
    --max-filesize 1024 \
    "$pages_url/deployed-commit.txt?commit=$frontend_main_sha" \
    -o "$deployed_commit.tmp"
  if /usr/bin/cmp -s "$deployed_config.tmp" "$frontend_config" \
     && test "$(/usr/bin/tr -d '\r\n' < "$deployed_commit.tmp")" = "$frontend_main_sha"; then
    /usr/bin/mv -- "$deployed_config.tmp" "$deployed_config"
    /usr/bin/mv -- "$deployed_commit.tmp" "$deployed_commit"
    pages_bytes_match=true
    break
  fi
  /usr/bin/rm -f -- "$deployed_config.tmp" "$deployed_commit.tmp"
  /usr/bin/sleep 5
done
test "$pages_bytes_match" = true
/usr/bin/curl --fail --silent --show-error --location \
  --proto '=https' --proto-redir '=https' --tlsv1.2 --max-time 30 \
  --max-filesize 1048576 "$pages_url/SHA256SUMS?commit=$frontend_main_sha" \
  -o "$deployed_sums"
test "$(/usr/bin/awk '$2 == "./shared/frontend/arc-network.json" {print $1}' \
  "$deployed_sums")" = "$(arc_sha256 "$frontend_config")"
test "$(/usr/bin/awk '$2 == "./deployed-commit.txt" {print $1}' \
  "$deployed_sums")" = "$(arc_sha256 "$deployed_commit")"
/usr/bin/chmod 0400 "$deployed_config" "$deployed_commit" "$deployed_sums" \
  "$post_release_attempt_root"/*.json

legacy_provenance_count=0
while IFS= read -r archive_source; do
  legacy_provenance_count=$((legacy_provenance_count + 1))
  archive_node="$(printf '%s' "$archive_source" | /usr/bin/jq -er '.archive.node')"
  [[ "$archive_node" =~ ^[a-z0-9][a-z0-9-]{0,62}$ ]]
  archive_url="$(printf '%s' "$archive_source" | /usr/bin/jq -er \
    '(.baseUrl | rtrimstr("/")) + .archive.provenancePath')"
  archive_response="$post_release_attempt_root/provenance-$archive_node.json"
  /usr/bin/curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' --tlsv1.2 --max-time 30 \
    --max-filesize 1048576 "$archive_url" -o "$archive_response"
  printf '%s' "$archive_source" | /usr/bin/jq -e \
    --slurpfile actual "$archive_response" '
    .archive as $a | $actual[0] == {
      schema:"arc.legacy-archive.query.v1",read_only:true,
      classification:$a.classification,capture_id:$a.captureId,node:$a.node,
      rollout_manifest_sha256:$a.rolloutManifestSha256,
      archive_manifest_sha256:$a.archiveManifestSha256,
      complete_sha256:$a.completeSha256,bundle_sha256:$a.bundleSha256,
      inventory_sha256:$a.inventorySha256,
      binding_index_sha256:$a.bindingIndexSha256,binding_sha256:$a.bindingSha256,
      checkpoint_sha256:$a.checkpointSha256,
      checkpoint_manifest_hash:$a.checkpointManifestHash,
      checkpoint_payload_hash:$a.checkpointPayloadHash,
      canonical_checkpoint_height:$a.canonicalCheckpointHeight,
      source_height:$a.sourceHeight,source_block_hash:$a.sourceBlockHash,
      source_state_root:$a.sourceStateRoot}' >/dev/null
  /usr/bin/chmod 0400 "$archive_response"
done < <(/usr/bin/jq -c '.sources[] | select(.kind == "legacy-fork")' "$deployed_config")
test "$legacy_provenance_count" = \
  "$(/usr/bin/jq '[.sources[] | select(.kind == "legacy-fork")] | length' \
    "$deployed_config")"
```

### Run published installer/update and live inference/reward acceptance gates

Use a fresh, no-service install root on the reviewed Linux x86_64 operator.
The bootstrap is downloaded from the exact immutable release and matched to
the API digest already sealed above. A partial canary root without its final
receipt is preserved and stops; a completed exact root is read-only verified
and reused. The update-only pass must resolve the public channel back to
v0.8.0 and report equality without replacement. Finally, rerun the rollout's
read-only live verifier against the immutable two-canary reward evidence. This
does not issue a third reward: it re-proves all-six convergence, inference
attestations, the two mined `0x25` receipts, exact 5 ARC delta, and honest
projection state from the existing evidence.

```bash
installer_canary_root="$release_control_root/installer-canary-linux-x86_64"
installer_canary_receipt="$installer_canary_root/ACCEPTED.json"
release_api_canary="$("$ARC_RECOVERY_GH_PATH" api \
  repos/FerrumVir/arc-chain/releases/tags/v0.8.0)"
printf '%s' "$release_api_canary" | /usr/bin/jq -e \
  --argjson release_id "$release_id" --arg sha "$protected_main_sha" '
  .id == $release_id and .tag_name == "v0.8.0" and .target_commitish == $sha
  and .draft == false and .prerelease == false and .immutable == true' >/dev/null
installer_expected_digest="$(printf '%s' "$release_api_canary" | /usr/bin/jq -er \
  '.assets[] | select(.name == "install.sh") | .digest | sub("^sha256:"; "")')"
node_expected_digest="$(printf '%s' "$release_api_canary" | /usr/bin/jq -er \
  '.assets[] | select(.name == "arc-node-linux-x86_64") | .digest | sub("^sha256:"; "")')"
cli_expected_digest="$(printf '%s' "$release_api_canary" | /usr/bin/jq -er \
  '.assets[] | select(.name == "arc-cli-linux-x86_64") | .digest | sub("^sha256:"; "")')"
[[ "$installer_expected_digest" =~ ^[0-9a-f]{64}$ ]]
[[ "$node_expected_digest" =~ ^[0-9a-f]{64}$ ]]
[[ "$cli_expected_digest" =~ ^[0-9a-f]{64}$ ]]
if [ -e "$installer_canary_root" ] && [ ! -f "$installer_canary_receipt" ]; then
  printf 'partial installer canary exists without acceptance receipt; preserve and stop\n' >&2
  exit 1
fi
if [ ! -e "$installer_canary_root" ]; then
  /usr/bin/mkdir -m 0700 "$installer_canary_root"
  /usr/bin/curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' --tlsv1.2 --max-time 60 \
    --max-filesize 4194304 \
    https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/install.sh \
    -o "$installer_canary_root/install.sh"
  test "$(arc_sha256 "$installer_canary_root/install.sh")" = \
    "$installer_expected_digest"
  /usr/bin/chmod 0500 "$installer_canary_root/install.sh"
  /usr/bin/bash "$installer_canary_root/install.sh" --version 0.8.0 \
    --install-dir "$installer_canary_root/install" \
    --data-dir "$installer_canary_root/data" \
    --no-service --no-auto-update \
    >"$installer_canary_root/install.stdout" \
    2>"$installer_canary_root/install.stderr"
  "$installer_canary_root/install/bin/arc-node" --version \
    | /usr/bin/grep -F ' 0.8.0'
  "$installer_canary_root/install/bin/arc-cli" --version \
    | /usr/bin/grep -F ' 0.8.0'
  /usr/bin/bash "$installer_canary_root/install.sh" --update-only \
    --install-dir "$installer_canary_root/install" --no-service --no-auto-update \
    >"$installer_canary_root/update.stdout" \
    2>"$installer_canary_root/update.stderr"
  /usr/bin/grep -Fq 'Already up to date at v0.8.0' \
    "$installer_canary_root/update.stdout"
  test "$(arc_sha256 "$installer_canary_root/install/bin/arc-node")" = \
    "$node_expected_digest"
  test "$(arc_sha256 "$installer_canary_root/install/bin/arc-cli")" = \
    "$cli_expected_digest"
  /usr/bin/jq -cnS --argjson release_run_id "$release_run_id" \
    --argjson release_run_attempt "$release_run_attempt" --argjson release_id "$release_id" \
    --arg node_sha "$node_expected_digest" --arg cli_sha "$cli_expected_digest" \
    --arg update_sha "$(arc_sha256 "$installer_canary_root/update.stdout")" \
    '{schema:"arc.post-release-installer-canary.v1",version:"0.8.0",
      platform:"linux-x86_64",release_run_id:$release_run_id,
      release_run_attempt:$release_run_attempt,release_id:$release_id,
      node_sha256:$node_sha,cli_sha256:$cli_sha,update_stdout_sha256:$update_sha,
      service_started:false,update_result:"already-up-to-date"}' \
    | write_once_or_compare "$installer_canary_receipt"
fi
test "$(/usr/bin/stat --format='%a:%h' "$installer_canary_root")" = 700:1
test "$(/usr/bin/stat --format='%a:%h' "$installer_canary_receipt")" = 400:1
test ! -L "$installer_canary_receipt"
test "$(arc_sha256 "$installer_canary_root/install.sh")" = "$installer_expected_digest"
test "$(arc_sha256 "$installer_canary_root/install/bin/arc-node")" = \
  "$node_expected_digest"
test "$(arc_sha256 "$installer_canary_root/install/bin/arc-cli")" = \
  "$cli_expected_digest"
/usr/bin/jq -e --argjson release_run_id "$release_run_id" \
  --argjson release_run_attempt "$release_run_attempt" --argjson release_id "$release_id" \
  --arg node_sha "$node_expected_digest" --arg cli_sha "$cli_expected_digest" \
  --arg update_sha "$(arc_sha256 "$installer_canary_root/update.stdout")" '
  . == {schema:"arc.post-release-installer-canary.v1",version:"0.8.0",
    platform:"linux-x86_64",release_run_id:$release_run_id,
    release_run_attempt:$release_run_attempt,release_id:$release_id,
    node_sha256:$node_sha,cli_sha256:$cli_sha,update_stdout_sha256:$update_sha,
    service_started:false,update_result:"already-up-to-date"}' \
  "$installer_canary_receipt" >/dev/null
/usr/bin/grep -Fq 'Already up to date at v0.8.0' \
  "$installer_canary_root/update.stdout"
"$installer_canary_root/install/bin/arc-node" --version | /usr/bin/grep -F ' 0.8.0'
"$installer_canary_root/install/bin/arc-cli" --version | /usr/bin/grep -F ' 0.8.0'

live_acceptance="$post_release_attempt_root/live-rollout-verify.txt"
"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/recovery_rollout.py verify \
  --manifest "$final_manifest" --reward-evidence "$reward_evidence" \
  >"$live_acceptance"
test -s "$live_acceptance"
/usr/bin/chmod 0400 "$live_acceptance"
(cd /secure/operator && \
  /usr/bin/sha256sum --check --strict recovery-v3.reward-evidence.json.sha256)

acceptance_receipt="$post_release_attempt_root/POST-RELEASE-ACCEPTANCE.json"
/usr/bin/jq -cnS --arg source_sha "$protected_main_sha" \
  --argjson release_run_id "$release_run_id" \
  --argjson release_run_attempt "$release_run_attempt" --argjson release_id "$release_id" \
  --arg frontend_commit "$frontend_commit_sha" --argjson frontend_pr "$frontend_pr_number" \
  --arg frontend_main "$frontend_main_sha" --argjson pages_run_id "$pages_run_id" \
  --argjson pages_run_attempt "$pages_run_attempt" \
  --argjson pages_deployment_id "$pages_deployment_id" \
  --arg config_sha "$(arc_sha256 "$deployed_config")" \
  --arg installer_receipt_sha "$(arc_sha256 "$installer_canary_receipt")" \
  --arg reward_evidence_sha "$(arc_sha256 "$reward_evidence")" \
  --arg live_verify_sha "$(arc_sha256 "$live_acceptance")" '
  {schema:"arc.post-release-acceptance.v1",repository:"FerrumVir/arc-chain",
   release_source_sha:$source_sha,release_run_id:$release_run_id,
   release_run_attempt:$release_run_attempt,release_id:$release_id,
   frontend_commit:$frontend_commit,frontend_pull_request:$frontend_pr,
   frontend_main_sha:$frontend_main,pages_run_id:$pages_run_id,
   pages_run_attempt:$pages_run_attempt,pages_deployment_id:$pages_deployment_id,
   deployed_config_sha256:$config_sha,installer_receipt_sha256:$installer_receipt_sha,
   reward_evidence_sha256:$reward_evidence_sha,live_verify_sha256:$live_verify_sha,
   release_immutable:true,pages_jobs_succeeded:true,provenance_verified:true,
   installer_update_verified:true,inference_reward_evidence_verified:true}' \
  > "$acceptance_receipt"
/usr/bin/chmod 0400 "$acceptance_receipt"
/usr/bin/sync -f "$acceptance_receipt"
/usr/bin/sync -f "$post_release_attempt_root"
```

Only now may the operator restore the normal package-update schedule:

```bash
/usr/bin/systemctl unmask --runtime "${operator_package_units[@]}"
/usr/bin/systemctl reset-failed "${operator_package_units[@]}"
/usr/bin/systemctl start apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service
for unit in apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service; do
  test "$(/usr/bin/systemctl show --value --property=LoadState "$unit")" = loaded
  test "$(/usr/bin/systemctl show --value --property=ActiveState "$unit")" = active
  test "$(/usr/bin/systemctl show --value --property=UnitFileState "$unit")" = enabled
done
```

<!-- END EXECUTABLE PRODUCTION RECOVERY PROCEDURE -->

If the enclave reboots before that point, the runtime masks disappear by
design. Do not recreate them and continue from remembered values: rerun every
preceding read-only package-version, file-hash, artifact, and remote-state
proof first.

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

For the production GO gate, the manifest validator requires the staged,
SHA-pinned repository probe with the exact one-token argv; policy-only mode,
fixed receipts, a foreign probe path/hash, or a different token count is
rejected before plan/GO. The probe first requires an issuance-ready validator that sees an eligible
full-model worker, submits one real one-token `/inference/run`, and refuses to
emit evidence unless the response proves community routing, the canonical
per-row INT8 execution profile, authenticated 2-of-3 verification for every
range/position, five validator approvals, and a pending `0x25` transaction:

```bash
probe="$PWD/scripts/recovery/community-reward-probe.py"
probe_sha256="$(/usr/bin/sha256sum "$probe" | /usr/bin/awk '{print $1}')"
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
sealed HTTPS origins. `/workers/scoreboard` is the public read-only
dashboard/probe inventory; the side-effecting compatibility handler
`/community/list` is not exposed by the v3 gateway. Shard forwarding and reward
approval stay validator-IP-only.

The probe receives only these non-secret environment values:
`ARC_RECOVERY_RPC_URLS`, `ARC_RECOVERY_ROLLOUT_MANIFEST_SHA256`, and
`ARC_RECOVERY_CHECKPOINT_MANIFEST_HASH`. It deterministically seals one of the
six coordinators for the rollout and never fails over to another coordinator.
The namespaced probe identity is committed as the signed assignment epoch and
has a consensus replay marker, so a retry rediscovers the same job/transaction
across client or coordinator restarts and cannot pay through another
coordinator. Before invoking the probe, the rollout queries all six earnings
indexes and fsyncs the complete canonical all-v3 history for every potentially
eligible worker visible to that sealed coordinator (fixed-evidence mode has one
known worker). The actual worker must belong to that pre-canary set. The
reserved checksum file is a canonical, fsynced baseline-plus-0/1/2 progress
journal until final evidence promotion; crashes after the baseline, either receipt, or during
the earnings/projection-state check therefore re-prove GET-only state without
issuing another reward. Immediately before ordinal one, including after a
baseline-only crash resume, the harness re-queries the selectable-worker set
and all six earnings indexes: a new selectable worker or any changed baseline
row aborts before the probe can issue a reward.
A pending or failed transaction never passes. Both jobs use a real one-token request, but the first must reach
`mined_success` on all six before the second is submitted. The receipts must
land at two distinct heights (different hashes at one height are a fork, not
two blocks), each carry at least five approvals, and reconcile on every
`/worker/earnings/{worker}` response to exactly 2,500,000,000 base units / 2.5
ARC apiece. Every pre-canary baseline row—including its block hash, transaction
index, recovery epoch, validator set, and transaction domain—must remain
byte-for-byte canonical;
the post-canary count must be baseline count + 2 and gross must be baseline
gross + 5 ARC, with no third new row. For an empty baseline, exactly two
immediate receipts are not a rate sample: `attestations_per_day_observed` and
`projected_daily_arc` must both remain null, and both unavailable reasons must exactly say
`collecting data: a projection needs at least 3 successful mined reward receipts spanning at least 24 hours, not the initial one or two rollout canaries`. A numeric rate or
forecast at that boundary fails closed. With a nonempty baseline, the full
all-v3 history controls projection truth: fewer than 24 hours produces the
canonical short-window null reason; a valid 24-hour-or-longer window must expose
the exact timestamp-derived rate; and a numeric forecast, when issuance and
budget state permit one, must equal that rate times 2.5 ARC. Otherwise the
forecast stays null with a nonempty reason. Counts, local observations,
configured rates, and pending submissions are not earnings. Frontend
publication may proceed with an honest null; it must not wait for or manufacture
a forecast.

Before any receipt-mode plan or execution, choose the create-only output that
will carry the two proven identities. The execute command printed by plan mode
preserves this argument:

```bash
--reward-evidence-output /secure/operator/recovery-v3.reward-evidence.json
```

The rollout writes that file and its `.sha256` sidecar mode `0444` only after
both receipts and the six-node baseline-retention/delta/projection contract
pass. Its JSON includes `schema: arc.recovery.reward-evidence.v2`, the exact
rollout SHA-256, both canary identities, and the selected worker's complete
pre-canary earnings baseline. It is never reconstructed from chat output and
is never overwritten.

A later read-only audit can use externally captured evidence:

```bash
python3 scripts/recovery/recovery_rollout.py verify \
  --manifest /secure/operator/arc-recovery-final.lock.json \
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
