# Offline original-layer reference comparison

This executes **synthetic, non-certifying** original embedding → original early
layers → original final RMSNorm/head graphs of depths 1, 2 and 3. Dense layer 0
is followed by MoE layers. It is not full Kimi-K2.6 or a quality/performance claim.
Tolerance policy remains **PROPOSED, not approved by TJ**. Numerical reports
never produce certification or a tolerance PASS.

## Implementations and provenance

ARC is pinned to **provisional/unreviewed** #168 `05afa5b068268860e4206307fb44c909645557ed`, with an
isolated diagnostic Cargo.lock. The production lock, engine and other PRs are
unchanged. The reviewed four-layer fixture generator produces the original
source; only declared experiment depth changes. Embedding, selected original
layers and head retain their original shard bytes/hashes, including packed INT4
experts. The one-layer ARC tensor hash remains
`fd1309caa28fdc3e58f7b12f3f9ccff5d897100d1e9252061e3e3e97573f3596`.

The official source/config retain revision
`7eb5002f6aadc958aed6a9177b7ed26bb94011bb`, reference SHA-256
`1fd8d198ff6ad69a5aec6fd85bf489d91ae2c432560b1e8ba7e34f710463c80a` and config SHA-256
`85825ca6e18cbe539eb83ee09eedfb3f4222265929f06e9f535a6d9364f55899`.
Apache-2.0 source/license/provenance are retained. AST extraction now includes
unmodified `MoEGate` and `DeepseekV3MoE`, with torch functional and NumPy in their
import environment. Execution uses torch 2.9.1 CPU FP32, one thread, eager
attention, `ep_size=1`, no reference cache. Actual official gate, group top-k,
routed/shared experts and combination execute; hooks only copy observations.
The exact-line generic-key scanner exception remains limited to the official
import-example false positive, with adjacent-credential controls retained.

Reference weight adaptation is explicit: BF16 dense/shared/router weights and
F32 correction bias become FP32. Compressed-tensors INT4 matrices are unpacked
from little-endian I32 words, eight low-to-high nibbles per word. A nibble stores
`q+8` (including -8); per-row/per-32-column BF16 scales produce `q*scale` in
FP32. `weight_shape`, packed/scales dimensions, storage dtypes and finite
nonnegative scales are checked. There is **no requantization** and no change to
original shard bytes. Every consumed component is recorded with its source
file hash, tensor name/shape/storage dtype. This is not a native INT4 GPU kernel
or BF16 arithmetic reference; FP32 values may vary slightly by host.

## Diagnostic observer isolation

Public `StageModel::open_range` captures each layer's actual output and logits.
The full pinned model runs alongside its one-layer stages: every stage output
hash must equal the corresponding full-model trace hash and final logits must
match exactly. The output boundary is after residual/FFN, before final norm.

Routing is private in the engine. `tools/quality-layer-probe/reference/model.rs`
is the unchanged pinned source (SHA-256
`15f2baef5e3db2a54ecba6831ee25d02570c84b3ce6026ba87cd3ca012af29eb`). `build.rs`
verifies that hash and generates a diagnostic-only module: module imports are
redirected to the pinned crate, its private I/O error helper is expanded with
identical formatting, doc comments are adapted, upstream in-crate tests are
omitted, and **one read-only observation is inserted after `combine`**. This
copies chosen experts, Q32 weights, Q16 router input and shared-expert output
into thread-local storage. No arithmetic, selection or accumulation is changed.
Every observer stage output/logit must also equal the unmodified pinned stage.
The observer is neither a production engine patch nor a new engine pin.

Every engine bump requires re-pinning the source hash and independent review of
all import/error-helper/test-truncation substitutions and the single insertion
anchor, followed by runtime neutrality checks. A feature-gated, read-only
engine routing hook remains follow-up work; this harness does not modify the
engine to introduce it.

## Alignment, routing and scales

Both implementations receive IDs `[1,42,7,3]`, positions `[0,1,2,3]`, causal
attention without padding and the exact declared graph/model/source/package
identity. The reference verifies source config/index/shards independently and
never reads ARC output values. Raw schema v2 adds ordered layer IDs and routing records; per-tensor SHA-256 binds
raw captures; missing layers and reordered layer/row values are rejected.
These consistency hashes are not third-party attestations or certification.

ARC activation/logit/shared/input values are signed int64 Q16 (`/65536`);
routing weights are Q32 (`/4294967296`). Reference values are FP32, scale 1.
Routing reports preserve each implementation's native selected IDs/order and
weights. Set disagreement, order disagreement and expert-weight L1 are separate
from boundary/logit errors; shared-output and router-input errors are also
reported. No common routing choice is imposed to improve apparent agreement.

