# Existing recovered-chain update release profile

`assemble-existing-chain-update.py` emits `arc.existing-recovered-chain-update/v1`. It attests readiness to update an existing recovered chain. The claim explicitly leaves the historical cutover ceremony unverified, reports no original cutover archive, and authorizes no validator retirement. It does not emit or synthesize a cutover lock, freeze proof, capture boundary, or retirement policy; existing v1 recovery/cutover/retirement checks remain unchanged.

The helper re-runs `select-pretag-artifacts.py` against the saved artifact API response, then invokes `materialize-pretag-artifacts.py` on the exact selected raw Actions ZIPs. That path checks API ID/digest/size, inner archive digest, canonical `BUILD-METADATA.json`, and payload hashes. It materializes two explicit roles: `--artifact-platform` must match the binary currently running on every live host; `--verifier-platform` selects an executable on the qualification machine to run the existing `arc-node recovery inspect` and `recovery verify` commands. A Mac verifier can qualify the signed checkpoint locally while a Linux artifact remains the deployed candidate; the two selected artifacts must carry identical genesis bytes and exact commit/run metadata.

The helper delegates quorum validation to tracked `scripts/release/update-quorum-validator.py` and records that script's hash. It requires a fresh advancing proof of every live host, pinned SSH identities, one common block, exact network pins, and every live host's process/hash to match the selected production artifact. By default all six validators must be live; the only exception is the declared-offline mode below, with up to two validators named offline by the owner input. The proof's RPC `genesis_network_hash` is recorded distinctly from the checkpoint verifier's signed-genesis hash; the latter must also equal the approved `genesis.network-hash` input file. Both values are retained without conflating the two fields. The verifier's certificate authority and stake set must equal the live validator pins plus the declared-offline pins, if any: each of the six signed validators is pinned exactly once.

Native inference is irreversibly activated on the live chain. A host config row may therefore carry a `native` pin (`argv_tail`, `files_sha256`, `context_commitment`); pins must cover every host or none and share one context commitment. For a pinned host the running argv must end with exactly that tail followed by the optional desktop-only `argv_suffix` described below, no native flag may appear anywhere else, and the proof row must repeat the pin and report identical native file hashes. An unpinned host must show no native flag and no native evidence.

The attestation is prepared against a planned `vX.Y.Z` and exact protected-main commit before that tag exists; it binds the tag version to pre-tag build metadata and the commit. The final release workflow remains responsible for requiring the real protected tag to resolve to that commit and current main. This avoids the release-cycle loop where a tag is needed to build the proof that must gate publishing the tag.

The operator assembles a fresh observation only after every live host runs the exact selected preflight Linux artifact. A separate runtime CI build from the same commit is insufficient: its binary can differ. The local helper uses the current time and the existing 120-second proof limit, after artifact and checkpoint verification.

`existing-chain-update-input.py create` makes a Git object with current main as its sole parent and exactly five root files: `approved-genesis.network-hash`, `host-config.json`, `quorum-proof.json`, `recovery.arcchkpt`, and `update-attestation.json`. It binds the evidence hashes, excludes extra files and credential fields, and never pushes or changes a checkout. The operator publishes that exact object at `refs/arc-existing-chain-updates/<main-sha>` and dispatches `existing-chain-update-handoff.yml` on main with its commit SHA, plus `declared_offline_site` only for a declared-offline update. The producer fetches that exact ref, validates the bounded tree, reselects the live preflight artifact tuple, and independently verifies the checkpoint with the selected Linux binary.

Queue time is not treated as a new network observation. Producer re-verification accepts an owner observation no more than two hours old, rechecks its proof against its original observation time, and records capture, observation, and re-verification times separately. The owner's Mac verifier and the producer's Linux verifier may differ; both must belong to the same selected preflight, and the production binary and verified checkpoint semantics must match. Different raw API JSON encodings are recorded separately while immutable artifact IDs and digests remain bound. This is historical operational evidence, not a claim of publication-time liveness.

