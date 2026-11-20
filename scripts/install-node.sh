#!/usr/bin/env bash
set -Eeuo pipefail

printf '%s\n' \
    'install-node.sh: legacy validator installation is retired.' \
    'Routing to the stake-zero community installer; release artifacts are checksum-verified.' >&2

if ! SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" 2>/dev/null && pwd)"; then
    SCRIPT_DIR=""
fi
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/../install.sh" ]; then
    exec bash "$SCRIPT_DIR/../install.sh" "$@"
fi

printf '%s\n' \
    'install-node.sh: canonical ../install.sh was not found.' \
    'Refusing to download or execute an installer from a mutable branch.' \
    'After v0.8.0 is approved and published, download that exact tag and verify its release checksum.' >&2
exit 78
