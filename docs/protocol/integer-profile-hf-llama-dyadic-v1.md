# ARC integer profile for modern Llama-family models (dyadic v1)

Status: **normative for `arc.hf-llama.i8-dyadic-row.q16.v1`**, the profile
ARC uses for SmolLM3-3B (`HuggingFaceTB/SmolLM3-3B`). It is an additional
profile. It does not replace or modify the canonical GGUF profile
(`arc.gguf-llama.i8-per-row.rope-interleaved.v1`, see
[integer-profile-contract-v1.md](integer-profile-contract-v1.md)), and nothing
in the network's consensus or native-inference bindings refers to it.

An implementation conforms when, for every admissible input, it produces
bit-identical package bytes (§4), logits (§5) and tokens (§6) to the rules
below. Two implementations exist and are checked against each other in CI:

| Implementation | Where |
|---|---|
| Rust engine and converter | `crates/arc-inference/src/modern/`, CLI `arc-modern` |
| Independent Python executor and preparer, written from this document | `scripts/arc_conformance/modern_reference.py` |

## 1. Identities

| Identity | String | BLAKE3 of the string |
|---|---|---|
| Arithmetic profile | `arc.hf-llama.i8-dyadic-row.q16.v1` | `3eb41a4fe376be020e2b93ff4ca10d55977d113278546ba2ee74622c3db1fa48` |
| Generation semantics (default) | `arc.hf-chat.no-bos.rp64-argmax.le-u32.v1` | `f267f6818464cfaed9afe66eec8b29efd59e7208dde72b711487e73756450fdb` |
| Generation semantics (diagnostic greedy) | `arc.hf-chat.no-bos.argmax.le-u32.v1` | `f04974ecfda896adb86c944b8a1dcd4c9060eec8193f8ecb6db3bac48c3b75a6` |
| Package schema | `arc.integer-package.v1` | |
| Package manifest schema | `arc.integer-package-manifest.v1` | |
| Tokenizer | `arc.hf-tokenizers.bytelevel-bpe.v1` + SHA-256 of `tokenizer.json` | |

## 2. What changed relative to the GGUF profile, and why

Every change below keeps the arithmetic integer-only and independent of
evaluation order. Each removes a known precision loss of the v1 contract.

| Item | GGUF profile v1 | This profile | Why |
|---|---|---|---|
| Source weights | GGUF Q4_K/Q6_K dequantised in f32 | BF16 safetensors, read as exact rationals | 4-bit source and f32 steps removed; preparation is pure integer arithmetic |
| Row scale | Q16 integer `round(absmax·2^16/127)` | dyadic `μ·2^−k`, `2^30 ≤ μ < 2^31` | Q16 scales carry 0.1–3 % gain error on rows with absmax 0.3–0.02; dyadic error ≤ 2^−31 |
| Embedding | Q16 i64 (8 B/value) plus an INT8 copy | INT8 rows with dyadic scales, expanded on lookup; shared with the tied LM head | 8× smaller resident embedding |
| RMSNorm | Q16 mean square, 5-step Newton isqrt, ε = 1 Q16 unit, two roundings | Q32 mean square, exact integer square root, model ε, one rounding | the v1 mean square truncates small-RMS vectors (embeddings) by up to several percent |
| exp table | recurrence `T[i] = (T[i+1]·65281) >> 16`, up to 142 units low | correctly rounded `e^x`, ≤ 0.5 unit | the v1 tail is 10–100 % low below `x = −4` |
| Softmax | sequential online softmax, truncating each term | two-pass: exact max, exact sums, one division | order-free (allows parallel attention) and one rounding |
| Attention scale | `isqrt_q16(d·2^16)` (5795 for d = 128) | `⌊2^30/√d⌋` applied in Q46 | exact constant |
| RoPE | split-half or interleaved, two roundings per output, libm-built tables | split-half (Hugging Face `rotate_half`), one rounding, tables shipped in the package | removes libm from the trust base |
| NoPE layers | none | per-layer flag | SmolLM3 skips RoPE on every 4th layer |
| KV cache | i64 | i32 (domain-checked) | half the memory; value-preserving |
| BOS | one internal BOS forward | none | SmolLM3's template and tokenizer add no BOS |

