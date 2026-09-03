#!/usr/bin/env bash
set -euo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
exec python3 "$TEST_DIR/test_cutover_handoff_commit.py"
