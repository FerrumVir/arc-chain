#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

release_manifest_handoff_is_exact_and_adversarially_gated() {
    python3 "$TEST_DIR/test_release_manifest_handoff.py"
}

run_test \
    'release manifest crosses isolated signer and publisher jobs through exact sealed bytes' \
    release_manifest_handoff_is_exact_and_adversarially_gated
finish_tests
