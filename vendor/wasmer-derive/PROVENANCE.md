# Upstream provenance and local delta

The source is `wasmer-derive` 6.1.0 from
`https://github.com/wasmerio/wasmer`, tag `v6.1.0`, commit
`3189527eec99cfea7e6991328509e72ed0bec2e0`, path `lib/derive`.

SHA-256 fingerprints of the unmodified upstream inputs are:

| Input | SHA-256 |
| --- | --- |
| crates.io archive `wasmer-derive-6.1.0.crate` | `c546f3380840cd63fdcc390f04cd19002f2dfa19b4691b77ecbd27642bd93452` |
| `Cargo.toml.orig` | `8047ad7c5344e7185fe2e29cf1aadbf6f2c6b7abe4498698f718de5e416af5cd` |
| `src/lib.rs` | `b8dfc19c116156f2e5dab1f53380b0f1d59fee2ab02959206b40e2eb27657eab` |
| `src/value_type.rs` | `1a1486a26e44f2a8690a6f879b066bfcb440c76f5204a2f9cd0efb85a33a525c` |
| repository-root `LICENSE` at the commit above | `76dc7d305458d07478bc62669fe53dbfd3b94b95c5e00fbb45af1f492cbd7284` |

The crates.io archive checksum is also the checksum Cargo recorded for the
registry package before the workspace patch was applied.

The complete local delta is:

- `Cargo.toml`: remove `proc-macro-error2`; make the patch non-publishable;
  add `wasmer-types` only as a dev dependency for the generated-code test.
- `src/lib.rs`: remove the `proc_macro_error2` wrapper and convert a returned
  `syn::Error` into compile-error tokens.
- `src/value_type.rs`: replace the two `abort!` paths with equivalent
  `syn::Error` values. The successful `quote!` expansion is unchanged.
- `tests/value_type.rs`: add a valid-expansion behavior regression.
- `LICENSE`, `README.md`, and this file: preserve licensing and document the
  patch; they do not compile into the proc macro.

No Wasmer runtime, ABI, wire format, or successful generated implementation is
changed by this patch.

