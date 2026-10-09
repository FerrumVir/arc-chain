#!/usr/bin/env bash
# Tiny MLA + MoE models through every cross-check of
# docs/protocol/integer-profile-mla-moe-dyadic-v1.md section 12.
#
#   bash scripts/arc_mla/tiny_checks.sh ARC_MLA_BINARY WORKDIR EVIDENCE_DIR
#
# Historical INT8 BF16-matrix comparison: the independent Python preparer
# implements this policy only. Kimi creation now defaults to INT16, so select
# --historical-int8 explicitly for every compared whole/stage conversion.
# Separate Kimi controls below require omitted == explicit INT16 != historical.
# For the Moonlight-like and the Kimi-like tiny model, each with INT8 dyadic
# experts and with INT4 group-32 experts (spec section 13):
#   1. the Rust converter and the independent Python preparer produce the same
#      bytes for the whole model and for every stage of the 2- and 4-stage
#      layouts; every stage package verifies against the whole-model manifest;
#   2. Rust generation with the scalar and the SIMD kernel, independent Python
#      generation and Python teacher-forced verification agree on every
#      logits hash, boundary digest and token;
#   3. the same sequences through 1, 2 and 4 stages, each stage a separate
#      process fed only the previous stage's boundary file (kernels alternate
#      scalar/SIMD along the 4-stage pipeline), match the single-process run;
#   4. the Python executor replays every stage of the 4-stage layout from the
#      Rust boundary files and reproduces the next boundary.
# Works with bash on Linux, macOS and Windows (Git Bash). Exits non-zero on the
# first mismatch.
set -euo pipefail

BIN="$1"
WORK="$2"
EV="$3"
ROOT="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd -P)"
mkdir -p "$WORK" "$EV"

