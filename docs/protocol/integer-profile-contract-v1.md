# ARC integer inference profile — executable contract v1

Status: **normative for the two canonical per-row INT8 profiles**, derived from
`crates/arc-inference/src/cached_integer_model.rs` and `integer_lut.rs` at
source `5978ff63`. An implementation conforms when it produces, for every
admissible input, bit-identical logits and tokens to the rules below.

This document is checked, not just written: an independent Python executor
(`scripts/arc_conformance/`) was implemented from it and reproduces every
committed Rust known answer (§10). Where the Rust engine and this document
disagree, §12 says so and which one is being fixed.

## 1. Identities and commitments

| Identity | Meaning | BLAKE3 of the identity bytes |
|---|---|---|
| `arc.gguf-llama.i8-per-row.rope-interleaved.v1` | GGUF Llama, per-row INT8, interleaved RoPE (native paid inference) | `8e1c69d4b325699481e0308bde3394efe1d81ea25057471c35647d14ebff689e` |
| `INT8 integer (per-row, cross-platform deterministic)` | historical split-half RoPE profile (community rewards, ARC-INT8 caches) | `27ff96cf88eaaf72d1f7c498bc74a506a2dba199bd679b36b4e8303674f56a25` |
| `ARC-native-inference/gguf-llama-i8-interleaved-rope/generation-v2/bos-once/le-u32/v1` | generation semantics bound by native requests (§5.1) | `89843f82d20f8541de5fdd250a4c3bf1f70d808faf35f5987b38f37dd5e123e9` |

The first and third commitments are the exact values
`canonical_i8_profile_commitment()` and `canonical_i8_generation_commitment()`
return (`crates/arc-node/src/native_inference.rs`); a native request whose
`profile_hash` or `generation_hash` differs is refused before execution. The
model itself is bound separately by the BLAKE3 of the artifact's bytes.

The two profiles differ only in how RoPE pairs coordinates (§3.5). Every other
clause applies to both.

## 2. Number format and primitive operations

* **Activations, norms, embeddings, RoPE tables:** signed 64-bit integers in
  Q16 fixed point: the integer `v` denotes `v / 2^16`. `ONE = 65536`.
* **Projection weights:** INT8 in `[-127, 127]` (−128 never occurs), symmetric,
  **zero-point 0**, one Q16 scale per output row, `scale ≥ 1`.
* `a >> k` is an **arithmetic shift**: floor division by `2^k`, so
  `-1 >> 16 = -1`.
* `a / b` on integers **truncates toward zero**: `-7 / 2 = -3`. Floor division
  is a different profile. Both conventions are exercised (§10).
* Products and sums are exact within the domain of §9. No operation in the
  execution path uses floating point; floating point appears only while the
  model is prepared (§8).

## 3. Operators

### 3.1 Exponential, `exp_q16(x)` for `x ≤ 0`

The table has 4,097 entries and is defined **by its recurrence**, not by
`round(exp(·))`:

```
T[4096] = 65536
T[i]    = (T[i+1] * 65281) >> 16        for i = 4095 … 0
```

`65281 = round(e^(−1/256) · 2^16)` is the only constant. The recurrence
accumulates truncation, so `T` differs from `round(e^((i−4096)/256)·2^16)` by up to
142 units. The engine's doc comment says otherwise and is wrong (§12).
BLAKE3 of the table as 4,097 little-endian i64 values:
`9f75603e61e4dd2adc0f5ac8a638e1f11d2dcbbdd679c42bbc42d6956423d03d`.

```
exp_q16(x) = 65536                        if x ≥ 0
           = 0                            if x ≤ −16·65536
           otherwise, with o = x + 16·65536, idx = o / 256, f = o % 256:
             T[idx] + ((T[idx+1] − T[idx]) · f) / 256
```

### 3.2 Inverse square root, `isqrt_q16(x)`

Five Newton steps from a bit-length estimate. The result is the iterate, **not**
`round(2^16/√(x/2^16))`. For example `isqrt_q16(128·65536) = 5795`, while
correct rounding gives 5793.

