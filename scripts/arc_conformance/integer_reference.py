"""Independent reference executor for ARC's per-row INT8 integer profile.

Written from docs/protocol/integer-profile-contract-v1.md, not translated
line by line from the Rust engine, so that agreement between the two is
evidence that the written contract is complete. Every value is a Python int;
each operation is checked against the signed 64-bit domain the contract
defines, and leaving that domain raises DomainError instead of wrapping.
"""

from __future__ import annotations

FRAC_BITS = 16
ONE = 1 << FRAC_BITS
I64_MIN = -(1 << 63)
I64_MAX = (1 << 63) - 1
I32_MIN = -(1 << 31)
I32_MAX = (1 << 31) - 1


class DomainError(ArithmeticError):
    """An intermediate left the domain in which the profile is defined."""


def i64(value: int, what: str = "value") -> int:
    if value < I64_MIN or value > I64_MAX:
        raise DomainError(f"{what} = {value} is outside signed 64-bit range")
    return value


def tdiv(a: int, b: int) -> int:
    """Signed division truncating toward zero (Rust `/` on integers)."""
    if b == 0:
        raise DomainError("division by zero")
    q = abs(a) // abs(b)
    return q if (a >= 0) == (b > 0) else -q


def trem(a: int, b: int) -> int:
    """Remainder with the sign of the dividend (Rust `%` on integers)."""
    return a - tdiv(a, b) * b


def mul(a: int, b: int, what: str = "product") -> int:
    return i64(a * b, what)


def shr(a: int, bits: int = FRAC_BITS) -> int:
    """Arithmetic right shift: floor division by 2**bits."""
    return a >> bits


# --- exp ---------------------------------------------------------------------

EXP_LUT_SIZE = 4096
EXP_LUT_RANGE = 16 * ONE
EXP_DECAY = 65281  # round(exp(-1/256) * 2**16)


def _build_exp_lut() -> list[int]:
    table = [0] * (EXP_LUT_SIZE + 1)
    table[EXP_LUT_SIZE] = ONE
    for i in range(EXP_LUT_SIZE - 1, -1, -1):
        table[i] = (table[i + 1] * EXP_DECAY) >> FRAC_BITS
    return table


EXP_LUT = _build_exp_lut()


def integer_exp(x: int) -> int:
    if x >= 0:
        return ONE
    if x <= -EXP_LUT_RANGE:
        return 0
    offset = x + EXP_LUT_RANGE
    step = ONE // 256
    idx = offset // step
    frac = offset % step
    lo = EXP_LUT[idx]
    hi = EXP_LUT[idx + 1]
    return lo + tdiv((hi - lo) * frac, step)


# --- inverse square root -------------------------------------------------------