## 3. Number formats and primitive operations

* **Activations, residual stream, Q/K/V, logits:** signed integers in Q16
  (`v` denotes `v / 2^16`), stored as i64.
* **Weights:** INT8 in `[−127, 127]`. A package containing −128 is refused.
* **Row scale:** a pair `(μ, k)` of integers. Either `2^30 ≤ μ < 2^31` and
  `16 ≤ k ≤ 62`, or the row is all zero and `(μ, k) = (0, 16)`. The real scale
  is `μ · 2^−k`.
* **Norm gains:** Q16, i64.
* **RoPE tables:** Q16, i32.
* **KV cache:** i32 values (Q16).
* `a >> b` is an arithmetic shift: `⌊a / 2^b⌋` for any sign of `a`.
* `tdiv(a, b)` divides and truncates toward zero.
* `rha(x)` rounds a real number half away from zero.
* `isqrt(n)` is the exact integer square root `⌊√n⌋` of a non-negative integer.
* Every product and sum is computed exactly in an integer wide enough to hold
  it (128 bits suffices everywhere below). §9 lists the domain; outside it an
  implementation **refuses** (returns an error). It never wraps.

## 4. Preparation: BF16 safetensors → integer package

Preparation reads only the integer bit patterns of BF16 values. No floating
point arithmetic is used, so any implementation gets the same bytes.

### 4.1 Exact value of a BF16 number

For 16-bit pattern `b`: `s = b >> 15`, `E = (b >> 7) & 0xFF`, `m = b & 0x7F`.
`E = 255` (infinity or NaN) is refused. Otherwise the value is
`(−1)^s · M · 2^e` with

* `E = 0`: `M = m`, `e = −133` (subnormal; `M = 0` is a zero);
* `E ≥ 1`: `M = 128 + m`, `e = E − 134`.

For finite values, `|v|` orders exactly as the integer `b & 0x7FFF`.

### 4.2 Per-row INT8 with a dyadic scale

For a row `v_0 … v_{n−1}` (one output feature: one row of a `[out, in]`
matrix, or one vocabulary row of the embedding):

1. `A = max_j |v_j|`. If `A = 0`, every `q_j = 0` and `(μ, k) = (0, 16)`.
2. Write `A = M_A · 2^{e_A}` (§4.1 of the element with the largest magnitude).
3. For each `j` with `M_j > 0`: `d = e_A − e_j` (always `≥ 0`).
   `q_j = sign(v_j) · ⌊(2·127·M_j + M_A·2^d) / (2·M_A·2^d)⌋`, i.e.
   `rha(127·|v_j|/A)` with sign. If `d > 60`, `q_j = 0`. If `M_j = 0`, `q_j = 0`.
4. Scale: let `t` be the smallest integer `≥ 0` with
   `M_A · 2^t ≥ 127 · 2^30`. Then
   `μ = ⌊(2·M_A·2^t + 127) / 254⌋` (that is `rha(M_A·2^t/127)`).
   If `μ = 2^31`, set `μ = 2^30` and `t = t − 1`.
   `k = t − e_A`. If `k < 16` or `k > 62`, the tensor is refused.

`μ·2^−k` equals `A/127` to within a relative `2^−31`.

### 4.3 Norm gains

`g = rha(v · 2^16)` computed exactly. With `v = (−1)^s M 2^e`: if
`e + 16 ≥ 0` then `|g| = M · 2^{e+16}`; otherwise, with `c = −(e+16) ≥ 1`,
`|g| = ⌊(2M + 2^c) / 2^{c+1}⌋`. The sign is `s`.

### 4.4 RMS epsilon

`ε_q32 = rha(ε · 2^32)` where `ε` is `rms_norm_eps` read from `config.json`
as an IEEE double (the decimal text parsed to the nearest double). For
SmolLM3 (`1e-06`) this is **4295**. `ε_q32 ≥ 1` is required.

