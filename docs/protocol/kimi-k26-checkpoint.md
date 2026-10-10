# What a real Kimi-K2.6 checkpoint would need

**Versioned preparation prerequisite:** [kimi-k26-yarn-v1.md](kimi-k26-yarn-v1.md)
now defines pinned K2.6 YaRN tables/scaling, explicit full/probe/fixture
identities, and finalization of declared pending weight commitments. The
legacy converter still refuses the wrapped packed checkpoint. ARC-72's
separate lossless loader must be integrated with this new preparation before
real slice assembly/forward; no weights were fetched and no real forward was
measured here. The legacy converter gaps below should be read with this
separate entry point in mind.


Status: **requirements**, 7 October 2026. Nothing in this repository has run
Kimi-K2.6 weights, and this document makes no speed claim about Kimi. The
engine work it builds on is the integer profile
`arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1` and its INT4 variant
`…i4g32-experts.q16.v1`
([integer-profile-mla-moe-dyadic-v1.md](integer-profile-mla-moe-dyadic-v1.md)),
developed on Moonlight-16B-A3B-Instruct (the same DeepseekV3 text architecture
at 1/64 of the parameters) and on tiny synthetic configurations.

Provenance tags: **[VENDOR]** read from the Kimi-K2.6 repository at revision
`7eb5002f6aadc958aed6a9177b7ed26bb94011bb` (`config.json`,
`model.safetensors.index.json`, safetensors headers; no weights downloaded);
**[CALC]** arithmetic from those inputs; **[CI]** measured in this branch's
GitHub Actions runs on Moonlight, not on Kimi; **[UNVERIFIED]** an assumption
still to be checked.

## 1. The checkpoint [VENDOR]

- `KimiK25ForConditionalGeneration`: a DeepseekV3 text model (`kimi_k2`) under
  `text_config`, plus a MoonViT vision tower that text serving does not use.
- 61 layers (layer 0 dense, 60 MoE); hidden width 7,168; 64 heads; query LoRA
  rank 1,536; KV latent rank 512; RoPE width 64.
- 384 routed experts, 8 per token, 1 shared expert; expert width 2,048;
  routing scale 2.827; F32 correction bias.
- YaRN RoPE, factor 64; 262,144 positions; vocabulary 163,840; `tiktoken.model`
  byte-identical to Moonlight's.
- Routed experts are INT4, group 32, symmetric (compressed-tensors
  `pack-quantized`: `weight_packed` I32, `weight_scale` BF16, `weight_shape`).
  Everything else is BF16. 64 shards, 595,148,192,736 bytes.
- Licence "Modified MIT": products above 100M monthly active users or US$20M
  monthly revenue must display "Kimi K2.6".

## 2. What the engine is missing for it

`parse_hf_config` already reads the wrapped configuration and refuses K2.6
with exactly these three gaps (unit test
`kimi_k26_and_k3_refusals_list_every_missing_feature`):

| Gap | Work needed |
|---|---|
| Multimodal wrapper: tensors under `language_model.`, a vision tower | Strip the prefix in the tensor mapping (spec §4.1); skip the vision tower and projector tensors. |
| Pre-quantised experts (compressed-tensors `pack-quantized`) | A lossless repacker from `weight_packed` / `weight_scale` to the §13 `.q4` / `.s` layout. No requantisation: the §13 format is K2.6's own values and scales. |
| `rope_scaling: yarn` | A new profile version whose preparation computes YaRN's `ω_i` and the attention scale `λ` (`mscale² = (0.1·ln 64 + 1)²`). The forward pass already reads both from the package, so no operator changes. |

Everything else K2.6 uses (query LoRA, 384/8/1 experts, the F32 bias, group
settings, MLA with the absorbed cache) is implemented and covered by the tiny
golden-digest tests (§6).

## 3. Memory

