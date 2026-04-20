# Integer Inference on ARC Chain — Determinism & Quality

**Version:** v0.5.2 (shipped)
**Last verified:** 2026-04-20

## Determinism (the IP)

All integer inference paths produce **bitwise identical** output across
ARM (Apple Silicon) and x86 (Vultr VPS), across runs, across GPU and CPU
backends. This is the feature that lets inference pass consensus the
same way transactions do.

Guarantees:

- `forward_one_token` → same logits, same hash on ARM = x86 = NEON = scalar
- `forward_shard_token` → same hidden-state hash across the pipeline
- `/inference/run` and `/inference/run_sharded` return identical
  `output_hash` on repeated calls with the same prompt (verified in
  prod against Mac + 8 Vultr seeds at round 1,176,883)
- Tests asserting bit-identical reruns: `test_deterministic_100_runs`,
  `test_deterministic_1000_runs`, `test_simd_matches_scalar`,
  `test_dot_i16_simd_matches_scalar`

## Quality (a known limitation)

WikiText-2 perplexity on Llama-2-7B **Q8_0 GGUF**, 1024-token sample:
**~107** (prior 63-token run) / **~155** (256-token run).

Published FP16 baseline is 5.47. We are running ~20–30× worse.

Root cause (diagnosed 2026-04-20, see `project_i16_ppl_bug.md` memory
and `crates/arc-inference/examples/probe_i16_real_weights.rs`):
`I8Weights::quantize_f32` truncates the f32 per-row `abs_max` with
`abs_max as i64` before computing the scale. For 100% of Llama-2-7B's
`output.weight` rows and 100% of `ffn_down` rows — which have
`abs_max ∈ [0.034, 0.523]` — this collapses the scale to 1, making
matmul output 36× smaller than f32 ground truth.

**Why the quality loss is not fixable with a one-line edit:** every
downstream integer primitive (`attn_scale`, `integer_exp` LUT input
range, KV cache scale conventions) was empirically tuned to the
36×-undersized I8 output. Fixing the matmul alone detonates the softmax
(verified: PPL explodes from 107 → 782 when I16::quantize_f32 produces
mathematically correct magnitudes).

**Path forward (not done):**
1. Coordinated rescale of `attn_scale`, `integer_exp` LUT, KV cache
   scales to match the corrected matmul magnitudes. Changes every
   chain attestation hash — requires rolling upgrade of all seeds.
2. Or swap to proper block-wise quantization (Q8_0 / Q4_K style) with
   per-block scales. Larger rewrite but matches llama.cpp quality.

## Investigation artifacts

Committed probes for whoever picks this up:

- `crates/arc-inference/examples/probe_i16_vs_i8.rs` — forward-pass A/B
- `crates/arc-inference/examples/probe_i16_matmul.rs` — single-row
  synthetic matmul vs ground truth
- `crates/arc-inference/examples/probe_i16_real_weights.rs` — real
  Llama-2-7B tensors vs ground truth (the definitive probe)

## What's unblocked on top of the current v0.5.2 baseline

The PPL-107 baseline is coherent enough that the chain-level features
work: shard pipeline agrees on output hashes, attestations verify,
cross-arch determinism holds. The inference quality limitation does
not block consensus, economics, or demo use cases — it blocks a
head-to-head comparison with FP16 Llama on natural-language benchmarks.
