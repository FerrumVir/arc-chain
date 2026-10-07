# ARC integer profile for DeepSeek-V3-architecture models: MLA + MoE, sharded by layer (dyadic v1)

Status: **normative for `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1`**.
The development model is Moonlight-16B-A3B-Instruct
(`moonshotai/Moonlight-16B-A3B-Instruct`, MIT licence). Moonlight uses the
DeepSeek-V3 architecture and the same `tiktoken.model` file as Kimi K2.6.
This is an additional profile. It changes nothing in consensus, rewards or
native inference, and it does not replace the canonical GGUF profile.

The profile builds on [integer-profile-hf-llama-dyadic-v1.md](integer-profile-hf-llama-dyadic-v1.md)
("dyadic v1"). It reuses that profile's:
- number formats and primitive operations (§3);
- BF16 rules (§4.1);
- per-row INT8 dyadic quantisation (§4.2), norm gains (§4.3) and ε (§4.4);
- RoPE table definition (§4.5, §4.7);
- exp, projection, embedding, RMS-norm and gated-SiLU operators (§5.1–§5.4, §5.7);
- generation semantics (§6).

This document adds four things:
- multi-head latent attention (MLA) with decoupled RoPE;
- the mixture-of-experts layer with sigmoid routing, a correction bias,
  shared experts and integer routing weights;
- pipeline **stages**: the package, boundary files and replay rules that let
  any contiguous range of layers be executed and verified alone;
- the tiktoken tokenizer.

An implementation conforms when, for every admissible input, it produces
bit-identical stage packages (§4), boundary files (§6), logits (§5) and tokens
(§7) to the rules below. Two implementations exist and are checked against
each other in CI:

| Implementation | Where |
|---|---|
| Rust engine, converter and CLI `arc-mla` | `crates/arc-inference/src/modern/mla/`, `src/modern/tiktoken.rs`, `src/bin/arc_mla.rs` |
| Independent Python preparer and executor, written from this document | `scripts/arc_conformance/mla_moe_reference.py` |

## 1. Identities

| Identity | String | BLAKE3 of the string |
|---|---|---|
| Arithmetic profile | `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1` | `7b0bd25616bd29195da71bb0b02d3c436350e801811280def1bb29c22d75207e` |
| Generation semantics (default) | `arc.hf-chat.no-bos.rp64-argmax.le-u32.v1` (dyadic v1 §6) | `f267f6818464cfaed9afe66eec8b29efd59e7208dde72b711487e73756450fdb` |
| Generation semantics (diagnostic) | `arc.hf-chat.no-bos.argmax.le-u32.v1` | `f04974ecfda896adb86c944b8a1dcd4c9060eec8193f8ecb6db3bac48c3b75a6` |
| Stage package schema | `arc.integer-stage-package.v1` | |
| Stage manifest schema | `arc.integer-stage-manifest.v1` | |
| Boundary file schema | `arc.stage-boundary.v1` | |
| Tokenizer | `arc.tiktoken-bpe.v1` + SHA-256 of `tiktoken.model` | `9506a8dc9c7806eae5eb7427be6056cb541ebbfcf1e9921915ce839e9d8a50c9` |
| Variant: INT4 group-32 routed experts (§13) | `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.i4g32-experts.q16.v1` | `5e6d6392186d817e57806184b1193ea1648805bb82cf7749f9d563306237e71c` |

The generation semantics are the dyadic v1 ones unchanged. They define
selection, stop rule and digests, and do not depend on the model.

## 2. Notation and model shape

| Symbol | Meaning | `config.json` key | Moonlight-16B-A3B | Kimi K2.6 (text) |
|---|---|---|---|---|
| L | layers | `num_hidden_layers` | 27 | 61 |
| D | model width | `hidden_size` | 2048 | 7168 |
| H | attention heads | `num_attention_heads` | 16 | 64 |
| Qr | query LoRA rank (0 = none) | `q_lora_rank` (null → 0) | 0 | 1536 |
| C | KV latent rank | `kv_lora_rank` | 512 | 512 |
| N | per-head no-RoPE query/key width | `qk_nope_head_dim` | 128 | 128 |
| R | per-head RoPE width | `qk_rope_head_dim` | 64 | 64 |
| Vh | per-head value width | `v_head_dim` | 128 | 128 |
| F | dense FFN width | `intermediate_size` | 11264 | 18432 |
| K0 | dense layers first | `first_k_dense_replace` | 1 | 1 |
| E | routed experts | `n_routed_experts` | 64 | 384 |
| k | experts per token | `num_experts_per_tok` | 6 | 8 |
| Es | shared experts | `n_shared_experts` | 2 | 1 |
| Fm | expert FFN width | `moe_intermediate_size` | 1408 | 2048 |
| G, Gk | expert groups, groups kept | `n_group`, `topk_group` | 1, 1 | 1, 1 |
| ρ | routing scale, Q32 | `routed_scaling_factor` | 2.446 → 10505490006 | 2.827 → 12141872546 |
| V | vocabulary | `vocab_size` | 163840 | 163840 |
| θ | RoPE base | `rope_theta` | 50000 | 50000 |
| ε | RMS epsilon, Q32 | `rms_norm_eps` | 1e-5 → 42950 | 1e-5 → 42950 |
| S | served context (cap) | source manifest `max_seq` | 4096 | — |