**Weights [CALC].** Per routed expert at INT4 g32: `3 × 7,168 × 2,048 =
44,040,192` values = 22,020,096 B of nibbles + 2,752,512 B of BF16 scales =
**24.8 MB**. Per MoE layer, 384 experts = **9.51 GB**, plus about 0.15 GB of
attention, shared expert and router at INT8.

| Weight format | Whole model | Per stage, S = 2 | S = 4 | S = 8 |
|---|---|---|---|---|
| v1 profile, INT8 everywhere | 1,028 GB | 514 GB | 257 GB | 128 GB |
| §13: INT4 g32 routed experts + INT8 elsewhere | **582 GB** | 291 GB | 146 GB | 73 GB |

The routed experts are 571 GB of the 582 GB; the remaining ~11.7 GB is
embedding, LM head, attention, the dense layer, shared experts and routers.

**INT4 scale summaries [CALC].** The engine keeps a parsed summary of each
expert matrix's group scales, 16 B per row, built on the matrix's first
projection (`ops::Q4ScaleTable`). Per expert that is 11,264 rows = 180,224 B,
0.73% of its 24.8 MB; 69.2 MB per MoE layer once all 384 experts have run;
4.15 GB over K2.6's 60 MoE layers. `StageModel::q4_scale_table_bytes` reports
what a stage holds (and `q4_scale_table_bound` the most it can reach), and
`arc-mla` records it beside `int8_weights`.

**Per sequence [CALC].**
- KV cache: `61 × (512 + 64)` i32 per position = 140,544 B; 576 MB at 4,096
  positions, 18.4 GB at 131,072, 36.8 GB at the full 262,144.
- The MLA latent cache is shared by all heads, so tensor parallelism over heads
  does not divide it: every device that runs a layer's attention holds that
  layer's cache. Pipeline stages divide it by layer.
- A stage boundary carries the exact Q16 residual: 7,168 × 8 B = 56 KiB per
  position.

**Per island [CALC].** Usable memory must hold the island's share of the 582 GB
plus KV for its concurrent sequences plus runtime. Examples by capacity only:

| Island | Usable memory | Layout | Fits? |
|---|---|---|---|
| 1 machine, 512 GB unified memory | ~410 GB (80%) | whole model | No |
| 2 machines, 512 GB each | ~820 GB | TP2 or EP2, or PP2 at 291 GB per machine | Yes |
| 1 box, 8 GPUs × 96 GB | 768 GB | EP8/TP8 at 73–83 GB per GPU (§5) | Yes, with ~13–23 GB per GPU for KV and runtime |
| 1 box, 8 GPUs × 141 GB | 1,128 GB | as above | Yes |
| Verifier holding one MoE layer | ~9.7 GB | 1-layer slice | Yes on a 16 GB machine |

The 80% usable fraction is [UNVERIFIED]. The v1 INT8 profile needs roughly
twice these weight figures.

## 4. Conversion pipeline

This is the Moonlight pipeline that CI runs today, with what K2.6 adds.

| Step | Moonlight today | K2.6 adds |
|---|---|---|
| 1. Pin the source: revision + per-shard SHA-256 from the Hub's LFS ids | `packages/moonlight-16b-a3b-instruct.source.json` | A K2.6 source manifest (64 shards). |
| 2. Read `config.json`, refuse anything unsupported | `parse_hf_config` | The three gaps of §2. |
| 3. Plan each stage from `model.safetensors.index.json`; download only that stage's shards | `arc-mla convert --layers`; CI converts layers 25–26 + head from 2 of 27 shards | Same code; the index decides which of the 64 shards a stage needs. |
| 4. Non-expert BF16 tensors → INT8 dyadic rows (§4.2); router → INT16 rows (§4.3); bias F32 → Q32 (§4.4) | Implemented | Unchanged. |
| 5. Routed experts → §13 `.q4` / `.s` | Quantised from BF16 (§13.3) | Lossless repack of `weight_packed` / `weight_scale`. The nibble order and value offset of compressed-tensors' packing must be pinned against its own unpacker on a real shard before the repacker lands [UNVERIFIED]. Refuse negative, infinite or NaN scales (§13.1). |
| 6. RoPE tables | Plain RoPE (§4.5) | YaRN tables, new profile version. |
| 7. Write stage packages; segment digests and model root (§4.7) | Rust converter == independent Python preparer: same sha256 and model root on the whole model [CI] | Same check per stage. The model root does not depend on the stage split, so stages can be converted on different machines. |
| 8. Prove the package | scalar == SIMD golden generation; 1/2/4 stages as separate processes; Python re-derives every logits hash, boundary and token; verifier slice replayed on linux x86-64, linux arm64 and windows [CI] | Synthetic K2.6-shaped stages first (real widths, few layers), then the real stages. |

