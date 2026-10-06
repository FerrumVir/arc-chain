#!/usr/bin/env bash
# GPU exactness on the tiny SmolLM3-shaped model (scripts/arc_modern/make_tiny_model.py):
# the CPU golden run, the GPU golden run (one token per pass and three per
# pass) and the Proof Kit's gpu-check must agree bit for bit, and the CPU run
# must still give the tiny model's pinned digest.
#
#   scripts/arc_modern/gpu_tiny_check.sh TINY_DIR EVIDENCE_DIR
#
# ARC_MODERN overrides the binary (default ./target/release/arc-modern).
# The GPU adapter comes from ARC_GPU_ADAPTER / WGPU_BACKEND.
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 TINY_DIR EVIDENCE_DIR" >&2
    exit 2
fi
bin="${ARC_MODERN:-./target/release/arc-modern}"
tiny="$1"
out="$2"
mkdir -p "$out"

python scripts/arc_modern/make_tiny_model.py "$tiny"
"$bin" convert --source-dir "$tiny" --source-manifest "$tiny/tiny-smollm3.source.json" \
    --out "$tiny/tiny.arcipkg" > "$out/tiny-convert.json"
cases="$tiny/tiny-smollm3.cases.json"
"$bin" golden --package "$tiny/tiny.arcipkg" --cases "$cases" --kernel scalar \
    --out "$out/tiny-cpu.json"
"$bin" golden --package "$tiny/tiny.arcipkg" --cases "$cases" --gpu \
    --out "$out/tiny-gpu.json"
"$bin" golden --package "$tiny/tiny.arcipkg" --cases "$cases" --gpu --gpu-batch 3 \
    --out "$out/tiny-gpu-batch3.json"
"$bin" gpu-check --package "$tiny/tiny.arcipkg" --cases "$cases" --golden "$out/tiny-cpu.json" \
    --out "$out/tiny-gpu-check.json" --self-test-rounds 20 --trace-forwards 8

python - "$out" <<'PY'
import json
import sys
from pathlib import Path

out = Path(sys.argv[1])
digests = {
    name: json.loads((out / name).read_text())["matrix_digest"]
    for name in ("tiny-cpu.json", "tiny-gpu.json", "tiny-gpu-batch3.json")
}
print(json.dumps(digests, indent=1))
pinned = "98cc9928cd019679b729a0983e15b3689e1429efe7ca795be89b05d7eb240918"
assert digests["tiny-cpu.json"] == pinned, "the tiny model's CPU digest changed"
assert len(set(digests.values())) == 1, "CPU and GPU tiny-model digests differ"
check = json.loads((out / "tiny-gpu-check.json").read_text())
gpu = check["gpu"]["adapter"]
print(f"gpu-check pass={check['pass']} on {gpu['name']} ({gpu['backend']}, {gpu['driver']} {gpu['driver_info']})")
print(f"trace check: {json.dumps(check['trace_check'])}")
assert check["pass"], json.dumps(check, indent=1)
assert check["trace_check"]["all_equal"], "per-layer trace hashes differ"
PY
