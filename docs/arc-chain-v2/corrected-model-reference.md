# Corrected same-GGUF reference check (2026-09-19)

## Result

The corrected ARC profile matched the pinned `llama.cpp` reference exactly for one bounded, deterministic Llama-2 prompt.

- Artifact BLAKE3: `934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67`
- Reference: `ggml-org/llama.cpp` commit `59657a613ab0fa4ab327d6c790123dff30bfbd67`
- ARC profile: `arc.gguf-llama.i8-per-row.rope-interleaved.v1`
- Prompt: `[INST] What is the capital of France? [/INST]`
- Sampling: greedy; ARC's repetition penalty did not affect this sequence.

Both returned the exact same eight completion IDs (including BOS):

```text
[29871, 450, 7483, 310, 3444, 338, 3681, 29889]
```

Decoded completion: `The capital of France is Paris.`

Timing is not apples-to-apples: the reference measurement is **1.51 seconds for decode-only eight-token generation**, while corrected ARC is **5.572 seconds for whole generation including prefill and decode**. These timing boundaries do not establish a fair latency or speed comparison. ARC reported 6,886,080,512-byte maximum RSS, 8,136,546,664-byte peak footprint, and zero swaps.

## Repair and evidence

The new Llama-only loader validates architecture and dimensions, loads canonical per-row I8 only, and permutes each Q/K head's data rows and per-row scales from `[e0,o0,e1,o1]` to `[e0,e1,o0,o1]`. Existing split-half RoPE then represents GGUF interleaved RoPE under a shared permutation, preserving Q·K scores. It has its own profile ID and refuses legacy ARC-INT8 cache export because that format has no profile tag.

The pinned tokenizer matched ARC IDs, including BOS, for the required France, arithmetic, and Unicode prompts. This validates those vectors only. The GGUF identifies SPM with score metadata; ARC's generic greedy encoder is not yet a general SentencePiece or production-tokenizer qualification.

Focused tests passed: interleaved RoPE pairing; multihead/GQA Q/K data-plus-scale permutation and dot-product oracle; versioned-profile cache rejection; v2 single-BOS/final-prompt-logit generation; and byte-fallback decoder. The candle-only validation example compiles and its help exposes `--interleaved-rope --reference-minimal-case`.

## Limit

This is one same-GGUF, same-prompt correctness result. It does not establish broad quality, Fireworks/API parity, production tokenizer qualification, query partitioning, WAN performance, cost, or commercial readiness.

Complete ARC phase log and JSON: `batch3-corrected-reference-arc.log`.
