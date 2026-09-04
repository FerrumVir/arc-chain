# ARC provenance and local delta

This directory is the complete crates.io source archive for `shared-buffer`
0.1.4. Upstream is
[`wasmerio/shared-buffer`](https://github.com/wasmerio/shared-buffer), tag
`v0.1.4`, commit `65e72d29726a748b71f754846fef3dad7df64f61`.

The immutable source input is
`https://static.crates.io/crates/shared-buffer/shared-buffer-0.1.4.crate`.
Its SHA-256 is
`f6c99835bad52957e7aa241d3975ed17c1e5f8c92026377d117a606f36b84b16`,
which is also the registry checksum recorded in ARC's pre-patch `Cargo.lock`.

ARC changes one dependency requirement in the normalized, active
`Cargo.toml`: `memmap2 = "0.6.1"` becomes `memmap2 = "0.9.11"`. The original
`Cargo.toml.orig` and every Rust source file remain byte-for-byte upstream.
The crate only uses `Mmap::map`, `Mmap::len`, and slice indexing, whose API is
unchanged in 0.9.11. This removes one of the two Wasmer edges affected by
[`RUSTSEC-2026-0186`](https://rustsec.org/advisories/RUSTSEC-2026-0186); the
other edge is patched in `vendor/wasmer-compiler`.

Tree hashes use this canonical construction: sort all archive-member regular
files by slash-separated relative path, emit
`<file-sha256><two spaces><relative-path><newline>` for each, concatenate the
rows, and SHA-256 the result. `ARC-PROVENANCE.md` is local metadata and is not
an archive member.

| Input | Files | SHA-256 |
| --- | ---: | --- |
| Unmodified archive-member tree | 13 | `9093016e27b7669d0c17645033923688760dd6c4da1e8b23c67048dd34efa553` |
| Patched archive-member tree | 13 | `3c190ce902b46fb35742215734a3d00f90bffb2d6bff9da464239f77cb8b5086` |
| Unmodified normalized `Cargo.toml` | 1 | `aa7c01b846cfc75309c0d59a7cf5aa9f936684b5da9c8c2e23890474d2b0fe67` |
| Patched normalized `Cargo.toml` | 1 | `f0bd49214e65470705d249096c9a7b794f7fe8b50440d16c9f7a1f003415eaeb` |
| Upstream `Cargo.toml.orig` | 1 | `0b88e0feff65c1b84d15b58edc72552c709473a3ee536f9f95df9fa39f6d7566` |

The upstream dual-license files are preserved unchanged:

- `LICENSE_MIT.md` SHA-256:
  `e25487d4fa108f45f082cb416574dd1d8888a036d733e0d6c891c78574acacb8`
- `LICENSE_APACHE.md` SHA-256:
  `62c7a1e35f56406896d7aa7ca52d0cc0d272ac022b5d2796e7d6905db8a3636a`

`tests/release/memmap_soundness_contract_test.sh` reconstructs the unmodified
tree in memory by reversing exactly that one manifest line, verifies both tree
hashes and licenses, and proves the locked `arc-node` graph reaches only
`memmap2` 0.9.11.
