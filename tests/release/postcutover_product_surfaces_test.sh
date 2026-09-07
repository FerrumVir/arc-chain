#!/usr/bin/env bash
set -euo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"

node "$REPO_ROOT/tests/release/test-postcutover-product-surfaces.mjs"
node --test "$REPO_ROOT/tests/release/test-desktop-live-product-receipt.mjs"
