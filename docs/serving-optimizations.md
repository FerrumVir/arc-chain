# Serving optimizations, bit-exact

How ARC's integer engine uses three tricks that make API providers fast without changing a single output byte:

- continuous batching;
- prefix (KV-cache) reuse;
- speculative decoding.

Code: `crates/arc-inference/src/modern/serving/`. Measurements: `arc-serve-bench`, run by `.github/workflows/serving-speed.yml`.

**Scope.** This is the additional SmolLM3-3B profile `arc.hf-llama.i8-dyadic-row.q16.v1`. Consensus, rewards, native inference and the network's canonical model are not touched. Nothing here adds an identity or changes an operator, kernel file, golden vector or table: the golden digests are unchanged by construction, and CI checks them on every run.

**The rule.** A serving trick may change *when* something is computed and *how often* the weights and cached keys are read. It may never change *what* is computed. A speedup that changes a digest is a bug.

**Numbers.** Every number below was measured on a GitHub-hosted CI runner by the job that produced it, and says which run, which commit and which CPU. They are CI-runner numbers, not device benchmarks, and they move between runs: the two x86-64 jobs of one run landed on different AMD EPYC models. "tok/s" always says which tokens it counts. Nothing here is projected unless it says so. The measured sections are from run 37555419649 (commit `25ccd495f`), before the review fixes of 7 October 2026 (section 1), which add the LM head's domain checks to every prompt row, about 8.5% more weight reads per prompt row, and change nothing in decode.

---

## Why an integer engine gets this for free

Float serving stacks are not batch-invariant:

- A float sum depends on its reduction order, and kernels pick tile shapes and split-K by batch size.
- So one request's output depends on how many other requests share its batch.
- Making float kernels batch-invariant has a measured cost. Thinking Machines ("Defeating Nondeterminism in LLM Inference", 10 Sep 2025) reports Qwen3-8B over 1,000 sequences going from 26 s on default vLLM to 55 s with unoptimized batch-invariant kernels and 42 s with an improved attention kernel. SGLang's deterministic mode ("Towards Deterministic Inference in SGLang", 22 Sep 2025) reports +24% to +55% on Qwen3-8B on an H200 depending on the attention backend, 34% on average.

The dyadic profile is integer-only and order-free:

- Every dot product is an exact `i64` sum, so vector width, tiling, thread split and batch size cannot change it.
- The rounding is one explicit shift per output, `(acc · μ) >> k`.
- RMSNorm and RoPE are per row.
- Attention is a two-pass softmax: the exact maximum first, then exact integer sums. The order in which positions are visited cannot matter either.
- Every operator is therefore a pure function of one row plus that row's own KV cache.

The serving layer only regroups rows, so batch invariance costs nothing.

## The contract every model obeys

`BatchModel::forward_rows(rows, kvs)` runs one step over rows that may mix many requests, prefill chunks and decode tokens.

**Input contract.**
- The rows of one sequence are contiguous and in position order.
- The first of them sits at that sequence's committed length.

**What a step must compute.** Each row's logits and each appended KV position must equal what the model computes for that token alone, given only that sequence's cache.

**What a step must not do.** No value may depend on:
- the other rows of the step;
- how rows are grouped;
- the size of the step.

**Failure.** A row that fails fails alone, with the error the model raises for that token alone. Its sequence keeps the rows before it (logits returned, positions committed), drops it and the rows after it, and reports how many rows it kept. Other sequences are unaffected.

The scheduler, the prefix cache and speculation see a model only through this trait and the plane-structured `SeqKv`. Any model that keeps the contract plugs in unchanged (see "MoE and islands" below).

---

## 1. Batch and phase invariance (the gate)

**What the batched step does** (`serving/dense.rs`). The batched step `layered_forward` loops in a different order from `ModernModel::forward`:

- each projection runs once over all rows of the step (`gemm::project_rows`);
- everything else is per row:
  - RMSNorm;
  - RoPE at the row's own position;
  - the `i32` KV append with the same domain check;
  - two-pass attention over the row's own sequence, up to its own position.

**Cross-request coupling found, and how it is handled.**

