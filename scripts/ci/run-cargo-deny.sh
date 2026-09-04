#!/usr/bin/env bash
# Run the release-blocking dependency policy with a checksummed cargo-deny
# binary. Keeping the installer here makes CI and the tag workflow use the
# exact same scanner and avoids Docker images silently selecting a different
# Rust toolchain or cargo-deny release.
set -euo pipefail

# Resolve physically. MANIFEST_DIR below uses `pwd -P`, so a logical `pwd` here
# makes MANIFEST_ABS and ROOT_MANIFEST/DESKTOP_MANIFEST disagree on any checkout
# path containing a symlink (/tmp on macOS, a symlinked CI workspace).
# SHADOW_PROFILE then stays empty, the whole vendored-advisory scan is skipped,
# and this release gate exits 0 without having checked anything.
SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd -P)"
ARGUMENT_COUNT="$#"
MANIFEST_PATH="${1:-$REPO_ROOT/Cargo.toml}"
VERSION=0.20.2

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64|Linux-amd64)
        TARGET=x86_64-unknown-linux-musl
        EXPECTED_SHA256=9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f
        ;;
    Linux-aarch64|Linux-arm64)
        TARGET=aarch64-unknown-linux-musl
        EXPECTED_SHA256=995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d
        ;;
    Darwin-arm64|Darwin-aarch64)
        TARGET=aarch64-apple-darwin
        EXPECTED_SHA256=fe67d82a10d8597a3549364cb733a3f9cc1bfff9031b7ae46384a9f2a72090c3
        ;;
    Darwin-x86_64|Darwin-amd64)
        TARGET=x86_64-apple-darwin
        EXPECTED_SHA256=248da7f581724e470071990c088ffc55c811981715f4cbdb258621fb79f8b7a6
        ;;
    *)
        printf 'Unsupported cargo-deny host: %s/%s\n' "$(uname -s)" "$(uname -m)" >&2
        exit 2
        ;;
esac

TEMP_PARENT="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
WORK_DIR="$(mktemp -d "$TEMP_PARENT/arc-cargo-deny.XXXXXX")"
trap 'rm -rf -- "$WORK_DIR"' EXIT

ARCHIVE="$WORK_DIR/cargo-deny.tar.gz"
ASSET="cargo-deny-$VERSION-$TARGET.tar.gz"
curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    "https://github.com/EmbarkStudios/cargo-deny/releases/download/$VERSION/$ASSET" \
    --output "$ARCHIVE"

if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$EXPECTED_SHA256" "$ARCHIVE" | sha256sum -c -
else
    printf '%s  %s\n' "$EXPECTED_SHA256" "$ARCHIVE" | shasum -a 256 -c -
fi

tar -xzf "$ARCHIVE" -C "$WORK_DIR"
CARGO_DENY="$WORK_DIR/cargo-deny-$VERSION-$TARGET/cargo-deny"
"$CARGO_DENY" --version
"$CARGO_DENY" \
    --config "$REPO_ROOT/deny.toml" \
    --manifest-path "$MANIFEST_PATH" \
    --locked \
    check advisories bans sources licenses

# cargo-deny intentionally omits local-path packages from advisory matching.
# After proving each provenance-pinned path patch is reachable in the real
# graph, project only those packages under their exact canonical registry
# identities and re-scan them with the database refreshed above. The root
# profile requires zero findings for all three Wasmer patches under a generated
# suppression-free policy. The desktop profile still
# requires exactly the one glib advisory whose upstream fix is carried locally.
MANIFEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$MANIFEST_PATH")" && pwd -P)"
MANIFEST_ABS="$MANIFEST_DIR/$(basename -- "$MANIFEST_PATH")"
ROOT_MANIFEST="$REPO_ROOT/Cargo.toml"
DESKTOP_MANIFEST="$REPO_ROOT/desktop/src-tauri/Cargo.toml"
SHADOW_PROFILE=
case "$MANIFEST_ABS" in
    "$ROOT_MANIFEST")
        SHADOW_PROFILE=root
        ;;
    "$DESKTOP_MANIFEST")
        SHADOW_PROFILE=desktop
        ;;
esac

# Skipping the shadow scan is legitimate only for a caller-supplied manifest
# that is neither the workspace root nor desktop. With no argument we defaulted
# to the root manifest, so an empty profile means path resolution disagreed and
# this gate would pass silently. Fail loudly instead.
if [ "$ARGUMENT_COUNT" -eq 0 ] && [ -z "$SHADOW_PROFILE" ]; then
    printf '%s: default manifest %s did not resolve to workspace root %s\n' \
        "$0" "$MANIFEST_ABS" "$ROOT_MANIFEST" >&2
    exit 2
fi

if [ -n "$SHADOW_PROFILE" ]; then
    ACTUAL_METADATA="$WORK_DIR/$SHADOW_PROFILE-metadata.json"
    SHADOW_METADATA="$WORK_DIR/$SHADOW_PROFILE-registry-shadow-metadata.json"
    SHADOW_REPORT="$WORK_DIR/$SHADOW_PROFILE-registry-shadow-advisories.jsonl"
    SHADOW_DIAGNOSTICS="$WORK_DIR/$SHADOW_PROFILE-registry-shadow-diagnostics.jsonl"
    SHADOW_POLICY="$WORK_DIR/vendored-registry-shadow-deny.toml"
    SHADOW_HELPER="$SCRIPT_DIR/vendored-advisory-shadow.py"

    cargo metadata \
        --manifest-path "$MANIFEST_ABS" \
        --format-version 1 \
        --locked \
        --offline >"$ACTUAL_METADATA"
    python3 "$SHADOW_HELPER" rewrite-metadata \
        "$SHADOW_PROFILE" \
        "$ACTUAL_METADATA" \
        "$SHADOW_METADATA"
    python3 "$SHADOW_HELPER" write-policy \
        "$REPO_ROOT/deny.toml" \
        "$SHADOW_POLICY"

    set +e
    "$CARGO_DENY" \
        --config "$SHADOW_POLICY" \
        --manifest-path "$MANIFEST_ABS" \
        --metadata-path "$SHADOW_METADATA" \
        --format json \
        --color never \
        --offline \
        check --audit-compatible-output advisories \
        >"$SHADOW_REPORT" 2>"$SHADOW_DIAGNOSTICS"
    SHADOW_EXIT=$?
    set -e

    if ! python3 "$SHADOW_HELPER" verify-report \
        "$SHADOW_PROFILE" \
        "$SHADOW_REPORT" \
        "$SHADOW_DIAGNOSTICS" \
        "$SHADOW_POLICY" \
        "$SHADOW_EXIT"; then
        sed -n '1,160p' "$SHADOW_DIAGNOSTICS" >&2
        exit 1
    fi
fi
