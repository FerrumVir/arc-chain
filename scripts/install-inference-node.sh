#!/usr/bin/env bash
# Legacy system-service name retained as a safe wrapper around install.sh.
set -Eeuo pipefail

REPO_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
HAS_MODEL=false
EXPECT_MODEL_VALUE=false
for argument in "$@"; do
    if [ "$EXPECT_MODEL_VALUE" = true ]; then
        [ -n "$argument" ] || {
            printf 'install-inference-node.sh: --model requires an absolute path\n' >&2
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
    printf 'install-inference-node.sh: --model requires an absolute path\n' >&2
    exit 2
}
if [ -n "${ARC_MODEL_PATH:-}" ]; then HAS_MODEL=true; fi
if [ "$HAS_MODEL" != true ]; then
    printf '%s\n' \
        'A verified local GGUF is required.' \
        'Usage: sudo scripts/install-inference-node.sh --model /absolute/path/to/model.gguf' >&2
    exit 2
fi

printf '%s\n' \
    'The legacy 5,000,000-ARC service installer is retired.' \
    'Delegating to the checksummed stake-zero system-service installer.'
exec /bin/bash "$REPO_ROOT/install.sh" --system-service "$@"