```
if x ≤ 0: return 100·65536
b = floor(log2 x)
y = (65536·256) / 2^((b+1)/2);  if y ≤ 0: y = 1
repeat 5 times:
    y2 = (y·y) >> 16
    t  = 3·65536 − ((x·y2) >> 16)
    y  = (y·t) / (2·65536);        if y ≤ 0: y = 1
return y
```

### 3.3 RMS normalisation

```
S       = Σ x_i²                       (exact, 128-bit accumulator)
m       = (S / n) >> 16                (then narrowed to i64)
r       = isqrt_q16(m + 1)
y_i     = (((x_i · r) >> 16) · g_i) >> 16,   g_i = 65536 when i ≥ len(g)
```

The epsilon is one Q16 unit (≈1.5·10⁻⁵). The artifact's
`layer_norm_rms_epsilon` (1·10⁻⁶ in the canonical GGUF) is **not** used. There
is no mean subtraction.

### 3.4 Projection (per-row INT8)

```
out_i = ((Σ_j w_ij · x_j) · s_i) >> 16
```

The input `x` is the full-precision i64 activation. It is **not** requantised
(the engine computes a `QuantizedInput` and discards it). The sum may be formed
in any order, because the domain (§9) bounds `Σ_j |w_ij·x_j|`. Each output row
depends only on its own weight row, so any partition of the rows computes the
same values. The row-partitioned backend
(`forward_one_token_canonical_i8_with_backend`) and the batched prefill kernel
(`prefill_canonical_i8_batched`) rely on this and are permitted strategies, not
profiles.

### 3.5 Rotary position embedding

For head width `d` and `h = d/2`, the tables hold `cos[pos·h + i]` and
`sin[pos·h + i]` for `pos < max_seq` (§8). A rotation of the pair `(x0, x1)`
is:

```
x0' = ((x0·c) >> 16) − ((x1·s) >> 16)
x1' = ((x0·s) >> 16) + ((x1·c) >> 16)
```

* **Split-half** (historical profile): pair `i` is coordinates `(i, i+h)`.
* **Interleaved** (`…rope-interleaved.v1`): pair `i` is `(2i, 2i+1)`, the GGUF
  Llama layout.

The engine executes the interleaved profile by permuting the Q and K
projection rows at load time from adjacent pairs to split-half order, then
applying the split-half rotation. The rotated values are identical and the
attention dot product is invariant under the shared permutation, so logits are
identical to rotating in place. Only the internal K-cache layout differs (§7).

### 3.6 SiLU and the gated feed-forward block

```
σ(x)   = (65536·65536) / max(65536 + exp_q16(−x), 1)        if x ≥ 0
       = (exp_q16(x)·65536) / max(65536 + exp_q16(x), 1)    if x < 0
silu(x) = (x · σ(x)) >> 16
a_j     = (silu(gate_j) · up_j) >> 16
```

### 3.7 Attention (one query head)

Query head `q` reads KV head `(q · n_kv_heads) / n_heads`; consecutive query
heads share a KV head. Keys are the RoPE-rotated K rows of positions
`0 … pos` (including the current one) and values are the V rows. The score is:

```
score_j = (((q · k_j) >> 16) · attn_scale) >> 16
```

where `q · k_j` is the **exact** 64-bit dot product (§12 records the kernel
that currently narrows it) and `attn_scale = isqrt_q16(d·65536)` (§8).

The softmax is the **sequential online form**, evaluated in position order.
It is not equal to a two-pass softmax, because rescaling truncates:

```
M = −2^62 (i64::MIN / 2),  Z = 0,  o = 0⃗
for j = 0 … pos:
    if score_j > M:                     (strictly greater)
        c = exp_q16(M − score_j)
        Z = (Z·c) >> 16;  o_t = (o_t·c) >> 16 for every t
        M = score_j
    w = exp_q16(score_j − M)
    Z = Z + w
    o_t = o_t + ((w · v_j,t) >> 16)
if Z > 0: o_t = (o_t · 65536) / Z       (truncates toward zero)
```

