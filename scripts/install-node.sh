#!/usr/bin/env bash
# Legacy compatibility entry point.
#
# This script previously created an ad-hoc private key, configured 500,000 ARC
# of validator stake, and exposed a deterministic validator seed in service
# argv. That path is permanently retired. The supported installer runs a
# stake-zero community observer and keeps its non-production identity out of
# argv; production validators require separately provisioned keyfiles and an
# approved complete genesis.
set -Eeuo pipefail

printf '%s\n' \
    'install-node.sh: legacy validator installation is retired.' \
    'Routing to the stake-zero community installer; release artifacts are checksum-verified.' >&2

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)"
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/../install.sh" ]; then
    exec bash "$SCRIPT_DIR/../install.sh" "$@"
fi

printf '%s\n' \
    'install-node.sh: canonical ../install.sh was not found.' \
    'Refusing to download or execute an installer from a mutable branch.' \
    'After v0.7.12 is approved and published, download that exact tag and verify its release checksum.' >&2
exit 78
