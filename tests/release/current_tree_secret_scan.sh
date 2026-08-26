#!/usr/bin/env bash
# Scan either the exact checked-out commit (the CI default) or every releasable
# tracked/untracked file in the local working tree. Never scan the already-
# compromised Git history. The pinned archive checksum prevents a replaced
# release asset from silently changing this blocking gate.
set -Eeuo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
GITLEAKS_VERSION=8.30.1
CONFIG_FILE="$TEST_DIR/gitleaks-current-tree.toml"
MATERIALIZER="$TEST_DIR/materialize_releasable_tree.py"
TEMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/arc-current-tree-secrets.XXXXXX")"
cleanup() { rm -rf -- "$TEMP_ROOT"; }
trap cleanup EXIT HUP INT TERM

SOURCE_MODE="${1:---commit}"
case "$SOURCE_MODE" in
    --commit|--worktree) ;;
    *)
        printf 'usage: %s [--commit|--worktree]\n' "$0" >&2
        exit 2
        ;;
esac

case "$(uname -s):$(uname -m)" in
    Linux:x86_64|Linux:amd64)
        PLATFORM=linux_x64
        ARCHIVE_SHA256=551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb
        ;;
    Linux:aarch64|Linux:arm64)
        PLATFORM=linux_arm64
        ARCHIVE_SHA256=e4a487ee7ccd7d3a7f7ec08657610aa3606637dab924210b3aee62570fb4b080
        ;;
    Darwin:x86_64)
        PLATFORM=darwin_x64
        ARCHIVE_SHA256=dfe101a4db2255fc85120ac7f3d25e4342c3c20cf749f2c20a18081af1952709
        ;;
    Darwin:arm64)
        PLATFORM=darwin_arm64
        ARCHIVE_SHA256=b40ab0ae55c505963e365f271a8d3846efbc170aa17f2607f13df610a9aeb6a5
        ;;
    *)
        printf 'secret scan: unsupported platform %s/%s\n' "$(uname -s)" "$(uname -m)" >&2
        exit 1
        ;;
esac

for command_name in curl git tar; do
    command -v "$command_name" >/dev/null 2>&1 || {
        printf 'secret scan: required command is missing: %s\n' "$command_name" >&2
        exit 1
    }
done

ARCHIVE="$TEMP_ROOT/gitleaks.tar.gz"
GITLEAKS_BIN="$TEMP_ROOT/gitleaks"
curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    "https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/gitleaks_${GITLEAKS_VERSION}_${PLATFORM}.tar.gz" \
    >"$ARCHIVE"
if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$ARCHIVE_SHA256" "$ARCHIVE" | sha256sum -c - >/dev/null
elif command -v shasum >/dev/null 2>&1; then
    printf '%s  %s\n' "$ARCHIVE_SHA256" "$ARCHIVE" | shasum -a 256 -c - >/dev/null
else
    printf 'secret scan: sha256sum or shasum is required\n' >&2
    exit 1
fi
tar -xzf "$ARCHIVE" -C "$TEMP_ROOT" gitleaks

if [ "$SOURCE_MODE" = --commit ]; then
    SCAN_ROOT="$TEMP_ROOT/commit-tree"
    mkdir -p "$SCAN_ROOT"
    git -C "$REPO_ROOT" archive --format=tar HEAD | tar -xf - -C "$SCAN_ROOT"
    "$GITLEAKS_BIN" dir \
        --no-banner \
        --redact \
        --config "$CONFIG_FILE" \
        "$SCAN_ROOT"
else
    # Scan both views. The index catches bytes already staged for commit even
    # when the working copy was subsequently cleaned; the worktree catches
    # tracked edits and untracked, non-ignored files not staged yet.
    for tree_mode in index worktree; do
        SCAN_ROOT="$TEMP_ROOT/$tree_mode-tree"
        mkdir -p "$SCAN_ROOT"
        python3 "$MATERIALIZER" "--$tree_mode" "$REPO_ROOT" "$SCAN_ROOT"
        printf 'secret scan: checking releasable %s bytes\n' "$tree_mode"
        "$GITLEAKS_BIN" dir \
            --no-banner \
            --redact \
            --config "$CONFIG_FILE" \
            "$SCAN_ROOT"
    done
fi