| Coupling | Where | Handling |
|---|---|---|
| A domain refusal fails the whole call | `arith::project` refuses a whole projection; `ModernModel::forward` fails the whole token | Checks are per row (`check_projection_input` and the epilogue per activation row). A row that leaves the domain fails alone, with the same error text it would get alone. The other rows' values are unaffected. |
| A batch-wide kernel fallback | `canonical_simd::matmul_i8_batched_fast` refuses the whole batch if one token is out of range | The serving GEMM falls back per activation row. Values cannot differ anyway (both paths are exact), but one hostile request cannot slow every neighbour onto the scalar path. |
| Shared scratch state | `canonical_simd`'s thread-local limb scratch stays borrowed across a rayon parallel loop | Calling `arith::project` from inside a parallel loop over requests could re-enter that scratch and panic, failing every request in the batch. The serving layer never nests projections: one projection per matrix per step, over all rows. Its own GEMM keeps no thread-local state. |
| One failure aborts the step | Errors propagate with `?` | Errors are tracked per row. A failing row appends zeros meanwhile so its sequence's planes stay aligned; when the step closes (`finish_step`), the sequence commits the rows before the failure and drops the failing row and the rows after it. Every other sequence commits normally. The scheduler then asks whether plain decoding would have run the failing row at all (section 4). |
| Mixture-of-experts capacity limits | Common in MoE serving: an expert takes at most N tokens per batch and drops the rest | Forbidden by the contract. Routing is a function of the row alone: top-k by integer score, lowest index on ties. |

**The proofs (CI, every run).** All of them are in `serving/tests.rs`. Every comparison is byte for byte against `ModernModel::forward` or `ModernModel::generate`, the single-token path the golden digests pin. Each one compares tokens, every logits hash and the KV digest. They run on Linux x86-64, Linux arm64 and Windows x86-64, with the default kernels and again with the SIMD kernels forced on.

- `every_request_is_byte_identical_at_batch_1_8_and_32`: 32 random requests at concurrency 1, 8 and 32. The step budgets go down to 7 rows (prefill chunked to 3), in forward and reversed arrival order.
- `a_request_does_not_depend_on_its_neighbours`: one fixed request, alone and among 7 or 31 random other requests.
- `a_prompt_gives_the_same_bytes_in_one_prefill_in_chunks_and_token_by_token`: the prompt as one 41-row prefill, as chunks of 1, 7, 16, 3 and 14, as 40 + 1, and as 41 single-token steps.
- `batched_projection_equals_one_row_at_a_time`: the batched GEMM against `arith::project`, row by row:
  - inner dimensions 1, 15, 16, 17, 33, 64, 2,049 and 4,111, crossing every vector tail and the x86 flush boundary;
  - activations at the digit-domain boundaries and past them;
  - one row outside the projection domain.
- `recomputing_a_preempted_request_is_byte_identical`: a request that loses its cache mid-generation (preemption, a device leaving an island) is rebuilt by prefilling its prompt plus the tokens already generated, in one chunk, in chunks of 5 and token by token. The rebuilt cache, the next logits and the next token are the uninterrupted ones.
- `a_failing_sequence_does_not_disturb_the_batch` and `a_failing_request_leaves_every_other_request_untouched`: failures stay with their request, and the rows of a sequence before its failing row are kept and equal single-token decoding.
- `refusals_match_generate_and_stay_with_their_request`: a refused request gets the same error text as `generate`.
- `a_prompt_token_outside_the_domain_is_refused_like_generate`: a prompt token whose forward pass leaves the domain (a value projection outside the KV `i32` range) is refused with `generate`'s error text, in whichever prefill chunk it falls, with the prefix cache off and on; the other requests of the batch are untouched.
- `prompt_rows_without_logits_run_the_same_head_checks_as_generate`: the reference refuses at a prompt position whose logits choose no token (a huge final-norm gain); default serving refuses with the same text, as golden mode does.
- `serving_reproduces_generate_on_the_tiny_model`: the module's pinned tiny model.

These cover the four invariances named by Vosti et al. (arXiv 2609.38981): batch composition, chunked prefill, prefill versus decode, and prefix reuse (section 3), plus speculation (section 4), a pipeline split and preemption, which that work does not test.

**On the real model (CI, every bench run).** Every stream's output hash at concurrency 2 to 32 is compared with the same prompt's hash at the first concurrency it ran. The golden digest through the serving engine is in section 5.

