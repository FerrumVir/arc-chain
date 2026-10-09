# K2.6 creation policy: INT16 default

TJ's 2026-10-09 policy selects INT16 for **attention, dense MLP, shared experts,
embedding and lm_head**. Routed experts retain their native packed INT4 bytes
and BF16 group scales. Routers retain their native INT16/power-of-two contract;
norms retain i64 Q16. This changes policy selection, not any arithmetic kernel.

## Entry points and identities

`arc-mla convert`, `slice-plan`, `slice`, `slice-manifest`, `yarn-prepare`,
`slice-assemble` and `slice-assemble-yarn` now select all-INT16 when the policy is
omitted for a Kimi source/preparation. A complete explicit policy with all five
classes INT16 produces exactly the same bytes and identities. Non-Kimi creation
defaults are unchanged. `stream_slices.py` resolves the default from hash-verified
original config; an omitted-policy resume needs that retained config before any
fetch/deletion. Conflicting existing unit records fail before shard operations.

```json
{"version":1,"attention":"int16","dense":"int16","shared":"int16","embedding":"int16","head":"int16"}
```

The selected policy is serialized into the existing mixed-precision model,
manifest and unit-record identities **before** conversion, layout or hashing.
It participates in the model root. Omission on creation therefore does not mean
omission in the output identity. Full, early-layers-plus-head probe and synthetic
fixture scope identities remain distinct. Pending YaRN still rejects ordinary
assembly; only explicit preparation can finalize it, after matching the policy.

`--historical-int8` explicitly reproduces the old policy-absent INT8 comparison
identity. It conflicts with `--precision`; duplicate flags and missing/malformed
policy files fail. `--precision FILE` retains explicit historical/mixed comparison
experiments. These controls do **not** authorize a deployment downgrade: changing
any class off INT16 requires #164 measurements and TJ's sign-off. An all-INT8 JSON
policy is a precision-bound comparison identity, not the older policy-absent identity.

Existing packages/manifests are decoded exactly as written. No new default is
injected by deserialization, verification or execution. Old slices cannot be
resumed or assembled with an omitted Kimi policy; select the matching explicit
historical control or rebuild them from original sources. Merely changing a
precision field cannot convert bytes: record, slice, segment, package and model
checks still reject substitutions.

Library defaults: `SliceSource::open`, `convert_stage`, `prepare_yarn_manifest`,
`assemble_yarn_bundle` and `finalize_pending_slices` select the creation policy.
Low-level `with_precision` / `*_with_precision` calls are explicit comparison
interfaces: passing `None` explicitly retains historical INT8. Model/config
parsers and layout writers consume already-declared identities, not defaults.
`yarn::official_config` remains a pinned reference/configuration constructor;
creation/finalization callers set policy before hashing it.

## Admission and #164 handoff

Every nonzero INT16 conversion row must satisfy **2^-17 ≤ max(abs(w)) < 2^30**,
after semantic KV-B transpose. Zero rows are valid. Below-window, upper-edge and
nonfinite data fail with tensor/row diagnostics; no downgrade, flush or clamp is
introduced. The census and independent conversion oracle remain required before
real conversion. Census callers should explicitly use the all-INT16 JSON above;
the diagnostic fixed-window census remains independent of chosen policy.

#164 should pin this #168 revision, update its hash-checked observer to that exact
source, and make `int16` the primary comparison. Legacy and single-class/mixed
profiles remain labelled historical experiments. Compare original-source FP32,
never dequantized ARC weights. Do not inject defaults into historical capture
identities. Old explicit INT16/legacy/mixed fixture model roots and arithmetic
are preserved. The existing CPU/Metal kernel work on #174/#177 is untouched.

Reproducible offline checks (fresh WORK/OUT directories; fixture weights only):

```sh
cargo +nightly-2026-03-16 build --locked -p arc-inference --bin arc-mla
python scripts/arc_mla/check_precision_slices.py target/debug/arc-mla WORK OUT
python scripts/arc_mla/check_int16_rows.py target/debug/arc-mla ROW_OUT
python scripts/arc_mla/check_yarn_cli.py target/debug/arc-mla YARN_CLI_OUT
python -m unittest scripts/arc_mla/tests/test_stream_slices.py -v
cargo +nightly-2026-03-16 test --locked -p arc-inference --lib modern::mla
```

The precision fixture checks omitted versus explicit policy byte equivalence,
all five classes, native INT4/router/norm preservation, scalar/SIMD and 1/2/4
stage tokens/logits/boundaries/roots, stale records, policy removal/substitution,
and altered/truncated/missing/reordered data. Full/probe controls are metadata-only
Rust tests. Row controls execute both default and explicit conversion for 30
cases: zero, both threshold neighbours/edges, five classes and KV-B transpose.
Existing CI runs these on Linux x86/ARM and Windows; local Studio evidence is
separate from exact-head CI. Historical proof artifacts retain their original SHA.

## Resource planning (not admission)

`precision_budget.py` and `reference/kimi-k26/precision-budgets.json` now mark
INT16 primary; numbers are recalculated from the same pinned config/source sizes.
Historical policies remain explicitly labelled. GiB means 2^30 bytes.

| Early layers + original head | Retained source GiB | Slices GiB | Assembly copy GiB | Peak disk GiB | Sequential RAM plan GiB |
|---:|---:|---:|---:|---:|---:|
| 1 | 5.3017 | 5.3038 | 5.3038 | 21.4103 | 23.2800 |
| 2 | 14.4371 | 14.4392 | 14.4392 | 48.8174 | 112.5915 |
| 3 | 23.5725 | 23.5745 | 23.5745 | 76.2245 | 185.2780 |

Peak disk retains original sources, slices and one assembled bundle, canonical
tables, 0.5 GiB metadata/scratch allowance and 5 GiB reserve. Extra retained
whole/split bundles add another payload. For one layer, conversion scratch is
4.4249 GiB plus 0.0312 GiB KV-B transpose scratch; assembly buffers are 0.3994 GiB.
The reference expands original sources to 10.6034 GiB FP32 parameters, with a
4.3750 GiB largest-tensor transient, retained sources and 3 GiB runtime margin:
23.2800 GiB sequential available-RAM planning. ARC exits before reference loads.
Persisting FP32 adds 10.6034 GiB disk for one layer (74.1544/137.7055 GiB for two/three).
These are planning estimates, not allocator hard bounds or measured free space.

The former 16.1087 GiB one-layer bound is historical INT8 only and cannot admit
the default. The last Studio snapshot (8.054 GiB, 2026-10-09 01:36:58 UTC) is stale
and below even this plan. No fetch is authorized by these calculations; fresh
resource checks and the separate storage/census/oracle gates still apply. The
~33 GB/token and 13–25% speed reduction are projections, not fixture-derived
throughput measurements. This code stage establishes no real Kimi quality/timing.

### YaRN CLI fixture policy repair

The metadata-only CLI harness now uses distinct INT16 and historical INT8 pending
fixtures. The INT16 fixture adds one byte per promoted BF16 matrix element to the
historical segment sizes, calculated from the pinned configuration. Omitted and
explicit INT16 preparation/finalization must be identical; the legacy fixture
requires `--historical-int8`. Both paths test incomplete full selection, forged
source, cleared pending marker, substituted policy and wrong segment sizes.
Legacy metadata under the default and policy-only relabelling are rejected.
All hashes in these fixtures are synthetic placeholders, not real-weight proof.

This repairs a stale test caller, not the production policy contract. #164 can
use the same INT16-default contract above at the corrected #168 head. No package,
model identity, arithmetic or resource-budget change follows from this repair.
