"""Independent reference preparer and executor for arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1.

Written from docs/protocol/integer-profile-mla-moe-dyadic-v1.md ("the spec";
section numbers below refer to it), never translated from the Rust engine, so
bit-for-bit agreement between the two in CI is evidence that the written
profile is complete. The dyadic v1 primitives it builds on (BF16 rules, per-row
INT8 quantisation, exp table, RoPE tables, RMS norm, gated SiLU, selection and
digests) come from ``modern_reference``, which was itself written from the
dyadic v1 document.

* Preparation (spec 4): BF16 safetensors + config.json + a pinned source
  manifest -> a stage package for any layer range, byte for byte, or only its
  digests (``--hash-only``, for checking a 16 GB package without a second
  copy on disk).
* ``SlowEngine``: the forward pass of spec 5 one token at a time in Python
  ints, with the MLA cache (tiny models, independent generation).
* ``FastEngine``: an exact batched teacher-forcing forward with numpy for real
  models. Integer products go through float64 BLAS only when every partial sum
  is provably an integer below 2**53 (otherwise in limbs, as in
  ``modern_reference``); anything outside the proven bounds falls back to
  Python ints. Unit tests check it against ``SlowEngine`` position by position.
* Boundary files (spec 6.3), stage replay (spec 6.4) and run verification.

Run from scripts/:  python3 -m arc_conformance.mla_moe_reference --help
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import struct
import sys
from fractions import Fraction
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Sequence, Tuple

import numpy as np

from . import modern_reference as dy
from .modern_reference import (DomainError, PackageError, PreparationError, RunError, blake3_hex,
                               blake3_raw, canonical_json, check62)

# --------------------------------------------------------------------------
# Identities (spec 1)

PROFILE_ID = "arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1"
PROFILE_BLAKE3 = "7b0bd25616bd29195da71bb0b02d3c436350e801811280def1bb29c22d75207e"
PROFILE_ID_I4G32 = "arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.i4g32-experts.q16.v1"
PROFILE_I4G32_BLAKE3 = "5e6d6392186d817e57806184b1193ea1648805bb82cf7749f9d563306237e71c"
PROFILES = {"i8": PROFILE_ID, "i4g32": PROFILE_ID_I4G32}
FORMATS = {v: k for k, v in PROFILES.items()}
Q4_GROUP = 32
Q4_SPAN = 40
STAGE_SCHEMA = "arc.integer-stage-package.v1"
STAGE_MANIFEST_SCHEMA = "arc.integer-stage-manifest.v1"
BOUNDARY_SCHEMA = "arc.stage-boundary.v1"
RUN_SCHEMA = "arc.mla-run.v1"
STAGE_RUN_SCHEMA = "arc.mla-stage-run.v1"
CASES_SCHEMA = "arc.modern-cases.v1"
STAGE_MAGIC = b"ARCSPKG1"
BOUNDARY_MAGIC = b"ARCBND01"
ALIGN = 64
ONE = 1 << 16
LIM62 = 1 << 62
I32_MIN, I32_MAX = -(1 << 31), (1 << 31) - 1

DTYPES = {"i8": np.dtype("i1"), "u8": np.dtype("u1"), "i16": np.dtype("<i2"), "u16": np.dtype("<u2"),
          "i32": np.dtype("<i4"), "i64": np.dtype("<i8")}


def _align(n: int) -> int:
    return (n + ALIGN - 1) // ALIGN * ALIGN


# --------------------------------------------------------------------------
# Model shape (spec 2)

MODEL_KEYS = ("architecture", "attention_lambda", "d_ff", "d_model", "first_k_dense", "kv_lora_rank",
              "max_seq", "moe_d_ff", "n_experts_per_tok", "n_group", "n_heads", "n_layers",
              "n_routed_experts", "n_shared_experts", "norm_topk_prob", "q_lora_rank", "qk_nope_dim",
              "qk_rope_dim", "rms_eps_q32", "rope_theta", "routed_scaling_q32", "tied_embeddings",
              "topk_group", "v_head_dim", "vocab_size")


def attention_lambda(width: int) -> int:
    """floor(2^30 / sqrt(width)) = isqrt(floor(2^60 / width)) (spec 2.2)."""
    return math.isqrt((1 << 60) // width)


def _rha_double_q32(value: Any, what: str) -> int:
    """rha(v * 2^32) for a JSON number read as the nearest double."""
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise PreparationError(f"{what} must be a finite number, got {value!r}")
    scaled = Fraction(value) * (1 << 32)
    return dy.rha_ratio(scaled.numerator, scaled.denominator)


def model_from_config(config: Dict[str, Any], max_seq: int) -> Dict[str, Any]:
    """The spec 2.2 model object from config.json; unsupported configurations refused (spec 2.1)."""
    def refuse(msg: str) -> None:
        raise PreparationError(f"unsupported config.json: {msg}")

    def integer(key: str, minimum: int = 0) -> int:
        v = config.get(key)
        if isinstance(v, bool) or not isinstance(v, int) or v < minimum:
            refuse(f"{key} = {v!r} is not an integer >= {minimum}")
        return v

    def absent_or(key: str, allowed: Any) -> None:
        if key in config and config[key] is not None and config[key] != allowed:
            refuse(f"{key} must be absent or {allowed!r}")
        if key in config and config[key] is not None and type(config[key]) is not type(allowed):
            refuse(f"{key} must be absent or {allowed!r}")

    arch = config.get("model_type")
    if arch not in ("deepseek_v3", "kimi_k2"):
        refuse(f"model_type {arch!r}")
    if config.get("hidden_act") != "silu":
        refuse("hidden_act must be silu")
    if config.get("scoring_func") != "sigmoid":
        refuse("scoring_func must be sigmoid")
    if config.get("topk_method") != "noaux_tc":
        refuse("topk_method must be noaux_tc")
    absent_or("attention_bias", False)
    absent_or("moe_layer_freq", 1)
    absent_or("num_nextn_predict_layers", 0)
    absent_or("tie_word_embeddings", False)
    absent_or("rope_interleave", True)
    if config.get("rope_scaling") is not None:
        refuse("rope_scaling must be null (YaRN is not in v1)")
    n_heads = integer("num_attention_heads", 1)
    if config.get("num_key_value_heads") is not None and config["num_key_value_heads"] != n_heads:
        refuse("num_key_value_heads must equal num_attention_heads")
    q_lora = config.get("q_lora_rank")
    q_lora = 0 if q_lora is None else integer("q_lora_rank", 1)
    theta = config.get("rope_theta")
    if isinstance(theta, bool) or not isinstance(theta, (int, float)) or not math.isfinite(theta) \
            or float(theta) != math.floor(theta) or not 2 <= theta <= 2 ** 53:
        refuse("rope_theta must be an integer in [2, 2^53]")
    if not isinstance(config.get("norm_topk_prob"), bool):
        refuse("norm_topk_prob must be a boolean")
    limit = config.get("max_position_embeddings")
    if isinstance(limit, int) and not isinstance(limit, bool) and max_seq > limit:
        refuse("max_seq exceeds max_position_embeddings")
    nope, rope = integer("qk_nope_head_dim", 1), integer("qk_rope_head_dim", 1)
    model = {
        "architecture": arch,
        "n_layers": integer("num_hidden_layers", 1),
        "d_model": integer("hidden_size", 1),
        "n_heads": n_heads,
        "q_lora_rank": q_lora,
        "kv_lora_rank": integer("kv_lora_rank", 1),
        "qk_nope_dim": nope,
        "qk_rope_dim": rope,
        "v_head_dim": integer("v_head_dim", 1),
        "d_ff": integer("intermediate_size", 1),
        "first_k_dense": integer("first_k_dense_replace", 0),
        "n_routed_experts": integer("n_routed_experts", 1),
        "n_experts_per_tok": integer("num_experts_per_tok", 1),
        "n_shared_experts": integer("n_shared_experts", 1),
        "moe_d_ff": integer("moe_intermediate_size", 1),
        "n_group": integer("n_group", 1),
        "topk_group": integer("topk_group", 1),
        "norm_topk_prob": config["norm_topk_prob"],
        "routed_scaling_q32": _rha_double_q32(config.get("routed_scaling_factor"), "routed_scaling_factor"),
        "vocab_size": integer("vocab_size", 1),
        "max_seq": max_seq,
        "rms_eps_q32": dy.rms_eps_q32(config.get("rms_norm_eps")),
        "rope_theta": int(theta),
        "attention_lambda": attention_lambda(nope + rope),
        "tied_embeddings": False,
    }
    validate_model(model, error=PreparationError)
    return model


PACKED_WEIGHTS = {"num_bits": 4, "type": "int", "symmetric": True, "strategy": "group",
                  "group_size": 32, "dynamic": False}


def weights_from_config(config: Dict[str, Any], max_seq: int) -> Tuple[Dict[str, Any], str, bool, List[str]]:
    """Spec 14.1-14.2: (model object, tensor prefix, packed experts?, pending preparation features).

    Accepts a multimodal wrapper (text model in text_config, tensors under language_model.),
    routed experts pre-quantised by compressed-tensors pack-quantized INT4 group 32 symmetric,
    and rope_scaling, reported as pending (the weights do not depend on it)."""
    wrapped = isinstance(config.get("text_config"), dict)
    text = dict(config["text_config"] if wrapped else config)
    q = text.pop("quantization_config", None)
    if q is None:
        q = config.get("quantization_config")
    if q is not None:
        groups = q.get("config_groups") if isinstance(q, dict) else None
        group = next(iter(groups.values())) if isinstance(groups, dict) and len(groups) == 1 else None
        w = group.get("weights") if isinstance(group, dict) else None
        ok = (q.get("quant_method") == "compressed-tensors" and q.get("format") == "pack-quantized"
              and q.get("kv_cache_scheme") is None and isinstance(w, dict)
              and group.get("input_activations") is None and group.get("output_activations") is None
              and all(w.get(k) == v and type(w.get(k)) is type(v) for k, v in PACKED_WEIGHTS.items())
              and w.get("actorder") is None)
        if not ok:
            raise PreparationError("unsupported config.json: pre-quantised weights other than "
                                   "compressed-tensors pack-quantized INT4 group-32 symmetric")
    pending: List[str] = []
    scaling = text.get("rope_scaling")
    if scaling is not None:
        kind = scaling.get("type") or scaling.get("rope_type") or "?" if isinstance(scaling, dict) else "?"
        pending.append(f"rope_scaling {kind}")
        text["rope_scaling"] = None
    m = model_from_config(text, max_seq)
    return m, ("language_model." if wrapped else ""), q is not None, pending


def validate_model(model: Any, error: type = PackageError) -> Dict[str, Any]:
    """Spec 2.1 constraints on a model object."""
    if not isinstance(model, dict) or sorted(model) != sorted(MODEL_KEYS):
        raise error("model object has the wrong fields")
    for key in MODEL_KEYS:
        if key in ("architecture", "norm_topk_prob", "tied_embeddings"):
            continue
        v = model[key]
        if isinstance(v, bool) or not isinstance(v, int) or v < 0:
            raise error(f"model.{key} = {v!r} is not a non-negative integer")
    m = model
    positive = ("n_layers", "d_model", "n_heads", "kv_lora_rank", "qk_nope_dim", "qk_rope_dim",
                "v_head_dim", "d_ff", "n_routed_experts", "n_experts_per_tok", "n_shared_experts",
                "moe_d_ff", "n_group", "topk_group", "vocab_size", "max_seq")
    group = m["n_routed_experts"] // max(1, m["n_group"])
    ok = (isinstance(m["architecture"], str) and m["architecture"]
          and all(m[k] > 0 for k in positive)
          and m["qk_rope_dim"] % 2 == 0
          and m["first_k_dense"] <= m["n_layers"]
          and m["n_experts_per_tok"] <= m["n_routed_experts"]
          and m["n_routed_experts"] % m["n_group"] == 0
          and m["topk_group"] <= m["n_group"]
          and (m["n_group"] == 1 or group >= 2)
          and m["n_experts_per_tok"] <= m["topk_group"] * group
          and 1 <= m["routed_scaling_q32"] < 1 << 40
          and m["rms_eps_q32"] >= 1
          and 2 <= m["rope_theta"] <= 1 << 53
          and m["attention_lambda"] == attention_lambda(m["qk_nope_dim"] + m["qk_rope_dim"])
          and isinstance(m["norm_topk_prob"], bool)
          and m["tied_embeddings"] is False)
    if not ok:
        raise error(f"unsupported model shape: {model!r}")
    return model


def is_moe(model: Dict[str, Any], layer: int) -> bool:
    return layer >= model["first_k_dense"]


# --------------------------------------------------------------------------
# Stage layout (spec 4.6) and segments (spec 4.7)

def _dyadic(out: List[Dict[str, Any]], segment: str, name: str, dims: Sequence[int], kind: str,
            source: Any) -> None:
    rows = list(dims[:-1])
    out.append({"name": name + ".q", "dtype": "i8", "shape": list(dims), "segment": segment,
                "kind": kind + ".q", "source": source})
    out.append({"name": name + ".mu", "dtype": "i32", "shape": rows, "segment": segment,
                "kind": kind + ".mu", "source": source})
    out.append({"name": name + ".k", "dtype": "u8", "shape": rows, "segment": segment,
                "kind": kind + ".k", "source": source})


def _int4(out: List[Dict[str, Any]], segment: str, name: str, dims: Sequence[int], source: Any) -> None:
    """Spec 13.1: packed values u8 [n, r, c/2] and BF16 group scales u16 [n, r, c/32]."""
    n, rows, cols = dims
    out.append({"name": name + ".q4", "dtype": "u8", "shape": [n, rows, cols // 2], "segment": segment,
                "kind": "stack4.q4", "source": source})
    out.append({"name": name + ".s", "dtype": "u16", "shape": [n, rows, cols // Q4_GROUP], "segment": segment,
                "kind": "stack4.s", "source": source})


def _vector(out: List[Dict[str, Any]], segment: str, name: str, dtype: str, shape: Sequence[int],
            kind: str, source: Any) -> None:
    out.append({"name": name, "dtype": dtype, "shape": list(shape), "segment": segment,
                "kind": kind, "source": source})


def layer_layout(m: Dict[str, Any], layer: int, fmt: str = "i8") -> List[Dict[str, Any]]:
    """The tensors of one layer in spec 4.6 order, with their sources (spec 4.1)."""
    seg, p, hf = f"layer.{layer}", f"layers.{layer}", f"model.layers.{layer}"
    d, h = m["d_model"], m["n_heads"]
    rank, nope, rope, vh = m["kv_lora_rank"], m["qk_nope_dim"], m["qk_rope_dim"], m["v_head_dim"]
    dq = h * (nope + rope)
    out: List[Dict[str, Any]] = []
    _vector(out, seg, f"{p}.attn_norm", "i64", [d], "norm", f"{hf}.input_layernorm.weight")
    if m["q_lora_rank"] == 0:
        _dyadic(out, seg, f"{p}.wq", [dq, d], "matrix", f"{hf}.self_attn.q_proj.weight")
    else:
        r = m["q_lora_rank"]
        _dyadic(out, seg, f"{p}.wq_a", [r, d], "matrix", f"{hf}.self_attn.q_a_proj.weight")
        _vector(out, seg, f"{p}.q_a_norm", "i64", [r], "norm", f"{hf}.self_attn.q_a_layernorm.weight")
        _dyadic(out, seg, f"{p}.wq_b", [dq, r], "matrix", f"{hf}.self_attn.q_b_proj.weight")
    _dyadic(out, seg, f"{p}.wkv_a", [rank + rope, d], "matrix", f"{hf}.self_attn.kv_a_proj_with_mqa.weight")
    _vector(out, seg, f"{p}.kv_a_norm", "i64", [rank], "norm", f"{hf}.self_attn.kv_a_layernorm.weight")
    _dyadic(out, seg, f"{p}.wk_b", [h, rank, nope], "wk_b", f"{hf}.self_attn.kv_b_proj.weight")
    _dyadic(out, seg, f"{p}.wv_b", [h, vh, rank], "wv_b", f"{hf}.self_attn.kv_b_proj.weight")
    _dyadic(out, seg, f"{p}.wo", [d, h * vh], "matrix", f"{hf}.self_attn.o_proj.weight")
    _vector(out, seg, f"{p}.ffn_norm", "i64", [d], "norm", f"{hf}.post_attention_layernorm.weight")
    if is_moe(m, layer):
        e, fm = m["n_routed_experts"], m["moe_d_ff"]
        sf = m["n_shared_experts"] * fm
        _vector(out, seg, f"{p}.router.q", "i16", [e, d], "router.q", f"{hf}.mlp.gate.weight")
        _vector(out, seg, f"{p}.router.k", "u8", [e], "router.k", f"{hf}.mlp.gate.weight")
        _vector(out, seg, f"{p}.router_bias", "i64", [e], "bias", f"{hf}.mlp.gate.e_score_correction_bias")
        s = f"{hf}.mlp.shared_experts"
        _dyadic(out, seg, f"{p}.shared.w_gate", [sf, d], "matrix", f"{s}.gate_proj.weight")
        _dyadic(out, seg, f"{p}.shared.w_up", [sf, d], "matrix", f"{s}.up_proj.weight")
        _dyadic(out, seg, f"{p}.shared.w_down", [d, sf], "matrix", f"{s}.down_proj.weight")
        for short, proj, dims in (("w_gate", "gate_proj", [e, fm, d]), ("w_up", "up_proj", [e, fm, d]),
                                  ("w_down", "down_proj", [e, d, fm])):
            sources = [f"{hf}.mlp.experts.{x}.{proj}.weight" for x in range(e)]
            if fmt == "i4g32":
                _int4(out, seg, f"{p}.experts.{short}", dims, sources)
            else:
                _dyadic(out, seg, f"{p}.experts.{short}", dims, "stack", sources)
    else:
        f = m["d_ff"]
        _dyadic(out, seg, f"{p}.w_gate", [f, d], "matrix", f"{hf}.mlp.gate_proj.weight")
        _dyadic(out, seg, f"{p}.w_up", [f, d], "matrix", f"{hf}.mlp.up_proj.weight")
        _dyadic(out, seg, f"{p}.w_down", [d, f], "matrix", f"{hf}.mlp.down_proj.weight")
    return out


def stage_layout(m: Dict[str, Any], first: int, end: int, fmt: str = "i8") -> List[Dict[str, Any]]:
    """Every tensor of stage [first, end) in file order, with offsets and byte counts."""
    if not 0 <= first < end <= m["n_layers"]:
        raise PreparationError(f"stage [{first}, {end}) is not a layer range of a {m['n_layers']}-layer model")
    half = m["qk_rope_dim"] // 2
    out: List[Dict[str, Any]] = []
    _vector(out, "tables", "rope.cos", "i32", [m["max_seq"], half], "rope", None)
    _vector(out, "tables", "rope.sin", "i32", [m["max_seq"], half], "rope", None)
    if first == 0:
        _dyadic(out, "embed", "embed", [m["vocab_size"], m["d_model"]], "matrix", "model.embed_tokens.weight")
    if fmt == "i4g32" and (m["d_model"] % Q4_GROUP or m["moe_d_ff"] % Q4_GROUP):
        raise PreparationError("INT4 experts need d_model and moe_d_ff divisible by 32")
    for layer in range(first, end):
        out.extend(layer_layout(m, layer, fmt))
    if end == m["n_layers"]:
        _vector(out, "head", "final_norm", "i64", [m["d_model"]], "norm", "model.norm.weight")
        _dyadic(out, "head", "lm_head", [m["vocab_size"], m["d_model"]], "matrix", "lm_head.weight")
    offset = 0
    for e in out:
        e["bytes"] = int(np.prod(e["shape"], dtype=np.int64)) * DTYPES[e["dtype"]].itemsize
        e["offset"] = offset
        offset = _align(offset + e["bytes"])
    return out


def segment_names(m: Dict[str, Any]) -> List[str]:
    return ["tables", "embed"] + [f"layer.{i}" for i in range(m["n_layers"])] + ["head"]


def build_header(m: Dict[str, Any], source: Dict[str, Any], first: int, end: int,
                 layout: Sequence[Dict[str, Any]], fmt: str = "i8") -> Dict[str, Any]:
    return {"schema": STAGE_SCHEMA, "profile": PROFILES[fmt], "model": m, "source": source,
            "stage": {"first_layer": first, "end_layer": end},
            "tensors": [{"name": e["name"], "dtype": e["dtype"], "shape": list(e["shape"]),
                         "offset": e["offset"], "bytes": e["bytes"]} for e in layout]}


def model_root(m: Dict[str, Any], source: Dict[str, Any], segments: Sequence[Dict[str, Any]],
               fmt: str = "i8") -> str:
    """Spec 4.7: BLAKE3 of the canonical JSON of model, profile, segment digests and source."""
    names = [s["name"] for s in segments]
    if names != segment_names(m):
        raise PackageError("the model root needs every segment, in canonical order")
    return blake3_hex(canonical_json({"model": m, "profile": PROFILES[fmt],
                                      "segments": [[s["name"], s["blake3"]] for s in segments],
                                      "source": source}))


# --------------------------------------------------------------------------
# Preparation (spec 4.1-4.5)

def quantize_router_rows(bits: np.ndarray, what: str = "router") -> Tuple[np.ndarray, np.ndarray]:
    """Spec 4.3, vectorised: (q int16 [rows, cols], k uint8 [rows])."""
    b = np.asarray(bits).astype(np.int64)
    if b.ndim != 2 or b.shape[1] == 0:
        raise PreparationError(f"{what}: expected a non-empty matrix")
    if np.any((b & 0x7F80) == 0x7F80):
        raise PreparationError(f"{what}: contains infinity or NaN")
    big_e = (b >> 7) & 0xFF
    m = b & 0x7F
    mant = np.where(big_e == 0, m, m + 128)
    expo = np.where(big_e == 0, -133, big_e - 134)
    mags = b & 0x7FFF
    rows = np.arange(b.shape[0])
    j_max = np.argmax(mags, axis=1)
    zero_row = mags[rows, j_max] == 0
    e_a = expo[rows, j_max]
    k = 7 - e_a
    bad = ~zero_row & ((k < 0) | (k > 62))
    if np.any(bad):
        row = int(np.nonzero(bad)[0][0])
        raise PreparationError(f"{what}: row {row} needs scale shift {int(k[row])} outside [0, 62]")
    shift = expo + np.where(zero_row, 0, k)[:, None]
    up = np.left_shift(mant, np.clip(shift, 0, 7))
    c = np.clip(-shift, 1, 16)  # c >= 9 rounds to 0 anyway (spec 4.3)
    down = (2 * mant + np.left_shift(1, c)) >> (c + 1)
    mag = np.where(shift >= 0, up, down)
    mag = np.where(mant == 0, 0, mag)
    q = np.where((b >> 15) & 1 == 1, -mag, mag)
    q[zero_row] = 0
    if np.any(np.abs(q) > 32767):
        raise AssertionError("router value beyond int16")
    k = np.where(zero_row, 16, k)
    return q.astype(np.int16), k.astype(np.uint8)


def quantize_router_row_exact(bits: Sequence[int]) -> Tuple[List[int], int]:
    """Spec 4.3 literally, one row in Python ints (test oracle)."""
    parts = [dy.bf16_parts(int(x)) for x in bits]
    mags = [int(x) & 0x7FFF for x in bits]
    if max(mags) == 0:
        return [0] * len(bits), 16
    _, _, e_a = parts[mags.index(max(mags))]
    k = 7 - e_a
    if not 0 <= k <= 62:
        raise PreparationError(f"router row scale shift {k} outside [0, 62]")
    out = []
    for s, mant, e in parts:
        shift = e + k
        if mant == 0:
            v = 0
        elif shift >= 0:
            v = mant << shift
        else:
            c = -shift
            v = (2 * mant + (1 << c)) >> (c + 1)
        out.append(-v if s else v)
    return out, k


def pack_q4(values: np.ndarray) -> np.ndarray:
    """Spec 13.1: signed 4-bit values [rows, cols] -> bytes [rows, cols/2], low nibble first."""
    v = np.asarray(values).astype(np.int64)
    return ((v[:, 0::2] & 0x0F) | ((v[:, 1::2] & 0x0F) << 4)).astype(np.uint8)


def unpack_q4(packed: np.ndarray) -> np.ndarray:
    """Bytes [rows, cols/2] -> signed values [rows, cols] (int64)."""
    b = np.asarray(packed).astype(np.int64)
    lo, hi = b & 0x0F, b >> 4
    out = np.empty((b.shape[0], 2 * b.shape[1]), dtype=np.int64)
    out[:, 0::2] = np.where(lo > 7, lo - 16, lo)
    out[:, 1::2] = np.where(hi > 7, hi - 16, hi)
    return out


def quantize_q4_rows(bits: np.ndarray, what: str = "experts") -> Tuple[np.ndarray, np.ndarray]:
    """Spec 13.3, vectorised: BF16 [rows, cols] -> (packed u8 [rows, cols/2], scales u16 [rows, cols/32])."""
    b = np.asarray(bits).astype(np.int64)
    rows, cols = b.shape
    if cols % Q4_GROUP:
        raise PreparationError(f"{what}: {cols} inputs are not a multiple of 32")
    if np.any((b & 0x7F80) == 0x7F80):
        raise PreparationError(f"{what}: contains infinity or NaN")
    g = b.reshape(rows, cols // Q4_GROUP, Q4_GROUP)
    big_e, low = (g >> 7) & 0xFF, g & 0x7F
    mant = np.where(big_e == 0, low, low + 128)
    expo = np.where(big_e == 0, -133, big_e - 134)
    mags = g & 0x7FFF
    j_max = np.argmax(mags, axis=2)[..., None]
    zero = np.take_along_axis(mags, j_max, 2)[..., 0] == 0
    m_a = np.where(zero, 255, np.take_along_axis(mant, j_max, 2)[..., 0])
    e_a = np.take_along_axis(expo, j_max, 2)[..., 0]
    d = np.zeros_like(m_a)
    for _ in range(11):
        d = d + ((m_a << d) < 896)
    f = e_a - d
    m = (2 * (m_a << d) + 7) // 14
    over = m == 256
    m, f = np.where(over, 128, m), np.where(over, f + 1, f)
    field = f + 134
    bad = ~zero & ((field < 1) | (field > 254))
    if np.any(bad):
        raise PreparationError(f"{what}: an INT4 group scale exponent field is outside [1, 254]")
    scales = np.where(zero, 0, (field << 7) | (m - 128))
    shift = expo - f[..., None]
    pos = shift >= 0
    num = np.where(pos, mant << np.clip(shift, 0, 30), mant)
    den = np.where(pos, m[..., None], m[..., None] << np.clip(-shift, 0, Q4_SPAN))
    q = (2 * num + den) // (2 * den)
    q = np.where((mant == 0) | (-shift > Q4_SPAN), 0, q)
    q = np.where(((g >> 15) & 1) == 1, -q, q)
    q = np.clip(q, -8, 7)
    q[zero] = 0
    return pack_q4(q.reshape(rows, cols)), scales.astype(np.uint16)


def quantize_q4_group_exact(bits: Sequence[int]) -> Tuple[int, List[int]]:
    """Spec 13.3 literally with rationals, one group of 32 (test oracle): (scale bits, values)."""
    values = [dy.bf16_value(int(x)) for x in bits]
    a = max(abs(v) for v in values)
    if a == 0:
        return 0, [0] * len(values)
    target = a / 7
    f = 0
    while target / Fraction(2) ** f >= 256:
        f += 1
    while target / Fraction(2) ** f < 128:
        f -= 1
    ratio = target / Fraction(2) ** f
    m = dy.rha_ratio(ratio.numerator, ratio.denominator)
    if m == 256:
        m, f = 128, f + 1
    field = f + 134
    if not 1 <= field <= 254:
        raise PreparationError("INT4 group scale exponent field outside [1, 254]")
    scale = Fraction(m) * Fraction(2) ** f
    out = []
    for v in values:
        r = v / scale
        if v == 0 or abs(r) < Fraction(1, 2 ** Q4_SPAN):
            out.append(0)
            continue
        out.append(max(-8, min(7, dy.rha_ratio(r.numerator, r.denominator))))
    return (field << 7) | (m - 128), out


def q4_project_int(x: Sequence[int], values: Sequence[Sequence[int]], scales: Sequence[Sequence[int]],
                   what: str = "int4 projection") -> List[int]:
    """Spec 13.2 in Python ints: one exact scaled sum per row, one floor."""
    if 8 * sum(abs(v) for v in x) >= 1 << 63:
        raise DomainError(f"{what}: 8 * sum|x| >= 2^63")
    out = []
    for q, row_scales in zip(values, scales):
        parts = [dy.bf16_parts(int(bits)) for bits in row_scales]
        live = [e for _, mant, e in parts if mant > 0]
        if not live:
            out.append(0)
            continue
        top = max(live)
        total = 0
        for g, (_, mant, e) in enumerate(parts):
            if mant == 0 or e < top - Q4_SPAN:
                continue
            acc = sum(q[j] * x[j] for j in range(g * Q4_GROUP, (g + 1) * Q4_GROUP))
            total += (mant * acc) << (e - top + Q4_SPAN)
        shift = top - Q4_SPAN
        out.append(check62(total >> -shift if shift < 0 else total << shift, what))
    return out


def f32_parts(bits: int) -> Tuple[int, int, int]:
    """(s, M, e) of an IEEE binary32 pattern with value (-1)^s M 2^e (spec 4.4)."""
    s, big_e, m = bits >> 31, (bits >> 23) & 0xFF, bits & 0x7FFFFF
    if big_e == 255:
        raise PreparationError(f"F32 pattern {bits:#010x} is infinity or NaN")
    if big_e == 0:
        return s, m, -149
    return s, (1 << 23) + m, big_e - 150


def bias_q32(s: int, mant: int, e: int) -> int:
    """rha(v * 2^32) exactly, refusing |b| > 2^62 (spec 4.4)."""
    shift = e + 32
    if mant == 0:
        mag = 0
    elif shift >= 0:
        mag = mant << shift
    else:
        c = -shift
        mag = (2 * mant + (1 << c)) >> (c + 1)
    if mag > LIM62:
        raise DomainError("correction bias beyond 2^62")
    return -mag if s else mag


class SourceShard:
    """A safetensors shard (BF16 and F32 tensors)."""

    def __init__(self, path: Path):
        self.path = Path(path)
        size = self.path.stat().st_size
        with open(self.path, "rb") as f:
            raw = f.read(8)
            if len(raw) != 8:
                raise PreparationError(f"{self.path.name}: truncated safetensors header")
            (n,) = struct.unpack("<Q", raw)
            if n > size - 8:
                raise PreparationError(f"{self.path.name}: header length exceeds the file")
            header = json.loads(f.read(n))
        self.data_start = 8 + n
        self.tensors: Dict[str, Dict[str, Any]] = {}
        for name, info in header.items():
            if name == "__metadata__":
                continue
            begin, end = info["data_offsets"]
            if not 0 <= begin <= end or self.data_start + end > size:
                raise PreparationError(f"{self.path.name}: {name} lies outside the file")
            self.tensors[name] = {"dtype": info["dtype"], "shape": tuple(info["shape"]),
                                  "begin": begin, "end": end}

    def array(self, name: str, dtype: str) -> np.ndarray:
        info = self.tensors[name]
        count = int(np.prod(info["shape"], dtype=np.int64))
        width = {"<u2": 2, "<u4": 4}[dtype]
        if info["end"] - info["begin"] != width * count:
            raise PreparationError(f"{name}: byte range does not match its shape")
        return np.memmap(self.path, dtype=dtype, mode="r", offset=self.data_start + info["begin"],
                         shape=info["shape"])


def _ignored(name: str, prefix: str = "") -> bool:
    """Rotary buffers (spec 4.1); in a wrapped checkpoint also the vision tower (spec 14.1)."""
    rest = name[len(prefix):] if name.startswith(prefix) else name
    if rest.startswith("model.layers.") and rest.endswith(".self_attn.rotary_emb.inv_freq"):
        return True
    return bool(prefix) and name.startswith(("vision_tower.", "mm_projector."))


def expected_sources(m: Dict[str, Any], prefix: str = "",
                     packed: bool = False) -> Dict[str, Tuple[Tuple[int, ...], str]]:
    """Every source tensor of the whole model (spec 4.1, 14.1), without the prefix:
    name -> (shape, 'bf16' | 'bias' | 'packed' | 'shape')."""
    out = _expected_sources(m)
    if packed:
        for name in [n for n in out if ".mlp.experts." in n]:
            (rows, cols), _ = out.pop(name)
            base = name[: -len(".weight")]
            out[base + ".weight_packed"] = ((rows, cols // 8), "packed")
            out[base + ".weight_scale"] = ((rows, cols // Q4_GROUP), "bf16")
            out[base + ".weight_shape"] = ((2,), "shape")
    return out


def _expected_sources(m: Dict[str, Any]) -> Dict[str, Tuple[Tuple[int, ...], str]]:
    d, h = m["d_model"], m["n_heads"]
    out: Dict[str, Tuple[Tuple[int, ...], str]] = {
        "model.embed_tokens.weight": ((m["vocab_size"], d), "bf16"),
        "model.norm.weight": ((d,), "bf16"),
        "lm_head.weight": ((m["vocab_size"], d), "bf16"),
    }
    for layer in range(m["n_layers"]):
        p = f"model.layers.{layer}"
        out[f"{p}.input_layernorm.weight"] = ((d,), "bf16")
        out[f"{p}.post_attention_layernorm.weight"] = ((d,), "bf16")
        dq = h * (m["qk_nope_dim"] + m["qk_rope_dim"])
        if m["q_lora_rank"] == 0:
            out[f"{p}.self_attn.q_proj.weight"] = ((dq, d), "bf16")
        else:
            r = m["q_lora_rank"]
            out[f"{p}.self_attn.q_a_proj.weight"] = ((r, d), "bf16")
            out[f"{p}.self_attn.q_a_layernorm.weight"] = ((r,), "bf16")
            out[f"{p}.self_attn.q_b_proj.weight"] = ((dq, r), "bf16")
        out[f"{p}.self_attn.kv_a_proj_with_mqa.weight"] = ((m["kv_lora_rank"] + m["qk_rope_dim"], d), "bf16")
        out[f"{p}.self_attn.kv_a_layernorm.weight"] = ((m["kv_lora_rank"],), "bf16")
        out[f"{p}.self_attn.kv_b_proj.weight"] = ((h * (m["qk_nope_dim"] + m["v_head_dim"]),
                                                   m["kv_lora_rank"]), "bf16")
        out[f"{p}.self_attn.o_proj.weight"] = ((d, h * m["v_head_dim"]), "bf16")
        if is_moe(m, layer):
            e, fm = m["n_routed_experts"], m["moe_d_ff"]
            sf = m["n_shared_experts"] * fm
            out[f"{p}.mlp.gate.weight"] = ((e, d), "bf16")
            out[f"{p}.mlp.gate.e_score_correction_bias"] = ((e,), "bias")
            out[f"{p}.mlp.shared_experts.gate_proj.weight"] = ((sf, d), "bf16")
            out[f"{p}.mlp.shared_experts.up_proj.weight"] = ((sf, d), "bf16")
            out[f"{p}.mlp.shared_experts.down_proj.weight"] = ((d, sf), "bf16")
            for x in range(e):
                out[f"{p}.mlp.experts.{x}.gate_proj.weight"] = ((fm, d), "bf16")
                out[f"{p}.mlp.experts.{x}.up_proj.weight"] = ((fm, d), "bf16")
                out[f"{p}.mlp.experts.{x}.down_proj.weight"] = ((d, fm), "bf16")
        else:
            out[f"{p}.mlp.gate_proj.weight"] = ((m["d_ff"], d), "bf16")
            out[f"{p}.mlp.up_proj.weight"] = ((m["d_ff"], d), "bf16")
            out[f"{p}.mlp.down_proj.weight"] = ((d, m["d_ff"]), "bf16")
    return out


class SourceTensors:
    """The verified shards present in a source directory (spec 4.8)."""

    def __init__(self, source_dir: Path, manifest: Dict[str, Any], m: Dict[str, Any],
                 prefix: str = "", packed: bool = False):
        expected = expected_sources(m, prefix, packed)
        self.prefix, self.packed = prefix, packed
        self.found: Dict[str, SourceShard] = {}
        self.shards_read: List[str] = []
        for entry in manifest["files"]:
            if not entry["name"].endswith(".safetensors"):
                continue
            path = Path(source_dir) / entry["name"]
            if not path.exists():
                continue
            _verify_file(path, entry)
            shard = SourceShard(path)
            self.shards_read.append(entry["name"])
            for full_name, info in shard.tensors.items():
                if _ignored(full_name, prefix):
                    continue
                if not full_name.startswith(prefix) or full_name[len(prefix):] not in expected:
                    raise PreparationError(f"unexpected source tensor {full_name}")
                name = full_name[len(prefix):]
                shape, kind = expected[name]
                if kind in ("packed", "shape"):
                    ok = info["dtype"] == "I32"
                else:
                    ok = info["dtype"] == "BF16" or (kind == "bias" and info["dtype"] == "F32")
                if not ok or info["shape"] != shape:
                    raise PreparationError(f"{name}: {info['dtype']} {list(info['shape'])}, "
                                           f"need {kind} {list(shape)}")
                if name in self.found:
                    raise PreparationError(f"tensor {name} appears in two shards")
                self.found[name] = shard

    def _shard(self, name: str) -> Tuple[SourceShard, str]:
        if name not in self.found:
            raise PreparationError(f"source tensor {self.prefix}{name} is not in any shard present")
        return self.found[name], self.prefix + name

    def bf16(self, name: str) -> np.ndarray:
        shard, full = self._shard(name)
        return shard.array(full, "<u2")

    def i32(self, name: str) -> np.ndarray:
        shard, full = self._shard(name)
        return shard.array(full, "<u4").view(np.int32)

    def bias(self, name: str) -> List[int]:
        shard, full = self._shard(name)
        if shard.tensors[full]["dtype"] == "F32":
            return [bias_q32(*f32_parts(int(x))) for x in np.asarray(shard.array(full, "<u4")).tolist()]
        return [bias_q32(*dy.bf16_parts(int(x))) for x in np.asarray(shard.array(full, "<u2")).tolist()]

    def packed_values(self, base: str, rows: int, cols: int) -> np.ndarray:
        """Spec 14.2: the INT4 values of a compressed-tensors matrix, value j of a row at
        bits 4(j mod 8) of word j/8, stored as v + 8; int64 [rows, cols] in [-8, 7]."""
        shape = [int(x) for x in np.asarray(self.i32(base + ".weight_shape")).tolist()]
        if shape != [rows, cols]:
            raise PreparationError(f"{base}.weight_shape is {shape}, need {[rows, cols]}")
        words = np.asarray(self.i32(base + ".weight_packed")).astype(np.int64) & 0xFFFFFFFF
        values = np.empty((rows, cols), dtype=np.int64)
        for i in range(8):
            values[:, i::8] = ((words >> (4 * i)) & 0xF) - 8
        return values

    def packed_scales(self, base: str) -> np.ndarray:
        """Spec 13.1 / 14.2: BF16 group scales, sign bit clear, finite."""
        scales = np.asarray(self.bf16(base + ".weight_scale")).astype(np.int64)
        if np.any(scales >> 15) or np.any((scales & 0x7F80) == 0x7F80):
            raise PreparationError(f"{base}: a group scale is negative, infinite or NaN")
        return scales


def _verify_file(path: Path, entry: Dict[str, Any]) -> None:
    size = path.stat().st_size
    if size != entry["bytes"]:
        raise PreparationError(f"{entry['name']}: {size} bytes, manifest pins {entry['bytes']}")
    _, digest = dy.sha256_file(path)
    if digest != entry["sha256"]:
        raise PreparationError(f"{entry['name']}: sha256 {digest} != pinned {entry['sha256']}")


class _StageWriter:
    """Streams a stage package (or only its digests) with file and segment hashes."""

    def __init__(self, f, header_bytes: bytes):
        self.f = f
        self.sha = hashlib.sha256()
        self.b3 = dy._blake3_ctor()()
        self.size = 0
        self.segments: List[Dict[str, Any]] = []
        self._seg: Optional[Tuple[str, Any, int]] = None
        self.write(STAGE_MAGIC)
        self.write(struct.pack("<Q", len(header_bytes)))
        self.write(header_bytes)
        self.pad()
        self.data_start = self.size

    def write(self, data: bytes, segment: Optional[str] = None) -> None:
        if not data:
            return
        if self.f is not None:
            self.f.write(data)
        self.sha.update(data)
        self.b3.update(data)
        self.size += len(data)
        if segment is not None:
            if self._seg is None or self._seg[0] != segment:
                self._close()
                self._seg = (segment, dy._blake3_ctor()(), 0)
            name, hasher, count = self._seg
            hasher.update(data)
            self._seg = (name, hasher, count + len(data))

    def _close(self) -> None:
        if self._seg is not None:
            name, hasher, count = self._seg
            self.segments.append({"name": name, "bytes": count, "blake3": hasher.hexdigest()})
            self._seg = None

    def pad(self) -> None:
        if self.size % ALIGN:
            zeros = b"\x00" * (_align(self.size) - self.size)
            if self.f is not None:
                self.f.write(zeros)
            self.sha.update(zeros)
            self.b3.update(zeros)
            self.size += len(zeros)

    def finish(self) -> Dict[str, Any]:
        self._close()
        self.pad()
        return {"bytes": self.size, "sha256": self.sha.hexdigest(), "blake3": self.b3.hexdigest(),
                "segments": self.segments}


def _rows_chunk(cols: int) -> int:
    return max(1, (1 << 20) // max(1, cols))


def _write_quantized(write: Callable[[bytes], None], bits: np.ndarray, what: str,
                     mus: List[np.ndarray], ks: List[np.ndarray]) -> None:
    rows, cols = bits.shape
    step = _rows_chunk(cols)
    for r0 in range(0, rows, step):
        q, mu, k = dy.quantize_rows(np.asarray(bits[r0:r0 + step]), what=what)
        write(q.tobytes())
        mus.append(mu)
        ks.append(k)


def _kv_b_blocks(bits: np.ndarray, m: Dict[str, Any]) -> Tuple[np.ndarray, np.ndarray]:
    """Spec 4.1: kv_b_proj [H(N+Vh), C] -> wk_b rows [H*C, N] (per-head transpose) and wv_b rows [H*Vh, C]."""
    h, nope, vh, rank = m["n_heads"], m["qk_nope_dim"], m["v_head_dim"], m["kv_lora_rank"]
    b = np.asarray(bits).reshape(h, nope + vh, rank)
    key = np.ascontiguousarray(b[:, :nope, :].transpose(0, 2, 1)).reshape(h * rank, nope)
    value = np.ascontiguousarray(b[:, nope:, :]).reshape(h * vh, rank)
    return key, value


def _emit(e: Dict[str, Any], tensors: "SourceTensors", m: Dict[str, Any],
          write: Callable[[bytes], None], state: Dict[str, Any]) -> None:
    """Write the bytes of layout entry e (spec 4.1-4.5, 13, 14.2) in order through write."""
    kind = e["kind"]
    pending: Dict[str, np.ndarray] = state.setdefault("pending", {})
    kv_cache: Dict[str, Tuple[np.ndarray, np.ndarray]] = state.setdefault("kv_cache", {})
    if kind == "rope":
        if state.get("rope") is None:
            state["rope"] = dy.build_rope_tables(m["rope_theta"], m["qk_rope_dim"], m["max_seq"])
        table = state["rope"][0] if e["name"] == "rope.cos" else state["rope"][1]
        write(table.astype("<i4").tobytes())
    elif kind == "norm":
        gains = [dy.norm_gain(int(x)) for x in np.asarray(tensors.bf16(e["source"])).tolist()]
        for g in gains:
            check62(g, e["name"])
        write(np.array(gains, dtype="<i8").tobytes())
    elif kind in ("matrix.q", "stack.q", "wk_b.q", "wv_b.q"):
        mus: List[np.ndarray] = []
        ks: List[np.ndarray] = []
        if kind == "matrix.q":
            _write_quantized(write, tensors.bf16(e["source"]), e["name"], mus, ks)
        elif kind == "stack.q":
            for source_name in e["source"]:
                _write_quantized(write, tensors.bf16(source_name), source_name, mus, ks)
        else:
            if e["source"] not in kv_cache:
                kv_cache.clear()
                kv_cache[e["source"]] = _kv_b_blocks(tensors.bf16(e["source"]), m)
            block = kv_cache[e["source"]][0 if kind == "wk_b.q" else 1]
            _write_quantized(write, block, e["name"], mus, ks)
        pending["mu"] = np.concatenate(mus)
        pending["k"] = np.concatenate(ks)
    elif kind.endswith(".mu"):
        write(pending.pop("mu").astype("<i4").tobytes())
    elif kind.endswith(".k") and kind != "router.k":
        write(pending.pop("k").astype("u1").tobytes())
    elif kind == "stack4.q4" and tensors.packed:
        # Spec 14.2: the checkpoint's own INT4 values, repacked into the 13.1 order.
        rows, cols = e["shape"][1], 2 * e["shape"][2]
        for source_name in e["source"]:
            write(pack_q4(tensors.packed_values(source_name[: -len(".weight")], rows, cols)).tobytes())
    elif kind == "stack4.s" and tensors.packed:
        for source_name in e["source"]:
            write(tensors.packed_scales(source_name[: -len(".weight")]).astype("<u2").tobytes())
    elif kind == "stack4.q4":
        groups: List[np.ndarray] = []
        for source_name in e["source"]:
            bits = tensors.bf16(source_name)
            step = _rows_chunk(bits.shape[1])
            for r0 in range(0, bits.shape[0], step):
                packed, scales = quantize_q4_rows(np.asarray(bits[r0:r0 + step]), what=source_name)
                write(packed.tobytes())
                groups.append(scales)
        pending["s"] = np.concatenate(groups)
    elif kind == "stack4.s":
        write(pending.pop("s").astype("<u2").tobytes())
    elif kind == "router.q":
        q, k = quantize_router_rows(np.asarray(tensors.bf16(e["source"])), what=e["name"])
        write(q.astype("<i2").tobytes())
        pending["router_k"] = k
    elif kind == "router.k":
        write(pending.pop("router_k").astype("u1").tobytes())
    elif kind == "bias":
        write(np.array(tensors.bias(e["source"]), dtype="<i8").tobytes())
    else:  # pragma: no cover
        raise AssertionError(kind)


def prepare_stage(source_dir: Path, manifest_path: Path, first: Optional[int], end: Optional[int],
                  out_path: Optional[Path], experts: str = "i8") -> Dict[str, Any]:
    """Convert stage [first, end) (whole model when None) to a package; out_path None = digests only."""
    if experts not in PROFILES:
        raise PreparationError(f"unknown expert format {experts!r}")
    source_dir = Path(source_dir)
    manifest = dy.load_source_manifest(Path(manifest_path))
    config_entry = next(e for e in manifest["files"] if e["name"] == "config.json")
    _verify_file(source_dir / "config.json", config_entry)
    config = json.loads((source_dir / "config.json").read_bytes())
    m, prefix, packed, pending = weights_from_config(config, manifest["max_seq"])
    if pending:
        raise PreparationError(f"unsupported config.json: {'; '.join(pending)}")
    if packed and experts != "i4g32":
        raise PreparationError("pre-quantised INT4 experts are stored as i4g32, never requantised")
    first = 0 if first is None else first
    end = m["n_layers"] if end is None else end
    layout = stage_layout(m, first, end, experts)
    tensors = SourceTensors(source_dir, manifest, m, prefix, packed)
    source = {"repo": manifest["repo"], "revision": manifest["revision"],
              "files": [{"name": e["name"], "bytes": e["bytes"], "sha256": e["sha256"]}
                        for e in manifest["files"]]}
    header = build_header(m, source, first, end, layout, experts)
    header_bytes = canonical_json(header)
    tmp = None if out_path is None else Path(out_path).with_name(Path(out_path).name + ".partial")
    f = open(tmp, "wb") if tmp is not None else None
    try:
        w = _StageWriter(f, header_bytes)
        state: Dict[str, Any] = {}
        for e in layout:
            if w.size - w.data_start != e["offset"]:
                raise AssertionError(f"layout drift at {e['name']}")
            start = w.size
            _emit(e, tensors, m, lambda data, seg=e["segment"]: w.write(data, seg), state)
            if w.size - start != e["bytes"]:
                raise AssertionError(f"{e['name']}: wrote {w.size - start} bytes, layout needs {e['bytes']}")
            w.pad()
        ident = w.finish()
    finally:
        if f is not None:
            f.close()
    if tmp is not None:
        os.replace(tmp, out_path)
    ident.update({"stage": {"first_layer": first, "end_layer": end}, "shards_read": tensors.shards_read,
                  "profile": PROFILES[experts]})
    if (first, end) == (0, m["n_layers"]):
        ident["model_root"] = model_root(m, source, ident["segments"], experts)
    return ident


# --------------------------------------------------------------------------
# Weight slices (spec 14)

SLICE_MANIFEST_SCHEMA = "arc.integer-slice-manifest.v1"
CONTRACT = "docs/protocol/integer-profile-mla-moe-dyadic-v1.md"
SHAPE_KEYS = ("architecture", "n_layers", "d_model", "n_heads", "q_lora_rank", "kv_lora_rank", "qk_nope_dim",
              "qk_rope_dim", "v_head_dim", "d_ff", "first_k_dense", "n_routed_experts", "n_shared_experts",
              "moe_d_ff", "vocab_size")


def unit_layout(m: Dict[str, Any], unit: str, fmt: str) -> List[Dict[str, Any]]:
    """The layout entries of one segment other than tables (spec 14.1)."""
    if unit == "embed":
        first = 0
    elif unit == "head":
        first = m["n_layers"] - 1
    else:
        first = int(unit.split(".")[1])
    return [e for e in stage_layout(m, first, first + 1, fmt) if e["segment"] == unit]


def slice_unit(m: Dict[str, Any], unit: str, fmt: str, groups: int, tensors: "SourceTensors") -> Dict[str, Any]:
    """Spec 14.1: the segment digest and the slices (name, BLAKE3, bytes, experts, tensors) of a unit."""
    entries = unit_layout(m, unit, fmt)
    moe = unit.startswith("layer.") and is_moe(m, int(unit.split(".")[1]))
    e_count = m["n_routed_experts"]
    expert_prefix = f"layers.{unit.split('.')[1]}.experts." if moe else None
    slices = [{"name": f"{unit}.core" if moe else unit, "experts": None}]
    if moe:
        slices += [{"name": f"{unit}.experts.{g}", "experts": [g * e_count // groups, (g + 1) * e_count // groups]}
                   for g in range(groups)]
    for sl in slices:
        sl.update({"segment": unit, "hasher": dy._blake3_ctor()(), "bytes": 0, "tensors": []})
    segment = dy._blake3_ctor()()
    seg_bytes = 0
    state: Dict[str, Any] = {}
    for e in entries:
        split = moe and e["name"].startswith(expert_prefix)
        targets = slices[1:] if split else slices[:1]
        part = e["bytes"] // len(targets)
        for sl in targets:
            shape = list(e["shape"])
            if split:
                shape[0] //= groups
            sl["tensors"].append({"name": e["name"], "dtype": e["dtype"], "shape": shape,
                                  "offset": sl["bytes"], "bytes": part})
        written = [0]

        def write(data: bytes, e=e, split=split, targets=targets, part=part, written=written) -> None:
            nonlocal seg_bytes
            segment.update(data)
            seg_bytes += len(data)
            view = memoryview(data)
            while len(view):
                pos = written[0]
                g = pos // part if split else 0
                take = min(len(view), (g + 1) * part - pos)
                targets[g]["hasher"].update(view[:take])
                targets[g]["bytes"] += take
                written[0] += take
                view = view[take:]

        _emit(e, tensors, m, write, state)
        if written[0] != e["bytes"]:
            raise AssertionError(f"{e['name']}: wrote {written[0]} bytes, layout needs {e['bytes']}")
    out = []
    for sl in slices:
        out.append({"name": sl["name"], "segment": unit, "blake3": sl["hasher"].hexdigest(), "bytes": sl["bytes"],
                    "experts": sl["experts"], "tensors": sl["tensors"]})
    return {"segment": {"name": unit, "bytes": seg_bytes, "blake3": segment.hexdigest()}, "slices": out}


def prepare_slices(source_dir: Path, manifest_path: Path, units: Optional[Sequence[str]], groups: int,
                   experts: Optional[str] = None) -> Dict[str, Any]:
    """Spec 14.3: the slice manifest of `units` (every unit when None), computed without writing slices."""
    source_dir = Path(source_dir)
    manifest = dy.load_source_manifest(Path(manifest_path))
    config_entry = next(e for e in manifest["files"] if e["name"] == "config.json")
    _verify_file(source_dir / "config.json", config_entry)
    config = json.loads((source_dir / "config.json").read_bytes())
    m, prefix, packed, pending = weights_from_config(config, manifest["max_seq"])
    fmt = experts or ("i4g32" if packed else "i8")
    if packed and fmt != "i4g32":
        raise PreparationError("pre-quantised INT4 experts are stored as i4g32, never requantised")
    if groups < 1 or m["n_routed_experts"] % groups:
        raise PreparationError(f"{groups} expert groups do not divide {m['n_routed_experts']} experts")
    every = ["embed"] + [f"layer.{i}" for i in range(m["n_layers"])] + ["head"]
    chosen = every if units is None else [u for u in every if u in set(units)]
    if units is not None and len(chosen) != len(set(units)):
        raise PreparationError(f"unknown units in {list(units)}")
    tensors = SourceTensors(source_dir, manifest, m, prefix, packed)
    records = [slice_unit(m, u, fmt, groups, tensors) for u in chosen]
    source = {"repo": manifest["repo"], "revision": manifest["revision"],
              "files": [{"name": e["name"], "bytes": e["bytes"], "sha256": e["sha256"]}
                        for e in manifest["files"]]}
    complete = len(chosen) == len(every)
    tables = None
    root = None
    if not pending:
        cos, sin = dy.build_rope_tables(m["rope_theta"], m["qk_rope_dim"], m["max_seq"])
        data = cos.astype("<i4").tobytes() + sin.astype("<i4").tobytes()
        tables = {"name": "tables", "bytes": len(data), "blake3": blake3_hex(data)}
        if complete:
            root = model_root(m, source, [tables] + [r["segment"] for r in records], fmt)
    out = {
        "schema": SLICE_MANIFEST_SCHEMA,
        "profile": PROFILES[fmt],
        "profile_blake3": blake3_hex(PROFILES[fmt].encode()),
        "contract": CONTRACT,
        "source": source,
        "weights": {"prefix": prefix, "packed_experts": packed},
        "shape": {k: m[k] for k in SHAPE_KEYS},
        "model": None if pending else m,
        "pending": pending,
        "expert_groups": groups,
        "complete": complete,
        "tables": tables,
        "segments": [r["segment"] for r in records],
        "slices": [s for r in records for s in r["slices"]],
        "model_root": root,
    }
    out["manifest_blake3"] = blake3_hex(canonical_json(out))
    out["shards_read"] = tensors.shards_read
    return out


# --------------------------------------------------------------------------
# Stage package reader (spec 4.6)

class StagePackage:
    """A validated stage package with memory-mapped tensors."""

    def __init__(self, path: Path, check_values: bool = True):
        self.path = Path(path)
        self.size = self.path.stat().st_size
        with open(self.path, "rb") as f:
            head = f.read(16)
            if len(head) != 16 or head[:8] != STAGE_MAGIC:
                raise PackageError("not an ARCSPKG1 file")
            (hlen,) = struct.unpack("<Q", head[8:])
            if hlen > self.size - 16:
                raise PackageError("header length exceeds the file")
            raw = f.read(hlen)
            header = json.loads(raw.decode("ascii"), parse_float=dy._reject_float,
                                parse_constant=dy._reject_float)
            if canonical_json(header) != raw:
                raise PackageError("header is not canonical JSON")
            if not isinstance(header, dict) or sorted(header) != ["model", "profile", "schema", "source",
                                                                   "stage", "tensors"]:
                raise PackageError("header has the wrong top-level fields")
            if header["schema"] != STAGE_SCHEMA or header["profile"] not in FORMATS:
                raise PackageError("schema or profile mismatch")
            self.fmt = FORMATS[header["profile"]]
            self.profile = header["profile"]
            self.model = validate_model(header["model"])
            stage = header["stage"]
            if not isinstance(stage, dict) or sorted(stage) != ["end_layer", "first_layer"]:
                raise PackageError("header 'stage' is malformed")
            self.first, self.end = stage["first_layer"], stage["end_layer"]
            layout = stage_layout(self.model, self.first, self.end, self.fmt)
            expected = build_header(self.model, header["source"], self.first, self.end, layout,
                                    self.fmt)["tensors"]
            if header["tensors"] != expected:
                raise PackageError("tensor table differs from the canonical layout")
            self.header = header
            self.source = header["source"]
            self.data_start = _align(16 + hlen)
            last = layout[-1]
            if self.size != self.data_start + _align(last["offset"] + last["bytes"]):
                raise PackageError("file length does not match the layout")
            pads = [(16 + hlen, self.data_start)] + [
                (self.data_start + e["offset"] + e["bytes"], self.data_start + _align(e["offset"] + e["bytes"]))
                for e in layout]
            for begin, stop in pads:
                if stop > begin:
                    f.seek(begin)
                    if f.read(stop - begin).strip(b"\x00"):
                        raise PackageError(f"non-zero padding at byte {begin}")
        self.layout = layout
        self.entries = {e["name"]: e for e in layout}
        self._maps: Dict[str, np.ndarray] = {}
        if check_values:
            self.check_values()

    @property
    def has_embed(self) -> bool:
        return self.first == 0

    @property
    def has_head(self) -> bool:
        return self.end == self.model["n_layers"]

    def tensor(self, name: str) -> np.ndarray:
        if name not in self._maps:
            e = self.entries[name]
            self._maps[name] = np.memmap(self.path, dtype=DTYPES[e["dtype"]], mode="r",
                                         offset=self.data_start + e["offset"], shape=tuple(e["shape"]))
        return self._maps[name]

    def check_values(self) -> None:
        """Spec 3: q in [-127, 127], valid dyadic scales, router rows and shifts in range."""
        for e in self.layout:
            name = e["name"]
            if name.endswith(".q") and e["dtype"] == "i8":
                base = name[:-2]
                q = self.tensor(name)
                flat_rows = int(np.prod(q.shape[:-1]))
                cols = q.shape[-1]
                q2 = q.reshape(flat_rows, cols)
                mu = np.asarray(self.tensor(base + ".mu")).reshape(-1).astype(np.int64)
                k = np.asarray(self.tensor(base + ".k")).reshape(-1).astype(np.int64)
                step = _rows_chunk(cols)
                for r0 in range(0, flat_rows, step):
                    if np.any(np.asarray(q2[r0:r0 + step]) == -128):
                        raise PackageError(f"{name} contains -128")
                zero = mu == 0
                if np.any(~zero & ((mu < 1 << 30) | (mu >= 1 << 31) | (k < 16) | (k > 62))):
                    raise PackageError(f"{base}: invalid dyadic scale")
                if np.any(zero & (k != 16)):
                    raise PackageError(f"{base}: zero scale with k != 16")
                for row in np.nonzero(zero)[0].tolist():
                    if np.any(np.asarray(q2[row]) != 0):
                        raise PackageError(f"{base}: zero-scale row {row} has weights")
            elif name.endswith(".s") and e["dtype"] == "u16":
                sc = np.asarray(self.tensor(name)).astype(np.int64)
                if np.any(sc >> 15 == 1) or np.any((sc >> 7) & 0xFF == 0xFF):
                    raise PackageError(f"{name}: a group scale is negative, infinite or NaN")
            elif name.endswith("router.q"):
                if np.any(np.asarray(self.tensor(name)) == -32768):
                    raise PackageError(f"{name} contains -32768")
            elif name.endswith("router.k"):
                if np.any(np.asarray(self.tensor(name)) > 62):
                    raise PackageError(f"{name} has a shift above 62")
            elif e["dtype"] == "i64":
                if np.any(np.abs(np.asarray(self.tensor(name)).astype(np.int64)) > LIM62):
                    raise PackageError(f"{name} holds a value beyond 2^62")

    def segment_digests(self) -> List[Dict[str, Any]]:
        out: List[Dict[str, Any]] = []
        current: Optional[Tuple[str, Any, int]] = None
        with open(self.path, "rb") as f:
            for e in self.layout:
                if current is None or current[0] != e["segment"]:
                    if current is not None:
                        out.append({"name": current[0], "bytes": current[2], "blake3": current[1].hexdigest()})
                    current = (e["segment"], dy._blake3_ctor()(), 0)
                f.seek(self.data_start + e["offset"])
                remaining = e["bytes"]
                while remaining:
                    block = f.read(min(remaining, 1 << 24))
                    current[1].update(block)
                    remaining -= len(block)
                current = (current[0], current[1], current[2] + e["bytes"])
        if current is not None:
            out.append({"name": current[0], "bytes": current[2], "blake3": current[1].hexdigest()})
        return out

    def identity(self) -> Dict[str, Any]:
        return dy.Package.identity(self)  # same streaming sha256/blake3 of the file


# --------------------------------------------------------------------------
# Operators in Python ints (spec 5)

def rope_pairs_int(u: Sequence[int], cos_row: Sequence[int], sin_row: Sequence[int]) -> List[int]:
    """Spec 5.1: adjacent pairs (u[2i], u[2i+1]) rotated by table entry i."""
    out = list(u)
    for i in range(len(u) // 2):
        a, b, c, s = u[2 * i], u[2 * i + 1], cos_row[i], sin_row[i]
        out[2 * i] = check62((a * c - b * s) >> 16, "rope")
        out[2 * i + 1] = check62((a * s + b * c) >> 16, "rope")
    return out


def router_logits_int(x: Sequence[int], rows: Sequence[Sequence[int]], shifts: Sequence[int]) -> List[int]:
    """Spec 5.3."""
    if 32767 * sum(abs(v) for v in x) >= 1 << 63:
        raise DomainError("router input: 32767 * sum|x| >= 2^63")
    return [check62(sum(map(int.__mul__, row, x)) >> k, "router logit") for row, k in zip(rows, shifts)]


def sigmoid_int(g: int) -> int:
    """Dyadic v1 spec 5.7 sigma(g), Q16."""
    if g >= 0:
        return (1 << 32) // (ONE + dy.exp_q16(-g))
    e = dy.exp_q16(g)
    return (e << 16) // (ONE + e)


def select_experts_int(keys: Sequence[int], k: int, n_group: int, topk_group: int) -> List[int]:
    """Spec 5.4: group limit, then top-k by key, ties to the lower index."""
    e = len(keys)
    eligible = set(range(e))
    if n_group > 1:
        size = e // n_group
        scores = []
        for g in range(n_group):
            top2 = sorted(keys[g * size:(g + 1) * size], reverse=True)[:2]
            scores.append((-(top2[0] + top2[1]), g))
        kept = {g for _, g in sorted(scores)[:topk_group]}
        eligible = {x for x in range(e) if x // size in kept}
    order = sorted(eligible, key=lambda x: (-keys[x], x))
    if len(order) < k:
        raise RunError("fewer eligible experts than experts per token")
    return order[:k]


def routing_weights_int(sigma: Sequence[int], rho: int, normalize: bool) -> List[int]:
    """Spec 5.5 (Q32)."""
    if normalize and len(sigma) > 1:
        total = sum(sigma)
        return [0 for _ in sigma] if total == 0 else [s * rho // total for s in sigma]
    return [(s * rho) >> 16 for s in sigma]


def combine_int(weights: Sequence[int], outputs: Sequence[Sequence[int]], shared: Sequence[int]) -> List[int]:
    """Spec 5.6: one exact sum, one floor shift, then the shared experts."""
    out = []
    for j, s in enumerate(shared):
        routed = check62(sum(w * y[j] for w, y in zip(weights, outputs)) >> 32, "routed sum")
        out.append(check62(routed + s, "moe output"))
    return out


def mla_attend_int(qa: Sequence[int], qp: Sequence[int], latents: Sequence[Sequence[int]],
                   keys: Sequence[Sequence[int]], lam: int) -> List[int]:
    """Spec 5.2 inner loop for one head: the attention-weighted latent u."""
    scores = [check62(((sum(map(int.__mul__, qa, c)) + sum(map(int.__mul__, qp, kp))) * lam) >> 46,
                      "attention score") for c, kp in zip(latents, keys)]
    top = max(scores)
    w = [dy.exp_q16(s - top) for s in scores]
    z = sum(w)
    return [check62(dy.tdiv(sum(wi * c[r] for wi, c in zip(w, latents)), z), "attention output")
            for r in range(len(qa))]


def activation_hash(values: Sequence[int]) -> bytes:
    """Spec 6.2."""
    return blake3_raw(np.asarray(values, dtype=np.int64).astype("<i8").tobytes())


def boundary_digest(rows: Sequence[Sequence[int]]) -> str:
    return blake3_hex(b"".join(activation_hash(r) for r in rows))


class _Weights:
    """Typed views of a stage package's tensors (shared by both engines)."""

    def __init__(self, pkg: StagePackage):
        self.pkg = pkg

    def scaled(self, name: str, index: Optional[int] = None) -> Tuple[np.ndarray, np.ndarray, np.ndarray]:
        q = self.pkg.tensor(name + ".q")
        mu = np.asarray(self.pkg.tensor(name + ".mu")).astype(np.int64)
        k = np.asarray(self.pkg.tensor(name + ".k")).astype(np.int64)
        if index is None:
            return q, mu, k
        return q[index], mu[index], k[index]

    def vec(self, name: str) -> np.ndarray:
        return np.asarray(self.pkg.tensor(name)).astype(np.int64)