### 4.5 RoPE tables

Let `D` be the head width, `h = D/2`, `θ` the integer `rope_theta`
(5,000,000 for SmolLM3; a non-integer `θ` is refused), and `S = max_seq`.
For `p ∈ [0, S)` and `i ∈ [0, h)`:

```text
ω_i       = θ^(−2i/D)                      (exact real number)
cos[p][i] = rha(cos(p·ω_i) · 2^16)
sin[p][i] = rha(sin(p·ω_i) · 2^16)
```

The tables are defined by these exact real values. An implementation must
compute with enough precision to round every entry correctly. For SmolLM3
(`D = 128`, `S = 4096`, `θ = 5·10^6`) the closest exact value lies
`1.8·10^−6` Q16 units from a rounding boundary (position 2189, pair 1), so an
absolute error below `10^−6` units suffices. The reference algorithm
(Q62 fixed point, i128) is in §4.7. BLAKE3 of `cos` (row-major, LE i32) followed
by `sin`:

`2024b1037902e099975b3c1e9fb989fe5e0845761b381401e9f7f295a3c8ea1b`

Spot values: `cos[1][0..3] = 35409, 46321, 53432`; `sin[1][0..3] = 55147,
46361, 37947`; `sin[4095][63] = 68`.

### 4.6 Model shape and NoPE

Read from `config.json`. Required: `hidden_act = "silu"`, `rope_scaling = null`,
`attention_bias = false`, `mlp_bias = false`, `tie_word_embeddings = true`,
no sliding window, `hidden_size = num_attention_heads · head_dim` (with
`head_dim = hidden_size / num_attention_heads` when absent). `rope_layers[l] =
no_rope_layers[l]` (1 = apply RoPE, 0 = NoPE). For SmolLM3 layers 3, 7, …, 35
are NoPE. `max_seq = 4096` (a cap below the declared 65,536; no YaRN).

### 4.7 Reference algorithm for the tables (informative)

All values are Q62 integers (`ONE = 2^62`) in signed 128-bit arithmetic.
`mulq(a, b) = (a·b + 2^61) >> 62`.

* `atanh(z)` for `0 ≤ z ≤ ONE/3`: sum `z + p_1/3 + p_2/5 + …` with
  `p_n = mulq(p_{n−1}, mulq(z, z))`, terms `⌊p_n/(2n+1)⌋`, until a term is 0.
* `ln2 = 2·atanh(⌊ONE/3⌋)`. For an integer `θ`, `b = ⌊log2 θ⌋`,
  `y = θ·2^{62−b}`, `z = ⌊(y − ONE)·2^62 / (y + ONE)⌋`, `ln θ = b·ln2 + 2·atanh(z)`.
* `x = ⌊2·ln θ / D⌋` (must be `< 64·ONE`). With `x = n·ONE + f`, `0 ≤ f < ONE`,
  `r = E(f)` multiplied `n` times by `E(ONE)` (`mulq`), where `E(y)` is the
  Taylor series `t_n = ⌊mulq(t_{n−1}, y) / n⌋` with alternating signs until a
  term is 0.
* `ω_0 = ONE`, `ω_i = mulq(ω_{i−1}, r)`. `cos ω_i`, `sin ω_i` by Taylor
  (`t_n = ⌊mulq(t_{n−1}, ω)/n⌋`).
* Rotation: `c_0 = ONE, s_0 = 0`;
  `c_p = (c·C − s·S + 2^61) >> 62`, `s_p = (s·C + c·S + 2^61) >> 62`.
* Entry: `rha(c_p / 2^46)`.

The exp table (§5.1) uses `E = Σ_n (−1)^n ⌊…⌋` with `t_0 = ONE`,
`t_n = ⌊t_{n−1}/(256 n)⌋` (n < 20), then `P_0 = ONE`,
`P_m = (P_{m−1}·E + 2^61) >> 62`, `T[4096 − m] = rha(P_m / 2^46)`.
Both algorithms reproduce the correctly rounded tables exactly (checked against
200-bit and 60-digit decimal evaluations).

