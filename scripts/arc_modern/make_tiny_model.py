#!/usr/bin/env python3
"""Write a tiny SmolLM3-shaped BF16 model for CI cross-checks.

Usage: python3 scripts/arc_modern/make_tiny_model.py OUTDIR

Writes into OUTDIR:
  config.json                       SmolLM3-style config (64 wide, 4 layers, NoPE on layer 3)
  model.safetensors                 one shard, every tensor BF16 with the Hugging Face names
  tiny-smollm3.source.json          source manifest (arc.hf-source.v1) pinning both files
  tiny-smollm3.cases.json           token-id cases (arc.modern-cases.v1)

Every value comes from a fixed 64-bit LCG turned directly into BF16 bit
patterns (no floating point), and every file is written as bytes, so the
output is identical on every OS and Python version. The weights include the
edge cases of spec 4.1-4.3: an all-zero embedding row (with negative zeros),
an all-zero projection row, subnormals, a row whose maximum is tied between
+v and -v, elements far below their row maximum, and norm gains that round
exactly half-way.
"""

from __future__ import annotations

import hashlib
import json
import struct
import sys
from pathlib import Path

import numpy as np

MASK64 = (1 << 64) - 1
SEED = 0x534D_4F4C_4C4D_3321  # "SMOLLM3!"

VOCAB = 300
D_MODEL = 64
N_HEADS = 4
N_KV_HEADS = 2
D_HEAD = D_MODEL // N_HEADS
D_FF = 128
N_LAYERS = 4
NO_ROPE_LAYERS = [1, 1, 1, 0]
MAX_SEQ = 64
EOS = 7
ZERO_EMBED_ROW = 5

CONFIG = {
    "architectures": ["SmolLM3ForCausalLM"],
    "attention_bias": False,
    "attention_dropout": 0.0,
    "bos_token_id": None,
    "eos_token_id": EOS,
    "hidden_act": "silu",
    "hidden_size": D_MODEL,
    "initializer_range": 0.02,
    "intermediate_size": D_FF,
    "max_position_embeddings": 256,
    "max_window_layers": N_LAYERS,
    "mlp_bias": False,
    "model_type": "smollm3",
    "no_rope_layer_interval": 4,
    "no_rope_layers": NO_ROPE_LAYERS,
    "num_attention_heads": N_HEADS,
    "num_hidden_layers": N_LAYERS,
    "num_key_value_heads": N_KV_HEADS,
    "pad_token_id": 0,
    "pretraining_tp": 1,
    "rms_norm_eps": 1e-06,
    "rope_scaling": None,
    "rope_theta": 5000000.0,
    "sliding_window": None,
    "tie_word_embeddings": True,
    "torch_dtype": "bfloat16",
    "transformers_version": "4.54.0",
    "use_cache": True,
    "use_sliding_window": False,
    "vocab_size": VOCAB,
}

CASES = [
    {"id": "zero-embedding-token", "prompt_tokens": [ZERO_EMBED_ROW], "max_tokens": 8,
     "eos": [EOS], "selection": "rp64-argmax"},
    {"id": "four-token-greedy", "prompt_tokens": [11, 42, 250, 3], "max_tokens": 10,
     "eos": [EOS], "selection": "argmax"},
    {"id": "eight-token-rp64", "prompt_tokens": [1, 99, 123, EOS, 64, 299, 17, 200], "max_tokens": 12,
     "eos": [EOS], "selection": "rp64-argmax"},
    {"id": "two-token-greedy", "prompt_tokens": [256, 2], "max_tokens": 9,
     "eos": [EOS], "selection": "argmax"},
]


class Lcg:
    def __init__(self, seed: int):
        self.state = seed & MASK64

    def next(self) -> int:
        self.state = (self.state * 6364136223846793005 + 1442695040888963407) & MASK64
        return self.state


def bf16(sign: int, exponent: int, mantissa: int) -> int:
    return (sign << 15) | (exponent << 7) | mantissa


def weight_bits(rng: Lcg, lo_exp: int, n_exp: int) -> int:
    """Random sign, 7 random mantissa bits, biased exponent in [lo_exp, lo_exp + n_exp)."""
    r = rng.next()
    return bf16((r >> 63) & 1, lo_exp + (r >> 40) % n_exp, (r >> 56) & 0x7F)


def matrix(rng: Lcg, rows: int, cols: int, lo_exp: int = 120, n_exp: int = 5) -> list:
    # Exponents 120..124: magnitudes in [2^-7, 2^-2), RMS about 0.1.
    return [[weight_bits(rng, lo_exp, n_exp) for _ in range(cols)] for _ in range(rows)]


def norm(rng: Lcg, width: int) -> list:
    # Gains in [0.5, 2): biased exponent 126 or 127, positive.
    out = []
    for _ in range(width):
        r = rng.next()
        out.append(bf16(0, 126 + (r >> 50) % 2, (r >> 56) & 0x7F))
    return out


