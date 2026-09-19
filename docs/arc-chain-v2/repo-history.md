# ARC Chain repository history (read-only check, 2026-09-19)

## Repository and Sep 18 update

- Repository: [FerrumVir/arc-chain](https://github.com/FerrumVir/arc-chain), public; default branch `main`.
- No local Git checkout exists at `/Users/excaulibur/work/arc-ch`; that directory is not a repository. GitHub CLI was authenticated as `FerrumVir` with `repo` and `workflow` scopes.
- `main` head: `f616705d6058b75543f6a894dfb7254410055453`, authored/committed `2026-09-18T20:11:46Z` (3:11:46 PM America/Chicago), pushed `20:11:48Z`.
- The update is merged PR [#104](https://github.com/FerrumVir/arc-chain/pull/104), “docs: ARC v3 live recovery deployment record”; source branch `docs/v3-live-recovery-record`; merge `20:11:47Z`. It adds only `docs/recovery/v3-live-recovery.md` (61 lines), with a valid GitHub signature.

## GitHub release API evidence (checked 2026-09-19)

- `GET /repos/FerrumVir/arc-chain/releases/latest` returns published `v0.7.11` (15 assets), published `2026-06-15T15:34:31Z`.
- `GET /repos/FerrumVir/arc-chain/releases/tags/v0.8.0` returns HTTP 404; the releases collection has no `v0.8*` entry, draft or published.
- `GET /repos/FerrumVir/arc-chain/git/matching-refs/tags/v0.8` returns an empty list. The newest tag is `v1.0.0-pre-partnership` (a tag only, no matching release in the releases list); the newest versioned release/tag is `v0.7.11`.
- Thus the documented v0.8.0 release and its claimed 32 assets do not exist in GitHub Releases today. The latest published release has 15 assets, including Linux packages/AppImage, Windows installers, macOS DMGs/app archives, signatures, and `latest.json`.

## What the update claims

The record says protocol-v3 was recovered from the divergent v0.7 fleet by direct import of a signed checkpoint into six fresh validator data directories. It documents a 5-of-6 signed checkpoint (source height 138310 / transition 138311), six validators, HTTPS gateways, maintenance interlock, public console/explorer configuration, and a slow-restart limitation due to full `state.wal` replay. It explicitly defers the v0.8.0 release, 32 assets/installers/updater, native-Mac desktop gate, post-release acceptance, and community reward canaries.

## CI and publication evidence

- PR checks: Golden vectors passed on Ubuntu, macOS arm64, macOS Intel, and Windows. The other listed PR checks passed except `supply chain (cargo-deny)`.
- Post-merge CI run [35390119807](https://github.com/FerrumVir/arc-chain/actions/runs/35390119807) on `f616705` failed only `supply chain (cargo-deny)` at `2026-09-18T20:16:53Z`; the exact finding was real vulnerability [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285) in `rustls 0.23.43`, fixed upstream in `>=0.23.45`. The configured `unic`/GTK exceptions were merely `advisory-not-detected` warnings.
- Post-merge Pages run [35390119811](https://github.com/FerrumVir/arc-chain/actions/runs/35390119811) failed the live-truth gate at `2026-09-18T20:12:11Z`; `dashboard/test-live.mjs:37` asserted expected checkpoint proof `verified` but observed `unknown` (`checkpoint-evidence-incomplete`). “Publish GitHub Pages” was skipped. The 56/56 frontend network contract checks and 35/35 composite explorer checks passed before the gate.
- Therefore the commit is pushed and merged, but the public console/explorer was not successfully republished by that push, and CI is red.

## Current blockers / next checks

1. Resolve the live checkpoint evidence path so `dashboard/test-live.mjs` can prove the configured recovered checkpoint against the live fleet; rerun the deployment workflow and verify Pages publication.
2. Resolve the cargo-deny advisory failure (or update a justified policy exception based on the actual dependency graph), then rerun CI.
3. Before claiming complete explorer/node/on-chain inference readiness, run the deferred v0.8.0 release gates and post-release acceptance, then validate model-loading validators and community RPC reward canaries. Add state snapshot persistence to reduce slow restart and production pause risk.

Open non-dependency PRs observed: #97 (quarantine recovery hardening) and #48 (`demo-hardening`, parallel sharded inference / on-chain income / cross-OS desktop / CI gates). No changes were made locally or remotely.
