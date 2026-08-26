#!/usr/bin/env bash
# Deprecated compatibility wrapper. Use ../install.sh directly for all new
# headless installs; this wrapper no longer has an independent download path.
set -Eeuo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)"
INSTALLER="$SCRIPT_DIR/../install.sh"
TEMP_INSTALLER=""
if [ ! -f "$INSTALLER" ]; then
    command -v curl >/dev/null 2>&1 || {
        printf 'Canonical installer not found and curl is unavailable.\n' >&2
        exit 1
    }
    TEMP_INSTALLER="$(mktemp "${TMPDIR:-/tmp}/arc-install.XXXXXX")"
    trap 'rm -f -- "$TEMP_INSTALLER"' EXIT HUP INT TERM
    curl --fail --silent --show-error --location \
        https://raw.githubusercontent.com/FerrumVir/arc-chain/main/install.sh \
        > "$TEMP_INSTALLER"
    INSTALLER="$TEMP_INSTALLER"
fi

case "${1:-}" in
    --help|-h)
        bash "$INSTALLER" --help ;;
    '')
        bash "$INSTALLER" ;;
    *)
        MODEL_PATH="$1"
        shift
        bash "$INSTALLER" --model "$MODEL_PATH" "$@" ;;
esac
