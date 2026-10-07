#!/usr/bin/env bash
# Weight slices (docs/protocol/integer-profile-mla-moe-dyadic-v1.md section 14)
# on tiny checkpoints stored the way Kimi-K2.6 stores its weights.
#
#   bash scripts/arc_mla/slice_checks.sh ARC_MLA_BINARY WORKDIR EVIDENCE_DIR
#
# For the tiny "kimi" model as a packed multimodal checkpoint (plain, with
# YaRN, and with -8 values and a zero-scale group):
#   1. streamed slicing, one shard at a time with each shard deleted after use,
#      gives the same manifest as slicing with every shard present, on 1 thread
#      and on all threads, and with 1, 2, 4 and 8 expert groups the same
#      segment digests;
#   2. the independent Python preparer computes the same slice manifest
#      (manifest_blake3);
#   3. without YaRN: the segments equal the i4g32 conversion of the BF16 twin
#      (the spec 13.3 quantiser produced the packed values), a stage package
#      assembled from the slices is byte-identical to the converter's package
#      (whole model and a 2-layer stage), and the model roots agree;
#   4. slice-verify re-hashes every slice and every segment;
#   5. assembled packages execute through the engine, exact across kernels and
#      pipeline splits; absent vision is harmless, YaRN/i8 fail before output.
# Writes EVIDENCE_DIR/slices-summary.json (one manifest digest per variant) for
# the cross-OS comparison. Works with bash on Linux, macOS and Windows (Git
# Bash). Exits non-zero on the first mismatch.
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
same() {
  if [ "$2" != "$3" ]; then
    echo "MISMATCH $1: $2 != $3" >&2
    exit 1
  fi
  echo "ok   $1"
}

summary="$EV/slices-summary.json"
echo "{" > "$summary"
first=1
for variant in plain yarn edge; do
  src="$WORK/$variant"
  out="$EV/$variant"
  mkdir -p "$out"
  flag=""
  [ "$variant" != plain ] && flag="--$variant"
  # shellcheck disable=SC2086
  python "$ROOT/scripts/arc_mla/make_tiny_kimi_packed.py" "$src" $flag > "$out/generator.txt"
  manifest="$src/tiny-kimi-packed.source.json"
  echo "== $variant"
  # 1. Streamed (shards deleted after use) vs every shard present.
  cp -R "$src" "$WORK/$variant-stream"
  python "$ROOT/scripts/arc_mla/stream_slices.py" --arc-mla "$BIN" --source-manifest "$manifest" \
    --work "$WORK/$variant-stream" --out "$WORK/$variant-streamed" --expert-groups 4 \
    --report "$out/stream-report.json" > "$out/stream.txt"
  left=0
  for f in "$WORK/$variant-stream"/model-*.safetensors; do
    [ -e "$f" ] && left=$((left + 1))
  done
  same "$variant: shards left after streaming (only the unused vision shard)" "$left" 1
  "$BIN" slice --source-dir "$src" --source-manifest "$manifest" --out-dir "$WORK/$variant-all" \
    --expert-groups 4 --threads 1 > /dev/null
  "$BIN" slice-manifest --source-dir "$src" --source-manifest "$manifest" --out-dir "$WORK/$variant-all" \
    --expert-groups 4 --out "$out/manifest-all-1-thread.json" > /dev/null
  streamed="$(field "$WORK/$variant-streamed/manifest.json" manifest_blake3)"
  same "$variant: streamed vs all shards on 1 thread" "$streamed" \
    "$(field "$out/manifest-all-1-thread.json" manifest_blake3)"
  cp "$WORK/$variant-streamed/manifest.json" "$out/manifest.json"
  "$BIN" slice-verify --manifest "$out/manifest.json" --slices "$WORK/$variant-streamed" --segments \
    > "$out/verify.json"
  for groups in 1 2 8; do
    "$BIN" slice --source-dir "$src" --source-manifest "$manifest" --out-dir "$WORK/$variant-g$groups" \
      --expert-groups "$groups" --discard > /dev/null
    "$BIN" slice-manifest --source-dir "$src" --source-manifest "$manifest" --out-dir "$WORK/$variant-g$groups" \
      --expert-groups "$groups" --out "$out/manifest-g$groups.json" > /dev/null
    python - "$out/manifest.json" "$out/manifest-g$groups.json" <<'PY'
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:])
assert a["segments"] == b["segments"], "segment digests depend on the expert grouping"
assert a["slices"] != b["slices"]
PY
    echo "ok   $variant: $groups expert groups, same segments"
  done
  # 2. The independent Python preparer.
  py_ref slices --source-dir "$src" --source-manifest "$manifest" --expert-groups 4 \
    --json-out "$out/python-manifest.json" > /dev/null
  same "$variant: Rust vs Python slice manifest" "$streamed" "$(field "$out/python-manifest.json" manifest_blake3)"
  # 3. Equivalence with the BF16 twin and with the converter's packages.
  if [ "$variant" = plain ]; then
    python "$ROOT/scripts/arc_mla/make_tiny_mla_model.py" "$WORK/bf16" --variant kimi > /dev/null
    "$BIN" convert --source-dir "$WORK/bf16" --source-manifest "$WORK/bf16/tiny-mla.source.json" \
      --experts i4g32 --out "$WORK/bf16.arcspkg" --manifest-out "$out/bf16-stage-manifest.json" > /dev/null
    "$BIN" convert --source-dir "$src" --source-manifest "$manifest" --experts i4g32 \
      --out "$WORK/packed.arcspkg" --manifest-out "$out/packed-stage-manifest.json" --report "$out/convert.json" > /dev/null
    python - "$out/manifest.json" "$out/bf16-stage-manifest.json" "$out/packed-stage-manifest.json" <<'PY'
