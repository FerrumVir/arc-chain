# Cross-platform determinism proof (real 7B model, CPUs)

**Claim under test:** ARC's canonical integer inference engine produces the
same answer, byte for byte, on different computers. The test runs one real
7-billion-parameter model on four CPU platforms and checks that every run
produces an identical transcript.

**Status:** measured 2026-10-06 in workflow run [37467005539](https://github.com/FerrumVir/arc-chain/actions/runs/37467005539) on commit `be37100b2`. On the full 16 x 32 workload, every completed run produced the same transcript, SHA-256 `d2c8c82b3a82beb166559b78ad26a748cbd6395c2cf13e8f012b03f90fa92872`. That covers Linux, Windows and Intel macOS with both the scalar and the AVX2 kernel, and Apple Silicon with the scalar kernel. Apple Silicon's NEON kernel matched on the smaller smoke test and on 3 of the 16 full-workload prompts; its full-workload run was deferred to free shared CI runners.

## What is compared

| Item | Value |
| --- | --- |
| Model | Llama-2-7B-Chat, `llama-2-7b-chat.Q4_K_M.gguf` from [TheBloke/Llama-2-7B-Chat-GGUF at revision `191239b`](https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/blob/191239b3e26b2882fb562ffccdd1cf0f65402adb/llama-2-7b-chat.Q4_K_M.gguf) |
| Model bytes | 4,081,004,224 bytes, SHA-256 `08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa`, BLAKE3 `934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67` (the same artifact as `TESTNET_MODEL_SHA256` in arc-node and `docs/protocol/packages/llama-2-7b-q4km.manifest.json`) |
| Execution profile | `arc.gguf-llama.i8-per-row.rope-interleaved.v1`, loaded with `load_cached_model_canonical_i8_interleaved_rope`, the loader the native executor uses |
| Generation | `arc.whole-model-generation.greedy.v1`: one internal BOS forward, token-at-a-time prefill, argmax with ties to the lowest token ID, stop after EOS or the token budget |
| Prompts | 16 fixed, harmless prompts in [`scripts/determinism-proof/prompts.json`](../scripts/determinism-proof/prompts.json), tokenized by the GGUF SentencePiece tokenizer (`arc.gguf-llama.spm-score-merge.v1`) |
| Budget | 32 new tokens per prompt |
| Platforms | GitHub-hosted runners `macos-15` (Apple Silicon), `macos-15-intel` (Intel x86_64), `ubuntu-24.04` (x86_64) and `windows-latest` (x86_64) |
| Kernels | The default scalar per-row INT8 projection kernel, and the opt-in SIMD "limb" kernel (`ARC_FAST_CANONICAL_KERNEL`): NEON dotprod on arm64, AVX2 on x86_64. Each runs in its own job. |

The driver is
[`crates/arc-inference/examples/determinism_proof.rs`](../crates/arc-inference/examples/determinism_proof.rs).
It writes a plain-text transcript that contains:

- the model file's BLAKE3 and the engine's digests of what it loaded: the INT8
  weights (`weight_hash`), the Q16 embedding table and norm vectors, and the
  RoPE tables;
- for every prompt, the prompt token IDs;
- for every forward position, the input token, the BLAKE3 of the exact i64
  logits (all 32,000 of them) and the BLAKE3 of the K/V rows written at that
  position;
- the generated token IDs, the engine's output hash, and digests of the final
  logits and the complete K/V cache.

The transcript contains nothing about the machine, the kernel or the timing.
Its SHA-256 is the **combined hash**. It is computed outside the code under
test: by Python's `hashlib` in CI, and by `sha256sum`, `shasum` or
`Get-FileHash` in the run-it-yourself scripts. Two runs agree exactly when
their combined hashes are equal. Machine details, timings and the SIMD
kernel's acceptance counts go to a separate JSON file.

The driver also replays the first prompt through the engine's public
`try_generate_v2_greedy` API and fails if the tokens or output hash differ, so
the recorded loop is the engine's own generation loop.

## Results

Full workload: 16 prompts x 32 new tokens, greedy, in
[run 37467005539](https://github.com/FerrumVir/arc-chain/actions/runs/37467005539), attempts 1-3 on
2026-10-06, on commit `be37100b2`. The Apple Silicon legs ran as six prompt shards per kernel. Two
scalar shards were re-run in later attempts of the same run: one lost its runner to a shutdown signal
before producing output, and the other was stopped when the NEON shards were cancelled.

| Platform (GitHub runner) | CPU reported by the runner | Kernel | Workload | Same transcript | Decode tok/s (CI runner) | SIMD projections accepted |
| --- | --- | --- | --- | --- | --- | --- |
| macOS 15, Apple Silicon (`macos-15`) | Apple M1 (Virtual) | scalar (`scalar-i8xi64`) | full, all 16 prompts | yes | 0.016 | not used |
| macOS 15, Apple Silicon (`macos-15`) | Apple M1 (Virtual) | NEON (`neon-sdot-limb`) | smoke test; full-workload prompts 13-15 | yes | 0.015 | 9,000 of 9,000 (smoke); 32,850 of 32,850 (prompts 13-15) |
| macOS 15, Intel (`macos-15-intel`) | Intel Core i7-8700B @ 3.20GHz | scalar (`scalar-i8xi64`) | full | yes | 0.65 | not used |
| macOS 15, Intel (`macos-15-intel`) | Intel Core i7-8700B @ 3.20GHz | AVX2 (`avx2-limb`) | full | yes | 0.81 | 176,175 of 176,175 |
| Linux, Ubuntu 24.04 (`ubuntu-24.04`) | AMD EPYC 9V74 | scalar (`scalar-i8xi64`) | full | yes | 0.77 | not used |
| Linux, Ubuntu 24.04 (`ubuntu-24.04`) | AMD EPYC 7763 | AVX2 (`avx2-limb`) | full | yes | 1.55 | 176,175 of 176,175 |
| Windows Server (`windows-latest`) | Intel Xeon Platinum 8573C | scalar (`scalar-i8xi64`) | full | yes | 0.93 | not used |
| Windows Server (`windows-latest`) | AMD EPYC 9V74 | AVX2 (`avx2-limb`) | full | yes | 1.44 | 176,175 of 176,175 |

- Every run loaded the same engine state: INT8 weights
  `e43ae7d054493f318e119c2de07d09064fe822ca79cadb04f6d4f339d5e3e72d`, Q16 embeddings and norms
  `d88be1b13dd355d67149a71f3e07ccae55f422a1693642a08126aa6af6f51230`, and RoPE tables
  `9867f7d8d0cceb1a1cc3474f5449060038b3e75a25cb9b7a02048ef496263dd3`. The RoPE tables come
  from each platform's math library, and they match on all four.
- The engine API cross-check passed on every platform and kernel that ran the first prompt.
- The run's compare job reports the Apple Silicon NEON pair as incomplete because its
  full-workload shards were cancelled; every completed cell matches. The smoke test, 2 prompts x 4
  tokens, matched on all eight cells: [run 37458030569](https://github.com/FerrumVir/arc-chain/actions/runs/37458030569).
- Decode tokens per second are forward passes per second on GitHub-hosted runners. The Apple
  Silicon runner has 7 GB of memory and swaps the ~7.3 GiB model, so its rate (about one token a
  minute) measures swapping, not the chip.

## How CI runs it

[`.github/workflows/determinism-proof.yml`](../.github/workflows/determinism-proof.yml)
runs both kernels on all four platforms: eight platform/kernel pairs. Each job
downloads the model from the pinned URL, checks its size and SHA-256, builds
the driver in release mode and runs it through the same script a developer
would use (bash on macOS and Linux, PowerShell on Windows). The Apple Silicon
runner has less memory than the resident model and swaps (about 57 seconds
per forward pass, against about one second on the x86 runners), so each of its
kernels runs as six parallel jobs, each over a sixth of the prompts. Every
prompt starts from an empty K/V cache, and the final job joins the six shard
transcripts into exactly the bytes a single run writes. The final job is green
only if:

- every job completed;
- all eight platform/kernel combined hashes are identical;
- in every SIMD job, the vectorised kernel accepted every projection it was
  offered (none fell back to the scalar kernel), and in every scalar job it was
  offered none;
- every job loaded the pinned model bytes, and the engine API cross-check
  (run by the job that holds the first prompt) passed for every platform and
  kernel.

The final job writes the hash matrix to the job summary and uploads the
evidence: every transcript, every per-runner JSON and a combined
`evidence.json`. The model itself is never uploaded.

To keep normal CI fast, the workflow runs only on a pull request that changes
its own files and carries the `determinism-proof` label, or by manual dispatch
once it is on the default branch.

## Run it yourself

You need about 5 GB of disk for the model, about 8 GB of free memory (the
process peaked at 8.2 GB on Linux; less works, slowly, through swap) and the
Rust toolchain manager `rustup`. The repository pins its toolchain, so the
first build installs it. Run times on the CI runners are listed under Results;
a personal computer will differ.

Fetch one exact commit: the measured commit listed under Results, or any
later commit that contains this page. Then run the script for your platform.

macOS or Linux:

```bash
ARC_SOURCE_REV=<commit>
git init arc-chain && cd arc-chain
git remote add origin https://github.com/FerrumVir/arc-chain.git
git fetch --depth 1 origin "$ARC_SOURCE_REV"
git checkout --detach FETCH_HEAD
scripts/determinism-proof/run-proof.sh            # default scalar kernel
scripts/determinism-proof/run-proof.sh --kernel simd
```

Windows (PowerShell, with the MSVC build tools installed):

```powershell
$ArcSourceRev = '<commit>'
git init arc-chain; Set-Location arc-chain
git remote add origin https://github.com/FerrumVir/arc-chain.git
git fetch --depth 1 origin $ArcSourceRev
git checkout --detach FETCH_HEAD
powershell -ExecutionPolicy Bypass -File scripts\determinism-proof\run-proof.ps1
powershell -ExecutionPolicy Bypass -File scripts\determinism-proof\run-proof.ps1 -Kernel simd
```

The script downloads the model once (or uses `--model PATH` / `-Model PATH`),
verifies its SHA-256, builds and runs the driver, and prints the combined
SHA-256. With the default options it compares that hash with
[`scripts/determinism-proof/expected-sha256.txt`](../scripts/determinism-proof/expected-sha256.txt)
and prints `MATCH` or `DIFFERENT`. The transcript and the run JSON are written
to `target/determinism-proof/`.

## Scope and limits

This is a measurement of four specific CPU platforms, not a proof about every
computer.

- **CPUs only.** GPU backends (Metal, CUDA, WGSL) are not covered.
- **One model, one quantization path.** The source weights are 4-bit GGUF
  (Q4_K_M). The engine dequantizes them and requantizes each row to INT8 when
  it loads the model. Other models and other storage modes (I16, block-INT8,
  Q4, ternary) are not covered.
- **Greedy decoding.** Production generation v2 adds an integer repetition
  penalty on top of the same forward pass; this test uses raw argmax.
- **Four runner types.** Other CPUs (for example RISC-V or 32-bit targets) are
  untested. The SIMD kernel is NEON dotprod on arm64 and AVX2 on x86_64; on
  arm64 the attention dot product also uses an exact NEON path in both jobs.
- **Load-time floating point.** The engine's arithmetic is integer-only, but
  model loading uses floating point. Dequantizing GGUF blocks, requantizing
  rows to INT8 and converting embeddings and norms to Q16 use only exactly
  rounded IEEE operations. The RoPE tables use the platform's `powf`, `sin` and
  `cos`. The transcript records digests of all of these results, so a platform
  difference would show up and be located; it does not prove that other math
  libraries round identically.
- **Determinism is not quality.** Identical output says nothing about whether
  the output is good.
- **Timings are CI-runner measurements.** GitHub-hosted runners are shared
  virtual machines; the Apple Silicon runner has 7 GB of memory, less than the
  ~7.3 GiB resident model, so it swaps. Treat tokens per second as a property
  of these runners, not as a product benchmark.

## If the hashes ever differ

The comparison reports the first transcript line that differs and what it
measures. A header line points at model loading (for example the RoPE tables
or the requantized weights). A `pos` line names the prompt and position of the
first divergent forward pass, and says whether the K/V rows written at that
position already differ (the divergence is at or before a Q/K/V projection or
RoPE) or only the logits differ (attention, MLP, final norm or LM head).
