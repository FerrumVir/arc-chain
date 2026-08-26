#!/usr/bin/env bash
# Backward-compatible entry point. The release asset/root install.sh is the
# single implementation so asset names, checksum policy, and service behavior
# cannot drift between two installers.
set -Eeuo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)"
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/../install.sh" ]; then
    exec bash "$SCRIPT_DIR/../install.sh" "$@"
fi

command -v curl >/dev/null 2>&1 || {
    printf 'install-community-node.sh: curl is required\n' >&2
    exit 1
}
TEMP_INSTALLER="$(mktemp "${TMPDIR:-/tmp}/arc-install.XXXXXX")"
cleanup() { rm -f -- "$TEMP_INSTALLER"; }
trap cleanup EXIT HUP INT TERM

curl --fail --silent --show-error --location \
    https://raw.githubusercontent.com/FerrumVir/arc-chain/main/install.sh \
    > "$TEMP_INSTALLER"
bash "$TEMP_INSTALLER" "$@"
