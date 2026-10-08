# Experimental deterministic BF16 matrix precision, v1

This #156 prerequisite adds **code and synthetic-fixture support**, not a
shipping precision decision. There is no real K2.6 forward, API-quality
assessment, or Kimi speed claim. INT8 vs INT16 still needs the same retained
real BF16 source tensors and reference inputs; tolerance policy is unapproved.
Native published INT4 routed experts must remain losslessly packed by #168.
This branch's older BF16 converter can *generate synthetic* INT4 test experts;
that is not permission to requantize the published K2.6 experts.

## Capability map

| Tensor class | Previous representation | Explicit new option | Selection / package entries |
|---|---|---|---|
| MLA attention: direct query, query LoRA A/B, KV A, transposed KV B key/value, output | INT8 per-row dyadic | INT16 per-row dyadic | `attention`; `wq*`, `wkv_a`, `wk_b`, `wv_b`, `wo` |
| Dense FFN gate/up/down | INT8 per-row dyadic | INT16 per-row dyadic | `dense`; layer `w_gate/w_up/w_down` |
| Shared experts gate/up/down | INT8 per-row dyadic | INT16 per-row dyadic | `shared`; `shared.w_*` |
| Token embedding | INT8 per-row dyadic | INT16 per-row dyadic | `embedding`; `embed` |
| Original LM head | INT8 per-row dyadic | INT16 per-row dyadic | `head`; `lm_head` |
| Routed experts | Published INT4, group-32 BF16 scales (or legacy synthetic INT8 profile) | Unchanged; no precision override | `experts.*.q4/.s` remain byte-identical |
| Router gate | Already signed INT16 with row power-of-two scale | Unchanged | `router.q/.k`; existing seven-binade contract |
| Routing correction bias | Already signed 64-bit Q32 | Unchanged | `router_bias` |
| Attention/LoRA/FFN/final norm gains | Already signed 64-bit Q16 | Unchanged | `*_norm` and `final_norm` |
| Activations, cache, RoPE, nonlinearities | Existing Q16 / integer tables | Unchanged | Existing domain and floor rules |

Norm and bias *storage width* already exceeds INT16; this is not a claim that
Q16/Q32 captures every BF16 exponent or meets the quality bar. Their existing
round-half-away conversion and magnitude checks remain. No norm is silently
narrowed to 16 bits. Router quality and exponent-range limits also remain.

## Canonical identity and serialization

Omit `model.precision` for the exact legacy profiles and identities. Explicit
policies contain **all** these fields, no others:

```json
{"version":1,"attention":"int16","dense":"int16","shared":"int16","embedding":"int16","head":"int16"}
```

Each class accepts `int8` or `int16`. Even an explicit all-INT8 policy has a new
identity: explicit policy and omission are distinct. Unknown versions, fields,
nulls, omissions and dtype names fail closed. Experts/norms cannot be overridden.
`MlaConfig.precision: Option<Precision>` is validated by the normal model parser.
`config.profile()` maps each old profile to its `mixed-dyadic-row` v1 counterpart;
full K2.6, early-layers-plus-original-head probe and synthetic YaRN profiles
remain distinct. Policy fields are in the canonical model preimage, header,
manifest body and model root. A profile downgrade or dtype/precision mismatch
is rejected by the package parser. No legacy golden was replaced.

`package::layout` selects `.q` I16 (signed **little endian**, two bytes/element)
for the chosen classes. `.mu` stays I32 and `.k` stays U8, one per row. Tensor
shapes, semantic rows and KV-B transpose order are unchanged. mmap reads use
explicit byte decoding, with no native-endian or alignment assumption.

## BF16 conversion, arithmetic and domains

`precision::quantize_row` decodes BF16 bits exactly (including signed zero and
subnormals); infinity and NaN reject. For row absolute maximum A > 0:

- `q_j = sign(w_j) floor(32767 |w_j| / A + 1/2)`; ties away from zero,
  range [-32767,32767]. This is calibrated directly from BF16, never an
  upcast of already-quantized INT8. `-32768` is invalid.
- Write A = m * 2^e using the exact BF16 significand. Choose the smallest
  nonnegative t such that `m * 2^t >= 32767 * 2^30`.
- `mu = floor(m * 2^t / 32767 + 1/2)`. If mu = 2^31, set mu=2^30 and t=t-1.
  `k=t-e`. Require `mu in [2^30,2^31)` and `k in [16,62]`.
  Unsupported row magnitudes **reject**, not clamp or wrap.
- An all-zero row is exactly q=0, mu=0, k=16. Loader rejects zero-scale rows
  containing nonzero weights. Tiny entries far below a row maximum may round
  to zero; INT16 conversion is not claimed lossless.
- Projection: exact signed dot d, then `floor(d*mu / 2^k)` using the existing
  i128 dyadic epilogue and activation bound. Embedding: `floor(q*mu/2^(k-16))`.
  Negative right shifts are arithmetic floor, not truncation toward zero.
- Conservative input guard: `sum(abs(x)) < floor(2^63/32767)`. It is checked
  before any i64 accumulation; intermediate signed products/sums cannot wrap.
  Existing output bound (absolute Q16 integer <= 2^62) remains enforced.

