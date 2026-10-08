# Provisional precision integration into lossless slices

This #168 revision integrates ARC-66 engine commit
`616ba16a60f43b5e70666ca24f5d7f9ce99ab932` over the reviewed slice integration
`df02231f4f999926e95e13b459ab5f14a5a8302c`. The recorded dependency label remains
**`provisional/unreviewed`** pending owner acceptance. This new integration
requires its own independent review. This is offline code/fixture readiness, not acceptance,
a shipping precision choice, API quality or a real K2.6 result. No weights were
fetched. #156/#164/#166/#167 are unchanged by this work.

The engine is already part of this repository's crate rather than an external
Cargo dependency. The integration imports that exact commit's precision module,
config/layout/model/ops changes, four new engine goldens and arithmetic contract;
it reconciles the existing streaming converter and prefixed packed-source
reader. The precision engine itself adds no Cargo dependency. The source engine's
arithmetic is unchanged; `INT16-CONTRACT.md` defines conversion, domains,
rounding, scales and SIMD fallback. Historical evidence keeps its original SHA.

The row-admission updates were imported with provenance-preserving cherry-picks:
`07a44d7c` → `0788c258`, then `616ba16a` → `b7b2d78b`. The earlier integration
had already imported the `596a61f6` engine. The new changes add an explicit
pre-write row-window rejection, tensor/row diagnostics, independent rational
arithmetic corpus and actual-converter row controls; successful arithmetic and
fixture identities must remain unchanged. `language_model.` class selection
and the existing KV-B key transpose/value split remain intact.

Main `b26f973913df34c238c63081d0245657ed2c2826` was merged separately at
`c550bb1b`. The sole conflict was `.gitattributes`: retain main's legacy-bridge
fixture `-text` rule together with both K2.6 rules (config/vector JSON and
retained-source manifest). `Cargo.lock` auto-merges main's legacy-bridge entry
while retaining the existing inference `memmap2` dependency; no dependency
upgrade was introduced. #156's later main merge `f0f02dca` was not imported and
is not treated as independently reviewed.

## Select policy before bytes exist

`convert`, `slice-plan`, `slice`, `slice-manifest`, `slice-assemble`,
`slice-assemble-yarn`, and `stream_slices.py` accept `--precision POLICY.json`.
The policy contains all five matrix classes and version 1. Omission preserves
legacy bytes/identities. For example:

```json
{"version":1,"attention":"int16","dense":"int16","shared":"int8","embedding":"int8","head":"int16"}
```

This integration's **mixed** fixture means attention/dense/head INT16 and
embedding/shared INT8. It is a test policy, not a recommendation. All-INT16
selects INT16 for all five classes; routers, i64 Q16 norms, Q32 bias and native
INT4 experts keep their existing representations.

The #156 `check_int16.py` mixed fixture instead selects attention/shared/embedding
INT16 and dense/head INT8. The word "mixed" alone does not identify a policy.
Both scripts print and retain the complete policy; budget rows and layout
tables carry it too. #164 must include all five selections beside every result.

`SliceSource::with_precision` validates policy before layout, planning and
conversion. Complete policy is committed in unit-record context, plan and slice
manifest, including manifests still pending YaRN. The prefixed source reader
strips `language_model.` before class selection and calibrates each chosen
matrix directly from original BF16 bits. KV-B still uses the existing semantic
transpose before quantization. The packed expert reader copies/reorders the
original low-nibble-first INT4 values and original BF16 group scales losslessly;
no policy can override routed experts. Vision skipping is unchanged.

Assembly requires the same **caller-owned** policy, not one inferred from the
manifest. `prepare_yarn_manifest_with_precision` verifies that policy against
the sealed input, independently pinned source/config and scope. It derives
canonical YaRN tables and a distinct full/probe/fixture profile. Legacy
`prepare_yarn_manifest` / `assemble_yarn_bundle` wrappers still require omitted
precision. Legacy pending-YaRN assembly remains rejected; no marker is cleared.
A source-unit record from another policy cannot be resumed or relabeled. The
streaming driver rejects resume policy mismatches before fetching or deleting
source files, and forwards the same policy to all three conversion commands.

