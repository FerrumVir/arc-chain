"""Which clauses of the integer contract does the committed KAT actually pin?

Each mutation changes one clause of the reference executor. A mutation the
KAT still passes marks a clause that only operator-level vectors can check.

    python3 -m arc_conformance.mutations
"""

from __future__ import annotations

import contextlib
import json
import math
import sys

from arc_conformance import integer_reference as ref
from arc_conformance import kat
from arc_conformance import operator_vectors


@contextlib.contextmanager
def patched(obj, name, value):
    original = getattr(obj, name)
    setattr(obj, name, value)
    try:
        yield
    finally:
        setattr(obj, name, original)


def floor_div(a, b):
    return a // b


def trunc_shr(a, bits=ref.FRAC_BITS):
    return -((-a) >> bits) if a < 0 else a >> bits


def rounded_exp_lut():
    return [round(math.exp(-(4096 - i) * 16.0 / 4096.0) * ref.ONE) for i in range(4097)]


def isqrt_iterations(n):
    def f(x):
        if x <= 0:
            return ref.ONE * 100
        bits = x.bit_length() - 1
        y = ref.tdiv(ref.ONE * 256, 1 << ((bits + 1) // 2))
        for _ in range(n):
            y2 = ref.shr(y * y)
            xy2 = ref.shr(x * y2)
            y = ref.tdiv(y * (3 * ref.ONE - xy2), 2 * ref.ONE)
            if y <= 0:
                y = 1
        return y
    return f


def rms_norm_no_eps(values, gamma):
    n = len(values)
    mean_sq = (sum(v * v for v in values) // n) >> ref.FRAC_BITS
    inv = ref.integer_isqrt(max(mean_sq, 1))
    return [ref.shr(ref.shr(x * inv) * (gamma[i] if i < len(gamma) else ref.ONE))
            for i, x in enumerate(values)]


def two_pass_attention(q, keys, values, attn_scale):
    scores = [ref.shr(ref.shr(sum(a * b for a, b in zip(q, k))) * attn_scale) for k in keys]
    top = max(scores)
    weights = [ref.integer_exp(s - top) for s in scores]
    total = sum(weights)
    out = [0] * len(q)
    for w, v in zip(weights, values):
        out = [o + ref.shr(w * x) for o, x in zip(out, v)]
    return [ref.tdiv(o * ref.ONE, total) for o in out] if total > 0 else out


def wide_score_attention(q, keys, values, attn_scale):
    running_max, running_sum, out = ref.tdiv(ref.I64_MIN, 2), 0, [0] * len(q)
    for k, v in zip(keys, values):
        score = (sum(a * b for a, b in zip(q, k)) * attn_scale) >> 32
        if score > running_max:
            c = ref.integer_exp(running_max - score)
            running_sum = ref.shr(running_sum * c)
            out = [ref.shr(o * c) for o in out]
            running_max = score
        w = ref.integer_exp(score - running_max)
        running_sum += w
        out = [o + ref.shr(w * x) for o, x in zip(out, v)]
    return [ref.tdiv(o * ref.ONE, running_sum) for o in out] if running_sum > 0 else out


def rounded_silu(x):
    if x >= 0:
        sig = ref.tdiv(ref.ONE * ref.ONE, ref.ONE + ref.integer_exp(-x))
    else:
        e = ref.integer_exp(x)
        sig = ref.tdiv(e * ref.ONE, ref.ONE + e)
    return (x * sig + (1 << 15)) >> 16


def penalty_variant(window, once_per_token):
    def apply(logits, generated):
        logits = list(logits)
        recent = list(reversed(generated))
        recent = recent[:window] if window is not None else recent
        if once_per_token:
            recent = list(dict.fromkeys(recent))
        for token in recent:
            if 0 <= token < len(logits):
                v = logits[token]
                logits[token] = ref.tdiv(v * 5, 6) if v > 0 else ref.tdiv(v * 6, 5)
        return logits
    return apply


def last_index_argmax(values):
    best_idx, best_val = 0, None
    for i, v in enumerate(values):
        if best_val is None or v >= best_val:
            best_idx, best_val = i, v
    return best_idx


def mutations():
    yield "division floors instead of truncating", lambda: patched(ref, "tdiv", floor_div)
    yield "shift truncates instead of flooring", lambda: patched(ref, "shr", trunc_shr)
    yield "exp table is round(exp) rather than the recurrence", lambda: patched(ref, "EXP_LUT", rounded_exp_lut())
    yield "inverse sqrt uses 4 Newton steps", lambda: patched(ref, "integer_isqrt", isqrt_iterations(4))
    yield "inverse sqrt uses 6 Newton steps", lambda: patched(ref, "integer_isqrt", isqrt_iterations(6))
    yield "rms_norm has no +1 epsilon", lambda: patched(ref, "rms_norm", rms_norm_no_eps)
    yield "attention uses a two-pass softmax", lambda: patched(ref, "attention_head", two_pass_attention)
    yield "attention score keeps full dot precision", lambda: patched(ref, "attention_head", wide_score_attention)
    yield "silu rounds to nearest", lambda: patched(ref, "silu", rounded_silu)
    yield "repetition penalty applied once per distinct token", lambda: patched(ref, "repetition_penalty", penalty_variant(64, True))
    yield "repetition penalty looks at the whole history", lambda: patched(ref, "repetition_penalty", penalty_variant(None, False))
    yield "repetition penalty looks at 63 tokens", lambda: patched(ref, "repetition_penalty", penalty_variant(63, False))
    yield "argmax ties go to the highest index", lambda: patched(ref, "argmax", last_index_argmax)
    yield "query head h reads KV head h mod n_kv", lambda: patched(ref, "kv_head", lambda h, n, k: h % k)


def detected_by(check) -> str:
    try:
        return "yes" if check() else "no"
    except ref.DomainError:
        return "yes (domain error)"


def main() -> int:
    fixture = json.loads(kat.DEFAULT_FIXTURE.read_text())
    operators = json.loads(operator_vectors.DEFAULT_PATH.read_text()) \
        if operator_vectors.DEFAULT_PATH.exists() else operator_vectors.build_document()
    assert not kat.compare(fixture), "the unmutated reference must reproduce the KAT"
    assert not operator_vectors.verify(operators), "the unmutated reference must reproduce its vectors"
    undetected = []
    print(f"{'model KAT':18s}{'operator KAT':18s}clause mutated")
    for label, make in mutations():
        with make():
            model = detected_by(lambda: kat.compare(fixture))
            ops = detected_by(lambda: operator_vectors.verify(operators))
        print(f"{model:18s}{ops:18s}{label}")
        if model == "no" and ops == "no":
            undetected.append(label)
    # Profile swap: the same weights under interleaved RoPE must differ.
    model, _ = kat.build(fixture, ref.GGUF_INTERLEAVED)
    swapped = kat.run_sequence(model, fixture["sequence_tokens"])
    differs = swapped["logits_hashes"] != fixture["expected"]["logits_hashes"]
    print(f"{'yes' if differs else 'no':18s}{'-':18s}interleaved RoPE on legacy weights")
    print(f"{len(undetected)} mutated clause(s) pass both known-answer files")
    return 1 if undetected else 0


if __name__ == "__main__":
    sys.exit(main())
