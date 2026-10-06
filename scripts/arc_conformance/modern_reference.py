"""Independent reference executor and preparer for arc.hf-llama.i8-dyadic-row.q16.v1.

Written only from docs/protocol/integer-profile-hf-llama-dyadic-v1.md ("the
spec"; section numbers below refer to it), never translated from the Rust
engine, so that bit-for-bit agreement between the two in CI is evidence that
the written profile is complete.

* Preparation (spec 4): BF16 safetensors + config.json + a pinned source
  manifest -> the integer package, byte for byte, with integer arithmetic only.
* Operators (spec 5) in plain Python ints, used by ``SlowEngine`` (one token
  at a time with a KV cache; tiny models and ``generate``).
* ``FastEngine``: an exact batched teacher-forcing forward for the real model.
  Integer matrix products go through float64 BLAS only when every partial sum
  is provably an integer below 2**53 (otherwise the operands are split into
  sign-magnitude limbs first); every other step is exact int64 arithmetic with
  explicit bound checks, and anything outside the proven bounds falls back to
  the Python-int operators. The results are therefore identical to the slow
  path, which the unit tests check position by position on a tiny model.
* Tables (spec 4.5, 5.1) are computed independently at high precision
  (``decimal`` and big-int fixed point with explicit rounding margins), not
  with the Q62 recurrences of spec 4.7.

Run from scripts/:  python3 -m arc_conformance.modern_reference --help
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import struct
import sys
from decimal import ROUND_FLOOR, ROUND_HALF_EVEN, Context, Decimal
from fractions import Fraction
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Sequence, Tuple

import numpy as np

try:  # blake3 is required for every digest; fail with a clear message at use.
    import blake3 as _blake3_module
except ImportError:  # pragma: no cover - exercised only without the package
    _blake3_module = None

# --------------------------------------------------------------------------
# Identities (spec 1) and formats (spec 3)

PROFILE_ID = "arc.hf-llama.i8-dyadic-row.q16.v1"
GENERATION_RP64_ID = "arc.hf-chat.no-bos.rp64-argmax.le-u32.v1"
GENERATION_ARGMAX_ID = "arc.hf-chat.no-bos.argmax.le-u32.v1"
PACKAGE_SCHEMA = "arc.integer-package.v1"
SOURCE_SCHEMA = "arc.hf-source.v1"
RUN_SCHEMA = "arc.modern-run.v1"
CASES_SCHEMA = "arc.modern-cases.v1"
GOLDEN_SCHEMA = "arc.modern-golden.v1"
IDENTITY_BLAKE3 = {
    PROFILE_ID: "3eb41a4fe376be020e2b93ff4ca10d55977d113278546ba2ee74622c3db1fa48",
    GENERATION_RP64_ID: "f267f6818464cfaed9afe66eec8b29efd59e7208dde72b711487e73756450fdb",
    GENERATION_ARGMAX_ID: "f04974ecfda896adb86c944b8a1dcd4c9060eec8193f8ecb6db3bac48c3b75a6",
}
SELECTION_GENERATION = {"rp64-argmax": GENERATION_RP64_ID, "argmax": GENERATION_ARGMAX_ID}

MAGIC = b"ARCIPKG1"
ALIGN = 64
ONE = 1 << 16
LIM62 = 1 << 62
I32_MIN = -(1 << 31)
I32_MAX = (1 << 31) - 1
I64_MAX = (1 << 63) - 1
MU_MIN = 1 << 30
MU_LIMIT = 1 << 31
K_MIN, K_MAX = 16, 62
ZERO_ROW_SCALE = (0, 16)

EXP_TABLE_BLAKE3 = "3586482438115a39e0e4b822f62451897f0b1b5baaf2e1019bbfb360a61bf0e2"
ROPE_SMOLLM3 = {"theta": 5_000_000, "d_head": 128, "max_seq": 4096}
ROPE_SMOLLM3_BLAKE3 = "2024b1037902e099975b3c1e9fb989fe5e0845761b381401e9f7f295a3c8ea1b"

DTYPE_NUMPY = {"i8": np.dtype("i1"), "u8": np.dtype("u1"),
               "i32": np.dtype("<i4"), "i64": np.dtype("<i8")}


class DomainError(ArithmeticError):
    """A value left the domain of spec 9; the computation is refused."""


class PreparationError(ValueError):
    """The BF16 source cannot be converted under spec 4 (refused)."""


class PackageError(ValueError):
    """A package file does not satisfy spec 4.8/4.9."""


class RunError(ValueError):
    """A cases/run document is malformed."""


# --------------------------------------------------------------------------
# Primitive operations (spec 3)

def tdiv(a: int, b: int) -> int:
    """Integer division truncating toward zero."""
    if b == 0:
        raise DomainError("division by zero")
    q = abs(a) // abs(b)
    return q if (a < 0) == (b < 0) else -q


def rha_ratio(num: int, den: int) -> int:
    """num/den rounded half away from zero (den > 0), exactly."""
    if den <= 0:
        raise ValueError("rha_ratio needs a positive denominator")
    q = (2 * abs(num) + den) // (2 * den)
    return -q if num < 0 else q


def check62(value: int, what: str) -> int:
    if value > LIM62 or value < -LIM62:
        raise DomainError(f"{what} = {value} exceeds 2^62 in magnitude")
    return value


def canonical_json(obj: Any) -> bytes:
    """Spec 4.9 canonical JSON bytes."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode("ascii")


def _blake3_ctor():
    if _blake3_module is None:
        raise RuntimeError("the 'blake3' package is required (pip install blake3)")
    return _blake3_module.blake3


def blake3_hex(data: bytes) -> str:
    return _blake3_ctor()(data).hexdigest()


def blake3_raw(data: bytes) -> bytes:
    return _blake3_ctor()(data).digest()


# --------------------------------------------------------------------------
# exp table (spec 5.1), computed as correctly rounded decimals

_EXP_TABLE: Optional[List[int]] = None
_EXP_TABLE_NP: Optional[np.ndarray] = None


def build_exp_table() -> List[int]:
    """T[i] = rha(e^(-(4096-i)/256) * 2^16), i in [0, 4096], from 60-digit decimals.

    Every entry's distance from a rounding boundary is checked to be far larger
    than the decimal error, so the rounding is provably correct.
    """
    ctx = Context(prec=60)
    scale = Decimal(ONE)
    half = Decimal(1) / Decimal(2)
    table = []
    for i in range(4097):
        x = Decimal(i - 4096) / Decimal(256)  # exact: 1/256 is a finite decimal
        v = ctx.multiply(ctx.exp(x), scale)
        n = int(v.to_integral_value(rounding=ROUND_FLOOR))
        frac = v - Decimal(n)
        if abs(frac - half) < Decimal("1e-40"):
            raise ArithmeticError(f"exp table entry {i} too close to a rounding boundary")
        table.append(n + 1 if frac > half else n)  # values are positive: half-up == half-away
    return table


def exp_table() -> List[int]:
    global _EXP_TABLE
    if _EXP_TABLE is None:
        _EXP_TABLE = build_exp_table()
    return _EXP_TABLE


def exp_table_np() -> np.ndarray:
    global _EXP_TABLE_NP
    if _EXP_TABLE_NP is None:
        _EXP_TABLE_NP = np.array(exp_table(), dtype=np.int64)
    return _EXP_TABLE_NP


def exp_table_blake3(table: Optional[Sequence[int]] = None) -> str:
    t = exp_table() if table is None else table
    return blake3_hex(np.array(t, dtype="<i8").tobytes())


def exp_q16(x: int) -> int:
    """Spec 5.1 for x <= 0 (Q16 in, Q16 out). x > 0 never occurs in the profile."""
    if x > 0:
        raise DomainError(f"exp argument {x} is positive")
    if x == 0:
        return ONE
    if x <= -16 * ONE:
        return 0
    t = exp_table()
    o = x + 16 * ONE
    i = o >> 8
    f = o & 255
    return t[i] + (((t[i + 1] - t[i]) * f) >> 8)


# --------------------------------------------------------------------------
# RoPE tables (spec 4.5), big-int fixed point with range reduction

_TABLE_BITS = 160  # fractional bits of the working precision
_TABLE_BLOCK = 64  # p = 64*a + b: cos/sin of 64*a*w and b*w evaluated directly


def _pi_fixed(bits: int) -> int:
    """pi * 2^bits (error below one unit) by Machin's formula."""
    guard = 32
    one = 1 << (bits + guard)

    def atan_inv(x: int) -> int:
        total, power, n, x2 = 0, one // x, 0, x * x
        while power:
            term = power // (2 * n + 1)
            total = total - term if n & 1 else total + term
            power //= x2
            n += 1
        return total

    return (4 * (4 * atan_inv(5) - atan_inv(239))) >> guard


def _sincos_fixed(x: int, bits: int, half_pi: int) -> Tuple[int, int]:
    """(cos x, sin x) * 2^bits for a fixed-point angle x >= 0 (reduced mod pi/2)."""
    n = (2 * x + half_pi) // (2 * half_pi)  # nearest multiple of pi/2
    r = x - n * half_pi  # |r| <= pi/4 (plus a few units)
    neg = r < 0
    ar = -r if neg else r
    r2 = (ar * ar) >> bits
    s = term = ar
    k = 1
    while True:
        term = ((term * r2) >> bits) // ((2 * k) * (2 * k + 1))
        if not term:
            break
        s = s - term if k & 1 else s + term
        k += 1
    c = term = 1 << bits
    k = 1
    while True:
        term = ((term * r2) >> bits) // ((2 * k - 1) * (2 * k))
        if not term:
            break
        c = c - term if k & 1 else c + term
        k += 1
    if neg:
        s = -s
    quadrant = n & 3
    if quadrant == 0:
        return c, s
    if quadrant == 1:
        return -s, c
    if quadrant == 2:
        return -c, -s
    return s, -c


