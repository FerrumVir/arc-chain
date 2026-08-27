# ARC recovery rehearsal and rollout

`recovery_rollout.py` is the execution boundary for one six-validator protocol-
v3 recovery. It does not choose the canonical legacy block, create or rotate
keys, sign a checkpoint, or authorize production. Those are human/offline
decisions. It makes the approved decision reproducible and refuses a partial,
mutable, clear-text, or same-data-directory rollout.

The default `run` behavior is read-only. Production mutation requires the same
SHA-256 three times: the sealed manifest sidecar, `--go-hash`, and the exact
`ARC_RECOVERY_GO="GO <hash>"` phrase.

## Inputs

Start from `recovery-manifest.schema.json`. The built-in validator is stricter
than JSON Schema and rejects unknown fields. A draft binds:

- protocol v3 chain ID, recovery epoch, validator-set ID, selected legacy H,
  exact H block hash, exact H+1 transition hash/root, and the approved
  checkpoint manifest hash;
- local absolute paths and SHA-256 values for `arc-node`, genesis, ARCCHKPT,
  and, in production, the exact Caddy executable;
- exactly six unique public validator addresses, stakes, keyfile paths, RPC
  origins, P2P addresses, and initially absent data directories;
- timeouts, minimum observed height advance, and either a policy-only or real
  mined-receipt reward gate;
- production-only IP-derived `nip.io` (or resealed `sslip.io` fallback)
  hostnames, SSH/service identity, and create-only release/unit paths;
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
`inspect` plus quorum `verify`, checks six fresh data/key/host prerequisites,
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

Use the identical command with a sealed `mode: "production"` manifest only
after the legacy fleet archive is complete and all legacy `arc-node` processes
are stopped. The orchestrator:

1. stages and re-hashes the exact binary/genesis/checkpoint/Caddy artifacts;
2. validates the checkpoint and Caddy configuration remotely;
3. imports into six fresh data directories;
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
the old archived fleet and never falls back to compromised identities.

### Efficient legacy archive

The legacy freeze and the final checkpoint seal are deliberately separate.
The final checkpoint hash cannot exist until the forked fleet has stopped, so
requiring that hash before capture would create a circular authorization. Seal
a small, create-only freeze plan first:

```bash
scripts/recovery/archive-fleet-to-drive.sh seal-freeze-plan \
  --window arc-v3-cutover-2026-08 \
  --output /secure/operator/arc-freeze.lock.json

scripts/recovery/archive-fleet-to-drive.sh capture \
  --freeze-plan /secure/operator/arc-freeze.lock.json

freeze_sha256='<hash printed by seal-freeze-plan>'
ARC_RECOVERY_FREEZE_GO="FREEZE $freeze_sha256" \
  scripts/recovery/archive-fleet-to-drive.sh capture \
    --freeze-plan /secure/operator/arc-freeze.lock.json \
    --execute
```

The default `capture` is read-only. Execution captures a stable
`/sync/snapshot` LZ4 payload plus bracketed snapshot metadata, health, latest
block, DAG round, validator set, and network identity before it stops a node.
It captures and cleanly stops NYC and then LAX, leaving only four of six equal-
stake validators running when five are required. With finality halted, it
captures the remaining four live RPCs in parallel, then stops all four and
copies each final `state.wal`. `freeze` accepts an already-stopped node; it
never uses SIGKILL. Every capture has a complete file index, refuses overwrite,
and detects changed, missing, unexpected, symlink, or special-file content.

Build the unsigned candidate from an accepted capture using the exact recovery
exporter. The capture directory is the data directory and its LZ4 payload is
the required snapshot; successful export independently decodes the snapshot,
recomputes its account/storage/code root, and requires it to equal the latest
complete WAL block/checkpoint boundary:

```bash
arc-node recovery export \
  --data-dir /root/arc-recovery-captures/<freeze-sha256>/<node> \
  --snapshot /root/arc-recovery-captures/<freeze-sha256>/<node>/state.snapshot.lz4 \
  --genesis /secure/operator/genesis.toml \
  --validator-public-keys /secure/operator/validator-public-keys.json \
  --output /secure/operator/candidate.arcchkpt \
  --source-consensus-round <captured-current-round> \
  --recovery-epoch 1 \
  --validator-set-id 1 \
  --allow-unbound-legacy-wal
```

The last flag is necessary for the audited legacy WAL, which predates the
authenticated genesis network hash. It is never implicit: both checkpoint
creation and final archive sealing require the operator to state it, and the
binding evidence records that exception. Sign the accepted candidate offline,
seal the production rollout manifest, then plan and execute the second phase:

```bash
scripts/recovery/archive-fleet-to-drive.sh seal \
  --freeze-plan /secure/operator/arc-freeze.lock.json \
  --manifest /secure/operator/arc-recovery.lock.json \
  --validator-public-keys /secure/operator/validator-public-keys.json \
  --allow-unbound-legacy-wal

locked_sha256='<sealed rollout-manifest sha256>'
ARC_RECOVERY_GO="GO $locked_sha256" \
  scripts/recovery/archive-fleet-to-drive.sh seal \
    --freeze-plan /secure/operator/arc-freeze.lock.json \
    --manifest /secure/operator/arc-recovery.lock.json \
    --validator-public-keys /secure/operator/validator-public-keys.json \
    --allow-unbound-legacy-wal \
    --execute
```

`seal` rechecks the immutable rollout sidecar, every artifact hash, and the
5-of-6 signed checkpoint. On each host it runs the same snapshot-assisted
export against the unchanged capture. A capture is labelled `canonical_match`
only when exported H, block hash, and full state root equal the selected
checkpoint. At least one real match is required; all six bundles, including
non-matching forks, are retained and uploaded under
`arc-drive:ARC Chain Recovery/<rollout-manifest hash>`. A changed or missing
capture, an invalid export, or a Drive object bound to a different freeze hash
stops before upload or replacement.

Each bundle contains the stopped `state.wal`, exact LZ4 snapshot, endpoint
evidence, optional public legacy binary/genesis inputs, and the semantic export
result. Shared uploads include the final binary, genesis, public validator set,
signed checkpoint, sealed rollout manifest, capture ID, and `SHA256SUMS`. The
archive intentionally excludes private identities, service environments,
build caches, model weights, Git objects, and tens of gigabytes of legacy DAG
trace. Those bytes remain untouched on each validator disk. Excluding the DAG
bulk cannot silently choose a fork: the signed checkpoint and exporter provide
the explicit H/hash/root recovery boundary.

## Reward gates

`checks.reward.mode: "policy"` verifies all six `/community/reward_policy`
responses, including the exact protocol/issuance state, active set size six,
required approvals five, six explicit RPC origins, stake-zero eligibility,
epoch, set, domain, validator-set commitment, and amount agreement.

`mode: "receipt"` additionally needs either:

- fixed `tx_hash`, `job_id`, and `worker` values; or
- `probe_argv` whose absolute executable is bound by `probe_sha256` and emits
  exactly `{"tx_hash":"0x...","job_id":"0x...","worker":"0x..."}`.

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
```

The tests cover manifest strictness, six-validator and restart-quorum rules,
content-addressed sealing/no-clobber behavior, dual GO authorization, exact
checkpoint commitments, explicit HTTPS origins, loopback gateway policy,
same-height fork rejection, clean restart command construction, hash-pinned
reward probes, and successful-receipt-only earnings.
