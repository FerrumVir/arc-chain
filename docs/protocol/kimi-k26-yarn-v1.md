# K2.6 versioned deterministic YaRN prerequisite

This defines new profiles; it does not change either existing MLA v1 identity,
operator, table byte or golden. Source/config provenance is pinned in
`reference/kimi-k26/provenance.json`. Only metadata/reference code was fetched.
There is no real-weight forward, speed or quality measurement in this change.

## Source and equations

The official [configuration](https://huggingface.co/moonshotai/Kimi-K2.6/blob/7eb5002f6aadc958aed6a9177b7ed26bb94011bb/config.json)
and [reference](https://huggingface.co/moonshotai/Kimi-K2.6/blob/7eb5002f6aadc958aed6a9177b7ed26bb94011bb/modeling_deepseek.py)
are at immutable revision `7eb5002f6aadc958aed6a9177b7ed26bb94011bb`.
Their SHA-256 values are checked/preserved in the repository. The reference
formulas appear at lines 245–339 and 693–699. The source config pins dimension
64, base 50000, factor 64, original context 4096, beta fast/slow 32/1, and both
mscale values 1. Only this parameter tuple is implemented; the official
configuration entry point requires its exact SHA-256, not a loose model name.

For frequency index i in [0,32), the correction limits are floor/ceil of
`64 * ln(4096 / (rotations * 2*pi)) / (2*ln(50000))`, giving **8 and 20**.
Let `r = clamp((i-8)/12, 0, 1)`. The frequency is
`50000^(-2*i/64) * (1-r+r/64)`.
The reference rotary magnitude ratio is
`(1+0.1*mscale*ln(64))/(1+0.1*mscale_all_dim*ln(64)) = 1`.
The attention scale is separately multiplied by
`(1+0.1*ln(64))^2`: the Q30 multiplier is
`floor(2^30 * (1+0.1*ln(64))^2 / sqrt(192)) = 155348565`.
It would be incorrect to omit that multiplier or apply it twice to RoPE.

## Canonical preparation

The 32 frequencies are rounded half away from zero to Q62; the committed
integer list is normative. The independent generator uses Decimal precision
90 to derive it. Rust uses no floating point in table preparation: each
frequency feeds the existing Q62 Taylor cos/sin and rotation recurrence,
then Q16 half-away output rounding. Context is restricted to 1..262144.
The pinned 4096-position tables segment BLAKE3 is
`0085d663f9d7284af6105325f57a9584c498c2e673f1df81b970e40f29c28e70`.
The tables segment is LE i32 cosine bytes followed by LE i32 sine bytes,
position-major, 32 entries per position per table, excluding file padding.

`yarn_reference.py` independently evaluates analytic sin/cos (Decimal Taylor
with argument reduction), not Rust's rotation recurrence. Its 224 sample
pairs cover all frequencies at positions 0, 1, 4095, 4096, 8191, 32768 and
262143. Rust matches their Q16 integers exactly. Binary64 evaluations of the
official mathematical formula differ by at most 0.5001 Q16 units in these
samples. These are **ARC-generated formula-derived vectors**, not an execution
of the official PyTorch model. The official float32/device and BF16 rounding
paths are not claimed bit-identical to this integer profile. Real reference
activation/logit error remains a later measurement.

## Identities and package consumption

The model object gains `preparation` only for the new profiles, including the
scheme `arc.kimi-k26.yarn-q62-preparation.v1`, exact official config SHA, and
an explicit scope. Absence retains the exact original model JSON and profile.

| Scope | Profile | Meaning |
|---|---|---|
| `full_kimi_k26` | `arc.kimi-k26.mla-moe.i4g32.yarn.q16.v1` | Official 61-layer shape. Finalization requires embed, all 61 layers and head. This is a preparation identity, not certification of a real run. |
| `early_layers_with_head_probe`, layers 1..3 | `arc.experimental.kimi-k26.early-layers-head.i4g32.yarn.q16.v1` | Original embed → layers [0,N) → original norm/head. Remaining layers are omitted intentionally. No complete K2.6 generation or quality claim. |
| `synthetic_fixture` | `arc.synthetic.kimi-k26-yarn.i4g32.q16.v1` | Tiny random weights, explicit `arc-test/kimi-k26-yarn` architecture; same YaRN constants, never real-weight evidence. |

Full/probe validation compares the complete model shape to the pinned official
shape; only a probe's layer count may change. The preparation participates in
the model root, package bytes and manifest hash. Profile downgrade, altered
attention scale, malformed preparation and mismatched tables fail closed.
StageModel recomputes the new tables and checks their bytes before accepting
a new-profile package; this adds preparation cost and transient table memory
(on the order of another pair of tables), not a measured runtime speedup.

## Manifest finalization and CLI

```
arc-mla yarn-prepare --config docs/protocol/reference/kimi-k26/config.json \
  --max-seq 4096 --probe-layers 2 --out preparation.json \
  --slice-manifest pending-slices.json --manifest-out probe-stage-manifest.json
```

Without `--probe-layers`, the scope is full K2.6. Without `--slice-manifest`,
the command emits preparation metadata only and cannot claim a finalized
weight manifest. Legacy `convert` still refuses the pending checkpoint.
The caller's pending manifest is never modified or cleared.

`finalize_pending_slices` checks the existing canonical manifest hash, legacy
INT4 profile, exact pending YaRN marker, null unprepared fields, packed/prefix
contract and pinned official source metadata. It checks segment uniqueness
and sizes against the original layout, selects the explicitly requested
scope, requires every selected segment in canonical order, validates digest
syntax, computes the new tables commitment and builds a stage manifest/root.
The output uses the existing stage-manifest schema with a new profile and
contract; it does not masquerade as a legacy prepared slice manifest.

This operation commits **declared weight digests**. It does not authenticate
unseen converted weight bytes. An assembler must rehash every selected slice
and segment, then call the existing package/manifest verifier before
execution. Missing embed/head, incomplete full-model coverage, substituted
source or malformed commitments are rejected. Tokenizer metadata remains
explicitly `not_prepared`; token-ID probes do not establish text serving.

## ARC-72 integration and next real-layer work

#168/#166/#167 are unchanged. Their old compiled consumers intentionally do
not understand these new profiles. A subsequent #168 integration must:

1. Reconcile MlaConfig's optional preparation and the shared text-shape
   parser with its wrapper/packed loader; preserve the legacy pending path.
2. Use this explicit finalizer after conversion, and `yarn::tables` instead
   of plain `rope_tables` when assembling the new profiles. Verify source,
   slice and segment bytes, then verify the final package against the new
   stage manifest. Do not just clear `pending` in an old slice manifest.
3. Supply approved tokenizer provenance for text use; token-ID probes can
   precede that. No full-model root may be inferred from early-layer records.
4. After the two-machine disk gate and separately dispatched bounded fetch,
   assemble the declared early-layer/head probe from real slices. Establish
   x86/ARM real-weight goldens, same-input official activation/logit errors,
   Studio layer/token timing, and only then the measured internet pipeline.

The newly pinned synthetic engine golden covers scalar 1/3 threads and SIMD,
manifest verification, canonical-table checking and downgrade rejection.
INT4 experts remain scalar in both modes; INT8 projections use SIMD.
Old golden constants remain unchanged. No new real-weight fetch or rerun is
needed for this prerequisite. The older Moonlight proof and reserved macOS
coverage remain separate gates; no new claim is made for them.