**Refusals are the reference's refusals.** `generate` runs the final norm and the LM head at every prompt position, and may refuse there even though those logits choose no token. The batched step does the same for every row: rows whose logits choose a token return them; the others run the same norm and head and keep only the domain result, 64 rows at a time so the discarded values stay small. A request the reference refuses is refused here with the same error, in the default mode as in golden mode (`all_logits`, which also records every prompt position's logits hash). The price is the LM head for every prompt row: 0.26 G of SmolLM3-3B's 3.08 G weights per row, about 8.5%. The batching, prefix and speculation numbers below were measured before this change.

## 2. Continuous batching

**Scheduler** (`serving/scheduler.rs`). Each iteration is one batched step over every running request. Requests join and leave between steps. A step holds, in this order:

1. one decode row per decoding request, plus its drafted tokens when speculation is on;
2. prefill chunks for requests still reading their prompts, up to a row budget.

Decode first protects inter-token latency. Chunking bounds how long a long prompt can stall streams already running. None of this can change a token; scheduling decides only when a row is computed.

**Kernel** (`serving/gemm.rs`). The point of batching is that one read of each weight row serves every row of the step. For the exact sums:

| Path | Digits | Instruction | Exactness bound |
|---|---|---|---|
| x86-64 AVX2 | balanced base 2^16 (2 digits cover ±2^31) | sign-extend weights to i16, `vpmaddwd` | each lane gains ≤ 2·128·2^15 = 2^23 per 16 columns; flushed to i64 every 2,048 columns (≤ 2^30) |
| arm64 NEON | balanced base 2^8 (4 digits) | `sdot` (inline asm, as in `canonical_simd`) | a plane sum is ≤ cols·2^14 < 2^31 for cols ≤ 131,071 |
| any | none | `dot_i8_i64` | exact i64 within the profile's projection domain |

- **Digits are an identity.** `x = Σ dᵢ·baseⁱ` is verified by construction, so the digit sums recombine to the same `i64`.
- **Registers.** A kernel keeps one named accumulator per digit plane and loads each weight vector once for all of them. There are kernels for eight, four and two planes, so a step with few rows (a two-stream decode, a short speculative verification) does not multiply padding.
- **Tiling.** A rayon task owns 8 weight rows. Columns are walked in chunks (1,024 on x86-64, 2,048 on arm64) so one group of plane chunks and the block's weight chunks stay in L1 while every pair is multiplied; the partial sums of chunks add up exactly.
- **Batch 1.** With a single live row the step calls `arith::project`, which is today's GEMV path. Single-token kernel work on the same base branch speeds that path up without touching this module.

**Measured, kernel alone** (`arc-serve-bench gemm`, synthetic SmolLM3-shaped matrices, no model; the `kernel` CI job; commit `25ccd495f`, run 37555419649, 4 threads). Per-row time of the batched projection against one row at a time, outputs compared and identical. x86-64 ran on an AMD EPYC 7763 (a different runner than the bench job below), arm64 on a Neoverse-N2.

| rows in the step | x86-64, 2048×2048 | x86-64, 2048×11008 | arm64, 2048×2048 | arm64, 2048×11008 |
|---|---|---|---|---|
| 1 | 1.07× | 1.08× | 1.07× | 1.05× |
| 2 | 2.28× | 2.19× | 1.97× | 1.91× |
| 4 | 2.22× | 2.53× | 2.60× | 2.78× |
| 8 | 2.35× | 2.51× | 2.62× | 2.89× |
| 16 | 2.66× | 2.54× | 2.93× | 3.07× |
| 32 | 2.47× | 2.59× | 2.98× | 3.09× |
| 64 | 2.64× | 2.48× | 2.91× | 3.04× |
| 128 | 2.44× | 2.25× | 2.84× | 2.93× |
| 256 | 2.63× | 2.16× | 2.61× | 2.73× |

In weights multiplied per second across all rows: x86-64 10–12 G at one row to 22–31 G batched; arm64 19–22 G to 55–65 G. One row is the GEMV path either way, so its ratio is noise around 1. In the run before the four- and two-plane kernels (37523722150) the two-row case was 1.42–1.60× on x86-64 and 2.00–2.07× on arm64; the x86-64 kernel job ran on a different CPU that time (EPYC 9V45), so only the arm64 pair is like for like, and there the two-row case did not move. On x86-64 two rows are exactly four base-2^16 planes, which the four-plane kernel fits; on arm64 two rows carry four to six base-2^8 planes, which it rarely fits exactly.

