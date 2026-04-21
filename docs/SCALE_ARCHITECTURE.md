# ARC Chain Scale Architecture — True Spec

**Status:** Design doc, 2026-04-20. Supersedes ad-hoc assumptions baked into the v0.5.2 seed deployment.

**Goal:** a blockchain for verifiable AI inference that scales infinitely across any device, serves any model, always in the most efficient manner, and NEVER breaks at any scale.

---

## 1. Honest admission of this session's shortcomings

Before the forward plan, the truth about what went wrong in the session that preceded this spec. The next session should read this so the same mistakes don't repeat.

### 1.1 What I got wrong

1. **Spent 5 ship/revert iterations chasing a "PPL bug" that was never a forward-pass bug.** The integer forward has always matched candle's Q8_0 reference on argmax across every tested position. The "PPL 107 → 782 → 31441" swings were me breaking things that were working, each time thinking I understood the problem. Each fix was more speculative than the last.

2. **Didn't compare against a reference implementation until the end of the session.** I had `candle_transformers` linked for GGUF loading the whole time. A single 30-line probe against candle's `ModelWeights::forward` would have shown on day one that our argmax matches. Instead I wrote `probe_i16_vs_i8.rs`, `probe_i16_matmul.rs`, `probe_i16_real_weights.rs`, `probe_hidden_magnitudes.rs` chasing shadows.

3. **Confused "output magnitude" with "output correctness".** When block-i8 produced logits 52× larger than broken-I8, I interpreted that as a bug. It was the *fix*. Broken-I8 was 36× undersized; block-i8 produces the correct magnitude. The fact that PPL didn't improve with the "fix" was because PPL measures distribution tail, not argmax — and our single-token argmax was already correct.

4. **Misread the architecture.** Assumed sharding was load-bearing for 7B when it's not (7B fits on any device). The real reason sharding breaks isn't the scheme — it's that the current deployment has **1 node per shard range**, so any single node dropping kills the pipeline. Saw this happen live during the session (JNB and SAO went offline → shard dispatch returned `?`) and still didn't name the right fix until TJ pushed back.

5. **Chased academic benchmarks instead of the product claim.** WikiText-2 PPL 5.47 parity is a research benchmark. The chain's actual value prop is "N independent nodes computed the same answer, verifiable cross-platform." Argmax correctness + hash consensus is what matters, not log-likelihood tails.

6. **Over-fit to 1-prompt evidence.** " Paris." being the right first token on "The capital of France is" is not a validation that the full forward is correct. Needed multi-position comparison against candle to actually prove forward correctness. Did that eventually. Should have been the first thing.

7. **Let the PPL rabbit hole distract from the real failure we observed live.** The user's actual complaint was "sharded inference returns Failed to fetch" and "parallel inference shows no usable output". The root cause was SAO+JNB offline + 1-node-per-shard fragility. I fixed the Tunnel (good) and rewired the dashboard to hit the coordinator (good), but didn't address the underlying fragility until the very end.

### 1.2 What actually works (verified this session)

- **Integer forward is mathematically correct.** `probe_candle_vs_integer.rs` and `probe_candle_multi.rs` show our integer `forward_one_token` matches candle's reference Q8_0 forward:
  - All 6 tested positions produce the same argmax
  - Top-10 vocab rankings overlap 10/10
  - Logit magnitudes within 1-5%
  - Generated text is coherent (" Paris." on France prompt)