Layer `l` is a **dense layer** when `l < K0` and an **MoE layer** otherwise.

### 2.1 Supported configurations

The converter reads `config.json` and refuses anything outside the following:
- `model_type` is `deepseek_v3` or `kimi_k2`, and `hidden_act = "silu"`.
- `attention_bias` is false or absent.
- `scoring_func = "sigmoid"` and `topk_method = "noaux_tc"`.
- `moe_layer_freq = 1`, and `num_nextn_predict_layers` is 0 or absent.
- `num_key_value_heads = num_attention_heads`.
- `rope_scaling = null` (YaRN is not in v1, see §11), `tie_word_embeddings = false`, and `rope_interleave` is true or absent.
- `rope_theta` is an integer.
- `E` is a multiple of `G`, and `E/G ≥ 2` when `G > 1`.
- `1 ≤ Gk ≤ G`, `1 ≤ k ≤ E` and `k ≤ Gk·E/G`.
- `n_shared_experts ≥ 1`.
- `K0 ≤ L`, and `R` is even.
- `routed_scaling_factor` gives `1 ≤ ρ < 2^40`, and the ε rule gives `ε_q32 ≥ 1`.

Other keys (dropout, auxiliary-loss settings, `ep_size`, `pretraining_tp`)
do not affect inference and are ignored.

The check is driven by the configuration. When the file wraps the language
model in a `text_config` object (Kimi K2.5, K2.6 and K3 do), the converter reads
that object. It recognises the family from the text model's `model_type`:
DeepSeek-V3 MLA (`deepseek_v3`, `kimi_k2`) or Kimi Linear (`kimi_linear`, which
Kimi K3 uses). It then refuses with the full list of unsupported features rather
than the first one. Today the list for Kimi-K2.6 is the multimodal wrapper, the
pre-quantised INT4 experts and YaRN. The list for Kimi K3 is in §11.2.

### 2.2 The `model` object

The package header and the stage manifest carry this canonical JSON object,
which contains integers, booleans and strings only. Its fields are:

```text
architecture, n_layers, d_model, n_heads, q_lora_rank, kv_lora_rank,
qk_nope_dim, qk_rope_dim, v_head_dim, d_ff, first_k_dense, n_routed_experts,
n_experts_per_tok, n_shared_experts, moe_d_ff, n_group, topk_group,
norm_topk_prob, routed_scaling_q32, vocab_size, max_seq, rms_eps_q32,
rope_theta, attention_lambda, tied_embeddings (false)
```

`routed_scaling_q32 = rha(routed_scaling_factor · 2^32)` and
`rms_eps_q32 = rha(rms_norm_eps · 2^32)`. In both cases the JSON decimal is
parsed to the nearest IEEE double, and the double is scaled exactly.

`attention_lambda = λ = ⌊2^30 / √(N + R)⌋ = isqrt(⌊2^60 / (N + R)⌋)`. It
is 77,490,641 for N + R = 192.

## 3. Number formats

These are as in dyadic v1 §3, with the following additions:
- **INT16 router rows** hold values `q ∈ [−32767, 32767]` with a
  power-of-two scale `2^−k`, where `0 ≤ k ≤ 62`. The row's real value is
  `q · 2^−k` (§4.3).
- **Routing weights** are Q32 integers in i64 (§5.5).
- **Correction biases** are Q32 integers in i64 (§4.4).

## 4. Preparation: BF16 safetensors → stage packages

Preparation reads integer bit patterns only and never evaluates floating point,
so every implementation produces the same bytes.

### 4.1 Tensor mapping

Every source tensor is BF16, except `e_score_correction_bias`, which may be
BF16 or F32. Tensors named `*.self_attn.rotary_emb.inv_freq` are ignored, and
any other unexpected tensor, missing tensor or wrong shape is refused. The
source prefix is `model.` (`lm_head.weight` has no prefix). "Dyadic `[r, c]`"
means the three tensors `X.q` (i8 `[r, c]`), `X.mu` (i32 `[r]`) and `X.k`
(u8 `[r]`), with each row quantised by dyadic v1 §4.2. "Dyadic `[n, r, c]`"
is the same with a leading dimension. Each of the `n·r` rows is quantised
independently, and `X.mu` and `X.k` have shape `[n, r]`.

