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

## Inputs

Start from `recovery-manifest.schema.json`. The built-in validator is stricter
than JSON Schema and rejects unknown fields. A draft binds:

- protocol v3 chain ID, recovery epoch, validator-set ID, selected legacy H,
  exact H block hash, exact H+1 transition hash/root, and the approved
  checkpoint manifest hash;
- local absolute paths and SHA-256 values for `arc-node`, genesis, ARCCHKPT,
  and, in production, the exact Caddy executable;
- exactly six unique public validator addresses, stakes, keyfile paths, RPC
  origins, P2P addresses, and fresh data directories (or exact
  same-manifest-owned prepared state during a retry);
- timeouts, minimum observed height advance, and either a policy-only or real
  mined-receipt reward gate;
- production-only IP-derived `nip.io` (or resealed `sslip.io` fallback)
  hostnames, SSH/service identity, and create-only release/unit paths;
- production-only archive destination, freeze/capture identities, executing
  helper/orchestrator/rollout/schema hashes, explicit legacy-WAL policy, and
  four finalization roots. A prearchive manifest has four all-zero roots; a
  final manifest may change only those roots and must project exactly to the
  archived prearchive digest;
- the pre-positioned canonical GGUF path and SHA-256 on every validator, plus
  exact per-node ranges that give every one of the 32 layers three replicas
  while loading exactly 16 layers on each validator.

Never put private key bytes, seed strings, passwords, tokens, or environment
secrets in the manifest. `key_file` is only a path to a separately delivered
mode-`0600` key.

For a local rehearsal, give each process a distinct loopback IP such as
`127.0.0.11` through `127.0.0.16` and use `http://<that-ip>:9090` for both its
`rpc_listen` and `rpc_url`. P2P ports must be distinct. Every runtime receives
all six origins through repeated `--community-rpc-url` flags; P2P peers are
never treated as RPC discovery.

For production, `rpc_listen` must be loopback (for example
`127.0.0.1:9944`) and `rpc_url` must exactly match the validator's HTTPS
hostname. The manifest must carry the reviewed public GET/POST allowlists
verbatim. The rollout stages the SHA-pinned Caddy binary, validates its config
before issuance, persists ACME state under the release root, and installs a
dedicated loopback nginx filter for body/rate limits. Only the six validator
IPs may reach the signed internal approval path or the shard announce,
forward, and cleanup paths. Shard destinations are bound to these six explicit
HTTPS origins and responses are authenticated by active validator keys.
Unknown paths return 404; there is no public `:9090` listener.

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

## Execute the production cutover

Use a roots-only finalized, sealed `mode: "production"` manifest only after the
exact capture-scoped archive has a fully verified `COMPLETE.json` and all six
controlled legacy writers remain persistently fenced. This is not a claim that
all dynamically observed external legacy forks are globally halted. Run the
read-only plan to obtain the verified archive-manifest hash and exact extended
phrase:

```bash
ARC_RECOVERY_GO="GO $final_rollout_sha256 FREEZE $freeze_sha256 CAPTURE $capture_id ARCHIVE $archive_manifest_sha256 DEST $destination_sha256 LEGACY_WAL $legacy_wal_policy" \
  python3 scripts/recovery/recovery_rollout.py run \
    --manifest /secure/operator/arc-recovery-final.lock.json \
    --execute \
    --go-hash "$final_rollout_sha256" \
    --archive-manifest-sha256 "$archive_manifest_sha256"
```

The orchestrator:

1. stages and re-hashes the exact binary/genesis/checkpoint/Caddy artifacts;
2. validates the checkpoint and Caddy configuration remotely;
3. imports into six fresh or exact same-manifest resumable data directories;
4. installs create-only filter, gateway, and validator systemd units;
5. obtains publicly trusted TLS or fails closed;
6. starts five validators in a tight quorum batch, then the sixth;
7. proves loopback-only node RPC, HTTPS health, H/H+1 continuity, advancing
   convergence, the sealed 32-layer/3x HTTPS shard topology, and every
   one-at-a-time restart;
8. proves reward-policy agreement and, when selected, one successful mined
   `0x25` receipt plus receipt-only worker earnings on every validator.

