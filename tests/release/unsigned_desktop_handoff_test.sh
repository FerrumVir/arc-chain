#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

unsigned_handoff_is_exact_and_adversarially_verified() {
    python3 "$TEST_DIR/test_unsigned_desktop_handoff.py"
}

run_test \
    'unsigned desktop bytes cross to a fresh signer through an exact-ID digest-verified handoff' \
    unsigned_handoff_is_exact_and_adversarially_verified

finish_tests