class SlowEngine:
    """Spec 5.7 one token at a time in Python ints, with the MLA cache (tiny models)."""

    def __init__(self, pkg: StagePackage):
        self.pkg = pkg
        self.m = pkg.model
        self.w = _Weights(pkg)
        lists = lambda arr: np.asarray(arr).astype(np.int64).tolist()  # noqa: E731
        self.cos, self.sin = lists(pkg.tensor("rope.cos")), lists(pkg.tensor("rope.sin"))
        self.reset()

    def reset(self) -> None:
        n = self.pkg.end - self.pkg.first
        self.latent: List[List[List[int]]] = [[] for _ in range(n)]
        self.rope_keys: List[List[List[int]]] = [[] for _ in range(n)]

    @property
    def position(self) -> int:
        return len(self.latent[0]) if self.latent else 0

    def _project(self, x: Sequence[int], name: str, index: Optional[int] = None) -> List[int]:
        if name + ".q4" in self.pkg.entries:
            q4, sc = self.pkg.tensor(name + ".q4"), self.pkg.tensor(name + ".s")
            if index is not None:
                q4, sc = q4[index], sc[index]
            values = unpack_q4(np.asarray(q4)).tolist()
            return q4_project_int(x, values, np.asarray(sc).astype(np.int64).tolist(), name)
        q, mu, k = self.w.scaled(name, index)
        rows = np.asarray(q).astype(np.int64).tolist()
        return dy.project_int(x, rows, mu.tolist(), k.tolist(), name)

    def _ffn(self, x: Sequence[int], base: str, index: Optional[int] = None) -> List[int]:
        g = self._project(x, base + ".w_gate", index)
        u = self._project(x, base + ".w_up", index)
        a = [dy.silu_gate_int(gi, ui) for gi, ui in zip(g, u)]
        return self._project(a, base + ".w_down", index)

    def layer(self, local: int, h: List[int], pos: int) -> List[int]:
        m = self.m
        li = self.pkg.first + local
        p = f"layers.{li}"
        eps, rank = m["rms_eps_q32"], m["kv_lora_rank"]
        nope, rope, vh = m["qk_nope_dim"], m["qk_rope_dim"], m["v_head_dim"]
        cos, sin = self.cos[pos], self.sin[pos]
        x = dy.rmsnorm_int(h, self.w.vec(p + ".attn_norm").tolist(), eps, p + " attn_norm")
        if m["q_lora_rank"] == 0:
            q = self._project(x, p + ".wq")
        else:
            qa = self._project(x, p + ".wq_a")
            qa = dy.rmsnorm_int(qa, self.w.vec(p + ".q_a_norm").tolist(), eps, p + " q_a_norm")
            q = self._project(qa, p + ".wq_b")
        kv = self._project(x, p + ".wkv_a")
        latent = dy.rmsnorm_int(kv[:rank], self.w.vec(p + ".kv_a_norm").tolist(), eps, p + " kv_a_norm")
        key = rope_pairs_int(kv[rank:], cos, sin)
        dy.check_i32(latent, p + " latent")
        dy.check_i32(key, p + " rope key")
        self.latent[local].append(latent)
        self.rope_keys[local].append(key)
        heads: List[int] = []
        for j in range(m["n_heads"]):
            base = j * (nope + rope)
            qp = rope_pairs_int(q[base + nope:base + nope + rope], cos, sin)
            qa = self._project(q[base:base + nope], p + ".wk_b", j)
            u = mla_attend_int(qa, qp, self.latent[local], self.rope_keys[local], m["attention_lambda"])
            heads += self._project(u, p + ".wv_b", j)
        h = dy.residual_int(h, self._project(heads, p + ".wo"))
        x = dy.rmsnorm_int(h, self.w.vec(p + ".ffn_norm").tolist(), eps, p + " ffn_norm")
        if not is_moe(m, li):
            return dy.residual_int(h, self._ffn(x, p))
        rq = np.asarray(self.pkg.tensor(p + ".router.q")).astype(np.int64).tolist()
        rk = np.asarray(self.pkg.tensor(p + ".router.k")).astype(np.int64).tolist()
        bias = self.w.vec(p + ".router_bias").tolist()
        logits = router_logits_int(x, rq, rk)
        sigma = [sigmoid_int(v) for v in logits]
        keys = [s * ONE + b for s, b in zip(sigma, bias)]
        chosen = select_experts_int(keys, m["n_experts_per_tok"], m["n_group"], m["topk_group"])
        weights = routing_weights_int([sigma[e] for e in chosen], m["routed_scaling_q32"], m["norm_topk_prob"])
        outputs = [self._ffn(x, p + ".experts", e) for e in chosen]
        shared = self._ffn(x, p + ".shared")
        return dy.residual_int(h, combine_int(weights, outputs, shared))

    def forward(self, token: Optional[int] = None, hidden: Optional[Sequence[int]] = None,
                trace: Optional[List[bytes]] = None) -> Tuple[List[int], Optional[List[int]]]:
        """One position: token for the first stage, boundary vector otherwise; (h, logits)."""
        m, pos = self.m, self.position
        if pos >= m["max_seq"]:
            raise DomainError("position >= max_seq")
        if self.pkg.has_embed:
            if token is None or hidden is not None or not 0 <= token < m["vocab_size"]:
                raise RunError("the first stage takes one valid token id")
            q, mu, k = self.w.scaled("embed")
            h = dy.embed_int(np.asarray(q[token]).astype(np.int64).tolist(), int(mu[token]), int(k[token]))
        else:
            if hidden is None or token is not None or len(hidden) != m["d_model"]:
                raise RunError("a later stage takes one boundary vector")
            h = [check62(int(v), "boundary") for v in hidden]
        if trace is not None:
            trace.append(activation_hash(h))
        for local in range(self.pkg.end - self.pkg.first):
            h = self.layer(local, h, pos)
            if trace is not None:
                trace.append(activation_hash(h))
        logits = None
        if self.pkg.has_head:
            n = dy.rmsnorm_int(h, self.w.vec("final_norm").tolist(), m["rms_eps_q32"], "final norm")
            logits = self._project(n, "lm_head")
        return h, logits


