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
authorize production. ARC's executable ceiling is 700000000000 bytes (700 GB decimal),
leaving 50000000000 bytes (50 GB) below Google's 750000000000-byte
per-24-hour upload cap documented in the
[official Drive API limits](https://developers.google.com/workspace/drive/api/guides/limits).
The operator must attest that this is the independently checked remaining
budget and that ARC is the account's only uploader for the quota window. Run
`capture` in plan mode first. Execution
requires exactly
`ARC_RECOVERY_FREEZE_GO="FREEZE <freeze-plan-sha256> CAPTURE <capture-id>"`;
the capture ID is deterministically derived from the freeze-plan digest.

Plan mode runs the Drive identity/capacity gate read-only and accepts only the
reviewed rclone v1.75.0, whose version is recorded in the receipt. The gate
first uses a benign `rclone about` to refresh the selected OAuth configuration,
then streams `rclone config show <selected-remote>` only into a hash-pinned,
isolated helper. It does not use backend-dependent `rclone config userinfo`.
From that same in-memory decrypted stream the helper requires and hashes the
unredacted custom client ID, rejects a missing/redacted client or client secret,
then performs the bounded verified-TLS Drive v3 request
`GET /drive/v3/about?fields=user(emailAddress,permissionId,me)`, requires
`me=true` plus exactly one normalized email and permission ID, and releases
only the client/account/permission hashes. No token, client secret, raw client
ID, raw account field, or raw API body is placed in argv, environment, logs,
durable temporary storage, or receipts. Real rclone v1.75.0 redacts `client_id`
as well as `client_secret` in `config redacted`, so that command is not an
identity source and the production gate never calls it.

After the exact authorization, execute mode repeats the gate immediately
before the first writer signal and must immutable-create, download and
hash-verify, permanently delete, and prove absence of one unique 8 MiB root
canary. It then repeats the rclone-version, ARC OAuth client, account,
permission identity, and capacity checks before persisting an
`arc.recovery.drive-prefreeze.v1` receipt. Any rclone warning or mismatch,
including a client, account, or permission-ID switch around the canary, is a
hard stop.

Immediately after the verified canary and before the first cgroup freeze,
restart-fence commit, or signal, execute mode takes exactly three bounded
read-only observations from every sealed writer's loopback origin:
`/inference/results`, `/workers/scoreboard`, and
`/inference/attestations`. It must never request `/community/list`. Each GET
has a 20-second deadline and an 8 MiB captured-body ceiling. HTTP 404,
unreachable, timeout, and oversize outcomes are recorded faithfully with UTC
time, node, endpoint, raw captured byte count, and SHA-256 rather than treated
as canonical state or reward evidence.

Each durable request intent is one-way: a crash/resume records an interrupted
attempt instead of issuing the GET again. A complete receipt is reused exactly;
if any receipt is missing, a fleet-wide eligibility barrier requires all six
writers to remain live and unfenced. After any writer's fence/stop begins, no
missing receipt may be captured or recaptured.
All six create-only `arc.recovery.legacy-live-observations.v1` trees must be
fsynced and reverified before any signal. They are labelled `diagnostic`,
`noncanonical`, and `nonreward`; endpoint failures do not invent data, but
failure to durably write a receipt aborts before the freeze.

Those diagnostic observations are not the recovery source. Before each live
target's first restart-effective dependency or nft mutation, the quarantine
round creates a role-tagged `preauthorization-boundary` source pair. A
production data directory and its sibling may contain no snapshot; operators
must not wait until after stop or assume a pre-existing snapshot. The helper
proves that the loopback `/sync/snapshot` listener belongs to the exact sealed
writer PID/boot/start/executable/argv/cgroup, journals every request in an
immutable attempt, copies and hashes the snapshot-selected fixed WAL prefix,
allows only an append-only source suffix, and repeats until the pinned offline
loader strictly replays the pair and proves the capture head plus ancestry
bounds. A receipt fsynced before its selector is recovered without issuing a
second request. Round authorization heights cannot exceed that captured head.

After a node crosses quarantine and two stability samples agree, it takes a
second pair tagged `post-quarantine-final-export` while public and
inter-validator ingress remain denied. The writer/listener identity must be
unchanged and the captured head must equal the stable quarantined tuple and
cover all authenticated bounds. This final role, receipt hash, and head bind
the stop intent, stop receipt, persisted head, and normal export. Only a
controlled persistently-stopped transition with no complete final receipt may
fall back to the initial role; its complete final WAL is preserved separately
and any later complete suffix is explicitly
`archived_noncanonical_post_capture_suffix`.

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
identity, stop evidence, and the exact live-observation tree/receipt hashes.
Each bundle retains that immutable observation tree. The final Drive archive
adds canonical `legacy-live-observations.json`, whose ordered six roots are
covered by the archive manifest and `SHA256SUMS` and rechecked during restore
verification. The original legacy data directory stays in place;
it is content-sealed by repeated hashing, not mounted read-only and not copied
into a second full local tree. Changed, missing, unexpected, cross-device,
symlink, or special-file content fails closed. The legacy `/sync/snapshot`
endpoint is never trusted alone as a state barrier; only its sealed-writer-
owned response paired to a fixed WAL prefix and accepted by strict offline
replay is eligible. No snapshot request occurs after stop.

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

### Restore and deliver the reviewed six-key vault

Use `scripts/release/restore-validator-vault.py`; do not run `openssl cms
-decrypt` or `tar -x` by hand. The `restore` subcommand requires the downloaded
CMS ciphertext and its typed SHA-256, canonical rewrap receipt, exact
protected-main SHA, matching mode-`0600` RSA restore certificate/private key,
and the raw exact-ID Linux x86_64 headless Actions ZIP. A fresh anonymous
public GitHub REST proof must bind the current protected `main`, exact
completed/successful preflight run and attempt, and exact unexpired artifact
ID/name/server digest/size. The helper then privately derives the exact
`arc-cli`, `BUILD-METADATA.json`, and complete genesis from the two verified
archive layers; caller-selected unpacked files or local receipts never
authorize a restore. The output path must be a new absolute directory; an
existing path is never merged or replaced. Before running this block, complete
the protected pre-tag selection/download/materialization block in the recovery
README; `PRETAG-SELECTION.json` and the raw Actions ZIP below are its verified
outputs.

```bash
set -Eeuo pipefail
umask 077
export PATH=/secure/operator/tools:/usr/bin:/bin
export ARC_RECOVERY_PYTHON_PATH=/usr/bin/python3.12
export ARC_RECOVERY_PYTHON_SHA256=1643dacd9feaedc58f3cc581e4d22577dfe25c09b10282936186ccf0f2e61118
test -f "$ARC_RECOVERY_PYTHON_PATH" && test ! -L "$ARC_RECOVERY_PYTHON_PATH"
printf '%s  %s\n' "$ARC_RECOVERY_PYTHON_SHA256" "$ARC_RECOVERY_PYTHON_PATH" \
  | /usr/bin/sha256sum --check --strict
export ARC_PROOF_CURL='/usr/bin/curl'
export ARC_PROOF_CA_BUNDLE='/etc/ssl/certs/ca-certificates.crt'
export ARC_PROOF_CURL_SHA256=74b4ce8f74b377f18ef1b3df7279c26cb3cd14c49e39ab1498575b209dc3f70f
export ARC_PROOF_CA_BUNDLE_SHA256=ecd9dc38bc3efb7dbd6431f57e29d2f8d6a0f0d211e1464b3fef2cbfe266fcd2
printf '%s  %s\n' "$ARC_PROOF_CURL_SHA256" "$ARC_PROOF_CURL" \
  | /usr/bin/sha256sum --check --strict
printf '%s  %s\n' "$ARC_PROOF_CA_BUNDLE_SHA256" "$ARC_PROOF_CA_BUNDLE" \
  | /usr/bin/sha256sum --check --strict

pretag_selection_json=/secure/operator/PRETAG-SELECTION.json
cms_path=/secure/operator/arc-validator-keys-v0.8.0.tar.cms
rewrap_receipt=/secure/operator/REWRAP-RECEIPT.json
test -f "$cms_path" && test ! -L "$cms_path"
test -f "$rewrap_receipt" && test ! -L "$rewrap_receipt"
cms_sha256="$(/usr/bin/jq -er '.cms_sha256' "$rewrap_receipt")"
protected_main_sha="$(/usr/bin/jq -er '.source_commit' "$rewrap_receipt")"
pretag_run_id="$(/usr/bin/jq -er '.run_id' "$pretag_selection_json")"
pretag_run_attempt="$(/usr/bin/jq -er '.run_attempt' "$pretag_selection_json")"
[[ "$cms_sha256" =~ ^[0-9a-f]{64}$ ]]
[[ "$protected_main_sha" =~ ^[0-9a-f]{40}$ ]]
[[ "$pretag_run_id" =~ ^[1-9][0-9]*$ ]]
[[ "$pretag_run_attempt" =~ ^[1-9][0-9]*$ ]]
printf '%s  %s\n' "$cms_sha256" "$cms_path" \
  | /usr/bin/sha256sum --check --strict
test "$(/usr/bin/git rev-parse HEAD)" = "$protected_main_sha"
pretag_linux_x86_64_artifact_id="$(
  /usr/bin/jq -er '.artifacts["linux-x86_64"].headless.id' \
    "$pretag_selection_json"
)"
[[ "$pretag_linux_x86_64_artifact_id" =~ ^[1-9][0-9]*$ ]]

"$ARC_RECOVERY_PYTHON_PATH" -I scripts/release/restore-validator-vault.py restore \
  --cms "$cms_path" \
  --expected-cms-sha256 "$cms_sha256" \
  --rewrap-receipt "$rewrap_receipt" \
  --source-main-sha "$protected_main_sha" \
  --raw-actions-zip /secure/pretag/raw-v0.8.0/headless-linux-x86_64/actions.zip \
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
```

Those are the reviewed Ubuntu x86_64 OpenSSL 3.0.13 executable/library
identities for this recovery. A different operating system or OpenSSL build requires a new
explicit path-and-hash review; neither `PATH` nor an unpinned loader dependency
is accepted. The helper copies all three into a private create-new runtime,
executes the copied binary directly with a sanitized environment, and proves
from every loader trace that the copied `libssl` and `libcrypto` were used.

The helper validates one RSA-OAEP-SHA256 recipient and AES-256-GCM content,
decrypts only below a mode-`0700` temporary directory, and extracts through
no-follow/create-new file descriptors. It rejects traversal, absolute or
Windows paths, links, special/PAX/sparse members, duplicate or case-folded
paths, non-private modes, and every size/count violation. Exactly six strict
Ed25519 JSON files must pass `arc keygen --verify-keyfile` under the hash-pinned
pre-tag CLI. Their derived addresses—not filenames or archive order—map to the
reviewed NYC/LAX/AMS/LHR/NRT/SGP trust root and must exactly match the complete
genesis addresses and stakes.

The new directory contains six mode-`0600` keyfiles, a mode-`0600` private
`RESTORE-RECEIPT.json`, and canonical `validator-public-keys.json`. The public
manifest is an array whose records contain only `address`, `public_key`, and
`stake`; the private receipt contains paths and hashes but never secret bytes.
Neither command emits private bytes on stdout, argv, or environment variables.

Key delivery is a separate post-freeze operation. A v5 freeze **plan is not
proof that a writer stopped**, and a caller-authored or merely self-hashed JSON
claim is never accepted. Use only the mode-`0400` canonical
`arc.validator-vault.offline-stop-evidence.v2` receipt and sidecar emitted by
`archive-fleet-to-drive.sh` after it re-runs the hash-pinned `stopped-status`
operation on all six `arc.recovery.offline-stop.v4` roots. It binds the exact
protected-main commit, freeze plan and sidecar, capture ID, remote-helper path
and SHA-256, and ordered stop-complete/files/status/argv roots for the fixed
NYC/LAX/AMS/LHR/NRT/SGP hosts.

The exact executable `install` block appears in section 5 immediately after
the capture that creates these artifacts. It is intentionally not adjacent to
`restore`: restoring private bytes does not authorize remote delivery.

The known-hosts source must be an operator-owned, single-link mode-`0400` file
with exactly six canonical, ordered literal-IP `ssh-ed25519` records for
NYC/LAX/AMS/LHR/NRT/SGP and six unique canonical key blobs. Wildcards, hashed
hosts, aliases, extra fields, reordered hosts, duplicate keys, or another key
algorithm fail closed. The explicit maintenance identity must also be one
operator-owned, single-link mode-`0400` file with its reviewed SHA-256.
Transport always uses `-F /dev/null`, exactly one `-i`, `IdentitiesOnly=yes`,
and `IdentityAgent=none`; it drops `SSH_AUTH_SOCK` and never consults an agent,
default identity, user SSH config, or global known-hosts file.

Install rejects any plan/evidence host mapping except NYC `149.28.32.76`, LAX
`140.82.16.112`, AMS `136.244.109.1`, LHR `104.238.171.11`, NRT
`202.182.107.41`, and SGP `149.28.153.31` before transport. It privately copies
the reviewed SSH/SCP images, sanitizes their environment and user config, and
forces SCP to use the same private SSH image. Before any key probe or upload it
re-runs the evidence-bound, hash-pinned remote helper on all six hosts and
requires byte-identical canonical stopped-status output and roots. It repeats
that same proof at each node's immediate install boundary.

Install freshly repeats the full GitHub artifact proof after all six remote
stopped-status checks and immediately before any remote key probe or mutation;
both complete initial/final public-API provenances are sealed in the receipt.
It uses batch-only, strict host-key-pinned SSH with forwarding and password
fallbacks disabled. Each already locally verified key is copied to a fresh
remote temporary inode, hash-checked, made `root:root` mode `0600`, fsynced,
and hard-linked create-only as `/etc/arc-v3/validator-key.json`. A matching
existing final file is an exact resume; any different bytes, type, owner, or
mode abort without overwrite. The final receipt contains only public
addresses, paths, states, and artifact/key/proof hashes.

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

```bash
immutable_enabled="$(gh api \
  repos/FerrumVir/arc-chain/immutable-releases \
  --jq '.enabled')"
test "$immutable_enabled" = true || {
  printf 'immutable GitHub releases are not enabled; do not create the tag\n' >&2
  exit 1
}
```

Run that command from the existing owner/admin `gh` session immediately before
tag creation. GitHub's endpoint requires repository Administration read access,
which is intentionally unavailable to the workflow `GITHUB_TOKEN`. The
least-privilege publisher instead creates a hidden draft, uploads and compares
every GitHub-computed asset digest to the local bytes, publishes the validated
draft, and requires the resulting release to report `immutable: true`. If it
does not, the workflow immediately deletes that exact release ID without
deleting the protected tag, so the same unchanged tag can be rerun after the
setting is repaired.

- protect `main` with a no-bypass PR/check/review ruleset. Protect **all** tag
  names with two `~ALL` tag rulesets: owner-only creation, plus no-bypass
  update, deletion, and non-fast-forward prevention. Enable immutable releases;
- restrict Actions to an owner-reviewed allowlist and require full commit-SHA
  pinning;
- keep Pages on the GitHub Actions source, restrict the `github-pages`
  environment to protected `main`, and disable administrator bypass. The Pages
  workflow runs on every `main` commit, cancels superseded runs, and re-resolves
  current `main` before artifact upload and immediately before deployment;
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
  `main` SHA and confirmation `BACKUP_EXISTING_RELEASE_KEYS`. Do not create the
  immutable tag yet. That one-shot
  workflow uses a fresh no-checkout job to create the encrypted archive,
  immediately restores and byte-compares both keys, uploads ciphertext only,
  and retains the temporary Actions artifact for one day. While either private
  key or `ARC_SIGNING_BACKUP_PASSPHRASE` exists, only workflow-inline logic and
  fixed system executables may run: no repository script, package lifecycle
  hook, interpreter-loaded repository module, or unpinned downloaded program
  is permitted. Before dispatch,
  disable administrator bypass and self-review on the `release` environment
  and require a distinct trusted reviewer. The artifact name binds protected
  `main`, run/attempt, and its ciphertext SHA-256. Download it into either a
  FileVault-protected directory or a dedicated AES-256 encrypted APFS image
  whose distinct unlock secret is held in macOS Keychain separately from the
  image, set the ciphertext mode 0600, and verify the mounted image reports
  `Encryption = AES-256` and `Properties.Encrypted = 1` before accepting the
  published exact-main/run/attempt identity, ciphertext SHA-256, and public-key
  fingerprints. An unencrypted host must never stage the artifact outside that mounted volume.
  Operators must not run a repository backup/restore script or
  expose either passphrase to a checkout. The protected job clears the passphrase,
  shreds both plaintext key files, and only then allows repository verification
  code to run against public outputs. Copy only the
  ciphertext to ARC Drive and a second independent recovery medium, then
  re-download and hash-match both copies. Do not delete the short-lived Actions
  artifact yet. Keep the passphrase environment secret in place while manually
  dispatching `release-signing-preflight.yml`: its `backup-readiness` job fails
  unless the 32+-character secret exists, the exact protected-main SHA has one
  successful backup artifact, that passphrase decrypts it, and both restored
  keys match the committed manifest and updater trust roots. After that job is
  green, delete the short-lived backup artifact; the two independently hash-matched
  ciphertext copies remain the recovery media. The same preflight binds every
  job to one dispatch-time main SHA; boots all five promised headless targets
  (Linux x86_64/ARM64, macOS Intel/ARM64, and Windows x86_64); assembles all
  four signed desktop groups; and rechecks unchanged protected main after the
  matrix, so every platform failure happens before a non-movable tag exists.
  Each matrix leg uploads a create-only 30-day candidate whose name binds kind,
  platform, candidate commit, workflow run/attempt, and inner archive SHA-256.
  The seal requires exactly nine unexpired, nonempty, unique artifact IDs with
  GitHub server SHA-256 digests. Use the Linux x86_64 payload from that selection
  to stage the six validators before updater users can see v0.8. These exact
  bytes are both rollout inputs and release assets: the tag workflow re-resolves
  the latest successful exact-commit preflight, pins its run/attempt and all nine
  artifact IDs, downloads raw ZIPs with digest mismatch set to error, independently
  re-hashes them against the selected server digests, safely extracts them, and
  publishes those bytes without an independent release rebuild.
  Before tagging, exercise the selected macOS arm64 headless node as the
  bounded stake-zero full-integer community worker in
  [MACOS-PRETAG-COMMUNITY-CANARY.md](MACOS-PRETAG-COMMUNITY-CANARY.md). The
  helper retains the already-verified build metadata, pins the exact
  commit/run/attempt and canonical GGUF, exposes RPC on loopback only, uses all
  six literal-IP HTTPS community origins, and stops only after exact
  PID/executable/argv proof with the full 4,420-second SIGTERM budget. Preserve
  its registration/work/receipt evidence; a running process alone is not a
  successful inference or reward canary.
  Only after every job in that exact-SHA preflight succeeds may the owner create
  `v0.8.0` at that SHA. Then delete only the temporary passphrase environment
  secret. Remove the one-shot workflow through a protected PR only after the
  immutable release is complete, because changing `main` before tagging would
  invalidate the exact-SHA backup and preflight evidence. Keep the passphrase
  outside every ciphertext provider. Never
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
It also refuses a superseded preflight run/attempt immediately before each
artifact download and again before immutable publication.
That one new release must contain the CLI/headless and desktop artifacts,
installer, updater manifest/signature, owner-signed `SHA256SUMS` plus
`SHA256SUMS.sig`, seeds, and genesis from the same commit and version. The
signed manifest header binds repository, tag, and commit. The publication gate
cryptographically verifies its signature and all
four updater payloads against the public key embedded in that exact commit.
Test Linux x86_64 in clean Ubuntu 22.04, 24.04, and 26.04 containers with
`DISPLAY` unset. Test Linux ARM64 in clean Ubuntu 24.04 and 26.04 environments
with `DISPLAY` unset. Test Intel macOS with the headless x86_64 artifact.
Confirm that update/install tests preserve node identity and roll back the
entire failed replacement.

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
earnings on all six. Production runtime argv force protected `--archive` mode
on every selectable public validator. After both canaries mine, the harness
restarts all six again one at a time and re-proves the exact receipts and
`archive_mode=true`, so a desktop fresh-host election cannot silently fall
back to a pruned earnings view.

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

The executable order is normative: complete protected pre-tag materialization
and vault `restore`; create-only materialize and verify
`/secure/operator/legacy-validator-set-40m.json` from the exact protected
checkout as shown in the recovery README; then prepare/freeze/capture; run
vault `install`; export and sign the checkpoint; build the prearchive; and only
then seal the Drive archive. The public key input is
`/secure/operator/arc-v0.8-validator-restore/validator-public-keys.json`, and
the materialized binary/genesis are under
`/secure/operator/pretag-materialized-v0.8.0/headless-linux-x86_64/`. Do not
substitute a hand-copied top-level file for any of those paths.

1. Announce a maintenance window and stop ordinary submissions.
2. Execute the separately sealed freeze plan with the exact `FREEZE
   <freeze-plan-sha256> CAPTURE <capture-id>` authorization. Before any stop,
   require the sealed ARC OAuth remote/root preflight receipt and the successful
   8 MiB write/read/hash/delete canary receipt. Persist and verify the systemd
   start fence, prior enablement evidence, and durable stop intent. Capture is
   a bounded sequence of immutable mixed-state quarantine rounds, because six
   remote nft boundaries cannot be atomic. Every round starts by authenticating
   the exact partition of already fenced nodes and still-live targets. It takes
   a fresh public `/info`, `/block/latest`, `/info` bracket and fresh
   authenticated loopback cross-proof only for those live targets, while each
   already-fenced node supplies a fresh capture-bound status inside the same
   observation bracket. The authorization deadline is exactly 300 seconds
   after that round's public sample completes. Immediately before each target's
   nft apply, the remote helper proves that exact authorization hash, node,
   writer identity, and deadline and writes a create-only applied receipt.
   Before the first restart-effective dependency, that authorization also binds
   the exact `preauthorization-boundary` capture described in section 1; this
   explicitly covers production sources with no snapshot in the data directory
   or its sibling. After quarantine stability and before stop, every still-live
   node must complete the distinct `post-quarantine-final-export` capture.
   A crash may therefore leave a valid mixed state: the immutable round result
   records whichever target subset crossed nft, and the next round freshly
   samples only the remaining live nodes. A zero-progress attempt is never
   appended to the transition ledger and may be resampled. A positive round is
   never rewritten or reused as authorization for a later target. Each node may
   cross live-to-fenced exactly once, so at most six positive rounds exist. The
   final generation ledger must cover all six nodes and sets the legacy cutoff
   to the maximum public height observed across all authorized rounds. There is
   no global all-six latch and no assumption that one local commit implies one
   remote mutation.
   The selected supervisor dependency is the first restart-effective persistent
   write. A natural same-boot exit before it remains resample/restart eligible.
   After it, a stopped terminal requires authorization expiry and two stable
   checks that the writer, supervisor, alternative activation sources, and
   pending jobs are absent while all exact dependencies are effective; it
   records unknown cause/no signal rather than fabricating a reboot. The final
   maintenance evidence is a tagged active/stopped union, and an all-stopped
   generation uses an explicit empty active sample set rather than an
   empty-input stability claim.
3. For NYC then LAX, freeze the exact cgroup-v2 supervisor subtree. For a
   detached writer, transiently freeze its audited root-session parent, move
   the sole writer into a newly created, locally frozen, inode-bound
   `arc-recovery-writer` child, durably seal that leaf, then thaw/release the
   parent. Require all four high-priority volatile control masks and the durable
   barrier arm before unlinking and fsyncing the allow marker. Before any
   frozen writer is thawed, install and prove the durable capture-bound
   maintenance fence on all six hosts, covering legacy RPC/P2P ingress and
   egress while retaining pinned SSH and exact loopback inspection. Send only pidfd
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
   forks rather than claiming a global legacy halt. Require `capture` to seal
   the mode-0400 `arc.validator-vault.offline-stop-evidence.v2` receipt and
   sidecar: all six fixed node-to-host identities, exact `stop.complete` and
`stop.files.sha256` roots, hash-pinned helper, and full stopped-status
argv/output hashes must verify before any new validator key is installed.
   Only after this successful capture, run the following exact vault `install`
   block, including both maintenance artifacts, both sidecars, all three
   derived artifact hashes, and the reviewed Ubuntu SSH/SCP paths and hashes.
   Its create-only `VALIDATOR-KEY-INSTALL-RECEIPT.json` must exist before
   checkpoint export or prearchive construction.

   ```bash
   legacy_maintenance_evidence_bundle=/secure/operator/arc-offline-stop-evidence.json.legacy-maintenance-evidence-bundle.json
   legacy_maintenance_evidence_bundle_sidecar="$legacy_maintenance_evidence_bundle.sha256"
   legacy_maintenance_evidence_bundle_sha256="$(/usr/bin/sha256sum "$legacy_maintenance_evidence_bundle" | /usr/bin/awk '{print $1}')"
   legacy_maintenance_boundary=/secure/operator/arc-offline-stop-evidence.json.legacy-maintenance-boundary.json
   legacy_maintenance_boundary_sidecar="$legacy_maintenance_boundary.sha256"
   legacy_maintenance_boundary_sha256="$(/usr/bin/sha256sum "$legacy_maintenance_boundary" | /usr/bin/awk '{print $1}')"
   offline_stop_evidence=/secure/operator/arc-offline-stop-evidence.json
   offline_stop_evidence_sidecar="$offline_stop_evidence.sha256"
   offline_stop_evidence_sha256="$(/usr/bin/sha256sum "$offline_stop_evidence" | /usr/bin/awk '{print $1}')"
   known_hosts=/secure/operator/arc-validator-known-hosts
   known_hosts_sha256=97c826f7e1a3940f6d18095ccdb0eaeebb5d66ec16fe60b9c5c47690e707485d
   ssh_identity=/secure/operator/arc-validator-maintenance-ed25519
   ssh_identity_sha256=9a7b57700dc7acf0faeca152fc341f237704e81965b5a9656fe8ccee4931444a
   printf '%s  %s\n' "$known_hosts_sha256" "$known_hosts" \
     | /usr/bin/sha256sum --check --strict
   printf '%s  %s\n' "$ssh_identity_sha256" "$ssh_identity" \
     | /usr/bin/sha256sum --check --strict
   printf '%s  %s\n' \
     47adf415134df7eff017e9557634696ba6b2a09f5a3bb1436d91d99b8a1cd5a6 \
     /secure/operator/tools/ssh | /usr/bin/sha256sum --check --strict
   printf '%s  %s\n' \
     92608e03bd81bf6cd96697ce3379fdf6a4c9bdba6a699f16bcc80cf0f49ce144 \
     /secure/operator/tools/scp | /usr/bin/sha256sum --check --strict

   "$ARC_RECOVERY_PYTHON_PATH" -I scripts/release/restore-validator-vault.py install \
     --restore-receipt /secure/operator/arc-v0.8-validator-restore/RESTORE-RECEIPT.json \
     --source-main-sha "$protected_main_sha" \
     --raw-actions-zip /secure/pretag/raw-v0.8.0/headless-linux-x86_64/actions.zip \
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
     --ssh-sha256 47adf415134df7eff017e9557634696ba6b2a09f5a3bb1436d91d99b8a1cd5a6 \
     --scp /secure/operator/tools/scp \
     --scp-sha256 92608e03bd81bf6cd96697ce3379fdf6a4c9bdba6a699f16bcc80cf0f49ce144 \
     --receipt-output /secure/operator/VALIDATOR-KEY-INSTALL-RECEIPT.json
   ```

   Treat that local receipt and its mode bits as integrity only, never as proof
   of origin. Prearchive additionally requires an operator-reviewed mode-0400
   known-hosts file containing one unique Ed25519 key for each exact fixed IP
   and one explicit mode-0400 SSH identity. The protected builder must generate
   a fresh random challenge and, through absolute root-owned `/usr/bin/ssh`
   with empty config/environment and agents/proxies/forwarding/passwords
   disabled, re-run the hash-pinned helper on all six hosts in parallel. Each
   canonical response binds the source commit, freeze/capture, helper, node/IP,
   legacy address/stake, challenge, and freshly re-derived stop tree roots. A
   missing host, host-key mismatch, replay, root mismatch, run over 120 seconds,
   or receipt older than 300 seconds fails before prearchive creation.
   The verifier's local Python is the exact normalized non-symlink
   `freeze_plan.operator_python_path` and is hash-bound before use. On Ubuntu,
   export its versioned target (for example `/usr/bin/python3.12`), never the
   usual `/usr/bin/python3` symlink; every parent must be root-owned and
   non-writable. No
   GNU-only `stat -c`/`readlink -f` or caller `PATH` is part of this gate.
4. Build and verify all six capture evidence trees and complete content indexes
   against the original fenced data directories. Preserve every source in
   place; do not discard a fork because it is not ultimately selected and do
   not create a second full local data-tree copy. Confirm each capture inventory
   binds the immutable pre-freeze observation root/receipt and that all three
   outcomes remain labelled diagnostic, noncanonical, and nonreward.
5. Run the recovery README's exact root/mode/size/SHA-256, four-row
   `SHA256SUMS`, and metadata verification for the independently preserved
   shared reference pair first. It binds block height 137145, block hash
   `8fac459a8de0164b28e30d3f67adf6aefe01054912a3d1ae5c53765e59935a90`,
   and state root
   `d300a2bb8dbe7f6da9596b550f31efd36eb842a1861e294c25740a19c8e3bc6d`.
   Source consensus round 9774808 is distinct recovery metadata, not the block
   height. Then use `arc-node recovery export --data-dir <reference-pair>
   --snapshot <reference-pair/state.snapshot.lz4> --legacy-validator-set
   <legacy-validator-set-40m.json> ...` to reproduce the candidate from that
   exact pair. Successful export—not
   endpoint metadata or a later validator capture—must prove that the decoded
   snapshot H/root equals its complete WAL block/checkpoint boundary.
   The audited legacy WAL needs the explicit `--allow-unbound-legacy-wal`
   exception because it predates the genesis network hash; record that fact.
6. In the one reviewed, root-only operator enclave that contains the six-key
   restored vault, isolate networking for the signing window and sequentially
   sign the accepted candidate with five distinct named keyfiles. Re-hash the
   exact materialized Linux binary against protected build metadata before
   every `recovery sign`; leave the sixth key unused and require final
   `recovery verify` to prove both five identities and strict-stake quorum.
   Then run `scripts/recovery/build-production-manifest.py prearchive`
   with the exact protected-main pre-tag Linux x86_64 node/CLI/build metadata,
   sealed freeze and public-height/offline-stop receipts, genesis/public and
   legacy validator sets, standalone `--legacy-maintenance-evidence-bundle`,
   `--legacy-maintenance-boundary`, and `--legacy-late-fork-source-set`
   artifacts with their exact sidecar-bound roots, the reviewed six-host
   known-hosts anchor and explicit
   SSH identity, preserved snapshot/WAL, checkpoint, reviewed Caddy
   2.11.4 binary, community reward probe, and a new private `--stage-root`.
   The builder copies every semantic input once through no-follow descriptors,
   fsyncs and seals the tree read-only, and only executes, reproduces, archives,
   or deploys those staged bytes. The builder—not a hand-authored
   draft—seals the **prearchive** production manifest and sidecar at mode 0400. Its
   `complete_sha256`, `archive_manifest_sha256`, `sha256sums_sha256`, and
   `prearchive_rollout_sha256` fields must all be 64 zeroes. The post-stop
   builder never compares historical round receipts with its current wall
   clock, because the official origins are intentionally offline and cannot be
   safely resampled. At entry and immediately before its create-only write it
   instead revalidates every immutable authorization/result pair in the
   quarantine generation ledger: exact prior-fenced/live partition, fresh
   target-only public and authenticated brackets, prior-fenced statuses inside
   the bracket, each nft apply at or before its round deadline, monotonic
   live-to-fenced transitions with positive progress, and final coverage of all
   six nodes. It also binds `first_nft_applied_at`, `all_nodes_fenced_at`, and
   the maximum public height across all rounds into the maintenance boundary.
   Thus a delayed but correctly sealed post-freeze build remains valid, while a
   stale target authorization, skipped/duplicate node, zero-progress ledger
   entry, reordered round, or rebound evidence fails closed.
7. Run `archive-fleet-to-drive.sh seal` in plan mode, then execute it only with
   the exact `ARC_RECOVERY_GO="GO <prearchive-rollout-sha256> FREEZE
   <freeze-plan-sha256> CAPTURE <capture-id> DEST
   <sha256-of-exact-drive-destination> LEGACY_WAL <BOUND|UNBOUND>"`.
   First run the authoritative [hash-pinned operator environment and exact
   seal commands](../scripts/recovery/README.md).
   That block requires the freeze-plan's versioned non-symlink Python path,
   exact SSH/SCP/known-hosts/identity hashes, rclone binary/config hashes, and
   `ARC_RECOVERY_GH_PATH`, `ARC_RECOVERY_GH_SHA256`, and
   `ARC_RECOVERY_GITHUB_LOGIN=FerrumVir`. Both plan and execute must pass the
   exact staged `--validator-install-receipt`, staged
   `--vault-restore-receipt`, protected `--finalization-intent`, and a distinct
   operator-owned mode-0700 `--work-root`. The execute boundary first proves a
   private Gist create/read-by-immutable-revision/delete canary; COMPLETE v2
   then binds the independently stored finalization intent's Gist id, revision,
   and content hash. It
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
9. Run `scripts/recovery/build-production-manifest.py finalize` with the sealed
   prearchive plus separately downloaded canonical `COMPLETE.json`,
   `ARCHIVE-MANIFEST.json` and sidecar, and `SHA256SUMS`, each accompanied by
   its independently verified trust root, plus the exact archived
   `drive-archive-seal-prefreeze.json` through required argument
   `--drive-archive-seal-prefreeze`, the unique immediately-pre-upload
   `drive-archive-seal-attempt.json` through required argument
   `--drive-archive-seal-attempt`, and archived
   `github-gist-write-canary.json` through required argument
   `--github-gist-write-canary`. The finalizer creates
   `/secure/operator/arc-recovery-final.lock.json` and its sidecar at mode 0400
   by changing only the four archive roots
   from step 6. Its canonical projection with those four fields
   reset to zero must hash exactly to the archived prearchive digest.
10. Confirm the final manifest stages the exact checksummed candidate and
   approved genesis/checkpoint for every host. The host keyfiles were already
   installed create-only after capture in step 3 and are represented here only
   by the sealed install receipt; do not deliver them again. The new release
   and data paths must be disjoint and non-nested with the preserved legacy
   source.
11. Run the finalized production plan, then execute only with
   `--go-hash <final-rollout-sha256> --archive-manifest-sha256
   <verified-archive-manifest-sha256>` and the exact
   `ARC_RECOVERY_GO="GO <final-rollout-sha256> FREEZE
   <freeze-plan-sha256> CAPTURE <capture-id> ARCHIVE
   <verified-archive-manifest-sha256> DEST
   <sha256-of-exact-drive-destination> LEGACY_WAL <BOUND|UNBOUND>"`. The exact
   executable plan and receipt-mode execute forms are:

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
12. Confirm public address, keyfile source, protocol v3, genesis/checkpoint,
   binary checksum, connected authenticated stake, and advancing chain on
   every host.
13. Re-verify the capture-bound maintenance fence that was installed before
   any frozen writer was thawed in step 3. Its sealed proof must cover legacy
   RPC and P2P ingress **and** egress (including established/source-port
   replies, TCP 9090, UDP 9091 and redirected UDP 443, IPv4/IPv6), retained SSH,
   exact loopback inspection, reboot persistence, stable authenticated heads,
   and the hash-pinned offline export of every final captured snapshot/WAL.
   Define the official six-origin cutoff as the maximum of pre-fence public
   observations, six stable fenced heads, and six final persisted heads. Seal
   `global_absence_claimed=false`, the exact official-origin set, late-fork
   maintenance policy, and `continuity_safety_margin=128`; set
   `chain.legacy_public_max_height` to cutoff plus that operational margin.
   The margin is not a cryptographic claim that no external legacy fork exists.
   Any independently validated higher late fork keeps v3/app/explorer in
   maintenance and is archived without rewriting v3 history. Require all six
   v3 nodes to converge on the same height/hash/root strictly above the sealed
   threshold and continue advancing through restarts and final publication.
14. In receipt mode, supply `--reward-evidence-output` before plan/preflight.
   Before submitting anything, fsync the six-node-agreed complete all-v3
   earnings baseline for every worker the sealed coordinator could select.
   Immediately re-prove that durable history and require the current selectable
   worker set to be a subset of the sealed set; this same GET-only check runs
   after a baseline-only crash resume, so a new worker or unrelated receipt
   aborts before ordinal one.
   Submit one real one-token job, wait (with the bounded rollout poll) until all
   six report the same `mined_success` 0x25 receipt, then submit the second.
   Require distinct transaction hashes, job IDs, block heights, and block
   hashes for the same worker. Each receipt must be exactly 2.5 ARC =
   2,500,000,000 base units. Every baseline block hash, transaction index, and
   receipt identity must remain,
   post-canary count must equal baseline + 2, and lifetime gross must equal
   baseline + 5 ARC with no third new row. Only the empty-baseline case ends at
   exactly two receipts; there, all six must report null observed rate and null
   `projected_daily_arc`, with both reasons exactly
   `collecting data: a projection needs at least 3 successful mined reward receipts spanning at least 24 hours, not the initial one or two rollout canaries`. For a nonempty
   baseline, the complete all-v3 timestamp window controls projection truth: a
   window shorter than 24 hours remains null with the canonical short-window
   reason; a valid window must expose the exact observed rate, and any numeric
   projection must equal that rate times 2.5 ARC. The tool then writes the two
   identities and selected pre-canary baseline as a create-only, mode-0444,
   rollout-SHA-bound v2 evidence file and checksum. It then restarts all six archive validators one
   at a time, requires continued fleet advancement, and re-proves both receipt
   rows from every restarted process. Same-height/different-hash results are a fork,
   not two blocks, and fail closed. Frontend publication may proceed with the
   honest null projection when projection inputs are unavailable and must not
   synthesize a forecast.
15. Let the authorized rollout automatically deploy every sealed
   `valid_noncanonical_fork`: it re-verifies the live stopped captures before
   mutation, stages only hash-indexed evidence on that validator's local disk,
   creates the locked `arc-archive` account, and generates the archive-only
   systemd unit plus GET-only loopback filter and derived
   `/legacy/<node>` TLS route. Require exact archive/validator MainPIDs,
   loopback listeners, provenance roots, method rejection, and Pages-only CORS
   before completion. Never mount or proxy Google Drive, and never expose
   archive POST, WAL, P2P, consensus, or signing state. No manual template or
   environment-file installation is part of the production sequence.
16. Generate the recovered frontend config only after its default path fully
   verifies and fetches the finalized Drive `ARCHIVE-MANIFEST.json`,
   `COMPLETE.json`, and fork inventories, plus the rollout-bound two-receipt
   evidence. (A paired mode-read-only local manifest/COMPLETE cache is an
   optional air-gapped input, never a required handoff.) The generator derives
   fork membership from the sealed six-node classification and URLs from
   sealed validator origins, verifies every live provenance pin, then writes
   create-only config bytes. Publish those exact
   bytes in one reviewable Git commit; verify the Pages hash and all
   provenance endpoints. Rollback by reverting only that config commit, which
   restores maintenance without rolling back or renumbering the canonical
   chain.

Each production validator accepts RPC only on its rollout-derived Unix-domain
socket. The remote unit passes `--rpc-unix` and omits `--rpc`; the manifest's
loopback `rpc_listen` value is retained only for local rehearsal and an exact
retired-TCP-port absence check. Configure these six explicit HTTPS origins on
every validator, each as its own repeated `--community-rpc-url` argument; P2P
peers are not RPC discovery:

```text
https://149.28.32.76
https://140.82.16.112
https://136.244.109.1
https://104.238.171.11
https://202.182.107.41
https://149.28.153.31
```

`/community/reward_policy.configured_community_rpc_origins` reports the
configured origin **count**, so the sealed production value is `6`; it does
not return the URL array. The locked rollout installs the SHA-pinned Caddy
2.11.4 gateway for each exact literal public IPv4 address. Caddy requests a
publicly trusted IP-address certificate from Let's Encrypt's production ACME
directory using its `shortlived` profile and HTTP-01 challenge; the
TLS-ALPN challenge is disabled. This removes any `nip.io`, `sslip.io`, or other
shared wildcard-DNS operator from the trust and availability path. Certificate
issuance and renewal still require the public ACME service and inbound port 80,
and the rollout fails closed unless a fresh direct-IP handshake validates
through the system public-CA store with hostname/IP verification, presents the
exact validator IPv4 as its sole SAN, identifies the public Let's Encrypt
issuer, has a total leaf lifetime <=160 hours with >=48 hours remaining, and
returns the expected HTTPS probe response. Exact leaf/timestamp evidence is
sealed once after all six maintenance gateways are installed but before any v3
start, then freshly sealed again after public promotion. A service restart
proves protected certificate-storage reuse only. The rollout does not wait
days and does not claim to have observed renewal; continuous expiry/renewal
monitoring is still required after handoff. ARI remains authoritative whenever
the CA supplies renewal timing; `renewal_window_ratio 0.5` is the generated
fallback and corresponds to roughly 80 hours remaining for a 160-hour leaf.
The gateway also installs a request/rate-limit filter
reachable only over a permission-sealed Unix socket, strict body limits,
security headers, an exact GitHub Pages CORS origin,
and a reviewed path allowlist. Public preflight terminates at Caddy; internal
validator routes never receive browser CORS. Unknown paths fail closed. These
origins remain candidate configuration until the coordinated cutover passes.
Raw public `:9090` endpoints and clear-text remote community origins are not
acceptable frontend or validator configuration.

The filter security boundary is pinned to Ubuntu Noble nginx
`1.24.0-2ubuntu7.17`, binary SHA-256
`1f16b72bea2f44e5d04fe6cf9e3e4b0dec53a82c50c7c1533c302a8ecaeccacf`.
It runs under the dedicated no-login `arc-rpc-filter` identity with no
capabilities and a strict systemd sandbox. Its exact binary, package, module,
config, preflight, and unit hashes are re-proved at each start and sealed in the
gateway security receipt. Every proxying location—including reward approval
and validator shard routes—uses nginx `auth_request`; stopping the interlock is
runtime-proved to deny both classes before any v3 process starts. Generated
Caddy configs require `admin off` and contain zero `forward_auth` handlers.
The distribution nginx unit remains stopped/disabled and its preserved config
is never loaded. This is not an all-host ports-empty precondition: the reviewed
LAX baseline has nginx active and enabled with one public port-80 listener and
no port-443 listener. The rollout records that baseline, stops/disables it
before Caddy, and rollback restores the exact active/enabled and 80/443 listener
counts. The nginx package is not held: a replacement fails the next
preflight/restart closed. Update it only by changing the reviewed version and
binary pins in protected code and rerunning the security tests and receipt,
never by an ad-hoc per-host hold/unhold or upgrade.

Validator RPC, legacy archive reads, Caddy-to-filter traffic,
filter-to-interlock checks, and filter-to-validator/archive traffic use
separate mode-`0660` Unix sockets beneath sealed mode-`0750` runtime
directories. The exact service identities and group memberships are proved;
there is no production TCP origin on the former loopback filter, interlock,
validator, or archive ports. The late-fork response schema is exactly
`arc.recovery.legacy-late-fork-interlock-status.v2`. A healthy gate has reason
`capture-bound-retirement-tripwire-clear`. Any retired official origin that
answers or is inconsistent, and any observed source above the cutoff, latches
`latched-legacy-source-incident`; an unavailable/inconsistent required
community monitor yields `community-source-observation-unavailable`. Every
status keeps `global_absence_claimed=false` because this is a capture-bound
retirement tripwire, not a global absence proof. The generated frontend
contract also pins `sourceMainCommit`, `observedCutoffHeight`, source-set hash,
boundary hash, and tool hash, so a status from another capture or code lineage
fails closed.

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
