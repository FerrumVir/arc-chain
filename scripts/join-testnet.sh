#!/usr/bin/env bash
# Compatibility entry point for the supported stake-zero headless installer.
#
# The old script built an arbitrary checkout, generated a short-lived seed on
# every run, and launched without an explicit stake/community role. That is not
# a safe way to join the forked public testnet. Keep this familiar filename,
# but make it use the exact-tagged, checksummed installer contract.
set -Eeuo pipefail

REPO_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = --with-inference ]; then
    printf '%s\n' \
        'The unchecked automatic model download was removed.' \
        'Download and verify the supported GGUF separately, then run:' \
        '  scripts/join-inference.sh --model /absolute/path/to/model.gguf' >&2
    exit 2
fi

printf '%s\n' \
    'join-testnet.sh now installs a checksummed stake-zero community observer.' \
    'It does not create validator stake or claim that the forked fleet is healthy.'
exec /bin/bash "$REPO_ROOT/install.sh" "$@"