Selected slice and segment records are reconstructed from exactly the bytes
sent to the package writer, then compared with the committed records. Final
packages are hashed/read back and verified against the finalized manifest.
The existing unpublished staging-directory transaction and rejection cleanup
remain; it publishes only a verified new bundle. Existing output is preserved.
This retains prior publication/durability limitations; it introduces no stronger
crash-fsync or adversarial-filesystem race claim.

## Offline verification

```sh
cargo test --locked -p arc-inference --lib -- --nocapture
cargo clippy --locked -p arc-inference --all-targets --features candle -- -D warnings
cargo build --release --locked -p arc-inference --bin arc-mla
python -m unittest scripts/arc_mla/tests/test_stream_slices.py -v
python scripts/arc_mla/check_precision_slices.py target/release/arc-mla precision-work precision-evidence
python scripts/arc_mla/precision_budget.py precision-budgets.json
```

Use fresh work/evidence directories. This executes three synthetic packed
four-layer fixtures (dense0 + MoE1–3), with canonical YaRN, no download, zero
expert scales and negative INT4 extrema. Tests compare native expert bytes,
router/norm/bias bytes across all policies and inspect each selected `.q` dtype.
Each policy verifies a full bundle, scalar/SIMD tokens/logits/boundaries/root,
and separately assembled 1/2/4-stage pipelines. Raw package/run/manifest evidence
is retained. The existing portable workflow executes this on Linux x86-64,
Linux ARM64 and Windows; local Studio supplies ARM evidence.
The same slice-workflow matrix also runs the standalone `check_int16.py`,
the independent arithmetic corpus regeneration and `check_int16_rows.py`, so
the separate dense/head INT8 fixture and row controls have CI coverage even on
the branch push. PR workflows supply repository-wide checks after main is
reconciled. Each CI result must be tied to the new head.

The inherited corruption controls cover alteration, truncation, appended/missing
bytes, tensor/slice/segment metadata, ordering/coverage, source/config/scope,
pending-marker removal, existing outputs and assembled-package corruption.
Additional controls reject policy changes/omission/invalid schemas, reused unit
records and cross-policy BF16 payload substitution without a published output.
Stale-unit controls try both omitted and different non-null policies. The
streaming driver test checks both before any fetch/run/deletion. Payload
controls try the raw legacy head slice and that payload zero-padded to the
expected INT16 length; the latter must fail the committed-byte comparison,
not the size check, and leave neither a bundle nor staging directory.
Full and 1/2/3-layer official metadata controls verify policy-bound layouts and
roots without allocating real weights. They do not claim real execution.

Cross-host Rust execution is not an independent execution oracle. The Python
reference still needs INT16 and YaRN execution support before real-weight
admission; independent conversion-byte agreement alone does not close that gap.

## #164 handoff (future edit, not performed here)

1. Pin this #168 revision in its isolated diagnostic Cargo manifest/lock, retaining
   the provisional #156 dependency status. Reconcile `MlaConfig.precision` and
   the `QView.q16` field; update the read-only observer source hash against the
   merged engine, preserving its numerical-neutrality comparisons.
2. Forward the identical policy file to slice planning/conversion, manifest
   generation and YaRN assembly. Consume the verified resulting manifest;
   record the complete policy alongside profile/model-root/source/request/scope
   identities. No reuse of legacy INT8 slices as INT16 input.
3. Execute the same declared graph/tokens/positions/masking on retained original
   source tensors in the pinned official reference. Keep original-source FP32
   comparison distinct from a dequantized-ARC-weight comparison. For the latter,
   decode signed LE INT16 q times mu times 2^-k; routed INT4 remains unchanged.
4. Preserve one-layer and multi-layer dense/MoE controls, routing difference
   reporting, missing/reordered captures, provenance/identity/nonfinite rejection,
   certification, budget and McNemar regressions. FP32 errors stay labelled per
   host, never golden digests. No tolerance PASS or precision decision until
   independent review and real-weight assessment.

