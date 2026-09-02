#!/usr/bin/env bash
set -Eeuo pipefail

REPO_ROOT="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
exec python3 "$REPO_ROOT/scripts/recovery/test_legacy_public_height.py"
