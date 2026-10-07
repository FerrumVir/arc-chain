# Kimi-K2.6 on an ARC island: plan, evidence and projections

Status: **plan**, 6 October 2026. Nothing here has run Kimi-K2.6. The engine
work behind it is the integer profile
`arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1`
([integer-profile-mla-moe-dyadic-v1.md](integer-profile-mla-moe-dyadic-v1.md)).
That profile was developed and proven on Moonlight-16B-A3B-Instruct, which has
Kimi K2's text architecture at 1/64 of the parameters and the same
`tiktoken.model` byte for byte.

Every number below carries one of these provenance tags:

| Tag | Meaning |
|---|---|
| **[CI]** | Measured in this pull request's GitHub Actions runs (run IDs in §8). These are hosted runners with 4 vCPUs, no GPU. |
| **[INDEPENDENT]** | A third party's published measurement: research-6, or Artificial Analysis as read for the North Star's market check. |
| **[VENDOR]** | The vendor's own statement or published price. |
| **[MEASURED-PROD]** | A production provider's self-reported figure (research-4). |
| **[CALC]** | Arithmetic from the stated inputs, with the formula shown. A projection, not a measurement. |
| **[UNVERIFIED]** | An assumption or a figure nobody has validated. |

Sources: `outputs/arc-planet-scale-20261005/sources/research-6-autocluster.md`
("r6"), `research-4-inference-scale.md` ("r4"), `research-2-supply.md` ("r2"),
`research-8-engine-speed.md` ("r8"); the Kimi-K2.6 `config.json`,
`model.safetensors.index.json` and `docs/deploy_guidance.md` at revision
`7eb5002f6aadc958aed6a9177b7ed26bb94011bb`; Kimi-K3's `config.json`,
`modeling_kimi_linear.py`, safetensors index, README and LICENSE at revision
`f831ab66814297da540d832a5235f8e904f29d06`; NVIDIA's RTX PRO 6000 page; and the
market check in `outputs/arc-proof-sprint-20261006/NORTH-STAR.md` (Artificial
Analysis, Kimi K2.6 providers, median of the past 72 hours, read by work-99).
All were read on 6 October 2026.

## 0. Bottom lines

1. **What is proven [CI].** The Kimi-K2 text architecture runs on ARC's
   deterministic integer engine, and the results do not depend on how the
   work is split:
   - multi-head latent attention, sigmoid routing with a correction bias,
     shared plus routed experts, and integer routing weights;
   - the same bytes for 1, 2 and 4 pipeline stages run as separate processes,
     for tensor-parallel row and K splits, and for expert-parallel splits;
   - one stage can be replayed by a verifier that holds only that stage's
     weights, on a different OS and ISA.

   The evidence is on Moonlight-16B-A3B and two tiny models (§8). Kimi-K2.6
   itself (1T parameters) has not been run.