**Measured, SmolLM3-3B, decode only** (`arc-serve-bench batching`, the `bench` CI job; commit `25ccd495f`, run 37555419649; x86-64 = AMD EPYC 9V74, 2 cores / 4 threads; arm64 = Neoverse-N2, 4 cores; SIMD kernels). Every stream generates 16 tokens; the chat template's shared prefix is served from the prefix cache, so each stream prefills only its own question. **tok/s counts output (generated) tokens** over the forward-pass time of the steps that held only decode rows; prefill steps are excluded. Per-stream tok/s is one token per step, i.e. the inverse of the step time. The streams' tokens were identical at every concurrency.

| concurrent streams | x86-64 aggregate tok/s | x86-64 per stream | x86-64 step (s) | arm64 aggregate tok/s | arm64 per stream | arm64 step (s) |
|---|---|---|---|---|---|---|
| 1 | 3.88 | 3.88 | 0.258 | 6.40 | 6.40 | 0.156 |
| 2 | 7.44 | 3.72 | 0.269 | 11.87 | 5.94 | 0.168 |
| 4 | 7.53 | 1.88 | 0.531 | 14.18 | 3.55 | 0.282 |
| 8 | 8.10 | 1.01 | 0.988 | 17.28 | 2.16 | 0.463 |
| 16 | 8.29 | 0.52 | 1.931 | 18.97 | 1.19 | 0.844 |
| 32 | 9.54 | 0.32 | 3.165 | 19.61 | 0.65 | 1.540 |

Reading it: 32 streams give 2.5× (x86-64) and 3.1× (arm64) the aggregate output of one stream on the same runner. Two streams cost 4% (x86-64) and 8% (arm64) more per step than one, so they produce 1.92× and 1.85× the output; the x86-64 runner is then near saturation from 4 streams, while the arm64 runner keeps gaining slowly to 32. A decode step's cost per row falls from 0.258 s (one row) to about 0.105 s (32 rows) on x86-64 and from 0.156 s to about 0.051 s on arm64, which is the kernel speedup above. Two to four CPU cores at full compute are the ceiling here, not memory bandwidth; GPUs move that ceiling (section 7).

## 3. Prefix and KV-cache reuse

Real traffic is prompt-heavy. DeepSeek's inference-system overview for V3/R1 (27–28 Feb 2025) reports 608 B input tokens against 168 B output tokens, with 56.3% of the input served from its on-disk KV cache. The agent replay below has 28 input tokens per output token.

**Design** (`serving/prefix.rs`).
- **Blocks and keys.** The cache stores blocks of 16 positions, keyed by a hash chain:
  - `key₀ = H(tag, model identity, tokens of block 0)`;
  - `keyᵢ = H(tag, identity, keyᵢ₋₁, tokens of block i)`.
  A key therefore commits to the model and to every token before the block's end, like a radix-tree path.
- **Hits are verified, not trusted.** A hit also compares the stored tokens and the parent key, so a hash collision can only cause a miss.
- **The last prompt token is always computed.** Its logits choose the first generated token.
- **What gets stored.** Prompt blocks are stored when a request's prefill finishes, and prompt plus output blocks when it retires. The next turn of an agent loop then reuses the previous turn as well.
- **Eviction.** Least recently used, leaves before parents, ties broken by key, so cache contents are reproducible.

**Why it stays bit-exact.** The KV at positions `0..n` is a pure function of the model and tokens `0..n`. Phase invariance makes it the same whether those positions were computed in one prefill, in chunks, token by token or during decoding. So a block is valid for any request whose tokens match.

**Proofs.**
- `the_prefix_cache_never_changes_a_byte`: six requests sharing a 37-token system prompt, block sizes 1, 4 and 16, an unbounded cache and an 8 KB cache that forces evictions, sequential and simultaneous arrivals. The tokens, logits and KV digests are identical to the cache-off run, and the hits are counted.
- `prefix_keys_commit_to_the_model_and_the_whole_prefix`: two model identities and two parents give different keys for the same tokens; a lookup never serves the last prompt position; a miss copies nothing.
- `generated_prefixes_are_reused_and_every_cached_block_equals_recomputation`: an agent's second turn re-sends the first turn's prompt and output. With an unbounded cache exactly the whole-block part of the first turn (prompt plus every generated token but the last) is served; with a 6 KB cache that evicts, fewer. In both, every block the cache can serve is compared with recomputing its token prefix from scratch: the KV digests are equal. That is the KV-cache invariant of Vosti et al.

