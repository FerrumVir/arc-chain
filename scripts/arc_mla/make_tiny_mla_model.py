#!/usr/bin/env python3
"""Write tiny DeepSeek-V3-shaped BF16 models for the MLA + MoE CI cross-checks.

Usage: python3 scripts/arc_mla/make_tiny_mla_model.py OUTDIR [--variant moonlight|kimi]

Writes into OUTDIR (variant "moonlight", the default, has Moonlight's shape
options; "kimi" has Kimi K2's):
  config.json                        model_type deepseek_v3 / kimi_k2
  model-00001-of-00002.safetensors   embedding and layers 0-1
  model-00002-of-00002.safetensors   layers 2-3, model.norm and lm_head
  tiny-mla.source.json               source manifest (arc.hf-source.v1)
  tiny-mla.cases.json                token-id cases (arc.modern-cases.v1)

| option                  | moonlight | kimi |
|-------------------------|-----------|------|
| q_lora_rank             | null      | 48   |
| n_group / topk_group    | 1 / 1     | 4 / 2 |
| n_shared_experts        | 2         | 1    |
| routing bias dtype      | BF16      | F32  |
| routed_scaling_factor   | 2.446     | 2.827 |

Both: 4 layers (layer 0 dense), width 64, 4 heads, latent rank 32, RoPE
width 8, 8 routed experts of width 32 with 3 per token, vocabulary 300,
max_seq 64. Widths are multiples of 32, so the experts also convert to the
INT4 group-32 variant (spec section 13).

Every value comes from a fixed 64-bit LCG turned directly into bit patterns
(no floating point), so the files are identical on every OS. Edge cases:
the dyadic v1 quantiser cases (all-zero rows, subnormals, tied maxima, elements
far below the row maximum), ignored rotary inv_freq buffers, and in layers 1
and 3 two experts with identical router rows and biases, so that their keys
tie exactly and the lower index must win (spec 5.4).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import struct
import sys
from pathlib import Path

import numpy as np

MASK64 = (1 << 64) - 1

VOCAB = 300
D_MODEL = 64
N_HEADS = 4
NOPE, ROPE, V_HEAD = 16, 8, 16
KV_RANK = 32
D_FF = 96
MOE_FF = 32
N_EXPERTS = 8
TOP_K = 3
N_LAYERS = 4
MAX_SEQ = 64
EOS = 7
ZERO_EMBED_ROW = 5


def variant_options(variant: str) -> dict:
    if variant == "kimi":
        return {"model_type": "kimi_k2", "q_lora_rank": 48, "n_group": 4, "topk_group": 2,
                "n_shared_experts": 1, "bias_f32": True, "routed_scaling_factor": 2.827,
                "seed": 0x4B49_4D49_4B32_3621, "rms_norm_eps": 1e-06}
    return {"model_type": "deepseek_v3", "q_lora_rank": None, "n_group": 1, "topk_group": 1,
            "n_shared_experts": 2, "bias_f32": False, "routed_scaling_factor": 2.446,
            "seed": 0x4D4F_4F4E_4C49_4748, "rms_norm_eps": 1e-05}


def config_for(o: dict) -> dict:
    return {
        "architectures": ["DeepseekV3ForCausalLM"],
        "attention_bias": False,
        "attention_dropout": 0.0,
        "aux_loss_alpha": 0.001,
        "bos_token_id": 1,
        "eos_token_id": EOS,
        "ep_size": 1,
        "first_k_dense_replace": 1,
        "hidden_act": "silu",
        "hidden_size": D_MODEL,
        "initializer_range": 0.02,
        "intermediate_size": D_FF,
        "kv_lora_rank": KV_RANK,
        "max_position_embeddings": 256,
        "model_type": o["model_type"],
        "moe_intermediate_size": MOE_FF,
        "moe_layer_freq": 1,
        "n_group": o["n_group"],
        "n_routed_experts": N_EXPERTS,
        "n_shared_experts": o["n_shared_experts"],
        "norm_topk_prob": True,
        "num_attention_heads": N_HEADS,
        "num_experts_per_tok": TOP_K,
        "num_hidden_layers": N_LAYERS,
        "num_key_value_heads": N_HEADS,
        "num_nextn_predict_layers": 0,
        "pretraining_tp": 1,
        "q_lora_rank": o["q_lora_rank"],
        "qk_nope_head_dim": NOPE,
        "qk_rope_head_dim": ROPE,
        "rms_norm_eps": o["rms_norm_eps"],
        "rope_scaling": None,
        "rope_theta": 50000.0,
        "routed_scaling_factor": o["routed_scaling_factor"],
        "scoring_func": "sigmoid",
        "seq_aux": True,
        "tie_word_embeddings": False,
        "topk_group": o["topk_group"],
        "topk_method": "noaux_tc",
        "torch_dtype": "bfloat16",
        "use_cache": True,
        "v_head_dim": V_HEAD,
        "vocab_size": VOCAB,
    }


CASES = [
    {"id": "zero-embedding-token", "prompt_tokens": [ZERO_EMBED_ROW], "max_tokens": 8,
     "eos": [EOS], "selection": "rp64-argmax"},
    {"id": "four-token-greedy", "prompt_tokens": [11, 42, 250, 3], "max_tokens": 10,
     "eos": [EOS], "selection": "argmax"},
    {"id": "nine-token-rp64", "prompt_tokens": [1, 99, 123, EOS, 64, 299, 17, 200, 17], "max_tokens": 12,
     "eos": [EOS], "selection": "rp64-argmax"},
    {"id": "repeated-token-greedy", "prompt_tokens": [256, 256, 256], "max_tokens": 9,
     "eos": [], "selection": "argmax"},
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
    r = rng.next()
    return bf16((r >> 63) & 1, lo_exp + (r >> 40) % n_exp, (r >> 56) & 0x7F)


def matrix(rng: Lcg, rows: int, cols: int, lo_exp: int = 120, n_exp: int = 5) -> list:
    # Exponents 120..124: magnitudes in [2^-7, 2^-2), RMS about 0.1.
    return [[weight_bits(rng, lo_exp, n_exp) for _ in range(cols)] for _ in range(rows)]


def norm(rng: Lcg, width: int) -> list:
    # Gains in [0.5, 2): biased exponent 126 or 127, positive.
    return [bf16(0, 126 + (rng.next() >> 50) % 2, (rng.next() >> 56) & 0x7F) for _ in range(width)]


def f32_bias(rng: Lcg) -> int:
    """A small F32 value (|v| < 2^-2) with random sign and mantissa."""
    r = rng.next()
    return ((r >> 63) << 31) | ((118 + (r >> 40) % 7) << 23) | ((r >> 20) & 0x7F_FFFF)


def layer_tensors(rng: Lcg, o: dict, layer: int) -> list:
    """(name, dtype, shape, values) for one layer, Hugging Face names."""
    p = f"model.layers.{layer}"
    out = []
    attn_norm, ffn_norm = norm(rng, D_MODEL), norm(rng, D_MODEL)
    dq = N_HEADS * (NOPE + ROPE)
    if o["q_lora_rank"] is None:
        wq = matrix(rng, dq, D_MODEL)
        if layer == 0:
            # Row maximum tied between +v and -v; subnormals far below it; negative zero.
            wq[3][0], wq[3][1] = bf16(0, 125, 0x55), bf16(1, 125, 0x55)
            wq[3][2], wq[3][3], wq[3][4] = 0x0001, 0x807F, 0x8000
        out.append((f"{p}.self_attn.q_proj.weight", "BF16", [dq, D_MODEL], wq))
    else:
        r = o["q_lora_rank"]
        out.append((f"{p}.self_attn.q_a_proj.weight", "BF16", [r, D_MODEL], matrix(rng, r, D_MODEL)))
        out.append((f"{p}.self_attn.q_a_layernorm.weight", "BF16", [r], norm(rng, r)))
        out.append((f"{p}.self_attn.q_b_proj.weight", "BF16", [dq, r], matrix(rng, dq, r)))
    kv_a = matrix(rng, KV_RANK + ROPE, D_MODEL)
    kv_b = matrix(rng, N_HEADS * (NOPE + V_HEAD), KV_RANK)
    if layer == 1:
        kv_b[2] = [0x0000] * KV_RANK  # an all-zero row of the key block (a zero column of wk_b)
        kv_a[KV_RANK + 1] = [0x8000] * D_MODEL  # an all-zero RoPE-key row
    out.append((f"{p}.self_attn.kv_a_proj_with_mqa.weight", "BF16", [KV_RANK + ROPE, D_MODEL], kv_a))
    out.append((f"{p}.self_attn.kv_a_layernorm.weight", "BF16", [KV_RANK], norm(rng, KV_RANK)))
    out.append((f"{p}.self_attn.kv_b_proj.weight", "BF16", [N_HEADS * (NOPE + V_HEAD), KV_RANK], kv_b))
    out.append((f"{p}.self_attn.o_proj.weight", "BF16", [D_MODEL, N_HEADS * V_HEAD],
                matrix(rng, D_MODEL, N_HEADS * V_HEAD)))
    out.append((f"{p}.self_attn.rotary_emb.inv_freq", "BF16", [ROPE // 2],
                [bf16(0, 127 - i, 0) for i in range(ROPE // 2)]))
    if layer == 0:
        attn_norm[0:6] = [0x37C0, 0xB7C0, 0x3820, 0x0003, 0x8000, bf16(1, 126, 0x40)]
    out.append((f"{p}.input_layernorm.weight", "BF16", [D_MODEL], attn_norm))
    out.append((f"{p}.post_attention_layernorm.weight", "BF16", [D_MODEL], ffn_norm))
    if layer == 0:
        down = matrix(rng, D_MODEL, D_FF)
        down[5][0] = bf16(0, 125, 0x7F)
        down[5][1], down[5][2] = bf16(1, 105, 0x01), bf16(0, 85, 0x00)
        out.append((f"{p}.mlp.gate_proj.weight", "BF16", [D_FF, D_MODEL], matrix(rng, D_FF, D_MODEL)))
        out.append((f"{p}.mlp.up_proj.weight", "BF16", [D_FF, D_MODEL], matrix(rng, D_FF, D_MODEL)))
        out.append((f"{p}.mlp.down_proj.weight", "BF16", [D_MODEL, D_FF], down))
        return out
    gate = matrix(rng, N_EXPERTS, D_MODEL, lo_exp=121, n_exp=4)
    if o["bias_f32"]:
        bias = [f32_bias(rng) for _ in range(N_EXPERTS)]
    else:
        bias = [weight_bits(rng, 116, 6) for _ in range(N_EXPERTS)]
    if layer in (1, 3):
        # Experts 2 and 5 (one group apart for kimi): identical router rows and
        # biases, so their selection keys tie exactly at every position.
        gate[5] = list(gate[2])
        bias[5] = bias[2]
    out.append((f"{p}.mlp.gate.weight", "BF16", [N_EXPERTS, D_MODEL], gate))
    out.append((f"{p}.mlp.gate.e_score_correction_bias", "F32" if o["bias_f32"] else "BF16",
                [N_EXPERTS], bias))
    sf = o["n_shared_experts"] * MOE_FF
    out.append((f"{p}.mlp.shared_experts.gate_proj.weight", "BF16", [sf, D_MODEL], matrix(rng, sf, D_MODEL)))
    out.append((f"{p}.mlp.shared_experts.up_proj.weight", "BF16", [sf, D_MODEL], matrix(rng, sf, D_MODEL)))
    out.append((f"{p}.mlp.shared_experts.down_proj.weight", "BF16", [D_MODEL, sf], matrix(rng, D_MODEL, sf)))
    for e in range(N_EXPERTS):
        q = f"{p}.mlp.experts.{e}"
        out.append((f"{q}.gate_proj.weight", "BF16", [MOE_FF, D_MODEL], matrix(rng, MOE_FF, D_MODEL)))
        out.append((f"{q}.up_proj.weight", "BF16", [MOE_FF, D_MODEL], matrix(rng, MOE_FF, D_MODEL)))
        out.append((f"{q}.down_proj.weight", "BF16", [D_MODEL, MOE_FF], matrix(rng, D_MODEL, MOE_FF)))
    return out


def shards(o: dict) -> list:
    """Two shards: [embed, layers 0-1] and [layers 2-3, norm, lm_head]."""
    rng = Lcg(o["seed"])
    embed = matrix(rng, VOCAB, D_MODEL, lo_exp=121)
    embed[ZERO_EMBED_ROW] = [0x8000 if j % 3 == 0 else 0x0000 for j in range(D_MODEL)]
    first = [("model.embed_tokens.weight", "BF16", [VOCAB, D_MODEL], embed)]
    second = []
    for layer in range(N_LAYERS):
        (first if layer < 2 else second).extend(layer_tensors(rng, o, layer))
    second.append(("model.norm.weight", "BF16", [D_MODEL], norm(rng, D_MODEL)))
    second.append(("lm_head.weight", "BF16", [VOCAB, D_MODEL], matrix(rng, VOCAB, D_MODEL)))
    return [first, second]


def safetensors_bytes(items: list) -> bytes:
    header = {"__metadata__": {"format": "pt"}}
    blobs, offset = [], 0
    for name, dtype, shape, values in items:
        flat = np.array(values, dtype=np.int64).reshape(-1)
        if flat.size != int(np.prod(shape)):
            raise AssertionError(name)
        blob = flat.astype("<u4" if dtype == "F32" else "<u2").tobytes()
        header[name] = {"dtype": dtype, "shape": shape, "data_offsets": [offset, offset + len(blob)]}
        blobs.append(blob)
        offset += len(blob)
    text = json.dumps(header, sort_keys=True, separators=(",", ":")).encode("ascii")
    text += b" " * (-len(text) % 8)
    return struct.pack("<Q", len(text)) + text + b"".join(blobs)


def json_bytes(obj) -> bytes:
    return (json.dumps(obj, indent=2, sort_keys=False) + "\n").encode("ascii")


def write(outdir: Path, variant: str = "moonlight") -> dict:
    o = variant_options(variant)
    outdir.mkdir(parents=True, exist_ok=True)
    files = {"config.json": (json.dumps(config_for(o), indent=2, sort_keys=True) + "\n").encode("ascii")}
    for i, items in enumerate(shards(o), start=1):
        files[f"model-0000{i}-of-00002.safetensors"] = safetensors_bytes(items)
    for name, data in files.items():
        (outdir / name).write_bytes(data)
    manifest = {
        "schema": "arc.hf-source.v1",
        "repo": f"arc-test/tiny-mla-{variant}",
        "revision": "0" * 40,
        "license": "apache-2.0",
        "url_template": "https://huggingface.co/{repo}/resolve/{revision}/{name}",
        "max_seq": MAX_SEQ,
        "files": [{"name": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                  for name, data in files.items()],
    }
    (outdir / "tiny-mla.source.json").write_bytes(json_bytes(manifest))
    (outdir / "tiny-mla.cases.json").write_bytes(json_bytes({"schema": "arc.modern-cases.v1", "cases": CASES}))
    return manifest


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("outdir")
    parser.add_argument("--variant", choices=["moonlight", "kimi"], default="moonlight")
    args = parser.parse_args(argv)
    manifest = write(Path(args.outdir), args.variant)
    for entry in manifest["files"]:
        print(f"{entry['name']}: {entry['bytes']} bytes sha256 {entry['sha256']}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
