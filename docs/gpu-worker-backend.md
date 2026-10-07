# GPU backend for community workers (`--gpu-inference`)

A community worker can use a GPU for the dyadic integer profile
(`arc.hf-llama.i8-dyadic-row.q16.v1`, SmolLM3-3B) through the portable WGSL
kernels ([gpu-portable-kernels.md](gpu-portable-kernels.md)). The switch is
**off by default**.

## The safety rule

**The GPU serves only after it has reproduced the CPU golden bit for bit, on
this adapter and driver, in this process. Anything else means the CPU serves,
and the log says why.** A GPU result is always the CPU result: no tolerance,
no digest changes. A speedup that changed a digest would be a bug, and the
gate is built to catch it before the first request.

## The switch

```sh
arc-node ... --gpu-inference [--gpu-adapter N|NAME] [--gpu-self-test-rounds 8]
```

| Flag | Default | Meaning |
|---|---|---|
| `--gpu-inference` | off | Run the GPU gate at worker startup |
| `--gpu-adapter N\|NAME` | best hardware GPU, software rasterizers last | Adapter index or name substring (`arc-modern gpu-info` lists them); needs `--gpu-inference` |
| `--gpu-self-test-rounds N` | 8 | Operator known-answer rounds; needs `--gpu-inference` |

`WGPU_BACKEND=vulkan|dx12|metal` restricts the graphics APIs, as for
`arc-modern`. The flag only acts on a node that runs the community inference
worker (a complete canonical model loaded). On any other node it logs that it
has no effect.

## What the gate does

`arc_inference::modern::gpu::backend::select`, run at worker startup off the
async runtime:

1. **CPU golden.** The CPU runs the self-test workload and must reproduce the
   pinned digest
   `SELF_TEST_GOLDEN = c6349c34493398680a1bd452b1e2e0c848e9246021d9ed7c411fc8f1b0c64193`.
   The workload is a deterministic three-layer model built in process (RoPE,
   NoPE and RoPE layers, grouped-query attention, a tied embedding) and four
   prompts under both selection rules. The digest is BLAKE3 over every case's
   output hash and logits digest. A CPU build that drifted cannot vouch for a
   GPU.
2. **Operator self-test.** The selected adapter runs the known-answer test:
   every kernel against the CPU operator on deterministic inputs, including
   exact edges and domain refusals. It must report no mismatch. This is the
   test that caught the Metal gated-SiLU defect
   ([gpu-portable-kernels.md](gpu-portable-kernels.md) §5).
3. **GPU golden.** The same adapter runs the self-test workload with the
   engine options the worker serves with (batched prefill). Its digest must
   equal the CPU golden exactly.

The GPU is selected only if all three pass. A missing adapter, a device or
shader error, a mismatching operator, or any differing digest selects the
CPU, with a one-sentence reason in the log.

A model that a worker then serves (`ModernBackend::for_model`) is uploaded to
the adapter that passed, which is checked to be the same one. Its first
forward passes are then compared with the CPU forward pass before the first
request. The self-test model is too small to reach the per-chunk splitting of
large matrices, so this check covers that path. While serving:

- a request beyond the GPU's KV capacity is answered by the CPU;
- a GPU execution failure moves the worker to the CPU for good, and the
  request is answered by the CPU;
