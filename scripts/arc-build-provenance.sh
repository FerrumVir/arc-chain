#!/usr/bin/env bash
# Build one arc-node binary through Cargo and record what it actually is.
#
# The `.rs` mtime guard in the fixture catches only the class of staleness it
# was written for. It cannot see a changed manifest, lockfile, toolchain,
# feature set or generated input, and copied or restored timestamps fool it. It
# is kept as a cheap supplement; this script is the record of provenance.
#
# The binary is copied to an immutable, digest-named path and that copy is what
# the run executes, so the artifact cannot change underneath a long fixture.
#
# Usage: scripts/arc-build-provenance.sh [--profile debug|release] [--out DIR]
set -euo pipefail

PROFILE=debug
OUT=${OUT:-/tmp/arc-provenance}
FEATURES=""
while [[ $# -gt 0 ]]; do
  case $1 in
    --profile) PROFILE=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --features) FEATURES=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

export PATH="$HOME/.local/bin:$PATH"
mkdir -p "$OUT"
stamp=$(date -u +%Y%m%dT%H%M%SZ)
manifest="$OUT/provenance-$stamp.txt"

cargo_args=(build -p arc-node --locked)
[[ $PROFILE == release ]] && cargo_args+=(--release)
[[ -n $FEATURES ]] && cargo_args+=(--features "$FEATURES")
cmd="cargo ${cargo_args[*]}"

# ── source identity ──────────────────────────────────────────────────────────
revision=$(git rev-parse HEAD 2>/dev/null || echo "not-a-git-checkout")
dirty=$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')
# A revision alone does not identify a dirty tree, so hash the exact inputs.
# Content, not timestamps: a restored mtime cannot forge this.
input_digest=$( { find crates scripts -type f \( -name '*.rs' -o -name '*.toml' -o -name '*.sh' \) -print0 2>/dev/null | sort -z | xargs -0 shasum -a 256; \
                  shasum -a 256 Cargo.toml Cargo.lock 2>/dev/null; } | shasum -a 256 | cut -d' ' -f1)

echo "=== ARC build provenance ===" | tee "$manifest"
{
  echo "recorded_utc:     $stamp"
  echo "command:          $cmd"
  echo "workdir:          $(pwd -P)"
  echo "source_revision:  $revision"
  echo "dirty_files:      $dirty"
  echo "input_digest:     $input_digest   (content of crates/**, scripts/**, Cargo.toml, Cargo.lock)"
  echo "profile:          $PROFILE"
  echo "features:         ${FEATURES:-<default>}"
  echo "toolchain:        $(rustc --version) / $(cargo --version)"
  echo "host:             $(uname -srm)"
} | tee -a "$manifest"

echo "--- building ---" | tee -a "$manifest"
set +e
cargo "${cargo_args[@]}" >"$OUT/build-$stamp.log" 2>&1
build_exit=$?
set -e
echo "build_exit:       $build_exit" | tee -a "$manifest"
if [[ $build_exit -ne 0 ]]; then
  echo "BUILD FAILED; see $OUT/build-$stamp.log" | tee -a "$manifest"
  tail -20 "$OUT/build-$stamp.log" | tee -a "$manifest"
  exit $build_exit
fi

built="target/$PROFILE/arc-node"
[[ -f $built ]] || { echo "built binary missing at $built" | tee -a "$manifest"; exit 1; }
digest=$(shasum -a 256 "$built" | cut -d' ' -f1)
immutable="$OUT/arc-node-${digest:0:16}"
if [[ ! -f $immutable ]]; then
  cp "$built" "$immutable"
  chmod a-w "$immutable"
fi
{
  echo "binary_source:    $built"
  echo "binary_sha256:    $digest"
  echo "binary_bytes:     $(wc -c < "$built" | tr -d ' ')"
  echo "immutable_copy:   $immutable   (read-only; this is what the run executes)"
} | tee -a "$manifest"

ln -sfn "$manifest" "$OUT/latest-provenance.txt"
ln -sfn "$immutable" "$OUT/latest-arc-node"
echo "manifest:         $manifest" | tee -a "$manifest"
echo "$immutable"
