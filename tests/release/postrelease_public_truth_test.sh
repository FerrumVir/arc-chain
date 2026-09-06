#!/usr/bin/env bash
set -Eeuo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
exec python3 -m unittest "$TEST_DIR/test_postrelease_public_truth.py"
