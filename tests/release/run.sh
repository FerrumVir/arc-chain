#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
status=0

for test_file in \
    "$TEST_DIR/static_contract_test.sh" \
    "$TEST_DIR/genesis_contract_test.sh" \
    "$TEST_DIR/release_assembly_test.sh" \
    "$TEST_DIR/installer_behavior_test.sh"
do
    printf '# %s\n' "${test_file##*/}"
    /bin/bash "$test_file" || status=1
done

exit "$status"
