"""Emit operator-level known answers from the independent reference.

The committed model KAT does not pin every clause (see mutations.py): the
repetition penalty's multiplicity and window, argmax tie-breaking, the
interleaved-RoPE profile and generation v2 are never exercised there. These
vectors cover them one operator at a time. The Rust engine is then checked
against this file, so agreement is between two independent implementations.

    python3 -m arc_conformance.operator_vectors > integer_operator_kat.json
"""

from __future__ import annotations

import json
import math
import sys

from arc_conformance import integer_reference as ref
from arc_conformance import kat
from arc_conformance import preparation

ONE = ref.ONE
DEFAULT_PATH = kat.REPO / "crates/arc-inference/tests/fixtures/integer_operator_kat.json"


class Lcg:
    def __init__(self, seed: int):
        self.state = seed

    def next(self) -> int:
        self.state = (self.state * 6364136223846793005 + 1442695040888963407) & ((1 << 64) - 1)
        return self.state

    def between(self, lo: int, hi: int) -> int:
        return lo + (self.next() >> 11) % (hi - lo + 1)


def host_rope_tables(d_head: int, max_seq: int, base: float):
    half = d_head // 2
    cos, sin = [], []
    for pos in range(max_seq):
        for i in range(half):
            angle = pos * (1.0 / base ** (2.0 * i / d_head))
            cos.append(kat_round(math.cos(angle) * ONE))
            sin.append(kat_round(math.sin(angle) * ONE))
    return cos, sin


def kat_round(value: float) -> int:
    whole = math.floor(abs(value))
    if abs(value) - whole >= 0.5:
        whole += 1
    return int(whole) if value >= 0 else -int(whole)


def exp_cases(rng: Lcg):
    xs = [0, 1, -1, -2, -255, -256, -257, -ONE + 1, -ONE, -ONE - 1,
          -16 * ONE + 1, -16 * ONE, -16 * ONE - 1, -1_000_000, ref.tdiv(ref.I64_MIN, 2)]
    xs += [rng.between(-17 * ONE, ONE) for _ in range(48)]
    return [[x, ref.integer_exp(x)] for x in xs]


def isqrt_cases():
    xs = [-5, 0, 1, 2, 3, 4, 255, 256, 65535, ONE, ONE + 1, 2 * ONE, 128 * ONE,
          10 ** 6, 10 ** 9, 10 ** 12, 1 << 40, (1 << 62) + 12345]
    return [[x, ref.integer_isqrt(x)] for x in xs]


