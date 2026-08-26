#!/usr/bin/env bash
# Assemble and validate the complete ARC release in one flat directory.
#
# This script is deliberately usable outside GitHub Actions.  The release
# workflow calls it after downloading every build artifact, while the release
# contract tests feed it fixtures.  A missing platform, updater signature, or
# checksum is therefore a hard failure before a GitHub release can be created.
set -Eeuo pipefail

ARTIFACTS_DIR="${ARTIFACTS_DIR:-artifacts}"
OUTPUT_DIR="${OUTPUT_DIR:-release-files}"
RELEASE_TAG="${RELEASE_TAG:-}"
REPOSITORY="${REPOSITORY:-FerrumVir/arc-chain}"
RELEASE_DATE="${RELEASE_DATE:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
GENESIS_FILE="${GENESIS_FILE:-genesis.toml}"
SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
GENESIS_VALIDATOR="$SCRIPT_DIR/validate-genesis.py"

die() {
    printf 'release assembly: %s\n' "$*" >&2
    exit 1
}

[ -n "$RELEASE_TAG" ] || die "RELEASE_TAG is required"
printf '%s\n' "$RELEASE_TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' \
    || die "release tag must be strict vX.Y.Z (got: $RELEASE_TAG)"
[ -d "$ARTIFACTS_DIR" ] || die "artifact directory does not exist: $ARTIFACTS_DIR"
[ -f install.sh ] || die "run from the repository root (install.sh is missing)"
[ -f testnet-seeds.txt ] || die "testnet-seeds.txt is missing"
[ -f "$GENESIS_FILE" ] || die "genesis file is missing: $GENESIS_FILE"
[ -f "$GENESIS_VALIDATOR" ] || die "genesis validator is missing: $GENESIS_VALIDATOR"
command -v python3 >/dev/null 2>&1 || die "python3 is required to generate latest.json"

# Validate before clearing or writing OUTPUT_DIR. A release may ship either a
# complete public-address-only validator set or the explicit empty migration
# placeholder used by stake-zero community observers. It must never package a
# deterministic seed, private key, partial validator set, or implicit mode.
python3 "$GENESIS_VALIDATOR" "$GENESIS_FILE" \
    || die "refusing to package unsafe genesis: $GENESIS_FILE"

case "$OUTPUT_DIR" in
    ''|/|.|..|"$PWD") die "refusing unsafe OUTPUT_DIR: $OUTPUT_DIR" ;;
esac
case "/$OUTPUT_DIR/" in
    *'/../'*|*'/./'*) die "refusing non-canonical OUTPUT_DIR: $OUTPUT_DIR" ;;
esac
[ ! -L "${OUTPUT_DIR%/}" ] || die "refusing symlinked OUTPUT_DIR: $OUTPUT_DIR"

rm -rf -- "$OUTPUT_DIR"
mkdir -p -- "$OUTPUT_DIR"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "sha256sum or shasum is required"
    fi
}

find_one() {
    local root="$1"
    local pattern="$2"
    local matches
    local count

    [ -d "$root" ] || die "missing artifact group: $root"
    matches="$(find "$root" -type f -name "$pattern" -print | LC_ALL=C sort)"
    count="$(printf '%s\n' "$matches" | awk 'NF { n += 1 } END { print n + 0 }')"
    [ "$count" -eq 1 ] || die "expected exactly one '$pattern' under $root; found $count"
    printf '%s\n' "$matches"
}

copy_as() {
    local source="$1"
    local destination="$2"
    [ -s "$source" ] || die "artifact is missing or empty: $source"
    [ ! -e "$OUTPUT_DIR/$destination" ] || die "release filename collision: $destination"
    cp -- "$source" "$OUTPUT_DIR/$destination"
}

# Headless assets have a stable, version-independent naming contract.  The
# installer and README use these names literally.
HEADLESS_ASSETS=(
    arc-node-linux-x86_64
    arc-cli-linux-x86_64
    arc-node-linux-arm64
    arc-cli-linux-arm64
    arc-node-macos-arm64
    arc-cli-macos-arm64
    arc-node-macos-x86_64
    arc-cli-macos-x86_64
    arc-node-windows-x86_64.exe
    arc-cli-windows-x86_64.exe
)

for asset in "${HEADLESS_ASSETS[@]}"; do
    copy_as "$(find_one "$ARTIFACTS_DIR" "$asset")" "$asset"
done

