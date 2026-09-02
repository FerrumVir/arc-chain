#!/usr/bin/env bash
set -euo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"

python3 "$REPO_ROOT/scripts/recovery/test_build_production_manifest.py"
