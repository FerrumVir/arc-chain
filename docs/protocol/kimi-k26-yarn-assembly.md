# Explicit YaRN assembly from verified slices

This path integrates the ARC-66 preparation contract reviewed at
`6aa65ba3ed1499b970ac823dc58f50f878175f7e` into ARC-72. The shared shape parser
retains packed INT4, `language_model.` prefix and vision exclusion support;
legacy `slice-assemble` and `convert` still refuse pending YaRN. Existing
profile/golden identities remain unchanged.

## Trust and execution boundary

`slice-assemble-yarn` takes an independently supplied source manifest, its
exact pinned config bytes, the hashed pending slice manifest, slice directory,
and explicit scope. The default is the pinned official full model;
`--probe-layers 1..3` is the separate experimental early-layers-plus-original-head
identity; `--fixture` is exclusively an `arc-test/` synthetic source using the
pinned YaRN equations/head dimensions. Fixture and probe flags are exclusive.
The fixture path cannot acquire an official full/probe profile.

Preparation checks the source/config pin, source identity, pending state,
weight storage, full source shape, selected segment order/coverage and scope.
It generates canonical YaRN tables and a separate finalized stage manifest;
the original pending manifest is never cleared or edited. A fresh output
directory is required. `--stages N` partitions all selected layers contiguously
and includes embedding/head at the appropriate ends.

Assembly tees the actual bytes delivered to each StageWriter through the
existing lossless SegmentSlicer in hash-only mode. Reconstructed slice records
(names, ranges, tensor metadata, sizes, hashes and order) and segment hashes
must equal the selected commitments. This verifies the copied bytes, rather
than trusting a pre-check followed by reopening potentially changed files.
The finished file is independently SHA-256 hashed and compared with the
writer's digest; its parsed header and verified segment hashes are then checked
against the finalized manifest. No execution occurs during assembly.

All stages, `manifest.json` and `report.json` are created in a private sibling
staging directory. Failure removes that directory. Only a successful complete
bundle is renamed to the requested fresh directory. Existing output is refused.
A crash may leave a hidden staging directory; it is not a published bundle.
The finalized manifest commits the whole model root, while per-stage full-file
digests are recorded in `report.json` (its `full_package` field stays null).
`arc-mla verify --manifest ...` verifies segments; `--full-digest` requires a
separately pinned full-package manifest and is not used for these bundles.

The externally supplied source/slice manifests are trust inputs. Rehashing
checks byte integrity and binding, not publisher authentication of arbitrary
replacement manifests. Official full/probe scopes additionally require the
immutable official source/config identity. Slice conversion already verified
original shard hashes; assembly does not require those source shards again.

## Offline regression and cross-platform evidence

Run `scripts/arc_mla/slice_checks.sh BIN WORK EVIDENCE` with fresh directories.
It preserves all previous plain/edge/pending fixtures and adds
`check_yarn_slices.py`: a distinct packed, prefixed, vision-excluded synthetic
YaRN fixture, lossless segment preservation, scalar/SIMD runs and 1/2/4-stage
execution. Tokens, every logit hash, boundaries and roots must agree.
Corruption tests reject altered/truncated/appended/missing files, reordered or
missing records, invalid tensor metadata/digests, source/config/scope changes,
and output reuse, including failures after staging starts. Post-assembly
package corruption is independently rejected by the engine verifier.

The Rust `official_full_and_probe_preparation_controls_keep_scope_and_source`
test checks full/1/2/3-layer official metadata and equivalence with ARC-66's
reviewed roots/segments. Those full/probe controls allocate no real weights;
only the separate synthetic profile executes. CI runs the fixture path on
Linux x86-64, Linux ARM64 and Windows x86-64. Formula-derived vectors are not
an official PyTorch model execution.

## Remaining real-weight commands (NOT executed in this pass)