The head outputs are concatenated in head order.

### 3.8 One token through the model

```
h = E[token]                                   (Q16 embedding row)
for each layer:
    n  = rmsnorm(h, attn_norm)
    q, k, v = Wq·n, Wk·n, Wv·n                  (§3.4)
    rotate every Q head and every KV head of k (§3.5) at position pos
    append k and v to this layer's cache
    h  = h + Wo·attention(q, cache)             (§3.7)
    n  = rmsnorm(h, ffn_norm)
    h  = h + Wdown·a(Wgate·n, Wup·n)            (§3.6)
logits = Wout · rmsnorm(h, final_norm)
```

All residual additions are exact. `token < vocab_size` and `pos < max_seq` are
preconditions (§9).

## 4. Token selection

* **argmax:** the first index holding the maximum (ties go to the **lowest
  index**).
* **Repetition penalty** (all paid and historical generation): for each of the
  **64 most recent** generated tokens, newest first, **once per occurrence**
  (a token that appears three times is scaled three times), ignoring ids
  `≥ vocab_size`:

  ```
  ℓ = (ℓ·5) / 6   if ℓ > 0
  ℓ = (ℓ·6) / 5   if ℓ ≤ 0          (truncating: −7 → −8, not −9)
  ```

  then argmax. Prompt tokens are not penalised.
* **Permitted sampling:** only the two deterministic rules above. No profile
  admits temperature, top-k, top-p or any randomised draw. Stochastic sampling
  would need a new identity with a committed seed derivation.

## 5. Generation

### 5.1 Generation v2 (bound by native paid requests)

Input: the request's `input_blob`, read as little-endian `u32` token ids. It
must be non-empty, a whole number of ids, and must not begin with BOS. Every id
must be `< vocab_size` (§12: not yet enforced). Admission requires
`1 + prompt_len + max_tokens ≤ max_seq`, checked before any compute.

```
logits = forward(BOS)                      (the only BOS; owned by generation)
for t in prompt: logits = forward(t)       (the last prompt logits are reused)
out = []
repeat max_tokens times:
    next = penalised_argmax(logits, out)   (§4)
    out.append(next)
    if next ∈ eos_tokens: stop             (the EOS id is part of the output)
    logits = forward(next)
output_hash = BLAKE3(out as little-endian u32)
```

For the canonical artifact, `eos_tokens = [2]` and `BOS = 1`, both read from
GGUF metadata. `eot`/`eom` ids are not read (M3).

### 5.2 Historical generation (`generate`, not for paid work)

Identical except that the prompt loop discards its logits, and each step
forwards the last token again, so the last prompt token is processed twice. An
empty prompt forwards token 0. The committed model KAT pins this form.

### 5.3 Greedy diagnostic

v2 with plain argmax and no penalty. It exists only for comparisons against
reference implementations and is never a paid identity.

### 5.4 Pipeline shards

A terminal shard returns the selected token and a `logits_hash`. That hash is
taken **after** the repetition penalty has been applied in place, so with a
non-empty history it commits to the penalised logits.

## 6. Execution strategies that must not change a value

These strategies are permitted only because each has bit-identity evidence.
None of them is a profile:

* batched multi-token prefill (`b489c80`/`92b9103`, 16,000,000 logits by digest);
* the limb kernel (on by default where the CPU has it: AVX2 on x86-64, NEON
  dotprod on arm64; `ARC_CANONICAL_KERNEL=scalar` forces the scalar kernel; it
  refuses inputs it cannot prove exact);
