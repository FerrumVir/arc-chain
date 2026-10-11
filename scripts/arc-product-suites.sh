#!/bin/bash
# Run the product test suites and produce evidence that can distinguish a pass
# from a suite that never ran.
#
# History, because it explains every rule below:
#
#   v1 (82e1355) was written after the saved desktop Playwright artifact turned
#   out to have NO exit-code file and only the Vite boot sequence in stdout - it
#   proved a preview server started, nothing more.
#
#   v1 then had the same defect in a new place: `run()` recorded each suite's
#   exit code to a file and unconditionally `return 0`, so the runner's own exit
#   status was decided by the final `cat`. A reviewer reproduced it with stubs:
#   all six suite exit-code artifacts nonzero, runner exit 0. It also accepted a
#   stale Playwright JSON left by an earlier run with the same tag, and treated
#   a missing or unparseable report as success.
#
# Rules now enforced:
#   - every required suite's exit code is aggregated; the runner exits nonzero
#     if any failed
#   - each run writes to its OWN directory; reusing one is refused, so no
#     artifact from a previous run can be mistaken for this one
#   - the Playwright JSON must exist, parse, and report at least one EXECUTED
#     test; absent/!empty/unparseable is a failure, not a pass
#   - skipped required live-backend tests are counted and reported separately;
#     they never contribute to mock-suite success
#
# usage: arc-product-suites.sh [tag]            run the suites
#        arc-product-suites.sh --self-test      prove failure propagation
set -uo pipefail

REPO="${HOME}/work/arc-chain-readiness-20260919"
OUT="${HOME}/work/outputs/arc-chain-readiness-20260919"
export PATH="$HOME/.local/bin:$HOME/.local/node/bin:$PATH"

FAILED=()
RUN_DIR=""

run() { # run NAME DIR CMD...
  local name="$1" dir="$2"; shift 2
  local s="$RUN_DIR/$name"
  echo "== $name : $* (cwd $dir)"
  ( cd "$REPO/$dir" && "$@" ) > "$s.stdout.txt" 2> "$s.stderr.txt"
  local rc=$?
  echo "$rc" > "$s.exitcode.txt"
  if [[ $rc -ne 0 ]]; then FAILED+=("$name(exit=$rc)"); fi
  printf "   exit=%s  stdout=%s lines  stderr=%s lines\n" \
         "$rc" "$(wc -l < "$s.stdout.txt")" "$(wc -l < "$s.stderr.txt")"
  return $rc
}

