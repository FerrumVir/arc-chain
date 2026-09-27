# Direct checkpoint recovery vs. release receipts (R9)

Purpose: keep the historical recovery and any later release as two separately
evidenced events, so that no release receipt reads as the event that
recovered the chain.

## 1. What the recovery record says

Source: `docs/recovery/v3-live-recovery.md`, added in `f616705d` (#104).

* The fleet was recovered onto protocol-v3 by **importing a quorum-signed
  checkpoint directly** into six fresh v3 data directories: source height
  138310 (transition 138311), manifest hash `0x9c6aa3ec…08acc0`, state root
  `0xd103671a…061c33`, 5-of-6 validator signatures. It was verified in the
  enclave (`recovery verify`) and activated on each node (`recovery import`).
* The production-rollout tool, which couples the cutover to full v0.8.0
  release provenance, was **not** used. The release itself (GitHub release,
  assets, installers/updater, native-Mac gate, post-release acceptance) is
  **deferred**.
* The record does **not** name the source commit or binary digests of the node
  binaries that performed the import and now run the fleet.

## 2. Why a later release could misstate this

`scripts/release/assemble-cutover-assets.py` accepts only a sealed
**production** recovery manifest (line 892), and it requires the manifest's
`provenance.pretag_version` to equal the release tag. The pipeline therefore
models exactly one history: the release performs the cutover. There is no
mode for a fleet that was recovered before the release existed.

Producing v0.8.0 cutover assets with the current tooling would require a
production recovery manifest that names v0.8.0 as its provenance. That
manifest would attribute the direct import to the release. It must not be
produced.

## 3. Rules for any later release receipt

1. **The recovery is a prior event.** A receipt refers to it by the record's
   commit (`f616705d`) and the checkpoint manifest hash. It never describes the
   recovery as performed by the release, its workflows or its binaries.
2. **Adopt, do not perform.** A release that ships to an already-recovered
   fleet states that it adopted that fleet at checkpoint 138310. It may bind the
   same approved checkpoint manifest hash, because the chain is the same. The
   cutover it did not perform must not appear as a cutover receipt. This needs
   either an explicit adoption receipt type in the release tooling or a
   release without cutover assets. That is an **owner decision**, because the
   tooling and its gates are owner-controlled.
3. **Running binaries change only by a recorded upgrade.** Until an upgrade
   is performed and recorded (before and after binary digests per host), the
   fleet runs the recovery binaries. Publishing a release changes nothing that
   is running.
4. **This candidate performed no recovery.** The ARC Chain V2 branch
   (`arc-chain-v2-implementation-20260919`, local and unpushed) ran no live
   node, changed no validator and published nothing. Its evidence is local
   fixtures and soaks. Its snapshot v2 and checkpoint-rebase work (C11) is not
   deployed. The record's restart limitation therefore still holds on the live
   fleet. In this candidate, a restart also still decodes the whole state WAL
   (documented limit).

## 4. Audit of this branch

The branch's Markdown was searched for statements tying recovery to a release
(`recover(y|ed)` within a sentence of `release|signed|v0.8|tag`, both orders).
The hits are the lifecycle caveats in `CHANGELOG.md`, `README.md`,
`docs/ANNOUNCEMENT.md`, `docs/GETTING_STARTED.md`, `docs/HEADLESS_INSTALL.md`
and `desktop/FIRST-RUN.md`. Each says v0.8.0 was an unreleased recovery
candidate at its cutoff, and none attributes the recovery to a release. The
recovery-process documents (`docs/VALIDATOR-FLEET-ROLLOUT.md`,
`scripts/recovery/README.md`) describe the gated pipeline, not the direct
import. That matches §2: the pipeline has no adoption path.

## 5. What closes R9

| Item | Needs | Status |
|---|---|---|
| Source commit and binary digest of each of the six recovery binaries, with each node's activation evidence | fleet access (E2) | open, external |
| Adoption receipt type, or the decision to ship without cutover assets | owner decision on owner-controlled tooling | open |
| No document in this branch attributes the recovery to a release | this audit | done |
