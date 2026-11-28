#!/usr/bin/env bash
set -uo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd -P)"
failed=0

run_unittest() {
  local label="$1"
  shift
  echo "--- $label ($*) ---"
  if ! nice -n 19 python3 -m unittest "$@" -v; then
    echo "FAILED: $label" >&2
    failed=1
  fi
}

cd "$REPO_ROOT/scripts" || exit 1

run_unittest "arc_conformance: integer reference"        arc_conformance.tests.test_integer_reference
run_unittest "arc_conformance: M3/M4 llama.cpp compare"  arc_conformance.tests.test_m4_compare
run_unittest "arc_conformance: model-package manifest"   arc_conformance.tests.test_package_manifest
run_unittest "arc_conformance: dyadic modern reference"  arc_conformance.tests.test_modern_reference
run_unittest "arc_soak: analyzer (verdict/exit-status)"  arc_soak.tests.test_analyze
run_unittest "arc_soak: growth fitter"                   arc_soak.tests.test_growth
run_unittest "arc_soak: orchestrator pure helpers"       arc_soak.tests.test_orchestrate
run_unittest "arc_soak: post-soak evidence collector"    arc_soak.tests.test_collect_evidence
run_unittest "arc_ops: backup/verify/restore"            arc_ops.tests.test_backup
run_unittest "arc_ops: operational checks (R7)"          arc_ops.tests.test_check
run_unittest "arc_ops: conservation audit (P7)"          arc_ops.tests.test_conservation
run_unittest "arc_ops: receipt reconciliation (P9)"     arc_ops.tests.test_receipts

cd "$REPO_ROOT" || exit 1
run_unittest "ci: exported-state privacy (ARC-410 L9)"        scripts/ci/tests/test_export_privacy.py
run_unittest "ci: desktop dependency boundary (ARC-410 L10)" scripts/ci/tests/test_desktop_dependency_boundary.py
run_unittest "release: SBOM builder (R2)"                    scripts/release/tests/test_sbom.py
run_unittest "release: signing-path FIXTURE tests (R3/U7)"   scripts/release/tests/test_signing_fixtures.py
run_unittest "release: local artifact verifier (R2)"         scripts/release/tests/test_verify_local_artifacts.py
run_unittest "release: local build script FIXTURE (R2)"      scripts/release/tests/test_build_local_artifacts.py
run_unittest "release: existing-chain update evidence"      scripts/release/tests/test_existing_chain_update.py
run_unittest "release: exact-parent update inputs"           scripts/release/tests/test_existing_chain_update_input.py
run_unittest "release: immutable update handoff ZIP"         scripts/release/tests/test_materialize_update_handoff.py

run_unittest "benchmarks: native request latency (M8)"      scripts/benchmarks/tests/test_native_request_latency.py
run_unittest "benchmarks: inference cost projection (M9)"   scripts/benchmarks/tests/test_project_inference_cost.py
run_unittest "ci: no-comments checker"                     scripts/ci/test_no_comments.py
run_unittest "qualification: mandatory recovery selectors" scripts/qualification/tests/test_recovery_peer_selectors.py
PYTHONPATH="$REPO_ROOT/scripts/qualification:$REPO_ROOT/scripts/qualification/tests" run_unittest "qualification: bounded shard evidence" discover -s scripts/qualification/scaling_tests -p 'test*.py'

exit "$failed"