| Package tensor | Format | Source (`model.layers.l.` prefix unless noted) |
|---|---|---|
| `rope.cos`, `rope.sin` | i32 `[S, R/2]` | computed, §4.5 |
| `embed` | dyadic `[V, D]` | `model.embed_tokens.weight` |
| `layers.l.attn_norm` | i64 `[D]`, dyadic v1 §4.3 | `input_layernorm.weight` |
| `layers.l.wq` (Qr = 0) | dyadic `[H(N+R), D]` | `self_attn.q_proj.weight` |
| `layers.l.wq_a` (Qr > 0) | dyadic `[Qr, D]` | `self_attn.q_a_proj.weight` |
| `layers.l.q_a_norm` (Qr > 0) | i64 `[Qr]` | `self_attn.q_a_layernorm.weight` |
| `layers.l.wq_b` (Qr > 0) | dyadic `[H(N+R), Qr]` | `self_attn.q_b_proj.weight` |
| `layers.l.wkv_a` | dyadic `[C+R, D]` | `self_attn.kv_a_proj_with_mqa.weight` |
| `layers.l.kv_a_norm` | i64 `[C]` | `self_attn.kv_a_layernorm.weight` |
| `layers.l.wk_b` | dyadic `[H, C, N]` | `self_attn.kv_b_proj.weight`, see below |
| `layers.l.wv_b` | dyadic `[H, Vh, C]` | `self_attn.kv_b_proj.weight`, see below |
| `layers.l.wo` | dyadic `[D, H·Vh]` | `self_attn.o_proj.weight` |
| `layers.l.ffn_norm` | i64 `[D]` | `post_attention_layernorm.weight` |
| `layers.l.w_gate`, `w_up` (dense) | dyadic `[F, D]` | `mlp.gate_proj.weight`, `mlp.up_proj.weight` |
| `layers.l.w_down` (dense) | dyadic `[D, F]` | `mlp.down_proj.weight` |
| `layers.l.router.q` (MoE) | i16 `[E, D]` | `mlp.gate.weight`, §4.3 |
| `layers.l.router.k` (MoE) | u8 `[E]` | ″ |
| `layers.l.router_bias` (MoE) | i64 `[E]` | `mlp.gate.e_score_correction_bias`, §4.4 |
| `layers.l.shared.w_gate`, `.w_up` | dyadic `[Es·Fm, D]` | `mlp.shared_experts.gate_proj.weight`, `.up_proj.weight` |
| `layers.l.shared.w_down` | dyadic `[D, Es·Fm]` | `mlp.shared_experts.down_proj.weight` |
| `layers.l.experts.w_gate`, `.w_up` | dyadic `[E, Fm, D]` | `mlp.experts.e.gate_proj.weight`, `.up_proj.weight`, for e = 0 … E−1 |
| `layers.l.experts.w_down` | dyadic `[E, D, Fm]` | `mlp.experts.e.down_proj.weight` |
| `final_norm` | i64 `[D]` | `model.norm.weight` |
| `lm_head` | dyadic `[V, D]` | `lm_head.weight` (no prefix) |

**Splitting `kv_b_proj`.** The source matrix `B` has shape `[H(N+Vh), C]`.
Its rows for head `j` are `j(N+Vh) … j(N+Vh)+N−1` (the key part) followed by
`Vh` value rows. The package stores:
- `wk_b[j]` as the `[C, N]` matrix whose row `r` is `(B[j(N+Vh)+t][r] for t = 0 … N−1)`. This is the transpose of head `j`'s key block, which is what absorbed attention multiplies by.
- `wv_b[j]` as the `[Vh, C]` matrix whose row `t` is `B[j(N+Vh)+N+t]`, the rows as stored.

Each row of both is quantised by dyadic v1 §4.2. This is the "absorbed"
arrangement of DeepSeek's reference inference (`attn_impl = "absorb"`).

### 4.2 Dyadic rows

These follow dyadic v1 §4.2 exactly. The row of a `[n, r, c]` tensor is the
`c` values of one `(n, r)` index.

### 4.3 Router rows: INT16 with a power-of-two scale

For one router row of BF16 patterns `v_0 … v_{D−1}`:
1. If every element is ±0, then `q = 0` and `k = 16`.
2. Otherwise, take `(M_A, e_A)`, the dyadic v1 §4.1 parts of an element
   with the largest magnitude. Set `k = 7 − e_A`; if `k < 0` or `k > 62`, the
   tensor is refused.
3. For each element with parts `(s, M, e)`, compute `q_j = rha(M · 2^{e+k})`
   with sign `s`. This is exact when `e + k ≥ 0`. Otherwise, with
   `c = −(e+k)`, `|q_j| = ⌊(2M + 2^c) / 2^{c+1}⌋`.

The largest element maps to `128·M_A ≤ 32640`. Every element within 7
binades of the row maximum is represented exactly. Smaller elements are
rounded half away from zero, with an absolute error of at most `2^{e_A−8}`.

### 4.4 Correction bias

`b_q32 = rha(v · 2^32)`, computed exactly from the value's parts:
- for BF16, the parts of dyadic v1 §4.1;
- for F32, with `s = bit 31`, `E = bits 23–30` and `m = bits 0–22`:
  - `E = 255` is refused;
  - `E = 0` gives `M = m`, `e = −149`;
  - otherwise `M = 2^23 + m`, `e = E − 150`.

As in dyadic v1 §4.3: if `e + 32 ≥ 0`, then `|b| = M·2^{e+32}`; otherwise
`|b| = ⌊(2M + 2^c) / 2^{c+1}⌋` with `c = −(e+32)`. `|b| ≤ 2^62` is required.

### 4.5 RoPE tables

`cos[p][i] = rha(cos(p·ω_i)·2^16)` and `sin[p][i] = rha(sin(p·ω_i)·2^16)`,
with `ω_i = θ^{−2i/R}`, for `i < R/2` and `p < S`. This is the dyadic v1 §4.5
definition with the head width replaced by the RoPE width `R`. The reference
algorithm of dyadic v1 §4.7 applies unchanged.

For Moonlight (`θ = 50000`, `R = 64`, `S = 4096`), the tables' digest is
pinned by the conformance tests once CI has produced it (§12).

### 4.6 Stage packages (`arc.integer-stage-package.v1`)