import json, sys
slices, twin, packed = (json.load(open(p)) for p in sys.argv[1:])
weights = lambda segs: [s for s in segs if s["name"] != "tables"]
assert slices["segments"] == weights(twin["segments"]), "packed slices differ from the BF16 twin"
assert slices["segments"] == weights(packed["segments"])
assert slices["model_root"] == packed["model_root"], "model roots differ"
PY
    echo "ok   plain: segments = BF16 twin's i4g32 conversion; model root = converter's"
    "$BIN" slice-assemble --manifest "$out/manifest.json" --slices "$WORK/$variant-streamed" \
      --out "$WORK/assembled.arcspkg" > "$out/assemble.json"
    same "plain: assembled package vs converter package (sha256)" \
      "$(field "$out/assemble.json" package sha256)" "$(field "$out/convert.json" package sha256)"
    "$BIN" slice-assemble --manifest "$out/manifest.json" --slices "$WORK/$variant-streamed" \
      --layers 1:3 --out "$WORK/assembled-1-3.arcspkg" > "$out/assemble-1-3.json"
    "$BIN" convert --source-dir "$src" --source-manifest "$manifest" --experts i4g32 --layers 1:3 \
      --out "$WORK/packed-1-3.arcspkg" --report "$out/convert-1-3.json" > /dev/null
    same "plain: assembled stage [1, 3) vs converter stage" \
      "$(field "$out/assemble-1-3.json" package sha256)" "$(field "$out/convert-1-3.json" package sha256)"
    "$BIN" verify --package "$WORK/assembled.arcspkg" --manifest "$out/packed-stage-manifest.json" > /dev/null
    echo "ok   plain: the assembled package verifies against the stage manifest"
  fi
  [ $first = 1 ] || echo "," >> "$summary"
  first=0
  printf '  "%s": "%s"' "$variant" "$streamed" >> "$summary"
done
printf '\n}\n' >> "$summary"
cat "$summary"
python "$ROOT/scripts/arc_mla/check_slice_engine.py" "$BIN" "$WORK" "$EV"
echo "slice checks: all passed"

# Explicit versioned preparation: fresh synthetic fixture only, never weights.
python "$ROOT/scripts/arc_mla/check_yarn_slices.py" "$BIN" "$WORK/prepared-yarn" "$EV/prepared-yarn"
