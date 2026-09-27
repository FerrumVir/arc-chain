# ARC Chain CI fix (uncommitted local patch, 2026-09-19)

## Finding

The failing post-merge CI run was [35390119807](https://github.com/FerrumVir/arc-chain/actions/runs/35390119807), job `supply chain (cargo-deny)`, completed 2026-09-18 20:16:55Z. Its exact finding was [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285): TLS 1.3 handshake messages were accepted across encryption-level boundaries. The affected locked crate was `rustls 0.23.43`; cargo-deny states the solution is `>=0.23.45`. The dependency reaches `arc-net`, `arc-node`, `arc-relayer`, `reqwest`, `quinn`, and related network paths. This is a real vulnerability, so no advisory suppression was added. The existing `unic`/GTK `advisory-not-detected` entries are warnings and were not the failure.

## Local patch

In fresh checkout `/Users/excaulibur/work/arc-chain-readiness-20260919`, only these lockfile entries changed:

- `Cargo.lock`: `rustls 0.23.43` -> `0.23.45`, checksum updated.
- `desktop/src-tauri/Cargo.lock`: `rustls 0.23.43` -> `0.23.45`, checksum updated.

The initial `cargo update` also proposed unrelated `getrandom` lock-edge rewrites; those were reverted so the final diff is limited to the security fix. No `Cargo.toml`, `deny.toml`, source files, commit, push, or deployment was changed.

## Verification

Using the repository's pinned `scripts/ci/run-cargo-deny.sh` with Python 3.11 for its shadow helper (the system Python 3.9 lacks `tomllib`):

- Root profile: PASS — `advisories ok, bans ok, licenses ok, sources ok`.
- Desktop profile: PASS — `advisories ok, bans ok, licenses ok, sources ok`.
- `cargo metadata --locked --offline --no-deps` passed for both root and desktop manifests.
- No full compilation was run, per scope.

## Separate Pages blocker

The failed post-merge Pages run was [35390119811](https://github.com/FerrumVir/arc-chain/actions/runs/35390119811), job `Verify and assemble public console`, completed 2026-09-18 20:12:13Z. Its live-truth step failed at 20:12:11Z with `AssertionError: exact recovery checkpoint proof is unknown: checkpoint-evidence-incomplete` (`actual: unknown`, `expected: verified`) from `dashboard/test-live.mjs:37`; GitHub Pages publication was skipped. The lockfile patch does not address this separate live-evidence issue.
