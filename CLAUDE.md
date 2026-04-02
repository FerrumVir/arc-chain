# ARC Chain — Session Guide

## What This Is

ARC Chain is a Layer 1 blockchain for verifiable AI inference. The core innovation: a pure integer inference engine that produces bitwise identical output across ARM, x86, and GPU. This enables hash-based verification of AI computation at O(1) cost.

**Version:** 0.2.1
**Repo:** `FerrumVir/arc-chain` (private)
**Language:** Rust (99K+ LOC, 16 crates, 1,196 tests)

## Build & Test

```bash
cargo test --workspace --lib          # 1,196 tests (1 pre-existing failure in arc-state is known)
cargo test -p arc-gpu --lib           # 64 GPU tests
cargo test -p arc-inference --lib     # 53 inference tests (includes INT16)
make eval-perplexity                  # Run perplexity evaluation (needs GGUF model)
```

## What Just Shipped (v0.2.1)

1. **INT16 is now default precision.** 32,767 quantization levels per weight (258x finer than INT8, 32x finer than FP16). Same 2 bytes/param as FP16 but fully deterministic. Dispatch priority: I16 > Q4 > I8.

2. **Attention kernel fixed.** Was broken for seq>32 (only 32 positions got Q*K scores). Now uses stride loops for arbitrary seq_len + parallel tree-reduction softmax.

3. **5 new Metal shaders written but UNTESTED:** `attention.metal`, `rope.metal`, `silu.metal`, `residual.metal`, `argmax.metal`. These complete the Metal ICB forward path in `metal_icb.rs`. They need hardware validation on Apple Silicon.

4. **Param writes batched** from 96 to 3 per token (shared uniform buffers).

## What Needs Testing on Mac Studio (M2 Ultra)

### Priority 1: Run Perplexity Evaluation
This is the single most important thing to prove. Measures INT16 quality vs published FP16 baselines.

```bash
# Download a model first:
huggingface-cli download TheBloke/Llama-2-7B-GGUF llama-2-7b.Q8_0.gguf --local-dir ~/.arc-models

# Or for the base model (preferred for fair comparison):
huggingface-cli download TheBloke/Llama-2-7B-GGUF llama-2-7b.Q8_0.gguf --local-dir ~/.arc-models

# Download WikiText-2:
curl -L -o ~/.arc-models/wikitext-2-raw-v1.zip "https://s3.amazonaws.com/research.metamind.io/wikitext/wikitext-2-raw-v1.zip"
cd ~/.arc-models && unzip -o wikitext-2-raw-v1.zip

# Run evaluation:
cargo run --example eval_perplexity --features candle --release -- \
  ~/.arc-models/llama-2-7b.Q8_0.gguf ~/.arc-models/wikitext-2-raw/wiki.test.raw 512
```

**What to look for:** PPL (perplexity) number. Published FP16 baseline for Llama-2-7B base is 5.47. If INT16 is close (say <10), that proves INT16 is production-quality. The paper currently shows INT8 at PPL 144 on a Chat model — that comparison is confounded. We need base-model INT16 vs base-model FP16.

**IMPORTANT:** The eval currently runs the INT16 path by default (dispatch_matmul prefers I16). To also test INT8 for comparison, temporarily comment out `enable_i16()` or the I16 dispatch path.

### Priority 2: Validate Metal Shaders
The 5 new Metal shaders have never been run on hardware. Test them:

```bash
# Build with Metal ICB feature:
cargo build -p arc-gpu --features metal-icb --release

# If it compiles, run GPU tests:
cargo test -p arc-gpu --features metal-icb --release

# Run GPU forward benchmark with profiling:
GPU_PROFILE=1 cargo run --example bench_gpu_forward --features candle --release -- ~/.arc-models/llama-2-7b.Q8_0.gguf
```

**What to look for:** 
- Do Metal shaders compile on the M2 Ultra's Metal driver?
- Does `forward_token()` in `metal_icb.rs` produce correct output?
- What's the encode time? Should be ~1-2ms instead of ~18ms via wgpu.
- What's total ms/tok? Target: 8-15ms.

### Priority 3: GPU Inference Benchmark
Test the attention fix and param batching improvements:

```bash
GPU_PROFILE=1 cargo run --example bench_gpu_forward --features candle --release -- ~/.arc-models/llama-2-7b.Q8_0.gguf
```

Compare against previous: 76ms/tok (52ms compute + 18ms encode + 6ms submit). The attention fix (parallel softmax) and param batching (96->3 writes) should reduce this.

### Priority 4: Cross-Platform Hash Verification
Generate a reference hash on Mac Studio and compare with x86:

```bash
cargo run --example determinism_check --features candle --release -- ~/.arc-models/llama-2-7b.Q8_0.gguf
```

INT16 inference must produce identical hashes across platforms. If the Mac Studio hash differs from an x86 run, there's a bug.

## Architecture Notes

- **Inference crate:** `crates/arc-inference/src/cached_integer_model.rs` — INT16 weights, matmul, forward pass
- **GPU crate:** `crates/arc-gpu/src/` — WGSL shaders (`transformer.wgsl`), Metal shaders (`.metal`), forward orchestration (`gpu_forward.rs`), Metal ICB (`metal_icb.rs`)
- **Fixed-point:** Q16 format (i64 with 16 fractional bits, ONE = 65536)
- **Quantization:** Per-row symmetric. INT16 = [-32767, 32767] with scale = abs_max * ONE
- **Feature flags:** `candle` (GGUF loading), `metal-icb` (native Metal dispatch), `stwo-prover` (STARK proofs, needs nightly)

## Known Issues

- `test_channel_close_releases_funds` in arc-state fails (pre-existing, unrelated to inference)
- Metal shaders are UNTESTED — marked with comments, need hardware validation
- `eval_perplexity.rs` needs model files to run (not checked into repo)
- Q4 scale factor `* 18` approximation in `Q4WeightsX86::from_i8` may produce wrong magnitudes on x86 (never validated on actual x86 hardware)

## The Stwo STARK System is REAL

The Stwo Circle STARK prover (`stwo_air.rs`) is REAL, not mock. It uses `stwo_prover_mod::prove::<SimdBackend, Blake2sMerkleChannel>()` when the `stwo-prover` feature is enabled. The default path (no feature) uses BLAKE3 for fast testing. Do NOT claim Stwo is mock.