On failure, the orchestrator stops/disables only the newly installed v3
services. It does not delete imported data, artifacts, configs, journals, or
the old archived fleet and never falls back to compromised identities. Each
prepared stage is marked with the exact rollout digest: the same manifest can
verify and resume it, while a different manifest is rejected. New release and
data paths must be disjoint and non-nested with every frozen legacy source.

### Efficient legacy archive

The legacy freeze and the final checkpoint seal are deliberately separate.
The final checkpoint hash cannot exist until the forked fleet has stopped, so
requiring that hash before capture would create a circular authorization. Seal
a small, create-only freeze plan v2 first. It binds the exact remote helper,
orchestrator, rollout verifier, and schema bytes plus the source commit,
sentinel order, six controlled writer contracts, and the sealed eight-member
40M legacy validator set:

```bash
scripts/recovery/archive-fleet-to-drive.sh audit-writers \
  --legacy-validator-set /secure/operator/legacy-validator-set-40m.json \
  --output /secure/operator/arc-writers.lock.json

scripts/recovery/archive-fleet-to-drive.sh seal-freeze-plan \
  --window arc-v3-cutover-2026-08 \
  --legacy-validator-set /secure/operator/legacy-validator-set-40m.json \
  --writer-contracts /secure/operator/arc-writers.lock.json \
  --output /secure/operator/arc-freeze.lock.json

scripts/recovery/archive-fleet-to-drive.sh capture \
  --freeze-plan /secure/operator/arc-freeze.lock.json

freeze_sha256='<freeze-plan hash printed by seal-freeze-plan>'
capture_id='<capture id printed by seal-freeze-plan>'
ARC_RECOVERY_FREEZE_GO="FREEZE $freeze_sha256 CAPTURE $capture_id" \
  scripts/recovery/archive-fleet-to-drive.sh capture \
    --freeze-plan /secure/operator/arc-freeze.lock.json \
    --execute
```

The capture ID is `SHA256("ARC recovery capture v2\0" || freeze_plan_digest)`;
it is not an operator-selected label. The default `capture` is read-only.
Immediately before every remote helper invocation, the orchestrator re-hashes
the installed helper and refuses any byte mismatch. Execution installs a
persistent systemd restart fence and cleanly stops NYC and then LAX, but those
sentinel stops do not authorize a global halt claim. It then stops AMS, LHR,
NRT, and SGP. The six exact controlled identities represent 30M of the sealed
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
then seal the prearchive production manifest with `complete_sha256`,
`archive_manifest_sha256`, `sha256sums_sha256`, and
`prearchive_rollout_sha256` all set to 64 zeroes. Plan and execute the archive
phase:

```bash
scripts/recovery/archive-fleet-to-drive.sh seal \
  --freeze-plan /secure/operator/arc-freeze.lock.json \
  --manifest /secure/operator/arc-recovery.lock.json \
  --validator-public-keys /secure/operator/validator-public-keys.json \
  --allow-unbound-legacy-wal

locked_sha256='<sealed prearchive rollout-manifest sha256>'
destination='arc-drive:ARC Chain Recovery/captures/'"$capture_id"
destination_sha256="$(printf %s "$destination" | python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')"
ARC_RECOVERY_GO="GO $locked_sha256 FREEZE $freeze_sha256 CAPTURE $capture_id DEST $destination_sha256 LEGACY_WAL UNBOUND" \
  scripts/recovery/archive-fleet-to-drive.sh seal \
    --freeze-plan /secure/operator/arc-freeze.lock.json \
    --manifest /secure/operator/arc-recovery.lock.json \
    --validator-public-keys /secure/operator/validator-public-keys.json \
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
`arc-drive:ARC Chain Recovery/captures/<capture-id>`. A changed or missing
capture or a Drive object bound to a different freeze hash stops before upload
or replacement. A per-node semantic export failure is preserved as
unclassified evidence and cannot masquerade as either a fork or a canonical
source. If strict offline recovery trims an uncheckpointed WAL tail, hashes of
the accepted prefix and quarantined tail bind the exact retained source bytes.

Each streamed bundle contains the complete stopped legacy data directory,
persistent fence evidence, optional public legacy binary/genesis inputs, and
semantic export result. Shared uploads include the sealed source snapshot/reference
WAL, final binary, genesis, source/public validator sets, signed checkpoint,
rollout manifest, capture ID, and `SHA256SUMS`. Private identities, service
environments, build caches, model weights, and Git objects outside `arc-data`
remain excluded; DAG persistence inside `arc-data` is retained in full.

After all six bundle/inventory pairs have been uploaded and independently
checked, the operator builds canonical `SHA256SUMS` and
`ARCHIVE-MANIFEST.json`. The manifest binds every shared input, all six
classifications, bundle/inventory sizes and SHA-256 values, both archive helper
hashes, rollout tool/schema hashes, the independently verified canonical
reference, source commit, freeze digest, capture ID, and prearchive rollout
digest. Those metadata files are uploaded and checked only after the bundles. Content-addressed
`COMPLETE.json`, which binds the archive-manifest hash, is the final create-only
mutation in this execution. Google Drive is not WORM or intrinsically
immutable: partial destinations without COMPLETE are resumable but must never
be consumed, and every object is re-downloaded and cryptographically checked.
Verify a destination before use:

```bash
scripts/recovery/archive-fleet-to-drive.sh verify-complete \
  --destination 'arc-drive:ARC Chain Recovery/captures/<capture-id>'