py_ref() { (cd "$ROOT/scripts" && python -m arc_conformance.mla_moe_reference "$@"); }
field() { python -c 'import json,sys; d=json.load(open(sys.argv[1]))
for k in sys.argv[2:]: d=d[k]
print(d)' "$@"; }

for combo in moonlight:i8 kimi:i8 moonlight:i4g32 kimi:i4g32; do
  variant="${combo%%:*}"
  fmt="${combo##*:}"
  src="$WORK/$variant"
  out="$EV/$variant-$fmt"
  pkgs="$WORK/$variant-$fmt"
  mkdir -p "$out"
  python "$ROOT/scripts/arc_mla/make_tiny_mla_model.py" "$src" --variant "$variant" > "$out/generator.txt"
  manifest="$src/tiny-mla.source.json"
  cases="$src/tiny-mla.cases.json"
  full="$pkgs-0-4.arcspkg"
  echo "== $variant ($fmt experts): historical INT8 matrix packages (Rust converter vs Python preparer)"
  "$BIN" convert --historical-int8 --source-dir "$src" --source-manifest "$manifest" --experts "$fmt" --out "$full" \
    --manifest-out "$out/manifest.json" --report "$out/convert-0-4.json" > /dev/null
  if [ "$variant" = kimi ]; then
    # The Python preparer is intentionally not reused as an INT16 oracle.
    # Verify that retaining historical comparisons has not hidden the new default.
    python -c 'import json,sys; json.dump(dict(version=1, **{k:"int16" for k in ("attention","dense","shared","embedding","head")}),open(sys.argv[1],"w"))' "$out/int16-policy.json"
    "$BIN" convert --source-dir "$src" --source-manifest "$manifest" --experts "$fmt" \
      --out "$pkgs-default.arcspkg" --manifest-out "$out/default-manifest.json" > /dev/null
    "$BIN" convert --source-dir "$src" --source-manifest "$manifest" --experts "$fmt" \
      --precision "$out/int16-policy.json" --out "$pkgs-int16.arcspkg" \
      --manifest-out "$out/int16-manifest.json" > /dev/null
    python - "$full" "$pkgs-default.arcspkg" "$pkgs-int16.arcspkg" "$out" <<'PYCONTROL'
import hashlib, json, struct, sys
from pathlib import Path
legacy, default, explicit = (Path(p).read_bytes() for p in sys.argv[1:4])
out = Path(sys.argv[4])
def model(raw):
    return json.loads(raw[16:16 + struct.unpack_from('<Q', raw, 8)[0]])['model']
wide = json.loads((out / 'int16-policy.json').read_bytes())
assert model(legacy).get('precision') is None, 'Python comparison requires historical matrix policy'
assert model(default)['precision'] == model(explicit)['precision'] == wide
assert default == explicit, 'omitted policy differs from explicit INT16 bytes'
assert default != legacy, 'INT16 default silently became historical INT8'
a, b = ((out / (name + '-manifest.json')).read_bytes() for name in ('default', 'int16'))
assert a == b, 'omitted/explicit INT16 manifests differ'
assert json.loads(a)['model_root'] != json.loads((out / 'manifest.json').read_bytes())['model_root']
(out / 'creation-policy-check.json').write_text(json.dumps(dict(
    historical_precision=None, default_precision=wide, explicit_precision=wide,
    default_explicit_bytes_and_manifest_equal=True, historical_identity_distinct=True,
    package_sha256={k: hashlib.sha256(v).hexdigest() for k,v in
                    [('historical',legacy),('default',default),('explicit',explicit)]}), indent=2)+'\n')
PYCONTROL
  fi
  for layers in 0:4 0:2 2:4 0:1 1:2 2:3 3:4; do
    tag="${layers/:/-}"
    if [ "$tag" != "0-4" ]; then
      "$BIN" convert --historical-int8 --source-dir "$src" --source-manifest "$manifest" --layers "$layers" --experts "$fmt" \
        --out "$pkgs-$tag.arcspkg" --report "$out/convert-$tag.json" > /dev/null
    fi
    py_ref prepare --source-dir "$src" --source-manifest "$manifest" --layers "$layers" --experts "$fmt" \
      --hash-only --json-out "$out/prepare-$tag.json" > /dev/null
    rust="$(field "$out/convert-$tag.json" package sha256)"
    python_sha="$(field "$out/prepare-$tag.json" sha256)"
    if [ "$rust" != "$python_sha" ]; then
      echo "$variant $fmt [$layers]: Rust package $rust != Python package $python_sha" >&2
      exit 1
    fi
    "$BIN" verify --package "$pkgs-$tag.arcspkg" --manifest "$out/manifest.json" \
      --full-digest > "$out/verify-$tag.json"
    echo "$variant $fmt [$layers]: sha256 $rust (Rust = Python; segments verified against the manifest)"
  done

  echo "== $variant ($fmt experts): generation (Rust scalar, Rust SIMD, Python)"
  for kernel in scalar simd; do
    "$BIN" golden --package "$full" --cases "$cases" --kernel "$kernel" --out "$out/run-$kernel.json" > /dev/null
  done
  py_ref generate --package "$full" --cases "$cases" --out "$out/run-python.json" > /dev/null
  py_ref verify-run --package "$full" --run "$out/run-scalar.json" --out "$out/verify-run.json" > /dev/null
  python - "$out" <<'PY'
import json, sys
from pathlib import Path
out = Path(sys.argv[1])
runs = {n: json.loads((out / n).read_text()) for n in ("run-scalar.json", "run-simd.json", "run-python.json")}
for key in ("matrix_digest", "boundary_matrix_digest", "model_root"):
    values = {n: r[key] for n, r in runs.items()}
    print(key, json.dumps(values, indent=1))
    assert len(set(values.values())) == 1, f"{key} differs between Rust scalar, Rust SIMD and Python"
golden = json.loads((out / "verify-run.json").read_text())
assert golden["checks"]["all_match"], "Python teacher-forced verification found a mismatch"
print("python verify-run: all logits hashes, boundary digests and tokens match")
PY

  echo "== $variant ($fmt experts): 1, 2 and 4 stages as separate processes"
  run="$out/run-scalar.json"
  "$BIN" stage --package "$full" --run "$run" --out "$pkgs-one-b4.bin" \
    --report "$out/one-0-4.json" --manifest "$out/manifest.json" --kernel simd > /dev/null
  "$BIN" stage --package "$pkgs-0-2.arcspkg" --run "$run" --out "$pkgs-two-b2.bin" \
    --report "$out/two-0-2.json" --manifest "$out/manifest.json" --kernel scalar > /dev/null
  "$BIN" stage --package "$pkgs-2-4.arcspkg" --input "$pkgs-two-b2.bin" \
    --out "$pkgs-two-b4.bin" --report "$out/two-2-4.json" --manifest "$out/manifest.json" \
    --kernel simd > /dev/null
  previous=""
  kernel=scalar
  four_reports=""
  for tag in 0-1 1-2 2-3 3-4; do
    end="${tag#*-}"
    if [ -z "$previous" ]; then
      input=(--run "$run")
    else
      input=(--input "$previous")
    fi
    "$BIN" stage --package "$pkgs-$tag.arcspkg" "${input[@]}" --out "$pkgs-four-b$end.bin" \
      --report "$out/four-$tag.json" --manifest "$out/manifest.json" --kernel "$kernel" > /dev/null
    if [ -z "$previous" ]; then
      py_ref stage-replay --package "$pkgs-$tag.arcspkg" --run "$run" --report "$out/py-replay-$tag.json" > /dev/null
    else
      py_ref stage-replay --package "$pkgs-$tag.arcspkg" --input "$previous" \
        --report "$out/py-replay-$tag.json" > /dev/null
    fi
    previous="$pkgs-four-b$end.bin"
    four_reports="${four_reports:+$four_reports,}$out/four-$tag.json"
    if [ "$kernel" = scalar ]; then kernel=simd; else kernel=scalar; fi
  done
  python "$ROOT/scripts/arc_mla/layout_check.py" --run "$run" --label "tiny $variant, $fmt experts" \
    --layout "one=$out/one-0-4.json" \
    --layout "two=$out/two-0-2.json,$out/two-2-4.json" \
    --layout "four=$four_reports" \
    --replay "python-0-1=$out/py-replay-0-1.json" --replay "python-1-2=$out/py-replay-1-2.json" \
    --replay "python-2-3=$out/py-replay-2-3.json" --replay "python-3-4=$out/py-replay-3-4.json" \
    --out "$out/layout-check.json" --summary-md "$out/layout-check.md"
done
echo "tiny checks: all passed"
