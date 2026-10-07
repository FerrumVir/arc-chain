# Kimi First Light: pre-download loader handoff

This is fixture-level loader/engine interoperability on PR #168, based on
`ce9831bf1ef46e1871b2b4437c4efeb0925078c4`. The engine/package contract is
inherited from PR #156 at `0e5a9d732d0c84fc6d8956fb2c93b2a1b0fd4169`.
No new real weights are needed for these checks. Neither PR #156 nor its
profile/goldens are changed by this follow-up.

## Loader to engine mapping

| Input / boundary | Implemented path on #168 | Evidence / remaining condition |
|---|---|---|
| Multimodal `text_config`, `language_model.` tensors | `config::parse_hf_weights_config` → `WeightsSource`; `convert::stage_source_tensors_in` and `SourceTensors` | Existing packed fixture supplies every text tensor under that prefix. Direct and sliced packages match byte for byte and execute. The strict `parse_hf_config` helper still rejects multimodal sources; **it is not the converter entry point**. |
| `vision_tower.*`, `mm_projector.*` | `slices::plan` selects text units; `convert::ignored` excludes only the documented vision names and rotary buffers | Engine check removes the vision-only shard while retaining its source pin; planning does not request it and direct conversion still produces the exact assembled package. |
| compressed-tensors `pack-quantized`, symmetric INT4 g32 | `packed_int4_scheme` validates storage; `write_stack_q4_packed` / `repack_int4_words` XOR 0x88 per byte, copy checked BF16 scales | Reuse existing plain, YaRN and edge fixtures and real-shard proof. Edge includes -8 and zero scale. No requantization of source INT4. |
| Expert format | Slice commands auto-select i4g32 for packed sources. Direct `convert` requires `--experts i4g32` (default is i8). | **New guard:** incompatible i8 is rejected before package creation, even for a dense-only stage; previously that range could succeed under an inconsistent model profile, and a whole conversion could leave a partial file. CLI checks cover both ranges. |
| Slices → stage package | `slices::assemble_stage` → `StageWriter`, shared `package::layout` and `TensorSink` | Existing checks prove byte-identical direct/assembled packages and matching segment hashes. |
| Stage package → actual engine | `arc-mla golden` / `stage` → `StageModel::open` / `open_range`, `generate` / `run_sequence` | **New checks:** plain and edge assembled packages run scalar (1 thread), SIMD (3 threads), and 1/2/4 file-backed pipeline stages. Every logits hash, selected token, boundary digest and model root agrees. Plain also agrees with its BF16 twin converted to i4g32. These are tiny synthetic executions, not real Kimi execution. |
| Real K2.6 YaRN → tables / model root | `HfWeightsConfig.pending`, `SliceManifest::config` and `convert_stage` | **Blocked:** both `convert` and `slice-assemble` refuse with `rope_scaling yarn` before creating output. Slices remain valid; model/tables/model_root are null. CLI regression pins this behavior. |

Run `scripts/arc_mla/slice_checks.sh` with a freshly built arc-mla and fresh
work/evidence directories. It now invokes `check_slice_engine.py`; existing
Kimi tiny CI legs run it on linux x86-64, linux arm64 and Windows x86-64.
`engine/summary.json`, per-layout checks, per-kernel runs and commands.log
record the new evidence. Timings inside these files are fixture timings.

## What #156 must define before a real forward

The versioned deterministic YaRN preparation is still missing: factor 64,
beta fast/slow 32/1, original context 4096, frequency interpolation and
attention scaling (including mscale/all-dimension semantics), rounding and
canonical table encoding. `rope_tables` currently computes plain RoPE;
`attention_lambda` is the current profile value. It is not valid to clear
`pending`, drop `rope_scaling`, or reuse plain tables for K2.6.

That preparation must create the matching model object, tables segment,
profile/contract identity and model-root commitments, with independent
reference vectors and cross-architecture goldens. Weight slice bytes do not
need to be repacked again, but manifest finalization under that identity is
not yet implemented. Merely downloading more shards does not fix this.

StageSpec is contiguous: embedding belongs to a stage starting at layer 0,
and the head belongs to a stage ending at layer 61. Selecting layers 0–2
plus a head is not a full K2.6 model. A layer probe must use the intended
boundary inputs, or explicitly define a reduced-depth experimental package
and distinct identity. It must not label a truncated path as K2.6 generation.
Full-model roots also require the complete committed segment set. These
execution/finalization decisions belong to the next architecture dispatch.

## Reused real-weight evidence

The real shards 1–2 proof is run
[37622036049](https://github.com/FerrumVir/arc-chain/actions/runs/37622036049),
original head `7a952ec7a4d00f1f0973c1c8be8e81fb5f874568`. It is not a
new measurement at this revision. Reviewed head `ce9831bf` retained that
proof: the layers 0–1 manifest is
`83347a71d6324dac0a7c7895305bfb149a639cfc55892deebbc6dbeeb5c24b59`.
The repacker, successful i4g32 conversion, profile layout, source pin and
published manifest remain unchanged. Only early rejection of the incompatible
format and the fixture execution coverage change here.

No full forward, model-quality result, internet pipeline or Kimi throughput
is established. Disk admission for a subsequent bounded fetch is a separate,
timestamped report, including explicit unavailable PC capacity.