Studio ARM measurements, tiny synthetic fixtures:

| Depth | Tensor | Max absolute error | Relative L2 |
|---|---|---:|---:|
| 1 | layer 0 | 0.030364275 | 0.012147774 |
| 1 | logits | 0.050032556 | 0.014210289 |
| 2 | layer 0 | 0.030364275 | 0.012147774 |
| 2 | layer 1 | 1.240849495 | 0.221161113 |
| 2 | logits | 1.452088594 | 0.243165966 |
| 3 | layer 0 | 0.030364275 | 0.012147774 |
| 3 | layer 1 | 1.240849495 | 0.221161113 |
| 3 | layer 2 | 2.327426434 | 0.350162367 |
| 3 | logits | 2.287083387 | 0.357204907 |

At position 3, layer 1 selects ARC `[0,4,1]` vs reference `[0,7,6]`; layer 2
selects ARC `[4,5,6]` vs reference `[4,5,2]`. Other positions' sets agree in this
fixture. These are observed differences, not isolated causal attributions.
Mean/RMSE/max-relative errors and all raw arrays are included in each report.
Relative error denominator floor is 1e-8. FP32 reference results are labeled per
host and **must never become golden-digest expectations**. Only integer ARC
captures are compared for cross-platform exactness.

`max_relative_error` is an unbounded elementwise diagnostic dominated by
near-zero reference elements, even for all-INT16. Do not use it in decision
text. Use relative L2 and `max_absolute_error_over_reference_l2` instead:
`max(abs(ARC-reference)) / max(norm(reference, 2), 1e-8)`, over the whole captured
tensor. Generated tables include those norm-based metrics and omit the
elementwise maximum. These four-position random fixtures establish **no ranking**
of precision classes. The attention-only promotion removes the observed routing
flip here; that does not establish its priority on real K2.6 weights.

## Reproduce without model downloads

Dependency/build setup may access package registries/GitHub; execution uses only
local inputs. Use Python 3.12 and a clean provisional-engine checkout (named reviewed-engine below for compatibility) at the pin:

```sh
python -m pip install -r scripts/arc_quality/layer_probe/requirements.txt
cargo build --locked --manifest-path ../reviewed-engine/Cargo.toml -p arc-inference --bin arc-mla
cargo build --locked --manifest-path tools/quality-layer-probe/Cargo.toml
export PYTHONPATH=scripts
export ARC_LAYER_DIAGNOSTIC="$PWD/tools/quality-layer-probe/target/debug/arc-quality-layer-probe"
for depth in 1 2 3; do
  python -m arc_quality.layer_probe.run --engine-source ../reviewed-engine \
    --arc-mla ../reviewed-engine/target/debug/arc-mla --diagnostic "$ARC_LAYER_DIAGNOSTIC" \
    --layers "$depth" --out "fixture-comparison/depth-$depth"
  ARC_LAYER_FIXTURE="$PWD/fixture-comparison/depth-$depth" python -m unittest arc_quality.layer_probe.test_comparison -v
done
python -m unittest discover -s scripts/arc_quality/tests -t scripts -v
```

Use fresh output directories. `capture` also supports existing verified 2/3-layer
synthetic bundles; real-weight capture remains restricted to one layer. The CI
Linux x86, macOS ARM64 and Windows x86 jobs each run all 21 depth/policy pairs
and upload separate artifacts. A dependent job downloads all three and compares
every ARC record from ARM64 and Windows against Linux using `cross_host.py`;
missing records or differing alignment, weights, tensors or routing fail the
job. FP32 values remain host-specific. No labels are changed; ordinary pushes
still cannot fetch weights. This matrix covers these CPU runners, not all
backends or macOS Intel.

Eighteen targeted tests run for each graph: honest alignment, actual reference
reexecution, native observer/full/stage equality, immutable one-layer integer
control, three routed experts, nonzero shared outputs, official MoE combination
with one **and two** shared experts, nibble -8/zero-scale decoding, model/input/
scope/position/depth mismatches, missing/reordered layer/row/routing captures,
nonfinite values, package/source corruption and no-output CLI rejection. The
62 existing certification/budget/McNemar regressions remain unchanged.

## Real-weight follow-up: not admitted or executed

1. Fresh free-space and RAM checks on both Studio and gaming PC, with target
   filesystem/time. PC access/volume is still unknown. Count crash leftovers;
   no cleanup, downloads or capacity assumption is authorized by this document.