def rms_cases(rng: Lcg):
    cases = []
    shapes = [(1, 1), (7, 7), (7, 4), (64, 64), (5, 0)]
    for n, g in shapes:
        values = [rng.between(-3 * ONE, 3 * ONE) for _ in range(n)]
        gamma = [ONE + rng.between(-ONE // 2, ONE // 2) for _ in range(g)]
        cases.append({"input": values, "gamma": gamma})
    cases.append({"input": [0] * 8, "gamma": [ONE] * 8})
    cases.append({"input": [-1, 1, -2, 2], "gamma": [ONE] * 4})
    cases.append({"input": [5000 * ONE] + [3] * 15, "gamma": [ONE] * 16})
    for case in cases:
        case["output"] = ref.rms_norm(case["input"], case["gamma"])
    return cases


def silu_cases(rng: Lcg):
    xs = [0, 1, -1, 2, -2, ONE, -ONE, 16 * ONE, -16 * ONE, -16 * ONE - 1,
          20 * ONE, -20 * ONE, 1 << 40, -(1 << 40)]
    xs += [rng.between(-12 * ONE, 12 * ONE) for _ in range(40)]
    return [[x, ref.silu(x)] for x in xs]


def matmul_cases(rng: Lcg):
    cases = []
    for rows, cols in ((1, 1), (3, 16), (5, 33)):
        weights = [rng.between(-127, 127) for _ in range(rows * cols)]
        scales = [rng.between(1, 1 << 20) for _ in range(rows)]
        x = [rng.between(-40 * ONE, 40 * ONE) for _ in range(cols)]
        m = ref.Matrix(weights, scales, rows, cols)
        cases.append({"rows": rows, "cols": cols, "weights": weights,
                      "scales": scales, "input": x, "output": m.apply(x)})
    return cases


def rope_cases(rng: Lcg):
    d_head, max_seq = 8, 4
    cos, sin = host_rope_tables(d_head, max_seq, 10000.0)
    cases = []
    for layout, fn in (("split_half", ref.rope_split_half), ("interleaved", ref.rope_interleaved)):
        for pos in range(max_seq):
            v = [rng.between(-9 * ONE, 9 * ONE) for _ in range(d_head)]
            out = list(v)
            fn(out, pos, cos, sin)
            cases.append({"layout": layout, "pos": pos, "input": v, "output": out})
    return {"d_head": d_head, "cos": cos, "sin": sin, "cases": cases}


def attention_cases(rng: Lcg):
    cases = []
    d_head = 8

    def case(q, keys, values, scale=23170, note=""):
        cases.append({"note": note, "attn_scale": scale, "q": q, "keys": keys,
                      "values": values, "output": ref.attention_head(q, keys, values, scale)})

    for seq in (1, 2, 5):
        q = [rng.between(-4 * ONE, 4 * ONE) for _ in range(d_head)]
        keys = [[rng.between(-4 * ONE, 4 * ONE) for _ in range(d_head)] for _ in range(seq)]
        values = [[rng.between(-6 * ONE, 6 * ONE) for _ in range(d_head)] for _ in range(seq)]
        case(q, keys, values, note=f"random, {seq} positions")
    q = [ONE] * d_head
    rising = [[(j + 1) * ONE // 2] * d_head for j in range(5)]
    values = [[-(j + 1) * 3 * ONE + 7] * d_head for j in range(5)]
    case(q, rising, values, note="strictly rising scores rescale at every position")
    case(q, list(reversed(rising)), values, note="falling scores never rescale")
    case([0] * d_head, [[0] * d_head] * 3, [[-5, 5, -1, 1, 0, 3, -3, 2]] * 3,
         note="equal scores; negative values truncate toward zero")
    big = (1 << 31) + 5
    q = [big, 3, -2, 1, 0, 4, -1, 2]
    keys = [[7, (1 << 31) + 9, 1, -1, 2, 0, 3, -(1 << 31) - 7],
            [-3, 2, 5, 1, -(1 << 32) - 1, 1, 0, 1]]
    values = [[ONE, -ONE, 2 * ONE, 0, 5, -5, 9, -9], [3 * ONE, 1, -1, 7, -7, ONE, 0, 2]]
    case(q, keys, values, note="elements outside signed 32 bits; exact 64-bit products required")
    # Deviation D1's boundary: the aarch64 fast path narrows Q and K to 32 bits
    # only when every element lies in [-2^31, 2^31 - 1]. These sit exactly on
    # the bounds (the fast path, which must stay exact) and one past them (the
    # exact path). Each large element meets a small partner, so every product
    # stays far inside signed 64 bits.
    lo, hi = -(1 << 31), (1 << 31) - 1
    q = [hi, 3, -2, 1, 0, 4, -1, lo]
    keys = [[7, 1, lo, -1, 2, 0, 3, 1], [-3, 2, 5, 1, hi, 1, 0, -1]]
    case(q, keys, values, note="elements exactly at the signed 32-bit bounds (aarch64 fast path)")
    q = [hi + 1, 3, -2, 1, 0, 4, -1, lo - 1]
    keys = [[7, 1, lo - 1, -1, 2, 0, 3, 1], [-3, 2, 5, 1, hi + 1, 1, 0, -1]]
    case(q, keys, values, note="elements one past the signed 32-bit bounds (exact path)")
    case([0] * d_head, [[0] * d_head] * 2,
         [[-5, 5, -1, 1, 0, 3, -3, 2], [-4, 4, -2, 2, 1, 2, -2, 1]],
         note="equal scores, averages not whole: negative ones truncate toward zero")
    return cases


def penalty_cases():
    cases = [
        ("no history: a tie goes to the lowest index", [1, 9, 9, 3], []),
        ("a repeated token is penalised once per occurrence",
         [6 * ONE, 0, 9 * ONE // 2], [0, 0]),
        ("a positive logit is scaled by 5/6, truncated", [7, 1, 5], [0]),
        ("a negative logit is scaled by 6/5, truncated toward zero", [-7, -9, -8], [0]),
        ("zero is scaled as non-positive and stays zero", [0, -1], [0, 0, 0]),
        ("only the 64 most recent tokens count", [6 * ONE, 0, 11 * ONE // 2], [0] + [1] * 64),
        ("the 64th most recent token still counts", [6 * ONE, 0, 11 * ONE // 2], [0] + [1] * 63),
        ("tokens outside the vocabulary are ignored", [1, 2], [99, 5]),
    ]
    return [{"note": note, "logits": logits, "generated": history,
             "penalized": ref.repetition_penalty(logits, history),
             "token": ref.repetition_penalty_select(logits, history)}
            for note, logits, history in cases]


def argmax_cases():
    rows = [[3], [1, 1, 1], [-5, -2, -2, -9], [0, 7, 7, 1, 7], [ref.I64_MIN, ref.I64_MIN]]
    return [{"values": r, "index": ref.argmax(r)} for r in rows]


def permuted_split_half_k(model, row):
    """K as Rust stores it after rewriting GGUF adjacent pairs to split-half."""
    dh, half = model.d_head, model.d_head // 2
    out = []
    for h in range(len(row) // dh):
        head = row[h * dh:(h + 1) * dh]
        out.extend(head[0::2][:half])
        out.extend(head[1::2][:half])
    return out


def interleaved_v2_section(fixture):
    model, weight_hash = kat.build(fixture, ref.GGUF_INTERLEAVED)
    sequence = kat.run_sequence(model, fixture["sequence_tokens"])
    cache = model.new_cache()
    for token in fixture["sequence_tokens"]:
        model.forward(token, cache)
    rust_layout = {"k": [[permuted_split_half_k(model, r) for r in layer] for layer in cache["k"]],
                   "v": cache["v"], "len": cache["len"]}
    # With this prompt the penalty changes the sixth token, so both sampling
    # rules are pinned by the sequences themselves.
    prompt, max_tokens = [2, 7], 10
    free_run = model.generate_v2(prompt, max_tokens, [])
    eos = [free_run[2]]
    stopped = model.generate_v2(prompt, max_tokens, eos)
    greedy = model.generate_v2(prompt, max_tokens, [], repetition_penalty=False)
    blake = kat.blake3_hex
    return {
        "recipe": "integer_inference_kat.json weights; Q/K rows read as GGUF adjacent RoPE pairs",
        "profile": ref.GGUF_INTERLEAVED,
        "model_weight_hash_before_row_rewrite": weight_hash,
        "sequence_tokens": fixture["sequence_tokens"],
        "next_tokens": sequence["next_tokens"],
        "logits_hashes": sequence["logits_hashes"],
        "kv_cache_hash_split_half_layout": kat.cache_hash(model, rust_layout),
        "generation_v2": [
            {"prompt": prompt, "max_tokens": max_tokens, "eos_tokens": [],
             "repetition_penalty": True, "tokens": free_run,
             "output_hash": blake(ref.tokens_le_bytes(free_run))},
            {"prompt": prompt, "max_tokens": max_tokens, "eos_tokens": eos,
             "repetition_penalty": True, "tokens": stopped,
             "output_hash": blake(ref.tokens_le_bytes(stopped))},
            {"prompt": prompt, "max_tokens": max_tokens, "eos_tokens": [],
             "repetition_penalty": False, "tokens": greedy,
             "output_hash": blake(ref.tokens_le_bytes(greedy))},
        ],
    }


def build_document():
    rng = Lcg(0xA5C0_1D5E_0B5E_7715)
    fixture = json.loads(kat.DEFAULT_FIXTURE.read_text())
    return {
        "schema": 1,
        "name": "integer-operators-v1",
        "source": "scripts/arc_conformance (independent Python reference of "
                  "docs/protocol/integer-profile-contract-v1.md)",
        "frac_bits": ref.FRAC_BITS,
        "exp_lut_blake3": kat.blake3_hex(ref.i64_le_bytes(ref.EXP_LUT)),
        "integer_exp": exp_cases(rng),
        "integer_isqrt": isqrt_cases(),
        "rms_norm": rms_cases(rng),
        "silu": silu_cases(rng),
        "matmul_rows": matmul_cases(rng),
        "rope": rope_cases(rng),
        "attention_head": attention_cases(rng),
        "repetition_penalty": penalty_cases(),
        "argmax": argmax_cases(),
        "interleaved_generation_v2": interleaved_v2_section(fixture),
        "gguf_preparation": preparation.section(),
    }


def verify(document: dict) -> list[str]:
    """Recompute every vector with the current reference; list disagreements."""
    fresh = build_document()
    problems = []

    def walk(path, expected, actual):
        if isinstance(expected, dict):
            for key in expected:
                walk(f"{path}.{key}", expected[key], actual.get(key) if isinstance(actual, dict) else None)
        elif isinstance(expected, list) and expected and isinstance(expected[0], (dict, list)):
            if not isinstance(actual, list) or len(actual) != len(expected):
                problems.append(f"{path}: length differs")
                return
            for index, (e, a) in enumerate(zip(expected, actual)):
                walk(f"{path}[{index}]", e, a)
        elif expected != actual:
            problems.append(f"{path}: expected {expected!r}, got {actual!r}")

    walk("", document, fresh)
    return problems


def main() -> int:
    json.dump(build_document(), sys.stdout, indent=1)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