### 4.8 Package file (`arc.integer-package.v1`)

```text
offset 0   : "ARCIPKG1"                 (8 bytes)
offset 8   : header length H            (u64 little-endian)
offset 16  : header                     (H bytes, canonical JSON, §4.9)
             zero bytes up to data_start = ⌈(16 + H)/64⌉·64
data_start : tensors in header order; tensor i starts at data_start + offset_i;
             every tensor is followed by zero bytes up to a multiple of 64.
file length = data_start + ⌈(offset_last + bytes_last)/64⌉·64
```

`offset_0 = 0`, `offset_{i+1} = ⌈(offset_i + bytes_i)/64⌉·64`. Data types:
`i8`, `u8`, `i32` (LE), `i64` (LE). Arrays are row-major.

Tensor order and shapes (`V` vocab, `D` model width, `F` FFN width, `Hq`
query heads, `Hk` KV heads, `Dh` head width, `S` max_seq, `L` layers):

```text
embed.q            i8  [V, D]        model.embed_tokens.weight
embed.mu           i32 [V]
embed.k            u8  [V]
final_norm         i64 [D]           model.norm.weight
rope.cos           i32 [S, Dh/2]
rope.sin           i32 [S, Dh/2]
for l in 0..L:
  layers.l.attn_norm   i64 [D]       model.layers.l.input_layernorm.weight
  layers.l.wq.q/mu/k   [Hq·Dh, D]    self_attn.q_proj.weight
  layers.l.wk.q/mu/k   [Hk·Dh, D]    self_attn.k_proj.weight
  layers.l.wv.q/mu/k   [Hk·Dh, D]    self_attn.v_proj.weight
  layers.l.wo.q/mu/k   [D, Hq·Dh]    self_attn.o_proj.weight
  layers.l.ffn_norm    i64 [D]       model.layers.l.post_attention_layernorm.weight
  layers.l.w_gate.q/mu/k [F, D]      mlp.gate_proj.weight
  layers.l.w_up.q/mu/k   [F, D]      mlp.up_proj.weight
  layers.l.w_down.q/mu/k [D, F]      mlp.down_proj.weight
```

(`X.q` is i8 `[rows, cols]`, `X.mu` i32 `[rows]`, `X.k` u8 `[rows]`.) The
source must contain exactly these tensors and no others, all BF16, with these
shapes.

### 4.9 Header

Canonical JSON: `json.dumps(obj, sort_keys=True, separators=(",", ":"),
ensure_ascii=True)`. No floating point numbers appear. Fields:

```text
schema   "arc.integer-package.v1"
profile  "arc.hf-llama.i8-dyadic-row.q16.v1"
model    {architecture, n_layers, d_model, n_heads, n_kv_heads, d_head, d_ff,
          vocab_size, max_seq, rms_eps_q32, rope_theta, rope_layers [0/1 per layer],
          tied_embeddings: true}
source   {repo, revision, files: [{name, bytes, sha256}] for config.json and the
          safetensors shards, in the order of the source manifest}
tensors  [{name, dtype, shape, offset, bytes}] in file order
```

The package's identity is the SHA-256 (and BLAKE3) of the whole file.

### 4.10 Distribution

ARC hosts no weights. A node reads a pinned source manifest
(`docs/protocol/packages/smollm3-3b.source.json`: repository, revision, byte
length and SHA-256 of each file), downloads the public BF16 files from Hugging
Face at that revision, refuses any file whose length or SHA-256 differs,
converts on the device, and refuses to serve unless the package's SHA-256
equals the pinned package manifest
(`docs/protocol/packages/smollm3-3b.integer-package.json`).

## 5. Operators

### 5.1 Exponential `exp(x)` for `x ≤ 0` (Q16 in, Q16 out)