A **stage** is a contiguous layer range `[a, b)` with `0 ≤ a < b ≤ L`. The
whole model is the stage `[0, L)`. A stage package holds exactly the tensors
that stage needs, in this order:

```text
rope.cos, rope.sin                                  (every stage)
embed.q, embed.mu, embed.k                          (only when a = 0)
for l in a .. b−1, the layer's tensors in this order:
  attn_norm,
  wq.{q,mu,k}                                       (Qr = 0)
  | wq_a.{q,mu,k}, q_a_norm, wq_b.{q,mu,k}          (Qr > 0)
  wkv_a.{q,mu,k}, kv_a_norm, wk_b.{q,mu,k}, wv_b.{q,mu,k}, wo.{q,mu,k},
  ffn_norm,
  w_gate.{q,mu,k}, w_up.{q,mu,k}, w_down.{q,mu,k}   (dense layer)
  | router.q, router.k, router_bias,
    shared.w_gate.{q,mu,k}, shared.w_up.{q,mu,k}, shared.w_down.{q,mu,k},
    experts.w_gate.{q,mu,k}, experts.w_up.{q,mu,k}, experts.w_down.{q,mu,k}   (MoE layer)
final_norm, lm_head.q, lm_head.mu, lm_head.k        (only when b = L)
```

Tensor names carry the layer prefix `layers.l.`. Data types are i8, u8, i16
(LE), i32 (LE) and i64 (LE), stored row-major.

The file container is the dyadic v1 §4.8 container with the magic
`ARCSPKG1`. Alignment, padding, offsets and file length are the same. The
header is canonical JSON (dyadic v1 §4.9) with these fields:

```text
schema   "arc.integer-stage-package.v1"
profile  "arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1"
model    the §2.2 object
source   {repo, revision, files: [{name, bytes, sha256}]} for config.json and every
         safetensors shard of the full model, in source-manifest order
stage    {first_layer: a, end_layer: b}
tensors  [{name, dtype, shape, offset, bytes}] in file order
```

The tensor table must equal the layout determined by (`model`, `stage`).
Readers refuse anything else.

### 4.7 Segments, model root and the stage manifest

Stage packages for different layouts contain different files, but the same
**segments**:

| Segment | Tensors |
|---|---|
| `tables` | `rope.cos`, `rope.sin` |
| `embed` | `embed.q`, `embed.mu`, `embed.k` |
| `layer.l` | every `layers.l.*` tensor |
| `head` | `final_norm`, `lm_head.q`, `lm_head.mu`, `lm_head.k` |

A segment's digest is the BLAKE3 of the concatenation of its tensors' bytes
(each tensor's `bytes` bytes, without padding), in layout order. The **model
root** is defined as follows:

```text
model_root = BLAKE3(canonical JSON {"model": model, "profile": PROFILE, "segments":
             [[name, blake3_hex] for tables, embed, layer.0 … layer.L−1, head], "source": source})
```

It identifies the weights independently of how they are split into stages.

The **stage manifest** (`arc.integer-stage-manifest.v1`, committed under
`docs/protocol/packages/`) records the profile, `model`, `source`, every
segment's byte count and digest, `model_root`, the generation and tokenizer
identities, and `manifest_blake3`. `manifest_blake3` is the BLAKE3 of the
canonical JSON with `manifest_blake3` removed, as in the dyadic v1 manifest.

A node holding any stage package checks:
- that its `model` and `source` equal the manifest's;
- that every segment it contains hashes to the manifest's digest.

Only then does it serve or verify that stage.

### 4.8 Distribution

Every stage, including a verifier's slice, is produced on the device:
- read the pinned source manifest (`docs/protocol/packages/moonlight-16b-a3b-instruct.source.json`);
- download only the safetensors shards that hold the needed tensors (Moonlight keeps layer `l` in shard `l+1`, the embedding in shard 1, and `lm_head` and `model.norm` in shard 27);
- refuse any file whose length or SHA-256 differs;
- convert `[a, b)`;
- check the segment digests against the stage manifest.

ARC hosts no weights.

## 5. Operators

### 5.1 Interleaved RoPE

For a vector `u` of width `R` at position `p`, for each `i < R/2`, let
`a = u_{2i}`, `b = u_{2i+1}`, `c = cos[p][i]` and `s = sin[p][i]`. Then:

```text
u_{2i}   = (a·c − b·s) >> 16
u_{2i+1} = (a·s + b·c) >> 16
```

This is the pairing of DeepSeek's reference code (complex multiplication of
adjacent pairs). Hugging Face's `apply_rotary_pos_emb` for this architecture
de-interleaves both `q` and `k` before `rotate_half`, which permutes the
output elements identically in `q` and `k`. Every attention score is a dot
product of the two, so the scores are the same.

### 5.2 Multi-head latent attention (one layer, position p, input h)

