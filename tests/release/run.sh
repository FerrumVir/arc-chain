#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
status=0

for test_file in \
    "$TEST_DIR/static_contract_test.sh" \
    "$TEST_DIR/public_site_contract_test.sh" \
    "$TEST_DIR/community_diagnostics_contract_test.sh" \
    "$TEST_DIR/recovery_archive_contract_test.sh" \
    "$TEST_DIR/production_manifest_builder_test.sh" \
    "$TEST_DIR/legacy_public_height_test.sh" \
    "$TEST_DIR/drive_prefreeze_gate_test.sh" \
    "$TEST_DIR/legacy_validator_set_contract_test.sh" \
    "$TEST_DIR/documentation_contract_test.sh" \
    "$TEST_DIR/legacy_operations_retirement_test.sh" \
    "$TEST_DIR/supply_chain_semantic_test.sh" \
    "$TEST_DIR/secret_scan_materialization_test.sh" \
    "$TEST_DIR/genesis_contract_test.sh" \
    "$TEST_DIR/pretag_artifact_test.sh" \
    "$TEST_DIR/unsigned_desktop_handoff_test.sh" \
    "$TEST_DIR/release_manifest_handoff_test.sh" \
    "$TEST_DIR/privileged_workflow_boundary_test.sh" \
    "$TEST_DIR/macos_community_canary_test.sh" \
    "$TEST_DIR/release_publication_contract_test.sh" \
    "$TEST_DIR/validator_vault_rewrap_contract_test.sh" \
    "$TEST_DIR/validator_vault_restore_contract_test.sh" \
    "$TEST_DIR/cutover_handoff_commit_test.sh" \
    "$TEST_DIR/release_assembly_test.sh" \
    "$TEST_DIR/release_manifest_signature_test.sh" \
    "$TEST_DIR/signing_key_backup_behavior_test.sh" \
    "$TEST_DIR/installer_behavior_test.sh"
do
    printf '# %s\n' "${test_file##*/}"
    /bin/bash "$test_file" || status=1
done

exit "$status"
