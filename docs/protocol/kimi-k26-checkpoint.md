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

`parse_hf_config`, the strict legacy parser, lists three K2.6 gaps (unit
test `kimi_k26_and_k3_refusals_list_every_missing_feature`). The actual
`convert_stage` and weight-slice entry points use `parse_hf_weights_config`:
both accept the wrapper and packed experts, leaving YaRN as the stage-package
blocker. Direct conversion requires `--experts i4g32`. See
[kimi-first-light-loader.md](kimi-first-light-loader.md) for the loader-to-engine
contract, fixture execution evidence and remaining architecture work:

| Gap | Status |
|---|---|
| Multimodal wrapper: tensors under `language_model.`, a vision tower | **Done for weights** (spec §14.1): `parse_hf_weights_config` reads `text_config`; the prefix is stripped and the vision tower and projector are skipped. |
| Pre-quantised experts (compressed-tensors `pack-quantized`) | **Done** (spec §14.2): a lossless repack of `weight_packed` / `weight_scale` into the §13 `.q4` / `.s` layout (each byte XOR 0x88; scales copied). No requantisation. Pinned against compressed-tensors' own `unpack_from_int32` on a real K2.6 shard in CI. |
| `rope_scaling: yarn` | **Open.** A new profile version whose preparation computes YaRN's `ω_i` and the attention scale `λ` (`mscale² = (0.1·ln 64 + 1)²`). The forward pass already reads both from the package, so no operator changes. Weights do not depend on it: the slice manifest lists it as pending, and its `model`, `tables` and `model_root` stay null until it is specified. |

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
| 1. Pin the source: revision + per-shard SHA-256 from the Hub's LFS ids | `packages/moonlight-16b-a3b-instruct.source.json` | **Done:** `packages/kimi-k26.source.json` (config.json, 64 shards, and the index). |
| 2. Read `config.json`, refuse anything unsupported | `parse_hf_config` | **Done for weights:** `parse_hf_weights_config` (§2 above); YaRN pending. |
| 3. Plan from `model.safetensors.index.json`; download only the needed shards | `arc-mla convert --layers`; CI converts layers 25–26 + head from 2 of 27 shards | **Done:** `arc-mla slice-plan` and `scripts/arc_mla/stream_slices.py` (spec §14.4). Layer `l` is in shard `l + 1`, embedding, norm and LM head in shard 62, so each step holds one shard; shards 63–64 (vision) are never fetched. |
| 4. Non-expert BF16 tensors → INT8 dyadic rows (§4.2); router → INT16 rows (§4.3); bias F32 → Q32 (§4.4) | Implemented | Unchanged; the same code writes slices. |
| 5. Routed experts → §13 `.q4` / `.s` | Quantised from BF16 (§13.3) | **Done:** lossless repack (spec §14.2), checked against compressed-tensors' `unpack_from_int32` on shard 2 [CI]. Negative, infinite or NaN scales are refused. |
| 6. RoPE tables | Plain RoPE (§4.5) | **Open:** YaRN tables, new profile version. |
| 7. Write segments; digests and model root (§4.7) | Rust converter == independent Python preparer: same sha256 and model root on the whole model [CI] | **Done as slices** (spec §14): content-addressed per layer and per expert group, segment digests in the same pass, Rust == Python manifest. The model root follows once YaRN is specified, from the same slices. |
| 8. Prove the package | scalar == SIMD golden generation; 1/2/4 stages as separate processes; Python re-derives every logits hash, boundary and token; verifier slice replayed on linux x86-64, linux arm64 and windows [CI] | Synthetic K2.6-shaped stages first (real widths, few layers), then the real stages. |

### 4.1 Resources per shard (measured)

`scripts/arc_mla/stream_slices.py` on the first two K2.6 shards, 48 expert
groups. "Convert" includes the converter's own SHA-256 re-check of the shard.
Peak RSS is the converter process's. CI runs are workflow
`kimi-k26-slices.yml` run 37622036049. The arm64 leg hashes slices without
storing them (`--discard`).

| Shard (unit) | Source | Slices | Runner | Download | Convert | Peak RSS |
|---|---|---|---|---|---|---|
| 1 (layer 0, dense) | 0.93 GiB | 1 slice, 0.46 GiB | CI runner, linux x86-64 (AMD EPYC 9V74, 4 vCPU, 16 GB) | 24.7 s | 2.7 s | 0.68 GiB |
| 1 | | | CI runner, linux arm64 (Neoverse-N2, 4 vCPU, 16 GB) | 29.2 s | 5.7 s | 0.68 GiB |
| 1 | | | Studio lab (M2 Ultra, macOS arm64) | 23.2 s | 4.9 s | 1.37 GiB |
| 2 (layer 1, MoE) | 9.14 GiB | 49 slices, 9.00 GiB | CI runner, linux x86-64 | 1,352.0 s | 54.7 s | 0.35 GiB |
| 2 | | | CI runner, linux arm64 | 937.8 s | 46.5 s | 0.35 GiB |
| 2 | | | Studio lab | 1,041.2 s | 44.0 s | 0.71 GiB |

- **Disk.** The source shard being converted plus the slices kept. With
  every step holding one shard, a machine that keeps all slices peaks at its
  slices plus the largest shard (9.8 GB). A node that keeps only its own
  slices needs those plus one shard.
- **RAM.** Under 1.4 GiB measured on both layer shards. The shard-62 step
  (embedding and LM head, 163,840 × 7,168 BF16 each) holds one 2.35 GB BF16
  matrix and its 1.18 GB INT8 form at a time: about 3.6 GB [CALC, not
  measured].
- **Time.** Conversion is about 45–55 s per MoE shard on a 4-vCPU CI runner.
  The download is the bound: 7.26–10.46 MB/s per stream from Hugging Face in
  these runs (shard 2, 9,809,047,464 B in 1,351.96 s on CI x86-64 and 937.8 s
  on CI arm64). A projection for all 62 text shards (594.2 GB) from these
  figures: about one hour of conversion [CALC]. The downloads would take
  about 16–23 h on one such stream [CALC]; shards are independent, so they
  parallelise across machines.

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

Also exists (spec §14):
- the K2.6 source manifest;
- weight slices of real K2.6 layers 0 and 1. Their manifest
  (`packages/kimi-k26.slices-layers-0-1.json`, `manifest_blake3`
  `83347a71…`) is identical on linux x86-64, linux arm64 (CI) and macOS arm64
  (Studio lab), on 1 thread and on all threads, and in the independent Python
  preparer. Compressed-tensors' own unpacker gives the same INT4 values and
  scales for 8 sampled experts × 3 projections. The pinned file was added
  after CI run 37622036049 (copied from the Studio-lab run of the same
  shards) and checked byte for byte against that run's artifacts; the run
  itself had no pin to compare with.

Does not exist: the YaRN preparation (§2), so no K2.6 stage package, model
root or forward pass; slices of layers 2–60 and of the embedding and head
(the same code, not yet run); synthetic K2.6-width stages; any GPU kernel; and
any measured Kimi speed. A plan with hardware options and projections was drafted on the
development branch `kimi-arch-integer`; it is not part of this change because
nothing in it is measured on Kimi.
