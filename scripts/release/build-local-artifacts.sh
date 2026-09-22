#!/usr/bin/env bash
# Build every locally-supported release artifact for THIS macOS arm64 host,
# in strict serial order, with provenance - for developer verification only.
#
#   scripts/release/build-local-artifacts.sh OUTPUT_DIR
#
# What this produces, in this exact order:
#   1. the arc-node release binary, via the existing
#      scripts/arc-build-provenance.sh (its own provenance manifest and
#      immutable digest-named copy land under OUTPUT_DIR/arc-node-provenance/)
#   2. the arc CLI release binary (crates/arc-cli), if that crate exists
#   3. the desktop bundle for this host - UNSIGNED, NOT A RELEASE
#   4. SHA256SUMS over every artifact this script copied into OUTPUT_DIR
#   5. a CycloneDX SBOM, via scripts/release/sbom.py
#
# THIS SCRIPT NEVER SIGNS, UPLOADS, OR PUBLISHES ANYTHING. The desktop bundle
# step explicitly disables Tauri's updater-artifact/signing step
# (bundle.createUpdaterArtifacts) via a one-off --config override, and the
# script refuses to run at all if an updater signing key, an Apple signing
# identity or certificate, or a notarization credential is present in its own
# environment. Every artifact this script produces lands in OUTPUT_DIR, which
# must be given as an argument and must be new or empty (cargo, npm ci and the
# Tauri bundler still use their usual build directories inside the checkout).
# Nothing is pushed, and no GitHub API or release-publication call is made.
#
# Refuses to run at all if a soak may be in progress: this repeats the exact
# process-name checks specified for this script (pgrep -f arc-soak, pgrep -f
# /tmp/arc-provenance/arc-node), independent of whatever else is running.
#
# This script is written to be run, not to be run by an agent editing this
# repository during a soak: see the header comment block end for the exact
# command to run once the soak this repo is currently measuring has finished.
set -Eeuo pipefail

die() {
    printf 'build-local-artifacts: %s\n' "$*" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# 0. Preflight: soak guard, host check, required tools, argument validation.
# ---------------------------------------------------------------------------

# A 24h soak writes an immutable arc-node binary under /tmp/arc-provenance and
# drives it from a module whose own working directory is named arc-soak-*, so
# both patterns below catch it by substring match against the full command
# line (pgrep -f). Refuse unconditionally rather than trying to tell "my own
# build" apart from "a running soak" - this script must never contend with a
# soak for CPU, memory, or the shared /tmp/arc-provenance immutable-binary
# naming convention.
if pgrep -f arc-soak >/dev/null 2>&1; then
    die "refusing to build: a process matching 'arc-soak' is running (soak in progress?)"
fi
if pgrep -f /tmp/arc-provenance/arc-node >/dev/null 2>&1; then
    die "refusing to build: a process matching '/tmp/arc-provenance/arc-node' is running"
fi

CALLER_PWD="$(pwd -P)"
SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd -P)"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) ;;
    *) die "this script only builds the locally-supported artifacts for macOS arm64 (host reports $(uname -s)-$(uname -m))" ;;
esac

for tool in cargo npm node python3 git shasum pgrep; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool not found on PATH: $tool"
done

[ $# -eq 1 ] && [ -n "$1" ] || die "usage: $0 OUTPUT_DIR"
case "$1" in
    -h|--help) echo "usage: $0 OUTPUT_DIR"; exit 0 ;;
esac
RAW_OUTPUT_DIR="$1"

