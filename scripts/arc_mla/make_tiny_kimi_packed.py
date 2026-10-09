#!/usr/bin/env python3
"""Write a tiny checkpoint stored the way Kimi-K2.6 stores its weights.

Usage: python3 scripts/arc_mla/make_tiny_kimi_packed.py OUTDIR [--yarn] [--edge]

The weights are those of make_tiny_mla_model.py's "kimi" variant (query LoRA,
group routing, one shared expert, an F32 correction bias). The storage follows
Kimi-K2.6 (moonshotai/Kimi-K2.6 at 7eb5002f6aadc958aed6a9177b7ed26bb94011bb):

* config.json wraps the text model in ``text_config`` (``model_type``
  ``kimi_k2``) inside a ``kimi_k25`` multimodal configuration, with the
  compressed-tensors ``pack-quantized`` block (INT4, symmetric, group 32);
* every language-model tensor is under ``language_model.``; a small
  ``vision_tower.*`` and ``mm_projector.*`` stand in for MoonViT;
* routed experts are ``weight_packed`` (I32 ``[rows, cols/8]``, value ``j`` at
  bits ``4(j mod 8)`` of word ``j/8``, stored as ``v + 8``), ``weight_scale``
  (BF16 ``[rows, cols/32]``) and ``weight_shape`` (I32 ``[2]``); everything
  else is BF16 (the bias F32);
* one shard per layer (layer l in shard l+1), then the embedding, final norm
  and LM head, then the vision tower, with ``model.safetensors.index.json``.

The INT4 values and scales are exactly what the spec section 13.3 quantiser
makes of the BF16 "kimi" tiny model, so the slices of this checkpoint must
equal, segment for segment, the i4g32 conversion of that BF16 model.

``--yarn`` adds Kimi-K2.6's ``rope_scaling`` (YaRN, factor 64): the weights
still slice, but the profile does not define the RoPE preparation, so the
slice manifest has no ``model`` object and no model root.

``--edge`` changes two groups of layer 1, expert 0, gate_proj: one holds the
value -8 (which the section 13.3 quantiser never produces but checkpoints
quantised elsewhere do), one is all zero with a zero scale. That model has no
BF16 twin.

Writes OUTDIR/config.json, the shards, model.safetensors.index.json,
tiny-kimi-packed.source.json (arc.hf-source.v1 with an ``index`` entry) and
tiny-kimi-packed.cases.json.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import struct
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent))

import make_tiny_mla_model as tiny  # noqa: E402
from arc_conformance.mla_moe_reference import quantize_q4_rows, unpack_q4  # noqa: E402

PREFIX = "language_model."
QUANTIZATION = {
    "config_groups": {
        "group_0": {
            "input_activations": None,
            "output_activations": None,
            "targets": ["Linear"],
            "weights": {
                "actorder": None,
                "block_structure": None,
                "dynamic": False,
                "group_size": 32,
                "num_bits": 4,
                "observer": "minmax",
                "observer_kwargs": {},
                "strategy": "group",
                "symmetric": True,
                "type": "int",
            },
        }
    },
    "format": "pack-quantized",
    "ignore": ["re:.*self_attn.*", "re:.*shared_experts.*", "re:.*mlp\\.(gate|up|gate_up|down)_proj.*",
               "re:.*lm_head.*", "re:vision_tower.*", "re:mm_projector.*"],
    "kv_cache_scheme": None,
    "quant_method": "compressed-tensors",
    "quantization_status": "compressed",
}
YARN = {"beta_fast": 32.0, "beta_slow": 1.0, "factor": 64.0, "mscale": 1.0, "mscale_all_dim": 1.0,
        "original_max_position_embeddings": 4096, "type": "yarn"}


def pack_words(values: np.ndarray) -> np.ndarray:
    """compressed-tensors pack_to_int32 (num_bits 4, packed_dim 1): [rows, cols] -> int32 [rows, cols/8]."""
    v = (np.asarray(values, dtype=np.int64) + 8).astype(np.uint64)
    rows, cols = v.shape
    words = np.zeros((rows, cols // 8), dtype=np.uint64)
    for i in range(8):
        words |= v[:, i::8] << np.uint64(4 * i)
    return words.astype(np.uint32).view(np.int32)


def packed_expert(name: str, bits: list, edge: bool) -> list:
    """BF16 expert matrix -> its three compressed-tensors tensors (spec 13.3 values)."""
    b = np.array(bits, dtype=np.int64)
    rows, cols = b.shape
    packed, scales = quantize_q4_rows(b, what=name)
    q = unpack_q4(packed)
    scales = scales.astype(np.int64)
    if edge:
        # Group 0 of row 0 holds -8; group 1 of row 1 is all zero with scale 0.
        q[0, 0], q[0, 5] = -8, -8
        q[1, 32:64] = 0
        scales[1, 1] = 0
    base = name[: -len(".weight")]
    return [
        (f"{base}.weight_packed", "I32", [rows, cols // 8], pack_words(q)),
        (f"{base}.weight_scale", "BF16", [rows, cols // 32], scales),
        (f"{base}.weight_shape", "I32", [2], np.array([rows, cols], dtype=np.int64)),
    ]


def safetensors_bytes(items: list) -> bytes:
    header = {"__metadata__": {"format": "pt"}}
    blobs, offset = [], 0
    for name, dtype, shape, values in items:
        flat = np.array(values, dtype=np.int64).reshape(-1)
        if flat.size != int(np.prod(shape)):
            raise AssertionError(name)
        layout = {"F32": "<u4", "I32": "<i4", "BF16": "<u2"}[dtype]
        if dtype == "I32":
            flat = flat.astype(np.int32)
        blob = flat.astype(layout).tobytes()
        header[name] = {"dtype": dtype, "shape": shape, "data_offsets": [offset, offset + len(blob)]}
        blobs.append(blob)
        offset += len(blob)
    text = json.dumps(header, sort_keys=True, separators=(",", ":")).encode("ascii")
    text += b" " * (-len(text) % 8)
    return struct.pack("<Q", len(text)) + text + b"".join(blobs)


def write(outdir: Path, yarn: bool = False, edge: bool = False, prepared: bool = False) -> dict:
    previous_dims = tiny.NOPE, tiny.ROPE
    if prepared:
        tiny.NOPE, tiny.ROPE = 128, 64
        yarn = True
    o = tiny.variant_options("kimi")
    first, second = tiny.shards(o)
    by_name = {name: (dtype, shape, values) for name, dtype, shape, values in first + second}
    n_shards = tiny.N_LAYERS + 2
    shard_items: list = [[] for _ in range(n_shards)]
    for name, (dtype, shape, values) in by_name.items():
        if name.startswith("model.layers."):
            layer = int(name.split(".")[2])
            target = layer
        else:
            target = tiny.N_LAYERS
        if ".mlp.experts." in name:
            is_edge = edge and name == "model.layers.1.mlp.experts.0.gate_proj.weight"
            shard_items[target].extend(
                (PREFIX + n, d, s, v) for n, d, s, v in packed_expert(name, values, is_edge))
        else:
            shard_items[target].append((PREFIX + name, dtype, shape, values))
    rng = tiny.Lcg(0x5649_5349_4F4E_3236)
    shard_items[-1] = [
        ("vision_tower.encoder.blocks.0.wqkv.weight", "BF16", [24, 8], tiny.matrix(rng, 24, 8)),
        ("vision_tower.patch_embed.proj.weight", "BF16", [8, 12], tiny.matrix(rng, 8, 12)),
        ("mm_projector.proj.0.weight", "BF16", [tiny.D_MODEL, 8], tiny.matrix(rng, tiny.D_MODEL, 8)),
        ("mm_projector.pre_norm.bias", "BF16", [8], tiny.norm(rng, 8)),
    ]
    text = tiny.config_for(o)
    text["rope_scaling"] = YARN if yarn else None
    text["quantization_config"] = QUANTIZATION
    config = {
        "architectures": ["KimiK25ForConditionalGeneration"],
        "bos_token_id": 1,
        "eos_token_id": tiny.EOS,
        "model_type": "kimi_k25",
        "text_config": text,
        "tie_word_embeddings": False,
        "vision_config": {"model_type": "moonvit", "hidden_size": 8},
    }
    outdir.mkdir(parents=True, exist_ok=True)
    files = {"config.json": (json.dumps(config, indent=2, sort_keys=True) + "\n").encode("ascii")}
    weight_map = {}
    for i, items in enumerate(shard_items, start=1):
        name = f"model-{i:05d}-of-{n_shards:05d}.safetensors"
        files[name] = safetensors_bytes(items)
        weight_map.update({t[0]: name for t in items})
    index = {"metadata": {"total_size": sum(len(v) for k, v in files.items() if k.endswith(".safetensors"))},
             "weight_map": dict(sorted(weight_map.items()))}
    index_bytes = (json.dumps(index, indent=2) + "\n").encode("ascii")
    for name, data in files.items():
        (outdir / name).write_bytes(data)
    (outdir / "model.safetensors.index.json").write_bytes(index_bytes)
    tag = "-yarn" if yarn else ""
    tag += "-edge" if edge else ""
    tag += "-prepared" if prepared else ""
    manifest = {
        "schema": "arc.hf-source.v1",
        "repo": f"arc-test/tiny-kimi-packed{tag}",
        "revision": "0" * 40,
        "license": "apache-2.0",
        "url_template": "https://huggingface.co/{repo}/resolve/{revision}/{name}",
        "max_seq": tiny.MAX_SEQ,
        "files": [{"name": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                  for name, data in files.items()],
        "index": {"name": "model.safetensors.index.json", "bytes": len(index_bytes),
                  "sha256": hashlib.sha256(index_bytes).hexdigest()},
    }
    (outdir / "tiny-kimi-packed.source.json").write_bytes(tiny.json_bytes(manifest))
    (outdir / "tiny-kimi-packed.cases.json").write_bytes(
        tiny.json_bytes({"schema": "arc.modern-cases.v1", "cases": tiny.CASES}))
    tiny.NOPE, tiny.ROPE = previous_dims
    return manifest


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("outdir")
    parser.add_argument("--yarn", action="store_true", help="add Kimi-K2.6's YaRN rope_scaling")
    parser.add_argument("--edge", action="store_true", help="a -8 value and a zero-scale group")
    parser.add_argument("--prepared", action="store_true", help="distinct synthetic fixture with K2.6 YaRN head dimensions")
    args = parser.parse_args(argv)
    manifest = write(Path(args.outdir), args.yarn, args.edge, args.prepared)
    for entry in manifest["files"] + [manifest["index"]]:
        print(f"{entry['name']}: {entry['bytes']} bytes sha256 {entry['sha256']}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