2. Retain original official config, index, source manifest and **both original
   selected BF16 source shards** (1 and 62) for reference, in addition to ARC
   slices/package. Do not use the stream-and-delete source policy for this run.
   Their total is 5,692,637,048 bytes. All pins come from the reviewed source
   manifest; no source/config substitution or synthetic identity for real weights.
3. After a later admitted fetch, use pinned #168 `slice --layers 0:1` and
   `slice-manifest --layers 0:1`, then `slice-assemble-yarn --probe-layers 1` and
   `verify`, as in `docs/protocol/kimi-k26-yarn-assembly.md` on that pinned engine.
   Preserve original official config depth 61; the finalized manifest explicitly
   declares the early-layer-with-original-head probe.
4. Run the comparison below on the already-present source and one-stage bundle.
   `tokens.json` is a JSON array of agreed token IDs. Start with four tokens;
   positions are 0..N-1, causal/no padding. No tokenizer/chat equivalence claim.

Expected stop condition for INT16 conversion at this pin: a nonzero row with
maximum absolute magnitude below `2^-17` is outside the supported row-magnitude
window and can be rejected. Preserve the conversion error and log tensor/row,
magnitude, policy and engine pin when available; report the run as stopped before
comparison, not as a quality result or a newly discovered converter defect.
Do not silently rescale, clamp or substitute a policy to proceed.

The current real probe has no MoE layer, so routing margins are not applicable.
The first admitted real **MoE** report must include the selection-score margin
between the 8th and 9th expert for each flipped position, on each implementation,
with its layer, selected IDs, score units, correction-bias/group eligibility and
tie rules. Counts alone are insufficient. This requires future score capture;
selected weights in the current records cannot reconstruct that margin.

```sh
PYTHONPATH=scripts python -m arc_quality.layer_probe.capture \
  --bundle first-light-probe --source-dir first-light-source \
  --source-manifest ../reviewed-engine/docs/protocol/packages/kimi-k26.source.json \
  --original-source-dir first-light-source \
  --original-source-manifest ../reviewed-engine/docs/protocol/packages/kimi-k26.source.json \
  --tokens tokens.json --diagnostic tools/quality-layer-probe/target/debug/arc-quality-layer-probe \
  --out first-light-reference
```

This command runs only the original dense layer 0 plus original head; the current
reference adapter permits later/MoE layers only for synthetic fixtures. It accepts the full original source
configuration, instantiates only layer 0, and records original depth 61 in the
request. Real execution of this command remains unverified. No diagnostic hook
is missing for this one-layer path. Real-weight MoE/multi-layer scaling, padded/arbitrary-position,
cache/reference BF16 compute, tokenizer/chat, full model, quality certification
and lab internet pipeline need separate work and evidence.

Disk planning (not measured peaks): slices W=2,848,653,632 bytes; package about
W + 1,048,576 canonical-table bytes; retained reference sources S=5,692,637,048.
Allow 512 MiB headers/temporary margin plus 5 GiB reserve. Keeping slices through
comparison requires `(S + 2W + tables + margin + reserve) = 16.1087 GiB`.
Removing only newly produced verified slices after assembly would reduce the
later retained footprint to 13.4557 GiB, but does not eliminate the assembly
peak. This pass performs no such removal and admits no fetch. The earlier
31.486 GiB Studio snapshot is historical; gaming PC capacity remains unknown.

RAM planning: original one-layer graph has 2,846,317,568 parameters, requiring
10.6034 GiB for FP32 embedding/layer/norm/head alone. The largest source BF16
matrix is 2.1875 GiB; source mappings, library allocations, activations, JSON and
OS add to this. File verification is streamed once per shard; loaded layer-state
copies are released. ARC export exits before reference construction, so ARC
weights are not deliberately kept resident with PyTorch. Budget a **23.2800 GiB conservative available-process-headroom plan**
(including source residency, FP32 parameters, a largest FP32 temporary and reserve),
then measure RSS; this is not a measured minimum or admission; 16-GB
physical RAM is not demonstrated sufficient. Long sequences add quadratic eager
attention memory and potentially large vocabulary logits/JSON. The four-token
fixture's measured peak RSS (~216.3 MiB, macOS `/usr/bin/time -l`) cannot predict
real-weight peak. There is no real-layer timing or quality result in this PR.

## Provisional precision matrix

