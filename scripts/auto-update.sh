#!/usr/bin/env bash
# Supported local-only compatibility wrapper for stake-zero community installs.
# It accepts no remote host, release URL, or executable payload. The installed
# canonical updater resolves one release, verifies SHA-256 before replacement,
# and applies its own downgrade/service safety checks.
set -Eeuo pipefail

ARC_DIR="${ARC_DIR:-$HOME/.arc}"
case "$ARC_DIR" in
    /*) ;;
    *)
        printf 'ARC_DIR must be an absolute local install path: %s\n' "$ARC_DIR" >&2
        exit 1 ;;
esac
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