`T[i] = rha(e^{−(4096−i)/256} · 2^16)` for `i ∈ [0, 4096]` (`T[4096] = 65536`,
`T[3840] = 24109`, `T[0..3] = 0`). BLAKE3 of the 4,097 LE i64 entries:
`3586482438115a39e0e4b822f62451897f0b1b5baaf2e1019bbfb360a61bf0e2`.

```text
x = 0               → 65536
x ≤ −16·2^16        → 0
otherwise           o = x + 16·2^16, i = o >> 8, f = o & 255
                    → T[i] + (((T[i+1] − T[i]) · f) >> 8)
```

### 5.2 Projection

Input `x` (length `K`), weight row `q_i` (INT8), scale `(μ_i, k_i)`:

```text
acc_i = Σ_j q_ij · x_j                     (exact)
y_i   = (acc_i · μ_i) >> k_i               (exact product, arithmetic shift)
```

Precondition (checked once per input vector): `127 · Σ_j |x_j| < 2^63`.
Each `y_i` must satisfy `|y_i| ≤ 2^62`.

### 5.3 Embedding lookup

Token `t`: `e_j = (q_tj · μ_t) >> (k_t − 16)`.

### 5.4 RMS normalisation

```text
S   = Σ_i x_i²                              (exact)
v   = ⌊S / n⌋ + ε_q32
r   = isqrt(⌊2^92 / v⌋)                     (1/rms in Q30)
y_i = (x_i · r · g_i) >> 46
```

`v ≤ 2^92` is required. There is no mean subtraction.

### 5.5 Rotary position embedding (split-half)

For a head vector `u` of width `D`, `h = D/2`, at position `p`, for `i < h`
with `c = cos[p][i]`, `s = sin[p][i]`, `a = u_i`, `b = u_{i+h}`:

```text
u_i     = (a·c − b·s) >> 16
u_{i+h} = (a·s + b·c) >> 16
```

Applied to every query head and every KV head of `k`, only in layers with
`rope_layers[l] = 1`. This is the pairing of Hugging Face `rotate_half`, which
the safetensors Q/K rows already use; no row permutation is applied.

### 5.6 Attention (one query head, two-pass)

Query head `h` reads KV head `⌊h · n_kv_heads / n_heads⌋`. Keys and values
are the cached rows of positions `0 … p` (the current position included).
With `λ = isqrt(⌊2^60 / D⌋)` (94,906,265 for `D = 128`):

```text
dot_j   = Σ_t q_t · k_{j,t}                 (exact)
score_j = (dot_j · λ) >> 46
M       = max_j score_j
w_j     = exp(score_j − M)
Z       = Σ_j w_j
o_t     = Σ_j w_j · v_{j,t}                 (exact)
out_t   = tdiv(o_t, Z)
```

Every sum is exact, so the result does not depend on the order in which
positions are visited. Head outputs are concatenated in head order.

### 5.7 Gated SiLU

For gate `g` and up `u` (Q16):

```text
σ(g) = ⌊2^32 / (2^16 + exp(−g))⌋                 if g ≥ 0
     = ⌊exp(g) · 2^16 / (2^16 + exp(g))⌋         if g < 0
a    = (g · σ(g) · u) >> 32
```

### 5.8 One token

```text
h = embed(token)
for each layer l:
    n = rmsnorm(h, attn_norm_l)
    q, k, v = Wq·n, Wk·n, Wv·n
    if rope_layers[l]: rotate every query head of q and every KV head of k at p
    append k and v (as i32) to layer l's cache
    h = h + Wo·attention(q, cache_l)
    n = rmsnorm(h, ffn_norm_l)
    h = h + Wdown·gated_silu(Wgate·n, Wup·n)
logits = Wembed·rmsnorm(h, final_norm)          (tied: the embedding rows and scales)
```

Residual additions are exact; every resulting element must satisfy
`|h_i| ≤ 2^62`. Cached K/V values must lie in `[−2^31, 2^31 − 1]`.

## 6. Generation

### 6.1 Selection