# Normalize Tauri's versioned/space-bearing bundle names into stable release
# names.  Renaming does not alter the updater payload bytes or their Tauri
# signatures, and gives README links that never need a version edit.
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-macos-arm64" '*.app.tar.gz')" \
    arc-desktop-macos-arm64.app.tar.gz
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-macos-arm64" '*.app.tar.gz.sig')" \
    arc-desktop-macos-arm64.app.tar.gz.sig
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-macos-arm64" '*.dmg')" \
    arc-desktop-macos-arm64.dmg

copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-macos-x86_64" '*.app.tar.gz')" \
    arc-desktop-macos-x86_64.app.tar.gz
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-macos-x86_64" '*.app.tar.gz.sig')" \
    arc-desktop-macos-x86_64.app.tar.gz.sig
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-macos-x86_64" '*.dmg')" \
    arc-desktop-macos-x86_64.dmg

copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-windows-x86_64" '*-setup.exe')" \
    arc-desktop-windows-x86_64-setup.exe
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-windows-x86_64" '*-setup.exe.sig')" \
    arc-desktop-windows-x86_64-setup.exe.sig
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-windows-x86_64" '*.msi')" \
    arc-desktop-windows-x86_64.msi

copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-linux-x86_64" '*.AppImage')" \
    arc-desktop-linux-x86_64.AppImage
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-linux-x86_64" '*.AppImage.sig')" \
    arc-desktop-linux-x86_64.AppImage.sig
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-linux-x86_64" '*.deb')" \
    arc-desktop-linux-x86_64.deb
copy_as "$(find_one "$ARTIFACTS_DIR/arc-desktop-linux-x86_64" '*.rpm')" \
    arc-desktop-linux-x86_64.rpm

# Ship the exact installer and network configuration from the validated tag
# commit; repository tag protection is a separate owner-controlled prerequisite.
# Auto-update installs this checksummed copy, not a moving main-branch script.
copy_as install.sh install.sh
copy_as testnet-seeds.txt testnet-seeds.txt
copy_as "$GENESIS_FILE" genesis.toml

VERSION="${RELEASE_TAG#v}"
BASE_URL="https://github.com/${REPOSITORY}/releases/download/${RELEASE_TAG}"
export OUTPUT_DIR VERSION RELEASE_TAG RELEASE_DATE BASE_URL

python3 <<'PY'
import json
import os
from pathlib import Path

root = Path(os.environ["OUTPUT_DIR"])
base = os.environ["BASE_URL"]

targets = {
    "darwin-aarch64": "arc-desktop-macos-arm64.app.tar.gz",
    "darwin-x86_64": "arc-desktop-macos-x86_64.app.tar.gz",
    "windows-x86_64": "arc-desktop-windows-x86_64-setup.exe",
    "linux-x86_64": "arc-desktop-linux-x86_64.AppImage",
}

platforms = {}
for target, filename in targets.items():
    payload = root / filename
    signature = root / f"{filename}.sig"
    if not payload.is_file() or payload.stat().st_size == 0:
        raise SystemExit(f"missing updater payload: {payload}")
    if not signature.is_file() or signature.stat().st_size == 0:
        raise SystemExit(f"missing updater signature: {signature}")
    platforms[target] = {
        "signature": signature.read_text(encoding="utf-8").strip(),
        "url": f"{base}/{filename}",
    }

manifest = {
    "version": os.environ["VERSION"],
    "notes": (
        f"ARC Node {os.environ['RELEASE_TAG']}. Automatic Linux desktop "
        "updates apply to the AppImage build; deb and rpm packages are "
        "updated by installing the new package."
    ),
    "pub_date": os.environ["RELEASE_DATE"],
    "platforms": platforms,
}

(root / "latest.json").write_text(
    json.dumps(manifest, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY

# Hash every published file (including Tauri signatures and latest.json).
# SHA256SUMS itself is intentionally the only file not listed in the manifest.
# shellcheck disable=SC2094 # find explicitly excludes the redirection target.
(
    cd "$OUTPUT_DIR"
    find . -maxdepth 1 -type f ! -name SHA256SUMS -print \
        | sed 's#^\./##' \
        | LC_ALL=C sort \
        | while IFS= read -r file; do
            printf '%s  %s\n' "$(sha256_file "$file")" "$file"
        done > SHA256SUMS
)

[ -s "$OUTPUT_DIR/SHA256SUMS" ] || die "failed to create SHA256SUMS"
printf 'release assembly: validated %s files for %s\n' \
    "$(find "$OUTPUT_DIR" -maxdepth 1 -type f | wc -l | tr -d ' ')" "$RELEASE_TAG"