- a GPU refusal (out of the profile's domain) is re-checked on the CPU. The
  same refusal is returned as is; if the CPU answers instead, the GPU
  disagreed, and the worker moves to the CPU for good.

## What is recorded

| Where | What |
|---|---|
| Log | The decision and its reason (`info` when the GPU serves, `warn` when it does not) |
| `GET /community/worker/status` (local) | `inference_backend` (`arc.inference-backend.v1`): `backend` (`gpu-wgpu` or `cpu`), `reason`, the expected, CPU and GPU self-test digests, operator test counts, the adapter as wgpu reports it, and `proof_adapter` |
| Registered capabilities | Only when the GPU serves: `gpu-wgpu`, `gpu-api-<api>`, `gpu-vendor-<vendor>`, `gpu-device-<device>`, `gpu-driver-<driver>` |
| Proof Kit `runs[]` | `backend: "gpu-wgpu"`, `adapter: {vendor, device, backend, driver}` from `adapter_json` |

The registration request has no other place for the adapter. Coordinators
reject unknown fields, and the request is signed over its exact bytes, so the
facts travel as capability tokens: lower-case letters, digits and hyphens, at
most 32 bytes each, 16 in total, which are the coordinator's existing rules.
A long device name is shortened, so the exact strings are in the local
status. For example, an Apple M2 Ultra registers `gpu-wgpu`, `gpu-api-metal`,
`gpu-vendor-apple` and `gpu-device-apple-m2-ultra`. Metal reports no driver
string, so there is no driver token.

`adapter_json` follows the Proof Kit's label rule (`arc.proof-result.v1`,
PR #149): `{vendor, device, backend, driver}`, each 1 to 64 characters from
letters, digits and ` ()@.,+/_-`, or `null`. `backend` is one of `vulkan`,
`metal`, `dx12`, `gl`. Metal's empty driver is `null`, not an empty string.

## What it does not change

- **Canonical reward-profile jobs run on the CPU.** The worker's network jobs
  use the canonical I8 reward profile (`CachedIntegerModel`). The portable
  kernels implement the dyadic profile only, so no GPU kernel exists for those
  jobs. The gate verifies and records a GPU for the dyadic profile. Network
  jobs reach it only when that profile is served to workers, which is a
  separate protocol change outside this switch.
- No consensus, reward, attestation or registration-format change. With the
  switch off the worker behaves exactly as before: no adapter is opened and
  no capability is added.
- No golden digest changes. The kernels are those of PR #150.

## Tests

| Test | Where it runs | Proves |
|---|---|---|
| `backend::tests::a_digest_mismatch_falls_back_to_the_cpu` | every CI OS (no GPU needed) | a GPU digest that differs from the golden selects the CPU and keeps the evidence |
| `backend::tests::every_other_failure_falls_back_to_the_cpu` | every CI OS | operator mismatches, a drifted CPU, a CPU error and a missing adapter all select the CPU |
| `backend::tests::the_cpu_self_test_reproduces_the_pinned_golden` | every CI OS | the pinned golden, and that the workload generates without a refusal |
| `backend::tests::a_gpu_that_computes_a_different_digest_falls_back_to_the_cpu` | lavapipe, WARP (required), any local GPU | the real gate on a GPU computing a model with one changed weight: digest mismatch, so the CPU serves; the served-model spot check catches the same change |
| `backend::tests::the_gate_selects_an_exact_gpu_and_serves_the_cpu_output` | lavapipe, WARP, any local GPU | an exact adapter is selected and serves tokens and logits hashes identical to the CPU, with the CPU's refusals |
| `backend::tests::the_switch_is_off_by_default` | every CI OS | the default config never touches a GPU |
| `community_worker::tests::gpu_capabilities_*`, `registration_adds_backend_capabilities_*`, `the_status_reports_the_backend_decision` | every CI OS | the coordinator's capability rules, only inference workers add them, and the local status |
| `tests::gpu_inference_is_off_by_default_and_its_options_require_it` (arc-node) | every CI OS | the flag's default and its dependent options |

## Measured (Studio lab, Apple M2 Ultra, Metal, macOS 14.6.1)

On the commit that added the switch: every `modern::gpu` test passed (18, in
release). The gate selected the GPU in 1.10 s (8 operator rounds would take
a little longer than the test's 2). The SmolLM3-3B `gpu-check` (full golden,
batch 16, trace 1 forward) passed: matrix digest
`3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2`, equal to
the CPU golden, 5 of 5 cases, self-test 198 cases and 0 mismatches. Lab speeds
were prefill 38.97 tok/s and decode 22.35 tok/s: one stream, token selection
on the CPU, logits read back every forward. That is one machine, one
measurement, not a general figure.
