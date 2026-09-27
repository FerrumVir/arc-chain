"""Model preparation (GGUF f32 tensors -> integer state), per contract §8.

A tiny Llama GGUF is described by a recipe of exactly-representable f32
values. This module prepares it the way the contract says: per-row INT8 in
f32, scales in f64, Q16 embeddings and norms, RoPE tables, the interleaved
row rewrite. It then digests the prepared state and runs it. The Rust test
writes the same GGUF with candle, loads it through the production loader, and
must reach the same digest, logits and tokens.
"""

from __future__ import annotations

import math
from decimal import Decimal

import numpy as np

from arc_conformance import integer_reference as ref
from arc_conformance import kat

SHAPE = {"vocab_size": 12, "d_model": 8, "n_heads": 2, "n_kv_heads": 1,
         "d_ff": 16, "n_layers": 1, "max_seq": 4096, "rope_base": 10000.0}
SEED = 0x7072_6570  # "prep"
MASK64 = (1 << 64) - 1


def nearest_f32(decimal_text: str) -> np.float32:
    target = Decimal(decimal_text)
    guess = np.float32(float(decimal_text))
    candidates = [np.nextafter(guess, np.float32(-np.inf)), guess,
                  np.nextafter(guess, np.float32(np.inf))]
    return min(candidates, key=lambda c: abs(Decimal(float(c)) - target))


F32_1E_10 = nearest_f32("1e-10")  # Rust's `1e-10` literal in f32 context


class RecipeRng:
    def __init__(self, seed: int):
        self.state = seed

    def next_u64(self) -> int:
        self.state = (self.state * 6364136223846793005 + 1442695040888963407) & MASK64
        return self.state

    def weight(self) -> np.float32:
        # k / 2^22 with |k| < 2^23: exact in f32, in [-2, 2).
        return np.float32(((self.next_u64() >> 40) - (1 << 23)) / float(1 << 22))

    def norm(self) -> np.float32:
        return np.float32(((self.next_u64() >> 43) - (1 << 20) + (1 << 22)) / float(1 << 22))


def recipe():
    """Tensors in the order both implementations draw them."""
    s = SHAPE
    d, vocab, d_ff = s["d_model"], s["vocab_size"], s["d_ff"]
    d_kv = d // s["n_heads"] * s["n_kv_heads"]
    rng = RecipeRng(SEED)
    tensors = []

    def matrix(name, rows, cols):
        tensors.append((name, rows, cols, [rng.weight() for _ in range(rows * cols)]))

    def norm(name, size):
        tensors.append((name, size, 1, [rng.norm() for _ in range(size)]))

    matrix("token_embd.weight", vocab, d)
    matrix("output.weight", vocab, d)
    norm("output_norm.weight", d)
    for layer in range(s["n_layers"]):
        p = f"blk.{layer}"
        matrix(f"{p}.attn_q.weight", d, d)
        matrix(f"{p}.attn_k.weight", d_kv, d)
        matrix(f"{p}.attn_v.weight", d_kv, d)
        matrix(f"{p}.attn_output.weight", d, d)
        matrix(f"{p}.ffn_gate.weight", d_ff, d)
        matrix(f"{p}.ffn_up.weight", d_ff, d)
        matrix(f"{p}.ffn_down.weight", d, d_ff)
        norm(f"{p}.attn_norm.weight", d)
        norm(f"{p}.ffn_norm.weight", d)
    # Token 0's embedding row is all zero: exercises the 1e-10 floor and the
    # minimum scale of 1.
    name, rows, cols, values = tensors[0]
    values[:cols] = [np.float32(0.0)] * cols
    return tensors


def round_half_away(value: float) -> int:
    whole = math.floor(abs(value))
    if abs(value) - whole >= 0.5:
        whole += 1
    return int(whole) if value >= 0 else -int(whole)


def quantize_rows(values, rows, cols):
    data, scales = [], []
    for r in range(rows):
        row = values[r * cols:(r + 1) * cols]
        abs_max = np.float32(0.0)
        for x in row:
            abs_max = max(abs_max, np.float32(abs(x)))
        abs_max = max(abs_max, F32_1E_10)
        inv = np.float32(127.0) / abs_max
        for x in row:
            y = np.float32(x * inv)
            data.append(max(-127, min(127, round_half_away(float(y)))))
        scales.append(max(round_half_away((float(abs_max) * 65536.0) / 127.0), 1))
    return data, scales


def q16_f64(values):
    return [round_half_away(float(x) * 65536.0) for x in values]


def q16_f32(values):
    return [round_half_away(float(np.float32(x * np.float32(65536.0)))) for x in values]


def rope_tables(d_head, max_seq, base):
    half = d_head // 2
    cos, sin = [], []
    for pos in range(max_seq):
        for i in range(half):
            angle = pos * (1.0 / base ** (2.0 * i / d_head))
            cos.append(round_half_away(math.cos(angle) * 65536.0))
            sin.append(round_half_away(math.sin(angle) * 65536.0))
    return cos, sin


def split_half_rows(data, scales, n_heads, d_head, cols):
    """The loader's interleaved-profile rewrite: [e0,o0,e1,o1] -> [e0,e1,o0,o1]."""
    half = d_head // 2
    out_data, out_scales = list(data), list(scales)
    for head in range(n_heads):
        base = head * d_head
        for pair in range(half):
            for dst, src in ((base + pair, base + 2 * pair),
                             (base + half + pair, base + 2 * pair + 1)):
                out_data[dst * cols:(dst + 1) * cols] = data[src * cols:(src + 1) * cols]
                out_scales[dst] = scales[src]
    return out_data, out_scales