## Revised resource plans: no admission

`reference/kimi-k26/precision-budgets.json` is generated from the pinned official
config/source metadata, with nine per-depth/per-policy rows. A Rust regression
independently compares its slice bytes and FP32 element counts with actual
canonical package layouts. These are **calculated planning quantities**, not
free-space/RSS measurements. The one-layer peaks are:

| Policy | Retained original source GiB | Slices GiB | Assembly copy GiB | Peak disk incl. tables/margin/reserve GiB |
|---|---:|---:|---:|---:|
| Legacy INT8 | 5.3017 | 2.6530 | 2.6530 | 16.1087 |
| All five classes INT16 | 5.3017 | 5.3038 | 5.3038 | 21.4103 |
| Attention/dense/head INT16; embedding/shared INT8 (v1) | 5.3017 | 4.2101 | 4.2101 | 19.2228 |

Each row counts retained source shards, slices **and** one assembled package
set, per-stage tables, 512 MiB metadata/alignment/scratch allowance and 5 GiB
reserve. Atomic rename contributes no second bundle copy. Retaining whole and
split packages simultaneously adds another payload. Existing crash leftovers
reduce actual free space; no unrelated cleanup is authorized. FP32 expansion
is in RAM by default; a persisted FP32 cache adds its separately reported bytes
to disk. No original source is deleted by these offline checks.

RAM rows separately disclose conversion source/output buffers, row scales,
KV-B transpose copies, slice/writer buffers, reference FP32 parameter expansion,
a largest-reference-tensor transient and a 3 GiB runtime/OS margin. One-layer
FP32 parameters alone are 10.6034 GiB. The conservative reference-resident plan
also counts 5.3017 GiB of source residency and a 4.375 GiB largest FP32 temporary,
for **23.2800 GiB** available process headroom; this is a revised planning
assumption, not a measured minimum or guaranteed allocator bound. ARC exits
before reference loads. Concurrency needs combined budgets. Two/three-layer
reference expansion is much larger (74.1544/137.7055 GiB parameters alone), so
these synthetic graphs do not admit a larger real experiment.

The old 16.1087-GiB disk bound admits no INT16 plan. Fresh disk **and available
RAM** checks on Studio and the gaming PC remain required. The PC endpoint,
authorized connection profile and target volume are still unknown. No host
admission is inferred from installed RAM, historical snapshots or tiny RSS.

INT16 also has a conversion admission constraint: a **nonzero** row's maximum
absolute original BF16 value must be at least **2^-17** (approximately 7.6e-6)
and strictly below **2^30**. All-zero rows are supported separately. A row
outside that range rejects; it is not silently flushed, clamped or retried as
INT8. Small entries in an otherwise admissible row are not the same condition.
Row semantics include the converter's KV-B transpose.

The first real-tensor report must record the source/config hashes, complete
policy, tensor/class and total row count, with separate counts for all-zero,
nonzero below 2^-17, supported, at/above 2^30 and nonfinite rows, evaluated from
retained BF16 in conversion row order. Resource headroom does not prove these
counts are zero. Stop on unsupported/nonfinite rows and take any proposed flush
or other conversion-rule change to the owner before conversion proceeds. No
such real-row census or flush decision is supplied by the synthetic fixtures.

The [read-only BF16 census](bf16-row-census.md) now provides the retained-source
scan and synthetic converter/count controls. Its report distinguishes selected
INT8 and INT16 windows and native tensors; it is not admission approval. The
independent INT16/YaRN forward-execution oracle remains separate work.

After a later explicit admission, the existing one-layer commands in
`kimi-k26-yarn-assembly.md` must add `--keep-source` to streaming and the same
`--precision POLICY.json` to streaming and assembly, using fresh directories.
Reference config/index/source manifest and original shards 1 and 62 must remain.
The verified bundle's policy and scope must match the requested one-layer
probe; neither full K2.6 execution nor quality/timing/network results exist yet.
