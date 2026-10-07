# Offline original-layer reference comparison

This is an executed **synthetic, non-certifying** comparison, not a complete
Kimi-K2.6 forward or quality/performance claim. It runs original embedding →
original dense layer 0 → original final RMSNorm/head, omitting later layers.
Tolerance policy remains **PROPOSED, not approved by TJ**. A numerical error
report never produces certification or a PASS verdict.

## Implementations and provenance

- ARC engine: immutable `b0673684a8c16f83b318fe1a33ab6b33f3dc79df` (#168).
  The isolated `tools/quality-layer-probe` crate pins it in its own Cargo.lock.
  The production workspace lock and #156/#168/#166/#167 are unchanged.
- Reference: actual unmodified dense decoder definitions from official
  `moonshotai/Kimi-K2.6` revision `7eb5002f6aadc958aed6a9177b7ed26bb94011bb`,
  retained under `scripts/arc_quality/layer_probe/reference/` with Apache-2.0
  notice, config, hashes and provenance. `NOTICE.md` documents AST selection.
  The secret scanner exempts only the exact official docstring import line at
  this path (a generic-key false positive); adjacent credential-shaped input
  remains detected. The official file is not edited to suppress the scanner.
  This executes PyTorch, not formula-only vectors or fabricated ARC records.
- Reference compute: torch 2.9.1, CPU, one thread, eager attention, source BF16
  weights decoded to FP32, deterministic algorithms, no cache. ARC uses its
  unchanged integer forward and cache. Both receive identical token IDs,
  contiguous positions starting at zero and causal attention without padding.
- The fixture starts with the reviewed four-layer synthetic source generator.
  Only the declared depth changes to one. The source shard hashes prove that
  embedding, layer 0 and original head are unchanged. No model fetch occurs.
  The synthetic package has its own finalized fixture identity, not full/probe.

The diagnostic uses existing public `StageModel::forward`: the returned hidden
vector is **after layer 0, before final RMSNorm**; logits follow final RMSNorm and
original head. There is no added engine hook and no numerical/golden change.
Raw ARC activations/logits are signed int64 Q16 (`real = integer / 65536`).
Raw reference values are FP32, scale 1. Each sequence row maps to the corresponding
position/token. JSON records include shape, model root, scope, graph, package,
source, request and implementation hashes. The reference independently verifies
its source config/index/shards, and executes without reading ARC output values.

## Reproduce without model downloads

Dependency/build setup may access package registries/GitHub; runtime inputs are
local. Create a Python 3.12 environment and install the pinned requirements
(`torch==2.9.1` CPU wheel on Linux). Use a clean checkout of the pinned engine.
From this PR checkout:

```sh
python -m pip install -r scripts/arc_quality/layer_probe/requirements.txt
cargo build --locked --manifest-path ../reviewed-engine/Cargo.toml -p arc-inference --bin arc-mla
cargo build --locked --manifest-path tools/quality-layer-probe/Cargo.toml
export PYTHONPATH=scripts
export ARC_LAYER_DIAGNOSTIC="$PWD/tools/quality-layer-probe/target/debug/arc-quality-layer-probe"
export ARC_LAYER_FIXTURE="$PWD/fixture-comparison"
python -m arc_quality.layer_probe.run --engine-source ../reviewed-engine \
  --arc-mla ../reviewed-engine/target/debug/arc-mla \
  --diagnostic "$ARC_LAYER_DIAGNOSTIC" --out "$ARC_LAYER_FIXTURE"
python -m unittest arc_quality.layer_probe.test_comparison -v
python -m unittest discover -s scripts/arc_quality/tests -t scripts -v
```

Use fresh output directories; failures do not publish a comparison report.
`run` preserves original/reduced source fixtures, verified slices and assembled
package, command logs, request, raw ARC/reference tensors and comparison.json.
The generic `capture` command runs the same comparison on an existing verified
one-layer bundle, including an explicit real-weight probe identity when admitted.
It never downloads weights. Windows binaries have the usual `.exe` suffix.

`quality-harness.yml` adds one CPU x86 fixture job and uploads all inputs/raw
outputs. The old multi-gigabyte proof jobs now require an **explicit labeling
event**; retaining the existing quality-proof label does not fetch weights on a
push. Existing harness tests and proof job bodies remain intact.

## Measurement and rejection

Studio ARM, synthetic width 64/vocabulary 300, token IDs `[1,42,7,3]`, positions
`[0,1,2,3]`; original fixture had four layers, experiment executes only layer 0:

| Tensor | Values | Max absolute | Mean absolute | RMSE | Relative L2 | Max relative |
|---|---:|---:|---:|---:|---:|---:|
| post-layer 0 | 256 | 0.030364275 | 0.006815758 | 0.008613549 | 0.012147774 | 1.033166414 |
| logits | 1200 | 0.050032556 | 0.010940573 | 0.013755598 | 0.014210289 | 33.363512212 |

Relative component error uses `abs(ARC-reference)/max(abs(reference),1e-8)`;
relative L2 uses the reference vector norm with the same floor. Large component
relative error near zero is disclosed, not hidden by a tolerance or certification.
Neither argmax agreement nor these errors establish quality equivalence.

Ten integration tests (with mutation subcases) consume actual generated raw
records: honest alignment and non-certification; model/input/scope/position/mask/
graph mismatch; missing tensors/positions/width; nonfinite, boolean, fractional
or overflowing Q16 values; scale/provenance mismatch; malformed request bytes;
actual diagnostic CLI rejection without output; truncated packages and missing/altered reference source/config/index; and reexecution of official
forward with identical original source tensors. Existing 62 harness tests retain
missing-PPL, prompt mismatch, coverage/provenance, budget override and large-count
McNemar regressions. The existing smoke report stays non-certifying.

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
reference adapter refuses later/MoE layers. It accepts the full original source
configuration, instantiates only layer 0, and records original depth 61 in the
request. Real execution of this command remains unverified. No diagnostic hook
is missing for this one-layer path. MoE/multi-layer, padded/arbitrary-position,
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
weights are not deliberately kept resident with PyTorch. Budget **at least
16 GiB available process headroom**, then measure RSS before admission; 16-GB
physical RAM is not demonstrated sufficient. Long sequences add quadratic eager
attention memory and potentially large vocabulary logits/JSON. The four-token
fixture's measured peak RSS (~216.3 MiB, macOS `/usr/bin/time -l`) cannot predict
real-weight peak. There is no real-layer timing or quality result in this PR.
