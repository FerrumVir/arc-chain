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

Stake-zero community nodes may run the release in migration-observer mode.
That mode intentionally disables chain P2P, consensus, and voting when the
bundled genesis says `validator_set_complete = false`; HTTP community
inference remains available for controlled testing.

## 1. Freeze and inventory the current fleet

Record the following for all six validators before touching any process:

- host, operator, launch mechanism, binary version, and binary SHA-256;
- validator public address and configured stake;
- latest height, block hash, state root, and last-progress timestamp;
- authenticated peers and connected stake;
- data, WAL, snapshot, model, and environment locations;
- worker, inference, shard, reward, and receipt evidence;
- a byte-for-byte backup and a restore test on an isolated host.

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
source is verified. Use `archive-fleet-to-drive.sh seal-freeze-plan` to create
a reviewed, immutable six-host plan, then run `capture` in plan mode. Execution
requires its own exact `ARC_RECOVERY_FREEZE_GO="FREEZE <freeze-plan-sha256>"`.
The helper captures a bracketed LZ4 `/sync/snapshot`, endpoint evidence, and—
after a clean TERM-only stop—the final `state.wal`. Complete capture indexes
are create-only and fail on any changed, missing, unexpected, symlink, or
special-file content.

## 2. Rotate every validator identity offline

Generate six new Ed25519 keyfiles on trusted offline operator systems. One
keyfile per validator; never reuse a legacy seed or key.

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
only by the old validator set is not a sufficient trust anchor. Operators must
make an explicit out-of-band decision and record it in a reviewable manifest:

1. start a clean chain from a new genesis; or
2. adopt a specifically identified canonical state checkpoint under the new
   validator set.

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
complete validator genesis and rejects any schedule on the incomplete
stake-zero observer placeholder. The node also requires the independent
`--enable-community-rewards-v1` switch, so neither the schedule nor the switch
can enable issuance by itself.

Populate all release/deployment genesis copies from that approved public
manifest, set `validator_set_complete = true`, copy the identical reward
activation schedule into each, and verify that the files are byte-identical.
Every validator public address must also appear exactly once in the shared
`[[accounts]]` list with an explicit `balance` (zero is allowed). Runtime
startup and release validation both reject a complete validator genesis when
an address is missing from accounts or duplicated. A node's local keyfile must
only prove that it matches this shared definition; local identity must never
insert an account or otherwise mutate genesis state at startup.
The schedule is included in the authenticated semantic genesis hash; nodes
with different activation rules are different networks. The incomplete
genesis currently in source control is a safe placeholder, not a production
network definition.

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

- protect `main` and release tags with branch/tag rulesets, and enable immutable
  releases;
- restrict Actions to an owner-reviewed allowlist and require full commit-SHA
  pinning;
- create a protected `release` environment, restrict its deployment tags, add
  required reviewers, move `TAURI_SIGNING_PRIVATE_KEY` and its password from
  repository secrets into that environment, and remove the repository-level
  copies;
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
installer, updater manifest/signature, `SHA256SUMS`, seeds, and genesis from the
same commit and version. The publication gate cryptographically verifies all
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
`0444`, and protected by a SHA-256 sidecar. `run` is plan/preflight-only unless
both `--execute --go-hash <locked-manifest-sha256>` and the exact
`ARC_RECOVERY_GO="GO <locked-manifest-sha256>"` value are present. The harness
imports the quorum-verified checkpoint into six fresh data directories, proves
the selected legacy block H and v3 transition H+1, requires advancing
same-height hash/root convergence, restarts one validator at a time, and checks
the configured reward policy. Receipt mode additionally requires the exact
successful mined `0x25` receipt and receipt-backed worker earnings on all six.

## 5. Execute the coordinated v3 cutover

With six equal-stake validators, the strict greater-than-two-thirds quorum is
five; four validators are exactly two thirds and cannot finalize. Since v2 and
v3 are mutually incompatible, the network is expected to stop while fewer
than five approved v3 validators are online. Do not interpret that deliberate
maintenance halt as permission to mix protocols or lower quorum.

1. Announce a maintenance window and stop ordinary submissions.
2. Execute the separately sealed freeze plan. It snapshots and stops NYC,
   then snapshots and stops LAX. Four of six equal-stake validators cannot
   reach the required five-validator quorum, so finality is now deliberately
   halted.
3. While quorum remains halted, capture stable live snapshots and endpoint
   evidence from AMS, LHR, NRT, and SGP in parallel; only after all four live
   captures complete, stop those four and copy every final WAL.
4. Verify all six immutable capture indexes and that no legacy process is
   listening or sealing. Preserve all six captures; do not discard a fork
   because it is not ultimately selected.
5. Use `arc-node recovery export --data-dir <capture> --snapshot
   <capture>/state.snapshot.lz4 ...` to build the candidate from the accepted
   source. Successful export—not snapshot metadata—must prove that the decoded
   snapshot H/root equals the latest complete WAL block/checkpoint boundary.
   The audited legacy WAL needs the explicit `--allow-unbound-legacy-wal`
   exception because it predates the genesis network hash; record that fact.
6. Sign the accepted candidate offline with the required 5-of-6 quorum and
   seal the final production rollout manifest.
7. Run `archive-fleet-to-drive.sh seal` in plan mode, then execute it only with
   the exact `ARC_RECOVERY_GO="GO <rollout-manifest-sha256>"`. It re-exports
   every unchanged snapshot/WAL pair, labels matches against the checkpoint's
   H/hash/full-root, requires at least one real canonical match, and uploads
   all six labelled fork bundles plus the shared signed artifacts immutably.
8. Install the exact checksummed candidate and approved genesis/checkpoint on
   every host; install the host's new keyfile separately.
9. Start enough prepared v3 validators in a tight window to reach quorum,
   then start the remainder.
10. Confirm public address, keyfile source, protocol v3, genesis/checkpoint,
   binary checksum, connected authenticated stake, and advancing chain on
   every host.
11. Require all six to converge on the same advancing height/hash/root for a
   full observation window before reopening ordinary traffic.

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
headers, and a reviewed path allowlist. Unknown paths fail closed. Raw public
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
