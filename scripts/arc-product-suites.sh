#!/bin/bash
# Run the product test suites and capture evidence that actually proves what ran.
#
# Why this exists: the saved stageb2-product-desktop-playwright-gate artifact has
# NO exit-code file and its stdout contains only the Vite/webServer boot
# sequence - no test name, no pass/fail count, no summary line. It proves the app
# built and a preview server started; it does not prove a single Playwright test
# executed. Every suite here writes stdout, stderr, an exit code, and for
# Playwright a machine-readable JSON report, so the next reader does not have to
# infer what happened.
#
# usage: arc-product-suites.sh [tag]
set -u
TAG="${1:-product}"
REPO=/Users/excaulibur/work/arc-chain-readiness-20260919
OUT=/Users/excaulibur/work/outputs/arc-chain-readiness-20260919
export PATH="$HOME/.local/bin:$HOME/.local/node/bin:$PATH"

run() { # run NAME DIR CMD...
  local name="$1" dir="$2"; shift 2
  local s="$OUT/stageb2-$TAG-$name"
  echo "== $name : $* (cwd $dir)"
  ( cd "$REPO/$dir" && "$@" ) > "$s.stdout.txt" 2> "$s.stderr.txt"
  local rc=$?
  echo "$rc" > "$s.exitcode.txt"
  echo "   exit=$rc  stdout=$(wc -l < "$s.stdout.txt") lines  stderr=$(wc -l < "$s.stderr.txt") lines"
  return 0
}

{
  echo "# product suites — $(date '+%Y-%m-%d %H:%M:%S %Z')"
  echo "node $(node --version)  npm $(npm --version)"
  echo
} > "$OUT/stageb2-$TAG-summary.txt"

run shared-arc-network  shared/frontend node ./test-arc-network.mjs
run explorer-contract   explorer        node ./test-contract.mjs
run dashboard-verify-css dashboard      node ./verify-css.mjs
run dashboard-contract  dashboard       node ./test-contract.mjs
run desktop-tsc         desktop         npx tsc --noEmit

# Playwright, with a JSON reporter so the result is a fact rather than an
# inference from an empty artifact directory.
PW_JSON="$OUT/stageb2-$TAG-desktop-playwright.report.json"
export PLAYWRIGHT_JSON_OUTPUT_NAME="$PW_JSON"
run desktop-playwright-gate desktop \
    npx playwright test --config playwright.gate.config.ts --reporter=list,json

{
  echo
  echo "## results"
  for f in "$OUT/stageb2-$TAG-"*.exitcode.txt; do
    n=$(basename "$f" .exitcode.txt); printf "%-48s exit=%s\n" "${n#stageb2-$TAG-}" "$(cat "$f")"
  done
  if [ -f "$PW_JSON" ]; then
    echo
    echo "## playwright tally (from the JSON reporter, not inferred)"
    node -e '
      const r=require(process.argv[1]);
      const t={};(function w(s){(s.suites||[]).forEach(w);(s.specs||[]).forEach(sp=>
        sp.tests.forEach(x=>{t[x.status]=(t[x.status]||0)+1}))})({suites:r.suites});
      console.log("  specs by status:", JSON.stringify(t));
      console.log("  expected:", r.stats?.expected, "unexpected:", r.stats?.unexpected,
                  "flaky:", r.stats?.flaky, "skipped:", r.stats?.skipped);
      console.log("  duration_ms:", r.stats?.duration);
    ' "$PW_JSON" 2>&1 || echo "  (could not parse report)"
  else
    echo
    echo "## playwright: NO JSON REPORT PRODUCED — treat as no evidence of execution"
  fi
} >> "$OUT/stageb2-$TAG-summary.txt"

cat "$OUT/stageb2-$TAG-summary.txt"
