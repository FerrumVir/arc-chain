# GPU self-test for community workers (`--gpu-inference`)

**Status: partial.**
- Community jobs do **not** run on a GPU yet.
- `--gpu-inference` (off by default) runs a GPU self-test for the dyadic
  integer profile (`arc.hf-llama.i8-dyadic-row.q16.v1`, SmolLM3-3B) at worker
  startup and reports the result locally.
- The worker's jobs use the network's canonical INT8 reward profile
  (`CachedIntegerModel`), which has no GPU kernels, so they run on the CPU
  either way.
- What GPU serving of those jobs still needs is listed under
  [Remaining work](#remaining-work-community-jobs-on-a-gpu).

## The safety rule

**A GPU may run a profile only after it has reproduced the CPU golden bit for
bit, on this adapter and driver, in this process. Anything else keeps the
work on the CPU, and the log says why.**
- A GPU result is always the CPU result: no tolerance, and no digest changes.
- A speedup that changed a digest would be a bug.
- The gate exists to catch it before the first request.

## The switch

```sh
arc-node ... --gpu-inference [--gpu-adapter N|NAME] [--gpu-self-test-rounds 8]
```

| Flag | Default | Meaning |
|---|---|---|
| `--gpu-inference` | off | Run the dyadic-profile GPU self-test at worker startup |
| `--gpu-adapter N\|NAME` | best hardware GPU, software rasterizers last | Adapter index or name substring (`arc-modern gpu-info` lists them); needs `--gpu-inference` |
| `--gpu-self-test-rounds N` | 8 | Operator known-answer rounds; needs `--gpu-inference` |

- `WGPU_BACKEND=vulkan|dx12|metal` restricts the graphics APIs, as for
  `arc-modern`.
- The flag only acts on a node that runs the community inference worker,
  meaning one with a complete canonical model loaded. On any other node it
  logs that it has no effect.

## What the gate does

`arc_inference::modern::gpu::backend::select` runs at worker startup, off the
async runtime:

1. **CPU golden.** The CPU runs the self-test workload and must reproduce the
   pinned digest
   `SELF_TEST_GOLDEN = c6349c34493398680a1bd452b1e2e0c848e9246021d9ed7c411fc8f1b0c64193`.
   - The workload is a deterministic three-layer model built in process (RoPE,
     NoPE and RoPE layers, grouped-query attention, a tied embedding), with
     four prompts under both selection rules.
   - The digest is BLAKE3 over every case's output hash and logits digest.
   - A CPU build that drifted cannot vouch for a GPU.
2. **Operator self-test.** The selected adapter runs the known-answer test:
   every kernel against the CPU operator on deterministic inputs, including
   exact edges and domain refusals. It must report no mismatch. This is the
   test that caught the Metal gated-SiLU defect
   ([gpu-portable-kernels.md](gpu-portable-kernels.md) §5).
3. **GPU golden.** The same adapter runs the self-test workload with the
   serving engine options (batched prefill). Its digest must equal the CPU
   golden exactly.

The GPU passes only if all three pass. A missing adapter, a device or shader
error, a mismatching operator, or any differing digest is a fail, with a
one-sentence reason. A pass makes the GPU *eligible* for the dyadic profile.
It does not move any community job to the GPU.

## Serving the dyadic profile: `ModernBackend` (library, no worker caller yet)

`ModernBackend` is the generation path for dyadic-profile models behind the
gate. No worker job calls it, because the network does not dispatch
dyadic-profile jobs.

**Before the first request**, it uploads the model to the adapter that passed
(checked to be the same one) and compares the model's first forward passes
with the CPU forward pass. The self-test model is too small to reach the
per-chunk splitting of large matrices, so this spot check covers that path.

**While serving:**
- a request beyond the GPU's KV capacity is answered by the CPU;
- a GPU execution failure moves generation to the CPU for good, and the CPU
  answers the request;
- a GPU refusal (out of the profile's domain) is re-checked on the CPU. If the
  CPU refuses too, the refusal is returned. If the CPU answers instead, the
  GPU disagreed, so generation moves to the CPU for good.

Fault-injection tests prove both "for good" cases (see Tests).

## What is reported

| Where | What |
|---|---|
| Log | The gate's result and reason. `info` when a GPU passed, with "community jobs (canonical INT8 profile) still run on the CPU"; `warn` when it did not |
| `GET /community/worker/status` | `inference_backend` (`arc.community.worker-backend.v1`), described below |
| Registration (`/community/register`) | **Unchanged.** `inference` (or `relay`) only. No GPU facts, whatever the gate found |

The `inference_backend` fields:
- `serving_backend: "cpu"`, `serving_profile` (the canonical profile) and
  `serving_reason`;
- `gpu_inference_requested`;
- `dyadic_gpu_self_test` (`arc.inference-backend.v1`):
  - `gpu_self_test_passed` and `dyadic_backend` (`gpu-wgpu` or `cpu`);
  - `reason`;
  - the expected, CPU and GPU self-test digests;
  - the operator test counts;
  - the adapter as wgpu reports it, and as the Proof Kit's `runs[].adapter`
    (`proof_adapter`).

Privacy: the adapter's vendor, device and driver appear only in the node's
own status endpoint and log. They are not sent to coordinators, so they never
reach the public scoreboard. Anyone who can reach this node's RPC port can
read the status.

## Proof Kit GPU run entry

`arc_inference::modern::gpu::proof_run` builds one `runs[]` object of an
`arc.proof-result.v1` result (PR #149). `arc-modern gpu-check --proof-run-out`
writes it:

```sh
arc-modern gpu-check --package smollm3-3b.arcipkg --tokenizer tokenizer.json \
  --cases scripts/arc_modern/smollm3_cases.json \
  --golden scripts/arc_modern/golden/smollm3-3b.cpu-golden.json --out gpu-result.json \
  --challenge-cases challenge.json --reference-challenge-digest <first run's digest> \
  --proof-run-out gpu-run-entry.json
```

**How the entry is built**, following #149's rules (`build_result`,
`run_matches`, `speed`):
- The full golden run and the challenge case run on the GPU.
- `backend` is `gpu-wgpu`; `isa`, `threads` and `vector_projections` are
  `null`.
- `verdict` is `MATCH` exactly when the golden digest is the published one
  and the challenge digest equals the reference (CPU) run's.
- Speeds use `arc.proof-speed.v1` over the golden prompts, to three decimals.
- `adapter` uses #149's label rules.
- On a mismatch, `divergence` maps the GPU trace localisation to #149's
  operation names (`layer12.gate` → layer 12, `w_gate`).
- `check_run_entry` restates #149's `runs[]` rules, and `gpu-check` refuses
  to write an entry that breaks them.

When `--proof-run-out` is requested, the CLI's `pass` field, printed PASS and
successful exit require the entry's `MATCH` verdict as well as the golden,
operator self-test and trace checks. A challenge mismatch writes both the
`MISMATCH` entry (including divergence evidence) and the failed result before
exiting nonzero. Without proof output, the existing golden-check behavior is
unchanged.

The lavapipe job also runs `scripts/arc_modern/gpu_proof_check.py` on the tiny
fixture: a matching challenge, a wrong reference digest, and a GPU challenge
execution error. The tiny fixture is not the published SmolLM3 golden, so
both emitted entries must remain `MISMATCH`; these tests do not weaken that
published-golden requirement. A CPU-only verdict regression separately uses
the published digest to isolate challenge mismatch from golden mismatch.

**Remaining output limitation:** a challenge execution error is explicit on
stderr and exits nonzero before either the result or run-entry file is written.
The test preserves those logs. Use fresh output paths; the CLI does not remove
pre-existing files on an early error. An assembled schema-valid entry/example
is still not an integrated Proof Kit run.

**Validated against #149 (93e4e368), Studio lab:** a `gpu-check` on Apple M2
Ultra (Metal) gave this entry.
- **Golden:** `3e43f342…49f2`, the published digest.
- **Test challenge** (`test-challenge-v1`, prompt derived by #149's reference
  code): `fad7f448…4f6c`, equal to #149's published CPU `TEST_CHALLENGE_DIGEST`.
- **Verdict:** `MATCH`.
- **Validation:** added as a third run to #149's published dry-run example, the
  result passes both #149 validators:
  - the Python reference (`proof_kit_reference.py validate --expect-verdict MATCH`);
  - the Rust `modern::proof::validate_result`.

  Both reject a tampered GPU entry.

**Not done: the Proof Kit does not record GPU runs yet.** #149 is not on
`main`. Its `--gpu` arm rejects `gpu-wgpu`
(`crates/arc-inference/src/bin/proof_kit/mod.rs:118`, at 93e4e368) and adds
no GPU run. Recording one needs four steps:
1. #149 and this branch on `main`.
2. #149's `Backend::parse` accepting `gpu-wgpu`.
3. Its `--gpu` arm running the golden prompts and the challenge through
   `generate_gpu`, with the same case rendering as the CPU runs.
4. Building the run with `proof_run::gpu_run_entry` (or filling
   `RunSummary { backend: "gpu-wgpu", adapter: Some(adapter_json(..)), .. }`).

## What it does not change

- Community jobs: `community_worker::compute_canonical_job` computes them on
  the CPU whatever the gate found. The offline test checks the output is
  byte-identical with the switch off and on.
- No consensus, reward, attestation, coordinator profile selection or
  registration change.
- With the switch off, no adapter is opened.
- No golden digest changes. The kernels are those of PR #150.

## Remaining work: community jobs on a GPU

There are two routes. Neither is in this PR.

### (a) Serve the dyadic profile to workers

This is #149's network-switch plan,
`docs/arc-chain-v2/smollm3-network-switch-plan.md` on `proof-kit-hash-wall`.
- **Kernels:** the GPU kernels, goldens and gate already exist (this PR,
  #150).
- **Protocol change:**
  - profile and package identities in `crates/arc-types/src/transaction.rs`;
  - commitments in `crates/arc-node/src/native_inference.rs`;
  - the package check (`model_package::verify_loaded_package`);
  - a `ModernModel` executor;
  - validator re-execution with `DyadicMatrix` row partitioning;
  - coordinator dispatch by the new profile;
  - node and desktop download plus conversion;
  - an admission KAT;
  - a new activation record.
- **Worker side once that lands:** load the package, build
  `ModernBackend::for_model`, and route dyadic-profile jobs through
  `ModernBackend::generate`.
- **Decision:** this needs TJ's go decision and its own issue.

### (b) Bit-exact GPU kernels for the canonical INT8 profile

This route needs no consensus change. It covers the forward pass of
`CachedIntegerModel::try_generate` with the profile pinned by
`enforce_canonical_i8_profile`. Each operation needs a kernel that equals the
CPU exactly (all in `crates/arc-inference/src/cached_integer_model.rs` unless
noted):

| Operation | CPU definition | GPU work |
|---|---|---|
| Embedding | `embedding_q16` row lookup (`forward_one_token`) | trivial |
| RMSNorm | `layernorm`: sum of squares in i128, `integer_isqrt` (`integer_lut.rs`, 5 Newton steps), two Q16 multiplies | wide-integer library from #150 (`int.wgsl`), isqrt kernel |
| Projections | `matmul_i8_into` / `dot_i8_i64`: i8 weights × **i64** activations, i64 accumulators, `(acc*scale)>>16` per row | digit-plane decomposition of the i64 activation (as `canonical_simd` does), exact i128 epilogue |
| RoPE | `apply_rope` split-half on Q16 tables (`LegacySplitHalfV0`) | straightforward |
| KV cache | `Vec<i64>` per layer | i64 as u32 pairs, 2× the dyadic kernels' memory |
| Attention | `flash_attention_i64`: score `((dot>>16)*attn_scale)>>16`, online softmax in position order with `integer_exp` LUT interpolation, rescale on each new max, final `out*ONE/running_sum` | must replay the same sequential online-softmax order (a two-pass softmax gives different integers); exact 64-bit division without native `/` (the Metal hazard in [gpu-portable-kernels.md](gpu-portable-kernels.md) §5) |
| SiLU gating | `silu_i64`: sigmoid from the exp LUT with integer division, `(silu(g)*up)>>16` | division-free exact routine, as #150 did for the dyadic gated SiLU |
| Final norm, LM head | `layernorm`, `matmul_i8` | as above |
| Selection | `select_next_token_with_repetition_penalty`, `argmax_i64` (first maximum) | stays on the CPU, as in #150 |

The existing `crates/arc-gpu/src/transformer.wgsl` / `gpu_forward.rs` cannot
be reused for exactness. They use i32 activations requantised to i8, an i8 KV
cache, an `f32 sqrt` norm and a two-pass softmax, and they have only an init
smoke test.

**Then, for route (b):**
- a canonical-profile operator KAT and golden;
- the same gate in the worker;
- `compute_canonical_job` choosing the GPU when it passed;
- lavapipe, WARP and hardware evidence.

Until that work is done, no canonical-profile GPU throughput figure exists.

## Tests

| Test | Where it runs | Proves |
|---|---|---|
| `community_worker::tests::gpu_inference_changes_neither_job_output_nor_registration` (arc-node) | every CI OS; lavapipe with the gate required to pass | the worker job path offline (canonical INT8 model, no network or registration): identical output with the switch off and on (gate on the CI adapter), computed on the CPU; registration identical, `inference` only, no GPU fact; the status keeps serving backend and gate result apart |
| `community_worker::tests::a_relay_registers_as_relay_and_a_fresh_status_has_no_gate_result` | every CI OS | relay registration, and the default status |
| `backend::tests::a_gpu_execution_failure_moves_generation_to_the_cpu_for_good` | lavapipe, WARP (required), any local GPU | injected execution failure: the CPU answers with the exact CPU output, the GPU is dropped, and a later request never reaches it |
| `backend::tests::a_gpu_refusal_of_a_cpu_valid_input_moves_generation_to_the_cpu_for_good` | lavapipe, WARP, any local GPU | injected GPU refusal of an input the CPU accepts: the same |
| `backend::tests::a_digest_mismatch_falls_back_to_the_cpu`, `every_other_failure_falls_back_to_the_cpu` | every CI OS | the gate's rule with fakes |
| `backend::tests::a_gpu_that_computes_a_different_digest_falls_back_to_the_cpu` | lavapipe, WARP, any local GPU | the real gate on a GPU computing a one-weight-altered model fails; the served-model spot check catches the same change |
| `backend::tests::the_gate_selects_an_exact_gpu_and_serves_the_cpu_output` | lavapipe, WARP, any local GPU | an exact adapter passes, and `ModernBackend` returns the CPU's tokens, logits hashes and refusals |
| `backend::tests::the_cpu_self_test_reproduces_the_pinned_golden`, `the_switch_is_off_by_default` | every CI OS | the pin, and the default |
| `proof_run::tests::*` | every CI OS | the run entry's shape, verdict rule, divergence mapping, and that the checker rejects what #149 rejects |
| `tests::gpu_inference_is_off_by_default_and_its_options_require_it` (arc-node) | every CI OS | the flag's default and dependent options |

## Measured (Studio lab: Apple M2 Ultra, Metal, macOS 14.6.1)

These are SmolLM3-3B lab numbers for one stream: token selection on the CPU,
logits read back every forward. They are **not** community-serving throughput
(community jobs run on the CPU) and not evidence toward the Kimi-class target.

- Real-model `gpu-check`: golden `3e43f342…49f2` (5/5 cases), test challenge
  `fad7f448…4f6c`, self-test 198 cases with 0 mismatches.
- Prefill 42.75 tok/s and decode 21.68 tok/s at batch 16 (an earlier run gave
  38.97 and 22.35).
- The gate passed in about 1.1 s with 2 operator rounds.
