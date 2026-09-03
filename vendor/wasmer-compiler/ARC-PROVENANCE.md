# ARC provenance and local delta

This directory is the complete crates.io source archive for `wasmer-compiler`
6.1.0. Upstream is [`wasmerio/wasmer`](https://github.com/wasmerio/wasmer),
tag `v6.1.0`, commit `3189527eec99cfea7e6991328509e72ed0bec2e0`,
path `lib/compiler`.

The immutable source input is
`https://static.crates.io/crates/wasmer-compiler/wasmer-compiler-6.1.0.crate`.
Its SHA-256 is
`4946475adc0af265af8f10aadf4d4a3c64845bcd3801c655bdd81ce5e3ee869b`,
which is also the registry checksum recorded in ARC's pre-patch `Cargo.lock`.

ARC changes one dependency requirement in the normalized, active
`Cargo.toml`: `memmap2 = "0.6.2"` becomes `memmap2 = "0.9.11"`. The original
`Cargo.toml.orig` and every Rust source file remain byte-for-byte upstream.
The compiler declares `memmap2` but contains no Rust call site for it, so this
manifest-only update cannot change Wasmer runtime behavior. Together with the
`shared-buffer` patch, it removes every old-version edge affected by
[`RUSTSEC-2026-0186`](https://rustsec.org/advisories/RUSTSEC-2026-0186).

Tree hashes use this canonical construction: sort all archive-member regular
files by slash-separated relative path, emit
`<file-sha256><two spaces><relative-path><newline>` for each, concatenate the
rows, and SHA-256 the result. `ARC-PROVENANCE.md` and the repository-root MIT
`LICENSE` copied below are local metadata, not crate-archive members.

| Input | Files | SHA-256 |
| --- | ---: | --- |
| Unmodified archive-member tree | 48 | `e7840ac914010cebba035656508e2be063324e8a86203cfea2782affd97f2dda` |
| Patched archive-member tree | 48 | `338c2b414786a8c34fce99045b001d360ffb4e8364bf9b1de248bc2ca54326b3` |
| Unmodified normalized `Cargo.toml` | 1 | `eba4475a226e9e1c9a72f9726041e7dc231a1d19df482d9c84dd425637966878` |
| Patched normalized `Cargo.toml` | 1 | `4f8183fb8c5f90a3f226f2962b5c8e61542ba1902451a6d6ac3b28cc08517b9a` |
| Upstream `Cargo.toml.orig` | 1 | `01a0233523acde44a2354252705042b9cf52d3e6a3151b362381043b5b805b3b` |

The crate declares MIT. The published archive omits the repository-root
license, so ARC preserves the exact `LICENSE` from Wasmer tag `v6.1.0`; its
SHA-256 is
`76dc7d305458d07478bc62669fe53dbfd3b94b95c5e00fbb45af1f492cbd7284`.

The upstream archive's nested `Cargo.lock` is retained byte-for-byte as an
archive member. Cargo ignores dependency-package lockfiles; ARC builds and
audits the root `Cargo.lock`, whose resolved graph contains only `memmap2`
0.9.11.

`tests/release/memmap_soundness_contract_test.sh` reconstructs the unmodified
tree in memory by reversing exactly that one manifest line, verifies both tree
hashes and the license, and proves both Wasmer consumers resolve to the one
patched `memmap2` package.
