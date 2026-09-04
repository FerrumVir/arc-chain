# ARC glib 0.18.5 security backport provenance

This directory contains the exact `glib` 0.18.5 crate published by the
gtk-rs project, plus the two-line upstream fix for
RUSTSEC-2024-0429 / GHSA-wrw7-89jp-8q8g. Tauri 2.11's released Linux GTK3
stack requires the 0.18 binding family, so ARC keeps the package version
honest and carries only this reviewed fix until Tauri supports a maintained
GTK stack.

## Canonical source

- Retrieved: 2026-09-03
- Crate: `glib` 0.18.5
- Registry archive: <https://static.crates.io/crates/glib/glib-0.18.5.crate>
- Archive SHA-256 (also the original Cargo.lock checksum):
  `233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5`
- Published VCS revision from `.cargo_vcs_info.json`:
  `42b9caf98e03ded086362d9653ca58fe94dc8658`
- License: MIT; the upstream `LICENSE` and `COPYRIGHT` files are preserved.
- Canonical upstream-file count: 121
- Canonical `src/variant_iter.rs` SHA-256:
  `1fd02859333761c45321b32f28b24233446b97d0022a90d3a937ed162585b90e`
- Canonical source-tree SHA-256:
  `c977877cf8a028d8e42fc2ce60cd85ae193c8959147d5560ed1958b9bfba6875`

The source-tree digest is SHA-256 over the concatenation of one row per
regular upstream file, sorted by POSIX relative path. Each row is
`SHA256(file)`, two ASCII spaces, the relative path, and a newline. ARC files
whose basename starts with `ARC-` are excluded.

## Applied upstream fix

- RustSec advisory:
  <https://rustsec.org/advisories/RUSTSEC-2024-0429.html>
- GitHub advisory:
  <https://github.com/advisories/GHSA-wrw7-89jp-8q8g>
- Upstream pull request:
  <https://github.com/gtk-rs/gtk-rs-core/pull/1343>
- Upstream merge commit:
  <https://github.com/gtk-rs/gtk-rs-core/commit/05dff0ee696f9bcd8617cd48c4b812d046d440cb>
- Merge commit SHA:
  `05dff0ee696f9bcd8617cd48c4b812d046d440cb`

Only `src/variant_iter.rs` differs from the canonical crate. The local pointer
used as the variadic GLib out-argument is mutable, and the call passes
`&mut p` instead of the unsound `&p`, exactly matching the upstream commit.

- Patched `src/variant_iter.rs` SHA-256:
  `a0f5ee8acb8faa089bcdfbc9a57372609fce7654026ccef7d9a224d05a654ccc`
- Patched source-tree SHA-256:
  `0a72c413b5a125e0312a2bd9740b852388f4e2ac784031dc78c683a78202b8b4`

## Reproduction validation

On 2026-09-03, both trees were release-tested on Linux ARM64 with Rust 1.93.1
and GLib 2.74.6 in the digest-pinned container
`rust@sha256:7c4ae649a84014c467d79319bbf17ce2632ae8b8be123ac2fb2ea5be46823f31`.
The canonical unpatched crate terminated its 11 `variant_iter` tests with
SIGSEGV. The patched tree passed all 11 tests in the same optimized profile.

`tests/release/glib_backport_contract_test.sh` reconstructs the canonical file
in memory, proves both canonical and patched tree identities, rejects links or
extra upstream files, checks the vulnerable pattern is absent, and verifies
that the desktop manifest and lock resolve this local source. Because cargo-deny
does not match advisories against a local-path package, the release-blocking
cargo-deny wrapper also shadows this crate with the canonical 0.18.5 registry
identity. Its live-database check requires exactly RUSTSEC-2024-0429 (and no
additional glib advisory) while the source contract proves that exact fix is
present. Remove this backport when ARC adopts a released Tauri stack backed by
a supported fixed GLib/GTK binding. No advisory suppression is used.
