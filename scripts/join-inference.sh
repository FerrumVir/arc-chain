#!/usr/bin/env bash
# Compatibility entry point for a checksummed stake-zero community worker.
set -Eeuo pipefail

REPO_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
HAS_MODEL=false
EXPECT_MODEL_VALUE=false
for argument in "$@"; do
    if [ "$EXPECT_MODEL_VALUE" = true ]; then
        [ -n "$argument" ] || {
            printf 'join-inference.sh: --model requires an absolute path\n' >&2
            exit 2
        }
        HAS_MODEL=true
        EXPECT_MODEL_VALUE=false
        continue
    fi
    case "$argument" in
        --model) EXPECT_MODEL_VALUE=true ;;
        -h|--help) exec /bin/bash "$REPO_ROOT/install.sh" "$@" ;;
    esac
done
[ "$EXPECT_MODEL_VALUE" = false ] || {
    printf 'join-inference.sh: --model requires an absolute path\n' >&2
    exit 2
}
if [ -n "${ARC_MODEL_PATH:-}" ]; then HAS_MODEL=true; fi
if [ "$HAS_MODEL" != true ]; then
    printf '%s\n' \
        'A verified local GGUF is required before this node may advertise inference.' \
        'Usage: scripts/join-inference.sh --model /absolute/path/to/model.gguf' >&2
    exit 2
fi

printf '%s\n' \
    'Installing a checksummed stake-zero community worker.' \
    'Work and rewards are not guaranteed; only mined 0x25 receipts are earnings.'
exec /bin/bash "$REPO_ROOT/install.sh" "$@"