```text
x   = rmsnorm(h, attn_norm)                                   (dyadic v1 §5.4)
q   = Wq·x                       if Qr = 0                    (dyadic v1 §5.2; H(N+R) values)
    = Wq_b·rmsnorm(Wq_a·x, q_a_norm)   if Qr > 0
kv  = Wkv_a·x                                                 (C + R values)
c   = rmsnorm(kv[0 .. C), kv_a_norm)                          (the latent)
kp  = rope(kv[C .. C+R), p)                                   (§5.1; one key shared by all heads)
append c and kp to the layer's cache as i32 (domain-checked)
for each head j in 0 .. H−1:
    qn = q[j(N+R) .. j(N+R)+N)
    qp = rope(q[j(N+R)+N .. (j+1)(N+R)), p)
    qa = Wk_b[j]·qn                                           (C values)
    for each cached position i in 0 .. p:
        dot_i   = Σ_r qa_r·c_{i,r} + Σ_t qp_t·kp_{i,t}        (exact)
        score_i = (dot_i·λ) >> 46
    M   = max_i score_i
    w_i = exp(score_i − M)                                    (dyadic v1 §5.1)
    Z   = Σ_i w_i
    u_r = tdiv(Σ_i w_i·c_{i,r}, Z)                            (r < C; exact sum, one rounding)
    o_j = Wv_b[j]·u                                           (Vh values)
h = h + Wo·(o_0 ‖ o_1 ‖ … ‖ o_{H−1})
```

