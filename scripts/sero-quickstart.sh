#!/usr/bin/env bash
# Deprecated compatibility wrapper. Use ../install.sh directly for all new
# headless installs; this wrapper no longer has an independent download path.
set -Eeuo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)"
INSTALLER="$SCRIPT_DIR/../install.sh"
if [ ! -f "$INSTALLER" ]; then
    printf '%s\n' \
        'sero-quickstart.sh: canonical ../install.sh was not found.' \
        'Refusing to download or execute an installer from a mutable branch.' \
        'After v0.8.0 is approved and published, use its exact-tag installer and release checksum.' >&2
    exit 78
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
