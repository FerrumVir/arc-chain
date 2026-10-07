# Portable bit-exact GPU kernels (WGSL / wgpu)

ARC's integer engine for SmolLM3-3B (profile
`arc.hf-llama.i8-dyadic-row.q16.v1`, spec:
[integer-profile-hf-llama-dyadic-v1.md](protocol/integer-profile-hf-llama-dyadic-v1.md))
now also runs on GPUs through one set of WGSL compute kernels. wgpu runs them
on Vulkan (NVIDIA, AMD, Intel, Mesa), DX12 (Windows, including the WARP software
adapter) and Metal (Apple). The GPU computes **the same integers** as the CPU
engine: the same logits at every position, the same tokens, and the same golden
digest `3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2`.
Every check compares hashes or integers for equality. There is no tolerance
anywhere in the gate: one differing bit in any logits vector fails it.

| Where | What |
|---|---|
| `crates/arc-gpu/src/modern/` | WGSL kernels, the GPU engine, the single-operation lab |
| `crates/arc-inference/src/modern/gpu/` | Model upload, the generation loop, the traced CPU reference, the operator self-test, the CLI commands |
| `arc-modern golden --gpu`, `gpu-info`, `gpu-check` | The command-line path; `gpu-check` is the Proof Kit's GPU mode |
| `scripts/arc_modern/golden/smollm3-3b.cpu-golden.json` | Every logits hash of the five golden prompts from the CPU engine (CI run 37473148757) |
| `.github/workflows/gpu-portable.yml` | The exactness gate on Mesa lavapipe (Linux) and WARP (Windows) |
| `crates/arc-inference/src/modern/gpu/backend.rs`, `arc-node --gpu-inference` | The community worker's GPU backend: off by default, used only after a bit-exact self-test on the adapter ([gpu-worker-backend.md](gpu-worker-backend.md)) |

## 1. Results so far