# ── self-test ───────────────────────────────────────────────────────────────
# Lightweight fixtures, no product suite involved. Proves the two properties
# that were broken: a failing suite must make the runner exit nonzero, and an
# all-passing set must exit 0.
if [[ "${1:-}" == "--self-test" ]]; then
  RUN_DIR=$(mktemp -d); trap 'rm -rf "$RUN_DIR"' EXIT
  echo "self-test A: one failing suite among passing ones"
  FAILED=()
  run ok-1 . true   >/dev/null 2>&1
  run bad-1 . false >/dev/null 2>&1
  run ok-2 . true   >/dev/null 2>&1
  if [[ ${#FAILED[@]} -eq 1 && "${FAILED[0]}" == "bad-1(exit=1)" ]]; then
    echo "  PASS: failure aggregated (${FAILED[0]})"
  else
    echo "  FAIL: expected exactly bad-1, got: ${FAILED[*]:-none}"; exit 1
  fi
  echo "self-test B: all suites passing"
  FAILED=()
  run ok-3 . true >/dev/null 2>&1
  run ok-4 . true >/dev/null 2>&1
  if [[ ${#FAILED[@]} -eq 0 ]]; then echo "  PASS: no failures aggregated"; else
    echo "  FAIL: unexpected failures: ${FAILED[*]}"; exit 1; fi
  echo "self-test C: exit-code artifacts written per suite"
  for n in ok-1 bad-1 ok-2 ok-3 ok-4; do
    [[ -f "$RUN_DIR/$n.exitcode.txt" ]] || { echo "  FAIL: no exitcode for $n"; exit 1; }
  done
  echo "  PASS: all exit-code artifacts present"
  echo "SELF-TEST OK"; exit 0
fi

TAG="${1:-$(date +%Y%m%d-%H%M%S)}"
RUN_DIR="$OUT/product-$TAG"
if [[ -e "$RUN_DIR" ]]; then
  echo "REFUSING: $RUN_DIR already exists. Re-using a tag lets a previous run's" >&2
  echo "          artifacts be read as this run's evidence. Choose a new tag." >&2
  exit 2
fi
mkdir -p "$RUN_DIR"

SUMMARY="$RUN_DIR/summary.txt"
{
  echo "# product suites — $(date '+%Y-%m-%d %H:%M:%S %Z')"
  echo "node $(node --version 2>/dev/null)  npm $(npm --version 2>/dev/null)"
  echo "repo HEAD $(cd "$REPO" && git rev-parse --short HEAD 2>/dev/null)"
  echo "run dir $RUN_DIR"
  echo
} | tee "$SUMMARY"

run shared-arc-network   shared/frontend node ./test-arc-network.mjs
run explorer-contract    explorer        node ./test-contract.mjs
run dashboard-verify-css dashboard       node ./verify-css.mjs
run dashboard-contract   dashboard       node ./test-contract.mjs
run desktop-tsc          desktop         npx tsc --noEmit

# Playwright: fresh report path inside THIS run's directory, removed first so a
# stale file cannot be read as this run's result.
PW_JSON="$RUN_DIR/desktop-playwright.report.json"
rm -f "$PW_JSON"
export PLAYWRIGHT_JSON_OUTPUT_NAME="$PW_JSON"
run desktop-playwright-gate desktop \
    npx playwright test --config playwright.gate.config.ts --reporter=list,json

# Evidence validation: the report must exist, parse, and show executed tests.
PW_EXPECTED=0; PW_UNEXPECTED=0; PW_FLAKY=0; PW_SKIPPED=0
if [[ ! -s "$PW_JSON" ]]; then
  FAILED+=("desktop-playwright-gate(no JSON report produced)")
else
  PW_STATS=$(node -e '
    try {
      const r = require(process.argv[1]);
      const s = r.stats || {};
      const v = n => Number.isFinite(s[n]) ? s[n] : -1;
      console.log([v("expected"), v("unexpected"), v("flaky"), v("skipped")].join(" "));
    } catch (e) { console.log("-1 -1 -1 -1"); }
  ' "$PW_JSON" 2>/dev/null) || PW_STATS="-1 -1 -1 -1"
  read -r PW_EXPECTED PW_UNEXPECTED PW_FLAKY PW_SKIPPED <<<"$PW_STATS"
  if [[ "$PW_EXPECTED" -lt 0 ]]; then
    FAILED+=("desktop-playwright-gate(report unparseable)")
  elif [[ "$PW_EXPECTED" -eq 0 ]]; then
    FAILED+=("desktop-playwright-gate(report shows ZERO executed tests)")
  elif [[ "$PW_UNEXPECTED" -gt 0 ]]; then
    FAILED+=("desktop-playwright-gate($PW_UNEXPECTED unexpected)")
  fi
fi

{
  echo
  echo "## suite exit codes"
  for f in "$RUN_DIR"/*.exitcode.txt; do
    [[ -e "$f" ]] || continue
    printf "  %-34s exit=%s\n" "$(basename "$f" .exitcode.txt)" "$(cat "$f")"
  done
  echo
  echo "## playwright (read from the JSON reporter, not inferred)"
  echo "  expected=$PW_EXPECTED unexpected=$PW_UNEXPECTED flaky=$PW_FLAKY skipped=$PW_SKIPPED"
  if [[ "$PW_SKIPPED" -gt 0 ]]; then
    echo "  NOTE: $PW_SKIPPED skipped. These are the live-backend specs; they need"
    echo "        ARC_LIVE_PORT and a real arc-node. Mock-suite success does NOT"
    echo "        cover any live journey and is not counted as if it did."
  fi
  echo
  if [[ ${#FAILED[@]} -eq 0 ]]; then
    echo "## VERDICT: PASS (mock-backend coverage only)"
  else
    echo "## VERDICT: FAIL"
    for f in "${FAILED[@]}"; do echo "   - $f"; done
  fi
} | tee -a "$SUMMARY"

echo "${#FAILED[@]}" > "$RUN_DIR/failed_count.txt"
[[ ${#FAILED[@]} -eq 0 ]] && exit 0 || exit 1