2. **Speed.** Nobody has measured ARC on Kimi-class hardware. The North Star
   has two bars per answer: **Milestone A, ≥ 59 tok/s** (Moonshot's own API)
   and **Milestone B, ≥ 186 tok/s** (the slowest of the fast providers).
   - Others have measured these Kimi-class speeds:
     - Kimi K2 Thinking at about **30 tok/s** on 4 Mac Studio M3 Ultra (exo/MLX, RDMA over Thunderbolt 5) [INDEPENDENT];
     - Kimi-K2.6 at **58.9 tok/s** in production on Chutes [MEASURED-PROD];
     - Kimi-K2.6 at **186–275 tok/s** from Parasail, CoreWeave, Azure and Nebius, and **59 tok/s** from Moonshot [INDEPENDENT, Artificial Analysis].
   - Under r6's calibration, Mac Studio islands project to **23–33 tok/s**.
     They reach neither milestone (§3).
   - The only configuration in our evidence projected above 59 tok/s is **one
     8-GPU box**, at about 81 tok/s (§3). That projection is unvalidated, and ARC
     has no GPU kernels yet.
   - **No configuration we can project reaches 186 tok/s** under r6's
     assumptions. Milestone B is a GPU-engineering target (§3.1), not a hardware
     purchase.
3. **Price.** All figures here are projections.
   - On a 2× M3 Ultra island, electricity alone costs **$0.41–1.24 per million
     output tokens** (r6 [CALC]). The Kimi-K2.6 API costs **$2.45–4.60** (r4
     [VENDOR]).
   - Counting hardware, no configuration in r6 reaches the North Star's
     **≤ $0.60** all-in: Mac islands cost $12–37/M and a 22× RTX 5090 LAN rig
     costs $0.99–1.63/M.
   - $0.60/M needs either owners whose hardware is already paid for or about
     twice r6's GPU-rig throughput per dollar (§5).
4. **The decision for TJ is hardware for one lab island (§7).** The options:
   - 2–4 Mac Studio M3 Ultra (measured class, about 30 tok/s, below the
     North Star);
   - one 8-GPU box with at least 768 GB of GPU memory (the only path projected
     above 59 tok/s);
   - renting Moonshot's reference node (8× H200) for the first experiment.

   No purchase is proposed here. Each option needs TJ's explicit yes and
   quotes.
5. **Kimi K3 changes the sizing (§9).** Its weights need about **1.50 TB**,
   it reads about **3.6× as many bytes per token** as K2.6, and its attention is
   a new architecture (Kimi Delta Attention). This pull request recognises K3's
   configuration and lists exactly what is missing; it does not run K3. A Mac
   island for K3 needs **four** 512 GB machines (§9). Which model to launch with,
   and its licence terms, are TJ's call.

## 1. Kimi-K2.6: what has to fit

**Shape [VENDOR config.json].** Kimi-K2.6 is `KimiK25ForConditionalGeneration`:
a DeepseekV3 text model plus a MoonViT vision tower that text serving does not
use.
- 61 layers (1 dense, then 60 MoE).
- Hidden width 7,168 and 64 heads.
- Query LoRA rank 1,536; KV latent rank 512; RoPE width 64.
- 384 routed experts, 8 per token, plus 1 shared expert; expert width 2,048.
- Routing scale 2.827; RMS ε 1e-5.
- YaRN RoPE with factor 64; vocabulary 163,840.
- Routed experts are stored as INT4, group 32, symmetric (compressed-tensors
  `pack-quantized`). Everything else is BF16. The 64 shards total
  595,148,192,736 bytes.

**Parameters and memory [CALC from that config].**

| Component | Parameters |
|---|---|
| Embedding + LM head (untied) | 2 × 1.174 B |
| Attention, per layer (incl. LoRA query, latent KV) | 101.1 M |
| Dense MLP (layer 0) | 396.4 M |
| MoE layer: router + shared expert + 384 routed experts | 16.958 B (routed 16.911 B) |
| **Total text model** | **1,026.4 B** (routed experts 1,014.7 B = 98.9%) |

| Weight format | Whole model | Per stage, S = 2 | S = 4 | S = 8 | S = 22 |
|---|---|---|---|---|---|
| This PR's v1 profile: INT8 everywhere (+ ~5 B of scales per row) | 1,028 GB | 514 GB | 257 GB | 128 GB | 47 GB |
| INT4 g32 routed experts (K2.6's native values) + INT8 elsewhere | 582 GB | 291 GB | 146 GB | 73 GB | 26 GB |

- The second row matches r6's 582 GB [CALC]. It is the format a Kimi island
  needs. This PR defines it as the §13 variant of the profile and checks it on
  Moonlight (§6, §8).
- Bytes read per token at batch 1, INT4 experts: about 22.6 GB [CALC]. r6 has
  22.3 GB.
- One MoE layer is about 9.7 GB at INT4 experts and 17.1 GB at INT8 [CALC].
  That is the granularity of a verifier's slice.
- KV cache per token is `61 × (512 + 64)` values [CALC]:
  - 137 KiB in this profile's i32 cache;
  - 34.3 KiB as INT8;
  - 576 MB at 4,096 positions (i32);
  - 18.4 GB at 131,072 positions (i32).
- The stage boundary carries the exact Q16 residual stream: 7,168 × 8 B =
  **56 KiB per position** [CALC]. r6 assumed 14 KiB of INT16 activations; §4
  has the consequence.

## 2. How Kimi-K2.6 would run on an island

The splitting rules follow r6 §2.4/§6.1 and the North Star:
- **tensor- and expert-parallel splits only inside an island** (Thunderbolt-5 RDMA or a PCIe/NVLink box);
- **pipeline stages between boxes, at most across one metro**;
- **never across regions inside one request**.

The integer engine makes every one of these splits exact:

| Split | Why the bytes cannot change | Evidence |
|---|---|---|
| Pipeline (PP), any layer cut | Each stage is a pure function of its input boundary. The boundary file carries the complete residual state. | 1/2/4 stages, separate processes [CI] |
| Tensor-parallel, output rows | Each row's dot product and epilogue are independent. | Row split [CI, unit test] |
| Tensor-parallel, input columns (K split) | Exact integer partial sums are added before the one shared epilogue. | K split [CI, unit test] |
| Expert-parallel | Routing is integer with lowest-index ties. Routed outputs are combined as one exact sum of `w·y`, then rounded once. | Experts on two simulated devices [CI, unit test] |

**Layouts, Kimi-K2.6 at INT4 experts:**

| Island | Memory | Layout | Fits? |
|---|---|---|---|
| 2× Mac Studio M3 Ultra 512 GB, Thunderbolt 5 RDMA | 2 × ~410 GB usable (r6 assumes 0.8) | TP2 (or EP2) inside the island | Yes: 291 GB per Mac plus KV. The v1 INT8 profile needs 3 Macs. |
| 4× Mac Studio (2× 512 GB + 2× 256 GB, as measured by Geerling) | ~1.2 TB usable | TP4; Apple RDMA needs a full mesh of at most 4 Macs (r6 §1.2) | Yes |
| One box, 8× RTX PRO 6000 Blackwell 96 GB | 768 GB [VENDOR] | TP8 over PCIe | Yes: 73 GB per GPU plus KV |
| One box, 8× H200 | Moonshot's reference deployment: "H200 single node with TP8" (vLLM and SGLang) [VENDOR] | TP8 | Yes |
| Metro pair: 2 islands in two buildings | per island as above | PP2 between sites, TP inside each | r6 T2: about 15 tok/s at 15 ms RTT [CALC] |

## 3. Expected speed: measured vs projected

| Configuration | Single-stream tok/s | Type | Source |
|---|---|---|---|
| Kimi K2 Thinking, 4× M3 Ultra, exo 1.0 + MLX, RDMA over TB5 | ~30 | measured by others | [INDEPENDENT] r6 §1.1 |
| DeepSeek-R1 671B Q4, 1× M3 Ultra 512 GB, llama.cpp | 20.21 | measured by others | [INDEPENDENT] r6 §1.1 |
| Kimi-K2.6 on Chutes (datacentre GPUs, production average 2026-09-26 to 10-04) | 58.9 | measured, self-reported | [MEASURED-PROD] r4 §5.4 |
| Kimi-K2.6, fast providers: Nebius 275, Azure 247, CoreWeave 205, Parasail 186 (median of 72 h, read 6 Oct; hardware not stated) | 186–275 | measured by a third party | [INDEPENDENT] Artificial Analysis, via NORTH-STAR.md |
| Kimi-K2.6, Moonshot's own API; Novita 72 | 59 | measured by a third party | [INDEPENDENT] Artificial Analysis, via NORTH-STAR.md |
| 2× M3 Ultra, TP2, TB5 RDMA | 23.4 | projection | [CALC] r6 §2.5 |
| 4× M3 Ultra, TP4 | 32.8 (vs ~30 measured) | projection | [CALC] r6 §2.5 |
| 2× M3 Ultra in two homes, PP2, 15 ms RTT | 15.5 (≈20 with speculation) | projection | [CALC] r6 §2.5 |
| One 8× RTX PRO 6000 box, TP8 | ~81 (upper bound ~159) | projection, unvalidated | [CALC] below |
| 22× RTX 5090 across a LAN, PP22 | 25–75 | projection, unvalidated | [CALC] r6 |

**Why Mac islands stop short of 59 tok/s [CALC].** r6 models a token as
`t = bytes / (N × BW_eff) + 122 × c`:
- 22.3 GB read per token;
- BW_eff = 456 GB/s per M3 Ultra, calibrated on DeepSeek-R1;
- 122 sequential collectives per token (2 per layer);
- c = 0.15 ms per collective on TB5 RDMA, calibrated on exo runs (r6 also
  derives up to 0.193 ms from the Kimi run).

The collective term alone is 18.3–23.5 ms, which caps a single stream at
**43–55 tok/s even with infinite memory bandwidth**. With bandwidth included,
the projection is 23–33 tok/s. Kimi-K2.6 has no MTP head, so speculation needs
a separate draft model. r6 puts the gain at 1.3–1.4× on shallow pipelines,
which is about 40 tok/s. The North Star's 59 tok/s is therefore not a Mac-island
target on current Thunderbolt collectives. Mac islands remain the cheapest way
to *fit* Kimi.

**One 8-GPU box [CALC, unvalidated].** The inputs, the same assumptions r6
uses for its "3 boxes × 8 RTX 5090" row:
- 22.6 GB per token split over 8 GPUs gives 2.8 GB per GPU;
- per-GPU effective bandwidth is 1,108 GB/s, the RTX 5090's calibrated llama.cpp
  efficiency (r2). The RTX PRO 6000 has the same 1,792 GB/s peak [VENDOR];
- 122 PCIe all-reduces at about 30 µs each give 3.7 ms;
- r6's fixed 0.1 ms per layer adds 6.1 ms.

That gives `t ≈ 2.5 + 3.7 + 6.1 ≈ 12.3 ms`, or **about 81 tok/s**. Without
the fixed per-layer overhead it would be about 159 tok/s. Every input here is
an assumption until measured.

The projection needs **integer CUDA kernels that run as efficiently as
llama.cpp's float kernels**, and ARC has no CUDA LLM kernel today (r8 §1.1).
Moonshot's guide gives no speed figure for its 8× H200 reference node.

### 3.1 What Milestone B (≥ 186 tok/s) would take [CALC]

186 tok/s leaves **5.4 ms per token**. Against that budget:
- **Mac islands.** The Thunderbolt collective term alone is 18.3–23.5 ms (122
  collectives at 0.15–0.193 ms). That is 3.4–4.4 times the whole budget, before
  any weights are read. No Mac island reaches Milestone B.
- **One 8-GPU PCIe box.** The §3 model gives about 12.3 ms (81 tok/s). r6's fixed
  0.1 ms per layer alone is 6.1 ms, more than the budget. Even with that
  overhead removed (the 159 tok/s bound), it needs speculative decoding at
  1.17× or better to pass 186. r6's 1.3–1.4× would give 207–223 tok/s, but
  both inputs are unvalidated.
- **What is needed:**
  - per-layer overheads far below r6's assumption, which means fused integer
    kernels with collectives that take microseconds, not milliseconds
    (NVLink-class links rather than PCIe);
  - speculative decoding that is exactly equal to greedy decoding (EX12);
  - the measured float baseline of a node like Moonshot's 8× H200 reference
    (option D) to know the ceiling.

  The providers at 186–275 tok/s are datacentre GPU clouds. The Artificial
  Analysis page does not state their hardware or software, so we cannot copy a
  configuration from it.

**ARC's own engine today [CI].** The CPU-only integer engine runs Moonlight
(2.6 GB of INT8 weights read per token) on a 4-vCPU CI runner at the speeds in
§8. Kimi reads about 12× more bytes per token [CALC]. Without GPU (or tuned
Metal) kernels, ARC would serve Kimi-K2.6 at well under 5 tok/s per island;
that bound is CALC, scaled from the CI figures.

## 4. How community verifiers check an island

This follows r6 §4.2, with the pieces this pull request implements:

1. **Commit.** For every request, each stage commits:
   - `activation_hash` per position for its output boundary (BLAKE3 of the
     exact Q16 vector, spec §6.2);
   - `boundary_digest` over the positions.

   The last stage also commits logits hashes and its tokens. The island posts
   one root per epoch on chain (r6 §6.5).
2. **Identify the weights.** The stage manifest pins a digest per segment:
   tables, embedding, each layer, head.
   - The **model root** commits to the whole model independently of how it is
     split into stages.
   - A verifier converts its slice on its own machine from the public BF16/INT4
     shards, downloading only the shards of its layers.
   - It then checks the slice's segment digests against the manifest.

   CI does exactly this for Moonlight layers 25–26 + LM head, from 2 of 27
   shards.
3. **Sample.** A beacon picks (request, stage, positions) with probability p:
   1% by default, 5% for new islands and 100% for probation (r6 §6.8). Auditing
   every stage at rate p costs about p × a full replay, 0.4–4% of compute at
   p = 1–5% (r4 §3.2).
4. **Replay.** The verifier runs its stage once over the committed input
   boundary (spec §6.4) and compares hashes. Any difference names the first
   faulty stage: there is no threshold and no false positive.
   - The verifier holds 1/S of the model: 26 GB for a 22-stage Kimi pipeline,
     73 GB for 8, 146 GB for 4, 291 GB for 2 (INT4 experts, [CALC]).
   - At this profile's layer granularity, a single 24–32 GB GPU or a 64 GB Mac
     can verify 2–3 layers.
5. **Boundary storage [CALC].** Exact boundaries are 56 KiB per position for
   Kimi. Keeping every boundary for the audit window would be 4× r6's INT16
   estimate: about 20 GB per hour per boundary at 100 tok/s.
   - The cheaper protocol is to commit only the 32-byte per-position hashes and
     keep full boundaries for sampled requests only.
   - Alternatively, re-derive a stage's input by replaying the stages before it.

   Choosing between them is protocol work, not engine work.

## 5. Price per million output tokens (projections)

| Configuration | $/M output | What it covers | Source |
|---|---|---|---|
| Kimi-K2.6 APIs on OpenRouter: Inceptron 2.45, Chutes 2.85 (INT4), Venice 3.50, Moonshot 4.00, Phala 4.60 | 2.45–4.60 | list price | [VENDOR] r4 §1.3 (18 providers list 2.40–4.60 in r6 §5) |
| 2× M3 Ultra, Kimi decode, single stream (~23 tok/s, 560 W) | 1.24 | electricity only | [CALC] r6 §5 |
| 2× M3 Ultra, 8 concurrent (~70 tok/s aggregate) | 0.41 | electricity only | [CALC] r6 §5 |
| 2× M3 Ultra, with hardware amortised over 3 years at 30% use | 12.18–37.08 | electricity + hardware | [CALC] r6 §5 |
| 22× RTX 5090 LAN rig, ~1,500 tok/s aggregate | 1.63 (30% use), 0.99 (60%) | electricity + hardware | [CALC] r6 §5 |
| Renting the same 22 RTX 5090s | 2.20 | rental | [CALC] r6 §5 |

The North Star is **≤ $0.60/M including node-owner payouts** [CALC]:
- **Electricity floor.** $0.60 per million tokens at $0.1831/kWh allows at most
  **11.8 J per token** for all costs. A batch-8 Mac island uses about 8 J per
  token ($0.41/M) for electricity alone, leaving $0.19/M for hardware and
  payout. That works only for owners whose Macs are already paid for.
- **New hardware.** r6's best full-cost configuration is $0.99/M (22 GPUs, 60%
  use, 10 kW). To reach $0.60/M it would need about **twice the aggregate
  tokens per second per dollar and per watt**.
- **What would have to be true** for an 8-GPU box (4.8 kW of GPU TDP [VENDOR];
  price not in our research): its batched Kimi throughput must be measured
  (W11 in r6), and integer kernels must keep up with float ones. Until then
  $0.60/M is a target, not a projection.

## 6. What GPU kernels need for islands to be fast

These must compute **the same integers** as the CPU engine. Any speedup that
changes a digest is a bug (North Star rule 1).

| Kernel / piece | Requirement | Status |
|---|---|---|
| **INT4 g32 expert GEMM/GEMV** | K2.6's experts are INT4 with BF16 group-32 scales. Each value × scale is an exact dyadic rational, so an exact integer form exists: per-group INT32 accumulation, then the scale's 8-bit mantissa and exponent applied by an exact shift. | Specified (profile §13, identity `…i4g32-experts.q16.v1`), with a CPU reference in Rust and Python and a quantiser for BF16 sources. CI results are in §8. Reading K2.6's `weight_packed` / `weight_scale` directly (lossless repacking) is not implemented. |
| **MXFP4 expert GEMM/GEMV (Kimi K3)** | E2M1 values with power-of-two group scales: 2 × an E2M1 value is an integer in [−12, 12], so the exact form is the §13 sum with shift-only scales | Not started (profile §11.2) |
| **INT8 expert GEMV** | Exact INT32 accumulation of int8 × activation limbs (r8 §3.1, the limb trick: `sdot` / `dp4a` / IMMA), dyadic epilogue `(acc·μ) >> k` | CPU NEON/AVX2 limb kernels exist and are exercised here (SIMD census 100% accepted [CI]). No CUDA or Metal LLM kernel. |
| **MLA attention (absorbed)** | Per head: `W_UK^T q`, latent dot products, two-pass softmax (already order-free, so split-K is legal on GPUs), weighted latent sum, then `W_UV`. Cache of 576 i32 per position per layer. | CPU reference only. |
| **Deterministic routing** | Integer router logits (INT16 rows), Q16 sigmoid, Q32 keys, lowest-index ties, Q32 weights, one exact combine | CPU reference. Trivial on GPU (E ≤ 384 per token). |
| **Exact collectives** | Tensor-parallel all-reduce of exact INT32/i64 partials. Expert-parallel exchange of exact `w·y` partials. NCCL in integer mode (no float reduction). | Proven on CPU [CI]. Not built on GPU. |
| **Activation format** | Q16 i64 residual stream; GPUs want A16 limbs (r8 v2) | Spec change, new identity |
| **YaRN tables** | K2.6 uses YaRN (factor 64). Only preparation changes (`ω_i`, λ); the forward pass reads both from the package. | Not started (profile §11) |
| **Gate** | G6 GPU parity (r8 §5.4): the same golden digests from CUDA and Metal as from the CPU engine, on the hash matrix | Not started |

**Ordering.**
1. Finish K2.6 on the CPU path. The INT4-expert variant is in this PR. Still
   needed are YaRN preparation, the lossless reader for K2.6's packed INT4
   experts and the `language_model.` prefix, then synthetic K2.6-shaped stages.
2. CUDA kernels (`dp4a`/IMMA) on one 8-GPU box.
3. Metal kernels for Mac islands. r8 §5.2 lists these as exact FP32 sums of
   int8 products in blocks of ≤ 1,024.

## 7. The hardware decision TJ must make

All of these are lab hardware for r6's W10/W11 experiments, not network
capacity. None is proposed for purchase here; each needs TJ's explicit yes.

| Option | What it buys | Speed evidence | Cost evidence |
|---|---|---|---|
| **A. 2× Mac Studio M3 Ultra 512 GB + Thunderbolt 5** | Kimi-K2.6 fits at INT4 experts (291 GB per Mac). Smallest island that fits. | Projected 23 tok/s (r6). Below the North Star. | ~$23.4k (2 × $11,699, Geerling's configuration [VENDOR], r6) |
| **B. 4× Mac Studio (2× 512 GB + 2× 256 GB)** | The exact cluster measured at ~30 tok/s on Kimi K2 Thinking | ~30 tok/s measured [INDEPENDENT]. Below the North Star. | "just shy of $40,000" [INDEPENDENT] |
| **C. One box, 8× RTX PRO 6000 Blackwell 96 GB (768 GB)** | The only configuration in our evidence projected above 59 tok/s (~81 [CALC], unvalidated). Also the box on which ARC's first CUDA integer kernels would be qualified. | Projection only | Not in our research. Needs quotes. GPU TDP 8 × 600 W [VENDOR]. |
| **D. Rent an 8× H200 node by the hour** | Moonshot's reference deployment; a float baseline for speed and throughput before buying | Production class (Chutes 58.9 tok/s [MEASURED-PROD]) | Not in our research (r2 has H100 SXM at a $3.34/GPU-hour median on Vast). Needs quotes. |

Against the two speed milestones and Kimi K3 [CALC, §3, §3.1, §9]:

| Option | Milestone A (≥ 59 tok/s) | Milestone B (≥ 186 tok/s) | Holds K3 (~1.50 TB)? |
|---|---|---|---|
| A. 2× Mac Studio | No (23 projected) | No | No (0.82 TB usable) |
| B. 4× Mac Studio (2× 512 + 2× 256 GB) | No (~30 measured) | No | No (1.23 TB usable) |
| B′. 4× Mac Studio 512 GB (~$46.8k) | No for K2.6 (33 projected) | No | Yes (1.64 TB usable); 12–14 tok/s projected |
| C. 8× RTX PRO 6000 | Projected (~81, unvalidated) | Not projected; only the no-overhead bound plus speculation passes it | No (768 GB) |
| D. Rented 8× H200 | Moonshot's reference node for K2.6; its speed is not published | Unknown; this is how to measure the float ceiling | Needs ~188 GB of weights per GPU; check the quote |

What each option proves first:
- **A or B:** Kimi fits, together with island formation, stage commitments and
  replay on real hardware (W11, using MLX for speed and ARC for the
  verification path).
- **C:** whether ≥ 59 tok/s and the throughput needed for $0.60/M are
  reachable with exact integer kernels.
- **D:** the float speed and batching ceiling on vendor hardware, with no
  capital.

## 8. CI evidence in this pull request

Everything in this section is **[CI]**: measured on GitHub-hosted runners
(4 vCPUs, no GPU; the CPU model is the one the job printed), single stream,
batch 1, the model read from a memory-mapped package, no kernel tuning beyond
the existing NEON/AVX2 limb kernels. These are Moonlight-16B-A3B figures, not
Kimi-K2.6 figures and not island figures. The three machines have different
CPUs, so their speeds are not comparable to each other; only the digests are.
Throughput is stated per the North Star's rule: output tokens per second
(decode steps of one sequence) and prefill tokens per second separately; no
processed-token figure is claimed.

### 8.1 Run 37522433467: commit `91a75f714`, 6 October 2026, INT8 dyadic profile

<https://github.com/FerrumVir/arc-chain/actions/runs/37522433467>

| Check | Runner, as reported by the job | Result |
|---|---|---|
| Rust converter vs the independent Python preparer, whole model (27 shards, 31.9 GB BF16 → 16,010,951,360 B INT8 package) | ubuntu-latest: Intel Xeon Platinum 8573C, 4 vCPU, 16 GB | Same package sha256 `868f772e…` and model root `80e919b6…`. Rust: 373 s, 1.3 GiB peak RSS. Python, digests only: 20 min, 0.73 GiB. |
| Golden generation, scalar vs SIMD kernels; 4 cases, 112 prompt tokens, 68 decode steps | same | Identical matrix digest `b5d81fff…` and boundary-matrix digest `47516325…`. |
| The Python executor re-derives every logits vector, boundary digest and token of the Rust run | same | All match; no fast-path fallbacks; 111 s. |
| 1, 2 and 4 pipeline stages as separate processes, kernels alternating between stages | same | Every boundary, logits hash and token equals the single-process run. |
| Verifier slice (layers 25–26 + LM head) converted from 2 of the 27 shards, verified against the whole-model manifest, replayed from the committed boundary 25 (180 positions) | linux x86-64 (above); ubuntu-24.04-arm: Neoverse-N2 (NEON, SVE2); windows-latest: AMD EPYC 9V74, 4 logical CPUs (AVX2) | Boundary-27 digests identical on all three, scalar and SIMD kernels; the Python executor's replay of the same slice identical. On Windows the digests are in the replay reports, but the summary script crashed decoding the run file as cp1252, so that job is marked failed. The script now reads UTF-8 (this PR); the re-run is §8.2. |
| Perplexity vs the BF16 weights: float32 streaming reference, itself checked against transformers 4.57.6 on the tiny models (max \|Δlogit\| 4.2e-5 and 2.9e-6) | ubuntu-latest, 4 vCPU | Federal Register 2026-20493 (after the model's cutoff), 1022 scored tokens: BF16 5.4642, INT8 5.4651 (+0.017%), top-1 agreement 97.1%. Alice ch. 1 (likely memorised): 1.3345 vs 1.3373 (+0.21%), 99.1%. |
| Tiny models (Moonlight-like and Kimi-like), the whole pipeline | ubuntu x86-64, ubuntu arm64, windows x86-64 | One package sha256 and one matrix digest per model on every runner and kernel (`2c4c1a03…`/`362460bf…`, `535350b0…`/`1aa4b6c6…`); 1/2/4-stage layouts and Python replays all match. |
| Tokenizer and chat prompt vs the tiktoken library and jinja2 | ubuntu-latest | 75 of 75 rows identical, including a 3,128-token text and 8 chat prompts. |
| Rust unit tests (profile, stages, TP row/K splits, EP split, thread and kernel invariance, tokenizer) | ubuntu-latest | 25 + 6 tests pass; rustfmt and clippy (deny warnings) clean. |

Speeds from the same run (single stream, CI runner):

| Measurement | Runner | Figure |
|---|---|---|
| Moonlight decode, INT8, scalar kernel | Xeon 8573C, 4 vCPU | 1.90 output tok/s; prefill 1.90 tok/s |
| Moonlight decode, INT8, SIMD (AVX2 limb) kernel | same | 2.70 output tok/s; prefill 2.65 tok/s |
| Perplexity scoring (teacher-forced), SIMD | ubuntu-latest, 4 vCPU | 2.44–2.46 tok/s |
| Slice replay, layers 25–26 + head, 180 positions, scalar / SIMD | linux x86-64, Xeon 8573C | 10.7 / 16.0 positions/s |
| same | linux arm64, Neoverse-N2 | 21.2 / 36.7 positions/s |
| same | windows x86-64, EPYC 9V74 | 6.6 / 15.9 positions/s |

What this run did not cover: macOS (no macOS job while those runners serve
the release sequence), the INT4 group-32 variant on Moonlight (§8.2), and any
GPU.

### 8.2 INT4 group-32 experts, config-driven detection, Windows re-run

*(Pending: the `[kimi-proof]` run of the commit that adds them. Filled in
from its artifacts once it completes.)*

## 9. Kimi K3: what changes

Kimi K3 is public at `moonshotai/Kimi-K3` (created 13 June 2026, last
modified 2 September 2026). Everything in this section was read from that
repository at revision `f831ab66814297da540d832a5235f8e904f29d06`, or computed
from it.

**Shape [VENDOR].**
- 2.8T total parameters (2,779,931,837,184 on the Hub), 104B activated per
  token.
- 93 layers: 69 Kimi Delta Attention (KDA) and 24 gated MLA; hidden width 7,168.
- 896 routed experts, 16 per token, plus 2 shared experts; the routed experts
  run on a 3,584-wide latent projection.
- Attention residuals over blocks of 12 layers; the SiTU-GLU activation;
  context of 1,048,576 tokens.
- Routed experts ship as MXFP4 (quantisation-aware training with MXFP8
  activations); attention, shared experts and the LM head are BF16.
  1,560,860,324,864 bytes on disk.

**What the integer engine needs.** The converter now reads the configuration's
family and refuses K3 with the full list of missing features. Profile §11.2 has
the table. In short:
- new operators: the KDA recurrence, NoPE gated MLA, SiTU tables, latent MoE
  and attention residuals;
- a lossless MXFP4 reader;
- a wider stage boundary.

The stage packages, model root, boundary files, router and verifier protocol
carry over unchanged. KDA is recurrent. A stage replay still works because the
stage rebuilds its own state from the boundary activations. The only exactly
reproducible order is the recurrent one, though, so prefill parallelism has to
come from heads and channels rather than from chunking time.

**Memory [CALC].**
- Weights at MXFP4 experts (4.25 bits) and INT8 elsewhere: about **1.50 TB**,
  or 188 GB per GPU over 8 GPUs.
- Per-sequence state: the 24 MLA layers cache 54 KiB per token (i32), which is
  58 GB at 1M tokens. The 69 KDA layers hold a fixed 108.5 million state values,
  about 0.43 GB at 32 bits, whatever the length.
- Stage boundaries carry the attention-residual block stack: up to 9 × 7,168
  values, **504 KiB per position**, against 56 KiB for K2.6.

**Islands [CALC].**
- **Macs.** With r6's 80% usable memory, three 512 GB Mac Studios give 1.23 TB
  and do not hold the weights; four give 1.64 TB. A K3 Mac island is **4× M3 Ultra
  512 GB**, about $46.8k at Geerling's per-machine price [VENDOR]. Options A and
  B in §7 cannot hold K3, and neither can option C (768 GB).
- **Speed.** K3 reads about 81 GB per token at these formats: 48.6B routed
  weights at 4.25 bits plus 55.4B other weights at INT8. That is 3.6× K2.6's
  22.6 GB. r6's model for 4× M3 Ultra (186 collectives) gives
  **12–14 tok/s**, with a collective-only cap of 28–36 tok/s.
- **GPU nodes.** K3 needs about 188 GB of weights per GPU on an 8-GPU node, plus
  KV and runtime. That is twice option C's memory. Which GPUs qualify is a
  question for quotes.

**Licence [VENDOR, LICENSE file].** It is MIT-style with two conditions:
- A licensee operating a "Model as a Service" business whose revenue,
  including affiliates, exceeds US$20 million over any 12 consecutive months
  must sign a separate agreement with Moonshot AI before commercial use.
- Products with more than 100 million monthly active users or more than US$20
  million monthly revenue must display "Kimi K3" prominently.

Neither condition applies to internal use. Whether and when this binds the
planned API is TJ's call.
