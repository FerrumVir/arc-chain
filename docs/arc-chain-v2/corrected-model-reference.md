# Corrected same-GGUF reference check (2026-09-19)

## Correction

The prior Paris exact-ID claim came from re-tokenizing rendered source text and did not observe sampler IDs. Direct instrumentation now shows ARC and llama.cpp share the first eight Paris content IDs, then llama.cpp emits EOG ID `2` while ARC continues under the 12-token bound. This is a terminal-token divergence, not an exact sampled-ID match.

## Bounded evidence

- Artifact BLAKE3: `934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67`
- Reference: `ggml-org/llama.cpp` commit `59657a613ab0fa4ab327d6c790123dff30bfbd67`
- ARC profile: `arc.gguf-llama.i8-per-row.rope-interleaved.v1`
- Six prompts: direct sampled IDs matched exactly for five; Paris matched the first eight content IDs, then diverged at EOG.
- All six bounded semantic gates passed: Paris, `4`, greeting, `café`/`世界`, `yellow`, and `four`.
- Tokenizer vectors matched 11/11; this is profile evidence, not general production tokenizer qualification.

Timing boundaries differ: llama.cpp source wall includes load/prefill/generation while ARC separates load and reports prefill plus generation. These are not a fair speed comparison.

This evidence does not establish broad quality, Fireworks/API parity, WAN performance, cost, or commercial readiness. The Paris EOG difference remains pending bounded logit/quantization investigation.