```

An absent, non-canonical, mismatched, or tampered `COMPLETE.json`, manifest, or
sidecar fails closed.

Use the emitted `FINAL-ROLLOUT-ROOTS` values to create a new final draft by
replacing **only** the prearchive manifest's four zero roots. Seal that draft to
a new file. Validation resets those four fields to zero and requires the
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
`/community/claim_work`, `/community/submit_work`, `/tx/submit_signed`, and
`/faucet/claim`. Inference has a 4,000-second upstream timeout, worker result
submission 2,700 seconds, and validator approval 1,500 seconds.

`/internal/community/reward/approve`, `/shards/announce`,
`/inference/forward_shard`, and `/inference/cleanup_shard` are validator-IP-only.
Source handlers `/inference/run_sharded`, `/inference/results`, `/tx/submit`,
`/community/reward_approval/{job_id}`, and `/eth` are not public v3 routes.
Unknown paths fail closed. The block explorer is a source-pinned static
candidate, not a deployed public service; configured origins or a successful
frontend build do not prove an explorer deployment or fleet cutover.

## Reward gates

`checks.reward.mode: "policy"` verifies all six `/community/reward_policy`
responses, including the exact protocol/issuance state, active set size six,
required approvals five, six explicit RPC origins, stake-zero eligibility,
epoch, set, domain, validator-set commitment, and amount agreement.

`mode: "receipt"` additionally needs either:

- fixed `tx_hash`, `job_id`, and `worker` values; or
- `probe_argv` whose absolute executable is bound by `probe_sha256` and emits
  exactly `{"tx_hash":"0x...","job_id":"0x...","worker":"0x..."}`.

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
  "expected_reward_base": 2500000
}
```

The stake-zero worker must already be running and registered with all six
sealed HTTPS origins. `/workers/scoreboard` and `/community/list` are public
read-only dashboard/probe endpoints; shard forwarding and reward approval stay
validator-IP-only.

The probe receives only these non-secret environment values:
`ARC_RECOVERY_RPC_URLS`, `ARC_RECOVERY_ROLLOUT_MANIFEST_SHA256`, and
`ARC_RECOVERY_CHECKPOINT_MANIFEST_HASH`. A pending or failed transaction never
passes. All six nodes must return the same successful mined `0x25` block
receipt with at least five approvals, and `/worker/earnings/{worker}` must
contain that exact successful receipt. Counts, local observations, configured
rates, and pending submissions are not earnings.

A later read-only audit can use externally captured evidence:

```bash
python3 scripts/recovery/recovery_rollout.py verify \
  --manifest /secure/operator/arc-recovery.lock.json \
  --reward-evidence /secure/operator/mined-reward-evidence.json
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
loopback gateway policy, same-height fork rejection, clean restart command
construction, hash-pinned reward probes, and successful-receipt-only earnings.