**Measured, SmolLM3-3B** (`arc-serve-bench prefix`, same job, commit and runners as the batching table). A prompt-heavy agent replay: one shared system prompt with tool definitions (fictional company and data, in `scripts/arc_modern/serving_workload.json`), eight user turns of 616–627 prompt tokens each, answers of at most 24 tokens; 4,969 input tokens to 175 output tokens (28.4:1). Requests run one at a time, prefill chunk 256. Time to first token (TTFT) runs from submission to the first generated token, so it includes the whole prefill.

| arm | prompt tokens from cache | x86-64 TTFT, request 1 (s) | x86-64 mean TTFT, requests 2–8 (s) | arm64 TTFT, request 1 (s) | arm64 mean TTFT, requests 2–8 (s) |
|---|---|---|---|---|---|
| no cache | 0 of 4,969 | 64.62 | 65.74 | 32.81 | 32.98 |
| prefix cache | 4,144 of 4,969 (592 per later request) | 66.19 | 3.49 (2.8–4.1) | 33.15 | 1.68 (1.4–2.0) |

Outputs with and without the cache were identical on both runners. Mean TTFT of requests 2–8 fell 18.8× (x86-64) and 19.6× (arm64). The first request pays the full prefill: a 622-token prompt costs 65 s on the x86-64 runner and 33 s on arm64, about 10 and 19 processed prompt tokens per second, the same per-row cost as a wide decode step.

## 4. Speculative decoding, provably exact

**The drafter.** Prompt lookup (`serving/spec.rs`) continues the most recent earlier occurrence of the context's last 2–4 tokens. It needs no second model, so it adds no memory, bandwidth or trust base. It suits agent traffic that quotes its prompt: code edits, JSON, tool output, RAG.

**Verification.** The drafts `d₁..d_k` are verified in one batched step at positions `p..p+k`:

```
rows:    last  d₁   d₂  …  d_k
logits:  L₀    L₁   L₂  …  L_k
```

**The accept rule** (`spec::accept`). At row `j`, select the next token from `Lⱼ` with the request's own rule and history (including the repetition penalty), exactly as plain decoding would, and emit it. Continue to row `j+1` only if that token equals `d_{j+1}`. Stop at the first mismatch, at EOS or at `max_tokens`. Then drop the KV of the rows after the last one used.

**Proof sketch, by induction over the rows.**
1. Row 0 is plain decoding's next forward pass.
2. If rows `0..j` were plain decoding's passes and the token emitted at `j` equals `d_{j+1}`, then row `j+1` feeds the same token at the same position on the same prefix. By batch and phase invariance it is bit for bit plain decoding's next pass.
3. Each emitted token is selected from the same logits with the same history, so it is the same token.
4. The stop conditions are checked after every token, exactly as in `generate`.

So the speculative output equals the greedy output for every drafter, including a hostile one. The logits hashes agree too, and so does the final KV digest.

**Draft length adapts per request.** A request starts by drafting 2 tokens. After a step whose drafts were all accepted it doubles its draft length, up to the configured bound; after a rejection it drops to the accepted length plus one. This decides how many rows are verified, never which tokens come out.

**A draft that fails.** A drafted token can be in the vocabulary and still leave the profile's domain when it is fed (its value projection outside the KV `i32` range, say). Such a row fails alone: the step keeps the rows before it and drops it and the rows after it. The accept rule then runs over the rows kept. If it stops before the failing row, at a mismatch, EOS or `max_tokens`, the failure is irrelevant: plain decoding would never have computed that row. If it would continue into the failing row, that row is the pass plain decoding runs next, so the request fails with that row's error, exactly as plain decoding does. Tests: `a_rejected_draft_outside_the_domain_never_fails_the_request` (the counterexample of the ARC-54 review: a drafter that always proposes the out-of-domain token, at draft bounds 0, 1, 2 and 4, in default and golden mode, with the prefix cache off and on), `a_failing_draft_after_accepted_drafts_is_ignored`, and `a_draft_plain_decoding_would_feed_fails_exactly_like_plain_decoding` (the same error text as `generate`; an EOS or output-length decision taken before the failing row ends the request normally).