* thread count (the KAT runs at 1, 2 and 4 threads);
* layer-range sharding (the KAT's three-way split matches the whole model);
* row-partitioned projections (§3.4).

## 7. KV cache

The cache holds post-RoPE K rows and raw V rows, one row of `n_kv_heads·d`
values per position and layer. Its byte layout is internal: an interleaved-profile
engine may store K in either pair order. The KAT's cache digests are
regression checks for the Rust layout, which is split-half order for both
profiles.

## 8. Preparing a model (GGUF → integer state)

Preparation is the only place floating point appears. Its results, not the
float steps, are what execution consumes. The rules are:

* **Dequantisation:** GGUF blocks are expanded to f32 exactly as candle does it:
  IEEE single precision in the written expression order, **without fused
  multiply-add**. A compiler that contracts `d·q − m` into an FMA can change
  weights.
* **Per-row INT8:**
  `a = max(max_j |x_j|, 1e-10)` (f32); `k = 127 / a` (f32);
  `w_j = clamp(round_half_away(x_j · k), −127, 127)` (f32 multiply);
  `s = max(round_half_away(a · 65536 / 127), 1)` computed in f64.
* **Embedding table:** `round_half_away(x · 65536)` in f64, kept as i64 for
  every vocabulary row.
* **Norm weights:** `round_half_away(x · 65536)` in f32. A missing norm tensor
  must be a load error. §12 records that the engine currently substitutes `ONE`.
* **RoPE tables:** `f_i = 1 / base^(2i/d)`, `θ = pos · f_i`, entries
  `round_half_away(cos θ · 65536)` and `round_half_away(sin θ · 65536)`, all in
  f64 with host `pow`/`cos`/`sin`, `max_seq = 4096` (a loader constant, not the
  GGUF context length), and `base = rope.freq_base` or 10000 when that key is
  absent (as in the canonical artifact).
  *Portability:* for the canonical shape (d = 128, 4,096 positions, base
  10000), `python3 -m arc_conformance.rope_margin` shows that this host's libm
  reproduces the correctly-rounded table. No exact value lies closer than
  1.1·10⁻⁶ units to a rounding boundary (worst entry: position 2909, pair 62).
  Any libm within **150,879 ulps** on cos/sin and **458 ulps** on pow therefore
  builds the identical table. BLAKE3 of the table (cos then sin, LE i64):
  `990bb9d51595861ef4d7a2a80da4a40a7bef5ad207011824c0f39fbb821d6c41`.
* **Attention scale:** `isqrt_q16(d·65536)` = 5795 for d = 128 (integer only).
* **Interleaved profile:** after loading, permute each Q head's and each K head's
  rows (and their scales) from `[e0, o0, e1, o1, …]` to
  `[e0, e1, …, o0, o1, …]`.

**Prepared-state digest (v1).** Two preparers agree when this digest matches.
It is BLAKE3 over, in order: `n_layers, d_model, n_heads, n_kv_heads, d_ff,
vocab_size, max_seq` as u64; `attn_scale`; the RoPE cos table, then the sin
table; the Q16 embedding table; then the INT8 embedding, the output matrix and
the final norm; then, per layer, `wq, wk, wv, wo, w_gate, w_up, w_down,
attn_norm, ffn_norm`. A matrix is its INT8 bytes followed by its i64 scales,
in executed row order (after the interleaved rewrite). All integers are
little-endian. The artifact identity stays the BLAKE3 of the GGUF bytes. This
digest is a conformance check on preparation, not a new identity (M1).

## 9. Domain

The profile is defined only where every intermediate fits its declared width:

* each projection row: `Σ_j |w_ij·x_j| < 2^63` and `|acc·s_i| < 2^63`;
* each attention dot: `Σ_t |q_t·k_t| < 2^63`, and every other product above
  fits in i64;
* the RMS mean square fits in i64 after the shift;
* `token < vocab_size`, `pos < max_seq`.

Outside the domain, a conforming executor **refuses**. The Python reference
raises `DomainError`. The Rust engine does not refuse: release builds wrap
(`overflow-checks` is off), debug builds panic, and out-of-vocabulary tokens
return empty logits. On the scalar paths wrapping is at least identical on
every platform, but it is not meaningful output. §12 tracks the one place where
it is not identical.

## 10. Conformance evidence

| Evidence | What it shows | Where |
|---|---|---|
| Model KAT (Rust constants) | 7 fields: weight hash, 6 logits digests, next tokens, KV digest, 12 shard-boundary digests, historical generation and its hash | `crates/arc-inference/tests/fixtures/integer_inference_kat.json`, `golden_vectors.rs` |
| **Independent reproduction** | the Python executor, written from this document, reproduces **7/7** of those fields | `python3 -m arc_conformance.kat` |
| Operator vectors | exp (63 inputs), isqrt (18), RMS norm (8), SiLU (54), projections (3), RoPE both layouts (8), attention (10, incl. rising/falling scores, negative values truncating toward zero, >32-bit elements, and Q/K elements exactly at and one past the signed 32-bit bounds that decide D1's aarch64 fast path), penalty (8), argmax (5), interleaved profile + generation v2 sequences | `crates/arc-inference/tests/fixtures/integer_operator_kat.json` |
| Mutation coverage | 15 single-clause mutations of the reference; every one is caught by at least one of the two files. 4 (penalty multiplicity, penalty window ×2, argmax ties) are caught **only** by the operator vectors | `python3 -m arc_conformance.mutations` |
| Preparation vectors | a 1-layer GGUF recipe (12-token vocabulary, grouped-query 2:1, one all-zero embedding row) prepared under §8: prepared-state digest, 6 logits digests, a generation-v2 run. The Rust side writes the same GGUF with candle and loads it through `load_cached_model_canonical_i8_interleaved_rope` | `integer_operator_kat.json` → `gguf_preparation`; `python3 -m arc_conformance.preparation` is imported by the vector generator |
| RoPE portability margin | §8 | `python3 -m arc_conformance.rope_margin` |
| Python unit tests | 11 tests, including domain refusals | `python3 -m unittest arc_conformance.tests.test_integer_reference` |

**Not yet executed:** the Rust engine has not been run against
`integer_operator_kat.json`. Until it has, the operator vectors are one
implementation's answers, and the interleaved/v2 and preparation sections are
expectations, not cross-checks. The Rust tests are written
(`golden_vectors.rs`: `integer_operators_match_the_independent_reference`,
`interleaved_profile_and_generation_v2_match_the_independent_reference`,
`gguf_preparation::*` under `--features candle`) and wait for a build window. Also not covered here: the real 7B artifact end to end (M4),
the preparation float steps on the real GGUF, and any non-ARM host.

## 11. Profiles outside this contract

INT16, block-INT8, Q4, ternary and ternary-hybrid are separate identities. They
are not eligible for paid work and are not covered here. The INT16 NEON dot
product narrows activations to 32 bits; its comment says so and that path has
no guard.

## 12. Known deviations and their status

| # | Deviation | Consequence | Status |
|---|---|---|---|
| D1 | The aarch64 attention dot (`dot_i64xi64_attn_neon`) narrows Q and K to 32 bits with `vmovn_s64`; the portable path multiplies full i64 | ARM and x86 disagree once any Q/K element exceeds ±2^31 (±32768.0). The operator vector "elements outside signed 32 bits" exercises it | fix pending (guarded fallback to the exact loop); needs a build |
| D2 | Native execution accepts prompt ids ≥ `vocab_size`; `forward_one_token` returns empty logits and skips the position | Every honest validator skips it identically, so there is no disagreement, but a paid output is computed from a corrupted sequence | fix pending: refuse in `prequalified_prompt` |
| D3 | A missing norm tensor silently becomes all-`ONE` (`extract_norm`) | A malformed artifact loads and runs as a different model | fix pending: load error for canonical profiles |
| D4 | Comments claim `round(exp)` for the table, `round(1/√x)` for isqrt, and "numerically equivalent" for the online softmax | Misleads re-implementers; this document is normative | comment fix pending |
| D5 | Out-of-domain arithmetic wraps (release) or panics (debug) instead of refusing | Meaningless but deterministic on the scalar paths | documented; a checked build is out of scope for v1 |
