#!/usr/bin/env bash
set -Eeuo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
python3 "$TEST_DIR/test_privileged_workflow_boundaries.py"