def _omegas_fixed(theta: int, d_head: int, bits: int) -> List[int]:
    """w_i = theta^(-2i/D) * 2^bits from 110-digit decimal ln/exp (correctly rounded)."""
    ctx = Context(prec=110)
    ln_theta = ctx.ln(Decimal(theta))
    scale = Decimal(1 << bits)
    out = []
    for i in range(d_head // 2):
        w = ctx.exp(ctx.divide(ctx.multiply(ln_theta, Decimal(-2 * i)), Decimal(d_head)))
        out.append(int(ctx.multiply(w, scale).to_integral_value(rounding=ROUND_HALF_EVEN)))
    return out


def build_rope_tables(theta: int, d_head: int, max_seq: int) -> Tuple[np.ndarray, np.ndarray]:
    """(cos, sin) as int32 arrays [max_seq, d_head/2], every entry correctly rounded.

    cos(p*w) and sin(p*w) come from the angle-addition formula applied to two
    directly evaluated angles (64a*w and b*w, p = 64a + b), each reduced mod
    pi/2 and summed by Taylor series at 160 fractional bits. The absolute
    error of every product is below 2^(160+8) units of 2^-320; an entry is
    accepted only when its distance from the rounding boundary exceeds
    2^(160+24) units, so every rounding is provably the exact one.
    """
    if isinstance(theta, bool) or not isinstance(theta, int) or theta < 1:
        raise PreparationError(f"rope_theta must be a positive integer, got {theta!r}")
    if d_head < 2 or d_head % 2:
        raise PreparationError(f"head width {d_head} is not a positive even number")
    if max_seq < 1:
        raise PreparationError("max_seq must be positive")
    bits = _TABLE_BITS
    half_pi = _pi_fixed(bits) >> 1
    shift = 2 * bits - 16
    half = 1 << (shift - 1)
    mask = (1 << shift) - 1
    margin = 1 << (bits + 24)
    blk = _TABLE_BLOCK
    n_hi = (max_seq + blk - 1) // blk
    cos_cols: List[List[int]] = []
    sin_cols: List[List[int]] = []
    for w in _omegas_fixed(theta, d_head, bits):
        hi = [_sincos_fixed(blk * a * w, bits, half_pi) for a in range(n_hi)]
        lo = [_sincos_fixed(b * w, bits, half_pi) for b in range(blk)]
        col_c: List[int] = []
        col_s: List[int] = []
        for p in range(max_seq):
            ca, sa = hi[p // blk]
            cb, sb = lo[p % blk]
            for value, col in ((ca * cb - sa * sb, col_c), (sa * cb + ca * sb, col_s)):
                mag = -value if value < 0 else value
                rem = mag & mask
                if abs(rem - half) <= margin:
                    raise ArithmeticError("RoPE entry too close to a rounding boundary")
                q = (mag + half) >> shift
                col.append(-q if value < 0 else q)
        cos_cols.append(col_c)
        sin_cols.append(col_s)
    cos = np.ascontiguousarray(np.array(cos_cols, dtype=np.int64).T).astype(np.int32)
    sin = np.ascontiguousarray(np.array(sin_cols, dtype=np.int64).T).astype(np.int32)
    return cos, sin


def rope_tables_blake3(cos: np.ndarray, sin: np.ndarray) -> str:
    """BLAKE3 of cos (row-major LE i32) followed by sin (spec 4.5)."""
    return blake3_hex(np.ascontiguousarray(cos, dtype="<i4").tobytes()
                      + np.ascontiguousarray(sin, dtype="<i4").tobytes())


# --------------------------------------------------------------------------
# BF16 (spec 4.1)

def bf16_parts(bits: int) -> Tuple[int, int, int]:
    """(s, M, e) with value (-1)^s * M * 2^e; infinity/NaN refused."""
    if not 0 <= bits <= 0xFFFF:
        raise PreparationError(f"{bits} is not a 16-bit pattern")
    s = bits >> 15
    big_e = (bits >> 7) & 0xFF
    m = bits & 0x7F
    if big_e == 255:
        raise PreparationError(f"BF16 pattern {bits:#06x} is infinity or NaN")
    if big_e == 0:
        return s, m, -133
    return s, 128 + m, big_e - 134


def bf16_value(bits: int) -> Fraction:
    """Exact rational value of a BF16 pattern."""
    s, mant, e = bf16_parts(bits)
    v = Fraction(mant) * (Fraction(2) ** e)
    return -v if s else v


# --------------------------------------------------------------------------
# Per-row INT8 with a dyadic scale (spec 4.2)

def quantize_row_exact(bits: Sequence[int]) -> Tuple[List[int], int, int]:
    """Spec 4.2 for one row, literally, in Python ints (the test oracle)."""
    parts = [bf16_parts(b) for b in bits]
    mags = [b & 0x7FFF for b in bits]
    if not parts or max(mags) == 0:
        return [0] * len(bits), 0, 16
    j_max = mags.index(max(mags))
    _, m_a, e_a = parts[j_max]
    q = []
    for s, m_j, e_j in parts:
        if m_j == 0:
            q.append(0)
            continue
        d = e_a - e_j
        if d < 0:
            raise AssertionError("an element exceeds the row maximum")
        if d > 60:
            q.append(0)
            continue
        val = (2 * 127 * m_j + m_a * (1 << d)) // (2 * m_a * (1 << d))
        q.append(-val if s else val)
    t = 0
    while m_a * (1 << t) < 127 * (1 << 30):
        t += 1
    mu = (2 * m_a * (1 << t) + 127) // 254
    if mu == 1 << 31:
        mu, t = 1 << 30, t - 1
    k = t - e_a
    if k < K_MIN or k > K_MAX:
        raise PreparationError(f"row scale exponent k = {k} outside [16, 62]")
    return q, mu, k


def _scale_lut() -> Tuple[np.ndarray, np.ndarray]:
    """(mu, t) for every row-maximum significand M_A in [0, 255] (index 0 unused)."""
    mus = np.zeros(256, dtype=np.int64)
    ts = np.zeros(256, dtype=np.int64)
    for m_a in range(1, 256):
        t = 0
        while m_a << t < 127 << 30:
            t += 1
        mu = (2 * (m_a << t) + 127) // 254
        if mu == 1 << 31:
            mu, t = 1 << 30, t - 1
        mus[m_a], ts[m_a] = mu, t
    return mus, ts


_SCALE_MU, _SCALE_T = _scale_lut()


def quantize_rows(bits: np.ndarray, what: str = "tensor") -> Tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Vectorised spec 4.2 on a [rows, cols] array of BF16 patterns.

    Returns (q int8 [rows, cols], mu int32 [rows], k uint8 [rows]). For an
    element with d >= 16 the quotient is 0 whatever d is (127*M_j <= 32385 <
    2^15 <= M_A*2^16 / 2), so d is clamped to 20 before shifting: this keeps
    every intermediate below 2^29 and gives the same q as the literal formula,
    including the d > 60 rule.
    """
    b = np.asarray(bits).astype(np.int64)
    if b.ndim != 2:
        raise PreparationError(f"{what}: expected a matrix")
    rows = b.shape[0]
    if b.size and np.any((b & 0x7F80) == 0x7F80):
        raise PreparationError(f"{what}: contains infinity or NaN")
    big_e = (b >> 7) & 0xFF
    m = b & 0x7F
    mant = np.where(big_e == 0, m, m + 128)
    expo = np.where(big_e == 0, -133, big_e - 134)
    mags = b & 0x7FFF
    j_max = np.argmax(mags, axis=1) if b.shape[1] else np.zeros(rows, dtype=np.int64)
    r_idx = np.arange(rows)
    zero_row = (mags[r_idx, j_max] == 0) if b.shape[1] else np.ones(rows, dtype=bool)
    m_a = np.where(zero_row, 1, mant[r_idx, j_max] if b.shape[1] else 1)
    e_a = np.where(zero_row, -133, expo[r_idx, j_max] if b.shape[1] else -133)
    d = e_a[:, None] - expo
    if np.any(d[mant > 0] < 0):
        raise AssertionError("an element exceeds the row maximum")
    dc = np.minimum(np.maximum(d, 0), 20)
    scaled_a = m_a[:, None] << dc
    q = (254 * mant + scaled_a) // (2 * scaled_a)
    q = np.where((mant == 0) | (d > 60), 0, q)
    q = np.where((b >> 15) & 1 == 1, -q, q)
    q[zero_row] = 0
    mu = _SCALE_MU[m_a]
    k = _SCALE_T[m_a] - e_a
    mu = np.where(zero_row, 0, mu)
    k = np.where(zero_row, 16, k)
    bad = (~zero_row) & ((k < K_MIN) | (k > K_MAX))
    if np.any(bad):
        row = int(np.nonzero(bad)[0][0])
        raise PreparationError(f"{what}: row {row} has scale exponent k = {int(k[row])} outside [16, 62]")
    return q.astype(np.int8), mu.astype(np.int32), k.astype(np.uint8)


# --------------------------------------------------------------------------
# Norm gains (spec 4.3), epsilon (spec 4.4)

def norm_gain(bits: int) -> int:
    """g = rha(v * 2^16), exactly; refused when it does not fit the i64 storage."""
    s, mant, e = bf16_parts(bits)
    if e + 16 >= 0:
        g = mant << (e + 16)
    else:
        c = -(e + 16)
        g = (2 * mant + (1 << c)) >> (c + 1)
    if g > I64_MAX:
        raise PreparationError(f"norm gain from {bits:#06x} does not fit in i64")
    return -g if s else g


def rms_eps_q32(eps: Any) -> int:
    """rha(eps * 2^32) for eps read from JSON as the nearest double."""
    if isinstance(eps, bool) or not isinstance(eps, (int, float)):
        raise PreparationError(f"rms_norm_eps must be a number, got {eps!r}")
    if isinstance(eps, float) and not math.isfinite(eps):
        raise PreparationError("rms_norm_eps is not finite")
    value = Fraction(eps) * (1 << 32)
    out = rha_ratio(value.numerator, value.denominator)
    if out < 1:
        raise PreparationError(f"rms_norm_eps gives eps_q32 = {out} < 1")
    return out


# --------------------------------------------------------------------------
# Model shape (spec 4.6) and package layout (spec 4.8)

def _cfg_int(config: Dict[str, Any], key: str, minimum: int = 1) -> int:
    if key not in config:
        raise PreparationError(f"config.json lacks {key!r}")
    v = config[key]
    if isinstance(v, bool) or not isinstance(v, int) or v < minimum:
        raise PreparationError(f"config.json {key!r} = {v!r} is not an integer >= {minimum}")
    return v


def model_from_config(config: Dict[str, Any], max_seq: int) -> Dict[str, Any]:
    """The package header's "model" object; unsupported configurations are refused."""
    def refuse(msg: str) -> None:
        raise PreparationError(f"unsupported config.json: {msg}")

    if config.get("hidden_act") != "silu":
        refuse(f"hidden_act = {config.get('hidden_act')!r}, need 'silu'")
    if config.get("rope_scaling") is not None:
        refuse("rope_scaling must be null")
    if config.get("attention_bias", False) is not False:
        refuse("attention_bias must be false")
    if config.get("mlp_bias", False) is not False:
        refuse("mlp_bias must be false")
    if config.get("tie_word_embeddings", True) is not True:
        refuse("tie_word_embeddings must be true")
    if config.get("use_sliding_window", False) is not False:
        refuse("use_sliding_window must be false")
    if config.get("sliding_window") is not None and config.get("use_sliding_window") is not False:
        refuse("sliding_window is set without use_sliding_window = false")
    layer_types = config.get("layer_types")
    if layer_types is not None and any(t != "full_attention" for t in layer_types):
        refuse("layer_types must all be 'full_attention'")
    arch = config.get("model_type")
    if not isinstance(arch, str) or not arch:
        refuse("model_type must be a non-empty string")
    n_layers = _cfg_int(config, "num_hidden_layers")
    d_model = _cfg_int(config, "hidden_size")
    n_heads = _cfg_int(config, "num_attention_heads")
    n_kv = _cfg_int(config, "num_key_value_heads") if "num_key_value_heads" in config else n_heads
    if config.get("head_dim") is not None:
        d_head = _cfg_int(config, "head_dim")
    else:
        if d_model % n_heads:
            refuse("hidden_size is not a multiple of num_attention_heads")
        d_head = d_model // n_heads
    if d_model != n_heads * d_head:
        refuse("hidden_size != num_attention_heads * head_dim")
    if d_head % 2:
        refuse("head_dim must be even (split-half RoPE)")
    if n_kv > n_heads or n_heads % n_kv:
        refuse("num_attention_heads must be a multiple of num_key_value_heads")
    d_ff = _cfg_int(config, "intermediate_size")
    vocab = _cfg_int(config, "vocab_size")
    eps_q32 = rms_eps_q32(config.get("rms_norm_eps"))
    theta = config.get("rope_theta")
    if isinstance(theta, bool) or not isinstance(theta, (int, float)):
        refuse(f"rope_theta = {theta!r}")
    if isinstance(theta, float) and not (math.isfinite(theta) and theta.is_integer()):
        refuse(f"rope_theta = {theta!r} is not an integer")
    theta = int(theta)
    if theta < 1:
        refuse("rope_theta must be positive")
    nope = config.get("no_rope_layers")
    if not isinstance(nope, list) or len(nope) != n_layers or any(
            isinstance(v, bool) or v not in (0, 1) for v in nope):
        refuse("no_rope_layers must list 0/1 for every layer")
    if isinstance(max_seq, bool) or not isinstance(max_seq, int) or max_seq < 1:
        raise PreparationError(f"max_seq = {max_seq!r} is not a positive integer")
    return {
        "architecture": arch, "n_layers": n_layers, "d_model": d_model, "n_heads": n_heads,
        "n_kv_heads": n_kv, "d_head": d_head, "d_ff": d_ff, "vocab_size": vocab,
        "max_seq": max_seq, "rms_eps_q32": eps_q32, "rope_theta": theta,
        "rope_layers": [int(v) for v in nope], "tied_embeddings": True,
    }


_PROJECTIONS = (("wq", "self_attn.q_proj"), ("wk", "self_attn.k_proj"),
                ("wv", "self_attn.v_proj"), ("wo", "self_attn.o_proj"))
_FFN = (("w_gate", "mlp.gate_proj"), ("w_up", "mlp.up_proj"), ("w_down", "mlp.down_proj"))


def projection_shapes(model: Dict[str, Any]) -> Dict[str, Tuple[int, int]]:
    d, dh = model["d_model"], model["d_head"]
    hq, hk, f = model["n_heads"] * dh, model["n_kv_heads"] * dh, model["d_ff"]
    return {"wq": (hq, d), "wk": (hk, d), "wv": (hk, d), "wo": (d, hq),
            "w_gate": (f, d), "w_up": (f, d), "w_down": (d, f)}


def package_layout(model: Dict[str, Any]) -> List[Dict[str, Any]]:
    """Tensor entries in file order: name, dtype, shape, kind, source tensor."""
    v, d = model["vocab_size"], model["d_model"]
    s, h = model["max_seq"], model["d_head"] // 2
    shapes = projection_shapes(model)
    out: List[Dict[str, Any]] = []

    def scaled(name: str, rows: int, cols: int, source: str) -> None:
        out.append({"name": name + ".q", "dtype": "i8", "shape": [rows, cols], "kind": "q", "source": source})
        out.append({"name": name + ".mu", "dtype": "i32", "shape": [rows], "kind": "mu", "source": source})
        out.append({"name": name + ".k", "dtype": "u8", "shape": [rows], "kind": "k", "source": source})

    scaled("embed", v, d, "model.embed_tokens.weight")
    out.append({"name": "final_norm", "dtype": "i64", "shape": [d], "kind": "norm", "source": "model.norm.weight"})
    out.append({"name": "rope.cos", "dtype": "i32", "shape": [s, h], "kind": "rope", "source": None})
    out.append({"name": "rope.sin", "dtype": "i32", "shape": [s, h], "kind": "rope", "source": None})
    for layer in range(model["n_layers"]):
        src = f"model.layers.{layer}."
        dst = f"layers.{layer}."
        out.append({"name": dst + "attn_norm", "dtype": "i64", "shape": [d], "kind": "norm",
                    "source": src + "input_layernorm.weight"})
        for short, hf in _PROJECTIONS:
            scaled(dst + short, *shapes[short], src + hf + ".weight")
        out.append({"name": dst + "ffn_norm", "dtype": "i64", "shape": [d], "kind": "norm",
                    "source": src + "post_attention_layernorm.weight"})
        for short, hf in _FFN:
            scaled(dst + short, *shapes[short], src + hf + ".weight")
    return out


def source_tensor_shapes(model: Dict[str, Any]) -> Dict[str, Tuple[int, ...]]:
    """The exact BF16 tensor set (name -> shape) spec 4.8 requires of the source."""
    out: Dict[str, Tuple[int, ...]] = {}
    for entry in package_layout(model):
        if entry["source"] and entry["kind"] in ("q", "norm"):
            out[entry["source"]] = tuple(entry["shape"])
    return out


def _align(n: int) -> int:
    return (n + ALIGN - 1) // ALIGN * ALIGN


def header_tensors(model: Dict[str, Any]) -> List[Dict[str, Any]]:
    entries, offset = [], 0
    for e in package_layout(model):
        nbytes = int(np.prod(e["shape"], dtype=np.int64)) * DTYPE_NUMPY[e["dtype"]].itemsize
        entries.append({"name": e["name"], "dtype": e["dtype"], "shape": list(e["shape"]),
                        "offset": offset, "bytes": nbytes})
        offset = _align(offset + nbytes)
    return entries


def build_header(model: Dict[str, Any], source: Dict[str, Any]) -> Dict[str, Any]:
    return {"schema": PACKAGE_SCHEMA, "profile": PROFILE_ID, "model": model,
            "source": source, "tensors": header_tensors(model)}


# --------------------------------------------------------------------------
# Source files: manifest verification and a minimal safetensors reader

def sha256_file(path: Path, chunk: int = 1 << 20) -> Tuple[int, str]:
    h = hashlib.sha256()
    size = 0
    with open(path, "rb") as f:
        while True:
            block = f.read(chunk)
            if not block:
                break
            size += len(block)
            h.update(block)
    return size, h.hexdigest()


def load_source_manifest(path: Path) -> Dict[str, Any]:
    manifest = json.loads(Path(path).read_bytes())
    if not isinstance(manifest, dict):
        raise PreparationError("source manifest is not a JSON object")
    for key in ("repo", "revision", "max_seq", "files"):
        if key not in manifest:
            raise PreparationError(f"source manifest lacks {key!r}")
    files = manifest["files"]
    if not isinstance(files, list) or not files:
        raise PreparationError("source manifest 'files' must be a non-empty list")
    names = []
    for entry in files:
        name = entry.get("name") if isinstance(entry, dict) else None
        if (not isinstance(name, str) or not name or "/" in name or "\\" in name
                or name in (".", "..")):
            raise PreparationError(f"bad file entry in source manifest: {entry!r}")
        size, digest = entry.get("bytes"), entry.get("sha256")
        if isinstance(size, bool) or not isinstance(size, int) or size < 0:
            raise PreparationError(f"{name}: bad byte length {size!r}")
        if not isinstance(digest, str) or len(digest) != 64 or digest != digest.lower() or \
                any(c not in "0123456789abcdef" for c in digest):
            raise PreparationError(f"{name}: bad sha256 {digest!r}")
        names.append(name)
    if len(set(names)) != len(names):
        raise PreparationError("source manifest lists a file twice")
    if "config.json" not in names:
        raise PreparationError("source manifest does not list config.json")
    if not any(n.endswith(".safetensors") for n in names):
        raise PreparationError("source manifest lists no safetensors shard")
    return manifest


def verify_source_files(source_dir: Path, manifest: Dict[str, Any]) -> None:
    """Refuse unless every listed file has exactly the pinned length and SHA-256."""
    for entry in manifest["files"]:
        path = Path(source_dir) / entry["name"]
        if not path.is_file():
            raise PreparationError(f"{entry['name']}: missing from {source_dir}")
        size = path.stat().st_size
        if size != entry["bytes"]:
            raise PreparationError(f"{entry['name']}: {size} bytes, manifest pins {entry['bytes']}")
        _, digest = sha256_file(path)
        if digest != entry["sha256"]:
            raise PreparationError(f"{entry['name']}: sha256 {digest} != pinned {entry['sha256']}")


class SafetensorsShard:
    """8-byte LE header length, JSON header, then the data region."""

    def __init__(self, path: Path):
        self.path = Path(path)
        size = self.path.stat().st_size
        with open(self.path, "rb") as f:
            raw = f.read(8)
            if len(raw) != 8:
                raise PreparationError(f"{self.path.name}: truncated safetensors header")
            (n,) = struct.unpack("<Q", raw)
            if n > size - 8:
                raise PreparationError(f"{self.path.name}: header length {n} exceeds the file")
            header = json.loads(f.read(n))
        if not isinstance(header, dict):
            raise PreparationError(f"{self.path.name}: header is not an object")
        self.data_start = 8 + n
        self.tensors: Dict[str, Dict[str, Any]] = {}
        for name, info in header.items():
            if name == "__metadata__":
                continue
            if not isinstance(info, dict):
                raise PreparationError(f"{self.path.name}: bad entry for {name}")
            shape, offs = info.get("shape"), info.get("data_offsets")
            if (not isinstance(shape, list) or any(isinstance(x, bool) or not isinstance(x, int) or x < 0
                                                   for x in shape)
                    or not isinstance(offs, list) or len(offs) != 2):
                raise PreparationError(f"{self.path.name}: bad shape/offsets for {name}")
            begin, end = offs
            if not (isinstance(begin, int) and isinstance(end, int) and 0 <= begin <= end
                    and self.data_start + end <= size):
                raise PreparationError(f"{self.path.name}: {name} lies outside the file")
            self.tensors[name] = {"dtype": info.get("dtype"), "shape": tuple(shape),
                                  "begin": begin, "end": end}

    def bf16(self, name: str) -> np.ndarray:
        info = self.tensors[name]
        if info["dtype"] != "BF16":
            raise PreparationError(f"{name}: dtype {info['dtype']!r}, need BF16")
        count = int(np.prod(info["shape"], dtype=np.int64))
        if info["end"] - info["begin"] != 2 * count:
            raise PreparationError(f"{name}: byte range does not match its shape")
        if count == 0:
            return np.zeros(info["shape"], dtype="<u2")
        return np.memmap(self.path, dtype="<u2", mode="r",
                         offset=self.data_start + info["begin"], shape=info["shape"])


def open_source_tensors(source_dir: Path, manifest: Dict[str, Any],
                        model: Dict[str, Any]) -> Dict[str, Tuple[SafetensorsShard, str]]:
    """Map every required tensor to its shard; refuse missing/extra/misshapen tensors."""
    expected = source_tensor_shapes(model)
    found: Dict[str, Tuple[SafetensorsShard, str]] = {}
    for entry in manifest["files"]:
        if not entry["name"].endswith(".safetensors"):
            continue
        shard = SafetensorsShard(Path(source_dir) / entry["name"])
        for name, info in shard.tensors.items():
            if name in found:
                raise PreparationError(f"tensor {name} appears in more than one shard")
            if name not in expected:
                raise PreparationError(f"unexpected source tensor {name}")
            if info["dtype"] != "BF16":
                raise PreparationError(f"{name}: dtype {info['dtype']!r}, need BF16")
            if info["shape"] != expected[name]:
                raise PreparationError(f"{name}: shape {list(info['shape'])}, need {list(expected[name])}")
            found[name] = (shard, name)
    missing = sorted(set(expected) - set(found))
    if missing:
        raise PreparationError(f"source lacks {len(missing)} tensor(s), e.g. {missing[0]}")
    return found


# --------------------------------------------------------------------------
# Package writer (spec 4.8, 4.9)

class _HashingWriter:
    def __init__(self, f):
        self.f = f
        self.sha = hashlib.sha256()
        self.b3 = _blake3_ctor()()
        self.size = 0

    def write(self, data: bytes) -> None:
        if data:
            self.f.write(data)
            self.sha.update(data)
            self.b3.update(data)
            self.size += len(data)

    def pad(self) -> None:
        self.write(b"\x00" * (_align(self.size) - self.size))


def _chunk_rows(cols: int) -> int:
    return max(1, (1 << 20) // max(1, cols))


def prepare_package(source_dir: Path, manifest_path: Path, out_path: Path) -> Dict[str, Any]:
    """Verify the pinned source files, convert them, stream the package; return its identity."""
    source_dir, out_path = Path(source_dir), Path(out_path)
    manifest = load_source_manifest(Path(manifest_path))
    verify_source_files(source_dir, manifest)
    config = json.loads((source_dir / "config.json").read_bytes())
    if not isinstance(config, dict):
        raise PreparationError("config.json is not an object")
    model = model_from_config(config, manifest["max_seq"])
    tensors = open_source_tensors(source_dir, manifest, model)
    source = {"repo": manifest["repo"], "revision": manifest["revision"],
              "files": [{"name": e["name"], "bytes": e["bytes"], "sha256": e["sha256"]}
                        for e in manifest["files"]]}
    header = build_header(model, source)
    header_bytes = canonical_json(header)
    layout = package_layout(model)
    tmp = out_path.with_name(out_path.name + ".partial")
    with open(tmp, "wb") as f:
        w = _HashingWriter(f)
        w.write(MAGIC)
        w.write(struct.pack("<Q", len(header_bytes)))
        w.write(header_bytes)
        w.pad()
        data_start = w.size
        pending: Dict[str, np.ndarray] = {}
        rope: Optional[Tuple[np.ndarray, np.ndarray]] = None
        for entry, hdr in zip(layout, header["tensors"]):
            if w.size - data_start != hdr["offset"]:
                raise AssertionError("layout drift")
            kind = entry["kind"]
            if kind == "q":
                shard, name = tensors[entry["source"]]
                src = shard.bf16(name)
                rows, cols = src.shape
                mus, ks = [], []
                step = _chunk_rows(cols)
                for r0 in range(0, rows, step):
                    q, mu, k = quantize_rows(np.asarray(src[r0:r0 + step]), what=name)
                    w.write(q.tobytes())
                    mus.append(mu)
                    ks.append(k)
                pending["mu"] = np.concatenate(mus) if mus else np.zeros(0, np.int32)
                pending["k"] = np.concatenate(ks) if ks else np.zeros(0, np.uint8)
            elif kind == "mu":
                w.write(pending.pop("mu").astype("<i4").tobytes())
            elif kind == "k":
                w.write(pending.pop("k").astype("u1").tobytes())
            elif kind == "norm":
                shard, name = tensors[entry["source"]]
                gains = [norm_gain(int(b)) for b in np.asarray(shard.bf16(name)).tolist()]
                w.write(np.array(gains, dtype="<i8").tobytes())
            elif kind == "rope":
                if rope is None:
                    rope = build_rope_tables(model["rope_theta"], model["d_head"], model["max_seq"])
                table = rope[0] if entry["name"] == "rope.cos" else rope[1]
                w.write(table.astype("<i4").tobytes())
            else:  # pragma: no cover
                raise AssertionError(kind)
            if w.size - data_start - hdr["offset"] != hdr["bytes"]:
                raise AssertionError(f"{entry['name']}: wrote the wrong number of bytes")
            w.pad()
        size, sha, b3 = w.size, w.sha.hexdigest(), w.b3.hexdigest()
    os.replace(tmp, out_path)
    return {"bytes": size, "sha256": sha, "blake3": b3}


# --------------------------------------------------------------------------
# Package reader (spec 4.8, 4.9, value ranges of spec 3)

_MODEL_KEYS = ("architecture", "n_layers", "d_model", "n_heads", "n_kv_heads", "d_head", "d_ff",
               "vocab_size", "max_seq", "rms_eps_q32", "rope_theta", "rope_layers", "tied_embeddings")


def _reject_float(text: str) -> Any:
    raise PackageError(f"floating point number {text} in the package header")


def _validate_model(model: Any) -> Dict[str, Any]:
    if not isinstance(model, dict) or sorted(model) != sorted(_MODEL_KEYS):
        raise PackageError("header 'model' has the wrong fields")
    for key in _MODEL_KEYS:
        if key in ("architecture", "rope_layers", "tied_embeddings"):
            continue
        v = model[key]
        if isinstance(v, bool) or not isinstance(v, int) or v < 1:
            raise PackageError(f"model.{key} = {v!r} is not a positive integer")
    if not isinstance(model["architecture"], str) or not model["architecture"]:
        raise PackageError("model.architecture must be a non-empty string")
    if model["tied_embeddings"] is not True:
        raise PackageError("model.tied_embeddings must be true")
    rl = model["rope_layers"]
    if not isinstance(rl, list) or len(rl) != model["n_layers"] or any(
            isinstance(v, bool) or v not in (0, 1) for v in rl):
        raise PackageError("model.rope_layers must list 0/1 for every layer")
    if model["d_model"] != model["n_heads"] * model["d_head"] or model["d_head"] % 2:
        raise PackageError("inconsistent head geometry")
    if model["n_kv_heads"] > model["n_heads"] or model["n_heads"] % model["n_kv_heads"]:
        raise PackageError("n_heads must be a multiple of n_kv_heads")
    return model


class Package:
    """A validated arc.integer-package.v1 file with memory-mapped tensors."""

    def __init__(self, path: Path, *, check_values: bool = True, check_tables: bool = False):
        self.path = Path(path)
        self.size = self.path.stat().st_size
        with open(self.path, "rb") as f:
            head = f.read(16)
            if len(head) != 16 or head[:8] != MAGIC:
                raise PackageError("not an ARCIPKG1 file")
            (hlen,) = struct.unpack("<Q", head[8:])
            if hlen > self.size - 16:
                raise PackageError("header length exceeds the file")
            raw = f.read(hlen)
            try:
                header = json.loads(raw.decode("ascii"), parse_float=_reject_float,
                                    parse_constant=_reject_float)
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                raise PackageError(f"header is not ASCII JSON: {error}") from None
            if canonical_json(header) != raw:
                raise PackageError("header is not canonical JSON")
            if not isinstance(header, dict) or sorted(header) != ["model", "profile", "schema",
                                                                   "source", "tensors"]:
                raise PackageError("header has the wrong top-level fields")
            if header["schema"] != PACKAGE_SCHEMA or header["profile"] != PROFILE_ID:
                raise PackageError(f"schema/profile {header['schema']!r}/{header['profile']!r}")
            self.model = _validate_model(header["model"])
            source = header["source"]
            if not isinstance(source, dict) or sorted(source) != ["files", "repo", "revision"] or \
                    not isinstance(source["files"], list) or any(
                        not isinstance(e, dict) or sorted(e) != ["bytes", "name", "sha256"]
                        for e in source["files"]):
                raise PackageError("header 'source' is malformed")
            expected = header_tensors(self.model)
            if header["tensors"] != expected:
                for i, (got, want) in enumerate(zip(header["tensors"], expected)):
                    if got != want:
                        raise PackageError(f"tensor entry {i} is {got!r}, expected {want!r}")
                raise PackageError("wrong number of tensor entries")
            self.header = header
            self.data_start = _align(16 + hlen)
            last = expected[-1]
            want_size = self.data_start + _align(last["offset"] + last["bytes"])
            if self.size != want_size:
                raise PackageError(f"file is {self.size} bytes, layout needs {want_size}")
            pads = [(16 + hlen, self.data_start)]
            pads += [(self.data_start + e["offset"] + e["bytes"],
                      self.data_start + _align(e["offset"] + e["bytes"])) for e in expected]
            for begin, end in pads:
                if end > begin:
                    f.seek(begin)
                    if f.read(end - begin).strip(b"\x00"):
                        raise PackageError(f"non-zero padding at byte {begin}")
        self.entries = {e["name"]: e for e in expected}
        self._maps: Dict[str, np.ndarray] = {}
        if check_values:
            self.check_values()
        if check_tables:
            self.check_tables()

    def tensor(self, name: str) -> np.ndarray:
        if name not in self._maps:
            e = self.entries[name]
            self._maps[name] = np.memmap(self.path, dtype=DTYPE_NUMPY[e["dtype"]], mode="r",
                                         offset=self.data_start + e["offset"], shape=tuple(e["shape"]))
        return self._maps[name]

    def scaled_names(self) -> List[str]:
        return [n[:-2] for n in self.entries if n.endswith(".q")]

    def check_values(self) -> None:
        """q in [-127, 127]; mu = 0 (all-zero row, k = 16) or mu in [2^30, 2^31) with k in [16, 62]."""
        for base in self.scaled_names():
            q = self.tensor(base + ".q")
            mu = np.asarray(self.tensor(base + ".mu")).astype(np.int64)
            k = np.asarray(self.tensor(base + ".k")).astype(np.int64)
            step = _chunk_rows(q.shape[1])
            for r0 in range(0, q.shape[0], step):
                if np.any(np.asarray(q[r0:r0 + step]) == -128):
                    raise PackageError(f"{base}.q contains -128")
            zero = mu == 0
            if np.any(~zero & ((mu < MU_MIN) | (mu >= MU_LIMIT))):
                raise PackageError(f"{base}.mu outside [2^30, 2^31)")
            if np.any(~zero & ((k < K_MIN) | (k > K_MAX))):
                raise PackageError(f"{base}.k outside [16, 62]")
            if np.any(zero & (k != 16)):
                raise PackageError(f"{base}: a row with mu = 0 has k != 16")
            for row in np.nonzero(zero)[0].tolist():
                if np.any(np.asarray(q[row]) != 0):
                    raise PackageError(f"{base}: row {row} has mu = 0 but non-zero weights")

    def check_tables(self) -> None:
        m = self.model
        cos, sin = build_rope_tables(m["rope_theta"], m["d_head"], m["max_seq"])
        if not (np.array_equal(cos, self.tensor("rope.cos")) and np.array_equal(sin, self.tensor("rope.sin"))):
            raise PackageError("RoPE tables differ from the spec 4.5 values")

    def identity(self) -> Dict[str, Any]:
        h, b3, size = hashlib.sha256(), _blake3_ctor()(), 0
        with open(self.path, "rb") as f:
            while True:
                block = f.read(1 << 20)
                if not block:
                    break
                size += len(block)
                h.update(block)
                b3.update(block)
        return {"bytes": size, "sha256": h.hexdigest(), "blake3": b3.hexdigest()}


# --------------------------------------------------------------------------
# Operators in Python ints (spec 5)

def embed_int(q_row: Sequence[int], mu: int, k: int) -> List[int]:
    """Spec 5.3: e_j = (q_tj * mu_t) >> (k_t - 16)."""
    sh = k - 16
    return [check62((qv * mu) >> sh, "embedding") for qv in q_row]


def project_int(x: Sequence[int], rows: Sequence[Sequence[int]], mus: Sequence[int],
                ks: Sequence[int], what: str = "projection") -> List[int]:
    """Spec 5.2: y_i = (sum_j q_ij x_j * mu_i) >> k_i with the spec 9 checks."""
    if 127 * sum(abs(v) for v in x) >= 1 << 63:
        raise DomainError(f"{what}: 127 * sum|x| >= 2^63")
    out = []
    for row, mu, k in zip(rows, mus, ks):
        acc = sum(map(int.__mul__, row, x))
        out.append(check62((acc * mu) >> k, what))
    return out


def rmsnorm_int(x: Sequence[int], gains: Sequence[int], eps_q32: int,
                what: str = "rmsnorm") -> List[int]:
    """Spec 5.4."""
    n = len(x)
    v = sum(t * t for t in x) // n + eps_q32
    if v > 1 << 92:
        raise DomainError(f"{what}: v = {v} > 2^92")
    r = math.isqrt((1 << 92) // v)
    out = []
    for xi, gi in zip(x, gains):
        p = xi * r * gi
        if abs(p) >= 1 << 127:
            raise DomainError(f"{what}: |x*r*g| >= 2^127")
        out.append(check62(p >> 46, what))
    return out


def rope_int(u: Sequence[int], cos_row: Sequence[int], sin_row: Sequence[int]) -> List[int]:
    """Spec 5.5 on one head vector (split-half pairing)."""
    h = len(u) // 2
    out = list(u)
    for i in range(h):
        a, b, c, s = u[i], u[i + h], cos_row[i], sin_row[i]
        out[i] = check62((a * c - b * s) >> 16, "rope")
        out[i + h] = check62((a * s + b * c) >> 16, "rope")
    return out


def attention_int(q: Sequence[int], keys: Sequence[Sequence[int]],
                  values: Sequence[Sequence[int]], lam: int) -> List[int]:
    """Spec 5.6 for one query head over cached positions 0..p."""
    scores = [(sum(map(int.__mul__, q, kj)) * lam) >> 46 for kj in keys]
    top = max(scores)
    w = [exp_q16(s - top) for s in scores]
    z = sum(w)
    return [tdiv(sum(wj * vj[t] for wj, vj in zip(w, values)), z) for t in range(len(q))]


def silu_gate_int(g: int, u: int) -> int:
    """Spec 5.7: a = (g * sigma(g) * u) >> 32."""
    if g >= 0:
        sig = (1 << 32) // (ONE + exp_q16(-g))
    else:
        e = exp_q16(g)
        sig = (e << 16) // (ONE + e)
    return check62((g * sig * u) >> 32, "gated silu")


def residual_int(h: Sequence[int], y: Sequence[int]) -> List[int]:
    return [check62(a + b, "residual") for a, b in zip(h, y)]


def check_i32(values: Sequence[int], what: str) -> None:
    for v in values:
        if v < I32_MIN or v > I32_MAX:
            raise DomainError(f"{what} = {v} does not fit the i32 KV cache")


class SlowEngine:
    """Spec 5.8 one token at a time with a KV cache, all in Python ints (tiny models)."""

    def __init__(self, pkg: Package):
        m = pkg.model
        self.model = m
        self.eps = m["rms_eps_q32"]
        self.lam = math.isqrt((1 << 60) // m["d_head"])
        lists = lambda name: np.asarray(pkg.tensor(name)).astype(np.int64).tolist()  # noqa: E731
        scaled = lambda base: (lists(base + ".q"), lists(base + ".mu"), lists(base + ".k"))  # noqa: E731
        self.embed = scaled("embed")
        self.final_norm = lists("final_norm")
        self.cos, self.sin = lists("rope.cos"), lists("rope.sin")
        self.layers = []
        for layer in range(m["n_layers"]):
            p = f"layers.{layer}."
            entry = {"attn_norm": lists(p + "attn_norm"), "ffn_norm": lists(p + "ffn_norm")}
            for short in ("wq", "wk", "wv", "wo", "w_gate", "w_up", "w_down"):
                entry[short] = scaled(p + short)
            self.layers.append(entry)
        self.reset()

    def reset(self) -> None:
        self.keys: List[List[List[int]]] = [[] for _ in self.layers]
        self.values: List[List[List[int]]] = [[] for _ in self.layers]

    @property
    def position(self) -> int:
        return len(self.keys[0]) if self.keys else 0

    def forward(self, token: int) -> List[int]:
        m = self.model
        pos = self.position
        if isinstance(token, bool) or not isinstance(token, int) or not 0 <= token < m["vocab_size"]:
            raise DomainError(f"token id {token!r} outside the vocabulary")
        if pos >= m["max_seq"]:
            raise DomainError(f"position {pos} >= max_seq")
        dh, hq, hk = m["d_head"], m["n_heads"], m["n_kv_heads"]
        q_rows, mus, ks = self.embed
        h = embed_int(q_rows[token], mus[token], ks[token])
        for li, layer in enumerate(self.layers):
            n = rmsnorm_int(h, layer["attn_norm"], self.eps, f"layer {li} attn_norm")
            q = project_int(n, *layer["wq"], f"layer {li} wq")
            k = project_int(n, *layer["wk"], f"layer {li} wk")
            v = project_int(n, *layer["wv"], f"layer {li} wv")
            if m["rope_layers"][li]:
                c, s = self.cos[pos], self.sin[pos]
                q = [x for hd in range(hq) for x in rope_int(q[hd * dh:(hd + 1) * dh], c, s)]
                k = [x for hd in range(hk) for x in rope_int(k[hd * dh:(hd + 1) * dh], c, s)]
            check_i32(k, f"layer {li} K")
            check_i32(v, f"layer {li} V")
            self.keys[li].append(k)
            self.values[li].append(v)
            att: List[int] = []
            for hd in range(hq):
                kv = (hd * hk) // hq
                sl = slice(kv * dh, (kv + 1) * dh)
                att += attention_int(q[hd * dh:(hd + 1) * dh], [r[sl] for r in self.keys[li]],
                                     [r[sl] for r in self.values[li]], self.lam)
            h = residual_int(h, project_int(att, *layer["wo"], f"layer {li} wo"))
            n = rmsnorm_int(h, layer["ffn_norm"], self.eps, f"layer {li} ffn_norm")
            g = project_int(n, *layer["w_gate"], f"layer {li} w_gate")
            u = project_int(n, *layer["w_up"], f"layer {li} w_up")
            a = [silu_gate_int(gi, ui) for gi, ui in zip(g, u)]
            h = residual_int(h, project_int(a, *layer["w_down"], f"layer {li} w_down"))
        n = rmsnorm_int(h, self.final_norm, self.eps, "final norm")
        return project_int(n, q_rows, mus, ks, "lm head")


# --------------------------------------------------------------------------
# Selection (spec 6.1) and digests (spec 6.3)

def select_next(logits: Any, history: Sequence[int], selection: str) -> int:
    """argmax (lowest id on ties), after the rp64 penalty when selected."""
    arr = np.array(logits, dtype=np.int64)
    if selection == "rp64-argmax":
        for t in reversed(list(history)[-64:]):  # newest first, once per occurrence
            v = int(arr[t])
            v = tdiv(5 * v, 6) if v > 0 else tdiv(6 * v, 5)
            arr[t] = check62(v, "penalised logit")
    elif selection != "argmax":
        raise RunError(f"unknown selection {selection!r}")
    return int(np.argmax(arr))


def logits_hash_raw(logits: Any) -> bytes:
    return blake3_raw(np.asarray(logits, dtype=np.int64).astype("<i8").tobytes())


def output_hash(tokens: Sequence[int]) -> str:
    return blake3_hex(np.array(list(tokens), dtype=np.int64).astype("<u4").tobytes())


def logits_digest(raw_hashes: Sequence[bytes]) -> str:
    return blake3_hex(b"".join(raw_hashes))


def matrix_digest(cases: Sequence[Dict[str, Any]]) -> str:
    return blake3_hex(canonical_json([{"id": c["id"], "logits_digest": c["logits_digest"],
                                       "output_hash": c["output_hash"], "tokens": list(c["tokens"])}
                                      for c in cases]))


def validate_case(case: Any, model: Dict[str, Any], *, need_tokens: bool = False) -> Dict[str, Any]:
    """Spec 6.2 input rules for one case (and the token list of a run when present)."""
    if not isinstance(case, dict):
        raise RunError("a case must be an object")
    cid = case.get("id")
    if not isinstance(cid, str) or not cid:
        raise RunError("a case needs a non-empty string id")

    def ids(key: str, allow_empty: bool) -> List[int]:
        v = case.get(key)
        if not isinstance(v, list) or (not v and not allow_empty) or any(
                isinstance(t, bool) or not isinstance(t, int) or not 0 <= t < model["vocab_size"] for t in v):
            raise RunError(f"case {cid}: {key} must be token ids < vocab_size")
        return list(v)

    prompt = ids("prompt_tokens", False)
    max_tokens = case.get("max_tokens")
    if isinstance(max_tokens, bool) or not isinstance(max_tokens, int) or max_tokens < 1:
        raise RunError(f"case {cid}: max_tokens must be an integer >= 1")
    if len(prompt) + max_tokens > model["max_seq"]:
        raise RunError(f"case {cid}: prompt_len + max_tokens > max_seq")
    eos = case.get("eos")
    if not isinstance(eos, list) or any(isinstance(t, bool) or not isinstance(t, int) for t in eos):
        raise RunError(f"case {cid}: eos must be a list of token ids")
    selection = case.get("selection")
    if selection not in SELECTION_GENERATION:
        raise RunError(f"case {cid}: selection must be one of {sorted(SELECTION_GENERATION)}")
    out = {"id": cid, "prompt_tokens": prompt, "max_tokens": max_tokens, "eos": list(eos),
           "selection": selection}
    if need_tokens:
        out["tokens"] = ids("tokens", True)
    return out


def generate_case(engine: SlowEngine, case: Dict[str, Any]) -> Dict[str, Any]:
    """Spec 6.2: P + len(out) - 1 forward calls; the last token is never forwarded."""
    engine.reset()
    hashes: List[bytes] = []
    logits: List[int] = []
    for t in case["prompt_tokens"]:
        logits = engine.forward(t)
        hashes.append(logits_hash_raw(logits))
    out: List[int] = []
    while True:
        nxt = select_next(logits, out, case["selection"])
        out.append(nxt)
        if nxt in case["eos"] or len(out) == case["max_tokens"]:
            break
        logits = engine.forward(nxt)
        hashes.append(logits_hash_raw(logits))
    return dict(case, tokens=out, output_hash=output_hash(out),
                logits_hashes=[h.hex() for h in hashes], logits_digest=logits_digest(hashes))


# --------------------------------------------------------------------------
# Exact vectorised arithmetic for the fast path
#
# float64 matmul is exact when every partial sum it can form is an integer of
# magnitude below 2^53: each partial sum of output (i, j) is bounded by
# sum_t |a_it| |b_tj| <= (sum_t |a_it|) * max|b|. When that bound fails the
# left operand (and, for attention, the right one) is split into
# sign-magnitude limbs whose products satisfy it; each limb product is then
# exact and the shifted limb products are summed in int64, where every
# partial sum is bounded by sum_t |a_it| |b_tj| as well. Tests lower
# FLOAT_EXACT_LIMIT to force the limb path on tiny models.

FLOAT_EXACT_LIMIT = float(1 << 53)
_M21 = (1 << 21) - 1
_M31 = (1 << 31) - 1
_M32 = (1 << 32) - 1


class _NeedSlow(Exception):
    """A fast-path bound failed; the caller recomputes with Python ints."""


def _abs_sum_bound(a: np.ndarray) -> np.ndarray:
    """An upper bound on sum |a| over the last axis (float64, relative slack 2^-30)."""
    return np.sum(np.abs(a).astype(np.float64), axis=-1) * (1.0 + 2.0 ** -30)


def _limbs(a: np.ndarray, bits: int) -> List[np.ndarray]:
    """a = sum_m limbs[m] << (bits*m); every limb has the sign of a and |limb| < 2^bits."""
    neg = a < 0
    mag = np.where(neg, -a, a)
    mask = (1 << bits) - 1
    out = []
    while True:
        limb = mag & mask
        out.append(np.where(neg, -limb, limb))
        mag = mag >> bits
        if not mag.any():
            return out


def _blas_int(af: np.ndarray, bf: np.ndarray) -> np.ndarray:
    """float64 matmul of integer-valued operands whose partial sums are proven < 2^53, as int64.

    macOS Accelerate sets spurious floating-point exception flags in dgemm
    while returning exact results, so the flags are ignored here; as a
    defensive check against a broken BLAS, the result must be finite and
    integral (exactness itself rests on the bound, not on this check).
    """
    with np.errstate(all="ignore"):
        r = np.matmul(af, bf)
    if r.size and not (np.all(np.isfinite(r)) and np.array_equal(r, np.trunc(r))):
        raise RuntimeError("BLAS returned a non-integral product of integer operands")
    return r.astype(np.int64)


def _matmul_exact(a: np.ndarray, b: Optional[np.ndarray] = None, *, b_float: Optional[np.ndarray] = None,
                  b_max: Optional[int] = None, guard: bool = True) -> np.ndarray:
    """Exact integer a @ b as int64 (a int64 [..., n, k]; b int64 [..., k, m] or b_float with |b| <= b_max).

    With guard=True, raises _NeedSlow unless sum_t |a_it| |b_tj| < 2^62 is
    proven; callers that have checked the exact spec 5.2 precondition
    (127 * sum|x| < 2^63 with |b| <= 127) pass guard=False.
    """
    if b_float is None:
        b_max = int(np.max(np.abs(b))) if b.size else 0
    k = a.shape[-1]
    rows = _abs_sum_bound(a)
    bound = (float(np.max(rows)) if rows.size else 0.0) * float(b_max)
    if guard and bound >= 2.0 ** 62:
        raise _NeedSlow("matmul bound")
    if bound < FLOAT_EXACT_LIMIT:
        bf = b_float if b_float is not None else b.astype(np.float64)
        return _blas_int(a.astype(np.float64), bf)
    budget = int(math.log2(FLOAT_EXACT_LIMIT)) - max(1, int(k).bit_length()) - 1
    bb = max(1, int(b_max).bit_length())
    if b_float is not None or bb <= budget // 2:
        b_parts = [b_float if b_float is not None else b.astype(np.float64)]
    else:
        bb = budget // 2
        b_parts = [x.astype(np.float64) for x in _limbs(b, bb)]
    ba = budget - bb
    if ba < 1:
        raise _NeedSlow("limb budget")
    total: Optional[np.ndarray] = None
    for i, limb in enumerate(_limbs(a, ba)):
        af = limb.astype(np.float64)
        for j, bf in enumerate(b_parts):
            part = _blas_int(af, bf)
            sh = ba * i + bb * j
            if sh:
                part = np.left_shift(part, sh)
            total = part if total is None else total + part
    return total


def _mul_u31_shr(a: np.ndarray, m: Any, s: Any) -> np.ndarray:
    """floor(a * m / 2^s) exactly for int64 a, 0 <= m < 2^31, 16 <= s <= 62 (m, s broadcast on the last axis).

    a = ah*2^32 + al with 0 <= al < 2^32, so a*m = P*2^32 + Q with |P| < 2^62
    and 0 <= Q < 2^63. For s >= 32 the result is (P + (Q >> 32)) >> (s - 32);
    for s < 32 it is P*2^(32-s) + (Q >> s), computed only when it cannot
    overflow (otherwise _NeedSlow).
    """
    m = np.asarray(m, dtype=np.int64)
    s = np.asarray(s, dtype=np.int64)
    p = (a >> 32) * m
    q = (a & _M32) * m
    y = (p + (q >> 32)) >> np.maximum(s - 32, 0)
    lo = s < 32
    if np.any(lo):
        if s.ndim == 0:
            sl = int(s)
            if np.any(np.abs(p) >= (1 << (29 + sl))):
                raise _NeedSlow("epilogue")
            return np.left_shift(p, 32 - sl) + (q >> sl)
        pl, ql, sl = p[..., lo], q[..., lo], s[lo]
        if np.any(np.abs(pl) >= np.left_shift(np.int64(1), 29 + sl)):
            raise _NeedSlow("epilogue")
        y[..., lo] = np.left_shift(pl, 32 - sl) + (ql >> sl)
    return y


def _mul_shr(a: np.ndarray, b: Any, s: int) -> np.ndarray:
    """floor(a * b / 2^s) exactly for int64 |a|, |b| <= 2^62 and 31 <= s <= 62.

    31-bit limbs: a*b = t2*2^62 + c1*2^31 + c0 with 0 <= c0, c1 < 2^31, then
    floor = t2*2^(62-s) + (c1 >> (s-31)); _NeedSlow when that could overflow.
    """
    b = np.asarray(b, dtype=np.int64)
    a1, a0 = a >> 31, a & _M31
    b1, b0 = b >> 31, b & _M31
    p0 = a0 * b0
    t1 = a1 * b0 + a0 * b1 + (p0 >> 31)
    t2 = a1 * b1 + (t1 >> 31)
    if np.any(np.abs(t2) >= (1 << (s - 1))):
        raise _NeedSlow("mul_shr")
    return np.left_shift(t2, 62 - s) + ((t1 & _M31) >> (s - 31))


def _check62_array(y: np.ndarray, what: str) -> np.ndarray:
    if y.size and (int(np.max(y)) > LIM62 or int(np.min(y)) < -LIM62):
        raise DomainError(f"{what}: a value exceeds 2^62 in magnitude")
    return y


def _exp_vec(x: np.ndarray) -> np.ndarray:
    """Spec 5.1 elementwise for int64 x <= 0."""
    if x.size and int(np.max(x)) > 0:
        raise DomainError("exp argument is positive")
    t = exp_table_np()
    out = np.zeros(x.shape, dtype=np.int64)
    out[x == 0] = ONE
    mid = (x < 0) & (x > -16 * ONE)
    o = x[mid] + 16 * ONE
    i, f = o >> 8, o & 255
    ti = t[i]
    out[mid] = ti + (((t[i + 1] - ti) * f) >> 8)
    return out


def _to_int64(rows: List[List[int]]) -> np.ndarray:
    return np.array(rows, dtype=np.int64)


class FastEngine:
    """Exact batched forward: spec 5.8 at every position of every sequence (teacher forcing).

    All positions of all sequences are stacked for the projections, layer by
    layer, so every INT8 matrix is converted to float64 once per call. The
    result at every position equals SlowEngine's bit for bit.
    """

    def __init__(self, pkg: Package):
        self.pkg = pkg
        self.model = pkg.model
        self.eps = self.model["rms_eps_q32"]
        self.lam = math.isqrt((1 << 60) // self.model["d_head"])
        self.cos = np.asarray(pkg.tensor("rope.cos")).astype(np.int64)
        self.sin = np.asarray(pkg.tensor("rope.sin")).astype(np.int64)
        self.fallbacks: Dict[str, int] = {}

    def _note(self, what: str) -> None:
        self.fallbacks[what] = self.fallbacks.get(what, 0) + 1

    def _scaled(self, base: str) -> Tuple[np.ndarray, np.ndarray, np.ndarray]:
        return (self.pkg.tensor(base + ".q"),
                np.asarray(self.pkg.tensor(base + ".mu")).astype(np.int64),
                np.asarray(self.pkg.tensor(base + ".k")).astype(np.int64))

    # -- operators ---------------------------------------------------------

    def _embed(self, tokens: np.ndarray) -> np.ndarray:
        q, mu, k = self._scaled("embed")
        rows = np.asarray(q[tokens]).astype(np.int64)
        return (rows * mu[tokens][:, None]) >> (k[tokens][:, None] - 16)

    @staticmethod
    def _check_projection_input(x: np.ndarray, what: str) -> None:
        est = _abs_sum_bound(x) * 127.0
        for i in np.nonzero(est >= 2.0 ** 62)[0].tolist():
            if 127 * sum(abs(v) for v in x[i].tolist()) >= 1 << 63:
                raise DomainError(f"{what}: 127 * sum|x| >= 2^63")

    def _epilogue(self, acc: np.ndarray, mu: np.ndarray, k: np.ndarray, what: str) -> np.ndarray:
        try:
            y = _mul_u31_shr(acc, mu, k)
        except _NeedSlow:
            self._note("projection epilogue")
            mus, ks = mu.tolist(), k.tolist()
            y = _to_int64([[check62((a * m) >> s, what) for a, m, s in zip(r, mus, ks)]
                           for r in acc.tolist()])
        return _check62_array(y, what)

    def _project(self, x: np.ndarray, base: str, what: str) -> np.ndarray:
        q, mu, k = self._scaled(base)
        self._check_projection_input(x, what)
        acc = _matmul_exact(x, b_float=np.asarray(q, dtype=np.float64).T, b_max=127, guard=False)
        return self._epilogue(acc, mu, k, what)

    def _rmsnorm(self, x: np.ndarray, gains_name: str, what: str) -> np.ndarray:
        g = np.asarray(self.pkg.tensor(gains_name)).astype(np.int64)
        try:
            return self._rmsnorm_fast(x, g, what)
        except _NeedSlow:
            self._note("rmsnorm")
            gl = g.tolist()
            return _to_int64([rmsnorm_int(r, gl, self.eps, what) for r in x.tolist()])

    def _rmsnorm_fast(self, x: np.ndarray, g: np.ndarray, what: str) -> np.ndarray:
        rows, n = x.shape
        if n > 1 << 19:
            raise _NeedSlow("rmsnorm width")
        mag = np.abs(x)
        a, b, c = mag >> 42, (mag >> 21) & _M21, mag & _M21  # |x| = a*2^42 + b*2^21 + c
        s4, s3 = (a * a).sum(axis=1), (2 * a * b).sum(axis=1)
        s2, s1, s0 = (2 * a * c + b * b).sum(axis=1), (2 * b * c).sum(axis=1), (c * c).sum(axis=1)
        maxabs = mag.max(axis=1)
        r = np.empty(rows, dtype=np.int64)
        for i in range(rows):
            total = ((int(s4[i]) << 84) + (int(s3[i]) << 63) + (int(s2[i]) << 42)
                     + (int(s1[i]) << 21) + int(s0[i]))
            v = total // n + self.eps
            if v > 1 << 92:
                raise DomainError(f"{what}: v = {v} > 2^92")
            ri = math.isqrt((1 << 92) // v)
            if int(maxabs[i]) * ri > LIM62:
                raise _NeedSlow("rmsnorm x*r")
            r[i] = ri
        if g.size and int(np.max(np.abs(g))) > LIM62:
            raise _NeedSlow("rmsnorm gain")
        # |x*r| <= 2^62 and |g| <= 2^62, so |x*r*g| < 2^124 < 2^127 holds.
        return _check62_array(_mul_shr(x * r[:, None], g[None, :], 46), what)

    def _rope(self, x: np.ndarray, n_heads: int, positions: np.ndarray, what: str) -> np.ndarray:
        d = self.model["d_head"]
        h = d // 2
        cos, sin = self.cos[positions], self.sin[positions]
        rows = x.shape[0]
        xs = x.reshape(rows, n_heads, d)
        tmax = max(int(np.max(np.abs(cos))), int(np.max(np.abs(sin))))
        if int(np.max(np.abs(xs))) * tmax < LIM62:
            a, b = xs[:, :, :h], xs[:, :, h:]
            c, s = cos[:, None, :], sin[:, None, :]
            out = np.concatenate(((a * c - b * s) >> 16, (a * s + b * c) >> 16), axis=2)
            return _check62_array(out.reshape(rows, n_heads * d), what)
        self._note("rope")
        out_rows = []
        for i, row in enumerate(x.tolist()):
            cr, sr = cos[i].tolist(), sin[i].tolist()
            out_rows.append([v for hd in range(n_heads) for v in rope_int(row[hd * d:(hd + 1) * d], cr, sr)])
        return _to_int64(out_rows)

    def _attention(self, q: np.ndarray, k: np.ndarray, v: np.ndarray,
                   starts: Sequence[int], lengths: Sequence[int]) -> np.ndarray:
        m = self.model
        hq, hk, d = m["n_heads"], m["n_kv_heads"], m["d_head"]
        kv_of = (np.arange(hq) * hk) // hq
        out = np.empty_like(q)
        for s0, length in zip(starts, lengths):
            sl = slice(s0, s0 + length)
            q3 = q[sl].reshape(length, hq, d).transpose(1, 0, 2)
            k3 = k[sl].reshape(length, hk, d).transpose(1, 0, 2)[kv_of]
            v3 = v[sl].reshape(length, hk, d).transpose(1, 0, 2)[kv_of]
            try:
                o = self._attention_fast(q3, k3, v3)
            except _NeedSlow:
                self._note("attention")
                o = np.array([[attention_int(q3[hd, i].tolist(), k3[hd, :i + 1].tolist(),
                                             v3[hd, :i + 1].tolist(), self.lam)
                               for i in range(length)] for hd in range(hq)], dtype=np.int64)
            out[sl] = o.transpose(1, 0, 2).reshape(length, hq * d)
        return out

    def _attention_fast(self, q3: np.ndarray, k3: np.ndarray, v3: np.ndarray) -> np.ndarray:
        dot = _matmul_exact(q3, k3.transpose(0, 2, 1))  # [H, P, P], |dot| < 2^62 proven
        score = _mul_u31_shr(dot, self.lam, 46)
        p = q3.shape[1]
        causal = np.tril(np.ones((p, p), dtype=bool))
        top = np.where(causal, score, np.iinfo(np.int64).min).max(axis=2, keepdims=True)
        w = _exp_vec(np.where(causal, score - top, -16 * ONE))  # masked -> exp(-16) = 0
        z = w.sum(axis=2)[..., None]
        o = _matmul_exact(w, v3)
        return np.where(o >= 0, o // z, -((-o) // z))  # tdiv, z > 0

    def _silu(self, g: np.ndarray, u: np.ndarray, what: str) -> np.ndarray:
        try:
            if g.size and int(np.max(np.abs(g))) >= 1 << 46:
                raise _NeedSlow("silu gate")
            neg = g < 0
            e = _exp_vec(np.where(neg, g, -g))
            sig = np.where(neg, (e << 16) // (ONE + e), (1 << 32) // (ONE + e))
            y = _mul_shr(g * sig, u, 32)
        except _NeedSlow:
            self._note("silu")
            y = _to_int64([[silu_gate_int(a, b) for a, b in zip(gr, ur)]
                           for gr, ur in zip(g.tolist(), u.tolist())])
        return _check62_array(y, what)

    def _residual(self, h: np.ndarray, y: np.ndarray, what: str) -> np.ndarray:
        if int(np.max(np.abs(h))) + int(np.max(np.abs(y))) >= 1 << 63:
            self._note("residual")
            return _to_int64([residual_int(a, b) for a, b in zip(h.tolist(), y.tolist())])
        return _check62_array(h + y, what)

    @staticmethod
    def _check_i32(x: np.ndarray, what: str) -> None:
        if x.size and (int(np.max(x)) > I32_MAX or int(np.min(x)) < I32_MIN):
            raise DomainError(f"{what}: a value does not fit the i32 KV cache")

    # -- forward -----------------------------------------------------------

    def _layer(self, li: int, h: np.ndarray, positions: np.ndarray,
               starts: Sequence[int], lengths: Sequence[int]) -> np.ndarray:
        m = self.model
        p = f"layers.{li}."
        n = self._rmsnorm(h, p + "attn_norm", f"layer {li} attn_norm")
        q = self._project(n, p + "wq", f"layer {li} wq")
        k = self._project(n, p + "wk", f"layer {li} wk")
        v = self._project(n, p + "wv", f"layer {li} wv")
        if m["rope_layers"][li]:
            q = self._rope(q, m["n_heads"], positions, f"layer {li} rope q")
            k = self._rope(k, m["n_kv_heads"], positions, f"layer {li} rope k")
        self._check_i32(k, f"layer {li} K")
        self._check_i32(v, f"layer {li} V")
        att = self._attention(q, k, v, starts, lengths)
        h = self._residual(h, self._project(att, p + "wo", f"layer {li} wo"), f"layer {li} residual")
        n = self._rmsnorm(h, p + "ffn_norm", f"layer {li} ffn_norm")
        g = self._project(n, p + "w_gate", f"layer {li} w_gate")
        u = self._project(n, p + "w_up", f"layer {li} w_up")
        a = self._silu(g, u, f"layer {li} gated silu")
        del g, u, n
        return self._residual(h, self._project(a, p + "w_down", f"layer {li} w_down"), f"layer {li} residual")

    def _lm_head(self, n: np.ndarray, seq_ids: np.ndarray, positions: np.ndarray,
                 on_logits: Callable[[int, int, np.ndarray], None]) -> None:
        q, mu, k = self._scaled("embed")
        vocab, width = q.shape
        self._check_projection_input(n, "lm head")
        group = max(1, min(n.shape[0], (1 << 25) // vocab))  # <= 256 MB of int64 logits
        chunk = max(1, (1 << 24) // width)  # <= 128 MB of float64 weights
        for g0 in range(0, n.shape[0], group):
            ng = n[g0:g0 + group]
            logits = np.empty((ng.shape[0], vocab), dtype=np.int64)
            for r0 in range(0, vocab, chunk):
                wf_t = np.asarray(q[r0:r0 + chunk], dtype=np.float64).T
                acc = _matmul_exact(ng, b_float=wf_t, b_max=127, guard=False)
                logits[:, r0:r0 + chunk] = self._epilogue(acc, mu[r0:r0 + chunk], k[r0:r0 + chunk], "lm head")
            for i in range(ng.shape[0]):
                on_logits(int(seq_ids[g0 + i]), int(positions[g0 + i]), logits[i])

    def teacher_forced(self, sequences: Sequence[Sequence[int]],
                       on_logits: Callable[[int, int, np.ndarray], None]) -> None:
        """Forward every sequence from position 0; on_logits(seq, pos, logits) in stacked order."""
        m = self.model
        seqs = [list(s) for s in sequences]
        for s in seqs:
            if not s:
                raise RunError("empty sequence")
            if len(s) > m["max_seq"]:
                raise DomainError("position >= max_seq")
            for t in s:
                if isinstance(t, bool) or not isinstance(t, int) or not 0 <= t < m["vocab_size"]:
                    raise DomainError(f"token id {t!r} outside the vocabulary")
        lengths = [len(s) for s in seqs]
        starts = [sum(lengths[:i]) for i in range(len(seqs))]
        tokens = np.array([t for s in seqs for t in s], dtype=np.int64)
        positions = np.concatenate([np.arange(n, dtype=np.int64) for n in lengths])
        seq_ids = np.repeat(np.arange(len(seqs)), lengths)
        h = self._embed(tokens)
        for li in range(m["n_layers"]):
            h = self._layer(li, h, positions, starts, lengths)
        n = self._rmsnorm(h, "final_norm", "final norm")
        self._lm_head(n, seq_ids, positions, on_logits)

    def all_logits(self, sequences: Sequence[Sequence[int]]) -> List[List[np.ndarray]]:
        """Every position's logits (small models / tests)."""
        out: List[List[np.ndarray]] = [[] for _ in sequences]
        self.teacher_forced(sequences, lambda s, p, lg: out[s].append(lg.copy()))
        return out


# --------------------------------------------------------------------------
# Runs: independent generation (slow path) and verification of a Rust run

def _read_json(path: Path) -> Any:
    return json.loads(Path(path).read_bytes())


def _write_json(path: Path, obj: Any) -> None:
    Path(path).write_bytes((json.dumps(obj, indent=1) + "\n").encode("ascii"))


def stop_rule_ok(case: Dict[str, Any]) -> bool:
    """Spec 6.2: an eos token may only be last; without eos the run used all max_tokens."""
    toks = case["tokens"]
    if not toks or len(toks) > case["max_tokens"]:
        return False
    if any(t in case["eos"] for t in toks[:-1]):
        return False
    return toks[-1] in case["eos"] or len(toks) == case["max_tokens"]


def _unique_ids(cases: Sequence[Dict[str, Any]]) -> None:
    ids = [c["id"] for c in cases]
    if len(set(ids)) != len(ids):
        raise RunError("case ids must be unique")


def generate_run(pkg_path: Path, cases_path: Path) -> Dict[str, Any]:
    pkg = Package(pkg_path, check_values=True)
    doc = _read_json(cases_path)
    if not isinstance(doc, dict) or doc.get("schema") != CASES_SCHEMA or not isinstance(doc.get("cases"), list):
        raise RunError(f"cases file must have schema {CASES_SCHEMA!r} and a 'cases' list")
    cases = [validate_case(c, pkg.model) for c in doc["cases"]]
    _unique_ids(cases)
    engine = SlowEngine(pkg)
    results = [generate_case(engine, c) for c in cases]
    selections = sorted({c["selection"] for c in cases})
    generation = SELECTION_GENERATION[selections[0]] if len(selections) == 1 else GENERATION_RP64_ID
    ident = pkg.identity()
    return {"schema": RUN_SCHEMA, "package": {"sha256": ident["sha256"], "bytes": ident["bytes"]},
            "profile": PROFILE_ID, "generation": generation, "kernel": "python-reference-int",
            "cases": results, "matrix_digest": matrix_digest(results)}


def verify_run(pkg_path: Path, run_path: Path) -> Tuple[Dict[str, Any], List[str]]:
    """Teacher-force every case of a run with FastEngine; return (golden document, mismatches)."""
    run = _read_json(run_path)
    if not isinstance(run, dict) or run.get("schema") != RUN_SCHEMA:
        raise RunError(f"run must have schema {RUN_SCHEMA!r}")
    if run.get("profile") != PROFILE_ID:
        raise RunError(f"run profile {run.get('profile')!r} is not {PROFILE_ID!r}")
    if run.get("generation") not in (GENERATION_RP64_ID, GENERATION_ARGMAX_ID):
        raise RunError(f"unknown generation semantics {run.get('generation')!r}")
    if not isinstance(run.get("cases"), list) or not run["cases"]:
        raise RunError("run has no cases")
    pkg = Package(pkg_path, check_values=True)
    ident = pkg.identity()
    problems: List[str] = []
    ref = run.get("package") if isinstance(run.get("package"), dict) else {}
    package_match = ref.get("sha256") == ident["sha256"] and ref.get("bytes") == ident["bytes"]
    if not package_match:
        problems.append(f"run package {ref!r} is not this package (sha256 {ident['sha256']}, "
                        f"{ident['bytes']} bytes)")
    cases = [validate_case(c, pkg.model, need_tokens=True) for c in run["cases"]]
    _unique_ids(cases)
    state: List[Dict[str, List[Any]]] = [{"hashes": [], "tokens": []} for _ in cases]

    def on_logits(ci: int, pos: int, logits: np.ndarray) -> None:
        st, c = state[ci], cases[ci]
        st["hashes"].append(logits_hash_raw(logits))
        gi = pos - (len(c["prompt_tokens"]) - 1)
        if 0 <= gi < len(c["tokens"]):
            st["tokens"].append(select_next(logits, c["tokens"][:gi], c["selection"]))

    # A run with more than max_tokens tokens is reported (stop rule), not forwarded past max_seq.
    engine = FastEngine(pkg)
    engine.teacher_forced([c["prompt_tokens"] + c["tokens"][:c["max_tokens"]][:-1] for c in cases], on_logits)
    golden_cases = []
    for c, raw, st in zip(cases, run["cases"], state):
        mine = [h.hex() for h in st["hashes"]]
        theirs = raw.get("logits_hashes")
        theirs = theirs if isinstance(theirs, list) else []
        first_hash = next((i for i in range(max(len(mine), len(theirs)))
                           if i >= len(mine) or i >= len(theirs) or mine[i] != theirs[i]), None)
        first_token = next((i for i in range(max(len(st["tokens"]), len(c["tokens"])))
                            if i >= len(st["tokens"]) or i >= len(c["tokens"])
                            or st["tokens"][i] != c["tokens"][i]), None)
        digest = logits_digest(st["hashes"])
        checks = {
            "logits_hashes_match": first_hash is None,
            "first_mismatch_position": first_hash,
            "tokens_match": first_token is None,
            "first_token_mismatch": first_token,
            "stop_rule_ok": stop_rule_ok(c),
            "output_hash_match": raw.get("output_hash") == output_hash(c["tokens"]),
            "logits_digest_match": raw.get("logits_digest") == digest,
        }
        if first_hash is not None:
            problems.append(f"case {c['id']!r}: logits hash differs at position {first_hash} "
                            f"(ours {mine[first_hash] if first_hash < len(mine) else 'none'}, "
                            f"run {theirs[first_hash] if first_hash < len(theirs) else 'none'})")
        if first_token is not None:
            problems.append(f"case {c['id']!r}: generated token {first_token} differs "
                            f"(re-derived {st['tokens'][first_token] if first_token < len(st['tokens']) else 'none'}, "
                            f"run {c['tokens'][first_token] if first_token < len(c['tokens']) else 'none'})")
        for key, label in (("stop_rule_ok", "stop rule violated"), ("output_hash_match", "output_hash differs"),
                           ("logits_digest_match", "logits_digest differs")):
            if not checks[key]:
                problems.append(f"case {c['id']!r}: {label}")
        golden_cases.append({
            "id": c["id"], "prompt_tokens": c["prompt_tokens"], "max_tokens": c["max_tokens"],
            "eos": c["eos"], "selection": c["selection"], "tokens": st["tokens"],
            "output_hash": output_hash(st["tokens"]), "logits_hashes": mine, "logits_digest": digest,
            "checks": checks})
    mdigest = matrix_digest(golden_cases)
    matrix_match = run.get("matrix_digest") == mdigest
    if not matrix_match:
        problems.append("matrix_digest differs")
    golden = {"schema": GOLDEN_SCHEMA, "package": {"sha256": ident["sha256"], "bytes": ident["bytes"]},
              "profile": PROFILE_ID, "generation": run["generation"],
              "verifier": "arc_conformance.modern_reference FastEngine (independent Python)",
              "cases": golden_cases, "matrix_digest": mdigest,
              "checks": {"package_match": package_match, "matrix_digest_match": matrix_match,
                         "fast_path_fallbacks": dict(engine.fallbacks), "all_match": not problems}}
    return golden, problems


# --------------------------------------------------------------------------
# CLI

def _cmd_tables(args: argparse.Namespace) -> int:
    t = exp_table()
    exp_digest = exp_table_blake3(t)
    cos, sin = build_rope_tables(args.theta, args.d_head, args.max_seq)
    rope_digest = rope_tables_blake3(cos, sin)
    pinned = (args.theta, args.d_head, args.max_seq) == tuple(ROPE_SMOLLM3.values())
    out = {"exp": {"blake3": exp_digest, "matches_spec": exp_digest == EXP_TABLE_BLAKE3,
                   "T[0..3]": t[0:4], "T[3840]": t[3840], "T[4096]": t[4096]},
           "rope": {"theta": args.theta, "d_head": args.d_head, "max_seq": args.max_seq, "blake3": rope_digest,
                    "matches_spec": (rope_digest == ROPE_SMOLLM3_BLAKE3) if pinned else None,
                    "cos[1][0..3]": cos[1][:3].tolist() if args.max_seq > 1 else None,
                    "sin[1][0..3]": sin[1][:3].tolist() if args.max_seq > 1 else None,
                    "sin[S-1][h-1]": int(sin[-1][-1])}}
    print(json.dumps(out, indent=1))
    return 0 if out["exp"]["matches_spec"] and out["rope"]["matches_spec"] is not False else 1


def _cmd_prepare(args: argparse.Namespace) -> int:
    ident = prepare_package(Path(args.source_dir), Path(args.source_manifest), Path(args.out))
    if args.json_out:
        _write_json(Path(args.json_out), ident)
    print(json.dumps(ident, indent=1))
    return 0


def _cmd_inspect(args: argparse.Namespace) -> int:
    pkg = Package(Path(args.package), check_values=True, check_tables=args.check_tables)
    totals: Dict[str, int] = {}
    for e in pkg.header["tensors"]:
        totals[e["dtype"]] = totals.get(e["dtype"], 0) + e["bytes"]
    out = {"schema": pkg.header["schema"], "profile": pkg.header["profile"], "model": pkg.model,
           "source": pkg.header["source"], "bytes": pkg.size, "data_start": pkg.data_start,
           "tensors": len(pkg.header["tensors"]), "tensor_bytes_by_dtype": totals,
           "values_checked": True, "rope_tables_checked": bool(args.check_tables)}
    if args.tensors:
        out["tensor_list"] = pkg.header["tensors"]
    print(json.dumps(out, indent=1))
    return 0


def _cmd_generate(args: argparse.Namespace) -> int:
    run = generate_run(Path(args.package), Path(args.cases))
    _write_json(Path(args.out), run)
    print(json.dumps({"cases": len(run["cases"]), "matrix_digest": run["matrix_digest"]}, indent=1))
    return 0


def _cmd_verify_run(args: argparse.Namespace) -> int:
    golden, problems = verify_run(Path(args.package), Path(args.run))
    _write_json(Path(args.out), golden)
    if problems:
        print(f"MISMATCH: {problems[0]}", file=sys.stderr)
        for p in problems[1:]:
            print(f"  also: {p}", file=sys.stderr)
        return 1
    print(json.dumps({"all_match": True, "cases": len(golden["cases"]),
                      "matrix_digest": golden["matrix_digest"]}, indent=1))
    return 0


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="python3 -m arc_conformance.modern_reference",
                                     description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="cmd")
    sub.required = True
    t = sub.add_parser("tables", help="exp and RoPE table digests and spot values")
    t.add_argument("--theta", type=int, default=ROPE_SMOLLM3["theta"])
    t.add_argument("--d-head", type=int, default=ROPE_SMOLLM3["d_head"])
    t.add_argument("--max-seq", type=int, default=ROPE_SMOLLM3["max_seq"])
    p = sub.add_parser("prepare", help="BF16 safetensors -> integer package")
    p.add_argument("--source-dir", required=True)
    p.add_argument("--source-manifest", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--json-out")
    i = sub.add_parser("inspect", help="validate a package and print its header summary")
    i.add_argument("--package", required=True)
    i.add_argument("--check-tables", action="store_true", help="also recompute the RoPE tables")
    i.add_argument("--tensors", action="store_true", help="list every tensor entry")
    g = sub.add_parser("generate", help="independent generation (slow path, tiny models)")
    g.add_argument("--package", required=True)
    g.add_argument("--cases", required=True)
    g.add_argument("--out", required=True)
    v = sub.add_parser("verify-run", help="teacher-force a run and compare every logits hash and token")
    v.add_argument("--package", required=True)
    v.add_argument("--run", required=True)
    v.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    handlers = {"tables": _cmd_tables, "prepare": _cmd_prepare, "inspect": _cmd_inspect,
                "generate": _cmd_generate, "verify-run": _cmd_verify_run}
    try:
        return handlers[args.cmd](args)
    except (DomainError, PreparationError, PackageError, RunError) as error:
        print(f"refused: {type(error).__name__}: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
