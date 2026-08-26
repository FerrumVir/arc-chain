#!/usr/bin/env bash
# Compatibility wrapper for installations made by the hardened installer.
set -Eeuo pipefail

ARC_DIR="${ARC_DIR:-$HOME/.arc}"
INSTALLER="$ARC_DIR/bin/arc-installer"
if [ ! -x "$INSTALLER" ]; then
    printf 'ARC updater is not installed at %s. Run install.sh once first.\n' "$INSTALLER" >&2
    exit 1
fi

case "${1:-}" in
    ''|--once) ;;
    --help|-h)
        printf 'Usage: ARC_DIR=/absolute/install/path %s [--once]\n' "$0"
        exit 0 ;;
    *)
        printf 'Unknown option: %s (only --once is retained for compatibility)\n' "$1" >&2
        exit 1 ;;
esac

exec "$INSTALLER" --update-only --install-dir "$ARC_DIR"
