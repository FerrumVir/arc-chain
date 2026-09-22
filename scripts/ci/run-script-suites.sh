#!/usr/bin/env bash
# Runs every non-Rust script suite that, before this existed, only ran by
# hand: the arc_conformance / arc_soak / arc_ops Python packages and the
# release tooling's Python tests.
#
# Each invocation below mirrors the exact command already documented for that
# suite (docs/protocol/integer-profile-contract-v1.md,
# docs/operations/backup-restore-upgrade.md, scripts/arc-soak.sh), so running
# one line by hand reproduces exactly what this script and CI do.
#
# Used by .github/workflows/ci.yml's `script-suites` job and by
# `make test-scripts`. Keep the three in sync.
#
# Every suite below is synthetic and self-contained: fixtures are built
# in-memory or from tiny committed JSON, nothing needs a real GGUF model or a
# built arc-node binary, and nothing touches the network or another process.
# A suite that needed one of those would be listed here as EXCLUDED with the
# reason, not run. (As of this writing, nothing under scripts/arc_conformance/
# tests, scripts/arc_soak/tests, scripts/arc_ops/tests or
# scripts/release/tests needs such an exclusion.)
#
# Deliberately `set -uo pipefail` and not `-e`: every suite must get a chance
# to run even if an earlier one fails, so a full report of failures comes
# back in one CI attempt instead of one failure at a time.
set -uo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd -P)"
failed=0

run_unittest() {
  # $1: human label; $2..: the argv that follows `python3 -m unittest`.
  local label="$1"
  shift
  echo "--- $label ($*) ---"
  if ! nice -n 19 python3 -m unittest "$@" -v; then
    echo "FAILED: $label" >&2
    failed=1
  fi
}

# arc_conformance, arc_soak and arc_ops are sibling packages under scripts/,
# and dotted imports between them (e.g. arc_soak.orchestrate importing
# arc_ops.backup) resolve relative to that directory - so run these with
# scripts/ as the working directory, exactly like scripts/arc-soak.sh does.
cd "$REPO_ROOT/scripts" || exit 1

run_unittest "arc_conformance: integer reference"        arc_conformance.tests.test_integer_reference
run_unittest "arc_conformance: M3/M4 llama.cpp compare"  arc_conformance.tests.test_m4_compare
run_unittest "arc_conformance: model-package manifest"   arc_conformance.tests.test_package_manifest
run_unittest "arc_soak: analyzer (verdict/exit-status)"  arc_soak.tests.test_analyze
run_unittest "arc_soak: growth fitter"                   arc_soak.tests.test_growth
run_unittest "arc_soak: orchestrator pure helpers"       arc_soak.tests.test_orchestrate
run_unittest "arc_soak: post-soak evidence collector"    arc_soak.tests.test_collect_evidence
run_unittest "arc_ops: backup/verify/restore"            arc_ops.tests.test_backup
run_unittest "arc_ops: operational checks (R7)"          arc_ops.tests.test_check
run_unittest "arc_ops: conservation audit (P7)"          arc_ops.tests.test_conservation
run_unittest "arc_ops: receipt reconciliation (P9)"     arc_ops.tests.test_receipts

# scripts/release/tests is not a package (no __init__.py): test_sbom.py and
# test_verify_local_artifacts.py load their module by file path via importlib,
# and test_signing_fixtures.py only shells out to ssh-keygen - all run fine
# addressed by file path from the repo root instead of a dotted module name.
cd "$REPO_ROOT" || exit 1
run_unittest "release: SBOM builder (R2)"                    scripts/release/tests/test_sbom.py
run_unittest "release: signing-path FIXTURE tests (R3/U7)"   scripts/release/tests/test_signing_fixtures.py
run_unittest "release: local artifact verifier (R2)"         scripts/release/tests/test_verify_local_artifacts.py

exit "$failed"