**Before creating the release tag or dispatching `release.yml`, the release owner must complete and sign off the
[single user-facing explanation review checklist](../../docs/VALIDATOR-FLEET-ROLLOUT.md#user-facing-explanation-review--every-tagged-release)
for the exact candidate and link the record from the release PR or internal release record.** This applies to every
tagged release with user-facing explanations, including desktop-only and update-profile releases. Unreviewed
applicable explanations and unresolved mismatches block sign-off: without the owner's signed-off record, do not
tag or dispatch. Follow that explanation-review step without treating the cutover-only procedures in the surrounding
runbook as update-profile requirements. Existing release approvals and artifact/attestation gates still apply;
this process requirement is not automatically enforced by the workflow.

After the protected producer succeeds and the explanation-review record is signed off, dispatch `release.yml` with `release_profile=existing-recovered-chain-update-v1`, the exact update handoff artifact ID/digest/attempt, and the preflight run/attempt. Leave the cutover-only input tuple empty. The final tag must exist at current protected main. The update package has 30 assets: it replaces the three historical cutover assets with `arc-existing-chain-update-attestation.json`. The signing handoff uses explicit v2 metadata; mixed file sets and schema downgrades are rejected. Ordinary updater signature checks remain unchanged. First-cutover and validator-retirement consumers remain v1-only and refuse to infer their required evidence from this update profile.

Stopped v0.7.7 installs also require the v1 retirement evidence. The installer recognizes the update attestation in the verified, exact-tag signed manifest and refuses legacy migration before downloading binaries or cutover assets. Published-artifact acceptance checks this refusal for both installation and `--update-only`, preserves the legacy executable/configuration/model/state, and records `legacy_migration_refused=true` and `v08_fresh_data=false`. Cutover-profile acceptance still requires successful migration. Fresh installations and updates of recovered installs keep their existing verification path.

Enabling legacy migration requires authentic ceremony outputs: the sealed, finalized `arc-recovery-final.lock.json` and its SHA-256 sidecar, `legacy-maintenance-boundary.json` with capture/freeze/quarantine and observed-cutoff evidence, and the matching quorum-signed `recovery.arcchkpt`. `assemble-cutover-assets.py` derives the compact descriptor and policy from these inputs and the pinned inspector binary/genesis. The compact assets bind a release tag and source commit (the boundary binds the source commit), so carrying them into later releases also needs a reviewed provenance contract. The update attestation and genesis cannot reconstruct the missing ceremony evidence; release signatures authenticate available bytes without adding those facts. `build-postrelease-public-truth.py` consumes the full cutover evidence and does not supply it.

These helpers and workflows do not by themselves prove fleet deployment, distributed inference, native payment settlement, platform signing, or a successful public release. Each requires its own actual execution evidence.

## Up to two validators declared offline

An update may publish while up to two of the six validators are offline only when the owner input says so explicitly. Without that declaration every rule above applies to all six, exactly as before. The mode is fail-closed:

- **Named in the owner input.** `host-config.json` lists every live host in `hosts` and `quorum_sites`, sets `unobserved_stake` to the sum of the offline stakes, and names each validator in `declared_offline` as a row with exactly `site`, `ip`, `validator` and `stake`. Unobserved stake must always be named this way; anonymous unobserved stake is refused. The host config is already bound by hash in the owner attestation, the input commit and the producer's re-verification, so the declaration is bound the same way.
- **Confirmed explicitly.** The owner passes `--declared-offline <site>` to the helper, once per declared validator, and the producer dispatch passes `declared_offline_site=<site>` or `declared_offline_site=<site>,<site>`. Sites must be distinct three-letter lowercase names, without spaces. Each confirmation must equal the host config's declaration: a declaration without the confirmation, or a confirmation without the declaration, is refused.
- **At most two.** Three or more declared validators are refused even when the remaining stake would still meet the quorum. Live hosts plus declared validators must make up the six.
- **Quorum without them.** The live stake must stay at or above the node's exact `strict_supermajority_threshold` from `crates/arc-types/src/lib.rs`: `1` when `total_stake == 0`, otherwise `quorum_stake = total_stake - (total_stake - 1) // 3`. This requires strictly more than two-thirds of the total, including totals divisible by three, and uses integer arithmetic through `2**64 - 1`. Host configs still require positive total stake, and live plus declared stake must equal the total. For a synthetic total of 60, live stake of 40 is refused and 41 meets the threshold; supplying the old threshold of 40 is also refused.
- **Zero margin.** Every quorum summary carries `zero_margin`, true when removing any single live validator would leave live stake below `quorum_stake`. When true, the assembled attestation carries a one-entry `warnings` list, the assembler prints the warning, and the handoff summary includes the same plain line: "Zero margin: if any one live validator fails to restart, blocks stop until it is fixed." When false, the warning is absent. This reports the margin without changing the gate.
- **Same certificate.** Each declared validator's identity and stake must match a remaining signed validator in the checkpoint certificate. It is not a live pin, and no live or declared pin may repeat it.
- **Every live host on the candidate.** Each live host must pass every per-host check above, including running the selected production binary, in both samples and in the common block.
- **Connected to every other live host.** Each live host must report at least `min(4, live hosts - 1)` peers in both samples. Six or five live hosts keep the floor of four; with four live hosts, each must see the other three. The count shows connections, not which validators they are.
- **Never inferred from silence.** The offline set comes only from the declaration. A configured live host that does not answer refuses the proof; it is never moved to the offline set.
- **Must not be answering.** Each proof sample carries one `declared_offline` row per declared validator (`site`, `ip`, `validator`, `health_url`, `http_status`, `binary_sha256`) recording an HTTPS GET of `https://<ip>/health`. Only no HTTP response (`http_status` null), or HTTP 502, 503 or 504 from the front, with `binary_sha256` null, counts as not answering. Any other result refuses the proof, on any binary: a validator that answers must be configured live and run the candidate. A declared validator that appears as a sampled host or common-block row also refuses the proof. A proof without a declaration may not carry these rows.

The attestation then names the validators in `declared_offline_validators`, in ascending `site` order exactly matching the host config's `declared_offline` list (unsorted lists are refused), and records its proof under `fresh_live_host_proof`, with `host_count`, `online_stake`, `declared_offline_sites` and `declared_offline_stake`. A six-host update keeps `fresh_six_host_proof` and is otherwise unchanged. The claim is unchanged too: each declared validator stays in the validator set, and nothing here authorizes retirement. The health read shows only that the declared host's public RPC did not answer. It does not show that no process runs on that host, and it does not observe peer-to-peer traffic. The declaration is the owner's record of an offline validator, not a measurement.

## Desktop updates with existing validators

`desktop-update-existing-validators-v1` is an explicit opt-in profile. Use it only
when an independent review finds the release's node changes compatible with the
validators' current version. The compatibility review for v0.8.20 with v0.8.19
validators is the initial use case; selecting the profile confirms that review.
This is a release preparation path, not authorization to restart validators.

Only this desktop profile permits an optional host `argv_suffix`, immediately
after the pinned native tail (or after the non-native prefix when there is no
native pin). Exactly four lists are permitted. The original dry-run and active
suffixes remain valid, unchanged:

```json
["--community-twin-execution", "--community-demand-pump", "--community-demand-dry-run"]
["--community-twin-execution", "--community-demand-pump"]
```

The two additional suffixes pin the interval and all three fan-out values.
Each flag and each value is a separate string argv item, in exactly this order:

```json
["--community-twin-execution", "--community-demand-pump", "--community-demand-dry-run", "--community-demand-interval-secs", "30", "--community-demand-max-groups", "32", "--community-demand-reserve-workers", "2", "--community-demand-groups-per-hour", "720"]
["--community-twin-execution", "--community-demand-pump", "--community-demand-interval-secs", "30", "--community-demand-max-groups", "32", "--community-demand-reserve-workers", "2", "--community-demand-groups-per-hour", "720"]
```

These pin a nominal 30-second interval, at most 32 open pump groups, two
reserved idle workers, and a 720-groups/hour token-bucket refill. The bucket
also permits a burst of 32; this is not a strict rolling-hour cap. Dry run
plans and logs demand without dispatching it. The owner must approve the
exact values, per-host suffix and maintenance window before any restart or
flag change, including a dry-run-to-active transition. An accepted evidence
suffix is not deployment approval or proof that an older binary supports it.

Every `--community-twin-*` and `--community-demand-*` argument must appear only
in that exact pinned suffix. Unpinned, missing, reordered, extended, duplicated,
unknown or malformed flags and values are refused, as are numeric JSON values,
`--flag=value` forms and values other than the exact strings above. Suffix pins
under the validator update profile or any other profile remain forbidden.
The exact observed argv-tail comparison and all native, binary, checkpoint,
quorum and baseline-version checks remain in force. In particular, this
allow-list does not permit a mixed-version validator proof: every live host
must still run the same authenticated previous-release binary required below.

Pass `--profile desktop-update-existing-validators-v1 --previous-release-tag
vX.Y.Z --confirm-node-compatible` to the owner assembler, then use the same
`release_profile` and `previous_release_tag` inputs on
`existing-chain-update-handoff.yml`. The named baseline must be a qualifying
strict semantic version in the complete GitHub release listing:
immutable, published, not a prerelease, authored by github-actions[bot], and named
`ARC Chain vX.Y.Z`. It must precede the candidate. Its published `SHA256SUMS` must
verify with the installer's source-pinned release-manifest key and namespace
`arc-release-manifest-v1`; its Linux node hash must match the published asset.

Every qualifying release newer than the baseline and older than the candidate
must be a desktop-only update over that same baseline. Each must carry
`arc-desktop-update-attestation.json`, uploaded by github-actions[bot], with an
API SHA-256 digest and size of at most 1 MiB. Its bytes must match both that digest
and its entry in the release's own `SHA256SUMS`, verified with the same
source-pinned key and namespace. Its `profile` must be
`desktop-update-existing-validators-v1`, `node_compatible_with_validators` must
be exactly true, and `validators_release.tag` and `binary_sha256` must match the
authenticated baseline. Missing, malformed or unverifiable evidence refuses the
selection with the existing newest-qualifying-release error. Duplicate baseline
or intervening qualifying versions and a qualifying release at or beyond the
candidate also refuse it.
When the baseline is already newest, the existing authentication rules apply.

The explicit `--confirm-node-compatible` confirmation remains mandatory. The
independent compatibility review must compare the candidate's node against the
validators' **actual release**, not merely the release immediately before the
candidate. Skipping desktop releases does not establish candidate compatibility.

Every live host must run exactly that previous Linux binary. All other quorum,
checkpoint, network, artifact, and at-most-two-declared-offline rules above still
apply. Capture retains the existing 120-second freshness check; the protected
producer accepts owner evidence up to two hours old. Desktop release assembly
also enforces that two-hour observation bound and re-authenticates the named
baseline and every skipped desktop update, so a newly published qualifying
release invalidates an old handoff. The candidate production and checkpoint-verifier binaries remain bound
to the selected signing-preflight artifacts, independently of the validator hash.

Dispatch `release.yml` explicitly with this profile and the same immutable update
handoff ID, digest and attempt tuple. Its signed, 30-asset package substitutes
`arc-desktop-update-attestation.json` for the ordinary update attestation. That
asset records the actual validator tag, node hash, signed manifest hashes and
compatibility declaration. `validators_release_skipped_desktop_updates` records
each skipped tag, release ID, commit and attestation SHA-256 in ascending version
order, or an empty list when there are none. Verification re-derives and compares
the entire list. Legacy newest-only attestations may omit the field only when
the re-derived list is empty. Release notes and the handoff summary repeat the
actual validator tag and node hash and name the skipped desktop updates (or none).
Post-release acceptance recognizes the distinct asset set and still refuses legacy migration. No default,
cutover behavior, ordinary update rule, signing gate or updater limit changes.

## Dependency audit policy

Rust release audits use `scripts/ci/run-cargo-deny.sh` and `deny.toml`. The
ignore-list policy requires a date added, named risk owner, explanation of why
the issue is not exploitable here, and a removal condition; re-review every
exception at every release.

JavaScript release audits use `python3 -I -B scripts/release/npm-audit-gate.py`
with `--package <dir> --level <low|moderate|high|critical>`, plus `--omit-dev`
and/or `--lock-only` where the caller previously used npm's corresponding flags.
Package paths are relative to the caller's directory; the policy file is always
`npm-audit-exceptions.json` at the script's repository root. Release builds
check all five lockfiles at low, except the compatibility SDK: low for runtime
dependencies and high for its development tools. CI and the local quality
harness use the same compatibility SDK split without lock-only mode.

The exceptions file follows the same policy as `deny.toml` and additionally
requires an ISO date hard expiry. Each entry names one exact GHSA and package,
`added`, `owner`, `reason`, `removal`, and `expires`. Remove it on the stated
condition, no later than expiry; never extend silently. Expiry is inclusive
through that UTC date and fails the gate the next day, even if unused. An
unused, unexpired exception produces a warning; remove it when a patched
release clears the advisory.

The gate follows every npm `via` link to all root advisories. A finding at or
above the threshold is excepted only when every root matches a valid entry.
Unknown or malformed reports, unresolved causes, inconsistent counts, audit
execution failures, and invalid or expired policies fail closed with exit 2.
Unexcepted findings exit 1 and print their advisory, package, and installed
paths; a successful gate exits 0. The summary counts gated package findings
(not unique advisories); `excepted` counts fully covered findings and
`exceptions_used` lists matched IDs. Error summaries are incomplete counts and
must never be interpreted as a pass independently of the exit code.

Run the offline saved-report tests with
`python3 -I -B tests/release/test_npm_audit_gate.py`; they also run as part of
`bash tests/release/run.sh`.
