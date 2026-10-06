# ARC Proof Kit

**One command checks, on your own computer, that a public AI model gives
exactly the published answer, bit for bit.** It runs the open SmolLM3-3B model
with ARC's integer engine, compares the result with the published fingerprint
and prints `MATCH` or `MISMATCH`. Nothing about your computer is shared unless
you ask for it and confirm.

Why it matters: ARC verifies AI work by checking that independent computers
produce identical results. CI covers a handful of cloud machines (Linux,
Windows, Intel and Apple Silicon Macs). Volunteers cover everything else:
newer Apple chips, AVX-512 PCs, Windows on ARM, other Linux distributions.

- [What the kit does](#what-the-kit-does)
- [Run it: Mac](#mac) · [Windows](#windows) · [Linux](#linux)
- [What MATCH proves, and what it does not](#what-match-proves-and-what-it-does-not)
- [Privacy](#privacy)
- [Speed measurement](#speed-measurement-arcproof-speedv1)
- [The challenge (anti-faking)](#the-challenge-arcproof-challengev1)
- [Result format](#result-format-arcproof-resultv1)
- [What the Hash Wall endpoint must accept](#what-the-hash-wall-endpoint-must-accept)
- [GPUs](#gpus)
- [For developers](#for-developers)

## What the kit does

1. **Checks your computer.** At least 4 GB of free memory and, for a first
   run, 10 GB of free disk.
2. **Downloads the model** (6.2 GB, once) from Hugging Face:
   `HuggingFaceTB/SmolLM3-3B` at the pinned revision
   `a07cc9a04f16550a088caea529712d1d335b0ac1`. Every file must have its
   pinned size and SHA-256. An interrupted download resumes where it stopped.
3. **Converts it on your computer** into ARC's integer format (3.1 GB) with
   ARC's deterministic converter, then checks the result against the published
   package fingerprint (SHA-256 `19c67496…aa91`). The original 6.2 GB of
   weights are deleted afterwards unless you pass `--keep-source`.
4. **Runs the five golden prompts** (fixed public questions, for example "What
   is the capital of France? Answer in one sentence.") on every CPU path your
   processor has: the plain `cpu-scalar` path and the vector `cpu-simd` path
   (AVX2 on Intel and AMD, NEON dot product on Apple and other ARM chips).
5. **Runs one challenge prompt**: a sentence built from a random seed (see
   [the challenge](#the-challenge-arcproof-challengev1)).
6. **Prints the result**: your combined golden digest next to the published
   one, `3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2`,
   `MATCH` or `MISMATCH` for each path, and prefill and decode speeds.
7. **Shares nothing** unless you run it with `--submit` and type `yes` after
   reading the exact data it would send.

On GitHub's 4-core cloud machines the download took about a minute, the
conversion 15 s to 3 minutes, and the five golden prompts 6 to 7 minutes on
the scalar path and 2.5 minutes on the vector path. Your computer will differ.
The model needs about 3 GB of memory while it runs.

## Run it

Until a release ships `arc-modern` binaries, the kit builds itself from the
source code, which needs the Rust toolchain once. The repository pins its
exact Rust version; `rustup` installs it on the first build (several minutes).

### Mac

Apple Silicon or Intel.

```bash
xcode-select --install                                   # once: Apple's command line tools
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # once: Rust
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain
scripts/proof-kit/run.sh
```

### Windows

Windows 10 (version 1803 or later) or Windows 11 on x86-64. (Windows on ARM
should work the same way but has not been tested yet; results from it are
especially welcome.) Install
[Git](https://git-scm.com/download/win), the
[Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
with "Desktop development with C++", and [Rust](https://rustup.rs). Then, in
PowerShell:

```powershell
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain
powershell -ExecutionPolicy Bypass -File scripts\proof-kit\run.ps1
```

`curl.exe` is built into Windows 10 and 11; the kit uses it for downloads.

### Linux

Any distribution with `curl`, `git` and a C compiler (for example
`sudo apt install curl git build-essential` on Debian and Ubuntu).

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # once: Rust
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain
scripts/proof-kit/run.sh
```

### With a release binary (when available)

`arc-modern` carries everything the kit needs, so with a release binary no
Rust toolchain is needed:

```bash
scripts/proof-kit/run.sh --release <tag>          # Mac and Linux
```

```powershell
powershell -ExecutionPolicy Bypass -File scripts\proof-kit\run.ps1 --release <tag>
```

The script downloads `arc-modern-<platform>` and the release's `SHA256SUMS`,
checks the `SHA256SUMS` signature with the same release key `install.sh`
trusts, then checks the binary's SHA-256. No release includes `arc-modern`
yet (v0.8.10 does not); until one does, use the source build above. With any
binary you already trust you can also run it directly: `arc-modern proof`.

### Options

| Option | What it does |
| --- | --- |
| *(none)* | Local run. Nothing is sent anywhere. |
| `--dry-run` | Also prints the exact JSON a submission would send. Sends nothing. |
| `--submit --endpoint URL` | After the run, shows the exact JSON and sends it to the Hash Wall at `URL` only if you type `yes`. |
| `--dir DIR` | Where the model and results live. Default: `~/Library/Caches/arc-proof-kit` (Mac), `~/.cache/arc-proof-kit` (Linux), `%LOCALAPPDATA%\arc-proof-kit` (Windows), or `ARC_PROOF_KIT_DIR`. |
| `--out DIR` | Where the result files go (default `DIR/results`). |
| `--backends cpu-scalar,cpu-simd` | Run only these paths (default: every path this CPU has). |
| `--threads N` | Use N threads (default: all logical CPUs). |
| `--keep-source` | Keep the 6.2 GB of original weights after conversion. |
| `--no-island` | Leave the optional hardware facts for model islands out of the result. |
| `--force` | Run even if free memory or disk looks too small (expect heavy swapping). |
| `--gpu` | Reserved for the GPU path; this version runs the CPU paths only. |
| `--bin PATH`, `--release TAG` | (scripts only) Use this binary, or a verified release binary, instead of building. |

Exit codes: `0` MATCH, `3` MISMATCH, `4` the Hash Wall did not accept a
submission, `1` any other error. Delete the kit directory to remove
everything the kit stored.

## What MATCH proves, and what it does not

**`MATCH` means:** on your computer, on every CPU path it ran, the five golden
prompts produced the same generated words *and* the same internal scores as
the published runs. The golden digest covers the BLAKE3 hash of every one of
the model's 128,256 scores at every step of every prompt, so equal digests
mean bit-identical computation, not just the same final words. The published
digest was produced identically in 28 of 28 CI runs on Linux, Windows and
macOS (Intel and Apple Silicon), on five processor models, and by an
independent Python implementation written from the specification. The kit's
own CI dry run reproduced it on a sixth, an AMD EPYC 9V74, on both kernels.

It also means your computer converted the public weights into exactly the
published package (the conversion check runs first).

**It does not prove:**

- that every other computer matches: it is one more computer, which is the
  point of collecting many;
- anything about GPUs: this version runs CPU paths only;
- that the answers are good: determinism is not quality (the model's quality
  is measured separately: its perplexity stays within 0.4% of the original
  BF16 model on the texts measured);
- anything about ARC's network or its speed.

**`MISMATCH`** is a valuable result. The kit says where the difference starts:
which prompt, which step, and, when another path on your computer matched,
which layer and operation. Keep `results/proof-run.json` (it stays on your
computer) and report it.

## Privacy

- **By default nothing is sent.** The kit downloads the model from
  `huggingface.co` (and, with `--release`, the binary from `github.com`).
  Those sites see an ordinary download request from your IP address.
- **`--dry-run`** prints the exact JSON a submission would contain, and sends
  nothing.
- **`--submit`** contacts the Hash Wall twice: once at the start to fetch a
  challenge (the request carries no data from your computer), and once at
  the end to send the result. Before sending, the kit shows you the exact
  bytes at the terminal and asks you to type `yes`. No option skips this
  question, and without a terminal nothing is sent. The server sees your IP
  address, as with any web request; the result itself has no field for it.
- **What a result contains:** the model and kit versions, the digests, the
  verdicts, speeds, a random submission nonce, your OS name and version,
  processor architecture, CPU and GPU model names, logical CPU count, a short
  list of CPU features, and, unless you pass `--no-island`, coarse hardware
  classes (memory, GPU memory, Thunderbolt 5 on Macs, and download speed
  class). Every value comes from a fixed list or a fixed format; names are
  limited to letters, digits, spaces and `()@.,+/_-`.
- **What a result never contains:** your hostname, user name, IP address,
  serial numbers, MAC addresses, file paths or any free text.
- **Local files:** `results/proof-result.json` is exactly what would be sent.
  `results/proof-run.json` has the full details (generated text, timings); it
  is never sent.

## Speed measurement (`arc.proof-speed.v1`)

The kit measures the five golden prompts on each path, one request at a time,
with the model already loaded and all logical CPUs in use (or `--threads N`):

- **prefill tok/s** = prompt tokens of the five prompts (393) divided by the
  time spent processing them. This engine processes a prompt one token per
  forward pass, so prefill and decode speeds are similar.
- **decode tok/s** = forward passes after the first generated token (86)
  divided by the time spent generating.

Times come from a monotonic clock around the engine's own prefill and decode
loops. The challenge prompt is not timed, so every computer measures the same
work. These are your computer's numbers, not a network benchmark.

## The challenge (`arc.proof-challenge.v1`)

The golden digest is public, so typing a fake `MATCH` would be easy. Each
submission therefore also answers a fresh challenge that only a real run of
the model can answer.

1. **Get a challenge.** The kit calls `GET <endpoint>/challenge` and receives
   `{"challenge_id", "seed", "expires_at", "signature"}`, signed by the server
   and short-lived. `--dry-run` and local runs use the fixed test challenge
   below and contact nothing.
2. **Derive a prompt.** From the seed, a fixed rule picks four words from a
   public word list and fills a fixed, harmless sentence.
3. **Run it and submit.** The kit runs the golden prompts and the challenge
   prompt on every path and submits the challenge digest with the golden
   digest. Every path must produce the same challenge digest.
4. **The server's rule.** The server cannot run the model itself. A submission
   counts only once a second, independent run (a different submission, ideally
   from a different device class or network) reports the same challenge
   digest for the same `challenge_id`. Agreement between independent
   submitters is the check, as in ARC's twin execution.

### Derivation (exact)

- `seed`: 64 lowercase hex characters (32 bytes), decoded to bytes.
- `h = SHA-256("arc.proof-challenge.v1" || 0x00 || seed_bytes)`: the ASCII
  scheme name, one zero byte, then the 32 seed bytes.
- `WORDS`: the 256 lines of
  [`docs/protocol/proof-kit/challenge-words-v1.txt`](protocol/proof-kit/challenge-words-v1.txt),
  in file order. SHA-256 of the file (each word followed by one LF):
  `4eda9812f9ae77ed55b5e73f10506e8f3cf3f294dc1f086c77884be95917ad60`.
- The prompt is
  `"Write one short sentence that mentions " + WORDS[h[0]] + ", " + WORDS[h[1]] + ", " + WORDS[h[2]] + " and " + WORDS[h[3]] + "."`
- It runs as one more case of the golden kind: user message = the prompt,
  rendered with the pinned SmolLM3 chat template, `today` =
  `06 October 2026`, no thinking (`/no_think`), up to 24 generated tokens, EOS
  `[128012]`, selection `rp64-argmax`, case id `challenge`.
- `prompt_sha256` = SHA-256 of the prompt's UTF-8 bytes (lets a server check
  the derivation without running anything).

Test vectors, which every implementation must reproduce:
[`docs/protocol/proof-kit/challenge-vectors-v1.json`](protocol/proof-kit/challenge-vectors-v1.json).
Implementations: Rust `arc_inference::modern::proof::challenge_prompt` (and
`arc-modern challenge-prompt --seed HEX`), and the independent Python
reference `scripts/arc_conformance/proof_kit_reference.py`
(`python3 -m arc_conformance.proof_kit_reference derive --seed HEX`). CI
derives 256 random seeds with both and requires identical text.

**Test challenge** (dry runs and local runs; a Hash Wall must never accept
it): `challenge_id` `test-challenge-v1`, seed
`000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f`,
`expires_at` `2099-12-31T23:59:59Z`, `signature` `unsigned-test-challenge`.
Its prompt: "Write one short sentence that mentions frogs, kayaks, buttons and
teapots." In the kit's CI dry run (Proof Kit run 37507415675, an AMD EPYC
9V74 GitHub runner) both CPU kernels answered "The frog leaped gracefully into
the kayak, causing a button to pop off and land in its teapot." with challenge
digest `fad7f4483e2092bd21f669fad7bc70e6f203792a81ddce440026917ffaba4f6c`.
Every computer should reproduce that digest; local and dry runs print whether
they did.

### Digests

Both digests use the method of `arc-modern golden`. For each case the entry
is `{"id", "logits_digest", "output_hash", "tokens"}` where:

- `tokens`: the generated token ids;
- `output_hash` = BLAKE3 of the token ids as little-endian u32;
- `logits_digest` = BLAKE3 of the concatenated 32-byte BLAKE3 hashes of every
  logits vector (128,256 little-endian i64 values) at every forward position.

The **golden digest** is BLAKE3 of the canonical JSON of the five entries in
order; the **challenge digest** is BLAKE3 of the canonical JSON of the
one-entry list `[challenge entry]`. Canonical JSON: no whitespace, object
keys sorted, ASCII only (Python
`json.dumps(x, sort_keys=True, separators=(",", ":"))`). For these entries
it is exactly
`[{"id":"capital","logits_digest":"…","output_hash":"…","tokens":[791,…]},…]`.

## Result format (`arc.proof-result.v1`)

One JSON object, at most 8,192 bytes as sent, UTF-8, every field present,
no other fields. The kit sends it with two-space indentation and number
lists on one line, and shows exactly those bytes before sending.

| Field | Type and rule |
| --- | --- |
| `schema` | `"arc.proof-result.v1"` |
| `kit_version` | Proof Kit version, `"0.1.0"` (`N.N.N`) |
| `arc_version` | ARC version the binary was built from (`N.N.N`) |
| `nonce` | 32 lowercase hex characters (16 random bytes from the OS), new for every run. One submission per nonce. |
| `model` | `{repo, revision, profile, package_sha256, manifest_blake3}`, exactly the pinned values: `HuggingFaceTB/SmolLM3-3B`, `a07cc9a04f16550a088caea529712d1d335b0ac1`, `arc.hf-llama.i8-dyadic-row.q16.v1`, `19c67496ee23fe5da0e12eb1f22cb17f6386c560071587b5b8dfa68731c0aa91`, `af388d01c3578c5f97238fd74aaa3d0d8194d29fc6fbd99f3aae226064659fa2` |
| `verdict` | `"MATCH"` when every run matches, else `"MISMATCH"` |
| `golden.published_digest` | `3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2` |
| `golden.digest` | The first run's golden digest (64 hex) |
| `golden.cases` | The five case entries of the first run, in order `capital`, `haiku`, `integers`, `primes`, `product-greedy`: `{id, tokens, output_hash, logits_digest}`; `tokens` holds 1 to 24 ids (16 for `product-greedy`) below 128,256. These per-prompt digests show which prompt differs. |
| `challenge.challenge_id`, `.seed`, `.expires_at`, `.signature` | Echoed exactly as the server sent them: id `[A-Za-z0-9._-]{1,128}`, seed 64 hex, `YYYY-MM-DDTHH:MM:SSZ`, signature `[A-Za-z0-9._~+/=-]{1,512}` |
| `challenge.prompt_sha256` | SHA-256 of the derived prompt (64 hex) |
| `challenge.digest` | The first run's challenge digest (64 hex) |
| `challenge.case` | The first run's challenge entry `{id: "challenge", tokens, output_hash, logits_digest}` (1 to 24 tokens) |
| `speed_method` | `"arc.proof-speed.v1"` |
| `runs` | 1 to 4 objects, one per backend, distinct backends, at least one CPU backend; the first is the reference for `golden` and `challenge` |
| `runs[].backend` | `"cpu-scalar"`, `"cpu-simd"` or (later) `"gpu-wgpu"` |
| `runs[].isa` | `"avx2"` or `"neon-dotprod"` for `cpu-simd`; `null` otherwise |
| `runs[].verdict` | `"MATCH"` exactly when `golden_digest` is the published digest and `challenge_digest` equals `challenge.digest` |
| `runs[].golden_digest`, `runs[].challenge_digest` | 64 hex |
| `runs[].prefill_tok_s`, `runs[].decode_tok_s` | Tokens per second, a number in [0, 100000], three decimals |
| `runs[].threads` | Threads used (1 to 4096), `null` for a GPU run |
| `runs[].vector_projections` | `{attempted, accepted}` vector-kernel projections over the golden prompts (`0/0` on `cpu-scalar`; all accepted on `cpu-simd`); `null` for a GPU run |
| `runs[].adapter` | `null` on CPU; for a GPU run `{vendor, device, backend, driver}` (names; `backend` one of `vulkan`, `metal`, `dx12`, `gl`) |
| `runs[].divergence` | `null` when the run matches; otherwise `{case, position, layer, op}`: the golden case id or `challenge`, the forward position, the layer, and the operation (`tokenizer`, `embed`, `attn_norm`, `wq`, `wk`, `wv`, `rope_q`, `rope_k`, `attention`, `wo`, `attn_residual`, `ffn_norm`, `w_gate`, `w_up`, `silu`, `w_down`, `ffn_residual`, `final_norm`, `lm_head`, `select`); any of them `null` when unknown |
| `device.os` | `macos`, `linux`, `windows`, … (Rust's OS name) |
| `device.os_version` | e.g. `macOS 15.1`, `ubuntu 24.04`, `Windows 10.0.26100`, or `null` |
| `device.arch` | `aarch64` or `x86_64` |
| `device.cpu_model`, `device.gpu_model` | Model name as the OS reports it, reduced to letters, digits, spaces and `()@.,+/_-`, at most 64 characters, or `null` |
| `device.logical_cpus` | 1 to 4096 |
| `device.cpu_features` | Distinct values from `avx2`, `fma`, `avx512f`, `avx512bw`, `dotprod`, `i8mm`, `sve`, `sve2` |
| `island` | `null` with `--no-island`; otherwise the coarse facts below |
| `island.memory_class_gb` | Largest of 1, 2, 4, 8, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024 not above the installed memory (allowing 15% that firmware or integrated graphics reserve), or `null` |
| `island.unified_memory` | `true` on Apple Silicon (the GPU uses system memory) |
| `island.gpu_vram_class_gb` | Largest of 1, 2, 4, 6, 8, 10, 12, 16, 20, 24, 32, 40, 48, 64, 80, 96, 128, 192 not above the GPU's own memory (5% allowance), or `null` (none found, or unified memory) |
| `island.thunderbolt5` | Mac only: `true` if macOS reports a Thunderbolt port of 80 Gb/s or more, `false` if only slower ports, `null` elsewhere |
| `island.download_mbps_class` | Largest of 1, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000 Mb/s not above the measured model download speed, or `null` (no download in this run) |

### Example: a real dry run

This is the exact output of `scripts/proof-kit/run.sh --dry-run` in the kit's
CI (Proof Kit run 37507415675, GitHub `ubuntu-latest`, AMD EPYC 9V74, 4
vCPU): 4,680 bytes. Both kernels matched the published golden digest, and
both produced the same test-challenge digest. The model download (6.15 GB in
58 s) gave the `download_mbps_class` of 500. The speeds are that cloud
machine's, not a benchmark.

```json
{
  "arc_version": "0.8.10",
  "challenge": {
    "case": {
      "id": "challenge",
      "logits_digest": "b17675fd936c02067c11510fb1e27bf85fb2ab8bf4dd32f4c6fa6e84125a897d",
      "output_hash": "fc071265009a51e9620f5b3b987385615d3638f83c9d61ad1adcffa8c6e3facb",
      "tokens": [791, 60981, 514, 10395, 79599, 1139, 279, 88078, 11, 14718, 264, 3215, 311, 2477, 1022, 323, 4363, 304, 1202, 1028, 91001, 13, 128012]
    },
    "challenge_id": "test-challenge-v1",
    "digest": "fad7f4483e2092bd21f669fad7bc70e6f203792a81ddce440026917ffaba4f6c",
    "expires_at": "2099-12-31T23:59:59Z",
    "prompt_sha256": "5fc6b7fa32ea6642a8ae0992164040418cd151ca5f854f1f998259d69dcc8748",
    "seed": "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    "signature": "unsigned-test-challenge"
  },
  "device": {
    "arch": "x86_64",
    "cpu_features": ["avx2", "fma"],
    "cpu_model": "AMD EPYC 9V74 80-Core Processor",
    "gpu_model": null,
    "logical_cpus": 4,
    "os": "linux",
    "os_version": "ubuntu 24.04"
  },
  "golden": {
    "cases": [
      {
        "id": "capital",
        "logits_digest": "380651493c7a67fb85c17d2ffb08672c5be2a38eefe1e51988bd783a96659242",
        "output_hash": "9ab86a7a11c164c14e4101cd9b624deb335df356f5f0ca7c5a5d1ae5ad3a5eea",
        "tokens": [791, 6864, 315, 9822, 374, 12366, 13, 128012]
      },
      {
        "id": "haiku",
        "logits_digest": "99e04500cdbe893686f61171bc068c10e4dbe65ceb1ab60742c9602b22e9550b",
        "output_hash": "8e7aaab19309868ee06fd7d4c75c559d3de4c66452a8bff5a9a46f37a7597ff6",
        "tokens": [28671, 306, 24811, 11, 2355, 2123, 596, 26348, 304, 279, 3805, 2345, 198, 9219, 770, 287, 16058, 13, 128012]
      },
      {
        "id": "integers",
        "logits_digest": "ab9ffa0042e0417065bc75630c9647a1427936321f6f58546446cf6c4a6b9a05",
        "output_hash": "230e4460a67d831e847ca4688cd6bae68a101a8061c55dae71709c07def94b77",
        "tokens": [3570, 35884, 374, 53823, 79385, 4028, 2204, 19002, 1606, 279, 3135, 315, 7698, 7677, 527, 11075, 21742, 555, 872, 8026, 44713, 323, 8521, 2766]
      },
      {
        "id": "primes",
        "logits_digest": "63bb9d89aee90b35a1ab5c0ae330de52a58a46d3de21ea05f1320c455f62ff69",
        "output_hash": "fc4fa66506fc0dd722bb8417b1f884111444b54bc2d06b70787db7341f343c0e",
        "tokens": [8586, 527, 2380, 10461, 5219, 7191, 1109, 220, 1041, 1473, 16, 13, 3146, 4645, 96618, 1115, 374, 264, 1664, 22015, 10461, 1396, 11, 323]
      },
      {
        "id": "product-greedy",
        "logits_digest": "9a2a06a1c7b1d8b1b5ad676b9704432ab3d5f1517cecff30fb6b6a7019c41cae",
        "output_hash": "6f40e1ddca6a30489d73600fb2dca92bd5d9d8529eb555a423f2ce99e570e78c",
        "tokens": [1271, 1505, 279, 2027, 315, 220, 1114, 323, 220, 1419, 11, 584, 649, 2804, 279, 47544]
      }
    ],
    "digest": "3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2",
    "published_digest": "3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2"
  },
  "island": {
    "download_mbps_class": 500,
    "gpu_vram_class_gb": null,
    "memory_class_gb": 16,
    "thunderbolt5": null,
    "unified_memory": false
  },
  "kit_version": "0.1.0",
  "model": {
    "manifest_blake3": "af388d01c3578c5f97238fd74aaa3d0d8194d29fc6fbd99f3aae226064659fa2",
    "package_sha256": "19c67496ee23fe5da0e12eb1f22cb17f6386c560071587b5b8dfa68731c0aa91",
    "profile": "arc.hf-llama.i8-dyadic-row.q16.v1",
    "repo": "HuggingFaceTB/SmolLM3-3B",
    "revision": "a07cc9a04f16550a088caea529712d1d335b0ac1"
  },
  "nonce": "bf4b33f80398b9520a45d13522c2cf9b",
  "runs": [
    {
      "adapter": null,
      "backend": "cpu-scalar",
      "challenge_digest": "fad7f4483e2092bd21f669fad7bc70e6f203792a81ddce440026917ffaba4f6c",
      "decode_tok_s": 1.145,
      "divergence": null,
      "golden_digest": "3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2",
      "isa": null,
      "prefill_tok_s": 1.163,
      "threads": 4,
      "vector_projections": {
        "accepted": 0,
        "attempted": 0
      },
      "verdict": "MATCH"
    },
    {
      "adapter": null,
      "backend": "cpu-simd",
      "challenge_digest": "fad7f4483e2092bd21f669fad7bc70e6f203792a81ddce440026917ffaba4f6c",
      "decode_tok_s": 3.2,
      "divergence": null,
      "golden_digest": "3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2",
      "isa": "avx2",
      "prefill_tok_s": 3.238,
      "threads": 4,
      "vector_projections": {
        "accepted": 121187,
        "attempted": 121187
      },
      "verdict": "MATCH"
    }
  ],
  "schema": "arc.proof-result.v1",
  "speed_method": "arc.proof-speed.v1",
  "verdict": "MATCH"
}
```

### Validation rules

A result is valid only if all of these hold (the Rust
`arc_inference::modern::proof::validate_result` and the Python
`proof_kit_reference.validate_result` implement exactly this list):

1. The body is at most 8,192 bytes and parses as one JSON object.
2. Every object has exactly the fields above: none missing, none extra.
3. Every value has its type, format and range; labels use only the label
   alphabet; lists hold distinct values from their fixed vocabularies.
4. `model` and `golden.published_digest` are the pinned values.
5. `challenge.prompt_sha256` is the SHA-256 of the prompt the seed derives.
6. Each `output_hash` is the BLAKE3 of its `tokens`; `golden.digest` is the
   digest of `golden.cases`; `challenge.digest` is the digest of
   `challenge.case`.
7. `runs[0].golden_digest` equals `golden.digest` and
   `runs[0].challenge_digest` equals `challenge.digest`.
8. Each run's `verdict` follows from its digests; the top-level `verdict` is
   `MATCH` exactly when every run's is; `divergence` is `null` exactly for
   matching runs.

Rules 1 to 8 make a tampered or inconsistent result detectable, but anyone
can copy the public golden digest. The challenge rule (a second independent
run must agree) is what makes a fake hard. Device names are reported by the
volunteer's computer and cannot be verified; show them as "reported device".

## What the Hash Wall endpoint must accept

The Hash Wall (the arc.ai site) exposes a base URL, for example
`https://<site>/api/hashwall`. The kit has no default endpoint until the Hash
Wall is live; volunteers pass `--endpoint` (or `ARC_PROOF_ENDPOINT`). The kit
accepts only `https://` URLs (plain `http://` only on 127.0.0.1 or
localhost, for tests), without credentials, query or fragment.

**`GET <base>/challenge`** returns `200` with
`application/json`:

```json
{
  "challenge_id": "c-2026-10-06-7f3a9b",
  "seed": "<64 lowercase hex: 32 random bytes from a CSPRNG>",
  "expires_at": "2026-10-06T19:00:00Z",
  "signature": "<server MAC>"
}
```

- `expires_at` is UTC, `YYYY-MM-DDTHH:MM:SSZ`. A real run takes minutes to
  an hour on slow computers; the kit fetches a fresh challenge and re-runs
  only the challenge prompt if fewer than 2 minutes remain, so 30 to 60
  minutes is enough.
- Sign `challenge_id`, `seed` and `expires_at` together, for example
  `HMAC-SHA256(server_key, challenge_id + "\n" + seed + "\n" + expires_at)`
  as hex. The kit treats the signature as opaque and echoes it.
- To let independent runs confirm each other, hand the same open challenge to
  more than one requester (for example, prefer a challenge that already has
  one submission from a different network) until it expires.
- Extra fields are ignored by the kit and never echoed.

**`POST <base>`** with `Content-Type: application/json` and the result as the
body:

1. Reject bodies over 8,192 bytes (`413`) and anything that fails the
   [validation rules](#validation-rules) (`400`, with the list of problems).
2. Check the signature and that `expires_at` has not passed (`400`).
   `challenge_id` `test-challenge-v1` never verifies.
3. Reject a nonce seen before (`409`); store a salted hash of the nonce, not
   the nonce.
4. Store the accepted fields (digests, verdicts, speeds, device and island
   facts, kit version, challenge id) with the server's own timestamp. Never
   store the submitter's IP address; if rate limiting needs it, keep only a
   rotating hash.
5. Count the submission once another independent submission reports the same
   `challenge.digest` for the same `challenge_id`; until then it is pending.
   If submissions for one challenge disagree, hold all of them for review.
6. Reply `202` with a small JSON body, for example
   `{"status": "pending"}` or `{"status": "verified"}`. The kit prints the
   status code and at most 500 printable characters of the body.

[`scripts/proof-kit/mock_hash_wall.py`](../scripts/proof-kit/mock_hash_wall.py)
implements these rules on 127.0.0.1 for tests (no rate limiting, no
persistence); CI submits to it.

## GPUs

This version runs CPU paths only. The result format already has room for a
GPU run: `backend` `gpu-wgpu`, the `adapter` facts, and the same digests and
divergence fields. When the portable GPU engine lands, `--gpu` will add a GPU
run after the CPU runs, and its digests must equal the CPU digests.

## For developers

- The command: `arc-modern proof` (`crates/arc-inference/src/bin/proof_kit/`);
  the pure parts (challenge, digests, result builder and validator) are in
  `crates/arc-inference/src/modern/proof.rs`.
- Pinned inputs are compiled into the binary: the source and package
  manifests (`docs/protocol/packages/smollm3-3b.*`), the golden prompts
  (`scripts/arc_modern/smollm3_cases.json`), the word list, and the
  per-position golden reference
  (`docs/protocol/proof-kit/smollm3-golden-reference.json`, copied from the
  SmolLM3 CI run 37473148757), which the kit uses to say where a mismatch
  starts.
- Downloads and Hash Wall calls go through the system `curl`; nothing else in
  the kit opens a connection.
- CI: `.github/workflows/proof-kit.yml` runs the unit tests, the scripts'
  checks, and the kit end to end on Linux in `--dry-run` mode (real model),
  then `--submit` against the local mock, confirmed and declined. The same
  dry run on macOS, Windows and Linux ARM runs only with the
  `proof-kit-matrix` label.
- Release binaries: adding `arc-modern-<platform>` assets to the release
  workflow and its `SHA256SUMS` is a separate change for the release process.