# --------------------------------------------------------------------------
# Exact batched teacher forcing (numpy)

class FastEngine:
    """Spec 5.7 at every position of every sequence of one stage, layer by layer."""

    def __init__(self, pkg: StagePackage):
        self.pkg = pkg
        self.m = pkg.model
        self.w = _Weights(pkg)
        self.cos = np.asarray(pkg.tensor("rope.cos")).astype(np.int64)
        self.sin = np.asarray(pkg.tensor("rope.sin")).astype(np.int64)
        self.fallbacks: Dict[str, int] = {}
        self._helper = dy.FastEngine.__new__(dy.FastEngine)  # reuse its exact elementwise operators
        self._helper.fallbacks = self.fallbacks
        self._helper.eps = self.m["rms_eps_q32"]

    def _note(self, what: str) -> None:
        self.fallbacks[what] = self.fallbacks.get(what, 0) + 1

    def _project_q4(self, x: np.ndarray, name: str, index: Optional[int]) -> np.ndarray:
        """Spec 13.2 for every row of x: float64 BLAS per group (exact below 2^53), int64 combine
        with a proven bound, Python ints otherwise."""
        q4, sc = self.pkg.tensor(name + ".q4"), self.pkg.tensor(name + ".s")
        if index is not None:
            q4, sc = q4[index], sc[index]
        q = unpack_q4(np.asarray(q4))
        s = np.asarray(sc).astype(np.int64)
        rows, cols = q.shape
        groups, t = cols // Q4_GROUP, x.shape[0]
        est = dy._abs_sum_bound(x) * 8.0
        for i in np.nonzero(est >= 2.0 ** 62)[0].tolist():
            if 8 * sum(abs(v) for v in x[i].tolist()) >= 1 << 63:
                raise DomainError(f"{name}: 8 * sum|x| >= 2^63")
        try:
            xg = x.reshape(t, groups, Q4_GROUP)
            if t and float(dy._abs_sum_bound(xg).max()) * 8.0 >= 2.0 ** 53:
                raise dy._NeedSlow("int4 group sum")
            with np.errstate(all="ignore"):
                acc_f = np.matmul(xg.transpose(1, 0, 2).astype(np.float64),
                                  q.reshape(rows, groups, Q4_GROUP).transpose(1, 2, 0).astype(np.float64))
            if acc_f.size and not (np.all(np.isfinite(acc_f)) and np.array_equal(acc_f, np.trunc(acc_f))):
                raise RuntimeError("BLAS returned a non-integral product of integer operands")
            acc = acc_f.astype(np.int64)  # [groups, t, rows]
            big_e, low = (s >> 7) & 0xFF, s & 0x7F
            mant = np.where(big_e == 0, low, low + 128)
            expo = np.where(big_e == 0, -133, big_e - 134)
            live = mant > 0
            top = np.where(live, expo, -(1 << 20)).max(axis=1)
            keep = live & (expo >= top[:, None] - Q4_SPAN)
            empty = ~keep.any(axis=1)
            e_min = np.where(keep, expo, 1 << 20).min(axis=1)
            e_min = np.where(empty, 0, e_min)
            if np.any(~empty & ((e_min >= 0) | (e_min <= -63))):
                raise dy._NeedSlow("int4 scale outside the int64 shift range")
            shift = np.where(keep, expo - e_min[:, None], 0)
            coef = np.where(keep, mant, 0) << shift  # [rows, groups], below 2^49
            weights = coef.astype(np.float64)
            bound = np.einsum("gtr,rg->tr", np.abs(acc).astype(np.float64), weights) * (1.0 + 2.0 ** -30)
            if bound.size and float(bound.max()) >= 2.0 ** 62:
                raise dy._NeedSlow("int4 combine")
            total = (acc.transpose(1, 2, 0) * coef[None, :, :]).sum(axis=2)  # [t, rows]
            y = total >> np.where(empty, 0, -e_min)[None, :]
            y[:, empty] = 0
            return dy._check62_array(y, name)
        except dy._NeedSlow:
            self._note("int4 projection")
            values, scales = q.tolist(), s.tolist()
            return dy._to_int64([q4_project_int(r, values, scales, name) for r in x.tolist()])

    def _project(self, x: np.ndarray, name: str, index: Optional[int] = None) -> np.ndarray:
        if name + ".q4" in self.pkg.entries:
            return self._project_q4(x, name, index)
        q, mu, k = self.w.scaled(name, index)
        dy.FastEngine._check_projection_input(x, name)
        acc = dy._matmul_exact(x, b_float=np.asarray(q, dtype=np.float64).T, b_max=127, guard=False)
        return self._helper._epilogue(acc, mu, k, name)

    def _rmsnorm(self, x: np.ndarray, gains: np.ndarray, what: str) -> np.ndarray:
        try:
            return self._helper._rmsnorm_fast(x, gains, what)
        except dy._NeedSlow:
            self._note("rmsnorm")
            gl = gains.tolist()
            return dy._to_int64([dy.rmsnorm_int(r, gl, self.m["rms_eps_q32"], what) for r in x.tolist()])

    def _rope(self, x: np.ndarray, positions: np.ndarray, what: str) -> np.ndarray:
        cos, sin = self.cos[positions], self.sin[positions]
        a, b = x[:, 0::2], x[:, 1::2]
        if x.size and int(np.max(np.abs(x))) * max(int(np.max(np.abs(cos))), int(np.max(np.abs(sin)))) < LIM62:
            out = np.empty_like(x)
            out[:, 0::2] = (a * cos - b * sin) >> 16
            out[:, 1::2] = (a * sin + b * cos) >> 16
            return dy._check62_array(out, what)
        self._note("rope")
        return dy._to_int64([rope_pairs_int(r, cr, sr) for r, cr, sr in
                             zip(x.tolist(), cos.tolist(), sin.tolist())])

    def _ffn(self, x: np.ndarray, base: str, index: Optional[int] = None) -> np.ndarray:
        g = self._project(x, base + ".w_gate", index)
        u = self._project(x, base + ".w_up", index)
        a = self._helper._silu(g, u, base + " gated silu")
        return self._project(a, base + ".w_down", index)

    def _attention(self, qa: np.ndarray, qp: np.ndarray, latent: np.ndarray, keys: np.ndarray,
                   spans: Sequence[Tuple[int, int]]) -> np.ndarray:
        """Spec 5.2 for one head over every sequence; returns u [T, C]."""
        lam = self.m["attention_lambda"]
        out = np.empty_like(qa)
        for s0, length in spans:
            sl = slice(s0, s0 + length)
            try:
                dot = dy._matmul_exact(qa[sl], latent[sl].T) + dy._matmul_exact(qp[sl], keys[sl].T)
                score = dy._check62_array(dy._mul_u31_shr(dot, lam, 46), "attention score")
                causal = np.tril(np.ones((length, length), dtype=bool))
                top = np.where(causal, score, np.iinfo(np.int64).min).max(axis=1, keepdims=True)
                w = dy._exp_vec(np.where(causal, score - top, -16 * ONE))
                z = w.sum(axis=1)[:, None]
                acc = dy._matmul_exact(w, latent[sl])
                out[sl] = np.where(acc >= 0, acc // z, -((-acc) // z))
            except dy._NeedSlow:
                self._note("attention")
                lat, ks = latent[sl].tolist(), keys[sl].tolist()
                out[sl] = dy._to_int64([mla_attend_int(qa[s0 + i].tolist(), qp[s0 + i].tolist(),
                                                       lat[:i + 1], ks[:i + 1], lam) for i in range(length)])
        return dy._check62_array(out, "attention output")

    def _router(self, x: np.ndarray, p: str) -> np.ndarray:
        rq = np.asarray(self.pkg.tensor(p + ".router.q")).astype(np.int64)
        rk = np.asarray(self.pkg.tensor(p + ".router.k")).astype(np.int64)
        try:
            acc = dy._matmul_exact(x, rq.T)
        except dy._NeedSlow:
            self._note("router")
            rows = rq.tolist()
            return dy._to_int64([router_logits_int(r, rows, rk.tolist()) for r in x.tolist()])
        est = dy._abs_sum_bound(x) * 32767.0
        for i in np.nonzero(est >= 2.0 ** 62)[0].tolist():
            if 32767 * sum(abs(v) for v in x[i].tolist()) >= 1 << 63:
                raise DomainError("router input: 32767 * sum|x| >= 2^63")
        return dy._check62_array(acc >> rk[None, :], "router logit")

    def _moe(self, x: np.ndarray, p: str) -> np.ndarray:
        m = self.m
        logits = self._router(x, p)
        neg = logits < 0
        e = dy._exp_vec(np.where(neg, logits, -logits))
        sigma = np.where(neg, (e << 16) // (ONE + e), (1 << 32) // (ONE + e))
        bias = self.w.vec(p + ".router_bias")
        keys = (sigma << 16) + bias[None, :]
        rows, experts = keys.shape
        masked = keys.copy()
        if m["n_group"] > 1:
            size = experts // m["n_group"]
            grouped = np.sort(keys.reshape(rows, m["n_group"], size), axis=2)
            scores = grouped[:, :, -1].astype(object) + grouped[:, :, -2].astype(object)
            for r in range(rows):
                order = sorted(range(m["n_group"]), key=lambda g: (-scores[r, g], g))
                kept = set(order[:m["topk_group"]])
                for g in range(m["n_group"]):
                    if g not in kept:
                        masked[r, g * size:(g + 1) * size] = np.iinfo(np.int64).min + 1
        order = np.argsort(-masked, axis=1, kind="stable")[:, :m["n_experts_per_tok"]]
        chosen_sigma = np.take_along_axis(sigma, order, axis=1)
        rho, k = m["routed_scaling_q32"], m["n_experts_per_tok"]
        if m["norm_topk_prob"] and k > 1:
            total = chosen_sigma.sum(axis=1, keepdims=True)
            weights = np.where(total > 0, (chosen_sigma * rho) // np.maximum(total, 1), 0)
        else:
            weights = (chosen_sigma * rho) >> 16
        d = m["d_model"]
        outputs = np.zeros((rows, k, d), dtype=np.int64)
        for expert in np.unique(order).tolist():
            r_idx, slot = np.nonzero(order == expert)
            outputs[r_idx, slot] = self._ffn(x[r_idx], p + ".experts", expert)
        shared = self._ffn(x, p + ".shared")
        bound = float(np.max(np.abs(weights))) * float(np.max(np.abs(outputs))) * k if outputs.size else 0.0
        if bound < 2.0 ** 62:
            routed = (weights[:, :, None] * outputs).sum(axis=1) >> 32
        else:
            self._note("combine")
            routed = dy._to_int64([[sum(int(w) * int(y) for w, y in zip(weights[r], outputs[r, :, j])) >> 32
                                    for j in range(d)] for r in range(rows)])
        routed = dy._check62_array(routed, "routed sum")
        return dy._check62_array(routed + shared, "moe output")

    def _layer(self, li: int, h: np.ndarray, positions: np.ndarray,
               spans: Sequence[Tuple[int, int]]) -> np.ndarray:
        m = self.m
        p = f"layers.{li}"
        rank, nope, rope = m["kv_lora_rank"], m["qk_nope_dim"], m["qk_rope_dim"]
        x = self._rmsnorm(h, self.w.vec(p + ".attn_norm"), p + " attn_norm")
        if m["q_lora_rank"] == 0:
            q = self._project(x, p + ".wq")
        else:
            qa = self._project(x, p + ".wq_a")
            qa = self._rmsnorm(qa, self.w.vec(p + ".q_a_norm"), p + " q_a_norm")
            q = self._project(qa, p + ".wq_b")
        kv = self._project(x, p + ".wkv_a")
        latent = self._rmsnorm(np.ascontiguousarray(kv[:, :rank]), self.w.vec(p + ".kv_a_norm"), p + " kv_a_norm")
        keys = self._rope(np.ascontiguousarray(kv[:, rank:]), positions, p + " rope key")
        dy.FastEngine._check_i32(latent, p + " latent")
        dy.FastEngine._check_i32(keys, p + " rope key")
        heads = []
        for j in range(m["n_heads"]):
            base = j * (nope + rope)
            qp = self._rope(np.ascontiguousarray(q[:, base + nope:base + nope + rope]), positions, p + " rope q")
            qa = self._project(np.ascontiguousarray(q[:, base:base + nope]), p + ".wk_b", j)
            u = self._attention(qa, qp, latent, keys, spans)
            heads.append(self._project(u, p + ".wv_b", j))
        h = self._helper._residual(h, self._project(np.concatenate(heads, axis=1), p + ".wo"), p + " residual")
        x = self._rmsnorm(h, self.w.vec(p + ".ffn_norm"), p + " ffn_norm")
        delta = self._moe(x, p) if is_moe(m, li) else self._ffn(x, p)
        return self._helper._residual(h, delta, p + " residual")

    def run(self, sequences: Sequence[Sequence[int]], inputs: Optional[Sequence[np.ndarray]] = None,
            on_layer: Optional[Callable[[int, np.ndarray], None]] = None) -> Tuple[np.ndarray, Optional[np.ndarray]]:
        """Teacher-force token sequences (first stage) or boundary rows (later stages).

        Returns (output boundary rows [T, D] stacked by sequence, logits [T, V] or None).
        on_layer(boundary_index, rows) is called at every boundary of the stage.
        """
        m = self.m
        lengths = [len(s) for s in sequences]
        if not lengths or min(lengths) < 1 or max(lengths) > m["max_seq"]:
            raise RunError("sequences must be non-empty and fit max_seq")
        spans, start = [], 0
        for n in lengths:
            spans.append((start, n))
            start += n
        positions = np.concatenate([np.arange(n, dtype=np.int64) for n in lengths])
        if self.pkg.has_embed:
            if inputs is not None:
                raise RunError("the first stage takes token ids")
            tokens = np.array([t for s in sequences for t in s], dtype=np.int64)
            if tokens.size and (int(tokens.min()) < 0 or int(tokens.max()) >= m["vocab_size"]):
                raise DomainError("token id outside the vocabulary")
            q, mu, k = self.w.scaled("embed")
            rows = np.asarray(q[tokens]).astype(np.int64)
            h = (rows * mu[tokens][:, None]) >> (k[tokens][:, None] - 16)
        else:
            if inputs is None or [len(x) for x in inputs] != lengths:
                raise RunError("a later stage needs boundary rows for every position")
            h = dy._check62_array(np.concatenate([np.asarray(x, dtype=np.int64) for x in inputs]), "boundary")
        if on_layer:
            on_layer(self.pkg.first, h)
        for li in range(self.pkg.first, self.pkg.end):
            h = self._layer(li, h, positions, spans)
            if on_layer:
                on_layer(li + 1, h)
        logits = None
        if self.pkg.has_head:
            n = self._rmsnorm(h, self.w.vec("final_norm"), "final norm")
            q, mu, k = self.w.scaled("lm_head")
            vocab = q.shape[0]
            dy.FastEngine._check_projection_input(n, "lm head")
            logits = np.empty((n.shape[0], vocab), dtype=np.int64)
            chunk = max(1, (1 << 24) // q.shape[1])
            for r0 in range(0, vocab, chunk):
                wf_t = np.asarray(q[r0:r0 + chunk], dtype=np.float64).T
                acc = dy._matmul_exact(n, b_float=wf_t, b_max=127, guard=False)
                logits[:, r0:r0 + chunk] = self._helper._epilogue(acc, mu[r0:r0 + chunk], k[r0:r0 + chunk],
                                                                  "lm head")
        return h, logits


# --------------------------------------------------------------------------
# Boundary files (spec 6.3)

def write_boundary(path: Optional[Path], layer: int, d_model: int, root: str,
                   sequences: Sequence[Dict[str, Any]], profile: str = PROFILE_ID) -> Tuple[bytes, str]:
    """sequences: {id, tokens, prompt_len, selection, eos, max_tokens, values: int64 [P, D]}."""
    meta = []
    blobs = []
    for s in sequences:
        values = np.asarray(s["values"], dtype=np.int64).reshape(len(s["tokens"]), d_model)
        if values.size and int(np.max(np.abs(values))) > LIM62:
            raise DomainError("boundary value beyond 2^62")
        meta.append({"id": s["id"], "tokens": list(s["tokens"]), "prompt_len": s["prompt_len"],
                     "selection": s["selection"], "eos": list(s["eos"]), "max_tokens": s["max_tokens"],
                     "digest": boundary_digest(values)})
        blobs.append(values.astype("<i8").tobytes())
    if profile not in FORMATS:
        raise RunError(f"{profile!r} is not an MLA + MoE profile")
    header = canonical_json({"schema": BOUNDARY_SCHEMA, "profile": profile, "model_root": root,
                             "layer": layer, "d_model": d_model, "sequences": meta})
    data = BOUNDARY_MAGIC + struct.pack("<Q", len(header)) + header
    data += b"\x00" * (_align(len(data)) - len(data)) + b"".join(blobs)
    if path is not None:
        Path(path).write_bytes(data)
    return data, blake3_hex(data)


def read_boundary(data: bytes) -> Dict[str, Any]:
    if len(data) < 16 or data[:8] != BOUNDARY_MAGIC:
        raise PackageError("not an ARCBND01 file")
    (hlen,) = struct.unpack("<Q", data[8:16])
    raw = data[16:16 + hlen]
    header = json.loads(raw)
    if canonical_json(header) != raw or sorted(header) != ["d_model", "layer", "model_root", "profile",
                                                            "schema", "sequences"]:
        raise PackageError("boundary header is not the canonical spec 6.3 object")
    if header["schema"] != BOUNDARY_SCHEMA or header["profile"] not in FORMATS:
        raise PackageError("boundary schema or profile mismatch")
    d = header["d_model"]
    offset = _align(16 + hlen)
    if data[16 + hlen:offset].strip(b"\x00"):
        raise PackageError("non-zero boundary padding")
    sequences = []
    for s in header["sequences"]:
        if sorted(s) != ["digest", "eos", "id", "max_tokens", "prompt_len", "selection", "tokens"]:
            raise PackageError("boundary sequence has the wrong fields")
        n = len(s["tokens"]) * d
        values = np.frombuffer(data[offset:offset + 8 * n], dtype="<i8").astype(np.int64)
        if values.size != n:
            raise PackageError("boundary file is truncated")
        offset += 8 * n
        values = values.reshape(len(s["tokens"]), d)
        if boundary_digest(values) != s["digest"]:
            raise PackageError(f"boundary sequence {s['id']} does not hash to its digest")
        sequences.append(dict(s, values=values))
    if offset != len(data):
        raise PackageError("boundary file has trailing bytes")
    return {"layer": header["layer"], "d_model": d, "model_root": header["model_root"],
            "profile": header["profile"], "sequences": sequences}


# --------------------------------------------------------------------------
# Runs, stage replay and verification

def forwarded_tokens(case: Dict[str, Any]) -> List[int]:
    """prompt || tokens[:n-1] with tokens cut at max_tokens (spec 6.4)."""
    toks = list(case["tokens"])[:case["max_tokens"]]
    return list(case["prompt_tokens"]) + toks[:-1]


def rederive(logits: np.ndarray, tokens: Sequence[int], prompt_len: int, selection: str) -> List[int]:
    """Re-derive generated tokens from teacher-forced logits rows (spec 6.4)."""
    out = []
    for pos in range(prompt_len - 1, len(tokens)):
        out.append(dy.select_next(logits[pos], tokens[prompt_len:pos + 1], selection))
    return out


def generate_run(pkg_path: Path, cases_path: Path) -> Dict[str, Any]:
    """Independent generation with SlowEngine (tiny whole-model packages)."""
    pkg = StagePackage(pkg_path)
    if not (pkg.has_embed and pkg.has_head):
        raise RunError("generation needs the whole-model package")
    doc = json.loads(Path(cases_path).read_bytes())
    if doc.get("schema") != CASES_SCHEMA:
        raise RunError(f"cases file must have schema {CASES_SCHEMA!r}")
    m = pkg.model
    engine = SlowEngine(pkg)
    results = []
    for raw in doc["cases"]:
        case = dy.validate_case(dict(raw, eos=raw.get("eos", []), selection=raw.get("selection", "rp64-argmax")),
                                {"vocab_size": m["vocab_size"], "max_seq": m["max_seq"]})
        engine.reset()
        hashes: List[bytes] = []
        boundaries = [dy._blake3_ctor()() for _ in range(m["n_layers"] + 1)]
        logits: List[int] = []

        def step(token: int) -> List[int]:
            trace: List[bytes] = []
            _, lg = engine.forward(token=token, trace=trace)
            for hasher, value in zip(boundaries, trace):
                hasher.update(value)
            return lg

        for t in case["prompt_tokens"]:
            logits = step(t)
            hashes.append(dy.logits_hash_raw(logits))
        out: List[int] = []
        while True:
            nxt = dy.select_next(logits, out, case["selection"])
            out.append(nxt)
            if nxt in case["eos"] or len(out) == case["max_tokens"]:
                break
            logits = step(nxt)
            hashes.append(dy.logits_hash_raw(logits))
        results.append(dict(case, tokens=out, output_hash=dy.output_hash(out),
                            logits_hashes=[h.hex() for h in hashes], logits_digest=dy.logits_digest(hashes),
                            boundary_digests=[b.hexdigest() for b in boundaries]))
    segments = pkg.segment_digests()
    return {"schema": RUN_SCHEMA, "package": pkg.identity(), "profile": pkg.profile,
            "model_root": model_root(m, pkg.source, segments, pkg.fmt), "kernel": "python-reference-int",
            "cases": results, "matrix_digest": dy.matrix_digest(results),
            "boundary_matrix_digest": blake3_hex(canonical_json([{"id": c["id"],
                                                                 "boundary_digests": c["boundary_digests"]}
                                                                for c in results]))}


def verify_run(pkg_path: Path, run_path: Path) -> Tuple[Dict[str, Any], List[str]]:
    """Teacher-force every case of a Rust run with FastEngine; compare logits, boundaries and tokens."""
    run = json.loads(Path(run_path).read_bytes())
    pkg = StagePackage(pkg_path)
    if run.get("schema") != RUN_SCHEMA or run.get("profile") != pkg.profile:
        raise RunError(f"run must have schema {RUN_SCHEMA!r} and the package's profile {pkg.profile!r}")
    if not (pkg.has_embed and pkg.has_head):
        raise RunError("verify-run needs the whole-model package")
    m = pkg.model
    problems: List[str] = []
    segments = pkg.segment_digests()
    root = model_root(m, pkg.source, segments, pkg.fmt)
    if run.get("model_root") != root:
        problems.append(f"run model_root {run.get('model_root')} != package model root {root}")
    cases = run["cases"]
    seqs = [forwarded_tokens(c) for c in cases]
    lengths = [len(s) for s in seqs]
    starts = np.cumsum([0] + lengths)
    boundary_hashers = [[dy._blake3_ctor()() for _ in range(m["n_layers"] + 1)] for _ in cases]

    def on_layer(layer: int, rows: np.ndarray) -> None:
        for ci in range(len(cases)):
            for r in rows[starts[ci]:starts[ci + 1]]:
                boundary_hashers[ci][layer].update(activation_hash(r))

    engine = FastEngine(pkg)
    _, logits = engine.run(seqs, on_layer=on_layer)
    golden = []
    for ci, c in enumerate(cases):
        rows = logits[starts[ci]:starts[ci + 1]]
        mine = [dy.logits_hash_raw(r).hex() for r in rows]
        tokens = rederive(rows, seqs[ci], len(c["prompt_tokens"]), c["selection"])
        bds = [h.hexdigest() for h in boundary_hashers[ci]]
        checks = {
            "logits_hashes_match": mine == c.get("logits_hashes"),
            "boundary_digests_match": bds == c.get("boundary_digests"),
            "tokens_match": tokens == list(c["tokens"])[:c["max_tokens"]],
            "output_hash_match": dy.output_hash(tokens) == c.get("output_hash"),
        }
        for key, ok in checks.items():
            if not ok:
                problems.append(f"case {c['id']!r}: {key} is false")
        golden.append({"id": c["id"], "prompt_tokens": c["prompt_tokens"], "tokens": tokens,
                       "output_hash": dy.output_hash(tokens), "logits_hashes": mine,
                       "logits_digest": dy.logits_digest([bytes.fromhex(x) for x in mine]),
                       "boundary_digests": bds, "checks": checks})
    mdigest = dy.matrix_digest(golden)
    if run.get("matrix_digest") != mdigest:
        problems.append("matrix_digest differs")
    return {"schema": "arc.mla-golden.v1", "profile": pkg.profile, "model_root": root,
            "verifier": "arc_conformance.mla_moe_reference FastEngine (independent Python)",
            "cases": golden, "matrix_digest": mdigest,
            "checks": {"fast_path_fallbacks": dict(engine.fallbacks), "all_match": not problems}}, problems


def stage_replay(pkg_path: Path, run_path: Optional[Path], input_path: Optional[Path],
                 out_path: Optional[Path]) -> Dict[str, Any]:
    """Spec 6.4/6.5: run one stage alone from tokens (first stage) or a boundary file."""
    pkg = StagePackage(pkg_path)
    m = pkg.model
    if run_path is not None:
        if not pkg.has_embed:
            raise RunError("--run feeds token ids, which only the first stage takes")
        run = json.loads(Path(run_path).read_bytes())
        root = run["model_root"]
        sequences = [{"id": c["id"], "tokens": forwarded_tokens(c), "prompt_len": len(c["prompt_tokens"]),
                      "selection": c["selection"], "eos": c["eos"], "max_tokens": c["max_tokens"]}
                     for c in run["cases"]]
        inputs = None
        input_desc: Dict[str, Any] = {"kind": "tokens", "layer": 0}
    else:
        data = Path(input_path).read_bytes()
        boundary = read_boundary(data)
        if boundary["layer"] != pkg.first or boundary["d_model"] != m["d_model"]:
            raise RunError("boundary file does not start this stage")
        if boundary["profile"] != pkg.profile:
            raise RunError(f"boundary produced under {boundary['profile']}, stage runs {pkg.profile}")
        root = boundary["model_root"]
        sequences = boundary["sequences"]
        inputs = [s["values"] for s in sequences]
        input_desc = {"kind": "boundary", "layer": boundary["layer"], "file_blake3": blake3_hex(data),
                      "digests": [s["digest"] for s in sequences]}
    engine = FastEngine(pkg)
    h, logits = engine.run([s["tokens"] for s in sequences], inputs)
    out_sequences, head, start = [], [], 0
    for s in sequences:
        n = len(s["tokens"])
        rows = h[start:start + n]
        out_sequences.append(dict({k: s[k] for k in ("id", "tokens", "prompt_len", "selection", "eos",
                                                       "max_tokens")}, values=rows))
        if logits is not None:
            lg = logits[start:start + n]
            hashes = [dy.logits_hash_raw(r).hex() for r in lg]
            derived = rederive(lg, s["tokens"], s["prompt_len"], s["selection"])
            head.append({"id": s["id"], "logits_hashes": hashes,
                         "logits_digest": dy.logits_digest([bytes.fromhex(x) for x in hashes]),
                         "derived_tokens": derived, "output_hash": dy.output_hash(derived)})
        start += n
    data, file_b3 = write_boundary(out_path, pkg.end, m["d_model"], root, out_sequences, pkg.profile)
    return {"schema": STAGE_RUN_SCHEMA, "profile": pkg.profile, "model_root": root,
            "stage": {"first_layer": pkg.first, "end_layer": pkg.end}, "segments": pkg.segment_digests(),
            "input": input_desc,
            "output": {"layer": pkg.end, "file_bytes": len(data), "file_blake3": file_b3,
                       "digests": [boundary_digest(s["values"]) for s in out_sequences]},
            "head": head if logits is not None else None,
            "kernel": "python-reference-fast", "fallbacks": dict(engine.fallbacks)}


# --------------------------------------------------------------------------
# CLI

def _parse_layers(text: Optional[str]) -> Tuple[Optional[int], Optional[int]]:
    if not text:
        return None, None
    a, b = text.split(":")
    return int(a), int(b)


def _write_json(path: Path, obj: Any) -> None:
    Path(path).write_text(json.dumps(obj, indent=1) + "\n", encoding="ascii")


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="python3 -m arc_conformance.mla_moe_reference",
                                     description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="cmd")
    sub.required = True
    p = sub.add_parser("prepare", help="BF16 safetensors -> stage package (or only its digests)")
    p.add_argument("--source-dir", required=True)
    p.add_argument("--source-manifest", required=True)
    p.add_argument("--layers", help="A:B (default: the whole model)")
    p.add_argument("--out", help="package path; omit with --hash-only")
    p.add_argument("--hash-only", action="store_true")
    p.add_argument("--experts", choices=sorted(PROFILES), default="i8",
                   help="routed experts as INT8 dyadic rows (default) or INT4 group-32 (spec 13)")
    p.add_argument("--json-out")
    g = sub.add_parser("generate", help="independent generation (SlowEngine, tiny models)")
    g.add_argument("--package", required=True)
    g.add_argument("--cases", required=True)
    g.add_argument("--out", required=True)
    v = sub.add_parser("verify-run", help="teacher-force a Rust run; compare logits, boundaries, tokens")
    v.add_argument("--package", required=True)
    v.add_argument("--run", required=True)
    v.add_argument("--out", required=True)
    s = sub.add_parser("stage-replay", help="run one stage alone from tokens or a boundary file")
    s.add_argument("--package", required=True)
    s.add_argument("--run")
    s.add_argument("--input")
    s.add_argument("--out")
    s.add_argument("--report", required=True)
    sl = sub.add_parser("slices", help="weight slice manifest (spec 14), digests only")
    sl.add_argument("--source-dir", required=True)
    sl.add_argument("--source-manifest", required=True)
    sl.add_argument("--units", help="comma-separated: embed, layer.N, head (default: every unit)")
    sl.add_argument("--expert-groups", type=int, default=1)
    sl.add_argument("--experts", choices=sorted(PROFILES))
    sl.add_argument("--json-out", required=True)
    t = sub.add_parser("tables", help="RoPE table digest for (theta, rope width, max_seq)")
    t.add_argument("--theta", type=int, default=50000)
    t.add_argument("--rope-dim", type=int, default=64)
    t.add_argument("--max-seq", type=int, default=4096)
    args = parser.parse_args(argv)
    try:
        if args.cmd == "prepare":
            if bool(args.out) == bool(args.hash_only):
                parser.error("give exactly one of --out and --hash-only")
            first, end = _parse_layers(args.layers)
            ident = prepare_stage(Path(args.source_dir), Path(args.source_manifest), first, end,
                                  None if args.hash_only else Path(args.out), args.experts)
            if args.json_out:
                _write_json(Path(args.json_out), ident)
            print(json.dumps({k: ident[k] for k in ident if k != "segments"}, indent=1))
        elif args.cmd == "generate":
            run = generate_run(Path(args.package), Path(args.cases))
            _write_json(Path(args.out), run)
            print(json.dumps({"cases": len(run["cases"]), "matrix_digest": run["matrix_digest"]}, indent=1))
        elif args.cmd == "verify-run":
            golden, problems = verify_run(Path(args.package), Path(args.run))
            _write_json(Path(args.out), golden)
            if problems:
                for problem in problems:
                    print(f"MISMATCH: {problem}", file=sys.stderr)
                return 1
            print(json.dumps({"all_match": True, "cases": len(golden["cases"]),
                              "matrix_digest": golden["matrix_digest"]}, indent=1))
        elif args.cmd == "stage-replay":
            if bool(args.run) == bool(args.input):
                parser.error("give exactly one of --run and --input")
            report = stage_replay(Path(args.package), Path(args.run) if args.run else None,
                                  Path(args.input) if args.input else None,
                                  Path(args.out) if args.out else None)
            _write_json(Path(args.report), report)
            print(json.dumps({"stage": report["stage"], "output": report["output"]}, indent=1))
        elif args.cmd == "slices":
            units = args.units.split(",") if args.units else None
            out = prepare_slices(Path(args.source_dir), Path(args.source_manifest), units,
                                 args.expert_groups, args.experts)
            _write_json(Path(args.json_out), out)
            print(json.dumps({"manifest_blake3": out["manifest_blake3"], "complete": out["complete"],
                              "model_root": out["model_root"], "pending": out["pending"],
                              "segments": len(out["segments"]), "slices": len(out["slices"])}, indent=1))
        elif args.cmd == "tables":
            cos, sin = dy.build_rope_tables(args.theta, args.rope_dim, args.max_seq)
            print(json.dumps({"theta": args.theta, "rope_dim": args.rope_dim, "max_seq": args.max_seq,
                              "blake3": dy.rope_tables_blake3(cos, sin),
                              "cos[1][0..3]": cos[1][:3].tolist() if args.max_seq > 1 else None,
                              "sin[1][0..3]": sin[1][:3].tolist() if args.max_seq > 1 else None}, indent=1))
    except (DomainError, PreparationError, PackageError, RunError) as error:
        print(f"refused: {type(error).__name__}: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
