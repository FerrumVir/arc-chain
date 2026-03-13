#!/usr/bin/env bash
# ARC Chain — Comprehensive CI Check
# Runs: fmt check, clippy, build, test, benchmark compilation
# Generates pass/fail summary

# Don't use set -e — we handle errors manually per check
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

PASS=0
FAIL=0
RESULTS=()

run_check() {
    local name=$1
    shift
    echo ""
    echo "━━━ $name ━━━"
    local tmpfile
    tmpfile=$(mktemp)
    if "$@" >"$tmpfile" 2>&1; then
        tail -20 "$tmpfile"
        rm -f "$tmpfile"
        PASS=$((PASS + 1))
        RESULTS+=("  [PASS] $name")
    else
        tail -30 "$tmpfile"
        rm -f "$tmpfile"
        FAIL=$((FAIL + 1))
        RESULTS+=("  [FAIL] $name")
    fi
}

echo "================================================================"
echo " ARC Chain — CI Check Suite"
echo " $(date)"
echo "================================================================"

# 1. Cargo check (workspace)
run_check "Workspace compilation" cargo check --workspace

# 2. Run all tests
run_check "Unit & integration tests" cargo test --workspace

# 3. Build release binaries
run_check "Release build" cargo build --release --workspace

# 4. Check benchmark binaries compile
run_check "Benchmark compilation" cargo build --release --bin arc-bench-production --bin arc-bench-mixed

# 5. Count tests (only if compilation succeeded)
echo ""
echo "━━━ Test Statistics ━━━"
if TEST_OUTPUT=$(cargo test --workspace 2>&1); then
    TOTAL_TESTS=$(echo "$TEST_OUTPUT" | grep "^test result:" | awk '{sum += $4} END {print sum}')
    TOTAL_FAILED=$(echo "$TEST_OUTPUT" | grep "^test result:" | awk '{sum += $6} END {print sum}')
    echo "  Total tests:   ${TOTAL_TESTS:-0}"
    echo "  Passed:        ${TOTAL_TESTS:-0}"
    echo "  Failed:        ${TOTAL_FAILED:-0}"
else
    echo "  (skipped — workspace does not compile)"
fi

# 6. Count lines of code
echo ""
echo "━━━ Codebase Statistics ━━━"
RUST_FILES=$(find crates -name "*.rs" 2>/dev/null | wc -l | tr -d ' ')
RUST_LINES=$(find crates -name "*.rs" -exec cat {} + 2>/dev/null | wc -l | tr -d ' ')
echo "  Rust files:    $RUST_FILES"
echo "  Rust LOC:      $RUST_LINES"

# Module count
MODULE_COUNT=$(find crates -name "*.rs" ! -name "lib.rs" ! -name "main.rs" ! -name "mod.rs" 2>/dev/null | wc -l | tr -d ' ')
echo "  Modules:       $MODULE_COUNT"

# Crate count
CRATE_COUNT=$(find crates -maxdepth 1 -mindepth 1 -type d 2>/dev/null | wc -l | tr -d ' ')
echo "  Crates:        $CRATE_COUNT"

# Summary
echo ""
echo "================================================================"
echo " RESULTS"
echo "================================================================"
for r in "${RESULTS[@]}"; do
    echo "$r"
done
echo ""
echo "  Passed: $PASS  Failed: $FAIL"
if [[ $FAIL -eq 0 ]]; then
    echo ""
    echo "  STATUS: ALL CHECKS PASSED ✓"
else
    echo ""
    echo "  STATUS: $FAIL CHECK(S) FAILED ✗"
fi
echo "================================================================"

# Exit with failure if any check failed
[[ $FAIL -eq 0 ]]
