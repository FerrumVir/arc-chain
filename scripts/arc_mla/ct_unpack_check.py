#!/usr/bin/env python3
"""Pin the INT4 repack against compressed-tensors' own unpacker on a real shard.

    python scripts/arc_mla/ct_unpack_check.py --shard SHARD.safetensors \\
        --manifest SLICES/manifest.json --slices SLICES --layer 1 [--experts 0,1,190,383] \\
        [--out CHECK.json]

For every sampled routed expert of the layer and each of its three
projections, compressed_tensors.unpack_from_int32 (the library the checkpoint
was written with) unpacks ``weight_packed`` to int8 values; the script reads
the same expert's values from ARC's expert-group slice (spec section 13.1:
value j in byte j/2, low nibble for even j, two's complement) and requires
them to be equal, and the slice's BF16 group scales to equal ``weight_scale``
bit for bit. This is the check kimi-k26-checkpoint.md asked for before the
repacker could be trusted: the nibble order and value offset, pinned against
the producer's own code rather than a reading of it.

Needs torch, safetensors and compressed-tensors.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np
import torch
from safetensors import safe_open

try:
    from compressed_tensors.compressors.pack_quantized.helpers import unpack_from_int32
except ImportError:  # older layout
    from compressed_tensors.compressors.quantized_compressors.pack_quantized import unpack_from_int32

import compressed_tensors

PROJECTIONS = (("w_gate", "gate_proj"), ("w_up", "up_proj"), ("w_down", "down_proj"))


def slice_part(manifest: dict, slices: Path, tensor: str, expert: int) -> np.ndarray:
    """The bytes of one expert's part of a stacked tensor, read from its group slice."""
    for s in manifest["slices"]:
        lo, hi = s["experts"] or (None, None)
        if lo is None or not lo <= expert < hi:
            continue
        for t in s["tensors"]:
            if t["name"] == tensor:
                per_expert = t["bytes"] // (hi - lo)
                with open(slices / f"{s['blake3']}.slice", "rb") as f:
                    f.seek(t["offset"] + (expert - lo) * per_expert)
                    return np.frombuffer(f.read(per_expert), dtype=np.uint8)
    raise SystemExit(f"no slice holds {tensor} of expert {expert}")


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--shard", required=True)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--slices", required=True)
    parser.add_argument("--layer", type=int, required=True)
    parser.add_argument("--experts", default="0,1,190,383")
    parser.add_argument("--prefix", default="language_model.")
    parser.add_argument("--out")
    args = parser.parse_args(argv)
    manifest = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
    slices = Path(args.slices)
    experts = [int(x) for x in args.experts.split(",")]
    rows = []
    with safe_open(args.shard, framework="pt") as f:
        for expert in experts:
            for short, proj in PROJECTIONS:
                base = f"{args.prefix}model.layers.{args.layer}.mlp.experts.{expert}.{proj}"
                packed = f.get_tensor(f"{base}.weight_packed")
                shape = torch.Size(f.get_tensor(f"{base}.weight_shape").tolist())
                scale = f.get_tensor(f"{base}.weight_scale")
                theirs = unpack_from_int32(packed, 4, shape).numpy().astype(np.int64)
                q4 = slice_part(manifest, slices, f"layers.{args.layer}.experts.{short}.q4", expert)
                nibbles = np.empty(2 * q4.size, dtype=np.int64)
                nibbles[0::2], nibbles[1::2] = q4 & 0x0F, q4 >> 4
                ours = np.where(nibbles > 7, nibbles - 16, nibbles).reshape(theirs.shape)
                s = slice_part(manifest, slices, f"layers.{args.layer}.experts.{short}.s", expert)
                scale_bits = scale.view(torch.int16).numpy().astype(np.uint16).reshape(-1)
                row = {
                    "expert": expert,
                    "projection": proj,
                    "shape": list(shape),
                    "values_equal": bool(np.array_equal(ours, theirs)),
                    "scales_equal": bool(np.array_equal(s.view("<u2"), scale_bits)),
                    "min": int(theirs.min()),
                    "max": int(theirs.max()),
                    "count_minus8": int((theirs == -8).sum()),
                }
                rows.append(row)
                print(json.dumps(row))
    ok = all(r["values_equal"] and r["scales_equal"] for r in rows)
    report = {"compressed_tensors": compressed_tensors.__version__, "torch": torch.__version__,
              "layer": args.layer, "checked": len(rows), "all_equal": ok, "rows": rows}
    if args.out:
        Path(args.out).write_text(json.dumps(report, indent=1) + "\n", encoding="utf-8")
    print(f"compressed-tensors {compressed_tensors.__version__}: {len(rows)} expert projections, "
          f"all equal: {ok}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