**Proof in CI.**
- `speculative_decoding_emits_exactly_the_plain_greedy_tokens` uses prompt lookup with draft bounds 1, 3 and 8 over 24 random requests.
- `verification_is_exact_under_honest_corrupted_and_hostile_drafts` covers:
  - an oracle drafter with perfect drafts, which accepts everything and takes the bonus token;
  - every third draft corrupted;
  - every draft wrong;
  - out-of-vocabulary ids.
- `the_accept_rule_stops_where_plain_decoding_would` covers the edge cases; `prompt_lookup_continues_the_latest_match` pins the drafter.
- On the real model, every bench run compares the speculative tokens with plain greedy decoding, and runs the golden cases with speculation and every logits vector hashed (section 5).

**Measured, SmolLM3-3B, concurrency 1** (`arc-serve-bench speculative`, same job, commit and runners as above; prompt lookup on 2–4-token suffixes, draft bound 8, up to 96 output tokens, EOS honoured; the plain run of each prompt warms the prefix cache so both arms start decoding at once). **tok/s counts output tokens from the first to the last generated token.** Outputs were identical to plain greedy decoding on both runners.

| prompts | drafted / accepted | tokens per pass | x86-64 plain → speculative tok/s | arm64 plain → speculative tok/s |
|---|---|---|---|---|
| quotes-the-prompt (4 prompts, 336 timed decode tokens, each prompt's first output token excluded: a checklist to repeat, a rename, a JSON edit, a spelling fix) | 163 / 91 (0.56) | 1.37 | 3.74 → 4.11 (1.10×) | 6.14 → 7.09 (1.15×) |
| general chat (the 5 golden prompts, 252 timed decode tokens) | 25 / 8 (0.32) | 1.03 | 3.91 → 3.92 (1.00×) | 6.28 → 6.34 (1.01×) |

Why the gain is small on a CPU. On the quoting prompts the lookup proposed 163 drafts over 245 verification passes (many passes found no match and cost one row) and 91 were accepted, so a pass yielded 1.37 tokens on average. If drafted rows were free the speedup would be 1.37×; it was 1.10–1.15×, so each drafted row cost about 0.3–0.4 of a single-row step on these runners: the batched projection is cheaper per row than the GEMV, but attention, the LM head and the rejected rows are paid in full. On general chat the adaptive window shrinks to one draft after the first rejections and the lookup mostly finds nothing, so speculation costs nothing. On hardware where extra rows in a step are nearly free, the same acceptance rate is worth more; that is a statement about the arithmetic, not a measurement.

**Where speculation does not help.** It is a latency-tier tool. When a node is already saturated with batched decoding, verification rows compete with other streams' rows.

## 5. The golden digest through the serving engine

On every bench run, each runner downloads SmolLM3-3B at the pinned revision, converts it, and verifies it against the pinned package manifest. The runner then computes the golden matrix digest twice:

1. **Single-token path.** `arc-modern golden` (one token at a time) must equal the pinned `3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2`.
2. **Serving engine.** `arc-serve-bench golden` runs all five cases concurrently with prefill chunks of 32, speculation with up to 6 drafted tokens, and every logits vector hashed. Its digest must equal the same value.

The cases then run again through a warm prefix cache, and the tokens must not change.

**Result** (commit `25ccd495f`, run 37555419649): both digests equal the pinned value on Linux x86-64 (AMD EPYC 9V74) and Linux arm64 (Neoverse-N2), and the warm-cache tokens are identical. The same held in the two earlier runs (commit `8d6ba36c4`, run 37523722150, on an EPYC 7763; commit `ef724dcf2`, run 37506215624). The job fails if either digest differs.

## 6. MoE and islands

The serving layer is written against `BatchModel`, so the north-star targets plug in without changes to the scheduler, the prefix cache or speculation. Those targets are Kimi-class MoE models served on islands (fast-linked multi-device groups).

**Mixture of experts.**
- `FeedForward` is pluggable.
- The test fixture `MoeFfn` routes each row to its top-2 of 6 experts by integer router score, with the lowest index winning ties and Q16 softmax weights over the chosen experts.
- It runs one batched projection per expert over just the rows routed to it, which is how real MoE kernels share expert weight reads.

The grouping depends on the batch; no value does. `mixture_of_experts_routing_is_batch_invariant` runs requests one token per step alone, then batched at concurrency 8 and 24 with chunked prefill and speculation, then through the prefix cache. All are byte-identical.

The rules for any MoE model:
- routing per row;
- deterministic tie-breaks;
- no expert capacity and no token dropping.

**Islands and pipelines.** The forward pass is split into stages:
- `embed_rows`;
- `forward_layers(range)`;
- `final_logits`.

The test fixture `Pipeline` cuts a 4-layer model into two stages, each touching only the KV planes of its own layers, and pushes the rows through in two micro-batches. Only hidden states cross the cut: `d_model` values of `i64` per row, which is 16 KB per token for SmolLM3 and 56 KB for a 7,168-wide model. `a_two_stage_pipeline_matches_the_whole_model` shows it is byte-identical to the whole model, with batching and speculation on. The stages run in one process here; transport between devices is not part of this change.

How the three techniques carry over:
- **Batching** works per stage: every stage's weight read serves the whole micro-batch.
- **Prefix blocks** use the same hash chain on every stage. A router can send a request to the island holding its longest prefix, moving the request to the KV, never the KV over the WAN.
- **Speculation helps most on a pipeline.** One verification pass pays the stage-to-stage latency once for up to `k+1` tokens.

**MLA.** `SeqKv` is a list of `i32` planes of any width. An MLA layer stores one latent plane instead of keys and values. Prefix blocks, rollback and digests work unchanged.

## 7. Mapping to GPU kernels

The same arithmetic maps to GPU kernels; only the kernels change. Nothing in this section is measured.

| Serving piece | CPU today | GPU kernel | Why it stays exact |
|---|---|---|---|
| Batched projection (decode batch, prefill chunk, verification rows) | digit planes × INT8 weights, `vpmaddwd` / `sdot` | CUDA `dp4a` (i8 digits), `__dp2a_lo/hi` (i16 digits × i8 weights), or tensor-core `mma.sync …s8.s8.s32` for prefill. Metal: int8 products summed in FP32 blocks of ≤ 1,024, exact while every block stays below 2^24. WGSL: `dot4I8Packed` | the same digit identity and the same i32 partial-sum bounds; the epilogue `(acc·μ)>>k` in emulated i64/i128 |
| Attention per (row, head) | two-pass softmax, exact integer sums | FlashDecoding with split-K over positions; paged KV | the exact maximum and exact integer sums make split-K legal: any split gives the same integers |
| Prefix cache | KV blocks copied into the request's cache | PagedAttention block tables pointing at shared, ref-counted blocks; the block id is the content hash | no copy is needed; the values are the same blocks |
| Speculation | (k+1)-row step + accept rule on the host | the same (k+1)-row GEMM plus causal attention; accept rule on the host or in a small kernel | identical rows give identical logits |

**What changes on a GPU.** SmolLM3-3B's integer package is 3,084,214,016 bytes, so a decode step at batch 1 reads about 3.1 GB of weights per token; on a device whose memory bandwidth is far above a CPU runner's, that single-stream floor moves first. On the CI runners above the batched kernel is compute-bound on two to four cores, which is why aggregate output flattens at 8–32 streams; a GPU's integer units move that ceiling by orders of magnitude, so the curve in section 2 should keep rising much further before it flattens. Prefill, which costs these runners 10–19 processed tokens per second, is the part that most needs tensor cores. All of this is arithmetic about the hardware, not a measurement.

Every GPU kernel must reproduce the CPU's integers. The golden digest through the serving engine (section 5) is the test, unchanged.

## 8. What this does not do yet

- **Time to first token on long prompts.** On these CPU runners a 622-token cold prompt takes 33–65 s. The prefix cache removes the shared part; it cannot make the first read of a long prompt fast. That needs GPU prefill.
- **A real MoE or MLA model.** Only the test fixtures exercise those paths.
- **Devices and transport.** The pipeline split is in-process; no network protocol between stages.
- **Better drafters.** Prompt lookup is the only drafter shipped. A draft model or n-gram tables plug into `Drafter` without touching the accept rule.
- **Scheduling policy.** Admission is first come, first served, with a fixed concurrency cap and row budget. No priorities, no fairness, no preemption of running requests (though recomputation after preemption is proven exact).
- **An API.** This is a library and a benchmark, not a server.