Resources [CALC]: per stage, disk for its source shards (~595/S GB) and its
package (~582/S GB). The Rust converter streams tensors; on Moonlight it peaked
at 1.3 GiB RSS [CI, ubuntu-latest]. K2.6's peak has not been measured.

## 5. Expert placement

**Placement cannot change the result.** Routing is integer (INT16 router rows,
Q16 sigmoid, Q32 keys) with ties broken by the lower expert index and the lower
group index, and the routed outputs are combined as one exact sum of `w·y`
rounded once (spec §5.4–5.6). The unit tests
`tensor_and_expert_splits_are_byte_identical` (experts on two simulated
devices), `routing_ties_resolve_to_the_lower_index_end_to_end` and the shared
tie vectors in `scripts/arc_mla/routing_ties.json` (checked by Rust and Python)
pin this. Placement is therefore an island scheduling choice; it is not part
of the profile or the model root.

**Default layout.** Device `d` of `N` holds experts `[d·384/N, (d+1)·384/N)` of
every MoE layer (384 divides by 2, 3, 4, 6, 8, 12, 16). Attention, the shared
expert and the router either sit on every device or are split by rows (TP);
both give the same bytes.

| N devices | Routed experts per device per layer | Routed bytes per device | + non-expert replicated | + non-expert split (TP) |
|---|---|---|---|---|
| 2 | 192 | 285 GB | 297 GB | 291 GB |
| 4 | 96 | 143 GB | 154 GB | 146 GB |
| 8 | 48 | 71 GB | 83 GB | 73 GB |

[CALC], weights only.

**Exchange.** Each device returns, per token and MoE layer, its exact partial
`Σ w·y` over its experts: 7,168 i128 values (112 KiB) before the single floor
shift. Narrowing that exchange to i64 needs a proven bound per layer; until
then it is i128 [CALC].

**Hot experts.** An expert may be replicated on several devices to balance
load, as long as each token's contribution is computed once; the bytes do not
change. Which experts are hot depends on traffic and has not been measured.

## 6. Evidence that exists, and what does not

Exists:
- Tiny MLA + MoE golden digests, pinned in
  `golden_digests_are_pinned_on_every_kernel` (crates/arc-inference
  `modern/mla/model.rs`): 7 configurations (INT8 and INT4 g32 experts, query
  LoRA and group routing on and off, random, pair-tied and fully tied routers).
  Scalar on 1 and 3 threads and the SIMD kernel must give the same pinned
  package and generation digests; CI checks them on linux x86-64, linux arm64
  and windows x86-64, and they were produced on macOS arm64.
- Moonlight-16B-A3B, INT8 and INT4 g32 experts: Rust == Python packages,
  scalar == SIMD golden generation, stage replay across OSes, perplexity against
  BF16 [CI]. Those figures are Moonlight's, not Kimi's.

Does not exist: the three §2 gaps, a K2.6 source manifest, synthetic
K2.6-width stages, any run on K2.6 weights, any GPU kernel, and any measured
Kimi speed. A plan with hardware options and projections was drafted on the
development branch `kimi-arch-integer`; it is not part of this change because
nothing in it is measured on Kimi.