The cache holds `C + R` values per position and layer (MLA's compressed KV),
not per-head keys and values.
- Every sum is exact, so neither the order in which positions are visited nor
  the order of heads changes a value.
- Key and value up-projections are applied to `q` and to the attention-weighted
  latent `u` rather than to every cached position. This is DeepSeek's absorbed
  form, fixed here as the definition. A "naive" implementation that
  up-projects every cached latent rounds at different points and computes a
  different function.

### 5.3 Router logits

For an MoE layer with normalised input `x` (Q16):

```text
ℓ_e = (Σ_j router.q[e][j]·x_j) >> router.k[e]                (exact sum; arithmetic shift)
```

The precondition is `32767·Σ_j |x_j| < 2^63`, and `|ℓ_e| ≤ 2^62`.

### 5.4 Expert selection

```text
σ_e   = sigmoid(ℓ_e)                       (dyadic v1 §5.7, Q16 in [0, 2^16])
key_e = σ_e·2^16 + router_bias_e          (Q32)
```

1. **Group limit (only when G > 1).** Expert `e` belongs to group
   `⌊e·G/E⌋`. A group's score is the sum of its two largest keys. Keep the
   `Gk` groups with the largest scores, breaking ties by the lower group index.
   Experts outside the kept groups are not eligible.
2. **Top-k.** `T` is the `k` eligible experts with the largest keys, ties
   broken by the **lower expert index**. `T` is listed by key, descending, then
   by index, ascending.

These are integer comparisons of exact values, so every implementation selects
the same experts.

### 5.5 Routing weights (Q32)

When `norm_topk_prob` is set and `k > 1`, let `Σσ = Σ_{e∈T} σ_e`:
- if `Σσ = 0`, then every `w_e = 0`;
- otherwise `w_e = ⌊σ_e·ρ / Σσ⌋`.

When it is not set, or `k = 1`, `w_e = ⌊σ_e·ρ / 2^16⌋`. Weights are Q32 and
use the sigmoid scores, not the biased keys, as in DeepSeek-V3.

### 5.6 Expert FFN, shared experts and combine

```text
ffn(W, x) = W.w_down·a,   a_t = gated_silu((W.w_gate·x)_t, (W.w_up·x)_t)    (dyadic v1 §5.7)
y_e       = ffn(experts[e], x)                       for e ∈ T              (D values each)
s         = ffn(shared, x)
out_j     = (Σ_{e∈T} w_e·y_{e,j}) >> 32 + s_j        (exact sum, one floor shift, exact add)
```

Because the routed sum is exact and rounded once, the result is identical
whichever device computes which expert. This holds for any expert-parallel
layout that exchanges either the exact `y_e` vectors or exact partial sums of
`w_e·y_e`.

### 5.7 One layer, one stage, one token

```text
layer l:  h = MLA_l(h, p)                                   (§5.2)
          x = rmsnorm(h, ffn_norm_l)
          h = h + ffn(dense_l, x)            if l < K0
          h = h + moe_l(x)                   otherwise      (§5.3–§5.6)
stage [a, b) at position p:
          h = embed(token)                   if a = 0       (dyadic v1 §5.3)
            = the boundary-a input vector    otherwise
          for l in a .. b−1: layer l
          output h (boundary b); if b = L also logits = lm_head·rmsnorm(h, final_norm)
```

The whole model is the stage `[0, L)`. Every residual addition is exact, and
every stored value must satisfy `|v| ≤ 2^62`.

## 6. Stage boundaries

### 6.1 Definition

Boundary `l` (`0 ≤ l ≤ L`) is the residual stream entering layer `l`:
- boundary 0 is the embedding output;
- boundary `L` is the last layer's output, before `final_norm`.

A stage `[a, b)` maps boundary `a` to boundary `b`.

### 6.2 Hashes

```text
activation_hash(v)  = BLAKE3(the D values of v as LE i64)
boundary_digest     = BLAKE3(activation_hash(position 0) ‖ … ‖ activation_hash(position P−1))
```

These are computed per sequence and boundary. A golden run records
`boundary_digest` at every boundary `0 … L` for every case.

### 6.3 Boundary file (`arc.stage-boundary.v1`)

```text
offset 0   "ARCBND01"                      (8 bytes)
offset 8   header length H                 (u64 LE)
offset 16  header                          (canonical JSON)
           zero bytes up to data_start = ⌈(16 + H)/64⌉·64
data_start for each sequence in header order, for each position, the D values as LE i64
file length = data_start + 8·D·Σ positions
```

The header has these fields:
- `schema`, `profile`, `model_root`;
- `layer`, the boundary index `l`;
- `d_model`;
- `sequences`: for each sequence `{id, tokens, prompt_len, selection, eos,
  max_tokens, digest}`, where:
  - `tokens` are the token ids forwarded at positions `0 … P−1`;
  - `prompt_len`, `selection`, `eos` and `max_tokens` describe the generation
    they came from (dyadic v1 §6.2);
  - `digest` is the sequence's `boundary_digest`.

A reader refuses a file whose data does not hash to the header's digests.

### 6.4 Executing a stage alone

Given a stage package `[a, b)` and either token sequences (`a = 0`) or a
boundary-`a` file, a stage runs each sequence from position 0 with an empty
cache, one position at a time, and writes the boundary-`b` file. When
`b = L`, it also reports, for every position:
- `logits_hash = BLAKE3(logits as LE i64)`;
- the token re-derived by the sequence's selection rule from the logits and
  the tokens already generated.

The stage never needs any other stage's weights or cache.

A generation forwards `prompt ‖ out[0 … n−2]`, and its forward call at
position `t` receives exactly `tokens[t]`. A stage pipeline over those token
sequences therefore reproduces every logits vector and every boundary of the
generation.

### 6.5 Replay and layout invariance

- **Replay.** A verifier holding stage `[a, b)`:
  1. takes the committed boundary-`a` file;
  2. checks it against the committed digests;
  3. runs the stage;
  4. compares its boundary-`b` digests (and, when `b = L`, logits hashes and
     re-derived tokens) with the commitments.

  Equal means the stage was computed correctly. Different means the first
  differing stage is at fault: no threshold, no false positive.
- **Layout invariance.** For any partition `0 = b_0 < b_1 < … < b_S = L`, the
  pipeline's boundary-`b_i` digests equal the single-process run's digests at
  `b_i`, and the final logits are byte-identical.

  This holds by construction:
  - each stage computes the same integer function of its input;
  - the boundary file carries the complete residual-stream state;
  - each stage's cache depends only on its own inputs.

  CI checks it for 1, 2 and 4 stages (§12).

## 7. Generation

Dyadic v1 §6 applies unchanged: `rp64-argmax` (default) or `argmax`, no BOS,
`P + len(out) − 1` forward calls, `output_hash`, `logits_hash`,
`logits_digest`. The EOS ids come from `config.json` `eos_token_id`; for
Moonlight-16B-A3B-Instruct this is `[163586]` (`<|im_end|>`).

## 8. Tokenizer (`arc.tiktoken-bpe.v1`) and chat prompt

Tokenisation is not on the verification path: requests and receipts carry
token ids. It is specified so that clients agree with the reference wrapper.

### 8.1 Vocabulary

- **`tiktoken.model`.** One line per token: `base64(bytes) rank`. Ranks are
  `0 … n_base−1`, with `n_base = 163584`.
- **Special tokens.** Ids `n_base … n_base + n_special − 1`:
  - `n_special = 258` for Moonlight's `tokenization_moonshot.py`;
  - `n_special = 256` for Kimi K2.6's `tokenization_kimi.py`.

  Each id's name comes from `tokenizer_config.json` `added_tokens_decoder`,
  else `<|reserved_token_{id}|>`.

### 8.2 Encoding (the wrapper's `encode(text)` with `allow_special_tokens=True`)

1. Cut the text into chunks of at most 400,000 code points. Then cut each
   chunk wherever a run of whitespace, or a run of non-whitespace, would
   exceed 25,000 code points. Whitespace here is Python's `str.isspace`.
2. In each piece, find the leftmost occurrence of any special-token name and
   emit its id. No name is a prefix of another. Encode the text between names
   as ordinary text.
3. **Ordinary text.** Split it with the regular expression below. For each
   match's UTF-8 bytes:
   - if the whole byte string is a token, emit its rank;
   - otherwise start from single bytes and repeatedly merge the adjacent pair
     whose concatenation has the lowest rank, leftmost first on ties, until no
     adjacent pair is a token. Then emit the parts' ranks.

```text
[\p{Han}]+
|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?
|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?
|\p{N}{1,3}
| ?[^\s\p{L}\p{N}]+[\r\n]*
|\s*[\r\n]+
|\s+(?!\S)
|\s+
```

(This is one alternation, printed with line breaks.)

**Decoding** concatenates the token bytes (a special token decodes to its
name) and decodes the result as UTF-8 with replacement.

### 8.3 Chat prompt (Moonlight-16B-A3B-Instruct, single turn)

The pinned `tokenizer_config.json` `chat_template`, rendered for one user
message with `add_generation_prompt = true`, gives:

```text
<|im_system|>system<|im_middle|>{system}<|im_end|><|im_user|>user<|im_middle|>{user}<|im_end|><|im_assistant|>assistant<|im_middle|>
```

`{system}` is `You are a helpful assistant` when no system message is given.
CI compares the rendering with `jinja2` applied to the pinned template.

## 9. KV cache and memory

| Item | Moonlight-16B-A3B | Kimi K2.6 (text) |
|---|---|---|
| Parameters in the stage packages (INT8 everywhere) | 15.96 G | 1.03 T (no vision tower) |
| KV per position, i32 (`L·(C+R)·4` B) | 62,208 B | 140,544 B |
| KV at 4,096 positions | 255 MB | 576 MB |
| Boundary vector per position (`8·D` B) | 16 KiB | 56 KiB |

## 10. Domain

An implementation refuses (never wraps) when any of the following fails:
- the dyadic v1 §9 conditions, for every projection, norm, RoPE, SiLU and
  residual;
- for the router, `32767·Σ|x| < 2^63` and `|ℓ| ≤ 2^62`;
- attention products and sums fit 127 bits, and every score satisfies
  `|score| ≤ 2^62`;
- the combine result, `|(Σ w·y) >> 32| ≤ 2^62`, and every boundary value
  satisfies `|h| ≤ 2^62`;
- latent and RoPE-key cache entries fit i32;
- token ids are `< V`, positions are `< S`, and boundary files match their
  header digests and the stage's `model_root` and layer.

## 11. Differences from Kimi K2.6 and K3, and what v1 does not cover

### 11.1 Kimi K2.6

| K2.6 feature | Status in v1 |
|---|---|
| Query LoRA (`q_lora_rank = 1536`, `q_a_layernorm`) | Implemented and tested on tiny models. Moonlight has no query LoRA. |
| 384 experts, top-8, 1 shared expert, group settings | General code paths. Tiny models exercise E = 8 / 16, k = 3 / 4, Es = 2 / 1, and G = 4 with Gk = 2. |
| F32 correction bias | Implemented (§4.4) and tested on tiny models. |
| YaRN RoPE (factor 64, β_fast 32, β_slow 1, mscale 1 / 1) | **Not in v1.** The converter refuses `rope_scaling`. YaRN changes only preparation: the frequencies `ω_i` and `λ` (by `mscale² = (0.1·ln 64 + 1)²`). The forward pass reads both from the package, so YaRN needs a new profile version, not new operators. |
| Routed experts stored as INT4 group-32 symmetric (`weight_packed`, BF16 `weight_scale`) | The **§13 variant** defines the exact integer form: INT4 values with their BF16 group scales, no requantisation. It is implemented with a quantiser for BF16 sources and checked on Moonlight. Reading K2.6's packed int32 words into the §13 layout is a lossless repacking that this PR does not implement. |
| `language_model.` tensor prefix, MoonViT vision tower | Not handled. The vision tower is unused for text. |
| `num_nextn_predict_layers = 0` | Same as Moonlight; MTP layers are refused. |

### 11.2 Kimi K3 (Kimi Linear architecture)

Kimi K3 was read from `moonshotai/Kimi-K3` at revision
`f831ab66814297da540d832a5235f8e904f29d06`: its `config.json`,
`modeling_kimi_linear.py` and its safetensors index. Its text model is
`kimi_linear`, not DeepSeek-V3. The converter recognises it and refuses it,
listing the features below. Supporting K3 needs a new profile with its own
`model` object. The stage package container, the segment digests and model root,
the boundary file (§6), the router (§5.5–§5.7) and the INT4 group format (§13)
carry over unchanged.

| K3 feature (config value) | What an integer profile needs |
|---|---|
| Kimi Delta Attention in 69 of 93 layers (`kda_layers`; the lists are 1-based) | A gated delta rule per head (96 heads of 128): a recurrent 128 × 128 state per head, L2-normalised q and k, a sigmoid β, a per-channel decay gate bounded below by `gate_lower_bound = −5`, 4-tap short convolutions on q, k and v, and a sigmoid-gated RMSNorm on the output. The recurrent order is the only one that is exactly reproducible, so it would be normative. Parallel prefill then comes from heads and channels, not from chunking time. |
| MLA in the other 24 layers, without RoPE and with a sigmoid output gate (`mla_use_nope`, `mla_use_output_gate`) | The §5.3 attention with the 64 decoupled dimensions left unrotated, plus one projection and a sigmoid (exp table) per output. |
| The `situ` activation: `β·tanh(g/β)·σ(g) · β′·tanh(u/β′)` with β = 4, β′ = 25 | Exact tanh and sigmoid tables over bounded domains, built like the exp table (§4.5). |
| Latent MoE (`routed_expert_hidden_size = 3584`): the routed experts run on a down-projection of the hidden state, followed by RMSNorm and an up-projection | Two dyadic projections and one RMSNorm (existing operators). The router stays on the full hidden state. |
| 896 routed experts, top-16, 2 shared, sigmoid router with correction bias, `n_group = 1` | Covered by §5.5–§5.7, with different key names in `config.json`. |
| Attention residuals (`attn_res_block_size = 12`): each layer softmax-mixes a stack of earlier block outputs | An exact RMS-normalised score and softmax over at most 9 vectors per position (existing operators). Stage boundaries must carry the block stack, not one hidden vector: up to 9 × 7168 values per position instead of 7168. |
| Routed experts as MXFP4 (E2M1 values, one power-of-two scale per 32) | Lossless: 2 × an E2M1 value is an integer in [−12, 12], and power-of-two scales make the §13 exact sum simpler. A reader for the packed format is needed. |
| Multimodal wrapper and vision tower | The same as for K2.6: read the `language_model.` prefix; the vision tower is unused for text. |

Sizes, from the safetensors index and the Hub's parameter count at that
revision: 2,779,931,837,184 parameters, of which 2,722,740,830,208 are routed
expert weights; 1,560,860,324,864 bytes on disk. Holding the experts at 4.25 bits
(MXFP4 with its scales) and the other 57.2 billion weights at INT8 takes about
1.50 TB. A KDA layer's state is a fixed 1,572,864 values per sequence, whatever
the context length; only the 24 MLA layers keep a per-token cache.

## 12. Conformance evidence

- Rust unit tests cover:
  - the identity hash, λ, the router quantiser, F32/BF16 bias rounding, RoPE pairing, the selection rules (ties and groups), routing weights, and the combine;
  - the stage layout, segment digests and the boundary file format;
  - thread-count and kernel (scalar/SIMD) invariance;
  - 1/2/4-stage layout invariance and per-stage replay, on tiny models;
  - the tokenizer's BPE merge order and pattern.
- The independent Python executor prepares the same stage packages from the
  same BF16 files. CI requires byte equality on tiny models and segment-digest
  equality on Moonlight-16B-A3B.
- On every forward call of the golden cases, the Python executor recomputes:
  - the logits;
  - the boundary digests;
  - the re-derived tokens.
- CI runs the hash matrix with scalar and SIMD kernels on ubuntu x86-64,
  ubuntu arm64 and windows x86-64. The macOS legs wait until macOS runners are
  free again; the workflow has no macOS job yet.
  - Tiny models run the whole pipeline.
  - Moonlight runs a two-layer + LM-head stage slice, replayed from the
    boundary committed by the Linux run.
- The §13 INT4 variant gets the same checks:
  - tiny models in both shapes run every check above;
  - Moonlight gets the Rust–Python package equality, scalar = SIMD golden
    generation, 1 and 4 stages, and a verifier slice replayed on every runner;
  - the quantiser is checked against a literal rational implementation of
    §13.3.
- Perplexity is measured against the BF16 weights on public-domain text, for
  both expert formats.
- The tokenizer is checked against the `tiktoken` library with the pinned
  `tiktoken.model`, and the chat prompt against `jinja2`.

## 13. Variant: INT4 group-32 routed experts

This variant is identified by `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.i4g32-experts.q16.v1`.
Everything is as above except the routed experts. Their three stacked matrices
are stored as signed 4-bit values with one BF16 scale per group of 32 inputs.
That is the representation Kimi-K2.6 ships its experts in: 98.9% of its
parameters, about 0.56 bytes per parameter, 582 GB in total instead of 1,028 GB.

### 13.1 Format

For each `layers.l.experts.{w_gate, w_up, w_down}` of shape `[E, r, c]` (where
`c` is a multiple of 32), the stage package stores two tensors in place of
`.q`, `.mu` and `.k`:

| Tensor | Format | Contents |
|---|---|---|
| `X.q4` | u8 `[E, r, c/2]` | Value `j` of a row is the low nibble of byte `j/2` for even `j` and the high nibble for odd `j`. Each nibble is a two's-complement integer in `[−8, 7]`. |
| `X.s` | u16 `[E, r, c/32]` | BF16 bit patterns of the group scales. The sign bit must be 0, and infinity and NaN are refused. |

The weight is exactly `q · s` for its group's scale `s`. The other segments, the
layout order and the container are unchanged. The `model` object is unchanged
too; the header's `profile` names the variant.

### 13.2 Projection

For one row with values `q_j` and scales `s_g` (dyadic v1 §4.1 parts
`(0, M_g, e_g)`), and input `x` (Q16):

```text
acc_g = Σ_{j in group g} q_j·x_j                     (exact)
E     = max{e_g : M_g > 0}                          (y = 0 if every M_g = 0)
S     = Σ_{g : M_g > 0, e_g ≥ E − 40} M_g·acc_g·2^(e_g − E + 40)   (exact)
y     = ⌊S·2^(E − 40)⌋                               (arithmetic shift)
```

The precondition is `8·Σ|x| < 2^63`, and `|y| ≤ 2^62`.
- This is the exact value `Σ_g s_g·acc_g`, rounded down once.
- A group whose scale is more than 40 binades below the row's largest scale
  contributes 0. This bound keeps the sum within 127 bits; real checkpoints
  are nowhere near it.
- Every term is exact, so splitting rows, groups or experts across threads or
  devices cannot change `y`.

### 13.3 Quantising BF16 experts (development models)

For each group of 32 BF16 values `w_j`, with `A = max |w_j|`:
1. **Zero group.** If `A = 0`, then `s = +0` and every `q_j = 0`.
2. **Scale.** Otherwise `s` is `A/7` rounded to 8 significant bits, half away
   from zero:
   - choose the integer `f` with `128 ≤ A/(7·2^f) < 256`;
   - `m = rha(A/(7·2^f))`;
   - if `m = 256`, set `m = 128` and increase `f` by 1;
   - `s = m·2^f`, encoded as a normal BF16 with exponent field `f + 134`. A
     field outside `[1, 254]` is refused.
3. **Values.** `q_j = clamp(rha(w_j / s), −8, 7)`, computed exactly from the
   BF16 parts. When `w_j/s` is below `2^−40` in magnitude, `q_j = 0`.

This gives `|q_j| ≤ 7`. The value −8 occurs only in checkpoints quantised
elsewhere (such as Kimi-K2.6's `weight_packed`), which the format accepts as
published.