Measured in GitHub Actions on 6 October 2026 ("CI runner" numbers). Software
rasterizers prove exactness, but their timings say nothing about GPU speed.
Every row comes from run
[37525693372](https://github.com/FerrumVir/arc-chain/actions/runs/37525693372)
on commit `1a28b26d0`, the last commit that changed a kernel or the engine; all
five jobs of that run passed. The adapter column is what wgpu reported
(`gpu.adapter` in the result files, kept as run artifacts). One hardware GPU
has run these kernels so far, an Apple M2 Ultra through Metal; its results and
the defect it found follow the table.

| Job (duration) | Adapter, as reported by wgpu | What ran | Result |
|---|---|---|---|
| SmolLM3-3B golden on lavapipe (43 min 49 s) | `llvmpipe (LLVM 20.1.2, 256 bits)`, Vulkan, driver `llvmpipe`, `Mesa 25.2.8-0ubuntu0.24.04.4 (LLVM 20.1.2)`, software adapter; AMD EPYC 7763, 4 vCPU; Ubuntu 24.04 runner image | The five SmolLM3-3B golden prompts: 393 prompt tokens, 86 decode passes, 479 forward passes in all. Package `19c67496…aa91` converted on the runner and checked against the pinned manifest; 3,086,602,240 bytes of weights uploaded in 2.6 s | Matrix digest `3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2`, **identical to the CPU golden**; 5 of 5 cases match, no divergence. Self-test (8 rounds): 56 kernel cases, 30 refused on both sides, 0 mismatches. Per-layer trace: 1,050 operation hashes over 2 forward passes, all equal. 707 dispatches per forward pass. Prefill 1,642 s (0.239 tok/s), decode 360 s (0.239 tok/s): software, not a speed number |
| same job | lavapipe, as above | Batched prefill, 16 tokens per pass, on golden case `capital` (80 prompt tokens, 7 decode passes) | Every logits hash and every token identical to the CPU golden's case `capital`; logits digest `38065149…9242` on both sides. Prefill 327 s (0.245 tok/s): software |
| SmolLM3-3B golden on WARP (13 min 3 s) | `Microsoft Basic Render Driver`, DX12, driver `10.0.26100.33438`, software adapter; Intel Xeon Platinum 8370C, 2 cores, 4 logical processors; Windows Server 2025 runner image | `gpu-check --prefix-forwards 4 --trace-forwards 1`: the same package, converted on Windows and checked against the pinned manifest; 3,086,602,240 bytes uploaded in 71.1 s | 4 of 4 forward passes of case `capital` with logits identical to the CPU golden (80.0 s, 27.5 s, 27.7 s and 27.2 s per pass; the first includes WARP's shader compilation). Self-test (8 rounds): 56 kernel cases, 30 refused on both sides, 0 mismatches, in 59.0 s. Per-layer trace: 525 operation hashes over 1 forward pass, all equal. 706 dispatches per forward pass. 0.025 tok/s: software, not a speed number |
| lavapipe (3 min 40 s) | lavapipe, as above | Operator known-answer tests (300 rounds) | 2,100 cases, 1,091 refused by both CPU and GPU (out-of-domain inputs), 0 mismatches |
| same job | lavapipe | Tiny SmolLM3-shaped model | CPU = GPU = GPU with 3 tokens per pass = `98cc9928…0918`. gpu-check passes: self-test 140 cases, 71 refused on both sides, 0 mismatches; 488 operation hashes equal over 8 forward passes |
| WARP (18 min 39 s) | WARP, as above | Operator known-answer tests (150 rounds) | 1,050 cases, 548 refused on both sides, 0 mismatches |
| same job | WARP | Tiny SmolLM3-shaped model | CPU = GPU = GPU with 3 tokens per pass = `98cc9928…0918`. gpu-check passes: self-test 140 cases, 71 refused on both sides, 0 mismatches; 488 operation hashes equal over 8 forward passes |
| fmt + clippy + actionlint (2 min 35 s) | none (CPU only) | rustfmt; clippy with `-D warnings` on arc-gpu and arc-inference (candle enabled); actionlint; shellcheck; the CPU-side unit tests | pass |

The dispatch count differs between the two adapters because each weight matrix
is uploaded in row chunks that fit one storage binding, with one GEMV dispatch
per chunk; an adapter with a smaller maximum binding size issues more
dispatches (707 on lavapipe, 706 on WARP). The values do not depend on it.

**On a hardware GPU: Apple M2 Ultra through Metal, 7 October 2026.** TJ's Mac
Studio (Mac14,14, Apple M2 Ultra, 64 GB unified memory, macOS 14.6.1) ran
`ex13-hw-run-macos.sh` at commit `b2f90797b` on the wgpu Metal backend. wgpu
reported the adapter as `Apple M2 Ultra`, `IntegratedGpu`, Metal, with an empty
driver string. Lab numbers: one stream, token selection on the CPU, logits read
back after every forward pass.

| What ran | Result |
|---|---|
| The five SmolLM3-3B golden prompts, 479 forward passes; package `19c67496…aa91` converted on the Mac and checked against the pinned manifest; 3,086,602,240 bytes uploaded in 1.53 s | Matrix digest `3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2`, **identical to the CPU golden**; 5 of 5 cases, every logits hash and every token; self-test (8 rounds) 56 cases, 0 mismatches; per-layer trace 1,050 hashes over 2 passes, all equal; `first_divergence` null; 706 dispatches per pass. Prefill 393 tokens in 18.49 s (21.26 tok/s); decode 86 passes in 3.99 s (21.56 tok/s) |
| Batched prefill, 16 tokens per pass, all five prompts | Matrix digest identical; prefill 8.96 s (43.86 tok/s), decode 21.50 tok/s |
| Prefix run: 4 passes of case `capital`, 1 traced pass | 4 of 4 identical; 525 trace hashes equal; 0.26 s for the first pass (shader compilation), then 0.044 to 0.048 s per pass |
| Tiny SmolLM3-shaped model | CPU = GPU = GPU with 3 tokens per pass = `98cc9928…0918` |
| Operator known-answer tests, 300 rounds | 2,100 cases, 1,091 refused on both sides, **9 mismatches, all in `gated_silu`**. The tiny model's gpu-check self-test (20 rounds) showed the same defect: 140 cases, 2 mismatches |

The defect, from the Studio's result files: every mismatching element had a
positive gate `g` of 55 to 59 bits (high word roughly in [2^22, 2^27)), and the
GPU's value equalled the CPU's product multiplied by exactly 2^15 + 1 before
the floor shift, as if sigma had been 0x80010000 instead of 65536. For
g = 81162991232727707 and u = -1 the GPU gave -40582734065326756 where the CPU
gives -1238448962902; the other seven cases were refusals of values the CPU
accepts, for the same reason. Gates below 2^54, the 63-bit edge values and
every negative gate agreed. The real model never produces such gates, which
is why the golden runs passed. The inputs were recovered by replaying the
self-test's SplitMix64 generator (seed `0x00A2C0DE`) in Python; the CPU
values matched the Studio's report to the digit.

The gated-SiLU kernel was the only one that divided with the native `/`
operator (two divisions by a per-thread divisor; naga routes integer division
through a helper function, and Apple GPUs lower it in software), and every
other kernel, including the ones that share its wide-multiply helpers, agreed
with the CPU on that adapter. The commit after `b2f90797b` on this branch
computes sigma with the shift-subtract divider of int.wgsl instead,
branch-free, and adds the recovered pairs plus an edge sweep over both
operands to the self-test as fixed cases, so lavapipe, WARP and every
volunteer adapter exercise the same inputs (self-test counts become 2,242
cases at 300 rounds and 198 at 8 rounds).

**Re-run at commit `7caaeb0a` (lab: TJ's Mac Studio, Apple M2 Ultra, Metal,
7 October 2026, 06:23 UTC):** all 26 summary rows pass. Known-answer tests:
2,242 cases, 1,231 refused on both sides, 0 mismatches; 11 tests passed. Tiny
model: CPU = GPU = GPU with 3 tokens per pass = `98cc9928…0918`; its gpu-check
self-test 282 cases, 211 refused, 0 mismatches, 488 trace hashes equal. Full
golden run: matrix digest `3e43f342…49f2`, identical to the CPU golden (every
logits hash and token; the golden file is the same at both pins); self-test
198 cases, 170 refused, 0 mismatches; 1,050 trace hashes equal;
`first_divergence` null. Batched prefill (16 tokens per pass) identical; prefix
run 4 of 4 with 525 trace hashes equal; package verified. Prefill 393 tokens in
18.75 s (20.96 tok/s), decode 86 passes in 3.99 s (21.55 tok/s), batched
prefill 43.88 tok/s; 706 dispatches per pass. On this machine the gated-SiLU
mismatch is resolved. That says nothing about other GPUs or vendors; the PC
run (DX12 and Vulkan) is pending.

**CI at the same commit `7caaeb0a`** (the branch was rebased onto `main` on
7 October after #137 merged, so the commit ids in the table above, `1a28b26d0`
and `b2f90797b`, are pre-rebase ids; the kernels are unchanged apart from the
gated-SiLU change). Run
[37580297266](https://github.com/FerrumVir/arc-chain/actions/runs/37580297266),
all jobs green: lavapipe known-answer tests 2,242 cases, 1,231 refused on both
sides, 0 mismatches, 11 tests passed; WARP 1,192 cases, 688 refused, 0
mismatches; the tiny model identical on both with self-tests 282 cases, 211
refused, 0 mismatches; the SmolLM3-3B full run on lavapipe with the new sigma
code gives matrix digest `3e43f342…49f2` again, 5 of 5 cases, self-test 198
cases, 170 refused, 0 mismatches, 1,050 trace hashes equal, 707 dispatches per
pass, 0.239 tok/s (software), and the 16-token batched prefill is identical;
the WARP prefix run matches 4 of 4 passes with 525 trace hashes equal (430 s
for the first pass on that runner instance, then about 28 s per pass).

**Not yet shown.** The two CI adapters above are software rasterizers for two
APIs (Vulkan and DX12), on two operating systems, through two shader
compilers (naga to SPIR-V for lavapipe; naga to HLSL, then FXC, for WARP); the
Apple GPU adds a third compiler (naga to MSL). NVIDIA, AMD and Intel GPUs wait
for the gaming PC's run (`ex13-hw-run-windows.ps1`, DX12 and Vulkan) or for
volunteers' `gpu-check` results (§7). Other teams verify across NVIDIA
generations or on one GPU type; the same bits across GPU vendors is the target
of this path, and so far it holds for the CPU, two software adapters and one
Apple GPU on the real model.

**Earlier runs.** Run
[37508127774](https://github.com/FerrumVir/arc-chain/actions/runs/37508127774)
(commit `d83d6334d`) produced the same lavapipe matrix digest
`3e43f342…49f2` at 0.238 tok/s, and showed that FXC, the DX12 shader
compiler wgpu uses by default, rejects dynamically indexed local arrays inside
loops; §5 covers the rewrite. Run
[37522544179](https://github.com/FerrumVir/arc-chain/actions/runs/37522544179)
(commit `97b74a277`) passed the operator tests and the tiny model on WARP and
showed that one forward pass of the 3B model on WARP exceeds wgpu-core's
built-in 60-second wait.

**WARP and the real model.** One forward pass of the 3B model takes WARP 27 to
80 seconds on the runner above. That exceeded wgpu-core's built-in 60-second
wait; the engine now keeps waiting, up to `ARC_GPU_WAIT_SECONDS` (default one
hour).

At that speed the full golden run (479 passes) cannot finish within a 6-hour CI
job. CI therefore checks WARP with `gpu-check --prefix-forwards`: the first
passes of the first golden case, teacher-forced with the golden tokens, each
logits hash compared exactly, plus a traced pass compared operation by
operation.

On Windows runners, FXC also takes about a minute to compile the nine kernels
each time an engine is built (the WARP self-test above took 59 s for that
reason). WARP compiles each kernel on its first dispatch, which inflates the
first pass's time. Neither affects any value.

## 2. Why the GPU computes the same integers

The CPU profile is defined in exact integer arithmetic: i64 Q16 activations,
INT8 weights with dyadic scales `mu * 2^-k`, i128 intermediates, floor shifts,
one truncating division per attention output, and refusal (never wrap-around)
outside the domain of spec §9. WGSL has no 64-bit integers, so the kernels
rebuild exactly this arithmetic from 32-bit parts.

1. **No floating point anywhere.** The shaders do not contain `f32` or `f16`
   (a unit test checks the sources). The only floats in the program are
   wall-clock timings on the host.
2. **Wide integers from u32 limbs.** An i64 is `vec2<u32>` and an i128 is
   `vec4<u32>`, both little-endian two's complement. Wide arithmetic uses only
   `u32` operations: unsigned overflow wraps on every backend, while signed
   overflow is undefined behaviour in MSL. `i32` arithmetic appears only where a
   bound proves it cannot overflow. Products come from four exact 16 x 16
   products (`mul32`). Every shift amount is a constant or is proven to lie in
   `[0, 31]`, because shifts by 32 or more are not portable.
3. **Projections without loss.** The split kernel writes each activation
   `x_j` (`|x_j| <= 2^62`) as eight balanced base-256 digits `c_d` in
   `[-128, 127]`, with `x_j = sum_d c_d 256^d`. This is the CPU limb kernel's
   decomposition with eight digits instead of four, so it covers every storable
   activation. Then `sum_j q_ij x_j = sum_d 256^d S_d`, where each
   `S_d = sum_j q_ij c_dj` is an i8 x i8 dot product.
   - Each `S_d` accumulates in i32. Since `|q c| <= 127 * 128`, no partial sum
     of any subset of a row can overflow while `K <= 132,104`; the engine
     refuses wider rows. Lanes and workgroup reductions may therefore add in
     any order.
   - The digit sums recombine in 64-bit two's complement. This is exact
     because the projection precondition `127 * sum |x| < 2^63`, checked
     exactly in 96 bits, bounds the accumulator.
   - The dyadic epilogue `(acc * mu) >> k` runs in i128 with a floor shift.
   - Planes above the highest non-zero digit are skipped. That changes only
     the work done, never the value.
4. **Order-free reductions.** Every reduction is an exact integer sum or an
   exact maximum. Examples: the 160-bit sum of squares in RMSNorm, the 96-bit
   `sum |x|`, the maximum attention score, the u32 softmax denominator, and
   the i32 digit sums. Thread count, workgroup size and scheduling therefore
   cannot change a bit. There are no float atomics. The only atomic is an
   integer OR into the status word.
5. **The same nonlinear functions.** The exp table is the CPU engine's own
   correctly rounded table; it is uploaded and checked for size, range and
   monotonicity. Interpolation, the sigmoid divisions, the integer square root
   (digit by digit), and every long division use exact integer algorithms.
   Any exact algorithm gives the same integer as the CPU's.
6. **The same refusals.** Every place where the CPU engine returns a domain
   error sets one bit of a status word at the same place in the computation.
   The host reports it as the same kind of refusal and discards the forward
   pass. This covers:
   - the projection input precondition and output bound;
   - the RMSNorm sum, mean and product bounds;
   - the RoPE, SiLU and residual bounds;
   - the attention product and score bounds;
   - KV values outside i32.
7. **What stays on the CPU, and why.**
   - Token selection (argmax and the rp64 penalty), logits hashing (BLAKE3)
     and tokenisation. The protocol hashes every logits vector, so the 1 MB of
     logits is read back on every forward pass anyway.
   - Selection then runs in the CPU engine's own `arith::select`, so it cannot
     drift from the CPU engine.
   - Nothing stays on the CPU because it could not be made exact on the GPU.

Tests that hold this together (they run on every lavapipe/WARP job):

- **Operator known-answer tests** (`modern::gpu::kat`): every kernel against the
  CPU operator, on deterministic pseudo-random inputs. The inputs mix
  model-like magnitudes, wide values, exact edges (0, ±1, ±2^62, i64
  extremes), the projection precondition boundary and out-of-domain inputs. A
  case passes only if both sides return identical integers or both refuse.
- **Whole-model checks on the tiny model.** The forward pass, KV-cache digest,
  generation (both selection rules), batched prefill and per-operation traces
  are compared against the CPU engine.
- **The traced CPU reference** (`trace_forward_cpu`) is checked to return
  exactly `ModernModel::forward`'s logits and KV cache. Its hashes are
  therefore the CPU's ground truth for localisation.

## 3. Operations

| Operation | Kernel | Exact because | Refusal mirrored |
|---|---|---|---|
| Embedding lookup | `embed` | i128 product, floor shift | token outside the vocabulary |
| RMSNorm | `rms_norm` | 160-bit sum of squares, exact u128 long division, digit-by-digit `isqrt`, 192-bit checked product, floor shift | sum ≥ 2^127, mean > 2^92, product outside i128, \|y\| > 2^62 |
| Projection input | `split` | exact 96-bit `sum \|x\|`, eight balanced digits | `127 sum \|x\| ≥ 2^63` |
| Projections and LM head | `gemv` | i8 x i8 digit sums in i32 (bounded), exact recombination, i128 epilogue | \|y\| > 2^62 |
| RoPE (split half), skipped on NoPE layers | `rope` | i128 products, one floor shift per output | \|u\| > 2^62 |
| KV append (i32 cache, GQA layout) | `kv_store` | narrowing check | \|v\| ≥ 2^31 |
| Attention (GQA, two-pass softmax) | `attention` | i128 dot products, 160-bit checked `dot * lambda`, exact maximum, table exp with integer interpolation, exact u32/i64 sums, one truncating division | product outside i128, \|score\| > 2^62 |
| Gated SiLU | `gated_silu` | exact integer divisions for sigma, 192-bit checked product, floor shift | product outside i128, \|a\| > 2^62 |
| Residual | `residual` | i128 addition | \|h\| > 2^62 |
| Token selection, logits hashes, digests | CPU | the CPU engine's own code | penalised logit > 2^62 |

**Shape limits** (the engine refuses anything outside them; SmolLM3-3B is far
inside each):

- projection inputs of at most 132,104 values (exact i32 digit sums);
- at most 2^15 cached positions. This keeps the softmax denominator within
  u32, keeps `sum_j w_j v_j` within i64, and keeps divisors within the
  `d <= 2^31` that the long division needs;
- at most 64 tokens per pass;
- an embedding of at most four storage bindings;
- every buffer within one storage binding, so that indices stay within u32.

## 4. The Proof Kit's GPU mode

`arc-modern gpu-check` is what a volunteer, or the Proof Kit, runs:

```sh
arc-modern gpu-check --package smollm3-3b.arcipkg --tokenizer tokenizer.json \
  --cases scripts/arc_modern/smollm3_cases.json \
  --golden scripts/arc_modern/golden/smollm3-3b.cpu-golden.json \
  --out gpu-result.json [--run-out gpu-run.json] [--gpu-adapter N|NAME] \
  [--self-test-rounds 8] [--trace-forwards 2] [--prefix-forwards N]
```

`--prefix-forwards N` is the quick mode for adapters too slow for the full run
(for example software rasterizers). It checks only the first N forward passes
of the first golden case, teacher-forced with the golden tokens, and still
compares every logits hash exactly. A pass may take up to
`ARC_GPU_WAIT_SECONDS` (default 3600 s) before it is reported as stuck.

It does four things:

1. It runs the operator self-test on the chosen adapter. A driver that
   miscompiles an integer operation fails here, and the failing operator is
   named.
2. It runs the five golden prompts on the GPU and compares every logits hash
   with the pinned CPU golden.
3. On a mismatch, it replays the first divergent case through the traced CPU
   forward and the traced GPU forward, and names the first divergent forward,
   layer and operation, for example `layer12.gate`.
4. It writes `arc.gpu-proof.v1` and exits 0 only on a full match.

The result reports:

- `gpu.adapter`: vendor, device name, backend, driver and driver version,
  device type, and whether the adapter is a software rasterizer;
- `golden`: the expected and actual matrix digests, the match, and a
  per-case first divergent forward;
- `first_divergence`: case, forward, layer and operation;
- `self_test`, `trace_check`, and `adapters_available`;
- `timing.prefill_tok_s` and `timing.decode_tok_s`, with `gpu.speed_label`,
  which states whether the number is a software-rasterizer timing.

`arc-modern gpu-info` lists every adapter wgpu sees.
`arc-modern golden --gpu` writes the same `arc.modern-run.v1` document as the
CPU golden run, plus a `gpu` section.

**The kit's hook** (EX10, `arc-modern proof`, result `arc.proof-result.v1`)
already reserves `--gpu`, the backend name `gpu-wgpu` and
`runs[].adapter = {vendor, device, backend, driver}`. The calls are:

| Kit need | Call |
|---|---|
| Build from the loaded model, keeping it for the CPU runs | `arc_inference::modern::gpu::engine_for(&model, &EngineOptions::default())` |
| Generation with exactly `ModernModel::generate`'s semantics | `arc_inference::modern::gpu::generate_gpu(&mut engine, &model.config, &request)` → `GenerationOutput` |
| `runs[].adapter` | `arc_inference::modern::gpu::adapter_json(engine.report())` |
| `runs[].threads`, `runs[].vector_projections` | `null` for GPU runs |
| Layer/operator localisation | `GpuEngine::forward_traced`, `trace_forward_cpu`, `localize(&model, &mut engine, &tokens, forwards)` |
| Speed | `GenerationOutput::{prefill_seconds, decode_seconds, decode_forwards}`, as for the CPU paths |

## 5. Known risks and how they are handled

- **Driver and compiler bugs.** Integer code paths are exercised much less
  than float paths. The self-test runs before every Proof Kit GPU run. A
  mismatch is reported with the failing operator or the first divergent layer,
  and that adapter is not trusted. Three compiler hazards have already been
  found and designed around:
  - **naga** hoists a WGSL `var` declared without an initializer to the
    function entry, so inside a loop it would keep the previous iteration's
    value. Every local `var` therefore has an explicit initializer, which
    naga re-runs on every iteration.
  - **FXC** (DX12's default compiler in wgpu 25) cannot write a dynamically
    indexed local vector or array inside a loop it cannot unroll. Long
    division therefore produces one 32-bit quotient word at a time as a
    scalar, and the digit split writes each plane word straight to storage.
  - **Metal** (Apple M2 Ultra, macOS 14.6.1, wgpu 25, commit `b2f90797b`): the
    gated-SiLU kernel returned products multiplied by exactly 2^15 + 1 for
    positive gates of 55 to 59 bits, while every other kernel agreed with the
    CPU (§1). Its two `u32` divisions by a per-thread divisor were the only
    native integer divisions in the kernels. Sigma is now computed with the
    same shift-subtract divider as every other quotient, branch-free, and the
    failing inputs are fixed cases of the self-test. The Studio's re-run at
    `7caaeb0a` (lab, 7 October 2026) reports 0 mismatches in 2,242
    known-answer cases with the real-model digests unchanged, so on that
    machine the change resolved it; other GPUs are untested.
- **Overflow bounds.** Every bound in §2–§3 is either proved in the comments
  of the WGSL or enforced by the host at build time (shape limits) or at run
  time (the status word). Out-of-domain inputs are refused, never wrapped.
- **Subgroup operations are not used.** Subgroup sizes, availability and
  semantics vary by vendor and are optional in WebGPU. All reductions use
  workgroup memory and barriers. Integer subgroup reductions would be just as
  exact and are a candidate speed-up once the per-vendor matrix shows where
  they are reliable.
- **`dot4I8Packed`** (exact packed i8 dot product) is not used yet. The locked
  naga 25 does not implement the `packed_4x8_integer_dot_product` language
  extension. The GEMV calls one function, `dot4_i8`. Today it sign-extends
  bytes with u32 arithmetic and takes an i32 `dot`. The engine switches to
  `dot4I8Packed` automatically when `Instance::wgsl_language_features()`
  reports the extension (a future wgpu upgrade). `ARC_GPU_DOT=fallback`
  forces the portable path, and the self-test checks either path.
- **Timeouts.** Windows resets a GPU after about 2 seconds without progress
  (TDR). Dispatches are small (one row group or one head each) and the LM
  head is split into row chunks. On very slow integrated GPUs a single
  forward pass could still approach that limit. Volunteer reports will show
  where.
- **Memory.** The model needs 3.09 GB of GPU memory, plus about 147 KB of KV
  cache per position and some scratch. Upload frees each host tensor right
  after it is uploaded. A traced localisation reloads the CPU model, so it
  briefly needs about one more model copy of system memory. On Macs, Metal can
  use roughly two thirds to three quarters of unified memory.

## 6. What native CUDA, ROCm and Metal paths would add

The portable path is the correctness and cross-vendor reference. Native
backends must follow the same spec and reproduce the same golden digests; a
speed-up that changes a digest is a bug. What they would add, in rough order of
effect:

1. **Hardware int8 dot products.**
   - `dp4a` (sm_61+) or INT8 tensor cores (`mma.sync` s8 with s32
     accumulation, sm_75+; exact unless `.satfinite`) on NVIDIA; `sdot` and
     WMMA int8 on AMD.
   - Today each 4-byte group of weights costs on the order of twenty integer
     instructions per digit plane (sign extension, four multiplies, adds); a
     hardware `dp4a` is one.
   - Prefill is compute-bound: up to 4 digit planes x 3.08 G MACs per token.
     It gains the most, because tensor cores run at hundreds of int8 TOPS.
2. **Native 64-bit integers.** CUDA, HIP and Metal have 64-bit integer types
   and `mulhi`, which would replace the limb emulation in the epilogues,
   RMSNorm, RoPE, attention and SiLU.
3. **Fewer, fused launches.**
   - A SmolLM3-3B forward pass is 707 dispatches today, and the host waits on
     the 1 MB logits read-back every token.
   - Fusing split + GEMV + epilogue and the attention passes, and using
     persistent kernels or CUDA graphs, removes most of the launch and
     synchronisation cost.
   - Batching several requests per weight read (continuous batching, EX12)
     multiplies throughput on the same bandwidth.
4. **Apple GPUs.**
   - Metal has no int8 dot-product instruction before the Metal 4 tensor
     operations, and integer multiply runs at quarter rate.
   - Research-8 notes an exact alternative for a native Metal path: sums of
     int8 x int8 products are exact in FP32 while each block total stays at
     or below 2^24 (K ≤ 1,024 per block), and each block converts to int32.
     It uses floating-point hardware, so the portable path does not, and a
     native path would need its own proof and gate.
5. **Vendor tuning.** Tile sizes, lanes per row, vectorised loads, and
   subgroup reductions per vendor are free to change: none of them changes a
   value.

**The speed gap, as projections, not measurements.** Research-8's roofline is
"decode tok/s ≤ achievable bandwidth ÷ bytes per token": 3.08 GB per token
for SmolLM3-3B INT8, at 58–75% of peak bandwidth.

| Device | Roofline for an INT8 3B GPU kernel |
|---|---|
| RTX 4060 | 51–64 tok/s |
| RTX 3060 12 GB | 67–84 tok/s |
| RTX 4090 | 188–235 tok/s |
| M-Max (Metal) | 72–93 tok/s |
| M4 Max | 99–128 tok/s |

The portable path's real speed on hardware GPUs is not measured yet.
Volunteers' `gpu-check` results will give measured numbers. The structural
overheads above (instruction count without `dp4a`, 707 dispatches, one
read-back per token) suggest it will sit well below these ceilings. That gap
is what the native paths close.

## 7. Toward Kimi-class models (MoE and MLA, EX9)

The kernels are written so that the larger architectures reuse them:

- **GEMV/GEMM.** Every kernel already carries a token dimension (up to 64
  tokens per pass, used by batched prefill). An MoE expert GEMM is the same
  digit-plane GEMM over the tokens routed to that expert: a gather index
  replaces the contiguous token range, and the exactness argument is
  unchanged.
  - INT4 expert weights (Kimi-K2.6's group-32 routed experts) unpack into
    the same i8 lanes before the digit dot products.
  - Group scales become one more exact integer epilogue.
- **Attention.** The two-pass exact softmax does not depend on the cache
  layout. For MLA, the scores run over the latent cache (`kv_lora_rank` 512
  plus the 64-wide RoPE part) with absorbed projections, as i128 dot products
  with the same refusal rules.
- **Routing** must be deterministic integer top-k, with ties broken by expert
  id. It is computed from logits that are themselves exact.

## 8. How community results feed optimisation

Each `gpu-check` result (and each Proof Kit `gpu-wgpu` run) carries the
adapter vendor, device, backend, driver version, dot-product path,
dispatches per forward, prefill and decode tok/s, and the self-test and trace
outcomes. Collected on the Hash Wall, they give:

1. **An exactness matrix by vendor, device and driver.** A failing driver
   comes with the first divergent operation, which turns a bug report into a
   one-kernel reproduction (the operator lab runs that kernel alone).
2. **Measured speed by device class.** This replaces the projections above
   and decides which native backend to build first (for example, CUDA if most
   capable volunteers run NVIDIA).
3. **Per-vendor tuning** (workgroup shapes, lanes per row, subgroup
   reductions). Each change is gated on unchanged golden digests in CI and on
   volunteers' devices.

## 9. Running it

**Common to every OS**

- Rust (the pinned nightly in `rust-toolchain.toml`).
- About 9.3 GB of disk: the 6.2 GB BF16 download, which can be deleted after
  conversion, and the 3.08 GB package.
- At least about 4 GB of GPU memory for the weights, KV cache and scratch, or
  the same amount of free unified memory.

```sh
cargo build --release -p arc-inference --bin arc-modern
python scripts/arc_modern/fetch_source.py --manifest docs/protocol/packages/smollm3-3b.source.json --dir MODEL
./target/release/arc-modern convert --source-dir MODEL \
  --source-manifest docs/protocol/packages/smollm3-3b.source.json --out MODEL/smollm3-3b.arcipkg
./target/release/arc-modern gpu-info
./target/release/arc-modern gpu-check --package MODEL/smollm3-3b.arcipkg \
  --tokenizer MODEL/tokenizer.json --cases scripts/arc_modern/smollm3_cases.json \
  --golden scripts/arc_modern/golden/smollm3-3b.cpu-golden.json --out gpu-result.json
```

**By OS and driver**

| OS | GPU and driver | Notes |
|---|---|---|
| Windows 10/11 | Any DX12 GPU (NVIDIA, AMD, Intel) with a current vendor driver; Vulkan also works (`WGPU_BACKEND=vulkan`) | Without a GPU, wgpu falls back to WARP (software): exact, but too slow for the full run (use `--prefix-forwards`). DX12 compiles the kernels with FXC at start, about a minute on a 4-core machine |
| Linux | Vulkan: NVIDIA proprietary driver, or Mesa RADV (AMD) / ANV (Intel). `vulkaninfo --summary` must list the GPU | Mesa lavapipe (`mesa-vulkan-drivers`) is the software reference used in CI. Hardware GPUs are picked before software adapters automatically |
| macOS 13 or later | Metal on Apple Silicon, or AMD GPUs in Intel Macs | Unified memory: about 3.1 GB for the weights plus the system's working-set limit. 8 GB Macs are tight |

**Choosing an adapter:** `--gpu-adapter 1` (index) or `--gpu-adapter "radeon"`
(name substring), or `ARC_GPU_ADAPTER`. `WGPU_BACKEND=vulkan|dx12|metal`
restricts the backends.

**In CI.** `gpu-portable.yml` runs the following:

- the lint job;
- the lavapipe and WARP jobs (operator tests, the tiny model, gpu-check);
- on pushes to `gpu-portable-*` branches or with the `gpu-proof` label, the
  real SmolLM3-3B package:
  - on lavapipe: all five golden prompts, plus the first case with 16
    tokens per pass;
  - on WARP: the first forward passes and a traced pass, because WARP is too
    slow for the full run within a CI job.

There are no macOS jobs: macOS runners are reserved for releases. Apple GPUs
are covered by volunteers and later by a self-hosted Metal runner.