Scalar decodes INT16 little-endian words. SIMD decomposes each weight exactly
into three signed base-128 limbs (`q = a + 128*b + 16384*c`) and calls the
existing AVX2/ARM NEON exact INT8-dot kernels for each limb. Signed remainder
and truncating division preserve exact reconstruction for negative weights.
No activation requantization is introduced. Unsupported SIMD/input domains
use the scalar implementation; a SIMD flag alone is not an acceleration claim.
The implementation materializes at most 64 rows of three weight limbs per
projection. This is a correctness prerequisite, not a tuned INT16 kernel.

## Reproduction and fixture evidence

```sh
cargo test --locked -p arc-inference --lib modern::mla -- --nocapture
cargo clippy --locked -p arc-inference --all-targets --features candle -- -D warnings
cargo build --release --locked -p arc-inference --bin arc-mla
python scripts/arc_mla/check_int16.py target/release/arc-mla int16-evidence --build-label release
```

The CLI accepts `convert --precision policy.json`; omitted precision preserves
legacy behavior. Bad schema and missing/option-as-value precision flags reject
before package creation. The offline script builds deterministic tiny BF16
weights (4 layers, dense0 then MoE1–3, width64, vocab300), converts legacy,
all-INT16-BF16 and mixed policies, verifies manifests and compares scalar/SIMD
and four separately converted stage packages. It retains exact commands, raw
runs, generated source, packages and OS-specific peak-RSS/time logs. Process
wall includes startup; internal prefill/decode timing is separately available.
Fresh process per operation; filesystem caches may be warm; one Rayon thread.
Windows retains golden/unit coverage; OS peak RSS is unavailable in this script
on Windows and is reported null rather than estimated.

`reference/int16-fixture-goldens.json` pins package, generation and model-root
identities for a dense-only fixture and three dense+MoE fixtures (including
canonical synthetic YaRN). Generation hash
commits tokens, every logits hash and every layer-boundary digest. The same
constants execute in x86 Linux, ARM Linux and Windows CI; Studio runs the ARM
checks locally. Existing seven legacy generation goldens and YaRN identity
remain pinned separately. Full/probe/fixture model/header controls test the
new profiles without allocating a real full model. These are synthetic tests.

## Integration contract for #168 and #164 (not implemented in those PRs here)

1. #168 must pin this #156 revision and reconcile `MlaConfig.precision` (None
   for legacy). Select the complete policy **before** conversion/layout/hash.
   Use `precision::quantize_matrix` on original BF16 bits for selected classes,
   including KV-B's existing semantic transpose. Never expand existing INT8 q
   into INT16 and call it higher precision. Preserve native packed INT4 source
   bytes and scales via the reviewed lossless path.
2. Recompute selected slice byte counts, slice and segment digests from actual
   new bytes, verify those bytes, then `yarn::finalize_manifest` with the precise
   config and assemble/verify using its new layout. Canonical YaRN tables remain
   unchanged. The old `finalize_pending_slices` helper intentionally accepts
   only legacy INT8 pending slices; it must not relabel their bytes as INT16.
   Keep pending-marker, scope/source and corruption rejection gates in #168.
3. #164 must pin the integrated engine, pass `q16: None` for legacy `QView`
   literals or actual LE bytes for wide views, and update its isolated observer
   against the new source hash. Reference dequantization must use selected
   signed-16 q * mu * 2^-k, or original BF16 for a source-reference experiment,
   with that distinction and dtype declared. Same weights, inputs, positions,
   mask, graph/scope and capture order are mandatory. Report routing differences
   separately. Do not adopt FP32 host-specific reference errors as goldens.
4. Run both INT8/INT16 experiments and report per-class activation/logit errors,
   top-1 agreement, memory and speed before deciding shipping precision. The
   existing certification/budget/McNemar tests and unapproved policy remain.

## Resource admission for later real weights

No new weights were fetched. The previous 16.1087 GiB candidate budget was
INT8-specific and is **not** an INT16 admission. For each selected class let E
be element count, R row count. INT8 storage is E+5R bytes; INT16 is 2E+5R.
Additional package/slice bytes = sum(E) over classes promoted to INT16, before
alignment/header growth. Count source BF16 retention (2E), simultaneous slice
and assembled copies, reference tensors, crash leftovers and reserve separately.
Do not discard source tensors required for same-input reference execution.

Conversion additionally materializes original BF16 bits (2E) and output INT16
bytes (2E) for one matrix, its scales (5R), a row buffer (2C), existing writer
buffer (8 MiB), mmap residency and transpose scratch. Execution maps packages;
INT16 weight-limb scratch is at most `3*64*C` bytes plus three 64-row dot arrays
and existing per-thread activation-limb scratch. Reference FP32 expansion, KV
cache, OS overhead and actual available RAM must be budgeted separately. There
is no inference from tiny RSS to real-model admission. Fresh Studio and gaming
PC disk/RAM measurements and PC endpoint/target-volume information remain
required; no new download is admitted by these results.