def integer_isqrt(x: int) -> int:
    """Q16 approximation of 1/sqrt(x / 2**16) by five Newton steps."""
    if x <= 0:
        return ONE * 100
    bits = x.bit_length() - 1
    y = tdiv(ONE * 256, 1 << ((bits + 1) // 2))
    if y <= 0:
        y = 1
    for _ in range(5):
        y2 = shr(mul(y, y, "isqrt y*y"))
        xy2 = shr(mul(x, y2, "isqrt x*y2"))
        three_minus = 3 * ONE - xy2
        y = tdiv(mul(y, three_minus, "isqrt y*(3-xy2)"), 2 * ONE)
        if y <= 0:
            y = 1
    return y


# --- operators -------------------------------------------------------------------

def rms_norm(values: list[int], gamma: list[int]) -> list[int]:
    n = len(values)
    if n == 0:
        return []
    sq_sum = sum(v * v for v in values)  # 128-bit accumulator in the contract
    if sq_sum >= 1 << 127:
        raise DomainError("rms_norm square sum exceeds signed 128-bit range")
    mean_sq = i64((sq_sum // n) >> FRAC_BITS, "rms_norm mean square")
    inv_rms = integer_isqrt(i64(mean_sq + 1, "rms_norm mean square + 1"))
    out = []
    for i, x in enumerate(values):
        norm = shr(mul(x, inv_rms, "rms_norm x*inv_rms"))
        g = gamma[i] if i < len(gamma) else ONE
        out.append(shr(mul(norm, g, "rms_norm norm*gamma")))
    return out


def matmul_rows(rows: list[list[int]], scales: list[int], x: list[int]) -> list[int]:
    """Per-row symmetric INT8 projection: (sum_j w_ij * x_j) * s_i >> 16."""
    out = []
    for row, scale in zip(rows, scales):
        products = [w * v for w, v in zip(row, x)]
        # The accumulator may be formed in any order: the domain bounds the
        # sum of magnitudes, so no partial sum in any order can overflow.
        if sum(abs(p) for p in products) > I64_MAX:
            raise DomainError("matmul row magnitude exceeds signed 64-bit range")
        out.append(shr(mul(sum(products), scale, "matmul acc*scale")))
    return out


def rope_split_half(v: list[int], pos: int, cos: list[int], sin: list[int]) -> None:
    half = len(v) // 2
    for i in range(half):
        c = cos[pos * half + i]
        s = sin[pos * half + i]
        x0, x1 = v[i], v[i + half]
        v[i] = shr(mul(x0, c)) - shr(mul(x1, s))
        v[i + half] = shr(mul(x0, s)) + shr(mul(x1, c))


def rope_interleaved(v: list[int], pos: int, cos: list[int], sin: list[int]) -> None:
    half = len(v) // 2
    for i in range(half):
        c = cos[pos * half + i]
        s = sin[pos * half + i]
        x0, x1 = v[2 * i], v[2 * i + 1]
        v[2 * i] = shr(mul(x0, c)) - shr(mul(x1, s))
        v[2 * i + 1] = shr(mul(x0, s)) + shr(mul(x1, c))


def silu(x: int) -> int:
    if x >= 0:
        e = integer_exp(-x)
        sig = tdiv(ONE * ONE, max(ONE + e, 1))
    else:
        e = integer_exp(x)
        sig = tdiv(mul(e, ONE), max(ONE + e, 1))
    return shr(mul(x, sig, "silu x*sigmoid"))


def attention_head(q: list[int], keys: list[list[int]], values: list[list[int]],
                   attn_scale: int) -> list[int]:
    """Causal attention for one head with the sequential online softmax.

    The accumulator is rescaled whenever a strictly larger score appears, in
    position order; that order is part of the arithmetic, not an optimisation.
    """
    running_max = tdiv(I64_MIN, 2)
    running_sum = 0
    out = [0] * len(q)
    for k, v in zip(keys, values):
        products = [a * b for a, b in zip(q, k)]
        if sum(abs(p) for p in products) > I64_MAX:
            raise DomainError("attention dot magnitude exceeds signed 64-bit range")
        dot = sum(products)
        score = shr(mul(shr(dot), attn_scale, "attention score"))
        if score > running_max:
            correction = integer_exp(i64(running_max - score, "attention max shift"))
            running_sum = shr(mul(running_sum, correction))
            out = [shr(mul(o, correction)) for o in out]
            running_max = score
        w = integer_exp(score - running_max)
        running_sum = i64(running_sum + w, "attention weight sum")
        out = [i64(o + shr(mul(w, x, "attention w*v")), "attention output") for o, x in zip(out, v)]
    if running_sum > 0:
        out = [tdiv(mul(o, ONE), running_sum) for o in out]
    return out


def kv_head(head: int, n_heads: int, n_kv_heads: int) -> int:
    """Grouped-query attention: consecutive query heads share one KV head."""
    return head * n_kv_heads // n_heads


def argmax(values: list[int]) -> int:
    best_idx, best_val = 0, None
    for i, v in enumerate(values):
        if best_val is None or v > best_val:
            best_idx, best_val = i, v
    return best_idx


def repetition_penalty(logits: list[int], generated: list[int]) -> list[int]:
    """Scale the logit of each of the 64 most recent tokens, once per occurrence."""
    logits = list(logits)
    for token in list(reversed(generated))[:64]:
        if 0 <= token < len(logits):
            value = logits[token]
            logits[token] = tdiv(mul(value, 5), 6) if value > 0 else tdiv(mul(value, 6), 5)
    return logits


def repetition_penalty_select(logits: list[int], generated: list[int]) -> int:
    return argmax(repetition_penalty(logits, generated))


# --- model -----------------------------------------------------------------------

LEGACY_SPLIT_HALF = "INT8 integer (per-row, cross-platform deterministic)"
GGUF_INTERLEAVED = "arc.gguf-llama.i8-per-row.rope-interleaved.v1"


class Matrix:
    __slots__ = ("rows", "scales")

    def __init__(self, data: list[int], scales: list[int], n_rows: int, n_cols: int):
        if len(data) != n_rows * n_cols or len(scales) != n_rows:
            raise ValueError("matrix shape does not match its data")
        for w in data:
            if not -127 <= w <= 127:
                raise ValueError("INT8 weight outside [-127, 127]")
        self.rows = [data[r * n_cols:(r + 1) * n_cols] for r in range(n_rows)]
        self.scales = list(scales)

    def apply(self, x: list[int]) -> list[int]:
        return matmul_rows(self.rows, self.scales, x)


class Layer:
    def __init__(self, wq, wk, wv, wo, w_gate, w_up, w_down, attn_norm, ffn_norm):
        self.wq, self.wk, self.wv, self.wo = wq, wk, wv, wo
        self.w_gate, self.w_up, self.w_down = w_gate, w_up, w_down
        self.attn_norm, self.ffn_norm = attn_norm, ffn_norm


class Model:
    def __init__(self, *, profile, d_model, n_heads, n_kv_heads, d_ff, vocab_size,
                 max_seq, attn_scale, rope_cos, rope_sin, embedding_q16, layers,
                 final_norm, output, bos_token):
        if profile not in (LEGACY_SPLIT_HALF, GGUF_INTERLEAVED):
            raise ValueError(f"unsupported profile {profile!r}")
        if d_model % n_heads or n_heads % n_kv_heads or (d_model // n_heads) % 2:
            raise ValueError("unsupported head geometry")
        self.profile = profile
        self.d_model, self.n_heads, self.n_kv_heads = d_model, n_heads, n_kv_heads
        self.d_head = d_model // n_heads
        self.d_kv = self.d_head * n_kv_heads
        self.d_ff, self.vocab_size, self.max_seq = d_ff, vocab_size, max_seq
        self.attn_scale = attn_scale
        self.rope_cos, self.rope_sin = rope_cos, rope_sin
        self.embedding_q16 = embedding_q16
        self.layers = layers
        self.final_norm = final_norm
        self.output = output
        self.bos_token = bos_token

    def new_cache(self):
        return {"k": [[] for _ in self.layers], "v": [[] for _ in self.layers], "len": 0}

    def _rope(self, v, pos):
        if self.profile == GGUF_INTERLEAVED:
            rope_interleaved(v, pos, self.rope_cos, self.rope_sin)
        else:
            rope_split_half(v, pos, self.rope_cos, self.rope_sin)

    def embed(self, token: int) -> list[int]:
        if not 0 <= token < self.vocab_size:
            raise DomainError(f"token {token} is outside the vocabulary")
        d = self.d_model
        return list(self.embedding_q16[token * d:(token + 1) * d])

    def run_layers(self, hidden, cache, start, end, pos):
        dh = self.d_head
        for index in range(start, end):
            layer = self.layers[index]
            normed = rms_norm(hidden, layer.attn_norm)
            q = layer.wq.apply(normed)
            k = layer.wk.apply(normed)
            v = layer.wv.apply(normed)
            for h in range(self.n_heads):
                head = q[h * dh:(h + 1) * dh]
                self._rope(head, pos)
                q[h * dh:(h + 1) * dh] = head
            for h in range(self.n_kv_heads):
                head = k[h * dh:(h + 1) * dh]
                self._rope(head, pos)
                k[h * dh:(h + 1) * dh] = head
            cache["k"][index].append(k)
            cache["v"][index].append(v)
            attn = []
            for h in range(self.n_heads):
                kv_h = kv_head(h, self.n_heads, self.n_kv_heads)
                keys = [row[kv_h * dh:(kv_h + 1) * dh] for row in cache["k"][index]]
                vals = [row[kv_h * dh:(kv_h + 1) * dh] for row in cache["v"][index]]
                attn.extend(attention_head(q[h * dh:(h + 1) * dh], keys, vals, self.attn_scale))
            projected = layer.wo.apply(attn)
            hidden = [i64(a + b, "residual") for a, b in zip(hidden, projected)]
            normed_ff = rms_norm(hidden, layer.ffn_norm)
            gate = layer.w_gate.apply(normed_ff)
            up = layer.w_up.apply(normed_ff)
            act = [shr(mul(silu(g), u, "silu(gate)*up")) for g, u in zip(gate, up)]
            ff = layer.w_down.apply(act)
            hidden = [i64(a + b, "residual") for a, b in zip(hidden, ff)]
        return hidden

    def logits(self, hidden):
        return self.output.apply(rms_norm(hidden, self.final_norm))

    def forward(self, token: int, cache) -> list[int]:
        pos = cache["len"]
        if pos >= self.max_seq:
            raise DomainError("position beyond the RoPE table")
        hidden = self.run_layers(self.embed(token), cache, 0, len(self.layers), pos)
        cache["len"] = pos + 1
        return self.logits(hidden)

    def generate_v2(self, prompt, max_tokens, eos_tokens, repetition_penalty=True):
        """Generation v2: one BOS forward, prompt prefill, reuse final logits."""
        cache = self.new_cache()
        logits = self.forward(self.bos_token, cache)
        for token in prompt:
            logits = self.forward(token, cache)
        generated = []
        for _ in range(max_tokens):
            if repetition_penalty:
                nxt = repetition_penalty_select(logits, generated)
            else:
                nxt = argmax(logits)
            generated.append(nxt)
            if nxt in eos_tokens:
                break
            logits = self.forward(nxt, cache)
        return generated

    def generate_v1(self, prompt, max_tokens, eos_tokens):
        """Historical generation: the last prompt token is forwarded twice."""
        cache = self.new_cache()
        self.forward(self.bos_token, cache)
        for token in prompt:
            self.forward(token, cache)
        generated = []
        for _ in range(max_tokens):
            last = generated[-1] if generated else (prompt[-1] if prompt else 0)
            logits = self.forward(last, cache)
            nxt = repetition_penalty_select(logits, generated)
            generated.append(nxt)
            if nxt in eos_tokens:
                break
        return generated


def tokens_le_bytes(tokens: list[int]) -> bytes:
    return b"".join(int(t).to_bytes(4, "little") for t in tokens)


def i64_le_bytes(values: list[int]) -> bytes:
    return b"".join(int(v).to_bytes(8, "little", signed=True) for v in values)
