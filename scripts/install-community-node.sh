#!/usr/bin/env bash
# Backward-compatible entry point. The release asset/root install.sh is the
# single implementation so asset names, checksum policy, and service behavior
# cannot drift between two installers.
set -Eeuo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)"
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/../install.sh" ]; then
    exec bash "$SCRIPT_DIR/../install.sh" "$@"
fi

printf '%s\n' \
    'install-community-node.sh: canonical ../install.sh was not found.' \
    'Refusing to download or execute an installer from a mutable branch.' \
    'After v0.7.12 is approved and published, use its exact-tag installer and release checksum.' >&2
exit 78