def prepare():
    s = SHAPE
    d, n_heads, n_kv = s["d_model"], s["n_heads"], s["n_kv_heads"]
    d_head = d // n_heads
    tensors = {name: (rows, cols, values) for name, rows, cols, values in recipe()}

    def i8(name):
        rows, cols, values = tensors[name]
        return quantize_rows(values, rows, cols) + (rows, cols)

    def norm(name):
        return q16_f32(tensors[name][2])

    state = {
        "embedding_q16": q16_f64(tensors["token_embd.weight"][2]),
        "embedding_i8": i8("token_embd.weight"),
        "output": i8("output.weight"),
        "final_norm": norm("output_norm.weight"),
        "layers": [],
        "attn_scale": ref.integer_isqrt(d_head * ref.ONE),
    }
    state["rope_cos"], state["rope_sin"] = rope_tables(d_head, s["max_seq"], s["rope_base"])
    for layer in range(s["n_layers"]):
        p = f"blk.{layer}"
        state["layers"].append({
            "wq": i8(f"{p}.attn_q.weight"), "wk": i8(f"{p}.attn_k.weight"),
            "wv": i8(f"{p}.attn_v.weight"), "wo": i8(f"{p}.attn_output.weight"),
            "w_gate": i8(f"{p}.ffn_gate.weight"), "w_up": i8(f"{p}.ffn_up.weight"),
            "w_down": i8(f"{p}.ffn_down.weight"),
            "attn_norm": norm(f"{p}.attn_norm.weight"),
            "ffn_norm": norm(f"{p}.ffn_norm.weight"),
        })
    return state


def prepared_digest(state) -> str:
    """BLAKE3 of the prepared state in executed (split-half) row order.

    Order: u64 n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size,
    max_seq; i64 attn_scale; rope cos then sin; embedding_q16; embedding_i8,
    output, final_norm; then per layer wq, wk, wv, wo, w_gate, w_up, w_down,
    attn_norm, ffn_norm. A matrix is its INT8 bytes then its i64 scales; all
    integers little-endian.
    """
    s = SHAPE
    d_head = s["d_model"] // s["n_heads"]
    out = bytearray()
    for key in ("n_layers", "d_model", "n_heads", "n_kv_heads", "d_ff", "vocab_size", "max_seq"):
        out += s[key].to_bytes(8, "little")
    out += ref.i64_le_bytes([state["attn_scale"]])
    out += ref.i64_le_bytes(state["rope_cos"]) + ref.i64_le_bytes(state["rope_sin"])
    out += ref.i64_le_bytes(state["embedding_q16"])

    def matrix(m):
        data, scales = m[0], m[1]
        return bytes(w & 0xFF for w in data) + ref.i64_le_bytes(scales)

    out += matrix(state["embedding_i8"]) + matrix(state["output"])
    out += ref.i64_le_bytes(state["final_norm"])
    for layer in state["layers"]:
        wq_data, wq_scales, rows, cols = layer["wq"]
        wk_data, wk_scales, k_rows, k_cols = layer["wk"]
        out += matrix(split_half_rows(wq_data, wq_scales, s["n_heads"], d_head, cols))
        out += matrix(split_half_rows(wk_data, wk_scales, s["n_kv_heads"], d_head, k_cols))
        for key in ("wv", "wo", "w_gate", "w_up", "w_down"):
            out += matrix(layer[key])
        out += ref.i64_le_bytes(layer["attn_norm"]) + ref.i64_le_bytes(layer["ffn_norm"])
    return kat.blake3_hex(bytes(out))


def model(state) -> ref.Model:
    s = SHAPE
    layers = [ref.Layer(*(ref.Matrix(*layer[k]) for k in
                          ("wq", "wk", "wv", "wo", "w_gate", "w_up", "w_down")),
                        layer["attn_norm"], layer["ffn_norm"]) for layer in state["layers"]]
    return ref.Model(
        profile=ref.GGUF_INTERLEAVED, d_model=s["d_model"], n_heads=s["n_heads"],
        n_kv_heads=s["n_kv_heads"], d_ff=s["d_ff"], vocab_size=s["vocab_size"],
        max_seq=s["max_seq"], attn_scale=state["attn_scale"],
        rope_cos=state["rope_cos"], rope_sin=state["rope_sin"],
        embedding_q16=state["embedding_q16"], layers=layers,
        final_norm=state["final_norm"], output=ref.Matrix(*state["output"]), bos_token=1)


def section() -> dict:
    state = prepare()
    m = model(state)
    tokens = [1, 5, 9, 3, 11, 0]
    sequence = kat.run_sequence(m, tokens)
    prompt, max_tokens, eos = [5, 9, 3], 6, [2]
    generated = m.generate_v2(prompt, max_tokens, eos)
    return {
        "recipe": {"shape": SHAPE, "seed": SEED,
                   "values": "per tensor, in draw order: weights k/2^22 with k = (next >> 40) - 2^23; "
                             "norms (k + 2^22)/2^22 with k = (next >> 43) - 2^20; "
                             "token_embd row 0 then set to zero",
                   "tensor_order": [name for name, _, _, _ in recipe()]},
        "f32_1e_10_bits": int(np.float32(F32_1E_10).view(np.uint32)),
        "prepared_state_blake3": prepared_digest(state),
        "embedding_row0_scale": state["embedding_i8"][1][0],
        "attn_scale": state["attn_scale"],
        "sequence_tokens": tokens,
        "next_tokens": sequence["next_tokens"],
        "logits_hashes": sequence["logits_hashes"],
        "generation_v2": {"prompt": prompt, "max_tokens": max_tokens, "eos_tokens": eos,
                          "tokens": generated,
                          "output_hash": kat.blake3_hex(ref.tokens_le_bytes(generated))},
    }