* **argmax:** the first index holding the maximum (ties go to the lowest id).
* **rp64-argmax (default):** for each of the 64 most recent generated tokens,
  newest first, once per occurrence, `ℓ = tdiv(5ℓ, 6)` if `ℓ > 0`, else
  `ℓ = tdiv(6ℓ, 5)`; then argmax. Prompt tokens are not penalised. (This is
  the selection rule of the GGUF profile's generation v2.)

### 6.2 Semantics `arc.hf-chat.no-bos.*.le-u32.v1`

Input: non-empty prompt token ids, each `< vocab_size`, with
`prompt_len + max_tokens ≤ max_seq`. **No BOS is forwarded.**

```text
for t in prompt: logits = forward(t)          (positions 0 … P−1)
out = []
loop:
    next = select(logits, out)
    out.append(next)
    if next ∈ eos or len(out) = max_tokens: stop   (SmolLM3: eos = [128012] "<|im_end|>")
    logits = forward(next)
output_hash = BLAKE3(out as LE u32)
```

`max_tokens ≥ 1`. The last selected token is never forwarded, so a run makes
exactly `P + len(out) − 1` forward calls.

### 6.3 Digests used in CI

* `logits_hash_t = BLAKE3(logits of forward call t as LE i64)`.
* A run's `logits_digest = BLAKE3(logits_hash_0 ‖ logits_hash_1 ‖ …)` over
  every forward call in order (`P + len(out) − 1` calls).

## 7. Prompt text (SmolLM3 chat template, single turn)

ARC renders the pinned `chat_template.jinja` for one user message, no tools,
`add_generation_prompt = true`. The template prints the current date; ARC
takes the date as an explicit request field (`today`, format `%d %B %Y`, e.g.
`06 October 2026`) so that every node renders the same text. Default
reasoning mode is `/no_think`. Without a system message:

```text
<|im_start|>system\n## Metadata\n\nKnowledge Cutoff Date: June 2025\nToday Date: {today}\n
Reasoning Mode: /no_think\n\n## Custom Instructions\n\nYou are a helpful AI assistant named
SmolLM, trained by Hugging Face.\n\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n
<think>\n\n</think>\n
```

(Line breaks above are for display; `\n` is the only newline.) The template
closes the system block with `<|im_end|>` only when tools are present; ARC
reproduces that exactly. CI compares the rendering with
`transformers.apply_chat_template`.

The text is tokenised by a byte-level BPE that implements the pinned
`tokenizer.json` (Llama-3 split regex, `ignore_merges`, 256 added tokens
matched leftmost-longest before splitting, no BOS/EOS added). Tokenisation is
not on the verification path: requests and receipts carry token ids.

## 8. KV cache and memory (SmolLM3-3B)

| Item | Bytes |
|---|---|
| INT8 weights incl. tied embedding | 3,075,098,624 values ≈ 3.08 GB |
| scales | 5 B per row |
| KV per position | 36 layers × 2 × 512 × 4 B = 147,456 B |
| KV at 4,096 positions | 604 MB |

## 9. Domain

An implementation refuses when any of these fails:

* projection input: `127 · Σ|x_j| < 2^63`; output `|y| ≤ 2^62`;
* residual and every stored activation: `|v| ≤ 2^62`;
* RMS: `v ≤ 2^92`; products `|x·r·g| < 2^127`;
* KV entries within i32;
* token ids `< vocab_size`; positions `< max_seq`.

## 10. Conformance evidence

* Rust unit tests pin the exp and RoPE table digests above, the BF16 rules,
  the package layout and every operator against hand-computed values.
* The Python executor prepares its own package from the same BF16 files; CI
  requires its SHA-256 to equal the Rust converter's on every OS.
* The Python executor recomputes every logits vector of the golden prompts by
  teacher forcing and must match the Rust engine's `logits_hash` at every
  position, then re-derives each generated token from its own logits.
* The hash matrix runs the converter and the engine (scalar and SIMD kernels)
  on ubuntu x86-64, windows x86-64, macOS arm64 and macOS x86-64.