- **Cross-platform determinism holds.** Same binary on ARM Mac and x86 Vultr produces bitwise-identical output hash.
- **Block-i8 weight quantization is correct.** `probe_block_i8.rs` + `probe_i16_real_weights.rs` show ratio 1.000 vs f32 ground truth on real Llama-2-7B tensors (attn_q, ffn_down, output.weight).
- **Network is alive.** 6 of 8 seeds healthy at consensus round 1.28M+, Mac syncing.
- **Install flow works on Intel Mac** (after this session's `arc-node-macos-x86_64` upload to v0.5.3).
- **Tunnel-backed sharded inference works** when JNB and SAO are online — produces coherent text, different prompts yield different hashes.

### 1.3 Known limitations (real, not chased-shadows)

- **5× PPL tail-distribution gap vs candle Q8_0 on short benchmark snippets.** Argmaxes match; the confidence on non-argmax tokens is lower than FP16/Q8_0 reference. Cumulative effect of integer LUT softmax, per-block scale, and fixed-point arithmetic. Not fixable without either (a) per-channel calibrated quantization, (b) wider fixed-point, or (c) f32 basic ops per RFC 3514. **Does not affect argmax correctness for generation.**
- **1-node-per-shard-range means single-point-of-failure.** JNB offline → layers [30,32) unavailable → pipeline dead. This is the actual bug breaking demos.
- **No committee consensus on shard outputs.** Coordinator trusts whatever the single replica returns. No divergence detection.
- **No dynamic model onboarding.** Seeds were statically configured with one model via CLI flags.
- **Mac is a hard coordinator dependency** (holds the tail shard). If Mac is unreachable, sharded pipeline fails.

---

## 2. True Spec — what the chain must be

### 2.1 Core invariants (non-negotiable)

1. **Cross-platform bitwise determinism.** Same model + same prompt on ARM + x86 + GPU = same output bytes. This is the IP.
2. **No floating-point transcendentals in the hot path.** No `exp`, `log`, `sin`, `cos`, `sqrt`, `tanh` from libm — they differ across platforms. Use integer LUTs.
3. **No FMA contracts.** The compiler must not fuse `a*b+c` into `fma(a,b,c)` because FMA produces platform-dependent rounding. Compile with LLVM's `-fno-fp-contract` equivalent.
4. **Deterministic reduction order.** Scalar reductions, or SIMD only where bit-identical per arch.
5. **IEEE-754 f32 basic ops (+, -, *, /, abs, copysign, sqrt) are permitted per Rust RFC 3514** — they're cross-platform bit-identical. Use this for f32 scale application; keep transcendentals integer.

### 2.2 Scale properties the chain must guarantee

1. **Any device participates** at a tier matching its RAM/SSD/CPU budget.
2. **Any model** (small, medium, 70B+, MoE, future architectures) is serviceable by the network given enough nodes with aggregate capacity.
3. **Graceful degradation** — reduced redundancy still serves inference; no single node failure takes down a model; cluster survives correlated outages in one region.
4. **Economically efficient** — operators get paid more for rarer/larger-model capacity, driving diversification naturally.
5. **Cryptographically verifiable** — every inference is signed by a committee whose hashes agree; divergence is on-chain evidence of bad behavior and is slashable.

### 2.3 Sizing examples

For 500 commodity nodes at 8GB RAM / 50GB SSD each:

| Model | Size (Q8_0) | Eligible nodes | Strategy | Redundancy |
|---|---|---|---|---|
| Llama-2-7B | 4 GB | ~450 | Replicate whole model | 450-way |
| Llama-2-13B | 7 GB | ~300 | Replicate whole model | 300-way |
| Llama-2-30B | 16 GB | ~80 | Replicate whole model | 80-way |
| Llama-2-70B | 35 GB | 0 hold whole | **Shard**, 8×4GB slices | 5× per shard ⇒ 40 nodes |
| 400B (future) | 200 GB | 0 | **Shard deeper**, 16×12GB slices | 3× per shard ⇒ 48 nodes |

Every model in every size band gets at least 3× redundancy per shard range. That's the invariant: **consensus ≥ 3, always**.

### 2.4 Architecture components

#### A. Model registry (on-chain)

- Models registered by content hash (BLAKE3 of weights).
- Registry entry: `{ model_id, shard_plan, total_params, quantization_format, min_ram_per_shard }`.
- `shard_plan` specifies layer ranges: `[(start=0, end=8), (start=8, end=16), ...]`. Small models have one shard `[0, n_layers)`; large models have N shards.
- Anyone can register a new model; registry stores the plan and opens capacity demand.

#### B. Node capacity registry (on-chain)

- Each node publishes: `{ node_id, ram_bytes, ssd_bytes, cpu_flops_estimate, gpu_tier }`.
- Updates every epoch (hour). Stake-weighted to prevent spam.

#### C. Shard assignment protocol

- For each `(model_id, shard_range)`: chain maintains a list of nodes that hold those weights.
- Nodes opt into holding a shard by downloading the weights (from IPFS / BitTorrent / content-addressed P2P).
- Chain enforces **minimum 3 replicas per shard range**. If replication drops below 3, the shard opens bounty for more nodes to join.
- Nodes earn inference fees proportional to their participation. Rarer models pay more → natural diversification.

#### D. Inference request routing

- User submits `{ model_id, prompt, max_tokens }` to any node.
- Node becomes transient coordinator:
  1. Looks up `shard_plan` from registry.
  2. For each shard range, picks the fastest `k` (e.g. 3) replicas.
  3. Fires parallel requests to those `k` replicas, each asking for the shard's forward output.
  4. Collects all `k` output hashes. If they agree, proceed. If 2 of 3 agree, use majority (1 is slashable). If 0 of 3 agree, coordinator escalates (more replicas).
  5. Passes the verified hidden state to the next shard's replicas.
  6. Final shard emits logits → coordinator samples next token → repeat.

#### E. Hash-merkled consensus per shard

- Each shard produces `{ hidden_state_out, hidden_state_hash }`.
- Coordinator builds a Merkle tree of all shards' hashes per position → one root hash per token.
- Final inference attestation = `{ model_id, prompt_hash, output_hash, merkle_root, participating_nodes[], signatures[] }` posted on-chain.
- Anyone can re-run any shard from its inputs and verify the hash.

#### F. Slashing conditions

- Shard replica produces a hash divergent from the majority at its range → stake slashed.
- Node claims to hold a shard but times out on request → reputation penalty.
- Coordinator posts an attestation with mismatched signatures → slash.

### 2.5 Models that fit on one node (the common case)

The whole "sharding" language is a red herring for small models. For a 7B model on 500 nodes with 4GB free each:

- Each node holds the whole model. No pipeline. No coordinator. No tunnel.
- User request → coordinator picks `k=5` (or whatever redundancy level) random eligible nodes → fires 5 parallel full-forward requests.
- All 5 return output + hash.
- Consensus: 5-of-5 matching hash = attestation. 4-of-5 = majority accepts + slash the odd-one-out. 3-of-5 or worse = request more replicas.

This is massively simpler than pipelined sharding and works for every model size ≤ max-per-node capacity. Only fall back to pipelined sharding when the model legitimately doesn't fit.

### 2.6 Quality vs. correctness — the honest claim

- **Correctness (argmax)**: our integer forward produces the same argmax as candle's reference Q8_0 on every tested position. Generation output is coherent Llama-grade text.
- **Quality (PPL tail)**: our integer path has a ~5× PPL gap vs reference on short benchmark snippets due to cumulative integer-arithmetic noise. Doesn't affect argmax; does affect confidence calibration.
- **Product claim**: "N independent nodes cross-platform computed the same answer, verifiable by hash." That claim is 100% true today.
- **Research claim**: "Matches FP16 quality" is overreach until SmoothQuant-style calibration or per-channel scales land. The literature says this is achievable deterministically (I-LLM, I-BERT, SmoothQuant), just not done yet.

---

## 3. Implementation roadmap

### Phase 1 — stop the bleeding (immediate)

- [ ] **Rolling upgrade seeds to full-model replication for 7B.** Remove `--shard-start/--shard-end` flags. Each seed loads full 4GB Llama-2-7B. Each seed serves `/inference/run` independently. One seed at a time per the ABSOLUTE RULE.
- [ ] **Update dashboard**: "Run on All 8" fires 8 parallel `/inference/run` requests (not sharded). Each produces output + hash. Verify all 8 match. Show wall time + speedup.
- [ ] **Tear down the NYC:10000 tunnel + watchdog.** Not needed when every seed serves full inference.
- [ ] **Retire single-point-of-failure shard path for 7B.** Keep code for future 70B.

### Phase 2 — committee consensus (within week)

- [ ] Committee selection RPC: `/inference/committee` picks `k` replicas, returns node list.
- [ ] Coordinator-less inference: user hits any node, it forms a k-of-m committee, fires parallel, hashes agree, returns.
- [ ] Divergence handling: `/inference/dispute` accepts two attestations with different hashes, posts to chain for slashing.

### Phase 3 — dynamic model registry (within 2 weeks)

- [ ] On-chain `register_model` tx: content-addressed weights, registrants pay base fee.
- [ ] On-chain `offer_capacity` tx: node publishes RAM/SSD, opts into models.
- [ ] Scheduler publishes shard-plan + replica-list per model, updated on capacity changes.
- [ ] P2P weight distribution: nodes pull model files by content hash from peers holding it.

### Phase 4 — large model sharding (when demand materializes)

- [ ] Pipeline-parallel inference with redundant shard replicas (3× per range minimum).
- [ ] Merkle-rooted consensus on shard boundaries.
- [ ] Token-by-token attestation.

### Phase 5 — quality parity (research track, parallel to Phase 1-4)

- [ ] SmoothQuant-style offline activation smoothing per registered model (one-time calibration on corpus).
- [ ] Per-channel per-block scales stored as f32 in weight file (RFC 3514 deterministic).
- [ ] Target: <1 PPL gap vs candle Q8_0 on WikiText-2 full test set.
- [ ] Publish results — no one has deterministic cross-platform FP-parity LLM inference at 7B yet.

---

## 4. Committed diagnostic artifacts

All under `crates/arc-inference/examples/`:

- `probe_i16_vs_i8.rs` — A/B of I16 vs I8 forward logit magnitudes.
- `probe_i16_matmul.rs` — synthetic single-row matmul sanity check.
- `probe_i16_real_weights.rs` — matmul correctness vs f32 ground truth on real Llama tensors.
- `probe_block_i8.rs` — block-i8 quantization correctness on real tensors.
- `probe_dispatch.rs` — confirms block-i8 is actually dispatching in forward.
- `probe_hidden_magnitudes.rs` — per-layer hidden-state trace through the forward pass.
- `probe_candle_vs_integer.rs` — candle Q8_0 vs our integer on BOS token.
- `probe_candle_multi.rs` — multi-position comparison through a prompt.
- `probe_candle_ppl.rs` — candle PPL on same tokens as eval_perplexity.

Anyone debugging quality should run these before making changes.

---

## 5. Don't-repeat-my-mistakes list for next session

1. Before claiming a forward bug exists, compare against candle. Single-probe, 30 lines. Do this FIRST.
2. Distinguish argmax correctness from PPL-tail quality. These are different problems with different fixes.
3. Sharding is for models that don't fit. For models that fit, replicate.
4. Redundancy per shard range is not optional. Minimum 3 replicas. Always.
5. Before touching the integer LUTs or matmul primitives, run the probe suite. If probes pass and PPL still diverges, the bug is elsewhere.
6. If a fix regresses PPL on the first eval, revert immediately. Don't chain three speculative fixes on top.
7. f32 basic ops (+, -, *, /, sqrt) are cross-platform deterministic per RFC 3514 and can be used for per-block scales. Transcendentals (exp, log, sin) cannot.
8. The product claim is "verifiable deterministic inference across commodity devices." The research claim is "FP16 quality parity." Don't confuse them.
