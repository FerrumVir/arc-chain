# Integer inference determinism

**Version:** v0.8.0 release candidate (unreleased)

**Last verified locally:** 2026-08-26

## What is now proven automatically

ARC has a blocking known-answer test (KAT) for the production
`CachedIntegerModel`. Unlike the older same-process rerun tests, it compares
against reviewed tokens and BLAKE3 digests committed in
`crates/arc-inference/tests/fixtures/integer_inference_kat.json`.

The three `golden_*` tests verify:

- the synthetic model's exact weight hash;
- next-token IDs and every logits hash for a fixed token sequence;
- the full KV-cache hash;
- one-thread I8 output versus four-thread promoted-I16 output;
- whole-model shard execution versus a three-way layer split, including every
  intermediate hidden-state hash; and
- a fixed autoregressive token sequence and final output hash.

Run the same gate locally with:

```bash
cargo test -p arc-inference --lib --locked golden -- --nocapture
```

`.github/workflows/golden-vectors.yml` runs it as a blocking matrix on Linux
x86_64, Windows x86_64, Apple Silicon macOS, and Intel macOS. Before this branch
is published, the fixture has also passed natively on Apple arm64 and under the
Apple x86_64/Rosetta target. CI results must still pass on the pushed commit;
local evidence is not a substitute for that gate.

## What this does not prove

The KAT is intentionally scoped. It does **not** establish bit identity for:

- Metal, CUDA, WGSL, or any other GPU backend;
- every quantization/storage mode (Q4, block-I8, ternary, or hybrid);
- a production Llama-2-7B GGUF and its real weight bytes; or
- every supported CPU architecture, such as RISC-V.

Until separate hardcoded vectors run on those paths, product copy must say
“CPU I8/I16 KAT verified on ARM and x86” rather than “identical on every chip”
or “GPU verified.” Existing same-host tests such as
`test_deterministic_1000_runs` and SIMD-versus-scalar comparisons remain useful
regression tests, but they are not cross-host proof by themselves.

## Quality remains a separate question

Historical Llama-2-7B Q8_0 measurements reported WikiText-2 perplexity around
107–155 versus a published FP16 baseline of 5.47. Those April 2026 measurements
have not been revalidated by the synthetic KAT and should not be presented as a
current benchmark.

The earlier investigation found a per-row scaling error in
`I8Weights::quantize_f32`: converting a sub-1.0 row maximum to `i64` before
forming the scale collapsed many values. The code now computes that scale in
`f64`, but a correct production-quality claim still needs a fresh, pinned GGUF
evaluation and an explicit quality threshold. Determinism only says repeated
computation agrees; it does not say the model output is good.

## Release rule

A release may claim the automated CPU determinism scope only when the golden
workflow passes on the exact tagged commit. GPU, additional quantization modes,
and large-model quality require their own evidence and must remain labeled
unverified until those gates exist.
