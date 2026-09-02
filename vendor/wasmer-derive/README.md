# ARC-maintained Wasmer derive patch

This directory is a narrow source patch of `wasmer-derive` 6.1.0 from Wasmer
tag `v6.1.0`, commit `3189527eec99cfea7e6991328509e72ed0bec2e0`,
subdirectory `lib/derive`.

The upstream crate depends on the unmaintained `proc-macro-error2` 2.0.1
(`RUSTSEC-2026-0173`) solely to wrap the `ValueType` proc macro and emit two
input diagnostics. This patch removes that dependency and returns equivalent
`syn::Error` compile errors. The successful expansion is otherwise the
upstream 6.1.0 implementation, so the Wasmer runtime version and generated
`ValueType` implementation remain unchanged.

`tests/value_type.rs` exercises the generated valid-code behavior by checking
that padding bytes are zeroed without modifying field bytes. Rebase this patch
against an upstream Wasmer release once Wasmer removes `proc-macro-error2`.

The upstream MIT license is preserved in `LICENSE`.
Byte-level upstream fingerprints and the complete local-change inventory are
recorded in `PROVENANCE.md`; verification requires no network access when the
original crates.io archive is already present in Cargo's cache.