case "$RAW_OUTPUT_DIR" in
    /*) OUTPUT_ARG_ABS="$RAW_OUTPUT_DIR" ;;
    *) OUTPUT_ARG_ABS="$CALLER_PWD/$RAW_OUTPUT_DIR" ;;
esac
OUTPUT_PARENT_RAW="$(dirname -- "$OUTPUT_ARG_ABS")"
OUTPUT_BASENAME="$(basename -- "$OUTPUT_ARG_ABS")"
case "$OUTPUT_BASENAME" in
    ''|.|..) die "refusing unsafe OUTPUT_DIR: $RAW_OUTPUT_DIR" ;;
esac
mkdir -p -- "$OUTPUT_PARENT_RAW" || die "cannot create parent of OUTPUT_DIR: $OUTPUT_PARENT_RAW"
OUTPUT_PARENT="$(CDPATH='' cd -- "$OUTPUT_PARENT_RAW" && pwd -P)" || die "OUTPUT_DIR parent does not resolve"
OUTPUT_DIR="$OUTPUT_PARENT/$OUTPUT_BASENAME"
case "$OUTPUT_DIR" in
    "$REPO_ROOT"|"$REPO_ROOT"/) die "refusing the repository root itself as OUTPUT_DIR" ;;
    "$HOME"|/) die "refusing an unsafe OUTPUT_DIR: $OUTPUT_DIR" ;;
esac
[ ! -e "$OUTPUT_DIR" ] || [ -d "$OUTPUT_DIR" ] || die "OUTPUT_DIR exists and is not a directory: $OUTPUT_DIR"
[ ! -L "$OUTPUT_DIR" ] || die "refusing a symlinked OUTPUT_DIR: $OUTPUT_DIR"
# A leftover file from an earlier or failed run would be summed into
# SHA256SUMS and verified as if this build had produced it, so only a new or
# empty directory is accepted.
if [ -d "$OUTPUT_DIR" ]; then
    OUTPUT_ENTRIES="$(ls -A -- "$OUTPUT_DIR")" || die "cannot list OUTPUT_DIR: $OUTPUT_DIR"
    [ -z "$OUTPUT_ENTRIES" ] || die "refusing a non-empty OUTPUT_DIR (use a new directory): $OUTPUT_DIR"
fi
mkdir -p -- "$OUTPUT_DIR"

# Never let a signing key or notarization credential reach this script's
# environment, even by inheritance from the caller's shell - the desktop step
# must be structurally incapable of producing a signed updater artifact, a
# Developer ID signature or a notarization upload. These are the names the
# pinned Tauri CLI (2.11.x) reads: the updater key (TAURI_SIGNING_*, and the
# v1 TAURI_PRIVATE_KEY* names it still recognises), a macOS signing identity
# (APPLE_SIGNING_IDENTITY, or a certificate it imports from
# APPLE_CERTIFICATE), and notarization (APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID,
# or APPLE_API_KEY/APPLE_API_ISSUER/APPLE_API_KEY_PATH). A set-but-empty
# variable is not refused, only unset.
SIGNING_ENV_NAMES=(
    TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD TAURI_SIGNING_PRIVATE_KEY_PATH
    TAURI_PRIVATE_KEY TAURI_PRIVATE_KEY_PASSWORD TAURI_PRIVATE_KEY_PATH
    APPLE_SIGNING_IDENTITY APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD
    APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_PROVIDER_SHORT_NAME
    APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH
)
for name in "${SIGNING_ENV_NAMES[@]}"; do
    [ -z "${!name:-}" ] || die "refusing to run with $name set in the environment (no signing or notarization input may reach this build)"
done
unset "${SIGNING_ENV_NAMES[@]}"

sha256_file() {
    shasum -a 256 "$1" | awk '{print $1}'
}

echo "=============================================================="
echo " ARC local artifact build - UNSIGNED, NOT A RELEASE"
echo " Output directory: $OUTPUT_DIR"
echo " Nothing here is signed, uploaded, or published."
echo "=============================================================="

cd "$REPO_ROOT"
REVISION="$(git rev-parse HEAD 2>/dev/null || echo not-a-git-checkout)"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)"
[ -n "$VERSION" ] || die "could not read the workspace version from Cargo.toml"

COLLECTED_DIR="$OUTPUT_DIR/collected"
mkdir -p "$COLLECTED_DIR"

# ---------------------------------------------------------------------------
# 1. arc-node release binary, via the existing provenance script.
# ---------------------------------------------------------------------------
echo
echo "--- [1/5] arc-node release binary (scripts/arc-build-provenance.sh) ---"
ARC_NODE_PROVENANCE_DIR="$OUTPUT_DIR/arc-node-provenance"
mkdir -p "$ARC_NODE_PROVENANCE_DIR"
ARC_NODE_IMMUTABLE="$("$REPO_ROOT/scripts/arc-build-provenance.sh" --profile release --out "$ARC_NODE_PROVENANCE_DIR" | tail -n1)"
[ -n "$ARC_NODE_IMMUTABLE" ] && [ -f "$ARC_NODE_IMMUTABLE" ] \
    || die "arc-build-provenance.sh did not report a built binary"
cp -- "$ARC_NODE_IMMUTABLE" "$COLLECTED_DIR/arc-node"
echo "arc-node: $ARC_NODE_IMMUTABLE -> $COLLECTED_DIR/arc-node"

# ---------------------------------------------------------------------------
# 2. arc CLI release binary, if crates/arc-cli exists.
# ---------------------------------------------------------------------------
echo
echo "--- [2/5] arc CLI release binary (crates/arc-cli) ---"
if [ -d "$REPO_ROOT/crates/arc-cli" ]; then
    cargo build --release -p arc-cli --locked
    CLI_BIN="$REPO_ROOT/target/release/arc"
    [ -f "$CLI_BIN" ] || die "cargo reported success but $CLI_BIN is missing"
    cp -- "$CLI_BIN" "$COLLECTED_DIR/arc-cli"
    {
        echo "binary:        arc (package crates/arc-cli)"
        echo "source_revision: $REVISION"
        echo "profile:       release"
        echo "binary_sha256: $(sha256_file "$CLI_BIN")"
        echo "binary_bytes:  $(wc -c < "$CLI_BIN" | tr -d ' ')"
    } > "$OUTPUT_DIR/arc-cli-provenance.txt"
    echo "arc-cli: $CLI_BIN -> $COLLECTED_DIR/arc-cli"
else
    echo "crates/arc-cli does not exist in this checkout; skipping (no CLI to build)."
fi

# ---------------------------------------------------------------------------
# 3. Desktop bundle for this host - UNSIGNED, NOT A RELEASE.
#
# --bundles app,dmg restricts output to the two macOS-native formats (no
# Linux/Windows formats are buildable here anyway). The --config override
# force-disables bundle.createUpdaterArtifacts for this invocation only - it
# is never written to the committed tauri.conf.json - so the updater
# artifact/signing code path in tauri-plugin-updater's bundler is never
# entered at all, regardless of this host's Tauri CLI version. macOS code
# signing still runs with tauri.conf.json's own "-" (ad-hoc) identity, which
# is what every unsigned local Tauri build on macOS already does and is not a
# distribution-capable signature.
# ---------------------------------------------------------------------------
echo
echo "--- [3/5] desktop bundle (UNSIGNED, NOT A RELEASE) ---"
DESKTOP_OUT="$OUTPUT_DIR/desktop-unsigned"
mkdir -p "$DESKTOP_OUT"
(
    cd "$REPO_ROOT/desktop"
    npm ci
    npm run tauri:build -- \
        --bundles app,dmg \
        --config '{"bundle":{"createUpdaterArtifacts":false}}'
)
BUNDLE_ROOT="$REPO_ROOT/desktop/src-tauri/target/release/bundle"
[ -d "$BUNDLE_ROOT" ] || die "expected Tauri bundle output missing: $BUNDLE_ROOT"
FOUND_BUNDLE_ITEM=0
while IFS= read -r -d '' item; do
    FOUND_BUNDLE_ITEM=1
    name="$(basename -- "$item")"
    if [ -d "$item" ]; then
        # A macOS .app is a directory; archive it so it is one hashable file,
        # exactly as an updater artifact would be, minus any signature.
        tar -C "$(dirname -- "$item")" -czf "$DESKTOP_OUT/UNSIGNED-$name.tar.gz" "$name"
    else
        cp -- "$item" "$DESKTOP_OUT/UNSIGNED-$name"
    fi
done < <(find "$BUNDLE_ROOT" -mindepth 1 -maxdepth 2 \( -type f -o -name '*.app' \) -print0)
[ "$FOUND_BUNDLE_ITEM" -eq 1 ] || die "Tauri build produced no bundle artifacts under $BUNDLE_ROOT"
cat > "$DESKTOP_OUT/NOT-A-RELEASE.txt" <<EOF
UNSIGNED desktop bundle built locally by scripts/release/build-local-artifacts.sh
on $(date -u +%Y-%m-%dT%H:%M:%SZ) from revision $REVISION.

This is NOT a signed production release. bundle.createUpdaterArtifacts was
force-disabled for this build, no updater signing key, Apple signing
identity or certificate, or notarization credential was present or used, and
nothing in this directory was uploaded or published. Do not
distribute these files as, or describe them as, an ARC Chain release.
EOF
# Loud on purpose: a desktop artifact missing from collected/ would also be
# missing from SHA256SUMS.
shopt -s nullglob
UNSIGNED_ITEMS=("$DESKTOP_OUT"/UNSIGNED-*)
shopt -u nullglob
[ "${#UNSIGNED_ITEMS[@]}" -gt 0 ] || die "no UNSIGNED-* desktop artifact to collect under $DESKTOP_OUT"
cp -- "${UNSIGNED_ITEMS[@]}" "$COLLECTED_DIR"/ || die "could not copy the desktop artifacts into $COLLECTED_DIR"
echo "desktop bundle (unsigned): $BUNDLE_ROOT -> $DESKTOP_OUT"

# ---------------------------------------------------------------------------
# 4. SHA256SUMS over every artifact collected above.
# ---------------------------------------------------------------------------
echo
echo "--- [4/5] SHA256SUMS ---"
(
    cd "$COLLECTED_DIR"
    find . -type f -print0 | LC_ALL=C sort -z | xargs -0 shasum -a 256 | sed 's#  \./#  #'
) > "$OUTPUT_DIR/SHA256SUMS"
echo "wrote $OUTPUT_DIR/SHA256SUMS ($(wc -l < "$OUTPUT_DIR/SHA256SUMS" | tr -d ' ') entries)"

# ---------------------------------------------------------------------------
# 5. CycloneDX SBOM, via scripts/release/sbom.py.
# ---------------------------------------------------------------------------
echo
echo "--- [5/5] CycloneDX SBOM (scripts/release/sbom.py) ---"
SBOM_ARGS=(--product arc-chain --version "$VERSION"
    --cargo-lock "$REPO_ROOT/Cargo.lock"
    --npm-lock "$REPO_ROOT/desktop/package-lock.json"
    --out "$OUTPUT_DIR/arc-chain.cdx.json")
if [ -f "$REPO_ROOT/desktop/src-tauri/Cargo.lock" ]; then
    SBOM_ARGS+=(--cargo-lock "$REPO_ROOT/desktop/src-tauri/Cargo.lock")
fi
python3 "$REPO_ROOT/scripts/release/sbom.py" "${SBOM_ARGS[@]}"

echo
echo "=============================================================="
echo " Done. Everything is under: $OUTPUT_DIR"
echo " UNSIGNED. NOT A RELEASE. Nothing was signed, uploaded, or published."
echo "=============================================================="