The dependency incorporates unreviewed #156 `596a61f6`; neither that independent
review nor cumulative #168/#164 review is closed by these fixture results.
The isolated lock changes only git revision pins. The observer source is copied
byte-for-byte from the exact engine revision and hash-checked before compilation.
No production lock, engine arithmetic or other PR is changed.

`run --policy` accepts `legacy`, `int16`, or a single class: `attention`, `dense`,
`shared`, `embedding`, `head`. Legacy omits engine precision (request explicitly
records null); all-INT16 promotes the five classes; each single-class control
sets that class INT16 and all other classes INT8. Native INT4 experts, router,
norm and bias retain their representations. No legacy slice is reused under a
new policy. The same complete policy is passed before conversion, manifest
construction and canonical YaRN assembly. Capture requests include the complete
policy; the actual diagnostic rejects missing or mismatched policy against its
verified package. Generic `capture --precision POLICY.json` likewise requires a
caller policy matching the manifest; omitted means legacy.

The reference always reads retained **original** source weights, never ARC's
converted/dequantized package. The matrix checks original weight provenance,
FP32 tensors and FP32 routes are identical across policies on each host. Every
ARC policy must retain identical native expert/norm/router package bytes. It
also checks shared promotion is inactive at depth 1 and head promotion leaves
upstream boundaries unchanged. The original legacy tensors/routes at all three
depths remain equal to the earlier capture; their original provenance is retained.

```sh
# CPU dependencies are those in layer_probe/requirements.txt; no model downloads.
# ENGINE is a clean checkout of 05afa5b068268860e4206307fb44c909645557ed.
cargo build --locked --manifest-path "$ENGINE/Cargo.toml" -p arc-inference --bin arc-mla
cargo build --locked --manifest-path tools/quality-layer-probe/Cargo.toml
PYTHONPATH=scripts python -m arc_quality.layer_probe.matrix \
  --engine-source "$ENGINE" --arc-mla "$ENGINE/target/debug/arc-mla" \
  --diagnostic tools/quality-layer-probe/target/debug/arc-quality-layer-probe \
  --out precision-evidence
PYTHONPATH=scripts python -m arc_quality.layer_probe.cross_host \
  studio-evidence x86-evidence cross-host.json
```

This executes 21 paired captures and 18 regressions per capture, retains original
inputs, policies, packages and both raw outputs, and writes per-layer absolute,
relative/RMSE errors, per-position final-logit top-1 IDs/agreement and native
routing differences. `matrix.json` reports `(all - legacy) - sum(single - legacy)`
for every tensor to expose nonadditivity; no ranking or additive quality claim
is inferred. Small fixtures do not select a production precision policy.

Resource measurements use one fresh child for each implementation, Unix
`getrusage` maximum RSS (bytes on macOS, KiB converted to bytes on Linux) and
wall/user/system seconds. Windows records wall seconds with RSS and CPU times
explicitly null (tables say "not measured"); no sampled RSS is passed off as a
peak. ARC diagnostic uses a debug build and verifies whole,
split and observed forwards; FP32 includes interpreter/torch imports, weight
load, forward and serialization. Local conversion uses a release binary and
CI uses debug; conversion is outside the measured children. OS cache is
uncontrolled, there is one sample, and shapes are tiny. These are host-specific
whole-process setup measurements, not matched inference throughput or real-model
RAM estimates. Cross-host comparison requires exact ARC raw records while
retaining each host's FP32 outputs/errors; FP32 equality is never a golden rule.

One-layer disk planning remains **16.1087 GiB legacy / 21.4103 GiB all-INT16 /
19.2228 GiB the earlier mixed policy** (the latter is not a single-class control).
Retained sources, slice/assembly copies, scratch, reference expansion and reserve
must all be budgeted for the selected policy. The conservative sequential
reference RAM envelope is **23.2800 GiB**, not measured admission or a proven
minimum. Two/three-layer reference expansion is much larger, so fixture depths
do not authorize a larger real plan. Before a real run: fresh disk and available
RAM on both hosts, gaming-PC access profile/target volume, explicit admission,
retained original source/reference bytes, same requested graph/tokens/positions,
and independently reviewed dependencies. No new weight fetch or paid call occurs.

## Historical evidence log labels

In the reviewed Studio evidence archive, `studio/[123]-*-tests.log` contains
superseded 17-test runs. Only `depth-*-final-tests.log` is the final 18-test
rerun evidence cited in the review. Do not add those two sets when counting
tests. This labels the old archive; it does not rewrite its raw evidence.
Fresh CI matrix artifacts have one test log per depth/policy in a new output
directory, so their `[123]-*-tests.log` files are current, not superseded.