First complete fresh capacity checks on both Studio and gaming PC, including
source, retained slices, assembly output and reserve. The PC target/access
information remains missing. The last Studio snapshot is historical, not a
reservation. Do not fetch until a subsequent dispatch admits the exact plan.

After that gate, for a **one-layer experimental probe** in fresh directories:

```sh
cargo build --release --locked -p arc-inference --bin arc-mla
python scripts/arc_mla/stream_slices.py --arc-mla ./target/release/arc-mla \
  --source-manifest docs/protocol/packages/kimi-k26.source.json \
  --work first-light-source --out first-light-slices \
  --layers 0:1 --embed --head --expert-groups 48 --keep-source --report first-light-stream.json
./target/release/arc-mla slice-assemble-yarn \
  --config first-light-source/config.json \
  --source-manifest docs/protocol/packages/kimi-k26.source.json \
  --manifest first-light-slices/manifest.json --slices first-light-slices \
  --probe-layers 1 --stages 1 --out-dir first-light-probe
./target/release/arc-mla verify --package first-light-probe/stage-0.arcspkg \
  --manifest first-light-probe/manifest.json
./target/release/arc-mla golden --package first-light-probe/stage-0.arcspkg \
  --cases scripts/arc_mla/kimi_layer_probe_cases.json --kernel scalar \
  --threads 1 --out first-light-scalar.json
./target/release/arc-mla golden --package first-light-probe/stage-0.arcspkg \
  --cases scripts/arc_mla/kimi_layer_probe_cases.json --kernel simd \
  --threads 4 --out first-light-simd.json
./target/release/arc-mla stage --package first-light-probe/stage-0.arcspkg \
  --manifest first-light-probe/manifest.json --run first-light-scalar.json \
  --kernel scalar --threads 1 --out first-light-boundary.bin \
  --report first-light-stage.json
```

Use the identical cases/package on x86 and ARM and compare tokens, logits,
boundaries and model roots; reported timings are real measurements only once
these commands actually run on real slices. For two/three early layers change
both `--layers 0:N` and `--probe-layers N`; use a fresh bundle and revised budget.
`--stages N` creates separate packages; replay in order with each prior `--out`
as the next `--input`, as demonstrated by `check_yarn_slices.py`.

The token probe intentionally makes no tokenizer/chat correctness claim. A
same-input official reference execution (matching the explicit truncated-layer
semantics), activation/logit error capture, and approved quality tolerances
remain to be implemented/measured. This pass supplies no command that pretends
to certify those unavailable results. Real Studio layer/token timings and the
lab internet pipeline come after those comparisons. No full K2.6 forward or
59 tok/s claim follows from an early-layer probe.

Disk arithmetic: at context 4096 the canonical tables require 1,048,576 bytes
per stage. Retained selected weight slices require 2,848,653,632 bytes for one
layer plus embedding/head; 12,512,505,856 for two; 22,176,358,080 for three.
During assembly budget twice that retained-weight count, plus tables per stage,
512 MiB metadata/header/temporary margin, and 5 GiB reserve. Atomic publication
renames rather than copies the staging bundle; no second package copy is
assumed. Download bounds additionally include the largest live source shard
(4,697,635,160 bytes for the one-layer plan, 9,809,047,464 for two/three).
These are planning bounds, not measured peaks, and do not budget extra package
copies, reference weights or reference-runtime scratch/RAM. Those need separate
admission. Historical real-shard results remain at `7a952ec7`, run 37622036049.

Precision policies and revised retained-source disk/RAM budgets are specified in
`kimi-k26-precision-integration.md`. The legacy command above omits precision;
INT16/mixed use the same explicit policy during conversion and assembly.
That document also requires a real BF16 row-range census: nonzero INT16 rows
must satisfy `2^-17 <= max(abs(w)) < 2^30` after semantic transpose; all-zero
rows are supported. Unsupported/nonfinite rows stop conversion pending an
owner decision; no flush rule is approved. Complete the independent Python
INT16/YaRN execution oracle before real-weight admission.