def tensors() -> list:
    """(name, shape, bit patterns) in a fixed order."""
    rng = Lcg(SEED)
    out = []
    embed = matrix(rng, VOCAB, D_MODEL, lo_exp=121)
    embed[ZERO_EMBED_ROW] = [0x8000 if j % 3 == 0 else 0x0000 for j in range(D_MODEL)]
    out.append(("model.embed_tokens.weight", [VOCAB, D_MODEL], embed))
    for layer in range(N_LAYERS):
        p = f"model.layers.{layer}."
        attn_norm = norm(rng, D_MODEL)
        ffn_norm = norm(rng, D_MODEL)
        wq = matrix(rng, N_HEADS * D_HEAD, D_MODEL)
        wk = matrix(rng, N_KV_HEADS * D_HEAD, D_MODEL)
        wv = matrix(rng, N_KV_HEADS * D_HEAD, D_MODEL)
        wo = matrix(rng, D_MODEL, N_HEADS * D_HEAD)
        gate = matrix(rng, D_FF, D_MODEL)
        up = matrix(rng, D_FF, D_MODEL)
        down = matrix(rng, D_MODEL, D_FF)
        if layer == 0:
            # Row maximum tied between +v and -v (exponent 125 is above every random
            # entry); subnormals 123 binades below it (the d > 60 rule); negative zero.
            wq[3][0], wq[3][1] = bf16(0, 125, 0x55), bf16(1, 125, 0x55)
            wq[3][2], wq[3][3], wq[3][4] = 0x0001, 0x807F, 0x8000
            # Elements 20 and 40 binades below the row maximum (q = 0 with d <= 60).
            down[5][0] = bf16(0, 125, 0x7F)
            down[5][1], down[5][2] = bf16(1, 105, 0x01), bf16(0, 85, 0x00)
            # Norm gains that round half-way: 1.5 * 2^-16 -> 2, -1.5 * 2^-16 -> -2,
            # 2.5 * 2^-16 -> 3 (half away from zero), tiny and subnormal -> 0, a negative gain.
            attn_norm[0:6] = [0x37C0, 0xB7C0, 0x3820, 0x0003, 0x8000, bf16(1, 126, 0x40)]
        if layer == 1:
            wk[0] = [0x0000] * D_MODEL  # an all-zero projection row
            ffn_norm[7] = bf16(0, 130, 0x20)  # a larger gain (~10)
        out += [(p + "input_layernorm.weight", [D_MODEL], attn_norm),
                (p + "self_attn.q_proj.weight", [N_HEADS * D_HEAD, D_MODEL], wq),
                (p + "self_attn.k_proj.weight", [N_KV_HEADS * D_HEAD, D_MODEL], wk),
                (p + "self_attn.v_proj.weight", [N_KV_HEADS * D_HEAD, D_MODEL], wv),
                (p + "self_attn.o_proj.weight", [D_MODEL, N_HEADS * D_HEAD], wo),
                (p + "post_attention_layernorm.weight", [D_MODEL], ffn_norm),
                (p + "mlp.gate_proj.weight", [D_FF, D_MODEL], gate),
                (p + "mlp.up_proj.weight", [D_FF, D_MODEL], up),
                (p + "mlp.down_proj.weight", [D_MODEL, D_FF], down)]
    out.append(("model.norm.weight", [D_MODEL], norm(rng, D_MODEL)))
    return out


def safetensors_bytes(items: list) -> bytes:
    header = {"__metadata__": {"format": "pt"}}
    blobs = []
    offset = 0
    for name, shape, values in items:
        flat = np.array(values, dtype=np.int64).reshape(-1)
        blob = flat.astype("<u2").tobytes()
        if flat.size != int(np.prod(shape)):
            raise AssertionError(name)
        header[name] = {"dtype": "BF16", "shape": shape, "data_offsets": [offset, offset + len(blob)]}
        blobs.append(blob)
        offset += len(blob)
    text = json.dumps(header, sort_keys=True, separators=(",", ":")).encode("ascii")
    text += b" " * (-len(text) % 8)
    return struct.pack("<Q", len(text)) + text + b"".join(blobs)


def json_bytes(obj) -> bytes:
    return (json.dumps(obj, indent=2, sort_keys=False) + "\n").encode("ascii")


def write(outdir: Path) -> dict:
    outdir.mkdir(parents=True, exist_ok=True)
    files = {"config.json": (json.dumps(CONFIG, indent=2, sort_keys=True) + "\n").encode("ascii"),
             "model.safetensors": safetensors_bytes(tensors())}
    for name, data in files.items():
        (outdir / name).write_bytes(data)
    manifest = {
        "schema": "arc.hf-source.v1",
        "repo": "arc-test/tiny-smollm3",
        "revision": "0" * 40,
        "license": "apache-2.0",
        "url_template": "https://huggingface.co/{repo}/resolve/{revision}/{name}",
        "max_seq": MAX_SEQ,
        "files": [{"name": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                  for name, data in files.items()],
    }
    (outdir / "tiny-smollm3.source.json").write_bytes(json_bytes(manifest))
    (outdir / "tiny-smollm3.cases.json").write_bytes(
        json_bytes({"schema": "arc.modern-cases.v1", "cases": CASES}))
    return manifest


def main(argv: list) -> int:
    if len(argv) != 2:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    manifest = write(Path(argv[1]))
    for entry in manifest["files"]:
        print(f"{entry['name']}: {entry['bytes']} bytes sha256 {entry['sha256']}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
